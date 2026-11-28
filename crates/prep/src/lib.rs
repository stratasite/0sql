//! Data prep: sources, and datasets made from them by a graph the
//! application sends. See `docs/ingestion.md`.
//!
//! A source is where rows come from, and what came: each upload, push
//! or pull kept whole as an immutable snapshot. A dataset is what is
//! browsed, shared and blended: its outputs, each made by one run of a
//! graph (a pipeline) over sources and other datasets, kept as an
//! immutable version with a record of what made it. The application
//! builds the graph, keeps it, and decides when it runs; nothing here
//! runs unasked, and a new snapshot runs nothing.
//!
//! Who may read or change what is the application's to decide: the
//! server keeps no user, as it keeps none for anything else.
//!
//! Dependencies point one way: this crate reads the model only as any
//! caller does, and nothing that plans a query knows this crate is
//! there. That is what keeps a dataset out of every query that did not
//! name it.
//!
//! - `source`: the kinds of source, and the CSV contract
//! - `snapshot`: rows that arrived, typed and met with their source's
//! - `schema`: column types and roles, and how a type evolves
//! - `graph`: a pipeline's nodes, and what is wrong with one
//! - `steps`: the steps of a `steps` node
//! - `formula`: the formula language, compiled to the engine's SQL
//! - `compile`: steps, joins and appends as the engine's SQL
//! - `pipeline`: a graph compiled and run, node by node
//! - `record`: what a run did, per node, in numbers and words
//! - `edits`: a user's edits, carried inline in the graph
//! - `blend`: datasets beside the model's rows, joined as a request says
//! - `engine`: DuckDB, in memory, with external access off
//! - `lineage`: what each output was made from
//! - `store`: where it is all kept, and the only module that knows
//! - `recovery`: what a crash or a stop leaves, and the next start
//! - `backup`: the object store's copy, and its restore

pub mod backup;
pub mod blend;
pub mod compile;
pub mod edits;
pub mod engine;
pub mod error;
pub mod formula;
pub mod graph;
pub mod lineage;
mod materialize;
mod pipeline;
pub mod record;
pub mod recovery;
pub mod schema;
mod snapshot;
pub mod source;
pub mod sql;
pub mod steps;
pub mod store;
pub mod time;

mod runs;
mod sources;

pub use edits::{Edit, Outcome, State};
pub use engine::{Engine, EngineLimits};
pub use error::{Error, Result};
pub use graph::{Graph, Node, Problem};
pub use lineage::Lineage;
pub use materialize::View;
pub use record::{InputPlan, NodeRecord, RunRecord, RunState};
pub use runs::{query_key, Begun, Checked, Keys, NodeColumns, PreviewRequest, Previewed, Pull, RunRequest, Ticket};
pub use schema::{Column, ColumnType, Role};
pub use source::{CsvOptions, Kind, Query, SemanticRead};
pub use sources::{Named, Pushed, SourceChange};
pub use steps::{SortKey, Step};
pub use store::{Dataset, Materialization, Snapshot, Source, Store, DIR};

use graph::Read;
use sha2::Digest;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use zsql_datasource::Batches;

/// How many blends a project keeps; past it, the least recently made go.
const BLENDS_KEPT: usize = 64;

/// How many run records a project keeps; past it, the oldest go.
const RUNS_KEPT: usize = 1000;

/// The longest name a dataset or a source is given.
const NAME_LIMIT: usize = 200;

/// The longest description.
const DESCRIPTION_LIMIT: usize = 2000;

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
        let _ = if path.is_dir() { std::fs::remove_dir_all(path) } else { std::fs::remove_file(path) };
    }
}

