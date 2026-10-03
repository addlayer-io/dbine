//! Blocking chains (`Session::blocking`) from `V$SESSION.BLOCKING_SESSION`,
//! and `ALTER SYSTEM KILL SESSION` (`Session::kill_session`). A session's id
//! is `sid,serial#`, as KILL SESSION takes it. Needs SELECT on the V$ views
//! (SELECT_CATALOG_ROLE) and, to kill, the ALTER SYSTEM privilege.

use crate::monitor::{num, rows, txt};
use crate::err;
use dbine_driver::{BlockedSession, Error, Result};
use oracledb::Connection;

const IDLE: &str = "inactiva con transacción abierta";

/// Every session waiting on another in this instance plus the ones they
/// wait for. The locked row of a row-lock wait comes as `OWNER.TABLE · rowid`.
const BLOCKING_CHAINS: &str = "WITH w AS (
    SELECT sid, blocking_session FROM v$session
     WHERE blocking_session IS NOT NULL AND blocking_session_status = 'VALID'
       AND (blocking_instance IS NULL OR blocking_instance = SYS_CONTEXT('USERENV', 'INSTANCE'))),
  ids AS (SELECT sid AS id FROM w UNION SELECT blocking_session FROM w)
SELECT s.sid || ',' || s.serial#,
       (SELECT b.sid || ',' || b.serial# FROM v$session b
         WHERE b.sid = s.blocking_session AND s.blocking_session_status = 'VALID'),
       s.username,
       s.machine || CASE WHEN s.program IS NOT NULL THEN ' · ' || s.program END,
       s.schemaname,
       CASE WHEN s.blocking_session IS NULL AND s.status = 'INACTIVE' AND s.taddr IS NOT NULL
            THEN 'inactiva con transacción abierta' ELSE s.event END,
       CASE WHEN s.state = 'WAITING' THEN ROUND(s.wait_time_micro / 1000) ELSE s.last_call_et * 1000 END,
       (SELECT o.owner || '.' || o.object_name
               || CASE WHEN s.event LIKE 'enq: TX - row lock%' AND o.data_object_id IS NOT NULL
                       THEN ' · ' || DBMS_ROWID.ROWID_CREATE(1, o.data_object_id, s.row_wait_file#,
                                                             s.row_wait_block#, s.row_wait_row#) END
          FROM all_objects o
         WHERE o.object_id = s.row_wait_obj# AND s.blocking_session IS NOT NULL),
       (SELECT DBMS_LOB.SUBSTR(q.sql_fulltext, 1000, 1) FROM v$sql q
         WHERE q.sql_id = NVL(s.sql_id, s.prev_sql_id) AND ROWNUM = 1)
  FROM ids JOIN v$session s ON s.sid = ids.id
 ORDER BY CASE WHEN s.blocking_session IS NULL THEN 1 ELSE 0 END, s.sid";

/// The last statement of an idle session, from its open cursors: its
/// PREV_SQL_ID is often a client's housekeeping call (DBine itself reads
/// DBMS_OUTPUT after each statement). The latest one, DML first on a tie.
pub(crate) fn last_statement_sql(sid: u32) -> String {
    format!(
        "SELECT * FROM (
           SELECT DBMS_LOB.SUBSTR(q.sql_fulltext, 1000, 1)
             FROM v$open_cursor oc JOIN v$sqlarea q ON q.sql_id = oc.sql_id
            WHERE oc.sid = {sid}
              AND oc.cursor_type NOT LIKE '%PL/SQL%' AND oc.cursor_type NOT LIKE '%DICTIONARY%'
              AND oc.cursor_type NOT LIKE '%RECURSIVE%'
              AND q.sql_text NOT LIKE '%DBMSOUTPUT_LINESARRAY%' AND q.sql_text NOT LIKE '%DBMS_OUTPUT.%'
            ORDER BY oc.last_sql_active_time DESC NULLS LAST,
                     CASE WHEN q.command_type IN (2, 6, 7, 189) THEN 0 ELSE 1 END)
          WHERE ROWNUM = 1"
    )
}

pub fn blocking(c: &Connection) -> Result<Vec<BlockedSession>> {
    let found = rows(c, BLOCKING_CHAINS).map_err(|e| match crate::db_code(&e) {
        Some(942 | 1031 | 1039) => Error::Query(format!(
            "Para ver los bloqueos hace falta leer V$SESSION y V$SQL (SELECT_CATALOG_ROLE): {e}"
        )),
        _ => err(e),
    })?;
    let text = |r: &[serde_json::Value], i: usize| r.get(i).and_then(txt).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut chain: Vec<BlockedSession> = found
        .iter()
        .map(|r| BlockedSession {
            id: text(r, 0).unwrap_or_default(),
            blocked_by: text(r, 1),
            user: text(r, 2),
            client: text(r, 3),
            database: text(r, 4),
            wait: text(r, 5),
            waited_ms: r.get(6).and_then(num).map(|v| v.max(0.0) as u64),
            object: text(r, 7),
            sql: text(r, 8),
        })
        .collect();
    for b in chain.iter_mut().filter(|b| b.blocked_by.is_none() && b.wait.as_deref() == Some(IDLE)) {
        let Ok((sid, _)) = session_id(&b.id) else { continue };
        match rows(c, &last_statement_sql(sid)) {
            Ok(r) => {
                if let Some(q) = r.first().and_then(|r| text(r, 0)) {
                    b.sql = Some(q);
                }
            }
            Err(e) => tracing::debug!("oracle blocking: open cursors of {sid}: {e}"),
        }
    }
    Ok(chain)
}

/// `sid,serial#`: two unsigned integers, nothing else.
pub(crate) fn session_id(id: &str) -> Result<(u32, u32)> {
    let bad = || Error::Query(format!("«{id}» no es un id de sesión de Oracle (se espera «sid,serial#»)"));
    let (sid, serial) = id.trim().split_once(',').ok_or_else(bad)?;
    let n = |s: &str| {
        let s = s.trim();
        if s.is_empty() || !s.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        s.parse::<u32>().map_err(|_| bad())
    };
    Ok((n(sid)?, n(serial)?))
}

pub fn kill(c: &Connection, id: &str) -> Result<()> {
    let (sid, serial) = session_id(id)?;
    match c.execute(&format!("ALTER SYSTEM KILL SESSION '{sid},{serial}' IMMEDIATE"), &[]) {
        Ok(_) => Ok(()),
        // ORA-00031: busy in a wait it can't leave right now; marked, it
        // ends (and rolls back) as soon as it does.
        Err(e) if crate::db_code(&e) == Some(31) => Ok(()),
        Err(e) => Err(err(e)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_sid_and_serial() {
        assert_eq!(session_id(" 12, 345 ").unwrap(), (12, 345));
        for bad in ["12", "12,", ",3", "12,3,@1", "1; DROP TABLE x", "12,3' IMMEDIATE --", "-1,2", "+1,2"] {
            assert!(session_id(bad).is_err(), "{bad}");
        }
    }
}
