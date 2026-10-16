//! `zsql serve`: the server, in this process, and `zsql server
//! start|stop|status|logs`, the same thing kept running in the
//! background.
//!
//! Inside a project, `serve` serves that project: once the server is
//! up it deploys the directory and redeploys as the files change, so
//! one command gives a teammate a live endpoint over the model they
//! are editing. Outside a project it serves whatever its data
//! directory holds. The server itself is the `zsql_server` library;
//! this module configures it and keeps it company. The settings come
//! from `ZSQL_*` variables, the environment file a deployment loads,
//! with flags overriding what they name; this is the one place in the
//! product a `ZSQL_*` variable is read.

use crate::{config, project, remote, show, Where};
use anyhow::{bail, Context, Result};
use clap::{Args, Subcommand};
use std::ffi::OsString;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zsql_server::limits::{Mode, Rate};
use zsql_server::{Config, MailConfig, McpSettings};

/// Where the server listens when nothing else is said: the address the
/// CLI's other commands talk to by default.
const DEFAULT_BIND: &str = "127.0.0.1:3699";

#[derive(Args, Clone, Debug)]
pub struct ServeArgs {
    /// Address to listen on; port 0 takes a free one, which is logged.
    #[arg(long, env = "ZSQL_BIND", default_value = DEFAULT_BIND)]
    pub bind: String,
    /// Where deployed models live; $ZSQL_HOME/data when neither this nor the variable says.
    #[arg(long, env = "ZSQL_DATA_DIR")]
    pub data_dir: Option<PathBuf>,
    /// Multi-tenant: keys, accounts, sign-up. Off, there is one tenant and no auth. ZSQL_ACCOUNTS=on means the same.
    #[arg(long, overrides_with = "no_accounts")]
    pub accounts: bool,
    /// One tenant and no auth, whatever ZSQL_ACCOUNTS says.
    #[arg(long)]
    pub no_accounts: bool,
    /// Allow listening beyond the loopback with accounts off, which lets anyone who can reach the port deploy and plan.
    #[arg(long)]
    pub open: bool,
    /// Serve only the data directory, even inside a project.
    #[arg(long)]
    pub no_project: bool,
    /// Where the app is reached from outside, for links and cookies; the bind address otherwise.
    #[arg(long, env = "ZSQL_PUBLIC_URL")]
    pub public_url: Option<String>,
}

impl ServeArgs {
    fn bind(&self) -> String {
        self.bind.clone()
    }

    /// `--accounts` and `--no-accounts` each win over the variable.
    fn accounts(&self) -> bool {
        if self.no_accounts {
            false
        } else {
            self.accounts || on("ZSQL_ACCOUNTS")
        }
    }

    /// Where deployed models live, as an absolute path: the background
    /// form records it, and `status` and `stop` read it from wherever
    /// they are run.
    fn data_dir(&self) -> Result<PathBuf> {
        let dir = match &self.data_dir {
            Some(d) => d.clone(),
            None => home()?.join("data"),
        };
        Ok(std::path::absolute(&dir).unwrap_or(dir))
    }

