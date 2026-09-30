//! The profiler ([`dbine_driver::profiler`]) for Databricks SQL: complete,
//! from the Query History API (`/api/2.0/sql/history/queries`), which lists
//! every statement run on the workspace's SQL warehouses and serverless
//! compute. Each read asks for the queries started since the newest one
//! seen, back [`OVERLAP_MS`] (the history can show one a little late) and to
//! the oldest one still running; a query is reported once, when it
//! finishes.
//!
//! Nothing is switched on and the profiler runs no SQL (so it doesn't keep
//! a warehouse awake). The history doesn't say which catalog a query used:
//! every warehouse's queries are shown. Without admin rights (or CAN_MANAGE
//! on a warehouse), only the login's own queries are listed.

use crate::DatabricksSession;
use dbine_driver::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::Value as Json;
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How far back each read goes before the newest query seen.
const OVERLAP_MS: i64 = 30_000;
/// How often the history is read.
const EVERY: Duration = Duration::from_secs(3);
/// Queries per page (the API's most) and pages per read.
const PAGE: usize = 1000;
const PAGES: usize = 5;

pub(crate) struct State {
    seen: Seen,
    next: Instant,
}

/// Which queries were already reported, by id, keyed on their start time
/// (epoch ms); `pending` are those seen running or queued.
#[derive(Debug)]
struct Seen {
    since: i64,
    mark: i64,
    ids: HashMap<String, i64>,
    pending: HashMap<String, i64>,
}

impl Seen {
    fn new(now: i64) -> Self {
        Self { since: now, mark: now, ids: HashMap::new(), pending: HashMap::new() }
    }

    /// Where the next read starts: back to the oldest query still running.
    fn from(&self) -> i64 {
        self.pending.values().copied().fold(self.mark - OVERLAP_MS, i64::min)
    }

    fn running(&mut self, id: &str, at: i64) {
        if at >= self.since && !self.ids.contains_key(id) {
            self.pending.insert(id.to_string(), at);
        }
    }

    /// True the first time a finished query is seen.
    fn first(&mut self, id: &str, at: i64) -> bool {
        self.pending.remove(id);
        if at < self.since || self.ids.contains_key(id) {
            return false;
        }
        self.ids.insert(id.to_string(), at);
        self.mark = self.mark.max(at);
        true
    }

    fn prune(&mut self) {
        let from = self.from();
        self.ids.retain(|_, at| *at >= from);
    }
}

fn now_ms() -> i64 {
    SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as i64).unwrap_or(0)
}

/// Epoch milliseconds as `YYYY-MM-DD HH:MM:SS.mmm` (UTC).
fn utc(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Civil date from days since 1970-01-01 (H. Hinnant's algorithm).
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}", rem / 3_600_000, rem / 60_000 % 60, rem / 1000 % 60, rem % 1000)
}

fn text<'a>(v: &'a Json, key: &str) -> Option<&'a str> {
    v.get(key).and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty())
}

fn num(v: &Json, key: &str) -> Option<f64> {
    match v.get(key)? {
        Json::String(s) => s.parse().ok(),
        x => x.as_f64(),
    }
}

/// A token in a query string (it may hold `+`, `/` and `=`).
fn escape(s: &str) -> String {
    s.bytes()
        .map(|b| match b {
            b'A'..=b'Z' | b'a'..=b'z' | b'0'..=b'9' | b'-' | b'_' | b'.' | b'~' => (b as char).to_string(),
            _ => format!("%{b:02X}"),
        })
        .collect()
}

fn statement(q: &Json, start: i64) -> Option<ProfiledStatement> {
    let sql = text(q, "query_text")?;
    let status = text(q, "status").unwrap_or("");
    let user = text(q, "user_name");
    let mut detail: Vec<String> = text(q, "statement_type").map(str::to_string).into_iter().collect();
    if let Some(w) = text(q, "warehouse_id") {
        detail.push(format!("warehouse {w}"));
    }
    if let Some(as_user) = text(q, "executed_as_user_name").filter(|a| Some(*a) != user) {
        detail.push(format!("como {as_user}"));
    }
    let end = num(q, "query_end_time_ms");
    let metrics = q.get("metrics").unwrap_or(&Json::Null);
    Some(ProfiledStatement {
        time: utc(start),
        duration_ms: num(q, "duration").or_else(|| end.map(|e| (e - start as f64).max(0.0))),
        text: sql.to_string(),
        database: None,
        user: user.map(str::to_string),
        client: None,
        application: text(q, "client_application").map(str::to_string),
        rows: num(q, "rows_produced").map(|r| r as u64),
        error: match status {
            "FAILED" => Some(text(q, "error_message").unwrap_or("falló").to_string()),
            "CANCELED" => Some(text(q, "error_message").unwrap_or("cancelada").to_string()),
            _ => None,
        },
        detail: (!detail.is_empty()).then(|| detail.join(" · ")),
        // The summed time of the query's tasks: Databricks' closest figure
        // to CPU time (it reports no CPU time as such).
        cpu_ms: num(metrics, "task_total_time_ms"),
        reads: num(metrics, "read_bytes").map(|b| b as u64),
        writes: num(metrics, "write_remote_bytes").map(|b| b as u64),
        ..Default::default()
    })
}

