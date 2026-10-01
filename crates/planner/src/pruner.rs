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

/// Rails `Partition#within_partition_range?`, for numeric and date range
/// partitions; a list partition holds the filter when every value the
/// filter wants is listed.
fn within_partition(part: &zsql_semantic::Partition, f: &RFilter) -> bool {
    use crate::sql::dates::parse_value;
    if part.predicate == Predicate::InList {
        let listed: HashSet<String> = part.filter_value.as_deref().map(split_list).unwrap_or_default().into_iter().collect();
        let wanted: Vec<String> = match (f.def.predicate, f.def.filter_value.as_deref()) {
            (Predicate::Equals, Some(v)) => vec![v.trim().to_lowercase()],
            (Predicate::InList, Some(v)) => split_list(v),
            _ => return false,
        };
        return !wanted.is_empty() && wanted.iter().all(|v| listed.contains(v));
    }
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

/// A list value's entries as the SQL visitor splits them (quotes honoured),
/// trimmed and lower-cased.
fn split_list(value: &str) -> Vec<String> {
    crate::visitors::sql::split_list(value)
        .iter()
        .map(|v| v.trim().trim_matches(|c| c == '\'' || c == '"').trim().to_lowercase())
        .filter(|v| !v.is_empty())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use zsql_semantic::Partition;

    fn field(data_type: &str) -> Field {
        serde_json::from_value(serde_json::json!({
            "id": 1, "uid": "region", "name": "Region", "type": "Semantic::Dimension", "data_type": data_type,
            "display_type": "", "format": null, "exclusion_type": null, "inclusions": null,
            "extended_blend_group": null, "snapshot": null, "value_list_size": 0, "description": null
        }))
        .unwrap()
    }

    fn filter<'a>(field: &'a Field, predicate: Predicate, value: &str, end: Option<&str>) -> RFilter<'a> {
        RFilter {
            fid: 0,
            field,
            def: zsql_spec::Filter {
                reference: "f0".into(),
                field_uid: field.uid.clone(),
                field_kind: Some(field.kind),
                predicate,
                filter_value: Some(value.into()),
                filter_value_end: end.map(str::to_string),
                top_n_measure: None,
                ordinal: 0,
                source_ref: None,
            },
        }
    }

    fn partition(predicate: Predicate, value: &str, end: Option<&str>) -> Partition {
        Partition { id: 1, table_id: 1, dimension_id: 1, predicate, filter_value: Some(value.into()), filter_value_end: end.map(str::to_string) }
    }

    #[test]
    fn list_partition_holds_listed_values_only() {
        let f = field("string");
        let p = partition(Predicate::InList, "US, 'EU', apac", None);
        assert!(within_partition(&p, &filter(&f, Predicate::Equals, "eu", None)));
        assert!(within_partition(&p, &filter(&f, Predicate::InList, "us,APAC", None)));
        assert!(!within_partition(&p, &filter(&f, Predicate::InList, "us,latam", None)));
        assert!(!within_partition(&p, &filter(&f, Predicate::DoesNotEqual, "us", None)));
        assert!(!within_partition(&p, &filter(&f, Predicate::GreaterThan, "1", None)));
    }

    #[test]
    fn range_partitions_cover_comparison_filters() {
        let f = field("integer");
        let ge = partition(Predicate::GreaterThanOrEqualTo, "2020", None);
        assert!(within_partition(&ge, &filter(&f, Predicate::GreaterThan, "2021", None)));
        assert!(within_partition(&ge, &filter(&f, Predicate::Between, "2020", Some("2024"))));
        assert!(!within_partition(&ge, &filter(&f, Predicate::Between, "2019", Some("2024"))));
        assert!(!within_partition(&ge, &filter(&f, Predicate::LessThan, "2024", None)));
        let gt = partition(Predicate::GreaterThan, "2020", None);
        assert!(!within_partition(&gt, &filter(&f, Predicate::Equals, "2020", None)));
        assert!(within_partition(&gt, &filter(&f, Predicate::Equals, "2021", None)));
        let le = partition(Predicate::LessThanOrEqualTo, "2020", None);
        assert!(within_partition(&le, &filter(&f, Predicate::LessThan, "2019", None)));
        assert!(!within_partition(&le, &filter(&f, Predicate::GreaterThan, "2000", None)));
    }
}
