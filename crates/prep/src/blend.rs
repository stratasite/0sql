//! Blends: a query of the model with datasets beside it, joined the way
//! the request says and no other.
//!
//! The model's part is one planned query, run in the warehouse; this
//! module is everything after: the request's joins, in DuckDB, over
//! the model's rows and each attached dataset's live version.
//!
//! - **Every join is the caller's.** Each names its two sides (`model`
//!   or an alias), the columns on each, and its kind. Nothing is found
//!   from names, relationships or roles.
//! - **Every attached dataset is joined, once,** and the joins connect
//!   every dataset to the model with no cycle; anything else is refused,
//!   naming it.
//! - **`filter`** keeps the rows of the side it is joined to whose keys
//!   the dataset has. It brings no column.
//! - **`enrich`** with only dimensions is a look-up: the dataset must be
//!   unique on its keys. With measures it is budget against actual: the
//!   dataset is aggregated to its keys and joined to the model keeping
//!   rows from both sides, and every dimension the request asks for must
//!   be one of those keys. A look-up joined to such a dataset on its keys
//!   is applied after, on the keys both sides now share.
//! - **The model is regrouped only when it adds up.** When a join key
//!   the request did not ask for made the model's part finer, the result
//!   is grouped back to the request's dimensions, which every measure of
//!   the model must survive: a sum or a count, not a distinct count or a
//!   ratio.

use crate::engine::Session;
use crate::error::{Error, Result};
use crate::schema::{common, ident, Column, ColumnType, Role};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use zsql_datasource::Batches;

/// A dataset attached to a request, under the alias its columns are
/// named by.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Attach {
    pub id: String,
    #[serde(rename = "as")]
    pub alias: String,
}

#[derive(Clone, Copy, Debug, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "lowercase")]
pub enum Kind {
    Filter,
    Enrich,
}

/// One join of a blend: the dataset `join` beside `to`.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct Join {
    pub join: String,
    pub to: String,
    pub on: Vec<On>,
    pub kind: Kind,
}

/// A pair of keys: `left` on the `to` side, `right` on the dataset's;
/// each written as the request names it (`store.region`,
/// `budget.region`).
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct On {
    pub left: String,
    pub right: String,
}

/// A column of the answer, in order.
#[derive(Clone, Debug, PartialEq)]
pub enum Output {
    /// A column of the model's part.
    Model { column: String, name: String, measure: bool, additive: bool },
    /// A column of an attached dataset, by alias.
    Dataset { alias: String, column: String, name: String },
}

/// The model's part as it came back: its rows, and the column each
/// field the joins name was planned as.
pub struct ModelPart {
    pub rows: Batches,
    /// A model key, as a join writes it, to its column.
    pub keys: BTreeMap<String, String>,
    /// Columns planned only because a join needs them.
    pub key_only: Vec<String>,
}

/// A blend, as data prep runs it.
pub struct Blend {
    pub datasets: Vec<Attach>,
    pub joins: Vec<Join>,
    pub outputs: Vec<Output>,
}

/// An attached dataset, found: its live version's columns.
pub(crate) struct Found {
    pub alias: String,
    pub id: String,
    pub version: u32,
    pub columns: Vec<Column>,
}

const MODEL: &str = "model";

fn refuse(m: impl Into<String>) -> Error {
    Error::Invalid(m.into())
}

/// Checks an alias as an alias: a lower-case name, not `model`.
pub fn check_alias(alias: &str) -> Result<()> {
    let ok = !alias.is_empty()
        && alias.len() <= 64
        && alias.starts_with(|c: char| c.is_ascii_lowercase() || c == '_')
        && alias.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit() || b == b'_');
    if !ok || alias == MODEL {
        return Err(refuse(format!(
            "'{alias}' is not an alias: lower-case letters, digits and _, starting with a letter, and not '{MODEL}'"
        )));
    }
    Ok(())
}

