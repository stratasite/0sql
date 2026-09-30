//! Rails `TemporalVisitor`: a measure compared to its value one period
//! earlier. A comparison node re-aggregates the measure with its date
//! filters shifted back and its date projections pushed forward, so the
//! two periods join on the same date column.

use crate::dag::{Filtered, Graph, MultiExpr, NodeId, NodeKind, Projected, Reference};
use crate::model::{RProjection, RQuery};
use crate::sql::dates::parse_value;
use crate::sql::expr::entity_gsub;
use crate::visitors::alias::SKIP_ALIAS_PREFIX;
use crate::visitors::grain::{create_agg_node, create_merge_node, grain_key};
use crate::{PlanError, Result};
use chrono::{Datelike, Duration, NaiveDate, NaiveDateTime, NaiveTime};
use std::collections::HashSet;
use std::rc::Rc;
use zsql_spec::{Decorator, DecoratorKind};
use zsql_semantic::{Field, Predicate, Universe};

pub fn run<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>) -> Result<()> {
    let node = graph.root();
    let measures: Vec<Projected<'a>> = graph.nodes[node].projections.iter().filter(|p| p.decorators().iter().any(|d| d.kind == DecoratorKind::Temporalize)).cloned().collect();
    if measures.is_empty() {
        return Ok(());
    }
    let has_dates = q.filters.iter().any(|f| f.is_date()) || q.projections.iter().any(|p| p.field.map(|f| f.is_date()).unwrap_or(false));
    if !has_dates {
        return Ok(());
    }
    if !graph.nodes[node].is_merge() {
        let merge = create_merge_node(graph, q);
        graph.add_edge(node, merge);
    }
    for mp in measures {
        let r = graph.nodes[node].get_ref(mp.proj.pid, mp.proj.field_id()).cloned();
        if mp.is_calculation() {
            process_calculation(graph, universe, q, &mp, node)?;
        } else if let Some(Reference::Multi { expression: Some(MultiExpr::Semantic(exp)), .. }) = r {
            process_compound(graph, universe, q, &mp, node, exp)?;
        } else {
            process_table(graph, universe, q, &mp)?;
        }
    }
    Ok(())
}

fn temporalizer(mp: &Projected) -> Decorator {
    mp.decorators().into_iter().find(|d| d.kind == DecoratorKind::Temporalize).cloned().expect("temporalizer")
}

fn unit(t: &Decorator) -> String {
    t.str("transform").unwrap_or("ERROR").split('_').next().unwrap_or("ERROR").to_string()
}

fn projected_grain(q: &RQuery) -> Option<String> {
    let date = q.projections.iter().find(|p| p.field.map(|f| f.is_date()).unwrap_or(false))?;
    date.def.decorators.iter().find(|d| d.kind == DecoratorKind::Truncate).and_then(|d| d.grain().map(str::to_string))
}

fn unit_rank(u: &str) -> Option<i32> {
    Some(match u {
        "day" => 0,
        "week" => 1,
        "month" => 2,
        "quarter" => 3,
        "year" => 4,
        _ => return None,
    })
}

fn unit_is_higher(t: &Decorator, q: &RQuery) -> bool {
    match (unit_rank(&unit(t)), projected_grain(q).and_then(|g| unit_rank(&g))) {
        (Some(u), Some(g)) => u >= g,
        _ => false,
    }
}

fn digest(name: &str, t: &Decorator) -> String {
    let prefix: String = t.transform_label().to_lowercase().chars().filter(|c| c.is_alphanumeric() || *c == '_').collect();
    format!("{}{}_{}", SKIP_ALIAS_PREFIX, prefix, &format!("{:x}", md5::compute(name.as_bytes()))[..9])
}

