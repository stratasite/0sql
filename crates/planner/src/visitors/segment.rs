//! Rails `SegmentVisitor`: each segment is planned once and imported as one
//! set of nodes; every consumer joins its root through a strategy, so the
//! final SQL carries exactly one CTE per segment.

use crate::dag::{Entity, Graph, NodeId, NodeKind, Owner, RefKey, Reference, Strategy, StrategyKind};
use crate::model::{FilledTree, RQuery, RSegment};
use crate::resolver::{self, Options};
use crate::universe::Universes;
use crate::visitors::segmentation::component_links;
use crate::{PlanError, Result};
use std::collections::HashMap;
use std::rc::Rc;
use zsql_dialect::Dialects;
use zsql_spec::LinkMode;
use zsql_semantic::Universe;

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, universes: &Universes<'a>, dialects: &Dialects, q: &RQuery<'a>) -> Result<()> {
    if q.is_segment() {
        return Ok(());
    }
    let mut segment_nodes: HashMap<usize, NodeId> = HashMap::new();
    for id in graph.post_order() {
        if graph.nodes[id].removed || graph.nodes[id].owner.is_some() {
            continue;
        }
        if !consumer(&graph.nodes[id]) {
            verify_expanded_bindings(graph, id, q)?;
            continue;
        }
        for (sid, mode) in links_for(graph, id, q) {
            let segment = q.segment(sid).expect("segment").clone();
            let seg_node = match segment_nodes.get(&sid) {
                Some(n) => *n,
                None => {
                    let n = build_segment_node(graph, universe, universes, dialects, &segment)?;
                    segment_nodes.insert(sid, n);
                    n
                }
            };
            verify_reachable(graph, id, &segment, q)?;
            let kind = match mode {
                LinkMode::ExpandInner => StrategyKind::ExpandByJoinKeys { sid, left: false },
                LinkMode::ExpandLeft => StrategyKind::ExpandByJoinKeys { sid, left: true },
                LinkMode::IncludeIn => StrategyKind::SegmentByJoinKeys { limit: None },
                LinkMode::ExcludeFrom => StrategyKind::ExcludeByJoinKeys,
            };
            graph.add_strategy_edge(Strategy { node: seg_node, kind }, id);
            if mode.is_expanding() {
                bind_expanded_columns(graph, id, seg_node, &segment)?;
            }
        }
        verify_expanded_bindings(graph, id, q)?;
    }
    Ok(())
}

/// Segments apply where the facts are computed: generic and temporal query
/// or aggregation nodes.
fn consumer(n: &crate::dag::Node) -> bool {
    matches!(n.kind, NodeKind::Query | NodeKind::Aggregation) && matches!(n.purpose, None | Some("temporal"))
}

/// Measure-level links, then query-level ones not overridden per segment;
/// unique by (segment, mode), sorted the way Rails sorts them.
fn links_for<'a>(graph: &Graph<'a>, id: NodeId, q: &RQuery<'a>) -> Vec<(usize, LinkMode)> {
    let node = &graph.nodes[id];
    let mut links: Vec<(usize, LinkMode)> = node.projections.iter().filter(|p| p.is_measure()).flat_map(|p| component_links(&p.proj)).collect();
    let measure_sids: Vec<usize> = links.iter().map(|(s, _)| *s).collect();
    for (sid, mode) in q.query_level_links() {
        if !measure_sids.contains(&sid) {
            links.push((sid, mode));
        }
    }
    let mut seen = std::collections::HashSet::new();
    links.retain(|l| seen.insert(*l));
    links.sort_by(|a, b| a.0.cmp(&b.0).then(a.1.name().cmp(b.1.name())));
    links
}

/// Rails `Planner.plan_segment_graph` plus import: resolution and the
/// fact-shaping visitors, with the datasource pinned to the consumer's.
fn build_segment_node<'a>(graph: &mut Graph<'a>, universe: &'a Universe, universes: &Universes<'a>, dialects: &Dialects, segment: &Rc<RSegment<'a>>) -> Result<NodeId> {
    let sq = segment.as_query();
    let opts = Options { datasource: Some(graph.datasource), ..Default::default() };
    let mut sub = match resolver::resolve_with(universe, universes, dialects, &sq, &opts) {
        Ok(g) => g,
        Err(PlanError::Resolution(m)) => return Err(PlanError::Resolution(format!("Could not resolve segment {}: {}", segment.name(), m))),
        Err(e) => return Err(e),
    };
    sub.dialect = graph.dialect.clone();
    crate::visitors::complex_measure::run(&mut sub, universe, universes, dialects, &sq)?;
    crate::visitors::exclusion::run(&mut sub, universe, &sq)?;
    crate::visitors::inclusion::run(&mut sub, universe, &sq)?;
    crate::visitors::snapshot::run(&mut sub, universe, &sq)?;
    crate::visitors::top_n::run(&mut sub, universe, universes, dialects, &sq)?;

    let owner = Rc::new(Owner { sid: segment.sid, tree: sq.tree.clone(), identity: tree_identity(segment) });
    let first_new = graph.nodes.len();
    let root = graph.import(sub);
    for n in graph.nodes.iter_mut().skip(first_new) {
        n.owner = Some(owner.clone());
    }
    if graph.nodes[root].purpose.is_none() {
        graph.nodes[root].purpose = Some("strategy");
    }
    if !graph.nodes[root].is_agg() && !graph.nodes[root].is_merge() {
        graph.nodes[root].should_group_by = Some(true);
    }
    Ok(root)
}

