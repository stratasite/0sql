//! Compiling: a `steps` node's steps as tables, each over the one
//! before, with the columns there at each, their types, roles and
//! ancestry, and the checks a run makes before it trusts a conversion;
//! and the joins and appends between nodes.
//!
//! The compiler walks the steps with the columns there at each, so it
//! knows what every step reads before anything runs. A step that reads
//! a column not there at its point is unresolved: it is passed over,
//! and so is every later step that reads what it would have made, and
//! all of them are named. A graph with one is refused, naming its node.

use crate::error::{Error, Result};
use crate::formula;
use crate::lineage::Origin;
use crate::schema::{ident, literal, Column, ColumnType, Role};
use crate::source::infer;
use crate::steps::{AggregateFn, JoinKey, JoinKind, Mapping, SortKey, Step, STEPS_LIMIT};
use serde::{Deserialize, Serialize};
use serde_json::Value;

/// A relation in a run's session: its name, as SQL, and its columns,
/// each with where it came from.
#[derive(Clone, Debug)]
pub struct Rel {
    pub name: String,
    pub columns: Vec<Column>,
    pub ancestry: Vec<Vec<Origin>>,
}

/// One step, compiled: the table it makes.
pub struct Stage {
    /// The table's name, unquoted.
    pub name: String,
    /// The step, from 1.
    pub step: usize,
    pub op: &'static str,
    /// What it reads, as SQL.
    pub input: String,
    pub sql: StageSql,
    /// The columns before it and after.
    pub before: Vec<Column>,
    pub after: Vec<Column>,
    /// The columns it rewrites in place, by their names after it: what
    /// a run counts the values it changed in.
    pub rewrites: Vec<String>,
}

/// How a stage is made.
pub enum StageSql {
    /// A select over the stage before.
    Select(String),
    /// A `sql` step, run in a session of its own.
    Sql(crate::sql::SqlStage),
}

/// A node's steps, compiled.
pub struct Chain {
    pub stages: Vec<Stage>,
    /// What the last stage makes.
    pub out: Rel,
    /// Conversions to check before the result is read.
    pub checks: Vec<Check>,
    /// Steps that read a column not there; empty when it runs.
    pub unresolved: Vec<Unresolved>,
}

/// A conversion, or a join, that keeps a value only if it is sound: the
/// rows of `relation` where `failing` holds are the ones that are not.
#[derive(Clone, Debug)]
pub struct Check {
    pub relation: String,
    /// The column whose values are shown when it fails.
    pub column: String,
    pub failing: String,
    /// What a failure says, `{n}` standing for how many failed.
    pub says: String,
}

/// What a `sql` step's query makes over these inputs, each by its
/// alias: DuckDB's answer, or its refusal in its own words. See
/// `sql::describe`.
pub type Describe<'a> =
    &'a dyn Fn(&[(String, Vec<(String, ColumnType)>)], &str) -> Result<std::result::Result<Vec<(String, ColumnType)>, String>>;

/// A step that reads a column not there at its point.
#[derive(Clone, Debug, Serialize, Deserialize, PartialEq, Eq)]
pub struct Unresolved {
    pub column: String,
    /// The step, from 1; 0 for what a node reads that is not a step.
    pub step: usize,
    pub op: String,
}

impl Unresolved {
    pub fn describe(&self) -> String {
        if self.step == 0 {
            format!("{} reads column '{}', which is not there", self.op, self.column)
        } else {
            format!("step {} ({}) reads column '{}', which is not there at that step", self.step, self.op, self.column)
        }
    }
}

/// The longest name a column is given.
const NAME_LIMIT: usize = 200;

pub struct Cols {
    pub columns: Vec<Column>,
    pub ancestry: Vec<Vec<Origin>>,
}

impl Cols {
    pub fn of(rel: &Rel) -> Cols {
        Cols { columns: rel.columns.clone(), ancestry: rel.ancestry.clone() }
    }

    fn find(&self, name: &str) -> Option<usize> {
        self.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name))
    }

    fn list(&self) -> String {
        if self.columns.is_empty() {
            return "NULL AS \"_\"".into();
        }
        self.columns.iter().map(|c| ident(&c.name)).collect::<Vec<_>>().join(", ")
    }

    /// The columns, comma-separated; nothing when there are none.
    fn list_or_empty(&self) -> String {
        self.columns.iter().map(|c| ident(&c.name)).collect::<Vec<_>>().join(", ")
    }

    /// The select list with one column's expression replaced.
    fn replacing(&self, i: usize, expr: &str) -> String {
        self.columns
            .iter()
            .enumerate()
            .map(|(j, c)| if i == j { format!("{expr} AS {}", ident(&c.name)) } else { ident(&c.name) })
            .collect::<Vec<_>>()
            .join(", ")
    }
}

