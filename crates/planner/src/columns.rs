//! The statement's result columns, in SELECT order, with what a client
//! needs to show them: the field each one is, its type, its grain, and
//! its format and display type. The planner reads none of the last two;
//! it only works out which apply. See docs/presentation.md.

use serde::Serialize;
use serde_json::{json, Value};
use zsql_semantic::FieldKind;
use zsql_spec::DecoratorKind;

use crate::dag::Graph;
use crate::lift::SYNTHETIC_MEASURE;

#[derive(Debug, Clone, Serialize)]
pub struct Column {
    /// As the statement names it, and as a template's slot reads it.
    pub name: String,
    /// `dimension` or `measure`.
    pub kind: &'static str,
    /// The model's field; absent for a calculation.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub field_uid: Option<String>,
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub calculation: bool,
    /// The model's data type, or a calculation's; absent when a decorator
    /// made the value something else, as an extracted weekday is.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub data_type: Option<String>,
    /// A truncated date's grain, so a client can pick a pattern for it.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub grain: Option<String>,
    /// What an extracted date part is: `month_name`, `day_of_week`.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub extract: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub format: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub display_type: Option<Value>,
    /// Carried for another column's template, or for the client's own
    /// use, and not shown.
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub hidden: bool,
}

/// A window function whose value counts rows, not the measure.
const COUNTS: [&str; 5] = ["rank", "dense_rank", "row_number", "ntile", "count"];
/// One whose value is a share of the rows.
const SHARES: [&str; 2] = ["percent_rank", "cume_dist"];

/// What a decorator makes of the field's value for showing it: kept, a
/// share (shown as a percent), or another thing with no format of its own.
enum Becomes {
    Same,
    Share,
    Other,
}

fn becomes(kind: DecoratorKind, d: &zsql_spec::Decorator) -> Becomes {
    match kind {
        DecoratorKind::Truncate | DecoratorKind::Customize => Becomes::Same,
        DecoratorKind::Extract => Becomes::Other,
        DecoratorKind::Contribute => Becomes::Share,
        DecoratorKind::Temporalize if d.bool("percent_change", false) => Becomes::Share,
        DecoratorKind::Temporalize => Becomes::Same,
        DecoratorKind::Window => match d.str("function").unwrap_or("") {
            f if COUNTS.contains(&f) => Becomes::Other,
            f if SHARES.contains(&f) => Becomes::Share,
            _ => Becomes::Same,
        },
    }
}

/// The root node's projections, as its SELECT lists them.
pub(crate) fn of(graph: &Graph) -> Vec<Column> {
    let root = &graph.nodes[graph.root()];
    root.projections
        .iter()
        .map(|p| {
            // A visitor that computes a projection another way (a share
            // of the total as a calculation over a total) replaces it
            // with a static calculation and keeps the one it replaced.
            // The column is still the caller's projection.
            let origin = p.proj.source().unwrap_or_else(|| p.proj.clone());
            let def = &origin.def;
            // A lifted calculation plans as a synthetic field; it is
            // still the calculation the caller wrote.
            let field = origin.field.filter(|f| !f.uid.starts_with(SYNTHETIC_MEASURE));
            let calculation = field.is_none();
            let kind = match (field.map(|f| f.kind), def.field_kind, def.calculation_field_type.as_deref()) {
                (Some(FieldKind::Dimension), _, _) | (None, Some(FieldKind::Dimension), _) | (None, None, Some("dimension")) => "dimension",
                _ => "measure",
            };
            let mut data_type = def
                .calculation_data_type
                .clone()
                .or_else(|| origin.field.and_then(|f| serde_json::to_value(f.data_type).ok()).and_then(|v| v.as_str().map(str::to_string)));
            let mut format = field.and_then(|f| f.format.clone());
            let mut display_type = field.and_then(|f| f.display_type.clone());
            let (mut grain, mut extract) = (None, None);
            for d in def.decorators.iter().chain(p.local_decorators.iter()) {
                match d.kind {
                    DecoratorKind::Truncate => grain = d.str("grain").filter(|g| *g != "raw").map(str::to_string),
                    DecoratorKind::Extract => extract = d.str("extract").map(str::to_string),
                    _ => {}
                }
                match becomes(d.kind, d) {
                    Becomes::Same => {}
                    Becomes::Share => {
                        format = Some(json!({ "type": "percent" }));
                        display_type = None;
                        data_type = Some("decimal".into());
                    }
                    Becomes::Other => {
                        format = None;
                        display_type = None;
                        data_type = None;
                    }
                }
            }
            // The projection's own, last: `raw` and `none` clear.
            if let Some(f) = &def.format {
                format = (f["type"] != "raw").then(|| f.clone());
            }
            if let Some(d) = &def.display_type {
                display_type = (d["type"] != "none").then(|| d.clone());
            }
            Column {
                name: p.alias.clone(),
                kind,
                field_uid: field.map(|f| f.uid.clone()),
                calculation,
                data_type,
                grain,
                extract,
                format,
                display_type,
                hidden: def.hidden,
            }
        })
        .collect()
}
