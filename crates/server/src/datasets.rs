//! Datasets: the routes of data prep, and nothing else of it. What a
//! dataset is, where it is kept and how it is made is `zsql-prep`'s;
//! this module reads the owner from its header, checks the names, and
//! answers.
//!
//! ```text
//! GET    /projects/{uid}/datasets                         this owner's datasets
//! POST   /projects/{uid}/datasets                         body: a CSV  ?name=&file_name=&delimiter=&quote=&header=&max_bad_rows=
//! POST   /projects/{uid}/datasets/query                   {"name", "branch", "spec" | "expr", "context", "connection", "wait"}: a dataset from a semantic query
//! POST   /projects/{uid}/datasets/{id}/refresh            {"context", "connection", "wait"}: its query run again, fresh
//! GET    /projects/{uid}/datasets/{id}
//! DELETE /projects/{uid}/datasets/{id}                    a hard delete; ?confirm=true when others read it
//! GET    /projects/{uid}/datasets/{id}/downstream         the datasets that read it, in the order they run
//! PUT    /projects/{uid}/datasets/{id}/upload             body: a CSV, a new snapshot, and the recipe run over it
//! GET    /projects/{uid}/datasets/{id}/rows               ?offset=&limit=&version=&sort=col:desc,col&filter=<formula>
//! GET    /projects/{uid}/datasets/{id}/lineage            ?version=
//! GET    /projects/{uid}/datasets/{id}/recipe             ?version=
//! POST   /projects/{uid}/datasets/{id}/recipe             {"against": m, "roles": {...}, "steps": [...]}
//! POST   /projects/{uid}/datasets/{id}/preview            {"steps": [...], "roles": {...}, "upto": n, "limit": n}: over a sample, kept nowhere
//! GET    /projects/{uid}/datasets/{id}/impact             ?column=: what reads it
//! GET    /projects/{uid}/datasets/{id}/edits              the edit log, and how each edit fared
//! POST   /projects/{uid}/datasets/{id}/edits              {"against": m, "edits": [{"op": "cell" | "delete" | "add" | "undo", ...}]}
//! POST   /projects/{uid}/datasets/{id}/confirm            {"against": m}: past a hold, without the steps that held it
//! GET    /projects/{uid}/datasets/{id}/sources/{source}   ?snapshot=
//! DELETE /datasets                                        every dataset of this owner in the org
//! POST   /datasets/restore                                every dataset of the org the object store has and this node does not
//! POST   /projects/{uid}/branches/{branch}/blend          {"spec", "datasets": [{"id", "as"}], "blend": [...], "context", "connection", "wait", "fresh", "offset", "limit"}
//! ```
//!
//! Every one needs the [`OWNER_HEADER`]: the application's reference
//! for whose datasets these are, trusted as the security context is.

use crate::error::{ApiError, Input, Org, Params, Search};
use crate::names::check_uid;
use crate::{blocking, Shared};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use zsql_exec::{Record, Status};
use zsql_prep::{Arrived, Change, CsvOptions, Owner, Prep, Preview, Query, Role, Scope, SemanticRead, SortKey, Upload, View};
use zsql_spec::SecurityContext;

/// The header that names whose datasets a request is about.
pub const OWNER_HEADER: &str = "zsql-owner";

impl From<zsql_prep::Error> for ApiError {
    fn from(e: zsql_prep::Error) -> Self {
        use zsql_prep::Error::*;
        match e {
            Invalid(m) => ApiError(StatusCode::BAD_REQUEST, "Invalid", m),
            Unprocessable(m) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable", m),
            NotFound(m) => ApiError(StatusCode::NOT_FOUND, "NotFound", m),
            Conflict(m) => ApiError(StatusCode::CONFLICT, "Conflict", m),
            Internal(m) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", m),
        }
    }
}

/// Data prep, unless the server was started with it off.
fn prep(app: &Shared) -> Result<Arc<Prep>, ApiError> {
    app.prep
        .clone()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "NotFound", "datasets are turned off on this server (ZSQL_DATASETS=off)".into()))
}

