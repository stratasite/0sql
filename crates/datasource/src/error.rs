#[derive(Debug, thiserror::Error)]
pub enum Error {
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
    #[error("{0} has no schemas; read it without --schema")]
    NoSchemas(&'static str),
    #[error("{0} has no catalogs above its schemas; ask without a catalog")]
    NoCatalogs(&'static str),
    #[error("{0}: private key: {1}")]
    PrivateKey(String, String),
    #[error("datasource {0}: {1}")]
    CacheTtl(String, String),
}

pub type Result<T> = std::result::Result<T, Error>;

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
