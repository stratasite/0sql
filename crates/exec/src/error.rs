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
    /// A run of the same question is already in flight on another node.
    #[error("a run of {0} is already in flight")]
    InFlight(String),
    #[error("{0}")]
    Registry(String),
    #[error("{0}")]
    Objects(String),
    /// The node's disk has no room for another result and there is
    /// nowhere else to put one.
    #[error("{0}")]
    DiskFull(String),
    /// A unique constraint refused a write: this is its name.
    #[error("the unique constraint {0} refused the write")]
    Taken(String),
}

pub type Result<T> = std::result::Result<T, Error>;
