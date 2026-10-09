//! SSH tunnels for DBine's connections: a local port (127.0.0.1, chosen by
//! the system) forwarded to the database server through an SSH server,
//! optionally reached through jump hosts (bastions). The driver connects to
//! the local port as if it were the server, so every engine that connects
//! over the network gets tunnels without knowing about them.
//!
//! Server keys are checked, each hop on its own: `~/.ssh/known_hosts` first
//! (a key that differs from the one there, or of another type than the ones
//! there, is refused, whatever the user accepted), then the keys the user
//! accepted for that same server (`Spec::trusted`, entries bound to
//! `[host]:port`): a server with an accepted key that presents another one
//! is refused too ([`Error::TrustedKeyChanged`]); only a server with nothing
//! accepted is an [`Error::UnknownHost`] carrying its fingerprint, for the
//! app to ask.
//!
//! The local port only serves processes of the user running DBine (see
//! `peer`): another account on the same machine can't ride the user's SSH
//! session.

use russh::client::{self, Handle};
use russh::keys::{self, HashAlg, PrivateKeyWithHashAlg, PublicKey};
use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use std::time::Duration;
use tokio::net::TcpListener;
use tokio::task::JoinHandle;

mod peer;

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
    /// Server keys the user accepted, each bound to the server it was
    /// accepted for: `[host]:port SHA256:…` ([`trusted_entry`]). Entries
    /// without the server (a bare `SHA256:…`, as older versions saved them)
    /// are ignored: the user is asked again.
    pub trusted: Vec<String>,
}

/// The `Spec::trusted` entry for a key accepted for `host:port`.
pub fn trusted_entry(host: &str, port: u16, fingerprint: &str) -> String {
    format!("[{host}]:{port} {fingerprint}")
}

/// The server and fingerprint of a `Spec::trusted` entry; None for a bare
/// fingerprint or anything malformed.
pub fn parse_trusted(entry: &str) -> Option<(&str, u16, &str)> {
    let rest = entry.trim().strip_prefix('[')?;
    let (host, rest) = rest.rsplit_once("]:")?;
    let (port, fingerprint) = rest.split_once(' ')?;
    let port = port.parse().ok()?;
    let fingerprint = fingerprint.trim();
    let valid = !host.is_empty()
        && !host.contains(|c: char| c.is_whitespace() || c == ',' || c == '[' || c == ']')
        && fingerprint.len() > "SHA256:".len()
        && fingerprint.starts_with("SHA256:")
        && !fingerprint.contains(|c: char| c.is_whitespace() || c == ',');
    valid.then_some((host, port, fingerprint))
}

/// The fingerprints accepted for exactly this server.
pub fn trusted_for(trusted: &[String], host: &str, port: u16) -> Vec<String> {
    trusted
        .iter()
        .filter_map(|e| parse_trusted(e))
        .filter(|(h, p, _)| *p == port && h.eq_ignore_ascii_case(host))
        .map(|(_, _, f)| f.to_string())
        .collect()
}

#[derive(Debug, thiserror::Error)]
pub enum Error {
    #[error("el servidor SSH {host}:{port} no es conocido (huella {fingerprint})")]
    UnknownHost { host: String, port: u16, fingerprint: String },
    #[error("la clave del servidor SSH {host}:{port} no coincide con la de known_hosts: puede ser otro servidor haciéndose pasar por él. Si el servidor cambió de clave, actualizá tu known_hosts.")]
    HostKeyChanged { host: String, port: u16 },
    #[error("la clave del servidor SSH {host}:{port} cambió: ahora presenta la huella {fingerprint} y la aceptada en DBine es {accepted}. Puede ser otro servidor haciéndose pasar por él. Si el servidor cambió de clave, olvidalo en los servidores SSH verificados de la conexión (Túnel SSH) y volvé a conectarte para verificar la huella nueva.")]
    TrustedKeyChanged { host: String, port: u16, fingerprint: String, accepted: String },
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

#[derive(Clone, Debug, PartialEq)]
enum Verdict {
    /// Nothing pins this server: the user may accept the key.
    Unknown(String),
    /// known_hosts pins another key for it.
    Changed,
    /// The user accepted other keys for it (these) in DBine.
    TrustedChanged { fingerprint: String, accepted: Vec<String> },
}

impl client::Handler for Checker {
    type Error = russh::Error;

