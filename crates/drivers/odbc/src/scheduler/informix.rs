//! Informix and GBase 8s: the DB Scheduler in the `sysadmin` database,
//! managed whole by INSERT / UPDATE / DELETE on its tables, the way the
//! Administrator's Guide does it: tasks and sensors (`ph_task`, every
//! column: type, group, database, what it runs, start and stop time,
//! frequency, days, next run, priority, enabled, a sensor's result table,
//! its CREATE and how long it keeps data), each one's parameters
//! (`ph_threshold`, by task name), and the groups they belong to
//! (`ph_group`). A save is one transaction when the session's database
//! has a log (sysadmin always has one). Tables are named `sysadmin:table`,
//! so the session stays in its own database.
//!
//! Times are DATETIME / INTERVAL values in the server's time zone, cast to
//! text to read them and written as literals checked for their shape.
//! Texts are literals with their quotes doubled; one with a line break
//! needs `IFX_ALLOW_NEWLINE` first, which the script calls. The alerts
//! (`ph_alert`) and the latest runs (`ph_run`) are the scheduler's log;
//! clearing a task's history deletes its `ph_run` rows.

use super::*;
use crate::jobs::informix::{built_in, NO_ACCESS, NO_SYSADMIN, NO_TABLES};
use crate::jobs::{code, int, int_id, lit, not_found, parse_time, parse_ts, yes};
use dbine_driver::{FormField, FormKind, FormList, FormSection, SchedulerKind};
use serde_json::json;

pub(super) enum Kind {
    Task,
    Group,
}

pub(super) fn kind(kind: &str) -> Result<Kind> {
    match kind.trim() {
        "job" => Ok(Kind::Task),
        "group" => Ok(Kind::Group),
        other => Err(bad_kind(other)),
    }
}

const TYPES: &[&str] = &["TASK", "SENSOR", "STARTUP TASK", "STARTUP SENSOR"];
const DAYS: [(&str, &str); 7] = [
    ("mon", "tk_monday"),
    ("tue", "tk_tuesday"),
    ("wed", "tk_wednesday"),
    ("thu", "tk_thursday"),
    ("fri", "tk_friday"),
    ("sat", "tk_saturday"),
    ("sun", "tk_sunday"),
];
/// INTERVAL DAY(2) TO SECOND: up to 99 days.
const MAX_INTERVAL: i64 = 99 * 86_400 + 86_399;
const PRIVILEGES: &str = "Cambiar el planificador pide permisos sobre la base sysadmin: el usuario informix, un DBA de sysadmin o quien tenga INSERT, UPDATE y DELETE sobre sus tablas.";
const BUILT_IN: &str = "Es una tarea del servidor (de las que trae Informix): cambiarla o borrarla cambia lo que el servidor hace solo.";

/// A task or sensor as `ph_task` has it. Times: seconds after midnight;
/// intervals: seconds; the next run: the server's local time in ms.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Task {
    pub(super) name: String,
    pub(super) kind: String,
    pub(super) group: String,
    pub(super) description: String,
    pub(super) database: String,
    pub(super) execute: String,
    pub(super) start: Option<i64>,
    pub(super) stop: Option<i64>,
    pub(super) frequency: Option<i64>,
    pub(super) days: [bool; 7],
    pub(super) enabled: bool,
    pub(super) priority: Option<i64>,
    pub(super) next: Option<i64>,
    pub(super) result_table: Option<String>,
    pub(super) create: Option<String>,
    pub(super) keep: Option<i64>,
    /// Parameter name → (value, type, description).
    pub(super) thresholds: BTreeMap<String, (String, String, String)>,
}

/// A table of the scheduler that isn't there or can't be read: why.
fn note(e: &Error) -> Option<&'static str> {
    match code(e) {
        Some(-329) => Some(NO_SYSADMIN),
        Some(-387 | -272 | -389) => Some(NO_ACCESS),
        Some(-206) => Some(NO_TABLES),
        _ => None,
    }
}

/// The login may write the scheduler's tables: DBA of sysadmin, or INSERT,
/// UPDATE and DELETE granted on ph_task (to it, to public or to a role it
/// was granted). `None`: it can't be told.
fn may_write(src: &mut dyn JobSource) -> Option<bool> {
    let dba = src
        .query(
            "SELECT usertype FROM sysadmin:sysusers WHERE username IN (USER, 'public')",
            &[],
        )
        .ok()?;
    if (0..dba.rows.len()).any(|r| dba.get(r, "usertype") == Some("D")) {
        return Some(true);
    }
    let me = src
        .query("SELECT FIRST 1 USER AS me FROM sysadmin:systables", &[])
        .ok()?;
    if me.get(0, "me") == Some("informix") {
        return Some(true);
    }
    let grants = src
        .query(
            "SELECT a.tabauth FROM sysadmin:systabauth a, sysadmin:systables t WHERE t.tabname = 'ph_task' AND a.tabid = t.tabid AND (a.grantee IN (USER, 'public') OR a.grantee IN (SELECT r.rolename FROM sysadmin:sysroleauth r WHERE r.grantee IN (USER, 'public')))",
            &[],
        )
        .ok()?;
    let all: String = (0..grants.rows.len())
        .filter_map(|r| grants.get(r, "tabauth"))
        .collect::<String>()
        .to_ascii_lowercase();
    Some(all.contains('u') && all.contains('i') && all.contains('d'))
}

