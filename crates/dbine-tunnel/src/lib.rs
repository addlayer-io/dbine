//! SSH tunnels for DBine's connections: a local port (127.0.0.1, chosen by
//! the system) forwarded to the database server through an SSH server,
//! optionally reached through jump hosts (bastions). The driver connects to
//! the local port as if it were the server, so every engine that connects
//! over the network gets tunnels without knowing about them.
//!
//! Server keys are checked: a key in the user's `~/.ssh/known_hosts` or one
//! the user accepted before (`Spec::trusted`) passes; an unknown one is an
//! [`Error::UnknownHost`] carrying its fingerprint, for the app to ask; a key
//! that differs from the one in known_hosts is refused.

use russh::client::{self, Handle};
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKey};
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

/// An SSH server on the way to the database.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Hop {
    pub host: String,
    pub port: u16,
    pub user: String,
}

#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Auth {
    Password(String),
    /// A private key file (OpenSSH or PEM), with its passphrase if it has one.
    Key { path: PathBuf, passphrase: Option<String> },
    /// The running SSH agent (ssh-agent, 1Password, Pageant…).
    Agent,
}

impl std::fmt::Debug for Auth {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Auth::Password(_) => f.write_str("Password(…)"),
            Auth::Key { path, .. } => write!(f, "Key({})", path.display()),
            Auth::Agent => f.write_str("Agent"),
        }
    }
}

/// What to open: the SSH hosts in order (jump hosts first, the last one is
/// the SSH server that reaches the database) and the database server as
/// that last host sees it.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct Spec {
    pub hops: Vec<Hop>,
    pub auth: Auth,
    pub target_host: String,
    pub target_port: u16,
    /// Server key fingerprints (`SHA256:…`) the user accepted.
    pub trusted: Vec<String>,
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("el servidor SSH {host}:{port} no es conocido (huella {fingerprint})")]
    UnknownHost { host: String, port: u16, fingerprint: String },
    #[error("la clave del servidor SSH {host}:{port} no coincide con la de known_hosts: puede ser otro servidor haciéndose pasar por él. Si el servidor cambió de clave, actualizá tu known_hosts.")]
    HostKeyChanged { host: String, port: u16 },
    #[error("el servidor SSH {host} rechazó al usuario «{user}»: {how}")]
    Auth { host: String, user: String, how: String },
    #[error("no se pudo conectar al servidor SSH {host}:{port}: {message}")]
    Connect { host: String, port: u16, message: String },
    #[error("no se pudo usar la clave {path}: {message}")]
    Key { path: String, message: String },
    #[error("no se pudo usar el agente SSH: {0}")]
    Agent(String),
    #[error("el túnel SSH no pudo llegar a {host}:{port}: {message}")]
    Forward { host: String, port: u16, message: String },
    #[error("el túnel SSH no pudo abrir un puerto local: {0}")]
    Listen(std::io::Error),
}

/// An open tunnel. Dropping it closes the local port, the connections
/// through it and the SSH sessions.
pub struct Tunnel {
    local_port: u16,
    last: Arc<Handle<Checker>>,
    _hops: Vec<Arc<Handle<Checker>>>,
    accept: JoinHandle<()>,
}

impl Tunnel {
    /// The local port that leads to the database server.
    pub fn local_port(&self) -> u16 {
        self.local_port
    }

    /// False once the SSH session dropped (network change, server restart):
    /// open a new tunnel then.
    pub fn is_alive(&self) -> bool {
        !self.last.is_closed()
    }
}

impl Drop for Tunnel {
    fn drop(&mut self) {
        self.accept.abort();
    }
}

/// Checks each server's key (see the module docs) and remembers the one it
/// saw, for the error.
struct Checker {
    host: String,
    port: u16,
    trusted: Vec<String>,
    seen: Arc<Mutex<Option<Verdict>>>,
}

#[derive(Clone)]
enum Verdict {
    Unknown(String),
    Changed,
}

