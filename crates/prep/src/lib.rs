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
//! A dataset belongs to an org and a project, and is addressed by its
//! id, as a deployed branch or a result is. Who may read or change
//! which dataset is the application's to decide: the server keeps no
//! user, as it keeps none for anything else.
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
//! - `blend`: datasets beside the model's rows, joined as a request says
//! - `edits`: the edit log: remaps, overrides, rows added and deleted
//! - `engine`: DuckDB, in memory, with external access off
//! - `materialize`: running a recipe, and pages of what it made
//! - `lineage`: what each materialization was made from
//! - `store`: where datasets are kept, and the only module that knows
//! - `backup`: each dataset's copy in the object store, and its restore

pub mod backup;
pub mod blend;
pub mod compile;
pub mod edits;
pub mod engine;
pub mod error;
pub mod formula;
pub mod lineage;
mod materialize;
pub mod recipe;
pub mod recovery;
pub mod schema;
mod snapshot;
pub mod source;
pub mod sql;
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

/// How many blends a project keeps; past it, the least recently made go.
const BLENDS_KEPT: usize = 64;

/// Keeps the `keep` newest entries of a directory of kept answers.
fn evict(dir: &Path, keep: usize) {
    let Ok(entries) = std::fs::read_dir(dir) else { return };
    let mut all: Vec<(std::time::SystemTime, PathBuf)> = entries
        .flatten()
        .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
        .map(|e| (e.metadata().and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH), e.path()))
        .collect();
    if all.len() <= keep {
        return;
    }
    all.sort();
    for (_, path) in &all[..all.len() - keep] {
        let _ = std::fs::remove_dir_all(path);
    }
}

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
            engine: EngineLimits { memory: "1GB".into(), threads: 2, spill: data_dir.join(".cache").join("prep"), job: engine::JOB_LIMIT },
        }
    }
}

/// Where a request's datasets are: an org and a project.
#[derive(Clone, Debug)]
pub struct Scope {
    pub org: String,
    pub project: String,
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

/// A blend's answer: its rows, and the version of each dataset it read.
pub struct Blended {
    pub rows: Batches,
    pub datasets: Vec<lineage::DatasetRead>,
    /// Whether it was a kept answer to the same question.
    pub hit: bool,
}

/// What a kept blend answered, beside its rows.
#[derive(serde::Serialize, serde::Deserialize)]
struct BlendKept {
    org: String,
    project: String,
    datasets: Vec<lineage::DatasetRead>,
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
    /// The object store each dataset is copied to.
    backup: Option<std::sync::Arc<zsql_exec::Objects>>,
    /// Set when the server stops: no job starts.
    stopping: std::sync::atomic::AtomicBool,
    /// The datasets whose jobs a stop suspended: their markers stay.
    suspended: Mutex<HashSet<PathBuf>>,
    /// What the start found, until the server takes it.
    recovered: Mutex<recovery::Recovered>,
}

/// A query a dataset waits on, run after its request was answered.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Pending {
    /// The run, by its id under `/results`.
    pub result: String,
    pub since: String,
    /// Set when it failed: the dataset was not made or not refreshed.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub failed: Option<String>,
    /// Set when a start found it in flight and did not run it again.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<recovery::Interrupted>,
}

/// A dataset held for one job; let go when dropped. Its marker goes
/// with it, unless a stop suspended it.
struct Job<'a> {
    prep: &'a Prep,
    dir: PathBuf,
}

impl Drop for Job<'_> {
    fn drop(&mut self) {
        if !lock(&self.prep.suspended).contains(&self.dir) {
            let _ = std::fs::remove_file(self.dir.join(recovery::MARKER));
        }
        lock(&self.prep.busy).remove(&self.dir);
    }
}

fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

impl Prep {
    pub fn open(data_dir: &Path, settings: Settings) -> Result<Prep> {
        // An answer handed back may be twice what a dataset holds: a step
        // between may make more than the recipe keeps.
        let bound = engine::Bound { rows: settings.max_rows.saturating_mul(2).max(1), bytes: settings.max_bytes.saturating_mul(2).max(1) };
        let engine = Engine::new(settings.engine.clone(), bound);
        let duckdb_version = engine.version()?;
        let prep = Prep {
            store: Store::new(data_dir),
            engine,
            settings,
            duckdb_version,
            busy: Mutex::new(HashSet::new()),
            backup: None,
            pending: Mutex::new(std::collections::HashMap::new()),
            stopping: std::sync::atomic::AtomicBool::new(false),
            suspended: Mutex::new(HashSet::new()),
            recovered: Mutex::new(recovery::Recovered::default()),
        };
        let recovered = prep.recover();
        if recovered.swept > 0 || !recovered.interrupted.is_empty() {
            tracing::info!(
                "data prep: {} half-made thing(s) swept; {} job(s) interrupted: {}",
                recovered.swept,
                recovered.interrupted.len(),
                recovered.interrupted.join(", ")
            );
        }
        *lock(&prep.recovered) = recovered;
        Ok(prep)
    }

