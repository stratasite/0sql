//! A pipeline: the graph a run is sent, from its inputs to one output.
//!
//! The application builds it, names its nodes and keeps its versions;
//! 0sql checks it, runs it, and keeps a copy with what it made. A node's
//! id is the application's, and everything a run says is keyed by it.

use crate::edits::Edit;
use crate::schema::{ColumnType, Role};
use crate::steps::{JoinKey, JoinKind, Step};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

/// The most nodes a graph has.
pub const NODES_LIMIT: usize = 200;

/// The longest id a node has.
const ID_LIMIT: usize = 64;

/// The longest note on a node.
const NOTE_LIMIT: usize = 2000;

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Graph {
    pub nodes: Vec<Node>,
}

/// One node. Each says what it reads by the ids of other nodes.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Node {
    /// A source's snapshot, as `read` says; `types` reads its columns
    /// as other types, for this graph only.
    Source {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        source: String,
        #[serde(default)]
        read: Read,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        types: BTreeMap<String, ColumnType>,
    },
    /// Another dataset's output.
    Dataset {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        dataset: String,
        #[serde(default)]
        read: Read,
    },
    /// Steps over one input, in order.
    Steps {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        input: String,
        steps: Vec<Step>,
    },
    /// The right's columns beside the left's, by keys.
    Join {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        left: String,
        right: String,
        on: Vec<JoinKey>,
        #[serde(default, rename = "type")]
        how: JoinKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        columns: Option<Vec<String>>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        prefix: Option<String>,
    },
    /// The inputs' rows, one after another, columns met by name.
    Append {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        inputs: Vec<String>,
    },
    /// A query over its inputs, each by the alias given it.
    Sql {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        /// Alias to node id.
        inputs: BTreeMap<String, String>,
        query: String,
    },
    /// A user's edits, carried inline, applied where the node is.
    Edits {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        input: String,
        /// The columns that name a row for an edit; unique here.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        key: Vec<String>,
        edits: Vec<Edit>,
    },
    /// The dataset's output: its roles, its key, its checks.
    Output {
        id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        note: Option<String>,
        input: String,
        #[serde(default, skip_serializing_if = "BTreeMap::is_empty")]
        roles: BTreeMap<String, Role>,
        /// The output's grain: unique and never empty, or the run is held.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        key: Vec<String>,
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        checks: Vec<Contract>,
    },
}

/// What an input reads.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Read {
    /// The newest snapshot, or the live version. For a query source, the
    /// newest taken as the run's context, else a pull.
    #[default]
    Latest,
    /// A query source pulled first.
    Pull,
    /// A query source pulled when its newest snapshot taken as the run's
    /// context is older than this many seconds.
    MaxAge(u64),
    /// That snapshot of a source.
    Snapshot(u32),
    /// That version of a dataset.
    Version(u32),
}

/// A check the output must pass.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Contract {
    pub check: CheckKind,
    pub columns: Vec<String>,
    #[serde(default)]
    pub on_fail: OnFail,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum CheckKind {
    /// No two rows share these columns' values.
    Unique,
    /// No row has an empty value in these columns.
    NotNull,
}

/// What a check that fails does.
#[derive(Clone, Copy, Debug, Default, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum OnFail {
    /// The run keeps nothing, and the last output stays.
    #[default]
    Hold,
    /// The output is kept, and the run says it failed.
    Warn,
}

/// Something wrong with a graph, or with what it reads, said of the
/// node it is in.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Problem {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub node: Option<String>,
    pub code: Code,
    pub message: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub column: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub suggestions: Vec<Suggestion>,
}

/// The kinds of problem, for a client to act on without reading words.
#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum Code {
    /// The graph's shape: ids, edges, a cycle, the output.
    Graph,
    /// A source or dataset it names is not there.
    NotFound,
    /// A node's definition: a step, a join, a check.
    Invalid,
    /// A column a node reads is not there.
    MissingColumn,
    /// What an input reads does not fit it.
    Read,
    /// A snapshot named was taken by another statement than its spec
    /// plans to now: another context, or another deploy.
    Context,
    /// A query source's spec does not plan.
    Plan,
    /// The data will not have it: a value that does not convert, a key
    /// that repeats.
    Data,
}

