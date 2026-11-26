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
use zsql_datasource::Batches;

/// What bounds a session.
#[derive(Clone, Debug)]
pub struct EngineLimits {
    /// DuckDB's memory limit, as it writes one: `512MB`, `2GB`.
    pub memory: String,
    pub threads: usize,
    /// Where a session spills what does not fit in memory.
    pub spill: PathBuf,
}

pub struct Engine {
    limits: EngineLimits,
}

/// One job's engine.
pub struct Session {
    conn: duckdb::Connection,
}

impl Engine {
    pub fn new(limits: EngineLimits) -> Engine {
        Engine { limits }
    }

    pub fn session(&self) -> Result<Session> {
        std::fs::create_dir_all(&self.limits.spill).map_err(|e| Error::io(&self.limits.spill, e))?;
        let config = duckdb::Config::default()
            .enable_autoload_extension(false)?
            .max_memory(&self.limits.memory)?
            .threads(self.limits.threads.max(1) as i64)?
            .with("temp_directory", self.limits.spill.to_string_lossy())?
            .with("autoinstall_known_extensions", "false")?;
        let conn = duckdb::Connection::open_in_memory_with_flags(config)?;
        // External access goes off once the spill directory is set,
        // which it would refuse after; then nothing run from here on can
        // turn any of it back.
        conn.execute_batch("SET enable_external_access = false; SET lock_configuration = true")?;
        Ok(Session { conn })
    }

    /// The engine's own version, for lineage.
    pub fn version(&self) -> Result<String> {
        let s = self.session()?;
        Ok(s.conn.query_row("SELECT version()", [], |r| r.get::<_, String>(0))?)
    }
}

impl Session {
    /// A table of the given columns, filled with these batches. The
    /// batches' columns are cast to the table's on the way in.
    pub fn load(&self, table: &str, columns: &[(String, ColumnType)], batches: &[RecordBatch]) -> Result<()> {
        let defs: Vec<String> = columns.iter().map(|(n, t)| format!("{} {}", ident(n), t.sql())).collect();
        self.conn.execute_batch(&format!("CREATE TABLE {} ({})", ident(table), defs.join(", ")))?;
        let mut appender = self.conn.appender(table)?;
        for b in batches {
            if b.num_rows() > 0 {
                appender.append_record_batch(b.clone())?;
            }
        }
        appender.flush()?;
        Ok(())
    }

    pub fn execute(&self, sql: &str) -> Result<()> {
        self.conn.execute_batch(sql)?;
        Ok(())
    }

    /// A query's rows, as Arrow.
    pub fn query(&self, sql: &str) -> Result<Batches> {
        let mut stmt = self.conn.prepare(sql)?;
        let arrow = stmt.query_arrow([])?;
        let schema: SchemaRef = arrow.get_schema();
        let batches: Vec<RecordBatch> = arrow.collect();
        Ok(Batches { schema, batches })
    }

    /// One number a query answers with.
    pub fn count(&self, sql: &str) -> Result<u64> {
        Ok(self.conn.query_row(sql, [], |r| r.get::<_, i64>(0))?.max(0) as u64)
    }

    /// The first column of a query, as text.
    pub fn texts(&self, sql: &str) -> Result<Vec<String>> {
        let mut stmt = self.conn.prepare(sql)?;
        let rows = stmt.query_map([], |r| r.get::<_, Option<String>>(0))?;
        Ok(rows.filter_map(|r| r.ok().flatten()).collect())
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) fn engine() -> Engine {
        let spill = std::env::temp_dir().join(format!("zsql-prep-engine-{}", std::process::id()));
        Engine::new(EngineLimits { memory: "256MB".into(), threads: 1, spill })
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
