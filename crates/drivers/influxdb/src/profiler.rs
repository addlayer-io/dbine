//! The profiler ([`dbine_driver::profiler`]) per API:
//!
//! - InfluxDB 1.x (InfluxQL): `SHOW QUERIES`, the queries running now,
//!   sampled and scoped to the chosen database. It says how long each has
//!   run but not when it started: the start is worked out when first seen.
//! - InfluxDB 3 (SQL): `system.queries`, the server's log of recent
//!   queries (running and finished, `--query-log-size`, 1000 by default):
//!   complete while they're kept. It has no database column, so the whole
//!   server is watched.
//! - InfluxDB 2 / Cloud (Flux): no API lists other clients' queries
//!   (`SHOW QUERIES` answers "not implemented"; the query log only goes to
//!   the server's log): not supported.
//!
//! Nothing is switched on.

use crate::v1::{statement_tables, InfluxQlSession};
use crate::v3::SqlSession;
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::Value as J;
use std::collections::{HashMap, HashSet};
use std::time::Instant;

/// Rows read from `system.queries` per poll.
const BATCH: usize = 1000;
/// Tags the profiler's own reads of `system.queries`, to leave them out.
const OWN: &str = "/* dbine profiler */";

fn now() -> String {
    chrono::Utc::now().format("%Y-%m-%d %H:%M:%S%.3f").to_string()
}

// -- InfluxDB 1.x -------------------------------------------------------------------------

pub(crate) struct V1State {
    sampler: Sampler,
    database: String,
    /// When each query (by qid) was first seen running.
    first_seen: HashMap<String, String>,
}

pub(crate) async fn v1_start(s: &mut InfluxQlSession, opts: &ProfilerOptions) -> Result<(V1State, ProfilerStarted)> {
    s.query("SHOW QUERIES").await.map_err(|e| match e {
        Error::Query(m) => Error::Query(format!("SHOW QUERIES no respondió (con autenticación hace falta un usuario administrador): {m}")),
        e => e,
    })?;
    let started = ProfilerStarted::new(ProfilerMode::Sampled, "SHOW QUERIES").note(
        "InfluxDB 1.x solo muestra las consultas en curso, sin usuario ni cliente: las que duran menos de una décima \
         de segundo pueden no verse.",
    );
    let state = V1State { sampler: Sampler::new(now()), database: opts.database.clone(), first_seen: HashMap::new() };
    Ok((state, started))
}

