//! A result as JSON, with its types kept as far as JSON has them.
//!
//! A number is a number, a boolean a boolean, a null a null. A decimal
//! is a string, because a JSON number is a double to most readers and
//! loses digits a decimal has. A date, a time and a timestamp are
//! strings in ISO form. A list is an array and a struct an object.
//! Whatever is left is the text it prints as. Each column says which of
//! these it is, so a reader never has to guess from a value.

use crate::error::{Error, Result};
use arrow_array::cast::AsArray;
use arrow_array::types::{
    Float32Type, Float64Type, Int16Type, Int32Type, Int64Type, Int8Type, UInt16Type, UInt32Type, UInt64Type, UInt8Type,
};
use arrow_array::{Array, RecordBatch};
use arrow_schema::{DataType, Schema};
use serde_json::{json, Value};
use zsql_datasource::Batches;

/// Each column's name, the kind of JSON value it arrives as, and the
/// Arrow type it was read as.
pub fn columns(schema: &Schema) -> Vec<Value> {
    schema
        .fields()
        .iter()
        .map(|f| json!({ "name": f.name(), "type": kind(f.data_type()), "arrow_type": f.data_type().to_string() }))
        .collect()
}

fn kind(t: &DataType) -> &'static str {
    match t {
        DataType::Int8 | DataType::Int16 | DataType::Int32 | DataType::Int64 => "integer",
        DataType::UInt8 | DataType::UInt16 | DataType::UInt32 | DataType::UInt64 => "integer",
        DataType::Float16 | DataType::Float32 | DataType::Float64 => "float",
        DataType::Boolean => "boolean",
        DataType::Utf8 | DataType::LargeUtf8 | DataType::Utf8View => "string",
        DataType::Decimal128(..) | DataType::Decimal256(..) => "decimal",
        DataType::Date32 | DataType::Date64 => "date",
        DataType::Time32(_) | DataType::Time64(_) => "time",
        DataType::Timestamp(..) => "timestamp",
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..) => "array",
        DataType::Struct(_) => "object",
        DataType::Binary | DataType::LargeBinary | DataType::FixedSizeBinary(_) | DataType::BinaryView => "binary",
        DataType::Null => "null",
        _ => "other",
    }
}

/// The rows of a page, each an array of its values in column order.
pub fn rows(page: &Batches) -> Result<Vec<Value>> {
    let mut out = Vec::with_capacity(page.num_rows());
    for batch in &page.batches {
        let columns = column_values(batch)?;
        for row in 0..batch.num_rows() {
            out.push(Value::Array(columns.iter().map(|c| c[row].clone()).collect()));
        }
    }
    Ok(out)
}

/// Each column of a batch as its values, top to bottom.
fn column_values(batch: &RecordBatch) -> Result<Vec<Vec<Value>>> {
    // The text of whatever is not a JSON value of its own comes from the
    // one rendering every result is printed with, trimmed decimals and
    // all, so a value reads the same here as in a table.
    let text = Batches { schema: batch.schema(), batches: vec![batch.clone()] }.to_text()?;
    batch
        .columns()
        .iter()
        .enumerate()
        .map(|(at, column)| {
            let printed = |row: usize| text.rows[row][at].clone().map(Value::String).unwrap_or(Value::Null);
            values(column.as_ref(), &printed)
        })
        .collect()
}

fn values(column: &dyn Array, printed: &dyn Fn(usize) -> Value) -> Result<Vec<Value>> {
    let n = column.len();
    macro_rules! numbers {
        ($t:ty) => {{
            let a = column.as_primitive::<$t>();
            Ok((0..n).map(|i| if a.is_null(i) { Value::Null } else { json!(a.value(i)) }).collect())
        }};
    }
    // A float JSON cannot say, an infinity or a not-a-number, is the
    // word for it.
    macro_rules! floats {
        ($t:ty) => {{
            let a = column.as_primitive::<$t>();
            Ok((0..n)
                .map(|i| {
                    if a.is_null(i) {
                        Value::Null
                    } else {
                        let v = f64::from(a.value(i));
                        serde_json::Number::from_f64(v).map(Value::Number).unwrap_or_else(|| Value::String(v.to_string()))
                    }
                })
                .collect())
        }};
    }
    match column.data_type() {
        DataType::Int8 => numbers!(Int8Type),
        DataType::Int16 => numbers!(Int16Type),
        DataType::Int32 => numbers!(Int32Type),
        DataType::Int64 => numbers!(Int64Type),
        DataType::UInt8 => numbers!(UInt8Type),
        DataType::UInt16 => numbers!(UInt16Type),
        DataType::UInt32 => numbers!(UInt32Type),
        DataType::UInt64 => numbers!(UInt64Type),
        DataType::Float32 => floats!(Float32Type),
        DataType::Float64 => floats!(Float64Type),
        DataType::Boolean => {
            let a = column.as_boolean();
            Ok((0..n).map(|i| if a.is_null(i) { Value::Null } else { Value::Bool(a.value(i)) }).collect())
        }
        DataType::Null => Ok(vec![Value::Null; n]),
        DataType::List(_) => {
            let a = column.as_list::<i32>();
            (0..n).map(|i| if a.is_null(i) { Ok(Value::Null) } else { nested(a.value(i).as_ref()).map(Value::Array) }).collect()
        }
        DataType::LargeList(_) => {
            let a = column.as_list::<i64>();
            (0..n).map(|i| if a.is_null(i) { Ok(Value::Null) } else { nested(a.value(i).as_ref()).map(Value::Array) }).collect()
        }
        DataType::Struct(fields) => {
            let a = column.as_struct();
            let children: Vec<Vec<Value>> = a.columns().iter().map(|c| nested(c.as_ref())).collect::<Result<_>>()?;
            Ok((0..n)
                .map(|i| {
                    if a.is_null(i) {
                        Value::Null
                    } else {
                        Value::Object(fields.iter().zip(&children).map(|(f, c)| (f.name().clone(), c[i].clone())).collect())
                    }
                })
                .collect())
        }
        _ => Ok((0..n).map(printed).collect()),
    }
}

