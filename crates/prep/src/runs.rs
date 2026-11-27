//! Runs: a graph the application sends, checked whole, its query
//! sources pulled as its inputs say under the context it was sent with,
//! run node by node, and its output kept as the dataset's next version.
//! Also the dry run, which checks and says what a run would do, and the
//! preview, which runs part of a graph over a sample and keeps nothing.

use crate::error::{Error, Result};
use crate::graph::{Code, Graph, Node, Problem, Read};
use crate::record::{self, InputPlan, NodeRecord, ReadRef, RunRecord, RunState, Will};
use crate::source::{Kind, Query};
use crate::store::{self, Dataset, Materialization, Snapshot, Source};
use crate::{
    evict, lineage, lock, pipeline, recovery, schema, sha256, source_read, time, Arrived, Column, ColumnType, Lineage, Prep, Scope, State,
    RUNS_KEPT,
};
use std::collections::{BTreeMap, HashMap};
use std::path::PathBuf;
use zsql_datasource::Batches;

/// How many rows of each input a preview reads, unless asked for all.
const PREVIEW_SAMPLE: usize = 1000;

/// A run, as the application asks for it.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct RunRequest {
    /// The application's id for it: sent again, it answers what the
    /// first did.
    pub id: String,
    /// The dataset whose output it makes.
    pub dataset: String,
    pub graph: Graph,
}

/// A query source a run or a preview pulls before it reads.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Pull {
    pub source: String,
    pub name: String,
    pub query: Query,
    /// The nodes that read it.
    pub nodes: Vec<String>,
}

/// What each query source of a graph is read and pulled as.
///
/// A query snapshot is kept under the key of the statement its spec
/// planned to: the SQL, the context folded into it, and the datasource
/// it runs on. A run reads a snapshot only if it was taken by the same
/// statement its own spec plans to now, as the query cache answers only
/// the same SQL; a new deploy or another context that changes the SQL
/// pulls again. The server plans each query source before a run is
/// checked, so a spec that does not plan is refused before anything is
/// pulled.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct Keys {
    /// The hash of the context a run was sent with, for its record.
    pub context: String,
    /// Each query source, to the key of what it plans to, or why it does
    /// not plan.
    pub sources: BTreeMap<String, std::result::Result<String, String>>,
}

impl Keys {
    pub fn new(context: impl Into<String>) -> Keys {
        Keys { context: context.into(), sources: BTreeMap::new() }
    }

    /// A source's key, as planned.
    pub fn with(mut self, source: impl Into<String>, key: std::result::Result<String, String>) -> Keys {
        self.sources.insert(source.into(), key);
        self
    }
}

/// The key a query snapshot is kept under: of the statement and where it runs.
pub fn query_key(datasource: &str, sql: &str) -> String {
    sha256(format!("{datasource}\n{sql}").as_bytes())
}

/// A run under way: what it pulls, then what it runs. Handed to the
/// server, which pulls, and back to [`Prep::execute`].
pub struct Ticket {
    pub scope: Scope,
    pub id: String,
    dir: PathBuf,
    graph: Graph,
    keys: Keys,
    started: String,
    request: String,
    /// The pulls it waits on.
    pub pulls: Vec<Pull>,
    /// What each pull made, by source.
    pulled: HashMap<String, Took>,
}

/// What a pull made, for the run or preview that reads it: a snapshot
/// kept, or for a preview one held only while it runs.
#[derive(Clone)]
pub(crate) struct Took {
    snap: Snapshot,
    rows: PathBuf,
    kept: bool,
}

/// A directory a preview's pulls are held in while it runs, removed
/// when it is done.
struct Scratch(PathBuf);

impl Drop for Scratch {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// What asking for a run came to.
pub enum Begun {
    /// It started: pull, then execute.
    Started(Box<Ticket>),
    /// This run id was asked for already: its record, as it stands.
    Answered(Box<RunRecord>),
}

/// A dry run's answer: what a run would read and pull, each node's
/// columns, and what is wrong.
#[derive(Clone, Debug, serde::Serialize)]
pub struct Checked {
    pub ok: bool,
    pub reads: Vec<ReadRef>,
    pub inputs: Vec<InputPlan>,
    pub nodes: Vec<NodeColumns>,
    pub problems: Vec<Problem>,
    /// What a run would do, in words.
    pub said: String,
}

/// A node's columns, as compiled.
#[derive(Clone, Debug, serde::Serialize)]
pub struct NodeColumns {
    pub id: String,
    pub kind: String,
    pub columns: Vec<Column>,
}

/// A preview, as asked for.
#[derive(Clone, Debug, serde::Deserialize)]
pub struct PreviewRequest {
    pub graph: Graph,
    /// The node to stop at; the output when none.
    #[serde(default)]
    pub node: Option<String>,
    /// Every row of each input, not a sample.
    #[serde(default)]
    pub whole: bool,
    #[serde(default)]
    pub limit: Option<u64>,
}

/// A preview's answer, kept nowhere.
pub struct Previewed {
    pub node: String,
    pub columns: Vec<Column>,
    pub rows: Batches,
    pub nodes: Vec<NodeRecord>,
    pub problems: Vec<Problem>,
    pub reads: Vec<ReadRef>,
    pub inputs: Vec<InputPlan>,
    /// How many rows of each input it read, at most, when it sampled.
    pub sample: Option<usize>,
}

/// Whether inputs are resolved for a run or a preview.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Mode {
    Run,
    Preview,
}

