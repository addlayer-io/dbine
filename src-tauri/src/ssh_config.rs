//! SSH tunnels by a `Host` of `~/.ssh/config` (docs/ssh-tunnels.md). OpenSSH
//! itself resolves the alias (`ssh -G`, which connects to nothing), so
//! `Include`, `Match` and its precedence rules are its own and not a copy of
//! them here. What's used from its answer: the server, the user, the keys and
//! the agent, and how to reach the server (`ProxyJump`). Host keys stay
//! DBine's: a `StrictHostKeyChecking no` doesn't turn the verification off.
//!
//! The alias is resolved when a tunnel opens, so a changed file applies on the
//! next connect, and it's text a connection may bring from another machine
//! (sync, import): it's checked before it reaches a command line.

use crate::error::{CommandError, CommandResult};
use dbine_tunnel::{AgentSocket, Auth, Hop, Identities};
use std::collections::BTreeSet;
use std::future::Future;
use std::path::{Path, PathBuf};
use std::pin::Pin;
use std::process::Stdio;
use std::time::Duration;

/// The option with the alias: the tunnel's servers, user and keys come from it.
pub const ALIAS: &str = "ssh.alias";
/// A longer chain is a mistake in the file, as much as a loop is.
const MAX_HOPS: usize = 8;
/// `ssh -G` connects to nothing, but a `Match exec` of the file runs here.
const RESOLVE_TIMEOUT: Duration = Duration::from_secs(10);

pub fn alias(cfg: &dbine_driver::ConnectionConfig) -> Option<&str> {
    cfg.option(ALIAS).map(str::trim).filter(|a| !a.is_empty())
}

fn bad(message: String) -> CommandError {
    CommandError::Connect(format!("túnel SSH: {message}"))
}

/// What can go on `ssh`'s command line as a host: `-oProxyCommand=…` as an
/// "alias" would run a command, and OpenSSH takes the rest of the text as is.
pub fn valid_host(host: &str) -> bool {
    !host.is_empty()
        && host.len() <= 255
        && !host.starts_with('-')
        && host
            .chars()
            .all(|c| c.is_alphanumeric() || matches!(c, '.' | '_' | '-' | ':' | '+'))
}

/// A server to ask `ssh -G` about: an alias, or an entry of a `ProxyJump`.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Target {
    user: Option<String>,
    host: String,
    port: Option<u16>,
}

/// `ProxyJump`'s value: `[user@]host[:port]` or `ssh://[user@]host[:port]`,
/// comma-separated; `none` is no jump.
fn parse_jumps(value: &str) -> Result<Vec<Target>, String> {
    let value = value.trim();
    if value.is_empty() || value.eq_ignore_ascii_case("none") {
        return Ok(Vec::new());
    }
    value
        .split(',')
        .map(|entry| {
            let unreadable = || format!("no se entiende el ProxyJump «{}»", entry.trim());
            let rest = entry.trim();
            let rest = rest.strip_prefix("ssh://").unwrap_or(rest);
            let (user, rest) = match rest.rsplit_once('@') {
                Some((u, r)) => (Some(u.to_string()), r),
                None => (None, rest),
            };
            let (host, port) = if let Some(v6) = rest.strip_prefix('[') {
                let (h, tail) = v6.split_once(']').ok_or_else(unreadable)?;
                let port = match tail.strip_prefix(':') {
                    Some(p) => Some(p.parse().map_err(|_| unreadable())?),
                    None if tail.is_empty() => None,
                    None => return Err(unreadable()),
                };
                (h, port)
            } else {
                match rest.split_once(':') {
                    Some((h, p)) => (h, Some(p.parse().map_err(|_| unreadable())?)),
                    None => (rest, None),
                }
            };
            let user_ok = user.as_deref().is_none_or(|u| {
                !u.is_empty() && !u.starts_with('-') && !u.contains(char::is_whitespace)
            });
            if !valid_host(host) || !user_ok {
                return Err(unreadable());
            }
            Ok(Target {
                user,
                host: host.to_string(),
                port,
            })
        })
        .collect()
}

/// `ssh -G`'s answer for one server.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Resolved {
    /// The name it was asked by, for `%n` and for the messages.
    pub name: String,
    pub hostname: String,
    pub port: u16,
    pub user: String,
    pub identity_files: Vec<String>,
    pub identities_only: bool,
    pub identity_agent: Option<String>,
    pub proxy_jump: Option<String>,
    pub proxy_command: Option<String>,
}

