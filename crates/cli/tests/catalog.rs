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
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "--all"]));
    assert!(ok && out.contains("2 tables, 0 views") && out.contains("(every schema)") && out.contains("next: zsql generate"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read"]));
    assert!(!ok && out.contains("say which: --schema NAME"), "no terminal, no picker: {out}");
    let (ok, out) = run(zsql(&home, &project).arg("catalog"));
    assert!(ok && out.contains("schema (every schema)") && out.contains("2 tables"), "bare catalog is the status: {out}");
    let v = file(&project);
    assert_eq!(v["scopes"].as_array().unwrap().len(), 1);
    assert!(v["scopes"][0].get("schema").is_none(), "{v}");
    assert!(v.get("added").is_none());
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "search", "ord"]));
    assert!(ok && out.contains("orders") && !out.contains("not in the catalog"), "{out}");

    // A table added by name after the read: `add` reads just it. Within a
    // whole-schema scope it needs no entry of its own.
    db.execute_batch("CREATE TABLE returns (id INTEGER PRIMARY KEY, order_id INTEGER);").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "returns"]));
    assert!(ok && out.contains("added   returns") && out.contains("table added:    returns"), "{out}");
    let v = file(&project);
    assert_eq!(names(&v), ["customers", "orders", "returns"]);
    assert!(v.get("added").is_none(), "covered by the scope read whole: {v}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "nowhere"]));
    assert!(!ok && out.contains("no table named nowhere"), "{out}");

    // A file with only tables read by name: each is its own provenance,
    // a sync re-reads each, and one found gone is dropped.
    std::fs::remove_file(project.join("catalog/local.json")).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "orders", "returns"]));
    assert!(ok && out.contains("2 added by name"), "{out}");
    let v = file(&project);
    assert_eq!(names(&v), ["orders", "returns"]);
    assert_eq!(v["added"].as_array().unwrap().len(), 2);
    assert!(v.get("scopes").is_none());
    db.execute_batch("DROP TABLE returns; ALTER TABLE orders ADD COLUMN status TEXT;").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "sync"]));
    assert!(ok, "{out}");
    assert!(out.contains("table dropped:  returns") && out.contains("column added:   orders.status"), "{out}");
    assert!(!out.contains("customers"), "a table never read is not reported: {out}");
    let v = file(&project);
    assert_eq!(names(&v), ["orders"]);
    assert_eq!(v["added"].as_array().unwrap().len(), 1);

    // A read of everything after that subsumes the entries read by name.
    // (SQLite has no schemas, so a named scope would cover nothing here;
    // the Postgres fixture is where a schema scope is proven.)
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "--schema", "nowhere"]));
    assert!(!ok && out.contains("SQLite has no schemas") && out.contains("kept as it was"), "{out}");
    std::fs::remove_file(project.join("catalog/local.json")).unwrap();
    assert!(run(zsql(&home, &project).args(["catalog", "read", "orders"])).0);
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "sync"]));
    assert!(ok, "{out}");
    let v = file(&project);
    assert_eq!(names(&v), ["orders"], "a sync re-reads what the file records, not more: {v}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "customers"]));
    assert!(ok, "{out}");
    assert_eq!(names(&file(&project)), ["customers", "orders"]);

    // A read that fails keeps the file as it was and exits non-zero.
    std::fs::rename(project.join("shop.db"), project.join("shop.db.away")).unwrap();
    let before = std::fs::read_to_string(project.join("catalog/local.json")).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "sync"]));
    assert!(!ok, "{out}");
    assert_eq!(std::fs::read_to_string(project.join("catalog/local.json")).unwrap(), before, "kept as it was");
    std::fs::rename(project.join("shop.db.away"), project.join("shop.db")).unwrap();

    // check reads the provenance: a model naming a table outside every
    // scope and not added is unchecked, not missing.
    std::fs::remove_file(project.join("catalog/local.json")).unwrap();
    assert!(run(zsql(&home, &project).args(["catalog", "read", "orders"])).0);
    std::fs::create_dir_all(project.join("models/core")).unwrap();
    std::fs::write(
        project.join("models/core/tbl.customers.yml"),
        "name: Customers\nphysical_name: customers\ndatasource: local\ncost: 1\nfields:\n- type: dimension\n  name: Name\n  data_type: string\n  expression:\n    sql: name\n",
    )
    .unwrap();
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "{out}");
    assert!(out.contains("1 table outside the catalog's scope") && out.contains("zsql catalog read -d local <table>"), "{out}");

    // Every next step `check` names has to be a command that exists: two of
    // these three used to print `zsql catalog <uid>`, which is not one.
    std::fs::remove_file(project.join("catalog/local.json")).unwrap();
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "{out}");
    assert!(out.contains("`zsql catalog read -d local --schema <name>` reads one"), "no catalog yet: {out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// `zsql datasource`: list says what is there and whether a secret is at
/// hand, add writes an entry from flags without asking, test opens it.
#[test]
fn datasources_are_listed_added_and_tested() {
    let root = std::env::temp_dir().join(format!("zsql-datasource-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop", "--no-git"]));
    assert!(ok && out.contains("next: zsql datasource add"), "{out}");
    assert!(!project.join(".git").exists(), "--no-git");
    // A fresh project loads, with no datasource.
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "a fresh project loads: {out}");
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "list"]));
    assert!(ok && out.contains("no datasource yet"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(!ok && out.contains("no datasource yet") && !out.contains("several"), "{out}");
    // Added from flags, no terminal needed.
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, total NUMERIC);").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "add", "local", "--adapter", "sqlite", "--file", "./shop.db"]));
    assert!(ok && out.contains("added   local (sqlite)"), "{out}");
    let ds = std::fs::read_to_string(project.join("datasources.yml")).unwrap();
    assert!(ds.contains("local:\n  name: local\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db"), "{ds}");
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "add", "wh", "--adapter", "postgres"]));
    assert!(!ok && out.contains("--host is needed for postgres"), "no terminal, no question: {out}");
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "list"]));
    assert!(ok && out.contains("local") && out.contains("none needed"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "test"]));
    assert!(ok && out.contains("local: ok, sqlite") && out.contains("next: zsql catalog read"), "{out}");
    // The catalog read with nothing named refuses without a terminal, naming the flags.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read"]));
    assert!(!ok && out.contains("--schema NAME") && out.contains("--all"), "{out}");
    // generate without the catalog, no terminal: the command that reads it.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(!ok && out.contains("no catalog yet") && out.contains("zsql catalog read -d local --schema NAME"), "{out}");
    assert!(run(zsql(&home, &project).args(["catalog", "read", "--all"])).0);
    let (ok, out) = run(zsql(&home, &project).arg("generate"));
    assert!(!ok && out.contains("say which tables") && out.contains("--all"), "no terminal, no picker: {out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "nowhere"]));
    assert!(!ok && out.contains("no table named nowhere") && out.contains("--live"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "orders"]));
    assert!(ok && out.contains("created models/local/tbl.orders.yml") && out.contains("next: zsql check"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["sql", "total"]));
    assert!(ok && out.contains("-- datasource: local (sqlite)"), "the key, not the display name: {out}");

    // One table's columns from the file; without -d, every file is searched.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "show", "orders"]));
    assert!(ok && out.contains("orders (local) table") && out.contains("id") && out.contains("key") && out.contains("total"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "show", "nowhere"]));
    assert!(!ok && out.contains("no table named nowhere") && out.contains("--live"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "show", "orders", "--json"]));
    assert!(ok && serde_json::from_str::<serde_json::Value>(&out).unwrap()["table"]["name"] == "orders", "{out}");
    // A second datasource with the same table: the match is ambiguous without -d.
    let db2 = rusqlite::Connection::open(project.join("other.db")).unwrap();
    db2.execute_batch("CREATE TABLE orders (id INTEGER PRIMARY KEY, amount NUMERIC);").unwrap();
    assert!(run(zsql(&home, &project).args(["datasource", "add", "other", "--adapter", "sqlite", "--file", "./other.db"])).0);
    assert!(run(zsql(&home, &project).args(["catalog", "read", "-d", "other", "--all"])).0);
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "show", "orders"]));
    assert!(!ok && out.contains("2 tables are named orders") && out.contains("local: orders") && out.contains("other: orders"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "show", "orders", "-d", "other"]));
    assert!(ok && out.contains("orders (other)") && out.contains("amount"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "search", "ord"]));
    assert!(ok && out.matches("orders").count() == 2 && out.contains("local") && out.contains("other"), "every file: {out}");

    // --agent: JSON where there is a shape, and never a prompt.
    let (ok, out) = run(zsql(&home, &project).args(["--agent", "catalog", "search", "ord"]));
    assert!(ok && out.trim_start().starts_with('{'), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["--agent", "fields"]));
    assert!(ok && out.trim_start().starts_with('{'), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["--agent", "run", "total"]));
    assert!(ok && serde_json::from_str::<serde_json::Value>(&out).unwrap()["manifest"]["rows"] == 1, "{out}");

    // A secret set after the fact, from stdin.
    let mut cmd = zsql(&home, &project);
    cmd.args(["datasource", "auth", "-d", "local", "--field", "password", "--secret-stdin"])
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped());
    let mut child = cmd.spawn().unwrap();
    use std::io::Write as _;
    child.stdin.take().unwrap().write_all(b"hunter2\n").unwrap();
    let out = child.wait_with_output().unwrap();
    assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
    assert!(std::fs::read_to_string(project.join(".zsql")).unwrap().contains("local_password: hunter2"));
    let (ok, out) = run(zsql(&home, &project).args(["datasource", "auth", "-d", "local"]));
    assert!(!ok && out.contains("takes no secret"), "sqlite opens a file: {out}");

    // An empty model from the template, and a test from the model's own fields.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "returns", "--empty", "-d", "local"]));
    assert!(ok && out.contains("create models/core/tbl.returns.yml"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["test", "--new", "smoke"]));
    assert!(ok && out.contains("create tests/smoke.yml"), "{out}");
    let test = std::fs::read_to_string(project.join("tests/smoke.yml")).unwrap();
    assert!(test.contains("  - Total") || test.contains("  - Id"), "from the model's own fields:\n{test}");
}

