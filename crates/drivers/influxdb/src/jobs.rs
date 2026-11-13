//! Server jobs ([`dbine_driver::Session::jobs`]) per API:
//!
//! - 2.x: tasks (`/api/v2/tasks`). Each has its Flux, an `every` / `cron`
//!   schedule, a status (`active` / `inactive`) and its runs
//!   (`/api/v2/tasks/{id}/runs`, newest first, with their logs). "Run now"
//!   asks for a manual run (`POST …/runs`: the server queues it and answers),
//!   stopping cancels the running run (`DELETE …/runs/{runID}`), and enabling
//!   patches the status. An inactive task refuses manual runs.
//! - 1.x: continuous queries (`SHOW CONTINUOUS QUERIES`). They run by
//!   themselves at their `GROUP BY time()` / `RESAMPLE EVERY` interval and
//!   can't be run, stopped or paused. Their runs are recorded only with
//!   `query-stats-enabled` (in `_internal`'s `cq_query`).
//! - 3.x: the processing engine's triggers
//!   (`system.processing_engine_triggers` in each database), with their runs
//!   from `system.processing_engine_logs` (grouped by `run_id`), enabled and
//!   disabled through `/api/v3/configure/processing_engine_trigger/…`. There's
//!   no API to fire a scheduled trigger now.
//!
//! Ids: a 2.x task's own id (16 hex digits); for 1.x and 3.x, the database
//! and the name as two double-quoted identifiers (`"db"."name"`). Ids that
//! come back are checked against that shape, and names travel as URL
//! parameters or quoted strings, never pasted.

use crate::http::{self, send_err};
use crate::v1::{ident, InfluxQlSession};
use crate::v2::FluxSession;
use crate::v3::{jsonl_table, SqlSession};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, PropertyInfo, Result, ServerJob,
};
use serde_json::{json, Value as J};
use std::collections::BTreeMap;

/// Characters kept of a message.
const MAX_MESSAGE: usize = 4000;
/// Tasks whose newest runs the list reads at the same time.
const PARALLEL: usize = 16;

// ---------------------------------------------------------------- shared

/// `"db"."name"`.
pub fn pair_id(db: &str, name: &str) -> String {
    format!("{}.{}", ident(db), ident(name))
}

/// One double-quoted identifier at the start of `s` (as [`ident`] writes
/// it), and what follows.
fn unquote(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?.1),
            '"' => return Some((out, &rest[i + 1..])),
            c => out.push(c),
        }
    }
    None
}

/// The database and name of an id [`pair_id`] made; `None` for anything
/// else.
pub fn parse_pair(id: &str) -> Option<(String, String)> {
    if id.len() > 1024 {
        return None;
    }
    let (db, rest) = unquote(id)?;
    let (name, rest) = unquote(rest.strip_prefix('.')?)?;
    let ok = |s: &str| !s.is_empty() && !s.chars().any(char::is_control);
    (rest.is_empty() && ok(&db) && ok(&name)).then_some((db, name))
}

/// A 2.x id: 16 lower-case hex digits.
pub fn task_id(id: &str) -> Option<&str> {
    (id.len() == 16
        && id
            .bytes()
            .all(|b| b.is_ascii_digit() || (b'a'..=b'f').contains(&b)))
    .then_some(id)
}

pub(crate) fn bad_id(id: &str) -> Error {
    Error::Query(format!(
        "«{}» no es un id de job de este servidor",
        id.chars().take(200).collect::<String>()
    ))
}

pub(crate) fn s(v: Option<&J>) -> Option<String> {
    match v? {
        J::String(s) => Some(s.trim().to_string()).filter(|s| !s.is_empty()),
        J::Null => None,
        v => Some(v.to_string()),
    }
}

fn cut(s: &str) -> String {
    s.chars().take(MAX_MESSAGE).collect()
}

/// An RFC 3339 time (any offset, any precision) in UTC, to the second; a
/// zone-less one is taken as UTC.
pub fn utc(t: &str) -> Option<String> {
    let t = t.trim();
    let dt = chrono::DateTime::parse_from_rfc3339(t)
        .map(|d| d.with_timezone(&chrono::Utc))
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f").map(|d| d.and_utc())
        })
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f").map(|d| d.and_utc())
        })
        .ok()?;
    (dt.timestamp() > 0).then(|| dt.format("%Y-%m-%dT%H:%M:%SZ").to_string())
}

fn millis(t: &str) -> Option<i64> {
    let t = t.trim();
    chrono::DateTime::parse_from_rfc3339(t)
        .map(|d| d.timestamp_millis())
        .or_else(|_| {
            chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%dT%H:%M:%S%.f")
                .map(|d| d.and_utc().timestamp_millis())
        })
        .or_else(|_| {
            // As `v3::jsonl_table` shows a zone-less time.
            chrono::NaiveDateTime::parse_from_str(t, "%Y-%m-%d %H:%M:%S%.f")
                .map(|d| d.and_utc().timestamp_millis())
        })
        .ok()
}

fn between(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    Some((millis(end?)? - millis(start?)?).max(0) as u64)
}

fn add(p: &mut Vec<PropertyInfo>, group: &str, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        p.push(PropertyInfo {
            group: group.into(),
            label: label.into(),
            value,
        });
    }
}

// ---------------------------------------------------------------- 2.x

const V2_KIND: &str = "Task";

fn v2_outcome(status: &str) -> Option<JobOutcome> {
    match status {
        "success" => Some(JobOutcome::Succeeded),
        "failed" => Some(JobOutcome::Failed),
        "canceled" | "cancelled" => Some(JobOutcome::Canceled),
        "started" | "scheduled" => Some(JobOutcome::Running),
        _ => None,
    }
}

fn v2_going(run: &J) -> bool {
    matches!(
        run.get("status").and_then(J::as_str),
        Some("started" | "scheduled")
    )
}

/// The task's schedule: its cron, or `every` (and `offset`).
pub fn v2_schedule(t: &J) -> Option<String> {
    if let Some(c) = s(t.get("cron")) {
        return Some(c);
    }
    let every = s(t.get("every"))?;
    Some(match s(t.get("offset")) {
        Some(o) => format!("every {every} offset {o}"),
        None => format!("every {every}"),
    })
}

