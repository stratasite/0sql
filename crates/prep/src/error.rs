//! What data prep refuses, and why, in the seven ways the API tells
//! them apart.

/// A refusal or a failure. Each says which kind it is, so the server
/// answers it with the right status without reading the message.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    /// The request is wrong: a bad option, a step that names nothing.
    #[error("{0}")]
    Invalid(String),
    /// The request is well formed and the data will not have it: a file
    /// past the caps, a column whose values do not convert, too many
    /// rows that do not parse.
    #[error("{0}")]
    Unprocessable(String),
    /// No such dataset, source or run in the project.
    #[error("{0}")]
    NotFound(String),
    /// What it works on is busy with another job, or a run id was sent
    /// with another request.
    #[error("{0}")]
    Conflict(String),
    /// A graph refused before anything ran: each problem said of its
    /// node, for a client to act on.
    #[error("{}", said(.0))]
    Problems(Vec<crate::graph::Problem>),
    /// Something on this side broke.
    #[error("{0}")]
    Internal(String),
    /// The server is stopping: nothing new starts, and what runs is
    /// suspended. Worth asking again in a moment.
    #[error("{0}")]
    Stopping(String),
}

pub type Result<T> = std::result::Result<T, Error>;

fn said(problems: &[crate::graph::Problem]) -> String {
    problems.iter().map(crate::graph::Problem::said).collect::<Vec<_>>().join("; ")
}

impl Error {
    pub(crate) fn io(what: &std::path::Path, e: std::io::Error) -> Error {
        Error::Internal(format!("{}: {e}", what.display()))
    }
}

impl From<duckdb::Error> for Error {
    fn from(e: duckdb::Error) -> Self {
        Error::Internal(format!("the engine: {e}"))
    }
}

impl From<arrow_schema::ArrowError> for Error {
    fn from(e: arrow_schema::ArrowError) -> Self {
        Error::Internal(format!("arrow: {e}"))
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::Internal(format!("a stored record: {e}"))
    }
}
