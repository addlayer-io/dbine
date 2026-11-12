//! Server jobs ([`dbine_driver::Session::jobs`] and the calls that follow
//! it): BigQuery's scheduled queries. They live in the BigQuery Data
//! Transfer Service, as transfer configs of the `scheduled_query` data
//! source, listed per location: the list looks in the connection's location
//! and in those of the project's datasets.
//!
//! Runs, "run now" (`startManualRuns`) and enabling (`disabled`, through
//! `patch`) go through the same service with the connection's token (its
//! `cloud-platform` scope covers it). The service can't stop a run: its
//! query is a BigQuery job, which the process list cancels. The emulator
//! doesn't have the service.
//!
//! The id is the config's resource name
//! (`projects/<number>/locations/<location>/transferConfigs/<id>`), checked
//! segment by segment before it goes into a URL, where each segment is
//! escaped.

use crate::{gcp, Api, BigQuerySession, Json};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, PropertyInfo, Result, ServerJob,
};
use serde_json::json;
use std::collections::BTreeSet;

const DTS: &str = "https://bigquerydatatransfer.googleapis.com";
/// The engine's word for them, shown as a tag.
const KIND: &str = "Scheduled query";
const SOURCE: &str = "scheduled_query";
/// Items per page (the service's maximum).
const PAGE: usize = 1000;
/// Configs the list reads at most.
const MAX_CONFIGS: usize = 5000;
const EMULATOR: &str = "el emulador de BigQuery no tiene el servicio de transferencia de datos, donde viven las consultas programadas";
const DISABLED: &str = "el proyecto no tiene habilitada la API de BigQuery Data Transfer, que guarda las consultas programadas";

/// A failed call: its HTTP status, whether the service is off in the
/// project, and the error.
#[derive(Debug)]
struct Failed {
    status: u16,
    disabled: bool,
    error: Error,
}

impl From<Failed> for Error {
    fn from(f: Failed) -> Error {
        f.error
    }
}

fn failed(error: Error) -> Failed {
    Failed {
        status: 0,
        disabled: false,
        error,
    }
}

/// A Google error body that says the API is off in the project.
fn service_disabled(body: &str) -> bool {
    let Ok(j) = serde_json::from_str::<Json>(body) else {
        return false;
    };
    let reason = j
        .pointer("/error/details")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .any(|d| d.get("reason").and_then(Json::as_str) == Some("SERVICE_DISABLED"));
    let message = j
        .pointer("/error/message")
        .and_then(Json::as_str)
        .unwrap_or("");
    reason || message.contains("has not been used in project") || message.contains("it is disabled")
}

/// A config's resource name → its project, location and id.
pub(crate) fn parse_name(id: &str) -> Option<[&str; 3]> {
    let ok = |s: &str| {
        !s.is_empty()
            && s.len() <= 128
            && s.chars()
                .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
    };
    match id.trim().split('/').collect::<Vec<_>>().as_slice() {
        ["projects", p, "locations", l, "transferConfigs", c] if ok(p) && ok(l) && ok(c) => {
            Some([*p, *l, *c])
        }
        _ => None,
    }
}

fn checked(id: &str) -> Result<[&str; 3]> {
    parse_name(id).ok_or_else(|| {
        Error::Query(format!(
            "«{}» no es un id de consulta programada de BigQuery",
            id.trim()
        ))
    })
}

fn text(v: &Json, ptr: &str) -> Option<String> {
    match v.pointer(ptr)? {
        Json::String(s) => Some(s.trim().to_string()),
        Json::Null => None,
        x => Some(x.to_string()),
    }
    .filter(|s| !s.is_empty())
}

/// An RFC 3339 time (any offset, any fraction) → RFC 3339 in UTC, to the
/// second.
fn utc(t: &str) -> Option<String> {
    chrono::DateTime::parse_from_rfc3339(t.trim())
        .ok()
        .map(|d| {
            d.with_timezone(&chrono::Utc)
                .format("%Y-%m-%dT%H:%M:%SZ")
                .to_string()
        })
}

fn millis(t: &str) -> Option<i64> {
    chrono::DateTime::parse_from_rfc3339(t.trim())
        .ok()
        .map(|d| d.timestamp_millis())
}

fn state_outcome(s: &str) -> Option<JobOutcome> {
    Some(match s {
        "SUCCEEDED" => JobOutcome::Succeeded,
        "FAILED" => JobOutcome::Failed,
        "CANCELLED" => JobOutcome::Canceled,
        "RUNNING" | "PENDING" => JobOutcome::Running,
        _ => return None,
    })
}

/// The schedule as the service states it ("every 24 hours"); `None` when it
/// only runs on demand.
fn schedule_of(c: &Json) -> Option<String> {
    if c.pointer("/scheduleOptionsV2/manualSchedule").is_some()
        || c.pointer("/scheduleOptions/disableAutoScheduling")
            .and_then(Json::as_bool)
            == Some(true)
    {
        return None;
    }
    if let Some(sub) = text(
        c,
        "/scheduleOptionsV2/eventDrivenSchedule/pubsubSubscription",
    ) {
        return Some(format!("al recibir mensajes en {sub}"));
    }
    text(c, "/scheduleOptionsV2/timeBasedSchedule/schedule").or_else(|| text(c, "/schedule"))
}

/// A transfer config as a job. Its `state` is its latest run's.
fn server_job(c: &Json) -> Option<ServerJob> {
    let id = text(c, "/name")?;
    let [_, location, config] = parse_name(&id)?;
    let (location, config) = (location.to_string(), config.to_string());
    let disabled = c.get("disabled").and_then(Json::as_bool).unwrap_or(false);
    let state = text(c, "/state");
    Some(ServerJob {
        name: text(c, "/displayName").unwrap_or(config),
        kind: KIND.into(),
        group: Some(location),
        description: None,
        owner: text(c, "/ownerInfo/email"),
        enabled: !disabled,
        schedule: schedule_of(c),
        running: matches!(state.as_deref(), Some("RUNNING" | "PENDING")),
        last_outcome: state.as_deref().and_then(state_outcome),
        next_run: if disabled {
            None
        } else {
            text(c, "/nextRunTime").and_then(|t| utc(&t))
        },
        can_run: true,
        // The service has no call that stops a run.
        can_stop: false,
        can_enable: true,
        has_history: true,
        has_script: true,
        id,
        ..Default::default()
    })
}

