//! The MCP endpoint: `POST /mcp`, Streamable HTTP, stateless.
//!
//! An agent connects with the same key an application would and gets
//! the server's reads as tools: the deployments it may see, a branch's
//! fields, tables and sampled values, and the SQL for a query spec.
//! Specs only, never the shorthand: a spec is JSON against a published
//! schema, which is what a model writes reliably, and the schema itself
//! is handed to the client in the tool definition. Nothing here deploys,
//! changes or executes anything.
//!
//! What the client is told, the server's instructions and each tool's
//! description, lives in `mcp_prompts.json` next to this file, and the
//! spec's schema in `mcp_spec_schema.json`. Both are compiled in. They
//! are the instruction set an agent plans from, and they are tuned
//! against the eval in `harness/mcp-eval`, so they are kept apart from
//! the code that enforces who may read what.
//!
//! Every tool runs the function the matching REST handler runs, with
//! the principal the guard worked out, so a key reaches through MCP
//! exactly what it reaches over `/projects/...` and nothing more. What
//! is different is the security context: an agent is never allowed to
//! say who it is acting as. A personal key plans as its user's email, a
//! query key as the context its project's owner bound to the grant, and
//! a `context` in a tool's arguments is refused outright. See
//! `docs/mcp.md`.
//!
//! The transport is the smallest the specification allows. Each HTTP
//! request carries one JSON-RPC message and its key; there is no
//! `Mcp-Session-Id`, no server-sent events and no GET stream, so there
//! is no session to hijack, outlive a key, or bind to the wrong one. A
//! JSON-RPC request is answered with `application/json`, a notification
//! with 202. Written by hand rather than taken from a library so that
//! every byte between the Authorization header and `gate()` is in this
//! crate, where it is tested with the rest of the isolation suite.

use super::*;
use axum::http::HeaderMap;
use std::sync::OnceLock;

pub const PATH: &str = "/mcp";

/// The instruction set: what `initialize` tells the client, and what
/// each tool says about itself.
#[derive(Deserialize)]
struct Prompts {
    instructions: String,
    tools: HashMap<String, String>,
}

/// The compiled-in file, unless `ZSQL_MCP_PROMPTS` names one to read
/// instead: the eval that tunes these restarts the server per variant,
/// and an operator may want their own wording. A file that does not
/// parse is a startup failure, not a silent fallback.
fn prompts() -> &'static Prompts {
    static P: OnceLock<Prompts> = OnceLock::new();
    P.get_or_init(|| serde_json::from_str(&file_or("ZSQL_MCP_PROMPTS", include_str!("mcp_prompts.json"))).expect("the MCP prompts parse"))
}

/// The query spec's JSON schema, as the sql, explain and explore tools
/// declare their `spec` argument. Its `$defs` are hoisted to the root of
/// each tool's input schema, where the `$ref`s point. `ZSQL_MCP_SPEC_SCHEMA`
/// names a file to read instead of the compiled-in one.
fn spec_schema() -> &'static Value {
    static S: OnceLock<Value> = OnceLock::new();
    S.get_or_init(|| serde_json::from_str(&file_or("ZSQL_MCP_SPEC_SCHEMA", include_str!("mcp_spec_schema.json"))).expect("the MCP spec schema parses"))
}

fn file_or(var: &str, compiled: &str) -> String {
    match std::env::var(var) {
        Ok(path) if !path.trim().is_empty() => {
            let text = std::fs::read_to_string(&path).unwrap_or_else(|e| panic!("{var}: cannot read {path}: {e}"));
            eprintln!("zsqld: mcp: {var} read from {path}");
            text
        }
        _ => compiled.to_string(),
    }
}

/// The protocol revisions answered, newest first. A client that asks
/// for one of these gets it back; any other gets the newest.
const VERSIONS: &[&str] = &["2025-11-25", "2025-06-18", "2025-03-26"];

/// `/mcp`, read the way the limiter reads paths.
pub fn is_endpoint(path: &str) -> bool {
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    p == ["mcp"]
}

