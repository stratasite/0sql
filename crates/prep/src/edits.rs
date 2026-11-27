//! Edits: what a user changes in the grid, as an append-only log
//! folded into the recipe where each edit was made.
//!
//! What an edit means depends on the column's role:
//!
//! - **A dimension cell is a remap**: the rule (column, value) to new
//!   value, for every row with that value. It needs no row identity.
//! - **A measure cell is an override of one row**, and so is a
//!   dimension cell edited for one row only (`one`). The row is named
//!   by its identity: the recipe's key when one is marked, else every
//!   dimension column, at the point the edit applies. Applied when
//!   exactly one row has those values; orphaned when none does;
//!   ambiguous when more than one does, unless the edit was made for
//!   all of them. The value it replaced is kept, so a sync that changes
//!   it upstream is flagged: the user's value wins, and the dataset
//!   says where upstream disagreed.
//! - **A row deleted or added** is named the same way.
//!
//! An edit applies after the steps the recipe had when it was made, so
//! an edit on a result past a join or an append names its row by what
//! is there at that point, and needs no rule of its own.
//!
//! The log is `edits.log`, a JSON line an entry; pasting five thousand
//! cells is one entry. Undo is an entry too.

use crate::error::{Error, Result};
use crate::schema::{Column, Role};
use crate::store;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;
use std::io::Write;
use std::path::Path;

pub(crate) const LOG: &str = "edits.log";

/// The most edits one entry holds.
pub const ENTRY_LIMIT: usize = 10_000;

/// One append to the log.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Entry {
    /// From 1, one more than the entry before.
    pub seq: u64,
    pub at: String,
    /// The recipe version it was made against.
    pub recipe: u32,
    /// How many of the recipe's steps it applies after.
    pub after: usize,
    pub edits: Vec<Edit>,
}

/// One edit, as it is kept.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Edit {
    Remap {
        id: String,
        column: String,
        from: Value,
        to: Value,
    },
    /// One cell, of the row whose identity is `key`.
    Set {
        id: String,
        column: String,
        key: BTreeMap<String, Value>,
        /// The value it replaced, which a later sync is checked against.
        was: Value,
        to: Value,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
    Delete {
        id: String,
        key: BTreeMap<String, Value>,
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        all: bool,
    },
    Add {
        id: String,
        key: BTreeMap<String, Value>,
        values: BTreeMap<String, Value>,
    },
    Undo {
        id: String,
        undoes: String,
    },
}

impl Edit {
    pub fn id(&self) -> &str {
        match self {
            Edit::Remap { id, .. } | Edit::Set { id, .. } | Edit::Delete { id, .. } | Edit::Add { id, .. } | Edit::Undo { id, .. } => id,
        }
    }

    /// The columns it reads where it applies.
    pub fn reads(&self) -> Vec<String> {
        match self {
            Edit::Remap { column, .. } => vec![column.clone()],
            Edit::Set { column, key, .. } => std::iter::once(column.clone()).chain(key.keys().cloned()).collect(),
            Edit::Delete { key, .. } => key.keys().cloned().collect(),
            Edit::Add { key, values, .. } => key.keys().chain(values.keys()).cloned().collect(),
            Edit::Undo { .. } => Vec::new(),
        }
    }
}

/// An edit as a request asks for it, in terms of the grid it was made
/// in: the live version's columns and rows.
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(tag = "op", rename_all = "snake_case", deny_unknown_fields)]
pub enum Asked {
    /// A cell changed. `row` is the row as the grid showed it; for a
    /// dimension edited as a remap, `from` may stand in for it.
    ///
    /// `match` says which rows it is for: the columns whose values must
    /// be the row's. None at all is every row with the edited value: a
    /// remap of the column. Some is the rows that agree on them: `NY` to
    /// `New York` where `country` is `US`. The row's whole identity is
    /// that one row. Left out, a dimension edit is a remap of the
    /// column, and a measure edit is of the row.
    Cell {
        column: String,
        #[serde(default)]
        row: Option<BTreeMap<String, Value>>,
        #[serde(default)]
        from: Option<Value>,
        to: Value,
        #[serde(default, rename = "match")]
        matching: Option<Vec<String>>,
        /// For a measure: every row `match` names, when it names more
        /// than one. A dimension edit is for every row it names already.
        #[serde(default)]
        all: bool,
    },
    Delete {
        row: BTreeMap<String, Value>,
        #[serde(default)]
        all: bool,
    },
    Add {
        values: BTreeMap<String, Value>,
    },
    Undo {
        edit: String,
    },
}

