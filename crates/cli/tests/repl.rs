//! The local repl, driven through a pipe: it holds the model, reloads
//! it when a file moves, and keeps the old one when the new one will
//! not load. `ZSQL_HOME` points at a scratch directory so nothing here
//! touches the real one.

use std::io::{BufRead, BufReader, Write};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

fn fixture() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects/grocery-store")
}

fn copy_dir(from: &Path, to: &Path) {
    std::fs::create_dir_all(to).unwrap();
    for e in std::fs::read_dir(from).unwrap().flatten() {
        let (src, dst) = (e.path(), to.join(e.file_name()));
        if src.is_dir() {
            copy_dir(&src, &dst);
        } else {
            std::fs::copy(&src, &dst).unwrap();
        }
    }
}

/// Reads lines until one holds `marker`, answering with all of them.
fn read_until(out: &mut impl BufRead, marker: &str) -> String {
    let mut text = String::new();
    loop {
        let mut line = String::new();
        if out.read_line(&mut line).unwrap() == 0 {
            panic!("the repl ended before {marker:?}; so far:\n{text}");
        }
        eprint!("repl| {line}");
        text.push_str(&line);
        if line.contains(marker) {
            return text;
        }
    }
}

#[test]
fn the_local_repl_holds_the_model_and_reloads_it() {
    let root = std::env::temp_dir().join(format!("zsql-repl-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let project = root.join("project");
    copy_dir(&fixture(), &project);
    let home = root.join("home");

    let mut child = Command::new(env!("CARGO_BIN_EXE_zsql"))
        .arg("repl")
        .current_dir(&project)
        .env("ZSQL_HOME", &home)
        .env_remove("ZSQL_SERVER")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .spawn()
        .expect("zsql starts");
    let mut stdin = child.stdin.take().unwrap();
    let mut out = BufReader::new(child.stdout.take().unwrap());

    let banner = read_until(&mut out, "type a line");
    assert!(banner.contains("grocery-store/working-tree here:"), "{banner}");
    assert!(banner.contains("loaded in"), "{banner}");

    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(answer.contains(r#"AS "Sales Value""#), "{answer}");

    // Point the measure at another column: the next line must read it.
    let table = project.join("models/transactions/tbl.transactions.yml");
    let text = std::fs::read_to_string(&table).unwrap();
    assert!(text.contains("sql: sum(SALES_VALUE)"));
    std::fs::write(&table, text.replace("sql: sum(SALES_VALUE)", "sql: sum(NET_SALES_VALUE)")).unwrap();
    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(answer.contains("reloaded in"), "{answer}");
    assert!(answer.contains(r#"sum(T0."net_sales_value")"#), "{answer}");

    // A file that did not exist before: the directory moved, so the
    // model loads again.
    std::fs::write(project.join("tests/added.yml"), "name: added\nprojections:\n  - Department\nassert_regex: SELECT\n").unwrap();
    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(answer.contains("reloaded in"), "{answer}");

    // A new file that does not load: the error is said once, and when
    // that file is fixed the model loads again, although the old model
    // never read it.
    std::fs::write(project.join("tests/broken.yml"), "name: broken\nprojections: [\n").unwrap();
    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(answer.contains("error:") && answer.contains("still on the model loaded before"), "{answer}");
    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(!answer.contains("error:"), "said again: {answer}");
    std::fs::write(project.join("tests/broken.yml"), "name: fixed\nprojections:\n  - Department\nassert_regex: SELECT\n").unwrap();
    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(answer.contains("reloaded in"), "the fixed file was not noticed: {answer}");

    // Break the file: the error is said once, and the model from before
    // keeps answering.
    std::fs::write(&table, "name: Transactions\nfields: [\n").unwrap();
    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(answer.contains("error:"), "{answer}");
    assert!(answer.contains("still on the model loaded before"), "{answer}");
    assert!(answer.contains(r#"sum(T0."net_sales_value")"#), "{answer}");
    writeln!(stdin, "department, sales value").unwrap();
    let answer = read_until(&mut out, "-- planned here in");
    assert!(!answer.contains("error:"), "said again: {answer}");

    // .reload says so explicitly; .branch is not a thing here.
    writeln!(stdin, ".reload").unwrap();
    let answer = read_until(&mut out, "still on the model loaded before");
    assert!(answer.contains("error:"), "{answer}");
    writeln!(stdin, ".branch dev").unwrap();
    let answer = read_until(&mut out, "check out dev");
    assert!(answer.contains("-r repl --branch dev"), "{answer}");

    writeln!(stdin, ".quit").unwrap();
    drop(stdin);
    let status = child.wait().unwrap();
    assert!(status.success(), "{status}");
    let _ = std::fs::remove_dir_all(&root);
}

/// A refusal under --json is JSON on stdout and exit 1; a usage error
/// is clap's and exit 2. Both from the same project, neither touching
/// a server.
#[test]
fn refusals_and_usage_errors_exit_as_promised() {
    let out = Command::new(env!("CARGO_BIN_EXE_zsql"))
        .args(["sql", "--json", "--expr", "no such field"])
        .current_dir(fixture())
        .env_remove("ZSQL_SERVER")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    let v: serde_json::Value = serde_json::from_slice(&out.stdout).expect("JSON on stdout");
    assert_eq!(v["error"]["class"], "FieldNotFound");
    let out = Command::new(env!("CARGO_BIN_EXE_zsql"))
        .args(["sql", "--expr", "no such field"])
        .current_dir(fixture())
        .env_remove("ZSQL_SERVER")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).starts_with("FieldNotFound: "));
    let out = Command::new(env!("CARGO_BIN_EXE_zsql")).args(["sql"]).current_dir(fixture()).env_remove("ZSQL_SERVER").output().unwrap();
    assert_eq!(out.status.code(), Some(2));
    let out = Command::new(env!("CARGO_BIN_EXE_zsql"))
        .args(["sql", "--branch", "dev", "--expr", "sales value"])
        .current_dir(fixture())
        .env_remove("ZSQL_SERVER")
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1));
    assert!(String::from_utf8_lossy(&out.stderr).contains("add -r"));
}
