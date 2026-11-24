//! What a statement returned, with its types: Arrow record batches.
//!
//! [`Rows`] is text, which is all a catalog read or a sampled value
//! needs. A result somebody keeps is different: a number that arrives as
//! a string has to be guessed back into a number by whoever reads it, so
//! the transports that are told a column's type hand it over here as
//! that type. A transport that only has text gives every column as
//! `Utf8`, which is the truth about what it knows.
//!
//! Text is still how a result is printed, and there is one rendering:
//! Arrow's own formatter with its defaults, which is what the DuckDB
//! transport has always used. A typed column and its text agree by
//! construction, since the second is made from the first.

use crate::{Error, Result, Rows};
use arrow_array::{Array, ArrayRef, RecordBatch, RecordBatchOptions, StringArray};
use arrow_cast::display::{ArrayFormatter, FormatOptions};
use arrow_schema::{DataType, Field, Schema, SchemaRef};
use std::sync::Arc;

/// A whole result: one schema and the batches under it, in order.
#[derive(Debug, Clone)]
pub struct Batches {
    pub schema: SchemaRef,
    pub batches: Vec<RecordBatch>,
}

impl Batches {
    pub fn empty() -> Batches {
        Batches { schema: Arc::new(Schema::empty()), batches: Vec::new() }
    }

    pub fn num_rows(&self) -> usize {
        self.batches.iter().map(RecordBatch::num_rows).sum()
    }

    pub fn columns(&self) -> Vec<String> {
        self.schema.fields().iter().map(|f| f.name().clone()).collect()
    }

    /// Text rows as one batch of nullable `Utf8` columns.
    pub fn from_text(rows: &Rows) -> Batches {
        let schema: SchemaRef = Arc::new(Schema::new(rows.columns.iter().map(|c| Field::new(c, DataType::Utf8, true)).collect::<Vec<_>>()));
        if rows.rows.is_empty() {
            return Batches { schema, batches: Vec::new() };
        }
        let arrays: Vec<ArrayRef> = (0..rows.columns.len())
            .map(|i| Arc::new(rows.rows.iter().map(|r| r.get(i).and_then(|c| c.as_deref())).collect::<StringArray>()) as ArrayRef)
            .collect();
        // A statement with no columns still has a row count to keep.
        let options = RecordBatchOptions::new().with_row_count(Some(rows.rows.len()));
        let batch =
            RecordBatch::try_new_with_options(schema.clone(), arrays, &options).expect("every column is Utf8 and as long as the rows");
        Batches { schema, batches: vec![batch] }
    }

    /// Marks every decimal column to print without trailing zeros, for
    /// a warehouse whose scale is a fixed property of the type and not
    /// of the column. The mark is field metadata, so it is stored with
    /// the result and a stored result prints the same. Values and types
    /// are untouched.
    pub fn printing_decimals_trimmed(self) -> Batches {
        let decimal = |t: &DataType| matches!(t, DataType::Decimal128(..) | DataType::Decimal256(..));
        if !self.schema.fields().iter().any(|f| decimal(f.data_type())) {
            return self;
        }
        let fields: Vec<Field> = self
            .schema
            .fields()
            .iter()
            .map(|f| {
                let mut field = f.as_ref().clone();
                if decimal(f.data_type()) {
                    let mut metadata = f.metadata().clone();
                    metadata.insert(TRIM_ZEROS.to_string(), "true".to_string());
                    field.set_metadata(metadata);
                }
                field
            })
            .collect();
        let schema: SchemaRef = Arc::new(Schema::new_with_metadata(fields, self.schema.metadata().clone()));
        let batches =
            self.batches.into_iter().map(|b| b.with_schema(schema.clone()).expect("the same columns, with metadata added")).collect();
        Batches { schema, batches }
    }

    /// Every cell as the text it prints as; a null stays a null.
    pub fn to_text(&self) -> Result<Rows> {
        let fail = |e: arrow_schema::ArrowError| Error::Query("result".into(), format!("rendering a value: {e}"));
        let options = FormatOptions::default();
        let mut rows: Vec<Vec<Option<String>>> = Vec::with_capacity(self.num_rows());
        for batch in &self.batches {
            let columns: Vec<ArrayRef> = batch.columns().iter().map(printable).collect::<std::result::Result<_, _>>().map_err(fail)?;
            let formatters: Vec<ArrayFormatter> = columns
                .iter()
                .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
                .collect::<std::result::Result<_, _>>()
                .map_err(fail)?;
            let trimmed: Vec<bool> = self.schema.fields().iter().map(|f| f.metadata().contains_key(TRIM_ZEROS)).collect();
            for i in 0..batch.num_rows() {
                rows.push(
                    columns
                        .iter()
                        .zip(&formatters)
                        .zip(&trimmed)
                        .map(|((c, f), trim)| {
                            if c.is_null(i) {
                                return None;
                            }
                            let text = f.value(i).to_string();
                            Some(if *trim { trim_zeros(text) } else { text })
                        })
                        .collect(),
                );
            }
        }
        Ok(Rows { columns: self.columns(), rows })
    }