/// A node's steps over `input`, with `today()` pinned to `pinned_at`.
/// Each stage's table is named `{prefix}_t{n}`.
pub fn chain(prefix: &str, input: &Rel, steps: &[Step], pinned_at: &str, describe: Describe<'_>) -> Result<Chain> {
    if steps.len() > STEPS_LIMIT {
        return Err(Error::Invalid(format!("a steps node has at most {STEPS_LIMIT} steps")));
    }
    let mut cols = Cols::of(input);
    let mut prev = input.name.clone();
    let mut stages = Vec::new();
    let mut checks = Vec::new();
    let mut unresolved = Vec::new();
    for (i, step) in steps.iter().enumerate() {
        let n = i + 1;
        // What the step reads that is not there: it is passed over.
        let missing: Vec<Unresolved> = reads(step)?
            .into_iter()
            .filter(|c| cols.find(c).is_none())
            .map(|column| Unresolved { column, step: n, op: step.op().into() })
            .collect();
        if !missing.is_empty() {
            unresolved.extend(missing);
            continue;
        }
        let at = |e: Error| match e {
            Error::Invalid(m) => Error::Invalid(format!("step {n} ({}): {m}", step.op())),
            other => other,
        };
        let before = cols.columns.clone();
        let rewrites: Vec<String> = rewritten(step).iter().filter_map(|c| cols.find(c)).map(|i| cols.columns[i].name.clone()).collect();
        let name = format!("{prefix}_t{n}");
        let sql = if let Step::Sql { query } = step {
            let query = crate::sql::checked(query).map_err(at)?;
            let typed: Vec<(String, ColumnType)> = cols.columns.iter().map(|c| (c.name.clone(), c.ty)).collect();
            match describe(&[("input".to_string(), typed.clone())], &query)? {
                Ok(made) => {
                    sql_columns(&mut cols, &made);
                    StageSql::Sql(crate::sql::SqlStage {
                        label: format!("step {n} (sql)"),
                        inputs: vec![crate::sql::SqlInput { alias: "input".into(), relation: prev.clone(), columns: typed }],
                        query,
                        columns: made,
                    })
                }
                Err(refusal) => match crate::sql::missing_column(&refusal) {
                    Some(column) => {
                        unresolved.push(Unresolved { column, step: n, op: "sql".into() });
                        continue;
                    }
                    None => return Err(Error::Invalid(format!("step {n} (sql): {refusal}"))),
                },
            }
        } else {
            StageSql::Select(one(step, n, &prev, &mut cols, &mut checks, pinned_at).map_err(at)?)
        };
        stages.push(Stage {
            name: name.clone(),
            step: n,
            op: step.op(),
            input: prev.clone(),
            sql,
            before,
            after: cols.columns.clone(),
            rewrites,
        });
        prev = ident(&name);
    }
    Ok(Chain { stages, out: Rel { name: prev, columns: cols.columns, ancestry: cols.ancestry }, checks, unresolved })
}

/// What a `sql` query made, as the columns after it: a black box, each
/// column it makes from all it was given. A column it keeps by name
/// keeps its role, while it is still a number or still not one; the
/// rest are inferred.
pub(crate) fn sql_columns(cols: &mut Cols, made: &[(String, ColumnType)]) {
    let mut from: Vec<Origin> = cols.ancestry.iter().flatten().cloned().collect();
    from.sort();
    from.dedup();
    let columns: Vec<Column> = made
        .iter()
        .map(|(name, ty)| {
            let kept = cols.find(name).map(|i| &cols.columns[i]).filter(|c| c.ty.is_number() == ty.is_number());
            Column { name: name.clone(), ty: *ty, role: kept.map_or_else(|| Role::inferred(name, *ty), |c| c.role) }
        })
        .collect();
    cols.ancestry = vec![from; columns.len()];
    cols.columns = columns;
}

/// The columns a step rewrites in place, as it names them.
fn rewritten(step: &Step) -> Vec<String> {
    match step {
        Step::FillEmpty { column, .. }
        | Step::FindAndReplaceVal { column, .. }
        | Step::RemapVal { column, .. }
        | Step::UpdateColumn { column, .. } => vec![column.clone()],
        Step::TrimVal { columns }
        | Step::UpperCase { columns }
        | Step::LowerCase { columns }
        | Step::TitleCase { columns }
        | Step::SnakeCase { columns }
        | Step::CamelCase { columns }
        | Step::FillDown { columns } => columns.clone(),
        _ => Vec::new(),
    }
}

/// The columns a step reads, by name as the step writes them.
pub fn reads(step: &Step) -> Result<Vec<String>> {
    Ok(match step {
        Step::RenameColumn { column, .. } | Step::RetypeColumn { column, .. } | Step::FillEmpty { column, .. } => vec![column.clone()],
        Step::FindAndReplaceVal { column, .. } | Step::RemapVal { column, .. } => vec![column.clone()],
        Step::RemoveColumns { columns } | Step::ReorderColumns { columns } | Step::TrimVal { columns } => columns.clone(),
        Step::UpperCase { columns }
        | Step::LowerCase { columns }
        | Step::TitleCase { columns }
        | Step::SnakeCase { columns }
        | Step::CamelCase { columns } => columns.clone(),
        Step::Dedupe { columns } | Step::FillDown { columns } => columns.clone(),
        Step::Unpivot { columns, keep, .. } => columns.iter().chain(keep).cloned().collect(),
        Step::AddColumn { formula, .. } | Step::Filter { formula } => formula::names(formula)?,
        // It reads the column it updates, whether its formula does or not.
        Step::UpdateColumn { column, formula } => {
            let mut r = formula::names(formula)?;
            r.push(column.clone());
            r
        }
        Step::Sort { by } => by.iter().map(|k| k.column.clone()).collect(),
        Step::Group { by, aggregates } => by.iter().cloned().chain(aggregates.iter().filter_map(|a| a.column.clone())).collect(),
        // A black box: what it reads is found when DuckDB is asked.
        Step::Sql { .. } => Vec::new(),
    })
}

fn check_name(name: &str, cols: &Cols, what: &str) -> Result<()> {
    if name.trim().is_empty() || name.trim() != name || name.chars().count() > NAME_LIMIT || name.chars().any(char::is_control) {
        return Err(Error::Invalid(format!(
            "'{name}' is not a column name: at most {NAME_LIMIT} characters, no control characters, nothing blank at either end"
        )));
    }
    if let Some(i) = cols.find(name) {
        return Err(Error::Invalid(format!("{what} '{name}', and there is a column '{}' already", cols.columns[i].name)));
    }
    Ok(())
}

fn text_column(cols: &Cols, i: usize, what: &str) -> Result<()> {
    let c = &cols.columns[i];
    if c.ty != ColumnType::Text {
        return Err(Error::Invalid(format!("{what} works on text, and '{}' is {}", c.name, c.ty)));
    }
    Ok(())
}

