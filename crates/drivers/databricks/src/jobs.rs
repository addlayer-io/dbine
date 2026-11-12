//! Server jobs ([`dbine_driver::Session::jobs`] and the calls that follow
//! it): the workspace's jobs, through the Jobs API 2.1 with the
//! connection's own credentials (a browser sign-in asks for `all-apis`,
//! which covers it).
//!
//! A job fires on its schedule, its trigger (file arrival, table update,
//! periodic) or continuously; each of those has a `pause_status`, which is
//! what enabling and disabling change. Its tasks are its steps. The list's
//! last outcomes come from the workspace's newest runs (a few pages of
//! `runs/list`, not a call per job); the detail and the history read the
//! job's own runs.
//!
//! The id is the numeric job id, checked before it goes into a URL or a
//! body.

use crate::{DatabricksSession, Json};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, JobStepRun, PropertyInfo, Result,
    ServerJob,
};
use serde_json::json;
use std::collections::HashMap;

/// The engine's word for them, shown as a tag.
const KIND: &str = "Job";
/// Jobs per page of `jobs/list` (its maximum).
const LIST_PAGE: usize = 100;
/// Jobs the list reads at most.
const MAX_JOBS: usize = 5000;
/// Runs per page of `runs/list` (its maximum).
const RUNS_PAGE: usize = 25;
/// Pages of the workspace's finished runs the list reads for the last
/// outcomes, at most.
const RECENT_PAGES: usize = 8;
/// Pages of the workspace's active runs the list reads, at most.
const ACTIVE_PAGES: usize = 4;

/// A job id as the API gives it: a positive integer.
pub(crate) fn job_id(id: &str) -> Option<u64> {
    let id = id.trim();
    if id.is_empty() || id.len() > 19 || !id.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    id.parse::<u64>().ok().filter(|&n| n > 0)
}

pub(crate) fn checked(id: &str) -> Result<u64> {
    job_id(id)
        .ok_or_else(|| Error::Query(format!("«{}» no es un id de job de Databricks", id.trim())))
}

fn text(v: &Json, ptr: &str) -> Option<String> {
    match v.pointer(ptr)? {
        Json::String(s) => Some(s.trim().to_string()),
        Json::Null => None,
        x => Some(x.to_string()),
    }
    .filter(|s| !s.is_empty())
}

fn num(v: &Json, ptr: &str) -> Option<i64> {
    match v.pointer(ptr)? {
        Json::String(s) => s.trim().parse().ok(),
        x => x.as_i64(),
    }
}

fn strings(v: &Json, ptr: &str) -> Vec<String> {
    v.pointer(ptr)
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| match x {
            Json::String(s) => Some(s.clone()),
            Json::Null => None,
            x => Some(x.to_string()),
        })
        .collect()
}

/// Epoch ms (the API's times) → RFC 3339 in UTC; 0 is "not yet".
pub(crate) fn rfc3339(ms: i64) -> Option<String> {
    (ms > 0).then(|| {
        format!(
            "{}Z",
            crate::monitor::datetime((ms / 1000) as u64).replacen(' ', "T", 1)
        )
    })
}

/// A trigger as the list says it.
fn trigger_text(t: &Json) -> String {
    if let Some(p) = t.get("periodic") {
        let n = text(p, "/interval").unwrap_or_default();
        return match text(p, "/unit").as_deref() {
            Some("HOURS") => format!("cada {n} horas"),
            Some("DAYS") => format!("cada {n} días"),
            Some("WEEKS") => format!("cada {n} semanas"),
            Some(u) => format!("cada {n} {u}"),
            None => format!("cada {n}"),
        };
    }
    if let Some(f) = t.get("file_arrival") {
        return format!(
            "al llegar archivos a {}",
            text(f, "/url").unwrap_or_default()
        );
    }
    if let Some(tb) = t.get("table_update").or_else(|| t.get("table")) {
        return format!("al actualizarse {}", strings(tb, "/table_names").join(", "));
    }
    // A kind of trigger this version doesn't know: its name.
    t.as_object()
        .and_then(|o| o.keys().find(|k| *k != "pause_status").cloned())
        .unwrap_or_else(|| "disparador".into())
}

/// The job's schedule as the list shows it, whether it's paused, and
/// whether it has something to pause (a schedule, a trigger or continuous
/// mode); a job without any only runs on demand or from another job.
fn schedule_of(s: &Json) -> (Option<String>, bool, bool) {
    let paused = |v: &Json| v.get("pause_status").and_then(Json::as_str) == Some("PAUSED");
    if let Some(sc) = s.get("schedule").filter(|v| v.is_object()) {
        let shown = match (
            text(sc, "/quartz_cron_expression"),
            text(sc, "/timezone_id"),
        ) {
            (Some(c), Some(tz)) => Some(format!("{c} ({tz})")),
            (c, _) => c,
        };
        return (shown, paused(sc), true);
    }
    if let Some(t) = s.get("trigger").filter(|v| v.is_object()) {
        return (Some(trigger_text(t)), paused(t), true);
    }
    if let Some(c) = s.get("continuous").filter(|v| v.is_object()) {
        return (Some("continuo".into()), paused(c), true);
    }
    (None, false, false)
}

/// A job of `jobs/list` or `jobs/get`.
pub(crate) fn server_job(j: &Json) -> Option<ServerJob> {
    let id = text(j, "/job_id")?;
    job_id(&id)?;
    let s = j.get("settings").unwrap_or(&Json::Null);
    let (schedule, paused, can_enable) = schedule_of(s);
    Some(ServerJob {
        name: text(s, "/name").unwrap_or_else(|| id.clone()),
        id,
        kind: KIND.into(),
        group: None,
        description: text(s, "/description"),
        owner: text(j, "/creator_user_name"),
        enabled: !paused,
        schedule,
        can_run: true,
        can_stop: true,
        can_enable,
        has_history: true,
        has_script: true,
        ..Default::default()
    })
}