pub(super) fn info(src: &mut dyn JobSource, _ctx: &Ctx) -> Result<SchedulerInfo> {
    if let Err(e) = src.query("SELECT FIRST 1 tk_id FROM sysadmin:ph_task", &[]) {
        return match note(&e) {
            Some(n) => Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![n.to_string()],
            }),
            None => Err(e),
        };
    }
    // When the privileges can't be read, the server decides on the save.
    let w = may_write(src) != Some(false);
    let mut notes = Vec::new();
    if !w {
        notes.push(PRIVILEGES.to_string());
    }
    Ok(SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Tareas y sensores", "tarea").abilities(w, w, w),
            SchedulerKind::new("group", "Grupos", "grupo").abilities(w, w, w),
        ],
        logs: true,
        notes,
    })
}

// ------------------------------------------------------------------ tasks

pub(super) fn task_sql(n: u64) -> String {
    format!(
        "SELECT t.*, CAST(t.tk_start_time AS CHAR(8)) AS dbine_start, CAST(t.tk_stop_time AS CHAR(8)) AS dbine_stop,
       CAST(t.tk_frequency AS CHAR(20)) AS dbine_frequency, CAST(t.tk_next_execution AS CHAR(19)) AS dbine_next,
       CAST(t.tk_delete AS CHAR(20)) AS dbine_delete
  FROM sysadmin:ph_task t WHERE t.tk_id = {n}"
    )
}

pub(super) const THRESHOLDS: &str =
    "SELECT * FROM sysadmin:ph_threshold WHERE task_name = ? ORDER BY name";

/// `[D ]HH:MM:SS[.fff]`, as an INTERVAL DAY TO SECOND reads as text, in
/// seconds.
pub(super) fn interval_secs(v: &str) -> Option<i64> {
    let parts: Vec<&str> = v.split_whitespace().collect();
    let (days, time) = match parts.as_slice() {
        [d, t] => (d.parse::<i64>().ok()?, *t),
        [t] if t.contains(':') => (0, *t),
        [d] => (d.parse::<i64>().ok()?, "00:00:00"),
        _ => return None,
    };
    let ms = parse_time(time)?;
    (days >= 0).then_some(days * 86_400 + ms / 1000)
}

/// Seconds as an INTERVAL DAY TO SECOND literal.
pub(super) fn interval_literal(secs: i64) -> String {
    format!(
        "INTERVAL({} {}) DAY TO SECOND",
        secs / 86_400,
        time_text(secs % 86_400)
    )
}

fn time_literal(secs: Option<i64>) -> String {
    secs.map(|s| format!("DATETIME({}) HOUR TO SECOND", time_text(s)))
        .unwrap_or_else(|| "NULL".into())
}

fn datetime_literal(ms: Option<i64>) -> String {
    ms.map(|ms| {
        let iso = crate::jobs::rfc3339(ms);
        format!("DATETIME({} {}) YEAR TO SECOND", &iso[..10], &iso[11..19])
    })
    .unwrap_or_else(|| "NULL".into())
}

fn bool_literal(b: bool) -> String {
    if b { "'t'" } else { "'f'" }.into()
}

fn text_literal(v: Option<&str>) -> String {
    v.map(lit).unwrap_or_else(|| "NULL".into())
}

fn task_of(s: &Set, th: &Set) -> Task {
    let t = |c: &str| s.get(0, c).map(str::to_string);
    let mut days = [true; 7];
    for (i, (_, c)) in DAYS.iter().enumerate() {
        days[i] = s.get(0, c).is_none_or(|v| yes(Some(v)));
    }
    let thresholds = (0..th.rows.len())
        .filter_map(|r| {
            Some((
                th.get(r, "name")?.to_string(),
                (
                    th.get(r, "value").unwrap_or_default().to_string(),
                    th.get(r, "value_type").unwrap_or_default().to_string(),
                    th.get(r, "description").unwrap_or_default().to_string(),
                ),
            ))
        })
        .collect();
    Task {
        name: t("tk_name").unwrap_or_default(),
        kind: t("tk_type")
            .map(|k| k.to_ascii_uppercase())
            .unwrap_or_else(|| "TASK".into()),
        group: t("tk_group").unwrap_or_else(|| "MISC".into()),
        description: t("tk_description").unwrap_or_default(),
        database: t("tk_dbs").unwrap_or_else(|| "sysadmin".into()),
        execute: t("tk_execute").unwrap_or_default(),
        start: s
            .get(0, "dbine_start")
            .and_then(parse_time)
            .map(|ms| ms / 1000),
        stop: s
            .get(0, "dbine_stop")
            .and_then(parse_time)
            .map(|ms| ms / 1000),
        frequency: s.get(0, "dbine_frequency").and_then(interval_secs),
        days,
        enabled: yes(s.get(0, "tk_enable")),
        priority: int(s.get(0, "tk_priority")),
        next: s.get(0, "dbine_next").and_then(parse_ts),
        result_table: t("tk_result_table"),
        create: t("tk_create"),
        keep: s.get(0, "dbine_delete").and_then(interval_secs),
        thresholds,
    }
}

