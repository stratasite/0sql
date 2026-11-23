//! The semantic model as the planner sees it.
//!
//! [`Snapshot`] is the serialized form of one branch: datasources, tables,
//! fields, expressions, joins, partitions and security policies, plus the
//! join paths stored with it. A project directory's paths are formed by
//! the formation crate; a stored snapshot carries its own, and the golden
//! scoreboard can plan over either. [`SemanticModel`] is the indexed, validated
//! in-memory form the planner works against.

pub mod model;
pub mod snapshot;

pub use model::{BuildError, SemanticModel};
pub use snapshot::{
    default_sample_size, BranchRef, Cardinality, DataType, Datasource, Exclusion, ExclusionScope, ExclusionType, Expression, Field,
    FieldKind, FilterMode, Id, Inclusion, JoinDef, JoinType, LoadError, Partition, Path, PermissionSource, PermissionValueFrom, PolicyMode,
    Predicate, ProjectRef, SecurityPolicy, Snapshot, SnapshotKind, Table, Tier, Unresolved, DATASOURCE_FIELDS, FORMAT, MAX_SAMPLE_SIZE,
};
