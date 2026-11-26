//! Running a recipe over the latest snapshots, and reading back what it
//! made.

use crate::engine::Session;
use crate::error::{Error, Result};
use crate::lineage::{Ancestry, Lineage, MadeBy, Origin, SourceRead, COMPILER_VERSION};
use crate::recipe::Recipe;
use crate::schema::{Column, ColumnType, Role};
use crate::store::{self, Dataset, Materialization, Snapshot};
use crate::Prep;
use std::path::Path;
use zsql_datasource::Batches;

/// A materialization, made and not yet kept.
pub(crate) struct Made {
    pub manifest: Materialization,
    pub lineage: Lineage,
    pub rows: Batches,
}

/// Loads each source's latest snapshot into the session as a table
/// named by the source's id.
pub(crate) fn load_sources(session: &Session, dir: &Path, dataset: &Dataset) -> Result<Vec<SourceRead>> {
    let mut read = Vec::new();
    for s in &dataset.sources {
        let (rows_path, manifest_path) = store::snapshot_paths(dir, &s.id, s.snapshot);
        let snapshot: Snapshot = store::read_json(&manifest_path)?;
        let rows = store::read_rows(&rows_path)?;
        let columns: Vec<(String, ColumnType)> = snapshot.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
        session.load(&s.id, &columns, &rows.batches)?;
        read.push(SourceRead {
            source: s.id.clone(),
            kind: snapshot.kind,
            snapshot: snapshot.number,
            file_name: snapshot.file_name,
            sha256: snapshot.sha256,
            parse: snapshot.parse,
        });
    }
    Ok(read)
}

/// Runs `recipe` over the dataset's latest snapshots as version `k`.
pub(crate) fn run(prep: &Prep, dir: &Path, dataset: &Dataset, recipe: &Recipe, k: u32) -> Result<Made> {
    let pinned_at = crate::time::now();
    let session = prep.engine.session()?;
    let sources = load_sources(&session, dir, dataset)?;
    let sql = crate::compile::recipe_sql(recipe, &dataset.sources)?;
    let rows = session.query(&sql)?;
    let n = rows.num_rows();
    if n > prep.settings.max_rows {
        return Err(Error::Unprocessable(format!("the recipe makes {n} rows, more than the {} a dataset holds", prep.settings.max_rows)));
    }
    let mut columns: Vec<Column> = Vec::new();
    for f in rows.schema.fields() {
        let ty = ColumnType::from_arrow(f.data_type())?;
        let role = dataset
            .sources
            .iter()
            .flat_map(|s| s.columns.iter())
            .find(|c| c.name == *f.name())
            .map(|c| c.role)
            .unwrap_or_else(|| Role::inferred(f.name(), ty));
        columns.push(Column { name: f.name().clone(), ty, role });
    }
    let mut notes = Vec::new();
    for missing in recipe.apply_roles(&mut columns) {
        notes.push(format!("the role given to column '{missing}' is not applied: the dataset no longer has it"));
    }
    // With no steps yet, each column is its source's column of the
    // same name.
    let ancestry: Vec<Ancestry> = columns
        .iter()
        .map(|c| Ancestry {
            column: c.name.clone(),
            from: dataset
                .sources
                .iter()
                .filter(|s| s.columns.iter().any(|sc| sc.name == c.name))
                .map(|s| Origin { source: s.id.clone(), column: c.name.clone() })
                .collect(),
        })
        .collect();
    let lineage = Lineage {
        dataset: dataset.id.clone(),
        version: k,
        recipe: recipe.version,
        edits: 0,
        sources,
        datasets: Vec::new(),
        roles: columns.iter().map(|c| (c.name.clone(), c.role)).collect(),
        made_by: MadeBy { zsql: env!("CARGO_PKG_VERSION").to_string(), compiler: COMPILER_VERSION, duckdb: prep.duckdb_version.clone() },
        pinned_at: pinned_at.clone(),
        notes,
        columns: ancestry,
    };
    let manifest = Materialization { version: k, made: pinned_at, columns, rows: n as u64, bytes: 0 };
    Ok(Made { manifest, lineage, rows })
}

/// Keeps a materialization as `v<k>`, whole or not at all, refused
/// past the byte cap.
pub(crate) fn keep(prep: &Prep, dir: &Path, made: &mut Made) -> Result<()> {
    let k = made.manifest.version;
    let target = store::version_dir(dir, k);
    let staging = dir.join(format!(".v{k}.staging-{}", crate::time::nanos()));
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
        std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))
    })();
    if result.is_err() {
        let _ = std::fs::remove_dir_all(&staging);
    }
    result
}

/// A page of a materialization's rows.
pub(crate) fn page(prep: &Prep, dir: &Path, k: u32, offset: u64, limit: u64) -> Result<(Materialization, Batches)> {
    let manifest: Materialization = store::read_json(&store::version_dir(dir, k).join("manifest.json"))?;
    let rows = store::read_rows(&store::version_dir(dir, k).join(store::ROWS))?;
    let session = prep.engine.session()?;
    let columns: Vec<(String, ColumnType)> = manifest.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
    session.load("v", &columns, &rows.batches)?;
    let page = session.query(&format!("SELECT * FROM v LIMIT {limit} OFFSET {offset}"))?;
    Ok((manifest, page))
}
