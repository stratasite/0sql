//! BigQuery over its REST API: a query POSTed to `jobs.query` for one
//! project, polled through `jobs.getQueryResults` when it is not done in
//! the request's own wait, the rows paged by token. Every cell arrives
//! as a string, which is how the API already sends them.
//!
//! Neither strata nor `dwh` has this one, so the credentials are 0sql's
//! own design, and they are files: a service account's JSON key, whose
//! private key signs a JWT the token endpoint trades for an access token
//! with the read-only BigQuery scope; or the application-default
//! credentials `gcloud auth application-default login` writes, whose
//! refresh token is traded the same way. The file is named by `key_file`
//! in `datasources.yml`, by `GOOGLE_APPLICATION_CREDENTIALS`, or found
//! where gcloud leaves it.

use crate::{http, jwt, read_only, Connection, Result, Rows, Secrets, Source, SourceError};
use std::path::PathBuf;
use std::time::{Duration, Instant};

const DEFAULT_BASE: &str = "https://bigquery.googleapis.com";
const DEFAULT_TOKEN_URI: &str = "https://oauth2.googleapis.com/token";
const SCOPE: &str = "https://www.googleapis.com/auth/bigquery.readonly";
const USER_AGENT: &str = concat!("zsql/", env!("CARGO_PKG_VERSION"));

pub struct BigQuery {
    agent: ureq::Agent,
    base: String,
    conn: Connection,
    project: String,
    credential: Credential,
    token: Option<(String, Instant)>,
    warnings: Vec<String>,
}

/// What the credentials file held.
enum Credential {
    ServiceAccount { email: String, signer: Box<jwt::Signer>, token_uri: String },
    AuthorizedUser { client_id: String, client_secret: String, refresh_token: String, token_uri: String },
}

impl BigQuery {
    pub fn open(conn: &Connection, secrets: Secrets) -> Result<BigQuery> {
        BigQuery::open_with(conn, secrets, &|k| std::env::var(k).ok(), gcloud_default())
    }

    /// `open` with the environment and gcloud's default file given, so a
    /// test can hand in its own.
    pub fn open_with(
        conn: &Connection,
        _secrets: Secrets,
        env: &dyn Fn(&str) -> Option<String>,
        default_file: Option<PathBuf>,
    ) -> Result<BigQuery> {
        let mut missing: Vec<String> = Vec::new();
        if conn.project.is_none() {
            missing.push("project".into());
        }
        let file = conn
            .key_file
            .clone()
            .or_else(|| env("GOOGLE_APPLICATION_CREDENTIALS").map(PathBuf::from))
            .or_else(|| default_file.filter(|p| p.exists()));
        let credential = match file {
            Some(path) => match credential(&conn.uid, &path) {
                Ok(c) => Some(c),
                Err(e) => {
                    missing.push(e.to_string());
                    None
                }
            },
            None => {
                missing.push(
                    "credentials: key_file in datasources.yml naming a service account's JSON key, or GOOGLE_APPLICATION_CREDENTIALS, or `gcloud auth application-default login`".into(),
                );
                None
            }
        };
        if !missing.is_empty() {
            return Err(SourceError::NoCredentials(conn.uid.clone(), missing.join("; ")));
        }
        let base = conn.url.clone().unwrap_or_else(|| DEFAULT_BASE.into());
        let note = http::plain(conn, &base, "bigquery")?;
        Ok(BigQuery {
            agent: http::agent(conn)?,
            base: base.trim_end_matches('/').to_string(),
            conn: conn.clone(),
            project: conn.project.clone().expect("checked"),
            credential: credential.expect("checked"),
            token: None,
            warnings: note.into_iter().collect(),
        })
    }

