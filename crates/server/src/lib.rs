//! The server: deploy a project directory (as a tar.gz), then
//! plan specs against it. Deployed models live in memory and under a data
//! directory so a restart reloads them.
//!
//! With `ZSQL_ACCOUNTS=on` the server is multi-tenant: every request
//! carries a key (`Authorization: Bearer ...`) that names a user or a
//! query key in an account, deployments are scoped to the account, and
//! the accounts endpoints manage members, keys and project access. Off
//! (the default, the self-hosted mode), there is one tenant and no auth.
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
//! GET  /projects/{uid}/branches/{branch}/tables
//! GET  /projects/{uid}/branches/{branch}/values/{dim}
//! DELETE /projects/{uid}/branches/{branch}
//! GET  /projects                                       every deployed branch
//! POST /mcp                                            the same reads, as MCP tools (see mcp.rs)
//! GET  /healthz
//! ```
//!
//! The crate is one module per concern:
//!
//! - `auth`: who is calling, and the guard every request meets
//! - `tenancy`: the tenant a request lives in and the gate on each branch
//! - `deploy`: an archive in, a model out
//! - `reads`: sql, explain, explore and the catalog
//! - `accounts_api`: sign-up, sessions, members, keys and grants
//! - `error`: the one error the API answers with
//! - `mcp`: the same reads as MCP tools
//! - `ui`: the embedded web app
//! - `run`: configuration, the router and the server's life

/// A line to the log. Never a panic: a server whose stderr has gone
/// away (a pipe its reader closed) keeps serving, which `eprintln!`
/// would not allow.
macro_rules! say {
    ($($t:tt)*) => {{
        use std::io::Write;
        let _ = writeln!(std::io::stderr(), $($t)*);
    }};
}

mod accounts_api;
mod auth;
mod deploy;
mod error;
pub mod limits;
pub mod mail;
mod mcp;
mod reads;
mod run;
mod tenancy;
mod ui;

pub use mcp::Settings as McpSettings;
pub use run::{run, run_blocking, serve, Config, MailConfig};

use crate::error::ApiError;
use crate::limits::{Admission, Limiter};
use axum::http::StatusCode;
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use zsql_accounts::Store;
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
    /// Deployments by `[account/]project/branch`.
    models: RwLock<HashMap<String, Arc<Model>>>,
    /// Accounts, users and keys; `None` in the single-tenant mode.
    accounts: Option<Store>,
    signup_open: bool,
    /// Session cookies marked Secure, for a server behind https.
    secure_cookies: bool,
    /// What each caller may ask for, and how fast.
    limits: Limiter,
    /// How much of each kind of work may be in the server at once.
    admission: Admission,
    /// How many branches an account may keep deployed.
    caps: Caps,
    /// How the verification and reset links get out.
    mailer: mail::Mailer,
    /// What MCP clients are told, and which tools they may call.
    mcp: mcp::Settings,
    /// One deploy writes at a time: two for the same branch would race
    /// each other's renames while swapping in.
    deploying: std::sync::Mutex<()>,
}

/// How many deployed branches are allowed.
///
/// This is a memory limit wearing a product's clothes: a deployed model
/// is held for the life of the process and nothing evicts it, so the
/// number of them is the number the machine can hold. Both counts are
/// needed: per project is the one a person feels and reasons about, and
/// per account is the one that actually bounds the process, since an
/// account may make as many projects as it likes.
///
/// Only with accounts on. A single-tenant server is one operator on
/// their own machine, who does not need protecting from themselves.
pub struct Caps {
    pub per_project: usize,
    pub per_account: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps { per_project: 5, per_account: 20 }
    }
}

pub(crate) type Shared = Arc<App>;

/// Runs work that would hold a runtime thread for seconds on a blocking
/// thread instead, so a deploy or a password hash never stalls the
/// planning the rest of the server is doing.
pub(crate) async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("this request did not finish: {e}"))),
    }
}

#[cfg(test)]
mod isolation_tests;

#[cfg(test)]
mod parity_tests;
