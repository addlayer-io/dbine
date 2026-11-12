//! Server jobs ([`dbine_driver::Session::jobs`]): Dremio's reflections,
//! whose refreshes Dremio schedules on its own (`sys.reflections`).
//!
//! A reflection is refreshed by the policy of its dataset, or else of the
//! dataset's source: every `accelerationRefreshPeriodMs`, on a cron
//! schedule (`accelerationRefreshSchedule`, 25.x on) or never; its status
//! says `refresh_status` (`SCHEDULED`, `RUNNING`, `MANUAL` when it isn't
//! refreshed on its own, `GIVEN_UP` after failing). Each refresh leaves a
//! materialization (`sys.materializations`: when, how long, its state and
//! error), which is the history while Dremio keeps it.
//!
//! - "Run now" refreshes the reflections of a physical dataset (`POST
//!   /api/v3/catalog/{id}/refresh`: every reflection that depends on that
//!   table, not only this one). A reflection on a view is refreshed from its
//!   tables, so it has no "run now". Dremio Software has no SQL for it
//!   (`REFRESH REFLECTIONS` is refused).
//! - "Stop" cancels the refresh going on now: the job of the reflection's
//!   `RUNNING` materialization (`init_refresh_job_id`; `sys.reflections`
//!   keeps saying `SCHEDULED` meanwhile, and `sys.jobs` doesn't list
//!   refreshes on Dremio Software), cancelled like a process (`POST
//!   /api/v3/job/{id}/cancel`).
//! - Enabling and disabling isn't offered: disabling a reflection drops its
//!   data and takes it out of queries, which is more than suspending its
//!   refreshes, and the refresh policy is the whole dataset's or source's.
//! - The script is the `ALTER TABLE … CREATE … REFLECTION` that recreates
//!   it (the one [`crate::index_usage`] writes).
//!
//! External reflections aren't listed: Dremio doesn't refresh them. Ids are
//! `reflection:<uuid>`; one that comes back has to be a UUID, which is the
//! only thing pasted into the history query.

use crate::index_usage::{create, dataset_path, index_def};
use crate::processes::valid_id;
use crate::{ddl, text, DremioSession};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, PropertyInfo, Result, ServerJob,
};
use reqwest::Method;
use serde_json::{Map, Value};
use std::collections::{HashMap, HashSet};

const KIND: &str = "Reflection";
/// Characters kept of a message.
const MAX_MESSAGE: usize = 4000;
/// Datasets whose refresh policy is read for the list.
const MAX_POLICIES: usize = 200;

const LIST: &str = "SELECT reflection_id, reflection_name, type, status, dataset_id, dataset_name, dataset_type, display_columns, dimensions, measures, sort_columns, partition_columns, distribution_columns, external_reflection, num_failures, last_failure_message, refresh_status, acceleration_status, record_count, current_footprint_bytes, last_refresh_duration_millis, last_refresh_from_table, refresh_method, reflection_mode, created_at FROM sys.reflections";

const LAST: &str =
    "SELECT reflection_id, created, state, last_refresh_duration_millis FROM sys.materializations";

const RUN_NOTE: &str =
    "«Ejecutar ahora» refresca todas las reflexiones que dependen de esta tabla, no solo esta.";
const VIEW_NOTE: &str = "Es una reflexión sobre una vista: Dremio la refresca a partir de las tablas de la vista, así que no tiene «Ejecutar ahora».";
const POLICY_NOTE: &str = "Dremio no permite suspender los refrescos de una sola reflexión: la política de refresco es de la tabla o del origen, y deshabilitar la reflexión borra sus datos.";
const HISTORY_NOTE: &str = "El historial son las materializaciones que Dremio todavía guarda: las viejas se borran al vencer.";

type Row = Map<String, Value>;
/// A reflection's newest materialization: when it started, its state and
/// how long it took (ms).
type Last = (Option<String>, Option<String>, Option<i64>);

