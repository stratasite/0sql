//! A filter comparing a field with another field: `Ship Date > Date`,
//! `Sales Price > Wholesale Cost`, `Sales Price > Current Price`. Two
//! dimensions compare row by row in every universe that aggregates; a
//! measure compares at the query's grain, which is the one aggregation's
//! HAVING or the final merge's WHERE when its sides come from different
//! universes or a rule sets one apart.

use serde_json::{json, Value};
use std::path::PathBuf;
use std::sync::OnceLock;
use zsql_dialect::Dialects;
use zsql_semantic::SemanticModel;

fn model(name: &str) -> SemanticModel {
    let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects").join(name);
    let loaded = zsql_project::load(&dir, &Dialects::builtin()).unwrap();
    SemanticModel::build(loaded.snapshot).unwrap()
}

fn tpcds() -> &'static SemanticModel {
    static MODEL: OnceLock<SemanticModel> = OnceLock::new();
    MODEL.get_or_init(|| model("tpcds"))
}

fn grocery() -> &'static SemanticModel {
    static MODEL: OnceLock<SemanticModel> = OnceLock::new();
    MODEL.get_or_init(|| model("grocery-store"))
}

fn plan_on(semantic: &SemanticModel, spec: Value) -> Result<String, String> {
    let dialects = Dialects::builtin();
    let spec = zsql_spec::Spec::from_value(spec).map_err(|e| e.to_string())?;
    let base = dialects.base().map_err(|e| e.to_string())?;
    let query = zsql_spec::lower(&spec, semantic, &base).map_err(|e| e.to_string())?;
    let planner = zsql_planner::Planner::new(semantic, &dialects, None);
    planner.plan_query(query, None).map(|p| p.sql).map_err(|e| e.to_string())
}

fn plan(spec: Value) -> Result<String, String> {
    plan_on(tpcds(), spec)
}

fn by_category(measures: &[&str], filters: Value) -> Value {
    let mut projections = vec![json!({"field": "Item Category"})];
    projections.extend(measures.iter().map(|m| json!({"field": m})));
    json!({"projections": projections, "filters": filters})
}

/// The final SELECT, after the last CTE.
fn last(sql: &str) -> &str {
    sql.rsplit("\n)\nSELECT").next().unwrap()
}

/// The CTE bodies, each up to its closing parenthesis.
fn ctes(sql: &str) -> Vec<&str> {
    let body = sql.rsplit_once("\n)\nSELECT").map(|(b, _)| b).unwrap_or("");
    body.split("\n), ").collect()
}

#[test]
fn two_dimensions_compare_in_the_where_clause_over_both_roles_of_a_date() {
    let sql = plan(by_category(
        &["Catalog Sales Price"],
        json!([{"field": "Catalog Ship Date", "predicate": "greater_than", "value_field": "Date"}]),
    ))
    .unwrap();
    assert!(sql.contains("ON T0.cs_ship_date_sk = ") && sql.contains("ON T0.cs_sold_date_sk = "), "{sql}");
    let filter = sql.split("WHERE\n\t").nth(1).unwrap().lines().next().unwrap();
    assert!(filter.contains("\"d_date\" > ") && filter.matches("\"d_date\"").count() == 2, "{sql}");
    assert!(!sql.contains("HAVING"), "{sql}");
}

#[test]
fn two_measures_of_one_universe_compare_in_its_having() {
    let sql = plan(by_category(
        &["Catalog Sales Price"],
        json!([{"field": "Catalog Sales Price", "predicate": "greater_than", "value_field": "Catalog Wholesale Cost"}]),
    ))
    .unwrap();
    assert!(sql.contains("HAVING\n\tsum(T0.\"cs_sales_price\") > sum(T0.\"cs_wholesale_cost\")"), "{sql}");
}

#[test]
fn two_measures_of_different_universes_compare_after_the_merge() {
    let sql = plan(by_category(
        &["Catalog Sales Price", "Store Sales Price"],
        json!([{"field": "Catalog Sales Price", "predicate": "greater_than", "value_field": "Store Sales Price"}]),
    ))
    .unwrap();
    for cte in ctes(&sql) {
        assert!(!cte.contains("HAVING"), "no child holds both sides: {sql}");
    }
    let root = last(&sql);
    assert!(root.contains("FULL OUTER JOIN") && root.contains("WHERE\n\tA0.") && root.contains(" > A1."), "{sql}");
}