/// Task `id` (a tk_id), its row and whether its parameters could be read.
fn read(src: &mut dyn JobSource, id: &str) -> Result<(u64, Task, Set, bool)> {
    let n = int_id(id)?;
    let s = src.query(&task_sql(n), &[])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let name = s.get(0, "tk_name").unwrap_or_default().to_string();
    let (th, ok) = match src.query(THRESHOLDS, &[&name]) {
        Ok(th) => (th, true),
        Err(e) => {
            tracing::debug!("odbc scheduler: ph_threshold failed: {e}");
            (Set::default(), false)
        }
    };
    Ok((n, task_of(&s, &th), s, ok))
}

pub(super) fn values_of(t: &Task) -> BTreeMap<String, Value> {
    let days: Vec<&str> = DAYS
        .iter()
        .zip(t.days)
        .filter(|(_, on)| *on)
        .map(|((d, _), _)| *d)
        .collect();
    values(vec![
        ("name", json!(t.name)),
        ("type", json!(t.kind)),
        ("group", json!(t.group)),
        ("description", json!(t.description)),
        ("enabled", json!(t.enabled)),
        ("priority", t.priority.map_or(Value::Null, |p| json!(p))),
        ("database", json!(t.database)),
        ("execute", json!(t.execute)),
        (
            "start_time",
            t.start.map_or(Value::Null, |s| json!(time_text(s))),
        ),
        (
            "stop_time",
            t.stop.map_or(Value::Null, |s| json!(time_text(s))),
        ),
        ("frequency", t.frequency.map_or(Value::Null, |s| json!(s))),
        ("days", json!(days)),
        (
            "next_execution",
            t.next.map_or(Value::Null, |ms| json!(dt_value(ms))),
        ),
        ("result_table", jtext(t.result_table.as_deref())),
        ("create", jtext(t.create.as_deref())),
        ("keep", t.keep.map_or(Value::Null, |s| json!(s))),
    ])
}

fn threshold_rows(t: &Task) -> Vec<BTreeMap<String, Value>> {
    t.thresholds
        .iter()
        .map(|(n, (v, ty, d))| {
            values(vec![
                ("name", json!(n)),
                ("value", json!(v)),
                ("value_type", json!(ty)),
                ("description", json!(d)),
            ])
        })
        .collect()
}

pub(super) fn task_sections(
    existing: bool,
    thresholds: Vec<BTreeMap<String, Value>>,
    thresholds_ok: bool,
) -> Vec<FormSection> {
    let timed: &[&str] = &["TASK", "SENSOR"];
    let sensor: &[&str] = &["SENSOR", "STARTUP SENSOR"];
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Único en el planificador, hasta 36 caracteres."),
            FormField::new(
                "type",
                "Tipo",
                FormKind::select(&[
                    ("TASK", "Tarea"),
                    ("SENSOR", "Sensor"),
                    ("STARTUP TASK", "Tarea al arrancar"),
                    ("STARTUP SENSOR", "Sensor al arrancar"),
                ]),
            )
            .default(json!("TASK"))
            .help("Un sensor guarda lo que recoge en su tabla de resultados; los de arranque corren una vez, cuando arranca el servidor."),
            FormField::new("group", "Grupo", FormKind::reference("group", false))
                .default(json!("MISC")),
            FormField::new("description", "Descripción", FormKind::Textarea),
            FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
            FormField::new("priority", "Prioridad", FormKind::number(Some(0.0), None))
                .default(json!(0)),
        ]),
        FormSection::new("action", "Acción").fields(vec![
            FormField::new("database", "Base", FormKind::reference("database", false))
                .default(json!("sysadmin"))
                .help("La base en la que corre."),
            FormField::new("execute", "Ejecuta", FormKind::code("sql"))
                .required()
                .help("Sentencias SQL, o el nombre de una función o procedimiento (recibe el id de la tarea y el número de ejecución). En un sensor, $DATA_SEQ_ID es el número de la recolección."),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("start_time", "Desde", FormKind::Time)
                .when("type", timed)
                .help("La hora del día desde la que corre. Vacío: a cualquier hora."),
            FormField::new("stop_time", "Hasta", FormKind::Time)
                .when("type", timed)
                .help("La hora del día hasta la que corre. Vacío: sin límite."),
            FormField::new("frequency", "Cada", FormKind::Duration)
                .when("type", timed)
                .help("Cada cuánto corre, hasta 99 días. Vacío: una sola vez."),
            FormField::new(
                "days",
                "Días",
                FormKind::multi(&[
                    ("mon", "Lunes"),
                    ("tue", "Martes"),
                    ("wed", "Miércoles"),
                    ("thu", "Jueves"),
                    ("fri", "Viernes"),
                    ("sat", "Sábado"),
                    ("sun", "Domingo"),
                ]),
            )
            .when("type", timed)
            .default(json!(["mon", "tue", "wed", "thu", "fri", "sat", "sun"])),
            FormField::new("next_execution", "Próxima ejecución", FormKind::DateTime)
                .when("type", timed)
                .help("La calcula el planificador; cambiarla mueve la próxima ejecución."),
        ]),
        FormSection::new("sensor", "Sensor").fields(vec![
            FormField::new("result_table", "Tabla de resultados", FormKind::Text)
                .when("type", sensor)
                .help("La tabla de sysadmin donde el sensor guarda lo que recoge."),
            FormField::new("create", "Creación de la tabla", FormKind::code("sql"))
                .when("type", sensor)
                .help("El CREATE TABLE de la tabla de resultados; el planificador lo corre si la tabla no existe."),
            FormField::new("keep", "Conservar los datos", FormKind::Duration)
                .when("type", sensor)
                .help("Cuánto guarda lo recogido antes de borrarlo, hasta 99 días."),
        ]),
        FormSection::new("parameters", "Parámetros").list(
            FormList::new(
                "thresholds",
                "Parámetros (ph_threshold)",
                "name",
                vec![
                    FormField::new("name", "Nombre", FormKind::Text).required(),
                    FormField::new("value", "Valor", FormKind::Text),
                    FormField::new("value_type", "Tipo de valor", FormKind::Text)
                        .default(json!("STRING"))
                        .help("STRING o NUMERIC."),
                    FormField::new("description", "Descripción", FormKind::Textarea),
                ],
            )
            .rows(thresholds)
            .read_only(!thresholds_ok),
        ),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id de la tarea", FormKind::Text).read_only(true),
            FormField::new("total_executions", "Ejecuciones", FormKind::Text).read_only(true),
            FormField::new("total_time", "Tiempo total (s)", FormKind::Text).read_only(true),
            FormField::new("sequence", "Número de recolección", FormKind::Text).read_only(true),
            FormField::new("attributes", "Atributos", FormKind::Text).read_only(true),
        ]));
    }
    out
}

