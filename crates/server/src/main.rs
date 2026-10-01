//! zsqld: a light service. Deploy a project directory (as a tar.gz), then
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
//! GET  /projects/{uid}/branches/{branch}/sampling-plan  the SQL the CLI runs to sample values
//! DELETE /projects/{uid}/branches/{branch}
//! GET  /projects                                       every deployed branch
//! GET  /healthz
//! ```

use axum::body::Bytes;
use axum::extract::{FromRequestParts, Path, Query, State};
use axum::http::request::Parts;
use zsql_accounts::{Level, Principal, Project, Role, Store, Verb};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{delete, get, post};
use axum::Router;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::{Arc, RwLock};
use std::time::Instant;
use zsql_dialect::Dialects;
use zsql_project::values::ValueCatalog;
use zsql_semantic::{FieldKind, Snapshot, Universe};
use zsql_spec::SecurityContext;

// A deployed branch's universe and the planner index built over it, in
// one cell: the planner borrows the universe, so the two must live and
// die together, and the cell keeps the universe where the borrow expects
// it. Built once per deploy; every request borrows it.
self_cell::self_cell!(
    struct Engine {
        owner: Universe,
        #[covariant]
        dependent: Planner,
    }
);

type Planner<'a> = zsql_planner::Planner<'a>;

struct Deployed {
    uid: String,
    branch: String,
    engine: Engine,
    values: Option<ValueCatalog>,
    tests: Vec<zsql_project::ProjectTest>,
    warnings: Vec<String>,
    deployed_at: String,
}

impl Deployed {
    fn universe(&self) -> &Universe {
        self.engine.borrow_owner()
    }
    fn planner(&self) -> &Planner<'_> {
        self.engine.borrow_dependent()
    }
}

struct App {
    /// Shared by every deployment for the life of the process, so each
    /// planner can borrow it for as long as its universe.
    dialects: &'static Dialects,
    data_dir: PathBuf,
    /// Deployments by `[account/]project/branch`.
    models: RwLock<HashMap<String, Arc<Deployed>>>,
    /// Accounts, users and keys; `None` in the single-tenant mode.
    accounts: Option<Store>,
    signup_open: bool,
    /// Session cookies marked Secure, for a server behind https.
    secure_cookies: bool,
}

// ---------------------------------------------------------------- tenancy

/// The caller: a user or a query key when accounts are on, nothing when off.
struct Auth(Option<Principal>);

impl FromRequestParts<Shared> for Auth {
    type Rejection = ApiError;

    async fn from_request_parts(parts: &mut Parts, app: &Shared) -> Result<Self, ApiError> {
        let Some(store) = &app.accounts else { return Ok(Auth(None)) };
        let header = parts.headers.get("authorization").and_then(|v| v.to_str().ok()).unwrap_or("");
        let secret = header.strip_prefix("Bearer ").or_else(|| header.strip_prefix("bearer ")).map(str::trim).unwrap_or("");
        if !secret.is_empty() {
            return match store.authenticate(secret).map_err(internal)? {
                Some(p) => Ok(Auth(Some(p))),
                None => Err(ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "this API key is not valid".into())),
            };
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
                return Ok(Auth(Some(p)));
            }
        }
        Err(ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "an API key is required: Authorization: Bearer <key>".into()))
    }
}

const SESSION_COOKIE: &str = "zsql_session";
const SESSION_DAYS: i64 = 30;

fn cookie(parts: &Parts, name: &str) -> Option<String> {
    let header = parts.headers.get("cookie")?.to_str().ok()?;
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
    let scope = scope_of(who);
    let (Some(store), Some(p)) = (&app.accounts, who) else { return Ok(Tenant { scope, project: None }) };
    let project = store.project(p.account().id, uid).map_err(internal)?.ok_or_else(|| not_found(uid, branch))?;
    store.authorize(p, &project, branch, verb).map_err(accounts_error)?;
    Ok(Tenant { scope, project: Some(project) })
}

/// The user behind a request, for actions only people may take.
fn user_of(who: &Option<Principal>) -> Result<(&zsql_accounts::User, &zsql_accounts::Account, Role), ApiError> {
    match who {
        Some(Principal::User { user, account, role, .. }) => Ok((user, account, *role)),
        Some(Principal::QueryKey { .. }) => Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "a query key cannot do this; use your personal key".into())),
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

fn not_found(uid: &str, branch: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no deployment for project {uid} branch {branch}"))
}

fn model(app: &App, t: &Tenant, uid: &str, branch: &str) -> Result<Arc<Deployed>, ApiError> {
    app.models.read().unwrap().get(&t.key(uid, branch)).cloned().ok_or_else(|| not_found(uid, branch))
}