fn process_table<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, mp: &Projected<'a>) -> Result<()> {
    let t = temporalizer(mp);
    let field = mp.field().expect("measure field");
    let agg = graph.live().find(|n| n.purpose.is_none() && n.kind == NodeKind::Aggregation && n.has_proj(mp.proj.pid)).map(|n| n.id);
    let agg = agg.ok_or_else(|| PlanError::Resolution(format!("No aggregation node holds {}", mp.to_s())))?;
    let calc = transformed_calculation(graph, universe, q, field, &t, Some(agg), Some(mp))?;
    let sql = if t.bool("percent_change", false) {
        format!(
            "( ([{}]@m) - ([{}]) ) /\n\t\t({})",
            field.name,
            calc.alias(),
            graph.dialect.null_if_zero(&format!("[{}]*1.00", calc.alias()))?
        )
    } else {
        format!("[{}]", calc.alias())
    };
    let mut final_calc = (*q.static_calc(universe, &mp.alias, &sql)).clone_shallow();
    final_calc.set_source(Some(mp.proj.clone()));
    final_calc.def.decorators = mp.decorators().into_iter().filter(|d| d.kind != DecoratorKind::Temporalize).cloned().collect();
    let root = graph.root();
    graph.nodes[root].replace_projection(mp.proj.pid, Rc::new(final_calc));
    Ok(())
}

/// Rails `process_calculation`: every measure the formula reads, directly or
/// through a projected measure's alias, is swapped for its comparison-period
/// static; the result replaces the calculation at the root.
fn process_calculation<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, mp: &Projected<'a>, merge: NodeId) -> Result<()> {
    let t = temporalizer(mp);
    let original = mp.proj.def.sql.clone().unwrap_or_default();
    let mut transformed = update_transformed_fields(graph, universe, q, &mp.proj, &original, &t)?;
    for r in mp.proj.def.projection_refs.clone() {
        transformed = update_transformed_projection_refs(graph, universe, q, &transformed, &r, merge, &t, &mp.proj)?;
    }
    if t.bool("percent_change", false) {
        transformed = format!("(({}) - ({})) / \n\t\t({})", original, transformed, graph.dialect.null_if_zero(&transformed)?);
    }
    let mut final_calc = (*q.static_calc(universe, &mp.alias, &transformed)).clone_shallow();
    final_calc.set_source(Some(mp.proj.clone()));
    final_calc.def.decorators = mp.decorators().into_iter().filter(|d| d.kind != DecoratorKind::Temporalize).cloned().collect();
    let root = graph.root();
    graph.nodes[root].replace_projection(mp.proj.pid, Rc::new(final_calc));
    Ok(())
}

fn update_transformed_fields<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, calc: &Rc<RProjection<'a>>, sql: &str, t: &Decorator) -> Result<String> {
    let mut out = sql.to_string();
    for m in calc.field_refs.iter().filter(|f| f.is_measure()) {
        let tf = transformed_calculation(graph, universe, q, m, t, None, None)?;
        out = entity_gsub(&out, &m.name, &format!("[{}]", tf.alias()));
    }
    Ok(out)
}

#[allow(clippy::too_many_arguments)]
fn update_transformed_projection_refs<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, sql: &str, alias: &str, merge: NodeId, t: &Decorator, parent: &Rc<RProjection<'a>>) -> Result<String> {
    let Some(pj) = graph.nodes[merge].projections.iter().find(|p| p.proj.match_alias(alias)).cloned() else { return Ok(sql.to_string()) };
    let mut out = sql.to_string();
    if !pj.is_calculation() && pj.is_measure() && pj.proj.pid != parent.pid {
        let tf = transformed_calculation(graph, universe, q, pj.field().expect("field"), t, None, None)?;
        out = entity_gsub(&out, &pj.proj.alias(), &format!("[{}]", tf.alias()));
    } else if pj.is_calculation() && pj.proj.pid != parent.pid {
        // A referenced calculation is inlined with its own measures moved to
        // the comparison period, recursively, so the whole formula shifts as
        // one. (Rails stops here with an undefined method.)
        let inner = transformed_formula(graph, universe, q, &pj.proj, merge, t, &mut vec![parent.pid, pj.proj.pid])?;
        out = entity_gsub(&out, &pj.proj.alias(), &format!("({inner})"));
    }
    Ok(out)
}

