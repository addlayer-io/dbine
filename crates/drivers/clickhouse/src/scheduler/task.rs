//! Timeplus Proton's tasks (3.0 on) as the scheduler's jobs: `CREATE TASK
//! db.name SCHEDULE <interval> [TIMEOUT <interval>] [INTO db.target] AS
//! <select>`.
//!
//! - Read: `SHOW CREATE TASK` parsed into a [`TaskSpec`]; whether it's
//!   paused from `SHOW TASKS … verbose` (a profile that can't set
//!   `verbose` doesn't say).
//! - Changed in place: `CREATE OR REPLACE TASK` keeps the task's UUID, its
//!   creation and its last run, but leaves it unpaused, so a paused task is
//!   paused again after it; a change to the paused state alone is `SYSTEM
//!   PAUSE | UNPAUSE TASK`.
//! - Renamed: Proton has no `RENAME` for tasks: the new one is created and
//!   the old one dropped (its UUID and its last run go with it), which the
//!   script and the form say.
//! - Deleted: `DROP TASK`.
//!
//! Intervals are Proton's (`30s`, `5m`, `1h`, `1d`, `500ms`: up to days).
//! A cron schedule (`SCHEDULE '0 * * * *'`) is in Proton's grammar, but 3.0
//! answers that it isn't implemented; the form offers it, saying so.

use super::lex::{self, Parser, T};
use super::{
    bad, database_field, id_pairs, ids, name_field, one_statement, qname, query_field, same_pairs,
    shown, Ctx, Plan, Vals,
};
use crate::jobs::{self, job_id, parse_id};
use crate::schema::string_literal;
use crate::ClickHouseSession;
use dbine_driver::{Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

const REPLACE_NOTE: &str = "Los cambios reemplazan la tarea en su lugar (CREATE OR REPLACE TASK): conserva su UUID y su última ejecución.";
const RENAME_NOTE: &str = "Timeplus Proton no renombra tareas: con otro nombre o en otra base de datos se crea una tarea nueva y se borra esta, con su UUID y su última ejecución.";
const NO_STATUS_NOTE: &str =
    "Este usuario no puede cambiar ajustes (readonly): no se ve si la tarea está pausada.";

/// When a task runs.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Schedule {
    /// `1h`, `30s`…
    Every(String),
    /// A cron expression.
    Cron(String),
}

/// A task's `CREATE` statement, part by part.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct TaskSpec {
    pub(crate) database: String,
    pub(crate) name: String,
    pub(crate) schedule: Schedule,
    pub(crate) timeout: Option<String>,
    pub(crate) into: Option<(String, String)>,
    pub(crate) query: String,
}

/// Why an interval was refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum BadInterval {
    NotOne,
    Zero,
    /// Weeks, months, years: Proton takes up to days.
    TooLong,
}

/// An interval as Proton writes it (`500ms`, `30s`, `5m`, `1h`, `1d`), from
/// that or from `5 SECOND`, `INTERVAL 5 SECOND`, `5 seconds`…
pub(crate) fn proton_interval(text: &str) -> std::result::Result<String, BadInterval> {
    let t = text.trim();
    let t = match t.get(..9) {
        Some(head) if head.eq_ignore_ascii_case("INTERVAL ") => &t[9..],
        _ => t,
    };
    let compact: String = t.split_whitespace().collect();
    let digits = compact
        .find(|c: char| !c.is_ascii_digit())
        .ok_or(BadInterval::NotOne)?;
    let (n, unit) = compact.split_at(digits);
    if n.is_empty() || n.len() > 9 {
        return Err(BadInterval::NotOne);
    }
    let n: u64 = n.parse().map_err(|_| BadInterval::NotOne)?;
    // `M` is a month; `m` a minute.
    let unit = match unit {
        "M" => return Err(BadInterval::TooLong),
        u => match u.to_ascii_lowercase().as_str() {
            "ms" | "millisecond" | "milliseconds" => "ms",
            "s" | "sec" | "second" | "seconds" => "s",
            "m" | "min" | "minute" | "minutes" => "m",
            "h" | "hour" | "hours" => "h",
            "d" | "day" | "days" => "d",
            "w" | "week" | "weeks" | "month" | "months" | "q" | "quarter" | "quarters" | "y"
            | "year" | "years" => return Err(BadInterval::TooLong),
            _ => return Err(BadInterval::NotOne),
        },
    };
    if n == 0 {
        return Err(BadInterval::Zero);
    }
    Ok(format!("{n}{unit}"))
}

