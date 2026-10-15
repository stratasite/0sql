//! Rails `SqlVisitor`: renders every node to SQL text, inputs first.

use crate::dag::{AliasKey, Entity, Filtered, Graph, Node, NodeId, Projected, RefKey, Reference};
use crate::model::{FilledTree, RQuery};
use crate::sql::dates::date_lit;
use crate::sql::expr::{entity_gsub, qualify, ungroupable};
use crate::sql::Builder;
use crate::{PlanError, Result};
use zsql_semantic::{DataType, Expression, Field, Predicate, Universe};
use zsql_spec::DecoratorKind;

pub fn run<'a>(graph: &Graph<'a>, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    for id in graph.post_order() {
        let sql = node_to_sql(graph, universe, q, id)?;
        *graph.nodes[id].sql.borrow_mut() = Some(sql);
    }
    Ok(())
}

struct Ctx<'g, 'a> {
    graph: &'g Graph<'a>,
    universe: &'a Universe,
    q: &'g RQuery<'a>,
}

fn node_to_sql<'a>(graph: &Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, id: NodeId) -> Result<String> {
    let cx = Ctx { graph, universe, q };
    let node = &graph.nodes[id];
    let mut sql = Builder::new();

    {
        let _t = crate::profile::span("sql.from");
        if node.is_merge() {
            for clause in nodes_to_from_clause(&cx, node)? {
                sql.from(clause);
            }
        } else {
            for clause in from_clauses(&cx, node)? {
                sql.from(clause);
            }
        }
    }

    {
        let _t = crate::profile::span("sql.select");
        for p in &node.projections {
            sql.select(projection_to_select_sql(&cx, p, node)?);
        }
    }

    {
        let _t = crate::profile::span("sql.where");
        translate_node_filters_to_sql(&cx, &mut sql, node)?;
    }

    if node.group_by() || q.db_setting_bool("force_group_by") {
        let _t = crate::profile::span("sql.group_by");
        for g in group_by_sqls(&cx, node)? {
            sql.group_by(g);
        }
    }

    for (ctx_dim, allowed) in &node.security_filters {
        let Some(allowed) = allowed else {
            // Denied: the node yields no rows.
            sql.r#where("1 = 0".to_string());
            continue;
        };
        let Some(context_ref) = node.field_refs.get(&RefKey::Field(ctx_dim.id)) else { continue };
        let context_sql = ref_to_sql_any(&cx, context_ref, node)?;
        let values: Vec<String> =
            allowed.iter().map(|v| graph.dialect.string_lit(&v.to_lowercase())).collect::<std::result::Result<_, _>>()?;
        sql.r#where(format!("LOWER({}) IN ({})", context_sql, values.join(", ")));
    }

    if graph.dialect.apply_advanced_filtering_on_array_projections() {
        apply_advanced_array_filter_handling(&cx, &mut sql, node)?;
    }

    {
        let _t = crate::profile::span("sql.strategies");
        for strategy in &node.strategies {
            apply_strategy(&cx, &mut sql, node, strategy)?;
        }
    }

    if node.order_by() {
        for p in node.projections.iter().filter(|p| p.proj.def.order_by.is_some()) {
            let mut order_sql = projection_to_sql(&cx, p, node)?;
            if reaggregate_wrap(&cx, p, node) {
                order_sql = format!("max({order_sql})");
            }
            sql.order(order_sql, p.proj.def.order_by.unwrap().sql());
        }
    }

    let _t = crate::profile::span("sql.render");
    Ok(sql.to_sql())
}

// ------------------------------------------------------------------ FROM

/// Rails `Sql::FromClauses#from_clauses`: the root table, then each join
/// once its left side is present.
fn from_clauses<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>) -> Result<Vec<String>> {
    let base = node.universe.clone().expect("query node universe");
    let root = base.root;
    let mut processed = vec![root.id];
    let mut clauses = vec![format!("\n\t{} {}", root.physical_name, node.table_alias(AliasKey::Table(root.id), 'T'))];

    let mut stack: std::collections::VecDeque<_> = node.from_clause_join_defs(cx.universe).into_iter().collect();
    let stack_size = stack.len();
    let mut missed = 1;
    while let Some(jd) = stack.pop_front() {
        if processed.contains(&jd.left_id) {
            let right = cx.universe.table(jd.right_id).expect("join right table");
            let left_alias = node.table_alias(AliasKey::Table(jd.left_id), 'T');
            let right_alias = node.table_alias(AliasKey::Table(jd.right_id), 'T');
            let join = to_join(join_type_name(jd.join_type), &right.physical_name, &right_alias)?;
            let cond = replace_ci(&replace_ci(&jd.sql, "left.", &format!("{left_alias}.")), "right.", &format!("{right_alias}."));
            clauses.push(format!("\n\t{join}\n\t\tON {cond}"));
            processed.push(jd.right_id);
        } else {
            stack.push_back(jd);
            if missed > stack_size {
                return Err(PlanError::Resolution("Exhausted join tree. Missing link.".into()));
            }
            missed += 1;
        }
    }
    Ok(clauses)
}

fn join_type_name(j: zsql_semantic::JoinType) -> &'static str {
    match j {
        zsql_semantic::JoinType::Inner => "inner",
        zsql_semantic::JoinType::Left => "left",
        zsql_semantic::JoinType::Right => "right",
    }
}

fn to_join(join_type: &str, table: &str, alias: &str) -> Result<String> {
    Ok(match join_type {
        "full" => format!("FULL OUTER JOIN {table} {alias}"),
        "inner" | "join" => format!("JOIN {table} {alias}"),
        "left" => format!("LEFT JOIN {table} {alias}"),
        "right" => format!("RIGHT JOIN {table} {alias}"),
        other => return Err(PlanError::Resolution(format!("Unsupported join type for Merge node: {other}"))),
    })
}

