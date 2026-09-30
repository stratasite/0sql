//! Row-level security policies. A policy triggered by a projected field
//! either masks that field or filters rows, keyed on a context dimension
//! the principal is allowed to see values of. A fired policy that resolves
//! no allowed values denies unless it says `unresolved: allow`.

use crate::dag::{Entity, Graph, NodeId, NodeKind, RefKey, Reference};
use crate::universe::Subtree;
use crate::visitors::grain::{exclusion_dimension_ids, inclusion_dimension_ids};
use crate::{PlanError, Result};
use zsql_spec::SecurityContext;
use zsql_semantic::{ExclusionType, Field, FieldKind, FilterMode, PermissionSource, PermissionValueFrom, PolicyMode, SecurityPolicy, Universe, Unresolved};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, user: &SecurityContext) -> Result<()> {
    // Policies whose context dimension does not resolve are inert.
    let policies: Vec<(&SecurityPolicy, &Field)> = universe
        .policies()
        .iter()
        .filter_map(|p| universe.field_by_uid(&p.context_dimension_uid, Some(FieldKind::Dimension)).map(|d| (p, d)))
        .collect();
    if policies.is_empty() {
        return Ok(());
    }
    for id in graph.post_order() {
        let node = &graph.nodes[id];
        if node.removed || node.kind == NodeKind::Merge {
            continue;
        }
        let fields: Vec<&Field> = node.projections.iter().filter(|p| !p.is_calculation()).filter_map(|p| p.field()).collect();
        if !fields.iter().any(|f| policies.iter().any(|(p, _)| triggered_by(p, f))) {
            continue;
        }
        for (policy, ctx_dim) in &policies {
            let triggered: Vec<&Field> = fields.iter().copied().filter(|f| triggered_by(policy, f)).collect();
            if triggered.is_empty() || bypassed_by(policy, user) {
                continue;
            }
            // None: nothing resolved for this user. With `allow` the policy
            // is skipped; with `deny` it applies with no values, which the
            // SQL visitor renders as no rows or every value masked.
            let allowed = allowed_values_for(policy, user);
            if allowed.is_none() && policy.unresolved == Unresolved::Allow {
                continue;
            }
            let denied = allowed.is_none();
            match policy.mode {
                PolicyMode::MaskData => {
                    if !denied {
                        ensure_context_dim_ref(graph, id, universe, ctx_dim)?;
                    }
                    for f in triggered {
                        let masks = &mut graph.nodes[id].security_masks;
                        masks.retain(|(fid, ..)| *fid != f.id);
                        masks.push((f.id, ctx_dim, allowed.clone(), policy.mask_value.clone().unwrap_or_else(|| "#######".into())));
                    }
                }
                PolicyMode::FilterData => {
                    // A measure's own rule on the context dimension outranks a
                    // resolved filter, never a denial.
                    if denied {
                        graph.nodes[id].security_filters.push((ctx_dim, None));
                    } else if !skip_filter_for_node(&graph.nodes[id], universe, ctx_dim) {
                        ensure_context_dim_ref(graph, id, universe, ctx_dim)?;
                        graph.nodes[id].security_filters.push((ctx_dim, allowed));
                    }
                }
            }
        }
    }
    Ok(())
}

fn triggered_by(policy: &SecurityPolicy, field: &Field) -> bool {
    (!policy.trigger_tags.is_empty() && field.tags.iter().any(|t| policy.trigger_tags.contains(t)))
        || (!policy.trigger_field_uids.is_empty() && policy.trigger_field_uids.contains(&field.uid))
}

fn bypassed_by(policy: &SecurityPolicy, user: &SecurityContext) -> bool {
    (policy.bypass_system_admin && user.system_admin) || (policy.bypass_project_admin && user.project_admin)
}

