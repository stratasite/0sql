//! Probing a datasource: what `zsql catalog` does from a project
//! directory, for a caller that has only the server. A page of a
//! warehouse's tables, and one table's columns, keys and comment.
//!
//! A deploy keeps every key of a datasource's entry, so the connection
//! is what was deployed and, over it, whatever the request sends: the
//! keys `datasources.yml` takes, and beside them the secret `.zsql`
//! would have held. A datasource deployed whole is probed with no body;
//! one deployed with only its adapter has everything sent. What a
//! request sends is used for the one read and dropped with it; nothing
//! here writes it down, logs it, or keeps a connection open between
//! requests. The adapter is the deployed one, so what is probed is the
//! warehouse the model was written for.
//!
//! These are reads, and as open as the others: who may ask is decided
//! by the service this server sits behind.

use crate::error::{ApiError, Org, Params, Search};
use crate::guard::Held;
use crate::reads::{check_term, too_big, ListQuery};
use crate::{blocking, Shared};
use axum::extract::{Extension, State};
use axum::http::StatusCode;
use axum::response::Json;
use serde::Deserialize;
use serde_json::{json, Map, Value};
use zsql_datasource::{Catalog, Connection, Filter, Secrets, ALIASES, CONNECTION_KEYS, SECRET_FIELDS};

/// The most tables one page holds, and how many it holds when the
/// request does not say. A warehouse can have tens of thousands, so a
/// listing is always a page, with or without a filter.
pub(crate) const PAGE: usize = 100;

/// The longest a probe's statement may run: short of the request's own
/// deadline, so a slow warehouse is answered with its own error rather
/// than a bare timeout.
const STATEMENT_SECS: i64 = 80;

/// The body of a probe: what the request says about where the
/// datasource is and the secret that opens it, over what the deploy
/// said. Empty, or no body at all, when the deploy said everything.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct ProbeRequest {
    #[serde(default)]
    connection: Map<String, Value>,
}

/// A probe's body, which may be absent: a datasource deployed with its
/// whole connection is probed with nothing sent.
pub(crate) struct Sent(ProbeRequest);

impl<S: Send + Sync> axum::extract::FromRequest<S> for Sent {
    type Rejection = ApiError;

    async fn from_request(req: axum::extract::Request, state: &S) -> Result<Self, ApiError> {
        let bytes = axum::body::Bytes::from_request(req, state).await.map_err(|e| ApiError(e.status(), "Invalid", e.body_text()))?;
        if bytes.iter().all(u8::is_ascii_whitespace) {
            return Ok(Sent(ProbeRequest::default()));
        }
        // The message names where the body went wrong and never quotes
        // it: serde_json's errors carry a position, not the text.
        serde_json::from_slice(&bytes).map(Sent).map_err(|e| invalid(format!("the body is not a probe request: {e}")))
    }
}

/// The connection is never printed, even by a derived `Debug` in an
/// error path: it holds a secret.
impl std::fmt::Debug for ProbeRequest {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ProbeRequest(…)")
    }
}

/// What narrows a listing. A warehouse has a catalog above its schemas,
/// or only schemas, or neither; each is optional, and one the engine
/// does not have is refused rather than read past.
#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct TablesQuery {
    #[serde(default)]
    q: String,
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    catalog: Option<String>,
    #[serde(default)]
    limit: Option<usize>,
    #[serde(default)]
    offset: Option<usize>,
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
pub(crate) struct TableQuery {
    #[serde(default)]
    schema: Option<String>,
    #[serde(default)]
    catalog: Option<String>,
}

fn invalid(message: String) -> ApiError {
    ApiError(StatusCode::BAD_REQUEST, "Invalid", message)
}

/// A name from the query string or the path: trimmed, absent when
/// empty, and bounded like a search term.
fn named(v: Option<String>) -> Result<Option<String>, ApiError> {
    let v = v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    if let Some(s) = &v {
        check_term(s)?;
    }
    Ok(v)
}

