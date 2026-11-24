//! Databricks over its Statement Execution API: a statement POSTed to
//! `/api/2.0/sql/statements` for one SQL warehouse, waited on for up to
//! thirty seconds in the request itself and polled after, the result
//! fetched chunk by chunk as JSON arrays of strings.
//!
//! Two ways in. A personal access token is sent as it is. A service
//! principal's client id and secret are exchanged for an access token at
//! the workspace's own `/oidc/v1/token` with the client-credentials
//! grant, and the token is minted again before it expires. The
//! authorization-code flow a web app runs is not here.
//!
//! `row_limit` rides on every statement one past the cap, so a result
//! over it costs the warehouse one extra row and this side one request:
//! the manifest says the total and whether it was cut.

use crate::rows::{Batcher, Capped, RowSink};
use crate::{http, Connection, Deadline, Error, Result, Rows, Secret, Secrets, Sink, Source};
use base64::Engine;
use std::time::{Duration, Instant};

const STATEMENTS: &str = "/api/2.0/sql/statements";
const USER_AGENT: &str = concat!("zsql/", env!("CARGO_PKG_VERSION"));
/// The longest the first request itself may wait before the API hands
/// back a pending statement: the API allows `0s` or five to fifty seconds.
const MAX_WAIT_SECS: u64 = 50;

pub struct Databricks {
    agent: ureq::Agent,
    base: String,
    conn: Connection,
    auth: Auth,
    /// A minted token and when it stops being good.
    token: Option<(String, Instant)>,
    warehouse: String,
    warnings: Vec<String>,
}

enum Auth {
    Token(Secret),
    ServicePrincipal { client_id: String, secret: Secret },
}

impl Databricks {
    pub fn open(conn: &Connection, secrets: Secrets) -> Result<Databricks> {
        let mut missing: Vec<String> = Vec::new();
        let base = match (&conn.url, &conn.host) {
            (Some(u), _) => u.clone(),
            (None, Some(h)) => format!("https://{}", h.trim().trim_end_matches('/')),
            (None, None) => {
                missing.push("host (the workspace, as in adb-123.4.azuredatabricks.net) or url".into());
                String::new()
            }
        };
        if conn.warehouse.is_none() {
            missing.push("warehouse (the SQL warehouse id, from the workspace's SQL Warehouses page)".into());
        }
        let env = |field: &str| crate::secret_env_var(&conn.uid, field);
        // An OAuth access token from upstream and a personal access token
        // are the same thing on the wire: a bearer.
        let auth = if let Some(t) = secrets.oauth_token.or(secrets.personal_access_token) {
            Some(Auth::Token(t))
        } else {
            let client_id = conn.client_id.clone().or_else(|| secrets.oauth_client_id.map(|s| s.expose().to_string()));
            match (client_id, secrets.oauth_client_secret) {
                (Some(client_id), Some(secret)) => Some(Auth::ServicePrincipal { client_id, secret }),
                (Some(_), None) => {
                    missing.push(format!("oauth_client_secret in .zsql under {} or in {}", conn.uid, env("oauth_client_secret")));
                    None
                }
                (None, _) => {
                    missing.push(format!(
                        "credentials: a personal access token as personal_access_token in .zsql under {uid} or in {pat}, or a service principal as client_id in datasources.yml with oauth_client_secret in .zsql or in {sec}",
                        uid = conn.uid,
                        pat = env("personal_access_token"),
                        sec = env("oauth_client_secret"),
                    ));
                    None
                }
            }
        };
        if !missing.is_empty() {
            return Err(Error::NoCredentials(conn.uid.clone(), missing.join("; ")));
        }
        let note = http::plain(conn, &base, "databricks")?;
        Ok(Databricks {
            agent: http::agent(conn)?,
            base: base.trim_end_matches('/').to_string(),
            conn: conn.clone(),
            auth: auth.expect("checked"),
            token: None,
            warehouse: conn.warehouse.clone().expect("checked"),
            warnings: note.into_iter().collect(),
        })
    }

    /// The bearer: the token as given, or one minted from the service
    /// principal and kept until half a minute before it expires.
    fn bearer(&mut self) -> Result<String> {
        match &self.auth {
            Auth::Token(t) => Ok(t.expose().to_string()),
            Auth::ServicePrincipal { client_id, secret } => {
                if let Some((t, until)) = &self.token {
                    if Instant::now() + Duration::from_secs(30) < *until {
                        return Ok(t.clone());
                    }
                }
                let basic = base64::engine::general_purpose::STANDARD.encode(format!("{client_id}:{}", secret.expose()));
                let body = http::form(&[("grant_type", "client_credentials"), ("scope", "all-apis")]);
                let url = format!("{}/oidc/v1/token", self.base);
                let (token, until) = http::token(&self.conn, &self.agent, &url, Some(&format!("Basic {basic}")), &body)?;
                self.token = Some((token.clone(), until));
                Ok(token)
            }
        }
    }

