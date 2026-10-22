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

use crate::{http, read_only, Connection, Result, Rows, Secret, Secrets, Source, SourceError};
use base64::Engine;
use std::time::{Duration, Instant};

const STATEMENTS: &str = "/api/2.0/sql/statements";
const USER_AGENT: &str = concat!("zsql/", env!("CARGO_PKG_VERSION"));
/// How long the first request itself waits before the API hands back a
/// pending statement: the API's own maximum.
const WAIT: &str = "30s";

pub struct Databricks {
    agent: ureq::Agent,
    base: String,
    conn: Connection,
    auth: Auth,
    /// A minted token and when it stops being good.
    token: Option<(String, Instant)>,
    warehouse: String,
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
        let auth = if let Some(t) = secrets.personal_access_token {
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
            return Err(SourceError::NoCredentials(conn.uid.clone(), missing.join("; ")));
        }
        if base.to_lowercase().starts_with("http://") && conn.sslmode.insists() {
            return Err(SourceError::PlainUrl(conn.uid.clone(), "databricks"));
        }
        Ok(Databricks {
            agent: http::agent(conn)?,
            base: base.trim_end_matches('/').to_string(),
            conn: conn.clone(),
            auth: auth.expect("checked"),
            token: None,
            warehouse: conn.warehouse.clone().expect("checked"),
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
                let auth = format!("Basic {basic}");
                let headers = [
                    ("Authorization", auth.as_str()),
                    ("Content-Type", "application/x-www-form-urlencoded"),
                    ("Accept", "application/json"),
                    ("User-Agent", USER_AGENT),
                ];
                let url = format!("{}/oidc/v1/token", self.base);
                let (status, text) = http::call(&self.conn, &self.agent, "POST", &url, &headers, Some(body.as_bytes()))?;
                let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
                if status != 200 {
                    let why = v.get("error_description").or_else(|| v.get("error")).and_then(|e| e.as_str()).unwrap_or(text.trim());
                    return Err(SourceError::Connect(self.conn.uid.clone(), format!("token: {status}: {why}")));
                }
                let token = v
                    .get("access_token")
                    .and_then(|t| t.as_str())
                    .map(str::to_string)
                    .ok_or_else(|| SourceError::Connect(self.conn.uid.clone(), "token: the answer carried no access_token".into()))?;
                let secs = v.get("expires_in").and_then(|e| e.as_u64()).unwrap_or(3600);
                self.token = Some((token.clone(), Instant::now() + Duration::from_secs(secs)));
                Ok(token)
            }
        }
    }

    fn request(&mut self, method: &str, url: &str, body: Option<String>) -> Result<(u16, String)> {
        let bearer = format!("Bearer {}", self.bearer()?);
        let headers = [
            ("Authorization", bearer.as_str()),
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("User-Agent", USER_AGENT),
        ];
        http::call(&self.conn, &self.agent, method, url, &headers, body.as_deref().map(str::as_bytes))
    }

    fn refused(&self, status: u16, text: &str) -> SourceError {
        let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
        let why = v.get("message").and_then(|m| m.as_str()).map(str::to_string).unwrap_or_else(|| text.trim().chars().take(400).collect());
        SourceError::Query(self.conn.uid.clone(), format!("{status}: {why}"))
    }

    fn parse(&self, text: &str) -> Result<Answer> {
        serde_json::from_str(text).map_err(|e| SourceError::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))
    }

    fn send(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        let mut body = serde_json::json!({
            "statement": sql,
            "warehouse_id": self.warehouse,
            "wait_timeout": WAIT,
            "on_wait_timeout": "CONTINUE",
            "format": "JSON_ARRAY",
            "disposition": "INLINE",
            "row_limit": max_rows + 1,
        });
        if let Some(c) = &self.conn.catalog {
            body["catalog"] = serde_json::Value::String(c.clone());
        }
        if let Some(s) = &self.conn.schema {
            body["schema"] = serde_json::Value::String(s.clone());
        }
        let url = format!("{}{STATEMENTS}", self.base);
        let (status, text) = self.request("POST", &url, Some(body.to_string()))?;
        if status != 200 {
            return Err(self.refused(status, &text));
        }
        let mut answer = self.parse(&text)?;
        if !matches!(answer.status.state.as_str(), "SUCCEEDED" | "FAILED" | "CANCELED" | "CLOSED") {
            answer = self.poll(&answer.statement_id)?;
        }
        if answer.status.state != "SUCCEEDED" {
            let why = answer.status.error.and_then(|e| e.message).unwrap_or_default();
            return Err(SourceError::Query(
                self.conn.uid.clone(),
                format!("{}: {why}", answer.status.state.to_lowercase()).trim().to_string(),
            ));
        }
        self.collect(answer, max_rows)
    }

    /// Waits on a pending statement: a second at first, doubling to five,
    /// until the statement timeout cancels it.
    fn poll(&mut self, id: &str) -> Result<Answer> {
        let url = format!("{}{STATEMENTS}/{id}", self.base);
        let deadline = Instant::now() + self.conn.timeout();
        let mut wait = Duration::from_secs(1);
        loop {
            if Instant::now() >= deadline {
                let _ = self.request("POST", &format!("{url}/cancel"), None);
                return Err(SourceError::Query(
                    self.conn.uid.clone(),
                    format!("still running after {}s; cancelled (query_timeout)", self.conn.timeout().as_secs()),
                ));
            }
            std::thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
            wait = (wait * 2).min(Duration::from_secs(5));
            let (status, text) = self.request("GET", &url, None)?;
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
    fn collect(&mut self, answer: Answer, max_rows: usize) -> Result<Rows> {
        let manifest = answer.manifest.unwrap_or_default();
        if manifest.truncated || manifest.total_row_count.is_some_and(|n| n > max_rows as i64) {
            return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
        }
        let mut columns: Vec<(i64, String)> = manifest.schema.columns.into_iter().map(|c| (c.position.unwrap_or(0), c.name)).collect();
        columns.sort_by_key(|(p, _)| *p);
        let columns: Vec<String> = columns.into_iter().map(|(_, n)| n).collect();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        let mut result = answer.result;
        while let Some(r) = result.take() {
            rows.extend(r.data_array.into_iter().map(|row| row.into_iter().map(cell).collect::<Vec<_>>()));
            if rows.len() > max_rows {
                return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
            }
            let Some(next) = r.next_chunk_index else { break };
            let url = format!("{}{STATEMENTS}/{}/result/chunks/{next}", self.base, answer.statement_id);
            let (status, text) = self.request("GET", &url, None)?;
            if status != 200 {
                return Err(self.refused(status, &text));
            }
            result =
                Some(serde_json::from_str(&text).map_err(|e| SourceError::Query(self.conn.uid.clone(), format!("unexpected chunk: {e}")))?);
        }
        Ok(Rows { columns, rows })
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
    next_chunk_index: Option<i64>,
}

/// `JSON_ARRAY` sends every value as a string already; this is for the
/// null and for anything that arrives bare.
fn cell(v: serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}

impl Source for Databricks {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        self.send(sql, max_rows)
    }

    fn adapter(&self) -> &str {
        "databricks"
    }
}
