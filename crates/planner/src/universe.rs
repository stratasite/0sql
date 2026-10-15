//! Planner-side universes: one per root table, holding the join paths that
//! reach fields from it, and subtrees, the per-join-path slices a node
//! actually walks. Paths come from the snapshot for now; the formation
//! crate will compute them.

use crate::model::table_to_s;
use std::collections::HashSet;
use std::sync::Arc;
use zsql_semantic::{Datasource, Field, FieldKind, Id, Path, Table, Tier, Universe};

/// Rails `Planner::Universe::Base`: a root table and the paths from it,
/// cheapest first. Rails breaks ties on row id; here they break on the
/// path signature (route table uids plus field uid), so the pick is
/// idempotent across branches and deploys.
#[derive(Debug)]
pub struct Base<'a> {
    pub root: &'a Table,
    pub datasource: &'a Datasource,
    pub paths: Vec<&'a Path>,
    field_ids: Vec<Id>,
    field_set: HashSet<Id>,
    measure_ids: HashSet<Id>,
    dimension_ids: HashSet<Id>,
    table_nodes: Vec<Id>,
    local_field_ids: HashSet<Id>,
    to_s: String,
    /// The paths grouped by join path, in first-seen order: what every
    /// node's subtrees slice from, computed once per universe.
    groups: Vec<Arc<PathGroup<'a>>>,
}

/// One join path out of a root and the paths that share it.
#[derive(Debug)]
pub struct PathGroup<'a> {
    pub join_path: Vec<Id>,
    pub paths: Vec<&'a Path>,
    pub join_cost: i64,
    /// The route as table uids past the root, e.g. `customer>customer_address`.
    pub route: String,
}

impl<'a> Base<'a> {
    fn new(universe: &'a Universe, root: &'a Table, mut paths: Vec<&'a Path>) -> Self {
        paths.sort_by_cached_key(|p| (p.cost, universe.path_signature(p)));
        let mut field_ids = Vec::new();
        let mut field_set = HashSet::new();
        let mut measure_ids = HashSet::new();
        let mut dimension_ids = HashSet::new();
        let mut table_nodes = Vec::new();
        for p in &paths {
            if field_set.insert(p.field_id) {
                field_ids.push(p.field_id);
                match p.field_type {
                    FieldKind::Measure => measure_ids.insert(p.field_id),
                    FieldKind::Dimension => dimension_ids.insert(p.field_id),
                };
            }
            for t in &p.table_nodes {
                if !table_nodes.contains(t) {
                    table_nodes.push(*t);
                }
            }
        }
        let local_field_ids = universe.expressions_of(root.id).map(|e| e.field_id).collect();
        let datasource = universe.datasource(root.datasource_id).expect("table datasource");
        let to_s = format!("<{}>", table_to_s(universe, root));
        let mut groups: Vec<(Vec<Id>, Vec<&'a Path>)> = Vec::new();
        for p in &paths {
            match groups.iter_mut().find(|(jp, _)| *jp == p.joins) {
                Some((_, ps)) => ps.push(p),
                None => groups.push((p.joins.clone(), vec![p])),
            }
        }
        let groups = groups
            .into_iter()
            .map(|(join_path, paths)| {
                let (join_cost, route) = route_of(universe, &join_path);
                Arc::new(PathGroup { join_path, paths, join_cost, route })
            })
            .collect();
        Base { root, datasource, paths, field_ids, field_set, measure_ids, dimension_ids, table_nodes, local_field_ids, to_s, groups }
    }

    /// The paths grouped by join path, cheapest path first within each.
    pub fn groups(&self) -> &[Arc<PathGroup<'a>>] {
        &self.groups
    }

    pub fn has_field(&self, id: Id) -> bool {
        self.field_set.contains(&id)
    }
    pub fn has_measure(&self, id: Id) -> bool {
        self.measure_ids.contains(&id)
    }
    pub fn has_dimension(&self, id: Id) -> bool {
        self.dimension_ids.contains(&id)
    }
    pub fn to_s(&self) -> &str {
        &self.to_s
    }
    pub fn all_dimensions(&self, ids: &[Id]) -> bool {
        ids.iter().all(|i| self.dimension_ids.contains(i))
    }
    pub fn any_dimensions(&self, ids: &[Id]) -> bool {
        ids.iter().any(|i| self.dimension_ids.contains(i))
    }
    pub fn any_measures(&self, ids: &[Id]) -> bool {
        ids.iter().any(|i| self.measure_ids.contains(i))
    }
    pub fn field_ids(&self) -> &[Id] {
        &self.field_ids
    }
    /// Cheapest path to the field.
    pub fn path_for(&self, field_id: Id) -> Option<&'a Path> {
        self.paths.iter().copied().find(|p| p.field_id == field_id)
    }
    pub fn paths_for(&self, field_id: Id) -> Vec<&'a Path> {
        self.paths.iter().copied().filter(|p| p.field_id == field_id).collect()
    }
}

