//! Running statements for a server: a record first, then a seat, then
//! the statement, its rows written to disk as they arrive.
//!
//! A run is a thread of this process. So many run at once and the rest
//! wait in a queue that has an end; a request past the end is refused
//! as busy rather than left to pile up behind a warehouse that has
//! stopped answering. Nothing here knows HTTP or a model: what arrives
//! is a statement, where it came from, and how to open the datasource
//! it runs on.

use crate::engine::{rfc3339, unix_now};
use crate::error::{Error, Result};
use crate::record::{Kind, Record, Status};
use crate::registry::{Registry, RESTARTED};
use crate::store::{rows_file, Store};
use std::collections::{HashMap, VecDeque};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Condvar, Mutex, MutexGuard};
use std::time::{Duration, Instant};
use zsql_datasource::{Sink, Source};

/// How much runs at once, and how much may wait.
#[derive(Debug, Clone)]
pub struct Limits {
    /// Statements running at once.
    pub concurrent: usize,
    /// Runs waiting for a seat; one more is refused as busy.
    pub queue_max: usize,
    /// This node's name, kept with every run it takes.
    pub hostname: String,
    /// How often this node says its runs are alive. A run not seen for
    /// six of these is nobody's, and is ended as failed.
    pub beat: Duration,
}

impl Limits {
    /// A hundred at once, five thousand waiting, a beat every ten seconds.
    pub fn of(hostname: &str) -> Limits {
        Limits { concurrent: 100, queue_max: 5_000, hostname: hostname.to_string(), beat: Duration::from_secs(10) }
    }
}

/// Opens the datasource a run reads, on the thread that runs it: a
/// connection never crosses threads, and a credential lives no longer
/// than the closure that carries it.
pub type Opener = Box<dyn FnOnce() -> zsql_datasource::Result<Box<dyn Source>> + Send>;

/// A statement to run and where it came from.
pub struct Submit {
    pub kind: Kind,
    pub org_uid: String,
    pub project_uid: String,
    pub branch: String,
    pub datasource_uid: String,
    pub sql: String,
    pub spec: Option<serde_json::Value>,
    pub expr: Option<String>,
    pub row_limit: Option<u64>,
    pub tables: Vec<String>,
    /// What an identical question finds the result by; `None` for a run
    /// that is neither looked up nor found again.
    pub cache_key: Option<String>,
    /// How long the result answers.
    pub cache_ttl: Duration,
    /// Run even though a stored result would answer.
    pub fresh: bool,
    pub open: Opener,
}

/// How a request was met.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum How {
    /// A stored result answers; nothing ran.
    Hit,
    /// An identical run was already in flight; this is that run.
    Attached,
    /// A run of its own, queued.
    Started,
}

struct Job {
    record: Record,
    open: Opener,
    cache_ttl: Duration,
    cancel: Arc<AtomicBool>,
}

#[derive(Default)]
struct State {
    queue: VecDeque<Job>,
    /// Workers waiting for a job.
    idle: usize,
    workers: usize,
    /// The flag of each run that is running, for a cancel to raise.
    cancels: HashMap<String, Arc<AtomicBool>>,
}

struct Inner {
    registry: Arc<dyn Registry>,
    store: Store,
    limits: Limits,
    state: Mutex<State>,
    /// A job was queued.
    queued: Condvar,
    /// A record changed status.
    changed: Condvar,
}

/// The runs of one server.
#[derive(Clone)]
pub struct Runs {
    inner: Arc<Inner>,
}

/// How long a worker with nothing to do stays before its thread ends.
const IDLE: Duration = Duration::from_secs(30);

impl Runs {
    /// Opens the runs whose results live under `dir`. Whatever this node
    /// had in progress when it last stopped is ended as failed: nothing
    /// of it survived.
    pub fn open(dir: &Path, registry: Arc<dyn Registry>, limits: Limits) -> Result<Runs> {
        let store = Store::open(dir)?;
        let ended = registry.fail_unfinished(&limits.hostname, RESTARTED)?;
        if ended > 0 {
            tracing::warn!(runs = ended as u64, "runs that were in progress when the server stopped were ended as failed");
        }
        let inner = Arc::new(Inner { registry, store, limits, state: Mutex::default(), queued: Condvar::new(), changed: Condvar::new() });
        // The beat: this node's runs are marked alive, and runs no node
        // has marked for a while are ended. It stops with the runs.
        let alive = Arc::downgrade(&inner);
        let beat = inner.limits.beat.max(Duration::from_millis(10));
        std::thread::Builder::new()
            .name("zsql-beat".into())
            .spawn(move || loop {
                std::thread::sleep(beat);
                let Some(inner) = alive.upgrade() else { return };
                let seen = inner.registry.touch(&inner.limits.hostname).and_then(|()| inner.registry.fail_abandoned(beat * 6, RESTARTED));
                match seen {
                    Ok(0) => {}
                    Ok(ended) => {
                        tracing::warn!(runs = ended as u64, "runs no node was running any more were ended as failed");
                        let _held = inner.lock();
                        inner.changed.notify_all();
                    }
                    Err(e) => tracing::warn!("the runs could not be marked alive: {e}"),
                }
            })
            .map_err(|e| Error::Io("a thread for the beat".into(), e))?;
        Ok(Runs { inner })
    }

