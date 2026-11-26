//! Where datasets are kept, and the only module that knows.
//!
//! ```text
//! <data-dir>/<org>/<project>/.datasets/<id>/
//!   dataset.json         the manifest: the registry entry, written last
//!   recipe/r<m>.json     every recipe version
//!   sources/<s>/<n>.*    snapshots: rows.arrow and its manifest
//!   v<k>/                materializations: rows.arrow, manifest, lineage
//! ```
//!
//! Nothing is changed in place. A file is written beside where it goes
//! and renamed there; a new dataset is built in a dot directory and
//! renamed into place whole; `dataset.json` is written after what it
//! points at, so a crash leaves the dataset as it was before the job.
//! The directory starts with a dot, which no branch name does, so the
//! server's loader passes over it.

use crate::error::{Error, Result};
use crate::lineage::Lineage;
use crate::schema::Column;
use crate::source::{Kind, Parse, Query, SemanticRead};
use arrow_array::RecordBatch;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::path::{Path, PathBuf};
use zsql_datasource::Batches;

/// The directory, under a project's, that holds its datasets.
pub const DIR: &str = ".datasets";

pub(crate) const MANIFEST: &str = "dataset.json";
pub(crate) const ROWS: &str = "rows.arrow";

/// A dataset's manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    /// The owner reference's hash; the reference itself is never kept.
    pub owner: String,
    pub created: String,
    pub updated: String,
    /// The recipe's latest version.
    pub recipe: u32,
    /// The materialization the dataset answers with, once it has one.
    pub live: Option<u32>,
    /// The last materialization made.
    pub made: u32,
    pub sources: Vec<SourceState>,
    /// The live materialization's columns and rows.
    pub columns: Vec<Column>,
    pub rows: u64,
    /// What the last job said: a column gone, a type widened.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// Set when the latest snapshots lack columns the recipe reads: the
    /// recipe has not run on them, and what is live is older.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub held: Option<Held>,
    /// How much of the edit log is in force.
    #[serde(default)]
    pub edits: u64,
}

/// Why a dataset's latest snapshots have not been made into a version.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Held {
    pub since: String,
    pub unresolved: Vec<crate::compile::Unresolved>,
}

/// A source of a dataset, as it stands.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct SourceState {
    pub id: String,
    pub kind: Kind,
    /// The latest snapshot.
    pub snapshot: u32,
    /// Its columns: the types every later snapshot meets.
    pub columns: Vec<Column>,
    /// What a semantic source runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Query>,
}

/// A snapshot's manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Snapshot {
    pub source: String,
    pub number: u32,
    pub kind: Kind,
    pub taken: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub file_name: Option<String>,
    /// The hash of what arrived: the file as uploaded.
    pub sha256: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse: Option<Parse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticRead>,
    pub columns: Vec<Column>,
    pub rows: u64,
    /// How its columns met the source's: new, gone, widened, converted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
}

/// A materialization's manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Materialization {
    pub version: u32,
    pub made: String,
    pub columns: Vec<Column>,
    pub rows: u64,
    pub bytes: u64,
}

pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: &Path) -> Store {
        Store { data_dir: data_dir.to_path_buf() }
    }

    /// Where blends' answers are kept.
    pub fn blends(&self) -> PathBuf {
        self.data_dir.join(".cache").join("blends")
    }

    /// Where one project's datasets are.
    pub fn root(&self, org: &str, project: &str) -> PathBuf {
        self.data_dir.join(org).join(project).join(DIR)
    }

    /// The projects of an org that have a dataset directory.
    pub fn projects(&self, org: &str) -> Vec<String> {
        let Ok(entries) = std::fs::read_dir(self.data_dir.join(org)) else { return Vec::new() };
        let mut out: Vec<String> = entries
            .flatten()
            .filter(|e| e.path().join(DIR).is_dir())
            .map(|e| e.file_name().to_string_lossy().to_string())
            .filter(|n| !n.starts_with('.'))
            .collect();
        out.sort();
        out
    }

    /// Every dataset of a project, whoever owns it, by id.
    pub fn all(&self, org: &str, project: &str) -> Vec<(PathBuf, Dataset)> {
        let Ok(entries) = std::fs::read_dir(self.root(org, project)) else { return Vec::new() };
        let mut out: Vec<(PathBuf, Dataset)> = entries
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| {
                let dir = e.path();
                let d: Dataset = read_json(&dir.join(MANIFEST)).ok()?;
                Some((dir, d))
            })
            .collect();
        out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        out
    }
}

pub(crate) fn snapshot_paths(dir: &Path, source: &str, n: u32) -> (PathBuf, PathBuf) {
    let base = dir.join("sources").join(source);
    (base.join(format!("{n}.arrow")), base.join(format!("{n}.json")))
}

pub(crate) fn recipe_path(dir: &Path, m: u32) -> PathBuf {
    dir.join("recipe").join(format!("r{m}.json"))
}

pub(crate) fn version_dir(dir: &Path, k: u32) -> PathBuf {
    dir.join(format!("v{k}"))
}

pub(crate) fn read_json<T: DeserializeOwned>(path: &Path) -> Result<T> {
    let text = std::fs::read_to_string(path).map_err(|e| Error::io(path, e))?;
    Ok(serde_json::from_str(&text)?)
}

/// Writes beside the target and renames it there, so a reader sees the
/// old file or the new one.
pub(crate) fn write_json<T: Serialize>(path: &Path, value: &T) -> Result<()> {
    let text = serde_json::to_vec_pretty(value)?;
    write_file(path, &text)
}

pub(crate) fn write_file(path: &Path, bytes: &[u8]) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    std::fs::write(&tmp, bytes).map_err(|e| Error::io(&tmp, e))?;
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))
}

/// Rows as an Arrow IPC file, LZ4-compressed as results are; the
/// number of bytes written.
pub(crate) fn write_rows(path: &Path, rows: &Batches) -> Result<u64> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| Error::io(parent, e))?;
    }
    let tmp = path.with_extension(format!("tmp-{}", std::process::id()));
    {
        let file = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        let options = arrow_ipc::writer::IpcWriteOptions::default().try_with_compression(Some(arrow_ipc::CompressionType::LZ4_FRAME))?;
        let mut writer = arrow_ipc::writer::FileWriter::try_new_with_options(std::io::BufWriter::new(file), &rows.schema, options)?;
        for b in &rows.batches {
            writer.write(b)?;
        }
        writer.finish()?;
    }
    let bytes = std::fs::metadata(&tmp).map_err(|e| Error::io(&tmp, e))?.len();
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
    Ok(bytes)
}

pub(crate) fn read_rows(path: &Path) -> Result<Batches> {
    let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None)?;
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.collect::<std::result::Result<_, _>>()?;
    Ok(Batches { schema, batches })
}

pub(crate) fn read_lineage(dir: &Path, k: u32) -> Result<Lineage> {
    read_json(&version_dir(dir, k).join("lineage.json"))
}

/// Removes a directory so that it is never seen half gone: renamed out
/// of the way first, then removed.
pub(crate) fn remove(dir: &Path) -> Result<()> {
    if !dir.exists() {
        return Ok(());
    }
    let name = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let doomed = dir.with_file_name(format!(".deleting-{name}-{}", crate::time::nanos()));
    std::fs::rename(dir, &doomed).map_err(|e| Error::io(dir, e))?;
    std::fs::remove_dir_all(&doomed).map_err(|e| Error::io(&doomed, e))
}
