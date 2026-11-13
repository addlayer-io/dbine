//! Neo4j's scheduled work through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest). Neo4j has no
//! scheduler of its own (Enterprise neither): what runs on its own are
//! APOC's background jobs, the one kind managed here ("job"):
//!
//! - `apoc.periodic.repeat(name, statement, rate, {params})`: runs the
//!   statement every `rate` seconds;
//! - `apoc.periodic.submit(name, statement, {params})`: runs it once, in
//!   the background;
//! - `apoc.periodic.countdown(name, statement, delay)`: runs it, and again
//!   after `delay` seconds while it returns a number above 0.
//!
//! They live in the server's memory (a restart loses them) and run in the
//! database of the session that created them. APOC lists their name, rate,
//! delay and whether they're done or cancelled, but not their statement or
//! parameters: an existing job's form brings them empty and the user types
//! them again. APOC has no call that changes a job: the edit runs the
//! create call again, which (with the same name) cancels the job and
//! schedules the new one; with a new name, it schedules the new one and
//! then cancels the old (`apoc.periodic.cancel`). Delete cancels it. No
//! history, log, "run at a step" or pause.
//!
//! A countdown can't be cancelled between two runs: APOC arms each next
//! run on its executor (`schedule(() -> submitJob(name, …), delay)`),
//! outside the job list that `cancel` clears, so the pending run comes
//! back, and it replaces whatever job has the countdown's name by then. It
//! stops only when its statement returns 0 (or the server restarts). APOC
//! lists a countdown as such (a `delay`, no `rate`) only until its first
//! run re-arms it; after that it looks like a one-off job. So a known
//! countdown isn't replaced under its own name (the edit asks for another
//! one), and the scripts and the form say what a cancel doesn't do.
//!
//! Ids are `apoc:<name>` (as [`crate::jobs`] lists them); the name, the
//! statement and every parameter go into the Cypher as literals
//! ([`cypher::string`], [`cypher::literal`]). APOC itself checks the
//! statement (an `EXPLAIN`) before it schedules it.

use crate::jobs::{self, flag, int, parse_id};
use crate::{as_text, cypher, Flavor, GraphSession};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerInfo,
    SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

#[cfg(test)]
mod tests;

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

const WHAT: &str = "Neo4j no tiene un planificador propio: estos son los jobs en segundo plano de APOC (apoc.periodic.repeat, submit y countdown).";
const IN_MEMORY: &str = "Los jobs de APOC viven en la memoria del servidor: no sobreviven a un reinicio, y APOC no guarda su consulta ni un historial de sus ejecuciones.";
const RETYPE: &str = "APOC no informa la consulta ni los parámetros de un job: para guardarlo hay que volver a escribirlos.";
const REPLACE: &str = "APOC no cambia un job: al guardar se vuelve a crear (con el mismo nombre, APOC cancela el anterior y programa el nuevo; con otro nombre, se programa el nuevo y se cancela el anterior).";
const COUNTDOWN: &str = "APOC no puede cancelar una cuenta regresiva entre dos ejecuciones: la próxima ya está programada, vuelve a armarla y reemplaza al job que tenga su nombre. Se detiene recién cuando su consulta devuelve 0 (o al reiniciar el servidor).";
const COUNTDOWN_SAME_NAME: &str = "Es una cuenta regresiva: APOC no la puede reemplazar con el mismo nombre, porque su próxima ejecución pisaría al job nuevo. Guardala con otro nombre.";
const MAYBE_COUNTDOWN: &str = "Si es una cuenta regresiva (después de su primera ejecución APOC la lista como un job de una vez), cancelarla no la detiene: su próxima ejecución la vuelve a armar y reemplaza al job que tenga su nombre.";
const NOT_FOUND: &str = "No existe ese job de APOC en el servidor.";
const READER: &str = "Este usuario solo tiene los roles reader y PUBLIC: APOC no le deja crear ni cambiar jobs (apoc.periodic.repeat, submit y countdown escriben), solo cancelarlos.";
const MAX_NAME: usize = 512;
const MAX_STATEMENT: usize = 1 << 20;
/// A year, in seconds: the longest rate DBine writes.
const MAX_RATE: i64 = 31_536_000;

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    cypher::comment_text(s.trim()).chars().take(60).collect()
}

fn check_kind(kind: &str) -> Result<()> {
    if kind.trim() == "job" {
        Ok(())
    } else {
        Err(bad(format!(
            "«{}» no es un tipo de objeto del planificador de Neo4j: solo hay jobs de APOC.",
            shown(kind)
        )))
    }
}

