//! The profiler ([`dbine_driver::profiler`]) per variant.
//!
//! - PostgreSQL and the engines that keep its `pg_stat_activity` (managed
//!   services, EDB, Fujitsu, KingbaseES, openGauss, TimescaleDB,
//!   YugabyteDB, Greenplum and its forks): sampled. The view shows each
//!   connection's current or last statement with its start and end; there is
//!   no per-statement history without the server's log.
//! - CockroachDB: `crdb_internal.cluster_queries` (running statements),
//!   sampled.
//! - H2: `INFORMATION_SCHEMA.SESSIONS` (the executing statement), sampled.
//! - Redshift (`sys_query_history`), Yellowbrick (`sys.log_query`), CrateDB
//!   (`sys.jobs_log`) and Materialize (`mz_internal.mz_recent_activity_log`)
//!   keep a history of finished statements: complete.
//! - RisingWave and Denodo: no view of other clients' statements.
//!
//! Only Materialize may need a server setting: its statement log samples
//! `statement_logging_default_sample_rate` of the statements (often 0).
//! When allowed, the profiler raises it to 1 (only the `mz_system` role can)
//! and puts it back at stop.

use crate::catalog::cell;
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::profiler::{Sample, Sampler, SAMPLE_EVERY, SAMPLE_FOR};
use dbine_driver::{Error, ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted, Result};
use std::collections::HashSet;
use std::time::{Duration, Instant};
use tokio_postgres::SimpleQueryRow;

/// Longest a profiler query may run before it's cancelled.
const LIMIT: Duration = Duration::from_secs(5);
/// Rows read from a history per poll.
const BATCH: usize = 1000;

pub(crate) fn supported(v: Variant) -> bool {
    !matches!(v, Variant::RisingWave | Variant::Denodo)
}

pub(crate) enum State {
    Sampled(Sampler),
    /// A history read past `after` (its time column, text); `ids` are the
    /// rows already read at exactly `after`.
    History { after: String, ids: HashSet<String>, restore: Vec<String> },
}

/// `now()` on the server, in the statements' time format (UTC).
const NOW_UTC: &str = "to_char(now() AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.MS')";

fn utc(col: &str) -> String {
    format!("to_char(({col}) AT TIME ZONE 'UTC', 'YYYY-MM-DD HH24:MI:SS.MS')")
}

pub(crate) async fn start(s: &PgSession, opts: &ProfilerOptions) -> Result<(State, ProfilerStarted)> {
    let v = s.variant;
    if !supported(v) {
        return Err(Error::Unsupported("este motor no muestra las consultas de otros clientes".into()));
    }
    let now = match v {
        // CrateDB and H2 have no `AT TIME ZONE` on now().
        Variant::CrateDb => "SELECT date_format('%Y-%m-%d %H:%i:%S.%f', 'UTC', now()) AS t".to_string(),
        Variant::H2 => "SELECT FORMATDATETIME(CURRENT_TIMESTAMP, 'yyyy-MM-dd HH:mm:ss.SSS', 'en', 'UTC') AS t".to_string(),
        _ => format!("SELECT {NOW_UTC} AS t"),
    };
    let since = s
        .text_within(&now, LIMIT)
        .await?
        .first()
        .and_then(|r| cell(r, "t"))
        .map(|t| trim_micros(&t))
        .unwrap_or_default();
    let history = |source: &str| {
        (
            State::History { after: since.clone(), ids: HashSet::new(), restore: Vec::new() },
            ProfilerStarted::new(ProfilerMode::Complete, source),
        )
    };
    Ok(match v {
        Variant::Redshift => history("sys_query_history"),
        Variant::Yellowbrick => history("sys.log_query"),
        Variant::CrateDb => {
            let (state, started) = history("sys.jobs_log");
            (state, started.note("CrateDB guarda las últimas 10.000 consultas por nodo (stats.jobs_log_size)."))
        }
        Variant::Materialize => {
            let (mut state, started) = history("mz_internal.mz_recent_activity_log");
            let restore = match &mut state {
                State::History { restore, .. } => restore,
                State::Sampled(_) => unreachable!(),
            };
            let started = materialize_logging(s, opts, started, restore).await?;
            (state, started)
        }
        Variant::Cockroach => {
            let mut started = ProfilerStarted::new(ProfilerMode::Sampled, "crdb_internal.cluster_queries")
                .note("CockroachDB solo muestra las consultas en curso de todo el clúster.");
            // Without admin or VIEWACTIVITY the view silently holds only the
            // user's own statements. The role option of the same name can't
            // be read without admin; before 22.2 the check fails: no note.
            let all = s
                .text_within(
                    "SELECT (pg_has_role('admin', 'MEMBER') OR has_system_privilege('VIEWACTIVITY') \
                             OR has_system_privilege('VIEWACTIVITYREDACTED'))::text AS all",
                    LIMIT,
                )
                .await
                .ok()
                .and_then(|r| r.first().and_then(|r| cell(r, "all")));
            if all.as_deref() == Some("false") {
                started = started.note(
                    "CockroachDB solo muestra las consultas en curso. Este usuario no es admin ni tiene el privilegio \
                     de sistema VIEWACTIVITY: salvo que tenga la opción \
                     de rol VIEWACTIVITY, solo se ven sus propias consultas (GRANT SYSTEM VIEWACTIVITY TO <usuario>).",
                );
            }
            (State::Sampled(Sampler::new(since)), started)
        }
        Variant::H2 => (State::Sampled(Sampler::new(since)), ProfilerStarted::new(ProfilerMode::Sampled, "INFORMATION_SCHEMA.SESSIONS")),
        _ => {
            let mut started = ProfilerStarted::new(ProfilerMode::Sampled, "pg_stat_activity");
            // Without superuser or pg_read_all_stats, other users' statements
            // read "<insufficient privilege>".
            let all = s
                .text_within(
                    "SELECT (usesuper OR pg_has_role(current_user, 'pg_read_all_stats', 'member'))::text AS all \
                     FROM pg_user WHERE usename = current_user",
                    LIMIT,
                )
                .await
                .ok()
                .and_then(|r| r.first().and_then(|r| cell(r, "all")));
            if all.as_deref() == Some("false") {
                started = started.note(
                    "Este usuario no es superusuario ni tiene pg_read_all_stats: solo se ven sus propias consultas.",
                );
            }
            (State::Sampled(Sampler::new(since)), started)
        }
    })
}