/// Groups and databases to pick from.
fn choices(src: &mut dyn JobSource) -> BTreeMap<String, Vec<(String, String)>> {
    let mut out = BTreeMap::new();
    let list = |src: &mut dyn JobSource, sql: &str, col: &str| -> Vec<(String, String)> {
        src.query(sql, &[])
            .map(|s| {
                (0..s.rows.len())
                    .filter_map(|r| s.get(r, col))
                    .map(|v| (v.to_string(), v.to_string()))
                    .collect()
            })
            .unwrap_or_else(|e| {
                tracing::debug!("odbc scheduler: {sql} failed: {e}");
                Vec::new()
            })
    };
    out.insert(
        "group".to_string(),
        list(
            src,
            "SELECT group_name FROM sysadmin:ph_group ORDER BY group_name",
            "group_name",
        ),
    );
    out.insert(
        "database".to_string(),
        list(
            src,
            "SELECT name FROM sysmaster:sysdatabases ORDER BY name",
            "name",
        ),
    );
    out
}

fn task_form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let choices = choices(src);
    let Some(id) = id else {
        let t = Task {
            kind: "TASK".into(),
            group: "MISC".into(),
            database: "sysadmin".into(),
            days: [true; 7],
            enabled: true,
            priority: Some(0),
            ..Task::default()
        };
        return Ok(ObjectForm {
            sections: task_sections(false, Vec::new(), true),
            values: values_of(&t),
            choices,
            ..ObjectForm::default()
        });
    };
    let (n, t, s, ok) = read(src, id)?;
    let mut v = values_of(&t);
    v.insert("id".into(), json!(n.to_string()));
    for (k, c) in [
        ("total_executions", "tk_total_executions"),
        ("total_time", "tk_total_time"),
        ("sequence", "tk_sequence"),
        ("attributes", "tk_attributes"),
    ] {
        v.insert(k.into(), jtext(s.get(0, c)));
    }
    let mut notes = Vec::new();
    if built_in(&t.name) {
        notes.push(BUILT_IN.to_string());
    }
    Ok(ObjectForm {
        sections: task_sections(true, threshold_rows(&t), ok),
        values: v,
        choices,
        notes,
        read_only: false,
    })
}

/// A name in a select list of the server's, when the list could be read.
fn picked(
    name: String,
    list: Option<&Vec<(String, String)>>,
    missing: impl FnOnce(&str) -> String,
) -> Result<String> {
    match list.filter(|l| !l.is_empty()) {
        None => Ok(name),
        Some(l) => l
            .iter()
            .find(|(v, _)| *v == name)
            .or_else(|| l.iter().find(|(v, _)| v.eq_ignore_ascii_case(&name)))
            .map(|(v, _)| v.clone())
            .ok_or_else(|| bad(missing(&short(&name)))),
    }
}

