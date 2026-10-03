//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! query ([`dbine_driver::Session::cancel_query`]) for Trino, Presto and
//! Starburst.
//!
//! The coordinator has no sessions to list or end (the client protocol is
//! stateless HTTP): each row is a query of `system.runtime.queries` that
//! hasn't finished, its id is the query id, and cancelling it is
//! `CALL system.runtime.kill_query`.

use crate::{Flavor, TrinoSession};
use dbine_driver::{Error, Result, ServerProcess};
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;
/// Marks the list's own statement, so it leaves itself out.
const TAG: &str = "/* dbine-processes */";

/// A query id as the coordinator makes them (`20241004_153012_00042_x7k2p`).
pub(crate) fn valid_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')).then_some(id)
}

/// The statement's first word, upper-cased ("SELECT", "INSERT"…).
fn command(sql: &str) -> Option<String> {
    let word: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_uppercase())
}

fn opt(v: Option<&String>) -> Option<String> {
    v.map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// One row: `query_id, state, user, source, query, elapsed_ms`.
fn row(r: &[String]) -> ServerProcess {
    let state = opt(r.get(1));
    let sql = opt(r.get(4));
    ServerProcess {
        id: opt(r.first()).unwrap_or_default(),
        // Queued or running: every query the list shows is in flight.
        active: true,
        status: state,
        user: opt(r.get(2)),
        program: opt(r.get(3)),
        command: sql.as_deref().and_then(command),
        elapsed_ms: r.get(5).and_then(|v| v.trim().parse::<f64>().ok()).map(|v| v.max(0.0) as u64),
        sql,
        ..Default::default()
    }
}

impl TrinoSession {
    pub(crate) async fn processes_list(&mut self) -> Result<Vec<ServerProcess>> {
        let sql = format!(
            "{TAG} SELECT query_id, state, \"user\", source, substr(query, 1, {MAX_TEXT}),
                    date_diff('millisecond', coalesce(started, created), current_timestamp)
             FROM system.runtime.queries
             WHERE state NOT IN ('FINISHED', 'FAILED') AND query NOT LIKE '{TAG}%'
             ORDER BY created LIMIT {MAX_ROWS}"
        );
        let rows = tokio::time::timeout(QUERY_LIMIT, self.strings(&sql))
            .await
            .map_err(|_| Error::Query("la lista de consultas tardó demasiado".into()))??;
        Ok(rows.iter().map(|r| row(r)).filter(|p| !p.id.is_empty()).collect())
    }

    pub(crate) async fn cancel_running(&mut self, id: &str) -> Result<()> {
        let id = valid_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de consulta", id.trim())))?;
        // Presto's procedure takes the message positionally too.
        let message = match self.flavor {
            Flavor::Presto => "'Cancelada desde DBine'",
            _ => "message => 'Cancelada desde DBine'",
        };
        let sql = match self.flavor {
            Flavor::Presto => format!("CALL system.runtime.kill_query('{id}', {message})"),
            _ => format!("CALL system.runtime.kill_query(query_id => '{id}', {message})"),
        };
        self.strings(&sql).await.map(|_| ()).map_err(|e| match e {
            Error::Query(m) if m.contains("not running") || m.contains("not found") => {
                Error::Query(format!("no se pudo cancelar la consulta {id}: ya terminó o no existe"))
            }
            e => e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked() {
        assert_eq!(valid_id(" 20241004_153012_00042_x7k2p "), Some("20241004_153012_00042_x7k2p"));
        assert_eq!(valid_id("x'); DROP"), None);
        assert_eq!(valid_id(""), None);
    }

    #[test]
    fn rows_become_processes() {
        let r: Vec<String> = ["q1", "RUNNING", "ana", "dbine", " select 1", "1500"].map(String::from).to_vec();
        let p = row(&r);
        assert!(p.active && !p.own);
        assert_eq!(p.command.as_deref(), Some("SELECT"));
        assert_eq!(p.elapsed_ms, Some(1500));
        let queued: Vec<String> = ["q2", "QUEUED", "", "", "", ""].map(String::from).to_vec();
        let p = row(&queued);
        assert!(p.active && p.user.is_none() && p.sql.is_none());
    }
}