/// A task (`/api/v2/tasks`) and its newest runs (newest first) as a job.
pub fn v2_job(t: &J, users: &BTreeMap<String, String>, runs: &[J]) -> ServerJob {
    let running = runs.first().is_some_and(v2_going);
    let last = runs.iter().find(|r| !v2_going(r));
    let last_status = last
        .and_then(|r| s(r.get("status")))
        .or_else(|| s(t.get("lastRunStatus")));
    let enabled = t.get("status").and_then(J::as_str) == Some("active");
    ServerJob {
        id: s(t.get("id")).unwrap_or_default(),
        name: s(t.get("name")).unwrap_or_default(),
        kind: V2_KIND.into(),
        group: s(t.get("org")),
        description: s(t.get("description")),
        owner: s(t.get("ownerID")).and_then(|o| users.get(&o).cloned()),
        enabled,
        schedule: v2_schedule(t),
        running,
        last_outcome: last_status
            .as_deref()
            .and_then(v2_outcome)
            .or(running.then_some(JobOutcome::Running)),
        last_started: last
            .and_then(|r| s(r.get("startedAt")))
            .as_deref()
            .and_then(utc),
        last_duration_ms: last.and_then(|r| {
            between(
                s(r.get("startedAt")).as_deref(),
                s(r.get("finishedAt")).as_deref(),
            )
        }),
        next_run: None,
        // An inactive task refuses manual runs.
        can_run: enabled,
        can_stop: true,
        can_enable: true,
        has_history: true,
        has_script: true,
        system: false,
    }
}

/// The run's log lines, without the "started from script" and "completed"
/// ones.
fn v2_log(run: &J) -> Option<String> {
    let lines: Vec<String> = run
        .get("log")
        .and_then(J::as_array)
        .into_iter()
        .flatten()
        .filter_map(|l| s(l.get("message")))
        .filter(|m| !m.starts_with("Started task from script") && !m.starts_with("Completed("))
        .collect();
    (!lines.is_empty()).then(|| cut(&lines.join("\n")))
}

pub fn v2_run(r: &J) -> JobRun {
    let started = s(r.get("startedAt"));
    JobRun {
        id: s(r.get("id")),
        started: started
            .clone()
            .or_else(|| s(r.get("scheduledFor")))
            .as_deref()
            .and_then(utc),
        duration_ms: between(started.as_deref(), s(r.get("finishedAt")).as_deref()),
        outcome: s(r.get("status"))
            .as_deref()
            .and_then(v2_outcome)
            .unwrap_or_default(),
        trigger: Some(
            if s(r.get("requestedAt")).is_some() {
                "manual"
            } else {
                "programada"
            }
            .into(),
        ),
        message: v2_log(r),
        steps: Vec::new(),
    }
}

pub fn v2_detail(t: &J, users: &BTreeMap<String, String>, runs: &[J]) -> JobDetail {
    let mut p = Vec::new();
    add(&mut p, "", "Organización", s(t.get("org")));
    add(&mut p, "", "Cada (every)", s(t.get("every")));
    add(&mut p, "", "Desfase (offset)", s(t.get("offset")));
    add(&mut p, "", "Cron", s(t.get("cron")));
    add(
        &mut p,
        "",
        "Creada",
        s(t.get("createdAt")).as_deref().and_then(utc),
    );
    add(
        &mut p,
        "",
        "Modificada",
        s(t.get("updatedAt")).as_deref().and_then(utc),
    );
    add(
        &mut p,
        "Última ejecución",
        "Último horario completado",
        s(t.get("latestCompleted")).as_deref().and_then(utc),
    );
    add(
        &mut p,
        "Última ejecución",
        "Error",
        s(t.get("lastRunError")).map(|e| cut(&e)),
    );
    let job = v2_job(t, users, runs);
    JobDetail {
        steps: vec![JobStep {
            name: job.name.clone(),
            kind: Some("Flux".into()),
            target: None,
            command: s(t.get("flux")),
        }],
        job,
        schedules: Vec::new(),
        properties: p,
        notes: Vec::new(),
    }
}

impl FluxSession {
    fn task_url(&self, id: &str) -> Result<String> {
        let id = task_id(id).ok_or_else(|| bad_id(id))?;
        Ok(format!("{}/api/v2/tasks/{id}", self.base))
    }

    pub(crate) async fn task(&self, id: &str) -> Result<J> {
        let url = self.task_url(id)?;
        self.send(self.http.get(url)).await
    }

    async fn task_runs(&self, id: &str, limit: u32) -> Result<Vec<J>> {
        let url = format!("{}/runs", self.task_url(id)?);
        let limit = limit.clamp(1, MAX_JOB_RUNS).to_string();
        let v = self
            .send(self.http.get(url).query(&[("limit", limit.as_str())]))
            .await?;
        Ok(v.get("runs")
            .and_then(J::as_array)
            .cloned()
            .unwrap_or_default())
    }

    /// User ids → names (empty when the token can't list users).
    pub(crate) async fn user_names(&self) -> BTreeMap<String, String> {
        let Ok(v) = self.get("/api/v2/users", &[]).await else {
            return BTreeMap::new();
        };
        v.get("users")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
            .filter_map(|u| Some((s(u.get("id"))?, s(u.get("name"))?)))
            .collect()
    }

    /// Every task of the organization the token can read, page by page.
    pub(crate) async fn all_tasks(&self) -> Result<Vec<J>> {
        let mut tasks: Vec<J> = Vec::new();
        let mut after: Option<String> = None;
        loop {
            let mut q = vec![("org", self.org.as_str()), ("limit", "500")];
            if let Some(a) = after.as_deref() {
                q.push(("after", a));
            }
            let v = self.get("/api/v2/tasks", &q).await?;
            let page = v
                .get("tasks")
                .and_then(J::as_array)
                .cloned()
                .unwrap_or_default();
            let n = page.len();
            after = page.last().and_then(|t| s(t.get("id")));
            tasks.extend(page);
            if n < 500 || after.is_none() {
                break;
            }
        }
        Ok(tasks)
    }

    pub(crate) async fn jobs_list(&self) -> Result<JobList> {
        let tasks = self.all_tasks().await?;
        let users = self.user_names().await;
        // Each task's two newest runs: whether one is going, and the last
        // finished one.
        let mut runs: BTreeMap<String, Vec<J>> = BTreeMap::new();
        let ids: Vec<String> = tasks.iter().filter_map(|t| s(t.get("id"))).collect();
        for chunk in ids.chunks(PARALLEL) {
            let mut set = tokio::task::JoinSet::new();
            for id in chunk {
                let req = self
                    .http
                    .get(format!("{}/api/v2/tasks/{id}/runs", self.base))
                    .query(&[("limit", "2")])
                    .header("Authorization", format!("Token {}", self.token));
                let id = id.clone();
                set.spawn(async move {
                    let body = match req.send().await {
                        Ok(r) => http::text(r).await.ok(),
                        Err(_) => None,
                    };
                    let runs = body
                        .and_then(|b| serde_json::from_str::<J>(&b).ok())
                        .and_then(|v| v.get("runs").and_then(J::as_array).cloned())
                        .unwrap_or_default();
                    (id, runs)
                });
            }
            while let Some(Ok((id, r))) = set.join_next().await {
                runs.insert(id, r);
            }
        }
        let jobs = tasks
            .iter()
            .map(|t| {
                let r = s(t.get("id"))
                    .and_then(|id| runs.get(&id))
                    .map(Vec::as_slice)
                    .unwrap_or(&[]);
                v2_job(t, &users, r)
            })
            .filter(|j| !j.id.is_empty())
            .collect();
        Ok(JobList {
            jobs,
            notes: vec![format!(
                "se ven las tareas de la organización «{}» que este token puede leer",
                self.org
            )],
        })
    }

