#[cfg(doc)]
use crate::model::Model;

/// What kind of refusal this is. The server turns it into a status;
/// the CLI into an exit code.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The request itself is malformed or missing something.
    Invalid,
    /// The request is well formed but the model cannot answer it.
    Unprocessable,
    NotFound,
    /// Something this side should have had and did not.
    Internal,
}

/// A refusal, with the class the goldens and the API name it by.
#[derive(Debug, Clone, thiserror::Error)]
#[error("{class}: {message}")]
pub struct Error {
    pub kind: Kind,
    pub class: &'static str,
    pub message: String,
}

impl Error {
    pub fn new(kind: Kind, class: &'static str, message: impl Into<String>) -> Self {
        Error { kind, class, message: message.into() }
    }
    pub fn invalid(class: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Invalid, class, message)
    }
    pub fn unprocessable(class: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Unprocessable, class, message)
    }
    pub fn not_found(message: impl Into<String>) -> Self {
        Self::new(Kind::NotFound, "NotFound", message)
    }
    pub fn internal(class: &'static str, message: impl Into<String>) -> Self {
        Self::new(Kind::Internal, class, message)
    }
}

impl From<zsql_spec::Error> for Error {
    fn from(e: zsql_spec::Error) -> Self {
        Self::unprocessable(e.error_class(), e.to_string())
    }
}

impl From<zsql_planner::Error> for Error {
    fn from(e: zsql_planner::Error) -> Self {
        Self::unprocessable(e.error_class(), e.to_string())
    }
}

impl From<zsql_dialect::Error> for Error {
    fn from(e: zsql_dialect::Error) -> Self {
        Self::internal("Dialect", e.to_string())
    }
}

pub type Result<T> = std::result::Result<T, Error>;

/// Why a project directory did not become a model.
#[derive(Debug, thiserror::Error)]
pub enum LoadError {
    #[error("{0}")]
    Project(#[from] zsql_project::Error),
    #[error("{0}")]
    SemanticModel(#[from] zsql_semantic::BuildError),
}

/// Why a stored model (what [`Model::write`] put on disk) did not load.
#[derive(Debug, thiserror::Error)]
pub enum StoredError {
    #[error("{0}")]
    Snapshot(#[from] zsql_semantic::snapshot::LoadError),
    #[error("{0}")]
    SemanticModel(#[from] zsql_semantic::BuildError),
    #[error("{0}")]
    Io(#[from] std::io::Error),
    #[error("tests.json does not parse: {0}")]
    Tests(String),
    #[error("secrets.json does not parse: {0}")]
    Secrets(String),
}