    /// An access token, minted or refreshed when the one held is within
    /// a minute of expiring.
    fn bearer(&mut self) -> Result<String> {
        if let Some((t, until)) = &self.token {
            if Instant::now() + Duration::from_secs(60) < *until {
                return Ok(t.clone());
            }
        }
        let (token_uri, body) = match &self.credential {
            Credential::ServiceAccount { email, signer, token_uri } => {
                let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                let claims = serde_json::json!({
                    "iss": email,
                    "scope": SCOPE,
                    "aud": token_uri,
                    "iat": now,
                    "exp": now + 3600,
                });
                let assertion = signer.rs256(&claims).map_err(|e| match e {
                    SourceError::PrivateKey(_, why) => SourceError::PrivateKey(self.conn.uid.clone(), why),
                    other => other,
                })?;
                (token_uri.clone(), http::form(&[("grant_type", "urn:ietf:params:oauth:grant-type:jwt-bearer"), ("assertion", &assertion)]))
            }
            Credential::AuthorizedUser { client_id, client_secret, refresh_token, token_uri } => (
                token_uri.clone(),
                http::form(&[
                    ("grant_type", "refresh_token"),
                    ("client_id", client_id),
                    ("client_secret", client_secret),
                    ("refresh_token", refresh_token),
                ]),
            ),
        };
        let (token, until) = http::token(&self.conn, &self.agent, &token_uri, None, &body)?;
        self.token = Some((token.clone(), until));
        Ok(token)
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

    fn refused(&self, status: u16, text: &str) -> SourceError {
        SourceError::Query(self.conn.uid.clone(), format!("{status}: {}", http::reason(text)))
    }

    fn parse(&self, text: &str) -> Result<Answer> {
        serde_json::from_str(text).map_err(|e| SourceError::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))
    }

    fn send(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        let timeout_ms = self.conn.timeout().as_millis().min(10 * 60 * 1000) as u64;
        let mut body = serde_json::json!({
            "query": sql,
            "useLegacySql": false,
            "timeoutMs": timeout_ms,
        });
        if let Some(d) = &self.conn.schema {
            body["defaultDataset"] = serde_json::json!({ "projectId": self.project, "datasetId": d });
        }
        if let Some(l) = &self.conn.location {
            body["location"] = serde_json::Value::String(l.clone());
        }
        let url = format!("{}/bigquery/v2/projects/{}/queries", self.base, http::urlencode(&self.project));
        let (status, text) = self.request(http::Method::Post, &url, Some(body.to_string()))?;
        if status != 200 {
            return Err(self.refused(status, &text));
        }
        let mut answer = self.parse(&text)?;
        let job = answer.job_reference.clone().unwrap_or_default();
        if job.job_id.is_empty() && !answer.job_complete {
            return Err(SourceError::Query(
                self.conn.uid.clone(),
                "the job is not complete and the answer names no job to wait for".into(),
            ));
        }
        let base = self.base.clone();
        let project = http::urlencode(&self.project);
        let results_url = |page: Option<&str>| {
            let mut u = format!("{base}/bigquery/v2/projects/{project}/queries/{}?timeoutMs={timeout_ms}", http::urlencode(&job.job_id));
            if let Some(l) = &job.location {
                u.push_str(&format!("&location={}", http::urlencode(l)));
            }
            if let Some(p) = page {
                u.push_str(&format!("&pageToken={}", http::urlencode(p)));
            }
            u
        };
        // Not done within the request's own wait: ask again until it is,
        // or until the statement timeout cancels the job.
        let deadline = Instant::now() + self.conn.timeout();
        while !answer.job_complete {
            if Instant::now() >= deadline {
                let cancel = format!(
                    "{}/bigquery/v2/projects/{}/jobs/{}/cancel{}",
                    self.base,
                    http::urlencode(&self.project),
                    http::urlencode(&job.job_id),
                    job.location.as_deref().map(|l| format!("?location={}", http::urlencode(l))).unwrap_or_default()
                );
                let _ = self.request(http::Method::Post, &cancel, Some("{}".into()));
                return Err(SourceError::Query(
                    self.conn.uid.clone(),
                    format!("still running after {}s; cancelled (query_timeout)", self.conn.timeout().as_secs()),
                ));
            }
            let (status, text) = self.request(http::Method::Get, &results_url(None), None)?;
            if status != 200 {
                return Err(self.refused(status, &text));
            }
            answer = self.parse(&text)?;
        }
        // `errors` on a completed job are its warnings as often as its
        // failure, the API says; without a result they are the failure.
        if let Some(e) = answer.errors.first() {
            let message = e.message.clone().unwrap_or_else(|| "query failed".into());
            if answer.schema.is_none() {
                return Err(SourceError::Query(self.conn.uid.clone(), message));
            }
            self.warnings.push(format!("{}: the job reported: {message}", self.conn.uid));
        }
        // The total is known from the first page, so a result over the cap
        // is refused before any other page is fetched.
        if answer.total_rows.as_deref().and_then(|n| n.parse::<i64>().ok()).is_some_and(|n| n > max_rows as i64) {
            return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
        }
        let columns: Vec<String> = answer.schema.as_ref().map(|s| s.fields.iter().map(|f| f.name.clone()).collect()).unwrap_or_default();
        let mut rows: Vec<Vec<Option<String>>> = Vec::new();
        loop {
            rows.extend(answer.rows.into_iter().map(|r| r.f.into_iter().map(|c| http::cell(c.v)).collect::<Vec<_>>()));
            if rows.len() > max_rows {
                return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
            }
            let Some(token) = answer.page_token.take() else { break };
            let (status, text) = self.request(http::Method::Get, &results_url(Some(&token)), None)?;
            if status != 200 {
                return Err(self.refused(status, &text));
            }
            answer = self.parse(&text)?;
        }
        Ok(Rows { columns, rows })
    }
}

