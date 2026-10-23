//! A field named on several tables is one field. Every definition updates
//! it, but only with what that definition says: a table that carries the
//! column and describes nothing must not erase the description written
//! beside the dimension, whichever file the directory walk reaches last.

use std::path::{Path, PathBuf};
use zsql_dialect::Dialects;

/// Two tables carrying one dimension. `rich` names the file that holds the
/// metadata; the other declares the same field with nothing but its
/// expression, as `zsql generate` writes it.
fn project(root: &Path, rich: &str) -> PathBuf {
    let dir = root.join(format!("project-{rich}"));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n").unwrap();
    let full = "  - type: dimension\n    name: Status\n    data_type: string\n    description: What the order is doing now\n    synonyms: [state]\n    tags: [core]\n    hidden: true\n    enable_sampling: true\n    expression: status\n";
    let bare = "  - type: dimension\n    name: Status\n    data_type: string\n    expression: status\n";
    for (file, table) in [("a", "A"), ("b", "B")] {
        let fields = if file == rich { full } else { bare };
        std::fs::write(
            dir.join(format!("models/tbl.{file}.yml")),
            format!("name: {table}\nphysical_name: {file}\ndatasource: wh\ncost: 1\nfields:\n{fields}"),
        )
        .unwrap();
    }
    dir
}

#[test]
fn a_bare_definition_does_not_erase_a_described_one() {
    let root = std::env::temp_dir().join(format!("zsql-field-merge-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let dialects = Dialects::builtin();

    // Whichever file holds the metadata, the metadata survives.
    for rich in ["a", "b"] {
        let loaded = zsql_project::load(&project(&root, rich), &dialects).unwrap();
        let f = loaded.snapshot.fields.iter().find(|f| f.name == "Status").expect("the field loads");
        assert_eq!(f.description.as_deref(), Some("What the order is doing now"), "described in {rich}");
        assert_eq!(f.synonyms, vec!["state".to_string()], "synonyms from {rich}");
        assert_eq!(f.tags, vec!["core".to_string()], "tags from {rich}");
        assert!(f.hidden, "hidden from {rich}");
        assert!(f.enable_sampling, "sampling from {rich}");
        // Both tables still map the field.
        let maps = loaded.snapshot.expressions.iter().filter(|e| e.field_id == f.id).count();
        assert_eq!(maps, 2, "one field, two tables");
    }
}

/// Both files describe the field, and differently. The last one loaded wins,
/// so the load warns rather than letting file order decide in silence.
fn project_disagreeing(root: &Path) -> PathBuf {
    let dir = root.join("project-disagree");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n").unwrap();
    for (file, table, description) in [("a", "A", "What the order is doing"), ("b", "B", "Something else entirely")] {
        std::fs::write(
            dir.join(format!("models/tbl.{file}.yml")),
            format!(
                "name: {table}\nphysical_name: {file}\ndatasource: wh\ncost: 1\nfields:\n  - type: dimension\n    name: Status\n    data_type: string\n    description: {description}\n    expression: status\n"
            ),
        )
        .unwrap();
    }
    dir
}

#[test]
fn two_definitions_that_disagree_warn() {
    let root = std::env::temp_dir().join(format!("zsql-field-merge-warn-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let loaded = zsql_project::load(&project_disagreeing(&root), &Dialects::builtin()).unwrap();
    let about: Vec<&String> = loaded.warnings.iter().filter(|w| w.contains("Status") && w.contains("description")).collect();
    assert_eq!(about.len(), 1, "one warning, naming both files: {:?}", loaded.warnings);
    let w = about[0];
    assert!(w.contains("tbl.a.yml") && w.contains("tbl.b.yml"), "both files are named: {w}");
}
