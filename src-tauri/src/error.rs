use serde::{ser::SerializeStruct, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Error)]
pub enum CommandError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Connect(String),

    #[error("{0}")]
    AuthFailed(String),

    /// No password to connect with (the connection doesn't keep it, or the
    /// keychain couldn't be read): the UI asks for it and retries. The
    /// message says why when there's more to say.
    #[error("{}", .0.as_deref().unwrap_or("se necesita la contraseña"))]
    PasswordRequired(Option<String>),

    #[error("{0}")]
    Sql(String),

    #[error("{0}")]
    State(String),

    #[error("cancelado")]
    Cancelled,

    /// The cloud backup's passphrase didn't open it.
    #[error("{0}")]
    WrongPassphrase(String),

    /// The cloud account needs signing in again.
    #[error("{0}")]
    SyncAuth(String),

    /// Talking to the cloud provider failed.
    #[error("{0}")]
    Sync(String),

    #[error("{0}")]
    Internal(String),

    /// The SSH server of a tunnel isn't known: the UI shows the fingerprint
    /// and, if the user trusts it, saves it (`trust_ssh_host`) and retries.
    #[error("el servidor SSH {host}:{port} no es conocido. Verificá que su huella sea {fingerprint} y aceptala para conectarte.")]
    SshUnknownHost { host: String, port: u16, fingerprint: String },
}

impl CommandError {
    fn kind(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Connect(_) => "connect",
            Self::AuthFailed(_) => "auth_failed",
            Self::PasswordRequired(_) => "password_required",
            Self::Sql(_) => "sql",
            Self::State(_) => "state",
            Self::Cancelled => "cancelled",
            Self::WrongPassphrase(_) => "wrong_passphrase",
            Self::SyncAuth(_) => "sync_auth",
            Self::Sync(_) => "sync",
            Self::Internal(_) => "internal",
            Self::SshUnknownHost { .. } => "ssh_unknown_host",
        }
    }
}

impl Serialize for CommandError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CommandError", 2)?;
        s.serialize_field("kind", self.kind())?;
        s.serialize_field("message", &self.to_string())?;
        s.end()
    }
}

impl From<dbine_driver::Error> for CommandError {
    fn from(e: dbine_driver::Error) -> Self {
        use dbine_driver::Error::*;
        match e {
            Connect(m) => Self::Connect(m),
            AuthFailed(m) => Self::AuthFailed(m),
            Unsupported(m) => Self::BadRequest(m),
            Query(m) => Self::Sql(m),
            State(m) => Self::State(m),
            Secrets(m) => Self::State(format!("llavero del sistema: {m}")),
            Cancelled => Self::Cancelled,
            Io(e) => Self::Internal(format!("io: {e}")),
            Serde(e) => Self::Internal(format!("serde: {e}")),
        }
    }
}

impl From<dbine_sync::SyncError> for CommandError {
    fn from(e: dbine_sync::SyncError) -> Self {
        use dbine_sync::SyncError::*;
        match e {
            WrongPassphrase => Self::WrongPassphrase(e.to_string()),
            AuthRequired(_) => Self::SyncAuth(e.to_string()),
            Cancelled => Self::Cancelled,
            Remote(m) | Local(m) | Format(m) => Self::Sync(m),
        }
    }
}

pub type CommandResult<T> = Result<T, CommandError>;