/// What the warehouse or the connection said, as the API reports it. A
/// connection that cannot be made from what was sent is the caller's to
/// fix (400); a warehouse that would not answer is upstream (502). The
/// datasource crate words some of these for a project directory, so the
/// ones that name a file are said again for a request.
fn refused(e: zsql_datasource::Error) -> ApiError {
    let (status, message) = match &e {
        zsql_datasource::Error::Connect(..)
        | zsql_datasource::Error::Query(..)
        | zsql_datasource::Error::TooManyRows(..)
        | zsql_datasource::Error::Io(..) => (StatusCode::BAD_GATEWAY, e.to_string()),
        zsql_datasource::Error::Missing(uid, what) => {
            (StatusCode::BAD_REQUEST, format!("datasource {uid} has no {what}; deploy it in datasources.yml or send it in the connection"))
        }
        zsql_datasource::Error::NoSecret { uid, .. } => (
            StatusCode::BAD_REQUEST,
            format!("datasource {uid} needs a password; deploy the datasource with its secret or send one in the connection"),
        ),
        zsql_datasource::Error::NoSchemas(engine) => (StatusCode::BAD_REQUEST, format!("{engine} has no schemas; ask without one")),
        _ => (StatusCode::BAD_REQUEST, e.to_string()),
    };
    ApiError(status, "DatasourceError", message)
}

/// The settings that are numbers; every other one is text.
const NUMBERS: [&str; 4] = ["port", "query_timeout", "cache_ttl", "query_wait_time"];

/// A setting as the connection reads it: a number sent as text is still
/// the number, and a secret or a name sent as a number is still text.
/// `None` for a value that cannot be the setting.
fn scalar(key: &str, value: &Value) -> Option<Value> {
    let number = NUMBERS.contains(&key);
    match value {
        Value::Array(_) | Value::Object(_) => None,
        Value::String(text) if number => text.trim().parse::<i64>().ok().map(Value::from),
        Value::Number(n) if !number => Some(Value::String(n.to_string())),
        other => Some(other.clone()),
    }
}

/// The connection for one probe or run: what the datasource was
/// deployed with, and over it what the request sent. Its credential is
/// the request's when the request sends any secret, and the one the
/// datasource was deployed with otherwise. The deployed entry is read as
/// `datasources.yml` is, past whatever is not a connection setting; the
/// request is checked key by key, so a misspelled setting is named and
/// not read past into a confusing refusal from the warehouse.
///
/// `(org, uid, branch)` is the branch the datasource is one of.
pub(crate) fn connection_of(
    app: &Shared,
    (org, uid, branch): (&str, &str, &str),
    datasource: &str,
    catalog: Option<&str>,
    sent: &Map<String, Value>,
    longest: Option<i64>,
) -> Result<(Connection, Secrets), ApiError> {
    let d = app.model(org, uid, branch)?;
    check_term(datasource)?;
    let Some(deployed) = d.datasource(datasource) else {
        let known: Vec<String> = d.datasources("")["datasources"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|x| x["uid"].as_str().map(str::to_string))
            .collect();
        return Err(ApiError(
            StatusCode::NOT_FOUND,
            "NotFound",
            format!("project {uid} branch {branch} has no datasource {datasource} (it has: {})", known.join(", ")),
        ));
    };
    let adapter = deployed.adapter.clone();
    let mut settings: Map<String, Value> =
        deployed.connection.iter().filter_map(|(key, value)| Some((key.clone(), scalar(key, value)?))).collect();
    if let Some(seconds) = deployed.query_timeout {
        settings.insert("query_timeout".into(), Value::from(seconds));
    }
    for (key, value) in sent {
        if !CONNECTION_KEYS.contains(&key.as_str()) && !SECRET_FIELDS.contains(&key.as_str()) {
            return Err(invalid(format!("connection.{key} is not a connection setting")));
        }
        let Some(value) = scalar(key, value) else {
            let what = if NUMBERS.contains(&key.as_str()) { "a number" } else { "a single value" };
            return Err(invalid(format!("connection.{key} must be {what}")));
        };
        if key == "adapter" && value.as_str().is_some_and(|sent| !sent.trim().eq_ignore_ascii_case(&adapter)) {
            return Err(invalid(format!(
                "datasource {datasource} is deployed as {adapter}; the connection says {}",
                value.as_str().unwrap_or_default().trim()
            )));
        }
        for names in ALIASES.iter().filter(|names| names.contains(&key.as_str())) {
            for name in names.iter() {
                settings.remove(*name);
            }
        }
        settings.insert(key.clone(), value);
    }
    // The credential is one or the other, never a blend: what the
    // request sent, when it sent any secret at all, and otherwise what
    // the datasource was deployed with. And the deployed one goes only
    // where it was deployed for: a request that names another host,
    // user, file or way of securing the connection connects with what
    // it sent, so nobody who reaches the port can have the deployed
    // secret presented to a server of their choosing.
    let sent_one = sent.keys().any(|key| SECRET_FIELDS.contains(&key.as_str()));
    let elsewhere = sent.keys().any(|key| {
        key != "adapter" && !SECRET_FIELDS.contains(&key.as_str()) && !crate::credentials::HOW_IT_IS_USED.contains(&key.as_str())
    });
    if !sent_one && !elsewhere {
        for (key, value) in d.secrets_of(datasource).into_iter().flatten() {
            if let Some(value) = scalar(key, value) {
                settings.insert(key.clone(), value);
            }
        }
    }
    let settings = serde_yaml::to_value(&settings).map_err(|e| invalid(format!("connection: {e}")))?;
    let mut conn = Connection::from_value(datasource, &settings).map_err(refused)?;
    conn.adapter = adapter;
    if let Some(catalog) = catalog {
        conn.in_catalog(catalog).map_err(refused)?;
    }
    // A probe's statement is kept short of its request's deadline; a
    // run's is the datasource's own.
    if let Some(longest) = longest {
        conn.query_timeout = Some(conn.query_timeout.filter(|s| *s > 0).unwrap_or(60).min(longest));
    }
    Ok((conn, Secrets::from_value(&settings)))
}

