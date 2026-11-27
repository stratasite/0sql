//! The recipe: what is done to a dataset's sources to make it. Every
//! version is kept, and a change names the version it was made against,
//! so two editors never overwrite each other and undo is going back to
//! a version that is still there.

use crate::error::{Error, Result};
use crate::schema::{Column, ColumnType, Role};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Recipe {
    pub version: u32,
    pub created: String,
    /// Roles the user gave columns, over the ones they were inferred
    /// with.
    #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
    pub roles: BTreeMap<String, Role>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub steps: Vec<Step>,
    /// The columns that name a row, for edits, when the dimensions do
    /// not; none means every dimension.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub key: Vec<String>,
}

/// One operation of a recipe: a closed set, each declaring the columns
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
    /// as `input` and as nothing else; see `sql`.
    Sql {
        query: String,
    },
    /// Another dataset's rows after these, its columns met by name.
    Append {
        dataset: String,
    },
    // Combine.
    /// Another dataset's columns beside these, by keys: `left` keeps every
    /// row here (a look-up), `inner` only those that match. The other
    /// side must be unique on its keys, so no row is multiplied.
    Join {
        dataset: String,
        on: Vec<JoinKey>,
        #[serde(default)]
        kind: JoinKind,
        /// The columns brought; all but its keys when not said.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<Vec<String>>,
        /// Put before each brought column's name.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct JoinKey {
    /// A column here.
    pub left: String,
    /// A column of the other dataset.
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
            Step::Append { .. } => "append",
            Step::Join { .. } => "join",
        }
    }

    /// The dataset the step reads, when it reads one.
    pub fn dataset(&self) -> Option<&str> {
        match self {
            Step::Append { dataset } | Step::Join { dataset, .. } => Some(dataset),
            _ => None,
        }
    }

    /// What the step needs of the dataset it reads, by name: a join's
    /// keys and the columns it names. An append or a join that brings
    /// every column needs no one column in particular.
    pub fn needs(&self) -> Vec<String> {
        match self {
            Step::Join { on, columns, .. } => on.iter().map(|k| k.right.clone()).chain(columns.iter().flatten().cloned()).collect(),
            _ => Vec::new(),
        }
    }
}

impl Recipe {
    /// The datasets its steps read.
    pub fn inputs(&self) -> Vec<String> {
        let mut out: Vec<String> = self.steps.iter().filter_map(Step::dataset).map(str::to_string).collect();
        out.sort();
        out.dedup();
        out
    }
}

/// The most steps a recipe has.
pub const STEPS_LIMIT: usize = 500;

/// A change to a recipe, as a request makes it: the whole of the new
/// recipe, and the version it replaces.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Change {
    pub against: u32,
    #[serde(default)]
    pub roles: BTreeMap<String, Role>,
    #[serde(default)]
    pub steps: Vec<Step>,
    #[serde(default)]
    pub key: Vec<String>,
}

impl Recipe {
    /// The recipe a dataset starts with: its sources as they are.
    pub fn first(created: String) -> Recipe {
        Recipe { version: 1, created, roles: BTreeMap::new(), steps: Vec::new(), key: Vec::new() }
    }

    /// The recipe a change makes of this one, refused when the change
    /// was made against another version.
    pub fn changed(&self, change: Change, created: String) -> Result<Recipe> {
        if change.against != self.version {
            return Err(Error::Conflict(format!(
                "this change was made against recipe version {}, and the recipe is at version {} now; read it again and make the change on that",
                change.against, self.version
            )));
        }
        if change.steps.len() > STEPS_LIMIT {
            return Err(Error::Invalid(format!("a recipe has at most {STEPS_LIMIT} steps")));
        }
        Ok(Recipe { version: self.version + 1, created, roles: change.roles, steps: change.steps, key: change.key })
    }

    /// The result's columns, with the user's roles over the ones they
    /// came with; the names of roles given to columns the result does
    /// not have.
    pub fn apply_roles(&self, columns: &mut [Column]) -> Vec<String> {
        let mut missing = Vec::new();
        for (name, role) in &self.roles {
            match columns.iter_mut().find(|c| &c.name == name) {
                Some(c) => c.role = *role,
                None => missing.push(name.clone()),
            }
        }
        missing
    }
}
