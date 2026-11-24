//! Running what was planned: a statement goes to its warehouse and its
//! rows come back, now or by its id.
//!
//! `run` plans a request exactly as `sql` does, then hands the
//! statement to the runs: a stored result that answers is returned, an
//! identical run in flight is joined, and otherwise a run of its own is
//! recorded and queued. The request waits a while for the rows. With
//! them, it answers 200 and a page; without, 202 and the id, which
//! `GET /results/{id}` asks about and `DELETE /results/{id}` cancels.
//!
//! A run that failed is not a failed request: the answer is 200 and a
//! record whose `status` is `failed` and whose `error` says why, the
//! same from `run` as from `results`, so a caller reads one shape.
//!
//! The credential a run connects with is the request's when it sends
//! one, and otherwise the one this server was given for the datasource.
//! Nothing here decides who may ask: the service in front does.

use crate::error::{ApiError, Input, Params, Search};
use crate::probe::connection_of;
use crate::reads::{check_expr, shape_check};
use crate::{blocking, Shared};
use axum::body::Body;
use axum::extract::State;
use axum::http::{header, HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Map, Value};
use std::time::{Duration, Instant};
use zsql_core::SqlRequest;
use zsql_exec::{How, Kind, Record, Status, Submit, DEFAULT_ORG};
use zsql_spec::SecurityContext;

/// The longest a request waits for rows: short of the request's own
/// deadline, so the answer is this server's and not a proxy's.
pub(crate) const LONGEST_WAIT: Duration = Duration::from_secs(80);

/// Rows in a page of JSON when the request does not say, and the most
/// it may ask for.
const PAGE: u64 = 10_000;
const PAGE_MAX: u64 = 100_000;

/// What Arrow is asked for as.
const ARROW: &str = "application/vnd.apache.arrow.stream";

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct RunRequest {
    #[serde(default)]
    spec: Option<Value>,
    #[serde(default)]
    expr: Option<String>,
    #[serde(default)]
    context: Option<SecurityContext>,
    /// Where the datasource is and the secret that opens it, over what
    /// was deployed: the keys a probe takes.
    #[serde(default)]
    connection: Map<String, Value>,
    /// Seconds to wait for the rows before answering with the id.
    #[serde(default)]
    wait: Option<f64>,
    /// Run even though a stored result would answer.
    #[serde(default)]
    fresh: bool,
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
}

/// The connection is never printed: it holds a secret.
impl std::fmt::Debug for RunRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RunRequest(…)")
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct PageQuery {
    #[serde(default)]
    offset: Option<u64>,
    #[serde(default)]
    limit: Option<u64>,
    #[serde(default)]
    wait: Option<f64>,
}

fn invalid(message: impl Into<String>) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "Invalid", message.into())
}

fn internal(e: zsql_exec::Error) -> ApiError {
    match e {
        zsql_exec::Error::Busy(_) => ApiError(StatusCode::SERVICE_UNAVAILABLE, "Busy", format!("{e}; try again shortly")),
        other => ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", other.to_string()),
    }
}

/// A wait in seconds as a request may give it: none is the default,
/// and more than the longest is the longest.
fn wait_of(asked: Option<f64>, default: Duration) -> Result<Duration, ApiError> {
    match asked {
        None => Ok(default.min(LONGEST_WAIT)),
        Some(s) if s.is_finite() && s >= 0.0 => Ok(Duration::from_secs_f64(s).min(LONGEST_WAIT)),
        Some(_) => Err(invalid("wait is a number of seconds, zero or more")),
    }
}

fn page_of(offset: Option<u64>, limit: Option<u64>) -> Result<(u64, u64), ApiError> {
    let limit = limit.unwrap_or(PAGE);
    if limit == 0 || limit > PAGE_MAX {
        return Err(invalid(format!("limit is between 1 and {PAGE_MAX} rows a page")));
    }
    Ok((offset.unwrap_or(0), limit))
}