fn summary(d: &Deployed) -> Value {
    let s = d.universe().snapshot();
    json!({
        "project": d.uid, "branch": d.branch, "deployed_at": d.deployed_at,
        "datasources": s.datasources.len(), "tables": s.tables.len(), "fields": s.fields.len(),
        "joins": s.join_defs.len(), "paths": s.paths.len(), "policies": s.security_policies.len(),
        "values": d.values.as_ref().map(|v| v.dimensions.len()).unwrap_or(0),
        "tests": d.tests.len(),
        "warnings": d.warnings,
    })
}

/// Runs the deployed project's tests/*.yml: each projects its fields and
/// compares the SQL with the assertion.
fn run_tests(app: &App, d: &Deployed) -> Value {
    let planner = d.planner();
    let base = match app.dialects.base() {
        Ok(b) => b,
        Err(e) => return json!({ "error": e.to_string() }),
    };
    let normalize = |sql: &str| sql.to_lowercase().split_whitespace().collect::<Vec<_>>().join(" ");
    let mut results = Vec::new();
    let (mut passed, mut failed) = (0, 0);
    for t in &d.tests {
        let spec = zsql_spec::Spec {
            projections: t.projections.iter().map(|f| zsql_spec::spec::ProjectionSpec { field: Some(Value::String(f.clone())), ..Default::default() }).collect(),
            ..Default::default()
        };
        let outcome = zsql_spec::lower(&spec, d.universe(), &base).map_err(|e| e.to_string()).and_then(|q| planner.plan_query(q, None).map(|p| p.sql).map_err(|e| e.to_string()));
        let (status, detail) = match (&outcome, &t.assert_sql, &t.assert_regex) {
            (Err(e), _, _) => ("error", json!({ "message": e })),
            (Ok(sql), Some(expected), _) => {
                if normalize(sql) == normalize(expected) {
                    ("passed", Value::Null)
                } else {
                    ("failed", json!({ "message": "Generated SQL does not match expected SQL", "expected_sql": expected, "generated_sql": sql }))
                }
            }
            (Ok(sql), None, Some(pattern)) => match regex::RegexBuilder::new(pattern).case_insensitive(true).dot_matches_new_line(true).build() {
                Ok(re) if re.is_match(sql) => ("passed", Value::Null),
                Ok(_) => ("failed", json!({ "message": "Generated SQL does not match regex pattern", "pattern": pattern, "generated_sql": sql })),
                Err(e) => ("error", json!({ "message": format!("bad assert_regex: {e}") })),
            },
            (Ok(_), None, None) => ("error", json!({ "message": "No assertion defined (assert_sql or assert_regex required)" })),
        };
        if status == "passed" {
            passed += 1;
        } else {
            failed += 1;
        }
        let mut r = json!({ "name": t.name, "status": status, "file": t.file.file_name().map(|f| f.to_string_lossy().to_string()) });
        if let Value::Object(extra) = detail {
            for (k, v) in extra {
                r[k] = v;
            }
        }
        results.push(r);
    }
    json!({ "passed": passed, "failed": failed, "results": results })
}

// ------------------------------------------------------------------ deploy

async fn deploy(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>, body: Bytes) -> Result<Json<Value>, ApiError> {
    deploy_or_validate(app, who, uid, branch, body, true)
}

async fn validate(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>, body: Bytes) -> Result<Json<Value>, ApiError> {
    deploy_or_validate(app, who, uid, branch, body, false)
}