    pub(crate) async fn job_info(&self, id: &str) -> Result<JobDetail> {
        let t = self.task(id).await?;
        let runs = self.task_runs(id, 2).await.unwrap_or_default();
        Ok(v2_detail(&t, &self.user_names().await, &runs))
    }

    pub(crate) async fn job_history(&self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        Ok(self
            .task_runs(id, limit)
            .await?
            .iter()
            .map(v2_run)
            .collect())
    }

    pub(crate) async fn job_start(&self, id: &str) -> Result<()> {
        self.check_writable("Conexión de solo lectura: no se pueden ejecutar tareas.")?;
        let url = format!("{}/runs", self.task_url(id)?);
        self.send(self.http.post(url).json(&json!({}))).await?;
        Ok(())
    }

    pub(crate) async fn job_stop(&self, id: &str) -> Result<()> {
        self.check_writable("Conexión de solo lectura: no se pueden detener tareas.")?;
        let going: Vec<String> = self
            .task_runs(id, 20)
            .await?
            .iter()
            .filter(|r| v2_going(r))
            .filter_map(|r| s(r.get("id")))
            .filter(|r| task_id(r).is_some())
            .collect();
        if going.is_empty() {
            return Err(Error::Query(
                "la tarea no tiene una ejecución en curso".into(),
            ));
        }
        let base = self.task_url(id)?;
        for run in going {
            self.send(self.http.delete(format!("{base}/runs/{run}")))
                .await?;
        }
        Ok(())
    }

    pub(crate) async fn job_enable(&self, id: &str, enabled: bool) -> Result<()> {
        self.check_writable(
            "Conexión de solo lectura: no se pueden habilitar ni deshabilitar tareas.",
        )?;
        let url = self.task_url(id)?;
        let status = if enabled { "active" } else { "inactive" };
        self.send(self.http.patch(url).json(&json!({ "status": status })))
            .await?;
        Ok(())
    }

    pub(crate) async fn job_create_script(&self, id: &str) -> Result<String> {
        let t = self.task(id).await?;
        s(t.get("flux")).ok_or_else(|| Error::Query("la tarea no tiene Flux".into()))
    }
}

// ---------------------------------------------------------------- 1.x

const V1_KIND: &str = "Continuous query";
pub(crate) const V1_FIXED: &str = "InfluxDB 1.x ejecuta las continuous queries solas en cada intervalo: no se pueden ejecutar a mano, detener ni pausar";
pub(crate) const V1_NO_STATS: &str = "el historial de las continuous queries se guarda solo con query-stats-enabled en la sección [continuous_queries] de la configuración";
pub(crate) const V1_NO_ACCESS: &str =
    "este usuario no puede ver las continuous queries (SHOW CONTINUOUS QUERIES)";

/// What a `CREATE CONTINUOUS QUERY` says.
#[derive(Debug, Default, PartialEq)]
pub struct CqDef {
    pub resample_every: Option<String>,
    pub resample_for: Option<String>,
    pub group_by_time: Option<String>,
    pub into: Option<String>,
    pub select: Option<String>,
}

/// Reads a CQ's definition (as `SHOW CONTINUOUS QUERIES` writes it).
pub fn cq_def(q: &str) -> CqDef {
    let up = q.to_ascii_uppercase();
    let mut out = CqDef::default();
    let begin = up.find(" BEGIN ");
    let head = &up[..begin.unwrap_or(up.len())];
    if let Some(r) = head.find(" RESAMPLE ") {
        let words: Vec<&str> = q[r + 10..begin.unwrap_or(q.len())]
            .split_whitespace()
            .collect();
        let mut i = 0;
        while i + 1 < words.len() {
            match words[i].to_ascii_uppercase().as_str() {
                "EVERY" => out.resample_every = Some(words[i + 1].to_string()),
                "FOR" => out.resample_for = Some(words[i + 1].to_string()),
                _ => {}
            }
            i += 1;
        }
    }
    if let Some(b) = begin {
        let end = up.rfind(" END").filter(|e| *e > b).unwrap_or(q.len());
        let select = q[b + 7..end].trim();
        out.select = Some(select.to_string()).filter(|s| !s.is_empty());
        let sup = select.to_ascii_uppercase();
        if let Some(i) = sup.find(" INTO ") {
            out.into = select[i + 6..]
                .split_whitespace()
                .next()
                .map(str::to_string);
        }
        if let Some(i) = sup.find("GROUP BY ") {
            let rest = &sup[i..];
            if let Some(t) = rest.find("TIME(") {
                let from = i + t + 5;
                if let Some(close) = select[from..].find(')') {
                    out.group_by_time = Some(select[from..from + close].trim().to_string());
                }
            }
        }
    }
    out
}

impl CqDef {
    /// "EVERY 1h", or the RESAMPLE's.
    pub fn schedule(&self) -> Option<String> {
        let every = self.resample_every.clone().or_else(|| {
            self.group_by_time
                .as_deref()
                .map(|t| t.split(',').next().unwrap_or(t).trim().to_string())
        })?;
        Some(match &self.resample_for {
            Some(f) => format!("EVERY {every} FOR {f}"),
            None => format!("EVERY {every}"),
        })
    }
}

pub fn v1_job(db: &str, name: &str, query: &str, history: bool) -> ServerJob {
    ServerJob {
        id: pair_id(db, name),
        name: name.into(),
        kind: V1_KIND.into(),
        group: Some(db.into()),
        enabled: true,
        schedule: cq_def(query).schedule(),
        has_history: history,
        has_script: true,
        ..Default::default()
    }
}

