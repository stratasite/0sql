//! The planner: a universe, a query definition and an optional principal in,
//! SQL for the datasource the query resolves to out.
//!
//! The pipeline mirrors strata's `Planner.plan`: resolve a universe, build
//! the DAG, run the visitors in order, then assemble the final statement.
//! Visitors not yet ported report [`PlanError::Unimplemented`] when the
//! query needs them, so the golden scoreboard stays honest.

pub mod dag;
pub mod model;
pub mod profile;
pub mod pruner;
pub mod resolver;
pub mod sql;
pub mod universe;
pub mod visitors;

use zsql_dialect::Dialects;
use zsql_spec::SecurityContext;
use zsql_semantic::Universe;

#[derive(Debug, Clone)]
pub struct Plan {
    pub sql: String,
    /// The datasource the plan executes in: its name, its uid and its adapter.
    pub datasource: String,
    pub datasource_uid: String,
    pub adapter: String,
    /// The plan explained: phase timings and the node graph. Asked for
    /// with [`Planner::plan_query_explained`].
    pub explain: Option<Explain>,
}

/// How a plan came to be: what each planning phase cost and the DAG the
/// visitors shaped, node by node.
#[derive(Debug, Clone, serde::Serialize)]
pub struct Explain {
    pub phases: Vec<Phase>,
    /// Live nodes in build (post) order; the last one is the root.
    pub nodes: Vec<ExplainNode>,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct Phase {
    pub name: &'static str,
    pub us: u128,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExplainNode {
    pub id: usize,
    /// The CTE name in the SQL.
    pub alias: String,
    /// `query`, `aggregation` or `merge`.
    pub kind: &'static str,
    pub root: bool,
    /// The table the node reads, for query and aggregation nodes.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub table: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub datasource: Option<String>,
    /// Join paths the node walks from its table.
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub paths: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub purpose: Option<&'static str>,
    /// The temporal transform a comparison node serves.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub transform: Option<String>,
    /// Imported from a segment's plan.
    pub segment: bool,
    pub projections: Vec<String>,
    pub filters: Vec<String>,
    /// Inputs read through the FROM clause.
    pub inputs: Vec<usize>,
    /// Inputs attached by a strategy (segments, exclusions, expansions).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub strategies: Vec<ExplainStrategy>,
    #[serde(skip_serializing_if = "String::is_empty")]
    pub join_type: String,
    pub group_by: bool,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub security_filters: Vec<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub security_masks: Vec<String>,
    /// The string the alias hashes from.
    pub identity: String,
}

#[derive(Debug, Clone, serde::Serialize)]
pub struct ExplainStrategy {
    pub node: usize,
    pub kind: String,
}

impl Plan {
    pub fn sql_with_limit(&self, limit: Option<u64>) -> String {
        match limit {
            Some(n) => format!("{}\nLIMIT {n}", self.sql),
            None => self.sql.clone(),
        }
    }
}

/// Planning errors, named after the Rails classes the goldens expect.
#[derive(Debug, thiserror::Error)]
pub enum PlanError {
    #[error("not implemented: {0}")]
    Unimplemented(&'static str),
    #[error("{0}")]
    Resolution(String),
    #[error("{0}")]
    SecurityPolicy(String),
    /// A segment key reachable only in another datasource; resolution retries there.
    #[error("{0}")]
    SegmentDatasource(String),
    #[error("{0}")]
    Dialect(#[from] zsql_dialect::DialectError),
}

impl PlanError {
    /// The Rails exception class this error corresponds to.
    pub fn rails_class(&self) -> &'static str {
        match self {
            PlanError::Resolution(_) => "Planner::ResolutionError",
            PlanError::SegmentDatasource(_) => "Planner::SegmentDatasourceError",
            PlanError::SecurityPolicy(_) => "Planner::SecurityPolicyError",
            PlanError::Unimplemented(_) | PlanError::Dialect(_) => "Unimplemented",
        }
    }
}

pub type Result<T> = std::result::Result<T, PlanError>;

/// Plans a typed query, as spec lowering or a stored model produces it.
pub fn plan_query_with<'a>(
    universe: &'a Universe,
    paths: Option<&'a [zsql_semantic::Path]>,
    dialects: &Dialects,
    query: zsql_spec::Query,
    principal: Option<&SecurityContext>,
) -> Result<Plan> {
    Planner::new(universe, dialects, paths).plan_query(query, principal)
}

/// A planner over one universe: the per-root path indexes are built once
/// and every plan reuses them. A server keeps one per loaded branch.
pub struct Planner<'a> {
    universe: &'a Universe,
    dialects: &'a Dialects,
    universes: universe::Universes<'a>,
}

impl<'a> Planner<'a> {
    /// Over the given join paths, or the snapshot's stored ones when `None`.
    pub fn new(universe: &'a Universe, dialects: &'a Dialects, paths: Option<&'a [zsql_semantic::Path]>) -> Self {
        let universes = match paths {
            Some(p) => universe::Universes::build_from(universe, p),
            None => universe::Universes::build(universe),
        };
        Planner { universe, dialects, universes }
    }

    pub fn universe(&self) -> &'a Universe {
        self.universe
    }