    fn request(&mut self, method: http::Method, url: &str, body: Option<String>) -> Result<(u16, String)> {
        let bearer = format!("Bearer {}", self.bearer()?);
        let headers = [
            ("Authorization", bearer.as_str()),
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("User-Agent", USER_AGENT),
        ];
        http::call(&self.conn, &self.agent, method, url, &headers, body.as_deref().map(str::as_bytes))
    }

    /// The rows behind one external link. The link carries its own
    /// credentials, so it is fetched with none of ours: the workspace's
    /// token is not for wherever the link points.
    fn linked(&self, link: &str) -> Result<Vec<Vec<serde_json::Value>>> {
        // A link without a host is this workspace's.
        let url = if link.starts_with('/') { format!("{}{link}", self.base) } else { link.to_string() };
        let (status, text) = http::call(&self.conn, &self.agent, http::Method::Get, &url, &[("User-Agent", USER_AGENT)], None)?;
        if status != 200 {
            let why: String = text.trim().chars().take(200).collect();
            return Err(Error::Query(self.conn.uid.clone(), format!("fetching a chunk of the result: {status}: {why}")));
        }
        serde_json::from_str(&text).map_err(|e| Error::Query(self.conn.uid.clone(), format!("unexpected chunk of the result: {e}")))
    }

    fn refused(&self, status: u16, text: &str) -> Error {
        Error::Query(self.conn.uid.clone(), format!("{status}: {}", http::reason(text)))
    }

    fn parse(&self, text: &str) -> Result<Answer> {
        serde_json::from_str(text).map_err(|e| Error::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))
    }

    fn send(&mut self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
        let arriving = Deadline::of(&self.conn);
        // The request's own wait: the statement timeout, within what the
        // API allows, and under the HTTP call's own limit.
        let wait = self.conn.timeout().as_secs().min(MAX_WAIT_SECS);
        let wait = if wait < 5 { "0s".to_string() } else { format!("{wait}s") };
        let mut body = serde_json::json!({
            "statement": sql,
            "warehouse_id": self.warehouse,
            "wait_timeout": wait,
            "on_wait_timeout": "CONTINUE",
            "format": "JSON_ARRAY",
            // A capped read is small and comes inline. A streamed one
            // may be any size, which inline cannot carry: Databricks
            // aborts a statement whose inline result passes 25 MiB. So
            // it is asked for as links to the result's chunks.
            "disposition": if out.limit().is_some() { "INLINE" } else { "EXTERNAL_LINKS" },
        });
        // A capped read asks for one row past its cap, enough to know
        // there were more; a streamed one asks for everything.
        if let Some(max_rows) = out.limit() {
            body["row_limit"] = serde_json::json!(max_rows + 1);
        }
        if let Some(c) = &self.conn.catalog {
            body["catalog"] = serde_json::Value::String(c.clone());
        }
        if let Some(s) = &self.conn.schema {
            body["schema"] = serde_json::Value::String(s.clone());
        }
        let url = format!("{}{STATEMENTS}", self.base);
        let (status, text) = self.request(http::Method::Post, &url, Some(body.to_string()))?;
        if status != 200 {
            return Err(self.refused(status, &text));
        }
        let mut answer = self.parse(&text)?;
        if !matches!(answer.status.state.as_str(), "SUCCEEDED" | "FAILED" | "CANCELED" | "CLOSED") {
            answer = self.poll(&answer.statement_id)?;
        }
        if answer.status.state != "SUCCEEDED" {
            let why = answer.status.error.and_then(|e| e.message).unwrap_or_default();
            return Err(Error::Query(self.conn.uid.clone(), format!("{}: {why}", answer.status.state.to_lowercase()).trim().to_string()));
        }
        self.collect(answer, out, &arriving)
    }

    /// Waits on a pending statement: a second at first, doubling to five,
    /// until the statement timeout cancels it.
    fn poll(&mut self, id: &str) -> Result<Answer> {
        let url = format!("{}{STATEMENTS}/{id}", self.base);
        let deadline = Instant::now() + self.conn.timeout();
        let mut wait = Duration::from_secs(1);
        loop {
            if Instant::now() >= deadline {
                let _ = self.request(http::Method::Post, &format!("{url}/cancel"), None);
                return Err(Error::Query(
                    self.conn.uid.clone(),
                    format!("still running after {}s; cancelled (query_timeout)", self.conn.timeout().as_secs()),
                ));
            }
            std::thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
            wait = (wait * 2).min(Duration::from_secs(5));
            let (status, text) = self.request(http::Method::Get, &url, None)?;
            if status != 200 {
                return Err(self.refused(status, &text));
            }
            let answer = self.parse(&text)?;
            if matches!(answer.status.state.as_str(), "PENDING" | "RUNNING") {
                continue;
            }
            return Ok(answer);
        }
    }

    /// The rows across the chunks. The manifest says the total first, so
    /// a result over the cap is refused before any chunk past the first.
    fn collect(&mut self, answer: Answer, out: &mut dyn RowSink, arriving: &Deadline) -> Result<()> {
        let manifest = answer.manifest.unwrap_or_default();
        if manifest.truncated {
            return Err(match out.limit() {
                // Cut at the one row past the cap that was asked for.
                Some(max_rows) => Error::TooManyRows(self.conn.uid.clone(), max_rows),
                // Nothing was asked to be cut, so Databricks cut it, at
                // the most it returns for one statement.
                None => Error::Query(
                    self.conn.uid.clone(),
                    "the result is larger than Databricks returns for one statement (100 GiB), and was truncated; add a LIMIT, or select fewer columns".into(),
                ),
            });
        }
        if let Some(total) = manifest.total_row_count.and_then(|n| u64::try_from(n).ok()) {
            out.total(total)?;
        }
        let mut columns: Vec<(i64, String)> = manifest.schema.columns.into_iter().map(|c| (c.position.unwrap_or(0), c.name)).collect();
        columns.sort_by_key(|(p, _)| *p);
        let columns: Vec<String> = columns.into_iter().map(|(_, n)| n).collect();
        out.columns(&columns)?;
        let mut result = answer.result;
        while let Some(r) = result.take() {
            for row in r.data_array {
                out.row(row.into_iter().map(http::cell).collect())?;
            }
            // A chunk given as links: each is fetched from where it
            // points and is the chunk's rows.
            let mut after = r.next_chunk_index;
            for link in r.external_links {
                arriving.check()?;
                for row in self.linked(&link.external_link)? {
                    out.row(row.into_iter().map(http::cell).collect())?;
                }
                after = link.next_chunk_index.or(after);
            }
            let Some(next) = after else { break };
            arriving.check()?;
            let url = format!("{}{STATEMENTS}/{}/result/chunks/{next}", self.base, answer.statement_id);
            let (status, text) = self.request(http::Method::Get, &url, None)?;
            if status != 200 {
                return Err(self.refused(status, &text));
            }
            result = Some(serde_json::from_str(&text).map_err(|e| Error::Query(self.conn.uid.clone(), format!("unexpected chunk: {e}")))?);
        }
        Ok(())
    }
}