/// `SHOW CONTINUOUS QUERIES` → (database, name, query).
pub fn v1_cqs(results: &[J]) -> Vec<(String, String, String)> {
    let mut out = Vec::new();
    for series in results
        .iter()
        .filter_map(|r| r.get("series").and_then(J::as_array))
        .flatten()
    {
        let Some(db) = s(series.get("name")) else {
            continue;
        };
        for v in series
            .get("values")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            let row = v.as_array().map(Vec::as_slice).unwrap_or(&[]);
            if let (Some(name), Some(q)) = (s(row.first()), s(row.get(1))) {
                out.push((db.clone(), name, q));
            }
        }
    }
    out
}

fn influxql_str(v: &str) -> String {
    format!("'{}'", http::escape(v, '\''))
}

/// `cq_query` rows (`_internal`): one per run, with `query-stats-enabled`.
/// The retention policy is named: the session sends its own (`USE db.rp`)
/// with every query, and InfluxDB would fill an empty one with it.
/// `monitor` is the one the monitor service writes `_internal` with.
pub fn v1_runs_sql(db: &str, cq: &str, limit: u32) -> String {
    format!(
        "SELECT \"durationNs\", \"pointsWrittenOK\" FROM \"_internal\".\"monitor\".\"cq_query\" WHERE \"db\" = {} AND \"cq\" = {} ORDER BY time DESC LIMIT {limit}",
        influxql_str(db),
        influxql_str(cq)
    )
}

pub fn v1_runs(results: &[J]) -> Vec<JobRun> {
    let mut out = Vec::new();
    for series in results
        .iter()
        .filter_map(|r| r.get("series").and_then(J::as_array))
        .flatten()
    {
        let cols: Vec<&str> = series
            .get("columns")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
            .filter_map(J::as_str)
            .collect();
        let at = |n: &str| cols.iter().position(|c| *c == n);
        for v in series
            .get("values")
            .and_then(J::as_array)
            .into_iter()
            .flatten()
        {
            let row = v.as_array().map(Vec::as_slice).unwrap_or(&[]);
            let get = |n: &str| at(n).and_then(|i| row.get(i));
            out.push(JobRun {
                started: s(get("time")).as_deref().and_then(utc),
                // Nanoseconds.
                duration_ms: get("durationNs")
                    .and_then(J::as_f64)
                    .map(|ns| (ns.max(0.0) / 1e6) as u64),
                outcome: JobOutcome::Succeeded,
                trigger: Some("programada".into()),
                message: get("pointsWrittenOK")
                    .and_then(J::as_i64)
                    .map(|n| format!("{n} puntos escritos")),
                ..Default::default()
            });
        }
    }
    out
}

impl InfluxQlSession {
    /// The runs are recorded (`query-stats-enabled` wrote `cq_query`).
    pub(crate) async fn cq_stats(&mut self) -> bool {
        self.query("SHOW MEASUREMENTS ON \"_internal\" WITH MEASUREMENT = \"cq_query\"")
            .await
            .is_ok_and(|r| {
                r.iter().any(|x| {
                    x.get("series")
                        .and_then(J::as_array)
                        .is_some_and(|s| !s.is_empty())
                })
            })
    }

    pub(crate) async fn cqs(&mut self) -> Result<Vec<(String, String, String)>> {
        Ok(v1_cqs(&self.query("SHOW CONTINUOUS QUERIES").await?))
    }

    pub(crate) async fn cq(&mut self, id: &str) -> Result<(String, String, String)> {
        let (db, name) = parse_pair(id).ok_or_else(|| bad_id(id))?;
        self.cqs()
            .await?
            .into_iter()
            .find(|(d, n, _)| *d == db && *n == name)
            .ok_or_else(|| Error::Query(format!("no existe la continuous query {name} en {db}")))
    }

    pub(crate) async fn jobs_list(&mut self) -> Result<JobList> {
        let cqs = match self.cqs().await {
            Ok(c) => c,
            Err(Error::Query(m)) | Err(Error::AuthFailed(m))
                if m.contains("privilege") || m.contains("authoriz") =>
            {
                return Ok(JobList {
                    jobs: Vec::new(),
                    notes: vec![V1_NO_ACCESS.into()],
                })
            }
            Err(e) => return Err(e),
        };
        let history = self.cq_stats().await;
        let jobs: Vec<ServerJob> = cqs
            .iter()
            .map(|(db, name, q)| v1_job(db, name, q, history))
            .collect();
        let mut notes = vec![V1_FIXED.to_string()];
        if !history && !jobs.is_empty() {
            notes.push(V1_NO_STATS.into());
        }
        Ok(JobList { jobs, notes })
    }

    pub(crate) async fn job_info(&mut self, id: &str) -> Result<JobDetail> {
        let (db, name, q) = self.cq(id).await?;
        let history = self.cq_stats().await;
        let def = cq_def(&q);
        let mut p = Vec::new();
        add(&mut p, "", "Base de datos", Some(db.clone()));
        add(
            &mut p,
            "",
            "Intervalo (GROUP BY time)",
            def.group_by_time.clone(),
        );
        add(&mut p, "", "RESAMPLE EVERY", def.resample_every.clone());
        add(&mut p, "", "RESAMPLE FOR", def.resample_for.clone());
        add(&mut p, "", "Destino (INTO)", def.into.clone());
        Ok(JobDetail {
            job: v1_job(&db, &name, &q, history),
            steps: vec![JobStep {
                name: name.clone(),
                kind: Some("InfluxQL".into()),
                target: def.into.clone(),
                command: def.select.clone(),
            }],
            schedules: Vec::new(),
            properties: p,
            notes: Vec::new(),
        })
    }

    pub(crate) async fn job_history(&mut self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let (db, name, _) = self.cq(id).await?;
        if !self.cq_stats().await {
            return Err(Error::Unsupported(V1_NO_STATS.into()));
        }
        let limit = limit.clamp(1, MAX_JOB_RUNS);
        Ok(v1_runs(&self.query(&v1_runs_sql(&db, &name, limit)).await?))
    }

    pub(crate) async fn job_create_script(&mut self, id: &str) -> Result<String> {
        let (_, _, q) = self.cq(id).await?;
        Ok(format!("{};\n", q.trim_end_matches(';')))
    }
}

// ---------------------------------------------------------------- 3.x

const V3_KIND: &str = "Trigger";
const V3_NONE: &str =
    "este servidor no tiene triggers del motor de procesamiento (processing engine)";
pub(crate) const V3_OLD: &str =
    "esta versión de InfluxDB 3 no informa los triggers del motor de procesamiento";
pub(crate) const V3_NO_RUN: &str =
    "InfluxDB 3 no permite disparar un trigger a mano ni detener una ejecución";