// ---------------------------------------------------------------------------
// Jobs as APOC lists them
// ---------------------------------------------------------------------------

/// How a job runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Mode {
    Repeat,
    Submit,
    Countdown,
}

impl Mode {
    fn parse(v: &str) -> Result<Mode> {
        Ok(match v.trim() {
            "" | "repeat" => Mode::Repeat,
            "submit" => Mode::Submit,
            "countdown" => Mode::Countdown,
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de job de APOC (se repite, una vez o cuenta regresiva).",
                    shown(other)
                )))
            }
        })
    }

    fn key(self) -> &'static str {
        match self {
            Mode::Repeat => "repeat",
            Mode::Submit => "submit",
            Mode::Countdown => "countdown",
        }
    }

    fn procedure(self) -> &'static str {
        match self {
            Mode::Repeat => "apoc.periodic.repeat",
            Mode::Submit => "apoc.periodic.submit",
            Mode::Countdown => "apoc.periodic.countdown",
        }
    }
}

/// One row of `apoc.periodic.list()`.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Job {
    pub name: String,
    pub rate: i64,
    pub delay: i64,
    pub done: bool,
    pub cancelled: bool,
}

impl Job {
    pub(crate) fn of(r: &Map<String, Value>) -> Option<Job> {
        let name = r.get("name").map(as_text).filter(|n| !n.is_empty())?;
        Some(Job {
            name,
            rate: int(r, "rate"),
            delay: int(r, "delay"),
            done: flag(r, "done"),
            cancelled: flag(r, "cancelled"),
        })
    }

    fn finished(&self) -> bool {
        self.done || self.cancelled
    }

    /// A countdown as APOC lists it until its first run re-arms it: a
    /// `delay` (the wait between runs) and no rate.
    pub(crate) fn countdown(&self) -> bool {
        self.rate == 0 && self.delay > 0
    }

    /// A job without a rate: one-off, or a countdown APOC no longer tells
    /// apart from one.
    pub(crate) fn maybe_countdown(&self) -> bool {
        self.rate == 0 && !self.countdown()
    }

    /// What APOC's list lets tell: a rate on a job not done is a repeat, a
    /// delay without a rate a countdown; the rest, one-off.
    fn mode(&self) -> Mode {
        if self.countdown() {
            Mode::Countdown
        } else if self.rate > 0 && !self.done {
            Mode::Repeat
        } else {
            Mode::Submit
        }
    }

    /// For the tree, each form whole so the UI translates it.
    fn describe(&self) -> String {
        let (r, d) = (self.rate, self.delay);
        match (self.rate > 0, self.countdown(), self.cancelled, self.done) {
            (true, _, true, _) => format!("Se repite cada {r} s (cancelado)"),
            (true, _, _, true) => format!("Se repite cada {r} s (terminado)"),
            (true, _, _, _) => format!("Se repite cada {r} s"),
            (_, true, true, _) => format!("Cuenta regresiva: se repite cada {d} s (cancelada)"),
            (_, true, _, true) => format!("Cuenta regresiva: se repite cada {d} s (terminada)"),
            (_, true, _, _) => format!("Cuenta regresiva: se repite cada {d} s"),
            (_, _, true, _) => "Una vez (cancelado)".into(),
            (_, _, _, true) => "Una vez (terminado)".into(),
            _ => "Una vez".into(),
        }
    }
}

pub(crate) fn job_id(name: &str) -> String {
    format!("apoc:{name}")
}

/// Whether the login may schedule APOC jobs: `repeat`, `submit` and
/// `countdown` are WRITE procedures (`cancel` isn't). `roles` is `SHOW
/// CURRENT USER`'s column. Only a login whose every role is a built-in
/// read-only one (Enterprise's `reader`, `PUBLIC`) is known not to write;
/// Community has no roles (`null`) and custom roles are the server's to
/// judge.
pub(crate) fn may_write(roles: Option<&Value>) -> bool {
    match roles {
        Some(Value::Array(r)) if !r.is_empty() => !r
            .iter()
            .all(|x| matches!(x.as_str(), Some("reader" | "PUBLIC"))),
        _ => true,
    }
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// What the designer asks for, checked.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wanted {
    pub name: String,
    pub mode: Mode,
    pub statement: String,
    pub rate: i64,
    pub params: Map<String, Value>,
}

