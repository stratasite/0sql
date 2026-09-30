//! The fixture format is a contract: every snapshot must deserialize and
//! build, every case must parse and name a snapshot, every compiled-in
//! dialect must load and render.

use zsql_golden::Fixtures;

#[test]
fn every_snapshot_builds_a_universe() {
    let fx = Fixtures::open(Fixtures::default_root());
    let hashes = fx.snapshot_hashes().unwrap();
    assert!(!hashes.is_empty(), "no snapshots under {}", fx.root().display());
    for hash in &hashes {
        let u = fx.universe(hash).unwrap_or_else(|e| panic!("{hash}: {e:#}"));
        assert!(!u.tables().is_empty(), "{hash} has no tables");
        assert!(!u.fields().is_empty(), "{hash} has no fields");
    }
}

#[test]
fn every_case_parses_and_references_a_snapshot() {
    let fx = Fixtures::open(Fixtures::default_root());
    let hashes = fx.snapshot_hashes().unwrap();
    let cases = fx.cases().unwrap();
    let n: usize = cases.values().map(Vec::len).sum();
    assert!(n > 0, "no cases");
    for case in cases.values().flatten() {
        assert!(hashes.contains(&case.universe), "{} references missing snapshot {}", case.id, case.universe);
        zsql_spec::Spec::from_value(case.spec.clone()).unwrap_or_else(|e| panic!("{}: {e}", case.id));
    }
}

#[test]
fn every_dialect_loads_and_renders() {
    let fx = Fixtures::open(Fixtures::default_root());
    let dialects = fx.dialects();
    let names = dialects.names().unwrap();
    assert!(names.contains(&"postgres".to_string()));
    for name in &names {
        let d = dialects.get(name).unwrap_or_else(|e| panic!("{name}: {e}"));
        d.quote("x").unwrap_or_else(|e| panic!("{name}: {e}"));
        d.render("truncate_date", &[("unit", "month"), ("exp", "d")]).unwrap_or_else(|e| panic!("{name}: {e}"));
    }
    let pg = dialects.get("postgres").unwrap();
    assert_eq!(pg.render("date_literal", &[("val", "2024-01-01")]).unwrap(), "'2024-01-01'::DATE");
    assert_eq!(pg.string_lit("it's").unwrap(), "'it''s'");
    assert!(!dialects.base().unwrap().aggregate_functions().is_empty());
}
