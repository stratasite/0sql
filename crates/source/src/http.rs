//! The one HTTP client the HTTP adapters share. Eight of the thirteen
//! warehouses are a JSON API behind a bearer or a basic header, and what
//! differs between them is the request shape and the response shape, not
//! the socket. `ureq` is already in the tree for the CLI's own calls to
//! the server and is synchronous like the rest of this crate.
//!
//! TLS follows the connection's `sslmode`. Over HTTPS the scheme already
//! encrypts, so the ladder collapses to two questions: is a plain `http`
//! url allowed, and is the certificate checked. `require` and below do
//! not check it; `verify-ca` and `verify-full` check it against the
//! platform's roots and `ca_file`, name included — HTTPS has no rung
//! between those two.

use crate::{Connection, Result, SourceError, SslMode};
use std::sync::Arc;

/// An agent with the connection's statement timeout as its whole-call
/// limit, a connect timeout short of it, and the TLS its mode asks for.
pub(crate) fn agent(conn: &Connection) -> Result<ureq::Agent> {
    let mut tls_config = ureq::tls::TlsConfig::builder().provider(ureq::tls::TlsProvider::Rustls);
    if conn.sslmode.verifies() {
        if let Some(certs) = certificates(&conn.uid, conn.ca_file.as_deref())? {
            tls_config = tls_config.root_certs(ureq::tls::RootCerts::Specific(Arc::new(certs)));
        }
    } else {
        tls_config = tls_config.disable_verification(true);
    }
    Ok(ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_global(Some(conn.timeout()))
            .http_status_as_error(false)
            .tls_config(tls_config.build())
            .build(),
    ))
}

/// The roots as ureq takes them, which is certificates and not trust
/// anchors, so the Mozilla bundle cannot be handed over the way the
/// platform's store can. The rule: the platform's certificates plus
/// `ca_file` when the platform has any; `ca_file` alone when it has none
/// and one was given, since a private CA names a private warehouse; and
/// `None`, meaning ureq's own copy of the bundle, when there is neither.
fn certificates(uid: &str, ca_file: Option<&std::path::Path>) -> Result<Option<Vec<ureq::tls::Certificate<'static>>>> {
    let native = rustls_native_certs::load_native_certs();
    let mut out: Vec<ureq::tls::Certificate<'static>> =
        native.certs.iter().map(|c| ureq::tls::Certificate::from_der(c.as_ref()).to_owned()).collect();
    if let Some(path) = ca_file {
        out.extend(pem(uid, path)?);
    }
    Ok((!out.is_empty()).then_some(out))
}

/// The certificates in a PEM file, as ureq takes them.
fn pem(uid: &str, path: &std::path::Path) -> Result<Vec<ureq::tls::Certificate<'static>>> {
    use rustls::pki_types::pem::PemObject;
    let fail = |e: String| SourceError::CaFile(format!("{uid}: {}", path.display()), e);
    let text = std::fs::read(path).map_err(|e| fail(e.to_string()))?;
    let mut out = Vec::new();
    for cert in rustls::pki_types::CertificateDer::pem_slice_iter(&text) {
        let cert = cert.map_err(|e| fail(e.to_string()))?;
        out.push(ureq::tls::Certificate::from_der(cert.as_ref()).to_owned());
    }
    if out.is_empty() {
        return Err(fail("no certificates in the file".into()));
    }
    Ok(out)
}

/// The base URL for an adapter reached over HTTP: `url` as given, else
/// built from the host and the adapter's default port. Plain `http` is
/// refused when the mode insists on TLS, and noted when the mode is
/// `prefer`; the note is the second value.
pub(crate) fn base_url(conn: &Connection, default_port: u16, what: &'static str) -> Result<(String, Option<String>)> {
    let url = match &conn.url {
        Some(u) => u.clone(),
        None => {
            let host = conn.host.clone().ok_or_else(|| SourceError::Missing(conn.uid.clone(), "host or url"))?;
            let scheme = if conn.sslmode.insists() { "https" } else { "http" };
            format!("{scheme}://{host}:{}", conn.port.unwrap_or(default_port))
        }
    };
    let plain = url.to_lowercase().starts_with("http://");
    if plain && conn.sslmode.insists() {
        return Err(SourceError::PlainUrl(conn.uid.clone(), what));
    }
    let note = (plain && conn.sslmode == SslMode::Prefer)
        .then(|| format!("{}: sslmode is prefer and the url is plain http, so nothing is encrypted", conn.uid));
    Ok((url, note))
}

/// The body of a response, as text, with the status folded into an error
/// when the warehouse refused: a refusal's body is where it says why.
pub(crate) fn text(conn: &Connection, r: ureq::http::Response<ureq::Body>) -> Result<String> {
    let status = r.status();
    let body = r.into_body().read_to_string().map_err(|e| SourceError::Query(conn.uid.clone(), e.to_string()))?;
    if status.is_success() {
        Ok(body)
    } else {
        let why: String = body.trim().chars().take(400).collect();
        Err(SourceError::Query(conn.uid.clone(), format!("{status}: {why}")))
    }
}
