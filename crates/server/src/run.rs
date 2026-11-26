//! Starting and running a server: what it is told, the data directory
//! and its lock, the deployments reloaded from disk, the router, and the
//! graceful stop.

use crate::admission::{Admission, AdmissionCounts};
use crate::deploy::is_swap_leftover;
use crate::guard::guard;
use crate::{datasets, deploy, exec, mcp, probe, reads, write, App, Shared};
use axum::routing::{delete, get, post};
use axum::Router;
use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use zsql_core::Model;
use zsql_dialect::Dialects;

const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
const JSON_LIMIT: usize = 1024 * 1024;

/// The directories of `dir`, by name.
fn dirs_of(dir: &std::path::Path) -> Vec<(String, PathBuf)> {
    let Ok(entries) = std::fs::read_dir(dir) else { return Vec::new() };
    let mut out: Vec<(String, PathBuf)> = entries
        .flatten()
        .map(|e| e.path())
        .filter(|p| p.is_dir())
        .map(|p| (p.file_name().unwrap_or_default().to_string_lossy().to_string(), p))
        .collect();
    out.sort();
    out
}

/// What the data directory holds, loaded: `<org>/<project>/<branch>/`.
///
/// An org is its directory's name and nothing else, so a directory
/// renamed while the server is down is that org under its new name the
/// next time it starts: `default` renamed to `acme` is how what was
/// deployed with no org becomes acme's.
pub(crate) fn reload(app: &App) -> anyhow::Result<(usize, Vec<String>)> {
    let mut n = 0;
    let mut skipped: Vec<String> = Vec::new();
    for (org, org_dir) in dirs_of(&app.data_dir) {
        // The server's own directories start with a dot, and so may
        // anything else a data directory pointed at the wrong place
        // holds: neither is an org.
        if org.starts_with('.') {
            continue;
        }
        // A directory an org could not be called is said, not passed
        // over: it is most likely `default` renamed to a name with a
        // capital or a dot in it, and its projects are not served.
        if !crate::names::is_slug(&org) {
            tracing::warn!(
                "{} is not served: '{org}' is not a name an org can have (lower-case letters, digits, - and _)",
                org_dir.display()
            );
            skipped.push(org);
            continue;
        }
        for (uid, project_dir) in dirs_of(&org_dir) {
            // A model one level up from where a branch keeps it is a
            // deployment from before there were orgs, when the data
            // directory held `<project>/<branch>/`. It is not moved
            // and not served: said, so it is not simply missing.
            if project_dir.join(zsql_core::SNAPSHOT_FILE).exists() {
                tracing::warn!(
                    "{org}/{uid} is a deployment laid out as <project>/<branch>, from before orgs: move {} under {}/ or deploy it again",
                    org_dir.display(),
                    app.data_dir.join(crate::names::DEFAULT_ORG).display()
                );
                skipped.push(format!("{org}/{uid}"));
                continue;
            }
            n += reload_project(app, &org, &uid, &project_dir, &mut skipped);
        }
    }
    Ok((n, skipped))
}

/// The branches of one project, loaded; how many were.
fn reload_project(app: &App, org: &str, uid: &str, project_dir: &std::path::Path, skipped: &mut Vec<String>) -> usize {
    let mut n = 0;
    for (branch, dir) in dirs_of(project_dir) {
        // A branch name never starts with a dot. A deploy's own
        // staging or previous copy, left by a crash between two
        // renames, is swept; any other dot directory is somebody
        // else's and is left alone, since a data directory can be
        // pointed anywhere by a typo.
        if branch.starts_with('.') {
            if is_swap_leftover(&branch) {
                let _ = std::fs::remove_dir_all(&dir);
            }
            continue;
        }
        if !dir.join(zsql_core::SNAPSHOT_FILE).exists() {
            continue;
        }
        // One unreadable branch is not a reason to refuse to start. A
        // model that will not load is almost always a format this
        // build is too old or too new for, and the archive it was
        // compiled from lives in someone's repository, so the fix is a
        // redeploy of that one branch. Coming up without it, and
        // saying which, beats a restart loop that systemd gives up on
        // after five tries.
        let key = crate::key(org, uid, &branch);
        match Model::from_stored(&dir, uid, &branch, app.dialects) {
            Ok(m) => {
                write(&app.models).insert(key, Arc::new(m));
                n += 1;
            }
            Err(e) => {
                tracing::warn!("{key} was not loaded: {e}");
                skipped.push(key);
            }
        }
    }
    n
}

