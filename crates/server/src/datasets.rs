//! Sources, datasets and runs: the routes of data prep, and nothing else
//! of it. What a source or a dataset is, where it is kept and how a graph
//! runs is `zsql-prep`'s; this module checks the names, pulls what a run
//! reads from the model under the context it was sent with, and answers.
//!
//! ```text
//! GET    /projects/{uid}/sources                          the explorer: every source
//! POST   /projects/{uid}/sources                          ?kind=upload (a CSV) | push ({"rows", "origin"}) | query ({"branch", "spec" | "expr"})
//!                                                         &name=&description=&dataset=true&file_name=&delimiter=&quote=&header=&max_bad_rows=
//! GET    /projects/{uid}/sources/{id}
//! PATCH  /projects/{uid}/sources/{id}                     {"name", "description", "types": {"column": "text"}}
//! DELETE /projects/{uid}/sources/{id}
//! GET    /projects/{uid}/sources/{id}/snapshots           the snapshots kept, the newest first
//! POST   /projects/{uid}/sources/{id}/snapshots           a CSV (upload), or {"rows", "origin"} (push); runs nothing
//! GET    /projects/{uid}/sources/{id}/snapshots/{n}
//! GET    /projects/{uid}/sources/{id}/rows                ?snapshot=&offset=&limit=&sort=&filter=
//! GET    /projects/{uid}/datasets                         what is browsed
//! POST   /projects/{uid}/datasets                         {"name", "description", "source"}: with a source, its newest snapshot as it is
//! GET    /projects/{uid}/datasets/{id}
//! DELETE /projects/{uid}/datasets/{id}
//! GET    /projects/{uid}/datasets/{id}/rows               ?offset=&limit=&version=&sort=col:desc,col&filter=<formula>
//! GET    /projects/{uid}/datasets/{id}/lineage            ?version=
//! GET    /projects/{uid}/datasets/{id}/versions/{k}       its manifest, lineage, run and graph
//! POST   /projects/{uid}/runs                             {"id", "dataset", "graph", "context", "connection", "wait"}
//! POST   /projects/{uid}/runs?check=true                  {"graph", "context"}: a dry run
//! GET    /projects/{uid}/runs/{id}
//! POST   /projects/{uid}/preview                          {"graph", "node", "whole", "limit", "context", "connection", "wait"}
//! POST   /datasets/restore                                every dataset and source of the org the object store has and this node does not
//! POST   /projects/{uid}/branches/{branch}/blend          {"spec", "datasets": [{"id", "as"}], "blend": [...], "context", "connection", "wait", "fresh", "offset", "limit"}
//! ```
//!
//! Sources and datasets are their project's, addressed by id. Who may
//! read or change which is the application's to decide, as who may
//! deploy or run is: the server keeps no user. Every answer that reads
//! inputs carries `reads`, each source and dataset by kind and id, for
//! the application to check against who is asking.

use crate::error::{ApiError, Input, Org, Params, Search};
use crate::names::check_uid;
use crate::{blocking, Shared};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::StatusCode;
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::sync::Arc;
use std::time::Duration;
use zsql_exec::{Record, Status};
use zsql_prep::{Arrived, CsvOptions, Graph, Keys, Named, Prep, Pull, Query, Role, Scope, SemanticRead, SortKey, Upload, View};
use zsql_spec::SecurityContext;

impl From<zsql_prep::Error> for ApiError {
    fn from(e: zsql_prep::Error) -> Self {
        use zsql_prep::Error::*;
        match e {
            Invalid(m) => ApiError(StatusCode::BAD_REQUEST, "Invalid", m),
            Unprocessable(m) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable", m),
            NotFound(m) => ApiError(StatusCode::NOT_FOUND, "NotFound", m),
            Conflict(m) => ApiError(StatusCode::CONFLICT, "Conflict", m),
            Internal(m) => ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", m),
            Stopping(m) => ApiError(StatusCode::SERVICE_UNAVAILABLE, "Busy", m),
            Problems(_) => ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Refused", e.to_string()),
        }
    }
}

