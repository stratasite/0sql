//! Reaching a datasource to read from it.
//!
//! The planner never connects to a warehouse; this crate is the only
//! place that does, which is what makes the question "what can open a
//! socket" answerable by reading one directory. It holds three things:
//!
//! - [`Connection`], where a datasource lives, read from the
//!   `datasources.yml` keys the model loader ignores;
//! - [`Secret`], resolved at connect time from the environment or the
//!   project's gitignored `.zsql`, and never part of a deployed model;
//! - [`Source`], one read-only statement in and [`Rows`] out, which is
//!   all that introspecting a catalog and sampling a dimension need.
//!
//! Statements come from `zsql` itself, never from a caller, and the
//! guard in [`read_only`] is a second pair of eyes on that rather than a
//! sandbox: the protection that matters is a warehouse credential with
//! no write grant. See `docs/introspection.md`.

use std::path::{Path, PathBuf};

pub mod athena;
pub mod bigquery;
pub mod catalog;
pub mod clickhouse;
pub mod databricks;
pub mod duckdb;
mod http;
pub mod jwt;
pub mod mysql;
pub mod postgres;
pub mod sigv4;
pub mod snowflake;
pub mod sqlite;
pub mod sqlserver;
pub mod tls;
pub mod trino;

pub use catalog::{Catalog, Column, Drift, ForeignKey, Table};

