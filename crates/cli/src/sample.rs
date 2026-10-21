//! `zsql sample`: the values a dimension holds, written to
//! `samples/values.jsonl.gz` from the warehouse. The model says which
//! dimensions, through `enable_sampling`; this command takes it at its
//! word and warns when what came back looks like long text. See
//! `docs/introspection.md`, phase D, and the project's AGENTS.md for the
//! judgement the flag asks of a modeller.

use crate::{config, local};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use zsql_project::values::{DimensionValues, ValueCatalog, ValueCount};
use zsql_semantic::{Expression, Field, FieldKind, Snapshot, Table};
use zsql_service::{Model, Trust};
use zsql_source::{Catalogue, Connection};

pub struct Options<'a> {
    pub datasource: Option<&'a str>,
    /// Only this dimension or table, by name or uid.
    pub only: Option<&'a str>,
    pub dry_run: bool,
    /// Refuse before running when the plan needs more queries than this.
    pub budget: Option<usize>,
}

/// A value longer than this, on average, is prose and not a value: the
/// catalogue fills with text that matches every request a little.
const LONG_TEXT: usize = 100;
/// A declared width past this is the same signal, from the catalogue,
/// before any row is read; it is the number in AGENTS.md.
const WIDE: i64 = 255;

/// One dimension the model wants sampled, and how.
struct Job<'a> {
    field: &'a Field,
    table: &'a Table,
    /// The rendered expression for the generated query, or none when the
    /// author's own `sample_sql` is used.
    expr: Option<String>,
}