/// A graph's inputs, resolved: what each node reads, what must be
/// pulled first, and what is wrong.
struct Resolved {
    loaded: HashMap<usize, pipeline::Loaded>,
    inputs: Vec<InputPlan>,
    pulls: Vec<Pull>,
    problems: Vec<Problem>,
    /// What each source node read, for the lineage.
    sources: HashMap<usize, (Source, Snapshot)>,
    /// What each dataset node read.
    datasets: HashMap<usize, (Dataset, u32)>,
    /// Whether the columns of some input are only a guess until it is
    /// pulled.
    provisional: bool,
}

fn empty_rows() -> Batches {
    Batches { schema: std::sync::Arc::new(arrow_schema::Schema::empty()), batches: Vec::new() }
}

fn reads_of(graph: &Graph) -> Vec<ReadRef> {
    graph.reads().into_iter().map(|(kind, id)| ReadRef { kind: kind.to_string(), id }).collect()
}

impl Prep {
    /// What each input node of `order` reads, as `read` says, for a run
    /// with the query keys `keys` holds. `pulled` is what a run's
    /// pulls made, source to snapshot.
    fn resolve(&self, scope: &Scope, graph: &Graph, order: &[usize], keys: &Keys, mode: Mode, pulled: &HashMap<String, Took>) -> Resolved {
        let mut r = Resolved {
            loaded: HashMap::new(),
            inputs: Vec::new(),
            pulls: Vec::new(),
            problems: Vec::new(),
            sources: HashMap::new(),
            datasets: HashMap::new(),
            provisional: false,
        };
        for &k in order {
            match &graph.nodes[k] {
                Node::Source { id, source, read, types, .. } => {
                    self.resolve_source(scope, &mut r, k, (id, source, *read, types), (keys, mode), pulled)
                }
                Node::Dataset { id, dataset, read, .. } => self.resolve_dataset(scope, &mut r, k, (id, dataset, *read)),
                _ => {}
            }
        }
        r
    }

    fn resolve_dataset(&self, scope: &Scope, r: &mut Resolved, k: usize, (id, dataset, read): (&str, &str, Read)) {
        let (dir, d) = match self.find(scope, dataset) {
            Ok(x) => x,
            Err(_) => {
                r.problems.push(Problem::new(
                    Some(id),
                    Code::NotFound,
                    format!("it reads dataset {dataset}, which is not in this project"),
                ));
                return;
            }
        };
        let v = match read {
            Read::Latest => match d.live {
                Some(v) => v,
                None => {
                    r.problems.push(Problem::new(Some(id), Code::NotFound, format!("dataset {} ({dataset}) has no output yet", d.name)));
                    return;
                }
            },
            Read::Version(v) => v,
            Read::Pull | Read::MaxAge(_) => {
                r.problems.push(Problem::new(
                    Some(id),
                    Code::Read,
                    "a dataset is read as it is, at its latest or a version; it is not pulled",
                ));
                return;
            }
            Read::Snapshot(_) => {
                r.problems.push(Problem::new(Some(id), Code::Read, "a dataset has versions, not snapshots: read {\"version\": k}"));
                return;
            }
        };
        let Ok(m) = store::read_json::<Materialization>(&store::version_dir(&dir, v).join("manifest.json")) else {
            r.problems.push(Problem::new(Some(id), Code::NotFound, format!("dataset {} has no version {v}", d.name)));
            return;
        };
        let rows = match &m.pointer {
            Some(p) => store::snapshot_dir(&store::source_dir(&self.root(scope), &p.source), p.snapshot).join(store::ROWS),
            None => store::version_dir(&dir, v).join(store::ROWS),
        };
        r.inputs.push(InputPlan {
            node: id.to_string(),
            kind: "dataset".into(),
            id: dataset.to_string(),
            name: d.name.clone(),
            will: Will::Read,
            number: Some(v),
            taken: Some(m.made.clone()),
            age_secs: time::age_secs(&m.made),
            why: if matches!(read, Read::Latest) {
                format!("version {v}, its live one")
            } else {
                format!("version {v}, as the graph says")
            },
        });
        r.loaded.insert(
            k,
            pipeline::Loaded {
                rows,
                columns: m.columns.clone(),
                origin: format!("dataset:{dataset}"),
                types: BTreeMap::new(),
                said: format!("dataset {}, version {v}", d.name),
                previous: None,
            },
        );
        r.datasets.insert(k, (d, v));
    }