/// A column that may be the one meant, and why.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Suggestion {
    pub column: String,
    pub because: String,
}

impl Problem {
    pub fn new(node: Option<&str>, code: Code, message: impl Into<String>) -> Problem {
        Problem { node: node.map(str::to_string), code, message: message.into(), column: None, suggestions: Vec::new() }
    }

    /// Said with its node, as a line of text.
    pub fn said(&self) -> String {
        match &self.node {
            Some(n) => format!("node {n}: {}", self.message),
            None => self.message.clone(),
        }
    }
}

impl Node {
    pub fn id(&self) -> &str {
        match self {
            Node::Source { id, .. }
            | Node::Dataset { id, .. }
            | Node::Steps { id, .. }
            | Node::Join { id, .. }
            | Node::Append { id, .. }
            | Node::Sql { id, .. }
            | Node::Edits { id, .. }
            | Node::Output { id, .. } => id,
        }
    }

    pub fn note(&self) -> Option<&str> {
        match self {
            Node::Source { note, .. }
            | Node::Dataset { note, .. }
            | Node::Steps { note, .. }
            | Node::Join { note, .. }
            | Node::Append { note, .. }
            | Node::Sql { note, .. }
            | Node::Edits { note, .. }
            | Node::Output { note, .. } => note.as_deref(),
        }
    }

    pub fn kind(&self) -> &'static str {
        match self {
            Node::Source { .. } => "source",
            Node::Dataset { .. } => "dataset",
            Node::Steps { .. } => "steps",
            Node::Join { .. } => "join",
            Node::Append { .. } => "append",
            Node::Sql { .. } => "sql",
            Node::Edits { .. } => "edits",
            Node::Output { .. } => "output",
        }
    }

    /// The nodes it reads, by id, in the order it reads them.
    pub fn inputs(&self) -> Vec<&str> {
        match self {
            Node::Source { .. } | Node::Dataset { .. } => Vec::new(),
            Node::Steps { input, .. } | Node::Edits { input, .. } | Node::Output { input, .. } => vec![input],
            Node::Join { left, right, .. } => vec![left, right],
            Node::Append { inputs, .. } => inputs.iter().map(String::as_str).collect(),
            Node::Sql { inputs, .. } => inputs.values().map(String::as_str).collect(),
        }
    }
}

fn plain_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= ID_LIMIT && id.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_' || b == b'-' || b == b'.')
}

impl Graph {
    pub fn node(&self, id: &str) -> Option<&Node> {
        self.nodes.iter().find(|n| n.id() == id)
    }

    /// The output node.
    pub fn output(&self) -> Option<&Node> {
        self.nodes.iter().find(|n| matches!(n, Node::Output { .. }))
    }

