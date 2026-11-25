use std::path::{Path, PathBuf};

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
        oauth_token: get("oauth_token"),
    }
}