fn owner(headers: &HeaderMap) -> Result<Owner, ApiError> {
    let raw = headers.get(OWNER_HEADER).map(|v| v.to_str().map_err(|_| format!("the {OWNER_HEADER} header is not text")));
    match raw {
        None => Err(ApiError(
            StatusCode::BAD_REQUEST,
            "Invalid",
            format!("datasets belong to an owner: send the {OWNER_HEADER} header with the application's reference for the user"),
        )),
        Some(Err(m)) => Err(ApiError(StatusCode::BAD_REQUEST, "Invalid", m)),
        Some(Ok(text)) => Ok(Owner::of(text)?),
    }
}

/// Who a request on a project's datasets is for, checked.
fn scope(app: &Shared, org: String, uid: String, headers: &HeaderMap) -> Result<(Arc<Prep>, Scope), ApiError> {
    let prep = prep(app)?;
    check_uid(&uid)?;
    Ok((prep, Scope { org, project: uid, owner: owner(headers)? }))
}

/// A dataset as answered: its manifest, without the owner's hash.
fn shown(d: &zsql_prep::Dataset) -> Value {
    let mut v = serde_json::to_value(d).unwrap_or(Value::Null);
    if let Some(o) = v.as_object_mut() {
        o.remove("owner");
    }
    v
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct UploadQuery {
    name: Option<String>,
    file_name: Option<String>,
    delimiter: Option<char>,
    quote: Option<char>,
    header: Option<bool>,
    max_bad_rows: Option<usize>,
}

impl UploadQuery {
    fn options(&self) -> CsvOptions {
        CsvOptions { delimiter: self.delimiter, quote: self.quote, header: self.header, max_bad_rows: self.max_bad_rows }
    }
}

pub(crate) async fn list(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || Ok(Json(json!({ "datasets": prep.list(&scope).iter().map(shown).collect::<Vec<_>>() })))).await
}

pub(crate) async fn create(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
    Search(q): Search<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    // A dataset is a project's, so the project has to be one: deployed
    // on some branch of this org.
    if !app.of_org(&scope.org).iter().any(|m| m.uid == scope.project) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("no project {} is deployed; deploy a branch of it before adding datasets", scope.project),
        ));
    }
    blocking(move || {
        let upload = Upload { file_name: q.file_name.clone(), bytes: &body, options: q.options() };
        let d = prep.create(&scope, q.name.as_deref(), upload)?;
        Ok((StatusCode::CREATED, Json(shown(&d))).into_response())
    })
    .await
}

/// A dataset; or, for one whose query is still running or failed
/// after its request was answered, what it waits on.
pub(crate) async fn get(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        let pending = prep.pending(&scope, &id);
        match prep.get(&scope, &id) {
            Ok(d) => {
                let mut v = shown(&d);
                if let Some(p) = pending {
                    v["pending"] = json!(p);
                }
                Ok(Json(v).into_response())
            }
            Err(zsql_prep::Error::NotFound(_)) if pending.is_some() => {
                Ok((StatusCode::ACCEPTED, Json(json!({ "id": id, "pending": pending }))).into_response())
            }
            Err(e) => Err(e.into()),
        }
    })
    .await
}

/// A dataset made from a semantic query.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct FromQuery {
    #[serde(default)]
    name: Option<String>,
    branch: String,
    #[serde(default)]
    spec: Option<Value>,
    #[serde(default)]
    expr: Option<String>,
    #[serde(default)]
    context: Option<SecurityContext>,
    #[serde(default)]
    connection: Map<String, Value>,
    #[serde(default)]
    wait: Option<f64>,
}

/// A semantic source run again.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct Refresh {
    #[serde(default)]
    context: Option<SecurityContext>,
    #[serde(default)]
    connection: Map<String, Value>,
    #[serde(default)]
    wait: Option<f64>,
}

/// What the rows of a query become.
#[derive(Clone)]
enum Then {
    Create { name: Option<String> },
    Refresh,
}

/// How long a query a request stopped waiting for is waited on after.
const AFTER: Duration = Duration::from_secs(3600);

