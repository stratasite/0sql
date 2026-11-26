//! The recipe: what is done to a dataset's sources to make it. Every
//! version is kept, and a change names the version it was made against,
//! so two editors never overwrite each other and undo is going back to
//! a version that is still there.

use crate::error::{Error, Result};
use crate::schema::{Column, Role};
use serde::{Deserialize, Serialize};
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
}

/// One operation of a recipe. The set of them arrives with the steps.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum Step {}

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
}

impl Recipe {
    /// The recipe a dataset starts with: its sources as they are.
    pub fn first(created: String) -> Recipe {
        Recipe { version: 1, created, roles: BTreeMap::new(), steps: Vec::new() }
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
        Ok(Recipe { version: self.version + 1, created, roles: change.roles, steps: change.steps })
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