/// A calculation's formula with every measure it stands on, directly or
/// through aliases, replaced by the comparison-period static. `seen` guards
/// alias cycles.
fn transformed_formula<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, calc: &Rc<RProjection<'a>>, merge: NodeId, t: &Decorator, seen: &mut Vec<usize>) -> Result<String> {
    let original = calc.def.sql.clone().unwrap_or_default();
    let mut out = update_transformed_fields(graph, universe, q, calc, &original, t)?;
    for r in calc.def.projection_refs.clone() {
        let Some(pj) = graph.nodes[merge].projections.iter().find(|p| p.proj.match_alias(&r)).cloned() else { continue };
        if !pj.is_calculation() && pj.is_measure() {
            let tf = transformed_calculation(graph, universe, q, pj.field().expect("field"), t, None, None)?;
            out = entity_gsub(&out, &pj.proj.alias(), &format!("[{}]", tf.alias()));
        } else if pj.is_calculation() {
            if seen.contains(&pj.proj.pid) {
                return Err(PlanError::Resolution(format!("Calculation {} references itself through {}", calc.alias(), pj.proj.alias())));
            }
            seen.push(pj.proj.pid);
            let inner = transformed_formula(graph, universe, q, &pj.proj, merge, t, seen)?;
            out = entity_gsub(&out, &pj.proj.alias(), &format!("({inner})"));
        }
    }
    Ok(out)
}

fn process_compound<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, mp: &Projected<'a>, merge: NodeId, exp: &'a zsql_semantic::Expression) -> Result<()> {
    let t = temporalizer(mp);
    let mut original = exp.sql.clone().unwrap_or_default();
    let refs: Vec<&Field> = exp.dimension_ref_ids.iter().chain(exp.measure_ref_ids.iter()).filter_map(|i| universe.field(*i)).collect();
    for f in &refs {
        let ty = if f.is_dimension() { "@D" } else { "@M" };
        original = entity_gsub(&original, &f.name, &format!("[{}]{}", f.name, ty));
    }
    let mut transformed = original.clone();
    for m in refs.iter().filter(|f| f.is_measure()) {
        let calc = transformed_calculation(graph, universe, q, m, &t, None, None)?;
        transformed = entity_gsub(&transformed, &m.name, &format!("[{}]", calc.alias()));
    }
    if t.bool("percent_change", false) {
        transformed = format!("(({}) - ({})) / \n\t\t({})", original, transformed, graph.dialect.null_if_zero(&transformed)?);
    }
    let calc = q.static_calc(universe, &mp.alias, &transformed);
    let key = mp.proj.field_id().map(crate::dag::RefKey::Field).unwrap_or(crate::dag::RefKey::Proj(mp.proj.pid));
    if let Some(Reference::Multi { expression, .. }) = graph.nodes[merge].field_refs.get_mut(&key) {
        *expression = Some(MultiExpr::Calc(calc));
    }
    Ok(())
}

/// The comparison-period static for the measure, on the transformed node.
fn transformed_calculation<'a>(
    graph: &mut Graph<'a>,
    universe: &'a Universe,
    q: &RQuery<'a>,
    measure: &'a Field,
    t: &Decorator,
    agg: Option<NodeId>,
    mp: Option<&Projected<'a>>,
) -> Result<Rc<RProjection<'a>>> {
    let agg = match agg {
        Some(a) => a,
        None => graph
            .live()
            .find(|n| n.purpose.is_none() && n.kind == NodeKind::Aggregation && n.has_field(measure.id))
            .map(|n| n.id)
            .ok_or_else(|| PlanError::Resolution(format!("No aggregation node holds {}", measure.name)))?,
    };
    let transformed = find_or_create_transformed_node(graph, universe, q, measure, t, agg)?;
    let alias = digest(&measure.name, t);
    let calc = match graph.nodes[transformed].projections.iter().find(|p| p.is_calculation() && p.proj.match_alias(&alias)) {
        Some(p) => p.proj.clone(),
        None => {
            let c = (*q.static_calc(universe, &alias, &format!("[{}]@m", measure.name))).clone_shallow();
            c.set_source(graph.nodes[agg].projections.iter().find(|p| p.field().map(|f| f.id == measure.id).unwrap_or(false)).map(|p| p.proj.clone()));
            let c = Rc::new(c);
            graph.nodes[transformed].add_projection(c.clone());
            c
        }
    };

    if !t.bool("percent_change", false) {
        // The displayed value is the comparison alone: this projection leaves
        // the current-period node.
        if let Some(mp) = mp {
            graph.nodes[agg].delete_projection(mp.proj.pid);
        }
        let plain_left = graph.nodes[agg].projections.iter().any(|p| !p.is_calculation() && p.field().map(|f| f.id == measure.id).unwrap_or(false));
        if !plain_left {
            let root = graph.root();
            let agg_fids: Vec<usize> = graph.nodes[agg].filters.iter().map(|f| f.fid()).collect();
            let doomed: Vec<usize> = graph.nodes[root].filters.iter().filter(|f| f.is_measure() && f.field().id == measure.id && agg_fids.contains(&f.fid())).map(|f| f.fid()).collect();
            for fid in doomed {
                graph.nodes[root].delete_filter(fid);
            }
        }
        if !graph.nodes[agg].projections.iter().any(|p| p.is_measure() || p.is_calculation()) {
            graph.destroy_node(agg);
        }
    }
    Ok(calc)
}