#[test]
fn a_measure_compared_with_but_not_projected_is_a_column_of_its_own_universe() {
    let sql = plan(by_category(
        &["Catalog Sales Price"],
        json!([{"field": "Catalog Sales Price", "predicate": "greater_than", "value_field": "Store Net Paid"}]),
    ))
    .unwrap();
    assert!(ctes(&sql).iter().any(|c| c.contains("FROM\n\tstore_sales") && c.contains("sum(T0.\"ss_net_paid\")")), "{sql}");
    let root = last(&sql);
    assert!(!root.contains("Store Net Paid"), "the comparison reads it but the query does not show it: {sql}");
    assert!(root.contains("WHERE\n\tA0.") && root.contains(" > A1."), "{sql}");
}

#[test]
fn a_measure_on_the_value_side_moves_to_the_field_and_the_operator_turns() {
    let sql = plan(json!({
        "projections": [{"field": "Item Category"}, {"field": "Item Current Price"}, {"field": "Store Sales Price"}],
        "filters": [{"field": "Item Current Price", "predicate": "less_than", "value_field": "Store Sales Price"}]
    }))
    .unwrap();
    assert!(sql.contains("HAVING\n\tsum(T0.\"ss_sales_price\") > T1.\"i_current_price\""), "{sql}");
}

#[test]
fn a_measure_compares_with_a_dimension_only_at_a_grain_holding_it() {
    let err = plan(by_category(
        &["Store Sales Price"],
        json!([{"field": "Store Sales Price", "predicate": "greater_than", "value_field": "Item Current Price"}]),
    ))
    .unwrap_err();
    assert!(err.contains("needs Item Current Price among the query's dimensions"), "{err}");
    // A decorated projection groups by something else than the field.
    let err = plan(json!({
        "projections": [
            {"field": "Item Current Price", "decorators": [{"type": "customize", "custom_sql": "round(@expression)"}]},
            {"field": "Store Sales Price"}
        ],
        "filters": [{"field": "Store Sales Price", "predicate": "greater_than", "value_field": "Item Current Price"}]
    }))
    .unwrap_err();
    assert!(err.contains("needs Item Current Price among the query's dimensions, undecorated"), "{err}");
}

#[test]
fn strings_compare_case_insensitively_by_a_search_not_a_pattern() {
    let sql = plan(by_category(
        &["Catalog Sales Price"],
        json!([
            {"field": "Billed Customer Address City", "predicate": "does_not_contain", "value_field": "Customer Address City"},
            {"field": "Billed Customer Address State", "predicate": "equals", "value_field": "Customer Address State"}
        ]),
    ))
    .unwrap();
    assert!(sql.contains("NOT (STRPOS(LOWER(T"), "{sql}");
    assert!(sql.contains("\"ca_state\") = LOWER(T"), "{sql}");
    assert!(!sql.contains("LIKE"), "{sql}");
}

#[test]
fn between_two_fields_reads_both_bounds() {
    let sql = plan(by_category(
        &["Catalog Sales Price"],
        json!([{"field": "Date", "predicate": "between", "value_field": "Catalog Ship Date", "value_end_field": "Catalog Ship Date"}]),
    ))
    .unwrap();
    let filter = sql.split("WHERE\n\t").nth(1).unwrap().lines().next().unwrap();
    assert!(filter.contains(" BETWEEN T") && filter.contains(" AND T"), "{sql}");
}

#[test]
fn a_comparison_refuses_what_it_cannot_compare() {
    let refused = |filter: Value, needle: &str| {
        let err = plan(by_category(&["Catalog Sales Price"], json!([filter]))).unwrap_err();
        assert!(err.contains(needle), "{needle:?} not in {err}");
    };
    refused(
        json!({"field": "Date", "predicate": "greater_than", "value_field": "Item Category"}),
        "Date is a date and Item Category is a string",
    );
    refused(
        json!({"field": "Item Category", "predicate": "greater_than", "value_field": "Item Class"}),
        "does not compare two string fields",
    );
    refused(json!({"field": "Date", "predicate": "in_list", "value_field": "Catalog Ship Date"}), "does not compare two date fields");
    refused(json!({"field": "Date", "predicate": "greater_than", "value_field": "Date"}), "compares Date with itself");
    refused(json!({"field": "Date", "predicate": "greater_than", "value": "2024-01-01", "value_field": "Catalog Ship Date"}), "not both");
    refused(json!({"field": "Date", "predicate": "between", "value_field": "Catalog Ship Date"}), "needs value_end_field");
    refused(
        json!({"field": "Date", "predicate": "greater_than", "value_field": "Catalog Ship Date", "value_end_field": "Date"}),
        "only for between",
    );
    refused(
        json!({"field": "Item Current Price", "predicate": "between", "value_field": "Catalog Sales Price", "value_end_field": "Catalog List Price"}),
        "Between with a measure for a bound",
    );
}

