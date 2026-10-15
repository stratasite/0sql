//! Rails `SegmentForkVisitor`: measures under different segment signatures
//! cannot share an aggregation, and a segmented measure's node must sit on
//! a universe that reaches the segment's keys. Runs first, so every node
//! later visitors derive is born already forked.

use crate::dag::{Filtered, Graph, NodeId, NodeKind, Projected};
use crate::model::{RFilter, RProjection, RQuery};
use crate::pruner::sort_for_max_hints_and_dims;
use crate::resolver::set_subtrees;
use crate::universe::{Base, Universes};
use crate::visitors::grain::{create_agg_node, create_merge_node, dimensionally_equivalent};
use crate::visitors::segmentation::{component_links, keys_for, node_signature, signature, Signature};
use crate::{PlanError, Result};
use std::rc::Rc;
use std::sync::Arc;
use zsql_semantic::{Field, Universe};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, universes: &Universes<'a>, q: &RQuery<'a>) -> Result<()> {
    for id in graph.post_order() {
        if graph.nodes[id].removed || graph.nodes[id].kind != NodeKind::Aggregation {
            continue;
        }
        adopt_calculation_links(graph, id, q)?;
        let Some(buckets) = bucket_measures(graph, id, universe, universes, q)? else { continue };
        let mut order: Vec<usize> = (0..buckets.len()).collect();
        order.sort_by_cached_key(|i| {
            let (sig, uni, _) = &buckets[*i];
            (if sig.is_empty() { 0 } else { 1 }, sig_to_s(sig), uni.to_s().to_string())
        });
        let home = order[0];
        for i in order.iter().skip(1) {
            let (sig, uni, projs) = &buckets[*i];
            fork_measures(graph, id, universe, q, projs, sig, uni.clone())?;
        }
        if order.len() > 1 {
            materialize_filter_columns(graph, id);
        }
        let (_, home_universe, _) = &buckets[home];
        let current = graph.nodes[id].universe.clone().expect("agg universe");
        if home_universe.root.id != current.root.id {
            rebind_universe(graph, id, universe, q, home_universe.clone())?;
        }
    }
    Ok(())
}

/// Ruby `Array#to_s` of a signature, as Rails sorts buckets by it.
fn sig_to_s(sig: &Signature) -> String {
    let parts: Vec<String> = sig.iter().map(|(s, m)| format!("[{}, \"{}\"]", s, m.name())).collect();
    format!("[{}]", parts.join(", "))
}

fn rebind_universe<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, q: &RQuery<'a>, base: Arc<Base<'a>>) -> Result<()> {
    graph.nodes[id].projections.retain(|p| p.proj.persisted || p.field().map(|f| base.has_field(f.id)).unwrap_or(true));
    graph.nodes[id].universe = Some(base);
    set_subtrees(graph, id, universe, q)
}

/// Rails `Dag.materialize_filter_columns` onto a node.
fn materialize_filter_columns<'a>(graph: &mut Graph<'a>, id: NodeId) {
    let fields: Vec<&'a Field> = graph.nodes[id].filters.iter().filter(|f| f.is_measure()).map(|f| f.field()).collect();
    for f in fields {
        let clean =
            graph.nodes[id].projections.iter().any(|p| p.field().map(|x| x.id == f.id).unwrap_or(false) && p.decorators().is_empty());
        if !clean {
            graph.nodes[id].add_projection(Rc::new(RProjection::for_field(f)));
        }
    }
}

/// Measure-level links on a calculation cascade to the aggregations it reads.
fn adopt_calculation_links<'a>(graph: &mut Graph<'a>, id: NodeId, q: &RQuery<'a>) -> Result<()> {
    let linked: Vec<Rc<RProjection<'a>>> =
        q.projections.iter().filter(|p| p.is_calculation() && p.persisted && !p.links.is_empty()).cloned().collect();
    for calc in linked {
        for field in referenced_measure_fields(&calc, q)? {
            let projs: Vec<Rc<RProjection<'a>>> = graph.nodes[id]
                .projections
                .iter()
                .filter(|p| !p.is_calculation() && p.field().map(|f| f.id == field.id).unwrap_or(false))
                .map(|p| p.proj.clone())
                .collect();
            for proj in projs {
                if proj.persisted || !component_links(&proj).is_empty() {
                    if signature(&proj) != signature(&calc) {
                        return Err(conflict(&calc, &field.name, q));
                    }
                } else {
                    proj.set_source(Some(calc.clone()));
                }
            }
        }
    }
    Ok(())
}

fn referenced_measure_fields<'a>(calc: &Rc<RProjection<'a>>, q: &RQuery<'a>) -> Result<Vec<&'a Field>> {
    let mut out: Vec<&Field> = calc.field_refs.iter().copied().filter(|f| f.is_measure()).collect();
    for a in &calc.def.projection_refs {
        let Some(p) = q.projections.iter().find(|p| p.match_alias(a)) else { continue };
        if !p.is_measure() {
            continue;
        }
        if p.is_calculation() {
            if signature(p) != signature(calc) {
                return Err(conflict(calc, &p.alias(), q));
            }
            continue;
        }
        if let Some(f) = p.field {
            if !out.iter().any(|x| x.id == f.id) {
                out.push(f);
            }
        }
    }
    Ok(out)
}