fn text(values: &BTreeMap<String, Value>, k: &str) -> String {
    match values.get(k) {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Null) | None => String::new(),
        Some(v @ (Value::Array(_) | Value::Object(_))) => v.to_string(),
        Some(v) => v.to_string(),
    }
}

pub(crate) fn wanted(values: &FormValues) -> Result<Wanted> {
    let v = &values.values;
    let name = text(v, "name").trim().to_string();
    if name.is_empty() {
        return Err(bad("El job necesita un nombre.".into()));
    }
    if name.chars().count() > MAX_NAME {
        return Err(bad(format!(
            "El nombre del job puede tener hasta {MAX_NAME} caracteres."
        )));
    }
    if name.chars().any(char::is_control) {
        return Err(bad(
            "El nombre del job no puede tener saltos de línea ni caracteres de control.".into(),
        ));
    }
    let mode = Mode::parse(&text(v, "type"))?;
    let statement = text(v, "statement").trim().to_string();
    if statement.is_empty() {
        return Err(bad("Escribí la consulta Cypher que ejecuta el job.".into()));
    }
    if statement.len() > MAX_STATEMENT {
        return Err(bad("La consulta del job es demasiado larga.".into()));
    }
    if statement.contains('\0') {
        return Err(bad(
            "La consulta del job no puede tener caracteres nulos.".into()
        ));
    }
    let rate = match mode {
        Mode::Submit => 0,
        Mode::Repeat | Mode::Countdown => {
            let raw = text(v, "rate");
            let n = match v.get("rate") {
                Some(Value::Number(n)) => n
                    .as_i64()
                    .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
                _ => raw.trim().parse::<i64>().ok(),
            };
            match n {
                Some(n) if (1..=MAX_RATE).contains(&n) => n,
                Some(_) => {
                    return Err(bad(format!(
                        "«Cada (segundos)» tiene que estar entre 1 y {MAX_RATE}."
                    )))
                }
                None if raw.trim().is_empty() => {
                    return Err(bad("Indicá cada cuántos segundos se ejecuta el job.".into()))
                }
                None => {
                    return Err(bad(
                        "«Cada (segundos)» tiene que ser un número entero.".into()
                    ))
                }
            }
        }
    };
    let params = match mode {
        Mode::Countdown => Map::new(),
        Mode::Repeat | Mode::Submit => match v.get("params") {
            None | Some(Value::Null) => Map::new(),
            Some(Value::Object(o)) => o.clone(),
            Some(Value::String(s)) if s.trim().is_empty() => Map::new(),
            Some(Value::String(s)) => match serde_json::from_str::<Value>(s) {
                Ok(Value::Object(o)) => o,
                Ok(_) => {
                    return Err(bad(
                        "Los parámetros tienen que ser un objeto JSON ({\"nombre\": valor})."
                            .into(),
                    ))
                }
                Err(e) => return Err(bad(format!("Los parámetros no son un JSON válido: {e}."))),
            },
            Some(_) => {
                return Err(bad(
                    "Los parámetros tienen que ser un objeto JSON ({\"nombre\": valor}).".into(),
                ))
            }
        },
    };
    Ok(Wanted {
        name,
        mode,
        statement,
        rate,
        params,
    })
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// What a change runs (each statement on its own, in order), the script
/// shown (comments, then the statements) and the job's id.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub script: String,
    pub statements: Vec<String>,
    pub id: String,
}

impl Plan {
    fn new(comments: &[String], statements: Vec<String>, id: String) -> Result<Plan> {
        let mut script = String::new();
        for c in comments {
            script.push_str("// ");
            script.push_str(&cypher::comment_text(c));
            script.push('\n');
        }
        for s in &statements {
            script.push_str(s);
            script.push_str(";\n");
        }
        // Every statement is one unit of the script as the editor splits it.
        let units = cypher::script(&script);
        if units.len() != statements.len()
            || units.iter().zip(&statements).any(|(u, s)| u.text != *s)
        {
            return Err(Error::Unsupported(
                "DBine no puede escribir este job como un script que se lea igual: revisá el nombre y la consulta.".into(),
            ));
        }
        Ok(Plan {
            script,
            statements,
            id,
        })
    }
}