/// Case-insensitive literal replace, as Rails' `gsub(/left\./i, …)`.
/// The patterns are ASCII (`left.`, `right.`), so a byte-wise scan does.
fn replace_ci(s: &str, pat: &str, with: &str) -> String {
    let (hay, needle) = (s.as_bytes(), pat.as_bytes());
    if needle.is_empty() || hay.len() < needle.len() {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len());
    let mut i = 0;
    while i < hay.len() {
        if i + needle.len() <= hay.len() && hay[i..i + needle.len()].eq_ignore_ascii_case(needle) {
            out.push_str(with);
            i += needle.len();
        } else {
            // Copy one whole UTF-8 character.
            let ch = s[i..].chars().next().unwrap();
            out.push(ch);
            i += ch.len_utf8();
        }
    }
    out
}

/// Rails `nodes_to_from_clause`: inputs in build order, each joining on the
/// dimension columns it shares with the ones already placed.
fn nodes_to_from_clause<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>) -> Result<Vec<String>> {
    let mut from = Vec::new();
    let mut processed: Vec<NodeId> = Vec::new();
    for input in node.ordered_inputs(cx.graph) {
        if processed.contains(&input) {
            continue;
        }
        let in_node = &cx.graph.nodes[input];
        let alias = node.table_alias(AliasKey::Node(input), in_node.kind.alias_letter());
        let name = in_node.alias(cx.graph);
        if processed.is_empty() {
            from.push(format!("\n\t{name} {alias}"));
        } else {
            let conditions = node_to_join_condition(cx, &processed, input, node);
            if conditions.trim().is_empty() {
                from.push(format!("\n\t{}", cx.graph.dialect.cross_join(&format!("{name} {alias}"))?));
            } else {
                from.push(format!("\n\t{}", to_join(&node.join_type, &name, &alias)?));
                from.push(format!("\n\t\tON {conditions}"));
            }
        }
        processed.push(input);
    }
    Ok(from)
}

fn node_to_join_condition<'a>(cx: &Ctx<'_, 'a>, processed: &[NodeId], input: NodeId, node: &Node<'a>) -> String {
    let in_node = &cx.graph.nodes[input];
    let shared: Vec<String> = processed
        .iter()
        .flat_map(|n| cx.graph.nodes[*n].projections.iter().filter(|p| p.is_dimension()).map(|p| p.proj.column_key()))
        .collect();
    let mut keys: Vec<&Projected<'a>> = Vec::new();
    for p in in_node.projections.iter().filter(|p| p.is_dimension() && shared.contains(&p.proj.column_key())) {
        if !keys.iter().any(|k| k.proj.column_key() == p.proj.column_key()) {
            keys.push(p);
        }
    }
    let conditions: Vec<String> = keys
        .iter()
        .map(|key| {
            let left = node_dimension_column(cx, node, input, key);
            let joinable: Vec<NodeId> = processed
                .iter()
                .copied()
                .filter(|n| cx.graph.nodes[*n].projections.iter().any(|p| p.is_dimension() && p.proj.column_key() == key.proj.column_key()))
                .collect();
            let rs: Vec<String> = joinable.iter().map(|n| node_dimension_column(cx, node, *n, key)).collect();
            let right = if rs.len() == 1 { rs[0].clone() } else { format!("COALESCE({})", rs.join(", ")) };
            format!("{left} = {right}")
        })
        .collect();
    conditions.join("\n\t\tAND ")
}

fn node_dimension_column<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>, input: NodeId, key: &Projected<'a>) -> String {
    let in_node = &cx.graph.nodes[input];
    let column = in_node.projections.iter().find(|p| p.is_dimension() && p.proj.column_key() == key.proj.column_key()).expect("shared key");
    format!("{}.{}", node.table_alias(AliasKey::Node(input), in_node.kind.alias_letter()), column.alias)
}

// ------------------------------------------------------------------ arrays

/// Rails `Sql::Arrays#apply_advanced_array_filter_handling`: a filtered and
/// projected array column either gets a HAVING on the grouped output (list
/// predicates) or is unnested and read through a derived alias.
fn apply_advanced_array_filter_handling<'a>(cx: &Ctx<'_, 'a>, sql: &mut Builder, node: &Node<'a>) -> Result<()> {
    let array_filters: Vec<&Filtered<'a>> = node
        .filters
        .iter()
        .filter(
            |f| matches!(node.field_refs.get(&RefKey::Field(f.field().id)), Some(Reference::Table { expression, .. }) if expression.array),
        )
        .filter(|f| node.projections.iter().any(|p| p.field().map(|x| x.id == f.field().id).unwrap_or(false)))
        .collect();
    for f in array_filters {
        let Some(r) = node.field_refs.get(&RefKey::Field(f.field().id)) else { continue };
        let exp_sql = ref_to_sql_on_table(cx, r, node)?;
        if matches!(f.predicate, Predicate::InList | Predicate::ExcludeList) {
            let cond = if f.predicate == Predicate::InList { "IN" } else { "NOT IN" };
            let value = f.filter_value.clone().unwrap_or_default();
            sql.having(format!("{} {} ({})", lower_sql(cx, &exp_sql, f.field())?, cond, lit(cx, f, &value, false)?));
            if !node.group_by() && sql.groupings.is_empty() {
                for g in group_by_sqls(cx, node)? {
                    sql.group_by(g);
                }
            }
        } else {
            unnest_array(cx, sql, &exp_sql, f, node)?;
        }
    }
    Ok(())
}

/// Rails `unnest_array`: cross join the decomposed array and point the
/// projection, grouping and filter at the decomposed value.
fn unnest_array<'a>(cx: &Ctx<'_, 'a>, sql: &mut Builder, exp_sql: &str, f: &Filtered<'a>, node: &Node<'a>) -> Result<()> {
    let table_alias = node.table_alias(AliasKey::Field(f.field().id), 'D');
    let exp = exp_sql.rsplit('.').next().unwrap_or(exp_sql).to_string();
    let unnest_from = cx.graph.dialect.array_unnest_join(exp_sql, &format!("{table_alias}({exp})"))?;
    sql.from(format!(
        "
	{unnest_from}"
    ));
    let new_sql = format!("{table_alias}.{exp}");
    let projected = node.projections.iter().filter(|p| p.field().map(|x| x.id == f.field().id).unwrap_or(false)).count();
    for _ in 0..projected {
        if let Some(i) = sql.selections.iter().position(|s| s.contains(exp_sql)) {
            sql.selections[i] = sql.selections[i].replace(exp_sql, &new_sql);
        }
        if let Some(i) = sql.groupings.iter().position(|g| g.contains(exp_sql)) {
            sql.groupings[i] = new_sql.clone();
        }
        if let Some(i) = sql.where_clauses.iter().position(|w| w.contains(exp_sql)) {
            sql.where_clauses[i] = sql.where_clauses[i].replace(exp_sql, &new_sql);
        }
    }
    Ok(())
}