fn job_run(r: &Json) -> JobRun {
    let outcome = text(r, "/state")
        .as_deref()
        .and_then(state_outcome)
        .unwrap_or_default();
    let start = text(r, "/startTime");
    let end = text(r, "/endTime");
    let run_time = text(r, "/runTime");
    let duration = || Some((millis(end.as_deref()?)? - millis(start.as_deref()?)?).max(0) as u64);
    JobRun {
        id: text(r, "/name").and_then(|n| n.rsplit('/').next().map(str::to_string)),
        started: start.as_deref().or(run_time.as_deref()).and_then(utc),
        duration_ms: if outcome == JobOutcome::Running {
            None
        } else {
            duration()
        },
        outcome,
        // A run of the schedule carries it; a manual one doesn't.
        trigger: Some(
            if text(r, "/schedule").is_some() {
                "programada"
            } else {
                "manual"
            }
            .into(),
        ),
        message: text(r, "/errorStatus/message"),
        steps: Vec::new(),
    }
}

/// Whether a run, the job's latest, is going on, and how the last finished
/// one went (runs newest first).
fn apply_runs(job: &mut ServerJob, runs: &[JobRun]) {
    let going = |r: &&JobRun| r.outcome == JobOutcome::Running;
    let running = runs.iter().find(going);
    let finished = runs.iter().find(|r| !going(r));
    job.running = running.is_some();
    if let Some(f) = finished {
        job.last_outcome = Some(f.outcome);
        job.last_started = f.started.clone();
        job.last_duration_ms = f.duration_ms;
    } else if job.running {
        job.last_outcome = Some(JobOutcome::Running);
    }
    if let Some(r) = running {
        job.last_started = r.started.clone();
    }
}

fn prop(p: &mut Vec<PropertyInfo>, label: &str, value: Option<String>) {
    if let Some(value) = value.filter(|v| !v.is_empty()) {
        p.push(PropertyInfo {
            group: String::new(),
            label: label.into(),
            value,
        });
    }
}

fn properties(c: &Json, location: &str, config: &str) -> Vec<PropertyInfo> {
    let mut p = Vec::new();
    prop(&mut p, "Ubicación", Some(location.to_string()));
    prop(
        &mut p,
        "Dataset de destino",
        text(c, "/destinationDatasetId"),
    );
    prop(
        &mut p,
        "Tabla de destino",
        text(c, "/params/destination_table_name_template"),
    );
    prop(&mut p, "Escritura", text(c, "/params/write_disposition"));
    prop(
        &mut p,
        "Campo de partición",
        text(c, "/params/partitioning_field"),
    );
    prop(
        &mut p,
        "Inicio de la programación",
        text(c, "/scheduleOptionsV2/timeBasedSchedule/startTime")
            .or_else(|| text(c, "/scheduleOptions/startTime"))
            .and_then(|t| utc(&t)),
    );
    prop(
        &mut p,
        "Fin de la programación",
        text(c, "/scheduleOptionsV2/timeBasedSchedule/endTime")
            .or_else(|| text(c, "/scheduleOptions/endTime"))
            .and_then(|t| utc(&t)),
    );
    prop(
        &mut p,
        "Actualizada",
        text(c, "/updateTime").and_then(|t| utc(&t)),
    );
    prop(
        &mut p,
        "Correo al fallar",
        c.pointer("/emailPreferences/enableFailureEmail")
            .and_then(Json::as_bool)
            .map(|on| if on { "Sí" } else { "No" }.to_string()),
    );
    prop(
        &mut p,
        "Tema de Pub/Sub",
        text(c, "/notificationPubsubTopic"),
    );
    prop(
        &mut p,
        "Clave de cifrado",
        text(c, "/encryptionConfiguration/kmsKeyName"),
    );
    prop(&mut p, "Id", Some(config.to_string()));
    p
}

/// A shell word in single quotes.
fn sh(s: &str) -> String {
    format!("'{}'", s.replace('\'', r"'\''"))
}

/// The `bq mk --transfer_config` command that creates the config again.
fn bq_script(c: &Json, project: &str) -> String {
    let location = text(c, "/name")
        .as_deref()
        .and_then(parse_name)
        .map(|[_, l, _]| l.to_string())
        .unwrap_or_default();
    let mut head = String::new();
    if c.get("disabled").and_then(Json::as_bool) == Some(true) {
        head.push_str("# En el origen está deshabilitada: bq mk la crea habilitada.\n");
    }
    let mut flags = vec![
        "--transfer_config".to_string(),
        format!("--project_id={}", sh(project)),
        format!("--data_source={SOURCE}"),
    ];
    if let Some(n) = text(c, "/displayName") {
        flags.push(format!("--display_name={}", sh(&n)));
    }
    if let Some(d) = text(c, "/destinationDatasetId") {
        flags.push(format!("--target_dataset={}", sh(&d)));
    }
    match schedule_of(c) {
        None => flags.push("--no_auto_scheduling".into()),
        Some(_)
            if c.pointer("/scheduleOptionsV2/eventDrivenSchedule")
                .is_some() =>
        {
            head.push_str("# Se dispara por Pub/Sub: bq mk no tiene una opción para eso.\n");
        }
        Some(s) => flags.push(format!("--schedule={}", sh(&s))),
    }
    for (flag, ptr) in [
        ("--schedule_start_time", "/scheduleOptions/startTime"),
        ("--schedule_end_time", "/scheduleOptions/endTime"),
        ("--notification_pubsub_topic", "/notificationPubsubTopic"),
        (
            "--destination_kms_key",
            "/encryptionConfiguration/kmsKeyName",
        ),
    ] {
        if let Some(v) = text(c, ptr) {
            flags.push(format!("{flag}={}", sh(&v)));
        }
    }
    let params = c.get("params").cloned().unwrap_or_else(|| json!({}));
    flags.push(format!("--params={}", sh(&params.to_string())));
    format!(
        "{head}bq --location={} mk \\\n  {}\n",
        sh(&location),
        flags.join(" \\\n  ")
    )
}

