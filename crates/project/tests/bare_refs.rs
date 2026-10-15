//! `[Field]` with no `@d`/`@m` in a model expression.
//!
//! These used to be parsed and then dropped: the bracket text reached the
//! warehouse verbatim, so `check`, `deploy` and `/sql` all passed and the
//! statement only failed when someone ran it. Worse for a measure, where
//! the lost reference took the aggregation with it and a tolerant
//! warehouse would have answered a wrong number rather than an error.

use zsql_dialect::Dialects;
use zsql_project::Loaded;

/// Writes a one-table project and loads it. `fields` is the YAML list.
fn load(name: &str, fields: &str) -> Result<Loaded, String> {
    let dir = std::env::temp_dir().join(format!("zsql-bare-refs-{}-{name}", std::process::id()));
    let _ = std::fs::remove_dir_all(&dir);
    std::fs::create_dir_all(dir.join("models")).unwrap();
    std::fs::write(dir.join("project.yml"), "name: T\nuid: t\nproduction_branch: main\n").unwrap();
    std::fs::write(dir.join("datasources.yml"), "wh:\n  name: W\n  adapter: duckdb\n  tier: hot\n").unwrap();
    std::fs::write(dir.join("models/tbl.t.yml"), format!("name: T\nphysical_name: loan\ndatasource: wh\ncost: 100\nfields:\n{fields}"))
        .unwrap();
    let dialects = Dialects::builtin();
    let out = zsql_project::load(&dir, &dialects).map_err(|e| e.to_string());
    let _ = std::fs::remove_dir_all(&dir);
    out
}

const AMT: &str = r#"  - type: dimension
    name: "Amt"
    data_type: integer
    expression: {sql: amount, primary_key: true}
"#;

fn refs_of<'a>(loaded: &'a Loaded, field: &str) -> (&'a [i64], &'a [i64]) {
    let snap = &loaded.snapshot;
    let f = snap.fields.iter().find(|f| f.name == field).unwrap_or_else(|| panic!("no field {field}"));
    let e = snap.expressions.iter().find(|e| e.field_id == f.id).expect("expression");
    (&e.dimension_ref_ids, &e.measure_ref_ids)
}

/// The heart of it: the two spellings must mean the same thing.
#[test]
fn a_bare_reference_resolves_like_a_suffixed_one() {
    let loaded = load(
        "same",
        &format!(
            r#"{AMT}  - type: measure
    name: "Suffixed"
    data_type: integer
    expression: {{sql: "sum([Amt]@d)"}}
  - type: measure
    name: "Bare"
    data_type: integer
    expression: {{sql: "sum([Amt])"}}
"#
        ),
    )
    .expect("loads");

    let (suffixed_dims, suffixed_msrs) = refs_of(&loaded, "Suffixed");
    let (bare_dims, bare_msrs) = refs_of(&loaded, "Bare");
    assert_eq!(bare_dims, suffixed_dims, "a bare reference resolves to the same dimension");
    assert_eq!(bare_msrs, suffixed_msrs);
    assert_eq!(bare_dims.len(), 1, "and it really did resolve to something");
}

/// A measure naming another measure, which is the form the bug was found
/// on: without the reference the aggregation is dropped too.
#[test]
fn a_bare_reference_to_a_measure_resolves() {
    let loaded = load(
        "measure",
        &format!(
            r#"{AMT}  - type: measure
    name: "Total"
    data_type: integer
    expression: {{sql: "sum(amount)"}}
  - type: measure
    name: "Double"
    data_type: integer
    expression: {{sql: "[Total] * 2"}}
"#
        ),
    )
    .expect("loads");
    let (dims, msrs) = refs_of(&loaded, "Double");
    assert!(dims.is_empty());
    assert_eq!(msrs.len(), 1, "[Total] names the measure");
}

/// Only one dimension and one measure may carry a name, so a bare
/// reference is ambiguous in exactly one case: when both exist.
#[test]
fn a_name_that_is_both_a_dimension_and_a_measure_is_ambiguous() {
    let err = load(
        "ambiguous",
        &format!(
            r#"{AMT}  - type: dimension
    name: "Dual"
    data_type: integer
    expression: {{sql: "amount + 1"}}
  - type: measure
    name: "Dual"
    data_type: integer
    expression: {{sql: "sum(amount)"}}
  - type: measure
    name: "Uses"
    data_type: integer
    expression: {{sql: "[Dual] * 2"}}
"#
        ),
    )
    .expect_err("ambiguous");
    assert!(err.contains("both a dimension and a measure"), "{err}");
    // The name is echoed as the model spells it, not as the parser
    // lower-cases it.
    assert!(err.contains("[Dual]@d"), "{err}");
}

/// The same two fields are fine as long as the reference says which.
#[test]
fn a_suffix_resolves_what_would_otherwise_be_ambiguous() {
    let loaded = load(
        "disambiguated",
        &format!(
            r#"{AMT}  - type: dimension
    name: "Dual"
    data_type: integer
    expression: {{sql: "amount + 1"}}
  - type: measure
    name: "Dual"
    data_type: integer
    expression: {{sql: "sum(amount)"}}
  - type: measure
    name: "Uses"
    data_type: integer
    expression: {{sql: "[Dual]@m * 2"}}
"#
        ),
    )
    .expect("loads");
    let (dims, msrs) = refs_of(&loaded, "Uses");
    assert!(dims.is_empty(), "@m picked the measure");
    assert_eq!(msrs.len(), 1);
}

