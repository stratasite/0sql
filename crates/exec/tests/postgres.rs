//! The records of runs in a real Postgres: the table is checked for,
//! a record goes in and comes back as it was, the constraints hold
//! across two nodes, and a run nobody is running is ended.
//!
//! One test, in order, because every step shares the one table. It
//! needs the database the Sources step brings up.

use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;
use zsql_exec::{
    run_cache_key, Error, How, Kind, Limits, Postgres, PostgresSettings, Record, Registry, Runs, Status, Submit, DEFAULT_ORG, RESTARTED,
};

fn settings() -> PostgresSettings {
    let var = |k: &str| std::env::var(k).unwrap_or_else(|_| panic!("{k} is not set"));
    PostgresSettings {
        url: format!("postgres://{}:{}/{}?sslmode=disable", var("ZSQL_TEST_PG_HOST"), var("ZSQL_TEST_PG_PORT"), var("ZSQL_TEST_PG_DB")),
        user: var("ZSQL_TEST_PG_USER"),
        password: zsql_datasource::Secret::new(var("ZSQL_TEST_PG_PASSWORD")),
    }
}

const LONG: &str = "WITH RECURSIVE c(x) AS (SELECT 1 UNION ALL SELECT x + 1 FROM c WHERE x < 4000000) SELECT x FROM c";

fn record(id: &str, key: Option<&str>, status: Status, hostname: &str, at: &str) -> Record {
    Record {
        id: id.into(),
        kind: Kind::Semantic,
        status,
        error: None,
        row_count: None,
        cache_key: key.map(str::to_string),
        dirty: false,
        tables: vec!["sales.orders".into(), "sales.items".into()],
        org_uid: DEFAULT_ORG.into(),
        project_uid: "shop".into(),
        branch: "main".into(),
        datasource_uid: "wh".into(),
        sql: "SELECT 'it''s' AS quoted".into(),
        spec: Some(serde_json::json!({"projections": [{"field": "status"}], "limit": 5})),
        expr: Some("status, limit 5".into()),
        row_limit: Some(5),
        hostname: hostname.into(),
        object_store_path: None,
        created_at: at.into(),
        queued_at: at.into(),
        started_at: None,
        finished_at: None,
        updated_at: at.into(),
        cancelled_at: None,
        expires_at: None,
    }
}