pub(crate) async fn from_query(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
    headers: HeaderMap,
    Input(req): Input<FromQuery>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    if req.spec.is_some() == req.expr.is_some() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Invalid", "a query is a spec or an expr, one of them".into()));
    }
    let query = Query { branch: req.branch, spec: req.spec, expr: req.expr };
    let id = zsql_prep::new_id(&scope.project);
    run_into(app, prep, scope, id, query, (req.context, req.connection, req.wait), Then::Create { name: req.name }).await
}

pub(crate) async fn refresh(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
    Input(req): Input<Refresh>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    let query = {
        let (prep, scope, id) = (prep.clone(), scope.clone(), id.clone());
        blocking(move || Ok(prep.query(&scope, &id)?)).await?
    };
    run_into(app, prep, scope, id, query, (req.context, req.connection, req.wait), Then::Refresh).await
}

/// Runs a query fresh, under the context the request sends, and makes
/// its rows a snapshot. A run that outlasts the request's wait is
/// answered 202 and waited on after; the dataset says what it waits on.
async fn run_into(
    app: Shared,
    prep: Arc<Prep>,
    scope: Scope,
    id: String,
    query: Query,
    (context, connection, wait): (Option<SecurityContext>, Map<String, Value>, Option<f64>),
    then: Then,
) -> Result<Response, ApiError> {
    crate::names::check_branch(&query.branch)?;
    let fingerprint = zsql_exec::sha256(&serde_json::to_string(&context).unwrap_or_default());
    let (record, wait) = {
        let (app, org, uid, q) = (app.clone(), scope.org.clone(), scope.project.clone(), query.clone());
        blocking(move || crate::exec::submit_fresh(&app, (&org, &uid, &q.branch), q.spec, q.expr, context, connection, wait)).await?
    };
    let record = crate::exec::settled(&app, record, wait).await?;
    if record.status.is_final() {
        let d = finish(&app, &prep, &scope, &id, &query, record, &fingerprint, &then).await?;
        let status = if matches!(then, Then::Create { .. }) { StatusCode::CREATED } else { StatusCode::OK };
        return Ok((status, Json(shown(&d))).into_response());
    }
    prep.waiting(&scope, &id, &record.id);
    let result = record.id.clone();
    let after = (app.clone(), prep.clone(), scope.clone(), id.clone());
    tokio::spawn(async move {
        let (app, prep, scope, id) = after;
        let outcome = match crate::exec::settled(&app, record, AFTER).await {
            Ok(r) if r.status.is_final() => finish(&app, &prep, &scope, &id, &query, r, &fingerprint, &then).await.map(|_| ()),
            Ok(r) => {
                Err(ApiError(StatusCode::GATEWAY_TIMEOUT, "Timeout", format!("run {} was still running after {}s", r.id, AFTER.as_secs())))
            }
            Err(e) => Err(e),
        };
        match outcome {
            Ok(()) => prep.done(&scope, &id),
            Err(e) => {
                tracing::warn!("dataset {id} in {}/{}: its query did not make a snapshot: {}", scope.org, scope.project, e.2);
                prep.failed(&scope, &id, &e.2);
            }
        }
    });
    Ok((StatusCode::ACCEPTED, Json(json!({ "id": id, "pending": { "result": result } }))).into_response())
}