    /// The first `n` rows, and whether there were more.
    pub fn truncate(&mut self, n: usize) -> bool {
        if self.num_rows() <= n {
            return false;
        }
        let mut left = n;
        let mut kept = Vec::new();
        for batch in self.batches.drain(..) {
            if left == 0 {
                break;
            }
            let take = left.min(batch.num_rows());
            kept.push(if take == batch.num_rows() { batch } else { batch.slice(0, take) });
            left -= take;
        }
        self.batches = kept;
        true
    }

    /// Rows `offset..`, for a reader paging through a stored result.
    pub fn skip(&mut self, offset: usize) {
        let mut left = offset;
        let mut kept = Vec::new();
        for batch in self.batches.drain(..) {
            if left >= batch.num_rows() {
                left -= batch.num_rows();
                continue;
            }
            kept.push(if left == 0 { batch } else { batch.slice(left, batch.num_rows() - left) });
            left = 0;
        }
        self.batches = kept;
    }
}

/// The field metadata key that says a decimal prints without its
/// trailing zeros.
const TRIM_ZEROS: &str = "zsql:trim_zeros";

/// `12.500000000` as `12.5`, `3.000` as `3`; anything without a
/// fraction as it is.
fn trim_zeros(text: String) -> String {
    if !text.contains('.') || text.contains(['e', 'E']) {
        return text;
    }
    text.trim_end_matches('0').trim_end_matches('.').to_string()
}

/// A column as the formatter can print it. A timestamp in a named time
/// zone (`UTC`, which is how BigQuery's driver labels every TIMESTAMP)
/// cannot be printed without a time zone database, which is not built
/// in; it is the same instant at offset `+00:00`, which can, so that is
/// how it is printed. Wherever it sits: a struct's field, a list's item.
fn printable(column: &ArrayRef) -> std::result::Result<ArrayRef, arrow_schema::ArrowError> {
    let wanted = printable_type(column.data_type());
    if &wanted == column.data_type() {
        return Ok(column.clone());
    }
    arrow_cast::cast(column.as_ref(), &wanted)
}

fn printable_type(t: &DataType) -> DataType {
    let field = |f: &Arc<Field>| Arc::new(f.as_ref().clone().with_data_type(printable_type(f.data_type())));
    match t {
        DataType::Timestamp(unit, Some(zone)) if zone.parse::<arrow_array::timezone::Tz>().is_err() => {
            DataType::Timestamp(*unit, Some("+00:00".into()))
        }
        DataType::List(f) => DataType::List(field(f)),
        DataType::LargeList(f) => DataType::LargeList(field(f)),
        DataType::FixedSizeList(f, n) => DataType::FixedSizeList(field(f), *n),
        DataType::Struct(fields) => DataType::Struct(fields.iter().map(field).collect()),
        other => other.clone(),
    }
}

