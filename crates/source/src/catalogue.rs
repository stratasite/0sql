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

// ---------------------------------------------------------------------- read

use crate::{Result, Source, SourceError};

/// Caps on a catalogue read. A warehouse with more tables or columns than
/// this is refused rather than half-read, so a generated model is never
/// silently missing a schema.
pub const MAX_TABLES: usize = 20_000;
pub const MAX_COLUMNS: usize = 200_000;

/// The schemas no model is built from: a warehouse's own bookkeeping.
const SYSTEM_SCHEMAS: [&str; 5] = ["pg_catalog", "information_schema", "INFORMATION_SCHEMA", "system", "pg_toast"];

impl Catalogue {
    /// Reads what `src` holds. `scope` narrows the read to one schema —
    /// `schema:` in `datasources.yml`, or ClickHouse's database — and
    /// without one every schema but the system ones is read. The queries
    /// are per adapter; the shape they fill is not.
    pub fn read(src: &mut dyn Source, datasource: &str, scope: Option<&str>) -> Result<Catalogue> {
        let adapter = src.adapter().to_string();
        let tables = match adapter.as_str() {
            "postgres" | "redshift" => postgres(src, scope)?,
            "clickhouse" => clickhouse(src, scope)?,
            "sqlite" => sqlite(src)?,
            other => return Err(SourceError::Unsupported(other.to_string())),
        };
        Ok(Catalogue {
            datasource: datasource.to_string(),
            adapter,
            read_at: now(),
            read_by: env!("CARGO_PKG_VERSION").to_string(),
            tables,
        })
    }
}

/// The two reads every SQL adapter shares in shape: one row per table,
/// one row per column, joined here by schema and table name. Each adapter
/// supplies the statements; the column names they must answer with are
/// fixed, so the assembly is written once.
fn assemble(src: &mut dyn Source, tables_sql: &str, columns_sql: &str) -> Result<Vec<Table>> {
    let t = src.query(tables_sql, MAX_TABLES)?;
    let mut tables: Vec<Table> = t
        .rows
        .iter()
        .map(|r| Table {
            schema: t.get(r, "table_schema").map(str::to_string),
            name: t.get(r, "table_name").unwrap_or_default().to_string(),
            kind: t.get(r, "table_kind").unwrap_or("table").to_lowercase(),
            rows: t.get(r, "row_estimate").and_then(|n| n.parse::<f64>().ok()).map(|n| n.max(0.0) as i64).filter(|n| *n > 0),
            comment: t.get(r, "table_comment").map(str::to_string).filter(|c| !c.trim().is_empty()),
            columns: vec![],
        })
        .collect();
    let c = src.query(columns_sql, MAX_COLUMNS)?;
    for r in &c.rows {
        let schema = c.get(r, "table_schema");
        let name = c.get(r, "table_name").unwrap_or_default();
        let Some(table) = tables.iter_mut().find(|t| t.name == name && t.schema.as_deref() == schema) else { continue };
        table.columns.push(Column {
            name: c.get(r, "column_name").unwrap_or_default().to_string(),
            data_type: c.get(r, "data_type").unwrap_or_default().to_string(),
            nullable: c.get(r, "is_nullable").map(|v| matches!(v, "YES" | "yes" | "1" | "true")).unwrap_or(true),
            max_length: c.get(r, "max_length").and_then(|n| n.parse().ok()),
            position: c.get(r, "position").and_then(|n| n.parse().ok()).unwrap_or(0),
            comment: c.get(r, "column_comment").map(str::to_string).filter(|c| !c.trim().is_empty()),
        });
    }
    for t in &mut tables {
        t.columns.sort_by_key(|c| c.position);
    }
    Ok(tables)
}

/// A schema predicate for a `WHERE`, over a column holding the schema
/// name: the scope when there is one, else everything but the system
/// schemas.
fn scope_clause(column: &str, scope: Option<&str>) -> String {
    match scope {
        Some(s) => format!("{column} = '{}'", s.replace('\'', "''")),
        None => {
            let list: Vec<String> = SYSTEM_SCHEMAS.iter().map(|s| format!("'{s}'")).collect();
            format!("{column} NOT IN ({})", list.join(", "))
        }
    }
}

