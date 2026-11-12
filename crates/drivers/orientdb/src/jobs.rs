//! Server jobs ([`dbine_driver::Session::jobs`]): OrientDB's scheduler,
//! the `OSchedule` records of the session's database. Each one runs a
//! function (`OFunction`) on a cron rule with seconds (`0 0/5 * * * ?`).
//!
//! What OrientDB keeps of an event: its name, rule, function, arguments,
//! `status` (`RUNNING` while the function runs, `STOPPED` otherwise),
//! `starttime` (when it was set to start, overwritten with the start of
//! each run) and `nextExecId` (how many times it ran). There's no way to
//! pause an event (writing `status` doesn't stop it: only deleting the
//! record does), no "run now" (calling the function isn't a run of the
//! event: the scheduler doesn't record it) and no history of runs. So the
//! list, the detail and the script are all there is, and the tab says so.
//!
//! Ids are the event names (the scheduler keeps its events by name). No
//! statement takes one: an id that comes back only picks one of the listed
//! events, exactly.

use crate::ddl::{function_statement, string};
use crate::{as_text, OrientSession};
use dbine_driver::{Error, JobDetail, JobList, JobStep, PropertyInfo, Result, ServerJob};
use serde_json::Value;

const KIND: &str = "Scheduled event";
/// Jobs listed at most.
const MAX_JOBS: i64 = 5000;

const LIST: &str = "SELECT @rid AS rid, name, rule, status, starttime.asLong() AS start_ms, nextExecId AS runs, arguments, function.name AS fn, function.language AS lang, function.code AS code, function.parameters AS params, function.idempotent AS idempotent FROM OSchedule ORDER BY name";

const EVENT_NOTE: &str = "OrientDB no permite pausar un evento programado, ejecutarlo a pedido ni detener una ejecución, y no guarda un historial: para frenarlo hay que borrar su registro de OSchedule y volver a crearlo con su script.";
const DENIED: &str = "Este usuario no puede leer los eventos programados (la clase OSchedule).";

/// One `OSchedule` record.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Event {
    pub rid: Option<String>,
    pub name: String,
    pub rule: Option<String>,
    pub status: Option<String>,
    pub start_ms: Option<i64>,
    pub runs: Option<i64>,
    pub arguments: Option<Value>,
    pub function: Option<String>,
    pub language: Option<String>,
    pub code: Option<String>,
    pub params: Option<Value>,
    pub idempotent: Option<Value>,
}

fn get<'a>(r: &'a [(String, Value)], k: &str) -> Option<&'a Value> {
    r.iter()
        .find(|(n, _)| n == k)
        .map(|(_, v)| v)
        .filter(|v| !v.is_null())
}

