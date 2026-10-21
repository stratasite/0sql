//! SQLite in-process, and the one adapter whose read-only promise is the
//! file's: the database is opened with the read-only flag, so a write is
//! refused by SQLite itself before the guard in [`read_only`] is reached.

use crate::{read_only, Connection, Result, Rows, Source, SourceError};

pub struct Sqlite {
    db: rusqlite::Connection,
    uid: String,
    timeout: std::time::Duration,
}

impl Sqlite {
    pub fn open(conn: &Connection) -> Result<Sqlite> {
        let file = conn.file.clone().ok_or_else(|| SourceError::Missing(conn.uid.clone(), "file"))?;
        let flags = rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let db = rusqlite::Connection::open_with_flags(&file, flags)
            .map_err(|e| SourceError::Connect(conn.uid.clone(), format!("{}: {e}", file.display())))?;
        db.busy_timeout(conn.timeout()).map_err(|e| SourceError::Connect(conn.uid.clone(), e.to_string()))?;
        Ok(Sqlite { db, uid: conn.uid.clone(), timeout: conn.timeout() })
    }
}

impl Source for Sqlite {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        let fail = |e: rusqlite::Error| SourceError::Query(self.uid.clone(), e.to_string());
        // SQLite has no statement timeout; a progress handler fires every so
        // many VM steps and can call the statement off. The budget is
        // checked against the clock rather than counted, so a slow step and
        // a fast one are charged alike.
        let deadline = std::time::Instant::now() + self.timeout;
        self.db.progress_handler(10_000, Some(move || std::time::Instant::now() > deadline));
        let result = (|| {
            let mut stmt = self.db.prepare(sql).map_err(fail)?;
            let columns: Vec<String> = stmt.column_names().iter().map(|c| c.to_string()).collect();
            let width = columns.len();
            let mut out = Vec::new();
            let mut rows = stmt.query([]).map_err(fail)?;
            while let Some(row) = rows.next().map_err(fail)? {
                if out.len() == max_rows {
                    return Err(SourceError::TooManyRows(self.uid.clone(), max_rows));
                }
                let mut cells = Vec::with_capacity(width);
                for i in 0..width {
                    cells.push(match row.get_ref(i).map_err(fail)? {
                        rusqlite::types::ValueRef::Null => None,
                        rusqlite::types::ValueRef::Integer(n) => Some(n.to_string()),
                        rusqlite::types::ValueRef::Real(f) => Some(f.to_string()),
                        rusqlite::types::ValueRef::Text(t) => Some(String::from_utf8_lossy(t).into_owned()),
                        rusqlite::types::ValueRef::Blob(b) => Some(format!("<{} bytes>", b.len())),
                    });
                }
                out.push(cells);
            }
            Ok(Rows { columns, rows: out })
        })();
        self.db.progress_handler(0, None::<fn() -> bool>);
        result
    }

    fn adapter(&self) -> &str {
        "sqlite"
    }
}
