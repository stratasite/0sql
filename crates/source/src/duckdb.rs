//! DuckDB in this process, linked and bundled: a file opened read-only,
//! a statement run through its Arrow interface, every cell rendered to
//! text by Arrow's own formatter so a date, a decimal or a struct prints
//! as DuckDB would print it. The one adapter that costs a build: the
//! engine is C++ and compiles in with the crate, which is the decision
//! `docs/introspection.md` records.
//!
//! Read-only is DuckDB's, not only the guard's: the file is opened in
//! `READ_ONLY` mode, so a write fails inside the engine. A statement
//! past the timeout is interrupted through the engine's own handle.

use crate::{read_only, Connection, Result, Rows, Source, SourceError};
use duckdb::arrow::util::display::{ArrayFormatter, FormatOptions};
use std::path::PathBuf;

// Why this crate names a symbol it never calls.
//
// `libduckdb-sys` takes the one object that registers DuckDB's linked
// extensions out of the DuckDB library and links it whole-archive,
// because nothing inside the library references it and the linker
// would drop it, leaving the extensions unregistered. That object
// calls `duckdb_register_static_extensions`, which is defined back in
// the library; cargo bundles the library into an rlib, and rustc emits
// that rlib before the extracted archive.
//
// GNU ld reads the line once. At the rlib nothing wants the symbol
// yet, so the member that defines it is skipped, and the reference
// arrives with nothing left to answer it. LLD keeps every archive's
// symbols and fetches a member whenever a later reference turns up, so
// it does not care about the order: rustc links this target's glibc
// builds with its bundled LLD, and macOS with Apple's linker, which
// is why only musl ever failed.
//
// A reference from this crate is compiled into an object file, and a
// crate's own objects precede every rlib on the line. That makes the
// member load whatever the linker. It replaces a `-Wl,-u` flag in
// `.cargo/config.toml`, which an exported `RUSTFLAGS` would have
// silently discarded and which a build from outside the checkout
// never saw at all.
//
// Found 2026-10-25, when the first release to bundle DuckDB failed to
// link on both Linux targets. Drop it when `libduckdb-sys` orders the
// two itself.
extern "C" {
    fn duckdb_register_static_extensions() -> i32;
}

// Never called: the reference is the point, and `used` keeps it from
// being optimised away before the linker sees it.
#[used]
static KEEP_STATIC_EXTENSIONS: unsafe extern "C" fn() -> i32 = duckdb_register_static_extensions;

pub struct DuckDb {
    conn: duckdb::Connection,
    uid: String,
    timeout: std::time::Duration,
}

impl DuckDb {
    pub fn open(conn: &Connection) -> Result<DuckDb> {
        let file: PathBuf = conn.file.clone().ok_or_else(|| SourceError::Missing(conn.uid.clone(), "file"))?;
        if !file.exists() {
            return Err(SourceError::Connect(conn.uid.clone(), format!("{} does not exist", file.display())));
        }
        let config = duckdb::Config::default()
            .access_mode(duckdb::AccessMode::ReadOnly)
            .map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        let db = duckdb::Connection::open_with_flags(&file, config).map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        Ok(DuckDb { conn: db, uid: conn.uid.clone(), timeout: conn.timeout() })
    }
}

impl Source for DuckDb {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        let uid = self.uid.clone();
        let fail = |e: duckdb::Error| SourceError::Query(uid.clone(), e.to_string());
        // The timeout: a watcher interrupts the engine, which then answers
        // the statement with an error naming the interrupt.
        let handle = self.conn.interrupt_handle();
        let (stop_tx, stop_rx) = std::sync::mpsc::channel::<()>();
        let timeout = self.timeout;
        let watcher = std::thread::spawn(move || {
            if stop_rx.recv_timeout(timeout).is_err() {
                handle.interrupt();
            }
        });
        let outcome = (|| {
            let mut stmt = self.conn.prepare(sql).map_err(fail)?;
            let batches = stmt.query_arrow([]).map_err(fail)?;
            // The names come from the executed statement's schema: before
            // execution the statement has none to give.
            let columns: Vec<String> = batches.get_schema().fields().iter().map(|f| f.name().clone()).collect();
            let options = FormatOptions::default();
            let mut rows: Vec<Vec<Option<String>>> = Vec::new();
            for batch in batches {
                let formatters: Vec<ArrayFormatter> = batch
                    .columns()
                    .iter()
                    .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
                    .collect::<std::result::Result<_, _>>()
                    .map_err(|e| SourceError::Query(uid.clone(), e.to_string()))?;
                for i in 0..batch.num_rows() {
                    if rows.len() == max_rows {
                        return Err(SourceError::TooManyRows(uid.clone(), max_rows));
                    }
                    rows.push(
                        batch
                            .columns()
                            .iter()
                            .zip(&formatters)
                            .map(|(c, f)| if c.is_null(i) { None } else { Some(f.value(i).to_string()) })
                            .collect(),
                    );
                }
            }
            Ok(Rows { columns, rows })
        })();
        let _ = stop_tx.send(());
        let _ = watcher.join();
        match outcome {
            Err(SourceError::Query(u, why)) if why.to_lowercase().contains("interrupt") => {
                Err(SourceError::Query(u, format!("no answer within {}s; interrupted", timeout.as_secs())))
            }
            other => other,
        }
    }

    fn adapter(&self) -> &str {
        "duckdb"
    }
}
