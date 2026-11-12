//! Server jobs ([`dbine_driver::Session::jobs`]): the Backup Service's
//! active repositories (Couchbase Server Enterprise 7.0 on), through the
//! cluster manager's proxy (`/_p/backup/api/v1`, so the connection's
//! management port is enough).
//!
//! A repository is the job: its plan's tasks (backups, merges, prunes,
//! each on its own schedule) are its steps and schedules, and what the
//! service says of it (`scheduled` with each task's next run, the tasks
//! running now) fills the row.
//!
//! - "Run now" is a one-off backup (`POST …/backup`, incremental: the
//!   first one of a repository is full anyway). The service takes it also
//!   while the repository is paused. Merges aren't offered: they need the
//!   range of backups to merge.
//! - Enabling and disabling is `…/resume` / `…/pause`.
//! - The service has no call to cancel a running task: no stop.
//! - The history is `…/taskHistory` (newest first), with each node's run
//!   as a step, plus the tasks running now.
//! - The script is the plan and the repository as the REST calls that
//!   create them (a built-in plan, `default: true`, isn't recreated).
//!
//! Eventing functions aren't listed: they react to document changes (they
//! are triggers, not scheduled work), and their timers are callbacks the
//! code creates per document, with no definition of their own.
//!
//! Community Edition has no Backup Service, so its list is empty with a
//! note. Ids are `repo:<repository>`: a name of letters, digits, `_`, `-`
//! and `.` (not only dots), percent-encoded into the path.

use crate::{encode, CbSession};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, JobStepRun, PropertyInfo, Result,
    ServerJob,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

const KIND: &str = "Backup repository";
const API: &str = "/_p/backup/api/v1";
/// Repositories whose last run is read for the list.
const MAX_LAST_RUNS: usize = 200;
/// Characters kept of a message.
const MAX_MESSAGE: usize = 4000;
/// Longest repository name taken back.
const MAX_NAME: usize = 128;

const NO_SERVICE: &str = "Este cluster no tiene el servicio de backup, que es el que programa tareas en Couchbase (Enterprise Edition 7.0 en adelante): no hay jobs del servidor.";
const COMMUNITY: &str = "Couchbase Community Edition no tiene el servicio de backup, que es el que programa tareas en Couchbase: no hay jobs del servidor.";
const DENIED: &str = "Este usuario no puede ver el servicio de backup (hace falta el rol Read-Only Admin, Backup Full Admin o Full Admin).";
const SCOPE: &str = "Se listan los repositorios activos del servicio de backup; los archivados no tienen tareas programadas. Las funciones de Eventing no se listan: reaccionan a cambios en los documentos.";
const RUN_NOTE: &str = "«Ejecutar ahora» hace un backup incremental (el primero de un repositorio es completo). Los merges se programan en el plan.";
const NO_STOP_NOTE: &str = "El servicio de backup no permite cancelar una tarea en curso.";

pub(crate) fn job_id(repo: &str) -> String {
    format!("repo:{repo}")
}

/// A repository name: letters, digits, `_`, `-` and `.`, not only dots.
fn repo_name(r: &str) -> bool {
    !r.is_empty()
        && r.len() <= MAX_NAME
        && r.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '-' | '.'))
        && r.chars().any(|c| c != '.')
}

pub(crate) fn parse_id(id: &str) -> Result<String> {
    id.strip_prefix("repo:")
        .filter(|r| repo_name(r))
        .map(str::to_string)
        .ok_or_else(|| {
            Error::Query(format!(
                "«{}» no es un id de repositorio de backup de Couchbase",
                id.chars().take(200).collect::<String>()
            ))
        })
}

fn s(v: &Value, k: &str) -> Option<String> {
    v.get(k)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|x| !x.is_empty())
        .map(str::to_string)
}

