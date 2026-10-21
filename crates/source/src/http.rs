//! The one HTTP client the HTTP adapters share. Eight of the thirteen
//! warehouses are a JSON API behind a bearer or a basic header, and what
//! differs between them is the request shape and the response shape, not
//! the socket. `ureq` is already in the tree for the CLI's own calls to
//! the server, is synchronous like the rest of this crate, and verifies
//! TLS on the platform roots with no switch to turn that off.

use crate::{Connection, Result, SourceError};

/// An agent with the connection's statement timeout as its whole-call
/// limit, and a connect timeout short of it.
pub(crate) fn agent(conn: &Connection) -> ureq::Agent {
    ureq::Agent::new_with_config(
        ureq::config::Config::builder()
            .timeout_connect(Some(std::time::Duration::from_secs(10)))
            .timeout_global(Some(conn.timeout()))
            .http_status_as_error(false)
            .build(),
    )
}

/// The base URL for an adapter reached over HTTP: `url` as given, else
/// built from the host and the adapter's default port, over plain HTTP.
/// A warehouse that wants TLS on an unusual port says so with `url`.
pub(crate) fn base_url(conn: &Connection, default_port: u16) -> Result<String> {
    if let Some(u) = &conn.url {
        return Ok(u.clone());
    }
    let host = conn.host.clone().ok_or_else(|| SourceError::Missing(conn.uid.clone(), "host or url"))?;
    Ok(format!("http://{host}:{}", conn.port.unwrap_or(default_port)))
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
