//! Server jobs ([`dbine_driver::Session::jobs`]).
//!
//! - Elasticsearch: transforms (`_transform`, with their state from
//!   `_transform/_all/_stats`; started and stopped with `_start` / `_stop`,
//!   a failed one with `_stop?force=true`, a continuous one checks for
//!   changes now with `_schedule_now`, 8.7 on) and Watcher watches
//!   (`_watcher/_query/watches`; `_execute` runs one now and records it,
//!   `_activate` / `_deactivate`, runs from `.watcher-history-*` when the
//!   user can read it). Watcher needs a gold, platinum, enterprise or trial
//!   license: on basic the list says so.
//! - OpenSearch and Open Distro (`_plugins` / `_opendistro`): the Index
//!   Management plugin's transforms (`_start` / `_stop`, their last run from
//!   `_explain`) and the Alerting plugin's monitors (`_execute` runs one now;
//!   enabling sends the monitor back whole with `enabled` changed, except
//!   for monitors another plugin manages, which the REST update refuses).
//!   Neither keeps a history of runs. ISM policies, rollups and snapshot
//!   policies are left out: they're per-index state or managed elsewhere.
//!
//! A listing the user may not read says so in a note, instead of saying
//! the feature isn't there.
//!
//! Ids are `transform:<id>`, `watch:<id>` and `monitor:<id>`. The id that
//! comes back has to name one object (no `*`, `,`, `_all` or dot-only
//! names; an Elasticsearch transform id has to follow Elasticsearch's own
//! rule), goes into the URL percent-encoded ([`path_segment`]), and what
//! the cluster answers has to carry that same id.

use crate::ddl::path_segment;
use crate::http;
use crate::json::J;
use crate::{es_error_message, EsSession};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, JobStepRun, PropertyInfo, Result,
    ServerJob,
};
use std::collections::BTreeMap;
use std::time::Duration;

const MAX_MESSAGE: usize = 4000;
/// Listed per page.
const PAGE: usize = 1000;
/// How far a search can page with `from` (`index.max_result_window`).
const MAX_SEARCH_WINDOW: usize = 10_000;
/// How long `run_job` waits on a synchronous `_execute` for an early
/// refusal before leaving the run going on its own.
const EXECUTE_WAIT: Duration = Duration::from_secs(3);

const TRANSFORM: &str = "Transform";
const WATCH: &str = "Watch";
const MONITOR: &str = "Monitor";

const NO_WATCHER_LICENSE: &str = "Watcher no está disponible con la licencia de este clúster: hace falta una licencia gold, platinum, enterprise o de prueba";
const NO_WATCHER: &str = "Watcher no está habilitado en este clúster";
const NO_TRANSFORMS: &str =
    "este clúster no tiene transforms (hacen falta X-Pack y un nodo con el rol transform)";
const NO_OS_TRANSFORMS: &str =
    "este clúster no tiene el plugin Index Management: no hay transforms";
const NO_TRANSFORM_LIST: &str =
    "esta versión no tiene una API que liste los transforms: no se muestran";
const NO_ALERTING: &str = "este clúster no tiene el plugin Alerting: no hay monitores";
const OS_SCOPE: &str = "se listan los transforms y los monitores de alertas; las políticas ISM, los rollups y las políticas de snapshots no";
const DENIED_TRANSFORMS: &str =
    "este usuario no puede ver los transforms (hace falta el privilegio monitor_transform)";
const DENIED_WATCHES: &str =
    "este usuario no puede ver los watches (hace falta el privilegio monitor_watcher)";
const DENIED_WATCH_HISTORY: &str =
    "este usuario no puede leer el historial de Watcher (.watcher-history-*): los watches se muestran sin historial";
const DENIED_OS_TRANSFORMS: &str =
    "este usuario no puede ver los transforms (faltan permisos de Index Management)";
const DENIED_MONITORS: &str =
    "este usuario no puede ver los monitores (faltan permisos de Alerting)";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    Transform,
    Watch,
    Monitor,
}

impl Kind {
    fn prefix(self) -> &'static str {
        match self {
            Kind::Transform => "transform",
            Kind::Watch => "watch",
            Kind::Monitor => "monitor",
        }
    }
}

pub(crate) fn job_id(kind: Kind, id: &str) -> String {
    format!("{}:{id}", kind.prefix())
}

/// The kind and the engine's id of an id [`job_id`] made.
pub(crate) fn parse_id(id: &str) -> Option<(Kind, &str)> {
    let (k, rest) = id.split_once(':')?;
    let kind = match k {
        "transform" => Kind::Transform,
        "watch" => Kind::Watch,
        "monitor" => Kind::Monitor,
        _ => return None,
    };
    one_name(rest).then_some((kind, rest))
}

/// An engine id that names one object in a path: not empty, no control
/// characters or surrounding spaces, not only dots (the URL drops `.` and
/// `..` segments) and nothing the cluster reads as several (`*`, `,`,
/// `_all`).
fn one_name(id: &str) -> bool {
    !id.is_empty()
        && id.len() <= 512
        && id.trim() == id
        && !id.chars().any(|c| c.is_control() || c == '*' || c == ',')
        && id.chars().any(|c| c != '.')
        && id != "_all"
}

/// Elasticsearch's rule for transform ids: lowercase letters, digits, `-`
/// and `_`, starting and ending with a letter or digit, 64 at most.
pub(crate) fn es_transform_id(id: &str) -> bool {
    let b = id.as_bytes();
    (1..=64).contains(&b.len())
        && b.iter()
            .all(|c| matches!(c, b'a'..=b'z' | b'0'..=b'9' | b'_' | b'-'))
        && b[0].is_ascii_alphanumeric()
        && b[b.len() - 1].is_ascii_alphanumeric()
}

/// A listing the user isn't allowed to read (a license refusal is also a
/// `security_exception`, so that's checked first).
fn denied(status: u16, body: &J) -> bool {
    let t = body.text();
    status == 401 || status == 403 || t.contains("security_exception")
}

fn license_refusal(body: &J) -> bool {
    body.text().contains("license")
}

fn bad_id(id: &str) -> Error {
    Error::Query(format!(
        "«{}» no es un id de job de este clúster",
        id.chars().take(200).collect::<String>()
    ))
}

fn cut(s: &str) -> String {
    s.chars().take(MAX_MESSAGE).collect()
}

