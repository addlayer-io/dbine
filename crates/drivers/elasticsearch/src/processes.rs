//! The process list ([`dbine_driver::Session::processes`]) and cancelling
//! another request ([`dbine_driver::Session::cancel_query`]) for
//! Elasticsearch, OpenSearch and Open Distro.
//!
//! A cluster has no client sessions: it knows running tasks. The list is
//! the monitor's `_tasks?detailed=true`, one row per top-level task (the
//! per-shard and per-node children are folded into their parent). The id
//! is the task id ("<node>:<n>"), cancelled with `POST _tasks/<id>/_cancel`
//! (children go with it). Persistent and internal tasks are the cluster's
//! own work (`system`); requests carrying this session's `X-Opaque-Id` are
//! DBine's own. OpenSearch adds the tasks' CPU time (`resource_stats`).

use crate::json::J;
use crate::monitor::f;
use dbine_driver::{Error, Result, ServerProcess};
use std::collections::HashSet;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
pub(crate) const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a task's description (the search source).
const MAX_TEXT: usize = 20000;
const MAX_ROWS: usize = 2000;

pub(crate) const LIST_PATH: &str = "/_tasks?detailed=true";

/// A task id as `_tasks` gives it: "<node id>:<number>".
pub(crate) fn valid_task_id(id: &str) -> bool {
    id.rsplit_once(':').is_some_and(|(node, n)| {
        !node.is_empty()
            && node.len() <= 64
            && node.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '-' | '_'))
            && !n.is_empty()
            && n.len() <= 19
            && n.chars().all(|c| c.is_ascii_digit())
    })
}

/// "indices[a,b], search_type[…], source[…]" → "a,b".
fn indices(desc: &str) -> Option<String> {
    let at = desc.find("indices[")? + 8;
    let end = desc[at..].find(']')? + at;
    Some(desc[at..end].to_string()).filter(|s| !s.is_empty())
}

pub(crate) fn rows(tasks: &J, opaque_id: &str) -> Vec<ServerProcess> {
    let all: Vec<(&String, &J)> = tasks
        .get("nodes")
        .and_then(J::as_obj)
        .into_iter()
        .flatten()
        .flat_map(|(_, node)| node.get("tasks").and_then(J::as_obj).into_iter().flatten())
        .map(|(id, t)| (id, t))
        .collect();
    let ids: HashSet<&str> = all.iter().map(|(id, _)| id.as_str()).collect();
    let mut out: Vec<ServerProcess> = all
        .iter()
        .filter(|(_, t)| !t.get("parent_task_id").and_then(J::as_str).is_some_and(|p| ids.contains(p)))
        .map(|(id, t)| {
            let text = |k: &str| t.get(k).map(J::text).filter(|v| !v.is_empty());
            let action = text("action").unwrap_or_default();
            let desc = text("description");
            let header = t.at(&["headers", "X-Opaque-Id"]).and_then(J::as_str);
            let cancelled = t.get("cancelled").and_then(J::as_bool).unwrap_or(false);
            ServerProcess {
                id: (*id).clone(),
                status: Some(if cancelled { "cancelándose" } else { "ejecutando" }.into()),
                active: true,
                system: text("type").as_deref() == Some("persistent") || action.starts_with("internal:"),
                own: header == Some(opaque_id),
                program: header.filter(|h| !h.is_empty()).map(str::to_string),
                database: desc.as_deref().and_then(indices),
                command: Some(action).filter(|a| !a.is_empty()),
                elapsed_ms: f(t, &["running_time_in_nanos"]).map(|ns| (ns / 1e6) as u64),
                cpu_ms: f(t, &["resource_stats", "total", "cpu_time_in_nanos"]).map(|ns| (ns / 1e6) as u64),
                sql: desc.map(|d| crate::http::clip(&d, MAX_TEXT)),
                ..Default::default()
            }
        })
        .collect();
    out.sort_by(|a, b| a.system.cmp(&b.system).then(b.elapsed_ms.cmp(&a.elapsed_ms)));
    out.truncate(MAX_ROWS);
    out
}

