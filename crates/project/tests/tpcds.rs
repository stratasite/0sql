//! Loads the TPC-DS fixture project that ships with this repository.

use std::path::PathBuf;
use zsql_dialect::Dialects;
use zsql_semantic::Universe;

fn sample() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects/tpcds")
}

fn plan(universe: &Universe, dialects: &Dialects, spec: serde_json::Value) -> Result<String, String> {
    let spec = zsql_spec::Spec::from_value(spec).map_err(|e| e.to_string())?;
    let base = dialects.base().map_err(|e| e.to_string())?;
    let query = zsql_spec::lower(&spec, universe, &base).map_err(|e| e.to_string())?;
    let planner = zsql_planner::Planner::new(universe, dialects, None);
    planner.plan_query(query, None).map(|p| p.sql).map_err(|e| e.to_string())
}

#[test]
fn tpcds_sample_loads_forms_and_plans() {
    let dir = sample();
    let dialects = Dialects::builtin();
    let loaded = zsql_project::load(&dir, &dialects).unwrap();
    let snap = &loaded.snapshot;
    assert_eq!(snap.datasources.len(), 1);
    assert_eq!(snap.datasources[0].uid, "tpcds");
    assert_eq!(snap.tables.len(), 33);
    assert!(snap.fields.len() > 300);
    assert!(snap.expressions.len() >= snap.fields.len(), "a field is mapped on at least one table");
    assert!(snap.join_defs.len() >= 50);
    assert!(loaded.sources.iter().any(|s| s.ends_with("project.yml")) && loaded.sources.iter().any(|s| s.ends_with("models")));
    assert!(snap.paths.len() > snap.fields.len());
    // uids are FriendlyId slugs of the names
    let store_sales = snap.tables.iter().find(|t| t.name == "Store Sales").unwrap();
    assert_eq!(store_sales.uid, "store-sales");
    assert_eq!(store_sales.source.as_deref(), Some("models/store/tbl.store_sales.yml"));
    let net_paid = snap.fields.iter().find(|f| f.name == "Store Net Paid").unwrap();
    assert_eq!(net_paid.uid, "store-net-paid");
    let e = snap.expressions.iter().find(|e| e.field_id == net_paid.id).unwrap();
    assert_eq!(e.column_refs, vec!["ss_net_paid"]);
    // the inventory table snapshots on Date
    let inventory = snap.tables.iter().find(|t| t.name == "Inventory").unwrap();
    let date = snap.fields.iter().find(|f| f.name == "Date" && f.is_dimension()).unwrap();
    assert_eq!(inventory.snapshot_date_id, Some(date.id));
    for w in &loaded.warnings {
        eprintln!("warning: {w}");
    }

    // The snapshot survives a JSON round trip unchanged.
    let text = serde_json::to_string(snap).unwrap();
    let again = zsql_semantic::Snapshot::from_str(&text).unwrap();
    assert_eq!(serde_json::to_string(&again).unwrap(), text);

    let universe = Universe::build(loaded.snapshot).unwrap();
    let sql = plan(
        &universe,
        &dialects,
        serde_json::json!({
            "projections": [{ "field": "Item Category" }, { "field": "Store Net Paid" }, { "field": "Date", "decorators": [{ "type": "truncate", "grain": "month" }] }],
            "filters": [{ "field": "Item Category", "predicate": "in_list", "value": "Books, Music" }]
        }),
    )
    .unwrap();
    assert!(sql.contains("sum(T0.\"ss_net_paid\") AS \"Store Net Paid\""), "{sql}");
    assert!(sql.contains("DATE_TRUNC('month', T1.\"d_date\")::DATE AS \"Month(Date)\""), "{sql}");
    assert!(sql.contains("LOWER(T2.\"i_category\") IN ('books', 'music')"), "{sql}");

    // Two fact tables merge; a calculation reads both.
    let sql = plan(
        &universe,
        &dialects,
        serde_json::json!({
            "projections": [{ "field": "item-category" }, { "field": "store-net-paid" }, { "field": "catalog-net-paid" }],
            "calculations": [{ "alias": "Store Share", "sql": "sum([Store Net Paid]@m) / (sum([Store Net Paid]@m) + sum([Catalog Net Paid]@m))" }]
        }),
    )
    .unwrap();
    assert!(sql.contains("FULL OUTER JOIN"), "{sql}");
    assert!(sql.contains("AS \"Store Share\""), "{sql}");

    // A snapshot measure on the inventory table.
    let sql = plan(
        &universe,
        &dialects,
        serde_json::json!({ "projections": [{ "field": "Date", "decorators": [{ "type": "truncate", "grain": "month" }] }, { "field": "Inventory Quantity On Hand" }] }),
    )
    .unwrap();
    assert!(sql.to_lowercase().contains("inv_quantity_on_hand"), "{sql}");

    // Names resolve without regard to case, and a bad name is refused by name.
    let err = plan(&universe, &dialects, serde_json::json!({ "projections": [{ "field": "No Such Field" }] })).unwrap_err();
    assert!(err.contains("No field named 'No Such Field'"), "{err}");
}

#[test]
fn a_broken_project_is_refused_with_its_file() {
    let dir = std::env::temp_dir().join(format!("strata-rs-project-{}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: Broken\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "pg:\n  name: PG\n  adapter: postgres\n  tier: hot\n").unwrap();
    std::fs::write(
        dir.join("models/tbl.orders.yml"),
        "name: Orders\nphysical_name: orders\ndatasource: pg\ncost: 10\nfields:\n  - type: measure\n    name: Revenue\n    data_type: decimal\n    expression:\n      sql: revenue\n",
    )
    .unwrap();
    let err = zsql_project::load(&dir, &Dialects::builtin()).unwrap_err().to_string();
    assert!(err.contains("models/tbl.orders.yml"), "{err}");
    assert!(err.contains("measure should have an aggregation function"), "{err}");
    let _ = std::fs::remove_dir_all(&dir);
}