/// The request: edits made against a recipe version.
#[derive(Clone, Debug, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct Request {
    pub against: u32,
    pub edits: Vec<Asked>,
}

/// The log's entries the dataset acknowledged: the first `upto`. An
/// entry past them was written by a job that failed before its manifest
/// was, and was never an edit.
pub(crate) fn read_log(dir: &Path, upto: u64) -> Result<Vec<Entry>> {
    Ok(read_all(dir)?.into_iter().filter(|e| e.seq <= upto).collect())
}

fn read_all(dir: &Path) -> Result<Vec<Entry>> {
    let path = dir.join(LOG);
    let text = match std::fs::read_to_string(&path) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(Error::io(&path, e)),
    };
    // A last line a crash cut short was never acknowledged, and is not
    // an edit.
    let mut out = Vec::new();
    for line in text.lines().filter(|l| !l.trim().is_empty()) {
        match serde_json::from_str::<Entry>(line) {
            Ok(e) => out.push(e),
            Err(_) if !text.ends_with('\n') && text.trim_end().ends_with(line) => break,
            Err(e) => return Err(e.into()),
        }
    }
    Ok(out)
}

/// Appends an entry and makes it durable before it is acknowledged. An
/// entry of its number or later, never acknowledged, goes first, so no
/// two entries share a number.
pub(crate) fn append(dir: &Path, entry: &Entry) -> Result<()> {
    let path = dir.join(LOG);
    let all = read_all(dir)?;
    if all.iter().any(|e| e.seq >= entry.seq) {
        let mut text = Vec::new();
        for e in all.iter().filter(|e| e.seq < entry.seq) {
            text.extend(serde_json::to_vec(e)?);
            text.push(b'\n');
        }
        let staging = dir.join(format!(".{LOG}.{}", crate::time::nanos()));
        let mut f = std::fs::File::create(&staging).map_err(|e| Error::io(&staging, e))?;
        f.write_all(&text).map_err(|e| Error::io(&staging, e))?;
        f.sync_data().map_err(|e| Error::io(&staging, e))?;
        std::fs::rename(&staging, &path).map_err(|e| Error::io(&path, e))?;
    }
    let mut line = serde_json::to_vec(entry)?;
    line.push(b'\n');
    let mut f = std::fs::OpenOptions::new().create(true).append(true).open(&path).map_err(|e| Error::io(&path, e))?;
    f.write_all(&line).map_err(|e| Error::io(&path, e))?;
    f.sync_data().map_err(|e| Error::io(&path, e))
}

/// The edits in force after the first `upto` entries: every edit not
/// undone, each with the position it applies at in `current`, in the
/// order made.
///
/// An edit applies after the steps the recipe had when it was made. The
/// recipe may have changed since, so that position is carried over to
/// the current steps: it follows the last of the steps before it that
/// the current recipe still has, wherever that now is. A step removed
/// or added earlier in the recipe moves no edit off what it was made on.
/// `steps_of` gives a recipe version's steps.
pub(crate) fn in_force(
    log: &[Entry],
    upto: u64,
    steps_of: &dyn Fn(u32) -> Option<Vec<crate::recipe::Step>>,
    current: &[crate::recipe::Step],
) -> Vec<(usize, Edit)> {
    let mut placed: std::collections::HashMap<(u32, usize), usize> = std::collections::HashMap::new();
    let log: Vec<Entry> = log
        .iter()
        .map(|e| {
            let at = *placed.entry((e.recipe, e.after)).or_insert_with(|| relocate(e.after, steps_of(e.recipe).as_deref(), current));
            Entry { after: at, ..e.clone() }
        })
        .collect();
    in_force_placed(&log, upto)
}