/// The order the joins are applied in: each after the side it joins to.
/// Refused when an attached dataset is joined never or twice, a join
/// names a dataset not attached, or the joins leave one unconnected or
/// make a loop.
fn order(blend: &Blend) -> Result<Vec<&Join>> {
    for a in &blend.datasets {
        let n = blend.joins.iter().filter(|j| j.join == a.alias).count();
        if n == 0 {
            return Err(refuse(format!(
                "dataset '{}' is attached and joined to nothing; say how it joins, or do not attach it: nothing is joined for you",
                a.alias
            )));
        }
        if n > 1 {
            return Err(refuse(format!("dataset '{}' is joined {n} times; a dataset is joined once", a.alias)));
        }
    }
    for j in &blend.joins {
        if !blend.datasets.iter().any(|a| a.alias == j.join) {
            return Err(refuse(format!("a join names '{}', which is not attached", j.join)));
        }
        if j.to != MODEL && !blend.datasets.iter().any(|a| a.alias == j.to) {
            return Err(refuse(format!("'{}' joins to '{}', which is neither the model nor attached", j.join, j.to)));
        }
        if j.on.is_empty() {
            return Err(refuse(format!("'{}' joins on nothing; a join names its keys", j.join)));
        }
    }
    let mut connected: Vec<&str> = vec![MODEL];
    let mut out: Vec<&Join> = Vec::new();
    while out.len() < blend.joins.len() {
        let next = blend.joins.iter().find(|j| !out.iter().any(|o| o.join == j.join) && connected.contains(&j.to.as_str()));
        let Some(j) = next else {
            let left: Vec<&str> = blend.joins.iter().filter(|j| !out.iter().any(|o| o.join == j.join)).map(|j| j.join.as_str()).collect();
            return Err(refuse(format!(
                "the joins do not connect {} to the model; they leave it apart or join in a loop",
                left.join(", ")
            )));
        };
        connected.push(&j.join);
        out.push(j);
    }
    Ok(out)
}

/// `alias.column` as the column of the alias it names.
fn column_of<'a>(written: &str, alias: &str, columns: &'a [Column]) -> Result<&'a Column> {
    let bare = written
        .strip_prefix(alias)
        .and_then(|r| r.strip_prefix('.'))
        .ok_or_else(|| refuse(format!("'{written}' is not a column of '{alias}': write it as {alias}.<column>")))?;
    columns.iter().find(|c| c.name.eq_ignore_ascii_case(bare)).ok_or_else(|| refuse(format!("dataset '{alias}' has no column '{bare}'")))
}

/// The name a dataset's column is carried under once it is brought.
fn carried(alias: &str, column: &str) -> String {
    format!("{alias}.{column}")
}

/// A relation being built, and its columns with their types.
struct Rel {
    sql: String,
    columns: Vec<(String, ColumnType)>,
}

impl Rel {
    fn ty(&self, name: &str) -> Result<ColumnType> {
        self.columns
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, t)| *t)
            .ok_or_else(|| Error::Internal(format!("'{name}' is not in a relation of the blend")))
    }
}

/// The keys of a join, each as a column of the `to` side's relation
/// and of the dataset's.
fn keys(j: &Join, to: &Rel, to_names: &dyn Fn(&str) -> Result<String>, found: &Found) -> Result<Vec<(String, String, ColumnType)>> {
    let mut out = Vec::new();
    for on in &j.on {
        let l = to_names(&on.left)?;
        let r = column_of(&on.right, &found.alias, &found.columns)?;
        let lt = to.ty(&l)?;
        let t = common(lt, r.ty).ok_or_else(|| {
            refuse(format!("'{}' ({lt}) and '{}' ({}) do not widen to one type, so they are not keys of one join", on.left, on.right, r.ty))
        })?;
        out.push((l, r.name.clone(), t));
    }
    Ok(out)
}

fn cond(keys: &[(String, String, ColumnType)], l: &str, r: &str) -> String {
    keys.iter()
        .map(|(a, b, t)| format!("CAST({l}.{} AS {}) = CAST({r}.{} AS {})", ident(a), t.sql(), ident(b), t.sql()))
        .collect::<Vec<_>>()
        .join(" AND ")
}

