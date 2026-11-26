//! Data prep: datasets kept beside the semantic model, and never part
//! of it. See `docs/ingestion.md`.
//!
//! A dataset is a recipe over sources. A source brings rows, kept as an
//! immutable snapshot; the recipe is the steps that shape them; a
//! materialization is the recipe run over the latest snapshots, kept as
//! an immutable version with a record of what made it. Editing changes
//! the recipe and never a stored row, and a new snapshot runs the same
//! recipe again.
//!
//! A dataset belongs to an org, a project and an owner: an opaque
//! reference the application sends with every request, trusted as the
//! security context is, and kept only as a hash. Under any other owner
//! a dataset's id is not found.
//!
//! Dependencies point one way: this crate reads the model only as any
//! caller does, and nothing that plans a query knows this crate is
//! there. That is what keeps a dataset out of every query that did not
//! name it.
//!
//! - `source`: the kinds of source, and the CSV contract
//! - `snapshot`: rows that arrived, typed and met with their source's
//! - `schema`: column types and roles, and how a type evolves
//! - `recipe`: recipe versions and their steps
//! - `compile`: a recipe as the engine's SQL
//! - `engine`: DuckDB, in memory, with external access off
//! - `materialize`: running a recipe, and pages of what it made
//! - `lineage`: what each materialization was made from
//! - `store`: where datasets are kept, and the only module that knows

mod compile;
pub mod engine;
pub mod error;
pub mod lineage;
mod materialize;
pub mod recipe;
pub mod schema;
mod snapshot;
pub mod source;
pub mod store;
pub mod time;

pub use engine::{Engine, EngineLimits};
pub use error::{Error, Result};
pub use lineage::Lineage;
pub use recipe::{Change, Recipe};
pub use schema::{Column, ColumnType, Role};
pub use source::CsvOptions;
pub use store::{Dataset, Materialization, Snapshot, Store, DIR};

use sha2::Digest;
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zsql_datasource::Batches;

/// What a server's data prep is allowed.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The most rows a dataset holds, at import and after its recipe.
    pub max_rows: usize,
    /// The most bytes an upload, a snapshot or a materialization is.
    pub max_bytes: u64,
    pub engine: EngineLimits,
}

impl Settings {
    /// The defaults, spilling under `data_dir`.
    pub fn defaults(data_dir: &Path) -> Settings {
        Settings {
            max_rows: 1_000_000,
            max_bytes: 256 << 20,
            engine: EngineLimits { memory: "1GB".into(), threads: 2, spill: data_dir.join(".cache").join("prep") },
        }
    }
}

/// Who a request is for: an org, a project, and the owner the
/// application named.
#[derive(Clone, Debug)]
pub struct Scope {
    pub org: String,
    pub project: String,
    pub owner: Owner,
}

/// An owner, by the hash of the reference the application sent.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Owner(String);

/// The longest owner reference accepted.
const OWNER_LIMIT: usize = 1024;

impl Owner {
    /// The owner a reference names. An empty one is refused: there is
    /// no shared owner that a missing header falls back to.
    pub fn of(reference: &str) -> Result<Owner> {
        if reference.trim().is_empty() {
            return Err(Error::Invalid("an owner reference is needed for datasets, and the request has none".into()));
        }
        if reference.len() > OWNER_LIMIT {
            return Err(Error::Invalid(format!("an owner reference is at most {OWNER_LIMIT} bytes")));
        }
        let digest = sha2::Sha256::digest(format!("zsql-prep-owner\n{reference}").as_bytes());
        Ok(Owner(digest.iter().map(|b| format!("{b:02x}")).collect()))
    }

    /// The hash, which is all that is kept or logged.
    pub fn hash(&self) -> &str {
        &self.0
    }
}

/// An upload: its bytes, the name it had, and how to read it.
pub struct Upload<'a> {
    pub file_name: Option<String>,
    pub bytes: &'a [u8],
    pub options: CsvOptions,
}

/// A page of rows.
pub struct Page {
    pub version: u32,
    pub columns: Vec<Column>,
    pub total: u64,
    pub offset: u64,
    pub rows: Batches,
}

/// The longest name a dataset is given.
const NAME_LIMIT: usize = 200;

/// The one source an upload feeds.
const UPLOAD_SOURCE: &str = "s1";

pub struct Prep {
    store: Store,
    pub(crate) engine: Engine,
    pub(crate) settings: Settings,
    pub(crate) duckdb_version: String,
    /// The datasets a job is running on: one at a time each.
    busy: Mutex<HashSet<PathBuf>>,
}

