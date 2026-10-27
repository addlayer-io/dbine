use thiserror::Error;

/// Every driver reports failures with this type. Driver crates can't add
/// `From` impls for their client's error type here (orphan rule): they map
/// with a small `fn err(e: ClientError) -> Error` of their own.
#[derive(Debug, Error)]
pub enum Error {
    #[error("{0}")]
    Connect(String),

    /// A connect that failed and the driver knows why (see
    /// [`ConnectFailure`]). Reads as a [`Error::Connect`] everywhere else
    /// (see [`Error::is_connect`]); drivers that don't classify keep
    /// returning `Connect(text)`.
    #[error("{}", .0.message)]
    ConnectFailed(Box<ConnectFailure>),

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

    /// A connect that failed, with why.
    pub fn connect_failed(f: ConnectFailure) -> Self {
        Self::ConnectFailed(Box::new(f))
    }

    /// The connection couldn't be opened or broke: [`Error::Connect`] or
    /// [`Error::ConnectFailed`].
    pub fn is_connect(&self) -> bool {
        matches!(self, Self::Connect(_) | Self::ConnectFailed(_))
    }

    /// Why a connect failed: the driver's classification, or a guess from the
    /// text of a plain [`Error::Connect`] (an [`ConnectFailureKind::Unknown`]
    /// when nothing in it is recognized). `None` for other errors.
    pub fn connect_failure(&self) -> Option<ConnectFailure> {
        match self {
            Self::ConnectFailed(f) => Some((**f).clone()),
            Self::Connect(m) => Some(ConnectFailure::from_message(m.clone())),
            Self::AuthFailed(m) => Some(ConnectFailure::new(ConnectFailureKind::Auth, m.clone())),
            _ => None,
        }
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
            Self::Connect(_)
            | Self::ConnectFailed(_)
            | Self::AuthFailed(_)
            | Self::Io(_)
            | Self::Cancelled => true,
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

/// Why a connection couldn't be opened, as far as the driver (or the
/// network error) tells: the app explains each kind and what to check.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectFailureKind {
    /// The server (or a firewall in front of it) refuses this client's IP
    /// (Azure SQL's server firewall: 40615).
    Firewall,
    /// Nothing answered in time: a firewall dropping packets, a VPN that's
    /// off, a server that's down.
    Timeout,
    /// The host answered but nothing listens on that port.
    Refused,
    /// The host name doesn't resolve.
    Dns,
    /// The TLS handshake or the server's certificate failed.
    Tls,
    /// TLS was asked for and the server has none (a development server
    /// without TLS): the connection's TLS mode has to allow plaintext. An
    /// older app reads it as `Unknown`.
    TlsNotOffered,
    /// The server's certificate couldn't be verified (unknown CA, self-signed,
    /// another host name): the CA file, or a mode that doesn't verify. An
    /// older app reads it as `Unknown`.
    TlsUntrusted,
    /// The server's certificate isn't the one pinned on the first
    /// connection (a mode that doesn't verify the chain pins the
    /// certificate instead, `config::TLS_PIN`): the server got a new
    /// certificate, or something in between answers for it. An older app
    /// reads it as `Unknown`.
    TlsPinChanged,
    /// The login was rejected.
    Auth,
    /// Anything else (and kinds a newer driver sends that this app doesn't know).
    #[default]
    #[serde(other)]
    Unknown,
}

impl ConnectFailureKind {
    /// A TLS failure of any kind.
    pub fn is_tls(self) -> bool {
        matches!(
            self,
            Self::Tls | Self::TlsNotOffered | Self::TlsUntrusted | Self::TlsPinChanged
        )
    }
}

/// A failed connect, classified. Crosses the plugin protocol as an optional
/// field: a peer that doesn't know it reads a plain connect error.
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
pub struct ConnectFailure {
    #[serde(default)]
    pub kind: ConnectFailureKind,
    /// The server's error number or SQLSTATE, when it answered with one.
    #[serde(default)]
    pub code: Option<String>,
    /// This client's address as the server saw it, when it says (Azure SQL
    /// does, in 40615).
    #[serde(default)]
    pub client_ip: Option<String>,
    pub message: String,
}

impl ConnectFailure {
    pub fn new(kind: ConnectFailureKind, message: impl Into<String>) -> Self {
        Self {
            kind,
            code: None,
            client_ip: None,
            message: message.into(),
        }
    }

    pub fn with_code(mut self, code: impl ToString) -> Self {
        self.code = Some(code.to_string());
        self
    }

    /// Classified from its text alone (see [`classify_message`]); the
    /// client's IP too, when the text has it.
    pub fn from_message(message: impl Into<String>) -> Self {
        let message = message.into();
        let mut f = Self::new(classify_message(&message), message);
        f.client_ip = client_ip_in(&f.message);
        f
    }

