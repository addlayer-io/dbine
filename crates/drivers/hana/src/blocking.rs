//! Blocking chains (`Session::blocking`) and `ALTER SYSTEM DISCONNECT
//! SESSION` (`Session::kill_session`).
//!
//! `M_BLOCKED_TRANSACTIONS` says which transaction waits on which lock
//! owner; `M_TRANSACTIONS` maps both to their connections, and
//! `M_CONNECTIONS`, `M_ACTIVE_STATEMENTS` and `M_PREPARED_STATEMENTS` fill
//! in user, client, schema and statement. Reading them needs the
//! MONITORING role (or CATALOG READ); disconnecting a session, SESSION ADMIN.

use crate::monitor::{query, Set};
use dbine_driver::{BlockedSession, Error, Result};
use hdbconnect_async::Connection;
use std::collections::HashMap;

const IDLE: &str = "inactiva con transacción abierta";

const WAITS: &str = "SELECT bt.CONNECTION_ID AS WAITER, ot.CONNECTION_ID AS BLOCKER,
       b.LOCK_TYPE, b.LOCK_MODE, NANO100_BETWEEN(b.BLOCKED_TIME, CURRENT_TIMESTAMP) / 10000 AS WAITED_MS,
       b.WAITING_SCHEMA_NAME, b.WAITING_OBJECT_NAME, b.WAITING_RECORD_ID
  FROM SYS.M_BLOCKED_TRANSACTIONS b
  JOIN SYS.M_TRANSACTIONS bt ON bt.TRANSACTION_ID = b.BLOCKED_TRANSACTION_ID AND bt.HOST = b.HOST AND bt.PORT = b.PORT
  JOIN SYS.M_TRANSACTIONS ot ON ot.TRANSACTION_ID = b.LOCK_OWNER_TRANSACTION_ID AND ot.HOST = b.HOST AND ot.PORT = b.PORT
 WHERE bt.CONNECTION_ID > 0 AND ot.CONNECTION_ID > 0
 ORDER BY b.BLOCKED_TIME";

/// One connection waiting for another.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Edge {
    pub waiter: u64,
    pub blocker: u64,
    pub wait: Option<String>,
    pub waited_ms: Option<u64>,
    pub object: Option<String>,
}

/// What M_CONNECTIONS and the statement views say about a connection.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Conn {
    pub user: Option<String>,
    pub client: Option<String>,
    pub schema: Option<String>,
    pub status: Option<String>,
    /// Since its transaction started, ms.
    pub trx_ms: Option<u64>,
    pub running: Option<String>,
    pub last: Option<String>,
}

fn s(set: &Set, row: usize, col: &str) -> Option<String> {
    set.get(row, col).map(str::to_string)
}

