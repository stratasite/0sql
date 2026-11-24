//! Trino over its client protocol: POST a statement to `/v1/statement`,
//! then follow `nextUri` until there is none, collecting rows as they
//! come. The catalog and schema go in headers, so a statement can say
//! `information_schema.tables` and mean this catalog's. Athena has its
//! own transport; it is not reached this way.

use crate::rows::{Batcher, Capped, RowSink};
use crate::{http, Connection, Deadline, Error, Result, Rows, Secret, Sink, Source};
use base64::Engine;
use serde_json::Value;

pub struct Trino {
    agent: ureq::Agent,
    base: String,
    user: String,
    password: Option<Secret>,
    /// An OAuth access token, sent as a bearer in place of the password.
    token: Option<Secret>,
    catalog: Option<String>,
    schema: Option<String>,
    conn: Connection,
    warnings: Vec<String>,
}

impl Trino {
    pub fn open(conn: &Connection, secret: Option<Secret>, token: Option<Secret>) -> Result<Trino> {
        let (base, note) = http::base_url(conn, 8080, "trino")?;
        Ok(Trino {
            agent: http::agent(conn)?,
            base,
            user: conn.username.clone().unwrap_or_else(|| "zsql".into()),
            password: secret,
            token,
            catalog: conn.database.clone(),
            schema: conn.schema.clone(),
            conn: conn.clone(),
            warnings: note.into_iter().collect(),
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
        if let Some(t) = &self.token {
            // The coordinator's JWT or OAuth 2 authenticator reads this.
            req = req.header("Authorization", &format!("Bearer {}", t.expose()));
        } else if let Some(p) = &self.password {
            // Basic over HTTPS; Trino refuses it over plain HTTP, which is right.
            let token = base64::engine::general_purpose::STANDARD.encode(format!("{}:{}", self.user, p.expose()));
            req = req.header("Authorization", &format!("Basic {token}"));
        }
        req
    }

    fn page(&self, body: String) -> Result<Value> {
        let v: Value = serde_json::from_str(&body).map_err(|e| Error::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))?;
        if let Some(err) = v.get("error") {
            let msg = err.get("message").and_then(Value::as_str).unwrap_or("query failed");
            let name = err.get("errorName").and_then(Value::as_str).unwrap_or("");
            return Err(Error::Query(self.conn.uid.clone(), format!("{name} {msg}").trim().to_string()));
        }
        Ok(v)
    }
}

impl Trino {
    /// The statement followed page by page, each row to `out` as its
    /// page arrives.
    fn read(&mut self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
        self.conn.access.check(sql)?;
        let fail = |e: ureq::Error| Error::Query(self.conn.uid.clone(), crate::chain(&e));
        let first = self.headers(self.agent.post(&format!("{}/v1/statement", self.base))).send(sql.as_bytes()).map_err(fail)?;
        let mut v = self.page(http::text(&self.conn, first)?)?;
        let deadline = Deadline::of(&self.conn);
        let mut named = false;
        loop {
            if !named {
                if let Some(cols) = v.get("columns").and_then(Value::as_array) {
                    let columns: Vec<String> =
                        cols.iter().filter_map(|c| c.get("name").and_then(Value::as_str)).map(str::to_string).collect();
                    out.columns(&columns)?;
                    named = true;
                }
            }
            // What a statement that returns no rows changed, said once
            // the coordinator knows.
            if let Some(changed) = v.get("updateCount").and_then(Value::as_u64) {
                out.affected(changed)?;
            }
            if let Some(data) = v.get("data").and_then(Value::as_array) {
                for r in data {
                    let row = r.as_array().map(|cells| cells.iter().cloned().map(http::cell).collect()).unwrap_or_default();
                    if let Err(e) = out.row(row) {
                        // Whoever takes the rows has had enough, a cap
                        // or a full disk: tell the coordinator to stop.
                        if let Some(next) = v.get("nextUri").and_then(Value::as_str) {
                            let _ = self.headers(self.agent.delete(next)).call();
                        }
                        return Err(e);
                    }
                }
            }
            let Some(next) = v.get("nextUri").and_then(Value::as_str).map(str::to_string) else { break };
            if deadline.passed() {
                let _ = self.headers(self.agent.delete(&next)).call();
                return Err(deadline.arriving());
            }
            // The protocol says a 503 on nextUri is "try again", not a refusal.
            let mut tries = 0;
            let r = loop {
                let r = self.headers(self.agent.get(&next)).call().map_err(fail)?;
                if r.status().as_u16() == 503 && tries < 10 {
                    tries += 1;
                    std::thread::sleep(std::time::Duration::from_millis(100 * tries));
                    continue;
                }
                break r;
            };
            v = self.page(http::text(&self.conn, r)?)?;
        }
        Ok(())
    }
}

impl Source for Trino {
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
        "trino"
    }

    fn transport(&self) -> &str {
        "rest"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
