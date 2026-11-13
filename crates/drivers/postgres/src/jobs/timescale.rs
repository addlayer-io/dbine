//! TimescaleDB's jobs: user-defined actions (`add_job`) and its policies
//! (compression, retention, continuous aggregate refresh, reorder), from
//! `timescaledb_information.jobs` and `job_stats`. Past runs come from
//! `job_history` (2.15+: every run with
//! `timescaledb.enable_job_execution_logging` on, else only the failures)
//! or `job_errors` (2.10 to 2.14), plus the last run `job_stats` keeps.
//! With logging on, a run's row is written when it starts, without an
//! outcome: it's the run going on while its process is there, a crash
//! once TimescaleDB clears the process (`job_history` says so).
//!
//! "Run now" moves the job's next start to now (`alter_job(next_start =>
//! now())`): the scheduler starts it within a moment and the call doesn't
//! wait for it, the way `CALL run_job()` would. Enabling is
//! `alter_job(scheduled => …)`; TimescaleDB has no way to stop a run.

use super::{
    flag, ident, message, ms, not_found, prop, text, time, yes_no, Found, JobId, Part, Row,
};
use crate::catalog::{comment_text, lit};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{Error, JobDetail, JobOutcome, JobRun, JobStep, Result, ServerJob};
use tokio_postgres::SimpleQueryRow;

/// Jobs below this id are the extension's own (telemetry, history
/// retention): it creates them, so they have no script.
const FIRST_USER_JOB: i32 = 1000;

/// The policies DBine can write the `add_*_policy` call of.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Policy {
    Compression,
    Retention,
    Refresh,
    Reorder,
    /// Merges small compressed chunks (2.2x+).
    Compaction,
}

impl Policy {
    fn of(schema: Option<&str>, proc: Option<&str>) -> Option<Policy> {
        if !internal(schema) {
            return None;
        }
        match proc? {
            "policy_compression" => Some(Policy::Compression),
            "policy_retention" => Some(Policy::Retention),
            "policy_refresh_continuous_aggregate" => Some(Policy::Refresh),
            "policy_reorder" => Some(Policy::Reorder),
            "policy_compaction" => Some(Policy::Compaction),
            _ => None,
        }
    }

    fn kind(self) -> &'static str {
        match self {
            Policy::Compression => "TimescaleDB · compresión",
            Policy::Retention => "TimescaleDB · retención",
            Policy::Refresh => "TimescaleDB · agregado continuo",
            Policy::Reorder => "TimescaleDB · reordenamiento",
            Policy::Compaction => "TimescaleDB · compactación",
        }
    }

    /// The function that creates it and the `config` keys it takes as
    /// arguments of the same name.
    fn call(self) -> (&'static str, &'static [&'static str]) {
        match self {
            Policy::Compression => (
                "add_compression_policy",
                &["compress_after", "compress_created_before"],
            ),
            Policy::Retention => (
                "add_retention_policy",
                &["drop_after", "drop_created_before"],
            ),
            Policy::Refresh => (
                "add_continuous_aggregate_policy",
                &[
                    "start_offset",
                    "end_offset",
                    "buckets_per_batch",
                    "max_batches_per_execution",
                    "refresh_newest_first",
                    "include_tiered_data",
                ],
            ),
            Policy::Reorder => ("add_reorder_policy", &["index_name"]),
            Policy::Compaction => (
                "add_compaction_policy",
                &["max_chunks", "max_batches", "inactive_for"],
            ),
        }
    }
}

/// The extension's own schemas (`_timescaledb_functions`,
/// `_timescaledb_internal`).
fn internal(schema: Option<&str>) -> bool {
    schema.is_some_and(|s| s.starts_with("_timescaledb"))
}

fn kind(id: i32, schema: Option<&str>, proc: Option<&str>) -> &'static str {
    if let Some(p) = Policy::of(schema, proc) {
        return p.kind();
    }
    if id < FIRST_USER_JOB {
        "TimescaleDB · sistema"
    } else if internal(schema) {
        "TimescaleDB · política"
    } else {
        "TimescaleDB"
    }
}

