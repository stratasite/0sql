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
use crate::objects::Objects;
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
    /// The share of the disk, in percent, this node's results may take.
    pub local_max_percent: u8,
    /// How often expired and dirty results are cleared away.
    pub sweep: Duration,
    /// The share as a number of bytes, in place of a share of the disk:
    /// for a test, which cannot fill a disk to see what happens.
    #[doc(hidden)]
    pub local_max_bytes: Option<u64>,
    /// Connections kept for reuse, across every datasource: one more
    /// and the one unused longest is closed.
    pub pool_max: usize,
    /// How long a connection nothing uses is kept.
    pub pool_idle: Duration,
}

impl Limits {
    /// A hundred at once, five thousand waiting, a beat every ten seconds.
    pub fn of(hostname: &str) -> Limits {
        Limits {
            concurrent: 100,
            queue_max: 5_000,
            hostname: hostname.to_string(),
            beat: Duration::from_secs(10),
            local_max_percent: 60,
            sweep: Duration::from_secs(300),
            local_max_bytes: None,
            pool_max: 200,
            pool_idle: Duration::from_secs(60),
        }
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
    /// Connections kept under a key that starts with this are closed
    /// before this run opens its own: for an engine that cannot be
    /// open two ways at once in one process.
    pub close_first: Option<String>,
    /// What a connection this run opens may be reused under: the
    /// datasource, where it points, how far it is trusted and whose
    /// credential opened it, as a digest. `None` and it is closed
    /// when the run ends.
    pub pool_key: Option<String>,
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
    pool_key: Option<String>,
    close_first: Option<String>,
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
    /// Where results are kept for every node, when there is such a
    /// place; without one, a result is only on this node's disk.
    objects: Option<Arc<Objects>>,
    /// Connections that are open and not in use, oldest first.
    pool: Mutex<Vec<Idle>>,
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

/// How soon a kept connection that has gone stale says so. A failure
/// that takes longer than this was the statement's, not the connection's.
const STALE_WITHIN: Duration = Duration::from_secs(2);

/// How long a worker with nothing to do stays before its thread ends.
const IDLE: Duration = Duration::from_secs(30);

impl Runs {
    /// The object store results are kept in, when there is one.
    pub fn objects(&self) -> Option<&Arc<Objects>> {
        self.inner.objects.as_ref()
    }

    /// Opens the runs whose results live under `dir`. Whatever this node
    /// had in progress when it last stopped is ended as failed: nothing
    /// of it survived.
    pub fn open(dir: &Path, registry: Arc<dyn Registry>, objects: Option<Arc<Objects>>, limits: Limits) -> Result<Runs> {
        let store = Store::open(dir)?;
        let ended = registry.fail_unfinished(&limits.hostname, RESTARTED)?;
        if ended > 0 {
            tracing::warn!(runs = ended as u64, "runs that were in progress when the server stopped were ended as failed");
        }
        let inner = Arc::new(Inner {
            registry,
            store,
            objects,
            pool: Mutex::default(),
            limits,
            state: Mutex::default(),
            queued: Condvar::new(),
            changed: Condvar::new(),
        });
        // The beat: this node's runs are marked alive, and runs no node
        // has marked for a while are ended. It stops with the runs.
        let alive = Arc::downgrade(&inner);
        let beat = inner.limits.beat.max(Duration::from_millis(10));
        let mut swept = Instant::now();
        let sweeping = Arc::new(AtomicBool::new(false));
        std::thread::Builder::new()
            .name("zsql-beat".into())
            .spawn(move || loop {
                std::thread::sleep(beat);
                let Some(inner) = alive.upgrade() else { return };
                // Alive first, and only the runs a thread here holds: a
                // record anything else left unfinished stops being
                // marked, and is ended like any other nobody is running.
                let held = inner.held();
                let seen = inner.registry.touch(&held).and_then(|()| inner.registry.fail_abandoned(beat * 6, RESTARTED));
                inner.close_idle();
                // Cleanup can take a while, and the beat must not wait
                // on it: other nodes would take this one for dead.
                if swept.elapsed() >= inner.limits.sweep && !sweeping.swap(true, Ordering::SeqCst) {
                    swept = Instant::now();
                    let (sweeper, done) = (inner.clone(), sweeping.clone());
                    let spawned = std::thread::Builder::new().name("zsql-sweep".into()).spawn(move || {
                        if let Err(e) = sweeper.sweep() {
                            tracing::warn!("expired results could not all be cleared away: {e}");
                        }
                        done.store(false, Ordering::SeqCst);
                    });
                    if spawned.is_err() {
                        sweeping.store(false, Ordering::SeqCst);
                    }
                }
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
                if let Some(hit) = inner.registry.cached(key)?.filter(|hit| inner.readable(hit)) {
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
        state.queue.push_back(Job {
            record: record.clone(),
            pool_key: submit.pool_key,
            close_first: submit.close_first,
            open: submit.open,
            cache_ttl: submit.cache_ttl,
            cancel: Arc::default(),
        });
        // A worker for this job unless one that is idle will take it:
        // an idle worker is counted until it wakes, so the jobs already
        // waiting are counted against the idle ones, and a burst gets a
        // thread each rather than one between them.
        if state.queue.len() > state.idle && state.workers < inner.limits.concurrent.max(1) {
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

    /// Where a finished run's rows are on this node's disk, once they
    /// are: a result another node made is fetched from the object store
    /// first, and later readers find it here.
    pub fn rows(&self, record: &Record) -> Result<PathBuf> {
        self.inner.rows(record)
    }

    /// Marks as dirty every result of a datasource that read any of
    /// `tables`, in a project or in all of them: it no longer answers,
    /// and the next request runs. How many were marked.
    pub fn invalidate(&self, org_uid: &str, project_uid: Option<&str>, datasource_uid: &str, tables: &[String]) -> Result<usize> {
        self.inner.registry.mark_dirty(org_uid, project_uid, datasource_uid, tables)
    }

    /// Clears away what no longer answers, now rather than at the next
    /// sweep.
    pub fn sweep(&self) -> Result<()> {
        self.inner.sweep()
    }

    /// How many connections are open and not in use.
    pub fn idle_connections(&self) -> usize {
        self.inner.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner).len()
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

    /// The runs a thread of this process holds: waiting for a seat, or
    /// in one.
    fn held(&self) -> Vec<String> {
        let state = self.lock();
        state.queue.iter().map(|job| job.record.id.clone()).chain(state.cancels.keys().cloned()).collect()
    }

    /// A kept connection under `key`, the one most recently used.
    fn take(&self, key: &str) -> Option<Box<dyn Source>> {
        let mut pool = self.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        let at = pool.iter().rposition(|idle| idle.key == key && idle.since.elapsed() < self.limits.pool_idle)?;
        Some(pool.remove(at).source)
    }

    /// Keeps a connection for the next run under `key`. One more than
    /// the pool holds, and the one unused longest is closed.
    fn keep(&self, key: &str, source: Box<dyn Source>) {
        if self.limits.pool_max == 0 {
            return;
        }
        let mut pool = self.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
        pool.push(Idle { key: key.to_string(), source, since: Instant::now() });
        while pool.len() > self.limits.pool_max {
            pool.remove(0);
        }
    }

    /// Closes every kept connection whose key starts with `prefix`.
    fn close_under(&self, prefix: &str) {
        let closed: Vec<Idle> = {
            let mut pool = self.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let (closed, kept) = std::mem::take(&mut *pool).into_iter().partition(|idle| idle.key.starts_with(prefix));
            *pool = kept;
            closed
        };
        drop(closed);
    }

    /// Closes connections nothing has used for a while.
    fn close_idle(&self) {
        let stale: Vec<Idle> = {
            let mut pool = self.pool.lock().unwrap_or_else(std::sync::PoisonError::into_inner);
            let (stale, fresh) = std::mem::take(&mut *pool).into_iter().partition(|idle| idle.since.elapsed() >= self.limits.pool_idle);
            *pool = fresh;
            stale
        };
        // Closed outside the lock: a connection may take a moment to go.
        drop(stale);
    }

    /// Whether a stored result can be read from here: its rows are on
    /// this disk, or in a store this node can fetch them from.
    fn readable(&self, record: &Record) -> bool {
        rows_file(&self.store.dir_of(&record.id)).exists() || (self.objects.is_some() && record.object_store_path.is_some())
    }

    fn rows(&self, record: &Record) -> Result<PathBuf> {
        let path = rows_file(&self.store.dir_of(&record.id));
        if !path.exists() {
            let (Some(objects), Some(key)) = (&self.objects, &record.object_store_path) else {
                return Err(Error::NotFound(format!(
                    "{}: its rows are on another node ({}) and there is no object store to read them from",
                    record.id, record.hostname
                )));
            };
            self.make_room()?;
            objects.download(key, &path)?;
        }
        // Read now: the last to be read is the last to be evicted.
        if let Ok(file) = std::fs::File::open(&path) {
            let _ = file.set_modified(std::time::SystemTime::now());
        }
        Ok(path)
    }

    /// Each result on this disk: its id, its size and when it was last read.
    fn on_disk(&self) -> Vec<(String, PathBuf, u64, std::time::SystemTime)> {
        let mut found = Vec::new();
        let Ok(shards) = std::fs::read_dir(self.store.root()) else { return found };
        for shard in shards.flatten().filter(|e| e.path().is_dir()) {
            let Ok(results) = std::fs::read_dir(shard.path()) else { continue };
            for dir in results.flatten().filter(|e| e.path().is_dir()) {
                let id = dir.file_name().to_string_lossy().to_string();
                // A result still being written is its writer's.
                if id.contains(".tmp-") {
                    continue;
                }
                let meta = std::fs::metadata(rows_file(&dir.path()));
                let size = meta.as_ref().map(|m| m.len()).unwrap_or(0);
                let read = meta.and_then(|m| m.modified()).unwrap_or(std::time::UNIX_EPOCH);
                found.push((id, dir.path(), size, read));
            }
        }
        found
    }

    /// Makes sure this node's results are within their share of the
    /// disk before another is written. With an object store the least
    /// recently read copies go, since they are only copies; without
    /// one, what no longer answers is cleared away and, if that is not
    /// enough, the run is refused.
    fn make_room(&self) -> Result<()> {
        let share = disk_bytes(self.store.root()).map(|total| total / 100 * u64::from(self.limits.local_max_percent.min(100)));
        let Some(allowed) = self.limits.local_max_bytes.or(share) else { return Ok(()) };
        let used = |found: &[(String, PathBuf, u64, std::time::SystemTime)]| found.iter().map(|f| f.2).sum::<u64>();
        let mut found = self.on_disk();
        if used(&found) < allowed {
            return Ok(());
        }
        if self.objects.is_none() {
            self.sweep()?;
            found = self.on_disk();
            if used(&found) < allowed {
                return Ok(());
            }
            return Err(self.disk_full());
        }
        found.sort_by_key(|f| f.3);
        let mut over = used(&found).saturating_sub(allowed);
        for (id, dir, size, _) in found {
            if over == 0 {
                break;
            }
            // Only what the store also holds is only a copy.
            // Nor is the result of a run that has not ended, whose record
            // does not yet say where it went.
            let copied = self
                .registry
                .get(&id)?
                .is_none_or(|r| r.status.is_final() && (r.object_store_path.is_some() || r.status != Status::Succeeded));
            if copied && std::fs::remove_dir_all(&dir).is_ok() {
                over = over.saturating_sub(size.max(1));
            }
        }
        Ok(())
    }

    fn disk_full(&self) -> Error {
        Error::DiskFull(format!(
            "this server's disk is full of results: they may take {}% of it (ZSQL_LOCAL_CACHE_MAX_PERCENT) and every one still answers. \
             Configure an object store with ZSQL_OBJECT_STORE, so results are kept there and this disk holds only copies",
            self.limits.local_max_percent
        ))
    }

    /// A failed write that was the disk running out says so, and what
    /// to do about it.
    fn or_disk_full(&self, why: Error) -> Error {
        let text = why.to_string().to_lowercase();
        if text.contains("no space left") || text.contains("storage full") {
            return self.disk_full();
        }
        why
    }

    /// Clears away what no longer answers: an expired or dirty result
    /// is deleted from the object store and from this disk. Each node
    /// clears its own disk; whichever gets there first clears the store.
    fn sweep(&self) -> Result<()> {
        if let Some(objects) = &self.objects {
            for mut record in self.registry.stale_objects(500)? {
                if let Some(key) = record.object_store_path.take() {
                    objects.delete(&key)?;
                    self.registry.save(&record)?;
                }
            }
        }
        let now = unix_now();
        let mut cleared = 0u64;
        for (id, dir, _, _) in self.on_disk() {
            // A run that has not ended is still writing its result, or
            // has just landed it and not yet said so: it is left alone.
            let answers = self.registry.get(&id)?.is_some_and(|r| {
                !r.status.is_final()
                    || (r.status == Status::Succeeded
                        && !r.dirty
                        && r.expires_at.as_deref().and_then(crate::engine::parse_rfc3339).is_some_and(|until| until > now))
            });
            if !answers && std::fs::remove_dir_all(&dir).is_ok() {
                cleared += 1;
            }
        }
        let forgotten = self.registry.prune()?;
        if cleared > 0 || forgotten > 0 {
            tracing::info!(results = cleared, records = forgotten as u64, "results that no longer answer were cleared away");
        }
        Ok(())
    }

    /// One run, start to finish. The record is saved at each change;
    /// what comes back is how it ended.
    fn run(&self, job: Job) -> Result<Record> {
        let Job { mut record, pool_key, close_first, open, cache_ttl, cancel } = job;
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
            self.make_room()?;
            if let Some(prefix) = &close_first {
                self.close_under(prefix);
            }
            // A connection kept from an earlier run under the same key,
            // or a new one.
            let kept = pool_key.as_deref().and_then(|key| self.take(key));
            let (mut src, mut open) = match kept {
                Some(src) => (src, Some(open)),
                None => (open()?, None),
            };
            let mut writing = self.store.begin(&record.id)?;
            let asked = Instant::now();
            let mut streamed = src.stream(&record.sql, &mut Stoppable { sink: &mut writing, cancel: &cancel });
            // A kept connection may have gone stale while it waited. A
            // read that failed on one before it handed anything over is
            // asked again on a new connection. Never a statement that
            // might write: it may already have.
            // And only a failure that came back at once, which is how a
            // dead connection fails: a statement that ran to its timeout
            // is not run a second time.
            let nothing_yet = writing.result_schema().is_none() && writing.rows() == 0 && asked.elapsed() < STALE_WITHIN;
            if let (Err(_), Some(reopen), true, Kind::Semantic) = (&streamed, open.take(), nothing_yet, record.kind) {
                if !cancel.load(Ordering::SeqCst) {
                    drop(src);
                    src = reopen()?;
                    writing = self.store.begin(&record.id)?;
                    streamed = src.stream(&record.sql, &mut Stoppable { sink: &mut writing, cancel: &cancel });
                }
            }
            // A connection whose statement ran clean is good for another.
            if let (Ok(()), Some(key)) = (&streamed, &pool_key) {
                self.keep(key, src);
            }
            match (streamed, writing.failure()) {
                (_, Some(why)) => Err(self.or_disk_full(why)),
                (Err(e), None) => Err(Error::Source(e)),
                (Ok(()), None) => {
                    // Rows returned; for a statement that returned none,
                    // the rows it changed, when its transport said.
                    let rows = if writing.rows() == 0 { writing.affected().unwrap_or(0) } else { writing.rows() };
                    let dir = writing.finish().map_err(|e| self.or_disk_full(e))?;
                    // With a store configured the result is not one
                    // until it is there: other nodes read it from there,
                    // and whoever configured it meant it to be used.
                    if let Some(objects) = &self.objects {
                        match objects.upload(&record.id, &rows_file(&dir)) {
                            Ok(key) => record.object_store_path = Some(key),
                            Err(e) => {
                                let _ = std::fs::remove_dir_all(&dir);
                                return Err(Error::Objects(format!(
                                    "the statement ran and returned {rows} rows, but the result could not be written to the object store {} at {}: {e}. \
                                     ZSQL_OBJECT_STORE is set, so a result must be stored there; nothing was kept",
                                    objects.named(),
                                    objects.key_of(&record.id)
                                )));
                            }
                        }
                    }
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

/// A connection that is open and not in use.
struct Idle {
    key: String,
    source: Box<dyn Source>,
    since: Instant,
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

    fn affected(&mut self, rows: u64) -> zsql_datasource::Result<()> {
        self.sink.affected(rows)
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

/// The size of the filesystem a directory is on, in bytes.
#[cfg(unix)]
fn disk_bytes(dir: &Path) -> Option<u64> {
    use std::os::unix::ffi::OsStrExt;
    let path = std::ffi::CString::new(dir.as_os_str().as_bytes()).ok()?;
    let mut stat: libc::statvfs = unsafe { std::mem::zeroed() };
    // SAFETY: the path is a NUL-terminated string that lives through the
    // call, and the struct is this function's own.
    if unsafe { libc::statvfs(path.as_ptr(), &mut stat) } != 0 {
        return None;
    }
    Some(stat.f_blocks as u64 * stat.f_frsize as u64)
}

#[cfg(not(unix))]
fn disk_bytes(_dir: &Path) -> Option<u64> {
    None
}
