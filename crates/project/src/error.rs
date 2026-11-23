use std::path::Path;

use zsql_semantic::BuildError;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{0}: {1}")]
    Io(String, std::io::Error),
    #[error("{0}: {1}")]
    Yaml(String, serde_yaml::Error),
    /// A definition the deployer would refuse, with the file it is in.
    #[error("{file}: {message}")]
    Invalid { file: String, message: String },
    #[error("{0}")]
    Dialect(#[from] zsql_dialect::Error),
    #[error("{0}")]
    SemanticModel(#[from] BuildError),
}

pub(crate) type Result<T> = std::result::Result<T, Error>;

pub(crate) fn invalid(file: &Path, message: impl Into<String>) -> Error {
    Error::Invalid { file: file.display().to_string(), message: message.into() }
}
