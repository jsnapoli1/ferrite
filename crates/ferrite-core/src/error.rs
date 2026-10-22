use std::fmt;

/// Errors surfaced by Ferrite services. The HTTP layer maps each variant to a status code.
#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("not found")]
    NotFound,
    #[error("authentication required")]
    Unauthorized,
    #[error("forbidden")]
    Forbidden,
    #[error("{0}")]
    BadRequest(String),
    #[error("{0}")]
    Conflict(String),
    #[error("locked by {0}")]
    Locked(String),
    #[error("quota exceeded")]
    QuotaExceeded,
    #[error("precondition failed")]
    PreconditionFailed,
    #[error("{0}")]
    Unavailable(String),
    #[error(transparent)]
    Internal(#[from] anyhow::Error),
}

pub type Result<T, E = Error> = std::result::Result<T, E>;

impl Error {
    pub fn bad(msg: impl fmt::Display) -> Self {
        Error::BadRequest(msg.to_string())
    }
    pub fn conflict(msg: impl fmt::Display) -> Self {
        Error::Conflict(msg.to_string())
    }
    pub fn internal(msg: impl fmt::Display) -> Self {
        Error::Internal(anyhow::anyhow!("{msg}"))
    }
}

impl From<sqlx::Error> for Error {
    fn from(e: sqlx::Error) -> Self {
        match e {
            sqlx::Error::RowNotFound => Error::NotFound,
            sqlx::Error::Database(ref d) if d.is_unique_violation() => Error::Conflict("already exists".into()),
            other => Error::Internal(other.into()),
        }
    }
}

impl From<std::io::Error> for Error {
    fn from(e: std::io::Error) -> Self {
        match e.kind() {
            std::io::ErrorKind::NotFound => Error::NotFound,
            _ => Error::Internal(e.into()),
        }
    }
}

impl From<serde_json::Error> for Error {
    fn from(e: serde_json::Error) -> Self {
        Error::BadRequest(format!("invalid JSON: {e}"))
    }
}
