//! `[Name]` in a query-time calculation.
//!
//! A bare reference is a projection of this query first and a field
//! second. The alias wins because it is the nearer thing and the one the
//! author just wrote; `@d`/`@m` is how to reach past it to the field.
//!
//! Before this, a bare reference matched an alias or nothing at all, so
//! `[Some Measure] * 2` failed unless that measure happened to be
//! projected under its own name.

use zsql_dialect::Dialects;
use zsql_semantic::Universe;

/// One table with a dimension, two distinct measures, and a name carried
/// by both a dimension and a measure.
fn universe(name: &str) -> (Universe, Dialects) {
    // A directory per caller: these tests run in parallel, and sharing
    // one would have them deleting each other's project mid-load.
    let dir = std::env::temp_dir().join(format!("zsql-calc-refs-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: T\nuid: t\nproduction_branch: main\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  name: W\n  adapter: duckdb\n  tier: hot\n").unwrap();
    std::fs::write(
        dir.join("models/tbl.t.yml"),
        r#"name: T
physical_name: loan
datasource: wh
cost: 100
fields:
  - type: dimension
    name: "Amt"
    data_type: integer
    expression: {sql: amount, primary_key: true}
  - type: measure
    name: "M"
    data_type: integer
    expression: {sql: "sum(amount)"}
  - type: measure
    name: "Other"
    data_type: integer
    expression: {sql: "sum(amount) * 10"}
  - type: dimension
    name: "Dual"
    data_type: integer
    expression: {sql: "amount + 1"}
  - type: measure
    name: "Dual"
    data_type: integer
    expression: {sql: "sum(amount) + 1"}
"#,
    )
    .unwrap();
    let dialects = Dialects::builtin();
    let loaded = zsql_project::load(&dir, &dialects).expect("loads");
    let _ = std::fs::remove_dir_all(&dir);
    (Universe::build(loaded.snapshot).expect("universe"), dialects)
}

fn plan(name: &str, spec: serde_json::Value) -> Result<String, String> {
    let (universe, dialects) = universe(name);
    let spec = zsql_spec::Spec::from_value(spec).map_err(|e| e.to_string())?;
    let base = dialects.base().map_err(|e| e.to_string())?;
    let q = zsql_spec::lower(&spec, &universe, &base).map_err(|e| e.to_string())?;
    zsql_planner::Planner::new(&universe, &dialects, None).plan_query(q, None).map(|p| p.sql).map_err(|e| e.to_string())
}

/// The case that already worked, and must keep working: a bare reference
/// to something this query projects.
#[test]
fn a_bare_reference_takes_a_projection_of_this_query() {
    let sql = plan("bare-projection", serde_json::json!({
        "projections": [{ "field": "M" }],
        "calculations": [{ "alias": "D", "sql": "[M] * 2", "data_type": "integer" }]
    }))
    .expect("plans");
    // One aggregate, read twice: the calculation reuses the projection
    // rather than computing the measure again.
    assert_eq!(sql.matches("sum(").count(), 1, "{sql}");
}

/// The case that did not: nothing in the query is called `M`, so the
/// reference means the field.
#[test]
fn a_bare_reference_falls_back_to_a_field() {
    let sql = plan("bare-field", serde_json::json!({
        "projections": [{ "field": "Amt" }],
        "calculations": [{ "alias": "D", "sql": "[M] * 2", "data_type": "integer" }]
    }))
    .expect("plans");
    assert!(sql.contains("sum("), "the measure was pulled in: {sql}");
    assert!(!sql.contains('['), "no bracket text survives: {sql}");
}

/// A projection's alias shadows a field of the same name, and the suffix
/// is how to say you meant the field anyway.
#[test]
fn a_suffix_reaches_past_an_alias_to_the_field() {
    // `M` is projected as "Other", which is also a field's name.
    let spec = |sql: &str| {
        serde_json::json!({
            "projections": [{ "field": "M", "alias": "Other" }],
            "calculations": [{ "alias": "D", "sql": sql, "data_type": "integer" }]
        })
    };
    let bare = plan("alias-bare", spec("[Other] * 2")).expect("plans");
    let suffixed = plan("alias-suffixed", spec("[Other]@m * 2")).expect("plans");

    // Bare: the projection, which is plain `sum(amount)`, so the field
    // `Other` (`sum(amount) * 10`) is never computed.
    assert!(!bare.contains("* 10"), "bare took the alias: {bare}");
    // Suffixed: the field, so its `* 10` is in the statement.
    assert!(suffixed.contains("* 10"), "@m took the field: {suffixed}");
}

/// The one ambiguity a name can have, since each kind allows only one.
#[test]
fn a_bare_reference_to_both_a_dimension_and_a_measure_is_refused() {
    let err = plan("ambiguous", serde_json::json!({
        "projections": [{ "field": "Amt" }],
        "calculations": [{ "alias": "D", "sql": "[Dual] * 2", "data_type": "integer" }]
    }))
    .expect_err("ambiguous");
    assert!(err.contains("both a dimension and a measure"), "{err}");
}

#[test]
fn a_suffix_resolves_that_ambiguity() {
    plan("disambiguated", serde_json::json!({
        "projections": [{ "field": "Amt" }],
        "calculations": [{ "alias": "D", "sql": "[Dual]@m * 2", "data_type": "integer" }]
    }))
    .expect("plans");
}

/// Naming nothing at all still fails, and the message now says a field
/// was looked for too.
#[test]
fn a_bare_reference_to_nothing_is_refused() {
    let err = plan("not-found", serde_json::json!({
        "projections": [{ "field": "Amt" }],
        "calculations": [{ "alias": "D", "sql": "[Nope] * 2", "data_type": "integer" }]
    }))
    .expect_err("not found");
    assert!(err.contains("could not find a projection or a field"), "{err}");
}
