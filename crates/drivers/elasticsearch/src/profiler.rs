//! The profiler ([`dbine_driver::profiler`]) for Elasticsearch, OpenSearch
//! and Open Distro: sampled from the task manager
//! (`GET _tasks?detailed=true&actions=indices:data/read/*`), which lists the
//! reads running now (search, msearch, scroll, count, get, SQL…). The slow
//! log goes to the server's files, not to an API, and OpenSearch's Query
//! Insights only keeps the top N queries: neither sees every search.
//!
//! A cluster has no databases, so the whole cluster is watched. Nothing is
//! switched on; only the `monitor` cluster privilege is needed.
//!
//! CPU: OpenSearch (2.x, task resource tracking) reports each task's CPU
//! time in `resource_stats`; a request's is the sum of its per-shard
//! children's (its own stays at 0). Elasticsearch and Open Distro don't
//! report it. No engine reports documents read or written per task.

use crate::json::J;
use crate::EsSession;
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// Longest a look at the task list may take.
const LIMIT: Duration = Duration::from_secs(5);
const TASKS: &str = "/_tasks?detailed=true&actions=indices:data/read/*";

pub(crate) struct State {
    sampler: Sampler,
    /// Most CPU (ms) seen per running request: its children's figures go
    /// away as they finish, so a later look may add up to less.
    cpu: HashMap<String, f64>,
}

pub(crate) async fn start(s: &EsSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let _ = opts;
    // Fails here (without the `monitor` privilege) rather than at the first poll.
    s.get_json(TASKS).await?;
    let since = SystemTime::now().duration_since(UNIX_EPOCH).map_or(0, |d| d.as_millis() as i64);
    Ok((
        State { sampler: Sampler::new(utc_ms(since)), cpu: HashMap::new() },
        ProfilerStarted::new(ProfilerMode::Sampled, "_tasks").note(
            "El clúster solo muestra las búsquedas en curso: las que duran menos de una décima de segundo pueden no \
             verse. El registro de búsquedas lentas va a archivos del servidor y no se puede leer por la API.",
        ),
    ))
}