/// A finished run's rows, made a snapshot: of a new dataset, or the next
/// of one there.
#[allow(clippy::too_many_arguments)]
async fn finish(
    app: &Shared,
    prep: &Arc<Prep>,
    scope: &Scope,
    id: &str,
    query: &Query,
    record: Record,
    fingerprint: &str,
    then: &Then,
) -> Result<zsql_prep::Dataset, ApiError> {
    if record.status != Status::Succeeded {
        let why = record.error.clone().unwrap_or_else(|| format!("{:?}", record.status).to_lowercase());
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable", format!("the query did not succeed: {why}")));
    }
    let (app, prep, scope, id, query, fingerprint, then) =
        (app.clone(), prep.clone(), scope.clone(), id.to_string(), query.clone(), fingerprint.to_string(), then.clone());
    blocking(move || {
        let model = app.model(&scope.org, &scope.project, &query.branch)?;
        let path = app.runs.rows(&record).map_err(crate::exec::internal)?;
        let rows = zsql_exec::page(&path, 0, u64::MAX).map_err(crate::exec::internal)?;
        // Each column's role, and its field, from the model where the
        // column is a field of it.
        let mut roles = std::collections::BTreeMap::new();
        let mut fields = std::collections::BTreeMap::new();
        for f in rows.schema.fields() {
            let name = f.name();
            if let Some(field) =
                model.semantic().fields().iter().find(|x| x.name.eq_ignore_ascii_case(name) || x.uid.eq_ignore_ascii_case(name))
            {
                let role = match field.kind {
                    zsql_semantic::FieldKind::Measure => Role::Measure,
                    zsql_semantic::FieldKind::Dimension => Role::Dimension,
                };
                roles.insert(name.clone(), role);
                fields.insert(name.clone(), field.uid.clone());
            }
        }
        let read = SemanticRead {
            branch: query.branch.clone(),
            deployed_at: model.loaded_at.clone(),
            spec: record.spec.clone(),
            expr: record.expr.clone(),
            sql_sha256: zsql_exec::sha256(&record.sql),
            context: fingerprint,
            fields,
        };
        let arrived = Arrived { rows, roles, read };
        Ok(match then {
            Then::Create { name } => prep.create_from_query(&scope, &id, name.as_deref(), query, arrived)?,
            Then::Refresh => prep.refresh(&scope, &id, arrived)?,
        })
    })
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeleteQuery {
    #[serde(default)]
    confirm: bool,
}

pub(crate) async fn delete(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<DeleteQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        prep.delete(&scope, &id, q.confirm)?;
        Ok(Json(json!({ "removed": id })))
    })
    .await
}

pub(crate) async fn replace(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<UploadQuery>,
    headers: HeaderMap,
    body: Bytes,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    if q.name.is_some() {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Invalid", "a replacement keeps the dataset's name; name is for a new one".into()));
    }
    blocking(move || {
        let upload = Upload { file_name: q.file_name.clone(), bytes: &body, options: q.options() };
        Ok(Json(shown(&prep.replace(&scope, &id, upload)?)))
    })
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageQuery {
    offset: Option<u64>,
    limit: Option<u64>,
    version: Option<u32>,
    /// `column`, or `column:desc`, comma-separated.
    sort: Option<String>,
    /// A formula, true for the rows shown.
    filter: Option<String>,
}

fn sort_keys(sort: Option<&str>) -> Result<Vec<SortKey>, ApiError> {
    let Some(sort) = sort else { return Ok(Vec::new()) };
    sort.split(',')
        .map(str::trim)
        .filter(|k| !k.is_empty())
        .map(|k| {
            let (column, dir) = k.rsplit_once(':').unwrap_or((k, "asc"));
            match dir {
                "asc" => Ok(SortKey { column: column.to_string(), desc: false }),
                "desc" => Ok(SortKey { column: column.to_string(), desc: true }),
                _ => Err(ApiError(StatusCode::BAD_REQUEST, "Invalid", format!("'{k}' sorts by a column, then :asc or :desc"))),
            }
        })
        .collect()
}

/// The most rows a page holds.
const PAGE_MAX: u64 = 10_000;

pub(crate) async fn rows(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<PageQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    let limit = q.limit.unwrap_or(100);
    if limit == 0 || limit > PAGE_MAX {
        return Err(ApiError(StatusCode::BAD_REQUEST, "Invalid", format!("limit is 1 to {PAGE_MAX}")));
    }
    let sort = sort_keys(q.sort.as_deref())?;
    blocking(move || {
        let view = View { sort: &sort, filter: q.filter.as_deref() };
        let page = prep.page(&scope, &id, q.version, q.offset.unwrap_or(0), limit, &view)?;
        let rows = zsql_exec::json::rows(&page.rows).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", e.to_string()))?;
        Ok(Json(json!({
            "version": page.version,
            "columns": page.columns,
            "total": page.total,
            "offset": page.offset,
            "rows": rows,
        })))
    })
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct VersionQuery {
    version: Option<u32>,
}

pub(crate) async fn lineage(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<VersionQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || Ok(Json(serde_json::to_value(prep.lineage(&scope, &id, q.version)?).unwrap_or(Value::Null)))).await
}

pub(crate) async fn recipe(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<VersionQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || Ok(Json(serde_json::to_value(prep.recipe(&scope, &id, q.version)?).unwrap_or(Value::Null)))).await
}

