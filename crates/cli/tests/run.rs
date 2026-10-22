//! `zsql run` and `zsql results`, end to end on SQLite: a planned query
//! runs on the datasource the planner chose, its rows are kept under the
//! home directory, a second run is a hit and says so, --fresh runs again,
//! the cap cuts and says so, every format is explicit, and the stored
//! result is listed, shown, exported and removed.

use std::path::Path;
use std::process::Command;

fn zsql(home: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zsql"));
    cmd.env("ZSQL_HOME", home)
        .env_remove("ZSQL_SERVER")
        .env_remove("ZSQL_API_KEY")
        .env_remove("ZSQL_RESULTS")
        .env_remove("ZSQL_RESULTS_DIR");
    cmd.current_dir(project);
    cmd
}

fn run(cmd: &mut Command) -> (bool, String, String) {
    let out = cmd.output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string());
    eprintln!("$ {:?}\n{stdout}{stderr}", cmd.get_args().collect::<Vec<_>>());
    (out.status.success(), stdout, stderr)
}

#[test]
fn a_query_runs_is_kept_and_answers_again_from_the_store() {
    let root = std::env::temp_dir().join(format!("zsql-run-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n").unwrap();
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT, region TEXT, total NUMERIC);\
         INSERT INTO orders VALUES (1, 'paid', 'east', 10.5), (2, 'paid', 'east', 4.25), (3, 'paid', 'west', 7), (4, 'open', 'east', 1), (5, NULL, 'north', 2);",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("models/core")).unwrap();
    std::fs::write(
        project.join("models/core/tbl.orders.yml"),
        "name: Orders\nphysical_name: orders\ndatasource: local\ncost: 1\nfields:\n\
         - type: dimension\n  name: Status\n  data_type: string\n  expression:\n    sql: status\n\
         - type: dimension\n  name: Region\n  data_type: string\n  expression:\n    sql: region\n\
         - type: measure\n  name: Total\n  data_type: decimal\n  expression:\n    sql: sum(total)\n",
    )
    .unwrap();
    assert!(run(zsql(&home, &project).arg("check")).0, "the fixture model loads");

    // A first run goes to the warehouse and is kept.
    let (ok, out, err) = run(zsql(&home, &project).args(["run", "--expr", "status, total"]));
    assert!(ok, "{out}{err}");
    assert!(out.contains("Status") && out.contains("Total"), "a header: {out}");
    assert!(out.contains("paid") && out.contains("21.75"), "rows: {out}");
    assert!(out.contains('∅'), "a null status is shown as such: {out}");
    assert!(err.contains("3 rows (complete)") && err.contains("local (sqlite)"), "the footer: {err}");
    assert!(!err.contains("cached"), "{err}");
    let id = err.split(" · ").find(|p| p.trim().starts_with("r-")).map(|p| p.trim().to_string()).expect("an id in the footer");
    let results = home.join("results").join("shop");
    assert!(results.is_dir(), "kept under the home directory, outside the project");
    assert!(!project.join("results").exists());

    // The same question again is a hit, and says so.
    let (ok, out2, err) = run(zsql(&home, &project).args(["run", "--expr", "status, total"]));
    assert!(ok && out2 == out, "{out2}{err}");
    assert!(err.contains("cached") && err.contains("--fresh to run again") && err.contains(&id), "{err}");

    // --fresh runs again and replaces the result; the id stays, being the key's.
    let (ok, _, err) = run(zsql(&home, &project).args(["run", "--expr", "status, total", "--fresh"]));
    assert!(ok && err.contains("ran fresh") && !err.contains("cached"), "{err}");

    // Another question is another result; a context is another key.
    let (ok, out, err) = run(zsql(&home, &project).args(["run", "--expr", "region, total"]));
    assert!(ok && out.contains("north") && !err.contains("cached"), "{out}{err}");

    // The cap cuts and says so; the user's own limit is the question's.
    let (ok, out, err) = run(zsql(&home, &project).env("ZSQL_RUN_CAP", "2").args(["run", "--expr", "region, total, status"]));
    assert!(ok, "{out}{err}");
    assert_eq!(out.lines().count(), 4, "header, rule, two rows: {out}");
    assert!(err.contains("2 of more") && err.contains("--all"), "{err}");
    assert!(err.contains("warning: cut at 2 rows"), "{err}");
    let (ok, out, err) = run(zsql(&home, &project).args(["run", "--expr", "region, total, limit 1"]));
    assert!(ok && out.lines().count() == 3 && err.contains("1 rows (your limit 1)"), "{out}{err}");
    let (ok, out, _) = run(zsql(&home, &project).args(["run", "--expr", "region, total", "--limit", "2", "--fresh"]));
    assert!(ok && out.lines().count() == 4, "{out}");

    // Every machine format is explicit.
    let (ok, out, _) = run(zsql(&home, &project).args(["run", "--expr", "status, total", "--format", "json"]));
    assert!(ok, "{out}");
    let v: serde_json::Value = serde_json::from_str(&out).unwrap();
    assert_eq!(v["cache"]["hit"], true);
    assert_eq!(v["manifest"]["rows"], 3);
    assert_eq!(v["manifest"]["complete"], true);
    assert_eq!(v["manifest"]["columns"][0]["data_type"], "string", "typed by the model");
    assert_eq!(v["manifest"]["columns"][1]["data_type"], "decimal");
    assert_eq!(v["manifest"]["lineage"]["tables"][0], "orders");
    assert_eq!(v["rows"].as_array().unwrap().len(), 3);
    let (ok, out, _) = run(zsql(&home, &project).args(["run", "--expr", "status, total", "--format", "jsonl"]));
    assert!(ok && out.lines().count() == 3 && out.lines().all(|l| l.starts_with('{')), "{out}");
    assert!(out.contains("\"Status\":null"), "null is null: {out}");
    let (ok, out, _) = run(zsql(&home, &project).args(["run", "--expr", "status, total", "--format", "csv"]));
    assert!(ok && out.starts_with("Status,Total\n") && out.lines().count() == 4, "{out}");
    assert!(out.contains("\n,2"), "null is empty: {out}");
    let csv = root.join("out.csv");
    let (ok, _, err) = run(zsql(&home, &project).args(["run", "--expr", "status, total", "--out", csv.to_str().unwrap()]));
    assert!(ok && err.contains("-> ") && std::fs::read_to_string(&csv).unwrap().starts_with("Status,Total"), "{err}");

    // What was kept: listed, shown, exported, the SQL, removed, purged.
    let (ok, out, err) = run(zsql(&home, &project).arg("results"));
    assert!(ok, "{out}{err}");
    assert!(out.contains(&id) && out.contains("status, total") && out.contains("region, total"), "{out}");
    assert!(err.contains("results under"), "{err}");
    let (ok, out, err) = run(zsql(&home, &project).args(["results", "show", &id, "--head", "1"]));
    assert!(ok && out.lines().count() == 3 && err.contains("1 shown"), "{out}{err}");
    let (ok, out, _) = run(zsql(&home, &project).args(["results", "show", &id[..15], "--format", "jsonl", "--offset", "2"]));
    assert!(ok && out.lines().count() == 1, "a prefix finds it, the offset skips: {out}");
    let (ok, out, _) = run(zsql(&home, &project).args(["results", "sql", &id]));
    assert!(ok && out.to_uppercase().contains("SELECT") && out.contains("LIMIT 10001"), "the cap went into the statement: {out}");
    let exported = root.join("exported.json");
    let (ok, _, err) = run(zsql(&home, &project).args(["results", "export", &id, "--out", exported.to_str().unwrap()]));
    assert!(ok && err.contains("3 rows ->"), "{err}");
    let (ok, _, err) = run(zsql(&home, &project).args(["results", "rm", &id]));
    assert!(ok && err.contains("removed"), "{err}");
    let (ok, _, err) = run(zsql(&home, &project).args(["results", "show", &id]));
    assert!(!ok && err.contains("no result"), "{err}");
    let (ok, _, err) = run(zsql(&home, &project).args(["results", "purge"]));
    assert!(ok && err.contains("purged"), "{err}");
    let (ok, out, _) = run(zsql(&home, &project).arg("results"));
    assert!(ok && out.contains("no results"), "{out}");

    // Nothing kept when told so; a datasource with cache_ttl 0 keeps nothing either.
    let (ok, _, err) = run(zsql(&home, &project).env("ZSQL_RESULTS", "off").args(["run", "--expr", "status, total"]));
    assert!(ok && err.contains("not kept"), "{err}");
    assert!(run(zsql(&home, &project).env("ZSQL_RESULTS", "off").arg("results")).2.contains("ZSQL_RESULTS=off"));
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n  cache_ttl: 0\n").unwrap();
    assert!(run(zsql(&home, &project).args(["run", "--expr", "status, total"])).0);
    let (ok, _, err) = run(zsql(&home, &project).args(["run", "--expr", "status, total"]));
    assert!(ok && !err.contains("cached"), "a zero TTL is never a hit: {err}");

    // Remote is refused: the credentials are here.
    let (ok, _, err) = run(zsql(&home, &project).args(["run", "-r", "--expr", "status, total"]));
    assert!(!ok && err.contains("does not take -r"), "{err}");
}