/// A Go-style duration from seconds ("1h30m", "5s").
fn duration_text(secs: u64) -> String {
    if secs == 0 {
        return "0s".into();
    }
    let mut out = String::new();
    for (unit, n) in [("d", 86_400), ("h", 3600), ("m", 60), ("s", 1)] {
        if secs / n > 0 && (unit != "d" || secs.is_multiple_of(86_400)) {
            out.push_str(&format!("{}{unit}", secs / n));
            return if secs.is_multiple_of(n) {
                out
            } else {
                out + &duration_text(secs % n)
            };
        }
    }
    out
}

/// `trigger_specification` as `influxdb3 create trigger --trigger-spec`
/// takes it (`every:5s`, `cron:0 0 3 * * *`, `table:cpu`, `all_tables`,
/// `request:path`). Older servers already store that text.
pub fn trigger_spec(raw: &str) -> String {
    let Ok(v) = serde_json::from_str::<J>(raw) else {
        return raw.to_string();
    };
    if let Some(s) = v.as_str() {
        return match s {
            "all_tables_wal_write" => "all_tables".into(),
            s => s.into(),
        };
    }
    if let Some(d) = v.pointer("/every/duration") {
        return format!(
            "every:{}",
            duration_text(d.get("secs").and_then(J::as_u64).unwrap_or(0))
        );
    }
    if let Some(c) = v.pointer("/schedule/schedule").and_then(J::as_str) {
        return format!("cron:{c}");
    }
    if let Some(t) = v
        .pointer("/single_table_wal_write/table_name")
        .and_then(J::as_str)
    {
        return format!("table:{t}");
    }
    if let Some(p) = v.pointer("/request_path/path").and_then(J::as_str) {
        return format!("request:{p}");
    }
    raw.to_string()
}

/// The schedule of a timed trigger; `None` for the ones that fire on
/// writes or requests.
fn trigger_schedule(spec: &str) -> Option<String> {
    (spec.starts_with("every:") || spec.starts_with("cron:")).then(|| spec.to_string())
}

/// What fires it, in Spanish.
pub(crate) fn trigger_fires(spec: &str) -> String {
    if let Some(t) = spec.strip_prefix("table:") {
        format!("Al escribir en la tabla {t}")
    } else if spec == "all_tables" {
        "Al escribir en cualquier tabla".into()
    } else if let Some(p) = spec.strip_prefix("request:") {
        format!("Con una petición a /api/v3/engine/{p}")
    } else if let Some(e) = spec.strip_prefix("every:") {
        format!("Cada {e}")
    } else if let Some(c) = spec.strip_prefix("cron:") {
        format!("Según el cron {c}")
    } else {
        spec.into()
    }
}

/// One run, from its `processing_engine_logs` rows grouped by `run_id`.
#[derive(Debug, Clone, PartialEq)]
pub struct LogRun {
    pub trigger: String,
    pub run_id: String,
    pub start: Option<String>,
    pub end: Option<String>,
    pub error: Option<String>,
    pub finished: bool,
}

/// The query that groups the logs by run (newest first).
pub fn v3_runs_sql(trigger: Option<&str>, limit: u32) -> String {
    let filter = trigger
        .map(|t| format!("WHERE trigger_name = '{}'", t.replace('\'', "''")))
        .unwrap_or_default();
    format!(
        "SELECT trigger_name, run_id, min(event_time) AS started, max(event_time) AS ended,
                max(CASE WHEN log_level = 'ERROR' THEN log_text END) AS error,
                max(CASE WHEN log_text LIKE 'finished execution%' THEN 1 ELSE 0 END) AS finished
         FROM system.processing_engine_logs {filter}
         GROUP BY trigger_name, run_id
         ORDER BY started DESC
         LIMIT {limit}"
    )
}

pub fn log_runs(cols: &[String], rows: &[Vec<J>]) -> Vec<LogRun> {
    let at = |n: &str| cols.iter().position(|c| c == n);
    rows.iter()
        .filter_map(|r| {
            let get = |n: &str| s(at(n).and_then(|i| r.get(i)));
            Some(LogRun {
                trigger: get("trigger_name")?,
                run_id: get("run_id")?,
                start: get("started"),
                end: get("ended"),
                error: get("error").map(|e| cut(&e)),
                finished: get("finished").is_some_and(|f| f != "0"),
            })
        })
        .collect()
}

/// What the history says of a run the engine logged no end for: a plugin
/// that raised an exception only logs its start (the error goes to the
/// server's log).
const V3_NO_END: &str = "InfluxDB no registró el fin de esta ejecución: sigue en curso o falló (el error queda en el log del servidor)";

impl LogRun {
    /// An `ERROR` line: failed; "finished execution": succeeded; neither:
    /// unknown.
    fn outcome(&self) -> JobOutcome {
        if self.error.is_some() {
            JobOutcome::Failed
        } else if self.finished {
            JobOutcome::Succeeded
        } else {
            JobOutcome::Unknown
        }
    }

    fn run(&self) -> JobRun {
        let outcome = self.outcome();
        JobRun {
            id: Some(self.run_id.clone()),
            duration_ms: if outcome == JobOutcome::Unknown {
                None
            } else {
                between(self.start.as_deref(), self.end.as_deref())
            },
            started: self.start.as_deref().and_then(utc),
            outcome,
            trigger: None,
            message: self
                .error
                .clone()
                .or_else(|| (outcome == JobOutcome::Unknown).then(|| V3_NO_END.into())),
            steps: Vec::new(),
        }
    }
}

/// The run the list shows for a trigger (its runs newest first): the newest
/// one that ended in a known way, else the newest.
pub fn last_run<'a>(runs: impl IntoIterator<Item = &'a LogRun>) -> Option<&'a LogRun> {
    let mut first = None;
    for r in runs {
        if r.outcome() != JobOutcome::Unknown {
            return Some(r);
        }
        first.get_or_insert(r);
    }
    first
}

/// A `system.processing_engine_triggers` row (by column name) as a job;
/// `last`: its newest run.
pub fn v3_job(db: &str, cols: &[String], row: &[J], last: Option<&LogRun>) -> ServerJob {
    let at = |n: &str| cols.iter().position(|c| c == n).and_then(|i| row.get(i));
    let name = s(at("trigger_name")).unwrap_or_default();
    let spec = trigger_spec(&s(at("trigger_specification")).unwrap_or_default());
    let disabled = matches!(at("disabled"), Some(J::Bool(true)))
        || s(at("disabled")).is_some_and(|d| d == "true");
    ServerJob {
        id: pair_id(db, &name),
        name,
        kind: V3_KIND.into(),
        group: Some(db.into()),
        description: s(at("plugin_filename")).map(|p| format!("Plugin {p}")),
        owner: None,
        enabled: !disabled,
        schedule: trigger_schedule(&spec),
        // The logs don't tell a run going on from one that died.
        running: false,
        last_outcome: last.map(LogRun::outcome),
        last_started: last.and_then(|l| l.start.as_deref()).and_then(utc),
        last_duration_ms: last.and_then(|l| l.run().duration_ms),
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: true,
        has_history: true,
        has_script: true,
        system: false,
    }
}