/// The Data Transfer Service at `base`.
pub(crate) struct Dts<'a> {
    api: &'a Api,
    base: &'a str,
}

impl Dts<'_> {
    /// `<base>/v1/<segments…>`, each segment escaped.
    fn url(&self, segments: &[&str]) -> std::result::Result<reqwest::Url, Failed> {
        let mut url = reqwest::Url::parse(self.base)
            .map_err(|e| failed(Error::Connect(format!("endpoint inválido: {e}"))))?;
        url.path_segments_mut()
            .map_err(|_| failed(Error::Connect("endpoint inválido".into())))?
            .pop_if_empty()
            .push("v1")
            .extend(segments);
        Ok(url)
    }

    async fn send(&self, req: reqwest::RequestBuilder) -> std::result::Result<Json, Failed> {
        let req = match self.api.tokens.bearer().await.map_err(failed)? {
            Some(b) => req.header("Authorization", b),
            None => req,
        };
        let resp = req
            .send()
            .await
            .map_err(|e| failed(Error::Connect(e.to_string())))?;
        let status = resp.status().as_u16();
        let body = resp
            .text()
            .await
            .map_err(|e| failed(Error::Connect(e.to_string())))?;
        if !(200..300).contains(&status) {
            return Err(Failed {
                status,
                disabled: service_disabled(&body),
                error: gcp::api_error(status, &body),
            });
        }
        if body.trim().is_empty() {
            return Ok(Json::Null);
        }
        serde_json::from_str(&body).map_err(|e| failed(e.into()))
    }

    fn config_segments<'b>(name: &[&'b str; 3]) -> [&'b str; 6] {
        let [p, l, c] = *name;
        ["projects", p, "locations", l, "transferConfigs", c]
    }

    /// The scheduled queries of one location.
    async fn configs(
        &self,
        project: &str,
        location: &str,
    ) -> std::result::Result<Vec<Json>, Failed> {
        let mut out = Vec::new();
        let mut token: Option<String> = None;
        loop {
            let mut q = vec![
                ("dataSourceIds", SOURCE.to_string()),
                ("pageSize", PAGE.to_string()),
            ];
            if let Some(t) = token.take() {
                q.push(("pageToken", t));
            }
            let url = self.url(&[
                "projects",
                project,
                "locations",
                location,
                "transferConfigs",
            ])?;
            let page = self.send(self.api.http.get(url).query(&q)).await?;
            out.extend(
                page.get("transferConfigs")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            match page.get("nextPageToken").and_then(Json::as_str) {
                Some(t) if !t.is_empty() && out.len() < MAX_CONFIGS => token = Some(t.to_string()),
                _ => break,
            }
        }
        Ok(out)
    }

    /// The scheduled queries of `locations`. `datasets_read`: they include
    /// those of the project's datasets (`false`: datasets.list failed, so
    /// some may be missing).
    async fn list(
        &self,
        project: &str,
        locations: &[String],
        datasets_read: bool,
    ) -> Result<JobList> {
        let mut jobs = Vec::new();
        let mut notes = Vec::new();
        for location in locations {
            match self.configs(project, location).await {
                Ok(cs) => jobs.extend(cs.iter().filter_map(server_job)),
                Err(f) if f.disabled => {
                    return Ok(JobList {
                        jobs: Vec::new(),
                        notes: vec![DISABLED.into()],
                    })
                }
                // Project-wide: the other locations answer the same.
                Err(f) if f.status == 403 => {
                    notes.push(format!(
                        "la cuenta no tiene permiso para ver las consultas programadas del proyecto (bigquery.transfers.get): {}",
                        f.error
                    ));
                    break;
                }
                // A location the service doesn't have.
                Err(f) if matches!(f.status, 400 | 404) => {
                    tracing::debug!("bigquery: no transfer configs in {location}: {}", f.error)
                }
                Err(f) => return Err(f.error),
            }
        }
        if jobs.len() > MAX_CONFIGS {
            jobs.truncate(MAX_CONFIGS);
            notes.push(format!(
                "se muestran las primeras {MAX_CONFIGS} consultas programadas"
            ));
        }
        // The service lists per location: say where the list looked, since
        // a scheduled query elsewhere (one that writes to another
        // project's dataset, say) isn't in it.
        let locs = locations.join(", ");
        let unread = "no se pudieron leer los datasets del proyecto para saber en qué otras ubicaciones buscar";
        if jobs.is_empty() && notes.is_empty() {
            notes.push(if datasets_read {
                format!("no hay consultas programadas en las ubicaciones {locs} (la de la conexión y las de los datasets del proyecto)")
            } else {
                format!("no hay consultas programadas en las ubicaciones {locs}: {unread}")
            });
        } else if !jobs.is_empty() {
            notes.push(if datasets_read {
                format!("se muestran las consultas programadas de las ubicaciones {locs}, la de la conexión y las de los datasets del proyecto")
            } else {
                format!("se muestran solo las consultas programadas de las ubicaciones {locs}: {unread}")
            });
        }
        Ok(JobList { jobs, notes })
    }

    async fn config(&self, name: &[&str; 3]) -> Result<Json> {
        let url = self.url(&Self::config_segments(name))?;
        Ok(self.send(self.api.http.get(url)).await?)
    }

    /// The config's newest runs (the latest attempt of each), newest first.
    async fn runs(&self, name: &[&str; 3], limit: usize) -> Result<Vec<JobRun>> {
        let mut segments = Self::config_segments(name).to_vec();
        segments.push("runs");
        let mut out: Vec<Json> = Vec::new();
        let mut token: Option<String> = None;
        while out.len() < limit {
            let mut q = vec![
                ("pageSize", limit.min(PAGE).to_string()),
                ("runAttempt", "LATEST".to_string()),
            ];
            if let Some(t) = token.take() {
                q.push(("pageToken", t));
            }
            let url = self.url(&segments)?;
            let page = self.send(self.api.http.get(url).query(&q)).await?;
            out.extend(
                page.get("transferRuns")
                    .and_then(Json::as_array)
                    .into_iter()
                    .flatten()
                    .cloned(),
            );
            match page.get("nextPageToken").and_then(Json::as_str) {
                Some(t) if !t.is_empty() => token = Some(t.to_string()),
                _ => break,
            }
        }
        // By when it started, as the run shows it: a backfill started today
        // has an old runTime.
        let key = |r: &Json| {
            text(r, "/startTime")
                .or_else(|| text(r, "/runTime"))
                .or_else(|| text(r, "/scheduleTime"))
                .and_then(|t| millis(&t))
        };
        out.sort_by_key(|r| std::cmp::Reverse(key(r)));
        out.truncate(limit);
        Ok(out.iter().map(job_run).collect())
    }

    async fn detail(&self, id: &str) -> Result<JobDetail> {
        let name = checked(id)?;
        let c = self.config(&name).await?;
        let mut job = server_job(&c).ok_or_else(|| {
            Error::Query(format!(
                "el servicio no devolvió la consulta programada {}",
                name[2]
            ))
        })?;
        match self.runs(&name, 5).await {
            Ok(runs) => apply_runs(&mut job, &runs),
            Err(e) => tracing::debug!("bigquery: runs of {} not read: {e}", name[2]),
        }
        let target = text(&c, "/destinationDatasetId").map(|d| {
            match text(&c, "/params/destination_table_name_template") {
                Some(t) => format!("{d}.{t}"),
                None => d,
            }
        });
        Ok(JobDetail {
            steps: vec![JobStep {
                name: job.name.clone(),
                kind: Some("SQL".into()),
                target,
                command: text(&c, "/params/query"),
            }],
            schedules: Vec::new(),
            properties: properties(&c, name[1], name[2]),
            job,
        })
    }

    async fn history(&self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let name = checked(id)?;
        let limit = limit.min(MAX_JOB_RUNS) as usize;
        if limit == 0 {
            return Ok(Vec::new());
        }
        self.runs(&name, limit).await
    }

    /// A manual run for now: the service answers once it's created.
    async fn run_now(&self, id: &str) -> Result<()> {
        let name = checked(id)?;
        let last = format!("{}:startManualRuns", name[2]);
        let mut segments = Self::config_segments(&name);
        segments[5] = &last;
        let url = self.url(&segments)?;
        // The service refuses a requested run time in the future, so a
        // desktop clock ahead of Google's would fail it: a minute back. It
        // becomes the run's @run_time (and @run_date).
        let now = (chrono::Utc::now() - chrono::Duration::seconds(60))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        self.send(
            self.api
                .http
                .post(url)
                .json(&json!({ "requestedRunTime": now })),
        )
        .await?;
        Ok(())
    }

    async fn set_enabled(&self, id: &str, enabled: bool) -> Result<()> {
        let name = checked(id)?;
        let url = self.url(&Self::config_segments(&name))?;
        self.send(
            self.api
                .http
                .patch(url)
                .query(&[("updateMask", "disabled")])
                .json(&json!({ "disabled": !enabled })),
        )
        .await?;
        Ok(())
    }

    async fn script(&self, id: &str, project: &str) -> Result<String> {
        let name = checked(id)?;
        let c = self.config(&name).await?;
        Ok(bq_script(&c, project))
    }
}

