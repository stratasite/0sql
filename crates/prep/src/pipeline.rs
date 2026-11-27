//! A graph, compiled and run: each node a table in one engine session,
//! over the tables of the nodes it reads, in an order where each comes
//! after what it reads.
//!
//! Compiling needs only the columns of what the inputs read, so a graph
//! is checked whole before anything is pulled or written: a column a
//! node reads that is not there, a step that is wrong, a join whose keys
//! do not meet, each said of its node. Running it counts what each node
//! did, for its record.

use crate::compile::{self, Chain, Cols, Describe, JoinSpec, Joined, Rel, StageSql};
use crate::edits::{Edit, Outcome};
use crate::engine::{Engine, Session};
use crate::error::{Error, Result};
use crate::graph::{CheckKind, Code, Contract, Graph, Node, OnFail, Problem, Suggestion};
use crate::lineage::Origin;
use crate::record::{self, AppendRecord, CheckResult, ColumnProfile, JoinRecord, NodeRecord, Retyped, StepRecord};
use crate::schema::{ident, Column, ColumnType};
use crate::steps::Step;
use arrow_array::RecordBatch;
use std::collections::{BTreeMap, HashMap, HashSet};
use std::path::PathBuf;
use std::time::Instant;

/// An input node's rows, as the run resolved it.
#[derive(Clone, Debug)]
pub(crate) struct Loaded {
    /// The rows file it loads.
    pub rows: PathBuf,
    pub columns: Vec<Column>,
    /// What its columns' ancestry names: a source's id, or `dataset:<id>`.
    pub origin: String,
    /// Columns read as other types: the source's, then the node's.
    pub types: BTreeMap<String, ColumnType>,
    /// What it read, in words: "budget.csv (source Budget, snapshot 3)".
    pub said: String,
    /// The snapshot before, for telling what a column gone became.
    pub previous: Option<(PathBuf, Vec<Column>, u32)>,
}

/// How a node is made.
pub(crate) enum Plan {
    /// An input's rows, loaded as `table`, then read as its types say.
    Load {
        table: String,
        chain: Option<Chain>,
    },
    Chain(Chain),
    Join {
        left: Rel,
        right: Rel,
        joined: Joined,
    },
    Append {
        stages: Vec<(String, String)>,
        only_in_some: Vec<String>,
    },
    Sql(crate::sql::SqlStage),
    Edits {
        input: Rel,
        key: Vec<String>,
        edits: Vec<Edit>,
    },
    Output {
        contracts: Vec<Contract>,
    },
}

/// A node, compiled: where it is in the graph, how it is made, and the
/// relation it makes.
pub(crate) struct Planned {
    pub node: usize,
    pub plan: Plan,
    pub out: Rel,
}

fn table(k: usize) -> String {
    format!("n{k}")
}

/// Compiles the nodes `order` names, each over the relations of the
/// nodes it reads; or every problem found. A node that cannot be
/// compiled is said once, and the nodes past it are passed over.
pub(crate) fn compile(
    graph: &Graph,
    order: &[usize],
    loaded: &HashMap<usize, Loaded>,
    pinned_at: &str,
    describe: Describe<'_>,
) -> std::result::Result<Vec<Planned>, Vec<Problem>> {
    let mut out: Vec<Planned> = Vec::new();
    let mut rels: HashMap<String, Rel> = HashMap::new();
    let mut problems = Vec::new();
    let mut broken: HashSet<String> = HashSet::new();
    for &k in order {
        let node = &graph.nodes[k];
        let id = node.id().to_string();
        if node.inputs().iter().any(|i| broken.contains(*i)) {
            broken.insert(id);
            continue;
        }
        match compile_node(k, node, &rels, loaded.get(&k), pinned_at, describe) {
            Ok((plan, rel)) => {
                rels.insert(id, rel.clone());
                out.push(Planned { node: k, plan, out: rel });
            }
            Err(mut p) => {
                broken.insert(id);
                problems.append(&mut p);
            }
        }
    }
    if problems.is_empty() {
        Ok(out)
    } else {
        Err(problems)
    }
}

/// A problem with a node's definition.
fn invalid(id: &str, e: Error) -> Vec<Problem> {
    let code = match e {
        Error::Invalid(_) => Code::Invalid,
        Error::Unprocessable(_) => Code::Data,
        _ => Code::Invalid,
    };
    vec![Problem::new(Some(id), code, e.to_string())]
}

/// A column a node reads that is not there, with the columns there that
/// may be the one meant.
pub(crate) fn missing(id: &str, column: &str, what: &str, there: &[Column]) -> Problem {
    let mut p = Problem::new(Some(id), Code::MissingColumn, format!("{what} reads column '{column}', which is not there"));
    p.column = Some(column.to_string());
    p.suggestions = similar(column, there);
    p
}

