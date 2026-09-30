//! A temporal transform on a calculation that references another calculation
//! by alias: the referenced formula is inlined with its measures moved to
//! the comparison period. Rails has no answer here (it raises), so this is
//! checked against the expected shape rather than a golden.

use zsql_golden::Fixtures;

#[test]
fn yoy_calculation_inlines_referenced_calculation() {
    let fx = Fixtures::open(Fixtures::default_root());
    let cases = fx.cases().unwrap();
    let base_case = cases.values().flatten().find(|c| c.id == "TemporalizerTest#test_yoy_calc_local_ref#1").expect("base case");
    let mut spec = base_case.spec.clone();

    // Add "Double" = [Net Paid]@m*2 and make the YoY calculation read it by alias.
    let projections = spec["projections"].as_array_mut().unwrap();
    let complex_at = projections.iter().position(|p| p["alias"] == "ComplexLocalCalc").expect("complex calc");
    projections[complex_at]["sql"] = "case when [Category] = 'Men' then [Double] else ([Net Paid]@m+[Local WNP])*2 end".into();
    projections.insert(complex_at, serde_json::json!({ "alias": "Double", "calculation": true, "sql": "[Net Paid]@m*2" }));

    let universe = fx.universe(&base_case.universe).unwrap();
    let base = fx.dialects().base().unwrap();
    let query = zsql_spec::lower(&zsql_spec::Spec::from_value(spec).unwrap(), &universe, &base).unwrap();
    let plan = zsql_planner::Planner::new(&universe, fx.dialects(), None).plan_query(query, None).unwrap();
    println!("{}", plan.sql);

    let line = plan.sql.lines().find(|l| l.contains("AS \"ComplexLocalCalc\"")).expect("calc column");
    assert!(line.contains("then (A"), "referenced calculation is inlined: {line}");
    assert!(line.contains("*2)"), "its measure is the comparison-period static: {line}");
    assert!(!line.contains("[Double]"), "no unresolved alias: {line}");
    let double_line = plan.sql.lines().find(|l| l.contains("AS \"Double\"")).expect("double column");
    assert!(double_line.contains("*2"), "the plain calculation still reads the current period: {double_line}");
}
