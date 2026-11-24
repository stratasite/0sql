//! ClickHouse over its HTTP interface, and the first of the HTTP adapters:
//! a statement POSTed with `FORMAT JSONCompact` and settings in the query
//! string, rows back as JSON arrays with their column names and types.
//!
//! Settings in the query string let the server enforce what this crate
//! would otherwise only promise: `readonly=2` refuses a write, and
//! `max_result_rows` with `result_overflow_mode=throw` refuses a result
//! past the cap rather than trimming it. But `readonly=1`, the strictest
//! mode and the one a locked-down user is given, forbids changing *any*
//! setting in a request — the cap included, and `readonly` itself. So a
//! connection asks once what mode its session is in and sends only what
//! that mode permits: under 1 the server already bans writes and the cap
//! is checked here; under 2 the caps go up; under 0 so does `readonly=2`.

use crate::rows::{Batcher, RowSink};
use crate::{http, read_only, Connection, Deadline, Error, Result, Rows, Secret, Sink, Source};

pub struct ClickHouse {
    agent: ureq::Agent,
    base: String,
    user: String,
    password: Option<Secret>,
    database: Option<String>,
    conn: Connection,
    /// The session's `readonly` setting, read on open: which settings a
    /// request may carry depends on it.
    readonly: u8,
    warnings: Vec<String>,
}

impl ClickHouse {
    pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<ClickHouse> {
        let (base, note) = http::base_url(conn, 8123, "clickhouse")?;
        let mut ch = ClickHouse {
            agent: http::agent(conn)?,
            base,
            user: conn.username.clone().unwrap_or_else(|| "default".into()),
            password: secret,
            database: conn.database.clone(),
            conn: conn.clone(),
            readonly: 0,
            warnings: note.into_iter().collect(),
        };
        // One plain request, no settings, to learn which settings are allowed.
        let rows = ch.send("SELECT value FROM system.settings WHERE name = 'readonly'", "", 1)?;
        ch.readonly = rows.rows.first().and_then(|r| rows.get(r, "value")).and_then(|v| v.parse().ok()).unwrap_or(0);
        Ok(ch)
    }

    /// The settings this session may be sent, as a query-string suffix.
    fn settings(&self, max_rows: Option<usize>) -> String {
        let rows = max_rows.map(|n| format!("&max_result_rows={n}&result_overflow_mode=throw")).unwrap_or_default();
        let caps = format!("{rows}&max_execution_time={}", self.conn.timeout().as_secs());
        match self.readonly {
            1 => String::new(),
            2 => caps,
            _ => format!("&readonly=2{caps}"),
        }
    }

    /// The statement sent, to be answered in `format`.
    fn post(&self, sql: &str, format: &str, settings: &str) -> Result<ureq::http::Response<ureq::Body>> {
        let mut url = format!("{}/?default_format={format}{settings}", self.base);
        if let Some(db) = &self.database {
            url.push_str(&format!("&database={}", http::urlencode(db)));
        }
        let mut req = self.agent.post(&url).header("X-ClickHouse-User", &self.user).header("content-type", "text/plain");
        if let Some(p) = &self.password {
            req = req.header("X-ClickHouse-Key", p.expose());
        }
        req.send(sql.as_bytes()).map_err(|e| Error::Query(self.conn.uid.clone(), crate::chain(&e)))
    }

    /// The statement answered a line a row, read as the lines arrive:
    /// the first is the column names, each one after it a row. Nothing
    /// is held but the line being read.
    fn lines(&self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
        use std::io::BufRead;
        let uid = &self.conn.uid;
        let response = self.post(sql, "JSONCompactEachRowWithNames", &self.settings(None))?;
        if !response.status().is_success() {
            // A refusal's body is where it says why.
            return http::text(&self.conn, response).map(|_| ());
        }
        let reader = std::io::BufReader::new(response.into_body().into_reader());
        let deadline = Deadline::of(&self.conn);
        let mut named = false;
        for (at, line) in reader.lines().enumerate() {
            // The server times the statement; this times the arriving,
            // looked at every so many rows.
            if at % 1024 == 1023 {
                deadline.check()?;
            }
            let line = line.map_err(|e| Error::Query(uid.clone(), e.to_string()))?;
            if line.trim().is_empty() {
                continue;
            }
            // A statement that fails after its first rows were sent ends
            // the body with the exception in place of a row.
            let cells: Vec<serde_json::Value> =
                serde_json::from_str(&line).map_err(|_| Error::Query(uid.clone(), line.trim().chars().take(400).collect()))?;
            if named {
                out.row(cells.into_iter().map(http::cell).collect())?;
            } else {
                out.columns(&cells.into_iter().map(|c| http::cell(c).unwrap_or_default()).collect::<Vec<_>>())?;
                named = true;
            }
        }
        Ok(())
    }

    fn send(&self, sql: &str, settings: &str, max_rows: usize) -> Result<Rows> {
        let response = self.post(sql, "JSONCompact", settings)?;
        let body = http::text(&self.conn, response)?;
        let compact: Compact =
            serde_json::from_str(&body).map_err(|e| Error::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))?;
        // Under readonly=1 the server was sent no cap, so this is the cap;
        // otherwise the server threw first and this is belt and braces.
        if compact.data.len() > max_rows {
            return Err(Error::TooManyRows(self.conn.uid.clone(), max_rows));
        }
        let columns = compact.meta.into_iter().map(|m| m.name).collect();
        let rows = compact.data.into_iter().map(|row| row.into_iter().map(http::cell).collect()).collect();
        Ok(Rows { columns, rows })
    }
}

/// The shape `FORMAT JSONCompact` returns: `meta` names and types the
/// columns, `data` is one array per row.
#[derive(serde::Deserialize)]
struct Compact {
    meta: Vec<Meta>,
    data: Vec<Vec<serde_json::Value>>,
}

#[derive(serde::Deserialize)]
struct Meta {
    name: String,
}

impl Source for ClickHouse {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        self.send(sql, &self.settings(Some(max_rows)), max_rows)
    }

    fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        read_only(sql)?;
        let mut batcher = Batcher::new(sink);
        self.lines(sql, &mut batcher)?;
        batcher.finish()
    }

    fn adapter(&self) -> &str {
        "clickhouse"
    }

    fn transport(&self) -> &str {
        "rest"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
