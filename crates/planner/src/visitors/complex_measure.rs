//! Rails `ComplexMeasureVisitor`: a measure whose expression names a measure
//! outside its node's universe is lifted to a merge, where its components
//! arrive as plain columns from sub-graphs planned for them.

use crate::dag::{Entity, Graph, MultiExpr, Node, NodeId, NodeKind, RefKey, Reference};
use crate::resolved::{FilledTree, ResolvedFilter, ResolvedProjection, ResolvedQuery};
use crate::resolver::{self, Options};
use crate::universe::Universes;
use crate::{Error, Result};
use std::rc::Rc;
use zsql_dialect::Dialects;
use zsql_semantic::{Field, SemanticModel};

/// A compound component of a node and the measures it needs from elsewhere.
enum Component<'a> {
    Proj(Rc<ResolvedProjection<'a>>),
    Filter(Rc<ResolvedFilter<'a>>),
}

impl<'a> Component<'a> {
    fn field(&self) -> &'a Field {
        match self {
            Component::Proj(p) => p.field.expect("measure projection field"),
            Component::Filter(f) => f.field,
        }
    }
}

pub fn run<'a>(
    graph: &mut Graph<'a>,
    semantic: &'a SemanticModel,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &ResolvedQuery<'a>,
) -> Result<()> {
    let mut registry: HashMap<i64, Signature> = HashMap::new();
    for id in graph.post_order() {
        visit(graph, id, semantic, universes, dialects, q, &mut registry)?;
    }
    Ok(())
}

use crate::visitors::segmentation::{component_links, node_signature, signature, Signature};
use std::collections::HashMap;

/// One field must never aggregate under two segment signatures.
fn register_reference_signatures<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    semantic: &'a SemanticModel,
    q: &ResolvedQuery<'a>,
    component: &Component<'a>,
    registry: &mut HashMap<i64, Signature>,
) -> Result<()> {
    let sig = match component {
        Component::Proj(p) => signature(p),
        Component::Filter(_) => vec![],
    };
    let exp = graph.nodes[id].expression(semantic, component.field())?;
    for mid in &exp.measure_ref_ids {
        let Some(field) = semantic.field(*mid) else { continue };
        if let Some(prev) = registry.get(&field.id) {
            if *prev != sig {
                return Err(conflict(component, field, q));
            }
        }
        registry.insert(field.id, sig.clone());
        if let Some(plain) =
            q.projections.iter().find(|p| !p.is_calculation() && p.persisted && p.field.map(|f| f.id == field.id).unwrap_or(false))
        {
            if signature(plain) != sig {
                return Err(conflict(component, field, q));
            }
        }
    }
    Ok(())
}

fn conflict<'a>(component: &Component<'a>, field: &Field, q: &ResolvedQuery<'a>) -> Error {
    let links = match component {
        Component::Proj(p) => component_links(p),
        Component::Filter(_) => vec![],
    };
    let segments: Vec<String> = links.iter().filter_map(|(s, _)| q.segment(*s)).map(|s| s.name()).collect();
    let subject = if segments.is_empty() {
        "A segment conflicts with".to_string()
    } else {
        format!("Segment {} cannot be applied to", segments.join(", "))
    };
    Error::Resolution(format!(
        "{} {}: it references {}, which aggregates under a different segment. Apply the same segment to every use of {}, or remove the reference.",
        subject,
        component.field().name,
        field.name,
        field.name
    ))
}

fn visit<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    semantic: &'a SemanticModel,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &ResolvedQuery<'a>,
    registry: &mut HashMap<i64, Signature>,
) -> Result<()> {
    if graph.nodes[id].removed || graph.nodes[id].kind == NodeKind::Merge {
        return Ok(());
    }
    let non_local = non_local_measure_references(graph, id, semantic)?;
    if non_local.is_empty() {
        return Ok(());
    }
    for (component, measures) in non_local {
        register_reference_signatures(graph, id, semantic, q, &component, registry)?;
        let merge = setup_merge_node(graph, id, semantic, &component, q)?;
        let segmented = matches!(&component, Component::Proj(p) if !component_links(p).is_empty());
        if segmented {
            let Component::Proj(p) = &component else { unreachable!() };
            create_segmented_component_graph(graph, id, merge, semantic, universes, dialects, q, p)?;
            graph.nodes[id].delete_projection(p.pid);
        } else {
            project_local_reference(graph, id, semantic, &component)?;
            match &component {
                Component::Proj(p) => graph.nodes[id].delete_projection(p.pid),
                Component::Filter(f) => graph.nodes[id].delete_filter(f.fid),
            }
            create_non_local_measure_graph(graph, id, merge, semantic, universes, dialects, q, &component, measures)?;
        }
    }
    if !graph.nodes[id].projections.iter().any(|p| p.is_measure()) {
        graph.destroy_node(id);
    }
    Ok(())
}

