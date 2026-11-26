//! Taking a snapshot: rows that arrived, parsed, typed, and met with
//! the types their source already has.

use crate::engine::Session;
use crate::error::{Error, Result};
use crate::schema::{ident, meet, Column, ColumnType, Meet, Role};
use crate::source::{parse_csv, CsvOptions, Parse};
use arrow_array::{ArrayRef, RecordBatch, StringArray};
use arrow_schema::{DataType, Field, Schema};
use std::sync::Arc;
use zsql_datasource::Batches;

/// A snapshot's rows, ready to be kept.
pub(crate) struct Taken {
    pub rows: Batches,
    pub columns: Vec<Column>,
    pub parse: Option<Parse>,
    pub changes: Vec<String>,
}

/// How many values that will not convert a refusal shows.
const EXAMPLES: usize = 3;

/// A CSV upload, as a snapshot of a source whose columns are
/// `established` (none for a new source).
pub(crate) fn from_csv(session: &Session, bytes: &[u8], options: &CsvOptions, established: &[Column], max_rows: usize) -> Result<Taken> {
    let parsed = parse_csv(bytes, options, max_rows)?;
    let fields: Vec<Field> = parsed.columns.iter().map(|(n, _)| Field::new(n, DataType::Utf8, true)).collect();
    let arrays: Vec<ArrayRef> = parsed.values.iter().map(|v| Arc::new(StringArray::from(v.clone())) as ArrayRef).collect();
    let batch = RecordBatch::try_new(Arc::new(Schema::new(fields)), arrays)?;
    let raw: Vec<(String, ColumnType)> = parsed.columns.iter().map(|(n, _)| (n.clone(), ColumnType::Text)).collect();
    session.load("raw", &raw, &[batch])?;
    let (exprs, columns, changes) = meet_established(session, "raw", &parsed.columns, established)?;
    let rows = session.query(&format!("SELECT {} FROM raw", exprs.join(", ")))?;
    Ok(Taken { rows, columns, parse: Some(parsed.parse), changes })
}

/// Rows a run returned, as a snapshot. Each column is cast to the type
/// a dataset holds it as, then met with the source's types as an
/// upload's are; `roles` are the model's, for columns new to the source.
pub(crate) fn from_rows(
    session: &Session,
    rows: &Batches,
    roles: &std::collections::BTreeMap<String, Role>,
    established: &[Column],
    max_rows: usize,
) -> Result<Taken> {
    if rows.num_rows() > max_rows {
        return Err(Error::Unprocessable(format!("the query returned {} rows, more than the {max_rows} a dataset holds", rows.num_rows())));
    }
    let (incoming, batches) = held(rows)?;
    session.load("raw", &incoming, &batches)?;
    let (exprs, mut columns, changes) = meet_established(session, "raw", &incoming, established)?;
    for c in columns.iter_mut() {
        if !established.iter().any(|e| e.name == c.name) {
            if let Some(r) = roles.get(&c.name) {
                c.role = *r;
            }
        }
    }
    let list = if exprs.is_empty() { "NULL AS \"_\"".to_string() } else { exprs.join(", ") };
    let rows = session.query(&format!("SELECT {list} FROM raw"))?;
    Ok(Taken { rows, columns, parse: None, changes })
}

/// Rows from elsewhere, each column cast to the type a dataset holds it
/// as; the columns and the batches.
pub(crate) fn held(rows: &Batches) -> Result<(Vec<(String, ColumnType)>, Vec<RecordBatch>)> {
    let mut incoming: Vec<(String, ColumnType)> = Vec::new();
    for f in rows.schema.fields() {
        if incoming.iter().any(|(n, _)| n.eq_ignore_ascii_case(f.name())) {
            return Err(Error::Unprocessable(format!("two columns are named '{}'", f.name())));
        }
        incoming.push((f.name().clone(), held_as(f.data_type())));
    }
    let fields: Vec<Field> = incoming.iter().map(|(n, t)| Field::new(n, t.arrow(), true)).collect();
    let schema = Arc::new(Schema::new(fields));
    let mut batches = Vec::new();
    for b in &rows.batches {
        let mut arrays: Vec<ArrayRef> = Vec::new();
        for (i, (name, t)) in incoming.iter().enumerate() {
            let cast = arrow_cast::cast(b.column(i), &t.arrow()).map_err(|e| {
                Error::Unprocessable(format!("column '{name}' of type {} cannot be held as {t}: {e}", b.column(i).data_type()))
            })?;
            arrays.push(cast);
        }
        batches.push(RecordBatch::try_new(schema.clone(), arrays)?);
    }
    Ok((incoming, batches))
}

/// The type a dataset holds a warehouse's column as: the nearest of its
/// own, and text for anything it has no type for.
fn held_as(t: &DataType) -> ColumnType {
    match t {
        DataType::Boolean => ColumnType::Boolean,
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => ColumnType::Integer,
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 => ColumnType::Integer,
        DataType::UInt64 => ColumnType::Decimal(0),
        DataType::Float16 | DataType::Float32 | DataType::Float64 => ColumnType::Double,
        DataType::Decimal128(_, s) | DataType::Decimal256(_, s) if *s >= 0 && *s as u8 <= crate::schema::DECIMAL_SCALE_MAX => {
            ColumnType::Decimal(*s as u8)
        }
        DataType::Decimal128(..) | DataType::Decimal256(..) => ColumnType::Double,
        DataType::Date32 | DataType::Date64 => ColumnType::Date,
        DataType::Timestamp(..) => ColumnType::Timestamp,
        _ => ColumnType::Text,
    }
}

