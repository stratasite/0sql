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

    assert!(
        !plan.sql.trim_end().to_uppercase().ends_with("LIMIT 5000"),
        "the plan's own SQL must not carry the spec's limit:\n{}",
        plan.sql
    );
    let dialect = dialects.get(&plan.adapter).expect("the plan's adapter has a dialect");
    assert_eq!(plan.sql_with_limit(None, &dialect).unwrap(), plan.sql, "no limit leaves the statement alone");

    // Where it goes is the dialect's business; that it goes in exactly once,
    // and into the statement the plan ends with, is not.
    let limited = plan.sql_with_limit(Some(25), &dialect).unwrap();
    assert_ne!(limited, plan.sql, "a limit of 25 must reach the statement");
    assert_eq!(limited[..plan.root_at], plan.sql[..plan.root_at], "anything before the root statement is untouched");
    match dialect.str("limit_style").unwrap() {
        "top" => assert!(limited[plan.root_at..].starts_with("SELECT TOP (25)"), "{limited}"),
        _ => assert!(limited.trim_end().ends_with("\nLIMIT 25"), "{limited}"),
    }
}

/// Every golden case, planned, takes its limit without the statement
/// growing a second one and without the limit landing in a CTE. The
/// per-dialect rendering is the dialect crate's own test; this is that the
/// offset a plan reports is the offset its statement actually has.
#[test]
fn every_planned_case_takes_a_limit_in_one_place() {
    let fx = Fixtures::open(Fixtures::default_root());
    let dialects = fx.dialects();
    let base = dialects.base().unwrap();
    let mut checked = 0;

    for (_, cases) in fx.cases().unwrap() {
        for case in &cases {
            let Ok((universe, paths)) = fx.planner_for(&case.universe, PathSource::Formed) else { continue };
            let planner = zsql_planner::Planner::new(&universe, dialects, paths.as_deref().map(Vec::as_slice));
            let Ok(spec) = Spec::from_value(case.spec.clone()) else { continue };
            let Ok(query) = lower(&spec, &universe, &base) else { continue };
            let Ok(plan) = planner.plan_query(query, case.context.as_ref()) else { continue };
            let dialect = dialects.get(&plan.adapter).unwrap();

            assert!(plan.root_at <= plan.sql.len(), "{}: root_at past the end", case.id);
            assert!(
                plan.sql[plan.root_at..].starts_with("SELECT"),
                "{}: the statement at root_at must be the root SELECT:\n{}",
                case.id,
                &plan.sql[plan.root_at..plan.sql.len().min(plan.root_at + 40)]
            );
            let limited = plan.sql_with_limit(Some(25), &dialect).unwrap();
            let marker = if dialect.str("limit_style").unwrap() == "top" { "TOP (25)" } else { "LIMIT 25" };
            assert_eq!(limited.matches(marker).count(), 1, "{}: exactly one limit\n{limited}", case.id);
            checked += 1;
        }
    }
    assert!(checked > 100, "only {checked} cases planned and were checked");
}