fn text(r: &[(String, Value)], k: &str) -> Option<String> {
    get(r, k)
        .map(as_text)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn int(r: &[(String, Value)], k: &str) -> Option<i64> {
    match get(r, k)? {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

pub(crate) fn events(records: &[Vec<(String, Value)>]) -> Vec<Event> {
    records
        .iter()
        .filter_map(|r| {
            let name = text(r, "name")?;
            if name.chars().any(char::is_control) {
                return None;
            }
            Some(Event {
                rid: text(r, "rid"),
                name,
                rule: text(r, "rule"),
                status: text(r, "status"),
                start_ms: int(r, "start_ms"),
                runs: int(r, "runs"),
                arguments: get(r, "arguments").cloned(),
                function: text(r, "fn"),
                language: text(r, "lang"),
                code: get(r, "code").map(as_text),
                params: get(r, "params").cloned(),
                idempotent: get(r, "idempotent").cloned(),
            })
        })
        .collect()
}

/// Milliseconds since 1970 as RFC 3339 in UTC.
pub(crate) fn utc_ms(ms: i64) -> String {
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

pub(crate) fn job_of(e: &Event, database: &str) -> ServerJob {
    let ran = e.runs.unwrap_or(0) > 0;
    let running = e.status.as_deref() == Some("RUNNING");
    ServerJob {
        id: e.name.clone(),
        name: e.name.clone(),
        kind: KIND.into(),
        group: Some(database.to_string()).filter(|d| !d.is_empty()),
        description: e.function.clone(),
        owner: None,
        // Nothing turns an event off but deleting it.
        enabled: true,
        schedule: e.rule.clone(),
        running,
        last_outcome: running.then_some(dbine_driver::JobOutcome::Running),
        // `starttime` is the last run's start once it ran.
        last_started: e.start_ms.filter(|_| ran).map(utc_ms),
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: false,
        has_history: false,
        has_script: e.function.is_some() && e.rule.is_some(),
        system: false,
    }
}

fn prop(label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: String::new(),
        label: label.into(),
        value: value.into(),
    }
}

pub(crate) fn detail_of(e: &Event, database: &str) -> JobDetail {
    let job = job_of(e, database);
    let mut properties = Vec::new();
    if let Some(f) = &e.function {
        properties.push(prop("Función", f.clone()));
    }
    if let Some(a) = e.arguments.as_ref().filter(|a| !a.is_null()) {
        properties.push(prop("Argumentos", a.to_string()));
    }
    if let Some(s) = &e.status {
        properties.push(prop("Estado", s.clone()));
    }
    properties.push(prop("Ejecuciones", e.runs.unwrap_or(0).to_string()));
    if let Some(ms) = e.start_ms.filter(|_| e.runs.unwrap_or(0) == 0) {
        properties.push(prop("Empieza", utc_ms(ms)));
    }
    if let Some(r) = &e.rid {
        properties.push(prop("RID", r.clone()));
    }
    JobDetail {
        steps: vec![JobStep {
            name: e.function.clone().unwrap_or_else(|| "Función".into()),
            kind: e.language.clone(),
            target: None,
            command: e.code.clone(),
        }],
        schedules: Vec::new(),
        properties,
        notes: vec![EVENT_NOTE.into()],
        job,
    }
}

/// The function's `CREATE FUNCTION`, for the script's comment.
fn function_ddl(e: &Event) -> Option<String> {
    let mut r = vec![
        ("name".to_string(), Value::String(e.function.clone()?)),
        (
            "code".to_string(),
            Value::String(e.code.clone().unwrap_or_default()),
        ),
    ];
    if let Some(p) = e.params.clone().filter(Value::is_array) {
        r.push(("parameters".into(), p));
    }
    if let Some(i) = e.idempotent.clone() {
        r.push(("idempotent".into(), i));
    }
    if let Some(l) = &e.language {
        r.push(("language".into(), Value::String(l.clone())));
    }
    Some(function_statement(&r))
}

/// The `INSERT INTO OSchedule` that recreates the event, starting now,
/// after its function's `CREATE FUNCTION` as a comment (the function is
/// its own object and is already in the database).
pub(crate) fn script_of(e: &Event) -> Result<String> {
    let (Some(f), Some(rule)) = (&e.function, &e.rule) else {
        return Err(Error::Query(format!(
            "al evento «{}» le falta la función o la regla: no se puede generar su script",
            e.name
        )));
    };
    let mut out = String::new();
    if let Some(ddl) = function_ddl(e).filter(|d| !d.contains("*/")) {
        out.push_str(&format!(
            "/* La función que ejecuta ya está en la base:\n{ddl}\n*/\n"
        ));
    }
    out.push_str(&format!(
        "INSERT INTO OSchedule SET name = {}, rule = {}, function = (SELECT FROM OFunction WHERE name = {})",
        string(&e.name),
        string(rule),
        string(f)
    ));
    if let Some(a) = e
        .arguments
        .as_ref()
        .filter(|a| a.as_object().is_some_and(|m| !m.is_empty()))
    {
        out.push_str(&format!(", arguments = {a}"));
    }
    out.push_str(", starttime = sysdate();\n");
    Ok(out)
}

fn denied(e: &Error) -> bool {
    let m = e.to_string().to_ascii_lowercase();
    m.contains("permission") || m.contains("not allowed")
}

fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe el evento programado «{}» en esta base",
        id.chars().take(200).collect::<String>()
    ))
}

impl OrientSession {
    /// The events, one past [`MAX_JOBS`] when there are more.
    async fn events(&self) -> Result<Vec<Event>> {
        Ok(events(&self.command(LIST, MAX_JOBS + 1).await?.records))
    }

    async fn event(&self, id: &str) -> Result<Event> {
        self.events()
            .await?
            .into_iter()
            .find(|e| e.name == id)
            .ok_or_else(|| not_found(id))
    }

