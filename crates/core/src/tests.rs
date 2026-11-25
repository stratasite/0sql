use crate::{unlimited, Kind, Model, SqlRequest, ValueCatalog};
use serde_json::{json, Value};
use zsql_dialect::Dialects;
use zsql_golden::{Fixtures, PathSource};

/// A model is shared across threads by the server; nothing added to
/// it may take that away.
#[test]
fn a_model_is_send_and_sync() {
    fn assert<T: Send + Sync>() {}
    assert::<Model>();
}

fn dialects() -> &'static Dialects {
    Box::leak(Box::new(Dialects::builtin()))
}

/// Every golden case planned over its universe's stored paths, as a
/// model does: the SQL for each, or the error, in case order.
fn plan_all(fixtures: &Fixtures, dialects: &'static Dialects) -> Vec<String> {
    let mut out = Vec::new();
    let mut models: std::collections::HashMap<String, Model> = Default::default();
    for cases in fixtures.cases().expect("golden cases").into_values() {
        for case in cases {
            let model = models.entry(case.universe.clone()).or_insert_with(|| {
                Model::build("g", &case.universe, fixtures.snapshot(&case.universe).expect("snapshot"), dialects).expect("universe")
            });
            let req = SqlRequest { spec: Some(case.spec.clone()), expr: None, context: case.context.clone() };
            out.push(match model.sql(req, &unlimited) {
                Ok(r) => format!("{}: {}", case.id, r.sql),
                Err(e) => format!("{}: ERROR {e}", case.id),
            });
        }
    }
    out
}

/// The planner numbers projections from a thread-local counter.
/// Whatever thread plans, and however many plans came before it on
/// that thread, the SQL must come out the same; otherwise the
/// server, which plans on a pool, and the CLI, which plans on its
/// main thread, would disagree.
#[test]
fn sql_is_the_same_on_every_thread() {
    let fx = Fixtures::open(Fixtures::default_root());
    let d = dialects();
    let here = plan_all(&fx, d);
    let there = std::thread::spawn(move || {
        // Advance the counter first, so the ids this thread hands
        // out do not line up with the main thread's by accident.
        let fx = Fixtures::open(Fixtures::default_root());
        let _ = plan_all(&fx, d);
        plan_all(&fx, d)
    })
    .join()
    .expect("planning thread");
    assert!(!here.is_empty(), "no golden cases found");
    assert_eq!(here, there);
    let _ = PathSource::Formed;
}

/// The same, across processes: a child of this test binary plans
/// everything and reports a digest, which must match this process's.
#[test]
fn sql_is_the_same_in_another_process() {
    const CHILD: &str = "ZSQL_SERVICE_DETERMINISM_CHILD";
    let fx = Fixtures::open(Fixtures::default_root());
    let here = digest(&plan_all(&fx, dialects()));
    if std::env::var_os(CHILD).is_some() {
        println!("DIGEST {here}");
        return;
    }
    let out = std::process::Command::new(std::env::current_exe().expect("test binary"))
        .args(["tests::sql_is_the_same_in_another_process", "--exact", "--nocapture", "--test-threads=1"])
        .env(CHILD, "1")
        .output()
        .expect("child test process");
    let text = String::from_utf8_lossy(&out.stdout);
    // With --nocapture the harness has already printed `test name ...` on the line.
    let there = text
        .lines()
        .find_map(|l| l.find("DIGEST ").map(|i| l[i + 7..].trim()))
        .unwrap_or_else(|| panic!("no digest from the child:\n{text}\n{}", String::from_utf8_lossy(&out.stderr)));
    assert_eq!(here, there);
}

fn digest(lines: &[String]) -> String {
    // FNV-1a over the whole text: no dependency, and a mismatch is
    // all the test needs to detect.
    let mut h: u64 = 0xcbf29ce484222325;
    for b in lines.join("\n").bytes() {
        h ^= b as u64;
        h = h.wrapping_mul(0x100000001b3);
    }
    format!("{h:016x}")
}

