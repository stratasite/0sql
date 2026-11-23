//! The server: deploy a project directory (as a tar.gz), then
//! plan specs against it. Deployed models live in memory and under a data
//! directory so a restart reloads them.
//!
//! The server answers whoever can reach it, the way a database on a
//! private network does. It is meant to sit behind the developer's own
//! service, which decides who may ask, sends the security context each
//! plan runs under, and paces its callers. It keeps of a
//! datasource what its entry was deployed with, every key, and a probe
//! connects with that and whatever the request sends over it; what a
//! request sends is dropped with the answer. `zsql deploy` strips
//! secrets from the entry, so a secret is kept only when a deployer
//! put one there on purpose, and it is never served. The server's own
//! credential is the optional deploy token: with one set, a deploy, a validate, a test
//! run and a removal each need `Authorization: Bearer <token>`, so a
//! server that is read from more widely than it is written to can be.
//!
//! ```text
//! POST /projects/{uid}/branches/{branch}/deploy        body: tar.gz of the project; tests run, results in the answer
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
//! GET  /projects                                       every deployed branch, ?q=text
//! POST /mcp                                            the same reads, as MCP tools (see mcp.rs)
//! GET  /healthz
//! ```
//!
//! The crate is one module per concern:
//!
//! - `guard`: the deploy token, the admission cap and the deadline
//!   every request meets
//! - `admission`: how much of each kind of work runs at once
//! - `names`: the project and branch names that become directories
//! - `deploy`: an archive in, a model out
//! - `reads`: sql, explain, explore and the catalog
//! - `probe`: a datasource's own tables, read from the warehouse with
//!   the connection the request sends
//! - `error`: the one error the API answers with
//! - `mcp`: the same reads as MCP tools
//! - `run`: configuration, the router and the server's life

pub mod admission;
mod deploy;
mod error;
mod guard;
mod mcp;
mod names;
mod probe;
mod reads;
mod run;

pub use admission::AdmissionCounts;
pub use mcp::Settings as McpSettings;
pub use run::{run, run_blocking, serve, Config};

use crate::admission::Admission;
use crate::error::{not_found, ApiError};
use axum::http::StatusCode;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use zsql_dialect::Dialects;
use zsql_service::Model;

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
    /// Deployments by `project/branch`.
    models: RwLock<HashMap<String, Arc<Model>>>,
    /// How much of each kind of work may be in the server at once.
    admission: Admission,
    /// What MCP clients are told, and which tools they may call.
    mcp: mcp::Settings,
    /// What a deploy, a validate, a test run and a removal must present,
    /// when the server was given one. `None` leaves them as open as the
    /// reads.
    deploy_token: Option<String>,
    /// One deploy writes at a time: two for the same branch would race
    /// each other's renames while swapping in.
    deploying: std::sync::Mutex<()>,
}

/// A deployment's key in the models map.
pub(crate) fn key(uid: &str, branch: &str) -> String {
    format!("{uid}/{branch}")
}

impl App {
    /// Where a deployed branch lives on disk.
    pub(crate) fn dir(&self, uid: &str, branch: &str) -> PathBuf {
        self.data_dir.join(uid).join(branch)
    }

    /// The deployed model of a branch. The names are checked first, so
    /// a name that could not have been deployed is told so rather than
    /// reported as merely missing.
    pub(crate) fn model(&self, uid: &str, branch: &str) -> Result<Arc<Model>, ApiError> {
        names::check_uid(uid)?;
        names::check_branch(branch)?;
        read(&self.models).get(&key(uid, branch)).cloned().ok_or_else(|| not_found(uid, branch))
    }
}

pub(crate) type Shared = Arc<App>;

/// Runs work that would hold a runtime thread for seconds on a blocking
/// thread instead, so a deploy never stalls the planning the rest of
/// the server is doing.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("this request did not finish: {e}"))),
    }
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod parity_tests;