/// What a server's data prep is allowed.
#[derive(Clone, Debug)]
pub struct Settings {
    /// The most rows a snapshot or an output holds.
    pub max_rows: usize,
    /// The most bytes an upload, a snapshot or an output is.
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

/// Where a request's sources and datasets are: an org and a project.
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

/// Rows a query returned, with the model's roles for its columns and
/// what made them.
pub struct Arrived {
    pub rows: Batches,
    pub roles: BTreeMap<String, Role>,
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

/// An output version, with what made it.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Version {
    pub manifest: Materialization,
    pub lineage: Option<Lineage>,
    pub run: Option<RunRecord>,
    pub graph: Option<Graph>,
}

pub struct Prep {
    store: Store,
    pub(crate) engine: Engine,
    pub(crate) settings: Settings,
    pub(crate) duckdb_version: String,
    /// The sources and datasets a job is on: one at a time each.
    busy: Mutex<HashSet<PathBuf>>,
    /// The datasets a run is making, by their directory: one at a time.
    running: Mutex<HashMap<PathBuf, (Scope, String)>>,
    /// The snapshots each run going reads, by its dataset's directory: a
    /// source's directory and snapshot number. A source read by a run is
    /// not deleted under it, nor a snapshot it reads pruned, so a run
    /// never ends pointing at rows that are gone.
    holds: Mutex<HashMap<PathBuf, Vec<(PathBuf, u32)>>>,
    /// The sources being deleted, by directory: a run that would read
    /// one is refused. Changed only with `holds` locked first.
    deleting: Mutex<HashSet<PathBuf>>,
    /// The run ids being asked for now: a run id is begun by one request
    /// at a time, so a retry sent while the first is checked waits, and
    /// is answered what the first came to.
    claims: Mutex<HashSet<PathBuf>>,
    claimed: std::sync::Condvar,
    /// The object store each dataset and source is copied to.
    backup: Option<std::sync::Arc<zsql_exec::Objects>>,
    /// Set when the server stops: no job starts.
    stopping: std::sync::atomic::AtomicBool,
    /// Set when a stop's grace is over: a run started before it may no
    /// longer keep what it pulled.
    stopped: std::sync::atomic::AtomicBool,
    /// The directories whose jobs a stop suspended: their markers stay.
    suspended: Mutex<HashSet<PathBuf>>,
    /// What the start found, until the server takes it.
    recovered: Mutex<recovery::Recovered>,
}

/// How long a run waits for another job on a source it pulled to end.
const RUN_JOB_PATIENCE: std::time::Duration = std::time::Duration::from_secs(120);

/// A run id claimed by one request; let go, and the next waiting woken,
/// when dropped.
pub(crate) struct Claim<'a> {
    prep: &'a Prep,
    path: PathBuf,
}

impl Drop for Claim<'_> {
    fn drop(&mut self) {
        lock(&self.prep.claims).remove(&self.path);
        self.prep.claimed.notify_all();
    }
}

/// A source or dataset held for one job; let go when dropped. Its
/// marker goes with it, unless a stop suspended it.
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

pub(crate) fn lock<T>(m: &Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    m.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
}

pub(crate) fn sha256(bytes: &[u8]) -> String {
    sha2::Sha256::digest(bytes).iter().map(|b| format!("{b:02x}")).collect()
}