#[derive(serde::Deserialize, Default)]
struct Answer {
    #[serde(default)]
    statement_id: String,
    #[serde(default)]
    status: Status,
    manifest: Option<Manifest>,
    result: Option<Chunk>,
}

#[derive(serde::Deserialize, Default)]
struct Status {
    #[serde(default)]
    state: String,
    error: Option<StatusError>,
}

#[derive(serde::Deserialize)]
struct StatusError {
    message: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct Manifest {
    #[serde(default)]
    schema: Schema,
    total_row_count: Option<i64>,
    #[serde(default)]
    truncated: bool,
}

#[derive(serde::Deserialize, Default)]
struct Schema {
    #[serde(default)]
    columns: Vec<SchemaColumn>,
}

#[derive(serde::Deserialize)]
struct SchemaColumn {
    name: String,
    position: Option<i64>,
}

#[derive(serde::Deserialize)]
struct Chunk {
    #[serde(default)]
    data_array: Vec<Vec<serde_json::Value>>,
    /// The chunk as links to it, when the result was asked for that way.
    #[serde(default)]
    external_links: Vec<Link>,
    next_chunk_index: Option<i64>,
}

#[derive(serde::Deserialize)]
struct Link {
    external_link: String,
    next_chunk_index: Option<i64>,
}

impl Source for Databricks {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        self.conn.access.check(sql)?;
        let mut kept = Capped::new(&self.conn.uid, max_rows);
        self.send(sql, &mut kept)?;
        Ok(kept.finish())
    }

    fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        self.conn.access.check(sql)?;
        let mut batcher = Batcher::new(sink);
        self.send(sql, &mut batcher)?;
        batcher.finish()
    }

    fn adapter(&self) -> &str {
        "databricks"
    }

    fn transport(&self) -> &str {
        "rest"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
