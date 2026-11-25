//! A calculation that aggregates only dimensions, as
//! `count(distinct [Customer ID]@d)` does, is a measure: one aggregation
//! computes it over base rows, and the dimensions it reads never join the
//! grain of the nodes beside it.

use std::path::PathBuf;
use std::sync::OnceLock;
use zsql_dialect::Dialects;
use zsql_semantic::SemanticModel;

fn semantic() -> &'static SemanticModel {
    static MODEL: OnceLock<SemanticModel> = OnceLock::new();
    MODEL.get_or_init(|| {
        let dir = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join("../../fixtures/projects/tpcds");
        let loaded = zsql_project::load(&dir, &Dialects::builtin()).unwrap();
        SemanticModel::build(loaded.snapshot).unwrap()
    })
}

fn plan(spec: serde_json::Value) -> Result<String, String> {
    let dialects = Dialects::builtin();
    let spec = zsql_spec::Spec::from_value(spec).map_err(|e| e.to_string())?;
    let base = dialects.base().map_err(|e| e.to_string())?;
    let query = zsql_spec::lower(&spec, semantic(), &base).map_err(|e| e.to_string())?;
    let planner = zsql_planner::Planner::new(semantic(), &dialects, None);
    planner.plan_query(query, None).map(|p| p.sql).map_err(|e| e.to_string())
}

fn customers() -> serde_json::Value {
    serde_json::json!({"calculation": true, "alias": "Customers", "sql": "count(distinct [Customer ID]@d)", "data_type": "integer"})
}

fn catalog_buyers(apply_to: &[&str]) -> serde_json::Value {
    let mut s = serde_json::json!({
        "keys": ["customer id"],
        "filters": [{"field": "catalog net paid", "predicate": "greater_than", "value": 0}]
    });
    if !apply_to.is_empty() {
        s["apply_to"] = serde_json::json!(apply_to);
    }
    s
}

/// The CTE bodies and the final select, split on the CTE boundaries.
fn parts(sql: &str) -> Vec<&str> {
    sql.split("\n), ").flat_map(|p| p.split("\n)\nSELECT")).collect()
}

fn part_with<'s>(sql: &'s str, needle: &str) -> &'s str {
    let found: Vec<&str> = parts(sql).into_iter().filter(|p| p.contains(needle) && p.contains("FROM\n\tstore_sales")).collect();
    assert_eq!(found.len(), 1, "one store_sales node holds {needle}:\n{sql}");
    found[0]
}

#[test]
fn a_dimension_measure_beside_a_measure_filter_keeps_every_node_at_the_query_grain() {
    let sql = plan(serde_json::json!({
        "projections": [{"field": "store net paid"}, {"field": "store quantity"}, customers()],
        "filters": [{"field": "catalog net paid", "predicate": "greater_than", "value": 0}]
    }))
    .unwrap();
    // The measures are sums, read as they are: nothing is grouped by the
    // customer and nothing takes a max over per-customer rows.
    assert!(!sql.contains("GROUP BY"), "{sql}");
    assert!(!sql.contains("max("), "{sql}");
    let store = part_with(&sql, "count(distinct");
    assert!(store.contains("sum(T0.\"ss_net_paid\")") && store.contains("sum(T0.\"ss_quantity\")"), "{sql}");
    // The filter still tests the catalog total, in its own ungrouped node.
    assert!(sql.contains("FROM\n\tcatalog_sales T0\nHAVING\n\tsum(T0.\"cs_net_paid\") > 0"), "{sql}");
}

#[test]
fn a_segment_on_one_measure_leaves_the_dimension_measure_beside_it_alone() {
    let sql = plan(serde_json::json!({
        "projections": [{"field": "store net paid"}, {"field": "store quantity"}, customers()],
        "segments": [catalog_buyers(&["store net paid"])]
    }))
    .unwrap();
    let plain = part_with(&sql, "count(distinct");
    assert!(plain.contains("sum(T0.\"ss_quantity\")") && !plain.contains("JOIN seg"), "{sql}");
    let segmented = part_with(&sql, "sum(T0.\"ss_net_paid\")");
    assert!(segmented.contains("JOIN seg") && !segmented.contains("count(distinct"), "{sql}");
}

#[test]
fn a_dimension_measure_forks_from_a_segmented_sibling_when_it_is_the_only_plain_member() {
    let sql = plan(serde_json::json!({
        "projections": [{"field": "store net paid"}, customers()],
        "segments": [catalog_buyers(&["store net paid"])]
    }))
    .unwrap();
    assert!(!part_with(&sql, "count(distinct").contains("JOIN seg"), "{sql}");
    assert!(part_with(&sql, "sum(T0.\"ss_net_paid\")").contains("JOIN seg"), "{sql}");
}