fn text(v: Option<&J>) -> Option<String> {
    v.map(J::text)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn num(v: Option<&J>) -> Option<f64> {
    match v? {
        J::Num(n) => n.as_f64(),
        J::Str(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Milliseconds since 1970 as RFC 3339 in UTC, to the second.
pub(crate) fn rfc3339_ms(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
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

/// An ISO 8601 time (`…Z` or `…±hh:mm`, any fraction) as milliseconds.
pub(crate) fn iso_ms(s: &str) -> Option<i64> {
    let s = s.trim();
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let rest = s.get(19..)?;
    let (frac, zone) = match rest.strip_prefix('.') {
        Some(r) => {
            let end = r.find(|c: char| !c.is_ascii_digit()).unwrap_or(r.len());
            (&r[..end], &r[end..])
        }
        None => ("", rest),
    };
    let ms = format!("{:0<3}", &frac[..frac.len().min(3)])
        .parse::<i64>()
        .unwrap_or(0);
    let offset = match zone {
        "" | "Z" | "z" => 0,
        z => {
            let sign = if z.starts_with('-') { -1 } else { 1 };
            let hh = z.get(1..3)?.parse::<i64>().ok()?;
            let mm = z.get(4..6).or_else(|| z.get(3..5))?.parse::<i64>().ok()?;
            sign * (hh * 3600 + mm * 60)
        }
    };
    let y2 = if mo <= 2 { y - 1 } else { y };
    let era = y2.div_euclid(400);
    let yoe = y2 - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some((days * 86_400 + h * 3600 + mi * 60 + se - offset) * 1000 + ms)
}

fn iso(v: Option<&J>) -> Option<String> {
    text(v).and_then(|s| iso_ms(&s)).map(rfc3339_ms)
}

fn millis(v: Option<&J>) -> Option<String> {
    num(v).filter(|m| *m > 0.0).map(|m| rfc3339_ms(m as i64))
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

/// `{ "k": … }` → `k <compact value>`; a string value stands alone.
fn one_entry(v: Option<&J>) -> Option<String> {
    let (k, v) = v?.as_obj()?.first()?;
    Some(match v {
        J::Str(s) => format!("{k} {s}"),
        v => format!("{k} {}", v.compact()),
    })
}

/// A JSON object without the given keys.
fn without(v: &J, drop: &[&str]) -> J {
    match v {
        J::Obj(o) => J::Obj(
            o.iter()
                .filter(|(k, _)| !drop.contains(&k.as_str()))
                .cloned()
                .collect(),
        ),
        v => v.clone(),
    }
}

/// The `major.minor` in "Elasticsearch 8.15.3".
fn version_at_least(version: &str, major: u64, minor: u64) -> bool {
    let num = version
        .split_whitespace()
        .find(|w| w.chars().next().is_some_and(|c| c.is_ascii_digit()))
        .unwrap_or("");
    let mut parts = num.split('.').map(|p| p.parse::<u64>().unwrap_or(0));
    let (a, b) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (a, b) >= (major, minor)
}

// ------------------------------------------------- Elasticsearch transforms

/// A transform (`_transform`) and its stats as a job.
pub(crate) fn es_transform_job(t: &J, stats: Option<&J>, schedule_now: bool) -> ServerJob {
    let id = text(t.get("id")).unwrap_or_default();
    let state = stats.and_then(|s| text(s.get("state"))).unwrap_or_default();
    let continuous = t.get("sync").is_some();
    let checkpoint = stats
        .and_then(|s| num(s.at(&["checkpointing", "last", "checkpoint"])))
        .unwrap_or(0.0);
    let started = matches!(state.as_str(), "started" | "indexing");
    ServerJob {
        id: job_id(Kind::Transform, &id),
        name: id,
        kind: TRANSFORM.into(),
        group: None,
        description: text(t.get("description")),
        owner: None,
        enabled: !state.is_empty() && state != "stopped",
        schedule: continuous.then(|| {
            format!(
                "frequency {}",
                text(t.get("frequency")).unwrap_or_else(|| "1m".into())
            )
        }),
        running: state == "indexing",
        last_outcome: if state == "failed" {
            Some(JobOutcome::Failed)
        } else if checkpoint > 0.0 {
            Some(JobOutcome::Succeeded)
        } else {
            None
        },
        last_started: stats
            .and_then(|s| millis(s.at(&["checkpointing", "last", "timestamp_millis"]))),
        last_duration_ms: None,
        next_run: None,
        // `_schedule_now` checks a started continuous transform now; a
        // stopped one is run by starting it.
        can_run: continuous && started && schedule_now,
        can_stop: false,
        can_enable: true,
        has_history: false,
        has_script: true,
        // Installed and managed by Elastic (Fleet integrations, Kibana's
        // own), what Kibana badges as managed.
        system: t.at(&["_meta", "managed"]).and_then(J::as_bool) == Some(true),
    }
}

pub(crate) fn es_transform_detail(t: &J, stats: Option<&J>, schedule_now: bool) -> JobDetail {
    let job = es_transform_job(t, stats, schedule_now);
    let mut p = Vec::new();
    let s = |path: &[&str]| stats.and_then(|s| text(s.at(path)));
    add(&mut p, "", "Estado", s(&["state"]));
    add(&mut p, "", "Motivo", s(&["reason"]).map(|r| cut(&r)));
    add(&mut p, "", "Salud", s(&["health", "status"]));
    add(
        &mut p,
        "",
        "Origen",
        t.at(&["source", "index"]).map(|i| match i {
            J::Arr(a) => a.iter().map(J::text).collect::<Vec<_>>().join(", "),
            i => i.text(),
        }),
    );
    add(&mut p, "", "Destino", text(t.at(&["dest", "index"])));
    add(&mut p, "", "Frecuencia", text(t.get("frequency")));
    add(
        &mut p,
        "",
        "Campo de sincronización",
        text(t.at(&["sync", "time", "field"])),
    );
    add(&mut p, "", "Demora", text(t.at(&["sync", "time", "delay"])));
    add(&mut p, "", "Creado", millis(t.get("create_time")));
    add(&mut p, "", "Versión", text(t.get("version")));
    const STATS: &str = "Estadísticas";
    add(
        &mut p,
        STATS,
        "Último checkpoint",
        s(&["checkpointing", "last", "checkpoint"]),
    );
    add(&mut p, STATS, "Disparos", s(&["stats", "trigger_count"]));
    add(
        &mut p,
        STATS,
        "Documentos procesados",
        s(&["stats", "documents_processed"]),
    );
    add(
        &mut p,
        STATS,
        "Documentos indexados",
        s(&["stats", "documents_indexed"]),
    );
    add(
        &mut p,
        STATS,
        "Fallos de búsqueda",
        s(&["stats", "search_failures"]),
    );
    add(
        &mut p,
        STATS,
        "Fallos de indexado",
        s(&["stats", "index_failures"]),
    );
    let (kind, body) = if let Some(l) = t.get("latest") {
        ("latest", l)
    } else {
        ("pivot", t.get("pivot").unwrap_or(&J::Null))
    };
    JobDetail {
        steps: vec![JobStep {
            name: job.name.clone(),
            kind: Some(kind.into()),
            target: text(t.at(&["dest", "index"])),
            command: Some(body.pretty()),
        }],
        job,
        schedules: Vec::new(),
        properties: p,
        notes: Vec::new(),
    }
}

/// `PUT _transform/<id>` with what defines it.
pub(crate) fn es_transform_script(t: &J) -> String {
    let id = text(t.get("id")).unwrap_or_default();
    let body = without(t, &["id", "version", "create_time", "authorization"]);
    format!("PUT /_transform/{}\n{}\n", path_segment(&id), body.pretty())
}

// ------------------------------------------------------------- Watcher

fn watch_schedule(w: &J) -> Option<String> {
    let s = w.at(&["trigger", "schedule"])?;
    if let Some(i) = text(s.get("interval")) {
        return Some(format!("interval {i}"));
    }
    if let Some(c) = s.get("cron") {
        return Some(match c {
            J::Arr(a) => format!(
                "cron {}",
                a.iter().map(J::text).collect::<Vec<_>>().join(" | ")
            ),
            c => format!("cron {}", c.text()),
        });
    }
    one_entry(Some(s))
}

fn watch_outcome(status: &J) -> Option<JobOutcome> {
    let state = text(status.get("execution_state"))?;
    let failed_action = status
        .get("actions")
        .and_then(J::as_obj)
        .into_iter()
        .flatten()
        .any(|(_, a)| a.at(&["last_execution", "successful"]).and_then(J::as_bool) == Some(false));
    Some(match state.as_str() {
        "executed" if failed_action => JobOutcome::Failed,
        s => record_outcome(s),
    })
}

/// A history record's `state`.
fn record_outcome(state: &str) -> JobOutcome {
    match state {
        "executed" | "acknowledged" => JobOutcome::Succeeded,
        "execution_not_needed"
        | "throttled"
        | "not_executed_already_queued"
        | "not_executed_watch_missing" => JobOutcome::Skipped,
        "failed" | "executed_multiple_times" => JobOutcome::Failed,
        "deleted_while_queued" => JobOutcome::Canceled,
        "awaits_execution"
        | "started"
        | "input_executed"
        | "condition_executed"
        | "watch_transform_executed"
        | "actions_executed" => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

/// A hit of `_watcher/_query/watches` as a job; `history`: the user can
/// read `.watcher-history-*`.
pub(crate) fn watch_job(hit: &J, history: bool) -> ServerJob {
    let id = text(hit.get("_id")).unwrap_or_default();
    let w = hit.get("watch").unwrap_or(&J::Null);
    let status = hit.get("status").unwrap_or(&J::Null);
    ServerJob {
        id: job_id(Kind::Watch, &id),
        name: id,
        kind: WATCH.into(),
        group: None,
        description: text(w.at(&["metadata", "name"])),
        owner: None,
        enabled: status
            .at(&["state", "active"])
            .and_then(J::as_bool)
            .unwrap_or(true),
        schedule: watch_schedule(w),
        running: false,
        last_outcome: watch_outcome(status),
        last_started: iso(status.get("last_checked")),
        last_duration_ms: None,
        next_run: None,
        // `_execute` runs an inactive watch too.
        can_run: true,
        can_stop: false,
        can_enable: true,
        has_history: history,
        has_script: true,
        // Monitoring's cluster alerts, which Elasticsearch creates itself.
        system: text(w.at(&["metadata", "xpack", "type"])).as_deref() == Some("monitoring"),
    }
}

pub(crate) fn watch_detail(hit: &J, history: bool) -> JobDetail {
    let job = watch_job(hit, history);
    let w = hit.get("watch").unwrap_or(&J::Null);
    let status = hit.get("status").unwrap_or(&J::Null);
    let mut p = Vec::new();
    add(
        &mut p,
        "",
        "Entrada",
        w.get("input")
            .and_then(J::as_obj)
            .and_then(|o| o.first())
            .map(|(k, _)| k.clone()),
    );
    add(&mut p, "", "Condición", w.get("condition").map(J::compact));
    add(
        &mut p,
        "",
        "Espera entre acciones",
        text(w.get("throttle_period")),
    );
    add(
        &mut p,
        "",
        "Última comprobación",
        iso(status.get("last_checked")),
    );
    add(
        &mut p,
        "",
        "Última vez que se cumplió",
        iso(status.get("last_met_condition")),
    );
    add(
        &mut p,
        "",
        "Estado de la última ejecución",
        text(status.get("execution_state")),
    );
    add(&mut p, "", "Versión", text(status.get("version")));
    let steps = w
        .get("actions")
        .and_then(J::as_obj)
        .into_iter()
        .flatten()
        .map(|(name, a)| {
            let kind = a
                .as_obj()
                .into_iter()
                .flatten()
                .map(|(k, _)| k.as_str())
                .find(|k| {
                    !matches!(
                        *k,
                        "throttle_period"
                            | "condition"
                            | "transform"
                            | "foreach"
                            | "max_iterations"
                    )
                })
                .map(str::to_string);
            JobStep {
                name: name.clone(),
                kind,
                target: None,
                command: Some(a.pretty()),
            }
        })
        .collect();
    JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties: p,
        notes: Vec::new(),
    }
}

/// `PUT _watcher/watch/<id>` with the watch (inactive ones with
/// `?active=false`).
pub(crate) fn watch_script(hit: &J) -> String {
    let id = text(hit.get("_id")).unwrap_or_default();
    let w = hit.get("watch").unwrap_or(&J::Null);
    let active = hit
        .at(&["status", "state", "active"])
        .and_then(J::as_bool)
        .unwrap_or(true);
    format!(
        "PUT /_watcher/watch/{}{}\n{}\n",
        path_segment(&id),
        if active { "" } else { "?active=false" },
        without(w, &["status"]).pretty()
    )
}

/// A `.watcher-history-*` record as a run.
pub(crate) fn watch_run(id: Option<String>, r: &J) -> JobRun {
    let state = text(r.get("state")).unwrap_or_default();
    let steps: Vec<JobStepRun> = r
        .at(&["result", "actions"])
        .and_then(J::as_arr)
        .into_iter()
        .flatten()
        .map(|a| {
            let status = text(a.get("status")).unwrap_or_default();
            JobStepRun {
                name: text(a.get("id")).unwrap_or_default(),
                outcome: match status.as_str() {
                    "success" => JobOutcome::Succeeded,
                    "failure" => JobOutcome::Failed,
                    "throttled" | "acknowledged" | "simulated" | "condition_failed" => {
                        JobOutcome::Skipped
                    }
                    _ => JobOutcome::Unknown,
                },
                started: None,
                duration_ms: None,
                message: text(a.get("reason"))
                    .or_else(|| text(a.at(&["error", "reason"])))
                    .or_else(|| a.get("error").map(J::compact))
                    .map(|m| cut(&m)),
            }
        })
        .collect();
    let mut outcome = record_outcome(&state);
    if outcome == JobOutcome::Succeeded && steps.iter().any(|s| s.outcome == JobOutcome::Failed) {
        outcome = JobOutcome::Failed;
    }
    let mut message: Vec<String> = r
        .get("messages")
        .and_then(J::as_arr)
        .into_iter()
        .flatten()
        .map(J::text)
        .filter(|m| !m.is_empty())
        .collect();
    if let Some(e) = text(r.at(&["exception", "reason"])).or_else(|| text(r.get("exception"))) {
        message.push(e);
    }
    JobRun {
        id,
        started: iso(r.at(&["result", "execution_time"]))
            .or_else(|| iso(r.at(&["trigger_event", "triggered_time"]))),
        duration_ms: num(r.at(&["result", "execution_duration"])).map(|d| d.max(0.0) as u64),
        outcome,
        trigger: text(r.at(&["trigger_event", "type"])).map(|t| match t.as_str() {
            "manual" => "manual".to_string(),
            "schedule" => "programada".to_string(),
            t => t.to_string(),
        }),
        message: (!message.is_empty()).then(|| cut(&message.join("\n"))),
        steps,
    }
}

// ------------------------------------------ OpenSearch / Open Distro

fn os_schedule(s: Option<&J>) -> Option<String> {
    let s = s?;
    if let Some(i) = s.get("interval").or_else(|| s.get("period")) {
        return Some(format!(
            "every {} {}",
            text(i.get("period").or_else(|| i.get("interval"))).unwrap_or_default(),
            text(i.get("unit")).unwrap_or_default()
        ));
    }
    if let Some(c) = s.get("cron") {
        let expr = text(c.get("expression")).unwrap_or_default();
        return Some(match text(c.get("timezone")) {
            Some(tz) => format!("cron {expr} ({tz})"),
            None => format!("cron {expr}"),
        });
    }
    Some(s.compact())
}

/// A transform (`{ "_id", "transform": {…} }`) and its `_explain` entry.
pub(crate) fn os_transform_job(hit: &J, explain: Option<&J>) -> ServerJob {
    let t = hit.get("transform").unwrap_or(&J::Null);
    let id = text(hit.get("_id"))
        .or_else(|| text(t.get("transform_id")))
        .unwrap_or_default();
    let meta = explain.and_then(|e| e.get("transform_metadata"));
    let status = meta.and_then(|m| text(m.get("status"))).unwrap_or_default();
    ServerJob {
        id: job_id(Kind::Transform, &id),
        name: id,
        kind: TRANSFORM.into(),
        group: None,
        description: text(t.get("description")),
        owner: None,
        enabled: t.get("enabled").and_then(J::as_bool).unwrap_or(false),
        schedule: os_schedule(t.get("schedule")),
        running: false,
        last_outcome: match status.as_str() {
            "failed" => Some(JobOutcome::Failed),
            "finished" | "started" | "stopped" if meta.and_then(|m| m.get("stats")).is_some() => {
                Some(JobOutcome::Succeeded)
            }
            _ => None,
        },
        last_started: meta.and_then(|m| millis(m.get("last_updated_at"))),
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: true,
        has_history: false,
        has_script: true,
        system: false,
    }
}

pub(crate) fn os_transform_detail(hit: &J, explain: Option<&J>) -> JobDetail {
    let job = os_transform_job(hit, explain);
    let t = hit.get("transform").unwrap_or(&J::Null);
    let meta = explain.and_then(|e| e.get("transform_metadata"));
    let m = |path: &[&str]| meta.and_then(|m| text(m.at(path)));
    let mut p = Vec::new();
    add(&mut p, "", "Estado", m(&["status"]));
    add(
        &mut p,
        "",
        "Motivo del fallo",
        m(&["failure_reason"]).map(|r| cut(&r)),
    );
    add(&mut p, "", "Origen", text(t.get("source_index")));
    add(&mut p, "", "Destino", text(t.get("target_index")));
    add(
        &mut p,
        "",
        "Continuo",
        t.get("continuous")
            .and_then(J::as_bool)
            .map(|c| if c { "Sí" } else { "No" }.to_string()),
    );
    add(&mut p, "", "Habilitado desde", millis(t.get("enabled_at")));
    add(&mut p, "", "Modificado", millis(t.get("updated_at")));
    const STATS: &str = "Estadísticas";
    add(
        &mut p,
        STATS,
        "Páginas procesadas",
        m(&["stats", "pages_processed"]),
    );
    add(
        &mut p,
        STATS,
        "Documentos procesados",
        m(&["stats", "documents_processed"]),
    );
    add(
        &mut p,
        STATS,
        "Documentos indexados",
        m(&["stats", "documents_indexed"]),
    );
    let mut body = Vec::new();
    for k in ["groups", "aggregations"] {
        if let Some(v) = t.get(k) {
            body.push((k.to_string(), v.clone()));
        }
    }
    JobDetail {
        steps: vec![JobStep {
            name: job.name.clone(),
            kind: Some("transform".into()),
            target: text(t.get("target_index")),
            command: Some(J::Obj(body).pretty()),
        }],
        job,
        schedules: Vec::new(),
        properties: p,
        notes: Vec::new(),
    }
}

pub(crate) fn os_transform_script(prefix: &str, hit: &J) -> String {
    let t = hit.get("transform").unwrap_or(&J::Null);
    let id = text(hit.get("_id"))
        .or_else(|| text(t.get("transform_id")))
        .unwrap_or_default();
    let body = J::Obj(vec![(
        "transform".into(),
        without(
            t,
            &[
                "transform_id",
                "schema_version",
                "metadata_id",
                "updated_at",
                "enabled_at",
                "roles",
                "user",
            ],
        ),
    )]);
    format!(
        "PUT /{prefix}/_transform/{}\n{}\n",
        path_segment(&id),
        body.pretty()
    )
}

/// Where the Alerting plugin keeps a monitor's alerts, findings and
/// queries by default.
const DEFAULT_DATA_SOURCES: &[(&str, &str)] = &[
    ("query_index", ".opensearch-alerting-queries"),
    (
        "findings_index",
        ".opensearch-alerting-finding-history-write",
    ),
    ("alerts_index", ".opendistro-alerting-alerts"),
    (
        "alerts_history_index",
        ".opendistro-alerting-alert-history-write",
    ),
];

/// The plugin that manages a monitor when it isn't Alerting's own (Security
/// Analytics' detectors make monitors with their own owner and indices).
/// The REST update refuses such a monitor ("Invalid owner field", "Custom
/// Data Sources are not allowed"), so it isn't enabled from here.
pub(crate) fn os_monitor_owner(m: &J) -> Option<String> {
    let owner = text(m.get("owner")).filter(|o| o != "alerting");
    let custom = m.get("data_sources").is_some_and(|d| {
        DEFAULT_DATA_SOURCES
            .iter()
            .any(|(k, v)| text(d.get(k)).is_some_and(|x| x != *v))
    });
    owner.or_else(|| custom.then(|| "otro plugin".to_string()))
}

/// A monitor (`_source` of a search hit, or `monitor` of a GET).
pub(crate) fn os_monitor_job(id: &str, m: &J) -> ServerJob {
    ServerJob {
        id: job_id(Kind::Monitor, id),
        name: text(m.get("name")).unwrap_or_else(|| id.to_string()),
        kind: MONITOR.into(),
        group: text(m.get("monitor_type")),
        description: None,
        owner: text(m.at(&["user", "name"])),
        enabled: m.get("enabled").and_then(J::as_bool).unwrap_or(false),
        schedule: os_schedule(m.get("schedule")),
        running: false,
        last_outcome: None,
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: true,
        can_stop: false,
        can_enable: os_monitor_owner(m).is_none(),
        has_history: false,
        has_script: true,
        system: false,
    }
}

pub(crate) fn os_monitor_detail(id: &str, m: &J) -> JobDetail {
    let job = os_monitor_job(id, m);
    let mut p = Vec::new();
    add(&mut p, "", "Tipo", text(m.get("monitor_type")));
    add(
        &mut p,
        "",
        "Habilitado desde",
        millis(m.get("enabled_time")),
    );
    add(&mut p, "", "Modificado", millis(m.get("last_update_time")));
    let inputs = m.get("inputs").map(J::pretty);
    let mut steps = vec![JobStep {
        name: "Entradas".into(),
        kind: Some("consulta".into()),
        target: None,
        command: inputs,
    }];
    for t in m.get("triggers").and_then(J::as_arr).into_iter().flatten() {
        // `{ "query_level_trigger": { name, condition, actions } }`.
        let (kind, body) = match t.as_obj().and_then(|o| o.first()) {
            Some((k, v)) => (k.clone(), v),
            None => ("trigger".to_string(), t),
        };
        steps.push(JobStep {
            name: text(body.get("name")).unwrap_or_default(),
            kind: Some(kind),
            target: None,
            command: Some(body.pretty()),
        });
    }
    JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties: p,
        notes: os_monitor_owner(m)
            .map(|owner| {
                format!("este monitor lo administra {owner}: se habilita o deshabilita desde ese plugin")
            })
            .into_iter()
            .collect(),
    }
}

/// The monitor as a create request (without what the server sets).
fn os_monitor_body(m: &J) -> J {
    without(
        m,
        &[
            "schema_version",
            "last_update_time",
            "enabled_time",
            "user",
            "data_sources",
            "owner",
        ],
    )
}

pub(crate) fn os_monitor_script(prefix: &str, m: &J) -> String {
    format!(
        "POST /{prefix}/_alerting/monitors\n{}\n",
        os_monitor_body(m).pretty()
    )
}

/// The monitor with `enabled` set. The update replaces the whole monitor,
/// so everything the GET gave is sent back (its `owner` and `data_sources`
/// too) except what the server sets itself.
pub(crate) fn os_monitor_enabled(m: &J, enabled: bool) -> J {
    let mut body = without(
        m,
        &["schema_version", "last_update_time", "enabled_time", "user"],
    );
    if let J::Obj(o) = &mut body {
        match o.iter_mut().find(|(k, _)| k == "enabled") {
            Some((_, v)) => *v = J::Bool(enabled),
            None => o.push(("enabled".into(), J::Bool(enabled))),
        }
    }
    body
}

// ------------------------------------------------------------- session

impl EsSession {
    /// `_plugins` (OpenSearch) or `_opendistro` (Open Distro).
    fn plugins(&self) -> &'static str {
        if self.opendistro {
            "_opendistro"
        } else {
            "_plugins"
        }
    }

    /// Sends and answers the status with the parsed body.
    async fn job_call(&self, method: &str, path: &str, body: Option<&J>) -> Result<(u16, J)> {
        let mut rb = self.request(method, path);
        if let Some(b) = body {
            rb = rb
                .header("Content-Type", "application/json")
                .body(b.compact());
        }
        let (status, text) = http::send(rb).await?;
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &text)));
        }
        Ok((status, J::parse(&text).unwrap_or(J::Null)))
    }

    /// Like [`Self::job_call`], but a 4xx comes back as the status and body.
    async fn job_probe(&self, method: &str, path: &str, body: Option<&J>) -> Result<(u16, J)> {
        let mut rb = self.request(method, path);
        if let Some(b) = body {
            rb = rb
                .header("Content-Type", "application/json")
                .body(b.compact());
        }
        let (status, text) = http::send(rb).await?;
        if status >= 500 {
            return Err(Error::Query(es_error_message(status, &text)));
        }
        Ok((status, J::parse(&text).unwrap_or(J::Null)))
    }

    fn check_writable(&self, what: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(format!(
                "Conexión de solo lectura: no se puede {what}."
            )));
        }
        Ok(())
    }

    /// Sends a synchronous `_execute` on its own and returns once the
    /// server took it (an early error comes back as the error).
    async fn execute_detached(&self, path: String, body: J) -> Result<()> {
        let rb = self
            .request("POST", &path)
            .header("Content-Type", "application/json")
            .body(body.compact());
        let mut run = tokio::spawn(async move {
            let (status, text) = http::send(rb).await?;
            if status >= 400 {
                return Err(Error::Query(es_error_message(status, &text)));
            }
            Ok(())
        });
        match tokio::time::timeout(EXECUTE_WAIT, &mut run).await {
            Ok(joined) => joined.map_err(|e| Error::Query(e.to_string()))?,
            Err(_) => Ok(()),
        }
    }

    pub(crate) async fn jobs_list(&self) -> Result<JobList> {
        if self.opensearch {
            self.os_jobs().await
        } else {
            self.es_jobs().await
        }
    }

    // ---- Elasticsearch

    fn schedule_now(&self) -> bool {
        version_at_least(&self.version, 8, 7)
    }

    /// Every transform with its stats; `Err` with the note when they can't
    /// be listed (no X-Pack transforms, or the user may not see them).
    async fn es_transforms(
        &self,
    ) -> Result<std::result::Result<Vec<(J, Option<J>)>, &'static str>> {
        let mut defs = Vec::new();
        let mut from = 0;
        loop {
            let (status, v) = self
                .job_probe("GET", &format!("/_transform?size={PAGE}&from={from}"), None)
                .await?;
            if status >= 400 {
                return Ok(Err(if denied(status, &v) {
                    DENIED_TRANSFORMS
                } else {
                    NO_TRANSFORMS
                }));
            }
            let page = v.get("transforms").and_then(J::as_arr).unwrap_or(&[]);
            defs.extend(page.iter().cloned());
            let count = num(v.get("count")).unwrap_or(0.0) as usize;
            from += PAGE;
            if page.len() < PAGE || from >= count {
                break;
            }
        }
        let mut stats: BTreeMap<String, J> = BTreeMap::new();
        if !defs.is_empty() {
            let mut from = 0;
            loop {
                let (_, v) = self
                    .job_call(
                        "GET",
                        &format!("/_transform/_all/_stats?size={PAGE}&from={from}"),
                        None,
                    )
                    .await?;
                let page = v.get("transforms").and_then(J::as_arr).unwrap_or(&[]);
                for s in page {
                    if let Some(id) = text(s.get("id")) {
                        stats.insert(id, s.clone());
                    }
                }
                from += PAGE;
                if page.len() < PAGE {
                    break;
                }
            }
        }
        Ok(Ok(defs
            .into_iter()
            .map(|d| {
                let s = text(d.get("id")).and_then(|id| stats.remove(&id));
                (d, s)
            })
            .collect()))
    }

    /// Every watch (`Err` with the note when Watcher isn't there or the
    /// user may not see the watches).
    async fn es_watches(&self) -> Result<std::result::Result<Vec<J>, &'static str>> {
        let mut out = Vec::new();
        let mut from = 0;
        loop {
            let body =
                J::parse(&format!(r#"{{"size": {PAGE}, "from": {from}}}"#)).unwrap_or(J::Null);
            let (status, v) = self
                .job_probe("POST", "/_watcher/_query/watches", Some(&body))
                .await?;
            if status >= 400 {
                return Ok(Err(if license_refusal(&v) {
                    NO_WATCHER_LICENSE
                } else if denied(status, &v) {
                    DENIED_WATCHES
                } else {
                    NO_WATCHER
                }));
            }
            let page = v.get("watches").and_then(J::as_arr).unwrap_or(&[]);
            out.extend(page.iter().cloned());
            from += PAGE;
            if page.len() < PAGE {
                break;
            }
        }
        Ok(Ok(out))
    }

    /// Whether the user can read `.watcher-history-*`. Security drops the
    /// indices a user can't read from a wildcard search, which then finds
    /// nothing, so it's asked with `_has_privileges`; a cluster without
    /// security can read it.
    async fn watch_history_readable(&self) -> bool {
        let body =
            J::parse(r#"{"index": [{"names": [".watcher-history-*"], "privileges": ["read"]}]}"#)
                .unwrap_or(J::Null);
        match self
            .job_probe("POST", "/_security/user/_has_privileges", Some(&body))
            .await
        {
            Ok((200, v)) => v.get("has_all_requested").and_then(J::as_bool) != Some(false),
            _ => true,
        }
    }

    async fn es_jobs(&self) -> Result<JobList> {
        let mut jobs = Vec::new();
        let mut notes = Vec::new();
        match self.es_transforms().await? {
            Ok(ts) => jobs.extend(
                ts.iter()
                    .map(|(d, s)| es_transform_job(d, s.as_ref(), self.schedule_now())),
            ),
            Err(note) => notes.push(note.to_string()),
        }
        match self.es_watches().await? {
            Ok(ws) => {
                let history = ws.is_empty() || self.watch_history_readable().await;
                if !history {
                    notes.push(DENIED_WATCH_HISTORY.to_string());
                }
                jobs.extend(ws.iter().map(|w| watch_job(w, history)));
            }
            Err(note) => notes.push(note.to_string()),
        }
        Ok(JobList { jobs, notes })
    }

    /// A transform and its stats. The definition that comes back has to be
    /// the one asked for (a pattern would answer others).
    async fn es_transform(&self, id: &str) -> Result<(J, Option<J>)> {
        let seg = path_segment(id);
        let (_, v) = self
            .job_call("GET", &format!("/_transform/{seg}"), None)
            .await?;
        let def = v
            .get("transforms")
            .and_then(J::as_arr)
            .and_then(|a| a.iter().find(|t| text(t.get("id")).as_deref() == Some(id)))
            .cloned()
            .ok_or_else(|| Error::Query(format!("no existe el transform {id}")))?;
        let (_, s) = self
            .job_call("GET", &format!("/_transform/{seg}/_stats"), None)
            .await?;
        let stats = s
            .get("transforms")
            .and_then(J::as_arr)
            .and_then(|a| a.iter().find(|t| text(t.get("id")).as_deref() == Some(id)))
            .cloned();
        Ok((def, stats))
    }

    async fn es_watch(&self, id: &str) -> Result<J> {
        let (status, v) = self
            .job_probe(
                "GET",
                &format!("/_watcher/watch/{}", path_segment(id)),
                None,
            )
            .await?;
        if status == 404 || v.get("found").and_then(J::as_bool) == Some(false) {
            return Err(Error::Query(format!("no existe el watch {id}")));
        }
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &v.compact())));
        }
        if text(v.get("_id")).as_deref() != Some(id) {
            return Err(Error::Query(format!("no existe el watch {id}")));
        }
        // GET answers `{ _id, found, status, watch }`, like a query hit.
        Ok(v)
    }

    // ---- OpenSearch

    /// Every transform with its `_explain`; `Err` with the note when they
    /// can't be listed.
    async fn os_transforms(
        &self,
    ) -> Result<std::result::Result<Vec<(J, Option<J>)>, &'static str>> {
        let p = self.plugins();
        let mut hits = Vec::new();
        let mut from = 0;
        loop {
            let (status, v) = self
                .job_probe(
                    "GET",
                    &format!("/{p}/_transform?size={PAGE}&from={from}"),
                    None,
                )
                .await?;
            // Open Distro 1.x has transforms but only `POST` on the list
            // path (405).
            if status == 405 {
                return Ok(Err(NO_TRANSFORM_LIST));
            }
            if status >= 400 {
                return Ok(Err(if denied(status, &v) {
                    DENIED_OS_TRANSFORMS
                } else {
                    NO_OS_TRANSFORMS
                }));
            }
            let page = v.get("transforms").and_then(J::as_arr).unwrap_or(&[]);
            hits.extend(page.iter().cloned());
            let total = num(v.get("total_transforms")).unwrap_or(0.0) as usize;
            from += PAGE;
            if page.len() < PAGE || from >= total {
                break;
            }
        }
        let mut explain: BTreeMap<String, J> = BTreeMap::new();
        let ids: Vec<String> = hits.iter().filter_map(|h| text(h.get("_id"))).collect();
        for chunk in ids.chunks(50) {
            let list = chunk
                .iter()
                .map(|i| path_segment(i))
                .collect::<Vec<_>>()
                .join(",");
            if let Ok((_, v)) = self
                .job_call("GET", &format!("/{p}/_transform/{list}/_explain"), None)
                .await
            {
                for (k, e) in v.as_obj().into_iter().flatten() {
                    explain.insert(k.clone(), e.clone());
                }
            }
        }
        Ok(Ok(hits
            .into_iter()
            .map(|h| {
                let e = text(h.get("_id")).and_then(|id| explain.remove(&id));
                (h, e)
            })
            .collect()))
    }

    /// Every monitor as (id, monitor), and how many the search found in
    /// all (more than were read past the search window); `Err` with the
    /// note without the Alerting plugin or when the user may not see them.
    async fn os_monitors(
        &self,
    ) -> Result<std::result::Result<(Vec<(String, J)>, usize, usize), &'static str>> {
        let p = self.plugins();
        let mut out = Vec::new();
        let mut read = 0;
        let mut total;
        let mut from = 0;
        loop {
            let body = J::parse(&format!(
                r#"{{"size": {PAGE}, "from": {from}, "track_total_hits": true, "query": {{"match_all": {{}}}}}}"#
            ))
            .unwrap_or(J::Null);
            let (status, v) = self
                .job_probe(
                    "POST",
                    &format!("/{p}/_alerting/monitors/_search"),
                    Some(&body),
                )
                .await?;
            if status >= 400 {
                // No monitor was ever made: the config index isn't there yet.
                let t = v.text();
                if t.contains("index_not_found")
                    || (t.contains("alerting-config") && t.contains("not found"))
                {
                    return Ok(Ok((Vec::new(), 0, 0)));
                }
                return Ok(Err(if denied(status, &v) {
                    DENIED_MONITORS
                } else {
                    NO_ALERTING
                }));
            }
            total = num(v.at(&["hits", "total", "value"]))
                .or_else(|| num(v.at(&["hits", "total"])))
                .unwrap_or(0.0) as usize;
            let page = v.at(&["hits", "hits"]).and_then(J::as_arr).unwrap_or(&[]);
            read += page.len();
            out.extend(page.iter().filter_map(|h| {
                let src = h.get("_source")?;
                // Workflows share the index.
                let m = src.get("monitor").unwrap_or(src);
                (text(m.get("type")).as_deref() != Some("workflow"))
                    .then(|| Some((text(h.get("_id"))?, m.clone())))
                    .flatten()
            }));
            from += PAGE;
            if page.len() < PAGE || from >= MAX_SEARCH_WINDOW {
                break;
            }
        }
        Ok(Ok((out, read, total)))
    }

    async fn os_jobs(&self) -> Result<JobList> {
        let mut jobs = Vec::new();
        let mut notes = Vec::new();
        match self.os_transforms().await? {
            Ok(ts) => jobs.extend(ts.iter().map(|(h, e)| os_transform_job(h, e.as_ref()))),
            Err(note) => notes.push(note.to_string()),
        }
        match self.os_monitors().await? {
            Ok((ms, read, total)) => {
                if total > read {
                    notes.push(format!(
                        "la lista de monitores está incompleta: se muestran los primeros {read} de {total}"
                    ));
                }
                jobs.extend(ms.iter().map(|(id, m)| os_monitor_job(id, m)));
            }
            Err(note) => notes.push(note.to_string()),
        }
        notes.push(OS_SCOPE.to_string());
        Ok(JobList { jobs, notes })
    }

    async fn os_transform(&self, id: &str) -> Result<(J, Option<J>)> {
        let p = self.plugins();
        let seg = path_segment(id);
        let (status, hit) = self
            .job_probe("GET", &format!("/{p}/_transform/{seg}"), None)
            .await?;
        if status == 404 {
            return Err(Error::Query(format!("no existe el transform {id}")));
        }
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &hit.compact())));
        }
        if text(hit.get("_id")).as_deref() != Some(id) {
            return Err(Error::Query(format!("no existe el transform {id}")));
        }
        let explain = self
            .job_call("GET", &format!("/{p}/_transform/{seg}/_explain"), None)
            .await
            .ok()
            .and_then(|(_, v)| v.get(id).cloned());
        Ok((hit, explain))
    }

    /// The monitor, its `_seq_no` and `_primary_term`.
    async fn os_monitor(&self, id: &str) -> Result<(J, String, String)> {
        let p = self.plugins();
        let (status, v) = self
            .job_probe(
                "GET",
                &format!("/{p}/_alerting/monitors/{}", path_segment(id)),
                None,
            )
            .await?;
        if status == 404 {
            return Err(Error::Query(format!("no existe el monitor {id}")));
        }
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &v.compact())));
        }
        let m = v
            .get("monitor")
            .filter(|_| text(v.get("_id")).as_deref() == Some(id))
            .cloned()
            .ok_or_else(|| Error::Query(format!("no existe el monitor {id}")))?;
        Ok((
            m,
            text(v.get("_seq_no")).unwrap_or_default(),
            text(v.get("_primary_term")).unwrap_or_default(),
        ))
    }

    // ---- the calls

    fn parse(&self, id: &str) -> Result<(Kind, String)> {
        let (kind, rest) = parse_id(id).ok_or_else(|| bad_id(id))?;
        let fits = match kind {
            Kind::Transform => self.opensearch || es_transform_id(rest),
            Kind::Watch => !self.opensearch,
            Kind::Monitor => self.opensearch,
        };
        if !fits {
            return Err(bad_id(id));
        }
        Ok((kind, rest.to_string()))
    }

    pub(crate) async fn job_info(&self, id: &str) -> Result<JobDetail> {
        let (kind, id) = self.parse(id)?;
        Ok(match (kind, self.opensearch) {
            (Kind::Transform, false) => {
                let (d, s) = self.es_transform(&id).await?;
                es_transform_detail(&d, s.as_ref(), self.schedule_now())
            }
            (Kind::Transform, true) => {
                let (h, e) = self.os_transform(&id).await?;
                os_transform_detail(&h, e.as_ref())
            }
            (Kind::Watch, _) => {
                let w = self.es_watch(&id).await?;
                watch_detail(&w, self.watch_history_readable().await)
            }
            (Kind::Monitor, _) => os_monitor_detail(&id, &self.os_monitor(&id).await?.0),
        })
    }

    pub(crate) async fn job_history(&self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let (kind, id) = self.parse(id)?;
        if kind != Kind::Watch {
            return Err(Error::Unsupported(
                "el clúster no guarda el historial de ejecuciones de este job".into(),
            ));
        }
        self.es_watch(&id).await?;
        if !self.watch_history_readable().await {
            return Err(Error::Unsupported(DENIED_WATCH_HISTORY.into()));
        }
        let limit = limit.clamp(1, MAX_JOB_RUNS);
        let body = J::Obj(vec![
            ("size".into(), J::Num(limit.into())),
            (
                "sort".into(),
                J::parse(r#"[{"trigger_event.triggered_time": {"order": "desc"}}]"#)
                    .unwrap_or(J::Null),
            ),
            (
                "query".into(),
                J::Obj(vec![(
                    "term".into(),
                    J::Obj(vec![("watch_id".into(), J::Str(id.clone()))]),
                )]),
            ),
        ]);
        let (_, v) = self
            .job_call(
                "POST",
                "/.watcher-history-*/_search?ignore_unavailable=true&allow_no_indices=true",
                Some(&body),
            )
            .await?;
        Ok(v.at(&["hits", "hits"])
            .and_then(J::as_arr)
            .into_iter()
            .flatten()
            .filter_map(|h| Some(watch_run(text(h.get("_id")), h.get("_source")?)))
            .collect())
    }

    pub(crate) async fn job_start(&self, id: &str) -> Result<()> {
        let (kind, id) = self.parse(id)?;
        self.check_writable("ejecutar jobs del servidor")?;
        let seg = path_segment(&id);
        match (kind, self.opensearch) {
            (Kind::Transform, false) => {
                let (d, s) = self.es_transform(&id).await?;
                if !es_transform_job(&d, s.as_ref(), self.schedule_now()).can_run {
                    return Err(Error::Query(format!(
                        "el transform {id} se ejecuta al iniciarlo: «Ejecutar ahora» es para los continuos ya iniciados (Elasticsearch 8.7 o posterior)"
                    )));
                }
                self.job_call("POST", &format!("/_transform/{seg}/_schedule_now"), None)
                    .await?;
                Ok(())
            }
            (Kind::Watch, _) => {
                self.es_watch(&id).await?;
                let body = J::parse(r#"{"record_execution": true}"#).unwrap_or(J::Null);
                self.execute_detached(format!("/_watcher/watch/{seg}/_execute"), body)
                    .await
            }
            (Kind::Monitor, _) => {
                self.os_monitor(&id).await?;
                let p = self.plugins();
                self.execute_detached(
                    format!("/{p}/_alerting/monitors/{seg}/_execute"),
                    J::Obj(Vec::new()),
                )
                .await
            }
            (Kind::Transform, true) => Err(Error::Unsupported(
                "OpenSearch ejecuta los transforms según su intervalo: no hay forma de dispararlos a mano".into(),
            )),
        }
    }

    pub(crate) async fn job_enable(&self, id: &str, enabled: bool) -> Result<()> {
        let (kind, id) = self.parse(id)?;
        self.check_writable("habilitar ni deshabilitar jobs del servidor")?;
        let seg = path_segment(&id);
        let p = self.plugins();
        let verb = if enabled { "_start" } else { "_stop" };
        match (kind, self.opensearch) {
            (Kind::Transform, false) => {
                let (_, stats) = self.es_transform(&id).await?;
                // A failed transform stops only when forced.
                let failed = stats
                    .as_ref()
                    .and_then(|s| text(s.get("state")))
                    .is_some_and(|s| s == "failed");
                let force = if !enabled && failed {
                    "?force=true"
                } else {
                    ""
                };
                self.job_call("POST", &format!("/_transform/{seg}/{verb}{force}"), None)
                    .await?;
            }
            (Kind::Transform, true) => {
                self.os_transform(&id).await?;
                self.job_call("POST", &format!("/{p}/_transform/{seg}/{verb}"), None)
                    .await?;
            }
            (Kind::Watch, _) => {
                self.es_watch(&id).await?;
                let verb = if enabled { "_activate" } else { "_deactivate" };
                self.job_call("PUT", &format!("/_watcher/watch/{seg}/{verb}"), None)
                    .await?;
            }
            (Kind::Monitor, _) => {
                let (m, seq, term) = self.os_monitor(&id).await?;
                if let Some(owner) = os_monitor_owner(&m) {
                    return Err(Error::Query(format!(
                        "el monitor {id} lo administra {owner}: se habilita o deshabilita desde ese plugin"
                    )));
                }
                let seq: u64 = seq.parse().unwrap_or(0);
                let term: u64 = term.parse().unwrap_or(0);
                self.job_call(
                    "PUT",
                    &format!(
                        "/{p}/_alerting/monitors/{seg}?if_seq_no={seq}&if_primary_term={term}"
                    ),
                    Some(&os_monitor_enabled(&m, enabled)),
                )
                .await?;
            }
        }
        Ok(())
    }

    pub(crate) async fn job_create_script(&self, id: &str) -> Result<String> {
        let (kind, id) = self.parse(id)?;
        Ok(match (kind, self.opensearch) {
            (Kind::Transform, false) => es_transform_script(&self.es_transform(&id).await?.0),
            (Kind::Transform, true) => {
                os_transform_script(self.plugins(), &self.os_transform(&id).await?.0)
            }
            (Kind::Watch, _) => watch_script(&self.es_watch(&id).await?),
            (Kind::Monitor, _) => os_monitor_script(self.plugins(), &self.os_monitor(&id).await?.0),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn j(s: &str) -> J {
        J::parse(s).unwrap()
    }

    #[test]
    fn ids_are_checked() {
        assert_eq!(parse_id("transform:tf_1"), Some((Kind::Transform, "tf_1")));
        assert_eq!(parse_id("watch:a b/c"), Some((Kind::Watch, "a b/c")));
        assert_eq!(
            parse_id("monitor:TzLnz6EB"),
            Some((Kind::Monitor, "TzLnz6EB"))
        );
        for bad in [
            "",
            "tf_1",
            "job:x",
            "watch:",
            "watch: x",
            "watch:x\n",
            "transform:x\u{0}",
            // Several objects, or a path the URL would rewrite.
            "transform:_all",
            "transform:*",
            "transform:tf*",
            "transform:a,b",
            "transform:.",
            "watch:..",
            "monitor:_all",
            "monitor:a,b",
        ] {
            assert_eq!(parse_id(bad), None, "{bad:?}");
        }
        // Elasticsearch's own rule for transform ids.
        for ok in ["tf", "tf_1", "a-b", "0", "dbine_test_jobs_tf"] {
            assert!(es_transform_id(ok), "{ok}");
        }
        for bad in [
            "_all",
            "Tf",
            "tf_",
            "-tf",
            "a b",
            "a/b",
            "a.b",
            &"a".repeat(65),
        ] {
            assert!(!es_transform_id(bad), "{bad}");
        }
        // Whatever the id holds stays one path segment.
        assert_eq!(
            path_segment("../_cluster/settings?x"),
            "..%2F_cluster%2Fsettings%3Fx"
        );
    }

    #[test]
    fn refusals_are_told_apart() {
        let lic = j(
            r#"{"error":{"type":"security_exception","reason":"current license is non-compliant for [watcher]"},"status":403}"#,
        );
        assert!(license_refusal(&lic) && denied(403, &lic));
        let priv_ = j(
            r#"{"error":{"type":"security_exception","reason":"action [cluster:monitor/transform/get] is unauthorized for user [u]"},"status":403}"#,
        );
        assert!(!license_refusal(&priv_) && denied(403, &priv_));
        let missing = j(
            r#"{"error":{"type":"illegal_argument_exception","reason":"no handler found"},"status":400}"#,
        );
        assert!(!denied(400, &missing));
    }

    #[test]
    fn times() {
        assert_eq!(rfc3339_ms(1_794_489_393_026), "2026-11-12T13:16:33Z");
        assert_eq!(
            iso_ms("2026-11-12T13:16:49.736Z")
                .map(rfc3339_ms)
                .as_deref(),
            Some("2026-11-12T13:16:49Z")
        );
        assert_eq!(
            iso_ms("2026-11-12T10:16:49.73574632-03:00")
                .map(rfc3339_ms)
                .as_deref(),
            Some("2026-11-12T13:16:49Z")
        );
        assert_eq!(iso_ms("ayer"), None);
        assert!(version_at_least("Elasticsearch 8.15.3", 8, 7));
        assert!(!version_at_least("Elasticsearch 8.6.2", 8, 7));
        assert!(!version_at_least("Elasticsearch 7.17.0", 8, 7));
    }

    #[test]
    fn transforms_become_jobs() {
        let t = j(
            r#"{"id":"tf","version":"10.0.0","create_time":1794489386003,"source":{"index":["src"],"query":{"match_all":{}}},"dest":{"index":"dst"},"frequency":"5m","sync":{"time":{"field":"t","delay":"60s"}},"pivot":{"group_by":{"host":{"terms":{"field":"host"}}}},"description":"d","settings":{}}"#,
        );
        let s = j(
            r#"{"id":"tf","state":"started","stats":{"trigger_count":2,"documents_indexed":2},"checkpointing":{"last":{"checkpoint":1,"timestamp_millis":1794489393026}},"health":{"status":"green"}}"#,
        );
        let job = es_transform_job(&t, Some(&s), true);
        assert_eq!(job.id, "transform:tf");
        assert_eq!(job.kind, "Transform");
        assert_eq!(job.schedule.as_deref(), Some("frequency 5m"));
        assert!(job.enabled && !job.running && job.can_run && job.can_enable && job.has_script);
        assert!(!job.can_stop && !job.has_history);
        assert_eq!(job.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(job.last_started.as_deref(), Some("2026-11-12T13:16:33Z"));
        assert!(!es_transform_job(&t, Some(&s), false).can_run);
        let stopped =
            j(r#"{"id":"tf","state":"stopped","checkpointing":{"last":{"checkpoint":0}}}"#);
        let off = es_transform_job(&t, Some(&stopped), true);
        assert!(!off.enabled && !off.can_run && off.last_outcome.is_none());
        let failed = j(r#"{"id":"tf","state":"failed","reason":"boom"}"#);
        assert_eq!(
            es_transform_job(&t, Some(&failed), true).last_outcome,
            Some(JobOutcome::Failed)
        );
        let script = es_transform_script(&t);
        assert!(script.starts_with("PUT /_transform/tf\n{"));
        assert!(!script.contains("create_time") && !script.contains("\"version\""));
        assert!(script.contains("\"frequency\": \"5m\""));
        let d = es_transform_detail(&t, Some(&s), true);
        assert_eq!(d.steps[0].kind.as_deref(), Some("pivot"));
        assert_eq!(d.steps[0].target.as_deref(), Some("dst"));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Origen" && p.value == "src"));
        assert!(!job.system, "a user's transform");
        // One a Fleet integration installed: Elastic manages it.
        let managed = j(
            r#"{"id":"logs-endpoint.metadata_current-default-8.15.0","dest":{"index":"x"},"pivot":{},"_meta":{"managed":true,"managed_by":"fleet","package":{"name":"endpoint"}}}"#,
        );
        assert!(es_transform_job(&managed, None, true).system);
        let unmanaged = j(r#"{"id":"tf","pivot":{},"_meta":{"managed":false}}"#);
        assert!(!es_transform_job(&unmanaged, None, true).system);
    }

    #[test]
    fn watches_become_jobs() {
        let hit = j(
            r#"{"_id":"w1","watch":{"trigger":{"schedule":{"interval":"1h"}},"input":{"simple":{"x":1}},"condition":{"always":{}},"actions":{"log1":{"logging":{"level":"info","text":"t"}}},"metadata":{"name":"Mi watch"}},"status":{"state":{"active":false,"timestamp":"2026-11-12T13:16:49.183Z"},"last_checked":"2026-11-12T13:16:49.736Z","actions":{"log1":{"last_execution":{"timestamp":"2026-11-12T13:16:49.736Z","successful":false}}},"execution_state":"executed","version":1}}"#,
        );
        let w = watch_job(&hit, true);
        assert_eq!(w.id, "watch:w1");
        assert_eq!(w.description.as_deref(), Some("Mi watch"));
        assert_eq!(w.schedule.as_deref(), Some("interval 1h"));
        assert!(!w.enabled && w.can_run && w.can_enable && w.has_history && w.has_script);
        assert_eq!(w.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(w.last_started.as_deref(), Some("2026-11-12T13:16:49Z"));
        let script = watch_script(&hit);
        assert!(
            script.starts_with("PUT /_watcher/watch/w1?active=false\n{"),
            "{script}"
        );
        assert!(!watch_job(&hit, false).has_history);
        assert!(!w.system, "a user's watch");
        // Monitoring's cluster alerts are Elasticsearch's own.
        let alert = j(
            r#"{"_id":"u1_elasticsearch_cluster_status","watch":{"trigger":{"schedule":{"interval":"1m"}},"metadata":{"name":"X-Pack Monitoring: Cluster Status","xpack":{"severity":2100,"cluster_uuid":"u1","type":"monitoring"}}},"status":{"state":{"active":true}}}"#,
        );
        assert!(watch_job(&alert, true).system);
        let d = watch_detail(&hit, true);
        assert_eq!(d.steps[0].name, "log1");
        assert_eq!(d.steps[0].kind.as_deref(), Some("logging"));
        assert_eq!(
            watch_schedule(&j(r#"{"trigger":{"schedule":{"daily":{"at":"03:00"}}}}"#)).as_deref(),
            Some(r#"daily {"at":"03:00"}"#)
        );
        assert_eq!(
            watch_schedule(&j(r#"{"trigger":{"schedule":{"cron":"0 0 3 * * ?"}}}"#)).as_deref(),
            Some("cron 0 0 3 * * ?")
        );
    }

    #[test]
    fn history_records_become_runs() {
        let r = j(
            r#"{"watch_id":"w1","state":"executed","trigger_event":{"type":"manual","triggered_time":"2026-11-12T13:16:49.735Z"},"result":{"execution_time":"2026-11-12T13:16:49.736Z","execution_duration":7,"actions":[{"id":"log1","type":"logging","status":"success"},{"id":"hook","type":"webhook","status":"failure","reason":"connection refused"}]},"messages":[]}"#,
        );
        let run = watch_run(Some("rec".into()), &r);
        assert_eq!(run.outcome, JobOutcome::Failed);
        assert_eq!(run.trigger.as_deref(), Some("manual"));
        assert_eq!(run.started.as_deref(), Some("2026-11-12T13:16:49Z"));
        assert_eq!(run.duration_ms, Some(7));
        assert_eq!(run.steps.len(), 2);
        assert_eq!(run.steps[1].outcome, JobOutcome::Failed);
        assert_eq!(run.steps[1].message.as_deref(), Some("connection refused"));
        let skipped = watch_run(
            None,
            &j(
                r#"{"state":"execution_not_needed","trigger_event":{"type":"schedule","triggered_time":"2026-11-12T13:00:00Z"}}"#,
            ),
        );
        assert_eq!(skipped.outcome, JobOutcome::Skipped);
        assert_eq!(skipped.trigger.as_deref(), Some("programada"));
        assert_eq!(skipped.started.as_deref(), Some("2026-11-12T13:00:00Z"));
    }

    #[test]
    fn opensearch_jobs() {
        let hit = j(
            r#"{"_id":"tf","transform":{"transform_id":"tf","schema_version":21,"schedule":{"interval":{"start_time":1602100553,"period":1,"unit":"Minutes"}},"metadata_id":"m","updated_at":1794489422136,"enabled":true,"enabled_at":1794489422135,"description":"d","source_index":"src","target_index":"dst","data_selection_query":{"match_all":{}},"page_size":1,"groups":[{"terms":{"source_field":"host","target_field":"host"}}],"aggregations":{"avg_v":{"avg":{"field":"v"}}},"continuous":true}}"#,
        );
        let explain = j(
            r#"{"metadata_id":"m","transform_metadata":{"status":"started","last_updated_at":1794489482136,"stats":{"pages_processed":2}}}"#,
        );
        let t = os_transform_job(&hit, Some(&explain));
        assert_eq!(t.id, "transform:tf");
        assert_eq!(t.schedule.as_deref(), Some("every 1 Minutes"));
        assert!(t.enabled && t.can_enable && !t.can_run && !t.has_history);
        assert_eq!(t.last_outcome, Some(JobOutcome::Succeeded));
        let script = os_transform_script("_plugins", &hit);
        assert!(script.starts_with("PUT /_plugins/_transform/tf\n{\n  \"transform\": {"));
        assert!(!script.contains("schema_version") && !script.contains("metadata_id"));

        let m = j(
            r#"{"type":"monitor","schema_version":0,"name":"mon","monitor_type":"query_level_monitor","enabled":true,"enabled_time":1794489425309,"schedule":{"period":{"interval":1,"unit":"HOURS"}},"inputs":[{"search":{"indices":["src"],"query":{"size":0}}}],"triggers":[{"query_level_trigger":{"id":"x","name":"t1","severity":"1","condition":{"script":{"source":"true"}},"actions":[]}}],"last_update_time":1794489425311,"owner":"alerting"}"#,
        );
        let job = os_monitor_job("TzL", &m);
        assert_eq!(job.id, "monitor:TzL");
        assert_eq!(job.name, "mon");
        assert_eq!(job.schedule.as_deref(), Some("every 1 HOURS"));
        assert!(job.enabled && job.can_run && job.can_enable && job.has_script);
        let d = os_monitor_detail("TzL", &m);
        assert_eq!(d.steps[1].kind.as_deref(), Some("query_level_trigger"));
        assert_eq!(d.steps[1].name, "t1");
        assert_eq!(d.steps[0].name, "Entradas");
        let off = os_monitor_enabled(&m, false);
        assert_eq!(off.get("enabled").and_then(J::as_bool), Some(false));
        assert!(off.get("enabled_time").is_none() && off.get("schema_version").is_none());
        // The update keeps what the monitor was made with.
        let custom = j(
            r#"{"type":"monitor","name":"doc","monitor_type":"doc_level_monitor","enabled":true,"schedule":{"period":{"interval":1,"unit":"MINUTES"}},"inputs":[],"triggers":[],"owner":"security_analytics","data_sources":{"findings_index":"my-findings","alerts_index":"my-alerts"},"user":{"name":"admin"},"last_update_time":1}"#,
        );
        let off = os_monitor_enabled(&custom, false);
        assert_eq!(off.get("owner"), custom.get("owner"));
        assert_eq!(off.get("data_sources"), custom.get("data_sources"));
        assert!(off.get("user").is_none() && off.get("last_update_time").is_none());
        // A monitor another plugin manages isn't enabled from here.
        assert_eq!(
            os_monitor_owner(&custom).as_deref(),
            Some("security_analytics")
        );
        assert!(!os_monitor_job("x", &custom).can_enable);
        // Who manages it is a note, not a property.
        let d = os_monitor_detail("x", &custom);
        assert_eq!(
            d.notes,
            ["este monitor lo administra security_analytics: se habilita o deshabilita desde ese plugin"]
        );
        assert!(!d.properties.iter().any(|p| p.label == "Administrado por"));
        assert!(os_monitor_detail("TzL", &m).notes.is_empty());
        assert_eq!(os_monitor_owner(&m), None);
        let defaults = j(
            r#"{"owner":"alerting","data_sources":{"query_index":".opensearch-alerting-queries","findings_index":".opensearch-alerting-finding-history-write","alerts_index":".opendistro-alerting-alerts","alerts_history_index":".opendistro-alerting-alert-history-write","comments_index":""}}"#,
        );
        assert_eq!(os_monitor_owner(&defaults), None);
        let moved = j(r#"{"owner":"alerting","data_sources":{"alerts_index":"my-alerts"}}"#);
        assert_eq!(os_monitor_owner(&moved).as_deref(), Some("otro plugin"));
        assert!(os_monitor_script("_opendistro", &m)
            .starts_with("POST /_opendistro/_alerting/monitors\n"));
    }
}
