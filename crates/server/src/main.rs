//! zsqld: a light service. Deploy a project directory (as a tar.gz), then
//! plan specs against it. No accounts, users or keys yet; deployed models
//! live in memory and under a data directory so a restart reloads them.
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
use axum::extract::{Path, Query, State};
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

struct Deployed {
    uid: String,
    branch: String,
    universe: Universe,
    values: Option<ValueCatalog>,
    tests: Vec<zsql_project::ProjectTest>,
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
        "tests": d.tests.len(),
        "warnings": d.warnings,
    })
}

/// Runs the deployed project's tests/*.yml: each projects its fields and
/// compares the SQL with the assertion.
fn run_tests(app: &App, d: &Deployed) -> Value {
    let planner = zsql_planner::Planner::new(&d.universe, &app.dialects, None);
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
        let outcome = zsql_spec::lower(&spec, &d.universe, &base).map_err(|e| e.to_string()).and_then(|q| planner.plan_query(q, None).map(|p| p.sql).map_err(|e| e.to_string()));
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

async fn deploy(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, body: Bytes) -> Result<Json<Value>, ApiError> {
    deploy_or_validate(app, uid, branch, body, true)
}

async fn validate(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, body: Bytes) -> Result<Json<Value>, ApiError> {
    deploy_or_validate(app, uid, branch, body, false)
}

fn deploy_or_validate(app: Shared, uid: String, branch: String, body: Bytes, keep: bool) -> Result<Json<Value>, ApiError> {
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
        let mut warnings = loaded.warnings;
        warnings.extend(universe.warnings().iter().cloned());
        let deployed = Arc::new(Deployed { uid: uid.clone(), branch: branch.clone(), universe, values: loaded.values, tests: loaded.tests, warnings, deployed_at: now() });
        let mut out = summary(&deployed);
        out["test_results"] = run_tests(&app, &deployed);
        out["validated_only"] = Value::Bool(!keep);
        if keep {
            // Keep the compiled model on disk so a restart reloads it.
            let dir = app.data_dir.join(&uid).join(&branch);
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
            app.models.write().unwrap().insert(key(&uid, &branch), deployed);
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

fn plan_request(app: &Shared, uid: &str, branch: &str, req: SqlRequest, explain: bool) -> Result<(zsql_planner::Plan, Vec<zsql_spec::fuzzy::Correction>, Timings), ApiError> {
    let d = model(app, uid, branch)?;
    if req.context.is_none() && !d.universe.policies().is_empty() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "ContextRequired", "this branch has security policies; a context is required to plan".into()));
    }
    let base = app.dialects.base().map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Dialect", e.to_string()))?;
    let t0 = Instant::now();
    let spec = zsql_spec::Spec::from_value(req.spec).map_err(spec_error)?;
    let mut query = zsql_spec::lower(&spec, &d.universe, &base).map_err(spec_error)?;
    let corrections = std::mem::take(&mut query.corrections);
    let t1 = Instant::now();
    let planner = zsql_planner::Planner::new(&d.universe, &app.dialects, None);
    let plan = planner.plan_query_explained(query, req.context.as_ref(), explain).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.rails_class(), e.to_string()))?;
    let t2 = Instant::now();
    Ok((plan, corrections, Timings { parser_us: (t1 - t0).as_micros(), plan_us: (t2 - t1).as_micros(), total_us: (t2 - t0).as_micros() }))
}

async fn sql(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, Json(req): Json<SqlRequest>) -> Result<Json<SqlResponse>, ApiError> {
    let (plan, corrections, _) = plan_request(&app, &uid, &branch, req, false)?;
    Ok(Json(SqlResponse { sql: plan.sql, datasource: plan.datasource, datasource_uid: plan.datasource_uid, adapter: plan.adapter, corrections }))
}

async fn explain(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, Json(req): Json<SqlRequest>) -> Result<Json<ExplainResponse>, ApiError> {
    let (plan, corrections, timings) = plan_request(&app, &uid, &branch, req, true)?;
    let x = plan.explain.unwrap_or_else(|| zsql_planner::Explain { phases: vec![], nodes: vec![] });
    Ok(Json(ExplainResponse { sql: plan.sql, datasource: plan.datasource, datasource_uid: plan.datasource_uid, adapter: plan.adapter, corrections, timings, phases: x.phases, nodes: x.nodes }))
}

/// The dimensions a query can take on: every visible dimension the
/// planner's universes can add to it, from a walk of the formed universes.
#[derive(Serialize)]
struct DrillResponse {
    dimensions: Vec<Value>,
    total_us: u128,
}

