//! SQLite in-process, and the one adapter whose read-only promise is the
//! file's: the database is opened with the read-only flag, so a write is
//! refused by SQLite itself before the guard in [`read_only`] is reached.

use crate::rows::{Batcher, Capped, RowSink};
use crate::{Access, Connection, Error, Result, Rows, Sink, Source};

pub struct Sqlite {
    db: rusqlite::Connection,
    uid: String,
    timeout: std::time::Duration,
    access: Access,
}

impl Sqlite {
    pub fn open(conn: &Connection) -> Result<Sqlite> {
        let file = conn.file.clone().ok_or_else(|| Error::Missing(conn.uid.clone(), "file"))?;
        // Read-only when statements are checked, so a write is refused
        // by SQLite itself; otherwise as the file allows. Never created:
        // a datasource names a database that is there.
        let mode = match conn.access {
            Access::ReadOnly => rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
            Access::Unchecked | Access::Writable => rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE,
        };
        let flags = mode | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX;
        let db = rusqlite::Connection::open_with_flags(&file, flags)
            .map_err(|e| Error::Connect(conn.uid.clone(), format!("{}: {e}", file.display())))?;
        db.busy_timeout(conn.timeout()).map_err(|e| Error::Connect(conn.uid.clone(), e.to_string()))?;
        Ok(Sqlite { db, uid: conn.uid.clone(), timeout: conn.timeout(), access: conn.access })
    }
}

impl Sqlite {
    /// The statement stepped to its end, each row to `out`, under the
    /// timeout.
    fn read(&mut self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
        self.access.check(sql)?;
        let fail = |e: rusqlite::Error| Error::Query(self.uid.clone(), e.to_string());
        // SQLite has no statement timeout; a progress handler fires every so
        // many VM steps and can call the statement off. The budget is
        // checked against the clock rather than counted, so a slow step and
        // a fast one are charged alike.
        let deadline = std::time::Instant::now() + self.timeout;
        // A handler that could not be installed is a query with no
        // deadline, so that is a refusal and not something to run past.
        self.db.progress_handler(10_000, Some(move || std::time::Instant::now() > deadline)).map_err(fail)?;
        let result = (|| {
            let mut stmt = self.db.prepare(sql).map_err(fail)?;
            let columns: Vec<String> = stmt.column_names().map(|c| c.to_string()).collect();
            let width = columns.len();
            out.columns(&columns)?;
            let mut rows = stmt.query([]).map_err(fail)?;
            while let Some(row) = rows.next().map_err(fail)? {
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
                out.row(cells)?;
            }
            drop(rows);
            if width == 0 {
                out.affected(self.db.changes())?;
            }
            Ok(())
        })();
        // Cleared whatever the query did. A handler left behind would
        // call off the next statement against this one's spent deadline,
        // so failing to clear it is reported when the query itself was
        // fine, and the query's own error wins when it was not.
        let cleared = self.db.progress_handler(0, None::<fn() -> bool>).map_err(fail);
        result.and(cleared)
    }
}

impl Source for Sqlite {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        let mut kept = Capped::new(&self.uid, max_rows);
        self.read(sql, &mut kept)?;
        Ok(kept.finish())
    }

    fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        let mut batcher = Batcher::new(sink);
        self.read(sql, &mut batcher)?;
        batcher.finish()
    }

    fn adapter(&self) -> &str {
        "sqlite"
    }
}
