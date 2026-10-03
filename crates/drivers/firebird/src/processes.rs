//! The process list ([`dbine_driver::Session::processes`]) from
//! `MON$ATTACHMENTS` with each one's running statement from
//! `MON$STATEMENTS`; cancelling it ([`dbine_driver::Session::cancel_query`])
//! by deleting the statement's row in `MON$STATEMENTS`, and ending the
//! attachment ([`dbine_driver::Session::kill_session`]) by deleting its row
//! in `MON$ATTACHMENTS`. Ids are attachment ids.
//!
//! Users without SYSDBA / RDB$ADMIN / MONITOR_ANY_ATTACHMENT see (and can
//! cancel or end) only their own user's attachments. Firebird doesn't say
//! which attachment a lock wait is on, so `blocked_by` stays empty.

use crate::monitor::in_snapshot;
use crate::{err, message, Conn};
use dbine_driver::{Error, Result, ServerProcess};
use rsfbclient_core::{
    Column, Dialect, FirebirdClientSqlOps, SqlType, TrDataAccessMode, TrIsolationLevel, TrLockResolution, TrOp,
    TransactionConfiguration,
};

/// Each attachment, with its running statement (the oldest, if it runs
/// more than one at a time: the rest are dropped). Statement figures when
/// it runs one, the attachment's otherwise.
const PROCESSES: &str = "
SELECT FIRST 2000 a.MON$ATTACHMENT_ID, a.MON$STATE, a.MON$SYSTEM_FLAG,
       CASE WHEN a.MON$ATTACHMENT_ID = CURRENT_CONNECTION THEN 1 ELSE 0 END,
       TRIM(a.MON$USER), TRIM(a.MON$REMOTE_ADDRESS), TRIM(a.MON$REMOTE_PROCESS), TRIM(a.MON$ATTACHMENT_NAME),
       DATEDIFF(MILLISECOND FROM s.MON$TIMESTAMP TO CURRENT_TIMESTAMP),
       COALESCE(sio.MON$PAGE_FETCHES, aio.MON$PAGE_FETCHES), COALESCE(sio.MON$PAGE_WRITES, aio.MON$PAGE_WRITES),
       CAST(SUBSTRING(s.MON$SQL_TEXT FROM 1 FOR 8000) AS VARCHAR(8000)), s.MON$STATE
  FROM MON$ATTACHMENTS a
  LEFT JOIN MON$STATEMENTS s ON s.MON$ATTACHMENT_ID = a.MON$ATTACHMENT_ID AND s.MON$STATE IN (1, 2)
  LEFT JOIN MON$IO_STATS aio ON aio.MON$STAT_ID = a.MON$STAT_ID
  LEFT JOIN MON$IO_STATS sio ON sio.MON$STAT_ID = s.MON$STAT_ID
 ORDER BY a.MON$STATE DESC, a.MON$ATTACHMENT_ID, s.MON$TIMESTAMP";