/// Plans the request and hands its statement to the runs. What comes
/// back is the record, how the request was met, and how long to wait.
fn submit(app: &Shared, uid: &str, branch: &str, req: RunRequest) -> Result<(Record, How, Duration), ApiError> {
    let model = app.model(uid, branch)?;
    check_expr(req.expr.as_deref())?;
    let planned =
        model.plan_request(SqlRequest { spec: req.spec.clone(), expr: req.expr.clone(), context: req.context }, false, &shape_check)?;
    let dialect = app.dialects.get(&planned.plan.adapter).map_err(zsql_core::Error::from)?;
    // The caller's limit, or the server's default when it was given one.
    // With neither, the statement has no limit and returns every row:
    // a sensible bound is the caller's to set.
    let row_limit = planned.limit.or(app.default_sql_limit);
    let sql = planned.plan.sql_with_limit(row_limit, &dialect).map_err(zsql_core::Error::from)?;
    let datasource = planned.plan.datasource_uid.clone();

    let (conn, sent) = connection_of(app, uid, branch, &datasource, None, &req.connection, None)?;
    // The request's credential when it sent one; this server's otherwise.
    let secrets = if sent.is_empty() { (app.secrets)(&datasource) } else { sent };
    let wait = wait_of(req.wait, conn.wait())?;
    // A datasource that keeps nothing is neither looked up nor found.
    let ttl = conn.cache_ttl.map(|s| Duration::from_secs(s.max(0) as u64)).unwrap_or(zsql_exec::DEFAULT_TTL);
    let cache_key = (!ttl.is_zero()).then(|| zsql_exec::run_cache_key(DEFAULT_ORG, uid, &sql));

    let (record, how) = app
        .runs
        .submit(Submit {
            kind: Kind::Semantic,
            org_uid: DEFAULT_ORG.into(),
            project_uid: uid.into(),
            branch: branch.into(),
            datasource_uid: datasource.clone(),
            tables: model.tables_read(&datasource, &sql),
            sql,
            spec: planned.spec.or(req.spec),
            expr: req.expr,
            row_limit,
            cache_key,
            cache_ttl: ttl,
            fresh: req.fresh,
            open: Box::new(move || zsql_datasource::open(&conn, secrets)),
        })
        .map_err(internal)?;
    Ok((record, how, wait))
}

/// The record once the run is over, or as it stands when `wait` is up.
/// Asked again at a widening interval, on no thread of its own.
async fn settled(app: &Shared, record: Record, wait: Duration) -> Result<Record, ApiError> {
    let deadline = Instant::now() + wait;
    let mut record = record;
    let mut pause = Duration::from_millis(5);
    while !record.status.is_final() && Instant::now() < deadline {
        tokio::time::sleep(pause.min(deadline.saturating_duration_since(Instant::now()))).await;
        pause = (pause * 2).min(Duration::from_millis(200));
        let (runs, id) = (app.runs.clone(), record.id.clone());
        record = blocking(move || runs.get(&id).map_err(internal)).await?.ok_or_else(|| gone(&record.id))?;
    }
    Ok(record)
}

fn gone(id: &str) -> ApiError {
    ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no result {id}"))
}

fn wants_arrow(headers: &HeaderMap) -> bool {
    headers.get(header::ACCEPT).and_then(|v| v.to_str().ok()).is_some_and(|accept| accept.contains(ARROW))
}