    /// Takes a statement: answers from a stored result, attaches to an
    /// identical run in flight, or records a run of its own and queues
    /// it. The record is written before anything runs.
    pub fn submit(&self, submit: Submit) -> Result<(Record, How)> {
        let inner = &self.inner;
        let mut state = inner.lock();
        if let Some(key) = submit.cache_key.as_deref().filter(|_| submit.kind == Kind::Semantic) {
            if !submit.fresh {
                // A result another node made is found here too, and
                // answers only where its rows can be read.
                if let Some(hit) = inner.registry.cached(key)?.filter(|hit| rows_file(&inner.store.dir_of(&hit.id)).exists()) {
                    return Ok((hit, How::Hit));
                }
            }
            // A fresh request too: that run is reading the warehouse now.
            if let Some(live) = inner.registry.in_flight(key)? {
                return Ok((live, How::Attached));
            }
        }
        if state.queue.len() >= inner.limits.queue_max {
            return Err(Error::Busy(inner.limits.queue_max));
        }
        let now = rfc3339(unix_now());
        let record = Record {
            id: new_id(&inner.limits.hostname),
            kind: submit.kind,
            status: Status::Queued,
            error: None,
            row_count: None,
            cache_key: submit.cache_key,
            dirty: false,
            tables: submit.tables,
            org_uid: submit.org_uid,
            project_uid: submit.project_uid,
            branch: submit.branch,
            datasource_uid: submit.datasource_uid,
            sql: submit.sql,
            spec: submit.spec,
            expr: submit.expr,
            row_limit: submit.row_limit,
            hostname: inner.limits.hostname.clone(),
            object_store_path: None,
            created_at: now.clone(),
            queued_at: now.clone(),
            started_at: None,
            finished_at: None,
            updated_at: now,
            cancelled_at: None,
            expires_at: None,
        };
        match inner.registry.insert(&record) {
            Ok(()) => {}
            // Another node queued the same question between the look
            // and the insert: this is that run.
            Err(Error::InFlight(key)) => {
                let live = inner.registry.in_flight(&key)?.ok_or(Error::InFlight(key))?;
                return Ok((live, How::Attached));
            }
            Err(e) => return Err(e),
        }
        state.queue.push_back(Job { record: record.clone(), open: submit.open, cache_ttl: submit.cache_ttl, cancel: Arc::default() });
        if state.idle == 0 && state.workers < inner.limits.concurrent.max(1) {
            state.workers += 1;
            let worker = inner.clone();
            std::thread::Builder::new()
                .name("zsql-run".into())
                .spawn(move || worker.work())
                .map_err(|e| Error::Io("a thread for the run".into(), e))?;
        }
        inner.queued.notify_one();
        Ok((record, How::Started))
    }

    pub fn get(&self, id: &str) -> Result<Option<Record>> {
        self.inner.registry.get(id)
    }

    /// The record once the run is over, or as it stands after `wait`.
    pub fn wait(&self, id: &str, wait: Duration) -> Result<Option<Record>> {
        let inner = &self.inner;
        let deadline = Instant::now() + wait;
        // Read under the lock a status change is announced under, so a
        // change between the read and the wait cannot be missed.
        let mut state = inner.lock();
        loop {
            let Some(record) = inner.registry.get(id)? else { return Ok(None) };
            let left = deadline.saturating_duration_since(Instant::now());
            if record.status.is_final() || left.is_zero() {
                return Ok(Some(record));
            }
            state = inner.changed.wait_timeout(state, left).unwrap_or_else(std::sync::PoisonError::into_inner).0;
        }
    }

    /// Cancels a run. One still waiting never starts. One that is
    /// running is told to stop, and stops when its read next hands over
    /// rows or gives up: the record says `cancelled` once it has.
    pub fn cancel(&self, id: &str) -> Result<Option<Record>> {
        let inner = &self.inner;
        let mut state = inner.lock();
        let Some(mut record) = inner.registry.get(id)? else { return Ok(None) };
        if let Some(at) = state.queue.iter().position(|j| j.record.id == id) {
            state.queue.remove(at);
            let now = rfc3339(unix_now());
            record.status = Status::Cancelled;
            record.cancelled_at = Some(now.clone());
            record.finished_at = Some(now.clone());
            record.updated_at = now;
            inner.registry.save(&record)?;
            inner.changed.notify_all();
        } else if let Some(flag) = state.cancels.get(id) {
            flag.store(true, Ordering::SeqCst);
        }
        Ok(Some(record))
    }

    /// Where a finished run's rows are.
    pub fn rows_path(&self, record: &Record) -> PathBuf {
        rows_file(&self.inner.store.dir_of(&record.id))
    }
}