/// The task the form describes, checked. `choices`: the groups and
/// databases the server has.
pub(super) fn from_values(
    values: &FormValues,
    choices: &BTreeMap<String, Vec<(String, String)>>,
) -> Result<Task> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre", 36)?;
    let kind = choice(v, "type", "Tipo", TYPES, "TASK")?;
    let group = picked(
        optional(v, "group", "Grupo", 128)?.unwrap_or_else(|| "MISC".into()),
        choices.get("group"),
        |g| format!("No existe el grupo «{g}»."),
    )?;
    let description = code_text(v, "description", "Descripción", 2048)?.unwrap_or_default();
    let database = picked(
        optional(v, "database", "Base", 128)?.unwrap_or_else(|| "sysadmin".into()),
        choices.get("database"),
        |d| format!("No existe la base «{d}»."),
    )?;
    let execute =
        code_text(v, "execute", "Ejecuta", 32_739)?.ok_or_else(|| bad("Falta «Ejecuta»."))?;
    let timed = matches!(kind.as_str(), "TASK" | "SENSOR");
    let sensor = kind.ends_with("SENSOR");
    let mut t = Task {
        name,
        kind,
        group,
        description,
        database,
        execute,
        enabled: v.bool_or("enabled", true),
        priority: v.int("priority", "Prioridad", 0, i64::from(i32::MAX))?,
        days: [true; 7],
        ..Task::default()
    };
    if timed {
        t.start = time(v, "start_time", "Desde")?;
        t.stop = time(v, "stop_time", "Hasta")?;
        t.frequency = v.int("frequency", "Cada", 1, MAX_INTERVAL)?;
        if v.0.contains_key("days") {
            let picked = v.strings("days");
            for p in &picked {
                if !DAYS.iter().any(|(d, _)| d == p) {
                    return Err(bad(format!("«{}» no es un día de la semana.", short(p))));
                }
            }
            for (i, (d, _)) in DAYS.iter().enumerate() {
                t.days[i] = picked.iter().any(|p| p == d);
            }
        }
        t.next = datetime(v, "next_execution", "Próxima ejecución")?;
    }
    if sensor {
        t.result_table = optional(v, "result_table", "Tabla de resultados", 128)?;
        t.create = code_text(v, "create", "Creación de la tabla", 32_739)?;
        t.keep = v.int("keep", "Conservar los datos", 0, MAX_INTERVAL)?;
    }
    for row in rows(values, "thresholds") {
        let n = required(row, "name", "Nombre del parámetro", 30)?;
        let value = code_text(row, "value", "Valor del parámetro", 2048)?.unwrap_or_default();
        let ty = optional(row, "value_type", "Tipo de valor", 30)?
            .unwrap_or_else(|| "STRING".into())
            .to_ascii_uppercase();
        let d =
            code_text(row, "description", "Descripción del parámetro", 2048)?.unwrap_or_default();
        if t.thresholds.insert(n.clone(), (value, ty, d)).is_some() {
            return Err(bad(format!("El parámetro «{}» está repetido.", short(&n))));
        }
    }
    Ok(t)
}

/// Whether any of the literals has a line break (Informix needs
/// IFX_ALLOW_NEWLINE for it).
fn newline(cols: &[(&str, String)]) -> bool {
    cols.iter().any(|(_, v)| v.contains('\n'))
}

fn task_columns(t: &Task) -> Vec<(&'static str, String)> {
    let mut cols: Vec<(&'static str, String)> = vec![
        ("tk_name", lit(&t.name)),
        ("tk_type", lit(&t.kind)),
        ("tk_group", lit(&t.group)),
        ("tk_description", lit(&t.description)),
        ("tk_dbs", lit(&t.database)),
        ("tk_execute", lit(&t.execute)),
        ("tk_start_time", time_literal(t.start)),
        ("tk_stop_time", time_literal(t.stop)),
        (
            "tk_frequency",
            t.frequency
                .map(interval_literal)
                .unwrap_or_else(|| "NULL".into()),
        ),
    ];
    for (i, (_, c)) in DAYS.iter().enumerate() {
        cols.push((c, bool_literal(t.days[i])));
    }
    cols.push(("tk_enable", bool_literal(t.enabled)));
    cols.push(("tk_priority", t.priority.unwrap_or(0).to_string()));
    if t.next.is_some() {
        cols.push(("tk_next_execution", datetime_literal(t.next)));
    }
    cols.push(("tk_result_table", text_literal(t.result_table.as_deref())));
    cols.push(("tk_create", text_literal(t.create.as_deref())));
    if let Some(k) = t.keep {
        cols.push(("tk_delete", interval_literal(k)));
    }
    cols
}

fn allow_newlines(plan: &mut Plan) {
    plan.exec("EXECUTE PROCEDURE IFX_ALLOW_NEWLINE('t')".into());
}

/// The parameters' changes for task `task`: deleted, changed, added.
fn thresholds(
    plan: &mut Plan,
    task: &str,
    want: &BTreeMap<String, (String, String, String)>,
    have: &BTreeMap<String, (String, String, String)>,
) {
    for n in have.keys().filter(|n| !want.contains_key(*n)) {
        plan.exec(format!(
            "DELETE FROM sysadmin:ph_threshold WHERE task_name = {} AND name = {}",
            lit(task),
            lit(n)
        ));
    }
    for (n, (v, ty, d)) in want {
        match have.get(n) {
            Some(h) if *h == (v.clone(), ty.clone(), d.clone()) => {}
            Some(_) => plan.exec(format!(
                "UPDATE sysadmin:ph_threshold SET value = {}, value_type = {}, description = {} WHERE task_name = {} AND name = {}",
                lit(v),
                lit(ty),
                lit(d),
                lit(task),
                lit(n)
            )),
            None => plan.exec(format!(
                "INSERT INTO sysadmin:ph_threshold (name, task_name, value, value_type, description) VALUES ({}, {}, {}, {}, {})",
                lit(n),
                lit(task),
                lit(v),
                lit(ty),
                lit(d)
            )),
        }
    }
}

fn has_newline_values(t: &Task) -> bool {
    newline(&task_columns(t))
        || t.thresholds
            .iter()
            .any(|(n, (v, ty, d))| [n, v, ty, d].iter().any(|x| x.contains('\n')))
}