    #[allow(clippy::too_many_arguments)]
    fn resolve_source(
        &self,
        scope: &Scope,
        r: &mut Resolved,
        k: usize,
        (id, source, read, types): (&str, &str, Read, &BTreeMap<String, ColumnType>),
        (keys, mode): (&Keys, Mode),
        pulled: &HashMap<String, Took>,
    ) {
        let bad = |code: Code, m: String| Problem::new(Some(id), code, m);
        let (dir, s) = match self.find_source(scope, source) {
            Ok(x) => x,
            Err(_) => {
                r.problems.push(bad(Code::NotFound, format!("it reads source {source}, which is not in this project")));
                return;
            }
        };
        let manifest = |n: u32| store::read_json::<Snapshot>(&store::snapshot_dir(&dir, n).join("manifest.json")).ok();
        // A query source's snapshot is its own only when taken by the
        // statement its spec plans to now.
        let key = match (s.kind, keys.sources.get(source)) {
            (Kind::Query, Some(Ok(key))) => Some(key.as_str()),
            (Kind::Query, Some(Err(why))) => {
                r.problems.push(bad(Code::Plan, format!("source {} does not plan: {why}", s.name)));
                return;
            }
            (Kind::Query, None) => {
                r.problems.push(bad(Code::Plan, format!("source {} was not planned before the run", s.name)));
                return;
            }
            _ => None,
        };
        let mine = |snap: &Snapshot| snap.semantic.as_ref().is_some_and(|x| Some(x.context.as_str()) == key);
        let newest_mine = || (1..=s.snapshot).rev().filter_map(manifest).find(mine);
        // What it reads, or none when it is pulled first; and why.
        let pulled_here = pulled.get(source).filter(|_| !matches!(read, Read::Snapshot(_)));
        let (chosen, will, why): (Option<Snapshot>, Will, String) = if let Some(t) = pulled_here {
            let why = if t.kept { "pulled for this run" } else { "pulled for this preview, and kept nowhere" };
            (Some(t.snap.clone()), Will::Pull, why.into())
        } else {
            match (s.kind, read) {
                (_, Read::Version(_)) => {
                    r.problems.push(bad(Code::Read, "a source has snapshots, not versions: read {\"snapshot\": n}".into()));
                    return;
                }
                (_, Read::Snapshot(n)) => match manifest(n) {
                    Some(snap) if s.kind == Kind::Query && !mine(&snap) => {
                        r.problems.push(bad(
                            Code::Context,
                            format!(
                                "snapshot {n} of {} was taken by another statement than its spec plans to now (another context or deploy); a run reads only its own",
                                s.name
                            ),
                        ));
                        return;
                    }
                    Some(snap) => (Some(snap), Will::Read, format!("snapshot {n}, as the graph says")),
                    None => {
                        r.problems.push(bad(Code::NotFound, format!("source {} has no snapshot {n} kept", s.name)));
                        return;
                    }
                },
                (Kind::Query, read) => match (read, newest_mine(), mode) {
                    (Read::Pull, _, Mode::Run) => (None, Will::Pull, "the graph says to pull it first".into()),
                    (Read::MaxAge(max), Some(snap), Mode::Run) => {
                        let a = time::age_secs(&snap.taken).unwrap_or(u64::MAX);
                        if a > max {
                            (
                                None,
                                Will::Pull,
                                format!("its newest snapshot of this statement is {} old, more than {}", record::age(a), record::age(max)),
                            )
                        } else {
                            let n = snap.number;
                            (Some(snap), Will::Read, format!("snapshot {n}, taken by this statement {} ago", record::age(a)))
                        }
                    }
                    (_, Some(snap), _) => {
                        let n = snap.number;
                        (Some(snap), Will::Read, format!("snapshot {n}, the newest taken by this statement"))
                    }
                    (_, None, _) if s.snapshot == 0 => (None, Will::Pull, "it has never been pulled".into()),
                    (_, None, _) => {
                        let last = manifest(s.snapshot).and_then(|m| time::age_secs(&m.taken));
                        let ago = last.map(|a| format!(", last pulled {} ago", record::age(a))).unwrap_or_default();
                        (None, Will::Pull, format!("its snapshots were taken by other statements (another context or deploy){ago}"))
                    }
                },
                (_, Read::Pull) => {
                    let how = if s.kind == Kind::Upload { "send it a new file" } else { "post it new rows" };
                    r.problems.push(bad(Code::Read, format!("{} is not pulled: {how}", s.name)));
                    return;
                }
                (_, read) => {
                    let Some(snap) = (s.snapshot > 0).then(|| manifest(s.snapshot)).flatten() else {
                        r.problems.push(bad(Code::NotFound, format!("source {} has no snapshot yet", s.name)));
                        return;
                    };
                    let a = time::age_secs(&snap.taken).unwrap_or(0);
                    let why = match read {
                        Read::MaxAge(max) if a > max => format!(
                            "its newest snapshot, {} old, more than {}; it is not pulled, so it is read as it is",
                            record::age(a),
                            record::age(max)
                        ),
                        _ => format!("snapshot {}, its newest", snap.number),
                    };
                    (Some(snap), Will::Read, why)
                }
            }
        };
        if will == Will::Pull && chosen.is_none() {
            let Some(query) = s.query.clone() else {
                r.problems.push(bad(Code::Read, format!("{} has no query to pull", s.name)));
                return;
            };
            match r.pulls.iter_mut().find(|p| p.source == s.id) {
                Some(p) => p.nodes.push(id.to_string()),
                None => r.pulls.push(Pull { source: s.id.clone(), name: s.name.clone(), query, nodes: vec![id.to_string()] }),
            }
        }
        r.inputs.push(InputPlan {
            node: id.to_string(),
            kind: "source".into(),
            id: s.id.clone(),
            name: s.name.clone(),
            will,
            number: chosen.as_ref().map(|c| c.number),
            taken: chosen.as_ref().map(|c| c.taken.clone()),
            age_secs: chosen.as_ref().and_then(|c| time::age_secs(&c.taken)),
            why,
        });
        // Until a pull is made, the newest snapshot of any context says
        // what its columns likely are, for checking what reads them.
        let provisional = chosen.is_none();
        let snap = chosen.or_else(|| (s.snapshot > 0).then(|| manifest(s.snapshot)).flatten());
        let Some(snap) = snap else {
            r.provisional = true;
            return;
        };
        r.provisional |= provisional;
        let mut all_types = s.types.clone();
        all_types.extend(types.iter().map(|(c, t)| (c.clone(), *t)));
        let previous = (snap.number > 1)
            .then(|| manifest(snap.number - 1))
            .flatten()
            .map(|p| (store::snapshot_dir(&dir, p.number).join(store::ROWS), p.columns, p.number));
        let age = time::age_secs(&snap.taken).map(|a| format!(", taken {} ago", record::age(a))).unwrap_or_default();
        let said = match &snap.file_name {
            Some(f) => format!("{f} (source {}, snapshot {}{age})", s.name, snap.number),
            None => format!("source {} (snapshot {}{age})", s.name, snap.number),
        };
        let rows = match pulled_here {
            Some(t) => t.rows.clone(),
            None => store::snapshot_dir(&dir, snap.number).join(store::ROWS),
        };
        r.loaded
            .insert(k, pipeline::Loaded { rows, columns: snap.columns.clone(), origin: s.id.clone(), types: all_types, said, previous });
        if !provisional && pulled_here.is_none_or(|t| t.kept) {
            r.sources.insert(k, (s, snap));
        }
    }