/// Measure components whose expression names measures the node's universe lacks.
fn non_local_measure_references<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    semantic: &'a SemanticModel,
) -> Result<Vec<(Component<'a>, Vec<&'a Field>)>> {
    let base = graph.nodes[id].universe.clone().expect("query node universe");
    let mut components: Vec<Component<'a>> = Vec::new();
    for p in &graph.nodes[id].projections {
        if p.is_measure() && !p.is_calculation() {
            components.push(Component::Proj(p.proj.clone()));
        }
    }
    for f in &graph.nodes[id].filters {
        if f.is_measure() {
            components.push(Component::Filter(f.filter.clone()));
        }
    }
    let mut out = Vec::new();
    for c in components {
        let exp = graph.nodes[id].expression(semantic, c.field())?;
        let non_local: Vec<&Field> =
            exp.measure_ref_ids.iter().filter(|m| !base.has_measure(**m)).filter_map(|m| semantic.field(*m)).collect();
        if !exp.measure_ref_ids.is_empty() && !non_local.is_empty() {
            out.push((c, non_local));
        }
    }
    Ok(out)
}

/// The merge the lifted measure computes in: the root when it already is
/// one, else a new merge fed by this node. Seeds its reference so the
/// reference visitor wires the components as node columns.
fn setup_merge_node<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    semantic: &'a SemanticModel,
    component: &Component<'a>,
    q: &ResolvedQuery<'a>,
) -> Result<NodeId> {
    let root = graph.root();
    let merge = if graph.nodes[root].is_merge() {
        root
    } else {
        let filters = q.filters.iter().filter(|f| f.is_measure() && graph.dialect.final_measure_filter()).cloned().collect();
        let m = Node::merge(q.all_projections(), filters, graph.dialect.final_pass_measure_join_type().to_string());
        let mid = graph.add_node(m);
        graph.add_edge(id, mid);
        mid
    };
    if let Component::Filter(f) = component {
        let same = graph.nodes[merge].filters.iter().any(|x| {
            x.field().id == f.field.id
                && x.predicate == f.def.predicate
                && x.filter_value == f.def.filter_value
                && x.filter_value_end == f.def.filter_value_end
        });
        if !same {
            graph.nodes[merge].filters.push(crate::dag::Filtered::new(f.clone()));
        }
    }
    let exp = graph.nodes[id].expression(semantic, component.field())?;
    let key = match component {
        Component::Filter(f) => RefKey::Field(f.field.id),
        Component::Proj(p) if !p.is_calculation() => RefKey::Field(p.field.expect("field").id),
        Component::Proj(p) => RefKey::Proj(p.pid),
    };
    graph.nodes[merge].field_refs.insert(
        key,
        Reference::Multi { entity: Entity::Field(component.field()), expression: Some(MultiExpr::Semantic(exp)), sources: vec![] },
    );
    Ok(merge)
}

/// Referenced measures this universe holds are projected here so the merge can read them.
fn project_local_reference<'a>(graph: &mut Graph<'a>, id: NodeId, semantic: &'a SemanticModel, component: &Component<'a>) -> Result<()> {
    let exp = graph.nodes[id].expression(semantic, component.field())?;
    let base = graph.nodes[id].universe.clone().expect("universe");
    for mid in &exp.measure_ref_ids {
        if !has_clean_projected_measure(&graph.nodes[id], *mid) && base.has_measure(*mid) {
            let m = semantic.field(*mid).expect("measure");
            let mut rp = ResolvedProjection::for_field(m);
            rp.def.ordinal = Some(graph.nodes[id].projections.len() as i64);
            graph.nodes[id].add_projection(Rc::new(rp));
        }
    }
    Ok(())
}

fn has_clean_projected_measure(node: &Node, measure_id: i64) -> bool {
    node.projections.iter().any(|p| p.is_measure() && p.field().map(|f| f.id == measure_id).unwrap_or(false) && p.decorators().is_empty())
}

#[allow(clippy::too_many_arguments)]
fn create_non_local_measure_graph<'a>(
    graph: &mut Graph<'a>,
    _id: NodeId,
    merge: NodeId,
    semantic: &'a SemanticModel,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &ResolvedQuery<'a>,
    component: &Component<'a>,
    measures: Vec<&'a Field>,
) -> Result<()> {
    let mut stack: Vec<&'a Field> = Vec::new();
    for m in measures {
        if let Some(agg) = build_from_existing_agg(graph, m) {
            graph.add_edge(agg, merge);
        } else {
            stack.push(m);
        }
    }
    if stack.is_empty() {
        return Ok(());
    }

    // The sub-query: the main query's dimensions (same records) plus the
    // missing measures, with the main query's dimension filters and any
    // filter on the component's own field lent to it.
    let mut projections: Vec<Rc<ResolvedProjection<'a>>> = q.all_projections().into_iter().filter(|p| p.is_dimension()).collect();
    for m in stack {
        let mut rp = ResolvedProjection::for_field(m);
        rp.def.ordinal = Some(projections.len() as i64);
        projections.push(Rc::new(rp));
    }
    let filters: Vec<Rc<ResolvedFilter<'a>>> = q
        .filters
        .iter()
        .filter(|f| f.is_dimension())
        .chain(q.filters.iter().filter(|f| f.field.id == component.field().id))
        .cloned()
        .collect();
    let name = format!("{}-subgraph", q.def.name.clone().unwrap_or_default());
    let sub = ResolvedQuery {
        def: zsql_spec::Query::planner_sub(name),
        projections,
        filters: filters.clone(),
        hints: vec![],
        tree: FilledTree::Group("and".into(), filters.iter().map(|f| FilledTree::Leaf(f.fid)).collect()),
        extras: Default::default(),
        segments: vec![],
    };
    let opts = Options { auto_level_dims: true, datasource: Some(graph.datasource), ..Default::default() };
    let mut sub_graph = resolver::resolve_with(semantic, universes, dialects, &sub, &opts)?;
    // Sub-plans carry no db settings, but render inside the main graph.
    sub_graph.dialect = graph.dialect.clone();
    run(&mut sub_graph, semantic, universes, dialects, &sub)?;
    let sub_root = graph.import(sub_graph);
    graph.add_edge(sub_root, merge);
    Ok(())
}

