//! Where sources and datasets are kept, and the only module that knows.
//!
//! ```text
//! <data-dir>/<org>/<project>/.datasets/
//!   .sources/<src>/source.json             a source: kind, name, how it is read
//!   .sources/<src>/snapshots/<n>/          rows.arrow and its manifest
//!   <ds>/dataset.json                      a dataset's manifest, written last
//!   <ds>/v<k>/                             an output: rows.arrow, or a pointer to a
//!                                          snapshot; manifest, lineage, run, graph
//!   .runs/<run>.json                       a run's record, in flight or done
//! ```
//!
//! Nothing is changed in place. A file is written beside where it goes
//! and renamed there; a new source or dataset is built in a dot
//! directory and renamed into place whole; a manifest is written after
//! what it points at, so a crash leaves things as they were before the
//! job. The directory starts with a dot, which no branch name does, so
//! the server's loader passes over it.

use crate::error::{Error, Result};
use crate::lineage::Lineage;
use crate::schema::{Column, ColumnType};
use crate::source::{Kind, Parse, Query, SemanticRead};
use arrow_array::RecordBatch;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use std::collections::BTreeMap;
use std::path::{Path, PathBuf};
use zsql_datasource::Batches;

/// The directory, under a project's, that holds its datasets.
pub const DIR: &str = ".datasets";

/// Where, under that, sources are.
pub(crate) const SOURCES: &str = ".sources";

/// Where, under that, runs are recorded.
pub(crate) const RUNS: &str = ".runs";

pub(crate) const MANIFEST: &str = "dataset.json";
pub(crate) const SOURCE: &str = "source.json";
pub(crate) const ROWS: &str = "rows.arrow";

/// A dataset's manifest: what is browsed, shared and blended.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Dataset {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub created: String,
    pub updated: String,
    /// The output it answers with, once a run made one.
    pub live: Option<u32>,
    /// The last output made.
    pub made: u32,
    /// The live output's columns and rows.
    pub columns: Vec<Column>,
    pub rows: u64,
    /// What the run that made the live output said: a warning, a check
    /// let through.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub notes: Vec<String>,
    /// The last run asked for it, whatever it came to.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub last_run: Option<LastRun>,
    /// Where its copy in the object store stands, when there is one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<crate::backup::Backup>,
    /// The job a restart found in flight and did not run again, until
    /// its next job. Kept beside the manifest, not in it.
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<crate::recovery::Interrupted>,
}

/// A dataset's last run, as its manifest says.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct LastRun {
    pub id: String,
    pub state: crate::record::RunState,
    pub at: String,
}

/// A source: where data comes from, and what came.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Source {
    pub id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub kind: Kind,
    pub created: String,
    pub updated: String,
    /// The newest snapshot; 0 before the first.
    pub snapshot: u32,
    /// The newest snapshot's columns: the types every later one meets.
    pub columns: Vec<Column>,
    /// What a query source runs.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub query: Option<Query>,
    /// Columns read as other types than they arrive as, for every graph
    /// that reads it.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub types: BTreeMap<String, ColumnType>,
    /// The version of how it is read: one more with each change of `types`.
    pub read: u32,
    /// Every version of how it is read.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub reads: Vec<ReadOptions>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub backup: Option<crate::backup::Backup>,
    #[serde(default, skip_deserializing, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<crate::recovery::Interrupted>,
}

/// One version of how a source is read.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct ReadOptions {
    pub version: u32,
    pub at: String,
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub types: BTreeMap<String, ColumnType>,
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
    /// The hash of what arrived: the file as uploaded, or the rows as kept.
    pub sha256: String,
    pub bytes: u64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parse: Option<Parse>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub semantic: Option<SemanticRead>,
    /// Where pushed rows came from, as the pusher said.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<serde_json::Value>,
    pub columns: Vec<Column>,
    pub rows: u64,
    /// How its columns met the source's: new, gone, widened, converted.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub changes: Vec<String>,
}

/// An output's manifest.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Materialization {
    pub version: u32,
    pub made: String,
    pub columns: Vec<Column>,
    pub rows: u64,
    pub bytes: u64,
    /// Set when the output is a source's snapshot as it is: no rows of
    /// its own are kept.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pointer: Option<Pointer>,
    /// The run that made it.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub run: Option<String>,
}

/// A source's snapshot an output is.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Pointer {
    pub source: String,
    pub snapshot: u32,
}

pub struct Store {
    data_dir: PathBuf,
}

impl Store {
    pub fn new(data_dir: &Path) -> Store {
        Store { data_dir: data_dir.to_path_buf() }
    }

    pub fn data_dir(&self) -> PathBuf {
        self.data_dir.clone()
    }

