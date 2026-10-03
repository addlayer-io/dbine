//! The process list ([`dbine_driver::Session::processes`]) and stopping a
//! job ([`dbine_driver::Session::cancel_query`]).
//!
//! BigQuery runs jobs, not connections: the list is the running and pending
//! jobs of the project from `jobs.list` (free, as the monitor reads it;
//! `INFORMATION_SCHEMA.JOBS` would be a billed query every few seconds), and
//! cancelling one is `jobs.cancel`. The id is `<location>.<job id>` (as `bq`
//! writes it), because `jobs.cancel` needs the location outside the US and
//! EU multi-regions; a job id has no dots.

use crate::{BigQuerySession, Json};
use dbine_driver::{Error, Result, ServerProcess};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

/// Longest the list may take: it's polled every few seconds.
const QUERY_LIMIT: Duration = Duration::from_secs(10);
/// Characters kept of a statement's text.
const MAX_TEXT: usize = 20000;
/// Rows at most (one page of `jobs.list`).
const MAX_ROWS: usize = 1000;
/// The monitor's own jobs, left out as the monitor does.
const MONITOR_TAG: &str = "/* dbine-monitor */";

/// `[location.]job_id` → (location, job id), checked: job ids are letters,
/// digits, `_` and `-`; locations add nothing else.
pub(crate) fn parse_id(id: &str) -> Option<(Option<&str>, &str)> {
    let id = id.trim();
    let ok = |s: &str| !s.is_empty() && s.len() <= 1024 && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '-');
    match id.split_once('.') {
        Some((loc, job)) if ok(loc) && ok(job) => Some((Some(loc), job)),
        None if ok(id) => Some((None, id)),
        _ => None,
    }
}

fn text(v: &Json, ptr: &str) -> Option<String> {
    v.pointer(ptr).and_then(Json::as_str).map(str::trim).filter(|s| !s.is_empty()).map(str::to_string)
}

fn num(v: &Json, ptr: &str) -> Option<f64> {
    match v.pointer(ptr)? {
        Json::String(s) => s.trim().parse().ok(),
        v => v.as_f64(),
    }
}

/// Epoch milliseconds; the emulator reports seconds.
fn epoch_ms(v: Option<f64>) -> Option<f64> {
    v.map(|t| if t < 1e11 { t * 1000.0 } else { t })
}

/// One job of `jobs.list` (projection=full); `now` in epoch ms.
fn row(v: &Json, now: f64) -> ServerProcess {
    let job = text(v, "/jobReference/jobId").unwrap_or_default();
    let id = match text(v, "/jobReference/location") {
        Some(l) if !job.is_empty() => format!("{l}.{job}"),
        _ => job,
    };
    let state = text(v, "/status/state");
    let since = epoch_ms(num(v, "/statistics/startTime")).or(epoch_ms(num(v, "/statistics/creationTime")));
    ServerProcess {
        id,
        active: true,
        wait: state.as_deref().filter(|s| s.eq_ignore_ascii_case("PENDING")).map(|_| "esperando slots".to_string()),
        status: state,
        user: text(v, "/user_email"),
        command: text(v, "/statistics/query/statementType").or_else(|| text(v, "/configuration/jobType")),
        elapsed_ms: since.map(|t| (now - t).max(0.0) as u64),
        // Slot time is the closest BigQuery has to CPU time.
        cpu_ms: num(v, "/statistics/query/totalSlotMs").or(num(v, "/statistics/totalSlotMs")).map(|v| v.max(0.0) as u64),
        sql: text(v, "/configuration/query/query").map(|q| q.chars().take(MAX_TEXT).collect()),
        ..Default::default()
    }
}

/// The running and pending jobs of one `jobs.list` answer.
fn rows(resp: &Json, now: f64) -> Vec<ServerProcess> {
    resp.get("jobs")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        // Also filtered here: the emulator ignores `stateFilter`.
        .filter(|j| text(j, "/status/state").is_some_and(|s| s.eq_ignore_ascii_case("RUNNING") || s.eq_ignore_ascii_case("PENDING")))
        .filter(|j| !text(j, "/configuration/query/query").is_some_and(|q| q.starts_with(MONITOR_TAG)))
        .map(|j| row(j, now))
        .filter(|p| !p.id.is_empty())
        .collect()
}