fn s(r: &Row, k: &str) -> Option<String> {
    r.get(k)
        .filter(|v| !v.is_null())
        .map(text)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

fn n(r: &Row, k: &str) -> Option<i64> {
    match r.get(k)? {
        Value::Number(x) => x.as_i64(),
        Value::String(t) => t.trim().parse().ok(),
        _ => None,
    }
}

fn cut(t: &str) -> String {
    t.chars().take(MAX_MESSAGE).collect()
}

/// A reflection id: a UUID (`8-4-4-4-12` hex digits).
pub(crate) fn is_uuid(id: &str) -> bool {
    id.len() == 36
        && id.char_indices().all(|(i, c)| {
            if matches!(i, 8 | 13 | 18 | 23) {
                c == '-'
            } else {
                c.is_ascii_hexdigit()
            }
        })
}

pub(crate) fn parse_id(id: &str) -> Result<String> {
    id.strip_prefix("reflection:")
        .filter(|u| is_uuid(u))
        .map(str::to_ascii_lowercase)
        .ok_or_else(|| {
            Error::Query(format!(
                "«{}» no es un id de reflexión de Dremio",
                id.chars().take(200).collect::<String>()
            ))
        })
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `2026-11-12 16:17:24.622` (UTC, as Dremio's system tables give it) or
/// RFC 3339 `…Z`, as epoch seconds.
fn epoch_secs(t: &str) -> Option<i64> {
    let t = t.trim();
    let num = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (num(0, 4)?, num(5, 7)?, num(8, 10)?);
    let (h, mi, se) = (num(11, 13)?, num(14, 16)?, num(17, 19)?);
    if y <= 1970 && mo == 1 && d == 1 && h == 0 && mi == 0 && se == 0 {
        // Dremio's "never".
        return None;
    }
    Some(days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se)
}

fn rfc3339(secs: i64) -> String {
    let (days, rem) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let z = days + 719_468;
    let (era, doe) = (z.div_euclid(146_097), z.rem_euclid(146_097));
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

fn utc(t: Option<String>) -> Option<String> {
    t.as_deref().and_then(epoch_secs).map(rfc3339)
}

/// A refresh period in ms as `EVERY <n> <unit>` (the largest whole unit).
pub(crate) fn every(ms: i64) -> Option<String> {
    if ms <= 0 {
        return None;
    }
    for (unit, size) in [
        ("DAYS", 86_400_000),
        ("HOURS", 3_600_000),
        ("MINUTES", 60_000),
        ("SECONDS", 1000),
    ] {
        if ms % size == 0 {
            return Some(format!("EVERY {} {unit}", ms / size));
        }
    }
    Some(format!("EVERY {ms} MILLISECONDS"))
}

/// The refresh policy of a dataset (`accelerationRefreshPolicy`) or of a
/// source (`acceleration…` fields): `Some(schedule)` when it refreshes on
/// its own, `Some(None)` when it never does, `None` when it doesn't say.
pub(crate) fn policy(entity: &Value) -> Option<Option<String>> {
    let (never, kind, period, cron) = match entity.get("accelerationRefreshPolicy") {
        Some(p) if p.is_object() => (
            p.get("neverRefresh").and_then(Value::as_bool),
            p.get("activePolicyType").and_then(Value::as_str),
            p.get("refreshPeriodMs").and_then(Value::as_i64),
            p.get("refreshSchedule").and_then(Value::as_str),
        ),
        _ if entity.get("accelerationRefreshPeriodMs").is_some()
            || entity.get("accelerationNeverRefresh").is_some() =>
        {
            (
                entity
                    .get("accelerationNeverRefresh")
                    .and_then(Value::as_bool),
                entity
                    .get("accelerationActivePolicyType")
                    .and_then(Value::as_str),
                entity
                    .get("accelerationRefreshPeriodMs")
                    .and_then(Value::as_i64),
                entity
                    .get("accelerationRefreshSchedule")
                    .and_then(Value::as_str),
            )
        }
        _ => return None,
    };
    if never == Some(true) || kind == Some("NEVER") {
        return Some(None);
    }
    Some(match (kind, cron) {
        (Some("SCHEDULE"), Some(c)) if !c.trim().is_empty() => Some(format!("CRON {}", c.trim())),
        _ => period.and_then(every),
    })
}

/// The newest finished materialization of each reflection: (started,
/// state, duration). One still `RUNNING` isn't the last run's outcome.
pub(crate) fn last_of(rows: &[Row]) -> HashMap<String, Last> {
    let mut out: HashMap<String, Last> = HashMap::new();
    for r in rows {
        let Some(id) = s(r, "reflection_id") else {
            continue;
        };
        if s(r, "state").is_none_or(|st| st == "RUNNING") {
            continue;
        }
        let created = s(r, "created");
        let newer = match out.get(&id) {
            Some((Some(c), _, _)) => created.as_deref().is_some_and(|x| x > c.as_str()),
            _ => true,
        };
        if newer {
            out.insert(
                id,
                (created, s(r, "state"), n(r, "last_refresh_duration_millis")),
            );
        }
    }
    out
}

/// The reflections with a refresh going on now: a `RUNNING`
/// materialization (`sys.reflections` keeps saying `SCHEDULED` meanwhile).
pub(crate) fn refreshing_of(rows: &[Row]) -> HashSet<String> {
    rows.iter()
        .filter(|r| s(r, "state").as_deref() == Some("RUNNING"))
        .filter_map(|r| s(r, "reflection_id"))
        .map(|id| id.to_ascii_lowercase())
        .collect()
}

fn materialization_outcome(state: Option<&str>) -> JobOutcome {
    match state {
        // A deprecated one was good and a newer one replaced it.
        Some("DONE" | "DEPRECATED") => JobOutcome::Succeeded,
        Some("FAILED") => JobOutcome::Failed,
        Some("RUNNING") => JobOutcome::Running,
        Some("CANCELED" | "CANCELLED") => JobOutcome::Canceled,
        _ => JobOutcome::Unknown,
    }
}

fn physical(r: &Row) -> bool {
    s(r, "dataset_type").is_some_and(|t| t.starts_with("PHYSICAL"))
}

/// A `sys.reflections` row as a job; `schedule` is its policy (see
/// [`policy`]), `last` its newest finished materialization, `refreshing`
/// whether one is being made now.
pub(crate) fn job_of(
    r: &Row,
    schedule: Option<Option<String>>,
    last: Option<&Last>,
    refreshing: bool,
) -> Option<ServerJob> {
    let id = s(r, "reflection_id").filter(|u| is_uuid(u))?;
    if s(r, "external_reflection").is_some() {
        return None;
    }
    let status = s(r, "status").unwrap_or_default();
    let refresh = s(r, "refresh_status").unwrap_or_default();
    let running = refreshing || refresh == "RUNNING" || status == "REFRESHING";
    let never = matches!(schedule, Some(None));
    let failed = status == "FAILED" || refresh == "GIVEN_UP";
    let (started, state, ms) = last.cloned().unwrap_or_default();
    Some(ServerJob {
        id: format!("reflection:{}", id.to_ascii_lowercase()),
        name: s(r, "reflection_name").unwrap_or_else(|| id.clone()),
        kind: KIND.into(),
        group: s(r, "dataset_name"),
        description: s(r, "type").map(|t| format!("{t} REFLECTION")),
        owner: None,
        enabled: status != "DISABLED" && refresh != "MANUAL" && !never,
        schedule: schedule.flatten(),
        running,
        // How the last finished refresh ended (a failing reflection's
        // status says it even when its failed materialization is gone);
        // Running only while the first one is going.
        last_outcome: if failed {
            Some(JobOutcome::Failed)
        } else {
            state
                .as_deref()
                .map(|st| materialization_outcome(Some(st)))
                .or(running.then_some(JobOutcome::Running))
        },
        last_started: utc(started),
        last_duration_ms: ms
            .or_else(|| n(r, "last_refresh_duration_millis"))
            .and_then(|m| u64::try_from(m).ok()),
        next_run: None,
        can_run: physical(r) && status != "DISABLED",
        can_stop: running,
        can_enable: false,
        has_history: true,
        has_script: true,
        system: false,
    })
}

fn prop(label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: String::new(),
        label: label.into(),
        value: value.into(),
    }
}

/// The quoted dataset name and the `CREATE … REFLECTION`.
pub(crate) fn script_of(r: &Row) -> Option<String> {
    let dataset = s(r, "dataset_name")?;
    let table = dataset_path(&dataset)
        .iter()
        .map(|p| ddl::q(p))
        .collect::<Vec<_>>()
        .join(".");
    Some(format!("{}\n", create(&table, &index_def(r))))
}

pub(crate) fn detail_of(
    r: &Row,
    schedule: Option<Option<String>>,
    last: Option<&Last>,
    refreshing: bool,
) -> Option<JobDetail> {
    let job = job_of(r, schedule, last, refreshing)?;
    let mut properties = Vec::new();
    for (k, label) in [
        ("status", "Estado"),
        ("refresh_status", "Refresco"),
        ("acceleration_status", "Aceleración"),
        ("refresh_method", "Método de refresco"),
        ("reflection_mode", "Modo"),
        ("dataset_type", "Tipo de dataset"),
        ("record_count", "Filas"),
        ("current_footprint_bytes", "Tamaño (bytes)"),
        ("num_failures", "Fallas seguidas"),
    ] {
        if let Some(v) = s(r, k).filter(|v| v != "-1") {
            properties.push(prop(label, v));
        }
    }
    if let Some(t) = utc(s(r, "last_refresh_from_table")) {
        properties.push(prop("Datos de", t));
    }
    if let Some(t) = utc(s(r, "created_at")) {
        properties.push(prop("Creada", t));
    }
    if let Some(m) = s(r, "last_failure_message") {
        properties.push(prop("Último error", cut(&m)));
    }
    let mut notes = Vec::new();
    if physical(r) {
        notes.push(RUN_NOTE.to_string());
    } else {
        notes.push(VIEW_NOTE.to_string());
    }
    notes.push(POLICY_NOTE.into());
    notes.push(HISTORY_NOTE.into());
    if job.schedule.is_none() && job.enabled {
        notes.push("Dremio no informa la política de refresco de esta reflexión.".into());
    }
    Some(JobDetail {
        steps: vec![JobStep {
            name: "Refresco".into(),
            kind: Some("Reflection".into()),
            target: s(r, "dataset_name"),
            command: script_of(r).map(|t| t.trim_end().to_string()),
        }],
        schedules: Vec::new(),
        properties,
        notes,
        job,
    })
}

/// The jobs of a reflection's refreshes going on now: the job that makes
/// each `RUNNING` materialization. `uuid` has to be a checked UUID (hex
/// digits and dashes: nothing to escape).
pub(crate) fn refresh_jobs_sql(uuid: &str) -> Option<String> {
    is_uuid(uuid).then(|| {
        format!(
            "SELECT init_refresh_job_id AS job_id FROM sys.materializations WHERE reflection_id = '{uuid}' AND state = 'RUNNING'"
        )
    })
}

/// The same from `sys.jobs`, for a Dremio whose `sys.materializations`
/// doesn't name the job: the refresh's statement is `REFRESH REFLECTION
/// '<reflection id>' AS '<materialization id>'` (a UUID has no `%` or `_`
/// for `LIKE`).
pub(crate) fn refresh_jobs_fallback_sql(uuid: &str) -> Option<String> {
    is_uuid(uuid).then(|| {
        format!(
            "SELECT job_id FROM sys.jobs WHERE status NOT IN ('COMPLETED', 'FAILED', 'CANCELED', 'CANCELLED') AND query LIKE 'REFRESH REFLECTION ''{uuid}''%'"
        )
    })
}

/// A materialization as a run.
pub(crate) fn run_of(r: &Row) -> JobRun {
    let state = s(r, "state");
    JobRun {
        id: s(r, "materialization_id"),
        started: utc(s(r, "created")),
        duration_ms: n(r, "last_refresh_duration_millis").and_then(|m| u64::try_from(m).ok()),
        outcome: materialization_outcome(state.as_deref()),
        trigger: None,
        message: s(r, "failure_msg").filter(|m| m != "NONE").map(|m| cut(&m)),
        steps: Vec::new(),
    }
}

fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe la reflexión «{id}» o este usuario no la puede ver"
    ))
}

