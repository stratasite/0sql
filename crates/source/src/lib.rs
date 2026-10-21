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
//!   all that introspecting a catalogue and sampling a dimension need.
//!
//! Statements come from `zsql` itself, never from a caller, and the
//! guard in [`read_only`] is a second pair of eyes on that rather than a
//! sandbox: the protection that matters is a warehouse credential with
//! no write grant. See `docs/introspection.md`.

use std::path::{Path, PathBuf};

pub mod catalogue;
pub mod postgres;

pub use catalogue::{Catalogue, Column, Table};

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
}

pub type Result<T> = std::result::Result<T, SourceError>;

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
    /// For an in-process adapter: the file it opens.
    pub file: Option<PathBuf>,
    /// Seconds. The same key the planner reads for its own purposes, used
    /// here as the statement timeout.
    pub query_timeout: Option<i64>,
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
                schema: s("schema"),
                username: s("username"),
                file: s("file").map(PathBuf::from),
                query_timeout: i("query_timeout"),
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
    /// generous for a catalogue read and short of any warehouse's own.
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

// ---------------------------------------------------------------------- rows

/// What a statement returned. Every cell is text or null: a catalogue is
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
}

/// Refuses a statement that is not a read.
///
/// `zsql` writes every statement this crate runs, so this is a second
/// pair of eyes rather than a defence: it catches a generated statement
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
    if READS.contains(&first.to_uppercase().as_str()) {
        Ok(())
    } else {
        Err(refuse())
    }
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

/// Opens the adapter this connection names.
pub fn open(conn: &Connection, secret: Option<Secret>) -> Result<Box<dyn Source>> {
    match conn.adapter.as_str() {
        "postgres" | "redshift" => Ok(Box::new(postgres::Postgres::open(conn, secret)?)),
        other => Err(SourceError::Unsupported(other.to_string())),
    }
}

#[cfg(test)]
mod tests;