/// `catalog read` serves two jobs: adding a table by name, and refreshing
/// one it already holds. A name the warehouse does not have means something
/// different in each case, and the command used to call both a typo: it
/// kept the stale table, printed its success summary, and only then failed.
#[test]
fn a_dropped_table_is_forgotten_by_a_read_and_a_typo_is_named_first() {
    let root = std::env::temp_dir().join(format!("zsql-catalog-dropped-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n").unwrap();
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch("CREATE TABLE orders (id INTEGER); CREATE TABLE legacy (id INTEGER);").unwrap();
    drop(db);

    // Both read by name, so both are recorded with their provenance.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "orders", "legacy"]));
    assert!(ok, "{out}");
    assert_eq!(names(&file(&project)), ["legacy", "orders"], "both are in the file");

    // One is dropped upstream. Re-reading it by name is a refresh that
    // found it gone, which is success, and the entry leaves the file.
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch("DROP TABLE legacy;").unwrap();
    drop(db);
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "legacy"]));
    assert!(ok, "a refresh that finds the table dropped is not a failure: {out}");
    assert!(out.contains("table dropped"), "and says so: {out}");
    assert_eq!(names(&file(&project)), ["orders"], "the stale table leaves the file, the rest stays");

    // A name the catalog never held is a mistake, and it is said before the
    // summary rather than after it.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "ordrs"]));
    assert!(!ok, "a name that is not there fails: {out}");
    assert!(out.contains("ordrs"), "and is named: {out}");
    // Both lines are on stderr, so compare them there: an offset into
    // stdout ++ stderr would satisfy this for the wrong reason if one of
    // them ever moved streams.
    let stderr = String::from_utf8_lossy(&zsql(&home, &project).args(["catalog", "read", "ordrs"]).output().unwrap().stderr).to_string();
    let summary = stderr.find("-> catalog").expect("the summary is on stderr");
    assert!(stderr.find("ordrs").expect("the error is on stderr") < summary, "the error comes before the summary:\n{stderr}");
    assert!(out.contains("1 not found"), "and the summary says it is partial: {out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The same, on a warehouse whose tables have schemas, which SQLite's do
/// not. An unqualified name matches whatever schema holds it, but forgetting
/// one matches the schema exactly, so a read that passed the user's spelling
/// through said "dropped" and removed nothing. DuckDB needs no server.
#[test]
fn a_dropped_table_is_forgotten_by_its_own_schema_not_the_name_you_typed() {
    let root = std::env::temp_dir().join(format!("zsql-catalog-schemas-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: duckdb\n  tier: hot\n  file: ./shop.duckdb\n").unwrap();
    let db = duckdb::Connection::open(project.join("shop.duckdb")).unwrap();
    db.execute_batch(
        "CREATE SCHEMA sales; CREATE SCHEMA ops;\
         CREATE TABLE sales.orders (id BIGINT);\
         CREATE TABLE sales.legacy (id BIGINT);\
         CREATE TABLE ops.legacy (id BIGINT);",
    )
    .unwrap();
    drop(db);

    // Read all three by name, unqualified for the pair that share one.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "sales.orders", "legacy"]));
    assert!(ok, "{out}");
    let listed = |project: &Path| -> Vec<String> {
        let v: serde_json::Value = serde_json::from_str(&std::fs::read_to_string(project.join("catalog/local.json")).unwrap()).unwrap();
        v["tables"]
            .as_array()
            .unwrap()
            .iter()
            .map(|t| format!("{}.{}", t["schema"].as_str().unwrap_or("-"), t["name"].as_str().unwrap()))
            .collect()
    };
    assert_eq!(listed(&project), ["ops.legacy", "sales.legacy", "sales.orders"], "all three recorded");

    // Drop one of the pair. The unqualified name still finds the survivor,
    // so the read succeeds, and the one that went has to be reported anyway.
    let db = duckdb::Connection::open(project.join("shop.duckdb")).unwrap();
    db.execute_batch("DROP TABLE ops.legacy;").unwrap();
    drop(db);
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "legacy"]));
    assert!(ok, "the survivor answered, so this is not a failure: {out}");
    assert!(out.contains("table dropped:  ops.legacy"), "the one that went is named with its schema: {out}");
    assert_eq!(listed(&project), ["sales.legacy", "sales.orders"], "and it leaves the file");

    // Now the last of that name. A qualified read removes it too.
    let db = duckdb::Connection::open(project.join("shop.duckdb")).unwrap();
    db.execute_batch("DROP TABLE sales.legacy;").unwrap();
    drop(db);
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "legacy"]));
    assert!(ok, "{out}");
    assert!(out.contains("table dropped:  sales.legacy"), "{out}");
    assert_eq!(listed(&project), ["sales.orders"], "nothing of that name is left");

    // And a name the catalog never held is still a mistake.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "legacyy"]));
    assert!(!ok && out.contains("legacyy"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}