impl DremioSession {
    async fn reflections(&self) -> Result<Vec<Row>> {
        self.records(LIST).await
    }

    async fn reflection(&self, uuid: &str) -> Result<Row> {
        // Read whole and picked here: nothing of the id goes to the server.
        self.reflections()
            .await?
            .into_iter()
            .find(|r| s(r, "reflection_id").is_some_and(|x| x.eq_ignore_ascii_case(uuid)))
            .ok_or_else(|| not_found(uuid))
    }

    /// The refresh policy of a dataset, else of its source; `None` when
    /// neither says (or can't be read).
    async fn refresh_policy(
        &self,
        r: &Row,
        sources: &mut HashMap<String, Option<Option<String>>>,
    ) -> Option<Option<String>> {
        let id = s(r, "dataset_id").filter(|u| is_uuid(u))?;
        if !physical(r) {
            return None;
        }
        if let Ok(d) = self
            .conn
            .send(Method::GET, &format!("/api/v3/catalog/{id}"), None)
            .await
        {
            if let Some(p) = policy(&d) {
                return Some(p);
            }
        }
        let source = dataset_path(&s(r, "dataset_name")?).into_iter().next()?;
        if let Some(p) = sources.get(&source) {
            return p.clone();
        }
        let p = match self
            .conn
            .send(
                Method::GET,
                &format!("/api/v3/catalog/by-path/{}", path_segment(&source)),
                None,
            )
            .await
        {
            Ok(v) if v.get("entityType").and_then(Value::as_str) == Some("source") => policy(&v),
            _ => None,
        };
        sources.insert(source, p.clone());
        p
    }

