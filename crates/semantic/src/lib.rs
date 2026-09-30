//! The semantic model as the planner sees it.
//!
//! [`Snapshot`] is the serialized form of one branch: datasources, tables,
//! fields, expressions, joins, partitions and security policies, plus the
//! join paths Rails' formation engine computed (kept only as an oracle while
//! the Rust formation crate is validated). [`Universe`] is the indexed,
//! validated in-memory form the planner works against.

pub mod snapshot;
pub mod universe;

pub use snapshot::*;
pub use universe::{BuildError, Universe};
