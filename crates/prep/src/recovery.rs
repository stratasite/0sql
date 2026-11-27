//! What survives a crash or a stop, and what the next start does about
//! it.
//!
//! - **Every job leaves a marker** in its dataset's directory while it
//!   runs (`job.json`), and a query a dataset waits on after its request
//!   was answered leaves one in the project's `.jobs/`. A job that ends
//!   removes its marker.
//! - **A marker found at start that still says `running`** is a job the
//!   process died with. It may be what killed it, so it is not run
//!   again: the dataset is marked *interrupted*, saying so, and keeps the
//!   version it had.
//! - **A stop suspends what is still running** after a grace period, and
//!   the next start marks it interrupted, cause `stop`. Nothing is
//!   resumed: the application stopped the server, and it decides what to
//!   send again.
//! - **What a job leaves half made is swept** before anything is served:
//!   spill directories, staging directories, files written beside their
//!   targets.

use crate::source::Query;
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A job's marker, in its dataset's directory.
pub const MARKER: &str = "job.json";

/// A dataset's interruption, beside its manifest until its next job.
pub const INTERRUPTED: &str = "interrupted.json";

/// Where a project keeps the queries its datasets wait on.
pub const QUERIES: &str = ".jobs";

/// How long the record of a query that never made its dataset is kept.
const TOMBSTONE_SECS: u64 = 7 * 24 * 3600;

/// What a job was doing.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// A new file, as the next snapshot.
    Replace,
    /// A query run again, as the next snapshot.
    Refresh,
    /// A query that makes a new dataset.
    Create,
    Change,
    Confirm,
    Edit,
    Delete,
    /// Run again because a dataset it reads changed.
    Update,
    /// This dataset changed; the datasets that read it are being run
    /// again. Its own change is made.
    Cascade,
}

impl JobKind {
    fn what(self) -> &'static str {
        match self {
            JobKind::Replace => "upload of a new file",
            JobKind::Refresh => "refresh",
            JobKind::Create => "query making this dataset",
            JobKind::Change => "recipe change",
            JobKind::Confirm => "confirm",
            JobKind::Edit => "edit",
            JobKind::Delete => "delete",
            JobKind::Update => "run, for a change in a dataset it reads,",
            JobKind::Cascade => "update of the datasets that read it",
        }
    }

    /// What to do about it, said after why it stopped.
    fn then(self) -> &'static str {
        match self {
            JobKind::Refresh => "Refresh it again.",
            JobKind::Create => "Make it again.",
            JobKind::Update => "Change or refresh it, or what it reads, to run it.",
            JobKind::Cascade => "Its own change is made; the datasets that read it were not run again: change or refresh it to run them.",
            _ => "Send it again.",
        }
    }
}

/// Whether a marker's job was running, or a stop suspended it.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Running,
    Suspended,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub(crate) struct Marker {
    pub job: JobKind,
    pub started: String,
    pub state: State,
}

/// What stopped a job.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Cause {
    /// The process died with it running.
    Crash,
    /// The server was stopped while it ran.
    Stop,
}

/// A job a start found in flight, and did not run again.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Interrupted {
    pub job: JobKind,
    /// When the job started.
    pub started: String,
    /// When the start found it.
    pub found: String,
    pub cause: Cause,
    pub reason: String,
}

impl Interrupted {
    pub(crate) fn new(job: JobKind, started: &str, cause: Cause, extra: Option<&str>) -> Interrupted {
        let why = match cause {
            Cause::Crash => format!(
                "the server restarted and found this {} in flight. It may be what stopped the server, so it was not run again.",
                job.what()
            ),
            Cause::Stop => format!("the server was stopped while this {} ran, and nothing is resumed.", job.what()),
        };
        let extra = extra.map(|e| format!(" {e}")).unwrap_or_default();
        Interrupted { job, started: started.to_string(), found: crate::time::now(), cause, reason: format!("{why}{extra} {}", job.then()) }
    }
}

/// What a query's rows become.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum Then {
    Create { name: Option<String> },
    Refresh,
}

