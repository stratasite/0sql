//! Rails `Planner::Universe::Resolver`: which universes answer the query,
//! and the DAG that composes them.

use crate::dag::{Graph, Node, NodeKind};
use crate::model::{RProjection, RQuery};
use crate::pruner::{prune_to_minimum, sort_for_max_hints_and_dims, sort_paths_for_max_hints_and_dims};
use crate::universe::{Base, Model, Universes};
use crate::{PlanError, Result};
use std::rc::Rc;
use std::sync::Arc;
use zsql_dialect::Dialects;
use zsql_semantic::{Field, FieldKind, Id, Universe};

fn uniq_fields(fields: Vec<&Field>) -> Vec<&Field> {
    let mut out: Vec<&Field> = Vec::new();
    for f in fields {
        if !out.iter().any(|o| o.id == f.id) {
            out.push(f);
        }
    }
    out
}

pub fn measures<'a>(universe: &'a Universe, q: &RQuery<'a>) -> Vec<&'a Field> {
    let mut uids: Vec<&str> = Vec::new();
    for p in q.projections.iter().filter(|p| !p.is_calculation() && p.is_measure()) {
        uids.push(p.def.field_uid.as_deref().unwrap_or(""));
    }
    for f in q.filters.iter().filter(|f| f.is_measure()) {
        uids.push(&f.def.field_uid);
    }
    for p in q.projections.iter().filter(|p| p.is_calculation()) {
        for u in &p.def.measure_refs {
            uids.push(u);
        }
    }
    let mut seen = std::collections::HashSet::new();
    uids.retain(|u| seen.insert(*u));
    uniq_fields(uids.iter().filter_map(|u| universe.field_by_uid(u, Some(FieldKind::Measure))).collect())
}

pub fn dimensions<'a>(universe: &'a Universe, q: &RQuery<'a>) -> Vec<&'a Field> {
    let mut uids: Vec<&str> = Vec::new();
    for p in q.projections.iter().filter(|p| !p.is_calculation() && !p.is_carried() && p.is_dimension()) {
        uids.push(p.def.field_uid.as_deref().unwrap_or(""));
    }
    for f in q.filters.iter().filter(|f| f.is_dimension()) {
        uids.push(&f.def.field_uid);
    }
    for p in q.projections.iter().filter(|p| p.is_calculation()) {
        for u in &p.def.dimension_refs {
            uids.push(u);
        }
    }
    let mut seen = std::collections::HashSet::new();
    uids.retain(|u| seen.insert(*u));
    uniq_fields(uids.iter().filter_map(|u| universe.field_by_uid(u, Some(FieldKind::Dimension))).collect())
}

/// How a resolution is constrained: a sub-plan levels dimensions loosely and
/// is pinned to the datasource that will execute it.
#[derive(Debug, Clone)]
pub struct Options<'a> {
    pub auto_level_dims: bool,
    pub datasource: Option<&'a zsql_semantic::Datasource>,
    pub required_dims: Vec<&'a Field>,
    /// Fold the query-level segments' key dimensions into universe selection.
    pub segment_keys: bool,
}

impl<'a> Default for Options<'a> {
    fn default() -> Self {
        Options { auto_level_dims: false, datasource: None, required_dims: vec![], segment_keys: true }
    }
}

pub fn resolve<'a>(universe: &'a Universe, universes: &Universes<'a>, dialects: &Dialects, q: &RQuery<'a>) -> Result<Graph<'a>> {
    resolve_with(universe, universes, dialects, q, &Options::default())
}

pub fn resolve_with<'a>(
    universe: &'a Universe,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &RQuery<'a>,
    opts: &Options<'a>,
) -> Result<Graph<'a>> {
    let segment_keys = if opts.segment_keys { q.query_level_key_dimensions() } else { vec![] };
    match resolve_inner(universe, universes, dialects, q, opts, &segment_keys) {
        Err(PlanError::Resolution(msg)) if !segment_keys.is_empty() => {
            // Re-resolve without the keys: when that works, the segments are to blame.
            let without = Options { segment_keys: false, ..opts.clone() };
            match resolve_inner(universe, universes, dialects, q, &without, &[]) {
                Ok(_) => {
                    let names: Vec<String> =
                        q.query_level_links().iter().filter_map(|(sid, _)| q.segment(*sid)).map(|s| s.name()).collect();
                    let keys: Vec<String> = segment_keys.iter().map(|k| k.name.clone()).collect();
                    Err(PlanError::Resolution(format!(
                        "Segment {} cannot be applied to the query \"{}\": key dimension {} is not reachable together with the query's fields. Apply the segment to specific measures instead, or re-key it on a dimension every field can reach.",
                        names.join(", "),
                        q.def.name.clone().unwrap_or_default(),
                        keys.join(", ")
                    )))
                }
                Err(_) => Err(PlanError::Resolution(msg)),
            }
        }
        other => other,
    }
}