impl Prep {
    pub fn open(data_dir: &Path, settings: Settings) -> Result<Prep> {
        // An answer handed back may be twice what an output holds: a
        // node between may make more than the output keeps.
        let bound = engine::Bound { rows: settings.max_rows.saturating_mul(2).max(1), bytes: settings.max_bytes.saturating_mul(2).max(1) };
        let engine = Engine::new(settings.engine.clone(), bound);
        let duckdb_version = engine.version()?;
        let prep = Prep {
            store: Store::new(data_dir),
            engine,
            settings,
            duckdb_version,
            busy: Mutex::new(HashSet::new()),
            running: Mutex::new(HashMap::new()),
            holds: Mutex::new(HashMap::new()),
            deleting: Mutex::new(HashSet::new()),
            claims: Mutex::new(HashSet::new()),
            claimed: std::sync::Condvar::new(),
            backup: None,
            stopping: std::sync::atomic::AtomicBool::new(false),
            stopped: std::sync::atomic::AtomicBool::new(false),
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

    /// Sweeps what jobs left half made, and turns each marker and run
    /// left behind into an interruption: cause `crash` for one that was
    /// running, `stop` for one a stop suspended. Nothing is run again.
    fn recover(&self) -> recovery::Recovered {
        use recovery::{Cause, Interrupted, JobKind, Marker, State};
        let mut out = recovery::Recovered { swept: recovery::sweep(&self.store.data_dir()), ..Default::default() };
        for (org, project, root) in recovery::roots(&self.store.data_dir()) {
            let dirs: Vec<(PathBuf, String)> = store::datasets_in(&root)
                .into_iter()
                .map(|(d, x)| (d, x.id))
                .chain(store::sources_in(&root).into_iter().map(|(d, x)| (d, x.id)))
                .collect();
            for (dir, id) in dirs {
                let path = dir.join(recovery::MARKER);
                let Ok(m) = store::read_json::<Marker>(&path) else { continue };
                let cause = if m.state == State::Running { Cause::Crash } else { Cause::Stop };
                let i = Interrupted::new(m.job, &m.started, cause);
                if let Err(e) = store::write_json(&dir.join(recovery::INTERRUPTED), &i) {
                    tracing::warn!("{id} was interrupted and that could not be written: {e}");
                }
                out.interrupted.push(format!("{org}/{project}/{id}"));
                let _ = std::fs::remove_file(&path);
            }
            for path in recovery::entries(&root.join(store::RUNS)) {
                let Ok(mut r) = store::read_json::<RunRecord>(&path) else { continue };
                let cause = match r.state {
                    RunState::Running => Cause::Crash,
                    RunState::Suspended => Cause::Stop,
                    _ => continue,
                };
                let i = Interrupted::new(JobKind::Run, &r.started, cause);
                r.state = RunState::Interrupted;
                r.ended = Some(time::now());
                r.summary = format!("Interrupted: {}", i.reason);
                r.interrupted = Some(i.clone());
                let _ = store::write_json(&path, &r);
                let dir = root.join(&r.dataset);
                if dir.join(store::MANIFEST).exists() {
                    let _ = store::write_json(&dir.join(recovery::INTERRUPTED), &i);
                    self.last_run(&dir, &r);
                }
                out.interrupted.push(format!("{org}/{project}/{} (run {})", r.dataset, r.id));
            }
            evict(&root.join(store::RUNS), RUNS_KEPT);
        }
        out
    }

    /// Stops data prep for a server that is stopping. No job starts from
    /// now; the jobs and runs going are given `grace` to finish, and the
    /// rest are stopped and kept as suspended, which the next start
    /// marks interrupted.
    pub fn stop(&self, grace: std::time::Duration) {
        use recovery::{Marker, State};
        self.stopping.store(true, std::sync::atomic::Ordering::SeqCst);
        let until = std::time::Instant::now() + grace;
        let idle = || lock(&self.busy).is_empty() && lock(&self.running).is_empty();
        while !idle() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        self.stopped.store(true, std::sync::atomic::Ordering::SeqCst);
        let busy: Vec<PathBuf> = lock(&self.busy).iter().cloned().collect();
        for dir in &busy {
            let path = dir.join(recovery::MARKER);
            if let Ok(mut m) = store::read_json::<Marker>(&path) {
                m.state = State::Suspended;
                lock(&self.suspended).insert(dir.clone());
                let _ = store::write_json(&path, &m);
            }
        }
        let runs: Vec<(Scope, String)> = lock(&self.running).values().cloned().collect();
        for (scope, id) in &runs {
            let path = store::run_path(&self.store.root(&scope.org, &scope.project), id);
            if let Ok(mut r) = store::read_json::<RunRecord>(&path) {
                if r.state == RunState::Running {
                    r.state = RunState::Suspended;
                    let _ = store::write_json(&path, &r);
                }
            }
        }
        self.engine.stop();
        // What was stopped unwinds; a moment for it to.
        let until = std::time::Instant::now() + std::time::Duration::from_secs(3);
        while !idle() && std::time::Instant::now() < until {
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        tracing::info!("data prep stopped: {} job(s) and {} run(s) interrupted", busy.len(), runs.len());
    }

    pub(crate) fn is_stopping(&self) -> bool {
        self.stopping.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Claims a run id, at `path`, waiting while another request has it.
    pub(crate) fn claim(&self, path: &Path) -> Claim<'_> {
        let mut claims = lock(&self.claims);
        while claims.contains(path) {
            claims = self.claimed.wait(claims).unwrap_or_else(std::sync::PoisonError::into_inner);
        }
        claims.insert(path.to_path_buf());
        Claim { prep: self, path: path.to_path_buf() }
    }

    /// A run's dataset let go, with the snapshots it held.
    pub(crate) fn release(&self, dir: &Path) {
        lock(&self.running).remove(dir);
        lock(&self.holds).remove(dir);
    }

    /// Holds a snapshot for the run making the dataset at `dir`.
    pub(crate) fn hold(&self, dir: &Path, source: PathBuf, n: u32) {
        lock(&self.holds).entry(dir.to_path_buf()).or_default().push((source, n));
    }

    /// The snapshots of the source at `source` that runs going hold.
    pub(crate) fn held(&self, source: &Path) -> HashSet<u32> {
        lock(&self.holds).values().flatten().filter(|(s, _)| s == source).map(|(_, n)| *n).collect()
    }

    /// Whether a stop's grace is over: what still runs is suspended.
    pub(crate) fn is_stopped(&self) -> bool {
        self.stopped.load(std::sync::atomic::Ordering::SeqCst)
    }

    /// The same, copying each dataset and source to the object store as
    /// it changes.
    pub fn with_backup(mut self, objects: Option<std::sync::Arc<zsql_exec::Objects>>) -> Prep {
        self.backup = objects;
        self
    }

    /// Copies a dataset or a source to the object store, when there is
    /// one, and says in its manifest how that went. A copy that fails
    /// fails nothing.
    pub(crate) fn backed_up(&self, dir: &Path) {
        let Some(objects) = &self.backup else { return };
        let failed = backup::copy(objects, dir).err().map(|e| e.to_string());
        let mark = backup::Backup { at: time::now(), failed: failed.clone() };
        let id = dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        if let Some(why) = &failed {
            tracing::warn!("{id} is not backed up: {why}");
        }
        let written = if dir.join(store::SOURCE).exists() {
            store::read_json::<Source>(&dir.join(store::SOURCE)).and_then(|mut s| {
                s.backup = Some(mark);
                store::write_json(&dir.join(store::SOURCE), &s)
            })
        } else {
            store::read_json::<Dataset>(&dir.join(store::MANIFEST)).and_then(|mut d| {
                d.backup = Some(mark);
                store::write_json(&dir.join(store::MANIFEST), &d)
            })
        };
        if let Err(e) = written {
            tracing::warn!("{id}: {e}");
        }
    }

    /// Removes a copy in the object store, when there is one.
    pub(crate) fn forget_backup(&self, org: &str, project: &str, below: &[&str]) {
        if let Some(objects) = &self.backup {
            if let Err(e) = backup::forget(objects, org, project, below) {
                tracing::warn!("{} was removed and its copy in the object store was not: {e}", below.join("/"));
            }
        }
    }

    /// Brings back every dataset and source of an org the object store
    /// has whole and this node does not: after a disk is lost, or on a
    /// new node. The ones brought back, as `project/id`.
    pub fn restore(&self, org: &str) -> Result<Vec<String>> {
        let objects =
            self.backup.as_ref().ok_or_else(|| Error::Invalid("there is no object store to restore from (ZSQL_OBJECT_STORE)".into()))?;
        let restored = backup::restore(objects, &self.store.data_dir(), org)?;
        tracing::info!("{} dataset(s) and source(s) of {org} restored from the object store", restored.len());
        Ok(restored)
    }

    pub fn settings(&self) -> &Settings {
        &self.settings
    }

    /// Takes a source or dataset for one job, and leaves its marker. A
    /// job ends an interruption: its own outcome is what it says next.
    pub(crate) fn job(&self, dir: &Path, kind: recovery::JobKind) -> Result<Job<'_>> {
        if self.is_stopping() {
            return Err(Error::Stopping("the server is stopping; try again in a moment".into()));
        }
        if !lock(&self.busy).insert(dir.to_path_buf()) {
            return Err(Error::Conflict("this is busy with another job; try again when it is done".into()));
        }
        self.marked(dir, kind)
    }

    /// The same, for a run already started: it may keep what it pulled
    /// through a stop's grace, and waits its turn behind another job on
    /// the same source (another run's pull, say) rather than failing
    /// after a pull that may have taken minutes.
    pub(crate) fn run_job(&self, dir: &Path, kind: recovery::JobKind) -> Result<Job<'_>> {
        let until = std::time::Instant::now() + RUN_JOB_PATIENCE;
        loop {
            if self.is_stopped() {
                return Err(Error::Stopping("the server is stopping; try again in a moment".into()));
            }
            if lock(&self.busy).insert(dir.to_path_buf()) {
                return self.marked(dir, kind);
            }
            if std::time::Instant::now() >= until {
                return Err(Error::Conflict("this is busy with another job; try again when it is done".into()));
            }
            std::thread::sleep(std::time::Duration::from_millis(20));
        }
    }

    /// A job taken: its marker left.
    fn marked(&self, dir: &Path, kind: recovery::JobKind) -> Result<Job<'_>> {
        let job = Job { prep: self, dir: dir.to_path_buf() };
        let marker = recovery::Marker { job: kind, started: time::now(), state: recovery::State::Running };
        store::write_json(&dir.join(recovery::MARKER), &marker)?;
        let _ = std::fs::remove_file(dir.join(recovery::INTERRUPTED));
        Ok(job)
    }

