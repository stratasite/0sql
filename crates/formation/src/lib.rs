//! Universe formation: for every table, the cheapest route to every other
//! table it can reach by following joins from their many side to their one
//! side, and from those routes the fields each root can answer.
//!
//! Rails enumerates every simple route with a recursive query and stores
//! one row per route per field. The planner only ever takes the cheapest
//! route per field, so a shortest-path tree per root reproduces its choices
//! at polynomial cost. Two trees per root keep Rails' measure rule: a
//! measure is reachable only over one-to-one or explicitly expandable joins,
//! so measures use a tree restricted to those edges.

use std::cmp::Reverse;
use std::collections::{BinaryHeap, HashMap};
use zsql_semantic::{Cardinality, Field, FieldKind, Id, Path, Tier, Universe};

pub struct Formation {
    pub paths: Vec<Path>,
    /// Equal-cost alternatives the tree had to break by rule.
    pub warnings: Vec<String>,
}

#[derive(Clone)]
struct Edge {
    join: Id,
    to: Id,
    cost: i64,
    measures_ok: bool,
}

/// A route from a root: the joins walked and the tables entered, in order.
#[derive(Clone, Debug)]
struct Route {
    cost: i64,
    joins: Vec<Id>,
    tables: Vec<Id>,
}

pub fn form(universe: &Universe) -> Formation {
    let snap = universe.snapshot();
    let mut warnings = Vec::new();

    let mut edges: HashMap<Id, Vec<Edge>> = HashMap::new();
    for j in &snap.join_defs {
        let measures_ok = j.cardinality == Cardinality::OneToOne || j.allow_measure_expansion;
        edges.entry(j.left_id).or_default().push(Edge { join: j.id, to: j.right_id, cost: j.cost, measures_ok });
    }

    let tier_of = |table_id: Id| -> Option<i64> {
        universe.table(table_id).and_then(|t| universe.datasource(t.datasource_id)).and_then(|d| d.tier).map(|t| match t {
            Tier::Hot => 0,
            Tier::Warm => 1,
            Tier::Cold => 2,
        })
    };
    let fields_on = |table_id: Id| -> Vec<&Field> { universe.expressions_of(table_id).filter_map(|e| universe.field(e.field_id)).collect() };

    let mut paths: Vec<Path> = Vec::new();
    let mut next_id: Id = 1;

    // Single-table paths first, as Rails creates them, in table order.
    for t in &snap.tables {
        for f in fields_on(t.id) {
            paths.push(Path {
                id: next_id,
                root_table_id: t.id,
                target_table_id: t.id,
                from_table_id: None,
                field_id: f.id,
                field_type: f.kind,
                step_number: Some(0),
                cost: t.cost,
                tier: tier_of(t.id),
                joins: vec![],
                table_nodes: vec![t.id],
                datasource_id: t.datasource_id,
                blending_field_id: None,
                extended_blend_group: None,
            });
            next_id += 1;
        }
    }

    // Multi-hop routes: one tree over all joins for dimensions, one over
    // measure-safe joins for measures.
    for root in &snap.tables {
        let dim_tree = shortest_routes(root.id, &edges, false, &mut warnings, universe);
        let msr_tree = shortest_routes(root.id, &edges, true, &mut warnings, universe);
        let emit = |route: &Route, kind: FieldKind, paths: &mut Vec<Path>, next_id: &mut Id| {
            let target = *route.tables.last().expect("route target");
            let from = if route.tables.len() >= 2 { route.tables[route.tables.len() - 2] } else { root.id };
            let cost = route.cost.max(route.joins.len() as i64);
            let mut nodes: Vec<Id> = Vec::new();
            for j in &route.joins {
                if let Some(jd) = universe.join(*j) {
                    if !nodes.contains(&jd.left_id) {
                        nodes.push(jd.left_id);
                    }
                }
            }
            for j in &route.joins {
                if let Some(jd) = universe.join(*j) {
                    if !nodes.contains(&jd.right_id) {
                        nodes.push(jd.right_id);
                    }
                }
            }
            let Some(target_table) = universe.table(target) else { return };
            for f in fields_on(target).into_iter().filter(|f| f.kind == kind) {
                paths.push(Path {
                    id: *next_id,
                    root_table_id: root.id,
                    target_table_id: target,
                    from_table_id: Some(from),
                    field_id: f.id,
                    field_type: f.kind,
                    step_number: Some(route.joins.len() as i64),
                    cost,
                    tier: tier_of(target),
                    joins: route.joins.clone(),
                    table_nodes: nodes.clone(),
                    datasource_id: target_table.datasource_id,
                    blending_field_id: None,
                    extended_blend_group: None,
                });
                *next_id += 1;
            }
        };
        // Emission order stands in for Rails' row order, which breaks cost
        // ties: shorter routes first, then by join ids.
        let mut targets: Vec<&Id> = dim_tree.keys().collect();
        targets.sort_by_key(|t| (dim_tree[*t].joins.len(), dim_tree[*t].joins.clone()));
        for t in targets {
            emit(&dim_tree[t], FieldKind::Dimension, &mut paths, &mut next_id);
        }
        let mut targets: Vec<&Id> = msr_tree.keys().collect();
        targets.sort_by_key(|t| (msr_tree[*t].joins.len(), msr_tree[*t].joins.clone()));
        for t in targets {
            emit(&msr_tree[t], FieldKind::Measure, &mut paths, &mut next_id);
        }
    }

    extended_blend_paths(universe, &mut paths, &mut next_id);
    ambiguous_field_routes(universe, &paths, &mut warnings);

    Formation { paths, warnings }
}

