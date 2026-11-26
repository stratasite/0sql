//! A result column comes back under the name the plan gives it, and BigQuery
//! refuses parentheses and operators in that name even inside backticks. A
//! golden case with a truncated date (`Month(Date)`) and a calculation the
//! shorthand left unaliased, replanned on a BigQuery datasource, must name
//! both columns in words BigQuery takes. The goldens' own dialect keeps the
//! names as they are.

use zsql_golden::{Expected, Fixtures, Variant};
use zsql_semantic::SemanticModel;

#[test]
fn bigquery_gets_column_names_it_accepts() {
    let fx = Fixtures::open(Fixtures::default_root());
    let case = fx
        .cases()
        .unwrap()
        .into_values()
        .flatten()
        .find(|c| c.variant == Variant::Main && matches!(&c.expected, Expected::Sql { sql } if sql.contains("\"Month(Date)\"")))
        .expect("a golden case that truncates Date to month");

    let mut snapshot = fx.snapshot(&case.universe).unwrap();
    for ds in &mut snapshot.datasources {
        ds.adapter = "bigquery".into();
    }
    let semantic = SemanticModel::build(snapshot).unwrap();

    let mut spec = case.spec.clone();
    let (calc, _) = zsql_shorthand::parse_line("calc(count(*), integer)").unwrap();
    spec["calculations"] = calc["calculations"].clone();

    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&zsql_spec::Spec::from_value(spec).unwrap(), &semantic, &base).expect("the spec resolves");
    let plan = zsql_planner::Planner::new(&semantic, fx.dialects(), None).plan_query(query, case.context.as_ref()).expect("it plans");

    assert_eq!(plan.adapter, "bigquery");
    assert!(plan.sql.contains("AS `Month_Date`"), "the truncated date is renamed: {}", plan.sql);
    assert!(plan.sql.contains("AS `count`"), "the calculation is named by its words: {}", plan.sql);
    assert!(!plan.sql.contains("`Month(Date)`") && !plan.sql.contains("`count(*)`"), "{}", plan.sql);
}