impl client::Handler for Checker {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let key = match key {
            keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            keys::PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        let key = &key;
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        if self.trusted.iter().any(|t| t == &fingerprint) {
            return Ok(true);
        }
        match keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                *self.seen.lock().unwrap() = Some(Verdict::Unknown(fingerprint));
                Ok(false)
            }
            Err(keys::Error::KeyChanged { .. }) => {
                *self.seen.lock().unwrap() = Some(Verdict::Changed);
                Ok(false)
            }
            // No known_hosts file (or an unreadable one): the key is unknown.
            Err(_) => {
                *self.seen.lock().unwrap() = Some(Verdict::Unknown(fingerprint));
                Ok(false)
            }
        }
    }
}

fn config() -> Arc<client::Config> {
    Arc::new(client::Config {
        // Keep NATs and firewalls from dropping an idle tunnel.
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        inactivity_timeout: None,
        nodelay: true,
        ..Default::default()
    })
}

/// Open the tunnel: connect and authenticate every hop, then listen on a
/// local port.
pub async fn open(spec: &Spec) -> Result<Tunnel, Error> {
    let Some(first) = spec.hops.first() else {
        return Err(Error::Connect { host: String::new(), port: 0, message: "falta el servidor SSH".into() });
    };
    let mut hops: Vec<Arc<Handle<Checker>>> = Vec::new();
    for (i, hop) in spec.hops.iter().enumerate() {
        let seen = Arc::new(Mutex::new(None));
        let checker = Checker { host: hop.host.clone(), port: hop.port, trusted: spec.trusted.clone(), seen: seen.clone() };
        let connecting = match hops.last() {
            None => tokio::time::timeout(Duration::from_secs(20), client::connect(config(), (first.host.as_str(), first.port), checker)).await,
            Some(prev) => {
                // The next hop, reached from inside the previous one.
                let channel = prev
                    .channel_open_direct_tcpip(hop.host.clone(), hop.port.into(), "127.0.0.1", 0)
                    .await
                    .map_err(|e| Error::Connect { host: hop.host.clone(), port: hop.port, message: format!("desde {}: {e}", spec.hops[i - 1].host) })?;
                tokio::time::timeout(Duration::from_secs(20), client::connect_stream(config(), channel.into_stream(), checker)).await
            }
        };
        let mut handle = match connecting {
            Err(_) => return Err(Error::Connect { host: hop.host.clone(), port: hop.port, message: "no respondió en 20 s".into() }),
            Ok(Err(e)) => {
                return Err(match seen.lock().unwrap().clone() {
                    Some(Verdict::Unknown(fingerprint)) => Error::UnknownHost { host: hop.host.clone(), port: hop.port, fingerprint },
                    Some(Verdict::Changed) => Error::HostKeyChanged { host: hop.host.clone(), port: hop.port },
                    None => Error::Connect { host: hop.host.clone(), port: hop.port, message: e.to_string() },
                })
            }
            Ok(Ok(h)) => h,
        };
        authenticate(&mut handle, hop, &spec.auth).await?;
        hops.push(Arc::new(handle));
    }
    let last = hops.last().cloned().expect("at least one hop");

    let listener = TcpListener::bind(("127.0.0.1", 0)).await.map_err(Error::Listen)?;
    let local_port = listener.local_addr().map_err(Error::Listen)?.port();
    let (target_host, target_port) = (spec.target_host.clone(), spec.target_port);
    // The database has to be reachable before the driver is told to use it.
    last.channel_open_direct_tcpip(target_host.clone(), target_port.into(), "127.0.0.1", 0)
        .await
        .map_err(|e| Error::Forward { host: target_host.clone(), port: target_port, message: e.to_string() })?
        .close()
        .await
        .ok();

    let session = last.clone();
    let accept = tokio::spawn(async move {
        loop {
            let Ok((mut socket, peer)) = listener.accept().await else { break };
            let session = session.clone();
            let host = target_host.clone();
            tokio::spawn(async move {
                let _ = socket.set_nodelay(true);
                match session.channel_open_direct_tcpip(host.clone(), target_port.into(), peer.ip().to_string(), peer.port().into()).await {
                    Ok(channel) => {
                        let mut remote = channel.into_stream();
                        let _ = tokio::io::copy_bidirectional(&mut socket, &mut remote).await;
                    }
                    Err(e) => tracing::warn!(%e, "SSH tunnel: couldn't reach {host}:{target_port}"),
                }
            });
        }
    });
    Ok(Tunnel { local_port, last, _hops: hops, accept })
}

