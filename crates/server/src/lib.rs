//! The server: deploy a project directory (as a tar.gz), then
//! plan specs against it. Deployed models live in memory and under a data
//! directory so a restart reloads them.
//!
//! With `ZSQL_ACCOUNTS=on` the server is multi-tenant: every request
//! carries a key (`Authorization: Bearer ...`) that names a user or a
//! query key in an account, deployments are scoped to the account, and
//! the accounts endpoints manage members, keys and project access. Off
//! (the default, the self-hosted mode), there is one tenant and no auth.
//!
//! ```text
//! POST /projects/{uid}/branches/{branch}/deploy        body: tar.gz of the project; tests run, results in the answer
//! POST /projects/{uid}/branches/{branch}/validate      the same, keeping nothing
//! POST /projects/{uid}/branches/{branch}/plan          {"spec": {...}, "context": {...}}
//! POST /projects/{uid}/branches/{branch}/test          run the deployed tests/*.yml again
//! GET  /projects/{uid}/branches/{branch}               summary and warnings
//! GET  /projects/{uid}/branches/{branch}/fields        ?q=text&hidden=true
//! GET  /projects/{uid}/branches/{branch}/tables
//! GET  /projects/{uid}/branches/{branch}/values/{dim}
//! DELETE /projects/{uid}/branches/{branch}
//! GET  /projects                                       every deployed branch
//! POST /mcp                                            the same reads, as MCP tools (see mcp.rs)
//! GET  /healthz
//! ```

pub mod limits;
pub mod mail;
mod mcp;

pub use mcp::Settings as McpSettings;

use axum::body::Bytes;
use axum::extract::{ConnectInfo, Extension, FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use limits::{Admission, Class, Limiter};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::net::{IpAddr, SocketAddr};
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Duration;
use tokio::sync::OwnedSemaphorePermit;
use zsql_accounts::{Level, Principal, Project, Role, Store, Verb};
use zsql_dialect::Dialects;
use zsql_semantic::FieldKind;
use zsql_service::{ExplainResponse, ExploreRequest, ExploreResponse, Kind, Model, ServiceError, SqlRequest, SqlResponse, Trust};
use zsql_spec::SecurityContext;

struct App {
    /// Shared by every deployment for the life of the process, so each
    /// planner can borrow it for as long as its universe.
    dialects: &'static Dialects,
    data_dir: PathBuf,
    /// Deployments by `[account/]project/branch`.
    models: RwLock<HashMap<String, Arc<Model>>>,
    /// Accounts, users and keys; `None` in the single-tenant mode.
    accounts: Option<Store>,
    signup_open: bool,
    /// Session cookies marked Secure, for a server behind https.
    secure_cookies: bool,
    /// What each caller may ask for, and how fast.
    limits: Limiter,
    /// How much of each kind of work may be in the server at once.
    admission: Admission,
    /// How many branches an account may keep deployed.
    caps: Caps,
    /// How the verification and reset links get out.
    mailer: mail::Mailer,
    /// What MCP clients are told, and which tools they may call.
    mcp: mcp::Settings,
    /// One deploy writes at a time: two for the same branch would race
    /// each other's renames while swapping in.
    deploying: std::sync::Mutex<()>,
}

/// How many deployed branches are allowed.
///
/// This is a memory limit wearing a product's clothes: a deployed model
/// is held for the life of the process and nothing evicts it, so the
/// number of them is the number the machine can hold. Both counts are
/// needed: per project is the one a person feels and reasons about, and
/// per account is the one that actually bounds the process, since an
/// account may make as many projects as it likes.
///
/// Only with accounts on. A single-tenant server is one operator on
/// their own machine, who does not need protecting from themselves.
pub struct Caps {
    pub per_project: usize,
    pub per_account: usize,
}

impl Default for Caps {
    fn default() -> Self {
        Caps { per_project: 5, per_account: 20 }
    }
}

// ---------------------------------------------------------------- tenancy

/// The caller: a user or a query key when accounts are on, nothing when off.
struct Auth(Option<Principal>);

/// What the guard worked out about the caller, carried on the request so
/// that authentication happens once however many extractors ask for it.
#[derive(Clone)]
struct Caller(Option<Principal>);

/// The admission permit, carried on the request. The guard holds one
/// clone until it answers; a handler that does its work on the blocking
/// pool moves another into that work, so the permit is released when
/// the work ends and not when the deadline gives up on it. Otherwise
/// deploys that outlive their deadline would pile up past the cap.
#[derive(Clone)]
struct Held(#[allow(dead_code)] Arc<Option<OwnedSemaphorePermit>>);

/// Who is calling: the key in the Authorization header, else the session
/// cookie the web app holds.
fn authenticate(app: &App, parts: &Parts) -> Result<Option<Principal>, ApiError> {
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
    Err(ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "an API key is required: Authorization: Bearer <key>".into()))
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

// ------------------------------------------------------------- the guard

/// Everything a request meets before its handler: the limit on what its
/// caller may ask for, the cap on how much of its kind of work may run
/// at once, and a deadline. Authentication happens here, once, and
/// travels on the request. See `docs/rate-limiting.md`.
async fn guard(
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

/// An answer that says when to come back.
fn retry_later(status: StatusCode, class: &'static str, message: String, secs: u64) -> Response {
    let mut res = ApiError(status, class, message).into_response();
    if secs > 0 {
        if let Ok(v) = secs.to_string().parse() {
            res.headers_mut().insert("retry-after", v);
        }
    }
    res
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

fn too_many(d: &limits::Decision) -> Response {
    let mut res =
        retry_later(StatusCode::TOO_MANY_REQUESTS, "RateLimited", format!("too many requests: retry in {}s", d.retry_after), d.retry_after);
    rate_headers(&mut res, d);
    res
}

fn busy(secs: u64, class: Class) -> Response {
    retry_later(StatusCode::SERVICE_UNAVAILABLE, "Busy", format!("the server is full of {} work: retry in {secs}s", class.name()), secs)
}

/// Runs work that would hold a runtime thread for seconds on a blocking
/// thread instead, so a deploy or a password hash never stalls the
/// planning the rest of the server is doing.
async fn blocking<T: Send + 'static>(work: impl FnOnce() -> Result<T, ApiError> + Send + 'static) -> Result<T, ApiError> {
    match tokio::task::spawn_blocking(work).await {
        Ok(result) => result,
        Err(e) => Err(ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", format!("this request did not finish: {e}"))),
    }
}

const SESSION_COOKIE: &str = "zsql_session";
const SESSION_DAYS: i64 = 30;

fn cookie(parts: &Parts, name: &str) -> Option<String> {
    cookie_in(&parts.headers, name)
}

fn cookie_in(headers: &axum::http::HeaderMap, name: &str) -> Option<String> {
    let header = headers.get("cookie")?.to_str().ok()?;
    header.split(';').map(str::trim).find_map(|kv| kv.strip_prefix(name).and_then(|rest| rest.strip_prefix('=')).map(str::to_string))
}

/// A response that sets (or clears) the session cookie.
fn with_session_cookie(app: &App, body: Value, token: Option<&str>) -> Response {
    let secure = if app.secure_cookies { "; Secure" } else { "" };
    let value = match token {
        Some(t) => format!("{SESSION_COOKIE}={t}; Path=/; HttpOnly; SameSite=Lax; Max-Age={}{secure}", SESSION_DAYS * 86400),
        None => format!("{SESSION_COOKIE}=; Path=/; HttpOnly; SameSite=Lax; Max-Age=0{secure}"),
    };
    let mut r = Json(body).into_response();
    r.headers_mut().insert("set-cookie", value.parse().expect("cookie header"));
    r
}

/// Where a request's deployments live: the account's slice, or everything.
struct Tenant {
    scope: String,
    project: Option<Project>,
}

impl Tenant {
    fn key(&self, uid: &str, branch: &str) -> String {
        format!("{}{uid}/{branch}", self.scope)
    }
    fn dir(&self, app: &App, uid: &str, branch: &str) -> PathBuf {
        app.data_dir.join(&self.scope).join(uid).join(branch)
    }
}

fn scope_of(who: &Option<Principal>) -> String {
    who.as_ref().map(|p| format!("{}/", p.account().uid)).unwrap_or_default()
}

fn internal(e: zsql_accounts::Error) -> ApiError {
    ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Accounts", e.to_string())
}

fn accounts_error(e: zsql_accounts::Error) -> ApiError {
    match e {
        zsql_accounts::Error::NotFound(m) => ApiError(StatusCode::NOT_FOUND, "NotFound", m),
        zsql_accounts::Error::Denied(m) => ApiError(StatusCode::FORBIDDEN, "Forbidden", m),
        zsql_accounts::Error::Invalid(m) => ApiError(StatusCode::BAD_REQUEST, "Invalid", m),
        zsql_accounts::Error::Db(e) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Accounts", e.to_string()),
    }
}

/// Checks the caller may do `verb` on the project's branch and answers
/// with the tenant to look deployments up in. In single-tenant mode
/// everything is allowed.
fn gate(app: &App, who: &Option<Principal>, uid: &str, branch: &str, verb: Verb) -> Result<Tenant, ApiError> {
    check_uid(uid)?;
    // Project-level routes come through with no branch at all.
    if !branch.is_empty() {
        check_branch(branch)?;
    }
    let scope = scope_of(who);
    let (Some(store), Some(p)) = (&app.accounts, who) else { return Ok(Tenant { scope, project: None }) };
    let project = store.project(p.account().id, uid).map_err(internal)?.ok_or_else(|| not_found(uid, branch))?;
    store.authorize(p, &project, branch, verb).map_err(accounts_error)?;
    Ok(Tenant { scope, project: Some(project) })
}

/// The security context a plan runs under.
///
/// A query key whose grant on this project carries a context bound by
/// the project's owner plans as that context and nothing else. A
/// context in the request is refused then, even an identical one, so
/// there is never a question of which won. Everyone else plans as the
/// context they sent, as before: an application acting for many users
/// sends each one's.
fn effective_context(t: &Tenant, who: &Option<Principal>, sent: Option<SecurityContext>) -> Result<Option<SecurityContext>, ApiError> {
    let (Some(Principal::QueryKey { grants, .. }), Some(project)) = (who, &t.project) else { return Ok(sent) };
    let Some(bound) = grants.iter().find(|g| g.project_id == project.id).and_then(|g| g.context.as_ref()) else { return Ok(sent) };
    if sent.is_some() {
        return Err(ApiError(
            StatusCode::BAD_REQUEST,
            "ContextBound",
            format!("this key's context on {} is bound by the project's owner; do not send one", project.uid),
        ));
    }
    let ctx: SecurityContext = serde_json::from_value(bound.clone()).map_err(|e| {
        ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Accounts", format!("the context bound on {} does not parse: {e}", project.uid))
    })?;
    Ok(Some(ctx))
}

/// The user behind a request, for actions only people may take.
fn user_of(who: &Option<Principal>) -> Result<(&zsql_accounts::User, &zsql_accounts::Account, Role), ApiError> {
    match who {
        Some(Principal::User { user, account, role, .. }) => Ok((user, account, *role)),
        Some(Principal::QueryKey { .. }) => {
            Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "a query key cannot do this; use your personal key".into()))
        }
        None => Err(ApiError(StatusCode::NOT_FOUND, "NotFound", "accounts are off on this server".into())),
    }
}

fn store_of(app: &App) -> Result<&Store, ApiError> {
    app.accounts.as_ref().ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "NotFound", "accounts are off on this server".into()))
}

