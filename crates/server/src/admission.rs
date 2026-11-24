//! Admission control: how much of each kind of work may be in the
//! server at once, and how long a request may run.
//!
//! This is what keeps the process alive, the way `max_connections`
//! keeps a database alive. It is not rate limiting: who may ask, and
//! how often, is for whatever stands in front of the server to decide.
//! Past a cap a request waits a moment and is then told to come back,
//! rather than queueing without bound. See `docs/admission.md`.

use std::sync::Arc;
use std::time::Duration;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};

/// What a request costs, which decides its concurrency cap and its
/// deadline. One deploy is not one `sql`.
#[derive(Clone, Copy, PartialEq, Eq, Hash, Debug)]
pub(crate) enum Class {
    /// sql, explain, tables, values, summaries: microseconds each.
    Plan,
    /// explore and the fields search: a walk or a scoring pass over
    /// every visible field, milliseconds, and retriggered on keystrokes.
    Explore,
    /// deploy, validate, test: seconds of CPU.
    Heavy,
    /// a probe of a datasource: a connection to a warehouse and a read
    /// of its catalog. No CPU to speak of, and seconds of waiting on
    /// someone else's machine, so it has seats of its own: a slow
    /// warehouse fills them and leaves planning and deploys alone.
    Probe,
    /// a run, or a question about one: a request that mostly waits for
    /// a statement another thread is running. How many statements run
    /// and how many wait is the runs' own business; a seat here is
    /// only a request in the server.
    Run,
    /// liveness and the root: no admission.
    Exempt,
}

impl Class {
    pub fn name(self) -> &'static str {
        match self {
            Class::Plan => "plan",
            Class::Explore => "explore",
            Class::Heavy => "heavy",
            Class::Probe => "probe",
            Class::Run => "run",
            Class::Exempt => "exempt",
        }
    }
}

/// A request's class, from its method and path alone.
pub(crate) fn classify(path: &str) -> Class {
    let p: Vec<&str> = path.split('/').filter(|s| !s.is_empty()).collect();
    match p.as_slice() {
        [] | ["healthz"] => Class::Exempt,
        ["projects", _, "branches", _, "deploy" | "validate" | "test"] => Class::Heavy,
        ["projects", _, "branches", _, "explore" | "fields"] => Class::Explore,
        // Below a datasource is the warehouse; the list of datasources
        // itself is the model's, and cheap.
        ["projects", _, "branches", _, "datasources", _, ..] => Class::Probe,
        ["projects", _, "branches", _, "run" | "sql-exec"] | ["results", _] | ["cache", "invalidate"] => Class::Run,
        // One HTTP request to /mcp carries one tool call, admitted here
        // as a plan; the explore-class tools take an explore seat too,
        // in mcp.rs. Everything else, a path that matches no route
        // included, is cheap.
        _ => Class::Plan,
    }
}

/// How long a class waits for a permit before the server says it is busy.
fn patience(class: Class) -> Duration {
    match class {
        Class::Heavy => Duration::from_secs(5),
        _ => Duration::from_secs(1),
    }
}

/// How long a request may run before it is abandoned.
///
/// Heavy work gets ninety seconds rather than the couple of minutes a
/// deploy might want, because Cloudflare gives an origin a hundred
/// seconds and then answers 524 itself. A 524 carries no retry hint, so
/// the CLI cannot act on it and the caller is left guessing about a
/// deploy that may well have landed. Better that the server always gets
/// to give its own answer first.
pub(crate) fn deadline(class: Class) -> Duration {
    match class {
        // A probe waits on a warehouse, which can take its time to wake.
        Class::Heavy | Class::Probe | Class::Run => Duration::from_secs(90),
        _ => Duration::from_secs(30),
    }
}

/// How many of each class may be in the server at once.
#[derive(Debug, Clone)]
pub struct AdmissionCounts {
    pub plan: usize,
    pub explore: usize,
    pub heavy: usize,
    pub probe: usize,
    /// Requests for a run in the server at once: every statement that
    /// may run or wait, and as many again asking after them.
    pub run: usize,
}

