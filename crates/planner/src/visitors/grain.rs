//! Shared machinery of the grain-shaping visitors: node grain equality,
//! moving a measure's components between nodes, and resolving the entity
//! names a measure rule lists into dimension ids.

use crate::dag::{Filtered, Graph, Node, NodeId, NodeKind, Projected};
use crate::model::{RFilter, RProjection, RQuery};
use crate::resolver::set_subtrees;
use crate::universe::Base;
use crate::Result;
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use zsql_semantic::{Exclusion, ExclusionScope, Field, FieldKind, Id, Inclusion, Universe};

/// Rails `Aggregation#grain_component`: a field, or a calculation's formula.
pub fn grain_key(p: &Projected) -> String {
    if p.is_calculation() {
        format!("calc:{}", p.proj.def.sql.clone().unwrap_or_default())
    } else {
        format!("field:{}", p.proj.field_id().unwrap_or(0))
    }
}

/// Rails `Aggregation#dimensionally_equivalent?` with the filters given.
pub fn dimensionally_equivalent(node: &Node, dims: &[Projected], filters: &[Filtered]) -> bool {
    let local: HashSet<String> = node.projections.iter().filter(|p| p.is_dimension()).map(grain_key).collect();
    let target: HashSet<String> = dims.iter().map(grain_key).collect();
    let local_f: HashSet<usize> = node.filters.iter().map(|f| f.fid()).collect();
    let target_f: HashSet<usize> = filters.iter().map(|f| f.fid()).collect();
    local == target && local_f == target_f
}

/// The first aggregation node at this grain holding the measure, under the same segment signature.
pub fn find_equivalent_agg(graph: &Graph, dims: &[Projected], filters: &[Filtered], measure: &Field, signature: &crate::visitors::segmentation::Signature) -> Option<NodeId> {
    graph
        .live()
        .find(|n| {
            n.kind == NodeKind::Aggregation
                && dimensionally_equivalent(n, dims, filters)
                && crate::visitors::segmentation::node_signature(n) == *signature
                && n.universe.as_ref().map(|u| u.has_measure(measure.id)).unwrap_or(false)
        })
        .map(|n| n.id)
}

/// The signature of the node's projection of the measure.
pub fn measure_signature(node: &Node, measure: &Field) -> crate::visitors::segmentation::Signature {
    node.projections
        .iter()
        .find(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false))
        .map(|p| crate::visitors::segmentation::signature(&p.proj))
        .unwrap_or_default()
}

/// Rails `Dag.create_agg_node`: wrappers are re-made around the shared records.
pub fn create_agg_node<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    base: Arc<Base<'a>>,
    projections: Vec<Rc<RProjection<'a>>>,
    filters: Vec<Rc<RFilter<'a>>>,
) -> Result<NodeId> {
    let node = Node::with_universe(NodeKind::Aggregation, base, projections, filters);
    let id = graph.add_node(node);
    set_subtrees(graph, id, universe, q)?;
    Ok(id)
}

/// Rails `Dag.create_merge_node`.
pub fn create_merge_node<'a>(graph: &mut Graph<'a>, q: &RQuery<'a>) -> NodeId {
    let filters = q.filters.iter().filter(|f| f.is_measure() && graph.dialect.final_measure_filter()).cloned().collect();
    let merge = Node::merge(q.all_projections(), filters, graph.dialect.final_pass_measure_join_type().to_string());
    graph.add_node(merge)
}