    fn compile(
        &self,
        graph: &Graph,
        order: &[usize],
        loaded: &HashMap<usize, pipeline::Loaded>,
        pinned_at: &str,
    ) -> Result<std::result::Result<Vec<pipeline::Planned>, Vec<Problem>>> {
        let describe = |inputs: &[(String, Vec<(String, ColumnType)>)], q: &str| crate::sql::describe(&self.engine, inputs, q);
        match pipeline::compile(graph, order, loaded, pinned_at, &describe) {
            Ok(p) => Ok(Ok(p)),
            Err(mut p) => {
                pipeline::overlaps(&self.engine, &mut p, loaded)?;
                Ok(Err(p))
            }
        }
    }

    /// Every query source a graph's nodes read, with its query: what the
    /// server plans, under the run's context, into [`Keys`] before the
    /// graph is checked. A source not found is left for the check to say.
    pub fn query_sources(&self, scope: &Scope, graph: &Graph) -> Vec<(String, Query)> {
        let mut out: Vec<(String, Query)> = Vec::new();
        for n in &graph.nodes {
            if let Node::Source { source, .. } = n {
                if out.iter().any(|(s, _)| s == source) {
                    continue;
                }
                if let Ok((_, s)) = self.find_source(scope, source) {
                    if let (Kind::Query, Some(q)) = (s.kind, s.query) {
                        out.push((s.id, q));
                    }
                }
            }
        }
        out
    }

    /// A dry run: what a run of `graph` would read and pull under the
    /// query keys `keys` holds, each node's columns, and every
    /// problem, with no pull and nothing written.
    pub fn check(&self, scope: &Scope, graph: &Graph, keys: &Keys) -> Result<Checked> {
        let order = match graph.order(None) {
            Ok(o) => o,
            Err(problems) => return Ok(checked_with(graph, Vec::new(), Vec::new(), Vec::new(), problems)),
        };
        let r = self.resolve(scope, graph, &order, keys, Mode::Run, &HashMap::new());
        let mut problems = r.problems;
        let nodes = match self.compile(graph, &order, &r.loaded, &time::now())? {
            Ok(planned) => planned
                .iter()
                .map(|p| NodeColumns {
                    id: graph.nodes[p.node].id().into(),
                    kind: graph.nodes[p.node].kind().into(),
                    columns: p.out.columns.clone(),
                })
                .collect(),
            Err(p) => {
                problems.extend(p);
                Vec::new()
            }
        };
        Ok(checked_with(graph, r.inputs, r.pulls, nodes, problems))
    }

