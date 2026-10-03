//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! statement ([`dbine_driver::Session::cancel_query`]) for Databricks SQL.
//!
//! A SQL warehouse is reached through stateless REST calls, so the list is
//! the warehouse's running and queued queries from its query history (the
//! monitor's "Consultas en curso"), with no SQL run on it. The id is the
//! query id, which is the Statement Execution API's statement id, and
//! cancelling is `POST /api/2.0/sql/statements/{id}/cancel`.

use crate::DatabricksSession;
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::{json, Value as Json};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(10);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most (the history API's page limit is 1000).
const MAX_ROWS: usize = 1000;

/// A query / statement id (`01ef…-…`: hex and dashes).
pub(crate) fn valid_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(id)
}

fn text(q: &Json, ptr: &str) -> Option<String> {
    match q.pointer(ptr)? {
        Json::String(s) => Some(s.trim().to_string()),
        Json::Null => None,
        v => Some(v.to_string()),
    }
    .filter(|s| !s.is_empty())
}

fn num(q: &Json, ptr: &str) -> Option<f64> {
    match q.pointer(ptr)? {
        Json::String(s) => s.parse().ok(),
        x => x.as_f64(),
    }
}

/// One query of the history; `now` in epoch ms.
fn row(q: &Json, now: f64) -> ServerProcess {
    let status = text(q, "/status");
    ServerProcess {
        id: text(q, "/query_id").unwrap_or_default(),
        // Running or queued: both are in flight.
        active: true,
        wait: status.as_deref().filter(|s| *s == "QUEUED").map(|_| "en cola del warehouse".to_string()),
        status,
        user: text(q, "/user_name"),
        program: text(q, "/client_application"),
        command: text(q, "/statement_type"),
        elapsed_ms: num(q, "/query_start_time_ms").map(|st| (now - st).max(0.0) as u64),
        cpu_ms: num(q, "/metrics/task_total_time_ms").map(|v| v.max(0.0) as u64),
        reads: num(q, "/metrics/rows_read_count").map(|v| v.max(0.0) as u64),
        sql: text(q, "/query_text").map(|t| t.chars().take(MAX_TEXT).collect()),
        ..Default::default()
    }
}

impl DatabricksSession {
    pub(crate) async fn processes_list(&self) -> Result<Vec<ServerProcess>> {
        let path = format!(
            "/api/2.0/sql/history/queries?filter_by.warehouse_ids={}&filter_by.statuses=RUNNING&filter_by.statuses=QUEUED\
             &include_metrics=true&max_results={MAX_ROWS}",
            self.warehouse
        );
        let r = tokio::time::timeout(QUERY_LIMIT, self.api.get(&path))
            .await
            .map_err(|_| Error::Query("la lista de consultas tardó demasiado".into()))??;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
        Ok(r.get("res")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .map(|q| row(q, now))
            .filter(|p| !p.id.is_empty())
            .collect())
    }

    pub(crate) async fn cancel_running(&self, id: &str) -> Result<()> {
        let id = valid_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de consulta de Databricks", id.trim())))?;
        let failed = |m: String| {
            Error::Query(format!(
                "no se pudo cancelar la consulta {id}: {m}. Por API solo se cancelan las sentencias enviadas con la \
                 Statement Execution API (como las de DBine); las de otros clientes se cancelan desde la interfaz de Databricks"
            ))
        };
        // The cancel is accepted even for an unknown id; asking for the
        // statement tells whether the API knows it.
        self.api.post(&format!("/api/2.0/sql/statements/{id}/cancel"), &json!({})).await.map_err(|e| failed(e.to_string()))?;
        self.api.get(&format!("/api/2.0/sql/statements/{id}")).await.map(|_| ()).map_err(|e| failed(e.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked() {
        assert_eq!(valid_id(" 01ef1234-abcd-1234-abcd-0123456789ab "), Some("01ef1234-abcd-1234-abcd-0123456789ab"));
        assert_eq!(valid_id("../../jobs"), None);
        assert_eq!(valid_id(""), None);
    }

    #[test]
    fn history_entries_become_processes() {
        let q = json!({"query_id": "01ef", "status": "QUEUED", "user_name": "ana@example.com", "statement_type": "SELECT",
                       "query_start_time_ms": 1000, "query_text": "SELECT 1",
                       "metrics": {"task_total_time_ms": 40, "rows_read_count": 7}});
        let p = row(&q, 2500.0);
        assert_eq!(p.id, "01ef");
        assert!(p.active && !p.own);
        assert_eq!(p.wait.as_deref(), Some("en cola del warehouse"));
        assert_eq!(p.elapsed_ms, Some(1500));
        assert_eq!((p.cpu_ms, p.reads), (Some(40), Some(7)));
        assert_eq!(p.command.as_deref(), Some("SELECT"));
    }
}