pub(crate) async fn change(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
    Input(change): Input<Change>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || Ok(Json(shown(&prep.change(&scope, &id, change)?)))).await
}

pub(crate) async fn preview(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
    Input(preview): Input<Preview>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        let p = prep.preview(&scope, &id, preview)?;
        let rows = zsql_exec::json::rows(&p.rows).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", e.to_string()))?;
        let unresolved: Vec<String> = p.unresolved.iter().map(zsql_prep::Unresolved::describe).collect();
        Ok(Json(json!({ "columns": p.columns, "rows": rows, "unresolved": unresolved, "sample": p.sample })))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ImpactQuery {
    column: String,
}

pub(crate) async fn impact(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<ImpactQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        let readers = prep.impact(&scope, &id, &q.column)?;
        let said: Vec<String> = readers.iter().map(zsql_prep::Unresolved::describe).collect();
        Ok(Json(json!({ "column": q.column, "readers": readers, "said": said })))
    })
    .await
}

pub(crate) async fn edit(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
    Input(request): Input<zsql_prep::edits::Request>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        let (d, outcomes) = prep.edit(&scope, &id, request)?;
        Ok(Json(json!({ "dataset": shown(&d), "outcomes": outcomes })))
    })
    .await
}

pub(crate) async fn edit_log(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        let (log, outcomes) = prep.edit_log(&scope, &id)?;
        Ok(Json(json!({ "entries": log, "outcomes": outcomes })))
    })
    .await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Against {
    against: u32,
}

pub(crate) async fn confirm(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
    Input(a): Input<Against>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || Ok(Json(shown(&prep.confirm(&scope, &id, a.against)?)))).await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct SnapshotQuery {
    snapshot: Option<u32>,
}

pub(crate) async fn source(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id, source)): Params<(String, String, String)>,
    Search(q): Search<SnapshotQuery>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || Ok(Json(serde_json::to_value(prep.snapshot(&scope, &id, &source, q.snapshot)?).unwrap_or(Value::Null)))).await
}

pub(crate) async fn downstream(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        let below: Vec<Value> =
            prep.downstream(&scope, &id)?.iter().map(|d| json!({ "id": d.id, "name": d.name, "live": d.live })).collect();
        Ok(Json(json!({ "downstream": below })))
    })
    .await
}

/// A query of the model with datasets beside it.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct BlendRequest {
    spec: Value,
    datasets: Vec<zsql_prep::blend::Attach>,
    blend: Vec<zsql_prep::blend::Join>,
    #[serde(default)]
    context: Option<SecurityContext>,
    #[serde(default)]
    connection: Map<String, Value>,
    #[serde(default)]
    wait: Option<f64>,
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// The model's part of a blend: the spec the planner is given, with
/// every projection named so its column can be found, and the answer's
/// columns in order.
struct Split {
    spec: Value,
    outputs: Vec<zsql_prep::blend::Output>,
    keys: std::collections::BTreeMap<String, String>,
    key_only: Vec<String>,
}

