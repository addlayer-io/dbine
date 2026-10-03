//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! query ([`dbine_driver::Session::cancel_query`]).
//!
//! Drill's REST sessions can't be ended from outside, so the list is the
//! queries running now (`/profiles/running.json`, the monitor's "Consultas
//! en curso"): the id is the query id, and cancelling it is
//! `/profiles/cancel/{id}`. Drill cuts the listed text to 150 characters.

use crate::{text, DrillSession};
use dbine_driver::{Error, Result, ServerProcess};
use serde_json::Value;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Rows at most.
const MAX_ROWS: usize = 2000;

/// A query id as Drill prints it (`1a2b3c4d-…`: hex and dashes).
pub(crate) fn valid_id(id: &str) -> Option<&str> {
    let id = id.trim();
    (!id.is_empty() && id.len() <= 64 && id.chars().all(|c| c.is_ascii_hexdigit() || c == '-')).then_some(id)
}

/// The statement's first word, upper-cased ("SELECT", "CREATE"…).
fn command(sql: &str) -> Option<String> {
    let word: String = sql.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect();
    (!word.is_empty()).then(|| word.to_ascii_uppercase())
}

fn opt(r: &Value, key: &str) -> Option<String> {
    r.get(key).map(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty())
}

/// One entry of `runningQueries`; `now` in epoch ms.
fn row(r: &Value, now: u64) -> ServerProcess {
    let sql = opt(r, "query");
    let state = opt(r, "state");
    ServerProcess {
        id: opt(r, "queryId").unwrap_or_default(),
        // Queued (ENQUEUED, STARTING) or running: all of them are in flight.
        active: true,
        status: state,
        user: opt(r, "user"),
        // The drillbit that runs it (its foreman).
        host: opt(r, "foreman"),
        command: sql.as_deref().and_then(command),
        elapsed_ms: r.get("startTime").and_then(Value::as_u64).filter(|t| *t > 0).map(|t| now.saturating_sub(t)),
        sql,
        ..Default::default()
    }
}

impl DrillSession {
    pub(crate) async fn processes_list(&self) -> Result<Vec<ServerProcess>> {
        let v = tokio::time::timeout(QUERY_LIMIT, self.conn.get("/profiles/running.json"))
            .await
            .map_err(|_| Error::Query("la lista de consultas tardó demasiado".into()))??;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as u64).unwrap_or(0);
        Ok(v.get("runningQueries")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|r| row(r, now))
            .filter(|p| !p.id.is_empty())
            .take(MAX_ROWS)
            .collect())
    }

    pub(crate) async fn cancel_running(&self, id: &str) -> Result<()> {
        let id = valid_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de consulta de Drill", id.trim())))?;
        let conn = &self.conn;
        let resp = conn
            .req(conn.http.get(format!("{}/profiles/cancel/{id}", conn.base)))
            .send()
            .await
            .map_err(crate::http_error)?;
        let status = resp.status();
        let body = resp.text().await.unwrap_or_default();
        // "Cancelled query … on … node." or "Failure attempting to cancel
        // query …", both with 200.
        if status.is_success() && body.trim_start().starts_with("Cancelled") {
            Ok(())
        } else {
            Err(Error::Query(format!("no se pudo cancelar la consulta {id}: ya terminó, no existe o tu usuario no tiene permiso")))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_are_checked() {
        assert_eq!(valid_id(" 1a2b3c4d-5e6f-7a8b-9c0d-e1f2a3b4c5d6 "), Some("1a2b3c4d-5e6f-7a8b-9c0d-e1f2a3b4c5d6"));
        assert_eq!(valid_id("../status"), None);
        assert_eq!(valid_id(""), None);
    }

    #[test]
    fn running_queries_become_processes() {
        let r = json!({"queryId": "1a2b", "startTime": 1000, "user": "anonymous", "foreman": "drill1",
                       "query": "select * from cp.`employee.json`", "state": "RUNNING"});
        let p = row(&r, 3500);
        assert_eq!(p.id, "1a2b");
        assert!(p.active && !p.own);
        assert_eq!(p.elapsed_ms, Some(2500));
        assert_eq!(p.command.as_deref(), Some("SELECT"));
        assert_eq!(p.status.as_deref(), Some("RUNNING"));
    }
}