    /// The pulls a preview waits on: its query sources with no snapshot
    /// taken by the statement `keys` holds for each.
    pub fn preview_pulls(&self, scope: &Scope, request: &PreviewRequest, keys: &Keys) -> Result<Vec<Pull>> {
        let order = request.graph.order(request.node.as_deref()).map_err(Error::Problems)?;
        Ok(self.resolve(scope, &request.graph, &order, keys, Mode::Preview, &HashMap::new()).pulls)
    }

    /// A graph run up to a node, over a sample of each input unless
    /// asked for all, kept nowhere: what the application shows while a
    /// pipeline is being built. `pulled` is the rows the pulls it waited
    /// on returned, by source: read while it runs, and kept nowhere.
    pub fn preview(&self, scope: &Scope, request: &PreviewRequest, keys: &Keys, pulled: Vec<(String, Arrived)>) -> Result<Previewed> {
        let graph = &request.graph;
        let order = graph.order(request.node.as_deref()).map_err(Error::Problems)?;
        let scratch = Scratch(self.store.data_dir().join(".previews").join(format!("{}", time::nanos())));
        let mut took = HashMap::new();
        for (source, arrived) in pulled {
            let (_, s) = self.find_source(scope, &source)?;
            let session = self.engine.session()?;
            let taken = crate::snapshot::from_rows(&session, &arrived.rows, &arrived.roles, &s.columns, self.settings.max_rows)?;
            let rows = scratch.0.join(format!("{source}.arrow"));
            let bytes = store::write_rows(&rows, &taken.rows)?;
            let snap = Snapshot {
                source: s.id.clone(),
                number: s.snapshot + 1,
                kind: s.kind,
                taken: time::now(),
                file_name: None,
                sha256: String::new(),
                bytes,
                parse: taken.parse,
                semantic: Some(arrived.read),
                origin: None,
                columns: taken.columns,
                rows: taken.rows.num_rows() as u64,
                changes: taken.changes,
            };
            took.insert(source, Took { snap, rows, kept: false });
        }
        let r = self.resolve(scope, graph, &order, keys, Mode::Preview, &took);
        let reads = reads_of(graph);
        let node = request.node.clone().or_else(|| graph.output().map(|n| n.id().to_string())).unwrap_or_default();
        let empty = |problems: Vec<Problem>, nodes: Vec<NodeRecord>| Previewed {
            node: node.clone(),
            columns: Vec::new(),
            rows: empty_rows(),
            nodes,
            problems,
            reads: reads.clone(),
            inputs: r.inputs.clone(),
            sample: None,
        };
        if !r.problems.is_empty() {
            return Ok(empty(r.problems, Vec::new()));
        }
        if let Some(p) = r.pulls.first() {
            let m = format!("{} is to be pulled first", p.name);
            return Ok(empty(vec![Problem::new(Some(&p.nodes[0]), Code::Read, m)], Vec::new()));
        }
        let planned = match self.compile(graph, &order, &r.loaded, &time::now())? {
            Ok(p) => p,
            Err(p) => return Ok(empty(p, Vec::new())),
        };
        let sample = (!request.whole).then_some(PREVIEW_SAMPLE);
        let session = self.engine.session()?;
        let done = pipeline::execute(&self.engine, &session, graph, &planned, &r.loaded, &pipeline::Options { sample, profile: true })?;
        if !done.problems.is_empty() {
            return Ok(empty(done.problems, done.records));
        }
        let limit = request.limit.unwrap_or(100).min(10_000);
        let rel = done.last.ok_or_else(|| Error::Internal("a preview made nothing".into()))?;
        let list: Vec<String> = rel.columns.iter().map(|c| schema::ident(&c.name)).collect();
        let rows = session.query(&format!("SELECT {} FROM {} LIMIT {limit}", list.join(", "), rel.name))?;
        Ok(Previewed { node, columns: rel.columns, rows, nodes: done.records, problems: Vec::new(), reads, inputs: r.inputs, sample })
    }

