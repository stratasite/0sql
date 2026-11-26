//! Running a recipe over the latest snapshots, and reading back what it
//! made.

use crate::compile::{self, Compiled, Unresolved};
use crate::edits::{Entry, Outcome, State};
use crate::engine::Session;
use crate::error::{Error, Result};
use crate::lineage::{Ancestry, Lineage, MadeBy, SourceRead, COMPILER_VERSION};
use crate::recipe::{Recipe, SortKey};
use crate::schema::{ident, Column, ColumnType};
use crate::store::{self, Dataset, Materialization, Snapshot};
use crate::Prep;
use arrow_array::RecordBatch;
use std::path::Path;
use zsql_datasource::Batches;

/// A materialization, made and not yet kept.
pub(crate) struct Made {
    pub manifest: Materialization,
    pub lineage: Lineage,
    pub rows: Batches,
    pub outcomes: Vec<Outcome>,
}

/// Another dataset a recipe reads: its live version, as this job found it.
#[derive(Clone)]
pub(crate) struct Input {
    pub id: String,
    pub version: u32,
    pub dir: std::path::PathBuf,
    pub columns: Vec<Column>,
}

pub(crate) fn inputs_map(inputs: &[Input]) -> compile::Inputs {
    inputs.iter().map(|i| (i.id.clone(), i.columns.clone())).collect()
}

/// Loads each input's version as the table its steps read.
pub(crate) fn load_inputs(session: &Session, inputs: &[Input], sample: Option<usize>) -> Result<()> {
    for i in inputs {
        let rows = store::read_rows(&store::version_dir(&i.dir, i.version).join(store::ROWS))?;
        let batches = match sample {
            Some(n) => first_rows(&rows.batches, n),
            None => rows.batches,
        };
        let columns: Vec<(String, ColumnType)> = i.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
        session.load(&compile::input_table(&i.id), &columns, &batches)?;
    }
    Ok(())
}

/// What a run came to: a materialization, or a recipe that reads
/// columns the sources no longer have.
pub(crate) enum Run {
    Made(Box<Made>),
    Held(Vec<Unresolved>),
}

/// Loads each source's latest snapshot into the session as a table
/// named by the source's id; with `sample`, only its first rows.
pub(crate) fn load_sources(session: &Session, dir: &Path, dataset: &Dataset, sample: Option<usize>) -> Result<Vec<SourceRead>> {
    let mut read = Vec::new();
    for s in &dataset.sources {
        let (rows_path, manifest_path) = store::snapshot_paths(dir, &s.id, s.snapshot);
        let snapshot: Snapshot = store::read_json(&manifest_path)?;
        let rows = store::read_rows(&rows_path)?;
        let batches = match sample {
            Some(n) => first_rows(&rows.batches, n),
            None => rows.batches,
        };
        let columns: Vec<(String, ColumnType)> = snapshot.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
        session.load(&s.id, &columns, &batches)?;
        read.push(SourceRead {
            source: s.id.clone(),
            kind: snapshot.kind,
            snapshot: snapshot.number,
            file_name: snapshot.file_name,
            sha256: snapshot.sha256,
            parse: snapshot.parse,
            semantic: snapshot.semantic,
        });
    }
    Ok(read)
}

fn first_rows(batches: &[RecordBatch], n: usize) -> Vec<RecordBatch> {
    let mut out = Vec::new();
    let mut left = n;
    for b in batches {
        if left == 0 {
            break;
        }
        let take = left.min(b.num_rows());
        out.push(b.slice(0, take));
        left -= take;
    }
    out
}

/// Creates a compiled recipe's views in the session, and runs its
/// checks: a conversion that would lose a value fails here, naming the
/// column, both types and the first values.
pub(crate) fn prepare(session: &Session, compiled: &Compiled) -> Result<Vec<Outcome>> {
    let mut outcomes = Vec::new();
    let apply_edits = |at: usize, outcomes: &mut Vec<Outcome>| -> Result<()> {
        for stage in compiled.edits.iter().filter(|s| s.at == at) {
            outcomes.extend(crate::edits::apply(session, &stage.input, &stage.name, &stage.columns, &stage.edits)?);
        }
        Ok(())
    };
    for (i, (name, sql)) in compiled.views.iter().enumerate() {
        apply_edits(i, &mut outcomes)?;
        session.execute(&format!("CREATE VIEW {} AS {sql}", ident(name)))?;
    }
    apply_edits(compiled.views.len(), &mut outcomes)?;
    for c in &compiled.checks {
        let n = session.count(&format!("SELECT count(*) FROM {} WHERE {}", c.relation, c.failing))?;
        if n > 0 {
            let x = ident(&c.column);
            let examples = session.texts(&format!("SELECT CAST({x} AS VARCHAR) FROM {} WHERE {} LIMIT 3", c.relation, c.failing))?;
            let shown: Vec<String> = examples.iter().map(|v| format!("'{v}'")).collect();
            return Err(Error::Unprocessable(format!("{}, such as {}", c.says.replace("{n}", &n.to_string()), shown.join(", "))));
        }
    }
    Ok(outcomes)
}

