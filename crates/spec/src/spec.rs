//! The query spec, the service's input: fields by uid or name, decorators
//! by type, a flat or tree filter spec, calculations by alias and formula,
//! and segments by keys, measures and criteria. [`lower`] resolves it
//! against a universe into the typed [`Query`] the planner consumes;
//! [`Query::to_spec`] is the reverse, so what comes in and what is stored
//! are one shape.
//!
//! The vocabulary is strata's `Query::Spec`, kept so specs written for
//! strata plan here unchanged; the differences are that calculations
//! listed among the projections keep their place, `hidden` is honoured, a
//! date projection is truncated only when the spec says so, `customize`
//! is a decorator type, and `limit`, `db_settings`, `hints` and a
//! segment's `pair_dedupe` are spec keys. Filters are plain: a caller who
//! lets viewers change a value resolves it before sending the spec. Hidden
//! fields resolve freely: hiding is a discovery concern, not a planning one.

use crate::expr::ParsedExpression;
use crate::model::*;
use crate::Attrs;
use regex::Regex;
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::sync::LazyLock;
use zsql_dialect::Dialect;
use zsql_semantic::{DataType, Field, FieldKind, Predicate, Universe};

#[derive(Debug, thiserror::Error)]
pub enum SpecError {
    /// No field answers to a reference.
    #[error("{0}")]
    NotFound(String),
    /// More than one field answers to a reference.
    #[error("{0}")]
    Ambiguous(String),
    /// A malformed spec: bad shape, unknown type, an unresolvable target.
    #[error("{0}")]
    Invalid(String),
    /// A well-formed spec holding a value that is not legal there: a blank
    /// alias, a decorator on the wrong kind of field, a non-numeric top-n.
    /// (The wording of these messages is still Rails'; see NOTES.md.)
    #[error("Validation failed: {0}")]
    RecordInvalid(String),
}

impl SpecError {
    /// The `class` an API error carries. Flat and 0sql's own, like the
    /// envelope classes beside it (`Unauthorized`, `ContextRequired`): what
    /// a caller switches on should not name the framework that happened to
    /// raise it first. `FieldNotFound` also stops reading like the 404
    /// `NotFound`, which is a different failure entirely.
    pub fn error_class(&self) -> &'static str {
        match self {
            SpecError::NotFound(_) => "FieldNotFound",
            SpecError::Ambiguous(_) => "FieldAmbiguous",
            SpecError::Invalid(_) => "InvalidSpec",
            SpecError::RecordInvalid(_) => "InvalidValue",
        }
    }
}

type Result<T> = std::result::Result<T, SpecError>;

fn invalid(msg: impl Into<String>) -> SpecError {
    SpecError::Invalid(msg.into())
}

/// The dialect settings a query may change: how it is planned, not how
/// its SQL is spelt. A template such as `string_literal` or `quote`
/// renders security clauses, so a query that could rewrite one could
/// rewrite those.
pub const DB_SETTINGS: &[&str] = &[
    "final_pass_measure_join_type",
    "cross_universe_measure_filtering_strategy",
    "force_group_by",
    "greedy_apply_date_filters",
    "extend_ending_date_to_last_hour_of_day",
    "apply_advanced_filtering_on_array_projections",
    "supports_array_functions",
    "supports_common_table_expressions",
    "supports_sub_queries",
    "temp_table_type",
    "temp_table_prefix",
    "week_start_day",
    "default_week_start_day",
];

fn checked_db_settings(given: Option<&Attrs>) -> Result<Attrs> {
    let Some(given) = given else { return Ok(Attrs::new()) };
    for key in given.keys() {
        if !DB_SETTINGS.contains(&key.as_str()) {
            return Err(record_invalid(format!(
                "db_settings: {key} is not a setting a query may change (one of: {})",
                DB_SETTINGS.join(", ")
            )));
        }
    }
    Ok(given.clone())
}

fn record_invalid(msg: impl Into<String>) -> SpecError {
    SpecError::RecordInvalid(msg.into())
}

// ---------------------------------------------------------------- spec types

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub classification: Option<Value>,
    /// Row limit; 5000 when absent, as in Rails.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub limit: Option<i64>,
    #[serde(default)]
    pub projections: Vec<ProjectionSpec>,
    /// Calculations kept apart from the projections; they follow them.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub calculations: Vec<ProjectionSpec>,
    /// An array (flat AND) or an `{and: [...]}`/`{or: [...]}` tree.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub segments: Vec<SegmentSpec>,
    /// Visualization settings; carried, not planned.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub view: Option<Value>,
    /// Per-query dialect overrides (`final_pass_measure_join_type`, ...).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub db_settings: Option<Attrs>,
    /// Table uids the resolver prefers to route through: the first key
    /// when it ranks the join routes, never a constraint.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub hints: Vec<String>,
}

impl Spec {
    pub fn from_value(v: Value) -> Result<Spec> {
        serde_json::from_value(v).map_err(|e| invalid(format!("malformed spec: {e}")))
    }

    pub fn from_json(text: &str) -> Result<Spec> {
        serde_json::from_str(text).map_err(|e| invalid(format!("malformed spec: {e}")))
    }

    pub fn to_value(&self) -> Value {
        serde_json::to_value(self).expect("spec serializes")
    }
}

/// A projection entry; with `calculation: true` (or listed under
/// `calculations`) it is a formula over the query's fields.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectionSpec {
    /// A uid or a name, optionally suffixed `@d`/`@m`; Rails' serializer
    /// may also emit the field as an object with a `uid`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_type: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub axis: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub order_by: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub hidden: Option<bool>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub decorators: Vec<DecoratorSpec>,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub calculation: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sql: Option<String>,
    /// A calculation's result type (decimal when absent). On a field
    /// projection Rails' serializer emits the output type; it is ignored.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    // Bookkeeping Rails' serializer emits and the spec never reads.
    #[serde(default, skip_serializing)]
    pub id: Option<Value>,
    #[serde(default, skip_serializing)]
    pub display_type: Option<Value>,
    #[serde(default, skip_serializing)]
    pub schema_type: Option<Value>,
    #[serde(default, skip_serializing)]
    pub ordinal: Option<Value>,
}

/// `{type: "truncate", grain: "month"}`: the type names the decorator, the
/// other keys are its attributes.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct DecoratorSpec {
    #[serde(rename = "type")]
    pub kind: String,
    #[serde(flatten)]
    pub attrs: Attrs,
}

/// A filter leaf. `value`/`value_end` are the spec's names; Rails'
/// serializer writes the column names `filter_value`/`filter_value_end`,
/// accepted as aliases.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct FilterLeafSpec {
    pub field: Value,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub field_type: Option<String>,
    pub predicate: String,
    #[serde(default, alias = "filter_value", skip_serializing_if = "Option::is_none")]
    pub value: Option<Value>,
    #[serde(default, alias = "filter_value_end", skip_serializing_if = "Option::is_none")]
    pub value_end: Option<Value>,
    /// For `top_n`: the ranking measure (a uid or a name), or absent to
    /// rank by row count. May also be written into the value as `"N:measure"`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub top_n_measure: Option<Value>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SegmentSpec {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// `include` (default) or `exclude`; not for expanding segments.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mode: Option<String>,
    /// Dimensions identifying the population's members: the join grain.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub keys: Vec<Value>,
    /// Measures defining membership through their fact table.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub measures: Vec<Value>,
    /// Dimensions an expanding segment carries into the query.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub expanding: Vec<ExpandingSpec>,
    /// Expanding segments: `inner` (default) or `left`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub join: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub filters: Option<Value>,
    /// Measure projections (alias or field uid) the segment constrains;
    /// absent, it constrains the whole query.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub apply_to: Vec<String>,
    /// Expanding segments: `neq` (default), `lt` or `none`.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub pair_dedupe: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExpandingSpec {
    pub field: Value,
    /// The carried column's name; `"<Field> (same <Key>)"` when absent.
    #[serde(default, rename = "as", skip_serializing_if = "Option::is_none")]
    pub alias: Option<String>,
}

/// A parsed filter spec: a flat list, or a logic tree.
#[derive(Debug, Clone)]
pub enum FilterNodeSpec {
    Leaf(FilterLeafSpec),
    Group(String, Vec<FilterNodeSpec>),
}