// ------------------------------------------------------------------ strategies

/// Rails `Strategy::SegmentOnTable#apply_strategy`.
fn apply_strategy<'a>(cx: &Ctx<'_, 'a>, sql: &mut Builder, parent: &Node<'a>, strategy: &crate::dag::Strategy<'a>) -> Result<()> {
    let child = &cx.graph.nodes[strategy.node];
    match &strategy.kind {
        crate::dag::StrategyKind::SegmentOnTable { table, join_type, map } => {
            let node_alias = parent.table_alias(AliasKey::Node(strategy.node), child.kind.alias_letter());
            let mut conds = Vec::new();
            for (field, pid) in map {
                let expression = resolve_expression(cx, sql, parent, field, Some(table))?;
                let left = aliased_expression_sql(cx, parent, expression);
                let right = child.projected(*pid).map(|p| p.alias.clone()).unwrap_or_default();
                conds.push(format!("{left} = {node_alias}.{right}"));
            }
            let entry = format!(
                "\n\t{} JOIN {} AS {}\n\t\tON {}",
                join_type.to_uppercase(),
                child.alias(cx.graph),
                node_alias,
                conds.join(" AND\n")
            );
            sql.from(entry);
        }
        crate::dag::StrategyKind::SegmentByJoinKeys { limit } => {
            if let Some(n) = limit {
                *child.limit.borrow_mut() = Some(n.clone());
            }
            join_keys_strategy(cx, sql, parent, strategy.node, "INNER JOIN", &[])?;
        }
        crate::dag::StrategyKind::ExcludeByJoinKeys => {
            let node_alias = join_keys_strategy(cx, sql, parent, strategy.node, "LEFT JOIN", &[])?;
            let first_key = child
                .projections
                .iter()
                .find(|p| p.is_dimension() && !p.proj.def.is_expansion())
                .map(|p| p.alias.clone())
                .unwrap_or_default();
            sql.r#where(format!("{node_alias}.{first_key} IS NULL"));
        }
        crate::dag::StrategyKind::ExpandByJoinKeys { sid, left } => {
            let keyword = if *left { "LEFT JOIN" } else { "INNER JOIN" };
            // Pair dedupe: a non-equi predicate against the parent's plain copy.
            let dedupe = cx.q.segment(*sid).map(|s| s.def.pair_dedupe).unwrap_or(zsql_spec::PairDedupe::Neq);
            let mut extra = Vec::new();
            if dedupe != zsql_spec::PairDedupe::None {
                let op = if dedupe == zsql_spec::PairDedupe::Lt { "<" } else { "<>" };
                let node_alias = parent.table_alias(AliasKey::Node(strategy.node), child.kind.alias_letter());
                for proj in child.projections.iter().filter(|p| p.proj.def.is_expansion()) {
                    let Some(field) = proj.field() else { continue };
                    let plain = parent.projections.iter().any(|p| {
                        p.is_dimension()
                            && !p.is_calculation()
                            && !p.proj.is_carried()
                            && p.field().map(|f| f.id == field.id).unwrap_or(false)
                    });
                    if !plain {
                        continue;
                    }
                    extra.push((field, op, node_alias.clone(), proj.alias.clone()));
                }
            }
            join_keys_strategy(cx, sql, parent, strategy.node, keyword, &extra)?;
        }
    }
    Ok(())
}

/// Rails `Strategy::SegmentByJoinKeys#from_entry` and `#on`: join on every
/// projected dimension, plus any extra predicates, returning the alias.
fn join_keys_strategy<'a>(
    cx: &Ctx<'_, 'a>,
    sql: &mut Builder,
    parent: &Node<'a>,
    child_id: NodeId,
    keyword: &str,
    extra: &[(&Field, &str, String, String)],
) -> Result<String> {
    let child = &cx.graph.nodes[child_id];
    let node_alias = parent.table_alias(AliasKey::Node(child_id), child.kind.alias_letter());
    let mut conds = Vec::new();
    for key in child.projections.iter().filter(|p| p.is_dimension() && !p.proj.def.is_expansion()) {
        let field = key.field().ok_or_else(|| PlanError::Resolution("join key without field".into()))?;
        let expression = resolve_expression(cx, sql, parent, field, None)?;
        let left = aliased_expression_sql(cx, parent, expression);
        conds.push(format!("{left} = {node_alias}.{}", key.alias));
    }
    for (field, op, alias, proj_alias) in extra {
        let expression = resolve_expression(cx, sql, parent, field, None)?;
        let left = aliased_expression_sql(cx, parent, expression);
        conds.push(format!("{left} {op} {alias}.{proj_alias}"));
    }
    let entry = format!("\n\t{} {} AS {}\n\t\tON {}", keyword, child.alias(cx.graph), node_alias, conds.join(" AND\n\t\t"));
    sql.from(entry);
    Ok(node_alias)
}

