//! The plan DAG: nodes are sub-queries (a table-backed Query or Aggregation,
//! or a Merge over other nodes), edges are inputs. Component wrappers
//! (`Projected`, `Filtered`) carry node-local state over shared records.

use crate::model::{field_to_s, RFilter, RProjection};
use crate::universe::{Base, Model, Subtree};
use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;
use std::sync::Arc;
use zsql_dialect::Dialect;
use zsql_semantic::{Datasource, Expression, Field, Id, Predicate, Table};
use zsql_spec::Decorator;

pub type NodeId = usize;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NodeKind {
    Query,
    Aggregation,
    Merge,
}

impl NodeKind {
    pub fn class_name(self) -> &'static str {
        match self {
            NodeKind::Query => "Planner::Dag::Query",
            NodeKind::Aggregation => "Planner::Dag::Aggregation",
            NodeKind::Merge => "Planner::Dag::Merge",
        }
    }
    pub fn name(self) -> &'static str {
        match self {
            NodeKind::Query => "query",
            NodeKind::Aggregation => "aggregation",
            NodeKind::Merge => "merge",
        }
    }
    pub fn prefix(self) -> &'static str {
        match self {
            NodeKind::Query => "qr",
            NodeKind::Aggregation => "ag",
            NodeKind::Merge => "mg",
        }
    }
    /// First letter of the demodulized class, for table aliases (A0, Q1, M2).
    pub fn alias_letter(self) -> char {
        match self {
            NodeKind::Query => 'Q',
            NodeKind::Aggregation => 'A',
            NodeKind::Merge => 'M',
        }
    }
}

/// A projection as one node renders it.
#[derive(Debug, Clone)]
pub struct Projected<'a> {
    pub proj: Rc<RProjection<'a>>,
    pub alias: String,
    pub local_decorators: Vec<Decorator>,
}

impl<'a> Projected<'a> {
    pub fn new(proj: Rc<RProjection<'a>>) -> Self {
        let alias = proj.alias();
        Projected { proj, alias, local_decorators: vec![] }
    }
    pub fn field(&self) -> Option<&'a Field> {
        self.proj.field
    }
    pub fn is_measure(&self) -> bool {
        self.proj.is_measure()
    }
    pub fn is_dimension(&self) -> bool {
        self.proj.is_dimension()
    }
    pub fn is_calculation(&self) -> bool {
        self.proj.is_calculation()
    }
    pub fn decorators(&self) -> Vec<&Decorator> {
        self.proj.def.decorators.iter().chain(self.local_decorators.iter()).collect()
    }
    pub fn to_s(&self) -> String {
        self.proj.to_s()
    }
}

/// A filter as one node applies it; values may be overridden node-locally.
#[derive(Debug, Clone)]
pub struct Filtered<'a> {
    pub filter: Rc<RFilter<'a>>,
    pub filter_value: Option<String>,
    pub filter_value_end: Option<String>,
    pub predicate: Predicate,
}

impl<'a> Filtered<'a> {
    pub fn new(filter: Rc<RFilter<'a>>) -> Self {
        let filter_value = filter.def.filter_value.clone();
        let filter_value_end = filter.def.filter_value_end.clone();
        let predicate = filter.def.predicate;
        Filtered { filter, filter_value, filter_value_end, predicate }
    }
    pub fn field(&self) -> &'a Field {
        self.filter.field
    }
    pub fn fid(&self) -> usize {
        self.filter.fid
    }
    pub fn is_measure(&self) -> bool {
        self.filter.is_measure()
    }
    pub fn is_dimension(&self) -> bool {
        self.filter.is_dimension()
    }
    pub fn is_date(&self) -> bool {
        self.filter.is_date()
    }
    /// Rails `Filtered#to_s`; part of node identity.
    pub fn to_s(&self) -> String {
        format!("<f:{}:{}:{}>", field_to_s(self.field()), self.predicate.name(), self.filter_value.clone().unwrap_or_default())
    }
}

/// What a reference stands for.
#[derive(Debug, Clone)]
pub enum Entity<'a> {
    Proj(Rc<RProjection<'a>>),
    Field(&'a Field),
}

impl<'a> Entity<'a> {
    pub fn field(&self) -> Option<&'a Field> {
        match self {
            Entity::Proj(p) => p.field,
            Entity::Field(f) => Some(f),
        }
    }
}