/// A refusal as answered: a graph's problems listed, each of its node,
/// beside the error every refusal has.
fn refusal(e: zsql_prep::Error) -> Response {
    match e {
        zsql_prep::Error::Problems(problems) => {
            let message = problems.iter().map(zsql_prep::Problem::said).collect::<Vec<_>>().join("; ");
            (StatusCode::UNPROCESSABLE_ENTITY, Json(json!({ "error": { "class": "Refused", "message": message }, "problems": problems })))
                .into_response()
        }
        other => ApiError::from(other).into_response(),
    }
}

/// Data prep, unless the server was started with it off.
fn prep(app: &Shared) -> Result<Arc<Prep>, ApiError> {
    app.prep
        .clone()
        .ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "NotFound", "datasets are turned off on this server (ZSQL_DATASETS=off)".into()))
}

/// The project a request's sources and datasets are in, checked.
fn scope(app: &Shared, org: String, uid: String) -> Result<(Arc<Prep>, Scope), ApiError> {
    let prep = prep(app)?;
    check_uid(&uid)?;
    Ok((prep, Scope { org, project: uid }))
}

fn shown<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

fn invalid(m: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "Invalid", m.into())
}

/// A number in the path: a snapshot's, a version's.
fn number(text: &str, what: &str) -> Result<u32, ApiError> {
    text.parse().map_err(|_| ApiError(StatusCode::NOT_FOUND, "NotFound", format!("{what} is a number, and '{text}' is not one")))
}

/// A source or dataset is a project's, so the project has to be one:
/// deployed on some branch of this org.
fn deployed(app: &Shared, scope: &Scope) -> Result<(), ApiError> {
    if !app.of_org(&scope.org).iter().any(|m| m.uid == scope.project) {
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("no project {} is deployed; deploy a branch of it before adding sources", scope.project),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------- sources

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewSource {
    kind: Option<String>,
    name: Option<String>,
    description: Option<String>,
    /// Also a dataset over it, as it is.
    #[serde(default)]
    dataset: bool,
    file_name: Option<String>,
    delimiter: Option<char>,
    quote: Option<char>,
    header: Option<bool>,
    max_bad_rows: Option<usize>,
}

impl NewSource {
    fn options(&self) -> CsvOptions {
        CsvOptions { delimiter: self.delimiter, quote: self.quote, header: self.header, max_bad_rows: self.max_bad_rows }
    }

    fn named(&self) -> Named {
        Named { name: self.name.clone(), description: self.description.clone() }
    }
}

/// A query source's spec.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryBody {
    branch: String,
    #[serde(default)]
    spec: Option<Value>,
    #[serde(default)]
    expr: Option<String>,
}

fn json_body<T: serde::de::DeserializeOwned>(body: &[u8]) -> Result<T, ApiError> {
    serde_json::from_slice(body).map_err(|e| invalid(format!("the body is not what this takes: {e}")))
}

pub(crate) async fn sources(State(app): State<Shared>, Org(org): Org, Params((uid,)): Params<(String,)>) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(Json(json!({ "sources": prep.sources(&scope) })))).await
}

