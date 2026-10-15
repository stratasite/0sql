//! The semantic model as the planner sees it.
//!
//! [`Snapshot`] is the serialized form of one branch: datasources, tables,
//! fields, expressions, joins, partitions and security policies, plus the
//! join paths stored with it. A project directory's paths are formed by
//! the formation crate; a stored snapshot carries its own, and the golden
//! scoreboard can plan over either. [`Universe`] is the indexed, validated
//! in-memory form the planner works against.

pub mod snapshot;
pub mod universe;

pub use snapshot::*;
pub use universe::{BuildError, Universe};
