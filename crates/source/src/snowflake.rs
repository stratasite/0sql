//! Snowflake over its SQL REST API: a statement POSTed to
//! `/api/v2/statements`, answered at once or with a handle to poll, the
//! result paged by partition. Every cell arrives as text because the
//! request pins the date and time output formats, which is the decision
//! the Postgres transport made with its simple-query protocol.
//!
//! Two ways in, neither a password. A personal access token is sent as it
//! is. A key pair signs a JWT per hour: `iss` is the account and user with
//! the public key's fingerprint, `sub` the account and user, and the
//! account is the identifier's first label upper-cased, which is what
//! Snowflake checks it against. There is no browser flow here; that is a
//! web app's shape, not a command's.
//!
//! The session context — database, warehouse, schema, role — rides on
//! each request as Snowflake would read it unquoted, upper-cased, unless
//! the value is written in double quotes, in which case it is sent as
//! written.

use crate::{http, jwt, read_only, Connection, Result, Rows, Secret, Secrets, Source, SourceError};
use std::time::{Duration, Instant};

const STATEMENTS: &str = "/api/v2/statements";
/// A key-pair token lives an hour by Snowflake's rule; one is minted
/// fresh once it is fifty minutes old.
const JWT_LIFETIME: Duration = Duration::from_secs(3600);
const JWT_RENEW_AFTER: Duration = Duration::from_secs(50 * 60);
const USER_AGENT: &str = concat!("zsql/", env!("CARGO_PKG_VERSION"));

pub struct Snowflake {
    agent: ureq::Agent,
    base: String,
    conn: Connection,
    auth: Auth,
    jwt: Option<(String, Instant)>,
    context: Context,
    warnings: Vec<String>,
}

enum Auth {
    Token(Secret),
    KeyPair { signer: Box<jwt::Signer>, account: String, user: String },
}

/// The session the statement runs in, already folded.
struct Context {
    database: Option<String>,
    schema: Option<String>,
    warehouse: Option<String>,
    role: Option<String>,
}

impl Snowflake {
    /// Opens without a round trip: a Snowflake request spins a warehouse
    /// up, so the first statement is the probe, and a bad credential is
    /// reported by the server's own message on it. What is checked here
    /// is that everything a request needs is present, all of it at once.
    pub fn open(conn: &Connection, secrets: Secrets) -> Result<Snowflake> {
        let mut missing: Vec<String> = Vec::new();
        let base = match (&conn.url, &conn.account) {
            (Some(u), _) => u.clone(),
            // The account becomes a host name, so it may only hold what a
            // host name does; anything else would send the bearer elsewhere.
            (None, Some(a))
                if !a.trim().is_empty() && a.trim().chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_')) =>
            {
                format!("https://{}.snowflakecomputing.com", a.trim())
            }
            (None, Some(a)) => {
                missing.push(format!("account: {a:?} is not an account identifier (letters, digits, dots and dashes)"));
                String::new()
            }
            (None, None) => {
                missing.push("account (the account identifier, as in <account>.snowflakecomputing.com) or url".into());
                String::new()
            }
        };
        if conn.database.is_none() {
            missing.push("database".into());
        }
        let auth = match credentials(conn, secrets) {
            Ok(a) => Some(a),
            Err(why) => {
                missing.push(why);
                None
            }
        };
        if !missing.is_empty() {
            return Err(SourceError::NoCredentials(conn.uid.clone(), missing.join("; ")));
        }
        let note = http::plain(conn, &base, "snowflake")?;
        Ok(Snowflake {
            agent: http::agent(conn)?,
            base: base.trim_end_matches('/').to_string(),
            conn: conn.clone(),
            auth: auth.expect("checked"),
            jwt: None,
            context: Context {
                database: conn.database.as_deref().map(ident),
                schema: conn.schema.as_deref().map(ident),
                warehouse: conn.warehouse.as_deref().map(ident),
                role: conn.role.as_deref().map(ident),
            },
            warnings: note.into_iter().collect(),
        })
    }

    /// The bearer and the header that says what kind it is.
    fn bearer(&mut self) -> Result<(String, &'static str)> {
        match &self.auth {
            Auth::Token(t) => Ok((t.expose().to_string(), "PROGRAMMATIC_ACCESS_TOKEN")),
            Auth::KeyPair { signer, account, user } => {
                let fresh = self.jwt.as_ref().filter(|(_, at)| at.elapsed() < JWT_RENEW_AFTER).map(|(t, _)| t.clone());
                let token = match fresh {
                    Some(t) => t,
                    None => {
                        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
                        let claims = serde_json::json!({
                            "iss": format!("{account}.{user}.SHA256:{}", signer.public_key_fingerprint()),
                            "sub": format!("{account}.{user}"),
                            "iat": now,
                            "exp": now + JWT_LIFETIME.as_secs(),
                        });
                        let t = signer.rs256(&claims).map_err(|e| match e {
                            SourceError::PrivateKey(_, why) => SourceError::PrivateKey(self.conn.uid.clone(), why),
                            other => other,
                        })?;
                        self.jwt = Some((t.clone(), Instant::now()));
                        t
                    }
                };
                Ok((token, "KEYPAIR_JWT"))
            }
        }
    }

