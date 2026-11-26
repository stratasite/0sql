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
    Rename {
        column: String,
        to: String,
    },
    /// A new type, kept only if every value converts; text to a date may
    /// name its format, in strptime's form.
    Retype {
        column: String,
        #[serde(rename = "type")]
        ty: ColumnType,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        format: Option<String>,
    },
    Remove {
        columns: Vec<String>,
    },
    /// These columns first, in this order, and the rest after as they were.
    Reorder {
        columns: Vec<String>,
    },
    /// A new column, computed by a formula.
    Formula {
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
    Fill {
        column: String,
        value: Value,
    },
    // Values.
    /// Text found and replaced: within a value, or the whole value.
    Replace {
        column: String,
        find: String,
        with: String,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        whole: bool,
    },
    /// Whole values made others: `NY` to `New York`.
    Remap {
        column: String,
        values: Vec<Mapping>,
    },
    Trim {
        columns: Vec<String>,
    },
    Case {
        columns: Vec<String>,
        to: Case,
    },
    // Shape.
    /// A row per set of values of `by`, and the aggregates over it.
    Group {
        by: Vec<String>,
        aggregates: Vec<Aggregate>,
    },
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

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Case {
    Upper,
    Lower,
    Title,
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
    /// The step's name, as it is told to a user: `step 3 (remap)`.
    pub fn op(&self) -> &'static str {
        match self {
            Step::Rename { .. } => "rename",
            Step::Retype { .. } => "retype",
            Step::Remove { .. } => "remove",
            Step::Reorder { .. } => "reorder",
            Step::Formula { .. } => "formula",
            Step::Filter { .. } => "filter",
            Step::Sort { .. } => "sort",
            Step::Dedupe { .. } => "dedupe",
            Step::Fill { .. } => "fill",
            Step::Replace { .. } => "replace",
            Step::Remap { .. } => "remap",
            Step::Trim { .. } => "trim",
            Step::Case { .. } => "case",
            Step::Group { .. } => "group",
        }
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