/// Columns whose names are near `name`: the same but for case,
/// spacing or punctuation, one inside the other, or a letter or two
/// apart.
fn similar(name: &str, there: &[Column]) -> Vec<Suggestion> {
    let norm = |s: &str| s.chars().filter(|c| c.is_alphanumeric()).collect::<String>().to_lowercase();
    let want = norm(name);
    let mut out: Vec<(usize, Suggestion)> = Vec::new();
    for c in there {
        let have = norm(&c.name);
        let because = if have == want {
            "the same name but for case or punctuation".to_string()
        } else if want.len() >= 3 && have.len() >= 3 && (have.contains(&want) || want.contains(&have)) {
            format!("its name has '{name}' in it, or is in '{name}'")
        } else {
            let d = distance(&want, &have);
            if d > 2 || d >= want.len() {
                continue;
            }
            format!("its name is {d} letter(s) from '{name}'")
        };
        out.push((distance(&want, &have), Suggestion { column: c.name.clone(), because }));
    }
    out.sort_by_key(|(d, _)| *d);
    out.into_iter().take(3).map(|(_, s)| s).collect()
}

/// Levenshtein's distance between two names.
fn distance(a: &str, b: &str) -> usize {
    let (a, b): (Vec<char>, Vec<char>) = (a.chars().collect(), b.chars().collect());
    let mut row: Vec<usize> = (0..=b.len()).collect();
    for i in 1..=a.len() {
        let mut prev = row[0];
        row[0] = i;
        for j in 1..=b.len() {
            let cur = row[j];
            row[j] = (row[j] + 1).min(row[j - 1] + 1).min(prev + usize::from(a[i - 1] != b[j - 1]));
            prev = cur;
        }
    }
    row[b.len()]
}

fn unresolved(id: &str, chain: &Chain, input: &Rel, what: &str) -> Vec<Problem> {
    chain
        .unresolved
        .iter()
        .map(|u| {
            let said = if u.step == 0 { what.to_string() } else { format!("step {} ({})", u.step, u.op) };
            missing(id, &u.column, &said, &input.columns)
        })
        .collect()
}