pub(super) fn create_plan(t: &Task) -> Plan {
    let mut p = Plan::new(&format!(
        "Tarea «{}» del planificador de Informix (sysadmin)",
        t.name
    ));
    p.atomic = true;
    if has_newline_values(t) {
        allow_newlines(&mut p);
    }
    let cols = task_columns(t);
    p.exec(format!(
        "INSERT INTO sysadmin:ph_task (\n    {}\n) VALUES (\n    {}\n)",
        cols.iter()
            .map(|(c, _)| *c)
            .collect::<Vec<_>>()
            .join(",\n    "),
        cols.iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(",\n    ")
    ));
    thresholds(&mut p, &t.name, &t.thresholds, &BTreeMap::new());
    let name = t.name.clone();
    p.lookup = Some(Box::new(move |src: &mut dyn JobSource| {
        let s = src.query(
            "SELECT MAX(tk_id) AS tk_id FROM sysadmin:ph_task WHERE tk_name = ?",
            &[&name],
        )?;
        Ok(int(s.get(0, "tk_id")).map(|n| n.to_string()))
    }));
    p
}

/// `UPDATE ph_task` with the columns that changed, the parameters' diff,
/// or nothing.
pub(super) fn alter_plan(id: u64, cur: &Task, t: &Task) -> Plan {
    // The fields the form hides for this type keep what the server has.
    let mut t = t.clone();
    if !matches!(t.kind.as_str(), "TASK" | "SENSOR") {
        t.start = cur.start;
        t.stop = cur.stop;
        t.frequency = cur.frequency;
        t.days = cur.days;
        t.next = cur.next;
    } else if t.next.is_none() {
        // The planner computes it: an emptied field leaves it alone.
        t.next = cur.next;
    }
    if !t.kind.ends_with("SENSOR") {
        t.result_table.clone_from(&cur.result_table);
        t.create.clone_from(&cur.create);
        t.keep = cur.keep;
    }
    let t = &t;
    let want = task_columns(t);
    let have = task_columns(cur);
    let mut sets: Vec<String> = Vec::new();
    for (c, v) in &want {
        let old = have.iter().find(|(h, _)| h == c).map(|(_, v)| v.as_str());
        if old != Some(v.as_str()) {
            sets.push(format!("{c} = {v}"));
        }
    }
    // A sensor's retention cleared: NULL.
    if cur.keep.is_some() && t.keep.is_none() {
        sets.push("tk_delete = NULL".into());
    }
    let renamed = t.name != cur.name;
    if sets.is_empty() && t.thresholds == cur.thresholds {
        return Plan::nothing(&cur.name, id.to_string());
    }
    let mut p = Plan::new(&format!(
        "Tarea «{}» del planificador de Informix (sysadmin)",
        cur.name
    ));
    p.atomic = true;
    p.id = Some(id.to_string());
    if has_newline_values(t) {
        allow_newlines(&mut p);
    }
    if !sets.is_empty() {
        p.exec(format!(
            "UPDATE sysadmin:ph_task SET\n    {}\n WHERE tk_id = {id}",
            sets.join(",\n    ")
        ));
    }
    if renamed && !cur.thresholds.is_empty() {
        p.exec(format!(
            "UPDATE sysadmin:ph_threshold SET task_name = {} WHERE task_name = {}",
            lit(&t.name),
            lit(&cur.name)
        ));
    }
    thresholds(&mut p, &t.name, &t.thresholds, &cur.thresholds);
    p
}

/// Another task already has `name`.
fn name_taken(src: &mut dyn JobSource, name: &str, except: Option<u64>) -> bool {
    src.query(
        "SELECT tk_id FROM sysadmin:ph_task WHERE tk_name = ?",
        &[name],
    )
    .map(|s| (0..s.rows.len()).any(|r| int(s.get(r, "tk_id")).map(|n| n as u64) != except))
    .unwrap_or(false)
}

fn task_plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let choices = choices(src);
    let t = from_values(values, &choices)?;
    match id {
        None => {
            if name_taken(src, &t.name, None) {
                return Err(bad(format!("Ya hay una tarea «{}».", short(&t.name))));
            }
            Ok(create_plan(&t))
        }
        Some(id) => {
            let (n, cur, _, _) = read(src, id)?;
            if t.name != cur.name && name_taken(src, &t.name, Some(n)) {
                return Err(bad(format!("Ya hay una tarea «{}».", short(&t.name))));
            }
            Ok(alter_plan(n, &cur, &t))
        }
    }
}

pub(super) fn drop_task_plan(id: u64, name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra la tarea «{name}» del planificador de Informix, con sus parámetros y sus ejecuciones (las alertas quedan)"
    ));
    p.atomic = true;
    p.exec(format!(
        "DELETE FROM sysadmin:ph_threshold WHERE task_name = {}",
        lit(name)
    ));
    p.exec(format!(
        "DELETE FROM sysadmin:ph_run WHERE run_task_id = {id}"
    ));
    p.exec(format!("DELETE FROM sysadmin:ph_task WHERE tk_id = {id}"));
    p
}

pub(super) fn clear_history_plan(id: u64, name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra el historial de ejecuciones (ph_run) de la tarea «{name}»; sus alertas quedan"
    ));
    p.exec(format!(
        "DELETE FROM sysadmin:ph_run WHERE run_task_id = {id}"
    ));
    p
}