    /// What the start found, handed over once.
    pub fn take_recovered(&self) -> recovery::Recovered {
        std::mem::take(&mut *lock(&self.recovered))
    }

    /// Sweeps what jobs left half made, and turns each marker left
    /// behind into an interruption: cause `crash` for a job that was
    /// running, `stop` for one a stop suspended. Nothing is run again.
    fn recover(&self) -> recovery::Recovered {
        use recovery::{Cause, Interrupted, Marker, QueryJob, State};
        let mut out = recovery::Recovered { swept: recovery::sweep(&self.store.data_dir()), ..Default::default() };
        for (org, project, root) in recovery::roots(&self.store.data_dir()) {
            for (dir, d) in self.store.all(&org, &project) {
                let path = dir.join(recovery::MARKER);
                let Ok(m) = store::read_json::<Marker>(&path) else { continue };
                let cause = match m.state {
                    State::Running => Cause::Crash,
                    State::Suspended => Cause::Stop,
                };
                let i = Interrupted::new(m.job, &m.started, cause, None);
                if let Err(e) = store::write_json(&dir.join(recovery::INTERRUPTED), &i) {
                    tracing::warn!("dataset {} was interrupted and that could not be written: {e}", d.id);
                }
                out.interrupted.push(format!("{org}/{project}/{}", d.id));
                let _ = std::fs::remove_file(&path);
            }
            let jobs = root.join(recovery::QUERIES);
            let Ok(entries) = std::fs::read_dir(&jobs) else { continue };
            for e in entries.flatten() {
                let path = e.path();
                let Ok(mut q) = store::read_json::<QueryJob>(&path) else { continue };
                let dir = root.join(&q.id);
                if q.interrupted.is_some() {
                    // The record of a dataset never made, until it is old.
                    if recovery::expired(&path) {
                        let _ = std::fs::remove_file(&path);
                    } else {
                        self.remember(&dir, &q);
                    }
                    continue;
                }
                let kind = recovery::query_kind(&q);
                let cause = match q.state {
                    State::Running => Cause::Crash,
                    State::Suspended => Cause::Stop,
                };
                let i = Interrupted::new(kind, &q.since, cause, None);
                out.interrupted.push(format!("{org}/{project}/{}", q.id));
                if dir.join(store::MANIFEST).exists() {
                    let _ = store::write_json(&dir.join(recovery::INTERRUPTED), &i);
                    let _ = std::fs::remove_file(&path);
                } else {
                    q.interrupted = Some(i);
                    let _ = store::write_json(&path, &q);
                    self.remember(&dir, &q);
                }
            }
        }
        out
    }

    /// An interrupted query of a dataset never made, as a failed pending
    /// one: what its id answers with.
    fn remember(&self, dir: &Path, q: &recovery::QueryJob) {
        let reason = q.interrupted.as_ref().map(|i| i.reason.clone());
        let p = Pending { result: q.result.clone(), since: q.since.clone(), failed: reason, interrupted: q.interrupted.clone() };
        lock(&self.pending).insert(dir.to_path_buf(), p);
    }

    /// Stops data prep for a server that is stopping. No job starts from
    /// now; the jobs running are given `grace` to finish, and the rest
    /// are stopped and kept as suspended, which the next start marks
    /// interrupted. A query waited on is suspended the same way.
    pub fn stop(&self, grace: std::time::Duration) {
        use recovery::{Marker, QueryJob, State};
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let until = std::time::Instant::now() + grace;
        while !lock(&self.busy).is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        let running: Vec<PathBuf> = lock(&self.busy).iter().cloned().collect();
        for dir in &running {
            let path = dir.join(recovery::MARKER);
            if let Ok(mut m) = store::read_json::<Marker>(&path) {
                m.state = State::Suspended;
                lock(&self.suspended).insert(dir.clone());
                let _ = store::write_json(&path, &m);
            }
        }
        self.engine.stop();
        for (_, _, root) in recovery::roots(&self.store.data_dir()) {
            let jobs = root.join(recovery::QUERIES);
            let Ok(entries) = std::fs::read_dir(&jobs) else { continue };
            for e in entries.flatten() {
                let Ok(mut q) = store::read_json::<QueryJob>(&e.path()) else { continue };
                if q.interrupted.is_none() && q.state == State::Running {
                    q.state = State::Suspended;
                    let _ = store::write_json(&e.path(), &q);
                }
            }
        }
        // What was interrupted unwinds; a moment for it to.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !lock(&self.busy).is_empty() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        tracing::info!("data prep stopped: {} job(s) interrupted", running.len());
    }

    /// The same, copying each dataset to the object store as it changes.
    pub fn with_backup(mut self, objects: Option<std::sync::Arc<zsql_exec::Objects>>) -> Prep {
        self.backup = objects;
        self
    }