fn jobs_sql(id: Option<i32>) -> String {
    let filter = id
        .map(|n| format!("WHERE j.job_id = {n}"))
        .unwrap_or_default();
    // `to_jsonb(j)` reads the columns that only newer versions have
    // (fixed_schedule and initial_start 2.9+, check_* 2.10+).
    format!(
        "SELECT j.job_id::text AS id, j.application_name AS name, j.schedule_interval::text AS every,
                j.max_runtime::text AS max_runtime, j.max_retries::text AS max_retries,
                j.retry_period::text AS retry_period, j.proc_schema, j.proc_name, j.owner::text AS owner,
                j.scheduled, j.config::text AS config, extract(epoch FROM j.next_start)::text AS next_start,
                j.hypertable_schema, j.hypertable_name, ca.view_schema, ca.view_name,
                to_jsonb(j) ->> 'fixed_schedule' AS fixed_schedule,
                to_jsonb(j) ->> 'initial_start' AS initial_start_text,
                extract(epoch FROM (to_jsonb(j) ->> 'initial_start')::timestamptz)::text AS initial_start,
                to_jsonb(j) ->> 'check_schema' AS check_schema, to_jsonb(j) ->> 'check_name' AS check_name,
                s.last_run_status, s.job_status,
                extract(epoch FROM s.last_run_started_at)::text AS last_started,
                extract(epoch FROM s.last_successful_finish)::text AS last_success,
                (extract(epoch FROM s.last_run_duration) * 1000)::text AS last_ms,
                s.total_runs::text AS total_runs, s.total_successes::text AS total_successes,
                s.total_failures::text AS total_failures,
                pg_has_role(j.owner::oid, 'USAGE') AS mine
         FROM timescaledb_information.jobs j
         LEFT JOIN timescaledb_information.job_stats s ON s.job_id = j.job_id
         LEFT JOIN timescaledb_information.continuous_aggregates ca
                ON ca.materialization_hypertable_schema = j.hypertable_schema
               AND ca.materialization_hypertable_name = j.hypertable_name
         {filter}
         ORDER BY j.job_id"
    )
}

/// What the job works on: the continuous aggregate, or the hypertable.
fn target(r: &dyn Row) -> Option<String> {
    match (text(r, "view_schema"), text(r, "view_name")) {
        (Some(s), Some(v)) => Some(format!("{s}.{v}")),
        _ => match (text(r, "hypertable_schema"), text(r, "hypertable_name")) {
            (Some(s), Some(h)) => Some(format!("{s}.{h}")),
            _ => None,
        },
    }
}

fn status_outcome(status: Option<&str>) -> Option<JobOutcome> {
    match status {
        Some("Success") => Some(JobOutcome::Succeeded),
        Some("Failed") => Some(JobOutcome::Failed),
        _ => None,
    }
}

fn job(r: &dyn Row) -> Option<ServerJob> {
    let n: i32 = text(r, "id")?.parse().ok()?;
    let schema = text(r, "proc_schema");
    let proc = text(r, "proc_name");
    let policy = Policy::of(schema.as_deref(), proc.as_deref());
    let system = n < FIRST_USER_JOB;
    let mine = flag(r, "mine");
    let scheduled = flag(r, "scheduled");
    let running = text(r, "job_status").as_deref() == Some("Running");
    let group = if policy.is_some() || internal(schema.as_deref()) {
        target(r)
    } else if system {
        None
    } else {
        schema.clone()
    };
    Some(ServerJob {
        id: JobId::Timescale(n).to_string(),
        name: text(r, "name").unwrap_or_else(|| format!("job {n}")),
        kind: kind(n, schema.as_deref(), proc.as_deref()).into(),
        group,
        description: None,
        owner: text(r, "owner"),
        enabled: scheduled,
        schedule: text(r, "every").map(|e| format!("INTERVAL '{e}'")),
        running,
        last_outcome: status_outcome(text(r, "last_run_status").as_deref())
            .or(running.then_some(JobOutcome::Running)),
        last_started: time(r, "last_started"),
        last_duration_ms: ms(r, "last_ms"),
        next_run: time(r, "next_start").filter(|_| scheduled),
        // `alter_job` takes the job's owner (or whoever has its rights).
        can_run: scheduled && mine && !running,
        can_stop: false,
        can_enable: mine,
        has_history: true,
        has_script: !system && (policy.is_some() || !internal(schema.as_deref())),
        system,
    })
}

pub(super) async fn list(s: &PgSession, _schema: &str) -> Result<Part> {
    let rows = s.text(&jobs_sql(None)).await?;
    Ok(Part {
        jobs: rows.iter().filter_map(|r| job(r)).collect(),
        notes: Vec::new(),
    })
}