fn resolve_inner<'a>(
    universe: &'a Universe,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &RQuery<'a>,
    opts: &Options<'a>,
    segment_keys: &[&'a Field],
) -> Result<Graph<'a>> {
    if q.projections.is_empty() {
        return Err(PlanError::Resolution("At least one projection required".into()));
    }
    if q.is_segment() && q.projections.iter().all(|p| !(p.is_dimension() && !p.is_calculation() && !p.def.is_expansion())) {
        return Err(PlanError::Resolution("Query is not valid: a segment needs key dimensions".into()));
    }
    let measures = measures(universe, q);
    let mut dimensions = dimensions(universe, q);
    for d in opts.required_dims.iter().chain(segment_keys.iter()) {
        if !dimensions.iter().any(|x| x.id == d.id) {
            dimensions.push(d);
        }
    }
    let dim_ids: Vec<Id> = dimensions.iter().map(|f| f.id).collect();
    let msr_ids: Vec<Id> = measures.iter().map(|f| f.id).collect();
    let ds_ids = universes.datasources_with_all_measures(universe, &measures);
    if ds_ids.is_empty() {
        return Err(PlanError::Resolution("All measures must be in the same datasource.".into()));
    }

    let candidates: Vec<Arc<Base<'a>>> = universes
        .bases
        .iter()
        .filter(|u| {
            if dimensions.is_empty() {
                ds_ids.contains(&u.datasource.id) && u.any_measures(&msr_ids)
            } else if measures.is_empty() {
                u.all_dimensions(&dim_ids)
            } else if opts.auto_level_dims {
                // A compound's reference aggregates at whatever subset of the
                // dimensions its universe reaches, possibly none.
                ds_ids.contains(&u.datasource.id) && u.any_measures(&msr_ids)
            } else {
                ds_ids.contains(&u.datasource.id) && u.all_dimensions(&dim_ids) && u.any_measures(&msr_ids)
            }
        })
        .filter(|u| opts.datasource.map(|d| d.id == u.datasource.id).unwrap_or(true))
        .cloned()
        .collect();
    if candidates.is_empty() {
        if let Some(d) = opts.datasource {
            return Err(PlanError::Resolution(format!("No universe can resolve the query within datasource {}.", d.name)));
        }
    }

    let dim_filters: Vec<_> = q.filters.iter().filter(|f| f.is_dimension()).cloned().collect();
    let chosen = {
        let _t = crate::profile::span("resolve.rank");
        let sorted = sort_for_max_hints_and_dims(candidates, &measures, &dimensions, &q.hints, &dim_filters, universe);
        prune_to_minimum(sorted, &measures, &[])?
    };

    let datasource = chosen[0].datasource;
    let dialect = dialects.get(&datasource.adapter)?.with_overrides(q.def.db_settings.iter().map(|(k, v)| (k.as_str(), v)));
    let mut graph = Graph::new(datasource, dialect);

    if chosen.len() == 1 && !calculations_depend_on_measures(q) {
        let kind = if q.has_measures() || q.calculations_with_measures() { NodeKind::Aggregation } else { NodeKind::Query };
        let u = &chosen[0];
        let (projections, filters) = if opts.auto_level_dims {
            // Level the node to the universe: what it cannot reach drops out.
            (
                q.projections.iter().filter(|p| p.field.map(|f| u.has_field(f.id)).unwrap_or(false) || p.is_carried()).cloned().collect(),
                q.filters.iter().filter(|f| u.has_field(f.field.id)).cloned().collect(),
            )
        } else {
            (q.projections.clone(), q.filters.clone())
        };
        let node = Node::with_universe(kind, u.clone(), projections, filters);
        let id = graph.add_node(node);
        set_subtrees(&mut graph, id, universe, q)?;
    } else {
        let merge_filters = q.filters.iter().filter(|f| f.is_measure() && graph.dialect.final_measure_filter()).cloned().collect();
        let mut merge = Node::merge(q.projections.clone(), merge_filters, graph.dialect.final_pass_measure_join_type().to_string());
        if q.projections.iter().any(|p| p.is_calculation() && p.is_measure()) {
            merge.should_group_by = Some(true);
        }
        let to = graph.add_node(merge);

        let pushdown: Vec<Rc<RProjection<'a>>> = q.projections.iter().filter(|c| pushable(c, q)).cloned().collect();
        let mut pushed: Vec<usize> = Vec::new();

        for u in &chosen {
            let mut projections: Vec<Rc<RProjection<'a>>> = q
                .projections
                .iter()
                .filter(|p| !p.is_calculation())
                .filter(|p| p.field.map(|f| u.has_field(f.id)).unwrap_or(false) || p.is_carried())
                .cloned()
                .collect();

            // Dimension calculations this universe can evaluate are computed
            // here, as derived dimensions.
            let local_calcs: Vec<Rc<RProjection<'a>>> = pushdown.iter().filter(|c| resolvable_in(u, c, q)).cloned().collect();
            for c in &local_calcs {
                if !pushed.contains(&c.pid) {
                    pushed.push(c.pid);
                }
            }
            projections.extend(local_calcs.iter().cloned());

            // Materialize every field the merge-level calculations reference
            // that this universe resolves, so the merge reads it as a column.
            let merge_calcs: Vec<&Rc<RProjection<'a>>> =
                q.projections.iter().filter(|p| p.is_calculation() && !local_calcs.iter().any(|l| l.pid == p.pid)).collect();
            let ref_uids: Vec<&str> =
                merge_calcs.iter().flat_map(|c| c.def.measure_refs.iter().chain(c.def.dimension_refs.iter()).map(String::as_str)).collect();
            let calc_measures = calculation_fields(universe, q, FieldKind::Measure);
            let calc_dimensions = calculation_fields(universe, q, FieldKind::Dimension);
            for f in calc_measures.iter().chain(calc_dimensions.iter()) {
                if ref_uids.contains(&f.uid.as_str()) && u.has_field(f.id) && !projections.iter().any(|p| p.field_id() == Some(f.id)) {
                    let mut rp = RProjection::for_field(f);
                    rp.def.ordinal = Some((projections.len() as i64 - 1).max(0));
                    projections.push(Rc::new(rp));
                }
            }
            // Measure filters not cleanly projected get a column in the child.
            for f in q.filters.iter().filter(|f| f.is_measure() && u.has_field(f.field.id)) {
                let projected = q.projections.iter().any(|p| p.field_id() == Some(f.field.id) && p.def.decorators.is_empty());
                if !projected {
                    projections.push(Rc::new(RProjection::for_field(f.field)));
                }
            }
            let filters = q
                .filters
                .iter()
                .filter(|f| u.has_field(f.field.id) && (f.is_dimension() || graph.dialect.intermediate_measure_filter()))
                .cloned()
                .collect();
            let node = Node::with_universe(NodeKind::Aggregation, u.clone(), projections, filters);
            let from = graph.add_node(node);
            set_subtrees(&mut graph, from, universe, q)?;
            graph.add_edge(from, to);
        }

        if let Some(c) = pushdown.iter().find(|c| !pushed.contains(&c.pid)) {
            return Err(PlanError::Resolution(format!(
                "Calculation \"{}\" references dimensions that no single universe resolves together, so it cannot be computed.",
                c.alias()
            )));
        }
    }
    Ok(graph)
}

