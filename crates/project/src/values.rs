//! Sampled dimension values: what a dimension's column actually holds, so a
//! question like "shoes" can be matched to a value before any warehouse
//! probe. Written by `zsql values sample` to `samples/values.jsonl.gz` in
//! the project directory, one line per dimension, and shipped with the model.
//!
//! The file is customer data: gitignored by default, regenerable, and
//! only ever holds dimensions that are listable (not hidden, listing not
//! disabled, not named by a security policy's triggers).

use serde::{Deserialize, Serialize};
use std::io::{BufRead, Write};
use std::path::Path;
use zsql_semantic::{DataType, FieldKind, Universe, ValueSampling};

pub const FILE_NAME: &str = "values.jsonl.gz";
/// A `full` sample above this many distinct values is refused.
pub const FULL_CEILING: i64 = 100_000;

#[derive(Debug, Clone, Default, Deserialize, Serialize)]
pub struct ValueCatalog {
    pub dimensions: Vec<DimensionValues>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct DimensionValues {
    pub uid: String,
    pub name: String,
    /// The table the values were read from.
    pub table: String,
    /// RFC 3339.
    pub sampled_at: String,
    pub mode: ValueSampling,
    /// Distinct values in the column, when counted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub distinct: Option<i64>,
    /// The sampled values with their row counts, most frequent first.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub values: Vec<ValueCount>,
    /// For numbers and dates: the column's bounds instead of its values.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub range: Option<Range>,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct ValueCount {
    pub v: String,
    pub n: i64,
}

#[derive(Debug, Clone, Deserialize, Serialize)]
pub struct Range {
    pub min: String,
    pub max: String,
}

impl ValueCatalog {
    /// `None` when the file does not exist.
    pub fn read(path: &Path) -> std::io::Result<Option<ValueCatalog>> {
        if !path.exists() {
            return Ok(None);
        }
        let file = std::fs::File::open(path)?;
        let reader = std::io::BufReader::new(flate2::read::GzDecoder::new(file));
        let mut dimensions = Vec::new();
        for line in reader.lines() {
            let line = line?;
            if line.trim().is_empty() {
                continue;
            }
            dimensions.push(serde_json::from_str(&line).map_err(|e| std::io::Error::new(std::io::ErrorKind::InvalidData, e))?);
        }
        Ok(Some(ValueCatalog { dimensions }))
    }

    pub fn write(&self, path: &Path) -> std::io::Result<()> {
        let file = std::fs::File::create(path)?;
        let mut enc = flate2::write::GzEncoder::new(file, flate2::Compression::default());
        for d in &self.dimensions {
            enc.write_all(serde_json::to_string(d)?.as_bytes())?;
            enc.write_all(b"\n")?;
        }
        enc.finish()?;
        Ok(())
    }

    pub fn get(&self, uid: &str) -> Option<&DimensionValues> {
        self.dimensions.iter().find(|d| d.uid == uid)
    }