#[test]
fn a_segment_applies_to_a_dimension_measure_by_its_alias() {
    let sql = plan(serde_json::json!({
        "projections": [{"field": "store net paid"}, customers()],
        "segments": [catalog_buyers(&["Customers"])]
    }))
    .unwrap();
    assert!(part_with(&sql, "count(distinct").contains("JOIN seg"), "{sql}");
    assert!(!part_with(&sql, "sum(T0.\"ss_net_paid\")").contains("JOIN seg"), "{sql}");
}

#[test]
fn a_grouping_merge_reads_a_hosted_dimension_measure_as_a_column() {
    let sql = plan(serde_json::json!({
        "projections": [
            {"field": "item category"},
            {"field": "store net paid"},
            {"field": "catalog net paid"},
            customers(),
            {"calculation": true, "alias": "Ratio", "sql": "sum([Store Net Paid]@m) / sum([Catalog Net Paid]@m)"}
        ]
    }))
    .unwrap();
    // One node counts the customers, grouped by the category alone; the
    // merge joins on the category alone and re-reads the count under its
    // own GROUP BY.
    assert_eq!(sql.matches("count(distinct").count(), 1, "{sql}");
    assert!(!sql.contains("c_customer_id\" AS"), "the customer is never a column:\n{sql}");
    let merge = sql.rsplit("\n)\nSELECT").next().unwrap();
    assert!(merge.lines().any(|l| l.trim_start().starts_with("max(") && l.contains("AS \"Customers\"")), "{sql}");
    assert!(merge.contains("GROUP BY"), "{sql}");
}

#[test]
fn a_query_of_one_universe_still_plans_as_one_select() {
    let sql = plan(serde_json::json!({
        "projections": [{"field": "store net paid"}, {"field": "store quantity"}, customers()]
    }))
    .unwrap();
    assert!(!sql.contains("WITH"), "{sql}");
    assert!(sql.contains("count(distinct T1.\"c_customer_id\") AS \"Customers\""), "{sql}");
}

#[test]
fn a_dimension_measure_alone_aggregates_on_the_table_that_holds_the_dimension() {
    let sql = plan(serde_json::json!({ "projections": [customers()] })).unwrap();
    assert!(sql.contains("count(distinct T0.\"c_customer_id\") AS \"Customers\"\nFROM\n\tcustomer T0"), "{sql}");
}

#[test]
fn a_dimension_measure_no_measure_table_reaches_aggregates_on_a_table_of_its_own() {
    // Inventory cannot reach the customer, so the count comes from the
    // table that can; the two sit side by side.
    let sql = plan(serde_json::json!({
        "projections": [{"field": "inventory quantity on hand"}, customers()]
    }))
    .unwrap();
    assert!(sql.contains("count(distinct T0.\"c_customer_id\")") && sql.contains("FROM\n\tcustomer T0"), "{sql}");
    assert!(sql.contains("sum(T0.\"inv_quantity_on_hand\")"), "{sql}");
}

#[test]
fn a_prior_period_of_a_dimension_measure_aggregates_base_rows_in_a_shifted_node() {
    let mut c = customers();
    c["alias"] = "Customers LY".into();
    c["decorators"] = serde_json::json!([{"type": "temporalize", "transform": "year_over_year"}]);
    let sql = plan(serde_json::json!({
        "projections": [
            {"field": "date", "decorators": [{"type": "truncate", "grain": "year"}]},
            {"field": "store net paid"},
            c
        ]
    }))
    .unwrap();
    assert!(!sql.contains("[Customer ID]"), "the formula is never left unresolved:\n{sql}");
    let shifted = parts(&sql).into_iter().find(|p| p.contains("date_add(") && p.contains("FROM\n\tstore_sales")).expect("a shifted node");
    assert!(shifted.contains("count(distinct T1.\"c_customer_id\") AS \"__msrly_"), "{sql}");
}

#[test]
fn a_share_of_a_dimension_measure_divides_by_a_total_aggregated_on_its_own() {
    let mut c = customers();
    c["alias"] = "Customer share".into();
    c["decorators"] = serde_json::json!([{"type": "contribute"}]);
    let sql = plan(serde_json::json!({
        "projections": [{"field": "item category"}, {"field": "store net paid"}, c]
    }))
    .unwrap();
    assert!(!sql.contains("[Customer ID]"), "{sql}");
    assert!(sql.contains("count(distinct T1.\"c_customer_id\") AS \"__msrtotal_"), "{sql}");
    assert!(sql.lines().any(|l| l.contains("__msrtotal_") && l.contains("AS \"Customer share\"")), "{sql}");
}

