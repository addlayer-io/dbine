//! The profiler ([`dbine_driver::profiler`]) for BigQuery: complete, from
//! the project's job history (`jobs.list`, free and not billed). Every
//! statement is a job, listed with its text, user, state and statistics.
//! Each read asks for the jobs created since the newest one seen, going back
//! [`OVERLAP_MS`] (a job can show up a little late) and to the oldest job
//! still running; a job is reported once, when it's done.
//!
//! Other users' jobs need `bigquery.jobs.listAll` (without it, only the
//! login's own). Nothing is switched on, and the profiler runs no jobs of
//! its own. A job belongs to the watched dataset when its default dataset,
//! the tables it reads or the table it writes are there; a job that names
//! no dataset at all (`SELECT 1`) is shown too.

use crate::{BigQuerySession, Json};
use dbine_driver::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use std::collections::{BTreeSet, HashMap};
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How far back each read goes before the newest job seen.
const OVERLAP_MS: i64 = 30_000;
/// How often the job list is read.
const EVERY: Duration = Duration::from_secs(2);
/// Jobs per page, and pages per read.
const PAGE: usize = 1000;
const PAGES: usize = 5;
/// Jobs read one by one per poll when the list leaves out their
/// configuration (the emulator does).
const GETS: usize = 50;

pub(crate) struct State {
    /// The dataset watched (empty: all).
    dataset: String,
    /// Every user's jobs (else only the login's).
    all: bool,
    seen: Seen,
    next: Instant,
}

/// Which jobs were already reported, by id, keyed on their creation time
/// (epoch ms); `pending` are those seen running.
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

    /// Where the next read starts: back to the oldest job still running.
    fn from(&self) -> i64 {
        self.pending.values().copied().fold(self.mark - OVERLAP_MS, i64::min)
    }

    /// A job not finished yet: read again until it is.
    fn running(&mut self, id: &str, at: i64) {
        if at >= self.since && !self.ids.contains_key(id) {
            self.pending.insert(id.to_string(), at);
        }
    }

    /// True the first time a finished job is seen.
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

fn at<'a>(v: &'a Json, ptr: &str) -> &'a str {
    v.pointer(ptr).and_then(Json::as_str).unwrap_or("")
}

fn n(v: &Json, ptr: &str) -> Option<f64> {
    match v.pointer(ptr)? {
        Json::String(s) => s.parse().ok(),
        x => x.as_f64(),
    }
}

/// Job times are epoch milliseconds; the emulator gives seconds.
fn ms(v: &Json, ptr: &str) -> Option<i64> {
    n(v, ptr).map(|t| if t < 1e11 { t * 1000.0 } else { t } as i64)
}

fn utc(ms: i64) -> String {
    chrono::DateTime::from_timestamp_millis(ms).map_or_else(String::new, |t| t.format("%Y-%m-%d %H:%M:%S%.3f").to_string())
}

/// The datasets a job names (anonymous result datasets, `_…`, left out).
fn datasets(v: &Json) -> BTreeSet<String> {
    let mut out = BTreeSet::new();
    for p in [
        "/configuration/query/defaultDataset/datasetId",
        "/configuration/query/destinationTable/datasetId",
        "/statistics/query/ddlTargetTable/datasetId",
        "/statistics/query/ddlTargetDataset/datasetId",
    ] {
        out.insert(at(v, p).to_string());
    }
    for t in v.pointer("/statistics/query/referencedTables").and_then(Json::as_array).into_iter().flatten() {
        out.insert(at(t, "/datasetId").to_string());
    }
    out.retain(|d| !d.is_empty() && !d.starts_with('_'));
    out
}

fn belongs(v: &Json, dataset: &str) -> bool {
    let ds = datasets(v);
    dataset.is_empty() || ds.is_empty() || ds.iter().any(|d| d.eq_ignore_ascii_case(dataset))
}

