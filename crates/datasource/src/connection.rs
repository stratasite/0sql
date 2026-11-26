use std::path::{Path, PathBuf};

use crate::{Access, Error, Result};

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
            _ => return Err(Error::SslMode(s.trim().to_string())),
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
/// planner never reads. The deployed model keeps them as they were
/// written, beside the name, adapter and tier, so a server that probes
/// a warehouse has what the deploy said and takes the rest, or a
/// replacement for any of it, from the request.
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
    /// BigQuery's service account, when its key is in `.zsql` as
    /// `private_key` and not in a file: the address the key signs for.
    pub client_email: Option<String>,
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
    /// Seconds a request for a run waits for its rows before it is
    /// answered with the run's id instead. Ten when not said. Read by a
    /// server that runs statements, ignored by the loader.
    pub query_wait_time: Option<i64>,
    /// How far the connection is trusted with a statement. Not a key of
    /// `datasources.yml`: whoever opens the connection says.
    pub access: Access,
}

impl Connection {
    /// Every datasource in a project's `datasources.yml`, in file order,
    /// each with what `.zsql` holds for it laid over its entry.
    pub fn read_all(project_dir: &Path) -> Result<Vec<Connection>> {
        let path = project_dir.join("datasources.yml");
        let text = std::fs::read_to_string(&path).map_err(|e| Error::Io(path.display().to_string(), e))?;
        let doc: serde_yaml::Value = serde_yaml::from_str(&text).map_err(|e| Error::Yaml(path.display().to_string(), e))?;
        let Some(map) = doc.as_mapping() else { return Ok(vec![]) };
        let mut out = Vec::new();
        for (key, d) in map {
            let Some(uid) = key.as_str() else { continue };
            // What this machine holds for it in `.zsql` goes over what
            // the file says, so a setting kept out of the repository
            // is still a setting.
            let mut entry = d.as_mapping().cloned().unwrap_or_default();
            overlay(&mut entry, crate::secret::local_entry(Some(project_dir), &uid.trim().to_lowercase()));
            out.push(Connection::from_value(uid, &serde_yaml::Value::Mapping(entry))?);
        }
        Ok(out)
    }

    /// One datasource from its settings, spelled as `datasources.yml`
    /// spells them: an entry of that file, or the same keys sent by a
    /// caller that holds the connection itself. Keys this does not know
    /// are read past; [`CONNECTION_KEYS`] is the list for a caller that
    /// would rather refuse a typo.
    pub fn from_value(uid: &str, d: &serde_yaml::Value) -> Result<Connection> {
        let s = |k: &str| d.get(k).and_then(|v| v.as_str()).map(|v| v.trim().to_string()).filter(|v| !v.is_empty());
        let i = |k: &str| d.get(k).and_then(|v| v.as_i64());
        Ok(Connection {
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
            client_email: s("client_email"),
            region: s("region"),
            workgroup: s("workgroup"),
            output_location: s("output_location").or_else(|| s("s3_output_location")),
            profile: s("profile"),
            cache_ttl: i("cache_ttl"),
            query_wait_time: i("query_wait_time"),
            access: Access::default(),
        })
    }

    /// Points this connection at another catalog: the level above a
    /// schema, which each warehouse spells its own way and some do not
    /// have. On Postgres, Redshift, SQL Server, Snowflake and Trino it
    /// is the database; on Databricks and Athena the catalog; on
    /// BigQuery the project. MySQL and ClickHouse call their one level
    /// a database and it is the schema here; Druid has schemas and
    /// nothing above them; SQLite and DuckDB are a file. Those five refuse a catalog rather than read past it.
    pub fn in_catalog(&mut self, catalog: &str) -> Result<()> {
        let catalog = Some(catalog.trim().to_string());
        match self.adapter.as_str() {
            "postgres" | "redshift" | "sqlserver" | "snowflake" | "trino" => self.database = catalog,
            "databricks" | "athena" => self.catalog = catalog,
            "bigquery" => self.project = catalog,
            "mysql" => return Err(Error::NoCatalogs("MySQL")),
            "clickhouse" => return Err(Error::NoCatalogs("ClickHouse")),
            "druid" => return Err(Error::NoCatalogs("Druid")),
            "sqlite" => return Err(Error::NoCatalogs("SQLite")),
            "duckdb" => return Err(Error::NoCatalogs("DuckDB")),
            other => return Err(Error::Unsupported(other.to_string())),
        }
        Ok(())
    }

    /// One datasource by its key, as `datasources.yml` spells it.
    pub fn read(project_dir: &Path, uid: &str) -> Result<Connection> {
        let want = uid.trim().to_lowercase();
        Connection::read_all(project_dir)?.into_iter().find(|c| c.uid == want).ok_or_else(|| Error::NoDatasource(uid.to_string()))
    }

    pub(crate) fn need(&self, value: &Option<String>, what: &'static str) -> Result<String> {
        value.clone().ok_or_else(|| Error::Missing(self.uid.clone(), what))
    }

    /// How long a request for a run waits for its rows, from
    /// `query_wait_time` or ten seconds.
    pub fn wait(&self) -> std::time::Duration {
        std::time::Duration::from_secs(self.query_wait_time.filter(|s| *s >= 0).unwrap_or(10) as u64)
    }

    /// The statement timeout, from `query_timeout` or a default that is
    /// generous for a catalog read and short of any warehouse's own.
    pub fn timeout(&self) -> std::time::Duration {
        let secs = self.query_timeout.filter(|s| *s > 0).unwrap_or(60);
        std::time::Duration::from_secs(secs.min(3600) as u64)
    }
}

/// Every key [`Connection::from_value`] reads, aliases included, for a
/// caller that refuses a key it would otherwise read past.
pub const CONNECTION_KEYS: [&str; 37] = [
    "adapter",
    "host",
    "port",
    "database",
    "schema",
    "dataset",
    "username",
    "url",
    "file",
    "query_timeout",
    "sslmode",
    "ssl_mode",
    "ca_file",
    "sslrootcert",
    "ssl_ca",
    "account",
    "account_identifier",
    "warehouse",
    "role",
    "private_key_file",
    "private_key_path",
    "catalog",
    "client_id",
    "oauth_client_id",
    "project",
    "project_id",
    "location",
    "key_file",
    "credentials_file",
    "client_email",
    "region",
    "workgroup",
    "output_location",
    "s3_output_location",
    "profile",
    "cache_ttl",
    "query_wait_time",
];

/// The settings that are numbers; every other one is text.
pub const NUMBERS: [&str; 4] = ["port", "query_timeout", "cache_ttl", "query_wait_time"];

/// The settings with more than one spelling, the one read first first.
pub const ALIASES: [&[&str]; 9] = [
    &["schema", "dataset"],
    &["sslmode", "ssl_mode"],
    &["ca_file", "sslrootcert", "ssl_ca"],
    &["account", "account_identifier"],
    &["private_key_file", "private_key_path"],
    &["client_id", "oauth_client_id"],
    &["project", "project_id"],
    &["key_file", "credentials_file"],
    &["output_location", "s3_output_location"],
];

/// Lays one entry over another: each setting of `over` replaces that
/// setting in `entry` under any of its spellings, so a `project` laid
/// over a `project_id` is the one that is read.
pub fn overlay(entry: &mut serde_yaml::Mapping, over: serde_yaml::Mapping) {
    for (key, value) in over {
        if let Some(names) = key.as_str().and_then(|k| ALIASES.iter().find(|names| names.contains(&k))) {
            for name in names.iter() {
                entry.remove(*name);
            }
        }
        entry.insert(key, value);
    }
}
