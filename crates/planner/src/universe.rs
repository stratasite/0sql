//! Planner-side universes: one per root table, holding the join paths that
//! reach fields from it, and subtrees, the per-join-path slices a node
//! actually walks. Paths come from the snapshot for now; the formation
//! crate will compute them.

use crate::resolved::table_identity;
use std::collections::HashSet;
use std::sync::Arc;
use zsql_semantic::{Datasource, Field, FieldKind, Id, Path, SemanticModel, Table, Tier};

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
    identity: String,
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
    fn new(semantic: &'a SemanticModel, root: &'a Table, mut paths: Vec<&'a Path>) -> Self {
        paths.sort_by_cached_key(|p| (p.cost, semantic.path_signature(p)));
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
        let local_field_ids = semantic.expressions_of(root.id).map(|e| e.field_id).collect();
        let datasource = semantic.datasource(root.datasource_id).expect("table datasource");
        let identity = format!("<{}>", table_identity(semantic, root));
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
                let (join_cost, route) = route_of(semantic, &join_path);
                Arc::new(PathGroup { join_path, paths, join_cost, route })
            })
            .collect();
        Base { root, datasource, paths, field_ids, field_set, measure_ids, dimension_ids, table_nodes, local_field_ids, identity, groups }
    }

    /// This universe with more paths from its root, which the layered
    /// model `semantic` knows the fields of. The paths already here keep
    /// their order and are not ranked again; each new one is placed where
    /// the ranking puts it.
    fn with_paths<'b>(&self, semantic: &'b SemanticModel, added: &[&'b Path]) -> Base<'b>
    where
        'a: 'b,
    {
        let mut paths: Vec<&'b Path> = self.paths.clone();
        let mut field_ids = self.field_ids.clone();
        let mut field_set = self.field_set.clone();
        let mut measure_ids = self.measure_ids.clone();
        let mut dimension_ids = self.dimension_ids.clone();
        let mut table_nodes = self.table_nodes.clone();
        let mut local_field_ids = self.local_field_ids.clone();
        let mut groups: Vec<Arc<PathGroup<'b>>> = self.groups.clone();
        for &p in added {
            let key = (p.cost, semantic.path_signature(p));
            let at = paths.partition_point(|q| (q.cost, semantic.path_signature(q)) <= key);
            paths.insert(at, p);
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
            if p.target_table_id == self.root.id {
                local_field_ids.insert(p.field_id);
            }
            // Its group, again in ranked order; a join path not seen yet
            // opens a group of its own.
            let members: Vec<&'b Path> = paths.iter().copied().filter(|q| q.joins == p.joins).collect();
            match groups.iter().position(|g| g.join_path == p.joins) {
                Some(i) => {
                    let g = &groups[i];
                    groups[i] = Arc::new(PathGroup {
                        join_path: g.join_path.clone(),
                        paths: members,
                        join_cost: g.join_cost,
                        route: g.route.clone(),
                    });
                }
                None => {
                    let (join_cost, route) = route_of(semantic, &p.joins);
                    groups.push(Arc::new(PathGroup { join_path: p.joins.clone(), paths: members, join_cost, route }));
                }
            }
        }
        Base {
            root: self.root,
            datasource: self.datasource,
            paths,
            field_ids,
            field_set,
            measure_ids,
            dimension_ids,
            table_nodes,
            local_field_ids,
            identity: self.identity.clone(),
            groups,
        }
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
    pub fn identity(&self) -> &str {
        &self.identity
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
    fn identity(&self) -> String;
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
    fn identity(&self) -> String {
        self.identity.clone()
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
fn route_of(semantic: &SemanticModel, join_path: &[Id]) -> (i64, String) {
    let joins: Vec<_> = join_path.iter().filter_map(|j| semantic.join(*j)).collect();
    let join_cost = joins.iter().map(|j| j.cost).sum();
    let route = joins
        .iter()
        .map(|j| semantic.table(j.right_id).map(|t| t.uid.clone()).unwrap_or_else(|| j.right_id.to_string()))
        .collect::<Vec<_>>()
        .join(">");
    (join_cost, route)
}

impl<'a> Subtree<'a> {
    pub fn new(semantic: &SemanticModel, base: Arc<Base<'a>>, join_path: Vec<Id>, paths: Vec<&'a Path>) -> Self {
        let (join_cost, route) = route_of(semantic, &join_path);
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
    fn identity(&self) -> String {
        format!("{}:[{}]", self.base.identity, self.route)
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
    /// Universes over the paths stored with the snapshot.
    pub fn build(semantic: &'a SemanticModel) -> Self {
        Self::build_from(semantic, &semantic.snapshot().paths)
    }

    /// Universes over the given paths, formed or stored.
    pub fn build_from(semantic: &'a SemanticModel, all_paths: &'a [Path]) -> Self {
        let snap = semantic.snapshot();
        let bases = snap
            .tables
            .iter()
            .map(|t| {
                let paths = all_paths.iter().filter(|p| p.root_table_id == t.id).collect();
                Arc::new(Base::new(semantic, t, paths))
            })
            .collect();
        Universes { bases }
    }

    /// These universes with more paths laid over them, for a layered
    /// model: a root the paths start from gets a universe of its own,
    /// built over its paths and the new ones, and every other universe is
    /// shared as it is.
    pub fn layered<'b>(&self, semantic: &'b SemanticModel, extra: &'b [Path]) -> Universes<'b>
    where
        'a: 'b,
    {
        let bases = self
            .bases
            .iter()
            .map(|base| -> Arc<Base<'b>> {
                let added: Vec<&'b Path> = extra.iter().filter(|p| p.root_table_id == base.root.id).collect();
                if added.is_empty() {
                    return base.clone();
                }
                Arc::new(base.with_paths(semantic, &added))
            })
            .collect();
        Universes { bases }
    }

    pub fn for_root(&self, table_id: Id) -> Option<Arc<Base<'a>>> {
        self.bases.iter().find(|b| b.root.id == table_id).cloned()
    }

    /// Datasource ids holding an expression for every one of the measures.
    pub fn datasources_with_all_measures(&self, semantic: &SemanticModel, measures: &[&Field]) -> Vec<Id> {
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
                    self.bases.iter().filter(|b| b.datasource.id == *ds).any(|b| semantic.expression_for(b.root.id, m.id).is_some())
                })
            })
            .collect()
    }
}
