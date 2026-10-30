//! SSH tunnels of connections (docs/ssh-tunnels.md). A connection with
//! `ssh.enabled` reaches its server through a local port forwarded over SSH
//! (crates/dbine-tunnel): the driver gets 127.0.0.1 and that port instead of
//! the server, so every engine that connects over the network works through
//! a tunnel without knowing about it.
//!
//! The settings live in the connection's options (`ssh.*`); the password and
//! the key's passphrase are secrets (keychain). Each saved connection keeps
//! one tunnel for all its sessions, opened on first use and again if the
//! SSH session dropped.

use crate::error::{CommandError, CommandResult};
use crate::state::driver_info;
use dashmap::DashMap;
use dbine_driver::ConnectionConfig;
use dbine_tunnel::{Auth, Hop, Spec, Tunnel};
use std::sync::Arc;

/// Secret keys of the tunnel (kept in the keychain like the driver's own).
pub const SECRETS: [&str; 2] = ["ssh.password", "ssh.passphrase"];

pub fn enabled(cfg: &ConnectionConfig) -> bool {
    cfg.option("ssh.enabled") == Some("true")
}

#[derive(Default)]
pub struct Tunnels {
    open: DashMap<String, (Spec, Arc<Tunnel>)>,
    /// One opening at a time per connection: sessions opened together share it.
    opening: DashMap<String, Arc<tokio::sync::Mutex<()>>>,
}

impl Tunnels {
    /// Route `cfg` through its tunnel, if it has one: the tunnel is opened
    /// (or reused, for a saved connection: `connection_id`) and the config
    /// rewritten to the local end. The `ssh.*` options are taken out (the
    /// driver doesn't need them). The returned tunnel must be kept alive
    /// while the session is used when there's no `connection_id` (a test).
    pub async fn route(
        &self,
        connection_id: Option<&str>,
        cfg: &mut ConnectionConfig,
    ) -> CommandResult<Option<Arc<Tunnel>>> {
        if !enabled(cfg) {
            strip(cfg);
            return Ok(None);
        }
        let (spec, target) = spec(cfg)?;
        strip(cfg);
        mark_origin(cfg, &spec.target_host, spec.target_port);
        let tunnel = match connection_id {
            None => Arc::new(dbine_tunnel::open(&spec).await.map_err(error)?),
            Some(id) => {
                let lock = self.opening.entry(id.to_string()).or_default().clone();
                let _guard = lock.lock().await;
                match self
                    .open
                    .get(id)
                    .filter(|e| e.0 == spec && e.1.is_alive())
                    .map(|e| e.1.clone())
                {
                    Some(t) => t,
                    None => {
                        let t = Arc::new(dbine_tunnel::open(&spec).await.map_err(error)?);
                        tracing::info!(connection = id, port = t.local_port(), "SSH tunnel open");
                        self.open.insert(id.to_string(), (spec, t.clone()));
                        t
                    }
                }
            }
        };
        target.rewrite(cfg, tunnel.local_port());
        Ok(Some(tunnel))
    }

    /// Close a connection's tunnel (disconnect, edit, delete).
    pub fn close(&self, connection_id: &str) {
        self.open.remove(connection_id);
    }
}

/// Takes out the tunnel's settings, and any `tunnel.*` option a saved
/// connection may carry: only a tunnel opened here sets them, so a direct
/// connection never verifies its certificate against another name.
fn strip(cfg: &mut ConnectionConfig) {
    cfg.options
        .retain(|k, _| !k.starts_with("ssh.") && !k.starts_with("tunnel."));
}

/// Where the tunnel goes, on every tunnelled connection: the driver dials
/// the local end, but the server's certificate is verified by the real
/// server's name (`ConnectionConfig::tls_server_name`), and a cloud
/// sign-in's token is signed for the real endpoint (the RDS IAM token names
/// host and port).
fn mark_origin(cfg: &mut ConnectionConfig, host: &str, port: u16) {
    cfg.options
        .insert(dbine_driver::TUNNEL_ORIGIN_HOST.into(), host.to_string());
    cfg.options
        .insert(dbine_driver::TUNNEL_ORIGIN_PORT.into(), port.to_string());
}

fn error(e: dbine_tunnel::Error) -> CommandError {
    match e {
        dbine_tunnel::Error::UnknownHost {
            host,
            port,
            fingerprint,
        } => CommandError::SshUnknownHost {
            host,
            port,
            fingerprint,
        },
        // HostKeyChanged and TrustedKeyChanged too: refused, never asked.
        e => CommandError::Connect(e.to_string()),
    }
}

