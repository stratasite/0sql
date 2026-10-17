//! Rails `ContributionVisitor`: a measure decorated as a share of total
//! divides itself by the same measure aggregated over the partition the
//! decorator names, computed in a sibling node.

use crate::dag::{Filtered, Graph, MultiExpr, NodeId, NodeKind, Projected, Reference};
use crate::model::{RFilter, RProjection, RQuery};
use crate::sql::expr::entity_gsub;
use crate::visitors::alias::SKIP_ALIAS_PREFIX;
use crate::visitors::grain::{create_agg_node, create_merge_node, dimensionally_equivalent};
use crate::{PlanError, Result};
use std::rc::Rc;
use zsql_semantic::{Field, Universe};
use zsql_spec::{Decorator, DecoratorKind};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    let node = graph.root();
    let measures: Vec<Projected<'a>> = graph.nodes[node]
        .projections
        .iter()
        .filter(|p| p.decorators().iter().any(|d| d.kind == DecoratorKind::Contribute))
        .cloned()
        .collect();
    if measures.is_empty() {
        return Ok(());
    }
    if !graph.nodes[node].is_merge() {
        let merge = create_merge_node(graph, q);
        graph.add_edge(node, merge);
    }
    for mp in measures {
        let r = graph.nodes[node].get_ref(mp.proj.pid, mp.proj.field_id()).cloned();
        if mp.is_calculation() {
            process_calculation(graph, universe, q, &mp)?;
        } else if let Some(Reference::Multi { expression: Some(MultiExpr::Semantic(exp)), .. }) = r {
            process_compound(graph, universe, q, &mp, node, exp)?;
        } else {
            let agg =
                graph.live().find(|n| n.kind == NodeKind::Aggregation && n.has_proj(mp.proj.pid) && n.purpose.is_none()).map(|n| n.id);
            let agg = agg.ok_or_else(|| PlanError::Resolution(format!("No aggregation node holds {}", mp.to_s())))?;
            process_table(graph, universe, q, &mp, agg)?;
        }
    }
    Ok(())
}

fn contributor(mp: &Projected) -> Decorator {
    mp.decorators().into_iter().find(|d| d.kind == DecoratorKind::Contribute).cloned().expect("contributor")
}

fn other_decorators(mp: &Projected) -> Vec<Decorator> {
    mp.decorators().into_iter().filter(|d| d.kind != DecoratorKind::Contribute).cloned().collect()
}

fn process_table<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, mp: &Projected<'a>, agg: NodeId) -> Result<()> {
    let c = contributor(mp);
    if graph.root() == agg {
        let merge = create_merge_node(graph, q);
        graph.add_edge(agg, merge);
    }
    let field = mp.field().expect("measure field");
    let total = total_calculation(graph, universe, q, field, &c)?;
    let sql = format!("[{}]@m / ([{}]*1.00)", field.name, total.alias());
    let mut final_calc = (*q.static_calc(universe, &mp.alias, &sql)).clone_shallow();
    final_calc.set_source(Some(mp.proj.clone()));
    final_calc.def.decorators = other_decorators(mp);
    let root = graph.root();
    graph.nodes[root].replace_projection(mp.proj.pid, Rc::new(final_calc));
    Ok(())
}

fn process_compound<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    mp: &Projected<'a>,
    merge: NodeId,
    exp: &'a zsql_semantic::Expression,
) -> Result<()> {
    let c = contributor(mp);
    let mut numerator = exp.sql.clone().unwrap_or_default();
    let refs: Vec<&Field> = exp.dimension_ref_ids.iter().chain(exp.measure_ref_ids.iter()).filter_map(|i| universe.field(*i)).collect();
    for f in &refs {
        let t = if f.is_dimension() { "@D" } else { "@M" };
        numerator = entity_gsub(&numerator, &f.name, &format!("[{}]{}", f.name, t));
    }
    let mut denominator = numerator.clone();
    for m in refs.iter().filter(|f| f.is_measure()) {
        let total = total_calculation(graph, universe, q, m, &c)?;
        denominator = entity_gsub(&denominator, &m.name, &format!("[{}]", total.alias()));
    }
    let sql = format!("({}) / \n\t\t({})*1.00", numerator, denominator);
    let calc = q.static_calc(universe, &mp.alias, &sql);
    let key = mp.proj.field_id().map(crate::dag::RefKey::Field).unwrap_or(crate::dag::RefKey::Proj(mp.proj.pid));
    if let Some(Reference::Multi { expression, .. }) = graph.nodes[merge].field_refs.get_mut(&key) {
        *expression = Some(MultiExpr::Calc(calc));
    }
    Ok(())
}