/// The formula a multi reference evaluates: a semantic expression on a
/// table, or a calculation the query carries.
#[derive(Debug, Clone)]
pub enum MultiExpr<'a> {
    Semantic(&'a Expression),
    Calc(Rc<RProjection<'a>>),
}

impl<'a> MultiExpr<'a> {
    pub fn sql(&self) -> String {
        match self {
            MultiExpr::Semantic(e) => e.sql.clone().unwrap_or_default(),
            MultiExpr::Calc(c) => c.def.sql.clone().unwrap_or_default(),
        }
    }
    pub fn is_semantic(&self, e: &Expression) -> bool {
        matches!(self, MultiExpr::Semantic(x) if x.id == e.id)
    }
    pub fn projection_refs(&self) -> &[String] {
        match self {
            MultiExpr::Semantic(_) => &[],
            MultiExpr::Calc(c) => &c.def.projection_refs,
        }
    }
}

/// How a node reads a component: from a table it joins, from an input
/// node's column, from another projection of its own, or composed.
#[derive(Debug, Clone)]
pub enum Reference<'a> {
    Table { entity: Entity<'a>, expression: &'a Expression, table: &'a Table, greedy: Vec<&'a Expression> },
    Node { entity: Entity<'a>, node: NodeId },
    Proj { proj: Rc<RProjection<'a>> },
    Multi { entity: Entity<'a>, expression: Option<MultiExpr<'a>>, sources: Vec<Reference<'a>> },
}

impl<'a> Reference<'a> {
    pub fn node_refs(&self) -> Vec<&Reference<'a>> {
        match self {
            Reference::Multi { sources, .. } => sources.iter().filter(|s| matches!(s, Reference::Node { .. })).collect(),
            _ => vec![],
        }
    }
    pub fn expression(&self) -> Option<&'a Expression> {
        match self {
            Reference::Table { expression, .. } => Some(expression),
            Reference::Multi { expression: Some(MultiExpr::Semantic(e)), .. } => Some(e),
            _ => None,
        }
    }
    pub fn entity(&self) -> Option<&Entity<'a>> {
        match self {
            Reference::Table { entity, .. } | Reference::Node { entity, .. } | Reference::Multi { entity, .. } => Some(entity),
            Reference::Proj { .. } => None,
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum RefKey {
    Proj(usize),
    Field(Id),
}

#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub enum AliasKey {
    Table(Id),
    Node(NodeId),
    /// An unnested array field gets an alias of its own (Rails aliases the field record).
    Field(Id),
}

/// How a node integrates into a parent's FROM other than as a plain input.
#[derive(Debug, Clone)]
pub enum StrategyKind<'a> {
    /// Join the node onto a known table of the parent, field to projection.
    SegmentOnTable { table: &'a Table, join_type: String, map: Vec<(&'a Field, usize)> },
    /// Intersect the parent with the node on every projected dimension,
    /// optionally keeping only the node's first `limit` rows.
    SegmentByJoinKeys { limit: Option<String> },
    /// Anti-join: keep parent rows with no match in the node.
    ExcludeByJoinKeys,
    /// Join on the keys only and carry the node's expanding columns in.
    ExpandByJoinKeys { sid: usize, left: bool },
}

/// The segment an imported node belongs to: its WHERE prunes against the
/// segment's own tree, and a custom tree shape is part of node identity.
#[derive(Debug)]
pub struct Owner {
    pub sid: usize,
    pub tree: crate::model::FilledTree,
    pub identity: String,
}

impl StrategyKind<'_> {
    pub fn name(&self) -> &'static str {
        match self {
            StrategyKind::SegmentOnTable { .. } => "segment_on_table",
            StrategyKind::SegmentByJoinKeys { .. } => "segment_by_join_keys",
            StrategyKind::ExcludeByJoinKeys => "exclude_by_join_keys",
            StrategyKind::ExpandByJoinKeys { .. } => "expand_by_join_keys",
        }
    }
}

#[derive(Debug, Clone)]
pub struct Strategy<'a> {
    pub node: NodeId,
    pub kind: StrategyKind<'a>,
}

