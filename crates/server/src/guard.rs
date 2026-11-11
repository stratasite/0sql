//! What every request meets before its handler: the deploy token, when
//! the server has one and the request is a write; the admission cap on
//! its kind of work; and its deadline.
//!
//! There is no other authentication here. The server answers whoever
//! can reach it, the way a database on a private network does: who may
//! ask, as whom, and how often belongs to the service in front of it.

use crate::admission::{self, Class};
use crate::error::{retry_later, ApiError};
use crate::Shared;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Response};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;

/// The admission permit, carried on the request. The guard holds one
/// clone until it answers; a handler that does its work on the blocking
/// pool moves another into that work, so the permit is released when
/// the work ends and not when the deadline gives up on it. Otherwise
/// deploys that outlive their deadline would pile up past the cap.
#[derive(Clone)]
pub(crate) struct Held(#[allow(dead_code)] pub(crate) Arc<Option<OwnedSemaphorePermit>>);

pub(crate) async fn guard(State(app): State<Shared>, req: axum::extract::Request, next: axum::middleware::Next) -> Response {
    let class = admission::classify(req.uri().path());
    if class == Class::Exempt {
        return next.run(req).await;
    }
    // Before a seat is taken: a write without the token costs the
    // server nothing, and never waits behind a deploy to be refused.
    if admission::writes(req.method().as_str(), req.uri().path()) {
        if let Some(token) = &app.deploy_token {
            if let Err(e) = check_token(token, req.headers()) {
                let mut r = e.into_response();
                r.headers_mut().insert("www-authenticate", "Bearer realm=\"0sql\"".parse().expect("a header"));
                return r;
            }
        }
    }

    // Room to run, or come back later. The permit is held until the
    // handler has finished with it.
    let held = match app.admission.admit(class).await {
        Ok(p) => Held(Arc::new(p)),
        Err(secs) => return busy(secs, class),
    };
    let (mut parts, body) = req.into_parts();
    parts.extensions.insert(held.clone());
    let req = axum::extract::Request::from_parts(parts, body);
    let response = match tokio::time::timeout(admission::deadline(class), next.run(req)).await {
        Ok(r) => r,
        Err(_) => retry_later(
            StatusCode::SERVICE_UNAVAILABLE,
            "Timeout",
            format!("this request passed its {}s deadline", admission::deadline(class).as_secs()),
            0,
        ),
    };
    drop(held);
    response
}

/// `Authorization: Bearer <token>`, which is what the CLI sends once
/// `zsql auth` or `ZSQL_API_KEY` has given it one.
fn check_token(token: &str, headers: &HeaderMap) -> Result<(), ApiError> {
    let header = headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    let sent = match header.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
        _ => "",
    };
    if sent.is_empty() {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "this server takes a deploy token for deploys, tests and removals: Authorization: Bearer <token> (from the CLI: `zsql auth --api-key <token>`, or ZSQL_API_KEY)".into(),
        ));
    }
    if !same(sent.as_bytes(), token.as_bytes()) {
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "this deploy token is not valid".into()));
    }
    Ok(())
}

/// Equality that takes the same time wherever the first difference is,
/// so the answer's timing says nothing about how much of a guess was
/// right. The length is not hidden; a token's length is not its secret.
fn same(a: &[u8], b: &[u8]) -> bool {
    if a.len() != b.len() {
        return false;
    }
    let mut diff = 0u8;
    for (x, y) in a.iter().zip(b) {
        diff |= std::hint::black_box(x ^ y);
    }
    diff == 0
}

pub(crate) fn busy(secs: u64, class: Class) -> Response {
    retry_later(StatusCode::SERVICE_UNAVAILABLE, "Busy", format!("the server is full of {} work: retry in {secs}s", class.name()), secs)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: Option<&str>) -> HeaderMap {
        let mut h = HeaderMap::new();
        if let Some(v) = value {
            h.insert("authorization", v.parse().unwrap());
        }
        h
    }

    #[test]
    fn the_token_is_the_whole_bearer_value_and_nothing_near_it() {
        assert!(check_token("s3cret", &headers(Some("Bearer s3cret"))).is_ok());
        assert!(check_token("s3cret", &headers(Some("bearer  s3cret "))).is_ok(), "the scheme is matched without regard to case");
        for wrong in [None, Some(""), Some("s3cret"), Some("Basic s3cret"), Some("Bearer"), Some("Bearer s3cre"), Some("Bearer s3cret1")] {
            let e = check_token("s3cret", &headers(wrong)).err().unwrap_or_else(|| panic!("{wrong:?} was taken"));
            assert_eq!(e.0, StatusCode::UNAUTHORIZED, "{wrong:?}");
        }
    }

    #[test]
    fn equality_is_exact() {
        assert!(same(b"", b""));
        assert!(same(b"abc", b"abc"));
        assert!(!same(b"abc", b"abd"));
        assert!(!same(b"abc", b"ab"));
        assert!(!same(b"abc", b"abcd"));
    }
}
