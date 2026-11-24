//! Running a planned query and keeping its rows.
//!
//! The planner says what to run and where; `zsql-datasource` runs it; this
//! crate decides whether to run it at all, keeps what came back, and
//! finds it again. A result is customer data: it lives where the caller
//! says (the command line puts it under the user's home, outside any
//! repository), it is never deployed, and it is immutable once written.
//! See `docs/run.md`.
//!
//! The store is a layout, not a database: a result is a directory named
//! by its cache key holding `rows.arrow` and `manifest.json`, the
//! manifest written last as the commit marker. Nothing is updated in
//! place, so a copy of the directory is a copy of the store.
//!
//! The rows are an Arrow IPC file: the batches the transport returned,
//! with the types it knew, LZ4-compressed. Anything that reads Arrow
//! reads a stored result without this crate, and a result read back is
//! the result that was written, not text parsed into a guess at it.
//! Results written before 0.7 are `rows.jsonl.gz`, every cell a string;
//! they are still read, as `Utf8` columns, and never written.

mod engine;
mod error;
pub mod json;
mod manifest;
mod objects;
mod postgres;
mod record;
mod registry;
mod runs;
mod store;

pub use engine::{cache_key, CacheStatus, Connector, Engine, Limit, Outcome, RunRequest, DEFAULT_CAP, DEFAULT_TTL};
pub use error::{Error, Result};
pub use manifest::{Column, Lineage, Manifest, Timings, MANIFEST_FORMAT, ROWS_FORMAT, ROWS_FORMAT_TEXT};
pub use objects::{Objects, PREFIX};
pub use postgres::{Postgres, Settings as PostgresSettings, TABLE};
pub use record::{run_cache_key, Kind, Record, Status, DEFAULT_ORG};
pub use registry::{Local, Registry, RESTARTED};
pub use runs::{How, Limits, Opener, Runs, Submit};
pub use store::{arrow_stream, each_batch, page, rows_file, Store, Stored, Writing};