pub(crate) async fn poll(s: &PgSession, state: &mut State) -> Result<Vec<ProfiledStatement>> {
    match state {
        State::Sampled(sampler) => {
            let mut out = Vec::new();
            let until = Instant::now() + SAMPLE_FOR;
            loop {
                let rows = match s.text_within(&sample_sql(s), LIMIT).await {
                    Err(Error::Query(e)) if s.variant == Variant::Cockroach => cockroach_fallback(s, &e).await?,
                    r => r?,
                };
                out.extend(sampler.feed(rows.iter().map(|r| sample(s.variant, r)).collect()));
                if Instant::now() + SAMPLE_EVERY > until {
                    break;
                }
                tokio::time::sleep(SAMPLE_EVERY).await;
            }
            out.sort_by(|a, b| a.time.cmp(&b.time));
            Ok(out)
        }
        State::History { after, ids, .. } => {
            let sql = history_sql(s.variant, after);
            let rows = s.text_within(&sql, LIMIT).await?;
            let mut out = Vec::new();
            for r in &rows {
                let (Some(time), Some(id)) = (cell(r, "time").map(|t| trim_micros(&t)), cell(r, "id")) else { continue };
                if time == *after && ids.contains(&id) {
                    continue;
                }
                if time != *after {
                    *after = time.clone();
                    ids.clear();
                }
                ids.insert(id);
                let text = cell(r, "text").unwrap_or_default();
                if text.trim().is_empty() {
                    continue;
                }
                out.push(ProfiledStatement {
                    time,
                    duration_ms: cell(r, "ms").and_then(|v| v.parse().ok()),
                    text,
                    database: cell(r, "db"),
                    user: cell(r, "usr"),
                    client: cell(r, "client").filter(|c| !c.is_empty()),
                    rows: cell(r, "rows").and_then(|v| v.parse().ok()),
                    error: cell(r, "error").filter(|e| !e.is_empty()),
                    detail: None,
                    application: cell(r, "app").filter(|a| !a.is_empty()),
                    ..Default::default()
                });
            }
            Ok(out)
        }
    }
}

/// Put back what `start` changed.
pub(crate) async fn stop(s: &PgSession, state: State) -> Result<()> {
    if let State::History { restore, .. } = state {
        for sql in restore {
            s.text_within(&sql, LIMIT).await?;
        }
    }
    Ok(())
}