#[derive(Debug)]
pub struct Node<'a> {
    pub id: NodeId,
    pub kind: NodeKind,
    /// A special-purpose node (snapshot date resolution) that fact-shaping
    /// visitors must leave alone.
    pub purpose: Option<&'static str>,
    /// Temporal comparison nodes remember which transform they serve.
    pub transform: Option<String>,
    /// Set on nodes imported from a segment's plan.
    pub owner: Option<Rc<Owner>>,
    /// Security masks by field id: (context dimension, allowed values, mask value).
    /// (field, context dimension, allowed values or None for denied, mask value).
    pub security_masks: Vec<(Id, &'a Field, Option<Vec<String>>, String)>,
    /// Security row filters: (context dimension, allowed values).
    /// (context dimension, allowed values or None for denied).
    pub security_filters: Vec<(&'a Field, Option<Vec<String>>)>,
    /// Inputs that attach through a strategy rather than the plain FROM.
    pub strategies: Vec<Strategy<'a>>,
    pub projections: Vec<Projected<'a>>,
    pub filters: Vec<Filtered<'a>>,
    pub inputs: Vec<NodeId>,
    pub outputs: Vec<NodeId>,
    pub should_group_by: Option<bool>,
    pub should_order_by: Option<bool>,
    pub universe: Option<Arc<Base<'a>>>,
    pub subtrees: Vec<Subtree<'a>>,
    pub field_refs: HashMap<RefKey, Reference<'a>>,
    pub join_type: String,
    /// Destroyed nodes keep their slot so ids stay stable.
    pub removed: bool,
    table_aliases: RefCell<Vec<(AliasKey, String)>>,
    pub sql: RefCell<Option<String>>,
    /// A row limit a strategy imposes on this node's statement.
    pub limit: RefCell<Option<String>>,
}

impl<'a> Node<'a> {
    pub fn with_universe(
        kind: NodeKind,
        universe: Arc<Base<'a>>,
        projections: Vec<Rc<RProjection<'a>>>,
        filters: Vec<Rc<RFilter<'a>>>,
    ) -> Self {
        let mut n = Node::bare(kind, projections, filters);
        n.universe = Some(universe);
        n.should_group_by = Some(kind == NodeKind::Aggregation);
        n
    }

    pub fn merge(projections: Vec<Rc<RProjection<'a>>>, filters: Vec<Rc<RFilter<'a>>>, join_type: String) -> Self {
        let mut n = Node::bare(NodeKind::Merge, projections, filters);
        n.join_type = join_type;
        n.should_group_by = Some(false);
        n
    }

    fn bare(kind: NodeKind, projections: Vec<Rc<RProjection<'a>>>, filters: Vec<Rc<RFilter<'a>>>) -> Self {
        Node {
            id: 0,
            kind,
            purpose: None,
            transform: None,
            owner: None,
            security_masks: vec![],
            security_filters: vec![],
            strategies: vec![],
            projections: projections.into_iter().map(Projected::new).collect(),
            filters: filters.into_iter().map(Filtered::new).collect(),
            inputs: vec![],
            outputs: vec![],
            should_group_by: None,
            should_order_by: None,
            universe: None,
            subtrees: vec![],
            field_refs: HashMap::new(),
            join_type: "full".into(),
            removed: false,
            table_aliases: RefCell::new(vec![]),
            sql: RefCell::new(None),
            limit: RefCell::new(None),
        }
    }

    /// The node's statement with any strategy-imposed limit, rendered the
    /// way this dialect takes one.
    pub fn final_sql(&self, dialect: &zsql_dialect::Dialect) -> Result<String, zsql_dialect::DialectError> {
        let s = self.sql.borrow().clone().unwrap_or_default();
        match self.limit.borrow().as_ref() {
            Some(n) => dialect.apply_limit(&s, 0, n),
            None => Ok(s),
        }
    }

    pub fn is_root(&self) -> bool {
        !self.removed && self.outputs.is_empty()
    }

    pub fn is_agg(&self) -> bool {
        self.kind == NodeKind::Aggregation
    }

    /// Rails `Node#delete` for a projection.
    pub fn delete_projection(&mut self, pid: usize) {
        self.projections.retain(|p| p.proj.pid != pid);
    }

    pub fn delete_filter(&mut self, fid: usize) {
        self.filters.retain(|f| f.fid() != fid);
    }

    /// Rails `Node#delete(field)`: every projection and filter on the field.
    pub fn delete_field(&mut self, field_id: Id) {
        self.projections.retain(|p| p.field().map(|f| f.id != field_id).unwrap_or(true));
        self.filters.retain(|f| f.field().id != field_id);
    }

    pub fn is_strategy_input(&self, id: NodeId) -> bool {
        self.strategies.iter().any(|s| s.node == id)
    }

    /// Inputs that are not strategies.
    pub fn plain_inputs(&self) -> Vec<NodeId> {
        self.inputs.iter().copied().filter(|i| !self.is_strategy_input(*i)).collect()
    }

    /// Rails `Node#replace`: a new wrapper in the old one's seat.
    pub fn replace_projection(&mut self, pid: usize, with: Rc<RProjection<'a>>) {
        if let Some(i) = self.projections.iter().position(|p| p.proj.pid == pid) {
            self.projections[i] = Projected::new(with);
        }
    }

    pub fn add_projection(&mut self, proj: Rc<RProjection<'a>>) {
        if !self.has_proj(proj.pid) {
            self.projections.push(Projected::new(proj));
        }
    }

    /// Rails `Dag::Query#expression`: the field's expression on the cheapest
    /// path of the subtree holding it, adding the path when none does.
    pub fn expression(&mut self, universe: &'a zsql_semantic::Universe, field: &'a Field) -> crate::Result<&'a Expression> {
        let base = self.universe.clone().ok_or_else(|| crate::PlanError::Resolution("node has no universe".into()))?;
        let path = match self.subtrees.iter().find(|t| t.paths.iter().any(|p| p.field_id == field.id)) {
            Some(t) => t.path_for(field.id).expect("subtree holds field"),
            None => {
                let path = base.path_for(field.id).ok_or_else(|| {
                    crate::PlanError::Resolution(format!("Could not find table or path for {} with id: {}.", field_to_s(field), field.id))
                })?;
                match self.subtrees.iter_mut().find(|t| t.join_path == path.joins) {
                    Some(t) => t.push(path),
                    None => self.subtrees.push(Subtree::new(universe, base.clone(), path.joins.clone(), vec![path])),
                }
                path
            }
        };
        universe.expression_for(path.target_table_id, field.id).ok_or_else(|| {
            crate::PlanError::Resolution(format!(
                "Could not find expression for {} in the the target table {}.",
                field_to_s(field),
                path.target_table_id
            ))
        })
    }
    pub fn is_merge(&self) -> bool {
        self.kind == NodeKind::Merge
    }

    /// Rails `Node#group_by?` is unused by SqlVisitor, which reads should_group_by directly.
    pub fn group_by(&self) -> bool {
        self.should_group_by.unwrap_or(false)
    }

    pub fn order_by(&self) -> bool {
        self.should_order_by.unwrap_or(self.is_root())
    }

    pub fn where_filters(&self) -> Vec<&Filtered<'a>> {
        if self.group_by() {
            self.filters.iter().filter(|f| !f.is_measure()).collect()
        } else {
            self.filters.iter().collect()
        }
    }

    pub fn having_filters(&self) -> Vec<&Filtered<'a>> {
        if self.group_by() {
            self.filters.iter().filter(|f| f.is_measure()).collect()
        } else {
            vec![]
        }
    }

    /// Dimension fields from projections (calculations excluded) then filters.
    pub fn dimensions(&self) -> Vec<&'a Field> {
        let mut out = Vec::new();
        for p in &self.projections {
            if p.is_dimension() && !p.is_calculation() {
                if let Some(f) = p.field() {
                    out.push(f);
                }
            }
        }
        for f in &self.filters {
            if f.is_dimension() {
                out.push(f.field());
            }
        }
        out
    }

    pub fn measures(&self) -> Vec<&'a Field> {
        let mut out = Vec::new();
        for p in &self.projections {
            if p.is_measure() && !p.is_calculation() {
                if let Some(f) = p.field() {
                    out.push(f);
                }
            }
        }
        for f in &self.filters {
            if f.is_measure() {
                out.push(f.field());
            }
        }
        out
    }

    pub fn has_proj(&self, pid: usize) -> bool {
        self.projections.iter().any(|p| p.proj.pid == pid)
    }

    pub fn has_field(&self, id: Id) -> bool {
        self.projections.iter().any(|p| p.field().map(|f| f.id == id).unwrap_or(false))
            || self.filters.iter().any(|f| f.field().id == id)
            || self.projections.iter().filter(|p| p.is_calculation()).any(|p| p.proj.field_refs.iter().any(|f| f.id == id))
    }

    pub fn projected(&self, pid: usize) -> Option<&Projected<'a>> {
        self.projections.iter().find(|p| p.proj.pid == pid)
    }

    pub fn get_ref(&self, pid: usize, field: Option<Id>) -> Option<&Reference<'a>> {
        self.field_refs.get(&RefKey::Proj(pid)).or_else(|| field.and_then(|f| self.field_refs.get(&RefKey::Field(f))))
    }

    pub fn table_aliased(&self, key: &AliasKey) -> bool {
        self.table_aliases.borrow().iter().any(|(k, _)| k == key)
    }

    /// Rails `Node#table_alias`: first letter of the thing's class plus the
    /// count of aliases so far, assigned on first ask.
    pub fn table_alias(&self, key: AliasKey, letter: char) -> String {
        if let Some((_, a)) = self.table_aliases.borrow().iter().find(|(k, _)| *k == key) {
            return a.clone();
        }
        let mut aliases = self.table_aliases.borrow_mut();
        let alias = format!("{}{}", letter, aliases.len());
        aliases.push((key, alias.clone()));
        alias
    }

    /// Rails `Node#to_s`, the string every alias hashes from.
    pub fn to_s(&self, graph: &Graph<'a>) -> String {
        let projections: Vec<String> = self.projections.iter().map(|p| format!("{} -> {}", p.to_s(), p.alias)).collect();
        let mut filters: Vec<String> = self.filters.iter().map(|f| f.to_s()).collect();
        filters.sort();
        let mut s =
            format!("{} \nprojections: \n\t{} \nfilters: \n\t{}", self.kind.class_name(), projections.join("\n\t"), filters.join("\n\t"));
        if let Some(o) = &self.owner {
            s.push_str(&o.identity);
        }
        match self.kind {
            NodeKind::Query | NodeKind::Aggregation => {
                let mut subs: Vec<String> = self.subtrees.iter().map(|t| t.to_s()).collect();
                subs.sort();
                s.push_str("\nsubtrees:  \n\t");
                s.push_str(&subs.join(" \n\t"));
            }
            NodeKind::Merge => {
                let mut ins: Vec<String> = self.inputs.iter().map(|i| graph.nodes[*i].alias(graph)).collect();
                ins.sort();
                s.push_str("\nnodes:  \n\t");
                s.push_str(&ins.join(" \n\t"));
            }
        }
        s
    }

    /// Rails `Node#alias`: temp table prefix, node prefix, md5 of `to_s`.
    /// A node imported from a segment is prefixed `seg` whatever its kind.
    pub fn alias(&self, graph: &Graph<'a>) -> String {
        let digest = md5::compute(self.to_s(graph).as_bytes());
        let prefix = if self.owner.is_some() { "seg" } else { self.kind.prefix() };
        format!("{}{}{:x}", graph.dialect.temp_table_prefix(), prefix, digest)
    }

    /// The inputs in FROM build order: most dimensions first, then most
    /// projections, then cheapest. Remaining ties break on projection order,
    /// the earliest measure the input carries in the query's own list, so
    /// the preserved side of a left final-pass join is the first measure the
    /// author asked for. Rails breaks the same tie on the CTE hash. The
    /// identity string is the last resort, for determinism.
    pub fn ordered_inputs(&self, graph: &Graph<'a>) -> Vec<NodeId> {
        let mut ins = self.plain_inputs();
        ins.sort_by_cached_key(|i| {
            let n = &graph.nodes[*i];
            let cost: i64 = -n.subtrees.iter().map(|s| s.cost()).sum::<i64>();
            let first_measure =
                n.projections.iter().filter(|p| p.is_measure()).filter_map(|p| p.proj.def.ordinal).min().unwrap_or(i64::MAX);
            (
                -(n.projections.iter().filter(|p| p.is_dimension()).count() as i64),
                -(n.projections.len() as i64),
                cost,
                first_measure,
                n.to_s(graph),
            )
        });
        ins
    }

    /// Rails `Node#from_clause_join_defs`: the joins the FROM walks, most
    /// expensive first, unique.
    pub fn from_clause_join_defs(&self, universe: &'a zsql_semantic::Universe) -> Vec<&'a zsql_semantic::JoinDef> {
        let mut jds: Vec<&zsql_semantic::JoinDef> =
            self.subtrees.iter().flat_map(|s| s.join_path.iter()).filter_map(|j| universe.join(*j)).collect();
        jds.sort_by_cached_key(|j| {
            let left = universe.table(j.left_id);
            let right = universe.table(j.right_id);
            (
                -j.cost,
                -left.map(|t| t.cost).unwrap_or(0),
                -right.map(|t| t.cost).unwrap_or(0),
                left.map(|t| t.name.clone()).unwrap_or_default(),
                right.map(|t| t.name.clone()).unwrap_or_default(),
            )
        });
        let mut seen = std::collections::HashSet::new();
        jds.retain(|j| seen.insert(j.id));
        jds
    }
}