pub(crate) fn create_call(w: &Wanted) -> String {
    let name = cypher::string(&w.name);
    let stmt = cypher::string(&w.statement);
    let config = if w.params.is_empty() {
        String::new()
    } else {
        format!(
            ", {{params: {}}}",
            cypher::literal(&Value::Object(w.params.clone()))
        )
    };
    match w.mode {
        Mode::Repeat => format!(
            "CALL apoc.periodic.repeat({name}, {stmt}, {}{config})",
            w.rate
        ),
        Mode::Submit => format!("CALL apoc.periodic.submit({name}, {stmt}{config})"),
        Mode::Countdown => format!("CALL apoc.periodic.countdown({name}, {stmt}, {})", w.rate),
    }
}

pub(crate) fn cancel_call(name: &str) -> String {
    format!("CALL apoc.periodic.cancel({})", cypher::string(name))
}

/// The job's database, for the comment.
fn database_text(database: &str) -> String {
    if database.is_empty() {
        "la base predeterminada del usuario".into()
    } else {
        format!("la base «{database}»")
    }
}

/// The script for `w`: a new job (`current` `None`) or the job again
/// (`current` its name); `jobs` are the server's, to refuse a name a
/// running job has.
pub(crate) fn plan(
    current: Option<&Job>,
    w: &Wanted,
    jobs: &[Job],
    database: &str,
) -> Result<Plan> {
    // A known countdown keeps its name even when done: its next run may
    // still come back.
    let taken = |name: &str| {
        jobs.iter()
            .any(|j| j.name == name && (!j.finished() || j.countdown()))
    };
    // A finished job without a rate under that name may be a countdown
    // whose next run is pending.
    let stale = |name: &str| {
        jobs.iter()
            .any(|j| j.name == name && j.finished() && j.maybe_countdown())
    };
    let what = format!(
        "Job de APOC «{}» ({}), en {}.",
        w.name,
        w.mode.procedure(),
        database_text(database)
    );
    let lost = "Vive en la memoria del servidor: no sobrevive a un reinicio.".to_string();
    let Some(cur) = current else {
        if taken(&w.name) {
            return Err(bad(format!(
                "Ya hay un job de APOC «{}» en el servidor.",
                shown(&w.name)
            )));
        }
        let mut notes = vec![what, lost];
        if stale(&w.name) {
            notes.push(MAYBE_COUNTDOWN.into());
        }
        return Plan::new(&notes, vec![create_call(w)], job_id(&w.name));
    };
    if cur.name == w.name {
        if cur.countdown() {
            return Err(bad(COUNTDOWN_SAME_NAME.into()));
        }
        let mut notes = vec![
            what,
            "APOC no cambia un job: con el mismo nombre, esta llamada cancela el anterior y programa este.".into(),
            lost,
        ];
        if cur.maybe_countdown() {
            notes.push(MAYBE_COUNTDOWN.into());
        }
        return Plan::new(&notes, vec![create_call(w)], job_id(&w.name));
    }
    if taken(&w.name) {
        return Err(bad(format!(
            "Ya hay un job de APOC «{}» en el servidor.",
            shown(&w.name)
        )));
    }
    let mut notes = vec![
        what,
        format!(
            "Reemplaza al job «{}»: se programa el nuevo y después se cancela el anterior.",
            cur.name
        ),
        lost,
    ];
    if cur.countdown() {
        notes.push(COUNTDOWN.into());
    } else if cur.maybe_countdown() {
        notes.push(MAYBE_COUNTDOWN.into());
    }
    if stale(&w.name) {
        notes.push(MAYBE_COUNTDOWN.into());
    }
    notes.dedup();
    Plan::new(
        &notes,
        vec![create_call(w), cancel_call(&cur.name)],
        job_id(&w.name),
    )
}

