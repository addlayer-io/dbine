//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! job ([`dbine_driver::Session::cancel_query`]).
//!
//! Dremio's REST API has no sessions: the list is the jobs that haven't
//! ended (`sys.jobs`, as the monitor reads it), the id is the job id, and
//! cancelling it is `POST /api/v3/job/{id}/cancel`.

use crate::DremioSession;
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::{Map, Value};
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;
/// Marks the list's own job, so it leaves itself out.
const MARK: &str = "dbine-processes-list";

/// A job id as Dremio prints it (`1b2c3d4e-…`: hex and dashes).
pub(crate) fn valid_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(id)
}

/// The statement's first word, upper-cased ("SELECT", "CREATE"…).
fn command(sql: &str) -> Option<String> {
    let word: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_uppercase())
}

fn opt(r: &Map<String, Value>, key: &str) -> Option<String> {
    match r.get(key)? {
        Value::Null => None,
        Value::String(s) => Some(s.trim().to_string()),
        v => Some(v.to_string()),
    }
    .filter(|s| !s.is_empty())
}

fn num(r: &Map<String, Value>, key: &str) -> Option<u64> {
    match r.get(key)? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
    .map(|v| v.max(0.0) as u64)
}

/// One `sys.jobs` row; `now` in epoch ms.
fn row(r: &Map<String, Value>, now: u64) -> ServerProcess {
    let sql = opt(r, "query").map(|q| q.chars().take(MAX_TEXT).collect::<String>());
    ServerProcess {
        id: opt(r, "job_id").unwrap_or_default(),
        status: opt(r, "status"),
        // Queued, planning or running: every listed job is in flight.
        active: true,
        user: opt(r, "user_name"),
        // How it was sent: UI_RUN, REST, JDBC, ODBC, FLIGHT…
        program: opt(r, "query_type"),
        command: sql.as_deref().and_then(command),
        elapsed_ms: num(r, "submitted_epoch_millis").filter(|t| *t > 0).map(|t| now.saturating_sub(t)),
        cpu_ms: num(r, "execution_cpu_time_millis"),
        reads: num(r, "rows_scanned"),
        // The queue only matters while the job waits in it.
        wait: opt(r, "queue_name")
            .filter(|q| q != "-" && opt(r, "status").is_some_and(|s| s.contains("QUEUE")))
            .map(|q| format!("cola {q}")),
        sql,
        ..Default::default()
    }
}

impl DremioSession {
    pub(crate) async fn processes_list(&self) -> Result<Vec<ServerProcess>> {
        let sql = format!(
            "SELECT /* {MARK} */ job_id, user_name, status, query_type, queue_name, submitted_epoch_millis,
                    execution_cpu_time_millis, rows_scanned, query
             FROM sys.jobs
             WHERE status NOT IN ('COMPLETED', 'FAILED', 'CANCELED', 'CANCELLED') AND query NOT LIKE '%{MARK}%'
             ORDER BY submitted_ts LIMIT {MAX_ROWS}"
        );
        let rows = tokio::time::timeout(QUERY_LIMIT, self.records(&sql))
            .await
            .map_err(|_| Error::Query("la lista de trabajos tardó demasiado".into()))??;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        Ok(rows.iter().map(|r| row(r, now)).filter(|p| !p.id.is_empty()).collect())
    }

    pub(crate) async fn cancel_running(&self, id: &str) -> Result<()> {
        let id = valid_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de trabajo de Dremio", id.trim())))?;
        self.conn
            .send(reqwest::Method::POST, &format!("/api/v3/job/{id}/cancel"), None)
            .await
            .map(|_| ())
            .map_err(|e| match e {
                Error::Query(m) => Error::Query(format!("no se pudo cancelar el trabajo {id}: {m}")),
                e => e,
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_are_checked() {
        assert_eq!(valid_id(" 1b2c3d4e-5f60-7a8b-9c0d-e1f2a3b4c5d6 "), Some("1b2c3d4e-5f60-7a8b-9c0d-e1f2a3b4c5d6"));
        assert_eq!(valid_id("../../catalog"), None);
        assert_eq!(valid_id(""), None);
    }

    #[test]
    fn jobs_become_processes() {
        let r = json!({"job_id": "1b2c", "user_name": "dbine", "status": "RUNNING", "query_type": "REST",
                       "queue_name": "SMALL", "submitted_epoch_millis": 1000, "execution_cpu_time_millis": 12,
                       "rows_scanned": "500", "query": "select 1"});
        let p = row(r.as_object().unwrap(), 4000);
        assert_eq!(p.id, "1b2c");
        assert!(p.active && !p.own);
        assert_eq!(p.elapsed_ms, Some(3000));
        assert_eq!(p.reads, Some(500));
        assert_eq!(p.wait, None);
        assert_eq!(p.command.as_deref(), Some("SELECT"));
        let q = json!({"job_id": "1b2d", "status": "ENQUEUED", "queue_name": "SMALL"});
        assert_eq!(row(q.as_object().unwrap(), 0).wait.as_deref(), Some("cola SMALL"));
    }
}