/// Rails: an Array is a flat AND; a Hash with a `field` key is a leaf and a
/// Hash with an `and`/`or` key is a group.
pub fn parse_filters(v: &Value) -> Result<Option<FilterNodeSpec>> {
    match v {
        Value::Null => Ok(None),
        Value::Array(items) => {
            let leaves = items.iter().map(parse_filter_leaf).collect::<Result<Vec<_>>>()?;
            Ok(Some(FilterNodeSpec::Group("and".into(), leaves)))
        }
        Value::Object(_) => Ok(Some(parse_filter_node(v)?)),
        other => Err(invalid(format!("filters must be an array or a logic tree, got {other}"))),
    }
}

fn parse_filter_leaf(v: &Value) -> Result<FilterNodeSpec> {
    let m = v.as_object().ok_or_else(|| invalid(format!("a filter must be an object, got {v}")))?;
    if !m.contains_key("field") {
        return Err(invalid(
            "Filters here are a flat AND list — each entry must name a field; and:/or: groups are not supported \
             in this position. For membership through any of several facts (e.g. bought in store OR catalog), \
             list those measures in the segment's `measures` instead.",
        ));
    }
    let leaf: FilterLeafSpec = serde_json::from_value(v.clone()).map_err(|e| invalid(format!("malformed filter: {e}")))?;
    Ok(FilterNodeSpec::Leaf(leaf))
}

fn parse_filter_node(v: &Value) -> Result<FilterNodeSpec> {
    let m = v.as_object().ok_or_else(|| invalid(format!("a filter node must be an object, got {v}")))?;
    if m.contains_key("field") {
        return parse_filter_leaf(v);
    }
    let (op, kids) = m.iter().next().ok_or_else(|| invalid("an empty filter node"))?;
    if op != "and" && op != "or" {
        return Err(invalid(format!("a filter node needs a field or an and/or key, got {op}")));
    }
    let kids = kids.as_array().ok_or_else(|| invalid(format!("{op} must hold a list of filters")))?;
    Ok(FilterNodeSpec::Group(op.clone(), kids.iter().map(parse_filter_node).collect::<Result<Vec<_>>>()?))
}

// ------------------------------------------------------------------ resolver

/// Rails `Branch::Store#field_by_ref!`: the field a reference names in any
/// spelling the outside world writes. A uid or a name, matched without
/// regard to case; an `@d`/`@m` suffix (or the longer `@dim`, `@measure`)
/// or an explicit `kind` picks between a dimension and a measure that
/// share the spelling. Hidden fields resolve like any other.
pub fn resolve_field<'u>(universe: &'u Universe, reference: &str, kind: Option<&str>) -> Result<&'u Field> {
    let reference = reference.trim();
    let (base, suffix) = match reference.split_once('@') {
        Some((b, s)) => (b, Some(s)),
        None => (reference, None),
    };
    let kind = kind_hint(kind).or_else(|| kind_hint(suffix));
    let kinds: Vec<FieldKind> = match kind {
        Some(k) => vec![k],
        None => vec![FieldKind::Dimension, FieldKind::Measure],
    };
    let mut matches: Vec<&Field> = kinds.iter().filter_map(|k| universe.field_by_uid(base, Some(*k))).collect();
    if matches.is_empty() {
        let wanted = base.trim().to_lowercase();
        matches = universe.fields().iter().filter(|f| kinds.contains(&f.kind) && f.name.trim().to_lowercase() == wanted).collect();
    }
    // A synonym the modeler wrote resolves like the name itself.
    if matches.is_empty() {
        let wanted = base.trim().to_lowercase();
        matches = universe
            .fields()
            .iter()
            .filter(|f| kinds.contains(&f.kind) && f.synonyms.iter().any(|s| s.trim().to_lowercase() == wanted))
            .collect();
    }
    match matches.len() {
        0 => Err(SpecError::NotFound(format!("No field named '{base}' in this model."))),
        1 => Ok(matches[0]),
        _ => Err(SpecError::Ambiguous(format!(
            "Field '{base}' is ambiguous — more than one field answers to it. Use {base}@d or {base}@m to pick the dimension or the measure."
        ))),
    }
}

/// Rails' type spellings: `Semantic::Dimension`, `dimension`, `d`, `dim`
/// (anything starting with a d) is a dimension; any other non-empty type
/// is a measure.
fn kind_hint(s: Option<&str>) -> Option<FieldKind> {
    let s = s?.trim();
    if s.is_empty() {
        return None;
    }
    let last = s.rsplit([':', '@']).next().unwrap_or(s).trim().to_lowercase();
    if last.is_empty() {
        None
    } else if last.starts_with('d') {
        Some(FieldKind::Dimension)
    } else {
        Some(FieldKind::Measure)
    }
}

/// A field reference as a spec writes it: a string, or an object with a
/// `uid` (Rails serializes a filter's field that way).
fn ref_text(v: &Value) -> Result<String> {
    match v {
        Value::String(s) => Ok(s.clone()),
        Value::Object(m) => m
            .get("uid")
            .and_then(Value::as_str)
            .map(str::to_string)
            .ok_or_else(|| invalid(format!("a field reference must be a uid or a name, got {v}"))),
        other => Err(invalid(format!("a field reference must be a uid or a name, got {other}"))),
    }
}

// ------------------------------------------------------------------ lowering

struct Cx<'u> {
    universe: &'u Universe,
    reserved: Vec<String>,
    aggregates: Vec<String>,
    corrections: std::cell::RefCell<Vec<crate::fuzzy::Correction>>,
}

impl<'u> Cx<'u> {
    /// [`resolve_field`], then the nearest field by trigram similarity when
    /// nothing answers exactly. A near match is recorded as a correction;
    /// an unclear one fails naming the closest fields.
    fn resolve(&self, reference: &str, kind: Option<&str>) -> Result<&'u Field> {
        match resolve_field(self.universe, reference, kind) {
            Err(SpecError::NotFound(msg)) => {
                let (base, suffix) = reference.trim().split_once('@').map(|(b, s)| (b, Some(s))).unwrap_or((reference.trim(), None));
                let kinds: Vec<FieldKind> = match kind_hint(kind).or_else(|| kind_hint(suffix)) {
                    Some(k) => vec![k],
                    None => vec![FieldKind::Dimension, FieldKind::Measure],
                };
                let ranked = crate::fuzzy::suggest(self.universe, base, &kinds);
                match crate::fuzzy::pick(&ranked) {
                    Some(best) => {
                        self.corrections.borrow_mut().push(crate::fuzzy::Correction {
                            term: base.trim().to_string(),
                            field_uid: best.field.uid.clone(),
                            field_name: best.field.name.clone(),
                            score: (best.score * 100.0).round() / 100.0,
                        });
                        Ok(best.field)
                    }
                    None => Err(SpecError::NotFound(format!("{msg}{}", crate::fuzzy::did_you_mean(&ranked)))),
                }
            }
            other => other,
        }
    }
}