fn find_or_create_transformed_node<'a>(graph: &mut Graph<'a>, universe: &'a Universe, q: &RQuery<'a>, measure: &'a Field, t: &Decorator, agg: NodeId) -> Result<NodeId> {
    let transform = t.str("transform").unwrap_or("").to_string();
    let agg_dims: HashSet<String> = graph.nodes[agg].projections.iter().filter(|p| p.is_dimension()).map(grain_key).collect();
    let signature = crate::visitors::segmentation::node_signature(&graph.nodes[agg]);
    let found = graph
        .live()
        .find(|n| {
            n.purpose == Some("temporal")
                && n.id != agg
                && n.kind == NodeKind::Aggregation
                && n.universe.as_ref().map(|u| u.has_measure(measure.id)).unwrap_or(false)
                && n.projections.iter().filter(|p| p.is_dimension()).map(grain_key).collect::<HashSet<_>>() == agg_dims
                && crate::visitors::segmentation::node_signature(n) == signature
                && n.transform.as_deref() == Some(transform.as_str())
        })
        .map(|n| n.id);
    if let Some(n) = found {
        return Ok(n);
    }
    let projections: Vec<Rc<RProjection<'a>>> = graph.nodes[agg].projections.iter().filter(|p| p.is_dimension()).map(|p| p.proj.clone()).collect();
    let mut filters: Vec<Filtered<'a>> = graph.nodes[agg].filters.iter().filter(|f| !f.is_date()).cloned().collect();
    // Date filters will be shifted, so they get uncoupled copies.
    filters.extend(graph.nodes[agg].filters.iter().filter(|f| f.is_date()).map(|f| Filtered::new(f.filter.clone())));
    let base = graph.nodes[agg].universe.clone().expect("agg universe");
    let root = graph.root();
    let id = create_agg_node(graph, universe, q, base, projections, vec![])?;
    graph.nodes[id].filters = filters;
    // Subtrees were set without the filters; rebuild with them.
    crate::resolver::set_subtrees(graph, id, universe, q)?;
    graph.add_edge(id, root);
    for input in graph.nodes[agg].inputs.clone() {
        match graph.nodes[agg].strategies.iter().find(|s| s.node == input).cloned() {
            Some(s) => graph.add_strategy_edge(s, id),
            None => graph.add_edge(input, id),
        }
    }
    graph.nodes[id].purpose = Some("temporal");
    graph.nodes[id].transform = Some(transform);
    transform_date_projections(graph, id, t, q)?;
    transform_date_filters(graph, id, t, q)?;
    Ok(id)
}

/// Push projected dates forward one period so the comparison rows join.
fn transform_date_projections<'a>(graph: &mut Graph<'a>, id: NodeId, t: &Decorator, q: &RQuery<'a>) -> Result<()> {
    let u = unit(t);
    let base_sql = graph.dialect.date_add(&u, 1, "@expression")?;
    let grain = projected_grain(q);
    for p in graph.nodes[id].projections.iter_mut().filter(|p| p.field().map(|f| f.is_date()).unwrap_or(false)) {
        let extract = p.proj.def.decorators.iter().find(|d| d.kind == DecoratorKind::Extract).and_then(|d| d.str("extract").map(str::to_string));
        let sql = match extract.as_deref() {
            Some("year") => Some("@expression + 1".to_string()),
            Some("year_month") if u == "year" => Some("@expression + 100".to_string()),
            Some(_) => None,
            None if grain.as_deref() == Some("week") => Some(graph.dialect.truncate_date("week", &base_sql)?),
            None => Some(base_sql.clone()),
        };
        if let Some(sql) = sql {
            // Rails normalises the customizer's template to lowercase.
            let mut attrs = zsql_spec::Attrs::new();
            attrs.insert("custom_sql".into(), serde_json::Value::String(sql.trim().to_lowercase()));
            p.local_decorators.push(Decorator::custom(DecoratorKind::Customize, attrs));
        }
    }
    Ok(())
}