/// Runs a blend: the model's part and the found datasets loaded, the
/// joins applied in order, the answer's columns in the request's order.
pub(crate) fn run(session: &Session, blend: &Blend, model: &ModelPart, found: &[Found]) -> Result<Batches> {
    let joins = order(blend)?;
    let found_of = |alias: &str| found.iter().find(|f| f.alias == alias).ok_or_else(|| Error::Internal(format!("'{alias}' was not found")));

    // The model's part, held at the types a dataset has, as `model`.
    let (model_cols, model_batches) = crate::snapshot::held(&model.rows)?;
    session.load(MODEL, &model_cols, &model_batches)?;

    // What each dataset is asked for.
    let mut asked: BTreeMap<&str, Vec<&Column>> = BTreeMap::new();
    for o in &blend.outputs {
        if let Output::Dataset { alias, column, .. } = o {
            let f = found_of(alias)?;
            let c = f
                .columns
                .iter()
                .find(|c| c.name.eq_ignore_ascii_case(column))
                .ok_or_else(|| refuse(format!("dataset '{alias}' has no column '{column}'")))?;
            asked.entry(alias.as_str()).or_default().push(c);
        }
    }
    let kind_of = |alias: &str| joins.iter().find(|j| j.join == alias).map(|j| j.kind);
    for (alias, cols) in &asked {
        if kind_of(alias) == Some(Kind::Filter) {
            return Err(refuse(format!("'{alias}' is a filter, which brings no column; '{alias}.{}' needs an enrich", cols[0].name)));
        }
    }
    let measures_of = |alias: &str| asked.get(alias).map(|c| c.iter().any(|c| c.role == Role::Measure)).unwrap_or(false);

    // An enrich with measures is budget against actual: it joins to the model.
    let aggregated: Vec<&Join> = joins.iter().copied().filter(|j| j.kind == Kind::Enrich && measures_of(&j.join)).collect();
    for j in &aggregated {
        if j.to != MODEL {
            return Err(refuse(format!(
                "'{}' brings measures, and measures are set beside the model's: join it to model, or ask only for its dimensions",
                j.join
            )));
        }
    }
    // A look-up on such a dataset, joined on its keys, is applied after
    // the outer join, on the keys both sides share.
    let after_outer = |j: &Join| -> Option<&Join> {
        let parent = aggregated.iter().find(|a| a.join == j.to)?;
        let on_keys = j.on.iter().all(|on| parent.on.iter().any(|p| p.right.eq_ignore_ascii_case(&on.left)));
        (j.kind == Kind::Enrich && !measures_of(&j.join) && on_keys).then_some(*parent)
    };
    for j in &joins {
        if aggregated.iter().any(|a| a.join == j.to) && after_outer(j).is_none() {
            return Err(refuse(format!(
                "'{}' joins to '{}', which is aggregated to its keys; join it on those keys, as a look-up",
                j.join, j.to
            )));
        }
    }

    // Each node's relation, children folded in: filters, then look-ups.
    let model_name = |written: &str| -> Result<String> {
        model.keys.get(written).cloned().ok_or_else(|| Error::Internal(format!("the model's part has no column for '{written}'")))
    };
    let mut rels: BTreeMap<String, Rel> = BTreeMap::new();
    rels.insert(MODEL.into(), Rel { sql: ident(MODEL), columns: model_cols.clone() });
    for f in found {
        rels.insert(f.alias.clone(), Rel { sql: ident(&f.alias), columns: f.columns.iter().map(|c| (c.name.clone(), c.ty)).collect() });
    }
    // Children before parents: the joins in reverse of the order they connect.
    let mut n = 0;
    for j in joins.iter().rev() {
        if aggregated.iter().any(|a| a.join == j.join) || after_outer(j).is_some() {
            continue;
        }
        let f = found_of(&j.join)?;
        let child = rels.remove(&j.join).ok_or_else(|| Error::Internal(format!("'{}' has no relation", j.join)))?;
        let parent = rels.remove(&j.to).ok_or_else(|| Error::Internal(format!("'{}' has no relation", j.to)))?;
        let to_alias = j.to.clone();
        let to_names = |written: &str| -> Result<String> {
            if to_alias == MODEL {
                model_name(written)
            } else {
                let pf = found_of(&to_alias)?;
                Ok(column_of(written, &to_alias, &pf.columns)?.name.clone())
            }
        };
        let k = keys(j, &parent, &to_names, f)?;
        n += 1;
        let next = match j.kind {
            Kind::Filter => Rel {
                sql: format!("(SELECT * FROM {} p WHERE EXISTS (SELECT 1 FROM {} c WHERE {}))", parent.sql, child.sql, cond(&k, "p", "c")),
                columns: parent.columns,
            },
            Kind::Enrich => {
                unique(session, &child, &k, &j.join)?;
                let mut columns = parent.columns.clone();
                let mut select: Vec<String> = parent.columns.iter().map(|(c, _)| format!("p.{}", ident(c))).collect();
                for (c, t) in &child.columns {
                    // Its own columns that are asked for, and what its own
                    // look-ups brought.
                    let own = asked.get(j.join.as_str()).is_some_and(|a| a.iter().any(|x| &x.name == c));
                    let brought = c.contains('.');
                    if own || brought {
                        let name = if brought { c.clone() } else { carried(&j.join, c) };
                        select.push(format!("c.{} AS {}", ident(c), ident(&name)));
                        columns.push((name, *t));
                    }
                }
                Rel {
                    sql: format!(
                        "(SELECT {} FROM (SELECT *, row_number() OVER () AS \"__p{n}\" FROM {}) p LEFT JOIN {} c ON {} ORDER BY p.\"__p{n}\")",
                        select.join(", "),
                        parent.sql,
                        child.sql,
                        cond(&k, "p", "c")
                    ),
                    columns,
                }
            }
        };
        rels.insert(j.to.clone(), next);
    }
    let mut root = rels.remove(MODEL).ok_or_else(|| Error::Internal("the model has no relation".into()))?;

    // Budget against actual: each aggregated to its keys, outer-joined.
    let mut request_dims: Vec<String> = Vec::new();
    for o in &blend.outputs {
        match o {
            Output::Model { column, measure: false, .. } => request_dims.push(column.clone()),
            Output::Dataset { alias, column, .. } => {
                let f = found_of(alias)?;
                if f.columns.iter().any(|c| c.name.eq_ignore_ascii_case(column) && c.role == Role::Dimension) {
                    request_dims.push(carried(alias, column));
                }
            }
            _ => {}
        }
    }
    for j in &aggregated {
        let f = found_of(&j.join)?;
        let child = rels.remove(&j.join).ok_or_else(|| Error::Internal(format!("'{}' has no relation", j.join)))?;
        let k = keys(j, &root, &model_name, f)?;
        // Every dimension asked for is a key here, or comes from a look-up
        // on these keys; anything else would repeat the dataset's values.
        let lookups: Vec<&Join> = joins.iter().copied().filter(|x| after_outer(x).is_some_and(|p| p.join == j.join)).collect();
        for d in &request_dims {
            let is_key = k.iter().any(|(l, _, _)| l == d);
            let from_lookup = lookups.iter().any(|l| d.starts_with(&format!("{}.", l.join)));
            if !is_key && !from_lookup {
                let shown = blend.outputs.iter().find_map(|o| match o {
                    Output::Model { column, name, .. } if column == d => Some(name.clone()),
                    _ => None,
                });
                return Err(refuse(format!(
                    "'{}' is asked for, and '{}' is not set beside the model by it: every dimension of a request with {}'s measures must be one of its keys ({}), or its values would repeat",
                    shown.unwrap_or_else(|| d.clone()),
                    j.join,
                    j.join,
                    j.on.iter().map(|o| o.left.clone()).collect::<Vec<_>>().join(", ")
                )));
            }
        }
        let measures: Vec<&Column> =
            asked.get(j.join.as_str()).into_iter().flatten().copied().filter(|c| c.role == Role::Measure).collect();
        for m in &measures {
            if !m.ty.is_number() {
                return Err(refuse(format!(
                    "'{}.{}' is a measure and is {}; a measure set beside the model is a number",
                    j.join, m.name, m.ty
                )));
            }
        }
        let group: Vec<String> = k.iter().map(|(_, r, _)| ident(r)).collect();
        let mut agg_select: Vec<String> = group.clone();
        for m in &measures {
            agg_select.push(format!("CAST(sum({}) AS {}) AS {}", ident(&m.name), m.ty.sql(), ident(&carried(&j.join, &m.name))));
        }
        let agg = format!("(SELECT {} FROM {} GROUP BY {})", agg_select.join(", "), child.sql, group.join(", "));
        let mut select = Vec::new();
        let mut columns = Vec::new();
        for (c, t) in &root.columns {
            match k.iter().find(|(l, _, _)| l == c) {
                Some((l, r, kt)) => {
                    select.push(format!(
                        "COALESCE(CAST(p.{} AS {}), CAST(a.{} AS {})) AS {}",
                        ident(l),
                        kt.sql(),
                        ident(r),
                        kt.sql(),
                        ident(l)
                    ));
                    columns.push((c.clone(), *kt));
                }
                None => {
                    select.push(format!("p.{}", ident(c)));
                    columns.push((c.clone(), *t));
                }
            }
        }
        for m in &measures {
            let name = carried(&j.join, &m.name);
            select.push(format!("a.{}", ident(&name)));
            columns.push((name, m.ty));
        }
        root = Rel {
            sql: format!("(SELECT {} FROM {} p FULL OUTER JOIN {agg} a ON {})", select.join(", "), root.sql, cond(&k, "p", "a")),
            columns,
        };
        // Its look-ups, on the keys both sides now share.
        for l in lookups {
            let lf = found_of(&l.join)?;
            let lrel = rels.remove(&l.join).ok_or_else(|| Error::Internal(format!("'{}' has no relation", l.join)))?;
            let parent_names = |written: &str| -> Result<String> {
                let bare = column_of(written, &j.join, &f.columns)?.name.clone();
                k.iter()
                    .find(|(_, r, _)| *r == bare)
                    .map(|(l, _, _)| l.clone())
                    .ok_or_else(|| Error::Internal(format!("'{written}' is not a key")))
            };
            let lk = keys(l, &root, &parent_names, lf)?;
            unique(session, &lrel, &lk, &l.join)?;
            let mut select: Vec<String> = root.columns.iter().map(|(c, _)| format!("p.{}", ident(c))).collect();
            let mut columns = root.columns.clone();
            for c in asked.get(l.join.as_str()).into_iter().flatten() {
                let name = carried(&l.join, &c.name);
                select.push(format!("c.{} AS {}", ident(&c.name), ident(&name)));
                columns.push((name, c.ty));
            }
            root = Rel {
                sql: format!("(SELECT {} FROM {} p LEFT JOIN {} c ON {})", select.join(", "), root.sql, lrel.sql, cond(&lk, "p", "c")),
                columns,
            };
        }
    }

    // The answer: regrouped to the request's dimensions when a key it did
    // not ask for made the model's part finer.
    let finer = model.key_only.iter().any(|k| root.columns.iter().any(|(c, _)| c == k));
    let mut select = Vec::new();
    let mut group = Vec::new();
    for o in &blend.outputs {
        let (col, name, measure) = match o {
            Output::Model { column, name, measure, additive } => {
                if finer && *measure && !additive {
                    return Err(refuse(format!(
                        "'{name}' does not add up, and a join key the request does not show makes the model's part finer than the answer; \
                         ask for the keys too, or make a dataset from a semantic query at the grain needed"
                    )));
                }
                (column.clone(), name.clone(), *measure)
            }
            Output::Dataset { alias, column, name } => {
                let f = found_of(alias)?;
                let c = f
                    .columns
                    .iter()
                    .find(|c| c.name.eq_ignore_ascii_case(column))
                    .ok_or_else(|| refuse(format!("dataset '{alias}' has no column '{column}'")))?;
                (carried(alias, &c.name), name.clone(), c.role == Role::Measure)
            }
        };
        if finer {
            if measure {
                // A sum is the type it adds, not the engine's wider one.
                let t = root.ty(&col)?;
                select.push(format!("CAST(sum({}) AS {}) AS {}", ident(&col), t.sql(), ident(&name)));
            } else {
                select.push(format!("{} AS {}", ident(&col), ident(&name)));
                group.push(ident(&col));
            }
        } else {
            select.push(format!("{} AS {}", ident(&col), ident(&name)));
        }
    }
    let sql = if finer && !group.is_empty() {
        format!("SELECT {} FROM {} GROUP BY {} ORDER BY {}", select.join(", "), root.sql, group.join(", "), group.join(", "))
    } else {
        format!("SELECT {} FROM {}", select.join(", "), root.sql)
    };
    session.query(&sql)
}

/// Refused when a look-up's side repeats a key: it would multiply rows.
fn unique(session: &Session, rel: &Rel, k: &[(String, String, ColumnType)], alias: &str) -> Result<()> {
    let cols: Vec<String> = k.iter().map(|(_, r, _)| ident(r)).collect();
    let n = session.count(&format!(
        "SELECT count(*) FROM (SELECT {c} FROM {} GROUP BY {c} HAVING count(*) > 1)",
        rel.sql,
        c = cols.join(", ")
    ))?;
    if n > 0 {
        return Err(Error::Unprocessable(format!(
            "'{alias}' is not unique on its keys ({n} repeat), so joining it would multiply rows; aggregate it in its recipe, or ask for a measure of it"
        )));
    }
    Ok(())
}