/// Lowers a spec into the typed query the planner consumes. `dialect` is
/// the base dialect (`Dialects::base`): its reserved keywords and aggregate
/// functions parse each calculation's formula, as Rails' defaults do.
pub fn lower(spec: &Spec, universe: &Universe, dialect: &Dialect) -> Result<Query> {
    let cx =
        Cx { universe, reserved: dialect.reserved_keywords(), aggregates: dialect.aggregate_functions(), corrections: Default::default() };

    // Projections in listed order, calculations in place, then the
    // separate calculations list; formulas resolve against every field
    // projection, so those come first.
    let entries: Vec<(&ProjectionSpec, bool)> =
        spec.projections.iter().map(|p| (p, p.calculation)).chain(spec.calculations.iter().map(|p| (p, true))).collect();
    let mut projections: Vec<Projection> = Vec::with_capacity(entries.len());
    for (i, (p, is_calc)) in entries.iter().enumerate() {
        if *is_calc {
            continue;
        }
        projections.push(lower_field_projection(&cx, p, format!("p{i}"), i as i64)?);
    }
    for (i, (p, is_calc)) in entries.iter().enumerate() {
        if !*is_calc {
            continue;
        }
        let calc = lower_calculation(&cx, p, format!("p{i}"), i as i64, &projections)?;
        let at = projections.iter().position(|q| q.ordinal > Some(i as i64)).unwrap_or(projections.len());
        projections.insert(at, calc);
    }

    // Rails `Decorator#cleanup_partition_and_order_by`, resolved rather
    // than pruned: a window's or contribution's partition and order refs
    // name projected dimensions, by uid, name or alias, and are rewritten
    // to the uid. One that names no projected dimension is an error, where
    // Rails dropped it and planned a different query without a word.
    let dims: Vec<(String, String, String)> = projections
        .iter()
        .filter(|p| p.field_kind == Some(FieldKind::Dimension) && !p.is_calculation())
        .filter_map(|p| {
            let uid = p.field_uid.clone()?;
            let name = cx.universe.field_by_uid(&uid, Some(FieldKind::Dimension)).map(|f| f.name.to_lowercase()).unwrap_or_default();
            Some((uid, name, display_alias(&cx, p).trim().to_lowercase()))
        })
        .collect();
    let measures: Vec<(String, String, String)> = projections
        .iter()
        .filter(|p| p.field_kind == Some(FieldKind::Measure) || p.is_calculation())
        .map(|p| {
            let uid = p.field_uid.clone().unwrap_or_default().to_lowercase();
            let name = p
                .field_uid
                .as_deref()
                .and_then(|u| cx.universe.field_by_uid(u, Some(FieldKind::Measure)))
                .map(|f| f.name.to_lowercase())
                .unwrap_or_default();
            (uid, name, display_alias(&cx, p).trim().to_lowercase())
        })
        .collect();
    let owners: Vec<String> = projections.iter().map(|p| display_alias(&cx, p)).collect();
    for (p, owner) in projections.iter_mut().zip(owners) {
        for d in p.decorators.iter_mut() {
            let label = decorator_type_key(d.kind);
            let resolve = |r: &str| -> Result<String> {
                let want = r.trim().to_lowercase();
                dims.iter()
                    .find(|(uid, name, alias)| uid.to_lowercase() == want || *name == want || *alias == want)
                    .map(|(uid, _, _)| uid.clone())
                    .ok_or_else(|| {
                        if measures.iter().any(|(uid, name, alias)| (!uid.is_empty() && *uid == want) || *name == want || *alias == want) {
                            record_invalid(format!(
                                "{label} on {owner}: '{}' is a measure; a window partitions and orders only by a projected dimension",
                                r.trim()
                            ))
                        } else {
                            record_invalid(format!(
                                "{label} on {owner}: '{}' is not a projected dimension; partition and order refs name a dimension the query projects",
                                r.trim()
                            ))
                        }
                    })
            };
            if let Some(Value::Array(refs)) = d.attrs.get_mut("partition_refs") {
                // `-1` (auto partition) may be written as a number or a string.
                let mut resolved = Vec::with_capacity(refs.len());
                for r in refs.iter() {
                    match r {
                        Value::String(s) if s == "-1" => resolved.push(r.clone()),
                        Value::Number(n) if n.as_i64() == Some(-1) => resolved.push(r.clone()),
                        Value::String(s) => resolved.push(Value::String(resolve(s)?)),
                        other => {
                            return Err(invalid(format!("{label} on {owner}: partition_refs holds {other}, not a dimension reference")))
                        }
                    }
                }
                *refs = resolved;
            }
            if let Some(Value::Object(order)) = d.attrs.get_mut("order_by") {
                let mut resolved = serde_json::Map::new();
                for (k, v) in order.iter() {
                    let key = if k == "-1" { k.clone() } else { resolve(k)? };
                    resolved.insert(key, v.clone());
                }
                *order = resolved;
            }
        }
    }

    let (filters, filter_tree) = lower_filters(&cx, spec.filters.as_ref(), "")?;

    let mut segments = Vec::new();
    for (i, s) in spec.segments.iter().enumerate() {
        segments.push(lower_segment(&cx, s, i, &mut projections)?);
    }

    Ok(Query {
        corrections: cx.corrections.take(),
        population: false,
        name: spec.name.clone(),
        // No default: a limit the caller did not ask for is not ours to
        // invent, and the server emits one only when it is here.
        limit: spec.limit,
        db_settings: checked_db_settings(spec.db_settings.as_ref())?,
        hints: spec.hints.clone(),
        filter_tree,
        projections,
        filters,
        segments,
    })
}

const AXES: [&str; 6] = ["row", "x", "y", "series", "tip", "pivot"];

fn lower_field_projection(cx: &Cx, p: &ProjectionSpec, reference: String, ordinal: i64) -> Result<Projection> {
    let source = p.field.as_ref().map(ref_text).transpose()?.ok_or_else(|| invalid(format!("projection {} needs a field", ordinal + 1)))?;
    let field = cx.resolve(&source, p.field_type.as_deref())?;
    let axis = p.axis.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| "row".into());
    if !AXES.contains(&axis.as_str()) {
        return Err(invalid(format!("'{axis}' is not a projection axis (row, x, y, series, tip, pivot)")));
    }
    if axis == "tip" && field.kind == FieldKind::Dimension {
        return Err(record_invalid("Axis the tooltip holds measures only"));
    }
    let order_by = lower_order_by(p.order_by.as_deref())?;

    // A date projection without a truncate decorator is the raw column.
    // (Rails' UI path adds a month truncator on creation; the spec is
    // explicit instead, so what is written is what is planned.)
    let mut decorators: Vec<Decorator> = Vec::new();
    for d in &p.decorators {
        let dec = lower_decorator(d, Some(field), &source)?;
        match decorators.iter_mut().find(|e| e.kind == dec.kind) {
            Some(existing) => existing.attrs.extend(dec.attrs),
            None => decorators.push(dec),
        }
    }

    let projection = Projection {
        reference,
        kind: ProjectionKind::Field,
        field_uid: Some(field.uid.clone()),
        field_kind: Some(field.kind),
        alias: p.alias.as_deref().map(str::trim).filter(|a| !a.is_empty()).map(str::to_string),
        ordinal: Some(ordinal),
        axis,
        hidden: p.hidden.unwrap_or(false),
        order_by,
        sql: None,
        calculation_field_type: None,
        calculation_data_type: None,
        dimension_refs: vec![],
        measure_refs: vec![],
        projection_refs: vec![],
        format: p.format.clone().filter(|v| !v.is_null()),
        decorators,
        parent: None,
        is_static: false,
        source_ref: Some(source),
    };
    Ok(projection)
}

const CALC_DATA_TYPES: [&str; 8] = ["string", "integer", "decimal", "date", "date_time", "boolean", "bigint", "binary"];

