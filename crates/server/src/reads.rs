//! The reads: sql, explain, explore, the catalog, the values of a
//! dimension, the tests, a branch's summary and the listing. Each
//! handler is a thin wrapper over a plain function so the MCP tools run
//! the same code.

use crate::error::{ApiError, Input, Params, Search};
use crate::guard::Held;
use crate::{blocking, read, Shared};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use serde_json::{json, Value};
use zsql_service::{ExplainResponse, ExploreRequest, ExploreResponse, ServiceError, SqlRequest, SqlResponse};

/// What one request may ask for. A spec is JSON the caller wrote and
/// the cost of planning grows with it, so its shape is bounded before
/// any work starts: generous beside any real query, a wall in front of
/// a generated one. See `docs/admission.md`.
pub(crate) mod shape {
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

pub(crate) fn too_big(what: &str, n: usize, limit: usize) -> ApiError {
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
pub(crate) fn check_term(term: &str) -> Result<(), ApiError> {
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

// The read handlers are each a thin wrapper over a plain function, so
// the MCP tools in mcp.rs run exactly the same code. The work itself is
// the service's: the server adds the bounds on a request. The security
// context a plan runs under is whatever the request carries: the caller
// is the developer's own service, and it says who it is asking for.

pub(crate) async fn sql(
    State(app): State<Shared>,
    Params((uid, branch)): Params<(String, String)>,
    Input(req): Input<SqlRequest>,
) -> Result<Json<SqlResponse>, ApiError> {
    Ok(Json(sql_of(&app, &uid, &branch, req)?))
}

pub(crate) fn sql_of(app: &Shared, uid: &str, branch: &str, req: SqlRequest) -> Result<SqlResponse, ApiError> {
    let d = app.model(uid, branch)?;
    check_expr(req.expr.as_deref())?;
    Ok(d.sql(req, &shape_check)?)
}

pub(crate) async fn explain(
    State(app): State<Shared>,
    Params((uid, branch)): Params<(String, String)>,
    Input(req): Input<SqlRequest>,
) -> Result<Json<ExplainResponse>, ApiError> {
    Ok(Json(explain_of(&app, &uid, &branch, req)?))
}

pub(crate) fn explain_of(app: &Shared, uid: &str, branch: &str, req: SqlRequest) -> Result<ExplainResponse, ApiError> {
    let d = app.model(uid, branch)?;
    check_expr(req.expr.as_deref())?;
    Ok(d.explain(req, &shape_check)?)
}

pub(crate) async fn explore(
    State(app): State<Shared>,
    Params((uid, branch)): Params<(String, String)>,
    Input(req): Input<ExploreRequest>,
) -> Result<Json<ExploreResponse>, ApiError> {
    Ok(Json(explore_of(&app, &uid, &branch, req)?))
}

pub(crate) fn explore_of(app: &Shared, uid: &str, branch: &str, req: ExploreRequest) -> Result<ExploreResponse, ApiError> {
    let d = app.model(uid, branch)?;
    check_expr(req.expr.as_deref())?;
    if let Some(term) = &req.q {
        check_term(term)?;
    }
    Ok(d.explore(req, &shape_check)?)
}

#[derive(Deserialize)]
pub(crate) struct FieldsQuery {
    #[serde(default)]
    pub(crate) q: String,
    #[serde(default)]
    pub(crate) hidden: bool,
}

/// Field discovery. With `q`, fields containing the text come first in
/// model order, then near matches by trigram similarity, each with its
/// `score` (1 for a containing match).
pub(crate) async fn fields(
    State(app): State<Shared>,
    Params((uid, branch)): Params<(String, String)>,
    Search(q): Search<FieldsQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(fields_of(&app, &uid, &branch, q)?))
}

pub(crate) fn fields_of(app: &Shared, uid: &str, branch: &str, q: FieldsQuery) -> Result<Value, ApiError> {
    let d = app.model(uid, branch)?;
    check_term(&q.q)?;
    Ok(d.fields(&q.q, q.hidden))
}

/// What every list read takes: `?q=text` keeps the entries that hold
/// the text, in any case. Without it the list is whole.
#[derive(Deserialize, Default)]
pub(crate) struct ListQuery {
    #[serde(default)]
    pub(crate) q: String,
}

pub(crate) async fn tables(
    State(app): State<Shared>,
    Params((uid, branch)): Params<(String, String)>,
    Search(q): Search<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(tables_of(&app, &uid, &branch, &q.q)?))
}

pub(crate) fn tables_of(app: &Shared, uid: &str, branch: &str, q: &str) -> Result<Value, ApiError> {
    let d = app.model(uid, branch)?;
    check_term(q)?;
    Ok(d.tables(q))
}

pub(crate) async fn values(
    State(app): State<Shared>,
    Params((uid, branch, dim)): Params<(String, String, String)>,
    Search(q): Search<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(values_of(&app, &uid, &branch, &dim, &q.q)?))
}

pub(crate) fn values_of(app: &Shared, uid: &str, branch: &str, dim: &str, q: &str) -> Result<Value, ApiError> {
    let d = app.model(uid, branch)?;
    check_term(q)?;
    Ok(d.values_of(dim, q)?)
}

pub(crate) async fn test(
    State(app): State<Shared>,
    held: Option<Extension<Held>>,
    Params((uid, branch)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    let d = app.model(&uid, &branch)?;
    blocking(move || {
        let _held = held;
        Ok(Json(d.run_tests()?))
    })
    .await
}

pub(crate) async fn branch_summary(
    State(app): State<Shared>,
    Params((uid, branch)): Params<(String, String)>,
) -> Result<Json<Value>, ApiError> {
    Ok(Json(app.model(&uid, &branch)?.summary()))
}

/// Every deployed branch; with `q`, the ones whose project or branch
/// holds it.
pub(crate) async fn projects(State(app): State<Shared>, Search(q): Search<ListQuery>) -> Result<Json<Value>, ApiError> {
    Ok(Json(projects_of(&app, &q.q)?))
}

pub(crate) fn projects_of(app: &Shared, q: &str) -> Result<Value, ApiError> {
    check_term(q)?;
    let wanted = zsql_service::holds(q);
    let mut out: Vec<Value> = read(&app.models).values().filter(|d| wanted(&d.uid) || wanted(&d.branch)).map(|d| d.summary()).collect();
    out.sort_by_key(|v| (v["project"].as_str().unwrap_or("").to_string(), v["branch"].as_str().unwrap_or("").to_string()));
    Ok(json!({ "deployments": out }))
}
