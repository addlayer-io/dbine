//! The model's refresh as server jobs and scheduler objects (spec §4.4,
//! §4.6). Owned by part C.
//!
//! Job ids: `refresh:<group>:<dataset>` (import schedule) and
//! `dqrefresh:<group>:<dataset>` (DirectQuery / LiveConnection cache), the
//! group a GUID or `me` for My workspace; both ids checked against their
//! shape and the cached listing before any request (§3.7). Every write
//! (`run_job`, `stop_job`, `set_job_enabled`, `scheduler_save`,
//! `scheduler_action`) is refused on read-only connections before any
//! request reaches the service (S1), whatever scopes the token carries.
//!
//! Endpoints (Microsoft Learn, "Datasets"): history `GET
//! …/refreshes?$top=N` (write permission), refresh `POST …/refreshes`,
//! cancel `DELETE …/refreshes/{requestId}` (enhanced refreshes only),
//! schedule `GET`/`PATCH …/refreshSchedule` (PATCH: the model's owner only),
//! DirectQuery cache `GET`/`PATCH …/directQueryRefreshSchedule`. `…` is
//! `/groups/{g}/datasets/{d}`, or `/datasets/{d}` for My workspace.

pub mod jobs;
pub mod scheduler;
pub mod timezones;

use crate::catalog::{valid_guid, Model};
use crate::http::Priority;
use crate::meta::service::Service;
use chrono::{DateTime, NaiveDateTime, SecondsFormat, Utc};
use dbine_driver::{Error, JobOutcome, Result};
use futures::stream::{self, StreamExt};
use serde_json::{json, Value};
use std::collections::BTreeMap;
use std::sync::Mutex;
use std::time::{Duration, Instant};

/// Which schedule a job is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// The import refresh schedule (`refreshSchedule`).
    Import,
    /// The DirectQuery / LiveConnection cache (`directQueryRefreshSchedule`).
    DirectQuery,
}

impl Kind {
    pub fn prefix(self) -> &'static str {
        match self {
            Kind::Import => "refresh",
            Kind::DirectQuery => "dqrefresh",
        }
    }

    /// What the job list calls it (Spanish).
    pub fn label(self) -> &'static str {
        match self {
            Kind::Import => "Actualización programada",
            Kind::DirectQuery => "Actualización de caché (DirectQuery)",
        }
    }

    pub fn endpoint(self) -> &'static str {
        match self {
            Kind::Import => "refreshSchedule",
            Kind::DirectQuery => "directQueryRefreshSchedule",
        }
    }
}

/// A job's id: `refresh:<group>:<dataset>` / `dqrefresh:<group>:<dataset>`.
pub fn job_id(kind: Kind, m: &Model) -> String {
    format!(
        "{}:{}:{}",
        kind.prefix(),
        m.workspace.id.as_deref().unwrap_or("me"),
        m.id
    )
}

/// A job id taken apart; anything but the exact shape is refused.
pub fn parse_job_id(id: &str) -> Result<(Kind, Option<String>, String)> {
    let bad = || {
        Error::Query(format!(
            "«{}» no es un trabajo de Power BI.",
            id.chars().take(80).collect::<String>()
        ))
    };
    let mut parts = id.split(':');
    let (Some(kind), Some(group), Some(dataset), None) =
        (parts.next(), parts.next(), parts.next(), parts.next())
    else {
        return Err(bad());
    };
    let kind = match kind {
        "refresh" => Kind::Import,
        "dqrefresh" => Kind::DirectQuery,
        _ => return Err(bad()),
    };
    let group = match group {
        "me" => None,
        g if valid_guid(g) => Some(g.to_ascii_lowercase()),
        _ => return Err(bad()),
    };
    if !valid_guid(dataset) {
        return Err(bad());
    }
    Ok((kind, group, dataset.to_ascii_lowercase()))
}

/// The model a job id names, checked against the listing (§3.7).
pub(crate) async fn job_model<S: Service>(s: &mut S, id: &str) -> Result<(Kind, Model)> {
    let (kind, group, dataset) = parse_job_id(id)?;
    let matches = |m: &Model| {
        m.id.eq_ignore_ascii_case(&dataset)
            && match (&group, &m.workspace.id) {
                (None, None) => true,
                (Some(g), Some(w)) => g.eq_ignore_ascii_case(w),
                _ => false,
            }
    };
    if let Some(m) = s.session_model().filter(|m| matches(m)) {
        return Ok((kind, m.clone()));
    }
    let models = s.workspace_models().await?;
    models
        .into_iter()
        .find(|m| matches(m))
        .map(|m| (kind, m))
        .ok_or_else(|| Error::Query("Ese modelo no está en la lista de esta conexión.".into()))
}