fn unquote(value: &str) -> &str {
    let value = value.trim();
    value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value)
}

fn parse_resolved(name: &str, output: &str) -> Resolved {
    let mut r = Resolved {
        name: name.to_string(),
        port: 22,
        ..Default::default()
    };
    for line in output.lines() {
        let Some((key, value)) = line.split_once(' ') else {
            continue;
        };
        let value = unquote(value);
        match key.to_ascii_lowercase().as_str() {
            "hostname" => r.hostname = value.to_string(),
            "user" => r.user = value.to_string(),
            "port" => r.port = value.parse().unwrap_or(22),
            "identityfile" if !value.eq_ignore_ascii_case("none") => {
                r.identity_files.push(value.to_string())
            }
            "identitiesonly" => r.identities_only = value.eq_ignore_ascii_case("yes"),
            "identityagent" => r.identity_agent = Some(value.to_string()),
            "proxyjump" if !value.eq_ignore_ascii_case("none") => {
                r.proxy_jump = Some(value.to_string())
            }
            "proxycommand" if !value.eq_ignore_ascii_case("none") => {
                r.proxy_command = Some(value.to_string())
            }
            _ => {}
        }
    }
    r
}

/// The file to read instead of `~/.ssh/config`: for tests of a debug build,
/// since OpenSSH takes the home folder from the system and not from `HOME`.
fn config_file() -> Option<PathBuf> {
    if cfg!(debug_assertions) {
        std::env::var_os("DBINE_SSH_CONFIG").map(PathBuf::from)
    } else {
        None
    }
}

async fn ask_ssh(target: Target) -> CommandResult<Resolved> {
    let mut c = tokio::process::Command::new("ssh");
    c.arg("-G");
    if let Some(file) = config_file() {
        c.arg("-F").arg(file);
    }
    if let Some(user) = &target.user {
        c.arg("-l").arg(user);
    }
    if let Some(port) = target.port {
        c.arg("-p").arg(port.to_string());
    }
    c.arg("--").arg(&target.host);
    c.stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .kill_on_drop(true);
    #[cfg(windows)]
    {
        // CREATE_NO_WINDOW: no console window next to the app.
        c.creation_flags(0x0800_0000);
    }
    let out = tokio::time::timeout(RESOLVE_TIMEOUT, c.output())
        .await
        .map_err(|_| {
            bad(format!(
                "ssh no terminó de leer la configuración de «{}» en {} s",
                target.host,
                RESOLVE_TIMEOUT.as_secs()
            ))
        })?
        .map_err(|e| match e.kind() {
            std::io::ErrorKind::NotFound => {
                bad("no se encontró el programa ssh, que hace falta para leer ~/.ssh/config: instalá OpenSSH o cargá el servidor a mano".into())
            }
            _ => bad(format!("no se pudo ejecutar ssh: {e}")),
        })?;
    if !out.status.success() {
        let said = String::from_utf8_lossy(&out.stderr);
        let said: String = said.trim().chars().take(500).collect();
        return Err(bad(format!(
            "ssh no pudo leer la configuración de «{}»: {said}",
            target.host
        )));
    }
    Ok(parse_resolved(
        &target.host,
        &String::from_utf8_lossy(&out.stdout),
    ))
}

type Asked = Pin<Box<dyn Future<Output = CommandResult<Resolved>> + Send>>;

/// The servers of an alias, in the order they're reached. As `ssh -J a,b`
/// does: the target goes through `b`, `b` through `a` whatever `b`'s own
/// config says, and only `a` follows its own `ProxyJump`.
async fn chain(alias: &str, mut ask: impl FnMut(Target) -> Asked) -> CommandResult<Vec<Resolved>> {
    let mut next = Target {
        user: None,
        host: alias.to_string(),
        port: None,
    };
    let mut forced: Option<Vec<Target>> = None;
    let mut seen: Vec<Target> = Vec::new();
    let mut backwards: Vec<Resolved> = Vec::new();
    loop {
        if seen.contains(&next) {
            let path: Vec<&str> = seen.iter().map(|t| t.host.as_str()).collect();
            return Err(bad(format!(
                "los saltos de «{alias}» forman un bucle: {} → {}",
                path.join(" → "),
                next.host
            )));
        }
        if seen.len() == MAX_HOPS {
            return Err(bad(format!(
                "«{alias}» pasa por más de {MAX_HOPS} servidores"
            )));
        }
        seen.push(next.clone());
        let resolved = ask(next.clone()).await?;
        let jumps = match forced.take() {
            Some(jumps) => jumps,
            None => {
                if resolved.proxy_jump.is_none() && resolved.proxy_command.is_some() {
                    return Err(bad(format!(
                        "«{}» usa ProxyCommand, que esta versión de DBine todavía no ejecuta",
                        resolved.name
                    )));
                }
                parse_jumps(resolved.proxy_jump.as_deref().unwrap_or("")).map_err(bad)?
            }
        };
        backwards.push(resolved);
        let Some((last, rest)) = jumps.split_last() else {
            break;
        };
        next = last.clone();
        forced = (!rest.is_empty()).then(|| rest.to_vec());
    }
    backwards.reverse();
    Ok(backwards)
}