    /// Each reflection's last finished materialization, and the ones
    /// being refreshed now.
    async fn last_materializations(&self) -> (HashMap<String, Last>, HashSet<String>) {
        match self.records(LAST).await {
            Ok(rows) => (last_of(&rows), refreshing_of(&rows)),
            Err(e) => {
                tracing::debug!("dremio sys.materializations: {e}");
                Default::default()
            }
        }
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        let rows = self.reflections().await?;
        let (last, refreshing) = self.last_materializations().await;
        let mut sources = HashMap::new();
        let mut jobs = Vec::new();
        for (i, r) in rows.iter().enumerate() {
            let schedule = if i < MAX_POLICIES {
                self.refresh_policy(r, &mut sources).await
            } else {
                None
            };
            let id = s(r, "reflection_id").unwrap_or_default();
            let busy = refreshing.contains(&id.to_ascii_lowercase());
            if let Some(j) = job_of(r, schedule, last.get(&id), busy) {
                jobs.push(j);
            }
        }
        let mut notes = Vec::new();
        if rows.len() > MAX_POLICIES {
            notes.push(format!(
                "Se muestra la política de refresco de las primeras {MAX_POLICIES} reflexiones."
            ));
        }
        Ok(JobList { jobs, notes })
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        let uuid = parse_id(id)?;
        let r = self.reflection(&uuid).await?;
        let schedule = self.refresh_policy(&r, &mut HashMap::new()).await;
        let (last, refreshing) = self.last_materializations().await;
        let id = s(&r, "reflection_id").unwrap_or_default();
        detail_of(&r, schedule, last.get(&id), refreshing.contains(&uuid))
            .ok_or_else(|| not_found(&uuid))
    }