fn admin_of(who: &Option<Principal>) -> Result<(&zsql_accounts::User, &zsql_accounts::Account), ApiError> {
    let (user, account, role) = user_of(who)?;
    if role != Role::Admin {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "this needs an account admin".into()));
    }
    Ok((user, account))
}

type Shared = Arc<App>;

/// An error as the API reports it: a status, a class the goldens would
/// name, and a message.
struct ApiError(StatusCode, &'static str, String);

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (self.0, Json(json!({ "error": { "class": self.1, "message": self.2 } }))).into_response()
    }
}

/// The longest project uid or branch name accepted. Both become one
/// directory name, and the limit keeps them well inside what any
/// filesystem allows.
const NAME_LIMIT: usize = 100;

/// A project uid as the URL carries it must be exactly the slug the
/// account store would keep (lower-case letters, digits, `-`, `_`),
/// because it names a directory under the account's slice of the data
/// directory and a key in the models map. The store would find
/// `Customer-Service` by lower-casing it, but the directory and the key
/// would not be the canonical ones.
fn check_uid(uid: &str) -> Result<(), ApiError> {
    let ok = !uid.is_empty()
        && uid.len() <= NAME_LIMIT
        && uid.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'-' || b == b'_');
    if ok {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid",
            format!("'{uid}' is not a project uid (lower-case letters, digits, - and _, at most {NAME_LIMIT} characters)"),
        ))
    }
}

/// A branch names a directory under the project's, and is read back
/// from that directory's name on restart, so it is one path component:
/// letters, digits, `-`, `_` and `.`, not starting with a dot. The
/// router decodes `%2F` and `%2E` before the handler sees the name, so
/// `..%2F..%2Fother-account%2Fproject%2Fmain` would otherwise reach
/// another account's deployment on disk.
fn check_branch(branch: &str) -> Result<(), ApiError> {
    let ok = !branch.is_empty()
        && branch.len() <= NAME_LIMIT
        && !branch.starts_with('.')
        && branch.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_' || b == b'.');
    if ok {
        Ok(())
    } else {
        Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid",
            format!("'{branch}' is not a branch name this server accepts (letters, digits, - _ and ., not starting with a dot, at most {NAME_LIMIT} characters; a git branch with a / in its name cannot be deployed yet)"),
        ))
    }
}

fn not_found(uid: &str, branch: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no deployment for project {uid} branch {branch}"))
}

fn model(app: &App, t: &Tenant, uid: &str, branch: &str) -> Result<Arc<Model>, ApiError> {
    app.models.read().unwrap().get(&t.key(uid, branch)).cloned().ok_or_else(|| not_found(uid, branch))
}

// ------------------------------------------------------------------ deploy

async fn deploy(
    State(app): State<Shared>,
    Auth(who): Auth,
    held: Option<Extension<Held>>,
    Path((uid, branch)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let _held = held;
        deploy_or_validate(app, who, uid, branch, body, true)
    })
    .await
}

async fn validate(
    State(app): State<Shared>,
    Auth(who): Auth,
    held: Option<Extension<Held>>,
    Path((uid, branch)): Path<(String, String)>,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    blocking(move || {
        let _held = held;
        deploy_or_validate(app, who, uid, branch, body, false)
    })
    .await
}

/// Refuses a deploy that would put an account over its branch caps.
///
/// Checked before the archive is unpacked, so going over costs the
/// caller nothing and the server nothing. Redeploying a branch that is
/// already there is not a new branch and is always allowed, which is
/// what keeps an account at its cap from being locked out of its own
/// models.
///
/// Counted from the models actually in memory rather than from the
/// project records, because memory is what is being protected: a branch
/// that is not loaded is not holding anything.
fn check_caps(app: &App, who: &Option<Principal>, uid: &str, branch: &str) -> Result<(), ApiError> {
    let models = app.models.read().unwrap();
    caps_allow(app, &models, &scope_of(who), uid, branch)
}

/// The cap check itself, over a models map the caller already holds.
/// Taken twice: once before the work, and once more under the write lock
/// that inserts, because two deploys can pass the first check at the
/// same moment and only the second one is exact.
fn caps_allow(app: &App, models: &HashMap<String, Arc<Model>>, scope: &str, uid: &str, branch: &str) -> Result<(), ApiError> {
    if app.accounts.is_none() {
        return Ok(());
    }
    let (key, project_prefix) = (format!("{scope}{uid}/{branch}"), format!("{scope}{uid}/"));
    if models.contains_key(&key) {
        return Ok(());
    }
    let (in_project, in_account) = branch_counts(models.keys().map(|k| k.as_str()), scope, &project_prefix);
    let over = |what: String, n: usize, cap: usize| {
        ApiError(
            StatusCode::CONFLICT,
            "OverLimit",
            format!(
                "{what} already has {n} deployed branch{} of the {cap} allowed. Remove one before deploying another: `zsql remove --branch <branch> --yes`, or delete it in the web app under the project.",
                if n == 1 { "" } else { "es" }
            ),
        )
    };
    if in_project >= app.caps.per_project {
        return Err(over(uid.to_string(), in_project, app.caps.per_project));
    }
    if in_account >= app.caps.per_account {
        return Err(over("this account".into(), in_account, app.caps.per_account));
    }
    Ok(())
}

/// Deployed branches in this project, and in the whole account, counted
/// from the keys of the models map (`[account/]project/branch`).
///
/// The trailing slash both prefixes carry is load-bearing: without it
/// the account `acme` would count `acme-2`'s branches against itself,
/// and the project `sales` would count `sales-eu`'s.
fn branch_counts<'a>(keys: impl Iterator<Item = &'a str>, scope: &str, project_prefix: &str) -> (usize, usize) {
    let (mut in_project, mut in_account) = (0usize, 0usize);
    for k in keys.filter(|k| k.starts_with(scope)) {
        in_account += 1;
        if k.starts_with(project_prefix) {
            in_project += 1;
        }
    }
    (in_project, in_account)
}

/// A user deploying to a project the account does not have yet creates
/// it and owns it; validating needs no project. Query keys cannot deploy.
fn deploy_gate(
    app: &App,
    who: &Option<Principal>,
    uid: &str,
    branch: &str,
    keep: bool,
    production_branch: &str,
    name: &str,
) -> Result<Tenant, ApiError> {
    check_uid(uid)?;
    check_branch(branch)?;
    let scope = scope_of(who);
    let (Some(store), Some(p)) = (&app.accounts, who) else { return Ok(Tenant { scope, project: None }) };
    match store.project(p.account().id, uid).map_err(internal)? {
        Some(project) => {
            store.authorize(p, &project, branch, Verb::Write).map_err(accounts_error)?;
            Ok(Tenant { scope, project: Some(project) })
        }
        None => {
            let (user, account, _) = user_of(who)?;
            if !keep {
                return Ok(Tenant { scope, project: None });
            }
            let project = store.create_project(account.id, user, uid, name, production_branch).map_err(accounts_error)?;
            Ok(Tenant { scope, project: Some(project) })
        }
    }
}

