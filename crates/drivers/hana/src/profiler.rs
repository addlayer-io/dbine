//! The profiler ([`dbine_driver::profiler`]). Untested: there's no HANA
//! to run against (see `tests/integration.rs`).
//!
//! - Complete: the expensive statements trace (`M_EXPENSIVE_STATEMENTS`)
//!   records every statement slower than `threshold_duration`. When
//!   allowed (INIFILE ADMIN), the profiler switches it on
//!   (`global.ini` › `expensive_statement` › `enable = true`,
//!   `threshold_duration = 1` µs) at the tenant's layer (`DATABASE`, or
//!   `SYSTEM` in SYSTEMDB) and puts each key back as it was at stop (its
//!   old value, or unset). Read-only connections use it only when it's
//!   already on.
//! - Sampled otherwise: `M_ACTIVE_STATEMENTS`, what runs at each look.
//!
//! The "database" is a schema: the connections whose current schema it is
//! (`M_CONNECTIONS`). Reading other connections' statements needs the
//! MONITORING role or CATALOG READ.

use crate::{err, int, message, num, text, HanaSession};
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use hdbconnect_async::HdbValue;
use std::collections::HashSet;
use std::time::Instant;

/// Rows read from the trace per poll.
const BATCH: usize = 1000;
/// The trace writes a statement when it ends, not always in order: each
/// poll reads again this far behind the latest end seen.
const LOOKBACK_S: u32 = 5;
const SECTION: &str = "expensive_statement";

pub(crate) enum State {
    Sampled { sampler: Sampler, sql: String },
    /// The trace read past `after` (latest end seen, server local time);
    /// `seen` are the statements read in the lookback window.
    Trace { after: String, seen: HashSet<String>, scope: String, restore: Vec<String> },
}

/// A server timestamp (local time) as UTC text.
fn utc(col: &str) -> String {
    format!("TO_VARCHAR(ADD_SECONDS({col}, SECONDS_BETWEEN(CURRENT_TIMESTAMP, CURRENT_UTCTIMESTAMP)), 'YYYY-MM-DD HH24:MI:SS.FF3')")
}

fn quote(s: &str) -> String {
    s.replace('\'', "''")
}

fn scope(schema: &str) -> String {
    if schema.is_empty() {
        String::new()
    } else {
        format!(" AND c.CURRENT_SCHEMA_NAME = '{}'", quote(schema))
    }
}

pub(crate) async fn start(s: &HanaSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let now = s.rows("SELECT TO_VARCHAR(CURRENT_TIMESTAMP, 'YYYY-MM-DD HH24:MI:SS.FF7') FROM DUMMY", &[]).await?;
    let now_local = now.first().and_then(|r| r.first()).and_then(text).unwrap_or_default();
    let since = s.rows(&format!("SELECT {} FROM DUMMY", utc("CURRENT_TIMESTAMP")), &[]).await?;
    let since = since.first().and_then(|r| r.first()).and_then(text).unwrap_or_default();

    let trace = |restore: Vec<String>| State::Trace { after: now_local.clone(), seen: HashSet::new(), scope: scope(&opts.database), restore };
    let sampled = |note: String| {
        (
            State::Sampled { sampler: Sampler::new(since.clone()), sql: sample_sql(&opts.database) },
            ProfilerStarted::new(ProfilerMode::Sampled, "M_ACTIVE_STATEMENTS").note(note),
        )
    };
    let source = "M_EXPENSIVE_STATEMENTS";

    // What's configured now, per layer.
    let config = s
        .rows(
            &format!(
                "SELECT LAYER_NAME, KEY, VALUE FROM SYS.M_INIFILE_CONTENTS \
                  WHERE FILE_NAME = 'global.ini' AND SECTION = '{SECTION}' AND KEY IN ('enable', 'threshold_duration')"
            ),
            &[],
        )
        .await
        .unwrap_or_default();
    let config: Vec<(String, String, String)> = config
        .iter()
        .filter_map(|r| Some((r.first().and_then(text)?, r.get(1).and_then(text)?, r.get(2).and_then(text)?)))
        .collect();
    // The most specific layer wins.
    let effective = |key: &str| {
        ["DATABASE", "SYSTEM", "DEFAULT"]
            .iter()
            .find_map(|l| config.iter().find(|(layer, k, _)| layer == l && k == key).map(|(_, _, v)| v.clone()))
    };
    let enabled = effective("enable").is_some_and(|v| v.eq_ignore_ascii_case("true"));
    let threshold: f64 = effective("threshold_duration").and_then(|v| v.parse().ok()).unwrap_or(1_000_000.0);
    let readable = s.rows("SELECT TOP 1 1 FROM SYS.M_EXPENSIVE_STATEMENTS", &[]).await.is_ok();

    if enabled && threshold <= 1000.0 && readable {
        return Ok((trace(Vec::new()), ProfilerStarted::new(ProfilerMode::Complete, source)));
    }
    if !opts.change_server {
        if enabled && readable {
            return Ok((
                trace(Vec::new()),
                ProfilerStarted::new(ProfilerMode::Complete, source).note(format!(
                    "Conexión de solo lectura: la traza de sentencias costosas registra solo las que duran más de {} ms.",
                    threshold / 1000.0
                )),
            ));
        }
        return Ok(sampled(
            "Conexión de solo lectura y la traza de sentencias costosas está apagada: se muestrean las sentencias en curso; \
             las muy rápidas pueden no verse."
                .into(),
        ));
    }

    let db = s.rows("SELECT DATABASE_NAME FROM SYS.M_DATABASE", &[]).await?;
    let layer = if db.first().and_then(|r| r.first()).and_then(text).as_deref() == Some("SYSTEMDB") { "SYSTEM" } else { "DATABASE" };
    let at_layer = |key: &str| config.iter().find(|(l, k, _)| l == layer && k == key).map(|(_, _, v)| v.clone());
    let alter = |what: String| format!("ALTER SYSTEM ALTER CONFIGURATION ('global.ini', '{layer}') {what} WITH RECONFIGURE");
    let mut restore = Vec::new();
    let mut started = ProfilerStarted::new(ProfilerMode::Complete, source);
    for (key, value) in [("threshold_duration", "1"), ("enable", "true")] {
        let was = at_layer(key);
        if was.as_deref() == Some(value) {
            continue;
        }
        if let Err(e) = s.conn.exec(alter(format!("SET ('{SECTION}', '{key}') = '{value}'"))).await {
            let e = message(&e);
            undo(s, &restore).await;
            return Ok(sampled(format!(
                "No se pudo activar la traza de sentencias costosas ({e}; hace falta INIFILE ADMIN): se muestrean las \
                 sentencias en curso; las muy rápidas pueden no verse."
            )));
        }
        let (back, before) = match &was {
            Some(v) => (alter(format!("SET ('{SECTION}', '{key}') = '{}'", quote(v))), format!("estaba en {v}")),
            None => (alter(format!("UNSET ('{SECTION}', '{key}')")), "no estaba definido".to_string()),
        };
        restore.push(back);
        started = started.change(format!("global.ini [{SECTION}] {key} = {value} en la capa {layer} ({before})"));
    }
    Ok((trace(restore), started))
}

