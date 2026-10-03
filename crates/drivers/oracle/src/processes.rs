//! The process list ([`dbine_driver::Session::processes`]) from `V$SESSION`
//! (with `V$SQL` for the running statement and `V$SESS_IO` for logical
//! reads), and stopping another session's statement
//! ([`dbine_driver::Session::cancel_query`]) with `ALTER SYSTEM CANCEL SQL`
//! (18c+). Ids are `sid,serial#`, as `ALTER SYSTEM KILL SESSION` takes them.

use crate::blocking::{last_statement_sql, session_id};
use crate::monitor::{num, rows, rows_max, txt};
use crate::{db_code, err};
use dbine_driver::{Error, Result, ServerProcess};
use oracledb::Connection;
use serde_json::Value;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most: a server with thousands of connections still answers fast.
const MAX_ROWS: usize = 2000;
/// Idle sessions with an open transaction whose last statement is looked up
/// (one query each, from their open cursors).
const MAX_IDLE_LOOKUPS: usize = 10;

/// One row per session of this instance (of this PDB, from a PDB). A
/// background process counts as active only while it isn't in an idle wait;
/// a user session, whenever it's in a call (a PL/SQL sleep waits in the
/// "Idle" class but is still running).
const PROCESSES: &str = "SELECT * FROM (
SELECT s.sid || ',' || s.serial#, s.status, s.username, s.machine, s.program, s.schemaname,
       (SELECT c.command_name FROM v$sqlcommand c WHERE c.command_type = s.command AND ROWNUM = 1),
       s.last_call_et * 1000,
       CASE WHEN s.state = 'WAITING' AND s.wait_class <> 'Idle' THEN s.wait_class || ': ' || s.event END,
       CASE WHEN s.blocking_session_status = 'VALID'
             AND (s.blocking_instance IS NULL OR s.blocking_instance = SYS_CONTEXT('USERENV', 'INSTANCE'))
            THEN (SELECT b.sid || ',' || b.serial# FROM v$session b WHERE b.sid = s.blocking_session) END,
       io.block_gets + io.consistent_gets,
       q.sql_fulltext,
       CASE WHEN s.status = 'ACTIVE' AND (s.type = 'USER' OR s.state <> 'WAITING' OR s.wait_class <> 'Idle')
            THEN 1 ELSE 0 END,
       CASE WHEN s.type = 'BACKGROUND' THEN 1 ELSE 0 END,
       CASE WHEN s.sid = SYS_CONTEXT('USERENV', 'SID') THEN 1 ELSE 0 END,
       CASE WHEN s.taddr IS NOT NULL THEN 1 ELSE 0 END
  FROM v$session s
  LEFT JOIN v$sess_io io ON io.sid = s.sid
  LEFT JOIN v$sql q ON q.sql_id = s.sql_id AND q.child_number = s.sql_child_number
 ORDER BY CASE WHEN s.status = 'ACTIVE' AND s.type = 'USER' THEN 0 ELSE 1 END, s.sid)
 WHERE ROWNUM <= 2000";

fn text(r: &[Value], i: usize) -> Option<String> {
    r.get(i).and_then(txt).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn count(r: &[Value], i: usize) -> Option<u64> {
    r.get(i).and_then(num).map(|v| v.max(0.0) as u64)
}

fn flag(r: &[Value], i: usize) -> bool {
    r.get(i).and_then(num) == Some(1.0)
}

fn clip(s: String) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((at, _)) => s[..at].to_string(),
        None => s,
    }
}

/// Without the grants on the V$ views (ORA-00942 / 01031 / 01039).
fn denied(e: oracledb::Error) -> Error {
    match db_code(&e) {
        Some(942 | 1031 | 1039) => Error::Query(format!(
            "Para ver los procesos hace falta leer V$SESSION, V$SQL y V$SESS_IO (SELECT_CATALOG_ROLE): {e}"
        )),
        _ => err(e),
    }
}

/// Run `f` with a call timeout, putting the previous one back after.
fn within<T>(c: &Connection, f: impl FnOnce() -> Result<T>) -> Result<T> {
    let before = c.call_timeout().ok().flatten();
    c.set_call_timeout(Some(QUERY_LIMIT)).map_err(err)?;
    let r = f();
    let _ = c.set_call_timeout(before);
    r
}