/// What the bug let through silently.
#[test]
fn a_bare_reference_to_nothing_is_refused() {
    let err = load(
        "missing",
        &format!(
            r#"{AMT}  - type: measure
    name: "M"
    data_type: integer
    expression: {{sql: "sum([No Such Field])"}}
"#
        ),
    )
    .expect_err("refused");
    assert!(err.contains("could not find a field named"), "{err}");
    // Someone writing SQL Server habits needs to know brackets are taken.
    assert!(err.contains("double quotes"), "{err}");
}

/// A cycle does not fail the planner, it overflows its stack and aborts
/// the process, so the model is refused before it can be deployed.
#[test]
fn an_expression_that_references_itself_is_refused() {
    let err = load(
        "selfref",
        &format!(
            r#"{AMT}  - type: measure
    name: "Loop"
    data_type: integer
    expression: {{sql: "sum([Loop]) + 1"}}
"#
        ),
    )
    .expect_err("refused");
    assert!(err.contains("references itself"), "{err}");
}

#[test]
fn a_cycle_through_another_field_is_refused() {
    let err = load(
        "cycle",
        &format!(
            r#"{AMT}  - type: measure
    name: "A"
    data_type: integer
    expression: {{sql: "[B] + 1"}}
  - type: measure
    name: "B"
    data_type: integer
    expression: {{sql: "[A] + 2"}}
"#
        ),
    )
    .expect_err("refused");
    assert!(err.contains("references itself through"), "{err}");
    assert!(err.contains("B") && err.contains("A"), "the chain is named: {err}");
}

/// Two fields may name the same third one without that being a cycle.
#[test]
fn a_diamond_is_not_a_cycle() {
    load(
        "diamond",
        &format!(
            r#"{AMT}  - type: measure
    name: "Base"
    data_type: integer
    expression: {{sql: "sum(amount)"}}
  - type: measure
    name: "Left"
    data_type: integer
    expression: {{sql: "[Base] * 2"}}
  - type: measure
    name: "Right"
    data_type: integer
    expression: {{sql: "[Base] * 3"}}
  - type: measure
    name: "Top"
    data_type: integer
    expression: {{sql: "[Left] + [Right]"}}
"#
        ),
    )
    .expect("a diamond loads");
}

/// The whole point: the SQL a bare reference plans is the SQL a suffixed
/// one plans. Checked through the planner, not just the ref lists.
#[test]
fn bare_and_suffixed_plan_the_same_sql() {
    let dialects = Dialects::builtin();
    let loaded = load(
        "plan",
        &format!(
            r#"{AMT}  - type: measure
    name: "Suffixed"
    data_type: integer
    expression: {{sql: "sum([Amt]@d)"}}
  - type: measure
    name: "Bare"
    data_type: integer
    expression: {{sql: "sum([Amt])"}}
"#
        ),
    )
    .expect("loads");
    let universe = zsql_semantic::Universe::build(loaded.snapshot.clone()).expect("universe");
    let plan = |field: &str| {
        let spec = zsql_spec::Spec::from_value(serde_json::json!({ "projections": [{ "field": field }] })).unwrap();
        let base = dialects.base().unwrap();
        let q = zsql_spec::lower(&spec, &universe, &base).unwrap();
        zsql_planner::Planner::new(&universe, &dialects, None).plan_query(q, None).unwrap().sql
    };
    let bare = plan("Bare").replace("Bare", "X");
    let suffixed = plan("Suffixed").replace("Suffixed", "X");
    assert_eq!(bare, suffixed, "the two spellings plan the same statement");
    assert!(!bare.contains('['), "no bracket text survives into the SQL: {bare}");
}

/// The loader refuses to build a cycle, but snapshots deployed before it
/// did are already on disk and reload straight into the planner. The
/// planner must answer rather than recurse: this exact shape overflowed
/// the stack and aborted the whole server process, so one query from one
/// tenant took the service down for everyone.
#[test]
fn a_cyclic_snapshot_already_on_disk_is_an_error_not_a_crash() {
    let loaded = load(
        "ondisk",
        &format!(
            r#"{AMT}  - type: measure
    name: "M"
    data_type: integer
    expression: {{sql: "sum([Amt]@d)"}}
"#
        ),
    )
    .expect("loads");

    // Bend the snapshot into the shape the old loader would have left:
    // M's expression naming M.
    let mut snapshot = loaded.snapshot;
    let m = snapshot.fields.iter().find(|f| f.name == "M").unwrap().id;
    for e in snapshot.expressions.iter_mut().filter(|e| e.field_id == m) {
        e.measure_ref_ids = vec![m];
    }

    let dialects = Dialects::builtin();
    let universe = zsql_semantic::Universe::build(snapshot).expect("a cyclic model still builds a universe");
    let spec = zsql_spec::Spec::from_value(serde_json::json!({ "projections": [{ "field": "M" }] })).unwrap();
    let base = dialects.base().unwrap();
    let q = zsql_spec::lower(&spec, &universe, &base).unwrap();
    let err = zsql_planner::Planner::new(&universe, &dialects, None).plan_query(q, None).expect_err("refuses to plan");
    let msg = err.to_string();
    assert!(msg.contains("references itself"), "{msg}");
}