/// Where the server is, as the connection says it: `host`, `host:port`,
/// SQL Server's `host,port` or a URL; the port field or the driver's
/// default otherwise.
struct Target {
    /// The host text before and after the server part (a URL's scheme and path).
    prefix: String,
    suffix: String,
    port_in_host: bool,
}

impl Target {
    fn rewrite(&self, cfg: &mut ConnectionConfig, local_port: u16) {
        if self.port_in_host {
            cfg.host = format!("{}127.0.0.1:{local_port}{}", self.prefix, self.suffix);
        } else {
            cfg.host = format!("{}127.0.0.1{}", self.prefix, self.suffix);
            cfg.port = local_port;
        }
    }
}

fn spec(cfg: &ConnectionConfig) -> CommandResult<(Spec, Target)> {
    let info = driver_info(&cfg.driver)?;
    let bad = |m: &str| CommandError::BadRequest(format!("túnel SSH: {m}"));
    let (target_host, target_port, target) = server(cfg, info.default_port).map_err(|m| bad(&m))?;

    let hops = hops(cfg)?;
    let auth = match cfg.option("ssh.auth").unwrap_or("password") {
        "agent" => Auth::Agent,
        "key" => {
            let path = cfg
                .option("ssh.key_path")
                .ok_or_else(|| bad("falta el archivo de la clave privada"))?;
            Auth::Key {
                path: expand_home(path),
                passphrase: cfg.option("ssh.passphrase").map(String::from),
            }
        }
        _ => Auth::Password(cfg.option("ssh.password").unwrap_or("").to_string()),
    };
    Ok((
        Spec {
            hops,
            auth,
            target_host,
            target_port,
            trusted: trusted(cfg),
        },
        target,
    ))
}

/// The SSH servers of a connection's tunnel, in order: the jump hosts, then
/// the one that reaches the database.
pub fn hops(cfg: &ConnectionConfig) -> CommandResult<Vec<Hop>> {
    let bad = |m: &str| CommandError::BadRequest(format!("túnel SSH: {m}"));
    let user = cfg
        .option("ssh.user")
        .ok_or_else(|| bad("falta el usuario SSH"))?
        .to_string();
    let host = cfg
        .option("ssh.host")
        .ok_or_else(|| bad("falta el servidor SSH"))?
        .to_string();
    let port = match cfg.option("ssh.port") {
        Some(p) => p
            .parse()
            .map_err(|_| bad("el puerto SSH no es un número"))?,
        None => 22,
    };
    let mut hops: Vec<Hop> = cfg
        .option("ssh.jump")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(|s| {
            parse_hop(s, &user).ok_or_else(|| {
                bad(&format!(
                    "no se entiende el bastión «{s}» (usuario@servidor:puerto)"
                ))
            })
        })
        .collect::<CommandResult<_>>()?;
    hops.push(Hop { host, port, user });
    Ok(hops)
}

/// `ssh.trusted`: the accepted server keys, `[host]:port SHA256:…` each
/// (dbine_tunnel::trusted_entry), comma-separated.
pub fn trusted(cfg: &ConnectionConfig) -> Vec<String> {
    cfg.option("ssh.trusted")
        .unwrap_or("")
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect()
}

/// `trusted` with `entry` added, dropping the bare fingerprints older
/// versions saved (no server matches them any more). None if another key is
/// already accepted for the same server: a changed key is never swapped in
/// by accepting it, the user forgets the old one first (in the connection's
/// form), knowingly.
pub fn add_trusted(mut trusted: Vec<String>, entry: &str) -> Option<Vec<String>> {
    let (host, port, fingerprint) = dbine_tunnel::parse_trusted(entry)?;
    let accepted = dbine_tunnel::trusted_for(&trusted, host, port);
    if accepted.iter().any(|f| f != fingerprint) {
        return None;
    }
    trusted.retain(|t| dbine_tunnel::parse_trusted(t).is_some());
    if accepted.is_empty() {
        trusted.push(entry.trim().to_string());
    }
    Some(trusted)
}

