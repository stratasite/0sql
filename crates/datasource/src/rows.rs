use crate::{Batches, Error, Result, Sink};
use std::sync::atomic::{AtomicUsize, Ordering};

/// What a statement returned. Every cell is text or null: a catalog is
/// strings and a sampled value is the string a consumer will match
/// against, so nothing here needs a type system yet.
#[derive(Debug, Clone, Default, serde::Serialize)]
pub struct Rows {
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Option<String>>>,
}

impl Rows {
    /// The cell in `row` under the column of this name, if the statement
    /// returned one and it is not null.
    pub fn get<'a>(&'a self, row: &'a [Option<String>], column: &str) -> Option<&'a str> {
        let at = self.columns.iter().position(|c| c == column)?;
        row.get(at)?.as_deref()
    }
}

/// Where an adapter that reads text puts what it reads, a row at a time.
/// One fetch loop per adapter serves both a capped read, which keeps the
/// rows, and a streamed one, which hands them on in batches.
pub(crate) trait RowSink {
    /// The column names, before the first row. A transport that learns
    /// them again with every page may say them again; the first counts.
    fn columns(&mut self, columns: &[String]) -> Result<()>;

    /// How many rows the warehouse says there are, when it says before
    /// sending them: a capped read refuses here, before fetching any.
    // Used by the REST adapters as they are converted.
    #[allow(dead_code)]
    fn total(&mut self, _rows: u64) -> Result<()> {
        Ok(())
    }

    fn row(&mut self, row: Vec<Option<String>>) -> Result<()>;
}

/// A read that keeps its rows, and refuses past a cap rather than
/// truncating: a caller never averages a partial answer.
pub(crate) struct Capped {
    uid: String,
    max_rows: usize,
    named: bool,
    rows: Rows,
}

impl Capped {
    pub(crate) fn new(uid: &str, max_rows: usize) -> Capped {
        Capped { uid: uid.to_string(), max_rows, named: false, rows: Rows::default() }
    }

    pub(crate) fn finish(self) -> Rows {
        self.rows
    }
}

impl RowSink for Capped {
    fn columns(&mut self, columns: &[String]) -> Result<()> {
        if !self.named {
            self.rows.columns = columns.to_vec();
            self.named = true;
        }
        Ok(())
    }

    fn total(&mut self, rows: u64) -> Result<()> {
        if rows > self.max_rows as u64 {
            return Err(Error::TooManyRows(self.uid.clone(), self.max_rows));
        }
        Ok(())
    }

    fn row(&mut self, row: Vec<Option<String>>) -> Result<()> {
        if self.rows.rows.len() == self.max_rows {
            return Err(Error::TooManyRows(self.uid.clone(), self.max_rows));
        }
        self.rows.rows.push(row);
        Ok(())
    }
}

/// How many rows of text make one batch on their way to a [`Sink`].
static BATCH_ROWS: AtomicUsize = AtomicUsize::new(8_192);

/// For a test that wants to see more than one batch from a small table.
#[cfg(test)]
pub(crate) fn batch_rows_for_tests(rows: usize) {
    BATCH_ROWS.store(rows, Ordering::Relaxed);
}

/// A read that hands its rows on as `Utf8` batches, holding one batch's
/// worth and no more.
pub(crate) struct Batcher<'a> {
    sink: &'a mut dyn Sink,
    columns: Option<Vec<String>>,
    pending: Vec<Vec<Option<String>>>,
    size: usize,
}

impl<'a> Batcher<'a> {
    pub(crate) fn new(sink: &'a mut dyn Sink) -> Batcher<'a> {
        Batcher { sink, columns: None, pending: Vec::new(), size: BATCH_ROWS.load(Ordering::Relaxed).max(1) }
    }

    fn flush(&mut self) -> Result<()> {
        if self.pending.is_empty() {
            return Ok(());
        }
        let rows = Rows { columns: self.columns.clone().unwrap_or_default(), rows: std::mem::take(&mut self.pending) };
        for batch in Batches::from_text(&rows).batches {
            self.sink.batch(batch)?;
        }
        Ok(())
    }

    /// The last rows, and the schema of a result that never named its
    /// columns because it had none to name.
    pub(crate) fn finish(mut self) -> Result<()> {
        if self.columns.is_none() {
            self.columns(&[])?;
        }
        self.flush()
    }
}

impl RowSink for Batcher<'_> {
    fn columns(&mut self, columns: &[String]) -> Result<()> {
        if self.columns.is_none() {
            self.columns = Some(columns.to_vec());
            self.sink.schema(&Batches::from_text(&Rows { columns: columns.to_vec(), rows: Vec::new() }).schema)?;
        }
        Ok(())
    }

    fn row(&mut self, row: Vec<Option<String>>) -> Result<()> {
        self.pending.push(row);
        if self.pending.len() >= self.size {
            self.flush()?;
        }
        Ok(())
    }
}
