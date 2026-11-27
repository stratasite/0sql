//! What a run did, kept with what it made: a record per node, which the
//! application shows on the graph and an AI reads to explain a run.
//!
//! Every number here is counted by the run, and every summary is written
//! from those numbers by this crate, never by a model: it costs nothing,
//! says the same every time, and is true.

use crate::edits::Outcome;
use crate::graph::{CheckKind, OnFail, Problem};
use crate::schema::{ColumnType, Role};
use serde::{Deserialize, Serialize};

/// Where a run is.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum RunState {
    /// Pulling or running now.
    Running,
    /// A stop cut it short; the next start says it was interrupted.
    Suspended,
    /// It made an output version.
    Succeeded,
    /// It made nothing: refused, or something failed.
    Failed,
    /// An output check that holds failed: nothing was kept, and the last
    /// output stays.
    Held,
    /// A start found it in flight, and did not run it again.
    Interrupted,
}

impl RunState {
    pub fn is_final(self) -> bool {
        !matches!(self, RunState::Running | RunState::Suspended)
    }
}

/// A run, as recorded.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct RunRecord {
    /// The application's id for it: sent again, it answers this record.
    pub id: String,
    pub dataset: String,
    pub state: RunState,
    pub started: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub ended: Option<String>,
    /// The hash of what was asked, so a run id sent with another request
    /// is told apart from a retry.
    pub request: String,
    /// The hash of the security context it ran under.
    pub context: String,
    /// The sources and datasets it reads.
    pub reads: Vec<ReadRef>,
    /// What each input read, or pulled, and why.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub inputs: Vec<InputPlan>,
    /// The output version it made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub version: Option<u32>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub nodes: Vec<NodeRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub problems: Vec<Problem>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<crate::recovery::Interrupted>,
}

/// A source or a dataset, by kind and id.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct ReadRef {
    pub kind: String,
    pub id: String,
}

/// Whether an input is read as it is, or pulled first.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Will {
    Read,
    Pull,
}

/// What one input reads, as a run decided it before reading.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct InputPlan {
    pub node: String,
    pub kind: String,
    pub id: String,
    pub name: String,
    pub will: Will,
    /// The snapshot of a source, or version of a dataset, it reads; for
    /// a pull, the one the pull makes, once made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub number: Option<u32>,
    /// When what it reads was taken or made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub taken: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub age_secs: Option<u64>,
    pub why: String,
}

/// What one node did.
#[derive(Clone, Debug, Default, Serialize, Deserialize, PartialEq)]
pub struct NodeRecord {
    pub id: String,
    pub kind: String,
    /// Rows of each input, in the order the node reads them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub rows_in: Vec<u64>,
    pub rows: u64,
    pub columns: usize,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub added: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub removed: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub retyped: Vec<Retyped>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<StepRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<JoinRecord>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub append: Option<AppendRecord>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub edits: Vec<Outcome>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub checks: Vec<CheckResult>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub warnings: Vec<String>,
    pub summary: String,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub profile: Vec<ColumnProfile>,
    pub ms: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Retyped {
    pub column: String,
    pub from: ColumnType,
    pub to: ColumnType,
}

/// One step of a `steps` node.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct StepRecord {
    pub step: usize,
    pub op: String,
    pub rows: u64,
    /// Values it changed in place, where it changes values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub changed: Option<u64>,
    pub said: String,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct JoinRecord {
    /// Rows of the left with a match on the right.
    pub matched: u64,
    pub unmatched_left: u64,
    /// Rows of the right that matched no row of the left.
    pub unmatched_right: u64,
    /// Whether the counts are of a sample, where rows unmatched may
    /// match in the whole.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub sample: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct AppendRecord {
    /// Columns only some inputs have, empty on the others.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub only_in_some: Vec<String>,
}

/// An output check, as a run found it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct CheckResult {
    pub check: CheckKind,
    pub columns: Vec<String>,
    pub on_fail: OnFail,
    pub passed: bool,
    /// Rows that fail it: those with an empty value, or the values that
    /// repeat.
    #[serde(default, skip_serializing_if = "is_zero")]
    pub failing: u64,
}

fn is_zero(n: &u64) -> bool {
    *n == 0
}

/// What a node made, per column: what an AI reasons from.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ColumnProfile {
    pub column: String,
    #[serde(rename = "type")]
    pub ty: ColumnType,
    pub role: Role,
    pub nulls: u64,
    pub distinct: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub min: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max: Option<String>,
}

/// A count as a person reads it: `1,204`.
pub fn n(v: u64) -> String {
    let s = v.to_string();
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if i > 0 && (s.len() - i).is_multiple_of(3) {
            out.push(',');
        }
        out.push(c);
    }
    out
}

/// `1 row`, `3 rows`.
pub fn rows(v: u64) -> String {
    format!("{} row{}", n(v), if v == 1 { "" } else { "s" })
}

/// An age as a person reads it: `3 hours`.
pub fn age(secs: u64) -> String {
    let (v, unit) = match secs {
        0..=119 => (secs, "second"),
        120..=7199 => (secs / 60, "minute"),
        7200..=172_799 => (secs / 3600, "hour"),
        _ => (secs / 86_400, "day"),
    };
    format!("{v} {unit}{}", if v == 1 { "" } else { "s" })
}

/// A list as a person reads it, the first few and how many more.
pub fn listed(names: &[String]) -> String {
    match names.len() {
        0 => String::new(),
        1..=4 => names.join(", "),
        k => format!("{} and {} more", names[..3].join(", "), k - 3),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_and_ages_are_said_as_a_person_reads_them() {
        assert_eq!((n(0), n(999), n(1204), n(1_000_000)), ("0".into(), "999".into(), "1,204".into(), "1,000,000".into()));
        assert_eq!((rows(1), rows(2)), ("1 row".to_string(), "2 rows".to_string()));
        assert_eq!(
            (age(5), age(600), age(3 * 3600), age(4 * 86_400)),
            ("5 seconds".into(), "10 minutes".into(), "3 hours".into(), "4 days".into())
        );
        let many: Vec<String> = ["a", "b", "c", "d", "e"].iter().map(|s| s.to_string()).collect();
        assert_eq!(listed(&many), "a, b, c and 2 more");
    }
}
