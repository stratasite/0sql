//! Trino over its client protocol, and Athena when it is reached the
//! same way: POST a statement to `/v1/statement`, then follow `nextUri`
//! until there is none, collecting rows as they come. The catalog and
//! schema go in headers, so a statement can say `information_schema.tables`
//! and mean this catalog's.

use crate::{http, read_only, Connection, Result, Rows, Secret, Source, SourceError};
use serde_json::Value;

pub struct Trino {
    agent: ureq::Agent,
    base: String,
    user: String,
    password: Option<Secret>,
    catalog: Option<String>,
    schema: Option<String>,
    conn: Connection,
}

impl Trino {
    pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<Trino> {
        Ok(Trino {
            agent: http::agent(conn),
            base: http::base_url(conn, 8080)?,
            user: conn.username.clone().unwrap_or_else(|| "zsql".into()),
            password: secret,
            catalog: conn.database.clone(),
            schema: conn.schema.clone(),
            conn: conn.clone(),
        })
    }

    fn headers<B>(&self, mut req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        req = req.header("X-Trino-User", &self.user).header("X-Trino-Source", "zsql");
        // The run-time limit is a session property the coordinator enforces.
        req = req.header("X-Trino-Session", &format!("query_max_run_time={}s", self.conn.timeout().as_secs()));
        if let Some(c) = &self.catalog {
            req = req.header("X-Trino-Catalog", c);
        }
        if let Some(s) = &self.schema {
            req = req.header("X-Trino-Schema", s);
        }
        if let Some(p) = &self.password {
            // Basic over HTTPS; Trino refuses it over plain HTTP, which is right.
            let token = base64(format!("{}:{}", self.user, p.expose()).as_bytes());
            req = req.header("Authorization", &format!("Basic {token}"));
        }
        req
    }

    fn page(&self, body: String) -> Result<Value> {
        let v: Value =
            serde_json::from_str(&body).map_err(|e| SourceError::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))?;
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("query failed");
            let name = err.get("errorName").and_then(Value::as_str).unwrap_or("");
            return Err(SourceError::Query(self.conn.uid.clone(), format!("{name} {msg}").trim().to_string()));
        }
        Ok(v)
    }
}

impl Source for Trino {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        let fail = |e: ureq::Error| SourceError::Query(self.conn.uid.clone(), e.to_string());
        let first = self.headers(self.agent.post(&format!("{}/v1/statement", self.base))).send(sql.as_bytes()).map_err(fail)?;
        let mut v = self.page(http::text(&self.conn, first)?)?;
        let mut columns: Vec<String> = Vec::new();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        loop {
            if columns.is_empty() {
                if let Some(cols) = v.get("columns").and_then(Value::as_array) {
                    columns = cols.iter().filter_map(|c| c.get("name").and_then(Value::as_str)).map(str::to_string).collect();
                }
            }
            if let Some(data) = v.get("data").and_then(Value::as_array) {
                for r in data {
                    if rows.len() == max_rows {
                        // Tell the coordinator to stop, then refuse.
                        if let Some(next) = v.get("nextUri").and_then(Value::as_str) {
                            let _ = self.headers(self.agent.delete(next)).call();
                        }
                        return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
                    }
                    rows.push(r.as_array().map(|cells| cells.iter().cloned().map(cell).collect()).unwrap_or_default());
                }
            }
            let Some(next) = v.get("nextUri").and_then(Value::as_str).map(str::to_string) else { break };
            let r = self.headers(self.agent.get(&next)).call().map_err(fail)?;
            v = self.page(http::text(&self.conn, r)?)?;
        }
        Ok(Rows { columns, rows })
    }

    fn adapter(&self) -> &str {
        "trino"
    }
}

fn cell(v: Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}

/// Standard base64, for one header; not worth a crate.
fn base64(bytes: &[u8]) -> String {
    const T: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";
    let mut out = String::with_capacity(bytes.len().div_ceil(3) * 4);
    for chunk in bytes.chunks(3) {
        let n = chunk.iter().fold(0u32, |acc, b| (acc << 8) | u32::from(*b)) << (8 * (3 - chunk.len()));
        for i in 0..4 {
            out.push(if i <= chunk.len() { T[((n >> (18 - 6 * i)) & 63) as usize] as char } else { '=' });
        }
    }
    out
}