    pub(crate) async fn server_job_runs(&mut self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let uuid = parse_id(id)?;
        self.reflection(&uuid).await?;
        let limit = limit.clamp(1, MAX_JOB_RUNS);
        // `uuid` is hex digits and dashes only.
        let rows = self
            .records(&format!(
                "SELECT materialization_id, created, state, failure_msg, last_refresh_duration_millis FROM sys.materializations WHERE reflection_id = '{uuid}' ORDER BY created DESC LIMIT {limit}"
            ))
            .await?;
        Ok(rows.iter().map(run_of).collect())
    }

    pub(crate) async fn run_server_job(&mut self, id: &str) -> Result<()> {
        let uuid = parse_id(id)?;
        let r = self.reflection(&uuid).await?;
        if !physical(&r) {
            return Err(Error::Unsupported(VIEW_NOTE.into()));
        }
        let dataset = s(&r, "dataset_id")
            .filter(|d| is_uuid(d))
            .ok_or_else(|| not_found(&uuid))?;
        self.conn
            .send(
                Method::POST,
                &format!("/api/v3/catalog/{dataset}/refresh"),
                None,
            )
            .await
            .map(|_| ())
    }

    /// Cancels the reflection's refresh going on now (the job that makes
    /// its `RUNNING` materialization, through the same call as the
    /// processes' cancel).
    pub(crate) async fn stop_server_job(&mut self, id: &str) -> Result<()> {
        let uuid = parse_id(id)?;
        let r = self.reflection(&uuid).await?;
        // The id as the system tables write it.
        let rid = s(&r, "reflection_id")
            .filter(|u| is_uuid(u))
            .ok_or_else(|| not_found(&uuid))?;
        let rows = match self
            .records(&refresh_jobs_sql(&rid).unwrap_or_default())
            .await
        {
            Ok(rows) => rows,
            Err(e) => {
                tracing::debug!("dremio refresh jobs from sys.materializations: {e}");
                self.records(&refresh_jobs_fallback_sql(&rid).unwrap_or_default())
                    .await?
            }
        };
        let running: Vec<String> = rows
            .iter()
            .filter_map(|j| s(j, "job_id"))
            .filter(|j| valid_id(j).is_some())
            .collect();
        if running.is_empty() {
            return Err(Error::Query(format!(
                "la reflexión «{}» no se está refrescando",
                s(&r, "reflection_name").unwrap_or(uuid)
            )));
        }
        for job in running {
            // The materialization names its job a moment before the job is
            // registered: Dremio answers "Could not find a job" meanwhile.
            let mut tries = 0;
            loop {
                match self.cancel_running(&job).await {
                    Err(Error::Query(m)) if m.contains("Could not find a job") && tries < 10 => {
                        tries += 1;
                        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    }
                    r => break r?,
                }
            }
        }
        Ok(())
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        let uuid = parse_id(id)?;
        let r = self.reflection(&uuid).await?;
        script_of(&r).ok_or_else(|| not_found(&uuid))
    }
}

