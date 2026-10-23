//! An expanding segment whose carried dimension is aliased, written in the
//! shorthand rather than as JSON. The spec spells that alias `as`; the
//! shorthand once wrote `alias`, and since the spec refuses unknown fields
//! the line parsed, described itself correctly and then could not plan.

use zsql_golden::Fixtures;

#[test]
fn a_shorthand_expand_alias_reaches_the_plan() {
    let fx = Fixtures::open(Fixtures::default_root());
    let cases = fx.cases().unwrap();
    let base_case = cases.values().flatten().find(|c| c.id.starts_with("BasketPairTest#test_dedupe_lt")).expect("a basket-pair case");
    let universe = fx.universe(&base_case.universe).unwrap();

    let line = "Product Name, Web Net Paid, \
                expand (Customer Id) with (Product Name as Also Bought) dedupe lt as Customer Products";
    let (spec, _items) = zsql_shorthand::parse_line(line).expect("the line parses");
    assert_eq!(spec["segments"][0]["expanding"][0]["as"], "Also Bought", "the shorthand writes the spec's own key");

    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&zsql_spec::Spec::from_value(spec).unwrap(), &universe, &base).expect("the spec resolves");
    let plan = zsql_planner::Planner::new(&universe, fx.dialects(), None).plan_query(query, None).expect("it plans");

    assert!(plan.sql.contains("Also Bought"), "the carried column takes the alias: {}", plan.sql);
    assert!(!plan.sql.contains("(same "), "and not the default name it would have had: {}", plan.sql);
}
