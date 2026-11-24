use crate::engine::{parse_rfc3339, unix_now};
use crate::error::{Error, Result};
use crate::manifest::{Manifest, ROWS_FORMAT, ROWS_FORMAT_TEXT};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zsql_datasource::{Batches, Rows};

const ROWS_FILE: &str = "rows.arrow";
const ROWS_FILE_TEXT: &str = "rows.jsonl.gz";

/// Where results live. `root` is one project's directory; a result is
/// `<root>/<key[..2]>/<key>/`.
pub struct Store {
    root: PathBuf,
}

/// A result on disk.
#[derive(Debug)]
pub struct Stored {
    pub manifest: Manifest,
    pub(crate) dir: PathBuf,
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        std::fs::create_dir_all(root).map_err(|e| Error::Io(root.display().to_string(), e))?;
        Ok(Store { root: root.to_path_buf() })
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn dir_of(&self, key: &str) -> PathBuf {
        self.root.join(&key[..2]).join(key)
    }

    /// The result under this key, if it was committed.
    pub fn lookup(&self, key: &str) -> Result<Option<Stored>> {
        let dir = self.dir_of(key);
        let path = dir.join("manifest.json");
        if !path.exists() {
            return Ok(None);
        }
        let text = std::fs::read_to_string(&path).map_err(|e| Error::Io(path.display().to_string(), e))?;
        Ok(Some(Stored { manifest: serde_json::from_str(&text)?, dir }))
    }

    /// Every committed result, newest first.
    pub fn list(&self) -> Result<Vec<Stored>> {
        let mut out = Vec::new();
        let Ok(shards) = std::fs::read_dir(&self.root) else { return Ok(out) };
        for shard in shards.flatten() {
            let Ok(entries) = std::fs::read_dir(shard.path()) else { continue };
            for entry in entries.flatten() {
                let path = entry.path().join("manifest.json");
                let Ok(text) = std::fs::read_to_string(&path) else { continue };
                let Ok(manifest) = serde_json::from_str::<Manifest>(&text) else { continue };
                out.push(Stored { manifest, dir: entry.path() });
            }
        }
        out.sort_by(|a, b| b.manifest.created_at.cmp(&a.manifest.created_at));
        Ok(out)
    }

    /// One result by its id, or by a prefix of its id or key that names
    /// exactly one.
    pub fn get(&self, id_or_prefix: &str) -> Result<Stored> {
        let want = id_or_prefix.trim();
        let all = self.list()?;
        let mut hits: Vec<Stored> = all
            .into_iter()
            .filter(|s| s.manifest.id == want || s.manifest.id.starts_with(want) || s.manifest.key.starts_with(want))
            .collect();
        match hits.len() {
            0 => Err(Error::NotFound(want.to_string())),
            1 => Ok(hits.remove(0)),
            _ => {
                if let Some(i) = hits.iter().position(|s| s.manifest.id == want) {
                    Ok(hits.remove(i))
                } else {
                    Err(Error::Ambiguous(want.to_string()))
                }
            }
        }
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        let dir = self.dir_of(key);
        std::fs::remove_dir_all(&dir).map_err(|e| Error::Io(dir.display().to_string(), e))
    }

    /// Every result of this project, gone.
    pub fn purge(&self) -> Result<usize> {
        let n = self.list()?.len();
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root).map_err(|e| Error::Io(self.root.display().to_string(), e))?;
        }
        std::fs::create_dir_all(&self.root).map_err(|e| Error::Io(self.root.display().to_string(), e))?;
        Ok(n)
    }

    /// Writes a result: the rows first, into a directory beside the
    /// final one, then the manifest, then one rename. A reader that
    /// finds no manifest finds no result.
    pub(crate) fn write(&self, manifest: &Manifest, rows: &Batches) -> Result<Stored> {
        let dir = self.dir_of(&manifest.key);
        let tmp = dir.with_extension("tmp");
        let io = |p: &Path, e: std::io::Error| Error::Io(p.display().to_string(), e);
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| io(&tmp, e))?;
        let rows_path = tmp.join(ROWS_FILE);
        {
            let arrow = |e: arrow_schema::ArrowError| Error::Arrow(rows_path.display().to_string(), e);
            let file = std::fs::File::create(&rows_path).map_err(|e| io(&rows_path, e))?;
            let options = arrow_ipc::writer::IpcWriteOptions::default()
                .try_with_compression(Some(arrow_ipc::CompressionType::LZ4_FRAME))
                .map_err(arrow)?;
            let mut writer =
                arrow_ipc::writer::FileWriter::try_new_with_options(std::io::BufWriter::new(file), &rows.schema, options).map_err(arrow)?;
            for batch in &rows.batches {
                writer.write(batch).map_err(arrow)?;
            }
            writer.finish().map_err(arrow)?;
        }
        let manifest_path = tmp.join("manifest.json");
        let mut text = serde_json::to_string_pretty(manifest)?;
        text.push('\n');
        std::fs::write(&manifest_path, text).map_err(|e| io(&manifest_path, e))?;
        if let Some(parent) = dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        // The same key already there is the same answer, written by a run
        // that overlapped this one or by --fresh: this one replaces it.
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::rename(&tmp, &dir).map_err(|e| io(&dir, e))?;
        Ok(Stored { manifest: manifest.clone(), dir })
    }
}