/// The transport's rule against DNS rebinding: a request that names an
/// `Origin` must name this service's own, or it is refused before any
/// key is read. Programs send no Origin and pass; a page in a browser
/// sends its own and is turned away unless it is the app itself.
pub fn check_origin(public_url: &str, headers: &HeaderMap) -> Result<(), Response> {
    let Some(origin) = headers.get("origin").and_then(|v| v.to_str().ok()) else { return Ok(()) };
    if origin_of(origin) == origin_of(public_url) {
        return Ok(());
    }
    Err(ApiError(StatusCode::FORBIDDEN, "Forbidden", format!("origin {origin} may not use this endpoint")).into_response())
}

/// `scheme://host[:port]`, lower-cased, default ports dropped; `None`
/// when the text is not an origin, which never equals anything.
fn origin_of(text: &str) -> Option<String> {
    let text = text.trim().to_lowercase();
    let (scheme, rest) = text.split_once("://")?;
    let host_port = rest.split(['/', '?', '#']).next()?;
    if host_port.is_empty() {
        return None;
    }
    let host_port = match (scheme, host_port.rsplit_once(':')) {
        ("http", Some((h, "80"))) | ("https", Some((h, "443"))) => h.to_string(),
        _ => host_port.to_string(),
    };
    Some(format!("{scheme}://{host_port}"))
}

/// On a 401, the header the specification has clients read. No
/// `resource_metadata` is named because there is no OAuth server to
/// discover; a client falls through to the fixed credential its user
/// configured, which is the key.
pub fn challenge(res: &mut Response) {
    res.headers_mut().insert("www-authenticate", "Bearer realm=\"0sql\"".parse().expect("a header"));
}

// --------------------------------------------------------------- the wire

/// A JSON-RPC error, or an HTTP answer that stands in for the whole
/// response: a rate limit or a full server is reported the way every
/// other route reports it, so a client's existing back-off applies.
enum Fail {
    Rpc(i64, String),
    Http(Response),
}

const INVALID_PARAMS: i64 = -32602;
const METHOD_NOT_FOUND: i64 = -32601;

fn bad_request(code: i64, message: &str) -> Response {
    (StatusCode::BAD_REQUEST, Json(json!({ "jsonrpc": "2.0", "id": null, "error": { "code": code, "message": message } }))).into_response()
}

pub async fn endpoint(State(app): State<Shared>, Auth(who): Auth, headers: HeaderMap, body: Bytes) -> Response {
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
    match dispatch(&app, &who, &method, params).await {
        Ok(result) => Json(json!({ "jsonrpc": "2.0", "id": id, "result": result })).into_response(),
        Err(Fail::Rpc(code, message)) => Json(json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })).into_response(),
        Err(Fail::Http(r)) => r,
    }
}

async fn dispatch(app: &Shared, who: &Option<Principal>, method: &str, params: Value) -> Result<Value, Fail> {
    match method {
        "initialize" => {
            let asked = params["protocolVersion"].as_str().unwrap_or("");
            let version = VERSIONS.iter().copied().find(|v| *v == asked).unwrap_or(VERSIONS[0]);
            let client = format!("{} {}", params["clientInfo"]["name"].as_str().unwrap_or("a client"), params["clientInfo"]["version"].as_str().unwrap_or("")).trim().to_string();
            eprintln!("zsqld: mcp: {} connected: {client}", who.as_ref().map(Principal::describe).unwrap_or_else(|| "single-tenant".into()));
            Ok(json!({
                "protocolVersion": version,
                "capabilities": { "tools": {} },
                "serverInfo": { "name": "0sql", "version": env!("CARGO_PKG_VERSION") },
                "instructions": prompts().instructions,
            }))
        }
        "ping" => Ok(json!({})),
        "tools/list" => Ok(json!({ "tools": tools() })),
        "tools/call" => {
            let name = params["name"].as_str().unwrap_or("");
            let args = params.get("arguments").cloned().unwrap_or_else(|| json!({}));
            call(app, who, name, args).await
        }
        _ => Err(Fail::Rpc(METHOD_NOT_FOUND, format!("no method {method}"))),
    }
}

// ------------------------------------------------------------------ tools