/// A finished query job as a statement (None for other kinds of jobs).
fn statement(v: &Json) -> Option<ProfiledStatement> {
    let text = at(v, "/configuration/query/query");
    if text.trim().is_empty() {
        return None;
    }
    let created = ms(v, "/statistics/creationTime")?;
    let started = ms(v, "/statistics/startTime");
    let ended = ms(v, "/statistics/endTime");
    let mut detail: Vec<String> =
        Some(at(v, "/statistics/query/statementType")).filter(|t| !t.is_empty()).map(str::to_string).into_iter().collect();
    if let Some(b) = n(v, "/statistics/query/totalBytesBilled") {
        detail.push(format!("{b} bytes facturados"));
    }
    if v.pointer("/statistics/query/cacheHit").and_then(Json::as_bool) == Some(true) {
        detail.push("desde caché".into());
    }
    let ds = datasets(v);
    let error = at(v, "/status/errorResult/message");
    Some(ProfiledStatement {
        time: utc(started.unwrap_or(created)),
        duration_ms: ended.map(|e| (e - started.unwrap_or(created)).max(0) as f64),
        text: text.to_string(),
        database: (!ds.is_empty()).then(|| ds.into_iter().collect::<Vec<_>>().join(", ")),
        user: Some(at(v, "/user_email").to_string()).filter(|u| !u.is_empty()),
        client: None,
        rows: n(v, "/statistics/query/numDmlAffectedRows").map(|r| r as u64),
        error: (!error.is_empty()).then(|| error.to_string()),
        detail: (!detail.is_empty()).then(|| detail.join(" · ")),
        application: None,
        // Slot time: BigQuery's closest figure to CPU time (it can exceed
        // the duration, since a job runs on many slots at once).
        cpu_ms: n(v, "/statistics/query/totalSlotMs").or(n(v, "/statistics/totalSlotMs")),
        reads: n(v, "/statistics/query/totalBytesProcessed").or(n(v, "/statistics/totalBytesProcessed")).map(|b| b as u64),
        // Nothing reports what a job wrote (DML counts rows, in `rows`).
        writes: None,
        ..Default::default()
    })
}

