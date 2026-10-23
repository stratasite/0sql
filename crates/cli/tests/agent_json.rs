//! Every command an agent drives answers in JSON, either with `--json` or
//! under the global `--agent`. The ones that print only lines are the ones
//! an agent cannot use, so this walks each of them and parses the output.

use std::path::Path;
use std::process::Command;

fn zsql(home: &Path, project: &Path) -> Command {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_zsql"));
    cmd.env("ZSQL_HOME", home).env_remove("ZSQL_SERVER").env_remove("ZSQL_API_KEY").current_dir(project);
    cmd
}

/// stdout alone: JSON goes there, and notes and warnings go to stderr.
fn out(cmd: &mut Command) -> (bool, String) {
    let out = cmd.output().unwrap();
    eprintln!("$ {:?}\n{}", cmd.get_args().collect::<Vec<_>>(), String::from_utf8_lossy(&out.stderr));
    (out.status.success(), String::from_utf8_lossy(&out.stdout).to_string())
}

/// Parses the whole of stdout as one JSON document, which also proves
/// there is exactly one: `from_str` refuses trailing content, so a command
/// that printed its answer and then an error would fail here.
fn parses(label: &str, text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{label} did not answer in one JSON document ({e}):\n{text}"))
}

#[test]
fn the_commands_an_agent_drives_all_answer_in_json() {
    let root = std::env::temp_dir().join(format!("zsql-agent-json-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let (home, project) = (root.join("home"), root.join("shop"));
    std::fs::create_dir_all(&home).unwrap();
    std::fs::create_dir_all(&project).unwrap();
    assert!(out(zsql(&home, &project).args(["init", "--name", "Shop", "--uid", "shop", "--no-git"])).0);
    std::fs::write(project.join("datasources.yml"), "local:\n  adapter: sqlite\n  tier: hot\n  file: ./shop.db\n").unwrap();
    std::fs::create_dir_all(project.join("models")).unwrap();
    std::fs::write(
        project.join("models/tbl.orders.yml"),
        "name: Orders\nphysical_name: orders\ndatasource: local\ncost: 1\nfields:\n  - type: dimension\n    name: Status\n    data_type: string\n    expression: status\n  - type: measure\n    name: Orders\n    data_type: integer\n    expression: count(*)\n",
    )
    .unwrap();

    // With --json on the command.
    let (ok, text) = out(zsql(&home, &project).args(["check", "--json"]));
    let v = parses("check --json", &text);
    assert!(ok && v["tables"] == 1, "{v}");

    let (ok, text) = out(zsql(&home, &project).args(["test", "--json"]));
    assert!(ok, "{text}");
    assert!(parses("test --json", &text)["failed"] == 0, "{text}");

    let (ok, text) = out(zsql(&home, &project).args(["catalog", "--json"]));
    let v = parses("catalog --json", &text);
    assert!(ok && v["datasources"][0]["datasource"] == "local", "{v}");

    let (ok, text) = out(zsql(&home, &project).args(["datasource", "list", "--json"]));
    let v = parses("datasource list --json", &text);
    assert!(ok && v["datasources"][0]["adapter"] == "sqlite", "{v}");

    let (ok, text) = out(zsql(&home, &project).args(["sample", "--dry-run", "--json"]));
    let v = parses("sample --json", &text);
    assert!(ok && v["written"] == false, "{v}");

    let (ok, text) = out(zsql(&home, &project).args(["server", "status", "--json"]));
    let v = parses("server status --json", &text);
    assert!(ok && v["running"] == false, "{v}");

    let (ok, text) = out(zsql(&home, &project).args(["spec", "status, orders"]));
    let v = parses("spec", &text);
    assert!(ok && v["projections"][0]["field"] == "status", "{v}");

    // A command that answers and then fails still prints one document,
    // with the failure inside it, and still exits non-zero.
    std::fs::create_dir_all(project.join("tests")).unwrap();
    std::fs::write(project.join("tests/fails.yml"), "name: fails\nprojections:\n  - Status\nassert_regex: ZZZ_NEVER_MATCHES\n").unwrap();
    for args in [vec!["test", "--json"], vec!["check", "--json"]] {
        let label = args.join(" ");
        let (ok, text) = out(zsql(&home, &project).args(&args));
        assert!(!ok, "{label} must fail when a test fails:\n{text}");
        let v = parses(&label, &text);
        assert_eq!(v["error"]["message"], "tests failed", "the failure is in the document: {v}");
    }
    // The text path keeps its own exit code and says the same thing.
    let (ok, text) = out(zsql(&home, &project).arg("test"));
    assert!(!ok, "{text}");
    std::fs::remove_file(project.join("tests/fails.yml")).unwrap();

    // The catalog family answers in JSON too, including the read that finds
    // a name the warehouse does not have.
    let db = rusqlite::Connection::open(project.join("shop.db")).unwrap();
    // The columns the model maps, so the catalog check stays happy.
    db.execute_batch("CREATE TABLE orders (status TEXT);").unwrap();
    drop(db);
    let (ok, text) = out(zsql(&home, &project).args(["catalog", "read", "orders", "--json"]));
    let v = parses("catalog read --json", &text);
    assert!(ok && v["added"][0] == "orders", "{v}");
    let (ok, text) = out(zsql(&home, &project).args(["catalog", "sync", "--json"]));
    assert!(ok, "{text}");
    parses("catalog sync --json", &text);
    let (ok, text) = out(zsql(&home, &project).args(["catalog", "read", "ordrs", "--json"]));
    let v = parses("catalog read a typo --json", &text);
    assert!(!ok, "a name that is not there fails: {v}");
    assert!(v["not_found"][0] == "ordrs" && v["error"]["message"].is_string(), "one document carries both: {v}");

    // A refusal goes to stdout as JSON too, which is the documented
    // contract and the only thing an agent can act on. Checked from a
    // directory with no project in it.
    let empty = root.join("not-a-project");
    std::fs::create_dir_all(&empty).unwrap();
    for args in [
        vec!["check", "--json"],
        vec!["test", "--json"],
        vec!["sample", "--json"],
        vec!["catalog", "--json"],
        vec!["datasource", "list", "--json"],
    ] {
        let label = args.join(" ");
        let (ok, text) = out(zsql(&home, &empty).args(&args));
        assert!(!ok, "{label} should refuse outside a project:\n{text}");
        let v = parses(&format!("{label} refusal"), &text);
        assert!(v["error"]["message"].is_string(), "{label} refusal has the error shape: {v}");
    }

    // And with the global --agent instead, which implies it.
    for args in [
        vec!["--agent", "check"],
        vec!["--agent", "test"],
        vec!["--agent", "catalog"],
        vec!["--agent", "datasource", "list"],
        vec!["--agent", "sample", "--dry-run"],
        vec!["--agent", "server", "status"],
        vec!["--agent", "catalog", "sync"],
    ] {
        let label = args.join(" ");
        let (ok, text) = out(zsql(&home, &project).args(&args));
        assert!(ok, "{label} failed:\n{text}");
        parses(&label, &text);
    }
}