    /// The whole configuration: the environment's settings, with the
    /// flags over them.
    fn config(&self) -> Result<Config> {
        let accounts = self.accounts();
        let mut cfg = Config::local(self.data_dir()?, self.bind());
        cfg.accounts = accounts;
        cfg.signup_open = !matches!(var("ZSQL_SIGNUP").as_deref(), Some("closed" | "off" | "0"));
        cfg.public_url = self.public_url.clone();
        // The limits are about sharing one server between tenants, so
        // they start with the accounts.
        cfg.limits.mode = if accounts { Mode::On } else { Mode::Off };
        if let Some(v) = var("ZSQL_LIMITS") {
            cfg.limits.mode = Mode::parse(&v);
        }
        let l = &mut cfg.limits;
        for (name, slot) in [
            ("ZSQL_LIMIT_PLAN", &mut l.plan),
            ("ZSQL_LIMIT_EXPLORE", &mut l.explore),
            ("ZSQL_LIMIT_HEAVY", &mut l.heavy),
            ("ZSQL_LIMIT_MANAGE", &mut l.manage),
            ("ZSQL_LIMIT_KEY_PLAN", &mut l.key_plan),
            ("ZSQL_LIMIT_KEY_EXPLORE", &mut l.key_explore),
            ("ZSQL_LIMIT_LOGIN", &mut l.login),
            ("ZSQL_LIMIT_SIGNUP", &mut l.signup),
            ("ZSQL_LIMIT_INVITE", &mut l.invite),
            ("ZSQL_LIMIT_RESET", &mut l.reset),
            ("ZSQL_LIMIT_BAD_KEY", &mut l.bad_key),
            ("ZSQL_LIMIT_HANDSHAKE", &mut l.handshake),
        ] {
            rate(name, slot)?;
        }
        if let Some(v) = var("ZSQL_LIMIT_EMAIL_FAILURES") {
            l.email_failures = if v.eq_ignore_ascii_case("off") {
                0
            } else {
                v.parse().with_context(|| format!("ZSQL_LIMIT_EMAIL_FAILURES: {v:?} is not a number"))?
            };
        }
        if let Some(v) = var("ZSQL_LIMIT_EMAIL_WINDOW") {
            l.email_window = Duration::from_secs(number(&v, "ZSQL_LIMIT_EMAIL_WINDOW")? as u64);
        }
        l.trusted_proxy = on("ZSQL_TRUSTED_PROXY");

        let a = &mut cfg.admission;
        for (name, slot) in [
            ("ZSQL_ADMIT_PLAN", &mut a.plan),
            ("ZSQL_ADMIT_EXPLORE", &mut a.explore),
            ("ZSQL_ADMIT_HEAVY", &mut a.heavy),
            ("ZSQL_ADMIT_MANAGE", &mut a.manage),
            ("ZSQL_ADMIT_PASSWORD", &mut a.password),
        ] {
            if let Some(v) = var(name) {
                *slot = number(&v, name)?;
            }
        }
        for (name, slot) in
            [("ZSQL_MAX_BRANCHES_PER_PROJECT", &mut cfg.caps.per_project), ("ZSQL_MAX_BRANCHES_PER_ACCOUNT", &mut cfg.caps.per_account)]
        {
            match var(name) {
                Some(v) if v.eq_ignore_ascii_case("off") => *slot = usize::MAX,
                Some(v) => *slot = number(&v, name)?,
                None => {}
            }
        }

        cfg.mail =
            MailConfig { command: var("ZSQL_MAIL_COMMAND"), from: var("ZSQL_MAIL_FROM").unwrap_or_default(), ..MailConfig::default() };
        if let Some(v) = var("ZSQL_MAIL_TIMEOUT") {
            cfg.mail.timeout = Duration::from_secs(number(&v, "ZSQL_MAIL_TIMEOUT")? as u64);
        }

        if let Some(v) = var("ZSQL_AUTH_CACHE_TTL") {
            cfg.auth_cache_ttl = Duration::from_secs(number(&v, "ZSQL_AUTH_CACHE_TTL")? as u64);
        }
        let tools = var("ZSQL_MCP_TOOLS").map(|v| v.split(',').map(|t| t.trim().to_string()).filter(|t| !t.is_empty()).collect());
        cfg.mcp = McpSettings::new(file("ZSQL_MCP_PROMPTS")?.as_deref(), file("ZSQL_MCP_SPEC_SCHEMA")?.as_deref(), tools)
            .map_err(|e| anyhow::anyhow!(e))?;
        Ok(cfg)
    }