pub(crate) async fn poll(s: &EsSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let mut out = Vec::new();
    let until = Instant::now() + SAMPLE_FOR;
    loop {
        let body = s.call(s.request("GET", TASKS).timeout(LIMIT)).await?;
        let mut tasks = J::parse(&body).map(|t| samples(&t, &s.opaque_id)).unwrap_or_default();
        state.cpu.retain(|id, _| tasks.iter().any(|t| &t.session == id));
        for t in &mut tasks {
            if let Some(ms) = t.cpu_ms {
                let most = state.cpu.entry(t.session.clone()).or_insert(ms);
                *most = most.max(ms);
                t.cpu_ms = Some(*most);
            }
        }
        out.extend(state.sampler.feed(tasks));
        if Instant::now() + SAMPLE_EVERY > until {
            break;
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

/// The running requests (not their per-shard children), except ours.
fn samples(tasks: &J, own: &str) -> Vec<Sample> {
    let nodes = || tasks.get("nodes").and_then(J::as_obj).into_iter().flatten().flat_map(|(_, n)| n.get("tasks").and_then(J::as_obj).into_iter().flatten());
    // Each task's parent, and its CPU time (ns) where the engine reports it.
    let parent: HashMap<&str, &str> =
        nodes().filter_map(|(id, t)| Some((id.as_str(), t.get("parent_task_id").and_then(J::as_str)?))).collect();
    let mut cpu: HashMap<&str, u64> = HashMap::new();
    for (id, t) in nodes() {
        let Some(ns) = t.at(&["resource_stats", "total", "cpu_time_in_nanos"]).and_then(J::as_u64) else { continue };
        let mut root = id.as_str();
        while let Some(p) = parent.get(root) {
            root = p;
        }
        *cpu.entry(root).or_default() += ns;
    }
    let mut out = Vec::new();
    for (_, node) in tasks.get("nodes").and_then(J::as_obj).into_iter().flatten() {
        let host = node.get("host").or_else(|| node.get("name")).and_then(J::as_str);
        for (id, t) in node.get("tasks").and_then(J::as_obj).into_iter().flatten() {
            let opaque = t.at(&["headers", "X-Opaque-Id"]).and_then(J::as_str);
            if t.get("parent_task_id").is_some() || opaque == Some(own) {
                continue;
            }
            let action = t.get("action").and_then(J::as_str).unwrap_or_default();
            let (text, indices) = request_text(action, t.get("description").and_then(J::as_str).unwrap_or_default());
            let started = t.get("start_time_in_millis").and_then(J::as_u64).unwrap_or(0) as i64;
            out.push(Sample {
                session: id.clone(),
                started: utc_ms(started),
                text,
                running: true,
                duration_ms: t.get("running_time_in_nanos").and_then(J::as_u64).map(|n| n as f64 / 1e6),
                database: indices,
                client: opaque.map(str::to_string),
                cpu_ms: cpu.get(id.as_str()).map(|ns| *ns as f64 / 1e6),
                detail: Some(match host {
                    Some(h) => format!("{action} (nodo {h})"),
                    None => action.to_string(),
                }),
                ..Default::default()
            });
        }
    }
    out
}

/// A search task's description (`indices[a,b], search_type[…], source[{…}]`)
/// as the console request that runs it, and its indices. Other reads keep
/// their description.
fn request_text(action: &str, description: &str) -> (String, Option<String>) {
    let indices = description
        .strip_prefix("indices[")
        .and_then(|r| r.split_once(']'))
        .map(|(i, _)| i.to_string())
        .filter(|i| !i.is_empty());
    let source = description.find("source[").map(|i| &description[i + 7..]).and_then(|s| s.strip_suffix(']'));
    match (action, &indices, source) {
        ("indices:data/read/search", Some(i), Some(src)) => (format!("GET /{i}/_search\n{src}"), indices),
        _ if description.is_empty() => (action.to_string(), indices),
        _ => (description.to_string(), indices),
    }
}

/// `YYYY-MM-DD HH:MM:SS.mmm` (UTC) of a Unix time in milliseconds (Howard
/// Hinnant's civil_from_days).
pub(crate) fn utc_ms(ms: i64) -> String {
    let (secs, milli) = (ms.div_euclid(1000), ms.rem_euclid(1000));
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{milli:03}", sod / 3600, sod % 3600 / 60, sod % 60)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_are_utc() {
        assert_eq!(utc_ms(0), "1970-01-01 00:00:00.000");
        assert_eq!(utc_ms(1_790_542_938_660), "2026-09-27 21:02:18.660");
        assert_eq!(utc_ms(951_782_400_001), "2000-02-29 00:00:00.001");
    }

    #[test]
    fn search_descriptions_become_requests() {
        let d = r#"indices[books,films], search_type[QUERY_THEN_FETCH], source[{"query":{"match_all":{}}}]"#;
        let (text, db) = request_text("indices:data/read/search", d);
        assert_eq!(text, "GET /books,films/_search\n{\"query\":{\"match_all\":{}}}");
        assert_eq!(db.as_deref(), Some("books,films"));
        assert_eq!(request_text("indices:data/read/scroll", "").0, "indices:data/read/scroll");
    }

    #[test]
    fn own_and_child_tasks_are_left_out() {
        let t = J::parse(
            r#"{"nodes":{"n1":{"host":"10.0.0.1","tasks":{
                "n1:1":{"action":"indices:data/read/search","description":"indices[a], source[{}]",
                        "start_time_in_millis":0,"running_time_in_nanos":5000000,"headers":{"X-Opaque-Id":"app"}},
                "n1:2":{"action":"indices:data/read/search[phase/query]","parent_task_id":"n1:1"},
                "n1:3":{"action":"indices:data/read/search","headers":{"X-Opaque-Id":"me"}}}}}}"#,
        )
        .unwrap();
        let s = samples(&t, "me");
        assert_eq!(s.len(), 1);
        assert_eq!((s[0].session.as_str(), s[0].duration_ms, s[0].client.as_deref()), ("n1:1", Some(5.0), Some("app")));
        assert_eq!(s[0].cpu_ms, None);
    }

    #[test]
    fn opensearch_cpu_adds_up_the_children() {
        let t = J::parse(
            r#"{"nodes":{
                "n1":{"tasks":{
                    "n1:1":{"action":"indices:data/read/search","description":"indices[a], source[{}]",
                            "resource_stats":{"total":{"cpu_time_in_nanos":0}}},
                    "n1:2":{"action":"indices:data/read/search[phase/query]","parent_task_id":"n1:1",
                            "resource_stats":{"total":{"cpu_time_in_nanos":1500000}}}}},
                "n2":{"tasks":{
                    "n2:7":{"action":"indices:data/read/search[phase/query]","parent_task_id":"n1:1",
                            "resource_stats":{"total":{"cpu_time_in_nanos":500000}}}}}}}"#,
        )
        .unwrap();
        let s = samples(&t, "me");
        assert_eq!((s.len(), s[0].cpu_ms), (1, Some(2.0)));
    }
}