pub(crate) async fn create_source(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
    Search(q): Search<NewSource>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    deployed(&app, &scope)?;
    let kind = q.kind.clone().unwrap_or_else(|| "upload".into());
    if q.dataset && kind == "query" {
        return Err(invalid("a query source has no snapshot until a run pulls it, so it is no dataset as it is yet"));
    }
    if kind == "query" {
        let b: QueryBody = json_body(&body)?;
        crate::names::check_branch(&b.branch)?;
    }
    blocking(move || {
        let (source, snapshot) = match kind.as_str() {
            "upload" => {
                let upload = Upload { file_name: q.file_name.clone(), bytes: &body, options: q.options() };
                let (s, snap) = prep.create_upload(&scope, q.named(), upload)?;
                (s, Some(snap))
            }
            "push" => {
                let (s, snap) = prep.create_push(&scope, q.named(), json_body(&body)?)?;
                (s, Some(snap))
            }
            "query" => {
                let b: QueryBody = json_body(&body)?;
                (prep.create_query(&scope, q.named(), Query { branch: b.branch, spec: b.spec, expr: b.expr })?, None)
            }
            other => return Err(invalid(format!("a source is kind upload, push or query; not {other}"))),
        };
        let dataset = if q.dataset { Some(prep.create_dataset(&scope, q.named(), Some(&source.id))?) } else { None };
        let mut v = json!({ "source": source });
        if let Some(s) = snapshot {
            v["snapshot"] = shown(&s);
        }
        if let Some(d) = dataset {
            v["dataset"] = shown(&d);
        }
        Ok((StatusCode::CREATED, Json(v)).into_response())
    })
    .await
}

pub(crate) async fn get_source(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(Json(shown(&prep.source(&scope, &id)?)))).await
}

pub(crate) async fn change_source(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Input(change): Input<zsql_prep::SourceChange>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(Json(shown(&prep.change_source(&scope, &id, change)?)))).await
}

pub(crate) async fn delete_source(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || {
        let copied = prep.delete_source(&scope, &id)?;
        Ok(Json(json!({ "removed": id, "datasets_given_their_rows": copied })))
    })
    .await
}

pub(crate) async fn snapshots(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(Json(json!({ "snapshots": prep.snapshots(&scope, &id)? })))).await
}

pub(crate) async fn snapshot(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id, n)): Params<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    let n = number(&n, "a snapshot")?;
    blocking(move || Ok(Json(shown(&prep.snapshot(&scope, &id, Some(n))?)))).await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewSnapshot {
    file_name: Option<String>,
    delimiter: Option<char>,
    quote: Option<char>,
    header: Option<bool>,
    max_bad_rows: Option<usize>,
}

pub(crate) async fn add_snapshot(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<NewSnapshot>,
    body: Bytes,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || {
        let source = prep.source(&scope, &id)?;
        let (s, snap) = match source.kind {
            zsql_prep::Kind::Push => prep.add_push(&scope, &id, json_body(&body)?)?,
            _ => {
                let options = CsvOptions { delimiter: q.delimiter, quote: q.quote, header: q.header, max_bad_rows: q.max_bad_rows };
                prep.add_upload(&scope, &id, Upload { file_name: q.file_name.clone(), bytes: &body, options })?
            }
        };
        Ok((StatusCode::CREATED, Json(json!({ "source": s, "snapshot": snap }))).into_response())
    })
    .await
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageQuery {
    offset: Option<u64>,
    limit: Option<u64>,
    version: Option<u32>,
    snapshot: Option<u32>,
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
                _ => Err(invalid(format!("'{k}' sorts by a column, then :asc or :desc"))),
            }
        })
        .collect()
}

/// The most rows a page holds.
const PAGE_MAX: u64 = 10_000;

fn page_json(page: &zsql_prep::Page, which: &str) -> Result<Value, ApiError> {
    let rows = zsql_exec::json::rows(&page.rows).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", e.to_string()))?;
    Ok(json!({ which: page.version, "columns": page.columns, "total": page.total, "offset": page.offset, "rows": rows }))
}

fn limit_of(q: &PageQuery) -> Result<u64, ApiError> {
    let limit = q.limit.unwrap_or(100);
    if limit == 0 || limit > PAGE_MAX {
        return Err(invalid(format!("limit is 1 to {PAGE_MAX}")));
    }
    Ok(limit)
}

