//! The process list ([`dbine_driver::Session::processes`]) and stopping
//! another client's work ([`dbine_driver::Session::cancel_query`]) per
//! flavor.
//!
//! - Neo4j (4.4+): `SHOW TRANSACTIONS YIELD *`, one row per open
//!   transaction (the monitor reads the same rows). The engine has no way
//!   to stop a query and keep its transaction: `TERMINATE TRANSACTION`
//!   stops the query and rolls the transaction back, leaving the client's
//!   connection open, so that's the cancel. `kill_session` (blocking.rs)
//!   runs the same command, with the same ids.
//! - Memgraph: `SHOW TRANSACTIONS` (`transaction_id`, `query`, `status`,
//!   `elapsed_ms`) and `TERMINATE TRANSACTIONS 'id'`, which likewise aborts
//!   the transaction and keeps the connection.
//! - Neptune: `/openCypher/status` lists the running queries (it has no
//!   sessions) and cancelling one is `cancelQuery` on the same endpoint.

use crate::blocking::blockers;
use crate::monitor::duration_ms;
use crate::{as_text, cypher, Flavor, GraphSession};
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::{Map, Value};
use std::time::Duration;

type Row = Map<String, Value>;

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
const MAX_ROWS: usize = 2000;

fn text(r: &Row, k: &str) -> Option<String> {
    r.get(k).map(as_text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

fn clip(s: String) -> String {
    if s.chars().count() > MAX_TEXT {
        s.chars().take(MAX_TEXT).collect()
    } else {
        s
    }
}

fn ms(r: &Row, k: &str) -> Option<u64> {
    r.get(k).and_then(duration_ms).map(|v| v.max(0.0) as u64)
}

fn count(r: &Row, k: &str) -> Option<u64> {
    r.get(k).and_then(Value::as_f64).map(|v| v.max(0.0) as u64)
}

/// The statement's first word ("MATCH", "CALL"…).
fn command(q: &str) -> Option<String> {
    q.split_whitespace().next().map(|w| w.trim_end_matches(';').to_uppercase()).filter(|w| !w.is_empty())
}

fn tagged(r: &Row, meta: &str, tag: &str) -> bool {
    r.get(meta).and_then(|m| m.get("dbine")).and_then(Value::as_str) == Some(tag)
}

/// Neo4j's `SHOW TRANSACTIONS YIELD *` rows → processes.
pub(crate) fn neo4j_rows(rows: &[Row], tag: &str) -> Vec<ServerProcess> {
    rows.iter()
        .take(MAX_ROWS)
        .filter_map(|r| {
            let id = text(r, "transactionId")?;
            let query = text(r, "currentQuery");
            let active = query.is_some();
            let status = text(r, "status");
            let blocked_by = status.as_deref().map(blockers).and_then(|b| b.into_iter().next());
            let lock = r.get("resourceInformation").and_then(|i| i.get("lockMode")).map(as_text).filter(|s| !s.is_empty());
            let wait = match (&blocked_by, lock) {
                (Some(_), Some(m)) => Some(format!("Esperando un bloqueo {m}")),
                (Some(_), None) => Some("Esperando un bloqueo".into()),
                // "planning", "waiting"… (plain "running" says nothing more).
                _ => text(r, "currentQueryStatus").filter(|s| active && s != "running"),
            };
            let (elapsed, cpu, hits, faults) = if active {
                (ms(r, "currentQueryElapsedTime"), ms(r, "currentQueryCpuTime"), count(r, "currentQueryPageHits"), count(r, "currentQueryPageFaults"))
            } else {
                (ms(r, "idleTime").or_else(|| ms(r, "elapsedTime")), ms(r, "cpuTime"), count(r, "pageHits"), count(r, "pageFaults"))
            };
            let app = r.get("metaData").and_then(|m| m.get("app")).map(as_text).filter(|s| !s.is_empty());
            Some(ServerProcess {
                status: if blocked_by.is_some() { Some("Bloqueada".into()) } else { status },
                active,
                own: tagged(r, "metaData", tag),
                user: text(r, "username"),
                host: text(r, "clientAddress"),
                program: app.or_else(|| text(r, "protocol")),
                database: text(r, "database"),
                command: query.as_deref().and_then(command),
                elapsed_ms: elapsed,
                cpu_ms: cpu,
                reads: match (hits, faults) {
                    (None, None) => None,
                    (h, f) => Some(h.unwrap_or(0) + f.unwrap_or(0)),
                },
                wait,
                blocked_by,
                sql: query.map(clip),
                id,
                ..Default::default()
            })
        })
        .collect()
}

/// Memgraph's `SHOW TRANSACTIONS` rows → processes. `query` is the list of
/// statements the transaction ran, the last one the current.
pub(crate) fn memgraph_rows(rows: &[Row], tag: &str) -> Vec<ServerProcess> {
    rows.iter()
        .take(MAX_ROWS)
        .filter_map(|r| {
            let id = text(r, "transaction_id")?;
            let queries: Vec<String> = match r.get("query") {
                Some(Value::Array(a)) => a.iter().map(as_text).map(|q| q.trim().to_string()).filter(|q| !q.is_empty()).collect(),
                Some(v) => Some(as_text(v).trim().to_string()).filter(|q| !q.is_empty()).into_iter().collect(),
                None => Vec::new(),
            };
            let status = text(r, "status");
            // Older versions have no status: a transaction listed is running.
            // (An explicit transaction between statements says "running" too.)
            let active = status.as_deref().is_none_or(|s| s.eq_ignore_ascii_case("running"));
            let current = queries.last().cloned();
            Some(ServerProcess {
                id,
                status,
                active,
                own: tagged(r, "metadata", tag),
                user: text(r, "username"),
                command: current.as_deref().filter(|_| active).and_then(command),
                elapsed_ms: ms(r, "elapsed_ms"),
                sql: (!queries.is_empty()).then(|| clip(queries.join(";\n"))),
                ..Default::default()
            })
        })
        .collect()
}

/// Neptune's `/openCypher/status` reply → processes (running queries).
pub(crate) fn neptune_rows(status: &Value) -> Vec<ServerProcess> {
    status
        .get("queries")
        .and_then(Value::as_array)
        .map(|a| a.as_slice())
        .unwrap_or_default()
        .iter()
        .take(MAX_ROWS)
        .filter_map(|q| {
            let id = q.get("queryId").map(as_text).filter(|s| !s.is_empty())?;
            let stats = q.get("queryEvalStats");
            let stat = |k: &str| stats.and_then(|s| s.get(k)).and_then(Value::as_f64).map(|v| v.max(0.0) as u64);
            let cancelled = stats.and_then(|s| s.get("cancelled")).and_then(Value::as_bool) == Some(true);
            let sql = q.get("queryString").map(as_text).filter(|s| !s.trim().is_empty());
            Some(ServerProcess {
                id,
                status: Some(if cancelled { "cancelando" } else { "en ejecución" }.into()),
                active: true,
                command: sql.as_deref().and_then(command),
                elapsed_ms: stat("elapsed"),
                wait: stat("waited").filter(|w| *w > 0).map(|w| format!("En cola {w} ms")),
                sql: sql.map(clip),
                ..Default::default()
            })
        })
        .collect()
}

/// A Memgraph transaction id (a number).
fn memgraph_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 20 && id.chars().all(|c| c.is_ascii_digit())
}

/// A Neptune query id (a UUID).
fn neptune_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')
}

