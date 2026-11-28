use crate::engine::{parse_rfc3339, unix_now};
use crate::error::{Error, Result};
use crate::manifest::{Manifest, ROWS_FORMAT, ROWS_FORMAT_TEXT};
use std::io::BufRead;
use std::path::{Path, PathBuf};
use std::time::Duration;
use zsql_datasource::{Batches, Rows, Sink};

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

    pub(crate) fn dir_of(&self, key: &str) -> PathBuf {
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
        let mut writing = self.begin(&manifest.key)?;
        let taken = writing.schema(&rows.schema).and_then(|()| rows.batches.iter().try_for_each(|batch| writing.batch(batch.clone())));
        match (taken, writing.failure.take()) {
            (_, Some(why)) => Err(why),
            (Err(e), None) => Err(Error::Source(e)),
            (Ok(()), None) => writing.commit(manifest),
        }
    }

    /// Starts a result that will arrive a batch at a time: a [`Sink`] a
    /// read is pointed at, which writes each batch to disk as it comes
    /// and holds none of them. Nothing is a result until [`Writing::commit`];
    /// a write that is dropped before then leaves nothing behind.
    pub fn begin(&self, key: &str) -> Result<Writing> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let dir = self.dir_of(key);
        // A directory of this write's own, so two runs of one key at
        // once each write a whole result and the later rename wins.
        let tmp = dir.with_extension(format!("tmp-{}-{}", std::process::id(), NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed)));
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| Error::Io(tmp.display().to_string(), e))?;
        Ok(Writing { dir, tmp, writer: None, schema: None, rows: 0, affected: None, failure: None })
    }
}

/// A result being written as it arrives.
pub struct Writing {
    dir: PathBuf,
    tmp: PathBuf,
    writer: Option<arrow_ipc::writer::FileWriter<std::io::BufWriter<std::fs::File>>>,
    schema: Option<arrow_schema::SchemaRef>,
    rows: u64,
    /// Rows a statement that returned none changed, when its
    /// transport said.
    affected: Option<u64>,
    /// Why a write failed, in full: the read is told only that it did,
    /// in the terms a read has, and whoever started it asks here.
    failure: Option<Error>,
}

impl Writing {
    /// How many rows have been written.
    pub fn rows(&self) -> u64 {
        self.rows
    }

    /// How many rows the statement changed, when it returned none and
    /// its transport said.
    pub fn affected(&self) -> Option<u64> {
        self.affected
    }

    /// The result's columns and types, once the read has said them.
    pub fn result_schema(&self) -> Option<&arrow_schema::SchemaRef> {
        self.schema.as_ref()
    }

    /// Why the last write failed, taken: a full disk, an Arrow error.
    pub fn failure(&mut self) -> Option<Error> {
        self.failure.take()
    }

    fn fail(&mut self, why: Error) -> zsql_datasource::Error {
        let said = zsql_datasource::Error::Query("result".into(), format!("the result could not be written: {why}"));
        self.failure = Some(why);
        said
    }

    /// Finishes the rows, writes the manifest beside them, and moves
    /// the whole into place with one rename.
    pub fn commit(mut self, manifest: &Manifest) -> Result<Stored> {
        let dir = self.land(Some(manifest))?;
        Ok(Stored { manifest: manifest.clone(), dir })
    }

    /// Finishes the rows and moves them into place with one rename, with
    /// no manifest: for a result whose record is kept somewhere else.
    /// The directory it lands in comes back.
    pub fn finish(mut self) -> Result<PathBuf> {
        self.land(None)
    }

