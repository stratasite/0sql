//! `zsql serve`: the server, in this process, and `zsql server
//! start|stop|status|logs`, the same thing kept running in the
//! background.
//!
//! Inside a project, `serve` serves that project: once the server is
//! up it deploys the directory and redeploys as the files change, so
//! one command gives a teammate a live endpoint over the model they
//! are editing. Outside a project it serves whatever its data
//! directory holds. The server itself is `zsql_server`, the crate
//! `zsqld` is built from; this module only configures it from flags
//! and keeps it company.

use crate::{config, project_archive, remote, show, Where};
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// Where the server listens when nothing else is said: the address the
/// CLI's other commands talk to by default.
const DEFAULT_BIND: &str = "127.0.0.1:3699";

#[derive(Args, Clone, Debug)]
pub struct ServeArgs {
    /// Address to listen on; port 0 takes a free one, which is logged.
    #[arg(long, default_value = DEFAULT_BIND)]
    pub bind: String,
    /// Where deployed models live; defaults to $ZSQL_HOME/data.
    #[arg(long)]
    pub data_dir: Option<PathBuf>,
    /// Multi-tenant: keys, accounts, sign-up. Off, there is one tenant and no auth.
    #[arg(long)]
    pub accounts: bool,
    /// Allow listening beyond the loopback with accounts off, which lets anyone who can reach the port deploy and plan.
    #[arg(long)]
    pub open: bool,
    /// Serve only the data directory, even inside a project.
    #[arg(long)]
    pub no_project: bool,
    /// Where the app is reached from outside, for links and cookies; the bind address otherwise.
    #[arg(long)]
    pub public_url: Option<String>,
}

impl ServeArgs {
    /// Where deployed models live.
    fn data_dir(&self) -> Result<PathBuf> {
        match &self.data_dir {
            Some(d) => Ok(d.clone()),
            None => Ok(home()?.join("data")),
        }
    }

    /// These arguments as `zsql serve` would take them, for the
    /// background form to hand on; kept next to the fields so a new
    /// flag cannot be forgotten here.
    fn to_argv(&self, project: Option<&Path>) -> Vec<OsString> {
        let mut argv: Vec<OsString> = vec!["serve".into(), "--bind".into(), self.bind.clone().into()];
        if let Some(d) = &self.data_dir {
            argv.extend(["--data-dir".into(), d.clone().into()]);
        }
        if self.accounts {
            argv.push("--accounts".into());
        }
        if self.open {
            argv.push("--open".into());
        }
        if self.no_project {
            argv.push("--no-project".into());
        } else if let Some(p) = project {
            argv.extend(["--project".into(), p.into()]);
        }
        if let Some(u) = &self.public_url {
            argv.extend(["--public-url".into(), u.clone().into()]);
        }
        argv
    }
}

