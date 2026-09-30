//! The query spec, its lowering into the typed query the planner consumes,
//! the formula parser, and the security context a query is planned for.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

pub type Attrs = Map<String, Value>;

/// Who the query is planned for. The caller passes this with every
/// request; row-level security policies read it to decide what the
/// principal may see. Nothing here is stored.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityContext {
    /// Used when a policy resolves permissions from the user's email.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub email: Option<String>,
    /// Bypasses policies that say system admins bypass them.
    #[serde(default)]
    pub system_admin: bool,
    /// Bypasses policies that say project admins bypass them.
    #[serde(default)]
    pub project_admin: bool,
    /// The user's own tags, `key:value`, read by user-scoped tag policies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
    /// The groups the user belongs to, read by group-scoped policies.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub groups: Vec<Group>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Group {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub tags: Vec<String>,
}

pub mod expr;
pub mod model;
pub mod spec;
pub use model::*;
pub use spec::{lower, resolve_field, Spec, SpecError};