#[test]
fn a_comparison_reads_back_as_the_spec_it_lowered_from() {
    let dialects = Dialects::builtin();
    let spec = zsql_spec::Spec::from_value(by_category(
        &["Store Sales Price", "Item Current Price"],
        json!([{"field": "Item Current Price", "predicate": "less_than", "value_field": "Store Sales Price"}]),
    ))
    .unwrap();
    let query = zsql_spec::lower(&spec, tpcds(), &dialects.base().unwrap()).unwrap();
    let back = query.to_spec().to_value();
    let filter = &back["filters"][0];
    assert_eq!(filter["field"], "store-sales-price");
    assert_eq!(filter["predicate"], "greater_than");
    assert_eq!(filter["value_field"], "item-current-price@d");
    assert!(filter.get("value").is_none(), "{back}");
    let again = zsql_spec::lower(&zsql_spec::Spec::from_value(back.clone()).unwrap(), tpcds(), &dialects.base().unwrap()).unwrap();
    assert_eq!(again.to_spec().to_value(), back);
}

#[test]
fn a_share_compares_at_the_root_and_never_in_its_denominator() {
    let sql = plan(json!({
        "projections": [
            {"field": "Item Category"}, {"field": "Item Class"},
            {"field": "Store Sales Price", "decorators": [{"type": "contribute", "partition_refs": ["Item Category"]}]},
            {"field": "Store Net Paid"}
        ],
        "filters": [{"field": "Store Net Paid", "predicate": "greater_than", "value_field": "Store Wholesale Cost"}]
    }))
    .unwrap();
    for cte in ctes(&sql) {
        assert!(!cte.contains("HAVING"), "{sql}");
    }
    assert!(last(&sql).contains("WHERE\n\tA0.") && last(&sql).contains(" > A0."), "{sql}");
}

#[test]
fn a_comparison_goes_to_the_merge_even_where_measure_filters_stay_below_it() {
    let mut spec = by_category(
        &["Catalog Sales Price", "Store Sales Price"],
        json!([
            {"field": "Catalog Sales Price", "predicate": "greater_than", "value_field": "Store Sales Price"},
            {"field": "Store Sales Price", "predicate": "greater_than", "value": 10}
        ]),
    );
    spec["db_settings"] = json!({"cross_universe_measure_filtering_strategy": "intermediate"});
    let sql = plan(spec).unwrap();
    assert!(ctes(&sql).iter().any(|c| c.contains("HAVING\n\tsum(T0.\"ss_sales_price\") > 10")), "{sql}");
    assert!(last(&sql).contains("WHERE\n\tA0.") && !last(&sql).contains("> 10"), "{sql}");
}

#[test]
fn a_segment_compares_dimensions_and_refuses_measures() {
    let segment = |filter: Value| {
        by_category(&["Catalog Sales Price"], json!([]))
            .as_object()
            .cloned()
            .map(|mut m| {
                m.insert(
                    "segments".into(),
                    json!([{"name": "s", "keys": ["Item Brand ID"], "measures": ["Catalog Quantity"], "filters": [filter]}]),
                );
                Value::Object(m)
            })
            .unwrap()
    };
    let sql = plan(segment(json!({"field": "Catalog Ship Date", "predicate": "greater_than", "value_field": "Date"}))).unwrap();
    assert!(sql.contains("\"d_date\" > T"), "{sql}");
    let err = plan(segment(json!({"field": "Catalog Sales Price", "predicate": "greater_than", "value_field": "Catalog Wholesale Cost"})))
        .unwrap_err();
    assert!(err.contains("a segment's filters compare dimensions only"), "{err}");
}

/// Total Sales All Products ignores filters on Product: a comparison of
/// two product fields is set aside for it like any product filter, and
/// the comparison of the two measures waits for the merge.
#[test]
fn a_rule_that_ignores_a_table_sets_aside_a_comparison_on_it() {
    let sql = plan_on(
        grocery(),
        json!({
            "projections": [{"field": "Department"}, {"field": "Sales Value"}, {"field": "Total Sales All Products"}],
            "filters": [
                {"field": "Sales Value", "predicate": "less_than", "value_field": "Total Sales All Products"},
                {"field": "Commodity", "predicate": "does_not_contain", "value_field": "Sub Commodity"}
            ]
        }),
    )
    .unwrap();
    let ctes = ctes(&sql);
    assert_eq!(ctes.iter().filter(|c| c.contains("NOT (STRPOS(")).count(), 1, "{sql}");
    assert!(ctes.iter().any(|c| !c.contains("dim_product") && !c.contains("STRPOS")), "{sql}");
    assert!(last(&sql).contains("WHERE\n\tA0.") && last(&sql).contains(" < A1."), "{sql}");
}