fn cut(t: &str) -> String {
    t.chars().take(MAX_MESSAGE).collect()
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

/// Milliseconds since 1970 of an RFC 3339 time (`…Z` or `…±hh:mm`, any
/// fraction). The zero time Go writes for "not yet" (`0001-01-01…`) is
/// `None`.
pub(crate) fn epoch_ms(t: &str) -> Option<i64> {
    let t = t.trim();
    let n = |a: usize, b: usize| t.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    if y <= 1 {
        return None;
    }
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let rest = t.get(19..)?;
    let (frac, zone) = match rest.strip_prefix('.') {
        Some(r) => {
            let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            (&r[..end], &r[end..])
        }
        None => ("", rest),
    };
    let ms_frac: i64 = format!("{frac:0<3}")[..3].parse().unwrap_or(0);
    let offset = match zone {
        "Z" | "z" | "" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let hh: i64 = z.get(1..3)?.parse().ok()?;
            let mm: i64 = z.get(4..6).and_then(|m| m.parse().ok()).unwrap_or(0);
            sign * (hh * 3600 + mm * 60)
        }
    };
    let secs = days_from_civil(y, mo, d) * 86_400 + h * 3600 + mi * 60 + se - offset;
    Some(secs * 1000 + ms_frac)
}

/// Milliseconds since 1970 as RFC 3339 in UTC, to the second.
fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
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
    t.as_deref().and_then(epoch_ms).map(rfc3339)
}

fn duration(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    let (a, b) = (epoch_ms(start?)?, epoch_ms(end?)?);
    u64::try_from(b - a).ok()
}

fn outcome(status: Option<&str>) -> JobOutcome {
    match status.map(str::to_ascii_lowercase).as_deref() {
        Some("done" | "success" | "succeeded" | "completed") => JobOutcome::Succeeded,
        Some("failed" | "error") => JobOutcome::Failed,
        Some("running" | "waiting") => JobOutcome::Running,
        Some("cancelled" | "canceled" | "stopped") => JobOutcome::Canceled,
        _ => JobOutcome::Unknown,
    }
}

/// A plan task's schedule as the service states it: `<frequency>
/// <period> <time>` (`1 HOURS 00:00`, `4 SUNDAY 23:50`).
pub(crate) fn schedule_text(task: &Value) -> Option<String> {
    let sch = task.get("schedule")?;
    let freq = sch.get("frequency").and_then(Value::as_u64)?;
    let period = s(sch, "period")?;
    Some(match s(sch, "time") {
        Some(t) => format!("{freq} {period} {t}"),
        None => format!("{freq} {period}"),
    })
}

fn tasks(plan: Option<&Value>) -> Vec<&Value> {
    plan.and_then(|p| p.get("tasks"))
        .and_then(Value::as_array)
        .map(|a| a.iter().collect())
        .unwrap_or_default()
}

/// The tasks running now in a repository (one-off and scheduled).
fn running_tasks(repo: &Value) -> Vec<&Value> {
    ["running_one_off", "running_tasks"]
        .iter()
        .filter_map(|k| repo.get(*k).and_then(Value::as_object))
        .flat_map(|m| m.values())
        .collect()
}

/// A repository as a job, with its plan (when it could be read) and its
/// last finished task (when there's one).
pub(crate) fn job_of(
    repo: &Value,
    plan: Option<&Value>,
    last: Option<&Value>,
) -> Option<ServerJob> {
    let name = s(repo, "id").filter(|r| repo_name(r))?;
    let running = !running_tasks(repo).is_empty();
    // `taskHistory` holds finished tasks; a running one there isn't "last".
    let last = last.filter(|l| outcome(s(l, "status").as_deref()) != JobOutcome::Running);
    let next_run = repo
        .get("scheduled")
        .and_then(Value::as_object)
        .into_iter()
        .flat_map(|m| m.values())
        .filter_map(|t| s(t, "next_run").as_deref().and_then(epoch_ms))
        .min()
        .map(rfc3339);
    let schedule = {
        let parts: Vec<String> = tasks(plan)
            .iter()
            .filter_map(|t| Some(format!("{}: {}", s(t, "name")?, schedule_text(t)?)))
            .collect();
        (!parts.is_empty()).then(|| parts.join("; "))
    };
    let last_start = last.and_then(|l| s(l, "start"));
    Some(ServerJob {
        id: job_id(&name),
        name,
        kind: KIND.into(),
        group: repo
            .pointer("/bucket/name")
            .and_then(Value::as_str)
            .map(str::to_string),
        description: plan.and_then(|p| s(p, "description")),
        owner: None,
        enabled: s(repo, "state").as_deref() != Some("paused"),
        schedule,
        running,
        // How the last finished task ended; Running only while the first
        // one is still going.
        last_outcome: last
            .map(|l| outcome(s(l, "status").as_deref()))
            .or(running.then_some(JobOutcome::Running)),
        last_started: utc(last_start.clone()),
        last_duration_ms: last
            .and_then(|l| duration(last_start.as_deref(), s(l, "end").as_deref())),
        next_run,
        can_run: true,
        can_stop: false,
        can_enable: true,
        has_history: true,
        has_script: true,
        system: false,
    })
}

