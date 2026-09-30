//! Blocking chains and ending a transaction, Neo4j only.
//!
//! `SHOW TRANSACTIONS YIELD *` (4.4+) reports, for a transaction waiting on
//! a lock, `status = "Blocked by: [neo4j-transaction-47, …]"` and the lock
//! in `resourceInformation` (`resourceType`, `resourceIds`, `lockMode`,
//! `waitTimeMillis`). The heads are the transactions named there. Killing
//! one is `TERMINATE TRANSACTION 'id'`.
//!
//! Memgraph and Neptune don't make a transaction wait on another: Memgraph
//! aborts the second writer at once with a serialization error, and
//! Neptune doesn't report locks at all.

use crate::monitor::duration_ms;
use crate::{as_text, cypher, GraphSession};
use dbine_driver::{BlockedSession, Error, Result};
use serde_json::{Map, Value};
use std::collections::BTreeSet;

type Row = Map<String, Value>;

/// The transactions a `status` says this one waits for.
pub(crate) fn blockers(status: &str) -> Vec<String> {
    let Some(rest) = status.trim().strip_prefix("Blocked by:") else { return Vec::new() };
    rest.trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(',')
        .map(|s| s.trim().trim_matches('"').to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

fn text(r: &Row, k: &str) -> Option<String> {
    r.get(k).map(as_text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// "NODE 2, 7" from `resourceInformation`.
fn object(r: &Row) -> Option<String> {
    let info = r.get("resourceInformation")?.as_object()?;
    let kind = info.get("resourceType").map(as_text).filter(|s| !s.is_empty())?;
    let ids: Vec<String> = info.get("resourceIds").and_then(Value::as_array).map(|a| a.iter().map(as_text).collect()).unwrap_or_default();
    Some(if ids.is_empty() { kind } else { format!("{kind} {}", ids.join(", ")) })
}

/// The chain out of `SHOW TRANSACTIONS YIELD *` rows.
pub(crate) fn chain(rows: &[Row]) -> Vec<BlockedSession> {
    let by_id = |id: &str| rows.iter().find(|r| text(r, "transactionId").as_deref() == Some(id));
    let mut out = Vec::new();
    let mut heads = BTreeSet::new();
    let mut waiters = BTreeSet::new();
    for r in rows {
        let status = text(r, "status").unwrap_or_default();
        let blocked_by = blockers(&status);
        let Some(first) = blocked_by.first().cloned() else { continue };
        let Some(id) = text(r, "transactionId") else { continue };
        let info = r.get("resourceInformation").and_then(Value::as_object);
        let mode = info.and_then(|i| i.get("lockMode")).map(as_text).filter(|s| !s.is_empty());
        let waited = info
            .and_then(|i| i.get("waitTimeMillis"))
            .and_then(Value::as_f64)
            .or_else(|| r.get("currentQueryWaitTime").and_then(duration_ms))
            .or_else(|| r.get("waitTime").and_then(duration_ms));
        heads.extend(blocked_by.iter().cloned());
        waiters.insert(id.clone());
        out.push(BlockedSession {
            id,
            blocked_by: Some(first),
            user: text(r, "username"),
            client: client(r),
            database: text(r, "database"),
            wait: Some(match mode {
                Some(m) => format!("Esperando un bloqueo {m}"),
                None => "Esperando un bloqueo".into(),
            }),
            waited_ms: waited.map(|v| v.max(0.0) as u64),
            object: object(r),
            sql: text(r, "currentQuery"),
        });
    }
    // Heads not already listed as waiters themselves.
    for h in heads.difference(&waiters) {
        let r = by_id(h);
        let running = r.and_then(|r| text(r, "currentQuery"));
        out.push(BlockedSession {
            id: h.clone(),
            blocked_by: None,
            user: r.and_then(|r| text(r, "username")),
            client: r.and_then(client),
            database: r.and_then(|r| text(r, "database")),
            wait: Some(match (r.and_then(|r| text(r, "status")), &running) {
                (_, None) if r.is_some() => "inactiva con transacción abierta".into(),
                (Some(s), _) => s,
                (None, _) => "retiene el bloqueo".into(),
            }),
            waited_ms: r.and_then(|r| r.get("elapsedTime")).and_then(duration_ms).map(|v| v.max(0.0) as u64),
            object: None,
            sql: running,
        });
    }
    out
}

fn client(r: &Row) -> Option<String> {
    let addr = text(r, "clientAddress");
    let app = r.get("metaData").and_then(|m| m.get("app")).map(as_text).filter(|s| !s.is_empty());
    match (addr, app) {
        (Some(a), Some(p)) => Some(format!("{a} · {p}")),
        (a, p) => a.or(p),
    }
}

/// A Neo4j transaction id: `<database>-transaction-<n>`.
pub(crate) fn valid_id(id: &str) -> bool {
    let Some((db, n)) = id.rsplit_once("-transaction-") else { return false };
    !db.is_empty()
        && db.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_'))
        && !n.is_empty()
        && n.chars().all(|c| c.is_ascii_digit())
}

pub async fn blocking(s: &mut GraphSession) -> Result<Vec<BlockedSession>> {
    let rows = s.records("SHOW TRANSACTIONS YIELD *").await?;
    Ok(chain(&rows))
}

pub async fn kill(s: &mut GraphSession, id: &str) -> Result<()> {
    let id = id.trim();
    if !valid_id(id) {
        return Err(Error::Query(format!("«{id}» no es un id de transacción de Neo4j")));
    }
    let rows = s.records(&format!("TERMINATE TRANSACTION {}", cypher::string(id))).await?;
    // Neo4j answers with one row per id, its `message` saying whether it was found.
    match rows.first().and_then(|r| text(r, "message")) {
        Some(m) if !m.to_ascii_lowercase().contains("terminated") => Err(Error::Query(format!("No se pudo terminar {id}: {m}"))),
        _ => Ok(()),
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
    fn parses_blocked_by() {
        assert_eq!(blockers("Blocked by: [neo4j-transaction-47]"), vec!["neo4j-transaction-47"]);
        assert_eq!(blockers("Blocked by: [a-transaction-1, a-transaction-2]"), vec!["a-transaction-1", "a-transaction-2"]);
        assert!(blockers("Running").is_empty());
    }

    #[test]
    fn builds_the_chain() {
        let rows = vec![
            row(json!({"transactionId": "neo4j-transaction-50", "status": "Blocked by: [neo4j-transaction-47]",
                "username": "neo4j", "database": "neo4j", "clientAddress": "127.0.0.1:1", "currentQuery": "MATCH (n) SET n.v = 2",
                "resourceInformation": {"waitTimeMillis": 3033, "lockMode": "EXCLUSIVE", "resourceType": "NODE", "resourceIds": [2]}})),
            row(json!({"transactionId": "neo4j-transaction-47", "status": "Running", "username": "neo4j", "database": "neo4j",
                "currentQuery": "MATCH (n) SET n.v = 1", "elapsedTime": "PT6.5S"})),
            row(json!({"transactionId": "neo4j-transaction-9", "status": "Running"})),
        ];
        let c = chain(&rows);
        assert_eq!(c.len(), 2);
        assert_eq!(c[0].blocked_by.as_deref(), Some("neo4j-transaction-47"));
        assert_eq!(c[0].waited_ms, Some(3033));
        assert_eq!(c[0].object.as_deref(), Some("NODE 2"));
        assert_eq!(c[1].id, "neo4j-transaction-47");
        assert_eq!(c[1].blocked_by, None);
        assert_eq!(c[1].waited_ms, Some(6500));
    }

    #[test]
    fn validates_ids() {
        assert!(valid_id("neo4j-transaction-47"));
        assert!(valid_id("my.db_2-transaction-1"));
        assert!(!valid_id("neo4j-transaction-"));
        assert!(!valid_id("x' OR 1=1 //-transaction-1"));
        assert!(!valid_id("47"));
    }
}