pub(crate) async fn source_rows(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    let limit = limit_of(&q)?;
    let sort = sort_keys(q.sort.as_deref())?;
    blocking(move || {
        let view = View { sort: &sort, filter: q.filter.as_deref() };
        let page = prep.source_rows(&scope, &id, q.snapshot, q.offset.unwrap_or(0), limit, &view)?;
        Ok(Json(page_json(&page, "snapshot")?))
    })
    .await
}

// --------------------------------------------------------------- datasets

pub(crate) async fn list(State(app): State<Shared>, Org(org): Org, Params((uid,)): Params<(String,)>) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(Json(json!({ "datasets": prep.list(&scope) })))).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct NewDataset {
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    description: Option<String>,
    /// A source whose newest snapshot is the dataset's first output, as
    /// it is.
    #[serde(default)]
    source: Option<String>,
}

pub(crate) async fn create(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
    Input(req): Input<NewDataset>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    deployed(&app, &scope)?;
    blocking(move || {
        let d = prep.create_dataset(&scope, Named { name: req.name, description: req.description }, req.source.as_deref())?;
        Ok((StatusCode::CREATED, Json(shown(&d))).into_response())
    })
    .await
}

pub(crate) async fn get(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(Json(shown(&prep.get(&scope, &id)?)))).await
}

pub(crate) async fn delete(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || {
        prep.delete(&scope, &id)?;
        Ok(Json(json!({ "removed": id })))
    })
    .await
}

pub(crate) async fn rows(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    Search(q): Search<PageQuery>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    let limit = limit_of(&q)?;
    let sort = sort_keys(q.sort.as_deref())?;
    blocking(move || {
        let view = View { sort: &sort, filter: q.filter.as_deref() };
        let page = prep.page(&scope, &id, q.version, q.offset.unwrap_or(0), limit, &view)?;
        Ok(Json(page_json(&page, "version")?))
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
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(Json(shown(&prep.lineage(&scope, &id, q.version)?)))).await
}

pub(crate) async fn version(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id, k)): Params<(String, String, String)>,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    let k = number(&k, "a version")?;
    blocking(move || Ok(Json(shown(&prep.version(&scope, &id, Some(k))?)))).await
}

// ------------------------------------------------------------------- runs

/// How long a run's request waits before it is answered 202, unless it
/// says: and the most it may say.
const WAIT: Duration = Duration::from_secs(30);
/// Under the prep class's 90s deadline, so the answer is always this
/// route's own 202 and never the guard's 503.
const WAIT_MAX: Duration = Duration::from_secs(80);