impl BigQuerySession {
    pub(crate) async fn processes_list(&self) -> Result<Vec<ServerProcess>> {
        let list = |all: bool| {
            let mut q = vec![
                ("projection", "full".to_string()),
                ("maxResults", MAX_ROWS.to_string()),
                ("stateFilter", "running".into()),
                ("stateFilter", "pending".into()),
            ];
            if all {
                q.push(("allUsers", "true".into()));
            }
            async move { self.api.get(&["jobs"], &q).await }
        };
        let resp = tokio::time::timeout(QUERY_LIMIT, async {
            // Every user's jobs need bigquery.jobs.listAll; otherwise the
            // user's own.
            match list(true).await {
                Ok(r) => Ok(r),
                Err(_) => list(false).await,
            }
        })
        .await
        .map_err(|_| Error::Query("la lista de trabajos tardó demasiado".into()))??;
        let now = SystemTime::now().duration_since(UNIX_EPOCH).map(|d| d.as_millis() as f64).unwrap_or(0.0);
        Ok(rows(&resp, now))
    }

    pub(crate) async fn cancel_running(&self, id: &str) -> Result<()> {
        let (location, job) = parse_id(id).ok_or_else(|| Error::Query(format!("«{}» no es un id de trabajo de BigQuery", id.trim())))?;
        let q: Vec<(&str, String)> = location.or(self.api.location.as_deref()).map(|l| ("location", l.to_string())).into_iter().collect();
        self.api.post(&["jobs", job, "cancel"], &q, &Json::Null).await.map(|_| ()).map_err(|e| match e {
            Error::Query(m) => Error::Query(format!("no se pudo cancelar el trabajo {job}: {m}")),
            e => e,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_are_checked() {
        assert_eq!(parse_id("US.bquxjob_1a2b_3c"), Some((Some("US"), "bquxjob_1a2b_3c")));
        assert_eq!(parse_id("southamerica-east1.job-1"), Some((Some("southamerica-east1"), "job-1")));
        assert_eq!(parse_id("job_1"), Some((None, "job_1")));
        assert_eq!(parse_id("../projects"), None);
        assert_eq!(parse_id("a.b.c"), None);
    }

    #[test]
    fn running_jobs_become_processes() {
        let resp = json!({"jobs": [
            {"jobReference": {"jobId": "job_1", "location": "US"}, "status": {"state": "RUNNING"}, "user_email": "ana@example.com",
             "statistics": {"creationTime": "1700000001000", "startTime": "1700000002000", "query": {"statementType": "SELECT", "totalSlotMs": "30"}},
             "configuration": {"jobType": "QUERY", "query": {"query": "SELECT 1"}}},
            {"jobReference": {"jobId": "job_2"}, "status": {"state": "PENDING"}, "statistics": {"creationTime": "1700000003"},
             "configuration": {"jobType": "LOAD"}},
            {"jobReference": {"jobId": "job_3"}, "status": {"state": "DONE"}},
            {"jobReference": {"jobId": "job_4"}, "status": {"state": "RUNNING"}, "configuration": {"query": {"query": "/* dbine-monitor */ SELECT 1"}}}
        ]});
        let ps = rows(&resp, 1_700_000_005_000.0);
        assert_eq!(ps.len(), 2);
        assert_eq!(ps[0].id, "US.job_1");
        assert_eq!((ps[0].elapsed_ms, ps[0].cpu_ms), (Some(3000), Some(30)));
        assert_eq!(ps[0].command.as_deref(), Some("SELECT"));
        assert!(ps[0].active && ps[0].wait.is_none());
        assert_eq!(ps[1].id, "job_2");
        assert_eq!(ps[1].wait.as_deref(), Some("esperando slots"));
        assert_eq!(ps[1].elapsed_ms, Some(2000), "seconds from the emulator");
        assert_eq!(ps[1].command.as_deref(), Some("LOAD"));
    }
}