/// Node ids inside references move with an imported graph.
fn remap_reference(r: &mut Reference, base: usize) {
    match r {
        Reference::Node { node, .. } => *node += base,
        Reference::Multi { sources, .. } => sources.iter_mut().for_each(|s| remap_reference(s, base)),
        _ => {}
    }
}

pub struct Graph<'a> {
    pub nodes: Vec<Node<'a>>,
    pub datasource: &'a Datasource,
    /// Shared with every sub-graph the visitors cut from this one.
    pub dialect: std::sync::Arc<Dialect>,
}

impl<'a> Graph<'a> {
    pub fn new(datasource: &'a Datasource, dialect: std::sync::Arc<Dialect>) -> Self {
        Graph { nodes: vec![], datasource, dialect }
    }

    pub fn add_node(&mut self, mut node: Node<'a>) -> NodeId {
        node.id = self.nodes.len();
        self.nodes.push(node);
        self.nodes.len() - 1
    }

    pub fn add_edge(&mut self, from: NodeId, to: NodeId) {
        if !self.nodes[to].inputs.contains(&from) {
            self.nodes[to].inputs.push(from);
        }
        if !self.nodes[from].outputs.contains(&to) {
            self.nodes[from].outputs.push(to);
        }
    }

    /// An edge whose input attaches through a strategy.
    pub fn add_strategy_edge(&mut self, strategy: Strategy<'a>, to: NodeId) {
        let from = strategy.node;
        self.add_edge(from, to);
        self.nodes[to].strategies.push(strategy);
    }

