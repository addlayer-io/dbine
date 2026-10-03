//! The process list ([`dbine_driver::Session::processes`]) and cancelling
//! a query ([`dbine_driver::Session::cancel_query`]). The REST API is
//! stateless, so the rows are the running queries: `SHOW QUERIES`
//! (`QueryId`, `DataNodeId`, `ElapsedTime` in seconds, `Statement`), the
//! monitor's query table, across every DataNode. `KILL QUERY '<id>'` stops
//! one.

use crate::monitor::Answer;
use crate::IotDbSession;
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::Value as J;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;

fn text(v: &J) -> Option<String> {
    match v {
        J::Null => None,
        J::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        v => Some(v.to_string()),
    }
}

/// `SHOW QUERIES` → processes; the list's own statement is flagged.
pub(crate) fn rows(q: &Answer) -> Vec<ServerProcess> {
    q.rows
        .iter()
        .filter_map(|r| {
            let id = text(q.get(r, "QueryId"))?;
            let sql = text(q.get(r, "Statement"));
            Some(ServerProcess {
                id,
                status: Some("en ejecución".into()),
                active: true,
                own: sql.as_deref().is_some_and(|s| s.eq_ignore_ascii_case("SHOW QUERIES")),
                host: text(q.get(r, "DataNodeId")).map(|n| format!("DataNode {n}")),
                command: sql.as_deref().and_then(|s| s.split_whitespace().next()).map(str::to_uppercase),
                elapsed_ms: q.get(r, "ElapsedTime").as_f64().or_else(|| text(q.get(r, "ElapsedTime")).and_then(|s| s.parse().ok())).map(|s| (s.max(0.0) * 1000.0) as u64),
                sql: sql.map(|s| s.chars().take(MAX_TEXT).collect()),
                ..Default::default()
            })
        })
        .take(MAX_ROWS)
        .collect()
}

/// A query id (`20240101_101010_00012_1`): letters, digits and `_`.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

impl IotDbSession {
    pub(crate) async fn processes(&mut self) -> Result<Vec<ServerProcess>> {
        let t = tokio::time::timeout(QUERY_LIMIT, self.query("SHOW QUERIES", MAX_ROWS))
            .await
            .map_err(|_| Error::Query("SHOW QUERIES no respondió a tiempo".into()))??;
        Ok(rows(&Answer { columns: t.columns.into_iter().map(|c| c.name).collect(), rows: t.rows }))
    }

    /// `KILL QUERY`. The list's own `SHOW QUERIES` is over by the time
    /// anyone could pick it, so there's no own query to refuse.
    pub(crate) async fn cancel(&mut self, id: &str) -> Result<()> {
        let id = id.trim();
        if !valid_id(id) {
            return Err(Error::Query(format!("«{id}» no es un id de consulta de IoTDB")));
        }
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden cancelar consultas de otros clientes.".into()));
        }
        self.non_query(&format!("KILL QUERY '{id}'")).await.map_err(|e| match e {
            Error::Query(m) => Error::Query(format!("no se pudo cancelar la consulta {id}: {m}")),
            e => e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn show_queries() {
        let a = Answer {
            columns: ["Time", "QueryId", "DataNodeId", "ElapsedTime", "Statement"].map(String::from).to_vec(),
            rows: vec![
                vec![json!("2026-10-03 22:00:00"), json!("20261003_220000_00001_1"), json!(1), json!(0.002), json!("SHOW QUERIES")],
                vec![json!("2026-10-03 21:59:57"), json!("20261003_215957_00002_1"), json!(1), json!(3.25), json!("select * from root.**")],
            ],
        };
        let p = rows(&a);
        assert!(p[0].own && p[0].active);
        assert_eq!(p[1].id, "20261003_215957_00002_1");
        assert_eq!((p[1].elapsed_ms, p[1].command.as_deref(), p[1].host.as_deref()), (Some(3250), Some("SELECT"), Some("DataNode 1")));
        assert!(!p[1].own);
        assert!(valid_id("20261003_215957_00002_1"));
        assert!(!valid_id("x'; DELETE DATABASE root.**"));
    }
}
