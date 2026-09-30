//! Rails `ReferenceVisitor`: binds every component of every node to where
//! its SQL comes from (a joined table, an input node's column, or a
//! composition of those).

use crate::dag::{Entity, Graph, MultiExpr, NodeId, NodeKind, RefKey, Reference};
use crate::model::{RProjection, RQuery};
use std::rc::Rc;
use crate::universe::Subtree;
use crate::{PlanError, Result};
use zsql_semantic::{Expression, Field, Universe};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, _q: &RQuery<'a>) -> Result<()> {
    for id in graph.post_order() {
        match graph.nodes[id].kind {
            NodeKind::Query | NodeKind::Aggregation => visit_query_node(graph, id, universe)?,
            NodeKind::Merge => visit_merge_node(graph, id, universe)?,
        }
    }
    Ok(())
}

fn visit_query_node<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe) -> Result<()> {
    let fields: Vec<&Field> = {
        let n = &graph.nodes[id];
        n.dimensions().into_iter().chain(n.measures()).collect()
    };

    for field in fields {
        if graph.nodes[id].field_refs.contains_key(&RefKey::Field(field.id)) {
            continue;
        }
        if only_carried(graph, id, field) {
            continue;
        }
        let r = create_reference(graph, id, universe, field)?;
        graph.nodes[id].field_refs.insert(RefKey::Field(field.id), r);
    }
    // Calculations: a formula over the node's own fields and projections.
    let calcs: Vec<Rc<RProjection<'a>>> = graph.nodes[id].projections.iter().filter(|p| p.is_calculation()).map(|p| p.proj.clone()).collect();
    for calc in calcs {
        if graph.nodes[id].field_refs.contains_key(&RefKey::Proj(calc.pid)) {
            continue;
        }
        let mut sources = Vec::new();
        for rf in calc.field_refs.clone() {
            sources.push(create_reference(graph, id, universe, rf)?);
        }
        append_projection_references(graph, id, &calc, &mut sources)?;
        graph.nodes[id].field_refs.insert(
            RefKey::Proj(calc.pid),
            Reference::Multi { entity: Entity::Proj(calc.clone()), expression: Some(MultiExpr::Calc(calc.clone())), sources },
        );
    }
    if graph.dialect.greedy_apply_date_filters() {
        let date_fields: Vec<&Field> = graph.nodes[id].filters.iter().filter(|f| f.is_date()).map(|f| f.field()).collect();
        for field in date_fields {
            create_greedy_filter_ref(graph, id, universe, field);
        }
    }
    Ok(())
}

fn only_carried(graph: &Graph, id: NodeId, field: &Field) -> bool {
    let n = &graph.nodes[id];
    let projs: Vec<_> = n.projections.iter().filter(|p| !p.is_calculation() && p.field().map(|f| f.id == field.id).unwrap_or(false)).collect();
    !projs.is_empty() && projs.iter().all(|p| p.proj.is_carried()) && !n.filters.iter().any(|f| f.field().id == field.id)
}

/// Rails `create_reference`: the field's expression on the cheapest path
/// of the subtree holding it, adding the path to the node when none does.
pub fn create_reference<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, field: &'a Field) -> Result<Reference<'a>> {
    let path = {
        let node = &graph.nodes[id];
        match node.subtrees.iter().find(|t| t.paths.iter().any(|p| p.field_id == field.id)) {
            Some(t) => t.path_for(field.id).expect("subtree holds field"),
            None => {
                let base = node.universe.clone().expect("query node universe");
                let path = base.path_for(field.id).ok_or_else(|| {
                    PlanError::Resolution(format!("Could not find path for {} with id: {}.", crate::model::field_to_s(field), field.id))
                })?;
                let node = &mut graph.nodes[id];
                match node.subtrees.iter_mut().find(|t| t.join_path == path.joins) {
                    Some(t) => t.push(path),
                    None => node.subtrees.push(Subtree::new(universe, base, path.joins.clone(), vec![path])),
                }
                path
            }
        }
    };
    let exp = if let Some(blend) = path.blending_field_id.filter(|_| field.is_dimension() && path.extended_blend_group.is_some()) {
        universe.expression_for(path.target_table_id, blend)
    } else {
        universe.expression_for(path.target_table_id, field.id)
    };
    let exp = exp.ok_or_else(|| {
        PlanError::Resolution(format!("Could not find expression for {} in the the target table {}.", crate::model::field_to_s(field), path.target_table_id))
    })?;
    let table = universe.table(exp.table_id).expect("expression table");
    if !exp.dimension_ref_ids.is_empty() || !exp.measure_ref_ids.is_empty() {
        create_local_multi_reference(graph, id, universe, field, exp)
    } else {
        Ok(Reference::Table { entity: Entity::Field(field), expression: exp, table, greedy: vec![] })
    }
}