/// How a run (or a task's run) went.
fn outcome(r: &Json) -> JobOutcome {
    match text(r, "/state/life_cycle_state")
        .or_else(|| text(r, "/status/state"))
        .as_deref()
    {
        Some("QUEUED" | "PENDING" | "RUNNING" | "TERMINATING" | "BLOCKED") => {
            return JobOutcome::Running
        }
        Some("WAITING_FOR_RETRY" | "WAITING") => return JobOutcome::Retrying,
        Some("SKIPPED") => return JobOutcome::Skipped,
        Some("INTERNAL_ERROR") => return JobOutcome::Failed,
        _ => {}
    }
    if let Some(rs) = text(r, "/state/result_state") {
        return match rs.as_str() {
            "SUCCESS" | "SUCCESS_WITH_FAILURES" => JobOutcome::Succeeded,
            "FAILED" => JobOutcome::Failed,
            "TIMEDOUT" | "CANCELED" => JobOutcome::Canceled,
            // A task that never ran because one before it failed or was
            // canceled (only tasks end like this, never a job's run).
            "UPSTREAM_FAILED" | "UPSTREAM_CANCELED" => JobOutcome::Skipped,
            "EXCLUDED" | "DISABLED" | "MAXIMUM_CONCURRENT_RUNS_REACHED" => JobOutcome::Skipped,
            _ => JobOutcome::Unknown,
        };
    }
    match text(r, "/status/termination_details/code").as_deref() {
        Some("SUCCESS") => JobOutcome::Succeeded,
        Some("CANCELED" | "USER_CANCELED") => JobOutcome::Canceled,
        Some("SKIPPED") => JobOutcome::Skipped,
        Some(_) => JobOutcome::Failed,
        None => JobOutcome::Unknown,
    }
}

fn active(o: JobOutcome) -> bool {
    matches!(o, JobOutcome::Running | JobOutcome::Retrying)
}

/// How long a finished run took, ms.
fn duration(r: &Json) -> Option<u64> {
    if let Some(d) = num(r, "/run_duration").filter(|&d| d > 0) {
        return Some(d as u64);
    }
    let parts: i64 = [
        "/setup_duration",
        "/execution_duration",
        "/cleanup_duration",
    ]
    .iter()
    .filter_map(|k| num(r, k))
    .filter(|&d| d > 0)
    .sum();
    if parts > 0 {
        return Some(parts as u64);
    }
    let (start, end) = (num(r, "/start_time")?, num(r, "/end_time")?);
    (start > 0 && end >= start).then(|| (end - start) as u64)
}

/// What started a run (`trigger`).
fn trigger(t: &str) -> String {
    match t {
        "PERIODIC" | "CRON" => "programada",
        "ONE_TIME" => "manual",
        "RETRY" => "reintento",
        "RUN_JOB_TASK" => "otro job",
        "FILE_ARRIVAL" => "llegada de archivos",
        "TABLE" => "actualización de tabla",
        "CONTINUOUS" | "CONTINUOUS_RESTART" => "continuo",
        other => other,
    }
    .to_string()
}

fn message(r: &Json) -> Option<String> {
    text(r, "/state/state_message").or_else(|| text(r, "/status/termination_details/message"))
}

fn step_run(t: &Json) -> JobStepRun {
    let outcome = outcome(t);
    JobStepRun {
        name: text(t, "/task_key").unwrap_or_default(),
        outcome,
        started: num(t, "/start_time").and_then(rfc3339),
        duration_ms: (!active(outcome)).then(|| duration(t)).flatten(),
        message: message(t),
    }
}

fn job_run(r: &Json) -> JobRun {
    let outcome = outcome(r);
    JobRun {
        id: text(r, "/run_id"),
        started: num(r, "/start_time").and_then(rfc3339),
        duration_ms: (!active(outcome)).then(|| duration(r)).flatten(),
        outcome,
        trigger: text(r, "/trigger").map(|t| trigger(&t)),
        message: message(r),
        steps: r
            .get("tasks")
            .and_then(Json::as_array)
            .map(|ts| ts.iter().map(step_run).collect())
            .unwrap_or_default(),
    }
}

/// The running state and the last outcome from a job's newest runs (newest
/// first).
fn apply_runs(job: &mut ServerJob, runs: &[Json]) {
    let running = runs.iter().find(|r| active(outcome(r)));
    let finished = runs.iter().find(|r| !active(outcome(r)));
    job.running = running.is_some();
    if let Some(f) = finished {
        job.last_outcome = Some(outcome(f));
        job.last_duration_ms = duration(f);
        job.last_started = num(f, "/start_time").and_then(rfc3339);
    } else if job.running {
        job.last_outcome = Some(JobOutcome::Running);
    }
    if let Some(r) = running {
        job.last_started = num(r, "/start_time").and_then(rfc3339);
    }
}

pub(crate) fn next_token(page: &Json) -> Option<String> {
    let more = page.get("has_more").and_then(Json::as_bool).unwrap_or(true);
    page.get("next_page_token")
        .and_then(Json::as_str)
        .filter(|t| more && !t.is_empty())
        .map(str::to_string)
}

/// The task types, with the name the workspace gives them.
const TASK_KINDS: &[(&str, &str)] = &[
    ("notebook_task", "Notebook"),
    ("spark_python_task", "Python script"),
    ("python_wheel_task", "Python wheel"),
    ("spark_jar_task", "JAR"),
    ("spark_submit_task", "Spark Submit"),
    ("pipeline_task", "Pipeline"),
    ("sql_task", "SQL"),
    ("dbt_task", "dbt"),
    ("run_job_task", "Run Job"),
    ("condition_task", "If/else condition"),
    ("for_each_task", "For each"),
    ("dashboard_task", "Dashboard"),
    ("clean_rooms_notebook_task", "Clean Rooms notebook"),
    ("power_bi_task", "Power BI"),
];

