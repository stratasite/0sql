//! Apache Druid over its SQL API: one POST to `/druid/v2/sql` with the
//! answer in `arrayLines`, a JSON array a line, read as the lines arrive.
//! The router (8888) or a broker takes it; the coordinator does not.
//!
//! A response is 200 before Druid knows whether the statement will
//! finish, so a status says nothing about a result that breaks partway.
//! `arrayLines` ends a complete result with one blank line, and a body
//! that stops without it is a result that was cut short: refused, never
//! taken as the whole.
//!
//! Each statement carries an id of its own, which is what cancels it
//! when whoever takes the rows has had enough. A capped read asks for
//! one row past the cap through `sqlOuterLimit`, so a result too big is
//! known by its extra row and Druid computes no more than that.
//!
//! The context pins `sqlTimeZone` to UTC, as the dialect assumes, and
//! hands `query_timeout` to Druid as its own `timeout`, so the server
//! stops a statement that has run out of time instead of only this end.

use crate::rows::{Batcher, Capped, RowSink};
use crate::{http, Connection, Deadline, Error, Result, Rows, Secret, Sink, Source};
use base64::Engine;
use serde_json::{json, Value};
use std::sync::atomic::{AtomicU64, Ordering};

pub struct Druid {
    agent: ureq::Agent,
    base: String,
    /// The `Authorization` header, when the cluster has an authenticator.
    authorization: Option<String>,
    conn: Connection,
    warnings: Vec<String>,
}

/// The SQL API's path, under the router or a broker.
const SQL: &str = "/druid/v2/sql";

/// The largest `sqlOuterLimit` worth sending: a cap past it is no cap.
const OUTER_LIMIT_CEILING: usize = 1 << 53;

impl Druid {
    pub fn open(conn: &Connection, password: Option<Secret>, token: Option<Secret>) -> Result<Druid> {
        let (base, note) = http::base_url(conn, 8888, "druid")?;
        let authorization = authorization(conn, password.as_ref(), token.as_ref())?;
        Ok(Druid {
            agent: http::agent(conn)?,
            base: base.trim_end_matches('/').to_string(),
            authorization,
            conn: conn.clone(),
            warnings: note.into_iter().collect(),
        })
    }

    fn headers<B>(&self, mut req: ureq::RequestBuilder<B>) -> ureq::RequestBuilder<B> {
        req = req.header("User-Agent", concat!("zsql/", env!("CARGO_PKG_VERSION")));
        if let Some(a) = &self.authorization {
            req = req.header("Authorization", a);
        }
        req
    }

    /// Asks the cluster to stop a statement: best effort, since the read
    /// is already ending with its own error.
    fn cancel(&self, id: &str) {
        let _ = self.headers(self.agent.delete(&format!("{}{SQL}/{}", self.base, http::urlencode(id)))).call();
    }

    /// The statement answered a line a row, each row to `out` as it is
    /// read: the first line names the columns, a blank line ends it.
    fn read(&self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
        use std::io::BufRead;
        self.conn.access.check(sql)?;
        let uid = &self.conn.uid;
        let id = query_id();
        let body = request(sql, &id, self.conn.timeout(), out.limit()).to_string();
        let response = self
            .headers(self.agent.post(&format!("{}{SQL}", self.base)))
            .header("Content-Type", "application/json")
            .send(body.as_bytes())
            .map_err(|e| Error::Query(uid.clone(), crate::chain(&e)))?;
        if !response.status().is_success() {
            let status = response.status().as_u16();
            let text = response.into_body().read_to_string().unwrap_or_default();
            return Err(Error::Query(uid.clone(), refusal(status, &text)));
        }
        let reader = std::io::BufReader::new(response.into_body().into_reader());
        let deadline = Deadline::of(&self.conn);
        let mut named = false;
        let mut ended = false;
        let stop = |e: Error| {
            self.cancel(&id);
            e
        };
        for (at, line) in reader.lines().enumerate() {
            if at % 1024 == 1023 && deadline.passed() {
                return Err(stop(deadline.arriving()));
            }
            let line = line.map_err(|e| stop(Error::Query(uid.clone(), format!("the result broke off: {e}"))))?;
            if line.trim().is_empty() {
                ended = true;
                continue;
            }
            // Anything after the blank line means it was not the end.
            ended = false;
            let cells: Vec<Value> =
                serde_json::from_str(&line).map_err(|_| stop(Error::Query(uid.clone(), line.trim().chars().take(400).collect())))?;
            if named {
                out.row(cells.into_iter().map(http::cell).collect()).map_err(stop)?;
            } else {
                out.columns(&cells.into_iter().map(|c| http::cell(c).unwrap_or_default()).collect::<Vec<_>>()).map_err(stop)?;
                named = true;
            }
        }
        if !named || !ended {
            return Err(Error::Query(uid.clone(), "the result ended without its closing blank line, so it was cut short".into()));
        }
        Ok(())
    }
}