/// Rails `Strategy::Base#resolve_expression`: the field's expression on the
/// parent's side of the join, appending the path's joins when the parent
/// does not walk it yet.
fn resolve_expression<'a>(
    cx: &Ctx<'_, 'a>,
    sql: &mut Builder,
    parent: &Node<'a>,
    field: &Field,
    table: Option<&zsql_semantic::Table>,
) -> Result<&'a Expression> {
    let subtree = parent.subtrees.iter().find(|t| {
        t.paths.iter().any(|p| p.field_id == field.id)
            && table.map(|tb| t.paths.iter().any(|p| p.table_nodes.contains(&tb.id))).unwrap_or(true)
    });
    let path = match subtree {
        None => {
            let base = parent.universe.clone().expect("parent universe");
            let paths = base.paths_for(field.id);
            let path = match table {
                Some(tb) => paths.into_iter().find(|p| p.table_nodes.contains(&tb.id)),
                None => paths.into_iter().next(),
            }
            .ok_or_else(|| {
                PlanError::Resolution(format!(
                    "Could not find table or path for {} with id: {}.",
                    crate::model::field_to_s(field),
                    field.id
                ))
            })?;
            append_path_joins(cx, sql, parent, path);
            path
        }
        Some(t) => {
            let paths = t.paths_for(field.id);
            match table {
                Some(tb) => paths.into_iter().find(|p| p.table_nodes.contains(&tb.id)),
                None => paths.into_iter().next(),
            }
            .ok_or_else(|| PlanError::Resolution(format!("Could not find path for {}", crate::model::field_to_s(field))))?
        }
    };
    cx.universe
        .expression_for(path.target_table_id, field.id)
        .ok_or_else(|| PlanError::Resolution(format!("Could not find expression for {}", crate::model::field_to_s(field))))
}

fn aliased_expression_sql<'a>(cx: &Ctx<'_, 'a>, parent: &Node<'a>, expression: &Expression) -> String {
    expression_sql(cx, parent, expression)
}

/// Rails `Strategy::Base#append_path_joins`: joins along the path, cheapest
/// first, skipping tables the parent already aliased.
fn append_path_joins<'a>(cx: &Ctx<'_, 'a>, sql: &mut Builder, parent: &Node<'a>, path: &zsql_semantic::Path) {
    let mut jds: Vec<&zsql_semantic::JoinDef> = path.joins.iter().filter_map(|j| cx.universe.join(*j)).collect();
    jds.sort_by_key(|j| (j.cost, j.id));
    for jd in jds {
        if parent.table_aliased(&AliasKey::Table(jd.right_id)) {
            continue;
        }
        let left = parent.table_alias(AliasKey::Table(jd.left_id), 'T');
        let right = parent.table_alias(AliasKey::Table(jd.right_id), 'T');
        let cond = replace_ci(&replace_ci(&jd.sql, "left.", &format!("{left}.")), "right.", &format!("{right}."));
        let right_table = cx.universe.table(jd.right_id).expect("join right table");
        sql.from(format!(
            "\n\t{} JOIN {} {}\n\t\tON {}",
            join_type_name(jd.join_type).to_uppercase(),
            right_table.physical_name,
            right,
            cond
        ));
    }
}

// ------------------------------------------------------------------ SELECT

fn projection_to_select_sql<'a>(cx: &Ctx<'_, 'a>, p: &Projected<'a>, node: &Node<'a>) -> Result<String> {
    let mut sql = projection_to_sql(cx, p, node)?;
    if reaggregate_wrap(cx, p, node) {
        sql = format!("max({sql})");
    }
    Ok(format!("{} AS {}", sql, cx.graph.dialect.quote(&p.alias)?))
}

fn projection_to_sql<'a>(cx: &Ctx<'_, 'a>, p: &Projected<'a>, node: &Node<'a>) -> Result<String> {
    let r = node
        .get_ref(p.proj.pid, p.proj.field_id())
        .ok_or_else(|| PlanError::Resolution(format!("No field reference found for projection: {}", p.to_s())))?;
    let base = {
        let _t = crate::profile::span(match r {
            Reference::Table { .. } => "sql.select.table_ref",
            Reference::Node { .. } => "sql.select.node_ref",
            _ => "sql.select.multi_ref",
        });
        match r {
            Reference::Table { .. } => ref_to_sql_on_table(cx, r, node)?,
            Reference::Node { .. } => ref_to_sql_on_node(cx, r, node)?,
            Reference::Multi { .. } => ref_to_sql_on_multi(cx, r, node, true)?,
            Reference::Proj { .. } => return Err(PlanError::Resolution("Unknown reference type".into())),
        }
    };
    let sql = {
        let _t = crate::profile::span("sql.select.decorate");
        decorate(cx, p, node, base)?
    };
    let _t = crate::profile::span("sql.select.mask");
    apply_security_mask(cx, node, p, sql)
}

/// Rails `apply_security_mask`: a triggered projection renders as a CASE
/// on the context dimension.
fn apply_security_mask<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>, p: &Projected<'a>, sql: String) -> Result<String> {
    let Some(field) = p.field() else { return Ok(sql) };
    let Some((_, ctx_dim, allowed, mask_value)) = node.security_masks.iter().find(|(fid, ..)| *fid == field.id) else { return Ok(sql) };
    let d = &cx.graph.dialect;
    let mask_lit = if field.data_type == Some(DataType::String) { d.string_lit(mask_value)? } else { "NULL".to_string() };
    // Denied: every value is the mask.
    let Some(allowed) = allowed else { return Ok(mask_lit) };
    let Some(context_ref) = node.field_refs.get(&RefKey::Field(ctx_dim.id)) else {
        return Ok(d.string_lit(mask_value)?);
    };
    let context_sql = ref_to_sql_any(cx, context_ref, node)?;
    let values: Vec<String> = allowed.iter().map(|v| d.string_lit(&v.to_lowercase())).collect::<std::result::Result<_, _>>()?;
    Ok(format!("CASE WHEN LOWER({}) IN ({}) THEN {} ELSE {} END", context_sql, values.join(", "), sql, mask_lit))
}

/// Rails `reaggregable_measure?`: on a grouping merge a plain measure column,
/// or an engine transform of one, takes an identity `max(...)`.
fn reaggregable_measure(cx: &Ctx, p: &Projected, node: &Node) -> bool {
    if !(node.is_merge() && node.group_by()) || !p.is_measure() {
        return false;
    }
    if p.is_calculation() {
        let Some(source) = p.proj.source() else { return false };
        if !source.is_measure() || source.is_calculation() {
            return false;
        }
        if p.proj.has_aggregate_functions(&cx.graph.dialect.aggregate_functions()) {
            return false;
        }
    }
    true
}

