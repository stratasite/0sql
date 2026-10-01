//! zsqld: a light service. Deploy a project directory (as a tar.gz), then
//! plan specs against it. No accounts, users or keys yet; deployed models
//! live in memory and under a data directory so a restart reloads them.
//!
//! ```text
//! POST /projects/{uid}/branches/{branch}/deploy    body: tar.gz of the project
//! POST /projects/{uid}/branches/{branch}/plan      {"spec": {...}, "context": {...}}
//! GET  /projects/{uid}/branches/{branch}           summary and warnings
//! GET  /projects/{uid}/branches/{branch}/fields    ?q=text&hidden=true
//! GET  /projects/{uid}/branches/{branch}/tables
//! GET  /projects/{uid}/branches/{branch}/values/{dim}
//! GET  /projects                                   every deployed branch
//! GET  /healthz
//! ```

use axum::body::Bytes;
use axum::extract::{Path, Query, State};
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use axum::routing::{get, post};
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

struct Deployed {
    uid: String,
    branch: String,
    universe: Universe,
    values: Option<ValueCatalog>,
    warnings: Vec<String>,
    deployed_at: String,
}

struct App {
    dialects: Dialects,
    data_dir: PathBuf,
    models: RwLock<HashMap<String, Arc<Deployed>>>,
}

type Shared = Arc<App>;

fn key(uid: &str, branch: &str) -> String {
    format!("{uid}/{branch}")
}

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

fn model(app: &App, uid: &str, branch: &str) -> Result<Arc<Deployed>, ApiError> {
    app.models.read().unwrap().get(&key(uid, branch)).cloned().ok_or_else(|| not_found(uid, branch))
}

fn summary(d: &Deployed) -> Value {
    let s = d.universe.snapshot();
    json!({
        "project": d.uid, "branch": d.branch, "deployed_at": d.deployed_at,
        "datasources": s.datasources.len(), "tables": s.tables.len(), "fields": s.fields.len(),
        "joins": s.join_defs.len(), "paths": s.paths.len(), "policies": s.security_policies.len(),
        "values": d.values.as_ref().map(|v| v.dimensions.len()).unwrap_or(0),
        "warnings": d.warnings,
    })
}

// ------------------------------------------------------------------ deploy

async fn deploy(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, body: Bytes) -> Result<Json<Value>, ApiError> {
    let bad = |m: String| ApiError(StatusCode::BAD_REQUEST, "DeployError", m);
    if body.is_empty() {
        return Err(bad("the body must be a tar.gz of the project directory".into()));
    }
    let work = std::env::temp_dir().join(format!("zsqld-deploy-{}-{}", std::process::id(), nanos()));
    std::fs::create_dir_all(&work).map_err(|e| bad(e.to_string()))?;
    let result = (|| -> Result<Value, ApiError> {
        let decoder = flate2::read::GzDecoder::new(&body[..]);
        let mut archive = tar::Archive::new(decoder);
        archive.unpack(&work).map_err(|e| bad(format!("unpacking the archive: {e}")))?;
        let root = project_root(&work).ok_or_else(|| bad("the archive holds no project.yml".into()))?;
        let loaded = zsql_project::load(&root, &app.dialects).map_err(|e| bad(e.to_string()))?;
        let universe = Universe::build(loaded.snapshot.clone()).map_err(|e| bad(e.to_string()))?;
        // Keep the compiled model on disk so a restart reloads it.
        let dir = app.data_dir.join(&uid).join(&branch);
        std::fs::create_dir_all(&dir).map_err(|e| bad(e.to_string()))?;
        write_gz(&dir.join("snapshot.json.gz"), &serde_json::to_vec(&loaded.snapshot).map_err(|e| bad(e.to_string()))?).map_err(|e| bad(e.to_string()))?;
        match &loaded.values {
            Some(v) => v.write(&dir.join(zsql_project::values::FILE_NAME)).map_err(|e| bad(e.to_string()))?,
            None => {
                let _ = std::fs::remove_file(dir.join(zsql_project::values::FILE_NAME));
            }
        }
        let mut warnings = loaded.warnings;
        warnings.extend(universe.warnings().iter().cloned());
        let deployed = Arc::new(Deployed { uid: uid.clone(), branch: branch.clone(), universe, values: loaded.values, warnings, deployed_at: now() });
        let out = summary(&deployed);
        app.models.write().unwrap().insert(key(&uid, &branch), deployed);
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

// -------------------------------------------------------------------- plan

#[derive(Deserialize)]
struct PlanRequest {
    spec: Value,
    #[serde(default)]
    context: Option<SecurityContext>,
    /// Also return each node's identity string.
    #[serde(default)]
    explain: bool,
}

#[derive(Serialize)]
struct PlanResponse {
    sql: String,
    adapter: String,
    timings_us: Timings,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    identities: Vec<(String, String)>,
}

#[derive(Serialize)]
struct Timings {
    lower: u128,
    plan: u128,
}

async fn plan(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, Json(req): Json<PlanRequest>) -> Result<Json<PlanResponse>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    if req.context.is_none() && !d.universe.policies().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "ContextRequired", "this branch has security policies; a context is required to plan".into()));
    }
    let base = app.dialects.base().map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Dialect", e.to_string()))?;
    let t0 = Instant::now();
    let spec = zsql_spec::Spec::from_value(req.spec).map_err(spec_error)?;
    let query = zsql_spec::lower(&spec, &d.universe, &base).map_err(spec_error)?;
    let t1 = Instant::now();
    let planner = zsql_planner::Planner::new(&d.universe, &app.dialects, None);
    let plan = planner.plan_query_explained(query, req.context.as_ref(), req.explain).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.rails_class(), e.to_string()))?;
    let t2 = Instant::now();
    Ok(Json(PlanResponse {
        sql: plan.sql,
        adapter: plan.adapter,
        timings_us: Timings { lower: (t1 - t0).as_micros(), plan: (t2 - t1).as_micros() },
        identities: plan.identities,
    }))
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

