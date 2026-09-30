//! Rails `InclusionVisitor`: a measure with an inclusion rule aggregates at
//! a lower grain first, then a merge rolls it up with the rule's aggregation.

use crate::dag::{Entity, Filtered, Graph, NodeId, NodeKind, Projected, RefKey, Reference};
use crate::model::{RProjection, RQuery};
use crate::visitors::grain::*;
use crate::{PlanError, Result};
use std::rc::Rc;
use zsql_semantic::{Field, FilterMode, Universe};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    let mut visited: Vec<NodeId> = Vec::new();
    for id in graph.post_order() {
        if visited.contains(&id) || graph.nodes[id].removed || graph.nodes[id].kind != NodeKind::Aggregation {
            continue;
        }
        visited.push(id);
        let mut measures: Vec<&'a Field> = Vec::new();
        for f in graph.nodes[id].measures() {
            if f.inclusions.is_some() && !measures.iter().any(|m| m.id == f.id) {
                measures.push(f);
            }
        }
        for m in measures {
            process(graph, id, universe, q, m, &mut visited)?;
        }
    }
    Ok(())
}

fn process<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, q: &RQuery<'a>, measure: &'a Field, visited: &mut Vec<NodeId>) -> Result<()> {
    let (dims, filters) = allowed_components(graph, id, universe, q, measure);
    if dimensionally_equivalent(&graph.nodes[id], &dims, &filters) {
        return Ok(());
    }
    let signature = measure_signature(&graph.nodes[id], measure);
    if let Some(target) = find_equivalent_agg(graph, &dims, &filters, measure, &signature) {
        let root = graph.root();
        let (projs, _) = move_measure_components(graph, id, target, measure);
        let out = graph.nodes[target].outputs.first().copied().ok_or_else(|| PlanError::Resolution("inclusion target has no output".into()))?;
        for p in projs {
            create_calculation(graph, &p, target, out, root, universe);
        }
        if !graph.nodes[id].projections.iter().any(|p| p.is_measure()) {
            graph.destroy_node(id);
        }
        return Ok(());
    }

    let mut root = graph.root();
    let measure_projections: Vec<Projected<'a>> = graph.nodes[id].projections.iter().filter(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false)).cloned().collect();
    let mut projections: Vec<Rc<RProjection<'a>>> = dims.iter().map(|d| d.proj.clone()).collect();
    projections.extend(measure_projections.iter().map(|p| p.proj.clone()));

    // The node at the extended dimensionality.
    let base = graph.nodes[id].universe.clone().expect("agg universe");
    let all_local = projections.iter().all(|p| p.field.map(|f| base.has_field(f.id)).unwrap_or(true)) && filters.iter().all(|f| base.has_field(f.field().id));
    if !all_local {
        return Err(PlanError::Unimplemented("inclusion sub-plan outside the node's universe"));
    }
    let extended = create_agg_node(graph, universe, q, base, projections, filters.iter().map(|f| f.filter.clone()).collect())?;
    visited.push(extended);

    // The merge at the query dimensionality.
    let re_agg = create_merge_node(graph, q);
    let extended_pids: Vec<usize> = graph.nodes[extended].projections.iter().map(|p| p.proj.pid).collect();
    let measure_pids: Vec<usize> = measure_projections.iter().map(|p| p.proj.pid).collect();
    graph.nodes[re_agg].projections.retain(|p| {
        if p.is_dimension() {
            extended_pids.contains(&p.proj.pid)
        } else if p.is_measure() {
            measure_pids.contains(&p.proj.pid)
        } else {
            true
        }
    });
    graph.nodes[re_agg].should_group_by = Some(true);
    graph.nodes[re_agg].projections.retain(|p| !(p.field().map(|f| f.id == measure.id).unwrap_or(false) || p.is_calculation()));
    // The merge is built with the query's final-pass measure filters; only
    // one on this measure can be read from the roll-up's single input. A
    // sibling measure's filter belongs to the node that still computes it.
    graph.nodes[re_agg].filters.retain(|f| f.field().id == measure.id);
    graph.add_edge(extended, re_agg);

    for p in &measure_projections {
        graph.nodes[id].delete_projection(p.proj.pid);
    }
    let fids: Vec<usize> = graph.nodes[id].filters.iter().filter(|f| f.field().id == measure.id).map(|f| f.fid()).collect();
    for fid in fids {
        graph.nodes[id].delete_filter(fid);
    }

    let node_has_measures = graph.nodes[id].projections.iter().any(|p| p.is_measure());
    if graph.nodes[id].is_root() && node_has_measures {
        root = create_merge_node(graph, q);
        graph.add_edge(id, root);
    }
    if !node_has_measures {
        graph.destroy_node(id);
    }
    if root != id {
        graph.add_edge(re_agg, root);
    }
    for p in &measure_projections {
        create_calculation(graph, p, re_agg, re_agg, root, universe);
    }
    Ok(())
}

