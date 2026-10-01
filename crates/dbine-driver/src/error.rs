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

    /// A statement failed and the driver knows more: the engine's code,
    /// SQLSTATE, position. Reads as a [`Error::Query`] everywhere else
    /// (see [`Error::is_query`]).
    #[error("{}", .0.message)]
    Statement(Box<crate::model::ScriptError>),

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

    /// A statement the server rejected, with or without details.
    pub fn is_query(&self) -> bool {
        matches!(self, Self::Query(_) | Self::Statement(_))
    }

    /// The failure as a [`crate::model::ScriptError`]: the driver's details
    /// when it gave them, the message alone otherwise.
    pub fn to_script_error(&self) -> crate::model::ScriptError {
        match self {
            Self::Statement(e) => (**e).clone(),
            other => crate::model::ScriptError {
                message: other.to_string(),
                fatal: other.ends_script(),
                ..Default::default()
            },
        }
    }

    /// A script can't go on after it, even when it continues on errors:
    /// the connection is gone, the run was cancelled, or the engine says so.
    pub fn ends_script(&self) -> bool {
        match self {
            Self::Connect(_) | Self::AuthFailed(_) | Self::Io(_) | Self::Cancelled => true,
            Self::Statement(e) => e.fatal,
            _ => false,
        }
    }
}

impl From<crate::model::ScriptError> for Error {
    fn from(e: crate::model::ScriptError) -> Self {
        Self::Statement(Box::new(e))
    }
}
