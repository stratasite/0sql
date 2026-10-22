//! `zsql catalog` then `zsql generate`, end to end: a draft the real
//! loader accepts and the planner can query, a new column listed and
//! then added, and nothing ever rewritten. SQLite needs no server; the
//! Postgres variant runs against the container fixture when CI has one.

use std::path::{Path, PathBuf};
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

fn scratch(name: &str) -> (PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("zsql-generate-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let home = root.join("home");
    let project = root.join("shop");
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    (home, project)
}

#[test]
fn a_sqlite_warehouse_becomes_a_model_the_planner_can_query() {
    let (home, project) = scratch("sqlite");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n").unwrap();
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name VARCHAR(80) NOT NULL, notes TEXT);\
         CREATE TABLE orders (id INTEGER PRIMARY KEY, customer_id INTEGER NOT NULL REFERENCES customers(id), \
           placed DATE NOT NULL, total NUMERIC(12,2) NOT NULL, tags TEXT[]);\
         INSERT INTO customers VALUES (1, 'Ann', NULL), (2, 'Bo', 'vip');\
         INSERT INTO orders VALUES (1, 1, '2026-03-01', 10.5, NULL), (2, 2, '2026-03-02', 4.25, NULL);",
    )
    .unwrap();

    // Introspect, then generate: two drafts and a join.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "-d", "local", "--all"]));
    assert!(ok && out.contains("2 tables"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(ok, "{out}");
    assert!(out.contains("created models/local/tbl.customers.yml") && out.contains("created models/local/tbl.orders.yml"), "{out}");
    assert!(out.contains("created models/local/rel.local.yml"), "{out}");
    assert!(out.contains("skipped tags: TEXT[] has no 0sql type"), "an untyped column is named, not dropped silently: {out}");

    let orders = std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap();
    assert!(orders.contains("physical_name: orders"));
    assert!(orders.contains("type: measure") && orders.contains("sql: sum(total)"), "total looks additive:\n{orders}");
    assert!(orders.contains("primary_key: true"), "the key is marked:\n{orders}");
    assert!(orders.contains("# inferred"), "guesses are marked on their lines:\n{orders}");
    assert!(orders.lines().filter(|l| l.contains("name: Customer Id")).count() == 1, "a key column stays a dimension:\n{orders}");
    let rel = std::fs::read_to_string(project.join("models/local/rel.local.yml")).unwrap();
    assert!(rel.contains("left: Orders") && rel.contains("right: Customers") && rel.contains("left.customer_id = right.id"), "{rel}");
    assert!(rel.contains("cardinality: many_to_one"), "{rel}");

    // The real loader accepts it, and the planner answers from it.
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("2 tables") && out.contains("1 joins"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["sql", "--expr", "name, total"]));
    assert!(ok, "{out}");
    assert!(
        out.to_uppercase().contains("SUM(") && out.contains("customers") && out.contains("orders"),
        "a join across the generated model:\n{out}"
    );

    // A second run rewrites nothing and has nothing to say but the untyped column.
    let before = orders.clone();
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(ok && out.contains("nothing to do"), "{out}");
    assert!(out.contains("no 0sql type, so not a field: tags"), "{out}");
    assert_eq!(std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap(), before, "never rewritten");

    // The warehouse grows two columns: listed with the command to add them.
    db.execute_batch("ALTER TABLE orders ADD COLUMN status VARCHAR(20); ALTER TABLE orders ADD COLUMN discount NUMERIC(6,2);").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "-d", "local", "--all"]));
    assert!(ok, "{out}");
    assert!(
        out.contains("column added:   orders.status") && out.contains("column added:   orders.discount"),
        "drift is reported before the catalog is replaced:\n{out}"
    );
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(ok && out.contains("2 new columns: status, discount"), "{out}");
    assert!(out.contains("zsql generate orders --columns status,discount"), "the exact command is printed: {out}");
    assert_eq!(std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap(), before, "listing changes nothing");

    // One by name, then the rest; the file keeps everything it had.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "orders", "--columns", "discount"]));
    assert!(ok && out.contains("added   1 field"), "{out}");
    let after = std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap();
    assert!(after.starts_with(&before), "appended, not rewritten");
    assert!(after.contains("sql: sum(discount)") && !after.contains("sql: status"), "{after}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all", "--new-columns"]));
    assert!(ok && out.contains("added   1 field"), "{out}");
    let after = std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap();
    assert!(after.contains("sql: status"), "{after}");
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("2 tables"), "still loads: {out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(ok && out.contains("nothing to do"), "{out}");

    // The warehouse changes under the model: check names what is gone.
    db.execute_batch("ALTER TABLE orders DROP COLUMN status; ALTER TABLE customers RENAME TO clients;").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "-d", "local", "--all"]));
    assert!(ok, "{out}");
    assert!(
        out.contains("column dropped: orders.status")
            && out.contains("table dropped:  customers")
            && out.contains("table added:    clients"),
        "{out}"
    );
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(!ok, "a missing table and column fail the check:\n{out}");
    assert!(out.contains("table Customers names customers, which the warehouse does not have"), "{out}");
    assert!(out.contains("orders has no column status"), "{out}");
    assert!(out.contains("checked against catalog"), "{out}");
    db.execute_batch("ALTER TABLE clients RENAME TO customers; ALTER TABLE orders ADD COLUMN status VARCHAR(20);").unwrap();
    assert!(run(zsql(&home, &project).args(["catalog", "read", "-d", "local", "--all"])).0);
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "put back, the check passes again:\n{out}");

    // --dry-run writes nothing, for a new table too.
    db.execute_batch("CREATE TABLE products (id INTEGER PRIMARY KEY, title TEXT);").unwrap();
    assert!(run(zsql(&home, &project).args(["catalog", "read", "-d", "local", "--all"])).0);
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all", "--dry-run"]));
    assert!(ok && out.contains("would create models/local/tbl.products.yml"), "{out}");
    assert!(!project.join("models/local/tbl.products.yml").exists());
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// The Postgres container's shop schema shows what SQLite cannot: a
/// declared width turning sampling on, comments becoming descriptions,
/// and a foreign key with a schema. `ZSQL_TEST_PG_*` as the sources job
/// sets them; ignored otherwise.
#[test]
#[ignore]
fn a_postgres_schema_becomes_a_model_with_sampling_and_descriptions() {
    let (home, project) = scratch("postgres");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    let var = |k: &str, d: &str| std::env::var(k).unwrap_or_else(|_| d.to_string());
    std::fs::write(
        project.join("datasources.yml"),
        format!(
            "wh:\n  adapter: postgres\n  tier: hot\n  host: {}\n  port: {}\n  database: {}\n  username: {}\n  schema: shop\n  sslmode: disable\n",
            var("ZSQL_TEST_PG_HOST", "localhost"),
            var("ZSQL_TEST_PG_PORT", "5466"),
            var("ZSQL_TEST_PG_DB", "postgres"),
            var("ZSQL_TEST_PG_USER", "postgres")
        ),
    )
    .unwrap();
    std::fs::write(
        project.join(".zsql"),
        format!("wh:\n  password: {}\n", std::env::var("ZSQL_TEST_PG_PASSWORD").expect("ZSQL_TEST_PG_PASSWORD")),
    )
    .unwrap();

    // The connection's schema is its search path, not the catalog's scope: the scope is said.
    let (ok, out) = run(zsql(&home, &project).args(["catalog", "read", "-d", "wh", "--schema", "shop"]));
    assert!(ok && out.contains("(shop)"), "{out}");
    if !out.contains("2 tables, 1 views") {
        // Say what this connection can see at all, so a miss explains itself.
        let ds = std::fs::read_to_string(project.join("datasources.yml")).unwrap().replace("  schema: shop\n", "");
        std::fs::write(project.join("datasources.yml"), ds).unwrap();
        let (_, every) = run(zsql(&home, &project).args(["catalog", "read", "-d", "wh", "--all"]));
        let cat = std::fs::read_to_string(project.join("catalog/wh.json")).unwrap_or_default();
        let names: Vec<String> = serde_json::from_str::<serde_json::Value>(&cat)
            .ok()
            .and_then(|v| {
                v["tables"].as_array().map(|ts| {
                    ts.iter().map(|t| format!("{}.{}", t["schema"].as_str().unwrap_or("-"), t["name"].as_str().unwrap_or("-"))).collect()
                })
            })
            .unwrap_or_default();
        panic!("shop was empty for the CLI. Without a scope it saw: {every}\n{}", names.join(", "));
    }
    let (ok, out) = run(zsql(&home, &project).args(["generate", "-d", "wh", "--all"]));
    assert!(ok && out.contains("created models/shop/tbl.customers.yml"), "{out}");

    let customers = std::fs::read_to_string(project.join("models/shop/tbl.customers.yml")).unwrap();
    assert!(customers.contains("physical_name: shop.customers"), "{customers}");
    assert!(customers.contains("description: Who buys"), "the table comment:\n{customers}");
    assert!(customers.contains("description: As they sign"), "the column comment:\n{customers}");
    assert!(
        customers.contains("enable_sampling: true") && customers.contains("a short string, 80 wide"),
        "varchar(80) is sampled:\n{customers}"
    );
    assert_eq!(customers.matches("enable_sampling: true").count(), 1, "text is not:\n{customers}");
    assert!(customers.contains("# character varying"), "the warehouse's spelling rides along:\n{customers}");
    let orders = std::fs::read_to_string(project.join("models/shop/tbl.orders.yml")).unwrap();
    assert!(orders.contains("sql: sum(total)") && orders.contains("# numeric"), "{orders}");
    assert!(orders.contains("cost: 1 ") && orders.contains("rows"), "cost from the estimate:\n{orders}");
    let rel = std::fs::read_to_string(project.join("models/shop/rel.shop.yml")).unwrap();
    assert!(rel.contains("left.customer_id = right.id"), "{rel}");

    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("1 joins"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["sql", "--expr", "name, total"]));
    assert!(ok && out.contains("shop.orders"), "{out}");
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}

