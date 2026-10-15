use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::Read;
use std::path::Path as FsPath;

/// The snapshot shape this build writes, and the newest it can read.
///
/// Raise it in the same change that makes a snapshot a reader of the
/// previous number could not understand: a field added without a serde
/// default, or one renamed or removed, since every struct here is
/// `deny_unknown_fields`. Adding a field that defaults is not such a
/// change, and does not need a new number.
pub const FORMAT: u32 = 1;

fn first_format() -> u32 {
    1
}

/// Row id as assigned by the strata database. Kept verbatim so a branch and
/// its clone stay distinguishable and so paths can reference joins and tables.
pub type Id = i64;

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Snapshot {
    /// The shape this snapshot was written in, so a build that cannot
    /// read one can say so instead of reporting a missing field.
    /// Snapshots written before the field existed have no `version` key
    /// and are version 1, which is what the default is for.
    #[serde(default = "first_format")]
    pub version: u32,
    pub project: ProjectRef,
    pub branch: BranchRef,
    pub datasources: Vec<Datasource>,
    pub tables: Vec<Table>,
    pub fields: Vec<Field>,
    pub expressions: Vec<Expression>,
    pub join_defs: Vec<JoinDef>,
    /// Join paths as enumerated by Rails' formation engine. Oracle only.
    pub paths: Vec<Path>,
    pub partitions: Vec<Partition>,
    pub security_policies: Vec<SecurityPolicy>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ProjectRef {
    pub uid: String,
    pub name: String,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BranchRef {
    pub id: Id,
    pub uid: String,
    pub production: bool,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
pub enum FieldKind {
    #[serde(rename = "Semantic::Dimension")]
    Dimension,
    #[serde(rename = "Semantic::Measure")]
    Measure,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DataType {
    String,
    Integer,
    Decimal,
    Date,
    DateTime,
    Boolean,
    Bigint,
    Binary,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, PartialOrd, Ord, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Tier {
    Hot,
    Warm,
    Cold,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Cardinality {
    OneToMany,
    ManyToOne,
    OneToOne,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum JoinType {
    Inner,
    Left,
    Right,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionType {
    Exclude,
    ExcludeAllExcept,
    ExcludeAll,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExclusionScope {
    Table,
    Dimension,
    Universe,
}

/// How partition and exclusion filters are treated inside a rule.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum FilterMode {
    Ignore,
    Only,
    Apply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SnapshotKind {
    Ending,
    Beginning,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PolicyMode {
    MaskData,
    FilterData,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionSource {
    Groups,
    User,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionValueFrom {
    FromName,
    FromTag,
    FromEmail,
}

/// Filter predicates, shared by query filters and table partitions.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Predicate {
    Equals,
    DoesNotEqual,
    GreaterThan,
    GreaterThanOrEqualTo,
    LessThan,
    LessThanOrEqualTo,
    Between,
    InList,
    ExcludeList,
    Contains,
    DoesNotContain,
    StartsWith,
    DoesNotStartWith,
    EndsWith,
    DoesNotEndWith,
    IsNull,
    IsNotNull,
    TopN,
    Keyword,
    Custom,
}

impl Predicate {
    pub fn name(self) -> &'static str {
        match self {
            Predicate::Equals => "equals",
            Predicate::DoesNotEqual => "does_not_equal",
            Predicate::GreaterThan => "greater_than",
            Predicate::GreaterThanOrEqualTo => "greater_than_or_equal_to",
            Predicate::LessThan => "less_than",
            Predicate::LessThanOrEqualTo => "less_than_or_equal_to",
            Predicate::Between => "between",
            Predicate::InList => "in_list",
            Predicate::ExcludeList => "exclude_list",
            Predicate::Contains => "contains",
            Predicate::DoesNotContain => "does_not_contain",
            Predicate::StartsWith => "starts_with",
            Predicate::DoesNotStartWith => "does_not_start_with",
            Predicate::EndsWith => "ends_with",
            Predicate::DoesNotEndWith => "does_not_end_with",
            Predicate::IsNull => "is_null",
            Predicate::IsNotNull => "is_not_null",
            Predicate::TopN => "top_n",
            Predicate::Keyword => "keyword",
            Predicate::Custom => "custom",
        }
    }
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Datasource {
    pub id: Id,
    pub uid: String,
    pub name: String,
    /// dwh adapter name; selects the dialect settings file.
    pub adapter: String,
    pub tier: Option<Tier>,
    pub description: Option<String>,
    #[serde(default)]
    pub extra_query_params: Value,
    pub query_timeout: Option<i64>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Table {
    pub id: Id,
    pub uid: String,
    pub name: String,
    pub physical_name: String,
    pub datasource_id: Id,
    pub cost: i64,
    pub snapshot_date_id: Option<Id>,
    #[serde(default)]
    pub tags: Vec<String>,
    pub source: Option<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub clear_cache_on_data_update: bool,
    pub data_updated_at: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Field {
    pub id: Id,
    pub uid: String,
    pub name: String,
    #[serde(rename = "type")]
    pub kind: FieldKind,
    pub data_type: Option<DataType>,
    pub display_type: String,
    #[serde(default)]
    pub hidden: bool,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub grains: Vec<String>,
    pub format: Option<Value>,
    pub exclusion_type: Option<ExclusionType>,
    #[serde(default)]
    pub exclusions: Vec<Exclusion>,
    pub inclusions: Option<Inclusion>,
    pub extended_blend_group: Option<String>,
    pub snapshot: Option<SnapshotKind>,
    #[serde(default)]
    pub secure: bool,
    #[serde(default)]
    pub disable_listing: bool,
    pub value_list_size: i64,
    /// How `zsql values sample` treats the field: the top `value_list_size`
    /// values by row count, every value, or none.
    #[serde(default)]
    pub value_sampling: ValueSampling,
    #[serde(default)]
    pub synonyms: Vec<String>,
    pub description: Option<String>,
    #[serde(default)]
    pub rank: i64,
}

impl Field {
    pub fn is_measure(&self) -> bool {
        self.kind == FieldKind::Measure
    }
    pub fn is_dimension(&self) -> bool {
        self.kind == FieldKind::Dimension
    }
    pub fn is_date(&self) -> bool {
        matches!(self.data_type, Some(DataType::Date) | Some(DataType::DateTime))
    }
}

#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ValueSampling {
    #[default]
    Sample,
    Full,
    None,
}

/// A measure exclusion rule: which tables, dimensions or universes the
/// measure must not be grouped or filtered by.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Exclusion {
    #[serde(rename = "type")]
    pub scope: ExclusionScope,
    pub filter: FilterMode,
    #[serde(default)]
    pub entities: Vec<String>,
    /// Resolved dimension ids, present only once Rails validated the rule.
    pub dimension_ids: Option<Vec<Id>>,
}

/// A measure inclusion rule: compute at a lower grain, then roll up.
#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Inclusion {
    pub filter: FilterMode,
    pub aggregation: String,
    #[serde(default)]
    pub dimensions: Vec<String>,
    pub dimension_ids: Option<Vec<Id>>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Expression {
    pub id: Id,
    pub table_id: Id,
    pub field_id: Id,
    pub sql: Option<String>,
    #[serde(default)]
    pub lookup: bool,
    #[serde(default)]
    pub primary_key: bool,
    #[serde(default)]
    pub array: bool,
    #[serde(default)]
    pub column_refs: Vec<String>,
    #[serde(default)]
    pub dimension_ref_ids: Vec<Id>,
    #[serde(default)]
    pub measure_ref_ids: Vec<Id>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct JoinDef {
    pub id: Id,
    pub datasource_id: Id,
    pub left_id: Id,
    pub right_id: Id,
    pub sql: String,
    pub cardinality: Cardinality,
    pub join_type: JoinType,
    pub cost: i64,
    #[serde(default)]
    pub allow_measure_expansion: bool,
    pub source: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Path {
    pub id: Id,
    pub root_table_id: Id,
    pub target_table_id: Id,
    pub from_table_id: Option<Id>,
    pub field_id: Id,
    pub field_type: FieldKind,
    pub step_number: Option<i64>,
    pub cost: i64,
    pub tier: Option<i64>,
    #[serde(default)]
    pub joins: Vec<Id>,
    #[serde(default)]
    pub table_nodes: Vec<Id>,
    pub datasource_id: Id,
    pub blending_field_id: Option<Id>,
    pub extended_blend_group: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct Partition {
    pub id: Id,
    pub table_id: Id,
    pub dimension_id: Id,
    pub predicate: Predicate,
    pub filter_value: Option<String>,
    pub filter_value_end: Option<String>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub struct SecurityPolicy {
    pub uid: String,
    pub name: String,
    pub mode: PolicyMode,
    pub mask_value: Option<String>,
    pub context_dimension_uid: String,
    #[serde(default)]
    pub trigger_tags: Vec<String>,
    #[serde(default)]
    pub trigger_field_uids: Vec<String>,
    pub permission_source: PermissionSource,
    pub permission_value_from: PermissionValueFrom,
    pub permission_tag_key: Option<String>,
    #[serde(default = "default_true")]
    pub bypass_system_admin: bool,
    #[serde(default)]
    pub bypass_project_admin: bool,
    /// What a fired policy does when the context yields no allowed values.
    #[serde(default)]
    pub unresolved: Unresolved,
}

/// The outcome of a fired policy whose permission resolution found nothing
/// for the user: no group carried the tag, no email was given.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq, Hash, Deserialize, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Unresolved {
    /// The user sees nothing: rows filtered to none, or every value masked.
    #[default]
    Deny,
    /// The policy is skipped for the user (Rails' rule).
    Allow,
}

fn default_true() -> bool {
    true
}

#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("io: {0}")]
    Io(#[from] std::io::Error),
    #[error("json: {0}")]
    Json(#[from] serde_json::Error),
    #[error("this snapshot is in format {found}, and this build reads up to {FORMAT}; deploy it again from its project")]
    Newer { found: u32 },
}

impl Snapshot {
    /// Reads a snapshot from JSON, gzip-compressed when the path ends in `.gz`.
    pub fn from_path(path: &FsPath) -> Result<Self, LoadError> {
        let file = std::fs::File::open(path)?;
        let is_gz = path.extension().and_then(|e| e.to_str()) == Some("gz");
        let mut text = String::new();
        if is_gz {
            flate2::read::GzDecoder::new(file).read_to_string(&mut text)?;
        } else {
            std::io::BufReader::new(file).read_to_string(&mut text)?;
        }
        Self::from_str(&text)
    }

    /// Not `FromStr`: the error is this crate's, and the name reads as
    /// the pair of `from_path`.
    #[allow(clippy::should_implement_trait)]
    pub fn from_str(text: &str) -> Result<Self, LoadError> {
        match serde_json::from_str(text) {
            Ok(s) => Ok(s),
            // A snapshot this build is too old for fails on whatever
            // field moved, which says nothing useful. Its `version` is
            // readable on its own, so ask that first and report the
            // real reason when it is the reason.
            Err(e) => match Self::format_of(text) {
                Some(found) if found > FORMAT => Err(LoadError::Newer { found }),
                _ => Err(e.into()),
            },
        }
    }

    /// The `version` of a snapshot, read without parsing the rest, so a
    /// snapshot that cannot be loaded can still be identified.
    fn format_of(text: &str) -> Option<u32> {
        #[derive(Deserialize)]
        struct JustTheVersion {
            #[serde(default = "first_format")]
            version: u32,
        }
        serde_json::from_str::<JustTheVersion>(text).ok().map(|v| v.version)
    }
}
