//! Edits: what a user changes in the grid, carried inline in an
//! `edits` node of the graph the application sends. The application
//! keeps them and places the node; 0sql applies them where the node is,
//! every run, and says how each fared.
//!
//! An edit names its rows by value, never by position, so it holds when
//! what is upstream changes:
//!
//! - **A remap** is the rule (column, value) to new value, for every row
//!   with that value. It needs no key.
//! - **A set** changes one column of the row whose key is `key`: the
//!   node's key columns and their values. Applied when one row has
//!   them; orphaned when none does; ambiguous when more than one does,
//!   unless it was made for all of them. With `was`, the value it
//!   replaced, a run where upstream changed that value is flagged: the
//!   user's value wins, and the run says where upstream disagreed.
//! - **A delete or an add** names its row the same way.
//!
//! The node's key must be unique where the node is, or the run fails:
//! an edit made for one row would find several.

use crate::error::{Error, Result};
use crate::schema::Column;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::BTreeMap;

/// The most edits one node holds.
pub const EDITS_LIMIT: usize = 10_000;

/// One edit, as the graph carries it.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum Edit {
    Remap {
        id: String,
        column: String,
        from: Value,
        to: Value,
    },
    /// One cell, of the row whose key is `key`.
    Set {
        id: String,
        column: String,
        key: BTreeMap<String, Value>,
        to: Value,
        /// The value it replaced, which a later run is checked against.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        was: Option<Value>,
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
        #[serde(default)]
        values: BTreeMap<String, Value>,
    },
}

impl Edit {
    pub fn id(&self) -> &str {
        match self {
            Edit::Remap { id, .. } | Edit::Set { id, .. } | Edit::Delete { id, .. } | Edit::Add { id, .. } => id,
        }
    }

    /// The columns it reads where it applies.
    pub fn reads(&self) -> Vec<String> {
        match self {
            Edit::Remap { column, .. } => vec![column.clone()],
            Edit::Set { column, key, .. } => std::iter::once(column.clone()).chain(key.keys().cloned()).collect(),
            Edit::Delete { key, .. } => key.keys().cloned().collect(),
            Edit::Add { key, values, .. } => key.keys().chain(values.keys()).cloned().collect(),
        }
    }

    fn key(&self) -> Option<&BTreeMap<String, Value>> {
        match self {
            Edit::Set { key, .. } | Edit::Delete { key, .. } | Edit::Add { key, .. } => Some(key),
            Edit::Remap { .. } => None,
        }
    }
}

/// Refused unless a node's edits are sound where it is: ids unique and
/// at most 64 characters; each value of its column's type; every key
/// naming exactly the node's key columns. What an edit reads that the
/// node's input does not have is said by the caller, as a column gone.
pub fn check(edits: &[Edit], key: &[String], columns: &[Column]) -> Result<()> {
    if edits.len() > EDITS_LIMIT {
        return Err(Error::Invalid(format!("an edits node holds at most {EDITS_LIMIT} edits")));
    }
    let mut seen = std::collections::HashSet::new();
    for e in edits {
        let id = e.id();
        if id.is_empty() || id.len() > 64 || id.chars().any(char::is_control) {
            return Err(Error::Invalid(format!("'{id}' is not an edit's id: 1 to 64 characters, none of them control characters")));
        }
        if !seen.insert(id.to_string()) {
            return Err(Error::Invalid(format!("two edits have the id '{id}'")));
        }
        let at = |m: String| Error::Invalid(format!("edit {id}: {m}"));
        if let Some(k) = e.key() {
            if key.is_empty() {
                return Err(at("the node has no key, so no edit can name a row; give it key columns".into()));
            }
            let named: Vec<String> = k.keys().map(|n| n.to_lowercase()).collect();
            let mut wanted: Vec<String> = key.iter().map(|n| n.to_lowercase()).collect();
            let mut named_sorted = named.clone();
            named_sorted.sort();
            wanted.sort();
            if named_sorted != wanted {
                return Err(at(format!(
                    "its key names {}; the node's key is {}",
                    k.keys().cloned().collect::<Vec<_>>().join(", "),
                    key.join(", ")
                )));
            }
        }
        let typed = |name: &str, v: &Value| -> Result<()> {
            match columns.iter().find(|c| c.name.eq_ignore_ascii_case(name)) {
                Some(c) => crate::compile::value_sql(v, c.ty, &c.name).map(|_| ()).map_err(|e| at(e.to_string())),
                None => Ok(()),
            }
        };
        match e {
            Edit::Remap { column, from, to, .. } => {
                typed(column, from)?;
                typed(column, to)?;
            }
            Edit::Set { column, key, to, was, .. } => {
                typed(column, to)?;
                if let Some(w) = was {
                    typed(column, w)?;
                }
                for (c, v) in key {
                    typed(c, v)?;
                }
            }
            Edit::Delete { key, .. } => {
                for (c, v) in key {
                    typed(c, v)?;
                }
            }
            Edit::Add { key, values, .. } => {
                for (c, v) in key.iter().chain(values) {
                    typed(c, v)?;
                }
            }
        }
    }
    Ok(())
}