fn lower_calculation(cx: &Cx, p: &ProjectionSpec, reference: String, ordinal: i64, peers: &[Projection]) -> Result<Projection> {
    let alias = p.alias.as_deref().map(str::trim).filter(|a| !a.is_empty()).ok_or_else(|| record_invalid("Alias can't be blank"))?;
    let sql = p
        .sql
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| record_invalid(format!("Sql can't be blank (calculation '{alias}')")))?;
    // Rails normalizes the brackets: `[ Name ]` is `[Name]`.
    static OPEN: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\[\s+").unwrap());
    static CLOSE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"\s+\]").unwrap());
    let sql = OPEN.replace_all(sql, "[");
    let sql = CLOSE.replace_all(&sql, "]").into_owned();
    let data_type = p.data_type.clone().filter(|d| !d.is_empty()).unwrap_or_else(|| "decimal".into());
    if !CALC_DATA_TYPES.contains(&data_type.as_str()) {
        return Err(record_invalid(format!("Calculation data type is not included in the list ('{data_type}')")));
    }
    let axis = p.axis.clone().filter(|a| !a.is_empty()).unwrap_or_else(|| "row".into());
    if !AXES.contains(&axis.as_str()) {
        return Err(invalid(format!("'{axis}' is not a projection axis (row, x, y, series, tip, pivot)")));
    }
    // Rails parses a calculation with its default (base) keyword and
    // aggregate lists and ANSI quoting.
    let parsed = ParsedExpression::parse(&sql, &cx.reserved, &cx.aggregates, "\"@exp\"");
    if !parsed.columns.is_empty() {
        return Err(record_invalid(format!("Sql columns like {} are not permitted in calculations.", parsed.columns.join(","))));
    }
    let field_type = if parsed.has_aggregate_functions() { "measure" } else { "dimension" };

    // `[Name]@m` and `[Name]@d` name fields; one that misses resolves by
    // near match, and the formula is rewritten to the field's name since
    // the planner substitutes references by name.
    let mut sql = sql;
    let mut measure_refs = Vec::new();
    for name in &parsed.measure_refs {
        let f = match cx.universe.field_by_name(name, FieldKind::Measure) {
            Some(f) => f,
            None => {
                let f = cx.resolve(name, Some("measure")).map_err(|e| {
                    record_invalid(format!("Measure refs could not find measure named: {name}.{}", did_you_mean_suffix(&e)))
                })?;
                sql = rename_ref(&sql, name, &f.name);
                f
            }
        };
        measure_refs.push(f.uid.clone());
    }
    let mut dimension_refs = Vec::new();
    for name in &parsed.dimension_refs {
        let f = match cx.universe.field_by_name(name, FieldKind::Dimension) {
            Some(f) => f,
            None => {
                let f = cx.resolve(name, Some("dimension")).map_err(|e| {
                    record_invalid(format!("Dimension refs could not find dimension named: {name}.{}", did_you_mean_suffix(&e)))
                })?;
                sql = rename_ref(&sql, name, &f.name);
                f
            }
        };
        dimension_refs.push(f.uid.clone());
    }
    // `[Name]` with no suffix: another projection of this query first,
    // then a field. An alias wins because it is the nearer thing and the
    // one the author just wrote; `@d`/`@m` is how to insist on the field
    // when a projection happens to share its name.
    let mut projection_refs = Vec::new();
    for name in &parsed.field_refs {
        if peers.iter().any(|q| alias_matches(cx, q, name)) {
            // The parser lower-cases the formula, so the reference is lower-case.
            projection_refs.push(name.clone());
            continue;
        }
        let dim = cx.universe.field_by_name(name, FieldKind::Dimension);
        let msr = cx.universe.field_by_name(name, FieldKind::Measure);
        // A name belongs to at most one dimension and at most one
        // measure, so the only ambiguity is a name that is both.
        match (dim, msr) {
            (Some(d), Some(_)) => {
                // The parser lower-cases the formula, so the name is
                // echoed back from the field, not from the reference.
                let n = &d.name;
                return Err(record_invalid(format!(
                    "Sql [{n}] is both a dimension and a measure. Write [{n}]@d or [{n}]@m to say which."
                )));
            }
            (Some(f), None) => {
                sql = rename_ref(&sql, name, &f.name);
                dimension_refs.push(f.uid.clone());
            }
            (None, Some(f)) => {
                sql = rename_ref(&sql, name, &f.name);
                measure_refs.push(f.uid.clone());
            }
            (None, None) => {
                return Err(record_invalid(format!(
                    "Sql could not find a projection or a field named {name}. If you meant a measure or a dimension you can say so with @m or @d: [My Measure]@m"
                )))
            }
        }
    }

    let mut decorators = Vec::new();
    for d in &p.decorators {
        decorators.push(lower_decorator(d, None, alias)?);
    }

    Ok(Projection {
        reference,
        kind: ProjectionKind::Calculation,
        field_uid: None,
        field_kind: None,
        alias: Some(alias.to_string()),
        ordinal: Some(ordinal),
        axis,
        hidden: p.hidden.unwrap_or(false),
        order_by: lower_order_by(p.order_by.as_deref())?,
        sql: Some(sql),
        calculation_field_type: Some(field_type.into()),
        calculation_data_type: Some(data_type),
        dimension_refs,
        measure_refs,
        projection_refs,
        format: p.format.clone().filter(|v| !v.is_null()),
        decorators,
        parent: None,
        is_static: false,
        source_ref: None,
    })
}

fn lower_order_by(s: Option<&str>) -> Result<Option<OrderBy>> {
    match s.map(|s| s.trim().to_lowercase()).as_deref() {
        None | Some("") => Ok(None),
        Some("asc") => Ok(Some(OrderBy::Asc)),
        Some("desc") => Ok(Some(OrderBy::Desc)),
        Some(o) => Err(invalid(format!("'{o}' is not an order_by (asc, desc)"))),
    }
}

// ---- decorators

const GRAINS: [&str; 10] = ["raw", "millisecond", "second", "minute", "hour", "day", "week", "month", "quarter", "year"];
const EXTRACTS: [&str; 12] = [
    "minute",
    "hour",
    "day_of_month",
    "day_of_week",
    "day_of_year",
    "week_of_year",
    "month",
    "quarter",
    "year",
    "day_name",
    "month_name",
    "year_month",
];
const WINDOW_FUNCTIONS: [&str; 15] = [
    "avg",
    "sum",
    "min",
    "max",
    "count",
    "row_number",
    "rank",
    "dense_rank",
    "percent_rank",
    "cume_dist",
    "ntile",
    "lag",
    "lead",
    "first_value",
    "last_value",
];
const TRANSFORMS: [&str; 5] = ["year_over_year", "quarter_over_quarter", "month_over_month", "week_over_week", "day_over_day"];
/// `["a", "b", "c"]` read out as `a, b or c`: what an unknown enum value's
/// message offers in place of the value it got.
fn one_of(allowed: &[&str]) -> String {
    match allowed.split_last() {
        Some((last, [])) => (*last).to_string(),
        Some((last, rest)) => format!("{} or {last}", rest.join(", ")),
        None => String::new(),
    }
}

/// Bookkeeping columns Rails' serializer emits on a decorator.
const DECORATOR_NOISE: [&str; 8] =
    ["id", "projection_id", "prompt", "share_full_result_set_with_ai", "created_at", "updated_at", "created_by_id", "updated_by_id"];

fn decorator_class(kind: DecoratorKind) -> &'static str {
    match kind {
        DecoratorKind::Truncate => "Query::Decorators::DateTruncator",
        DecoratorKind::Extract => "Query::Decorators::DateExtractor",
        DecoratorKind::Window => "Query::Decorators::Windower",
        DecoratorKind::Contribute => "Query::Decorators::Contributor",
        DecoratorKind::Temporalize => "Query::Decorators::Temporalizer",
        DecoratorKind::Customize => "Query::Decorators::Customizer",
    }
}

pub fn decorator_type_key(kind: DecoratorKind) -> &'static str {
    match kind {
        DecoratorKind::Truncate => "truncate",
        DecoratorKind::Extract => "extract",
        DecoratorKind::Window => "window",
        DecoratorKind::Contribute => "contribute",
        DecoratorKind::Temporalize => "temporalize",
        DecoratorKind::Customize => "customize",
    }
}

fn decorator_kind(name: &str) -> Result<DecoratorKind> {
    Ok(match name.trim() {
        "truncate" | "Query::Decorators::DateTruncator" => DecoratorKind::Truncate,
        "extract" | "Query::Decorators::DateExtractor" => DecoratorKind::Extract,
        "window" | "Query::Decorators::Windower" => DecoratorKind::Window,
        "contribute" | "Query::Decorators::Contributor" => DecoratorKind::Contribute,
        "temporalize" | "Query::Decorators::Temporalizer" => DecoratorKind::Temporalize,
        "customize" | "custom" | "Query::Decorators::Customizer" => DecoratorKind::Customize,
        other => return Err(invalid(format!("Unknown decorator type: {other}"))),
    })
}

/// The decorator row as Rails stores it: every column at its default, so
/// a lowered decorator carries the same attribute set as a loaded one and
/// column identities agree.
pub fn decorator_defaults(kind: DecoratorKind) -> Attrs {
    let mut a = Attrs::new();
    a.insert("abbreviate".into(), json!(true));
    a.insert("buckets".into(), json!(4));
    a.insert("custom_sql".into(), Value::Null);
    a.insert("extract".into(), Value::Null);
    a.insert("function".into(), Value::Null);
    a.insert("grain".into(), Value::Null);
    a.insert("ignore_partition_filters".into(), json!(true));
    a.insert("mode".into(), Value::Null);
    a.insert("offset".into(), json!(0));
    a.insert("order_by".into(), if kind == DecoratorKind::Window { json!({"-1": "asc"}) } else { Value::Null });
    a.insert("partition_refs".into(), Value::Null);
    a.insert("percent_change".into(), Value::Null);
    a.insert("prompt".into(), Value::Null);
    a.insert("share_full_result_set_with_ai".into(), Value::Null);
    a.insert("size".into(), json!(7));
    a.insert("transform".into(), Value::Null);
    a.insert("type".into(), json!(decorator_class(kind)));
    a
}