pub fn run(project_dir: Option<&Path>, opts: Options<'_>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let model = Model::from_dir(&project.dir, Trust::Owned, local::dialects()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let s = model.universe().snapshot();
    let only = opts.only.map(|o| o.trim().to_lowercase());

    let out_path = project.dir.join("samples").join(zsql_project::values::FILE_NAME);
    let mut catalog = ValueCatalog::read(&out_path).with_context(|| out_path.display().to_string())?.unwrap_or_default();

    let mut total_queries = 0;
    let mut sampled = 0;
    for ds in &s.datasources {
        if opts.datasource.is_some_and(|d| !d.eq_ignore_ascii_case(&ds.uid)) {
            continue;
        }
        let dialect = model.dialects.get(&ds.adapter)?;
        // What the model asks for on this datasource, with the reasons for
        // what it does not get.
        let mut jobs: Vec<Job> = Vec::new();
        for f in s.fields.iter().filter(|f| f.kind == FieldKind::Dimension) {
            let Some((e, table)) = expression_on(s, f, ds.id) else { continue };
            if let Some(o) = &only {
                let hit = f.uid.to_lowercase() == *o
                    || f.name.to_lowercase() == *o
                    || table.name.to_lowercase() == *o
                    || table.physical_name.to_lowercase() == *o;
                if !hit {
                    continue;
                }
            }
            if !f.enable_sampling {
                continue;
            }
            match why_not(s, f, e, table) {
                Some(why) => println!("skip    {} ({}): {why}", f.name, table.name),
                None => {
                    let expr = match &f.sample_sql {
                        Some(_) => None,
                        None => Some(render(e, &dialect)?),
                    };
                    jobs.push(Job { field: f, table, expr });
                }
            }
        }
        if jobs.is_empty() {
            continue;
        }
        // One distinct-count query per table for the generated ones, plus
        // one query per dimension.
        let tables: BTreeMap<zsql_semantic::Id, Vec<&Job>> = jobs.iter().filter(|j| j.expr.is_some()).fold(BTreeMap::new(), |mut m, j| {
            m.entry(j.table.id).or_default().push(j);
            m
        });
        let planned = tables.len() + jobs.len();
        total_queries += planned;
        println!(
            "{}: {} dimension{} on {} table{}: {planned} quer{}",
            ds.uid,
            jobs.len(),
            if jobs.len() == 1 { "" } else { "s" },
            tables.len(),
            if tables.len() == 1 { "" } else { "s" },
            if planned == 1 { "y" } else { "ies" }
        );
        if let Some(b) = opts.budget {
            if planned > b {
                bail!("{planned} queries is more than the budget of {b}; narrow with --only, or raise --budget");
            }
        }
        if opts.dry_run {
            for j in &jobs {
                println!(
                    "  would sample {} from {} ({})",
                    j.field.name,
                    j.table.physical_name,
                    if j.expr.is_some() { "generated" } else { "sample_sql" }
                );
            }
            continue;
        }

        let conn = Connection::read(&project.dir, &ds.uid).context("reading datasources.yml")?;
        let secret = zsql_source::secret(Some(&project.dir), &conn.uid, "password");
        let mut src = zsql_source::open(&conn, secret).with_context(|| format!("opening datasource {}", conn.uid))?;
        for w in src.warnings() {
            eprintln!("warning: {w}");
        }
        let catalogue = std::fs::read_to_string(crate::introspect::path(&project.dir, &ds.uid))
            .ok()
            .and_then(|t| serde_json::from_str::<Catalogue>(&t).ok());

        // Distinct counts, one scan per table.
        let mut distinct: BTreeMap<String, i64> = BTreeMap::new();
        for group in tables.values() {
            let cols: Vec<String> =
                group.iter().enumerate().map(|(i, j)| format!("COUNT(DISTINCT {}) AS d{i}", j.expr.as_ref().unwrap())).collect();
            let sql = format!("SELECT {} FROM {}", cols.join(", "), group[0].table.physical_name);
            match src.query(&sql, 1) {
                Ok(rows) => {
                    for (i, j) in group.iter().enumerate() {
                        if let Some(n) = rows.rows.first().and_then(|r| rows.get(r, &format!("d{i}"))).and_then(|n| n.parse::<f64>().ok()) {
                            distinct.insert(j.field.uid.clone(), n as i64);
                        }
                    }
                }
                Err(e) => eprintln!("warning: distinct counts for {}: {e}", group[0].table.name),
            }
        }

        // The values, most frequent first.
        for j in &jobs {
            let size = j.field.sample_size;
            let rows = match (&j.field.sample_sql, &j.expr) {
                (Some(own), _) => src.query(own, zsql_semantic::MAX_SAMPLE_SIZE as usize),
                (None, Some(expr)) => {
                    // Most frequent first, ties by value, so two runs of the
                    // same warehouse write the same file.
                    let sql =
                        format!("SELECT {expr} AS v, COUNT(*) AS n FROM {} GROUP BY {expr} ORDER BY n DESC, v", j.table.physical_name);
                    let sql = dialect.apply_limit(&sql, 0, &size.to_string())?;
                    src.query(&sql, size as usize)
                }
                (None, None) => unreachable!(),
            };
            let rows = match rows {
                Ok(r) => r,
                Err(e) => {
                    eprintln!("warning: {} ({}): {e}", j.field.name, j.table.name);
                    continue;
                }
            };
            let values: Vec<ValueCount> = rows
                .rows
                .iter()
                .filter_map(|r| {
                    let v = r.first()?.clone()?;
                    let n = r.get(1).and_then(|c| c.as_deref()).and_then(|c| c.parse::<f64>().ok()).map(|n| n as i64).unwrap_or(1);
                    Some(ValueCount { v, n })
                })
                .collect();
            let d = distinct.get(&j.field.uid).copied();

            // Long text: what came back, and what the catalogue declared.
            if !values.is_empty() {
                let avg = values.iter().map(|v| v.v.chars().count()).sum::<usize>() / values.len();
                if avg > LONG_TEXT {
                    eprintln!("warning: {} ({}): the values average {avg} characters, which is text, not a value; sampling it will blur resolution — turn enable_sampling off, or give it a sample_sql that trims", j.field.name, j.table.name);
                }
            }
            if let (Some(cat), Some(e)) = (&catalogue, j.expr.as_deref()) {
                if let Some(col) = declared(cat, &j.table.physical_name, e) {
                    match col.max_length {
                        Some(w) if w > WIDE => eprintln!(
                            "warning: {} ({}): declared {} wide, past the {WIDE} AGENTS.md advises; check it is a value and not text",
                            j.field.name, j.table.name, w
                        ),
                        None if col.data_type.to_lowercase().contains("text") || col.data_type.to_lowercase().contains("clob") => {
                            eprintln!(
                                "warning: {} ({}): declared {}, unbounded text; check it is a value and not prose",
                                j.field.name, j.table.name, col.data_type
                            )
                        }
                        _ => {}
                    }
                }
            }

            let complete = match d {
                Some(n) => n <= size,
                None => (values.len() as i64) < size,
            };
            println!(
                "sampled {} ({}): {} value{}{}{}",
                j.field.name,
                j.table.name,
                values.len(),
                if values.len() == 1 { "" } else { "s" },
                match d {
                    Some(n) => format!(" of {n}"),
                    None => String::new(),
                },
                if complete { "" } else { ", cut at the limit" }
            );
            let record = DimensionValues {
                uid: j.field.uid.clone(),
                name: j.field.name.clone(),
                table: j.table.name.clone(),
                sampled_at: zsql_source::catalogue::now(),
                sample_size: Some(size),
                distinct: d,
                values,
                range: None,
            };
            catalog.dimensions.retain(|x| x.uid != record.uid);
            catalog.dimensions.push(record);
            sampled += 1;
        }
    }
    if opts.dry_run {
        println!("{total_queries} quer{} in all; nothing run, nothing written", if total_queries == 1 { "y" } else { "ies" });
        return Ok(());
    }
    if sampled == 0 {
        println!("nothing sampled: no dimension has enable_sampling: true, or none matched --only");
        return Ok(());
    }
    catalog.dimensions.sort_by(|a, b| a.uid.cmp(&b.uid));
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    catalog.write(&out_path).with_context(|| out_path.display().to_string())?;
    println!(
        "{sampled} dimension{} -> {}",
        if sampled == 1 { "" } else { "s" },
        out_path.strip_prefix(&project.dir).unwrap_or(&out_path).display()
    );
    Ok(())
}

