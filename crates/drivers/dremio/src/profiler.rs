//! The profiler ([`dbine_driver::profiler`]): Dremio keeps every job
//! (`sys.jobs_recent`, or `sys.jobs` before 25; kept `jobs.max.age_in_days`),
//! so it's complete: each poll reads the jobs that ended since the last one.
//! Jobs are scoped to the chosen container by their context or by the
//! datasets they read. The profiler's own reads carry a comment and are
//! left out. Nothing is switched on.

use crate::{text, DremioSession};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::{Map, Value};
use std::collections::HashMap;

/// Starts the profiler's own statements, to leave them out.
const OWN: &str = "/* dbine profiler */";
/// Jobs read per poll.
const BATCH: usize = 1000;

pub(crate) struct State {
    /// `sys.jobs_recent` or `sys.jobs`.
    table: &'static str,
    /// Only this container's jobs ("" = all).
    database: String,
    /// `submitted_epoch_millis` read from: the oldest job still running, or the newest seen.
    after: i64,
    /// Jobs already reported at or after `after`, with their submit time.
    seen: HashMap<String, i64>,
}

pub(crate) async fn start(s: &DremioSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let mut last = Err(Error::Query(String::new()));
    let mut table = "";
    for t in ["sys.jobs_recent", "sys.jobs"] {
        last = s.strings(&format!("{OWN} SELECT max(submitted_epoch_millis) AS t FROM {t}")).await;
        if last.is_ok() {
            table = t;
            break;
        }
    }
    let rows = last.map_err(|e| match e {
        Error::Query(m) => Error::Query(format!("No se pudo leer el historial de trabajos (sys.jobs_recent / sys.jobs): {m}")),
        e => e,
    })?;
    // Jobs submitted up to now ran before profiling: not reported.
    let after = rows.first().and_then(|r| r.first()).and_then(|t| t.parse::<i64>().ok()).map_or(0, |t| t + 1);
    let mut note = String::from(
        "Dremio guarda cada trabajo; aparece al terminar. Sin permiso de administrador solo se ven los trabajos propios.",
    );
    if !opts.database.is_empty() {
        note.push_str(&format!(" Se muestran los que corren en {} o leen sus datasets.", opts.database));
    }
    let state = State { table, database: opts.database.clone(), after, seen: HashMap::new() };
    Ok((state, ProfilerStarted::new(ProfilerMode::Complete, table).units(Some("filas"), None).note(note)))
}

pub(crate) async fn poll(s: &DremioSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    // `context` exists only in `sys.jobs_recent`.
    let context = if state.table == "sys.jobs_recent" { "context" } else { "'' AS context" };
    let sql = format!(
        "{OWN} SELECT job_id, status, query_type, user_name, queried_datasets, {context}, submitted_epoch_millis, \
                final_state_epoch_millis, rows_returned, rows_scanned, execution_cpu_time_millis, queue_name, error_msg, query \
         FROM {} \
         WHERE submitted_epoch_millis >= {} AND query NOT LIKE '{OWN}%' \
         ORDER BY submitted_epoch_millis LIMIT {BATCH}",
        state.table, state.after
    );
    let rows = s.records(&sql).await?;
    Ok(jobs(&rows, state))
}