/// How long a pull is waited on, after its request was answered.
const PULL_LIMIT: Duration = Duration::from_secs(3600);

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunBody {
    #[serde(default)]
    id: Option<String>,
    #[serde(default)]
    dataset: Option<String>,
    graph: Graph,
    #[serde(default)]
    context: Option<SecurityContext>,
    #[serde(default)]
    connection: Map<String, Value>,
    #[serde(default)]
    wait: Option<f64>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunQuery {
    #[serde(default)]
    check: bool,
}

/// What each query source of a graph plans to under the context sent:
/// the key its snapshots are kept under and read only by, as the query
/// cache answers only the same SQL, or why it does not plan. Planned
/// before the graph is checked, so a spec that does not plan is
/// refused before anything is pulled.
async fn keys(app: &Shared, prep: &Arc<Prep>, scope: &Scope, graph: &Graph, context: &Option<SecurityContext>) -> Result<Keys, ApiError> {
    let (app, prep, scope, graph, context) = (app.clone(), prep.clone(), scope.clone(), graph.clone(), context.clone());
    blocking(move || {
        let mut keys = Keys::new(zsql_exec::sha256(&serde_json::to_string(&context).unwrap_or_default()));
        for (source, q) in prep.query_sources(&scope, &graph) {
            let planned = crate::names::check_branch(&q.branch)
                .and_then(|()| crate::exec::plan_spec(&app, (&scope.org, &scope.project, &q.branch), (q.spec, q.expr), context.clone()));
            keys = keys.with(source, planned.map(|(ds, sql)| zsql_prep::query_key(&ds, &sql)).map_err(|e| e.2));
        }
        Ok(keys)
    })
    .await
}

fn wait_of(wait: Option<f64>) -> Result<Duration, ApiError> {
    match wait {
        None => Ok(WAIT),
        Some(w) if w.is_finite() && w >= 0.0 => Ok(Duration::from_secs_f64(w).min(WAIT_MAX)),
        Some(_) => Err(invalid("wait is a number of seconds, 0 or more")),
    }
}

/// A run's record as answered: 200 when it is over, 202 while it runs.
fn answered(r: &zsql_prep::RunRecord) -> Response {
    let status = if r.state.is_final() { StatusCode::OK } else { StatusCode::ACCEPTED };
    (status, Json(shown(r))).into_response()
}

pub(crate) async fn run(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
    Search(q): Search<RunQuery>,
    Input(body): Input<RunBody>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    let keys = keys(&app, &prep, &scope, &body.graph, &body.context).await?;
    if q.check {
        let graph = body.graph;
        return blocking(move || Ok(Json(shown(&prep.check(&scope, &graph, &keys)?)).into_response())).await;
    }
    let (Some(id), Some(dataset)) = (body.id.clone(), body.dataset.clone()) else {
        return Err(invalid("a run says its id, which a retry sends again, and the dataset it makes"));
    };
    let wait = wait_of(body.wait)?;
    let begun = {
        let (prep, scope) = (prep.clone(), scope.clone());
        let request = zsql_prep::RunRequest { id: id.clone(), dataset, graph: body.graph };
        blocking(move || Ok(prep.begin(&scope, request, &keys))).await?
    };
    let ticket = match begun {
        Ok(zsql_prep::Begun::Answered(r)) => return Ok(answered(&r)),
        Ok(zsql_prep::Begun::Started(t)) => t,
        Err(e) => return Ok(refusal(e)),
    };
    let (context, connection) = (body.context, body.connection);
    let work = {
        let (app, prep) = (app.clone(), prep.clone());
        tokio::spawn(async move { carry_out(app, prep, *ticket, context, connection).await })
    };
    // Answered when it is over, or as it stands when the wait is up; it
    // goes on either way, and GET /runs/{id} says how it ended.
    let _ = tokio::time::timeout(wait, work).await;
    let r = blocking(move || Ok(prep.run(&scope, &id)?)).await?;
    Ok(answered(&r))
}

/// A started run, carried to its end: each pull run fresh under the
/// run's context and kept as a snapshot, then the graph.
async fn carry_out(
    app: Shared,
    prep: Arc<Prep>,
    mut ticket: zsql_prep::Ticket,
    context: Option<SecurityContext>,
    connection: Map<String, Value>,
) {
    let pulls = ticket.pulls.clone();
    for p in pulls {
        match pull(&app, &ticket.scope, &p, context.clone(), connection.clone(), PULL_LIMIT, true).await {
            Ok(arrived) => {
                let source = p.source.clone();
                let kept = {
                    let prep = prep.clone();
                    tokio::task::spawn_blocking(move || {
                        let r = prep.pulled_for(&mut ticket, &source, arrived);
                        (ticket, r)
                    })
                    .await
                };
                match kept {
                    Ok((t, Ok(()))) => ticket = t,
                    Ok((t, Err(e))) => {
                        let why = format!("{} was pulled and could not be kept: {e}", p.name);
                        let _ = tokio::task::spawn_blocking(move || prep.fail(t, &why)).await;
                        return;
                    }
                    Err(e) => {
                        tracing::warn!("a run's pull could not be kept: {e}");
                        return;
                    }
                }
            }
            Err(e) => {
                let why = format!("pulling {}: {}", p.name, e.2);
                let _ = tokio::task::spawn_blocking(move || prep.fail(ticket, &why)).await;
                return;
            }
        }
    }
    let seat = crate::guard::seat();
    let _ = tokio::task::spawn_blocking(move || {
        let held = seat;
        prep.execute(ticket);
        drop(held);
    })
    .await;
}

/// A query source's spec, run under the context given, waited on, and
/// its rows as a snapshot takes them. `fresh` for a run's pull; a
/// preview's joins a run already going or answered, so sending it again
/// is answered when the warehouse is done.
async fn pull(
    app: &Shared,
    scope: &Scope,
    p: &Pull,
    context: Option<SecurityContext>,
    connection: Map<String, Value>,
    wait: Duration,
    fresh: bool,
) -> Result<Arrived, ApiError> {
    crate::names::check_branch(&p.query.branch)?;
    let (record, _, _) = {
        let (app, org, uid, q) = (app.clone(), scope.org.clone(), scope.project.clone(), p.query.clone());
        blocking(move || crate::exec::submit_spec(&app, (&org, &uid, &q.branch), (q.spec, q.expr), context, connection, Some(0.0), fresh))
            .await?
    };
    let record = crate::exec::settled(app, record, wait).await?;
    if !record.status.is_final() {
        return Err(ApiError(
            StatusCode::ACCEPTED,
            "Pulling",
            format!("{} is still running in the warehouse (result {}); send the same request again", p.name, record.id),
        ));
    }
    arrived(app, scope, &p.query, record).await
}

/// A finished run's rows, with the model's role and field for each
/// column that is one.
async fn arrived(app: &Shared, scope: &Scope, query: &Query, record: Record) -> Result<Arrived, ApiError> {
    if record.status != Status::Succeeded {
        let why = record.error.clone().unwrap_or_else(|| format!("{:?}", record.status).to_lowercase());
        return Err(ApiError(StatusCode::UNPROCESSABLE_ENTITY, "Unprocessable", format!("the query did not succeed: {why}")));
    }
    let (app, scope, query) = (app.clone(), scope.clone(), query.clone());
    blocking(move || {
        let model = app.model(&scope.org, &scope.project, &query.branch)?;
        let path = app.runs.rows(&record).map_err(crate::exec::internal)?;
        let rows = zsql_exec::page(&path, 0, u64::MAX).map_err(crate::exec::internal)?;
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
            // Kept under the key of the statement run, which a later
            // run's spec must plan to again to read it.
            context: zsql_prep::query_key(&record.datasource_uid, &record.sql),
            fields,
        };
        Ok(Arrived { rows, roles, read })
    })
    .await
}

