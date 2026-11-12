//! Server jobs ([`dbine_driver::Session::jobs`]) per flavor.
//!
//! - Neo4j has no scheduler of its own. What runs on its own there are
//!   APOC's background jobs (`apoc.periodic.repeat`, `submit`,
//!   `countdown`): `apoc.periodic.list()` gives their name, rate, and
//!   whether they're done or cancelled. A one-off job not done yet is the
//!   running one, and `apoc.periodic.cancel(name)` stops it. A repeating
//!   job isn't offered a stop: APOC can't stop one of its runs, only cancel
//!   the job for good, and the contract's stop is for the current run (the
//!   detail shows the statement that cancels it). They live in the
//!   server's memory: a restart loses them, and APOC keeps neither their
//!   statement nor their runs, so there's no history, script, "run now" or
//!   pause. Without APOC the list is empty with a note.
//! - Memgraph: no scheduled jobs (its triggers fire on changes and its
//!   `periodic` procedures run in the caller's query).
//! - Neptune: no scheduled jobs in the database.
//!
//! The id is `apoc:<name>`; the name goes into Cypher as a string literal
//! ([`cypher::string`]).

use crate::{as_text, cypher, Flavor, GraphSession};
use dbine_driver::{Error, JobDetail, JobList, JobRun, PropertyInfo, Result, ServerJob};
use serde_json::{Map, Value};

const KIND: &str = "apoc.periodic";
const LIST: &str = "CALL apoc.periodic.list() YIELD name, delay, rate, done, cancelled RETURN name, delay, rate, done, cancelled ORDER BY name";
pub(crate) const IN_MEMORY: &str = "los jobs de APOC (apoc.periodic) viven en la memoria del servidor: se pierden al reiniciarlo, y APOC no guarda la consulta que ejecutan ni su historial";
pub(crate) const NO_APOC: &str =
    "Neo4j no programa jobs por su cuenta y este servidor no tiene APOC (apoc.periodic)";
const NO_STOP_REPEAT: &str = "APOC no detiene una ejecución de un job que se repite: solo lo cancela para siempre (apoc.periodic.cancel)";
const STOP_CANCELS: &str =
    "al detenerlo, APOC lo cancela y lo quita de la lista: no se puede reanudar";
const MEMGRAPH: &str = "Memgraph no tiene jobs programados: sus triggers se disparan con los cambios y sus procedimientos periodic corren dentro de la consulta que los llama";
const NEPTUNE: &str = "Neptune no tiene jobs programados en la base de datos";

/// An `apoc:<name>` id's name.
pub(crate) fn parse_id(id: &str) -> Option<&str> {
    let name = id.strip_prefix("apoc:")?;
    (!name.is_empty() && name.len() <= 512 && !name.chars().any(char::is_control)).then_some(name)
}

fn bad_id(id: &str) -> Error {
    Error::Query(format!(
        "«{}» no es un id de job de APOC",
        id.chars().take(200).collect::<String>()
    ))
}

pub(crate) fn flag(r: &Map<String, Value>, k: &str) -> bool {
    r.get(k).and_then(Value::as_bool).unwrap_or(false)
}

pub(crate) fn int(r: &Map<String, Value>, k: &str) -> i64 {
    r.get(k).and_then(Value::as_i64).unwrap_or(0)
}

/// A row of `apoc.periodic.list()` as a job.
pub(crate) fn job(r: &Map<String, Value>) -> Option<ServerJob> {
    let name = r.get("name").map(as_text).filter(|n| !n.is_empty())?;
    let done = flag(r, "done");
    let cancelled = flag(r, "cancelled");
    let rate = int(r, "rate");
    let finished = done || cancelled;
    Some(ServerJob {
        id: format!("apoc:{name}"),
        name,
        kind: KIND.into(),
        group: None,
        description: Some(
            if rate > 0 {
                "Se repite (apoc.periodic.repeat)"
            } else {
                "Una vez (apoc.periodic.submit o countdown)"
            }
            .into(),
        ),
        owner: None,
        enabled: !finished,
        schedule: (rate > 0).then(|| format!("rate {rate}s")),
        // A one-off job not done yet is running (or about to).
        running: rate == 0 && !finished,
        last_outcome: cancelled.then_some(dbine_driver::JobOutcome::Canceled),
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        // Only that one: a repeating job would be cancelled for good.
        can_stop: rate == 0 && !finished,
        can_enable: false,
        has_history: false,
        has_script: false,
        system: false,
    })
}