fn deploy_or_validate(
    app: Shared,
    who: Option<Principal>,
    uid: String,
    branch: String,
    body: Bytes,
    keep: bool,
) -> Result<Json<Value>, ApiError> {
    let bad = |m: String| ApiError(StatusCode::BAD_REQUEST, "DeployError", m);
    if body.is_empty() {
        return Err(bad("the body must be a tar.gz of the project directory".into()));
    }
    if matches!(who, Some(Principal::QueryKey { .. })) {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "query keys are read only; deploy with your personal key".into()));
    }
    // Before the caps are counted under these names, and before the
    // archive is unpacked: a bad name is answered without doing work.
    check_uid(&uid)?;
    check_branch(&branch)?;
    // A validate keeps nothing, so it is not held to the caps.
    if keep {
        check_caps(&app, &who, &uid, &branch)?;
    }
    let work = std::env::temp_dir().join(format!("zsqld-deploy-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&work).map_err(|e| bad(e.to_string()))?;
    // Paths in what the loader says are inside the work directory; the
    // caller knows their project by its own layout, not by where the
    // server unpacked it.
    let work_prefix = format!("{}/", work.display());
    let result = (|| -> Result<Value, ApiError> {
        unpack_archive(&body, &work).map_err(|m| bad(m.replace(&work_prefix, "")))?;
        let root = project_root(&work).ok_or_else(|| bad("the archive holds no project.yml".into()))?;
        let loaded = zsql_service::load(&root, Trust::Untrusted, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?;
        let t = deploy_gate(&app, &who, &uid, &branch, keep, &loaded.snapshot.branch.uid, &loaded.snapshot.project.name)?;
        let deployed =
            Arc::new(Model::from_loaded(&uid, &branch, loaded, app.dialects).map_err(|e| bad(e.to_string().replace(&work_prefix, "")))?);
        let mut out = deployed.summary();
        out["test_results"] = deployed.run_tests();
        out["validated_only"] = Value::Bool(!keep);
        if keep {
            // Keep the compiled model on disk so a restart reloads it:
            // written beside the branch, then swapped in whole.
            let _writing = app.deploying.lock().unwrap();
            let live = t.dir(&app, &uid, &branch);
            let staged = live.with_file_name(format!(".{branch}.staging-{}", nanos()));
            deployed.write(&staged).and_then(|()| swap_in(&staged, &live)).map_err(|e| {
                let _ = std::fs::remove_dir_all(&staged);
                bad(e.to_string())
            })?;
            let mut models = app.models.write().unwrap();
            caps_allow(&app, &models, &t.scope, &uid, &branch)?;
            models.insert(t.key(&uid, &branch), deployed);
            drop(models);
            if let (Some(store), Some(p)) = (&app.accounts, &who) {
                let _ = store.record(p.account().id, &p.describe(), "deploy", &format!("{uid}/{branch}"));
            }
        }
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result.map(Json)
}

/// The most an archive may hold once decompressed, and the most entries.
/// The body limit bounds what is sent; gzip can grow a thousandfold on
/// the way out, into a temp directory that is memory on the hosted box.
const UNPACKED_LIMIT: u64 = 256 * 1024 * 1024;
const ENTRY_LIMIT: usize = 20_000;

/// A reader that stops with an error, not a quiet end, past a limit, so
/// the archive reader's message says what happened.
struct Capped<R> {
    inner: R,
    left: u64,
    /// Set when the limit was hit, for the caller to tell that apart
    /// from any other failure the archive reader wraps it in.
    tripped: Arc<std::sync::atomic::AtomicBool>,
}

impl<R: std::io::Read> std::io::Read for Capped<R> {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left == 0 {
            self.tripped.store(true, std::sync::atomic::Ordering::Relaxed);
            return Err(std::io::Error::other("decompressed size limit"));
        }
        let want = buf.len().min(self.left as usize);
        let n = self.inner.read(&mut buf[..want])?;
        self.left -= n as u64;
        Ok(n)
    }
}

/// Unpacks the project archive into `work`: regular files and
/// directories only, within the size and count limits.
///
/// A symbolic or hard link is refused rather than created. The loader
/// follows links when it reads, so a link to another account's
/// directory, or to anything on the host, would be read as part of the
/// project and shown back in the model or its errors. The CLI never
/// writes one.
fn unpack_archive(body: &[u8], work: &std::path::Path) -> Result<(), String> {
    let tripped = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let decoder = Capped { inner: flate2::read::GzDecoder::new(body), left: UNPACKED_LIMIT, tripped: tripped.clone() };
    let mut archive = tar::Archive::new(decoder);
    archive.set_overwrite(false);
    let too_big = || format!("the archive is larger than {} MiB when decompressed", UNPACKED_LIMIT / (1024 * 1024));
    let failed = |what: &str, e: std::io::Error| {
        if tripped.load(std::sync::atomic::Ordering::Relaxed) {
            too_big()
        } else {
            format!("{what} the archive: {e}")
        }
    };
    let entries = archive.entries().map_err(|e| failed("reading", e))?;
    let mut n = 0usize;
    for entry in entries {
        let mut entry = entry.map_err(|e| failed("reading", e))?;
        n += 1;
        if n > ENTRY_LIMIT {
            return Err(format!("the archive holds more than {ENTRY_LIMIT} entries"));
        }
        let kind = entry.header().entry_type();
        if !matches!(kind, tar::EntryType::Regular | tar::EntryType::Directory) {
            let path = entry.path().map(|p| p.display().to_string()).unwrap_or_default();
            return Err(format!("the archive holds {path}, which is not a file or a directory ({kind:?}); only those are deployed"));
        }
        entry.unpack_in(work).map_err(|e| failed("unpacking", e))?;
    }
    Ok(())
}

/// The directory holding project.yml: the archive root, or its single top folder.
fn project_root(work: &std::path::Path) -> Option<PathBuf> {
    if work.join("project.yml").exists() {
        return Some(work.to_path_buf());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(work).ok()?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
    entries.into_iter().find(|p| p.join("project.yml").exists())
}

// --------------------------------------------------------------- sql, explain

/// What one request may ask for. A spec is JSON the caller wrote and
/// the cost of planning grows with it, so its shape is bounded before
/// any work starts: generous beside any real query, a wall in front of
/// a generated one. See `docs/rate-limiting.md`.
mod shape {
    pub const EXPR: usize = 8 * 1024;
    pub const PROJECTIONS: usize = 100;
    pub const CALCULATIONS: usize = 50;
    pub const SEGMENTS: usize = 50;
    pub const FILTERS: usize = 200;
    pub const FILTER_DEPTH: usize = 8;
    pub const VALUES: usize = 20_000;
    pub const DEPTH: usize = 32;
    pub const TEXT: usize = 64 * 1024;
    pub const SEARCH_TERM: usize = 200;
}

fn too_big(what: &str, n: usize, limit: usize) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "TooLarge", format!("this request has {n} {what}; the limit is {limit}"))
}

/// Counts a spec's parts and refuses one built to be expensive.
fn check_shape(spec: &Value) -> Result<(), ApiError> {
    for (part, limit) in [("projections", shape::PROJECTIONS), ("calculations", shape::CALCULATIONS), ("segments", shape::SEGMENTS)] {
        let n = spec.get(part).and_then(Value::as_array).map(Vec::len).unwrap_or(0);
        if n > limit {
            return Err(too_big(part, n, limit));
        }
    }
    // Filters are a flat list or an and/or tree; resolution walks it, so
    // bound both how many there are and how deeply they nest.
    fn walk(v: &Value, depth: usize, n: &mut usize) -> Result<(), ApiError> {
        if depth > shape::FILTER_DEPTH {
            return Err(too_big("levels of nested filters", depth, shape::FILTER_DEPTH));
        }
        match v {
            // The list inside an `and` or an `or` is part of that level,
            // not another one, so the count means what it says.
            Value::Array(items) => {
                for i in items {
                    walk(i, depth, n)?;
                }
            }
            Value::Object(o) => match o.get("and").or_else(|| o.get("or")) {
                Some(inner) => walk(inner, depth + 1, n)?,
                None => *n += 1,
            },
            _ => *n += 1,
        }
        Ok(())
    }
    if let Some(filters) = spec.get("filters") {
        let mut n = 0;
        walk(filters, 0, &mut n)?;
        if n > shape::FILTERS {
            return Err(too_big("filters", n, shape::FILTERS));
        }
    }
    // Whatever else the document carries, bound the document.
    fn measure(v: &Value, depth: usize, n: &mut usize) -> Result<(), ApiError> {
        *n += 1;
        if *n > shape::VALUES {
            return Err(too_big("values in this spec", *n, shape::VALUES));
        }
        if depth > shape::DEPTH {
            return Err(too_big("levels of nesting", depth, shape::DEPTH));
        }
        match v {
            Value::Array(items) => {
                for i in items {
                    measure(i, depth + 1, n)?;
                }
            }
            Value::Object(o) => {
                for (_, i) in o {
                    measure(i, depth + 1, n)?;
                }
            }
            Value::String(text) if text.len() > shape::TEXT => return Err(too_big("characters in one value", text.len(), shape::TEXT)),
            _ => {}
        }
        Ok(())
    }
    measure(spec, 0, &mut 0)
}

/// A search term long enough to be slow to score against every field.
fn check_term(term: &str) -> Result<(), ApiError> {
    if term.len() > shape::SEARCH_TERM {
        return Err(too_big("characters in this search term", term.len(), shape::SEARCH_TERM));
    }
    Ok(())
}

/// A shorthand line long enough to be slow to parse.
fn check_expr(expr: Option<&str>) -> Result<(), ApiError> {
    if let Some(line) = expr {
        if line.len() > shape::EXPR {
            return Err(too_big("characters in this expression", line.len(), shape::EXPR));
        }
    }
    Ok(())
}

/// The shape bound, as the service takes it.
fn shape_check(v: &Value) -> Result<(), ServiceError> {
    check_shape(v).map_err(|ApiError(_, class, message)| ServiceError::invalid(class, message))
}

/// A refusal from the service, as the API reports it.
impl From<ServiceError> for ApiError {
    fn from(e: ServiceError) -> Self {
        let status = match e.kind {
            Kind::Invalid => StatusCode::BAD_REQUEST,
            Kind::Unprocessable => StatusCode::UNPROCESSABLE_ENTITY,
            Kind::NotFound => StatusCode::NOT_FOUND,
            Kind::Internal => StatusCode::INTERNAL_SERVER_ERROR,
        };
        ApiError(status, e.class, e.message)
    }
}

// The read handlers are each a thin wrapper over a plain function, so
// the MCP tools in mcp.rs run exactly the same code with the same
// principal, and there is one place per read where access is decided.
// The work itself is the service's: the server adds who may read the
// branch, the context a key is bound to, and the bounds on a request.

async fn sql(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((uid, branch)): Path<(String, String)>,
    Json(req): Json<SqlRequest>,
) -> Result<Json<SqlResponse>, ApiError> {
    Ok(Json(sql_of(&app, &who, &uid, &branch, req)?))
}

fn sql_of(app: &Shared, who: &Option<Principal>, uid: &str, branch: &str, mut req: SqlRequest) -> Result<SqlResponse, ApiError> {
    let t = gate(app, who, uid, branch, Verb::Read)?;
    let d = model(app, &t, uid, branch)?;
    req.context = effective_context(&t, who, req.context)?;
    check_expr(req.expr.as_deref())?;
    Ok(d.sql(req, &shape_check)?)
}

async fn explain(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((uid, branch)): Path<(String, String)>,
    Json(req): Json<SqlRequest>,
) -> Result<Json<ExplainResponse>, ApiError> {
    Ok(Json(explain_of(&app, &who, &uid, &branch, req)?))
}

fn explain_of(app: &Shared, who: &Option<Principal>, uid: &str, branch: &str, mut req: SqlRequest) -> Result<ExplainResponse, ApiError> {
    let t = gate(app, who, uid, branch, Verb::Read)?;
    let d = model(app, &t, uid, branch)?;
    req.context = effective_context(&t, who, req.context)?;
    check_expr(req.expr.as_deref())?;
    Ok(d.explain(req, &shape_check)?)
}

async fn explore(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((uid, branch)): Path<(String, String)>,
    Json(req): Json<ExploreRequest>,
) -> Result<Json<ExploreResponse>, ApiError> {
    Ok(Json(explore_of(&app, &who, &uid, &branch, req)?))
}

/// Exploring resolves universes only, so no policy applies yet; the
/// context is still held to the bound-context rule, so a key behaves
/// the same on every read.
fn explore_of(
    app: &Shared,
    who: &Option<Principal>,
    uid: &str,
    branch: &str,
    mut req: ExploreRequest,
) -> Result<ExploreResponse, ApiError> {
    let t = gate(app, who, uid, branch, Verb::Read)?;
    let d = model(app, &t, uid, branch)?;
    req.context = effective_context(&t, who, req.context)?;
    check_expr(req.expr.as_deref())?;
    if let Some(term) = &req.q {
        check_term(term)?;
    }
    Ok(d.explore(req, &shape_check)?)
}

// --------------------------------------------------------------- discovery

#[derive(Deserialize)]
struct FieldsQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    hidden: bool,
}

/// Field discovery. With `q`, fields containing the text come first in
/// model order, then near matches by trigram similarity, each with its
/// `score` (1 for a containing match).
async fn fields(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((uid, branch)): Path<(String, String)>,
    Query(q): Query<FieldsQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(fields_of(&app, &who, &uid, &branch, q)?))
}