fn compile_node(
    k: usize,
    node: &Node,
    rels: &HashMap<String, Rel>,
    loaded: Option<&Loaded>,
    pinned_at: &str,
    describe: Describe<'_>,
) -> std::result::Result<(Plan, Rel), Vec<Problem>> {
    let id = node.id();
    let rel = |i: &str| {
        rels.get(i).cloned().ok_or_else(|| vec![Problem::new(Some(id), Code::Graph, format!("it reads '{i}', which was not made"))])
    };
    match node {
        Node::Source { .. } | Node::Dataset { .. } => {
            // Not found is said where inputs are resolved; a query source
            // never pulled has no columns until it is, and what reads it
            // is checked then.
            let l = loaded.ok_or_else(Vec::new)?;
            let loaded_as = format!("{}_in", table(k));
            let base = Rel {
                name: ident(&loaded_as),
                columns: l.columns.clone(),
                ancestry: l.columns.iter().map(|c| vec![Origin { source: l.origin.clone(), column: c.name.clone() }]).collect(),
            };
            if l.types.is_empty() {
                return Ok((Plan::Load { table: loaded_as, chain: None }, base));
            }
            let steps: Vec<Step> = l.types.iter().map(|(c, t)| Step::RetypeColumn { column: c.clone(), ty: *t, format: None }).collect();
            let chain = compile::chain(&table(k), &base, &steps, pinned_at, describe).map_err(|e| invalid(id, e))?;
            if !chain.unresolved.is_empty() {
                return Err(chain.unresolved.iter().map(|u| missing(id, &u.column, "the types it is read with", &base.columns)).collect());
            }
            let out = chain.out.clone();
            Ok((Plan::Load { table: loaded_as, chain: Some(chain) }, out))
        }
        Node::Steps { input, steps, .. } => {
            let input = rel(input)?;
            let chain = compile::chain(&table(k), &input, steps, pinned_at, describe).map_err(|e| invalid(id, e))?;
            if !chain.unresolved.is_empty() {
                return Err(unresolved(id, &chain, &input, "it"));
            }
            let out = chain.out.clone();
            Ok((Plan::Chain(chain), out))
        }
        Node::Join { left, right, on, how, columns, prefix, .. } => {
            let (l, r) = (rel(left)?, rel(right)?);
            let spec = JoinSpec { on, kind: *how, columns: columns.as_deref(), prefix: prefix.as_deref() };
            let gone = spec.missing(&l, &r);
            if !gone.is_empty() {
                return Err(gone
                    .into_iter()
                    .map(|(is_right, c)| {
                        let (side, there) =
                            if is_right { (format!("its right side, {right},"), &r) } else { (format!("its left side, {left},"), &l) };
                        let mut p = missing(id, &c, "the join", &there.columns);
                        p.message = format!("the join reads column '{c}' of {side} which is not there");
                        p
                    })
                    .collect());
            }
            let joined = compile::join(&l, &r, &format!("node {right}"), &spec).map_err(|e| invalid(id, e))?;
            let out = Rel { name: ident(&table(k)), columns: joined.out.columns.clone(), ancestry: joined.out.ancestry.clone() };
            Ok((Plan::Join { left: l, right: r, joined }, out))
        }
        Node::Append { inputs, .. } => {
            let rs: Vec<Rel> = inputs.iter().map(|i| rel(i)).collect::<std::result::Result<_, _>>()?;
            let mut cur = rs[0].clone();
            let mut stages = Vec::new();
            for (j, other) in rs.iter().enumerate().skip(1) {
                let (select, cols) = compile::append(&cur, other, &format!("node {}", inputs[j])).map_err(|e| invalid(id, e))?;
                let name = if j + 1 == rs.len() { table(k) } else { format!("{}_a{j}", table(k)) };
                stages.push((name.clone(), select));
                cur = Rel { name: ident(&name), columns: cols.columns, ancestry: cols.ancestry };
            }
            let only_in_some: Vec<String> = cur
                .columns
                .iter()
                .filter(|c| !rs.iter().all(|r| r.columns.iter().any(|x| x.name.eq_ignore_ascii_case(&c.name))))
                .map(|c| c.name.clone())
                .collect();
            let out = cur.clone();
            Ok((Plan::Append { stages, only_in_some }, out))
        }
        Node::Sql { inputs, query, .. } => {
            let query = crate::sql::checked(query).map_err(|e| invalid(id, e))?;
            let mut given = Vec::new();
            let mut all = Cols { columns: Vec::new(), ancestry: Vec::new() };
            for (alias, from) in inputs {
                let r = rel(from)?;
                let typed: Vec<(String, ColumnType)> = r.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
                given.push(crate::sql::SqlInput { alias: alias.clone(), relation: r.name.clone(), columns: typed });
                all.columns.extend(r.columns.iter().cloned());
                all.ancestry.extend(r.ancestry.iter().cloned());
            }
            let typed: Vec<(String, Vec<(String, ColumnType)>)> = given.iter().map(|g| (g.alias.clone(), g.columns.clone())).collect();
            let made = match describe(&typed, &query).map_err(|e| invalid(id, e))? {
                Ok(made) => made,
                Err(refusal) => {
                    return Err(match crate::sql::missing_column(&refusal) {
                        Some(c) => vec![missing(id, &c, "its query", &all.columns)],
                        None => vec![Problem::new(Some(id), Code::Invalid, refusal)],
                    })
                }
            };
            compile::sql_columns(&mut all, &made);
            let stage = crate::sql::SqlStage { label: format!("node {id}"), inputs: given, query, columns: made };
            Ok((Plan::Sql(stage), Rel { name: ident(&table(k)), columns: all.columns, ancestry: all.ancestry }))
        }
        Node::Edits { input, key, edits, .. } => {
            let input = rel(input)?;
            let has = |c: &str| input.columns.iter().any(|x| x.name.eq_ignore_ascii_case(c));
            let mut problems: Vec<Problem> = key.iter().filter(|c| !has(c)).map(|c| missing(id, c, "its key", &input.columns)).collect();
            for e in edits {
                for c in e.reads().iter().filter(|c| !has(c)) {
                    problems.push(missing(id, c, &format!("edit {}", e.id()), &input.columns));
                }
            }
            if !problems.is_empty() {
                return Err(problems);
            }
            crate::edits::check(edits, key, &input.columns).map_err(|e| invalid(id, e))?;
            let out = Rel { name: ident(&table(k)), ..input.clone() };
            Ok((Plan::Edits { input, key: key.clone(), edits: edits.clone() }, out))
        }
        Node::Output { input, roles, key, checks, .. } => {
            let input = rel(input)?;
            let has = |c: &str| input.columns.iter().any(|x| x.name.eq_ignore_ascii_case(c));
            let mut problems: Vec<Problem> = Vec::new();
            for c in roles.keys().filter(|c| !has(c)) {
                problems.push(missing(id, c, "a role it gives", &input.columns));
            }
            for c in key.iter().filter(|c| !has(c)) {
                problems.push(missing(id, c, "its key", &input.columns));
            }
            for (i, check) in checks.iter().enumerate() {
                if check.columns.is_empty() {
                    problems.push(Problem::new(Some(id), Code::Invalid, format!("check {} names no column", i + 1)));
                }
                for c in check.columns.iter().filter(|c| !has(c)) {
                    problems.push(missing(id, c, &format!("check {}", i + 1), &input.columns));
                }
            }
            if !problems.is_empty() {
                return Err(problems);
            }
            let mut out = input.clone();
            for (name, role) in roles {
                if let Some(c) = out.columns.iter_mut().find(|c| c.name.eq_ignore_ascii_case(name)) {
                    c.role = *role;
                }
            }
            let mut contracts: Vec<Contract> = Vec::new();
            if !key.is_empty() {
                contracts.push(Contract { check: CheckKind::Unique, columns: key.clone(), on_fail: OnFail::Hold });
                contracts.push(Contract { check: CheckKind::NotNull, columns: key.clone(), on_fail: OnFail::Hold });
            }
            contracts.extend(checks.iter().cloned());
            Ok((Plan::Output { contracts }, out))
        }
    }
}