/// Materialize logs a sample of the statements
/// (`statement_logging_default_sample_rate`, capped by
/// `statement_logging_max_sample_rate`): all of them while profiling, when
/// the user may change it.
async fn materialize_logging(
    s: &PgSession,
    opts: &ProfilerOptions,
    mut started: ProfilerStarted,
    restore: &mut Vec<String>,
) -> Result<ProfilerStarted> {
    const NAMES: [&str; 2] = ["statement_logging_max_sample_rate", "statement_logging_default_sample_rate"];
    let mut rates = Vec::new();
    for name in NAMES {
        let rows = s.text_within(&format!("SHOW {name}"), LIMIT).await?;
        rates.push(rows.first().and_then(|r| cell(r, name)).and_then(|v| v.parse::<f64>().ok()).unwrap_or(0.0));
    }
    let rate = rates[0].min(rates[1]);
    if rate >= 1.0 {
        return Ok(started);
    }
    if !opts.change_server {
        if rate == 0.0 {
            return Err(Error::Query(
                "Materialize no está registrando consultas (statement logging en 0) y la conexión es de solo lectura".into(),
            ));
        }
        return Ok(started.note(format!("Materialize registra una muestra del {} % de las consultas.", rate * 100.0)));
    }
    for (name, was) in NAMES.into_iter().zip(rates) {
        if was >= 1.0 {
            continue;
        }
        if let Err(e) = s.text_within(&format!("ALTER SYSTEM SET {name} = 1"), LIMIT).await {
            for sql in restore.drain(..) {
                let _ = s.text_within(&sql, LIMIT).await;
            }
            let now = if rate == 0.0 { "no registra consultas".to_string() } else { format!("registra solo el {} % de las consultas", rate * 100.0) };
            return Err(Error::Query(format!(
                "Materialize {now} y solo el rol mz_system puede activar el registro completo ({e})"
            )));
        }
        restore.push(format!("ALTER SYSTEM SET {name} = {was}"));
        started = started.change(format!("{name} = 1 (estaba en {was})"));
    }
    Ok(started.note("Las sesiones abiertas antes de iniciar conservan el muestreo que tenían."))
}

/// CockroachDB may close `crdb_internal` to non-admin users (v25+ gate,
/// `allow_unsafe_internals`); `SHOW CLUSTER STATEMENTS` is the supported
/// equivalent, with the same columns and the same VIEWACTIVITY rules.
async fn cockroach_fallback(s: &PgSession, first: &str) -> Result<Vec<SimpleQueryRow>> {
    let sql = sample_sql(s).replace("crdb_internal.cluster_queries", "[SHOW CLUSTER STATEMENTS]");
    s.text_within(&sql, LIMIT).await.map_err(|e| {
        Error::Query(format!(
            "CockroachDB no dejó leer las consultas en curso ({first}; {e}). Hace falta ser admin o tener el privilegio \
             VIEWACTIVITY: GRANT SYSTEM VIEWACTIVITY TO <usuario>"
        ))
    })
}

/// What each connection runs now (or last ran), except this one.
fn sample_sql(s: &PgSession) -> String {
    match s.variant {
        Variant::Cockroach => format!(
            "SELECT session_id AS session, {} AS started, query AS text, 'true' AS running, \
                    (EXTRACT(EPOCH FROM (now() - start)) * 1000)::FLOAT8::TEXT AS ms, \
                    NULL AS db, user_name AS usr, \
                    NULLIF(application_name, '') AS app, client_address AS client \
             FROM crdb_internal.cluster_queries \
             WHERE session_id <> (SELECT session_id FROM [SHOW session_id])",
            utc("start")
        ),
        Variant::H2 => "SELECT CAST(SESSION_ID AS VARCHAR) AS session, \
                    FORMATDATETIME(EXECUTING_STATEMENT_START, 'yyyy-MM-dd HH:mm:ss.SSS', 'en', 'UTC') AS started, \
                    EXECUTING_STATEMENT AS text, 'true' AS running, \
                    CAST(DATEDIFF('MILLISECOND', EXECUTING_STATEMENT_START, CURRENT_TIMESTAMP) AS VARCHAR) AS ms, \
                    NULL AS db, USER_NAME AS usr, NULL AS client \
             FROM INFORMATION_SCHEMA.SESSIONS \
             WHERE SESSION_ID <> SESSION_ID() AND EXECUTING_STATEMENT IS NOT NULL"
            .to_string(),
        _ => format!(
            "SELECT pid::text AS session, {} AS started, query AS text, (state = 'active')::text AS running, \
                    (EXTRACT(EPOCH FROM (CASE WHEN state = 'active' THEN clock_timestamp() ELSE state_change END) - query_start) \
                     * 1000)::float8::text AS ms, \
                    datname AS db, usename AS usr, \
                    NULLIF(application_name, '') AS app, COALESCE(host(client_addr), 'local') AS client \
             FROM pg_stat_activity \
             WHERE pid <> pg_backend_pid() AND query_start IS NOT NULL AND datname = current_database()",
            utc("query_start")
        ),
    }
}

