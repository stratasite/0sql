//! What each materialization was made from, kept beside it, so that
//! any version of a dataset can say exactly which recipe, which
//! snapshots and which engine made it, and each of its columns can be
//! followed back to the source columns it came from.

use crate::schema::Role;
use crate::source::{Kind, Parse};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The version of the step compiler. A change to what a step compiles
/// to changes this, so two materializations made by different
/// compilers can be told apart.
pub const COMPILER_VERSION: u32 = 1;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Lineage {
    pub dataset: String,
    /// The materialization this describes: `v<version>`.
    pub version: u32,
    /// The recipe version it ran.
    pub recipe: u32,
    /// How much of the edit log was folded in.
    pub edits: u64,
    pub sources: Vec<SourceRead>,
    /// Other datasets read, each at the version read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub datasets: Vec<DatasetRead>,
    pub roles: BTreeMap<String, Role>,
    pub made_by: MadeBy,
    /// The time the run was pinned to: what `today()` means in it.
    pub pinned_at: String,
    /// What the run skipped, held, orphaned or flagged.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    pub columns: Vec<Ancestry>,
}

/// One snapshot a run read.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SourceRead {
    pub source: String,
    pub kind: Kind,
    pub snapshot: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    pub sha256: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse: Option<Parse>,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct DatasetRead {
    pub id: String,
    pub version: u32,
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

/// A column of a source, or of another dataset.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq, PartialOrd, Ord)]
pub struct Origin {
    pub source: String,
    pub column: String,
}