fn prop(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: value.into(),
    }
}

fn task_kind(task: &Value) -> Option<String> {
    let t = s(task, "task_type")?;
    Some(match t.as_str() {
        "BACKUP" if task.get("full_backup").and_then(Value::as_bool) == Some(true) => {
            "Backup completo".into()
        }
        "BACKUP" => "Backup incremental".into(),
        _ => t,
    })
}

pub(crate) fn detail_of(
    repo: &Value,
    plan: Option<&Value>,
    last: Option<&Value>,
) -> Option<JobDetail> {
    let job = job_of(repo, plan, last)?;
    let mut steps = Vec::new();
    let mut schedules = Vec::new();
    let next: HashMap<String, String> = repo
        .get("scheduled")
        .and_then(Value::as_object)
        .map(|m| {
            m.values()
                .filter_map(|t| Some((s(t, "name")?, utc(s(t, "next_run"))?)))
                .collect()
        })
        .unwrap_or_default();
    for t in tasks(plan) {
        let Some(name) = s(t, "name") else { continue };
        let when = schedule_text(t);
        let mut command = when.clone().unwrap_or_default();
        if let Some(m) = t.get("merge_options") {
            command.push_str(&format!(" merge_options={m}"));
        }
        if let Some(n) = next.get(&name) {
            command.push_str(&format!(" (próxima: {n})"));
        }
        if let Some(w) = &when {
            schedules.push(format!("{name}: {w}"));
        }
        steps.push(JobStep {
            name,
            kind: task_kind(t),
            target: None,
            command: Some(command.trim().to_string()).filter(|c| !c.is_empty()),
        });
    }
    let mut properties = Vec::new();
    if let Some(p) = s(repo, "plan_name") {
        properties.push(prop("", "Plan", p));
    }
    if let Some(st) = s(repo, "state") {
        properties.push(prop("", "Estado", st));
    }
    if let Some(a) = s(repo, "archive") {
        properties.push(prop("", "Archivo", a));
    }
    if let Some(b) = repo.pointer("/bucket/name").and_then(Value::as_str) {
        properties.push(prop("", "Bucket", b));
    }
    if let Some(svc) = plan
        .and_then(|p| p.get("services"))
        .and_then(Value::as_array)
    {
        let v: Vec<String> = svc
            .iter()
            .filter_map(Value::as_str)
            .map(str::to_string)
            .collect();
        if !v.is_empty() {
            properties.push(prop("", "Servicios", v.join(", ")));
        }
    }
    if let Some(h) = repo.pointer("/health/healthy").and_then(Value::as_bool) {
        properties.push(prop("", "Sano", if h { "sí" } else { "no" }));
    }
    if let Some(i) = repo.pointer("/health/issue").and_then(Value::as_str) {
        properties.push(prop("", "Problema", cut(i)));
    }
    if let Some(u) = s(repo, "repo") {
        properties.push(prop("", "UUID", u));
    }
    if let Some(c) = utc(s(repo, "creation_time")) {
        properties.push(prop("", "Creado", c));
    }
    if let Some(cloud) = repo.get("cloud_info").and_then(Value::as_object) {
        for (k, label) in [
            ("region", "Región"),
            ("endpoint", "Endpoint"),
            ("staging_dir", "Directorio de preparación"),
            ("cloud_credentials_name", "Credenciales"),
        ] {
            if let Some(v) = cloud
                .get(k)
                .and_then(Value::as_str)
                .filter(|v| !v.is_empty())
            {
                properties.push(prop("Nube", label, v));
            }
        }
    }
    Some(JobDetail {
        steps,
        schedules,
        properties,
        notes: vec![RUN_NOTE.into(), NO_STOP_NOTE.into()],
        job,
    })
}