const OWN: &str = "esa es la transacción con la que DBine está consultando: no se puede cancelar desde acá";

pub(crate) async fn processes(s: &mut GraphSession) -> Result<Vec<ServerProcess>> {
    let within = |_: tokio::time::error::Elapsed| Error::Query("la lista de procesos no respondió a tiempo".into());
    match s.flavor {
        Flavor::Neo4j => {
            let rows = tokio::time::timeout(QUERY_LIMIT, s.records("SHOW TRANSACTIONS YIELD *")).await.map_err(within)??;
            Ok(neo4j_rows(&rows, &s.tag))
        }
        Flavor::Memgraph => {
            let rows = tokio::time::timeout(QUERY_LIMIT, s.records("SHOW TRANSACTIONS")).await.map_err(within)??;
            Ok(memgraph_rows(&rows, &s.tag))
        }
        Flavor::Neptune => {
            let c = s.neptune_client().expect("neptune");
            let st = tokio::time::timeout(QUERY_LIMIT, c.json("/openCypher/status")).await.map_err(within)??;
            Ok(neptune_rows(&st))
        }
    }
}

pub(crate) async fn cancel(s: &mut GraphSession, id: &str) -> Result<()> {
    let id = id.trim();
    s.refuse_if_read_only("cancelar consultas de otras sesiones")?;
    match s.flavor {
        Flavor::Neo4j => {
            if !crate::blocking::valid_id(id) {
                return Err(Error::Query(format!("«{id}» no es un id de transacción de Neo4j")));
            }
            let rows = s.records(&format!("SHOW TRANSACTIONS {} YIELD metaData", cypher::string(id))).await?;
            if rows.iter().any(|r| tagged(r, "metaData", &s.tag)) {
                return Err(Error::Query(OWN.into()));
            }
            if rows.is_empty() {
                return Err(Error::Query(format!("no se pudo cancelar {id}: la transacción ya terminó")));
            }
            crate::blocking::kill(s, id).await
        }
        Flavor::Memgraph => {
            if !memgraph_id(id) {
                return Err(Error::Query(format!("«{id}» no es un id de transacción de Memgraph")));
            }
            let rows = s.records("SHOW TRANSACTIONS").await?;
            let Some(r) = rows.iter().find(|r| text(r, "transaction_id").as_deref() == Some(id)) else {
                return Err(Error::Query(format!("no se pudo cancelar la transacción {id}: ya terminó")));
            };
            if tagged(r, "metadata", &s.tag) {
                return Err(Error::Query(OWN.into()));
            }
            let rows = s.records(&format!("TERMINATE TRANSACTIONS {}", cypher::string(id))).await?;
            // One row per id: `killed` false when it was gone or not ours to kill.
            match rows.first().and_then(|r| r.get("killed")).and_then(Value::as_bool) {
                Some(false) => Err(Error::Query(format!(
                    "no se pudo cancelar la transacción {id}: ya terminó o tu usuario no tiene permiso (TRANSACTION_MANAGEMENT)"
                ))),
                _ => Ok(()),
            }
        }
        Flavor::Neptune => {
            if !neptune_id(id) {
                return Err(Error::Query(format!("«{id}» no es un id de consulta de Neptune")));
            }
            s.neptune_client().expect("neptune").cancel(id).await
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn neo4j_transactions() {
        let rows = vec![
            row(json!({"transactionId": "neo4j-transaction-50", "status": "Blocked by: [neo4j-transaction-47]", "username": "app",
                "database": "neo4j", "clientAddress": "10.0.0.5:4000", "currentQuery": "MATCH (n) SET n.v = 2", "currentQueryStatus": "waiting",
                "metaData": {"app": "billing"}, "protocol": "bolt", "resourceInformation": {"lockMode": "EXCLUSIVE"},
                "currentQueryElapsedTime": "PT3.5S", "currentQueryPageHits": 10, "currentQueryPageFaults": 2})),
            row(json!({"transactionId": "neo4j-transaction-47", "status": "Running", "currentQuery": "", "idleTime": "PT12S",
                "elapsedTime": "PT20S", "protocol": "bolt"})),
            row(json!({"transactionId": "neo4j-transaction-60", "status": "Running", "currentQuery": "SHOW TRANSACTIONS YIELD *",
                "metaData": {"dbine": "tag-1"}, "currentQueryStatus": "running"})),
        ];
        let p = neo4j_rows(&rows, "tag-1");
        assert_eq!(p[0].blocked_by.as_deref(), Some("neo4j-transaction-47"));
        assert_eq!(p[0].wait.as_deref(), Some("Esperando un bloqueo EXCLUSIVE"));
        assert_eq!((p[0].elapsed_ms, p[0].reads), (Some(3500), Some(12)));
        assert_eq!((p[0].command.as_deref(), p[0].program.as_deref()), (Some("MATCH"), Some("billing")));
        assert!(p[0].active && !p[0].own);
        assert!(!p[1].active && p[1].sql.is_none());
        assert_eq!(p[1].elapsed_ms, Some(12000));
        assert!(p[2].own && p[2].wait.is_none());
    }

    #[test]
    fn memgraph_transactions() {
        let rows = vec![
            row(json!({"username": "", "transaction_id": "9223372036854775808", "query": ["SHOW TRANSACTIONS"], "status": "running",
                "metadata": {"dbine": "t"}, "elapsed_ms": 0})),
            row(json!({"username": "app", "transaction_id": "12", "query": ["CREATE (n)", "MATCH (n) RETURN n"], "status": "idle",
                "metadata": {}, "elapsed_ms": 900})),
        ];
        let p = memgraph_rows(&rows, "t");
        assert!(p[0].own && p[0].active);
        assert_eq!(p[0].command.as_deref(), Some("SHOW"));
        assert!(!p[1].active && p[1].command.is_none());
        assert_eq!(p[1].sql.as_deref(), Some("CREATE (n);\nMATCH (n) RETURN n"));
        assert_eq!(p[1].elapsed_ms, Some(900));
    }

    #[test]
    fn neptune_queries() {
        let st = json!({"acceptedQueryCount": 3, "runningQueryCount": 1, "queries": [
            {"queryId": "4b9d8a0f-6c1e-4f0a-9b1a-1c2d3e4f5a6b", "queryString": "MATCH (n) RETURN count(n)",
             "queryEvalStats": {"waited": 0, "elapsed": 2300, "cancelled": false}}]});
        let p = neptune_rows(&st);
        assert_eq!(p.len(), 1);
        assert!(p[0].active);
        assert_eq!((p[0].elapsed_ms, p[0].wait.as_deref()), (Some(2300), None));
    }

    #[test]
    fn ids_are_validated() {
        assert!(memgraph_id("9223372036854775808"));
        assert!(!memgraph_id("1' OR true"));
        assert!(neptune_id("4b9d8a0f-6c1e-4f0a-9b1a-1c2d3e4f5a6b"));
        assert!(!neptune_id("x&queryId=y"));
    }
}
