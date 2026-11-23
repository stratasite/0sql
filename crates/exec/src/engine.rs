use crate::error::{Error, Result};
use crate::manifest::{Column, Lineage, Manifest, Timings, MANIFEST_FORMAT, ROWS_FORMAT};
use crate::store::Store;
use sha2::Digest;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};
use zsql_core::{Model, SqlRequest};
use zsql_datasource::{Batches, Source};

/// Rows a run keeps when the question did not bound itself.
pub const DEFAULT_CAP: u64 = 10_000;
/// How long a result answers for a datasource that says nothing.
pub const DEFAULT_TTL: Duration = Duration::from_secs(24 * 3600);

/// Opens the datasource the planner named. The command line reads
/// `datasources.yml` and `.zsql`; a server would bring its own.
pub trait Connector {
    fn open(&self, datasource_uid: &str) -> zsql_datasource::Result<Box<dyn Source>>;
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
    pub batches: Batches,
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
        let planned = model.plan_request(req.query, false, &zsql_core::unlimited).map_err(|e| Error::Plan(e.to_string()))?;
        let plan_us = t0.elapsed().as_micros();
        let dialect = model.dialects.get(&planned.plan.adapter).map_err(|e| Error::Plan(e.to_string()))?;

        // The user's limit is the question's; the cap is this side's.
        // Without a user limit, one past the cap goes into the statement,
        // so the warehouse never ships more than is kept plus one.
        let (sql, cap, max_rows) = match (planned.limit, req.limit) {
            (Some(n), _) => (planned.plan.sql_with_limit(Some(n), &dialect).map_err(|e| Error::Plan(e.to_string()))?, None, n as usize),
            (None, Limit::Rows(cap)) => {
                (planned.plan.sql_with_limit(Some(cap + 1), &dialect).map_err(|e| Error::Plan(e.to_string()))?, Some(cap), cap as usize + 1)
            }
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
                        tracing::debug!(datasource = %datasource_uid, age_secs = stored.age().as_secs(), "answered from a stored result");
                        let batches = stored.batches()?;
                        return Ok(Outcome {
                            cache: CacheStatus::Hit { age: stored.age() },
                            stored: Some(stored.dir.clone()),
                            manifest: stored.manifest,
                            batches,
                        });
                    }
                }
            }
        }

        let t1 = Instant::now();
        let mut src = connector.open(&datasource_uid)?;
        let connect_ms = t1.elapsed().as_millis();
        let t2 = Instant::now();
        let mut batches = src.batches(&sql, max_rows)?;
        let run_ms = t2.elapsed().as_millis();
        // Read after the statement: a source warns about what running it
        // found too, a transport it had to give up on among them, and
        // the manifest is where that is kept.
        let mut warnings: Vec<String> = src.warnings().to_vec();
        let transport = src.transport().to_string();
        tracing::debug!(
            datasource = %datasource_uid,
            transport = %transport,
            rows = batches.num_rows() as u64,
            connect_ms = connect_ms as u64,
            run_ms = run_ms as u64,
            "a statement ran"
        );
        let complete = match cap {
            Some(cap) => !batches.truncate(cap as usize),
            None => true,
        };
        if !complete {
            warnings.push(format!(
                "cut at {} rows; the question had more. A cut GROUP BY without an ORDER BY is an arbitrary subset.",
                cap.unwrap_or(0)
            ));
        }

        let s = model.semantic().snapshot();
        let columns: Vec<Column> = batches
            .schema
            .fields()
            .iter()
            .map(|column| {
                let name = column.name();
                let arrow_type = Some(column.data_type().to_string());
                match s.fields.iter().find(|f| f.name.eq_ignore_ascii_case(name) || f.uid.eq_ignore_ascii_case(name)) {
                    Some(f) => Column {
                        name: name.clone(),
                        data_type: f.data_type.and_then(|d| serde_json::to_value(d).ok()).and_then(|v| v.as_str().map(str::to_string)),
                        source: "model".into(),
                        arrow_type,
                        field_uid: Some(f.uid.clone()),
                    },
                    None => Column { name: name.clone(), data_type: None, source: "unknown".into(), arrow_type, field_uid: None },
                }
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
            transport: Some(transport),
            connection_identity: identity,
            expr,
            spec,
            context_digest,
            sql,
            user_limit: planned.limit,
            cap,
            rows: batches.num_rows() as u64,
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
            (Some(store), true) => Some(store.write(&manifest, &batches)?.dir),
            _ => None,
        };
        Ok(Outcome { manifest, batches, cache, stored })
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

pub(crate) fn unix_now() -> u64 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0)
}

/// RFC 3339 at second precision, the same arithmetic the catalog uses.
pub(crate) fn rfc3339(secs: u64) -> String {
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
pub(crate) fn parse_rfc3339(s: &str) -> Option<u64> {
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
}
