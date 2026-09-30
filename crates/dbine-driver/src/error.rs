use thiserror::Error;

/// Every driver reports failures with this type. Driver crates can't add
/// `From` impls for their client's error type here (orphan rule): they map
/// with a small `fn err(e: ClientError) -> Error` of their own.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Connect(String),

    #[error("{0}")]
    AuthFailed(String),

    #[error("{0}")]
    Unsupported(String),

    /// The server rejected a statement (syntax, permissions, constraint…).
    #[error("{0}")]
    Query(String),

    #[error("{0}")]
    State(String),

    #[error("{0}")]
    Secrets(String),

    #[error("cancelado")]
    Cancelled,

    #[error("io: {0}")]
    Io(#[from] std::io::Error),

    #[error("serde: {0}")]
    Serde(#[from] serde_json::Error),
}

pub type Result<T> = std::result::Result<T, Error>;

impl Error {
    pub fn query(e: impl std::fmt::Display) -> Self {
        Self::Query(e.to_string())
    }
    pub fn connect(e: impl std::fmt::Display) -> Self {
        Self::Connect(e.to_string())
    }
}
