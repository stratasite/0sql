//! The server: deploy a project directory (as a tar.gz), then
//! plan specs against it. Deployed models live in memory and under a data
//! directory so a restart reloads them.
//!
//! The server answers whoever can reach it, the way a database on a
//! private network does. It is meant to sit behind the developer's own
//! service, which decides who may ask, sends the security context each
//! plan runs under, and paces its callers. It keeps of a
//! datasource what its entry was deployed with, every key, and a probe
//! or a run connects with that and whatever the request sends over it;
//! what a request sends is dropped with the answer. A deploy carries a
//! datasource's secrets, and they are its credential on that branch:
//! kept apart from the compiled model, never served, and set aside
//! whole by a request that sends a secret of its own (see
//! `credentials`). The server has no
//! credential of its own to check: a deploy, a validate, a test run
//! and a removal, of a branch or a whole project, are as open as a read, and whoever may do any of them
//! is decided by the service in front.
//!
//! A project belongs to an org, and an org is a directory of the data
//! directory and nothing more: `<org>/<project>/<branch>`. Every route
//! below, down to `/mcp`, is also served under `/orgs/{org}` for that
//! org's projects, results and MCP endpoint; without the prefix a
//! request is for the org `default`. Who may ask for which org is,
//! like everything else, decided in front.
//!
//! ```text
//! POST /projects/{uid}/branches/{branch}/deploy       body: tar.gz of the project; tests run, results in the answer
//! POST /projects/{uid}/branches/{branch}/validate      the same, keeping nothing
//! POST /projects/{uid}/branches/{branch}/sql           {"spec": {...}, "context": {...}} or {"expr": "..."}
//! POST /projects/{uid}/branches/{branch}/explain       the same, with the node graph and timings
//! POST /projects/{uid}/branches/{branch}/explore       what the query can add
//! POST /projects/{uid}/branches/{branch}/test          run the deployed tests/*.yml again
//! GET  /projects/{uid}/branches/{branch}               summary and warnings
//! GET  /projects/{uid}/branches/{branch}/fields        ?q=text&hidden=true
//! GET  /projects/{uid}/branches/{branch}/tables        ?q=text
//! GET  /projects/{uid}/branches/{branch}/values/{dim}  ?q=text
//! GET  /projects/{uid}/branches/{branch}/datasources   ?q=text
//! POST /projects/{uid}/branches/{branch}/datasources/{ds}/tables          {"connection": {...}}  ?q=&schema=&catalog=&limit=&offset=
//! POST /projects/{uid}/branches/{branch}/datasources/{ds}/tables/{table}  {"connection": {...}}  ?schema=&catalog=
//! DELETE /projects/{uid}/branches/{branch}
//! DELETE /projects/{uid}                              the whole project: every branch of it
//! GET  /projects                                       every deployed branch, ?q=text
//! POST /projects/{uid}/branches/{branch}/run           {"spec": {...}} or {"expr": "..."}: planned, run, and the rows
//! POST /projects/{uid}/branches/{branch}/sql-exec      {"datasource": "...", "sql": "..."}: one statement, run as written
//! GET  /results/{id}                                   a run: its record and a page of rows; DELETE cancels it
//! POST /cache/invalidate                               {"datasource": "...", "tables": [...]}: results that read them stop answering
//! /projects/{uid}/datasets/...                      datasets: data prep, beside the model (see datasets.rs)
//! POST /mcp                                            the same reads, as MCP tools (see mcp.rs)
//! ANY  /orgs/{org}/...                                 any route above, for one org
//! GET  /orgs                                           the orgs with something deployed
//! GET  /healthz
//! ```
//!
//! The crate is one module per concern:
//!
//! - `guard`: the admission cap and the deadline
//!   every request meets
//! - `admission`: how much of each kind of work runs at once
//! - `names`: the org, project and branch names that become directories
//! - `deploy`: an archive in, a model out
//! - `archive`: a deploy kept in the object store, without its secrets
//! - `credentials`: the secrets a datasource is deployed with, and what
//!   a later deploy keeps of them
//! - `reads`: sql, explain, explore and the catalog
//! - `probe`: a datasource's own tables, read from the warehouse with
//!   the connection the request sends
//! - `error`: the one error the API answers with
//! - `mcp`: the same reads as MCP tools
//! - `datasets`: the routes of data prep, which is `zsql-prep`'s
//! - `run`: configuration, the router and the server's life

pub mod admission;
mod archive;
mod credentials;
mod datasets;
mod deploy;
mod error;
mod exec;
mod guard;
mod mcp;
mod names;
mod probe;
mod reads;
mod run;

pub use admission::AdmissionCounts;
pub use mcp::Settings as McpSettings;
pub use run::{run, run_blocking, serve, Config, RunLimits, STOP_INTENT};
pub use zsql_prep::Settings as PrepSettings;

use crate::admission::Admission;
use crate::error::{not_found, ApiError};
use axum::http::StatusCode;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use zsql_core::Model;
use zsql_dialect::Dialects;