/// The wrap goes outside a plain column, inside a window instead.
fn reaggregate_wrap(cx: &Ctx, p: &Projected, node: &Node) -> bool {
    reaggregable_measure(cx, p, node) && !applied_decorators(cx, p, node).iter().any(|d| d.kind == DecoratorKind::Window)
}

fn applied_decorators<'p>(cx: &Ctx, p: &'p Projected, node: &Node) -> Vec<&'p zsql_spec::Decorator> {
    p.decorators().into_iter().filter(|d| d.inline() && can_apply_decorator(cx, d.kind, node)).collect()
}

fn decorate<'a>(cx: &Ctx<'_, 'a>, p: &Projected<'a>, node: &Node<'a>, sql: String) -> Result<String> {
    let mut decs = applied_decorators(cx, p, node);
    decs.sort_by_key(|d| d.priority());
    let mut res = sql;
    for d in decs {
        if d.kind == DecoratorKind::Window && reaggregable_measure(cx, p, node) {
            res = format!("max({res})");
        }
        res = match d.kind {
            DecoratorKind::Truncate => match d.grain() {
                None | Some("raw") => res,
                Some(g) => cx.graph.dialect.truncate_date(g, &res)?,
            },
            DecoratorKind::Extract => cx.graph.dialect.extract(d.str("extract").unwrap_or(""), &res, d.bool("abbreviate", true))?,
            DecoratorKind::Customize => d.str("custom_sql").unwrap_or("").replace("@expression", &res),
            DecoratorKind::Window => window_sql(cx, d, p, node, &res)?,
            DecoratorKind::Contribute | DecoratorKind::Temporalize => res,
        };
    }
    Ok(res)
}

/// Rails `Windower#decorate`.
fn window_sql<'a>(cx: &Ctx<'_, 'a>, d: &zsql_spec::Decorator, _p: &Projected<'a>, node: &Node<'a>, expr: &str) -> Result<String> {
    let function = d.str("function").unwrap_or("");
    let call = match function {
        "lag" | "lead" => format!("{}({}, {})", function, expr, d.int("offset").unwrap_or(0)),
        "ntile" => format!("{}({}, {})", function, expr, d.int("buckets").unwrap_or(0)),
        "row_number" | "rank" | "dense_rank" | "percent_rank" | "cume_dist" => format!("{function}()"),
        _ => format!("{function}({expr})"),
    };
    let partition_refs = d.strs("partition_refs");
    let auto_partition = partition_refs.iter().any(|r| r == "-1");
    let mut partitions = Vec::new();
    if auto_partition {
        for x in node
            .projections
            .iter()
            .filter(|x| x.proj.def.axis == "row" && x.is_dimension() && !x.field().map(|f| f.is_date()).unwrap_or(false))
        {
            partitions.push(projection_to_sql(cx, x, node)?);
        }
    } else if !partition_refs.is_empty() {
        for x in node.projections.iter().filter(|x| x.proj.def.field_uid.as_ref().map(|u| partition_refs.contains(u)).unwrap_or(false)) {
            partitions.push(projection_to_sql(cx, x, node)?);
        }
    }
    let order_by = d.map("order_by");
    let mut orders = Vec::new();
    if let Some((_, dir)) = order_by.iter().find(|(k, _)| k == "-1") {
        let dates: Vec<&Projected<'a>> = node.projections.iter().filter(|x| x.field().map(|f| f.is_date()).unwrap_or(false)).collect();
        if let Some(dt) = dates.iter().find(|x| x.proj.def.axis == "x").or(dates.first()) {
            let direction = if dir == "desc" { " desc" } else { "" };
            orders.push(format!("{}{}", projection_to_sql(cx, dt, node)?, direction));
        }
    } else if !order_by.is_empty() {
        for x in node.projections.iter() {
            if let Some((_, dir)) = x.proj.def.field_uid.as_ref().and_then(|u| order_by.iter().find(|(k, _)| k == u)) {
                orders.push(format!("{} {}", projection_to_sql(cx, x, node)?, dir));
            }
        }
    }
    let mut win = format!("{call} OVER (");
    if !partitions.is_empty() {
        win.push_str(&format!("PARTITION BY {}", partitions.join(", ")));
    }
    if !orders.is_empty() {
        win.push_str(&format!(" ORDER BY {}", orders.join(", ")));
    }
    if d.str("mode") == Some("moving") {
        win.push_str(&format!(" ROWS BETWEEN {} PRECEDING AND CURRENT ROW", d.int("size").unwrap_or(0)));
    }
    win.push(')');
    Ok(win)
}

/// Truncate and extract are intermediate-pass decorators.
fn can_apply_decorator(cx: &Ctx, kind: DecoratorKind, node: &Node) -> bool {
    let (final_pass, intermediate) = match kind {
        DecoratorKind::Truncate | DecoratorKind::Extract | DecoratorKind::Customize => (false, true),
        DecoratorKind::Window | DecoratorKind::Contribute | DecoratorKind::Temporalize => (true, false),
    };
    let root = cx.graph.root() == node.id;
    let only_strategies = node.inputs.iter().all(|i| node.is_strategy_input(*i));
    (root && final_pass) || (!root && intermediate) || (root && !final_pass && only_strategies)
}

fn group_by_sqls<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for p in node.projections.iter().filter(|p| p.is_dimension()) {
        let sql = projection_to_sql(cx, p, node)?;
        if p.is_calculation() {
            let aggs = cx.graph.dialect.aggregate_functions();
            let refs: Vec<&str> = aggs.iter().map(String::as_str).collect();
            if ungroupable(&sql, &refs) {
                continue;
            }
        }
        out.push(sql);
    }
    Ok(out)
}

// ------------------------------------------------------------------ references

fn expression_sql<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>, exp: &Expression) -> String {
    let _t = crate::profile::span("sql.qualify");
    let alias = node.table_alias(AliasKey::Table(exp.table_id), 'T');
    qualify(exp.sql.as_deref().unwrap_or_default(), &exp.column_refs, &cx.graph.dialect, &alias)
}