impl Inner {
    fn lock(&self) -> MutexGuard<'_, State> {
        self.state.lock().unwrap_or_else(std::sync::PoisonError::into_inner)
    }

    /// A worker: takes jobs until there has been none for a while.
    fn work(self: Arc<Self>) {
        loop {
            let job = {
                let mut state = self.lock();
                loop {
                    if let Some(job) = state.queue.pop_front() {
                        state.cancels.insert(job.record.id.clone(), job.cancel.clone());
                        break job;
                    }
                    state.idle += 1;
                    let (back, waited) = self.queued.wait_timeout(state, IDLE).unwrap_or_else(std::sync::PoisonError::into_inner);
                    state = back;
                    state.idle -= 1;
                    if waited.timed_out() && state.queue.is_empty() {
                        state.workers -= 1;
                        return;
                    }
                }
            };
            let id = job.record.id.clone();
            // A panic in a transport ends its run, not the server's
            // ability to run: the record is closed and the seat freed.
            let mut record = job.record.clone();
            let ran = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| self.run(job)));
            if let Ok(Ok(finished)) = &ran {
                record = finished.clone();
            } else {
                let why = match ran {
                    Ok(Err(e)) => format!("the run's record could not be kept: {e}"),
                    _ => "the run stopped on an internal error".to_string(),
                };
                tracing::error!(run = %id, "{why}");
                let now = rfc3339(unix_now());
                record.status = Status::Failed;
                record.error = Some(why);
                record.finished_at = Some(now.clone());
                record.updated_at = now;
                let _ = self.registry.save(&record);
            }
            let mut state = self.lock();
            state.cancels.remove(&id);
            drop(state);
            // Under no lock a waiter holds while it reads the record, the
            // change is announced after the record says it.
            let _held = self.lock();
            self.changed.notify_all();
        }
    }

    /// One run, start to finish. The record is saved at each change;
    /// what comes back is how it ended.
    fn run(&self, job: Job) -> Result<Record> {
        let Job { mut record, open, cache_ttl, cancel } = job;
        let started = unix_now();
        record.status = Status::Running;
        record.started_at = Some(rfc3339(started));
        record.updated_at = rfc3339(started);
        self.registry.save(&record)?;
        {
            let _held = self.lock();
            self.changed.notify_all();
        }
        let t0 = Instant::now();
        let outcome: Result<u64> = (|| {
            let mut src = open()?;
            let mut writing = self.store.begin(&record.id)?;
            let streamed = src.stream(&record.sql, &mut Stoppable { sink: &mut writing, cancel: &cancel });
            match (streamed, writing.failure()) {
                (_, Some(why)) => Err(why),
                (Err(e), None) => Err(Error::Source(e)),
                (Ok(()), None) => {
                    let rows = writing.rows();
                    writing.finish()?;
                    Ok(rows)
                }
            }
        })();
        let ended = unix_now();
        record.finished_at = Some(rfc3339(ended));
        record.updated_at = rfc3339(ended);
        match outcome {
            Ok(rows) => {
                record.status = Status::Succeeded;
                record.row_count = Some(rows);
                record.expires_at = Some(rfc3339(ended + cache_ttl.as_secs()));
            }
            Err(_) if cancel.load(Ordering::SeqCst) => {
                record.status = Status::Cancelled;
                record.cancelled_at = Some(rfc3339(ended));
            }
            Err(e) => {
                record.status = Status::Failed;
                record.error = Some(e.to_string());
            }
        }
        tracing::info!(
            run = %record.id,
            kind = record.kind.as_str(),
            status = record.status.as_str(),
            datasource = %record.datasource_uid,
            rows = record.row_count.unwrap_or(0),
            ms = t0.elapsed().as_millis() as u64,
            "a run ended"
        );
        self.registry.save(&record)?;
        Ok(record)
    }
}

/// A sink that stops taking rows once its run is cancelled.
struct Stoppable<'a> {
    sink: &'a mut dyn Sink,
    cancel: &'a AtomicBool,
}

impl Stoppable<'_> {
    fn check(&self) -> zsql_datasource::Result<()> {
        if self.cancel.load(Ordering::SeqCst) {
            return Err(zsql_datasource::Error::Query("run".into(), "cancelled".into()));
        }
        Ok(())
    }
}

impl Sink for Stoppable<'_> {
    fn schema(&mut self, schema: &arrow_schema::SchemaRef) -> zsql_datasource::Result<()> {
        self.check()?;
        self.sink.schema(schema)
    }

    fn batch(&mut self, batch: arrow_array::RecordBatch) -> zsql_datasource::Result<()> {
        self.check()?;
        self.sink.batch(batch)
    }
}

/// An id no other run has: the day, then a digest of this node, this
/// process, the clock and a count.
fn new_id(hostname: &str) -> String {
    static NEXT: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let digest = crate::engine::sha256(&format!("{hostname}\n{}\n{nanos}\n{}", std::process::id(), NEXT.fetch_add(1, Ordering::Relaxed)));
    format!("r-{}-{}", rfc3339(unix_now())[..10].replace('-', ""), &digest[..20])
}
