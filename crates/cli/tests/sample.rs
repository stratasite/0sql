//! `zsql sample`, end to end on SQLite: a sampled string comes back most
//! frequent first with its counts and its distinct total, the author's
//! own sample_sql is run as given, a hidden dimension and a calculated
//! one are skipped with their reasons, and long text is warned about.

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

#[test]
fn sampling_writes_what_the_model_asks_for_and_says_why_not_otherwise() {
    let root = std::env::temp_dir().join(format!("zsql-sample-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    assert!(run(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n").unwrap();
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    db.execute_batch(
        "CREATE TABLE orders (id INTEGER PRIMARY KEY, status TEXT, region TEXT, notes TEXT, total REAL);\
         INSERT INTO orders (status, region, notes, total) VALUES \
           ('paid', 'east', 'a long note about this order that goes on and on and on and on and on and on and on and on and on and on and on', 1),\
           ('paid', 'west', 'another long note about this order that goes on and on and on and on and on and on and on and on and on and on', 2),\
           ('paid', 'east', 'yet another long note about this order that goes on and on and on and on and on and on and on and on and on', 3),\
           ('open', 'east', 'a fourth long note about this order that goes on and on and on and on and on and on and on and on and on and on', 4),\
           ('refunded', 'north', 'a fifth long note about this order that goes on and on and on and on and on and on and on and on and on and on and on', 5);",
    )
    .unwrap();
    std::fs::create_dir_all(project.join("models/core")).unwrap();
    std::fs::write(
        project.join("models/core/tbl.orders.yml"),
        "name: Orders\nphysical_name: orders\ndatasource: local\ncost: 1\nfields:\n\
         - type: dimension\n  name: Status\n  data_type: string\n  enable_sampling: true\n  sample_size: 2\n  expression:\n    sql: status\n\
         - type: dimension\n  name: Region\n  data_type: string\n  enable_sampling: true\n  sample_sql: SELECT upper(region), count(*) FROM orders GROUP BY 1 ORDER BY 2 DESC\n  expression:\n    sql: region\n\
         - type: dimension\n  name: Notes\n  data_type: string\n  enable_sampling: true\n  expression:\n    sql: notes\n\
         - type: dimension\n  name: Secret\n  data_type: string\n  hidden: true\n  enable_sampling: true\n  expression:\n    sql: status\n\
         - type: dimension\n  name: Shouting\n  data_type: string\n  enable_sampling: true\n  expression:\n    sql: \"status || [Quiet]\"\n\
         - type: dimension\n  name: Tied\n  data_type: string\n  enable_sampling: true\n  sample_size: 3\n  expression:\n    sql: status\n\
         - type: dimension\n  name: Quiet\n  data_type: string\n  expression:\n    sql: region\n\
         - type: measure\n  name: Total\n  data_type: decimal\n  expression:\n    sql: sum(total)\n",
    )
    .unwrap();
    assert!(run(zsql(&home, &project).arg("check")).0, "the fixture model loads");

    // Say the plan first.
    let (ok, out) = run(zsql(&home, &project).args(["sample", "--dry-run"]));
    assert!(ok, "{out}");
    assert!(out.contains("would sample Status (planned)") && out.contains("would sample Region (sample_sql)"), "{out}");
    assert!(out.contains("skip    Secret: hidden"), "{out}");
    assert!(out.contains("would sample Shouting (planned)"), "a calculated dimension is the planner's to answer: {out}");
    assert!(!out.contains("Quiet"), "a dimension that did not ask is not mentioned: {out}");
    assert!(!project.join("samples/values.jsonl.gz").exists(), "dry run writes nothing");
    let (ok, out) = run(zsql(&home, &project).args(["sample", "--budget", "1"]));
    assert!(!ok && out.contains("more than the budget of 1"), "{out}");

    // Then do it.
    let (ok, out) = run(zsql(&home, &project).arg("sample"));
    assert!(ok, "{out}");
    assert!(out.contains("sampled Status (local): 2 values of 3, cut at the limit"), "{out}");
    assert!(out.contains("sampled Region (local): 3 values"), "{out}");
    assert!(out.contains("sampled Shouting (local):"), "{out}");
    assert!(out.contains("warning: Notes: the values average") && out.contains("text, not a value"), "{out}");
    assert!(!out.contains("warning: Status"), "{out}");

    let cat = zsql_project::values::ValueCatalog::read(&project.join("samples/values.jsonl.gz")).unwrap().expect("written");
    let by = |name: &str| {
        cat.dimensions
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} in {:?}", cat.dimensions.iter().map(|d| &d.name).collect::<Vec<_>>()))
    };
    let status = by("Status");
    // Most frequent first, cut at sample_size. Which of the two singletons
    // survives the cut is the warehouse's choice, since the planner orders
    // by the count alone.
    let got: Vec<(&str, i64)> = status.values.iter().map(|v| (v.v.as_str(), v.n)).collect();
    assert_eq!(got.len(), 2, "{got:?}");
    assert_eq!(got[0], ("paid", 3), "{got:?}");
    assert!(got[1] == ("open", 1) || got[1] == ("refunded", 1), "{got:?}");
    assert_eq!(status.distinct, Some(3));
    assert_eq!(status.sample_size, Some(2));
    assert!(status.sampled_at.ends_with('Z'));
    let region = by("Region");
    assert_eq!(region.values[0].v, "EAST", "the author's query as written");
    assert_eq!(region.values[0].n, 3);
    assert_eq!(region.sample_size, None, "an author's query has no limit to be cut at");
    assert!(out.contains("sampled Region (local): 3 values\n"), "not cut: {out}");
    let tied: Vec<(&str, i64)> = by("Tied").values.iter().map(|v| (v.v.as_str(), v.n)).collect();
    assert_eq!(tied, [("paid", 3), ("open", 1), ("refunded", 1)], "sorted by count, then by value");
    assert!(by("Notes").values.len() == 5);
    assert!(cat.dimensions.iter().all(|d| d.name != "Secret" && d.name != "Quiet"));
    let shouting = by("Shouting");
    assert_eq!(shouting.values[0].v, "paideast", "status || region, planned by the planner");

    // --only touches one dimension and keeps the rest.
    db.execute_batch("UPDATE orders SET status = 'open' WHERE status = 'refunded'").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["sample", "--only", "status"]));
    assert!(ok && out.contains("sampled Status") && !out.contains("sampled Region"), "{out}");
    let cat = zsql_project::values::ValueCatalog::read(&project.join("samples/values.jsonl.gz")).unwrap().unwrap();
    assert_eq!(cat.dimensions.len(), 5, "the others are kept");
    assert_eq!(cat.dimensions.iter().find(|d| d.name == "Status").unwrap().distinct, Some(2));

    // And the planner serves them.
    let (ok, out) = run(zsql(&home, &project).args(["values", "Status"]));
    assert!(ok && out.contains("paid"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// Sampling the Postgres fixture through a model `generate` drafted from
/// it: the width it inferred sampling from, the real values most frequent
/// first with their distinct total, and the warning the catalog's
/// declared type raises when a text column is turned on by hand.
/// `ZSQL_TEST_PG_*` as the sources job sets them; ignored otherwise.
#[test]
#[ignore]
fn sampling_a_postgres_fixture_through_a_generated_model() {
    let root = std::env::temp_dir().join(format!("zsql-sample-pg-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
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
    assert!(run(zsql(&home, &project).args(["catalog", "wh", "--schema", "shop"])).0);
    assert!(run(zsql(&home, &project).args(["generate", "wh"])).0);

    // The draft turned sampling on for name (varchar(80)) and not for notes (text).
    let (ok, out) = run(zsql(&home, &project).args(["sample", "wh"]));
    assert!(ok, "{out}");
    assert!(out.contains("sampled Name (wh): 2 values of 2"), "{out}");
    assert!(!out.contains("Notes"), "text was not turned on by the draft: {out}");
    assert!(!out.contains("warning:"), "an 80-wide varchar raises nothing: {out}");
    let cat = zsql_project::values::ValueCatalog::read(&project.join("samples/values.jsonl.gz")).unwrap().expect("written");
    let name = cat.dimensions.iter().find(|d| d.name == "Name").expect("Name");
    assert_eq!(name.values.iter().map(|v| (v.v.as_str(), v.n)).collect::<Vec<_>>(), [("Ann", 1), ("Bo", 1)], "ties by value");
    assert_eq!(name.distinct, Some(2));
    assert_eq!(name.sample_size, Some(1000));

    // Turn the text column on by hand: the catalog's declared type warns.
    let path = project.join("models/shop/tbl.customers.yml");
    let yml = std::fs::read_to_string(&path).unwrap();
    let on = yml.replacen("    name: Notes\n", "    name: Notes\n    enable_sampling: true\n", 1);
    assert_ne!(on, yml, "Notes is in the draft");
    std::fs::write(&path, on).unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["sample", "wh", "--only", "notes"]));
    assert!(ok, "{out}");
    assert!(out.contains("warning: Notes (Customers): declared text, unbounded text"), "{out}");
    let (ok, out) = run(zsql(&home, &project).args(["values", "Name"]));
    assert!(ok && out.contains("Ann"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}
