//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! query ([`dbine_driver::Session::cancel_query`]).
//!
//! Athena is serverless: no connections, only query executions. The list is
//! the running and queued ones among the workgroup's latest executions
//! (`ListQueryExecutions` + `BatchGetQueryExecution`, as the monitor reads
//! them; the API can't filter by state), the id is the execution id, and
//! cancelling is `StopQueryExecution`.

use crate::{err, AthenaSession};
use aws_sdk_athena::types::{QueryExecution, QueryExecutionState};
use dbine_driver::{Error, Result, ServerProcess};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(10);
/// Executions looked at (the batch call's limit).
const BATCH: i32 = 50;
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;

/// An execution id (a UUID).
pub(crate) fn valid_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(id)
}

fn in_flight(q: &QueryExecution) -> bool {
    matches!(q.status().and_then(|s| s.state()), Some(QueryExecutionState::Running | QueryExecutionState::Queued))
}

/// The executions still in flight; `now` in epoch ms.
fn rows(list: &[QueryExecution], now: f64) -> Vec<ServerProcess> {
    list.iter()
        .filter(|q| in_flight(q))
        .map(|q| {
            let state = q.status().and_then(|s| s.state());
            let st = q.statistics();
            ServerProcess {
                id: q.query_execution_id().unwrap_or_default().to_string(),
                status: state.map(|s| s.as_str().to_string()),
                active: true,
                wait: (state == Some(&QueryExecutionState::Queued)).then(|| "en cola".to_string()),
                database: q.query_execution_context().and_then(|c| c.database()).map(str::to_string),
                command: q.statement_type().map(|s| s.as_str().to_string()),
                elapsed_ms: q
                    .status()
                    .and_then(|s| s.submission_date_time())
                    .map(|t| (now - t.as_secs_f64() * 1000.0).max(0.0) as u64),
                cpu_ms: st.and_then(|s| s.engine_execution_time_in_millis()).map(|v| v.max(0) as u64),
                reads: st.and_then(|s| s.data_scanned_in_bytes()).map(|v| v.max(0) as u64),
                sql: q.query().map(|t| t.chars().take(MAX_TEXT).collect()),
                ..Default::default()
            }
        })
        .filter(|p| !p.id.is_empty())
        .collect()
}

impl AthenaSession {
    pub(crate) async fn processes_list(&self) -> Result<Vec<ServerProcess>> {
        let list = tokio::time::timeout(QUERY_LIMIT, async {
            let ids = self.client.list_query_executions().work_group(&self.workgroup).max_results(BATCH).send().await.map_err(err)?;
            let ids = ids.query_execution_ids().to_vec();
            if ids.is_empty() {
                return Ok(Vec::new());
            }
            let got = self.client.batch_get_query_execution().set_query_execution_ids(Some(ids)).send().await.map_err(err)?;
            Ok::<_, Error>(got.query_executions().to_vec())
        })
        .await
        .map_err(|_| Error::Query("la lista de consultas tardó demasiado".into()))??;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
        Ok(rows(&list, now))
    }

    pub(crate) async fn cancel_running(&self, id: &str) -> Result<()> {
        let id = valid_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de ejecución de Athena", id.trim())))?;
        // Stopping a finished execution succeeds silently: check it first.
        let got = self.client.get_query_execution().query_execution_id(id).send().await.map_err(err)?;
        if !got.query_execution().is_some_and(in_flight) {
            return Err(Error::Query(format!("no se pudo cancelar la consulta {id}: ya terminó")));
        }
        self.client.stop_query_execution().query_execution_id(id).send().await.map_err(err)?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_athena::primitives::DateTime;
    use aws_sdk_athena::types::{QueryExecutionStatistics, QueryExecutionStatus, StatementType};

    #[test]
    fn ids_are_checked() {
        assert_eq!(valid_id(" a1b2c3d4-0000-1111-2222-333344445555 "), Some("a1b2c3d4-0000-1111-2222-333344445555"));
        assert_eq!(valid_id("../x"), None);
        assert_eq!(valid_id(""), None);
    }

    #[test]
    fn executions_in_flight_become_processes() {
        let exec = |id: &str, state: QueryExecutionState| {
            QueryExecution::builder()
                .query_execution_id(id)
                .query("SELECT 1")
                .statement_type(StatementType::Dml)
                .status(QueryExecutionStatus::builder().state(state).submission_date_time(DateTime::from_secs(1_700_000_000)).build())
                .statistics(QueryExecutionStatistics::builder().data_scanned_in_bytes(10).build())
                .build()
        };
        let list = [
            exec("a1", QueryExecutionState::Running),
            exec("a2", QueryExecutionState::Queued),
            exec("a3", QueryExecutionState::Succeeded),
        ];
        let ps = rows(&list, 1_700_000_002_500.0);
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].id, "a1");
        assert_eq!(ps[0].elapsed_ms, Some(2500));
        assert_eq!(ps[0].reads, Some(10));
        assert_eq!(ps[0].command.as_deref(), Some("DML"));
        assert!(ps[0].active && ps[0].wait.is_none());
        assert_eq!(ps[1].wait.as_deref(), Some("en cola"));
    }
}