    /// Copies a dataset to the object store, when there is one, and says
    /// in its manifest how that went. A copy that fails fails nothing.
    fn backed_up(&self, dir: &Path, dataset: &mut Dataset) {
        let Some(objects) = &self.backup else { return };
        let failed = backup::copy(objects, dir).err().map(|e| e.to_string());
        if let Some(why) = &failed {
            tracing::warn!("dataset {} is not backed up: {why}", dataset.id);
        }
        dataset.backup = Some(backup::Backup { at: time::now(), failed });
        if let Err(e) = store::write_json(&dir.join(store::MANIFEST), dataset) {
            tracing::warn!("dataset {}: {e}", dataset.id);
        }
    }

    /// Removes a dataset's copy in the object store, when there is one.
    fn forget_backup(&self, org: &str, project: &str, id: &str) {
        if let Some(objects) = &self.backup {
            if let Err(e) = backup::forget(objects, org, project, id) {
                tracing::warn!("dataset {id} was removed and its copy in the object store was not: {e}");
            }
        }
    }

    /// Brings back every dataset of an org the object store has whole and
    /// this node does not: after a disk is lost, or on a new node. The
    /// ones brought back, as `project/id`.
    pub fn restore(&self, org: &str) -> Result<Vec<String>> {
        let objects =
            self.backup.as_ref().ok_or_else(|| Error::Invalid("there is no object store to restore from (ZSQL_OBJECT_STORE)".into()))?;
        let restored = backup::restore(objects, &self.store.data_dir(), org)?;
        tracing::info!("{} dataset(s) of {org} restored from the object store", restored.len());
        Ok(restored)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Takes a dataset for one job, and leaves its marker. A job the
    /// user asked for ends an interruption: its own outcome is what the
    /// dataset says next.
    fn job(&self, dir: &Path, kind: recovery::JobKind) -> Result<Job<'_>> {
        if self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            return Err(Error::Stopping("the server is stopping; try again in a moment".into()));
        }
        if !lock(&self.busy).insert(dir.to_path_buf()) {
            return Err(Error::Conflict("this dataset is busy with another job; try again when it is done".into()));
        }
        let job = Job { prep: self, dir: dir.to_path_buf() };
        let marker = recovery::Marker { job: kind, started: time::now(), state: recovery::State::Running };
        store::write_json(&dir.join(recovery::MARKER), &marker)?;
        if kind != recovery::JobKind::Update {
            let _ = std::fs::remove_file(dir.join(recovery::INTERRUPTED));
        }
        Ok(job)
    }

    /// Says in a job's marker what it does now.
    fn mark(&self, dir: &Path, kind: recovery::JobKind) {
        let path = dir.join(recovery::MARKER);
        if let Ok(mut m) = store::read_json::<recovery::Marker>(&path) {
            m.job = kind;
            let _ = store::write_json(&path, &m);
        }
    }

    /// The directory of a dataset of the project, or not found.
    fn find(&self, scope: &Scope, id: &str) -> Result<(PathBuf, Dataset)> {
        let missing = || Error::NotFound(format!("no dataset {id} in project {}", scope.project));
        if !is_id(id) {
            return Err(missing());
        }
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        let mut dataset: Dataset = match store::read_json(&dir.join(store::MANIFEST)) {
            Ok(d) => d,
            Err(_) if !dir.join(store::MANIFEST).exists() => return Err(missing()),
            Err(e) => return Err(e),
        };
        dataset.interrupted = store::read_json(&dir.join(recovery::INTERRUPTED)).ok();
        Ok((dir, dataset))
    }

    /// Says a query runs on for a dataset, after its request was
    /// answered; `failed` says it ended without a snapshot; `done` that
    /// it made one.
    ///
    /// The query is kept on disk while it runs, never with its context,
    /// so a start can say it was interrupted.
    pub fn waiting(&self, scope: &Scope, job: recovery::QueryJob) {
        let root = self.store.root(&scope.org, &scope.project);
        let dir = root.join(&job.id);
        let path = root.join(recovery::QUERIES).join(format!("{}.json", job.id));
        if let Err(e) = store::write_json(&path, &job) {
            tracing::warn!("the query dataset {} waits on is not kept, and a restart forgets it: {e}", job.id);
        }
        let p = Pending { result: job.result, since: job.since, failed: None, interrupted: None };
        lock(&self.pending).insert(dir, p);
    }

    fn forget_query(&self, scope: &Scope, id: &str) {
        let path = self.store.root(&scope.org, &scope.project).join(recovery::QUERIES).join(format!("{id}.json"));
        let _ = std::fs::remove_file(path);
    }

    pub fn failed(&self, scope: &Scope, id: &str, why: &str) {
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        if let Some(p) = lock(&self.pending).get_mut(&dir) {
            p.failed = Some(why.to_string());
        }
        // A query that ends while the server stops was refused its
        // snapshot by the stop: it stays, for the stop to suspend.
        if !self.stopping.load(std::sync::atomic::Ordering::SeqCst) {
            self.forget_query(scope, id);
        }
    }

    pub fn done(&self, scope: &Scope, id: &str) {
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        lock(&self.pending).remove(&dir);
        self.forget_query(scope, id);
    }

    /// The query a dataset waits on, if one does.
    pub fn pending(&self, scope: &Scope, id: &str) -> Option<Pending> {
        let dir = self.store.root(&scope.org, &scope.project).join(id);
        self.pending.lock().unwrap_or_else(std::sync::PoisonError::into_inner).get(&dir).cloned()
    }

    /// The project's datasets.
    pub fn list(&self, scope: &Scope) -> Vec<Dataset> {
        self.store.all(&scope.org, &scope.project).into_iter().map(|(_, d)| d).collect()
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
                backup: None,
                interrupted: None,
            };
            let recipe = Recipe::first(now);
            store::write_json(&store::recipe_path(&staging, 1), &recipe)?;
            if self.materialize(&staging, &mut dataset, &recipe, &[])?.is_some() {
                return Err(Error::Internal("a first recipe has no steps to hold it".into()));
            }
            store::write_json(&staging.join(store::MANIFEST), &dataset)?;
            let target = root.join(&id);
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))?;
            store::sync_dir(Some(&root))?;
            self.backed_up(&target, &mut dataset);
            Ok(dataset)
        })();
        if result.is_err() {
            let _ = std::fs::remove_dir_all(&staging);
        }
        let dataset = result?;
        tracing::info!("dataset {} made in {}/{}: {} rows", dataset.id, scope.org, scope.project, dataset.rows);
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
        let what = if kind == source::Kind::Semantic { recovery::JobKind::Refresh } else { recovery::JobKind::Replace };
        let _job = self.job(&dir, what)?;
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
        let log = edits::read_log(&dir, dataset.edits)?;
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
        if made?.is_none() {
            self.after(&dir, &mut dataset)?;
        } else {
            self.backed_up(&dir, &mut dataset);
        }
        Ok(dataset)
    }

    /// A new version of the recipe, made against the latest, and run.
    /// Nothing is kept unless the run succeeds; a recipe that reads a
    /// column not there at its step is refused, naming each.
    pub fn change(&self, scope: &Scope, id: &str, change: Change) -> Result<Dataset> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir, recovery::JobKind::Change)?;
        let (_, dataset) = self.find(scope, id)?;
        let current: Recipe = store::read_json(&store::recipe_path(&dir, dataset.recipe))?;
        let recipe = current.changed(change, time::now())?;
        self.check_cycle(&dir, id, &recipe.inputs())?;
        self.commit(&dir, dataset, recipe, None, false)
    }

    /// Takes a held dataset past what held it: a recipe version without
    /// the steps and roles that read the columns that are gone, run
    /// over the latest snapshots.
    pub fn confirm(&self, scope: &Scope, id: &str, against: u32) -> Result<Dataset> {
        let (dir, _) = self.find(scope, id)?;
        let _job = self.job(&dir, recovery::JobKind::Confirm)?;
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
        // What its readers named of it and it no longer has, they lose:
        // they are held when the cascade reaches them.
        self.commit(&dir, dataset, recipe, entry, true)
    }

    /// Runs a new recipe version and, when it makes a materialization,
    /// keeps both.
    fn commit(&self, dir: &Path, dataset: Dataset, recipe: Recipe, entry: Option<Entry>, readers_go_stale: bool) -> Result<Dataset> {
        let mut next = dataset;
        let mut log = edits::read_log(dir, next.edits)?;
        if let Some(e) = &entry {
            log.push(e.clone());
            next.edits = e.seq;
        }
        if let Some(unresolved) = self.materialize_as(dir, &mut next, &recipe, &log, readers_go_stale)? {
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
        self.after(dir, &mut next)?;
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
        let log = edits::read_log(&dir, dataset.edits)?;
        let input_ids: Vec<String> = steps.iter().filter_map(Step::dataset).map(str::to_string).collect();
        let inputs = self.inputs(&dir, &input_ids)?;
        let compiled = compile::compile_all(
            &steps,
            &roles,
            &recipe.key,
            &edits::in_force(&log, dataset.edits, &steps_of(&dir), &steps),
            (&dataset.sources, &materialize::inputs_map(&inputs)),
            &pinned_at,
            &|c, q| sql::describe(&self.engine, c, q),
        )?;
        let session = self.engine.session()?;
        materialize::load_sources(&session, &dir, &dataset, Some(PREVIEW_SAMPLE))?;
        materialize::load_inputs(&session, &inputs, Some(PREVIEW_SAMPLE))?;
        materialize::prepare(&self.engine, &session, &compiled)?;
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
        let describe: compile::Describe<'_> = &|c, q| sql::describe(&self.engine, c, q);
        // Where the column is first there: a source, or the step that makes it.
        let mut made_at = None;
        let all_inputs = materialize::inputs_map(&self.inputs(&dir, &recipe.inputs())?);
        for k in 0..=recipe.steps.len() {
            let c = compile::compile_all(&recipe.steps[..k], &empty, &[], &[], (&dataset.sources, &all_inputs), &pinned_at, describe)?;
            if c.columns.iter().any(|x| x.name.eq_ignore_ascii_case(column)) {
                made_at = Some(k);
                break;
            }
        }
        let Some(k) = made_at else {
            return Err(Error::NotFound(format!("dataset {id} has no column '{column}' at any step")));
        };
        let mut steps = recipe.steps.clone();
        steps.insert(k, Step::RemoveColumns { columns: vec![column.to_string()] });
        let log = edits::read_log(&dir, dataset.edits)?;
        let in_force: Vec<(usize, Edit)> = edits::in_force(&log, dataset.edits, &steps_of(&dir), &recipe.steps)
            .into_iter()
            .map(|(a, e)| (if a >= k { a + 1 } else { a }, e))
            .collect();
        let inputs = materialize::inputs_map(&self.inputs(&dir, &recipe.inputs())?);
        let c = compile::compile_all(&steps, &recipe.roles, &recipe.key, &in_force, (&dataset.sources, &inputs), &pinned_at, describe)?;
        // And the datasets that read it, by what they name of it.
        let mut c = c;
        if k == recipe.steps.len() {
            for (reader, needs) in self.readers(&dir, id) {
                if needs.iter().any(|n| n.eq_ignore_ascii_case(column)) {
                    c.unresolved.push(Unresolved {
                        column: column.to_string(),
                        step: 0,
                        op: "reader".into(),
                        edit: None,
                        dataset: Some(reader),
                    });
                }
            }
        }
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
    pub fn delete(&self, scope: &Scope, id: &str, confirmed: bool) -> Result<()> {
        let (dir, _) = self.find(scope, id)?;
        let readers: Vec<String> = self.readers(&dir, id).into_iter().map(|(r, _)| r).collect();
        if !readers.is_empty() && !confirmed {
            return Err(Error::Conflict(format!(
                "datasets {} read dataset {id}; delete it with confirm=true and they keep their last version, and fail when run again",
                readers.join(", ")
            )));
        }
        let _job = self.job(&dir, recovery::JobKind::Delete)?;
        store::remove(&dir)?;
        self.forget_blends(&scope.org, &scope.project, id);
        self.forget_backup(&scope.org, &scope.project, id);
        tracing::info!("dataset {id} removed from {}/{}", scope.org, scope.project);
        Ok(())
    }

    /// Removes every dataset of a project: part of
    /// removing the project. The ids removed.
    pub fn delete_project(&self, org: &str, project: &str) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for (dir, d) in self.store.all(org, project) {
            store::remove(&dir)?;
            self.forget_blends(org, project, &d.id);
            self.forget_backup(org, project, &d.id);
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
        let _job = self.job(&dir, recovery::JobKind::Edit)?;
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
        let log = edits::read_log(&dir, dataset.edits)?;
        let in_force = edits::in_force(&log, dataset.edits, &steps_of(&dir), &recipe.steps);
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
        self.after(&dir, &mut next)?;
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
            Asked::Cell { column, row, from, to, matching: matched, all } => {
                let c = column_of(&column)?.clone();
                compile::value_sql(&to, c.ty, &c.name)?;
                let dimension = c.role == Role::Dimension;
                let others: Option<Vec<String>> = match matched {
                    // A dimension's default is the column; a measure's, its row.
                    None if dimension => None,
                    None => Some(identity.to_vec()),
                    Some(m) if m.iter().all(|x| x.eq_ignore_ascii_case(&c.name)) && dimension => None,
                    Some(m) => Some(m),
                };
                if let Some(names) = &others {
                    for n in names {
                        column_of(n)?;
                    }
                }
                if others.is_none() {
                    let from = from
                        .or_else(|| {
                            row.as_ref().and_then(|r| r.iter().find(|(n, _)| n.eq_ignore_ascii_case(&c.name)).map(|(_, v)| v.clone()))
                        })
                        .ok_or_else(|| Error::Invalid(format!("a remap of '{}' says the value it changes: from, or the row", c.name)))?;
                    compile::value_sql(&from, c.ty, &c.name)?;
                    Edit::Remap { id, column: c.name, from, to }
                } else {
                    let row = row.ok_or_else(|| Error::Invalid(format!("an edit of '{}' for some rows says which: row", c.name)))?;
                    // The columns that name its rows; a dimension's own value is one of them.
                    let mut names = others.unwrap_or_default();
                    if dimension && !names.iter().any(|n| n.eq_ignore_ascii_case(&c.name)) {
                        names.push(c.name.clone());
                    }
                    if names.is_empty() {
                        return Err(Error::Invalid(format!(
                            "an edit of measure '{}' names its rows: match the columns that pick them out",
                            c.name
                        )));
                    }
                    let key = edits::key_of(&row, &names)?;
                    let (n, current) = matching(&key, Some(&c))?;
                    // A dimension edit with conditions is for every row they
                    // name, however many, now and after every sync.
                    let all = all || dimension;
                    names_one(n, all)?;
                    // The value first replaced is what a sync is checked against.
                    let earlier = in_force.iter().rev().find_map(|(_, e)| match e {
                        Edit::Set { column: col, key: k, was, .. } if *col == c.name && *k == key => Some(was.clone()),
                        _ => None,
                    });
                    let was = earlier.unwrap_or(if n == 1 { current } else { serde_json::Value::Null });
                    Edit::Set { id, column: c.name, key, was, to, all }
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
        let log: Vec<Entry> = edits::read_log(&dir, dataset.edits)?;
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
        self.materialize_as(dir, dataset, recipe, log, false)
    }

    /// [`Self::materialize`]; with `readers_go_stale`, a version that
    /// lacks a column another dataset reads is made anyway, and holds
    /// that reader when the cascade reaches it.
    fn materialize_as(
        &self,
        dir: &Path,
        dataset: &mut Dataset,
        recipe: &Recipe,
        log: &[Entry],
        readers_go_stale: bool,
    ) -> Result<Option<Vec<Unresolved>>> {
        let k = dataset.made + 1;
        let inputs = self.inputs(dir, &recipe.inputs())?;
        let mut made = match materialize::run(self, dir, dataset, recipe, (log, dataset.edits), &inputs, k)? {
            materialize::Run::Made(m) => m,
            materialize::Run::Held(unresolved) => return Ok(Some(unresolved)),
        };
        if !readers_go_stale {
            // A column another dataset names, gone from this version,
            // holds this one: its readers are named, and nothing is made.
            let mut lost = Vec::new();
            for (reader, needs) in self.readers(dir, &dataset.id) {
                for n in needs {
                    if !made.manifest.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&n)) {
                        lost.push(Unresolved { column: n, step: 0, op: "reader".into(), edit: None, dataset: Some(reader.clone()) });
                    }
                }
            }
            if !lost.is_empty() {
                return Ok(Some(lost));
            }
        }
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

impl Prep {
    /// The live versions of the datasets `ids` names in the same project
    /// as `dir`. One that is not there, or has no version, is left out,
    /// and the step that reads it is refused.
    fn inputs(&self, dir: &Path, ids: &[String]) -> Result<Vec<materialize::Input>> {
        let root = dir.parent().unwrap_or(dir);
        let mut out = Vec::new();
        for id in ids {
            if !is_id(id) || out.iter().any(|i: &materialize::Input| &i.id == id) {
                continue;
            }
            let d_dir = root.join(id);
            let Ok(d) = store::read_json::<Dataset>(&d_dir.join(store::MANIFEST)) else { continue };
            let Some(version) = d.live else { continue };
            let m: Materialization = store::read_json(&store::version_dir(&d_dir, version).join("manifest.json"))?;
            out.push(materialize::Input { id: id.clone(), version, dir: d_dir, columns: m.columns });
        }
        Ok(out)
    }

    /// The datasets beside `dir`, with their latest recipes.
    fn siblings(&self, dir: &Path) -> Vec<(PathBuf, Dataset, Recipe)> {
        let root = dir.parent().unwrap_or(dir);
        let Ok(entries) = std::fs::read_dir(root) else { return Vec::new() };
        let mut out: Vec<(PathBuf, Dataset, Recipe)> = entries
            .flatten()
            .filter(|e| !e.file_name().to_string_lossy().starts_with('.'))
            .filter_map(|e| {
                let d: Dataset = store::read_json(&e.path().join(store::MANIFEST)).ok()?;
                let r: Recipe = store::read_json(&store::recipe_path(&e.path(), d.recipe)).ok()?;
                Some((e.path(), d, r))
            })
            .collect();
        out.sort_by(|a, b| a.1.id.cmp(&b.1.id));
        out
    }

    /// The datasets whose recipes read `id`, each with the columns of it
    /// they name.
    fn readers(&self, dir: &Path, id: &str) -> Vec<(String, Vec<String>)> {
        self.siblings(dir)
            .into_iter()
            .filter_map(|(_, d, r)| {
                let reads: Vec<&Step> = r.steps.iter().filter(|s| s.dataset() == Some(id)).collect();
                (!reads.is_empty()).then(|| (d.id, reads.iter().flat_map(|s| s.needs()).collect()))
            })
            .collect()
    }

    /// Every dataset downstream of `id`, in an order where each comes
    /// after everything it reads.
    fn downstream_of(&self, dir: &Path, id: &str) -> Vec<(PathBuf, Dataset, Recipe)> {
        let all = self.siblings(dir);
        let mut below: Vec<String> = vec![id.to_string()];
        let mut grew = true;
        while grew {
            grew = false;
            for (_, d, r) in &all {
                if !below.contains(&d.id) && r.inputs().iter().any(|i| below.contains(i)) {
                    below.push(d.id.clone());
                    grew = true;
                }
            }
        }
        let mut order: Vec<(PathBuf, Dataset, Recipe)> = Vec::new();
        let mut left: Vec<&(PathBuf, Dataset, Recipe)> = all.iter().filter(|x| x.1.id != id && below.contains(&x.1.id)).collect();
        while !left.is_empty() {
            let ready = left.iter().position(|(_, _, r)| r.inputs().iter().all(|i| i == id || !left.iter().any(|x| &x.1.id == i)));
            // No cycle can be made, so one is always ready; a hand-edited
            // directory that has one ends the walk rather than spinning.
            let Some(i) = ready else { break };
            order.push(left.remove(i).clone());
        }
        order
    }

    /// Refused when making `id` read `inputs` would close a loop.
    fn check_cycle(&self, dir: &Path, id: &str, inputs: &[String]) -> Result<()> {
        if inputs.iter().any(|i| i == id) {
            return Err(Error::Invalid(format!("dataset {id} cannot read itself")));
        }
        let below: Vec<String> = self.downstream_of(dir, id).into_iter().map(|(_, d, _)| d.id).collect();
        if let Some(i) = inputs.iter().find(|i| below.contains(i)) {
            return Err(Error::Invalid(format!("dataset {i} reads dataset {id}, so {id} reading {i} would make a cycle")));
        }
        Ok(())
    }

    /// Runs every dataset downstream of `id` again, in order, each over
    /// the versions this job made. One that fails or is held stops its
    /// own descendants, which keep their last version. What happened to
    /// each, as notes.
    fn cascade(&self, dir: &Path, id: &str) -> Vec<String> {
        let mut notes = Vec::new();
        let mut stopped: Vec<String> = Vec::new();
        for (d_dir, _, _) in self.downstream_of(dir, id) {
            if let Some(note) = self.update(&d_dir, &mut stopped) {
                notes.push(note);
            }
        }
        notes
    }

    /// Runs one dataset again over what it reads now, as a job of its
    /// own; what happened, as a note. `stopped` is the datasets that did
    /// not run, so what reads them does not either. One interrupted is
    /// not run: its job may be what stopped the server, and the user's
    /// next job on it says.
    fn update(&self, d_dir: &Path, stopped: &mut Vec<String>) -> Option<String> {
        // Its job first, then what it is: read before, it could be a
        // version another job has since moved past.
        let name = d_dir.file_name().map(|n| n.to_string_lossy().into_owned()).unwrap_or_default();
        let Ok(_job) = self.job(d_dir, recovery::JobKind::Update) else {
            stopped.push(name.clone());
            return Some(format!("dataset {name} was not run again: it was busy"));
        };
        if d_dir.join(recovery::INTERRUPTED).exists() {
            stopped.push(name.clone());
            return Some(format!("dataset {name} was not run again: it is interrupted; run it once yourself"));
        }
        let mut d = store::read_json::<Dataset>(&d_dir.join(store::MANIFEST)).ok()?;
        let recipe = store::read_json::<Recipe>(&store::recipe_path(d_dir, d.recipe)).ok()?;
        if let Some(i) = recipe.inputs().iter().find(|i| stopped.contains(i)) {
            let note = format!("dataset {} was not run again: {i}, which it reads, was not", d.id);
            stopped.push(d.id);
            return Some(note);
        }
        let log = edits::read_log(d_dir, d.edits).unwrap_or_default();
        let note = match self.materialize(d_dir, &mut d, &recipe, &log) {
            Ok(None) => {
                d.held = None;
                format!("dataset {} is at version {} now", d.id, d.live.unwrap_or(0))
            }
            Ok(Some(unresolved)) => {
                let said: Vec<String> = unresolved.iter().map(Unresolved::describe).collect();
                d.notes.push(format!("held by a change upstream: {}", said.join("; ")));
                d.held = Some(Held { since: time::now(), unresolved });
                stopped.push(d.id.clone());
                format!("dataset {} is held: {}", d.id, said.join("; "))
            }
            Err(e) => {
                d.notes = vec![format!("a change upstream failed here: {e}")];
                stopped.push(d.id.clone());
                format!("dataset {} failed and keeps version {}: {e}", d.id, d.live.unwrap_or(0))
            }
        };
        d.updated = time::now();
        if let Err(e) = store::write_json(&d_dir.join(store::MANIFEST), &d) {
            return Some(format!("dataset {} could not be written: {e}", d.id));
        }
        self.backed_up(d_dir, &mut d);
        Some(note)
    }

    /// Runs what is downstream of a dataset just made, and keeps what
    /// happened among its notes.
    fn after(&self, dir: &Path, dataset: &mut Dataset) -> Result<()> {
        // Its own change is made: what is left of the job is its readers'.
        self.mark(dir, recovery::JobKind::Cascade);
        let notes = self.cascade(dir, &dataset.id.clone());
        if !notes.is_empty() {
            dataset.notes.extend(notes);
            store::write_json(&dir.join(store::MANIFEST), dataset)?;
        }
        self.backed_up(dir, dataset);
        Ok(())
    }

    /// A blend: the model's part, run, and the attached datasets joined
    /// to it as the request says. Each dataset is found in the project
    /// before anything is looked up, and the answer is kept by the
    /// model's run, each dataset's id and version, and the joins, so a
    /// match is the same question over the same data. `model_run` names
    /// the run of the model's part: the run's own cache decides when it
    /// is run again (`fresh`, the datasource's TTL, an invalidation, the
    /// credential), and a new run is a new question here.
    pub fn blend(&self, scope: &Scope, plan: &blend::Blend, model: &blend::ModelPart, model_run: &str) -> Result<Blended> {
        let mut found = Vec::new();
        for a in &plan.datasets {
            blend::check_alias(&a.alias)?;
            if found.iter().any(|f: &blend::Found| f.alias == a.alias) {
                return Err(Error::Invalid(format!("'{}' is the alias of two datasets", a.alias)));
            }
            let (dir, d) = self.find(scope, &a.id)?;
            let version = d.live.ok_or_else(|| Error::NotFound(format!("dataset {} has no version yet", a.id)))?;
            let m: Materialization = store::read_json(&store::version_dir(&dir, version).join("manifest.json"))?;
            found.push(blend::Found { alias: a.alias.clone(), id: a.id.clone(), version, columns: m.columns });
        }
        let datasets: Vec<lineage::DatasetRead> =
            found.iter().map(|f| lineage::DatasetRead { id: f.id.clone(), version: f.version }).collect();
        let question = serde_json::json!({
            "model": model_run,
            "datasets": found.iter().map(|f| (f.alias.clone(), f.id.clone(), f.version)).collect::<Vec<_>>(),
            "joins": plan.joins,
            "outputs": plan.outputs.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>(),
            "org": scope.org,
            "project": scope.project,
        });
        let key: String = sha2::Sha256::digest(question.to_string().as_bytes()).iter().map(|b| format!("{b:02x}")).collect();
        let blends = self.store.blends(&scope.org, &scope.project);
        let kept = blends.join(&key);
        if let Ok(rows) = store::read_rows(&kept.join(store::ROWS)) {
            return Ok(Blended { rows, datasets, hit: true });
        }
        let session = self.engine.session()?;
        for f in &found {
            let (dir, _) = self.find(scope, &f.id)?;
            let rows = store::read_rows(&store::version_dir(&dir, f.version).join(store::ROWS))?;
            let cols: Vec<(String, ColumnType)> = f.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
            session.load(&f.alias, &cols, &rows.batches)?;
        }
        let rows = blend::run(&session, plan, model, &found)?;
        // Kept whole or not at all; a failure to keep is not a failure to answer.
        let staging = blends.join(format!(".{key}.{}", time::nanos()));
        let keep = (|| -> Result<()> {
            store::write_rows(&staging.join(store::ROWS), &rows)?;
            store::write_json(
                &staging.join("blend.json"),
                &BlendKept { org: scope.org.clone(), project: scope.project.clone(), datasets: datasets.clone() },
            )?;
            std::fs::rename(&staging, &kept).map_err(|e| Error::io(&kept, e))
        })();
        if let Err(e) = keep {
            let _ = std::fs::remove_dir_all(&staging);
            tracing::warn!("a blend's answer was not kept: {e}");
        }
        evict(&blends, BLENDS_KEPT);
        Ok(Blended { rows, datasets, hit: false })
    }

    /// Removes the kept blends that read a dataset: they answer no more.
    fn forget_blends(&self, org: &str, project: &str, id: &str) {
        let Ok(entries) = std::fs::read_dir(self.store.blends(org, project)) else { return };
        for e in entries.flatten() {
            let Ok(k) = store::read_json::<BlendKept>(&e.path().join("blend.json")) else { continue };
            if k.datasets.iter().any(|d| d.id == id) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }

    /// The datasets that read this one, directly or through others, in
    /// the order a cascade runs them.
    pub fn downstream(&self, scope: &Scope, id: &str) -> Result<Vec<Dataset>> {
        let (dir, _) = self.find(scope, id)?;
        Ok(self.downstream_of(&dir, id).into_iter().map(|(_, d, _)| d).collect())
    }
}

/// A recipe version's steps, read from a dataset's directory.
fn steps_of(dir: &Path) -> impl Fn(u32) -> Option<Vec<Step>> + '_ {
    move |m| store::read_json::<Recipe>(&store::recipe_path(dir, m)).ok().map(|r| r.steps)
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
