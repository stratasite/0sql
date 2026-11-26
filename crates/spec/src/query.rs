//! The typed query: what the planner reads. A spec lowers into it
//! (`crate::spec::lower`), and it serializes back to a spec.

use crate::Attrs;
use serde_json::Value;
use zsql_semantic::{FieldKind, Predicate};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ProjectionKind {
    Field,
    Calculation,
    Expansion,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OrderBy {
    Asc,
    Desc,
}

impl OrderBy {
    pub fn sql(self) -> &'static str {
        match self {
            OrderBy::Asc => "asc",
            OrderBy::Desc => "desc",
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DecoratorKind {
    Truncate,
    Extract,
    Window,
    Contribute,
    Temporalize,
    Customize,
}

#[derive(Debug, Clone)]
pub struct Decorator {
    pub kind: DecoratorKind,
    pub attrs: Attrs,
}

impl Decorator {
    pub fn str(&self, key: &str) -> Option<&str> {
        self.attrs.get(key).and_then(Value::as_str)
    }
    pub fn grain(&self) -> Option<&str> {
        self.str("grain")
    }
    pub fn bool(&self, key: &str, default: bool) -> bool {
        self.attrs.get(key).and_then(Value::as_bool).unwrap_or(default)
    }
    pub fn int(&self, key: &str) -> Option<i64> {
        self.attrs.get(key).and_then(Value::as_i64)
    }
    pub fn strs(&self, key: &str) -> Vec<String> {
        self.attrs
            .get(key)
            .and_then(Value::as_array)
            .map(|a| {
                a.iter()
                    .map(|v| match v {
                        Value::String(s) => s.clone(),
                        other => other.to_string(),
                    })
                    .collect()
            })
            .unwrap_or_default()
    }
    /// A map attribute such as a window's order_by (`{"-1": "asc"}`).
    pub fn map(&self, key: &str) -> Vec<(String, String)> {
        self.attrs
            .get(key)
            .and_then(Value::as_object)
            .map(|m| m.iter().map(|(k, v)| (k.clone(), v.as_str().unwrap_or("").to_string())).collect())
            .unwrap_or_default()
    }
    pub fn custom(kind: DecoratorKind, attrs: Attrs) -> Decorator {
        Decorator { kind, attrs }
    }
    pub fn inline(&self) -> bool {
        !matches!(self.kind, DecoratorKind::Contribute | DecoratorKind::Temporalize)
    }
    /// Rails applies decorators lowest priority first.
    pub fn priority(&self) -> i32 {
        match self.kind {
            DecoratorKind::Truncate => 0,
            DecoratorKind::Extract => 1,
            DecoratorKind::Contribute => 2,
            DecoratorKind::Temporalize => 3,
            DecoratorKind::Window => 3,
            DecoratorKind::Customize => 10000,
        }
    }
    /// Whether the attribute is absent or null (as opposed to an empty list).
    pub fn is_null(&self, key: &str) -> bool {
        self.attrs.get(key).map(Value::is_null).unwrap_or(true)
    }
    /// The alias the decorator lends to its projection, if any.
    pub fn alias(&self, field_name: &str) -> Option<String> {
        match self.kind {
            DecoratorKind::Truncate => match self.grain() {
                None | Some("raw") => Some(field_name.to_string()),
                Some(g) => Some(format!("{}({})", capitalize(g), field_name)),
            },
            DecoratorKind::Extract => self.str("extract").map(|e| {
                let label = match e {
                    "minute" => "min",
                    "hour" => "hr",
                    "day_of_month" => "dom",
                    "day_of_week" => "dow",
                    "day_of_year" => "doy",
                    "week_of_year" => "woy",
                    "month" => "mon",
                    "quarter" => "qtr",
                    "year" => "yr",
                    "day_name" => "day",
                    "month_name" => "month",
                    "year_month" => "ym",
                    other => other,
                };
                format!("{}({})", label, field_name)
            }),
            DecoratorKind::Window => Some(format!(
                "{} {}({})",
                capitalize(self.str("mode").unwrap_or("")),
                capitalize(self.str("function").unwrap_or("")),
                field_name
            )),
            DecoratorKind::Contribute => {
                Some(if field_name.contains('%') { format!("{field_name} of Total") } else { format!("% {field_name} of Total") })
            }
            DecoratorKind::Temporalize => Some(format!("{}({})", self.transform_label(), field_name)),
            DecoratorKind::Customize => Some(format!("Custom({field_name})")),
        }
    }
    /// Rails `Temporalizer#transform_label`: `LY`, `LQ`, `LM`, `LW`, `D/D`, with `%` when percent change.
    pub fn transform_label(&self) -> String {
        let t = self.str("transform").unwrap_or("");
        if t.is_empty() {
            return "ERROR".into();
        }
        let label = if t == "day_over_day" { "D/D".to_string() } else { format!("L{}", t.chars().next().unwrap().to_ascii_uppercase()) };
        if self.bool("percent_change", false) {
            format!("%{label}")
        } else {
            label
        }
    }

    /// Everything except bookkeeping; part of a projection's column identity.
    pub fn column_key(&self) -> Attrs {
        let mut a = self.attrs.clone();
        for k in ["prompt", "share_full_result_set_with_ai", "type"] {
            a.remove(k);
        }
        a
    }
}

fn capitalize(s: &str) -> String {
    let mut c = s.chars();
    match c.next() {
        Some(f) => f.to_uppercase().collect::<String>() + c.as_str(),
        None => String::new(),
    }
}

#[derive(Debug, Clone)]
pub struct Projection {
    pub reference: String,
    pub kind: ProjectionKind,
    pub field_uid: Option<String>,
    pub field_kind: Option<FieldKind>,
    pub alias: Option<String>,
    pub ordinal: Option<i64>,
    pub axis: String,
    pub hidden: bool,
    pub order_by: Option<OrderBy>,
    pub sql: Option<String>,
    pub calculation_field_type: Option<String>,
    pub calculation_data_type: Option<String>,
    pub dimension_refs: Vec<String>,
    pub measure_refs: Vec<String>,
    pub projection_refs: Vec<String>,
    pub format: Option<Value>,
    pub decorators: Vec<Decorator>,
    pub parent: Option<String>,
    /// Engine-injected and read-only: rendered, never validated, and not a
    /// measure for the purpose of typing the query.
    pub is_static: bool,
    /// The field reference as the author wrote it in a spec (a uid or a
    /// name, with or without an `@d`/`@m` suffix); kept for error messages
    /// and for serializing back. Absent on definitions loaded from records.
    pub source_ref: Option<String>,
}

impl Projection {
    pub fn is_calculation(&self) -> bool {
        self.kind == ProjectionKind::Calculation
    }
    pub fn is_expansion(&self) -> bool {
        self.kind == ProjectionKind::Expansion
    }
    /// A carried expansion column lives on a consumer and points at its definition.
    pub fn is_carried(&self) -> bool {
        self.is_expansion() && self.parent.is_some()
    }
}

#[derive(Debug, Clone)]
pub struct Filter {
    pub reference: String,
    pub field_uid: String,
    pub field_kind: Option<FieldKind>,
    pub predicate: Predicate,
    pub filter_value: Option<String>,
    pub filter_value_end: Option<String>,
    pub top_n_measure: Option<String>,
    /// Position among the query's filters, so the WHERE clause order is
    /// the order the spec listed them in.
    pub ordinal: i64,
    /// The field reference as written in a spec; see `Projection::source_ref`.
    pub source_ref: Option<String>,
}

/// A filter logic tree: `and`/`or` groups over filter refs.
#[derive(Debug, Clone, PartialEq)]
pub enum Tree {
    Group(String, Vec<Tree>),
    Leaf(String),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub enum LinkMode {
    IncludeIn,
    ExcludeFrom,
    ExpandInner,
    ExpandLeft,
}

impl LinkMode {
    pub fn name(self) -> &'static str {
        match self {
            LinkMode::IncludeIn => "include_in",
            LinkMode::ExcludeFrom => "exclude_from",
            LinkMode::ExpandInner => "expand_inner",
            LinkMode::ExpandLeft => "expand_left",
        }
    }
    pub fn is_expanding(self) -> bool {
        matches!(self, LinkMode::ExpandInner | LinkMode::ExpandLeft)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum LinkTarget {
    /// The queryable as a whole.
    Query,
    /// A projection of the queryable, by ref.
    Projection(String),
}

#[derive(Debug, Clone)]
pub struct Link {
    pub mode: LinkMode,
    pub target: LinkTarget,
    pub rank: i64,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PairDedupe {
    None,
    Neq,
    Lt,
}

#[derive(Debug, Clone)]
pub struct Segment {
    pub reference: String,
    pub expanding: bool,
    pub name: Option<String>,
    pub limit: Option<i64>,
    pub db_settings: Attrs,
    pub hints: Vec<String>,
    pub filter_tree: Option<Tree>,
    pub projections: Vec<Projection>,
    pub filters: Vec<Filter>,
    pub links: Vec<Link>,
    pub pair_dedupe: PairDedupe,
    pub rank: i64,
}

#[derive(Debug, Clone)]
pub struct Query {
    /// Set by the planner when it plans a segment's population on its own;
    /// a query from a spec never is.
    pub population: bool,
    pub name: Option<String>,
    pub limit: Option<i64>,
    pub db_settings: Attrs,
    pub hints: Vec<String>,
    pub filter_tree: Option<Tree>,
    pub projections: Vec<Projection>,
    pub filters: Vec<Filter>,
    pub segments: Vec<Segment>,
    /// References that resolved to a field by near match rather than
    /// exactly; see [`crate::fuzzy`].
    pub corrections: Vec<crate::fuzzy::Correction>,
}

impl Query {
    /// A query the planner builds for itself out of another's parts.
    pub fn planner_sub(name: String) -> Self {
        Query {
            population: false,
            name: Some(name),
            limit: None,
            db_settings: Attrs::new(),
            hints: vec![],
            filter_tree: None,
            projections: vec![],
            filters: vec![],
            segments: vec![],
            corrections: vec![],
        }
    }
}