/// Trust `entry` (`[host]:port SHA256:…`, from an `ssh_unknown_host` error)
/// in a connection's `ssh.trusted`: the server has to be one of its tunnel's
/// and have no other key accepted (see [`add_trusted`]).
pub fn trust(cfg: &mut ConnectionConfig, entry: &str) -> CommandResult<()> {
    let invalid = || CommandError::BadRequest("huella SSH inválida".into());
    let (host, port, _) = dbine_tunnel::parse_trusted(entry).ok_or_else(invalid)?;
    if !hops(cfg)?
        .iter()
        .any(|h| h.port == port && h.host.eq_ignore_ascii_case(host))
    {
        return Err(invalid());
    }
    let trusted = add_trusted(trusted(cfg), entry).ok_or_else(|| {
        CommandError::BadRequest(format!(
            "el servidor SSH {host}:{port} ya tiene otra clave aceptada: para aceptar una nueva, olvidá la anterior en los servidores SSH verificados de la conexión (Túnel SSH)"
        ))
    })?;
    cfg.options.insert("ssh.trusted".into(), trusted.join(","));
    Ok(())
}

/// `user@host:port`, `host:port` or `host` (the tunnel's user, port 22).
fn parse_hop(s: &str, default_user: &str) -> Option<Hop> {
    let (user, rest) = match s.split_once('@') {
        Some((u, r)) => (u.to_string(), r),
        None => (default_user.to_string(), s),
    };
    let (host, port) = match rest.rsplit_once(':') {
        Some((h, p)) => (h.to_string(), p.parse().ok()?),
        None => (rest.to_string(), 22),
    };
    (!host.is_empty() && !user.is_empty()).then_some(Hop { host, port, user })
}

fn server(cfg: &ConnectionConfig, default_port: u16) -> Result<(String, u16, Target), String> {
    let host = cfg.host.trim();
    let port_or = |p: Option<u16>| {
        p.filter(|p| *p != 0)
            .or((cfg.port != 0).then_some(cfg.port))
            .or((default_port != 0).then_some(default_port))
    };
    let need_port = || "indicá el puerto del servidor de la base".to_string();
    if host.is_empty() {
        return Err("falta el servidor de la base".into());
    }
    // A URL: scheme://[user@]host[:port][/…]
    if let Some(i) = host.find("://") {
        let start = i + 3;
        let rest = &host[start..];
        let end = rest.find(['/', '?', '#']).unwrap_or(rest.len());
        let authority = &rest[..end];
        let (userinfo, hostport) = match authority.rsplit_once('@') {
            Some((u, h)) => (format!("{u}@"), h),
            None => (String::new(), authority),
        };
        let (h, p) = match hostport.rsplit_once(':') {
            Some((h, p)) if p.chars().all(|c| c.is_ascii_digit()) && !p.is_empty() => {
                (h, p.parse().ok())
            }
            _ => (hostport, None),
        };
        let default = if host[..i].eq_ignore_ascii_case("https") {
            443
        } else if host[..i].eq_ignore_ascii_case("http") {
            80
        } else {
            0
        };
        // A URL without a port means its scheme's (what the driver would use).
        let port = p
            .or((default != 0).then_some(default))
            .or_else(|| port_or(None))
            .ok_or_else(need_port)?;
        let target = Target {
            prefix: format!("{}{userinfo}", &host[..start]),
            suffix: rest[end..].to_string(),
            port_in_host: true,
        };
        return Ok((h.to_string(), port, target));
    }
    // host,port (SQL Server) or host:port
    for sep in [',', ':'] {
        if let Some((h, p)) = host.rsplit_once(sep) {
            if !h.contains(':') && !p.is_empty() && p.chars().all(|c| c.is_ascii_digit()) {
                let port = p.parse().map_err(|_| need_port())?;
                return Ok((
                    h.to_string(),
                    port,
                    Target {
                        prefix: String::new(),
                        suffix: String::new(),
                        port_in_host: false,
                    },
                ));
            }
        }
    }
    let port = port_or(None).ok_or_else(need_port)?;
    Ok((
        host.to_string(),
        port,
        Target {
            prefix: String::new(),
            suffix: String::new(),
            port_in_host: false,
        },
    ))
}