/// A user deploying to a project the account does not have yet creates
/// it and owns it; validating needs no project. Query keys cannot deploy.
fn deploy_gate(app: &App, who: &Option<Principal>, uid: &str, branch: &str, keep: bool, production_branch: &str, name: &str) -> Result<Tenant, ApiError> {
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

fn deploy_or_validate(app: Shared, who: Option<Principal>, uid: String, branch: String, body: Bytes, keep: bool) -> Result<Json<Value>, ApiError> {
    let bad = |m: String| ApiError(StatusCode::BAD_REQUEST, "DeployError", m);
    if body.is_empty() {
        return Err(bad("the body must be a tar.gz of the project directory".into()));
    }
    if matches!(who, Some(Principal::QueryKey { .. })) {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "query keys are read only; deploy with your personal key".into()));
    }
    let work = std::env::temp_dir().join(format!("zsqld-deploy-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&work).map_err(|e| bad(e.to_string()))?;
    let result = (|| -> Result<Value, ApiError> {
        let decoder = flate2::read::GzDecoder::new(&body[..]);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(&work).map_err(|e| bad(format!("unpacking the archive: {e}")))?;
        let root = project_root(&work).ok_or_else(|| bad("the archive holds no project.yml".into()))?;
        let loaded = zsql_project::load(&root, app.dialects).map_err(|e| bad(e.to_string()))?;
        let t = deploy_gate(&app, &who, &uid, &branch, keep, &loaded.snapshot.branch.uid, &loaded.snapshot.project.name)?;
        let universe = Universe::build(loaded.snapshot.clone()).map_err(|e| bad(e.to_string()))?;
        let mut warnings = loaded.warnings;
        warnings.extend(universe.warnings().iter().cloned());
        let engine = Engine::new(universe, |u| Planner::new(u, app.dialects, None));
        let deployed = Arc::new(Deployed { uid: uid.clone(), branch: branch.clone(), engine, values: loaded.values, tests: loaded.tests, warnings, deployed_at: now() });
        let mut out = summary(&deployed);
        out["test_results"] = run_tests(&app, &deployed);
        out["validated_only"] = Value::Bool(!keep);
        if keep {
            // Keep the compiled model on disk so a restart reloads it.
            let dir = t.dir(&app, &uid, &branch);
            std::fs::create_dir_all(&dir).map_err(|e| bad(e.to_string()))?;
            write_gz(&dir.join("snapshot.json.gz"), &serde_json::to_vec(&loaded.snapshot).map_err(|e| bad(e.to_string()))?).map_err(|e| bad(e.to_string()))?;
            match &deployed.values {
                Some(v) => v.write(&dir.join(zsql_project::values::FILE_NAME)).map_err(|e| bad(e.to_string()))?,
                None => {
                    let _ = std::fs::remove_file(dir.join(zsql_project::values::FILE_NAME));
                }
            }
            let tests_json = serde_json::to_vec(&deployed.tests.iter().map(|t| json!({ "file": t.file.file_name().map(|f| f.to_string_lossy().to_string()), "name": t.name, "projections": t.projections, "assert_sql": t.assert_sql, "assert_regex": t.assert_regex })).collect::<Vec<_>>()).map_err(|e| bad(e.to_string()))?;
            std::fs::write(dir.join("tests.json"), tests_json).map_err(|e| bad(e.to_string()))?;
            app.models.write().unwrap().insert(t.key(&uid, &branch), deployed);
            if let (Some(store), Some(p)) = (&app.accounts, &who) {
                let _ = store.record(p.account().id, &p.describe(), "deploy", &format!("{uid}/{branch}"));
            }
        }
        Ok(out)
    })();
    let _ = std::fs::remove_dir_all(&work);
    result.map(Json)
}

/// The directory holding project.yml: the archive root, or its single top folder.
fn project_root(work: &std::path::Path) -> Option<PathBuf> {
    if work.join("project.yml").exists() {
        return Some(work.to_path_buf());
    }
    let entries: Vec<PathBuf> = std::fs::read_dir(work).ok()?.filter_map(|e| e.ok().map(|e| e.path())).filter(|p| p.is_dir()).collect();
    entries.into_iter().find(|p| p.join("project.yml").exists())
}

fn write_gz(path: &std::path::Path, bytes: &[u8]) -> std::io::Result<()> {
    use std::io::Write;
    let file = std::fs::File::create(path)?;
    let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
    enc.write_all(bytes)?;
    enc.finish()?;
    Ok(())
}

// --------------------------------------------------------------- sql, explain

#[derive(Deserialize)]
struct SqlRequest {
    spec: Value,
    #[serde(default)]
    context: Option<SecurityContext>,
}

/// What a query needs to run: the statement and the datasource it is for.
/// `corrections` lists references that resolved by near match.
#[derive(Serialize)]
struct SqlResponse {
    sql: String,
    datasource: String,
    datasource_uid: String,
    adapter: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    corrections: Vec<zsql_spec::fuzzy::Correction>,
}

/// The statement, how long the server took to make it, each planning phase's
/// share, and the node graph the visitors shaped.
#[derive(Serialize)]
struct ExplainResponse {
    sql: String,
    datasource: String,
    datasource_uid: String,
    adapter: String,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    corrections: Vec<zsql_spec::fuzzy::Correction>,
    timings: Timings,
    phases: Vec<zsql_planner::Phase>,
    nodes: Vec<zsql_planner::ExplainNode>,
}

#[derive(Serialize)]
/// Microseconds the server spent: parsing and resolving the spec,
/// planning it, and both together.
struct Timings {
    parser_us: u128,
    plan_us: u128,
    total_us: u128,
}

fn plan_request(app: &Shared, who: &Option<Principal>, uid: &str, branch: &str, req: SqlRequest, explain: bool) -> Result<(zsql_planner::Plan, Vec<zsql_spec::fuzzy::Correction>, Timings), ApiError> {
    let t = gate(app, who, uid, branch, Verb::Read)?;
    let d = model(app, &t, uid, branch)?;
    if req.context.is_none() && !d.universe().policies().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "ContextRequired", "this branch has security policies; a context is required to plan".into()));
    }
    let base = app.dialects.base().map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Dialect", e.to_string()))?;
    let t0 = Instant::now();
    let spec = zsql_spec::Spec::from_value(req.spec).map_err(spec_error)?;
    let mut query = zsql_spec::lower(&spec, d.universe(), &base).map_err(spec_error)?;
    let corrections = std::mem::take(&mut query.corrections);
    let t1 = Instant::now();
    let planner = d.planner();
    let plan = planner.plan_query_explained(query, req.context.as_ref(), explain).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.rails_class(), e.to_string()))?;
    let t2 = Instant::now();
    Ok((plan, corrections, Timings { parser_us: (t1 - t0).as_micros(), plan_us: (t2 - t1).as_micros(), total_us: (t2 - t0).as_micros() }))
}