fn lower_decorator(d: &DecoratorSpec, field: Option<&Field>, owner: &str) -> Result<Decorator> {
    let kind = decorator_kind(&d.kind)?;
    let mut attrs = decorator_defaults(kind);
    for (k, v) in &d.attrs {
        if DECORATOR_NOISE.contains(&k.as_str()) || k == "type" {
            continue;
        }
        if !attrs.contains_key(k) {
            return Err(invalid(format!("unknown attribute '{k}' on the {} decorator of {owner}", d.kind)));
        }
        attrs.insert(k.clone(), v.clone());
    }
    let label = d.kind.as_str();
    // Absent is the caller's `ok_or_else`; set but not in `allowed` is this
    // error, which names the value it got and every value that would do.
    let choice = |key: &str, allowed: &[&str]| -> Result<Option<String>> {
        match attrs.get(key) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) if allowed.contains(&s.to_lowercase().as_str()) => Ok(Some(s.to_lowercase())),
            Some(other) => Err(record_invalid(format!("unknown {key} {other}; use {} ({label} on {owner})", one_of(allowed)))),
        }
    };
    if let Some(f) = field {
        let dimension_only = matches!(kind, DecoratorKind::Truncate | DecoratorKind::Extract | DecoratorKind::Customize);
        if dimension_only && f.kind != FieldKind::Dimension {
            return Err(record_invalid(format!("{label} can only be applied to dimensions ({owner})")));
        }
        if !dimension_only && f.kind != FieldKind::Measure {
            return Err(record_invalid(format!("{label} can only be applied to measures ({owner})")));
        }
        if kind == DecoratorKind::Truncate && !f.is_date() {
            return Err(record_invalid(format!("only date/datetime types can be date truncated ({owner})")));
        }
        if kind == DecoratorKind::Extract && !f.is_date() {
            return Err(record_invalid(format!("only date/datetime types can be date extracted ({owner})")));
        }
    }
    match kind {
        DecoratorKind::Truncate => {
            let grain = choice("grain", &GRAINS)?.ok_or_else(|| record_invalid(format!("Grain must be set ({owner})")))?;
            attrs.insert("grain".into(), json!(grain));
        }
        DecoratorKind::Extract => {
            let e = choice("extract", &EXTRACTS)?.ok_or_else(|| record_invalid(format!("Extract must be set ({owner})")))?;
            attrs.insert("extract".into(), json!(e));
        }
        DecoratorKind::Window => {
            let mode = choice("mode", &["moving", "running"])?
                .ok_or_else(|| record_invalid(format!("Mode must be moving or running ({owner})")))?;
            let func = choice("function", &WINDOW_FUNCTIONS)?
                .ok_or_else(|| record_invalid(format!("Function must be a valid window function ({owner})")))?;
            attrs.insert("mode".into(), json!(mode));
            attrs.insert("function".into(), json!(func));
            for (key, needed) in
                [("size", mode == "moving"), ("offset", matches!(func.as_str(), "lag" | "lead")), ("buckets", func == "ntile")]
            {
                let n = attrs.get(key).and_then(Value::as_i64);
                if needed && n.is_none_or(|n| n < 1) {
                    return Err(record_invalid(format!("{key} must be greater than or equal to 1 ({owner})")));
                }
            }
        }
        DecoratorKind::Temporalize => {
            let t =
                choice("transform", &TRANSFORMS)?.ok_or_else(|| record_invalid(format!("Transform invalid transform type ({owner})")))?;
            attrs.insert("transform".into(), json!(t));
        }
        // Not in Rails' spec vocabulary (its DECORATOR_MAP has no customize
        // key), accepted here so a stored Customizer can serialize and re-apply.
        DecoratorKind::Customize => {
            let sql = attrs.get("custom_sql").and_then(Value::as_str).map(|s| s.trim().to_lowercase()).filter(|s| !s.is_empty());
            let sql = sql.ok_or_else(|| record_invalid(format!("Custom sql can't be blank ({owner})")))?;
            if !sql.contains("@expression") {
                return Err(record_invalid(format!("Custom sql must include @expression ({owner})")));
            }
            static AGGREGATE: LazyLock<Regex> = LazyLock::new(|| Regex::new(r"(avg|sum|min|max|count)\(").unwrap());
            if AGGREGATE.is_match(&sql) {
                return Err(record_invalid(format!("Custom sql must not include aggregation function ({owner})")));
            }
            attrs.insert("custom_sql".into(), json!(sql));
        }
        DecoratorKind::Contribute => {}
    }
    Ok(Decorator { kind, attrs })
}

// ---- aliases

/// Rails `Projection#alias`: the explicit alias, else the one the highest
/// priority decorator lends, else the field's name.
fn derived_alias(p: &Projection, field_name: Option<&str>) -> String {
    if let Some(a) = &p.alias {
        return a.clone();
    }
    let name = field_name.unwrap_or("");
    let mut lent: Vec<(i32, String)> = p.decorators.iter().filter_map(|d| d.alias(name).map(|a| (d.priority(), a))).collect();
    if !lent.is_empty() {
        lent.sort_by_key(|(pr, _)| *pr);
        return lent.pop().unwrap().1;
    }
    name.to_string()
}

/// Rails `String#humanize` for a predicate name: `top_n` reads `Top n`.
fn humanize(s: &str) -> String {
    let mut out = s.replace('_', " ");
    if let Some(first) = out.get(..1) {
        let upper = first.to_uppercase();
        out.replace_range(..1, &upper);
    }
    out
}

fn display_alias(cx: &Cx, p: &Projection) -> String {
    let name = p.field_uid.as_deref().and_then(|u| cx.universe.field_by_uid(u, p.field_kind)).map(|f| f.name.as_str());
    derived_alias(p, name)
}

fn alias_matches(cx: &Cx, p: &Projection, other: &str) -> bool {
    display_alias(cx, p).trim().to_lowercase() == other.trim().to_lowercase()
}

// ---- filters

fn lower_filters(cx: &Cx, spec: Option<&Value>, prefix: &str) -> Result<(Vec<Filter>, Option<Tree>)> {
    let Some(node) = spec.map(parse_filters).transpose()?.flatten() else { return Ok((vec![], None)) };
    let flat = matches!(&node, FilterNodeSpec::Group(op, kids) if op == "and" && kids.iter().all(|k| matches!(k, FilterNodeSpec::Leaf(_))));
    let mut filters = Vec::new();
    let tree = lower_filter_node(cx, &node, prefix, &mut filters)?;
    Ok((filters, if flat { None } else { Some(tree) }))
}

fn lower_filter_node(cx: &Cx, node: &FilterNodeSpec, prefix: &str, out: &mut Vec<Filter>) -> Result<Tree> {
    match node {
        FilterNodeSpec::Leaf(leaf) => {
            let f = lower_filter(cx, leaf, format!("{prefix}f{}", out.len()), out.len() as i64)?;
            let r = f.reference.clone();
            out.push(f);
            Ok(Tree::Leaf(r))
        }
        FilterNodeSpec::Group(op, kids) => {
            let kids = kids.iter().map(|k| lower_filter_node(cx, k, prefix, out)).collect::<Result<Vec<_>>>()?;
            Ok(Tree::Group(op.clone(), kids))
        }
    }
}

/// A filter value as text; Rails strips it.
fn scalar_text(v: Option<&Value>) -> Result<Option<String>> {
    Ok(match v {
        None | Some(Value::Null) => None,
        Some(Value::String(s)) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        Some(Value::Number(n)) => Some(n.to_string()),
        Some(Value::Bool(b)) => Some(b.to_string()),
        Some(other) => return Err(invalid(format!("a filter value must be a scalar, got {other}"))),
    })
}

