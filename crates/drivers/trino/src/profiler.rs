//! The profiler ([`dbine_driver::profiler`]) for Trino, Presto and
//! Starburst: the coordinator's query list (`GET /v1/query`), complete
//! while it keeps the queries (`query.max-history`, 100 by default, for at
//! least `query.min-expire-age`, 15 min). Reading it runs no query, so
//! there is nothing of its own to leave out.
//!
//! The database is the catalog: a query counts when its session uses it or
//! its text names it (`catalog.schema.table`).

use crate::monitor::duration_secs;
use crate::TrinoSession;
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::Value;
use std::collections::HashSet;

pub(crate) struct State {
    catalog: String,
    /// Finished queries already reported (or finished before the start).
    seen: HashSet<String>,
}

fn finished(q: &Value) -> bool {
    matches!(q["state"].as_str(), Some("FINISHED" | "FAILED"))
}

impl TrinoSession {
    pub(crate) async fn profiler_begin(&self, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
        let list = self
            .get_json("/v1/query")
            .await
            .map_err(|e| Error::Query(format!("no se pudo leer la lista de consultas del coordinador (/v1/query): {e}")))?;
        let seen = list.as_array().into_iter().flatten().filter(|q| finished(q)).filter_map(id).collect();
        let started = ProfilerStarted::new(ProfilerMode::Complete, "/v1/query").units(Some("filas"), Some("bytes")).note(
            "El coordinador guarda las últimas consultas (query.max-history, 100 por defecto): con más de 100 por segundo \
             pueden perderse algunas. Solo se ven las que el control de acceso deja ver a este usuario.",
        );
        Ok((State { catalog: opts.database.clone(), seen }, started))
    }

    pub(crate) async fn profiler_next(&self, state: &mut State) -> Result<Vec<ProfiledStatement>> {
        let list = self.get_json("/v1/query").await?;
        let list = list.as_array().cloned().unwrap_or_default();
        let mut out = Vec::new();
        for q in list.iter().filter(|q| finished(q)) {
            let Some(qid) = id(q) else { continue };
            if !state.seen.insert(qid.clone()) || !in_catalog(q, &state.catalog) {
                continue;
            }
            let mut s = statement(q);
            if q["state"] == "FAILED" {
                // The list has the error's name; the message is in the query's
                // own info.
                let code = q.pointer("/errorCode/name").and_then(Value::as_str).map(str::to_string);
                let message = self
                    .get_json(&format!("/v1/query/{qid}"))
                    .await
                    .ok()
                    .and_then(|full| full.pointer("/failureInfo/message").and_then(Value::as_str).map(str::to_string));
                s.error = message.or(code).or_else(|| Some("falló".into()));
            }
            out.push(s);
        }
        // Forget what the coordinator no longer lists.
        let listed: HashSet<String> = list.iter().filter_map(id).collect();
        state.seen.retain(|q| listed.contains(q));
        out.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(out)
    }
}

fn id(q: &Value) -> Option<String> {
    q["queryId"].as_str().map(str::to_string)
}

fn in_catalog(q: &Value, catalog: &str) -> bool {
    if catalog.is_empty() {
        return true;
    }
    if let Some(c) = q.pointer("/session/catalog").and_then(Value::as_str) {
        if c.eq_ignore_ascii_case(catalog) {
            return true;
        }
    }
    let text = q["query"].as_str().unwrap_or_default().to_ascii_lowercase();
    let name = catalog.to_ascii_lowercase();
    text.contains(&format!("{name}.")) || text.contains(&format!("\"{name}\"."))
}