fn n(set: &Set, row: usize, col: &str) -> Option<u64> {
    set.get(row, col).and_then(|v| v.parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

fn edges(set: &Set) -> Vec<Edge> {
    (0..set.rows.len())
        .filter_map(|r| {
            let (waiter, blocker) = (n(set, r, "WAITER")?, n(set, r, "BLOCKER")?);
            let wait = match (s(set, r, "LOCK_TYPE"), s(set, r, "LOCK_MODE")) {
                (Some(t), Some(m)) => Some(format!("{t} LOCK {m}")),
                (t, m) => t.or(m),
            };
            let mut object = match (s(set, r, "WAITING_SCHEMA_NAME"), s(set, r, "WAITING_OBJECT_NAME")) {
                (Some(sc), Some(o)) => Some(format!("{sc}.{o}")),
                (sc, o) => o.or(sc),
            };
            if let (Some(o), Some(rec)) = (object.as_mut(), s(set, r, "WAITING_RECORD_ID")) {
                if rec != "0" {
                    o.push_str(&format!(" · {rec}"));
                }
            }
            (waiter != blocker).then_some(Edge { waiter, blocker, wait, waited_ms: n(set, r, "WAITED_MS"), object })
        })
        .collect()
}

/// Waiters (one row each, on their first blocker) and then the heads.
pub(crate) fn assemble(edges: &[Edge], conns: &HashMap<u64, Conn>) -> Vec<BlockedSession> {
    let mut out = Vec::new();
    let mut waiters: Vec<u64> = Vec::new();
    for e in edges {
        if waiters.contains(&e.waiter) {
            continue;
        }
        waiters.push(e.waiter);
        let c = conns.get(&e.waiter).cloned().unwrap_or_default();
        out.push(BlockedSession {
            id: e.waiter.to_string(),
            blocked_by: Some(e.blocker.to_string()),
            user: c.user,
            client: c.client,
            database: c.schema,
            wait: e.wait.clone(),
            waited_ms: e.waited_ms,
            object: e.object.clone(),
            sql: c.running.or(c.last),
        });
    }
    let mut heads: Vec<u64> = Vec::new();
    for e in edges {
        if waiters.contains(&e.blocker) || heads.contains(&e.blocker) {
            continue;
        }
        heads.push(e.blocker);
        let c = conns.get(&e.blocker).cloned().unwrap_or_default();
        let idle = c.running.is_none() && c.status.as_deref().is_none_or(|v| v.eq_ignore_ascii_case("IDLE"));
        out.push(BlockedSession {
            id: e.blocker.to_string(),
            blocked_by: None,
            user: c.user,
            client: c.client,
            database: c.schema,
            wait: if idle { Some(IDLE.into()) } else { c.status },
            waited_ms: c.trx_ms,
            object: None,
            sql: c.running.or(c.last),
        });
    }
    out
}

pub(crate) async fn blocking(conn: &Connection) -> Result<Vec<BlockedSession>> {
    let found = query(conn, WAITS).await.map_err(|e| {
        Error::Query(format!("No se pudieron leer los bloqueos (hace falta el rol MONITORING): {e}"))
    })?;
    let edges = edges(&found);
    if edges.is_empty() {
        return Ok(Vec::new());
    }
    let mut ids: Vec<u64> = edges.iter().flat_map(|e| [e.waiter, e.blocker]).collect();
    ids.sort_unstable();
    ids.dedup();
    // Numbers parsed above: safe to interpolate.
    let list = ids.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
    let mut conns: HashMap<u64, Conn> = HashMap::new();
    let sql = format!(
        "SELECT c.CONNECTION_ID, c.USER_NAME, c.CLIENT_HOST, c.CURRENT_SCHEMA_NAME, c.CONNECTION_STATUS,
                NANO100_BETWEEN(t.START_TIME, CURRENT_TIMESTAMP) / 10000 AS TRX_MS
           FROM SYS.M_CONNECTIONS c
           LEFT JOIN SYS.M_TRANSACTIONS t ON t.TRANSACTION_ID = c.TRANSACTION_ID AND t.HOST = c.HOST AND t.PORT = c.PORT
          WHERE c.CONNECTION_ID IN ({list})"
    );
    match query(conn, &sql).await {
        Ok(set) => {
            for r in 0..set.rows.len() {
                let Some(id) = n(&set, r, "CONNECTION_ID") else { continue };
                conns.insert(
                    id,
                    Conn {
                        user: s(&set, r, "USER_NAME"),
                        client: s(&set, r, "CLIENT_HOST"),
                        schema: s(&set, r, "CURRENT_SCHEMA_NAME"),
                        status: s(&set, r, "CONNECTION_STATUS"),
                        trx_ms: n(&set, r, "TRX_MS"),
                        ..Default::default()
                    },
                );
            }
        }
        Err(e) => tracing::debug!("hana blocking: M_CONNECTIONS: {e}"),
    }
    // The running statement, else the last one each connection executed.
    let running = format!(
        "SELECT CONNECTION_ID, TO_NVARCHAR(SUBSTR(STATEMENT_STRING, 1, 4000)) AS SQL_TEXT
           FROM SYS.M_ACTIVE_STATEMENTS
          WHERE CONNECTION_ID IN ({list}) AND STATEMENT_STATUS IN ('ACTIVE', 'SUSPENDED')"
    );
    let last = format!(
        "SELECT CONNECTION_ID, TO_NVARCHAR(SUBSTR(STATEMENT_STRING, 1, 4000)) AS SQL_TEXT
           FROM SYS.M_PREPARED_STATEMENTS
          WHERE CONNECTION_ID IN ({list}) AND LAST_EXECUTED_TIME IS NOT NULL
          ORDER BY LAST_EXECUTED_TIME DESC"
    );
    for (sql, is_running) in [(running, true), (last, false)] {
        match query(conn, &sql).await {
            Ok(set) => {
                for r in 0..set.rows.len() {
                    let (Some(id), Some(text)) = (n(&set, r, "CONNECTION_ID"), s(&set, r, "SQL_TEXT")) else { continue };
                    let c = conns.entry(id).or_default();
                    let slot = if is_running { &mut c.running } else { &mut c.last };
                    if slot.is_none() {
                        *slot = Some(text);
                    }
                }
            }
            Err(e) => tracing::debug!("hana blocking: statements: {e}"),
        }
    }
    Ok(assemble(&edges, &conns))
}

/// A connection id: an unsigned integer, nothing else.
pub(crate) fn session_id(id: &str) -> Result<u64> {
    let t = id.trim();
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(Error::Query(format!("«{id}» no es un id de conexión de SAP HANA")));
    }
    t.parse().map_err(|_| Error::Query(format!("«{id}» no es un id de conexión de SAP HANA")))
}

pub(crate) async fn kill(conn: &Connection, id: &str) -> Result<()> {
    let id = session_id(id)?;
    conn.exec(format!("ALTER SYSTEM DISCONNECT SESSION '{id}'")).await.map_err(crate::err)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(cols: &[&str], rows: &[&[Option<&str>]]) -> Set {
        Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: rows.iter().map(|r| r.iter().map(|v| v.map(str::to_string)).collect()).collect(),
        }
    }

    #[test]
    fn ids_are_numbers() {
        assert_eq!(session_id(" 300123 ").unwrap(), 300123);
        for bad in ["", "1; DROP TABLE x", "1' --", "-1", "+1"] {
            assert!(session_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn waits_become_a_chain() {
        let waits = set(
            &["WAITER", "BLOCKER", "LOCK_TYPE", "LOCK_MODE", "WAITED_MS", "WAITING_SCHEMA_NAME", "WAITING_OBJECT_NAME", "WAITING_RECORD_ID"],
            &[&[Some("201"), Some("200"), Some("RECORD"), Some("EXCLUSIVE"), Some("1500.4"), Some("APP"), Some("T"), Some("42")]],
        );
        let e = edges(&waits);
        assert_eq!(e[0].object.as_deref(), Some("APP.T · 42"));
        assert_eq!(e[0].wait.as_deref(), Some("RECORD LOCK EXCLUSIVE"));
        assert_eq!(e[0].waited_ms, Some(1500));
        let mut conns = HashMap::new();
        conns.insert(200, Conn { status: Some("IDLE".into()), last: Some("UPDATE T SET V = 1".into()), ..Default::default() });
        let chain = assemble(&e, &conns);
        assert_eq!(chain.len(), 2);
        assert_eq!(chain[0].blocked_by.as_deref(), Some("200"));
        assert_eq!(chain[1].id, "200");
        assert_eq!(chain[1].wait.as_deref(), Some(IDLE));
        assert_eq!(chain[1].sql.as_deref(), Some("UPDATE T SET V = 1"));
    }
}