/// A model written to disk and read back answers the same.
#[test]
fn a_stored_model_comes_back_whole() {
    let fx = Fixtures::open(Fixtures::default_root());
    let d = dialects();
    let hash = fx.snapshot_hashes().expect("hashes").into_iter().next().expect("a universe");
    let model = Model::build("p", "b", fx.snapshot(&hash).expect("snapshot"), d).expect("universe");
    let dir = std::env::temp_dir().join(format!("zsql-core-stored-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    model.write(&dir).expect("write");
    let back = Model::from_stored(&dir, "p", "b", d).expect("read back");
    assert_eq!(back.summary()["fields"], model.summary()["fields"]);
    assert_eq!(back.tables(""), model.tables(""));
    assert_eq!(back.fields("", true), model.fields("", true));
    let _ = std::fs::remove_dir_all(&dir);
}

/// Each list keeps what holds its `q`, in any case, and is whole
/// without one.
#[test]
fn a_list_keeps_what_holds_its_q() {
    use zsql_project::values::{DimensionValues, ValueCount};
    let fx = Fixtures::open(Fixtures::default_root());
    let hash = fx.snapshot_hashes().expect("hashes").into_iter().next().expect("a universe");
    let mut model = Model::build("p", "b", fx.snapshot(&hash).expect("snapshot"), dialects()).expect("universe");
    let names = |v: &Value, list: &str, key: &str| -> Vec<String> {
        v[list].as_array().expect("a list").iter().map(|x| x[key].as_str().unwrap_or_default().to_string()).collect()
    };

    let all = names(&model.tables(""), "tables", "name");
    assert_eq!(names(&model.tables("   "), "tables", "name"), all, "a blank q filters nothing");
    let asked = format!("  {}  ", all[0].to_uppercase());
    let hit = model.tables(&asked);
    assert!(names(&hit, "tables", "name").contains(&all[0]), "found in any case, untrimmed");
    for t in hit["tables"].as_array().unwrap() {
        let holds = |k: &str| t[k].as_str().unwrap_or_default().to_lowercase().contains(&all[0].to_lowercase());
        assert!(holds("uid") || holds("name") || holds("physical_name"), "{t}");
    }
    assert!(names(&model.tables("no table is called this"), "tables", "name").is_empty());

    let sources = model.datasources("");
    let first = &sources["datasources"][0];
    let (uid, adapter) = (first["uid"].as_str().unwrap(), first["adapter"].as_str().unwrap());
    assert_eq!(model.datasource(&uid.to_uppercase()).map(|d| d.adapter.as_str()), Some(adapter));
    assert!(model.datasource("no-such-datasource").is_none());
    assert!(names(&model.datasources(adapter), "datasources", "uid").contains(&uid.to_string()));
    assert!(names(&model.datasources("no datasource is called this"), "datasources", "uid").is_empty());

    let dim = model.semantic().fields().iter().find(|f| f.is_dimension()).expect("a dimension").uid.clone();
    let count = |v: &str, n: i64| ValueCount { v: v.into(), n };
    model.values = Some(ValueCatalog {
        dimensions: vec![DimensionValues {
            uid: dim.clone(),
            name: "d".into(),
            table: "t".into(),
            sampled_at: "2026-01-01T00:00:00Z".into(),
            sample_size: None,
            distinct: Some(3),
            values: vec![count("Northeast", 5), count("North", 3), count("South", 1)],
            range: None,
        }],
    });
    assert_eq!(names(&model.values_of(&dim, "").unwrap(), "values", "v"), ["Northeast", "North", "South"]);
    let north = model.values_of(&dim, "NORTH").unwrap();
    assert_eq!(names(&north, "values", "v"), ["Northeast", "North"], "most frequent first, still");
    assert_eq!(north["distinct"], 3, "the counts describe the sample, not the filter");
    assert_eq!(model.values_of(&dim, "nowhere").unwrap()["values"], json!([]), "no match is an empty list, not a refusal");
}

/// A refusal names its class and kind the way the API always has.
#[test]
fn refusals_carry_their_class() {
    let fx = Fixtures::open(Fixtures::default_root());
    let d = dialects();
    // A universe with policies would refuse everything below for
    // the want of a context, which is its own test.
    let (open, guarded) = fx
        .snapshot_hashes()
        .expect("hashes")
        .into_iter()
        .map(|h| fx.snapshot(&h).expect("snapshot"))
        .fold((None, None), |(o, g), s| if s.security_policies.is_empty() { (o.or(Some(s)), g) } else { (o, g.or(Some(s))) });
    let model = Model::build("p", "b", open.expect("a universe without policies"), d).expect("universe");
    let e = model.sql(SqlRequest::default(), &unlimited).unwrap_err();
    assert_eq!((e.kind, e.class), (Kind::Invalid, "Invalid"));
    let secured = Model::build("p", "b", guarded.expect("a universe with policies"), d).expect("universe");
    let e = secured.sql(SqlRequest { spec: None, expr: Some("anything".into()), context: None }, &unlimited).unwrap_err();
    assert_eq!((e.kind, e.class), (Kind::Invalid, "ContextRequired"));
    let e = model.values_of("no-such-dimension", "").unwrap_err();
    assert_eq!(e.kind, Kind::NotFound);
    let e = model.sql(SqlRequest { spec: None, expr: Some("no such field at all".into()), context: None }, &unlimited).unwrap_err();
    assert_eq!(e.kind, Kind::Unprocessable, "{e}");
}