/// A segmented compound's components aggregate within the segment in a
/// dedicated sub-graph whose projections carry the component as source.
#[allow(clippy::too_many_arguments)]
fn create_segmented_component_graph<'a>(
    graph: &mut Graph<'a>,
    id: NodeId,
    merge: NodeId,
    semantic: &'a SemanticModel,
    universes: &Universes<'a>,
    dialects: &Dialects,
    q: &ResolvedQuery<'a>,
    component: &Rc<ResolvedProjection<'a>>,
) -> Result<()> {
    let exp = graph.nodes[id].expression(semantic, component.field.expect("measure"))?;
    let measures: Vec<&Field> = exp.measure_ref_ids.iter().filter_map(|m| semantic.field(*m)).collect();
    let mut projections: Vec<Rc<ResolvedProjection<'a>>> = q.all_projections().into_iter().filter(|p| p.is_dimension()).collect();
    for m in &measures {
        let mut rp = ResolvedProjection::for_field(m);
        rp.def.ordinal = Some(projections.len() as i64);
        rp.set_source(Some(component.clone()));
        projections.push(Rc::new(rp));
    }
    let filters: Vec<Rc<ResolvedFilter<'a>>> = q
        .filters
        .iter()
        .filter(|f| f.is_dimension())
        .chain(q.filters.iter().filter(|f| f.field.id == component.field.expect("measure").id))
        .cloned()
        .collect();
    let sub = ResolvedQuery {
        def: zsql_spec::Query::planner_sub(format!("{}-subgraph", q.def.name.clone().unwrap_or_default())),
        projections,
        filters: filters.clone(),
        hints: vec![],
        tree: FilledTree::Group("and".into(), filters.iter().map(|f| FilledTree::Leaf(f.fid)).collect()),
        extras: Default::default(),
        segments: vec![],
    };
    let mut required: Vec<&Field> = q.query_level_key_dimensions();
    for (sid, _) in component_links(component) {
        if let Some(s) = q.segment(sid) {
            for k in s.key_dimensions() {
                if !required.iter().any(|x| x.id == k.id) {
                    required.push(k);
                }
            }
        }
    }
    let opts = Options { auto_level_dims: true, datasource: Some(graph.datasource), required_dims: required.clone(), ..Default::default() };
    let mut sub_graph = match resolver::resolve_with(semantic, universes, dialects, &sub, &opts) {
        Ok(g) => g,
        Err(Error::Resolution(m)) => {
            let unpinned = Options { auto_level_dims: true, datasource: None, required_dims: required, ..Default::default() };
            return match resolver::resolve_with(semantic, universes, dialects, &sub, &unpinned) {
                Ok(_) => Err(Error::SegmentDatasource(m)),
                Err(_) => Err(Error::Resolution(m)),
            };
        }
        Err(e) => return Err(e),
    };
    sub_graph.dialect = graph.dialect.clone();
    run(&mut sub_graph, semantic, universes, dialects, &sub)?;
    let sub_root = graph.import(sub_graph);
    graph.add_edge(sub_root, merge);
    Ok(())
}

/// An existing node that already aggregates the measure, or an aggregation
/// whose universe could: the measure is projected there instead of
/// planning a new sub-graph. Never a segment-constrained node.
fn build_from_existing_agg<'a>(graph: &mut Graph<'a>, measure: &'a Field) -> Option<NodeId> {
    let root = graph.root();
    let found = graph
        .live()
        .find(|n| {
            !(n.id == root && n.is_merge())
                && node_signature(n).is_empty()
                && (n.projections.iter().any(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false) && p.decorators().is_empty())
                    || (n.is_agg() && n.universe.as_ref().map(|u| u.has_measure(measure.id)).unwrap_or(false)))
        })?
        .id;
    let node = &mut graph.nodes[found];
    if node.is_agg()
        && !node.projections.iter().any(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false) && p.decorators().is_empty())
    {
        let mut rp = ResolvedProjection::for_field(measure);
        rp.def.ordinal = Some(node.projections.len() as i64);
        node.add_projection(Rc::new(rp));
    }
    Some(found)
}
