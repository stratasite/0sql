//! The MCP endpoint: `POST /mcp`, Streamable HTTP, stateless.
//!
//! An agent gets the server's reads as tools: the deployed branches, a
//! branch's fields, tables and sampled values, and the SQL for a query
//! spec. Specs only, never the shorthand: a spec is JSON against a
//! published schema, which is what a model writes reliably, and the
//! schema itself is handed to the client in the tool definition.
//! Nothing here deploys, changes or executes anything.
//!
//! What the client is told, the server's instructions and each tool's
//! description, lives in `mcp_prompts.json` next to this file, and the
//! spec's schema in `mcp_spec_schema.json`. Both are compiled in. They
//! are the instruction set an agent plans from, and they are tuned
//! against the eval in `harness/mcp-eval`.
//!
//! Every tool runs the function the matching REST handler runs. What
//! is different is the security context: an agent is never allowed to
//! say who it is acting as, so a `context` in a tool's arguments is
//! refused outright. The context comes from the connection instead, in
//! the `Zsql-Context` header, which the model does not write: the
//! service in front of this server sets it per user, or whoever
//! configures a client that connects directly sets it once. Without
//! the header a plan runs with no context, which a branch with policies
//! refuses exactly as it does over REST. See `docs/mcp.md`.
//!
//! No authentication and no Origin check: like the rest of the server
//! this endpoint answers whoever can reach it, and belongs behind the
//! developer's own service or on a loopback.
//!
//! The transport is the smallest the specification allows. Each HTTP
//! request carries one JSON-RPC message; there is no `Mcp-Session-Id`,
//! no server-sent events and no GET stream, so there is no session to
//! hijack or outlive anything. A JSON-RPC request is answered with
//! `application/json`, a notification with 202.

use crate::admission::Class;
use crate::error::ApiError;
use crate::guard::{busy, Held};
use crate::names::check_uid;
use crate::reads::{explain_of, explore_of, fields_of, projects_of, sql_of, tables_of, values_of, FieldsQuery};
use crate::{read, App, Shared};
use axum::body::Bytes;
use axum::extract::State;
use axum::http::{HeaderMap, StatusCode};
use axum::response::{IntoResponse, Json, Response};
use serde::Deserialize;
use serde_json::{json, Value};
use std::collections::HashMap;
use std::sync::Arc;
use zsql_semantic::FieldKind;
use zsql_service::{ExploreRequest, Model, SqlRequest};
use zsql_spec::SecurityContext;

pub const PATH: &str = "/mcp";

/// The instruction set: what `initialize` tells the client, and what
/// each tool says about itself.
#[derive(Deserialize)]
struct Prompts {
    instructions: String,
    tools: HashMap<String, String>,
}

/// What MCP clients are told and may call: the prompts, the spec's
/// schema, and which tools are exposed. Compiled in by default; the
/// binary may read other texts (the eval that tunes the wording
/// restarts the server per variant, and an operator may want their
/// own) and an operator may expose a subset of the tools.
pub struct Settings {
    prompts: Prompts,
    schema: Value,
    tools: Option<Vec<String>>,
}

impl Settings {
    /// The compiled-in prompts and schema, every tool exposed.
    pub fn builtin() -> Settings {
        Settings::new(None, None, None).expect("the compiled-in MCP texts parse")
    }

    /// `prompts` and `schema` are JSON texts in place of the compiled-in
    /// ones; `tools` names the tools to expose, or every tool.
    pub fn new(prompts: Option<&str>, schema: Option<&str>, tools: Option<Vec<String>>) -> Result<Settings, String> {
        let prompts: Prompts = serde_json::from_str(prompts.unwrap_or(include_str!("mcp_prompts.json")))
            .map_err(|e| format!("the MCP prompts do not parse: {e}"))?;
        let schema: Value = serde_json::from_str(schema.unwrap_or(include_str!("mcp_spec_schema.json")))
            .map_err(|e| format!("the MCP spec schema does not parse: {e}"))?;
        Ok(Settings { prompts, schema, tools })
    }

    /// Whether a tool is exposed. A call to one that is not is refused
    /// like an unknown tool.
    fn exposed(&self, name: &str) -> bool {
        match &self.tools {
            Some(list) => list.iter().any(|t| t == name),
            None => true,
        }
    }
}