    /// Starts a run: checks the request whole, and records it as
    /// running. A run id sent again answers the first's record; another
    /// request under that id is refused. One run per dataset at a time.
    /// What it must pull first is on the ticket, for the server to pull
    /// and hand back to [`Prep::execute`].
    pub fn begin(&self, scope: &Scope, request: RunRequest, keys: &Keys) -> Result<Begun> {
        if !crate::is_run_id(&request.id) {
            return Err(Error::Invalid(format!("'{}' is not a run id: 1 to 128 letters, digits, '_', '-', '.' or ':'", request.id)));
        }
        let root = self.root(scope);
        let asked = sha256(&serde_json::to_vec(&(&request.dataset, &request.graph, keys))?);
        let path = store::run_path(&root, &request.id);
        if let Ok(r) = store::read_json::<RunRecord>(&path) {
            if r.request != asked {
                return Err(Error::Conflict(format!(
                    "run {} was asked for already, with another graph, dataset or context; a new run takes a new id",
                    request.id
                )));
            }
            return Ok(Begun::Answered(Box::new(r)));
        }
        if self.is_stopping() {
            return Err(Error::Stopping("the server is stopping; try again in a moment".into()));
        }
        let (dir, _) = self.find(scope, &request.dataset)?;
        let order = request.graph.order(None).map_err(Error::Problems)?;
        let r = self.resolve(scope, &request.graph, &order, keys, Mode::Run, &HashMap::new());
        if !r.problems.is_empty() {
            return Err(Error::Problems(r.problems));
        }
        // What reads a source not pulled yet is checked again after the pull.
        if let Err(p) = self.compile(&request.graph, &order, &r.loaded, &time::now())? {
            if !r.provisional {
                return Err(Error::Problems(p));
            }
        }
        {
            let mut running = lock(&self.running);
            if let Some((_, other)) = running.get(&dir) {
                return Err(Error::Conflict(format!(
                    "dataset {} is being made by run {other}; try again when it is done",
                    request.dataset
                )));
            }
            running.insert(dir.clone(), (scope.clone(), request.id.clone()));
        }
        let started = time::now();
        let pulling: Vec<String> = r.pulls.iter().map(|p| p.name.clone()).collect();
        let record = RunRecord {
            id: request.id.clone(),
            dataset: request.dataset.clone(),
            state: RunState::Running,
            started: started.clone(),
            ended: None,
            request: asked.clone(),
            context: keys.context.clone(),
            reads: reads_of(&request.graph),
            inputs: r.inputs,
            version: None,
            nodes: Vec::new(),
            problems: Vec::new(),
            error: None,
            summary: if pulling.is_empty() { "Running.".into() } else { format!("Pulling {}.", record::listed(&pulling)) },
            interrupted: None,
        };
        if let Err(e) = store::write_json(&path, &record) {
            lock(&self.running).remove(&dir);
            return Err(e);
        }
        let _ = std::fs::remove_file(dir.join(recovery::INTERRUPTED));
        Ok(Begun::Started(Box::new(Ticket {
            scope: scope.clone(),
            id: request.id,
            dir,
            graph: request.graph,
            keys: keys.clone(),
            started,
            request: asked,
            pulls: r.pulls,
            pulled: HashMap::new(),
        })))
    }

    /// The rows a ticket's pull returned, kept as a snapshot of its
    /// source, for the run to read.
    pub fn pulled_for(&self, ticket: &mut Ticket, source: &str, arrived: Arrived) -> Result<()> {
        let (_, snap) = self.pulled(&ticket.scope, source, arrived)?;
        let (dir, _) = self.find_source(&ticket.scope, source)?;
        let rows = store::snapshot_dir(&dir, snap.number).join(store::ROWS);
        ticket.pulled.insert(source.to_string(), Took { snap, rows, kept: true });
        Ok(())
    }

    /// Ends a run that failed before it ran, such as a pull that did
    /// not succeed.
    pub fn fail(&self, ticket: Ticket, why: &str) -> RunRecord {
        let mut r = self.record_of(&ticket);
        // Past a stop's grace the record says suspended, and the next
        // start says it was interrupted by the stop.
        if self.is_stopped() {
            return self.suspend(&ticket, r);
        }
        r.state = RunState::Failed;
        r.error = Some(why.to_string());
        r.summary = format!("Failed: {why}");
        self.finish(&ticket, r)
    }

    /// A run a stop cut short: recorded suspended, which the next start
    /// says was interrupted by the stop, and its dataset let go.
    fn suspend(&self, ticket: &Ticket, mut r: RunRecord) -> RunRecord {
        if r.state == RunState::Running {
            r.state = RunState::Suspended;
            let _ = store::write_json(&store::run_path(&self.root(&ticket.scope), &ticket.id), &r);
        }
        lock(&self.running).remove(&ticket.dir);
        r
    }

    fn record_of(&self, ticket: &Ticket) -> RunRecord {
        let path = store::run_path(&self.root(&ticket.scope), &ticket.id);
        store::read_json(&path).unwrap_or_else(|_| RunRecord {
            id: ticket.id.clone(),
            dataset: ticket.dir.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default(),
            state: RunState::Running,
            started: ticket.started.clone(),
            ended: None,
            request: ticket.request.clone(),
            context: ticket.keys.context.clone(),
            reads: reads_of(&ticket.graph),
            inputs: Vec::new(),
            version: None,
            nodes: Vec::new(),
            problems: Vec::new(),
            error: None,
            summary: String::new(),
            interrupted: None,
        })
    }

    /// Writes how a run ended, and lets its dataset go.
    fn finish(&self, ticket: &Ticket, mut r: RunRecord) -> RunRecord {
        r.ended = Some(time::now());
        let root = self.root(&ticket.scope);
        if let Err(e) = store::write_json(&store::run_path(&root, &ticket.id), &r) {
            tracing::warn!("run {} could not be recorded: {e}", ticket.id);
        }
        self.last_run(&ticket.dir, &r);
        lock(&self.running).remove(&ticket.dir);
        evict(&root.join(store::RUNS), RUNS_KEPT);
        tracing::info!("run {} of {} in {}/{}: {:?}", r.id, r.dataset, ticket.scope.org, ticket.scope.project, r.state);
        r
    }