    /// These arguments as `zsql serve` would take them, for the
    /// background form to hand on; kept next to the fields so a new
    /// flag cannot be forgotten here.
    fn to_argv(&self, project: Option<&Path>) -> Result<Vec<OsString>> {
        let mut argv: Vec<OsString> = vec!["serve".into(), "--bind".into(), self.bind.clone().into()];
        argv.extend(["--data-dir".into(), self.data_dir()?.into()]);
        if self.accounts {
            argv.push("--accounts".into());
        }
        if self.no_accounts {
            argv.push("--no-accounts".into());
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
        Ok(argv)
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
    /// Whether the background server is running, and whether it answers;
    /// also whether a foreground `zsql serve` holds the data directory.
    Status {
        /// The data directory to look at for a foreground server; $ZSQL_HOME/data when neither this nor the variable says.
        #[arg(long, env = "ZSQL_DATA_DIR")]
        data_dir: Option<PathBuf>,
    },
    /// The last lines of the background server's log.
    Logs {
        #[arg(long, default_value_t = 100)]
        lines: usize,
    },
}

fn home() -> Result<PathBuf> {
    config::home().context("neither ZSQL_HOME nor HOME is set")
}

// ---------------------------------------------------- the environment

fn var(name: &str) -> Option<String> {
    std::env::var(name).ok().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn on(name: &str) -> bool {
    matches!(var(name).as_deref(), Some("on" | "1" | "true"))
}

fn number(v: &str, name: &str) -> Result<usize> {
    v.parse().with_context(|| format!("{name}: {v:?} is not a number"))
}

fn rate(name: &str, slot: &mut Rate) -> Result<()> {
    if let Some(v) = var(name) {
        *slot = Rate::parse(&v).map_err(|e| anyhow::anyhow!("{name}: {e}"))?;
    }
    Ok(())
}

/// A file named by a variable, read whole, or nothing.
fn file(name: &str) -> Result<Option<String>> {
    match var(name) {
        Some(path) => {
            let text = std::fs::read_to_string(&path).with_context(|| format!("{name}: cannot read {path}"))?;
            eprintln!("zsqld: mcp: {name} read from {path}");
            Ok(Some(text))
        }
        None => Ok(None),
    }
}

fn loopback(bind: &str) -> bool {
    bind.parse::<SocketAddr>().map(|a| a.ip().is_loopback()).unwrap_or(false) || bind.starts_with("localhost:")
}

/// Runs the server here until it is stopped. Inside a project, and with
/// accounts off, the project is deployed to it and redeployed as its
/// files change.
pub fn serve(args: ServeArgs, at: Where) -> Result<()> {
    let (bind, accounts) = (args.bind(), args.accounts());
    if !accounts && !args.open && !loopback(&bind) {
        bail!("{bind} is not a loopback address, and with accounts off anyone who reaches it may deploy and plan; add --accounts, or --open to mean it");
    }
    // A project named explicitly must be there; one looked for is optional.
    let project = match (args.no_project, at.project.is_some()) {
        (true, _) => None,
        (false, true) => Some(at.project_dir()?),
        (false, false) => at.project_dir().ok(),
    };
    let cfg = args.config()?;

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

    match (project, accounts) {
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
                let archive = project::archive(&p.dir)?;
                let v = client.post_bytes(&format!("{}/deploy", remote::branch_path(&p.uid, &branch)), "application/gzip", &archive)?;
                show::print_deploy(&v);
                Ok(())
            };
            if let Err(e) = deploy() {
                eprintln!("error: {e}");
            }
            project::watch(
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
        }
        (None, _) => {}
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

/// Who holds a data directory's lock: the server itself writes its pid
/// there under the lock, so this is the one answer that a stale pidfile
/// or a reused pid cannot fake.
enum Lock {
    /// Nobody: the file could be opened and locked.
    Free,
    /// A process, by the pid it wrote.
    Held(Option<u32>),
    /// The file could not be opened, so nothing can be said.
    Unreachable,
}

#[cfg(unix)]
fn lock_holder(data_dir: &Path) -> Lock {
    use std::io::Read;
    use std::os::unix::io::AsRawFd;
    let Ok(mut file) = std::fs::OpenOptions::new().read(true).write(true).open(data_dir.join("lock")) else { return Lock::Unreachable };
    if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } == 0 {
        unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_UN) };
        return Lock::Free;
    }
    let mut text = String::new();
    let _ = file.read_to_string(&mut text);
    Lock::Held(text.trim().parse().ok())
}

#[cfg(not(unix))]
fn lock_holder(_data_dir: &Path) -> Lock {
    Lock::Unreachable
}

/// The server `start` recorded, when it is the one holding its data
/// directory's lock. When the lock cannot be reached, the recorded pid
/// is believed only while that process exists.
fn running() -> Result<Option<Started>> {
    let Ok(text) = std::fs::read_to_string(pid_file()?) else { return Ok(None) };
    let Ok(s) = serde_json::from_str::<Started>(&text) else { return Ok(None) };
    let yes = match lock_holder(&s.data_dir) {
        Lock::Held(pid) => pid == Some(s.pid),
        Lock::Free => false,
        Lock::Unreachable => alive(s.pid),
    };
    Ok(yes.then_some(s))
}

/// Whether a process exists. Not being allowed to signal it still means
/// it exists; only "no such process" means it does not.
#[cfg(unix)]
fn alive(pid: u32) -> bool {
    unsafe { libc::kill(pid as libc::pid_t, 0) == 0 || *libc::__errno_location() != libc::ESRCH }
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
        ServerCommand::Status { data_dir } => status(data_dir),
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
    let project = project.map(|p| std::path::absolute(&p).unwrap_or(p));
    let log_path = log_file()?;
    let log = std::fs::OpenOptions::new().create(true).append(true).open(&log_path)?;
    let log_len = log.metadata().map(|m| m.len()).unwrap_or(0);
    let mut cmd = std::process::Command::new(std::env::current_exe()?);
    cmd.args(args.to_argv(project.as_deref())?);
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
    let mut started = Started { pid: child.id(), bind: args.bind(), data_dir };
    std::fs::write(pid_file()?, serde_json::to_string(&started)?)?;
    // Wait for it to say where it listens, which is also when a bad
    // bind or a held data directory has been refused. The child is
    // reaped here, not just signalled, since an exited child answers a
    // signal until it is.
    let t0 = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            let _ = std::fs::remove_file(pid_file()?);
            let text = std::fs::read_to_string(&log_path).unwrap_or_default();
            let last = text.lines().rev().take(5).collect::<Vec<_>>().into_iter().rev().collect::<Vec<_>>().join("\n");
            bail!("the server stopped at once ({status}); the log ends:\n{last}");
        }
        let text = std::fs::read_to_string(&log_path).unwrap_or_default();
        let since = text.get(log_len as usize..).unwrap_or("");
        if let Some(addr) = since.lines().find_map(|l| l.trim().strip_prefix("zsqld: listening on http://")) {
            // The address actually bound, which a bind of port 0 does not say.
            started.bind = addr.to_string();
            std::fs::write(pid_file()?, serde_json::to_string(&started)?)?;
            break;
        }
        if t0.elapsed() > Duration::from_secs(10) {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    println!("started process {} on {}; log at {}", started.pid, started.bind, log_path.display());
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

/// The server `start` recorded, if it is running; failing that, the
/// data directory's lock, which a foreground `zsql serve` holds too.
/// That server was not started here, so `stop` will not stop it, and
/// its address is not on record, so it is not probed.
fn status(data_dir: Option<PathBuf>) -> Result<()> {
    if let Some(s) = running()? {
        let server = format!("http://{}", s.bind);
        match remote::Client::new(&server, None).get("/healthz") {
            Ok(_) => println!("running as process {} on {}; it answers", s.pid, s.bind),
            Err(e) => println!("running as process {} on {}; it does not answer: {e}", s.pid, s.bind),
        }
        return Ok(());
    }
    let dir = match data_dir {
        Some(d) => d,
        None => home()?.join("data"),
    };
    let dir = std::path::absolute(&dir).unwrap_or(dir);
    match lock_holder(&dir) {
        Lock::Held(Some(pid)) => {
            println!("not started by `zsql server start`, but process {pid} holds {} and is serving it in the foreground", dir.display())
        }
        Lock::Held(None) => println!("not started by `zsql server start`, but another process holds {}", dir.display()),
        Lock::Free | Lock::Unreachable => println!("not running (nothing holds {})", dir.display()),
    }
    Ok(())
}
