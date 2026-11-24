#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("planning: {0}")]
    Plan(String),
    #[error("{0}")]
    Source(#[from] zsql_datasource::Error),
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    #[error("{0}")]
    Json(#[from] serde_json::Error),
    #[error("no result {0}")]
    NotFound(String),
    #[error("{0} matches more than one result; give more of the id")]
    Ambiguous(String),
    #[error("{0}: {1}")]
    Arrow(String, arrow_schema::ArrowError),
    #[error("{0}: rows in a format this build does not read: {1}")]
    RowsFormat(String, String),
    #[error("busy: {0} runs are already waiting")]
    Busy(usize),
    #[error("a run with the id {0} is already recorded")]
    Duplicate(String),
}

pub type Result<T> = std::result::Result<T, Error>;
