//! The process list ([`dbine_driver::Session::processes`]) from
//! `M_CONNECTIONS`, with the running statement from `M_ACTIVE_STATEMENTS`
//! and the lock wait from `M_BLOCKED_TRANSACTIONS`; and stopping another
//! connection's statement ([`dbine_driver::Session::cancel_query`]) with
//! `ALTER SYSTEM CANCEL SESSION`, which keeps the connection (DISCONNECT
//! SESSION, in `blocking`, ends it). Ids are connection ids. Reading the
//! views needs the MONITORING role; cancelling, SESSION ADMIN.

use crate::blocking::session_id;
use crate::monitor::{query, Set};
use dbine_driver::{Error, Result, ServerProcess};
use hdbconnect_async::Connection;

/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Idle connections with an open write transaction whose last statement is
/// looked up (one query for all of them).
const MAX_IDLE_LOOKUPS: usize = 50;

/// One row per live connection, joined to its running statements (a
/// connection may have more than one: the longest-running comes first and
/// the rest are dropped) and to the first connection it waits for.
const PROCESSES: &str = "SELECT TOP 2000 c.CONNECTION_ID, c.CONNECTION_STATUS, c.CONNECTION_TYPE, c.USER_NAME,
       c.CLIENT_HOST, sc.VALUE AS APPLICATION, c.CURRENT_SCHEMA_NAME, c.IDLE_TIME,
       NANO100_BETWEEN(a.LAST_EXECUTED_TIME, CURRENT_TIMESTAMP) / 10000 AS RUNNING_MS,
       TO_NVARCHAR(SUBSTR(a.STATEMENT_STRING, 1, 20000)) AS SQL_TEXT,
       w.BLOCKER, w.WAIT,
       CASE WHEN c.CONNECTION_ID = CURRENT_CONNECTION THEN 1 ELSE 0 END AS OWN,
       CASE WHEN t.TRANSACTION_STATUS = 'ACTIVE' AND t.UPDATE_TRANSACTION_ID > 0 THEN 1 ELSE 0 END AS OPEN_TX
  FROM SYS.M_CONNECTIONS c
  LEFT JOIN SYS.M_ACTIVE_STATEMENTS a
         ON a.CONNECTION_ID = c.CONNECTION_ID AND a.STATEMENT_STATUS IN ('ACTIVE', 'SUSPENDED')
  LEFT JOIN SYS.M_SESSION_CONTEXT sc
         ON sc.CONNECTION_ID = c.CONNECTION_ID AND sc.HOST = c.HOST AND sc.PORT = c.PORT AND sc.KEY = 'APPLICATION'
  LEFT JOIN SYS.M_TRANSACTIONS t ON t.TRANSACTION_ID = c.TRANSACTION_ID AND t.HOST = c.HOST AND t.PORT = c.PORT
  LEFT JOIN (SELECT bt.CONNECTION_ID AS WAITER, MIN(ot.CONNECTION_ID) AS BLOCKER,
                    MIN(b.LOCK_TYPE || ' LOCK ' || b.LOCK_MODE) AS WAIT
               FROM SYS.M_BLOCKED_TRANSACTIONS b
               JOIN SYS.M_TRANSACTIONS bt ON bt.TRANSACTION_ID = b.BLOCKED_TRANSACTION_ID AND bt.HOST = b.HOST AND bt.PORT = b.PORT
               JOIN SYS.M_TRANSACTIONS ot ON ot.TRANSACTION_ID = b.LOCK_OWNER_TRANSACTION_ID AND ot.HOST = b.HOST AND ot.PORT = b.PORT
              WHERE bt.CONNECTION_ID > 0 AND ot.CONNECTION_ID > 0 AND bt.CONNECTION_ID <> ot.CONNECTION_ID
              GROUP BY bt.CONNECTION_ID) w ON w.WAITER = c.CONNECTION_ID
 WHERE c.CONNECTION_ID > 0 AND c.IS_ACTIVE = 'TRUE'
 ORDER BY CASE WHEN c.CONNECTION_STATUS = 'IDLE' THEN 1 ELSE 0 END, c.CONNECTION_ID,
          a.LAST_EXECUTED_TIME";

fn s(set: &Set, row: usize, col: &str) -> Option<String> {
    set.get(row, col).map(str::to_string)
}

fn n(set: &Set, row: usize, col: &str) -> Option<u64> {
    set.get(row, col).and_then(|v| v.parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

fn clip(s: String) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((at, _)) => s[..at].to_string(),
        None => s,
    }
}

/// The statement's first keyword, upper case ("SELECT", "CALL"…).
fn command(sql: &str) -> Option<String> {
    let w: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!w.is_empty()).then(|| w.to_ascii_uppercase())
}

