//! The process list ([`dbine_driver::Session::processes`]) and stopping
//! another session's statement ([`dbine_driver::Session::cancel_query`])
//! per variant.
//!
//! - PostgreSQL and the engines that keep its `pg_stat_activity` (Aurora,
//!   AlloyDB, Cloud SQL, Timescale, Yugabyte, EDB, Greenplum and its forks,
//!   openGauss…): one row per backend, `pg_blocking_pids()` from 9.6, and
//!   `pg_cancel_backend()` to cancel.

use crate::catalog::cell;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{Error, Result, ServerProcess};
use std::time::Duration;
use tokio_postgres::SimpleQueryRow;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most: a server with thousands of connections still answers fast.
const MAX_ROWS: usize = 2000;

/// Variants whose sessions answer `processes` and `cancel_query`.
pub(crate) fn supported(v: Variant) -> bool {
    unsupported_reason(v).is_none()
}

/// Why a variant has neither, in Spanish (for the error and the docs).
pub(crate) fn unsupported_reason(v: Variant) -> Option<&'static str> {
    match v {
        Variant::Cockroach
        | Variant::Redshift
        | Variant::Denodo
        | Variant::Materialize
        | Variant::RisingWave
        | Variant::CrateDb
        | Variant::Yellowbrick
        | Variant::H2 => Some("este motor todavía no lista sus procesos en DBine"),
        _ => None,
    }
}

fn text(r: &SimpleQueryRow, name: &str) -> Option<String> {
    cell(r, name).map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn ms(r: &SimpleQueryRow, name: &str) -> Option<u64> {
    cell(r, name).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v.max(0.0) as u64)
}

fn truthy(r: &SimpleQueryRow, name: &str) -> bool {
    matches!(cell(r, name).map(|v| v.trim().to_ascii_lowercase()).as_deref(), Some("t" | "true" | "1"))
}

pub(crate) async fn processes(s: &PgSession) -> Result<Vec<ServerProcess>> {
    if let Some(why) = unsupported_reason(s.variant) {
        return Err(Error::Unsupported(why.into()));
    }
    let ver = s.version;
    // Before 10 only client backends show up, except in openGauss, whose
    // background threads have no client port.
    let system = if ver >= 100000 { "coalesce(backend_type, '') <> 'client backend'" } else { "client_port IS NULL" };
    let wait = if ver >= 90600 { "coalesce(wait_event_type || ': ' || wait_event, '')" } else { "CASE WHEN waiting THEN 'Lock' ELSE '' END" };
    let blocked_by = if ver >= 90600 { "(pg_blocking_pids(pid))[1]::text" } else { "NULL::text" };
    let rows = s
        .text_within(
            &format!(
                "SELECT pid::text AS id, state, usename,
                        coalesce(host(client_addr), CASE WHEN client_port = -1 THEN 'socket local' END) AS host,
                        application_name, datname,
                        CASE WHEN state = 'active' THEN upper(split_part(ltrim(query), ' ', 1)) END AS command,
                        round(extract(epoch FROM now() - CASE WHEN state = 'active' THEN query_start ELSE coalesce(state_change, backend_start) END) * 1000) AS elapsed,
                        {wait} AS wait, {blocked_by} AS blocked_by,
                        left(query, {MAX_TEXT}) AS query,
                        (state = 'active') AS active, ({system}) AS system, (pid = pg_backend_pid()) AS own
                 FROM pg_stat_activity
                 ORDER BY (state = 'active') DESC, pid LIMIT {MAX_ROWS}"
            ),
            QUERY_LIMIT,
        )
        .await?;
    Ok(rows
        .iter()
        .map(|r| {
            let active = truthy(r, "active");
            ServerProcess {
                id: text(r, "id").unwrap_or_default(),
                status: text(r, "state"),
                active,
                system: truthy(r, "system"),
                own: truthy(r, "own"),
                user: text(r, "usename"),
                host: text(r, "host"),
                program: text(r, "application_name"),
                database: text(r, "datname"),
                command: text(r, "command"),
                elapsed_ms: ms(r, "elapsed"),
                wait: text(r, "wait"),
                blocked_by: text(r, "blocked_by"),
                // An idle session's text is its last statement, not one running.
                sql: text(r, "query").filter(|_| active || text(r, "state").is_some_and(|st| st.starts_with("idle in transaction"))),
                ..Default::default()
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect())
}

pub(crate) async fn cancel(s: &PgSession, id: &str) -> Result<()> {
    if let Some(why) = unsupported_reason(s.variant) {
        return Err(Error::Unsupported(why.into()));
    }
    let id = id.trim();
    // Backend pids are int4; openGauss's are thread ids (int8).
    let n: i64 = if s.variant == Variant::OpenGauss { id.parse().ok() } else { id.parse::<i32>().ok().map(i64::from) }
        .filter(|n| *n > 0)
        .ok_or_else(|| Error::Query(format!("«{id}» no es un id de sesión (PID)")))?;
    let first = |rows: Vec<SimpleQueryRow>| rows.first().and_then(|r| r.get(0).map(str::to_string));
    if first(s.text("SELECT pg_backend_pid()::text").await?).as_deref() == Some(id) {
        return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá".into()));
    }
    let sql = if s.variant == Variant::OpenGauss { format!("SELECT pg_cancel_backend({n})::text") } else { format!("SELECT pg_cancel_backend({n}::int)::text") };
    match first(s.text(&sql).await?).map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("t" | "true" | "1") => Ok(()),
        _ => Err(Error::Query(format!(
            "no se pudo cancelar la consulta de la sesión {id}: no existe, no está ejecutando nada o tu usuario no tiene permiso"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn variants_with_pg_stat_activity_are_supported() {
        assert!(supported(Variant::Postgres));
        assert!(supported(Variant::Aurora));
        assert!(supported(Variant::OpenGauss));
        assert!(!supported(Variant::Cockroach));
    }
}