/// The tokens from `from` up to the next of the statement's keywords.
fn until_keyword(p: &Parser, from: usize) -> usize {
    let mut i = from;
    while i < p.t.len()
        && !["SCHEDULE", "TIMEOUT", "INTO", "AS", "SETTINGS"]
            .iter()
            .any(|k| p.is_kw_at(i, k))
    {
        i += 1;
    }
    i
}

/// Reads `SHOW CREATE TASK`.
pub(crate) fn parse_task(create: &str) -> Option<TaskSpec> {
    let mut p = Parser::new(create)?;
    p.kw("CREATE")?;
    if p.eat_kw("OR") {
        p.kw("REPLACE")?;
    }
    p.kw("TASK")?;
    if p.eat_kw("IF") {
        p.kw("NOT")?;
        p.kw("EXISTS")?;
    }
    let (db, name) = p.qname()?;
    let database = db.unwrap_or_default();
    let mut spec = TaskSpec {
        database: database.clone(),
        name,
        schedule: Schedule::Every(String::new()),
        timeout: None,
        into: None,
        query: String::new(),
    };
    fn interval(p: &mut Parser) -> String {
        let from = p.i;
        let to = until_keyword(p, from);
        p.i = to;
        let text = p.span(from, to).trim().to_string();
        proton_interval(&text).unwrap_or(text)
    }
    loop {
        if p.eat_kw("SCHEDULE") {
            spec.schedule = if p.kind_at(p.i) == Some(T::Str) {
                Schedule::Cron(p.string()?)
            } else {
                Schedule::Every(interval(&mut p))
            };
        } else if p.eat_kw("TIMEOUT") {
            spec.timeout = Some(interval(&mut p)).filter(|t| !t.is_empty());
        } else if p.eat_kw("INTO") {
            let (db, n) = p.qname()?;
            spec.into = Some((db.unwrap_or_else(|| database.clone()), n));
        } else if p.eat_kw("AS") {
            spec.query = lex::trim_query(p.rest(p.i));
            break;
        } else {
            return None;
        }
    }
    Some(spec)
}

/// `CREATE [OR REPLACE] TASK …`.
pub(crate) fn create_sql(t: &TaskSpec, replace: bool) -> String {
    let mut s = format!(
        "CREATE {}TASK {}\nSCHEDULE {}",
        if replace { "OR REPLACE " } else { "" },
        qname(&t.database, &t.name),
        match &t.schedule {
            Schedule::Every(i) => i.clone(),
            Schedule::Cron(c) => string_literal(c),
        }
    );
    if let Some(to) = &t.timeout {
        s.push_str(&format!("\nTIMEOUT {to}"));
    }
    if let Some((db, n)) = &t.into {
        s.push_str(&format!("\nINTO {}", qname(db, n)));
    }
    s.push_str(&format!("\nAS\n{}", t.query));
    s
}

// ---------------------------------------------------------------------------
// What the server has
// ---------------------------------------------------------------------------

/// Databases, and the streams and tables a task can write into.
#[derive(Debug, Clone, Default)]
pub(crate) struct Choices {
    pub(crate) databases: Vec<String>,
    pub(crate) targets: Vec<(String, String)>,
}

impl ClickHouseSession {
    pub(crate) async fn task_choices(&self) -> Result<Choices> {
        let databases = self
            .rows(
                "SELECT name FROM system.databases
                 WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name",
                &[],
            )
            .await?
            .iter()
            .filter_map(|r| jobs::opt(r.first()))
            .collect();
        let targets = self
            .rows(
                "SELECT database, name FROM system.tables
                 WHERE database NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema')
                   AND NOT is_temporary AND name NOT LIKE '.inner%'
                   AND engine NOT IN ('View', 'MaterializedView')
                 ORDER BY database, name LIMIT 20000",
                &[],
            )
            .await?
            .iter()
            .filter_map(|r| Some((jobs::opt(r.first())?, jobs::opt(r.get(1))?)))
            .collect();
        Ok(Choices { databases, targets })
    }

    /// The task's names in a database.
    async fn task_names(&self, db: &str) -> Result<Vec<String>> {
        let (rows, _) = self.tasks_in(db).await?;
        Ok(rows.iter().filter_map(|r| jobs::opt(r.first())).collect())
    }

