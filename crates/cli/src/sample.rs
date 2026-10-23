//! `zsql sample`: the values a dimension holds, written to
//! `samples/values.jsonl.gz` from the warehouse. The model says which
//! dimensions, through `enable_sampling`; this command takes it at its
//! word and warns when what came back looks like long text. See
//! `docs/introspection.md`, phase D, and the project's AGENTS.md for the
//! judgment the flag asks of a modeller.

use crate::{config, local, tty};
use anyhow::{bail, Context, Result};
use std::collections::BTreeMap;
use std::path::Path;
use zsql_project::values::{DimensionValues, ValueCatalog, ValueCount};
use zsql_semantic::{Field, FieldKind, Snapshot, Table};
use zsql_service::{Model, Trust};
use zsql_source::{Catalog, Connection};

pub struct Options<'a> {
    pub datasource: Option<&'a str>,
    /// Only these dimensions or tables, by name or uid.
    pub names: &'a [String],
    pub dry_run: bool,
    /// Refuse before running when the plan needs more queries than this;
    /// a hundred when unsaid.
    pub budget: Option<usize>,
    /// Run without asking.
    pub yes: bool,
    /// Report the plan and the outcome as JSON. Counts only: the values
    /// themselves are customer data and go to the samples file, never to
    /// stdout.
    pub json: bool,
}

/// What each dimension this run would read is, which every answer carries
/// whether or not the reading happened.
fn plan_of(jobs: &[Job<'_>]) -> Vec<serde_json::Value> {
    jobs.iter()
        .map(|j| {
            serde_json::json!({
                "field": j.field.name,
                "uid": j.field.uid,
                "sample_sql": j.field.sample_sql.is_some(),
                "sample_size": j.field.sample_size,
            })
        })
        .collect()
}

/// One shape for every answer this command can give, so a caller reads the
/// same keys whether it planned, read nothing or wrote a file. Counts only:
/// the values are customer data and go to the samples file.
fn answer(
    plan: &[serde_json::Value],
    sampled: &[serde_json::Value],
    skipped: &[serde_json::Value],
    planned_queries: usize,
    file: Option<&str>,
) -> serde_json::Value {
    serde_json::json!({
        "plan": plan,
        "planned_queries": planned_queries,
        "sampled": sampled,
        "skipped": skipped,
        "written": file.is_some(),
        "file": file,
        "note": "a samples file is customer data: it ships with the model on deploy and is gitignored by default",
    })
}

/// Queries a run may need before it is refused without a budget said.
const DEFAULT_BUDGET: usize = 100;

/// A value longer than this, on average, is prose and not a value: the
/// catalog fills with text that matches every request a little.
const LONG_TEXT: usize = 100;
/// A declared width past this is the same signal, from the catalog,
/// before any row is read; it is the number in AGENTS.md.
const WIDE: i64 = 255;

/// One dimension the model wants sampled, and how: through the planner,
/// or through the author's own query.
struct Job<'a> {
    field: &'a Field,
    /// Tables whose expression for this field is marked `lookup`, passed
    /// to the planner as hints.
    lookups: Vec<String>,
}