/// A field reachable from one root through two different tables at the
/// same cost: the model does not say whose value the user gets, and the
/// engine can only pick consistently, not correctly. Named so the modeler
/// can split the dimension or set costs so the intended table wins.
fn ambiguous_field_routes(universe: &Universe, paths: &[Path], warnings: &mut Vec<String>) {
    let mut by_pair: HashMap<(Id, Id), Vec<&Path>> = HashMap::new();
    for p in paths.iter().filter(|p| p.blending_field_id.is_none()) {
        by_pair.entry((p.root_table_id, p.field_id)).or_default().push(p);
    }
    let mut keys: Vec<&(Id, Id)> = by_pair.keys().collect();
    keys.sort();
    for key in keys {
        let mut candidates = by_pair[key].clone();
        candidates.sort_by_key(|p| (p.cost, universe.path_signature(p)));
        let cheapest = candidates[0];
        let tied: Vec<&Path> = candidates.iter().copied().filter(|p| p.cost == cheapest.cost && p.target_table_id != cheapest.target_table_id).collect();
        if tied.is_empty() {
            continue;
        }
        let name = |id: Id| universe.table(id).map(|t| t.uid.as_str()).unwrap_or("?");
        let field = universe.field(key.1).map(|f| f.name.as_str()).unwrap_or("?");
        let mut tables: Vec<String> = std::iter::once(cheapest).chain(tied.iter().copied()).map(|p| name(p.target_table_id).to_string()).collect();
        tables.sort();
        tables.dedup();
        warnings.push(format!(
            "ambiguous: from {} the field \"{}\" is reachable at cost {} through {}; {} is used, set costs or split the dimension to choose deliberately",
            name(key.0),
            field,
            cheapest.cost,
            tables.join(" and "),
            name(cheapest.target_table_id)
        ));
    }
}