#[test]
fn a_window_over_a_dimension_measure_runs_over_its_column_at_the_merge() {
    let mut c = customers();
    c["alias"] = "Running customers".into();
    c["decorators"] = serde_json::json!([{"type": "window", "function": "sum", "mode": "running", "order_by": {"item-category": "asc"}}]);
    let sql = plan(serde_json::json!({
        "projections": [{"field": "item category"}, {"field": "store net paid"}, {"field": "catalog net paid"}, c]
    }))
    .unwrap();
    assert!(!sql.contains("c_customer_id\" AS"), "the customer is never a column:\n{sql}");
    assert_eq!(sql.matches("count(distinct").count(), 1, "{sql}");
    assert!(sql.lines().any(|l| l.contains(" OVER (") && l.contains("AS \"Running customers\"")), "{sql}");
}

#[test]
fn a_dimension_used_beside_a_measure_stays_a_final_pass_calculation() {
    // The dimension is read row by row next to a measure, so this is not
    // a dimension measure and the planner still evaluates it at the merge.
    let sql = plan(serde_json::json!({
        "projections": [
            {"field": "item category"},
            {"field": "store net paid"},
            {"field": "catalog net paid"},
            {"calculation": true, "alias": "Actives", "data_type": "integer",
             "sql": "count(distinct case when [Store Net Paid]@m > 0 or [Catalog Net Paid]@m > 0 then [Item Category]@d else null end)"}
        ]
    }))
    .unwrap();
    let merge = sql.rsplit("\n)\nSELECT").next().unwrap();
    assert!(merge.contains("count(distinct case when"), "{sql}");
}

#[test]
fn a_measure_inside_an_aggregate_over_an_unprojected_dimension_is_refused() {
    // The measure would have to be totalled per customer before the count,
    // and nothing plans that; the old plan folded the customer into every
    // node and took a max of the measures beside it.
    let err = plan(serde_json::json!({
        "projections": [
            {"field": "store net paid"},
            {"calculation": true, "alias": "Big spenders", "data_type": "integer",
             "sql": "count(distinct case when [Store Net Paid]@m > 100 then [Customer ID]@d end)"}
        ]
    }))
    .unwrap_err();
    assert!(
        err.contains("\"Big spenders\"") && err.contains("Store Net Paid") && err.contains("inside an aggregate over Customer ID"),
        "{err}"
    );
    assert!(err.contains("segment keyed on Customer ID"), "{err}");
}

#[test]
fn the_same_refusal_reaches_a_measure_read_by_its_alias() {
    let err = plan(serde_json::json!({
        "projections": [
            {"field": "store net paid", "alias": "Paid"},
            {"calculation": true, "alias": "Big spenders", "data_type": "integer",
             "sql": "count(distinct case when [Paid] > 100 then [Customer ID]@d end)"}
        ]
    }))
    .unwrap_err();
    assert!(err.contains("inside an aggregate over Customer ID"), "{err}");
}

#[test]
fn a_measure_beside_an_aggregate_over_an_unprojected_dimension_is_refused_with_the_way_to_write_it() {
    let err = plan(serde_json::json!({
        "projections": [
            {"field": "store net paid"},
            {"calculation": true, "alias": "Per customer", "sql": "sum([Store Net Paid]@m) / count(distinct [Customer ID]@d)"}
        ]
    }))
    .unwrap_err();
    assert!(err.contains("\"Per customer\"") && err.contains("a calculation of its own") && err.contains("by its alias"), "{err}");

    // What the message says to write plans, and keeps the measure a sum.
    let sql = plan(serde_json::json!({
        "projections": [
            {"field": "item category"},
            {"field": "store net paid"},
            customers(),
            {"calculation": true, "alias": "Per customer", "sql": "sum([Store Net Paid]@m) / sum([Customers])"}
        ]
    }))
    .unwrap();
    assert!(!sql.contains("c_customer_id\" AS"), "the customer is never a column:\n{sql}");
    assert!(sql.lines().any(|l| l.contains(" / ") && l.contains("AS \"Per customer\"")), "{sql}");
}

#[test]
fn a_segment_on_the_dimension_measure_is_the_way_to_count_members_by_a_measure() {
    // Customers whose store total is over 100: the segment totals the
    // measure per customer, the dimension measure counts who is left.
    let sql = plan(serde_json::json!({
        "projections": [{"field": "store net paid"}, customers()],
        "segments": [{
            "keys": ["customer id"],
            "filters": [{"field": "store net paid", "predicate": "greater_than", "value": 100}],
            "apply_to": ["Customers"]
        }]
    }))
    .unwrap();
    assert!(sql.contains("HAVING\n\tsum(T0.\"ss_net_paid\") > 100"), "{sql}");
    assert!(part_with(&sql, "count(distinct").contains("JOIN seg"), "{sql}");
    assert!(!part_with(&sql, "sum(T0.\"ss_net_paid\") AS").contains("JOIN seg"), "{sql}");
}