/// Checks before cancelling, from `GET _tasks/<id>`: it must be running,
/// cancellable and not this session's.
pub(crate) fn check_cancel(id: &str, status: u16, body: &str, opaque_id: &str) -> Result<()> {
    if status == 404 {
        return Err(Error::Query(format!("la tarea {id} ya terminó o no existe")));
    }
    let j = J::parse(body).map_err(|e| Error::Query(format!("Respuesta inesperada del servidor: {e}")))?;
    if j.get("completed").and_then(J::as_bool) == Some(true) {
        return Err(Error::Query(format!("la tarea {id} ya terminó")));
    }
    let Some(task) = j.get("task") else {
        return Err(Error::Query(format!("la tarea {id} ya terminó o no existe")));
    };
    if task.at(&["headers", "X-Opaque-Id"]).and_then(J::as_str) == Some(opaque_id) {
        return Err(Error::Query("esa es una tarea de la sesión con la que DBine está consultando: no se puede cancelar desde acá".into()));
    }
    if task.get("cancellable").and_then(J::as_bool) == Some(false) {
        return Err(Error::Query(format!("la tarea {id} no se puede cancelar (el servidor no la marca como cancelable)")));
    }
    Ok(())
}

/// `POST _tasks/<id>/_cancel` answers 200 with failures listed inside.
pub(crate) fn cancel_failure(body: &str) -> Option<String> {
    let j = J::parse(body).ok()?;
    ["node_failures", "task_failures"].iter().find_map(|k| {
        let first = j.get(k)?.as_arr()?.first()?;
        Some(first.at(&["caused_by", "reason"]).or_else(|| first.get("reason")).map(J::text).unwrap_or_else(|| first.compact()))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn top_level_tasks_only() {
        let t = J::parse(
            r#"{"nodes":{"n1":{"name":"a","tasks":{
            "n1:149":{"type":"transport","action":"cluster:monitor/tasks/lists","description":"","running_time_in_nanos":5000000,"headers":{"X-Opaque-Id":"me"}},
            "n1:150":{"type":"transport","action":"cluster:monitor/tasks/lists[n]","parent_task_id":"n1:149","running_time_in_nanos":1,"headers":{}},
            "n1:43":{"type":"persistent","action":"health-node[c]","parent_task_id":"cluster:61","description":"id=health-node","running_time_in_nanos":9},
            "n1:7":{"type":"transport","action":"indices:data/read/search","description":"indices[films], search_type[QUERY_THEN_FETCH], source[{\"query\":{}}]",
                    "running_time_in_nanos":2500000000,"cancellable":true,"headers":{"X-Opaque-Id":"app"},
                    "resource_stats":{"total":{"cpu_time_in_nanos":120000000}}}}}}}"#,
        )
        .unwrap();
        let r = rows(&t, "me");
        assert_eq!(r.iter().map(|p| p.id.as_str()).collect::<Vec<_>>(), ["n1:7", "n1:149", "n1:43"]);
        let s = &r[0];
        assert_eq!((s.database.as_deref(), s.program.as_deref(), s.elapsed_ms, s.cpu_ms), (Some("films"), Some("app"), Some(2500), Some(120)));
        assert!(s.sql.as_deref().unwrap().contains("source["));
        assert!(r[1].own && !r[1].system);
        assert!(r[2].system);
    }

    #[test]
    fn task_ids_and_cancel_checks() {
        assert!(valid_task_id("gT921SvoSiywpqoXvL4OBw:149") && valid_task_id("uycW5Z-tSoO_UtEDkqwBWQ:1"));
        assert!(!valid_task_id("149") && !valid_task_id("a:b") && !valid_task_id("x/../_cluster:1") && !valid_task_id(":1"));
        assert!(check_cancel("n:1", 404, "", "me").is_err());
        assert!(check_cancel("n:1", 200, r#"{"completed":true,"task":{}}"#, "me").is_err());
        assert!(check_cancel("n:1", 200, r#"{"completed":false,"task":{"headers":{"X-Opaque-Id":"me"}}}"#, "me").is_err());
        assert!(check_cancel("n:1", 200, r#"{"completed":false,"task":{"cancellable":false}}"#, "me").is_err());
        assert!(check_cancel("n:1", 200, r#"{"completed":false,"task":{"cancellable":true}}"#, "me").is_ok());
        assert_eq!(cancel_failure(r#"{"node_failures":[{"caused_by":{"reason":"no"}}]}"#).as_deref(), Some("no"));
        assert_eq!(cancel_failure(r#"{"nodes":{}}"#), None);
    }
}