/// The record as an answer: with a page of rows when it has them, 202
/// while it has not ended, and as Arrow when that was asked for.
async fn answer(app: &Shared, record: Record, how: Option<How>, page: (u64, u64), arrow: bool) -> Result<Response, ApiError> {
    let status = if record.status.is_final() { StatusCode::OK } else { StatusCode::ACCEPTED };
    if record.status == Status::Succeeded && arrow {
        return Ok(arrow_stream(app, &record));
    }
    let mut body = serde_json::to_value(&record).map_err(|e| ApiError(StatusCode::INTERNAL_SERVER_ERROR, "Internal", e.to_string()))?;
    if let Some(how) = how {
        body["cache"] = json!(match how {
            How::Hit => "hit",
            How::Attached => "attached",
            How::Started => "miss",
        });
    }
    if record.status == Status::Succeeded {
        let path = app.runs.rows_path(&record);
        let (offset, limit) = page;
        let total = record.row_count.unwrap_or(0);
        let (columns, rows) = blocking(move || {
            let page = zsql_exec::page(&path, offset, limit).map_err(internal)?;
            Ok((zsql_exec::json::columns(&page.schema), zsql_exec::json::rows(&page).map_err(internal)?))
        })
        .await?;
        body["offset"] = json!(offset);
        body["limit"] = json!(limit);
        // Whether this page reaches the end of the result.
        body["complete"] = json!(offset + rows.len() as u64 >= total);
        body["columns"] = Value::Array(columns);
        body["rows"] = Value::Array(rows);
    }
    Ok((status, Json(body)).into_response())
}

/// The whole result as an Arrow stream, read from its file and sent a
/// batch at a time: neither end holds more than the batch in hand.
fn arrow_stream(app: &Shared, record: &Record) -> Response {
    let path = app.runs.rows_path(record);
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(4);
    tokio::task::spawn_blocking(move || {
        let sent = zsql_exec::arrow_stream(&path, |bytes| {
            tx.blocking_send(Ok(bytes))
                .map_err(|_| zsql_exec::Error::Io("the response".into(), std::io::Error::other("the reader went away")))
        });
        if let Err(e) = sent {
            let _ = tx.blocking_send(Err(std::io::Error::other(e.to_string())));
        }
    });
    let mut response = Response::new(Body::from_stream(Chunks(rx)));
    response.headers_mut().insert(header::CONTENT_TYPE, header::HeaderValue::from_static(ARROW));
    if let Ok(id) = header::HeaderValue::from_str(&record.id) {
        response.headers_mut().insert("zsql-result-id", id);
    }
    response
}

/// The channel a blocking reader fills, as the stream a body is.
struct Chunks(tokio::sync::mpsc::Receiver<Result<Vec<u8>, std::io::Error>>);

impl futures_core::Stream for Chunks {
    type Item = Result<Vec<u8>, std::io::Error>;

    fn poll_next(mut self: std::pin::Pin<&mut Self>, cx: &mut std::task::Context<'_>) -> std::task::Poll<Option<Self::Item>> {
        self.0.poll_recv(cx)
    }
}

pub(crate) async fn run(
    State(app): State<Shared>,
    Params((uid, branch)): Params<(String, String)>,
    headers: HeaderMap,
    Input(req): Input<RunRequest>,
) -> Result<Response, ApiError> {
    let page = page_of(req.offset, req.limit)?;
    let planning = app.clone();
    let (record, how, wait) = blocking(move || submit(&planning, &uid, &branch, req)).await?;
    let record = settled(&app, record, wait).await?;
    answer(&app, record, Some(how), page, wants_arrow(&headers)).await
}

pub(crate) async fn result(
    State(app): State<Shared>,
    Params(id): Params<String>,
    Search(q): Search<PageQuery>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let page = page_of(q.offset, q.limit)?;
    let wait = wait_of(q.wait, Duration::ZERO)?;
    let (runs, asked) = (app.runs.clone(), id.clone());
    let record = blocking(move || runs.get(&asked).map_err(internal)).await?.ok_or_else(|| gone(&id))?;
    let record = settled(&app, record, wait).await?;
    answer(&app, record, None, page, wants_arrow(&headers)).await
}

pub(crate) async fn cancel(State(app): State<Shared>, Params(id): Params<String>) -> Result<Response, ApiError> {
    let (runs, asked) = (app.runs.clone(), id.clone());
    let record = blocking(move || runs.cancel(&asked).map_err(internal)).await?.ok_or_else(|| gone(&id))?;
    // A running statement stops when its read next looks; give it a
    // moment, so the usual answer says `cancelled` and not `running`.
    let record = settled(&app, record, Duration::from_secs(2)).await?;
    answer(&app, record, None, (0, 1), false).await
}