/// A single-quoted shell word.
fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// The `influxdb3 create trigger` command that makes the trigger again.
pub fn v3_script(db: &str, cols: &[String], row: &[J], args: &[(String, String)]) -> String {
    let at = |n: &str| s(cols.iter().position(|c| c == n).and_then(|i| row.get(i)));
    let mut cmd = format!(
        "influxdb3 create trigger \\\n  --database {} \\\n  --plugin-filename {} \\\n  --trigger-spec {}",
        sh(db),
        sh(&at("plugin_filename").unwrap_or_default()),
        sh(&trigger_spec(&at("trigger_specification").unwrap_or_default()))
    );
    if !args.is_empty() {
        let list: Vec<String> = args.iter().map(|(k, v)| format!("{k}={v}")).collect();
        cmd.push_str(&format!(
            " \\\n  --trigger-arguments {}",
            sh(&list.join(","))
        ));
    }
    if let Some(e) = at("error_behavior").filter(|e| e != "log") {
        cmd.push_str(&format!(" \\\n  --error-behavior {}", sh(&e)));
    }
    if at("disabled").is_some_and(|d| d == "true") {
        cmd.push_str(" \\\n  --disabled");
    }
    cmd.push_str(&format!(
        " \\\n  {}\n",
        sh(&at("trigger_name").unwrap_or_default())
    ));
    cmd
}

pub(crate) const TRIGGERS: &str =
    "SELECT * FROM system.processing_engine_triggers ORDER BY trigger_name";

impl SqlSession {
    pub(crate) async fn jobs_sql(&self, db: &str, q: &str) -> Result<(Vec<String>, Vec<Vec<J>>)> {
        let req = self
            .http
            .post(format!("{}/api/v3/query_sql", self.base))
            .json(&json!({ "db": db, "q": q, "format": "jsonl" }));
        let body = http::text(self.auth(req).send().await.map_err(send_err)?).await?;
        jsonl_table(&body)
    }

    /// The trigger's row, and the table's columns.
    pub(crate) async fn trigger(&self, id: &str) -> Result<(String, Vec<String>, Vec<J>)> {
        let (db, name) = parse_pair(id).ok_or_else(|| bad_id(id))?;
        let (cols, rows) = self.jobs_sql(&db, TRIGGERS).await?;
        let at = cols.iter().position(|c| c == "trigger_name");
        let row = rows
            .into_iter()
            .find(|r| at.and_then(|i| s(r.get(i))).as_deref() == Some(name.as_str()))
            .ok_or_else(|| Error::Query(format!("no existe el trigger {name} en {db}")))?;
        Ok((db, cols, row))
    }

    pub(crate) async fn trigger_args(&self, db: &str, name: &str) -> Vec<(String, String)> {
        let q = format!(
            "SELECT argument_key, argument_value FROM system.processing_engine_trigger_arguments WHERE trigger_name = '{}' ORDER BY argument_key",
            name.replace('\'', "''")
        );
        self.jobs_sql(db, &q)
            .await
            .map(|(_, rows)| {
                rows.iter()
                    .filter_map(|r| Some((s(r.first())?, s(r.get(1)).unwrap_or_default())))
                    .collect()
            })
            .unwrap_or_default()
    }

    pub(crate) async fn jobs_list(&self) -> Result<JobList> {
        let mut jobs = Vec::new();
        let mut old = false;
        for db in self.databases().await? {
            if db.starts_with('_') {
                continue;
            }
            let (cols, rows) = match self.jobs_sql(&db, TRIGGERS).await {
                Ok(t) => t,
                Err(Error::Query(m)) if m.contains("processing_engine") => {
                    old = true;
                    continue;
                }
                Err(e) => return Err(e),
            };
            if rows.is_empty() {
                continue;
            }
            let runs = self
                .jobs_sql(&db, &v3_runs_sql(None, 100_000))
                .await
                .map(|(c, r)| log_runs(&c, &r))
                .unwrap_or_default();
            let at = cols.iter().position(|c| c == "trigger_name");
            for row in &rows {
                let name = at.and_then(|i| s(row.get(i))).unwrap_or_default();
                let last = last_run(runs.iter().filter(|r| r.trigger == name));
                jobs.push(v3_job(&db, &cols, row, last));
            }
        }
        let mut notes = Vec::new();
        if old {
            notes.push(V3_OLD.to_string());
        } else if jobs.is_empty() {
            notes.push(V3_NONE.to_string());
        }
        Ok(JobList { jobs, notes })
    }

    pub(crate) async fn job_info(&self, id: &str) -> Result<JobDetail> {
        let (db, cols, row) = self.trigger(id).await?;
        let at = |n: &str| s(cols.iter().position(|c| c == n).and_then(|i| row.get(i)));
        let name = at("trigger_name").unwrap_or_default();
        let runs = self
            .jobs_sql(&db, &v3_runs_sql(Some(&name), 50))
            .await
            .map(|(c, r)| log_runs(&c, &r))
            .unwrap_or_default();
        let args = self.trigger_args(&db, &name).await;
        let spec = trigger_spec(&at("trigger_specification").unwrap_or_default());
        let mut p = Vec::new();
        add(&mut p, "", "Base de datos", Some(db.clone()));
        add(&mut p, "", "Se dispara", Some(trigger_fires(&spec)));
        add(&mut p, "", "Plugin", at("plugin_filename"));
        add(
            &mut p,
            "",
            "Si falla",
            at("error_behavior").map(|e| match e.as_str() {
                "log" => "Lo registra en el log".to_string(),
                "retry" => "Reintenta".to_string(),
                "disable" => "Deshabilita el trigger".to_string(),
                _ => e,
            }),
        );
        for (k, v) in &args {
            add(&mut p, "Argumentos", k, Some(v.clone()));
        }
        Ok(JobDetail {
            job: v3_job(&db, &cols, &row, last_run(&runs)),
            steps: vec![JobStep {
                name: name.clone(),
                kind: Some("Python".into()),
                target: None,
                command: at("plugin_filename"),
            }],
            schedules: Vec::new(),
            properties: p,
            notes: Vec::new(),
        })
    }