/// The cloud's API URL of a path, for the comment scripts.
pub(crate) fn api_url<S: Service>(s: &S, path: &str) -> String {
    format!("{}{path}", s.settings().cloud.api_base())
}

pub(crate) fn schedule_path(kind: Kind, m: &Model) -> String {
    format!("{}/{}", m.path(), kind.endpoint())
}

/// The API's day names, Sunday first, with their Spanish labels.
pub const DAYS: &[(&str, &str, &str)] = &[
    ("Sunday", "domingo", "dom"),
    ("Monday", "lunes", "lun"),
    ("Tuesday", "martes", "mar"),
    ("Wednesday", "miércoles", "mié"),
    ("Thursday", "jueves", "jue"),
    ("Friday", "viernes", "vie"),
    ("Saturday", "sábado", "sáb"),
];

/// A refresh schedule (`refreshSchedule` or `directQueryRefreshSchedule`).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Schedule {
    pub enabled: Option<bool>,
    pub days: Vec<String>,
    pub times: Vec<String>,
    pub time_zone: Option<String>,
    pub notify: Option<String>,
    /// DirectQuery: minutes between cache refreshes.
    pub frequency: Option<u64>,
}

impl Schedule {
    pub fn from_json(v: &Value) -> Schedule {
        let strs = |k: &str| -> Vec<String> {
            v.get(k)
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .filter_map(Value::as_str)
                        .map(str::to_string)
                        .collect()
                })
                .unwrap_or_default()
        };
        Schedule {
            enabled: v.get("enabled").and_then(Value::as_bool),
            days: strs("days"),
            times: strs("times"),
            time_zone: v
                .get("localTimeZoneId")
                .and_then(Value::as_str)
                .map(str::to_string),
            notify: v
                .get("notifyOption")
                .and_then(Value::as_str)
                .map(str::to_string),
            frequency: v.get("frequency").and_then(Value::as_u64),
        }
    }

    /// Whether it fires: the import schedule's `enabled`; a DirectQuery
    /// one when it has a frequency or times.
    pub fn fires(&self, kind: Kind) -> bool {
        match kind {
            Kind::Import => self.enabled.unwrap_or(false),
            Kind::DirectQuery => self.frequency.is_some() || !self.times.is_empty(),
        }
    }

    /// "lun, mié, vie · 05:00, 17:30 · UTC", "cada 15 min"; `None` when it
    /// says nothing.
    pub fn summary(&self) -> Option<String> {
        if let Some(f) = self.frequency {
            return Some(format!("cada {f} min"));
        }
        if self.times.is_empty() {
            return None;
        }
        let days: Vec<&str> = DAYS
            .iter()
            .filter(|(en, _, _)| self.days.iter().any(|d| d.eq_ignore_ascii_case(en)))
            .map(|(_, _, short)| *short)
            .collect();
        let mut parts = Vec::new();
        if !days.is_empty() {
            parts.push(days.join(", "));
        }
        parts.push(self.times.join(", "));
        if let Some(tz) = &self.time_zone {
            parts.push(tz.clone());
        }
        Some(parts.join(" · "))
    }

    /// The schedule as a PATCH body would set it.
    pub fn body(&self, kind: Kind) -> Value {
        let mut v = serde_json::Map::new();
        if kind == Kind::Import {
            if let Some(e) = self.enabled {
                v.insert("enabled".into(), json!(e));
            }
        }
        if let Some(f) = self.frequency {
            v.insert("frequency".into(), json!(f));
        } else {
            v.insert("days".into(), json!(self.days));
            v.insert("times".into(), json!(self.times));
            if let Some(tz) = &self.time_zone {
                v.insert("localTimeZoneId".into(), json!(tz));
            }
        }
        if kind == Kind::Import {
            if let Some(n) = &self.notify {
                v.insert("notifyOption".into(), json!(n));
            }
        }
        json!({ "value": v })
    }
}

/// One refresh of the history.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct Run {
    pub request_id: Option<String>,
    pub refresh_type: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub status: String,
    pub exception: Option<String>,
    pub attempts: Vec<Attempt>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Attempt {
    pub id: Option<i64>,
    pub kind: Option<String>,
    pub start: Option<String>,
    pub end: Option<String>,
    pub exception: Option<String>,
}

