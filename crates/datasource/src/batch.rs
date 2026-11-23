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

    /// Every cell as the text it prints as; a null stays a null.
    pub fn to_text(&self) -> Result<Rows> {
        let fail = |e: arrow_schema::ArrowError| Error::Query("result".into(), format!("rendering a value: {e}"));
        let options = FormatOptions::default();
        let mut rows: Vec<Vec<Option<String>>> = Vec::with_capacity(self.num_rows());
        for batch in &self.batches {
            let formatters: Vec<ArrayFormatter> = batch
                .columns()
                .iter()
                .map(|c| ArrayFormatter::try_new(c.as_ref(), &options))
                .collect::<std::result::Result<_, _>>()
                .map_err(fail)?;
            for i in 0..batch.num_rows() {
                rows.push(
                    batch
                        .columns()
                        .iter()
                        .zip(&formatters)
                        .map(|(c, f)| if c.is_null(i) { None } else { Some(f.value(i).to_string()) })
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
