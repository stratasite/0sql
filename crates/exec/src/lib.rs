//! Running a planned query and keeping its rows.
//!
//! The planner says what to run and where; `zsql-source` runs it; this
//! crate decides whether to run it at all, keeps what came back, and
//! finds it again. A result is customer data: it lives where the caller
//! says (the command line puts it under the user's home, outside any
//! repository), it is never deployed, and it is immutable once written.
//! See `docs/run.md`.
//!
//! The store is a layout, not a database: a result is a directory named
//! by its cache key holding `rows.jsonl.gz` and `manifest.json`, the
//! manifest written last as the commit marker. Nothing is updated in
//! place, so a copy of the directory is a copy of the store.

use serde::{Deserialize, Serialize};
use serde_json::Value;
use sha2::Digest;
use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zsql_service::{Model, SqlRequest};
use zsql_source::Source;

pub const MANIFEST_FORMAT: u32 = 1;
pub const ROWS_FORMAT: &str = "jsonl.gz";
/// Rows a run keeps when the question did not bound itself.
pub const DEFAULT_CAP: u64 = 10_000;
/// How long a result answers for a datasource that says nothing.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);

#[derive(Debug, thiserror::Error)]
pub enum ExecError {
    #[error("planning: {0}")]
    Plan(String),
    #[error("{0}")]
    Source(#[from] zsql_source::SourceError),
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("no result {0}")]
    NotFound(String),
    #[error("{0} matches more than one result; give more of the id")]
    Ambiguous(String),
}

pub type Result<T> = std::result::Result<T, ExecError>;

// ----------------------------------------------------------------- manifest

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
    pub cache_ttl_secs: u64,
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
    /// `model` or `unknown`; `warehouse` once the transports say theirs.
    pub source: String,
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

// -------------------------------------------------------------------- store

/// Where results live. `root` is one project's directory; a result is
/// `<root>/<key[..2]>/<key>/`.
pub struct Store {
    root: PathBuf,
}

/// A result on disk.
#[derive(Debug)]
pub struct Stored {
    pub manifest: Manifest,
    dir: PathBuf,
}

