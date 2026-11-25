//! BigQuery over its REST API: a query POSTed to `jobs.query` for one
//! project, polled through `jobs.getQueryResults` when it is not done in
//! the request's own wait, the rows paged by token. Every cell arrives
//! as a string, which is how the API already sends them.
//!
//! The credential is a service account's or a user's. A service
//! account's private key signs a JWT the token endpoint trades for an
//! access token with the read-only BigQuery scope; the key and the
//! address it signs for are either `private_key` and `client_email`
//! in `.zsql`, which is where strata kept them, or in the JSON key file
//! Google hands out. A user's is the application-default credentials
//! `gcloud auth application-default login` writes, whose refresh token
//! is traded the same way. A file is named by `key_file` in
//! `datasources.yml`, by `GOOGLE_APPLICATION_CREDENTIALS`, or found
//! where gcloud leaves it; a key in `.zsql` is used before any file.

use crate::rows::{Batcher, Capped, RowSink};
use crate::{http, jwt, Connection, Deadline, Error, Result, Rows, Secrets, Sink, Source};
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

/// What the credential turned out to be.
enum Credential {
    /// An access token obtained elsewhere, sent as it is: nothing here
    /// can refresh it, so the connection is good for as long as it is.
    AccessToken(crate::Secret),
    ServiceAccount {
        email: String,
        signer: Box<jwt::Signer>,
        token_uri: String,
    },
    AuthorizedUser {
        client_id: String,
        client_secret: String,
        refresh_token: String,
        token_uri: String,
    },
}

impl BigQuery {
    pub fn open(conn: &Connection, secrets: Secrets) -> Result<BigQuery> {
        BigQuery::open_with(conn, secrets, &|k| std::env::var(k).ok(), gcloud_default())
    }

