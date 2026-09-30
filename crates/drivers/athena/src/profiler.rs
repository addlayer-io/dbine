//! The profiler ([`dbine_driver::profiler`]) for Athena: complete, from
//! the workgroup's query history (`ListQueryExecutions`, newest first, and
//! `BatchGetQueryExecution`), which keeps every execution for 45 days. Each
//! read goes through the executions submitted since the newest one seen,
//! back [`OVERLAP_MS`] (one can be listed a little late) and to the oldest
//! one still running; an execution is reported once, when it finishes.
//!
//! Nothing is switched on and the profiler runs no queries. The history is
//! per workgroup (the session's) and doesn't say who ran a query; an
//! execution belongs to the watched database when it ran with it as its
//! context database (or with none).

use crate::{err, AthenaSession};
use aws_sdk_athena::primitives::DateTime;
use aws_sdk_athena::types::{QueryExecution, QueryExecutionState};
use dbine_driver::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use std::collections::HashMap;
use std::time::{Duration, Instant, SystemTime, UNIX_EPOCH};

/// How far back each read goes before the newest execution seen.
const OVERLAP_MS: i64 = 30_000;
/// How often the history is read (the API allows a few calls a second).
const EVERY: Duration = Duration::from_secs(3);
/// Ids per page (the batch call's limit) and pages per read.
const PAGE: i32 = 50;
const PAGES: usize = 10;

pub(crate) struct State {
    /// The database watched (empty: all).
    database: String,
    seen: Seen,
    next: Instant,
}

/// Which executions were already reported, by id, keyed on their
/// submission time (epoch ms); `pending` are those seen running.
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

    /// Where the next read stops: back to the oldest execution still running.
    fn from(&self) -> i64 {
        self.pending.values().copied().fold(self.mark - OVERLAP_MS, i64::min)
    }

    fn running(&mut self, id: &str, at: i64) {
        if at >= self.since && !self.ids.contains_key(id) {
            self.pending.insert(id.to_string(), at);
        }
    }

    /// True the first time a finished execution is seen.
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