async fn sql(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>, Json(req): Json<SqlRequest>) -> Result<Json<SqlResponse>, ApiError> {
    let (plan, corrections, _) = plan_request(&app, &who, &uid, &branch, req, false)?;
    Ok(Json(SqlResponse { sql: plan.sql, datasource: plan.datasource, datasource_uid: plan.datasource_uid, adapter: plan.adapter, corrections }))
}

async fn explain(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>, Json(req): Json<SqlRequest>) -> Result<Json<ExplainResponse>, ApiError> {
    let (plan, corrections, timings) = plan_request(&app, &who, &uid, &branch, req, true)?;
    let x = plan.explain.unwrap_or_else(|| zsql_planner::Explain { phases: vec![], nodes: vec![] });
    Ok(Json(ExplainResponse { sql: plan.sql, datasource: plan.datasource, datasource_uid: plan.datasource_uid, adapter: plan.adapter, corrections, timings, phases: x.phases, nodes: x.nodes }))
}

/// What a query can take on: the visible dimensions and measures the
/// planner's universes can add to it, from a walk of the formed
/// universes, narrowed by a search term when one is given.
#[derive(Deserialize)]
struct ExploreRequest {
    spec: Value,
    /// Accepted for symmetry with sql and explain; exploring resolves
    /// universes only, so no policy applies yet.
    #[serde(default)]
    #[allow(dead_code)]
    context: Option<SecurityContext>,
    /// Keep only fields whose name, uid or synonyms contain this, or come
    /// close to it.
    #[serde(default)]
    q: Option<String>,
}

#[derive(Serialize)]
struct ExploreResponse {
    dimensions: Vec<Value>,
    measures: Vec<Value>,
    total_us: u128,
}

async fn explore(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>, Json(req): Json<ExploreRequest>) -> Result<Json<ExploreResponse>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Read)?;
    let d = model(&app, &t, &uid, &branch)?;
    let base = app.dialects.base().map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Dialect", e.to_string()))?;
    let t0 = Instant::now();
    let spec = zsql_spec::Spec::from_value(req.spec).map_err(spec_error)?;
    let query = zsql_spec::lower(&spec, d.universe(), &base).map_err(spec_error)?;
    let planner = d.planner();
    let x = planner.explore(query).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.rails_class(), e.to_string()))?;
    let q = req.q.as_deref().map(zsql_spec::fuzzy::normalize).filter(|q| !q.is_empty());
    let wanted = |f: &zsql_semantic::Field| -> Option<f64> {
        let Some(q) = &q else { return Some(1.0) };
        let forms = std::iter::once(&f.name).chain(std::iter::once(&f.uid)).chain(f.synonyms.iter());
        let mut best = 0.0f64;
        for form in forms {
            let n = zsql_spec::fuzzy::normalize(form);
            if n.contains(q.as_str()) {
                return Some(1.0);
            }
            best = best.max(zsql_spec::fuzzy::similarity(q, &n));
        }
        (best >= zsql_spec::fuzzy::FLOOR).then_some(best)
    };
    let describe = |fields: Vec<&zsql_semantic::Field>| -> Vec<Value> {
        let mut out: Vec<(f64, Value)> = fields
            .into_iter()
            .filter(|f| !f.hidden)
            .filter_map(|f| {
                let score = wanted(f)?;
                let mut v = json!({
                    "uid": f.uid, "name": f.name, "data_type": f.data_type, "description": f.description,
                    "tables": d.universe().snapshot().expressions.iter().filter(|e| e.field_id == f.id).filter_map(|e| d.universe().table(e.table_id)).map(|t| t.uid.clone()).collect::<Vec<_>>(),
                });
                if q.is_some() {
                    v["score"] = json!((score * 100.0).round() / 100.0);
                }
                Some((score, v))
            })
            .collect();
        if q.is_some() {
            out.sort_by(|a, b| b.0.total_cmp(&a.0));
        }
        out.into_iter().map(|(_, v)| v).collect()
    };
    Ok(Json(ExploreResponse { dimensions: describe(x.dimensions), measures: describe(x.measures), total_us: t0.elapsed().as_micros() }))
}