impl BigQuerySession {
    /// The service, unless this is the emulator.
    fn dts(&self) -> Result<Dts<'_>> {
        if self.api.emulator {
            return Err(Error::Unsupported(EMULATOR.into()));
        }
        Ok(Dts {
            api: &self.api,
            base: DTS,
        })
    }

    /// Where to look for scheduled queries: the connection's location and
    /// those of the project's datasets (US and EU when there's none); and
    /// whether the datasets were read.
    async fn job_locations(&self) -> (Vec<String>, bool) {
        let mut locs = BTreeSet::new();
        if let Some(l) = self.api.location.as_deref().map(str::trim) {
            if !l.is_empty() {
                locs.insert(l.to_ascii_lowercase());
            }
        }
        let read = match self.api.list_all(&["datasets"], "datasets").await {
            Ok(ds) => {
                locs.extend(
                    ds.iter()
                        .filter_map(|d| text(d, "/location"))
                        .map(|l| l.to_ascii_lowercase()),
                );
                true
            }
            Err(e) => {
                tracing::debug!("bigquery: datasets not listed for the jobs: {e}");
                false
            }
        };
        if locs.is_empty() {
            locs.extend(["us".to_string(), "eu".to_string()]);
        }
        (locs.into_iter().collect(), read)
    }

    pub(crate) async fn jobs_list(&self) -> Result<JobList> {
        if self.api.emulator {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![EMULATOR.into()],
            });
        }
        let (locations, datasets_read) = self.job_locations().await;
        self.dts()?
            .list(&self.api.project, &locations, datasets_read)
            .await
    }

    pub(crate) async fn job_info(&self, id: &str) -> Result<JobDetail> {
        self.dts()?.detail(id).await
    }

    pub(crate) async fn job_history(&self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        self.dts()?.history(id, limit).await
    }

    pub(crate) async fn job_run_now(&self, id: &str) -> Result<()> {
        self.dts()?.run_now(id).await
    }

    /// The service has no call for it.
    pub(crate) async fn job_stop(&self, id: &str) -> Result<()> {
        checked(id)?;
        Err(Error::Unsupported(
            "el servicio de transferencia de datos no detiene una ejecución en curso: cancelá su trabajo desde la lista de procesos".into(),
        ))
    }

    pub(crate) async fn job_enable(&self, id: &str, enabled: bool) -> Result<()> {
        self.dts()?.set_enabled(id, enabled).await
    }

    pub(crate) async fn job_bq_script(&self, id: &str) -> Result<String> {
        self.dts()?.script(id, &self.api.project).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ConnectionConfig;
    use std::sync::{Arc, Mutex};

    const NAME: &str =
        "projects/123456/locations/us/transferConfigs/6789abcd-0000-2a6d-91b4-089e0820c3c4";

    fn config() -> Json {
        json!({
            "name": NAME,
            "displayName": "ventas diarias",
            "dataSourceId": "scheduled_query",
            "destinationDatasetId": "ventas",
            "datasetRegion": "us",
            "params": {
                "query": "SELECT * FROM ventas.crudo WHERE dia = @run_date",
                "destination_table_name_template": "resumen_{run_date}",
                "write_disposition": "WRITE_TRUNCATE",
                "partitioning_field": ""
            },
            "schedule": "every 24 hours",
            "scheduleOptions": { "startTime": "2026-01-01T03:00:00.123Z" },
            "updateTime": "2026-11-10T12:00:00.5-03:00",
            "nextRunTime": "2026-11-13T03:00:00Z",
            "state": "FAILED",
            "userId": "-123",
            "ownerInfo": { "email": "ana@example.com" },
            "emailPreferences": { "enableFailureEmail": true },
            "disabled": false
        })
    }

    #[test]
    fn names_are_checked() {
        assert_eq!(
            parse_name(NAME),
            Some(["123456", "us", "6789abcd-0000-2a6d-91b4-089e0820c3c4"])
        );
        for bad in [
            "",
            "projects/1/locations/us/transferConfigs",
            "projects/1/locations/us/transferConfigs/a/runs/b",
            "projects/1/locations/../transferConfigs/a",
            "projects/1/locations/us/transferConfigs/a:startManualRuns",
            "projects/1/locations/us/transferConfigs/a?updateMask=x",
            "projects/1/locations/us/datasets/a/transferConfigs/b",
            "projects//locations/us/transferConfigs/a",
        ] {
            assert_eq!(parse_name(bad), None, "{bad}");
        }
    }

    #[test]
    fn a_config_becomes_a_job() {
        let j = server_job(&config()).unwrap();
        assert_eq!(j.id, NAME);
        assert_eq!(j.name, "ventas diarias");
        assert_eq!(j.kind, "Scheduled query");
        assert_eq!(j.group.as_deref(), Some("us"));
        assert_eq!(j.owner.as_deref(), Some("ana@example.com"));
        assert_eq!(j.schedule.as_deref(), Some("every 24 hours"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T03:00:00Z"));
        assert!(j.enabled && !j.running);
        assert!(j.can_run && !j.can_stop && j.can_enable && j.has_history && j.has_script);

        let mut off = config();
        off["disabled"] = json!(true);
        off["state"] = json!("RUNNING");
        off["scheduleOptions"] = json!({ "disableAutoScheduling": true });
        let j = server_job(&off).unwrap();
        assert!(!j.enabled && j.running && j.next_run.is_none() && j.schedule.is_none());
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));

        let mut v2 = config();
        v2["scheduleOptionsV2"] = json!({ "eventDrivenSchedule": { "pubsubSubscription": "projects/p/subscriptions/s" } });
        assert_eq!(
            server_job(&v2).unwrap().schedule.as_deref(),
            Some("al recibir mensajes en projects/p/subscriptions/s")
        );
        let mut forged = config();
        forged["name"] = json!("projects/1/locations/us/transferConfigs/../../x");
        assert!(server_job(&forged).is_none());
    }

    #[test]
    fn runs_map_states_and_times() {
        let r = job_run(&json!({
            "name": format!("{NAME}/runs/6a1b"),
            "schedule": "every 24 hours",
            "runTime": "2026-11-12T03:00:00Z",
            "startTime": "2026-11-12T03:00:01.250Z",
            "endTime": "2026-11-12T03:00:04.250Z",
            "state": "FAILED",
            "errorStatus": { "code": 3, "message": "Syntax error: Unexpected end of script" }
        }));
        assert_eq!(r.id.as_deref(), Some("6a1b"));
        assert_eq!(r.outcome, JobOutcome::Failed);
        assert_eq!(r.started.as_deref(), Some("2026-11-12T03:00:01Z"));
        assert_eq!(r.duration_ms, Some(3000));
        assert_eq!(r.trigger.as_deref(), Some("programada"));
        assert_eq!(
            r.message.as_deref(),
            Some("Syntax error: Unexpected end of script")
        );
        let going = job_run(&json!({ "state": "PENDING", "runTime": "2026-11-12T05:00:00-03:00" }));
        assert_eq!(going.outcome, JobOutcome::Running);
        assert_eq!(going.started.as_deref(), Some("2026-11-12T08:00:00Z"));
        assert_eq!(going.duration_ms, None);
        assert_eq!(going.trigger.as_deref(), Some("manual"));
    }

    #[test]
    fn the_script_is_a_bq_command() {
        let mut c = config();
        c["displayName"] = json!("it's daily");
        let s = bq_script(&c, "mi-proyecto");
        let (command, params) = s.split_once("--params='").unwrap();
        assert_eq!(
            command,
            "bq --location='us' mk \\\n  --transfer_config \\\n  --project_id='mi-proyecto' \\\n  --data_source=scheduled_query \\\n  \
             --display_name='it'\\''s daily' \\\n  --target_dataset='ventas' \\\n  --schedule='every 24 hours' \\\n  \
             --schedule_start_time='2026-01-01T03:00:00.123Z' \\\n  "
        );
        let params: Json = serde_json::from_str(params.strip_suffix("'\n").unwrap()).unwrap();
        assert_eq!(params, c["params"]);
        c["disabled"] = json!(true);
        c["scheduleOptions"] = json!({ "disableAutoScheduling": true });
        let s = bq_script(&c, "p");
        assert!(s.starts_with("# En el origen está deshabilitada"));
        assert!(s.contains("--no_auto_scheduling") && !s.contains("--schedule="));
    }

    #[test]
    fn a_disabled_api_is_recognized() {
        let body = json!({ "error": { "code": 403, "status": "PERMISSION_DENIED",
            "message": "BigQuery Data Transfer API has not been used in project 123 before or it is disabled.",
            "details": [{ "@type": "type.googleapis.com/google.rpc.ErrorInfo", "reason": "SERVICE_DISABLED" }] } });
        assert!(service_disabled(&body.to_string()));
        let denied = json!({ "error": { "code": 403, "message": "The caller does not have permission", "status": "PERMISSION_DENIED" } });
        assert!(!service_disabled(&denied.to_string()));
    }

    /// A tiny HTTP server: one request per connection, answered by `h`
    /// (method, path with query, body) → (status, body). As the Databricks
    /// crate's test server.
    type Handler = Arc<dyn Fn(&str, &str, &str) -> (u16, String) + Send + Sync>;

    async fn serve(h: Handler) -> String {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let l = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = l.local_addr().unwrap();
        tokio::spawn(async move {
            while let Ok((mut sock, _)) = l.accept().await {
                let h = h.clone();
                tokio::spawn(async move {
                    let mut buf = Vec::new();
                    let mut tmp = vec![0u8; 1 << 16];
                    let (head_end, len) = loop {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                        if let Some(p) = buf.windows(4).position(|w| w == b"\r\n\r\n") {
                            let head = String::from_utf8_lossy(&buf[..p]).to_ascii_lowercase();
                            let len = head
                                .lines()
                                .find_map(|l| {
                                    l.strip_prefix("content-length:")
                                        .map(|v| v.trim().parse().unwrap())
                                })
                                .unwrap_or(0);
                            break (p + 4, len);
                        }
                    };
                    while buf.len() < head_end + len {
                        let n = sock.read(&mut tmp).await.unwrap_or(0);
                        if n == 0 {
                            return;
                        }
                        buf.extend_from_slice(&tmp[..n]);
                    }
                    let head = String::from_utf8_lossy(&buf[..head_end]).to_string();
                    let mut first = head.lines().next().unwrap_or("").split(' ');
                    let (method, path) = (
                        first.next().unwrap_or("").to_string(),
                        first.next().unwrap_or("").to_string(),
                    );
                    let body = String::from_utf8_lossy(&buf[head_end..head_end + len]).to_string();
                    let (status, text) = h(&method, &path, &body);
                    let resp = format!(
                        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n",
                        text.len()
                    );
                    let _ = sock.write_all(resp.as_bytes()).await;
                    let _ = sock.write_all(text.as_bytes()).await;
                });
            }
        });
        format!("http://{addr}")
    }

    /// An `Api` that sends no token (as against an emulator).
    fn api() -> Api {
        let http = reqwest::Client::new();
        let cfg = ConnectionConfig {
            options: [("endpoint_url".to_string(), "http://localhost:1".to_string())].into(),
            ..Default::default()
        };
        Api {
            tokens: gcp::Tokens::from_config(&cfg, http.clone()).unwrap(),
            http,
            base: "http://localhost:1".into(),
            project: "mi-proyecto".into(),
            location: None,
            emulator: false,
        }
    }

    type Seen = Arc<Mutex<Vec<(String, String, String)>>>;

    /// The service: one config in `us`, none in `eu`, `asia-x` unknown.
    async fn service(seen: Seen) -> String {
        serve(Arc::new(move |m, p, b| {
            seen.lock()
                .unwrap()
                .push((m.to_string(), p.to_string(), b.to_string()));
            let (path, query) = p.split_once('?').unwrap_or((p, ""));
            let cfg_path = format!("/v1/{NAME}");
            let body = match (m, path) {
                ("GET", "/v1/projects/mi-proyecto/locations/us/transferConfigs")
                    if query.contains("pageToken=t2") =>
                {
                    json!({ "transferConfigs": [] })
                }
                ("GET", "/v1/projects/mi-proyecto/locations/us/transferConfigs") => {
                    json!({ "transferConfigs": [config()], "nextPageToken": "t2" })
                }
                ("GET", "/v1/projects/mi-proyecto/locations/eu/transferConfigs") => json!({}),
                ("GET", x) if x == cfg_path => config(),
                ("GET", x) if x == format!("{cfg_path}/runs") => json!({ "transferRuns": [
                    { "name": format!("{NAME}/runs/r1"), "state": "SUCCEEDED", "schedule": "every 24 hours",
                      "runTime": "2026-11-11T03:00:00Z", "startTime": "2026-11-11T03:00:01Z", "endTime": "2026-11-11T03:00:03Z" },
                    { "name": format!("{NAME}/runs/r2"), "state": "RUNNING",
                      "runTime": "2026-11-12T03:00:00Z", "startTime": "2026-11-12T03:00:01Z" },
                    // A backfill started last: an old runTime.
                    { "name": format!("{NAME}/runs/r3"), "state": "SUCCEEDED",
                      "runTime": "2026-11-01T03:00:00Z", "startTime": "2026-11-12T05:00:00Z", "endTime": "2026-11-12T05:00:02Z" }
                ] }),
                ("POST", x) if x == format!("{cfg_path}:startManualRuns") => json!({ "runs": [] }),
                ("PATCH", x) if x == cfg_path => config(),
                _ => {
                    return (
                        404,
                        json!({ "error": { "code": 404, "message": "Location not found" } }).to_string(),
                    )
                }
            };
            (200, body.to_string())
        }))
        .await
    }

    #[tokio::test]
    async fn the_service_is_called_as_documented() {
        let seen: Seen = Default::default();
        let base = service(seen.clone()).await;
        let api = api();
        let dts = Dts {
            api: &api,
            base: &base,
        };

        let list = dts
            .list(
                "mi-proyecto",
                &["asia-x".into(), "eu".into(), "us".into()],
                true,
            )
            .await
            .unwrap();
        assert_eq!(list.jobs.len(), 1);
        assert_eq!(
            list.notes,
            ["se muestran las consultas programadas de las ubicaciones asia-x, eu, us, la de la conexión y las de los datasets del proyecto"]
        );
        assert!(seen
            .lock()
            .unwrap()
            .iter()
            .any(|c| c.1.contains("dataSourceIds=scheduled_query")));

        let d = dts.detail(NAME).await.unwrap();
        assert!(d.job.running, "the newest run is going on");
        assert_eq!(d.job.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(d.job.last_started.as_deref(), Some("2026-11-12T03:00:01Z"));
        assert_eq!(d.job.last_duration_ms, Some(2000));
        assert_eq!(
            d.steps[0].target.as_deref(),
            Some("ventas.resumen_{run_date}")
        );
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some("SELECT * FROM ventas.crudo WHERE dia = @run_date")
        );
        let prop = |l: &str| {
            d.properties
                .iter()
                .find(|p| p.label == l)
                .map(|p| p.value.as_str())
        };
        assert_eq!(prop("Actualizada"), Some("2026-11-10T15:00:00Z"));
        assert_eq!(prop("Campo de partición"), None);
        assert_eq!(prop("Correo al fallar"), Some("Sí"));

        let runs = dts.history(NAME, 10).await.unwrap();
        let ids: Vec<&str> = runs.iter().filter_map(|r| r.id.as_deref()).collect();
        assert_eq!(ids, ["r3", "r2", "r1"], "newest start first");
        assert_eq!(dts.history(NAME, 1).await.unwrap().len(), 1);

        dts.run_now(NAME).await.unwrap();
        dts.set_enabled(NAME, false).await.unwrap();
        let script = dts.script(NAME, "mi-proyecto").await.unwrap();
        assert!(script.starts_with("bq --location='us' mk"));

        let calls = seen.lock().unwrap().clone();
        let post = calls.iter().find(|c| c.0 == "POST").unwrap();
        assert!(post.1.ends_with(":startManualRuns"));
        let body: Json = serde_json::from_str(&post.2).unwrap();
        let asked = body["requestedRunTime"].as_str().unwrap();
        assert!(asked.ends_with('Z'));
        let ago = chrono::Utc::now().timestamp_millis() - millis(asked).unwrap();
        assert!(
            (59_000..120_000).contains(&ago),
            "a minute back, never in the future: {ago} ms"
        );
        let patch = calls.iter().find(|c| c.0 == "PATCH").unwrap();
        assert!(patch.1.ends_with("?updateMask=disabled"), "{}", patch.1);
        assert_eq!(
            serde_json::from_str::<Json>(&patch.2).unwrap(),
            json!({ "disabled": true })
        );
        assert!(calls.iter().any(|c| c.1.contains("runAttempt=LATEST")));

        let before = calls.len();
        for bad in [
            "projects/1/locations/us/transferConfigs/a/../../../x",
            "projects/1/locations/us/transferConfigs/a?updateMask=name",
            "x'; DROP",
        ] {
            assert!(dts.detail(bad).await.is_err());
            assert!(dts.history(bad, 5).await.is_err());
            assert!(dts.run_now(bad).await.is_err());
            assert!(dts.set_enabled(bad, true).await.is_err());
            assert!(dts.script(bad, "p").await.is_err());
        }
        assert_eq!(
            seen.lock().unwrap().len(),
            before,
            "a bad id never reaches the service"
        );
    }

    #[tokio::test]
    async fn a_disabled_service_or_no_permission_is_a_note() {
        let disabled = serve(Arc::new(|_, _, _| {
            (403, json!({ "error": { "code": 403, "status": "PERMISSION_DENIED",
                "message": "BigQuery Data Transfer API has not been used in project 1 before or it is disabled.",
                "details": [{ "reason": "SERVICE_DISABLED" }] } }).to_string())
        }))
        .await;
        let api = api();
        let list = Dts {
            api: &api,
            base: &disabled,
        }
        .list("p", &["us".into()], true)
        .await
        .unwrap();
        assert!(list.jobs.is_empty());
        assert_eq!(list.notes, [DISABLED]);

        let denied = serve(Arc::new(|_, _, _| {
            (
                403,
                json!({ "error": { "code": 403, "message": "Permission denied" } }).to_string(),
            )
        }))
        .await;
        let list = Dts {
            api: &api,
            base: &denied,
        }
        .list("p", &["us".into(), "eu".into()], true)
        .await
        .unwrap();
        assert_eq!(
            list.notes,
            ["la cuenta no tiene permiso para ver las consultas programadas del proyecto (bigquery.transfers.get): Permission denied"]
        );

        let empty = serve(Arc::new(|_, _, _| (200, "{}".into()))).await;
        let dts = Dts {
            api: &api,
            base: &empty,
        };
        let list = dts.list("p", &["us".into()], true).await.unwrap();
        assert_eq!(
            list.notes,
            ["no hay consultas programadas en las ubicaciones us (la de la conexión y las de los datasets del proyecto)"]
        );
        // Without the datasets, the list says where it couldn't look.
        let list = dts.list("p", &["us".into()], false).await.unwrap();
        assert_eq!(
            list.notes,
            ["no hay consultas programadas en las ubicaciones us: no se pudieron leer los datasets del proyecto para saber en qué otras ubicaciones buscar"]
        );
    }

    /// Against a real project with the Data Transfer API on, and
    /// credentials that can create datasets and transfer configs (a
    /// service-account key file, or `adc`). It makes a dataset and a
    /// scheduled query named `dbine_test_jobs_*` and deletes both at the
    /// end, also when an assertion fails:
    ///
    /// ```sh
    /// DBINE_TEST_BIGQUERY_PROJECT=mi-proyecto DBINE_TEST_BIGQUERY_CREDENTIALS=/ruta/clave.json \
    /// DBINE_TEST_BIGQUERY_LOCATION=us \
    ///   cargo test -p dbine-driver-bigquery --lib jobs::tests::live_scheduled_queries -- --ignored --nocapture
    /// ```
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn live_scheduled_queries() {
        use crate::BigQuerySession;
        use dbine_driver::Session;
        let var = |k: &str| {
            std::env::var(format!("DBINE_TEST_BIGQUERY_{k}"))
                .ok()
                .filter(|v| !v.is_empty())
        };
        let (Some(project), Some(key)) = (var("PROJECT"), var("CREDENTIALS")) else {
            eprintln!("sin DBINE_TEST_BIGQUERY_PROJECT / _CREDENTIALS: se omite");
            return;
        };
        let location = var("LOCATION")
            .unwrap_or_else(|| "us".into())
            .to_ascii_lowercase();
        let mut cfg = ConnectionConfig::default();
        if key == "adc" {
            cfg.options.insert("auth_mode".into(), "adc".into());
        } else {
            cfg.options.insert(
                "service_account_json".into(),
                std::fs::read_to_string(&key).unwrap(),
            );
        }
        let http = gcp::http_client().unwrap();
        let api = Api {
            tokens: gcp::Tokens::from_config(&cfg, http.clone()).unwrap(),
            http,
            base: crate::API.into(),
            project: project.clone(),
            location: Some(location.clone()),
            emulator: false,
        };
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs();
        let ds = format!("dbine_test_jobs_{stamp}");
        api.post(
            &["datasets"],
            &[],
            &json!({ "datasetReference": { "projectId": project, "datasetId": ds }, "location": location }),
        )
        .await
        .unwrap();
        let dts = Dts {
            api: &api,
            base: DTS,
        };
        let tomorrow = (chrono::Utc::now() + chrono::Duration::days(1))
            .format("%Y-%m-%dT%H:%M:%SZ")
            .to_string();
        let created = dts
            .send(
                api.http
                    .post(
                        dts.url(&["projects", &project, "locations", &location, "transferConfigs"])
                            .unwrap(),
                    )
                    .json(&json!({
                        "displayName": ds,
                        "dataSourceId": SOURCE,
                        "destinationDatasetId": ds,
                        "schedule": "every 24 hours",
                        "scheduleOptions": { "startTime": tomorrow },
                        "params": { "query": "SELECT 1 AS x", "destination_table_name_template": "dbine_test_jobs_t",
                                    "write_disposition": "WRITE_TRUNCATE" }
                    })),
            )
            .await;
        let name = created
            .as_ref()
            .ok()
            .and_then(|c| text(c, "/name"))
            .unwrap_or_default();

        let body = {
            let (api, name, ds, created) = (
                api.clone(),
                name.clone(),
                ds.clone(),
                created.map_err(|f| f.error.to_string()),
            );
            tokio::spawn(async move {
                created.expect("the scheduled query is created");
                let mut s = BigQuerySession {
                    api,
                    dataset: None,
                    session_id: None,
                    job: Arc::new(Mutex::new(None)),
                    storage: Default::default(),
                    profiler: None,
                };
                let list = s.jobs().await.unwrap();
                eprintln!("{:?}", list.notes);
                let job = list.jobs.iter().find(|j| j.id == name).expect("listed");
                assert_eq!(job.name, ds);
                assert_eq!(job.schedule.as_deref(), Some("every 24 hours"));
                assert!(job.enabled && job.can_run && !job.can_stop && job.can_enable);

                let d = s.job_detail(&name).await.unwrap();
                assert_eq!(d.steps[0].command.as_deref(), Some("SELECT 1 AS x"));

                s.run_job(&name).await.unwrap();
                let mut done = None;
                for _ in 0..40 {
                    tokio::time::sleep(std::time::Duration::from_secs(5)).await;
                    let runs = s.job_runs(&name, 5).await.unwrap();
                    if let Some(r) = runs.first().filter(|r| r.outcome != JobOutcome::Running) {
                        done = Some(r.clone());
                        break;
                    }
                }
                let r = done.expect("the run ends");
                eprintln!("{r:#?}");
                assert_eq!(r.outcome, JobOutcome::Succeeded, "{:?}", r.message);
                assert_eq!(r.trigger.as_deref(), Some("manual"));

                s.set_job_enabled(&name, false).await.unwrap();
                assert!(!s.job_detail(&name).await.unwrap().job.enabled);
                s.set_job_enabled(&name, true).await.unwrap();
                assert!(s.job_detail(&name).await.unwrap().job.enabled);

                let script = s.job_script(&name).await.unwrap();
                eprintln!("{script}");
                assert!(script.contains("--transfer_config") && script.contains("SELECT 1 AS x"));
                assert!(matches!(
                    s.stop_job(&name).await,
                    Err(Error::Unsupported(_))
                ));

                for evil in [
                    format!("{name}/../../x"),
                    format!("{name}?updateMask=name"),
                    "x'; DROP".into(),
                ] {
                    assert!(s.run_job(&evil).await.is_err(), "{evil}");
                    assert!(s.set_job_enabled(&evil, true).await.is_err(), "{evil}");
                }
            })
        };
        let outcome = body.await;
        // Cleanup before reporting: the scheduled query, then the dataset.
        if let Some(n) = parse_name(&name) {
            let url = dts.url(&Dts::config_segments(&n)).unwrap();
            if let Err(f) = dts.send(api.http.delete(url)).await {
                eprintln!("no se borró {name}: {}", f.error);
            }
        }
        let url = api.url(&["datasets", &ds]).unwrap();
        api.send(api.http.delete(url).query(&[("deleteContents", "true")]))
            .await
            .unwrap();
        if let Err(e) = outcome {
            std::panic::resume_unwind(e.into_panic());
        }
    }
}
