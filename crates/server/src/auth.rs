//! Who is calling, and the guard every request meets: the key or the
//! session cookie behind it, the limits on what it may ask for, the
//! admission cap on its kind of work, and its deadline.

use crate::error::{internal, key_required, retry_later, ApiError};
use crate::limits::{self, Class};
use crate::{mcp, App, Shared};
use axum::extract::{ConnectInfo, FromRequestParts, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde_json::Value;
use std::net::{IpAddr, SocketAddr};
use std::sync::Arc;
use tokio::sync::OwnedSemaphorePermit;
use zsql_accounts::{Principal, Role, Store};

/// The caller: a user or a query key when accounts are on, nothing when off.
pub(crate) struct Auth(pub(crate) Option<Principal>);

/// What the guard worked out about the caller, carried on the request so
/// that authentication happens once however many extractors ask for it.
#[derive(Clone)]
struct Caller(pub(crate) Option<Principal>);

/// The admission permit, carried on the request. The guard holds one
/// clone until it answers; a handler that does its work on the blocking
/// pool moves another into that work, so the permit is released when
/// the work ends and not when the deadline gives up on it. Otherwise
/// deploys that outlive their deadline would pile up past the cap.
#[derive(Clone)]
pub(crate) struct Held(#[allow(dead_code)] pub(crate) Arc<Option<OwnedSemaphorePermit>>);

/// Who is calling: the key in the Authorization header, else the session
/// cookie the web app holds.
pub(crate) fn authenticate(app: &App, parts: &Parts) -> Result<Option<Principal>, ApiError> {
    let Some(store) = &app.accounts else { return Ok(None) };
    let header = parts.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("").trim();
    // `Bearer <key>` is the form; a bare key is taken too, since a
    // connector's header box is typed by hand and the key names its own
    // kind. Any other scheme is nothing.
    let secret = match header.split_once(' ') {
        Some((scheme, rest)) if scheme.eq_ignore_ascii_case("bearer") => rest.trim(),
        None if header.starts_with("zsk_") || header.starts_with("zqk_") => header,
        _ => "",
    };
    if !secret.is_empty() {
        return match store.authenticate(secret).map_err(internal)? {
            Some(p) => Ok(Some(p)),
            None => Err(ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "this API key is not valid".into())),
        };
    }
    // The MCP endpoint takes a key and nothing else. A session cookie
    // is never read there, so a signed-in browser cannot be made to
    // speak MCP as its user, whatever page it is on.
    if mcp::is_endpoint(parts.uri.path()) {
        return Err(ApiError(
            StatusCode::UNAUTHORIZED,
            "Unauthorized",
            "the MCP endpoint takes an API key: Authorization: Bearer <key>".into(),
        ));
    }
    // The web app: a session cookie. Anything but a read must also
    // carry the header the app's own scripts send, which a cross-site
    // form cannot, so a stolen navigation cannot act as the user.
    if let Some(token) = cookie(parts, SESSION_COOKIE) {
        let safe = matches!(parts.method.as_str(), "GET" | "HEAD" | "OPTIONS");
        if !safe && parts.headers.get("x-requested-with").and_then(|v| v.to_str().ok()) != Some("zsql") {
            return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "this request did not come from the app".into()));
        }
        if let Some(p) = store.session(&token).map_err(internal)? {
            return Ok(Some(p));
        }
    }
    Err(key_required())
}

impl FromRequestParts<Shared> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, ApiError> {
        // The guard in front of every route has already decided; this
        // reads that back. The fallback keeps the extractor correct on
        // its own, for a route reached without the guard.
        if let Some(Caller(who)) = parts.extensions.get::<Caller>() {
            return Ok(Auth(who.clone()));
        }
        authenticate(app, parts).map(Auth)
    }
}