/// What universe selection sorts on; shared by bases and subtrees.
pub trait Model {
    fn root(&self) -> &Table;
    fn datasource(&self) -> &Datasource;
    /// How many of the hinted tables the model walks.
    fn hint_count(&self, hints: &HashSet<Id>) -> usize;
    /// How many of the wanted dimensions the model reaches.
    fn dimension_count(&self, wanted: &HashSet<Id>) -> usize;
    /// How many of the wanted measures the model reaches.
    fn measure_count(&self, wanted: &HashSet<Id>) -> usize;
    fn local_field_ids(&self) -> &HashSet<Id>;
    fn cost(&self) -> i64;
    fn to_s(&self) -> String;
    fn has_field(&self, id: Id) -> bool;
    fn tier(&self) -> i64 {
        match self.datasource().tier {
            Some(Tier::Hot) => 0,
            Some(Tier::Warm) => 1,
            Some(Tier::Cold) => 2,
            None => 3,
        }
    }
}

impl<'a> Model for Arc<Base<'a>> {
    fn root(&self) -> &Table {
        self.root
    }
    fn datasource(&self) -> &Datasource {
        self.datasource
    }
    fn hint_count(&self, hints: &HashSet<Id>) -> usize {
        self.table_nodes.iter().filter(|t| hints.contains(t)).count()
    }
    fn dimension_count(&self, wanted: &HashSet<Id>) -> usize {
        self.dimension_ids.intersection(wanted).count()
    }
    fn measure_count(&self, wanted: &HashSet<Id>) -> usize {
        self.measure_ids.intersection(wanted).count()
    }
    fn local_field_ids(&self) -> &HashSet<Id> {
        &self.local_field_ids
    }
    fn cost(&self) -> i64 {
        self.root.cost
    }
    fn to_s(&self) -> String {
        self.to_s.clone()
    }
    fn has_field(&self, id: Id) -> bool {
        self.field_set.contains(&id)
    }
}

/// Rails `Planner::Universe::Subtree`: one join path out of a universe and
/// the paths that share it.
#[derive(Debug, Clone)]
pub struct Subtree<'a> {
    pub base: Arc<Base<'a>>,
    pub join_path: Vec<Id>,
    pub paths: Vec<&'a Path>,
    join_cost: i64,
    /// The route as table uids past the root, e.g. `customer>customer_address`.
    /// Rails prints join row ids here; uids make node identity, and so CTE
    /// names, the same on every branch and deploy.
    route: String,
}

/// A join path's summed cost and its route as table uids past the root
/// (`customer>customer_address`). Rails prints join row ids in the route;
/// uids make node identity, and so CTE names, the same on every branch.
fn route_of(universe: &Universe, join_path: &[Id]) -> (i64, String) {
    let joins: Vec<_> = join_path.iter().filter_map(|j| universe.join(*j)).collect();
    let join_cost = joins.iter().map(|j| j.cost).sum();
    let route = joins
        .iter()
        .map(|j| universe.table(j.right_id).map(|t| t.uid.clone()).unwrap_or_else(|| j.right_id.to_string()))
        .collect::<Vec<_>>()
        .join(">");
    (join_cost, route)
}

impl<'a> Subtree<'a> {
    pub fn new(universe: &Universe, base: Arc<Base<'a>>, join_path: Vec<Id>, paths: Vec<&'a Path>) -> Self {
        let (join_cost, route) = route_of(universe, &join_path);
        Subtree { base, join_path, paths, join_cost, route }
    }