fn ref_to_sql_on_table<'a>(cx: &Ctx<'_, 'a>, r: &Reference<'a>, node: &Node<'a>) -> Result<String> {
    match r {
        Reference::Table { expression, .. } => Ok(expression_sql(cx, node, expression)),
        _ => Err(PlanError::Resolution("not a table reference".into())),
    }
}

fn ref_to_sql_on_node<'a>(cx: &Ctx<'_, 'a>, r: &Reference<'a>, node: &Node<'a>) -> Result<String> {
    let Reference::Node { entity, node: src } = r else {
        return Err(PlanError::Resolution("not a node reference".into()));
    };
    let src_node = &cx.graph.nodes[*src];
    let ref_alias = match entity {
        Entity::Proj(p) => src_node.projected(p.pid).map(|x| x.alias.clone()),
        Entity::Field(f) => src_node.projections.iter().find(|x| x.field().map(|y| y.id == f.id).unwrap_or(false)).map(|x| x.alias.clone()),
    }
    .ok_or_else(|| PlanError::Resolution("The entity was not found in the node its linked to.".into()))?;
    Ok(format!("{}.{}", node.table_alias(AliasKey::Node(*src), src_node.kind.alias_letter()), ref_alias))
}

/// Rails `Reference#entity_alias`: the name a formula uses for this source.
fn entity_alias(entity: &Entity) -> String {
    match entity {
        Entity::Proj(p) => {
            if p.alias().starts_with(crate::visitors::alias::SKIP_ALIAS_PREFIX) {
                p.alias()
            } else {
                p.field.map(|f| f.name.clone()).unwrap_or_else(|| p.alias())
            }
        }
        Entity::Field(f) => f.name.clone(),
    }
}

fn ref_to_sql_on_multi<'a>(cx: &Ctx<'_, 'a>, r: &Reference<'a>, node: &Node<'a>, coalesce_dims: bool) -> Result<String> {
    let Reference::Multi { expression, sources, .. } = r else {
        return Err(PlanError::Resolution("not a multi reference".into()));
    };
    if let Some(exp) = expression {
        let mut base_sql = exp.sql();
        for rr in sources {
            let ref_sql = match rr {
                Reference::Table { expression: rexp, .. } => {
                    let s = ref_to_sql_on_table(cx, rr, node)?;
                    if exp.is_semantic(rexp) {
                        base_sql = s.clone();
                    }
                    s
                }
                Reference::Node { .. } => ref_to_sql_on_node(cx, rr, node)?,
                Reference::Proj { proj } => {
                    let p = node.projected(proj.pid).ok_or_else(|| PlanError::Resolution("projection ref missing".into()))?;
                    projection_to_sql(cx, p, node)?
                }
                Reference::Multi { .. } => ref_to_sql_on_multi(cx, rr, node, coalesce_dims)?,
            };
            let name = match rr {
                Reference::Table { entity, .. } | Reference::Node { entity, .. } | Reference::Multi { entity, .. } => entity_alias(entity),
                // A projection reference uses the projection's true alias.
                Reference::Proj { proj } => proj.alias(),
            };
            base_sql = entity_gsub(&base_sql, &name, &ref_sql);
        }
        Ok(base_sql)
    } else {
        let node_refs = r.node_refs();
        if coalesce_dims && node_refs.len() > 1 {
            let parts: Vec<String> = node_refs.iter().map(|nr| ref_to_sql_on_node(cx, nr, node)).collect::<Result<_>>()?;
            Ok(format!("COALESCE({})", parts.join(", ")))
        } else {
            let first = node_refs.first().ok_or_else(|| PlanError::Resolution("multi reference without sources".into()))?;
            ref_to_sql_on_node(cx, first, node)
        }
    }
}

fn ref_to_sql_any<'a>(cx: &Ctx<'_, 'a>, r: &Reference<'a>, node: &Node<'a>) -> Result<String> {
    match r {
        Reference::Table { .. } => ref_to_sql_on_table(cx, r, node),
        Reference::Node { .. } => ref_to_sql_on_node(cx, r, node),
        Reference::Multi { .. } => ref_to_sql_on_multi(cx, r, node, false),
        Reference::Proj { .. } => Err(PlanError::Resolution("unexpected projection reference".into())),
    }
}

// ------------------------------------------------------------------ filters

/// A node's slice of the query's filter tree, with greedy clauses spliced in.
#[derive(Debug, Clone)]
enum PTree {
    Group(String, Vec<PTree>),
    Filter(usize),
    Clause(String),
}

impl PTree {
    fn size(&self) -> usize {
        match self {
            PTree::Group(_, kids) => kids.iter().map(PTree::size).sum(),
            PTree::Filter(_) => 1,
            PTree::Clause(_) => 0,
        }
    }
    fn depth(&self) -> usize {
        match self {
            PTree::Group(_, kids) => 1 + kids.iter().map(PTree::depth).max().unwrap_or(0),
            _ => 0,
        }
    }
}

fn prune(tree: &FilledTree, filters: &[&Filtered]) -> PTree {
    match tree {
        FilledTree::Leaf(fid) => match filters.iter().position(|f| f.fid() == *fid) {
            Some(i) => PTree::Filter(i),
            None => PTree::Clause(String::new()), // dropped below
        },
        FilledTree::Group(op, kids) => {
            let kept: Vec<PTree> = kids
                .iter()
                .filter_map(|k| match k {
                    FilledTree::Group(..) => {
                        let p = prune(k, filters);
                        if let PTree::Group(_, ref inner) = p {
                            if inner.is_empty() {
                                return None;
                            }
                        }
                        Some(p)
                    }
                    FilledTree::Leaf(_) => match prune(k, filters) {
                        PTree::Filter(i) => Some(PTree::Filter(i)),
                        _ => None,
                    },
                })
                .collect();
            PTree::Group(op.clone(), kept)
        }
    }
}