/// The body of one statement. `limit` is the cap of a read that has one:
/// Druid is asked for one row past it, which is how a result too big is
/// told from one exactly at the cap.
pub(crate) fn request(sql: &str, id: &str, timeout: std::time::Duration, limit: Option<usize>) -> Value {
    let mut context = json!({
        "sqlQueryId": id,
        "sqlTimeZone": "Etc/UTC",
        "timeout": timeout.as_millis() as u64,
    });
    if let Some(n) = limit.filter(|n| *n < OUTER_LIMIT_CEILING) {
        context["sqlOuterLimit"] = json!(n + 1);
    }
    json!({ "query": sql, "resultFormat": "arrayLines", "header": true, "context": context })
}

/// The header a cluster with an authenticator reads: a bearer for an
/// access token (pac4j, or a JWT authenticator), Basic for a username
/// and password. A cluster without one is sent neither.
pub(crate) fn authorization(conn: &Connection, password: Option<&Secret>, token: Option<&Secret>) -> Result<Option<String>> {
    if let Some(t) = token {
        return Ok(Some(format!("Bearer {}", t.expose())));
    }
    match (&conn.username, password) {
        (Some(user), Some(p)) => {
            let pair = base64::engine::general_purpose::STANDARD.encode(format!("{user}:{}", p.expose()));
            Ok(Some(format!("Basic {pair}")))
        }
        (None, Some(_)) => Err(Error::Missing(conn.uid.clone(), "username to go with its password")),
        (_, None) => Ok(None),
    }
}

/// Why Druid refused, from its error body: `errorMessage` with the
/// category or class it names, else the body itself.
pub(crate) fn refusal(status: u16, text: &str) -> String {
    let v: Value = serde_json::from_str(text).unwrap_or_default();
    let s = |k: &str| v.get(k).and_then(Value::as_str).filter(|s| !s.trim().is_empty());
    // A syntax error goes on to list every token the parser would have
    // taken, some sixty lines of them; the line before says what is wrong.
    let message = s("errorMessage").or_else(|| s("error")).unwrap_or(text);
    let message: String = message.split("\nWas expecting").next().unwrap_or_default().trim().chars().take(400).collect();
    match s("category").or_else(|| s("errorCode")).or_else(|| s("errorClass")) {
        Some(kind) if kind != message => format!("{status}: {message} ({kind})"),
        _ => format!("{status}: {message}"),
    }
}

/// An id no other statement from here has: what a cancel names.
fn query_id() -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    format!("zsql-{}-{nanos:x}-{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed))
}

impl Source for Druid {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        let mut kept = Capped::new(&self.conn.uid, max_rows);
        self.read(sql, &mut kept)?;
        Ok(kept.finish())
    }

    fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        let mut batcher = Batcher::new(sink);
        self.read(sql, &mut batcher)?;
        batcher.finish()
    }

    fn adapter(&self) -> &str {
        "druid"
    }

    fn transport(&self) -> &str {
        "rest"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