fn process_calculation<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, mp: &Projected<'a>) -> Result<()> {
    let c = contributor(mp);
    let root = graph.root();
    let numerator = mp.proj.def.sql.clone().unwrap_or_default();
    let mut denominator = update_denominator_fields(graph, universe, q, &mp.proj, &numerator, &c)?;
    for r in mp.proj.def.projection_refs.clone() {
        denominator = update_denominator_projection_refs(graph, universe, q, &denominator, &r, root, &c, &mp.proj)?;
    }
    let sql = format!("({}) / \n\t\t({})", numerator, denominator);
    let mut final_calc = (*q.static_calc(universe, &mp.alias, &sql)).clone_shallow();
    final_calc.set_source(Some(mp.proj.clone()));
    final_calc.def.decorators = other_decorators(mp);
    graph.nodes[root].replace_projection(mp.proj.pid, Rc::new(final_calc));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
fn update_denominator_projection_refs<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    sql: &str,
    alias: &str,
    merge: NodeId,
    c: &Decorator,
    parent: &Rc<RProjection<'a>>,
) -> Result<String> {
    let Some(pj) = graph.nodes[merge].projections.iter().find(|p| p.proj.match_alias(alias)).cloned() else { return Ok(sql.to_string()) };
    let mut out = sql.to_string();
    if !pj.is_calculation() && pj.is_measure() && pj.proj.pid != parent.pid {
        let total = total_calculation(graph, universe, q, pj.field().expect("field"), c)?;
        out = entity_gsub(&out, &pj.proj.alias(), &format!("[{}]", total.alias()));
    } else if pj.is_calculation() {
        out = update_denominator_fields(graph, universe, q, &pj.proj, &out, c)?;
        for sub in pj.proj.def.projection_refs.clone() {
            out = update_denominator_projection_refs(graph, universe, q, &out, &sub, merge, c, &pj.proj)?;
        }
    }
    Ok(out)
}

fn update_denominator_fields<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    calc: &Rc<RProjection<'a>>,
    sql: &str,
    c: &Decorator,
) -> Result<String> {
    let mut out = sql.to_string();
    for m in calc.field_refs.iter().filter(|f| f.is_measure()) {
        let total = total_calculation(graph, universe, q, m, c)?;
        out = entity_gsub(&out, &m.name, &format!("[{}]", total.alias()));
    }
    Ok(out)
}

fn digest(name: &str, prefix: &str) -> String {
    format!("{}{}{}", SKIP_ALIAS_PREFIX, prefix, &format!("{:x}", md5::compute(name.as_bytes()))[..9])
}

/// The `__msrtotal_…` static on the partition-total node for the measure.
fn total_calculation<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    measure: &'a Field,
    c: &Decorator,
) -> Result<Rc<RProjection<'a>>> {
    let agg = graph.live().find(|n| n.kind == NodeKind::Aggregation && n.has_field(measure.id)).map(|n| n.id);
    let agg = agg.ok_or_else(|| PlanError::Resolution(format!("No aggregation node holds {}", measure.name)))?;
    let total_node = find_or_create_total_node(graph, universe, q, measure, c, agg)?;
    let alias = digest(&measure.name, "total_");
    if let Some(existing) = graph.nodes[total_node].projections.iter().find(|p| p.is_calculation() && p.proj.match_alias(&alias)) {
        return Ok(existing.proj.clone());
    }
    let calc = (*q.static_calc(universe, &alias, &format!("[{}]@m", measure.name))).clone_shallow();
    calc.set_source(
        graph.nodes[agg].projections.iter().find(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false)).map(|p| p.proj.clone()),
    );
    let calc = Rc::new(calc);
    graph.nodes[total_node].add_projection(calc.clone());
    Ok(calc)
}

