//! The steps of a `steps` node: a closed set of operations on one
//! input's rows, each declaring the columns it reads, compiled by
//! `compile` and never written as SQL by anyone but a `sql` step's
//! author.

use crate::schema::ColumnType;
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// One operation of a `steps` node: a closed set, each declaring the columns
/// it reads, compiled by `compile` and never written as SQL by anyone.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Step {
    // Columns.
    RenameColumn {
        column: String,
        to: String,
    },
    /// A new type, kept only if every value converts; text to a date may
    /// name its format, in strptime's form.
    RetypeColumn {
        column: String,
        #[serde(rename = "type")]
        ty: ColumnType,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    RemoveColumns {
        columns: Vec<String>,
    },
    /// These columns first, in this order, and the rest after as they were.
    ReorderColumns {
        columns: Vec<String>,
    },
    /// A new column, computed by a formula.
    AddColumn {
        column: String,
        formula: String,
    },
    /// A column computed again by a formula, which may read it: its
    /// values, and its type, are the formula's.
    UpdateColumn {
        column: String,
        formula: String,
    },
    // Rows.
    /// The rows the formula is true for.
    Filter {
        formula: String,
    },
    Sort {
        by: Vec<SortKey>,
    },
    /// The first row of each set of rows equal on these columns, or on
    /// all of them.
    Dedupe {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        columns: Vec<String>,
    },
    /// An empty value made this one.
    FillEmpty {
        column: String,
        value: Value,
    },
    /// Each empty value made the nearest one above it, in the order the
    /// rows are in: a label a spreadsheet wrote once for the rows below.
    FillDown {
        columns: Vec<String>,
    },
    // Values.
    /// Text found and replaced: within a value, or the whole value.
    FindAndReplaceVal {
        column: String,
        find: String,
        with: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        whole: bool,
    },
    /// Whole values made others: `NY` to `New York`.
    RemapVal {
        column: String,
        values: Vec<Mapping>,
    },
    TrimVal {
        columns: Vec<String>,
    },
    /// `Grocery Store` to `GROCERY STORE`.
    UpperCase {
        columns: Vec<String>,
    },
    /// To `grocery store`.
    LowerCase {
        columns: Vec<String>,
    },
    /// To `Grocery Store`.
    TitleCase {
        columns: Vec<String>,
    },
    /// To `grocery_store`; `GroceryStore` too.
    SnakeCase {
        columns: Vec<String>,
    },
    /// To `groceryStore`.
    CamelCase {
        columns: Vec<String>,
    },
    // Shape.
    /// A row per set of values of `by`, and the aggregates over it.
    Group {
        by: Vec<String>,
        aggregates: Vec<Aggregate>,
    },
    /// Columns made rows: each row becomes one per column unpivoted, with
    /// that column's name in `name` and its value in `value`; the other
    /// columns repeat. Name the columns to unpivot, or the ones to
    /// `keep` as they are, so that a column a later file adds (a new
    /// month) is unpivoted too. Empty values make no row unless
    /// `keep_empty`.
    Unpivot {
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        columns: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        keep: Vec<String>,
        name: String,
        value: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        keep_empty: bool,
    },
    /// A query, run as a black box over the step before, which it reads
    /// as `input` and as nothing else; see `sql`. A `sql` node reads
    /// several inputs by alias.
    Sql {
        query: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JoinKey {
    /// A column here.
    pub left: String,
    /// A column of the other side.
    pub right: String,
}

#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum JoinKind {
    #[default]
    Left,
    Inner,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct SortKey {
    pub column: String,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub desc: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Mapping {
    pub from: Value,
    pub to: Value,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Aggregate {
    /// The column aggregated; none for a count of rows.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(rename = "fn")]
    pub func: AggregateFn,
    #[serde(rename = "as")]
    pub name: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum AggregateFn {
    Sum,
    Min,
    Max,
    Avg,
    Count,
    CountDistinct,
}

impl Step {
    /// The step's name, as it is told to a user: `step 3 (remap_val)`.
    pub fn op(&self) -> &'static str {
        match self {
            Step::RenameColumn { .. } => "rename_column",
            Step::RetypeColumn { .. } => "retype_column",
            Step::RemoveColumns { .. } => "remove_columns",
            Step::ReorderColumns { .. } => "reorder_columns",
            Step::AddColumn { .. } => "add_column",
            Step::UpdateColumn { .. } => "update_column",
            Step::Filter { .. } => "filter",
            Step::Sort { .. } => "sort",
            Step::Dedupe { .. } => "dedupe",
            Step::FillEmpty { .. } => "fill_empty",
            Step::FillDown { .. } => "fill_down",
            Step::FindAndReplaceVal { .. } => "find_and_replace_val",
            Step::RemapVal { .. } => "remap_val",
            Step::TrimVal { .. } => "trim_val",
            Step::UpperCase { .. } => "upper_case",
            Step::LowerCase { .. } => "lower_case",
            Step::TitleCase { .. } => "title_case",
            Step::SnakeCase { .. } => "snake_case",
            Step::CamelCase { .. } => "camel_case",
            Step::Group { .. } => "group",
            Step::Unpivot { .. } => "unpivot",
            Step::Sql { .. } => "sql",
        }
    }
}

/// The most steps a node has.
pub const STEPS_LIMIT: usize = 500;