/// Rails `Node#owner_tree_identity`: a custom tree shape, leaves rendered as
/// the filter's text, in Ruby's `inspect` form.
fn tree_identity(segment: &RSegment) -> String {
    if segment.def.filter_tree.is_none() || !segment.custom_tree {
        return String::new();
    }
    fn render(t: &FilledTree, segment: &RSegment) -> String {
        match t {
            FilledTree::Group(op, kids) => {
                let mut parts = vec![ruby_str(op)];
                parts.extend(kids.iter().map(|k| render(k, segment)));
                format!("[{}]", parts.join(", "))
            }
            FilledTree::Leaf(fid) => {
                let f = segment.filters.iter().find(|f| f.fid == *fid).expect("segment filter");
                ruby_str(&format!("<f:{}:{}:{}>", crate::model::field_to_s(f.field), f.def.predicate.name(), f.def.filter_value.clone().unwrap_or_default()))
            }
        }
    }
    format!(" \nfilter_tree: {}", render(&segment.tree, segment))
}

fn ruby_str(s: &str) -> String {
    format!("\"{}\"", s.replace('\\', "\\\\").replace('"', "\\\""))
}

/// Pre-binds each carried column to the segment node's definition projection.
fn bind_expanded_columns<'a>(graph: &mut Graph<'a>, id: NodeId, seg_node: NodeId, segment: &Rc<RSegment<'a>>) -> Result<()> {
    let carried: Vec<_> = graph.nodes[id].projections.iter().filter(|p| p.proj.carried_from.map(|(_, s)| s == segment.sid).unwrap_or(false)).map(|p| p.proj.clone()).collect();
    for proj in carried {
        let (parent_pid, _) = proj.carried_from.expect("carried");
        let Some(seg_proj) = graph.nodes[seg_node].projected(parent_pid).map(|p| p.proj.clone()) else {
            return Err(PlanError::Resolution(format!(
                "Segment {} does not expand {} — remove the carried column or add the dimension to the segment's expanding zone.",
                segment.name(),
                proj.field.map(|f| f.name.clone()).unwrap_or_default()
            )));
        };
        graph.nodes[id].field_refs.insert(RefKey::Proj(proj.pid), Reference::Node { entity: Entity::Proj(seg_proj), node: seg_node });
    }
    Ok(())
}

fn verify_expanded_bindings<'a>(graph: &Graph<'a>, id: NodeId, q: &RQuery<'a>) -> Result<()> {
    let node = &graph.nodes[id];
    if node.kind == NodeKind::Merge {
        return Ok(());
    }
    for p in node.projections.iter().filter(|p| p.proj.is_carried()) {
        if !node.field_refs.contains_key(&RefKey::Proj(p.proj.pid)) {
            let kind = if q.def.population { "segment" } else { "query" };
            return Err(PlanError::Resolution(format!(
                "{} reads from an expanding segment that does not apply here — expanding segments cannot combine with top-n rankings or snapshot resolution, and the segment must be linked to this {}.",
                p.alias, kind
            )));
        }
    }
    Ok(())
}

fn verify_reachable<'a>(graph: &Graph<'a>, id: NodeId, segment: &Rc<RSegment<'a>>, q: &RQuery<'a>) -> Result<()> {
    let node = &graph.nodes[id];
    let base = node.universe.as_ref().expect("consumer universe");
    let missing: Vec<String> = segment.key_dimensions().iter().filter(|k| !base.has_dimension(k.id)).map(|k| k.name.clone()).collect();
    if missing.is_empty() {
        return Ok(());
    }
    let measures: Vec<String> = node.projections.iter().filter(|p| p.is_measure()).map(|p| p.alias.clone()).collect();
    let desc = if measures.is_empty() { q.def.name.clone().unwrap_or_default() } else { format!("measure {}", measures.join(", ")) };
    Err(PlanError::Resolution(format!(
        "Segment {} cannot be applied to {}: key dimension {} is not reachable from {}.",
        segment.name(),
        desc,
        missing.join(", "),
        base.root.name
    )))
}