/// A dataset held for one job; let go when dropped.
struct Job<'a> {
    prep: &'a Prep,
    dir: PathBuf,
}

impl Drop for Job<'_> {
    fn drop(&mut self) {
        self.prep.busy.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&self.dir);
    }
}

impl Prep {
    pub fn open(data_dir: &Path, settings: Settings) -> Result<Prep> {
        let engine = Engine::new(settings.engine.clone());
        let duckdb_version = engine.version()?;
        Ok(Prep { store: Store::new(data_dir), engine, settings, duckdb_version, busy: Mutex::new(HashSet::new()) })
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    fn job(&self, dir: &Path) -> Result<Job<'_>> {
        let mut busy = self.busy.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        if !busy.insert(dir.to_path_buf()) {
            return Err(Error::Conflict("this dataset is busy with another job; try again when it is done".into()));
        }
        Ok(Job { prep: self, dir: dir.to_path_buf() })
    }

    /// The directory of a dataset this owner has, or not found.
    fn find(&self, scope: &Scope, id: &str) -> Result<(PathBuf, Dataset)> {
        let missing = || Error::NotFound(format!("no dataset {id} in project {}", scope.project));
        if !is_id(id) {
            return Err(missing());
        }
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        let dataset: Dataset = match store::read_json(&dir.join(store::MANIFEST)) {
            Ok(d) => d,
            Err(_) if !dir.join(store::MANIFEST).exists() => return Err(missing()),
            Err(e) => return Err(e),
        };
        if dataset.owner != scope.owner.hash() {
            return Err(missing());
        }
        Ok((dir, dataset))
    }

    /// This owner's datasets in the project.
    pub fn list(&self, scope: &Scope) -> Vec<Dataset> {
        self.store.all(&scope.org, &scope.project).into_iter().map(|(_, d)| d).filter(|d| d.owner == scope.owner.hash()).collect()
    }

    pub fn get(&self, scope: &Scope, id: &str) -> Result<Dataset> {
        Ok(self.find(scope, id)?.1)
    }

    /// A new dataset, from an upload: its first snapshot, its first
    /// recipe, and its first materialization, made live.
    pub fn create(&self, scope: &Scope, name: Option<&str>, upload: Upload<'_>) -> Result<Dataset> {
        let name = dataset_name(name, upload.file_name.as_deref())?;
        self.check_bytes(upload.bytes.len())?;
        let id = new_id(&scope.project);
        let root = self.store.root(&scope.org, &scope.project);
        let staging = root.join(format!(".new-{id}"));
        let result: Result<Dataset> = (|| {
            let now = time::now();
            let taken = self.take(&staging, UPLOAD_SOURCE, 1, &upload, &[])?;
            let mut dataset = Dataset {
                id: id.clone(),
                name,
                owner: scope.owner.hash().to_string(),
                created: now.clone(),
                updated: now.clone(),
                recipe: 1,
                live: None,
                made: 0,
                sources: vec![store::SourceState { id: UPLOAD_SOURCE.into(), kind: source::Kind::Upload, snapshot: 1, columns: taken }],
                columns: Vec::new(),
                rows: 0,
                notes: Vec::new(),
            };
            let recipe = Recipe::first(now);
            store::write_json(&store::recipe_path(&staging, 1), &recipe)?;
            self.materialize(&staging, &mut dataset, &recipe)?;
            store::write_json(&staging.join(store::MANIFEST), &dataset)?;
            let target = root.join(&id);
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))?;
            Ok(dataset)
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        let dataset = result?;
        tracing::info!(
            "dataset {} made in {}/{} for owner {}: {} rows",
            dataset.id,
            scope.org,
            scope.project,
            &scope.owner.hash()[..12],
            dataset.rows
        );
        Ok(dataset)
    }