/// The field's expression on a table of this datasource, and that table.
fn expression_on<'a>(s: &'a Snapshot, f: &Field, ds: zsql_semantic::Id) -> Option<(&'a Expression, &'a Table)> {
    s.expressions.iter().filter(|e| e.field_id == f.id).find_map(|e| {
        let t = s.tables.iter().find(|t| t.id == e.table_id && t.datasource_id == ds)?;
        Some((e, t))
    })
}

/// Why a dimension that asks to be sampled is not, in one line.
fn why_not(s: &Snapshot, f: &Field, e: &Expression, table: &Table) -> Option<String> {
    if f.hidden {
        return Some("hidden".into());
    }
    for p in &s.security_policies {
        let named = p.trigger_field_uids.iter().any(|u| u == &f.uid)
            || p.context_dimension_uid == f.uid
            || p.trigger_tags.iter().any(|t| f.tags.iter().any(|ft| ft.eq_ignore_ascii_case(t)));
        if named {
            return Some(format!("named by the security policy \"{}\"; its values are what the policy protects", p.name));
        }
    }
    if f.sample_sql.is_some() {
        return None;
    }
    if e.lookup || e.array {
        return Some("a lookup or array expression; give it a sample_sql".into());
    }
    if !e.dimension_ref_ids.is_empty() || !e.measure_ref_ids.is_empty() {
        return Some("a calculated dimension; give it a sample_sql".into());
    }
    if e.sql.as_deref().map(str::trim).is_none_or(str::is_empty) {
        return Some("no sql expression".into());
    }
    if s.partitions.iter().any(|p| p.table_id == table.id) {
        return Some("a partitioned table; without a window the query scans all history — give it a sample_sql with one".into());
    }
    None
}

/// A bare column quoted by the dialect; anything else as the author wrote it.
fn render(e: &Expression, dialect: &zsql_dialect::Dialect) -> Result<String> {
    let sql = e.sql.as_deref().unwrap_or("").trim();
    let bare = !sql.is_empty() && sql.chars().all(|c| c.is_alphanumeric() || c == '_') && !sql.chars().next().unwrap().is_ascii_digit();
    Ok(if bare { dialect.quote(sql)? } else { sql.to_string() })
}

/// The catalogue's column behind a bare-column expression, if the
/// catalogue has the table.
fn declared<'a>(cat: &'a Catalogue, physical: &str, expr: &str) -> Option<&'a zsql_source::Column> {
    let name = expr.trim_matches(|c: char| !c.is_alphanumeric() && c != '_');
    let (schema, table) = match physical.rsplit_once('.') {
        Some((s, t)) => (Some(s), t),
        None => (None, physical),
    };
    let t = cat.tables.iter().find(|t| {
        t.name.eq_ignore_ascii_case(table) && schema.is_none_or(|s| t.schema.as_deref().is_some_and(|ts| ts.eq_ignore_ascii_case(s)))
    })?;
    t.columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))
}