fn spec_error(e: zsql_spec::SpecError) -> ApiError {
    ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.rails_class(), e.to_string())
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
async fn fields(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>, Query(q): Query<FieldsQuery>) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Read)?;
    let d = model(&app, &t, &uid, &branch)?;
    let needle = q.q.trim().to_lowercase();
    let visible = |f: &&zsql_semantic::Field| q.hidden || !f.hidden;
    let describe = |f: &zsql_semantic::Field, score: Option<f64>| {
        let mut v = json!({
            "uid": f.uid, "name": f.name,
            "kind": match f.kind { FieldKind::Dimension => "dimension", FieldKind::Measure => "measure" },
            "data_type": f.data_type, "description": f.description, "synonyms": f.synonyms, "tags": f.tags, "hidden": f.hidden,
            "tables": d.universe().snapshot().expressions.iter().filter(|e| e.field_id == f.id).filter_map(|e| d.universe().table(e.table_id)).map(|t| t.uid.clone()).collect::<Vec<_>>(),
        });
        if let Some(s) = score {
            v["score"] = json!((s * 100.0).round() / 100.0);
        }
        v
    };
    if needle.is_empty() {
        return Ok(Json(json!({ "fields": d.universe().fields().iter().filter(visible).map(|f| describe(f, None)).collect::<Vec<_>>() })));
    }
    let mut out: Vec<Value> = d
        .universe()
        .fields()
        .iter()
        .filter(visible)
        .filter(|f| f.uid.to_lowercase().contains(&needle) || f.name.to_lowercase().contains(&needle) || f.synonyms.iter().any(|s| s.to_lowercase().contains(&needle)))
        .map(|f| describe(f, Some(1.0)))
        .collect();
    let seen: std::collections::HashSet<&str> = out.iter().filter_map(|v| v["uid"].as_str()).collect();
    let seen: std::collections::HashSet<String> = seen.into_iter().map(str::to_string).collect();
    for s in zsql_spec::fuzzy::suggest(d.universe(), &needle, &[FieldKind::Dimension, FieldKind::Measure]) {
        if s.score < zsql_spec::fuzzy::FLOOR / 2.0 {
            break;
        }
        if visible(&s.field) && !seen.contains(&s.field.uid) {
            out.push(describe(s.field, Some(s.score)));
        }
    }
    Ok(Json(json!({ "fields": out })))
}

async fn tables(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Read)?;
    let d = model(&app, &t, &uid, &branch)?;
    let out: Vec<Value> = d
        .universe()
        .tables()
        .iter()
        .map(|t| {
            json!({
                "uid": t.uid, "name": t.name, "physical_name": t.physical_name, "cost": t.cost,
                "datasource": d.universe().datasource(t.datasource_id).map(|x| x.uid.clone()),
                "fields": d.universe().expressions_of(t.id).filter_map(|e| d.universe().field(e.field_id)).map(|f| f.uid.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({ "tables": out })))
}

async fn values(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch, dim)): Path<(String, String, String)>) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Read)?;
    let d = model(&app, &t, &uid, &branch)?;
    let field = d.universe().field_by_uid(&dim, Some(FieldKind::Dimension)).or_else(|| d.universe().field_by_name(&dim, FieldKind::Dimension));
    let Some(field) = field else {
        return Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no dimension {dim}")));
    };
    let entry = d.values.as_ref().and_then(|v| v.get(&field.uid));
    match entry {
        Some(e) => Ok(Json(serde_json::to_value(e).unwrap())),
        None => Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no sampled values for {}", field.uid))),
    }
}

async fn test(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Write)?;
    let d = model(&app, &t, &uid, &branch)?;
    Ok(Json(run_tests(&app, &d)))
}