fn condition_op(op: &str) -> &str {
    match op {
        "EQUAL_TO" => "==",
        "NOT_EQUAL" => "!=",
        "GREATER_THAN" => ">",
        "GREATER_THAN_OR_EQUAL" => ">=",
        "LESS_THAN" => "<",
        "LESS_THAN_OR_EQUAL" => "<=",
        other => other,
    }
}

/// What a task runs, by its type.
fn command(key: &str, b: &Json) -> Option<String> {
    match key {
        "notebook_task" => text(b, "/notebook_path"),
        "spark_python_task" => text(b, "/python_file"),
        "python_wheel_task" => match (text(b, "/package_name"), text(b, "/entry_point")) {
            (Some(p), Some(e)) => Some(format!("{p}:{e}")),
            (p, e) => p.or(e),
        },
        "spark_jar_task" => text(b, "/main_class_name"),
        "spark_submit_task" => Some(strings(b, "/parameters").join(" ")).filter(|s| !s.is_empty()),
        "pipeline_task" => text(b, "/pipeline_id"),
        "sql_task" => text(b, "/file/path")
            .or_else(|| text(b, "/query/query_id").map(|q| format!("consulta {q}")))
            .or_else(|| text(b, "/dashboard/dashboard_id").map(|d| format!("dashboard {d}")))
            .or_else(|| text(b, "/alert/alert_id").map(|a| format!("alerta {a}"))),
        "dbt_task" => Some(strings(b, "/commands").join("; ")).filter(|s| !s.is_empty()),
        "run_job_task" => text(b, "/job_id").map(|j| format!("job {j}")),
        "condition_task" => Some(format!(
            "{} {} {}",
            text(b, "/left").unwrap_or_default(),
            condition_op(&text(b, "/op").unwrap_or_default()),
            text(b, "/right").unwrap_or_default()
        )),
        "for_each_task" => text(b, "/inputs"),
        "dashboard_task" => text(b, "/dashboard_id"),
        "clean_rooms_notebook_task" => text(b, "/notebook_name"),
        _ => None,
    }
}

/// A job's task as a step.
fn step(t: &Json) -> JobStep {
    let typed = TASK_KINDS
        .iter()
        .find(|(k, _)| t.get(*k).is_some())
        .map(|(k, label)| (k.to_string(), label.to_string()))
        .or_else(|| {
            // A type this version doesn't know: its field's name.
            t.as_object()?
                .keys()
                .find(|k| k.ends_with("_task"))
                .map(|k| (k.clone(), k.trim_end_matches("_task").to_string()))
        });
    let body = typed
        .as_ref()
        .and_then(|(k, _)| t.get(k))
        .unwrap_or(&Json::Null);
    let target = text(body, "/warehouse_id")
        .map(|w| format!("SQL warehouse {w}"))
        .or_else(|| text(t, "/existing_cluster_id").map(|c| format!("clúster {c}")))
        .or_else(|| text(t, "/job_cluster_key").map(|c| format!("clúster del job {c}")))
        .or_else(|| t.get("new_cluster").map(|_| "clúster nuevo".to_string()))
        .or_else(|| text(t, "/environment_key").map(|e| format!("serverless ({e})")));
    JobStep {
        name: text(t, "/task_key").unwrap_or_default(),
        command: typed.as_ref().and_then(|(k, _)| command(k, body)),
        kind: typed.map(|(_, label)| label),
        target,
    }
}

fn prop(p: &mut Vec<PropertyInfo>, group: &str, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        p.push(PropertyInfo {
            group: group.into(),
            label: label.into(),
            value,
        });
    }
}

fn properties(j: &Json) -> Vec<PropertyInfo> {
    let s = j.get("settings").unwrap_or(&Json::Null);
    let mut p = Vec::new();
    prop(&mut p, "", "Creador", text(j, "/creator_user_name"));
    prop(
        &mut p,
        "",
        "Ejecutar como",
        text(j, "/run_as_user_name")
            .or_else(|| text(s, "/run_as/user_name"))
            .or_else(|| text(s, "/run_as/service_principal_name")),
    );
    prop(
        &mut p,
        "",
        "Creado",
        num(j, "/created_time").and_then(rfc3339),
    );
    prop(
        &mut p,
        "",
        "Máximo de ejecuciones a la vez",
        text(s, "/max_concurrent_runs"),
    );
    prop(
        &mut p,
        "",
        "Tiempo máximo (s)",
        num(s, "/timeout_seconds")
            .filter(|&t| t > 0)
            .map(|t| t.to_string()),
    );
    let mut tags: Vec<String> = s
        .get("tags")
        .and_then(Json::as_object)
        .into_iter()
        .flatten()
        .map(|(k, v)| match v.as_str() {
            Some("") | None => k.clone(),
            Some(v) => format!("{k}={v}"),
        })
        .collect();
    tags.sort();
    prop(&mut p, "", "Etiquetas", Some(tags.join(", ")));
    let params: Vec<String> = s
        .get("parameters")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|x| {
            let name = text(x, "/name")?;
            Some(match text(x, "/default") {
                Some(d) => format!("{name}={d}"),
                None => name,
            })
        })
        .collect();
    prop(&mut p, "", "Parámetros", Some(params.join(", ")));
    prop(
        &mut p,
        "",
        "Cola",
        s.pointer("/queue/enabled")
            .and_then(Json::as_bool)
            .map(|q| if q { "Sí" } else { "No" }.to_string()),
    );
    prop(
        &mut p,
        "",
        "Repositorio Git",
        text(s, "/git_source/git_url").map(|u| {
            match text(s, "/git_source/git_branch")
                .or_else(|| text(s, "/git_source/git_tag"))
                .or_else(|| text(s, "/git_source/git_commit"))
            {
                Some(r) => format!("{u} ({r})"),
                None => u,
            }
        }),
    );
    prop(
        &mut p,
        "",
        "Correo al fallar",
        Some(strings(s, "/email_notifications/on_failure").join(", ")),
    );
    prop(&mut p, "", "Modo de edición", text(s, "/edit_mode"));
    for t in s
        .get("tasks")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
    {
        let deps: Vec<String> = t
            .get("depends_on")
            .and_then(Json::as_array)
            .into_iter()
            .flatten()
            .filter_map(|d| text(d, "/task_key"))
            .collect();
        if deps.is_empty() {
            continue;
        }
        let mut v = format!("después de {}", deps.join(", "));
        if let Some(when) = text(t, "/run_if").filter(|w| w != "ALL_SUCCESS") {
            v.push_str(&format!(" ({when})"));
        }
        prop(
            &mut p,
            "Dependencias",
            &text(t, "/task_key").unwrap_or_default(),
            Some(v),
        );
    }
    for c in s
        .get("job_clusters")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
    {
        let parts: Vec<String> = [
            text(c, "/new_cluster/spark_version"),
            text(c, "/new_cluster/node_type_id"),
        ]
        .into_iter()
        .flatten()
        .collect();
        prop(
            &mut p,
            "Clústeres del job",
            &text(c, "/job_cluster_key").unwrap_or_default(),
            Some(parts.join(" · ")),
        );
    }
    p
}

