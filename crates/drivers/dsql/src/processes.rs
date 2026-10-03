//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! session's statement ([`dbine_driver::Session::cancel_query`]).
//!
//! Aurora DSQL keeps PostgreSQL's `pg_stat_activity` (the monitor's
//! "Sesiones") and `pg_cancel_backend()`. It has no locks to wait on
//! (optimistic concurrency), so there's no `blocked_by`, and a connection
//! may only see the sessions its query processor reports.

use crate::monitor::rows;
use dbine_driver::{Error, Result, ServerProcess};
use std::time::Duration;
use tokio_postgres::{Client, SimpleQueryRow};

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;

fn text(r: &SimpleQueryRow, name: &str) -> Option<String> {
    r.try_get(name).ok().flatten().map(|v| v.trim().to_string()).filter(|v| !v.is_empty())
}

fn truthy(r: &SimpleQueryRow, name: &str) -> bool {
    matches!(text(r, name).map(|v| v.to_ascii_lowercase()).as_deref(), Some("t" | "true" | "1"))
}

/// A backend pid as the list shows it.
pub(crate) fn pid(id: &str) -> Option<i32> {
    id.trim().parse::<i32>().ok().filter(|n| *n > 0)
}

pub(crate) async fn processes(client: &Client) -> Result<Vec<ServerProcess>> {
    let sql = format!(
        "SELECT pid::text AS id, state, usename, host(client_addr) AS host, application_name, datname,
                CASE WHEN state = 'active' THEN upper(split_part(ltrim(query), ' ', 1)) END AS command,
                round(extract(epoch FROM now() - CASE WHEN state = 'active' THEN query_start ELSE coalesce(state_change, backend_start) END) * 1000) AS elapsed,
                left(query, {MAX_TEXT}) AS query, (state = 'active') AS active, (pid = pg_backend_pid()) AS own
         FROM pg_stat_activity
         ORDER BY (state = 'active') DESC, pid LIMIT {MAX_ROWS}"
    );
    let rs = tokio::time::timeout(QUERY_LIMIT, rows(client, &sql))
        .await
        .map_err(|_| Error::Query("la lista de sesiones tardó demasiado".into()))?
        .map_err(Error::Query)?;
    Ok(rs
        .iter()
        .map(|r| {
            let active = truthy(r, "active");
            let state = text(r, "state");
            ServerProcess {
                id: text(r, "id").unwrap_or_default(),
                active,
                own: truthy(r, "own"),
                user: text(r, "usename"),
                host: text(r, "host"),
                program: text(r, "application_name"),
                database: text(r, "datname"),
                command: text(r, "command"),
                elapsed_ms: text(r, "elapsed").and_then(|v| v.parse::<f64>().ok()).map(|v| v.max(0.0) as u64),
                // An idle session's text is its last statement, not one running.
                sql: text(r, "query").filter(|_| active || state.as_deref().is_some_and(|s| s.starts_with("idle in transaction"))),
                status: state,
                ..Default::default()
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect())
}

pub(crate) async fn cancel(client: &Client, id: &str) -> Result<()> {
    let n = pid(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de sesión (PID)", id.trim())))?;
    let first = |rs: Vec<SimpleQueryRow>| rs.first().and_then(|r| r.get(0).map(str::to_string));
    let own = first(rows(client, "SELECT pg_backend_pid()::text").await.map_err(Error::Query)?);
    if own.as_deref() == Some(n.to_string().as_str()) {
        return Err(Error::Query("esa es la sesión con la que DBine está consultando: no se puede cancelar desde acá".into()));
    }
    let r = rows(client, &format!("SELECT pg_cancel_backend({n})::text")).await.map_err(|e| Error::Query(format!("no se pudo cancelar la consulta de la sesión {n}: {e}")))?;
    match first(r).map(|v| v.trim().to_ascii_lowercase()).as_deref() {
        Some("t" | "true" | "1") => Ok(()),
        _ => Err(Error::Query(format!(
            "no se pudo cancelar la consulta de la sesión {n}: no existe, no está ejecutando nada o tu usuario no tiene permiso"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pids_are_checked() {
        assert_eq!(pid(" 4242 "), Some(4242));
        assert_eq!(pid("0"), None);
        assert_eq!(pid("1; DROP TABLE x"), None);
    }
}