    /// Runs a started run, its pulls made: the graph checked again over
    /// what they made, run node by node, and its output kept as the
    /// dataset's next version, whole or not at all. How it ended is its
    /// record; a run a stop cut short stays suspended.
    pub fn execute(&self, ticket: Ticket) -> RunRecord {
        let mut r = self.record_of(&ticket);
        match self.run_ticket(&ticket, &mut r) {
            Ok(()) => {}
            Err(_) if self.is_stopped() => return self.suspend(&ticket, self.record_of(&ticket)),
            Err(Error::Problems(p)) => {
                r.state = RunState::Failed;
                r.summary = format!("Failed: {}", p.iter().map(Problem::said).collect::<Vec<_>>().join("; "));
                r.problems = p;
            }
            Err(e) => {
                r.state = RunState::Failed;
                r.summary = format!("Failed: {e}");
                r.error = Some(e.to_string());
            }
        }
        self.finish(&ticket, r)
    }

    fn run_ticket(&self, ticket: &Ticket, r: &mut RunRecord) -> Result<()> {
        let graph = &ticket.graph;
        let order = graph.order(None).map_err(Error::Problems)?;
        let res = self.resolve(&ticket.scope, graph, &order, &ticket.keys, Mode::Run, &ticket.pulled);
        r.inputs = res.inputs.clone();
        if !res.problems.is_empty() {
            return Err(Error::Problems(res.problems));
        }
        if let Some(p) = res.pulls.first() {
            let m = format!("{} was to be pulled first, and was not", p.name);
            return Err(Error::Problems(vec![Problem::new(Some(&p.nodes[0]), Code::Read, m)]));
        }
        let pinned_at = time::now();
        let planned = self.compile(graph, &order, &res.loaded, &pinned_at)?.map_err(Error::Problems)?;
        let session = self.engine.session()?;
        let done =
            pipeline::execute(&self.engine, &session, graph, &planned, &res.loaded, &pipeline::Options { sample: None, profile: true })?;
        r.nodes = done.records;
        if !done.problems.is_empty() {
            return Err(Error::Problems(done.problems));
        }
        let (_, dataset) = self.find(&ticket.scope, &r.dataset)?;
        if done.held {
            r.state = RunState::Held;
            let failed: Vec<String> = r
                .nodes
                .last()
                .map(|n| {
                    n.checks.iter().filter(|c| !c.passed).map(|c| format!("{} {}", c.columns.join(", "), check_name(c.check))).collect()
                })
                .unwrap_or_default();
            r.summary = match dataset.live {
                Some(v) => format!("Held: {} failed; version {v} is still live.", failed.join(", ")),
                None => format!("Held: {} failed; the dataset has no output yet.", failed.join(", ")),
            };
            return Ok(());
        }
        let out = done.last.ok_or_else(|| Error::Internal("a run made no output".into()))?;
        let list: Vec<String> = out.columns.iter().map(|c| schema::ident(&c.name)).collect();
        let rows = session.query(&format!("SELECT {} FROM {}", list.join(", "), out.name))?;
        let n = rows.num_rows();
        if n > self.settings.max_rows {
            return Err(Error::Unprocessable(format!(
                "the graph makes {n} rows, more than the {} a dataset holds",
                self.settings.max_rows
            )));
        }
        // A source read as it is, to an output: the snapshot is the output.
        let pointer = match (graph.nodes.len(), res.sources.values().next(), res.loaded.values().next()) {
            (2, Some((s, snap)), Some(l)) if l.types.is_empty() && res.sources.len() == 1 => {
                Some(store::Pointer { source: s.id.clone(), snapshot: snap.number })
            }
            _ => None,
        };
        let k = dataset.made + 1;
        let mut notes = done.warnings;
        for node in &r.nodes {
            for (state, what) in [
                (State::Orphaned, "found no row"),
                (State::Ambiguous, "found several rows"),
                (State::Flagged, "were changed upstream since"),
            ] {
                let ids: Vec<&str> = node.edits.iter().filter(|o| o.state == state).map(|o| o.id.as_str()).collect();
                if !ids.is_empty() {
                    notes.push(format!("{} edit(s) of node {} {what}: {}", ids.len(), node.id, ids.join(", ")));
                }
            }
        }
        let mut sources: Vec<(&usize, &(Source, Snapshot))> = res.sources.iter().collect();
        sources.sort_by_key(|(k, _)| **k);
        let mut datasets: Vec<(&usize, &(Dataset, u32))> = res.datasets.iter().collect();
        datasets.sort_by_key(|(k, _)| **k);
        let lineage = Lineage {
            dataset: r.dataset.clone(),
            version: k,
            run: r.id.clone(),
            graph: sha256(&serde_json::to_vec(graph)?),
            sources: sources.iter().map(|(k, (s, snap))| source_read(graph.nodes[**k].id(), s, snap)).collect(),
            datasets: datasets
                .iter()
                .map(|(k, (d, v))| lineage::DatasetRead { id: d.id.clone(), version: *v, node: Some(graph.nodes[**k].id().to_string()) })
                .collect(),
            roles: out.columns.iter().map(|c| (c.name.clone(), c.role)).collect(),
            made_by: self.made_by(),
            pinned_at: pinned_at.clone(),
            notes: notes.clone(),
            columns: out
                .columns
                .iter()
                .zip(&out.ancestry)
                .map(|(c, from)| lineage::Ancestry { column: c.name.clone(), from: from.clone() })
                .collect(),
        };
        r.state = RunState::Succeeded;
        r.version = Some(k);
        r.ended = Some(time::now());
        let mut names: Vec<String> = Vec::new();
        for i in &r.inputs {
            if !names.contains(&i.name) {
                names.push(i.name.clone());
            }
        }
        r.summary = format!(
            "Made version {k}: {}, {} columns, from {}{}.",
            record::rows(n as u64),
            out.columns.len(),
            record::listed(&names),
            if notes.is_empty() { String::new() } else { format!("; {}", notes.join("; ")) }
        );
        // Kept whole or not at all, the manifest pointing at it last.
        let target = store::version_dir(&ticket.dir, k);
        let staging = ticket.dir.join(format!(".v{k}.staging-{}", time::nanos()));
        if target.exists() {
            // Left by a run that did not finish: no manifest pointed at it.
            std::fs::remove_dir_all(&target).map_err(|e| Error::io(&target, e))?;
        }
        let kept = (|| -> Result<()> {
            let bytes = match &pointer {
                Some(_) => 0,
                None => store::write_rows(&staging.join(store::ROWS), &rows)?,
            };
            if bytes > self.settings.max_bytes {
                return Err(Error::Unprocessable(format!(
                    "the graph makes {bytes} bytes of rows, more than the {} a dataset holds",
                    self.settings.max_bytes
                )));
            }
            let m = Materialization {
                version: k,
                made: pinned_at.clone(),
                columns: out.columns.clone(),
                rows: n as u64,
                bytes,
                pointer: pointer.clone(),
                run: Some(r.id.clone()),
            };
            store::write_json(&staging.join("manifest.json"), &m)?;
            store::write_json(&staging.join("lineage.json"), &lineage)?;
            store::write_json(&staging.join("graph.json"), graph)?;
            store::write_json(&staging.join("run.json"), &*r)?;
            std::fs::rename(&staging, &target).map_err(|e| Error::io(&target, e))?;
            store::sync_dir(Some(&ticket.dir))
        })();
        if let Err(e) = kept {
            let _ = std::fs::remove_dir_all(&staging);
            r.version = None;
            return Err(e);
        }
        let path = ticket.dir.join(store::MANIFEST);
        let mut d: Dataset = store::read_json(&path)?;
        d.made = k;
        d.live = Some(k);
        d.columns = out.columns;
        d.rows = n as u64;
        d.notes = notes;
        d.updated = time::now();
        store::write_json(&path, &d)?;
        self.forget_blends(&ticket.scope.org, &ticket.scope.project, &r.dataset);
        self.backed_up(&ticket.dir);
        Ok(())
    }