pub(crate) async fn get_run(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    blocking(move || Ok(answered(&prep.run(&scope, &id)?))).await
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreviewBody {
    graph: Graph,
    #[serde(default)]
    node: Option<String>,
    #[serde(default)]
    whole: bool,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    context: Option<SecurityContext>,
    #[serde(default)]
    connection: Map<String, Value>,
    #[serde(default)]
    wait: Option<f64>,
}

/// How long a preview waits on its pulls before it says to send it again.
const PREVIEW_WAIT: Duration = Duration::from_secs(10);

pub(crate) async fn preview(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid,)): Params<(String,)>,
    Input(body): Input<PreviewBody>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
    let keys = keys(&app, &prep, &scope, &body.graph, &body.context).await?;
    let wait = body.wait.map(|w| wait_of(Some(w))).transpose()?.unwrap_or(PREVIEW_WAIT);
    let request = zsql_prep::PreviewRequest { graph: body.graph, node: body.node, whole: body.whole, limit: body.limit };
    let pulls = {
        let (prep, scope, keys, request) = (prep.clone(), scope.clone(), keys.clone(), request.clone());
        match blocking(move || Ok(prep.preview_pulls(&scope, &request, &keys))).await? {
            Ok(p) => p,
            Err(e) => return Ok(refusal(e)),
        }
    };
    // A query source with no snapshot of the statement it plans to is
    // pulled first, as a run would; the preview reads the rows, and
    // keeps nothing.
    let mut pulled = Vec::new();
    for p in &pulls {
        let arrived = match pull(&app, &scope, p, body.context.clone(), body.connection.clone(), wait, false).await {
            Ok(a) => a,
            Err(e) if e.0 == StatusCode::ACCEPTED => {
                return Ok((StatusCode::ACCEPTED, Json(json!({ "status": "pulling", "source": p.source, "message": e.2 }))).into_response())
            }
            Err(e) => return Err(e),
        };
        pulled.push((p.source.clone(), arrived));
    }
    blocking(move || {
        let p = match prep.preview(&scope, &request, &keys, pulled) {
            Ok(p) => p,
            Err(e) => return Ok(refusal(e)),
        };
        let rows = zsql_exec::json::rows(&p.rows).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", e.to_string()))?;
        Ok(Json(json!({
            "node": p.node,
            "columns": p.columns,
            "rows": rows,
            "nodes": p.nodes,
            "problems": p.problems,
            "reads": p.reads,
            "inputs": p.inputs,
            "sample": p.sample,
        }))
        .into_response())
    })
    .await
}

