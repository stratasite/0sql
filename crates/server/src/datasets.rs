//! Datasets: the routes of data prep, and nothing else of it. What a
//! dataset is, where it is kept and how it is made is `zsql-prep`'s;
//! this module reads the owner from its header, checks the names, and
//! answers.
//!
//! ```text
//! GET    /projects/{uid}/datasets                         this owner's datasets
//! POST   /projects/{uid}/datasets                         body: a CSV  ?name=&file_name=&delimiter=&quote=&header=&max_bad_rows=
//! GET    /projects/{uid}/datasets/{id}
//! DELETE /projects/{uid}/datasets/{id}                    a hard delete
//! PUT    /projects/{uid}/datasets/{id}/upload             body: a CSV, a new snapshot, and the recipe run over it
//! GET    /projects/{uid}/datasets/{id}/rows               ?offset=&limit=&version=
//! GET    /projects/{uid}/datasets/{id}/lineage            ?version=
//! GET    /projects/{uid}/datasets/{id}/recipe             ?version=
//! POST   /projects/{uid}/datasets/{id}/recipe             {"against": m, "roles": {...}, "steps": [...]}
//! GET    /projects/{uid}/datasets/{id}/sources/{source}   ?snapshot=
//! DELETE /datasets                                        every dataset of this owner in the org
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
use serde_json::{json, Value};
use std::sync::Arc;
use zsql_prep::{Change, CsvOptions, Owner, Prep, Scope, Upload};

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

pub(crate) async fn get(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || Ok(Json(shown(&prep.get(&scope, &id)?)))).await
}

pub(crate) async fn delete(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, id)): Params<(String, String)>,
    headers: HeaderMap,
) -> Result<Json<Value>, ApiError> {
    let (prep, scope) = scope(&app, org, uid, &headers)?;
    blocking(move || {
        prep.delete(&scope, &id)?;
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
    blocking(move || {
        let page = prep.page(&scope, &id, q.version, q.offset.unwrap_or(0), limit)?;
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

/// Every dataset of one owner in the org, in every project: what an
/// application does when it removes a user.
pub(crate) async fn delete_owner(State(app): State<Shared>, Org(org): Org, headers: HeaderMap) -> Result<Json<Value>, ApiError> {
    let prep = prep(&app)?;
    let owner = owner(&headers)?;
    blocking(move || Ok(Json(json!({ "removed": prep.delete_owner(&org, &owner)? })))).await
}