    /// Where a project's kept blends are.
    pub fn blends(&self, org: &str, project: &str) -> PathBuf {
        self.data_dir.join(".cache").join("blends").join(org).join(project)
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

    /// Every dataset of a project, by id.
    pub fn all(&self, org: &str, project: &str) -> Vec<(PathBuf, Dataset)> {
        datasets_in(&self.root(org, project))
    }

    /// Every source of a project, by id.
    pub fn sources(&self, org: &str, project: &str) -> Vec<(PathBuf, Source)> {
        sources_in(&self.root(org, project))
    }
}

/// Every dataset under a project's root.
pub(crate) fn datasets_in(root: &Path) -> Vec<(PathBuf, Dataset)> {
    let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
    let mut out: Vec<(PathBuf, Dataset)> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let dir = e.path();
            let mut d: Dataset = read_json(&dir.join(MANIFEST)).ok()?;
            d.interrupted = read_json(&dir.join(crate::recovery::INTERRUPTED)).ok();
            Some((dir, d))
        })
        .collect();
    out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    out
}

/// Every source under a project's root.
pub(crate) fn sources_in(root: &Path) -> Vec<(PathBuf, Source)> {
    let Ok(entries) = std::fs::read_dir(root.join(SOURCES)) else { return Vec::new() };
    let mut out: Vec<(PathBuf, Source)> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .filter_map(|e| {
            let dir = e.path();
            let mut s: Source = read_json(&dir.join(SOURCE)).ok()?;
            s.interrupted = read_json(&dir.join(crate::recovery::INTERRUPTED)).ok();
            Some((dir, s))
        })
        .collect();
    out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
    out
}

pub(crate) fn source_dir(root: &Path, id: &str) -> PathBuf {
    root.join(SOURCES).join(id)
}

pub(crate) fn snapshot_dir(source_dir: &Path, n: u32) -> PathBuf {
    source_dir.join("snapshots").join(n.to_string())
}

pub(crate) fn version_dir(dir: &Path, k: u32) -> PathBuf {
    dir.join(format!("v{k}"))
}

pub(crate) fn run_path(root: &Path, id: &str) -> PathBuf {
    root.join(RUNS).join(format!("{id}.json"))
}

/// An output's manifest and rows: its own, or the snapshot it points to.
pub(crate) fn version_rows(dir: &Path, k: u32) -> Result<(Materialization, Batches)> {
    let (m, path) = version_rows_path(dir, k)?;
    let rows = read_rows(&path)?;
    Ok((m, rows))
}

/// An output's manifest, and where its rows are.
pub(crate) fn version_rows_path(dir: &Path, k: u32) -> Result<(Materialization, PathBuf)> {
    let v = version_dir(dir, k);
    let m: Materialization = read_json(&v.join("manifest.json"))?;
    let path = match &m.pointer {
        Some(p) => {
            let root = dir.parent().unwrap_or(dir);
            snapshot_dir(&source_dir(root, &p.source), p.snapshot).join(ROWS)
        }
        None => v.join(ROWS),
    };
    Ok((m, path))
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
    {
        use std::io::Write;
        let mut f = std::fs::File::create(&tmp).map_err(|e| Error::io(&tmp, e))?;
        f.write_all(bytes).map_err(|e| Error::io(&tmp, e))?;
        f.sync_data().map_err(|e| Error::io(&tmp, e))?;
    }
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
    sync_dir(path.parent())
}

/// Makes a directory's entries durable: a rename into it survives a
/// power cut once this returns. A file is synced before it is renamed,
/// so a manifest never names what is not on disk.
pub(crate) fn sync_dir(dir: Option<&Path>) -> Result<()> {
    if let Some(dir) = dir {
        std::fs::File::open(dir).and_then(|d| d.sync_all()).map_err(|e| Error::io(dir, e))?;
    }
    Ok(())
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
    std::fs::File::open(&tmp).and_then(|f| f.sync_all()).map_err(|e| Error::io(&tmp, e))?;
    let bytes = std::fs::metadata(&tmp).map_err(|e| Error::io(&tmp, e))?.len();
    std::fs::rename(&tmp, path).map_err(|e| Error::io(path, e))?;
    sync_dir(path.parent())?;
    Ok(bytes)
}

pub(crate) fn read_rows(path: &Path) -> Result<Batches> {
    let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None)?;
    let schema = reader.schema();
    let batches: Vec<RecordBatch> = reader.collect::<std::result::Result<_, _>>()?;
    Ok(Batches { schema, batches })
}

/// The rows of a rows file from `offset`, at most `limit` of them,
/// reading no batch past them.
pub(crate) fn read_rows_range(path: &Path, offset: usize, limit: usize) -> Result<Batches> {
    let file = std::fs::File::open(path).map_err(|e| Error::io(path, e))?;
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None)?;
    let schema = reader.schema();
    let mut batches: Vec<RecordBatch> = Vec::new();
    let (mut skip, mut left) = (offset, limit);
    for b in reader {
        if left == 0 {
            break;
        }
        let b = b?;
        if skip >= b.num_rows() {
            skip -= b.num_rows();
            continue;
        }
        let take = left.min(b.num_rows() - skip);
        batches.push(if skip == 0 && take == b.num_rows() { b } else { b.slice(skip, take) });
        skip = 0;
        left -= take;
    }
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