fn fields_of(app: &Shared, who: &Option<Principal>, uid: &str, branch: &str, q: FieldsQuery) -> Result<Value, ApiError> {
    let t = gate(app, who, uid, branch, Verb::Read)?;
    let d = model(app, &t, uid, branch)?;
    check_term(&q.q)?;
    Ok(d.fields(&q.q, q.hidden))
}

async fn tables(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    Ok(Json(tables_of(&app, &who, &uid, &branch)?))
}

fn tables_of(app: &Shared, who: &Option<Principal>, uid: &str, branch: &str) -> Result<Value, ApiError> {
    let t = gate(app, who, uid, branch, Verb::Read)?;
    let d = model(app, &t, uid, branch)?;
    Ok(d.tables())
}

async fn values(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((uid, branch, dim)): Path<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(values_of(&app, &who, &uid, &branch, &dim)?))
}

fn values_of(app: &Shared, who: &Option<Principal>, uid: &str, branch: &str, dim: &str) -> Result<Value, ApiError> {
    let t = gate(app, who, uid, branch, Verb::Read)?;
    let d = model(app, &t, uid, branch)?;
    Ok(d.values_of(dim)?)
}

async fn test(
    State(app): State<Shared>,
    Auth(who): Auth,
    held: Option<Extension<Held>>,
    Path((uid, branch)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Write)?;
    let d = model(&app, &t, &uid, &branch)?;
    blocking(move || {
        let _held = held;
        Ok(Json(d.run_tests()))
    })
    .await
}

async fn remove(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Write)?;
    let removed = app.models.write().unwrap().remove(&t.key(&uid, &branch)).is_some();
    if !removed {
        return Err(not_found(&uid, &branch));
    }
    let _ = std::fs::remove_dir_all(t.dir(&app, &uid, &branch));
    if let (Some(store), Some(p)) = (&app.accounts, &who) {
        let _ = store.record(p.account().id, &p.describe(), "branch_removed", &format!("{uid}/{branch}"));
    }
    Ok(Json(json!({ "removed": format!("{uid}/{branch}") })))
}

async fn branch_summary(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((uid, branch)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Read)?;
    let d = model(&app, &t, &uid, &branch)?;
    Ok(Json(d.summary()))
}

/// The deployments in one account's slice of the models map: the keys
/// that start with its scope (`acme/`), or every key in single-tenant
/// mode, where the scope is empty.
///
/// The map holds every account's deployments side by side, and
/// `Model` names its project and branch but not its account, so two
/// accounts that both deploy `customer-service/main` are told apart by
/// the key alone. Listing by project and branch without the key showed
/// each of them the other's deployment as a second copy of their own.
fn in_scope<'a, V>(models: &'a HashMap<String, V>, scope: &str) -> impl Iterator<Item = &'a V> {
    let scope = scope.to_string();
    models.iter().filter(move |(k, _)| k.starts_with(&scope)).map(|(_, d)| d)
}

/// Every deployment the caller may read; with accounts on, also the
/// account's projects and the caller's level on each.
async fn projects(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    Ok(Json(projects_of(&app, &who)?))
}

fn projects_of(app: &Shared, who: &Option<Principal>) -> Result<Value, ApiError> {
    let mut out = Vec::new();
    for d in in_scope(&app.models.read().unwrap(), &scope_of(who)) {
        if gate(app, who, &d.uid, &d.branch, Verb::Read).is_ok() {
            out.push(d.summary());
        }
    }
    out.sort_by_key(|v| (v["project"].as_str().unwrap_or("").to_string(), v["branch"].as_str().unwrap_or("").to_string()));
    let mut answer = json!({ "deployments": out });
    if let (Some(store), Some(p)) = (&app.accounts, who) {
        let mut projects = Vec::new();
        for project in store.projects(p.account().id).map_err(internal)? {
            let level = match p {
                Principal::User { role: Role::Admin, .. } => Some(Level::Owner),
                Principal::User { user, .. } => store.level_for(&project, user.id).map_err(internal)?,
                Principal::QueryKey { grants, .. } => grants.iter().any(|g| g.project_id == project.id).then_some(Level::Read),
            };
            if level.is_some() {
                projects.push(json!({ "uid": project.uid, "name": project.name, "production_branch": project.production_branch, "visibility": project.visibility, "protected_production": project.protected_production, "level": level }));
            }
        }
        answer["projects"] = json!(projects);
    }
    Ok(answer)
}

// ---------------------------------------------------------------- accounts

#[derive(Deserialize)]
struct SignupRequest {
    account: String,
    email: String,
    #[serde(default)]
    name: String,
    /// Optional: an account made from the CLI alone has no password until
    /// someone sets one; the web app always sends one.
    #[serde(default)]
    password: Option<String>,
}

/// A new account with its first admin and key. The secret is shown once.
/// With a password, the answer also opens a session.
async fn signup(State(app): State<Shared>, Json(req): Json<SignupRequest>) -> Result<Response, ApiError> {
    store_of(&app)?;
    if !app.signup_open {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "sign-up is closed on this server".into()));
    }
    let password = req.password.filter(|p| !p.is_empty());
    let su = {
        let (app, account, email, name, password) = (app.clone(), req.account, req.email, req.name, password.clone());
        blocking(move || {
            let su = store_of(&app)?.signup(&account, &email, &name, password.as_deref()).map_err(accounts_error)?;
            if let Some(token) = &su.verify_token {
                let link = app.mailer.link("verify", token);
                let (subject, body) = mail::verification(&link);
                deliver(&app, &su.user.email, &subject, &body, &link, "verification");
            }
            Ok(su)
        })
        .await?
    };
    let store = store_of(&app)?;
    let body = json!({ "account": su.account, "user": su.user, "key": su.key, "secret": su.secret });
    if password.is_some() {
        let (token, _) = store.create_session(&su.user, SESSION_DAYS).map_err(accounts_error)?;
        return Ok(with_session_cookie(&app, body, Some(&token)));
    }
    Ok(Json(body).into_response())
}

#[derive(Deserialize)]
struct LoginRequest {
    email: String,
    password: String,
}

async fn login(State(app): State<Shared>, Json(req): Json<LoginRequest>) -> Result<Response, ApiError> {
    store_of(&app)?;
    // Enough wrong passwords for one email and it is refused for a
    // while, whichever address they come from: that is what someone
    // working through a password list looks like.
    if let Some(secs) = app.limits.login_blocked(&req.email) {
        return Ok(retry_later(
            StatusCode::TOO_MANY_REQUESTS,
            "RateLimited",
            format!("too many failed sign-ins for this email: retry in {secs}s"),
            secs,
        ));
    }
    let found = {
        let (app, email, password) = (app.clone(), req.email.clone(), req.password);
        blocking(move || store_of(&app)?.verify_password(&email, &password).map_err(accounts_error)).await?
    };
    let Some(user) = found else {
        app.limits.login_failed(&req.email);
        return Err(ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "wrong email or password".into()));
    };
    app.limits.login_ok(&req.email);
    let store = store_of(&app)?;
    let (token, account) = store.create_session(&user, SESSION_DAYS).map_err(accounts_error)?;
    Ok(with_session_cookie(&app, json!({ "user": user, "account": account }), Some(&token)))
}

async fn logout(State(app): State<Shared>, parts: axum::http::request::Parts) -> Result<Response, ApiError> {
    let store = store_of(&app)?;
    // A cross-site form can post here with the cookie attached; it
    // cannot send this header. Without the check it could sign the
    // user out at will.
    if parts.headers.get("x-requested-with").and_then(|v| v.to_str().ok()) != Some("zsql") {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "this request did not come from the app".into()));
    }
    if let Some(token) = cookie(&parts, SESSION_COOKIE) {
        store.delete_session(&token).map_err(accounts_error)?;
    }
    Ok(with_session_cookie(&app, json!({ "logged_out": true }), None))
}

#[derive(Deserialize)]
struct PasswordRequest {
    password: String,
}

async fn set_password(
    State(app): State<Shared>,
    Auth(who): Auth,
    headers: axum::http::HeaderMap,
    Json(req): Json<PasswordRequest>,
) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    // The session this change is made from survives it; any other is
    // ended, which is the point of changing a password.
    let keep = cookie_in(&headers, SESSION_COOKIE);
    let (user, account_id, password) = (user.clone(), account.id, req.password);
    let app = app.clone();
    blocking(move || {
        store_of(&app)?.set_password(account_id, &user, &password, keep.as_deref()).map_err(accounts_error)?;
        Ok(Json(json!({ "ok": true })))
    })
    .await
}