    /// A subtree over a precomputed group, keeping only the given paths.
    pub fn from_group(base: Arc<Base<'a>>, group: &PathGroup<'a>, paths: Vec<&'a Path>) -> Self {
        Subtree { base, join_path: group.join_path.clone(), paths, join_cost: group.join_cost, route: group.route.clone() }
    }

    pub fn push(&mut self, path: &'a Path) {
        self.paths.push(path);
    }

    pub fn path_for(&self, field_id: Id) -> Option<&'a Path> {
        self.paths.iter().copied().find(|p| p.field_id == field_id)
    }

    pub fn paths_for(&self, field_id: Id) -> Vec<&'a Path> {
        self.paths.iter().copied().filter(|p| p.field_id == field_id).collect()
    }

    pub fn same(&self, other: &Subtree<'a>) -> bool {
        self.base.root.id == other.base.root.id && self.join_path == other.join_path
    }
}

impl<'a> Model for Subtree<'a> {
    fn root(&self) -> &Table {
        self.base.root
    }
    fn datasource(&self) -> &Datasource {
        self.base.datasource
    }
    fn hint_count(&self, hints: &HashSet<Id>) -> usize {
        let mut seen: Vec<Id> = Vec::new();
        for p in &self.paths {
            for t in &p.table_nodes {
                if hints.contains(t) && !seen.contains(t) {
                    seen.push(*t);
                }
            }
        }
        seen.len()
    }
    fn dimension_count(&self, wanted: &HashSet<Id>) -> usize {
        let mut seen: Vec<Id> = Vec::new();
        for p in &self.paths {
            if p.field_type == FieldKind::Dimension && wanted.contains(&p.field_id) && !seen.contains(&p.field_id) {
                seen.push(p.field_id);
            }
        }
        seen.len()
    }
    fn measure_count(&self, wanted: &HashSet<Id>) -> usize {
        let mut seen: Vec<Id> = Vec::new();
        for p in &self.paths {
            if p.field_type == FieldKind::Measure && wanted.contains(&p.field_id) && !seen.contains(&p.field_id) {
                seen.push(p.field_id);
            }
        }
        seen.len()
    }
    fn local_field_ids(&self) -> &HashSet<Id> {
        &self.base.local_field_ids
    }
    fn cost(&self) -> i64 {
        self.join_cost
    }
    fn to_s(&self) -> String {
        format!("{}:[{}]", self.base.to_s, self.route)
    }
    fn has_field(&self, id: Id) -> bool {
        self.paths.iter().any(|p| p.field_id == id)
    }
}

/// Every base universe of the branch, in table order.
pub struct Universes<'a> {
    pub bases: Vec<Arc<Base<'a>>>,
}

impl<'a> Universes<'a> {
    /// Universes over the snapshot's own (Rails-enumerated) paths.
    pub fn build(universe: &'a Universe) -> Self {
        Self::build_from(universe, &universe.snapshot().paths)
    }

    /// Universes over the given paths, formed or stored.
    pub fn build_from(universe: &'a Universe, all_paths: &'a [Path]) -> Self {
        let snap = universe.snapshot();
        let bases = snap
            .tables
            .iter()
            .map(|t| {
                let paths = all_paths.iter().filter(|p| p.root_table_id == t.id).collect();
                Arc::new(Base::new(universe, t, paths))
            })
            .collect();
        Universes { bases }
    }

    pub fn for_root(&self, table_id: Id) -> Option<Arc<Base<'a>>> {
        self.bases.iter().find(|b| b.root.id == table_id).cloned()
    }

    /// Datasource ids holding an expression for every one of the measures.
    pub fn datasources_with_all_measures(&self, universe: &Universe, measures: &[&Field]) -> Vec<Id> {
        let mut ds_ids: Vec<Id> = Vec::new();
        for b in &self.bases {
            if !ds_ids.contains(&b.datasource.id) {
                ds_ids.push(b.datasource.id);
            }
        }
        if measures.is_empty() {
            return ds_ids;
        }
        ds_ids
            .into_iter()
            .filter(|ds| {
                measures.iter().all(|m| {
                    self.bases.iter().filter(|b| b.datasource.id == *ds).any(|b| universe.expression_for(b.root.id, m.id).is_some())
                })
            })
            .collect()
    }
}