    /// The per-root path indexes this planner resolves over.
    pub fn universes(&self) -> &universe::Universes<'a> {
        &self.universes
    }

    pub fn plan_query(&self, query: zsql_spec::Query, principal: Option<&SecurityContext>) -> Result<Plan> {
        self.plan_query_explained(query, principal, false)
    }

    /// As `plan_query`; with `explain` the plan also carries every
    /// node's alias and the identity string it hashes from.
    pub fn plan_query_explained(&self, query: zsql_spec::Query, principal: Option<&SecurityContext>, explain: bool) -> Result<Plan> {
        let rquery = model::RQuery::resolve(self.universe, query)?;
        model::check_supported(&rquery, self.universe, principal)?;
        match run_plan(self.universe, &self.universes, self.dialects, &rquery, principal, vec![], explain) {
            // A segment keyed in another datasource: resolve again with every key required.
            Err(PlanError::SegmentDatasource(_)) => {
                let required = visitors::segmentation::all_key_dimensions(&rquery);
                run_plan(self.universe, &self.universes, self.dialects, &rquery, principal, required, explain)
            }
            other => other,
        }
    }
}

/// Rails `Planner.run_plan`: resolve, then the visitors in order.
fn run_plan<'a>(
    universe: &'a Universe,
    universes: &universe::Universes<'a>,
    dialects: &Dialects,
    rquery: &model::RQuery<'a>,
    principal: Option<&SecurityContext>,
    required_dims: Vec<&'a zsql_semantic::Field>,
    explain: bool,
) -> Result<Plan> {
    let opts = resolver::Options { required_dims, ..Default::default() };
    let mut phases: Vec<Phase> = Vec::new();
    macro_rules! phase {
        ($name:literal, $e:expr) => {{
            let _t = profile::span($name);
            let start = explain.then(std::time::Instant::now);
            let out = $e;
            if let Some(start) = start {
                phases.push(Phase { name: $name, us: start.elapsed().as_micros() });
            }
            out
        }};
    }
    let mut graph = phase!("resolve", resolver::resolve_with(universe, universes, dialects, rquery, &opts)?);

    // A segment planned directly is a population preview: a criteria-only
    // segment resolves to a plain query node whose keys would repeat.
    if rquery.is_segment() {
        let root = graph.root();
        if !graph.nodes[root].is_agg() && !graph.nodes[root].is_merge() {
            graph.nodes[root].should_group_by = Some(true);
        }
    }

    phase!("segment_fork", visitors::segment_fork::run(&mut graph, universe, universes, rquery)?);
    phase!("complex_measure", visitors::complex_measure::run(&mut graph, universe, universes, dialects, rquery)?);
    phase!("exclusion", visitors::exclusion::run(&mut graph, universe, rquery)?);
    phase!("inclusion", visitors::inclusion::run(&mut graph, universe, rquery)?);
    phase!("snapshot", visitors::snapshot::run(&mut graph, universe, rquery)?);
    phase!("contribution", visitors::contribution::run(&mut graph, universe, rquery)?);
    phase!("temporal", visitors::temporal::run(&mut graph, universe, rquery)?);
    phase!("top_n", visitors::top_n::run(&mut graph, universe, universes, dialects, rquery)?);
    phase!("segment", visitors::segment::run(&mut graph, universe, universes, dialects, rquery)?);
    if let Some(user) = principal {
        phase!("security", visitors::security::run(&mut graph, universe, user)?);
    }
    phase!("reference", visitors::reference::run(&mut graph, universe, rquery)?);
    phase!("alias", visitors::alias::run(&mut graph)?);
    phase!("sql", visitors::sql::run(&graph, universe, rquery)?);
    let sql = phase!("final_query", visitors::final_query::to_sql(&graph)?);
    let explain = explain.then(|| Explain { nodes: explain_nodes(&graph), phases });

    Ok(Plan { sql, datasource: graph.datasource.name.clone(), datasource_uid: graph.datasource.uid.clone(), adapter: graph.datasource.adapter.clone(), explain })
}

fn explain_nodes(graph: &dag::Graph) -> Vec<ExplainNode> {
    let order = graph.post_order();
    order
        .iter()
        .map(|&id| {
            let n = &graph.nodes[id];
            let base = n.universe.as_ref();
            ExplainNode {
                id,
                alias: n.alias(graph),
                kind: n.kind.name(),
                root: n.is_root(),
                table: base.map(|b| b.root.name.clone()),
                datasource: base.map(|b| b.datasource.uid.clone()),
                paths: n.subtrees.iter().map(|t| universe::Model::to_s(t)).collect(),
                purpose: n.purpose,
                transform: n.transform.clone(),
                segment: n.owner.is_some(),
                projections: n.projections.iter().map(|p| format!("{} AS {}", p.to_s(), p.alias)).collect(),
                filters: n.filters.iter().map(|f| f.to_s()).collect(),
                inputs: n.plain_inputs(),
                strategies: n.strategies.iter().map(|s| ExplainStrategy { node: s.node, kind: s.kind.name().to_string() }).collect(),
                join_type: if n.is_merge() { n.join_type.clone() } else { String::new() },
                group_by: n.should_group_by.unwrap_or(false),
                security_filters: n.security_filters.iter().map(|(f, allowed)| security_to_s(f, allowed)).collect(),
                security_masks: n.security_masks.iter().map(|(_, f, allowed, mask)| format!("{} mask {mask:?}", security_to_s(f, allowed))).collect(),
                identity: n.to_s(graph),
            }
        })
        .collect()
}

fn security_to_s(f: &zsql_semantic::Field, allowed: &Option<Vec<String>>) -> String {
    match allowed {
        Some(v) => format!("[{}] in {v:?}", f.name),
        None => format!("[{}] denied", f.name),
    }
}