fn lower_filter(cx: &Cx, leaf: &FilterLeafSpec, reference: String, ordinal: i64) -> Result<Filter> {
    let source = ref_text(&leaf.field)?;
    let field = cx.resolve(&source, leaf.field_type.as_deref())?;
    let predicate: Predicate = serde_json::from_value(Value::String(leaf.predicate.trim().to_lowercase()))
        .map_err(|_| invalid(format!("'{}' is not a predicate (filter on {source})", leaf.predicate)))?;
    if !predicate_allowed(field, predicate) {
        return Err(record_invalid(format!("Predicate {} predicate is not supported for {}.", humanize(predicate.name()), field.name)));
    }
    if predicate == Predicate::TopN && field.kind != FieldKind::Dimension {
        return Err(record_invalid(format!("Predicate top n can only be applied to a dimension field ({source})")));
    }

    let mut value = scalar_text(leaf.value.as_ref())?;
    let value_end = scalar_text(leaf.value_end.as_ref())?;
    // top_n's shorthand: "N:measure".
    let mut top_n_measure = match &leaf.top_n_measure {
        None | Some(Value::Null) => None,
        Some(v) => Some(ref_text(v)?),
    };
    if predicate == Predicate::TopN {
        if let Some((count, measure)) = value.as_deref().and_then(|v| v.split_once(':')) {
            let (count, measure) = (count.to_string(), measure.to_string());
            value = Some(count);
            if top_n_measure.is_none() {
                top_n_measure = Some(measure);
            }
        }
    }
    let top_n_measure = match top_n_measure {
        Some(m) => Some(cx.resolve(&m, Some("measure"))?.uid.clone()),
        None => None,
    };

    let value_required = !matches!(predicate, Predicate::IsNull | Predicate::IsNotNull);
    if value_required && value.is_none() {
        return Err(record_invalid(format!("Filter value can't be blank ({} on {source})", predicate.name())));
    }
    if predicate == Predicate::Between && value_end.is_none() {
        return Err(record_invalid(format!("Filter value end can't be blank (between on {source})")));
    }
    // Rails `numericality_of_filter_values`.
    let numeric = |s: &str| s.trim().parse::<f64>().is_ok();
    let number_field = matches!(field.data_type, Some(DataType::Integer | DataType::Decimal | DataType::Bigint));
    if value_required {
        if let Some(v) = &value {
            if number_field && matches!(predicate, Predicate::InList | Predicate::ExcludeList) && v.split(',').any(|x| !numeric(x)) {
                return Err(record_invalid(format!("Filter value values must be numeric ({source})")));
            }
            if predicate == Predicate::TopN && !numeric(v) {
                return Err(record_invalid(format!("Filter value value must be numeric ({source})")));
            }
            if number_field
                && !matches!(
                    predicate,
                    Predicate::InList | Predicate::ExcludeList | Predicate::TopN | Predicate::Custom | Predicate::Keyword
                )
                && !numeric(v)
            {
                return Err(record_invalid(format!("Filter value is not a number ({source})")));
            }
        }
    }

    Ok(Filter {
        reference,
        field_uid: field.uid.clone(),
        field_kind: Some(field.kind),
        predicate,
        filter_value: value,
        filter_value_end: value_end,
        top_n_measure,
        ordinal,
        source_ref: Some(source),
    })
}

/// Rails `Predicatable#valid_predicates`, by the field's type.
fn predicate_allowed(field: &Field, p: Predicate) -> bool {
    use Predicate::*;
    let equality_or_null = matches!(p, Equals | DoesNotEqual | IsNull | IsNotNull);
    let list = matches!(p, TopN | InList | ExcludeList);
    let like = matches!(p, StartsWith | Contains | EndsWith | DoesNotContain | DoesNotStartWith | DoesNotEndWith);
    let range = matches!(p, Between | GreaterThan | GreaterThanOrEqualTo | LessThan | LessThanOrEqualTo);
    let number = matches!(field.data_type, Some(DataType::Integer | DataType::Decimal | DataType::Bigint));
    let date = matches!(field.data_type, Some(DataType::Date | DataType::DateTime));
    if p == Custom || equality_or_null {
        return true;
    }
    match field.data_type {
        Some(DataType::String) => list || like || p == Keyword,
        _ if number && field.kind == FieldKind::Dimension => list || range,
        _ if number => range,
        _ if date => range,
        _ => false,
    }
}

// ---- segments

fn lower_segment(cx: &Cx, s: &SegmentSpec, index: usize, query_projections: &mut Vec<Projection>) -> Result<Segment> {
    let name = s.name.clone().filter(|n| !n.trim().is_empty()).unwrap_or_else(|| format!("Segment {}", index + 1));
    let reference = format!("s{index}");
    let expanding = !s.expanding.is_empty();
    if s.keys.is_empty() && s.measures.is_empty() && s.filters.is_none() && !expanding {
        return Err(invalid(format!(
            "Segment '{name}' has no definition — a stateless spec cannot adopt a stored segment by name; give its keys, measures and filters."
        )));
    }
    if s.keys.is_empty() {
        return Err(invalid(format!(
            "Segment '{name}' needs at least one key dimension — list the member-identifying dimension UIDs under keys (e.g. keys: [\"customer-id\"])."
        )));
    }

    // Grain: keys as dimension projections, measures as measure projections.
    let mut projections = Vec::new();
    for k in &s.keys {
        let text = ref_text(k)?;
        let f = cx.resolve(&text, None).or_else(|e| match e {
            SpecError::Ambiguous(_) => cx.resolve(&text, Some("dimension")),
            e => Err(e),
        })?;
        if f.kind != FieldKind::Dimension {
            return Err(invalid(format!("Segment key '{text}' is a measure — list it under measures instead")));
        }
        let ordinal = projections.len() as i64;
        projections.push(segment_projection(format!("{reference}.p{ordinal}"), f, ordinal, ProjectionKind::Field, text));
    }
    for m in &s.measures {
        let text = ref_text(m)?;
        let f = cx.resolve(&text, None).or_else(|e| match e {
            SpecError::Ambiguous(_) => cx.resolve(&text, Some("measure")),
            e => Err(e),
        })?;
        if f.kind != FieldKind::Measure {
            return Err(invalid(format!("Segment measure '{text}' is a dimension — list it under keys instead")));
        }
        let ordinal = projections.len() as i64;
        projections.push(segment_projection(format!("{reference}.p{ordinal}"), f, ordinal, ProjectionKind::Field, text));
    }

    let mut links = Vec::new();
    let mut pair_dedupe = PairDedupe::Neq;
    if expanding {
        if s.mode.is_some() {
            return Err(invalid(format!("Segment '{name}' is expanding — use join: inner|left instead of mode")));
        }
        if !s.apply_to.is_empty() {
            return Err(invalid(format!(
                "Segment '{name}' is expanding — apply_to is not supported; it always constrains the whole query"
            )));
        }
        let mode = match s.join.as_deref().map(|j| j.trim().to_lowercase()).as_deref() {
            None | Some("") | Some("inner") => LinkMode::ExpandInner,
            Some("left") => LinkMode::ExpandLeft,
            Some(other) => return Err(invalid(format!("Unknown join '{other}' on expanding segment '{name}' — use inner or left"))),
        };
        pair_dedupe = match s.pair_dedupe.as_deref().map(|p| p.trim().to_lowercase()).as_deref() {
            None | Some("") | Some("neq") => PairDedupe::Neq,
            Some("lt") => PairDedupe::Lt,
            Some("none") => PairDedupe::None,
            Some(other) => {
                return Err(invalid(format!("Unknown pair_dedupe '{other}' on expanding segment '{name}' — use neq, lt or none")))
            }
        };
        let first_key = projections[0].source_ref.clone().unwrap_or_default();
        let key_name = cx
            .universe
            .field_by_uid(projections[0].field_uid.as_deref().unwrap_or(""), Some(FieldKind::Dimension))
            .map(|f| f.name.clone())
            .unwrap_or(first_key);
        for e in &s.expanding {
            let text = ref_text(&e.field)?;
            let f = cx.resolve(&text, None).or_else(|err| match err {
                SpecError::Ambiguous(_) => cx.resolve(&text, Some("dimension")),
                err => Err(err),
            })?;
            if f.kind != FieldKind::Dimension {
                return Err(invalid(format!("Expanding field '{text}' on segment '{name}' is a measure — only dimensions expand")));
            }
            // The definition on the segment, and its carried copy on the query.
            let ordinal = projections.len() as i64;
            let mut definition = segment_projection(format!("{reference}.p{ordinal}"), f, ordinal, ProjectionKind::Expansion, text.clone());
            definition.axis = "expansion".into();
            let definition_ref = definition.reference.clone();
            projections.push(definition);

            let carried_ordinal = query_projections.iter().filter_map(|p| p.ordinal).max().map(|m| m + 1).unwrap_or(0);
            let alias = e.alias.clone().filter(|a| !a.trim().is_empty()).unwrap_or_else(|| format!("{} (same {})", f.name, key_name));
            let mut carried =
                segment_projection(format!("p{}", query_projections.len()), f, carried_ordinal, ProjectionKind::Expansion, text);
            carried.alias = Some(alias);
            carried.parent = Some(definition_ref);
            query_projections.push(carried);
        }
        links.push(Link { mode, target: LinkTarget::Query, rank: 0 });
    } else {
        if s.join.is_some() {
            return Err(invalid(format!("join applies only to expanding segments — segment '{name}' has no expanding dimensions")));
        }
        let mode = match s.mode.as_deref().map(|m| m.trim().to_lowercase()).as_deref() {
            None | Some("") | Some("include") => LinkMode::IncludeIn,
            Some("exclude") => LinkMode::ExcludeFrom,
            Some(other) => return Err(invalid(format!("Unknown segment mode '{other}' — use include or exclude"))),
        };
        if s.apply_to.is_empty() {
            links.push(Link { mode, target: LinkTarget::Query, rank: 0 });
        } else {
            for (rank, target) in s.apply_to.iter().enumerate() {
                let p = measure_projection_for(cx, query_projections, target)?;
                links.push(Link { mode, target: LinkTarget::Projection(p.reference.clone()), rank: rank as i64 });
            }
        }
    }

    let (filters, filter_tree) = lower_filters(cx, s.filters.as_ref(), &format!("{reference}."))?;

    Ok(Segment {
        reference,
        expanding,
        name: Some(name),
        limit: None,
        db_settings: Attrs::new(),
        hints: vec![],
        filter_tree,
        projections,
        filters,
        links,
        pair_dedupe,
        rank: index as i64,
    })
}