/// A value from a request as a literal of a column's type, refused when
/// it is not one.
pub(crate) fn value_sql(v: &Value, ty: ColumnType, column: &str) -> Result<String> {
    let text = match v {
        Value::Null => return Ok("NULL".into()),
        Value::String(s) => s.clone(),
        Value::Number(n) => n.to_string(),
        Value::Bool(b) => b.to_string(),
        other => return Err(Error::Invalid(format!("{other} is not a value a column holds"))),
    };
    let fits = ty == ColumnType::Text || {
        let t = infer(std::iter::once(text.as_str()));
        t.widens_to(ty) || (t == ColumnType::Date && ty == ColumnType::Timestamp)
    };
    if !fits {
        return Err(Error::Invalid(format!("'{text}' is not a value of column '{column}', which is {ty}")));
    }
    Ok(format!("CAST({} AS {})", literal(text.trim_matches(|c: char| c.is_whitespace() && ty != ColumnType::Text)), ty.sql()))
}

fn one(step: &Step, n: usize, prev: &str, cols: &mut Cols, checks: &mut Vec<Check>, pinned_at: &str) -> Result<String> {
    let found = |cols: &Cols, name: &str| cols.find(name).ok_or_else(|| Error::Internal(format!("'{name}' was resolved and is not there")));
    Ok(match step {
        Step::RenameColumn { column, to } => {
            let i = found(cols, column)?;
            if !column.eq_ignore_ascii_case(to) {
                check_name(to, cols, "it renames a column to")?;
            }
            let old = cols.columns[i].name.clone();
            cols.columns[i].name = to.clone();
            let list: Vec<String> = cols
                .columns
                .iter()
                .enumerate()
                .map(|(j, c)| if j == i { format!("{} AS {}", ident(&old), ident(to)) } else { ident(&c.name) })
                .collect();
            format!("SELECT {} FROM {prev}", list.join(", "))
        }
        Step::RetypeColumn { column, ty, format } => {
            let i = found(cols, column)?;
            let from = cols.columns[i].ty;
            let x = ident(&cols.columns[i].name);
            let (expr, failing) = match (from, ty, format) {
                (ColumnType::Text, ColumnType::Date | ColumnType::Timestamp, Some(f)) => {
                    if !f.contains('%') || f.len() > 64 {
                        return Err(Error::Invalid("a date's format is in strptime's form, such as '%d/%m/%Y'".into()));
                    }
                    let parsed = format!("try_strptime({x}, {})", literal(f));
                    (format!("CAST({parsed} AS {})", ty.sql()), format!("{x} IS NOT NULL AND {parsed} IS NULL"))
                }
                (_, _, Some(_)) => return Err(Error::Invalid("a format is for text made a date or a timestamp".into())),
                (ColumnType::Text, _, None) => {
                    let t = format!("TRY_CAST(trim({x}) AS {})", ty.sql());
                    (t.clone(), format!("{x} IS NOT NULL AND {t} IS NULL"))
                }
                (_, ColumnType::Text, None) => (format!("CAST({x} AS VARCHAR)"), "FALSE".into()),
                (_, _, None) if from.widens_to(*ty) => (format!("CAST({x} AS {})", ty.sql()), "FALSE".into()),
                // A narrowing keeps a value only if it comes back the same.
                (_, _, None) => {
                    let t = format!("TRY_CAST({x} AS {})", ty.sql());
                    (t.clone(), format!("{x} IS NOT NULL AND ({t} IS NULL OR CAST({t} AS {}) <> {x})", from.sql()))
                }
            };
            if failing != "FALSE" {
                let says = format!(
                    "step {n} (retype_column) makes column '{}' {ty} from {from}, and {{n}} value(s) do not convert without loss",
                    cols.columns[i].name
                );
                checks.push(Check { relation: prev.to_string(), column: cols.columns[i].name.clone(), failing, says });
            }
            cols.columns[i].ty = *ty;
            if !ty.is_number() && cols.columns[i].role == Role::Measure {
                cols.columns[i].role = Role::Dimension;
            }
            format!("SELECT {} FROM {prev}", cols.replacing(i, &expr))
        }
        Step::RemoveColumns { columns } => {
            if columns.is_empty() {
                return Err(Error::Invalid("it removes no column".into()));
            }
            let gone: Vec<usize> = columns.iter().map(|c| found(cols, c)).collect::<Result<_>>()?;
            let mut j = 0;
            cols.columns.retain(|_| {
                j += 1;
                !gone.contains(&(j - 1))
            });
            let mut j = 0;
            cols.ancestry.retain(|_| {
                j += 1;
                !gone.contains(&(j - 1))
            });
            format!("SELECT {} FROM {prev}", cols.list())
        }
        Step::ReorderColumns { columns } => {
            let mut order: Vec<usize> = Vec::new();
            for c in columns {
                let i = found(cols, c)?;
                if order.contains(&i) {
                    return Err(Error::Invalid(format!("it names '{c}' twice")));
                }
                order.push(i);
            }
            let rest: Vec<usize> = (0..cols.columns.len()).filter(|i| !order.contains(i)).collect();
            order.extend(rest);
            cols.columns = order.iter().map(|i| cols.columns[*i].clone()).collect();
            cols.ancestry = order.iter().map(|i| cols.ancestry[*i].clone()).collect();
            format!("SELECT {} FROM {prev}", cols.list())
        }
        Step::AddColumn { column, formula } => {
            check_name(column, cols, "it makes a column")?;
            let f = formula::compile(formula, &formula::Scope { columns: &cols.columns, pinned_at })?;
            let mut from: Vec<Origin> = f.reads.iter().filter_map(|r| cols.find(r)).flat_map(|i| cols.ancestry[i].clone()).collect();
            from.sort();
            from.dedup();
            let select = format!("SELECT *, CAST({} AS {}) AS {} FROM {prev}", f.sql, f.ty.sql(), ident(column));
            cols.columns.push(Column { name: column.clone(), ty: f.ty, role: Role::inferred(column, f.ty) });
            cols.ancestry.push(from);
            select
        }
        Step::UpdateColumn { column, formula } => {
            let i = found(cols, column)?;
            let f = formula::compile(formula, &formula::Scope { columns: &cols.columns, pinned_at })?;
            let mut from: Vec<Origin> = f.reads.iter().filter_map(|r| cols.find(r)).flat_map(|j| cols.ancestry[j].clone()).collect();
            from.sort();
            from.dedup();
            let select = format!("SELECT {} FROM {prev}", cols.replacing(i, &format!("CAST({} AS {})", f.sql, f.ty.sql())));
            let c = &mut cols.columns[i];
            c.ty = f.ty;
            if !f.ty.is_number() {
                c.role = Role::Dimension;
            }
            cols.ancestry[i] = from;
            select
        }
        Step::Filter { formula } => {
            let f = formula::compile(formula, &formula::Scope { columns: &cols.columns, pinned_at })?;
            if f.ty != ColumnType::Boolean {
                return Err(Error::Invalid(format!("a filter is true or false, and `{formula}` is {}", f.ty)));
            }
            format!("SELECT * FROM {prev} WHERE {}", f.sql)
        }
        Step::Sort { by } => {
            if by.is_empty() {
                return Err(Error::Invalid("it sorts by nothing".into()));
            }
            format!("SELECT * FROM {prev} ORDER BY {}", order_by(by, cols)?)
        }
        Step::Dedupe { columns } => {
            let keys = if columns.is_empty() {
                cols.list()
            } else {
                columns.iter().map(|c| found(cols, c).map(|i| ident(&cols.columns[i].name))).collect::<Result<Vec<_>>>()?.join(", ")
            };
            // The first of each set, in the order the rows came.
            format!(
                "SELECT {} FROM (SELECT *, row_number() OVER () AS \"__position\" FROM {prev}) \
                 QUALIFY row_number() OVER (PARTITION BY {keys} ORDER BY \"__position\") = 1 ORDER BY \"__position\"",
                cols.list()
            )
        }
        Step::FillEmpty { column, value } => {
            let i = found(cols, column)?;
            let v = value_sql(value, cols.columns[i].ty, &cols.columns[i].name)?;
            format!("SELECT {} FROM {prev}", cols.replacing(i, &format!("COALESCE({}, {v})", ident(&cols.columns[i].name))))
        }
        Step::FindAndReplaceVal { column, find, with, whole } => {
            let i = found(cols, column)?;
            text_column(cols, i, "find_and_replace_val")?;
            if find.is_empty() {
                return Err(Error::Invalid("it finds nothing: 'find' is empty".into()));
            }
            let x = ident(&cols.columns[i].name);
            let expr = if *whole {
                format!("CASE WHEN {x} = {} THEN {} ELSE {x} END", literal(find), literal(with))
            } else {
                format!("replace({x}, {}, {})", literal(find), literal(with))
            };
            format!("SELECT {} FROM {prev}", cols.replacing(i, &expr))
        }
        Step::RemapVal { column, values } => {
            let i = found(cols, column)?;
            format!("SELECT {} FROM {prev}", cols.replacing(i, &remap_sql(&cols.columns[i], values)?))
        }
        Step::TrimVal { columns }
        | Step::UpperCase { columns }
        | Step::LowerCase { columns }
        | Step::TitleCase { columns }
        | Step::SnakeCase { columns }
        | Step::CamelCase { columns } => {
            if columns.is_empty() {
                return Err(Error::Invalid("it names no column".into()));
            }
            let mut list: Vec<String> = cols.columns.iter().map(|c| ident(&c.name)).collect();
            for c in columns {
                let i = found(cols, c)?;
                text_column(cols, i, step.op())?;
                let x = ident(&cols.columns[i].name);
                let expr = match step {
                    Step::UpperCase { .. } => format!("upper({x})"),
                    Step::LowerCase { .. } => format!("lower({x})"),
                    Step::TitleCase { .. } => formula::title_case(&x),
                    Step::SnakeCase { .. } => formula::snake_case(&x),
                    Step::CamelCase { .. } => formula::camel_case(&x),
                    _ => format!("trim({x})"),
                };
                list[i] = format!("{expr} AS {x}");
            }
            format!("SELECT {} FROM {prev}", list.join(", "))
        }
        Step::FillDown { columns } => {
            if columns.is_empty() {
                return Err(Error::Invalid("it names no column".into()));
            }
            let mut list: Vec<String> = cols.columns.iter().map(|c| ident(&c.name)).collect();
            for c in columns {
                let i = found(cols, c)?;
                let x = ident(&cols.columns[i].name);
                // Blank text is empty too: a step before may have made it.
                // A value that is there is kept as it is, spaces and all.
                let value =
                    if cols.columns[i].ty == ColumnType::Text { format!("CASE WHEN trim({x}) <> '' THEN {x} END") } else { x.clone() };
                list[i] = format!(
                    "COALESCE({value}, last_value({value} IGNORE NULLS) OVER (ORDER BY \"__position\" ROWS BETWEEN UNBOUNDED PRECEDING AND CURRENT ROW)) AS {x}"
                );
            }
            format!(
                "SELECT {} FROM (SELECT *, row_number() OVER () AS \"__position\" FROM {prev}) ORDER BY \"__position\"",
                list.join(", ")
            )
        }
        Step::Unpivot { columns, keep, name, value, keep_empty } => unpivot(prev, cols, (columns, keep), (name, value), *keep_empty)?,
        Step::Group { by, aggregates } => group(by, aggregates, prev, cols)?,
        Step::Sql { .. } => return Err(Error::Internal("a sql step is compiled on its own".into())),
    })
}