/// Shift the date filters back one period.
fn transform_date_filters<'a>(graph: &mut Graph<'a>, id: NodeId, t: &Decorator, q: &RQuery<'a>) -> Result<()> {
    let u = unit(t);
    let grain = projected_grain(q);
    let higher = unit_is_higher(t, q);
    let format = graph.dialect.date_format().to_string();
    for f in graph.nodes[id].filters.iter_mut().filter(|f| f.is_date()) {
        let value = f.filter_value.clone().unwrap_or_default();
        let start = parse_value(&value, false).ok_or_else(|| PlanError::Resolution(format!("cannot parse date {value}")))?;
        let mut fv = shift_back(start, &u);
        let mut fve = if f.predicate == Predicate::Between {
            let end = f.filter_value_end.clone().unwrap_or_default();
            shift_back(parse_value(&end, true).ok_or_else(|| PlanError::Resolution(format!("cannot parse date {end}")))?, &u)
        } else {
            f.predicate = Predicate::Between;
            start - Duration::days(1)
        };
        if higher {
            if let Some(g) = &grain {
                fv = beginning_of(fv, g);
                fve = end_of(fve, g);
            }
        }
        f.filter_value = Some(fv.format(&format).to_string());
        f.filter_value_end = Some(fve.format(&format).to_string());
    }
    Ok(())
}

fn shift_back(dt: NaiveDateTime, u: &str) -> NaiveDateTime {
    match u {
        "day" => dt - Duration::days(1),
        "week" => dt - Duration::weeks(1),
        "month" => shift_months(dt, -1),
        "quarter" => shift_months(dt, -3),
        "year" => shift_months(dt, -12),
        _ => dt,
    }
}

fn shift_months(dt: NaiveDateTime, months: i32) -> NaiveDateTime {
    let total = dt.year() * 12 + dt.month() as i32 - 1 + months;
    let (y, m) = (total.div_euclid(12), total.rem_euclid(12) as u32 + 1);
    let last = last_day(y, m);
    NaiveDate::from_ymd_opt(y, m, dt.day().min(last)).unwrap().and_time(dt.time())
}

fn last_day(y: i32, m: u32) -> u32 {
    let next = if m == 12 { NaiveDate::from_ymd_opt(y + 1, 1, 1) } else { NaiveDate::from_ymd_opt(y, m + 1, 1) }.unwrap();
    (next - Duration::days(1)).day()
}

/// Rails `beginning_of_<grain>`, weeks starting Monday.
fn beginning_of(dt: NaiveDateTime, grain: &str) -> NaiveDateTime {
    let d = dt.date();
    let start = match grain {
        "week" => d - Duration::days(d.weekday().num_days_from_monday() as i64),
        "month" => NaiveDate::from_ymd_opt(d.year(), d.month(), 1).unwrap(),
        "quarter" => NaiveDate::from_ymd_opt(d.year(), (d.month() - 1) / 3 * 3 + 1, 1).unwrap(),
        "year" => NaiveDate::from_ymd_opt(d.year(), 1, 1).unwrap(),
        _ => d,
    };
    start.and_time(NaiveTime::MIN)
}

fn end_of(dt: NaiveDateTime, grain: &str) -> NaiveDateTime {
    let d = dt.date();
    let end = match grain {
        "week" => d + Duration::days(6 - d.weekday().num_days_from_monday() as i64),
        "month" => NaiveDate::from_ymd_opt(d.year(), d.month(), last_day(d.year(), d.month())).unwrap(),
        "quarter" => {
            let m = (d.month() - 1) / 3 * 3 + 3;
            NaiveDate::from_ymd_opt(d.year(), m, last_day(d.year(), m)).unwrap()
        }
        "year" => NaiveDate::from_ymd_opt(d.year(), 12, 31).unwrap(),
        _ => d,
    };
    end.and_time(NaiveTime::from_hms_opt(23, 59, 59).unwrap())
}