#[test]
#[ignore = "needs the Postgres the Sources step brings up"]
fn the_records_of_runs_in_postgres() {
    let admin = Postgres::connect(&settings()).unwrap();

    // No table, then one without its columns: said, and what to do.
    admin.batch_execute("DROP TABLE IF EXISTS zsql_results").unwrap();
    let none = Postgres::open(&settings()).err().expect("no table").to_string();
    assert!(none.contains("no table zsql_results") && none.contains("ops/zsql_results.sql"), "{none}");
    admin.batch_execute("CREATE TABLE zsql_results (id text PRIMARY KEY, type text, status text)").unwrap();
    let short = Postgres::open(&settings()).err().expect("columns missing").to_string();
    assert!(short.contains("missing the columns") && short.contains("expires_at") && !short.contains(" id,"), "{short}");
    admin.batch_execute("DROP TABLE zsql_results").unwrap();
    admin.batch_execute(include_str!("../../../ops/zsql_results.sql")).unwrap();
    let db = Postgres::open(&settings()).unwrap();
    // The reference can be applied again to a table that is there.
    admin.batch_execute(include_str!("../../../ops/zsql_results.sql")).unwrap();

    // A record goes in and comes back as it was.
    let key = run_cache_key(DEFAULT_ORG, "shop", "SELECT 1");
    let mut first = record("r-20261124-first", Some(&key), Status::Queued, "node-a", "2026-11-24T10:00:00Z");
    db.insert(&first).unwrap();
    assert_eq!(db.get("r-20261124-first").unwrap().unwrap(), first);
    assert!(db.get("r-none").unwrap().is_none());
    assert!(matches!(db.insert(&first), Err(Error::Duplicate(id)) if id == "r-20261124-first"));
    // One run of a question in flight, whichever node asks.
    let twin = record("r-20261124-twin", Some(&key), Status::Queued, "node-b", "2026-11-24T10:00:01Z");
    assert!(matches!(db.insert(&twin), Err(Error::InFlight(k)) if k == key));
    assert_eq!(db.in_flight(&key).unwrap().unwrap().id, "r-20261124-first");
    assert!(db.cached(&key).unwrap().is_none(), "nothing has finished");
    // A run that is not cached has no such limit.
    db.insert(&record("r-20261124-sql-1", None, Status::Queued, "node-a", "2026-11-24T10:00:00Z")).unwrap();
    db.insert(&record("r-20261124-sql-2", None, Status::Queued, "node-a", "2026-11-24T10:00:00Z")).unwrap();

    // It ends, and answers until it expires or is marked dirty.
    first.status = Status::Succeeded;
    first.row_count = Some(3);
    first.started_at = Some("2026-11-24T10:00:02Z".into());
    first.finished_at = Some("2026-11-24T10:00:03Z".into());
    first.updated_at = "2026-11-24T10:00:03Z".into();
    first.expires_at = Some("2999-01-01T00:00:00Z".into());
    db.save(&first).unwrap();
    assert_eq!(db.cached(&key).unwrap().unwrap(), first);
    assert!(db.in_flight(&key).unwrap().is_none());
    db.insert(&twin).unwrap();
    let mut twin = twin;
    twin.status = Status::Succeeded;
    twin.finished_at = Some("2026-11-24T10:05:00Z".into());
    twin.expires_at = Some("2999-01-01T00:00:00Z".into());
    db.save(&twin).unwrap();
    assert_eq!(db.cached(&key).unwrap().unwrap().id, "r-20261124-twin", "the newest answers");
    // Another system marks it dirty. A run saving itself afterwards
    // does not unmark it.
    admin.batch_execute("UPDATE zsql_results SET dirty = true WHERE tables && ARRAY['sales.orders'] AND id = 'r-20261124-twin'").unwrap();
    assert_eq!(db.cached(&key).unwrap().unwrap().id, "r-20261124-first", "a dirty result does not answer");
    db.save(&twin).unwrap();
    assert!(db.get("r-20261124-twin").unwrap().unwrap().dirty, "what an invalidation set, a save leaves");
    let mut gone = first.clone();
    gone.expires_at = Some("2020-01-01T00:00:00Z".into());
    db.save(&gone).unwrap();
    assert!(db.cached(&key).unwrap().is_none(), "nor does an expired one");

    // Invalidation by table, and what cleanup is told still sits in the store.
    let other = run_cache_key(DEFAULT_ORG, "shop", "SELECT 2");
    let mut kept = record("r-20261124-kept", Some(&other), Status::Succeeded, "node-a", "2026-11-24T11:00:00Z");
    kept.finished_at = Some("2026-11-24T11:00:01Z".into());
    kept.expires_at = Some("2999-01-01T00:00:00Z".into());
    kept.object_store_path = Some("cache/r-20261124-kept/rows.arrow".into());
    db.insert(&kept).unwrap();
    assert!(db.stale_objects(10).unwrap().iter().all(|r| r.id != "r-20261124-kept"), "it still answers");
    assert_eq!(db.mark_dirty(DEFAULT_ORG, Some("elsewhere"), "wh", &["sales.items".into()]).unwrap(), 0);
    assert_eq!(db.mark_dirty(DEFAULT_ORG, None, "other-wh", &["sales.items".into()]).unwrap(), 0);
    assert_eq!(db.mark_dirty(DEFAULT_ORG, None, "wh", &["nothing.read".into()]).unwrap(), 0);
    assert!(db.mark_dirty(DEFAULT_ORG, Some("shop"), "wh", &["sales.items".into()]).unwrap() >= 1);
    assert!(db.cached(&other).unwrap().is_none());
    let stale = db.stale_objects(10).unwrap();
    assert_eq!(stale.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["r-20261124-kept"], "dirty, and still in the store");
    let mut cleared = stale[0].clone();
    cleared.object_store_path = None;
    db.save(&cleared).unwrap();
    assert!(db.stale_objects(10).unwrap().is_empty());
    assert_eq!(db.prune().unwrap(), 0, "the table is the application's to trim");

    // Two nodes, one table, a disk each.
    admin.batch_execute("DELETE FROM zsql_results").unwrap();
    let root = std::env::temp_dir().join(format!("zsql-pg-runs-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let file = root.join("w.db");
    rusqlite::Connection::open(&file).unwrap().execute_batch("CREATE TABLE t (id INTEGER); INSERT INTO t VALUES (1), (2), (3);").unwrap();
    let node = |name: &str, beat: Duration| {
        let registry: Arc<dyn Registry> = Arc::new(Postgres::open(&settings()).unwrap());
        Runs::open(&root.join(name), registry, None, Limits { beat, ..Limits::of(name) }).unwrap()
    };
    let submit = |sql: &str| {
        let file: PathBuf = file.clone();
        Submit {
            kind: Kind::Semantic,
            org_uid: DEFAULT_ORG.into(),
            project_uid: "shop".into(),
            branch: "main".into(),
            datasource_uid: "wh".into(),
            sql: sql.into(),
            spec: None,
            expr: None,
            row_limit: None,
            tables: vec!["t".into()],
            cache_key: Some(run_cache_key(DEFAULT_ORG, "shop", sql)),
            cache_ttl: Duration::from_secs(3600),
            fresh: false,
            pool_key: None,
            open: Box::new(move || {
                let conn =
                    zsql_datasource::Connection { uid: "wh".into(), adapter: "sqlite".into(), file: Some(file), ..Default::default() };
                zsql_datasource::open(&conn, zsql_datasource::Secrets::default())
            }),
        }
    };
    let (a, b) = (node("node-a", Duration::from_secs(10)), node("node-b", Duration::from_secs(10)));
    let (running, how) = a.submit(submit(LONG)).unwrap();
    assert_eq!(how, How::Started);
    let (joined, how) = b.submit(submit(LONG)).unwrap();
    assert_eq!((how, joined.id.as_str()), (How::Attached, running.id.as_str()), "another node joins the run in flight");
    // The other node sees it end, by asking the table.
    let done = b.wait(&running.id, Duration::from_secs(1)).unwrap().unwrap();
    assert!(!done.status.is_final() || done.status == Status::Succeeded);
    let done = a.wait(&running.id, Duration::from_secs(60)).unwrap().unwrap();
    assert_eq!((done.status, done.hostname.as_str()), (Status::Succeeded, "node-a"), "{done:?}");
    assert_eq!(b.get(&running.id).unwrap().unwrap().status, Status::Succeeded);
    // Its rows are on the first node's disk. There it answers; on the
    // other, which cannot read them, the statement runs.
    assert_eq!(a.submit(submit(LONG)).unwrap().1, How::Hit);
    let (elsewhere, how) = b.submit(submit("SELECT id FROM t")).unwrap();
    assert_eq!(how, How::Started);
    b.wait(&elsewhere.id, Duration::from_secs(20)).unwrap();
    let (again, how) = a.submit(submit("SELECT id FROM t")).unwrap();
    assert_eq!(how, How::Started, "found in the table, and its rows are not here");
    a.wait(&again.id, Duration::from_secs(20)).unwrap();

    // A node that comes back ends what it had in progress; and a run
    // whose node never comes back is ended by one that is alive.
    db.insert(&record("r-20261124-mine", None, Status::Running, "node-c", "2026-11-24T10:00:00Z")).unwrap();
    db.insert(&record("r-20261124-orphan", None, Status::Running, "node-gone", "2026-11-24T10:00:00Z")).unwrap();
    let c = node("node-c", Duration::from_millis(50));
    let mine = c.get("r-20261124-mine").unwrap().unwrap();
    assert_eq!((mine.status, mine.error.as_deref()), (Status::Failed, Some(RESTARTED)));
    let deadline = std::time::Instant::now() + Duration::from_secs(10);
    loop {
        let orphan = c.get("r-20261124-orphan").unwrap().unwrap();
        if orphan.status == Status::Failed {
            assert_eq!(orphan.error.as_deref(), Some(RESTARTED));
            break;
        }
        assert!(std::time::Instant::now() < deadline, "the orphan was never ended: {orphan:?}");
        std::thread::sleep(Duration::from_millis(20));
    }
    // A run that is alive is not: its node marks it as it beats.
    let (alive, _) = c.submit(submit(LONG)).unwrap();
    std::thread::sleep(Duration::from_millis(600));
    let still = c.get(&alive.id).unwrap().unwrap();
    assert!(still.status != Status::Failed, "twelve beats on, a run its node is running is still its own: {still:?}");
    c.cancel(&alive.id).unwrap();
    let _ = std::fs::remove_dir_all(&root);
}