/// The field a spec names, and whether it is a measure and one that
/// adds up: a sum or a count on every table it is on.
fn field_kind(model: &zsql_core::Model, written: &str) -> (bool, bool) {
    let (name, suffix) = match written.rsplit_once('@') {
        Some((n, s)) => (n, Some(s)),
        None => (written, None),
    };
    let sem = model.semantic();
    let field = sem.fields().iter().find(|f| f.uid.eq_ignore_ascii_case(name) || f.name.eq_ignore_ascii_case(name));
    let measure = match (suffix, field) {
        (Some("m"), _) => true,
        (Some("d"), _) => false,
        (_, Some(f)) => f.kind == zsql_semantic::FieldKind::Measure,
        _ => false,
    };
    let additive = field.is_some_and(|f| {
        let sqls: Vec<String> = sem
            .tables()
            .iter()
            .filter_map(|t| sem.expression_for(t.id, f.id))
            .filter_map(|e| e.sql.as_ref())
            .map(|s| s.trim().to_lowercase())
            .collect();
        !sqls.is_empty() && sqls.iter().all(|s| (s.starts_with("sum(") || s.starts_with("count(")) && !s.contains("distinct"))
    });
    (measure, additive)
}

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "Invalid", m.into())
}

/// Splits a blend's spec: projections of an attached alias are the
/// datasets'; the rest, and the model keys the joins name, are the
/// model's, each given an alias so its column is known.
fn split(model: &zsql_core::Model, req: &BlendRequest) -> Result<Split, ApiError> {
    let aliases: Vec<&str> = req.datasets.iter().map(|a| a.alias.as_str()).collect();
    let sem = model.semantic();
    for a in &aliases {
        zsql_prep::blend::check_alias(a)?;
        let clash = sem.tables().iter().any(|t| t.uid.eq_ignore_ascii_case(a) || t.name.eq_ignore_ascii_case(a))
            || sem.fields().iter().any(|f| {
                f.name.eq_ignore_ascii_case(a)
                    || f.uid.eq_ignore_ascii_case(a)
                    || f.uid.to_lowercase().starts_with(&format!("{}.", a.to_lowercase()))
            });
        if clash {
            return Err(invalid(format!(
                "'{a}' is a name in the model, so it cannot be a dataset's alias: a bare name never means a dataset"
            )));
        }
    }
    let Some(obj) = req.spec.as_object() else { return Err(invalid("a blend's spec is an object")) };
    if obj.get("calculations").and_then(Value::as_array).is_some_and(|c| !c.is_empty()) {
        return Err(invalid("a blend's model part takes projections, not calculations, for now"));
    }
    let projections = obj.get("projections").and_then(Value::as_array).cloned().unwrap_or_default();
    let mut spec = req.spec.clone();
    let mut model_projections: Vec<Value> = Vec::new();
    let mut outputs = Vec::new();
    let mut written_of: Vec<(String, String)> = Vec::new();
    for p in projections {
        let written = p.get("field").and_then(Value::as_str).map(str::to_string);
        let shown = p.get("alias").and_then(Value::as_str).map(str::to_string);
        let alias_of: Option<String> =
            written.as_deref().and_then(|w| w.split_once('.')).map(|(a, _)| a).filter(|a| aliases.contains(a)).map(str::to_string);
        match (written, alias_of) {
            (Some(w), Some(a)) => {
                let column = w[a.len() + 1..].to_string();
                outputs.push(zsql_prep::blend::Output::Dataset { alias: a, column, name: shown.unwrap_or(w) });
            }
            (written, _) => {
                let column = format!("__m{}", model_projections.len());
                let w = written.clone().unwrap_or_default();
                let (measure, additive) = field_kind(model, &w);
                let mut p = p.clone();
                p["alias"] = json!(column);
                model_projections.push(p);
                if !w.is_empty() {
                    written_of.push((w.to_lowercase(), column.clone()));
                }
                outputs.push(zsql_prep::blend::Output::Model { column, name: shown.unwrap_or(w), measure, additive });
            }
        }
    }
    let mut keys = std::collections::BTreeMap::new();
    let mut key_only = Vec::new();
    for j in req.blend.iter().filter(|j| j.to == "model") {
        for on in &j.on {
            if keys.contains_key(&on.left) {
                continue;
            }
            let column = match written_of.iter().find(|(w, _)| *w == on.left.to_lowercase()) {
                Some((_, c)) => c.clone(),
                None => {
                    let c = format!("__k{}", key_only.len());
                    model_projections.push(json!({ "field": on.left, "alias": c }));
                    key_only.push(c.clone());
                    c
                }
            };
            keys.insert(on.left.clone(), column);
        }
    }
    spec["projections"] = Value::Array(model_projections);
    Ok(Split { spec, outputs, keys, key_only })
}