/// The protocol revisions answered, newest first. A client that asks
/// for one of these gets it back; any other gets the newest.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

/// The header that carries the security context every tool call on
/// this request plans under, as the JSON a REST request would send in
/// `context`.
pub const CONTEXT_HEADER: &str = "zsql-context";

/// The context the connection carries, if it carries one. A header
/// that does not parse is refused rather than ignored: planning with no
/// context when one was meant is the wrong answer to a typo.
fn context_of(headers: &HeaderMap) -> Result<Option<SecurityContext>, String> {
    let Some(raw) = headers.get(CONTEXT_HEADER) else { return Ok(None) };
    let text = raw.to_str().map_err(|_| format!("the {CONTEXT_HEADER} header is not text"))?;
    if text.trim().is_empty() {
        return Ok(None);
    }
    let bad = |why: String| format!("the {CONTEXT_HEADER} header is not a security context: {why}");
    // An object and nothing else: serde would read a JSON array as the
    // members in order, and `[]` as a context with nobody in it.
    let value: Value = serde_json::from_str(text).map_err(|e| bad(e.to_string()))?;
    if !value.is_object() {
        return Err(bad("it must be a JSON object".into()));
    }
    serde_json::from_value(value).map(Some).map_err(|e| bad(e.to_string()))
}

// --------------------------------------------------------------- the wire

/// A JSON-RPC error, or an HTTP answer that stands in for the whole
/// response: a full server is reported the way every other route
/// reports it, so a client's existing back-off applies.
enum Fail {
    Rpc(i64, String),
    Http(Response),
}

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;

fn bad_request(code: i64, message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": code, "message": message } }))).into_response()
}

pub async fn endpoint(State(app): State<Shared>, headers: HeaderMap, body: Bytes) -> Response {
    let context = match context_of(&headers) {
        Ok(c) => c,
        Err(m) => return bad_request(-32600, &m),
    };
    if let Some(v) = headers.get("mcp-protocol-version").and_then(|v| v.to_str().ok()) {
        if !VERSIONS.contains(&v.trim()) {
            return bad_request(-32600, &format!("protocol version {v} is not supported; this server speaks {}", VERSIONS.join(", ")));
        }
    }
    let msg: Value = match serde_json::from_slice(&body) {
        Ok(v) => v,
        Err(e) => return bad_request(-32700, &format!("the body is not JSON: {e}")),
    };
    if !msg.is_object() {
        return bad_request(-32600, "the body must be a single JSON-RPC message; batches are not accepted");
    }
    if msg["jsonrpc"] != "2.0" {
        return bad_request(-32600, "jsonrpc must be \"2.0\"");
    }
    let id = msg.get("id").cloned().filter(|v| !v.is_null());
    let method = msg["method"].as_str().map(str::to_string);
    let (Some(method), Some(id)) = (method, id) else {
        // A notification, or a response to something this server never
        // asks: taken and dropped.
        return StatusCode::ACCEPTED.into_response();
    };
    let params = msg.get("params").cloned().unwrap_or(Value::Null);
    match dispatch(&app, context, &method, params).await {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err(Fail::Rpc(code, message)) => {
            Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })).into_response()
        }
        Err(Fail::Http(r)) => r,
    }
}

async fn dispatch(app: &Shared, context: Option<SecurityContext>, method: &str, params: Value) -> Result<Value, Fail> {
    match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = VERSIONS.iter().copied().find(|v| *v == asked).unwrap_or(VERSIONS[0]);
            let client = format!(
                "{} {}",
                params["clientInfo"]["name"].as_str().unwrap_or("a client"),
                params["clientInfo"]["version"].as_str().unwrap_or("")
            )
            .trim()
            .to_string();
            tracing::info!("mcp: connected: {client}");
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "0sql", "version": env!("CARGO_PKG_VERSION") },
                "instructions": format!("{}\n\n{}", app.mcp.prompts.instructions, catalog(app)),
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools(&app.mcp) })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            call(app, context, name, args).await
        }
        _ => Err(Fail::Rpc(METHOD_NOT_FOUND, format!("no method {method}"))),
    }
}