async fn undo(s: &HanaSession, restore: &[String]) {
    for sql in restore.iter().rev() {
        if let Err(e) = s.conn.exec(sql.clone()).await {
            tracing::warn!("hana profiler: {sql}: {}", message(&e));
        }
    }
}

pub(crate) async fn poll(s: &HanaSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    match state {
        State::Sampled { sampler, sql } => {
            let mut out = Vec::new();
            let until = Instant::now() + SAMPLE_FOR;
            loop {
                let rows = s.rows(sql, &[]).await?;
                out.extend(sampler.feed(rows.iter().map(|r| sample(r)).collect()));
                if Instant::now() + SAMPLE_EVERY > until {
                    break;
                }
                tokio::time::sleep(SAMPLE_EVERY).await;
            }
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
        State::Trace { after, seen, scope, .. } => {
            let rows = s.rows(&trace_sql(after, scope), &[]).await?;
            let mut out = Vec::new();
            let mut now_seen = HashSet::new();
            for r in &rows {
                let t = |i: usize| r.get(i).and_then(text).filter(|v| !v.is_empty());
                let (Some(id), Some(end)) = (t(0), t(1)) else { continue };
                now_seen.insert(id.clone());
                if end > *after {
                    *after = end;
                }
                if seen.contains(&id) {
                    continue;
                }
                let body = t(3).unwrap_or_default();
                if body.trim().is_empty() {
                    continue;
                }
                out.push(ProfiledStatement {
                    time: t(2).unwrap_or_default(),
                    duration_ms: r.get(4).and_then(num).map(|us| us / 1000.0),
                    text: body,
                    database: t(5),
                    user: t(6),
                    client: t(7),
                    rows: r.get(8).and_then(int).filter(|v| *v >= 0).map(|v| v as u64),
                    error: t(11).filter(|c| c != "0").map(|c| match t(12) {
                        Some(m) => format!("{c}: {m}"),
                        None => c,
                    }),
                    detail: t(10),
                    application: t(13),
                    cpu_ms: r.get(9).and_then(num).and_then(cpu_ms),
                    ..Default::default()
                });
            }
            *seen = now_seen;
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
    }
}

pub(crate) async fn stop(s: &HanaSession, state: State) -> Result<()> {
    if let State::Trace { restore, .. } = state {
        for sql in restore.iter().rev() {
            s.conn.exec(sql.clone()).await.map_err(err)?;
        }
    }
    Ok(())
}

/// Statements that ended since `after` (less the lookback), oldest first:
/// id, end (local), start (UTC), text, µs, schema, user, client, rows, cpu
/// µs, operation, error code, error text, application.
fn trace_sql(after: &str, scope: &str) -> String {
    format!(
        "SELECT TOP {BATCH} TO_VARCHAR(e.CONNECTION_ID) || '|' || TO_VARCHAR(e.STATEMENT_ID) || '|' || TO_VARCHAR(e.START_TIME, 'YYYYMMDDHH24MISSFF7'),
                TO_VARCHAR(ADD_NANO100(e.START_TIME, e.DURATION_MICROSEC * 10), 'YYYY-MM-DD HH24:MI:SS.FF7'),
                {start}, TO_NVARCHAR(SUBSTR(e.STATEMENT_STRING, 1, 5000)), e.DURATION_MICROSEC,
                c.CURRENT_SCHEMA_NAME, COALESCE(NULLIF(e.APP_USER, '') || ' (' || e.DB_USER || ')', e.DB_USER), c.CLIENT_HOST,
                e.RECORDS, e.CPU_TIME, e.OPERATION, TO_VARCHAR(e.ERROR_CODE), e.ERROR_TEXT,
                (SELECT TOP 1 x.VALUE FROM SYS.M_SESSION_CONTEXT x WHERE x.HOST = c.HOST AND x.PORT = c.PORT AND x.CONNECTION_ID = c.CONNECTION_ID AND x.KEY = 'APPLICATION')
           FROM SYS.M_EXPENSIVE_STATEMENTS e
           LEFT JOIN SYS.M_CONNECTIONS c ON c.CONNECTION_ID = e.CONNECTION_ID AND c.HOST = e.HOST AND c.PORT = e.PORT
          WHERE e.CONNECTION_ID <> CURRENT_CONNECTION
            AND ADD_NANO100(e.START_TIME, e.DURATION_MICROSEC * 10) >= ADD_SECONDS(TO_TIMESTAMP('{after}'), -{LOOKBACK_S}){scope}
          ORDER BY 2",
        start = utc("e.START_TIME"),
        after = quote(after),
    )
}

/// `CPU_TIME` (µs) in ms; 0 or less where the server doesn't measure CPU
/// time (`resource_tracking` › `cpu_time_measurement_mode` off).
fn cpu_ms(us: f64) -> Option<f64> {
    (us > 0.0).then_some(us / 1000.0)
}

/// The statements running now, except this connection's: key, start,
/// text, ms, schema, user, client, application.
fn sample_sql(schema: &str) -> String {
    format!(
        "SELECT TO_VARCHAR(a.CONNECTION_ID) || '|' || TO_VARCHAR(a.STATEMENT_ID), {start},
                TO_NVARCHAR(SUBSTR(a.STATEMENT_STRING, 1, 5000)),
                NANO100_BETWEEN(a.LAST_EXECUTED_TIME, CURRENT_TIMESTAMP) / 10000.0,
                c.CURRENT_SCHEMA_NAME, c.USER_NAME, c.CLIENT_HOST,
                (SELECT TOP 1 x.VALUE FROM SYS.M_SESSION_CONTEXT x WHERE x.HOST = c.HOST AND x.PORT = c.PORT AND x.CONNECTION_ID = c.CONNECTION_ID AND x.KEY = 'APPLICATION')
           FROM SYS.M_ACTIVE_STATEMENTS a
           LEFT JOIN SYS.M_CONNECTIONS c ON c.CONNECTION_ID = a.CONNECTION_ID AND c.HOST = a.HOST AND c.PORT = a.PORT
          WHERE a.STATEMENT_STATUS = 'ACTIVE' AND a.CONNECTION_ID <> CURRENT_CONNECTION
            AND a.LAST_EXECUTED_TIME IS NOT NULL{}",
        scope(schema),
        start = utc("a.LAST_EXECUTED_TIME"),
    )
}

fn sample(r: &[HdbValue<'static>]) -> Sample {
    let t = |i: usize| r.get(i).and_then(text).filter(|v| !v.is_empty());
    Sample {
        session: t(0).unwrap_or_default(),
        started: t(1).unwrap_or_default(),
        text: t(2).unwrap_or_default(),
        running: true,
        duration_ms: r.get(3).and_then(num).map(|v| v.max(0.0)),
        database: t(4),
        user: t(5),
        client: t(6),
        application: t(7),
        ..Default::default()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trace_reads_back_the_lookback_in_the_schema() {
        let sql = trace_sql("2024-01-31 10:00:00.0000000", &scope("VENTAS"));
        assert!(sql.contains("ADD_SECONDS(TO_TIMESTAMP('2024-01-31 10:00:00.0000000'), -5)"), "{sql}");
        assert!(sql.contains("c.CURRENT_SCHEMA_NAME = 'VENTAS'"));
        assert!(!sample_sql("").contains("CURRENT_SCHEMA_NAME ="));
        assert!(sql.contains("e.CPU_TIME"));
    }

    #[test]
    fn cpu_time_in_ms_only_when_measured() {
        assert_eq!(cpu_ms(2500.0), Some(2.5));
        assert_eq!(cpu_ms(0.0), None);
        assert_eq!(cpu_ms(-1.0), None);
    }
}