pub(crate) fn assemble(set: &Set) -> Vec<ServerProcess> {
    let mut out: Vec<ServerProcess> = Vec::new();
    for r in 0..set.rows.len() {
        let Some(id) = n(set, r, "CONNECTION_ID") else { continue };
        let id = id.to_string();
        // Already listed: a further running statement of the same connection.
        if out.last().is_some_and(|p| p.id == id) {
            continue;
        }
        let status = s(set, r, "CONNECTION_STATUS");
        let sql = s(set, r, "SQL_TEXT").map(clip);
        let active = sql.is_some() || status.as_deref().is_some_and(|v| !v.eq_ignore_ascii_case("IDLE"));
        let system = s(set, r, "CONNECTION_TYPE").is_some_and(|t| !t.eq_ignore_ascii_case("Remote"))
            || s(set, r, "USER_NAME").is_some_and(|u| u.starts_with("_SYS"));
        out.push(ServerProcess {
            id,
            status,
            active,
            system,
            own: n(set, r, "OWN") == Some(1),
            user: s(set, r, "USER_NAME"),
            host: s(set, r, "CLIENT_HOST"),
            program: s(set, r, "APPLICATION"),
            database: s(set, r, "CURRENT_SCHEMA_NAME"),
            command: sql.as_deref().and_then(command),
            elapsed_ms: if active { n(set, r, "RUNNING_MS") } else { n(set, r, "IDLE_TIME") },
            wait: s(set, r, "WAIT"),
            blocked_by: s(set, r, "BLOCKER"),
            sql,
            ..Default::default()
        });
    }
    out
}

pub(crate) async fn processes(conn: &Connection) -> Result<Vec<ServerProcess>> {
    let set = query(conn, PROCESSES)
        .await
        .map_err(|e| Error::Query(format!("No se pudieron leer los procesos (hace falta el rol MONITORING): {e}")))?;
    let mut list = assemble(&set);
    // Idle in an open write transaction: its last statement.
    let open: Vec<u64> = (0..set.rows.len())
        .filter(|&r| n(&set, r, "OPEN_TX") == Some(1) && s(&set, r, "SQL_TEXT").is_none())
        .filter_map(|r| n(&set, r, "CONNECTION_ID"))
        .take(MAX_IDLE_LOOKUPS)
        .collect();
    if !open.is_empty() {
        // Numbers parsed above: safe to interpolate.
        let ids = open.iter().map(u64::to_string).collect::<Vec<_>>().join(",");
        let sql = format!(
            "SELECT CONNECTION_ID, TO_NVARCHAR(SUBSTR(STATEMENT_STRING, 1, {MAX_TEXT})) AS SQL_TEXT
               FROM SYS.M_PREPARED_STATEMENTS
              WHERE CONNECTION_ID IN ({ids}) AND LAST_EXECUTED_TIME IS NOT NULL
              ORDER BY LAST_EXECUTED_TIME DESC"
        );
        match query(conn, &sql).await {
            Ok(last) => {
                for r in 0..last.rows.len() {
                    let (Some(id), Some(text)) = (n(&last, r, "CONNECTION_ID"), s(&last, r, "SQL_TEXT")) else { continue };
                    let id = id.to_string();
                    if let Some(p) = list.iter_mut().find(|p| p.id == id && !p.active && p.sql.is_none()) {
                        p.sql = Some(text);
                    }
                }
            }
            Err(e) => tracing::debug!("hana processes: M_PREPARED_STATEMENTS: {e}"),
        }
    }
    Ok(list)
}

pub(crate) async fn cancel(conn: &Connection, id: &str) -> Result<()> {
    let id = session_id(id)?;
    let own = query(conn, "SELECT CURRENT_CONNECTION AS ID FROM DUMMY").await?;
    if own.get(0, "ID").and_then(|v| v.parse::<u64>().ok()) == Some(id) {
        return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá".into()));
    }
    conn.exec(format!("ALTER SYSTEM CANCEL SESSION '{id}'"))
        .await
        .map_err(|e| Error::Query(format!("no se pudo cancelar la consulta de la conexión {id} (hace falta SESSION ADMIN): {e}")))
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
    fn rows_become_processes() {
        let cols = [
            "CONNECTION_ID",
            "CONNECTION_STATUS",
            "CONNECTION_TYPE",
            "USER_NAME",
            "IDLE_TIME",
            "RUNNING_MS",
            "SQL_TEXT",
            "BLOCKER",
            "WAIT",
            "OWN",
        ];
        let list = assemble(&set(
            &cols,
            &[
                &[Some("200"), Some("RUNNING"), Some("Remote"), Some("APP"), Some("0"), Some("1500.7"), Some(" select * from t"), None, None, Some("0")],
                // A second running statement of the same connection.
                &[Some("200"), Some("RUNNING"), Some("Remote"), Some("APP"), Some("0"), Some("10"), Some("SELECT 1 FROM DUMMY"), None, None, Some("0")],
                &[Some("201"), Some("RUNNING"), Some("Remote"), Some("APP"), Some("0"), Some("900"), Some("UPDATE T SET V = 1"), Some("202"), Some("RECORD LOCK EXCLUSIVE"), Some("0")],
                &[Some("202"), Some("IDLE"), Some("Remote"), Some("SYSTEM"), Some("4000"), None, None, None, None, Some("1")],
                &[Some("300"), Some("IDLE"), Some("Local"), Some("_SYS_STATISTICS"), Some("10"), None, None, None, None, Some("0")],
            ],
        ));
        assert_eq!(list.len(), 4);
        assert!(list[0].active && list[0].elapsed_ms == Some(1500) && list[0].command.as_deref() == Some("SELECT"));
        assert_eq!(list[1].blocked_by.as_deref(), Some("202"));
        assert_eq!(list[1].wait.as_deref(), Some("RECORD LOCK EXCLUSIVE"));
        assert!(!list[2].active && list[2].own && list[2].elapsed_ms == Some(4000) && list[2].sql.is_none());
        assert!(list[3].system);
    }
}
