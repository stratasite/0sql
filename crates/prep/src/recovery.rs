//! What survives a crash or a stop, and what the next start does about
//! it.
//!
//! - **Every job leaves a marker** while it runs: a snapshot being taken
//!   leaves `job.json` in its source's directory, a dataset being deleted
//!   one in the dataset's, and a run its record in the project's `.runs/`,
//!   saying `running`. A job that ends removes its marker, or records how
//!   the run ended.
//! - **A job a start finds running** is one the process died with. It may
//!   be what killed it, so it is not run again: it is marked
//!   *interrupted*, cause `crash`, saying so, and what it worked on keeps
//!   what it had.
//! - **A stop suspends what is still running** after a grace period, and
//!   the next start marks it interrupted, cause `stop`. Nothing is
//!   resumed: the application stopped the server, and it decides what to
//!   send again.
//! - **What a job leaves half made is swept** before anything is served:
//!   spill directories, staging directories, files written beside their
//!   targets.

use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// A job's marker, in the directory of what it works on.
pub const MARKER: &str = "job.json";

/// An interruption, beside a manifest until the next job on it.
pub const INTERRUPTED: &str = "interrupted.json";

/// What a job was doing.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum JobKind {
    /// A file taken as a snapshot.
    Upload,
    /// Rows posted, taken as a snapshot.
    Push,
    /// A query's rows, taken as a snapshot.
    Pull,
    /// A graph run for a dataset.
    Run,
    Delete,
}

impl JobKind {
    fn what(self) -> &'static str {
        match self {
            JobKind::Upload => "upload",
            JobKind::Push => "push of rows",
            JobKind::Pull => "pull",
            JobKind::Run => "run",
            JobKind::Delete => "delete",
        }
    }
}

/// Whether a job was running, or a stop suspended it.
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
    pub(crate) fn new(job: JobKind, started: &str, cause: Cause) -> Interrupted {
        let why = match cause {
            Cause::Crash => format!(
                "the server restarted and found this {} in flight. It may be what stopped the server, so it was not run again.",
                job.what()
            ),
            Cause::Stop => format!("the server was stopped while this {} ran, and nothing is resumed.", job.what()),
        };
        Interrupted { job, started: started.to_string(), found: crate::time::now(), cause, reason: format!("{why} Send it again.") }
    }
}

/// What a start found.
#[derive(Debug, Default)]
pub struct Recovered {
    /// Half-made things removed.
    pub swept: usize,
    /// What was marked interrupted, as `org/project/id`.
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
    let staged = |n: &str| n.starts_with(".new-") || n.starts_with(".deleting-") || n.starts_with(".restore-");
    for (_, _, root) in roots(data_dir) {
        for dir in [root.clone(), root.join(crate::store::SOURCES)] {
            for e in entries(&dir) {
                let n = name(&e);
                if staged(&n) {
                    gone(&e);
                } else if !n.starts_with('.') && e.is_dir() {
                    sweep_within(&e, &mut gone);
                }
            }
        }
        for e in entries(&root.join(crate::store::RUNS)) {
            if name(&e).contains(".tmp-") {
                gone(&e);
            }
        }
    }
    n
}

/// A source's or a dataset's staging directories and files written
/// beside their targets, at any depth.
fn sweep_within(dir: &Path, gone: &mut impl FnMut(&Path)) {
    for e in entries(dir) {
        let n = name(&e);
        let half = (n.starts_with('.') && n.contains(".staging-")) || n.contains(".tmp-");
        if half {
            gone(&e);
        } else if e.is_dir() {
            sweep_within(&e, gone);
        }
    }
}

pub(crate) fn entries(dir: &Path) -> Vec<PathBuf> {
    std::fs::read_dir(dir).map(|r| r.flatten().map(|e| e.path()).collect()).unwrap_or_default()
}

fn name(p: &Path) -> String {
    p.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default()
}
