//! The process list ([`dbine_driver::Session::processes`]) and stopping
//! another session's statement ([`dbine_driver::Session::cancel_query`])
//! per preset, from the same system views the monitor reads:
//!
//! - Db2 LUW: `MON_GET_CONNECTION` + `MON_CURRENT_SQL` (+ `MON_LOCKWAITS`);
//!   cancels with `WLM_CANCEL_ACTIVITY` on the activities `MON_GET_ACTIVITY`
//!   lists. Ids are application handles, as FORCE APPLICATION takes them.
//! - Db2 for i: `QSYS2.ACTIVE_JOB_INFO` on the database server jobs
//!   (QZDASOINIT); cancels with `QSYS2.CANCEL_SQL`. Ids are job names
//!   (`123456/QUSER/QZDASOINIT`).
//! - Sybase ASE: `master..sysprocesses`, the running batch's text from
//!   `monProcessSQLText` (MDA) when monitoring is on. No cancel: ASE only
//!   ends a whole session (KILL).
//! - SQL Anywhere: `sa_conn_info()` with `ReqStatus` and `LastStatement`.
//!   No cancel of another connection's request, only DROP CONNECTION.
//! - Teradata: `MonitorSession` (MONITOR SESSION privilege). The request
//!   text needs one `MonitorSQLText` call per session, so it's left out;
//!   aborting a request needs the PM/API's host id: no cancel.
//! - Vertica: `v_monitor.sessions`; `INTERRUPT_STATEMENT` cancels and
//!   `CLOSE_SESSION` ends a session. Ids are Vertica's session ids
//!   (`node-1234:0x5a`).
//! - Exasol: `EXA_DBA_SESSIONS` (else `EXA_ALL_SESSIONS`); `KILL STATEMENT
//!   IN SESSION` cancels, `KILL SESSION` ends.
//! - Netezza: `_V_SESSION`; `DROP SESSION` ends a session, no cancel.
//! - Dameng: `V$SESSIONS`; `SP_CANCEL_SESSION_OPERATION` cancels,
//!   `SP_CLOSE_SESSION` ends.
//! - Altibase: `V$SESSION` + `V$STATEMENT`; no cancel by SQL.
//!
//! Everything goes through [`Source`], so it's tested with canned sets.

use crate::design::{eng, Eng};
use crate::monitor::{parse, Source};
use crate::presets::Preset;
use dbine_driver::{Error, Result, ServerProcess};

/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;
/// Sessions whose running batch text is looked up in ASE's MDA tables.
const MAX_TEXT_LOOKUPS: usize = 200;

const OWN: &str = "esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá";

/// Why a preset has no process list, in Spanish (for the error and the docs).
pub fn unsupported_reason(p: &Preset) -> Option<&'static str> {
    match eng(p) {
        Eng::Db2 | Eng::Db2i | Eng::Ase | Eng::Sqla | Eng::Teradata | Eng::Vertica | Eng::Exasol | Eng::Netezza | Eng::Dameng | Eng::Altibase => None,
        Eng::Generic => Some("el preset ODBC genérico no conoce las vistas de sesiones del motor: usá el preset del motor"),
        Eng::Db2zos => Some("Db2 for z/OS no expone sus hilos por SQL: se ven con -DISPLAY THREAD, IFI u OMEGAMON"),
        Eng::Access | Eng::DBase => Some("es una base de archivos sin servidor: no tiene sesiones que listar"),
        Eng::Spark => Some("Spark Thrift Server y Kyuubi no exponen sus sesiones por SQL: se ven en la interfaz web de Spark"),
        Eng::Zen => Some("Actian Zen no expone sus sesiones por SQL: se ven en Zen Monitor"),
        Eng::NetSuite => Some("SuiteAnalytics Connect es un servicio de solo lectura de NetSuite: no informa sesiones"),
        Eng::Mimer => Some("Mimer SQL no expone sus sesiones por SQL: se ven con sqlmonitor"),
        Eng::Hive => Some("HiveServer2 no lista sus sesiones por SQL: se ven en su interfaz web (puerto 10002)"),
        _ => Some("este motor todavía no lista sus procesos en DBine"),
    }
}

pub fn supports(p: &Preset) -> bool {
    unsupported_reason(p).is_none()
}

/// The preset cancels another session's statement and keeps the session.
pub fn supports_cancel(p: &Preset) -> bool {
    matches!(eng(p), Eng::Db2 | Eng::Db2i | Eng::Vertica | Eng::Exasol | Eng::Dameng)
}