async fn authenticate(handle: &mut Handle<Checker>, hop: &Hop, auth: &Auth) -> Result<(), Error> {
    let fail = |how: &str| Error::Auth { host: hop.host.clone(), user: hop.user.clone(), how: how.to_string() };
    let net = |e: russh::Error| Error::Connect { host: hop.host.clone(), port: hop.port, message: e.to_string() };
    let ok = match auth {
        Auth::Password(password) => handle.authenticate_password(hop.user.clone(), password.clone()).await.map_err(net)?.success(),
        Auth::Key { path, passphrase } => {
            let key = keys::load_secret_key(path, passphrase.as_deref().filter(|p| !p.is_empty()))
                .map_err(|e| Error::Key { path: path.display().to_string(), message: key_error(&e, passphrase.is_some()) })?;
            let hash = handle.best_supported_rsa_hash().await.map_err(net)?.flatten();
            handle.authenticate_publickey(hop.user.clone(), PrivateKeyWithHashAlg::new(Arc::new(key), hash)).await.map_err(net)?.success()
        }
        Auth::Agent => {
            let mut agent = agent().await?;
            let identities = agent.request_identities().await.map_err(|e| Error::Agent(e.to_string()))?;
            if identities.is_empty() {
                return Err(Error::Agent("no tiene claves cargadas (ssh-add)".into()));
            }
            let hash = handle.best_supported_rsa_hash().await.map_err(net)?.flatten();
            let mut ok = false;
            for id in identities {
                let key = match id {
                    keys::agent::AgentIdentity::PublicKey { key, .. } => key,
                    keys::agent::AgentIdentity::Certificate { certificate, .. } => PublicKey::from(certificate.public_key().clone()),
                };
                if handle.authenticate_publickey_with(hop.user.clone(), key, hash, &mut agent).await.map_err(|e| Error::Agent(e.to_string()))?.success() {
                    ok = true;
                    break;
                }
            }
            ok
        }
    };
    if ok {
        Ok(())
    } else {
        Err(fail(match auth {
            Auth::Password(_) => "contraseña incorrecta",
            Auth::Key { .. } => "no acepta esa clave",
            Auth::Agent => "no acepta ninguna clave del agente",
        }))
    }
}

fn key_error(e: &keys::Error, had_passphrase: bool) -> String {
    match e {
        keys::Error::KeyIsEncrypted if !had_passphrase => "está protegida con una frase; escribila en el campo de la frase".into(),
        keys::Error::KeyIsEncrypted => "la frase no es correcta".into(),
        e => e.to_string(),
    }
}

#[cfg(unix)]
async fn agent() -> Result<keys::agent::client::AgentClient<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send>, Error> {
    keys::agent::client::AgentClient::connect_env().await.map_err(|e| Error::Agent(format!("{e} (¿está corriendo? SSH_AUTH_SOCK)")))
}

#[cfg(windows)]
async fn agent() -> Result<keys::agent::client::AgentClient<impl tokio::io::AsyncRead + tokio::io::AsyncWrite + Unpin + Send>, Error> {
    // The OpenSSH agent service of Windows; Pageant as a fallback.
    match keys::agent::client::AgentClient::connect_named_pipe(r"\\.\pipe\openssh-ssh-agent").await {
        Ok(a) => Ok(a),
        Err(e) => Err(Error::Agent(format!("{e} (¿está corriendo el servicio «OpenSSH Authentication Agent»?)"))),
    }
}