    pub fn root(&self) -> NodeId {
        self.nodes.iter().find(|n| n.is_root()).map(|n| n.id).expect("graph has a root")
    }

    pub fn live(&self) -> impl Iterator<Item = &Node<'a>> {
        self.nodes.iter().filter(|n| !n.removed)
    }

    /// Rails `Graph#destroy_node`: the node and every edge touching it.
    pub fn destroy_node(&mut self, id: NodeId) {
        self.nodes[id].removed = true;
        self.nodes[id].inputs.clear();
        self.nodes[id].outputs.clear();
        for n in self.nodes.iter_mut() {
            n.inputs.retain(|i| *i != id);
            n.outputs.retain(|o| *o != id);
        }
    }

    /// Moves another graph's nodes in, renumbering ids; returns the new id of its root.
    pub fn import(&mut self, other: Graph<'a>) -> NodeId {
        let other_root = other.root();
        let base = self.nodes.len();
        let mut root_id = None;
        for mut n in other.nodes {
            let old = n.id;
            n.id = base + old;
            n.inputs = n.inputs.iter().map(|i| base + i).collect();
            n.outputs = n.outputs.iter().map(|o| base + o).collect();
            for s in n.strategies.iter_mut() {
                s.node += base;
            }
            for r in n.field_refs.values_mut() {
                remap_reference(r, base);
            }
            if old == other_root {
                root_id = Some(n.id);
            }
            self.nodes.push(n);
        }
        root_id.expect("imported graph root")
    }

    /// Node ids in post-order from the root: inputs before the node.
    pub fn post_order(&self) -> Vec<NodeId> {
        let mut out = Vec::new();
        let mut visited = vec![false; self.nodes.len()];
        fn go(g: &Graph, id: NodeId, visited: &mut Vec<bool>, out: &mut Vec<NodeId>) {
            if visited[id] {
                return;
            }
            visited[id] = true;
            for i in g.nodes[id].inputs.clone() {
                go(g, i, visited, out);
            }
            out.push(id);
        }
        go(self, self.root(), &mut visited, &mut out);
        out
    }
}