    fn land(&mut self, manifest: Option<&Manifest>) -> Result<PathBuf> {
        let io = |p: &Path, e: std::io::Error| Error::Io(p.display().to_string(), e);
        let rows_path = self.tmp.join(ROWS_FILE);
        let Some(mut writer) = self.writer.take() else {
            return Err(Error::Io(rows_path.display().to_string(), std::io::Error::other("the read ended without saying its columns")));
        };
        writer.finish().map_err(|e| Error::Arrow(rows_path.display().to_string(), e))?;
        drop(writer);
        if let Some(manifest) = manifest {
            let manifest_path = self.tmp.join("manifest.json");
            let mut text = serde_json::to_string_pretty(manifest)?;
            text.push('\n');
            std::fs::write(&manifest_path, text).map_err(|e| io(&manifest_path, e))?;
        }
        if let Some(parent) = self.dir.parent() {
            std::fs::create_dir_all(parent).map_err(|e| io(parent, e))?;
        }
        // The same key already there is the same answer, written by a run
        // that overlapped this one or by --fresh: this one replaces it.
        let _ = std::fs::remove_dir_all(&self.dir);
        std::fs::rename(&self.tmp, &self.dir).map_err(|e| io(&self.dir, e))?;
        Ok(self.dir.clone())
    }
}

impl Drop for Writing {
    /// After a commit the directory has been renamed away and this
    /// removes nothing; before one, it removes what was half written.
    fn drop(&mut self) {
        self.writer = None;
        let _ = std::fs::remove_dir_all(&self.tmp);
    }
}

impl Sink for Writing {
    fn schema(&mut self, schema: &arrow_schema::SchemaRef) -> zsql_datasource::Result<()> {
        let rows_path = self.tmp.join(ROWS_FILE);
        let opened = (|| {
            let arrow = |e: arrow_schema::ArrowError| Error::Arrow(rows_path.display().to_string(), e);
            let file = std::fs::File::create(&rows_path).map_err(|e| Error::Io(rows_path.display().to_string(), e))?;
            let options = arrow_ipc::writer::IpcWriteOptions::default()
                .try_with_compression(Some(arrow_ipc::CompressionType::LZ4_FRAME))
                .map_err(arrow)?;
            arrow_ipc::writer::FileWriter::try_new_with_options(std::io::BufWriter::new(file), schema, options).map_err(arrow)
        })();
        match opened {
            Ok(writer) => {
                self.writer = Some(writer);
                self.schema = Some(schema.clone());
                Ok(())
            }
            Err(why) => Err(self.fail(why)),
        }
    }

    fn batch(&mut self, batch: arrow_array::RecordBatch) -> zsql_datasource::Result<()> {
        let rows_path = self.tmp.join(ROWS_FILE);
        let written = match &mut self.writer {
            Some(writer) => writer.write(&batch).map_err(|e| Error::Arrow(rows_path.display().to_string(), e)),
            None => Err(Error::Io(rows_path.display().to_string(), std::io::Error::other("rows arrived before their columns"))),
        };
        match written {
            Ok(()) => {
                self.rows += batch.num_rows() as u64;
                Ok(())
            }
            Err(why) => Err(self.fail(why)),
        }
    }

    fn affected(&mut self, rows: u64) -> zsql_datasource::Result<()> {
        self.affected = Some(rows);
        Ok(())
    }
}

/// The rows file of a result that was written into `dir`.
pub fn rows_file(dir: &Path) -> PathBuf {
    dir.join(ROWS_FILE)
}

/// Rows `offset..offset + limit` of a result's file, and its schema,
/// read without the rest: a batch that holds none of the page is
/// skipped where it lies, and nothing past the page is read.
pub fn page(path: &Path, offset: u64, limit: u64) -> Result<Batches> {
    let arrow = |e: arrow_schema::ArrowError| Error::Arrow(path.display().to_string(), e);
    let file = std::fs::File::open(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    let mut reader = arrow_ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None).map_err(arrow)?;
    let schema = reader.schema();
    let (mut skip, mut left) = (offset, limit);
    let mut batches = Vec::new();
    while left > 0 {
        let Some(batch) = reader.next() else { break };
        let batch = batch.map_err(arrow)?;
        let rows = batch.num_rows() as u64;
        if skip >= rows {
            skip -= rows;
            continue;
        }
        let take = (rows - skip).min(left);
        batches.push(batch.slice(skip as usize, take as usize));
        left -= take;
        skip = 0;
    }
    Ok(Batches { schema, batches })
}