pub(crate) fn delete_plan(j: &Job) -> Result<Plan> {
    let notes = if j.countdown() {
        vec![
            format!(
                "Cancela la cuenta regresiva de APOC «{}» y la quita de la lista del servidor, si todavía no terminó su primera ejecución.",
                j.name
            ),
            COUNTDOWN.into(),
        ]
    } else if j.maybe_countdown() {
        vec![
            format!(
                "Cancela el job de APOC «{}» y lo quita de la lista del servidor.",
                j.name
            ),
            MAYBE_COUNTDOWN.into(),
        ]
    } else {
        vec![format!(
            "Cancela el job de APOC «{}» y lo quita de la lista del servidor.",
            j.name
        )]
    };
    Plan::new(&notes, vec![cancel_call(&j.name)], job_id(&j.name))
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

pub(crate) fn form_of(job: Option<&Job>, database: &str) -> ObjectForm {
    let mut f = ObjectForm::default();
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("Único en el servidor: un job nuevo con el nombre de otro lo reemplaza."),
        FormField::new(
            "type",
            "Tipo",
            FormKind::select(&[
                ("repeat", "Se repite (apoc.periodic.repeat)"),
                ("submit", "Una vez, en segundo plano (apoc.periodic.submit)"),
                ("countdown", "Cuenta regresiva (apoc.periodic.countdown)"),
            ]),
        )
        .required()
        .default(json!("repeat")),
        FormField::new(
            "rate",
            "Cada (segundos)",
            FormKind::number(Some(1.0), Some(MAX_RATE as f64)),
        )
        .required()
        .when("type", &["repeat", "countdown"])
        .default(json!(60))
        .help("En una cuenta regresiva, la espera entre una ejecución y la siguiente."),
        FormField::new("database", "Base de datos", FormKind::Text)
            .read_only(true)
            .help("El job corre en la base de la conexión con la que se crea; vacía, en la base predeterminada del usuario."),
    ]);
    if job.is_some() {
        general = general.fields(vec![
            FormField::new(
                "delay",
                "Demora inicial (segundos)",
                FormKind::number(Some(0.0), None),
            )
            .read_only(true),
            FormField::new("done", "Terminado", FormKind::Bool).read_only(true),
            FormField::new("cancelled", "Cancelado", FormKind::Bool).read_only(true),
        ]);
    }
    let statement = FormSection::new("statement", "Consulta").fields(vec![
        FormField::new("statement", "Consulta", FormKind::code("cypher"))
            .required()
            .placeholder("MATCH (n:Sesion) WHERE n.vence < datetime() DETACH DELETE n")
            .help("La consulta Cypher que ejecuta el job. En una cuenta regresiva tiene que devolver un número: el job se repite mientras sea mayor que 0."),
        FormField::new("params", "Parámetros", FormKind::code("json"))
            .when("type", &["repeat", "submit"])
            .default(json!("{}"))
            .help("Un objeto JSON: la consulta lee cada clave como $nombre."),
    ]);
    f.sections = vec![general, statement];
    // Empty: the user's default database (the field's help says so).
    f.values.insert("database".into(), json!(database));
    f.values.insert("statement".into(), json!(""));
    f.values.insert("params".into(), json!("{}"));
    match job {
        None => {
            f.values.insert("name".into(), json!(""));
            f.values.insert("type".into(), json!("repeat"));
            f.values.insert("rate".into(), json!(60));
        }
        Some(j) => {
            f.values.insert("name".into(), json!(j.name));
            f.values.insert("type".into(), json!(j.mode().key()));
            f.values.insert(
                "rate".into(),
                if j.rate > 0 {
                    json!(j.rate)
                } else if j.countdown() {
                    json!(j.delay)
                } else {
                    json!(60)
                },
            );
            f.values.insert("delay".into(), json!(j.delay));
            f.values.insert("done".into(), json!(j.done));
            f.values.insert("cancelled".into(), json!(j.cancelled));
            f.notes.push(RETYPE.into());
            if j.countdown() {
                f.notes.push(COUNTDOWN_SAME_NAME.into());
            } else {
                f.notes.push(REPLACE.into());
            }
            if j.maybe_countdown() {
                f.notes.push(MAYBE_COUNTDOWN.into());
            }
        }
    }
    if job.is_none_or(|j| j.countdown()) {
        f.notes.push(COUNTDOWN.into());
    }
    f.notes.push(IN_MEMORY.into());
    f
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