#[derive(Debug, thiserror::Error)]
pub enum SourceError {
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    #[error("{0}: {1}")]
    Yaml(String, serde_yaml::Error),
    #[error("datasources.yml names no datasource {0}")]
    NoDatasource(String),
    #[error("datasource {0} has no {1}; add it to datasources.yml")]
    Missing(String, &'static str),
    #[error("datasource {uid} needs a password: put it in .zsql under {uid}, or in {env}")]
    NoSecret { uid: String, env: String },
    #[error("this statement is not a read: {0}")]
    NotARead(String),
    #[error("{0} is not an adapter this build can reach")]
    Unsupported(String),
    #[error("connecting to {0}: {1}")]
    Connect(String, String),
    #[error("running a statement on {0}: {1}")]
    Query(String, String),
    #[error("{0} returned more than the {1} rows asked for")]
    TooManyRows(String, usize),
    #[error("sslmode {0} is not one of disable, prefer, require, verify-ca, verify-full")]
    SslMode(String),
    #[error("{0}: {1} requires TLS, and the url is plain http; use https, or say sslmode: disable")]
    PlainUrl(String, &'static str),
    #[error("reading ca_file {0}: {1}")]
    CaFile(String, String),
    #[error("datasource {0} is missing: {1}")]
    NoCredentials(String, String),
    #[error("{0}: private key: {1}")]
    PrivateKey(String, String),
}

pub type Result<T> = std::result::Result<T, SourceError>;

/// An error and everything underneath it, joined: a driver's outer
/// message is often "error performing TLS handshake" while the reason —
/// "server does not support TLS" — is the cause below it.
pub fn chain(e: &dyn std::error::Error) -> String {
    let mut out = e.to_string();
    let mut cur = e.source();
    while let Some(inner) = cur {
        let s = inner.to_string();
        if !out.contains(&s) {
            out.push_str(": ");
            out.push_str(&s);
        }
        cur = inner.source();
    }
    out
}

// ------------------------------------------------------------------ sslmode

/// How much TLS a connection insists on: libpq's five rungs, with libpq's
/// meanings, because that is the ladder every warehouse client shares —
/// MySQL's `--ssl-mode` is the same five with other names, and SQL
/// Server's `Encrypt` and `TrustServerCertificate` map onto three of them.
///
/// `require` encrypts and does not verify, as it does in libpq; that is
/// the rung a self-signed development server needs, and it is named
/// rather than hidden behind a flag. The default is `verify-full`, which
/// is where Microsoft moved its drivers' default in 2022 and which the
/// PostgreSQL documentation recommends over its own historical `prefer`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Default)]
pub enum SslMode {
    /// Plaintext. For a container on loopback.
    Disable,
    /// TLS if the server offers it, plaintext if not, never verified. A
    /// connection that ends up plaintext says so in its warnings.
    Prefer,
    /// TLS or nothing; the certificate is not checked.
    Require,
    /// TLS, the chain checked against the trust store, the name not.
    VerifyCa,
    /// TLS, chain and host name both checked.
    #[default]
    VerifyFull,
}

impl SslMode {
    /// libpq's spellings, and MySQL's, in any case.
    pub fn parse(s: &str) -> Result<SslMode> {
        Ok(match s.trim().to_lowercase().replace('_', "-").as_str() {
            "disable" | "disabled" | "off" | "false" => SslMode::Disable,
            "prefer" | "preferred" | "allow" => SslMode::Prefer,
            "require" | "required" | "on" | "true" => SslMode::Require,
            "verify-ca" => SslMode::VerifyCa,
            "verify-full" | "verify-identity" => SslMode::VerifyFull,
            _ => return Err(SourceError::SslMode(s.trim().to_string())),
        })
    }

    pub fn name(self) -> &'static str {
        match self {
            SslMode::Disable => "disable",
            SslMode::Prefer => "prefer",
            SslMode::Require => "require",
            SslMode::VerifyCa => "verify-ca",
            SslMode::VerifyFull => "verify-full",
        }
    }

    /// Whether a plaintext connection is refused outright.
    pub fn insists(self) -> bool {
        self >= SslMode::Require
    }

    /// Whether the server's certificate is checked at all.
    pub fn verifies(self) -> bool {
        self >= SslMode::VerifyCa
    }
}

// ---------------------------------------------------------------- connection

/// Where a datasource lives. These are the `datasources.yml` keys the
/// model loader reads past: the deployed model keeps only the name, the
/// adapter and the tier, so nothing here ever reaches the server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Connection {
    pub uid: String,
    pub adapter: String,
    pub host: Option<String>,
    pub port: Option<u16>,
    pub database: Option<String>,
    pub schema: Option<String>,
    pub username: Option<String>,
    /// For an adapter reached over HTTP: the base URL, scheme included, when
    /// `host` and `port` are not enough to say it — a cloud endpoint, or
    /// TLS on a port that does not imply it.
    pub url: Option<String>,
    /// For an in-process adapter: the file it opens.
    pub file: Option<PathBuf>,
    /// Seconds. The same key the planner reads for its own purposes, used
    /// here as the statement timeout.
    pub query_timeout: Option<i64>,
    /// How much TLS to insist on. `verify-full` unless said otherwise.
    pub sslmode: SslMode,
    /// A PEM file of certificates to trust in addition to the platform's:
    /// libpq's `sslrootcert`, MySQL's `--ssl-ca`. For a private CA.
    pub ca_file: Option<PathBuf>,
    /// Snowflake's account identifier: the host is
    /// `<account>.snowflakecomputing.com`, and a key pair signs for it.
    pub account: Option<String>,
    /// Snowflake's compute to run on; optional, the user's default otherwise.
    pub warehouse: Option<String>,
    /// Snowflake's role to assume; optional.
    pub role: Option<String>,
    /// A PEM file holding the private key of a key pair. A path is not a
    /// secret, so it may live in `datasources.yml`; the key itself may
    /// instead be in `.zsql` as `private_key`.
    pub private_key_file: Option<PathBuf>,
    /// Databricks' Unity catalog, Athena's data catalog.
    pub catalog: Option<String>,
    /// An OAuth client id: Databricks' service principal application id.
    /// Not a secret, unlike the client secret beside it in `.zsql`.
    pub client_id: Option<String>,
    /// BigQuery's project.
    pub project: Option<String>,
    /// BigQuery's location, for a job and for a region-wide catalog read.
    pub location: Option<String>,
    /// BigQuery's service-account key file, or an application-default
    /// credentials file.
    pub key_file: Option<PathBuf>,
    /// An AWS region, for Athena.
    pub region: Option<String>,
    /// An Athena workgroup.
    pub workgroup: Option<String>,
    /// Athena's result location on S3, unless the workgroup sets one.
    pub output_location: Option<String>,
    /// A named profile in the AWS shared credentials file.
    pub profile: Option<String>,
    /// Seconds a stored result from this datasource answers for; `0`
    /// keeps nothing. Read by `zsql run`, ignored by the loader.
    pub cache_ttl: Option<i64>,
}