/// The new finished queries of a read; tracks the running ones.
fn take(res: &[Json], from: i64, seen: &mut Seen) -> Vec<ProfiledStatement> {
    let mut res: Vec<&Json> = res.iter().collect();
    res.sort_by_key(|q| num(q, "query_start_time_ms").unwrap_or(0.0) as i64);
    let mut out = Vec::new();
    for q in res {
        let (Some(id), Some(at)) = (text(q, "query_id"), num(q, "query_start_time_ms").map(|t| t as i64)) else { continue };
        if at < from {
            continue;
        }
        if !matches!(text(q, "status"), Some("FINISHED" | "FAILED" | "CANCELED")) {
            seen.running(id, at);
            continue;
        }
        if seen.first(id, at) {
            out.extend(statement(q, at));
        }
    }
    out
}

impl DatabricksSession {
    /// The queries started from `from` (epoch ms) on.
    async fn started_since(&self, from: i64, max: usize, pages: usize) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        // The metrics (task time, bytes read and written) come in the same
        // response when asked for.
        let mut path = format!(
            "/api/2.0/sql/history/queries?max_results={max}&include_metrics=true&filter_by.query_start_time_range.start_time_ms={from}"
        );
        for _ in 0..pages {
            let r = self.api.get(&path).await?;
            out.extend(r.get("res").and_then(Json::as_array).into_iter().flatten().cloned());
            match text(&r, "next_page_token") {
                // The token carries the filter.
                Some(t) if r.get("has_next_page").and_then(Json::as_bool) != Some(false) => {
                    path = format!("/api/2.0/sql/history/queries?max_results={max}&include_metrics=true&page_token={}", escape(t));
                }
                _ => break,
            }
        }
        Ok(out)
    }
}

pub(crate) async fn start(s: &DatabricksSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let _ = opts;
    let now = now_ms();
    s.started_since(now, 1, 1).await?;
    let started = ProfilerStarted::new(ProfilerMode::Complete, "Query History API").units(Some("bytes"), Some("bytes")).note(
        "Databricks no informa el catálogo de cada consulta: se ven las de todos los warehouses del workspace. \
         Sin permisos de administrador (o CAN_MANAGE sobre el warehouse) solo se ven tus consultas.",
    );
    Ok((State { seen: Seen::new(now), next: Instant::now() }, started))
}

pub(crate) async fn poll(s: &DatabricksSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    if Instant::now() < state.next {
        return Ok(Vec::new());
    }
    state.next = Instant::now() + EVERY;
    let from = state.seen.from();
    let res = s.started_since(from, PAGE, PAGES).await?;
    let mut out = take(&res, from, &mut state.seen);
    state.seen.prune();
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn history_entries_are_reported_once_when_finished() {
        let mut seen = Seen::new(1_700_000_000_000);
        let res = vec![
            json!({"query_id": "old", "status": "FINISHED", "query_start_time_ms": 1_699_999_999_000i64, "query_text": "SELECT 0"}),
            json!({"query_id": "a", "status": "FINISHED", "query_start_time_ms": 1_700_000_000_123i64, "query_text": "SELECT 1",
                   "duration": 250, "rows_produced": 1, "user_name": "ana@x.com", "executed_as_user_name": "ana@x.com",
                   "statement_type": "SELECT", "warehouse_id": "wh1", "client_application": "Tableau",
                   "metrics": {"task_total_time_ms": 480, "read_bytes": 4096, "write_remote_bytes": 0}}),
            json!({"query_id": "b", "status": "RUNNING", "query_start_time_ms": 1_700_000_001_000i64, "query_text": "SELECT 2"}),
            json!({"query_id": "c", "status": "FAILED", "query_start_time_ms": 1_700_000_002_000i64, "query_text": "SELEC",
                   "error_message": "syntax"}),
        ];
        let out = take(&res, seen.from(), &mut seen);
        assert_eq!(out.len(), 2);
        let a = &out[0];
        assert_eq!(a.time, "2023-11-14 22:13:20.123");
        assert_eq!((a.duration_ms, a.rows, a.application.as_deref()), (Some(250.0), Some(1), Some("Tableau")));
        assert_eq!(a.detail.as_deref(), Some("SELECT · warehouse wh1"));
        assert_eq!((a.cpu_ms, a.reads, a.writes), (Some(480.0), Some(4096), Some(0)));
        assert_eq!((out[1].cpu_ms, out[1].reads), (None, None));
        assert_eq!(out[1].error.as_deref(), Some("syntax"));
        assert!(seen.pending.contains_key("b"));
        let res = vec![
            json!({"query_id": "a", "status": "FINISHED", "query_start_time_ms": 1_700_000_000_123i64, "query_text": "SELECT 1"}),
            json!({"query_id": "b", "status": "FINISHED", "query_start_time_ms": 1_700_000_001_000i64, "query_text": "SELECT 2"}),
        ];
        let out = take(&res, seen.from(), &mut seen);
        assert_eq!(out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["SELECT 2"]);
        assert!(seen.pending.is_empty());
    }

    #[test]
    fn page_tokens_are_escaped() {
        assert_eq!(escape("Ci0+a/b=="), "Ci0%2Ba%2Fb%3D%3D");
        assert_eq!(utc(0), "1970-01-01 00:00:00.000");
    }
}