/// The `jobs/update` body that pauses or resumes what fires the job (each of
/// its schedule, trigger and continuous mode, whole: `new_settings` replaces
/// the top-level fields it names). `None`: the job has none of them.
fn pause_update(id: u64, s: &Json, paused: bool) -> Option<Json> {
    let mut new = serde_json::Map::new();
    for k in ["schedule", "trigger", "continuous"] {
        if let Some(mut v) = s.get(k).filter(|v| v.is_object()).cloned() {
            v["pause_status"] = json!(if paused { "PAUSED" } else { "UNPAUSED" });
            new.insert(k.into(), v);
        }
    }
    (!new.is_empty()).then(|| json!({ "job_id": id, "new_settings": new }))
}

impl DatabricksSession {
    /// One page of `runs/list` with `params`: its runs and the next page's
    /// token.
    pub(crate) async fn runs_page(
        &self,
        params: &[(&str, String)],
        token: Option<String>,
    ) -> Result<(Vec<Json>, Option<String>)> {
        let mut q: Vec<(&str, String)> = params.to_vec();
        q.push(("limit", RUNS_PAGE.to_string()));
        if let Some(t) = token {
            q.push(("page_token", t));
        }
        let page = self.api.get_query("/api/2.1/jobs/runs/list", &q).await?;
        let runs = page
            .get("runs")
            .and_then(Json::as_array)
            .cloned()
            .unwrap_or_default();
        Ok((runs, next_token(&page)))
    }

