//! Starting and running a server: what it is told, the data directory
//! and its lock, the deployments reloaded from disk, the router, and the
//! graceful stop.

use crate::auth::guard;
use crate::deploy::is_swap_leftover;
use crate::limits::{self, Admission, Limiter};
use crate::{accounts_api, deploy, mail, mcp, reads, ui, write, App, Caps, Shared};
use axum::routing::{delete, get, post};
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use zsql_accounts::Store;
use zsql_dialect::Dialects;
use zsql_service::Model;

const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
const JSON_LIMIT: usize = 1024 * 1024;

pub(crate) fn reload(app: &App) -> anyhow::Result<(usize, Vec<String>)> {
    let mut n = 0;
    let mut skipped: Vec<String> = Vec::new();
    // Single-tenant: data/<project>/<branch>; with accounts: data/<account>/<project>/<branch>.
    let roots: Vec<(String, PathBuf)> = if app.accounts.is_some() {
        let Ok(accounts) = std::fs::read_dir(&app.data_dir) else { return Ok((0, skipped)) };
        accounts.flatten().filter(|e| e.path().is_dir()).map(|e| (format!("{}/", e.file_name().to_string_lossy()), e.path())).collect()
    } else {
        vec![(String::new(), app.data_dir.clone())]
    };
    for (scope, root) in roots {
        let Ok(projects) = std::fs::read_dir(&root) else { continue };
        for p in projects.flatten().filter(|e| e.path().is_dir()) {
            let uid = p.file_name().to_string_lossy().to_string();
            let Ok(branches) = std::fs::read_dir(p.path()) else { continue };
            for b in branches.flatten().filter(|e| e.path().is_dir()) {
                let branch = b.file_name().to_string_lossy().to_string();
                // A branch name never starts with a dot. A deploy's own
                // staging or previous copy, left by a crash between two
                // renames, is swept; any other dot directory is somebody
                // else's and is left alone, since a data directory can be
                // pointed anywhere by a typo.
                if branch.starts_with('.') {
                    if is_swap_leftover(&branch) {
                        let _ = std::fs::remove_dir_all(b.path());
                    }
                    continue;
                }
                if !b.path().join(zsql_service::SNAPSHOT_FILE).exists() {
                    continue;
                }
                // One unreadable branch is not a reason to refuse to
                // start. A model that will not load is almost always a
                // format this build is too old or too new for, and the
                // archive it was compiled from lives in someone's
                // repository, so the fix is a redeploy of that one branch.
                // Coming up without it, and saying which, beats a restart
                // loop that systemd gives up on after five tries.
                let key = format!("{scope}{uid}/{branch}");
                match Model::from_stored(&b.path(), &uid, &branch, app.dialects) {
                    Ok(m) => {
                        write(&app.models).insert(key, Arc::new(m));
                        n += 1;
                    }
                    Err(e) => {
                        say!("zsqld: {key} was not loaded: {e}");
                        skipped.push(key);
                    }
                }
            }
        }
    }
    Ok((n, skipped))
}

/// Every route, behind the guard, over the app. Built here so the
/// tests drive the same router the binary serves.
pub(crate) fn router(app: Shared) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(ui::healthz))
        .route("/projects", get(reads::projects))
        .route("/projects/{uid}/branches/{branch}", get(reads::branch_summary))
        .route("/projects/{uid}/branches/{branch}/deploy", post(deploy::deploy).layer(axum::extract::DefaultBodyLimit::max(ARCHIVE_LIMIT)))
        .route(
            "/projects/{uid}/branches/{branch}/validate",
            post(deploy::validate).layer(axum::extract::DefaultBodyLimit::max(ARCHIVE_LIMIT)),
        )
        .route("/projects/{uid}/branches/{branch}/test", post(reads::test))
        .route("/projects/{uid}/branches/{branch}", delete(deploy::remove))
        .route("/projects/{uid}/branches/{branch}/sql", post(reads::sql))
        .route("/projects/{uid}/branches/{branch}/explain", post(reads::explain))
        .route("/projects/{uid}/branches/{branch}/explore", post(reads::explore))
        .route("/projects/{uid}/branches/{branch}/fields", get(reads::fields))
        .route("/projects/{uid}/branches/{branch}/tables", get(reads::tables))
        .route("/projects/{uid}/branches/{branch}/values/{dim}", get(reads::values))
        .route(mcp::PATH, post(mcp::endpoint))
        .route("/", get(ui::root))
        .route("/app", get(ui::ui_root))
        .route("/app/", get(ui::ui_root))
        .route("/app/{*rest}", get(ui::ui))
        .route("/signup", post(accounts_api::signup))
        .route("/login", post(accounts_api::login))
        .route("/logout", post(accounts_api::logout))
        .route("/account/password", post(accounts_api::set_password))
        .route("/password-reset", post(accounts_api::request_reset))
        .route("/password-reset/accept", post(accounts_api::accept_reset))
        .route("/verify-email", post(accounts_api::verify_email))
        .route("/account/verify-email", post(accounts_api::resend_verification))
        .route("/me", get(accounts_api::me))
        .route("/account/members", get(accounts_api::members).post(accounts_api::add_member))
        .route("/account/members/{email}", delete(accounts_api::remove_member))
        .route("/account/members/{email}/invite", post(accounts_api::invite))
        .route("/invites/accept", post(accounts_api::accept_invite))
        .route("/account/keys", get(accounts_api::user_keys).post(accounts_api::create_user_key))
        .route("/account/keys/{id}", delete(accounts_api::revoke_user_key))
        .route("/account/query-keys", get(accounts_api::query_keys).post(accounts_api::create_query_key))
        .route("/account/query-keys/{id}", delete(accounts_api::revoke_query_key))
        .route("/account/query-keys/{id}/rotate", post(accounts_api::rotate_query_key))
        .route("/account/query-keys/{id}/grants", post(accounts_api::grant))
        .route("/account/query-keys/{id}/grants/{project}", delete(accounts_api::revoke_grant))
        .route("/account/audit", get(accounts_api::audit))
        .route("/projects/{uid}", axum::routing::patch(accounts_api::update_project).delete(accounts_api::delete_project))
        .route("/projects/{uid}/access", get(accounts_api::project_access).put(accounts_api::set_access))
        .route("/projects/{uid}/access/{email}", delete(accounts_api::remove_access));
    for name in ui::SITE_FILES {
        router = router.route(&format!("/{name}"), get(ui::site_file));
    }
    router
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        // Every route but a deploy takes a small JSON body; the archive
        // routes raise their own limit above.
        .layer(axum::extract::DefaultBodyLimit::max(JSON_LIMIT))
        .with_state(app)
}