fn jobs(rows: &[Map<String, Value>], state: &mut State) -> Vec<ProfiledStatement> {
    let mut out = Vec::new();
    let mut oldest_running: Option<i64> = None;
    let mut newest = state.after;
    for r in rows {
        let n = |k: &str| r.get(k).and_then(|v| v.as_i64().or_else(|| text(v).parse().ok()));
        let (Some(id), Some(submitted)) = (r.get("job_id").map(text), n("submitted_epoch_millis")) else { continue };
        newest = newest.max(submitted);
        let status = r.get("status").map(text).unwrap_or_default();
        if !matches!(status.as_str(), "COMPLETED" | "FAILED" | "CANCELED" | "CANCELLED") {
            oldest_running = Some(oldest_running.map_or(submitted, |o| o.min(submitted)));
            continue;
        }
        if state.seen.insert(id, submitted).is_some() {
            continue;
        }
        let query = r.get("query").map(text).unwrap_or_default();
        let context = r.get("context").map(text).unwrap_or_default();
        let datasets = r.get("queried_datasets").map(text).unwrap_or_default();
        if query.trim().is_empty() || query.starts_with(OWN) || !in_database(&state.database, &context, &datasets) {
            continue;
        }
        let end = n("final_state_epoch_millis").filter(|e| *e > 0);
        let error = match status.as_str() {
            "FAILED" => Some(r.get("error_msg").map(text).filter(|e| !e.is_empty()).unwrap_or_else(|| "falló".into())),
            "CANCELED" | "CANCELLED" => Some("cancelado".into()),
            _ => None,
        };
        let mut detail = Vec::new();
        for (k, label) in [("query_type", ""), ("queue_name", "cola ")] {
            if let Some(v) = r.get(k).map(text).filter(|v| !v.is_empty()) {
                detail.push(format!("{label}{v}"));
            }
        }
        out.push(ProfiledStatement {
            time: utc_ms(submitted),
            duration_ms: end.map(|e| (e - submitted).max(0) as f64),
            text: query,
            database: first_level(&context),
            user: r.get("user_name").map(text).filter(|u| !u.is_empty()),
            rows: n("rows_returned").map(|v| v.max(0) as u64),
            cpu_ms: n("execution_cpu_time_millis").map(|v| v.max(0) as f64),
            // Rows, not bytes: bytes_scanned stays 0 for many sources (the
            // system tables, some connectors). Jobs report nothing written.
            reads: n("rows_scanned").map(|v| v.max(0) as u64),
            error,
            detail: Some(detail.join("; ")),
            ..Default::default()
        });
    }
    // Never back: what started before `after` was seen, or ran before profiling.
    state.after = state.after.max(oldest_running.map_or(newest, |o| o.min(newest)));
    let after = state.after;
    state.seen.retain(|_, t| *t >= after);
    out.sort_by(|a, b| a.time.cmp(&b.time));
    out
}

/// The container of a job's context (`[space, folder]` → `space`).
fn first_level(context: &str) -> Option<String> {
    let c = context.trim().trim_start_matches('[').trim_end_matches(']');
    c.split(',').next().map(|p| p.trim().to_string()).filter(|p| !p.is_empty())
}

/// Whether a job runs in `db`'s container (its context) or reads one of
/// its datasets (`[db.t, other.u]`).
fn in_database(db: &str, context: &str, datasets: &str) -> bool {
    if db.is_empty() {
        return true;
    }
    let db = db.split('.').next().unwrap_or(db);
    let is = |p: &str| p.trim().trim_matches('"').eq_ignore_ascii_case(db);
    if first_level(context).is_some_and(|c| is(&c)) {
        return true;
    }
    datasets
        .trim()
        .trim_start_matches('[')
        .trim_end_matches(']')
        .split(", ")
        .any(|d| d.split('.').next().is_some_and(is))
}

fn utc_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let sod = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}.{:03}", sod / 3600, sod % 3600 / 60, sod % 60, ms.rem_euclid(1000))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(id: &str, status: &str, submitted: i64, context: &str, datasets: &str) -> Map<String, Value> {
        json!({"job_id": id, "status": status, "submitted_epoch_millis": submitted, "final_state_epoch_millis": submitted + 250,
               "context": context, "queried_datasets": datasets, "query": format!("SELECT {id}"), "user_name": "u",
               "rows_returned": 3, "query_type": "REST", "rows_scanned": 803, "execution_cpu_time_millis": "887", "queue_name": "SMALL"})
        .as_object()
        .cloned()
        .unwrap()
    }

    #[test]
    fn times_are_utc() {
        assert_eq!(utc_ms(1_790_544_121_467), "2026-09-27 21:22:01.467");
    }

    #[test]
    fn scope() {
        assert!(in_database("", "", ""));
        assert!(in_database("sys", "[sys]", ""));
        assert!(in_database("sys", "", "[sys.version]"));
        assert!(in_database("@dbine", "[@dbine, sub]", ""));
        assert!(!in_database("sys", "[other]", "[other.t]"));
    }

    #[test]
    fn jobs_once_after_they_end() {
        let mut st = State { table: "sys.jobs_recent", database: String::new(), after: 100, seen: HashMap::new() };
        let out = jobs(&[row("a", "COMPLETED", 100, "", ""), row("b", "RUNNING", 200, "", "")], &mut st);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].duration_ms, out[0].rows), (Some(250.0), Some(3)));
        assert_eq!((out[0].cpu_ms, out[0].reads, out[0].writes), (Some(887.0), Some(803), None));
        assert_eq!(out[0].detail.as_deref(), Some("REST; cola SMALL"));
        assert_eq!(st.after, 200);
        let out = jobs(&[row("b", "FAILED", 200, "", ""), row("c", "COMPLETED", 300, "", "")], &mut st);
        assert_eq!(out.len(), 2);
        assert_eq!(out[0].error.as_deref(), Some("falló"));
        assert!(jobs(&[row("c", "COMPLETED", 300, "", "")], &mut st).is_empty(), "reported once");
    }
}
