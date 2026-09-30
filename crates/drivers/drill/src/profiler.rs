//! The profiler ([`dbine_driver::profiler`]): Drill keeps a profile of
//! every query in its profile store (`/profiles.json` lists the running
//! ones and the latest finished), so it's complete while they're kept. It
//! reads only REST pages, never queries, so it has nothing of its own to
//! leave out. Profiles don't say which schema a query used: the whole
//! cluster is watched. Nothing is switched on.

use crate::{text, DrillSession};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::time::Duration;

/// Finished queries listed per poll (the newest first).
const LIST: &str = "/profiles.json?max=500";
/// Longest listed query text: longer ones are cut (then read from the profile).
const LISTED_TEXT: usize = 150;

pub(crate) struct State {
    /// `startTime` (ms) read from: the oldest query still running, or the newest seen.
    after: i64,
    /// Queries already seen at or after `after`, with their start.
    seen: HashMap<String, i64>,
}

pub(crate) async fn start(s: &DrillSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let _ = opts;
    let list = list(s).await?;
    // What's listed now ran (or started) before profiling: not reported.
    let mut state = State { after: 0, seen: HashMap::new() };
    for q in queries(&list, "finishedQueries").chain(queries(&list, "runningQueries")) {
        if let (Some(id), Some(t)) = (q.get("queryId").map(text), start_ms(q)) {
            state.after = state.after.max(t);
            state.seen.insert(id, t);
        }
    }
    let started = ProfilerStarted::new(ProfilerMode::Complete, "/profiles.json").note(
        "Drill guarda el perfil de cada consulta de todo el cluster (sin indicar el esquema) mientras no se borren \
         del almacén de perfiles; cada consulta aparece al terminar.",
    );
    Ok((state, started))
}

/// The list of profiles. Drill fails it (HTTP 500, "Failed to get completed
/// profiles") for a moment while a finished query's profile is being
/// written: tried again a few times.
async fn list(s: &DrillSession) -> Result<Value> {
    let mut tries = 0;
    loop {
        match s.conn.get(LIST).await {
            Err(Error::Query(_)) if tries < 10 => {
                tries += 1;
                tokio::time::sleep(Duration::from_millis(100)).await;
            }
            r => return r,
        }
    }
}

fn queries<'a>(list: &'a Value, key: &str) -> impl Iterator<Item = &'a Value> {
    list.get(key).and_then(Value::as_array).into_iter().flatten()
}

fn start_ms(q: &Value) -> Option<i64> {
    q.get("startTime").and_then(|v| v.as_i64().or_else(|| text(v).parse().ok()))
}

pub(crate) async fn poll(s: &DrillSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let list = list(s).await?;
    let oldest_running = queries(&list, "runningQueries").filter_map(start_ms).min();
    let mut out = Vec::new();
    let mut newest = state.after;
    for q in queries(&list, "finishedQueries") {
        let (Some(id), Some(start)) = (q.get("queryId").map(text), start_ms(q)) else { continue };
        newest = newest.max(start);
        if start < state.after || state.seen.contains_key(&id) {
            continue;
        }
        state.seen.insert(id.clone(), start);
        let mut st = statement(q, start);
        let failed = !q.get("state").map(text).unwrap_or_default().eq_ignore_ascii_case("succeeded");
        // The list cuts long statements and has no error message: both are in the profile.
        if failed || st.text.chars().count() >= LISTED_TEXT {
            if let Ok(p) = s.conn.get(&format!("/profiles/{id}.json")).await {
                if let Some(t) = p.get("query").map(text).filter(|t| !t.is_empty()) {
                    st.text = t;
                }
                if failed {
                    st.error = p.get("error").map(text).map(|e| e.trim().to_string()).filter(|e| !e.is_empty()).or(st.error);
                }
            }
        }
        out.push(st);
    }
    // Never back: what started before `after` was seen, or ran before profiling.
    state.after = state.after.max(oldest_running.map_or(newest, |r| r.min(newest)));
    let after = state.after;
    state.seen.retain(|_, t| *t >= after);
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

/// A finished query of the list.
fn statement(q: &Value, start: i64) -> ProfiledStatement {
    let end = q.get("endTime").and_then(|v| v.as_i64().or_else(|| text(v).parse().ok()));
    let state = q.get("state").map(text).unwrap_or_default();
    let error = match state.as_str() {
        "Succeeded" | "" => None,
        s => Some(s.to_string()),
    };
    let mut detail = Vec::new();
    if let Some(f) = q.get("foreman").map(text).filter(|f| !f.is_empty()) {
        detail.push(format!("foreman {f}"));
    }
    if let Some(c) = q.get("totalCost").and_then(Value::as_f64) {
        detail.push(format!("costo {c}"));
    }
    if let Some(queue) = q.get("queueName").map(text).filter(|q| !q.is_empty() && q != "-" && q != "Unknown") {
        detail.push(format!("cola {queue}"));
    }
    ProfiledStatement {
        time: utc_ms(start),
        duration_ms: end.map(|e| (e - start).max(0) as f64),
        text: q.get("query").map(text).unwrap_or_default(),
        user: q.get("user").map(text).filter(|u| !u.is_empty()),
        error,
        detail: (!detail.is_empty()).then(|| detail.join("; ")),
        ..Default::default()
    }
}

fn utc_ms(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map(|t| t.format("%Y-%m-%d %H:%M:%S%.3f").to_string()).unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn listed_queries() {
        let q = json!({"queryId": "a", "startTime": 1790543869516i64, "endTime": 1790543869636i64, "query": "SELECT 1",
                       "state": "Failed", "user": "anonymous", "foreman": "node1", "totalCost": 5.0, "queueName": "-"});
        let s = statement(&q, 1790543869516);
        assert_eq!(s.time, "2026-09-27 21:17:49.516");
        assert_eq!(s.duration_ms, Some(120.0));
        assert_eq!(s.error.as_deref(), Some("Failed"));
        assert_eq!(s.detail.as_deref(), Some("foreman node1; costo 5"));
    }
}