/// Hands one message to the mailer. Answers whether it went.
///
/// With nothing configured the link is written to the journal instead,
/// which is what makes a self-hosted or development server usable
/// without mail. It is never put in a response: a reset is asked for by
/// whoever typed the address, who may not be its owner.
///
/// A send that fails is logged and nothing else. The alternative is a
/// 500 that tells the caller their address exists and that the operator
/// has a broken relay, which helps an attacker and not the user.
fn deliver(app: &App, to: &str, subject: &str, body: &str, link: &str, what: &str) -> bool {
    match app.mailer.send(to, subject, body) {
        Ok(mail::Sent::Delivered) => true,
        Ok(mail::Sent::NotConfigured) => {
            eprintln!("zsqld: no ZSQL_MAIL_COMMAND is set, so the {what} link for {to} was not sent: {link}");
            false
        }
        Err(e) => {
            eprintln!("zsqld: sending the {what} link to {to} failed: {e}");
            false
        }
    }
}

#[derive(Deserialize)]
struct EmailRequest {
    email: String,
}

#[derive(Deserialize)]
struct TokenRequest {
    token: String,
}

#[derive(Deserialize)]
struct ResetRequest {
    token: String,
    password: String,
}

/// Asks for a password reset link. No auth: forgetting a password is the
/// whole reason to be here.
///
/// Always answers the same, whether the address has an account, belongs
/// to no account, or asked moments ago. Saying which would turn this
/// into a way to test whether an address is registered, and the answer
/// would be worth having for whoever asked.
async fn request_reset(State(app): State<Shared>, Json(req): Json<EmailRequest>) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    let (app, email) = (app.clone(), req.email);
    blocking(move || {
        if let Some((_, user, token)) = store_of(&app)?.start_reset(&email).map_err(accounts_error)? {
            let link = app.mailer.link("reset", &token);
            let (subject, body) = mail::reset(&user.email, &link);
            deliver(&app, &user.email, &subject, &body, &link, "reset");
        }
        Ok(Json(json!({ "ok": true })))
    })
    .await
}

/// Sets a new password from a reset link and signs the user in, so the
/// one thing they came to do is not followed by a login form.
///
/// No auth: the token is the credential. It stops working here, and
/// every other session of that user is dropped.
async fn accept_reset(State(app): State<Shared>, Json(req): Json<ResetRequest>) -> Result<Response, ApiError> {
    store_of(&app)?;
    let (account, user) = {
        let (app, token, password) = (app.clone(), req.token, req.password);
        blocking(move || store_of(&app)?.reset_password(&token, &password).map_err(accounts_error)).await?
    };
    let store = store_of(&app)?;
    let (token, _) = store.create_session(&user, SESSION_DAYS).map_err(accounts_error)?;
    Ok(with_session_cookie(&app, json!({ "user": user, "account": account }), Some(&token)))
}

/// Confirms an address from a verification link. No auth: the token is
/// the credential, and the link is often opened in another browser than
/// the one that signed up.
async fn verify_email(State(app): State<Shared>, Json(req): Json<TokenRequest>) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    let (app, token) = (app.clone(), req.token);
    blocking(move || {
        let (account, user) = store_of(&app)?.verify_email(&token).map_err(accounts_error)?;
        Ok(Json(json!({ "verified": true, "user": user, "account": account })))
    })
    .await
}

/// Sends the caller a fresh verification link for their own address.
/// `sent` is false when mail is not configured or a link went out
/// moments ago, so the app can say which.
async fn resend_verification(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    if user.email_verified {
        return Ok(Json(json!({ "verified": true, "sent": false })));
    }
    let (app, user, account_id) = (app.clone(), user.clone(), account.id);
    blocking(move || {
        let Some(token) = store_of(&app)?.start_verification(account_id, &user).map_err(accounts_error)? else {
            // One went out in the last minute. Sending another would
            // only invalidate the link already in their inbox.
            return Ok(Json(json!({ "verified": false, "sent": false, "waiting": true })));
        };
        let link = app.mailer.link("verify", &token);
        let (subject, body) = mail::verification(&link);
        let sent = deliver(&app, &user.email, &subject, &body, &link, "verification");
        Ok(Json(json!({ "verified": false, "sent": sent })))
    })
    .await
}

async fn me(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    Ok(Json(match &who {
        Some(Principal::User { user, account, role, key_id }) => {
            json!({ "kind": "user", "user": user, "account": account, "role": role, "key_id": key_id })
        }
        Some(Principal::QueryKey { key, account, grants }) => {
            json!({ "kind": "query_key", "key": key, "account": account, "grants": grants })
        }
        None => json!({}),
    }))
}

async fn members(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (_, account, _) = user_of(&who)?;
    Ok(Json(json!({ "members": store.members(account.id).map_err(accounts_error)? })))
}

#[derive(Deserialize)]
struct MemberRequest {
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default = "developer")]
    role: String,
}

fn developer() -> String {
    "developer".into()
}

/// Adds or re-roles a member. A member with no personal key yet also gets
/// an invite token, shown once, to redeem at `/invites/accept`.
async fn add_member(State(app): State<Shared>, Auth(who): Auth, Json(req): Json<MemberRequest>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account) = admin_of(&who)?;
    let role = zsql_accounts::parse_role(&req.role).map_err(accounts_error)?;
    let m = store.add_member(account.id, &user.email, &req.email, &req.name, role).map_err(accounts_error)?;
    let mut out = json!(m);
    // Only a newcomer gets a token: for someone who already signs in
    // the membership is the whole of it, and a token could not be
    // redeemed for them anyway.
    if !store.can_sign_in(m.user.id).map_err(accounts_error)? {
        out["invite"] = json!(store.create_invite(account.id, &user.email, &m.user.email).map_err(accounts_error)?);
    }
    Ok(Json(out))
}

/// A fresh invite token for a member, for when the first was lost.
async fn invite(State(app): State<Shared>, Auth(who): Auth, Path(email): Path<String>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account) = admin_of(&who)?;
    Ok(Json(json!({ "invite": store.create_invite(account.id, &user.email, &email).map_err(accounts_error)? })))
}

#[derive(Deserialize)]
struct AcceptRequest {
    token: String,
    #[serde(default)]
    name: String,
    #[serde(default)]
    key_name: String,
    #[serde(default)]
    password: Option<String>,
}

/// No auth: the token is the credential. Answers with the member's first
/// personal key, shown once, and opens a session when a password was set.
async fn accept_invite(State(app): State<Shared>, Json(req): Json<AcceptRequest>) -> Result<Response, ApiError> {
    store_of(&app)?;
    let password = req.password.filter(|p| !p.is_empty());
    let (account, user, key, secret) = {
        let (app, token, name, key_name, password) = (app.clone(), req.token, req.name, req.key_name, password.clone());
        blocking(move || store_of(&app)?.accept_invite(&token, &name, &key_name, password.as_deref()).map_err(accounts_error)).await?
    };
    let store = store_of(&app)?;
    let body = json!({ "account": account, "user": user, "key": key, "secret": secret });
    if password.is_some() {
        let (token, _) = store.create_session(&user, SESSION_DAYS).map_err(accounts_error)?;
        return Ok(with_session_cookie(&app, body, Some(&token)));
    }
    Ok(Json(body).into_response())
}

// ------------------------------------------------------------------ web app

/// The app shell and its assets, embedded. Every `/app/...` path gets the
/// shell; the script reads the path and renders the view.
async fn ui(Path(rest): Path<String>) -> Response {
    ui_file(&rest)
}

async fn ui_root() -> Response {
    ui_file("")
}

/// The files browsers and manifests expect at the site root: favicons,
/// the touch icon, the Android icons and the manifest that names them.
const SITE_FILES: &[&str] = &[
    "favicon.ico",
    "favicon.svg",
    "apple-touch-icon.png",
    "site.webmanifest",
    "icon-192.png",
    "icon-512.png",
    "icon-maskable-192.png",
    "icon-maskable-512.png",
];

async fn site_file(uri: axum::http::Uri) -> Response {
    ui_file(uri.path().trim_start_matches('/'))
}

/// An embedded asset: its bytes, media type, and whether browsers may keep
/// it for a day (the brand files never change under one build; the shell,
/// stylesheet and script are revalidated every time).
fn asset(path: &str) -> Option<(&'static [u8], &'static str, bool)> {
    Some(match path {
        "app.css" => (include_bytes!("../ui/app.css"), "text/css; charset=utf-8", false),
        "app.js" => (include_bytes!("../ui/app.js"), "text/javascript; charset=utf-8", false),
        "brand/wordmark-light.svg" => (include_bytes!("../ui/brand/wordmark-light.svg"), "image/svg+xml", true),
        "brand/wordmark-dark.svg" => (include_bytes!("../ui/brand/wordmark-dark.svg"), "image/svg+xml", true),
        "brand/icon-light.svg" => (include_bytes!("../ui/brand/icon-light.svg"), "image/svg+xml", true),
        "brand/icon-dark.svg" => (include_bytes!("../ui/brand/icon-dark.svg"), "image/svg+xml", true),
        "favicon.svg" => (include_bytes!("../ui/brand/favicon.svg"), "image/svg+xml", true),
        "favicon.ico" => (include_bytes!("../ui/brand/favicon.ico"), "image/x-icon", true),
        "apple-touch-icon.png" => (include_bytes!("../ui/brand/apple-touch-icon.png"), "image/png", true),
        "icon-192.png" => (include_bytes!("../ui/brand/icon-192.png"), "image/png", true),
        "icon-512.png" => (include_bytes!("../ui/brand/icon-512.png"), "image/png", true),
        "icon-maskable-192.png" => (include_bytes!("../ui/brand/icon-maskable-192.png"), "image/png", true),
        "icon-maskable-512.png" => (include_bytes!("../ui/brand/icon-maskable-512.png"), "image/png", true),
        "site.webmanifest" => (include_bytes!("../ui/brand/site.webmanifest"), "application/manifest+json", true),
        _ => return None,
    })
}