/// `%h`, `%p`… of a path, which `ssh -G` leaves as written. An unknown token
/// stays: that file won't exist, and files that don't exist are skipped.
fn expand_tokens(path: &str, r: &Resolved) -> String {
    let home = crate::tunnels::expand_home("~/");
    let mut out = String::new();
    let mut chars = path.chars();
    while let Some(c) = chars.next() {
        if c != '%' {
            out.push(c);
            continue;
        }
        match chars.next() {
            Some('%') => out.push('%'),
            Some('d') => out.push_str(home.to_string_lossy().trim_end_matches(['/', '\\'])),
            Some('h') => out.push_str(&r.hostname),
            Some('n') => out.push_str(&r.name),
            Some('p') => out.push_str(&r.port.to_string()),
            Some('r') => out.push_str(&r.user),
            Some('u') => out.push_str(
                &std::env::var("USER")
                    .or_else(|_| std::env::var("USERNAME"))
                    .unwrap_or_default(),
            ),
            Some(other) => {
                out.push('%');
                out.push(other);
            }
            None => out.push('%'),
        }
    }
    out
}

fn agent_socket(r: &Resolved) -> AgentSocket {
    match r.identity_agent.as_deref() {
        None | Some("SSH_AUTH_SOCK") => AgentSocket::Default,
        Some(v) if v.eq_ignore_ascii_case("none") => AgentSocket::Off,
        Some(v) => match v.strip_prefix('$') {
            Some("SSH_AUTH_SOCK") => AgentSocket::Default,
            Some(var) => match std::env::var_os(var) {
                Some(path) => AgentSocket::At(PathBuf::from(path)),
                None => AgentSocket::Off,
            },
            None => AgentSocket::At(crate::tunnels::expand_home(&expand_tokens(v, r))),
        },
    }
}

fn hop_of(r: &Resolved, passphrase: Option<&str>) -> Hop {
    Hop {
        host: r.hostname.clone(),
        port: r.port,
        user: r.user.clone(),
        auth: Some(Auth::Identities(Identities {
            agent: agent_socket(r),
            files: r
                .identity_files
                .iter()
                .map(|f| crate::tunnels::expand_home(&expand_tokens(f, r)))
                .collect(),
            only: r.identities_only,
            passphrase: passphrase.map(String::from),
        })),
    }
}

/// The alias's servers, as `ssh` resolves them on this machine.
pub async fn resolve(alias: &str) -> CommandResult<Vec<Resolved>> {
    if !valid_host(alias) {
        return Err(bad(format!(
            "«{alias}» no es un alias válido de ~/.ssh/config"
        )));
    }
    let chain = chain(alias, |t| Box::pin(ask_ssh(t))).await?;
    // `ssh -G` answers for any name: a name nothing in the files mentions is
    // an alias of another machine (a synced connection), or a typo.
    if let Some(target) = chain.last() {
        let listed = tokio::task::spawn_blocking(listed)
            .await
            .unwrap_or_default();
        if target.hostname == alias && !listed.mentions(alias) {
            return Err(bad(format!(
                "«{alias}» no está en ~/.ssh/config de esta computadora"
            )));
        }
    }
    Ok(chain)
}

pub async fn hops(alias: &str, passphrase: Option<&str>) -> CommandResult<Vec<Hop>> {
    Ok(resolve(alias)
        .await?
        .iter()
        .map(|r| hop_of(r, passphrase))
        .collect())
}

/// What the config files name: for the form's list and to tell a name they
/// don't mention. Not their meaning, which is `ssh -G`'s.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct Listed {
    pub hosts: BTreeSet<String>,
    patterns: Vec<String>,
    /// A `Match` block may apply to any name.
    has_match: bool,
}