pub(crate) async fn v1_poll(s: &mut InfluxQlSession, state: &mut V1State) -> Result<Vec<ProfiledStatement>> {
    let mut out = Vec::new();
    let until = Instant::now() + SAMPLE_FOR;
    loop {
        let results = s.query("SHOW QUERIES").await?;
        let mut samples = Vec::new();
        for (cols, rows) in results.iter().flat_map(statement_tables) {
            let col = |r: &[J], name: &str| cols.iter().position(|c| c == name).and_then(|i| r.get(i)).map(text);
            for r in &rows {
                if let Some(x) = v1_sample(col(r, "qid"), col(r, "query"), col(r, "database"), col(r, "duration"), col(r, "status"), state) {
                    samples.push(x);
                }
            }
        }
        let live: HashSet<&str> = samples.iter().map(|x| x.session.as_str()).collect();
        state.first_seen.retain(|k, _| live.contains(k.as_str()));
        out.extend(state.sampler.feed(samples));
        if Instant::now() + SAMPLE_EVERY > until {
            break;
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

fn text(v: &J) -> String {
    match v {
        J::String(s) => s.clone(),
        J::Null => String::new(),
        other => other.to_string(),
    }
}

/// One `SHOW QUERIES` row, except `SHOW QUERIES` itself and other databases'.
fn v1_sample(
    qid: Option<String>,
    query: Option<String>,
    db: Option<String>,
    duration: Option<String>,
    status: Option<String>,
    state: &mut V1State,
) -> Option<Sample> {
    let (qid, query) = (qid?, query?);
    let db = db.filter(|d| !d.is_empty());
    if query.trim().eq_ignore_ascii_case("SHOW QUERIES") || (!state.database.is_empty() && db.as_deref() != Some(state.database.as_str())) {
        return None;
    }
    let ms = duration.as_deref().and_then(go_duration_ms);
    let started = state
        .first_seen
        .entry(qid.clone())
        .or_insert_with(|| {
            let back = chrono::Duration::microseconds((ms.unwrap_or(0.0) * 1000.0) as i64);
            (chrono::Utc::now() - back).format("%Y-%m-%d %H:%M:%S%.3f").to_string()
        })
        .clone();
    Some(Sample {
        session: qid,
        started,
        text: query,
        running: true,
        duration_ms: ms,
        database: db,
        detail: status.filter(|s| s != "running"),
        ..Default::default()
    })
}

/// A Go duration (`90µs`, `150ms`, `1.5s`, `2m3s`, `1h2m3.5s`) in ms.
fn go_duration_ms(s: &str) -> Option<f64> {
    let mut total = 0.0;
    let mut rest = s.trim();
    if rest.is_empty() {
        return None;
    }
    while !rest.is_empty() {
        let n = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let value: f64 = rest[..n].parse().ok()?;
        rest = &rest[n..];
        let (factor, len) = [("ns", 1e-6), ("µs", 1e-3), ("us", 1e-3), ("ms", 1.0), ("s", 1e3), ("m", 60e3), ("h", 3600e3)]
            .iter()
            .find(|(u, _)| rest.starts_with(u))
            .map(|(u, f)| (*f, u.len()))?;
        total += value * factor;
        rest = &rest[len..];
    }
    Some(total)
}

// -- InfluxDB 3 ---------------------------------------------------------------------------

pub(crate) struct V3State {
    /// The database the log is read through (any one sees the whole log).
    db: String,
    /// `issue_time` read up to: the oldest still-running query, or the newest one.
    after: String,
    /// Queries already reported, with their `issue_time`.
    ids: HashMap<String, String>,
}

pub(crate) async fn v3_start(s: &mut SqlSession, opts: &ProfilerOptions) -> Result<(V3State, ProfilerStarted)> {
    let db = Some(opts.database.clone())
        .filter(|d| !d.is_empty())
        .or_else(|| s.db.clone())
        .unwrap_or_else(|| "_internal".into());
    let q = format!("{OWN} SELECT CAST(now() AS VARCHAR) AS t");
    let (_, rows) = v3_sql(s, &db, &q).await?;
    let t = rows.first().and_then(|r| r.first()).map(text).unwrap_or_default();
    let started = ProfilerStarted::new(ProfilerMode::Complete, "system.queries").note(
        "InfluxDB 3 guarda las últimas consultas de todo el servidor (1000 por omisión, --query-log-size) sin \
         indicar la base, el usuario ni el cliente: se ven las de todas las bases.",
    );
    Ok((V3State { db, after: t.replace('T', " ").trim_end_matches('Z').to_string(), ids: HashMap::new() }, started))
}

/// A query through `db`, whatever the session's database is.
async fn v3_sql(s: &mut SqlSession, db: &str, q: &str) -> Result<(Vec<String>, Vec<Vec<J>>)> {
    let saved = s.db.replace(db.to_string());
    let r = s.sql(q).await;
    s.db = saved;
    r
}

pub(crate) async fn v3_poll(s: &mut SqlSession, state: &mut V3State) -> Result<Vec<ProfiledStatement>> {
    let after = state.after.replace('\'', "''");
    let q = format!(
        "{OWN} SELECT id, CAST(issue_time AS VARCHAR) AS issue_time, query_type, query_text, \
                CAST(end2end_duration AS VARCHAR) AS duration, CAST(compute_duration AS VARCHAR) AS compute, success, running, cancelled, phase, partitions, parquet_files \
         FROM system.queries \
         WHERE issue_time >= TIMESTAMP '{after}' AND query_text NOT LIKE '{OWN}%' \
         ORDER BY issue_time LIMIT {BATCH}"
    );
    let (cols, rows) = v3_sql(s, &state.db.clone(), &q).await?;
    Ok(v3_rows(&cols, &rows, state))
}

fn v3_rows(cols: &[String], rows: &[Vec<J>], state: &mut V3State) -> Vec<ProfiledStatement> {
    let mut out = Vec::new();
    let mut oldest_running: Option<String> = None;
    let mut newest = state.after.clone();
    for r in rows {
        let get = |name: &str| cols.iter().position(|c| c == name).and_then(|i| r.get(i)).filter(|v| !v.is_null());
        let (Some(id), Some(issued)) = (get("id").map(text), get("issue_time").map(text)) else { continue };
        let issued = issued.replace('T', " ").trim_end_matches('Z').to_string();
        if issued > newest {
            newest = issued.clone();
        }
        if get("running").and_then(J::as_bool) == Some(true) {
            if oldest_running.as_ref().is_none_or(|o| issued < *o) {
                oldest_running = Some(issued);
            }
            continue;
        }
        if state.ids.insert(id, issued.clone()).is_some() {
            continue;
        }
        let text_ = get("query_text").map(text).unwrap_or_default();
        if text_.trim().is_empty() || text_.starts_with(OWN) {
            continue;
        }
        let error = if get("cancelled").and_then(J::as_bool) == Some(true) {
            Some("cancelada".to_string())
        } else if get("success").and_then(J::as_bool) == Some(false) {
            Some(format!("falló (fase {})", get("phase").map(text).unwrap_or_default()))
        } else {
            None
        };
        let n = |k: &str| get(k).and_then(J::as_u64);
        let mut detail = vec![get("query_type").map(text).unwrap_or_default()];
        if let (Some(p), Some(f)) = (n("partitions"), n("parquet_files")) {
            detail.push(format!("{p} particiones, {f} archivos Parquet"));
        }
        out.push(ProfiledStatement {
            time: millis(&issued),
            duration_ms: get("duration").map(text).as_deref().and_then(iso_duration_ms),
            // The CPU time the query's execution used. Nothing counts what
            // it read or wrote (the partitions and files stay in detail).
            cpu_ms: get("compute").map(text).as_deref().and_then(iso_duration_ms),
            text: text_,
            error,
            detail: Some(detail.join("; ")).filter(|d| !d.is_empty()),
            ..Default::default()
        });
    }
    // Read again from the oldest query still running (its row changes when
    // it ends); the ids leave out what was already reported.
    state.after = oldest_running.unwrap_or(newest);
    let after = state.after.clone();
    state.ids.retain(|_, issued| *issued >= after);
    out.sort_by(|a, b| a.time.cmp(&b.time));
    out
}

/// A time with nanoseconds at millisecond precision.
fn millis(t: &str) -> String {
    match t.find('.') {
        Some(i) if t.len() > i + 4 => t[..i + 4].to_string(),
        Some(_) => t.to_string(),
        None => format!("{t}.000"),
    }
}

/// An Arrow interval as text: ISO 8601 (`PT0.0019S`) or DataFusion's
/// (`0 days 0 hours 0 mins 0.001905855 secs`), in ms.
fn iso_duration_ms(s: &str) -> Option<f64> {
    if let Some(t) = s.strip_prefix("PT") {
        let mut total = 0.0;
        let mut n = String::new();
        for c in t.chars() {
            match c {
                '0'..='9' | '.' => n.push(c),
                u => {
                    let x: f64 = n.parse().ok()?;
                    n.clear();
                    total += x * match u {
                        'H' => 3_600_000.0,
                        'M' => 60_000.0,
                        'S' => 1_000.0,
                        _ => return None,
                    };
                }
            }
        }
        return Some(total);
    }
    let words: Vec<&str> = s.split_whitespace().collect();
    let mut total = 0.0;
    for pair in words.chunks(2) {
        let [v, unit] = pair else { return None };
        let x: f64 = v.parse().ok()?;
        total += x * match unit.trim_end_matches('s') {
            "day" => 86_400_000.0,
            "hour" => 3_600_000.0,
            "min" => 60_000.0,
            "sec" => 1_000.0,
            _ => return None,
        };
    }
    (!words.is_empty()).then_some(total)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn go_durations() {
        assert_eq!(go_duration_ms("150ms"), Some(150.0));
        assert_eq!(go_duration_ms("90µs"), Some(0.09));
        assert_eq!(go_duration_ms("2m3.5s"), Some(123_500.0));
        assert_eq!(go_duration_ms("1h"), Some(3_600_000.0));
        assert_eq!(go_duration_ms("x"), None);
    }

    #[test]
    fn interval_durations() {
        assert_eq!(iso_duration_ms("PT0.25S"), Some(250.0));
        assert_eq!(iso_duration_ms("PT1M0.5S"), Some(60_500.0));
        assert_eq!(iso_duration_ms("0 days 0 hours 0 mins 1.5 secs"), Some(1500.0));
    }

    #[test]
    fn v1_rows_keep_their_start() {
        let mut st = V1State { sampler: Sampler::new(""), database: "db".into(), first_seen: HashMap::new() };
        let mut row = |q: &str, db: &str| v1_sample(Some("7".into()), Some(q.into()), Some(db.into()), Some("1s".into()), Some("running".into()), &mut st);
        let a = row("SELECT 1", "db").unwrap();
        assert_eq!(a.duration_ms, Some(1000.0));
        assert!(row("SHOW QUERIES", "db").is_none());
        assert!(row("SELECT 1", "other").is_none());
        let b = v1_sample(Some("7".into()), Some("SELECT 1".into()), Some("db".into()), Some("2s".into()), None, &mut st).unwrap();
        assert_eq!(a.started, b.started);
    }

    #[test]
    fn v3_rows_once_and_after_they_end() {
        let cols: Vec<String> = ["id", "issue_time", "query_text", "duration", "compute", "success", "running"].map(String::from).to_vec();
        let mut st = V3State { db: "d".into(), after: "2026-01-01 00:00:00".into(), ids: HashMap::new() };
        let rows = vec![
            vec![
                json!("a"),
                json!("2026-01-01T00:00:01.123456789"),
                json!("SELECT 1"),
                json!("PT0.1S"),
                json!("0 days 0 hours 0 mins 0.000672337 secs"),
                json!(true),
                json!(false),
            ],
            vec![json!("b"), json!("2026-01-01T00:00:02"), json!("SELECT 2"), J::Null, J::Null, J::Null, json!(true)],
        ];
        let out = v3_rows(&cols, &rows, &mut st);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].time.as_str(), out[0].duration_ms), ("2026-01-01 00:00:01.123", Some(100.0)));
        assert!((out[0].cpu_ms.unwrap() - 0.672337).abs() < 1e-9);
        assert_eq!((out[0].reads, out[0].writes), (None, None));
        assert_eq!(st.after, "2026-01-01 00:00:02");
        // Read again from the running one, which has ended.
        let rows = vec![vec![json!("b"), json!("2026-01-01T00:00:02"), json!("SELECT 2"), json!("PT2S"), J::Null, json!(false), json!(false)]];
        let out = v3_rows(&cols, &rows, &mut st);
        assert_eq!(out.len(), 1);
        assert_eq!((out[0].text.as_str(), out[0].error.is_some()), ("SELECT 2", true));
        assert!(v3_rows(&cols, &rows, &mut st).is_empty(), "reported once");
    }
}
