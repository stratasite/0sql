//! `zsql catalog`, end to end on SQLite: a read records what it covered,
//! a sync re-reads what the file records and reports drift, a table read
//! by name is kept and re-read on its own, a table found gone by a read
//! that asked for it is dropped, a read that fails keeps the file as it
//! was, and search looks in the file or in the warehouse.

use std::path::Path;
use std::process::Command;

fn zsql(home: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zsql"));
    cmd.env("ZSQL_HOME", home).env_remove("ZSQL_SERVER").env_remove("ZSQL_API_KEY").current_dir(project);
    cmd
}

fn run(cmd: &mut Command) -> (bool, String) {
    let out = cmd.output().unwrap();
    let text = format!("{}{}", String::from_utf8_lossy(&out.stdout), String::from_utf8_lossy(&out.stderr));
    eprintln!("$ {:?}\n{text}", cmd.get_args().collect::<Vec<_>>());
    (out.status.success(), text)
}

fn file(project: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(project.join("catalog/local.json")).unwrap()).unwrap()
}

fn names(v: &serde_json::Value) -> Vec<String> {
    v["tables"].as_array().unwrap().iter().map(|t| t["name"].as_str().unwrap().to_string()).collect()
}

#[test]
fn a_catalog_is_a_union_with_its_provenance_and_a_sync_re_reads_it() {
    let root = std::env::temp_dir().join(format!("zsql-catalog-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n").unwrap();
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name TEXT); CREATE TABLE orders (id INTEGER PRIMARY KEY, total NUMERIC);",
    )
    .unwrap();

    // Nothing to search before a read; the warehouse answers with --live.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "search", "ord"]));
    assert!(!ok && out.contains("no catalog yet"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "search", "ord", "--live"]));
    assert!(ok && out.contains("orders") && out.contains("not in the catalog") && !out.contains("customers"), "{out}");

    // A read of everything records one scope of every schema.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "local"]));
    assert!(ok && out.contains("2 tables, 0 views") && out.contains("(every schema)"), "{out}");
    let v = file(&project);
    assert_eq!(v["scopes"].as_array().unwrap().len(), 1);
    assert!(v["scopes"][0].get("schema").is_none(), "{v}");
    assert!(v.get("added").is_none());
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "search", "ord"]));
    assert!(ok && out.contains("orders") && !out.contains("not in the catalog"), "{out}");

    // A table added by name after the read: `add` reads just it. Within a
    // whole-schema scope it needs no entry of its own.
    db.execute_batch("CREATE TABLE returns (id INTEGER PRIMARY KEY, order_id INTEGER);").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "add", "returns"]));
    assert!(ok && out.contains("added   returns") && out.contains("table added:    returns"), "{out}");
    let v = file(&project);
    assert_eq!(names(&v), ["customers", "orders", "returns"]);
    assert!(v.get("added").is_none(), "covered by the scope read whole: {v}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "add", "nowhere"]));
    assert!(!ok && out.contains("no table named nowhere"), "{out}");

    // A file with only tables read by name: each is its own provenance,
    // a sync re-reads each, and one found gone is dropped.
    std::fs::remove_file(project.join("catalog/local.json")).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "add", "orders", "returns"]));
    assert!(ok && out.contains("2 added by name"), "{out}");
    let v = file(&project);
    assert_eq!(names(&v), ["orders", "returns"]);
    assert_eq!(v["added"].as_array().unwrap().len(), 2);
    assert!(v.get("scopes").is_none());
    db.execute_batch("DROP TABLE returns; ALTER TABLE orders ADD COLUMN status TEXT;").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "local"]));
    assert!(ok, "{out}");
    assert!(out.contains("table dropped:  returns") && out.contains("column added:   orders.status"), "{out}");
    assert!(!out.contains("customers"), "a table never read is not reported: {out}");
    let v = file(&project);
    assert_eq!(names(&v), ["orders"]);
    assert_eq!(v["added"].as_array().unwrap().len(), 1);

    // A read of everything after that subsumes the entries read by name.
    // (SQLite has no schemas, so a named scope would cover nothing here;
    // the Postgres fixture is where a schema scope is proven.)
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "local", "--schema", "nowhere"]));
    assert!(!ok && out.contains("SQLite has no schemas") && out.contains("kept as it was"), "{out}");
    std::fs::remove_file(project.join("catalog/local.json")).unwrap();
    assert!(run(zsql(&home, &project).args(["catalog", "add", "orders"])).0);
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "local"]));
    assert!(ok, "{out}");
    let v = file(&project);
    assert_eq!(names(&v), ["orders"], "a sync re-reads what the file records, not more: {v}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "add", "customers"]));
    assert!(ok, "{out}");
    assert_eq!(names(&file(&project)), ["customers", "orders"]);

    // A read that fails keeps the file as it was and exits non-zero.
    std::fs::rename(project.join("shop.db"), project.join("shop.db.away")).unwrap();
    let before = std::fs::read_to_string(project.join("catalog/local.json")).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "local"]));
    assert!(!ok, "{out}");
    assert_eq!(std::fs::read_to_string(project.join("catalog/local.json")).unwrap(), before, "kept as it was");
    std::fs::rename(project.join("shop.db.away"), project.join("shop.db")).unwrap();

    // check reads the provenance: a model naming a table outside every
    // scope and not added is unchecked, not missing.
    std::fs::remove_file(project.join("catalog/local.json")).unwrap();
    assert!(run(zsql(&home, &project).args(["catalog", "add", "orders"])).0);
    std::fs::create_dir_all(project.join("models/core")).unwrap();
    std::fs::write(
        project.join("models/core/tbl.customers.yml"),
        "name: Customers\nphysical_name: customers\ndatasource: local\ncost: 1\nfields:\n- type: dimension\n  name: Name\n  data_type: string\n  expression:\n    sql: name\n",
    )
    .unwrap();
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "{out}");
    assert!(out.contains("1 table outside the catalog's scope") && out.contains("zsql catalog add <table>"), "{out}");
}