    /// The running state and the last outcome of each job, from the
    /// workspace's newest runs; what's missing goes to `notes`.
    async fn fill_last_runs(&self, jobs: &mut [ServerJob], notes: &mut Vec<String>) {
        if jobs.is_empty() {
            return;
        }
        let at: HashMap<String, usize> = jobs
            .iter()
            .enumerate()
            .map(|(i, j)| (j.id.clone(), i))
            .collect();
        let index = |r: &Json| text(r, "/job_id").and_then(|id| at.get(&id).copied());
        let failed = |e: Error| format!("no se pudieron leer las ejecuciones de los jobs: {e}");
        let mut token = None;
        let mut active_cut = false;
        for page in 0..ACTIVE_PAGES {
            let (runs, next) = match self
                .runs_page(&[("active_only", "true".into())], token.take())
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    notes.push(failed(e));
                    return;
                }
            };
            for r in &runs {
                if let Some(i) = index(r) {
                    let j = &mut jobs[i];
                    if !j.running {
                        j.running = true;
                        j.last_started = num(r, "/start_time").and_then(rfc3339);
                    }
                }
            }
            match next {
                Some(t) => {
                    token = Some(t);
                    active_cut = page + 1 == ACTIVE_PAGES;
                }
                None => break,
            }
        }
        if active_cut {
            notes.push(format!(
                "el estado en curso se muestra para las primeras {} ejecuciones activas del workspace",
                ACTIVE_PAGES * RUNS_PAGE
            ));
        }
        let mut seen = vec![false; jobs.len()];
        let mut left = jobs.len();
        let mut cut = false;
        let mut token = None;
        for page in 0..RECENT_PAGES {
            let (runs, next) = match self
                .runs_page(&[("completed_only", "true".into())], token.take())
                .await
            {
                Ok(p) => p,
                Err(e) => {
                    notes.push(failed(e));
                    break;
                }
            };
            // Newest first: a job's first run here is its last one.
            for r in &runs {
                let Some(i) = index(r) else { continue };
                if seen[i] {
                    continue;
                }
                seen[i] = true;
                left -= 1;
                let j = &mut jobs[i];
                j.last_outcome = Some(outcome(r));
                j.last_duration_ms = duration(r);
                if !j.running {
                    j.last_started = num(r, "/start_time").and_then(rfc3339);
                }
            }
            match next {
                Some(t) if left > 0 => {
                    token = Some(t);
                    cut = page + 1 == RECENT_PAGES;
                }
                _ => break,
            }
        }
        for j in jobs.iter_mut() {
            if j.running && j.last_outcome.is_none() {
                j.last_outcome = Some(JobOutcome::Running);
            }
        }
        if cut {
            notes.push(format!(
                "la última ejecución se muestra para los jobs que están entre las {} ejecuciones más recientes del workspace",
                RECENT_PAGES * RUNS_PAGE
            ));
        }
    }

    pub(crate) async fn jobs_list(&self) -> Result<JobList> {
        let mut jobs = Vec::new();
        let mut notes = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![("limit", LIST_PAGE.to_string())];
            if let Some(t) = token.take() {
                q.push(("page_token", t));
            }
            let page = match self.api.get_query("/api/2.1/jobs/list", &q).await {
                Ok(p) => p,
                // The connection works (its SQL warehouse answered): what's
                // refused is the Jobs API for these credentials.
                Err(Error::AuthFailed(m)) if jobs.is_empty() => {
                    notes.push(format!(
                        "Databricks no deja ver los jobs del workspace con estas credenciales: {m}"
                    ));
                    return Ok(JobList { jobs, notes });
                }
                Err(e) => return Err(e),
            };
            jobs.extend(
                page.get("jobs")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(server_job),
            );
            match next_token(&page) {
                Some(_) if jobs.len() >= MAX_JOBS => {
                    notes.push(format!(
                        "se muestran los primeros {MAX_JOBS} jobs del workspace"
                    ));
                    break;
                }
                Some(t) => token = Some(t),
                None => break,
            }
        }
        if jobs.is_empty() {
            notes.push("no hay jobs en el workspace que este usuario pueda ver".into());
        } else if !self.workspace_admin().await {
            // jobs/list gives only the jobs the caller may view.
            notes.push("se ven solo los jobs que este usuario puede ver".into());
        }
        self.fill_last_runs(&mut jobs, &mut notes).await;
        Ok(JobList { jobs, notes })
    }

    /// A workspace admin can view every job: SCIM `Me` lists `admins`.
    pub(crate) async fn workspace_admin(&self) -> bool {
        let me = self.api.get("/api/2.0/preview/scim/v2/Me");
        match tokio::time::timeout(std::time::Duration::from_secs(20), me).await {
            Ok(Ok(me)) => crate::permissions::is_admin(&me),
            Ok(Err(e)) => {
                tracing::debug!("databricks jobs (SCIM Me): {e}");
                false
            }
            Err(_) => false,
        }
    }

    pub(crate) async fn job_json(&self, id: u64) -> Result<Json> {
        self.api
            .get_query("/api/2.1/jobs/get", &[("job_id", id.to_string())])
            .await
    }

    pub(crate) async fn job_info(&self, id: &str) -> Result<JobDetail> {
        let n = checked(id)?;
        let j = self.job_json(n).await?;
        let mut job = server_job(&j)
            .ok_or_else(|| Error::Query(format!("Databricks no devolvió el job {n}")))?;
        // Its newest runs: whether one is going and how the last one ended.
        match self.runs_page(&[("job_id", n.to_string())], None).await {
            Ok((runs, _)) => apply_runs(&mut job, &runs),
            Err(e) => tracing::debug!("databricks: runs of job {n} not read: {e}"),
        }
        let s = j.get("settings").unwrap_or(&Json::Null);
        Ok(JobDetail {
            job,
            steps: s
                .get("tasks")
                .and_then(Json::as_array)
                .map(|ts| ts.iter().map(step).collect())
                .unwrap_or_default(),
            schedules: Vec::new(),
            properties: properties(&j),
            notes: Vec::new(),
        })
    }

    pub(crate) async fn job_history(&self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let n = checked(id)?;
        let limit = limit.min(MAX_JOB_RUNS) as usize;
        let mut out = Vec::new();
        let mut token = None;
        while out.len() < limit {
            let (runs, next) = self
                .runs_page(
                    &[("job_id", n.to_string()), ("expand_tasks", "true".into())],
                    token.take(),
                )
                .await?;
            out.extend(runs.iter().map(job_run));
            match next {
                Some(t) => token = Some(t),
                None => break,
            }
        }
        out.truncate(limit);
        Ok(out)
    }

    /// `run-now`: answers once the run is queued, with its id.
    pub(crate) async fn job_run_now(&self, id: &str) -> Result<()> {
        let n = checked(id)?;
        self.api
            .post("/api/2.1/jobs/run-now", &json!({ "job_id": n }))
            .await
            .map(|_| ())
    }

    /// Cancels every active run of the job (asynchronously, on the server).
    pub(crate) async fn job_cancel_all(&self, id: &str) -> Result<()> {
        let n = checked(id)?;
        self.api
            .post("/api/2.1/jobs/runs/cancel-all", &json!({ "job_id": n }))
            .await
            .map(|_| ())
    }

    pub(crate) async fn job_pause(&self, id: &str, paused: bool) -> Result<()> {
        let n = checked(id)?;
        let j = self.job_json(n).await?;
        let body = pause_update(n, j.get("settings").unwrap_or(&Json::Null), paused)
            .ok_or_else(|| {
                Error::Query(format!(
                    "el job {n} no tiene programación, disparador ni modo continuo que habilitar o deshabilitar"
                ))
            })?;
        self.api
            .post("/api/2.1/jobs/update", &body)
            .await
            .map(|_| ())
    }

    /// The job's settings: the body of `jobs/create` that makes it again.
    pub(crate) async fn job_settings_json(&self, id: &str) -> Result<String> {
        let n = checked(id)?;
        let j = self.job_json(n).await?;
        let s = j
            .get("settings")
            .ok_or_else(|| Error::Query(format!("Databricks no devolvió el job {n}")))?;
        Ok(serde_json::to_string_pretty(s)? + "\n")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::transfer::tests::{serve, session};
    use dbine_driver::Session;
    use std::sync::{Arc, Mutex};

    /// A scheduled job with four tasks, as `jobs/get` gives it.
    fn job_get() -> Json {
        json!({
            "job_id": 11,
            "creator_user_name": "ana@example.com",
            "run_as_user_name": "etl@example.com",
            "created_time": 1762916400000_i64,
            "settings": {
                "name": "carga diaria",
                "description": "ventas al lakehouse",
                "max_concurrent_runs": 1,
                "timeout_seconds": 3600,
                "tags": { "equipo": "datos", "pii": "" },
                "schedule": { "quartz_cron_expression": "0 0 3 * * ?", "timezone_id": "UTC", "pause_status": "UNPAUSED" },
                "email_notifications": { "on_failure": ["ana@example.com"] },
                "parameters": [{ "name": "fecha", "default": "hoy" }],
                "job_clusters": [{ "job_cluster_key": "jc", "new_cluster": { "spark_version": "15.4.x-scala2.12", "node_type_id": "i3.xlarge" } }],
                "tasks": [
                    { "task_key": "ingesta", "notebook_task": { "notebook_path": "/Repos/etl/ingesta" }, "job_cluster_key": "jc" },
                    { "task_key": "modelo", "depends_on": [{ "task_key": "ingesta" }],
                      "sql_task": { "file": { "path": "/sql/modelo.sql" }, "warehouse_id": "abc123" } },
                    { "task_key": "hay_datos", "depends_on": [{ "task_key": "modelo" }], "run_if": "ALL_DONE",
                      "condition_task": { "op": "GREATER_THAN", "left": "{{tasks.modelo.values.n}}", "right": "0" } },
                    { "task_key": "aviso", "depends_on": [{ "task_key": "hay_datos", "outcome": "true" }],
                      "run_job_task": { "job_id": 12 } },
                    { "task_key": "nuevo", "gen_ai_compute_task": { "dl_runtime_image": "x" }, "environment_key": "default" }
                ]
            }
        })
    }

    fn run(id: i64, job: i64, life: &str, result: Option<&str>, start: i64, dur: i64) -> Json {
        let mut state = json!({ "life_cycle_state": life, "state_message": "" });
        if let Some(r) = result {
            state["result_state"] = json!(r);
            if r == "FAILED" {
                state["state_message"] = json!("Workload failed, see run output for details");
            }
        }
        json!({ "run_id": id, "job_id": job, "state": state, "start_time": start,
                "run_duration": dur, "trigger": "PERIODIC",
                "tasks": [
                    { "task_key": "ingesta", "run_id": id * 10, "state": { "life_cycle_state": life, "result_state": result.unwrap_or("") },
                      "start_time": start, "execution_duration": dur / 2 },
                ] })
    }

    #[test]
    fn ids_are_checked() {
        assert_eq!(job_id(" 1234 "), Some(1234));
        for bad in [
            "",
            "0",
            "-1",
            "1e3",
            "12 34",
            "../2.1/jobs",
            "99999999999999999999",
            "1; DROP",
        ] {
            assert_eq!(job_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_job_becomes_a_server_job_with_its_steps() {
        let j = job_get();
        let job = server_job(&j).unwrap();
        assert_eq!(job.id, "11");
        assert_eq!(job.name, "carga diaria");
        assert_eq!(job.kind, "Job");
        assert_eq!(job.description.as_deref(), Some("ventas al lakehouse"));
        assert_eq!(job.owner.as_deref(), Some("ana@example.com"));
        assert_eq!(job.schedule.as_deref(), Some("0 0 3 * * ? (UTC)"));
        assert!(job.enabled && job.can_run && job.can_stop && job.can_enable);
        assert!(job.has_history && job.has_script);
        let steps: Vec<JobStep> = j["settings"]["tasks"]
            .as_array()
            .unwrap()
            .iter()
            .map(step)
            .collect();
        type Shown<'a> = (&'a str, Option<&'a str>, Option<&'a str>, Option<&'a str>);
        let shown: Vec<Shown> = steps
            .iter()
            .map(|s| {
                (
                    s.name.as_str(),
                    s.kind.as_deref(),
                    s.target.as_deref(),
                    s.command.as_deref(),
                )
            })
            .collect();
        assert_eq!(
            shown,
            [
                (
                    "ingesta",
                    Some("Notebook"),
                    Some("clúster del job jc"),
                    Some("/Repos/etl/ingesta")
                ),
                (
                    "modelo",
                    Some("SQL"),
                    Some("SQL warehouse abc123"),
                    Some("/sql/modelo.sql")
                ),
                (
                    "hay_datos",
                    Some("If/else condition"),
                    None,
                    Some("{{tasks.modelo.values.n}} > 0")
                ),
                ("aviso", Some("Run Job"), None, Some("job 12")),
                (
                    "nuevo",
                    Some("gen_ai_compute"),
                    Some("serverless (default)"),
                    None
                ),
            ]
        );
        let p = properties(&j);
        let get = |l: &str| p.iter().find(|x| x.label == l).map(|x| x.value.as_str());
        assert_eq!(get("Creado"), Some("2025-11-12T03:00:00Z"));
        assert_eq!(get("Ejecutar como"), Some("etl@example.com"));
        assert_eq!(get("Etiquetas"), Some("equipo=datos, pii"));
        assert_eq!(get("Parámetros"), Some("fecha=hoy"));
        assert_eq!(get("hay_datos"), Some("después de modelo (ALL_DONE)"));
        assert_eq!(get("jc"), Some("15.4.x-scala2.12 · i3.xlarge"));
        assert_eq!(get("Cola"), None);
        let queued = properties(&json!({ "settings": { "queue": { "enabled": true } } }));
        assert_eq!(queued[0].label, "Cola");
        assert_eq!(queued[0].value, "Sí");
    }

    #[test]
    fn triggers_and_pauses() {
        let paused = json!({ "job_id": 1, "settings": { "trigger": { "pause_status": "PAUSED", "file_arrival": { "url": "s3://b/in/" } } } });
        let j = server_job(&paused).unwrap();
        assert_eq!(
            j.schedule.as_deref(),
            Some("al llegar archivos a s3://b/in/")
        );
        assert!(!j.enabled && j.can_enable);
        let periodic = json!({ "periodic": { "interval": 4, "unit": "HOURS" } });
        assert_eq!(trigger_text(&periodic), "cada 4 horas");
        let manual =
            server_job(&json!({ "job_id": "2", "settings": { "name": "a mano" } })).unwrap();
        assert!(manual.enabled && !manual.can_enable && manual.schedule.is_none());
        let cont = json!({ "continuous": { "pause_status": "UNPAUSED" } });
        assert_eq!(schedule_of(&cont), (Some("continuo".into()), false, true));
        // Pausing sends the whole object back.
        let body = pause_update(1, &paused["settings"], false).unwrap();
        assert_eq!(
            body,
            json!({ "job_id": 1, "new_settings": { "trigger": { "pause_status": "UNPAUSED", "file_arrival": { "url": "s3://b/in/" } } } })
        );
        assert!(pause_update(2, &json!({ "name": "a mano" }), true).is_none());
        assert!(server_job(&json!({ "job_id": "x/../1" })).is_none());
    }

    #[test]
    fn runs_map_states_times_and_tasks() {
        let ok = job_run(&run(
            5,
            11,
            "TERMINATED",
            Some("SUCCESS"),
            1762916400000,
            65000,
        ));
        assert_eq!(ok.id.as_deref(), Some("5"));
        assert_eq!(ok.outcome, JobOutcome::Succeeded);
        assert_eq!(ok.started.as_deref(), Some("2025-11-12T03:00:00Z"));
        assert_eq!(ok.duration_ms, Some(65000));
        assert_eq!(ok.trigger.as_deref(), Some("programada"));
        assert_eq!(ok.message, None);
        assert_eq!(ok.steps.len(), 1);
        assert_eq!(ok.steps[0].name, "ingesta");
        assert_eq!(ok.steps[0].duration_ms, Some(32500));
        let bad = job_run(&run(
            6,
            11,
            "TERMINATED",
            Some("FAILED"),
            1762916400000,
            1000,
        ));
        assert_eq!(bad.outcome, JobOutcome::Failed);
        assert_eq!(
            bad.message.as_deref(),
            Some("Workload failed, see run output for details")
        );
        let going = job_run(&run(7, 11, "RUNNING", None, 1762916400000, 0));
        assert_eq!(
            (going.outcome, going.duration_ms),
            (JobOutcome::Running, None)
        );
        for (life, result, want) in [
            ("TERMINATED", "TIMEDOUT", JobOutcome::Canceled),
            (
                "TERMINATED",
                "MAXIMUM_CONCURRENT_RUNS_REACHED",
                JobOutcome::Skipped,
            ),
            ("WAITING_FOR_RETRY", "", JobOutcome::Retrying),
            ("INTERNAL_ERROR", "", JobOutcome::Failed),
            ("SKIPPED", "", JobOutcome::Skipped),
            // Tasks that never ran because one before them didn't end well.
            ("TERMINATED", "UPSTREAM_FAILED", JobOutcome::Skipped),
            ("TERMINATED", "UPSTREAM_CANCELED", JobOutcome::Skipped),
        ] {
            let r = json!({ "state": { "life_cycle_state": life, "result_state": result } });
            assert_eq!(outcome(&r), want, "{life} {result}");
        }
        let newer = json!({ "status": { "state": "TERMINATED", "termination_details": { "code": "CANCELED", "message": "a mano" } } });
        assert_eq!(outcome(&newer), JobOutcome::Canceled);
        assert_eq!(message(&newer).as_deref(), Some("a mano"));
    }

    /// A workspace: jobs 11 and 12 (two pages), job 12 running now, both with
    /// finished runs. Records what was asked.
    async fn workspace(seen: Arc<Mutex<Vec<(String, String, String)>>>) -> String {
        serve(Arc::new(move |m, p, b| {
            seen.lock()
                .unwrap()
                .push((m.to_string(), p.to_string(), b.to_string()));
            let (path, query) = p.split_once('?').unwrap_or((p, ""));
            let body = match (m, path) {
                ("GET", "/api/2.1/jobs/list") if query.contains("page_token=next%2B1") => {
                    json!({ "jobs": [{ "job_id": 12, "settings": { "name": "otro" } }], "has_more": false })
                }
                ("GET", "/api/2.1/jobs/list") => json!({
                    "jobs": [job_get()], "has_more": true, "next_page_token": "next+1"
                }),
                ("GET", "/api/2.1/jobs/get") if query == "job_id=11" => job_get(),
                ("GET", "/api/2.0/preview/scim/v2/Me") => json!({
                    "userName": "ana@example.com",
                    "groups": [{ "display": "users" }, { "display": "admins" }]
                }),
                ("GET", "/api/2.1/jobs/runs/list") if query.contains("active_only=true") => {
                    json!({ "runs": [run(9, 12, "RUNNING", None, 1762920000000, 0)], "has_more": false })
                }
                ("GET", "/api/2.1/jobs/runs/list") if query.contains("completed_only=true") => json!({
                    "runs": [
                        run(8, 11, "TERMINATED", Some("FAILED"), 1762916400000, 1500),
                        run(7, 12, "TERMINATED", Some("SUCCESS"), 1762900000000, 2000),
                        run(6, 11, "TERMINATED", Some("SUCCESS"), 1762800000000, 1000),
                    ],
                    "has_more": false
                }),
                ("GET", "/api/2.1/jobs/runs/list") if query.contains("job_id=11") => {
                    let page2 = query.contains("page_token=p2");
                    let base = if page2 { 100 } else { 200 };
                    let runs: Vec<Json> = (0..25)
                        .map(|i| run(base - i, 11, "TERMINATED", Some("SUCCESS"), 1762916400000 - (200 - base + i) * 1000, 10))
                        .collect();
                    if page2 {
                        json!({ "runs": runs, "has_more": false })
                    } else {
                        json!({ "runs": runs, "has_more": true, "next_page_token": "p2" })
                    }
                }
                ("POST", "/api/2.1/jobs/run-now") => json!({ "run_id": 99, "number_in_job": 99 }),
                ("POST", "/api/2.1/jobs/runs/cancel-all") | ("POST", "/api/2.1/jobs/update") => json!({}),
                _ => return (404, json!({ "error_code": "NOT_FOUND", "message": "no" }).to_string()),
            };
            (200, body.to_string())
        }))
        .await
    }

    #[tokio::test]
    async fn the_list_pages_and_reads_the_newest_runs() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut s = session(workspace(seen.clone()).await);
        let list = s.jobs().await.unwrap();
        assert!(list.notes.is_empty(), "{:?}", list.notes);
        let ids: Vec<&str> = list.jobs.iter().map(|j| j.id.as_str()).collect();
        assert_eq!(ids, ["11", "12"]);
        let (a, b) = (&list.jobs[0], &list.jobs[1]);
        assert_eq!(a.last_outcome, Some(JobOutcome::Failed), "the newest run");
        assert_eq!(a.last_duration_ms, Some(1500));
        assert!(!a.running);
        assert!(b.running);
        assert_eq!(b.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(
            b.last_started.as_deref(),
            Some("2025-11-12T04:00:00Z"),
            "the run going on"
        );
        assert!(!b.can_enable);
        let asked: Vec<String> = seen.lock().unwrap().iter().map(|c| c.1.clone()).collect();
        assert!(asked[0].starts_with("/api/2.1/jobs/list?limit=100"));
        assert!(asked[1].contains("page_token=next%2B1"), "{asked:?}");
    }

    #[tokio::test]
    async fn a_partial_list_says_so() {
        // Not an admin, and more active runs than the list reads.
        let base = serve(Arc::new(|_, p, _| {
            let (path, query) = p.split_once('?').unwrap_or((p, ""));
            let body = match path {
                "/api/2.1/jobs/list" => json!({ "jobs": [job_get()], "has_more": false }),
                "/api/2.0/preview/scim/v2/Me" => {
                    json!({ "userName": "ana@example.com", "groups": [{ "display": "users" }] })
                }
                "/api/2.1/jobs/runs/list" if query.contains("active_only=true") => json!({
                    "runs": [run(1, 99, "RUNNING", None, 1762920000000, 0)],
                    "has_more": true, "next_page_token": "more"
                }),
                "/api/2.1/jobs/runs/list" => json!({ "runs": [], "has_more": false }),
                _ => return (404, json!({ "message": "no" }).to_string()),
            };
            (200, body.to_string())
        }))
        .await;
        let list = session(base).jobs().await.unwrap();
        assert_eq!(
            list.notes,
            [
                "se ven solo los jobs que este usuario puede ver",
                "el estado en curso se muestra para las primeras 100 ejecuciones activas del workspace",
            ]
        );
    }

    #[tokio::test]
    async fn the_detail_history_and_actions() {
        let seen = Arc::new(Mutex::new(Vec::new()));
        let mut s = session(workspace(seen.clone()).await);
        let d = s.job_detail("11").await.unwrap();
        assert_eq!(d.steps.len(), 5);
        assert_eq!(d.job.last_outcome, Some(JobOutcome::Succeeded));
        assert!(!d.job.running);

        let runs = s.job_runs("11", 30).await.unwrap();
        assert_eq!(runs.len(), 30, "two pages, cut at the limit");
        assert_eq!(runs[0].id.as_deref(), Some("200"));
        assert_eq!(runs[25].id.as_deref(), Some("100"));
        assert!(s.job_runs("11", 0).await.unwrap().is_empty());

        s.run_job("11").await.unwrap();
        s.stop_job("11").await.unwrap();
        s.set_job_enabled("11", false).await.unwrap();
        let script = s.job_script("11").await.unwrap();
        let back: Json = serde_json::from_str(&script).unwrap();
        assert_eq!(back, job_get()["settings"]);

        let calls = seen.lock().unwrap().clone();
        let posts: Vec<(&str, Json)> = calls
            .iter()
            .filter(|c| c.0 == "POST")
            .map(|c| (c.1.as_str(), serde_json::from_str(&c.2).unwrap()))
            .collect();
        assert_eq!(posts[0], ("/api/2.1/jobs/run-now", json!({ "job_id": 11 })));
        assert_eq!(
            posts[1],
            ("/api/2.1/jobs/runs/cancel-all", json!({ "job_id": 11 }))
        );
        assert_eq!(posts[2].0, "/api/2.1/jobs/update");
        assert_eq!(
            posts[2].1["new_settings"]["schedule"]["pause_status"],
            "PAUSED"
        );
        assert_eq!(
            posts[2].1["new_settings"]["schedule"]["quartz_cron_expression"],
            "0 0 3 * * ?"
        );

        let before = calls.len();
        for bad in ["11/../../2.0/sql", "11&job_id=12", "-11", "x"] {
            assert!(s.run_job(bad).await.is_err());
            assert!(s.stop_job(bad).await.is_err());
            assert!(s.set_job_enabled(bad, true).await.is_err());
            assert!(s.job_detail(bad).await.is_err());
            assert!(s.job_runs(bad, 5).await.is_err());
            assert!(s.job_script(bad).await.is_err());
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            before,
            "a bad id never reaches the server"
        );
    }

    #[tokio::test]
    async fn jobs_refused_by_the_workspace_are_a_note() {
        let base = serve(Arc::new(|_, _, _| {
            (
                403,
                json!({ "error_code": "PERMISSION_DENIED", "message": "Invalid scope" })
                    .to_string(),
            )
        }))
        .await;
        let list = session(base).jobs().await.unwrap();
        assert!(list.jobs.is_empty());
        assert_eq!(
            list.notes,
            ["Databricks no deja ver los jobs del workspace con estas credenciales: Invalid scope"]
        );
    }
}