    /// From an OS-level error: its kind first, its text when the kind says
    /// nothing (name lookups fail with kinds that vary by OS).
    pub fn from_io(e: &std::io::Error, message: impl Into<String>) -> Self {
        let message = message.into();
        let kind = match classify_io_kind(e.kind()) {
            ConnectFailureKind::Unknown => classify_message(&format!("{e} {message}")),
            k => k,
        };
        Self::new(kind, message)
    }

    /// From a client library's error: the first `std::io::Error` in its
    /// chain of sources decides, else the text of the whole chain.
    pub fn from_error(e: &(dyn std::error::Error + 'static)) -> Self {
        let message = e.to_string();
        let mut text = message.clone();
        let mut cur: Option<&(dyn std::error::Error + 'static)> = Some(e);
        let mut first = true;
        while let Some(x) = cur {
            if let Some(io) = x.downcast_ref::<std::io::Error>() {
                if let k @ (ConnectFailureKind::Timeout | ConnectFailureKind::Refused) =
                    classify_io_kind(io.kind())
                {
                    return Self::new(k, message);
                }
            }
            if !first {
                text.push(' ');
                text.push_str(&x.to_string());
            }
            first = false;
            cur = x.source();
        }
        let mut f = Self::new(classify_message(&text), message);
        f.client_ip = client_ip_in(&text);
        f
    }
}

/// What an OS error kind says about a connect; `Unknown` when it says nothing.
pub fn classify_io_kind(kind: std::io::ErrorKind) -> ConnectFailureKind {
    use std::io::ErrorKind as K;
    match kind {
        K::TimedOut => ConnectFailureKind::Timeout,
        // No route: the host can't be reached from here (VPN, network), the
        // same checks as a refused port.
        K::ConnectionRefused | K::HostUnreachable | K::NetworkUnreachable => {
            ConnectFailureKind::Refused
        }
        _ => ConnectFailureKind::Unknown,
    }
}

/// A connect failure's kind guessed from its text (English and Spanish
/// messages of clients, OSes and DBine's drivers). `Unknown` when nothing
/// matches.
pub fn classify_message(text: &str) -> ConnectFailureKind {
    let t = text.to_lowercase();
    let has = |words: &[&str]| words.iter().any(|w| t.contains(w));
    if has(&[
        r"is not allowed to access the server",
        r"client with ip address",
        r"not allowed to connect to this mysql server",
        r"no pg_hba.conf entry",
    ]) {
        ConnectFailureKind::Firewall
    } else if has(&[
        r"failed to lookup address",
        r"nodename nor servname",
        r"name or service not known",
        r"no such host",
        r"getaddrinfo",
        r"could not translate host name",
        r"temporary failure in name resolution",
        r"dns error",
        r"no se encontró el servidor",
    ]) {
        ConnectFailureKind::Dns
    } else if has(&[
        r"connection refused",
        r"actively refused",
        r"rechazó la conexión",
        r"os error 61)",
        r"os error 111)",
        r"os error 10061)",
        r"no route to host",
        r"network is unreachable",
        r"no hay ruta",
    ]) {
        ConnectFailureKind::Refused
    } else if has(&[
        r"timed out",
        r"timeout",
        r"time out",
        r"tiempo de espera",
        r"no respondió",
    ]) {
        ConnectFailureKind::Timeout
    } else if has(&[
        r"password authentication failed",
        r"login failed",
        r"access denied",
        r"authentication failed",
        r"invalid password",
        r"invalid username",
    ]) {
        ConnectFailureKind::Auth
    } else if has(&[
        r"server does not support ssl",
        r"server does not support tls",
        r"ssl is not enabled on the server",
        r"server doesn't support ssl",
        r"does not support ssl connections",
        r"ssl connection is required but the server",
        r"tls not supported",
        r"ssl not supported",
    ]) {
        ConnectFailureKind::TlsNotOffered
    } else if has(&[
        r"unknownissuer",
        r"unknown issuer",
        r"notvalidforname",
        r"not valid for name",
        r"self signed",
        r"self-signed",
        r"certificate verify failed",
        r"unable to get local issuer certificate",
        r"certificate is not trusted",
        r"hostname mismatch",
        r"invalid peer certificate",
        // macOS (Security.framework, native-tls).
        r"certificate was not trusted",
        r"invalid certificate chain",
        r"certificate is not standards compliant",
        // Windows (Schannel): SQL Server's usual ones.
        r"issued by an authority that is not trusted",
        r"target principal name is incorrect",
        r"certificate chain was issued",
    ]) {
        ConnectFailureKind::TlsUntrusted
    } else if has(&[r"certificate", r"tls", r"ssl", r"handshake", r"x509"]) {
        ConnectFailureKind::Tls
    } else {
        ConnectFailureKind::Unknown
    }
}

/// The client IP a server names in its refusal (r"Client with IP address
/// '203.0.113.7' is not allowed…", MySQL's r"Host '203.0.113.7' is not
/// allowed…"): a valid IPv4 or IPv6 address only.
pub fn client_ip_in(text: &str) -> Option<String> {
    // ASCII lowering keeps byte offsets, so they index `text` too.
    let lower = text.to_ascii_lowercase();
    for marker in [
        r"ip address '",
        r#"ip address ""#,
        r"host '",
        r#"entry for host ""#,
    ] {
        let Some(at) = lower.find(marker) else {
            continue;
        };
        let rest = &text[at + marker.len()..];
        let end = rest.find(['\'', '"'])?;
        let ip = &rest[..end];
        if ip.parse::<std::net::IpAddr>().is_ok() {
            return Some(ip.to_string());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn messages_are_classified() {
        let k = |m: &str| classify_message(m);
        assert_eq!(k("Cannot open server 'x' requested by the login. Client with IP address '203.0.113.7' is not allowed to access the server."), ConnectFailureKind::Firewall);
        assert_eq!(k("failed to lookup address information: nodename nor servname provided, or not known"), ConnectFailureKind::Dns);
        assert_eq!(
            k("Connection refused (os error 61)"),
            ConnectFailureKind::Refused
        );
        assert_eq!(k("tiempo de espera agotado"), ConnectFailureKind::Timeout);
        assert_eq!(
            k("password authentication failed for user \"x\""),
            ConnectFailureKind::Auth
        );
        assert_eq!(
            k("invalid peer certificate: UnknownIssuer"),
            ConnectFailureKind::TlsUntrusted
        );
        assert_eq!(
            k("error performing TLS handshake: server does not support TLS"),
            ConnectFailureKind::TlsNotOffered
        );
        for m in [
            "The certificate was not trusted.",
            "Invalid certificate chain",
            "TLS handshake failed: The certificate chain was issued by an authority that is not trusted.",
            "The target principal name is incorrect.",
        ] {
            assert_eq!(k(m), ConnectFailureKind::TlsUntrusted, "{m}");
        }
        assert_eq!(k("tls handshake eof"), ConnectFailureKind::Tls);
        assert!(ConnectFailureKind::TlsUntrusted.is_tls() && !ConnectFailureKind::Auth.is_tls());
        assert_eq!(k("algo raro"), ConnectFailureKind::Unknown);
    }

    #[test]
    fn client_ip_is_read_only_when_valid() {
        let m = "Cannot open server 'srv' requested by the login. Client with IP address '203.0.113.7' is not allowed to access the server.";
        assert_eq!(client_ip_in(m).as_deref(), Some("203.0.113.7"));
        assert_eq!(
            client_ip_in("Host '10.0.0.5' is not allowed to connect to this MySQL server")
                .as_deref(),
            Some("10.0.0.5")
        );
        assert_eq!(
            client_ip_in("Client with IP address 'nope; rm -rf' is not allowed").as_deref(),
            None
        );
        assert_eq!(client_ip_in("nothing here"), None);
        let f = ConnectFailure::from_message(m);
        assert_eq!(
            (f.kind, f.client_ip.as_deref()),
            (ConnectFailureKind::Firewall, Some("203.0.113.7"))
        );
    }

    #[test]
    fn io_errors_are_classified_by_kind() {
        use std::io::{Error as E, ErrorKind as K};
        assert_eq!(
            ConnectFailure::from_io(&E::from(K::TimedOut), "x").kind,
            ConnectFailureKind::Timeout
        );
        assert_eq!(
            ConnectFailure::from_io(&E::from(K::ConnectionRefused), "x").kind,
            ConnectFailureKind::Refused
        );
        assert_eq!(
            ConnectFailure::from_io(&E::other("failed to lookup address information"), "x").kind,
            ConnectFailureKind::Dns
        );
        // Wrapped in a client's error: found through `source()`.
        #[derive(Debug)]
        struct Wrap(E);
        impl std::fmt::Display for Wrap {
            fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
                f.write_str("error connecting to server")
            }
        }
        impl std::error::Error for Wrap {
            fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
                Some(&self.0)
            }
        }
        let f = ConnectFailure::from_error(&Wrap(E::from(K::ConnectionRefused)));
        assert_eq!(
            (f.kind, f.message.as_str()),
            (ConnectFailureKind::Refused, "error connecting to server")
        );
        let f = ConnectFailure::from_error(&Wrap(E::other("failed to lookup address information")));
        assert_eq!(f.kind, ConnectFailureKind::Dns);
    }

    #[test]
    fn unknown_kinds_from_newer_peers_read_as_unknown() {
        let f: ConnectFailure = serde_json::from_str(r#"{"kind":"quota","message":"m"}"#).unwrap();
        assert_eq!(f.kind, ConnectFailureKind::Unknown);
        let e = Error::connect_failed(ConnectFailure::new(ConnectFailureKind::Timeout, "slow"));
        assert!(e.is_connect() && e.ends_script());
        assert_eq!(e.to_string(), "slow");
        assert_eq!(
            Error::Connect("Connection refused".into())
                .connect_failure()
                .unwrap()
                .kind,
            ConnectFailureKind::Refused
        );
    }
}
