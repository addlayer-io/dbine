//! The process list ([`dbine_driver::Session::processes`]) per variant,
//! in one query:
//!
//! - SQL Server and Azure SQL: `sys.dm_exec_sessions` with its first
//!   request (status, command, CPU, reads, writes, wait, blocker) and the
//!   running statement from `dm_exec_sql_text`; an idle session holding a
//!   transaction shows its last batch.
//! - Fabric: sessions and requests, without statement text (the
//!   warehouse's DMVs don't join `dm_exec_sql_text`).
//! - Babelfish: PostgreSQL's `pg_stat_activity`, read through T-SQL (its
//!   DATEDIFF counts whole minutes on `timestamptz`: cast to datetime2
//!   first).
//!
//! None of them can stop another session's statement and keep the session:
//! `KILL` ends it (and rolls back its transaction), and Babelfish refuses
//! `pg_cancel_backend` from T-SQL. `cancel_query` stays unsupported; ending
//! the session is `kill_session`.

use crate::monitor::f;
use crate::variant::Variant;
use crate::SqlServerSession;
use dbine_driver::{Error, Result, ServerProcess};
use tiberius::Row;

/// Rows at most: a server with thousands of connections still answers fast.
const MAX_ROWS: usize = 2000;
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;

/// What an idle session holding a transaction shows as its status.
const IDLE_IN_TRANSACTION: &str = "inactiva con transacción abierta";

/// Why `cancel_query` isn't there, in Spanish (for the error and the docs).
pub(crate) fn cancel_reason(v: Variant) -> &'static str {
    match v {
        Variant::Babelfish => {
            "Babelfish no permite detener la consulta de otra sesión desde T-SQL: solo terminarla (KILL), lo que cierra la sesión"
        }
        _ => "SQL Server no puede detener la consulta de otra sesión sin terminarla: KILL cierra la sesión y deshace su transacción",
    }
}

/// Columns, in order: id, status, active, system, own, user, host, program,
/// database, command, elapsed ms, cpu ms, reads, writes, wait, blocker, sql.
fn sql(v: Variant) -> String {
    match v {
        Variant::Babelfish => format!(
            "SELECT TOP ({MAX_ROWS}) CAST(pid AS varchar(20)),
                    CAST(CASE WHEN state LIKE 'idle in transaction%' THEN '{IDLE_IN_TRANSACTION}' ELSE state END AS varchar(64)),
                    CAST(CASE WHEN state = 'active' THEN 1 ELSE 0 END AS int),
                    CAST(CASE WHEN backend_type <> 'client backend' THEN 1 ELSE 0 END AS int),
                    CAST(CASE WHEN pid = @@SPID THEN 1 ELSE 0 END AS int),
                    CAST(usename AS varchar(128)), CAST(client_addr AS varchar(64)), CAST(application_name AS varchar(256)),
                    CAST(datname AS varchar(128)), CAST(NULL AS varchar(10)),
                    CASE WHEN state = 'active' THEN CAST(DATEDIFF(MILLISECOND, CAST(query_start AS datetime2), SYSDATETIME()) AS bigint)
                         ELSE CAST(DATEDIFF(SECOND, CAST(COALESCE(state_change, backend_start) AS datetime2), SYSDATETIME()) AS bigint) * 1000 END,
                    CAST(NULL AS bigint), CAST(NULL AS bigint), CAST(NULL AS bigint),
                    CAST(wait_event_type AS varchar(64)) + ': ' + CAST(wait_event AS varchar(64)),
                    CAST(pg_blocking_pids(pid) AS varchar(200)),
                    CASE WHEN state = 'active' OR state LIKE 'idle in transaction%' THEN CAST(LEFT(query, {MAX_TEXT}) AS varchar(max)) END
               FROM pg_catalog.pg_stat_activity
              ORDER BY CASE WHEN state = 'active' THEN 0 ELSE 1 END, pid"
        ),
        Variant::Fabric => format!(
            "SELECT TOP ({MAX_ROWS}) CAST(s.session_id AS nvarchar(20)), COALESCE(r.status, s.status),
                    CAST(CASE WHEN r.session_id IS NOT NULL AND r.status NOT IN (N'background', N'sleeping') THEN 1 ELSE 0 END AS int),
                    CAST(CASE WHEN s.is_user_process = 1 THEN 0 ELSE 1 END AS int),
                    CAST(CASE WHEN s.session_id = @@SPID THEN 1 ELSE 0 END AS int),
                    s.login_name, s.host_name, s.program_name, DB_NAME(COALESCE(r.database_id, s.database_id)), r.command,
                    CAST(COALESCE(r.total_elapsed_time,
                                  CAST(DATEDIFF(SECOND, s.last_request_start_time, SYSDATETIME()) AS bigint) * 1000) AS bigint),
                    CAST(r.cpu_time AS bigint), CAST(NULL AS bigint), CAST(NULL AS bigint), r.wait_type,
                    CAST(CASE WHEN r.blocking_session_id > 0 THEN r.blocking_session_id END AS nvarchar(20)),
                    CAST(NULL AS nvarchar(10))
               FROM sys.dm_exec_sessions s
               LEFT JOIN sys.dm_exec_requests r ON r.session_id = s.session_id
              ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, s.session_id"
        ),
        _ => format!(
            "SELECT TOP ({MAX_ROWS}) CAST(s.session_id AS nvarchar(20)),
                    CASE WHEN r.session_id IS NULL AND s.open_transaction_count > 0 THEN N'{IDLE_IN_TRANSACTION}'
                         ELSE COALESCE(r.status, s.status) END,
                    CAST(CASE WHEN r.session_id IS NOT NULL AND r.status NOT IN (N'background', N'sleeping') THEN 1 ELSE 0 END AS int),
                    CAST(CASE WHEN s.is_user_process = 1 THEN 0 ELSE 1 END AS int),
                    CAST(CASE WHEN s.session_id = @@SPID THEN 1 ELSE 0 END AS int),
                    s.login_name, COALESCE(s.host_name, c.client_net_address), s.program_name,
                    DB_NAME(COALESCE(r.database_id, s.database_id)), r.command,
                    CAST(CASE WHEN r.session_id IS NOT NULL THEN r.total_elapsed_time
                              ELSE CAST(DATEDIFF(SECOND, COALESCE(s.last_request_end_time, s.login_time), SYSDATETIME()) AS bigint) * 1000
                         END AS bigint),
                    CAST(COALESCE(r.cpu_time, s.cpu_time) AS bigint),
                    CAST(COALESCE(r.logical_reads, s.logical_reads) AS bigint),
                    CAST(COALESCE(r.writes, s.writes) AS bigint),
                    r.wait_type,
                    CAST(CASE WHEN r.blocking_session_id > 0 THEN r.blocking_session_id END AS nvarchar(20)),
                    CAST(LEFT(CASE WHEN r.session_id IS NULL THEN t.text
                                   ELSE SUBSTRING(t.text, r.statement_start_offset / 2 + 1,
                                        (CASE WHEN r.statement_end_offset <= 0 THEN DATALENGTH(t.text) ELSE r.statement_end_offset END
                                         - r.statement_start_offset) / 2 + 1) END, {MAX_TEXT}) AS nvarchar(max))
               FROM sys.dm_exec_sessions s
              OUTER APPLY (SELECT TOP (1) * FROM sys.dm_exec_requests q WHERE q.session_id = s.session_id ORDER BY q.request_id) r
               LEFT JOIN sys.dm_exec_connections c ON c.session_id = s.session_id AND c.parent_connection_id IS NULL
              OUTER APPLY sys.dm_exec_sql_text(CASE WHEN r.session_id IS NOT NULL THEN r.sql_handle
                                                    WHEN s.open_transaction_count > 0 THEN c.most_recent_sql_handle END) t
              ORDER BY CASE WHEN r.session_id IS NULL THEN 1 ELSE 0 END, s.session_id"
        ),
    }
}

