//! What a warehouse says it holds: the shape a model is generated from
//! and checked against.
//!
//! Written to `catalogue.json` in the project, committed rather than
//! gitignored, because it is schema and not customer data and a diff in
//! review is where drift wants to be seen. The queries that fill it are
//! per-adapter; the shape is not.

use serde::{Deserialize, Serialize};

/// A datasource's tables and columns, as read at one moment.
#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Catalogue {
    /// The `datasources.yml` key this was read from.
    pub datasource: String,
    pub adapter: String,
    /// RFC 3339, so two catalogues can be diffed and dated.
    pub read_at: String,
    /// The version of `zsql` that read it: a catalogue outlives a build.
    pub read_by: String,
    pub tables: Vec<Table>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Table {
    pub schema: Option<String>,
    pub name: String,
    /// `table` or `view`, as the warehouse classifies it.
    pub kind: String,
    /// Approximate, from the warehouse's own statistics; never a count.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rows: Option<i64>,
    /// The table's own comment, where the warehouse keeps one.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
    pub columns: Vec<Column>,
}

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct Column {
    pub name: String,
    /// The warehouse's own spelling, not 0sql's `data_type`. Mapping the
    /// two is generation's job, and keeping the original is what lets a
    /// mapping be corrected later without re-reading the warehouse.
    pub data_type: String,
    pub nullable: bool,
    /// Declared character length: `varchar(64)` is 64, `text` is None.
    /// This is the signal behind the sampling guidance in a project's
    /// AGENTS.md, and the one the sampler's long-text warning should
    /// eventually read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub max_length: Option<i64>,
    /// Ordinal position, so a generated model keeps the warehouse's order.
    pub position: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub comment: Option<String>,
}

impl Table {
    /// The name a model's `physical_name` would carry: qualified by schema
    /// when there is one.
    pub fn physical_name(&self) -> String {
        match &self.schema {
            Some(s) => format!("{s}.{}", self.name),
            None => self.name.clone(),
        }
    }
}

impl Catalogue {
    pub fn table(&self, schema: Option<&str>, name: &str) -> Option<&Table> {
        self.tables.iter().find(|t| t.name == name && t.schema.as_deref() == schema)
    }
}
