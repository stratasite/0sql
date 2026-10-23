//! A hint names a table. It once resolved by uid alone, through a filter
//! that dropped whatever it could not find, so a hint written the way the
//! shorthand invites you to write it steered nothing and said nothing.

use zsql_golden::Fixtures;

fn plan_line(fx: &Fixtures, universe_id: &str, line: &str) -> Result<String, String> {
    let universe = fx.universe(universe_id).unwrap();
    let (spec, _) = zsql_shorthand::parse_line(line).expect("the line parses");
    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&zsql_spec::Spec::from_value(spec).unwrap(), &universe, &base).map_err(|e| e.to_string())?;
    zsql_planner::Planner::new(&universe, fx.dialects(), None).plan_query(query, None).map(|p| p.sql).map_err(|e| e.to_string())
}

#[test]
fn a_hint_resolves_by_name_as_well_as_by_uid() {
    let fx = Fixtures::open(Fixtures::default_root());
    let cases = fx.cases().unwrap();
    let base_case = cases.values().flatten().find(|c| c.id.starts_with("BasketPairTest#test_dedupe_lt")).expect("a basket-pair case");

    // The table is named "Isolated Web Sales" and its uid is isolated_web_sales.
    let by_uid = plan_line(&fx, &base_case.universe, "Product Name, Web Net Paid, hints (isolated_web_sales)").expect("plans");
    let by_name = plan_line(&fx, &base_case.universe, "Product Name, Web Net Paid, hints (Isolated Web Sales)").expect("plans");
    assert_eq!(by_uid, by_name, "the same hint written either way steers the same plan");
}

#[test]
fn a_hint_that_names_no_table_refuses() {
    let fx = Fixtures::open(Fixtures::default_root());
    let cases = fx.cases().unwrap();
    let base_case = cases.values().flatten().find(|c| c.id.starts_with("BasketPairTest#test_dedupe_lt")).expect("a basket-pair case");

    let err = plan_line(&fx, &base_case.universe, "Product Name, Web Net Paid, hints (Isolated Web Sails)")
        .expect_err("a misspelled hint is a mistake, not a preference");
    assert!(err.contains("Isolated Web Sails"), "it names the hint: {err}");
    assert!(err.contains("Isolated Web Sales"), "and the table it nearly matched: {err}");
}