fn owned(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

fn clip(s: &str) -> String {
    match s.char_indices().nth(MAX_TEXT) {
        Some((at, _)) => s[..at].to_string(),
        None => s.to_string(),
    }
}

fn count(s: Option<&str>) -> Option<u64> {
    s.and_then(parse).map(|v| v.max(0.0) as u64)
}

/// A positive integer id, nothing else.
fn int(id: &str) -> Option<u64> {
    let t = id.trim();
    (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten().filter(|n| *n > 0)
}

fn numeric_id(id: &str) -> Result<u64> {
    int(id).ok_or_else(|| Error::Query(format!("«{id}» no es un id de sesión válido")))
}

/// An IBM i job name: `number/user/job`, the number six digits and each
/// name up to ten characters of the system's name alphabet.
pub fn job_name(id: &str) -> Result<String> {
    let bad = || Error::Query(format!("«{id}» no es un nombre de trabajo de IBM i (se espera «123456/USUARIO/TRABAJO»)"));
    let t = id.trim().to_ascii_uppercase();
    let parts: Vec<&str> = t.split('/').collect();
    let name = |s: &str| (1..=10).contains(&s.len()) && s.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'$' | b'#' | b'@' | b'.'));
    match parts.as_slice() {
        [n, u, j] if n.len() == 6 && n.bytes().all(|b| b.is_ascii_digit()) && name(u) && name(j) => Ok(t),
        _ => Err(bad()),
    }
}

/// A Vertica session id (`v_db_node0001-12345:0x1a2b`).
pub fn vertica_session(id: &str) -> Result<String> {
    let t = id.trim();
    let ok = !t.is_empty() && t.len() <= 128 && t.bytes().all(|b| b.is_ascii_alphanumeric() || matches!(b, b'_' | b'-' | b':' | b'.'));
    ok.then(|| t.to_string()).ok_or_else(|| Error::Query(format!("«{id}» no es un id de sesión de Vertica")))
}

/// The statement's first keyword, upper case ("SELECT", "CALL"…).
fn command(sql: &str) -> Option<String> {
    let w: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!w.is_empty()).then(|| w.to_ascii_uppercase())
}

/// "HHH:MM:SS" (Exasol's DURATION) in ms.
fn hms_ms(s: Option<&str>) -> Option<u64> {
    let parts: Vec<u64> = s?.trim().split(':').map(|p| p.trim().parse().ok()).collect::<Option<_>>()?;
    match parts.as_slice() {
        [h, m, s] => Some((h * 3600 + m * 60 + s) * 1000),
        [m, s] => Some((m * 60 + s) * 1000),
        _ => None,
    }
}

/// Keep the first row of each id (a session with several running
/// activities comes once), skip rows without one, cap the list.
fn dedup(list: Vec<ServerProcess>) -> Vec<ServerProcess> {
    let mut out: Vec<ServerProcess> = Vec::new();
    for p in list {
        if !p.id.is_empty() && !out.iter().any(|o| o.id == p.id) {
            out.push(p);
        }
        if out.len() >= MAX_ROWS {
            break;
        }
    }
    out
}

fn denied(what: &str, e: Error) -> Error {
    Error::Query(format!("No se pudieron leer los procesos ({what}): {e}"))
}

pub fn collect(p: &Preset, src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    if let Some(why) = unsupported_reason(p) {
        return Err(Error::Unsupported(why.into()));
    }
    let list = match eng(p) {
        Eng::Db2 => db2(src)?,
        Eng::Db2i => db2i(src)?,
        Eng::Ase => ase(src)?,
        Eng::Sqla => sqla(src)?,
        Eng::Teradata => teradata(src)?,
        Eng::Vertica => vertica(src)?,
        Eng::Exasol => exasol(src)?,
        Eng::Netezza => netezza(src)?,
        Eng::Dameng => dameng(src)?,
        Eng::Altibase => altibase(src)?,
        _ => Vec::new(),
    };
    Ok(dedup(list))
}

// ------------------------------------------------------------------- Db2

const DB2: &str = "SELECT C.APPLICATION_HANDLE AS ID, C.SESSION_AUTH_ID AS USR, C.CLIENT_HOSTNAME AS HOST,
       C.APPLICATION_NAME AS PROG, CURRENT SERVER AS DB, COALESCE(Q.ACTIVITY_STATE, 'IDLE') AS STATE,
       Q.ELAPSED_TIME_SEC AS SECS, C.TOTAL_CPU_TIME / 1000 AS CPU_MS, C.ROWS_READ AS READS, C.ROWS_MODIFIED AS WRITES,
       W.HLD AS BLOCKER, W.MODE AS WAIT_MODE,
       CAST(SUBSTR(Q.STMT_TEXT, 1, 20000) AS VARCHAR(20000)) AS STMT,
       CASE WHEN C.APPLICATION_HANDLE = MON_GET_APPLICATION_HANDLE() THEN 1 ELSE 0 END AS OWN
  FROM TABLE(MON_GET_CONNECTION(NULL, -1)) AS C
  LEFT JOIN SYSIBMADM.MON_CURRENT_SQL Q ON Q.APPLICATION_HANDLE = C.APPLICATION_HANDLE
  LEFT JOIN (SELECT REQ_APPLICATION_HANDLE AS REQ, MIN(HLD_APPLICATION_HANDLE) AS HLD, MIN(LOCK_MODE_REQUESTED) AS MODE
               FROM SYSIBMADM.MON_LOCKWAITS GROUP BY REQ_APPLICATION_HANDLE) W ON W.REQ = C.APPLICATION_HANDLE
 ORDER BY Q.ELAPSED_TIME_SEC DESC NULLS LAST, C.APPLICATION_HANDLE
 FETCH FIRST 2000 ROWS ONLY";