/// The model's datasources: what there is to probe.
pub(crate) async fn datasources(
    State(app): State<Shared>,
    Org(org): Org,
    Params((uid, branch)): Params<(String, String)>,
    Search(q): Search<ListQuery>,
) -> Result<Json<Value>, ApiError> {
    let d = app.model(&org, &uid, &branch)?;
    check_term(&q.q)?;
    Ok(Json(d.datasources(&q.q)))
}

/// One page of a datasource's tables, by schema and name: each with its
/// kind, the warehouse's row estimate and its comment, and nothing
/// below. `q` keeps the tables whose name holds it; `schema` and
/// `catalog` say where to look; `limit` and `offset` walk the pages, and
/// `next_offset` is the offset of the next one when there is one.
pub(crate) async fn tables(
    State(app): State<Shared>,
    held: Option<Extension<Held>>,
    Org(org): Org,
    Params((uid, branch, datasource)): Params<(String, String, String)>,
    Search(p): Search<TablesQuery>,
    Sent(req): Sent,
) -> Result<Json<Value>, ApiError> {
    check_term(&p.q)?;
    let q = p.q.trim().to_string();
    let schema = named(p.schema)?;
    let catalog = named(p.catalog)?;
    let limit = p.limit.unwrap_or(PAGE);
    if limit == 0 {
        return Err(invalid("limit must be at least 1".into()));
    }
    if limit > PAGE {
        return Err(too_big("tables in one page", limit, PAGE));
    }
    let offset = p.offset.unwrap_or(0);
    let (conn, secrets) =
        connection_of(&app, (&org, &uid, &branch), &datasource, catalog.as_deref(), &req.connection, Some(STATEMENT_SECS))?;
    blocking(move || {
        let _held = held;
        let mut src = zsql_datasource::open(&conn, secrets).map_err(refused)?;
        let filter = if q.is_empty() { Filter::schema(schema.as_deref()) } else { Filter::like(schema.as_deref(), &q) };
        let page = Catalog::list(&mut *src, &filter, limit, offset).map_err(refused)?;
        let tables: Vec<Value> = page
            .tables
            .iter()
            .map(|t| {
                json!({
                    "schema": t.schema, "name": t.name, "physical_name": t.physical_name(),
                    "kind": t.kind, "rows": t.rows, "comment": t.comment,
                })
            })
            .collect();
        Ok(Json(json!({
            "datasource": conn.uid, "adapter": conn.adapter, "catalog": catalog, "schema": schema,
            "tables": tables,
            "limit": limit, "offset": offset,
            "next_offset": page.more.then_some(offset + limit),
            "warnings": src.warnings(),
        })))
    })
    .await
}

/// A table as the path names it: `table`, `schema.table` or
/// `catalog.schema.table`. With `?schema=` the path is the table's
/// whole name, which is how a name with a dot in it is asked for.
fn split(path: &str, schema: Option<String>, catalog: Option<String>) -> Result<(Option<String>, Option<String>, String), ApiError> {
    let path = path.trim();
    if path.is_empty() {
        return Err(invalid("a table name is needed".into()));
    }
    if schema.is_some() {
        return Ok((catalog, schema, path.to_string()));
    }
    let mut parts: Vec<&str> = path.rsplitn(3, '.').collect();
    parts.reverse();
    if parts.iter().any(|p| p.is_empty()) {
        return Err(invalid(format!("'{path}' is not a table name: table, schema.table or catalog.schema.table")));
    }
    Ok(match parts.as_slice() {
        [c, s, t] => {
            if let Some(asked) = catalog.as_deref().filter(|asked| !asked.eq_ignore_ascii_case(c)) {
                return Err(invalid(format!("the path says catalog {c} and the query says {asked}")));
            }
            (Some(c.to_string()), Some(s.to_string()), t.to_string())
        }
        [s, t] => (catalog, Some(s.to_string()), t.to_string()),
        _ => (catalog, None, path.to_string()),
    })
}

