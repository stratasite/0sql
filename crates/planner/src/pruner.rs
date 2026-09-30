//! Rails `Planner::Universe::Pruner`: ranking candidate universes or
//! subtrees for a field set, and cutting the set to the minimum that covers it.

use crate::model::RFilter;
use crate::universe::{Base, Model, Subtree};
use crate::{PlanError, Result};
use std::collections::HashSet;
use std::rc::Rc;
use std::sync::Arc;
use zsql_semantic::{Field, Id, Predicate, Table, Universe};

/// Sort ascending on: hints hit, dimensions covered, partition score, tier,
/// measures covered, cost, local fields, then the model's string for stability.
pub fn sort_for_max_hints_and_dims<M: Model>(
    mut models: Vec<M>,
    measures: &[&Field],
    dimensions: &[&Field],
    hints: &[&Table],
    filters: &[Rc<RFilter>],
    universe: &Universe,
) -> Vec<M> {
    let measure_ids: HashSet<Id> = measures.iter().map(|f| f.id).collect();
    let dimension_ids: HashSet<Id> = dimensions.iter().map(|f| f.id).collect();
    let hint_ids: HashSet<Id> = hints.iter().map(|t| t.id).collect();
    let all_field_ids: HashSet<Id> = measure_ids.union(&dimension_ids).copied().collect();

    let key = |m: &M| {
        let hint_count = m.hint_count(&hint_ids) as i64;
        let dim_count = m.dimension_count(&dimension_ids) as i64;
        let measure_count = m.measure_count(&measure_ids) as i64;
        let local = all_field_ids.intersection(m.local_field_ids()).count() as i64;
        (
            -hint_count,
            -dim_count,
            -partition_score(m.root(), filters, universe),
            m.tier(),
            -measure_count,
            m.cost(),
            -local,
            m.to_s(),
        )
    };
    models.sort_by_cached_key(key);
    models
}

/// Rails pops measures off a stack, so it walks them last to first; each
/// picks the first model holding it. Dimensions follow when given. Then
/// unique, ordered by cost and string.
pub fn prune_to_minimum<M: Model + Clone>(models: Vec<M>, measures: &[&Field], dimensions: &[&Field]) -> Result<Vec<M>> {
    let mut pruned: Vec<M> = Vec::new();
    if measures.is_empty() {
        let first = models.first().cloned().ok_or_else(|| PlanError::Resolution("Could not find a universe for the query.".into()))?;
        pruned.push(first);
    } else {
        for m in measures.iter().rev() {
            let u = models.iter().find(|u| u.has_field(m.id)).cloned().ok_or_else(|| {
                PlanError::Resolution(format!("Could not find Universe for measure: {}, id: {}", m.name, m.id))
            })?;
            pruned.push(u);
        }
        for d in dimensions.iter().rev() {
            let u = models.iter().find(|u| u.has_field(d.id)).cloned().ok_or_else(|| {
                PlanError::Resolution(format!("Could not find Universe for dimension: {}, id: {}", d.name, d.id))
            })?;
            pruned.push(u);
        }
    }
    let mut seen = HashSet::new();
    pruned.retain(|m| seen.insert(m.to_s()));
    pruned.sort_by_cached_key(|m| (m.cost(), m.to_s()));
    Ok(pruned)
}

/// Rails `sort_paths_for_max_hints_and_dims`: the universe's paths grouped
/// by join path, each subtree keeping only the requested fields' paths.
pub fn sort_paths_for_max_hints_and_dims<'a>(
    universe: &'a Universe,
    base: Arc<Base<'a>>,
    measures: &[&Field],
    dimensions: &[&Field],
    hints: &[&Table],
) -> Vec<Subtree<'a>> {
    let field_ids: HashSet<Id> = measures.iter().chain(dimensions.iter()).map(|f| f.id).collect();
    let subtrees: Vec<Subtree<'a>> = base
        .groups()
        .iter()
        .map(|g| {
            let kept = g.paths.iter().copied().filter(|p| field_ids.contains(&p.field_id)).collect();
            Subtree::from_group(base.clone(), g, kept)
        })
        .collect();
    sort_for_max_hints_and_dims(subtrees, measures, dimensions, hints, &[], universe)
}

/// +1 per dimension filter inside a root-table partition, -1 outside, 0 when unrelated.
pub fn partition_score(root: &Table, filters: &[Rc<RFilter>], universe: &Universe) -> i64 {
    if filters.is_empty() {
        return 0;
    }
    let parts: Vec<_> = universe.snapshot().partitions.iter().filter(|p| p.table_id == root.id).collect();
    if parts.is_empty() {
        return 0;
    }
    let mut score = 0;
    for part in parts {
        for f in filters {
            if f.field.id == part.dimension_id {
                score += if within_partition(part, f) { 1 } else { -1 };
            }
        }
    }
    score
}

/// Rails `Partition#within_partition_range?`, for numeric and date partitions.
fn within_partition(part: &zsql_semantic::Partition, f: &RFilter) -> bool {
    use crate::sql::dates::parse_value;
    let numeric = f.field.data_type.map(|d| {
        matches!(d, zsql_semantic::DataType::Integer | zsql_semantic::DataType::Decimal | zsql_semantic::DataType::Bigint)
    });
    let to_num = |v: &Option<String>| -> Option<f64> {
        let v = v.as_deref()?;
        if numeric == Some(true) {
            v.trim().parse::<f64>().ok()
        } else {
            parse_value(v, false).map(|d| d.and_utc().timestamp() as f64)
        }
    };
    let (Some(lo_raw), hi_raw) = (to_num(&part.filter_value), to_num(&part.filter_value_end)) else { return false };
    let day = if numeric == Some(true) { 1.0 } else { 86400.0 };
    let (lo, hi) = match part.predicate {
        Predicate::Between => (lo_raw, hi_raw.unwrap_or(f64::INFINITY)),
        Predicate::GreaterThan => (lo_raw + day, f64::INFINITY),
        Predicate::GreaterThanOrEqualTo => (lo_raw, f64::INFINITY),
        Predicate::LessThan | Predicate::LessThanOrEqualTo => (f64::NEG_INFINITY, lo_raw),
        _ => return false,
    };
    let Some(fv) = to_num(&f.def.filter_value) else { return false };
    let cover = |x: f64| x >= lo && x <= hi;
    match f.def.predicate {
        Predicate::Between => cover(fv) && to_num(&f.def.filter_value_end).map(cover).unwrap_or(false),
        Predicate::GreaterThan => (fv + day) >= lo && hi == f64::INFINITY,
        Predicate::GreaterThanOrEqualTo => fv >= lo && hi == f64::INFINITY,
        Predicate::LessThan | Predicate::LessThanOrEqualTo => lo == f64::NEG_INFINITY && fv <= hi,
        Predicate::Equals => cover(fv),
        Predicate::DoesNotEqual => !cover(fv),
        _ => false,
    }
}