/// Where a position after `after` steps of `old` is in `new`: after the
/// last of those steps that `new` still has, by the longest run of steps
/// the two share in order.
pub(crate) fn relocate(after: usize, old: Option<&[crate::recipe::Step]>, new: &[crate::recipe::Step]) -> usize {
    let Some(old) = old else { return after.min(new.len()) };
    if old == new {
        return after;
    }
    let after = after.min(old.len());
    let (n, m) = (old.len(), new.len());
    // lcs[i][j]: the longest common run of old[i..] and new[j..].
    let mut lcs = vec![vec![0u32; m + 1]; n + 1];
    for i in (0..n).rev() {
        for j in (0..m).rev() {
            lcs[i][j] = if old[i] == new[j] { lcs[i + 1][j + 1] + 1 } else { lcs[i + 1][j].max(lcs[i][j + 1]) };
        }
    }
    let (mut i, mut j, mut at) = (0, 0, 0);
    while i < n && j < m && i < after {
        if old[i] == new[j] {
            at = j + 1;
            i += 1;
            j += 1;
        } else if lcs[i + 1][j] >= lcs[i][j + 1] {
            i += 1;
        } else {
            j += 1;
        }
    }
    at
}

fn in_force_placed(log: &[Entry], upto: u64) -> Vec<(usize, Edit)> {
    let entries: Vec<&Entry> = log.iter().filter(|e| e.seq <= upto).collect();
    let undone: std::collections::HashSet<&str> = entries
        .iter()
        .flat_map(|e| e.edits.iter())
        .filter_map(|e| match e {
            Edit::Undo { undoes, .. } => Some(undoes.as_str()),
            _ => None,
        })
        .collect();
    entries
        .iter()
        .flat_map(|e| e.edits.iter().map(move |x| (e.after, x)))
        .filter(|(_, x)| !matches!(x, Edit::Undo { .. }) && !undone.contains(x.id()))
        .map(|(a, x)| (a, x.clone()))
        .collect()
}

/// The columns that name a row: the key when one is marked, else every
/// dimension.
pub(crate) fn identity(columns: &[Column], key: &[String]) -> Vec<String> {
    if key.is_empty() {
        columns.iter().filter(|c| c.role == Role::Dimension).map(|c| c.name.clone()).collect()
    } else {
        key.to_vec()
    }
}

/// How an edit fared in a run.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Applied,
    /// No row has its identity: kept, listed, not applied.
    Orphaned,
    /// More than one row has it, and it was made for one.
    Ambiguous,
    /// Applied, and upstream changed the value it replaced.
    Flagged,
    /// An added row whose identity a row already has.
    Conflict,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Outcome {
    pub id: String,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

/// The values of a row's identity, taken from the row as the grid
/// showed it, refused when it lacks one.
pub(crate) fn key_of(row: &BTreeMap<String, Value>, identity: &[String]) -> Result<BTreeMap<String, Value>> {
    if identity.is_empty() {
        return Err(Error::Invalid(
            "the dataset has no dimension to name a row by; mark a key in the recipe, or make a column a dimension".into(),
        ));
    }
    let mut key = BTreeMap::new();
    for c in identity {
        let v = row
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(c))
            .map(|(_, v)| v.clone())
            .ok_or_else(|| Error::Invalid(format!("the row does not say its '{c}', which is part of what names a row")))?;
        key.insert(c.clone(), v);
    }
    Ok(key)
}

/// The id of the `i`th edit of entry `seq`.
pub(crate) fn id_of(seq: u64, i: usize) -> String {
    format!("{seq}.{}", i + 1)
}

pub(crate) fn read_outcomes(dir: &Path, k: u32) -> Vec<Outcome> {
    store::read_json(&store::version_dir(dir, k).join("edits.json")).unwrap_or_default()
}

// ------------------------------------------------------------- applying

use crate::engine::Session;
use crate::schema::{ident, ColumnType};
use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use std::sync::Arc;