    pub(crate) async fn job_history(&self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let (db, name) = parse_pair(id).ok_or_else(|| bad_id(id))?;
        self.trigger(id).await?;
        let limit = limit.clamp(1, MAX_JOB_RUNS);
        let (cols, rows) = self.jobs_sql(&db, &v3_runs_sql(Some(&name), limit)).await?;
        Ok(log_runs(&cols, &rows).iter().map(LogRun::run).collect())
    }

    pub(crate) async fn job_enable(&self, id: &str, enabled: bool) -> Result<()> {
        let (db, name) = parse_pair(id).ok_or_else(|| bad_id(id))?;
        self.trigger(id).await?;
        let verb = if enabled { "enable" } else { "disable" };
        let req = self
            .http
            .post(format!(
                "{}/api/v3/configure/processing_engine_trigger/{verb}",
                self.base
            ))
            .query(&[("db", db.as_str()), ("trigger_name", name.as_str())]);
        self.configure(
            req,
            "Conexión de solo lectura: no se pueden habilitar ni deshabilitar triggers.",
        )
        .await
    }

    pub(crate) async fn job_create_script(&self, id: &str) -> Result<String> {
        let (db, cols, row) = self.trigger(id).await?;
        let name = s(cols
            .iter()
            .position(|c| c == "trigger_name")
            .and_then(|i| row.get(i)))
        .unwrap_or_default();
        let args = self.trigger_args(&db, &name).await;
        Ok(v3_script(&db, &cols, &row, &args))
    }
}

/// Why 3.x has no "run now" or "stop".
pub fn v3_no_run() -> Error {
    Error::Unsupported(V3_NO_RUN.into())
}