/// A service timestamp as RFC 3339 UTC: `2017-06-13T09:25:43.153Z` and
/// `2021-12-10T08:40:31.57` (no zone: UTC) both read.
pub fn utc(s: &str) -> Option<String> {
    let t = DateTime::parse_from_rfc3339(s)
        .map(|d| d.with_timezone(&Utc))
        .ok()
        .or_else(|| {
            NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f")
                .ok()
                .map(|n| n.and_utc())
        })?;
    Some(t.to_rfc3339_opts(SecondsFormat::Millis, true))
}

pub(crate) fn millis_of(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    let a = DateTime::parse_from_rfc3339(&utc(start?)?).ok()?;
    let b = DateTime::parse_from_rfc3339(&utc(end?)?).ok()?;
    u64::try_from((b - a).num_milliseconds()).ok()
}

/// `serviceExceptionJson`: its `errorCode` and, with `detail`, its
/// description (which may quote values: shown in the app, never logged).
pub fn exception_text(json: &str, detail: bool) -> String {
    let v: Value = serde_json::from_str(json).unwrap_or(Value::Null);
    let code = v.get("errorCode").and_then(Value::as_str);
    let desc = v.get("errorDescription").and_then(Value::as_str);
    match (code, desc) {
        (Some(c), Some(d)) if detail => {
            format!("{c}: {}", d.chars().take(1000).collect::<String>())
        }
        (Some(c), _) => c.to_string(),
        (None, Some(d)) if detail => d.chars().take(1000).collect(),
        _ => "error sin código".into(),
    }
}

impl Run {
    pub fn from_json(v: &Value) -> Run {
        let s = |v: &Value, k: &str| v.get(k).and_then(Value::as_str).map(str::to_string);
        Run {
            request_id: s(v, "requestId"),
            refresh_type: s(v, "refreshType"),
            start: s(v, "startTime"),
            end: s(v, "endTime"),
            status: s(v, "status").unwrap_or_default(),
            exception: s(v, "serviceExceptionJson"),
            attempts: v
                .get("refreshAttempts")
                .and_then(Value::as_array)
                .map(|a| {
                    a.iter()
                        .map(|x| Attempt {
                            id: x.get("attemptId").and_then(Value::as_i64),
                            kind: s(x, "type"),
                            start: s(x, "startTime"),
                            end: s(x, "endTime"),
                            exception: s(x, "serviceExceptionJson"),
                        })
                        .collect()
                })
                .unwrap_or_default(),
        }
    }

    /// A refresh still going: `Unknown` (or `NotStarted`) with no end.
    pub fn running(&self) -> bool {
        matches!(
            self.status.as_str(),
            "Unknown" | "NotStarted" | "InProgress"
        ) && self.end.is_none()
    }

    /// Started through the enhanced refresh API: the only kind that can be
    /// cancelled.
    pub fn enhanced(&self) -> bool {
        self.refresh_type.as_deref() == Some("ViaEnhancedApi")
    }

    pub fn outcome(&self) -> JobOutcome {
        match self.status.as_str() {
            "Completed" => JobOutcome::Succeeded,
            "Failed" => JobOutcome::Failed,
            "Cancelled" => JobOutcome::Canceled,
            "Disabled" => JobOutcome::Skipped,
            _ if self.running() => JobOutcome::Running,
            _ => JobOutcome::Unknown,
        }
    }

    pub fn duration_ms(&self) -> Option<u64> {
        millis_of(self.start.as_deref(), self.end.as_deref())
    }

    /// What started it (Spanish).
    pub fn trigger(&self) -> Option<String> {
        Some(
            match self.refresh_type.as_deref()? {
                "Scheduled" => "programada",
                "OnDemand" => "manual (portal)",
                "ViaApi" => "API",
                "ViaEnhancedApi" => "API mejorada",
                "ViaXmlaEndpoint" => "XMLA",
                "OnDemandTraining" => "manual con entrenamiento",
                other => return Some(other.to_string()),
            }
            .to_string(),
        )
    }
}

/// The history, newest first.
pub fn runs_from(v: &Value) -> Vec<Run> {
    v.get("value")
        .and_then(Value::as_array)
        .map(|a| a.iter().map(Run::from_json).collect())
        .unwrap_or_default()
}