/// How a graph is run.
pub(crate) struct Options {
    /// Rows of each input read, at most: a preview's sample.
    pub sample: Option<usize>,
    /// Whether each node's columns are profiled.
    pub profile: bool,
}

/// What a run of the compiled nodes came to.
pub(crate) struct Executed {
    pub records: Vec<NodeRecord>,
    /// What the data would not have, said of its node: the run made
    /// nothing.
    pub problems: Vec<Problem>,
    /// Whether an output check that holds failed.
    pub held: bool,
    /// What the run let through, with a warning.
    pub warnings: Vec<String>,
    /// The relation of the last node run.
    pub last: Option<Rel>,
}

/// The most columns of a node profiled.
const PROFILED: usize = 100;

/// Runs the compiled nodes in order in `session`.
pub(crate) fn execute(
    engine: &Engine,
    session: &Session,
    graph: &Graph,
    planned: &[Planned],
    loaded: &HashMap<usize, Loaded>,
    options: &Options,
) -> Result<Executed> {
    let mut done = Executed { records: Vec::new(), problems: Vec::new(), held: false, warnings: Vec::new(), last: None };
    let mut rows_of: HashMap<String, u64> = HashMap::new();
    for p in planned {
        let node = &graph.nodes[p.node];
        let id = node.id();
        let t0 = Instant::now();
        let mut rec = NodeRecord { id: id.to_string(), kind: node.kind().to_string(), ..Default::default() };
        rec.rows_in = node.inputs().iter().map(|i| rows_of.get(*i).copied().unwrap_or(0)).collect();
        let ran = run_node(engine, session, p, loaded.get(&p.node), options, &mut rec);
        match ran {
            Ok(()) => {}
            Err(e @ (Error::Unprocessable(_) | Error::Invalid(_))) => {
                let code = if matches!(e, Error::Invalid(_)) { Code::Invalid } else { Code::Data };
                rec.summary = format!("Failed: {e}");
                rec.ms = t0.elapsed().as_millis() as u64;
                done.records.push(rec);
                done.problems.push(Problem::new(Some(id), code, e.to_string()));
                return Ok(done);
            }
            Err(e) => return Err(e),
        }
        rec.rows = session.count(&format!("SELECT count(*) FROM {}", p.out.name))?;
        rec.columns = p.out.columns.len();
        if let Some(input) = node.inputs().first().and_then(|i| planned.iter().find(|q| graph.nodes[q.node].id() == *i)) {
            if !matches!(node, Node::Append { .. } | Node::Join { .. } | Node::Sql { .. }) {
                changed_columns(&input.out.columns, &p.out.columns, &mut rec);
            }
        } else if let Some(l) = loaded.get(&p.node) {
            // An input read with types of its own.
            changed_columns(&l.columns, &p.out.columns, &mut rec);
        }
        if let Plan::Output { contracts, .. } = &p.plan {
            for c in contracts {
                let r = contract(session, &p.out, c)?;
                if !r.passed {
                    let what = check_words(c);
                    match c.on_fail {
                        OnFail::Hold => done.held = true,
                        OnFail::Warn => done.warnings.push(format!("check failed, let through: {what} ({} failing)", record::n(r.failing))),
                    }
                }
                rec.checks.push(r);
            }
        }
        if options.profile {
            rec.profile = profile(session, &p.out)?;
        }
        rec.summary = summary(node, &rec, loaded.get(&p.node), options.sample.is_some());
        rec.ms = t0.elapsed().as_millis() as u64;
        rows_of.insert(id.to_string(), rec.rows);
        done.records.push(rec);
        done.last = Some(p.out.clone());
    }
    Ok(done)
}

/// The rows of a rows file, or its first `n`: a sample reads no batch
/// past them.
fn first_rows(path: &std::path::Path, n: Option<usize>) -> Result<Vec<RecordBatch>> {
    Ok(match n {
        Some(n) => crate::store::read_rows_range(path, 0, n)?.batches,
        None => crate::store::read_rows(path)?.batches,
    })
}