/// A join between two nodes, as a node asks for it.
pub struct JoinSpec<'a> {
    pub on: &'a [JoinKey],
    pub kind: JoinKind,
    /// The columns brought from the right; all but its keys when none.
    pub columns: Option<&'a [String]>,
    /// Put before each brought column's name.
    pub prefix: Option<&'a str>,
}

impl JoinSpec<'_> {
    /// The columns it names that a side does not have: `(right, name)`.
    pub fn missing(&self, left: &Rel, right: &Rel) -> Vec<(bool, String)> {
        let has = |r: &Rel, n: &str| r.columns.iter().any(|c| c.name.eq_ignore_ascii_case(n));
        let mut out = Vec::new();
        for k in self.on {
            if !has(left, &k.left) {
                out.push((false, k.left.clone()));
            }
            if !has(right, &k.right) {
                out.push((true, k.right.clone()));
            }
        }
        for c in self.columns.into_iter().flatten() {
            if !has(right, c) {
                out.push((true, c.clone()));
            }
        }
        out
    }
}

/// A join, compiled.
pub struct Joined {
    pub select: String,
    pub out: Cols,
    /// The right side repeating a key: refused at run time, since it
    /// would multiply rows.
    pub check: Check,
    /// The condition between `l` and `r`, for counting what matched.
    pub on: String,
}