fn segment_projection(reference: String, f: &Field, ordinal: i64, kind: ProjectionKind, source: String) -> Projection {
    Projection {
        reference,
        kind,
        field_uid: Some(f.uid.clone()),
        field_kind: Some(f.kind),
        alias: None,
        ordinal: Some(ordinal),
        axis: "row".into(),
        hidden: false,
        order_by: None,
        sql: None,
        calculation_field_type: None,
        calculation_data_type: None,
        dimension_refs: vec![],
        measure_refs: vec![],
        projection_refs: vec![],
        format: None,
        decorators: vec![],
        parent: None,
        is_static: false,
        source_ref: Some(source),
    }
}

/// `[old]` becomes `[new]` wherever the formula writes it, in any case.
fn rename_ref(sql: &str, old: &str, new: &str) -> String {
    let re = Regex::new(&format!(r"(?i)\[\s*{}\s*\]", regex::escape(old.trim()))).expect("escaped reference");
    re.replace_all(sql, format!("[{new}]").as_str()).into_owned()
}

/// The "Did you mean" tail of a not-found error, or empty.
fn did_you_mean_suffix(e: &SpecError) -> String {
    match e {
        SpecError::NotFound(m) | SpecError::Ambiguous(m) => m.find(" Did you mean").map(|i| m[i..].to_string()).unwrap_or_default(),
        _ => String::new(),
    }
}

/// Rails `Spec#measure_projection_for`: an `apply_to` ref names a measure
/// projection of the consuming query, by alias first, then by field uid.
fn measure_projection_for<'p>(cx: &Cx, projections: &'p [Projection], reference: &str) -> Result<&'p Projection> {
    if reference.trim().is_empty() {
        return Err(invalid("apply_to: references must be non-empty strings"));
    }
    let candidates: Vec<&Projection> = projections.iter().filter(|p| segment_targetable(cx, p, projections)).collect();
    let mut matches: Vec<&Projection> = candidates.iter().copied().filter(|p| alias_matches(cx, p, reference)).collect();
    if matches.is_empty() {
        // Rails `resolve_target_ref`: a measure's name or uid becomes its uid
        // before matching; a ref that is no field is tried as written.
        let uid = match resolve_field(cx.universe, reference, Some("measure")) {
            Ok(f) => f.uid.to_lowercase(),
            Err(_) => reference.split('@').next().unwrap_or("").trim().to_lowercase(),
        };
        matches = candidates
            .iter()
            .copied()
            .filter(|p| !p.is_calculation() && p.field_uid.as_deref().map(|u| u.to_lowercase()) == Some(uid.clone()))
            .collect();
    }
    match matches.len() {
        0 => {
            let handles: Vec<String> = candidates.iter().map(|p| display_alias(cx, p)).collect();
            let available = if handles.is_empty() {
                " — the target has no measure projections".to_string()
            } else {
                format!(" (measure projections: {})", handles.join(", "))
            };
            Err(invalid(format!(
                "apply_to: no measure projection matches '{reference}'{available}. Reference one of those verbatim, or set an alias on the projection to constrain."
            )))
        }
        1 => Ok(matches[0]),
        _ => {
            Err(invalid(format!("apply_to: '{reference}' matches multiple projections — set an alias on the one to constrain and use it")))
        }
    }
}

/// Rails `Projection#measure?`: a measure field, or a measure calculation.
fn is_measure(p: &Projection) -> bool {
    if p.is_calculation() {
        p.calculation_field_type.as_deref() == Some("measure")
    } else {
        p.field_kind == Some(FieldKind::Measure)
    }
}

/// Rails `Projection#segment_targetable?`: a plain measure, or a
/// calculation that depends on measures.
fn segment_targetable(cx: &Cx, p: &Projection, peers: &[Projection]) -> bool {
    if p.is_calculation() {
        p.calculation_field_type.as_deref() == Some("measure")
            || !p.measure_refs.is_empty()
            || p.projection_refs.iter().any(|a| peers.iter().any(|q| alias_matches(cx, q, a) && is_measure(q)))
    } else {
        p.field_kind == Some(FieldKind::Measure) && !p.is_expansion()
    }
}

// --------------------------------------------------------------- to_spec

impl Query {
    /// The spec form of a typed query: what the API returns and what a
    /// stored query re-applies from. Carried expansion columns are declared
    /// by their segment's `expanding` entry, statics are engine-internal,
    /// and a segment whose links the flat shape cannot express (mixed
    /// modes, no links) is left out, as in Rails.
    pub fn to_spec(&self) -> Spec {
        let mut projections: Vec<&Projection> = self.projections.iter().filter(|p| !p.is_static && !p.is_carried()).collect();
        projections.sort_by_key(|p| p.ordinal);
        Spec {
            name: self.name.clone(),
            description: None,
            classification: None,
            limit: self.limit,
            projections: projections.iter().map(|p| projection_spec(p)).collect(),
            calculations: vec![],
            filters: filters_spec(&self.filters, self.filter_tree.as_ref()),
            segments: self.segments.iter().filter_map(|s| segment_spec(self, s)).collect(),
            view: None,
            db_settings: if self.db_settings.is_empty() { None } else { Some(self.db_settings.clone()) },
            hints: self.hints.clone(),
        }
    }
}

fn projection_spec(p: &Projection) -> ProjectionSpec {
    let mut spec = ProjectionSpec {
        alias: p.alias.clone(),
        axis: Some(p.axis.clone()).filter(|a| a != "row"),
        order_by: p.order_by.map(|o| o.sql().to_string()),
        format: p.format.clone(),
        hidden: if p.hidden { Some(true) } else { None },
        decorators: p.decorators.iter().map(decorator_spec).collect(),
        ..Default::default()
    };
    if p.is_calculation() {
        spec.calculation = true;
        spec.sql = p.sql.clone();
        spec.data_type = p.calculation_data_type.clone();
    } else {
        spec.field = p.field_uid.clone().map(Value::String);
        // A dimension and a measure may share a uid; the kind keeps the
        // reference unambiguous when it is read back.
        spec.field_type = p.field_kind.map(|k| match k {
            FieldKind::Dimension => "Semantic::Dimension".to_string(),
            FieldKind::Measure => "Semantic::Measure".to_string(),
        });
    }
    spec
}

fn decorator_spec(d: &Decorator) -> DecoratorSpec {
    let defaults = decorator_defaults(d.kind);
    let attrs: Attrs = d
        .attrs
        .iter()
        .filter(|(k, v)| !v.is_null() && k.as_str() != "type" && !DECORATOR_NOISE.contains(&k.as_str()) && defaults.get(*k) != Some(*v))
        .map(|(k, v)| (k.clone(), v.clone()))
        .collect();
    DecoratorSpec { kind: decorator_type_key(d.kind).to_string(), attrs }
}