/// The shapes the second run has to live with: a model whose list sits
/// at the margin, a table with two keys to one other, and an existing
/// model spelled with its schema.
#[test]
fn generate_keeps_a_margin_list_loadable_and_names_two_keys_to_one_table() {
    let (home, project) = scratch("sqlite-shapes");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n").unwrap();
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE customers (id INTEGER PRIMARY KEY, name VARCHAR(80) NOT NULL);\
         CREATE TABLE orders (id INTEGER PRIMARY KEY, bill_to_id INTEGER REFERENCES customers(id), \
           ship_to_id INTEGER REFERENCES customers(id), total NUMERIC(12,2) NOT NULL);",
    )
    .unwrap();
    assert!(run(zsql(&home, &project).args(["catalog", "read", "-d", "local", "--all"])).0);

    // An existing model, written by hand: the list at the margin, the name qualified.
    std::fs::create_dir_all(project.join("models/core")).unwrap();
    std::fs::write(
        project.join("models/core/tbl.orders.yml"),
        "name: Orders\nphysical_name: main.orders\ndatasource: local\ncost: 1\nfields:\n\
         - type: dimension\n  name: Id\n  data_type: integer\n  expression:\n    sql: id\n    primary_key: true\n",
    )
    .unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(ok, "{out}");
    assert!(!out.contains("created models/local/tbl.orders.yml"), "the qualified name matched the catalog's bare one: {out}");
    assert!(out.contains("3 new columns") || out.contains("new columns"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all", "--new-columns"]));
    assert!(ok && out.contains("added"), "{out}");
    let orders = std::fs::read_to_string(project.join("models/core/tbl.orders.yml")).unwrap();
    assert!(orders.contains("\n- type: measure") && orders.contains("\n  name: Total\n"), "appended at the list's own indent:\n{orders}");
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "still loads:\n{out}");

    // Two keys to one table: one join, and the other written down as the
    // role it needs, since the model holds one join per pair of tables.
    let rel = std::fs::read_to_string(project.join("models/local/rel.local.yml")).unwrap();
    assert_eq!(rel.matches("orders_customers:").count(), 1, "{rel}");
    assert!(rel.contains("# orders also references customers through") && rel.contains("#   sql: left."), "{rel}");
    assert!(rel.contains("left.bill_to_id = right.id") && rel.contains("left.ship_to_id = right.id"), "neither key is lost: {rel}");
    assert!(ok && out.contains("1 joins"), "{out}");

    // The declared width turns sampling on for a short string, even on SQLite.
    let customers = std::fs::read_to_string(project.join("models/local/tbl.customers.yml")).unwrap();
    assert!(customers.contains("enable_sampling: true"), "VARCHAR(80) is a short string:\n{customers}");
}