async fn sampling_plan(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>, Query(q): Query<SamplingQuery>) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Write)?;
    let d = model(&app, &t, &uid, &branch)?;
    let dims: Vec<String> = q.dims.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let qualify = |sql: &str, columns: &[String], adapter: &str, alias: &str| -> String {
        match app.dialects.get(adapter) {
            Ok(dl) => zsql_planner::sql::expr::qualify(sql, columns, &dl, alias),
            Err(_) => sql.to_string(),
        }
    };
    let (jobs, skipped) = zsql_project::values::plan(d.universe(), &dims, q.full, &qualify);
    let jobs: Vec<Value> = jobs
        .iter()
        .map(|j| {
            json!({
                "uid": j.uid, "name": j.name, "table": j.table_uid, "datasource": j.datasource_uid, "adapter": j.adapter,
                "mode": j.mode, "cap": j.cap, "distinct_sql": j.distinct_sql, "values_sql": j.values_sql, "range_sql": j.range_sql,
                "sampled_at": d.values.as_ref().and_then(|v| v.get(&j.uid)).map(|e| e.sampled_at.clone()),
            })
        })
        .collect();
    let skipped: Vec<Value> = skipped.iter().map(|(uid, why)| json!({ "uid": uid, "reason": why })).collect();
    Ok(Json(json!({ "jobs": jobs, "skipped": skipped, "full_ceiling": zsql_project::values::FULL_CEILING })))
}

