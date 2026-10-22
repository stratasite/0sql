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
    /// The primary key's columns in order, when one is declared. On
    /// ClickHouse this is the sorting key, which is what it calls its
    /// primary key and is not a uniqueness promise.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub primary_key: Vec<String>,
    /// Declared foreign keys, which is where join inference starts. The
    /// warehouses that enforce them (Postgres, MySQL, SQL Server, SQLite)
    /// answer here; the ones that only record them as hints will when
    /// phase E reaches them; Trino and ClickHouse have none to give.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub foreign_keys: Vec<ForeignKey>,
}

/// One foreign key: these columns reference those, on that table.
#[derive(Debug, Clone, Default, Deserialize, Serialize, PartialEq, Eq)]
pub struct ForeignKey {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    pub columns: Vec<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub references_schema: Option<String>,
    pub references_table: String,
    pub references_columns: Vec<String>,
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

use crate::{Result, Rows, Source, SourceError};

/// Caps on a catalogue read. A warehouse with more tables or columns than
/// this is refused rather than half-read, so a generated model is never
/// silently missing a schema.
pub const MAX_TABLES: usize = 20_000;
pub const MAX_COLUMNS: usize = 200_000;

/// The schemas no model is built from: a warehouse's own bookkeeping.
const SYSTEM_SCHEMAS: [&str; 9] =
    ["pg_catalog", "information_schema", "INFORMATION_SCHEMA", "system", "pg_toast", "mysql", "performance_schema", "sys", "guest"];

impl Catalogue {
    /// Reads what `src` holds. `scope` narrows the read to one schema —
    /// `schema:` in `datasources.yml`, or ClickHouse's database — and
    /// without one every schema but the system ones is read. The queries
    /// are per adapter; the shape they fill is not.
    pub fn read(src: &mut dyn Source, datasource: &str, scope: Option<&str>) -> Result<Catalogue> {
        let adapter = src.adapter().to_string();
        let tables = match adapter.as_str() {
            "postgres" => postgres(src, scope)?,
            "redshift" => redshift(src, scope)?,
            "clickhouse" => clickhouse(src, scope)?,
            "mysql" => mysql(src, scope)?,
            "sqlserver" => sqlserver(src, scope)?,
            "trino" => trino(src, scope)?,
            "athena" => athena(src, scope)?,
            "snowflake" => snowflake(src, scope)?,
            "databricks" => databricks(src, scope)?,
            "bigquery" => bigquery(src, scope)?,
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
            primary_key: vec![],
            foreign_keys: vec![],
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

/// The third shared read: one row per key column. `key_kind` is
/// `primary` or `foreign`; a foreign row also names what it references.
/// Rows arrive in any order and are grouped here by table, kind and
/// constraint name, each group's columns put in `position` order.
fn keys(src: &mut dyn Source, tables: &mut [Table], keys_sql: &str) -> Result<()> {
    let k = src.query(keys_sql, MAX_COLUMNS)?;
    apply_keys(tables, &k);
    Ok(())
}

/// The key rows applied to the tables, for an adapter whose keys arrive
/// in that shape from somewhere other than one statement.
pub(crate) fn apply_keys(tables: &mut [Table], k: &Rows) {
    // (schema, table, kind, name) -> rows, keeping first-seen order.
    let mut groups: Vec<((Option<String>, String, String, Option<String>), Vec<&Vec<Option<String>>>)> = Vec::new();
    for r in &k.rows {
        let id = (
            k.get(r, "table_schema").map(str::to_string),
            k.get(r, "table_name").unwrap_or_default().to_string(),
            k.get(r, "key_kind").unwrap_or("primary").to_lowercase(),
            k.get(r, "key_name").map(str::to_string),
        );
        match groups.iter_mut().find(|(g, _)| *g == id) {
            Some((_, rows)) => rows.push(r),
            None => groups.push((id, vec![r])),
        }
    }
    for ((schema, name, kind, key_name), mut rows) in groups {
        rows.sort_by_key(|r| k.get(r, "position").and_then(|n| n.parse::<i64>().ok()).unwrap_or(0));
        let Some(table) = tables.iter_mut().find(|t| t.name == name && t.schema.as_deref() == schema.as_deref()) else { continue };
        let columns: Vec<String> = rows.iter().filter_map(|r| k.get(r, "column_name")).map(str::to_string).collect();
        if kind == "foreign" {
            let first = rows[0];
            table.foreign_keys.push(ForeignKey {
                name: key_name,
                columns,
                references_schema: k.get(first, "ref_schema").map(str::to_string),
                references_table: k.get(first, "ref_table").unwrap_or_default().to_string(),
                references_columns: rows.iter().filter_map(|r| k.get(r, "ref_column")).map(str::to_string).collect(),
            });
        } else if table.primary_key.is_empty() {
            table.primary_key = columns;
        }
    }
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
    let mut tables = assemble(src, &tables, &columns)?;
    // pg_constraint holds the key columns as attribute numbers; unnest
    // them with their ordinality so each row is one column in order, and
    // for a foreign key the referenced column at the same ordinal.
    let keys_sql = format!(
        "SELECT n.nspname AS table_schema, c.relname AS table_name, \
         CASE con.contype WHEN 'p' THEN 'primary' ELSE 'foreign' END AS key_kind, con.conname AS key_name, \
         a.attname AS column_name, k.ord::text AS position, \
         rn.nspname AS ref_schema, rc.relname AS ref_table, ra.attname AS ref_column \
         FROM pg_catalog.pg_constraint con \
         JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         CROSS JOIN LATERAL unnest(con.conkey) WITH ORDINALITY AS k(attnum, ord) \
         JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum = k.attnum \
         LEFT JOIN pg_catalog.pg_class rc ON rc.oid = con.confrelid \
         LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace \
         LEFT JOIN pg_catalog.pg_attribute ra ON ra.attrelid = con.confrelid AND ra.attnum = con.confkey[k.ord] \
         WHERE con.contype IN ('p', 'f') AND {} ORDER BY 1, 2, 4, k.ord",
        scope_clause("n.nspname", scope)
    );
    keys(src, &mut tables, &keys_sql)?;
    Ok(tables)
}

/// The three statements of a Redshift read. Redshift is Postgres 8 with
/// its own bookkeeping: `reltuples` is not maintained there, so the row
/// count is `svv_table_info`'s; `information_schema.columns` does not see
/// a late-binding view, so the columns come from `pg_attribute`; and
/// there is no `unnest … WITH ORDINALITY`, so a constraint's attribute
/// numbers arrive as the array's text and are expanded here against the
/// columns already read. Every statement but the first is also valid
/// Postgres, which is how two of the three are proven on the fixture.
pub(crate) fn redshift_sql(scope: Option<&str>) -> (String, String, String) {
    let tables = format!(
        "SELECT n.nspname AS table_schema, c.relname AS table_name, \
         CASE c.relkind WHEN 'v' THEN 'view' ELSE 'table' END AS table_kind, \
         CAST(t.tbl_rows AS VARCHAR) AS row_estimate, d.description AS table_comment \
         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN svv_table_info t ON t.schema = n.nspname AND t.\"table\" = c.relname \
         LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = 0 \
         WHERE c.relkind IN ('r', 'v') AND {} ORDER BY 1, 2",
        scope_clause("n.nspname", scope)
    );
    let columns = format!(
        "SELECT n.nspname AS table_schema, c.relname AS table_name, a.attname AS column_name, \
         format_type(a.atttypid, a.atttypmod) AS data_type, \
         CASE WHEN a.attnotnull THEN 'NO' ELSE 'YES' END AS is_nullable, \
         CAST(CASE WHEN a.atttypid IN (1042, 1043) AND a.atttypmod > 4 THEN a.atttypmod - 4 END AS VARCHAR) AS max_length, \
         CAST(a.attnum AS VARCHAR) AS position, d.description AS column_comment \
         FROM pg_catalog.pg_attribute a \
         JOIN pg_catalog.pg_class c ON c.oid = a.attrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_description d ON d.objoid = c.oid AND d.objsubid = a.attnum \
         WHERE a.attnum > 0 AND NOT a.attisdropped AND c.relkind IN ('r', 'v') AND {} ORDER BY 1, 2, a.attnum",
        scope_clause("n.nspname", scope)
    );
    let keys = format!(
        "SELECT n.nspname AS table_schema, c.relname AS table_name, \
         CASE con.contype WHEN 'p' THEN 'primary' ELSE 'foreign' END AS key_kind, con.conname AS key_name, \
         CAST(con.conkey AS VARCHAR) AS key_attnums, \
         rn.nspname AS ref_schema, rc.relname AS ref_table, CAST(con.confkey AS VARCHAR) AS ref_attnums \
         FROM pg_catalog.pg_constraint con \
         JOIN pg_catalog.pg_class c ON c.oid = con.conrelid \
         JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace \
         LEFT JOIN pg_catalog.pg_class rc ON rc.oid = con.confrelid \
         LEFT JOIN pg_catalog.pg_namespace rn ON rn.oid = rc.relnamespace \
         WHERE con.contype IN ('p', 'f') AND {} ORDER BY 1, 2, 4",
        scope_clause("n.nspname", scope)
    );
    (tables, columns, keys)
}

fn redshift(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let (tables_sql, columns_sql, keys_sql) = redshift_sql(scope);
    let mut tables = assemble(src, &tables_sql, &columns_sql)?;
    let k = src.query(&keys_sql, MAX_COLUMNS)?;
    let expanded = expand_attnums(&tables, &k);
    apply_keys(&mut tables, &expanded);
    Ok(tables)
}

/// Key rows with attribute-number arrays (`{1,3}`) turned into one row per
/// column in the shared shape, the names looked up by position among the
/// columns already read; a foreign key pairs each with the referenced
/// array's entry at the same index.
pub(crate) fn expand_attnums(tables: &[Table], k: &Rows) -> Rows {
    let nums = |s: Option<&str>| -> Vec<i64> {
        s.unwrap_or_default().trim_matches(|c| c == '{' || c == '}').split(',').filter_map(|n| n.trim().parse().ok()).collect()
    };
    let name_at = |schema: Option<&str>, table: &str, attnum: i64| -> Option<String> {
        tables
            .iter()
            .find(|t| t.name == table && t.schema.as_deref() == schema)?
            .columns
            .iter()
            .find(|c| c.position == attnum)
            .map(|c| c.name.clone())
    };
    let mut out = Rows {
        columns: ["table_schema", "table_name", "key_kind", "key_name", "column_name", "position", "ref_schema", "ref_table", "ref_column"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        rows: vec![],
    };
    for r in &k.rows {
        let g = |c: &str| k.get(r, c).map(str::to_string);
        let (schema, table) = (g("table_schema"), g("table_name").unwrap_or_default());
        let (ref_schema, ref_table) = (g("ref_schema"), g("ref_table"));
        let refs = nums(k.get(r, "ref_attnums"));
        for (i, attnum) in nums(k.get(r, "key_attnums")).into_iter().enumerate() {
            let Some(column) = name_at(schema.as_deref(), &table, attnum) else { continue };
            let ref_column = match (&ref_table, refs.get(i)) {
                (Some(rt), Some(n)) => name_at(ref_schema.as_deref(), rt, *n),
                _ => None,
            };
            out.rows.push(vec![
                schema.clone(),
                Some(table.clone()),
                g("key_kind"),
                g("key_name"),
                Some(column),
                Some((i + 1).to_string()),
                ref_schema.clone(),
                ref_table.clone(),
                ref_column,
            ]);
        }
    }
    out
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
    let mut tables = assemble(src, &tables, &columns)?;
    // system.tables.primary_key is one expression string, "a, b"; split
    // it into its columns. ClickHouse has no foreign keys.
    let keys_sql = format!(
        "SELECT database AS table_schema, name AS table_name, 'primary' AS key_kind, '' AS key_name, \
         trim(col) AS column_name, toString(pos) AS position, '' AS ref_schema, '' AS ref_table, '' AS ref_column \
         FROM system.tables ARRAY JOIN splitByChar(',', primary_key) AS col, arrayEnumerate(splitByChar(',', primary_key)) AS pos \
         WHERE primary_key != '' AND {} ORDER BY 1, 2, pos",
        scope_clause("database", scope)
    );
    keys(src, &mut tables, &keys_sql)?;
    Ok(tables)
}

/// `information_schema` has everything MySQL keeps: the type, the length,
/// the comment on both table and column, and an estimate of rows. A
/// schema is a database here, so the scope is the connection's database.
/// Every column is aliased, because MySQL answers an unaliased
/// `information_schema` column under its defined, upper-case name.
fn mysql(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let tables = format!(
        "SELECT table_schema AS table_schema, table_name AS table_name, \
         IF(table_type = 'VIEW', 'view', 'table') AS table_kind, \
         CAST(table_rows AS CHAR) AS row_estimate, table_comment AS table_comment \
         FROM information_schema.tables WHERE {} ORDER BY 1, 2",
        scope_clause("table_schema", scope)
    );
    let columns = format!(
        "SELECT table_schema AS table_schema, table_name AS table_name, column_name AS column_name, \
         data_type AS data_type, is_nullable AS is_nullable, \
         CAST(character_maximum_length AS CHAR) AS max_length, CAST(ordinal_position AS CHAR) AS position, \
         column_comment AS column_comment FROM information_schema.columns WHERE {} ORDER BY 1, 2, ordinal_position",
        scope_clause("table_schema", scope)
    );
    let mut tables = assemble(src, &tables, &columns)?;
    let keys_sql = format!(
        "SELECT kcu.table_schema AS table_schema, kcu.table_name AS table_name, \
         IF(tc.constraint_type = 'PRIMARY KEY', 'primary', 'foreign') AS key_kind, kcu.constraint_name AS key_name, \
         kcu.column_name AS column_name, CAST(kcu.ordinal_position AS CHAR) AS position, \
         kcu.referenced_table_schema AS ref_schema, kcu.referenced_table_name AS ref_table, kcu.referenced_column_name AS ref_column \
         FROM information_schema.key_column_usage kcu \
         JOIN information_schema.table_constraints tc \
           ON tc.constraint_schema = kcu.constraint_schema AND tc.constraint_name = kcu.constraint_name \
          AND tc.table_schema = kcu.table_schema AND tc.table_name = kcu.table_name \
         WHERE tc.constraint_type IN ('PRIMARY KEY', 'FOREIGN KEY') AND {} ORDER BY 1, 2, 4, kcu.ordinal_position",
        scope_clause("kcu.table_schema", scope)
    );
    keys(src, &mut tables, &keys_sql)?;
    Ok(tables)
}

/// `INFORMATION_SCHEMA` for the shape, `sys` for the rest: the row count
/// from the partitions of the heap or clustered index, and comments from
/// the `MS_Description` extended property, which is where SQL Server keeps
/// a comment when anyone has written one.
fn sqlserver(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let tables = format!(
        "SELECT s.name AS table_schema, o.name AS table_name, \
         CASE o.type WHEN 'V' THEN 'view' ELSE 'table' END AS table_kind, \
         CAST((SELECT SUM(p.rows) FROM sys.partitions p WHERE p.object_id = o.object_id AND p.index_id IN (0, 1)) AS VARCHAR(20)) AS row_estimate, \
         CAST(ep.value AS NVARCHAR(4000)) AS table_comment \
         FROM sys.objects o JOIN sys.schemas s ON s.schema_id = o.schema_id \
         LEFT JOIN sys.extended_properties ep ON ep.major_id = o.object_id AND ep.minor_id = 0 AND ep.name = 'MS_Description' \
         WHERE o.type IN ('U', 'V') AND {} ORDER BY 1, 2",
        scope_clause("s.name", scope)
    );
    let columns = format!(
        "SELECT c.TABLE_SCHEMA AS table_schema, c.TABLE_NAME AS table_name, c.COLUMN_NAME AS column_name, \
         c.DATA_TYPE AS data_type, c.IS_NULLABLE AS is_nullable, \
         CAST(c.CHARACTER_MAXIMUM_LENGTH AS VARCHAR(20)) AS max_length, CAST(c.ORDINAL_POSITION AS VARCHAR(20)) AS position, \
         CAST(ep.value AS NVARCHAR(4000)) AS column_comment \
         FROM INFORMATION_SCHEMA.COLUMNS c \
         JOIN sys.schemas s ON s.name = c.TABLE_SCHEMA \
         JOIN sys.objects o ON o.schema_id = s.schema_id AND o.name = c.TABLE_NAME \
         LEFT JOIN sys.columns sc ON sc.object_id = o.object_id AND sc.name = c.COLUMN_NAME \
         LEFT JOIN sys.extended_properties ep ON ep.major_id = o.object_id AND ep.minor_id = sc.column_id AND ep.name = 'MS_Description' \
         WHERE {} ORDER BY 1, 2, c.ORDINAL_POSITION",
        scope_clause("c.TABLE_SCHEMA", scope)
    );
    let mut tables = assemble(src, &tables, &columns)?;
    let keys_sql = format!(
        "SELECT s.name AS table_schema, t.name AS table_name, 'primary' AS key_kind, kc.name AS key_name, \
         col.name AS column_name, CAST(ic.key_ordinal AS VARCHAR(10)) AS position, \
         NULL AS ref_schema, NULL AS ref_table, NULL AS ref_column \
         FROM sys.key_constraints kc \
         JOIN sys.tables t ON t.object_id = kc.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = t.schema_id \
         JOIN sys.index_columns ic ON ic.object_id = t.object_id AND ic.index_id = kc.unique_index_id \
         JOIN sys.columns col ON col.object_id = t.object_id AND col.column_id = ic.column_id \
         WHERE kc.type = 'PK' AND {} \
         UNION ALL \
         SELECT s.name, t.name, 'foreign', fk.name, pc.name, CAST(fkc.constraint_column_id AS VARCHAR(10)), \
         rs.name, rt.name, rc.name \
         FROM sys.foreign_keys fk \
         JOIN sys.tables t ON t.object_id = fk.parent_object_id \
         JOIN sys.schemas s ON s.schema_id = t.schema_id \
         JOIN sys.foreign_key_columns fkc ON fkc.constraint_object_id = fk.object_id \
         JOIN sys.columns pc ON pc.object_id = fkc.parent_object_id AND pc.column_id = fkc.parent_column_id \
         JOIN sys.tables rt ON rt.object_id = fk.referenced_object_id \
         JOIN sys.schemas rs ON rs.schema_id = rt.schema_id \
         JOIN sys.columns rc ON rc.object_id = fkc.referenced_object_id AND rc.column_id = fkc.referenced_column_id \
         WHERE {} ORDER BY 1, 2, 4, 6",
        scope_clause("s.name", scope),
        scope_clause("s.name", scope)
    );
    keys(src, &mut tables, &keys_sql)?;
    Ok(tables)
}

/// `information_schema` of the catalog the connection named, which the
/// transport sets as a header so the schema name alone reaches here. Trino
/// puts a column's comment in `information_schema.columns` and a table's
/// in `system.metadata.table_comments`; it has no cheap row estimate.
fn trino(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let tables = format!(
        "SELECT t.table_schema, t.table_name, IF(t.table_type = 'VIEW', 'view', 'table') AS table_kind, \
         CAST(NULL AS VARCHAR) AS row_estimate, c.comment AS table_comment \
         FROM information_schema.tables t \
         LEFT JOIN system.metadata.table_comments c \
           ON c.catalog_name = t.table_catalog AND c.schema_name = t.table_schema AND c.table_name = t.table_name \
         WHERE {} ORDER BY 1, 2",
        scope_clause("t.table_schema", scope)
    );
    let columns = format!(
        "SELECT table_schema, table_name, column_name, data_type, is_nullable, \
         CAST(NULL AS VARCHAR) AS max_length, CAST(ordinal_position AS VARCHAR) AS position, comment AS column_comment \
         FROM information_schema.columns WHERE {} ORDER BY 1, 2, ordinal_position",
        scope_clause("table_schema", scope)
    );
    assemble(src, &tables, &columns)
}

/// `INFORMATION_SCHEMA` of the session's database for the tables and
/// columns, comments included; `SHOW PRIMARY KEYS` and `SHOW IMPORTED
/// KEYS` for the constraints, since Snowflake's `INFORMATION_SCHEMA` has
/// no `KEY_COLUMN_USAGE`. Every column is aliased in quotes because an
/// unquoted alias comes back upper-cased, and the scope is folded the
/// way Snowflake folds an unquoted name.
fn snowflake(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let scope = scope.map(crate::snowflake::ident);
    let tables = format!(
        "SELECT table_schema AS \"table_schema\", table_name AS \"table_name\", \
         CASE WHEN table_type LIKE '%VIEW' THEN 'view' ELSE 'table' END AS \"table_kind\", \
         CAST(row_count AS VARCHAR) AS \"row_estimate\", comment AS \"table_comment\" \
         FROM INFORMATION_SCHEMA.TABLES \
         WHERE {} AND table_type IN ('BASE TABLE', 'VIEW', 'MATERIALIZED VIEW', 'EXTERNAL TABLE') ORDER BY 1, 2",
        scope_clause("table_schema", scope.as_deref())
    );
    let columns = format!(
        "SELECT table_schema AS \"table_schema\", table_name AS \"table_name\", column_name AS \"column_name\", \
         data_type AS \"data_type\", is_nullable AS \"is_nullable\", \
         CAST(character_maximum_length AS VARCHAR) AS \"max_length\", CAST(ordinal_position AS VARCHAR) AS \"position\", \
         comment AS \"column_comment\" \
         FROM INFORMATION_SCHEMA.COLUMNS WHERE {} ORDER BY 1, 2, ordinal_position",
        scope_clause("table_schema", scope.as_deref())
    );
    let mut tables = assemble(src, &tables, &columns)?;
    let within = match &scope {
        Some(s) => format!("IN SCHEMA \"{}\"", s.replace('"', "\"\"")),
        None => "IN DATABASE".to_string(),
    };
    let pk = src.query(&format!("SHOW PRIMARY KEYS {within}"), MAX_COLUMNS)?;
    let fk = src.query(&format!("SHOW IMPORTED KEYS {within}"), MAX_COLUMNS)?;
    let own = |s: &str| Some(s.to_string());
    let mut k = Rows {
        columns: ["table_schema", "table_name", "key_kind", "key_name", "column_name", "position", "ref_schema", "ref_table", "ref_column"]
            .iter()
            .map(|s| s.to_string())
            .collect(),
        rows: vec![],
    };
    for r in &pk.rows {
        let g = |c: &str| pk.get(r, c).map(str::to_string);
        k.rows.push(vec![
            g("schema_name"),
            g("table_name"),
            own("primary"),
            g("constraint_name"),
            g("column_name"),
            g("key_sequence"),
            None,
            None,
            None,
        ]);
    }
    for r in &fk.rows {
        let g = |c: &str| fk.get(r, c).map(str::to_string);
        k.rows.push(vec![
            g("fk_schema_name"),
            g("fk_table_name"),
            own("foreign"),
            g("fk_name"),
            g("fk_column_name"),
            g("key_sequence"),
            g("pk_schema_name"),
            g("pk_table_name"),
            g("pk_column_name"),
        ]);
    }
    apply_keys(&mut tables, &k);
    Ok(tables)
}

/// Athena's `information_schema`, which is Trino's without the comment
/// view: the column comments are there, a table's are not, and nothing
/// counts rows or declares a key. The scope is the Athena database.
fn athena(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let tables = format!(
        "SELECT table_schema, table_name, CASE WHEN table_type = 'VIEW' THEN 'view' ELSE 'table' END AS table_kind, \
         CAST(NULL AS VARCHAR) AS row_estimate, CAST(NULL AS VARCHAR) AS table_comment \
         FROM information_schema.tables WHERE {} ORDER BY 1, 2",
        scope_clause("table_schema", scope)
    );
    let columns = format!(
        "SELECT table_schema, table_name, column_name, data_type, is_nullable, \
         CAST(NULL AS VARCHAR) AS max_length, CAST(ordinal_position AS VARCHAR) AS position, comment AS column_comment \
         FROM information_schema.columns WHERE {} ORDER BY 1, 2, ordinal_position",
        scope_clause("table_schema", scope)
    );
    assemble(src, &tables, &columns)
}

/// Unity Catalog's `information_schema`, which the session's catalog
/// resolves: comments on tables and columns, widths, and the keys through
/// the standard four views, a foreign key's referenced columns paired by
/// `position_in_unique_constraint`.
fn databricks(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let tables = format!(
        "SELECT table_schema, table_name, \
         CASE WHEN table_type IN ('VIEW', 'MATERIALIZED_VIEW') THEN 'view' ELSE 'table' END AS table_kind, \
         CAST(NULL AS STRING) AS row_estimate, comment AS table_comment \
         FROM information_schema.tables WHERE {} ORDER BY 1, 2",
        scope_clause("table_schema", scope)
    );
    let columns = format!(
        "SELECT table_schema, table_name, column_name, full_data_type AS data_type, is_nullable, \
         CAST(character_maximum_length AS STRING) AS max_length, CAST(ordinal_position AS STRING) AS position, \
         comment AS column_comment \
         FROM information_schema.columns WHERE {} ORDER BY 1, 2, ordinal_position",
        scope_clause("table_schema", scope)
    );
    let mut tables = assemble(src, &tables, &columns)?;
    let keys_sql = format!("{} AND {}", standard_keys_sql("information_schema", false), scope_clause("k.table_schema", scope));
    keys(src, &mut tables, &keys_sql)?;
    Ok(tables)
}

/// The key read over the SQL standard's four views, for a warehouse that
/// has them with `position_in_unique_constraint`: Databricks and BigQuery.
/// `prefix` qualifies the views, spelled upper-case when `upper` says so,
/// as BigQuery's are; the caller's `AND` follows the `WHERE`.
fn standard_keys_sql(prefix: &str, upper: bool) -> String {
    let sql = format!(
        "SELECT k.table_schema, k.table_name, \
         CASE t.constraint_type WHEN 'PRIMARY KEY' THEN 'primary' ELSE 'foreign' END AS key_kind, \
         k.constraint_name AS key_name, k.column_name, CAST(k.ordinal_position AS STRING) AS position, \
         u.table_schema AS ref_schema, u.table_name AS ref_table, rk.column_name AS ref_column \
         FROM {prefix}.key_column_usage k \
         JOIN {prefix}.table_constraints t \
           ON t.constraint_schema = k.constraint_schema AND t.constraint_name = k.constraint_name \
         LEFT JOIN (SELECT DISTINCT constraint_schema, constraint_name, table_schema, table_name FROM {prefix}.constraint_column_usage) u \
           ON t.constraint_type = 'FOREIGN KEY' AND u.constraint_schema = k.constraint_schema AND u.constraint_name = k.constraint_name \
         LEFT JOIN {prefix}.table_constraints rt \
           ON rt.table_schema = u.table_schema AND rt.table_name = u.table_name AND rt.constraint_type = 'PRIMARY KEY' \
         LEFT JOIN {prefix}.key_column_usage rk \
           ON rk.constraint_schema = rt.constraint_schema AND rk.constraint_name = rt.constraint_name \
          AND rk.ordinal_position = k.position_in_unique_constraint \
         WHERE t.constraint_type IN ('PRIMARY KEY', 'FOREIGN KEY')"
    );
    if upper {
        ["key_column_usage", "table_constraints", "constraint_column_usage"]
            .iter()
            .fold(sql, |s, v| s.replace(&format!(".{v}"), &format!(".{}", v.to_uppercase())))
    } else {
        sql
    }
}

/// BigQuery's `INFORMATION_SCHEMA`, read for one dataset when the scope
/// names one and region-wide otherwise. A dataset read joins `__TABLES__`
/// for the row count, which the region has no view for. Descriptions
/// live in `TABLE_OPTIONS` as quoted literals and in `COLUMN_FIELD_PATHS`
/// for a column; a parameterised `STRING(n)` carries its width in the
/// type's spelling.
fn bigquery(src: &mut dyn Source, scope: Option<&str>) -> Result<Vec<Table>> {
    let conn = src.connection().ok_or_else(|| SourceError::Missing("bigquery".into(), "project"))?;
    let project = conn.project.clone().ok_or_else(|| SourceError::Missing(conn.uid.clone(), "project"))?;
    let (qualifier, rows_join, rows_expr) = match scope {
        Some(dataset) => (
            format!("`{project}.{dataset}`"),
            format!("LEFT JOIN `{project}.{dataset}.__TABLES__` s ON s.table_id = t.table_name"),
            "CAST(s.row_count AS STRING)",
        ),
        None => {
            let location = conn
                .location
                .clone()
                .ok_or_else(|| SourceError::Missing(conn.uid.clone(), "dataset, or location to read a whole region's catalogue"))?;
            (format!("`{project}.region-{location}`"), String::new(), "CAST(NULL AS STRING)")
        }
    };
    let tables = format!(
        "SELECT t.table_schema, t.table_name, CASE WHEN t.table_type LIKE '%VIEW' THEN 'view' ELSE 'table' END AS table_kind, \
         {rows_expr} AS row_estimate, TRIM(o.option_value, '\"') AS table_comment \
         FROM {qualifier}.INFORMATION_SCHEMA.TABLES t \
         LEFT JOIN {qualifier}.INFORMATION_SCHEMA.TABLE_OPTIONS o \
           ON o.table_schema = t.table_schema AND o.table_name = t.table_name AND o.option_name = 'description' \
         {rows_join} \
         WHERE {} ORDER BY 1, 2",
        scope_clause("t.table_schema", scope)
    );
    let columns = format!(
        "SELECT c.table_schema, c.table_name, c.column_name, c.data_type, c.is_nullable, \
         REGEXP_EXTRACT(c.data_type, r'^STRING\\((\\d+)\\)$') AS max_length, CAST(c.ordinal_position AS STRING) AS position, \
         f.description AS column_comment \
         FROM {qualifier}.INFORMATION_SCHEMA.COLUMNS c \
         LEFT JOIN {qualifier}.INFORMATION_SCHEMA.COLUMN_FIELD_PATHS f \
           ON f.table_schema = c.table_schema AND f.table_name = c.table_name AND f.field_path = c.column_name \
         WHERE {} ORDER BY 1, 2, c.ordinal_position",
        scope_clause("c.table_schema", scope)
    );
    let mut tables = assemble(src, &tables, &columns)?;
    let keys_sql =
        format!("{} AND {}", standard_keys_sql(&format!("{qualifier}.INFORMATION_SCHEMA"), true), scope_clause("k.table_schema", scope));
    keys(src, &mut tables, &keys_sql)?;
    Ok(tables)
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
        // table_info's pk column is the column's position in the primary
        // key, zero when it is not part of it.
        let mut pk: Vec<(i64, String)> = c
            .rows
            .iter()
            .filter_map(|r| {
                let pos = c.get(r, "pk")?.parse::<i64>().ok().filter(|p| *p > 0)?;
                Some((pos, c.get(r, "name")?.to_string()))
            })
            .collect();
        pk.sort();
        let f = src.query(&format!("PRAGMA foreign_key_list(\"{}\")", name.replace('"', "\"\"")), MAX_COLUMNS)?;
        let mut fks: Vec<(i64, ForeignKey)> = Vec::new();
        for r in &f.rows {
            let id: i64 = f.get(r, "id").and_then(|n| n.parse().ok()).unwrap_or(0);
            let entry = match fks.iter_mut().find(|(i, _)| *i == id) {
                Some((_, fk)) => fk,
                None => {
                    fks.push((
                        id,
                        ForeignKey { references_table: f.get(r, "table").unwrap_or_default().to_string(), ..Default::default() },
                    ));
                    &mut fks.last_mut().unwrap().1
                }
            };
            entry.columns.push(f.get(r, "from").unwrap_or_default().to_string());
            entry.references_columns.push(f.get(r, "to").unwrap_or_default().to_string());
        }
        tables.push(Table {
            schema: None,
            name,
            kind: t.get(r, "type").unwrap_or("table").to_string(),
            rows: None,
            comment: None,
            columns,
            primary_key: pk.into_iter().map(|(_, n)| n).collect(),
            foreign_keys: fks.into_iter().map(|(_, fk)| fk).collect(),
        });
    }
    Ok(tables)
}

/// Now, RFC 3339 to the second, with no date crate: civil-from-days.
pub fn now() -> String {
    let secs = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_secs()).unwrap_or(0);
    let (y, m, d, hh, mm, ss) = crate::sigv4::civil_time(secs);
    format!("{y:04}-{m:02}-{d:02}T{hh:02}:{mm:02}:{ss:02}Z")
}

// --------------------------------------------------------------------- drift

/// What changed between two reads of the same datasource: the schema's
/// drift, which is what a committed catalogue is for.
#[derive(Debug, Clone, Default, Serialize)]
pub struct Drift {
    pub tables_added: Vec<String>,
    pub tables_dropped: Vec<String>,
    /// (table, column)
    pub columns_added: Vec<(String, String)>,
    pub columns_dropped: Vec<(String, String)>,
    /// (table, column, was, now)
    pub columns_retyped: Vec<(String, String, String, String)>,
}

impl Drift {
    pub fn is_empty(&self) -> bool {
        self.tables_added.is_empty()
            && self.tables_dropped.is_empty()
            && self.columns_added.is_empty()
            && self.columns_dropped.is_empty()
            && self.columns_retyped.is_empty()
    }

    /// From `before` to `after`, by qualified table name and column name.
    pub fn between(before: &Catalogue, after: &Catalogue) -> Drift {
        let key = |t: &Table| t.physical_name().to_lowercase();
        let mut d = Drift::default();
        for t in &after.tables {
            match before.tables.iter().find(|b| key(b) == key(t)) {
                None => d.tables_added.push(t.physical_name()),
                Some(b) => {
                    for c in &t.columns {
                        match b.columns.iter().find(|bc| bc.name.eq_ignore_ascii_case(&c.name)) {
                            None => d.columns_added.push((t.physical_name(), c.name.clone())),
                            Some(bc) if !bc.data_type.eq_ignore_ascii_case(&c.data_type) => {
                                d.columns_retyped.push((t.physical_name(), c.name.clone(), bc.data_type.clone(), c.data_type.clone()))
                            }
                            Some(_) => {}
                        }
                    }
                    for bc in &b.columns {
                        if !t.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&bc.name)) {
                            d.columns_dropped.push((t.physical_name(), bc.name.clone()));
                        }
                    }
                }
            }
        }
        for b in &before.tables {
            if !after.tables.iter().any(|t| key(t) == key(b)) {
                d.tables_dropped.push(b.physical_name());
            }
        }
        d
    }

    /// One line per change, in the order a reader wants them.
    pub fn lines(&self) -> Vec<String> {
        let mut out = Vec::new();
        for t in &self.tables_added {
            out.push(format!("table added:    {t}"));
        }
        for t in &self.tables_dropped {
            out.push(format!("table dropped:  {t}"));
        }
        for (t, c) in &self.columns_added {
            out.push(format!("column added:   {t}.{c}"));
        }
        for (t, c) in &self.columns_dropped {
            out.push(format!("column dropped: {t}.{c}"));
        }
        for (t, c, was, now) in &self.columns_retyped {
            out.push(format!("column retyped: {t}.{c}: {was} -> {now}"));
        }
        out
    }
}
