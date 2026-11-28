use serde::{Deserialize, Serialize};
use serde_json::Value;

pub const MANIFEST_FORMAT: u32 = 2;
/// What this build writes.
pub const ROWS_FORMAT: &str = "arrow";
/// What builds before 0.7 wrote, still read.
pub const ROWS_FORMAT_TEXT: &str = "jsonl.gz";

/// What a stored result is: the question, where it ran, what came back.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Manifest {
    pub format: u32,
    /// The cache key, which is also the directory's name.
    pub key: String,
    /// The human handle: `r-<date>-<eight of the key>`.
    pub id: String,
    pub created_at: String,
    pub project_uid: String,
    pub branch: String,
    pub datasource_uid: String,
    pub adapter: String,
    /// How the rows travelled: `native`, `rest` or `adbc`. One datasource
    /// can answer through two of them, typed through one and as text
    /// through the other, and a result should say which it was.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub transport: Option<String>,
    /// Which database answered, never a secret: every name, location and
    /// timestamp that decides what a query sees, as a JSON object. Two
    /// connections that differ here are two databases, and a result from
    /// one is never served for the other.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub connection_identity: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub expr: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub context_digest: Option<String>,
    /// As sent, limit included.
    pub sql: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub user_limit: Option<u64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub cap: Option<u64>,
    pub rows: u64,
    /// False when the cap cut the result: there were more rows.
    pub complete: bool,
    pub columns: Vec<Column>,
    pub rows_format: String,
    pub timings: Timings,
    /// How long it answers, in seconds; `-1` never expires.
    pub cache_ttl_secs: i64,
    pub lineage: Lineage,
    #[serde(default)]
    pub warnings: Vec<String>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Column {
    pub name: String,
    /// The model's data type when the column is a field of the model.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    /// `model` or `unknown`: where `data_type` came from.
    pub source: String,
    /// The type the rows are stored in, as Arrow names it: what the
    /// transport said the warehouse returned, or `Utf8` when it only
    /// had text.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub arrow_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_uid: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Timings {
    pub plan_us: u128,
    pub connect_ms: u128,
    pub run_ms: u128,
}

/// What the question was made of, kept whether or not anything reads
/// it yet, since it cannot be backfilled. Versioned on its own.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct Lineage {
    pub lineage_format: u32,
    /// The items of the expression line, or the spec's projections.
    pub items: Vec<String>,
    /// Fields the result's columns resolved to, by uid.
    pub fields: Vec<String>,
    /// Tables the statement reads, by uid, as the snapshot names them.
    pub tables: Vec<String>,
}