impl Listed {
    fn mentions(&self, name: &str) -> bool {
        self.has_match
            || self.hosts.contains(name)
            || self.patterns.iter().any(|p| wildcard(p, name))
    }
}

/// `*` and `?`, as `Host` patterns and `Include` paths use them.
fn wildcard(pattern: &str, text: &str) -> bool {
    let (p, t): (Vec<char>, Vec<char>) = (pattern.chars().collect(), text.chars().collect());
    let (mut i, mut j) = (0, 0);
    let mut star: Option<(usize, usize)> = None;
    while j < t.len() {
        if i < p.len() && (p[i] == '?' || p[i] == t[j]) {
            i += 1;
            j += 1;
        } else if i < p.len() && p[i] == '*' {
            star = Some((i, j));
            i += 1;
        } else if let Some((si, sj)) = star {
            i = si + 1;
            j = sj + 1;
            star = Some((si, sj + 1));
        } else {
            return false;
        }
    }
    p[i..].iter().all(|c| *c == '*')
}

/// A config line: its keyword and its arguments (quotes hold spaces, and the
/// keyword may end in `=`).
fn config_line(line: &str) -> Option<(String, Vec<String>)> {
    let line = line.trim();
    if line.is_empty() || line.starts_with('#') {
        return None;
    }
    let end = line.find(|c: char| c.is_whitespace() || c == '=')?;
    let (key, rest) = line.split_at(end);
    let rest = rest.trim_start_matches(|c: char| c.is_whitespace() || c == '=');
    let mut args = Vec::new();
    let mut current = String::new();
    let mut quoted = false;
    for c in rest.chars() {
        match c {
            '"' => quoted = !quoted,
            '#' if !quoted && current.is_empty() => break,
            c if c.is_whitespace() && !quoted => {
                if !current.is_empty() {
                    args.push(std::mem::take(&mut current));
                }
            }
            c => current.push(c),
        }
    }
    if !current.is_empty() {
        args.push(current);
    }
    Some((key.to_ascii_lowercase(), args))
}

/// An `Include` argument's files: wildcards in any of its folders or its name.
fn include_paths(pattern: &str, base: &Path) -> Vec<PathBuf> {
    let expanded = crate::tunnels::expand_home(pattern);
    let full = if expanded.is_absolute() {
        expanded
    } else {
        base.join(expanded)
    };
    let mut found = vec![PathBuf::new()];
    for part in full.components() {
        let name = part.as_os_str().to_string_lossy();
        if !name.contains(['*', '?']) {
            for f in &mut found {
                f.push(part);
            }
            continue;
        }
        let mut next = Vec::new();
        for folder in &found {
            let Ok(entries) = std::fs::read_dir(folder) else {
                continue;
            };
            for entry in entries.flatten() {
                if wildcard(&name, &entry.file_name().to_string_lossy()) {
                    next.push(entry.path());
                }
            }
        }
        next.sort();
        found = next;
    }
    found
}

fn read_config(path: &Path, base: &Path, depth: usize, listed: &mut Listed) {
    // OpenSSH's own limit on nested `Include`s.
    if depth > 16 {
        return;
    }
    let Ok(text) = std::fs::read_to_string(path) else {
        return;
    };
    for line in text.lines() {
        let Some((key, args)) = config_line(line) else {
            continue;
        };
        match key.as_str() {
            "host" => {
                for arg in args {
                    if arg.starts_with('!') {
                        continue;
                    }
                    if arg.contains(['*', '?']) {
                        // `Host *` holds defaults: it names nothing.
                        if arg != "*" {
                            listed.patterns.push(arg);
                        }
                    } else {
                        listed.hosts.insert(arg);
                    }
                }
            }
            "match" => listed.has_match = true,
            "include" => {
                for arg in args {
                    for file in include_paths(&arg, base) {
                        read_config(&file, base, depth + 1, listed);
                    }
                }
            }
            _ => {}
        }
    }
}