pub fn run(project_dir: Option<&Path>, opts: Options<'_>) -> Result<()> {
    let project = config::find_project(project_dir.unwrap_or(Path::new(".")))?;
    let model = Model::from_dir(&project.dir, Trust::Owned, local::dialects()).map_err(|e| anyhow::anyhow!("{e}"))?;
    let s = model.universe().snapshot();
    let only: Vec<String> = opts.names.iter().map(|o| o.trim().to_lowercase()).filter(|o| !o.is_empty()).collect();

    let out_path = project.dir.join("samples").join(zsql_project::values::FILE_NAME);
    let mut catalog = ValueCatalog::read(&out_path).with_context(|| out_path.display().to_string())?.unwrap_or_default();

    // What the model asks for, with the reasons for what it does not get.
    // Which table answers is the planner's decision, not this command's,
    // so a dimension is a job whatever its expression looks like.
    let mut jobs: Vec<Job> = Vec::new();
    let mut skipped: Vec<(String, String)> = Vec::new();
    for f in s.fields.iter().filter(|f| f.kind == FieldKind::Dimension && f.enable_sampling) {
        let tables: Vec<&Table> =
            s.expressions.iter().filter(|e| e.field_id == f.id).filter_map(|e| s.tables.iter().find(|t| t.id == e.table_id)).collect();
        if !only.is_empty() {
            let hit = only.iter().any(|o| {
                f.uid.to_lowercase() == *o
                    || f.name.to_lowercase() == *o
                    || tables
                        .iter()
                        .any(|t| t.name.to_lowercase() == *o || t.uid.to_lowercase() == *o || t.physical_name.to_lowercase() == *o)
            });
            if !hit {
                continue;
            }
        }
        if let Some(d) = opts.datasource {
            if !tables.iter().any(|t| s.datasources.iter().any(|ds| ds.id == t.datasource_id && ds.uid.eq_ignore_ascii_case(d))) {
                continue;
            }
        }
        match why_not(s, f) {
            Some(why) => skipped.push((f.name.clone(), why)),
            None => {
                let lookups = s
                    .expressions
                    .iter()
                    .filter(|e| e.field_id == f.id && e.lookup)
                    .filter_map(|e| s.tables.iter().find(|t| t.id == e.table_id).map(|t| t.uid.clone()))
                    .collect();
                jobs.push(Job { field: f, lookups });
            }
        }
    }
    let say = |line: String| {
        if !opts.json {
            println!("{line}");
        }
    };
    let skip_rows: Vec<serde_json::Value> =
        if opts.json { skipped.iter().map(|(name, why)| serde_json::json!({ "field": name, "why": why })).collect() } else { Vec::new() };
    for (name, why) in &skipped {
        say(format!("skip    {name}: {why}"));
    }
    if jobs.is_empty() {
        if opts.json {
            return crate::show::print_json(&answer(&[], &[], &skip_rows, 0, None));
        }
        println!("nothing to sample: no dimension has enable_sampling: true, or none matched");
        return Ok(());
    }
    // The planner answers twice per dimension — the values and their
    // distinct total — and an author's query once.
    let planned: usize = jobs.iter().map(|j| if j.field.sample_sql.is_some() { 1 } else { 2 }).sum();
    say(format!(
        "{} dimension{}: {planned} quer{}",
        jobs.len(),
        if jobs.len() == 1 { "" } else { "s" },
        if planned == 1 { "y" } else { "ies" }
    ));
    let budget = opts.budget.unwrap_or(DEFAULT_BUDGET);
    if planned > budget {
        bail!("{planned} queries is more than the budget of {budget}; narrow by naming dimensions or tables, or raise --budget");
    }
    // The plan: each dimension, where it is declared, and its width.
    let datasources: Vec<String> = {
        let mut d: Vec<String> = jobs
            .iter()
            .flat_map(|j| s.expressions.iter().filter(|e| e.field_id == j.field.id))
            .filter_map(|e| s.tables.iter().find(|t| t.id == e.table_id))
            .filter_map(|t| s.datasources.iter().find(|d| d.id == t.datasource_id))
            .map(|d| d.uid.clone())
            .collect();
        d.sort();
        d.dedup();
        d
    };
    for j in &jobs {
        let on: Vec<String> = s
            .expressions
            .iter()
            .filter(|e| e.field_id == j.field.id)
            .filter_map(|e| s.tables.iter().find(|t| t.id == e.table_id))
            .map(|t| t.name.clone())
            .collect();
        say(format!(
            "  {:<32} on {:<24} {}",
            j.field.name,
            on.join(", "),
            if j.field.sample_sql.is_some() { "sample_sql".to_string() } else { format!("planned, up to {} values", j.field.sample_size) }
        ));
    }
    if opts.dry_run {
        if opts.json {
            return crate::show::print_json(&answer(&plan_of(&jobs), &[], &skip_rows, planned, None));
        }
        println!("nothing run, nothing written");
        return Ok(());
    }
    if !opts.yes {
        let question = format!("run {planned} quer{} on {}?", if planned == 1 { "y" } else { "ies" }, datasources.join(", "));
        if !tty::confirm(&question, "--yes")? {
            bail!("nothing run");
        }
    }

    // A policy needs a context to plan under; none of these projections
    // names a policy's trigger, so an empty one fires nothing.
    let context = (!s.security_policies.is_empty()).then(zsql_spec::SecurityContext::default);
    let catalogs: BTreeMap<String, Catalog> = s
        .datasources
        .iter()
        .filter_map(|ds| {
            let text = std::fs::read_to_string(crate::catalog::path(&project.dir, &ds.uid)).ok()?;
            Some((ds.uid.clone(), serde_json::from_str(&text).ok()?))
        })
        .collect();
    // Connections by the datasource the planner chose, opened on first use.
    let mut sources: BTreeMap<String, Box<dyn zsql_source::Source>> = BTreeMap::new();
    let mut sampled = 0;
    let mut rows: Vec<serde_json::Value> = Vec::new();
    for j in &jobs {
        let size = j.field.sample_size;
        let result = sample_one(&model, &project.dir, s, j, context.as_ref(), &mut sources, &catalogs);
        let (values, distinct, via) = match result {
            Ok(r) => r,
            Err(e) => {
                eprintln!("warning: {}: {e}", j.field.name);
                continue;
            }
        };
        if !values.is_empty() {
            let avg = values.iter().map(|v| v.v.chars().count()).sum::<usize>() / values.len();
            if avg > LONG_TEXT {
                eprintln!("warning: {}: the values average {avg} characters, which is text, not a value; sampling it will blur resolution — turn enable_sampling off, or give it a sample_sql that trims", j.field.name);
            }
        }
        let own = j.field.sample_sql.is_some();
        let complete = match distinct {
            Some(n) => n <= size,
            None => own || (values.len() as i64) < size,
        };
        say(format!(
            "sampled {} ({via}): {} value{}{}{}",
            j.field.name,
            values.len(),
            if values.len() == 1 { "" } else { "s" },
            match distinct {
                Some(n) => format!(" of {n}"),
                None => String::new(),
            },
            if complete { "" } else { ", cut at the limit" }
        ));
        // Counts, never the values: those are customer data and belong in
        // the samples file, which is gitignored and never printed.
        if opts.json {
            rows.push(serde_json::json!({
                "field": j.field.name,
                "uid": j.field.uid,
                "table": via,
                "values": values.len(),
                "distinct": distinct,
                "complete": complete,
            }));
        }
        let record = DimensionValues {
            uid: j.field.uid.clone(),
            name: j.field.name.clone(),
            table: via.clone(),
            sampled_at: zsql_source::catalog::now(),
            sample_size: (!own).then_some(size),
            distinct,
            values,
            range: None,
        };
        catalog.dimensions.retain(|x| x.uid != record.uid);
        catalog.dimensions.push(record);
        sampled += 1;
    }
    if sampled == 0 {
        if opts.json {
            return crate::show::print_json(&answer(&plan_of(&jobs), &[], &skip_rows, planned, None));
        }
        println!("nothing sampled");
        return Ok(());
    }
    catalog.dimensions.sort_by(|a, b| a.uid.cmp(&b.uid));
    if let Some(parent) = out_path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    catalog.write(&out_path).with_context(|| out_path.display().to_string())?;
    let file = out_path.strip_prefix(&project.dir).unwrap_or(&out_path).display().to_string();
    if opts.json {
        return crate::show::print_json(&answer(&plan_of(&jobs), &rows, &skip_rows, planned, Some(&file)));
    }
    println!("{sampled} dimension{} -> {file}", if sampled == 1 { "" } else { "s" });
    println!("that file is customer data: it ships with the model on deploy and is gitignored by default; committing it is a decision about where those values may go");
    Ok(())
}