/// Everything a request meets before its handler: the limit on what its
/// caller may ask for, the cap on how much of its kind of work may run
/// at once, and a deadline. Authentication happens here, once, and
/// travels on the request. See `docs/rate-limiting.md`.
pub(crate) async fn guard(
    State(app): State<Shared>,
    ConnectInfo(peer): ConnectInfo<SocketAddr>,
    req: axum::extract::Request,
    next: axum::middleware::Next,
) -> Response {
    let class = limits::classify(req.method().as_str(), req.uri().path());
    if class == Class::Exempt {
        return next.run(req).await;
    }
    let (mut parts, body) = req.into_parts();
    let addr = client_addr(app.limits.cfg.trusted_proxy, &parts.headers, peer.ip());

    // The routes that cost before anyone is identified are counted
    // against the address they came from.
    if let Some(gate) = address_gate(&parts) {
        let d = app.limits.address(addr, gate);
        if app.limits.enforced(&d, &format!("{addr} on {}", gate_name(gate))) {
            return too_many(&d);
        }
    }

    let mcp = mcp::is_endpoint(parts.uri.path());

    let mut who = None;
    // The MCP handshake may come without a key: a hosted client checks
    // that a server is there and what it speaks before its user's
    // credential is applied. A keyless caller gets the handshake and
    // nothing else; mcp::call refuses it before any tool runs, and a
    // key that is presented and wrong is still refused here.
    let handshake = mcp && !parts.headers.contains_key("authorization");
    if handshake {
        let d = app.limits.address(addr, limits::Gate::Handshake);
        if app.limits.enforced(&d, &format!("{addr} on the MCP handshake without a key")) {
            return too_many(&d);
        }
    }
    if limits::authenticates(parts.uri.path()) && !handshake {
        match authenticate(&app, &parts) {
            Ok(p) => who = p,
            Err(e) => {
                // Answering a key that is not valid is cheap but not
                // free, and a stream of them is someone guessing.
                if e.0 == StatusCode::UNAUTHORIZED {
                    let d = app.limits.address(addr, limits::Gate::BadKey);
                    if app.limits.enforced(&d, &format!("{addr} sending keys that are not valid")) {
                        return too_many(&d);
                    }
                }
                let mut r = e.into_response();
                if mcp {
                    mcp::challenge(&mut r);
                    mcp::note_refusal(r.status(), &parts.headers);
                }
                return r;
            }
        }
    }
    parts.extensions.insert(Caller(who.clone()));

    let allowance = who.as_ref().map(|p| {
        let key_id = match p {
            Principal::QueryKey { key, .. } => Some(key.id),
            Principal::User { .. } => None,
        };
        app.limits.principal(p.account().id, key_id, class)
    });
    if let (Some(d), Some(p)) = (&allowance, &who) {
        if app.limits.enforced(d, &format!("{} on {}", p.describe(), class.name())) {
            return too_many(d);
        }
    }

    // Room to run, or come back later. The permit is held until the
    // handler has finished with it.
    let held = match app.admission.admit(class).await {
        Ok(p) => Held(Arc::new(p)),
        Err(secs) => return busy(secs, class),
    };
    parts.extensions.insert(held.clone());
    let req = axum::extract::Request::from_parts(parts, body);
    let mut response = match tokio::time::timeout(limits::deadline(class), next.run(req)).await {
        Ok(r) => r,
        Err(_) => retry_later(
            StatusCode::SERVICE_UNAVAILABLE,
            "Timeout",
            format!("this request passed its {}s deadline", limits::deadline(class).as_secs()),
            0,
        ),
    };
    drop(held);
    if let Some(d) = allowance {
        rate_headers(&mut response, &d);
    }
    response
}

fn gate_name(gate: limits::Gate) -> &'static str {
    match gate {
        limits::Gate::Login => "signing in",
        limits::Gate::Signup => "signing up",
        limits::Gate::Invite => "redeeming a link",
        limits::Gate::Reset => "asking for a password reset",
        limits::Gate::BadKey => "keys that are not valid",
        limits::Gate::Handshake => "the MCP handshake without a key",
    }
}