    pub(crate) async fn server_jobs(&mut self) -> Result<JobList> {
        if self.db.is_empty() {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec!["Los eventos programados son de cada base: elegí una base de datos para verlos.".into()],
            });
        }
        match self.events().await {
            Ok(all) => {
                let mut notes = vec![format!(
                    "Se ven los eventos programados de la base «{}»; cada base tiene los suyos.",
                    self.db
                )];
                if all.len() as i64 > MAX_JOBS {
                    notes.push(format!(
                        "Se muestran los primeros {MAX_JOBS} eventos programados."
                    ));
                }
                Ok(JobList {
                    jobs: all
                        .iter()
                        .take(MAX_JOBS as usize)
                        .map(|e| job_of(e, &self.db))
                        .collect(),
                    notes,
                })
            }
            Err(e) if denied(&e) => Ok(JobList {
                jobs: Vec::new(),
                notes: vec![DENIED.into()],
            }),
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn server_job_detail(&mut self, id: &str) -> Result<JobDetail> {
        Ok(detail_of(&self.event(id).await?, &self.db))
    }

    pub(crate) async fn server_job_script(&mut self, id: &str) -> Result<String> {
        script_of(&self.event(id).await?)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn rec(v: Value) -> Vec<(String, Value)> {
        v.as_object()
            .unwrap()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    fn sample() -> Vec<Vec<(String, Value)>> {
        vec![
            rec(
                json!({"rid": "#9:0", "name": "nightly", "rule": "0 0 3 * * ?", "status": "STOPPED",
                "start_ms": 1794499904004_i64, "runs": 11, "arguments": {"days": 7},
                "fn": "purge", "lang": "SQL", "code": "DELETE FROM Log WHERE at < sysdate() - :days",
                "params": ["days"], "idempotent": false}),
            ),
            rec(
                json!({"rid": "#9:1", "name": "it's \"quoted\"", "rule": "0/5 * * * * ?", "status": "RUNNING",
                "start_ms": 1794499904004_i64, "runs": 0, "fn": "f2", "lang": "javascript", "code": "return 1;"}),
            ),
            rec(json!({"rid": "#9:2", "name": "bad\nname", "rule": "x"})),
            rec(json!({"rid": "#9:3", "rule": "x"})),
        ]
    }

    #[test]
    fn records_become_jobs() {
        let ev = events(&sample());
        assert_eq!(ev.len(), 2);
        let jobs: Vec<ServerJob> = ev.iter().map(|e| job_of(e, "db1")).collect();
        let (a, b) = (&jobs[0], &jobs[1]);
        assert_eq!(a.id, "nightly");
        assert_eq!(a.kind, "Scheduled event");
        assert_eq!(a.group.as_deref(), Some("db1"));
        assert_eq!(a.schedule.as_deref(), Some("0 0 3 * * ?"));
        assert_eq!(a.last_started.as_deref(), Some("2026-11-12T16:11:44Z"));
        assert!(a.enabled && !a.running && a.has_script);
        assert!(!a.can_run && !a.can_stop && !a.can_enable && !a.has_history && !a.system);
        // Never ran: the start time isn't a last run.
        assert!(b.running && b.last_started.is_none());
        assert_eq!(b.last_outcome, Some(dbine_driver::JobOutcome::Running));
        let d = detail_of(&ev[1], "db1");
        assert!(d.properties.iter().any(|p| p.label == "Empieza"));
        assert_eq!(d.steps[0].kind.as_deref(), Some("javascript"));
        assert!(d.notes[0].contains("OSchedule"));
    }

    #[test]
    fn the_script_recreates_the_event_with_its_names_quoted() {
        let ev = events(&sample());
        let s = script_of(&ev[0]).unwrap();
        assert!(s.starts_with("/* La función que ejecuta ya está en la base:\nCREATE FUNCTION"));
        assert!(s.contains("PARAMETERS [days]"));
        assert!(s.ends_with(
            "INSERT INTO OSchedule SET name = 'nightly', rule = '0 0 3 * * ?', function = (SELECT FROM OFunction WHERE name = 'purge'), arguments = {\"days\":7}, starttime = sysdate();\n"
        ));
        let s = script_of(&ev[1]).unwrap();
        assert!(s.contains("name = 'it\\'s \"quoted\"'"), "{s}");
        // A function whose code closes the comment isn't put in one.
        let mut e = ev[1].clone();
        e.code = Some("/* x */ return 1;".into());
        assert!(script_of(&e).unwrap().starts_with("INSERT INTO OSchedule"));
        e.function = None;
        assert!(script_of(&e).is_err());
    }
}