impl Connection {
    /// Every datasource in a project's `datasources.yml`, in file order.
    pub fn read_all(project_dir: &Path) -> Result<Vec<Connection>> {
        let path = project_dir.join("datasources.yml");
        let text = std::fs::read_to_string(&path).map_err(|e| SourceError::Io(path.display().to_string(), e))?;
        let doc: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| SourceError::Yaml(path.display().to_string(), e))?;
        let Some(map) = doc.as_mapping() else { return Ok(vec![]) };
        let mut out = Vec::new();
        for (key, d) in map {
            let Some(uid) = key.as_str() else { continue };
            let s = |k: &str| d.get(k).and_then(|v| v.as_str()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
            let i = |k: &str| d.get(k).and_then(|v| v.as_i64());
            out.push(Connection {
                uid: uid.trim().to_lowercase(),
                adapter: s("adapter").map(|a| a.to_lowercase()).unwrap_or_default(),
                host: s("host"),
                port: i("port").and_then(|p| u16::try_from(p).ok()),
                database: s("database"),
                schema: s("schema").or_else(|| s("dataset")),
                username: s("username"),
                url: s("url").map(|u| u.trim_end_matches('/').to_string()),
                file: s("file").map(PathBuf::from),
                query_timeout: i("query_timeout"),
                sslmode: match s("sslmode").or_else(|| s("ssl_mode")) {
                    Some(m) => SslMode::parse(&m)?,
                    None => SslMode::default(),
                },
                ca_file: s("ca_file").or_else(|| s("sslrootcert")).or_else(|| s("ssl_ca")).map(PathBuf::from),
                account: s("account").or_else(|| s("account_identifier")),
                warehouse: s("warehouse"),
                role: s("role"),
                private_key_file: s("private_key_file").or_else(|| s("private_key_path")).map(PathBuf::from),
                catalog: s("catalog"),
                client_id: s("client_id").or_else(|| s("oauth_client_id")),
                project: s("project").or_else(|| s("project_id")),
                location: s("location"),
                key_file: s("key_file").or_else(|| s("credentials_file")).map(PathBuf::from),
                region: s("region"),
                workgroup: s("workgroup"),
                output_location: s("output_location").or_else(|| s("s3_output_location")),
                profile: s("profile"),
                cache_ttl: i("cache_ttl"),
            });
        }
        Ok(out)
    }

    /// One datasource by its key, as `datasources.yml` spells it.
    pub fn read(project_dir: &Path, uid: &str) -> Result<Connection> {
        let want = uid.trim().to_lowercase();
        Connection::read_all(project_dir)?.into_iter().find(|c| c.uid == want).ok_or_else(|| SourceError::NoDatasource(uid.to_string()))
    }

    fn need(&self, value: &Option<String>, what: &'static str) -> Result<String> {
        value.clone().ok_or_else(|| SourceError::Missing(self.uid.clone(), what))
    }

    /// The statement timeout, from `query_timeout` or a default that is
    /// generous for a catalog read and short of any warehouse's own.
    pub fn timeout(&self) -> std::time::Duration {
        let secs = self.query_timeout.filter(|s| *s > 0).unwrap_or(60);
        std::time::Duration::from_secs(secs.min(3600) as u64)
    }
}

// -------------------------------------------------------------------- secret

/// A datasource's secret, resolved when a connection is opened and held
/// no longer than the connection is.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(String);

impl Secret {
    pub fn expose(&self) -> &str {
        &self.0
    }
}

/// Never print a secret, even by accident in an error or a log line.
impl std::fmt::Debug for Secret {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Secret(…)")
    }
}

/// The environment variable a datasource's password may arrive in, as the
/// `datasources.yml` template promises: the uid upper-cased, with anything
/// that is not a letter or a digit becoming an underscore.
pub fn secret_env_var(uid: &str, field: &str) -> String {
    let key: String = uid.chars().map(|c| if c.is_ascii_alphanumeric() { c.to_ascii_uppercase() } else { '_' }).collect();
    format!("ZSQL_DS_{key}_{}", field.to_uppercase())
}

