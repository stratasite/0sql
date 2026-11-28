use crate::error::{Error, Result};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use zsql_spec::SecurityContext;

/// A bound a caller puts on a spec before any work starts. The server
/// refuses one built to be expensive (`docs/admission.md`); a local
/// caller passes [`unlimited`].
pub type ShapeCheck<'a> = &'a dyn Fn(&Value) -> Result<()>;

/// No bound at all.
pub fn unlimited(_: &Value) -> Result<()> {
    Ok(())
}

#[derive(Debug, Default, Deserialize)]
pub struct SqlRequest {
    /// A query spec, or instead `expr`, a line of the shorthand.
    #[serde(default)]
    pub spec: Option<Value>,
    #[serde(default)]
    pub expr: Option<String>,
    #[serde(default)]
    pub context: Option<SecurityContext>,
}

/// The spec a request names: given, or made from its shorthand line, in
/// which case the made spec and the item descriptions come back too.
pub fn spec_of(spec: Option<Value>, expr: Option<&str>, check: ShapeCheck) -> Result<(Value, Option<Value>, Vec<String>)> {
    match (spec, expr.map(str::trim).filter(|e| !e.is_empty())) {
        (Some(s), _) => {
            check(&s)?;
            Ok((s, None, vec![]))
        }
        (None, Some(line)) => {
            let (spec, items) = zsql_shorthand::parse_line(line).map_err(|m| Error::invalid("Shorthand", m))?;
            check(&spec)?;
            let described = zsql_shorthand::describe(&items);
            Ok((spec.clone(), Some(spec), described))
        }
        (None, None) => Err(Error::invalid("Invalid", "give a spec or an expr")),
    }
}

/// What a query needs to run: the statement and the datasource it is for.
/// `corrections` lists references that resolved by near match.
#[derive(Debug, Serialize)]
pub struct SqlResponse {
    pub sql: String,
    pub datasource: String,
    pub datasource_uid: String,
    pub adapter: String,
    /// The statement's result columns in SELECT order: the field each is,
    /// and the format and display type a client shows it with.
    pub columns: Vec<zsql_planner::columns::Column>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub corrections: Vec<zsql_spec::fuzzy::Correction>,
    /// The spec a shorthand `expr` made, and how each item was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}

/// The statement, how long planning took, each planning phase's share,
/// and the node graph the visitors shaped.
#[derive(Debug, Serialize)]
pub struct ExplainResponse {
    pub sql: String,
    pub datasource: String,
    pub datasource_uid: String,
    pub adapter: String,
    pub columns: Vec<zsql_planner::columns::Column>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub corrections: Vec<zsql_spec::fuzzy::Correction>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
    pub timings: Timings,
    pub phases: Vec<zsql_planner::Phase>,
    pub nodes: Vec<zsql_planner::ExplainNode>,
}

/// Microseconds spent: parsing and resolving the spec, planning it,
/// and both together.
#[derive(Debug, Clone, Serialize)]
pub struct Timings {
    pub parser_us: u128,
    pub plan_us: u128,
    pub total_us: u128,
}

/// A planned request before it is shaped into an answer.
pub struct Planned {
    pub plan: zsql_planner::Plan,
    pub corrections: Vec<zsql_spec::fuzzy::Correction>,
    pub timings: Timings,
    pub spec: Option<Value>,
    pub items: Vec<String>,
    /// The caller's row limit, taken off the spec before planning. `None`
    /// when they did not send one, and no default is supplied for them.
    pub limit: Option<u64>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ExploreRequest {
    #[serde(default)]
    pub spec: Option<Value>,
    #[serde(default)]
    pub expr: Option<String>,
    /// Accepted for symmetry with sql and explain; exploring resolves
    /// universes only, so no policy applies yet.
    #[serde(default)]
    pub context: Option<SecurityContext>,
    /// Keep only fields whose name, uid or synonyms contain this, or come
    /// close to it.
    #[serde(default)]
    pub q: Option<String>,
}

#[derive(Debug, Serialize)]
pub struct ExploreResponse {
    pub dimensions: Vec<Value>,
    pub measures: Vec<Value>,
    pub total_us: u128,
    /// The spec an expression line made, and how each item was read.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub spec: Option<Value>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub items: Vec<String>,
}