fn conflict<'a>(calc: &Rc<RProjection<'a>>, name: &str, q: &RQuery<'a>) -> PlanError {
    let segments: Vec<String> = component_links(calc).iter().filter_map(|(s, _)| q.segment(*s)).map(|s| s.name()).collect();
    PlanError::Resolution(format!(
        "Segment {} cannot be applied to calculation {}: it references {}, which aggregates outside the calculation's segment. Apply the same segment to it, or remove the reference.",
        segments.join(", "),
        calc.alias(),
        name
    ))
}

type Bucket<'a> = (Signature, Arc<Base<'a>>, Vec<Rc<RProjection<'a>>>);

/// Measures grouped by (signature, universe); None when nothing is segmented.
fn bucket_measures<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    universe: &'a Universe,
    universes: &Universes<'a>,
    q: &RQuery<'a>,
) -> Result<Option<Vec<Bucket<'a>>>> {
    let members: Vec<Rc<RProjection<'a>>> =
        graph.nodes[id].projections.iter().filter(|p| p.is_measure() && !p.is_calculation()).map(|p| p.proj.clone()).collect();
    if members.iter().all(|p| signature(p).is_empty()) {
        return Ok(None);
    }
    let node_universe = graph.nodes[id].universe.clone().expect("agg universe");
    let mut buckets: Vec<Bucket<'a>> = Vec::new();
    let push = |sig: Signature, uni: Arc<Base<'a>>, p: Option<Rc<RProjection<'a>>>, buckets: &mut Vec<Bucket<'a>>| match buckets
        .iter_mut()
        .find(|(s, u, _)| *s == sig && u.root.id == uni.root.id)
    {
        Some((_, _, v)) => {
            if let Some(p) = p {
                v.push(p);
            }
        }
        None => buckets.push((sig, uni, p.into_iter().collect())),
    };
    // Rails groups by signature first (insertion order), then buckets per group.
    let mut groups: Vec<(Signature, Vec<Rc<RProjection<'a>>>)> = Vec::new();
    for p in &members {
        let sig = signature(p);
        match groups.iter_mut().find(|(s, _)| *s == sig) {
            Some((_, v)) => v.push(p.clone()),
            None => groups.push((sig, vec![p.clone()])),
        }
    }
    for (sig, projs) in groups {
        if sig.is_empty() {
            for p in projs {
                push(sig.clone(), node_universe.clone(), Some(p), &mut buckets);
            }
        } else {
            for p in projs {
                let uni = universe_for(graph, id, universe, universes, q, &p)?;
                push(sig.clone(), uni, Some(p), &mut buckets);
            }
        }
    }
    let orphan_filter = graph.nodes[id].filters.iter().any(|f| f.is_measure() && owner_projection(&graph.nodes[id], f).is_none());
    if orphan_filter {
        push(vec![], node_universe, None, &mut buckets);
    }
    Ok(Some(buckets))
}

/// The persisted projection a measure filter belongs to.
fn owner_projection<'a>(node: &crate::dag::Node<'a>, f: &Filtered<'a>) -> Option<Rc<RProjection<'a>>> {
    node.projections
        .iter()
        .find(|p| {
            p.proj.persisted && !p.is_calculation() && p.field().map(|x| x.id == f.field().id).unwrap_or(false) && p.decorators().is_empty()
        })
        .or_else(|| {
            node.projections
                .iter()
                .find(|p| p.proj.persisted && !p.is_calculation() && p.field().map(|x| x.id == f.field().id).unwrap_or(false))
        })
        .map(|p| p.proj.clone())
}