/// How an edit fared in a run.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum State {
    Applied,
    /// No row has its key: kept, listed, not applied.
    Orphaned,
    /// More than one row has it, and it was made for one.
    Ambiguous,
    /// Applied, and upstream changed the value it replaced.
    Flagged,
    /// An added row whose key a row already has.
    Conflict,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Outcome {
    pub id: String,
    pub state: State,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
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

/// How many edits of one kind and key are applied as conditions written
/// into the select, which keeps the rows in their order as it reads
/// them; past it, by a join, sorted back into that order.
const INLINED: usize = 64;

/// A text as the engine casts from, or a null.
fn text_sql(t: Option<String>) -> String {
    t.map_or_else(|| "NULL".to_string(), |t| crate::schema::literal(&t))
}

/// The key column and its type, as `key_match` casts to it.
fn key_column(columns: &[Column], k: &str) -> (String, ColumnType) {
    type_of(columns, k).map(|c| (c.name.clone(), c.ty)).unwrap_or((k.to_string(), ColumnType::Text))
}

/// What `key_match` is for one edit's key values: the row `r` has them.
fn key_is(columns: &[Column], key: &[String], values: &BTreeMap<String, Value>) -> String {
    key.iter()
        .map(|k| {
            let (name, ty) = key_column(columns, k);
            format!("r.{} IS NOT DISTINCT FROM TRY_CAST({} AS {})", ident(&name), text_sql(values.get(k).and_then(text_of)), ty.sql())
        })
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Whether no two of the edits in `m` whose ids are `ids` name the same
/// row: their keys, cast to the key columns' types, are all different.
/// Only then is each row matched by one edit at most, and a set can be
/// written as a condition, not a join that would repeat the row.
fn keys_differ(session: &Session, m: &str, ids: &[String], columns: &[Column], key: &[String]) -> Result<bool> {
    let types: Vec<(String, ColumnType)> = key.iter().map(|k| key_column(columns, k)).collect();
    // A float key is joined, as it always was: its values that compare
    // equal need not group as one.
    if types.iter().any(|(_, t)| *t == ColumnType::Double) {
        return Ok(false);
    }
    let cast: Vec<String> = types.iter().enumerate().map(|(i, (_, t))| format!("TRY_CAST(\"__k{i}\" AS {})", t.sql())).collect();
    let repeats = session.count(&format!(
        "SELECT count(*) FROM (SELECT 1 FROM {} WHERE \"__id\" IN ({}) GROUP BY {} HAVING count(*) > 1)",
        ident(m),
        ids.join(", "),
        cast.join(", ")
    ))?;
    Ok(repeats == 0)
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
    apply_inlining(session, (input, output), columns, edits, INLINED)
}

/// The same, writing at most `inlined` edits of a kind into a select.
fn apply_inlining(
    session: &Session,
    (input, output): (&str, &str),
    columns: &[Column],
    edits: &[Edit],
    inlined: usize,
) -> Result<Vec<Outcome>> {
    let mut outcomes: Vec<Outcome> = Vec::new();
    let mut rel = input.to_string();
    let mut n = 0usize;
    for (tag, run) in runs(columns, edits).iter().enumerate() {
        apply_run(session, (output, tag, inlined), columns, run, (&mut rel, &mut n), &mut outcomes)?;
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
    (output, tag, inlined): (&str, usize, usize),
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
    let mut remaps: Vec<(String, Vec<crate::steps::Mapping>)> = Vec::new();
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
            entry.1.push(crate::steps::Mapping { from: from.clone(), to: to.clone() });
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
        let mut names = vec!["__id".to_string(), "__to".to_string(), "__was".to_string(), "__known".to_string()];
        names.extend((0..key.len()).map(|i| format!("__k{i}")));
        let rows: Vec<Vec<Option<String>>> = last
            .iter()
            .filter_map(|e| match e {
                Edit::Set { id, key: k, was, to, .. } => {
                    let known = Some(if was.is_some() { "y" } else { "n" }.to_string());
                    let mut r = vec![Some(id.clone()), text_of(to), was.as_ref().and_then(text_of), known];
                    r.extend(key.iter().map(|c| k.get(c).and_then(text_of)));
                    Some(r)
                }
                _ => None,
            })
            .collect();
        let m = format!("{output}__set{tag}_{gi}");
        load_texts(session, &m, &names, &rows)?;
        let x = ident(&target.name);
        let counted = session.query(&format!(
            "SELECT m.\"__id\", CAST(count(r.\"__one\") AS BIGINT), \
             CAST(bool_or(m.\"__known\" = 'y' AND r.{x} IS DISTINCT FROM TRY_CAST(m.\"__was\" AS {t})) AS BOOLEAN) \
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
                State::Orphaned => Some("no row has its key".to_string()),
                State::Ambiguous => Some(format!("{count} rows have its key")),
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
        if ids.len() <= inlined && keys_differ(session, &m, &ids, columns, &key)? {
            let whens: Vec<String> = last
                .iter()
                .filter_map(|e| match e {
                    Edit::Set { id, key: k, to, .. } if apply_ids.contains(id) => {
                        Some(format!("WHEN {} THEN TRY_CAST({} AS {})", key_is(columns, &key, k), text_sql(text_of(to)), target.ty.sql()))
                    }
                    _ => None,
                })
                .collect();
            let select: Vec<String> = columns
                .iter()
                .map(|c| {
                    if c.name == target.name {
                        format!("CASE {} ELSE r.{x} END AS {x}", whens.join(" "), x = ident(&c.name))
                    } else {
                        format!("r.{}", ident(&c.name))
                    }
                })
                .collect();
            next(format!("SELECT {} FROM {rel} r", select.join(", ")), rel)?;
            continue;
        }
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
        let m = format!("{output}__delete{tag}_{gi}");
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
                State::Orphaned => Some("no row has its key".to_string()),
                State::Ambiguous => Some(format!("{count} rows have its key")),
                _ => None,
            };
            outcomes.push(Outcome { id, state, detail });
        }
        if apply_ids.is_empty() {
            continue;
        }
        if apply_ids.len() <= inlined {
            let named: Vec<String> = dels
                .iter()
                .filter_map(|e| match e {
                    Edit::Delete { id, key: k, .. } if apply_ids.contains(&crate::schema::literal(id)) => {
                        Some(format!("({})", key_is(columns, &key, k)))
                    }
                    _ => None,
                })
                .collect();
            next(format!("SELECT {} FROM {rel} r WHERE NOT ({})", list("r."), named.join(" OR ")), rel)?;
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
                outcomes.push(Outcome { id: id.clone(), state: State::Conflict, detail: Some("a row with its key is there".into()) });
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
            // Named for its node: two edits nodes of a graph share a session.
            let add = ident(&format!("{output}__add{tag}"));
            load_texts(session, &format!("{output}__add{tag}"), &names, &rows)?;
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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn columns(of: &[(&str, ColumnType)]) -> Vec<Column> {
        of.iter().map(|(n, t)| Column { name: n.to_string(), ty: *t, role: crate::schema::Role::inferred(n, *t) }).collect()
    }

    /// The rows and outcomes of edits applied as conditions where they
    /// can be, and of the same edits applied by joins only.
    fn both_ways(table: &str, cols: &[Column], edits: serde_json::Value, joined: bool) {
        let edits: Vec<Edit> = serde_json::from_value(edits).unwrap();
        let engine = crate::engine::tests::engine();
        let list = cols.iter().map(|c| ident(&c.name)).collect::<Vec<_>>().join(", ");
        let mut seen = Vec::new();
        for (out, inlined) in [("inline", INLINED), ("joined", 0)] {
            let s = engine.session().unwrap();
            s.execute(table).unwrap();
            let outcomes = apply_inlining(&s, ("t", out), cols, &edits, inlined).unwrap();
            let sorted = s
                .count(&format!("SELECT count(*) FROM duckdb_views() WHERE view_name LIKE '{out}_%' AND sql LIKE '%row_number%'"))
                .unwrap();
            assert_eq!(sorted > 0, out == "joined" || joined, "{out}");
            let rows = zsql_exec::json::rows(&s.query(&format!("SELECT {list} FROM {out}")).unwrap()).unwrap();
            seen.push((outcomes, rows));
        }
        assert_eq!(seen[0], seen[1]);
    }

    #[test]
    fn edits_written_as_conditions_do_what_joins_do() {
        let cols = columns(&[("id", ColumnType::Integer), ("code", ColumnType::Text), ("amount", ColumnType::Decimal(2))]);
        let table = "CREATE TABLE t AS SELECT * FROM (VALUES (3, 'c', 1.50), (1, 'a', 2.00), (NULL, 'n', 3.00), (2, 'b', NULL), \
                     (5, 'e', 4.25), (4, 'd', 0.10)) v(id, code, amount)";
        both_ways(
            table,
            &cols,
            json!([
                { "id": "r", "kind": "remap", "column": "code", "from": "a", "to": "A" },
                { "id": "s1", "kind": "set", "column": "amount", "key": { "id": 1 }, "to": 9.5, "was": 2 },
                { "id": "s2", "kind": "set", "column": "amount", "key": { "id": null }, "to": "7" },
                { "id": "s3", "kind": "set", "column": "amount", "key": { "id": 42 }, "to": 1 },
                { "id": "s4", "kind": "set", "column": "amount", "key": { "id": 2 }, "to": null },
                { "id": "s5", "kind": "set", "column": "amount", "key": { "id": 4 }, "to": "not a number" },
                { "id": "s6", "kind": "set", "column": "amount", "key": { "id": 3 }, "to": 1 },
                { "id": "s7", "kind": "set", "column": "amount", "key": { "id": 3 }, "to": 2 },
                { "id": "d1", "kind": "delete", "key": { "id": 5 } },
                { "id": "d2", "kind": "delete", "key": { "id": 77 } },
                { "id": "d3", "kind": "delete", "key": { "id": null } },
                { "id": "a1", "kind": "add", "key": { "id": 8 }, "values": { "code": "h" } },
            ]),
            false,
        );
        // Two sets whose keys differ as written and not as the key's
        // type: joined, and the row they both name comes out twice.
        both_ways(
            table,
            &cols,
            json!([
                { "id": "s1", "kind": "set", "column": "amount", "key": { "id": 1 }, "to": 5 },
                { "id": "s2", "kind": "set", "column": "amount", "key": { "id": "1" }, "to": 6 },
                { "id": "s3", "kind": "set", "column": "amount", "key": { "id": "x" }, "to": 6 },
            ]),
            true,
        );
        // A text key, and a set of every row with it.
        let cols = columns(&[("code", ColumnType::Text), ("n", ColumnType::Integer)]);
        both_ways(
            "CREATE TABLE t AS SELECT * FROM (VALUES ('a', 1), ('b', 2), ('a', 3), ('it''s', 4)) v(code, n)",
            &cols,
            json!([
                { "id": "s1", "kind": "set", "column": "n", "key": { "code": "a" }, "to": 10, "all": true },
                { "id": "s2", "kind": "set", "column": "n", "key": { "code": "it's" }, "to": 11 },
                { "id": "d1", "kind": "delete", "key": { "code": "b" } },
                { "id": "d2", "kind": "delete", "key": { "code": "a" }, "all": true },
            ]),
            false,
        );
    }
}
