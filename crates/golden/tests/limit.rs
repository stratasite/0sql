//! A row limit belongs to the caller. The spec carries it, the planner
//! ignores it, and the statement gets it only when one was asked for — no
//! default is invented on the way through.

use zsql_golden::{Fixtures, PathSource};
use zsql_spec::{lower, Spec};

/// Takes the first golden case that plans, and lowers its spec three ways:
/// as written, with the limit removed, and with a limit of its own.
#[test]
fn a_limit_is_carried_only_when_the_caller_sends_one() {
    let fx = Fixtures::open(Fixtures::default_root());
    let dialects = fx.dialects();
    let base = dialects.base().unwrap();

    let (_, cases) = fx.cases().unwrap().into_iter().next().expect("a golden case file");
    let case = cases.first().expect("a golden case");
    let (universe, _) = fx.planner_for(&case.universe, PathSource::Formed).unwrap();

    // Every golden spec carries a limit; they were captured that way.
    assert!(case.spec.get("limit").is_some(), "this case was expected to carry a limit");
    let with = |limit: Option<i64>| {
        let mut value = case.spec.clone();
        let object = value.as_object_mut().unwrap();
        match limit {
            Some(n) => object.insert("limit".into(), serde_json::json!(n)),
            None => object.remove("limit"),
        };
        lower(&Spec::from_value(value).unwrap(), &universe, &base).unwrap()
    };

    assert_eq!(
        lower(&Spec::from_value(case.spec.clone()).unwrap(), &universe, &base).unwrap().limit,
        case.spec.get("limit").and_then(|v| v.as_i64()),
    );
    // Absent stays absent: nothing supplies 5000, or anything else.
    assert_eq!(with(None).limit, None, "a spec with no limit must lower to no limit");
    assert_eq!(with(Some(25)).limit, Some(25));
}

/// The statement itself: `sql` is the shape of the question and never ends in
/// a LIMIT; `sql_with_limit` is what a caller's limit turns it into.
#[test]
fn the_statement_carries_the_limit_only_through_sql_with_limit() {
    let fx = Fixtures::open(Fixtures::default_root());
    let dialects = fx.dialects();
    let base = dialects.base().unwrap();

    let (_, cases) = fx.cases().unwrap().into_iter().next().expect("a golden case file");
    let case = cases.first().expect("a golden case");
    let (universe, paths) = fx.planner_for(&case.universe, PathSource::Formed).unwrap();
    let planner = zsql_planner::Planner::new(&universe, dialects, paths.as_deref().map(Vec::as_slice));

    let query = lower(&Spec::from_value(case.spec.clone()).unwrap(), &universe, &base).unwrap();
    let plan = planner.plan_query(query, case.context.as_ref()).expect("the first golden case plans");

    assert!(!plan.sql.trim_end().to_uppercase().ends_with("LIMIT 5000"), "the plan's own SQL must not carry the spec's limit:\n{}", plan.sql);
    assert_eq!(plan.sql_with_limit(None), plan.sql, "no limit leaves the statement alone");
    assert!(plan.sql_with_limit(Some(25)).trim_end().ends_with("\nLIMIT 25"), "a limit is appended as its own clause");
}
