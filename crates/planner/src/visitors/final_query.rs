//! Rails `FinalQueryVisitor`: intermediate nodes become CTEs (or inlined
//! subqueries) around the root statement.

use crate::dag::Graph;
use crate::{PlanError, Result};

/// The statement, and the offset in it where the root's own statement
/// begins: zero, or past the common table expressions wrapped around it.
/// A caller's row limit applies to that statement and not to a CTE.
pub fn to_sql(graph: &Graph) -> Result<(String, usize)> {
    let root = graph.root();
    let mut intermediates: Vec<(String, String)> = Vec::new();
    for id in graph.post_order() {
        if id == root {
            continue;
        }
        let alias = graph.nodes[id].alias(graph);
        let sql = graph.nodes[id].final_sql(&graph.dialect)?;
        if !intermediates.iter().any(|(a, _)| *a == alias) {
            intermediates.push((alias, sql));
        }
    }
    let last = graph.nodes[root].final_sql(&graph.dialect)?;
    if intermediates.is_empty() {
        return Ok((last, 0));
    }
    let d = &graph.dialect;
    if d.temp_table_type() == "cte" && d.supports_cte() {
        let mut sql = String::new();
        for (a, inter) in &intermediates {
            if sql.is_empty() {
                sql.push_str(&format!("WITH {a} AS (\n{inter}\n)"));
            } else {
                sql.push_str(&format!(", {a} AS (\n{inter}\n)"));
            }
        }
        sql.push('\n');
        let root_at = sql.len();
        sql.push_str(&last);
        Ok((sql, root_at))
    } else if d.temp_table_type() == "subquery" && d.supports_sub_queries() {
        // The root statement is the whole string here: the intermediates
        // are spliced into it, not placed in front of it.
        let mut sql = last;
        for (a, inter) in &intermediates {
            sql = sql.replace(a, &format!("(\n{inter}\n)"));
        }
        Ok((sql, 0))
    } else {
        Err(PlanError::Resolution("The datasource doesn't support cross universe joins. Could not produce final SQL query.".into()))
    }
}