/// The three routes that spend an argon2 hash before anyone is known.
fn address_gate(parts: &Parts) -> Option<limits::Gate> {
    if parts.method != axum::http::Method::POST {
        return None;
    }
    match parts.uri.path().trim_end_matches('/') {
        "/login" => Some(limits::Gate::Login),
        "/signup" => Some(limits::Gate::Signup),
        "/invites/accept" | "/password-reset/accept" | "/verify-email" => Some(limits::Gate::Invite),
        "/password-reset" => Some(limits::Gate::Reset),
        _ => None,
    }
}

/// The address a per-address limit counts against.
///
/// The socket peer, unless the server has been told a proxy is in front
/// of it, and then two headers in order of how much they can be
/// trusted. `CF-Connecting-IP` first: Cloudflare sets it to the single
/// address that reached its edge, and being one value it cannot be
/// turned into a list to hide behind.
///
/// Failing that, the **rightmost** hop of `X-Forwarded-For`. That is
/// the opposite of the usual advice, and deliberately so. Every proxy
/// appends, so the leftmost entry is whatever the caller sent and is
/// forgeable; the rightmost is the one the nearest proxy wrote itself.
/// Cloudflare's edge and `cloudflared` both append, so the rightmost is
/// the real visitor there too.
///
/// Neither header is read unless `ZSQL_TRUSTED_PROXY` says a proxy is
/// really there. Reading them uninvited would let every caller choose
/// its own bucket, which is how per-address limits get bypassed.
fn client_addr(trusted_proxy: bool, headers: &axum::http::HeaderMap, peer: IpAddr) -> IpAddr {
    if trusted_proxy {
        if let Some(ip) = headers.get("cf-connecting-ip").and_then(|v| v.to_str().ok()).and_then(|v| v.trim().parse().ok()) {
            return ip;
        }
        if let Some(hops) = headers.get("x-forwarded-for").and_then(|v| v.to_str().ok()) {
            if let Some(ip) = hops.split(',').map(str::trim).rfind(|h| !h.is_empty()).and_then(|h| h.parse().ok()) {
                return ip;
            }
        }
    }
    peer
}

/// What is left of an allowance, so a client can pace itself before it
/// is refused rather than after.
fn rate_headers(res: &mut Response, d: &limits::Decision) {
    if d.limit == 0.0 || !d.limit.is_finite() {
        return;
    }
    for (name, value) in [("ratelimit-limit", d.limit as u64), ("ratelimit-remaining", d.remaining as u64), ("ratelimit-reset", d.reset)] {
        if let Ok(v) = value.to_string().parse() {
            res.headers_mut().insert(name, v);
        }
    }
}

pub(crate) fn too_many(d: &limits::Decision) -> Response {
    let mut res =
        retry_later(StatusCode::TOO_MANY_REQUESTS, "RateLimited", format!("too many requests: retry in {}s", d.retry_after), d.retry_after);
    rate_headers(&mut res, d);
    res
}

pub(crate) fn busy(secs: u64, class: Class) -> Response {
    retry_later(StatusCode::SERVICE_UNAVAILABLE, "Busy", format!("the server is full of {} work: retry in {secs}s", class.name()), secs)
}

pub(crate) const SESSION_COOKIE: &str = "zsql_session";
pub(crate) const SESSION_DAYS: i64 = 30;

pub(crate) fn cookie(parts: &Parts, name: &str) -> Option<String> {
    cookie_in(&parts.headers, name)
}