async fn drill(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, Json(req): Json<SqlRequest>) -> Result<Json<DrillResponse>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    let base = app.dialects.base().map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Dialect", e.to_string()))?;
    let t0 = Instant::now();
    let spec = zsql_spec::Spec::from_value(req.spec).map_err(spec_error)?;
    let query = zsql_spec::lower(&spec, &d.universe, &base).map_err(spec_error)?;
    let planner = zsql_planner::Planner::new(&d.universe, &app.dialects, None);
    let fields = planner.drillable(query).map_err(|e| ApiError(StatusCode::UNPROCESSABLE_ENTITY, e.rails_class(), e.to_string()))?;
    let dimensions = fields
        .into_iter()
        .filter(|f| !f.hidden)
        .map(|f| {
            json!({
                "uid": f.uid, "name": f.name, "data_type": f.data_type, "description": f.description,
                "tables": d.universe.snapshot().expressions.iter().filter(|e| e.field_id == f.id).filter_map(|e| d.universe.table(e.table_id)).map(|t| t.uid.clone()).collect::<Vec<_>>(),
            })
        })
        .collect();
    Ok(Json(DrillResponse { dimensions, total_us: t0.elapsed().as_micros() }))
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
async fn fields(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, Query(q): Query<FieldsQuery>) -> Result<Json<Value>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    let needle = q.q.trim().to_lowercase();
    let visible = |f: &&zsql_semantic::Field| q.hidden || !f.hidden;
    let describe = |f: &zsql_semantic::Field, score: Option<f64>| {
        let mut v = json!({
            "uid": f.uid, "name": f.name,
            "kind": match f.kind { FieldKind::Dimension => "dimension", FieldKind::Measure => "measure" },
            "data_type": f.data_type, "description": f.description, "synonyms": f.synonyms, "tags": f.tags, "hidden": f.hidden,
            "tables": d.universe.snapshot().expressions.iter().filter(|e| e.field_id == f.id).filter_map(|e| d.universe.table(e.table_id)).map(|t| t.uid.clone()).collect::<Vec<_>>(),
        });
        if let Some(s) = score {
            v["score"] = json!((s * 100.0).round() / 100.0);
        }
        v
    };
    if needle.is_empty() {
        return Ok(Json(json!({ "fields": d.universe.fields().iter().filter(visible).map(|f| describe(f, None)).collect::<Vec<_>>() })));
    }
    let mut out: Vec<Value> = d
        .universe
        .fields()
        .iter()
        .filter(visible)
        .filter(|f| f.uid.to_lowercase().contains(&needle) || f.name.to_lowercase().contains(&needle) || f.synonyms.iter().any(|s| s.to_lowercase().contains(&needle)))
        .map(|f| describe(f, Some(1.0)))
        .collect();
    let seen: std::collections::HashSet<&str> = out.iter().filter_map(|v| v["uid"].as_str()).collect();
    let seen: std::collections::HashSet<String> = seen.into_iter().map(str::to_string).collect();
    for s in zsql_spec::fuzzy::suggest(&d.universe, &needle, &[FieldKind::Dimension, FieldKind::Measure]) {
        if s.score < zsql_spec::fuzzy::FLOOR / 2.0 {
            break;
        }
        if visible(&s.field) && !seen.contains(&s.field.uid) {
            out.push(describe(s.field, Some(s.score)));
        }
    }
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

async fn test(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    Ok(Json(run_tests(&app, &d)))
}

async fn sampling_plan(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>, Query(q): Query<SamplingQuery>) -> Result<Json<Value>, ApiError> {
    let d = model(&app, &uid, &branch)?;
    let dims: Vec<String> = q.dims.split(',').map(|s| s.trim().to_string()).filter(|s| !s.is_empty()).collect();
    let qualify = |sql: &str, columns: &[String], adapter: &str, alias: &str| -> String {
        match app.dialects.get(adapter) {
            Ok(dl) => zsql_planner::sql::expr::qualify(sql, columns, &dl, alias),
            Err(_) => sql.to_string(),
        }
    };
    let (jobs, skipped) = zsql_project::values::plan(&d.universe, &dims, q.full, &qualify);
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

async fn remove(State(app): State<Shared>, Path((uid, branch)): Path<(String, String)>) -> Result<Json<Value>, ApiError> {
    let removed = app.models.write().unwrap().remove(&key(&uid, &branch)).is_some();
    if !removed {
        return Err(not_found(&uid, &branch));
    }
    let _ = std::fs::remove_dir_all(app.data_dir.join(&uid).join(&branch));
    Ok(Json(json!({ "removed": key(&uid, &branch) })))
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
            app.models.write().unwrap().insert(key(&uid, &branch), Arc::new(Deployed { uid: uid.clone(), branch, universe, values, tests, warnings: vec![], deployed_at }));
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
    let bind = std::env::var("ZSQL_BIND").unwrap_or_else(|_| "127.0.0.1:3699".into());
    std::fs::create_dir_all(&data_dir)?;
    let app: Shared = Arc::new(App { dialects: Dialects::builtin(), data_dir, models: RwLock::new(HashMap::new()) });
    let n = reload(&app)?;
    eprintln!("zsqld: {n} deployment(s) loaded from {}", app.data_dir.display());

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
        .route("/projects/{uid}/branches/{branch}/drill", post(drill))
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
