//! `zsql introspect` then `zsql generate`, end to end: a draft the real
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
    let (ok, out) = run(zsql(&home, &project).args(["introspect", "local"]));
    assert!(ok && out.contains("2 tables"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "local"]));
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
    let (ok, out) = run(zsql(&home, &project).args(["generate", "local"]));
    assert!(ok && out.contains("nothing to do"), "{out}");
    assert!(out.contains("no 0sql type, so not a field: tags"), "{out}");
    assert_eq!(std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap(), before, "never rewritten");

    // The warehouse grows two columns: listed with the command to add them.
    db.execute_batch("ALTER TABLE orders ADD COLUMN status VARCHAR(20); ALTER TABLE orders ADD COLUMN discount NUMERIC(6,2);").unwrap();
    assert!(run(zsql(&home, &project).args(["introspect", "local"])).0);
    let (ok, out) = run(zsql(&home, &project).args(["generate", "local"]));
    assert!(ok && out.contains("2 new columns: status, discount"), "{out}");
    assert!(out.contains("--table orders --columns status,discount"), "the exact command is printed: {out}");
    assert_eq!(std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap(), before, "listing changes nothing");

    // One by name, then the rest; the file keeps everything it had.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "local", "--table", "orders", "--columns", "discount"]));
    assert!(ok && out.contains("added   1 field"), "{out}");
    let after = std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap();
    assert!(after.starts_with(&before), "appended, not rewritten");
    assert!(after.contains("sql: sum(discount)") && !after.contains("sql: status"), "{after}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "local", "--new"]));
    assert!(ok && out.contains("added   1 field"), "{out}");
    let after = std::fs::read_to_string(project.join("models/local/tbl.orders.yml")).unwrap();
    assert!(after.contains("sql: status"), "{after}");
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("2 tables"), "still loads: {out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "local"]));
    assert!(ok && out.contains("nothing to do"), "{out}");

    // --dry-run writes nothing, for a new table too.
    db.execute_batch("CREATE TABLE products (id INTEGER PRIMARY KEY, title TEXT);").unwrap();
    assert!(run(zsql(&home, &project).args(["introspect", "local"])).0);
    let (ok, out) = run(zsql(&home, &project).args(["generate", "local", "--dry-run"]));
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
            var("ZSQL_TEST_PG_PORT", "5432"),
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

    let (ok, out) = run(zsql(&home, &project).args(["introspect", "wh"]));
    assert!(ok && out.contains("in shop"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "wh"]));
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
