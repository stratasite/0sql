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
    // name is varchar(80), a short string, and looks like a person: the
    // width rule writes the line, the identity rule comments it out.
    assert!(
        customers.contains("# enable_sampling: true") && customers.contains("80 wide, but looks like a person"),
        "varchar(80) named name is left to the author:\n{customers}"
    );
    assert_eq!(customers.matches("\n    enable_sampling: true").count(), 0, "text is not sampled either:\n{customers}");
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

/// Two defects a real grocery-store model found. A column named for a
/// reserved word (`DAY`) must be quoted, or the dimension's expression
/// parses to no column and the loader refuses the file. And a column
/// name on two tables is one field in the model, so the two drafts must
/// declare one type: the loader keeps the first it reads and `check`
/// would report the other table as a mismatch.
#[test]
fn a_reserved_column_is_quoted_and_one_field_name_gets_one_type() {
    let (home, project) = scratch("reserved");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop", "--no-git"])).0);
    std::fs::write(project.join("datasources.yml"), "wh:\n  adapter: duckdb\n  tier: hot\n  file: ./shop.duckdb\n").unwrap();
    let db = duckdb::Connection::open(project.join("shop.duckdb")).unwrap();
    db.execute_batch(
        "CREATE TABLE fact_transactions (BASKET_ID BIGINT, \"DAY\" SMALLINT, WEEK_NO TINYINT, PRODUCT_ID INTEGER, \"Gross Sales\" DOUBLE, LABEL VARCHAR);\
         CREATE TABLE dim_date (Week_No BIGINT, AnchorDate DATE);",
    )
    .unwrap();
    drop(db);
    assert!(run(zsql(&home, &project).args(["catalog", "read", "--all"])).0);
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all"]));
    assert!(ok, "{out}");

    let facts = std::fs::read_to_string(project.join("models/main/tbl.fact_transactions.yml")).unwrap();
    // The reserved word and the name holding a space are quoted the way
    // DuckDB quotes an identifier; the plain ones are left bare.
    assert!(facts.contains(r#"sql: "\"DAY\"""#), "a reserved column is quoted:\n{facts}");
    assert!(facts.contains(r#"sql: "\"Gross Sales\"""#), "a name with a space is quoted:\n{facts}");
    assert!(facts.contains("sql: BASKET_ID"), "a plain name stays bare:\n{facts}");

    // WEEK_NO is TINYINT here and Week_No is BIGINT there: one field, so
    // the wider type is declared on both, and the line says why.
    let dates = std::fs::read_to_string(project.join("models/main/tbl.dim_date.yml")).unwrap();
    assert_eq!(facts.matches("name: Week No").count(), 1, "{facts}");
    assert_eq!(dates.matches("name: Week No").count(), 1, "{dates}");
    for (file, text) in [("facts", &facts), ("dates", &dates)] {
        let line = text.lines().skip_while(|l| !l.contains("name: Week No")).nth(1).unwrap_or("");
        assert!(line.contains("data_type: bigint"), "{file}: the wider type on both: {line}");
    }
    assert!(facts.contains("one field across tables, so the wider of"), "the draft says why:\n{facts}");

    // Which is the whole point: the model loads, and check finds no
    // disagreement between what it declares and what the warehouse says.
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("2 tables"), "{out}");
    assert!(!out.contains("and its column"), "no type finding against the catalog:\n{out}");

    // check judges a field against every column it maps to, at once, and
    // names the type to declare. The loader keeps the first declaration
    // it reads, so narrowing the field means narrowing both files.
    let dates = project.join("models/main/tbl.dim_date.yml");
    let facts_path = project.join("models/main/tbl.fact_transactions.yml");
    let retype = |to: &str| {
        for p in [&dates, &facts_path] {
            let yml = std::fs::read_to_string(p).unwrap();
            let lines: Vec<String> = yml
                .lines()
                .scan(false, |seen_week, l| {
                    if l.contains("name: Week No") {
                        *seen_week = true;
                        return Some(l.to_string());
                    }
                    if *seen_week && l.trim_start().starts_with("data_type:") {
                        *seen_week = false;
                        return Some(format!("    data_type: {to}"));
                    }
                    Some(l.to_string())
                })
                .collect();
            std::fs::write(p, lines.join("\n") + "\n").unwrap();
        }
    };
    retype("integer");
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "a type finding is advice, not a failure: {out}");
    assert_eq!(out.matches("Week No says").count(), 1, "one line per field, not per table:\n{out}");
    assert!(
        out.contains("Week No says integer, and its columns are")
            && out.contains("BIGINT on Dim Date")
            && out.contains("TINYINT on Fact Transactions")
            && out.contains("declare bigint"),
        "{out}"
    );
    // Wider than every column it maps to is said too, with the type to use.
    retype("decimal");
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("Week No says decimal") && out.contains("declare bigint"), "{out}");
    retype("bigint");
    assert!(!run(zsql(&home, &project).arg("check")).1.contains("Week No says"), "the widest is accepted");

    // Different kinds of thing: no declaration describes both, and the
    // line says so rather than naming a type.
    let yml = std::fs::read_to_string(&facts_path).unwrap();
    std::fs::write(&facts_path, yml.replacen("      sql: WEEK_NO", "      sql: LABEL", 1)).unwrap();
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok, "{out}");
    assert!(
        out.contains("Week No says bigint") && out.contains("VARCHAR on Fact Transactions") && out.contains("different kinds of thing"),
        "{out}"
    );
    std::fs::write(&facts_path, std::fs::read_to_string(&facts_path).unwrap().replacen("      sql: LABEL", "      sql: WEEK_NO", 1))
        .unwrap();

    // And the planner can use the reserved column.
    let (ok, out) = run(zsql(&home, &project).args(["sql", "day"]));
    assert!(ok && out.contains(r#""DAY""#), "{out}");
}

/// `--dir` and a trailing slash put the drafts where the model wants
/// them rather than where the warehouse keeps them, and
/// `--relationships --infer` writes the joins file from the table files
/// alone: a column that names another table joins to that table's key,
/// and a column that names nothing is left alone.
#[test]
fn a_target_directory_and_an_inferred_joins_file() {
    let (home, project) = scratch("relationships");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop", "--no-git"])).0);
    std::fs::write(project.join("datasources.yml"), "wh:\n  adapter: duckdb\n  tier: hot\n  file: ./shop.duckdb\n").unwrap();
    let db = duckdb::Connection::open(project.join("shop.duckdb")).unwrap();
    db.execute_batch(
        "CREATE SCHEMA retail;\
         CREATE TABLE retail.fact_transactions (household_key BIGINT, PRODUCT_ID BIGINT, COUPON_UPC BIGINT, \
           BASKET_ID BIGINT, WEEK_NO TINYINT, SALES_VALUE DOUBLE);\
         CREATE TABLE retail.dim_household (household_key BIGINT, AGE_DESC VARCHAR);\
         CREATE TABLE retail.dim_products (PRODUCT_ID BIGINT, BRAND VARCHAR);\
         CREATE TABLE retail.dim_coupon (COUPON_UPC BIGINT, campaign BIGINT);\
         CREATE TABLE retail.dim_date (Day_Idx BIGINT, Week_No BIGINT);",
    )
    .unwrap();
    drop(db);
    assert!(run(zsql(&home, &project).args(["catalog", "read", "--all"])).0);

    // The warehouse's schema is `retail`; the model's directory is ours.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all", "--dir", "shopping"]));
    assert!(ok && out.contains("created models/shopping/tbl.fact_transactions.yml"), "{out}");
    assert!(!project.join("models/retail").exists(), "not the warehouse's schema: {out}");
    // A trailing slash says the same thing.
    std::fs::remove_dir_all(project.join("models/shopping")).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["generate", "basket/", "--all"]));
    assert!(ok && out.contains("created models/basket/tbl.dim_coupon.yml"), "{out}");
    assert_eq!(std::fs::read_dir(project.join("models/basket")).unwrap().count(), 5, "every table, one directory");

    // The joins file, inferred from those files and nothing else.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--relationships", "basket/retail", "--infer"]));
    assert!(ok, "{out}");
    assert!(out.contains("created models/basket/rel.retail.yml (3 joins inferred)"), "{out}");
    let rel = std::fs::read_to_string(project.join("models/basket/rel.retail.yml")).unwrap();
    assert!(rel.contains("datasource: wh"), "{rel}");
    // The key by its name, the key by a plural table name, and a key
    // whose suffix nobody could enumerate, found because it names the table.
    assert!(rel.contains("left.household_key = right.household_key"), "{rel}");
    assert!(rel.contains("left.PRODUCT_ID = right.PRODUCT_ID"), "a plural table still owns its key:\n{rel}");
    assert!(rel.contains("left.COUPON_UPC = right.COUPON_UPC"), "a column that names the table:\n{rel}");
    assert!(rel.contains("cardinality: many_to_one") && rel.contains("join: left"), "a guess drops no rows:\n{rel}");
    // WEEK_NO is on the fact and the date dimension and keys neither, so
    // it is no join, and the file says the date dimension needs one.
    assert!(!rel.contains("week_no") && !rel.contains("WEEK_NO"), "a shared non-key is not a join:\n{rel}");
    assert!(rel.contains("Dim Date"), "the table left out is named:\n{rel}");
    assert!(rel.contains("BASKET_ID points at a basket"), "a key pointing nowhere is named:\n{rel}");
    // The forms stay in the file, under what was inferred.
    assert!(rel.contains("# Other forms") && rel.contains("allow_measure_expansion"), "{rel}");
    let (joins, examples) = (rel.find("household_key = right").unwrap(), rel.find("# Other forms").unwrap());
    assert!(joins < examples, "what was inferred comes first:\n{rel}");

    // It loads, with the joins the planner can use.
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("5 tables") && out.contains("3 joins"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["sql", "\"Age Desc\", sales value"]));
    assert!(ok && out.to_lowercase().contains("join") && out.contains("dim_household"), "a join across the inferred model:\n{out}");

    // Never rewritten, and the template is there without --infer.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--relationships", "basket/retail", "--infer"]));
    assert!(ok && out.contains("not touched"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--relationships", "basket/by-hand"]));
    assert!(ok && out.contains("the template"), "{out}");
    let blank = std::fs::read_to_string(project.join("models/basket/rel.by-hand.yml")).unwrap();
    assert!(blank.contains("datasource: wh") && blank.contains("cardinality: many_to_one"), "{blank}");
    // Nothing but comments and the datasource: the joins are the author's.
    assert!(
        blank.lines().all(|l| l.trim().is_empty() || l.trim_start().starts_with('#') || l.starts_with("datasource:")),
        "the template declares no join:\n{blank}"
    );
}

/// A warehouse that puts the kind on the end of a table's name rather than
/// the front, and a namespace in front of it: `cs_call_center_d` is a
/// dimension about a call center. Inference once stripped prefixes only,
/// and compared one word of a column against a whole table name, so it
/// found nothing at all here and then said each table was absent.
#[test]
fn suffixed_table_names_still_infer_their_joins() {
    let (home, project) = scratch("suffixes");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Service", "--uid", "svc", "--no-git"])).0);
    std::fs::write(project.join("datasources.yml"), "wh:\n  adapter: duckdb\n  file: ./svc.duckdb\n").unwrap();
    let dir = project.join("models/main");
    std::fs::create_dir_all(&dir).unwrap();

    let table = |file: &str, name: &str, physical: &str, fields: &str| {
        std::fs::write(
            dir.join(format!("tbl.{file}.yml")),
            format!("name: {name}\nphysical_name: {physical}\ndatasource: wh\ncost: 1\nfields:\n{fields}"),
        )
        .unwrap()
    };
    let key = |name: &str, column: &str, kind: &str| {
        format!(
            "  - type: dimension\n    name: {name}\n    data_type: {kind}\n    expression:\n      sql: {column}\n      primary_key: true\n"
        )
    };
    let plain = |name: &str, column: &str, kind: &str| {
        format!("  - type: dimension\n    name: {name}\n    data_type: {kind}\n    expression: {column}\n")
    };

    table(
        "cs_call_center_d",
        "Call Center",
        "main.cs_call_center_d",
        &format!("{}{}", key("Call Center Id", "call_center_id", "bigint"), plain("Call Center Name", "name", "string")),
    );
    table(
        "account_d",
        "Account",
        "main.account_d",
        &format!("{}{}", key("Account Id", "account_id", "bigint"), plain("Account Type", "account_type", "string")),
    );
    table(
        "cs_contact_f",
        "Contact",
        "main.cs_contact_f",
        &format!(
            "{}{}{}",
            plain("Contact Call Center Id", "call_center_id", "bigint"),
            plain("Contact Account Id", "account_id", "bigint"),
            "  - type: measure\n    name: Contacts\n    data_type: integer\n    expression: count(*)\n"
        ),
    );

    let (ok, out) = run(zsql(&home, &project).args(["generate", "--relationships", "main/service", "--infer"]));
    assert!(ok, "{out}");
    assert!(out.contains("(2 joins inferred)"), "both keys find their dimension: {out}");
    let rel = std::fs::read_to_string(dir.join("rel.service.yml")).unwrap();
    assert!(rel.contains("left.call_center_id = right.call_center_id"), "{rel}");
    assert!(rel.contains("left.account_id = right.account_id"), "{rel}");
    // And no line claiming a table that is right there is missing.
    assert!(!rel.contains("no table here"), "nothing is reported absent: {rel}");
    assert!(ok && run(zsql(&home, &project).arg("check")).0, "the inferred file loads");
}

/// Two schemas with a table of the same name. The model's name must not be
/// written twice, or the project will not load at all; the second one takes
/// its schema in front of it, and the file follows the name.
#[test]
fn two_schemas_sharing_a_table_name_get_two_names() {
    let (home, project) = scratch("collide");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop", "--no-git"])).0);
    std::fs::write(project.join("datasources.yml"), "wh:\n  adapter: duckdb\n  file: ./shop.duckdb\n").unwrap();
    let db = duckdb::Connection::open(project.join("shop.duckdb")).unwrap();
    db.execute_batch(
        "CREATE SCHEMA alpha; CREATE SCHEMA beta;\
         CREATE TABLE alpha.customer (customer_id BIGINT, name VARCHAR);\
         CREATE TABLE beta.customer (customer_id BIGINT, email VARCHAR);",
    )
    .unwrap();
    drop(db);
    assert!(run(zsql(&home, &project).args(["catalog", "read", "--all"])).0);

    // One directory, so the names have to do the work.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "--all", "--dir", "shop"]));
    assert!(ok, "{out}");
    let a = std::fs::read_to_string(project.join("models/shop/tbl.customer.yml")).unwrap();
    let b = std::fs::read_to_string(project.join("models/shop/tbl.beta_customer.yml")).unwrap();
    assert!(a.contains("name: Customer\n"), "the first keeps its name:\n{a}");
    assert!(b.contains("name: Beta Customer\n"), "the second takes its schema:\n{b}");
    assert!(a.contains("physical_name: alpha.customer") && b.contains("physical_name: beta.customer"), "{a}{b}");

    // Which is the point: it loads.
    let (ok, out) = run(zsql(&home, &project).arg("check"));
    assert!(ok && out.contains("2 tables"), "both tables load:\n{out}");
}

/// `--columns` names a column. One that is not there is a mistake and has
/// to fail, since an exit code is all an agent reads; one that is already a
/// field is the request already satisfied, and says so.
#[test]
fn a_column_asked_for_by_name_is_answered_either_way() {
    let (home, project) = scratch("columns");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop", "--no-git"])).0);
    std::fs::write(project.join("datasources.yml"), "wh:\n  adapter: duckdb\n  file: ./shop.duckdb\n").unwrap();
    let db = duckdb::Connection::open(project.join("shop.duckdb")).unwrap();
    db.execute_batch("CREATE TABLE orders (order_id BIGINT, qty_shipped BIGINT, total DOUBLE);").unwrap();
    drop(db);
    assert!(run(zsql(&home, &project).args(["catalog", "read", "--all"])).0);
    assert!(run(zsql(&home, &project).args(["generate", "orders"])).0);

    // A typo: named, and a non-zero exit.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "orders", "--columns", "qty_shipepd"]));
    assert!(!ok, "a column that is not there fails: {out}");
    assert!(out.contains("qty_shipepd"), "and is named: {out}");
    assert!(!out.contains("nothing to do"), "and is not reported as success: {out}");

    // Already a field: said, and nothing fails.
    let (ok, out) = run(zsql(&home, &project).args(["generate", "orders", "--columns", "qty_shipped"]));
    assert!(ok, "{out}");
    assert!(out.to_lowercase().contains("already"), "it says the model has it: {out}");
}

/// `--empty table` needs no warehouse, so it honours `--dry-run` like every
/// other write and works before a datasource is configured. Both were
/// silently ignored: the flag was accepted and the file written anyway, and
/// the command insisted on a datasource it never used.
#[test]
fn an_empty_scaffold_needs_no_warehouse_and_honours_dry_run() {
    let (home, project) = scratch("empty");
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop", "--no-git"])).0);
    // init writes a datasources.yml of comments only: nothing is configured.
    assert!(std::fs::read_to_string(project.join("datasources.yml"))
        .unwrap()
        .lines()
        .all(|l| l.trim().is_empty() || l.trim_start().starts_with('#')));

    let (ok, out) = run(zsql(&home, &project).args(["generate", "widgets", "--empty", "table", "--dry-run"]));
    assert!(ok, "{out}");
    assert!(out.contains("would create models/core/tbl.widgets.yml"), "{out}");
    assert!(!project.join("models/core/tbl.widgets.yml").exists(), "dry-run writes nothing: {out}");

    let (ok, out) = run(zsql(&home, &project).args(["generate", "widgets", "--empty", "table"]));
    assert!(ok && out.contains("create models/core/tbl.widgets.yml"), "{out}");
    let text = std::fs::read_to_string(project.join("models/core/tbl.widgets.yml")).unwrap();
    assert!(text.contains("name: widgets") && text.contains("datasource:"), "{text}");
    assert!(text.contains("the datasource's key"), "the blank says what to put there:\n{text}");

    // A datasources.yml that does not parse is an error, not "no datasource".
    std::fs::write(project.join("datasources.yml"), "local: [not a mapping\n").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["generate", "gadgets", "--empty", "table"]));
    assert!(!ok, "a broken file is reported, not worked around: {out}");
    assert!(out.contains("datasources.yml"), "{out}");
    assert!(!project.join("models/core/tbl.gadgets.yml").exists(), "and nothing is written: {out}");
    let _ = std::fs::remove_dir_all(home.parent().unwrap());
}