/// Collects a stream of batches under a row cap, refusing past it as
/// every transport does: a caller never averages a partial answer.
pub fn collect<E: std::fmt::Display>(
    uid: &str,
    schema: SchemaRef,
    stream: impl Iterator<Item = std::result::Result<RecordBatch, E>>,
    max_rows: usize,
) -> Result<Batches> {
    let mut out = Batches { schema, batches: Vec::new() };
    let mut rows = 0usize;
    for batch in stream {
        let batch = batch.map_err(|e| Error::Query(uid.to_string(), e.to_string()))?;
        rows += batch.num_rows();
        if rows > max_rows {
            return Err(Error::TooManyRows(uid.to_string(), max_rows));
        }
        if batch.num_rows() > 0 {
            out.batches.push(batch);
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::Int64Array;

    fn text() -> Rows {
        Rows {
            columns: vec!["status".into(), "n".into()],
            rows: vec![vec![Some("paid".into()), Some("3".into())], vec![None, Some("1".into())], vec![Some("open".into()), None]],
        }
    }

    #[test]
    fn text_goes_to_batches_and_back_unchanged() {
        let rows = text();
        let b = Batches::from_text(&rows);
        assert_eq!(b.num_rows(), 3);
        assert_eq!(b.columns(), rows.columns);
        assert!(b.schema.fields().iter().all(|f| f.data_type() == &DataType::Utf8 && f.is_nullable()));
        let back = b.to_text().unwrap();
        assert_eq!(back.columns, rows.columns);
        assert_eq!(back.rows, rows.rows);
    }

    #[test]
    fn no_rows_keeps_the_columns() {
        let b = Batches::from_text(&Rows { columns: vec!["a".into()], rows: vec![] });
        assert_eq!(b.num_rows(), 0);
        assert_eq!(b.to_text().unwrap().columns, vec!["a".to_string()]);
    }

    #[test]
    fn a_typed_column_prints_as_its_value() {
        let schema: SchemaRef = Arc::new(Schema::new(vec![Field::new("n", DataType::Int64, true)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![Arc::new(Int64Array::from(vec![Some(7), None]))]).unwrap();
        let rows = Batches { schema, batches: vec![batch] }.to_text().unwrap();
        assert_eq!(rows.rows, vec![vec![Some("7".to_string())], vec![None]]);
    }

    /// What BigQuery's driver hands over for a TIMESTAMP, alone and
    /// inside a struct: printed as the instant it is, not refused.
    #[test]
    fn a_timestamp_in_a_named_zone_prints_at_its_utc_offset() {
        use arrow_array::{StructArray, TimestampMicrosecondArray};
        let ts = || TimestampMicrosecondArray::from(vec![Some(1_767_323_045_123_456), None]).with_timezone("UTC");
        let plain: ArrayRef = Arc::new(ts());
        let inner = Arc::new(Field::new("at", plain.data_type().clone(), true));
        let nested: ArrayRef = Arc::new(StructArray::from(vec![(inner, Arc::new(ts()) as ArrayRef)]));
        let offset: ArrayRef = Arc::new(TimestampMicrosecondArray::from(vec![Some(0), Some(0)]).with_timezone("+05:30"));
        let schema: SchemaRef = Arc::new(Schema::new(vec![
            Field::new("ts", plain.data_type().clone(), true),
            Field::new("st", nested.data_type().clone(), true),
            Field::new("off", offset.data_type().clone(), true),
        ]));
        let batch = RecordBatch::try_new(schema.clone(), vec![plain, nested, offset]).unwrap();
        let b = Batches { schema, batches: vec![batch] };
        let rows = b.to_text().unwrap().rows;
        assert_eq!(rows[0][0].as_deref(), Some("2026-01-02T03:04:05.123456Z"));
        assert_eq!(rows[1][0], None);
        assert_eq!(rows[0][1].as_deref(), Some("{at: 2026-01-02T03:04:05.123456Z}"));
        assert_eq!(rows[0][2].as_deref(), Some("1970-01-01T05:30:00+05:30"), "an offset zone is printed as it was");
        // What is kept is what arrived: only the printing changes.
        assert_eq!(b.schema.field(0).data_type(), &DataType::Timestamp(arrow_schema::TimeUnit::Microsecond, Some("UTC".into())));
    }

    /// A NUMERIC from BigQuery's driver: nine places whatever the value.
    #[test]
    fn a_marked_decimal_prints_without_its_trailing_zeros() {
        use arrow_array::Decimal128Array;
        let values = vec![Some(12_500_000_000), Some(3_000_000_000), Some(0), Some(-1_050_000_000), None, Some(100_000_000_000)];
        let decimals: ArrayRef = Arc::new(Decimal128Array::from(values).with_precision_and_scale(38, 9).unwrap());
        let strings: ArrayRef = Arc::new(StringArray::from(vec![Some("1.500"); 6]));
        let schema: SchemaRef =
            Arc::new(Schema::new(vec![Field::new("n", DataType::Decimal128(38, 9), true), Field::new("s", DataType::Utf8, true)]));
        let batch = RecordBatch::try_new(schema.clone(), vec![decimals, strings]).unwrap();
        let as_sent = Batches { schema, batches: vec![batch] };
        let column = |b: &Batches, i: usize| b.to_text().unwrap().rows.into_iter().map(|r| r[i].clone()).collect::<Vec<_>>();
        assert_eq!(column(&as_sent, 0)[0].as_deref(), Some("12.500000000"), "unmarked, a decimal prints at its scale");

        let marked = as_sent.clone().printing_decimals_trimmed();
        let got = column(&marked, 0);
        let got: Vec<Option<&str>> = got.iter().map(|c| c.as_deref()).collect();
        assert_eq!(got, [Some("12.5"), Some("3"), Some("0"), Some("-1.05"), None, Some("100")]);
        assert_eq!(column(&marked, 1)[0].as_deref(), Some("1.500"), "text that looks like a decimal is left alone");
        // Only the printing: the type and the values are what arrived.
        assert_eq!(marked.schema.field(0).data_type(), &DataType::Decimal128(38, 9));
        assert_eq!(marked.batches[0].column(0).as_ref(), as_sent.batches[0].column(0).as_ref());
    }

    #[test]
    fn truncating_and_skipping_cross_batch_boundaries() {
        let one = Batches::from_text(&text());
        let mut b = Batches { schema: one.schema.clone(), batches: vec![one.batches[0].clone(), one.batches[0].clone()] };
        assert!(!b.clone().truncate(6), "nothing was cut");
        assert!(b.truncate(4));
        assert_eq!(b.num_rows(), 4);
        b.skip(2);
        let rows = b.to_text().unwrap().rows;
        assert_eq!(rows, vec![vec![Some("open".to_string()), None], vec![Some("paid".to_string()), Some("3".to_string())]]);
    }

    #[test]
    fn collecting_refuses_past_the_cap() {
        let one = Batches::from_text(&text());
        let stream = || vec![Ok::<_, String>(one.batches[0].clone()), Ok(one.batches[0].clone())].into_iter();
        assert_eq!(collect("wh", one.schema.clone(), stream(), 6).unwrap().num_rows(), 6);
        assert!(matches!(collect("wh", one.schema.clone(), stream(), 5).unwrap_err(), Error::TooManyRows(_, 5)));
    }
}