    /// A task as the server has it, and whether it's unpaused (`None`: the
    /// user can't see it).
    pub(crate) async fn task_current(&self, id: &str) -> Result<(TaskSpec, Option<bool>)> {
        let (db, name) = ids(id)?;
        let (rows, known) = self.tasks_in(&db).await.map_err(|e| {
            if jobs::no_tasks(&e) {
                Error::Unsupported(jobs::NO_TASKS.into())
            } else {
                e
            }
        })?;
        let row = rows
            .into_iter()
            .find(|r| jobs::opt(r.first()).as_deref() == Some(name.as_str()))
            .ok_or_else(|| Error::Query(format!("no existe la tarea {db}.{name}")))?;
        let enabled = known.then(|| jobs::num(row.get(3)).is_none_or(|s| s == 0.0));
        let rows = self
            .rows(&format!("SHOW CREATE TASK {}", job_id(&db, &name)), &[])
            .await?;
        let create = rows
            .first()
            .and_then(|r| r.first())
            .map(crate::text)
            .unwrap_or_default();
        let mut spec = parse_task(&create).ok_or_else(|| {
            Error::Query(format!(
                "no se pudo leer la definición de la tarea {db}.{name}"
            ))
        })?;
        spec.database = db;
        spec.name = name;
        Ok((spec, enabled))
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

pub(crate) fn values_of(t: &TaskSpec, enabled: bool) -> BTreeMap<String, Value> {
    let (mode, every, cron) = match &t.schedule {
        Schedule::Every(i) => ("interval", i.clone(), String::new()),
        Schedule::Cron(c) => ("cron", String::new(), c.clone()),
    };
    BTreeMap::from([
        ("database".to_string(), json!(t.database)),
        ("name".to_string(), json!(t.name)),
        ("enabled".to_string(), json!(enabled)),
        ("schedule_mode".to_string(), json!(mode)),
        ("schedule".to_string(), json!(every)),
        ("cron".to_string(), json!(cron)),
        (
            "timeout".to_string(),
            json!(t.timeout.clone().unwrap_or_default()),
        ),
        (
            "into".to_string(),
            json!(t
                .into
                .as_ref()
                .map(|(d, n)| job_id(d, n))
                .unwrap_or_default()),
        ),
        ("query".to_string(), json!(t.query)),
    ])
}

pub(crate) fn build_form(
    t: &TaskSpec,
    enabled: Option<bool>,
    existing: bool,
    ch: &Choices,
) -> ObjectForm {
    let mut f = ObjectForm {
        values: values_of(t, enabled.unwrap_or(true)),
        ..ObjectForm::default()
    };
    f.sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("database", "Base de datos", FormKind::reference("database", false))
                .required(),
            FormField::new("name", "Nombre", FormKind::Text).required(),
            FormField::new("enabled", "Habilitada", FormKind::Bool)
                .default(json!(true))
                .help("SYSTEM UNPAUSE TASK / SYSTEM PAUSE TASK.")
                .read_only(existing && enabled.is_none()),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new(
                "schedule_mode",
                "Cuándo corre",
                FormKind::select(&[
                    ("interval", "Cada intervalo"),
                    ("cron", "Según una expresión cron"),
                ]),
            )
            .required()
            .default(json!("interval")),
            FormField::new("schedule", "Intervalo", FormKind::Text)
                .placeholder("1h")
                .help("Un número y una unidad: ms, s, m, h o d (como 30s, 5m, 1h o 1d).")
                .default(json!("1h"))
                .when("schedule_mode", &["interval"]),
            FormField::new("cron", "Expresión cron", FormKind::Cron)
                .placeholder("0 * * * *")
                .help("Timeplus Proton 3.0 todavía no ejecuta tareas con cron: las rechaza al crearlas.")
                .when("schedule_mode", &["cron"]),
            FormField::new("timeout", "Tiempo máximo (TIMEOUT)", FormKind::Text)
                .placeholder("30m")
                .help("Cuánto puede durar cada ejecución, con las mismas unidades. Vacío: el del servidor (1h)."),
        ]),
        FormSection::new("target", "Destino").field(
            FormField::new("into", "Destino (INTO)", FormKind::reference("stream", false)).help(
                "El stream o la tabla donde escribe lo que devuelve la consulta. Vacío: la tarea solo corre la consulta.",
            ),
        ),
        FormSection::new("query", "Consulta").field(
            FormField::new("query", "Consulta", FormKind::code("sql"))
                .required()
                .help("Una consulta SELECT histórica (sin streaming): la tarea la corre en cada ejecución."),
        ),
    ];
    f.choices
        .insert("database".into(), same_pairs(&ch.databases));
    let mut targets = ch.targets.clone();
    if let Some(i) = t.into.as_ref().filter(|i| !targets.contains(i)) {
        targets.push(i.clone());
    }
    f.choices.insert("stream".into(), id_pairs(&targets));
    if existing {
        f.notes.push(REPLACE_NOTE.into());
        f.notes.push(RENAME_NOTE.into());
        if enabled.is_none() {
            f.notes.push(NO_STATUS_NOTE.into());
        }
    }
    f
}

