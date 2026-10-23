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
    let _ = std::fs::remove_dir_all(&root);
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
    let _ = std::fs::remove_dir_all(&root);
}

/// What a later definition leaves out is treated two ways. Metadata, which
/// is how the field reads, blank-fills. Everything that changes the plan is
/// taken from the last definition whether it states it or not, so a bare
/// mapping can still clear a rule another file set, and two files cannot
/// between them assemble a field the loader rejects when one file writes it.
#[test]
fn a_rule_is_cleared_by_a_later_definition_but_a_description_is_not() {
    let root = std::env::temp_dir().join(format!("zsql-field-rules-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    let dir = root.join("project");
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n").unwrap();
    // a.yml states the rule, the grain and the description; b.yml, which
    // loads second, states only the mapping.
    std::fs::write(
        dir.join("models/tbl.a.yml"),
        "name: A\nphysical_name: a\ndatasource: wh\ncost: 1\nfields:\n  - type: dimension\n    name: Day\n    data_type: date\n    grains: [day, month]\n    description: The day it happened\n    expression: d\n  - type: measure\n    name: Sales\n    data_type: decimal\n    exclusion_type: exclude_all\n    expression: sum(amt)\n",
    )
    .unwrap();
    std::fs::write(
        dir.join("models/tbl.b.yml"),
        "name: B\nphysical_name: b\ndatasource: wh\ncost: 1\nfields:\n  - type: dimension\n    name: Day\n    data_type: date\n    expression: d\n  - type: measure\n    name: Sales\n    data_type: decimal\n    expression: sum(amt)\n",
    )
    .unwrap();

    let loaded = zsql_project::load(&dir, &Dialects::builtin()).unwrap();
    let field = |name: &str| loaded.snapshot.fields.iter().find(|f| f.name == name).expect("the field loads").clone();

    // Metadata survives the bare definition.
    assert_eq!(field("Day").description.as_deref(), Some("The day it happened"), "a description blank-fills");
    // Semantics do not: the last definition said nothing, so there is no rule.
    assert_eq!(field("Sales").exclusion_type, None, "a bare definition clears a rule, as a deploy does");
    assert!(field("Day").grains.is_empty(), "and clears the grains");
    let _ = std::fs::remove_dir_all(&root);
}