fn ui_file(path: &str) -> Response {
    let (body, mime, cache): (&'static [u8], &str, &str) = match asset(path) {
        Some((body, mime, stable)) => (body, mime, if stable { "public, max-age=86400" } else { "no-cache" }),
        None => (include_bytes!("../ui/app.html"), "text/html; charset=utf-8", "no-cache"),
    };
    let mut r = ([("content-type", mime.to_string()), ("cache-control", cache.to_string())], body).into_response();
    let h = r.headers_mut();
    h.insert("x-content-type-options", "nosniff".parse().unwrap());
    h.insert("referrer-policy", "same-origin".parse().unwrap());
    h.insert("x-frame-options", "DENY".parse().unwrap());
    if mime.starts_with("text/html") {
        // Every interpolation in the app escapes, so this is the net
        // under that: a script or style from anywhere but here does not
        // run, and the page cannot be framed.
        h.insert("content-security-policy", UI_CSP.parse().unwrap());
    }
    r
}

const UI_CSP: &str = "default-src 'self'; script-src 'self'; style-src 'self' 'unsafe-inline' https://fonts.googleapis.com; font-src https://fonts.gstatic.com; img-src 'self' data:; connect-src 'self'; frame-ancestors 'none'; base-uri 'none'; form-action 'self'; object-src 'none'";

async fn root(State(app): State<Shared>) -> Response {
    if app.accounts.is_some() {
        axum::response::Redirect::to("/app/").into_response()
    } else {
        "zsqld: accounts off; see /healthz".into_response()
    }
}

async fn remove_member(State(app): State<Shared>, Auth(who): Auth, Path(email): Path<String>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account) = admin_of(&who)?;
    store.remove_member(account.id, &user.email, &email).map_err(accounts_error)?;
    Ok(Json(json!({ "removed": email })))
}

#[derive(Deserialize)]
struct NameRequest {
    name: String,
}

async fn user_keys(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    Ok(Json(json!({ "keys": store.user_keys(account.id, user.id).map_err(accounts_error)? })))
}

async fn create_user_key(State(app): State<Shared>, Auth(who): Auth, Json(req): Json<NameRequest>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let (key, secret) = store.create_user_key(account.id, user, &req.name).map_err(accounts_error)?;
    Ok(Json(json!({ "key": key, "secret": secret })))
}

async fn revoke_user_key(State(app): State<Shared>, Auth(who): Auth, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    store.revoke_user_key(account.id, user, id).map_err(accounts_error)?;
    Ok(Json(json!({ "revoked": id })))
}

async fn query_keys(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (_, account, _) = user_of(&who)?;
    let mut keys = Vec::new();
    for k in store.query_keys(account.id).map_err(accounts_error)? {
        let grants = store.grants(account.id, k.id).map_err(accounts_error)?;
        keys.push(json!({ "key": k, "grants": grants }));
    }
    Ok(Json(json!({ "query_keys": keys })))
}

async fn create_query_key(State(app): State<Shared>, Auth(who): Auth, Json(req): Json<NameRequest>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let (key, secret) = store.create_query_key(account.id, user, &req.name).map_err(accounts_error)?;
    Ok(Json(json!({ "key": key, "secret": secret })))
}

/// Admins and the key's creator manage it.
fn may_manage_key(
    store: &Store,
    who: &Option<Principal>,
    id: i64,
) -> Result<(zsql_accounts::User, zsql_accounts::Account, zsql_accounts::QueryKey), ApiError> {
    let (user, account, role) = user_of(who)?;
    let key = store.query_key(account.id, id).map_err(accounts_error)?;
    if role != Role::Admin && key.created_by != Some(user.id) {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "only an admin or the key's creator can manage it".into()));
    }
    Ok((user.clone(), account.clone(), key))
}

async fn rotate_query_key(State(app): State<Shared>, Auth(who): Auth, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = may_manage_key(store, &who, id)?;
    let (key, secret) = store.rotate_query_key(account.id, &user, id).map_err(accounts_error)?;
    Ok(Json(json!({ "key": key, "secret": secret })))
}

async fn revoke_query_key(State(app): State<Shared>, Auth(who): Auth, Path(id): Path<i64>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = may_manage_key(store, &who, id)?;
    store.revoke_query_key(account.id, &user, id).map_err(accounts_error)?;
    Ok(Json(json!({ "revoked": id })))
}

#[derive(Deserialize)]
struct GrantRequest {
    project: String,
    #[serde(default)]
    branch: Option<String>,
    /// The security context the key plans with on this project, bound
    /// by its owner. Absent or null leaves the grant unbound.
    #[serde(default)]
    context: Option<Value>,
}

/// Granting a key read on a project is the project owner's call (or an
/// admin's), and so is binding the context it plans with there: that
/// context can assert claims that bypass the model's policies, which is
/// the owner's to decide and nobody else's.
async fn grant(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path(id): Path<i64>,
    Json(req): Json<GrantRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let key = store.query_key(account.id, id).map_err(accounts_error)?;
    let t = gate(&app, &who, &req.project, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let context = match req.context {
        None | Some(Value::Null) => None,
        Some(v) => {
            let n = v.to_string().len();
            if n > shape::TEXT {
                return Err(too_big("bytes in this context", n, shape::TEXT));
            }
            // Stored in its canonical shape, so what the owner sees back
            // is exactly what the planner will be given.
            let ctx: SecurityContext = serde_json::from_value(v)
                .map_err(|e| ApiError(StatusCode::BAD_REQUEST, "Invalid", format!("this is not a security context: {e}")))?;
            Some(serde_json::to_value(ctx).expect("a context serializes"))
        }
    };
    let g = store.grant(account.id, user, key.id, &project, req.branch.as_deref(), context.as_ref()).map_err(accounts_error)?;
    Ok(Json(json!({ "key": key, "grant": g })))
}

async fn revoke_grant(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((id, project_uid)): Path<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let t = gate(&app, &who, &project_uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    store.revoke_grant(account.id, user, id, &project).map_err(accounts_error)?;
    Ok(Json(json!({ "revoked": { "key": id, "project": project_uid } })))
}

async fn project_access(State(app): State<Shared>, Auth(who): Auth, Path(uid): Path<String>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    // Who has access is for people, not for an application's query key.
    user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Read)?;
    let project = t.project.expect("accounts on");
    let access: Vec<Value> =
        store.project_access(&project).map_err(accounts_error)?.into_iter().map(|(u, l)| json!({ "user": u, "level": l })).collect();
    let keys: Vec<Value> =
        store.keys_granted_on(&project).map_err(accounts_error)?.into_iter().map(|(k, g)| json!({ "key": k, "grant": g })).collect();
    Ok(Json(json!({ "project": project, "access": access, "query_keys": keys })))
}

#[derive(Deserialize)]
struct AccessRequest {
    email: String,
    level: String,
}

async fn set_access(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path(uid): Path<String>,
    Json(req): Json<AccessRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let level = zsql_accounts::parse_level(&req.level).map_err(accounts_error)?;
    let (u, l) = store.set_access(user, &project, &req.email, level).map_err(accounts_error)?;
    Ok(Json(json!({ "user": u, "level": l })))
}

async fn remove_access(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path((uid, email)): Path<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    store.remove_access(user, &project, &email).map_err(accounts_error)?;
    Ok(Json(json!({ "removed": email })))
}

#[derive(Deserialize)]
struct ProjectPatch {
    name: Option<String>,
    production_branch: Option<String>,
    visibility: Option<String>,
    protected_production: Option<bool>,
}

async fn update_project(
    State(app): State<Shared>,
    Auth(who): Auth,
    Path(uid): Path<String>,
    Json(req): Json<ProjectPatch>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let p = store
        .update_project(
            user,
            &project,
            req.name.as_deref(),
            req.production_branch.as_deref(),
            req.visibility.as_deref(),
            req.protected_production,
        )
        .map_err(accounts_error)?;
    Ok(Json(json!(p)))
}

/// Deletes the project record and every deployment of it.
async fn delete_project(State(app): State<Shared>, Auth(who): Auth, Path(uid): Path<String>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let prefix = format!("{}{uid}/", t.scope);
    app.models.write().unwrap().retain(|k, _| !k.starts_with(&prefix));
    let _ = std::fs::remove_dir_all(app.data_dir.join(&t.scope).join(&uid));
    store.delete_project(user, &project).map_err(accounts_error)?;
    Ok(Json(json!({ "deleted": uid })))
}

#[derive(Deserialize)]
struct AuditQuery {
    #[serde(default = "fifty")]
    limit: usize,
}

fn fifty() -> usize {
    50
}

async fn audit(State(app): State<Shared>, Auth(who): Auth, Query(q): Query<AuditQuery>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (_, account) = admin_of(&who)?;
    Ok(Json(json!({ "events": store.audit_log(account.id, q.limit.min(1000)).map_err(accounts_error)? })))
}

async fn healthz() -> &'static str {
    "ok"
}

// ----------------------------------------------------------------- startup