/// Every batch of a result's file to `each`, one at a time, after its
/// schema: for handing a whole result on without holding it.
pub fn each_batch(
    path: &Path,
    mut each: impl FnMut(&arrow_schema::SchemaRef, Option<arrow_array::RecordBatch>) -> Result<()>,
) -> Result<()> {
    let arrow = |e: arrow_schema::ArrowError| Error::Arrow(path.display().to_string(), e);
    let file = std::fs::File::open(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None).map_err(arrow)?;
    let schema = reader.schema();
    each(&schema, None)?;
    for batch in reader {
        each(&schema, Some(batch.map_err(arrow)?))?;
    }
    Ok(())
}

/// A result's file as an Arrow IPC stream, given to `emit` in pieces as
/// it is written: for sending a whole result on without holding it.
pub fn arrow_stream(path: &Path, emit: impl FnMut(Vec<u8>) -> Result<()>) -> Result<()> {
    struct Pieces<F> {
        emit: F,
        failed: Option<Error>,
    }
    impl<F: FnMut(Vec<u8>) -> Result<()>> std::io::Write for Pieces<F> {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            match (self.emit)(bytes.to_vec()) {
                Ok(()) => Ok(bytes.len()),
                Err(e) => {
                    let said = std::io::Error::other(e.to_string());
                    self.failed = Some(e);
                    Err(said)
                }
            }
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    let arrow = |e: arrow_schema::ArrowError| Error::Arrow(path.display().to_string(), e);
    let file = std::fs::File::open(path).map_err(|e| Error::Io(path.display().to_string(), e))?;
    let reader = arrow_ipc::reader::FileReader::try_new(std::io::BufReader::new(file), None).map_err(arrow)?;
    // A piece is a buffer's worth, not a write's: the writer makes many
    // small writes to a batch.
    let out = std::io::BufWriter::with_capacity(256 * 1024, Pieces { emit, failed: None });
    let mut writer = arrow_ipc::writer::StreamWriter::try_new(out, &reader.schema()).map_err(arrow)?;
    for batch in reader {
        writer.write(&batch.map_err(arrow)?).map_err(arrow)?;
    }
    writer.finish().map_err(arrow)?;
    let mut out = writer.into_inner().map_err(arrow)?;
    std::io::Write::flush(&mut out).map_err(|e| Error::Io(path.display().to_string(), e))?;
    Ok(())
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
        !zsql_datasource::CacheTtl::from_secs(self.manifest.cache_ttl_secs).answers(self.age())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::{cache_key, rfc3339};
    use crate::manifest::{Column, Lineage, Timings, MANIFEST_FORMAT};

    fn manifest_for_tests() -> Manifest {
        Manifest {
            format: MANIFEST_FORMAT,
            key: String::new(),
            id: "r-20261022-test".into(),
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
        }
    }

    /// A warehouse read straight into the store: the rows go from the
    /// engine to disk a batch at a time, and what is read back is what
    /// the warehouse holds.
    #[test]
    fn a_read_streams_into_the_store() {
        let root = std::env::temp_dir().join(format!("zsql-exec-stream-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(&root).unwrap();
        let file = root.join("w.db");
        {
            let db = rusqlite::Connection::open(&file).unwrap();
            db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);").unwrap();
            let mut insert = db.prepare("INSERT INTO t VALUES (?1, ?2)").unwrap();
            for i in 0..20_000 {
                insert.execute((i, if i % 7 == 0 { None } else { Some(format!("row {i}")) })).unwrap();
            }
        }
        let conn = zsql_datasource::Connection { uid: "wh".into(), adapter: "sqlite".into(), file: Some(file), ..Default::default() };
        let mut src = zsql_datasource::open(&conn, zsql_datasource::Secrets::default()).unwrap();
        let store = Store::open(&root.join("results")).unwrap();
        let key = cache_key("wh", "sqlite", None, "SELECT id, name FROM t ORDER BY id", None);

        let mut writing = store.begin(&key).unwrap();
        src.stream("SELECT id, name FROM t ORDER BY id", &mut writing).unwrap();
        assert_eq!(writing.rows(), 20_000);
        assert_eq!(writing.result_schema().unwrap().fields().len(), 2);
        let manifest = Manifest { key: key.clone(), rows: writing.rows(), ..manifest_for_tests() };
        let back = writing.commit(&manifest).unwrap().batches().unwrap();
        assert!(back.batches.len() > 1, "more than one batch arrived: {}", back.batches.len());
        assert_eq!(back.num_rows(), 20_000);
        let text = back.to_text().unwrap();
        assert_eq!(text.rows[0], vec![Some("0".to_string()), None]);
        assert_eq!(text.rows[19_998], vec![Some("19998".to_string()), Some("row 19998".to_string())]);
        assert_eq!(text.rows[19_999], vec![Some("19999".to_string()), None], "the last row, a null in it");

        // A statement that fails leaves no result and no litter.
        let mut failed = store.begin(&key).unwrap();
        assert!(src.stream("SELECT nope FROM t", &mut failed).is_err());
        drop(failed);
        assert_eq!(store.list().unwrap().len(), 1);
        let _ = std::fs::remove_dir_all(&root);
    }

    #[test]
    fn a_result_is_written_whole_or_not_at_all_and_read_back() {
        let root = std::env::temp_dir().join(format!("zsql-exec-store-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let store = Store::open(&root).unwrap();
        let key = cache_key("wh", "sqlite", None, "SELECT status, count(*) FROM orders GROUP BY 1", None);
        let manifest = Manifest { key: key.clone(), id: format!("r-20261022-{}", &key[..8]), ..manifest_for_tests() };
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
        let leftovers = |store: &Store| {
            let shard = store.dir_of(&key).parent().unwrap().to_path_buf();
            std::fs::read_dir(shard).unwrap().filter(|e| e.as_ref().unwrap().file_name().to_string_lossy().contains("tmp")).count()
        };
        assert_eq!(leftovers(&store), 0, "the temporary directory was renamed away");

        let back = store.lookup(&key).unwrap().unwrap().batches().unwrap();
        assert_eq!(back.schema, written.schema, "the types survive the disk");
        assert_eq!(back.batches, written.batches);
        assert_eq!(back.to_text().unwrap().rows, rows);

        // The same result arriving a batch at a time: each batch is on
        // disk as it comes, nothing is a result until it is committed,
        // and a write that is given up leaves nothing behind.
        {
            let mut writing = store.begin(&key).unwrap();
            writing.schema(&written.schema).unwrap();
            for batch in &written.batches {
                writing.batch(batch.slice(0, 1)).unwrap();
                writing.batch(batch.slice(1, batch.num_rows() - 1)).unwrap();
            }
            assert_eq!(writing.rows(), 2);
            assert_eq!(leftovers(&store), 1, "written beside the result, not over it");
        }
        assert_eq!(leftovers(&store), 0, "dropped before a commit, it is gone");
        let mut writing = store.begin(&key).unwrap();
        writing.schema(&written.schema).unwrap();
        for batch in &written.batches {
            writing.batch(batch.slice(0, 1)).unwrap();
            writing.batch(batch.slice(1, batch.num_rows() - 1)).unwrap();
        }
        let streamed = writing.commit(&manifest).unwrap().batches().unwrap();
        assert_eq!(streamed.schema, written.schema);
        assert_eq!(streamed.batches.len(), 2, "the batches as they arrived");
        assert_eq!(streamed.to_text().unwrap().rows, rows);
        assert_eq!(leftovers(&store), 0);
        // Rows before their columns is a failed write, kept in full for
        // whoever started it, and told to the read in a read's terms.
        let mut unnamed = store.begin(&key).unwrap();
        let said = unnamed.batch(written.batches[0].clone()).unwrap_err().to_string();
        assert!(said.contains("the result could not be written"), "{said}");
        assert!(matches!(unnamed.failure(), Some(Error::Io(_, _))));
        drop(unnamed);
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
