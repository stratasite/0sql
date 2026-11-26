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
//! - `formula`: the formula language, compiled to the engine's SQL
//! - `compile`: a recipe as the engine's SQL, and the column graph
//! - `edits`: the edit log: remaps, overrides, rows added and deleted
//! - `engine`: DuckDB, in memory, with external access off
//! - `materialize`: running a recipe, and pages of what it made
//! - `lineage`: what each materialization was made from
//! - `store`: where datasets are kept, and the only module that knows

pub mod compile;
pub mod edits;
pub mod engine;
pub mod error;
pub mod formula;
pub mod lineage;
mod materialize;
pub mod recipe;
pub mod schema;
mod snapshot;
pub mod source;
pub mod store;
pub mod time;

pub use compile::Unresolved;
pub use edits::{Asked, Edit, Entry, Outcome, State};
pub use engine::{Engine, EngineLimits};
pub use error::{Error, Result};
pub use lineage::Lineage;
pub use materialize::View;
pub use recipe::{Change, Recipe, SortKey, Step};
pub use schema::{Column, ColumnType, Role};
pub use source::{CsvOptions, Query, SemanticRead};
pub use store::{Dataset, Held, Materialization, Snapshot, Store, DIR};

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

/// What an edit is checked against: the live version, loaded as `v`,
/// and the log so far.
#[derive(Clone, Copy)]
struct EditCx<'a> {
    session: &'a engine::Session,
    columns: &'a [Column],
    identity: &'a [String],
    in_force: &'a [(usize, Edit)],
    log: &'a [Entry],
}

/// Rows a semantic query returned, with the model's roles for its
/// columns and what made them.
pub struct Arrived {
    pub rows: Batches,
    pub roles: std::collections::BTreeMap<String, Role>,
    pub read: source::SemanticRead,
}

/// What a preview is asked for.
#[derive(Clone, Debug, Default, serde::Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Preview {
    pub steps: Option<Vec<Step>>,
    pub roles: Option<std::collections::BTreeMap<String, Role>>,
    pub upto: Option<usize>,
    pub limit: Option<u64>,
}

/// A preview's answer.
pub struct Previewed {
    pub columns: Vec<Column>,
    pub rows: Batches,
    pub unresolved: Vec<Unresolved>,
    /// How many rows of each source it was made from, at most.
    pub sample: usize,
}

/// How many rows of each source a preview reads.
const PREVIEW_SAMPLE: usize = 1000;

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
    /// Queries still running for a dataset, or that failed after their
    /// request was answered, by the dataset's directory.
    pending: Mutex<std::collections::HashMap<PathBuf, Pending>>,
}

