//! Starting and running a server: what it is told, the data directory
//! and its lock, the deployments reloaded from disk, the router, and the
//! graceful stop.

use crate::admission::{Admission, AdmissionCounts};
use crate::deploy::is_swap_leftover;
use crate::guard::guard;
use crate::{deploy, mcp, probe, reads, write, App, Shared};
use axum::routing::{delete, get, post};
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use zsql_dialect::Dialects;
use zsql_service::Model;

const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
const JSON_LIMIT: usize = 1024 * 1024;

/// What the data directory holds, loaded: `<project>/<branch>/`.
pub(crate) fn reload(app: &App) -> anyhow::Result<(usize, Vec<String>)> {
    let mut n = 0;
    let mut skipped: Vec<String> = Vec::new();
    let Ok(projects) = std::fs::read_dir(&app.data_dir) else { return Ok((0, skipped)) };
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
            let key = crate::key(&uid, &branch);
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
    Ok((n, skipped))
}

/// Every route, behind the guard, over the app. Built here so the
/// tests drive the same router the binary serves.
pub(crate) fn router(app: Shared) -> Router {
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
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
        .route("/projects/{uid}/branches/{branch}/datasources", get(probe::datasources))
        .route("/projects/{uid}/branches/{branch}/datasources/{datasource}/tables", post(probe::tables))
        .route("/projects/{uid}/branches/{branch}/datasources/{datasource}/tables/{table}", post(probe::table))
        .route(mcp::PATH, post(mcp::endpoint))
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        // Every route but a deploy takes a small JSON body; the archive
        // routes raise their own limit above.
        .layer(axum::extract::DefaultBodyLimit::max(JSON_LIMIT))
        .with_state(app)
}

async fn root() -> String {
    format!("zsqld {}; see /healthz", env!("CARGO_PKG_VERSION"))
}

async fn healthz() -> &'static str {
    "ok"
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
    /// Where the server is reached from outside, when that is not the
    /// bind address. Nothing here uses it; it is written beside the pid
    /// so `zsql server status` can say it.
    pub public_url: Option<String>,
    pub admission: AdmissionCounts,
    pub mcp: mcp::Settings,
    /// With a token, a deploy, a validate, a test run and a removal each
    /// need `Authorization: Bearer <token>`. Without one they are as
    /// open as the reads, which is right on a loopback and nowhere else.
    pub deploy_token: Option<String>,
}

impl Config {
    /// A server on its own machine: no deploy token, the compiled-in
    /// MCP wording.
    pub fn local(data_dir: PathBuf, bind: String) -> Config {
        Config {
            data_dir,
            bind,
            public_url: None,
            admission: AdmissionCounts::default(),
            mcp: mcp::Settings::builtin(),
            deploy_token: None,
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
    let advertised = config.public_url.clone();
    let deploy_token = config.deploy_token.map(|t| t.trim().to_string()).filter(|t| !t.is_empty());
    let app: Shared = Arc::new(App {
        dialects,
        data_dir: config.data_dir,
        models: RwLock::new(HashMap::new()),
        admission: Admission::new(&config.admission),
        mcp: config.mcp,
        deploy_token,
        deploying: std::sync::Mutex::new(()),
    });
    let (n, skipped) = reload(&app)?;
    say!(
        "zsqld: {n} deployment(s) loaded from {}; deploys {}",
        app.data_dir.display(),
        if app.deploy_token.is_some() { "need the deploy token" } else { "are open to whoever reaches this address" }
    );
    if !skipped.is_empty() {
        // Said again, together, after the per-branch lines above: the
        // one line someone watching a restart will actually read.
        say!("zsqld: {} deployment(s) could not be loaded and are not being served: {}", skipped.len(), skipped.join(", "));
    }
    // A data directory from a release that had accounts keeps its
    // deployments one level down, under each account, where nothing
    // looks now. Said once, so an upgrade that serves nothing says why.
    if app.data_dir.join("accounts.db").exists() {
        say!(
            "zsqld: {} holds accounts.db from a release with accounts. It is not read, and deployments under an account's directory are not served: deploy them again, or move <account>/<project> up one level.",
            app.data_dir.display()
        );
    }

    let router = router(app.clone());

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
    axum::serve(listener, router).with_graceful_shutdown(shutdown).await?;
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
    /// The library is configured, never by the environment: a `zsql
    /// serve` flag and a `ZSQL_*` variable must mean the same thing, so
    /// there is one place that reads variables and it is the CLI's
    /// serve module.
    #[test]
    fn the_library_reads_no_environment() {
        let needle = ["env::", "var("].concat();
        let src = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("src");
        let sources: Vec<_> = std::fs::read_dir(src).unwrap().flatten().map(|e| e.path()).collect();
        for path in sources.iter().filter(|p| p.extension().is_some_and(|x| x == "rs")) {
            let text = std::fs::read_to_string(path).unwrap();
            assert!(!text.contains(&needle), "{} reads the environment", path.display());
        }
    }
}