pub(crate) fn cookie_in(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let header = headers.get("cookie")?.to_str().ok()?;
    header.split(';').map(str::trim).find_map(|kv| kv.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')).map(str::to_string))
}

/// A response that sets (or clears) the session cookie.
pub(crate) fn with_session_cookie(app: &App, body: Value, token: Option<&str>) -> Response {
    let secure = if app.secure_cookies { "; Secure" } else { "" };
    let value = match token {
        Some(t) => format!("{SESSION_COOKIE}={t}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}", SESSION_DAYS * 86400),
        None => format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"),
    };
    let mut r = Json(body).into_response();
    r.headers_mut().insert("set-cookie", value.parse().expect("cookie header"));
    r
}

/// The user behind a request, for actions only people may take.
pub(crate) fn user_of(who: &Option<Principal>) -> Result<(&zsql_accounts::User, &zsql_accounts::Account, Role), ApiError> {
    match who {
        Some(Principal::User { user, account, role, .. }) => Ok((user, account, *role)),
        Some(Principal::QueryKey { .. }) => {
            Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "a query key cannot do this; use your personal key".into()))
        }
        None => Err(ApiError(StatusCode::NOT_FOUND, "NotFound", "accounts are off on this server".into())),
    }
}

pub(crate) fn store_of(app: &App) -> Result<&Store, ApiError> {
    app.accounts.as_ref().ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "NotFound", "accounts are off on this server".into()))
}

pub(crate) fn admin_of(who: &Option<Principal>) -> Result<(&zsql_accounts::User, &zsql_accounts::Account), ApiError> {
    let (user, account, role) = user_of(who)?;
    if role != Role::Admin {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "this needs an account admin".into()));
    }
    Ok((user, account))
}

#[cfg(test)]
mod tests {
    use super::*;
    use axum::http::{HeaderMap, HeaderName};

    fn headers(pairs: &[(&str, &str)]) -> HeaderMap {
        let mut h = HeaderMap::new();
        for (k, v) in pairs {
            h.insert(k.parse::<HeaderName>().unwrap(), v.parse().unwrap());
        }
        h
    }

    fn ip(s: &str) -> IpAddr {
        s.parse().unwrap()
    }

    /// Which address a request is counted against decides whether the
    /// login and sign-up limits mean anything, so each way of getting
    /// it wrong is worth a line here.
    #[test]
    fn the_address_a_request_is_counted_against() {
        let peer = ip("10.0.0.1");
        let visitor = ip("203.0.113.9");

        // No proxy in front: headers are not read at all, however
        // confidently they are sent.
        let forged = headers(&[("x-forwarded-for", "1.2.3.4"), ("cf-connecting-ip", "5.6.7.8")]);
        assert_eq!(client_addr(false, &forged, peer), peer);

        // Behind a proxy, Cloudflare's single-value header wins.
        let both = headers(&[("x-forwarded-for", "1.2.3.4, 203.0.113.9"), ("cf-connecting-ip", "198.51.100.2")]);
        assert_eq!(client_addr(true, &both, peer), ip("198.51.100.2"));

        // Without it, the rightmost hop: the one the nearest proxy
        // wrote. The leftmost is whatever the caller sent.
        let appended = headers(&[("x-forwarded-for", "1.2.3.4, 203.0.113.9")]);
        assert_eq!(client_addr(true, &appended, peer), visitor);

        // A caller stuffing the header cannot reach past the entry the
        // proxy appends after it.
        let stuffed = headers(&[("x-forwarded-for", "9.9.9.9, 9.9.9.9, 9.9.9.9, 203.0.113.9")]);
        assert_eq!(client_addr(true, &stuffed, peer), visitor);

        // Nonsense in the header falls back rather than failing.
        assert_eq!(client_addr(true, &headers(&[("x-forwarded-for", "not-an-ip")]), peer), peer);
        assert_eq!(client_addr(true, &headers(&[("x-forwarded-for", "")]), peer), peer);
        assert_eq!(client_addr(true, &headers(&[("cf-connecting-ip", "nope")]), peer), peer);
        assert_eq!(client_addr(true, &HeaderMap::new(), peer), peer);

        // Spacing and IPv6 are both ordinary.
        assert_eq!(client_addr(true, &headers(&[("x-forwarded-for", " 1.2.3.4 ,  203.0.113.9 ")]), peer), visitor);
        assert_eq!(client_addr(true, &headers(&[("cf-connecting-ip", "2001:db8::1")]), peer), ip("2001:db8::1"));
    }
}