pub(super) fn clear_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (n, t, _, _) = read(src, id)?;
    Ok(clear_history_plan(n, &t.name))
}

// ----------------------------------------------------------------- groups

/// A group's name as an id: any text without control characters, of a
/// sane length. It's only ever bound or a literal.
fn group_id(id: &str) -> Result<String> {
    let t = id.trim();
    let ok = !t.is_empty() && t.chars().count() <= 128 && !t.chars().any(char::is_control);
    ok.then(|| t.to_string())
        .ok_or_else(|| bad(format!("«{}» no es un grupo del planificador.", short(id))))
}

pub(super) fn group_items(src: &mut dyn JobSource) -> Result<SchedulerList> {
    let s = match src.query("SELECT * FROM sysadmin:ph_group ORDER BY group_name", &[]) {
        Ok(s) => s,
        Err(e) => {
            return match note(&e) {
                Some(n) => Ok(SchedulerList {
                    items: Vec::new(),
                    notes: vec![n.to_string()],
                }),
                None => Err(e),
            }
        }
    };
    Ok(SchedulerList {
        items: (0..s.rows.len())
            .filter_map(|r| {
                let name = s.get(r, "group_name")?.to_string();
                Some(SchedulerItem {
                    id: name.clone(),
                    name,
                    description: s.get(r, "group_description").map(str::to_string),
                    enabled: None,
                    system: false,
                })
            })
            .collect(),
        notes: Vec::new(),
    })
}

/// Group `id`: its name and description.
fn read_group(src: &mut dyn JobSource, id: &str) -> Result<(String, String)> {
    let name = group_id(id)?;
    let s = src.query(
        "SELECT * FROM sysadmin:ph_group WHERE group_name = ?",
        &[&name],
    )?;
    let found = s
        .get(0, "group_name")
        .ok_or_else(|| bad(format!("No existe el grupo «{}».", short(&name))))?;
    Ok((
        found.to_string(),
        s.get(0, "group_description")
            .unwrap_or_default()
            .to_string(),
    ))
}

fn group_tasks(src: &mut dyn JobSource, name: &str) -> Vec<String> {
    src.query(
        "SELECT tk_name FROM sysadmin:ph_task WHERE tk_group = ? ORDER BY tk_name",
        &[name],
    )
    .map(|s| {
        (0..s.rows.len())
            .filter_map(|r| s.get(r, "tk_name").map(str::to_string))
            .collect()
    })
    .unwrap_or_default()
}

pub(super) fn group_sections(tasks: Vec<String>) -> Vec<FormSection> {
    vec![FormSection::new("general", "General")
        .fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Cambiarlo pasa sus tareas al nombre nuevo."),
            FormField::new("description", "Descripción", FormKind::Textarea),
        ])
        .list(
            FormList::new(
                "tasks",
                "Tareas del grupo",
                "task",
                vec![FormField::new("task", "Tarea", FormKind::Text).read_only(true)],
            )
            .rows(
                tasks
                    .into_iter()
                    .map(|t| BTreeMap::from([("task".to_string(), json!(t))]))
                    .collect(),
            )
            .read_only(true),
        )]
}

fn group_form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let (name, description, tasks) = match id {
        None => (String::new(), String::new(), Vec::new()),
        Some(id) => {
            let (n, d) = read_group(src, id)?;
            let tasks = group_tasks(src, &n);
            (n, d, tasks)
        }
    };
    Ok(ObjectForm {
        sections: group_sections(tasks),
        values: values(vec![
            ("name", json!(name)),
            ("description", json!(description)),
        ]),
        ..ObjectForm::default()
    })
}

pub(super) fn create_group_plan(name: &str, description: &str) -> Plan {
    let mut p = Plan::new(&format!("Grupo «{name}» del planificador de Informix"));
    if description.contains('\n') {
        allow_newlines(&mut p);
    }
    p.exec(format!(
        "INSERT INTO sysadmin:ph_group (group_name, group_description) VALUES ({}, {})",
        lit(name),
        lit(description)
    ));
    p.id = Some(name.to_string());
    p
}

/// A description changed in place; a new name as a new group that takes
/// the old one's tasks (ph_task refers to it by name).
pub(super) fn alter_group_plan(cur: (&str, &str), name: &str, description: &str) -> Plan {
    if name == cur.0 && description == cur.1 {
        return Plan::nothing(cur.0, cur.0.to_string());
    }
    let mut p = Plan::new(&format!("Grupo «{}» del planificador de Informix", cur.0));
    p.atomic = true;
    p.id = Some(name.to_string());
    if description.contains('\n') {
        allow_newlines(&mut p);
    }
    if name == cur.0 {
        p.exec(format!(
            "UPDATE sysadmin:ph_group SET group_description = {} WHERE group_name = {}",
            lit(description),
            lit(cur.0)
        ));
        return p;
    }
    p.comment(
        "Las tareas nombran su grupo: se crea el grupo nuevo, se le pasan y se borra el viejo",
    );
    p.exec(format!(
        "INSERT INTO sysadmin:ph_group (group_name, group_description) VALUES ({}, {})",
        lit(name),
        lit(description)
    ));
    p.exec(format!(
        "UPDATE sysadmin:ph_task SET tk_group = {} WHERE tk_group = {}",
        lit(name),
        lit(cur.0)
    ));
    p.exec(format!(
        "DELETE FROM sysadmin:ph_group WHERE group_name = {}",
        lit(cur.0)
    ));
    p
}