/// A value of a request as the text the engine casts from; a null is
/// none.
pub(crate) fn text_of(v: &Value) -> Option<String> {
    match v {
        Value::Null => None,
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        Value::Number(n) => Some(n.to_string()),
        other => Some(other.to_string()),
    }
}

/// A table of text columns, loaded into the session.
fn load_texts(session: &Session, table: &str, names: &[String], rows: &[Vec<Option<String>>]) -> Result<()> {
    let fields: Vec<Field> = names.iter().map(|n| Field::new(n, DataType::Utf8, true)).collect();
    let arrays: Vec<ArrayRef> =
        (0..names.len()).map(|c| Arc::new(rows.iter().map(|r| r[c].clone()).collect::<StringArray>()) as ArrayRef).collect();
    let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)?;
    let columns: Vec<(String, ColumnType)> = names.iter().map(|n| (n.clone(), ColumnType::Text)).collect();
    session.load(table, &columns, &[batch])
}

fn type_of<'a>(columns: &'a [Column], name: &str) -> Option<&'a Column> {
    columns.iter().find(|c| c.name.eq_ignore_ascii_case(name))
}

/// The join condition between a relation's row `r` and an edit's key
/// columns in `m`, each text cast to its column's type.
fn key_match(columns: &[Column], key: &[String]) -> String {
    key.iter()
        .enumerate()
        .map(|(i, k)| {
            let c = type_of(columns, k).map(|c| (c.name.clone(), c.ty)).unwrap_or((k.clone(), ColumnType::Text));
            format!("r.{} IS NOT DISTINCT FROM TRY_CAST(m.\"__k{i}\" AS {})", ident(&c.0), c.1.sql())
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Applies edits to `input`, as the view `output`; how each fared.
/// Every edit here reads only columns `columns` has.
///
/// They apply in the order they were made: a delete and then a remap of
/// the value it named deletes the row, and a remap of a remapped value
/// chains. Edits in a row of one kind that cannot change each other's
/// rows are applied together, in one view.
pub(crate) fn apply(session: &Session, input: &str, output: &str, columns: &[Column], edits: &[Edit]) -> Result<Vec<Outcome>> {
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut rel = input.to_string();
    let mut n = 0usize;
    for (tag, run) in runs(columns, edits).iter().enumerate() {
        apply_run(session, (output, tag), columns, run, (&mut rel, &mut n), &mut outcomes)?;
    }
    let list = columns.iter().map(|c| ident(&c.name)).collect::<Vec<_>>().join(", ");
    session.execute(&format!("CREATE VIEW {} AS SELECT {list} FROM {rel}", ident(output)))?;
    Ok(outcomes)
}

/// The edits cut where one could change what the next one finds: at a
/// change of kind; for overrides, of the rows' columns or the target; for
/// remaps, where one maps the value another made.
fn runs(columns: &[Column], edits: &[Edit]) -> Vec<Vec<Edit>> {
    let column_of = |c: &str| type_of(columns, c).map(|c| c.name.clone()).unwrap_or_else(|| c.to_string());
    let group = |e: &Edit| -> Option<(u8, Vec<String>, String)> {
        Some(match e {
            Edit::Remap { .. } => (0, vec![], String::new()),
            Edit::Set { column, key, .. } => (1, key.keys().cloned().collect(), column_of(column)),
            Edit::Delete { .. } => (2, vec![], String::new()),
            Edit::Add { .. } => (3, vec![], String::new()),
            _ => return None,
        })
    };
    let mut out: Vec<Vec<Edit>> = Vec::new();
    for e in edits {
        let Some(g) = group(e) else { continue };
        let joins = out.last().is_some_and(|run| {
            group(&run[0]).as_ref() == Some(&g) && match e {
                Edit::Remap { column, from, .. } => !run.iter().any(
                    |r| matches!(r, Edit::Remap { column: c, to, .. } if column_of(c) == column_of(column) && text_of(to) == text_of(from)),
                ),
                _ => true,
            }
        });
        match out.last_mut() {
            Some(run) if joins => run.push(e.clone()),
            _ => out.push(vec![e.clone()]),
        }
    }
    out
}

/// One run of edits over `rel`, which it moves to the view it makes.
fn apply_run(
    session: &Session,
    (output, tag): (&str, usize),
    columns: &[Column],
    edits: &[Edit],
    (rel, n): (&mut String, &mut usize),
    outcomes: &mut Vec<Outcome>,
) -> Result<()> {
    let list = |prefix: &str| columns.iter().map(|c| format!("{prefix}{}", ident(&c.name))).collect::<Vec<_>>().join(", ");
    let mut next = |sql: String, rel: &mut String| -> Result<()> {
        *n += 1;
        let name = format!("{output}_{n}");
        session.execute(&format!("CREATE VIEW {} AS {sql}", ident(&name)))?;
        *rel = ident(&name);
        Ok(())
    };

    // Remaps, the last for each value of each column.
    let mut remaps: Vec<(String, Vec<crate::recipe::Mapping>)> = Vec::new();
    for e in edits {
        if let Edit::Remap { id, column, from, to } = e {
            let c = type_of(columns, column).map(|c| c.name.clone()).unwrap_or_else(|| column.clone());
            let entry = match remaps.iter_mut().find(|(n, _)| *n == c) {
                Some(entry) => entry,
                None => {
                    remaps.push((c.clone(), Vec::new()));
                    remaps.last_mut().expect("just pushed")
                }
            };
            entry.1.retain(|m| m.from != *from);
            entry.1.push(crate::recipe::Mapping { from: from.clone(), to: to.clone() });
            outcomes.push(Outcome { id: id.clone(), state: State::Applied, detail: None });
        }
    }
    if !remaps.is_empty() {
        let mut select = Vec::new();
        for c in columns {
            match remaps.iter().find(|(n, _)| *n == c.name) {
                Some((_, values)) => select.push(format!("{} AS {}", crate::compile::remap_sql(c, values)?, ident(&c.name))),
                None => select.push(ident(&c.name)),
            }
        }
        next(format!("SELECT {} FROM {rel}", select.join(", ")), rel)?;
    }

    // Overrides of one cell, by key columns and target column.
    let mut groups: Vec<((Vec<String>, String), Vec<&Edit>)> = Vec::new();
    for e in edits {
        if let Edit::Set { column, key, .. } = e {
            let g = (key.keys().cloned().collect::<Vec<_>>(), column.clone());
            match groups.iter_mut().find(|(k, _)| *k == g) {
                Some((_, v)) => v.push(e),
                None => groups.push((g, vec![e])),
            }
        }
    }
    for (gi, ((key, column), sets)) in groups.into_iter().enumerate() {
        let Some(target) = type_of(columns, &column).cloned() else { continue };
        // The last edit of each row wins; the ones before it are
        // superseded.
        let mut last: Vec<&Edit> = Vec::new();
        for e in sets {
            if let Edit::Set { key: k, .. } = e {
                if let Some(i) = last.iter().position(|x| matches!(x, Edit::Set { key: k2, .. } if k2 == k)) {
                    outcomes.push(Outcome {
                        id: last[i].id().to_string(),
                        state: State::Applied,
                        detail: Some(format!("superseded by {}", e.id())),
                    });
                    last.remove(i);
                }
                last.push(e);
            }
        }
        let mut names = vec!["__id".to_string(), "__to".to_string(), "__was".to_string()];
        names.extend((0..key.len()).map(|i| format!("__k{i}")));
        let rows: Vec<Vec<Option<String>>> = last
            .iter()
            .filter_map(|e| match e {
                Edit::Set { id, key: k, was, to, .. } => {
                    let mut r = vec![Some(id.clone()), text_of(to), text_of(was)];
                    r.extend(key.iter().map(|c| k.get(c).and_then(text_of)));
                    Some(r)
                }
                _ => None,
            })
            .collect();
        let m = format!("__set{tag}_{gi}");
        load_texts(session, &m, &names, &rows)?;
        let x = ident(&target.name);
        let counted = session.query(&format!(
            "SELECT m.\"__id\", CAST(count(r.\"__one\") AS BIGINT), \
             CAST(bool_or(r.{x} IS DISTINCT FROM TRY_CAST(m.\"__was\" AS {t})) AS BOOLEAN) \
             FROM {m} m LEFT JOIN (SELECT 1 AS \"__one\", * FROM {rel}) r ON {on} GROUP BY m.\"__id\"",
            m = ident(&m),
            t = target.ty.sql(),
            on = key_match(columns, &key),
        ))?;
        let mut apply_ids: Vec<String> = Vec::new();
        for (id, count, changed) in triples(&counted) {
            let all = last.iter().any(|e| matches!(e, Edit::Set { id: i, all: true, .. } if *i == id));
            let state = match count {
                0 => State::Orphaned,
                1 if changed => State::Flagged,
                1 => State::Applied,
                _ if all => State::Applied,
                _ => State::Ambiguous,
            };
            let detail = match state {
                State::Orphaned => Some("no row has its identity".to_string()),
                State::Ambiguous => Some(format!("{count} rows have its identity")),
                State::Flagged => Some(format!("upstream changed the value of '{}' it replaced", target.name)),
                _ => None,
            };
            if matches!(state, State::Applied | State::Flagged) {
                apply_ids.push(id.clone());
            }
            outcomes.push(Outcome { id, state, detail });
        }
        if apply_ids.is_empty() {
            continue;
        }
        let ids: Vec<String> = apply_ids.iter().map(|i| crate::schema::literal(i)).collect();
        let select: Vec<String> = columns
            .iter()
            .map(|c| {
                if c.name == target.name {
                    format!("CASE WHEN m.\"__id\" IS NOT NULL THEN TRY_CAST(m.\"__to\" AS {}) ELSE r.{x} END AS {x}", c.ty.sql())
                } else {
                    format!("r.{}", ident(&c.name))
                }
            })
            .collect();
        next(
            format!(
                "SELECT {} FROM (SELECT *, row_number() OVER () AS \"__pos\" FROM {rel}) r \
                 LEFT JOIN (SELECT * FROM {m} WHERE \"__id\" IN ({ids})) m ON {on} ORDER BY r.\"__pos\"",
                select.join(", "),
                m = ident(&m),
                ids = ids.join(", "),
                on = key_match(columns, &key),
            ),
            rel,
        )?;
    }

    // Deletions, each of the rows its identity names.
    let deletes: Vec<&Edit> = edits.iter().filter(|e| matches!(e, Edit::Delete { .. })).collect();
    let mut by_key: Vec<(Vec<String>, Vec<&Edit>)> = Vec::new();
    for e in deletes {
        if let Edit::Delete { key, .. } = e {
            let k: Vec<String> = key.keys().cloned().collect();
            match by_key.iter_mut().find(|(x, _)| *x == k) {
                Some((_, v)) => v.push(e),
                None => by_key.push((k, vec![e])),
            }
        }
    }
    for (gi, (key, dels)) in by_key.into_iter().enumerate() {
        let mut names = vec!["__id".to_string()];
        names.extend((0..key.len()).map(|i| format!("__k{i}")));
        let rows: Vec<Vec<Option<String>>> = dels
            .iter()
            .filter_map(|e| match e {
                Edit::Delete { id, key: k, .. } => {
                    let mut r = vec![Some(id.clone())];
                    r.extend(key.iter().map(|c| k.get(c).and_then(text_of)));
                    Some(r)
                }
                _ => None,
            })
            .collect();
        let m = format!("__delete{tag}_{gi}");
        load_texts(session, &m, &names, &rows)?;
        let counted = session.query(&format!(
            "SELECT m.\"__id\", CAST(count(r.\"__one\") AS BIGINT), FALSE FROM {m} m \
             LEFT JOIN (SELECT 1 AS \"__one\", * FROM {rel}) r ON {on} GROUP BY m.\"__id\"",
            m = ident(&m),
            on = key_match(columns, &key),
        ))?;
        let mut apply_ids = Vec::new();
        for (id, count, _) in triples(&counted) {
            let all = dels.iter().any(|e| matches!(e, Edit::Delete { id: i, all: true, .. } if *i == id));
            let state = match count {
                0 => State::Orphaned,
                1 => State::Applied,
                _ if all => State::Applied,
                _ => State::Ambiguous,
            };
            if state == State::Applied {
                apply_ids.push(crate::schema::literal(&id));
            }
            let detail = match state {
                State::Orphaned => Some("no row has its identity".to_string()),
                State::Ambiguous => Some(format!("{count} rows have its identity")),
                _ => None,
            };
            outcomes.push(Outcome { id, state, detail });
        }
        if apply_ids.is_empty() {
            continue;
        }
        next(
            format!(
                "SELECT {cols} FROM (SELECT *, row_number() OVER () AS \"__pos\" FROM {rel}) r \
                 WHERE NOT EXISTS (SELECT 1 FROM {m} m WHERE m.\"__id\" IN ({ids}) AND {on}) ORDER BY r.\"__pos\"",
                cols = list("r."),
                m = ident(&m),
                ids = apply_ids.join(", "),
                on = key_match(columns, &key),
            ),
            rel,
        )?;
    }

    // Added rows, after the rest, when no row has their identity.
    let adds: Vec<&Edit> = edits.iter().filter(|e| matches!(e, Edit::Add { .. })).collect();
    if !adds.is_empty() {
        let mut names = vec!["__id".to_string()];
        names.extend(columns.iter().map(|c| c.name.clone()));
        let mut rows = Vec::new();
        for e in &adds {
            let Edit::Add { id, key, values } = e else { continue };
            let mut filters = Vec::new();
            for (c, v) in key {
                let col = type_of(columns, c).map(|c| (c.name.clone(), c.ty)).unwrap_or((c.clone(), ColumnType::Text));
                match text_of(v) {
                    Some(t) => filters.push(format!("{} = TRY_CAST({} AS {})", ident(&col.0), crate::schema::literal(&t), col.1.sql())),
                    None => filters.push(format!("{} IS NULL", ident(&col.0))),
                }
            }
            let taken = if filters.is_empty() {
                0
            } else {
                session.count(&format!("SELECT count(*) FROM {rel} WHERE {}", filters.join(" AND ")))?
            };
            if taken > 0 {
                outcomes.push(Outcome { id: id.clone(), state: State::Conflict, detail: Some("a row with its identity is there".into()) });
                continue;
            }
            let mut r = vec![Some(id.clone())];
            r.extend(
                columns
                    .iter()
                    .map(|c| values.iter().chain(key.iter()).find(|(n, _)| n.eq_ignore_ascii_case(&c.name)).and_then(|(_, v)| text_of(v))),
            );
            rows.push(r);
            outcomes.push(Outcome { id: id.clone(), state: State::Applied, detail: None });
        }
        if !rows.is_empty() {
            let add = ident(&format!("__add{tag}"));
            load_texts(session, &format!("__add{tag}"), &names, &rows)?;
            let casts: Vec<String> =
                columns.iter().map(|c| format!("TRY_CAST({} AS {}) AS {}", ident(&c.name), c.ty.sql(), ident(&c.name))).collect();
            next(format!("SELECT {} FROM {rel} UNION ALL SELECT {} FROM {add}", list(""), casts.join(", ")), rel)?;
        }
    }
    Ok(())
}

/// `(id, count, changed)` from a query that answers those three.
fn triples(rows: &zsql_datasource::Batches) -> Vec<(String, i64, bool)> {
    use arrow_array::cast::AsArray;
    use arrow_array::types::Int64Type;
    use arrow_array::Array;
    let mut out = Vec::new();
    for b in &rows.batches {
        let ids = b.column(0).as_string::<i32>();
        let counts = b.column(1).as_primitive::<Int64Type>();
        let changed = b.column(2).as_boolean();
        for i in 0..b.num_rows() {
            out.push((ids.value(i).to_string(), counts.value(i), !changed.is_null(i) && changed.value(i)));
        }
    }
    out
}
