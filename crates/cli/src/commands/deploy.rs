//! `zsql deploy`: the checked-out branch to a server, once or on every
//! change.

use crate::cli::{Target, Where};
use crate::side::Side;
use crate::{project, remote, show};
use anyhow::{bail, Result};
use serde_json::Value;
use std::sync::{Arc, Mutex};

/// How a deploy is asked for.
pub struct How {
    pub dry_run: bool,
    pub watch: bool,
    pub skip_check: bool,
    pub clear_secrets: bool,
}

pub fn deploy(at: Where, how: How) -> Result<()> {
    let How { dry_run, watch, skip_check, clear_secrets } = how;
    let t = at.target(true)?;
    let Some(p) = &t.project else { bail!("deploy needs a project directory") };
    // The model loaded and tested here first: a round trip to the server
    // to be told the file does not load is a round trip wasted.
    if !skip_check {
        let local = crate::local::Local { project: p.clone() };
        let mut side = crate::side::Session::quiet(local)?;
        let v = side.check()?;
        if !show::print_tests(&v["test_results"]) {
            bail!("tests fail here; fix them, or --skip-check to send anyway");
        }
    }
    let action = if dry_run { "validate" } else { "deploy" };
    let once = |t: &Target| -> Result<bool> {
        let archive = project::archive(&p.dir, if dry_run { project::For::Validate } else { project::For::Deploy })?;
        let note = if t.branch == p.production_branch { " (the production branch)" } else { "" };
        println!("{action} {} ({} bytes) as {}{note} to {}", p.name, archive.len(), t.name(), t.client.server);
        // Said before it goes: a deploy carries the secrets this machine
        // holds, and they become the datasource's credential there.
        let carried = project::carries_secrets_for(&p.dir);
        if !carried.is_empty() && !dry_run {
            println!("with the secrets held here for: {}", carried.join(", "));
        }
        let clear = if clear_secrets && !dry_run { "?clear_secrets=true" } else { "" };
        let progress = Progress::start(!dry_run);
        let v = t.client.post_bytes_streamed(
            &format!("{}/{action}{clear}", remote::branch_path(t.org.as_deref(), &t.uid, &t.branch)),
            "application/gzip",
            &archive,
            &mut |step| progress.step(step),
        );
        progress.stop();
        Ok(show::print_deploy(&v?))
    };
    if !watch {
        if !once(&t)? {
            bail!("tests failed");
        }
        return Ok(());
    }
    if let Err(e) = once(&t) {
        eprintln!("error: {e}");
    }
    println!("watching {} for changes (ctrl-c to stop)", p.dir.display());
    project::watch(
        &p.dir,
        || false,
        || {
            println!();
            if let Err(e) = once(&t) {
                eprintln!("error: {e}");
            }
        },
    );
    Ok(())
}

/// The steps of a deploy as the server says them, a line each, and on a
/// terminal a spinner naming the step under way.
struct Progress {
    deploying: bool,
    shown: Arc<Mutex<Shown>>,
    spinner: Option<std::thread::JoinHandle<()>>,
}

struct Shown {
    /// What the server is doing now, by the last step it said; `None`
    /// once it is done or nothing more is to be drawn.
    doing: Option<&'static str>,
    since: std::time::Instant,
}

impl Progress {
    fn start(deploying: bool) -> Self {
        let shown = Arc::new(Mutex::new(Shown { doing: Some("sending"), since: std::time::Instant::now() }));
        let live = std::io::IsTerminal::is_terminal(&std::io::stdout()) && !crate::tty::agent();
        let spinner = live.then(|| {
            let shown = shown.clone();
            std::thread::spawn(move || {
                const FRAMES: [char; 10] = ['⠋', '⠙', '⠹', '⠸', '⠼', '⠴', '⠦', '⠧', '⠇', '⠏'];
                for n in 0.. {
                    {
                        let s = shown.lock().unwrap_or_else(|e| e.into_inner());
                        let Some(doing) = s.doing else { break };
                        let secs = s.since.elapsed().as_secs_f64();
                        print!("\r\x1b[2K  {} {}", FRAMES[n % FRAMES.len()], dim(&format!("{doing}... {secs:.1}s")));
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    std::thread::sleep(std::time::Duration::from_millis(80));
                }
            })
        });
        Progress { deploying, shown, spinner }
    }

    /// One step the server said: its line, and what comes after it.
    fn step(&self, v: &Value) {
        let Some((line, next)) = describe(v) else { return };
        let mut s = self.shown.lock().unwrap_or_else(|e| e.into_inner());
        if self.spinner.is_some() {
            print!("\r\x1b[2K");
        }
        println!("{line}");
        // A validation ends at its tests.
        s.doing = if v["event"] == "tested" && !self.deploying { None } else { next };
    }

    fn stop(mut self) {
        self.shown.lock().unwrap_or_else(|e| e.into_inner()).doing = None;
        if let Some(spinner) = self.spinner.take() {
            let _ = spinner.join();
            print!("\r\x1b[2K");
            let _ = std::io::Write::flush(&mut std::io::stdout());
        }
    }
}

/// A step's line, and what the server does next. A step this CLI does
/// not know, from a newer server, is passed over; so is each test, which
/// the answer lists in full.
fn describe(v: &Value) -> Option<(String, Option<&'static str>)> {
    let n = |k: &str| v[k].as_u64().unwrap_or(0);
    let (ok, what, detail, next) = match v["event"].as_str()? {
        "unpacked" => (true, "unpacked", plural(n("entries"), "entry", "entries"), Some("loading")),
        "loaded" => (true, "loaded", String::new(), Some("compiling")),
        "compiled" => (
            true,
            "compiled",
            format!(
                "{}, {}, {}",
                plural(n("tables"), "table", "tables"),
                plural(n("fields"), "field", "fields"),
                plural(n("joins"), "join", "joins")
            ),
            Some("testing"),
        ),
        "tested" => {
            let failed = n("failed");
            let said = match (n("passed"), failed) {
                (0, 0) => "no tests".to_string(),
                (p, 0) => format!("{p} passed"),
                (p, f) => format!("{p} passed, {f} failed"),
            };
            (failed == 0, "tested", said, Some("making it live"))
        }
        "kept" => (true, "kept", v["archive"].as_str().unwrap_or("").to_string(), Some("making it live")),
        "live" => (true, "live", String::new(), None),
        _ => return None,
    };
    let mark = if ok { "✓" } else { "✗" };
    let ms = dim(&format!("{}ms", n("ms")));
    Some((format!("  {mark} {what:<9} {detail:<44} {ms}"), next))
}

fn plural(n: u64, one: &str, many: &str) -> String {
    format!("{n} {}", if n == 1 { one } else { many })
}

use crate::side::dim;