/// A datasource's secret, from the environment first and then the
/// gitignored files, so a shell can override a checked-out project
/// without editing it.
///
/// In a file it is either nested under the datasource's own key, as the
/// template describes, or flat as `<uid>_<field>`:
///
/// ```yaml
/// warehouse:
///   password: hunter2
/// # or
/// warehouse_password: hunter2
/// ```
pub fn secret(project_dir: Option<&Path>, uid: &str, field: &str) -> Option<Secret> {
    let clean = |s: &str| {
        let s = s.trim();
        (!s.is_empty()).then(|| Secret(s.to_string()))
    };
    if let Some(v) = std::env::var(secret_env_var(uid, field)).ok().as_deref().and_then(clean) {
        return Some(v);
    }
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(dir) = project_dir {
        // `.strata` is the older CLI's file and is read only when `.zsql`
        // is absent, which is the rule the rest of the CLI follows.
        let local = dir.join(".zsql");
        files.push(if local.exists() { local } else { dir.join(".strata") });
    }
    if let Some(home) = std::env::var_os("ZSQL_HOME")
        .filter(|h| !h.is_empty())
        .map(PathBuf::from)
        .or_else(|| std::env::var_os("HOME").map(|h| PathBuf::from(h).join(".zsql")))
    {
        files.push(home.join("config"));
    }
    for f in files {
        let Ok(text) = std::fs::read_to_string(&f) else { continue };
        let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text) else { continue };
        let nested = doc.get(uid).and_then(|d| d.get(field)).and_then(|v| v.as_str()).and_then(clean);
        let flat = doc.get(format!("{uid}_{field}")).and_then(|v| v.as_str()).and_then(clean);
        if let Some(v) = nested.or(flat) {
            return Some(v);
        }
    }
    None
}

/// Every secret a transport might ask for, resolved once. An adapter takes
/// the ones it understands and names the rest in one error when none of
/// them is there.
#[derive(Clone, Default, Debug, PartialEq, Eq)]
pub struct Secrets {
    /// What every password-taking adapter reads.
    pub password: Option<Secret>,
    /// Snowflake's and Databricks' personal access token.
    pub personal_access_token: Option<Secret>,
    /// The PEM of a key pair's private key, or a path to it.
    pub private_key: Option<Secret>,
    /// An OAuth client id, when it is kept beside its secret rather than
    /// in `datasources.yml`.
    pub oauth_client_id: Option<Secret>,
    /// Databricks' service-principal secret.
    pub oauth_client_secret: Option<Secret>,
    /// AWS credentials, for Athena.
    pub access_key_id: Option<Secret>,
    pub secret_access_key: Option<Secret>,
    pub session_token: Option<Secret>,
}

/// The field names [`secrets`] resolves, as `.zsql` and the environment
/// spell them.
pub const SECRET_FIELDS: [&str; 8] = [
    "password",
    "personal_access_token",
    "private_key",
    "oauth_client_id",
    "oauth_client_secret",
    "access_key_id",
    "secret_access_key",
    "session_token",
];

impl Secrets {
    /// Just a password, which is what most adapters are given.
    pub fn password(password: Option<Secret>) -> Secrets {
        Secrets { password, ..Default::default() }
    }
}

/// Every field of [`SECRET_FIELDS`] for one datasource, each resolved as
/// [`secret`] resolves it.
pub fn secrets(project_dir: Option<&Path>, uid: &str) -> Secrets {
    let get = |field: &str| secret(project_dir, uid, field);
    Secrets {
        password: get("password"),
        personal_access_token: get("personal_access_token"),
        private_key: get("private_key"),
        oauth_client_id: get("oauth_client_id"),
        oauth_client_secret: get("oauth_client_secret"),
        access_key_id: get("access_key_id"),
        secret_access_key: get("secret_access_key"),
        session_token: get("session_token"),
    }
}

// ---------------------------------------------------------------------- rows

/// What a statement returned. Every cell is text or null: a catalog is
/// strings and a sampled value is the string a consumer will match
/// against, so nothing here needs a type system yet.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Rows {
    /// The cell in `row` under the column of this name, if the statement
    /// returned one and it is not null.
    pub fn get<'a>(&'a self, row: &'a [Option<String>], column: &str) -> Option<&'a str> {
        let at = self.columns.iter().position(|c| c == column)?;
        row.get(at)?.as_deref()
    }
}

// -------------------------------------------------------------------- source

