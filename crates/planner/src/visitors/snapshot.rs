//! Rails `SnapshotVisitor`: a snapshot measure is read at the first or last
//! snapshot date within the node's grain. A side node resolves that date
//! per grain and joins back onto the snapshot table.

use crate::dag::{Graph, NodeId, NodeKind, Projected, Strategy, StrategyKind};
use crate::model::{RProjection, RQuery};
use crate::visitors::grain::{create_agg_node, create_merge_node};
use crate::{PlanError, Result};
use std::rc::Rc;
use zsql_spec::DecoratorKind;
use zsql_semantic::{Field, SnapshotKind, Table, Universe};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    for id in graph.post_order() {
        if graph.nodes[id].removed || graph.nodes[id].kind != NodeKind::Aggregation {
            continue;
        }
        materialize_filtered_snapshots(graph, id);
        if snapshot_measures(&graph.nodes[id]).is_empty() {
            continue;
        }
        let sets = group_by_snapshot_date(graph, id, universe)?;
        create_snap_nodes(graph, id, universe, q, sets)?;
    }
    Ok(())
}

/// A snapshot measure that is only filtered still needs a projection so
/// the as-of machinery sees it.
fn materialize_filtered_snapshots<'a>(graph: &mut Graph<'a>, id: NodeId) {
    let fields: Vec<&'a Field> = graph.nodes[id].filters.iter().filter(|f| f.is_measure() && f.field().snapshot.is_some()).map(|f| f.field()).collect();
    for f in fields {
        let projected = graph.nodes[id].projections.iter().any(|p| p.field().map(|x| x.id == f.id).unwrap_or(false));
        if !projected {
            graph.nodes[id].add_projection(Rc::new(RProjection::for_field(f)));
        }
    }
}

fn snapshot_measures<'a>(node: &crate::dag::Node<'a>) -> Vec<&'a Field> {
    let mut out: Vec<&Field> = Vec::new();
    for p in &node.projections {
        if p.is_calculation() {
            for f in &p.proj.field_refs {
                if f.is_measure() && f.snapshot.is_some() && !out.iter().any(|x| x.id == f.id) {
                    out.push(f);
                }
            }
        } else if let Some(f) = p.field() {
            if f.is_measure() && f.snapshot.is_some() && !out.iter().any(|x| x.id == f.id) {
                out.push(f);
            }
        }
    }
    out
}

/// Measures grouped by snapshot table and kind, in first-seen order.
fn group_by_snapshot_date<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe) -> Result<Vec<(String, Vec<&'a Field>)>> {
    let mut groups: Vec<(String, Vec<&'a Field>)> = Vec::new();
    for m in snapshot_measures(&graph.nodes[id]) {
        let exp = graph.nodes[id].expression(universe, m)?;
        let key = format!("{}_{}", exp.table_id, match m.snapshot { Some(SnapshotKind::Beginning) => "beginning", _ => "ending" });
        match groups.iter_mut().find(|(k, _)| *k == key) {
            Some((_, v)) => v.push(m),
            None => groups.push((key, vec![m])),
        }
    }
    Ok(groups)
}

fn create_snap_nodes<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, q: &RQuery<'a>, sets: Vec<(String, Vec<&'a Field>)>) -> Result<()> {
    let mut root = graph.root();
    let mut snap_nodes = Vec::new();
    for (_, measures) in sets {
        let mut projections: Vec<Rc<RProjection<'a>>> = graph.nodes[id].projections.iter().filter(|p| p.is_dimension()).map(|p| p.proj.clone()).collect();
        for m in &measures {
            let existing: Vec<Rc<RProjection<'a>>> =
                graph.nodes[id].projections.iter().filter(|p| p.field().map(|f| f.id == m.id).unwrap_or(false)).map(|p| p.proj.clone()).collect();
            if existing.is_empty() {
                projections.push(Rc::new(RProjection::for_field(m)));
            } else {
                projections.extend(existing);
            }
        }
        let filters: Vec<_> = graph.nodes[id].filters.iter().map(|f| f.filter.clone()).collect();
        for m in &measures {
            graph.nodes[id].delete_field(m.id);
        }
        let base = graph.nodes[id].universe.clone().expect("agg universe");
        let snap = create_agg_node(graph, universe, q, base, projections, filters)?;
        create_snapshot_query(graph, snap, universe, q)?;
        snap_nodes.push(snap);
    }

    if !graph.nodes[root].is_merge() && snap_nodes.len() > 1 {
        root = create_merge_node(graph, q);
    }
    for s in &snap_nodes {
        graph.add_edge(*s, root);
    }

    if !graph.nodes[id].projections.iter().any(|p| p.is_measure()) {
        let calcs: Vec<Projected<'a>> = graph.nodes[id].projections.iter().filter(|p| p.is_calculation()).cloned().collect();
        graph.destroy_node(id);
        let new_root = graph.root();
        if id == root && !graph.nodes[new_root].is_merge() {
            for c in calcs {
                if !graph.nodes[new_root].has_proj(c.proj.pid) {
                    graph.nodes[new_root].projections.push(c);
                }
            }
            let all = q.all_projections();
            if graph.nodes[new_root].projections.len() > all.len() {
                let projected: Vec<i64> = all.iter().filter_map(|p| p.field_id()).collect();
                graph.nodes[new_root].projections.retain(|p| p.field().map(|f| projected.contains(&f.id)).unwrap_or(true));
            }
        }
    }
    Ok(())
}