#[derive(Subcommand, Debug)]
pub enum ServerCommand {
    /// Start `zsql serve` in the background, logging to $ZSQL_HOME/server.log.
    Start {
        #[command(flatten)]
        args: ServeArgs,
    },
    /// Stop the background server (SIGTERM, then wait for it to finish).
    Stop,
    /// Whether the background server is running, and whether it answers.
    Status,
    /// The last lines of the background server's log.
    Logs {
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
}

fn home() -> Result<PathBuf> {
    config::home().context("neither ZSQL_HOME nor HOME is set")
}

fn loopback(bind: &str) -> bool {
    bind.parse::<SocketAddr>().map(|a| a.ip().is_loopback()).unwrap_or(false) || bind.starts_with("localhost:")
}

/// Runs the server here until it is stopped. Inside a project, and with
/// accounts off, the project is deployed to it and redeployed as its
/// files change.
pub fn serve(args: ServeArgs, at: Where) -> Result<()> {
    if !args.accounts && !args.open && !loopback(&args.bind) {
        bail!("{} is not a loopback address, and with accounts off anyone who reaches it may deploy and plan; add --accounts, or --open to mean it", args.bind);
    }
    let data_dir = args.data_dir()?;
    // A project named explicitly must be there; one looked for is optional.
    let project = match (args.no_project, at.project.is_some()) {
        (true, _) => None,
        (false, true) => Some(at.project_dir()?),
        (false, false) => at.project_dir().ok(),
    };
    let mut cfg = zsql_server::Config::local(data_dir, args.bind.clone());
    cfg.accounts = args.accounts;
    cfg.public_url = args.public_url;
    if args.accounts {
        // The limits are about sharing one server between tenants.
        cfg.limits.mode = zsql_server::limits::Mode::On;
    }

    let (tx, rx) = std::sync::mpsc::channel::<SocketAddr>();
    let server = std::thread::Builder::new()
        .name("zsqld".into())
        .spawn(move || {
            zsql_server::run_blocking(cfg, move |addr| {
                let _ = tx.send(addr);
            })
        })
        .context("starting the server thread")?;

    // The address, or the reason there is none.
    let addr = loop {
        match rx.recv_timeout(Duration::from_millis(100)) {
            Ok(a) => break a,
            Err(std::sync::mpsc::RecvTimeoutError::Timeout) if !server.is_finished() => continue,
            Err(_) => return finish(server),
        }
    };

    match (project, args.accounts) {
        (Some(p), false) => {
            // An unspecified address is listened on, not connected to.
            let reach = match addr.ip() {
                IpAddr::V4(ip) if ip.is_unspecified() => IpAddr::V4(Ipv4Addr::LOCALHOST),
                IpAddr::V6(ip) if ip.is_unspecified() => IpAddr::V6(Ipv6Addr::LOCALHOST),
                ip => ip,
            };
            let client = remote::Client::new(&format!("http://{}", SocketAddr::new(reach, addr.port())), None);
            let branch = config::git_branch(&p.dir).unwrap_or_else(|_| "main".into());
            eprintln!("zsql: serving {} as {}/{branch}; redeploying as its files change", p.dir.display(), p.uid);
            let deploy = || -> Result<()> {
                let archive = project_archive(&p.dir)?;
                let v = client.post_bytes(&format!("{}/deploy", remote::branch_path(&p.uid, &branch)), "application/gzip", &archive)?;
                show::print_deploy(&v);
                Ok(())
            };
            if let Err(e) = deploy() {
                eprintln!("error: {e}");
            }
            crate::watch_files(
                &p.dir,
                || server.is_finished(),
                || {
                    println!();
                    if let Err(e) = deploy() {
                        eprintln!("error: {e}");
                    }
                },
            );
        }
        (Some(p), true) => {
            eprintln!(
                "zsql: accounts are on, so {} is not deployed by itself; sign up, make a key, and `zsql deploy --server http://{addr}`",
                p.dir.display()
            );
            while !server.is_finished() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
        (None, _) => {
            while !server.is_finished() {
                std::thread::sleep(Duration::from_millis(500));
            }
        }
    }
    finish(server)
}

fn finish(server: std::thread::JoinHandle<Result<()>>) -> Result<()> {
    match server.join() {
        Ok(outcome) => outcome,
        Err(_) => bail!("the server thread panicked"),
    }
}

// --------------------------------------------------------- in the background

/// What `start` records about the server it started, so `status` can
/// ask the right address and `stop` the right process.
#[derive(serde::Serialize, serde::Deserialize)]
struct Started {
    pid: u32,
    bind: String,
    data_dir: PathBuf,
}

fn pid_file() -> Result<PathBuf> {
    Ok(home()?.join("server.pid"))
}

fn log_file() -> Result<PathBuf> {
    Ok(home()?.join("server.log"))
}

/// The pid holding a data directory's lock, if any: the server itself
/// writes its pid there under the lock, so this is the one answer that
/// a stale pidfile or a reused pid cannot fake.
#[cfg(unix)]
fn lock_holder(data_dir: &Path) -> Option<u32> {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    let mut file = std::fs::OpenOptions::new().read(true).write(true).open(data_dir.join("lock")).ok()?;
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return None;
    }
    let mut text = String::new();
    file.read_to_string(&mut text).ok()?;
    text.trim().parse().ok()
}

#[cfg(not(unix))]
fn lock_holder(_data_dir: &Path) -> Option<u32> {
    None
}

/// The server `start` recorded, when it is the one holding its data
/// directory's lock.
fn running() -> Result<Option<Started>> {
    let Ok(text) = std::fs::read_to_string(pid_file()?) else { return Ok(None) };
    let Ok(s) = serde_json::from_str::<Started>(&text) else { return Ok(None) };
    Ok((lock_holder(&s.data_dir) == Some(s.pid)).then_some(s))
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    false
}

#[cfg(unix)]
fn signal(pid: u32, sig: libc::c_int) {
    unsafe {
        libc::kill(pid as libc::pid_t, sig);
    }
}

#[cfg(not(unix))]
fn signal(_pid: u32, _sig: i32) {}

pub fn server(command: ServerCommand, at: Where) -> Result<()> {
    match command {
        ServerCommand::Start { args } => start(args, at),
        ServerCommand::Stop => stop(),
        ServerCommand::Status => status(),
        ServerCommand::Logs { lines } => {
            let text = std::fs::read_to_string(log_file()?).unwrap_or_default();
            let all: Vec<&str> = text.lines().collect();
            for l in &all[all.len().saturating_sub(lines)..] {
                println!("{l}");
            }
            Ok(())
        }
    }
}

/// `zsql serve`, detached, with its output in the log file. The project
/// directory is handed over explicitly, so the server keeps serving it
/// after this shell has moved on.
fn start(args: ServeArgs, at: Where) -> Result<()> {
    if let Some(s) = running()? {
        bail!("a server is already running as process {} on {}; `zsql server stop` first", s.pid, s.bind);
    }
    let home = home()?;
    std::fs::create_dir_all(&home)?;
    let data_dir = args.data_dir()?;
    let project = match (args.no_project, at.project.is_some()) {
        (true, _) => None,
        (false, true) => Some(at.project_dir()?.dir),
        (false, false) => at.project_dir().ok().map(|p| p.dir),
    };
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_file()?)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(args.to_argv(project.as_deref()));
    cmd.stdin(std::process::Stdio::null()).stdout(log.try_clone()?).stderr(log);
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // Its own session: closing this terminal does not take it down.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }
    let mut child = cmd.spawn().context("starting zsql serve")?;
    let started = Started { pid: child.id(), bind: args.bind.clone(), data_dir };
    std::fs::write(pid_file()?, serde_json::to_string(&started)?)?;
    // Give it a moment to refuse, so a bad bind or a held data
    // directory is reported here and not only in the log. The child is
    // reaped here, not just signalled: an exited child answers a signal
    // until it is.
    std::thread::sleep(Duration::from_millis(700));
    if let Some(status) = child.try_wait()? {
        let _ = std::fs::remove_file(pid_file()?);
        let text = std::fs::read_to_string(log_file()?).unwrap_or_default();
        let last = text.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        bail!("the server stopped at once ({status}); the log ends:\n{last}");
    }
    println!("started process {} on {}; log at {}", started.pid, started.bind, log_file()?.display());
    Ok(())
}