/// A query a dataset waits on after its request was answered, as kept.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct QueryJob {
    pub id: String,
    /// The run, by its id under `/results`.
    pub result: String,
    pub since: String,
    pub then: Then,
    /// What it runs; never its context or a connection, which are the
    /// request's.
    pub query: Query,
    pub state: State,
    /// Set when a start found it in flight and did not run it again: the
    /// record of a dataset that was never made.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub interrupted: Option<Interrupted>,
}

impl QueryJob {
    fn kind(&self) -> JobKind {
        match self.then {
            Then::Create { .. } => JobKind::Create,
            Then::Refresh => JobKind::Refresh,
        }
    }
}

/// What a start found.
#[derive(Debug, Default)]
pub struct Recovered {
    /// Half-made things removed.
    pub swept: usize,
    /// The datasets marked interrupted, as `org/project/id`.
    pub interrupted: Vec<String>,
}

/// Every project directory of datasets under `data_dir`, with its org
/// and project.
pub(crate) fn roots(data_dir: &Path) -> Vec<(String, String, PathBuf)> {
    let mut out = Vec::new();
    let Ok(orgs) = std::fs::read_dir(data_dir) else { return out };
    for org in orgs.flatten() {
        let o = org.file_name().to_string_lossy().to_string();
        if o.starts_with('.') || !org.path().is_dir() {
            continue;
        }
        let Ok(projects) = std::fs::read_dir(org.path()) else { continue };
        for project in projects.flatten() {
            let p = project.file_name().to_string_lossy().to_string();
            let root = project.path().join(crate::store::DIR);
            if !p.starts_with('.') && root.is_dir() {
                out.push((o.clone(), p, root));
            }
        }
    }
    out.sort();
    out
}

/// Removes what a job leaves half made when it does not finish. Run
/// before anything is served, by the one process that owns the data
/// directory (the server holds its lock).
pub(crate) fn sweep(data_dir: &Path) -> usize {
    let mut n = 0;
    let mut gone = |p: &Path| {
        let r = if p.is_dir() { std::fs::remove_dir_all(p) } else { std::fs::remove_file(p) };
        match r {
            Ok(()) => n += 1,
            Err(e) => tracing::warn!("{} was left by a job that did not finish and could not be removed: {e}", p.display()),
        }
    };
    // Spill directories: no session outlives the process.
    for e in entries(&data_dir.join(".cache").join("prep")) {
        gone(&e);
    }
    // A blend kept half.
    for (o, p, _) in roots(data_dir) {
        for e in entries(&data_dir.join(".cache").join("blends").join(&o).join(&p)) {
            if name(&e).starts_with('.') {
                gone(&e);
            }
        }
    }
    for (_, _, root) in roots(data_dir) {
        for e in entries(&root) {
            let n = name(&e);
            if n.starts_with(".new-") || n.starts_with(".deleting-") || n.starts_with(".restore-") {
                gone(&e);
            } else if !n.starts_with('.') && e.is_dir() {
                sweep_dataset(&e, &mut gone);
            }
        }
    }
    n
}

/// A dataset's staging directories and files written beside their
/// targets, at any depth.
fn sweep_dataset(dir: &Path, gone: &mut impl FnMut(&Path)) {
    for e in entries(dir) {
        let n = name(&e);
        let half = (n.starts_with(".v") && n.contains(".staging-")) || n.contains(".tmp-") || n.starts_with(".edits.log.");
        if half {
            gone(&e);
        } else if e.is_dir() {
            sweep_dataset(&e, gone);
        }
    }
}

fn entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}

/// Whether the file of a query's tombstone is past keeping.
pub(crate) fn expired(path: &Path) -> bool {
    let age = std::fs::metadata(path).and_then(|m| m.modified()).ok().and_then(|t| t.elapsed().ok());
    age.is_some_and(|a| a.as_secs() > TOMBSTONE_SECS)
}

pub(crate) fn query_kind(q: &QueryJob) -> JobKind {
    q.kind()
}