impl Store {
    pub fn open(root: &Path) -> Result<Store> {
        std::fs::create_dir_all(root).map_err(|e| ExecError::Io(root.display().to_string(), e))?;
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
        let text = std::fs::read_to_string(&path).map_err(|e| ExecError::Io(path.display().to_string(), e))?;
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
            0 => Err(ExecError::NotFound(want.to_string())),
            1 => Ok(hits.remove(0)),
            _ => {
                if let Some(i) = hits.iter().position(|s| s.manifest.id == want) {
                    Ok(hits.remove(i))
                } else {
                    Err(ExecError::Ambiguous(want.to_string()))
                }
            }
        }
    }

    pub fn remove(&self, key: &str) -> Result<()> {
        let dir = self.dir_of(key);
        std::fs::remove_dir_all(&dir).map_err(|e| ExecError::Io(dir.display().to_string(), e))
    }

    /// Every result of this project, gone.
    pub fn purge(&self) -> Result<usize> {
        let n = self.list()?.len();
        if self.root.exists() {
            std::fs::remove_dir_all(&self.root).map_err(|e| ExecError::Io(self.root.display().to_string(), e))?;
        }
        std::fs::create_dir_all(&self.root).map_err(|e| ExecError::Io(self.root.display().to_string(), e))?;
        Ok(n)
    }

    /// Writes a result: the rows first, into a directory beside the
    /// final one, then the manifest, then one rename. A reader that
    /// finds no manifest finds no result.
    fn write(&self, manifest: &Manifest, rows: &[Vec<Option<String>>]) -> Result<Stored> {
        let dir = self.dir_of(&manifest.key);
        let tmp = dir.with_extension("tmp");
        let io = |p: &Path, e: std::io::Error| ExecError::Io(p.display().to_string(), e);
        let _ = std::fs::remove_dir_all(&tmp);
        std::fs::create_dir_all(&tmp).map_err(|e| io(&tmp, e))?;
        let rows_path = tmp.join("rows.jsonl.gz");
        {
            let file = std::fs::File::create(&rows_path).map_err(|e| io(&rows_path, e))?;
            let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
            for r in rows {
                enc.write_all(serde_json::to_string(r)?.as_bytes()).map_err(|e| io(&rows_path, e))?;
                enc.write_all(b"\n").map_err(|e| io(&rows_path, e))?;
            }
            enc.finish().map_err(|e| io(&rows_path, e))?;
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

    /// The rows, read back as they were written.
    pub fn rows(&self) -> Result<RowReader> {
        let path = self.dir.join("rows.jsonl.gz");
        let file = std::fs::File::open(&path).map_err(|e| ExecError::Io(path.display().to_string(), e))?;
        Ok(RowReader { lines: std::io::BufReader::new(flate2::read::GzDecoder::new(file)).lines(), path })
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

pub struct RowReader {
    lines: std::io::Lines<std::io::BufReader<flate2::read::GzDecoder<std::fs::File>>>,
    path: PathBuf,
}

impl Iterator for RowReader {
    type Item = Result<Vec<Option<String>>>;
    fn next(&mut self) -> Option<Self::Item> {
        let line = self.lines.next()?;
        Some(match line {
            Ok(l) if l.trim().is_empty() => return self.next(),
            Ok(l) => serde_json::from_str(&l).map_err(ExecError::Json),
            Err(e) => Err(ExecError::Io(self.path.display().to_string(), e)),
        })
    }
}

// ------------------------------------------------------------------- engine

/// Opens the datasource the planner named. The command line reads
/// `datasources.yml` and `.zsql`; a server would bring its own.
pub trait Connector {
    fn open(&self, datasource_uid: &str) -> zsql_source::Result<Box<dyn Source>>;
    /// Who the connection is, for the key: never a secret.
    fn identity(&self, datasource_uid: &str) -> Option<String>;
    /// How long a result from this datasource answers, when it says.
    fn cache_ttl(&self, datasource_uid: &str) -> Option<Duration>;
}

/// How many rows a run keeps.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Limit {
    /// At most this many; one more is fetched to know whether there were more.
    Rows(u64),
    /// Every row, held in memory: for a result known to fit.
    All,
}

pub struct RunRequest {
    pub query: SqlRequest,
    pub limit: Limit,
    /// Bypass the cache and overwrite what it holds.
    pub fresh: bool,
    /// Keep nothing: run and answer, write no result.
    pub store: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum CacheStatus {
    Miss,
    Hit { age: Duration },
    Bypassed,
    Disabled,
}

/// A run's answer: the manifest, the rows, and whether the warehouse
/// was asked.
pub struct Outcome {
    pub manifest: Manifest,
    pub rows: Vec<Vec<Option<String>>>,
    pub cache: CacheStatus,
    /// Where it was written, when it was.
    pub stored: Option<PathBuf>,
}

pub struct Engine {
    store: Option<Store>,
}

impl Engine {
    /// A store under `root`, or none when `root` is `None`: then every run
    /// goes to the warehouse and nothing is kept.
    pub fn open(root: Option<&Path>) -> Result<Engine> {
        Ok(Engine { store: root.map(Store::open).transpose()? })
    }

    pub fn store(&self) -> Option<&Store> {
        self.store.as_ref()
    }

    pub fn run(&self, model: &Model, connector: &dyn Connector, req: RunRequest) -> Result<Outcome> {
        let t0 = Instant::now();
        let expr = req.query.expr.clone();
        let spec = req.query.spec.clone();
        let context_digest = req.query.context.as_ref().map(|c| sha256(&serde_json::to_string(c).unwrap_or_default()));
        let planned = model.plan_request(req.query, false, &zsql_service::unlimited).map_err(|e| ExecError::Plan(e.to_string()))?;
        let plan_us = t0.elapsed().as_micros();
        let dialect = model.dialects.get(&planned.plan.adapter).map_err(|e| ExecError::Plan(e.to_string()))?;

        // The user's limit is the question's; the cap is this side's.
        // Without a user limit, one past the cap goes into the statement,
        // so the warehouse never ships more than is kept plus one.
        let (sql, cap, max_rows) = match (planned.limit, req.limit) {
            (Some(n), _) => (planned.plan.sql_with_limit(Some(n), &dialect).map_err(|e| ExecError::Plan(e.to_string()))?, None, n as usize),
            (None, Limit::Rows(cap)) => (
                planned.plan.sql_with_limit(Some(cap + 1), &dialect).map_err(|e| ExecError::Plan(e.to_string()))?,
                Some(cap),
                cap as usize + 1,
            ),
            (None, Limit::All) => (planned.plan.sql.clone(), None, usize::MAX),
        };
        let datasource_uid = planned.plan.datasource_uid.clone();
        let adapter = planned.plan.adapter.clone();
        let identity = connector.identity(&datasource_uid);
        let key = cache_key(&datasource_uid, &adapter, identity.as_deref(), &sql, context_digest.as_deref());
        let ttl = connector.cache_ttl(&datasource_uid).unwrap_or(DEFAULT_TTL);

        // The cache: a committed result under this key, within its TTL.
        if let (Some(store), false) = (&self.store, req.fresh) {
            if ttl.as_secs() > 0 {
                if let Some(stored) = store.lookup(&key)? {
                    if !stored.expired() {
                        let rows: Vec<Vec<Option<String>>> = stored.rows()?.collect::<Result<_>>()?;
                        return Ok(Outcome {
                            cache: CacheStatus::Hit { age: stored.age() },
                            stored: Some(stored.dir.clone()),
                            manifest: stored.manifest,
                            rows,
                        });
                    }
                }
            }
        }

        let t1 = Instant::now();
        let mut src = connector.open(&datasource_uid)?;
        let connect_ms = t1.elapsed().as_millis();
        let mut warnings: Vec<String> = src.warnings().to_vec();
        let t2 = Instant::now();
        let got = src.query(&sql, max_rows)?;
        let run_ms = t2.elapsed().as_millis();
        let mut rows = got.rows;
        let complete = match cap {
            Some(cap) if rows.len() as u64 > cap => {
                rows.truncate(cap as usize);
                false
            }
            _ => true,
        };
        if !complete {
            warnings.push(format!(
                "cut at {} rows; the question had more. A cut GROUP BY without an ORDER BY is an arbitrary subset.",
                cap.unwrap_or(0)
            ));
        }

        let s = model.universe().snapshot();
        let columns: Vec<Column> = got
            .columns
            .iter()
            .map(|name| match s.fields.iter().find(|f| f.name.eq_ignore_ascii_case(name) || f.uid.eq_ignore_ascii_case(name)) {
                Some(f) => Column {
                    name: name.clone(),
                    data_type: f.data_type.and_then(|d| serde_json::to_value(d).ok()).and_then(|v| v.as_str().map(str::to_string)),
                    source: "model".into(),
                    field_uid: Some(f.uid.clone()),
                },
                None => Column { name: name.clone(), data_type: None, source: "unknown".into(), field_uid: None },
            })
            .collect();
        let sql_lower = sql.to_lowercase();
        let tables: Vec<String> = s
            .tables
            .iter()
            .filter(|t| s.datasources.iter().any(|d| d.id == t.datasource_id && d.uid == datasource_uid))
            .filter(|t| sql_lower.contains(&t.physical_name.to_lowercase()))
            .map(|t| t.uid.clone())
            .collect();
        let created_at = rfc3339(unix_now());
        let manifest = Manifest {
            format: MANIFEST_FORMAT,
            id: format!("r-{}-{}", created_at[..10].replace('-', ""), &key[..8]),
            key: key.clone(),
            created_at,
            project_uid: model.uid.clone(),
            branch: model.branch.clone(),
            datasource_uid,
            adapter,
            connection_identity: identity,
            expr,
            spec,
            context_digest,
            sql,
            user_limit: planned.limit,
            cap,
            rows: rows.len() as u64,
            complete,
            columns: columns.clone(),
            rows_format: ROWS_FORMAT.into(),
            timings: Timings { plan_us, connect_ms, run_ms },
            cache_ttl_secs: ttl.as_secs(),
            lineage: Lineage {
                lineage_format: 1,
                items: planned.items.clone(),
                fields: columns.iter().filter_map(|c| c.field_uid.clone()).collect(),
                tables,
            },
            warnings,
        };
        let cache = if self.store.is_none() || !req.store {
            CacheStatus::Disabled
        } else if req.fresh {
            CacheStatus::Bypassed
        } else {
            CacheStatus::Miss
        };
        let stored = match (&self.store, req.store) {
            (Some(store), true) => Some(store.write(&manifest, &rows)?.dir),
            _ => None,
        };
        Ok(Outcome { manifest, rows, cache, stored })
    }
}

/// The key: the exact question on the exact data for the exact
/// principal. Not the spec (two specs can plan to one statement), not the
/// model (an edit that changes the statement changes the key; one that
/// does not keeps its cache). Secrets are never in it.
pub fn cache_key(datasource_uid: &str, adapter: &str, identity: Option<&str>, sql: &str, context_digest: Option<&str>) -> String {
    sha256(&format!("{datasource_uid}\n{adapter}\n{}\n{sql}\n{}", identity.unwrap_or(""), context_digest.unwrap_or("")))
}

fn sha256(text: &str) -> String {
    let digest = sha2::Sha256::digest(text.as_bytes());
    digest.iter().map(|b| format!("{b:02x}")).collect()
}

fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// RFC 3339 at second precision, the same arithmetic the catalog uses.
fn rfc3339(secs: u64) -> String {
    let (y, m, d, hh, mm, ss) = civil_time(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

fn civil_time(secs: u64) -> (i64, u32, u32, u64, u64, u64) {
    let days = (secs / 86_400) as i64;
    let rem = secs % 86_400;
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d, rem / 3600, (rem % 3600) / 60, rem % 60)
}

/// The inverse, for a manifest's own timestamp; `None` if it is not one.
fn parse_rfc3339(s: &str) -> Option<u64> {
    let (date, time) = s.split_once('T')?;
    let mut d = date.split('-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let time = time.trim_end_matches('Z');
    let mut t = time.split(':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (t.next()??, t.next()??, t.next()??);
    // days_from_civil
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = if m > 2 { m - 3 } else { m + 9 };
    let doy = (153 * mp + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    u64::try_from(days * 86_400 + hh * 3600 + mm * 60 + ss).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_clock_round_trips() {
        for secs in [0u64, 951_782_400, 1_761_091_200, 4_102_444_799] {
            assert_eq!(parse_rfc3339(&rfc3339(secs)), Some(secs), "{secs}");
        }
        assert_eq!(rfc3339(1_761_091_200), "2025-10-22T00:00:00Z");
    }

    #[test]
    fn the_key_is_the_question_on_the_data_for_the_principal() {
        let a = cache_key("wh", "postgres", Some("analyst"), "SELECT 1", None);
        assert_eq!(a, cache_key("wh", "postgres", Some("analyst"), "SELECT 1", None));
        assert_ne!(a, cache_key("wh2", "postgres", Some("analyst"), "SELECT 1", None), "another datasource");
        assert_ne!(a, cache_key("wh", "postgres", Some("other"), "SELECT 1", None), "another identity");
        assert_ne!(a, cache_key("wh", "postgres", Some("analyst"), "SELECT 2", None), "another statement");
        assert_ne!(a, cache_key("wh", "postgres", Some("analyst"), "SELECT 1", Some("ctx")), "another context");
        assert_eq!(a.len(), 64);
    }

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
                field_uid: Some("status".into()),
            }],
            rows_format: ROWS_FORMAT.into(),
            timings: Timings::default(),
            cache_ttl_secs: 3600,
            lineage: Lineage::default(),
            warnings: vec![],
        };
        let rows = vec![vec![Some("paid".to_string()), Some("3".to_string())], vec![None, Some("1".to_string())]];
        let stored = store.write(&manifest, &rows).unwrap();
        assert!(stored.dir().join("manifest.json").exists());
        assert!(!stored.dir().with_extension("tmp").exists(), "the temporary directory was renamed away");
        let back: Vec<_> = store.lookup(&key).unwrap().unwrap().rows().unwrap().collect::<Result<_>>().unwrap();
        assert_eq!(back, rows);
        assert_eq!(store.list().unwrap().len(), 1);
        assert_eq!(store.get(&manifest.id[..6]).unwrap().manifest.key, key, "a prefix of the id");
        assert_eq!(store.get(&key[..10]).unwrap().manifest.key, key, "a prefix of the key");
        assert!(matches!(store.get("r-1999").unwrap_err(), ExecError::NotFound(_)));
        assert!(!store.lookup(&key).unwrap().unwrap().expired());
        store.remove(&key).unwrap();
        assert!(store.lookup(&key).unwrap().is_none());
        assert_eq!(store.purge().unwrap(), 0);
        let _ = std::fs::remove_dir_all(&root);
    }
}