fn fork_measures<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    universe: &'a Universe,
    q: &RQuery<'a>,
    projs: &[Rc<RProjection<'a>>],
    sig: &Signature,
    uni: Arc<Base<'a>>,
) -> Result<()> {
    let moved_filters: Vec<Filtered<'a>> = {
        let node = &graph.nodes[id];
        node.filters
            .iter()
            .filter(|f| f.is_measure() && owner_projection(node, f).map(|o| projs.iter().any(|p| p.pid == o.pid)).unwrap_or(false))
            .cloned()
            .collect()
    };
    let followers: Vec<Rc<RProjection<'a>>> = graph.nodes[id]
        .projections
        .iter()
        .filter(|p| {
            !p.proj.persisted
                && p.is_measure()
                && component_links(&p.proj).is_empty()
                && moved_filters.iter().any(|f| Some(f.field().id) == p.field().map(|x| x.id))
        })
        .map(|p| p.proj.clone())
        .collect();
    let mut built: Vec<Rc<RProjection<'a>>> = Vec::new();
    for f in &moved_filters {
        let projected = projs.iter().chain(followers.iter()).any(|p| p.field_id() == Some(f.field().id) && p.def.decorators.is_empty());
        if !projected {
            built.push(Rc::new(RProjection::for_field(f.field())));
        }
    }
    let moved: Vec<Rc<RProjection<'a>>> = projs.iter().cloned().chain(followers.iter().cloned()).collect();
    let dims: Vec<Projected<'a>> = graph.nodes[id].projections.iter().filter(|p| p.is_dimension()).cloned().collect();
    let mut filters: Vec<Filtered<'a>> = graph.nodes[id].filters.iter().filter(|f| f.is_dimension()).cloned().collect();
    filters.extend(moved_filters.iter().cloned());

    let target = graph
        .live()
        .find(|c| {
            c.id != id
                && c.kind == NodeKind::Aggregation
                && dimensionally_equivalent(c, &dims, &filters)
                && node_signature(c) == *sig
                && moved
                    .iter()
                    .chain(built.iter())
                    .all(|p| p.field.map(|f| c.universe.as_ref().map(|u| u.has_field(f.id)).unwrap_or(false)).unwrap_or(false))
        })
        .map(|c| c.id);

    match target {
        Some(t) => {
            for p in moved.iter().chain(built.iter()) {
                graph.nodes[t].add_projection(p.clone());
            }
            for f in &moved_filters {
                if !graph.nodes[t].filters.iter().any(|x| x.fid() == f.fid()) {
                    graph.nodes[t].filters.push(f.clone());
                }
            }
        }
        None => {
            let merge = if graph.nodes[id].is_root() {
                let m = create_merge_node(graph, q);
                graph.add_edge(id, m);
                m
            } else {
                graph.root()
            };
            let mut projections: Vec<Rc<RProjection<'a>>> = graph.nodes[id]
                .projections
                .iter()
                .filter(|p| p.is_dimension() || moved.iter().any(|m| m.pid == p.proj.pid))
                .map(|p| p.proj.clone())
                .collect();
            projections.extend(built.iter().cloned());
            let rfilters: Vec<Rc<RFilter<'a>>> = filters.iter().map(|f| f.filter.clone()).collect();
            let t = create_agg_node(graph, universe, q, uni, projections, rfilters)?;
            graph.add_edge(t, merge);
        }
    }
    for p in &moved {
        graph.nodes[id].delete_projection(p.pid);
    }
    for f in &moved_filters {
        graph.nodes[id].delete_filter(f.fid());
    }
    Ok(())
}

/// The universe a segmented measure's node must sit on.
fn universe_for<'a>(
    graph: &Graph<'a>,
    id: NodeId,
    universe: &'a Universe,
    universes: &Universes<'a>,
    q: &RQuery<'a>,
    p: &Rc<RProjection<'a>>,
) -> Result<Arc<Base<'a>>> {
    let node = &graph.nodes[id];
    let keys = keys_for(p, q);
    let field = p.field.expect("measure projection field");
    let mut required: Vec<&Field> = node.projections.iter().filter(|x| x.is_dimension()).filter_map(|x| x.field()).collect();
    required.extend(node.filters.iter().filter(|f| f.is_dimension()).map(|f| f.field()));
    required.push(field);
    let mut seen = std::collections::HashSet::new();
    required.retain(|f| seen.insert(f.id));
    let covers = |u: &Base<'a>| required.iter().all(|f| u.has_field(f.id)) && keys.iter().all(|k| u.has_dimension(k.id));
    let current = node.universe.clone().expect("agg universe");
    if covers(&current) {
        return Ok(current);
    }
    let candidates: Vec<Arc<Base<'a>>> =
        universes.bases.iter().filter(|u| u.datasource.id == graph.datasource.id && covers(u)).cloned().collect();
    if candidates.is_empty() {
        let segments: Vec<String> = component_links(p).iter().filter_map(|(s, _)| q.segment(*s)).map(|s| s.name()).collect();
        let message = format!(
            "Segment {} cannot be applied to {}: no universe reaches key dimension {} together with the measure.",
            segments.join(", "),
            field.name,
            keys.iter().map(|k| k.name.clone()).collect::<Vec<_>>().join(", ")
        );
        if universes.bases.iter().any(|u| covers(u)) {
            return Err(PlanError::SegmentDatasource(message));
        }
        return Err(PlanError::Resolution(message));
    }
    let dims: Vec<&Field> = node.projections.iter().filter(|x| x.is_dimension()).filter_map(|x| x.field()).collect();
    let dim_filters: Vec<Rc<RFilter<'a>>> = node.filters.iter().filter(|f| f.is_dimension()).map(|f| f.filter.clone()).collect();
    let sorted = sort_for_max_hints_and_dims(candidates, &[field], &dims, &q.hints, &dim_filters, universe);
    Ok(sorted[0].clone())
}
