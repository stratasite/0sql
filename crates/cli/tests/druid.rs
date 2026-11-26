//! Druid end to end. Planning connects to nothing, so the SQL it is
//! given is checked here on every run; running it needs a cluster, the
//! profile `druid` in harness/sources/compose.yml with the tables the
//! datasource crate's live Druid test loads, and is ignored without one.

use std::path::{Path, PathBuf};
use std::process::Command;

fn zsql(home: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zsql"));
    cmd.env("ZSQL_HOME", home).env_remove("ZSQL_SERVER").env_remove("ZSQL_RESULTS").env_remove("ZSQL_RESULTS_DIR");
    cmd.current_dir(project);
    cmd
}

fn run(cmd: &mut Command) -> (bool, String, String) {
    let out = cmd.output().unwrap();
    let (stdout, stderr) = (String::from_utf8_lossy(&out.stdout).to_string(), String::from_utf8_lossy(&out.stderr).to_string());
    eprintln!("$ {:?}\n{stdout}{stderr}", cmd.get_args().collect::<Vec<_>>());
    (out.status.success(), stdout, stderr)
}

/// A project of posts, users and tags on one Druid datasource at `url`,
/// the tables the live test loads, joined as `generate --infer` joins them.
fn project(name: &str, url: &str) -> (PathBuf, PathBuf, PathBuf) {
    let root = std::env::temp_dir().join(format!("zsql-druid-{name}-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("blog"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(project.join("models/blog")).unwrap();
    assert!(run(zsql(&home, &project).args(["init", "--name", "Blog", "--uid", "blog"])).0);
    std::fs::write(project.join("datasources.yml"), format!("dr:\n  adapter: druid\n  tier: hot\n  url: {url}\n  sslmode: disable\n"))
        .unwrap();
    let dim =
        |name: &str, ty: &str, sql: &str| format!("- type: dimension\n  name: {name}\n  data_type: {ty}\n  expression:\n    sql: {sql}\n");
    let msr = |name: &str, sql: &str| format!("- type: measure\n  name: {name}\n  data_type: bigint\n  expression:\n    sql: {sql}\n");
    let table = |file: &str, name: &str, physical: &str, fields: Vec<String>| {
        let body = format!("name: {name}\nphysical_name: {physical}\ndatasource: dr\ncost: 1\nfields:\n{}", fields.concat());
        std::fs::write(project.join(format!("models/blog/tbl.{file}.yml")), body).unwrap();
    };
    table(
        "posts",
        "Posts",
        "druid.posts",
        vec![
            dim("Posted At", "date_time", "__time"),
            dim("Post Id", "bigint", "id"),
            dim("Author Id", "bigint", "user_id"),
            dim("Title", "string", "title"),
            msr("Total Views", "sum(views)"),
            msr("Post Count", "count(id)"),
        ],
    );
    table("users", "Users", "druid.users", vec![dim("User Id", "bigint", "id"), dim("User Name", "string", "name")]);
    table(
        "post_tags",
        "Post Tags",
        "druid.post_tags",
        vec![dim("Tagged Post", "bigint", "post_id"), dim("Tag", "string", "tag"), msr("Tag Count", "count(tag)")],
    );
    std::fs::write(
        project.join("models/blog/rel.blog.yml"),
        "datasource: dr\n\nposts_users:\n  left: Posts\n  right: Users\n  sql: \"left.user_id = right.id\"\n  cardinality: many_to_one\n  join: left\n\n\
         post_tags_posts:\n  left: Post Tags\n  right: Posts\n  sql: \"left.post_id = right.id\"\n  cardinality: many_to_one\n  join: left\n",
    )
    .unwrap();
    assert!(run(zsql(&home, &project).arg("check")).0, "the fixture model loads");
    (root, home, project)
}

/// Druid takes no CTE, so measures of two tables merge as subqueries
/// under one full join; and a range of days starts at the first day's
/// midnight, where only the end is carried to the last second of its day.
#[test]
fn druid_is_planned_as_subqueries_and_a_range_starts_at_its_first_day() {
    let (root, home, project) = project("plan", "http://127.0.0.1:9");
    let (ok, out, err) = run(zsql(&home, &project).args(["sql", "--expr", "User Name, Total Views, Tag Count"]));
    assert!(ok, "{out}{err}");
    assert!(!out.contains("WITH"), "no CTE on Druid: {out}");
    assert!(out.contains("FULL OUTER JOIN ("), "the two measures merge as subqueries: {out}");

    let (ok, out, err) =
        run(zsql(&home, &project).args(["sql", "--expr", "Posted At, Total Views, Posted At between 2024-01-01 and 2024-01-02"]));
    assert!(ok, "{out}{err}");
    assert!(out.contains("BETWEEN '2024-01-01' AND TIMESTAMP '2024-01-02 23:59:59'"), "{out}");
    let _ = std::fs::remove_dir_all(&root);
}

/// The same queries run on a real cluster: one table, a join, a merge of
/// two tables' measures under a full join, and a range of days.
#[test]
#[ignore]
fn a_druid_model_runs_its_joins_merges_and_ranges() {
    let url = std::env::var("ZSQL_TEST_DRUID_URL").expect("ZSQL_TEST_DRUID_URL");
    let (root, home, project) = project("run", &url);
    let answer = |expr: &str| -> String {
        let (ok, out, err) = run(zsql(&home, &project).args(["run", "--fresh", "--format", "csv", "--expr", expr]));
        assert!(ok, "{expr}: {out}{err}");
        out
    };
    assert_eq!(answer("Total Views, Post Count").trim(), "Total Views,Post Count\n42,4");
    let joined = answer("User Name, Total Views, Post Count");
    for row in ["Bob Johnson,7,1", "Jane Smith,25,1", "John Doe,10,2"] {
        assert!(joined.contains(row), "{row}: {joined}");
    }
    let merged = answer("User Name, Total Views, Tag Count");
    for row in ["Bob Johnson,7,", "Jane Smith,25,1", "John Doe,10,1"] {
        assert!(merged.lines().any(|l| l == row), "{row}: {merged}");
    }
    // The first day's two posts are inside the range.
    let ranged = answer("Total Views, Posted At between 2024-01-01 and 2024-01-02");
    assert_eq!(ranged.trim(), "Total Views\n35", "{ranged}");
    let _ = std::fs::remove_dir_all(&root);
}
