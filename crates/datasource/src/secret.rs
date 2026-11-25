use std::path::{Path, PathBuf};

use crate::connection::{overlay, CONNECTION_KEYS};
#[cfg(doc)]
use crate::takes_oauth_token;

/// A datasource's secret, resolved when a connection is opened and held
/// no longer than the connection is.
#[derive(Clone, PartialEq, Eq)]
pub struct Secret(pub(crate) String);

impl Secret {
    /// A secret handed over by whoever holds it: a server's upstream, a
    /// test. The command line resolves its own with [`secret`].
    pub fn new(value: impl Into<String>) -> Secret {
        Secret(value.into())
    }

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

/// The gitignored files a datasource's settings may be in, nearest first:
/// the project's `.zsql` (or `.strata`, the older CLI's file, read only
/// when `.zsql` is absent, which is the rule the rest of the CLI
/// follows), then the account-wide config.
fn local_files(project_dir: Option<&Path>) -> Vec<PathBuf> {
    let mut files: Vec<PathBuf> = Vec::new();
    if let Some(dir) = project_dir {
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
    files
}

/// A setting as text: what was written, trimmed, and a number as its
/// digits, so a password of `123456` is still a password.
fn text(v: &serde_yaml::Value) -> Option<String> {
    let s = match v {
        serde_yaml::Value::String(s) => s.trim().to_string(),
        serde_yaml::Value::Number(n) => n.to_string(),
        _ => return None,
    };
    (!s.is_empty()).then_some(s)
}

/// What this machine holds for a datasource outside `datasources.yml`:
/// its entry in `.zsql`, then in the account-wide config. Any key of
/// [`CONNECTION_KEYS`] or [`SECRET_FIELDS`] may be there, nested under
/// the datasource's own key, as the template describes and as `.strata`
/// had them, or flat as `<uid>_<key>`:
///
/// ```yaml
/// warehouse:
///   username: analyst
///   password: hunter2
/// # or
/// warehouse_password: hunter2
/// ```
///
/// The nearest file that holds a key decides it, and within a file the
/// nested spelling does. `adapter` is not read from here: which
/// warehouse a datasource is belongs to the model.
pub fn local_entry(project_dir: Option<&Path>, uid: &str) -> serde_yaml::Mapping {
    let known = || CONNECTION_KEYS.iter().chain(SECRET_FIELDS.iter()).copied().filter(|k| *k != "adapter");
    let mut out = serde_yaml::Mapping::new();
    for f in local_files(project_dir) {
        let Ok(text_of) = std::fs::read_to_string(&f) else { continue };
        let Ok(doc) = serde_yaml::from_str::<serde_yaml::Value>(&text_of) else { continue };
        for key in known() {
            let nested = doc.get(uid).and_then(|d| d.get(key)).filter(|v| text(v).is_some());
            let flat = doc.get(format!("{uid}_{key}")).filter(|v| text(v).is_some());
            if let Some(v) = nested.or(flat) {
                out.entry(key.into()).or_insert_with(|| v.clone());
            }
        }
    }
    out
}

/// A datasource's entry as a command run here reads it: what
/// `datasources.yml` says, and over it what this machine holds for it
/// in `.zsql`. A key in both is `.zsql`'s, under any of its spellings.
pub(crate) fn entry(project_dir: Option<&Path>, uid: &str) -> serde_yaml::Mapping {
    let written = project_dir
        .and_then(|dir| std::fs::read_to_string(dir.join("datasources.yml")).ok())
        .and_then(|text| serde_yaml::from_str::<serde_yaml::Value>(&text).ok())
        .and_then(|doc| {
            let (_, d) = doc.as_mapping()?.iter().find(|(k, _)| k.as_str().is_some_and(|k| k.trim().to_lowercase() == uid))?;
            d.as_mapping().cloned()
        });
    let mut entry = written.unwrap_or_default();
    overlay(&mut entry, local_entry(project_dir, uid));
    entry
}

/// A datasource's secret: from the environment first, so a shell can
/// override a checked-out project without editing it; then `.zsql` and
/// the account-wide config, as [`local_entry`] reads them; then
/// `datasources.yml` itself, for whoever wrote it there.
pub fn secret(project_dir: Option<&Path>, uid: &str, field: &str) -> Option<Secret> {
    held(&entry(project_dir, uid), uid, field)
}

fn held(entry: &serde_yaml::Mapping, uid: &str, field: &str) -> Option<Secret> {
    std::env::var(secret_env_var(uid, field))
        .ok()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .or_else(|| entry.get(field).and_then(text))
        .map(Secret)
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
    /// An OAuth access token somebody else obtained: the end user's, for
    /// a warehouse that takes one. Whoever hands it over keeps it fresh;
    /// nothing here refreshes it, and a connection lives no longer than
    /// the token it was opened with. See [`takes_oauth_token`].
    pub oauth_token: Option<Secret>,
}

/// The field names [`secrets`] resolves, as `.zsql` and the environment
/// spell them.
pub const SECRET_FIELDS: [&str; 9] = [
    "password",
    "personal_access_token",
    "private_key",
    "oauth_client_id",
    "oauth_client_secret",
    "access_key_id",
    "secret_access_key",
    "session_token",
    "oauth_token",
];

impl Secrets {
    /// A digest of every secret here, for telling one credential from
    /// another without keeping either: the same secrets give the same
    /// digest, and nothing of them can be read back from it.
    pub fn fingerprint(&self) -> String {
        let fields = [
            &self.password,
            &self.personal_access_token,
            &self.private_key,
            &self.oauth_client_id,
            &self.oauth_client_secret,
            &self.access_key_id,
            &self.secret_access_key,
            &self.session_token,
            &self.oauth_token,
        ];
        let mut hash = ring::digest::Context::new(&ring::digest::SHA256);
        for field in fields {
            // Each field framed by its length, so two fields can never
            // read as one longer one.
            let value = field.as_ref().map(Secret::expose).unwrap_or_default();
            hash.update(&(value.len() as u64).to_be_bytes());
            hash.update(&[u8::from(field.is_some())]);
            hash.update(value.as_bytes());
        }
        hash.finish().as_ref().iter().map(|b| format!("{b:02x}")).collect()
    }

    /// Each secret held, under its name in [`SECRET_FIELDS`]: for
    /// whoever sends a credential on, as a deploy does. Not for
    /// printing.
    pub fn fields(&self) -> Vec<(&'static str, &str)> {
        let all = [
            ("password", &self.password),
            ("personal_access_token", &self.personal_access_token),
            ("private_key", &self.private_key),
            ("oauth_client_id", &self.oauth_client_id),
            ("oauth_client_secret", &self.oauth_client_secret),
            ("access_key_id", &self.access_key_id),
            ("secret_access_key", &self.secret_access_key),
            ("session_token", &self.session_token),
            ("oauth_token", &self.oauth_token),
        ];
        all.into_iter().filter_map(|(name, held)| Some((name, held.as_ref()?.expose()))).collect()
    }

    /// Whether nothing was given: no field of it holds a secret.
    pub fn is_empty(&self) -> bool {
        self.password.is_none()
            && self.personal_access_token.is_none()
            && self.private_key.is_none()
            && self.oauth_client_id.is_none()
            && self.oauth_client_secret.is_none()
            && self.access_key_id.is_none()
            && self.secret_access_key.is_none()
            && self.session_token.is_none()
            && self.oauth_token.is_none()
    }

    /// The secrets a caller sent beside the connection, under the names
    /// of [`SECRET_FIELDS`]: for a server that is handed a credential
    /// with each request and keeps none. Nothing is read from the
    /// environment or a file here.
    pub fn from_value(d: &serde_yaml::Value) -> Secrets {
        let get =
            |field: &str| d.get(field).and_then(|v| v.as_str()).map(str::trim).filter(|s| !s.is_empty()).map(|s| Secret(s.to_string()));
        Secrets {
            password: get("password"),
            personal_access_token: get("personal_access_token"),
            private_key: get("private_key"),
            oauth_client_id: get("oauth_client_id"),
            oauth_client_secret: get("oauth_client_secret"),
            access_key_id: get("access_key_id"),
            secret_access_key: get("secret_access_key"),
            session_token: get("session_token"),
            oauth_token: get("oauth_token"),
        }
    }

    /// Just a password, which is what most adapters are given.
    pub fn password(password: Option<Secret>) -> Secrets {
        Secrets { password, ..Default::default() }
    }
}

/// Every field of [`SECRET_FIELDS`] for one datasource, each resolved as
/// [`secret`] resolves it.
pub fn secrets(project_dir: Option<&Path>, uid: &str) -> Secrets {
    let entry = entry(project_dir, uid);
    let get = |field: &str| held(&entry, uid, field);
    Secrets {
        password: get("password"),
        personal_access_token: get("personal_access_token"),
        private_key: get("private_key"),
        oauth_client_id: get("oauth_client_id"),
        oauth_client_secret: get("oauth_client_secret"),
        access_key_id: get("access_key_id"),
        secret_access_key: get("secret_access_key"),
        session_token: get("session_token"),
        oauth_token: get("oauth_token"),
    }
}