fn sample(v: Variant, r: &SimpleQueryRow) -> Sample {
    let _ = v;
    Sample {
        session: cell(r, "session").unwrap_or_default(),
        started: cell(r, "started").map(|t| trim_micros(&t)).unwrap_or_default(),
        text: cell(r, "text").unwrap_or_default(),
        running: cell(r, "running").as_deref() == Some("true"),
        duration_ms: cell(r, "ms").and_then(|v| v.parse::<f64>().ok()).map(|ms| ms.max(0.0)),
        database: cell(r, "db"),
        user: cell(r, "usr"),
        client: cell(r, "client").filter(|c| !c.is_empty()),
        application: cell(r, "app").filter(|a| !a.is_empty()),
        ..Default::default()
    }
}

/// Finished statements after `after` (inclusive: ties are filtered by id),
/// oldest first.
fn history_sql(v: Variant, after: &str) -> String {
    let after = after.replace('\'', "''");
    match v {
        Variant::Redshift => format!(
            "SELECT query_id::text AS id, {} AS time, query_text AS text, (elapsed_time / 1000.0)::text AS ms, \
                    database_name AS db, (SELECT usename FROM pg_user WHERE usesysid = user_id) AS usr, NULL AS client, \
                    returned_rows::text AS rows, error_message AS error \
             FROM sys_query_history \
             WHERE start_time >= '{after}'::timestamp AND database_name = current_database() \
               AND session_id <> pg_backend_pid() AND status IN ('success', 'failed', 'canceled') \
             ORDER BY start_time LIMIT {BATCH}",
            utc("start_time")
        ),
        Variant::Yellowbrick => format!(
            "SELECT query_id::text AS id, {} AS time, query_text AS text, (total_ms)::text AS ms, \
                    database_name AS db, username AS usr, application_name AS app, NULL AS client, \
                    rows_returned::text AS rows, error_message AS error \
             FROM sys.log_query \
             WHERE submit_time >= '{after}'::timestamp AND database_name = current_database() \
               AND session_id <> pg_backend_pid() \
             ORDER BY submit_time LIMIT {BATCH}",
            utc("submit_time")
        ),
        Variant::CrateDb => format!(
            "SELECT id::TEXT AS id, date_format('%Y-%m-%d %H:%i:%S.%f', 'UTC', started) AS time, stmt AS text, \
                    (ended::BIGINT - started::BIGINT)::TEXT AS ms, NULL AS db, username AS usr, node['name'] AS client, \
                    NULL AS rows, error \
             FROM sys.jobs_log \
             WHERE started >= '{after}'::TIMESTAMP AND stmt NOT LIKE '%sys.jobs_log%' \
             ORDER BY started LIMIT {BATCH}"
        ),
        Variant::Materialize => format!(
            "SELECT execution_id::text AS id, {} AS time, sql AS text, \
                    (EXTRACT(EPOCH FROM (finished_at - began_at)) * 1000)::text AS ms, \
                    database_name AS db, authenticated_user AS usr, application_name AS app, NULL AS client, \
                    rows_returned::text AS rows, error_message AS error \
             FROM mz_internal.mz_recent_activity_log \
             WHERE began_at >= '{after}'::timestamp AND finished_at IS NOT NULL \
               AND session_id <> (SELECT id FROM mz_internal.mz_sessions WHERE connection_id = pg_backend_pid()) \
             ORDER BY began_at LIMIT {BATCH}",
            utc("began_at")
        ),
        _ => String::new(),
    }
}

/// Times at millisecond precision (some engines give microseconds).
fn trim_micros(t: &str) -> String {
    match t.find('.') {
        Some(i) if t.len() > i + 4 => t[..i + 4].to_string(),
        _ => t.to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn times_keep_milliseconds() {
        assert_eq!(trim_micros("2024-01-31 10:00:00.123456"), "2024-01-31 10:00:00.123");
        assert_eq!(trim_micros("2024-01-31 10:00:00.1"), "2024-01-31 10:00:00.1");
        assert_eq!(trim_micros("2024-01-31 10:00:00"), "2024-01-31 10:00:00");
    }
}