    pub(crate) fn root(&self, scope: &Scope) -> PathBuf {
        self.store.root(&scope.org, &scope.project)
    }

    pub(crate) fn made_by(&self) -> lineage::MadeBy {
        lineage::MadeBy {
            zsql: env!("CARGO_PKG_VERSION").to_string(),
            compiler: lineage::COMPILER_VERSION,
            duckdb: self.duckdb_version.clone(),
        }
    }
}

// --------------------------------------------------------------- datasets

impl Prep {
    /// The directory of a dataset of the project, or not found.
    pub(crate) fn find(&self, scope: &Scope, id: &str) -> Result<(PathBuf, Dataset)> {
        let missing = || Error::NotFound(format!("no dataset {id} in project {}", scope.project));
        if !is_id(id) {
            return Err(missing());
        }
        let dir = self.root(scope).join(id);
        let mut dataset: Dataset = match store::read_json(&dir.join(store::MANIFEST)) {
            Ok(d) => d,
            Err(_) if !dir.join(store::MANIFEST).exists() => return Err(missing()),
            Err(e) => return Err(e),
        };
        dataset.interrupted = store::read_json(&dir.join(recovery::INTERRUPTED)).ok();
        Ok((dir, dataset))
    }

    /// The project's datasets: what is browsed.
    pub fn list(&self, scope: &Scope) -> Vec<Dataset> {
        self.store.all(&scope.org, &scope.project).into_iter().map(|(_, d)| d).collect()
    }