fn column_of<'a>(rel: &'a Rel, name: &str) -> Result<&'a Column> {
    rel.columns
        .iter()
        .find(|c| c.name.eq_ignore_ascii_case(name))
        .ok_or_else(|| Error::Internal(format!("'{name}' was resolved and is not there")))
}

/// The right side's columns beside the left's, by keys whose types meet:
/// `left` keeps every row of the left (a look-up), `inner` only those
/// that match. The right must be unique on its keys, so no row is
/// multiplied. `right_label` names it in what is said.
pub fn join(left: &Rel, right: &Rel, right_label: &str, spec: &JoinSpec<'_>) -> Result<Joined> {
    if spec.on.is_empty() {
        return Err(Error::Invalid("a join names its keys: on".into()));
    }
    let mut cols = Cols::of(left);
    let mut conds = Vec::new();
    let mut right_keys = Vec::new();
    for k in spec.on {
        let l = column_of(left, &k.left)?;
        let r = column_of(right, &k.right)?;
        let t = crate::schema::common(l.ty, r.ty).ok_or_else(|| {
            Error::Invalid(format!(
                "its keys meet '{}' ({}) with '{}' of {right_label} ({}), which do not widen to one type",
                l.name, l.ty, r.name, r.ty
            ))
        })?;
        conds.push(format!("CAST(l.{} AS {}) = CAST(r.{} AS {})", ident(&l.name), t.sql(), ident(&r.name), t.sql()));
        right_keys.push(r.name.clone());
    }
    let keys: Vec<String> = right_keys.iter().map(|k| ident(k)).collect();
    let check = Check {
        // A null key joins nothing, so two of them are no repeat.
        relation: format!(
            "(SELECT {k} FROM {table} WHERE {nn} GROUP BY {k} HAVING count(*) > 1)",
            k = keys.join(", "),
            table = right.name,
            nn = keys.iter().map(|k| format!("{k} IS NOT NULL")).collect::<Vec<_>>().join(" AND ")
        ),
        column: right_keys[0].clone(),
        failing: "TRUE".into(),
        says: format!("{right_label} is not unique on its keys, and {{n}} key value(s) repeat; joining it would multiply rows"),
    };
    let brought: Vec<(usize, &Column)> = match spec.columns {
        Some(names) => names
            .iter()
            .map(|c| {
                let i = right.columns.iter().position(|x| x.name.eq_ignore_ascii_case(c));
                i.map(|i| (i, &right.columns[i])).ok_or_else(|| Error::Internal(format!("'{c}' was resolved and is not there")))
            })
            .collect::<Result<_>>()?,
        None => right.columns.iter().enumerate().filter(|(_, c)| !right_keys.iter().any(|k| k.eq_ignore_ascii_case(&c.name))).collect(),
    };
    let mut select: Vec<String> = cols.columns.iter().map(|c| format!("l.{}", ident(&c.name))).collect();
    for (i, c) in brought {
        let name = format!("{}{}", spec.prefix.unwrap_or(""), c.name);
        check_name(&name, &cols, &format!("it brings '{}' of {right_label} as", c.name))?;
        select.push(format!("r.{} AS {}", ident(&c.name), ident(&name)));
        cols.columns.push(Column { name, ..c.clone() });
        cols.ancestry.push(right.ancestry.get(i).cloned().unwrap_or_default());
    }
    let how = match spec.kind {
        JoinKind::Left => "LEFT JOIN",
        JoinKind::Inner => "JOIN",
    };
    let on = conds.join(" AND ");
    let select = format!(
        "SELECT {} FROM (SELECT *, row_number() OVER () AS \"__position\" FROM {}) l {how} {} r ON {on} ORDER BY l.\"__position\"",
        select.join(", "),
        left.name,
        right.name,
    );
    Ok(Joined { select, out: cols, check, on })
}

/// The other side's rows after these: columns met by name, each at the
/// type both widen to; a column only one side has is empty on the other.
pub fn append(prev: &Rel, other: &Rel, label: &str) -> Result<(String, Cols)> {
    let mut cols = Cols::of(prev);
    let mut left = Vec::new();
    let mut right = Vec::new();
    for (i, c) in cols.columns.iter_mut().enumerate() {
        let x = ident(&c.name);
        match other.columns.iter().position(|r| r.name.eq_ignore_ascii_case(&c.name)) {
            Some(j) => {
                let r = &other.columns[j];
                let t = crate::schema::common(c.ty, r.ty).ok_or_else(|| {
                    Error::Invalid(format!("column '{}' is {} here and {} in {label}, which do not widen to one type", c.name, c.ty, r.ty))
                })?;
                left.push(format!("CAST({x} AS {}) AS {x}", t.sql()));
                right.push(format!("CAST({} AS {}) AS {x}", ident(&r.name), t.sql()));
                c.ty = t;
                cols.ancestry[i].extend(other.ancestry.get(j).cloned().unwrap_or_default());
                cols.ancestry[i].sort();
                cols.ancestry[i].dedup();
            }
            None => {
                left.push(x.clone());
                right.push(format!("CAST(NULL AS {}) AS {x}", c.ty.sql()));
            }
        }
    }
    for (j, r) in other.columns.iter().enumerate() {
        if cols.columns.iter().any(|c| c.name.eq_ignore_ascii_case(&r.name)) {
            continue;
        }
        let x = ident(&r.name);
        left.push(format!("CAST(NULL AS {}) AS {x}", r.ty.sql()));
        right.push(x);
        cols.columns.push(r.clone());
        cols.ancestry.push(other.ancestry.get(j).cloned().unwrap_or_default());
    }
    Ok((format!("SELECT {} FROM {} UNION ALL SELECT {} FROM {}", left.join(", "), prev.name, right.join(", "), other.name), cols))
}