fn detail(r: &Map<String, Value>) -> Option<JobDetail> {
    let job = job(r)?;
    let mut properties = Vec::new();
    let mut add = |label: &str, value: String| {
        properties.push(PropertyInfo {
            group: String::new(),
            label: label.into(),
            value,
        })
    };
    add("Cada (segundos)", int(r, "rate").to_string());
    add("Demora inicial (segundos)", int(r, "delay").to_string());
    add(
        "Terminado",
        if flag(r, "done") { "Sí" } else { "No" }.into(),
    );
    add(
        "Cancelado",
        if flag(r, "cancelled") { "Sí" } else { "No" }.into(),
    );
    let mut notes = Vec::new();
    if job.can_stop {
        notes.push(STOP_CANCELS.into());
    } else if int(r, "rate") > 0 && job.enabled {
        notes.push(cancel_note(&job.name));
    }
    Some(JobDetail {
        job,
        steps: Vec::new(),
        schedules: Vec::new(),
        properties,
        notes,
    })
}

/// How to cancel a repeating job, which DBine doesn't offer (see
/// [`NO_STOP_REPEAT`]).
fn cancel_note(name: &str) -> String {
    format!(
        "APOC no detiene una ejecución de un job que se repite; para cancelarlo para siempre: CALL apoc.periodic.cancel({})",
        cypher::string(name)
    )
}

/// The server has no APOC (or not its periodic procedures).
pub(crate) fn missing_apoc(e: &Error) -> bool {
    let m = e.to_string();
    m.contains("apoc.periodic") && (m.contains("no procedure") || m.contains("not found"))
        || m.contains("ProcedureNotFound")
}

pub(crate) fn none(flavor: Flavor) -> Option<&'static str> {
    match flavor {
        Flavor::Neo4j => None,
        Flavor::Memgraph => Some(MEMGRAPH),
        Flavor::Neptune => Some(NEPTUNE),
    }
}

impl GraphSession {
    pub(crate) async fn apoc_jobs(&mut self) -> Result<Vec<Map<String, Value>>> {
        self.records(LIST).await
    }

    async fn apoc_job(&mut self, id: &str) -> Result<Map<String, Value>> {
        if let Some(why) = none(self.flavor) {
            return Err(Error::Unsupported(why.into()));
        }
        let name = parse_id(id).ok_or_else(|| bad_id(id))?;
        self.apoc_jobs()
            .await?
            .into_iter()
            .find(|r| r.get("name").map(as_text).as_deref() == Some(name))
            .ok_or_else(|| Error::Query(format!("no existe el job de APOC {name}")))
    }

    pub(crate) async fn jobs_list(&mut self) -> Result<JobList> {
        if let Some(why) = none(self.flavor) {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![why.into()],
            });
        }
        match self.apoc_jobs().await {
            Ok(rows) => Ok(JobList {
                jobs: rows.iter().filter_map(job).collect(),
                notes: vec![IN_MEMORY.into()],
            }),
            Err(e) if missing_apoc(&e) => Ok(JobList {
                jobs: Vec::new(),
                notes: vec![NO_APOC.into()],
            }),
            Err(e) => Err(e),
        }
    }

    pub(crate) async fn job_info(&mut self, id: &str) -> Result<JobDetail> {
        let r = self.apoc_job(id).await?;
        detail(&r).ok_or_else(|| bad_id(id))
    }

    pub(crate) async fn job_cancel(&mut self, id: &str) -> Result<()> {
        self.refuse_if_read_only(
            "Conexión de solo lectura: no se pueden detener jobs del servidor.",
        )?;
        let r = self.apoc_job(id).await?;
        if flag(&r, "done") || flag(&r, "cancelled") {
            return Err(Error::Query("el job ya terminó".into()));
        }
        if int(&r, "rate") > 0 {
            return Err(Error::Unsupported(NO_STOP_REPEAT.into()));
        }
        let name = parse_id(id).ok_or_else(|| bad_id(id))?;
        self.records(&format!(
            "CALL apoc.periodic.cancel({}) YIELD name RETURN name",
            cypher::string(name)
        ))
        .await?;
        Ok(())
    }
}