impl Default for AdmissionCounts {
    /// Sized from the cores: planning is quick and parallel, heavy work
    /// is neither.
    fn default() -> Self {
        let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(4);
        AdmissionCounts { plan: cores * 4, explore: cores, heavy: 2, probe: cores * 2, run: 2 * (100 + 5_000) }
    }
}

pub(crate) struct Admission {
    plan: Arc<Semaphore>,
    explore: Arc<Semaphore>,
    heavy: Arc<Semaphore>,
    probe: Arc<Semaphore>,
    run: Arc<Semaphore>,
}

impl Admission {
    pub fn new(counts: &AdmissionCounts) -> Admission {
        Admission {
            plan: Arc::new(Semaphore::new(counts.plan)),
            explore: Arc::new(Semaphore::new(counts.explore)),
            heavy: Arc::new(Semaphore::new(counts.heavy)),
            probe: Arc::new(Semaphore::new(counts.probe)),
            run: Arc::new(Semaphore::new(counts.run)),
        }
    }

    fn semaphore(&self, class: Class) -> Option<&Arc<Semaphore>> {
        Some(match class {
            Class::Plan => &self.plan,
            Class::Explore => &self.explore,
            Class::Heavy => &self.heavy,
            Class::Probe => &self.probe,
            Class::Run => &self.run,
            Class::Exempt => return None,
        })
    }

    /// Waits a bounded moment for room to run. `None` means the class has
    /// no cap; `Err` means the server is busy.
    pub async fn admit(&self, class: Class) -> Result<Option<OwnedSemaphorePermit>, u64> {
        let Some(sem) = self.semaphore(class) else { return Ok(None) };
        let wait = patience(class);
        match tokio::time::timeout(wait, sem.clone().acquire_owned()).await {
            Ok(Ok(permit)) => Ok(Some(permit)),
            // The semaphore is never closed; a closed one would mean the
            // server is shutting down, which is also "come back later".
            Ok(Err(_)) | Err(_) => Err(wait.as_secs().max(1)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Instant;

    #[test]
    fn routes_land_in_the_right_class() {
        use Class::*;
        for (p, want) in [
            ("/projects/a/branches/main/sql", Plan),
            ("/projects/a/branches/main/explain", Plan),
            ("/projects/a/branches/main/explore", Explore),
            ("/projects/a/branches/main/fields", Explore),
            ("/projects/a/branches/main/deploy", Heavy),
            ("/projects/a/branches/main/validate", Heavy),
            ("/projects/a/branches/main/test", Heavy),
            ("/projects/a/branches/main/values/dim", Plan),
            ("/projects/a/branches/main/datasources", Plan),
            ("/projects/a/branches/main/datasources/dw/tables", Probe),
            ("/projects/a/branches/main/datasources/dw/tables/dse.cs_contact_f", Probe),
            ("/projects/a/branches/main", Plan),
            ("/projects", Plan),
            ("/mcp", Plan),
            ("/no/such/route", Plan),
            ("/healthz", Exempt),
            ("/", Exempt),
        ] {
            assert_eq!(classify(p), want, "{p}");
        }
    }

    #[tokio::test]
    async fn admission_refuses_when_there_is_no_room() {
        let a = Admission::new(&AdmissionCounts { heavy: 1, ..AdmissionCounts::default() });
        let held = a.admit(Class::Heavy).await.expect("the first one gets in");
        // The second waits its patience out and is told to come back.
        let t0 = Instant::now();
        let refused = a.admit(Class::Heavy).await;
        assert!(refused.is_err());
        assert!(t0.elapsed() >= Duration::from_secs(5));
        drop(held);
        assert!(a.admit(Class::Heavy).await.is_ok(), "room again once the first is done");
    }

    #[tokio::test]
    async fn exempt_requests_need_no_permit() {
        let a = Admission::new(&AdmissionCounts::default());
        assert!(a.admit(Class::Exempt).await.unwrap().is_none());
    }
}
