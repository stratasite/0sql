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
         - type: dimension\n  name: Quiet\n  data_type: string\n  expression:\n    sql: region\n\
         - type: measure\n  name: Total\n  data_type: decimal\n  expression:\n    sql: sum(total)\n",
    )
    .unwrap();
    assert!(run(zsql(&home, &project).arg("check")).0, "the fixture model loads");

    // Say the plan first.
    let (ok, out) = run(zsql(&home, &project).args(["sample", "--dry-run"]));
    assert!(ok, "{out}");
    assert!(
        out.contains("would sample Status from orders (generated)") && out.contains("would sample Region from orders (sample_sql)"),
        "{out}"
    );
    assert!(out.contains("skip    Secret (Orders): hidden"), "{out}");
    assert!(out.contains("skip    Shouting (Orders): a calculated dimension"), "{out}");
    assert!(!out.contains("Quiet"), "a dimension that did not ask is not mentioned: {out}");
    assert!(!project.join("samples/values.jsonl.gz").exists(), "dry run writes nothing");
    let (ok, out) = run(zsql(&home, &project).args(["sample", "--budget", "1"]));
    assert!(!ok && out.contains("more than the budget of 1"), "{out}");

    // Then do it.
    let (ok, out) = run(zsql(&home, &project).arg("sample"));
    assert!(ok, "{out}");
    assert!(out.contains("sampled Status (Orders): 2 values of 3, cut at the limit"), "{out}");
    assert!(out.contains("sampled Region (Orders): 3 values"), "{out}");
    assert!(out.contains("warning: Notes (Orders): the values average") && out.contains("text, not a value"), "{out}");
    assert!(!out.contains("warning: Status"), "{out}");

    let cat = zsql_project::values::ValueCatalog::read(&project.join("samples/values.jsonl.gz")).unwrap().expect("written");
    let by = |name: &str| {
        cat.dimensions
            .iter()
            .find(|d| d.name == name)
            .unwrap_or_else(|| panic!("{name} in {:?}", cat.dimensions.iter().map(|d| &d.name).collect::<Vec<_>>()))
    };
    let status = by("Status");
    assert_eq!(
        status.values.iter().map(|v| (v.v.as_str(), v.n)).collect::<Vec<_>>(),
        [("paid", 3), ("open", 1)],
        "most frequent first, cut at sample_size"
    );
    assert_eq!(status.distinct, Some(3));
    assert_eq!(status.sample_size, Some(2));
    assert!(status.sampled_at.ends_with('Z'));
    let region = by("Region");
    assert_eq!(region.values[0].v, "EAST", "the author's query as written");
    assert_eq!(region.values[0].n, 3);
    assert!(by("Notes").values.len() == 5);
    assert!(cat.dimensions.iter().all(|d| d.name != "Secret" && d.name != "Shouting" && d.name != "Quiet"));

    // --only touches one dimension and keeps the rest.
    db.execute_batch("UPDATE orders SET status = 'open' WHERE status = 'refunded'").unwrap();
    let (ok, out) = run(zsql(&home, &project).args(["sample", "--only", "status"]));
    assert!(ok && out.contains("sampled Status") && !out.contains("sampled Region"), "{out}");
    let cat = zsql_project::values::ValueCatalog::read(&project.join("samples/values.jsonl.gz")).unwrap().unwrap();
    assert_eq!(cat.dimensions.len(), 3, "the others are kept");
    assert_eq!(cat.dimensions.iter().find(|d| d.name == "Status").unwrap().distinct, Some(2));

    // And the planner serves them.
    let (ok, out) = run(zsql(&home, &project).args(["values", "Status"]));
    assert!(ok && out.contains("paid"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}