pub(crate) async fn form(s: &ClickHouseSession, _c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let ch = s.task_choices().await?;
    match id {
        None => {
            let database = if ch.databases.contains(&s.database) {
                s.database.clone()
            } else {
                ch.databases
                    .first()
                    .cloned()
                    .unwrap_or_else(|| "default".into())
            };
            let t = TaskSpec {
                database,
                name: String::new(),
                schedule: Schedule::Every("1h".into()),
                timeout: None,
                into: None,
                query: String::new(),
            };
            Ok(build_form(&t, Some(true), false, &ch))
        }
        Some(id) => {
            let (t, enabled) = s.task_current(id).await?;
            Ok(build_form(&t, enabled, true, &ch))
        }
    }
}

// ---------------------------------------------------------------------------
// From the designer's values
// ---------------------------------------------------------------------------

fn interval_field(v: Vals, k: &str, label: &str) -> Result<Option<String>> {
    let Some(t) = v.opt(k) else { return Ok(None) };
    match proton_interval(&t) {
        Ok(i) => Ok(Some(i)),
        Err(BadInterval::Zero) => Err(bad(format!("«{label}» tiene que ser mayor que cero."))),
        Err(BadInterval::TooLong) => Err(bad(format!(
            "«{label}»: Timeplus Proton toma intervalos de hasta días (d)."
        ))),
        Err(BadInterval::NotOne) => Err(bad(format!(
            "«{label}»: «{}» no es un intervalo de Timeplus Proton: un número y una unidad (ms, s, m, h o d), como 30s o 1h.",
            shown(&t)
        ))),
    }
}