    pub fn get(&self, scope: &Scope, id: &str) -> Result<Dataset> {
        Ok(self.find(scope, id)?.1)
    }

    /// A new dataset. With `over`, a source, its first output is that
    /// source's newest snapshot as it is, and copies nothing: one thing
    /// to list, share and blend. Without, it has no output until a run.
    pub fn create_dataset(&self, scope: &Scope, named: Named, over: Option<&str>) -> Result<Dataset> {
        let id = new_id(&scope.project);
        let root = self.root(scope);
        let staging = root.join(format!(".new-{id}"));
        let now = time::now();
        let source = over.map(|s| self.find_source(scope, s)).transpose()?;
        let fallback = source.as_ref().map(|(_, s)| s.name.clone()).unwrap_or_else(|| "untitled".into());
        let mut d = Dataset {
            id: id.clone(),
            name: name_of(named.name.as_deref(), Some(&fallback))?,
            description: description_of(named.description.as_deref())?,
            created: now.clone(),
            updated: now.clone(),
            live: None,
            made: 0,
            columns: Vec::new(),
            rows: 0,
            notes: Vec::new(),
            last_run: None,
            backup: None,
            interrupted: None,
        };
        let made = (|| {
            std::fs::create_dir_all(&staging).map_err(|e| Error::io(&staging, e))?;
            if let Some((sdir, s)) = &source {
                if s.snapshot == 0 {
                    return Err(Error::Invalid(format!(
                        "source {} has no snapshot yet to be a dataset as it is; run a graph over it",
                        s.id
                    )));
                }
                if !s.types.is_empty() {
                    return Err(Error::Invalid(format!(
                        "source {} is read with types of its own, so a dataset over it is made by a run, not as it is",
                        s.id
                    )));
                }
                let snap: Snapshot = store::read_json(&store::snapshot_dir(sdir, s.snapshot).join("manifest.json"))?;
                let graph = as_it_is(&s.id);
                let lineage = Lineage {
                    dataset: id.clone(),
                    version: 1,
                    run: String::new(),
                    graph: sha256(&serde_json::to_vec(&graph)?),
                    sources: vec![source_read("source", s, &snap)],
                    datasets: Vec::new(),
                    roles: snap.columns.iter().map(|c| (c.name.clone(), c.role)).collect(),
                    made_by: self.made_by(),
                    pinned_at: now.clone(),
                    notes: Vec::new(),
                    columns: snap
                        .columns
                        .iter()
                        .map(|c| lineage::Ancestry {
                            column: c.name.clone(),
                            from: vec![lineage::Origin { source: s.id.clone(), column: c.name.clone() }],
                        })
                        .collect(),
                };
                let m = Materialization {
                    version: 1,
                    made: now.clone(),
                    columns: snap.columns.clone(),
                    rows: snap.rows,
                    bytes: 0,
                    pointer: Some(store::Pointer { source: s.id.clone(), snapshot: s.snapshot }),
                    run: None,
                };
                let v = store::version_dir(&staging, 1);
                store::write_json(&v.join("manifest.json"), &m)?;
                store::write_json(&v.join("lineage.json"), &lineage)?;
                store::write_json(&v.join("graph.json"), &graph)?;
                d.live = Some(1);
                d.made = 1;
                d.columns = snap.columns;
                d.rows = snap.rows;
            }
            store::write_json(&staging.join(store::MANIFEST), &d)?;
            let target = root.join(&id);
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))?;
            store::sync_dir(Some(&root))
        })();
        if let Err(e) = made {
            let _ = std::fs::remove_dir_all(&staging);
            return Err(e);
        }
        self.backed_up(&root.join(&id));
        tracing::info!("dataset {id} made in {}/{}", scope.org, scope.project);
        self.get(scope, &id)
    }

    /// Removes a dataset: every output of it. Its sources stay.
    pub fn delete(&self, scope: &Scope, id: &str) -> Result<()> {
        let (dir, _) = self.find(scope, id)?;
        if let Some((_, run)) = lock(&self.running).get(&dir) {
            return Err(Error::Conflict(format!("dataset {id} is being made by run {run}; delete it when that is done")));
        }
        let _job = self.job(&dir, recovery::JobKind::Delete)?;
        store::remove(&dir)?;
        // Its runs go with it: a record holds what the rows looked like.
        for path in recovery::entries(&self.root(scope).join(store::RUNS)) {
            if store::read_json::<RunRecord>(&path).is_ok_and(|r| r.dataset == id) {
                let _ = std::fs::remove_file(&path);
            }
        }
        self.forget_blends(&scope.org, &scope.project, id);
        self.forget_backup(&scope.org, &scope.project, &[id]);
        tracing::info!("dataset {id} removed from {}/{}", scope.org, scope.project);
        Ok(())
    }

    /// Removes every dataset and source of a project: part of removing
    /// the project. The ids removed.
    pub fn delete_project(&self, org: &str, project: &str) -> Result<Vec<String>> {
        let mut removed = Vec::new();
        for (dir, d) in self.store.all(org, project) {
            store::remove(&dir)?;
            self.forget_blends(org, project, &d.id);
            self.forget_backup(org, project, &[&d.id]);
            removed.push(d.id);
        }
        for (dir, s) in self.store.sources(org, project) {
            store::remove(&dir)?;
            self.forget_backup(org, project, &["sources", &s.id]);
            removed.push(s.id);
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
            None => return Err(Error::NotFound(format!("dataset {} has no output yet", dataset.id))),
        };
        if !store::version_dir(dir, k).join("manifest.json").exists() {
            return Err(Error::NotFound(format!("dataset {} has no version {k}", dataset.id)));
        }
        Ok(k)
    }

    /// A page of an output: the live one, or the one asked for.
    pub fn page(&self, scope: &Scope, id: &str, version: Option<u32>, offset: u64, limit: u64, view: &View<'_>) -> Result<Page> {
        let (dir, dataset) = self.find(scope, id)?;
        let k = self.version_of(&dir, &dataset, version)?;
        let (m, path) = store::version_rows_path(&dir, k)?;
        let (total, rows) = materialize::page_of(&self.engine, &m.columns, (&path, m.rows), offset, limit, view)?;
        Ok(Page { version: k, columns: m.columns, total, offset, rows })
    }

    /// What made an output: the live one, or the one asked for.
    pub fn lineage(&self, scope: &Scope, id: &str, version: Option<u32>) -> Result<Lineage> {
        let (dir, dataset) = self.find(scope, id)?;
        let k = self.version_of(&dir, &dataset, version)?;
        store::read_lineage(&dir, k)
    }

    /// An output version: its manifest, lineage, the run that made it
    /// and the graph it ran.
    pub fn version(&self, scope: &Scope, id: &str, version: Option<u32>) -> Result<Version> {
        let (dir, dataset) = self.find(scope, id)?;
        let k = self.version_of(&dir, &dataset, version)?;
        let v = store::version_dir(&dir, k);
        Ok(Version {
            manifest: store::read_json(&v.join("manifest.json"))?,
            lineage: store::read_json(&v.join("lineage.json")).ok(),
            run: store::read_json(&v.join("run.json")).ok(),
            graph: store::read_json(&v.join("graph.json")).ok(),
        })
    }

    /// Says in a dataset's manifest how its last run ended.
    pub(crate) fn last_run(&self, dir: &Path, r: &RunRecord) {
        let path = dir.join(store::MANIFEST);
        if let Ok(mut d) = store::read_json::<Dataset>(&path) {
            d.last_run = Some(store::LastRun { id: r.id.clone(), state: r.state, at: r.ended.clone().unwrap_or_else(time::now) });
            d.updated = time::now();
            let _ = store::write_json(&path, &d);
        }
    }
}