fn group_plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre", 128)?;
    let description = code_text(v, "description", "Descripción", 2048)?.unwrap_or_default();
    let exists = |src: &mut dyn JobSource, n: &str| read_group(src, n).is_ok();
    match id {
        None => {
            if exists(src, &name) {
                return Err(bad(format!("Ya hay un grupo «{}».", short(&name))));
            }
            Ok(create_group_plan(&name, &description))
        }
        Some(id) => {
            let (n, d) = read_group(src, id)?;
            if name != n && exists(src, &name) {
                return Err(bad(format!("Ya hay un grupo «{}».", short(&name))));
            }
            Ok(alter_group_plan((&n, &d), &name, &description))
        }
    }
}

pub(super) fn drop_group_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra el grupo «{name}» del planificador de Informix"
    ));
    p.exec(format!(
        "DELETE FROM sysadmin:ph_group WHERE group_name = {}",
        lit(name)
    ));
    p
}

// ------------------------------------------------------------- dispatch

pub(super) fn form(
    src: &mut dyn JobSource,
    _ctx: &Ctx,
    kind: Kind,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let mut f = match kind {
        Kind::Task => task_form(src, id)?,
        Kind::Group => group_form(src, id)?,
    };
    // The login can't write the scheduler's tables: only read, and why.
    if may_write(src) == Some(false) {
        f.read_only = true;
        f.notes.insert(0, PRIVILEGES.to_string());
    }
    Ok(f)
}

pub(super) fn plan(
    src: &mut dyn JobSource,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    match kind {
        Kind::Task => task_plan(src, id, values),
        Kind::Group => group_plan(src, id, values),
    }
}

pub(super) fn delete_plan(src: &mut dyn JobSource, kind: Kind, id: &str) -> Result<Plan> {
    match kind {
        Kind::Task => {
            let (n, t, _, _) = read(src, id)?;
            Ok(drop_task_plan(n, &t.name))
        }
        Kind::Group => {
            let (name, _) = read_group(src, id)?;
            let used = group_tasks(src, &name);
            if !used.is_empty() {
                return Err(bad(format!(
                    "El grupo «{}» tiene tareas ({}): pasalas a otro grupo antes de borrarlo.",
                    short(&name),
                    used.len()
                )));
            }
            Ok(drop_group_plan(&name))
        }
    }
}

// ------------------------------------------------------------------- log

pub(super) const ALERTS_SQL: &str =
    "SELECT FIRST 500 a.*, CAST(a.alert_time AS CHAR(19)) AS dbine_time, t.tk_name AS dbine_task
  FROM sysadmin:ph_alert a LEFT OUTER JOIN sysadmin:ph_task t ON t.tk_id = a.alert_task_id
 ORDER BY a.alert_time DESC";
pub(super) const RUNS_SQL: &str = "SELECT FIRST 500 r.run_id, CAST(r.run_time AS CHAR(19)) AS dbine_time, r.run_retcode, r.run_duration, t.tk_name AS dbine_task
  FROM sysadmin:ph_run r LEFT OUTER JOIN sysadmin:ph_task t ON t.tk_id = r.run_task_id
 ORDER BY r.run_id DESC";

pub(super) fn logs(src: &mut dyn JobSource) -> Result<Vec<MonitorTable>> {
    let alerts = src.query(ALERTS_SQL, &[])?;
    let mut a = MonitorTable::new(
        "ph_alert",
        "Alertas del planificador",
        &[
            "Momento", "Tarea", "Tipo", "Color", "Estado", "Objeto", "Mensaje", "Acción",
        ],
    );
    for r in 0..alerts.rows.len() {
        let object = [
            alerts.get(r, "alert_object_type"),
            alerts.get(r, "alert_object_name"),
        ]
        .into_iter()
        .flatten()
        .collect::<Vec<_>>()
        .join(" ");
        a.rows.push(vec![
            jtext(alerts.get(r, "dbine_time")),
            jtext(alerts.get(r, "dbine_task")),
            jtext(alerts.get(r, "alert_type")),
            jtext(alerts.get(r, "alert_color")),
            jtext(alerts.get(r, "alert_state")),
            jtext(Some(object.as_str()).filter(|o| !o.is_empty())),
            jtext(alerts.get(r, "alert_message")),
            jtext(alerts.get(r, "alert_action")),
        ]);
    }
    let mut out = vec![a];
    match src.query(RUNS_SQL, &[]) {
        Ok(runs) => {
            let mut t = MonitorTable::new(
                "ph_run",
                "Ejecuciones recientes",
                &["Momento", "Tarea", "Código de retorno", "Duración (s)"],
            );
            for r in 0..runs.rows.len() {
                t.rows.push(vec![
                    jtext(runs.get(r, "dbine_time")),
                    jtext(runs.get(r, "dbine_task")),
                    jtext(runs.get(r, "run_retcode")),
                    jtext(runs.get(r, "run_duration")),
                ]);
            }
            out.push(t);
        }
        Err(e) => tracing::debug!("odbc scheduler: ph_run failed: {e}"),
    }
    Ok(out)
}