/// What the jobs list and the scheduler know about one model.
#[derive(Debug, Clone)]
pub struct ModelRefresh {
    pub model: Model,
    pub kind: Kind,
    pub schedule: Option<Schedule>,
    /// The newest refreshes (two: the one going on and the last finished).
    pub recent: Vec<Run>,
}

impl ModelRefresh {
    pub fn running(&self) -> Option<&Run> {
        self.recent.first().filter(|r| r.running())
    }

    pub fn last_finished(&self) -> Option<&Run> {
        self.recent.iter().find(|r| !r.running())
    }
}

/// How long a model's schedule and newest refreshes are kept: the jobs
/// list and the scheduler poll, and each model costs two REST calls, so
/// it's the monitor's refresh-history time.
const SURVEY_TTL: Duration = Duration::from_secs(60);

/// The most models surveyed when the connection names no workspace (spec
/// §4.6 scopes jobs to the session's workspace).
const MAX_UNSCOPED: usize = 25;

type Surveyed = (Instant, Kind, Option<Schedule>, Vec<Run>);

static SURVEY: Mutex<BTreeMap<String, Surveyed>> = Mutex::new(BTreeMap::new());

fn survey_key<S: Service>(s: &S, m: &Model) -> Option<String> {
    let a = s.account()?;
    Some(format!("{}:{}|{}", a.tenant_id, a.object_id, m.id))
}

/// Forget what was read about `m` (after a refresh or a schedule change).
pub(crate) fn forget<S: Service>(s: &S, m: &Model) {
    if let (Some(k), Ok(mut c)) = (survey_key(s, m), SURVEY.lock()) {
        c.remove(&k);
    }
}

/// One model's schedule and newest refreshes.
pub(crate) async fn survey_one<S: Service>(s: &S, m: &Model) -> Option<ModelRefresh> {
    let key = survey_key(s, m);
    if let Some(k) = &key {
        let hit = SURVEY.lock().ok().and_then(|c| {
            c.get(k)
                .filter(|e| e.0.elapsed() < SURVEY_TTL)
                .map(|e| (e.1, e.2.clone(), e.3.clone()))
        });
        if let Some((kind, schedule, recent)) = hit {
            return Some(ModelRefresh {
                model: m.clone(),
                kind,
                schedule,
                recent,
            });
        }
    }
    let kind = if m.is_refreshable == Some(false) {
        Kind::DirectQuery
    } else {
        Kind::Import
    };
    let schedule = match s.get(&schedule_path(kind, m), Priority::Background).await {
        Ok(v) => Some(Schedule::from_json(&v)),
        // Not a DirectQuery model (a push or live model): not a job.
        Err(_) if kind == Kind::DirectQuery => return None,
        Err(_) => None,
    };
    let recent = if kind == Kind::Import && m.write_permission {
        s.get(
            &format!("{}/refreshes?$top=2", m.path()),
            Priority::Background,
        )
        .await
        .map(|v| runs_from(&v))
        .unwrap_or_default()
    } else {
        Vec::new()
    };
    if let (Some(k), Ok(mut c)) = (key, SURVEY.lock()) {
        c.retain(|_, e| e.0.elapsed() < SURVEY_TTL);
        c.insert(k, (Instant::now(), kind, schedule.clone(), recent.clone()));
    }
    Some(ModelRefresh {
        model: m.clone(),
        kind,
        schedule,
        recent,
    })
}

/// What [`survey`] read, and what it left out and why (Spanish notes).
#[derive(Debug, Default)]
pub(crate) struct Survey {
    pub models: Vec<ModelRefresh>,
    pub notes: Vec<String>,
}

/// The models of the session's workspace (or the connection's), four at a
/// time. Without either, `workspace_models` is every model of every
/// workspace: then only the ones the account can write to (the ones it can
/// refresh and whose history it sees), at most [`MAX_UNSCOPED`], and the
/// notes say to set a workspace.
pub(crate) async fn survey<S: Service>(s: &mut S) -> Result<Survey> {
    let mut models = s.workspace_models().await?;
    let mut notes = Vec::new();
    if s.session_model().is_none() && s.settings().workspace.is_none() {
        let before = models.len();
        models.retain(|m| m.write_permission);
        let no_write = before - models.len();
        if no_write > 0 {
            notes.push(format!(
                "Sin un área de trabajo en la conexión solo se listan los modelos con permiso de escritura ({no_write} quedaron fuera): indicá una para verlos todos."
            ));
        }
        if models.len() > MAX_UNSCOPED {
            notes.push(format!(
                "Se muestran {MAX_UNSCOPED} de {} modelos: con muchas áreas de trabajo conviene indicar una en la conexión.",
                models.len()
            ));
            models.truncate(MAX_UNSCOPED);
        }
    }
    let s: &S = s;
    let out: Vec<Option<ModelRefresh>> = stream::iter(models)
        .map(|m| async move { survey_one(s, &m).await })
        .buffered(4)
        .collect()
        .await;
    Ok(Survey {
        models: out.into_iter().flatten().collect(),
        notes,
    })
}

