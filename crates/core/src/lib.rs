//! The core: a loaded model, and every read over it.
//!
//! The server keeps one [`Model`] per deployed branch and answers each
//! route from it; the CLI builds one from the project directory at hand
//! and answers the same questions without a server. Both call the
//! functions here, so what a caller gets back, the SQL, the explain
//! graph, the field catalog, a refusal and its class, is the same
//! whichever binary produced it. The server adds only what is about
//! the server: admission, and the bounds it puts on a request's shape;
//! the bounds arrive here as a [`ShapeCheck`] the caller supplies.
//!
//! The planner is "the engine" throughout this repository; this crate
//! is the layer between a caller and the engine, not the engine.

mod error;
mod model;
mod request;
mod time;

#[cfg(test)]
mod tests;

pub use error::{Error, Kind, LoadError, Result, StoredError};
pub use model::{holds, is_secret, is_secret_to_a_copy, load, strip_secrets, without_url_secrets, Model, Trust, SNAPSHOT_FILE, TESTS_FILE};
pub use request::{
    spec_of, unlimited, ExplainResponse, ExploreRequest, ExploreResponse, Planned, ShapeCheck, SqlRequest, SqlResponse, Timings,
};
pub use time::{now, utc_minute, utc_stamp};

pub use zsql_project::values::ValueCatalog;
pub use zsql_project::{Loaded, ProjectTest};