/// Everything the server is told. `zsql serve` fills it from `ZSQL_*`
/// variables and flags. The library itself reads no environment, so a
/// flag and a variable can only ever mean the same thing, and two
/// servers can start in one test process.
pub struct Config {
    pub data_dir: PathBuf,
    /// `host:port`; port 0 takes a free one, and the chosen address is
    /// logged.
    pub bind: String,
    /// Multi-tenant: every request carries a key, deployments are
    /// scoped to an account. Off is one tenant and no auth.
    pub accounts: bool,
    pub signup_open: bool,
    /// Where the app is reached from outside, for links and cookies;
    /// the bind address when `None`. `https://` marks cookies Secure.
    pub public_url: Option<String>,
    pub limits: limits::Config,
    pub admission: limits::AdmissionCounts,
    pub caps: Caps,
    pub mail: MailConfig,
    pub mcp: mcp::Settings,
    /// How long an authentication decision may be reused; zero turns
    /// the cache off.
    pub auth_cache_ttl: Duration,
}

/// How mail goes out: a command fed the message on stdin, or nothing,
/// in which case links are written to the log.
#[derive(Debug, Clone)]
pub struct MailConfig {
    pub command: Option<String>,
    pub from: String,
    pub timeout: Duration,
}

impl Default for MailConfig {
    fn default() -> Self {
        MailConfig { command: None, from: String::new(), timeout: Duration::from_secs(20) }
    }
}

impl Config {
    /// A server on its own machine: one tenant, no auth, no limits, the
    /// compiled-in MCP wording.
    pub fn local(data_dir: PathBuf, bind: String) -> Config {
        Config {
            data_dir,
            bind,
            accounts: false,
            signup_open: true,
            public_url: None,
            limits: limits::Config { mode: limits::Mode::Off, ..limits::Config::default() },
            admission: limits::AdmissionCounts::default(),
            caps: Caps::default(),
            mail: MailConfig::default(),
            mcp: mcp::Settings::builtin(),
            auth_cache_ttl: zsql_accounts::DEFAULT_AUTH_CACHE_TTL,
        }
    }
}

/// Runs the server until SIGTERM or ctrl-c, then finishes what is in
/// flight and returns.
pub async fn run(config: Config) -> anyhow::Result<()> {
    serve(config, |_| {}).await
}