/// The select list that gives a table of text columns, whose values
/// were read as `incoming`, the types the source established: each
/// established column that is still there, at its type or widened, then
/// each new one. A column whose values neither fit nor widen is kept at
/// its type only if every value converts, and otherwise the snapshot is
/// refused, naming the column, both types and the first values that do
/// not convert.
fn meet_established(
    session: &Session,
    table: &str,
    incoming: &[(String, ColumnType)],
    established: &[Column],
) -> Result<(Vec<String>, Vec<Column>, Vec<String>)> {
    let mut exprs = Vec::new();
    let mut columns = Vec::new();
    let mut changes = Vec::new();
    for e in established {
        let Some((_, t)) = incoming.iter().find(|(n, _)| *n == e.name) else {
            changes.push(format!("column '{}' is not in this snapshot", e.name));
            continue;
        };
        let x = ident(&e.name);
        let ty = match meet(e.ty, *t) {
            Meet::Same | Meet::Fits => e.ty,
            Meet::Widens(w) => {
                changes.push(format!("column '{}' widened from {} to {w}", e.name, e.ty));
                w
            }
            Meet::Converts => {
                let failing = format!("{x} IS NOT NULL AND TRY_CAST({x} AS {}) IS NULL", e.ty.sql());
                let n = session.count(&format!("SELECT count(*) FROM {} WHERE {failing}", ident(table)))?;
                if n > 0 {
                    let examples = session.texts(&format!("SELECT {x} FROM {} WHERE {failing} LIMIT {EXAMPLES}", ident(table)))?;
                    let shown: Vec<String> = examples.iter().map(|v| format!("'{v}'")).collect();
                    return Err(Error::Unprocessable(format!(
                        "column '{}' is {} and arrived as {t}: {n} value(s) do not convert, such as {}; nothing was changed",
                        e.name,
                        e.ty,
                        shown.join(", ")
                    )));
                }
                if e.ty != ColumnType::Text {
                    changes.push(format!("column '{}' arrived as {t}; every value converts, so it stays {}", e.name, e.ty));
                }
                e.ty
            }
        };
        exprs.push(format!("CAST({x} AS {}) AS {x}", ty.sql()));
        columns.push(Column { name: e.name.clone(), ty, role: e.role });
    }
    for (name, t) in incoming {
        if established.iter().any(|e| e.name == *name) {
            continue;
        }
        if !established.is_empty() {
            changes.push(format!("column '{name}' is new, as {t}"));
        }
        let x = ident(name);
        exprs.push(format!("CAST({x} AS {}) AS {x}", t.sql()));
        columns.push(Column { name: name.clone(), ty: *t, role: Role::inferred(name, *t) });
    }
    Ok((exprs, columns, changes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::engine::tests::engine;
    use ColumnType::*;

    fn take(text: &str, established: &[Column]) -> Result<Taken> {
        from_csv(&engine().session().unwrap(), text.as_bytes(), &CsvOptions::default(), established, 100)
    }

    #[test]
    fn a_first_snapshot_takes_the_inferred_types() {
        let t = take(
            "region,month,amount,at,ok\nEMEA,2026-01-01,10.5,2026-01-01 10:00,true\nAPAC,2026-02-01,7,2026-01-02T11:30:05,false\n",
            &[],
        )
        .unwrap();
        let types: Vec<ColumnType> = t.columns.iter().map(|c| c.ty).collect();
        assert_eq!(types, [Text, Date, Decimal(1), Timestamp, Boolean]);
        assert_eq!(t.rows.num_rows(), 2);
        assert_eq!(t.columns[2].role, Role::Measure);
        assert!(t.changes.is_empty());
    }

    #[test]
    fn a_later_snapshot_widens_converts_or_is_refused() {
        let first = take("id,amount,day\n1,10,2026-01-01\n", &[]).unwrap().columns;
        let t = take("id,amount,day,extra\n2,10.25,2026-01-02,x\n", &first).unwrap();
        assert_eq!(t.columns[1].ty, Decimal(2));
        assert!(t.changes.iter().any(|c| c.contains("widened from integer to decimal(2)")), "{:?}", t.changes);
        assert!(t.changes.iter().any(|c| c.contains("'extra' is new")));
        let t = take("amount,day\n5,2026-01-02\n", &first).unwrap();
        assert!(t.changes.iter().any(|c| c.contains("'id' is not in this snapshot")));
        assert_eq!(t.columns.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["amount", "day"]);
        let e = take("id,amount,day\n1,n/a,2026-01-01\n2,3,soon\n", &first).err().unwrap();
        assert!(e.to_string().contains("column 'amount' is integer and arrived as text") && e.to_string().contains("'n/a'"), "{e}");
    }

    #[test]
    fn text_that_converts_keeps_the_established_type() {
        let first = take("day\n2026-01-01\n", &[]).unwrap().columns;
        // A file whose one value is empty-looking text still converts.
        let t = take("day\n2026-01-05\n", &first).unwrap();
        assert_eq!(t.columns[0].ty, Date);
        let text_first = take("code\nabc\n", &[]).unwrap().columns;
        let t = take("code\n12\n", &text_first).unwrap();
        assert_eq!(t.columns[0].ty, Text, "a number arriving in a text column stays text");
    }
}
