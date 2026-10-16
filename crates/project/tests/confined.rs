//! The confined loader, the one the server uses on an archive someone
//! sent, follows no import out of the project; the owned loader, the
//! CLI's, does.

use std::path::{Path, PathBuf};
use zsql_dialect::Dialects;

fn project_with_import(root: &Path, import: &str) -> PathBuf {
    let dir = root.join("project");
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n").unwrap();
    std::fs::write(
        dir.join("models/tbl.t.yml"),
        format!("name: T\nphysical_name: t\ndatasource: wh\ncost: 1\nimports:\n  - {import}\nfields:\n  - type: dimension\n    name: Own\n    data_type: string\n    expression: own\n"),
    )
    .unwrap();
    dir
}

const SHARED: &str = "fields:\n  - type: dimension\n    name: Shared\n    data_type: string\n    expression: shared\n";

#[test]
fn an_import_outside_the_project_is_refused_when_confined_and_followed_when_owned() {
    let root = std::env::temp_dir().join(format!("zsql-confined-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let outside = root.join("shared.yml");
    std::fs::write(&outside, SHARED).unwrap();
    let dir = project_with_import(&root, outside.to_str().unwrap());
    let dialects = Dialects::builtin();

    let err = zsql_project::load_confined(&dir, &dialects).unwrap_err().to_string();
    assert!(err.contains("outside the project"), "{err}");

    let loaded = zsql_project::load(&dir, &dialects).unwrap();
    assert!(loaded.snapshot.fields.iter().any(|f| f.name == "Shared"), "the owned loader follows the import");
    assert!(loaded.sources.iter().any(|s| s == &outside), "the import is a source the watcher must stamp");

    // A relative path that climbs out is the same thing.
    let dir = project_with_import(&root, "../shared.yml");
    let err = zsql_project::load_confined(&dir, &dialects).unwrap_err().to_string();
    assert!(err.contains("outside the project"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

#[test]
fn a_message_names_what_would_have_been_right() {
    let root = std::env::temp_dir().join(format!("zsql-messages-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("project");
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n").unwrap();
    std::fs::write(dir.join("models/tbl.t.yml"), "name: T\nphysical_name: t\ndatasource: warehouse\ncost: 1\nfields: []\n").unwrap();
    let dialects = Dialects::builtin();
    let err = zsql_project::load(&dir, &dialects).unwrap_err().to_string();
    assert!(err.contains("'warehouse' is not in datasources.yml (one of: wh)"), "{err}");
    assert!(!err.contains('\n'), "one line: {err}");
    std::fs::write(dir.join("models/tbl.t.yml"), "name: T\nphysical_name: t\ndatasource: wh\ncost: 1\nfields:\n  - type: dimension\n    name: X\n    data_type: text\n    expression: x\n").unwrap();
    let err = zsql_project::load(&dir, &dialects).unwrap_err().to_string();
    assert!(err.contains("Data type 'text' is not included in the list (one of: string,"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}

/// What the loader lets through, the universe still checks: a negative
/// cost would rank a route backwards, and a model that would plan
/// wrong is refused rather than loaded.
#[test]
fn a_negative_cost_is_refused_by_name() {
    let root = std::env::temp_dir().join(format!("zsql-cost-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("project");
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n").unwrap();
    std::fs::write(
        dir.join("models/tbl.t.yml"),
        "name: T\nphysical_name: t\ndatasource: wh\ncost: -3\nfields:\n  - type: dimension\n    name: X\n    data_type: string\n    expression: x\n",
    )
    .unwrap();
    let dialects = Dialects::builtin();
    let err = zsql_project::load(&dir, &dialects).unwrap_err().to_string();
    assert!(err.contains("table t has a negative cost (-3)"), "{err}");
    let _ = std::fs::remove_dir_all(&root);
}
