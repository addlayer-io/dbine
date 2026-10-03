//! The process list ([`dbine_driver::Session::processes`]) and cancelling
//! another client's query ([`dbine_driver::Session::cancel_query`]) per
//! API. InfluxDB has no sessions: the HTTP API is stateless, so the rows
//! are the running queries.
//!
//! - 1.x: `SHOW QUERIES` (`qid`, `query`, `database`, `duration`,
//!   `status`), the monitor's query table, and `KILL QUERY <qid>`.
//! - 2.x: no API lists or stops other clients' queries.
//! - 3.x: the running rows of `system.queries` (the monitor reads the
//!   same table); there's no way to stop one.

use crate::monitor::go_duration;
use dbine_driver::ServerProcess;
use serde_json::Value as J;
use std::collections::BTreeMap;
use std::time::Duration;

/// Longest the list may take: it's polled every few seconds.
pub const QUERY_LIMIT: Duration = Duration::from_secs(5);
/// Characters kept of a query's text.
const MAX_TEXT: usize = 20000;
/// Rows at most.
pub const MAX_ROWS: usize = 2000;

/// Why 2.x has neither, in Spanish (for the error and the docs).
pub const V2_UNSUPPORTED: &str = "InfluxDB 2 no tiene una API que liste ni cancele las consultas de otros clientes";
/// Why 3.x can't cancel, in Spanish.
pub const V3_NO_CANCEL: &str = "InfluxDB 3 no permite cancelar la consulta de otro cliente (no tiene KILL QUERY ni una API para eso)";

/// What 3.x's list runs: its own row is recognised by this text.
pub const V3_QUERIES: &str =
    "SELECT id, query_type, phase, issue_time, query_text, running FROM system.queries WHERE running ORDER BY issue_time LIMIT 2000";

fn clip(s: &str) -> String {
    s.chars().take(MAX_TEXT).collect()
}

fn command(q: &str) -> Option<String> {
    q.split_whitespace().next().map(str::to_uppercase)
}

fn text(v: Option<&J>) -> Option<String> {
    match v? {
        J::Null => None,
        J::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        v => Some(v.to_string()),
    }
}

/// `SHOW QUERIES` result (the first statement's) → processes.
pub fn v1_rows(result: &J) -> Vec<ServerProcess> {
    let mut out = Vec::new();
    for s in result.get("series").and_then(J::as_array).into_iter().flatten() {
        let cols: Vec<&str> = s.get("columns").and_then(J::as_array).into_iter().flatten().filter_map(J::as_str).collect();
        for r in s.get("values").and_then(J::as_array).into_iter().flatten() {
            let row: BTreeMap<&str, &J> = cols.iter().copied().zip(r.as_array().into_iter().flatten()).collect();
            let get = |k: &str| text(row.get(k).copied());
            let Some(id) = get("qid") else { continue };
            let query = get("query").unwrap_or_default();
            let status = get("status");
            out.push(ServerProcess {
                id,
                active: status.as_deref().is_none_or(|s| s == "running"),
                // The list's own SHOW QUERIES.
                own: query.eq_ignore_ascii_case("SHOW QUERIES"),
                status,
                database: get("database"),
                command: command(&query),
                elapsed_ms: get("duration").as_deref().and_then(go_duration).map(|s| (s.max(0.0) * 1000.0) as u64),
                sql: Some(clip(&query)).filter(|q| !q.is_empty()),
                ..Default::default()
            });
            if out.len() >= MAX_ROWS {
                return out;
            }
        }
    }
    out
}

/// A 1.x query id (`qid`, a positive integer).
pub fn v1_qid(id: &str) -> Option<u64> {
    id.trim().parse().ok().filter(|n| *n > 0)
}

/// `system.queries` rows ([`V3_QUERIES`]) → processes. `now_ms` dates
/// `issue_time` (UTC, no zone), for the elapsed time.
pub fn v3_rows(cols: &[String], rows: &[Vec<J>], now_ms: i64) -> Vec<ServerProcess> {
    let col = |n: &str| cols.iter().position(|c| c == n);
    rows.iter()
        .filter_map(|r| {
            let get = |n: &str| text(col(n).and_then(|i| r.get(i)));
            let id = get("id")?;
            let query = get("query_text").unwrap_or_default();
            let issued = get("issue_time").and_then(|t| {
                // `cell` already turned the `T` into a space.
                chrono::NaiveDateTime::parse_from_str(&t.trim_end_matches('Z').replacen('T', " ", 1), "%Y-%m-%d %H:%M:%S%.f")
                    .ok()
                    .map(|d| d.and_utc().timestamp_millis())
            });
            Some(ServerProcess {
                id,
                status: get("phase"),
                active: true,
                own: query.trim() == V3_QUERIES,
                program: get("query_type"),
                command: command(&query),
                elapsed_ms: issued.map(|t| (now_ms - t).max(0) as u64),
                sql: Some(clip(&query)).filter(|q| !q.is_empty()),
                ..Default::default()
            })
        })
        .take(MAX_ROWS)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn show_queries() {
        let r = json!({"statement_id": 0, "series": [{"columns": ["qid", "query", "database", "duration", "status"], "values": [
            [7, "SHOW QUERIES", "", "52µs", "running"],
            [5, "SELECT count(*) FROM cpu WHERE time > now() - 30d", "telegraf", "2.5s", "running"],
            [6, "SELECT 1", "telegraf", "10s", "killed"],
        ]}]});
        let p = v1_rows(&r);
        assert!(p[0].own && p[0].active);
        assert_eq!(p[1].id, "5");
        assert_eq!((p[1].elapsed_ms, p[1].command.as_deref(), p[1].database.as_deref()), (Some(2500), Some("SELECT"), Some("telegraf")));
        assert!(!p[1].own && p[1].active);
        assert!(!p[2].active);
        assert_eq!(v1_qid(" 5 "), Some(5));
        assert_eq!(v1_qid("5; DROP DATABASE x"), None);
    }

    #[test]
    fn system_queries() {
        let cols: Vec<String> = ["id", "query_type", "phase", "issue_time", "query_text", "running"].map(String::from).to_vec();
        let rows = vec![
            vec![json!("a1"), json!("sql"), json!("EXECUTING"), json!("2026-10-03T22:00:00.000"), json!("SELECT * FROM cpu"), json!(true)],
            vec![json!("b2"), json!("sql"), json!("PLANNED"), json!("2026-10-03 22:00:09.500"), json!(V3_QUERIES), json!(true)],
        ];
        let now = chrono::NaiveDateTime::parse_from_str("2026-10-03T22:00:10", "%Y-%m-%dT%H:%M:%S").unwrap().and_utc().timestamp_millis();
        let p = v3_rows(&cols, &rows, now);
        assert_eq!((p[0].elapsed_ms, p[0].status.as_deref(), p[0].command.as_deref()), (Some(10000), Some("EXECUTING"), Some("SELECT")));
        assert!(!p[0].own && p[1].own);
    }
}