/// Rails `create_calculation`: the roll-up formula on the merge, and the
/// root reads the measure from that column.
fn create_calculation<'a>(graph: &mut Graph<'a>, proj: &Projected<'a>, _target: NodeId, calc_node: NodeId, root: NodeId, _universe: &'a Universe) {
    let field = proj.field().expect("measure projection");
    let rule = field.inclusions.as_ref().expect("inclusion rule");
    let name = format!("[{}]@m", field.name);
    let sql = if rule.aggregation.contains("@exp") { rule.aggregation.replace("@exp", &name) } else { format!("{}({})", rule.aggregation, name) };
    let calc = Rc::new(RProjection::for_calc(sql, proj.proj.alias(), proj.proj.def.ordinal, vec![field], vec![]));
    graph.nodes[calc_node].add_projection(calc.clone());
    if root != calc_node && !graph.nodes[root].removed {
        graph.nodes[root].field_refs.insert(RefKey::Proj(proj.proj.pid), Reference::Node { entity: Entity::Proj(calc), node: calc_node });
    }
}

/// Rails `get_allowed_components`: the node's dimensions plus the rule's,
/// with dimension filters per the rule's mode.
fn allowed_components<'a>(graph: &Graph<'a>, id: NodeId, universe: &'a Universe, q: &RQuery<'a>, measure: &'a Field) -> (Vec<Projected<'a>>, Vec<Filtered<'a>>) {
    let node = &graph.nodes[id];
    let rule = measure.inclusions.as_ref().expect("inclusion rule");
    let rule_ids = inclusion_dimension_ids(universe, rule);
    let mut projections: Vec<Projected<'a>> = node.projections.iter().filter(|p| p.is_dimension()).cloned().collect();
    // Only a filter on the inclusion measure itself travels to the
    // inclusion level, where its expression is what gets aggregated. Rails
    // carries every measure filter along, and a filter on a sibling
    // measure then reaches the roll-up with no column to read: "The filter
    // on X cannot be applied at the final pass". Such a filter stays where
    // it was, on the node that still computes that measure.
    let mut filters: Vec<Filtered<'a>> = node.filters.iter().filter(|f| f.is_measure() && f.field().id == measure.id).cloned().collect();
    let dim_filters = node.filters.iter().filter(|f| f.is_dimension());
    match rule.filter {
        FilterMode::Ignore => filters.extend(dim_filters.filter(|f| !rule_ids.contains(&f.field().id)).cloned()),
        FilterMode::Apply => filters.extend(dim_filters.cloned()),
        FilterMode::Only => filters.extend(dim_filters.filter(|f| rule_ids.contains(&f.field().id)).cloned()),
    }
    for did in rule_ids {
        if let Some(d) = universe.field(did) {
            // Built against the query, so it seats after the query's projections.
            let mut rp = RProjection::for_field(d);
            rp.def.ordinal = Some(q.next_ordinal());
            projections.push(Projected::new(Rc::new(rp)));
        }
    }
    (projections, filters)
}