fn run_node(
    engine: &Engine,
    session: &Session,
    p: &Planned,
    loaded: Option<&Loaded>,
    options: &Options,
    rec: &mut NodeRecord,
) -> Result<()> {
    match &p.plan {
        Plan::Load { table, chain } => {
            let l = loaded.ok_or_else(|| Error::Internal("an input was not resolved".into()))?;
            let typed: Vec<(String, ColumnType)> = l.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
            session.load(table, &typed, &first_rows(&l.rows, options.sample)?)?;
            if let Some(chain) = chain {
                run_chain(engine, session, chain, None)?;
            }
        }
        Plan::Chain(chain) => run_chain(engine, session, chain, Some(rec))?,
        Plan::Join { left, right, joined } => {
            check(session, &joined.check)?;
            session.execute(&format!("CREATE TABLE {} AS {}", p.out.name, joined.select))?;
            let matched = session.count(&format!(
                "SELECT count(*) FROM {} l WHERE EXISTS (SELECT 1 FROM {} r WHERE {})",
                left.name, right.name, joined.on
            ))?;
            let left_rows = session.count(&format!("SELECT count(*) FROM {}", left.name))?;
            let unmatched_right = session.count(&format!(
                "SELECT count(*) FROM {} r WHERE NOT EXISTS (SELECT 1 FROM {} l WHERE {})",
                right.name, left.name, joined.on
            ))?;
            rec.join = Some(JoinRecord {
                matched,
                unmatched_left: left_rows - matched.min(left_rows),
                unmatched_right,
                sample: options.sample.is_some(),
            });
        }
        Plan::Append { stages, only_in_some, .. } => {
            for (name, select) in stages {
                session.execute(&format!("CREATE TABLE {} AS {select}", ident(name)))?;
            }
            rec.append = Some(AppendRecord { only_in_some: only_in_some.clone() });
        }
        Plan::Sql(stage) => {
            let name = p.out.name.trim_matches('"').to_string();
            crate::sql::run(engine, session, &name, stage)?;
        }
        Plan::Edits { input, key, edits } => {
            if !key.is_empty() {
                let k: Vec<String> = key
                    .iter()
                    .map(|c| input.columns.iter().find(|x| x.name.eq_ignore_ascii_case(c)).map_or(ident(c), |x| ident(&x.name)))
                    .collect();
                let repeats = session.count(&format!(
                    "SELECT count(*) FROM (SELECT 1 FROM {} GROUP BY {} HAVING count(*) > 1)",
                    input.name,
                    k.join(", ")
                ))?;
                if repeats > 0 {
                    return Err(Error::Unprocessable(format!(
                        "its key {} is not unique here: {} value(s) repeat, so an edit made for one row would find several",
                        key.join(", "),
                        record::n(repeats)
                    )));
                }
            }
            let view = format!("{}_e", p.out.name.trim_matches('"'));
            let outcomes: Vec<Outcome> = crate::edits::apply(session, &input.name, &view, &input.columns, edits)?;
            session.execute(&format!("CREATE TABLE {} AS SELECT * FROM {}", p.out.name, ident(&view)))?;
            rec.edits = outcomes;
        }
        Plan::Output { .. } => {}
    }
    Ok(())
}

/// Runs a chain's stages, then its checks. With a record, each step's
/// rows and the values it changed are counted.
fn run_chain(engine: &Engine, session: &Session, chain: &Chain, mut rec: Option<&mut NodeRecord>) -> Result<()> {
    for stage in &chain.stages {
        match &stage.sql {
            StageSql::Select(sql) => session.execute(&format!("CREATE TABLE {} AS {sql}", ident(&stage.name)))?,
            StageSql::Sql(s) => crate::sql::run(engine, session, &stage.name, s)?,
        }
        if let Some(rec) = rec.as_deref_mut() {
            let before = session.count(&format!("SELECT count(*) FROM {}", stage.input))?;
            let rows = session.count(&format!("SELECT count(*) FROM {}", ident(&stage.name)))?;
            let changed = if !stage.rewrites.is_empty() && before == rows {
                let differs: Vec<String> = stage
                    .rewrites
                    .iter()
                    .map(|c| format!("CAST(a.{x} AS VARCHAR) IS DISTINCT FROM CAST(b.{x} AS VARCHAR)", x = ident(c)))
                    .collect();
                // Rows, not cells: a row a step changed in two columns is one.
                Some(session.count(&format!(
                    "SELECT count(*) FROM {} a POSITIONAL JOIN {} b WHERE {}",
                    stage.input,
                    ident(&stage.name),
                    differs.join(" OR ")
                ))?)
            } else {
                None
            };
            rec.steps.push(StepRecord {
                step: stage.step,
                op: stage.op.to_string(),
                rows,
                changed,
                said: said(stage, before, rows, changed),
            });
        }
    }
    for c in &chain.checks {
        check(session, c)?;
    }
    Ok(())
}

