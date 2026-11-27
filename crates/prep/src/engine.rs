//! The engine: DuckDB, in memory, with no access to anything outside
//! itself.
//!
//! A session is opened for one job and dropped with it. It is opened
//! with external access off, extensions neither installed nor loaded,
//! a memory limit, a thread count and a directory to spill to, and its
//! configuration is then locked so nothing run in it can change any of
//! that. It reads no file and writes none: the rows it works on are
//! handed to it as Arrow batches, and what it makes is handed back the
//! same way. A step or a formula therefore cannot reach a file, a URL
//! or another dataset, whatever it compiles to.

use crate::error::{Error, Result};
use crate::schema::{ident, ColumnType};
use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use zsql_datasource::Batches;

/// What bounds a session.
#[derive(Clone, Debug)]
pub struct EngineLimits {
    /// DuckDB's memory limit, as it writes one: `512MB`, `2GB`.
    pub memory: String,
    pub threads: usize,
    /// Where sessions spill what does not fit in memory: each in a
    /// directory of its own under this one, removed when it closes.
    pub spill: PathBuf,
    /// How long one session may work, all its queries together: a job,
    /// whatever its steps, is stopped past this.
    pub job: Duration,
}

/// How long a job may run, by default.
pub const JOB_LIMIT: Duration = Duration::from_secs(300);

/// The most a query's answer may be when it is handed back: rows, and
/// bytes of Arrow. The engine's memory limit does not count what has
/// left it, so the answer is counted as it comes.
#[derive(Clone, Copy, Debug)]
pub struct Bound {
    pub rows: usize,
    pub bytes: u64,
}

pub struct Engine {
    limits: EngineLimits,
    bound: Bound,
}

/// One job's engine. The connection closes before its spill directory
/// goes: fields drop in the order they are declared.
pub struct Session {
    watch: Watch,
    conn: duckdb::Connection,
    bound: Bound,
    deadline: Instant,
    _spill: Spill,
}

/// A session's deadline, kept by a thread of its own: at the deadline it
/// interrupts what is running, and marks the session so nothing after
/// runs either.
struct Watch {
    expired: Arc<AtomicBool>,
    limit: Duration,
    done: Option<std::sync::mpsc::Sender<()>>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Watch {
    fn drop(&mut self) {
        self.done.take();
        if let Some(t) = self.thread.take() {
            let _ = t.join();
        }
    }
}

/// A session's own spill directory. DuckDB names its spill files alike
/// in every instance, and removes them all when one closes, so two
/// sessions sharing a directory would read and delete each other's.
struct Spill(PathBuf);

impl Drop for Spill {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

impl Engine {
    pub fn new(limits: EngineLimits, bound: Bound) -> Engine {
        Engine { limits, bound }
    }

    pub fn session(&self) -> Result<Session> {
        self.session_until(Instant::now() + self.limits.job)
    }

    /// A session that stops at `deadline`: one opened for a part of a
    /// job, which has no more time than the job.
    pub fn session_until(&self, deadline: Instant) -> Result<Session> {
        static NEXT: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
        let n = NEXT.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        let dir = self.limits.spill.join(format!("s{}-{n}-{}", std::process::id(), crate::time::nanos()));
        std::fs::create_dir_all(&dir).map_err(|e| Error::io(&dir, e))?;
        let spill = Spill(dir);
        let config = duckdb::Config::default()
            .enable_autoload_extension(false)?
            .max_memory(&self.limits.memory)?
            .threads(self.limits.threads.max(1) as i64)?
            .with("temp_directory", spill.0.to_string_lossy())?
            .with("autoinstall_known_extensions", "false")?;
        let conn = duckdb::Connection::open_in_memory_with_flags(config)?;
        // External access goes off once the spill directory is set,
        // which it would refuse after; then nothing run from here on can
        // turn any of it back.
        conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true")?;
        let expired = Arc::new(AtomicBool::new(false));
        let (done, waiting) = std::sync::mpsc::channel::<()>();
        let (handle, flag) = (conn.interrupt_handle(), expired.clone());
        let left = deadline.saturating_duration_since(Instant::now());
        let thread = std::thread::spawn(move || {
            if let Err(std::sync::mpsc::RecvTimeoutError::Timeout) = waiting.recv_timeout(left) {
                flag.store(true, Ordering::SeqCst);
                handle.interrupt();
            }
        });
        let watch = Watch { expired, limit: self.limits.job, done: Some(done), thread: Some(thread) };
        Ok(Session { watch, conn, bound: self.bound, deadline, _spill: spill })
    }