pub(crate) async fn blend(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
    headers: HeaderMap,
    Input(req): Input<BlendRequest>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    let model = app.model(&scope.org, &scope.project, &branch)?;
    let page = crate::exec::page_of(req.offset, req.limit)?;
    let split = split(&model, &req)?;
    let (record, _, wait) = {
        let (app, scope, branch, spec) = (app.clone(), scope.clone(), branch.clone(), split.spec.clone());
        let (context, connection, wait, fresh) = (req.context.clone(), req.connection.clone(), req.wait, req.fresh);
        blocking(move || {
            crate::exec::submit_spec(&app, (&scope.org, &scope.project, &branch), (Some(spec), None), context, connection, wait, fresh)
        })
        .await?
    };
    let record = crate::exec::settled(&app, record, wait).await?;
    if !record.status.is_final() {
        return Ok((
            StatusCode::ACCEPTED,
            Json(json!({
                "status": "running",
                "model": { "result": record.id },
                "message": "the model's part is still running in the warehouse; send the same request again, and it is joined to the run",
            })),
        )
            .into_response());
    }
    if record.status != Status::Succeeded {
        let why = record.error.clone().unwrap_or_else(|| format!("{:?}", record.status).to_lowercase());
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable", format!("the model's part did not succeed: {why}")));
    }
    let plan = zsql_prep::blend::Blend { datasets: req.datasets, joins: req.blend, outputs: split.outputs };
    blocking(move || {
        let path = app.runs.rows(&record).map_err(crate::exec::internal)?;
        let rows = zsql_exec::page(&path, 0, u64::MAX).map_err(crate::exec::internal)?;
        let part = zsql_prep::blend::ModelPart { rows, keys: split.keys, key_only: split.key_only };
        let b = prep.blend(&scope, &plan, &part, &record.sql)?;
        let (offset, limit) = page;
        let total = b.rows.num_rows() as u64;
        let mut skip = offset as usize;
        let mut left = limit as usize;
        let mut batches = Vec::new();
        for batch in &b.rows.batches {
            if left == 0 {
                break;
            }
            if skip >= batch.num_rows() {
                skip -= batch.num_rows();
                continue;
            }
            let take = (batch.num_rows() - skip).min(left);
            batches.push(batch.slice(skip, take));
            left -= take;
            skip = 0;
        }
        let shown = zsql_datasource::Batches { schema: b.rows.schema.clone(), batches };
        let rows = zsql_exec::json::rows(&shown).map_err(crate::exec::internal)?;
        Ok(Json(json!({
            "columns": zsql_exec::json::columns(&b.rows.schema),
            "rows": rows,
            "total": total,
            "offset": offset,
            "complete": offset + (rows.len() as u64) >= total,
            "cache": if b.hit { "hit" } else { "miss" },
            "model": { "result": record.id, "sql": record.sql },
            "datasets": b.datasets,
        }))
        .into_response())
    })
    .await
}

/// Brings back from the object store every dataset of the org this
/// node does not have: the restore of the backup. No owner: it is the
/// org's, as a deploy is.
pub(crate) async fn restore(State(app): State<Shared>, Org(org): Org) -> Result<Json<Value>, ApiError> {
    let prep = prep(&app)?;
    blocking(move || Ok(Json(json!({ "restored": prep.restore(&org)? })))).await
}

/// Every dataset of one owner in the org, in every project: what an
/// application does when it removes a user.
pub(crate) async fn delete_owner(State(app): State<Shared>, Org(org): Org, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let prep = prep(&app)?;
    let owner = owner(&headers)?;
    blocking(move || Ok(Json(json!({ "removed": prep.delete_owner(&org, &owner)? })))).await
}