/// The most columns an unpivot makes rows of.
const UNPIVOT_LIMIT: usize = 1000;

/// Columns made rows, in the order they were columns, each row's after
/// the row before's.
fn unpivot(
    prev: &str,
    cols: &mut Cols,
    (columns, keep): (&[String], &[String]),
    (name, value): (&str, &str),
    keep_empty: bool,
) -> Result<String> {
    let idx = |c: &String| cols.find(c).ok_or_else(|| Error::Internal(format!("'{c}' was resolved and is not there")));
    let turned: Vec<usize> = match (columns.is_empty(), keep.is_empty()) {
        (false, true) => {
            let mut out = Vec::new();
            for c in columns {
                let i = idx(c)?;
                if out.contains(&i) {
                    return Err(Error::Invalid(format!("it names '{c}' twice")));
                }
                out.push(i);
            }
            out
        }
        (true, false) => {
            let kept: Vec<usize> = keep.iter().map(idx).collect::<Result<_>>()?;
            (0..cols.columns.len()).filter(|i| !kept.contains(i)).collect()
        }
        _ => return Err(Error::Invalid("name the columns to unpivot, or the ones to keep, and not both".into())),
    };
    if turned.is_empty() {
        return Err(Error::Invalid("every column is kept, so there is nothing to unpivot".into()));
    }
    if turned.len() > UNPIVOT_LIMIT {
        return Err(Error::Invalid(format!("it unpivots {} columns, and the most is {UNPIVOT_LIMIT}", turned.len())));
    }
    let mut ty = cols.columns[turned[0]].ty;
    for &i in &turned[1..] {
        let c = &cols.columns[i];
        ty = crate::schema::common(ty, c.ty).ok_or_else(|| {
            Error::Invalid(format!(
                "'{}' is {} and the columns before it are {ty}, which do not widen to one type; retype them first",
                c.name, c.ty
            ))
        })?;
    }
    let kept = Cols {
        columns: (0..cols.columns.len()).filter(|i| !turned.contains(i)).map(|i| cols.columns[i].clone()).collect(),
        ancestry: (0..cols.columns.len()).filter(|i| !turned.contains(i)).map(|i| cols.ancestry[i].clone()).collect(),
    };
    check_name(name, &kept, "it puts the column names in")?;
    check_name(value, &kept, "it puts the values in")?;
    if name.eq_ignore_ascii_case(value) {
        return Err(Error::Invalid(format!("the names and the values are both '{name}'")));
    }
    let names: Vec<String> = turned.iter().map(|&i| literal(&cols.columns[i].name)).collect();
    let values: Vec<String> = turned.iter().map(|&i| format!("CAST({} AS {})", ident(&cols.columns[i].name), ty.sql())).collect();
    let mut from: Vec<Origin> = turned.iter().flat_map(|&i| cols.ancestry[i].clone()).collect();
    from.sort();
    from.dedup();
    let inner = format!(
        "SELECT {}{}unnest([{}]) AS {}, unnest([{}]) AS {} FROM {prev}",
        kept.list_or_empty(),
        if kept.columns.is_empty() { "" } else { ", " },
        names.join(", "),
        ident(name),
        values.join(", "),
        ident(value)
    );
    let select = if keep_empty { inner } else { format!("SELECT * FROM ({inner}) WHERE {} IS NOT NULL", ident(value)) };
    *cols = kept;
    cols.columns.push(Column { name: name.to_string(), ty: ColumnType::Text, role: Role::Dimension });
    cols.ancestry.push(from.clone());
    cols.columns.push(Column { name: value.to_string(), ty, role: Role::inferred(value, ty) });
    cols.ancestry.push(from);
    Ok(select)
}

fn order_by(by: &[SortKey], cols: &Cols) -> Result<String> {
    let mut out = Vec::new();
    for k in by {
        let i = cols.find(&k.column).ok_or_else(|| Error::Internal(format!("'{}' was resolved and is not there", k.column)))?;
        out.push(format!("{} {} NULLS LAST", ident(&cols.columns[i].name), if k.desc { "DESC" } else { "ASC" }));
    }
    Ok(out.join(", "))
}

/// A remap as a CASE: each value given, made the one it maps to; every
/// other value as it was.
pub(crate) fn remap_sql(c: &Column, values: &[Mapping]) -> Result<String> {
    if values.is_empty() {
        return Err(Error::Invalid("it remaps no value".into()));
    }
    let x = ident(&c.name);
    let mut arms = Vec::new();
    for m in values {
        if m.from.is_null() {
            arms.push(format!("WHEN {x} IS NULL THEN {}", value_sql(&m.to, c.ty, &c.name)?));
        } else {
            arms.push(format!("WHEN {x} = {} THEN {}", value_sql(&m.from, c.ty, &c.name)?, value_sql(&m.to, c.ty, &c.name)?));
        }
    }
    Ok(format!("CASE {} ELSE {x} END", arms.join(" ")))
}

