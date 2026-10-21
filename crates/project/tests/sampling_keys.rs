//! The sampling keys a dimension carries: off unless the modeller turns it
//! on, a bounded size, and a query of the modeller's own. Nothing here
//! judges whether the field is a sensible one to sample — that rule lives
//! in the project's AGENTS.md, because the modeller knows the data and the
//! loader does not.

use std::path::{Path, PathBuf};
use zsql_dialect::Dialects;

fn project_with_field(root: &Path, keys: &str) -> PathBuf {
    let dir = root.join("project");
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: P\nuid: p\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  adapter: duckdb\n").unwrap();
    std::fs::write(
        dir.join("models/tbl.t.yml"),
        format!("name: T\nphysical_name: t\ndatasource: wh\ncost: 1\nfields:\n  - type: dimension\n    name: Status\n    data_type: string\n    expression: status\n{keys}"),
    )
    .unwrap();
    dir
}

#[test]
fn sampling_is_off_until_asked_for_and_its_size_is_bounded() {
    let root = std::env::temp_dir().join(format!("zsql-sampling-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&root);
    std::fs::create_dir_all(&root).unwrap();
    let dialects = Dialects::builtin();
    let field = |dir: &Path| {
        let loaded = zsql_project::load(dir, &dialects).unwrap();
        loaded.snapshot.fields.iter().find(|f| f.name == "Status").cloned().expect("the field loads")
    };

    // Nothing said: off, and the default size.
    let f = field(&project_with_field(&root, ""));
    assert!(!f.enable_sampling, "a dimension is not sampled unless asked for");
    assert_eq!(f.sample_size, zsql_semantic::default_sample_size());
    assert_eq!(f.sample_sql, None);

    // Asked for, with a size and a query of its own.
    let f = field(&project_with_field(
        &root,
        "    enable_sampling: true\n    sample_size: 50\n    sample_sql: SELECT status, COUNT(*) FROM t GROUP BY 1\n",
    ));
    assert!(f.enable_sampling);
    assert_eq!(f.sample_size, 50);
    assert_eq!(f.sample_sql.as_deref(), Some("SELECT status, COUNT(*) FROM t GROUP BY 1"));

    // The ceiling is a refusal that names the field and the bound, because
    // the sample file is read with a limit that refuses the whole file: one
    // greedy dimension would cost every other one its values.
    for bad in ["20001", "0", "-5"] {
        let dir = project_with_field(&root, &format!("    sample_size: {bad}\n"));
        let err = zsql_project::load(&dir, &dialects).unwrap_err().to_string();
        assert!(err.contains("sample_size") && err.contains("Status"), "{bad}: {err}");
        assert!(err.contains(&zsql_semantic::MAX_SAMPLE_SIZE.to_string()), "{bad} must name the bound: {err}");
    }

    // The keys the model dropped are no longer part of the field set: an
    // unknown key warns and is ignored rather than being applied.
    let dir = project_with_field(&root, "    disable_listing: true\n    value_list_size: 10\n    values: full\n");
    let loaded = zsql_project::load(&dir, &dialects).unwrap();
    for gone in ["disable_listing", "value_list_size", "values"] {
        assert!(loaded.warnings.iter().any(|w| w.contains(gone)), "{gone} should warn as unknown: {:?}", loaded.warnings);
    }
    let _ = std::fs::remove_dir_all(&root);
}