/// The aliases of the user's and the system's config files.
pub fn listed() -> Listed {
    let mut listed = Listed::default();
    let home = crate::tunnels::expand_home("~/.ssh");
    if let Some(file) = config_file() {
        read_config(&file, &home, 0, &mut listed);
        return listed;
    }
    read_config(&home.join("config"), &home, 0, &mut listed);
    let system = if cfg!(windows) {
        std::env::var_os("PROGRAMDATA")
            .map(PathBuf::from)
            .unwrap_or_default()
            .join("ssh")
    } else {
        PathBuf::from("/etc/ssh")
    };
    read_config(&system.join("ssh_config"), &system, 0, &mut listed);
    listed
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    #[test]
    fn nothing_that_ssh_would_read_as_an_option_is_an_alias() {
        for ok in [
            "demo-internal",
            "bastion.example.test",
            "10.10.0.20",
            "a_b",
            "fe80::1",
        ] {
            assert!(valid_host(ok), "{ok}");
        }
        for bad in [
            "",
            "-oProxyCommand=calc",
            "-G",
            "a b",
            "a;b",
            "$(id)",
            "`id`",
            "a|b",
            "a\nb",
            "%h",
            "\"a\"",
        ] {
            assert!(!valid_host(bad), "{bad}");
        }
    }

    #[test]
    fn reads_what_ssh_answers() {
        let r = parse_resolved(
            "demo",
            "user tunnel_user\nhostname 10.10.0.20\nport 2222\nidentitiesonly yes\n\
             identityfile ~/.ssh/demo_key\nidentityfile ~/.ssh/other\nidentityfile none\n\
             identityagent \"~/Library/Group Containers/agent.sock\"\nproxyjump demo-bastion\n\
             stricthostkeychecking no\n",
        );
        assert_eq!(r.hostname, "10.10.0.20");
        assert_eq!((r.port, r.user.as_str()), (2222, "tunnel_user"));
        assert!(r.identities_only);
        assert_eq!(r.identity_files, ["~/.ssh/demo_key", "~/.ssh/other"]);
        assert_eq!(
            r.identity_agent.as_deref(),
            Some("~/Library/Group Containers/agent.sock")
        );
        assert_eq!(r.proxy_jump.as_deref(), Some("demo-bastion"));
        assert_eq!(parse_resolved("d", "proxyjump none\n").proxy_jump, None);
    }

    #[test]
    fn reads_every_jump_form() {
        let t = |user: Option<&str>, host: &str, port| Target {
            user: user.map(String::from),
            host: host.into(),
            port,
        };
        assert_eq!(
            parse_jumps("a, ops@b:2200,ssh://c:23,[fe80::1]:2").unwrap(),
            vec![
                t(None, "a", None),
                t(Some("ops"), "b", Some(2200)),
                t(None, "c", Some(23)),
                t(None, "fe80::1", Some(2)),
            ]
        );
        assert!(parse_jumps("none").unwrap().is_empty());
        for bad in ["-oProxyCommand=x", "a:notaport", "-l@a", "[a", "a b"] {
            assert!(parse_jumps(bad).is_err(), "{bad}");
        }
    }

    /// The chain with `ssh -G` answered from a table.
    async fn chain_of(
        table: &[(&str, &str, Option<&str>)],
        alias: &str,
    ) -> CommandResult<Vec<String>> {
        let table: HashMap<String, (String, Option<String>)> = table
            .iter()
            .map(|(n, h, j)| (n.to_string(), (h.to_string(), j.map(String::from))))
            .collect();
        let found = chain(alias, move |t: Target| -> Asked {
            let (hostname, proxy_jump) = table[&t.host].clone();
            let name = t.host;
            Box::pin(async move {
                Ok(Resolved {
                    name,
                    hostname,
                    proxy_jump,
                    port: 22,
                    ..Default::default()
                })
            })
        })
        .await?;
        Ok(found.into_iter().map(|r| r.hostname).collect())
    }

    #[tokio::test]
    async fn a_chain_goes_as_ssh_dash_j_goes() {
        let table = [
            ("target", "t.host", Some("a,b")),
            ("a", "a.host", Some("first")),
            // `-J a,b`: b is reached through a, whatever its file says.
            ("b", "b.host", Some("ignored")),
            ("first", "first.host", None),
            ("ignored", "ignored.host", None),
            ("alone", "alone.host", None),
        ];
        assert_eq!(
            chain_of(&table, "target").await.unwrap(),
            ["first.host", "a.host", "b.host", "t.host"]
        );
        assert_eq!(chain_of(&table, "alone").await.unwrap(), ["alone.host"]);
    }

    #[tokio::test]
    async fn a_loop_and_a_too_long_chain_are_refused() {
        let table = [("a", "a.host", Some("b")), ("b", "b.host", Some("a"))];
        let e = chain_of(&table, "a").await.unwrap_err().to_string();
        assert!(e.contains("bucle"), "{e}");

        let names: Vec<String> = (0..12).map(|i| format!("h{i}")).collect();
        let long: Vec<(&str, &str, Option<&str>)> = names
            .iter()
            .enumerate()
            .map(|(i, n)| (n.as_str(), n.as_str(), names.get(i + 1).map(String::as_str)))
            .collect();
        let e = chain_of(&long, "h0").await.unwrap_err().to_string();
        assert!(e.contains("más de"), "{e}");
    }

    #[tokio::test]
    async fn proxy_command_is_said_not_ignored() {
        let found = chain("p", |t: Target| -> Asked {
            Box::pin(async move {
                Ok(Resolved {
                    name: t.host,
                    hostname: "p.host".into(),
                    proxy_command: Some("ssh -W %h:%p x".into()),
                    ..Default::default()
                })
            })
        })
        .await;
        let e = found.unwrap_err().to_string();
        assert!(e.contains("ProxyCommand"), "{e}");
    }

    #[test]
    fn paths_get_their_tokens_and_each_hop_its_keys() {
        let r = Resolved {
            name: "demo".into(),
            hostname: "h.test".into(),
            port: 2222,
            user: "ops".into(),
            identity_files: vec!["/k/%h_%r_%p_%n%%".into(), "/k/%X".into()],
            identities_only: true,
            ..Default::default()
        };
        let hop = hop_of(&r, Some("frase"));
        assert_eq!(
            (hop.host.as_str(), hop.port, hop.user.as_str()),
            ("h.test", 2222, "ops")
        );
        let Some(Auth::Identities(i)) = hop.auth else {
            panic!("{:?}", hop.auth)
        };
        assert_eq!(
            i.files,
            [
                PathBuf::from("/k/h.test_ops_2222_demo%"),
                PathBuf::from("/k/%X")
            ]
        );
        assert!(i.only);
        assert_eq!(i.agent, AgentSocket::Default);
        assert_eq!(i.passphrase.as_deref(), Some("frase"));
    }

    #[test]
    fn the_agent_is_the_one_the_file_names() {
        let with = |v: &str| Resolved {
            identity_agent: Some(v.into()),
            ..Default::default()
        };
        assert_eq!(agent_socket(&Resolved::default()), AgentSocket::Default);
        assert_eq!(agent_socket(&with("SSH_AUTH_SOCK")), AgentSocket::Default);
        assert_eq!(agent_socket(&with("$SSH_AUTH_SOCK")), AgentSocket::Default);
        assert_eq!(agent_socket(&with("none")), AgentSocket::Off);
        assert_eq!(
            agent_socket(&with("/run/agent.sock")),
            AgentSocket::At(PathBuf::from("/run/agent.sock"))
        );
        assert_eq!(
            agent_socket(&with("$DBINE_NO_SUCH_VARIABLE")),
            AgentSocket::Off
        );
    }

    #[test]
    fn lists_the_aliases_through_includes() {
        let dir = std::env::temp_dir().join(format!("dbine-ssh-config-{}", std::process::id()));
        let conf = dir.join("conf.d");
        std::fs::create_dir_all(&conf).unwrap();
        let main = dir.join("config");
        std::fs::write(
            &main,
            "# comment\nHost demo-bastion demo-internal\n  HostName x\nHost=spaced\n\
             Host *\n  User u\nHost *.corp !skip.corp\nInclude conf.d/*.conf\nHost \"quoted\" # note\n",
        )
        .unwrap();
        std::fs::write(conf.join("a.conf"), "Host from-include\n").unwrap();
        std::fs::write(conf.join("b.txt"), "Host not-this\n").unwrap();
        let mut listed = Listed::default();
        read_config(&main, &dir, 0, &mut listed);
        let names: Vec<&str> = listed.hosts.iter().map(String::as_str).collect();
        assert_eq!(
            names,
            [
                "demo-bastion",
                "demo-internal",
                "from-include",
                "quoted",
                "spaced"
            ]
        );
        assert!(listed.mentions("demo-internal"));
        assert!(listed.mentions("db.corp"), "by its pattern");
        assert!(!listed.mentions("typo"), "`Host *` names nothing");
        assert!(!listed.mentions("not-this"));
        let _ = std::fs::remove_dir_all(&dir);
        assert!(Listed {
            has_match: true,
            ..Default::default()
        }
        .mentions("anything"));
    }

    #[test]
    fn wildcards_match_as_ssh_matches() {
        assert!(wildcard("*.corp", "db.corp"));
        assert!(wildcard("db?", "db1"));
        assert!(wildcard("a*b*c", "axxbyyc"));
        assert!(!wildcard("*.corp", "db.corp.net"));
        assert!(!wildcard("db?", "db"));
    }
}