/// A finished (or running) task as a run, its nodes as steps.
pub(crate) fn run_of(t: &Value, scheduled: &[String]) -> JobRun {
    let name = s(t, "task_name");
    let start = s(t, "start");
    let steps = t
        .get("node_runs")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|n| {
            let st = s(n, "start");
            JobStepRun {
                name: format!("Nodo {}", s(n, "node_id").unwrap_or_default()),
                outcome: outcome(s(n, "status").as_deref()),
                started: utc(st.clone()),
                duration_ms: duration(st.as_deref(), s(n, "end").as_deref()),
                message: s(n, "error").map(|e| cut(&e)),
            }
        })
        .collect();
    JobRun {
        trigger: name.as_ref().map(|n| {
            if scheduled.contains(n) {
                "programada".to_string()
            } else {
                "manual".to_string()
            }
        }),
        id: name,
        started: utc(start.clone()),
        duration_ms: duration(start.as_deref(), s(t, "end").as_deref()),
        outcome: outcome(s(t, "status").as_deref()),
        message: s(t, "error").map(|e| cut(&e)),
        steps,
    }
}

/// The REST calls that create the plan (unless it's built in) and the
/// repository.
pub(crate) fn script_of(repo: &Value, plan: Option<&Value>) -> Result<String> {
    let name = s(repo, "id").filter(|r| repo_name(r)).ok_or_else(|| {
        Error::Query("el servicio de backup devolvió un repositorio sin nombre".into())
    })?;
    let plan_name = s(repo, "plan_name")
        .ok_or_else(|| Error::Query(format!("el repositorio «{name}» no tiene plan")))?;
    let mut out = String::from(
        "// API REST del servicio de backup de Couchbase (puerto 8097, o /_p/backup en el puerto de administración).\n",
    );
    match plan {
        Some(p) if p.get("default").and_then(Value::as_bool) == Some(true) => {
            out.push_str(&format!(
                "// El plan «{plan_name}» viene con Couchbase: no hace falta crearlo.\n\n"
            ));
        }
        Some(p) => {
            let mut body = Map::new();
            for k in ["description", "services", "tasks"] {
                if let Some(v) = p.get(k).filter(|v| !v.is_null()) {
                    body.insert(k.into(), v.clone());
                }
            }
            out.push_str(&format!(
                "POST /api/v1/plan/{}\n{}\n\n",
                encode(&plan_name),
                serde_json::to_string_pretty(&Value::Object(body)).unwrap_or_default()
            ));
        }
        None => out.push_str(&format!(
            "// No se pudo leer el plan «{plan_name}»: crealo antes que el repositorio.\n\n"
        )),
    }
    let mut body = Map::new();
    body.insert("plan".into(), json!(plan_name));
    if let Some(a) = s(repo, "archive") {
        body.insert("archive".into(), json!(a));
    }
    if let Some(b) = repo.pointer("/bucket/name").and_then(Value::as_str) {
        body.insert("bucket_name".into(), json!(b));
    }
    if let Some(cloud) = repo.get("cloud_info").and_then(Value::as_object) {
        for (k, to) in [
            ("cloud_credentials_name", "cloud_credentials_name"),
            ("staging_dir", "cloud_staging_dir"),
            ("region", "cloud_region"),
            ("endpoint", "cloud_endpoint"),
            ("force_path_style", "cloud_force_path_style"),
        ] {
            if let Some(v) = cloud.get(k).filter(|v| !v.is_null()) {
                body.insert(to.into(), v.clone());
            }
        }
    }
    out.push_str(&format!(
        "POST /api/v1/cluster/self/repository/active/{}\n{}\n",
        encode(&name),
        serde_json::to_string_pretty(&Value::Object(body)).unwrap_or_default()
    ));
    Ok(out)
}

fn denied(e: &Error) -> bool {
    match e {
        Error::AuthFailed(_) => true,
        Error::Query(m) => m.starts_with("Sin permiso") || m.contains("HTTP 403"),
        _ => false,
    }
}

fn not_found(repo: &str) -> Error {
    Error::Query(format!(
        "no existe el repositorio de backup activo «{repo}»"
    ))
}

impl CbSession {
    async fn backup_get(&self, path: &str) -> Result<Value> {
        self.conn.mgmt_get(&format!("{API}{path}")).await
    }

    async fn backup_post(&self, path: &str, body: Value) -> Result<String> {
        self.conn
            .mgmt_send(
                self.conn
                    .http
                    .post(format!("{}{API}{path}", self.conn.mgmt))
                    .json(&body),
            )
            .await
    }