/// The values, their distinct total, and where they came from.
#[allow(clippy::too_many_arguments)]
fn sample_one(
    model: &Model,
    project_dir: &Path,
    s: &Snapshot,
    j: &Job<'_>,
    context: Option<&zsql_spec::SecurityContext>,
    sources: &mut BTreeMap<String, Box<dyn zsql_source::Source>>,
    catalogs: &BTreeMap<String, Catalog>,
) -> Result<(Vec<ValueCount>, Option<i64>, String)> {
    let size = j.field.sample_size;
    let open = |uid: &str, sources: &mut BTreeMap<String, Box<dyn zsql_source::Source>>| -> Result<()> {
        if !sources.contains_key(uid) {
            let conn = Connection::read(project_dir, uid).context("reading datasources.yml")?;
            let secrets = zsql_source::secrets(Some(project_dir), &conn.uid);
            let src = zsql_source::open(&conn, secrets).with_context(|| format!("opening datasource {}", conn.uid))?;
            for w in src.warnings() {
                eprintln!("warning: {w}");
            }
            sources.insert(uid.to_string(), src);
        }
        Ok(())
    };

    // The author's own query runs on the datasource the field's tables are
    // on, as written, with the value first and an optional count second.
    if let Some(own) = &j.field.sample_sql {
        let ds = s
            .expressions
            .iter()
            .filter(|e| e.field_id == j.field.id)
            .filter_map(|e| s.tables.iter().find(|t| t.id == e.table_id))
            .filter_map(|t| s.datasources.iter().find(|d| d.id == t.datasource_id))
            .next()
            .ok_or_else(|| anyhow::anyhow!("no datasource"))?;
        open(&ds.uid, sources)?;
        let rows = sources.get_mut(&ds.uid).unwrap().query(own, zsql_semantic::MAX_SAMPLE_SIZE as usize)?;
        return Ok((values_of(&rows, 0, 1), None, ds.uid.clone()));
        // (The record's sample_size is left unset for this path: the
        // author's query is what it is, and nothing was cut by a limit.)
    }

    // Through the planner, like any other query: the dimension, a count as
    // a calculation, descending, the limit, and the lookup tables as hints.
    // The planner lays dimensions before calculations, so the count cannot
    // lead the order and the warehouse picks which of the tied values a
    // limit cuts; the file is still sorted, by count then value.
    // The dimension is projected quoted, so a name that holds a keyword or
    // a comma reads as the one field it is.
    let dim = format!("\"{}\"", j.field.name.replace('"', "\"\""));
    let hints = if j.lookups.is_empty() { String::new() } else { format!(", hints ({})", j.lookups.join(", ")) };
    let line = format!("{dim}, calc(count(*), integer) as n desc, limit {size}{hints}");
    let req = zsql_service::SqlRequest { spec: None, expr: Some(line), context: context.cloned() };
    let planned = model.plan_request(req, false, &zsql_service::unlimited).map_err(|e| anyhow::anyhow!("{e}"))?;
    let dialect = model.dialects.get(&planned.plan.adapter)?;
    let sql = planned.plan.sql_with_limit(planned.limit, &dialect).map_err(|e| anyhow::anyhow!("{e}"))?;
    let via = planned.plan.datasource_uid.clone();
    open(&via, sources)?;
    let rows = sources.get_mut(&via).unwrap().query(&sql, size as usize)?;
    // The planner lays the dimension first and the calculation last, so
    // the value is the first column and the count the last whatever the
    // dimension is called — a dimension named N would otherwise be read
    // as its own count.
    let count = rows.columns.len().saturating_sub(1).max(1);
    let mut values = values_of(&rows, 0, count);
    values.sort_by(|a, b| b.n.cmp(&a.n).then_with(|| a.v.cmp(&b.v)));
    // A null group is a row the file does not keep; the distinct total
    // below counts it, so it is taken back when it was seen.
    let null_group = rows.rows.iter().any(|r| r.first().is_some_and(|c| c.is_none()));

    // The distinct total: the same question without the order or the
    // limit, counted as a derived table. Any common table expressions stay
    // in front, which is what the plan's root offset is for.
    let line = format!("{dim}, calc(count(*), integer) as n{hints}");
    let req = zsql_service::SqlRequest { spec: None, expr: Some(line), context: context.cloned() };
    let distinct = match model.plan_request(req, false, &zsql_service::unlimited) {
        Ok(p) => {
            let (prefix, root) = p.plan.sql.split_at(p.plan.root_at);
            let count = format!("{prefix}SELECT COUNT(*) AS d FROM (\n{root}\n) AS zsql_distinct");
            match sources.get_mut(&via).unwrap().query(&count, 1) {
                Ok(r) => r
                    .rows
                    .first()
                    .and_then(|row| r.get(row, "d"))
                    .and_then(|n| n.parse::<f64>().ok())
                    .map(|n| (n as i64 - i64::from(null_group)).max(0)),
                Err(e) => {
                    eprintln!("warning: {}: distinct total: {e}", j.field.name);
                    None
                }
            }
        }
        Err(e) => {
            eprintln!("warning: {}: distinct total: {e}", j.field.name);
            None
        }
    };

    // What the catalog declares for the column behind a bare-column
    // expression, on any table the field has one on.
    for e in s.expressions.iter().filter(|e| e.field_id == j.field.id) {
        let Some(t) = s.tables.iter().find(|t| t.id == e.table_id) else { continue };
        let Some(ds) = s.datasources.iter().find(|d| d.id == t.datasource_id) else { continue };
        let (Some(cat), Some(expr)) = (catalogs.get(&ds.uid), e.sql.as_deref()) else { continue };
        if let Some(col) = declared(cat, &t.physical_name, expr) {
            match col.max_length {
                Some(w) if w > WIDE => eprintln!(
                    "warning: {} ({}): declared {} wide, past the {WIDE} AGENTS.md advises; check it is a value and not text",
                    j.field.name, t.name, w
                ),
                None if col.data_type.to_lowercase().contains("text") || col.data_type.to_lowercase().contains("clob") => eprintln!(
                    "warning: {} ({}): declared {}, unbounded text; check it is a value and not prose",
                    j.field.name, t.name, col.data_type
                ),
                _ => {}
            }
        }
    }
    Ok((values, distinct, via))
}

/// Rows as values with their counts, given which column holds the value
/// and which the count. A missing count is one; a null value is no value.
fn values_of(rows: &zsql_source::Rows, value: usize, count: usize) -> Vec<ValueCount> {
    rows.rows
        .iter()
        .filter_map(|r| {
            let v = r.get(value)?.clone()?;
            let n = r.get(count).and_then(|c| c.as_deref()).and_then(|c| c.parse::<f64>().ok()).map(|n| n as i64).unwrap_or(1);
            Some(ValueCount { v, n })
        })
        .collect()
}

/// Why a dimension that asks to be sampled is not, in one line. Only the
/// model's own rules: what table answers, and how, is the planner's.
fn why_not(s: &Snapshot, f: &Field) -> Option<String> {
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
    None
}

/// The catalog's column behind a bare-column expression, if the
/// catalog has the table.
fn declared<'a>(cat: &'a Catalog, physical: &str, expr: &str) -> Option<&'a zsql_source::Column> {
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
