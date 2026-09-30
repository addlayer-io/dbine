//! The profiler ([`dbine_driver::profiler`]) for Cloud Spanner: sampled,
//! from `SPANNER_SYS.OLDEST_ACTIVE_QUERIES` (the queries running now in the
//! database). Spanner keeps no per-statement history: its other statistics
//! (`QUERY_STATS_TOP_MINUTE`…) are aggregated per minute and per query
//! shape. A query that runs between two looks goes unseen.
//!
//! Nothing is switched on. The profiler reads through a Spanner session of
//! its own on the watched database, whose queries are left out; it's
//! deleted at stop. The emulator has no `SPANNER_SYS`.

use crate::{create_session, Json, SpannerSession};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::json;
use std::time::Instant;

const SAMPLE_SQL: &str = "SELECT QUERY_ID, SESSION_ID, START_TIME, CURRENT_TIMESTAMP() AS NOW, TEXT, \
                                 CLIENT_IP_ADDRESS, USER_AGENT_HEADER, TRANSACTION_TYPE, PRIORITY \
                          FROM SPANNER_SYS.OLDEST_ACTIVE_QUERIES";

pub(crate) struct State {
    /// The profiler's own Spanner session (a resource name).
    session: String,
    database: String,
    sampler: Sampler,
}

impl State {
    pub(crate) fn session(&self) -> String {
        self.session.clone()
    }
}

/// An RFC 3339 timestamp in UTC (`2024-01-31T10:00:00.123456Z`, as the REST
/// API gives them) as epoch milliseconds.
fn epoch_ms(t: &str) -> Option<i64> {
    let t = t.trim().strip_suffix('Z')?;
    let (date, time) = t.split_once(['T', ' '])?;
    let mut d = date.splitn(3, '-').map(|p| p.parse::<i64>().ok());
    let (y, m, day) = (d.next()??, d.next()??, d.next()??);
    let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
    let mut h = hms.splitn(3, ':').map(|p| p.parse::<i64>().ok());
    let (hh, mm, ss) = (h.next()??, h.next()??, h.next()??);
    let ms: i64 = format!("{:0<3}", frac.chars().take(3).collect::<String>()).parse().ok()?;
    // Days since 1970-01-01 from a civil date (H. Hinnant's algorithm).
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + day - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 24 + hh) * 60 + mm) * 60_000 + ss * 1000 + ms)
}

