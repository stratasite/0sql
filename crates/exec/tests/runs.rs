//! Runs, end to end on SQLite: a statement is recorded before it runs,
//! takes a seat or waits for one, writes its rows as they arrive, and
//! is found again by an identical question; and what happens when it is
//! cancelled, when it fails, when the queue is full, and when the
//! server it was running on stops.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use zsql_exec::{run_cache_key, How, Kind, Limits, Local, Record, Registry, Runs, Status, Submit, DEFAULT_ORG, RESTARTED};

struct Scratch(PathBuf);

impl Scratch {
    fn new(name: &str) -> Scratch {
        let dir = std::env::temp_dir().join(format!("zsql-runs-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        let db = rusqlite::Connection::open(dir.join("w.db")).unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT);").unwrap();
        let mut insert = db.prepare("INSERT INTO t VALUES (?1, ?2)").unwrap();
        for i in 0..5_000 {
            insert.execute((i, if i % 7 == 0 { None } else { Some(format!("row {i}")) })).unwrap();
        }
        Scratch(dir)
    }

    fn runs(&self, concurrent: usize, queue_max: usize) -> Runs {
        let registry: Arc<dyn Registry> = Arc::new(Local::open(&self.0.join("records")).unwrap());
        Runs::open(&self.0.join("cache"), registry, None, Limits { concurrent, queue_max, ..Limits::of("node-a") }).unwrap()
    }

    fn submit(&self, sql: &str) -> Submit {
        let file = self.0.join("w.db");
        Submit {
            kind: Kind::Semantic,
            org_uid: DEFAULT_ORG.into(),
            project_uid: "shop".into(),
            branch: "main".into(),
            datasource_uid: "wh".into(),
            sql: sql.into(),
            spec: None,
            expr: Some("id, name".into()),
            row_limit: None,
            tables: vec!["t".into()],
            cache_key: Some(run_cache_key(DEFAULT_ORG, "shop", sql)),
            cache_ttl: Duration::from_secs(3600),
            fresh: false,
            pool_key: None,
            close_first: None,
            open: Box::new(move || {
                let conn =
                    zsql_datasource::Connection { uid: "wh".into(), adapter: "sqlite".into(), file: Some(file), ..Default::default() };
                zsql_datasource::open(&conn, zsql_datasource::Secrets::default())
            }),
        }
    }
}

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// A statement that hands over rows for a good while.
const LONG: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4000000) SELECT x FROM c";

fn until(runs: &Runs, id: &str, status: Status) -> Record {
    let deadline = std::time::Instant::now() + Duration::from_secs(20);
    loop {
        let record = runs.get(id).unwrap().unwrap();
        if record.status == status {
            return record;
        }
        assert!(std::time::Instant::now() < deadline, "{id} never became {status:?}: {record:?}");
        std::thread::sleep(Duration::from_millis(5));
    }
}

#[test]
fn a_run_is_recorded_first_then_runs_and_its_rows_are_paged() {
    let scratch = Scratch::new("basic");
    let runs = scratch.runs(4, 10);
    let (queued, how) = runs.submit(scratch.submit("SELECT id, name FROM t ORDER BY id")).unwrap();
    assert_eq!(how, How::Started);
    assert!(queued.id.starts_with("r-") && queued.started_at.is_none(), "{queued:?}");
    assert_eq!((queued.org_uid.as_str(), queued.project_uid.as_str(), queued.branch.as_str()), ("default", "shop", "main"));
    assert_eq!((queued.hostname.as_str(), queued.tables.clone()), ("node-a", vec!["t".to_string()]));

    let done = runs.wait(&queued.id, Duration::from_secs(20)).unwrap().unwrap();
    assert_eq!(done.status, Status::Succeeded, "{done:?}");
    assert_eq!(done.row_count, Some(5_000));
    assert!(done.started_at.is_some() && done.finished_at.is_some() && done.expires_at > done.finished_at, "{done:?}");
    assert_eq!(runs.get(&queued.id).unwrap().unwrap(), done, "what is kept is what was returned");

    // A page from the middle, read without the rest.
    let page = zsql_exec::page(&runs.rows(&done).unwrap(), 4_998, 10).unwrap();
    assert_eq!(page.num_rows(), 2, "the last two rows, however many were asked for");
    assert_eq!(zsql_exec::json::rows(&page).unwrap(), vec![serde_json::json!(["4998", null]), serde_json::json!(["4999", "row 4999"])]);
    assert_eq!(zsql_exec::json::columns(&page.schema)[0], serde_json::json!({"name": "id", "type": "string", "arrow_type": "Utf8"}));
    assert_eq!(zsql_exec::page(&runs.rows(&done).unwrap(), 9_000, 10).unwrap().num_rows(), 0, "past the end is no rows");
    let mut seen = 0;
    zsql_exec::each_batch(&runs.rows(&done).unwrap(), |_, batch| {
        seen += batch.map(|b| b.num_rows()).unwrap_or(0);
        Ok(())
    })
    .unwrap();
    assert_eq!(seen, 5_000);

    // A wait that runs out says how things stand, and an id nobody has
    // is nobody's.
    assert!(runs.wait("r-none", Duration::from_millis(1)).unwrap().is_none());
}

