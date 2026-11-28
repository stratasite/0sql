//! Results in an object store, with a directory standing in for one:
//! a result is uploaded when it is made, read by a node that did not
//! make it, cleared from the store and the disk when it stops
//! answering, and a store that cannot be written to fails the run.

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::time::Duration;
use zsql_exec::{run_cache_key, How, Kind, Limits, Local, Objects, Record, Registry, Runs, Status, Submit, DEFAULT_ORG};

struct Cluster {
    root: PathBuf,
    registry: Arc<dyn Registry>,
}

impl Cluster {
    fn new(name: &str) -> Cluster {
        let root = std::env::temp_dir().join(format!("zsql-objects-{name}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&root);
        std::fs::create_dir_all(root.join("store")).unwrap();
        let db = rusqlite::Connection::open(root.join("w.db")).unwrap();
        db.execute_batch("CREATE TABLE t (id INTEGER PRIMARY KEY, name TEXT); CREATE TABLE u (id INTEGER);").unwrap();
        let mut insert = db.prepare("INSERT INTO t VALUES (?1, ?2)").unwrap();
        for i in 0..2_000 {
            insert.execute((i, format!("row {i}"))).unwrap();
        }
        // One set of records for every node, as a shared database is.
        let registry: Arc<dyn Registry> = Arc::new(Local::open(&root.join("records")).unwrap());
        Cluster { root, registry }
    }

    fn store(&self) -> Arc<Objects> {
        Arc::new(Objects::open(&format!("file://{}", self.root.join("store").display()), &[]).unwrap())
    }

    fn node(&self, name: &str, objects: Option<Arc<Objects>>, limits: impl FnOnce(Limits) -> Limits) -> Runs {
        Runs::open(&self.root.join(name), self.registry.clone(), objects, limits(Limits::of(name))).unwrap()
    }

    fn submit(&self, sql: &str, tables: &[&str], ttl: u64) -> Submit {
        let file = self.root.join("w.db");
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
            tables: tables.iter().map(|t| t.to_string()).collect(),
            cache_key: Some(run_cache_key(DEFAULT_ORG, "shop", sql)),
            cache_ttl: zsql_datasource::CacheTtl::For(Duration::from_secs(ttl)),
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

    /// Where the store keeps a result, on the disk that stands in for it.
    fn in_store(&self, record: &Record) -> PathBuf {
        Path::new("/").join(record.object_store_path.as_deref().expect("a path in the store"))
    }
}

impl Drop for Cluster {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.root);
    }
}

fn ran(runs: &Runs, submit: Submit) -> Record {
    let (record, how) = runs.submit(submit).unwrap();
    assert_eq!(how, How::Started);
    runs.wait(&record.id, Duration::from_secs(30)).unwrap().unwrap()
}

#[test]
fn a_result_goes_to_the_store_and_another_node_reads_it_from_there() {
    let cluster = Cluster::new("shared");
    let (a, b) = (cluster.node("node-a", Some(cluster.store()), |l| l), cluster.node("node-b", Some(cluster.store()), |l| l));
    let made = ran(&a, cluster.submit("SELECT id, name FROM t ORDER BY id", &["t"], 3600));
    assert_eq!(made.status, Status::Succeeded, "{made:?}");
    let key = made.object_store_path.clone().expect("it says where it is in the store");
    assert!(key.ends_with(&format!("cache/{}/rows.arrow", made.id)), "under cache/, by its id: {key}");
    assert!(cluster.in_store(&made).exists());

    // The other node finds it, though it holds none of its rows, and
    // fetches them when they are asked for.
    let (hit, how) = b.submit(cluster.submit("SELECT id, name FROM t ORDER BY id", &["t"], 3600)).unwrap();
    assert_eq!((how, hit.id.as_str()), (How::Hit, made.id.as_str()));
    assert!(!cluster.root.join("node-b").join("r-").join(&made.id).exists(), "nothing is fetched until it is read");
    // Two readers ask at once, before either has it: both get the whole file.
    let fetched = std::thread::scope(|scope| {
        let readers: Vec<_> = (0..4).map(|_| scope.spawn(|| b.rows(&hit))).collect();
        let paths: Vec<PathBuf> = readers.into_iter().map(|r| r.join().unwrap().expect("each reader gets the rows")).collect();
        paths[0].clone()
    });
    assert_eq!(zsql_exec::page(&fetched, 0, 5_000).unwrap().num_rows(), 2_000, "and it is whole");
    let leftovers = std::fs::read_dir(fetched.parent().unwrap())
        .unwrap()
        .flatten()
        .filter(|e| e.file_name().to_string_lossy().contains("part-"))
        .count();
    assert_eq!(leftovers, 0, "no half-fetched file is left beside it");
    assert!(fetched.starts_with(cluster.root.join("node-b")));
    let page = zsql_exec::page(&fetched, 1_998, 5).unwrap();
    assert_eq!(
        zsql_exec::json::rows(&page).unwrap(),
        vec![serde_json::json!(["1998", "row 1998"]), serde_json::json!(["1999", "row 1999"])]
    );
    // A later read on that node is from its own disk.
    std::fs::rename(cluster.in_store(&made), cluster.root.join("moved-away")).unwrap();
    assert_eq!(zsql_exec::page(&b.rows(&hit).unwrap(), 0, 3).unwrap().num_rows(), 3);
    std::fs::rename(cluster.root.join("moved-away"), cluster.in_store(&made)).unwrap();

    // A table it read changes. It stops answering, on every node, and
    // cleanup removes it from the store and from each node's own disk.
    assert_eq!(a.invalidate(DEFAULT_ORG, Some("other-project"), "wh", &["t".into()]).unwrap(), 0, "another project's are another's");
    assert_eq!(a.invalidate(DEFAULT_ORG, None, "wh", &["u".into()]).unwrap(), 0, "it did not read that table");
    assert_eq!(a.invalidate(DEFAULT_ORG, Some("shop"), "wh", &["u".into(), "t".into()]).unwrap(), 1);
    assert_eq!(a.invalidate(DEFAULT_ORG, Some("shop"), "wh", &["t".into()]).unwrap(), 0, "marked once");
    assert!(b.get(&made.id).unwrap().unwrap().dirty);
    let (again, how) = b.submit(cluster.submit("SELECT id, name FROM t ORDER BY id", &["t"], 3600)).unwrap();
    assert_eq!(how, How::Started, "a dirty result does not answer");
    let again = b.wait(&again.id, Duration::from_secs(30)).unwrap().unwrap();
    assert_eq!((again.status, again.dirty), (Status::Succeeded, false));
    a.sweep().unwrap();
    assert!(!cluster.in_store(&made).exists(), "gone from the store");
    assert!(a.get(&made.id).unwrap().unwrap().object_store_path.is_none());
    assert!(a.rows(&made).is_err(), "and from the disk of the node that made it");
    b.sweep().unwrap();
    assert!(!fetched.exists(), "and each node clears its own copy");
    assert!(cluster.in_store(&again).exists() && b.rows(&again).is_ok(), "what still answers is left alone");

    // A result that has had its time goes the same way.
    let brief = ran(&a, cluster.submit("SELECT count(*) FROM t", &["t"], 1));
    assert!(cluster.in_store(&brief).exists());
    std::thread::sleep(Duration::from_millis(2_100));
    a.sweep().unwrap();
    assert!(!cluster.in_store(&brief).exists() && a.rows(&brief).is_err());
}

#[test]
fn a_store_that_cannot_be_written_to_fails_the_run_and_says_so() {
    let cluster = Cluster::new("unwritable");
    // A file where the store's directory should be: nothing can go under it.
    std::fs::write(cluster.root.join("not-a-directory"), b"").unwrap();
    let broken = Arc::new(Objects::open(&format!("file://{}", cluster.root.join("not-a-directory").join("store").display()), &[]).unwrap());
    let a = cluster.node("node-a", Some(broken), |l| l);
    let failed = ran(&a, cluster.submit("SELECT id FROM t", &["t"], 3600));
    assert_eq!(failed.status, Status::Failed);
    let why = failed.error.clone().unwrap();
    assert!(why.contains("the statement ran and returned 2000 rows"), "{why}");
    assert!(why.contains("could not be written to the object store") && why.contains("not-a-directory"), "{why}");
    assert!(why.contains("ZSQL_OBJECT_STORE is set") && why.contains("nothing was kept"), "{why}");
    assert!(failed.object_store_path.is_none() && a.rows(&failed).is_err(), "and nothing was kept: {failed:?}");
    let (next, how) = a.submit(cluster.submit("SELECT id FROM t", &["t"], 3600)).unwrap();
    assert_eq!(how, How::Started, "a failure does not answer");
    a.wait(&next.id, Duration::from_secs(30)).unwrap();
}

#[test]
fn a_full_disk_evicts_copies_when_there_is_a_store_and_refuses_when_there_is_not() {
    // With a store, what is on the disk is a copy: the least recently
    // read goes to make room, and is fetched again when it is asked for.
    let cluster = Cluster::new("full");
    let a = cluster.node("node-a", Some(cluster.store()), |l| Limits { local_max_bytes: Some(1), ..l });
    let first = ran(&a, cluster.submit("SELECT id FROM t WHERE id < 100", &["t"], 3600));
    let second = ran(&a, cluster.submit("SELECT id FROM t WHERE id < 200", &["t"], 3600));
    assert_eq!((first.status, second.status), (Status::Succeeded, Status::Succeeded));
    let local = |r: &Record| cluster.root.join("node-a").join("r-").join(&r.id).join("rows.arrow");
    assert!(!local(&first).exists() && local(&second).exists(), "the older copy went to make room for the newer");
    assert_eq!(zsql_exec::page(&a.rows(&first).unwrap(), 0, 1_000).unwrap().num_rows(), 100, "and comes back from the store");

    // Without one, the result on the disk is the only one there is.
    let alone = Cluster::new("full-alone");
    let b = alone.node("node-b", None, |l| Limits { local_max_bytes: Some(1), ..l });
    let kept = ran(&b, alone.submit("SELECT id FROM t WHERE id < 100", &["t"], 3600));
    assert_eq!(kept.status, Status::Succeeded);
    let refused = ran(&b, alone.submit("SELECT id FROM t WHERE id < 200", &["t"], 3600));
    assert_eq!(refused.status, Status::Failed);
    let why = refused.error.unwrap();
    assert!(
        why.contains("disk is full of results") && why.contains("ZSQL_OBJECT_STORE") && why.contains("ZSQL_LOCAL_CACHE_MAX_PERCENT"),
        "{why}"
    );
    assert!(b.rows(&kept).is_ok(), "what was there still answers");
    // Once that one has had its time, there is room again.
    b.invalidate(DEFAULT_ORG, None, "wh", &["t".into()]).unwrap();
    let after = ran(&b, alone.submit("SELECT id FROM t WHERE id < 300", &["t"], 3600));
    assert_eq!(after.status, Status::Succeeded, "what no longer answers was cleared to make room: {after:?}");
}

#[test]
fn an_option_the_store_does_not_have_is_refused_by_name() {
    let misspelled = Objects::open("s3://bucket/prefix", &[("aws_acess_key_id".into(), "AKIA".into())]).err().expect("refused").to_string();
    assert!(misspelled.contains("ZSQL_OBJECT_STORE_AWS_ACESS_KEY_ID") && !misspelled.contains("AKIA"), "{misspelled}");
    let s3 = Objects::open(
        "s3://bucket/prefix",
        &[
            ("aws_access_key_id".into(), "AKIA".into()),
            ("aws_secret_access_key".into(), "s3cret".into()),
            ("aws_region".into(), "us-east-1".into()),
        ],
    )
    .unwrap();
    assert_eq!(s3.key_of("r-1"), "prefix/cache/r-1/rows.arrow", "under the store's own prefix, then cache/");
    assert!(Objects::open("file:///tmp/zsql-none", &[("aws_region".into(), "x".into())]).is_err(), "a directory takes no options");
    assert!(Objects::open("not a url", &[]).is_err());
}