    /// The engine's own version, for lineage.
    pub fn version(&self) -> Result<String> {
        let s = self.session()?;
        Ok(s.conn.query_row("SELECT version()", [], |r| r.get::<_, String>(0))?)
    }
}

impl Session {
    /// When this session stops.
    pub fn deadline(&self) -> Instant {
        self.deadline
    }

    fn expired(&self) -> bool {
        self.watch.expired.load(Ordering::SeqCst)
    }

    fn stopped(&self) -> Error {
        Error::Unprocessable(format!("this job ran for more than {} seconds and was stopped", self.watch.limit.as_secs()))
    }

    /// An engine result, as this crate's: an error after the deadline is
    /// the deadline's.
    fn ran<T>(&self, work: impl FnOnce() -> duckdb::Result<T>) -> Result<T> {
        if self.expired() {
            return Err(self.stopped());
        }
        work().map_err(|e| if self.expired() { self.stopped() } else { e.into() })
    }

    /// A table of the given columns, filled with these batches. The
    /// batches' columns are cast to the table's on the way in.
    pub fn load(&self, table: &str, columns: &[(String, ColumnType)], batches: &[RecordBatch]) -> Result<()> {
        let defs: Vec<String> = columns.iter().map(|(n, t)| format!("{} {}", ident(n), t.sql())).collect();
        self.ran(|| {
            self.conn.execute_batch(&format!("CREATE TABLE {} ({})", ident(table), defs.join(", ")))?;
            let mut appender = self.conn.appender(table)?;
            for b in batches {
                if b.num_rows() > 0 {
                    appender.append_record_batch(b.clone())?;
                }
            }
            appender.flush()
        })
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        self.ran(|| self.conn.execute_batch(sql))
    }

    /// A query's rows, as Arrow, refused past the session's [`Bound`].
    pub fn query(&self, sql: &str) -> Result<Batches> {
        self.ran(|| self.collect(sql))?.map_err(Error::Unprocessable)
    }

    /// A query's rows, counted as they come and given up past the bound.
    fn collect(&self, sql: &str) -> std::result::Result<std::result::Result<Batches, String>, duckdb::Error> {
        let mut stmt = self.conn.prepare(sql)?;
        let arrow = stmt.query_arrow([])?;
        let schema: SchemaRef = arrow.get_schema();
        let (mut rows, mut bytes, mut batches) = (0usize, 0u64, Vec::new());
        for b in arrow {
            rows += b.num_rows();
            bytes += b.get_array_memory_size() as u64;
            if rows > self.bound.rows {
                return Ok(Err(format!("it makes more than the {} rows a dataset holds", self.bound.rows)));
            }
            if bytes > self.bound.bytes {
                return Ok(Err(format!("it makes more than the {} bytes a dataset holds", self.bound.bytes)));
            }
            batches.push(b);
        }
        Ok(Ok(Batches { schema, batches }))
    }

    /// A query's rows, interrupted if it runs past `limit`. An engine
    /// error comes back as its own words, for a step to name.
    pub fn query_within(&self, sql: &str, limit: Duration) -> std::result::Result<Batches, String> {
        if self.expired() {
            return Err(self.stopped().to_string());
        }
        let handle = self.conn.interrupt_handle();
        let (done, waiting) = std::sync::mpsc::channel::<()>();
        let watch = std::thread::spawn(move || {
            if waiting.recv_timeout(limit).is_err() {
                handle.interrupt();
                return true;
            }
            false
        });
        let result = self.collect(sql);
        let _ = done.send(());
        let interrupted = watch.join().unwrap_or(false);
        match result {
            Ok(b) => b,
            Err(_) if self.expired() => Err(self.stopped().to_string()),
            Err(_) if interrupted => Err(format!("it ran for more than {} seconds and was stopped", limit.as_secs())),
            Err(e) => Err(e.to_string()),
        }
    }

    /// The one text a query answers with, given one text parameter.
    pub fn text_of(&self, sql: &str, param: &str) -> Result<String> {
        self.ran(|| self.conn.query_row(sql, [param], |r| r.get::<_, String>(0)))
    }

