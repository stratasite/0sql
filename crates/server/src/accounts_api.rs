//! The accounts endpoints: sign-up and sign-in, passwords and their
//! resets, members, keys, grants, project access and the audit log.

use crate::auth::{admin_of, cookie, cookie_in, store_of, user_of, with_session_cookie, Auth, SESSION_COOKIE, SESSION_DAYS};
use crate::error::{accounts_error, retry_later, ApiError, Input, Params, Search};
use crate::reads::{shape, too_big};
use crate::tenancy::gate;
use crate::{blocking, lock, mail, write, App, Shared};
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use zsql_accounts::{Principal, Role, Store, Verb};
use zsql_spec::SecurityContext;

#[derive(Deserialize)]
pub(crate) struct SignupRequest {
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
pub(crate) async fn signup(State(app): State<Shared>, Input(req): Input<SignupRequest>) -> Result<Response, ApiError> {
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
pub(crate) struct LoginRequest {
    email: String,
    password: String,
}

pub(crate) async fn login(State(app): State<Shared>, Input(req): Input<LoginRequest>) -> Result<Response, ApiError> {
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

pub(crate) async fn logout(State(app): State<Shared>, parts: axum::http::request::Parts) -> Result<Response, ApiError> {
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
pub(crate) struct PasswordRequest {
    password: String,
    /// The password as it is; needed when the user has one.
    #[serde(default)]
    current_password: Option<String>,
}

pub(crate) async fn set_password(
    State(app): State<Shared>,
    Auth(who): Auth,
    headers: axum::http::HeaderMap,
    Input(req): Input<PasswordRequest>,
) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    // The session this change is made from survives it; any other is
    // ended, which is the point of changing a password.
    let keep = cookie_in(&headers, SESSION_COOKIE);
    let (user, account_id, password, current) = (user.clone(), account.id, req.password, req.current_password);
    let app = app.clone();
    blocking(move || {
        store_of(&app)?.set_password(account_id, &user, &password, current.as_deref(), keep.as_deref()).map_err(accounts_error)?;
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
            say!("zsqld: no ZSQL_MAIL_COMMAND is set, so the {what} link for {to} was not sent: {link}");
            false
        }
        Err(e) => {
            say!("zsqld: sending the {what} link to {to} failed: {e}");
            false
        }
    }
}

#[derive(Deserialize)]
pub(crate) struct EmailRequest {
    email: String,
}

#[derive(Deserialize)]
pub(crate) struct TokenRequest {
    token: String,
}

#[derive(Deserialize)]
pub(crate) struct ResetRequest {
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
pub(crate) async fn request_reset(State(app): State<Shared>, Input(req): Input<EmailRequest>) -> Result<Json<Value>, ApiError> {
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
pub(crate) async fn accept_reset(State(app): State<Shared>, Input(req): Input<ResetRequest>) -> Result<Response, ApiError> {
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
pub(crate) async fn verify_email(State(app): State<Shared>, Input(req): Input<TokenRequest>) -> Result<Json<Value>, ApiError> {
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
pub(crate) async fn resend_verification(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
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

pub(crate) async fn me(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
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

pub(crate) async fn members(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (_, account, _) = user_of(&who)?;
    Ok(Json(json!({ "members": store.members(account.id).map_err(accounts_error)? })))
}

#[derive(Deserialize)]
pub(crate) struct MemberRequest {
    email: String,
    #[serde(default)]
    name: String,
    #[serde(default = "developer")]
    role: String,
}

pub(crate) fn developer() -> String {
    "developer".into()
}

/// Adds or re-roles a member. A member with no personal key yet also gets
/// an invite token, shown once, to redeem at `/invites/accept`.
pub(crate) async fn add_member(
    State(app): State<Shared>,
    Auth(who): Auth,
    Input(req): Input<MemberRequest>,
) -> Result<Json<Value>, ApiError> {
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
pub(crate) async fn invite(State(app): State<Shared>, Auth(who): Auth, Params(email): Params<String>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account) = admin_of(&who)?;
    Ok(Json(json!({ "invite": store.create_invite(account.id, &user.email, &email).map_err(accounts_error)? })))
}

#[derive(Deserialize)]
pub(crate) struct AcceptRequest {
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
pub(crate) async fn accept_invite(State(app): State<Shared>, Input(req): Input<AcceptRequest>) -> Result<Response, ApiError> {
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

pub(crate) async fn remove_member(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params(email): Params<String>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account) = admin_of(&who)?;
    store.remove_member(account.id, &user.email, &email).map_err(accounts_error)?;
    Ok(Json(json!({ "removed": email })))
}

#[derive(Deserialize)]
pub(crate) struct NameRequest {
    name: String,
}

pub(crate) async fn user_keys(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    Ok(Json(json!({ "keys": store.user_keys(account.id, user.id).map_err(accounts_error)? })))
}

pub(crate) async fn create_user_key(
    State(app): State<Shared>,
    Auth(who): Auth,
    Input(req): Input<NameRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let (key, secret) = store.create_user_key(account.id, user, &req.name).map_err(accounts_error)?;
    Ok(Json(json!({ "key": key, "secret": secret })))
}

pub(crate) async fn revoke_user_key(State(app): State<Shared>, Auth(who): Auth, Params(id): Params<i64>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    store.revoke_user_key(account.id, user, id).map_err(accounts_error)?;
    Ok(Json(json!({ "revoked": id })))
}

pub(crate) async fn query_keys(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (_, account, _) = user_of(&who)?;
    let mut keys = Vec::new();
    for k in store.query_keys(account.id).map_err(accounts_error)? {
        let grants = store.grants(account.id, k.id).map_err(accounts_error)?;
        keys.push(json!({ "key": k, "grants": grants }));
    }
    Ok(Json(json!({ "query_keys": keys })))
}

pub(crate) async fn create_query_key(
    State(app): State<Shared>,
    Auth(who): Auth,
    Input(req): Input<NameRequest>,
) -> Result<Json<Value>, ApiError> {
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

pub(crate) async fn rotate_query_key(State(app): State<Shared>, Auth(who): Auth, Params(id): Params<i64>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = may_manage_key(store, &who, id)?;
    let (key, secret) = store.rotate_query_key(account.id, &user, id).map_err(accounts_error)?;
    Ok(Json(json!({ "key": key, "secret": secret })))
}

pub(crate) async fn revoke_query_key(State(app): State<Shared>, Auth(who): Auth, Params(id): Params<i64>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = may_manage_key(store, &who, id)?;
    store.revoke_query_key(account.id, &user, id).map_err(accounts_error)?;
    Ok(Json(json!({ "revoked": id })))
}

#[derive(Deserialize)]
pub(crate) struct GrantRequest {
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
pub(crate) async fn grant(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params(id): Params<i64>,
    Input(req): Input<GrantRequest>,
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

pub(crate) async fn revoke_grant(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params((id, project_uid)): Params<(i64, String)>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let t = gate(&app, &who, &project_uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    store.revoke_grant(account.id, user, id, &project).map_err(accounts_error)?;
    Ok(Json(json!({ "revoked": { "key": id, "project": project_uid } })))
}

pub(crate) async fn project_access(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params(uid): Params<String>,
) -> Result<Json<Value>, ApiError> {
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
pub(crate) struct AccessRequest {
    email: String,
    level: String,
}

pub(crate) async fn set_access(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params(uid): Params<String>,
    Input(req): Input<AccessRequest>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let level = zsql_accounts::parse_level(&req.level).map_err(accounts_error)?;
    let (u, l) = store.set_access(user, &project, &req.email, level).map_err(accounts_error)?;
    Ok(Json(json!({ "user": u, "level": l })))
}

pub(crate) async fn remove_access(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params((uid, email)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    store.remove_access(user, &project, &email).map_err(accounts_error)?;
    Ok(Json(json!({ "removed": email })))
}

#[derive(Deserialize)]
pub(crate) struct ProjectPatch {
    name: Option<String>,
    production_branch: Option<String>,
    visibility: Option<String>,
    protected_production: Option<bool>,
}

pub(crate) async fn update_project(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params(uid): Params<String>,
    Input(req): Input<ProjectPatch>,
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
pub(crate) async fn delete_project(
    State(app): State<Shared>,
    Auth(who): Auth,
    Params(uid): Params<String>,
) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    blocking(move || {
        let store = store_of(&app)?;
        let (user, _, _) = user_of(&who)?;
        let project = t.project.expect("accounts on");
        let prefix = format!("{}{uid}/", t.scope);
        let _writing = lock(&app.deploying);
        write(&app.models).retain(|k, _| !k.starts_with(&prefix));
        let _ = std::fs::remove_dir_all(app.data_dir.join(&t.scope).join(&uid));
        store.delete_project(user, &project).map_err(accounts_error)?;
        Ok(Json(json!({ "deleted": uid })))
    })
    .await
}

#[derive(Deserialize)]
pub(crate) struct AuditQuery {
    #[serde(default = "fifty")]
    limit: usize,
}

fn fifty() -> usize {
    50
}

pub(crate) async fn audit(State(app): State<Shared>, Auth(who): Auth, Search(q): Search<AuditQuery>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (_, account) = admin_of(&who)?;
    Ok(Json(json!({ "events": store.audit_log(account.id, q.limit.min(1000)).map_err(accounts_error)? })))
}