/// The values of an array that sits inside another: the same rules,
/// with its own text for what has no JSON value of its own.
fn nested(column: &dyn Array) -> Result<Vec<Value>> {
    let options = arrow_cast::display::FormatOptions::default();
    let formatter = arrow_cast::display::ArrayFormatter::try_new(column, &options).map_err(|e| Error::Arrow("a nested value".into(), e))?;
    let printed = |row: usize| if column.is_null(row) { Value::Null } else { Value::String(formatter.value(row).to_string()) };
    values(column, &printed)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::builder::{Int64Builder, ListBuilder};
    use arrow_array::{
        ArrayRef, BooleanArray, Date32Array, Decimal128Array, Float64Array, Int64Array, StringArray, StructArray, TimestampMicrosecondArray,
    };
    use arrow_schema::Field;
    use std::sync::Arc;

    #[test]
    fn every_kind_of_value_arrives_as_json_has_it() {
        let mut lists = ListBuilder::new(Int64Builder::new());
        lists.append_value([Some(1), None, Some(3)]);
        lists.append_null();
        let inner: Vec<(Arc<Field>, ArrayRef)> = vec![
            (Arc::new(Field::new("a", DataType::Int64, true)), Arc::new(Int64Array::from(vec![Some(7), None]))),
            (Arc::new(Field::new("b", DataType::Utf8, true)), Arc::new(StringArray::from(vec![Some("x"), None]))),
            (Arc::new(Field::new("d", DataType::Date32, true)), Arc::new(Date32Array::from(vec![Some(20_455), None]))),
        ];
        let columns: Vec<(&str, ArrayRef)> = vec![
            ("i", Arc::new(Int64Array::from(vec![Some(9_007_199_254_740_993), None]))),
            ("f", Arc::new(Float64Array::from(vec![Some(1.5), Some(f64::NAN)]))),
            ("b", Arc::new(BooleanArray::from(vec![Some(true), None]))),
            ("s", Arc::new(StringArray::from(vec![Some("paid"), None]))),
            ("n", Arc::new(Decimal128Array::from(vec![Some(12_500_000_000), None]).with_precision_and_scale(38, 9).unwrap())),
            ("d", Arc::new(Date32Array::from(vec![Some(20_455), None]))),
            ("ts", Arc::new(TimestampMicrosecondArray::from(vec![Some(1_767_323_045_123_456), None]).with_timezone("UTC"))),
            ("arr", Arc::new(lists.finish())),
            ("st", Arc::new(StructArray::from(inner))),
        ];
        let schema = Arc::new(Schema::new(columns.iter().map(|(n, a)| Field::new(*n, a.data_type().clone(), true)).collect::<Vec<_>>()));
        let batch = RecordBatch::try_new(schema.clone(), columns.into_iter().map(|(_, a)| a).collect()).unwrap();
        // A decimal whose scale is the type's and not the column's, as
        // BigQuery's driver marks it.
        let page = Batches { schema, batches: vec![batch] }.printing_decimals_trimmed();

        let kinds: Vec<String> = super::columns(&page.schema).iter().map(|c| c["type"].as_str().unwrap().to_string()).collect();
        assert_eq!(kinds, ["integer", "float", "boolean", "string", "decimal", "date", "timestamp", "array", "object"]);
        let got = rows(&page).unwrap();
        assert_eq!(
            got[0],
            json!([9_007_199_254_740_993_i64, 1.5, true, "paid", "12.5", "2026-01-02", "2026-01-02T03:04:05.123456Z", [1, null, 3], {"a": 7, "b": "x", "d": "2026-01-02"}])
        );
        assert_eq!(got[1], json!([null, "NaN", null, null, null, null, null, null, {"a": null, "b": null, "d": null}]));
        assert_eq!(serde_json::to_string(&got[0][0]).unwrap(), "9007199254740993", "an integer keeps every digit");
    }
}