fn insert_after(tree: &mut PTree, target: usize, clause: String) -> bool {
    if let PTree::Group(_, kids) = tree {
        for i in 0..kids.len() {
            match &mut kids[i] {
                PTree::Filter(f) if *f == target => {
                    kids.insert(i + 1, PTree::Clause(clause));
                    return true;
                }
                // Not a match guard: the guard would borrow `kids` a
                // second time while the arm holds it.
                #[allow(clippy::collapsible_match)]
                PTree::Group(..) => {
                    if insert_after(&mut kids[i], target, clause.clone()) {
                        return true;
                    }
                }
                _ => {}
            }
        }
    }
    false
}

fn filter_indices(tree: &PTree, out: &mut Vec<usize>) {
    match tree {
        PTree::Group(_, kids) => kids.iter().for_each(|k| filter_indices(k, out)),
        PTree::Filter(i) => out.push(*i),
        PTree::Clause(_) => {}
    }
}

fn translate_node_filters_to_sql<'a>(cx: &Ctx<'_, 'a>, sql: &mut Builder, node: &Node<'a>) -> Result<()> {
    let where_filters = node.where_filters();
    let having_filters = node.having_filters();
    // An imported segment node prunes against its own tree.
    let tree = node.owner.as_ref().map(|o| &o.tree).unwrap_or(&cx.q.tree);
    let mut subtree = prune(tree, &where_filters);
    let measure_tree = if node.group_by() { Some(prune(tree, &having_filters)) } else { None };

    if cx.graph.dialect.greedy_apply_date_filters() {
        apply_greedy_date_filters(cx, &mut subtree, &where_filters, node)?;
    }

    if subtree.size() > 0 {
        let rendered = render_tree(cx, &subtree, &where_filters, node, 1)?;
        if let Some(r) = rendered {
            sql.r#where(r);
        }
    }
    if let Some(mt) = measure_tree {
        if mt.size() > 0 {
            if let Some(r) = render_tree(cx, &mt, &having_filters, node, 1)? {
                sql.having(r);
            }
        }
    }
    Ok(())
}

/// Swaps leaves to SQL then joins: flat trees on one line per condition,
/// nested ones with parenthesised groups.
fn render_tree<'a>(cx: &Ctx<'_, 'a>, tree: &PTree, filters: &[&Filtered<'a>], node: &Node<'a>, depth: usize) -> Result<Option<String>> {
    let PTree::Group(op, kids) = tree else { return Ok(None) };
    let operator = op.to_uppercase();
    let mut conditions: Vec<String> = Vec::new();
    for k in kids {
        match k {
            PTree::Filter(i) => {
                if let Some(s) = filter_to_sql(cx, node, filters[*i])? {
                    conditions.push(s);
                }
            }
            PTree::Clause(c) => conditions.push(c.clone()),
            PTree::Group(..) => {
                if let Some(inner) = render_tree(cx, k, filters, node, depth + 1)? {
                    let inner_tabs = "\t".repeat(depth + 1);
                    conditions.push(format!("(\n{inner_tabs}{inner})"));
                }
            }
        }
    }
    if conditions.is_empty() {
        return Ok(None);
    }
    let sep = if tree.depth() == 1 && depth == 1 { format!("\n\t{operator} ") } else { format!("\n{}{operator} ", "\t".repeat(depth)) };
    Ok(Some(conditions.join(&sep)))
}

fn apply_greedy_date_filters<'a>(cx: &Ctx<'_, 'a>, subtree: &mut PTree, filters: &[&Filtered<'a>], node: &Node<'a>) -> Result<()> {
    let mut idx = Vec::new();
    filter_indices(subtree, &mut idx);
    for i in idx {
        let f = filters[i];
        if !f.is_date() {
            continue;
        }
        let greedy: Vec<&Expression> = match node.field_refs.get(&RefKey::Field(f.field().id)) {
            Some(Reference::Table { greedy, .. }) => greedy.clone(),
            _ => vec![],
        };
        for exp in greedy {
            if let Ok(clause) = expressed_filter_to_sql(cx, exp, node, f) {
                insert_after(subtree, i, clause);
            }
        }
    }
    Ok(())
}

fn expressed_filter_to_sql<'a>(cx: &Ctx<'_, 'a>, exp: &Expression, node: &Node<'a>, f: &Filtered<'a>) -> Result<String> {
    let sql = expression_sql(cx, node, exp);
    if f.predicate == Predicate::Between {
        Ok(format!(
            "{} BETWEEN {} AND {}",
            sql,
            lit(cx, f, f.filter_value.as_deref().unwrap_or(""), true)?,
            lit(cx, f, f.filter_value_end.as_deref().unwrap_or(""), true)?
        ))
    } else {
        let op = predicate_op(f.predicate)
            .ok_or_else(|| PlanError::Resolution(format!("Unsupported filter predicate: {}", f.predicate.name())))?;
        Ok(format!("{} {} {}", lower_sql(cx, &sql, f.field())?, op, lit(cx, f, f.filter_value.as_deref().unwrap_or(""), false)?))
    }
}

fn predicate_op(p: Predicate) -> Option<&'static str> {
    Some(match p {
        Predicate::Equals => "=",
        Predicate::DoesNotEqual => "!=",
        Predicate::Between => "BETWEEN",
        Predicate::GreaterThan => ">",
        Predicate::GreaterThanOrEqualTo => ">=",
        Predicate::LessThan => "<",
        Predicate::LessThanOrEqualTo => "<=",
        Predicate::InList => "IN",
        Predicate::ExcludeList => "NOT IN",
        Predicate::StartsWith | Predicate::Contains | Predicate::EndsWith | Predicate::Keyword => "LIKE",
        Predicate::IsNull => "IS NULL",
        Predicate::IsNotNull => "IS NOT NULL",
        Predicate::DoesNotStartWith | Predicate::DoesNotContain | Predicate::DoesNotEndWith => "NOT LIKE",
        Predicate::TopN | Predicate::Custom => return None,
    })
}

fn filter_expression_sql<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>, f: &Filtered<'a>) -> Result<String> {
    let r = node
        .field_refs
        .get(&RefKey::Field(f.field().id))
        .ok_or_else(|| PlanError::Resolution(format!("No field reference found for filter on {}", f.field().name)))?;
    let sql = ref_to_sql_any(cx, r, node)?;
    let wrap = node.is_merge() && node.group_by() && f.is_measure();
    Ok(if wrap { format!("max({sql})") } else { sql })
}