    /// A new snapshot of the dataset's upload, and the recipe run over
    /// it. The snapshot is kept even when the recipe then fails, and the
    /// last materialization stays live.
    pub fn replace(&self, scope: &Scope, id: &str, upload: Upload<'_>) -> Result<Dataset> {
        self.check_bytes(upload.bytes.len())?;
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir)?;
        let (_, mut dataset) = self.find(scope, id)?;
        let Some(index) = dataset.sources.iter().position(|s| s.kind == source::Kind::Upload) else {
            return Err(Error::Invalid(format!("dataset {id} is not fed by an upload")));
        };
        let source = dataset.sources[index].clone();
        let n = source.snapshot + 1;
        let columns = self.take(&dir, &source.id, n, &upload, &source.columns)?;
        dataset.sources[index] = store::SourceState { snapshot: n, columns, ..source };
        dataset.updated = time::now();
        let recipe: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let made = self.materialize(&dir, &mut dataset, &recipe);
        if made.is_err() {
            // The snapshot is what arrived, and stays; what is live does not change.
            dataset.notes =
                vec![format!("snapshot {n} was taken and the recipe failed on it; version {} is still live", dataset.live.unwrap_or(0))];
        }
        store::write_json(&dir.join(store::MANIFEST), &dataset)?;
        made?;
        Ok(dataset)
    }

    /// A new version of the recipe, made against the latest, and run.
    /// Nothing is kept unless the run succeeds.
    pub fn change(&self, scope: &Scope, id: &str, change: Change) -> Result<Dataset> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir)?;
        let (_, dataset) = self.find(scope, id)?;
        let current: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let recipe = current.changed(change, time::now())?;
        let known: Vec<&str> = dataset.columns.iter().map(|c| c.name.as_str()).collect();
        if let Some(name) = recipe.roles.keys().find(|n| !known.contains(&n.as_str())) {
            return Err(Error::Invalid(format!("a role is given to column '{name}', which the dataset does not have")));
        }
        let mut next = dataset.clone();
        self.materialize(&dir, &mut next, &recipe)?;
        store::write_json(&store::recipe_path(&dir, recipe.version), &recipe)?;
        next.recipe = recipe.version;
        next.updated = time::now();
        store::write_json(&dir.join(store::MANIFEST), &next)?;
        Ok(next)
    }

    /// A recipe version: the latest, or the one asked for.
    pub fn recipe(&self, scope: &Scope, id: &str, version: Option<u32>) -> Result<Recipe> {
        let (dir, dataset) = self.find(scope, id)?;
        let m = version.unwrap_or(dataset.recipe);
        if m == 0 || m > dataset.recipe {
            return Err(Error::NotFound(format!("dataset {id} has recipe versions 1 to {}", dataset.recipe)));
        }
        store::read_json(&store::recipe_path(&dir, m))
    }

    /// A page of a materialization: the live one, or the one asked for.
    pub fn page(&self, scope: &Scope, id: &str, version: Option<u32>, offset: u64, limit: u64) -> Result<Page> {
        let (dir, dataset) = self.find(scope, id)?;
        let k = self.version_of(&dir, &dataset, version)?;
        let (manifest, rows) = materialize::page(self, &dir, k, offset, limit)?;
        Ok(Page { version: k, columns: manifest.columns, total: manifest.rows, offset, rows })
    }

    /// What made a materialization: the live one, or the one asked for.
    pub fn lineage(&self, scope: &Scope, id: &str, version: Option<u32>) -> Result<Lineage> {
        let (dir, dataset) = self.find(scope, id)?;
        let k = self.version_of(&dir, &dataset, version)?;
        store::read_lineage(&dir, k)
    }

    /// A snapshot's manifest: the latest of a source, or the one asked
    /// for.
    pub fn snapshot(&self, scope: &Scope, id: &str, source: &str, number: Option<u32>) -> Result<Snapshot> {
        let (dir, dataset) = self.find(scope, id)?;
        let Some(s) = dataset.sources.iter().find(|s| s.id == source) else {
            return Err(Error::NotFound(format!("dataset {id} has no source {source}")));
        };
        let n = number.unwrap_or(s.snapshot);
        if n == 0 || n > s.snapshot {
            return Err(Error::NotFound(format!("source {source} of dataset {id} has snapshots 1 to {}", s.snapshot)));
        }
        store::read_json(&store::snapshot_paths(&dir, source, n).1)
    }

    /// Removes a dataset: every snapshot, recipe version and
    /// materialization of it.
    pub fn delete(&self, scope: &Scope, id: &str) -> Result<()> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir)?;
        store::remove(&dir)?;
        tracing::info!("dataset {id} removed from {}/{}", scope.org, scope.project);
        Ok(())
    }

    /// Removes every dataset an owner has in an org, in every project:
    /// what an application does when it removes a user. The ids
    /// removed.
    pub fn delete_owner(&self, org: &str, owner: &Owner) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for project in self.store.projects(org) {
            let scope = Scope { org: org.to_string(), project, owner: owner.clone() };
            for d in self.list(&scope) {
                self.delete(&scope, &d.id)?;
                removed.push(d.id);
            }
        }
        Ok(removed)
    }

    /// Removes every dataset of a project, whoever owns it: part of
    /// removing the project. The ids removed.
    pub fn delete_project(&self, org: &str, project: &str) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for (dir, d) in self.store.all(org, project) {
            store::remove(&dir)?;
            removed.push(d.id);
        }
        let root = self.store.root(org, project);
        if root.exists() {
            store::remove(&root)?;
        }
        Ok(removed)
    }

    fn version_of(&self, dir: &Path, dataset: &Dataset, version: Option<u32>) -> Result<u32> {
        let k = match version.or(dataset.live) {
            Some(k) => k,
            None => return Err(Error::NotFound(format!("dataset {} has no materialization yet", dataset.id))),
        };
        if !store::version_dir(dir, k).join("manifest.json").exists() {
            return Err(Error::NotFound(format!("dataset {} has no version {k}", dataset.id)));
        }
        Ok(k)
    }

    fn check_bytes(&self, n: usize) -> Result<()> {
        if n as u64 > self.settings.max_bytes {
            return Err(Error::Unprocessable(format!("this file is {n} bytes, more than the {} a dataset takes", self.settings.max_bytes)));
        }
        Ok(())
    }

    /// Takes snapshot `n` of a source into `dir`; its columns.
    fn take(&self, dir: &Path, source: &str, n: u32, upload: &Upload<'_>, established: &[Column]) -> Result<Vec<Column>> {
        let session = self.engine.session()?;
        let taken = snapshot::from_csv(&session, upload.bytes, &upload.options, established, self.settings.max_rows)?;
        let (rows_path, manifest_path) = store::snapshot_paths(dir, source, n);
        let bytes = store::write_rows(&rows_path, &taken.rows)?;
        let digest = sha2::Sha256::digest(upload.bytes);
        let snapshot = Snapshot {
            source: source.to_string(),
            number: n,
            kind: source::Kind::Upload,
            taken: time::now(),
            file_name: upload.file_name.clone(),
            sha256: digest.iter().map(|b| format!("{b:02x}")).collect(),
            bytes,
            parse: Some(taken.parse),
            columns: taken.columns.clone(),
            rows: taken.rows.num_rows() as u64,
            changes: taken.changes,
        };
        store::write_json(&manifest_path, &snapshot)?;
        Ok(taken.columns)
    }

    /// Runs the recipe as the next version and makes it live.
    fn materialize(&self, dir: &Path, dataset: &mut Dataset, recipe: &Recipe) -> Result<()> {
        let k = dataset.made + 1;
        let mut made = materialize::run(self, dir, dataset, recipe, k)?;
        materialize::keep(self, dir, &mut made)?;
        let mut notes: Vec<String> = Vec::new();
        for s in &dataset.sources {
            if let Ok(snap) = store::read_json::<Snapshot>(&store::snapshot_paths(dir, &s.id, s.snapshot).1) {
                notes.extend(snap.changes);
            }
        }
        notes.extend(made.lineage.notes.iter().cloned());
        dataset.made = k;
        dataset.live = Some(k);
        dataset.columns = made.manifest.columns;
        dataset.rows = made.manifest.rows;
        dataset.notes = notes;
        Ok(())
    }
}

/// A dataset's id: `ds_` and sixteen hex digits, unguessable enough
/// that one is never made twice, and checked before it names a
/// directory.
fn new_id(salt: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let digest = sha2::Sha256::digest(format!("{}\n{}\n{n}\n{salt}", time::nanos(), std::process::id()).as_bytes());
    format!("ds_{}", digest.iter().take(8).map(|b| format!("{b:02x}")).collect::<String>())
}

pub fn is_id(id: &str) -> bool {
    id.strip_prefix("ds_").is_some_and(|h| h.len() == 16 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
}

fn dataset_name(name: Option<&str>, file_name: Option<&str>) -> Result<String> {
    let from_file = file_name.map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f)).map(|f| f.strip_suffix(".csv").unwrap_or(f).to_string());
    let name = name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).or(from_file).unwrap_or_else(|| "untitled".into());
    if name.chars().count() > NAME_LIMIT || name.chars().any(char::is_control) {
        return Err(Error::Invalid(format!("a dataset's name is at most {NAME_LIMIT} characters, none of them control characters")));
    }
    Ok(name)
}

#[cfg(test)]
mod tests;