/// Refused when a check's failing rows are there, naming the first
/// values.
fn check(session: &Session, c: &compile::Check) -> Result<()> {
    let n = session.count(&format!("SELECT count(*) FROM {} WHERE {}", c.relation, c.failing))?;
    if n > 0 {
        let x = ident(&c.column);
        let examples = session.texts(&format!("SELECT CAST({x} AS VARCHAR) FROM {} WHERE {} LIMIT 3", c.relation, c.failing))?;
        let shown: Vec<String> = examples.iter().map(|v| format!("'{v}'")).collect();
        let such = if shown.is_empty() { String::new() } else { format!(", such as {}", shown.join(", ")) };
        return Err(Error::Unprocessable(format!("{}{such}", c.says.replace("{n}", &record::n(n)))));
    }
    Ok(())
}

/// A step's record, in words.
fn said(stage: &compile::Stage, before: u64, rows: u64, changed: Option<u64>) -> String {
    let ch = changed.map(record::n).unwrap_or_else(|| "some".into());
    let added: Vec<String> =
        stage.after.iter().filter(|c| !stage.before.iter().any(|b| b.name.eq_ignore_ascii_case(&c.name))).map(|c| c.name.clone()).collect();
    let removed: Vec<String> =
        stage.before.iter().filter(|c| !stage.after.iter().any(|a| a.name.eq_ignore_ascii_case(&c.name))).map(|c| c.name.clone()).collect();
    let cols = record::listed(&stage.rewrites);
    match stage.op {
        "rename_column" => format!("renamed {} to {}", record::listed(&removed), record::listed(&added)),
        "retype_column" => {
            let t: Vec<String> = stage
                .after
                .iter()
                .filter(|a| stage.before.iter().any(|b| b.name == a.name && b.ty != a.ty))
                .map(|a| format!("{} to {}", a.name, a.ty))
                .collect();
            if t.is_empty() {
                "kept the types as they were".into()
            } else {
                format!("made {}", t.join(", "))
            }
        }
        "remove_columns" => format!("removed {}", record::listed(&removed)),
        "reorder_columns" => "put columns in a new order".into(),
        "add_column" | "update_column" if !added.is_empty() => format!("added {}", record::listed(&added)),
        "update_column" => format!("computed {cols} again: {ch} value(s) changed"),
        "filter" => format!("kept {} of {}", record::rows(rows), record::n(before)),
        "sort" => "sorted the rows".into(),
        "dedupe" => format!("removed {} repeated", record::rows(before.saturating_sub(rows))),
        "fill_empty" => format!("filled {ch} blank(s) in {cols}"),
        "fill_down" => format!("filled {ch} blank(s) in {cols} from the row above"),
        "find_and_replace_val" => format!("replaced text in {ch} value(s) of {cols}"),
        "remap_val" => format!("remapped {ch} value(s) of {cols}"),
        "trim_val" => format!("trimmed {ch} value(s) in {cols}"),
        "upper_case" | "lower_case" | "title_case" | "snake_case" | "camel_case" => format!("changed the case of {ch} value(s) in {cols}"),
        "group" => format!("grouped {} into {}", record::rows(before), record::n(rows)),
        "unpivot" => format!("unpivoted {} into {}", record::rows(before), record::n(rows)),
        "sql" => format!("ran a query: {} in, {} out, {} columns", record::n(before), record::n(rows), stage.after.len()),
        op => format!("{op}: {}", record::rows(rows)),
    }
}

/// What a node added, removed and retyped of its input's columns.
fn changed_columns(before: &[Column], after: &[Column], rec: &mut NodeRecord) {
    let find = |cs: &[Column], n: &str| cs.iter().position(|c| c.name.eq_ignore_ascii_case(n));
    rec.added = after.iter().filter(|c| find(before, &c.name).is_none()).map(|c| c.name.clone()).collect();
    rec.removed = before.iter().filter(|c| find(after, &c.name).is_none()).map(|c| c.name.clone()).collect();
    rec.retyped = after
        .iter()
        .filter_map(|a| {
            let b = &before[find(before, &a.name)?];
            (b.ty != a.ty).then(|| Retyped { column: a.name.clone(), from: b.ty, to: a.ty })
        })
        .collect();
}

fn check_words(c: &Contract) -> String {
    let cols = c.columns.join(", ");
    match c.check {
        CheckKind::Unique => format!("{cols} unique"),
        CheckKind::NotNull => format!("{cols} never empty"),
    }
}

