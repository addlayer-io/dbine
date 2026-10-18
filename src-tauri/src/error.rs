use serde::{ser::SerializeStruct, Serialize, Serializer};
use thiserror::Error;

#[derive(Debug, Error, Clone)]
pub enum CommandError {
    #[error("{0}")]
    NotFound(String),

    #[error("{0}")]
    BadRequest(String),

    #[error("{0}")]
    Connect(String),

    /// A connect that failed, classified (firewall, timeout…): serialized
    /// as kind "connect" plus `detail`.
    #[error("{}", .0.message)]
    ConnectFailed(dbine_driver::ConnectFailure),

    #[error("{0}")]
    AuthFailed(String),

    /// The connection is up but a structure read couldn't run in time (the
    /// session busy with another read, or the server blocked on a lock):
    /// not a connection failure, so the UI doesn't recheck the connection.
    #[error("{0}")]
    Busy(String),

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

    /// A secret field's command or secret manager failed (not found, not
    /// signed in, permission denied, timeout…): the message says which and why.
    #[error("{0}")]
    SecretSource(String),

    /// A secret field's source came from outside this computer and wasn't
    /// confirmed: the UI shows `approval` (the exact definition), and if the
    /// user confirms it (`approve_secret_source`) it retries. The message
    /// (all MCP and the phone get) never has the definition's text.
    #[error("la conexión «{}» toma un secreto ({}) de un origen que todavía no se confirmó en esta computadora: abrila en DBine para revisarlo", .0.connection_name, .0.field_label)]
    SecretSourceApproval(Box<crate::secret_sources::Approval>),
}

impl CommandError {
    fn kind(&self) -> &'static str {
        match self {
            Self::NotFound(_) => "not_found",
            Self::BadRequest(_) => "bad_request",
            Self::Connect(_) | Self::ConnectFailed(_) => "connect",
            Self::AuthFailed(_) => "auth_failed",
            Self::Busy(_) => "busy",
            Self::PasswordRequired(_) => "password_required",
            Self::Sql(_) => "sql",
            Self::State(_) => "state",
            Self::Cancelled => "cancelled",
            Self::WrongPassphrase(_) => "wrong_passphrase",
            Self::SyncAuth(_) => "sync_auth",
            Self::Sync(_) => "sync",
            Self::Internal(_) => "internal",
            Self::SshUnknownHost { .. } => "ssh_unknown_host",
            Self::SecretSource(_) => "secret_source",
            Self::SecretSourceApproval(_) => "secret_source_approval",
        }
    }
}

impl Serialize for CommandError {
    fn serialize<S: Serializer>(&self, serializer: S) -> Result<S::Ok, S::Error> {
        let mut s = serializer.serialize_struct("CommandError", 4)?;
        s.serialize_field("kind", self.kind())?;
        s.serialize_field("message", &self.to_string())?;
        match self.detail() {
            Some(d) => s.serialize_field("detail", &d)?,
            None => s.skip_field("detail")?,
        }
        match self {
            Self::SecretSourceApproval(a) => s.serialize_field("approval", a)?,
            _ => s.skip_field("approval")?,
        }
        s.end()
    }
}

/// Why a connect failed, for the UI's explanation (`detail` of a
/// "connect" or "auth_failed" error).
#[derive(Debug, Serialize)]
pub struct ConnectDetail<'a> {
    pub kind: dbine_driver::ConnectFailureKind,
    pub code: Option<&'a str>,
    pub client_ip: Option<&'a str>,
}

impl CommandError {
    pub fn detail(&self) -> Option<ConnectDetail<'_>> {
        match self {
            Self::ConnectFailed(f) => Some(ConnectDetail { kind: f.kind, code: f.code.as_deref(), client_ip: f.client_ip.as_deref() }),
            Self::AuthFailed(_) => Some(ConnectDetail { kind: dbine_driver::ConnectFailureKind::Auth, code: None, client_ip: None }),
            _ => None,
        }
    }
}

impl From<dbine_driver::Error> for CommandError {
    fn from(e: dbine_driver::Error) -> Self {
        use dbine_driver::Error::*;
        match e {
            ConnectFailed(f) => Self::ConnectFailed(*f),
            // Drivers that don't classify: a guess from the text, so every
            // engine gets the explanation when its message is a known one.
            Connect(m) => match dbine_driver::ConnectFailure::from_message(m.clone()) {
                f if f.kind != dbine_driver::ConnectFailureKind::Unknown => Self::ConnectFailed(f),
                _ => Self::Connect(m),
            },
            AuthFailed(m) => Self::AuthFailed(m),
            Unsupported(m) => Self::BadRequest(m),
            Query(m) => Self::Sql(m),
            Statement(e) => Self::Sql(e.message),
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

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{ConnectFailure, ConnectFailureKind};

    #[test]
    fn connect_failures_carry_their_detail() {
        let mut f = ConnectFailure::from_message("Client with IP address '203.0.113.7' is not allowed to access the server.");
        f.code = Some("40615".into());
        let v = serde_json::to_value(CommandError::from(dbine_driver::Error::connect_failed(f))).unwrap();
        assert_eq!(v["kind"], "connect");
        assert_eq!(v["detail"], serde_json::json!({ "kind": "firewall", "code": "40615", "client_ip": "203.0.113.7" }));
        // A driver's plain text, recognized.
        let v = serde_json::to_value(CommandError::from(dbine_driver::Error::Connect("Connection refused (os error 61)".into()))).unwrap();
        assert_eq!((v["kind"].as_str(), v["detail"]["kind"].as_str()), (Some("connect"), Some("refused")));
        // Not recognized: no detail, as before.
        let v = serde_json::to_value(CommandError::from(dbine_driver::Error::Connect("algo".into()))).unwrap();
        assert!(v.get("detail").is_none());
        let v = serde_json::to_value(CommandError::AuthFailed("no".into())).unwrap();
        assert_eq!(v["detail"]["kind"], "auth");
        let v = serde_json::to_value(CommandError::Sql("x".into())).unwrap();
        assert_eq!(v, serde_json::json!({ "kind": "sql", "message": "x" }));
        let _ = ConnectFailureKind::Unknown;
    }
}
