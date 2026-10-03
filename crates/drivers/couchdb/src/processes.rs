//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! replication ([`dbine_driver::Session::cancel_query`]).
//!
//! CouchDB doesn't list its HTTP requests: what it reports running is
//! `/_active_tasks` (the monitor's table), one row per indexer,
//! compaction or replication. Indexing and compaction are the server's
//! own work (`system`) and can't be stopped from the API. A replication
//! is the user's: its id is its `replication_id`, and a transient one
//! (started with `POST /_replicate`) is cancelled with `_replicate` and
//! `cancel: true`. One defined in a `_replicator` document only stops by
//! changing that document, so cancelling it explains that instead.

use crate::CouchSession;
use dbine_driver::{Error, Result, ServerProcess};
use reqwest::Method;
use serde_json::{json, Value};

/// Rows at most.
const MAX_ROWS: usize = 2000;

fn s<'a>(t: &'a Value, k: &str) -> Option<&'a str> {
    t.get(k).and_then(Value::as_str).map(str::trim).filter(|v| !v.is_empty())
}

fn n(t: &Value, k: &str) -> Option<u64> {
    t.get(k).and_then(Value::as_u64)
}

/// A replication id: "<hex>" plus "+continuous" / "+create_target".
pub(crate) fn valid_replication_id(id: &str) -> bool {
    !id.is_empty() && id.len() <= 128 && id.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '+' | '_'))
}

fn is_replication(t: &Value) -> bool {
    s(t, "type") == Some("replication")
}

fn task_id(t: &Value) -> Option<String> {
    if is_replication(t) {
        if let Some(r) = s(t, "replication_id") {
            return Some(r.to_string());
        }
    }
    s(t, "pid").map(str::to_string)
}

pub(crate) fn rows(tasks: &Value, now_s: u64) -> Vec<ServerProcess> {
    let list = tasks.as_array().map(Vec::as_slice).unwrap_or(&[]);
    list.iter()
        .filter_map(|t| {
            let id = task_id(t)?;
            let repl = is_replication(t);
            let kind = s(t, "type").unwrap_or("").to_string();
            let what = if repl {
                Some(format!("{} → {}", s(t, "source").unwrap_or("?"), s(t, "target").unwrap_or("?")))
            } else {
                let mut d = s(t, "design_document").map(|d| d.to_string()).unwrap_or_default();
                if let Some(p) = n(t, "progress") {
                    d = format!("{d} ({p} %)").trim().to_string();
                }
                Some(d).filter(|d| !d.is_empty())
            };
            Some(ServerProcess {
                id,
                status: Some(if repl && t.get("continuous").and_then(Value::as_bool) == Some(true) { "continua" } else { "en curso" }.into()),
                active: true,
                system: !repl,
                user: s(t, "user").map(str::to_string),
                host: s(t, "node").map(str::to_string),
                database: s(t, "database").or_else(|| s(t, "source")).map(str::to_string),
                command: Some(kind).filter(|k| !k.is_empty()),
                elapsed_ms: n(t, "started_on").map(|st| now_s.saturating_sub(st) * 1000),
                reads: n(t, "docs_read").or_else(|| n(t, "changes_done")),
                writes: n(t, "docs_written"),
                sql: what,
                ..Default::default()
            })
        })
        .take(MAX_ROWS)
        .collect()
}

impl CouchSession {
    pub(crate) async fn processes(&self) -> Result<Vec<ServerProcess>> {
        let tasks = self.call(Method::GET, "/_active_tasks", None).await?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_secs());
        Ok(rows(&tasks, now))
    }

    pub(crate) async fn cancel_task(&self, id: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden detener replicaciones.".into()));
        }
        let id = id.trim();
        let tasks = self.call(Method::GET, "/_active_tasks", None).await?;
        let list = tasks.as_array().map(Vec::as_slice).unwrap_or(&[]);
        let Some(t) = list.iter().find(|t| task_id(t).as_deref() == Some(id)) else {
            return Err(Error::Query(format!("la tarea «{id}» ya terminó o no existe")));
        };
        if !is_replication(t) {
            return Err(Error::Query(format!(
                "CouchDB no permite detener una tarea de tipo «{}» desde la API: solo se cancelan replicaciones",
                s(t, "type").unwrap_or("?")
            )));
        }
        if let Some(doc) = s(t, "doc_id") {
            return Err(Error::Query(format!(
                "esta replicación está definida en el documento «{doc}» de {}: se detiene borrando o editando ese documento",
                s(t, "database").unwrap_or("_replicator")
            )));
        }
        if !valid_replication_id(id) {
            return Err(Error::Query(format!("«{id}» no es un id de replicación")));
        }
        self.call(Method::POST, "/_replicate", Some(&json!({ "replication_id": id, "cancel": true }))).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rows_from_active_tasks() {
        let t = json!([
            {"type": "replication", "replication_id": "a81a78e8+continuous+create_target", "pid": "<0.1.0>", "node": "n@1",
             "source": "http://127.0.0.1:5984/a/", "target": "http://127.0.0.1:5984/b/", "continuous": true, "user": "admin",
             "started_on": 100, "docs_read": 7, "docs_written": 7},
            {"type": "indexer", "pid": "<0.2.0>", "database": "shards/00-1f/db.1", "design_document": "_design/v", "progress": 45, "started_on": 190},
            {"type": "indexer"}
        ]);
        let r = rows(&t, 200);
        assert_eq!(r.len(), 2);
        assert_eq!(r[0].id, "a81a78e8+continuous+create_target");
        assert_eq!((r[0].system, r[0].elapsed_ms, r[0].writes, r[0].status.as_deref()), (false, Some(100_000), Some(7), Some("continua")));
        assert!(r[0].sql.as_deref().unwrap().contains(" → "));
        assert_eq!((r[1].id.as_str(), r[1].system), ("<0.2.0>", true));
        assert_eq!(r[1].sql.as_deref(), Some("_design/v (45 %)"));
        assert!(valid_replication_id("a81a78e8+continuous") && !valid_replication_id("a b") && !valid_replication_id(""));
    }
}