fn statement(q: &Value) -> ProfiledStatement {
    let s = &q["session"];
    let st = &q["queryStats"];
    let text_of = |v: &Value| v.as_str().filter(|t| !t.is_empty()).map(str::to_string);
    let db = match (text_of(&s["catalog"]), text_of(&s["schema"])) {
        (Some(c), Some(sc)) => Some(format!("{c}.{sc}")),
        (c, _) => c,
    };
    let mut detail = Vec::new();
    if let Some(mem) = text_of(&st["peakUserMemoryReservation"]) {
        detail.push(format!("memoria pico: {mem}"));
    }
    ProfiledStatement {
        time: text_of(&st["createTime"]).map(|t| iso_utc(&t)).unwrap_or_default(),
        duration_ms: text_of(&st["elapsedTime"]).and_then(|d| duration_secs(&d)).map(|s| (s * 1e6).round() / 1000.0),
        text: q["query"].as_str().unwrap_or_default().trim().to_string(),
        database: db,
        user: text_of(&s["user"]),
        client: text_of(&s["remoteUserAddress"]),
        rows: st["outputPositions"].as_u64(),
        error: None,
        detail: (!detail.is_empty()).then(|| detail.join(", ")),
        application: text_of(&s["source"]).or_else(|| text_of(&s["userAgent"])),
        cpu_ms: text_of(&st["totalCpuTime"]).and_then(|d| duration_secs(&d)).map(|s| (s * 1e6).round() / 1000.0),
        // Rows: the list has no input size in bytes the in-memory
        // connectors fill (physicalInputDataSize stays 0 for them).
        reads: st["processedInputPositions"].as_u64().or_else(|| st["rawInputPositions"].as_u64()),
        // Presto's list doesn't have it.
        writes: text_of(&st["physicalWrittenDataSize"]).and_then(|d| data_size_bytes(&d)),
        ..Default::default()
    }
}

/// An Airlift data size (`0B`, `2894B`, `1.5kB`, `12MB`) in bytes.
fn data_size_bytes(s: &str) -> Option<u64> {
    let s = s.trim();
    let split = s.find(|c: char| c.is_ascii_alphabetic())?;
    let (n, unit) = s.split_at(split);
    let n: f64 = n.trim().parse().ok()?;
    let exp = ["B", "KB", "MB", "GB", "TB", "PB"].iter().position(|u| u.eq_ignore_ascii_case(unit))?;
    Some((n * 1024f64.powi(exp as i32)).round() as u64)
}

/// `2024-01-31T10:00:00.123456789Z` → `2024-01-31 10:00:00.123`.
fn iso_utc(t: &str) -> String {
    let t = t.trim_end_matches('Z').replace('T', " ");
    match t.find('.') {
        Some(i) => format!("{}.{:0<3}", &t[..i], &t[i + 1..].chars().take(3).collect::<String>()),
        None => format!("{t}.000"),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn times_are_utc_milliseconds() {
        assert_eq!(iso_utc("2026-09-27T21:16:59.499235395Z"), "2026-09-27 21:16:59.499");
        assert_eq!(iso_utc("2026-09-27T21:17:48.1Z"), "2026-09-27 21:17:48.100");
        assert_eq!(iso_utc("2026-09-27T21:17:48Z"), "2026-09-27 21:17:48.000");
    }

    #[test]
    fn query_stats() {
        let q = json!({"queryId": "q1", "state": "FINISHED", "query": "SELECT 1", "session": {"user": "ana"},
            "queryStats": {"createTime": "2026-09-30T11:24:09.401215051Z", "elapsedTime": "1.70s", "totalCpuTime": "1.02s",
                "processedInputPositions": 60175, "physicalWrittenDataSize": "1.5kB", "peakUserMemoryReservation": "384B"}});
        let s = statement(&q);
        assert_eq!((s.cpu_ms, s.reads, s.writes), (Some(1020.0), Some(60175), Some(1536)));
        assert_eq!(s.detail.as_deref(), Some("memoria pico: 384B"));
        assert_eq!(data_size_bytes("0B"), Some(0));
        assert_eq!(data_size_bytes("2MB"), Some(2 * 1024 * 1024));
        assert_eq!(data_size_bytes("x"), None);
    }

    #[test]
    fn catalog_scope() {
        let q = json!({"session": {"catalog": "Hive"}, "query": "SELECT 1"});
        assert!(in_catalog(&q, "hive"));
        assert!(in_catalog(&q, ""));
        assert!(!in_catalog(&q, "memory"));
        let q = json!({"session": {}, "query": "SELECT * FROM memory.default.t"});
        assert!(in_catalog(&q, "memory"));
    }
}