impl GraphSession {
    /// The jobs, or why there are none to manage.
    async fn sched_jobs(&mut self) -> Result<std::result::Result<Vec<Job>, &'static str>> {
        if let Some(why) = jobs::none(self.flavor) {
            return Ok(Err(why));
        }
        match self.apoc_jobs().await {
            Ok(rows) => Ok(Ok(rows.iter().filter_map(Job::of).collect())),
            Err(e) if jobs::missing_apoc(&e) => Ok(Err(jobs::NO_APOC)),
            Err(e) => Err(e),
        }
    }

    /// The jobs, refusing a server where there are none to manage.
    async fn sched_ready(&mut self) -> Result<Vec<Job>> {
        match self.sched_jobs().await? {
            Ok(all) => Ok(all),
            Err(why) => Err(Error::Unsupported(why.into())),
        }
    }

    /// See [`may_write`]; a server that can't say lets the user try.
    async fn sched_may_write(&mut self) -> bool {
        match self.records("SHOW CURRENT USER YIELD roles").await {
            Ok(rows) => may_write(rows.first().and_then(|r| r.get("roles"))),
            Err(e) => {
                tracing::debug!("neo4j scheduler: SHOW CURRENT USER: {e}");
                true
            }
        }
    }

    async fn sched_job(&mut self, id: &str) -> Result<(Job, Vec<Job>)> {
        let name = parse_id(id).ok_or_else(|| {
            bad(format!(
                "«{}» no es un id de job de APOC",
                id.chars().take(200).collect::<String>()
            ))
        })?;
        let all = self.sched_ready().await?;
        let j = all
            .iter()
            .find(|j| j.name == name)
            .cloned()
            .ok_or_else(|| bad(NOT_FOUND.into()))?;
        Ok((j, all))
    }

    pub(crate) async fn sched_info(&mut self) -> Result<SchedulerInfo> {
        match self.sched_jobs().await? {
            Err(why) => Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![why.into()],
            }),
            Ok(_) => {
                let write = self.sched_may_write().await;
                let mut notes = vec![WHAT.into(), IN_MEMORY.into()];
                if !write {
                    notes.push(READER.into());
                }
                Ok(SchedulerInfo {
                    kinds: vec![SchedulerKind::new("job", "Jobs de APOC", "job de APOC")
                        .abilities(write, write, true)],
                    logs: false,
                    notes,
                })
            }
        }
    }

    pub(crate) async fn sched_items(&mut self, kind: &str) -> Result<SchedulerList> {
        check_kind(kind)?;
        match self.sched_jobs().await? {
            Err(why) => Ok(SchedulerList {
                items: Vec::new(),
                notes: vec![why.into()],
            }),
            Ok(all) => Ok(SchedulerList {
                items: all
                    .iter()
                    .map(|j| SchedulerItem {
                        id: job_id(&j.name),
                        name: j.name.clone(),
                        description: Some(j.describe()),
                        enabled: Some(!j.finished()),
                        system: false,
                        can_edit: None,
                        can_delete: None,
                        actions: None,
                        run_from_step: None,
                        clear_history: None,
                    })
                    .collect(),
                notes: vec![IN_MEMORY.into()],
            }),
        }
    }

    pub(crate) async fn sched_form(&mut self, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
        check_kind(kind)?;
        let job = match id {
            Some(id) => Some(self.sched_job(id).await?.0),
            None => {
                self.sched_ready().await?;
                None
            }
        };
        let mut f = form_of(job.as_ref(), &self.db);
        if !self.sched_may_write().await {
            f.read_only = true;
            f.notes.insert(0, READER.into());
        }
        Ok(f)
    }

    async fn sched_plan(&mut self, id: Option<&str>, values: &FormValues) -> Result<Plan> {
        let w = wanted(values)?;
        match id {
            None => {
                let all = self.sched_ready().await?;
                plan(None, &w, &all, &self.db)
            }
            Some(id) => {
                let (cur, all) = self.sched_job(id).await?;
                plan(Some(&cur), &w, &all, &self.db)
            }
        }
    }

    pub(crate) async fn sched_script(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<String> {
        check_kind(kind)?;
        Ok(self.sched_plan(id, values).await?.script)
    }

    async fn run_plan(&mut self, plan: &Plan) -> Result<()> {
        // Read-only connections never get here: ReadOnlySession refuses
        // saves and deletes.
        for (i, stmt) in plan.statements.iter().enumerate() {
            if let Err(e) = self.records(stmt).await {
                if i == 0 {
                    return Err(e);
                }
                return Err(bad(format!(
                    "Se programó el job nuevo, pero no se pudo cancelar el anterior: {e}"
                )));
            }
        }
        Ok(())
    }

    pub(crate) async fn sched_save(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<Option<String>> {
        check_kind(kind)?;
        let plan = self.sched_plan(id, values).await?;
        self.run_plan(&plan).await?;
        Ok(Some(plan.id))
    }

    pub(crate) async fn sched_delete_script(&mut self, kind: &str, id: &str) -> Result<String> {
        check_kind(kind)?;
        let (j, _) = self.sched_job(id).await?;
        Ok(delete_plan(&j)?.script)
    }

    pub(crate) async fn sched_delete(&mut self, kind: &str, id: &str) -> Result<()> {
        check_kind(kind)?;
        let (j, _) = self.sched_job(id).await?;
        self.run_plan(&delete_plan(&j)?).await
    }
}

/// Only Neo4j has APOC's jobs.
pub(crate) fn supported(flavor: Flavor) -> bool {
    flavor == Flavor::Neo4j
}
