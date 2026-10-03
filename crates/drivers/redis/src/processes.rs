//! The process list ([`dbine_driver::Session::processes`]), stopping a
//! client's blocking command ([`dbine_driver::Session::cancel_query`]) and
//! closing a client (`kill_session`), for Redis, Valkey and Dragonfly.
//!
//! - `CLIENT LIST` gives one row per connection (the sessions table of the
//!   monitor reads the same reply) and `CLIENT ID` tells DBine's own.
//! - Redis runs one command at a time, so while `CLIENT LIST` answers no
//!   other client is mid-command: the only ones "running" are those parked
//!   in a blocking command (`BLPOP`, `XREAD BLOCK`, `WAIT`…, flag `b`).
//!   Cancelling is `CLIENT UNBLOCK <id> ERROR`, which makes that command
//!   fail and keeps the connection. A long Lua script or function blocks
//!   the whole server instead and is stopped with `SCRIPT KILL` /
//!   `FUNCTION KILL` from the console, not per client.
//! - `CLIENT KILL ID <id>` closes a connection.
//! - Dragonfly has no `CLIENT UNBLOCK` (cancelling is unsupported there),
//!   doesn't report the command or the user, and names an unnamed client
//!   after its id.
//!
//! The reply doesn't carry a command's arguments, only its name, so `sql`
//! stays empty.

use crate::shape;
use crate::RedisSession;
use dbine_driver::monitor::num;
use dbine_driver::{Error, Result, ServerProcess};
use redis::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Why a server without `CLIENT UNBLOCK` (Dragonfly) can't cancel, in
/// Spanish (for the error and the docs).
pub(crate) const UNBLOCK_MISSING: &str =
    "Dragonfly no tiene CLIENT UNBLOCK: no se puede cortar el comando bloqueante de otro cliente sin cerrar su conexión (CLIENT KILL)";
/// Rows at most: a server with thousands of clients still answers fast.
const MAX_ROWS: usize = 2000;

/// What a client's flags say it's doing, in Spanish, and whether that's
/// running (a blocking command waiting for data counts).
fn state(flags: &str, own: bool) -> (&'static str, bool) {
    if own {
        ("activa", true)
    } else if flags.contains('b') {
        ("bloqueada", true)
    } else if flags.contains('x') {
        ("en transacción (MULTI)", false)
    } else if flags.contains('P') {
        ("suscrita (pub/sub)", false)
    } else if flags.contains('O') {
        ("MONITOR", false)
    } else if flags.contains('S') {
        ("réplica", false)
    } else if flags.contains('M') {
        ("primaria", false)
    } else {
        ("inactiva", false)
    }
}

/// `CLIENT LIST` text → the process rows (running first, then by id).
pub(crate) fn parse(text: &str, own_id: Option<&str>) -> Vec<ServerProcess> {
    let mut rows: Vec<ServerProcess> = text
        .lines()
        .filter(|l| !l.trim().is_empty())
        .filter_map(|l| {
            let p: HashMap<&str, &str> = l.split_whitespace().filter_map(|f| f.split_once('=')).collect();
            let field = |k: &str| p.get(k).copied().map(str::trim).filter(|v| !v.is_empty());
            let id = field("id")?.to_string();
            let flags = field("flags").unwrap_or("");
            let own = own_id == Some(id.as_str());
            let (status, blocked_or_own) = state(flags, own);
            // Dragonfly runs commands on several threads: one being
            // processed right now says so in `phase`.
            let active = blocked_or_own || field("phase") == Some("process");
            let cmd = field("cmd").filter(|c| *c != "NULL");
            // Dragonfly's default name is the id itself.
            let name = field("name").filter(|n| *n != id);
            let program = name.or_else(|| field("lib-name")).map(|n| match field("lib-ver").filter(|_| name.is_none()) {
                Some(v) => format!("{n} {v}"),
                None => n.to_string(),
            });
            Some(ServerProcess {
                id,
                status: Some(status.into()),
                active,
                // Replication links (to a replica or from the primary).
                system: flags.contains('S') || flags.contains('M'),
                own,
                user: field("user").map(Into::into),
                host: field("addr").map(Into::into),
                program,
                database: field("db").map(|d| format!("db{d}")),
                command: cmd.map(|c| c.replace('|', " ").to_uppercase()),
                // For a blocked client, idle is how long it has been waiting.
                elapsed_ms: field("idle").and_then(num).map(|s| (s.max(0.0) * 1000.0) as u64),
                wait: flags.contains('b').then(|| "comando bloqueante".to_string()),
                ..Default::default()
            })
        })
        .collect();
    rows.sort_by(|a, b| b.active.cmp(&a.active).then_with(|| id_key(&a.id).cmp(&id_key(&b.id))));
    rows.truncate(MAX_ROWS);
    rows
}

fn id_key(id: &str) -> (usize, &str) {
    (id.len(), id)
}