/// [`run`], telling `listening` the address once it is bound: the
/// port a bind of `:0` was given, which is what a caller that then
/// talks to the server needs.
pub async fn serve(config: Config, listening: impl FnOnce(SocketAddr) + Send + 'static) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let mut lock = lock_data_dir(&config.data_dir)?;
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    let accounts_on = config.accounts;
    let accounts = if accounts_on { Some(Store::open_with(&config.data_dir.join("accounts.db"), config.auth_cache_ttl)?) } else { None };
    let advertised = config.public_url.clone();
    let public_url = config.public_url.clone().unwrap_or_else(|| format!("http://{}", config.bind));
    let secure_cookies = public_url.starts_with("https://");
    let limits = Limiter::new(config.limits);
    let admission = Admission::new(&config.admission);
    let mailer =
        mail::Mailer::new(config.mail.command, &config.mail.from, config.mail.timeout, &public_url).map_err(|e| anyhow::anyhow!("{e}"))?;
    let limits_mode = if limits.on() {
        if limits.cfg.mode == limits::Mode::Shadow {
            "shadow"
        } else {
            "on"
        }
    } else {
        "off"
    };
    let mail_state = if mailer.configured() { "on" } else { "off, links to the journal" };
    let app: Shared = Arc::new(App {
        dialects,
        data_dir: config.data_dir,
        models: RwLock::new(HashMap::new()),
        accounts,
        signup_open: config.signup_open,
        secure_cookies,
        limits,
        admission,
        caps: config.caps,
        mailer,
        mcp: config.mcp,
        deploying: std::sync::Mutex::new(()),
    });
    let (n, skipped) = reload(&app)?;
    say!(
        "zsqld: {n} deployment(s) loaded from {}; accounts {}; rate limits {limits_mode}; mail {mail_state}",
        app.data_dir.display(),
        if accounts_on {
            if app.signup_open {
                "on, sign-up open"
            } else {
                "on, sign-up closed"
            }
        } else {
            "off"
        }
    );
    if !skipped.is_empty() {
        // Said again, together, after the per-branch lines above: the
        // one line someone watching a restart will actually read.
        say!("zsqld: {} deployment(s) could not be loaded and are not being served: {}", skipped.len(), skipped.join(", "));
    }
    if accounts_on {
        let cap = |n: usize| if n == usize::MAX { "off".to_string() } else { n.to_string() };
        say!("zsqld: branch caps: {} per project, {} per account", cap(app.caps.per_project), cap(app.caps.per_account));
    }

    let router = router(app.clone());

    // Buckets for callers that have gone quiet, and failure windows that
    // have passed, are dropped now and then so an idle server does not
    // hold a row per address it has ever seen.
    let sweeper = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tick.tick().await;
            sweeper.limits.sweep();
        }
    });

    // The handlers are installed before the address is announced, so a
    // signal sent the moment the listening line is read is a graceful
    // stop and not the default disposition's instant death.
    let shutdown = shutdown_signal();
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    // The address actually bound, so a port of 0 is answered with the
    // port it got.
    let addr = listener.local_addr()?;
    say!("zsqld: listening on http://{addr}");
    // The address, and the public URL when there is one, join the pid
    // in the lock file, so `zsql server status` can say where a server
    // it did not start is reached.
    {
        use std::io::{Seek, Write};
        let public = advertised.as_deref().unwrap_or("");
        let _ = lock.set_len(0).and_then(|()| lock.rewind()).and_then(|()| write!(lock, "{}\n{addr}\n{public}\n", std::process::id()));
    }
    listening(addr);
    // Connect info: the per-address limits need the peer's address.
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown).await?;
    say!("zsqld: stopped");
    Ok(())
}

/// [`serve`] on a runtime of its own, for a caller that is not async.
pub fn run_blocking(config: Config, listening: impl FnOnce(SocketAddr) + Send + 'static) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve(config, listening))
}

/// Resolves on SIGTERM or ctrl-c. What is in flight finishes; nothing
/// new is accepted. The handlers are installed when this is called,
/// not when the future is first polled.
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    let mut streams = {
        use tokio::signal::unix::{signal, SignalKind};
        (signal(SignalKind::terminate()).ok(), signal(SignalKind::interrupt()).ok())
    };
    async move {
        #[cfg(unix)]
        {
            let (term, int) = (&mut streams.0, &mut streams.1);
            let terminate = async {
                match term {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            let interrupt = async {
                match int {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = interrupt => {}
                _ = terminate => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        say!("zsqld: stopping");
    }
}

/// One server per data directory. The lock file holds the pid of the
/// server that has it, so the refusal can say who, and once it is bound
/// the address and the public URL, so `zsql server status` can say where.
fn lock_data_dir(dir: &std::path::Path) -> anyhow::Result<std::fs::File> {
    use std::io::{Read, Write};
    let path = dir.join("lock");
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        // A `zsql server status` probes the lock by taking it for a
        // moment; one short retry keeps that from refusing a server.
        let taken = (0..2).any(|i| {
            if i > 0 {
                std::thread::sleep(Duration::from_millis(100));
            }
            unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) == 0 }
        });
        if !taken {
            let mut other = String::new();
            let _ = file.read_to_string(&mut other);
            let pid = other.lines().next().unwrap_or("").trim();
            anyhow::bail!(
                "{} is already served by process {}; stop that server, or give this one another data directory",
                dir.display(),
                if pid.is_empty() { "?" } else { pid }
            );
        }
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    Ok(file)
}

#[cfg(test)]
mod tests {
    /// The library, and the accounts crate it links, are configured,
    /// never by the environment: a `zsql serve` flag and a `ZSQL_*`
    /// variable must mean the same thing, so there is one place that
    /// reads variables and it is the CLI's serve module.
    #[test]
    fn the_library_reads_no_environment() {
        let needle = ["env::", "var("].concat();
        let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("..");
        let mut sources: Vec<_> = std::fs::read_dir(root.join("server/src")).unwrap().flatten().map(|e| e.path()).collect();
        sources.push(root.join("accounts/src/lib.rs"));
        for path in sources.iter().filter(|p| p.extension().is_some_and(|x| x == "rs")) {
            let text = std::fs::read_to_string(path).unwrap();
            assert!(!text.contains(&needle), "{} reads the environment", path.display());
        }
    }
}