    /// One number a query answers with.
    pub fn count(&self, sql: &str) -> Result<u64> {
        Ok(self.ran(|| self.conn.query_row(sql, [], |r| r.get::<_, i64>(0)))?.max(0) as u64)
    }

    /// The first column of a query, as text.
    pub fn texts(&self, sql: &str) -> Result<Vec<String>> {
        self.ran(|| {
            let mut stmt = self.conn.prepare(sql)?;
            let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
            Ok(rows.filter_map(|r| r.ok().flatten()).collect())
        })
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn engine() -> Engine {
        let spill = std::env::temp_dir().join(format!("zsql-prep-engine-{}", std::process::id()));
        Engine::new(EngineLimits { memory: "256MB".into(), threads: 1, spill, job: JOB_LIMIT }, Bound { rows: 100_000, bytes: 64 << 20 })
    }

    #[test]
    fn a_job_past_its_deadline_is_stopped_and_runs_nothing_after() {
        let e = engine();
        let s = e.session_until(Instant::now() + Duration::from_millis(300)).unwrap();
        let started = Instant::now();
        let err = s.count("SELECT count(*) FROM range(100000000000) a, range(1000) b").err().unwrap();
        assert!(err.to_string().contains("was stopped"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(10));
        assert!(s.execute("SELECT 1").err().unwrap().to_string().contains("was stopped"));
    }

    #[test]
    fn each_session_spills_to_its_own_directory_and_removes_it() {
        let e = engine();
        let (a, b) = (e.session().unwrap(), e.session().unwrap());
        assert_ne!(a._spill.0, b._spill.0);
        assert!(a._spill.0.is_dir());
        let gone = a._spill.0.clone();
        drop(a);
        assert!(!gone.exists());
        drop(b);
    }

    #[test]
    fn an_answer_past_the_bound_is_refused_as_it_comes() {
        let s = engine().session().unwrap();
        let e = s.query("SELECT * FROM range(200000)").err().unwrap();
        assert!(e.to_string().contains("more than the 100000 rows"), "{e}");
        let e = s.query("SELECT repeat('x', 1000000) AS s FROM range(100)").err().unwrap();
        assert!(e.to_string().contains("bytes"), "{e}");
        let e = s.query_within("SELECT * FROM range(200000)", std::time::Duration::from_secs(5)).err().unwrap();
        assert!(e.contains("more than the 100000 rows"), "{e}");
    }

    #[test]
    fn a_session_reads_no_file_and_cannot_be_reopened_to_one() {
        let s = engine().session().unwrap();
        for sql in [
            "SELECT * FROM read_csv('/etc/passwd')",
            "COPY (SELECT 1) TO '/tmp/zsql-prep-escape.csv'",
            "ATTACH '/tmp/zsql-prep-escape.duckdb'",
            "INSTALL httpfs",
            "SET enable_external_access = true",
            "SET memory_limit = '64GB'",
        ] {
            assert!(s.execute(sql).is_err(), "{sql} ran");
        }
        assert!(!std::path::Path::new("/tmp/zsql-prep-escape.csv").exists());
    }

    #[test]
    fn a_query_past_its_time_is_stopped() {
        let s = engine().session().unwrap();
        let started = std::time::Instant::now();
        let e = s
            .query_within("SELECT count(*) FROM range(100000000000) a, range(1000) b", std::time::Duration::from_millis(300))
            .err()
            .unwrap();
        assert!(e.contains("was stopped"), "{e}");
        assert!(started.elapsed() < std::time::Duration::from_secs(10));
        assert_eq!(s.query_within("SELECT 1 AS one", std::time::Duration::from_secs(5)).unwrap().num_rows(), 1);
    }

    #[test]
    fn rows_go_in_and_come_out_as_arrow() {
        let s = engine().session().unwrap();
        s.execute("CREATE TABLE t AS SELECT 1::BIGINT AS a, 'x' AS b").unwrap();
        let rows = s.query("SELECT * FROM t").unwrap();
        assert_eq!(rows.num_rows(), 1);
        s.load("u", &[("a".into(), ColumnType::Integer), ("b".into(), ColumnType::Text)], &rows.batches).unwrap();
        assert_eq!(s.count("SELECT count(*) FROM u").unwrap(), 1);
        assert!(engine().version().unwrap().starts_with('v'));
    }
}