#[derive(Deserialize)]
struct SamplingQuery {
    #[serde(default)]
    dims: String,
    #[serde(default)]
    full: bool,
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

async fn branch_summary(State(app): State<Shared>, Auth(who): Auth, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let t = gate(&app, &who, &uid, &branch, Verb::Read)?;
    let d = model(&app, &t, &uid, &branch)?;
    Ok(Json(summary(&d)))
}

/// Every deployment the caller may read; with accounts on, also the
/// account's projects and the caller's level on each.
async fn projects(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    let mut out = Vec::new();
    for d in app.models.read().unwrap().values() {
        if gate(&app, &who, &d.uid, &d.branch, Verb::Read).is_ok() {
            out.push(summary(d));
        }
    }
    out.sort_by_key(|v| (v["project"].as_str().unwrap_or("").to_string(), v["branch"].as_str().unwrap_or("").to_string()));
    let mut answer = json!({ "deployments": out });
    if let (Some(store), Some(p)) = (&app.accounts, &who) {
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
    Ok(Json(answer))
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
    let store = store_of(&app)?;
    if !app.signup_open {
        return Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", "sign-up is closed on this server".into()));
    }
    let password = req.password.as_deref().filter(|p| !p.is_empty());
    let su = store.signup(&req.account, &req.email, &req.name, password).map_err(accounts_error)?;
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
    let store = store_of(&app)?;
    let user = store
        .verify_password(&req.email, &req.password)
        .map_err(accounts_error)?
        .ok_or_else(|| ApiError(StatusCode::UNAUTHORIZED, "Unauthorized", "wrong email or password".into()))?;
    let (token, account) = store.create_session(&user, SESSION_DAYS).map_err(accounts_error)?;
    Ok(with_session_cookie(&app, json!({ "user": user, "account": account }), Some(&token)))
}

async fn logout(State(app): State<Shared>, parts: axum::http::request::Parts) -> Result<Response, ApiError> {
    let store = store_of(&app)?;
    if let Some(token) = cookie(&parts, SESSION_COOKIE) {
        store.delete_session(&token).map_err(accounts_error)?;
    }
    Ok(with_session_cookie(&app, json!({ "logged_out": true }), None))
}

#[derive(Deserialize)]
struct PasswordRequest {
    password: String,
}

async fn set_password(State(app): State<Shared>, Auth(who): Auth, Json(req): Json<PasswordRequest>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    store.set_password(account.id, user, &req.password).map_err(accounts_error)?;
    Ok(Json(json!({ "ok": true })))
}

async fn me(State(app): State<Shared>, Auth(who): Auth) -> Result<Json<Value>, ApiError> {
    store_of(&app)?;
    Ok(Json(match &who {
        Some(Principal::User { user, account, role, key_id }) => json!({ "kind": "user", "user": user, "account": account, "role": role, "key_id": key_id }),
        Some(Principal::QueryKey { key, account, grants }) => json!({ "kind": "query_key", "key": key, "account": account, "grants": grants }),
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
    if store.user_keys(m.user.id).map_err(accounts_error)?.is_empty() {
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
    let store = store_of(&app)?;
    let password = req.password.as_deref().filter(|p| !p.is_empty());
    let (account, user, key, secret) = store.accept_invite(&req.token, &req.name, &req.key_name, password).map_err(accounts_error)?;
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

fn ui_file(path: &str) -> Response {
    let (body, mime): (&'static str, &str) = match path {
        "app.css" => (include_str!("../ui/app.css"), "text/css; charset=utf-8"),
        "app.js" => (include_str!("../ui/app.js"), "text/javascript; charset=utf-8"),
        _ => (include_str!("../ui/app.html"), "text/html; charset=utf-8"),
    };
    ([("content-type", mime.to_string()), ("cache-control", "no-cache".to_string())], body).into_response()
}

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
    let (user, _, _) = user_of(&who)?;
    Ok(Json(json!({ "keys": store.user_keys(user.id).map_err(accounts_error)? })))
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
fn may_manage_key(store: &Store, who: &Option<Principal>, id: i64) -> Result<(zsql_accounts::User, zsql_accounts::Account, zsql_accounts::QueryKey), ApiError> {
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
}

/// Granting a key read on a project is the project owner's call (or an admin's).
async fn grant(State(app): State<Shared>, Auth(who): Auth, Path(id): Path<i64>, Json(req): Json<GrantRequest>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let key = store.query_key(account.id, id).map_err(accounts_error)?;
    let t = gate(&app, &who, &req.project, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let g = store.grant(account.id, user, key.id, &project, req.branch.as_deref()).map_err(accounts_error)?;
    Ok(Json(json!({ "key": key, "grant": g })))
}

async fn revoke_grant(State(app): State<Shared>, Auth(who): Auth, Path((id, project_uid)): Path<(i64, String)>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, account, _) = user_of(&who)?;
    let t = gate(&app, &who, &project_uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    store.revoke_grant(account.id, user, id, &project).map_err(accounts_error)?;
    Ok(Json(json!({ "revoked": { "key": id, "project": project_uid } })))
}

async fn project_access(State(app): State<Shared>, Auth(who): Auth, Path(uid): Path<String>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let t = gate(&app, &who, &uid, "", Verb::Read)?;
    let project = t.project.expect("accounts on");
    let access: Vec<Value> = store.project_access(&project).map_err(accounts_error)?.into_iter().map(|(u, l)| json!({ "user": u, "level": l })).collect();
    let keys: Vec<Value> = store.keys_granted_on(&project).map_err(accounts_error)?.into_iter().map(|(k, g)| json!({ "key": k, "grant": g })).collect();
    Ok(Json(json!({ "project": project, "access": access, "query_keys": keys })))
}

#[derive(Deserialize)]
struct AccessRequest {
    email: String,
    level: String,
}

async fn set_access(State(app): State<Shared>, Auth(who): Auth, Path(uid): Path<String>, Json(req): Json<AccessRequest>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let level = zsql_accounts::parse_level(&req.level).map_err(accounts_error)?;
    let (u, l) = store.set_access(user, &project, &req.email, level).map_err(accounts_error)?;
    Ok(Json(json!({ "user": u, "level": l })))
}

async fn remove_access(State(app): State<Shared>, Auth(who): Auth, Path((uid, email)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
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

async fn update_project(State(app): State<Shared>, Auth(who): Auth, Path(uid): Path<String>, Json(req): Json<ProjectPatch>) -> Result<Json<Value>, ApiError> {
    let store = store_of(&app)?;
    let (user, _, _) = user_of(&who)?;
    let t = gate(&app, &who, &uid, "", Verb::Owner)?;
    let project = t.project.expect("accounts on");
    let p = store.update_project(user, &project, req.name.as_deref(), req.production_branch.as_deref(), req.visibility.as_deref(), req.protected_production).map_err(accounts_error)?;
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

fn reload(app: &App) -> anyhow::Result<usize> {
    let mut n = 0;
    // Single-tenant: data/<project>/<branch>; with accounts: data/<account>/<project>/<branch>.
    let roots: Vec<(String, PathBuf)> = if app.accounts.is_some() {
        let Ok(accounts) = std::fs::read_dir(&app.data_dir) else { return Ok(0) };
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
            let snap_path = b.path().join("snapshot.json.gz");
            if !snap_path.exists() {
                continue;
            }
            let snapshot = Snapshot::from_path(&snap_path)?;
            let universe = Universe::build(snapshot)?;
            let values = ValueCatalog::read(&b.path().join(zsql_project::values::FILE_NAME))?;
            let tests: Vec<zsql_project::ProjectTest> = std::fs::read_to_string(b.path().join("tests.json"))
                .ok()
                .and_then(|t| serde_json::from_str::<Vec<Value>>(&t).ok())
                .unwrap_or_default()
                .into_iter()
                .map(|v| zsql_project::ProjectTest {
                    file: PathBuf::from(v["file"].as_str().unwrap_or("")),
                    name: v["name"].as_str().unwrap_or("").to_string(),
                    projections: v["projections"].as_array().into_iter().flatten().filter_map(|p| p.as_str().map(str::to_string)).collect(),
                    assert_sql: v["assert_sql"].as_str().map(str::to_string),
                    assert_regex: v["assert_regex"].as_str().map(str::to_string),
                })
                .collect();
            let deployed_at = std::fs::metadata(&snap_path).and_then(|m| m.modified()).map(|t| humantime(t)).unwrap_or_default();
            let engine = Engine::new(universe, |u| Planner::new(u, app.dialects, None));
            app.models.write().unwrap().insert(format!("{scope}{uid}/{branch}"), Arc::new(Deployed { uid: uid.clone(), branch, engine, values, tests, warnings: vec![], deployed_at }));
            n += 1;
        }
    }
    }
    Ok(n)
}

fn nanos() -> u128 {
    std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0)
}

fn now() -> String {
    humantime(std::time::SystemTime::now())
}

/// UTC, to the minute: `2026-10-01 15:04`.
fn humantime(t: std::time::SystemTime) -> String {
    let secs = t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    // Civil-from-days (Howard Hinnant), so no date crate is needed here.
    let days = secs.div_euclid(86400);
    let rem = secs.rem_euclid(86400);
    let z = days + 719468;
    let era = z.div_euclid(146097);
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}", rem / 3600, (rem % 3600) / 60)
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(std::env::var("ZSQL_DATA_DIR").unwrap_or_else(|_| "data".into()));
    let bind = std::env::var("ZSQL_BIND").unwrap_or_else(|_| "127.0.0.1:3699".into());
    std::fs::create_dir_all(&data_dir)?;
    let dialects: &'static Dialects = Box::leak(Box::new(Dialects::builtin()));
    let accounts_on = matches!(std::env::var("ZSQL_ACCOUNTS").as_deref(), Ok("on") | Ok("1") | Ok("true"));
    let accounts = if accounts_on { Some(Store::open(&data_dir.join("accounts.db"))?) } else { None };
    let signup_open = !matches!(std::env::var("ZSQL_SIGNUP").as_deref(), Ok("closed") | Ok("off") | Ok("0"));
    let secure_cookies = std::env::var("ZSQL_PUBLIC_URL").map(|u| u.starts_with("https://")).unwrap_or(false);
    let app: Shared = Arc::new(App { dialects, data_dir, models: RwLock::new(HashMap::new()), accounts, signup_open, secure_cookies });
    let n = reload(&app)?;
    eprintln!("zsqld: {n} deployment(s) loaded from {}; accounts {}", app.data_dir.display(), if accounts_on { if signup_open { "on, sign-up open" } else { "on, sign-up closed" } } else { "off" });

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/projects", get(projects))
        .route("/projects/{uid}/branches/{branch}", get(branch_summary))
        .route("/projects/{uid}/branches/{branch}/deploy", post(deploy))
        .route("/projects/{uid}/branches/{branch}/validate", post(validate))
        .route("/projects/{uid}/branches/{branch}/test", post(test))
        .route("/projects/{uid}/branches/{branch}/sampling-plan", get(sampling_plan))
        .route("/projects/{uid}/branches/{branch}", delete(remove))
        .route("/projects/{uid}/branches/{branch}/sql", post(sql))
        .route("/projects/{uid}/branches/{branch}/explain", post(explain))
        .route("/projects/{uid}/branches/{branch}/explore", post(explore))
        .route("/projects/{uid}/branches/{branch}/fields", get(fields))
        .route("/projects/{uid}/branches/{branch}/tables", get(tables))
        .route("/projects/{uid}/branches/{branch}/values/{dim}", get(values))
        .route("/", get(root))
        .route("/app", get(ui_root))
        .route("/app/", get(ui_root))
        .route("/app/{*rest}", get(ui))
        .route("/signup", post(signup))
        .route("/login", post(login))
        .route("/logout", post(logout))
        .route("/account/password", post(set_password))
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
        .route("/projects/{uid}/access/{email}", delete(remove_access))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("zsqld: listening on http://{bind}");
    axum::serve(listener, router).await?;
    Ok(())
}