// A lock is never poisoned here on purpose: a panic in one request
// must not take the lock, and with it every later request, down with
// it. The data behind these locks is a connection, a cache or a map
// that a half-finished request leaves consistent.
pub(crate) fn lock<T>(m: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn read<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockReadGuard<'_, T> {
    l.read().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn write<T>(l: &std::sync::RwLock<T>) -> std::sync::RwLockWriteGuard<'_, T> {
    l.write().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) struct App {
    /// Shared by every deployment for the life of the process, so each
    /// planner can borrow it for as long as its universe.
    dialects: &'static Dialects,
    data_dir: PathBuf,
    /// Deployments by `org/project/branch`.
    models: RwLock<HashMap<String, Arc<Model>>>,
    /// How much of each kind of work may be in the server at once.
    admission: Admission,
    /// What MCP clients are told, and which tools they may call.
    mcp: mcp::Settings,
    /// One deploy writes at a time: two for the same branch would race
    /// each other's renames while swapping in.
    deploying: std::sync::Mutex<()>,
    /// The statements this server runs and their results.
    runs: zsql_exec::Runs,
    /// The row limit a run is given when its request has none. `None`
    /// leaves it without one: a bound is the caller's to set.
    default_sql_limit: Option<u64>,
    /// Data prep, unless it is turned off.
    prep: Option<Arc<zsql_prep::Prep>>,
}

/// A deployment's key in the models map. None of the three names can
/// hold a `/`, so the keys of one org are the ones that start with it
/// and a `/`.
pub(crate) fn key(org: &str, uid: &str, branch: &str) -> String {
    format!("{org}/{uid}/{branch}")
}

impl App {
    /// Where a deployed branch lives on disk.
    pub(crate) fn dir(&self, org: &str, uid: &str, branch: &str) -> PathBuf {
        self.data_dir.join(org).join(uid).join(branch)
    }

    /// The deployed model of a branch. The names are checked first, so
    /// a name that could not have been deployed is told so rather than
    /// reported as merely missing.
    pub(crate) fn model(&self, org: &str, uid: &str, branch: &str) -> Result<Arc<Model>, ApiError> {
        names::check_org(org)?;
        names::check_uid(uid)?;
        names::check_branch(branch)?;
        read(&self.models).get(&key(org, uid, branch)).cloned().ok_or_else(|| not_found(org, uid, branch))
    }

    /// Every deployment of one org, by project and branch. Nothing of
    /// another org is in it: a listing, a catalog and a default branch
    /// are always one org's.
    pub(crate) fn of_org(&self, org: &str) -> Vec<Arc<Model>> {
        let prefix = format!("{org}/");
        let mut out: Vec<Arc<Model>> = read(&self.models).iter().filter(|(k, _)| k.starts_with(&prefix)).map(|(_, m)| m.clone()).collect();
        out.sort_by(|a, b| (&a.uid, &a.branch).cmp(&(&b.uid, &b.branch)));
        out
    }

    /// The orgs with something deployed, and the default one.
    pub(crate) fn orgs(&self) -> Vec<String> {
        let mut out: Vec<String> = read(&self.models).keys().filter_map(|k| k.split('/').next()).map(str::to_string).collect();
        out.push(names::DEFAULT_ORG.to_string());
        out.sort();
        out.dedup();
        out
    }
}

pub(crate) type Shared = Arc<App>;

/// Where results and their records live under a data directory. A dot
/// in front, because every other directory there is an org, and an
/// org may well be called `cache`; an org never starts with a dot.
pub(crate) const CACHE_DIR: &str = ".cache";

/// The runs of a server whose data is under `data_dir`, with their
/// records in the database it was given or on this node's disk.
pub(crate) fn open_runs(data_dir: &std::path::Path, limits: &RunLimits) -> anyhow::Result<zsql_exec::Runs> {
    let cache = data_dir.join(CACHE_DIR);
    let registry: Arc<dyn zsql_exec::Registry> = match &limits.db {
        Some(db) => {
            let registry = zsql_exec::Postgres::open(db)?;
            tracing::info!("the records of runs are kept in the {} table of ZSQL_DB", zsql_exec::TABLE);
            Arc::new(registry)
        }
        None => Arc::new(zsql_exec::Local::open(&cache.join("records"))?),
    };
    let objects = match &limits.object_store {
        Some((url, options)) => {
            let objects = zsql_exec::Objects::open(url, options)?;
            tracing::info!("results are kept in the object store {url}, under {}/", zsql_exec::PREFIX);
            Some(Arc::new(objects))
        }
        None => None,
    };
    let limits = zsql_exec::Limits {
        concurrent: limits.concurrent,
        queue_max: limits.queue_max,
        local_max_percent: limits.local_cache_max_percent,
        sweep: limits.cache_sweep,
        pool_max: limits.pool_max,
        pool_idle: limits.pool_idle,
        ..zsql_exec::Limits::of(&hostname())
    };
    Ok(zsql_exec::Runs::open(&cache.join("results"), registry, objects, limits)?)
}

/// This machine's name, as it calls itself.
fn hostname() -> String {
    let mut buffer = [0u8; 256];
    // SAFETY: the buffer is ours and its length is what is passed; the
    // name is read back only up to the first NUL.
    let named = unsafe { libc::gethostname(buffer.as_mut_ptr().cast(), buffer.len()) } == 0;
    let name: String = if named { buffer.iter().take_while(|b| **b != 0).map(|b| *b as char).collect() } else { String::new() };
    if name.is_empty() {
        "localhost".to_string()
    } else {
        name
    }
}

/// Runs work that would hold a runtime thread for seconds on a blocking
/// thread instead, so a deploy never stalls the planning the rest of
/// the server is doing.
///
/// The work holds the request's admission seat until it ends. A request
/// that passes its deadline is answered, but its work goes on, and the
/// seat must stay taken while it does, or work past its deadline would
/// pile up past the cap.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    let seat = guard::seat();
    match tokio::task::spawn_blocking(move || {
        let held = seat;
        let result = work();
        drop(held);
        result
    })
    .await
    {
        Ok(result) => result,
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("this request did not finish: {e}"))),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod parity_tests;