/// The request id of a refresh the service accepted: from `Location`
/// (`…/refreshes/<id>`) or `x-ms-request-id`, a GUID or nothing. Never
/// followed.
pub fn request_id(location: Option<&str>, header: Option<&str>) -> Option<String> {
    let from_location = location
        .and_then(|l| l.rsplit('/').next())
        .map(|id| id.split('?').next().unwrap_or(id));
    from_location
        .filter(|id| valid_guid(id))
        .or(header.filter(|id| valid_guid(id)))
        .map(str::to_ascii_lowercase)
}

/// A comment-only script: the `// ⚠` lines first, then the request and its
/// JSON body as `//` lines (the app shows it; the driver sends the request
/// itself).
pub fn comment_script(
    warnings: &[String],
    method: &str,
    url: &str,
    body: Option<&Value>,
) -> String {
    let mut out = String::new();
    for w in warnings {
        out.push_str(&format!("// ⚠ {}\n", w.replace('\n', " ")));
    }
    out.push_str(&format!("// {method} {url}\n"));
    if let Some(b) = body {
        let pretty = serde_json::to_string_pretty(b).unwrap_or_default();
        for l in pretty.lines() {
            out.push_str(&format!("// {l}\n"));
        }
    }
    out
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    pub(crate) const HISTORY: &str = include_str!("../../tests/fixtures/rest_refresh_history.json");
    pub(crate) const HISTORY_FAILED: &str =
        include_str!("../../tests/fixtures/rest_refresh_history_failed.json");
    pub(crate) const HISTORY_RUNNING: &str =
        include_str!("../../tests/fixtures/rest_refresh_history_running.json");
    pub(crate) const SCHEDULE: &str =
        include_str!("../../tests/fixtures/rest_refresh_schedule.json");
    pub(crate) const DQ_SCHEDULE: &str =
        include_str!("../../tests/fixtures/rest_dq_refresh_schedule.json");

    pub(crate) fn v(s: &str) -> Value {
        serde_json::from_str(s).unwrap()
    }

    /// Without a workspace on the connection (and no model in the
    /// session) the survey keeps to the models the account can write to,
    /// at most `MAX_UNSCOPED`, and says so; with one, it reads them all.
    #[tokio::test]
    async fn without_a_workspace_the_survey_is_bounded() {
        use crate::meta::fake::{config, Fake};
        let mut f = Fake::writer();
        f.model = None;
        let base = f.models[0].clone();
        f.models = (0..40)
            .map(|i| Model {
                id: format!("{i:08x}-0000-4000-8000-000000000000"),
                name: format!("Modelo {i}"),
                listed_as: format!("Finanzas › Modelo {i}"),
                // Every fourth one only readable.
                write_permission: i % 4 != 0,
                ..base.clone()
            })
            .collect();
        let got = survey(&mut f).await.unwrap();
        assert_eq!(got.models.len(), MAX_UNSCOPED);
        assert!(got.models.iter().all(|r| r.model.write_permission));
        assert_eq!(got.notes.len(), 2, "{:?}", got.notes);
        assert!(got.notes[0].contains("10 quedaron fuera"));
        assert!(got.notes[1].contains(&format!("{MAX_UNSCOPED} de 30")));
        // Two GETs per model surveyed, none for the rest.
        assert_eq!(f.requests().len(), 2 * MAX_UNSCOPED);
        // A workspace on the connection: every model, no notes.
        let mut f2 = Fake::writer();
        f2.model = None;
        f2.models = f.models.clone();
        f2.settings =
            crate::config::Settings::from_config(&config(false, &[("workspace", "Finanzas")]))
                .unwrap();
        let got = survey(&mut f2).await.unwrap();
        assert_eq!(got.models.len(), 40);
        assert!(got.notes.is_empty());
    }

    #[test]
    fn ids_have_one_shape() {
        let g = "f089354e-8366-4e18-aea3-4cb4a3a50b48";
        let d = "cfafbeb1-8037-4d0c-896e-a46fb27ff229";
        assert_eq!(
            parse_job_id(&format!("refresh:{g}:{d}")).unwrap(),
            (Kind::Import, Some(g.into()), d.into())
        );
        assert_eq!(
            parse_job_id(&format!("dqrefresh:me:{d}")).unwrap(),
            (Kind::DirectQuery, None, d.into())
        );
        for bad in [
            "",
            "refresh",
            &format!("refresh:{g}"),
            &format!("refresh:{g}:{d}:x"),
            &format!("other:{g}:{d}"),
            &format!("refresh:../x:{d}"),
            &format!("refresh:{g}:{d}/refreshes"),
            &format!("refresh:{g}:../../groups"),
        ] {
            assert!(parse_job_id(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn learns_history_samples_read() {
        let runs = runs_from(&v(HISTORY));
        assert_eq!(runs[0].outcome(), JobOutcome::Succeeded);
        assert_eq!(runs[0].trigger().as_deref(), Some("API"));
        assert_eq!(runs[0].duration_ms(), Some(360_000));
        assert_eq!(runs[0].attempts.len(), 2);
        let failed = runs_from(&v(HISTORY_FAILED));
        assert_eq!(failed[0].outcome(), JobOutcome::Failed);
        assert_eq!(
            exception_text(failed[0].exception.as_deref().unwrap(), true),
            "ModelRefreshFailed_CredentialsNotSpecified"
        );
        let running = runs_from(&v(HISTORY_RUNNING));
        assert!(running[0].running());
        assert_eq!(running[0].outcome(), JobOutcome::Running);
        assert_eq!(
            utc("2021-12-10T08:40:31.57").as_deref(),
            Some("2021-12-10T08:40:31.570Z")
        );
        assert_eq!(
            utc("2017-06-13T09:25:43.153Z").as_deref(),
            Some("2017-06-13T09:25:43.153Z")
        );
        assert_eq!(utc("nope"), None);
    }

    #[test]
    fn schedules_read_and_summarize() {
        let s = Schedule::from_json(&v(SCHEDULE));
        assert_eq!(
            s.summary().as_deref(),
            Some("dom, vie, sáb · 05:00, 11:30, 17:30, 23:00 · UTC")
        );
        assert!(s.fires(Kind::Import));
        assert_eq!(
            s.body(Kind::Import),
            serde_json::json!({"value": {"enabled": true, "days": ["Sunday", "Friday", "Saturday"], "times": ["05:00", "11:30", "17:30", "23:00"], "localTimeZoneId": "UTC", "notifyOption": "MailOnFailure"}})
        );
        let dq = Schedule::from_json(&v(DQ_SCHEDULE));
        assert_eq!(dq.summary().as_deref(), Some("cada 15 min"));
        assert!(dq.fires(Kind::DirectQuery));
        assert_eq!(
            dq.body(Kind::DirectQuery),
            serde_json::json!({"value": {"frequency": 15}})
        );
    }

    #[test]
    fn request_ids_are_parsed_never_trusted() {
        let id = "87f31ef7-1e3a-4006-9b0b-191693e79e9e";
        assert_eq!(
            request_id(
                Some(&format!("https://api.powerbi.com/v1.0/myorg/groups/f089354e-8366-4e18-aea3-4cb4a3a50b48/datasets/cfafbeb1-8037-4d0c-896e-a46fb27ff229/refreshes/{id}")),
                None
            )
            .as_deref(),
            Some(id)
        );
        assert_eq!(request_id(None, Some(id)).as_deref(), Some(id));
        assert_eq!(
            request_id(Some("https://evil.example/x"), Some("../x")),
            None
        );
    }

    #[test]
    fn scripts_are_comments_with_the_warnings_first() {
        let s = comment_script(
            &["Solo el propietario".into()],
            "PATCH",
            "https://api.powerbi.com/v1.0/myorg/x",
            Some(&serde_json::json!({"value": {"enabled": false}})),
        );
        assert!(s.starts_with(
            "// ⚠ Solo el propietario\n// PATCH https://api.powerbi.com/v1.0/myorg/x\n// {\n"
        ));
        assert!(s.lines().all(|l| l.starts_with("//")));
    }
}