/// The task the values describe, checked, and whether it should run
/// (unpaused).
pub(crate) fn wanted(
    values: &FormValues,
    ch: &Choices,
    current: Option<&TaskSpec>,
) -> Result<(TaskSpec, bool)> {
    let v = Vals(&values.values);
    let database = database_field(v, &ch.databases)?;
    let name = name_field(v, "name", "Nombre")?;
    let schedule = match v.text("schedule_mode").as_str() {
        "" | "interval" => Schedule::Every(
            interval_field(v, "schedule", "Intervalo")?
                .ok_or_else(|| bad("«Intervalo» es obligatorio.".into()))?,
        ),
        "cron" => {
            let c = v
                .opt("cron")
                .ok_or_else(|| bad("«Expresión cron» es obligatorio.".into()))?;
            if c.chars().any(char::is_control) || c.len() > 200 {
                return Err(bad(format!(
                    "«Expresión cron»: «{}» no es una expresión cron.",
                    shown(&c)
                )));
            }
            Schedule::Cron(c)
        }
        other => {
            return Err(bad(format!(
                "«{}» no es un modo de programación (intervalo o cron).",
                shown(other)
            )))
        }
    };
    let timeout = interval_field(v, "timeout", "Tiempo máximo (TIMEOUT)")?;
    let into = match v.opt("into") {
        None => None,
        Some(id) => Some(
            parse_id(&id)
                .filter(|p| {
                    ch.targets.contains(p) || current.and_then(|t| t.into.as_ref()) == Some(p)
                })
                .ok_or_else(|| {
                    bad(format!(
                        "«Destino (INTO)»: «{}» no es un stream ni una tabla de este servidor.",
                        shown(&id)
                    ))
                })?,
        ),
    };
    let t = TaskSpec {
        database,
        name,
        schedule,
        timeout,
        into,
        query: query_field(v)?,
    };
    Ok((t, v.bool_or("enabled", true)))
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

fn checked(p: Plan) -> Result<Plan> {
    for s in &p.statements {
        one_statement(s)?;
    }
    Ok(p)
}

fn label(t: &TaskSpec) -> String {
    format!("{}.{}", t.database, t.name)
}

pub(crate) fn plan_create(w: &TaskSpec, enabled: bool) -> Result<Plan> {
    let mut p = Plan {
        comments: vec![format!("Crea la tarea {} de Timeplus Proton.", label(w))],
        settings: Vec::new(),
        statements: vec![create_sql(w, false)],
        database: Some(w.database.clone()),
        id: job_id(&w.database, &w.name),
        restore: None,
    };
    if !enabled {
        p.statements
            .push(format!("SYSTEM PAUSE TASK {}", qname(&w.database, &w.name)));
    }
    checked(p)
}

/// The change from `cur` (unpaused or not; `None`: unknown) to `w`.
pub(crate) fn plan_edit(
    cur: &TaskSpec,
    cur_enabled: Option<bool>,
    w: &TaskSpec,
    enabled: bool,
) -> Result<Plan> {
    let old_id = qname(&cur.database, &cur.name);
    let new_id = qname(&w.database, &w.name);
    let mut p = Plan {
        comments: vec![format!(
            "Cambia la tarea {} de Timeplus Proton.",
            label(cur)
        )],
        settings: Vec::new(),
        statements: Vec::new(),
        database: None,
        id: job_id(&w.database, &w.name),
        restore: None,
    };
    if (&cur.database, &cur.name) != (&w.database, &w.name) {
        p.comments.push(format!(
            "Timeplus Proton no renombra tareas: se crea {} y se borra {}; se pierden su UUID y su última ejecución.",
            label(w),
            label(cur)
        ));
        p.database = Some(w.database.clone());
        p.statements.push(create_sql(w, false));
        if !enabled {
            p.statements.push(format!("SYSTEM PAUSE TASK {new_id}"));
        }
        p.statements.push(format!("DROP TASK {old_id}"));
        return checked(p);
    }
    let changed = cur.schedule != w.schedule
        || cur.timeout != w.timeout
        || cur.into != w.into
        || !lex::same_sql(&cur.query, &w.query);
    if changed {
        p.comments.push(
            "CREATE OR REPLACE TASK la cambia en su lugar: conserva su UUID y su última ejecución, y la deja sin pausar.".into(),
        );
        p.database = Some(w.database.clone());
        p.statements.push(create_sql(w, true));
        if !enabled {
            p.statements.push(format!("SYSTEM PAUSE TASK {new_id}"));
        }
    } else if enabled != cur_enabled.unwrap_or(true) {
        let verb = if enabled { "UNPAUSE" } else { "PAUSE" };
        p.statements.push(format!("SYSTEM {verb} TASK {new_id}"));
    }
    if p.statements.is_empty() {
        p.comments = vec![format!("{}: no hay cambios que guardar.", label(cur))];
    }
    checked(p)
}

pub(crate) fn drop_plan(cur: &TaskSpec) -> Plan {
    Plan {
        comments: vec![format!("Borra la tarea {} de Timeplus Proton.", label(cur))],
        settings: Vec::new(),
        statements: vec![format!("DROP TASK {}", qname(&cur.database, &cur.name))],
        database: None,
        id: job_id(&cur.database, &cur.name),
        restore: None,
    }
}

pub(crate) async fn plan(
    s: &ClickHouseSession,
    _c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let ch = s.task_choices().await?;
    match id {
        None => {
            let (w, enabled) = wanted(values, &ch, None)?;
            plan_create(&w, enabled)
        }
        Some(id) => {
            let (cur, cur_enabled) = s.task_current(id).await?;
            let (w, enabled) = wanted(values, &ch, Some(&cur))?;
            if (&w.database, &w.name) != (&cur.database, &cur.name)
                && s.task_names(&w.database).await?.contains(&w.name)
            {
                return Err(bad(format!(
                    "Ya existe la tarea {}.{}.",
                    w.database, w.name
                )));
            }
            plan_edit(&cur, cur_enabled, &w, enabled)
        }
    }
}

pub(crate) async fn delete_plan(s: &ClickHouseSession, id: &str) -> Result<Plan> {
    let (cur, _) = s.task_current(id).await?;
    Ok(drop_plan(&cur))
}
