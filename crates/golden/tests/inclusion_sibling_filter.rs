//! A measure filter beside an inclusion measure. Rails carries every
//! measure filter into the inclusion roll-up, where no input outputs the
//! sibling's column, and refuses the query. Here the sibling's filter
//! stays on the node that computes it, so the query plans.

use zsql_golden::Fixtures;

#[test]
fn a_filter_on_a_sibling_measure_plans_beside_an_inclusion_measure() {
    let fx = Fixtures::open(Fixtures::default_root());
    let case = &fx.cases().unwrap()["inclusion_measure_test"][0];
    let universe = fx.universe(&case.universe).unwrap();
    let spec = zsql_spec::Spec::from_value(serde_json::json!({
        "projections": [{ "field": "Date" }, { "field": "Net Paid" }, { "field": "Median Store Order Size" }],
        "filters": [{ "field": "Net Paid", "predicate": "greater_than", "value": "0" }]
    }))
    .unwrap();
    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&spec, &universe, &base).unwrap();
    let plan = zsql_planner::Planner::new(&universe, fx.dialects(), None).plan_query(query, None).unwrap();
    assert!(plan.sql.contains("percentile_cont(0.5)"), "{}", plan.sql);
    let last = plan.sql.rsplit("\nSELECT").next().unwrap();
    assert!(last.contains("WHERE\n\tA0.msr") && last.contains(" > 0"), "{}", plan.sql);
}
