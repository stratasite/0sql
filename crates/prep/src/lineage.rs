//! What each output was made from, kept beside it, so that any version
//! of a dataset can say exactly which run, which graph, which snapshots
//! and which engine made it, and each of its columns can be followed
//! back to the source columns it came from.

use crate::schema::Role;
use crate::source::{Kind, Parse, SemanticRead};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The version of the compiler. A change to what a node compiles to
/// changes this, so two outputs made by different compilers can be told
/// apart.
pub const COMPILER_VERSION: u32 = 2;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Lineage {
    pub dataset: String,
    /// The output this describes: `v<version>`.
    pub version: u32,
    /// The run that made it.
    pub run: String,
    /// The hash of the graph it ran, kept beside it as `graph.json`.
    pub graph: String,
    pub sources: Vec<SourceRead>,
    /// Other datasets read, each at the version read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub datasets: Vec<DatasetRead>,
    pub roles: BTreeMap<String, Role>,
    pub made_by: MadeBy,
    /// The time the run was pinned to: what `today()` means in it.
    pub pinned_at: String,
    /// What the run let through: a check that warns, edits orphaned or
    /// flagged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub columns: Vec<Ancestry>,
}

/// One snapshot a run read.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SourceRead {
    /// The node that read it.
    pub node: String,
    pub source: String,
    pub kind: Kind,
    pub snapshot: u32,
    /// The version of how the source was read.
    pub read: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse: Option<Parse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticRead>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DatasetRead {
    pub id: String,
    pub version: u32,
    /// The node that read it, for a run.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct MadeBy {
    pub zsql: String,
    pub compiler: u32,
    pub duckdb: String,
}

/// Where a column of the result came from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Ancestry {
    pub column: String,
    pub from: Vec<Origin>,
}

/// A column of a source (by its id), or of another dataset
/// (`dataset:<id>`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin {
    pub source: String,
    pub column: String,
}