/// Rails `SecurityPolicy#allowed_values_for`: None when unconstrained.
fn allowed_values_for(policy: &SecurityPolicy, user: &SecurityContext) -> Option<Vec<String>> {
    let values: Vec<String> = match policy.permission_source {
        PermissionSource::Groups => match policy.permission_value_from {
            PermissionValueFrom::FromName => user.groups.iter().filter_map(|g| g.name.clone()).collect(),
            PermissionValueFrom::FromTag => extract_tag_values(policy, user.groups.iter().flat_map(|g| g.tags.iter())),
            PermissionValueFrom::FromEmail => vec![],
        },
        PermissionSource::User => match policy.permission_value_from {
            PermissionValueFrom::FromEmail => user.email.iter().cloned().collect(),
            PermissionValueFrom::FromTag => extract_tag_values(policy, user.tags.iter()),
            PermissionValueFrom::FromName => vec![],
        },
    };
    let mut uniq: Vec<String> = Vec::new();
    for v in values {
        if !uniq.contains(&v) {
            uniq.push(v);
        }
    }
    if uniq.is_empty() {
        None
    } else {
        Some(uniq)
    }
}

fn extract_tag_values<'t>(policy: &SecurityPolicy, tags: impl Iterator<Item = &'t String>) -> Vec<String> {
    let prefix = format!("{}:", policy.permission_tag_key.clone().unwrap_or_default());
    tags.filter_map(|t| t.strip_prefix(&prefix).map(str::to_string)).collect()
}

/// Mirrors the reference visitor's path handling for the context dimension.
fn ensure_context_dim_ref<'a>(graph: &mut Graph<'a>, id: NodeId, universe: &'a Universe, ctx: &'a Field) -> Result<()> {
    if graph.nodes[id].field_refs.contains_key(&RefKey::Field(ctx.id)) {
        return Ok(());
    }
    let unreachable = || {
        PlanError::SecurityPolicy(format!(
            "Security policy context dimension '{}' is not reachable in the universe for this query. Cannot safely enforce security policy.",
            ctx.name
        ))
    };
    let base = graph.nodes[id].universe.clone().ok_or_else(unreachable)?;
    let path = base.path_for(ctx.id).ok_or_else(unreachable)?;
    {
        let node = &mut graph.nodes[id];
        if !node.subtrees.iter().any(|t| t.paths.iter().any(|p| p.field_id == ctx.id)) {
            match node.subtrees.iter_mut().find(|t| t.join_path == path.joins) {
                Some(t) => t.push(path),
                None => node.subtrees.push(Subtree::new(universe, base.clone(), path.joins.clone(), vec![path])),
            }
        }
    }
    let exp = if let Some(blend) = path.blending_field_id.filter(|_| path.extended_blend_group.is_some()) {
        universe.expression_for(path.target_table_id, blend)
    } else {
        universe.expression_for(path.target_table_id, ctx.id)
    }
    .ok_or_else(unreachable)?;
    if !exp.dimension_ref_ids.is_empty() || !exp.measure_ref_ids.is_empty() {
        return Err(PlanError::SecurityPolicy(format!(
            "Security policy context dimension '{}' has a composite expression and cannot be used as a security context dimension.",
            ctx.name
        )));
    }
    let table = universe.table(exp.table_id).expect("expression table");
    graph.nodes[id].field_refs.insert(RefKey::Field(ctx.id), Reference::Table { entity: Entity::Field(ctx), expression: exp, table, greedy: vec![] });
    Ok(())
}

/// A measure's own exclusion or inclusion rule on the context dimension
/// takes precedence over the security filter.
fn skip_filter_for_node(node: &crate::dag::Node, universe: &Universe, ctx: &Field) -> bool {
    node.measures().iter().any(|m| {
        if m.exclusion_type == Some(ExclusionType::ExcludeAll) {
            return true;
        }
        let rule = m.exclusions.iter().find(|r| exclusion_dimension_ids(universe, r).contains(&ctx.id));
        match m.exclusion_type {
            Some(ExclusionType::Exclude) => rule.map(|r| r.filter == FilterMode::Ignore).unwrap_or(false),
            Some(ExclusionType::ExcludeAllExcept) => rule.is_none() && !m.exclusions.first().map(|r| r.filter == FilterMode::Apply).unwrap_or(false),
            _ => match &m.inclusions {
                Some(inc) => inclusion_dimension_ids(universe, inc).contains(&ctx.id) && inc.filter == FilterMode::Ignore,
                None => false,
            },
        }
    })
}