/// Why 1.x has no "run now", "stop" or enabling.
pub fn v1_fixed() -> Error {
    Error::Unsupported(V1_FIXED.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_are_checked() {
        let id = pair_id("my \"db\"", "cq\\1");
        assert_eq!(parse_pair(&id), Some(("my \"db\"".into(), "cq\\1".into())));
        assert_eq!(
            parse_pair("\"a\".\"b\""),
            Some(("a".to_string(), "b".to_string()))
        );
        for bad in [
            "",
            "a.b",
            "\"a\".\"b\"; DROP DATABASE x",
            "\"a\".\"b",
            "\"a\".\"\"",
            "\"a\"\"b\"",
            "\"a\".\"b\n\"",
        ] {
            assert_eq!(parse_pair(bad), None, "{bad}");
        }
        assert_eq!(task_id("119ff5d05e465000"), Some("119ff5d05e465000"));
        for bad in [
            "119ff5d05e46500",
            "119FF5D05E465000",
            "../../orgs/x",
            "119ff5d05e46500g",
        ] {
            assert_eq!(task_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn times_are_utc_seconds() {
        assert_eq!(
            utc("2026-11-12T13:02:09.569323156Z").as_deref(),
            Some("2026-11-12T13:02:09Z")
        );
        assert_eq!(
            utc("2026-11-12T10:02:09-03:00").as_deref(),
            Some("2026-11-12T13:02:09Z")
        );
        assert_eq!(
            utc("2026-11-12T13:04:00.016190468").as_deref(),
            Some("2026-11-12T13:04:00Z")
        );
        assert_eq!(utc("0001-01-01T00:00:00Z"), None);
        assert_eq!(
            between(
                Some("2026-11-12T13:02:09.569323156Z"),
                Some("2026-11-12T13:02:09.5914034Z")
            ),
            Some(22)
        );
    }

    #[test]
    fn tasks_become_jobs() {
        let t = serde_json::json!({
            "id": "119ff5d05e465000", "orgID": "d832b5ae3ab61b21", "org": "dbine",
            "ownerID": "1163ed1279d71000", "name": "nightly", "description": "probe",
            "status": "active",
            "flux": "option task = {name: \"nightly\", every: 1h, offset: 5m}\n\nfrom(bucket: \"test\")",
            "every": "1h", "offset": "5m", "latestCompleted": "2026-11-12T13:02:09Z",
            "lastRunStatus": "success", "createdAt": "2026-11-12T13:02:04Z"
        });
        let users = BTreeMap::from([("1163ed1279d71000".to_string(), "admin".to_string())]);
        let runs = [
            serde_json::json!({"id": "a", "status": "started", "startedAt": "2026-11-12T14:00:00.1Z"}),
            serde_json::json!({"id": "b", "status": "failed", "startedAt": "2026-11-12T13:02:09.569323156Z",
                "finishedAt": "2026-11-12T13:02:09.5914034Z", "requestedAt": "2026-11-12T13:02:09Z",
                "log": [{"message": "Started task from script: \"…\""}, {"message": "bucket not found"}, {"message": "Completed(failed)"}]}),
        ];
        let j = v2_job(&t, &users, &runs);
        assert_eq!(j.id, "119ff5d05e465000");
        assert_eq!(j.kind, "Task");
        assert_eq!(j.group.as_deref(), Some("dbine"));
        assert_eq!(j.owner.as_deref(), Some("admin"));
        assert_eq!(j.schedule.as_deref(), Some("every 1h offset 5m"));
        assert!(j.enabled && j.running && j.can_run && j.can_stop && j.can_enable);
        assert!(j.has_history && j.has_script);
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T13:02:09Z"));
        assert_eq!(j.last_duration_ms, Some(22));

        let r = v2_run(&runs[1]);
        assert_eq!(r.outcome, JobOutcome::Failed);
        assert_eq!(r.trigger.as_deref(), Some("manual"));
        assert_eq!(r.message.as_deref(), Some("bucket not found"));
        assert_eq!(v2_run(&runs[0]).outcome, JobOutcome::Running);
        assert_eq!(v2_run(&runs[0]).trigger.as_deref(), Some("programada"));

        let mut off = t.clone();
        off["status"] = "inactive".into();
        off["cron"] = "0 3 * * *".into();
        let j = v2_job(&off, &BTreeMap::new(), &[]);
        assert!(!j.enabled && !j.can_run && !j.running && j.owner.is_none());
        assert_eq!(j.schedule.as_deref(), Some("0 3 * * *"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        let d = v2_detail(&t, &users, &runs);
        assert_eq!(d.steps[0].kind.as_deref(), Some("Flux"));
        assert!(d.steps[0].command.as_deref().unwrap().contains("every: 1h"));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Desfase (offset)" && p.value == "5m"));
    }

    #[test]
    fn continuous_queries_are_read() {
        let r = serde_json::json!({"statement_id": 0, "series": [
            {"name": "_internal", "columns": ["name", "query"]},
            {"name": "db", "columns": ["name", "query"], "values": [
                ["cq1", "CREATE CONTINUOUS QUERY cq1 ON db RESAMPLE EVERY 30m FOR 2h BEGIN SELECT mean(v) INTO db.autogen.cpu_1h FROM db.autogen.cpu GROUP BY time(1h), * END"],
                ["cq2", "CREATE CONTINUOUS QUERY cq2 ON db BEGIN SELECT count(v) INTO db.autogen.cpu_5m FROM db.autogen.cpu GROUP BY time(5m, 1m) END"]
            ]}
        ]});
        let cqs = v1_cqs(&[r]);
        assert_eq!(cqs.len(), 2);
        let d = cq_def(&cqs[0].2);
        assert_eq!(d.resample_every.as_deref(), Some("30m"));
        assert_eq!(d.resample_for.as_deref(), Some("2h"));
        assert_eq!(d.group_by_time.as_deref(), Some("1h"));
        assert_eq!(d.into.as_deref(), Some("db.autogen.cpu_1h"));
        assert!(d.select.as_deref().unwrap().starts_with("SELECT mean(v)"));
        assert_eq!(d.schedule().as_deref(), Some("EVERY 30m FOR 2h"));
        assert_eq!(cq_def(&cqs[1].2).schedule().as_deref(), Some("EVERY 5m"));
        let j = v1_job("db", "cq1", &cqs[0].2, false);
        assert_eq!(j.id, "\"db\".\"cq1\"");
        assert_eq!(j.kind, "Continuous query");
        assert!(j.enabled && j.has_script);
        assert!(!j.can_run && !j.can_stop && !j.can_enable && !j.has_history);
        assert_eq!(
            v1_runs_sql("d'b", "c\\q", 5),
            "SELECT \"durationNs\", \"pointsWrittenOK\" FROM \"_internal\".\"monitor\".\"cq_query\" WHERE \"db\" = 'd\\'b' AND \"cq\" = 'c\\\\q' ORDER BY time DESC LIMIT 5"
        );
        let runs = v1_runs(&[serde_json::json!({"series": [{"name": "cq_query",
            "columns": ["time", "durationNs", "pointsWrittenOK"],
            "values": [["2026-11-12T13:00:00Z", 2_500_000, 12]]}]})]);
        assert_eq!(runs[0].started.as_deref(), Some("2026-11-12T13:00:00Z"));
        assert_eq!(runs[0].duration_ms, Some(2));
        assert_eq!(runs[0].message.as_deref(), Some("12 puntos escritos"));
    }

    #[test]
    fn trigger_specs_read_back() {
        assert_eq!(
            trigger_spec(r#"{"every":{"duration":{"secs":5400,"nanos":0}}}"#),
            "every:1h30m"
        );
        assert_eq!(
            trigger_spec(r#"{"every":{"duration":{"secs":172800,"nanos":0}}}"#),
            "every:2d"
        );
        assert_eq!(
            trigger_spec(r#"{"schedule":{"schedule":"0 0 3 * * *"}}"#),
            "cron:0 0 3 * * *"
        );
        assert_eq!(
            trigger_spec(r#"{"single_table_wal_write":{"table_name":"cpu"}}"#),
            "table:cpu"
        );
        assert_eq!(trigger_spec(r#""all_tables_wal_write""#), "all_tables");
        assert_eq!(
            trigger_spec(r#"{"request_path":{"path":"hook"}}"#),
            "request:hook"
        );
        assert_eq!(trigger_spec("every:5s"), "every:5s");
        assert_eq!(trigger_schedule("table:cpu"), None);
        assert_eq!(duration_text(90), "1m30s");
    }

    #[test]
    fn triggers_become_jobs() {
        let cols: Vec<String> = [
            "trigger_name",
            "plugin_filename",
            "trigger_specification",
            "disabled",
            "error_behavior",
        ]
        .map(String::from)
        .to_vec();
        let row = vec![
            J::from("tick"),
            J::from("p.py"),
            J::from(r#"{"every":{"duration":{"secs":5,"nanos":0}}}"#),
            J::Bool(true),
            J::from("retry"),
        ];
        let runs = log_runs(
            &[
                "trigger_name",
                "run_id",
                "started",
                "ended",
                "error",
                "finished",
            ]
            .map(String::from),
            &[
                vec![
                    J::from("tick"),
                    J::from("r2"),
                    J::from("2026-11-12T13:04:15.020290360"),
                    J::from("2026-11-12T13:04:15.075293806"),
                    J::from("boom"),
                    J::from(0),
                ],
                vec![
                    J::from("tick"),
                    J::from("r1"),
                    J::from("2026-11-12 13:04:10.001474317"),
                    J::from("2026-11-12 13:04:10.004779380"),
                    J::Null,
                    J::from(1),
                ],
            ],
        );
        let j = v3_job("db", &cols, &row, last_run(&runs));
        assert_eq!(j.id, "\"db\".\"tick\"");
        assert_eq!(j.kind, "Trigger");
        assert_eq!(j.schedule.as_deref(), Some("every:5s"));
        assert!(!j.enabled && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_run && !j.can_stop);
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T13:04:15Z"));
        assert_eq!(j.last_duration_ms, Some(55));
        assert_eq!(runs[1].run().outcome, JobOutcome::Succeeded);
        assert_eq!(runs[1].run().duration_ms, Some(3));
        // A run with no end logged: unknown, and the list shows the one
        // before it.
        let mut going = runs[1].clone();
        going.run_id = "r3".into();
        going.finished = false;
        let all = [going.clone(), runs[1].clone()];
        assert_eq!(last_run(&all).unwrap().run_id, "r1");
        assert_eq!(going.run().outcome, JobOutcome::Unknown);
        assert!(going.run().message.unwrap().contains("log del servidor"));
        assert_eq!(last_run(&[going.clone()]).unwrap().run_id, "r3");
        let script = v3_script(
            "db",
            &cols,
            &row,
            &[("k".into(), "v".into()), ("x".into(), "it's".into())],
        );
        assert_eq!(
            script,
            "influxdb3 create trigger \\\n  --database 'db' \\\n  --plugin-filename 'p.py' \\\n  --trigger-spec 'every:5s' \\\n  --trigger-arguments 'k=v,x=it'\\''s' \\\n  --error-behavior 'retry' \\\n  --disabled \\\n  'tick'\n"
        );
        assert!(v3_runs_sql(Some("a'b"), 3).contains("WHERE trigger_name = 'a''b'"));
    }
}
