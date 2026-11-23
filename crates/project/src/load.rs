use std::path::{Path, PathBuf};

use zsql_dialect::Dialects;
use zsql_semantic::Snapshot;

use crate::builder::Builder;
use crate::error::Result;
use crate::values;

/// A loaded project: the snapshot, with paths formed, and what the loader
/// noticed but did not refuse.
#[derive(Debug)]
pub struct Loaded {
    pub snapshot: Snapshot,
    pub warnings: Vec<String>,
    pub tests: Vec<ProjectTest>,
    /// Sampled dimension values (`values.jsonl.gz`), when the project has them.
    pub values: Option<values::ValueCatalog>,
    /// Everything the load depends on, each once, in the order it was
    /// met: every file read, imports included; the directories walked
    /// for model and test files, whose modification time moves when a
    /// file is added or removed; and the optional files (project.yml,
    /// security.yml, the values file) whether or not they exist, since
    /// their appearing is a change too. What a watcher stats to know
    /// the model moved, and what a cache would key on.
    pub sources: Vec<PathBuf>,
    /// What each step of the load cost, in microseconds, in order.
    pub phases: Vec<(&'static str, u128)>,
}

/// One `tests/*.yml` entry: fields to project by name and what the SQL
/// must equal or match.
#[derive(Debug, Clone, serde::Serialize, serde::Deserialize)]
pub struct ProjectTest {
    /// Where the test came from; stored and shown by its file name only.
    #[serde(serialize_with = "file_name_only")]
    pub file: PathBuf,
    pub name: String,
    pub projections: Vec<String>,
    pub assert_sql: Option<String>,
    pub assert_regex: Option<String>,
}

fn file_name_only<S: serde::Serializer>(path: &Path, s: S) -> std::result::Result<S::Ok, S::Error> {
    s.serialize_str(&path.file_name().map(|f| f.to_string_lossy().to_string()).unwrap_or_default())
}

/// Loads the project at `dir`. `dialects` supplies each datasource's
/// reserved keywords, aggregate functions and identifier quote, which the
/// expression parser needs to find the columns an expression reads.
pub fn load(dir: &Path, dialects: &Dialects) -> Result<Loaded> {
    load_with(dir, dialects, false)
}

/// [`load`] for a directory the loader must not read outside of: the
/// server's unpacked archive. An `imports` entry that resolves beyond
/// the directory, or a link out of it, is an error rather than a read
/// of whatever is there on the host. Locally the CLI reads the
/// developer's own machine and keeps the freedom.
pub fn load_confined(dir: &Path, dialects: &Dialects) -> Result<Loaded> {
    load_with(dir, dialects, true)
}

fn load_with(dir: &Path, dialects: &Dialects, confined: bool) -> Result<Loaded> {
    let mut b = Builder::new(dir, dialects);
    b.confined = confined;
    macro_rules! step {
        ($name:literal, $e:expr) => {{
            let t = std::time::Instant::now();
            let out = $e;
            b.phases.push(($name, t.elapsed().as_micros()));
            out
        }};
    }
    step!("project", b.project()?);
    step!("datasources", b.datasources()?);
    step!("tables", b.tables()?);
    step!("relationships", b.relationships()?);
    step!("form", b.form()?);
    step!("post_universe", b.post_universe()?);
    step!("security", b.security()?);
    step!("tests", b.tests()?);
    Ok(b.finish())
}