    fn request(&mut self, method: http::Method, url: &str, body: Option<String>) -> Result<(u16, String)> {
        let (token, kind) = self.bearer()?;
        let bearer = format!("Bearer {token}");
        let headers = [
            ("Authorization", bearer.as_str()),
            ("X-Snowflake-Authorization-Token-Type", kind),
            ("Content-Type", "application/json"),
            ("Accept", "application/json"),
            ("User-Agent", USER_AGENT),
        ];
        http::call(&self.conn, &self.agent, method, url, &headers, body.as_deref().map(str::as_bytes))
    }

    /// A refusal, as the server put it: the `message` of its JSON when
    /// there is one, the body otherwise.
    fn refused(&self, status: u16, text: &str) -> SourceError {
        SourceError::Query(self.conn.uid.clone(), format!("{status}: {}", http::reason(text)))
    }

    fn send(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        let body = serde_json::json!({
            "statement": sql,
            "timeout": self.conn.timeout().as_secs(),
            "database": self.context.database,
            "schema": self.context.schema,
            "warehouse": self.context.warehouse,
            "role": self.context.role,
            "parameters": {
                "DATE_OUTPUT_FORMAT": "YYYY-MM-DD",
                "TIME_OUTPUT_FORMAT": "HH24:MI:SS",
                "TIMESTAMP_OUTPUT_FORMAT": "YYYY-MM-DD HH24:MI:SS.FF3 TZHTZM",
                "TIMESTAMP_NTZ_OUTPUT_FORMAT": "YYYY-MM-DD HH24:MI:SS.FF3",
                "TIMESTAMP_LTZ_OUTPUT_FORMAT": "YYYY-MM-DD HH24:MI:SS.FF3 TZHTZM",
                "TIMESTAMP_TZ_OUTPUT_FORMAT": "YYYY-MM-DD HH24:MI:SS.FF3 TZHTZM",
            },
        });
        // Null context keys are dropped rather than sent as JSON null.
        let body = match body {
            serde_json::Value::Object(mut m) => {
                m.retain(|_, v| !v.is_null());
                serde_json::Value::Object(m)
            }
            other => other,
        };
        let url = format!("{}{STATEMENTS}", self.base);
        let (status, text) = self.request(http::Method::Post, &url, Some(body.to_string()))?;
        let answer = match status {
            200 => self.parse(&text)?,
            202 => {
                let handle = self.parse(&text)?.handle.ok_or_else(|| self.refused(status, &text))?;
                self.poll(&handle)?
            }
            _ => return Err(self.refused(status, &text)),
        };
        self.collect(answer, max_rows)
    }

    /// Waits on a statement the server is still running: a quarter second
    /// first, doubling to five, until the statement timeout, at which
    /// point the statement is cancelled so it stops costing.
    fn poll(&mut self, handle: &str) -> Result<Answer> {
        let url = format!("{}{STATEMENTS}/{handle}", self.base);
        let deadline = Instant::now() + self.conn.timeout();
        let mut wait = Duration::from_millis(250);
        loop {
            if Instant::now() >= deadline {
                let _ = self.request(http::Method::Post, &format!("{url}/cancel"), None);
                return Err(SourceError::Query(
                    self.conn.uid.clone(),
                    format!("still running after {}s; cancelled (query_timeout)", self.conn.timeout().as_secs()),
                ));
            }
            std::thread::sleep(wait.min(deadline.saturating_duration_since(Instant::now())));
            wait = (wait * 2).min(Duration::from_secs(5));
            let (status, text) = self.request(http::Method::Get, &url, None)?;
            match status {
                200 => return self.parse(&text),
                202 => continue,
                _ => return Err(self.refused(status, &text)),
            }
        }
    }

    fn parse(&self, text: &str) -> Result<Answer> {
        serde_json::from_str(text).map_err(|e| SourceError::Query(self.conn.uid.clone(), format!("unexpected answer: {e}")))
    }