impl BigQuerySession {
    /// Top-level jobs created from `from` (epoch ms) on, newest first.
    async fn profiler_jobs(&self, all: bool, from: i64, pages: usize) -> Result<Vec<Json>> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        for _ in 0..pages {
            let mut q = vec![
                ("projection", "full".to_string()),
                ("maxResults", PAGE.to_string()),
                ("minCreationTime", from.max(0).to_string()),
            ];
            if all {
                q.push(("allUsers", "true".into()));
            }
            if let Some(t) = token.take() {
                q.push(("pageToken", t));
            }
            let resp = self.api.get(&["jobs"], &q).await?;
            out.extend(resp.get("jobs").and_then(Json::as_array).into_iter().flatten().cloned());
            match resp.get("nextPageToken").and_then(Json::as_str) {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }
}

pub(crate) async fn start(s: &BigQuerySession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let dataset = if opts.database.is_empty() { s.dataset.clone().unwrap_or_default() } else { opts.database.clone() };
    let now = now_ms();
    let mut started = ProfilerStarted::new(ProfilerMode::Complete, "jobs.list").units(Some("bytes"), None);
    let all = match s.profiler_jobs(true, now, 1).await {
        Ok(_) => true,
        Err(e) => {
            // Without bigquery.jobs.listAll: the login's own jobs.
            s.profiler_jobs(false, now, 1).await?;
            started = started.note(format!(
                "Solo se ven los jobs de este usuario: ver los de todos requiere el permiso bigquery.jobs.listAll ({e})."
            ));
            false
        }
    };
    if all {
        started = started.note("Cada consulta aparece al terminar su job; las que no nombran ningún dataset también se muestran.");
    }
    Ok((State { dataset, all, seen: Seen::new(now), next: Instant::now() }, started))
}

pub(crate) async fn poll(s: &BigQuerySession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    if Instant::now() < state.next {
        return Ok(Vec::new());
    }
    state.next = Instant::now() + EVERY;
    let from = state.seen.from();
    let mut jobs = s.profiler_jobs(state.all, from, PAGES).await?;
    // Oldest first, so the mark only moves forward.
    jobs.sort_by_key(|j| ms(j, "/statistics/creationTime").unwrap_or(0));
    let mut out = Vec::new();
    let mut gets = 0;
    for mut job in jobs {
        let id = at(&job, "/jobReference/jobId").to_string();
        let Some(created) = ms(&job, "/statistics/creationTime") else { continue };
        // Also filtered here: the emulator ignores minCreationTime.
        if id.is_empty() || created < from || created < state.seen.since {
            continue;
        }
        if !at(&job, "/status/state").eq_ignore_ascii_case("DONE") {
            state.seen.running(&id, created);
            continue;
        }
        if state.seen.ids.contains_key(&id) {
            continue;
        }
        if job.get("configuration").is_none() && gets < GETS {
            gets += 1;
            let loc: Vec<(&str, String)> =
                Some(at(&job, "/jobReference/location")).filter(|l| !l.is_empty()).map(|l| ("location", l.to_string())).into_iter().collect();
            if let Ok(full) = s.api.get(&["jobs", &id], &loc).await {
                job = full;
            }
        }
        if !state.seen.first(&id, created) || !belongs(&job, &state.dataset) {
            continue;
        }
        out.extend(statement(&job));
    }
    state.seen.prune();
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn job(id: &str, created: i64, state: &str, dataset: &str) -> Json {
        json!({
            "jobReference": {"jobId": id, "location": "US"},
            "user_email": "ana@example.com",
            "status": {"state": state},
            "configuration": {"jobType": "QUERY", "query": {"query": format!("SELECT '{id}'"),
                "defaultDataset": {"projectId": "p", "datasetId": dataset},
                "destinationTable": {"projectId": "p", "datasetId": "_abc123", "tableId": "anon"}}},
            "statistics": {"creationTime": created.to_string(), "startTime": (created + 5).to_string(),
                "endTime": (created + 105).to_string(),
                "query": {"statementType": "SELECT", "totalBytesProcessed": "2048", "totalSlotMs": "37", "cacheHit": false,
                    "referencedTables": [{"projectId": "p", "datasetId": "ds2", "tableId": "t"}]}}
        })
    }

    #[test]
    fn jobs_become_statements() {
        let st = statement(&job("a", 1_700_000_000_000, "DONE", "ds1")).unwrap();
        assert_eq!(st.time, "2023-11-14 22:13:20.005");
        assert_eq!(st.duration_ms, Some(100.0));
        assert_eq!(st.database.as_deref(), Some("ds1, ds2"));
        assert_eq!(st.user.as_deref(), Some("ana@example.com"));
        assert_eq!(st.detail.as_deref(), Some("SELECT"));
        assert_eq!((st.cpu_ms, st.reads, st.writes), (Some(37.0), Some(2048), None));
        // Seconds (the emulator) read as milliseconds.
        assert_eq!(ms(&json!({"t": "1700000000"}), "/t"), Some(1_700_000_000_000));
        // Not a query: nothing.
        assert!(statement(&json!({"configuration": {"jobType": "LOAD"}, "statistics": {"creationTime": "1"}})).is_none());
    }

    #[test]
    fn jobs_are_scoped_to_the_dataset() {
        let j = job("a", 1, "DONE", "ds1");
        assert!(belongs(&j, "ds1") && belongs(&j, "DS2") && belongs(&j, ""));
        assert!(!belongs(&j, "other"));
        // A job naming no dataset belongs anywhere.
        assert!(belongs(&json!({"configuration": {"query": {"query": "SELECT 1"}}}), "other"));
    }

    #[test]
    fn running_jobs_hold_the_window_until_done() {
        let mut seen = Seen::new(1_000_000);
        seen.running("slow", 1_000_500);
        assert!(seen.first("fast", 1_100_000));
        seen.prune();
        // The slow job keeps the next read going back to it.
        assert_eq!(seen.from(), 1_000_500);
        assert!(seen.first("slow", 1_000_500));
        assert!(!seen.first("slow", 1_000_500));
        assert!(!seen.first("fast", 1_100_000));
        assert_eq!(seen.from(), 1_100_000 - OVERLAP_MS);
        // Created before profiling began: not reported.
        assert!(!seen.first("old", 999_999));
    }
}
