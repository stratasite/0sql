//! Rails `TopNVisitor`: a top-n filter becomes a ranking node joined onto
//! the filtered node by the ranked dimension, limited to n rows.

use crate::dag::{Graph, NodeId, NodeKind, Strategy, StrategyKind};
use crate::model::{FilledTree, RFilter, RProjection, RQuery};
use crate::resolver::{self, Options};
use crate::universe::Universes;
use crate::visitors::grain::create_agg_node;
use crate::{PlanError, Result};
use std::rc::Rc;
use zsql_dialect::Dialects;
use zsql_semantic::{FieldKind, Predicate, Universe};
use zsql_spec::OrderBy;

pub fn run<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &RQuery<'a>,
) -> Result<()> {
    for id in graph.post_order() {
        let n = &graph.nodes[id];
        if n.removed || n.kind == NodeKind::Merge || !n.filters.iter().any(|f| f.predicate == Predicate::TopN) {
            continue;
        }
        let top_filters: Vec<Rc<RFilter<'a>>> =
            graph.nodes[id].filters.iter().filter(|f| f.predicate == Predicate::TopN).map(|f| f.filter.clone()).collect();
        for f in top_filters {
            create_top_n_segmentation(graph, id, universe, universes, dialects, q, &f)?;
        }
    }
    Ok(())
}

fn create_top_n_segmentation<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    universe: &'a Universe,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &RQuery<'a>,
    top: &Rc<RFilter<'a>>,
) -> Result<()> {
    let filters: Vec<Rc<RFilter<'a>>> =
        graph.nodes[id].filters.iter().filter(|f| f.predicate != Predicate::TopN).map(|f| f.filter.clone()).collect();
    let filter_fids: Vec<usize> = filters.iter().map(|f| f.fid).collect();
    let measure = top.def.top_n_measure.as_ref().and_then(|u| universe.field_by_uid(u, Some(FieldKind::Measure)));
    let dimension = top.field;

    let existing = graph
        .live()
        .find(|n| {
            n.purpose == Some("strategy")
                && n.projections.iter().find(|p| p.is_dimension()).and_then(|p| p.field()).map(|f| f.id == dimension.id).unwrap_or(false)
                && (measure.is_none()
                    || n.projections
                        .iter()
                        .find(|p| p.is_measure())
                        .and_then(|p| p.field())
                        .map(|f| Some(f.id) == measure.map(|m| m.id))
                        .unwrap_or(false))
                && n.filters.len() == filters.len()
                && n.filters.iter().all(|f| filter_fids.contains(&f.fid()))
        })
        .map(|n| n.id);

    let top_node = match existing {
        Some(n) => n,
        None => match measure {
            Some(m) => build_via_sub_graph(graph, universe, universes, dialects, q, dimension, m)?,
            None => build_count_node(graph, universe, q, id, dimension, filters)?,
        },
    };
    let limit = top.def.filter_value.clone();
    graph.nodes[top_node].should_order_by = Some(limit.is_some());
    graph.add_strategy_edge(Strategy { node: top_node, kind: StrategyKind::SegmentByJoinKeys { limit } }, id);
    graph.nodes[id].delete_filter(top.fid);
    Ok(())
}

/// The ranking as a sub-plan, so the resolver picks the measure's fact table.
fn build_via_sub_graph<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &RQuery<'a>,
    dimension: &'a zsql_semantic::Field,
    measure: &'a zsql_semantic::Field,
) -> Result<NodeId> {
    let mut dim = RProjection::for_field(dimension);
    dim.def.ordinal = Some(0);
    let mut msr = RProjection::for_field(measure);
    msr.def.ordinal = Some(1);
    msr.def.order_by = Some(OrderBy::Desc);
    let filters: Vec<Rc<RFilter<'a>>> =
        q.filters.iter().filter(|f| f.def.predicate != Predicate::TopN && f.is_dimension()).cloned().collect();
    let sub = RQuery {
        def: zsql_spec::Query::planner_sub(format!("{}-top_n", q.def.name.clone().unwrap_or_default())),
        projections: vec![Rc::new(dim), Rc::new(msr)],
        filters: filters.clone(),
        hints: vec![],
        tree: FilledTree::Group("and".into(), filters.iter().map(|f| FilledTree::Leaf(f.fid)).collect()),
        extras: Default::default(),
        segments: vec![],
    };
    let opts = Options { auto_level_dims: true, datasource: Some(graph.datasource), ..Default::default() };
    let mut sub_graph = resolver::resolve_with(universe, universes, dialects, &sub, &opts)?;
    sub_graph.dialect = graph.dialect.clone();
    crate::visitors::complex_measure::run(&mut sub_graph, universe, universes, dialects, &sub)?;
    let root = graph.import(sub_graph);
    graph.nodes[root].purpose = Some("strategy");
    graph.nodes[root].should_order_by = Some(true);
    Ok(root)
}

/// Rank by row count when no measure is named.
fn build_count_node<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    id: NodeId,
    dimension: &'a zsql_semantic::Field,
    filters: Vec<Rc<RFilter<'a>>>,
) -> Result<NodeId> {
    let mut calc = (*q.static_calc(universe, "row_count_measure", "count(*)")).clone_shallow();
    calc.def.order_by = Some(OrderBy::Desc);
    calc.def.ordinal = Some(1);
    let base = graph.nodes[id].universe.clone().ok_or_else(|| PlanError::Resolution("top-n on a node without a universe".into()))?;
    let projections = vec![Rc::new(RProjection::for_field(dimension)), Rc::new(calc)];
    let top = create_agg_node(graph, universe, q, base, projections, filters)?;
    graph.nodes[top].purpose = Some("strategy");
    Ok(top)
}