fn filter_leaf_spec(f: &Filter) -> Value {
    let leaf = FilterLeafSpec {
        field: Value::String(f.field_uid.clone()),
        field_type: f.field_kind.map(|k| match k {
            FieldKind::Dimension => "Semantic::Dimension".to_string(),
            FieldKind::Measure => "Semantic::Measure".to_string(),
        }),
        predicate: f.predicate.name().to_string(),
        value: f.filter_value.clone().map(Value::String),
        value_end: f.filter_value_end.clone().map(Value::String),
        top_n_measure: f.top_n_measure.clone().map(Value::String),
    };
    serde_json::to_value(leaf).expect("filter serializes")
}

/// A flat AND of every filter serializes as an array in ordinal order;
/// anything else as the `{and:/or:}` tree.
fn filters_spec(filters: &[Filter], tree: Option<&Tree>) -> Option<Value> {
    if filters.is_empty() {
        return None;
    }
    let flat = match tree {
        None => true,
        Some(Tree::Group(op, kids)) => op == "and" && kids.iter().all(|k| matches!(k, Tree::Leaf(_))) && kids.len() == filters.len(),
        Some(_) => false,
    };
    if flat {
        let mut sorted: Vec<&Filter> = filters.iter().collect();
        sorted.sort_by_key(|f| f.ordinal);
        return Some(Value::Array(sorted.iter().map(|f| filter_leaf_spec(f)).collect()));
    }
    fn node(t: &Tree, filters: &[Filter]) -> Option<Value> {
        match t {
            Tree::Leaf(r) => filters.iter().find(|f| &f.reference == r).map(filter_leaf_spec),
            Tree::Group(op, kids) => Some(json!({ op: kids.iter().filter_map(|k| node(k, filters)).collect::<Vec<_>>() })),
        }
    }
    node(tree?, filters)
}

fn segment_spec(query: &Query, s: &Segment) -> Option<SegmentSpec> {
    if s.links.is_empty() {
        return None;
    }
    let keys: Vec<Value> = s
        .projections
        .iter()
        .filter(|p| p.kind == ProjectionKind::Field && p.field_kind == Some(FieldKind::Dimension))
        .filter_map(|p| p.field_uid.clone())
        .map(Value::String)
        .collect();
    let measures: Vec<Value> = s
        .projections
        .iter()
        .filter(|p| p.kind == ProjectionKind::Field && p.field_kind == Some(FieldKind::Measure))
        .filter_map(|p| p.field_uid.clone())
        .map(Value::String)
        .collect();
    let filters = filters_spec(&s.filters, s.filter_tree.as_ref());
    if s.expanding {
        let link = s.links.iter().find(|l| l.target == LinkTarget::Query).or_else(|| s.links.first())?;
        let expanding = s
            .projections
            .iter()
            .filter(|p| p.is_expansion() && p.parent.is_none())
            .map(|def| ExpandingSpec {
                field: Value::String(def.field_uid.clone().unwrap_or_default()),
                alias: query
                    .projections
                    .iter()
                    .find(|c| c.is_carried() && c.parent.as_deref() == Some(&def.reference))
                    .and_then(|c| c.alias.clone()),
            })
            .collect();
        return Some(SegmentSpec {
            name: s.name.clone(),
            join: (link.mode == LinkMode::ExpandLeft).then(|| "left".to_string()),
            keys,
            expanding,
            measures,
            filters,
            pair_dedupe: match s.pair_dedupe {
                PairDedupe::Neq => None,
                PairDedupe::Lt => Some("lt".into()),
                PairDedupe::None => Some("none".into()),
            },
            ..Default::default()
        });
    }
    let mut modes: Vec<LinkMode> = s.links.iter().map(|l| l.mode).collect();
    modes.sort();
    modes.dedup();
    if modes.len() != 1 {
        return None;
    }
    let whole_query = s.links.iter().any(|l| l.target == LinkTarget::Query);
    let apply_to = if whole_query {
        vec![]
    } else {
        s.links
            .iter()
            .filter_map(|l| match &l.target {
                LinkTarget::Projection(r) => query.projections.iter().find(|p| &p.reference == r),
                LinkTarget::Query => None,
            })
            .map(|p| p.alias.clone().or_else(|| p.field_uid.clone()).unwrap_or_default())
            .collect()
    };
    Some(SegmentSpec {
        name: s.name.clone(),
        mode: Some(match modes[0] {
            LinkMode::ExcludeFrom => "exclude".to_string(),
            _ => "include".to_string(),
        }),
        keys,
        measures,
        filters,
        apply_to,
        ..Default::default()
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kind_hints_follow_rails_spellings() {
        assert_eq!(kind_hint(Some("Semantic::Dimension")), Some(FieldKind::Dimension));
        assert_eq!(kind_hint(Some("d")), Some(FieldKind::Dimension));
        assert_eq!(kind_hint(Some("dim")), Some(FieldKind::Dimension));
        assert_eq!(kind_hint(Some("Semantic::Measure")), Some(FieldKind::Measure));
        assert_eq!(kind_hint(Some("m")), Some(FieldKind::Measure));
        assert_eq!(kind_hint(Some("")), None);
        assert_eq!(kind_hint(None), None);
    }

    #[test]
    fn filter_spec_shapes() {
        let flat = parse_filters(&json!([{ "field": "a", "predicate": "equals", "value": "x" }])).unwrap().unwrap();
        assert!(matches!(flat, FilterNodeSpec::Group(ref op, ref kids) if op == "and" && kids.len() == 1));
        let tree = parse_filters(&json!({ "or": [{ "field": "a", "predicate": "equals", "filter_value": "x" }, { "and": [] }] }))
            .unwrap()
            .unwrap();
        assert!(matches!(tree, FilterNodeSpec::Group(ref op, ref kids) if op == "or" && kids.len() == 2));
        assert!(parse_filters(&json!([{ "and": [] }])).is_err());
        assert!(parse_filters(&json!({ "xor": [] })).is_err());
    }

    /// The `class` on an error is a contract: callers switch on it, the
    /// OpenAPI publishes it, and `zsql` prints it. Pin the set so a rename
    /// is a deliberate act with a failing test, not a silent break.
    #[test]
    fn error_classes_are_0sqls_own() {
        assert_eq!(SpecError::NotFound(String::new()).error_class(), "FieldNotFound");
        assert_eq!(SpecError::Ambiguous(String::new()).error_class(), "FieldAmbiguous");
        assert_eq!(SpecError::Invalid(String::new()).error_class(), "InvalidSpec");
        assert_eq!(SpecError::RecordInvalid(String::new()).error_class(), "InvalidValue");
        // Nothing here may name a framework we are not.
        for class in ["FieldNotFound", "FieldAmbiguous", "InvalidSpec", "InvalidValue"] {
            assert!(!class.contains("ActiveRecord") && !class.contains("::"), "{class}");
        }
    }

    /// A decorator attribute that is set but not one of the choices is a
    /// different failure from one that is missing, and says so.
    #[test]
    fn unknown_enum_value_is_not_reported_as_missing() {
        let dec = |kind: &str, key: &str, value: Value| {
            let mut attrs = Attrs::new();
            attrs.insert(key.into(), value);
            lower_decorator(&DecoratorSpec { kind: kind.into(), attrs }, None, "Contact Date").unwrap_err().to_string()
        };
        assert_eq!(
            dec("truncate", "grain", json!("fortnight")),
            "Validation failed: unknown grain \"fortnight\"; use raw, millisecond, second, minute, hour, day, week, month, quarter or year (truncate on Contact Date)"
        );
        assert_eq!(
            dec("extract", "extract", json!("fortnight")),
            "Validation failed: unknown extract \"fortnight\"; use minute, hour, day_of_month, day_of_week, day_of_year, week_of_year, month, quarter, year, day_name, month_name or year_month (extract on Contact Date)"
        );
        assert_eq!(
            dec("window", "mode", json!("sliding")),
            "Validation failed: unknown mode \"sliding\"; use moving or running (window on Contact Date)"
        );
        assert_eq!(
            dec("temporalize", "transform", json!("decade_over_decade")),
            "Validation failed: unknown transform \"decade_over_decade\"; use year_over_year, quarter_over_quarter, month_over_month, week_over_week or day_over_day (temporalize on Contact Date)"
        );
        // Absent keeps the wording Rails' own validations use.
        assert_eq!(dec("truncate", "grain", Value::Null), "Validation failed: Grain must be set (Contact Date)");
        assert_eq!(dec("extract", "extract", Value::Null), "Validation failed: Extract must be set (Contact Date)");
        assert_eq!(dec("window", "mode", Value::Null), "Validation failed: Mode must be moving or running (Contact Date)");
    }
}
