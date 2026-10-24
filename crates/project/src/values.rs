//! Sampled dimension values: what a dimension's column actually holds,
//! so a question like "shoes" can be matched to a value before any
//! warehouse probe. `zsql sample` writes it to `samples/values.jsonl.gz`,
//! one JSON object per line, gzipped, from the warehouse, for every
//! dimension whose model says `enable_sampling: true` — never a hidden
//! dimension, never one a security policy names — and it ships with the
//! model on deploy.
//!
//! The file is customer data. It is gitignored by default, and committing
//! it is a decision about where those values may go. Collecting it for a
//! project's own model is one consent; pooling it into anything beyond
//! that project is a second, separate one, and nothing here does that.

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::Path;

pub const FILE_NAME: &str = "values.jsonl.gz";
/// The most the file may hold once inflated: a thousand values for
/// each of a few thousand dimensions is tens of megabytes.
pub const INFLATED_LIMIT: u64 = 256 << 20;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ValueCatalog {
    pub dimensions: Vec<DimensionValues>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DimensionValues {
    pub uid: String,
    pub name: String,
    /// Where the values came from: the datasource the planner chose for
    /// them, or the one an author's `sample_sql` ran on.
    pub table: String,
    /// RFC 3339.
    pub sampled_at: String,
    /// The limit the sampling query ran with. With `distinct` it says
    /// whether the list is complete; without `distinct`, a list as long
    /// as this is the only sign that it was cut.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sample_size: Option<i64>,
    /// Distinct values in the column, when counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<i64>,
    /// The sampled values with their row counts, most frequent first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<ValueCount>,
    /// For numbers and dates: the column's bounds instead of its values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValueCount {
    pub v: String,
    pub n: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Range {
    pub min: String,
    pub max: String,
}

impl ValueCatalog {
    /// `None` when the file does not exist.
    pub fn read(path: &Path) -> std::io::Result<Option<ValueCatalog>> {
        if !path.exists() {
            return Ok(None);
        }
        use std::io::Read;
        let file = std::fs::File::open(path)?;
        // The file is a second gzip layer inside a deploy, so the
        // archive's own unpacked limit does not cover it: a small file
        // of zeros could inflate without end. Read at most this much.
        let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(file).take(INFLATED_LIMIT + 1));
        let mut dimensions = Vec::new();
        let mut total = 0u64;
        for line in reader.lines() {
            let line = line?;
            total += line.len() as u64 + 1;
            if total > INFLATED_LIMIT {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    format!("{FILE_NAME} inflates past {} MB", INFLATED_LIMIT >> 20),
                ));
            }
            if line.trim().is_empty() {
                continue;
            }
            dimensions.push(serde_json::from_str(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?);
        }
        Ok(Some(ValueCatalog { dimensions }))
    }

    /// Written beside the target and renamed over it, so a run that dies
    /// mid-write leaves the old file whole rather than a truncated gzip
    /// that nothing can read.
    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let tmp = path.with_extension("gz.tmp");
        let file = std::fs::File::create(&tmp)?;
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        for d in &self.dimensions {
            enc.write_all(serde_json::to_string(d)?.as_bytes())?;
            enc.write_all(b"\n")?;
        }
        enc.finish()?.sync_all()?;
        std::fs::rename(&tmp, path)
    }

    pub fn get(&self, uid: &str) -> Option<&DimensionValues> {
        self.dimensions.iter().find(|d| d.uid == uid)
    }

    /// Replaces or adds a dimension's entry, keeping the list sorted by uid.
    pub fn put(&mut self, d: DimensionValues) {
        self.dimensions.retain(|x| x.uid != d.uid);
        self.dimensions.push(d);
        self.dimensions.sort_by(|a, b| a.uid.cmp(&b.uid));
    }
}
