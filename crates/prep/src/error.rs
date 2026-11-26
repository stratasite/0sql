//! What data prep refuses, and why, in the five ways the API tells
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
    /// No such dataset in the project.
    #[error("{0}")]
    NotFound(String),
    /// The change was made against a recipe that has moved on, or the
    /// dataset is busy with another job.
    #[error("{0}")]
    Conflict(String),
    /// Something on this side broke.
    #[error("{0}")]
    Internal(String),
}

pub type Result<T> = std::result::Result<T, Error>;

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
