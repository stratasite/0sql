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
use std::time::{Duration, Instant};

/// The most a response body may be. A catalogue read of two hundred
/// thousand columns, or one Snowflake partition, can pass ureq's own
/// ten-megabyte default by a distance.
const MAX_BODY: u64 = 256 * 1024 * 1024;

/// How much longer than the statement timeout a single HTTP call may
/// take: the APIs hold a request open while a statement runs, up to
/// their own ceilings, and the poll loops own the budget after that.
const CALL_MARGIN: Duration = Duration::from_secs(60);

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
            .timeout_global(Some(conn.timeout() + CALL_MARGIN))
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
    let note = plain(conn, &url, what)?;
    Ok((url, note))
}

/// The body of a response, as text, with the status folded into an error
/// when the warehouse refused: a refusal's body is where it says why.
pub(crate) fn text(conn: &Connection, r: ureq::http::Response<ureq::Body>) -> Result<String> {
    let status = r.status();
    let body =
        r.into_body().with_config().limit(MAX_BODY).read_to_string().map_err(|e| SourceError::Query(conn.uid.clone(), e.to_string()))?;
    if status.is_success() {
        Ok(body)
    } else {
        let why: String = body.trim().chars().take(400).collect();
        Err(SourceError::Query(conn.uid.clone(), format!("{status}: {why}")))
    }
}

/// What a call is.
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum Method {
    Get,
    Post,
}

/// One call, the status and body back as they came: for an API whose
/// refusals carry a JSON message the adapter wants to read rather than
/// have folded.
pub(crate) fn call(
    conn: &Connection,
    agent: &ureq::Agent,
    method: Method,
    url: &str,
    headers: &[(&str, &str)],
    body: Option<&[u8]>,
) -> Result<(u16, String)> {
    let fail = |e: ureq::Error| SourceError::Query(conn.uid.clone(), crate::chain(&e));
    let response = match method {
        Method::Post => {
            let mut req = agent.post(url);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            req.send(body.unwrap_or_default()).map_err(fail)?
        }
        Method::Get => {
            let mut req = agent.get(url);
            for (k, v) in headers {
                req = req.header(*k, *v);
            }
            req.call().map_err(fail)?
        }
    };
    let status = response.status().as_u16();
    let text = response
        .into_body()
        .with_config()
        .limit(MAX_BODY)
        .read_to_string()
        .map_err(|e| SourceError::Query(conn.uid.clone(), e.to_string()))?;
    Ok((status, text))
}

/// A refusal's reason: the body's `message` (or `error_description`,
/// `error`, or `error.message`) when it is JSON, else the body, cut to
/// a line's worth.
pub(crate) fn reason(text: &str) -> String {
    let v: serde_json::Value = serde_json::from_str(text).unwrap_or_default();
    let found = [v.get("message"), v.get("error_description"), v.pointer("/error/message"), v.get("error"), v.get("Message")]
        .into_iter()
        .flatten()
        .find_map(|m| m.as_str().map(str::to_string));
    found.unwrap_or_else(|| text.trim().chars().take(400).collect())
}

/// An OAuth token endpoint: the form POSTed, the access token and when
/// it stops being good. A lifetime the answer does not say, or says
/// absurdly, is an hour.
pub(crate) fn token(
    conn: &Connection,
    agent: &ureq::Agent,
    url: &str,
    authorization: Option<&str>,
    form_body: &str,
) -> Result<(String, Instant)> {
    let mut headers = vec![
        ("Content-Type", "application/x-www-form-urlencoded"),
        ("Accept", "application/json"),
        ("User-Agent", concat!("zsql/", env!("CARGO_PKG_VERSION"))),
    ];
    if let Some(a) = authorization {
        headers.push(("Authorization", a));
    }
    let (status, text) = call(conn, agent, Method::Post, url, &headers, Some(form_body.as_bytes()))?;
    if status != 200 {
        return Err(SourceError::Connect(conn.uid.clone(), format!("token: {status}: {}", reason(&text))));
    }
    let v: serde_json::Value = serde_json::from_str(&text).unwrap_or_default();
    let token = v
        .get("access_token")
        .and_then(|t| t.as_str())
        .map(str::to_string)
        .ok_or_else(|| SourceError::Connect(conn.uid.clone(), "token: the answer carried no access_token".into()))?;
    let secs = v.get("expires_in").and_then(|e| e.as_u64()).filter(|s| (1..=86_400).contains(s)).unwrap_or(3600);
    Ok((token, Instant::now() + Duration::from_secs(secs)))
}

/// Whether a base URL may be plain `http`: refused when the mode insists
/// on TLS, noted when the mode is `prefer`.
pub(crate) fn plain(conn: &Connection, url: &str, what: &'static str) -> Result<Option<String>> {
    let plain = url.to_lowercase().starts_with("http://");
    if plain && conn.sslmode.insists() {
        return Err(SourceError::PlainUrl(conn.uid.clone(), what));
    }
    Ok((plain && conn.sslmode == SslMode::Prefer)
        .then(|| format!("{}: sslmode is prefer and the url is plain http, so nothing is encrypted", conn.uid)))
}

/// A JSON cell as the text a consumer will match on: strings as they
/// are, numbers and booleans as they print, null as none, and anything
/// nested as its JSON.
pub(crate) fn cell(v: serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Null => None,
        serde_json::Value::String(s) => Some(s),
        other => Some(other.to_string()),
    }
}

/// A URL-encoded form body, for a token endpoint.
pub(crate) fn form(fields: &[(&str, &str)]) -> String {
    fields.iter().map(|(k, v)| format!("{}={}", urlencode(k), urlencode(v))).collect::<Vec<_>>().join("&")
}

pub(crate) fn urlencode(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}