fn text(r: &Row, i: usize) -> Option<String> {
    r.try_get::<&str, _>(i).ok().flatten().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn count(r: &Row, i: usize) -> Option<u64> {
    f(r, i).map(|v| v.max(0.0) as u64)
}

/// The first id of `pg_blocking_pids` (`{123,456}`), or the id itself.
fn first_id(v: &str) -> Option<String> {
    let id: String = v.trim_start_matches('{').chars().take_while(char::is_ascii_digit).collect();
    (!id.is_empty()).then_some(id)
}

/// The first keyword of a statement (`SELECT`, `UPDATE`…).
fn first_word(sql: &str) -> Option<String> {
    let w: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!w.is_empty()).then(|| w.to_ascii_uppercase())
}

pub(crate) async fn processes(s: &mut SqlServerSession) -> Result<Vec<ServerProcess>> {
    let babelfish = s.variant == Variant::Babelfish;
    let rows = s.rows(&sql(s.variant), &[]).await?;
    Ok(rows
        .iter()
        .map(|r| {
            let active = f(r, 2) == Some(1.0);
            let sql = text(r, 16);
            ServerProcess {
                id: text(r, 0).unwrap_or_default(),
                status: text(r, 1),
                active,
                system: f(r, 3) == Some(1.0),
                own: f(r, 4) == Some(1.0),
                user: text(r, 5),
                host: text(r, 6),
                program: text(r, 7),
                database: text(r, 8),
                command: if babelfish { sql.as_deref().filter(|_| active).and_then(first_word) } else { text(r, 9) },
                elapsed_ms: count(r, 10),
                cpu_ms: count(r, 11),
                reads: count(r, 12),
                writes: count(r, 13),
                wait: text(r, 14),
                blocked_by: text(r, 15).and_then(|b| first_id(&b)),
                sql,
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect())
}

pub(crate) fn cancel(s: &SqlServerSession) -> Result<()> {
    Err(Error::Unsupported(cancel_reason(s.variant).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn blockers_and_commands_read_from_text() {
        assert_eq!(first_id("{123,456}").as_deref(), Some("123"));
        assert_eq!(first_id("{}"), None);
        assert_eq!(first_id("53").as_deref(), Some("53"));
        assert_eq!(first_word("  select 1").as_deref(), Some("SELECT"));
        assert_eq!(first_word(""), None);
    }

    #[test]
    fn every_variant_has_a_query() {
        for v in Variant::ALL {
            assert!(sql(v).contains("TOP (2000)"));
        }
        assert!(cancel_reason(Variant::SqlServer).contains("KILL"));
    }
}
