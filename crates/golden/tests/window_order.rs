//! A rank over an unordered window ranks every row the same, so a rank
//! function with no explicit order takes the measure it decorates. The
//! functions that keep the expression as their argument take the date.

use zsql_golden::Fixtures;

fn sql_for(fx: &Fixtures, universe_id: &str, line: &str) -> String {
    let universe = fx.universe(universe_id).unwrap();
    let (spec, _) = zsql_shorthand::parse_line(line).expect("the line parses");
    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&zsql_spec::Spec::from_value(spec).unwrap(), &universe, &base).expect("resolves");
    zsql_planner::Planner::new(&universe, fx.dialects(), None).plan_query(query, None).expect("plans").sql
}

#[test]
fn a_rank_with_no_order_ranks_by_what_it_decorates() {
    let fx = Fixtures::open(Fixtures::default_root());
    let cases = fx.cases().unwrap();
    let base_case = cases.values().flatten().find(|c| c.id.starts_with("BasketPairTest#test_dedupe_lt")).expect("a basket-pair case");

    let sql = sql_for(&fx, &base_case.universe, "Product Name, rank(Web Net Paid)");
    let line = sql.lines().find(|l| l.to_uppercase().contains("RANK()")).unwrap_or_else(|| panic!("a rank column: {sql}"));
    assert!(!line.contains("OVER ()"), "the window is ordered: {line}");
    assert!(line.to_uppercase().contains("ORDER BY"), "by the decorated measure, descending: {line}");
    assert!(line.to_lowercase().contains("desc"), "descending: {line}");
}

#[test]
fn an_explicit_order_still_wins() {
    let fx = Fixtures::open(Fixtures::default_root());
    let cases = fx.cases().unwrap();
    let base_case = cases.values().flatten().find(|c| c.id.starts_with("BasketPairTest#test_dedupe_lt")).expect("a basket-pair case");

    let sql = sql_for(&fx, &base_case.universe, "Product Name, rank(Web Net Paid order by Product Name asc)");
    let line = sql.lines().find(|l| l.to_uppercase().contains("RANK()")).unwrap_or_else(|| panic!("a rank column: {sql}"));
    assert!(line.to_lowercase().contains("asc"), "the author's own order: {line}");
    assert!(!line.to_lowercase().contains("desc"), "and not the default: {line}");
}