/// One table's structure: its columns in order with the warehouse's own
/// types, its primary and foreign keys, its comment and row estimate.
/// A name found in several schemas is refused with the schemas, never
/// answered with the first.
pub(crate) async fn table(
    State(app): State<Shared>,
    held: Option<Extension<Held>>,
    Org(org): Org,
    Params((uid, branch, datasource, table)): Params<(String, String, String, String)>,
    Search(p): Search<TableQuery>,
    Sent(req): Sent,
) -> Result<Json<Value>, ApiError> {
    check_term(&table)?;
    let (catalog, schema, name) = split(&table, named(p.schema)?, named(p.catalog)?)?;
    let (conn, secrets) =
        connection_of(&app, (&org, &uid, &branch), &datasource, catalog.as_deref(), &req.connection, Some(STATEMENT_SECS))?;
    blocking(move || {
        let _held = held;
        let mut src = zsql_datasource::open(&conn, secrets).map_err(refused)?;
        let mut cat = Catalog::read_filtered(&mut *src, &conn.uid, &Filter::table(schema.as_deref(), &name)).map_err(refused)?;
        let within = schema.as_deref().map(|s| format!(" in schema {s}")).unwrap_or_default();
        if cat.tables.len() > 1 {
            let found: Vec<String> = cat.tables.iter().map(|t| t.physical_name()).collect();
            return Err(ApiError(
                StatusCode::BAD_REQUEST,
                "Ambiguous",
                format!("datasource {} has several tables named {name}: {}; say which with its schema", conn.uid, found.join(", ")),
            ));
        }
        let Some(found) = cat.tables.pop() else {
            return Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("datasource {} has no table {name}{within}", conn.uid)));
        };
        let mut out = serde_json::to_value(&found).expect("a table serializes");
        out["physical_name"] = Value::String(found.physical_name());
        Ok(Json(json!({
            "datasource": conn.uid, "adapter": conn.adapter, "catalog": catalog,
            "table": out,
            "read_at": cat.read_at,
            "warnings": src.warnings(),
        })))
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_table_is_named_by_one_two_or_three_parts_and_whole_beside_a_schema() {
        let s = |v: &str| Some(v.to_string());
        assert_eq!(split("orders", None, None).ok(), Some((None, None, "orders".into())));
        assert_eq!(split("dse.cs_contact_f", None, None).ok(), Some((None, s("dse"), "cs_contact_f".into())));
        assert_eq!(split("main.dse.cs_contact_f", None, None).ok(), Some((s("main"), s("dse"), "cs_contact_f".into())));
        assert_eq!(split("dse.cs_contact_f", None, s("main")).ok(), Some((s("main"), s("dse"), "cs_contact_f".into())));
        assert_eq!(split("a.b.c.d", None, None).ok(), Some((s("a.b"), s("c"), "d".into())), "what is left of three is the catalog");
        assert_eq!(
            split("odd.name", s("dse"), None).ok(),
            Some((None, s("dse"), "odd.name".into())),
            "a schema makes the path the whole name"
        );
        assert_eq!(split("MAIN.dse.t", None, s("main")).ok(), Some((s("MAIN"), s("dse"), "t".into())));
        for bad in ["", " ", ".t", "s.", "a..t"] {
            assert!(split(bad, None, None).is_err(), "{bad:?}");
        }
        let e = split("main.dse.t", None, s("other")).err().unwrap();
        assert!(e.2.contains("main") && e.2.contains("other"), "{}", e.2);
    }

    #[test]
    fn a_probe_request_never_prints_its_connection() {
        let req: ProbeRequest = serde_json::from_value(json!({ "connection": { "password": "hunter2" } })).unwrap();
        assert!(!format!("{req:?}").contains("hunter2"));
        assert!(
            serde_json::from_value::<ProbeRequest>(json!({ "connection": {}, "schema": "x" })).is_err(),
            "filters go in the query string"
        );
    }
}
