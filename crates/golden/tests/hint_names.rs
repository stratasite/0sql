//! A hint names a table. It once resolved by uid alone, through a filter
//! that dropped whatever it could not find, so a hint written the way the
//! shorthand invites you to write it steered nothing and said nothing.

use zsql_golden::Fixtures;

/// `Item Id` is reachable from several tables, and the planner's own choice
/// is `web_sales_sum`, so a hint naming another one has somewhere to move
/// the plan to. Without that, a test comparing two hinted plans cannot tell
/// "both steer the same" from "neither steers".
const LINE: &str = "Item Id";
const CASE: &str = "BasketPairTest#test_dedupe_lt_keeps_one_canonical_ordering_per_pair#1";

fn plan_line(fx: &Fixtures, universe_id: &str, line: &str) -> Result<String, String> {
    let universe = fx.universe(universe_id).unwrap();
    let (spec, _) = zsql_shorthand::parse_line(line).expect("the line parses");
    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&zsql_spec::Spec::from_value(spec).unwrap(), &universe, &base).map_err(|e| e.to_string())?;
    zsql_planner::Planner::new(&universe, fx.dialects(), None).plan_query(query, None).map(|p| p.sql).map_err(|e| e.to_string())
}

fn universe_of(fx: &Fixtures) -> String {
    fx.cases().unwrap().values().flatten().find(|c| c.id == CASE).expect("the basket-pair case").universe.clone()
}

#[test]
fn a_hint_steers_the_plan_and_every_spelling_of_it_steers_the_same() {
    let fx = Fixtures::open(Fixtures::default_root());
    let u = universe_of(&fx);

    // The hint has to change something, or nothing below means anything.
    let unhinted = plan_line(&fx, &u, LINE).expect("plans");
    let by_uid = plan_line(&fx, &u, &format!("{LINE}, hints (web_sales)")).expect("plans");
    assert_ne!(unhinted, by_uid, "the hint moves the plan");
    assert!(by_uid.contains("web_sales") && !unhinted.contains(" web_sales"), "to the table it names:\n{by_uid}\n{unhinted}");

    // The table's uid is web_sales. Every spelling of that name resolves to
    // it: the separators and the case are not part of what was meant.
    for spelling in ["web sales", "Web Sales", "web-sales", "websales", "WEB_SALES"] {
        let hinted = plan_line(&fx, &u, &format!("{LINE}, hints ({spelling})")).unwrap_or_else(|e| panic!("{spelling}: {e}"));
        assert_eq!(hinted, by_uid, "`{spelling}` steers the same plan as the uid");
    }
}

#[test]
fn a_hint_that_names_no_table_refuses_and_suggests_the_nearest() {
    let fx = Fixtures::open(Fixtures::default_root());
    let u = universe_of(&fx);

    // A typo in the spelling the resolver exists to accept is the case that
    // most needs a suggestion, so the near match is scored on normalized
    // forms rather than raw ones.
    let err = plan_line(&fx, &u, &format!("{LINE}, hints (web sails)")).expect_err("a misspelled hint is a mistake");
    assert!(err.contains("web sails"), "it names the hint: {err}");
    assert!(err.contains("web_sales"), "and the table it nearly matched: {err}");

    // Every unresolved hint gets its own suggestion, not just the first.
    let err = plan_line(&fx, &u, &format!("{LINE}, hints (zzzzzzzz, web sails)")).expect_err("still a mistake");
    assert!(err.contains("zzzzzzzz") && err.contains("web sails"), "both are named: {err}");
    assert!(err.contains("web_sales"), "and the one with a near match keeps it: {err}");
}