/// The credentials file, by its `type`.
fn credential(uid: &str, path: &std::path::Path) -> Result<Credential> {
    let fail = |why: String| SourceError::NoCredentials(uid.to_string(), format!("{}: {why}", path.display()));
    let text = std::fs::read_to_string(path).map_err(|e| fail(e.to_string()))?;
    let v: serde_json::Value = serde_json::from_str(&text).map_err(|e| fail(format!("not JSON: {e}")))?;
    let field = |k: &str| v.get(k).and_then(|x| x.as_str()).map(str::to_string);
    let token_uri = field("token_uri").unwrap_or_else(|| DEFAULT_TOKEN_URI.into());
    match field("type").as_deref() {
        Some("service_account") => {
            let email = field("client_email").ok_or_else(|| fail("no client_email".into()))?;
            let pem = field("private_key").ok_or_else(|| fail("no private_key".into()))?;
            let signer = jwt::Signer::from_pem(uid, pem.as_bytes())?;
            Ok(Credential::ServiceAccount { email, signer: Box::new(signer), token_uri })
        }
        Some("authorized_user") => Ok(Credential::AuthorizedUser {
            client_id: field("client_id").ok_or_else(|| fail("no client_id".into()))?,
            client_secret: field("client_secret").ok_or_else(|| fail("no client_secret".into()))?,
            refresh_token: field("refresh_token").ok_or_else(|| fail("no refresh_token".into()))?,
            token_uri,
        }),
        other => Err(fail(format!(
            "type is {}; a service account key or application-default credentials were expected",
            other.unwrap_or("missing")
        ))),
    }
}

/// Where `gcloud auth application-default login` leaves its file.
fn gcloud_default() -> Option<PathBuf> {
    if let Some(dir) = std::env::var_os("CLOUDSDK_CONFIG").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(dir).join("application_default_credentials.json"));
    }
    if let Some(app) = std::env::var_os("APPDATA").filter(|d| !d.is_empty()) {
        return Some(PathBuf::from(app).join("gcloud").join("application_default_credentials.json"));
    }
    std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".config").join("gcloud").join("application_default_credentials.json"))
}

#[derive(serde::Deserialize, Default)]
#[serde(rename_all = "camelCase")]
struct Answer {
    #[serde(default)]
    job_complete: bool,
    job_reference: Option<JobReference>,
    schema: Option<Schema>,
    total_rows: Option<String>,
    page_token: Option<String>,
    #[serde(default)]
    rows: Vec<Row>,
    #[serde(default)]
    errors: Vec<ApiError>,
}

#[derive(serde::Deserialize, Default, Clone)]
#[serde(rename_all = "camelCase")]
struct JobReference {
    #[serde(default)]
    job_id: String,
    location: Option<String>,
}

#[derive(serde::Deserialize)]
struct Schema {
    #[serde(default)]
    fields: Vec<Field>,
}

#[derive(serde::Deserialize)]
struct Field {
    name: String,
}

#[derive(serde::Deserialize)]
struct Row {
    #[serde(default)]
    f: Vec<Cell>,
}

#[derive(serde::Deserialize)]
struct Cell {
    #[serde(default)]
    v: serde_json::Value,
}

#[derive(serde::Deserialize)]
struct ApiError {
    message: Option<String>,
}

impl Source for BigQuery {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        self.send(sql, max_rows)
    }

    fn adapter(&self) -> &str {
        "bigquery"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn connection(&self) -> Option<&Connection> {
        Some(&self.conn)
    }
}