/// Moves the measure's projections and filters from one node to another,
/// keeping the same wrappers as Rails' `Node#add` does.
pub fn move_measure_components<'a>(graph: &mut Graph<'a>, from: NodeId, to: NodeId, measure: &Field) -> (Vec<Projected<'a>>, Vec<Filtered<'a>>) {
    let projs: Vec<Projected<'a>> = graph.nodes[from].projections.iter().filter(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false)).cloned().collect();
    let filters: Vec<Filtered<'a>> = graph.nodes[from].filters.iter().filter(|f| f.field().id == measure.id).cloned().collect();
    for p in &projs {
        if !graph.nodes[to].has_proj(p.proj.pid) {
            graph.nodes[to].projections.push(p.clone());
        }
        graph.nodes[from].delete_projection(p.proj.pid);
    }
    for f in &filters {
        if !graph.nodes[to].filters.iter().any(|x| x.fid() == f.fid()) {
            graph.nodes[to].filters.push(f.clone());
        }
        graph.nodes[from].delete_filter(f.fid());
    }
    (projs, filters)
}

/// Rails `Rules::Exclusion#dimension_ids`, resolved from entity names when
/// the rule carries no cached ids: named dimensions, the dimensions mapped
/// on named tables, or every dimension reachable from named root tables.
pub fn exclusion_dimension_ids(universe: &Universe, rule: &Exclusion) -> Vec<Id> {
    if let Some(ids) = &rule.dimension_ids {
        return ids.clone();
    }
    let names: Vec<String> = rule.entities.iter().map(|e| e.trim().to_lowercase()).collect();
    let snap = universe.snapshot();
    match rule.scope {
        ExclusionScope::Dimension => snap
            .fields
            .iter()
            .filter(|f| f.kind == FieldKind::Dimension && names.contains(&f.name.to_lowercase()))
            .map(|f| f.id)
            .collect(),
        ExclusionScope::Table => {
            let tables: Vec<Id> = snap.tables.iter().filter(|t| names.contains(&t.name.to_lowercase())).map(|t| t.id).collect();
            let mut out = Vec::new();
            for e in &snap.expressions {
                if tables.contains(&e.table_id) {
                    if let Some(f) = universe.field(e.field_id) {
                        if f.kind == FieldKind::Dimension && !out.contains(&f.id) {
                            out.push(f.id);
                        }
                    }
                }
            }
            out
        }
        ExclusionScope::Universe => {
            let tables: Vec<Id> = snap.tables.iter().filter(|t| names.contains(&t.name.to_lowercase())).map(|t| t.id).collect();
            let mut out = Vec::new();
            for p in &snap.paths {
                if tables.contains(&p.root_table_id) && p.field_type == FieldKind::Dimension && !out.contains(&p.field_id) {
                    out.push(p.field_id);
                }
            }
            out
        }
    }
}

/// Rails `Inclusion#dimension_ids`, resolved from names when not cached.
pub fn inclusion_dimension_ids(universe: &Universe, rule: &Inclusion) -> Vec<Id> {
    if let Some(ids) = &rule.dimension_ids {
        return ids.clone();
    }
    let names: Vec<String> = rule.dimensions.iter().map(|e| e.trim().to_lowercase()).collect();
    universe
        .snapshot()
        .fields
        .iter()
        .filter(|f| f.kind == FieldKind::Dimension && names.contains(&f.name.to_lowercase()))
        .map(|f| f.id)
        .collect()
}

/// Whether another query node already outputs every projection of this one.
pub fn covered_elsewhere(graph: &Graph, id: NodeId) -> bool {
    let mine: Vec<usize> = graph.nodes[id].projections.iter().map(|p| p.proj.pid).collect();
    graph.live().any(|n| n.id != id && n.kind != NodeKind::Merge && mine.iter().all(|pid| n.has_proj(*pid)))
}

/// Dimension ids a calculation stands on, directly or through aliases.
pub fn calc_dimension_ids(calc: &RProjection, q: &RQuery) -> Vec<Id> {
    let mut ids: Vec<Id> = calc.field_refs.iter().filter(|f| f.is_dimension()).map(|f| f.id).collect();
    for a in &calc.def.projection_refs {
        if let Some(p) = q.projections.iter().find(|p| p.match_alias(a)) {
            if !p.is_calculation() && p.is_dimension() {
                if let Some(f) = p.field {
                    ids.push(f.id);
                }
            }
        }
    }
    ids
}