    /// Whether some node runs the Backup Service, and whether the cluster
    /// is Enterprise.
    async fn backup_service(&self) -> Result<(bool, bool)> {
        let enterprise = self
            .conn
            .mgmt_get("/pools")
            .await?
            .get("isEnterprise")
            .and_then(Value::as_bool)
            .unwrap_or(true);
        let pool = self.conn.mgmt_get("/pools/default").await?;
        let has = pool
            .get("nodes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .any(|n| {
                n.get("services")
                    .and_then(Value::as_array)
                    .is_some_and(|s| s.iter().any(|x| x.as_str() == Some("backup")))
            });
        Ok((has, enterprise))
    }

    async fn repo(&self, name: &str) -> Result<Value> {
        self.backup_get(&format!("/cluster/self/repository/active/{}", encode(name)))
            .await
            .map_err(|e| match e {
                Error::Query(m) if m.starts_with("HTTP 404") => not_found(name),
                e => e,
            })
    }

    async fn plan(&self, name: &str) -> Option<Value> {
        match self.backup_get(&format!("/plan/{}", encode(name))).await {
            Ok(p) => Some(p),
            Err(e) => {
                tracing::debug!("couchbase backup plan {name}: {e}");
                None
            }
        }
    }

    async fn history(&self, repo: &str, limit: u32) -> Result<Vec<Value>> {
        let v = self
            .backup_get(&format!(
                "/cluster/self/repository/active/{}/taskHistory?limit={limit}",
                encode(repo)
            ))
            .await?;
        Ok(v.as_array().cloned().unwrap_or_default())
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        // A user who can't read the cluster (`/pools/default`) has no
        // backup role either: the same note as the service's own 403.
        let (has, enterprise) = match self.backup_service().await {
            Ok(v) => v,
            Err(e) if denied(&e) => {
                return Ok(JobList {
                    jobs: Vec::new(),
                    notes: vec![DENIED.into()],
                })
            }
            Err(e) => return Err(e),
        };
        if !has {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![if enterprise { NO_SERVICE } else { COMMUNITY }.into()],
            });
        }
        let repos = match self.backup_get("/cluster/self/repository/active").await {
            Ok(v) => v.as_array().cloned().unwrap_or_default(),
            Err(e) if denied(&e) => {
                return Ok(JobList {
                    jobs: Vec::new(),
                    notes: vec![DENIED.into()],
                })
            }
            Err(e) => return Err(e),
        };
        let mut plans: HashMap<String, Option<Value>> = HashMap::new();
        let mut jobs = Vec::new();
        let mut notes = vec![SCOPE.to_string()];
        for (i, r) in repos.iter().enumerate() {
            let Some(name) = s(r, "id").filter(|n| repo_name(n)) else {
                continue;
            };
            let plan_name = s(r, "plan_name").unwrap_or_default();
            if !plans.contains_key(&plan_name) {
                let p = if plan_name.is_empty() {
                    None
                } else {
                    self.plan(&plan_name).await
                };
                plans.insert(plan_name.clone(), p);
            }
            let last = if i < MAX_LAST_RUNS {
                self.history(&name, 1)
                    .await
                    .ok()
                    .and_then(|h| h.into_iter().next())
            } else {
                None
            };
            if let Some(j) = job_of(r, plans[&plan_name].as_ref(), last.as_ref()) {
                jobs.push(j);
            }
        }
        if repos.len() > MAX_LAST_RUNS {
            notes.push(format!(
                "Se muestra la última ejecución de los primeros {MAX_LAST_RUNS} repositorios."
            ));
        }
        Ok(JobList { jobs, notes })
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        let name = parse_id(id)?;
        let repo = self.repo(&name).await?;
        let plan = match s(&repo, "plan_name") {
            Some(p) => self.plan(&p).await,
            None => None,
        };
        let last = self
            .history(&name, 1)
            .await
            .ok()
            .and_then(|h| h.into_iter().next());
        detail_of(&repo, plan.as_ref(), last.as_ref()).ok_or_else(|| not_found(&name))
    }

    pub(crate) async fn server_job_runs(&mut self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let name = parse_id(id)?;
        let repo = self.repo(&name).await?;
        let limit = limit.min(MAX_JOB_RUNS);
        let plan = match s(&repo, "plan_name") {
            Some(p) => self.plan(&p).await,
            None => None,
        };
        let scheduled: Vec<String> = tasks(plan.as_ref())
            .iter()
            .filter_map(|t| s(t, "name"))
            .collect();
        let mut runs: Vec<JobRun> = running_tasks(&repo)
            .into_iter()
            .map(|t| {
                let mut r = run_of(t, &scheduled);
                r.outcome = JobOutcome::Running;
                r.duration_ms = None;
                r
            })
            .collect();
        runs.extend(
            self.history(&name, limit)
                .await?
                .iter()
                .map(|t| run_of(t, &scheduled)),
        );
        runs.truncate(limit as usize);
        Ok(runs)
    }

    pub(crate) async fn run_server_job(&mut self, id: &str) -> Result<()> {
        if self.conn.read_only {
            return Err(Error::Query(
                "Conexión de solo lectura: no se pueden ejecutar jobs del servidor.".into(),
            ));
        }
        let name = parse_id(id)?;
        self.repo(&name).await?;
        self.backup_post(
            &format!("/cluster/self/repository/active/{}/backup", encode(&name)),
            json!({ "full_backup": false }),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn set_server_job_enabled(&mut self, id: &str, enabled: bool) -> Result<()> {
        if self.conn.read_only {
            return Err(Error::Query(
                "Conexión de solo lectura: no se pueden habilitar ni deshabilitar jobs del servidor."
                    .into(),
            ));
        }
        let name = parse_id(id)?;
        self.repo(&name).await?;
        let verb = if enabled { "resume" } else { "pause" };
        self.backup_post(
            &format!("/cluster/self/repository/active/{}/{verb}", encode(&name)),
            json!({}),
        )
        .await
        .map(|_| ())
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        let name = parse_id(id)?;
        let repo = self.repo(&name).await?;
        let plan = match s(&repo, "plan_name") {
            Some(p) => self.plan(&p).await,
            None => None,
        };
        script_of(&repo, plan.as_ref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> Value {
        json!({"id": "nightly", "plan_name": "p1", "state": "active", "archive": "/backups",
            "repo": "c796-uuid", "bucket": {"name": "sales", "uuid": "f07c"},
            "scheduled": {
                "full": {"name": "full", "task_type": "BACKUP", "next_run": "2026-11-12T18:00:29.5Z"},
                "merge": {"name": "merge", "task_type": "MERGE", "next_run": "2026-11-12T17:00:00-03:00"}
            },
            "health": {"healthy": true}, "creation_time": "2026-11-12T16:15:29.40645089Z"})
    }

    fn plan() -> Value {
        json!({"name": "p1", "description": "cada hora", "services": ["data"], "default": false, "tasks": [
            {"name": "full", "task_type": "BACKUP", "full_backup": true,
             "schedule": {"job_type": "BACKUP", "frequency": 1, "period": "HOURS", "time": "00:00"}},
            {"name": "merge", "task_type": "MERGE", "full_backup": false, "merge_options": {"offset_start": 0, "offset_end": 6},
             "schedule": {"job_type": "MERGE", "frequency": 4, "period": "SUNDAY", "time": "23:50"}}
        ]})
    }

    fn last() -> Value {
        json!({"task_name": "BACKUP-20d2", "status": "done", "start": "2026-11-12T16:15:33.395942004Z",
            "end": "2026-11-12T16:15:36.128948097Z", "error_code": 0, "type": "BACKUP",
            "node_runs": [{"node_id": "9701", "status": "done", "start": "2026-11-12T16:15:33.396Z",
                           "end": "2026-11-12T16:15:36.115Z", "progress": 100}]})
    }

    #[test]
    fn repositories_become_jobs() {
        let j = job_of(&repo(), Some(&plan()), Some(&last())).unwrap();
        assert_eq!(j.id, "repo:nightly");
        assert_eq!(j.kind, "Backup repository");
        assert_eq!(j.group.as_deref(), Some("sales"));
        assert_eq!(j.description.as_deref(), Some("cada hora"));
        assert_eq!(
            j.schedule.as_deref(),
            Some("full: 1 HOURS 00:00; merge: 4 SUNDAY 23:50")
        );
        // The earliest next run, converted to UTC.
        assert_eq!(j.next_run.as_deref(), Some("2026-11-12T18:00:29Z"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T16:15:33Z"));
        assert_eq!(j.last_duration_ms, Some(2733));
        assert!(j.enabled && !j.running && j.can_run && j.can_enable && !j.can_stop);
        assert!(j.has_history && j.has_script && !j.system);

        let mut paused = repo();
        paused["state"] = json!("paused");
        paused["running_one_off"] = json!({"BACKUP-1": {"task_name": "BACKUP-1", "status": "running", "start": "2026-11-12T16:16:00Z"}});
        let j = job_of(&paused, None, None).unwrap();
        assert!(!j.enabled && j.running && j.schedule.is_none());
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));
        // A run in progress doesn't hide how the last finished one ended.
        let mut failed = last();
        failed["status"] = json!("failed");
        let j = job_of(&paused, None, Some(&failed)).unwrap();
        assert!(j.running);
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert!(job_of(&json!({"id": "../x"}), None, None).is_none());
    }

    #[test]
    fn detail_and_runs() {
        let d = detail_of(&repo(), Some(&plan()), Some(&last())).unwrap();
        assert_eq!(d.steps.len(), 2);
        assert_eq!(d.steps[0].kind.as_deref(), Some("Backup completo"));
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some("1 HOURS 00:00 (próxima: 2026-11-12T18:00:29Z)")
        );
        assert_eq!(d.steps[1].kind.as_deref(), Some("MERGE"));
        assert!(d.steps[1]
            .command
            .as_deref()
            .unwrap()
            .contains("offset_end"));
        assert_eq!(
            d.schedules,
            ["full: 1 HOURS 00:00", "merge: 4 SUNDAY 23:50"]
        );
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Archivo" && p.value == "/backups"));

        let r = run_of(&last(), &["full".into()]);
        assert_eq!(r.id.as_deref(), Some("BACKUP-20d2"));
        assert_eq!(r.trigger.as_deref(), Some("manual"));
        assert_eq!(r.outcome, JobOutcome::Succeeded);
        assert_eq!(r.steps[0].name, "Nodo 9701");
        assert_eq!(r.steps[0].duration_ms, Some(2719));
        let failed = json!({"task_name": "full", "status": "failed", "error": "disk full",
            "start": "2026-11-12T16:00:00Z", "end": "0001-01-01T00:00:00Z"});
        let r = run_of(&failed, &["full".into()]);
        assert_eq!(r.trigger.as_deref(), Some("programada"));
        assert_eq!(r.outcome, JobOutcome::Failed);
        assert_eq!(r.message.as_deref(), Some("disk full"));
        assert_eq!(r.duration_ms, None, "the zero time isn't an end");
    }

    #[test]
    fn script_has_the_plan_and_the_repository() {
        let sc = script_of(&repo(), Some(&plan())).unwrap();
        assert!(sc.contains("POST /api/v1/plan/p1\n{"));
        assert!(sc.contains("\"offset_end\": 6"));
        assert!(sc.contains("POST /api/v1/cluster/self/repository/active/nightly\n{"));
        assert!(sc.contains("\"bucket_name\": \"sales\""));
        assert!(!sc.contains("\"default\""));
        let mut builtin = plan();
        builtin["default"] = json!(true);
        let sc = script_of(&repo(), Some(&builtin)).unwrap();
        assert!(sc.contains("viene con Couchbase") && !sc.contains("POST /api/v1/plan/"));
    }

    #[test]
    fn ids_have_one_shape() {
        assert_eq!(parse_id("repo:nightly-1.v2").unwrap(), "nightly-1.v2");
        for bad in [
            "",
            "nightly",
            "repo:",
            "repo:..",
            "repo:a/b",
            "repo:a b",
            "repo:a?x=1",
            "repo:a%2Fb",
            "repo:x\n",
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn times() {
        assert_eq!(epoch_ms("1970-01-01T00:00:01.5Z"), Some(1500));
        assert_eq!(epoch_ms("1970-01-01T01:00:00+01:00"), Some(0));
        assert_eq!(epoch_ms("0001-01-01T00:00:00Z"), None);
        assert_eq!(rfc3339(1_794_500_133_395), "2026-11-12T16:15:33Z");
    }
}