    async fn check_server_key(&mut self, key: &keys::PublicKeyOrCertificate) -> Result<bool, Self::Error> {
        let key = match key {
            keys::PublicKeyOrCertificate::PublicKey { key, .. } => key.clone(),
            keys::PublicKeyOrCertificate::Certificate(cert) => PublicKey::from(cert.public_key().clone()),
        };
        let fingerprint = key.fingerprint(HashAlg::Sha256).to_string();
        let known = keys::check_known_hosts(&self.host, self.port, &key);
        // known_hosts holds keys for this server, none of this key's type.
        let other_type = matches!(known, Ok(false)) && !known_types(&self.host, self.port).is_empty();
        match decide(known, other_type, fingerprint, &self.trusted) {
            Ok(()) => Ok(true),
            Err(verdict) => {
                *self.seen.lock().unwrap() = Some(verdict);
                Ok(false)
            }
        }
    }
}

/// known_hosts rules: a pinned key that changed (or a key of another type
/// than the pinned ones: `other_type`) is refused even if the user accepted
/// this one in DBine; only a server known_hosts doesn't pin falls back to
/// `trusted` (the fingerprints accepted for this very server). A server with
/// an accepted key that presents another one is refused as well: only one
/// nothing pins is unknown, for the user to accept.
fn decide(known: Result<bool, keys::Error>, other_type: bool, fingerprint: String, trusted: &[String]) -> Result<(), Verdict> {
    match known {
        Ok(true) => Ok(()),
        Err(keys::Error::KeyChanged { .. }) => Err(Verdict::Changed),
        Ok(false) if other_type => Err(Verdict::Changed),
        // Not in known_hosts (or no known_hosts file, or an unreadable one).
        Ok(false) | Err(_) if trusted.iter().any(|t| t == &fingerprint) => Ok(()),
        Ok(false) | Err(_) if !trusted.is_empty() => Err(Verdict::TrustedChanged { fingerprint, accepted: trusted.to_vec() }),
        Ok(false) | Err(_) => Err(Verdict::Unknown(fingerprint)),
    }
}

/// The key types known_hosts holds for this server (none if it has no file).
fn known_types(host: &str, port: u16) -> Vec<keys::Algorithm> {
    keys::known_hosts::known_host_keys(host, port).unwrap_or_default().into_iter().map(|(_, k)| k.algorithm()).collect()
}

fn same_type(a: &keys::Algorithm, b: &keys::Algorithm) -> bool {
    match (a, b) {
        // ssh-rsa, rsa-sha2-256 and rsa-sha2-512 are the same RSA key.
        (keys::Algorithm::Rsa { .. }, keys::Algorithm::Rsa { .. }) => true,
        _ => a == b,
    }
}

/// The SSH settings for one server. Like OpenSSH, it asks first for the key
/// types known_hosts holds for it, so a server pinned there with a key of a
/// type russh doesn't prefer presents that one (and isn't refused for
/// presenting another).
fn config(known: &[keys::Algorithm]) -> Arc<client::Config> {
    let mut preferred = russh::Preferred::default();
    if !known.is_empty() {
        let (mut first, rest): (Vec<_>, Vec<_>) = preferred.key.iter().cloned().partition(|a| known.iter().any(|k| same_type(a, k)));
        first.extend(rest);
        preferred.key = first.into();
    }
    Arc::new(client::Config {
        // Keep NATs and firewalls from dropping an idle tunnel.
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 3,
        inactivity_timeout: None,
        nodelay: true,
        preferred,
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
        let trusted = trusted_for(&spec.trusted, &hop.host, hop.port);
        let checker = Checker { host: hop.host.clone(), port: hop.port, trusted, seen: seen.clone() };
        let config = config(&known_types(&hop.host, hop.port));
        let connecting = match hops.last() {
            None => tokio::time::timeout(Duration::from_secs(20), client::connect(config, (first.host.as_str(), first.port), checker)).await,
            Some(prev) => {
                // The next hop, reached from inside the previous one.
                let channel = prev
                    .channel_open_direct_tcpip(hop.host.clone(), hop.port.into(), "127.0.0.1", 0)
                    .await
                    .map_err(|e| Error::Connect { host: hop.host.clone(), port: hop.port, message: format!("desde {}: {e}", spec.hops[i - 1].host) })?;
                tokio::time::timeout(Duration::from_secs(20), client::connect_stream(config, channel.into_stream(), checker)).await
            }
        };
        let mut handle = match connecting {
            Err(_) => return Err(Error::Connect { host: hop.host.clone(), port: hop.port, message: "no respondió en 20 s".into() }),
            Ok(Err(e)) => {
                return Err(match seen.lock().unwrap().clone() {
                    Some(Verdict::Unknown(fingerprint)) => Error::UnknownHost { host: hop.host.clone(), port: hop.port, fingerprint },
                    Some(Verdict::Changed) => Error::HostKeyChanged { host: hop.host.clone(), port: hop.port },
                    Some(Verdict::TrustedChanged { fingerprint, accepted }) => {
                        Error::TrustedKeyChanged { host: hop.host.clone(), port: hop.port, fingerprint, accepted: accepted.join(", ") }
                    }
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
    let local: SocketAddr = listener.local_addr().map_err(Error::Listen)?;
    let local_port = local.port();
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
                // Only the user's own processes (DBine, its driver hosts) may
                // use the user's SSH session.
                if !tokio::task::spawn_blocking(move || peer::same_user(peer, local)).await.unwrap_or(false) {
                    tracing::warn!(%peer, "SSH tunnel: refused a local connection that isn't from this user's processes");
                    return;
                }
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

#[cfg(test)]
mod tests {
    use super::*;

    const FP_J: &str = "SHA256:jumpjumpjumpjumpjumpjumpjumpjumpjumpjumpjum";
    const FP_H: &str = "SHA256:hosthosthosthosthosthosthosthosthosthosthos";

    #[test]
    fn trusted_entries_carry_their_server() {
        let e = trusted_entry("bastion.corp", 2200, FP_J);
        assert_eq!(e, format!("[bastion.corp]:2200 {FP_J}"));
        assert_eq!(parse_trusted(&e), Some(("bastion.corp", 2200, FP_J)));
        assert_eq!(parse_trusted(&format!("[::1]:22 {FP_H}")), Some(("::1", 22, FP_H)));
        // Older versions saved bare fingerprints: no server, not honored.
        assert_eq!(parse_trusted(FP_H), None);
        assert_eq!(parse_trusted(&format!("bastion:22 {FP_J}")), None);
        assert_eq!(parse_trusted(&format!("[]:22 {FP_J}")), None);
        assert_eq!(parse_trusted(&format!("[h]:x {FP_J}")), None);
        assert_eq!(parse_trusted("[h]:22 MD5:aa"), None);
        assert_eq!(parse_trusted("[h]:22 SHA256:"), None);
    }

    #[test]
    fn a_key_trusted_for_one_hop_doesnt_pass_another() {
        let trusted = vec![trusted_entry("jump", 22, FP_J), trusted_entry("db-ssh", 2222, FP_H), FP_J.to_string()];
        assert_eq!(trusted_for(&trusted, "jump", 22), vec![FP_J.to_string()]);
        assert_eq!(trusted_for(&trusted, "JUMP", 22), vec![FP_J.to_string()]);
        assert_eq!(trusted_for(&trusted, "db-ssh", 2222), vec![FP_H.to_string()]);
        // Same host, other port; or a hop with no entry of its own: the
        // bare legacy fingerprint doesn't count either.
        assert!(trusted_for(&trusted, "db-ssh", 22).is_empty());
        assert!(trusted_for(&trusted, "other", 22).is_empty());
    }

    #[test]
    fn known_hosts_wins_over_trusted() {
        let trusted = vec![FP_H.to_string()];
        let changed = || Err(keys::Error::KeyChanged { line: 3 });
        assert_eq!(decide(Ok(true), false, FP_J.into(), &[]), Ok(()));
        // A key that differs from the pinned one is refused even if accepted in DBine.
        assert_eq!(decide(changed(), false, FP_H.into(), &trusted), Err(Verdict::Changed));
        // known_hosts pins the server with keys of another type only.
        assert_eq!(decide(Ok(false), true, FP_H.into(), &trusted), Err(Verdict::Changed));
        assert_eq!(decide(Ok(false), true, FP_H.into(), &[]), Err(Verdict::Changed));
        assert_eq!(decide(Ok(false), false, FP_H.into(), &trusted), Ok(()));
        assert_eq!(decide(Err(keys::Error::CouldNotReadKey), false, FP_H.into(), &trusted), Ok(()));
    }

    #[test]
    fn a_key_accepted_for_the_server_is_never_swapped_by_asking() {
        let pinned = vec![FP_H.to_string()];
        // Accepted before and presented again: connects.
        assert_eq!(decide(Ok(false), false, FP_H.into(), &pinned), Ok(()));
        // Another key for a server with one accepted: refused, not asked.
        assert_eq!(
            decide(Ok(false), false, FP_J.into(), &pinned),
            Err(Verdict::TrustedChanged { fingerprint: FP_J.into(), accepted: pinned.clone() })
        );
        assert_eq!(
            decide(Err(keys::Error::CouldNotReadKey), false, FP_J.into(), &pinned),
            Err(Verdict::TrustedChanged { fingerprint: FP_J.into(), accepted: pinned.clone() })
        );
        // Nothing accepted for it: the user is asked.
        assert_eq!(decide(Ok(false), false, FP_J.into(), &[]), Err(Verdict::Unknown(FP_J.into())));
    }

    #[test]
    fn rsa_hashes_are_one_key_type() {
        use keys::Algorithm;
        assert!(same_type(&Algorithm::Rsa { hash: None }, &Algorithm::Rsa { hash: Some(keys::HashAlg::Sha512) }));
        assert!(!same_type(&Algorithm::Ed25519, &Algorithm::Rsa { hash: None }));
        // The known types go first, the rest keep their order after them.
        let key = &config(&[Algorithm::Rsa { hash: None }]).preferred.key;
        assert!(matches!(key[0], Algorithm::Rsa { .. }));
        assert!(key.contains(&Algorithm::Ed25519));
        assert_eq!(key.len(), russh::Preferred::default().key.len());
    }
}