/// A client id as the server prints it (a positive integer).
fn client_id(id: &str) -> Result<String> {
    let id = id.trim();
    id.parse::<u64>().ok().filter(|n| *n > 0).map(|n| n.to_string()).ok_or_else(|| Error::Query(format!("«{id}» no es un id de cliente de Redis")))
}

impl RedisSession {
    async fn own_id(&mut self) -> Result<String> {
        Ok(shape::text_of(&self.run(&[b"CLIENT", b"ID"]).await?).trim().to_string())
    }

    pub(crate) async fn processes(&mut self) -> Result<Vec<ServerProcess>> {
        let own = self.own_id().await.ok();
        let list = tokio::time::timeout(QUERY_LIMIT, self.run(&[b"CLIENT", b"LIST"]))
            .await
            .map_err(|_| Error::Query("CLIENT LIST no respondió a tiempo".into()))?
            .map_err(|e| Error::Query(format!("no se pudo leer CLIENT LIST (en servicios administrados suele estar deshabilitado): {e}")))?;
        Ok(parse(&shape::text_of(&list), own.as_deref()))
    }

    /// Checks shared by cancelling and closing: the id's shape, a read-only
    /// connection and DBine's own client.
    async fn other_client(&mut self, id: &str, what: &str) -> Result<String> {
        let id = client_id(id)?;
        if self.read_only {
            return Err(Error::Query(format!("Conexión de solo lectura: no se puede {what}.")));
        }
        if self.own_id().await.ok().as_deref() == Some(id.as_str()) {
            return Err(Error::Query("esa es la conexión con la que DBine está consultando: no se puede cortar desde acá".into()));
        }
        Ok(id)
    }

    pub(crate) async fn cancel(&mut self, id: &str) -> Result<()> {
        let id = self.other_client(id, "cancelar comandos de otros clientes").await?;
        let reply = self.send(&[b"CLIENT", b"UNBLOCK", id.as_bytes(), b"ERROR"]).await.map_err(|e| {
            if e.to_string().to_ascii_lowercase().contains("unknown subcommand") {
                Error::Unsupported(UNBLOCK_MISSING.into())
            } else {
                crate::err(e)
            }
        })?;
        match reply {
            Value::Int(1) => Ok(()),
            _ => Err(Error::Query(format!(
                "no se pudo cancelar el comando del cliente {id}: no existe o no está esperando en un comando bloqueante (BLPOP, XREAD BLOCK, WAIT…). Redis ejecuta un comando a la vez, así que los demás terminan solos; un script largo se corta con SCRIPT KILL"
            ))),
        }
    }

    pub(crate) async fn kill(&mut self, id: &str) -> Result<()> {
        let id = self.other_client(id, "cerrar conexiones de otros clientes").await?;
        match self.run(&[b"CLIENT", b"KILL", b"ID", id.as_bytes()]).await? {
            Value::Int(n) if n > 0 => Ok(()),
            _ => Err(Error::Query(format!("no se pudo cerrar el cliente {id}: ya no existe"))),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn client_list_rows() {
        let rows = parse(
            "id=3 addr=127.0.0.1:5000 laddr=127.0.0.1:6379 fd=8 name= age=10 idle=5 flags=N db=0 cmd=get user=default lib-name=redis-rs lib-ver=0.27\n\
             id=12 addr=127.0.0.1:5001 name=worker age=40 idle=30 flags=b db=1 cmd=blpop user=app\n\
             id=4 addr=127.0.0.1:5002 name=dbine age=2 idle=0 flags=N db=0 cmd=client|list user=default\n\
             id=5 addr=10.0.0.2:6380 name= age=99 idle=1 flags=S db=0 cmd=replconf user=default\n\
             id=6 addr=127.0.0.1:43526 name=6 tid=0 age=1 idle=1 db=0 flags=b phase=process lib-name= lib-ver=\n",
            Some("4"),
        );
        assert_eq!(rows.iter().map(|r| r.id.as_str()).collect::<Vec<_>>(), ["4", "6", "12", "3", "5"]);
        let own = &rows[0];
        assert!(own.own && own.active);
        assert_eq!(own.command.as_deref(), Some("CLIENT LIST"));
        // Dragonfly: no command, the name is the id.
        assert!(rows[1].active && rows[1].program.is_none() && rows[1].command.is_none());
        let blocked = &rows[2];
        assert!(blocked.active && !blocked.own);
        assert_eq!(blocked.status.as_deref(), Some("bloqueada"));
        assert_eq!(blocked.elapsed_ms, Some(30000));
        assert_eq!(blocked.database.as_deref(), Some("db1"));
        assert_eq!(blocked.program.as_deref(), Some("worker"));
        assert!(blocked.wait.is_some());
        assert_eq!(rows[3].program.as_deref(), Some("redis-rs 0.27"));
        assert!(!rows[3].active && rows[3].sql.is_none());
        assert!(rows[4].system);
    }

    #[test]
    fn ids_are_validated() {
        assert_eq!(client_id(" 42 ").unwrap(), "42");
        assert!(client_id("1; FLUSHALL").is_err());
        assert!(client_id("0").is_err());
    }
}