/// One read-only statement in, rows out. Implemented once per transport,
/// which is three implementations for thirteen adapters.
pub trait Source {
    /// Runs `sql` and returns at most `max_rows` rows, refusing rather
    /// than truncating past it so a caller never averages a partial
    /// answer. Implementations call [`read_only`] first.
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows>;

    /// The adapter's name, for an error that says where it happened.
    fn adapter(&self) -> &str;

    /// What a caller should be told about this connection that is not an
    /// error: that `prefer` ended plaintext, for one.
    fn warnings(&self) -> &[String] {
        &[]
    }

    /// The connection this was opened from, for a catalog read that
    /// needs more than the adapter's name: BigQuery's project and location.
    fn connection(&self) -> Option<&Connection> {
        None
    }
}

/// Refuses a statement that is not a read.
///
/// `zsql` writes every statement this crate runs, so this is a second
/// pair of eyes rather than a defense: it catches a generated statement
/// that went wrong, and a `sample_sql` an author wrote by hand. The
/// protection that matters is a credential with no write grant, which the
/// documentation asks for.
pub fn read_only(sql: &str) -> Result<()> {
    let refuse = || SourceError::NotARead(sql.chars().take(80).collect());
    let text = skeleton(sql);
    let trimmed = text.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() || trimmed.contains(';') {
        // One statement per call: a second one is how a read becomes a write.
        return Err(refuse());
    }
    let first: String = trimmed.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    const READS: [&str; 5] = ["SELECT", "WITH", "SHOW", "DESCRIBE", "PRAGMA"];
    if !READS.contains(&first.to_uppercase().as_str()) {
        return Err(refuse());
    }
    // A read can still carry a write: a data-modifying CTE, `SELECT …
    // INTO`, `INSERT … SELECT` behind a `WITH`. None of these words has a
    // place in a read, so any of them as a bare word refuses. A quoted
    // identifier was blanked with the literals, so a column called
    // `update` in quotes is not caught by this.
    const WRITES: [&str; 16] = [
        "INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "CREATE", "DROP", "ALTER", "TRUNCATE", "GRANT", "REVOKE", "EXEC", "EXECUTE", "CALL",
        "COPY", "UNLOAD",
    ];
    let words = trimmed.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty());
    for w in words {
        if WRITES.contains(&w.to_uppercase().as_str()) {
            return Err(refuse());
        }
    }
    Ok(())
}

/// The statement with its comments removed and the inside of every string
/// literal blanked, keeping the quotes. Comments go so that a leading
/// `/* */` cannot hide what the statement starts with and a `--` cannot
/// hide what follows it; literals are blanked so that a semicolon a value
/// legitimately contains is not read as a second statement. Only
/// [`read_only`] sees this; the warehouse gets the statement as written.
fn skeleton(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
                out.push(c);
            } else {
                // Whatever a value holds, it is not syntax.
                out.push(if c == '\n' { '\n' } else { ' ' });
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}

/// Opens the adapter this connection names, with whichever of the
/// secrets it takes.
pub fn open(conn: &Connection, secrets: Secrets) -> Result<Box<dyn Source>> {
    let secret = secrets.password.clone();
    match conn.adapter.as_str() {
        "postgres" | "redshift" => Ok(Box::new(postgres::Postgres::open(conn, secret)?)),
        "clickhouse" => Ok(Box::new(clickhouse::ClickHouse::open(conn, secret)?)),
        "mysql" => Ok(Box::new(mysql::MySql::open(conn, secret)?)),
        "trino" => Ok(Box::new(trino::Trino::open(conn, secret)?)),
        "athena" => Ok(Box::new(athena::Athena::open(conn, secrets)?)),
        "sqlserver" => Ok(Box::new(sqlserver::SqlServer::open(conn, secret)?)),
        "snowflake" => Ok(Box::new(snowflake::Snowflake::open(conn, secrets)?)),
        "databricks" => Ok(Box::new(databricks::Databricks::open(conn, secrets)?)),
        "bigquery" => Ok(Box::new(bigquery::BigQuery::open(conn, secrets)?)),
        "sqlite" => Ok(Box::new(sqlite::Sqlite::open(conn)?)),
        "duckdb" => Ok(Box::new(duckdb::DuckDb::open(conn)?)),
        other => Err(SourceError::Unsupported(other.to_string())),
    }
}

#[cfg(test)]
mod tests;