/// Why APOC jobs have no runs, script, "run now" or pause.
pub(crate) fn no_history() -> Error {
    Error::Unsupported("APOC no guarda el historial de sus jobs".into())
}

pub(crate) fn no_run() -> Error {
    Error::Unsupported("APOC no permite ejecutar un job a mano ni pausarlo: solo cancelarlo".into())
}

pub(crate) fn no_script() -> Error {
    Error::Unsupported("APOC no informa la consulta de sus jobs: no hay script".into())
}

/// No job has runs to show (see [`no_history`]).
pub(crate) fn runs() -> Result<Vec<JobRun>> {
    Err(no_history())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn row(v: Value) -> Map<String, Value> {
        v.as_object().unwrap().clone()
    }

    #[test]
    fn ids_are_checked() {
        assert_eq!(parse_id("apoc:my job"), Some("my job"));
        assert_eq!(parse_id("apoc:it's"), Some("it's"));
        for bad in ["", "apoc:", "my job", "apoc:a\nb", "transform:x"] {
            assert_eq!(parse_id(bad), None, "{bad:?}");
        }
        // The name is a Cypher string literal in the cancel.
        assert_eq!(
            cypher::string("x') YIELD name RETURN 1 //"),
            "'x\\') YIELD name RETURN 1 //'"
        );
    }

    #[test]
    fn rows_become_jobs() {
        let rep = job(&row(
            json!({"name": "rep", "delay": 0, "rate": 60, "done": false, "cancelled": false}),
        ))
        .unwrap();
        assert_eq!(rep.id, "apoc:rep");
        assert_eq!(rep.kind, "apoc.periodic");
        assert_eq!(rep.schedule.as_deref(), Some("rate 60s"));
        assert!(rep.enabled && !rep.running && !rep.can_stop);
        assert!(!rep.can_run && !rep.can_enable && !rep.has_history && !rep.has_script);
        let d = detail(&row(
            json!({"name": "it's", "delay": 0, "rate": 60, "done": false, "cancelled": false}),
        ))
        .unwrap();
        assert_eq!(d.notes, [cancel_note("it's")]);
        assert!(d.notes[0].ends_with("CALL apoc.periodic.cancel('it\\'s')"));
        assert!(!d.properties.iter().any(|p| p.label == "Para cancelarlo"));
        let once = job(&row(
            json!({"name": "once", "delay": 0, "rate": 0, "done": false, "cancelled": false}),
        ))
        .unwrap();
        assert!(once.running && once.schedule.is_none() && once.can_stop);
        let d = detail(&row(
            json!({"name": "once", "delay": 0, "rate": 0, "done": false, "cancelled": false}),
        ))
        .unwrap();
        assert_eq!(d.notes, [STOP_CANCELS]);
        assert!(!d.properties.iter().any(|p| p.label == "Al detenerlo"));
        let done = job(&row(
            json!({"name": "d", "delay": 1, "rate": 0, "done": true, "cancelled": false}),
        ))
        .unwrap();
        assert!(!done.enabled && !done.running && !done.can_stop && done.last_outcome.is_none());
        let d = detail(&row(
            json!({"name": "d", "delay": 1, "rate": 0, "done": true, "cancelled": true}),
        ))
        .unwrap();
        assert_eq!(d.job.last_outcome, Some(dbine_driver::JobOutcome::Canceled));
        assert!(d.notes.is_empty(), "nothing to stop or cancel");
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Demora inicial (segundos)" && p.value == "1"));
        assert!(job(&row(json!({"name": ""}))).is_none());
    }

    #[test]
    fn missing_apoc_is_recognised() {
        let e = Error::Query("There is no procedure with the name `apoc.periodic.list` registered for this database instance.".into());
        assert!(missing_apoc(&e));
        assert!(!missing_apoc(&Error::Query(
            "Neo.ClientError.Security.Unauthorized".into()
        )));
    }
}