/// The graph of a dataset that is a source as it is.
fn as_it_is(source: &str) -> Graph {
    Graph {
        nodes: vec![
            Node::Source { id: "source".into(), note: None, source: source.into(), read: Read::Latest, types: BTreeMap::new() },
            Node::Output {
                id: "output".into(),
                note: None,
                input: "source".into(),
                roles: BTreeMap::new(),
                key: Vec::new(),
                checks: Vec::new(),
            },
        ],
    }
}

pub(crate) fn source_read(node: &str, s: &Source, snap: &Snapshot) -> lineage::SourceRead {
    lineage::SourceRead {
        node: node.to_string(),
        source: s.id.clone(),
        kind: snap.kind,
        snapshot: snap.number,
        read: s.read,
        file_name: snap.file_name.clone(),
        sha256: snap.sha256.clone(),
        parse: snap.parse.clone(),
        semantic: snap.semantic.clone(),
        origin: snap.origin.clone(),
    }
}

// ----------------------------------------------------------------- blends

impl Prep {
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
            let version = d.live.ok_or_else(|| Error::NotFound(format!("dataset {} has no output yet", a.id)))?;
            let m: Materialization = store::read_json(&store::version_dir(&dir, version).join("manifest.json"))?;
            found.push(blend::Found { alias: a.alias.clone(), id: a.id.clone(), version, columns: m.columns });
        }
        let datasets: Vec<lineage::DatasetRead> =
            found.iter().map(|f| lineage::DatasetRead { id: f.id.clone(), version: f.version, node: None }).collect();
        let question = serde_json::json!({
            "model": model_run,
            "datasets": found.iter().map(|f| (f.alias.clone(), f.id.clone(), f.version)).collect::<Vec<_>>(),
            "joins": plan.joins,
            "outputs": plan.outputs.iter().map(|o| format!("{o:?}")).collect::<Vec<_>>(),
            "org": scope.org,
            "project": scope.project,
        });
        let key = sha256(question.to_string().as_bytes());
        let blends = self.store.blends(&scope.org, &scope.project);
        let kept = blends.join(&key);
        if let Ok(rows) = store::read_rows(&kept.join(store::ROWS)) {
            return Ok(Blended { rows, datasets, hit: true });
        }
        let session = self.engine.session()?;
        for f in &found {
            let (dir, _) = self.find(scope, &f.id)?;
            let (_, rows) = store::version_rows(&dir, f.version)?;
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
    pub(crate) fn forget_blends(&self, org: &str, project: &str, id: &str) {
        let Ok(entries) = std::fs::read_dir(self.store.blends(org, project)) else { return };
        for e in entries.flatten() {
            let Ok(k) = store::read_json::<BlendKept>(&e.path().join("blend.json")) else { continue };
            if k.datasets.iter().any(|d| d.id == id) {
                let _ = std::fs::remove_dir_all(e.path());
            }
        }
    }
}

