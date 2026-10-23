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

fn parses(label: &str, text: &str) -> serde_json::Value {
    serde_json::from_str(text).unwrap_or_else(|e| panic!("{label} did not answer in JSON ({e}):\n{text}"))
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

    // And with the global --agent instead, which implies it.
    for args in [
        vec!["--agent", "check"],
        vec!["--agent", "test"],
        vec!["--agent", "catalog"],
        vec!["--agent", "datasource", "list"],
        vec!["--agent", "sample", "--dry-run"],
        vec!["--agent", "server", "status"],
    ] {
        let label = args.join(" ");
        let (ok, text) = out(zsql(&home, &project).args(&args));
        assert!(ok, "{label} failed:\n{text}");
        parses(&label, &text);
    }
}