/// An output check, run.
fn contract(session: &Session, rel: &Rel, c: &Contract) -> Result<CheckResult> {
    let cols: Vec<String> = c
        .columns
        .iter()
        .map(|n| rel.columns.iter().find(|x| x.name.eq_ignore_ascii_case(n)).map_or(ident(n), |x| ident(&x.name)))
        .collect();
    let failing = match c.check {
        CheckKind::Unique => {
            session.count(&format!("SELECT count(*) FROM (SELECT 1 FROM {} GROUP BY {} HAVING count(*) > 1)", rel.name, cols.join(", ")))?
        }
        CheckKind::NotNull => session.count(&format!(
            "SELECT count(*) FROM {} WHERE {}",
            rel.name,
            cols.iter().map(|x| format!("{x} IS NULL")).collect::<Vec<_>>().join(" OR ")
        ))?,
    };
    Ok(CheckResult { check: c.check, columns: c.columns.clone(), on_fail: c.on_fail, passed: failing == 0, failing })
}

/// Per column of a relation: its empty values, distinct values, least
/// and greatest, in one pass over it.
pub(crate) fn profile(session: &Session, rel: &Rel) -> Result<Vec<ColumnProfile>> {
    let cols: Vec<&Column> = rel.columns.iter().take(PROFILED).collect();
    if cols.is_empty() {
        return Ok(Vec::new());
    }
    let parts: Vec<String> = cols
        .iter()
        .map(|c| {
            let x = ident(&c.name);
            format!(
                "struct_pack(n := count(*) - count({x}), d := count(DISTINCT {x}), lo := left(CAST(min({x}) AS VARCHAR), 80), hi := left(CAST(max({x}) AS VARCHAR), 80))"
            )
        })
        .collect();
    let json = session.texts(&format!("SELECT CAST(to_json([{}]) AS VARCHAR) FROM {}", parts.join(", "), rel.name))?;
    let values: Vec<serde_json::Value> = json.first().and_then(|j| serde_json::from_str(j).ok()).unwrap_or_default();
    Ok(cols
        .iter()
        .zip(values)
        .map(|(c, v)| ColumnProfile {
            column: c.name.clone(),
            ty: c.ty,
            role: c.role,
            nulls: v["n"].as_u64().unwrap_or(0),
            distinct: v["d"].as_u64().unwrap_or(0),
            min: v["lo"].as_str().map(str::to_string),
            max: v["hi"].as_str().map(str::to_string),
        })
        .collect())
}

/// A node's record in one line.
fn summary(node: &Node, rec: &NodeRecord, loaded: Option<&Loaded>, sample: bool) -> String {
    let shape = format!("{}, {} column{}", record::rows(rec.rows), rec.columns, if rec.columns == 1 { "" } else { "s" });
    let input = rec.rows_in.first().copied().unwrap_or(0);
    let of_sample = if sample { " (of a sample)" } else { "" };
    match node {
        Node::Source { .. } | Node::Dataset { .. } => {
            let what = loaded.map(|l| l.said.clone()).unwrap_or_default();
            let retyped = if rec.retyped.is_empty() {
                String::new()
            } else {
                let r: Vec<String> = rec.retyped.iter().map(|r| format!("{} as {}", r.column, r.to)).collect();
                format!("; read {}", r.join(", "))
            };
            format!("Read {what}: {shape}{retyped}.")
        }
        Node::Steps { steps, .. } => {
            let notable: Vec<String> = rec
                .steps
                .iter()
                .filter(|s| s.changed.is_some_and(|c| c > 0) || matches!(s.op.as_str(), "filter" | "dedupe" | "group" | "unpivot" | "sql"))
                .take(3)
                .map(|s| s.said.clone())
                .collect();
            let tail = if notable.is_empty() { String::new() } else { format!("; {}", notable.join("; ")) };
            format!(
                "{} step{}: {} in, {} out{tail}.",
                steps.len(),
                if steps.len() == 1 { "" } else { "s" },
                record::rows(input),
                record::n(rec.rows)
            )
        }
        Node::Join { right, how, .. } => {
            let j = rec.join.as_ref();
            let (m, ul) = j.map_or((0, 0), |j| (j.matched, j.unmatched_left));
            let unmatched = if ul == 0 {
                "every row matched".to_string()
            } else {
                match how {
                    crate::steps::JoinKind::Left => format!("{} have no match and are kept", record::n(ul)),
                    crate::steps::JoinKind::Inner => format!("{} have no match and are dropped", record::n(ul)),
                }
            };
            format!("Joined {right}: {} of {} matched{of_sample}; {unmatched}.", record::n(m), record::rows(input))
        }
        Node::Append { inputs, .. } => {
            let parts: Vec<String> = rec.rows_in.iter().map(|r| record::n(*r)).collect();
            let some = rec.append.as_ref().map(|a| a.only_in_some.len()).unwrap_or(0);
            let tail = if some == 0 { String::new() } else { format!("; {some} column(s) only in some, empty in the rest") };
            format!("Appended {} inputs: {} rows, {} in all{tail}.", inputs.len(), parts.join(" + "), record::n(rec.rows))
        }
        Node::Sql { .. } => format!("Ran a query: {} in, {shape} out.", record::rows(rec.rows_in.iter().sum())),
        Node::Edits { .. } => {
            let count = |s: crate::edits::State| rec.edits.iter().filter(|o| o.state == s).count();
            let mut parts = vec![format!("{} applied", count(crate::edits::State::Applied))];
            for (s, w) in [
                (crate::edits::State::Orphaned, "found no row"),
                (crate::edits::State::Ambiguous, "found several rows"),
                (crate::edits::State::Flagged, "changed upstream since"),
                (crate::edits::State::Conflict, "added a row already there"),
            ] {
                let k = count(s);
                if k > 0 {
                    parts.push(format!("{k} {w}"));
                }
            }
            format!("{} edit{}: {}.", rec.edits.len(), if rec.edits.len() == 1 { "" } else { "s" }, parts.join(", "))
        }
        Node::Output { .. } => {
            let failed: Vec<String> = rec
                .checks
                .iter()
                .filter(|c| !c.passed)
                .map(|c| {
                    format!(
                        "{} ({} failing)",
                        check_words(&Contract { check: c.check, columns: c.columns.clone(), on_fail: c.on_fail }),
                        record::n(c.failing)
                    )
                })
                .collect();
            let checks = match (rec.checks.len(), failed.is_empty()) {
                (0, _) => String::new(),
                (k, true) => format!("; {k} check{} passed", if k == 1 { "" } else { "s" }),
                (_, false) => format!("; failed: {}", failed.join("; ")),
            };
            format!("Output: {shape}{checks}.")
        }
    }
}