/// A query a dataset waits on, run after its request was answered.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Pending {
    #[serde(skip)]
    owner: String,
    /// The run, by its id under `/results`.
    pub result: String,
    pub since: String,
    /// Set when it failed: the dataset was not made or not refreshed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
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
        Ok(Prep {
            store: Store::new(data_dir),
            engine,
            settings,
            duckdb_version,
            busy: Mutex::new(HashSet::new()),
            pending: Mutex::new(std::collections::HashMap::new()),
        })
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

    /// Says a query runs on for a dataset, after its request was
    /// answered; `failed` says it ended without a snapshot; `done` that
    /// it made one.
    pub fn waiting(&self, scope: &Scope, id: &str, result: &str) {
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        let p = Pending { owner: scope.owner.hash().to_string(), result: result.to_string(), since: time::now(), failed: None };
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).insert(dir, p);
    }

    pub fn failed(&self, scope: &Scope, id: &str, why: &str) {
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        if let Some(p) = self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get_mut(&dir) {
            p.failed = Some(why.to_string());
        }
    }

    pub fn done(&self, scope: &Scope, id: &str) {
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).remove(&dir);
    }

    /// The query a dataset of this owner waits on, if one does.
    pub fn pending(&self, scope: &Scope, id: &str) -> Option<Pending> {
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&dir).filter(|p| p.owner == scope.owner.hash()).cloned()
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
        self.create_with(scope, &new_id(&scope.project), name, None, |staging| self.take(staging, UPLOAD_SOURCE, 1, &upload, &[]))
    }

    /// A new dataset, from rows a semantic query returned, under the id
    /// given (`new_id`), so a caller that answered before the query
    /// finished can say which it will be.
    pub fn create_from_query(
        &self,
        scope: &Scope,
        id: &str,
        name: Option<&str>,
        query: source::Query,
        arrived: Arrived,
    ) -> Result<Dataset> {
        if !is_id(id) {
            return Err(Error::Invalid(format!("'{id}' is not a dataset id")));
        }
        let name = dataset_name(name, Some(&query.branch).map(|b| format!("query on {b}")).as_deref())?;
        self.create_with(scope, id, name, Some(query), |staging| self.take_rows(staging, UPLOAD_SOURCE, 1, &arrived, &[]))
    }

    fn create_with(
        &self,
        scope: &Scope,
        id: &str,
        name: String,
        query: Option<source::Query>,
        take: impl FnOnce(&Path) -> Result<Vec<Column>>,
    ) -> Result<Dataset> {
        let id = id.to_string();
        let root = self.store.root(&scope.org, &scope.project);
        if root.join(&id).exists() {
            return Err(Error::Conflict(format!("dataset {id} is there already")));
        }
        let staging = root.join(format!(".new-{id}"));
        let result: Result<Dataset> = (|| {
            let now = time::now();
            let kind = if query.is_some() { source::Kind::Semantic } else { source::Kind::Upload };
            let taken = take(&staging)?;
            let mut dataset = Dataset {
                id: id.clone(),
                name,
                owner: scope.owner.hash().to_string(),
                created: now.clone(),
                updated: now.clone(),
                recipe: 1,
                live: None,
                made: 0,
                sources: vec![store::SourceState { id: UPLOAD_SOURCE.into(), kind, snapshot: 1, columns: taken, query }],
                columns: Vec::new(),
                rows: 0,
                notes: Vec::new(),
                held: None,
                edits: 0,
            };
            let recipe = Recipe::first(now);
            store::write_json(&store::recipe_path(&staging, 1), &recipe)?;
            if self.materialize(&staging, &mut dataset, &recipe, &[])?.is_some() {
                return Err(Error::Internal("a first recipe has no steps to hold it".into()));
            }
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

    /// What a dataset's semantic source runs, for the caller that runs
    /// it.
    pub fn query(&self, scope: &Scope, id: &str) -> Result<source::Query> {
        let (_, dataset) = self.find(scope, id)?;
        dataset
            .sources
            .iter()
            .find_map(|s| s.query.clone())
            .ok_or_else(|| Error::Invalid(format!("dataset {id} is not made from a semantic query; there is nothing to refresh")))
    }

    /// A new snapshot of a dataset's semantic source, from rows its query
    /// returned, and the recipe run over it; as a replaced upload is.
    pub fn refresh(&self, scope: &Scope, id: &str, arrived: Arrived) -> Result<Dataset> {
        self.resnapshot(scope, id, source::Kind::Semantic, |prep, dir, source, n, established| {
            prep.take_rows(dir, source, n, &arrived, established)
        })
    }

    /// A new snapshot of the dataset's upload, and the recipe run over
    /// it. The snapshot is kept even when the recipe then fails, and the
    /// last materialization stays live.
    pub fn replace(&self, scope: &Scope, id: &str, upload: Upload<'_>) -> Result<Dataset> {
        self.check_bytes(upload.bytes.len())?;
        self.resnapshot(scope, id, source::Kind::Upload, |prep, dir, source, n, established| {
            prep.take(dir, source, n, &upload, established)
        })
    }

    fn resnapshot(
        &self,
        scope: &Scope,
        id: &str,
        kind: source::Kind,
        take: impl FnOnce(&Prep, &Path, &str, u32, &[Column]) -> Result<Vec<Column>>,
    ) -> Result<Dataset> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir)?;
        let (_, mut dataset) = self.find(scope, id)?;
        let Some(index) = dataset.sources.iter().position(|s| s.kind == kind) else {
            let what = match kind {
                source::Kind::Upload => "an upload",
                source::Kind::Semantic => "a semantic query",
            };
            return Err(Error::Invalid(format!("dataset {id} is not fed by {what}")));
        };
        let source = dataset.sources[index].clone();
        let n = source.snapshot + 1;
        let columns = take(self, &dir, &source.id, n, &source.columns)?;
        dataset.sources[index] = store::SourceState { snapshot: n, columns, ..source };
        dataset.updated = time::now();
        let recipe: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let log = edits::read_log(&dir)?;
        let made = self.materialize(&dir, &mut dataset, &recipe, &log);
        match &made {
            Ok(None) => dataset.held = None,
            Ok(Some(unresolved)) => {
                // Columns the recipe reads are gone: nothing is made, what
                // is live stays, and each reader is named.
                dataset.notes = snapshot_notes(&dir, &dataset);
                dataset.notes.push(format!(
                    "snapshot {n} is held: {}; change the recipe, or confirm to drop those steps",
                    unresolved.iter().map(Unresolved::describe).collect::<Vec<_>>().join("; ")
                ));
                dataset.held = Some(Held { since: time::now(), unresolved: unresolved.clone() });
            }
            Err(_) => {
                // The snapshot is what arrived, and stays; what is live does not change.
                dataset.notes = vec![format!(
                    "snapshot {n} was taken and the recipe failed on it; version {} is still live",
                    dataset.live.unwrap_or(0)
                )];
            }
        }
        store::write_json(&dir.join(store::MANIFEST), &dataset)?;
        made?;
        Ok(dataset)
    }

    /// A new version of the recipe, made against the latest, and run.
    /// Nothing is kept unless the run succeeds; a recipe that reads a
    /// column not there at its step is refused, naming each.
    pub fn change(&self, scope: &Scope, id: &str, change: Change) -> Result<Dataset> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir)?;
        let (_, dataset) = self.find(scope, id)?;
        let current: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let recipe = current.changed(change, time::now())?;
        self.commit(&dir, dataset, recipe, None)
    }

    /// Takes a held dataset past what held it: a recipe version without
    /// the steps and roles that read the columns that are gone, run
    /// over the latest snapshots.
    pub fn confirm(&self, scope: &Scope, id: &str, against: u32) -> Result<Dataset> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir)?;
        let (_, dataset) = self.find(scope, id)?;
        let Some(held) = dataset.held.clone() else {
            return Err(Error::Invalid(format!("dataset {id} is not held; there is nothing to confirm")));
        };
        let current: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let steps_of = |u: &&Unresolved| u.edit.is_none() && u.step > 0;
        let dropped: Vec<usize> = held.unresolved.iter().filter(steps_of).map(|u| u.step).collect();
        let gone = |op: &str, name: &str| held.unresolved.iter().any(|u| u.edit.is_none() && u.step == 0 && u.op == op && u.column == name);
        let change = Change {
            against,
            roles: current.roles.iter().filter(|(n, _)| !gone("role", n)).map(|(n, r)| (n.clone(), *r)).collect(),
            steps: current.steps.iter().enumerate().filter(|(i, _)| !dropped.contains(&(i + 1))).map(|(_, s)| s.clone()).collect(),
            key: current.key.iter().filter(|k| !gone("key", k)).cloned().collect(),
        };
        let recipe = current.changed(change, time::now())?;
        // The edits that held it are undone, as an entry of the log.
        let mut undone: Vec<&str> = held.unresolved.iter().filter_map(|u| u.edit.as_deref()).collect();
        undone.sort();
        undone.dedup();
        let entry = (!undone.is_empty()).then(|| {
            let seq = dataset.edits + 1;
            Entry {
                seq,
                at: time::now(),
                recipe: recipe.version,
                after: recipe.steps.len(),
                edits: undone.iter().enumerate().map(|(i, u)| Edit::Undo { id: edits::id_of(seq, i), undoes: u.to_string() }).collect(),
            }
        });
        self.commit(&dir, dataset, recipe, entry)
    }

    /// Runs a new recipe version and, when it makes a materialization,
    /// keeps both.
    fn commit(&self, dir: &Path, dataset: Dataset, recipe: Recipe, entry: Option<Entry>) -> Result<Dataset> {
        let mut next = dataset;
        let mut log = edits::read_log(dir)?;
        if let Some(e) = &entry {
            log.push(e.clone());
            next.edits = e.seq;
        }
        if let Some(unresolved) = self.materialize(dir, &mut next, &recipe, &log)? {
            return Err(Error::Invalid(format!(
                "the recipe reads columns that are not there: {}",
                unresolved.iter().map(Unresolved::describe).collect::<Vec<_>>().join("; ")
            )));
        }
        store::write_json(&store::recipe_path(dir, recipe.version), &recipe)?;
        if let Some(e) = &entry {
            edits::append(dir, e)?;
        }
        next.recipe = recipe.version;
        next.updated = time::now();
        next.held = None;
        store::write_json(&dir.join(store::MANIFEST), &next)?;
        Ok(next)
    }

    /// A recipe's result over a sample of the latest snapshots, kept
    /// nowhere: what the grid shows while a step is being made. The
    /// steps are the recipe's, the first `upto` of them, or the ones
    /// given.
    pub fn preview(&self, scope: &Scope, id: &str, preview: Preview) -> Result<Previewed> {
        let (dir, dataset) = self.find(scope, id)?;
        let recipe: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let mut steps = preview.steps.unwrap_or(recipe.steps);
        if steps.len() > recipe::STEPS_LIMIT {
            return Err(Error::Invalid(format!("a recipe has at most {} steps", recipe::STEPS_LIMIT)));
        }
        if let Some(n) = preview.upto {
            steps.truncate(n);
        }
        let roles = preview.roles.unwrap_or(recipe.roles);
        let pinned_at = time::now();
        let log = edits::read_log(&dir)?;
        let compiled =
            compile::compile_all(&steps, &roles, &recipe.key, &edits::in_force(&log, dataset.edits), &dataset.sources, &pinned_at)?;
        let session = self.engine.session()?;
        materialize::load_sources(&session, &dir, &dataset, Some(PREVIEW_SAMPLE))?;
        materialize::prepare(&session, &compiled)?;
        let list: Vec<String> = compiled.columns.iter().map(|c| schema::ident(&c.name)).collect();
        let limit = preview.limit.unwrap_or(100).min(PREVIEW_SAMPLE as u64);
        let rows = session.query(&format!("SELECT {} FROM {} LIMIT {limit}", list.join(", "), compiled.last))?;
        Ok(Previewed { columns: compiled.columns, rows, unresolved: compiled.unresolved, sample: PREVIEW_SAMPLE })
    }

    /// What reads a column: the steps and roles of the recipe that would
    /// not resolve if it were gone from where it is made.
    pub fn impact(&self, scope: &Scope, id: &str, column: &str) -> Result<Vec<Unresolved>> {
        let (dir, dataset) = self.find(scope, id)?;
        let recipe: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let pinned_at = time::now();
        let empty = std::collections::BTreeMap::new();
        // Where the column is first there: a source, or the step that makes it.
        let mut made_at = None;
        for k in 0..=recipe.steps.len() {
            let c = compile::compile_steps(&recipe.steps[..k], &empty, &dataset.sources, &pinned_at)?;
            if c.columns.iter().any(|x| x.name.eq_ignore_ascii_case(column)) {
                made_at = Some(k);
                break;
            }
        }
        let Some(k) = made_at else {
            return Err(Error::NotFound(format!("dataset {id} has no column '{column}' at any step")));
        };
        let mut steps = recipe.steps.clone();
        steps.insert(k, Step::Remove { columns: vec![column.to_string()] });
        let log = edits::read_log(&dir)?;
        let in_force: Vec<(usize, Edit)> =
            edits::in_force(&log, dataset.edits).into_iter().map(|(a, e)| (if a >= k { a + 1 } else { a }, e)).collect();
        let c = compile::compile_all(&steps, &recipe.roles, &recipe.key, &in_force, &dataset.sources, &pinned_at)?;
        // Told by the recipe's own numbers, without the removal put in.
        Ok(c.unresolved
            .into_iter()
            .map(|mut u| {
                if u.step > k {
                    u.step -= 1;
                }
                u
            })
            .collect())
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
    pub fn page(&self, scope: &Scope, id: &str, version: Option<u32>, offset: u64, limit: u64, view: &View<'_>) -> Result<Page> {
        let (dir, dataset) = self.find(scope, id)?;
        let k = self.version_of(&dir, &dataset, version)?;
        let (manifest, total, rows) = materialize::page(self, &dir, k, offset, limit, view)?;
        Ok(Page { version: k, columns: manifest.columns, total, offset, rows })
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

    /// Edits made in the grid, against the latest recipe version: one
    /// entry of the log, and the recipe run with it. Each is checked
    /// against the live version first, so an edit that names no row, or
    /// more rows than it was made for, is refused before it is kept.
    pub fn edit(&self, scope: &Scope, id: &str, request: edits::Request) -> Result<(Dataset, Vec<Outcome>)> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir)?;
        let (_, dataset) = self.find(scope, id)?;
        if request.against != dataset.recipe {
            return Err(Error::Conflict(format!(
                "these edits were made against recipe version {}, and the recipe is at version {} now; read it again",
                request.against, dataset.recipe
            )));
        }
        if let Some(held) = &dataset.held {
            return Err(Error::Conflict(format!(
                "dataset {id} is held ({}); change the recipe or confirm before editing",
                held.unresolved.iter().map(Unresolved::describe).collect::<Vec<_>>().join("; ")
            )));
        }
        if request.edits.is_empty() || request.edits.len() > edits::ENTRY_LIMIT {
            return Err(Error::Invalid(format!("an entry holds 1 to {} edits", edits::ENTRY_LIMIT)));
        }
        let recipe: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let log = edits::read_log(&dir)?;
        let in_force = edits::in_force(&log, dataset.edits);
        let k = dataset.live.ok_or_else(|| Error::Internal(format!("dataset {id} has no live version")))?;
        let manifest: Materialization = store::read_json(&store::version_dir(&dir, k).join("manifest.json"))?;
        let rows = store::read_rows(&store::version_dir(&dir, k).join(store::ROWS))?;
        let session = self.engine.session()?;
        let typed: Vec<(String, ColumnType)> = manifest.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
        session.load("v", &typed, &rows.batches)?;
        let columns = &manifest.columns;
        let identity = edits::identity(columns, &recipe.key);
        let seq = dataset.edits + 1;
        let mut made = Vec::new();
        for (i, asked) in request.edits.into_iter().enumerate() {
            let eid = edits::id_of(seq, i);
            let at = |e: Error| match e {
                Error::Invalid(m) => Error::Invalid(format!("edit {} : {m}", i + 1).replacen(" :", ":", 1)),
                other => other,
            };
            let cx = EditCx { session: &session, columns, identity: &identity, in_force: &in_force, log: &log };
            made.push(Self::one_edit(&cx, eid, asked).map_err(at)?);
        }
        let entry = Entry { seq, at: time::now(), recipe: dataset.recipe, after: recipe.steps.len(), edits: made };
        let mut next = dataset.clone();
        let mut log = log;
        log.push(entry.clone());
        next.edits = seq;
        if let Some(u) = self.materialize(&dir, &mut next, &recipe, &log)? {
            return Err(Error::Invalid(format!(
                "the edits read columns that are not there: {}",
                u.iter().map(Unresolved::describe).collect::<Vec<_>>().join("; ")
            )));
        }
        edits::append(&dir, &entry)?;
        next.updated = time::now();
        store::write_json(&dir.join(store::MANIFEST), &next)?;
        let outcomes = edits::read_outcomes(&dir, next.live.unwrap_or(0));
        Ok((next, outcomes))
    }

    fn one_edit(cx: &EditCx<'_>, id: String, asked: Asked) -> Result<Edit> {
        let EditCx { session, columns, identity, in_force, log } = *cx;
        let column_of = |name: &str| {
            columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)).ok_or_else(|| Error::Invalid(format!("there is no column '{name}'")))
        };
        // The rows of the live version a key names, and the first one's
        // value of `column`.
        let matching = |key: &std::collections::BTreeMap<String, serde_json::Value>,
                        column: Option<&Column>|
         -> Result<(u64, serde_json::Value)> {
            let mut conds = Vec::new();
            for (c, v) in key {
                let col = column_of(c)?;
                compile::value_sql(v, col.ty, &col.name)?;
                match edits::text_of(v) {
                    Some(t) => conds.push(format!("{} = TRY_CAST({} AS {})", schema::ident(&col.name), schema::literal(&t), col.ty.sql())),
                    None => conds.push(format!("{} IS NULL", schema::ident(&col.name))),
                }
            }
            let cond = if conds.is_empty() { "TRUE".to_string() } else { conds.join(" AND ") };
            let n = session.count(&format!("SELECT count(*) FROM v WHERE {cond}"))?;
            let value = match column {
                Some(c) if n > 0 => {
                    let r = session.query(&format!("SELECT {} FROM v WHERE {cond} LIMIT 1", schema::ident(&c.name)))?;
                    zsql_exec::json::rows(&r)
                        .map_err(|e| Error::Internal(e.to_string()))?
                        .first()
                        .and_then(|row| row.get(0).cloned())
                        .unwrap_or_default()
                }
                _ => serde_json::Value::Null,
            };
            Ok((n, value))
        };
        let names_one = |n: u64, all: bool| -> Result<()> {
            match n {
                0 => Err(Error::Invalid("no row has these values".into())),
                1 => Ok(()),
                _ if all => Ok(()),
                _ => Err(Error::Invalid(format!(
                    "{n} rows have these values: the dimensions do not name one row; mark a key in the recipe, or send all: true to edit every one"
                ))),
            }
        };
        Ok(match asked {
            Asked::Cell { column, row, from, to, one, all } => {
                let c = column_of(&column)?.clone();
                compile::value_sql(&to, c.ty, &c.name)?;
                if c.role == Role::Dimension && !one {
                    let from = from
                        .or_else(|| {
                            row.as_ref().and_then(|r| r.iter().find(|(n, _)| n.eq_ignore_ascii_case(&c.name)).map(|(_, v)| v.clone()))
                        })
                        .ok_or_else(|| Error::Invalid(format!("a remap of '{}' says the value it changes: from, or the row", c.name)))?;
                    compile::value_sql(&from, c.ty, &c.name)?;
                    Edit::Remap { id, column: c.name, from, to }
                } else {
                    let row = row.ok_or_else(|| Error::Invalid(format!("an edit of '{}' for one row says which row: row", c.name)))?;
                    let key = edits::key_of(&row, identity)?;
                    let (n, current) = matching(&key, Some(&c))?;
                    names_one(n, all)?;
                    // The value first replaced is what a sync is checked against.
                    let earlier = in_force.iter().rev().find_map(|(_, e)| match e {
                        Edit::Set { column: col, key: k, was, .. } if *col == c.name && *k == key => Some(was.clone()),
                        _ => None,
                    });
                    let was = earlier.unwrap_or(if n == 1 { current } else { serde_json::Value::Null });
                    Edit::Set { id, column: c.name, key, was, to, all: all && n > 1 }
                }
            }
            Asked::Delete { row, all } => {
                let key = edits::key_of(&row, identity)?;
                names_one(matching(&key, None)?.0, all)?;
                Edit::Delete { id, key, all }
            }
            Asked::Add { values } => {
                let key = edits::key_of(&values, identity)?;
                for (name, v) in &values {
                    let c = column_of(name)?;
                    compile::value_sql(v, c.ty, &c.name)?;
                }
                if matching(&key, None)?.0 > 0 {
                    return Err(Error::Invalid("a row with these values is there already; edit it instead".into()));
                }
                let key: std::collections::BTreeMap<_, _> = key.into_iter().collect();
                let values = values.into_iter().filter(|(n, _)| !key.contains_key(n)).collect();
                Edit::Add { id, key, values }
            }
            Asked::Undo { edit } => {
                let known = log.iter().flat_map(|e| e.edits.iter()).any(|e| e.id() == edit && !matches!(e, Edit::Undo { .. }));
                if !known {
                    return Err(Error::Invalid(format!("there is no edit {edit} to undo")));
                }
                if !in_force.iter().any(|(_, e)| e.id() == edit) {
                    return Err(Error::Invalid(format!("edit {edit} is undone already")));
                }
                Edit::Undo { id, undoes: edit }
            }
        })
    }

    /// The edit log, and how each edit fared in the live version.
    pub fn edit_log(&self, scope: &Scope, id: &str) -> Result<(Vec<Entry>, Vec<Outcome>)> {
        let (dir, dataset) = self.find(scope, id)?;
        let log: Vec<Entry> = edits::read_log(&dir)?.into_iter().filter(|e| e.seq <= dataset.edits).collect();
        Ok((log, dataset.live.map(|k| edits::read_outcomes(&dir, k)).unwrap_or_default()))
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

    /// Takes snapshot `n` of an upload into `dir`; its columns.
    fn take(&self, dir: &Path, source: &str, n: u32, upload: &Upload<'_>, established: &[Column]) -> Result<Vec<Column>> {
        let session = self.engine.session()?;
        let taken = snapshot::from_csv(&session, upload.bytes, &upload.options, established, self.settings.max_rows)?;
        let digest = sha2::Sha256::digest(upload.bytes);
        let sha = digest.iter().map(|b| format!("{b:02x}")).collect();
        self.keep_snapshot(dir, source, n, source::Kind::Upload, upload.file_name.clone(), Some(sha), None, taken)
    }

    /// Takes snapshot `n` of a semantic source into `dir`; its columns.
    fn take_rows(&self, dir: &Path, source: &str, n: u32, arrived: &Arrived, established: &[Column]) -> Result<Vec<Column>> {
        let session = self.engine.session()?;
        let taken = snapshot::from_rows(&session, &arrived.rows, &arrived.roles, established, self.settings.max_rows)?;
        self.keep_snapshot(dir, source, n, source::Kind::Semantic, None, None, Some(arrived.read.clone()), taken)
    }

    #[allow(clippy::too_many_arguments)]
    fn keep_snapshot(
        &self,
        dir: &Path,
        source: &str,
        n: u32,
        kind: source::Kind,
        file_name: Option<String>,
        sha256: Option<String>,
        semantic: Option<source::SemanticRead>,
        taken: snapshot::Taken,
    ) -> Result<Vec<Column>> {
        let (rows_path, manifest_path) = store::snapshot_paths(dir, source, n);
        let bytes = store::write_rows(&rows_path, &taken.rows)?;
        if bytes > self.settings.max_bytes {
            let _ = std::fs::remove_file(&rows_path);
            return Err(Error::Unprocessable(format!(
                "the snapshot is {bytes} bytes, more than the {} a dataset holds",
                self.settings.max_bytes
            )));
        }
        // What arrived, by its hash: the file as uploaded, or the rows as kept.
        let sha256 = match sha256 {
            Some(s) => s,
            None => {
                let kept = std::fs::read(&rows_path).map_err(|e| Error::io(&rows_path, e))?;
                sha2::Sha256::digest(&kept).iter().map(|b| format!("{b:02x}")).collect()
            }
        };
        let snapshot = Snapshot {
            source: source.to_string(),
            number: n,
            kind,
            taken: time::now(),
            file_name,
            sha256,
            bytes,
            parse: taken.parse,
            semantic,
            columns: taken.columns.clone(),
            rows: taken.rows.num_rows() as u64,
            changes: taken.changes,
        };
        store::write_json(&manifest_path, &snapshot)?;
        Ok(taken.columns)
    }

    /// Runs the recipe as the next version and makes it live; or, when
    /// the recipe reads columns that are not there, makes nothing and
    /// says which.
    fn materialize(&self, dir: &Path, dataset: &mut Dataset, recipe: &Recipe, log: &[Entry]) -> Result<Option<Vec<Unresolved>>> {
        let k = dataset.made + 1;
        let mut made = match materialize::run(self, dir, dataset, recipe, log, dataset.edits, k)? {
            materialize::Run::Made(m) => m,
            materialize::Run::Held(unresolved) => return Ok(Some(unresolved)),
        };
        materialize::keep(self, dir, &mut made)?;
        dataset.made = k;
        dataset.live = Some(k);
        dataset.columns = made.manifest.columns;
        dataset.rows = made.manifest.rows;
        dataset.notes = snapshot_notes(dir, dataset);
        dataset.notes.extend(made.lineage.notes.iter().cloned());
        Ok(None)
    }
}

/// What the latest snapshots said of their columns: new, gone, widened.
fn snapshot_notes(dir: &Path, dataset: &Dataset) -> Vec<String> {
    dataset
        .sources
        .iter()
        .filter_map(|s| store::read_json::<Snapshot>(&store::snapshot_paths(dir, &s.id, s.snapshot).1).ok())
        .flat_map(|snap| snap.changes)
        .collect()
}

/// A dataset's id: `ds_` and sixteen hex digits, unguessable enough
/// that one is never made twice, and checked before it names a
/// directory.
pub fn new_id(salt: &str) -> String {
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