pub fn processes(c: &Connection) -> Result<Vec<ServerProcess>> {
    let found = within(c, || rows_max(c, PROCESSES, MAX_ROWS).map_err(denied))?;
    let mut list: Vec<ServerProcess> = found
        .iter()
        .map(|r| {
            let active = flag(r, 12);
            ServerProcess {
                id: text(r, 0).unwrap_or_default(),
                status: text(r, 1),
                active,
                system: flag(r, 13),
                own: flag(r, 14),
                user: text(r, 2),
                host: text(r, 3),
                program: text(r, 4),
                database: text(r, 5),
                command: text(r, 6).filter(|_| active),
                elapsed_ms: count(r, 7),
                reads: count(r, 10),
                wait: text(r, 8),
                blocked_by: text(r, 9),
                sql: text(r, 11).filter(|_| active).map(clip),
                ..Default::default()
            }
        })
        .collect();
    // An idle session in an open transaction: its last statement, as the
    // blocking view finds it (PREV_SQL_ID is often a client's housekeeping).
    let idle_in_tx: Vec<usize> = found
        .iter()
        .zip(&list)
        .enumerate()
        .filter(|(_, (r, p))| !p.active && !p.system && flag(r, 15))
        .map(|(i, _)| i)
        .take(MAX_IDLE_LOOKUPS)
        .collect();
    for i in idle_in_tx {
        let Ok((sid, _)) = session_id(&list[i].id) else { continue };
        match within(c, || rows(c, &last_statement_sql(sid)).map_err(err)) {
            Ok(r) => list[i].sql = r.first().and_then(|r| text(r, 0)),
            Err(e) => tracing::debug!("oracle processes: open cursors of {sid}: {e}"),
        }
        if list[i].status.as_deref() == Some("INACTIVE") {
            list[i].status = Some("INACTIVE (transacción abierta)".into());
        }
    }
    list.retain(|p| !p.id.is_empty());
    Ok(list)
}

pub fn cancel(c: &Connection, id: &str) -> Result<()> {
    let (sid, serial) = session_id(id)?;
    let own = rows(c, "SELECT SYS_CONTEXT('USERENV', 'SID') FROM dual").map_err(err)?;
    if own.first().and_then(|r| text(r, 0)) == Some(sid.to_string()) {
        return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá".into()));
    }
    // Another DBine session (its program is set to "DBine"): the server
    // stops the statement, but this client library never returns control to
    // it, and that tab would hang. Ending the session doesn't.
    let program = rows(c, &format!("SELECT program FROM v$session WHERE sid = {sid} AND serial# = {serial}")).map_err(err)?;
    if program.first().and_then(|r| text(r, 0)).is_some_and(|p| p.eq_ignore_ascii_case("DBine")) {
        return Err(Error::Query(format!(
            "la sesión {sid},{serial} es de DBine: cancelá la consulta desde su pestaña, o usá «Terminar sesión»"
        )));
    }
    c.execute(&format!("ALTER SYSTEM CANCEL SQL '{sid}, {serial}'"), &[]).map(|_| ()).map_err(|e| match db_code(&e) {
        Some(30) => Error::Query(format!("la sesión {sid},{serial} ya no existe")),
        Some(1031) => Error::Query(format!("tu usuario no tiene el privilegio ALTER SYSTEM para cancelar la consulta: {e}")),
        // Before 18c CANCEL SQL doesn't parse.
        Some(2000 | 905 | 922 | 2065) => {
            Error::Query(format!("cancelar una consulta (ALTER SYSTEM CANCEL SQL) requiere Oracle 18c o posterior: {e}"))
        }
        _ => err(e),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn statement_text_is_clipped_on_a_char_boundary() {
        assert_eq!(clip("ñ".repeat(MAX_TEXT + 5)).chars().count(), MAX_TEXT);
        assert_eq!(clip("SELECT 1".into()), "SELECT 1");
    }

    #[test]
    fn the_list_is_capped() {
        assert!(PROCESSES.contains(&format!("ROWNUM <= {MAX_ROWS}")));
    }
}
