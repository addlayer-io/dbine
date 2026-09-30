//! The profiler ([`dbine_driver::profiler`]) for TDengine:
//! `performance_schema.perf_queries`, sampled. Clients report their running
//! queries with each heartbeat (about every second), so the view shows a
//! query only when it outlives one, with the time it had run at that
//! heartbeat, and keeps it until the next. It doesn't say which database a
//! query uses: the whole server is watched.
//!
//! The slow query log would be complete, but it reaches SQL only through
//! taosKeeper's `log` database, when the cluster runs one.

use crate::{text, TdSession};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use serde_json::Value;
use std::time::Instant;

const QUERIES: &str = "SELECT kill_id, CAST(create_time AS BIGINT) AS started, exec_usec, `sql`, `user`, app, user_app, \
                              user_ip, end_point, sub_num \
                       FROM performance_schema.perf_queries";

pub(crate) struct State {
    sampler: Sampler,
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.parse().ok(),
        _ => None,
    }
}

impl TdSession {
    pub(crate) async fn profiler_begin(&self, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
        let now = self.records("SELECT CAST(NOW() AS BIGINT) AS t").await?;
        let now = now.first().and_then(|r| num(r.get("t"))).unwrap_or(0.0) as i64;
        // Fail now (no privilege on performance_schema) rather than at every poll.
        self.records(QUERIES).await?;
        let mut note = String::from(
            "TDengine publica las consultas en curso con el latido de cada cliente (cerca de 1 s): solo se ven las que \
             duran más que eso, con el tiempo que llevaban en ese latido.",
        );
        if !opts.database.is_empty() {
            note.push_str(" No informa la base de cada consulta: se ven las de todo el servidor.");
        }
        Ok((
            State { sampler: Sampler::new(utc(now * 1000)) },
            ProfilerStarted::new(ProfilerMode::Sampled, "performance_schema.perf_queries").note(note),
        ))
    }

    pub(crate) async fn profiler_next(&self, state: &mut State) -> Result<Vec<ProfiledStatement>> {
        let mut out = Vec::new();
        let until = Instant::now() + SAMPLE_FOR;
        loop {
            let rows = self.records(QUERIES).await?;
            let samples = rows
                .iter()
                .filter_map(|r| {
                    let t = |k: &str| r.get(k).map(text).filter(|v| !v.is_empty());
                    let sql = t("sql")?;
                    // This one (the view shows the text in lower case).
                    if sql.eq_ignore_ascii_case(QUERIES) {
                        return None;
                    }
                    Some(Sample {
                        session: t("kill_id")?,
                        started: utc(num(r.get("started"))? as i64 * 1000),
                        text: sql,
                        running: true,
                        duration_ms: num(r.get("exec_usec")).map(|us| us / 1000.0),
                        user: t("user"),
                        client: t("user_ip").or_else(|| t("end_point")),
                        application: t("user_app").or_else(|| t("app")),
                        detail: num(r.get("sub_num")).filter(|n| *n > 0.0).map(|n| format!("subconsultas: {n}")),
                        ..Default::default()
                    })
                })
                .collect();
            out.extend(state.sampler.feed(samples));
            if Instant::now() + SAMPLE_EVERY > until {
                break;
            }
            tokio::time::sleep(SAMPLE_EVERY).await;
        }
        out.sort_by(|a, b| a.time.cmp(&b.time));
        Ok(out)
    }
}

/// Epoch microseconds as `YYYY-MM-DD HH:MM:SS.mmm`, UTC.
fn utc(us: i64) -> String {
    let ms = us.div_euclid(1000);
    let (days, rem) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    // Civil date from days since 1970-01-01 (Howard Hinnant's algorithm).
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn epoch_to_utc() {
        assert_eq!(utc(1_700_000_000_123_456), "2023-11-14 22:13:20.123");
    }
}