    /// The sources and datasets it reads, each once, as `(kind, id)`.
    pub fn reads(&self) -> Vec<(&'static str, String)> {
        let mut out: Vec<(&'static str, String)> = Vec::new();
        for n in &self.nodes {
            let r = match n {
                Node::Source { source, .. } => ("source", source.clone()),
                Node::Dataset { dataset, .. } => ("dataset", dataset.clone()),
                _ => continue,
            };
            if !out.contains(&r) {
                out.push(r);
            }
        }
        out
    }

    /// The nodes in an order where each comes after what it reads, or
    /// what is wrong with its shape. `upto` is a node a preview stops at:
    /// then there need be no output, and only what it reads is ordered.
    pub fn order(&self, upto: Option<&str>) -> Result<Vec<usize>, Vec<Problem>> {
        let mut problems = Vec::new();
        fn bad(problems: &mut Vec<Problem>, node: Option<&str>, m: String) {
            problems.push(Problem::new(node, Code::Graph, m));
        }
        if self.nodes.is_empty() || self.nodes.len() > NODES_LIMIT {
            bad(&mut problems, None, format!("a graph has 1 to {NODES_LIMIT} nodes"));
            return Err(problems);
        }
        let mut seen = std::collections::HashSet::new();
        for n in &self.nodes {
            let id = n.id();
            if !plain_id(id) {
                bad(&mut problems, Some(id), format!("'{id}' is not a node id: 1 to {ID_LIMIT} letters, digits, '_', '-' or '.'"));
            } else if !seen.insert(id) {
                bad(&mut problems, Some(id), format!("two nodes have the id '{id}'"));
            }
            if n.note().is_some_and(|t| t.chars().count() > NOTE_LIMIT) {
                bad(&mut problems, Some(id), format!("a note is at most {NOTE_LIMIT} characters"));
            }
        }
        let outputs: Vec<&str> = self.nodes.iter().filter(|n| matches!(n, Node::Output { .. })).map(Node::id).collect();
        match (outputs.len(), upto) {
            (1, _) | (0, Some(_)) => {}
            (0, None) => bad(&mut problems, None, "a graph has one output node, and this one has none".into()),
            (k, _) => bad(&mut problems, None, format!("a graph has one output node, and this one has {k}: {}", outputs.join(", "))),
        }
        for n in &self.nodes {
            let id = n.id();
            match n {
                Node::Append { inputs, .. } if inputs.len() < 2 => {
                    bad(&mut problems, Some(id), "an append reads two inputs or more".into())
                }
                Node::Join { left, right, .. } if left == right => bad(&mut problems, Some(id), "a join reads two different nodes".into()),
                Node::Sql { inputs, .. } if inputs.is_empty() => bad(&mut problems, Some(id), "a sql node reads one input or more".into()),
                Node::Sql { inputs, .. } => {
                    for alias in inputs.keys() {
                        if let Err(e) = crate::sql::check_alias(alias) {
                            bad(&mut problems, Some(id), e.to_string());
                        }
                    }
                }
                _ => {}
            }
            for i in n.inputs() {
                match self.node(i) {
                    None => bad(&mut problems, Some(id), format!("it reads node '{i}', which the graph does not have")),
                    Some(Node::Output { .. }) => bad(&mut problems, Some(id), format!("it reads '{i}', the output, which nothing reads")),
                    Some(_) if i == id => bad(&mut problems, Some(id), "it reads itself".into()),
                    Some(_) => {}
                }
            }
        }
        if !problems.is_empty() {
            return Err(problems);
        }
        // Kahn's order, stable in the order the nodes were given.
        let index = |id: &str| self.nodes.iter().position(|n| n.id() == id);
        let mut order: Vec<usize> = Vec::new();
        let mut placed = vec![false; self.nodes.len()];
        loop {
            let ready =
                (0..self.nodes.len()).find(|&k| !placed[k] && self.nodes[k].inputs().iter().all(|i| index(i).is_some_and(|j| placed[j])));
            let Some(k) = ready else { break };
            placed[k] = true;
            order.push(k);
        }
        if order.len() < self.nodes.len() {
            let stuck: Vec<&str> = (0..self.nodes.len()).filter(|k| !placed[*k]).map(|k| self.nodes[k].id()).collect();
            bad(&mut problems, None, format!("the graph has a cycle through {}", stuck.join(", ")));
            return Err(problems);
        }
        // What leads to the end, the output or the node a preview stops at.
        let end = match upto {
            Some(u) => match index(u) {
                Some(k) => k,
                None => {
                    bad(&mut problems, None, format!("there is no node '{u}' to preview"));
                    return Err(problems);
                }
            },
            None => self.nodes.iter().position(|n| matches!(n, Node::Output { .. })).expect("one output, checked"),
        };
        let mut needed = vec![false; self.nodes.len()];
        let mut todo = vec![end];
        while let Some(k) = todo.pop() {
            if std::mem::replace(&mut needed[k], true) {
                continue;
            }
            todo.extend(self.nodes[k].inputs().iter().filter_map(|i| index(i)));
        }
        if upto.is_none() {
            let idle: Vec<&str> = (0..self.nodes.len()).filter(|k| !needed[*k]).map(|k| self.nodes[k].id()).collect();
            if !idle.is_empty() {
                bad(&mut problems, None, format!("{} lead(s) nowhere: nothing between them and the output reads them", idle.join(", ")));
                return Err(problems);
            }
        }
        Ok(order.into_iter().filter(|k| needed[*k]).collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn graph(v: serde_json::Value) -> Graph {
        serde_json::from_value(json!({ "nodes": v })).unwrap()
    }

    #[test]
    fn a_graph_reads_as_the_application_writes_it() {
        let g = graph(json!([
            { "id": "out", "kind": "output", "input": "j", "key": ["store"] },
            { "id": "j", "kind": "join", "left": "a", "right": "b", "on": [{ "left": "store", "right": "id" }], "type": "inner" },
            { "id": "a", "kind": "source", "source": "src_1", "read": { "max_age": 3600 }, "note": "the budget file" },
            { "id": "b", "kind": "dataset", "dataset": "ds_1", "read": { "version": 2 } },
        ]));
        let order: Vec<&str> = g.order(None).unwrap().into_iter().map(|k| g.nodes[k].id()).collect();
        assert_eq!(order, ["a", "b", "j", "out"]);
        assert_eq!(g.reads(), [("source", "src_1".to_string()), ("dataset", "ds_1".to_string())]);
        assert!(matches!(g.node("a"), Some(Node::Source { read: Read::MaxAge(3600), .. })));
        assert!(serde_json::from_value::<Graph>(json!({ "nodes": [{ "id": "a", "kind": "source", "source": "s", "x": 1 }] })).is_err());
    }

    #[test]
    fn its_shape_is_refused_plainly() {
        for (v, says) in [
            (json!([]), "1 to 200 nodes"),
            (json!([{ "id": "a", "kind": "source", "source": "s" }]), "has none"),
            (json!([{ "id": "a b", "kind": "source", "source": "s" }, { "id": "o", "kind": "output", "input": "a b" }]), "not a node id"),
            (json!([{ "id": "a", "kind": "source", "source": "s" }, { "id": "a", "kind": "output", "input": "a" }]), "two nodes"),
            (json!([{ "id": "o", "kind": "output", "input": "x" }]), "does not have"),
            (
                json!([
                    { "id": "a", "kind": "steps", "input": "b", "steps": [] },
                    { "id": "b", "kind": "steps", "input": "a", "steps": [] },
                    { "id": "o", "kind": "output", "input": "a" },
                ]),
                "a cycle through a, b, o",
            ),
            (
                json!([
                    { "id": "a", "kind": "source", "source": "s" },
                    { "id": "b", "kind": "source", "source": "t" },
                    { "id": "o", "kind": "output", "input": "a" },
                ]),
                "b lead(s) nowhere",
            ),
            (json!([{ "id": "a", "kind": "source", "source": "s" }, { "id": "p", "kind": "append", "inputs": ["a"] }]), "two inputs"),
            (
                json!([{ "id": "a", "kind": "source", "source": "s" }, { "id": "q", "kind": "sql", "inputs": { "2x": "a" }, "query": "x" }]),
                "not an alias",
            ),
        ] {
            let g = graph(v.clone());
            let e = g.order(None).err().unwrap_or_else(|| panic!("{v} passed"));
            assert!(e.iter().any(|p| p.message.contains(says)), "{v}: {e:?}");
        }
    }

    #[test]
    fn a_preview_orders_only_what_its_node_reads() {
        let g = graph(json!([
            { "id": "a", "kind": "source", "source": "s" },
            { "id": "b", "kind": "source", "source": "t" },
            { "id": "s", "kind": "steps", "input": "a", "steps": [] },
        ]));
        let order: Vec<&str> = g.order(Some("s")).unwrap().into_iter().map(|k| g.nodes[k].id()).collect();
        assert_eq!(order, ["a", "s"], "no output needed, and b is not read");
    }
}