fn db2(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(DB2).map_err(|e| denied("MON_GET_CONNECTION", e))?;
    Ok((0..s.rows.len())
        .map(|r| {
            let state = s.get(r, "STATE");
            let active = state.is_some_and(|v| !v.eq_ignore_ascii_case("IDLE"));
            let sql = s.get(r, "STMT").filter(|_| active).map(clip);
            ServerProcess {
                id: s.get(r, "ID").and_then(int).map(|n| n.to_string()).unwrap_or_default(),
                status: owned(state),
                active,
                own: s.get(r, "OWN") == Some("1"),
                user: owned(s.get(r, "USR")),
                host: owned(s.get(r, "HOST")),
                program: owned(s.get(r, "PROG")),
                database: owned(s.get(r, "DB")),
                command: sql.as_deref().and_then(command),
                elapsed_ms: count(s.get(r, "SECS")).map(|v| v * 1000).filter(|_| active),
                cpu_ms: count(s.get(r, "CPU_MS")),
                reads: count(s.get(r, "READS")),
                writes: count(s.get(r, "WRITES")),
                wait: s.get(r, "WAIT_MODE").map(|m| format!("LOCK WAIT {m}")),
                blocked_by: s.get(r, "BLOCKER").and_then(int).map(|n| n.to_string()),
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// ------------------------------------------------------------- Db2 for i

const DB2I: &str = "SELECT JOB_NAME, AUTHORIZATION_NAME, CLIENT_IP_ADDRESS, CLIENT_APPLNAME, JOB_STATUS,
       SQL_STATEMENT_STATUS, CPU_TIME, TOTAL_DISK_IO_COUNT,
       TIMESTAMPDIFF(2, CHAR(CURRENT TIMESTAMP - SQL_STATEMENT_START_TIMESTAMP)) AS SECS,
       CAST(SUBSTR(SQL_STATEMENT_TEXT, 1, 20000) AS VARCHAR(20000)) AS SQL_TEXT,
       CASE WHEN JOB_NAME = QSYS2.JOB_NAME THEN 1 ELSE 0 END AS OWN
  FROM TABLE(QSYS2.ACTIVE_JOB_INFO(JOB_NAME_FILTER => 'QZDASOINIT', DETAILED_INFO => 'ALL')) X
 WHERE JOB_STATUS <> 'PSRW'
 FETCH FIRST 2000 ROWS ONLY";

fn db2i(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(DB2I).map_err(|e| denied("QSYS2.ACTIVE_JOB_INFO", e))?;
    Ok((0..s.rows.len())
        .map(|r| {
            let active = s.get(r, "SQL_STATEMENT_STATUS").is_some_and(|v| v.eq_ignore_ascii_case("ACTIVE"));
            let sql = s.get(r, "SQL_TEXT").filter(|_| active).map(clip);
            ServerProcess {
                id: s.get(r, "JOB_NAME").and_then(|j| job_name(j).ok()).unwrap_or_default(),
                status: owned(s.get(r, "JOB_STATUS")),
                active,
                own: s.get(r, "OWN") == Some("1"),
                user: owned(s.get(r, "AUTHORIZATION_NAME")),
                host: owned(s.get(r, "CLIENT_IP_ADDRESS")),
                program: owned(s.get(r, "CLIENT_APPLNAME")),
                command: sql.as_deref().and_then(command),
                elapsed_ms: count(s.get(r, "SECS")).map(|v| v * 1000).filter(|_| active),
                cpu_ms: count(s.get(r, "CPU_TIME")),
                reads: count(s.get(r, "TOTAL_DISK_IO_COUNT")),
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// ------------------------------------------------------------ Sybase ASE

const ASE: &str = "SELECT TOP 2000 spid, suser_name(suid) AS usr, hostname, program_name, db_name(dbid) AS db,
       status, cmd, blocked, time_blocked, suid, CASE WHEN spid = @@spid THEN 1 ELSE 0 END AS own
  FROM master..sysprocesses ORDER BY spid";

fn ase(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(ASE).map_err(|e| denied("master..sysprocesses", e))?;
    let mut list: Vec<ServerProcess> = (0..s.rows.len())
        .map(|r| {
            let status = s.get(r, "status");
            let cmd = s.get(r, "cmd");
            let system = s.get(r, "suid") == Some("0");
            // A user session between batches says AWAITING COMMAND.
            let active = !system && cmd.is_some_and(|c| !c.eq_ignore_ascii_case("AWAITING COMMAND"));
            let blocked_by = s.get(r, "blocked").and_then(int).map(|n| n.to_string());
            ServerProcess {
                id: s.get(r, "spid").and_then(int).map(|n| n.to_string()).unwrap_or_default(),
                status: owned(status),
                active,
                system,
                own: s.get(r, "own") == Some("1"),
                user: owned(s.get(r, "usr")),
                host: owned(s.get(r, "hostname")),
                program: owned(s.get(r, "program_name")),
                database: owned(s.get(r, "db")),
                command: owned(cmd).filter(|_| active),
                elapsed_ms: blocked_by.as_ref().and(count(s.get(r, "time_blocked")).map(|v| v * 1000)),
                wait: status.filter(|v| active && v.to_ascii_lowercase().contains("sleep")).map(str::to_string),
                blocked_by,
                ..Default::default()
            }
        })
        .collect();
    // The running batch's text: MDA tables, only with monitoring enabled.
    let ids: Vec<String> = list.iter().filter(|p| p.active && !p.own).map(|p| p.id.clone()).take(MAX_TEXT_LOOKUPS).collect();
    if !ids.is_empty() {
        let sql = format!(
            "SELECT SPID, SQLText FROM master..monProcessSQLText WHERE SPID IN ({})
              ORDER BY SPID, BatchID, LineNumber, SequenceInLine",
            ids.join(",")
        );
        if let Ok(t) = src.query(&sql) {
            for r in 0..t.rows.len() {
                let (Some(id), Some(text)) = (t.get(r, "SPID"), t.rows[r].get(1).cloned().flatten()) else { continue };
                if let Some(p) = list.iter_mut().find(|p| p.id == id) {
                    let s = p.sql.get_or_insert_with(String::new);
                    if s.chars().count() < MAX_TEXT {
                        s.push_str(&text);
                    }
                }
            }
            for p in &mut list {
                p.sql = p.sql.take().map(|s| clip(&s));
            }
        }
    }
    Ok(list)
}

// ---------------------------------------------------------- SQL Anywhere

const SQLA: &str = "SELECT TOP 2000 Number, Userid, NodeAddr, Name, DB_NAME(DBNumber) AS db, ReqStatus, BlockedOn,
       UncommitOps, LockTable,
       CAST(LEFT(CONNECTION_PROPERTY('LastStatement', Number), 20000) AS LONG VARCHAR) AS stmt,
       CASE WHEN Number = CONNECTION_PROPERTY('Number') THEN 1 ELSE 0 END AS own
  FROM sa_conn_info() ORDER BY Number";

fn sqla(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(SQLA).map_err(|e| denied("sa_conn_info", e))?;
    Ok((0..s.rows.len())
        .map(|r| {
            let status = s.get(r, "ReqStatus");
            let active = status.is_some_and(|v| !v.eq_ignore_ascii_case("Idle"));
            let open_tx = count(s.get(r, "UncommitOps")).is_some_and(|n| n > 0);
            let sql = s.get(r, "stmt").filter(|_| active || open_tx).map(clip);
            let blocked_by = s.get(r, "BlockedOn").and_then(int).map(|n| n.to_string());
            ServerProcess {
                id: s.get(r, "Number").and_then(int).map(|n| n.to_string()).unwrap_or_default(),
                status: owned(status),
                active,
                // Internal connections (events, the cleaner…) are named "INT: …".
                system: s.get(r, "Name").is_some_and(|n| n.starts_with("INT:")),
                own: s.get(r, "own") == Some("1"),
                user: owned(s.get(r, "Userid")),
                host: owned(s.get(r, "NodeAddr")),
                program: owned(s.get(r, "Name")),
                database: owned(s.get(r, "db")),
                command: sql.as_deref().filter(|_| active).and_then(command),
                wait: status.filter(|v| v.to_ascii_lowercase().starts_with("blocked")).map(|v| match s.get(r, "LockTable") {
                    Some(t) if blocked_by.is_some() => format!("{v} · {t}"),
                    _ => v.to_string(),
                }),
                blocked_by,
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// --------------------------------------------------------------- Teradata

const TERADATA: &str = "SELECT t.*, SESSION AS DBINE_OWN FROM TABLE (MonitorSession(-1, '*', 0)) AS t";

fn teradata(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(TERADATA).map_err(|e| denied("MonitorSession, hace falta el permiso MONITOR SESSION", e))?;
    Ok((0..s.rows.len())
        .take(MAX_ROWS)
        .map(|r| {
            let pe = s.get(r, "PEState");
            let active = pe.is_some_and(|v| !v.to_ascii_uppercase().starts_with("IDLE"));
            let id = s.get(r, "SessionNo").and_then(int).map(|n| n.to_string()).unwrap_or_default();
            let blocked_by = s.get(r, "Blk_1_SessNo").and_then(int).map(|n| n.to_string());
            ServerProcess {
                own: s.get(r, "DBINE_OWN").and_then(int).map(|n| n.to_string()) == Some(id.clone()),
                id,
                status: match (pe, s.get(r, "AMPState")) {
                    (Some(p), Some(a)) => Some(format!("{p} / {a}")),
                    (p, a) => owned(p.or(a)),
                },
                active,
                user: owned(s.get(r, "UserName")),
                host: owned(s.get(r, "LogonSource").or(s.get(r, "HostId"))),
                database: owned(s.get(r, "DefaultDataBase")),
                cpu_ms: s.get(r, "AMPCPUSec").and_then(parse).map(|v| (v.max(0.0) * 1000.0) as u64),
                reads: count(s.get(r, "AMPIO")),
                wait: blocked_by.as_ref().and(owned(s.get(r, "Blk_1_LMode").or(s.get(r, "Blk_1_ObjType")))),
                blocked_by,
                ..Default::default()
            }
        })
        .collect())
}

// ---------------------------------------------------------------- Vertica

const VERTICA: &str = "SELECT session_id, user_name, client_hostname, client_label, client_type, node_name,
       CASE WHEN current_statement <> '' THEN 1 ELSE 0 END AS running,
       DATEDIFF('millisecond', statement_start, NOW()) AS ms, LEFT(current_statement, 20000) AS stmt,
       CASE WHEN session_id = (SELECT session_id FROM v_monitor.current_session) THEN 1 ELSE 0 END AS own
  FROM v_monitor.sessions ORDER BY statement_start LIMIT 2000";

fn vertica(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(VERTICA).map_err(|e| denied("v_monitor.sessions", e))?;
    Ok((0..s.rows.len())
        .map(|r| {
            let active = s.get(r, "running") == Some("1");
            let sql = s.get(r, "stmt").filter(|_| active).map(clip);
            ServerProcess {
                id: s.get(r, "session_id").and_then(|v| vertica_session(v).ok()).unwrap_or_default(),
                status: Some(if active { "activa" } else { "inactiva" }.into()),
                active,
                own: s.get(r, "own") == Some("1"),
                user: owned(s.get(r, "user_name")),
                host: owned(s.get(r, "client_hostname")),
                program: owned(s.get(r, "client_label").or(s.get(r, "client_type"))),
                database: owned(s.get(r, "node_name")),
                command: sql.as_deref().and_then(command),
                elapsed_ms: count(s.get(r, "ms")).filter(|_| active),
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// ----------------------------------------------------------------- Exasol

fn exasol(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    // Without DBA rights only the user's own sessions are visible.
    let s = src
        .query("SELECT s.*, CURRENT_SESSION AS DBINE_OWN FROM EXA_DBA_SESSIONS s")
        .or_else(|_| src.query("SELECT s.*, CURRENT_SESSION AS DBINE_OWN FROM EXA_ALL_SESSIONS s"))
        .map_err(|e| denied("EXA_ALL_SESSIONS", e))?;
    Ok((0..s.rows.len())
        .take(MAX_ROWS)
        .map(|r| {
            let status = s.get(r, "STATUS");
            let active = status.is_some_and(|v| !v.eq_ignore_ascii_case("IDLE"));
            let id = s.get(r, "SESSION_ID").and_then(int).map(|n| n.to_string()).unwrap_or_default();
            let sql = s.get(r, "SQL_TEXT").filter(|_| active).map(clip);
            ServerProcess {
                own: s.get(r, "DBINE_OWN").and_then(int).map(|n| n.to_string()) == Some(id.clone()),
                id,
                status: owned(status),
                active,
                // Session 4 is the database's own (system) session.
                system: s.get(r, "SESSION_ID") == Some("4"),
                user: owned(s.get(r, "USER_NAME")),
                host: owned(s.get(r, "HOST")),
                program: owned(s.get(r, "CLIENT")),
                database: owned(s.get(r, "SCOPE_SCHEMA")),
                command: owned(s.get(r, "COMMAND_NAME")).filter(|_| active).or_else(|| sql.as_deref().and_then(command)),
                elapsed_ms: hms_ms(s.get(r, "DURATION")),
                wait: status.filter(|v| v.to_ascii_uppercase().contains("WAIT")).map(str::to_string),
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// ---------------------------------------------------------------- Netezza

const NETEZZA: &str = "SELECT ID, USERNAME, DBNAME, IPADDR, STATUS, TYPE, SUBSTR(COMMAND, 1, 20000) AS CMD,
       CASE WHEN ID = CURRENT_SID THEN 1 ELSE 0 END AS OWN
  FROM _V_SESSION ORDER BY ID LIMIT 2000";

fn netezza(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(NETEZZA).map_err(|e| denied("_V_SESSION", e))?;
    Ok((0..s.rows.len())
        .map(|r| {
            let status = s.get(r, "STATUS");
            let active = status.is_some_and(|v| v.eq_ignore_ascii_case("active"));
            let sql = s.get(r, "CMD").filter(|_| active).map(clip);
            ServerProcess {
                id: s.get(r, "ID").and_then(int).map(|n| n.to_string()).unwrap_or_default(),
                status: owned(status),
                active,
                own: s.get(r, "OWN") == Some("1"),
                user: owned(s.get(r, "USERNAME")),
                host: owned(s.get(r, "IPADDR")),
                program: owned(s.get(r, "TYPE")),
                database: owned(s.get(r, "DBNAME")),
                command: sql.as_deref().and_then(command),
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// ----------------------------------------------------------------- Dameng

const DAMENG: &str = "SELECT s.*, SESSID() AS DBINE_OWN FROM V$SESSIONS s LIMIT 2000";

fn dameng(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(DAMENG).map_err(|e| denied("V$SESSIONS", e))?;
    Ok((0..s.rows.len())
        .map(|r| {
            let state = s.get(r, "STATE");
            let active = state.is_some_and(|v| v.eq_ignore_ascii_case("ACTIVE"));
            let id = s.get(r, "SESS_ID").and_then(int).map(|n| n.to_string()).unwrap_or_default();
            let sql = s.get(r, "SQL_TEXT").filter(|_| active).map(clip);
            ServerProcess {
                own: s.get(r, "DBINE_OWN").and_then(int).map(|n| n.to_string()) == Some(id.clone()),
                id,
                status: owned(state),
                active,
                user: owned(s.get(r, "USER_NAME")),
                host: owned(s.get(r, "CLNT_IP").or(s.get(r, "CLNT_HOST"))),
                program: owned(s.get(r, "APPNAME")),
                database: owned(s.get(r, "CURR_SCH")),
                command: sql.as_deref().and_then(command),
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// --------------------------------------------------------------- Altibase

const ALTIBASE: &str = "SELECT s.ID, s.DB_USERNAME, s.COMM_NAME, s.CLIENT_APP_INFO, s.SESSION_STATE,
       t.EXECUTE_FLAG, t.TOTAL_TIME, t.QUERY, SESSION_ID() AS DBINE_OWN
  FROM V$SESSION s LEFT JOIN V$STATEMENT t ON t.SESSION_ID = s.ID AND t.ID = s.CURRENT_STMT_ID
 LIMIT 2000";

fn altibase(src: &mut dyn Source) -> Result<Vec<ServerProcess>> {
    let s = src.query(ALTIBASE).map_err(|e| denied("V$SESSION", e))?;
    Ok((0..s.rows.len())
        .map(|r| {
            let active = s.get(r, "EXECUTE_FLAG") == Some("1");
            let id = s.get(r, "ID").and_then(int).map(|n| n.to_string()).unwrap_or_default();
            let sql = s.get(r, "QUERY").filter(|_| active).map(clip);
            ServerProcess {
                own: s.get(r, "DBINE_OWN").and_then(int).map(|n| n.to_string()) == Some(id.clone()),
                id,
                status: owned(s.get(r, "SESSION_STATE")),
                active,
                user: owned(s.get(r, "DB_USERNAME")),
                host: owned(s.get(r, "COMM_NAME")),
                program: owned(s.get(r, "CLIENT_APP_INFO")),
                command: sql.as_deref().and_then(command),
                // TOTAL_TIME is in microseconds.
                elapsed_ms: count(s.get(r, "TOTAL_TIME")).map(|us| us / 1000).filter(|_| active),
                sql,
                ..Default::default()
            }
        })
        .collect())
}

// ----------------------------------------------------------------- cancel

/// The first column of the first row.
fn scalar(src: &mut dyn Source, sql: &str) -> Result<Option<String>> {
    Ok(src.query(sql)?.at(0, 0).map(str::to_string))
}

pub fn cancel(p: &Preset, src: &mut dyn Source, id: &str) -> Result<()> {
    if !supports_cancel(p) {
        return Err(Error::Unsupported(match unsupported_reason(p) {
            Some(why) => why.to_string(),
            None => "este motor no cancela la sentencia de otra sesión sin cerrarla".into(),
        }));
    }
    match eng(p) {
        Eng::Db2 => {
            let n = numeric_id(id)?;
            if scalar(src, "SELECT MON_GET_APPLICATION_HANDLE() FROM SYSIBM.SYSDUMMY1")?.as_deref().and_then(int) == Some(n) {
                return Err(Error::Query(OWN.into()));
            }
            let acts = src.query(&format!("SELECT UOW_ID, ACTIVITY_ID FROM TABLE(MON_GET_ACTIVITY({n}, -2)) AS A"))?;
            let ids: Vec<(u64, u64)> =
                (0..acts.rows.len()).filter_map(|r| Some((acts.get(r, "UOW_ID").and_then(int)?, acts.get(r, "ACTIVITY_ID").and_then(int)?))).collect();
            if ids.is_empty() {
                return Err(Error::Query(format!("la sesión {n} no está ejecutando nada")));
            }
            for (uow, act) in ids {
                src.query(&format!("CALL WLM_CANCEL_ACTIVITY({n}, {uow}, {act})"))?;
            }
            Ok(())
        }
        Eng::Db2i => {
            let job = job_name(id)?;
            if scalar(src, "SELECT QSYS2.JOB_NAME FROM SYSIBM.SYSDUMMY1")?.map(|j| j.to_ascii_uppercase()).as_deref() == Some(job.as_str()) {
                return Err(Error::Query(OWN.into()));
            }
            src.query(&format!("CALL QSYS2.CANCEL_SQL('{job}')")).map(|_| ())
        }
        Eng::Vertica => {
            let sid = vertica_session(id)?;
            let own = scalar(src, "SELECT session_id FROM v_monitor.current_session")?;
            if own.as_deref() == Some(sid.as_str()) {
                return Err(Error::Query(OWN.into()));
            }
            let stmt = scalar(src, &format!("SELECT statement_id FROM v_monitor.sessions WHERE session_id = '{sid}'"))?
                .as_deref()
                .and_then(int)
                .ok_or_else(|| Error::Query(format!("la sesión {sid} no existe o no está ejecutando nada")))?;
            src.query(&format!("SELECT INTERRUPT_STATEMENT('{sid}', {stmt})")).map(|_| ())
        }
        Eng::Exasol => {
            let n = numeric_id(id)?;
            if scalar(src, "SELECT CURRENT_SESSION")?.as_deref().and_then(int) == Some(n) {
                return Err(Error::Query(OWN.into()));
            }
            src.query(&format!("KILL STATEMENT IN SESSION {n}")).map(|_| ())
        }
        Eng::Dameng => {
            let n = numeric_id(id)?;
            if scalar(src, "SELECT SESSID()")?.as_deref().and_then(int) == Some(n) {
                return Err(Error::Query(OWN.into()));
            }
            src.query(&format!("CALL SP_CANCEL_SESSION_OPERATION({n})")).map(|_| ())
        }
        _ => Err(Error::Unsupported("este motor no cancela la sentencia de otra sesión sin cerrarla".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::monitor::Set;
    use crate::presets::PRESETS;

    fn preset(id: &str) -> &'static Preset {
        PRESETS.iter().find(|p| p.id == id).unwrap()
    }

    /// Answers the first query containing a needle (recording every query);
    /// any other fails.
    struct Fake {
        answers: Vec<(&'static str, Set)>,
        seen: Vec<String>,
    }

    impl Fake {
        fn new(answers: Vec<(&'static str, Set)>) -> Fake {
            Fake { answers, seen: Vec::new() }
        }
    }

    impl Source for Fake {
        fn query(&mut self, sql: &str) -> Result<Set> {
            self.seen.push(sql.to_string());
            self.answers
                .iter()
                .find(|(needle, _)| sql.contains(needle))
                .map(|(_, s)| s.clone())
                .ok_or_else(|| Error::Query("objeto inexistente".into()))
        }
    }

    fn set(cols: &[&str], rows: &[&[&str]]) -> Set {
        Set::new(cols, rows.iter().map(|r| r.iter().map(|c| (!c.is_empty()).then_some(*c)).collect()).collect())
    }

    #[test]
    fn capabilities_follow_the_engine() {
        for id in ["db2", "db2i", "sybase", "sqlanywhere", "teradata", "vertica", "exasol", "netezza", "dameng", "altibase"] {
            assert!(supports(preset(id)), "{id}");
        }
        for id in ["odbc", "db2zos", "access", "spark", "informix", "hive"] {
            assert!(!supports(preset(id)) && !supports_cancel(preset(id)), "{id}");
        }
        for id in ["db2", "db2i", "vertica", "exasol", "dameng"] {
            assert!(supports_cancel(preset(id)), "{id}");
        }
        for id in ["sybase", "sqlanywhere", "teradata", "netezza", "altibase"] {
            assert!(!supports_cancel(preset(id)), "{id}");
        }
    }

    #[test]
    fn ids_are_validated() {
        assert_eq!(job_name(" 123456/quser/qzdasoinit ").unwrap(), "123456/QUSER/QZDASOINIT");
        for bad in ["", "123/QUSER/QZDASOINIT", "123456/QUSER", "123456/QUSER/X'); DROP", "123456/A B/C"] {
            assert!(job_name(bad).is_err(), "{bad}");
        }
        assert_eq!(vertica_session("v_db_node0001-123:0x1a").unwrap(), "v_db_node0001-123:0x1a");
        assert!(vertica_session("x'; SELECT 1 --").is_err());
        assert!(numeric_id("1; DROP TABLE x").is_err() && numeric_id("0").is_err() && numeric_id("-1").is_err());
        assert_eq!(hms_ms(Some("001:02:03")), Some(3_723_000));
    }

    #[test]
    fn db2_sessions() {
        let s = set(
            &["ID", "USR", "HOST", "PROG", "DB", "STATE", "SECS", "CPU_MS", "READS", "WRITES", "BLOCKER", "WAIT_MODE", "STMT", "OWN"],
            &[
                &["21", "ANA", "pc2", "app", "SAMPLE", "EXECUTING", "3", "120", "10", "1", "20", "X", "update t set v = 2", "0"],
                // A second activity of the same connection.
                &["21", "ANA", "pc2", "app", "SAMPLE", "EXECUTING", "1", "120", "10", "1", "", "", "SELECT 1", "0"],
                &["20", "BOB", "pc1", "app", "SAMPLE", "IDLE", "", "5", "", "", "", "", "", "1"],
            ],
        );
        let list = collect(preset("db2"), &mut Fake::new(vec![("MON_GET_CONNECTION", s)])).unwrap();
        assert_eq!(list.len(), 2);
        let w = &list[0];
        assert!(w.active && w.elapsed_ms == Some(3000) && w.command.as_deref() == Some("UPDATE"));
        assert_eq!((w.blocked_by.as_deref(), w.wait.as_deref()), (Some("20"), Some("LOCK WAIT X")));
        assert!(!list[1].active && list[1].own && list[1].sql.is_none());
    }

    #[test]
    fn ase_sessions_with_mda_text() {
        let p = set(
            &["spid", "usr", "hostname", "program_name", "db", "status", "cmd", "blocked", "time_blocked", "suid", "own"],
            &[
                &["2", "", "", "", "master", "sleeping", "DEADLOCK TUNE", "0", "", "0", "0"],
                &["15", "sa", "pc1", "isql", "app", "lock sleep", "UPDATE", "16", "4", "1", "0"],
                &["16", "sa", "pc2", "isql", "app", "recv sleep", "AWAITING COMMAND", "0", "", "1", "1"],
            ],
        );
        let t = set(&["SPID", "SQLText"], &[&["15", "UPDATE t "], &["15", "SET v = 1"]]);
        let mut src = Fake::new(vec![("sysprocesses", p), ("monProcessSQLText", t)]);
        let list = collect(preset("sybase"), &mut src).unwrap();
        assert!(list[0].system && !list[0].active);
        assert_eq!(list[1].sql.as_deref(), Some("UPDATE t SET v = 1"));
        assert_eq!((list[1].blocked_by.as_deref(), list[1].wait.as_deref(), list[1].elapsed_ms), (Some("16"), Some("lock sleep"), Some(4000)));
        assert!(!list[2].active && list[2].own);
        assert!(src.seen[1].contains("IN (15)"), "{}", src.seen[1]);
    }

    #[test]
    fn exasol_falls_back_to_its_own_sessions() {
        let s = set(
            &["SESSION_ID", "USER_NAME", "STATUS", "COMMAND_NAME", "DURATION", "SQL_TEXT", "DBINE_OWN"],
            &[&["7", "U", "EXECUTE SQL", "SELECT", "000:00:05", "select 1", "8"], &["8", "U", "IDLE", "", "000:01:00", "select 2", "8"]],
        );
        let list = collect(preset("exasol"), &mut Fake::new(vec![("EXA_ALL_SESSIONS", s)])).unwrap();
        assert!(list[0].active && list[0].elapsed_ms == Some(5000) && list[0].command.as_deref() == Some("SELECT"));
        assert!(!list[1].active && list[1].own && list[1].sql.is_none());
    }

    #[test]
    fn cancel_per_engine() {
        let mut src = Fake::new(vec![
            ("MON_GET_APPLICATION_HANDLE", set(&["1"], &[&["5"]])),
            ("MON_GET_ACTIVITY", set(&["UOW_ID", "ACTIVITY_ID"], &[&["3", "1"]])),
            ("WLM_CANCEL_ACTIVITY", Set::default()),
        ]);
        cancel(preset("db2"), &mut src, "21").unwrap();
        assert_eq!(src.seen.last().unwrap(), "CALL WLM_CANCEL_ACTIVITY(21, 3, 1)");
        assert!(cancel(preset("db2"), &mut src, "5").is_err(), "not its own session");

        let mut src = Fake::new(vec![("CURRENT_SESSION", set(&["1"], &[&["8"]])), ("KILL STATEMENT", Set::default())]);
        cancel(preset("exasol"), &mut src, "7").unwrap();
        assert_eq!(src.seen.last().unwrap(), "KILL STATEMENT IN SESSION 7");

        let mut src = Fake::new(vec![
            ("current_session", set(&["session_id"], &[&["n1-1:0x1"]])),
            ("v_monitor.sessions", set(&["statement_id"], &[&["4"]])),
            ("INTERRUPT_STATEMENT", Set::default()),
        ]);
        cancel(preset("vertica"), &mut src, "n1-2:0x9").unwrap();
        assert_eq!(src.seen.last().unwrap(), "SELECT INTERRUPT_STATEMENT('n1-2:0x9', 4)");

        assert!(matches!(cancel(preset("sybase"), &mut Fake::new(vec![]), "12"), Err(Error::Unsupported(_))));
    }
}