/// Columns a source's newest snapshot lacks that its snapshot before
/// had, matched against the columns new in it by the values they share:
/// "'area' is new, and 98% of the values 'region' had are in it". Adds a
/// suggestion to each problem about such a column.
pub(crate) fn overlaps(engine: &Engine, problems: &mut [Problem], loaded: &HashMap<usize, Loaded>) -> Result<()> {
    let wanted: HashSet<String> = problems.iter().filter(|p| p.code == Code::MissingColumn).filter_map(|p| p.column.clone()).collect();
    if wanted.is_empty() {
        return Ok(());
    }
    let session = engine.session()?;
    for (k, l) in loaded {
        let Some((prev_rows, prev_cols, prev_n)) = &l.previous else { continue };
        let has = |cs: &[Column], n: &str| cs.iter().any(|c| c.name.eq_ignore_ascii_case(n));
        let gone: Vec<&Column> =
            prev_cols.iter().filter(|c| !has(&l.columns, &c.name) && wanted.iter().any(|w| w.eq_ignore_ascii_case(&c.name))).collect();
        let new: Vec<&Column> = l.columns.iter().filter(|c| !has(prev_cols, &c.name)).collect();
        if gone.is_empty() || new.is_empty() {
            continue;
        }
        let (a, b) = (format!("prev{k}"), format!("cur{k}"));
        let load = |table: &str, path: &PathBuf, cols: &[Column]| -> Result<()> {
            let typed: Vec<(String, ColumnType)> = cols.iter().map(|c| (c.name.clone(), c.ty)).collect();
            session.load(table, &typed, &first_rows(path, Some(100_000))?)
        };
        load(&a, prev_rows, prev_cols)?;
        load(&b, &l.rows, &l.columns)?;
        for g in gone {
            let mut best: Option<(f64, String)> = None;
            let had = session.count(&format!("SELECT count(DISTINCT CAST({} AS VARCHAR)) FROM {}", ident(&g.name), ident(&a)))?;
            if had == 0 {
                continue;
            }
            for c in &new {
                let shared = session.count(&format!(
                    "SELECT count(*) FROM (SELECT DISTINCT CAST({} AS VARCHAR) AS v FROM {}) x WHERE v IN (SELECT CAST({} AS VARCHAR) FROM {})",
                    ident(&g.name),
                    ident(&a),
                    ident(&c.name),
                    ident(&b)
                ))?;
                let share = shared as f64 / had as f64;
                if share >= 0.5 && best.as_ref().is_none_or(|(s, _)| share > *s) {
                    best = Some((share, c.name.clone()));
                }
            }
            let Some((share, column)) = best else { continue };
            let because = format!(
                "'{}' is gone since snapshot {prev_n}, '{column}' is new, and {:.0}% of the values '{}' had are in it",
                g.name,
                share * 100.0,
                g.name
            );
            for p in problems.iter_mut().filter(|p| p.column.as_deref().is_some_and(|c| c.eq_ignore_ascii_case(&g.name))) {
                p.suggestions.retain(|s| s.column != column);
                p.suggestions.insert(0, Suggestion { column: column.clone(), because: because.clone() });
            }
        }
    }
    Ok(())
}
