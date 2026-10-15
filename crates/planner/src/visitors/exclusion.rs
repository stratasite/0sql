//! Rails `ExclusionVisitor`: a measure with exclusion rules aggregates at a
//! grain without the excluded dimensions, in its own node.

use crate::dag::{Filtered, Graph, NodeId, NodeKind, Projected};
use crate::model::RQuery;
use crate::visitors::grain::*;
use crate::{PlanError, Result};
use zsql_semantic::{ExclusionType, Field, FilterMode, Universe};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    for id in graph.post_order() {
        if graph.nodes[id].removed || graph.nodes[id].kind != NodeKind::Aggregation {
            continue;
        }
        let mut measures: Vec<&'a Field> = Vec::new();
        for f in graph.nodes[id].measures() {
            if f.exclusion_type.is_some() && !measures.iter().any(|m| m.id == f.id) {
                measures.push(f);
            }
        }
        for m in measures {
            process(graph, id, universe, q, m)?;
        }
    }
    Ok(())
}

fn process<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, q: &RQuery<'a>, measure: &'a Field) -> Result<()> {
    let (dims, filters) = allowed_components(graph, id, universe, q, measure)?;
    if dimensionally_equivalent(&graph.nodes[id], &dims, &filters) {
        return Ok(());
    }
    let signature = measure_signature(&graph.nodes[id], measure);
    if let Some(target) = find_equivalent_agg(graph, &dims, &filters, measure, &signature) {
        move_measure_components(graph, id, target, measure);
        return Ok(());
    }

    let measure_projections: Vec<_> = graph.nodes[id]
        .projections
        .iter()
        .filter(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false))
        .map(|p| p.proj.clone())
        .collect();
    let mut merge = graph.root();
    let base = graph.nodes[id].universe.clone().expect("agg universe");
    let mut projections = measure_projections.clone();
    projections.extend(dims.iter().map(|d| d.proj.clone()));
    let target = create_agg_node(graph, universe, q, base, projections, filters.iter().map(|f| f.filter.clone()).collect())?;

    if graph.nodes[id].is_root() {
        merge = create_merge_node(graph, q);
        graph.add_edge(id, merge);
    }
    graph.add_edge(target, merge);

    for p in &measure_projections {
        graph.nodes[id].delete_projection(p.pid);
    }
    let fids: Vec<usize> = graph.nodes[id].filters.iter().filter(|f| f.field().id == measure.id).map(|f| f.fid()).collect();
    for fid in fids {
        graph.nodes[id].delete_filter(fid);
    }

    let has_measure = graph.nodes[id].projections.iter().any(|p| p.is_measure()) || graph.nodes[id].filters.iter().any(|f| f.is_measure());
    if !has_measure && covered_elsewhere(graph, id) {
        graph.destroy_node(id);
    }
    Ok(())
}

/// Rails `get_allowed_components`: the dimensions and filters the measure
/// may aggregate with, per its rules. Sets keep first-insertion order.
fn allowed_components<'a>(
    graph: &Graph<'a>,
    id: NodeId,
    universe: &'a Universe,
    q: &RQuery<'a>,
    measure: &'a Field,
) -> Result<(Vec<Projected<'a>>, Vec<Filtered<'a>>)> {
    let node = &graph.nodes[id];
    let mut projections: Vec<Projected<'a>> = Vec::new();
    let mut filters: Vec<Filtered<'a>> = node.filters.iter().filter(|f| f.is_measure()).cloned().collect();
    let add_p = |v: Vec<&Projected<'a>>, projections: &mut Vec<Projected<'a>>| {
        for p in v {
            if !projections.iter().any(|x| x.proj.pid == p.proj.pid) {
                projections.push(p.clone());
            }
        }
    };
    let add_f = |v: Vec<&Filtered<'a>>, filters: &mut Vec<Filtered<'a>>| {
        for f in v {
            if !filters.iter().any(|x| x.fid() == f.fid()) {
                filters.push(f.clone());
            }
        }
    };
    let dim_projs: Vec<&Projected<'a>> = node.projections.iter().filter(|p| p.is_dimension()).collect();
    let dim_filters: Vec<&Filtered<'a>> = node.filters.iter().filter(|f| f.is_dimension()).collect();

    let rule_ids: Vec<i64> = measure.exclusions.iter().flat_map(|r| exclusion_dimension_ids(universe, r)).collect();
    let excluded = |p: &Projected<'a>| -> bool {
        if p.is_calculation() {
            calc_dimension_ids(&p.proj, q).iter().any(|i| rule_ids.contains(i))
        } else {
            p.field().map(|f| rule_ids.contains(&f.id)).unwrap_or(false)
        }
    };
    let allowed = |p: &Projected<'a>| -> bool {
        if p.is_calculation() {
            let ids = calc_dimension_ids(&p.proj, q);
            !ids.is_empty() && ids.iter().all(|i| rule_ids.contains(i))
        } else {
            p.field().map(|f| rule_ids.contains(&f.id)).unwrap_or(false)
        }
    };

    match measure.exclusion_type {
        Some(ExclusionType::ExcludeAll) => {}
        Some(ExclusionType::Exclude) => {
            for rule in &measure.exclusions {
                match rule.filter {
                    FilterMode::Ignore => {
                        add_p(dim_projs.iter().copied().filter(|p| !excluded(p)).collect(), &mut projections);
                        add_f(dim_filters.iter().copied().filter(|f| !rule_ids.contains(&f.field().id)).collect(), &mut filters);
                    }
                    FilterMode::Apply => {
                        add_p(dim_projs.iter().copied().filter(|p| !excluded(p)).collect(), &mut projections);
                        add_f(dim_filters.clone(), &mut filters);
                    }
                    FilterMode::Only => {
                        add_p(dim_projs.clone(), &mut projections);
                        add_f(dim_filters.iter().copied().filter(|f| !rule_ids.contains(&f.field().id)).collect(), &mut filters);
                    }
                }
            }
        }
        Some(ExclusionType::ExcludeAllExcept) => {
            for rule in &measure.exclusions {
                match rule.filter {
                    FilterMode::Ignore => {
                        add_p(dim_projs.iter().copied().filter(|p| allowed(p)).collect(), &mut projections);
                        add_f(dim_filters.iter().copied().filter(|f| rule_ids.contains(&f.field().id)).collect(), &mut filters);
                    }
                    FilterMode::Apply => {
                        add_p(dim_projs.iter().copied().filter(|p| allowed(p)).collect(), &mut projections);
                        add_f(dim_filters.clone(), &mut filters);
                    }
                    FilterMode::Only => {
                        add_p(dim_projs.clone(), &mut projections);
                        add_f(dim_filters.iter().copied().filter(|f| rule_ids.contains(&f.field().id)).collect(), &mut filters);
                    }
                }
            }
        }
        None => return Err(PlanError::Resolution("Unhandled measure exclusion type.".into())),
    }
    Ok((projections, filters))
}
