//! A fired policy whose permission resolution finds nothing for the user:
//! `unresolved: deny` (the default) yields no rows or every value masked,
//! `unresolved: allow` (Rails' rule, what the imported goldens state) skips
//! the policy. Bypass flags still open the door explicitly.

use zsql_golden::Fixtures;
use zsql_semantic::{Universe, Unresolved};
use zsql_spec::{Group, SecurityContext};

fn plan(universe: &Universe, fx: &Fixtures, spec: serde_json::Value, ctx: &SecurityContext) -> String {
    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&zsql_spec::Spec::from_value(spec).unwrap(), universe, &base).unwrap();
    zsql_planner::Planner::new(universe, fx.dialects(), None).plan_query(query, Some(ctx)).unwrap().sql
}

fn universe_with(fx: &Fixtures, hash: &str, unresolved: Unresolved) -> Universe {
    let mut snap = fx.snapshot(hash).unwrap();
    for p in snap.security_policies.iter_mut() {
        p.unresolved = unresolved;
    }
    Universe::build(snap).unwrap()
}

#[test]
fn mask_policy_masks_everything_when_nothing_resolves() {
    let fx = Fixtures::open(Fixtures::default_root());
    // mask_data on pii fields, keyed on Call Center ID from the groups' call_center_id tags
    // (the universe of "mask mode wraps triggered projection in CASE expression").
    let hash = "d0a3fb66df4f9a0c";
    let spec = serde_json::json!({ "projections": [{ "field": "Country" }] });
    let stranger = SecurityContext { email: Some("nobody@example.com".into()), groups: vec![Group { name: Some("Guests".into()), tags: vec!["region:emea".into()] }], ..Default::default() };

    let deny = universe_with(&fx, hash, Unresolved::Deny);
    let sql = plan(&deny, &fx, spec.clone(), &stranger);
    let country = sql.lines().find(|l| l.contains("AS \"Country\"")).unwrap();
    assert!(country.trim().starts_with("'#######'"), "every value masked: {country}");
    assert!(!sql.contains("CASE WHEN"), "{sql}");

    let allow = universe_with(&fx, hash, Unresolved::Allow);
    let sql = plan(&allow, &fx, spec.clone(), &stranger);
    assert!(sql.contains("country\" AS \"Country\"") && !sql.contains("#######"), "policy skipped: {sql}");

    // A user the policy resolves is masked per row, on either setting.
    let member = SecurityContext { groups: vec![Group { name: Some("CC-TMNT".into()), tags: vec!["call_center_id:TMNT".into()] }], ..Default::default() };
    let sql = plan(&deny, &fx, spec.clone(), &member);
    assert!(sql.contains("CASE WHEN LOWER(") && sql.contains("IN ('tmnt')"), "{sql}");

    // An admin the policy bypasses sees the column.
    let admin = SecurityContext { system_admin: true, ..Default::default() };
    let sql = plan(&deny, &fx, spec, &admin);
    assert!(!sql.contains("#######") && !sql.contains("CASE WHEN"), "{sql}");
}

#[test]
fn filter_policy_yields_no_rows_when_nothing_resolves() {
    let fx = Fixtures::open(Fixtures::default_root());
    // "Include Apply Filter": filter_data on secure-user fields, groups' cat tags on Category.
    let hash = "080cf893a6d30e32";
    let spec = serde_json::json!({ "projections": [{ "field": "Category" }, { "field": "Median Web Category Size" }] });
    let stranger = SecurityContext { groups: vec![Group { name: Some("Guests".into()), tags: vec![] }], ..Default::default() };

    let deny = universe_with(&fx, hash, Unresolved::Deny);
    let sql = plan(&deny, &fx, spec.clone(), &stranger);
    assert!(sql.contains("1 = 0"), "no rows: {sql}");

    let allow = universe_with(&fx, hash, Unresolved::Allow);
    let sql = plan(&allow, &fx, spec.clone(), &stranger);
    assert!(!sql.contains("1 = 0") && !sql.contains("IN ('"), "policy skipped: {sql}");

    // A user the policy resolves is not denied. Here the measure's own
    // inclusion rule ignores filters on the context dimension, which outranks
    // a resolved security filter (never a denial), so no filter is added.
    let member = SecurityContext { groups: vec![Group { name: Some("Men".into()), tags: vec!["cat:men".into()] }], ..Default::default() };
    let sql = plan(&deny, &fx, spec, &member);
    assert!(!sql.contains("1 = 0") && !sql.contains("IN ('men')"), "{sql}");
}