fn num(c: Option<&Column>) -> Option<f64> {
    match &c?.value {
        SqlType::Integer(i) => Some(*i as f64),
        SqlType::Floating(x) => Some(*x),
        SqlType::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn count(c: Option<&Column>) -> Option<u64> {
    num(c).map(|v| v.max(0.0) as u64)
}

fn text(c: Option<&Column>) -> Option<String> {
    match &c?.value {
        SqlType::Text(t) => Some(t.trim().to_string()).filter(|t| !t.is_empty()),
        SqlType::Integer(i) => Some(i.to_string()),
        _ => None,
    }
}

/// The statement's first keyword, upper case ("SELECT", "EXECUTE"…).
fn command(sql: &str) -> Option<String> {
    let w: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!w.is_empty()).then(|| w.to_ascii_uppercase())
}

/// The client program's file name, without its folder.
fn program(path: String) -> String {
    path.rsplit(['/', '\\']).next().filter(|p| !p.is_empty()).map(str::to_string).unwrap_or(path)
}

pub(crate) fn assemble(rows: Vec<Vec<Column>>) -> Vec<ServerProcess> {
    let mut out: Vec<ServerProcess> = Vec::new();
    for r in rows {
        let at = |i: usize| r.get(i);
        let Some(id) = count(at(0)).map(|n| n.to_string()) else { continue };
        if out.last().is_some_and(|p| p.id == id) {
            continue;
        }
        let sql = text(at(11));
        // MON$STATE: 0 idle, 1 running, 2 stalled (a cursor not fetched to
        // the end, FB 3+).
        let (status, active) = match (count(at(1)), count(at(12))) {
            (_, Some(2)) => ("detenida", false),
            (Some(1), _) => ("activa", true),
            _ => ("inactiva", false),
        };
        out.push(ServerProcess {
            id,
            status: Some(status.into()),
            active,
            system: count(at(2)) == Some(1),
            own: count(at(3)) == Some(1),
            user: text(at(4)),
            host: text(at(5)),
            program: text(at(6)).map(program),
            database: text(at(7)),
            command: sql.as_deref().filter(|_| active).and_then(command),
            elapsed_ms: count(at(8)).filter(|_| sql.is_some()),
            reads: count(at(9)),
            writes: count(at(10)),
            sql,
            ..Default::default()
        });
    }
    out
}

pub fn processes(c: &mut Conn) -> Result<Vec<ServerProcess>> {
    let rows = in_snapshot(c, &[PROCESSES])
        .map_err(|e| Error::Query(message(&e)))?
        .pop()
        .unwrap_or_else(|| Ok(Vec::new()))
        .map_err(|e| Error::Query(format!("No se pudieron leer las conexiones (MON$ATTACHMENTS): {e}")))?;
    Ok(assemble(rows))
}

/// An attachment id: an unsigned integer, nothing else.
pub(crate) fn attachment_id(id: &str) -> Result<i64> {
    let t = id.trim();
    let bad = || Error::Query(format!("«{id}» no es un id de conexión de Firebird"));
    if t.is_empty() || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    t.parse().map_err(|_| bad())
}

/// Delete from a MON$ table in a short transaction of its own, so the
/// session's own transaction (autocommit off) is left alone. The monitoring
/// snapshot of that transaction says first whether the attachment is there
/// (and visible to this user).
fn delete(c: &mut Conn, id: i64, sql: &str) -> Result<()> {
    let conf = TransactionConfiguration {
        data_access: TrDataAccessMode::ReadWrite,
        isolation: TrIsolationLevel::Concurrency,
        lock_resolution: TrLockResolution::NoWait,
    };
    let mut tr = c.client.begin_transaction(&mut c.db, conf).map_err(err)?;
    let r = (|| {
        let (_, mut stmt) = c
            .client
            .prepare_statement(&mut c.db, &mut tr, Dialect::D3, &format!("SELECT COUNT(*) FROM MON$ATTACHMENTS WHERE MON$ATTACHMENT_ID = {id}"))
            .map_err(err)?;
        let found = (|| {
            c.client.execute(&mut c.db, &mut tr, &mut stmt, vec![])?;
            c.client.fetch(&mut c.db, &mut tr, &mut stmt)
        })();
        let _ = c.client.free_statement(&mut stmt, rsfbclient_core::FreeStmtOp::Drop);
        if found.map_err(err)?.and_then(|r| count(r.first())) != Some(1) {
            return Err(Error::Query(format!(
                "la conexión {id} no existe o tu usuario no la ve (hace falta SYSDBA, RDB$ADMIN o MONITOR_ANY_ATTACHMENT para las de otros usuarios)"
            )));
        }
        c.client.exec_immediate(&mut c.db, &mut tr, Dialect::D3, sql).map_err(err)
    })();
    let op = if r.is_ok() { TrOp::Commit } else { TrOp::Rollback };
    if let Err(e) = c.client.transaction_operation(&mut tr, op) {
        return r.and(Err(err(e)));
    }
    r
}

pub fn cancel(c: &mut Conn, own: i64, id: &str) -> Result<()> {
    let id = attachment_id(id)?;
    if id == own {
        return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá".into()));
    }
    delete(c, id, &format!("DELETE FROM MON$STATEMENTS WHERE MON$ATTACHMENT_ID = {id} AND MON$STATE = 1"))
}

pub fn kill(c: &mut Conn, own: i64, id: &str) -> Result<()> {
    let id = attachment_id(id)?;
    if id == own {
        return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede cerrar desde acá".into()));
    }
    delete(c, id, &format!("DELETE FROM MON$ATTACHMENTS WHERE MON$ATTACHMENT_ID = {id}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(v: SqlType) -> Column {
        Column::new(String::new(), 0, v)
    }

    fn row(vals: Vec<SqlType>) -> Vec<Column> {
        vals.into_iter().map(col).collect()
    }

    #[test]
    fn ids_are_numbers() {
        assert_eq!(attachment_id(" 42 ").unwrap(), 42);
        for bad in ["", "1; DROP TABLE x", "1 OR 1=1", "-1", "+1"] {
            assert!(attachment_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn rows_become_processes() {
        use SqlType::{Integer as I, Null as N, Text as T};
        let list = assemble(vec![
            row(vec![I(7), I(1), I(0), I(0), T("DBINE".into()), T("172.17.0.1/5000".into()), T("/opt/app/dbine".into()),
                     T("/data/test.fdb".into()), I(1500), I(90), I(3), T(" select 1 from rdb$database".into()), I(1)]),
            // A second statement of the same attachment.
            row(vec![I(7), I(1), I(0), I(0), N, N, N, N, I(10), N, N, T("SELECT 2".into()), I(1)]),
            row(vec![I(8), I(0), I(0), I(1), T("SYSDBA".into()), N, N, N, N, I(5), I(0), N, N]),
            row(vec![I(2), I(0), I(1), I(0), T("Garbage Collector".into()), N, N, N, N, N, N, N, N]),
        ]);
        assert_eq!(list.len(), 3);
        let w = &list[0];
        assert!(w.active && w.elapsed_ms == Some(1500) && w.command.as_deref() == Some("SELECT"));
        assert_eq!(w.program.as_deref(), Some("dbine"));
        assert!(!list[1].active && list[1].own && list[1].sql.is_none() && list[1].elapsed_ms.is_none());
        assert!(list[2].system);
    }
}