/// Measures (or dimensions) named by calculations, in reference order, unique.
fn calculation_fields<'a>(universe: &'a Universe, q: &RQuery<'a>, kind: FieldKind) -> Vec<&'a Field> {
    let uids: Vec<&String> = q
        .projections
        .iter()
        .filter(|p| p.is_calculation())
        .flat_map(|p| if kind == FieldKind::Measure { p.def.measure_refs.iter() } else { p.def.dimension_refs.iter() })
        .collect();
    uniq_fields(uids.iter().filter_map(|u| universe.field_by_uid(u, Some(kind))).collect())
}

/// Rails `pushable?`: a calculation standing only on dimensions.
fn pushable<'a>(c: &Rc<RProjection<'a>>, q: &RQuery<'a>) -> bool {
    if !c.is_calculation() || !c.is_dimension() || c.measure_dependent(&q.projections) {
        return false;
    }
    c.def
        .projection_refs
        .iter()
        .all(|r| q.projections.iter().find(|p| p.match_alias(r)).map(|t| !t.is_calculation() && t.is_dimension()).unwrap_or(false))
}

/// Rails `resolvable_in?`: the universe reaches every field the calculation reads.
fn resolvable_in<'a>(u: &Arc<Base<'a>>, c: &Rc<RProjection<'a>>, q: &RQuery<'a>) -> bool {
    let mut fields: Vec<&Field> = c.field_refs.clone();
    for r in &c.def.projection_refs {
        if let Some(f) = q.projections.iter().find(|p| p.match_alias(r)).and_then(|p| p.field) {
            fields.push(f);
        }
    }
    fields.iter().all(|f| u.has_field(f.id))
}

fn calculations_depend_on_measures(q: &RQuery) -> bool {
    q.projections.iter().filter(|p| p.is_calculation()).any(|c| {
        !c.def.measure_refs.is_empty()
            || c.def.projection_refs.iter().any(|a| q.projections.iter().any(|p| p.match_alias(a) && p.is_measure()))
    })
}

/// Rails `Dag.set_subtrees_and_tables`.
pub fn set_subtrees<'a>(graph: &mut Graph<'a>, id: usize, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    let _t = crate::profile::span("resolve.subtrees");
    let node = &graph.nodes[id];
    let base = node.universe.clone().expect("query node has a universe");
    let measures = node.measures();
    let dimensions = node.dimensions();
    let sorted = sort_paths_for_max_hints_and_dims(universe, base, &measures, &dimensions, &q.hints);
    graph.nodes[id].subtrees = prune_to_minimum(sorted, &measures, &dimensions)?;
    Ok(())
}