// -------------------------------------------------------------- catalog

/// How much catalog `initialize` carries, in bytes, before it falls
/// back to naming the branches and leaving discovery to `fields`.
const CATALOG_BYTES: usize = 48 * 1024;

/// Every field of every deployed branch, with its uid, as
/// part of the instructions. A client puts the server's instructions in
/// its system prompt, so this is what lets a question be answered in one
/// `sql` call with no discovery round trips: the same move an
/// application makes when it pastes the field catalog into its own
/// prompt. Measures carry their description and synonyms, since that is
/// where the business definitions live; dimensions are named by table,
/// where the table says which thing they describe. Deterministic, so the
/// text is byte-identical from one connection to the next and a client's
/// prompt cache can hold it.
fn catalog(app: &App) -> String {
    let mut deployed: Vec<Arc<Model>> = read(&app.models).values().cloned().collect();
    deployed.sort_by(|a, b| (&a.uid, &a.branch).cmp(&(&b.uid, &b.branch)));
    if deployed.is_empty() {
        return "No branch is deployed yet.".into();
    }
    let mut out = String::new();
    for d in &deployed {
        out.push_str(&branch_catalog(d));
        out.push('\n');
    }
    if out.len() <= CATALOG_BYTES {
        return format!("# Catalog\nEvery field of the deployed branches, with its uid. Write specs from it; call fields only for a term it does not settle.\n\n{out}");
    }
    let mut short = String::from("# Deployed branches\nToo many fields to list here; call fields with q to find them.\n");
    for d in &deployed {
        let s = d.universe().snapshot();
        short.push_str(&format!("- {}/{}: {} tables, {} fields\n", d.uid, d.branch, s.tables.len(), s.fields.len()));
    }
    short
}

fn branch_catalog(d: &Model) -> String {
    let u = d.universe();
    let s = u.snapshot();
    // Field id to the uids of the tables carrying it, sorted.
    let mut tables_of: HashMap<i64, Vec<String>> = HashMap::new();
    for e in &s.expressions {
        if let Some(t) = u.table(e.table_id) {
            tables_of.entry(e.field_id).or_default().push(t.uid.clone());
        }
    }
    for v in tables_of.values_mut() {
        v.sort();
        v.dedup();
    }
    let home = |f: &zsql_semantic::Field| tables_of.get(&f.id).and_then(|t| t.first().cloned()).unwrap_or_else(|| "?".into());
    let mut out = format!(
        "## Project {}, branch {}: {} tables, {} fields, {} policies\n",
        d.uid,
        d.branch,
        s.tables.len(),
        s.fields.len(),
        s.security_policies.len()
    );
    let mut measures: Vec<&zsql_semantic::Field> = u.fields().iter().filter(|f| !f.hidden && f.kind == FieldKind::Measure).collect();
    measures.sort_by(|a, b| (home(a), &a.name).cmp(&(home(b), &b.name)));
    out.push_str("### Measures, by table (uid: name (type), description [synonyms])\n");
    let mut last = String::new();
    for f in measures {
        let h = home(f);
        if h != last {
            out.push_str(&format!("{h}:\n"));
            last = h;
        }
        let data_type = serde_json::to_value(f.data_type).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_else(|| "?".into());
        out.push_str(&format!("- {}: {} ({})", f.uid, f.name, data_type));
        if let Some(desc) = f.description.as_deref().filter(|d| !d.trim().is_empty()) {
            out.push_str(&format!(", {}", desc.trim()));
        }
        if !f.synonyms.is_empty() {
            out.push_str(&format!(" [{}]", f.synonyms.join(", ")));
        }
        out.push('\n');
    }
    // Dimensions by table, each with its uid and the start of its
    // description: that is where "the city" or "at the time of the
    // contact" lives, and it is what tells two same-worded fields apart.
    out.push_str("### Dimensions, by table (name (uid): description)\n");
    let mut dims: Vec<&zsql_semantic::Field> = u.fields().iter().filter(|f| !f.hidden && f.kind == FieldKind::Dimension).collect();
    dims.sort_by(|a, b| (home(a), &a.name).cmp(&(home(b), &b.name)));
    let mut last = String::new();
    for f in dims {
        let h = home(f);
        if h != last {
            if !last.is_empty() {
                out.push('\n');
            }
            out.push_str(&format!("{h}: "));
            last = h;
        } else {
            out.push_str("; ");
        }
        out.push_str(&format!("{} ({})", f.name, f.uid));
        if let Some(desc) = f.description.as_deref().map(str::trim).filter(|d| !d.is_empty()) {
            let short: String = desc.chars().take(110).collect();
            out.push_str(&format!(": {}{}", short.trim_end(), if short.len() < desc.len() { "…" } else { "" }));
        }
    }
    out.push('\n');
    out
}