fn group(by: &[String], aggregates: &[crate::steps::Aggregate], prev: &str, cols: &mut Cols) -> Result<String> {
    if by.is_empty() && aggregates.is_empty() {
        return Err(Error::Invalid("it groups by nothing and aggregates nothing".into()));
    }
    let mut next = Cols { columns: Vec::new(), ancestry: Vec::new() };
    let mut select = Vec::new();
    for b in by {
        let i = cols.find(b).ok_or_else(|| Error::Internal(format!("'{b}' was resolved and is not there")))?;
        if next.find(&cols.columns[i].name).is_some() {
            return Err(Error::Invalid(format!("it groups by '{b}' twice")));
        }
        select.push(ident(&cols.columns[i].name));
        next.columns.push(Column { role: Role::Dimension, ..cols.columns[i].clone() });
        next.ancestry.push(cols.ancestry[i].clone());
    }
    for a in aggregates {
        check_name(&a.name, &next, "it makes a column")?;
        let (expr, ty, from) = match (&a.column, a.func) {
            (None, AggregateFn::Count) => ("count(*)".to_string(), ColumnType::Integer, Vec::new()),
            (None, f) => return Err(Error::Invalid(format!("{f:?} needs a column; only a count can count rows").to_lowercase())),
            (Some(c), f) => {
                let i = cols.find(c).ok_or_else(|| Error::Internal(format!("'{c}' was resolved and is not there")))?;
                let col = &cols.columns[i];
                let x = ident(&col.name);
                let numeric = |what: &str| -> Result<()> {
                    if col.ty.is_number() {
                        Ok(())
                    } else {
                        Err(Error::Invalid(format!("{what} takes a number, and '{}' is {}", col.name, col.ty)))
                    }
                };
                let (expr, ty) = match f {
                    AggregateFn::Sum => {
                        numeric("sum")?;
                        (format!("sum({x})"), col.ty)
                    }
                    AggregateFn::Avg => {
                        numeric("avg")?;
                        (format!("avg({x})"), ColumnType::Double)
                    }
                    AggregateFn::Min => (format!("min({x})"), col.ty),
                    AggregateFn::Max => (format!("max({x})"), col.ty),
                    AggregateFn::Count => (format!("count({x})"), ColumnType::Integer),
                    AggregateFn::CountDistinct => (format!("count(DISTINCT {x})"), ColumnType::Integer),
                };
                (expr, ty, cols.ancestry[i].clone())
            }
        };
        select.push(format!("CAST({expr} AS {}) AS {}", ty.sql(), ident(&a.name)));
        next.columns.push(Column { name: a.name.clone(), ty, role: Role::Measure });
        next.ancestry.push(from);
    }
    let keys: Vec<String> = by.iter().filter_map(|b| cols.find(b)).map(|i| ident(&cols.columns[i].name)).collect();
    let sql = if keys.is_empty() {
        format!("SELECT {} FROM {prev}", select.join(", "))
    } else {
        let order: Vec<String> = keys.iter().map(|k| format!("{k} NULLS LAST")).collect();
        format!("SELECT {} FROM {prev} GROUP BY {} ORDER BY {}", select.join(", "), keys.join(", "), order.join(", "))
    };
    *cols = next;
    Ok(sql)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::engine;
    use crate::steps::{Aggregate, Mapping};
    use serde_json::json;

    fn source() -> Rel {
        let c = |n: &str, ty: ColumnType, role: Role| Column { name: n.into(), ty, role };
        let columns = vec![
            c("region", ColumnType::Text, Role::Dimension),
            c("store", ColumnType::Text, Role::Dimension),
            c("amount", ColumnType::Decimal(2), Role::Measure),
            c("day", ColumnType::Text, Role::Dimension),
        ];
        let ancestry = columns.iter().map(|c| vec![Origin { source: "s1".into(), column: c.name.clone() }]).collect();
        Rel { name: "s1".into(), columns, ancestry }
    }

    fn compile_steps(steps: &[Step]) -> Result<Chain> {
        let no_engine: Describe<'_> = &|_, _| Ok(Err("no engine here".into()));
        chain("n", &source(), steps, "2026-11-26T00:00:00.000Z", no_engine)
    }

    fn steps(v: serde_json::Value) -> Vec<Step> {
        serde_json::from_value(v).unwrap()
    }

    /// The rows a recipe makes of a small table, as text.
    fn run(v: serde_json::Value) -> Result<(Vec<Column>, Vec<Vec<String>>)> {
        let s = engine().session().unwrap();
        s.execute(
            "CREATE TABLE s1 AS SELECT * FROM (VALUES \
             ('NY', ' a ', 10.00::DECIMAL(38,2), '2026-01-31'), \
             ('CA', 'b', 5.50::DECIMAL(38,2), '2026-02-01'), \
             ('NY', 'c', NULL, '2026-02-01'), \
             ('NY', ' a ', 10.00::DECIMAL(38,2), '2026-01-31')) AS t(region, store, amount, day)",
        )
        .unwrap();
        let c = compile_steps(&steps(v))?;
        assert!(c.unresolved.is_empty(), "{:?}", c.unresolved);
        for stage in &c.stages {
            let StageSql::Select(sql) = &stage.sql else { panic!("no sql step here") };
            s.execute(&format!("CREATE VIEW {} AS {sql}", ident(&stage.name)))?;
        }
        for check in &c.checks {
            let n = s.count(&format!("SELECT count(*) FROM {} WHERE {}", check.relation, check.failing))?;
            if n > 0 {
                return Err(Error::Unprocessable(format!("{n} values of {} do not convert", check.column)));
            }
        }
        let list: Vec<String> = c.out.columns.iter().map(|c| format!("COALESCE(CAST({} AS VARCHAR), 'NULL')", ident(&c.name))).collect();
        let sql = format!("SELECT {} FROM {}", list.join(" || '|' || "), c.out.name);
        let rows = s.texts(&sql)?.into_iter().map(|r| r.split('|').map(str::to_string).collect()).collect();
        Ok((c.out.columns, rows))
    }

    #[test]
    fn steps_shape_rows_and_columns() {
        let (cols, rows) = run(json!([
            { "op": "remap_val", "column": "region", "values": [{ "from": "NY", "to": "New York" }] },
            { "op": "trim_val", "columns": ["store"] },
            { "op": "upper_case", "columns": ["store"] },
            { "op": "fill_empty", "column": "amount", "value": 0 },
            { "op": "retype_column", "column": "day", "type": "date" },
            { "op": "add_column", "column": "double", "formula": "amount * 2" },
            { "op": "dedupe" },
            { "op": "filter", "formula": "amount >= 0" },
            { "op": "rename_column", "column": "store", "to": "shop" },
            { "op": "remove_columns", "columns": ["day"] },
            { "op": "reorder_columns", "columns": ["double"] },
            { "op": "sort", "by": [{ "column": "double", "desc": true }] },
        ]))
        .unwrap();
        let names: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names, ["double", "region", "shop", "amount"]);
        assert_eq!(cols[0].ty, ColumnType::Decimal(2));
        assert_eq!(cols[0].role, Role::Measure);
        assert_eq!(rows, [["20.00", "New York", "A", "10.00"], ["11.00", "CA", "B", "5.50"], ["0.00", "New York", "C", "0.00"]]);
    }

    #[test]
    fn a_group_makes_a_row_per_set() {
        let (cols, rows) = run(json!([
            { "op": "group", "by": ["region"], "aggregates": [
                { "column": "amount", "fn": "sum", "as": "total" },
                { "fn": "count", "as": "rows" },
                { "column": "store", "fn": "count_distinct", "as": "stores" },
            ]},
        ]))
        .unwrap();
        assert_eq!(
            cols.iter().map(|c| (c.name.as_str(), c.role)).collect::<Vec<_>>(),
            [("region", Role::Dimension), ("total", Role::Measure), ("rows", Role::Measure), ("stores", Role::Measure)]
        );
        assert_eq!(rows, [["CA", "5.50", "1", "1"], ["NY", "20.00", "3", "2"]]);
    }

    #[test]
    fn a_retype_keeps_only_what_converts() {
        let e = run(json!([{ "op": "retype_column", "column": "region", "type": "integer" }])).err().unwrap();
        assert!(e.to_string().contains("do not convert"), "{e}");
        let e = run(json!([{ "op": "retype_column", "column": "amount", "type": "integer" }])).err().unwrap();
        assert!(e.to_string().contains("do not convert"), "5.50 would lose its fraction: {e}");
        assert!(run(json!([{ "op": "retype_column", "column": "amount", "type": "decimal(4)" }])).is_ok());
        let (cols, rows) = run(json!([{ "op": "retype_column", "column": "day", "type": "date", "format": "%Y-%m-%d" }])).unwrap();
        assert_eq!(cols[3].ty, ColumnType::Date);
        assert_eq!(rows[0][3], "2026-01-31");
    }

    #[test]
    fn a_step_on_a_column_that_is_not_there_is_named_with_what_follows_from_it() {
        let c = compile_steps(&steps(json!([
            { "op": "add_column", "column": "x", "formula": "upper(gone)" },
            { "op": "remap_val", "column": "x", "values": [{ "from": "A", "to": "B" }] },
            { "op": "remove_columns", "columns": ["store"] },
            { "op": "trim_val", "columns": ["store"] },
            { "op": "sort", "by": [{ "column": "region" }] },
        ])))
        .unwrap();
        let said: Vec<String> = c.unresolved.iter().map(Unresolved::describe).collect();
        assert_eq!(
            said,
            [
                "step 1 (add_column) reads column 'gone', which is not there at that step",
                "step 2 (remap_val) reads column 'x', which is not there at that step",
                "step 4 (trim_val) reads column 'store', which is not there at that step",
            ]
        );
        assert_eq!(c.stages.len(), 2, "the steps that resolve still compile");
        assert_eq!(c.stages[1].name, "n_t5", "each stage is named by its node and step");
    }

    #[test]
    fn steps_are_refused_plainly() {
        for (v, says) in [
            (json!([{ "op": "rename_column", "column": "store", "to": "Region" }]), "there is a column 'region' already"),
            (json!([{ "op": "trim_val", "columns": ["amount"] }]), "works on text"),
            (json!([{ "op": "fill_empty", "column": "amount", "value": "lots" }]), "not a value of column 'amount'"),
            (json!([{ "op": "filter", "formula": "amount + 1" }]), "true or false"),
            (json!([{ "op": "group", "by": ["region"], "aggregates": [{ "column": "store", "fn": "sum", "as": "s" }] }]), "takes a number"),
            (json!([{ "op": "add_column", "column": "", "formula": "1" }]), "not a column name"),
            (json!([{ "op": "find_and_replace_val", "column": "store", "find": "", "with": "x" }]), "finds nothing"),
        ] {
            let e = compile_steps(&steps(v.clone())).err();
            let e = e.unwrap_or_else(|| panic!("{v} compiled"));
            assert!(e.to_string().contains(says), "{v}: {e}");
            assert!(e.to_string().starts_with("step 1 ("), "{e}");
        }
        assert!(serde_json::from_value::<Vec<Step>>(json!([{ "op": "sql", "sql": "DROP TABLE x" }])).is_err(), "no step runs SQL");
        assert!(serde_json::from_value::<Vec<Step>>(json!([{ "op": "trim_val", "columns": ["a"], "extra": 1 }])).is_err());
    }

    #[test]
    fn ancestry_follows_formulas_and_groups() {
        let c = compile_steps(&steps(json!([
            { "op": "add_column", "column": "label", "formula": "region & store" },
            { "op": "group", "by": ["label"], "aggregates": [{ "column": "amount", "fn": "sum", "as": "total" }] },
        ])))
        .unwrap();
        let from = |i: usize| c.out.ancestry[i].iter().map(|o| o.column.as_str()).collect::<Vec<_>>();
        assert_eq!(from(0), ["region", "store"]);
        assert_eq!(from(1), ["amount"]);
        let _ = Aggregate { column: None, func: AggregateFn::Count, name: "n".into() };
        let _ = Mapping { from: json!(1), to: json!(2) };
    }
}