fn reload(app: &App) -> anyhow::Result<(usize, Vec<String>)> {
    let mut n = 0;
    let mut skipped: Vec<String> = Vec::new();
    // Single-tenant: data/<project>/<branch>; with accounts: data/<account>/<project>/<branch>.
    let roots: Vec<(String, PathBuf)> = if app.accounts.is_some() {
        let Ok(accounts) = std::fs::read_dir(&app.data_dir) else { return Ok((0, skipped)) };
        accounts.flatten().filter(|e| e.path().is_dir()).map(|e| (format!("{}/", e.file_name().to_string_lossy()), e.path())).collect()
    } else {
        vec![(String::new(), app.data_dir.clone())]
    };
    for (scope, root) in roots {
        let Ok(projects) = std::fs::read_dir(&root) else { continue };
        for p in projects.flatten().filter(|e| e.path().is_dir()) {
            let uid = p.file_name().to_string_lossy().to_string();
            for b in std::fs::read_dir(p.path())?.flatten().filter(|e| e.path().is_dir()) {
                let branch = b.file_name().to_string_lossy().to_string();
                // A branch name never starts with a dot. A deploy's own
                // staging or previous copy, left by a crash between two
                // renames, is swept; any other dot directory is somebody
                // else's and is left alone, since a data directory can be
                // pointed anywhere by a typo.
                if branch.starts_with('.') {
                    if is_swap_leftover(&branch) {
                        let _ = std::fs::remove_dir_all(b.path());
                    }
                    continue;
                }
                if !b.path().join(zsql_service::SNAPSHOT_FILE).exists() {
                    continue;
                }
                // One unreadable branch is not a reason to refuse to
                // start. A model that will not load is almost always a
                // format this build is too old or too new for, and the
                // archive it was compiled from lives in someone's
                // repository, so the fix is a redeploy of that one branch.
                // Coming up without it, and saying which, beats a restart
                // loop that systemd gives up on after five tries.
                let key = format!("{scope}{uid}/{branch}");
                match Model::from_stored(&b.path(), &uid, &branch, app.dialects) {
                    Ok(m) => {
                        app.models.write().unwrap().insert(key, Arc::new(m));
                        n += 1;
                    }
                    Err(e) => {
                        eprintln!("zsqld: {key} was not loaded: {e}");
                        skipped.push(key);
                    }
                }
            }
        }
    }
    Ok((n, skipped))
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

/// Every route, behind the guard, over the app. Built here so the
/// tests drive the same router the binary serves.
fn router(app: Shared) -> Router {
    let mut router = Router::new()
        .route("/healthz", get(healthz))
        .route("/projects", get(projects))
        .route("/projects/{uid}/branches/{branch}", get(branch_summary))
        .route("/projects/{uid}/branches/{branch}/deploy", post(deploy).layer(axum::extract::DefaultBodyLimit::max(ARCHIVE_LIMIT)))
        .route("/projects/{uid}/branches/{branch}/validate", post(validate).layer(axum::extract::DefaultBodyLimit::max(ARCHIVE_LIMIT)))
        .route("/projects/{uid}/branches/{branch}/test", post(test))
        .route("/projects/{uid}/branches/{branch}", delete(remove))
        .route("/projects/{uid}/branches/{branch}/sql", post(sql))
        .route("/projects/{uid}/branches/{branch}/explain", post(explain))
        .route("/projects/{uid}/branches/{branch}/explore", post(explore))
        .route("/projects/{uid}/branches/{branch}/fields", get(fields))
        .route("/projects/{uid}/branches/{branch}/tables", get(tables))
        .route("/projects/{uid}/branches/{branch}/values/{dim}", get(values))
        .route(mcp::PATH, post(mcp::endpoint))
        .route("/", get(root))
        .route("/app", get(ui_root))
        .route("/app/", get(ui_root))
        .route("/app/{*rest}", get(ui))
        .route("/signup", post(signup))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/account/password", post(set_password))
        .route("/password-reset", post(request_reset))
        .route("/password-reset/accept", post(accept_reset))
        .route("/verify-email", post(verify_email))
        .route("/account/verify-email", post(resend_verification))
        .route("/me", get(me))
        .route("/account/members", get(members).post(add_member))
        .route("/account/members/{email}", delete(remove_member))
        .route("/account/members/{email}/invite", post(invite))
        .route("/invites/accept", post(accept_invite))
        .route("/account/keys", get(user_keys).post(create_user_key))
        .route("/account/keys/{id}", delete(revoke_user_key))
        .route("/account/query-keys", get(query_keys).post(create_query_key))
        .route("/account/query-keys/{id}", delete(revoke_query_key))
        .route("/account/query-keys/{id}/rotate", post(rotate_query_key))
        .route("/account/query-keys/{id}/grants", post(grant))
        .route("/account/query-keys/{id}/grants/{project}", delete(revoke_grant))
        .route("/account/audit", get(audit))
        .route("/projects/{uid}", axum::routing::patch(update_project).delete(delete_project))
        .route("/projects/{uid}/access", get(project_access).put(set_access))
        .route("/projects/{uid}/access/{email}", delete(remove_access));
    for name in SITE_FILES {
        router = router.route(&format!("/{name}"), get(site_file));
    }
    router
        .layer(axum::middleware::from_fn_with_state(app.clone(), guard))
        // Every route but a deploy takes a small JSON body; the archive
        // routes raise their own limit above.
        .layer(axum::extract::DefaultBodyLimit::max(JSON_LIMIT))
        .with_state(app)
}

// ----------------------------------------------------------------- running

/// Everything the server is told. `zsql serve` fills it from `ZSQL_*`
/// variables and flags. The library itself reads no environment, so a
/// flag and a variable can only ever mean the same thing, and two
/// servers can start in one test process.
pub struct Config {
    pub data_dir: PathBuf,
    /// `host:port`; port 0 takes a free one, and the chosen address is
    /// logged.
    pub bind: String,
    /// Multi-tenant: every request carries a key, deployments are
    /// scoped to an account. Off is one tenant and no auth.
    pub accounts: bool,
    pub signup_open: bool,
    /// Where the app is reached from outside, for links and cookies;
    /// the bind address when `None`. `https://` marks cookies Secure.
    pub public_url: Option<String>,
    pub limits: limits::Config,
    pub admission: limits::AdmissionCounts,
    pub caps: Caps,
    pub mail: MailConfig,
    pub mcp: mcp::Settings,
    /// How long an authentication decision may be reused; zero turns
    /// the cache off.
    pub auth_cache_ttl: Duration,
}

/// How mail goes out: a command fed the message on stdin, or nothing,
/// in which case links are written to the log.
#[derive(Debug, Clone)]
pub struct MailConfig {
    pub command: Option<String>,
    pub from: String,
    pub timeout: Duration,
}

impl Default for MailConfig {
    fn default() -> Self {
        MailConfig { command: None, from: String::new(), timeout: Duration::from_secs(20) }
    }
}

impl Config {
    /// A server on its own machine: one tenant, no auth, no limits, the
    /// compiled-in MCP wording.
    pub fn local(data_dir: PathBuf, bind: String) -> Config {
        Config {
            data_dir,
            bind,
            accounts: false,
            signup_open: true,
            public_url: None,
            limits: limits::Config { mode: limits::Mode::Off, ..limits::Config::default() },
            admission: limits::AdmissionCounts::default(),
            caps: Caps::default(),
            mail: MailConfig::default(),
            mcp: mcp::Settings::builtin(),
            auth_cache_ttl: zsql_accounts::DEFAULT_AUTH_CACHE_TTL,
        }
    }
}

/// Runs the server until SIGTERM or ctrl-c, then finishes what is in
/// flight and returns.
pub async fn run(config: Config) -> anyhow::Result<()> {
    serve(config, |_| {}).await
}

/// [`run`], telling `listening` the address once it is bound: the
/// port a bind of `:0` was given, which is what a caller that then
/// talks to the server needs.
pub async fn serve(config: Config, listening: impl FnOnce(SocketAddr) + Send + 'static) -> anyhow::Result<()> {
    std::fs::create_dir_all(&config.data_dir)?;
    let _lock = lock_data_dir(&config.data_dir)?;
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    let accounts_on = config.accounts;
    let accounts = if accounts_on { Some(Store::open_with(&config.data_dir.join("accounts.db"), config.auth_cache_ttl)?) } else { None };
    let public_url = config.public_url.clone().unwrap_or_else(|| format!("http://{}", config.bind));
    let secure_cookies = public_url.starts_with("https://");
    let limits = Limiter::new(config.limits);
    let admission = Admission::new(&config.admission);
    let mailer =
        mail::Mailer::new(config.mail.command, &config.mail.from, config.mail.timeout, &public_url).map_err(|e| anyhow::anyhow!("{e}"))?;
    let limits_mode = if limits.on() {
        if limits.cfg.mode == limits::Mode::Shadow {
            "shadow"
        } else {
            "on"
        }
    } else {
        "off"
    };
    let mail_state = if mailer.configured() { "on" } else { "off, links to the journal" };
    let app: Shared = Arc::new(App {
        dialects,
        data_dir: config.data_dir,
        models: RwLock::new(HashMap::new()),
        accounts,
        signup_open: config.signup_open,
        secure_cookies,
        limits,
        admission,
        caps: config.caps,
        mailer,
        mcp: config.mcp,
        deploying: std::sync::Mutex::new(()),
    });
    let (n, skipped) = reload(&app)?;
    eprintln!(
        "zsqld: {n} deployment(s) loaded from {}; accounts {}; rate limits {limits_mode}; mail {mail_state}",
        app.data_dir.display(),
        if accounts_on {
            if app.signup_open {
                "on, sign-up open"
            } else {
                "on, sign-up closed"
            }
        } else {
            "off"
        }
    );
    if !skipped.is_empty() {
        // Said again, together, after the per-branch lines above: the
        // one line someone watching a restart will actually read.
        eprintln!("zsqld: {} deployment(s) could not be loaded and are not being served: {}", skipped.len(), skipped.join(", "));
    }
    if accounts_on {
        let cap = |n: usize| if n == usize::MAX { "off".to_string() } else { n.to_string() };
        eprintln!("zsqld: branch caps: {} per project, {} per account", cap(app.caps.per_project), cap(app.caps.per_account));
    }

    let router = router(app.clone());

    // Buckets for callers that have gone quiet, and failure windows that
    // have passed, are dropped now and then so an idle server does not
    // hold a row per address it has ever seen.
    let sweeper = app.clone();
    tokio::spawn(async move {
        let mut tick = tokio::time::interval(Duration::from_secs(300));
        loop {
            tick.tick().await;
            sweeper.limits.sweep();
        }
    });

    // The handlers are installed before the address is announced, so a
    // signal sent the moment the listening line is read is a graceful
    // stop and not the default disposition's instant death.
    let shutdown = shutdown_signal();
    let listener = tokio::net::TcpListener::bind(&config.bind).await?;
    // The address actually bound, so a port of 0 is answered with the
    // port it got.
    let addr = listener.local_addr()?;
    eprintln!("zsqld: listening on http://{addr}");
    listening(addr);
    // Connect info: the per-address limits need the peer's address.
    axum::serve(listener, router.into_make_service_with_connect_info::<SocketAddr>()).with_graceful_shutdown(shutdown).await?;
    eprintln!("zsqld: stopped");
    Ok(())
}

/// [`serve`] on a runtime of its own, for a caller that is not async.
pub fn run_blocking(config: Config, listening: impl FnOnce(SocketAddr) + Send + 'static) -> anyhow::Result<()> {
    tokio::runtime::Builder::new_multi_thread().enable_all().build()?.block_on(serve(config, listening))
}

/// Resolves on SIGTERM or ctrl-c. What is in flight finishes; nothing
/// new is accepted. The handlers are installed when this is called,
/// not when the future is first polled.
fn shutdown_signal() -> impl std::future::Future<Output = ()> {
    #[cfg(unix)]
    let mut streams = {
        use tokio::signal::unix::{signal, SignalKind};
        (signal(SignalKind::terminate()).ok(), signal(SignalKind::interrupt()).ok())
    };
    async move {
        #[cfg(unix)]
        {
            let (term, int) = (&mut streams.0, &mut streams.1);
            let terminate = async {
                match term {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            let interrupt = async {
                match int {
                    Some(s) => {
                        s.recv().await;
                    }
                    None => std::future::pending::<()>().await,
                }
            };
            tokio::select! {
                _ = interrupt => {}
                _ = terminate => {}
            }
        }
        #[cfg(not(unix))]
        {
            let _ = tokio::signal::ctrl_c().await;
        }
        eprintln!("zsqld: stopping");
    }
}

/// One server per data directory. The lock file holds the pid of the
/// server that has it, so the refusal can say who.
fn lock_data_dir(dir: &std::path::Path) -> anyhow::Result<std::fs::File> {
    use std::io::{Read, Write};
    let path = dir.join("lock");
    let mut file = std::fs::OpenOptions::new().read(true).write(true).create(true).truncate(false).open(&path)?;
    #[cfg(unix)]
    {
        use std::os::unix::io::AsRawFd;
        if unsafe { libc::flock(file.as_raw_fd(), libc::LOCK_EX | libc::LOCK_NB) } != 0 {
            let mut other = String::new();
            let _ = file.read_to_string(&mut other);
            anyhow::bail!(
                "{} is already served by process {}; stop that server, or give this one another data directory",
                dir.display(),
                if other.trim().is_empty() { "?".to_string() } else { other.trim().to_string() }
            );
        }
    }
    file.set_len(0)?;
    write!(file, "{}", std::process::id())?;
    Ok(file)
}

/// What a deploy leaves beside a branch while it swaps:
/// `.<branch>.staging-<n>`, then `.<branch>.previous-<n>`.
fn is_swap_leftover(name: &str) -> bool {
    let Some(rest) = name.strip_prefix('.') else { return false };
    [".staging-", ".previous-"]
        .iter()
        .any(|mark| rest.rsplit_once(mark).is_some_and(|(_, n)| !n.is_empty() && n.bytes().all(|b| b.is_ascii_digit())))
}

/// Puts `staged` where `live` is, by rename, so a reader of the data
/// directory sees the branch as it was or as it is, never half
/// written. A leftover `.previous` directory is swept at startup.
fn swap_in(staged: &std::path::Path, live: &std::path::Path) -> std::io::Result<()> {
    let name = live.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let previous = live.with_file_name(format!(".{name}.previous-{}", nanos()));
    let had_live = live.exists();
    if had_live {
        std::fs::rename(live, &previous)?;
    }
    if let Err(e) = std::fs::rename(staged, live) {
        if had_live {
            let _ = std::fs::rename(&previous, live);
        }
        return Err(e);
    }
    if had_live {
        let _ = std::fs::remove_dir_all(&previous);
    }
    Ok(())
}

#[cfg(test)]
mod library_tests {
    use super::*;

    /// The library, and the accounts crate it links, are configured,
    /// never by the environment: a `zsql serve` flag and a `ZSQL_*`
    /// variable must mean the same thing, so there is one place that
    /// reads variables and it is the CLI's serve module.
    #[test]
    fn the_library_reads_no_environment() {
        let needle = ["env::", "var("].concat();
        for f in ["server/src/lib.rs", "server/src/limits.rs", "server/src/mail.rs", "server/src/mcp.rs", "accounts/src/lib.rs"] {
            let text = std::fs::read_to_string(format!("{}/../{f}", env!("CARGO_MANIFEST_DIR"))).unwrap();
            assert!(!text.contains(&needle), "{f} reads the environment");
        }
    }

    /// Only a deploy's own leftovers are swept at startup.
    #[test]
    fn only_swap_leftovers_are_swept() {
        assert!(is_swap_leftover(".main.staging-17"));
        assert!(is_swap_leftover(".release-1.2.previous-1700000000"));
        assert!(!is_swap_leftover(".git"));
        assert!(!is_swap_leftover(".main.staging-"));
        assert!(!is_swap_leftover(".main.staging-x"));
        assert!(!is_swap_leftover("main"));
    }

    /// The swap leaves the new branch, whole, and nothing else.
    #[test]
    fn a_deploy_swaps_in_whole() {
        let root = std::env::temp_dir().join(format!("zsqld-swap-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        let live = root.join("main");
        std::fs::create_dir_all(&live).unwrap();
        std::fs::write(live.join("old"), "1").unwrap();
        let staged = root.join(".main.staging-1");
        std::fs::create_dir_all(&staged).unwrap();
        std::fs::write(staged.join("new"), "2").unwrap();
        swap_in(&staged, &live).unwrap();
        assert!(live.join("new").exists() && !live.join("old").exists());
        let names: Vec<String> = std::fs::read_dir(&root).unwrap().flatten().map(|e| e.file_name().to_string_lossy().to_string()).collect();
        assert_eq!(names, vec!["main"]);
        let _ = std::fs::remove_dir_all(&root);
    }
}

/// A project archive. The TPC-DS sample is ten kilobytes and a large
/// model is single-digit megabytes, so this is roomy; the point is that
/// it no longer applies to every other route.
const ARCHIVE_LIMIT: usize = 64 * 1024 * 1024;
const JSON_LIMIT: usize = 1024 * 1024;

#[cfg(test)]
mod isolation_tests;

#[cfg(test)]
mod parity_tests;

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

    /// The caps are counted by string prefix over the deployment keys,
    /// and a prefix is an easy thing to get subtly wrong: the account
    /// `acme` must not be charged for `acme-2`, and the project `sales`
    /// must not be charged for `sales-eu`. Both of those would show up
    /// as an account mysteriously unable to deploy.
    #[test]
    fn branches_are_counted_against_the_right_project_and_account() {
        let keys =
            ["acme/sales/main", "acme/sales/feature-x", "acme/sales-eu/main", "acme/web/main", "acme-2/sales/main", "acme-2/sales/staging"];
        let it = || keys.iter().copied();

        // Four branches in acme, two of them in acme/sales.
        assert_eq!(branch_counts(it(), "acme/", "acme/sales/"), (2, 4));

        // `sales-eu` is its own project, with its own count.
        assert_eq!(branch_counts(it(), "acme/", "acme/sales-eu/"), (1, 4));

        // The other account is counted separately, and is not reached
        // by a prefix of its name.
        assert_eq!(branch_counts(it(), "acme-2/", "acme-2/sales/"), (2, 2));

        // A project with nothing deployed yet starts at zero.
        assert_eq!(branch_counts(it(), "acme/", "acme/new/"), (0, 4));

        // Single-tenant: no scope at all, so everything is in scope and
        // the account count is simply the total.
        let flat = ["sales/main", "sales/dev", "web/main"];
        assert_eq!(branch_counts(flat.iter().copied(), "", "sales/"), (2, 3));
    }

    /// Both names become directory names under the account's slice of
    /// the data directory, and the router hands them over decoded, so
    /// anything that could step out of that slice is refused before it
    /// is used. The uid must also be the canonical slug, or the same
    /// project gets a second directory and key per spelling.
    #[test]
    fn project_and_branch_names_stay_inside_the_account() {
        for uid in ["customer-service", "tpcds", "bird_financial", "a1"] {
            assert!(check_uid(uid).is_ok(), "{uid}");
        }
        for uid in [
            "",
            "Customer-Service",
            " customer-service",
            "../newco/customer-service",
            "a/b",
            "a\\b",
            "a.b",
            "a b",
            &"x".repeat(NAME_LIMIT + 1),
        ] {
            assert!(check_uid(uid).is_err(), "{uid:?}");
        }
        for branch in ["main", "master", "probe-holder-loan", "release_1.2", "v1.2.3", "RC1"] {
            assert!(check_branch(branch).is_ok(), "{branch}");
        }
        for branch in [
            "",
            ".",
            "..",
            ".hidden",
            "../../newco/customer-service/main",
            "feature/x",
            "a\\b",
            "a b",
            "a\0b",
            "main\n",
            &"x".repeat(NAME_LIMIT + 1),
        ] {
            assert!(check_branch(branch).is_err(), "{branch:?}");
        }
    }

    /// Two accounts may each deploy a project called `customer-service`
    /// on `main`; the listing must show each only its own. The scope
    /// ends in a slash, so `acme` does not reach into `acme-2` either.
    #[test]
    fn the_listing_shows_an_account_only_its_own_deployments() {
        let models: HashMap<String, &str> = [
            ("acme/customer-service/main", "acme's"),
            ("acme/web/main", "acme's web"),
            ("acme-2/customer-service/main", "acme-2's"),
            ("newco/customer-service/main", "newco's"),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v))
        .collect();
        let seen = |scope: &str| {
            let mut v: Vec<&str> = in_scope(&models, scope).copied().collect();
            v.sort();
            v
        };
        assert_eq!(seen("acme/"), ["acme's", "acme's web"]);
        assert_eq!(seen("acme-2/"), ["acme-2's"]);
        assert_eq!(seen("newco/"), ["newco's"]);
        assert_eq!(seen("nobody/"), Vec::<&str>::new());
        // Single-tenant: no scope, everything is in scope.
        assert_eq!(seen("").len(), 4);
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