/// `information_schema` for the shape, `pg_catalog` for what it lacks:
/// the planner's row estimate and the comments.
fn postgres(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let tables = format!(
        "SELECT n.nspname AS table_schema, c.relname AS table_name, \
         CASE c.relkind WHEN 'v' THEN 'view' WHEN 'm' THEN 'view' ELSE 'table' END AS table_kind, \
         c.reltuples::text AS row_estimate, obj_description(c.oid, 'pg_class') AS table_comment \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         WHERE c.relkind IN ('r', 'p', 'v', 'm', 'f') AND {} ORDER BY 1, 2",
        scope_clause("n.nspname", scope)
    );
    let columns = format!(
        "SELECT col.table_schema, col.table_name, col.column_name, col.data_type, col.is_nullable, \
         col.character_maximum_length::text AS max_length, col.ordinal_position::text AS position, \
         col_description(c.oid, col.ordinal_position::int) AS column_comment \
         FROM information_schema.columns col \
         JOIN pg_catalog.pg_namespace n ON n.nspname = col.table_schema \
         JOIN pg_catalog.pg_class c ON c.relnamespace = n.oid AND c.relname = col.table_name \
         WHERE {} ORDER BY 1, 2, col.ordinal_position",
        scope_clause("col.table_schema", scope)
    );
    assemble(src, &tables, &columns)
}

/// `system.tables` and `system.columns`, which carry comments and a row
/// count that `INFORMATION_SCHEMA` here does not. ClickHouse has no
/// nullable flag; the type says `Nullable(…)`. `FixedString(N)` is the
/// only bounded string, so `max_length` is taken from it and nothing else.
fn clickhouse(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let tables = format!(
        "SELECT database AS table_schema, name AS table_name, \
         if(engine LIKE '%View', 'view', 'table') AS table_kind, \
         toString(total_rows) AS row_estimate, comment AS table_comment \
         FROM system.tables WHERE {} ORDER BY 1, 2",
        scope_clause("database", scope)
    );
    let columns = format!(
        "SELECT database AS table_schema, table AS table_name, name AS column_name, type AS data_type, \
         if(startsWith(type, 'Nullable('), 'YES', 'NO') AS is_nullable, \
         if(startsWith(type, 'FixedString('), extract(type, '\\\\((\\\\d+)\\\\)'), '') AS max_length, \
         toString(position) AS position, comment AS column_comment \
         FROM system.columns WHERE {} ORDER BY 1, 2, position",
        scope_clause("database", scope)
    );
    assemble(src, &tables, &columns)
}

/// `sqlite_master` for the tables and `PRAGMA table_info` per table for
/// the columns. No schema, no comments, no row estimate: SQLite keeps none.
fn sqlite(src: &mut dyn Source) -> Result<Vec<Table>> {
    let t = src.query(
        "SELECT name, type FROM sqlite_master WHERE type IN ('table', 'view') AND name NOT LIKE 'sqlite_%' ORDER BY name",
        MAX_TABLES,
    )?;
    let mut tables = Vec::new();
    for r in &t.rows {
        let name = t.get(r, "name").unwrap_or_default().to_string();
        let c = src.query(&format!("PRAGMA table_info(\"{}\")", name.replace('"', "\"\"")), MAX_COLUMNS)?;
        let columns = c
            .rows
            .iter()
            .map(|r| Column {
                name: c.get(r, "name").unwrap_or_default().to_string(),
                data_type: c.get(r, "type").unwrap_or_default().to_string(),
                nullable: c.get(r, "notnull") != Some("1"),
                max_length: None,
                position: c.get(r, "cid").and_then(|n| n.parse::<i64>().ok()).map(|n| n + 1).unwrap_or(0),
                comment: None,
            })
            .collect();
        tables.push(Table {
            schema: None,
            name,
            kind: t.get(r, "type").unwrap_or("table").to_string(),
            rows: None,
            comment: None,
            columns,
        });
    }
    Ok(tables)
}

/// Now, RFC 3339 to the second, with no date crate: civil-from-days.
fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs() as i64).unwrap_or(0);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = if m <= 2 { y + 1 } else { y };
    format!("{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z", rem / 3600, (rem % 3600) / 60, rem % 60)
}