    /// The rows of a finished statement, across its partitions. The total
    /// is known before any partition past the first is fetched, so a
    /// result over the cap costs one request, not all of them.
    fn collect(&mut self, answer: Answer, max_rows: usize) -> Result<Rows> {
        let meta = answer.meta.unwrap_or_default();
        if meta.num_rows.is_some_and(|n| n > max_rows as i64) {
            return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
        }
        let columns: Vec<String> = meta.row_type.into_iter().map(|c| c.name).collect();
        let mut rows: Vec<Vec<Option<String>>> =
            answer.data.into_iter().flatten().map(|r| r.into_iter().map(http::cell).collect()).collect();
        if let Some(handle) = answer.handle.filter(|_| meta.partitions.len() > 1) {
            for p in 1..meta.partitions.len() {
                let url = format!("{}{STATEMENTS}/{handle}?partition={p}", self.base);
                let (status, text) = self.request(http::Method::Get, &url, None)?;
                if status != 200 {
                    return Err(self.refused(status, &text));
                }
                let part = self.parse(&text)?;
                rows.extend(part.data.into_iter().flatten().map(|r| r.into_iter().map(http::cell).collect::<Vec<_>>()));
                if rows.len() > max_rows {
                    return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
                }
            }
        }
        if rows.len() > max_rows {
            return Err(SourceError::TooManyRows(self.conn.uid.clone(), max_rows));
        }
        Ok(Rows { columns, rows })
    }
}

/// Which credential the connection carries, or what it would take.
fn credentials(conn: &Connection, secrets: Secrets) -> std::result::Result<Auth, String> {
    let env = |field: &str| crate::secret_env_var(&conn.uid, field);
    if let Some(t) = secrets.personal_access_token {
        return Ok(Auth::Token(t));
    }
    // The key, as PEM text in .zsql, or a path there or in datasources.yml.
    let pem: Option<Vec<u8>> = match (&secrets.private_key, &conn.private_key_file) {
        (Some(s), _) if s.expose().contains("-----BEGIN") => Some(s.expose().as_bytes().to_vec()),
        (Some(s), _) => Some(std::fs::read(s.expose()).map_err(|e| format!("private_key names a file that cannot be read: {e}"))?),
        (None, Some(path)) => Some(std::fs::read(path).map_err(|e| format!("private_key_file {}: {e}", path.display()))?),
        (None, None) => None,
    };
    let Some(pem) = pem else {
        return Err(format!(
            "credentials: a personal access token as personal_access_token in .zsql under {uid} or in {pat}, or a key pair as username in datasources.yml with private_key_file there or private_key (the PEM, or its path) in .zsql or in {key}",
            uid = conn.uid,
            pat = env("personal_access_token"),
            key = env("private_key"),
        ));
    };
    let Some(user) = conn.username.as_deref().map(str::trim).filter(|u| !u.is_empty()) else {
        return Err("username, which a key pair signs for".into());
    };
    let signer = jwt::Signer::from_pem(&conn.uid, &pem).map_err(|e| e.to_string())?;
    let account = conn
        .account
        .as_deref()
        .or_else(|| conn.url.as_deref().and_then(host_label))
        .ok_or_else(|| "account, which a key pair signs for".to_string())?;
    Ok(Auth::KeyPair { signer: Box::new(signer), account: jwt_account(account), user: user.to_uppercase() })
}

/// The account as a key-pair token names it: the identifier's first label,
/// upper-cased. A locator with its region and cloud (`xy12345.us-east-2.aws`)
/// becomes `XY12345`; an organization form (`myorg-myaccount`) stays whole.
fn jwt_account(account: &str) -> String {
    account.trim().split('.').next().unwrap_or_default().to_uppercase()
}

/// The first label of a url's host, for an `url` that stands in for `account`.
fn host_label(url: &str) -> Option<&str> {
    let host = url.split("://").nth(1)?.split('/').next()?;
    host.split('.').next().filter(|s| !s.is_empty())
}

/// An identifier as Snowflake reads it unquoted — upper-cased — unless it
/// is written in double quotes, in which case what is inside them, as is.
pub(crate) fn ident(v: &str) -> String {
    let v = v.trim();
    match v.strip_prefix('"').and_then(|s| s.strip_suffix('"')) {
        Some(inner) => inner.to_string(),
        None => v.to_uppercase(),
    }
}

/// What the statements endpoint answers with, in the parts read here.
#[derive(serde::Deserialize, Default)]
struct Answer {
    #[serde(rename = "resultSetMetaData")]
    meta: Option<Meta>,
    #[serde(default)]
    data: Option<Vec<Vec<serde_json::Value>>>,
    #[serde(rename = "statementHandle")]
    handle: Option<String>,
}

#[derive(serde::Deserialize, Default)]
struct Meta {
    #[serde(rename = "numRows")]
    num_rows: Option<i64>,
    #[serde(rename = "rowType", default)]
    row_type: Vec<RowType>,
    #[serde(rename = "partitionInfo", default)]
    partitions: Vec<serde_json::Value>,
}

#[derive(serde::Deserialize)]
struct RowType {
    name: String,
}

impl Source for Snowflake {
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows> {
        read_only(sql)?;
        self.send(sql, max_rows)
    }

    fn adapter(&self) -> &str {
        "snowflake"
    }

    fn warnings(&self) -> &[String] {
        &self.warnings
    }
}