/// Every route, behind the guard, over the app. Built here so the
/// tests drive the same router the binary serves.
pub(crate) fn router(app: Shared) -> Router {
    let upload_limit = app.prep.as_ref().map(|p| p.settings().max_bytes as usize).unwrap_or(JSON_LIMIT);
    Router::new()
        .route("/", get(root))
        .route("/healthz", get(healthz))
        .route("/orgs", get(reads::orgs))
        // Once for the default org, and once for the org the path names.
        .merge(of_an_org(upload_limit))
        .nest("/orgs/{org}", of_an_org(upload_limit))
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        // Every route but a deploy takes a small JSON body; the archive
        // routes raise their own limit above.
        .layer(axum::extract::DefaultBodyLimit::max(JSON_LIMIT))
        .with_state(app)
}

/// Everything that is one org's: its projects, its results and its
/// MCP endpoint. Which org is the handler's to read, with
/// [`crate::error::Org`]: the one in the path, or the default.
fn of_an_org(upload_limit: usize) -> Router<Shared> {
    let upload = axum::extract::DefaultBodyLimit::max(upload_limit);
    Router::new()
        .route("/projects", get(reads::projects))
        .route("/projects/{uid}", delete(deploy::remove_project))
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
        .route("/projects/{uid}/branches/{branch}/run", post(exec::run))
        .route("/projects/{uid}/branches/{branch}/sql-exec", post(exec::sql_exec))
        .route("/results/{id}", get(exec::result).delete(exec::cancel))
        .route("/cache/invalidate", post(exec::invalidate))
        .route(mcp::PATH, post(mcp::endpoint))
        .route("/projects/{uid}/datasets", get(datasets::list).post(datasets::create).layer(upload))
        .route("/projects/{uid}/datasets/{id}", get(datasets::get).delete(datasets::delete))
        .route("/projects/{uid}/datasets/{id}/upload", axum::routing::put(datasets::replace).layer(upload))
        .route("/projects/{uid}/datasets/{id}/rows", get(datasets::rows))
        .route("/projects/{uid}/datasets/{id}/lineage", get(datasets::lineage))
        .route("/projects/{uid}/datasets/{id}/recipe", get(datasets::recipe).post(datasets::change))
        .route("/projects/{uid}/datasets/{id}/preview", post(datasets::preview))
        .route("/projects/{uid}/datasets/{id}/impact", get(datasets::impact))
        .route("/projects/{uid}/datasets/{id}/confirm", post(datasets::confirm))
        .route("/projects/{uid}/datasets/{id}/edits", get(datasets::edit_log).post(datasets::edit))
        .route("/projects/{uid}/datasets/{id}/sources/{source}", get(datasets::source))
        .route("/datasets", delete(datasets::delete_owner))
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
    /// How many statements run at once, how many may wait, and the row
    /// limit a run is given when its request has none.
    pub runs: RunLimits,
    /// What data prep is allowed; `None` turns datasets off.
    pub datasets: Option<zsql_prep::Settings>,
}

