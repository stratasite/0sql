use crate::{Batches, Connection, Error, Result, Rows};
use arrow_array::RecordBatch;
use arrow_schema::SchemaRef;

/// Where a result goes as it arrives: a file being written, a response
/// being sent. A read hands over one batch at a time and holds no more
/// than that, so what bounds a result is where it is going and not the
/// memory of whoever fetched it.
pub trait Sink {
    /// Once, before any batch, and for a result with no rows too: the
    /// columns and their types.
    fn schema(&mut self, schema: &SchemaRef) -> Result<()>;

    /// Each batch, in order. An error ends the read: the transport stops
    /// fetching and the error is the read's.
    fn batch(&mut self, batch: RecordBatch) -> Result<()>;
}

/// The row cap a read is given when it is to have none.
const UNBOUNDED: usize = usize::MAX - 1;

/// One read-only statement in, rows out. Implemented once per transport,
/// which is three implementations for thirteen adapters.
pub trait Source {
    /// Runs `sql` and returns at most `max_rows` rows, refusing rather
    /// than truncating past it so a caller never averages a partial
    /// answer. Implementations call [`read_only`] first.
    fn query(&mut self, sql: &str, max_rows: usize) -> Result<Rows>;

    /// The same read with its types kept: what a result that is stored
    /// or handed on is made of. A transport that only has text answers
    /// with `Utf8` columns, which is what this default does.
    fn batches(&mut self, sql: &str, max_rows: usize) -> Result<Batches> {
        Ok(Batches::from_text(&self.query(sql, max_rows)?))
    }

    /// The same read handed over as it arrives, a batch at a time, with
    /// no row cap of its own: a statement that should return fewer rows
    /// says so in its SQL, and a sink that has had enough answers with an
    /// error. Every adapter here fetches incrementally; this default,
    /// which reads the whole result first, is for an implementation that
    /// has nothing better.
    fn stream(&mut self, sql: &str, sink: &mut dyn Sink) -> Result<()> {
        let all = self.batches(sql, UNBOUNDED)?;
        sink.schema(&all.schema)?;
        for batch in all.batches {
            sink.batch(batch)?;
        }
        Ok(())
    }

    /// The adapter's name, for an error that says where it happened.
    fn adapter(&self) -> &str;

    /// How the last read travelled: `native` for a wire protocol or an
    /// engine in this process, `rest` for a warehouse's HTTP API, `adbc`
    /// for its driver. Kept with a stored result, because the same
    /// datasource can answer through two of them.
    fn transport(&self) -> &str {
        "native"
    }

    /// What a caller should be told about this connection that is not an
    /// error: that `prefer` ended plaintext, for one.
    fn warnings(&self) -> &[String] {
        &[]
    }

    /// The connection this was opened from, for a catalog read that
    /// needs more than the adapter's name: BigQuery's project and location.
    fn connection(&self) -> Option<&Connection> {
        None
    }
}

/// Refuses a statement that is not a read.
///
/// `zsql` writes every statement this crate runs, so this is a second
/// pair of eyes rather than a defense: it catches a generated statement
/// that went wrong, and a `sample_sql` an author wrote by hand. The
/// protection that matters is a credential with no write grant, which the
/// documentation asks for.
pub fn read_only(sql: &str) -> Result<()> {
    let refuse = || Error::NotARead(sql.chars().take(80).collect());
    let text = skeleton(sql);
    let trimmed = text.trim().trim_end_matches(';').trim();
    if trimmed.is_empty() || trimmed.contains(';') {
        // One statement per call: a second one is how a read becomes a write.
        return Err(refuse());
    }
    let first: String = trimmed.chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    const READS: [&str; 5] = ["SELECT", "WITH", "SHOW", "DESCRIBE", "PRAGMA"];
    if !READS.contains(&first.to_uppercase().as_str()) {
        return Err(refuse());
    }
    // A read can still carry a write: a data-modifying CTE, `SELECT …
    // INTO`, `INSERT … SELECT` behind a `WITH`. None of these words has a
    // place in a read, so any of them as a bare word refuses. A quoted
    // identifier was blanked with the literals, so a column called
    // `update` in quotes is not caught by this.
    const WRITES: [&str; 16] = [
        "INSERT", "UPDATE", "DELETE", "MERGE", "INTO", "CREATE", "DROP", "ALTER", "TRUNCATE", "GRANT", "REVOKE", "EXEC", "EXECUTE", "CALL",
        "COPY", "UNLOAD",
    ];
    let words = trimmed.split(|c: char| !(c.is_alphanumeric() || c == '_')).filter(|w| !w.is_empty());
    for w in words {
        if WRITES.contains(&w.to_uppercase().as_str()) {
            return Err(refuse());
        }
    }
    Ok(())
}

/// The statement with its comments removed and the inside of every string
/// literal blanked, keeping the quotes. Comments go so that a leading
/// `/* */` cannot hide what the statement starts with and a `--` cannot
/// hide what follows it; literals are blanked so that a semicolon a value
/// legitimately contains is not read as a second statement. Only
/// [`read_only`] sees this; the warehouse gets the statement as written.
fn skeleton(sql: &str) -> String {
    let mut out = String::with_capacity(sql.len());
    let mut chars = sql.chars().peekable();
    let mut quote: Option<char> = None;
    while let Some(c) = chars.next() {
        if let Some(q) = quote {
            if c == q {
                quote = None;
                out.push(c);
            } else {
                // Whatever a value holds, it is not syntax.
                out.push(if c == '\n' { '\n' } else { ' ' });
            }
            continue;
        }
        match c {
            '\'' | '"' => {
                quote = Some(c);
                out.push(c);
            }
            '-' if chars.peek() == Some(&'-') => {
                for c in chars.by_ref() {
                    if c == '\n' {
                        out.push('\n');
                        break;
                    }
                }
            }
            '/' if chars.peek() == Some(&'*') => {
                chars.next();
                let mut last = ' ';
                for c in chars.by_ref() {
                    if last == '*' && c == '/' {
                        break;
                    }
                    last = c;
                }
                out.push(' ');
            }
            _ => out.push(c),
        }
    }
    out
}
