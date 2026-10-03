//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! query ([`dbine_driver::Session::cancel_query`]). ksqlDB has no client
//! sessions (the REST API is stateless): the rows are the queries of
//! `SHOW QUERIES` (the monitor runs its `EXTENDED` form), persistent ones
//! (`CREATE … AS SELECT`, which run until terminated) and the push queries
//! clients have open. Cancelling a persistent query pauses it (`PAUSE`,
//! undone with `RESUME`) instead of `TERMINATE`, which would stop it for
//! good; a push query ends for its client (`TERMINATE`).

use crate::{text, KsqlSession};
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::Value;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;

/// `SHOW QUERIES` entities → processes. `own` is the push query this
/// session has open, if any.
pub(crate) fn rows(ents: &[Value], own: Option<&str>) -> Vec<ServerProcess> {
    ents.iter()
        .filter_map(|e| e.get("queries").and_then(Value::as_array))
        .flatten()
        .filter_map(|q| {
            let id = q.get("id").map(|i| i.get("id").map(text).unwrap_or_else(|| text(i))).filter(|s| !s.is_empty())?;
            // `statusCount` ({"RUNNING": 2}) is what each server is doing;
            // `state` (recent servers) can lag behind it: a paused query
            // still says RUNNING there. `state` only when there are no counts.
            let state = q
                .get("statusCount")
                .and_then(Value::as_object)
                .map(|counts| counts.iter().filter(|(_, n)| n.as_u64().unwrap_or(0) > 0).map(|(k, _)| k.as_str()).collect::<Vec<_>>().join(", "))
                .filter(|s| !s.is_empty())
                .or_else(|| q.get("state").map(text).filter(|s| !s.is_empty()));
            let sql = q.get("queryString").map(text).filter(|s| !s.trim().is_empty());
            let sinks = q.get("sinks").and_then(Value::as_array).map(|a| a.iter().map(text).collect::<Vec<_>>().join(", ")).filter(|s| !s.is_empty());
            Some(ServerProcess {
                active: state.as_deref().is_some_and(|s| s.split(", ").any(|s| s == "RUNNING")),
                own: own == Some(id.as_str()),
                // What it writes to, for a persistent query.
                database: sinks,
                command: q.get("queryType").map(text).filter(|s| !s.is_empty()),
                sql: sql.map(|s| s.chars().take(MAX_TEXT).collect()),
                status: state,
                id,
                ..Default::default()
            })
        })
        .take(MAX_ROWS)
        .collect()
}

/// A query id (`CTAS_ORDERS_7`, `transient_ORDERS_123…`): letters, digits
/// and `_` / `-`.
fn valid_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 256 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-'))
}

impl KsqlSession {
    fn open_query(&self) -> Option<String> {
        self.in_flight.query_id.lock().unwrap_or_else(|e| e.into_inner()).clone()
    }

    pub(crate) async fn processes(&mut self) -> Result<Vec<ServerProcess>> {
        let ents = tokio::time::timeout(QUERY_LIMIT, self.ksql("SHOW QUERIES"))
            .await
            .map_err(|_| Error::Query("SHOW QUERIES no respondió a tiempo".into()))??;
        Ok(rows(&ents, self.open_query().as_deref()))
    }

    pub(crate) async fn terminate(&mut self, id: &str) -> Result<()> {
        let id = id.trim();
        if !valid_id(id) {
            return Err(Error::Query(format!("«{id}» no es un id de consulta de ksqlDB")));
        }
        if self.open_query().as_deref() == Some(id) {
            return Err(Error::Query("esa es la consulta que DBine tiene abierta en esta sesión: se detiene desde su pestaña".into()));
        }
        let persistent = self.processes().await?.into_iter().find(|p| p.id == id).map(|p| p.command.as_deref() == Some("PERSISTENT"));
        let pause = match persistent {
            None => return Err(Error::Query(format!("la consulta {id} ya no está en ejecución"))),
            Some(p) => p,
        };
        let stmt = if pause { "PAUSE" } else { "TERMINATE" };
        // Whole sentences, so the backend catalog translates each one.
        self.ksql(&format!("{stmt} {id}")).await.map(|_| ()).map_err(|e| match e {
            Error::Query(m) if pause => Error::Query(format!("no se pudo pausar la consulta {id}: {m}")),
            Error::Query(m) => Error::Query(format!("no se pudo terminar la consulta {id}: {m}")),
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
        let ents = vec![json!({"@type": "queries", "queries": [
            {"queryString": "CREATE TABLE T AS SELECT V, COUNT(*) C FROM S GROUP BY V EMIT CHANGES;", "sinks": ["T"], "id": "CTAS_T_7",
             "statusCount": {"RUNNING": 1}, "queryType": "PERSISTENT", "state": "RUNNING"},
            {"queryString": "SELECT * FROM S EMIT CHANGES;", "sinks": [], "id": "transient_S_42", "statusCount": {"RUNNING": 1},
             "queryType": "PUSH"},
            {"queryString": "CREATE STREAM X AS SELECT * FROM S;", "sinks": ["X"], "id": "CSAS_X_3", "statusCount": {"ERROR": 1, "RUNNING": 0},
             "queryType": "PERSISTENT"},
            // Paused: 7.x still says RUNNING in `state`.
            {"queryString": "CREATE STREAM Y AS SELECT * FROM S;", "sinks": ["Y"], "id": "CSAS_Y_4", "statusCount": {"PAUSED": 1},
             "queryType": "PERSISTENT", "state": "RUNNING"},
        ]})];
        let p = rows(&ents, Some("transient_S_42"));
        assert_eq!(p.len(), 4);
        assert!(!p[3].active && p[3].status.as_deref() == Some("PAUSED"), "{:?}", p[3]);
        assert!(p[0].active && !p[0].own);
        assert_eq!((p[0].command.as_deref(), p[0].database.as_deref()), (Some("PERSISTENT"), Some("T")));
        assert!(p[1].active && p[1].own && p[1].database.is_none());
        assert_eq!(p[2].status.as_deref(), Some("ERROR"));
        assert!(!p[2].active);
        assert!(valid_id("transient_S_42") && !valid_id("X; DROP STREAM S"));
    }
}