#[test]
fn an_identical_question_attaches_then_hits_and_fresh_runs_again() {
    let scratch = Scratch::new("cache");
    let runs = scratch.runs(4, 10);
    let (first, how) = runs.submit(scratch.submit(LONG)).unwrap();
    assert_eq!(how, How::Started);
    let (second, how) = runs.submit(scratch.submit(LONG)).unwrap();
    assert_eq!((how, second.id.as_str()), (How::Attached, first.id.as_str()), "in flight, it is the same run");
    let (other, how) = runs.submit(scratch.submit("SELECT count(*) FROM t")).unwrap();
    assert_eq!(how, How::Started, "another statement is another run");
    assert_ne!(other.id, first.id);

    let done = runs.wait(&first.id, Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!(done.status, Status::Succeeded, "{done:?}");
    let (hit, how) = runs.submit(scratch.submit(LONG)).unwrap();
    assert_eq!((how, hit.id.as_str()), (How::Hit, first.id.as_str()), "finished, it answers from what is stored");

    // Fresh runs even so, and what it returns answers from then on.
    let (again, how) = runs.submit(Submit { fresh: true, ..scratch.submit("SELECT count(*) FROM t") }).unwrap();
    assert_eq!(how, How::Started);
    assert_ne!(again.id, other.id);
    runs.wait(&other.id, Duration::from_secs(20)).unwrap();
    let again = runs.wait(&again.id, Duration::from_secs(20)).unwrap().unwrap();
    assert_eq!(again.status, Status::Succeeded);
    std::thread::sleep(Duration::from_millis(1100));
    let (third, _) = runs.submit(Submit { fresh: true, ..scratch.submit("SELECT count(*) FROM t") }).unwrap();
    let third = runs.wait(&third.id, Duration::from_secs(20)).unwrap().unwrap();
    let (hit, how) = runs.submit(scratch.submit("SELECT count(*) FROM t")).unwrap();
    assert_eq!((how, hit.id), (How::Hit, third.id), "the newest result is the one that answers");

    // A run that is not cached is neither found nor attached to: a `sql`
    // run, or a datasource that keeps nothing.
    let uncached = || Submit { kind: Kind::Sql, cache_key: None, ..scratch.submit("SELECT count(*) FROM t") };
    let (a, how_a) = runs.submit(uncached()).unwrap();
    let (b, how_b) = runs.submit(uncached()).unwrap();
    assert_eq!((how_a, how_b), (How::Started, How::Started));
    assert_ne!(a.id, b.id, "two identical statements are two runs");
    assert!(a.cache_key.is_none());
}

#[test]
fn seats_a_queue_with_an_end_and_cancelling() {
    let scratch = Scratch::new("limits");
    let runs = scratch.runs(1, 1);
    let (running, _) = runs.submit(scratch.submit(LONG)).unwrap();
    until(&runs, &running.id, Status::Running);
    let (waiting, _) = runs.submit(scratch.submit("SELECT id FROM t")).unwrap();
    assert_eq!(runs.get(&waiting.id).unwrap().unwrap().status, Status::Queued, "one seat, and it is taken");
    let busy = runs.submit(scratch.submit("SELECT name FROM t")).expect_err("the queue has an end").to_string();
    assert!(busy.contains("busy"), "{busy}");

    // A run still waiting never starts.
    let cancelled = runs.cancel(&waiting.id).unwrap().unwrap();
    assert_eq!(cancelled.status, Status::Cancelled);
    assert!(cancelled.cancelled_at.is_some() && cancelled.started_at.is_none(), "{cancelled:?}");

    // A running one stops when it next hands over rows, and says so.
    runs.cancel(&running.id).unwrap().unwrap();
    let stopped = runs.wait(&running.id, Duration::from_secs(20)).unwrap().unwrap();
    assert_eq!(stopped.status, Status::Cancelled, "{stopped:?}");
    assert!(stopped.cancelled_at.is_some() && stopped.error.is_none());
    assert!(runs.rows(&stopped).is_err(), "what it had written is gone");
    assert!(runs.cancel("r-none").unwrap().is_none());

    // The seat is free again, and a cancelled run is not an answer.
    let (next, how) = runs.submit(scratch.submit(LONG)).unwrap();
    assert_eq!(how, How::Started, "a cancelled result does not answer");
    until(&runs, &next.id, Status::Running);
    runs.cancel(&next.id).unwrap();
}

#[test]
fn a_failure_is_recorded_and_a_restart_ends_what_was_in_progress() {
    let scratch = Scratch::new("fail");
    let runs = scratch.runs(2, 10);
    let (bad, _) = runs.submit(scratch.submit("SELECT nope FROM t")).unwrap();
    let bad = runs.wait(&bad.id, Duration::from_secs(20)).unwrap().unwrap();
    assert_eq!(bad.status, Status::Failed);
    assert!(bad.error.as_deref().unwrap_or_default().contains("nope"), "{bad:?}");
    assert!(bad.expires_at.is_none() && bad.row_count.is_none());
    let (unopened, _) = runs
        .submit(Submit {
            open: Box::new(|| Err(zsql_datasource::Error::Connect("wh".into(), "no route to host".into()))),
            ..scratch.submit("SELECT 1")
        })
        .unwrap();
    let unopened = runs.wait(&unopened.id, Duration::from_secs(20)).unwrap().unwrap();
    assert!(unopened.error.as_deref().unwrap_or_default().contains("no route to host"), "{unopened:?}");
    let (failed_again, how) = runs.submit(scratch.submit("SELECT nope FROM t")).unwrap();
    assert_eq!(how, How::Started, "a failure does not answer the next question");
    runs.wait(&failed_again.id, Duration::from_secs(20)).unwrap();

    let (good, _) = runs.submit(scratch.submit("SELECT id FROM t WHERE id < 3")).unwrap();
    let good = runs.wait(&good.id, Duration::from_secs(20)).unwrap().unwrap();
    let (long, _) = runs.submit(scratch.submit(LONG)).unwrap();
    until(&runs, &long.id, Status::Running);

    // The server stops with a run in flight, and comes back. What was
    // running is failed and says why; what had finished still answers.
    // (The thread of the first server is still reading; a real restart
    // has no such thread, so its last word is not looked at.)
    let reopened = scratch.runs(2, 10);
    let lost = reopened.get(&long.id).unwrap().unwrap();
    assert_eq!((lost.status, lost.error.as_deref()), (Status::Failed, Some(RESTARTED)));
    let (hit, how) = reopened.submit(scratch.submit("SELECT id FROM t WHERE id < 3")).unwrap();
    assert_eq!((how, hit.id.as_str()), (How::Hit, good.id.as_str()));
    assert_eq!(zsql_exec::page(&reopened.rows(&hit).unwrap(), 0, 10).unwrap().num_rows(), 3);
    runs.cancel(&long.id).unwrap();
}

/// A connection that counts how often one was opened, and can be told
/// to fail the next statement on whichever of them is asked.
struct Counted {
    fail_next: Arc<std::sync::atomic::AtomicBool>,
}

impl zsql_datasource::Source for Counted {
    fn query(&mut self, _sql: &str, _max_rows: usize) -> zsql_datasource::Result<zsql_datasource::Rows> {
        if self.fail_next.swap(false, std::sync::atomic::Ordering::SeqCst) {
            return Err(zsql_datasource::Error::Query("wh".into(), "the connection was closed by the server".into()));
        }
        Ok(zsql_datasource::Rows { columns: vec!["n".into()], rows: vec![vec![Some("1".into())]] })
    }

    fn adapter(&self) -> &str {
        "counted"
    }
}

#[test]
fn a_connection_is_kept_for_the_next_run_under_its_key() {
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
    let scratch = Scratch::new("pool");
    let registry: Arc<dyn Registry> = Arc::new(Local::open(&scratch.0.join("records")).unwrap());
    let limits = Limits { beat: Duration::from_millis(20), pool_idle: Duration::from_millis(400), pool_max: 2, ..Limits::of("node-a") };
    let runs = Runs::open(&scratch.0.join("cache"), registry, None, limits).unwrap();
    let (opened, fail_next) = (Arc::new(AtomicUsize::new(0)), Arc::new(AtomicBool::new(false)));
    let submit = |kind: Kind, key: Option<&str>, n: usize| {
        let (opened, fail_next) = (opened.clone(), fail_next.clone());
        Submit {
            kind,
            cache_key: None,
            pool_key: key.map(str::to_string),
            close_first: None,
            open: Box::new(move || {
                opened.fetch_add(1, Ordering::SeqCst);
                Ok(Box::new(Counted { fail_next }) as Box<dyn zsql_datasource::Source>)
            }),
            ..scratch.submit(&format!("SELECT {n}"))
        }
    };
    let run = |kind: Kind, key: Option<&str>, n: usize| {
        let (record, _) = runs.submit(submit(kind, key, n)).unwrap();
        runs.wait(&record.id, Duration::from_secs(20)).unwrap().unwrap()
    };

    // Three runs under one key open one connection.
    for n in 0..3 {
        assert_eq!(run(Kind::Semantic, Some("wh/ada"), n).status, Status::Succeeded);
    }
    assert_eq!((opened.load(Ordering::SeqCst), runs.idle_connections()), (1, 1));
    // Another credential is another connection, and a run with no key
    // keeps none.
    run(Kind::Semantic, Some("wh/bo"), 3);
    run(Kind::Semantic, None, 4);
    assert_eq!((opened.load(Ordering::SeqCst), runs.idle_connections()), (3, 2));
    // One more than the pool holds, and the one unused longest is closed.
    run(Kind::Semantic, Some("wh/cy"), 5);
    assert_eq!(runs.idle_connections(), 2);
    run(Kind::Semantic, Some("wh/ada"), 6);
    assert_eq!(opened.load(Ordering::SeqCst), 5, "ada's had been closed to make room, so a new one was opened");

    // A kept connection that has gone stale: a read is asked again on a
    // new one, and nobody is told.
    fail_next.store(true, Ordering::SeqCst);
    let retried = run(Kind::Semantic, Some("wh/ada"), 7);
    assert_eq!((retried.status, opened.load(Ordering::SeqCst)), (Status::Succeeded, 6), "{retried:?}");
    // A statement that may write is not: it may already have.
    fail_next.store(true, Ordering::SeqCst);
    let not_retried = run(Kind::Sql, Some("wh/ada"), 8);
    assert_eq!((not_retried.status, opened.load(Ordering::SeqCst)), (Status::Failed, 6), "{not_retried:?}");
    assert!(not_retried.error.unwrap().contains("closed by the server"));
    // A connection whose statement failed is not kept.
    let before = runs.idle_connections();
    fail_next.store(true, Ordering::SeqCst);
    assert_eq!(run(Kind::Sql, Some("wh/new"), 9).status, Status::Failed);
    assert_eq!(runs.idle_connections(), before);

    // Left unused, a connection is closed.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while runs.idle_connections() > 0 {
        assert!(std::time::Instant::now() < deadline, "idle connections were never closed");
        std::thread::sleep(Duration::from_millis(20));
    }
}