/// What's installed here, refused when TimescaleDB isn't.
async fn found(s: &PgSession) -> Result<Found> {
    let f = Found::detect(s).await?;
    if f.timescale.is_none() {
        return Err(Error::Query(
            "TimescaleDB no está instalado en esta base".into(),
        ));
    }
    Ok(f)
}

/// TimescaleDB's schema in this database.
async fn schema(s: &PgSession) -> Result<String> {
    Ok(found(s).await?.timescale.unwrap_or_default())
}

async fn row(s: &PgSession, n: i32) -> Result<SimpleQueryRow> {
    s.text(&jobs_sql(Some(n)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(JobId::Timescale(n)))
}

/// How a routine's language reads in the step.
fn language(lanname: &str) -> String {
    match lanname {
        "plpgsql" => "PL/pgSQL".into(),
        "sql" => "SQL".into(),
        "c" | "internal" => "C".into(),
        other => other.into(),
    }
}

fn properties(r: &dyn Row) -> Vec<dbine_driver::PropertyInfo> {
    let mut out = Vec::new();
    let mut add = |group: &str, label: &str, value: Option<String>| {
        if let Some(v) = value {
            out.push(prop(group, label, v));
        }
    };
    add("", "Id", text(r, "id"));
    add("", "Propietario", text(r, "owner"));
    add("", "Intervalo", text(r, "every"));
    add(
        "",
        "Horario fijo",
        text(r, "fixed_schedule").map(|f| yes_no(f == "true").into()),
    );
    add("", "Primer inicio", time(r, "initial_start"));
    add(
        "",
        "Duración máxima",
        text(r, "max_runtime").map(|m| {
            if m == "00:00:00" {
                "sin límite".into()
            } else {
                m
            }
        }),
    );
    add(
        "",
        "Reintentos",
        text(r, "max_retries").map(|m| if m == "-1" { "sin límite".into() } else { m }),
    );
    add("", "Espera entre reintentos", text(r, "retry_period"));
    add("", "Configuración", text(r, "config"));
    add(
        "",
        "Verificación",
        match (text(r, "check_schema"), text(r, "check_name")) {
            (Some(s), Some(n)) => Some(format!("{s}.{n}")),
            _ => None,
        },
    );
    add("Estadísticas", "Ejecuciones", text(r, "total_runs"));
    add("Estadísticas", "Correctas", text(r, "total_successes"));
    add("Estadísticas", "Fallidas", text(r, "total_failures"));
    add(
        "Estadísticas",
        "Último fin correcto",
        time(r, "last_success"),
    );
    out
}

pub(super) async fn detail(s: &PgSession, n: i32) -> Result<JobDetail> {
    let f = found(s).await?;
    let r = row(s, n).await?;
    let mut job = job(&r).ok_or_else(|| not_found(JobId::Timescale(n)))?;
    // The scheduler starts nothing (the list says why).
    job.can_run &= f.timescale_off().is_none();
    let proc_schema = text(&r, "proc_schema").unwrap_or_default();
    let proc = text(&r, "proc_name").unwrap_or_default();
    let routine = s
        .text(&format!(
            "SELECT p.prokind::text AS prokind, l.lanname
             FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             JOIN pg_catalog.pg_language l ON l.oid = p.prolang
             WHERE n.nspname = {} AND p.proname = {} LIMIT 1",
            lit(s.variant, &proc_schema),
            lit(s.variant, &proc)
        ))
        .await
        .ok()
        .and_then(|rows| rows.into_iter().next());
    let procedure = routine
        .as_ref()
        .is_none_or(|p| text(p, "prokind").as_deref() == Some("p"));
    let config = text(&r, "config")
        .map(|c| format!("{}::jsonb", lit(s.variant, &c)))
        .unwrap_or_else(|| "NULL".into());
    let call = format!(
        "{} {}.{}({n}, {config})",
        if procedure { "CALL" } else { "SELECT" },
        ident(&proc_schema),
        ident(&proc)
    );
    let policy = Policy::of(Some(&proc_schema), Some(&proc));
    let step_kind = if policy.is_some() || internal(Some(&proc_schema)) {
        Some("Política".into())
    } else {
        routine
            .as_ref()
            .and_then(|p| text(p, "lanname"))
            .map(|l| language(&l))
    };
    Ok(JobDetail {
        steps: vec![JobStep {
            name: format!("{proc_schema}.{proc}"),
            kind: step_kind,
            target: target(&r),
            command: Some(call),
        }],
        schedules: Vec::new(),
        properties: properties(&r),
        job,
        notes: Vec::new(),
    })
}

/// `job_history` (2.15+) or `job_errors` (2.10 to 2.14), whichever this
/// version has; `None` before 2.10.
async fn history_sql(s: &PgSession, n: i32, limit: u32) -> Result<Option<String>> {
    let rows = s
        .text(
            "SELECT to_regclass('timescaledb_information.job_history') IS NOT NULL AS history,
                    to_regclass('timescaledb_information.job_errors') IS NOT NULL AS errors",
        )
        .await?;
    let r = rows.first();
    let has = |name: &str| r.is_some_and(|r| flag(r, name));
    Ok(if has("history") {
        Some(history_view_sql(n, limit))
    } else if has("errors") {
        Some(format!(
            "SELECT NULL::text AS id, false AS succeeded, false AS open, false AS nopid,
                    extract(epoch FROM start_time)::text AS started,
                    (extract(epoch FROM finish_time - start_time) * 1000)::text AS ms, sqlerrcode, err_message
             FROM timescaledb_information.job_errors WHERE job_id = {n}
             ORDER BY start_time DESC LIMIT {limit}"
        ))
    } else {
        None
    })
}

/// `job_history`, with what tells a run going on from a crash when
/// `succeeded` is null: no `finish_time`, and whether the process is still
/// there (`pid`, read through `to_jsonb` in case a version lacks it).
fn history_view_sql(n: i32, limit: u32) -> String {
    format!(
        "SELECT h.id::text AS id, h.succeeded, h.finish_time IS NULL AS open,
                (to_jsonb(h) ->> 'pid') IS NULL AS nopid,
                extract(epoch FROM h.start_time)::text AS started,
                (extract(epoch FROM h.finish_time - h.start_time) * 1000)::text AS ms, h.sqlerrcode, h.err_message
         FROM timescaledb_information.job_history h WHERE h.job_id = {n}
         ORDER BY h.start_time DESC, h.id DESC LIMIT {limit}"
    )
}

/// A history row's outcome. `succeeded` is null while the run goes on, and
/// after a crash.
fn history_outcome(r: &dyn Row) -> JobOutcome {
    match r
        .get("succeeded")
        .map(|v| v.trim().to_ascii_lowercase())
        .as_deref()
    {
        Some("t" | "true") => JobOutcome::Succeeded,
        Some("f" | "false") => JobOutcome::Failed,
        _ if flag(r, "open") && flag(r, "nopid") => JobOutcome::Failed,
        _ if flag(r, "open") => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

fn history_run(r: &dyn Row) -> JobRun {
    let message = message(r, "err_message").or_else(|| text(r, "sqlerrcode"));
    let outcome = history_outcome(r);
    JobRun {
        id: text(r, "id"),
        started: time(r, "started"),
        duration_ms: if outcome == JobOutcome::Running {
            None
        } else {
            ms(r, "ms")
        },
        outcome,
        trigger: None,
        message,
        steps: Vec::new(),
    }
}

/// The history plus the run `job_stats` knows of (the one going on, or a
/// success the history didn't log), newest first.
fn merge(mut runs: Vec<JobRun>, stats: &dyn Row, limit: u32) -> Vec<JobRun> {
    if let Some(start) = time(stats, "last_started") {
        let running = text(stats, "job_status").as_deref() == Some("Running");
        // The same run: the same start, or the one going on (the logged
        // start and job_stats' may fall in different seconds).
        let known = runs.iter().any(|r| {
            r.started.as_deref() == Some(start.as_str())
                || (running && r.outcome == JobOutcome::Running)
        });
        if !known {
            runs.push(JobRun {
                id: None,
                started: Some(start),
                duration_ms: if running { None } else { ms(stats, "last_ms") },
                outcome: if running {
                    JobOutcome::Running
                } else {
                    status_outcome(text(stats, "last_run_status").as_deref())
                        .unwrap_or(JobOutcome::Unknown)
                },
                trigger: None,
                message: None,
                steps: Vec::new(),
            });
        }
    }
    runs.sort_by(|a, b| b.started.cmp(&a.started));
    runs.truncate(limit as usize);
    runs
}

pub(super) async fn runs(s: &PgSession, n: i32, limit: u32) -> Result<Vec<JobRun>> {
    schema(s).await?;
    let stats = row(s, n).await?;
    let history = match history_sql(s, n, limit).await? {
        Some(sql) => s.text(&sql).await?.iter().map(|r| history_run(r)).collect(),
        None => Vec::new(),
    };
    Ok(merge(history, &stats, limit))
}

pub(super) async fn run(s: &PgSession, n: i32) -> Result<()> {
    let f = found(s).await?;
    let ts = f.timescale.clone().unwrap_or_default();
    let r = row(s, n).await?;
    if !flag(&r, "scheduled") {
        return Err(Error::Query(
            "el job está deshabilitado: TimescaleDB no lo ejecuta hasta habilitarlo".into(),
        ));
    }
    if let Some(why) = f.timescale_off() {
        return Err(Error::Query(why.trim_end_matches('.').into()));
    }
    s.text(&format!(
        "SELECT job_id FROM {}.alter_job({n}, next_start => now())",
        ident(&ts)
    ))
    .await?;
    Ok(())
}

pub(super) async fn set_enabled(s: &PgSession, n: i32, enabled: bool) -> Result<()> {
    let ts = schema(s).await?;
    row(s, n).await?;
    s.text(&format!(
        "SELECT job_id FROM {}.alter_job({n}, scheduled => {enabled})",
        ident(&ts)
    ))
    .await?;
    Ok(())
}

/// A `config` value as the argument of the same name: intervals as
/// `INTERVAL '…'` (integer-time hypertables keep their numbers).
fn config_arg(v: Variant, key: &str, value: &serde_json::Value) -> String {
    match value {
        serde_json::Value::Null => "NULL".into(),
        serde_json::Value::Bool(b) => b.to_string(),
        serde_json::Value::Number(n) => n.to_string(),
        serde_json::Value::String(s) if key == "index_name" => lit(v, s),
        serde_json::Value::String(s) => format!("INTERVAL {}", lit(v, s)),
        other => format!("{}::jsonb", lit(v, &other.to_string())),
    }
}

/// The call that creates the job again: the policy's `add_*_policy`, or
/// `add_job` for a user-defined action; `None` for the extension's own.
fn script_of(v: Variant, ts: &str, r: &dyn Row, job_name_arg: bool) -> Option<String> {
    let n: i32 = text(r, "id")?.parse().ok()?;
    let proc_schema = text(r, "proc_schema");
    let proc = text(r, "proc_name");
    let policy = Policy::of(proc_schema.as_deref(), proc.as_deref());
    if n < FIRST_USER_JOB || (policy.is_none() && internal(proc_schema.as_deref())) {
        return None;
    }
    let ts = ident(ts);
    let every = text(r, "every").map(|e| format!("INTERVAL {}", lit(v, &e)));
    let fixed = text(r, "fixed_schedule").map(|f| f == "true");
    let initial = text(r, "initial_start_text")
        .filter(|_| fixed == Some(true))
        .map(|t| format!("{}::timestamptz", lit(v, &t)));
    let config_text = text(r, "config");
    let config: Option<serde_json::Value> = config_text
        .as_deref()
        .and_then(|c| serde_json::from_str(c).ok());
    let scheduled = flag(r, "scheduled");
    let name = text(r, "name").unwrap_or_else(|| format!("job {n}"));
    let mut args = Vec::new();
    let call = match policy {
        Some(p) => {
            let (func, keys) = p.call();
            let on = match (text(r, "view_schema"), text(r, "view_name")) {
                (Some(s), Some(view)) => format!("{}.{}", ident(&s), ident(&view)),
                _ => format!(
                    "{}.{}",
                    ident(&text(r, "hypertable_schema").unwrap_or_default()),
                    ident(&text(r, "hypertable_name").unwrap_or_default())
                ),
            };
            args.push(lit(v, &on));
            if let Some(serde_json::Value::Object(cfg)) = &config {
                for key in keys {
                    if let Some(value) = cfg.get(*key) {
                        args.push(format!("{key} => {}", config_arg(v, key, value)));
                    }
                }
            }
            // A reorder policy has no interval of its own.
            if p != Policy::Reorder {
                if let Some(e) = &every {
                    args.push(format!("schedule_interval => {e}"));
                }
            }
            if let Some(i) = &initial {
                args.push(format!("initial_start => {i}"));
            }
            format!("{ts}.{func}({})", args.join(", "))
        }
        None => {
            let routine = format!(
                "{}.{}",
                ident(&proc_schema.unwrap_or_default()),
                ident(&proc.unwrap_or_default())
            );
            args.push(lit(v, &routine));
            args.push(every.unwrap_or_else(|| "NULL".into()));
            if let Some(c) = &config_text {
                args.push(format!("config => {}::jsonb", lit(v, c)));
            }
            if let Some(i) = &initial {
                args.push(format!("initial_start => {i}"));
            }
            if !scheduled {
                args.push("scheduled => false".into());
            }
            if let (Some(s), Some(c)) = (text(r, "check_schema"), text(r, "check_name")) {
                args.push(format!(
                    "check_config => {}",
                    lit(v, &format!("{}.{}", ident(&s), ident(&c)))
                ));
            }
            if let Some(f) = fixed {
                args.push(format!("fixed_schedule => {f}"));
            }
            if job_name_arg {
                // TimescaleDB adds " [<id>]" to the name it's given.
                let given = name.strip_suffix(&format!(" [{n}]")).unwrap_or(&name);
                args.push(format!("job_name => {}", lit(v, given)));
            }
            format!("{ts}.add_job({})", args.join(", "))
        }
    };
    let call = if policy.is_some() && !scheduled {
        format!("{ts}.alter_job({call}, scheduled => false)")
    } else {
        call
    };
    let mut out = format!("-- TimescaleDB: {}\n", comment_text(&name));
    if policy.is_some() {
        if let Some(c) = &config_text {
            out.push_str(&format!("-- Configuración actual: {}\n", comment_text(c)));
        }
    }
    out.push_str(&format!("SELECT {call};\n"));
    Some(out)
}

pub(super) async fn script(s: &PgSession, n: i32) -> Result<String> {
    let ts = schema(s).await?;
    let r = row(s, n).await?;
    // `job_name` is a newer argument of add_job.
    let job_name = s
        .text(&format!(
            "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                            WHERE n.nspname = {} AND p.proname = 'add_job'
                              AND 'job_name' = ANY (p.proargnames)) AS job_name",
            lit(s.variant, &ts)
        ))
        .await?
        .first()
        .is_some_and(|r| flag(r, "job_name"));
    script_of(s.variant, &ts, &r, job_name).ok_or_else(|| {
        Error::Unsupported("TimescaleDB crea este job por su cuenta: no tiene script".into())
    })
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;

    #[test]
    fn kinds_tell_policies_from_actions() {
        let f = Some("_timescaledb_functions");
        assert_eq!(
            kind(1000, f, Some("policy_compression")),
            "TimescaleDB · compresión"
        );
        assert_eq!(
            kind(
                1001,
                Some("_timescaledb_internal"),
                Some("policy_retention")
            ),
            "TimescaleDB · retención"
        );
        assert_eq!(
            kind(1, f, Some("policy_telemetry")),
            "TimescaleDB · sistema"
        );
        assert_eq!(kind(1002, f, Some("policy_new")), "TimescaleDB · política");
        assert_eq!(
            kind(1003, Some("public"), Some("policy_compression")),
            "TimescaleDB"
        );
    }

    fn compression() -> super::super::Fixture {
        fixture(&[
            ("id", "1000"),
            ("name", "Columnstore Policy [1000]"),
            ("every", "12:00:00"),
            ("proc_schema", "_timescaledb_functions"),
            ("proc_name", "policy_compression"),
            ("owner", "postgres"),
            ("scheduled", "t"),
            (
                "config",
                "{\"hypertable_id\": 1, \"compress_after\": \"7 days\"}",
            ),
            ("next_start", "1794528000"),
            ("hypertable_schema", "public"),
            ("hypertable_name", "m1"),
            ("fixed_schedule", "false"),
            ("last_run_status", "Success"),
            ("job_status", "Scheduled"),
            ("last_started", "1794488199"),
            ("last_ms", "32.8"),
            ("mine", "t"),
        ])
    }

    #[test]
    fn a_policy_row_maps_to_a_server_job() {
        let j = job(&compression()).unwrap();
        assert_eq!(j.id, "timescaledb:1000");
        assert_eq!(j.kind, "TimescaleDB · compresión");
        assert_eq!(j.group.as_deref(), Some("public.m1"));
        assert_eq!(j.schedule.as_deref(), Some("INTERVAL '12:00:00'"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(j.last_duration_ms, Some(33));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T00:00:00Z"));
        assert!(j.enabled && j.can_run && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_stop && !j.running);
        assert!(!j.system, "a policy is the user's");
    }

    #[test]
    fn system_jobs_and_other_owners_allow_less() {
        let telemetry = fixture(&[
            ("id", "1"),
            ("proc_schema", "_timescaledb_functions"),
            ("proc_name", "policy_telemetry"),
            ("scheduled", "t"),
            ("next_start", "-Infinity"),
            ("mine", "f"),
        ]);
        let j = job(&telemetry).unwrap();
        assert!(!j.has_script && !j.can_run && !j.can_enable && j.next_run.is_none());
        assert_eq!(j.group, None);
        assert!(j.system, "the extension's own job");
        let paused = fixture(&[
            ("id", "1005"),
            ("proc_schema", "public"),
            ("proc_name", "p"),
            ("scheduled", "f"),
            ("next_start", "1794528000"),
            ("mine", "t"),
            ("job_status", "Running"),
        ]);
        let j = job(&paused).unwrap();
        assert!(!j.enabled && !j.can_run && j.can_enable && j.next_run.is_none());
        assert!(j.running && j.last_outcome == Some(JobOutcome::Running));
        assert_eq!(j.group.as_deref(), Some("public"));
        assert!(!j.system, "a user's action");
    }

    #[test]
    fn the_policy_script_uses_its_add_function() {
        let sql = script_of(Variant::Timescale, "public", &compression(), true).unwrap();
        assert!(
            sql.contains(
                "SELECT \"public\".add_compression_policy(E'\"public\".\"m1\"', compress_after => INTERVAL E'7 days', schedule_interval => INTERVAL E'12:00:00');"
            ),
            "{sql}"
        );
        assert!(sql.contains("-- Configuración actual: {\"hypertable_id\": 1, \"compress_after\""));
    }

    #[test]
    fn a_continuous_aggregate_policy_names_the_view() {
        let r = fixture(&[
            ("id", "1002"),
            ("every", "01:00:00"),
            ("proc_schema", "_timescaledb_functions"),
            ("proc_name", "policy_refresh_continuous_aggregate"),
            ("scheduled", "f"),
            (
                "config",
                "{\"end_offset\": \"01:00:00\", \"start_offset\": null, \"mat_hypertable_id\": 3}",
            ),
            ("hypertable_schema", "_timescaledb_internal"),
            ("hypertable_name", "_materialized_hypertable_3"),
            ("view_schema", "public"),
            ("view_name", "daily"),
        ]);
        assert_eq!(job(&r).unwrap().group.as_deref(), Some("public.daily"));
        let sql = script_of(Variant::Timescale, "ext", &r, false).unwrap();
        assert!(sql.contains("\"ext\".alter_job(\"ext\".add_continuous_aggregate_policy(E'\"public\".\"daily\"', start_offset => NULL, end_offset => INTERVAL E'01:00:00', schedule_interval => INTERVAL E'01:00:00'), scheduled => false)"), "{sql}");
    }

    #[test]
    fn a_user_action_script_is_add_job() {
        let r = fixture(&[
            ("id", "1003"),
            ("name", "dbine_test_jobs_it's [1003]"),
            ("every", "1 day"),
            ("proc_schema", "public"),
            ("proc_name", "my proc"),
            ("scheduled", "f"),
            ("config", "{\"a\": 1}"),
            ("fixed_schedule", "true"),
            ("initial_start_text", "2026-11-12T03:00:00+00:00"),
            ("check_schema", "public"),
            ("check_name", "chk"),
        ]);
        let sql = script_of(Variant::Timescale, "public", &r, true).unwrap();
        assert!(sql.contains("SELECT \"public\".add_job(E'\"public\".\"my proc\"', INTERVAL E'1 day', config => E'{\"a\": 1}'::jsonb, initial_start => E'2026-11-12T03:00:00+00:00'::timestamptz, scheduled => false, check_config => E'\"public\".\"chk\"', fixed_schedule => true, job_name => E'dbine_test_jobs_it''s');"), "{sql}");
        assert!(!script_of(Variant::Timescale, "public", &r, false)
            .unwrap()
            .contains("job_name"));
    }

    #[test]
    fn the_extensions_own_jobs_have_no_script() {
        let r = fixture(&[
            ("id", "3"),
            ("proc_schema", "_timescaledb_functions"),
            ("proc_name", "policy_job_stat_history_retention"),
        ]);
        assert!(script_of(Variant::Timescale, "public", &r, true).is_none());
    }

    #[test]
    fn the_last_run_joins_the_history_once() {
        let logged = JobRun {
            id: Some("7".into()),
            started: Some("2026-11-12T12:56:39Z".into()),
            outcome: JobOutcome::Failed,
            ..Default::default()
        };
        let older = JobRun {
            started: Some("2026-11-11T12:00:00Z".into()),
            outcome: JobOutcome::Failed,
            ..Default::default()
        };
        // The stats' last run is the logged one: not added twice.
        let same = fixture(&[
            ("last_started", "1794488199"),
            ("last_run_status", "Failed"),
        ]);
        assert_eq!(
            merge(vec![logged.clone(), older.clone()], &same, 10).len(),
            2
        );
        // A later success the history didn't log goes first.
        let newer = fixture(&[
            ("last_started", "1794500000"),
            ("last_run_status", "Success"),
            ("last_ms", "1200"),
        ]);
        let runs = merge(vec![logged.clone(), older.clone()], &newer, 10);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].outcome, JobOutcome::Succeeded);
        assert_eq!(runs[0].duration_ms, Some(1200));
        // One going on.
        let going = fixture(&[
            ("last_started", "1794500000"),
            ("job_status", "Running"),
            ("last_ms", "5"),
        ]);
        let runs = merge(Vec::new(), &going, 10);
        assert_eq!(runs[0].outcome, JobOutcome::Running);
        assert_eq!(runs[0].duration_ms, None);
        // The limit holds.
        assert_eq!(merge(vec![logged, older], &newer, 1).len(), 1);
    }

    #[test]
    fn history_rows_map_to_runs() {
        let r = fixture(&[
            ("id", "5"),
            ("succeeded", "f"),
            ("started", "100"),
            ("ms", "20.2"),
            ("sqlerrcode", "P0001"),
            ("err_message", "boom"),
        ]);
        let run = history_run(&r);
        assert_eq!(run.outcome, JobOutcome::Failed);
        assert_eq!(run.message.as_deref(), Some("boom"));
        assert_eq!(run.duration_ms, Some(20));
        assert_eq!(run.started.as_deref(), Some("1970-01-01T00:01:40Z"));
    }

    #[test]
    fn a_run_without_an_outcome_is_going_on_or_crashed() {
        // Logged at its start: no outcome, no finish, its process there.
        let going = fixture(&[
            ("id", "6"),
            ("open", "t"),
            ("nopid", "f"),
            ("started", "100"),
        ]);
        let run = history_run(&going);
        assert_eq!(run.outcome, JobOutcome::Running);
        assert_eq!(run.duration_ms, None);
        // TimescaleDB cleared the process: a crash.
        let crashed = fixture(&[
            ("id", "7"),
            ("open", "t"),
            ("nopid", "t"),
            ("err_message", "job crash detected, see server logs"),
        ]);
        let run = history_run(&crashed);
        assert_eq!(run.outcome, JobOutcome::Failed);
        assert!(run.message.unwrap().contains("crash"));
        // Finished without an outcome: nothing to say.
        let odd = fixture(&[("id", "8"), ("open", "f"), ("nopid", "f")]);
        assert_eq!(history_run(&odd).outcome, JobOutcome::Unknown);
        let ok = fixture(&[("id", "9"), ("succeeded", "t"), ("open", "f")]);
        assert_eq!(history_run(&ok).outcome, JobOutcome::Succeeded);
        // The run going on is the one job_stats knows of: not added twice.
        let stats = fixture(&[("last_started", "100"), ("job_status", "Running")]);
        let runs = merge(vec![history_run(&going)], &stats, 10);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].outcome, JobOutcome::Running);
        // Also when their starts fall in different seconds.
        let later = fixture(&[("last_started", "101"), ("job_status", "Running")]);
        assert_eq!(merge(vec![history_run(&going)], &later, 10).len(), 1);
    }

    #[test]
    fn statements_take_only_numbers() {
        assert!(jobs_sql(Some(1000)).contains("WHERE j.job_id = 1000"));
        assert!(!jobs_sql(None).contains("WHERE"));
        let history = history_view_sql(1000, 5);
        assert!(history.contains("WHERE h.job_id = 1000") && history.ends_with("LIMIT 5"));
        assert!(history.contains("h.finish_time IS NULL AS open"));
        assert!(history.contains("(to_jsonb(h) ->> 'pid') IS NULL AS nopid"));
    }
}