/// A path segment, percent-encoded.
fn path_segment(v: &str) -> String {
    v.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Row {
        v.as_object().unwrap().clone()
    }

    fn sample() -> Row {
        row(
            json!({"reflection_id": "8c65f4b8-783c-405d-88b6-b4605b94643e", "reflection_name": "r1", "type": "RAW",
            "status": "CAN_ACCELERATE", "dataset_id": "d9ced5ed-cb9b-4ce4-9b6e-799d6f51c138",
            "dataset_name": "\"$scratch\".t1", "dataset_type": "PHYSICAL_DATASET_SOURCE_FOLDER",
            "display_columns": "a, b", "dimensions": "", "measures": "", "sort_columns": "", "partition_columns": "a",
            "distribution_columns": "", "external_reflection": "", "num_failures": 0, "last_failure_message": "",
            "refresh_status": "SCHEDULED", "acceleration_status": "NONE", "record_count": 2,
            "current_footprint_bytes": 6467, "last_refresh_duration_millis": 886,
            "last_refresh_from_table": "2026-11-12 16:17:24.621", "refresh_method": "INCREMENTAL",
            "reflection_mode": "Manual", "created_at": "2026-11-12 16:17:24.524"}),
        )
    }

    #[test]
    fn reflections_become_jobs() {
        let last = (
            Some("2026-11-12 16:17:37.203".to_string()),
            Some("DONE".to_string()),
            Some(180),
        );
        let j = job_of(
            &sample(),
            Some(Some("EVERY 1 HOURS".into())),
            Some(&last),
            false,
        )
        .unwrap();
        assert_eq!(j.id, "reflection:8c65f4b8-783c-405d-88b6-b4605b94643e");
        assert_eq!(j.kind, "Reflection");
        assert_eq!(j.group.as_deref(), Some("\"$scratch\".t1"));
        assert_eq!(j.schedule.as_deref(), Some("EVERY 1 HOURS"));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T16:17:37Z"));
        assert_eq!(j.last_duration_ms, Some(180));
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert!(j.enabled && j.can_run && j.has_history && j.has_script);
        assert!(!j.can_stop && !j.can_enable && !j.running && !j.system);

        let mut never = sample();
        never.insert("refresh_status".into(), json!("MANUAL"));
        assert!(!job_of(&never, None, None, false).unwrap().enabled);
        assert!(!job_of(&sample(), Some(None), None, false).unwrap().enabled);

        let mut failed = sample();
        failed.insert("status".into(), json!("FAILED"));
        failed.insert("refresh_status".into(), json!("GIVEN_UP"));
        failed.insert("dataset_type".into(), json!("VIRTUAL_DATASET"));
        let j = job_of(&failed, None, None, false).unwrap();
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert!(!j.can_run, "a view's reflection has no run now");

        let mut external = sample();
        external.insert("external_reflection".into(), json!("ext"));
        assert!(job_of(&external, None, None, false).is_none());

        // Refreshing: it can be stopped, and the last finished refresh still
        // says how it ended (Running only before the first one ends).
        let j = job_of(&sample(), None, Some(&last), true).unwrap();
        assert!(j.running && j.can_stop);
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        let j = job_of(&sample(), None, None, true).unwrap();
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));
        let mut busy = sample();
        busy.insert("refresh_status".into(), json!("RUNNING"));
        assert!(job_of(&busy, None, None, false).unwrap().can_stop);
    }

    #[test]
    fn policies() {
        assert_eq!(
            policy(
                &json!({"entityType": "source", "accelerationRefreshPeriodMs": 3_600_000,
                "accelerationActivePolicyType": "PERIOD", "accelerationNeverRefresh": false})
            ),
            Some(Some("EVERY 1 HOURS".into()))
        );
        assert_eq!(
            policy(
                &json!({"accelerationRefreshPolicy": {"activePolicyType": "SCHEDULE",
                "refreshSchedule": "0 0 8 * * ?", "refreshPeriodMs": 3_600_000}})
            ),
            Some(Some("CRON 0 0 8 * * ?".into()))
        );
        assert_eq!(
            policy(
                &json!({"accelerationRefreshPolicy": {"neverRefresh": true, "refreshPeriodMs": 1}})
            ),
            Some(None)
        );
        assert_eq!(policy(&json!({"entityType": "dataset"})), None);
        assert_eq!(every(90_000).as_deref(), Some("EVERY 90 SECONDS"));
        assert_eq!(every(2 * 86_400_000).as_deref(), Some("EVERY 2 DAYS"));
        assert_eq!(every(0), None);
    }

    #[test]
    fn detail_runs_and_script() {
        let d = detail_of(&sample(), None, None, false).unwrap();
        assert!(d.notes[0].contains("todas las reflexiones"));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Refresco" && p.value == "SCHEDULED"));
        assert_eq!(
            script_of(&sample()).unwrap(),
            "ALTER TABLE \"$scratch\".\"t1\" CREATE RAW REFLECTION \"r1\" USING DISPLAY (\"a\", \"b\") PARTITION BY (\"a\");\n"
        );
        let r = run_of(&row(
            json!({"materialization_id": "m1", "created": "2026-11-12 16:17:24.622",
            "state": "FAILED", "failure_msg": "out of memory", "last_refresh_duration_millis": -1}),
        ));
        assert_eq!(r.outcome, JobOutcome::Failed);
        assert_eq!(r.message.as_deref(), Some("out of memory"));
        assert_eq!(r.duration_ms, None);
        assert_eq!(r.started.as_deref(), Some("2026-11-12T16:17:24Z"));
        let ok = run_of(&row(json!({"state": "DONE", "failure_msg": "NONE"})));
        assert_eq!(ok.outcome, JobOutcome::Succeeded);
        assert!(ok.message.is_none());
        let last = last_of(&[
            row(
                json!({"reflection_id": "x", "created": "2026-11-12 16:17:24.622", "state": "DEPRECATED"}),
            ),
            row(
                json!({"reflection_id": "x", "created": "2026-11-12 16:17:37.203", "state": "DONE"}),
            ),
            row(
                json!({"reflection_id": "x", "created": "2026-11-12 16:18:00.000", "state": "RUNNING"}),
            ),
        ]);
        assert_eq!(
            last["x"].1.as_deref(),
            Some("DONE"),
            "a running one isn't the last"
        );
        let busy = refreshing_of(&[
            row(json!({"reflection_id": "X", "state": "RUNNING"})),
            row(json!({"reflection_id": "y", "state": "DONE"})),
        ]);
        assert!(busy.contains("x") && !busy.contains("y"));
    }

    #[test]
    fn the_refresh_to_stop() {
        assert_eq!(
            refresh_jobs_sql("8c65f4b8-783c-405d-88b6-b4605b94643e").unwrap(),
            "SELECT init_refresh_job_id AS job_id FROM sys.materializations WHERE reflection_id = '8c65f4b8-783c-405d-88b6-b4605b94643e' AND state = 'RUNNING'"
        );
        assert_eq!(
            refresh_jobs_fallback_sql("8c65f4b8-783c-405d-88b6-b4605b94643e").unwrap(),
            "SELECT job_id FROM sys.jobs WHERE status NOT IN ('COMPLETED', 'FAILED', 'CANCELED', 'CANCELLED') AND query LIKE 'REFRESH REFLECTION ''8c65f4b8-783c-405d-88b6-b4605b94643e''%'"
        );
        for bad in ["x' OR '1'='1", "8c65f4b8-783c-405d-88b6-b4605b94643%"] {
            assert!(refresh_jobs_sql(bad).is_none() && refresh_jobs_fallback_sql(bad).is_none());
        }
    }

    #[test]
    fn ids_are_uuids() {
        assert_eq!(
            parse_id("reflection:8C65F4B8-783C-405D-88B6-B4605B94643E").unwrap(),
            "8c65f4b8-783c-405d-88b6-b4605b94643e"
        );
        for bad in [
            "",
            "8c65f4b8-783c-405d-88b6-b4605b94643e",
            "reflection:8c65f4b8-783c-405d-88b6-b4605b94643",
            "reflection:8c65f4b8' OR '1'='1-405d-88b6-b4605b94",
            "reflection:zc65f4b8-783c-405d-88b6-b4605b94643e",
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?}");
        }
        assert_eq!(epoch_secs("1970-01-01 00:00:00.000"), None);
        assert_eq!(path_segment("$scratch"), "%24scratch");
        assert_eq!(path_segment("a/b c"), "a%2Fb%20c");
    }
}
