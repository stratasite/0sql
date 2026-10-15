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
use std::net::SocketAddr;
use std::path::PathBuf;
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
    let data_dir = match args.data_dir {
        Some(d) => d,
        None => home()?.join("data"),
    };
    let project = if args.no_project { None } else { at.project_dir().ok() };
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
            let client = remote::Client::new(&format!("http://{addr}"), None);
            let branch = config::git_branch(&p.dir).unwrap_or_else(|_| "main".into());
            eprintln!("zsql: serving {} as {}/{branch}; redeploying as its files change", p.dir.display(), p.uid);
            let mut last = crate::newest_mtime(&p.dir);
            let deploy = || -> Result<()> {
                let archive = project_archive(&p.dir)?;
                let v = client.post_bytes(&format!("{}/deploy", remote::branch_path(&p.uid, &branch)), "application/gzip", &archive)?;
                show::print_deploy(&v);
                Ok(())
            };
            if let Err(e) = deploy() {
                eprintln!("error: {e}");
            }
            while !server.is_finished() {
                std::thread::sleep(Duration::from_millis(500));
                let now = crate::newest_mtime(&p.dir);
                if now != last {
                    std::thread::sleep(Duration::from_millis(200));
                    println!();
                    if let Err(e) = deploy() {
                        eprintln!("error: {e}");
                    }
                    last = crate::newest_mtime(&p.dir);
                }
            }
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

fn pid_file() -> Result<PathBuf> {
    Ok(home()?.join("server.pid"))
}

fn log_file() -> Result<PathBuf> {
    Ok(home()?.join("server.log"))
}

/// The pid in the pidfile, when that process is still alive.
fn running() -> Result<Option<u32>> {
    let Ok(text) = std::fs::read_to_string(pid_file()?) else { return Ok(None) };
    let Ok(pid) = text.trim().parse::<u32>() else { return Ok(None) };
    Ok(alive(pid).then_some(pid))
}

#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 }
}

#[cfg(not(unix))]
fn alive(_pid: u32) -> bool {
    false
}

pub fn server(command: ServerCommand, at: Where) -> Result<()> {
    match command {
        ServerCommand::Start { args } => start(args, at),
        ServerCommand::Stop => stop(),
        ServerCommand::Status => status(at),
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
    if let Some(pid) = running()? {
        bail!("a server is already running as process {pid}; `zsql server stop` first");
    }
    let home = home()?;
    std::fs::create_dir_all(&home)?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(log_file()?)?;
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.arg("serve").arg("--bind").arg(&args.bind);
    if let Some(d) = &args.data_dir {
        cmd.arg("--data-dir").arg(d);
    }
    if args.accounts {
        cmd.arg("--accounts");
    }
    if args.open {
        cmd.arg("--open");
    }
    if args.no_project {
        cmd.arg("--no-project");
    } else if let Ok(p) = at.project_dir() {
        cmd.arg("--project").arg(&p.dir);
    }
    if let Some(u) = &args.public_url {
        cmd.arg("--public-url").arg(u);
    }
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
    let child = cmd.spawn().context("starting zsql serve")?;
    std::fs::write(pid_file()?, child.id().to_string())?;
    // Give it a moment to refuse, so a bad bind or a held data
    // directory is reported here and not only in the log.
    std::thread::sleep(Duration::from_millis(700));
    if !alive(child.id()) {
        let _ = std::fs::remove_file(pid_file()?);
        let text = std::fs::read_to_string(log_file()?).unwrap_or_default();
        let last = text.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
        bail!("the server stopped at once; the log ends:\n{last}");
    }
    println!("started process {} on {}; log at {}", child.id(), args.bind, log_file()?.display());
    Ok(())
}

fn stop() -> Result<()> {
    let Some(pid) = running()? else {
        println!("no server is running");
        let _ = std::fs::remove_file(pid_file()?);
        return Ok(());
    };
    #[cfg(unix)]
    unsafe {
        libc::kill(pid as libc::pid_t, libc::SIGTERM);
    }
    let t0 = Instant::now();
    while alive(pid) {
        if t0.elapsed() > Duration::from_secs(15) {
            bail!("process {pid} has not stopped after 15 s");
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    let _ = std::fs::remove_file(pid_file()?);
    println!("stopped process {pid}");
    Ok(())
}

fn status(at: Where) -> Result<()> {
    match running()? {
        Some(pid) => {
            let server = config::server(at.server.as_deref(), at.project_dir().ok().as_ref());
            match remote::Client::new(&server, None).get("/healthz") {
                Ok(_) => println!("running as process {pid}; {server} answers"),
                Err(e) => println!("running as process {pid}; {server} does not answer: {e}"),
            }
        }
        None => println!("not running"),
    }
    Ok(())
}
