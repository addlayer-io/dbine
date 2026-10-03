//! The process list ([`dbine_driver::Session::processes`]) and stopping
//! another operation ([`dbine_driver::Session::cancel_query`]).
//!
//! One `$currentOp` aggregation (the monitor's `currentOp`, with idle
//! connections and, on MongoDB, idle sessions) gives one row per
//! connection, operation or idle transaction. Ids match `kill_session`
//! (see blocking.rs):
//!
//! - a running operation: its `opid` ("845", "shard01:845" through mongos),
//!   stopped with `killOp` (the connection stays open);
//! - an idle connection: its description ("conn1182"), nothing to stop;
//! - an idle session with an open transaction: `lsid:<uuid>`, which
//!   `kill_session` ends with `killSessions`;
//! - the server's own threads (no client): their description, `system`.
//!
//! The listing carries the session's `comment`, so it and the session's own
//! operations show up as `own`. FerretDB lists its sessions (PostgreSQL
//! backends) with little detail and has no `killOp`. Amazon DocumentDB
//! reports `WaitState` and `blockedOn`.

use crate::blocking::{lsid, opid, unsupported_ferret, valid_opid, TXN_PREFIX};
use crate::monitor::{compact, num};
use crate::{err, Flavor};
use dbine_driver::{Error, Result, ServerProcess};
use futures::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::Client;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of an operation's command.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: i64 = 2000;

fn stage(flavor: Flavor) -> Document {
    match flavor {
        // DocumentDB's $currentOp takes allUsers and idleConnections only.
        Flavor::DocumentDb => doc! { "$currentOp": { "allUsers": true, "idleConnections": true } },
        _ => doc! { "$currentOp": { "allUsers": true, "idleConnections": true, "idleSessions": true } },
    }
}

async fn current_ops(client: &Client, pipeline: Vec<Document>, tag: &str) -> Result<Vec<Document>> {
    let run = async {
        let cur = client.database("admin").aggregate(pipeline).max_time(QUERY_LIMIT).comment(tag).await.map_err(err)?;
        cur.try_collect::<Vec<Document>>().await.map_err(err)
    };
    tokio::time::timeout(QUERY_LIMIT + Duration::from_secs(1), run)
        .await
        .map_err(|_| Error::Query("el servidor tardó demasiado en listar sus operaciones (currentOp)".into()))?
}

pub(crate) async fn processes(client: &Client, flavor: Flavor, tag: &str) -> Result<Vec<ServerProcess>> {
    let ops = current_ops(client, vec![stage(flavor), doc! { "$sort": { "active": -1 } }, doc! { "$limit": MAX_ROWS }], tag).await?;
    Ok(ops.iter().filter_map(|op| row(op, flavor, tag)).collect())
}

/// Whether the operation carries this session's `comment`.
fn tagged(op: &Document, tag: &str) -> bool {
    let comment = |d: Option<&Document>| d.and_then(|c| c.get_str("comment").ok()) == Some(tag);
    comment(op.get_document("command").ok())
        || comment(op.get_document("cursor").ok().and_then(|c| c.get_document("originatingCommand").ok()))
}

fn row(op: &Document, flavor: Flavor, tag: &str) -> Option<ServerProcess> {
    let active = op.get_bool("active").unwrap_or(false);
    let idle_session = op.get_str("type") == Ok("idleSession");
    let in_txn = idle_session && op.get_document("transaction").is_ok();
    let txn = if idle_session { lsid(op) } else { None };
    let id = match (&txn, op.get("opid").and_then(opid)) {
        (Some(u), _) => format!("{TXN_PREFIX}{u}"),
        (None, Some(o)) => o,
        (None, None) => op.get_str("desc").ok().filter(|d| !d.is_empty())?.to_string(),
    };
    let client = op.get_str("client").or_else(|_| op.get_str("client_s")).ok().filter(|c| !c.is_empty());
    let user = op
        .get_array("effectiveUsers")
        .ok()
        .and_then(|a| a.first())
        .and_then(Bson::as_document)
        .and_then(|u| u.get_str("user").ok())
        .map(str::to_string);
    let command = op.get_document("command").ok().filter(|c| !c.is_empty());
    let database = op
        .get_str("ns")
        .ok()
        .and_then(|ns| ns.split('.').next())
        .filter(|d| !d.is_empty())
        .or_else(|| command.and_then(|c| c.get_str("$db").ok()))
        .map(str::to_string);
    let waiting = op.get_bool("waitingForLock").unwrap_or(false);
    let status = if active && waiting {
        "esperando un bloqueo"
    } else if active {
        "ejecutando"
    } else if in_txn {
        "inactiva con transacción abierta"
    } else {
        "inactiva"
    };
    let ms = |us: Option<f64>, secs: Option<f64>| us.map(|v| v / 1000.0).or(secs.map(|s| s * 1000.0)).map(|v| v.max(0.0) as u64);
    let elapsed_ms = if active {
        ms(num(op, &["microsecs_running"]), num(op, &["secs_running"]))
    } else {
        ms(num(op, &["transaction", "timeInactiveMicros"]), num(op, &["secs_idle"]))
    };
    Some(ServerProcess {
        id,
        status: Some(status.into()),
        active,
        // Server threads have no client; FerretDB reports no clients at all.
        system: flavor != Flavor::Ferret && client.is_none() && user.is_none() && !idle_session,
        own: tagged(op, tag),
        user,
        host: client.map(str::to_string),
        program: op.get_str("appName").ok().filter(|a| !a.is_empty()).map(str::to_string),
        database,
        command: if active {
            command
                .and_then(|c| c.keys().find(|k| !k.starts_with('$')).cloned())
                .or_else(|| op.get_str("op").ok().map(str::to_string))
        } else {
            None
        },
        elapsed_ms,
        wait: op
            .get_str("WaitState")
            .ok()
            .map(str::to_string)
            .or_else(|| waiting.then(|| "bloqueo".to_string())),
        blocked_by: op.get("blockedOn").and_then(opid).filter(|b| valid_opid(b)),
        sql: command.filter(|_| active).map(|c| compact(&Bson::Document(c.clone()), MAX_TEXT)),
        ..Default::default()
    })
}