// ----------------------------------------------------------------- blends

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
}

/// Whether the field a spec names is a measure.
fn is_measure(model: &zsql_core::Model, written: &str) -> bool {
    let (name, suffix) = match written.rsplit_once('@') {
        Some((n, s)) => (n, Some(s)),
        None => (written, None),
    };
    let sem = model.semantic();
    let field = sem.fields().iter().find(|f| f.uid.eq_ignore_ascii_case(name) || f.name.eq_ignore_ascii_case(name));
    match (suffix, field) {
        (Some("m"), _) => true,
        (Some("d"), _) => false,
        (_, Some(f)) => f.kind == zsql_semantic::FieldKind::Measure,
        _ => false,
    }
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
                let measure = is_measure(model, &w);
                let mut p = p.clone();
                p["alias"] = json!(column);
                model_projections.push(p);
                if !w.is_empty() {
                    written_of.push((w.to_lowercase(), column.clone()));
                }
                outputs.push(zsql_prep::blend::Output::Model { column, name: shown.unwrap_or(w), measure });
            }
        }
    }
    // A model key a join names must be asked for: the dataset is at
    // that grain, and a key planned only to join on would make the
    // warehouse send rows finer than the answer, to be added up again.
    let mut keys = std::collections::BTreeMap::new();
    for j in req.blend.iter().filter(|j| j.to == "model") {
        for on in &j.on {
            let Some((_, column)) = written_of.iter().find(|(w, _)| *w == on.left.to_lowercase()) else {
                return Err(invalid(format!(
                    "'{}' joins '{}' to the model, so ask for it too: add {{\"field\": \"{}\"}} to the projections. \
                     The dataset is at that grain, and the answer is joined on it",
                    on.left, j.join, on.left
                )));
            };
            keys.insert(on.left.clone(), column.clone());
        }
    }
    spec["projections"] = Value::Array(model_projections);
    Ok(Split { spec, outputs, keys })
}

pub(crate) async fn blend(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
    Input(req): Input<BlendRequest>,
) -> Result<Response, ApiError> {
    let (prep, scope) = scope(&app, org, uid)?;
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
        let part = zsql_prep::blend::ModelPart { rows, keys: split.keys };
        // Kept by the model's run, whose cache says when it is run again.
        let b = prep.blend(&scope, &plan, &part, &record.id)?;
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

/// Brings back from the object store every dataset and source of the
/// org this node does not have: the restore of the backup.
pub(crate) async fn restore(State(app): State<Shared>, Org(org): Org) -> Result<Json<Value>, ApiError> {
    let prep = prep(&app)?;
    blocking(move || Ok(Json(json!({ "restored": prep.restore(&org)? })))).await
}