fn create_snapshot_query<'a>(graph: &mut Graph<'a>, snap: NodeId, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    let first_measure = graph.nodes[snap].measures().into_iter().next().ok_or_else(|| PlanError::Resolution("snapshot node without measure".into()))?;
    let snap_point = first_measure.snapshot;
    let exp = graph.nodes[snap].expression(universe, first_measure)?;
    let snap_table: &'a Table = universe.table(exp.table_id).expect("snapshot table");
    let snap_date_dim = snap_table
        .snapshot_date_id
        .and_then(|d| universe.field(d))
        .ok_or_else(|| PlanError::Resolution(format!("{} is not a snapshot table", snap_table.name)))?;

    let snap_date_proj = snapshot_date_projection(&graph.nodes[snap], snap_date_dim, universe);
    let snap_date_filter = snapshot_date_filter(&graph.nodes[snap], snap_date_dim, snap_date_proj.as_ref(), universe);
    if grain_equals_snap_grain(snap_date_proj.as_ref()) {
        return Ok(());
    }

    let agg = if snap_point == Some(SnapshotKind::Beginning) { "MIN" } else { "MAX" };
    let sql = format!("{agg}([{}]@d)", snap_date_dim.name);
    let calc = Rc::new(RProjection::for_calc_typed(sql, "SnapshotJoinDate".into(), Some(q.next_ordinal()), vec![], vec![snap_date_dim], Some("measure"), true));
    q.push_static(calc.clone());

    let mut projections: Vec<Rc<RProjection<'a>>> = Vec::new();
    if let Some(p) = &snap_date_proj {
        projections.push(p.proj.clone());
    }
    projections.push(calc.clone());
    let filters = snap_date_filter.map(|f| vec![f.filter.clone()]).unwrap_or_default();
    let base = graph.nodes[snap].universe.clone().expect("universe");
    let snap_query = create_agg_node(graph, universe, q, base, projections, filters)?;
    graph.nodes[snap_query].purpose = Some("snapshot");
    let strategy = Strategy { node: snap_query, kind: StrategyKind::SegmentOnTable { table: snap_table, join_type: "inner".into(), map: vec![(snap_date_dim, calc.pid)] } };
    graph.add_strategy_edge(strategy, snap);
    Ok(())
}

fn blendables<'a>(field: &Field, universe: &'a Universe) -> Vec<&'a Field> {
    match &field.extended_blend_group {
        Some(g) => universe.fields().iter().filter(|f| f.id != field.id && f.extended_blend_group.as_ref() == Some(g)).collect(),
        None => vec![],
    }
}

fn snapshot_date_projection<'a>(node: &crate::dag::Node<'a>, expected: &Field, universe: &'a Universe) -> Option<Projected<'a>> {
    if let Some(p) = node.projections.iter().find(|p| p.field().map(|f| f.id == expected.id).unwrap_or(false)) {
        return Some(p.clone());
    }
    let candidates = blendables(expected, universe);
    if let Some(p) = node.projections.iter().find(|p| p.field().map(|f| candidates.iter().any(|c| c.id == f.id)).unwrap_or(false)) {
        return Some(p.clone());
    }
    node.projections.iter().find(|p| p.field().map(|f| f.is_date()).unwrap_or(false)).cloned()
}

fn snapshot_date_filter<'a>(node: &crate::dag::Node<'a>, expected: &Field, proj: Option<&Projected<'a>>, universe: &'a Universe) -> Option<crate::dag::Filtered<'a>> {
    if let Some(p) = proj {
        let fid = p.field()?.id;
        return node.filters.iter().find(|f| f.field().id == fid).cloned();
    }
    if let Some(f) = node.filters.iter().find(|f| f.field().id == expected.id) {
        return Some(f.clone());
    }
    let candidates = blendables(expected, universe);
    if let Some(f) = node.filters.iter().find(|f| candidates.iter().any(|c| c.id == f.field().id)) {
        return Some(f.clone());
    }
    node.filters.iter().find(|f| f.is_date()).cloned()
}

/// Daily or finer grains already resolve to one snapshot per row.
fn grain_equals_snap_grain(proj: Option<&Projected>) -> bool {
    let Some(p) = proj else { return false };
    let decs = p.decorators();
    let extract = decs.iter().any(|d| d.kind == DecoratorKind::Extract);
    let truncation = decs.iter().find(|d| d.kind == DecoratorKind::Truncate).and_then(|d| d.grain());
    !extract && truncation.map(|g| matches!(g, "raw" | "millisecond" | "hour" | "minute" | "day")).unwrap_or(true)
}