// -------------------------------------------------------------------- ids

/// Sixteen hex digits, unguessable enough that one is never made twice.
fn hex_id(salt: &str) -> String {
    static COUNTER: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let n = COUNTER.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    let digest = sha2::Sha256::digest(format!("{}\n{}\n{n}\n{salt}", time::nanos(), std::process::id()).as_bytes());
    digest.iter().take(8).map(|b| format!("{b:02x}")).collect()
}

fn hex16(h: &str) -> bool {
    h.len() == 16 && h.bytes().all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b))
}

/// A dataset's id: `ds_` and sixteen hex digits, checked before it names
/// a directory.
pub fn new_id(salt: &str) -> String {
    format!("ds_{}", hex_id(salt))
}

pub fn is_id(id: &str) -> bool {
    id.strip_prefix("ds_").is_some_and(hex16)
}

/// A source's id: `src_` and sixteen hex digits.
pub fn new_source_id(salt: &str) -> String {
    format!("src_{}", hex_id(salt))
}

pub fn is_source_id(id: &str) -> bool {
    id.strip_prefix("src_").is_some_and(hex16)
}

/// A run's id is the application's: 1 to 128 letters, digits, `_`, `-`,
/// `.` or `:`, so it names a file and nothing else.
pub fn is_run_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && !id.starts_with('.') && id.bytes().all(|b| b.is_ascii_alphanumeric() || b"_-.:".contains(&b))
}

pub(crate) fn name_of(name: Option<&str>, file_name: Option<&str>) -> Result<String> {
    let from_file = file_name.map(|f| f.rsplit(['/', '\\']).next().unwrap_or(f)).map(|f| f.strip_suffix(".csv").unwrap_or(f).to_string());
    let name = name.map(str::trim).filter(|n| !n.is_empty()).map(str::to_string).or(from_file).unwrap_or_else(|| "untitled".into());
    if name.chars().count() > NAME_LIMIT || name.chars().any(char::is_control) {
        return Err(Error::Invalid(format!("a name is at most {NAME_LIMIT} characters, none of them control characters")));
    }
    Ok(name)
}

pub(crate) fn description_of(d: Option<&str>) -> Result<Option<String>> {
    let d = d.map(str::trim).filter(|d| !d.is_empty());
    if d.is_some_and(|d| d.chars().count() > DESCRIPTION_LIMIT) {
        return Err(Error::Invalid(format!("a description is at most {DESCRIPTION_LIMIT} characters")));
    }
    Ok(d.map(str::to_string))
}

#[cfg(test)]
mod tests;

#[cfg(test)]
mod bench;