/// The tools, as the client lists them. Every schema refuses unknown
/// properties, so a `context` can be reported for what it is rather
/// than silently dropped.
fn tools() -> Vec<Value> {
    let target = json!({
        "project": { "type": "string", "description": "the project uid" },
        "branch": { "type": "string", "description": "a deployed branch; the project's production branch when omitted" },
    });
    let mut spec = spec_schema().clone();
    let defs = spec.as_object_mut().and_then(|m| m.remove("$defs")).unwrap_or(Value::Null);
    let spec_arg = json!({ "spec": spec });
    let describe = |name: &str| prompts().tools.get(name).cloned().unwrap_or_default();
    let schema = |props: &[&Value], required: &[&str], description: &str, with_defs: bool| {
        let mut p = serde_json::Map::new();
        for v in props {
            p.extend(v.as_object().unwrap().clone());
        }
        let mut out = json!({ "type": "object", "properties": p, "required": required, "additionalProperties": false, "description": description });
        if with_defs && !defs.is_null() {
            out["$defs"] = defs.clone();
        }
        out
    };
    vec![
        json!({ "name": "list_projects", "description": describe("list_projects"), "inputSchema": schema(&[], &[], "no arguments", false) }),
        json!({ "name": "fields", "description": describe("fields"),
                "inputSchema": schema(&[&target, &json!({ "q": { "type": "string", "description": "text to search for" }, "hidden": { "type": "boolean", "description": "include hidden fields" } })], &["project"], "which branch, and what to look for", false) }),
        json!({ "name": "tables", "description": describe("tables"), "inputSchema": schema(&[&target], &["project"], "which branch", false) }),
        json!({ "name": "values", "description": describe("values"),
                "inputSchema": schema(&[&target, &json!({ "dimension": { "type": "string", "description": "the dimension's uid or name" } })], &["project", "dimension"], "which dimension", false) }),
        json!({ "name": "explore", "description": describe("explore"),
                "inputSchema": schema(&[&target, &spec_arg, &json!({ "q": { "type": "string", "description": "keep fields whose name, uid or synonyms contain or come near this" } })], &["project", "spec"], "a query to extend", true) }),
        json!({ "name": "sql", "description": describe("sql"), "inputSchema": schema(&[&target, &spec_arg], &["project", "spec"], "a query spec", true) }),
        json!({ "name": "explain", "description": describe("explain"), "inputSchema": schema(&[&target, &spec_arg], &["project", "spec"], "a query spec", true) }),
    ]
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct NoArgs {}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct BranchArgs {
    project: String,
    #[serde(default)]
    branch: Option<String>,
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
        return Err(Fail::Rpc(INVALID_PARAMS, "a security context is not accepted here: who a query is planned for is fixed by the key".into()));
    }
    serde_json::from_value(v).map_err(|e| Fail::Rpc(INVALID_PARAMS, format!("arguments: {e}")))
}

/// Who the plan is for, when the caller may not say. A person plans as
/// themselves; a query key sends nothing and gets the context bound to
/// its grant, or the refusal a missing context always got.
fn context_for(who: &Option<Principal>) -> Option<SecurityContext> {
    match who {
        Some(Principal::User { user, .. }) => Some(SecurityContext { email: Some(user.email.clone()), ..Default::default() }),
        _ => None,
    }
}