    /// A run's record, in flight or done.
    pub fn run(&self, scope: &Scope, id: &str) -> Result<RunRecord> {
        if !crate::is_run_id(id) {
            return Err(Error::NotFound(format!("no run {id}")));
        }
        store::read_json(&store::run_path(&self.root(scope), id))
            .map_err(|_| Error::NotFound(format!("no run {id} in project {}", scope.project)))
    }
}

fn check_name(c: crate::graph::CheckKind) -> &'static str {
    match c {
        crate::graph::CheckKind::Unique => "unique",
        crate::graph::CheckKind::NotNull => "never empty",
    }
}

fn checked_with(graph: &Graph, inputs: Vec<InputPlan>, pulls: Vec<Pull>, nodes: Vec<NodeColumns>, problems: Vec<Problem>) -> Checked {
    if !problems.is_empty() {
        let said = format!("It would not run: {}.", problems.iter().map(Problem::said).collect::<Vec<_>>().join("; "));
        return Checked { ok: false, reads: reads_of(graph), inputs, nodes, problems, said };
    }
    let mut parts = Vec::new();
    for p in &pulls {
        let why = inputs.iter().find(|i| i.id == p.source).map(|i| i.why.clone()).unwrap_or_default();
        parts.push(format!("pulls {} ({why})", p.name));
    }
    for i in inputs.iter().filter(|i| i.will == Will::Read) {
        parts.push(format!("reads {} ({})", i.name, i.why));
    }
    let k = graph.nodes.len();
    parts.push(format!("runs {k} node{}", if k == 1 { "" } else { "s" }));
    let said = parts.join(", ");
    let said = format!("{}{}.", said[..1].to_uppercase(), &said[1..]);
    Checked { ok: true, reads: reads_of(graph), inputs, nodes, problems, said }
}