async fn fields(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, Query(q): Query<FieldsQuery>) -> Result<Json<Value>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    let needle = q.q.to_lowercase();
    let out: Vec<Value> = d
        .universe
        .fields()
        .iter()
        .filter(|f| q.hidden || !f.hidden)
        .filter(|f| needle.is_empty() || f.uid.to_lowercase().contains(&needle) || f.name.to_lowercase().contains(&needle) || f.synonyms.iter().any(|s| s.to_lowercase().contains(&needle)))
        .map(|f| {
            json!({
                "uid": f.uid, "name": f.name,
                "kind": match f.kind { FieldKind::Dimension => "dimension", FieldKind::Measure => "measure" },
                "data_type": f.data_type, "description": f.description, "synonyms": f.synonyms, "tags": f.tags, "hidden": f.hidden,
                "tables": d.universe.snapshot().expressions.iter().filter(|e| e.field_id == f.id).filter_map(|e| d.universe.table(e.table_id)).map(|t| t.uid.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({ "fields": out })))
}

async fn tables(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    let out: Vec<Value> = d
        .universe
        .tables()
        .iter()
        .map(|t| {
            json!({
                "uid": t.uid, "name": t.name, "physical_name": t.physical_name, "cost": t.cost,
                "datasource": d.universe.datasource(t.datasource_id).map(|x| x.uid.clone()),
                "fields": d.universe.expressions_of(t.id).filter_map(|e| d.universe.field(e.field_id)).map(|f| f.uid.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(json!({ "tables": out })))
}

async fn values(State(app): State<Shared>, Path((uid, branch, dim)): Path<(String, String, String)>) -> Result<Json<Value>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    let field = d.universe.field_by_uid(&dim, Some(FieldKind::Dimension)).or_else(|| d.universe.field_by_name(&dim, FieldKind::Dimension));
    let Some(field) = field else {
        return Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no dimension {dim}")));
    };
    let entry = d.values.as_ref().and_then(|v| v.get(&field.uid));
    match entry {
        Some(e) => Ok(Json(serde_json::to_value(e).unwrap())),
        None => Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no sampled values for {}", field.uid))),
    }
}

async fn branch_summary(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    Ok(Json(summary(&d)))
}

async fn projects(State(app): State<Shared>) -> Json<Value> {
    let models = app.models.read().unwrap();
    let mut out: Vec<Value> = models.values().map(|d| summary(d)).collect();
    out.sort_by_key(|v| (v["project"].as_str().unwrap_or("").to_string(), v["branch"].as_str().unwrap_or("").to_string()));
    Json(json!({ "deployments": out }))
}

async fn healthz() -> &'static str {
    "ok"
}

// ----------------------------------------------------------------- startup

fn reload(app: &App) -> anyhow::Result<usize> {
    let mut n = 0;
    let Ok(projects) = std::fs::read_dir(&app.data_dir) else { return Ok(0) };
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
            let deployed_at = std::fs::metadata(&snap_path).and_then(|m| m.modified()).map(|t| humantime(t)).unwrap_or_default();
            app.models.write().unwrap().insert(key(&uid, &branch), Arc::new(Deployed { uid: uid.clone(), branch, universe, values, warnings: vec![], deployed_at }));
            n += 1;
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

/// Seconds since the epoch as text; good enough for a test server.
fn humantime(t: std::time::SystemTime) -> String {
    t.duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs().to_string()).unwrap_or_default()
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let data_dir = PathBuf::from(std::env::var("ZSQL_DATA_DIR").unwrap_or_else(|_| "data".into()));
    let bind = std::env::var("ZSQL_BIND").unwrap_or_else(|_| "127.0.0.1:8080".into());
    std::fs::create_dir_all(&data_dir)?;
    let app: Shared = Arc::new(App { dialects: Dialects::builtin(), data_dir, models: RwLock::new(HashMap::new()) });
    let n = reload(&app)?;
    eprintln!("zsqld: {n} deployment(s) loaded from {}", app.data_dir.display());

    let router = Router::new()
        .route("/healthz", get(healthz))
        .route("/projects", get(projects))
        .route("/projects/{uid}/branches/{branch}", get(branch_summary))
        .route("/projects/{uid}/branches/{branch}/deploy", post(deploy))
        .route("/projects/{uid}/branches/{branch}/plan", post(plan))
        .route("/projects/{uid}/branches/{branch}/fields", get(fields))
        .route("/projects/{uid}/branches/{branch}/tables", get(tables))
        .route("/projects/{uid}/branches/{branch}/values/{dim}", get(values))
        .layer(axum::extract::DefaultBodyLimit::max(256 * 1024 * 1024))
        .with_state(app);
    let listener = tokio::net::TcpListener::bind(&bind).await?;
    eprintln!("zsqld: listening on http://{bind}");
    axum::serve(listener, router).await?;
    Ok(())
}