/// The branch a tool works on: the one named, else the project's
/// production branch, else (with accounts off) the one branch deployed.
fn branch_for(app: &App, who: &Option<Principal>, uid: &str, branch: Option<String>) -> Result<String, ApiError> {
    if let Some(b) = branch.map(|b| b.trim().to_string()).filter(|b| !b.is_empty()) {
        return Ok(b);
    }
    check_uid(uid)?;
    if let (Some(store), Some(p)) = (&app.accounts, who) {
        let project = store.project(p.account().id, uid).map_err(internal)?.ok_or_else(|| ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no project {uid}")))?;
        return Ok(project.production_branch);
    }
    let models = app.models.read().unwrap();
    let mut branches: Vec<String> = models.values().filter(|d| d.uid == uid).map(|d| d.branch.clone()).collect();
    match branches.len() {
        1 => Ok(branches.remove(0)),
        0 => Err(ApiError(StatusCode::NOT_FOUND, "NotFound", format!("no project {uid}"))),
        _ => Err(ApiError(StatusCode::BAD_REQUEST, "Invalid", format!("{uid} has several branches deployed; name one"))),
    }
}

/// The explore-class tools cost more than a plan, and the guard charged
/// this request as a plan: take the explore allowance and a seat too.
async fn admit_explore(app: &App, who: &Option<Principal>) -> Result<Held, Fail> {
    if let Some(p) = who {
        let key_id = match p {
            Principal::QueryKey { key, .. } => Some(key.id),
            Principal::User { .. } => None,
        };
        let d = app.limits.principal(p.account().id, key_id, Class::Explore);
        if app.limits.enforced(&d, &format!("{} on explore (mcp)", p.describe())) {
            return Err(Fail::Http(too_many(&d)));
        }
    }
    match app.admission.admit(Class::Explore).await {
        Ok(p) => Ok(Held(Arc::new(p))),
        Err(secs) => Err(Fail::Http(busy(secs, Class::Explore))),
    }
}

async fn call(app: &Shared, who: &Option<Principal>, name: &str, arguments: Value) -> Result<Value, Fail> {
    let answer: Result<Value, ApiError> = match name {
        "list_projects" => {
            let NoArgs {} = args(arguments)?;
            projects_of(app, who)
        }
        "fields" => {
            let a: FieldsArgs = args(arguments)?;
            let _seat = admit_explore(app, who).await?;
            branch_for(app, who, &a.project, a.branch).and_then(|b| fields_of(app, who, &a.project, &b, FieldsQuery { q: a.q.unwrap_or_default(), hidden: a.hidden }))
        }
        "tables" => {
            let a: BranchArgs = args(arguments)?;
            branch_for(app, who, &a.project, a.branch).and_then(|b| tables_of(app, who, &a.project, &b))
        }
        "values" => {
            let a: ValuesArgs = args(arguments)?;
            branch_for(app, who, &a.project, a.branch).and_then(|b| values_of(app, who, &a.project, &b, &a.dimension))
        }
        "explore" => {
            let a: QueryArgs = args(arguments)?;
            let _seat = admit_explore(app, who).await?;
            branch_for(app, who, &a.project, a.branch)
                .and_then(|b| explore_of(app, who, &a.project, &b, ExploreRequest { spec: Some(a.spec), expr: None, context: context_for(who), q: a.q }))
                .map(|r| serde_json::to_value(r).expect("an explore answer serializes"))
        }
        "sql" | "explain" => {
            let a: QueryArgs = args(arguments)?;
            if a.q.is_some() {
                return Err(Fail::Rpc(INVALID_PARAMS, "arguments: q is for explore".into()));
            }
            let req = SqlRequest { spec: Some(a.spec), expr: None, context: context_for(who) };
            branch_for(app, who, &a.project, a.branch).and_then(|b| {
                if name == "sql" {
                    sql_of(app, who, &a.project, &b, req).map(|r| serde_json::to_value(r).expect("a plan serializes"))
                } else {
                    explain_of(app, who, &a.project, &b, req).map(|r| serde_json::to_value(r).expect("a plan serializes"))
                }
            })
        }
        _ => return Err(Fail::Rpc(INVALID_PARAMS, format!("no tool {name}"))),
    };
    Ok(match answer {
        Ok(v) => json!({ "content": [{ "type": "text", "text": serde_json::to_string_pretty(&v).unwrap_or_default() }], "structuredContent": v }),
        // What the REST route would have said, as the tool's own result,
        // so the model reads the refusal instead of a transport failure.
        Err(ApiError(status, class, message)) => json!({ "content": [{ "type": "text", "text": format!("{class} ({}): {message}", status.as_u16()) }], "isError": true }),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_origin_is_its_scheme_and_host() {
        assert_eq!(origin_of("https://app.0sql.io"), Some("https://app.0sql.io".into()));
        assert_eq!(origin_of("HTTPS://App.0sql.io:443/mcp"), Some("https://app.0sql.io".into()));
        assert_eq!(origin_of("http://127.0.0.1:3699"), Some("http://127.0.0.1:3699".into()));
        assert_eq!(origin_of("http://localhost:80/"), Some("http://localhost".into()));
        assert_eq!(origin_of("null"), None);
        assert_eq!(origin_of(""), None);
        assert_eq!(origin_of("https://"), None);
        assert_ne!(origin_of("https://app.0sql.io.evil.example"), origin_of("https://app.0sql.io"));
    }

    #[test]
    fn the_endpoint_path_and_no_other() {
        assert!(is_endpoint("/mcp"));
        assert!(is_endpoint("/mcp/"));
        assert!(!is_endpoint("/mcp/x"));
        assert!(!is_endpoint("/projects"));
    }
}