fn ms(t: &DateTime) -> i64 {
    t.secs() * 1000 + i64::from(t.subsec_nanos() / 1_000_000)
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

fn submitted(q: &QueryExecution) -> Option<i64> {
    q.status().and_then(|s| s.submission_date_time()).map(ms)
}

fn done(q: &QueryExecution) -> bool {
    matches!(
        q.status().and_then(|s| s.state()),
        Some(QueryExecutionState::Succeeded | QueryExecutionState::Failed | QueryExecutionState::Cancelled)
    )
}

/// Ran in `database` of `catalog` (or with no context database).
fn belongs(q: &QueryExecution, catalog: &str, database: &str) -> bool {
    let ctx = q.query_execution_context();
    let db = ctx.and_then(|c| c.database()).unwrap_or("");
    let cat = ctx.and_then(|c| c.catalog()).unwrap_or("");
    (cat.is_empty() || cat.eq_ignore_ascii_case(catalog)) && (database.is_empty() || db.is_empty() || db.eq_ignore_ascii_case(database))
}

fn statement(q: &QueryExecution) -> Option<ProfiledStatement> {
    let text = q.query().unwrap_or("");
    if text.trim().is_empty() {
        return None;
    }
    let status = q.status();
    let start = submitted(q)?;
    let st = q.statistics();
    let mut detail: Vec<String> = q.statement_type().map(|t| t.as_str().to_string()).into_iter().collect();
    if let Some(e) = st.and_then(|s| s.engine_execution_time_in_millis()) {
        detail.push(format!("motor {e} ms"));
    }
    if let Some(w) = st.and_then(|s| s.query_queue_time_in_millis()).filter(|w| *w > 0) {
        detail.push(format!("en cola {w} ms"));
    }
    if st.and_then(|s| s.result_reuse_information()).is_some_and(|r| r.reused_previous_result()) {
        detail.push("resultado reutilizado".into());
    }
    let failed = matches!(status.and_then(|s| s.state()), Some(QueryExecutionState::Failed | QueryExecutionState::Cancelled));
    let error = status
        .and_then(|s| s.athena_error().and_then(|e| e.error_message()).or(s.state_change_reason()))
        .map(str::to_string)
        .or_else(|| failed.then(|| status.and_then(|s| s.state()).map_or("", |s| s.as_str()).to_string()));
    Some(ProfiledStatement {
        time: utc(start),
        duration_ms: st
            .and_then(|s| s.total_execution_time_in_millis())
            .map(|v| v as f64)
            .or_else(|| status.and_then(|s| s.completion_date_time()).map(|c| (ms(c) - start).max(0) as f64)),
        text: text.to_string(),
        database: q.query_execution_context().and_then(|c| c.database()).map(str::to_string),
        user: None,
        client: None,
        rows: None,
        error: if failed { error } else { None },
        detail: (!detail.is_empty()).then(|| detail.join(" · ")),
        application: None,
        // Athena reports neither CPU time nor what a query wrote.
        reads: st.and_then(|s| s.data_scanned_in_bytes()).map(|b| b.max(0) as u64),
        ..Default::default()
    })
}

/// The new finished executions of a batch (in the listing's order); tracks
/// the running ones.
fn take(list: &[QueryExecution], catalog: &str, database: &str, from: i64, seen: &mut Seen) -> Vec<ProfiledStatement> {
    let mut out = Vec::new();
    let mut list: Vec<&QueryExecution> = list.iter().collect();
    list.sort_by_key(|q| submitted(q).unwrap_or(0));
    for q in list {
        let (Some(id), Some(at)) = (q.query_execution_id(), submitted(q)) else { continue };
        if at < from {
            continue;
        }
        if !done(q) {
            seen.running(id, at);
            continue;
        }
        if seen.first(id, at) && belongs(q, catalog, database) {
            out.extend(statement(q));
        }
    }
    out
}

pub(crate) async fn start(s: &AthenaSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let database = if opts.database.is_empty() { s.database.clone().unwrap_or_default() } else { opts.database.clone() };
    // Proves athena:ListQueryExecutions on the workgroup.
    s.client.list_query_executions().work_group(&s.workgroup).max_results(1).send().await.map_err(err)?;
    let started = ProfilerStarted::new(ProfilerMode::Complete, format!("ListQueryExecutions ({})", s.workgroup))
        .units(Some("bytes"), None)
        .note(format!(
        "Se ven las consultas del workgroup {} al terminar. Athena no informa quién ejecutó cada una.",
        s.workgroup
    ));
    Ok((State { database, seen: Seen::new(now_ms()), next: Instant::now() }, started))
}

pub(crate) async fn poll(s: &AthenaSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    if Instant::now() < state.next {
        return Ok(Vec::new());
    }
    state.next = Instant::now() + EVERY;
    let from = state.seen.from();
    let mut out = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..PAGES {
        let page = s
            .client
            .list_query_executions()
            .work_group(&s.workgroup)
            .max_results(PAGE)
            .set_next_token(token.take())
            .send()
            .await
            .map_err(err)?;
        let ids: Vec<String> = page.query_execution_ids().to_vec();
        // Reported ones are known: only the rest are fetched.
        let mut oldest = ids.iter().filter_map(|id| state.seen.ids.get(id).copied()).min().unwrap_or(i64::MAX);
        let unknown: Vec<String> = ids.iter().filter(|id| !state.seen.ids.contains_key(*id)).cloned().collect();
        if !unknown.is_empty() {
            let got = s.client.batch_get_query_execution().set_query_execution_ids(Some(unknown)).send().await.map_err(err)?;
            let list = got.query_executions();
            oldest = list.iter().filter_map(submitted).fold(oldest, i64::min);
            out.extend(take(list, &s.catalog, &state.database, from, &mut state.seen));
        }
        // Newest first: once past `from`, the rest is older.
        match page.next_token() {
            Some(t) if oldest >= from && !ids.is_empty() => token = Some(t.to_string()),
            _ => break,
        }
    }
    state.seen.prune();
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use aws_sdk_athena::types::{
        AthenaError, QueryExecutionContext, QueryExecutionStatistics, QueryExecutionStatus, StatementType,
    };

    fn exec(id: &str, at: i64, state: QueryExecutionState, db: &str) -> QueryExecution {
        QueryExecution::builder()
            .query_execution_id(id)
            .query(format!("SELECT '{id}'"))
            .statement_type(StatementType::Dml)
            .query_execution_context(QueryExecutionContext::builder().catalog("AwsDataCatalog").database(db).build())
            .status(
                QueryExecutionStatus::builder()
                    .state(state.clone())
                    .submission_date_time(DateTime::from_millis(at))
                    .set_athena_error((state == QueryExecutionState::Failed).then(|| AthenaError::builder().error_message("boom").build()))
                    .build(),
            )
            .statistics(QueryExecutionStatistics::builder().total_execution_time_in_millis(120).data_scanned_in_bytes(10).build())
            .build()
    }

    #[test]
    fn times_are_utc_milliseconds() {
        assert_eq!(utc(1_700_000_000_123), "2023-11-14 22:13:20.123");
        assert_eq!(utc(951_782_400_000), "2000-02-29 00:00:00.000");
        assert_eq!(ms(&DateTime::from_millis(1_700_000_000_123)), 1_700_000_000_123);
    }

    #[test]
    fn executions_are_reported_once_when_finished() {
        use QueryExecutionState::*;
        let mut seen = Seen::new(1_000_000);
        let batch = [
            exec("old", 999_000, Succeeded, "ventas"),
            exec("a", 1_001_000, Succeeded, "ventas"),
            exec("b", 1_002_000, Running, "ventas"),
            exec("c", 1_003_000, Failed, "ventas"),
            exec("d", 1_004_000, Succeeded, "otra"),
        ];
        let out = take(&batch, "AwsDataCatalog", "ventas", seen.from(), &mut seen);
        assert_eq!(out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["SELECT 'a'", "SELECT 'c'"]);
        assert_eq!((out[0].duration_ms, out[0].time.as_str()), (Some(120.0), "1970-01-01 00:16:41.000"));
        assert_eq!(out[0].detail.as_deref(), Some("DML"));
        assert_eq!((out[0].cpu_ms, out[0].reads, out[0].writes), (None, Some(10), None));
        assert_eq!(out[1].error.as_deref(), Some("boom"));
        // The running one is kept; once done, it's reported alone (the
        // others come again through the overlap).
        assert!(seen.pending.contains_key("b"));
        assert_eq!(seen.from(), 1_004_000 - OVERLAP_MS);
        let batch = [exec("a", 1_001_000, Succeeded, "ventas"), exec("b", 1_002_000, Succeeded, "ventas")];
        let out = take(&batch, "AwsDataCatalog", "ventas", seen.from(), &mut seen);
        assert_eq!(out.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), ["SELECT 'b'"]);
    }
}