/// Rails `append_projection_references`: `[Alias]` references resolve to a
/// projection of this node, or of an input when none here matches.
fn append_projection_references<'a>(graph: &mut Graph<'a>, id: NodeId, calc: &Rc<RProjection<'a>>, sources: &mut Vec<Reference<'a>>) -> Result<()> {
    for alias in calc.def.projection_refs.clone() {
        let local = graph.nodes[id].projections.iter().find(|p| p.proj.match_alias(&alias)).map(|p| p.proj.clone());
        let local = local.filter(|p| p.pid != calc.pid);
        if let Some(pj) = local {
            sources.push(Reference::Proj { proj: pj });
            continue;
        }
        let mut found = None;
        for input in graph.nodes[id].inputs.clone() {
            if let Some(pj) = graph.nodes[input].projections.iter().find(|p| p.proj.match_alias(&alias)).map(|p| p.proj.clone()) {
                found = Some((pj, input));
                break;
            }
        }
        let Some((pj, input)) = found else {
            return Err(PlanError::Resolution(format!("Could not find projection with alias: {alias}.")));
        };
        sources.push(Reference::Node { entity: Entity::Proj(pj), node: input });
    }
    Ok(())
}

/// Rails `build_out_multi_source_reference`: a merge-level formula with no
/// sources yet reads each referenced field from the input that outputs it.
fn build_out_multi_source_reference<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, key: RefKey) -> Result<()> {
    let (expr, entity) = match graph.nodes[id].field_refs.get(&key) {
        Some(Reference::Multi { expression: Some(e), sources, entity }) if sources.is_empty() => (e.clone(), entity.clone()),
        _ => return Ok(()),
    };
    let fields: Vec<&'a Field> = match &expr {
        MultiExpr::Semantic(e) => e.dimension_ref_ids.iter().chain(e.measure_ref_ids.iter()).filter_map(|i| universe.field(*i)).collect(),
        MultiExpr::Calc(c) => c.field_refs.clone(),
    };
    let inputs = graph.nodes[id].inputs.clone();
    let full = graph.nodes[id].join_type == "full";
    let mut sources = Vec::new();
    for field in fields {
        let source_refs: Vec<Reference<'a>> = inputs
            .iter()
            .filter_map(|i| {
                graph.nodes[*i]
                    .projections
                    .iter()
                    .find(|p| p.field().map(|f| f.id == field.id).unwrap_or(false))
                    .map(|p| Reference::Node { entity: Entity::Proj(p.proj.clone()), node: *i })
            })
            .collect();
        let Some(first) = source_refs.first().cloned() else { continue };
        if field.is_dimension() && source_refs.len() > 1 && full {
            let first_entity = first.entity().cloned().expect("node ref entity");
            sources.push(Reference::Multi { entity: first_entity, expression: None, sources: source_refs });
        } else {
            sources.push(first);
        }
    }
    if let MultiExpr::Calc(c) = &expr {
        append_projection_references(graph, id, c, &mut sources)?;
    }
    graph.nodes[id].field_refs.insert(key, Reference::Multi { entity, expression: Some(expr), sources });
    Ok(())
}

/// A compound expression: its own columns on the local table plus a
/// reference per field it names.
fn create_local_multi_reference<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, field: &'a Field, exp: &'a Expression) -> Result<Reference<'a>> {
    let mut sources = Vec::new();
    if !exp.column_refs.is_empty() {
        let table = universe.table(exp.table_id).expect("expression table");
        sources.push(Reference::Table { entity: Entity::Field(field), expression: exp, table, greedy: vec![] });
    }
    for rid in exp.dimension_ref_ids.iter().chain(exp.measure_ref_ids.iter()) {
        let rf = universe.field(*rid).ok_or_else(|| PlanError::Resolution(format!("referenced field {rid} not found")))?;
        sources.push(create_reference(graph, id, universe, rf)?);
    }
    Ok(Reference::Multi { entity: Entity::Field(field), expression: Some(MultiExpr::Semantic(exp)), sources })
}

/// Expressions of a date field on the node's other joined tables, so the
/// same filter can be applied on every side of the join.
fn create_greedy_filter_ref<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, field: &'a Field) {
    let node = &graph.nodes[id];
    let (ref_table_id, _) = match node.field_refs.get(&RefKey::Field(field.id)) {
        Some(Reference::Table { table, .. }) => (table.id, ()),
        _ => return,
    };
    let mut table_ids: Vec<i64> = Vec::new();
    for t in node.subtrees.iter().filter(|t| t.paths.iter().any(|p| p.field_id == field.id)) {
        for p in t.paths_for(field.id) {
            if !table_ids.contains(&p.target_table_id) {
                table_ids.push(p.target_table_id);
            }
        }
    }
    table_ids.retain(|t| *t != ref_table_id);
    let extra: Vec<&'a Expression> = universe
        .snapshot()
        .expressions
        .iter()
        .filter(|e| e.field_id == field.id && table_ids.contains(&e.table_id))
        .collect();
    if let Some(Reference::Table { greedy, .. }) = graph.nodes[id].field_refs.get_mut(&RefKey::Field(field.id)) {
        for e in extra {
            if !greedy.iter().any(|g| g.id == e.id) {
                greedy.push(e);
            }
        }
    }
}