pub(crate) async fn cancel(client: &Client, flavor: Flavor, tag: &str, id: &str) -> Result<()> {
    if flavor == Flavor::Ferret {
        return Err(unsupported_ferret());
    }
    let id = id.trim();
    if id.starts_with(TXN_PREFIX) {
        return Err(Error::Query(format!(
            "«{id}» es una transacción inactiva: no hay una operación en curso que cancelar (se puede terminar la sesión)"
        )));
    }
    if !valid_opid(id) {
        let idle = id.strip_prefix("conn").is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()));
        return Err(Error::Query(if idle {
            format!("la conexión {id} está inactiva: no hay una operación en curso que cancelar")
        } else {
            format!("«{id}» no es un id de operación (opid)")
        }));
    }
    let op = if id.contains(':') {
        Bson::String(id.to_string())
    } else {
        let n: i64 = id.parse().map_err(|_| Error::Query(format!("«{id}» no es un id de operación (opid)")))?;
        i32::try_from(n).map(Bson::Int32).unwrap_or(Bson::Int64(n))
    };
    let found = current_ops(client, vec![doc! { "$currentOp": { "allUsers": true } }, doc! { "$match": { "opid": op } }], tag).await?;
    let Some(running) = found.first() else {
        return Err(Error::Query(format!("la operación {id} ya terminó o no existe")));
    };
    if tagged(running, tag) {
        return Err(Error::Query("esa es una operación de la sesión con la que DBine está consultando: no se puede cancelar desde acá".into()));
    }
    crate::blocking::kill(client, flavor, id).await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_from_current_op() {
        let tag = "dbine-1";
        let running = doc! { "type": "op", "opid": 845, "active": true, "client": "10.0.0.5:5000", "appName": "app",
            "effectiveUsers": [ { "user": "ana", "db": "admin" } ], "ns": "shop.orders", "microsecs_running": 2_500_000_i64,
            "op": "query", "command": { "find": "orders", "filter": { "x": 1 }, "$db": "shop" } };
        let p = row(&running, Flavor::Mongo, tag).unwrap();
        assert_eq!((p.id.as_str(), p.active, p.own, p.system), ("845", true, false, false));
        assert_eq!(p.command.as_deref(), Some("find"));
        assert_eq!(p.database.as_deref(), Some("shop"));
        assert_eq!(p.elapsed_ms, Some(2500));
        assert!(p.sql.unwrap().contains("\"filter\""));

        let own = doc! { "opid": 9, "active": true, "client": "c", "command": { "aggregate": 1, "comment": tag } };
        assert!(row(&own, Flavor::Mongo, tag).unwrap().own);

        let idle = doc! { "type": "op", "desc": "conn1182", "active": false, "client": "127.0.0.1:1" };
        let p = row(&idle, Flavor::Mongo, tag).unwrap();
        assert_eq!((p.id.as_str(), p.active, p.system, p.sql), ("conn1182", false, false, None));

        let thread = doc! { "type": "op", "desc": "TTLMonitor", "active": false };
        assert!(row(&thread, Flavor::Mongo, tag).unwrap().system);

        let ferret = doc! { "type": "idleSession", "opid": "1000:1791", "active": false, "secs_idle": 7_i64 };
        let p = row(&ferret, Flavor::Ferret, tag).unwrap();
        assert_eq!((p.id.as_str(), p.elapsed_ms, p.system), ("1000:1791", Some(7000), false));

        let docdb = doc! { "opid": 75, "active": true, "WaitState": "CollectionLock", "blockedOn": 74 };
        let p = row(&docdb, Flavor::DocumentDb, tag).unwrap();
        assert_eq!((p.wait.as_deref(), p.blocked_by.as_deref()), (Some("CollectionLock"), Some("74")));
        let internal = doc! { "opid": 76, "active": true, "blockedOn": "INTERNAL" };
        assert_eq!(row(&internal, Flavor::DocumentDb, tag).unwrap().blocked_by, None);
    }
}