fn expand_home(path: &str) -> std::path::PathBuf {
    match path.strip_prefix("~/").or_else(|| path.strip_prefix("~\\")) {
        Some(rest) => std::env::var_os("HOME")
            .or_else(|| std::env::var_os("USERPROFILE"))
            .map(std::path::PathBuf::from)
            .unwrap_or_default()
            .join(rest),
        None => path.into(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cfg(host: &str, port: u16) -> ConnectionConfig {
        ConnectionConfig {
            driver: "postgres".into(),
            host: host.into(),
            port,
            ..Default::default()
        }
    }

    #[test]
    fn finds_the_server_in_every_host_form() {
        let t = |host: &str, port: u16| {
            let c = cfg(host, port);
            let (h, p, target) = server(&c, 5432).unwrap();
            let mut c2 = c.clone();
            target.rewrite(&mut c2, 40000);
            (h, p, c2.host, c2.port)
        };
        assert_eq!(
            t("db.interno", 0),
            ("db.interno".into(), 5432, "127.0.0.1".into(), 40000)
        );
        assert_eq!(
            t("db.interno", 6543),
            ("db.interno".into(), 6543, "127.0.0.1".into(), 40000)
        );
        assert_eq!(
            t("sql01,1444", 0),
            ("sql01".into(), 1444, "127.0.0.1".into(), 40000)
        );
        assert_eq!(
            t("db:7000", 0),
            ("db".into(), 7000, "127.0.0.1".into(), 40000)
        );
        assert_eq!(
            t("https://es.interno:9200/base?x=1", 0),
            (
                "es.interno".into(),
                9200,
                "https://127.0.0.1:40000/base?x=1".into(),
                0
            )
        );
        assert_eq!(
            t("http://u:p@api.local/v1", 0),
            (
                "api.local".into(),
                80,
                "http://u:p@127.0.0.1:40000/v1".into(),
                0
            )
        );
    }

    #[test]
    fn a_tunnelled_connection_verifies_the_real_servers_name() {
        let mut c = cfg("db.interno", 6543);
        c.options.insert("ssh.enabled".into(), "true".into());
        // A stale or hand-written origin never survives the routing.
        c.options
            .insert(dbine_driver::TUNNEL_ORIGIN_HOST.into(), "evil".into());
        let (host, port, target) = server(&c, 5432).unwrap();
        strip(&mut c);
        assert!(c.options.is_empty());
        mark_origin(&mut c, &host, port);
        target.rewrite(&mut c, 40000);
        assert_eq!((c.host.as_str(), c.port), ("127.0.0.1", 40000));
        assert_eq!(c.tls_server_name(), "db.interno");
        assert_eq!(c.option(dbine_driver::TUNNEL_ORIGIN_PORT), Some("6543"));
        // Without a tunnel: the host as typed, whatever the options said.
        let mut d = cfg("db.interno", 0);
        d.options
            .insert(dbine_driver::TUNNEL_ORIGIN_HOST.into(), "evil".into());
        strip(&mut d);
        assert_eq!(d.tls_server_name(), "db.interno");
    }

    fn fp(c: char) -> String {
        format!("SHA256:{}", c.to_string().repeat(43))
    }

    #[test]
    fn trusting_a_key_never_replaces_the_servers_accepted_one() {
        let old = vec![
            dbine_tunnel::trusted_entry("jump", 22, &fp('a')),
            dbine_tunnel::trusted_entry("db", 22, &fp('b')),
            fp('c'),
        ];
        // Another key for a server that has one: refused.
        assert_eq!(
            add_trusted(
                old.clone(),
                &dbine_tunnel::trusted_entry("DB", 22, &fp('d'))
            ),
            None
        );
        // The same key again: kept once (the legacy bare one dropped).
        assert_eq!(
            add_trusted(
                old.clone(),
                &dbine_tunnel::trusted_entry("db", 22, &fp('b'))
            ),
            Some(old[..2].to_vec())
        );
        // A server with nothing accepted (same host, other port).
        let new = dbine_tunnel::trusted_entry("db", 2222, &fp('d'));
        assert_eq!(
            add_trusted(old.clone(), &new),
            Some(vec![old[0].clone(), old[1].clone(), new])
        );
        // Not an entry bound to a server.
        assert_eq!(add_trusted(old.clone(), &fp('d')), None);
    }

    #[test]
    fn trust_ssh_host_refuses_to_replace_a_pin() {
        let mut c = cfg("db.interno", 5432);
        for (k, v) in [
            ("ssh.enabled", "true"),
            ("ssh.host", "bastion"),
            ("ssh.user", "ops"),
            ("ssh.jump", "jump:2200"),
        ] {
            c.options.insert(k.into(), v.into());
        }
        c.options.insert(
            "ssh.trusted".into(),
            dbine_tunnel::trusted_entry("bastion", 22, &fp('a')),
        );
        let before = c.options.clone();
        // A changed key for the pinned server: refused, the pin untouched.
        assert!(matches!(
            trust(
                &mut c,
                &dbine_tunnel::trusted_entry("bastion", 22, &fp('b'))
            ),
            Err(CommandError::BadRequest(_))
        ));
        assert_eq!(c.options, before);
        // A server that isn't one of the tunnel's, or a bare fingerprint.
        assert!(trust(
            &mut c,
            &dbine_tunnel::trusted_entry("elsewhere", 22, &fp('b'))
        )
        .is_err());
        assert!(trust(&mut c, &fp('b')).is_err());
        assert_eq!(c.options, before);
        // The jump host has nothing accepted: trusted.
        trust(&mut c, &dbine_tunnel::trusted_entry("jump", 2200, &fp('b'))).unwrap();
        assert_eq!(
            trusted(&c),
            vec![
                dbine_tunnel::trusted_entry("bastion", 22, &fp('a')),
                dbine_tunnel::trusted_entry("jump", 2200, &fp('b'))
            ]
        );
        // Forgotten in the form (the pin removed and saved): asked and trusted again.
        c.options.insert(
            "ssh.trusted".into(),
            dbine_tunnel::trusted_entry("jump", 2200, &fp('b')),
        );
        trust(
            &mut c,
            &dbine_tunnel::trusted_entry("bastion", 22, &fp('c')),
        )
        .unwrap();
        assert!(trusted(&c).contains(&dbine_tunnel::trusted_entry("bastion", 22, &fp('c'))));
    }

    /// A connection through an SSH profile: the profile's servers, user and
    /// auth, the connection's own database server as the target.
    #[test]
    fn a_profiles_tunnel_goes_to_the_connections_server() {
        use dbine_core::ssh_profiles::{self, SecretStore, SshProfile};
        struct NoKeychain;
        impl SecretStore for NoKeychain {
            fn get(&self, _: &str) -> dbine_core::Result<dbine_core::secrets::Secrets> {
                Ok(Default::default())
            }
            fn set(&self, _: &str, _: &dbine_core::secrets::Secrets) -> dbine_core::Result<()> {
                Ok(())
            }
        }
        let store = dbine_core::StateStore::open_in_memory().unwrap();
        let p = ssh_profiles::save(
            &store,
            &NoKeychain,
            SshProfile {
                id: String::new(),
                name: "prod".into(),
                options: [
                    ("ssh.host", "bastion"),
                    ("ssh.user", "ops"),
                    ("ssh.port", "2222"),
                    ("ssh.auth", "agent"),
                    ("ssh.jump", "edge:22"),
                ]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
                updated_at: String::new(),
            },
            None,
        )
        .unwrap();
        let spec_of = |host: &str| {
            let mut c = cfg(host, 6543);
            c.options.insert("ssh.enabled".into(), "true".into());
            c.options.insert("ssh.profile".into(), p.id.clone());
            // The connection's own leftovers never win over the profile.
            c.options.insert("ssh.host".into(), "stale".into());
            store.resolve_ssh_profile(&mut c).unwrap();
            spec(&c).unwrap().0
        };
        let a = spec_of("db-a.internal");
        let b = spec_of("db-b.internal");
        let hops = vec![
            Hop {
                host: "edge".into(),
                port: 22,
                user: "ops".into(),
            },
            Hop {
                host: "bastion".into(),
                port: 2222,
                user: "ops".into(),
            },
        ];
        assert_eq!(a.hops, hops);
        assert_eq!(b.hops, hops);
        assert!(matches!(a.auth, Auth::Agent));
        assert_eq!(
            (a.target_host.as_str(), a.target_port),
            ("db-a.internal", 6543)
        );
        assert_eq!(
            (b.target_host.as_str(), b.target_port),
            ("db-b.internal", 6543)
        );
    }

    #[test]
    fn reads_bastions() {
        assert_eq!(
            parse_hop("ops@bastion.corp:2200", "yo"),
            Some(Hop {
                host: "bastion.corp".into(),
                port: 2200,
                user: "ops".into()
            })
        );
        assert_eq!(
            parse_hop("bastion", "yo"),
            Some(Hop {
                host: "bastion".into(),
                port: 22,
                user: "yo".into()
            })
        );
        assert_eq!(parse_hop("x:puerto", "yo"), None);
    }
}
