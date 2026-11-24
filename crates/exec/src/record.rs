//! The record of a run: one per statement a server was asked to run,
//! written before the statement starts and kept after it ends. It is
//! what a caller polls, what a cache lookup finds, and what an operator
//! reads to see what the server has been doing.
//!
//! The same fields are a row of the results table when the records are
//! kept in Postgres, so a name here is a column there. A credential is
//! never among them.

use serde::{Deserialize, Serialize};

/// What kind of run it is.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    /// Planned here from a spec or an expression: cached, and shared
    /// with an identical run in flight.
    Semantic,
    /// The caller's own statement: never cached, never shared.
    Sql,
}

impl Kind {
    pub fn as_str(self) -> &'static str {
        match self {
            Kind::Semantic => "semantic",
            Kind::Sql => "sql",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum Status {
    Queued,
    Running,
    Succeeded,
    Failed,
    Cancelled,
}

impl Status {
    pub fn as_str(self) -> &'static str {
        match self {
            Status::Queued => "queued",
            Status::Running => "running",
            Status::Succeeded => "succeeded",
            Status::Failed => "failed",
            Status::Cancelled => "cancelled",
        }
    }

    /// Whether the run is over, one way or another.
    pub fn is_final(self) -> bool {
        matches!(self, Status::Succeeded | Status::Failed | Status::Cancelled)
    }
}

/// One run. Times are RFC 3339, UTC.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Record {
    pub id: String,
    #[serde(rename = "type")]
    pub kind: Kind,
    pub status: Status,
    pub error: Option<String>,
    /// Rows returned; rows affected, for a statement that wrote.
    pub row_count: Option<u64>,
    /// What an identical question finds this by. `None` for a run that
    /// is not cached: a `sql` run, or a datasource whose `cache_ttl` is 0.
    pub cache_key: Option<String>,
    /// Set by invalidation: the result no longer answers.
    pub dirty: bool,
    /// The physical tables the statement reads, as the model names
    /// them. Empty for a `sql` run.
    pub tables: Vec<String>,
    pub org_uid: String,
    pub project_uid: String,
    /// Where it came from, for whoever reads the record. Not part of
    /// the cache key.
    pub branch: String,
    pub datasource_uid: String,
    pub sql: String,
    pub spec: Option<serde_json::Value>,
    pub expr: Option<String>,
    /// The row limit in the statement, when it has one.
    pub row_limit: Option<u64>,
    /// The node running it.
    pub hostname: String,
    /// Where the result is in the object store. `None` when it is only
    /// on the node's own disk.
    pub object_store_path: Option<String>,
    pub created_at: String,
    pub queued_at: String,
    pub started_at: Option<String>,
    pub finished_at: Option<String>,
    /// Refreshed while the run is alive, so a run nobody is running any
    /// more can be told from one that is merely long.
    pub updated_at: String,
    pub cancelled_at: Option<String>,
    /// When the result stops answering: `finished_at` plus the
    /// datasource's `cache_ttl`.
    pub expires_at: Option<String>,
}

/// The org every record belongs to until there are orgs.
pub const DEFAULT_ORG: &str = "default";

/// What an identical question in the same org and project is found by:
/// the planned SQL, which already carries the row policies. Not the
/// user, the credential, the datasource or the branch.
pub fn run_cache_key(org_uid: &str, project_uid: &str, sql: &str) -> String {
    crate::engine::sha256(&format!("{org_uid}\n{project_uid}\n{sql}"))
}
