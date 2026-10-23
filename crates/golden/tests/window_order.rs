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
    // By its whole id: two cases share that prefix, with different universes.
    let base_case = cases
        .values()
        .flatten()
        .find(|c| c.id == "BasketPairTest#test_dedupe_lt_keeps_one_canonical_ordering_per_pair#1")
        .expect("the basket-pair case");

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
    // By its whole id: two cases share that prefix, with different universes.
    let base_case = cases
        .values()
        .flatten()
        .find(|c| c.id == "BasketPairTest#test_dedupe_lt_keeps_one_canonical_ordering_per_pair#1")
        .expect("the basket-pair case");

    let sql = sql_for(&fx, &base_case.universe, "Product Name, rank(Web Net Paid order by Product Name asc)");
    let line = sql.lines().find(|l| l.to_uppercase().contains("RANK()")).unwrap_or_else(|| panic!("a rank column: {sql}"));
    assert!(line.to_lowercase().contains("asc"), "the author's own order: {line}");
    assert!(!line.to_lowercase().contains("desc"), "and not the default: {line}");
}

/// `last_value` is the trap in the rank family's neighbourhood: an ORDER BY
/// with no frame means "up to this row", so the function answers with the
/// row it is on. Where the order is ours, the frame has to be ours too.
#[test]
fn last_value_gets_the_frame_it_needs_when_the_order_is_ours() {
    let fx = Fixtures::open(Fixtures::default_root());
    let cases = fx.cases().unwrap();
    let base_case = cases
        .values()
        .flatten()
        .find(|c| c.id == "BasketPairTest#test_dedupe_lt_keeps_one_canonical_ordering_per_pair#1")
        .expect("the basket-pair case");

    let sql = sql_for(&fx, &base_case.universe, "Product Name, last_value(Web Net Paid)");
    let line = sql.lines().find(|l| l.to_lowercase().contains("last_value")).unwrap_or_else(|| panic!("a last_value column: {sql}"));
    assert!(line.to_uppercase().contains("ORDER BY"), "ordered: {line}");
    assert!(
        line.to_uppercase().contains("UNBOUNDED PRECEDING AND UNBOUNDED FOLLOWING"),
        "over the whole partition, or it answers with the current row: {line}"
    );

    // A rank needs no frame: the default one is what ranking means.
    let sql = sql_for(&fx, &base_case.universe, "Product Name, rank(Web Net Paid)");
    let line = sql.lines().find(|l| l.to_uppercase().contains("RANK()")).unwrap();
    assert!(!line.to_uppercase().contains("UNBOUNDED"), "no frame on a rank: {line}");
}