// ------------------------------------------------------------------ tools

/// The tools, as the client lists them. Every schema refuses unknown
/// properties, so a `context` can be reported for what it is rather
/// than silently dropped.
fn tools(settings: &Settings) -> Vec<Value> {
    let target = json!({
        "project": { "type": "string", "description": "the project uid" },
        "branch": { "type": "string", "description": "a deployed branch; the project's production branch when omitted" },
    });
    // The list tools' filter: the same `q` the REST lists take.
    let holding = json!({ "q": { "type": "string", "description": "keep only entries holding this text, in any case" } });
    let mut spec = settings.schema.clone();
    let defs = spec.as_object_mut().and_then(|m| m.remove("$defs")).unwrap_or(Value::Null);
    let spec_arg = json!({ "spec": spec });
    let describe = |name: &str| settings.prompts.tools.get(name).cloned().unwrap_or_default();
    let schema = |props: &[&Value], required: &[&str], description: &str, with_defs: bool| {
        let mut p = serde_json::Map::new();
        for v in props {
            p.extend(v.as_object().unwrap().clone());
        }
        let mut out =
            json!({ "type": "object", "properties": p, "required": required, "additionalProperties": false, "description": description });
        if with_defs && !defs.is_null() {
            out["$defs"] = defs.clone();
        }
        out
    };
    let all = vec![
        json!({ "name": "list_projects", "description": describe("list_projects"), "inputSchema": schema(&[&holding], &[], "optionally, what to look for", false) }),
        json!({ "name": "fields", "description": describe("fields"),
                "inputSchema": schema(&[&target, &json!({ "q": { "type": "string", "description": "text to search for" }, "hidden": { "type": "boolean", "description": "include hidden fields" } })], &["project"], "which branch, and what to look for", false) }),
        json!({ "name": "tables", "description": describe("tables"), "inputSchema": schema(&[&target, &holding], &["project"], "which branch", false) }),
        json!({ "name": "values", "description": describe("values"),
                "inputSchema": schema(&[&target, &json!({ "dimension": { "type": "string", "description": "the dimension's uid or name" } }), &holding], &["project", "dimension"], "which dimension", false) }),
        json!({ "name": "explore", "description": describe("explore"),
                "inputSchema": schema(&[&target, &spec_arg, &json!({ "q": { "type": "string", "description": "keep fields whose name, uid or synonyms contain or come near this" } })], &["project", "spec"], "a query to extend", true) }),
        json!({ "name": "sql", "description": describe("sql"), "inputSchema": schema(&[&target, &spec_arg], &["project", "spec"], "a query spec", true) }),
        json!({ "name": "explain", "description": describe("explain"), "inputSchema": schema(&[&target, &spec_arg], &["project", "spec"], "a query spec", true) }),
    ];
    all.into_iter().filter(|t| settings.exposed(t["name"].as_str().unwrap_or(""))).collect()
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ListArgs {
    #[serde(default)]
    q: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchArgs {
    project: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    q: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct FieldsArgs {
    project: String,
    #[serde(default)]
    branch: Option<String>,
    #[serde(default)]
    q: Option<String>,
    #[serde(default)]
    hidden: bool,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct ValuesArgs {
    project: String,
    #[serde(default)]
    branch: Option<String>,
    dimension: String,
    #[serde(default)]
    q: Option<String>,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct QueryArgs {
    project: String,
    #[serde(default)]
    branch: Option<String>,
    spec: Value,
    #[serde(default)]
    q: Option<String>,
}

/// Arguments as the tool declared them. A `context` among them gets
/// its own refusal, since a model that sends one is trying to choose
/// who it acts as, and should be told that is not its to choose.
fn args<T: serde::de::DeserializeOwned>(v: Value) -> Result<T, Fail> {
    if v.get("context").is_some() {
        return Err(Fail::Rpc(
            INVALID_PARAMS,
            "a security context is not accepted here: who a query is planned for is fixed by the connection".into(),
        ));
    }
    serde_json::from_value(v).map_err(|e| Fail::Rpc(INVALID_PARAMS, format!("arguments: {e}")))
}

/// The branch a tool works on: the one named, else the one branch of
/// the project that is deployed, else, among several, the project's
/// production branch as its own project.yml names it.
fn branch_for(app: &App, uid: &str, branch: Option<String>) -> Result<String, ApiError> {
    if let Some(b) = branch.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()) {
        return Ok(b);
    }
    check_uid(uid)?;
    let models = read(&app.models);
    let of_project: Vec<&Arc<Model>> = models.values().filter(|d| d.uid == uid).collect();
    match of_project.as_slice() {
        [] => Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no project {uid}"))),
        [one] => Ok(one.branch.clone()),
        several => several.iter().find(|d| d.universe().snapshot().branch.uid == d.branch).map(|d| d.branch.clone()).ok_or_else(|| {
            ApiError(
                StatusCode::BAD_REQUEST,
                "Invalid",
                format!("{uid} has several branches deployed and its production branch is not one of them; name one"),
            )
        }),
    }
}

/// The explore-class tools cost more than a plan, and the guard
/// admitted this request as a plan: take an explore seat too.
async fn admit_explore(app: &App) -> Result<Held, Fail> {
    match app.admission.admit(Class::Explore).await {
        Ok(p) => Ok(Held(Arc::new(p))),
        Err(secs) => Err(Fail::Http(busy(secs, Class::Explore))),
    }
}

/// A refusal for a field that did not resolve, with the fields nearest
/// the term appended: uid, kind, type, description and tables. The
/// refusal then carries the search the client would otherwise make
/// next, so a spec written from the request's own words costs one
/// round trip when it is right and still only two when it is not.
fn with_candidates(app: &Shared, uid: &str, branch: &str, e: ApiError) -> ApiError {
    let ApiError(status, class, message) = e;
    if class != "FieldNotFound" && class != "FieldAmbiguous" {
        return ApiError(status, class, message);
    }
    // The term is the first single-quoted span of the message.
    let term = message.split('\'').nth(1).unwrap_or("").trim().to_string();
    if term.is_empty() || term.len() > 200 {
        return ApiError(status, class, message);
    }
    let found = fields_of(app, uid, branch, FieldsQuery { q: term.clone(), hidden: false });
    let Ok(found) = found else { return ApiError(status, class, message) };
    let mut lines = Vec::new();
    for f in found["fields"].as_array().into_iter().flatten().take(8) {
        let desc = f["description"].as_str().map(|d| d.chars().take(110).collect::<String>()).filter(|d| !d.trim().is_empty());
        let tables: Vec<&str> = f["tables"].as_array().into_iter().flatten().filter_map(Value::as_str).collect();
        lines.push(format!(
            "- {}: {} ({}, {}){} [{}]",
            f["uid"].as_str().unwrap_or(""),
            f["name"].as_str().unwrap_or(""),
            f["kind"].as_str().unwrap_or(""),
            f["data_type"].as_str().unwrap_or(""),
            desc.map(|d| format!(", {d}")).unwrap_or_default(),
            tables.join(", ")
        ));
    }
    if lines.is_empty() {
        return ApiError(status, class, message);
    }
    ApiError(status, class, format!("{message}\nFields near '{term}', by uid:\n{}", lines.join("\n")))
}

async fn call(app: &Shared, context: Option<SecurityContext>, name: &str, arguments: Value) -> Result<Value, Fail> {
    if !app.mcp.exposed(name) {
        return Err(Fail::Rpc(INVALID_PARAMS, format!("no tool {name}")));
    }
    let answer: Result<Value, ApiError> = match name {
        "list_projects" => {
            let a: ListArgs = args(arguments)?;
            projects_of(app, a.q.as_deref().unwrap_or(""))
        }
        "fields" => {
            let a: FieldsArgs = args(arguments)?;
            let _seat = admit_explore(app).await?;
            let q = a.q.unwrap_or_default();
            if q.trim().is_empty() && !a.hidden {
                // No search: the whole catalog, as one text, the same
                // one initialize carries. One call is all the discovery a
                // question needs, and a client that cut the instructions
                // short gets it here in full.
                let text = branch_for(app, &a.project, a.branch).and_then(|b| Ok(branch_catalog(&*app.model(&a.project, &b)?)));
                return Ok(match text {
                    Ok(text) => json!({ "content": [{ "type": "text", "text": text }], "structuredContent": { "catalog": text } }),
                    Err(ApiError(status, class, message)) => {
                        json!({ "content": [{ "type": "text", "text": format!("{class} ({}): {message}", status.as_u16()) }], "isError": true })
                    }
                });
            }
            branch_for(app, &a.project, a.branch).and_then(|b| fields_of(app, &a.project, &b, FieldsQuery { q, hidden: a.hidden }))
        }
        "tables" => {
            let a: BranchArgs = args(arguments)?;
            branch_for(app, &a.project, a.branch).and_then(|b| tables_of(app, &a.project, &b, a.q.as_deref().unwrap_or("")))
        }
        "values" => {
            let a: ValuesArgs = args(arguments)?;
            branch_for(app, &a.project, a.branch).and_then(|b| values_of(app, &a.project, &b, &a.dimension, a.q.as_deref().unwrap_or("")))
        }
        "explore" => {
            let a: QueryArgs = args(arguments)?;
            let _seat = admit_explore(app).await?;
            branch_for(app, &a.project, a.branch)
                .and_then(|b| explore_of(app, &a.project, &b, ExploreRequest { spec: Some(a.spec), expr: None, context, q: a.q }))
                .map(|r| serde_json::to_value(r).expect("an explore answer serializes"))
        }
        "sql" | "explain" => {
            let a: QueryArgs = args(arguments)?;
            if a.q.is_some() {
                return Err(Fail::Rpc(INVALID_PARAMS, "arguments: q is for explore".into()));
            }
            let req = SqlRequest { spec: Some(a.spec), expr: None, context };
            branch_for(app, &a.project, a.branch).and_then(|b| {
                let planned = if name == "sql" {
                    sql_of(app, &a.project, &b, req).map(|r| serde_json::to_value(r).expect("a plan serializes"))
                } else {
                    explain_of(app, &a.project, &b, req).map(|r| serde_json::to_value(r).expect("a plan serializes"))
                };
                planned.map_err(|e| with_candidates(app, &a.project, &b, e))
            })
        }
        _ => return Err(Fail::Rpc(INVALID_PARAMS, format!("no tool {name}"))),
    };
    Ok(match answer {
        Ok(v) => {
            json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "structuredContent": v })
        }
        // What the REST route would have said, as the tool's own result,
        // so the model reads the refusal instead of a transport failure.
        Err(ApiError(status, class, message)) => {
            json!({ "content": [{ "type": "text", "text": format!("{class} ({}): {message}", status.as_u16()) }], "isError": true })
        }
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn headers(value: &str) -> HeaderMap {
        let mut h = HeaderMap::new();
        h.insert(CONTEXT_HEADER, value.parse().unwrap());
        h
    }

    #[test]
    fn the_context_header_is_a_security_context_or_a_refusal() {
        assert!(context_of(&HeaderMap::new()).unwrap().is_none());
        assert!(context_of(&headers(" ")).unwrap().is_none());
        let c = context_of(&headers(r#"{"email":"ann@acme.test","groups":[{"name":"emea"}]}"#)).unwrap().unwrap();
        assert_eq!(c.email.as_deref(), Some("ann@acme.test"));
        assert_eq!(c.groups[0].name.as_deref(), Some("emea"));
        assert!(context_of(&headers("ann@acme.test")).is_err());
        assert!(context_of(&headers("[]")).is_err());
        assert!(context_of(&headers(r#"{"emial":"ann@acme.test"}"#)).is_err(), "an unknown member is a typo, not nothing");
    }
}