/// Epoch milliseconds as `YYYY-MM-DD HH:MM:SS.mmm` (UTC).
fn utc(ms: i64) -> String {
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
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

/// Rows of a read as text (NULL: None).
async fn rows(s: &SpannerSession, session: &str, sql: &str) -> Result<Vec<Vec<Option<String>>>> {
    let r = s.api.post(&format!("{session}:executeSql"), &json!({ "sql": sql })).await?;
    Ok(r.get("rows")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .map(|row| {
            row.as_array()
                .into_iter()
                .flatten()
                .map(|v| match v {
                    Json::Null => None,
                    Json::String(s) => Some(s.clone()),
                    other => Some(other.to_string()),
                })
                .collect()
        })
        .collect())
}

/// One row of [`SAMPLE_SQL`] as a sample; None for the profiler's own.
fn sample(r: &[Option<String>], own: &str, database: &str) -> Option<Sample> {
    let g = |i: usize| r.get(i).cloned().flatten().filter(|v| !v.trim().is_empty());
    let (id, session, start, now, text) = (g(0)?, g(1).unwrap_or_default(), g(2)?, g(3), g(4).unwrap_or_default());
    // SESSION_ID is the session's id, the last part of its name.
    if (!session.is_empty() && own.rsplit('/').next() == Some(session.rsplit('/').next().unwrap_or(&session)))
        || text.contains("SPANNER_SYS.OLDEST_ACTIVE_QUERIES")
    {
        return None;
    }
    let start = epoch_ms(&start)?;
    let detail = [g(7), g(8)].into_iter().flatten().collect::<Vec<_>>().join(" · ");
    Some(Sample {
        session: id,
        started: utc(start),
        text,
        running: true,
        duration_ms: now.as_deref().and_then(epoch_ms).map(|n| (n - start).max(0) as f64),
        database: Some(database.to_string()),
        client: g(5),
        application: g(6),
        detail: (!detail.is_empty()).then_some(detail),
        ..Default::default()
    })
}

pub(crate) async fn start(s: &SpannerSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let current = s.database.rsplit('/').next().unwrap_or(&s.database).to_string();
    let database = if opts.database.is_empty() { current } else { opts.database.clone() };
    let session = create_session(&s.api, &format!("{}/databases/{database}", s.instance)).await?;
    let state = State { session, database, sampler: Sampler::default() };
    let probe = async {
        let now = rows(s, &state.session, "SELECT CURRENT_TIMESTAMP() AS NOW").await?;
        let now = now.first().and_then(|r| r.first().cloned().flatten()).and_then(|t| epoch_ms(&t)).map(utc);
        rows(s, &state.session, SAMPLE_SQL)
            .await
            .map_err(|e| match s.api.base.starts_with(crate::API) {
                true => Error::Query(format!("no se pudo leer SPANNER_SYS.OLDEST_ACTIVE_QUERIES: {e}")),
                false => Error::Unsupported("el emulador de Spanner no implementa SPANNER_SYS: no hay consultas que ver".into()),
            })?;
        Ok::<_, Error>(now.unwrap_or_default())
    };
    match probe.await {
        Ok(since) => {
            let state = State { sampler: Sampler::new(since), ..state };
            let started = ProfilerStarted::new(ProfilerMode::Sampled, "SPANNER_SYS.OLDEST_ACTIVE_QUERIES").note(
                "Spanner solo muestra las consultas en curso; las que terminan entre dos muestras no se ven. \
                 No informa el usuario de cada consulta.",
            );
            Ok((state, started))
        }
        Err(e) => {
            stop(s, state).await;
            Err(e)
        }
    }
}

pub(crate) async fn poll(s: &SpannerSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    let mut out = Vec::new();
    let until = Instant::now() + SAMPLE_FOR;
    loop {
        let rs = rows(s, &state.session, SAMPLE_SQL).await?;
        out.extend(state.sampler.feed(rs.iter().filter_map(|r| sample(r, &state.session, &state.database)).collect()));
        if Instant::now() + SAMPLE_EVERY > until {
            break;
        }
        tokio::time::sleep(SAMPLE_EVERY).await;
    }
    out.sort_by(|a, b| a.time.cmp(&b.time));
    Ok(out)
}

/// Drops the profiler's session.
pub(crate) async fn stop(s: &SpannerSession, state: State) {
    let url = format!("{}/v1/{}", s.api.base, state.session);
    if let Err(e) = s.api.send(s.api.http.delete(url)).await {
        tracing::debug!("spanner profiler session delete failed: {e}");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn row(id: &str, session: &str, text: &str) -> Vec<Option<String>> {
        [
            Some(id),
            Some(session),
            Some("2024-01-31T10:00:00.123456Z"),
            Some("2024-01-31T10:00:01.623Z"),
            Some(text),
            Some("10.0.0.1"),
            Some("dbine/1.0"),
            Some("READ_ONLY"),
            None,
        ]
        .into_iter()
        .map(|v| v.map(str::to_string))
        .collect()
    }

    #[test]
    fn timestamps_round_trip() {
        assert_eq!(epoch_ms("1970-01-01T00:00:00Z"), Some(0));
        assert_eq!(epoch_ms("2000-02-29T12:34:56.7Z"), Some(951_827_696_700));
        assert_eq!(utc(epoch_ms("2024-01-31T10:00:00.123456Z").unwrap()), "2024-01-31 10:00:00.123");
        assert_eq!(epoch_ms("2024-01-31T10:00:00+01:00"), None);
    }

    #[test]
    fn samples_leave_out_the_profiler() {
        let own = "projects/p/instances/i/databases/d/sessions/ABC";
        let s = sample(&row("1", "XYZ", "SELECT * FROM t"), own, "d").unwrap();
        assert_eq!((s.session.as_str(), s.started.as_str()), ("1", "2024-01-31 10:00:00.123"));
        assert_eq!(s.duration_ms, Some(1500.0));
        assert_eq!(s.client.as_deref(), Some("10.0.0.1"));
        assert_eq!(s.application.as_deref(), Some("dbine/1.0"));
        assert_eq!(s.detail.as_deref(), Some("READ_ONLY"));
        assert!(sample(&row("2", "ABC", "SELECT 1"), own, "d").is_none());
        assert!(sample(&row("3", "XYZ", "SELECT * FROM SPANNER_SYS.OLDEST_ACTIVE_QUERIES"), own, "d").is_none());
    }
}