impl Stored {
    pub fn dir(&self) -> &Path {
        &self.dir
    }

    /// The rows, read back as they were written: the batches and their
    /// types. A result from before 0.7 comes back as `Utf8` columns.
    pub fn batches(&self) -> Result<Batches> {
        match self.manifest.rows_format.as_str() {
            ROWS_FORMAT => {
                let path = self.dir.join(ROWS_FILE);
                let arrow = |e: arrow_schema::ArrowError| Error::Arrow(path.display().to_string(), e);
                let file = std::fs::File::open(&path).map_err(|e| Error::Io(path.display().to_string(), e))?;
                let reader = arrow_ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None).map_err(arrow)?;
                let schema = reader.schema();
                let batches = reader.collect::<std::result::Result<Vec<_>, _>>().map_err(arrow)?;
                Ok(Batches { schema, batches })
            }
            ROWS_FORMAT_TEXT => {
                let path = self.dir.join(ROWS_FILE_TEXT);
                let file = std::fs::File::open(&path).map_err(|e| Error::Io(path.display().to_string(), e))?;
                let mut rows: Vec<Vec<Option<String>>> = Vec::new();
                for line in std::io::BufReader::new(flate2::read::GzDecoder::new(file)).lines() {
                    let line = line.map_err(|e| Error::Io(path.display().to_string(), e))?;
                    if !line.trim().is_empty() {
                        rows.push(serde_json::from_str(&line)?);
                    }
                }
                let columns = self.manifest.columns.iter().map(|c| c.name.clone()).collect();
                Ok(Batches::from_text(&Rows { columns, rows }))
            }
            other => Err(Error::RowsFormat(self.dir.display().to_string(), other.to_string())),
        }
    }

    /// The rows as the text they print as.
    pub fn rows(&self) -> Result<Rows> {
        Ok(self.batches()?.to_text()?)
    }

    /// Seconds since this result was written, by its own timestamp.
    pub fn age(&self) -> Duration {
        let then = parse_rfc3339(&self.manifest.created_at).unwrap_or(0);
        let now = unix_now();
        Duration::from_secs(now.saturating_sub(then))
    }

    pub fn expired(&self) -> bool {
        self.age() > Duration::from_secs(self.manifest.cache_ttl_secs)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{cache_key, rfc3339};
    use crate::manifest::{Column, Lineage, Timings, MANIFEST_FORMAT};

    #[test]
    fn a_result_is_written_whole_or_not_at_all_and_read_back() {
        let root = std::env::temp_dir().join(format!("zsql-exec-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let key = cache_key("wh", "sqlite", None, "SELECT status, count(*) FROM orders GROUP BY 1", None);
        let manifest = Manifest {
            format: MANIFEST_FORMAT,
            key: key.clone(),
            id: format!("r-20261022-{}", &key[..8]),
            created_at: rfc3339(unix_now()),
            project_uid: "shop".into(),
            branch: "working-tree".into(),
            datasource_uid: "wh".into(),
            adapter: "sqlite".into(),
            transport: Some("native".into()),
            connection_identity: None,
            expr: Some("status, calc(count(*), integer) as n".into()),
            spec: None,
            context_digest: None,
            sql: "SELECT status, count(*) FROM orders GROUP BY 1".into(),
            user_limit: None,
            cap: Some(10_000),
            rows: 2,
            complete: true,
            columns: vec![Column {
                name: "status".into(),
                data_type: Some("string".into()),
                source: "model".into(),
                arrow_type: Some("Utf8".into()),
                field_uid: Some("status".into()),
            }],
            rows_format: ROWS_FORMAT.into(),
            timings: Timings::default(),
            cache_ttl_secs: 3600,
            lineage: Lineage::default(),
            warnings: vec![],
        };
        // A typed column beside a text one: what is read back is what was
        // written, type included, not a string to be parsed again.
        let schema: arrow_schema::SchemaRef = std::sync::Arc::new(arrow_schema::Schema::new(vec![
            arrow_schema::Field::new("status", arrow_schema::DataType::Utf8, true),
            arrow_schema::Field::new("n", arrow_schema::DataType::Int64, true),
            arrow_schema::Field::new("amount", arrow_schema::DataType::Decimal128(38, 9), true),
        ]));
        let batch = arrow_array::RecordBatch::try_new(
            schema.clone(),
            vec![
                std::sync::Arc::new(arrow_array::StringArray::from(vec![Some("paid"), None])),
                std::sync::Arc::new(arrow_array::Int64Array::from(vec![Some(3), Some(1)])),
                std::sync::Arc::new(
                    arrow_array::Decimal128Array::from(vec![Some(12_500_000_000), Some(3_000_000_000)])
                        .with_precision_and_scale(38, 9)
                        .unwrap(),
                ),
            ],
        )
        .unwrap();
        // The decimal is marked to print without trailing zeros, as a
        // warehouse of fixed scale marks it; the mark is kept with it.
        let written = Batches { schema, batches: vec![batch] }.printing_decimals_trimmed();
        let rows = vec![
            vec![Some("paid".to_string()), Some("3".to_string()), Some("12.5".to_string())],
            vec![None, Some("1".to_string()), Some("3".to_string())],
        ];
        let stored = store.write(&manifest, &written).unwrap();
        assert!(stored.dir().join("rows.arrow").exists());
        assert!(stored.dir().join("manifest.json").exists());
        assert!(!stored.dir().with_extension("tmp").exists(), "the temporary directory was renamed away");
        let back = store.lookup(&key).unwrap().unwrap().batches().unwrap();
        assert_eq!(back.schema, written.schema, "the types survive the disk");
        assert_eq!(back.batches, written.batches);
        assert_eq!(back.to_text().unwrap().rows, rows);
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.get(&manifest.id[..6]).unwrap().manifest.key, key, "a prefix of the id");
        assert_eq!(store.get(&key[..10]).unwrap().manifest.key, key, "a prefix of the key");
        assert!(matches!(store.get("r-1999").unwrap_err(), Error::NotFound(_)));
        assert!(!store.lookup(&key).unwrap().unwrap().expired());

        // What a build before 0.7 left behind: gzipped JSON lines and a
        // manifest that says so. Read as text columns, never rewritten.
        let old_key = cache_key("wh", "sqlite", None, "SELECT status FROM orders", None);
        let old_dir = store.dir_of(&old_key);
        std::fs::create_dir_all(&old_dir).unwrap();
        {
            use std::io::Write;
            let mut enc = flate2::write::GzEncoder::new(
                std::fs::File::create(old_dir.join("rows.jsonl.gz")).unwrap(),
                flate2::Compression::default(),
            );
            enc.write_all(b"[\"paid\",\"3\"]\n[null,\"1\"]\n").unwrap();
            enc.finish().unwrap();
        }
        let mut old =
            Manifest { format: 1, key: old_key.clone(), rows_format: ROWS_FORMAT_TEXT.into(), transport: None, ..manifest.clone() };
        old.columns.push(Column { name: "n".into(), data_type: None, source: "unknown".into(), arrow_type: None, field_uid: None });
        std::fs::write(old_dir.join("manifest.json"), serde_json::to_string(&old).unwrap()).unwrap();
        let read = store.lookup(&old_key).unwrap().unwrap();
        let old_rows = vec![vec![Some("paid".to_string()), Some("3".to_string())], vec![None, Some("1".to_string())]];
        assert_eq!(read.rows().unwrap().rows, old_rows, "an old result still answers");
        assert!(read.batches().unwrap().schema.fields().iter().all(|f| f.data_type() == &arrow_schema::DataType::Utf8));
        let unknown = Stored { manifest: Manifest { rows_format: "parquet".into(), ..manifest.clone() }, dir: old_dir.clone() };
        assert!(matches!(unknown.batches().unwrap_err(), Error::RowsFormat(_, f) if f == "parquet"));
        store.remove(&old_key).unwrap();

        store.remove(&key).unwrap();
        assert!(store.lookup(&key).unwrap().is_none());
        assert_eq!(store.purge().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