fn find_or_create_total_node<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    measure: &'a Field,
    c: &Decorator,
    agg: NodeId,
) -> Result<NodeId> {
    let dims: Vec<Rc<RProjection<'a>>> = denominator_dimensions(c, q);
    let filters: Vec<Rc<RFilter<'a>>> = denominator_filters(c, q, measure);
    let dims_w: Vec<Projected<'a>> = dims.iter().cloned().map(Projected::new).collect();
    let filters_w: Vec<Filtered<'a>> = filters.iter().cloned().map(Filtered::new).collect();
    let signature = crate::visitors::segmentation::node_signature(&graph.nodes[agg]);
    let found = graph
        .live()
        .find(|n| {
            n.id != agg
                && n.kind == NodeKind::Aggregation
                && n.universe.as_ref().map(|u| u.has_measure(measure.id)).unwrap_or(false)
                && dimensionally_equivalent(n, &dims_w, &filters_w)
                && crate::visitors::segmentation::node_signature(n) == signature
                && n.purpose.is_none()
        })
        .map(|n| n.id);
    if let Some(t) = found {
        return Ok(t);
    }
    let base = graph.nodes[agg].universe.clone().expect("agg universe");
    let root = graph.root();
    let total = create_agg_node(graph, universe, q, base, dims, filters)?;
    graph.add_edge(total, root);
    for input in graph.nodes[agg].inputs.clone() {
        match graph.nodes[agg].strategies.iter().find(|s| s.node == input).cloned() {
            Some(s) => graph.add_strategy_edge(s, total),
            None => graph.add_edge(input, total),
        }
    }
    Ok(total)
}

/// Rails `Contributor#denominator_dimensions`.
fn denominator_dimensions<'a>(c: &Decorator, q: &RQuery<'a>) -> Vec<Rc<RProjection<'a>>> {
    let dims: Vec<Rc<RProjection<'a>>> = q.projections.iter().filter(|p| p.is_dimension()).cloned().collect();
    let refs = c.strs("partition_refs");
    let auto = c.is_null("partition_refs") || refs.iter().any(|r| r == "-1");
    if !auto {
        return dims.into_iter().filter(|p| p.def.field_uid.as_ref().map(|u| refs.contains(u)).unwrap_or(false)).collect();
    }
    let date = dims.iter().find(|p| p.field.map(|f| f.is_date()).unwrap_or(false)).cloned();
    let partitioning: Vec<Rc<RProjection<'a>>> =
        dims.iter().filter(|p| date.as_ref().map(|d| d.pid != p.pid).unwrap_or(true)).cloned().collect();
    match (&date, partitioning.len()) {
        (Some(d), n) if n > 0 => vec![d.clone()],
        (Some(_), 0) => vec![],
        (None, 1) => vec![],
        (None, n) if n > 1 => vec![partitioning[0].clone()],
        _ => vec![],
    }
}

/// Rails `Contributor#denominator_filters`.
fn denominator_filters<'a>(c: &Decorator, q: &RQuery<'a>, measure: &Field) -> Vec<Rc<RFilter<'a>>> {
    let filters: Vec<Rc<RFilter<'a>>> = q.filters.iter().filter(|f| f.field.id != measure.id).cloned().collect();
    if !c.bool("ignore_partition_filters", true) {
        return filters;
    }
    let dim_uids: Vec<String> = q.projections.iter().filter(|p| p.is_dimension()).filter_map(|p| p.def.field_uid.clone()).collect();
    let denom_uids: Vec<String> = denominator_dimensions(c, q).iter().filter_map(|p| p.def.field_uid.clone()).collect();
    let ignorable: Vec<&String> = dim_uids.iter().filter(|u| !denom_uids.contains(u)).collect();
    filters.into_iter().filter(|f| f.is_date() || !ignorable.contains(&&f.def.field_uid)).collect()
}