/// What bounds the statements a server runs.
#[derive(Debug, Clone)]
pub struct RunLimits {
    pub concurrent: usize,
    pub queue_max: usize,
    /// Where the records of runs are kept: a Postgres every node
    /// shares, or, with `None`, this node's own disk.
    pub db: Option<zsql_exec::PostgresSettings>,
    /// `None` is no limit: a query with no limit of its own returns
    /// every row.
    pub default_sql_limit: Option<u64>,
    /// The object store results are kept in, as a URL and its options
    /// by name; with `None`, a result is only on this node's disk.
    pub object_store: Option<(String, Vec<(String, String)>)>,
    /// The share of the disk, in percent, this node's results may take.
    pub local_cache_max_percent: u8,
    /// How often results that no longer answer are cleared away.
    pub cache_sweep: Duration,
    /// Warehouse connections kept for reuse, across every datasource.
    pub pool_max: usize,
    /// How long a connection nothing uses is kept.
    pub pool_idle: Duration,
}

impl Default for RunLimits {
    fn default() -> Self {
        RunLimits {
            concurrent: 100,
            queue_max: 5_000,
            db: None,
            default_sql_limit: None,
            object_store: None,
            local_cache_max_percent: 60,
            cache_sweep: Duration::from_secs(300),
            pool_max: 200,
            pool_idle: Duration::from_secs(60),
        }
    }
}

impl Config {
    /// A server on its own machine: the compiled-in
    /// MCP wording.
    pub fn local(data_dir: PathBuf, bind: String) -> Config {
        Config {
            bind,
            public_url: None,
            admission: AdmissionCounts::default(),
            mcp: mcp::Settings::builtin(),
            runs: RunLimits::default(),
            datasets: Some(zsql_prep::Settings::defaults(&data_dir)),
            data_dir,
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
    // The default org's directory is always there, so what is deployed
    // with no org has somewhere to go, and so that renaming it to give
    // its projects to a named org leaves a default behind to deploy to.
    std::fs::create_dir_all(config.data_dir.join(crate::names::DEFAULT_ORG))?;
    let mut lock = lock_data_dir(&config.data_dir)?;
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    let advertised = config.public_url.clone();
    // Opening the records may mean a database, whose client waits on
    // a runtime of its own: not something to do on a thread of this one.
    let (dir, limits) = (config.data_dir.clone(), config.runs.clone());
    let runs = tokio::task::spawn_blocking(move || crate::open_runs(&dir, &limits)).await??;
    let prep = match config.datasets.clone() {
        Some(settings) => {
            let dir = config.data_dir.clone();
            Some(Arc::new(tokio::task::spawn_blocking(move || zsql_prep::Prep::open(&dir, settings)).await??))
        }
        None => {
            tracing::info!("datasets are off");
            None
        }
    };
    let app: Shared = Arc::new(App {
        dialects,
        data_dir: config.data_dir,
        models: RwLock::new(HashMap::new()),
        admission: Admission::new(&AdmissionCounts {
            run: 2 * (config.runs.concurrent + config.runs.queue_max),
            ..config.admission.clone()
        }),
        mcp: config.mcp,
        deploying: std::sync::Mutex::new(()),
        runs,
        default_sql_limit: config.runs.default_sql_limit,
        prep,
    });
    let (n, skipped) = reload(&app)?;
    tracing::info!("{n} deployment(s) loaded from {}", app.data_dir.display());
    if !skipped.is_empty() {
        // Said again, together, after the per-branch lines above: the
        // one line someone watching a restart will actually read.
        tracing::warn!("{} deployment(s) could not be loaded and are not being served: {}", skipped.len(), skipped.join(", "));
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
    tracing::info!("listening on http://{addr}");
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
    tracing::info!("stopped");
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
        tracing::info!("stopping");
    }
}

/// One server per data directory. The lock file holds the pid of the
/// server that has it, so the refusal can say who, and once it is bound
/// the address and the public URL, so `zsql server status` can say where.
fn lock_data_dir(dir: &std::path::Path) -> anyhow::Result<std::fs::File> {
    use std::io::{Read, Write};
    let path = dir.join(crate::names::LOCK_FILE);
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
