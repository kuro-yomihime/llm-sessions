use std::path::PathBuf;

use crate::model::SourceId;

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("{path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: std::io::Error,
    },

    #[error("{path}: {source}")]
    Sqlite {
        path: PathBuf,
        #[source]
        source: rusqlite::Error,
    },

    #[error("{agent} does not support {what}")]
    Unsupported { agent: SourceId, what: &'static str },

    #[error("serializing output: {0}")]
    Json(#[from] serde_json::Error),

    #[error("--read-only refuses {what}")]
    ReadOnly { what: &'static str },

    #[error("{what}: {got:?} is not a value I understand")]
    BadArgument { what: &'static str, got: String },

    #[error("no session {id} in {}", db.display())]
    MissingRow { db: PathBuf, id: String },

    #[error("{host}: {message}")]
    Remote { host: String, message: String },

    #[error("no home directory")]
    NoHome,

    #[error("clipboard: {message}")]
    Clipboard { message: String },

    /// `llm-sessions serve` could not start or keep serving.
    #[cfg(feature = "serve")]
    #[error("serve: {0}")]
    Serve(String),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

pub fn io(path: impl Into<PathBuf>) -> impl FnOnce(std::io::Error) -> Error {
    let path = path.into();
    move |source| Error::Io { path, source }
}

pub fn sql(path: impl Into<PathBuf>) -> impl FnOnce(rusqlite::Error) -> Error {
    let path = path.into();
    move |source| Error::Sqlite { path, source }
}