    /// Replaces or adds a dimension's entry, keeping the list sorted by uid.
    pub fn put(&mut self, d: DimensionValues) {
        self.dimensions.retain(|x| x.uid != d.uid);
        self.dimensions.push(d);
        self.dimensions.sort_by(|a, b| a.uid.cmp(&b.uid));
    }
}

/// What to run for one dimension: the SQL for its values or range, on
/// which table, in which datasource.
#[derive(Debug, Clone)]
pub struct SampleJob {
    pub uid: String,
    pub name: String,
    pub table_uid: String,
    pub datasource_uid: String,
    pub adapter: String,
    pub mode: ValueSampling,
    /// `SELECT count(DISTINCT expr)`.
    pub distinct_sql: String,
    /// For text: `SELECT expr, count(*) ... LIMIT cap`. Empty for ranges.
    pub values_sql: Option<String>,
    /// The cap the values query carries (absent for `full`).
    pub cap: Option<i64>,
    /// For numbers and dates: `SELECT min(expr), max(expr)`.
    pub range_sql: Option<String>,
}

/// Why a dimension is not sampled.
pub fn skip_reason(universe: &Universe, uid: &str) -> Option<String> {
    let Some(f) = universe.field_by_uid(uid, Some(FieldKind::Dimension)) else { return Some("not a dimension".into()) };
    if f.hidden {
        return Some("hidden".into());
    }
    if f.disable_listing {
        return Some("listing disabled".into());
    }
    if f.value_sampling == ValueSampling::None {
        return Some("values: none".into());
    }
    for p in universe.policies() {
        if p.trigger_field_uids.contains(&f.uid) || f.tags.iter().any(|t| p.trigger_tags.contains(t)) {
            return Some(format!("named by security policy '{}'", p.name));
        }
    }
    if f.data_type.is_none() {
        return Some("no data type".into());
    }
    None
}

/// The sampling plan for the listable dimensions (or the given uids), one
/// job each, reading from the cheapest table that holds the dimension.
/// `qualify` rewrites an expression's column references with a table alias.
pub fn plan(
    universe: &Universe,
    only: &[String],
    full: bool,
    qualify: &dyn Fn(&str, &[String], &str, &str) -> String,
) -> (Vec<SampleJob>, Vec<(String, String)>) {
    let mut jobs = Vec::new();
    let mut skipped = Vec::new();
    let wanted: Vec<&zsql_semantic::Field> = if only.is_empty() {
        universe.fields().iter().filter(|f| f.kind == FieldKind::Dimension).collect()
    } else {
        only.iter()
            .filter_map(|u| {
                universe.field_by_uid(u, Some(FieldKind::Dimension)).or_else(|| universe.field_by_name(u, FieldKind::Dimension))
            })
            .collect()
    };
    for f in wanted {
        if let Some(why) = skip_reason(universe, &f.uid) {
            skipped.push((f.uid.clone(), why));
            continue;
        }
        // The cheapest table with an expression for the dimension.
        let mut candidates: Vec<_> = universe
            .snapshot()
            .expressions
            .iter()
            .filter(|e| e.field_id == f.id)
            .filter_map(|e| universe.table(e.table_id).map(|t| (t, e)))
            .collect();
        candidates.sort_by_key(|(t, _)| (t.cost, t.uid.clone()));
        let Some((table, exp)) = candidates.first() else {
            skipped.push((f.uid.clone(), "no expression".into()));
            continue;
        };
        let Some(ds) = universe.datasource(table.datasource_id) else { continue };
        let expr = qualify(exp.sql.as_deref().unwrap_or_default(), &exp.column_refs, &ds.adapter, "T0");
        let from = format!("{} T0", table.physical_name);
        let mode = if full { ValueSampling::Full } else { f.value_sampling };
        let distinct_sql = format!("SELECT count(DISTINCT {expr}) AS \"n\" FROM {from}");
        let (values_sql, cap, range_sql) = match f.data_type {
            Some(DataType::String | DataType::Boolean) => {
                let cap = (mode == ValueSampling::Sample).then_some(f.value_list_size.max(1));
                let limit = cap.map(|c| format!("\nLIMIT {c}")).unwrap_or_default();
                (Some(format!("SELECT {expr} AS \"value\", count(*) AS \"n\" FROM {from}\nWHERE {expr} IS NOT NULL\nGROUP BY {expr}\nORDER BY count(*) DESC, {expr}{limit}")), cap, None)
            }
            _ => (None, None, Some(format!("SELECT min({expr}) AS \"lo\", max({expr}) AS \"hi\" FROM {from}"))),
        };
        jobs.push(SampleJob {
            uid: f.uid.clone(),
            name: f.name.clone(),
            table_uid: table.uid.clone(),
            datasource_uid: ds.uid.clone(),
            adapter: ds.adapter.clone(),
            mode,
            distinct_sql,
            values_sql,
            cap,
            range_sql,
        });
    }
    (jobs, skipped)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn catalog_round_trips_through_the_file() {
        let dir = std::env::temp_dir().join(format!("zsql-values-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let mut c = ValueCatalog::default();
        c.put(DimensionValues {
            uid: "category".into(),
            name: "Category".into(),
            table: "item".into(),
            sampled_at: "2026-09-30T00:00:00Z".into(),
            mode: ValueSampling::Sample,
            distinct: Some(3),
            values: vec![ValueCount { v: "Books".into(), n: 10 }, ValueCount { v: "Music".into(), n: 4 }],
            range: None,
        });
        let path = dir.join(FILE_NAME);
        c.write(&path).unwrap();
        let back = ValueCatalog::read(&path).unwrap().unwrap();
        assert_eq!(back.dimensions.len(), 1);
        assert_eq!(back.get("category").unwrap().values[0].v, "Books");
        assert!(ValueCatalog::read(&dir.join("missing.gz")).unwrap().is_none());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