/// Rails `set_field_reference_by_input_nodes` for a merge.
fn visit_merge_node<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe) -> Result<()> {
    let inputs = graph.nodes[id].inputs.clone();
    let projections: Vec<_> = graph.nodes[id].projections.iter().map(|p| p.proj.clone()).collect();

    // Calculations first: one an input already computes is read as that
    // input's column; the rest are evaluated here over the inputs' columns.
    for calc in projections.iter().filter(|p| p.is_calculation()) {
        if graph.nodes[id].field_refs.contains_key(&RefKey::Proj(calc.pid)) {
            continue;
        }
        let sources: Vec<NodeId> = inputs.iter().copied().filter(|i| graph.nodes[*i].has_proj(calc.pid)).collect();
        let r = if sources.is_empty() {
            Reference::Multi { entity: Entity::Proj(calc.clone()), expression: Some(MultiExpr::Calc(calc.clone())), sources: vec![] }
        } else {
            Reference::Multi {
                entity: Entity::Proj(calc.clone()),
                expression: None,
                sources: sources.iter().map(|n| Reference::Node { entity: Entity::Proj(calc.clone()), node: *n }).collect(),
            }
        };
        graph.nodes[id].field_refs.insert(RefKey::Proj(calc.pid), r);
    }

    // Unconstrained copies bind first, so the field-keyed fallback always
    // belongs to an unsegmented copy when one exists.
    let (plain, constrained): (Vec<_>, Vec<_>) = projections.into_iter().partition(|p| !(p.is_measure() && !crate::visitors::segmentation::component_links(p).is_empty()));
    let projections: Vec<_> = plain.into_iter().chain(constrained).collect();
    for p in projections {
        build_out_multi_source_reference(graph, id, universe, RefKey::Proj(p.pid))?;
        if let Some(f) = p.field_id() {
            if !graph.nodes[id].field_refs.contains_key(&RefKey::Proj(p.pid)) {
                build_out_multi_source_reference(graph, id, universe, RefKey::Field(f))?;
            }
        }
        if graph.nodes[id].field_refs.contains_key(&RefKey::Proj(p.pid)) {
            continue;
        }
        let field_id = p.field_id();
        if p.is_dimension() {
            let sources: Vec<NodeId> = inputs.iter().copied().filter(|i| graph.nodes[*i].has_proj(p.pid)).collect();
            if sources.is_empty() && graph.nodes[id].get_ref(p.pid, field_id).is_some() {
                continue;
            }
            let entity = Entity::Proj(p.clone());
            let r = Reference::Multi {
                entity: p.field.map(Entity::Field).unwrap_or(entity.clone()),
                expression: None,
                sources: sources.iter().map(|n| Reference::Node { entity: entity.clone(), node: *n }).collect(),
            };
            let node = &mut graph.nodes[id];
            node.field_refs.insert(RefKey::Proj(p.pid), r.clone());
            if let Some(f) = field_id {
                node.field_refs.entry(RefKey::Field(f)).or_insert(r);
            }
        } else {
            let source = inputs.iter().copied().find(|i| graph.nodes[*i].has_proj(p.pid));
            let Some(source) = source else {
                if graph.nodes[id].get_ref(p.pid, field_id).is_some() {
                    continue;
                }
                return Err(PlanError::Resolution(format!("No input materializes {}", p.to_s())));
            };
            let r = Reference::Node { entity: Entity::Proj(p.clone()), node: source };
            let node = &mut graph.nodes[id];
            node.field_refs.insert(RefKey::Proj(p.pid), r.clone());
            if let Some(f) = field_id {
                node.field_refs.entry(RefKey::Field(f)).or_insert(r);
            }
        }
    }
    let filters: Vec<_> = graph.nodes[id].filters.iter().map(|f| f.filter.clone()).collect();
    for f in filters {
        build_out_multi_source_reference(graph, id, universe, RefKey::Field(f.field.id))?;
        if graph.nodes[id].field_refs.contains_key(&RefKey::Field(f.field.id)) {
            continue;
        }
        let source = inputs.iter().copied().find(|i| graph.nodes[*i].projections.iter().any(|p| p.field().map(|x| x.id == f.field.id).unwrap_or(false)));
        let Some(source) = source else {
            return Err(PlanError::Resolution(format!(
                "The filter on {} cannot be applied at the final pass: no input outputs that column. Filter a projected copy of the field, or remove the filter.",
                f.field.name
            )));
        };
        graph.nodes[id].field_refs.insert(RefKey::Field(f.field.id), Reference::Node { entity: Entity::Field(f.field), node: source });
    }
    Ok(())
}