fn filter_to_sql<'a>(cx: &Ctx<'_, 'a>, node: &Node<'a>, f: &Filtered<'a>) -> Result<Option<String>> {
    use Predicate::*;
    let value = f.filter_value.clone().unwrap_or_default();
    let out = match f.predicate {
        TopN => return Ok(None),
        Custom => return Err(PlanError::Unimplemented("custom predicate")),
        Equals | DoesNotEqual | GreaterThan | GreaterThanOrEqualTo | LessThan | LessThanOrEqualTo => {
            let sql = filter_expression_sql(cx, node, f)?;
            format!("{} {} {}", lower_sql(cx, &sql, f.field())?, predicate_op(f.predicate).unwrap(), lit(cx, f, &value, false)?)
        }
        Between => {
            let sql = filter_expression_sql(cx, node, f)?;
            // Rails passes the node as is_ending for the start value: truthy.
            format!("{} BETWEEN {} AND {}", sql, lit(cx, f, &value, true)?, lit(cx, f, f.filter_value_end.as_deref().unwrap_or(""), true)?)
        }
        InList | ExcludeList => {
            let sql = filter_expression_sql(cx, node, f)?;
            let is_array =
                matches!(node.field_refs.get(&RefKey::Field(f.field().id)), Some(r) if r.expression().map(|e| e.array).unwrap_or(false));
            let list = lit(cx, f, &value, false)?;
            if is_array {
                if f.predicate == InList {
                    cx.graph.dialect.array_in_list(&sql, &list)?
                } else {
                    cx.graph.dialect.array_exclude_list(&sql, &list)?
                }
            } else {
                let op = if f.predicate == InList { "IN" } else { "NOT IN" };
                format!("{} {} ({})", lower_sql(cx, &sql, f.field())?, op, list)
            }
        }
        IsNull => format!("{} IS NULL", filter_expression_sql(cx, node, f)?),
        IsNotNull => format!("{} IS NOT NULL", filter_expression_sql(cx, node, f)?),
        // A pattern is a string literal like any other value: the
        // dialect quotes it, so a value holding a quote cannot end the
        // literal and continue the WHERE clause.
        Keyword => {
            let sql = lower_sql(cx, &filter_expression_sql(cx, node, f)?, f.field())?;
            let d = &cx.graph.dialect;
            let parts: Vec<String> = value
                .split(',')
                .map(|v| Ok(format!("{sql} LIKE {}", d.string_lit(&format!("%{}%", v.trim().to_lowercase()))?)))
                .collect::<Result<_>>()?;
            format!("({})", parts.join(" OR "))
        }
        StartsWith | Contains | EndsWith | DoesNotStartWith | DoesNotContain | DoesNotEndWith => {
            let sql = lower_sql(cx, &filter_expression_sql(cx, node, f)?, f.field())?;
            let val = value.trim().to_lowercase();
            let pattern = match f.predicate {
                StartsWith | DoesNotStartWith => format!("{val}%"),
                EndsWith | DoesNotEndWith => format!("%{val}"),
                _ => format!("%{val}%"),
            };
            let op = if matches!(f.predicate, StartsWith | Contains | EndsWith) { "LIKE" } else { "NOT LIKE" };
            format!("{sql} {op} {}", cx.graph.dialect.string_lit(&pattern)?)
        }
    };
    Ok(Some(out))
}

/// Rails `Filters#lit`.
fn lit<'a>(cx: &Ctx<'_, 'a>, f: &Filtered<'a>, value: &str, is_ending: bool) -> Result<String> {
    let field = f.field();
    let d = &cx.graph.dialect;
    if field.is_date() {
        return date_lit(d, field, value, is_ending).map_err(PlanError::Resolution);
    }
    let is_string = field.data_type == Some(DataType::String);
    if matches!(f.predicate, Predicate::InList | Predicate::ExcludeList) {
        let items: Vec<String> = split_list(value)
            .into_iter()
            .map(|item| if is_string { d.string_lit(&item.trim().to_lowercase()).map_err(PlanError::from) } else { bare_lit(d, &item) })
            .collect::<Result<_>>()?;
        return Ok(items.join(", "));
    }
    if is_string {
        return Ok(d.string_lit(&value.trim().to_lowercase())?);
    }
    bare_lit(d, value)
}

/// A value for a field that is not a string: a number or a boolean goes
/// into the SQL as it is, anything else goes in quoted, so no text ever
/// reaches the statement bare.
fn bare_lit(d: &zsql_dialect::Dialect, value: &str) -> Result<String> {
    let v = value.trim();
    if v.parse::<f64>().is_ok_and(f64::is_finite) || v.eq_ignore_ascii_case("true") || v.eq_ignore_ascii_case("false") {
        return Ok(v.to_string());
    }
    Ok(d.string_lit(v)?)
}

/// Commas outside quotes split the list; quoted items keep their quotes.
pub fn split_list(value: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut quote: Option<char> = None;
    for c in value.chars() {
        match quote {
            Some(q) if c == q => {
                quote = None;
                cur.push(c);
            }
            Some(_) => cur.push(c),
            None if c == '"' || c == '\'' => {
                quote = Some(c);
                cur.push(c);
            }
            None if c == ',' => {
                out.push(std::mem::take(&mut cur));
            }
            None => cur.push(c),
        }
    }
    out.push(cur);
    out
}

fn lower_sql(cx: &Ctx, sql: &str, field: &Field) -> Result<String> {
    // Already lower-cased by its own expression: `lower(...)` at the
    // front, not a column that happens to be called `follower`.
    if sql.trim_start().get(..6).is_some_and(|head| head.eq_ignore_ascii_case("lower(")) {
        return Ok(sql.to_string());
    }
    if field.data_type == Some(DataType::String) {
        return Ok(cx.graph.dialect.lower_case(sql)?);
    }
    Ok(sql.to_string())
}