/// Dijkstra from `root` over the directed join graph. Equal-cost routes
/// break on fewer hops, then on the route's table uids, so the choice is a
/// property of the model rather than of row order.
fn shortest_routes(root: Id, edges: &HashMap<Id, Vec<Edge>>, measures_only: bool, warnings: &mut Vec<String>, universe: &Universe) -> HashMap<Id, Route> {
    let uid = |id: Id| universe.table(id).map(|t| t.uid.clone()).unwrap_or_else(|| id.to_string());
    let mut best: HashMap<Id, Route> = HashMap::new();
    let mut heap: BinaryHeap<Reverse<(i64, usize, String, Vec<Id>, Id)>> = BinaryHeap::new();
    heap.push(Reverse((0, 0, uid(root), vec![], root)));
    let mut pending: HashMap<(Id, Vec<Id>), Route> = HashMap::new();
    pending.insert((root, vec![]), Route { cost: 0, joins: vec![], tables: vec![] });

    while let Some(Reverse((cost, hops, sig, joins, table))) = heap.pop() {
        let Some(route) = pending.remove(&(table, joins.clone())) else { continue };
        if table != root {
            if let Some(existing) = best.get(&table) {
                if existing.cost == cost && existing.joins != joins {
                    warnings.push(format!(
                        "root {} reaches {} by two routes of cost {}: {:?} chosen over {:?}",
                        universe.table(root).map(|t| t.uid.as_str()).unwrap_or("?"),
                        universe.table(table).map(|t| t.uid.as_str()).unwrap_or("?"),
                        cost,
                        existing.joins,
                        joins
                    ));
                }
                continue;
            }
            best.insert(table, route.clone());
        }
        let _ = hops;
        for e in edges.get(&table).into_iter().flatten() {
            if measures_only && !e.measures_ok {
                continue;
            }
            // Simple routes only: never re-enter a table already on the route.
            if e.to == root || route.tables.contains(&e.to) || route.joins.contains(&e.join) {
                continue;
            }
            if best.contains_key(&e.to) {
                continue;
            }
            let mut joins = route.joins.clone();
            joins.push(e.join);
            let mut tables = route.tables.clone();
            tables.push(e.to);
            let next = Route { cost: cost + e.cost, joins: joins.clone(), tables };
            let next_sig = format!("{}>{}", sig, uid(e.to));
            pending.insert((e.to, joins.clone()), next);
            heap.push(Reverse((cost + e.cost, joins.len(), next_sig, joins, e.to)));
        }
    }
    best
}

/// Rails `create_extended_blending_paths`: a dimension in a blend group is
/// answered from any universe holding another member of the group, through
/// that local member's path.
fn extended_blend_paths(universe: &Universe, paths: &mut Vec<Path>, next_id: &mut Id) {
    let mut groups: Vec<(String, Vec<Id>)> = Vec::new();
    for f in universe.fields().iter().filter(|f| f.kind == FieldKind::Dimension) {
        if let Some(g) = &f.extended_blend_group {
            match groups.iter_mut().find(|(k, _)| k == g) {
                Some((_, v)) => v.push(f.id),
                None => groups.push((g.clone(), vec![f.id])),
            }
        }
    }
    for (group, dim_ids) in groups {
        let roots: Vec<Id> = {
            let mut r: Vec<Id> = Vec::new();
            for p in paths.iter().filter(|p| p.field_type == FieldKind::Dimension && dim_ids.contains(&p.field_id)) {
                if !r.contains(&p.root_table_id) {
                    r.push(p.root_table_id);
                }
            }
            r
        };
        for root in roots {
            let reached: Vec<Id> = paths.iter().filter(|p| p.root_table_id == root).map(|p| p.field_id).collect();
            let non_local: Vec<Id> = dim_ids.iter().copied().filter(|d| !reached.contains(d)).collect();
            if non_local.is_empty() {
                continue;
            }
            let local: Vec<Id> = dim_ids.iter().copied().filter(|d| reached.contains(d)).collect();
            let mut candidates: Vec<&Path> = paths.iter().filter(|p| p.root_table_id == root && local.contains(&p.field_id) && p.extended_blend_group.is_none()).collect();
            candidates.sort_by_key(|p| (p.cost, p.id));
            let Some(local_path) = candidates.first().cloned().cloned() else { continue };
            for d in non_local {
                let mut blend = local_path.clone();
                blend.id = *next_id;
                *next_id += 1;
                blend.blending_field_id = Some(local_path.field_id);
                blend.field_id = d;
                blend.extended_blend_group = Some(group.clone());
                blend.field_type = FieldKind::Dimension;
                paths.push(blend);
            }
        }
    }
}