/// Runs `recipe` over the dataset's latest snapshots as version `k`.
#[allow(clippy::too_many_arguments)]
pub(crate) fn run(
    prep: &Prep,
    dir: &Path,
    dataset: &Dataset,
    recipe: &Recipe,
    (log, upto): (&[Entry], u64),
    inputs: &[Input],
    k: u32,
) -> Result<Run> {
    let pinned_at = crate::time::now();
    let edits = crate::edits::in_force(log, upto);
    let compiled = compile::compile(recipe, &edits, &dataset.sources, &inputs_map(inputs), &pinned_at)?;
    if !compiled.unresolved.is_empty() {
        return Ok(Run::Held(compiled.unresolved));
    }
    let session = prep.engine.session()?;
    let sources = load_sources(&session, dir, dataset, None)?;
    load_inputs(&session, inputs, None)?;
    let outcomes = prepare(&session, &compiled)?;
    let list: Vec<String> = compiled.columns.iter().map(|c| ident(&c.name)).collect();
    let rows = session.query(&format!("SELECT {} FROM {}", list.join(", "), compiled.last))?;
    let mut notes = Vec::new();
    for (state, what) in
        [(State::Orphaned, "orphaned"), (State::Ambiguous, "ambiguous"), (State::Flagged, "flagged"), (State::Conflict, "in conflict")]
    {
        let ids: Vec<&str> = outcomes.iter().filter(|o| o.state == state).map(|o| o.id.as_str()).collect();
        if !ids.is_empty() {
            notes.push(format!("{} edit(s) {what}: {}", ids.len(), ids.join(", ")));
        }
    }
    let n = rows.num_rows();
    if n > prep.settings.max_rows {
        return Err(Error::Unprocessable(format!("the recipe makes {n} rows, more than the {} a dataset holds", prep.settings.max_rows)));
    }
    let columns: Vec<Column> = compiled.columns.clone();
    let ancestry: Vec<Ancestry> =
        columns.iter().zip(&compiled.ancestry).map(|(c, from)| Ancestry { column: c.name.clone(), from: from.clone() }).collect();
    let lineage = Lineage {
        dataset: dataset.id.clone(),
        version: k,
        recipe: recipe.version,
        edits: upto,
        sources,
        datasets: inputs.iter().map(|i| crate::lineage::DatasetRead { id: i.id.clone(), version: i.version }).collect(),
        roles: columns.iter().map(|c| (c.name.clone(), c.role)).collect(),
        made_by: MadeBy { zsql: env!("CARGO_PKG_VERSION").to_string(), compiler: COMPILER_VERSION, duckdb: prep.duckdb_version.clone() },
        pinned_at: pinned_at.clone(),
        notes,
        columns: ancestry,
    };
    let manifest = Materialization { version: k, made: pinned_at, columns, rows: n as u64, bytes: 0 };
    Ok(Run::Made(Box::new(Made { manifest, lineage, rows, outcomes })))
}

/// Keeps a materialization as `v<k>`, whole or not at all, refused
/// past the byte cap.
pub(crate) fn keep(prep: &Prep, dir: &Path, made: &mut Made) -> Result<()> {
    let k = made.manifest.version;
    let target = store::version_dir(dir, k);
    let staging = dir.join(format!(".v{k}.staging-{}", crate::time::nanos()));
    // A version there that the manifest never pointed at was left by a
    // job that did not finish, and is not a version.
    if target.exists() {
        std::fs::remove_dir_all(&target).map_err(|e| Error::io(&target, e))?;
    }
    let result = (|| {
        let bytes = store::write_rows(&staging.join(store::ROWS), &made.rows)?;
        if bytes > prep.settings.max_bytes {
            return Err(Error::Unprocessable(format!(
                "the recipe makes {bytes} bytes of rows, more than the {} a dataset holds",
                prep.settings.max_bytes
            )));
        }
        made.manifest.bytes = bytes;
        store::write_json(&staging.join("manifest.json"), &made.manifest)?;
        store::write_json(&staging.join("lineage.json"), &made.lineage)?;
        store::write_json(&staging.join("edits.json"), &made.outcomes)?;
        std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// How a page is sorted and filtered for viewing; neither changes the
/// dataset.
#[derive(Default)]
pub struct View<'a> {
    pub sort: &'a [SortKey],
    pub filter: Option<&'a str>,
}

/// A page of a materialization's rows, and how many rows the view has.
pub(crate) fn page(prep: &Prep, dir: &Path, k: u32, offset: u64, limit: u64, view: &View<'_>) -> Result<(Materialization, u64, Batches)> {
    let manifest: Materialization = store::read_json(&store::version_dir(dir, k).join("manifest.json"))?;
    let rows = store::read_rows(&store::version_dir(dir, k).join(store::ROWS))?;
    let session = prep.engine.session()?;
    let columns: Vec<(String, ColumnType)> = manifest.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
    session.load("v", &columns, &rows.batches)?;
    let pinned_at = crate::time::now();
    let filter = match view.filter {
        Some(f) => {
            let c = crate::formula::compile(f, &crate::formula::Scope { columns: &manifest.columns, pinned_at: &pinned_at })?;
            if c.ty != ColumnType::Boolean {
                return Err(Error::Invalid(format!("a filter is true or false, and `{f}` is {}", c.ty)));
            }
            format!(" WHERE {}", c.sql)
        }
        None => String::new(),
    };
    let mut order = Vec::new();
    for k in view.sort {
        let c = manifest
            .columns
            .iter()
            .find(|c| c.name.eq_ignore_ascii_case(&k.column))
            .ok_or_else(|| Error::Invalid(format!("there is no column '{}' to sort by", k.column)))?;
        order.push(format!("{} {} NULLS LAST", ident(&c.name), if k.desc { "DESC" } else { "ASC" }));
    }
    let order = if order.is_empty() { String::new() } else { format!(" ORDER BY {}", order.join(", ")) };
    let total = if filter.is_empty() { manifest.rows } else { session.count(&format!("SELECT count(*) FROM v{filter}"))? };
    let page = session.query(&format!("SELECT * FROM v{filter}{order} LIMIT {limit} OFFSET {offset}"))?;
    Ok((manifest, total, page))
}