/// SIGTERM, which finishes what is in flight; then, if the server has
/// not gone in fifteen seconds, SIGKILL, since a request that long is
/// not worth a server that cannot be stopped.
fn stop() -> Result<()> {
    let Some(s) = running()? else {
        println!("no server is running");
        let _ = std::fs::remove_file(pid_file()?);
        return Ok(());
    };
    #[cfg(unix)]
    signal(s.pid, libc::SIGTERM);
    let t0 = Instant::now();
    let mut killed = false;
    while alive(s.pid) {
        if !killed && t0.elapsed() > Duration::from_secs(15) {
            #[cfg(unix)]
            signal(s.pid, libc::SIGKILL);
            killed = true;
        }
        if t0.elapsed() > Duration::from_secs(20) {
            bail!("process {} has not stopped", s.pid);
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(pid_file()?);
    if killed {
        println!("killed process {} after 15 s; it did not finish on its own", s.pid);
    } else {
        println!("stopped process {}", s.pid);
    }
    Ok(())
}

fn status() -> Result<()> {
    match running()? {
        Some(s) => {
            let server = format!("http://{}", s.bind);
            match remote::Client::new(&server, None).get("/healthz") {
                Ok(_) => println!("running as process {} on {}; it answers", s.pid, s.bind),
                Err(e) => println!("running as process {} on {}; it does not answer: {e}", s.pid, s.bind),
            }
        }
        None => println!("not running"),
    }
    Ok(())
}