    /// `open` with the environment and gcloud's default file given, so a
    /// test can hand in its own.
    pub fn open_with(
        conn: &Connection,
        secrets: Secrets,
        env: &dyn Fn(&str) -> Option<String>,
        default_file: Option<PathBuf>,
    ) -> Result<BigQuery> {
        let mut missing: Vec<String> = Vec::new();
        if conn.project.is_none() {
            missing.push("project".into());
        }
        let credential = match (secrets.oauth_token, &secrets.private_key, file(conn, env, default_file)) {
            // A token from upstream is the end user's; a key or a file on
            // this machine is somebody else, and is not consulted.
            (Some(token), _, _) => Some(Credential::AccessToken(token)),
            // The key held for this datasource, before any file.
            (None, Some(key), _) => match inline(conn, key) {
                Ok(c) => Some(c),
                Err(e) => {
                    missing.push(e);
                    None
                }
            },
            (None, None, Some(path)) => match credential(&conn.uid, &path) {
                Ok(c) => Some(c),
                Err(e) => {
                    missing.push(e.to_string());
                    None
                }
            },
            (None, None, None) => {
                missing.push(format!(
                    "credentials: a service account's private_key and client_email in .zsql under {uid}, or key_file in datasources.yml naming its JSON key, or GOOGLE_APPLICATION_CREDENTIALS, or `gcloud auth application-default login`",
                    uid = conn.uid
                ));
                None
            }
        };
        if !missing.is_empty() {
            return Err(Error::NoCredentials(conn.uid.clone(), missing.join("; ")));
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
    pub(crate) fn bearer(&mut self) -> Result<String> {
        if let Some((t, until)) = &self.token {
            if Instant::now() + Duration::from_secs(60) < *until {
                return Ok(t.clone());
            }
        }
        let (token_uri, body) = match &self.credential {
            Credential::AccessToken(t) => return Ok(t.expose().to_string()),
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
                    Error::PrivateKey(_, why) => Error::PrivateKey(self.conn.uid.clone(), why),
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

    fn refused(&self, status: u16, text: &str) -> Error {
        Error::Query(self.conn.uid.clone(), format!("{status}: {}", http::reason(text)))
    }

    fn parse(&self, text: &str) -> Result<Answer> {
        serde_json::from_str(text).map_err(|e| Error::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))
    }

    fn send(&mut self, sql: &str, out: &mut dyn RowSink) -> Result<()> {
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
            return Err(Error::Query(self.conn.uid.clone(), "the job is not complete and the answer names no job to wait for".into()));
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
        let arriving = Deadline::of(&self.conn);
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
                return Err(Error::Query(
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
                return Err(Error::Query(self.conn.uid.clone(), message));
            }
            self.warnings.push(format!("{}: the job reported: {message}", self.conn.uid));
        }
        // The total is known from the first page, so a result over the cap
        // is refused before any other page is fetched.
        if let Some(total) = answer.total_rows.as_deref().and_then(|n| n.parse::<u64>().ok()) {
            out.total(total)?;
        }
        let columns: Vec<String> = answer.schema.as_ref().map(|s| s.fields.iter().map(|f| f.name.clone()).collect()).unwrap_or_default();
        out.columns(&columns)?;
        if let Some(changed) = answer.num_dml_affected_rows.as_deref().and_then(|n| n.parse::<u64>().ok()) {
            out.affected(changed)?;
        }
        loop {
            for r in answer.rows {
                out.row(r.f.into_iter().map(|c| http::cell(c.v)).collect())?;
            }
            let Some(token) = answer.page_token.take() else { break };
            arriving.check()?;
            let (status, text) = self.request(http::Method::Get, &results_url(Some(&token)), None)?;
            if status != 200 {
                return Err(self.refused(status, &text));
            }
            answer = self.parse(&text)?;
        }
        Ok(())
    }
}

/// The credentials file this connection would read: the one it names,
/// the environment's, or gcloud's own where there is one.
fn file(conn: &Connection, env: &dyn Fn(&str) -> Option<String>, default_file: Option<PathBuf>) -> Option<PathBuf> {
    conn.key_file
        .clone()
        .or_else(|| env("GOOGLE_APPLICATION_CREDENTIALS").filter(|p| !p.trim().is_empty()).map(PathBuf::from))
        .or_else(|| default_file.filter(|p| p.exists()))
}

/// Whether a key in `.zsql` is this connection's credential: both
/// halves of a service account are held.
pub(crate) fn has_inline(conn: &Connection, secrets: &Secrets) -> bool {
    secrets.private_key.is_some() && conn.client_email.is_some()
}

/// Whether a credential is at hand, without reading or trading it.
pub(crate) fn at_hand(conn: &Connection, secrets: &Secrets) -> bool {
    // A key held is used before any file, so half a service account is
    // no credential even beside a file that would have been one.
    match (&secrets.oauth_token, &secrets.private_key) {
        (Some(_), _) => true,
        (None, Some(_)) => conn.client_email.is_some(),
        (None, None) => file(conn, &|k| std::env::var(k).ok(), gcloud_default()).is_some(),
    }
}

/// A service account from `.zsql`: `private_key`, the PEM or a path to
/// it, and the `client_email` it signs for.
fn inline(conn: &Connection, key: &crate::Secret) -> std::result::Result<Credential, String> {
    let Some(email) = conn.client_email.clone() else {
        return Err(format!(
            "client_email, the service account the private_key signs for: beside it in .zsql under {}, or in datasources.yml",
            conn.uid
        ));
    };
    let pem = if key.expose().contains("-----BEGIN") {
        // A key copied out of the JSON file keeps that file's escapes
        // unless the YAML around it is double-quoted.
        key.expose().replace("\\n", "\n")
    } else {
        std::fs::read_to_string(key.expose()).map_err(|e| format!("private_key is neither a PEM nor a file that can be read: {e}"))?
    };
    let signer = jwt::Signer::from_pem(&conn.uid, pem.as_bytes()).map_err(|e| e.to_string())?;
    Ok(Credential::ServiceAccount { email, signer: Box::new(signer), token_uri: DEFAULT_TOKEN_URI.into() })
}

/// The credentials file, by its `type`.
fn credential(uid: &str, path: &std::path::Path) -> Result<Credential> {
    let fail = |why: String| Error::NoCredentials(uid.to_string(), format!("{}: {why}", path.display()));
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
    /// Rows a DML statement changed, as BigQuery counts them.
    num_dml_affected_rows: Option<String>,
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
        "bigquery"
    }

    fn transport(&self) -> &str {
        "rest"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }

    fn connection(&self) -> Option<&Connection> {
        Some(&self.conn)
    }
}
