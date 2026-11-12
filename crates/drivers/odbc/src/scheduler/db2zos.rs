//! Db2 for z/OS's administrative task scheduler, managed whole: a task
//! runs a stored procedure or a JCL job, once, every so many minutes, on a
//! UNIX cron schedule, or when another task (or Db2 starting or stopping)
//! ends, optionally only for some return code; between a begin and an end
//! time, up to a number of runs, as a user and on a data sharing member.
//! `SYSPROC.ADMIN_TASK_ADD` creates it, `ADMIN_TASK_UPDATE` changes its
//! schedule, user, member and description in place, and
//! `ADMIN_TASK_REMOVE` removes it. Each procedure answers a return code and
//! a message as output parameters (12: refused), read after the call.
//! ADD's task name is an input-output parameter (a constant there is
//! SQLCODE -469): it is bound, and what comes back is the new task's id.
//!
//! The name and the work (procedure or JCL) can't be updated: a task with
//! another name is added and then the old one removed (two calls the
//! scheduler can't make atomic; a failed add leaves the old task as it
//! was), and its status starts over. The work can't change under the same
//! name: removing first could leave the user without the task. The
//! password is never read back (the list has only the user) and Db2 wants
//! it whenever there is a user, so it is typed again on every save; it
//! goes as a bound parameter, never in the statement's text (which Db2
//! keeps in its dynamic statement cache), and the script shows it masked.
//! `DSNADM.ADMIN_TASK_STATUS()` is the log; the scheduler has no call that
//! clears a task's status.

use super::*;
use crate::jobs::db2::{NOW, NO_READ, NO_ZOS_ATS, NO_ZOS_STATUS, ZOS_LIST, ZOS_STATUS};
use crate::jobs::{code, int, lit, not_found, parse_ts, Tz};
use dbine_driver::{FormField, FormKind, FormSection, SchedulerKind, SECRET_MASK};
use serde_json::json;

pub(super) const NO_CLEAR: &str = "el planificador de Db2 for z/OS no tiene una llamada que borre el estado de sus tareas: se va con la tarea";
const RECREATE: &str = "Db2 for z/OS no cambia el nombre ni el trabajo (procedimiento o JCL) de una tarea: con otro nombre se crea la tarea nueva y después se borra la anterior, en dos llamadas que el planificador no hace en una transacción (si la creación falla, la anterior queda como estaba). Su estado vuelve a empezar.";
const SAME_NAME_WORK: &str = "Para cambiar el trabajo (procedimiento o JCL) cambiá también el «Nombre»: Db2 for z/OS no lo cambia en una tarea, así que se crea una nueva antes de borrar esta, y con el mismo nombre habría que borrarla primero y podrías quedarte sin ella.";
const PASSWORD_NOTE: &str = "La contraseña no se lee del servidor y Db2 la pide siempre que la tarea corre con un usuario: para guardar cualquier cambio hay que escribirla de nuevo.";
const PASSWORD_NEEDED: &str = "Con un «Usuario» hace falta su «Contraseña»: Db2 la pide al crear la tarea y en cada cambio, y no se lee del servidor, así que hay que escribirla de nuevo.";
/// ADMIN_TASK_ADD's and UPDATE's password is a VARCHAR(24).
const PASSWORD_MAX: usize = 24;
const BEGIN_PAST: &str =
    "«Inicio» ya pasó: Db2 no acepta un inicio en el pasado. Dejalo vacío para que corra desde ya.";
const END_PAST: &str =
    "«Fin» ya pasó: Db2 no acepta un fin en el pasado. Poné uno futuro o dejalo vacío.";

const TRIGGERS: &[&str] = &["once", "interval", "cron", "task", "db2start", "db2stop"];
const CONDS: &[&str] = &["GT", "GE", "EQ", "LT", "LE", "NE"];
const WAITS: &[&str] = &["NO", "YES", "PURGE"];

/// A task as `DSNADM.ADMIN_TASK_LIST()` has it (times in the server's
/// local time, ms).
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Task {
    pub(super) name: String,
    pub(super) description: Option<String>,
    pub(super) begin: Option<i64>,
    pub(super) end: Option<i64>,
    pub(super) max: Option<i64>,
    /// One of [`TRIGGERS`].
    pub(super) trigger: String,
    pub(super) interval: Option<i64>,
    pub(super) point: Option<String>,
    pub(super) trigger_task: Option<String>,
    pub(super) cond: Option<String>,
    pub(super) code: Option<i64>,
    pub(super) ssid: Option<String>,
    /// "procedure" or "jcl".
    pub(super) action: String,
    pub(super) schema: Option<String>,
    pub(super) procedure: Option<String>,
    pub(super) input: Option<String>,
    pub(super) jcl_library: Option<String>,
    pub(super) jcl_member: Option<String>,
    pub(super) job_wait: Option<String>,
    pub(super) user: Option<String>,
}

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

/// The scheduler isn't there or can't be read: why.
fn note(e: &Error) -> Option<String> {
    match code(e) {
        Some(-440 | -204 | -4700) => Some(NO_ZOS_ATS.into()),
        Some(-551 | -552) => Some(NO_READ.into()),
        Some(-471 | -443) => Some(format!(
            "El planificador de tareas administrativas no respondió: {}",
            crate::jobs::clip(&e.to_string(), 300)
        )),
        _ => None,
    }
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    if let Err(e) = src.query(ZOS_LIST, &[]) {
        return match note(&e) {
            Some(n) => Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![n],
            }),
            None => Err(e),
        };
    }
    let status = src.query(ZOS_STATUS, &[]).is_ok();
    let mut notes = vec![PASSWORD_NOTE.to_string()];
    if !status {
        notes.push(NO_ZOS_STATUS.to_string());
    }
    Ok(SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Tareas", "tarea").abilities(true, true, true)],
        logs: status,
        notes,
    })
}

pub(super) fn task_of(s: &Set, r: usize) -> Task {
    let t = |c: &str| s.get(r, c).map(str::to_string);
    let trigger_task = t("TRIGGER_TASK_NAME");
    let trigger = if t("POINT_IN_TIME").is_some() {
        "cron"
    } else if int(s.get(r, "INTERVAL")).is_some() {
        "interval"
    } else {
        match trigger_task
            .as_deref()
            .map(str::to_ascii_uppercase)
            .as_deref()
        {
            Some("DB2START") => "db2start",
            Some("DB2STOP") => "db2stop",
            Some(_) => "task",
            None => "once",
        }
    };
    let jcl = t("JCL_LIBRARY").is_some() && t("PROCEDURE_NAME").is_none();
    Task {
        name: t("TASK_NAME").unwrap_or_default(),
        description: t("DESCRIPTION"),
        begin: s.get(r, "BEGIN_TIMESTAMP").and_then(parse_ts),
        end: s.get(r, "END_TIMESTAMP").and_then(parse_ts),
        max: int(s.get(r, "MAX_INVOCATIONS")),
        trigger: trigger.into(),
        interval: int(s.get(r, "INTERVAL")),
        point: t("POINT_IN_TIME"),
        trigger_task: trigger_task.filter(|_| trigger == "task"),
        cond: t("TRIGGER_TASK_COND"),
        code: int(s.get(r, "TRIGGER_TASK_CODE")),
        ssid: t("DB2_SSID"),
        action: if jcl { "jcl" } else { "procedure" }.into(),
        schema: t("PROCEDURE_SCHEMA"),
        procedure: t("PROCEDURE_NAME"),
        input: t("PROCEDURE_INPUT"),
        jcl_library: t("JCL_LIBRARY"),
        jcl_member: t("JCL_MEMBER"),
        job_wait: t("JOB_WAIT"),
        user: t("USERID"),
    }
}

/// A task name as the app sends it back: up to 128 characters, none of
/// them control characters.
fn task_name(id: &str) -> Result<String> {
    let t = id.trim();
    let ok = !t.is_empty() && t.chars().count() <= 128 && !t.chars().any(char::is_control);
    ok.then(|| t.to_string())
        .ok_or_else(|| crate::jobs::bad_id(id))
}

/// Every task's row, and the one named `id`.
fn read(src: &mut dyn JobSource, id: &str) -> Result<(Task, Set, usize)> {
    let name = task_name(id)?;
    let s = src.query(ZOS_LIST, &[])?;
    let r = (0..s.rows.len())
        .find(|r| s.get(*r, "TASK_NAME") == Some(name.as_str()))
        .ok_or_else(|| not_found(id))?;
    Ok((task_of(&s, r), s, r))
}

fn dt(ms: Option<i64>) -> Value {
    ms.map_or(Value::Null, |ms| json!(dt_value(ms)))
}

pub(super) fn values_of(t: &Task) -> BTreeMap<String, Value> {
    values(vec![
        ("task_name", json!(t.name)),
        ("description", jtext(t.description.as_deref())),
        ("action", json!(t.action)),
        ("procedure_schema", jtext(t.schema.as_deref())),
        ("procedure_name", jtext(t.procedure.as_deref())),
        ("procedure_input", jtext(t.input.as_deref())),
        ("jcl_library", jtext(t.jcl_library.as_deref())),
        ("jcl_member", jtext(t.jcl_member.as_deref())),
        ("job_wait", json!(t.job_wait.as_deref().unwrap_or("NO"))),
        ("trigger", json!(t.trigger)),
        ("interval", t.interval.map_or(Value::Null, |n| json!(n))),
        ("point_in_time", jtext(t.point.as_deref())),
        ("trigger_task_name", jtext(t.trigger_task.as_deref())),
        ("trigger_task_cond", json!(t.cond.as_deref().unwrap_or(""))),
        (
            "trigger_task_code",
            t.code.map_or(Value::Null, |n| json!(n)),
        ),
        ("begin", dt(t.begin)),
        ("end", dt(t.end)),
        ("max_invocations", t.max.map_or(Value::Null, |n| json!(n))),
        ("user_id", jtext(t.user.as_deref())),
        ("password", Value::Null),
        ("db2_ssid", jtext(t.ssid.as_deref())),
    ])
}

pub(super) fn sections(existing: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("task_name", "Nombre", FormKind::Text)
                .required()
                .help("Único en el planificador, hasta 128 caracteres. Cambiarlo vuelve a crear la tarea."),
            FormField::new("description", "Descripción", FormKind::Text)
                .help("Hasta 128 caracteres."),
        ]),
        FormSection::new("work", "Trabajo").fields(vec![
            FormField::new(
                "action",
                "Ejecuta",
                FormKind::select(&[
                    ("procedure", "Un procedimiento almacenado"),
                    ("jcl", "Un job JCL"),
                ]),
            )
            .default(json!("procedure")),
            FormField::new("procedure_schema", "Esquema", FormKind::Text)
                .when("action", &["procedure"])
                .default(json!("SYSPROC")),
            FormField::new("procedure_name", "Procedimiento", FormKind::Text)
                .when("action", &["procedure"]),
            FormField::new(
                "procedure_input",
                "Consulta de parámetros",
                FormKind::code("sql"),
            )
            .when("action", &["procedure"])
            .help("Un SELECT que devuelve una fila con los argumentos de entrada del procedimiento. Vacío: no recibe argumentos."),
            FormField::new("jcl_library", "Biblioteca JCL", FormKind::Text)
                .when("action", &["jcl"])
                .help("El data set particionado que tiene el JCL, hasta 44 caracteres.")
                .placeholder("SYS1.PROCLIB"),
            FormField::new("jcl_member", "Miembro JCL", FormKind::Text)
                .when("action", &["jcl"])
                .help("Hasta 8 caracteres."),
            FormField::new(
                "job_wait",
                "Espera al job",
                FormKind::select(&[
                    ("NO", "No: lo envía y sigue"),
                    ("YES", "Sí: espera a que termine"),
                    ("PURGE", "Sí, y después lo borra de la cola"),
                ]),
            )
            .when("action", &["jcl"])
            .default(json!("NO")),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new(
                "trigger",
                "Se ejecuta",
                FormKind::select(&[
                    ("once", "Una vez"),
                    ("interval", "Cada tantos minutos"),
                    ("cron", "Según un calendario cron"),
                    ("task", "Al terminar otra tarea"),
                    ("db2start", "Cuando arranca Db2"),
                    ("db2stop", "Cuando se detiene Db2"),
                ]),
            )
            .default(json!("once")),
            FormField::new(
                "interval",
                "Cada (minutos)",
                FormKind::number(Some(1.0), Some(f64::from(i32::MAX))),
            )
            .when("trigger", &["interval"]),
            FormField::new("point_in_time", "Calendario", FormKind::Cron)
                .when("trigger", &["cron"])
                .help("Formato cron de UNIX: minuto, hora, día del mes, mes y día de la semana.")
                .placeholder("0 3 * * *"),
            FormField::new(
                "trigger_task_name",
                "Tarea que la dispara",
                FormKind::reference("task", false),
            )
            .when("trigger", &["task"]),
            FormField::new(
                "trigger_task_cond",
                "Condición sobre su código de retorno",
                FormKind::select(&[
                    ("", "Cualquier resultado"),
                    ("GT", "Mayor que"),
                    ("GE", "Mayor o igual que"),
                    ("EQ", "Igual a"),
                    ("LT", "Menor que"),
                    ("LE", "Menor o igual que"),
                    ("NE", "Distinto de"),
                ]),
            )
            .when("trigger", &["task"])
            .help("Para un procedimiento se compara su SQLCODE; para un JCL, el código de retorno más alto."),
            FormField::new("trigger_task_code", "Código", FormKind::number(None, None))
                .when("trigger", &["task"]),
            FormField::new("begin", "Inicio", FormKind::DateTime).help(
                "La primera vez que puede correr, en la hora del servidor. Vacío: desde ya.",
            ),
            FormField::new("end", "Fin", FormKind::DateTime)
                .help("La última vez que puede empezar. Vacío: sin fin."),
            FormField::new(
                "max_invocations",
                "Máximo de ejecuciones",
                FormKind::number(Some(0.0), Some(f64::from(i32::MAX))),
            )
            .help("Vacío: sin límite."),
        ]),
        FormSection::new("execution", "Ejecución").fields(vec![
            FormField::new("user_id", "Usuario", FormKind::Text)
                .help("El usuario con el que corre la tarea. Vacío: el usuario de ejecución por omisión del planificador."),
            FormField::new("password", "Contraseña", FormKind::Secret)
                .help("La del «Usuario», hasta 24 caracteres. No se lee del servidor: con un usuario, Db2 la pide al crear la tarea y en cada cambio."),
            FormField::new("db2_ssid", "Miembro (data sharing)", FormKind::Text)
                .help("El subsistema que la ejecuta, hasta 4 caracteres. Vacío: cualquier miembro."),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("creator", "Creada por", FormKind::Text).read_only(true),
            FormField::new("last_modified", "Modificada", FormKind::Text).read_only(true),
            FormField::new("status", "Último estado", FormKind::Text).read_only(true),
            FormField::new("invocations", "Ejecuciones", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let all = match id {
        Some(_) => None,
        None => Some(src.query(ZOS_LIST, &[]).unwrap_or_default()),
    };
    let (t, s, r) = match id {
        Some(id) => {
            let (t, s, r) = read(src, id)?;
            (t, s, Some(r))
        }
        None => (
            Task {
                trigger: "once".into(),
                action: "procedure".into(),
                schema: Some("SYSPROC".into()),
                job_wait: Some("NO".into()),
                ..Task::default()
            },
            all.unwrap_or_default(),
            None,
        ),
    };
    let tasks: Vec<(String, String)> = (0..s.rows.len())
        .filter_map(|i| s.get(i, "TASK_NAME"))
        .filter(|n| *n != t.name)
        .map(|n| (n.to_string(), n.to_string()))
        .collect();
    let mut v = values_of(&t);
    let mut notes = vec![PASSWORD_NOTE.to_string()];
    if let Some(r) = r {
        notes.insert(0, RECREATE.to_string());
        v.insert("creator".into(), jtext(s.get(r, "CREATOR")));
        v.insert("last_modified".into(), jtext(s.get(r, "LAST_MODIFIED")));
        if let Ok(st) = src.query(&format!("{ZOS_STATUS} WHERE S.TASK_NAME = ?"), &[&t.name]) {
            let status = st.get(0, "STATUS").map(|x| match st.get(0, "MSG") {
                Some(m) => format!("{x}: {m}"),
                None => x.to_string(),
            });
            v.insert("status".into(), jtext(status.as_deref()));
            v.insert("invocations".into(), jtext(st.get(0, "NUM_INVOCATIONS")));
        }
    }
    Ok(ObjectForm {
        sections: sections(r.is_some()),
        values: v,
        choices: BTreeMap::from([("task".to_string(), tasks)]),
        notes,
        read_only: false,
    })
}

/// A data set name: qualifiers of 1 to 8 of A–Z, 0–9, `@ # $ -`, not
/// starting with a digit, joined by dots, 44 at most (upper-cased).
fn dsn(s: &str) -> bool {
    s.len() <= 44
        && s.split('.').all(|q| {
            (1..=8).contains(&q.len())
                && !q.as_bytes()[0].is_ascii_digit()
                && q.bytes()
                    .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b"@#$-".contains(&b))
        })
}

/// A member or subsystem name: 1 to `max` of A–Z, 0–9, `@ # $`, not
/// starting with a digit (upper-cased).
fn member(s: &str, max: usize) -> bool {
    (1..=max).contains(&s.len())
        && !s.as_bytes()[0].is_ascii_digit()
        && s.bytes()
            .all(|b| b.is_ascii_uppercase() || b.is_ascii_digit() || b"@#$".contains(&b))
}

/// The task the form describes, checked, and the password typed (if any).
pub(super) fn from_values(values: &FormValues) -> Result<(Task, Option<String>)> {
    let v = Vals(&values.values);
    let name = required(v, "task_name", "Nombre", 128)?;
    let description = optional(v, "description", "Descripción", 128)?;
    let action = choice(v, "action", "Ejecuta", &["procedure", "jcl"], "procedure")?;
    let mut t = Task {
        name,
        description,
        action: action.clone(),
        ..Task::default()
    };
    if action == "procedure" {
        t.schema = Some(required(v, "procedure_schema", "Esquema", 128)?);
        t.procedure = Some(required(v, "procedure_name", "Procedimiento", 128)?);
        t.input = code_text(v, "procedure_input", "Consulta de parámetros", 4096)?;
    } else {
        let lib = required(v, "jcl_library", "Biblioteca JCL", 44)?.to_ascii_uppercase();
        if !dsn(&lib) {
            return Err(bad(format!(
                "«Biblioteca JCL»: «{}» no es un nombre de data set válido.",
                short(&lib)
            )));
        }
        let mem = required(v, "jcl_member", "Miembro JCL", 8)?.to_ascii_uppercase();
        if !member(&mem, 8) {
            return Err(bad(format!(
                "«Miembro JCL»: «{}» no es un nombre de miembro válido.",
                short(&mem)
            )));
        }
        t.jcl_library = Some(lib);
        t.jcl_member = Some(mem);
        t.job_wait = Some(choice(v, "job_wait", "Espera al job", WAITS, "NO")?);
    }
    t.trigger = choice(v, "trigger", "Se ejecuta", TRIGGERS, "once")?;
    match t.trigger.as_str() {
        "interval" => {
            t.interval = Some(
                v.int("interval", "Cada (minutos)", 1, i64::from(i32::MAX))?
                    .ok_or_else(|| bad("Falta «Cada (minutos)»."))?,
            );
        }
        "cron" => {
            t.point = Some(
                cron(v, "point_in_time", "Calendario", 5..=5)?
                    .ok_or_else(|| bad("Falta «Calendario»."))?,
            );
        }
        "task" => {
            let other = required(v, "trigger_task_name", "Tarea que la dispara", 128)?;
            if other == t.name {
                return Err(bad("Una tarea no puede dispararse a sí misma."));
            }
            t.trigger_task = Some(other);
            let cond = v.text("trigger_task_cond").to_ascii_uppercase();
            if !cond.is_empty() {
                if !CONDS.contains(&cond.as_str()) {
                    return Err(bad(format!(
                        "«Condición sobre su código de retorno»: «{}» no es una opción válida.",
                        short(&cond)
                    )));
                }
                t.cond = Some(cond);
                t.code = Some(
                    v.int(
                        "trigger_task_code",
                        "Código",
                        i64::from(i32::MIN),
                        i64::from(i32::MAX),
                    )?
                    .ok_or_else(|| bad("Con una condición hace falta el «Código»."))?,
                );
            }
        }
        "db2start" => t.trigger_task = Some("DB2START".into()),
        "db2stop" => t.trigger_task = Some("DB2STOP".into()),
        _ => {}
    }
    t.begin = datetime(v, "begin", "Inicio")?;
    t.end = datetime(v, "end", "Fin")?;
    if let (Some(b), Some(e)) = (t.begin, t.end) {
        if e < b {
            return Err(bad("«Fin» tiene que ser posterior a «Inicio»."));
        }
    }
    t.max = v.int(
        "max_invocations",
        "Máximo de ejecuciones",
        0,
        i64::from(i32::MAX),
    )?;
    t.user = optional(v, "user_id", "Usuario", 128)?;
    t.ssid = optional(v, "db2_ssid", "Miembro (data sharing)", 4)?.map(|s| s.to_ascii_uppercase());
    if let Some(s) = &t.ssid {
        if !member(s, 4) {
            return Err(bad(format!(
                "«Miembro (data sharing)»: «{}» no es un nombre de subsistema válido.",
                short(s)
            )));
        }
    }
    let password = match v.raw("password") {
        p if p.is_empty() => None,
        p if p.chars().any(char::is_control) || p.chars().count() > PASSWORD_MAX => {
            return Err(bad(
                "«Contraseña» no es válida: hasta 24 caracteres, sin saltos de línea.",
            ))
        }
        p => Some(p),
    };
    if password.is_some() && t.user.is_none() {
        return Err(bad("La contraseña va con un «Usuario»."));
    }
    Ok((t, password))
}

/// Db2 refuses a user without its password (return code 12), on ADD and
/// on UPDATE alike: a save that sends the user needs it typed again.
fn password_needed(t: &Task, password: Option<&str>) -> Result<()> {
    if t.user.is_some() && password.is_none() {
        return Err(bad(PASSWORD_NEEDED));
    }
    Ok(())
}

fn ts_arg(ms: Option<i64>) -> String {
    ms.map(|ms| format!("TIMESTAMP('{}')", crate::jobs::db2::db2_ts(ms)))
        .unwrap_or_else(|| "NULL".into())
}

fn text_arg(v: Option<&str>) -> String {
    v.map(lit).unwrap_or_else(|| "NULL".into())
}

fn int_arg(v: Option<i64>) -> String {
    v.map(|n| n.to_string()).unwrap_or_else(|| "NULL".into())
}

/// The password argument: NULL, or a marker bound to the password (the
/// mask in the script's plan, which never runs). Never a literal: the
/// statement's text stays in Db2's dynamic statement cache.
fn password_arg(p: Option<&str>, reveal: Reveal, params: &mut Vec<Param>) -> String {
    let Some(p) = p else {
        return "NULL".into();
    };
    params.push(Param::In(Some(match reveal {
        Reveal::Mask => SECRET_MASK.to_string(),
        Reveal::Real => p.to_string(),
    })));
    "?".into()
}

/// The script's line on what the markers carry (the password masked).
fn markers_note(password: bool, name: Option<&str>) -> String {
    let mut parts = Vec::new();
    if password {
        parts.push(format!("la contraseña ({})", lit(SECRET_MASK)));
    }
    if let Some(n) = name {
        parts.push(format!(
            "el nombre de la tarea ({}), de entrada y salida: vuelve con el nombre que le dio Db2",
            lit(n)
        ));
    }
    parts.push("el código de retorno y el mensaje, que devuelve el planificador".into());
    format!("Los ? van como parámetros, en orden: {}.", parts.join("; "))
}

/// The return code and message every z/OS procedure answers: 12 is an
/// error, its message the reason.
pub(super) fn check(outs: &[Option<String>]) -> Result<Option<String>> {
    let rc = int(outs.first().and_then(|o| o.as_deref()));
    match rc {
        Some(rc) if rc >= 8 => Err(Error::Query(
            outs.get(1)
                .and_then(|m| m.as_deref())
                .map(str::trim)
                .filter(|m| !m.is_empty())
                .map(str::to_string)
                .unwrap_or_else(|| format!("El planificador respondió con el código {rc}.")),
        )),
        _ => Ok(None),
    }
}

/// ADMIN_TASK_ADD's values back: the task's name, then [`check`]'s.
pub(super) fn check_add(outs: &[Option<String>]) -> Result<Option<String>> {
    check(outs.get(1..).unwrap_or_default())?;
    Ok(outs
        .first()
        .and_then(|n| n.as_deref())
        .map(str::trim)
        .filter(|n| !n.is_empty())
        .map(str::to_string))
}

fn call(proc_name: &str, args: Vec<String>) -> String {
    format!("CALL SYSPROC.{proc_name}(\n    {}\n)", args.join(",\n    "))
}

/// The schedule arguments, from interval to trigger-task-code.
fn trigger_args(t: &Task) -> Vec<String> {
    vec![
        int_arg(t.interval),
        text_arg(t.point.as_deref()),
        text_arg(t.trigger_task.as_deref()),
        text_arg(t.cond.as_deref()),
        int_arg(t.code),
    ]
}

/// Begin and end: a kept begin in the past goes as NULL with a comment.
fn window(
    plan: &mut Plan,
    t: &Task,
    kept_begin: Option<i64>,
    now: Option<i64>,
) -> Result<(String, String)> {
    let past = |ms: Option<i64>| matches!((ms, now), (Some(t), Some(n)) if t <= n);
    let begin = if past(t.begin) {
        if t.begin != kept_begin {
            return Err(bad(BEGIN_PAST));
        }
        plan.comment(&format!(
            "Inicio original: {}. Db2 no acepta un inicio pasado: sin él, la tarea corre desde ya según su programación.",
            crate::jobs::plain(t.begin.unwrap_or_default())
        ));
        "NULL".to_string()
    } else {
        ts_arg(t.begin)
    };
    if past(t.end) {
        return Err(bad(END_PAST));
    }
    Ok((begin, ts_arg(t.end)))
}

fn add(
    plan: &mut Plan,
    t: &Task,
    password: Option<&str>,
    reveal: Reveal,
    kept_begin: Option<i64>,
    now: Option<i64>,
) -> Result<()> {
    password_needed(t, password)?;
    let (begin, end) = window(plan, t, kept_begin, now)?;
    let mut params = Vec::new();
    let mut args = vec![
        text_arg(t.user.as_deref()),
        password_arg(password, reveal, &mut params),
        begin,
        end,
        int_arg(t.max),
    ];
    args.extend(trigger_args(t));
    args.extend([
        text_arg(t.ssid.as_deref()),
        text_arg(t.schema.as_deref()),
        text_arg(t.procedure.as_deref()),
        text_arg(t.input.as_deref()),
        text_arg(t.jcl_library.as_deref()),
        text_arg(t.jcl_member.as_deref()),
        text_arg(t.job_wait.as_deref()),
        // The task name is INOUT: a marker, never a constant.
        "?".into(),
        text_arg(t.description.as_deref()),
        "?".into(),
        "?".into(),
    ]);
    params.extend([Param::InOut(Some(t.name.clone())), Param::Out, Param::Out]);
    plan.comment(&markers_note(password.is_some(), Some(&t.name)));
    plan.call_out(call("ADMIN_TASK_ADD", args), params, check_add);
    Ok(())
}

fn remove(plan: &mut Plan, name: &str, check: OutCheck) {
    plan.call_out(
        call("ADMIN_TASK_REMOVE", vec![lit(name), "?".into(), "?".into()]),
        vec![Param::Out, Param::Out],
        check,
    );
}

/// [`check`] for the old task's removal after the new one was added: a
/// failure says both are there now.
fn check_old_removed(outs: &[Option<String>]) -> Result<Option<String>> {
    check(outs).map_err(|e| {
        bad(format!(
            "La tarea nueva se creó, pero la anterior no se pudo borrar (ahora están las dos): {}",
            match e {
                Error::Query(m) => m,
                e => e.to_string(),
            }
        ))
    })
}

pub(super) fn create_plan(
    t: &Task,
    password: Option<&str>,
    reveal: Reveal,
    now: Option<i64>,
) -> Result<Plan> {
    let mut p = Plan::new(&format!(
        "Tarea «{}» del planificador de tareas administrativas de Db2 for z/OS",
        t.name
    ));
    add(&mut p, t, password, reveal, None, now)?;
    p.id = Some(t.name.clone());
    Ok(p)
}

pub(super) fn alter_plan(
    cur: &Task,
    t: &Task,
    password: Option<&str>,
    reveal: Reveal,
    now: Option<i64>,
) -> Result<Plan> {
    let recreate = t.name != cur.name
        || t.action != cur.action
        || !same(&t.schema, &cur.schema)
        || !same(&t.procedure, &cur.procedure)
        || !same(&t.input, &cur.input)
        || !same(&t.jcl_library, &cur.jcl_library)
        || !same(&t.jcl_member, &cur.jcl_member)
        || (t.action == "jcl" && !same(&t.job_wait, &cur.job_wait));
    let changed = t.begin != cur.begin
        || t.end != cur.end
        || t.max != cur.max
        || t.trigger != cur.trigger
        || t.interval != cur.interval
        || !same(&t.point, &cur.point)
        || !same(&t.trigger_task, &cur.trigger_task)
        || !same(&t.cond, &cur.cond)
        || t.code != cur.code
        || !same(&t.ssid, &cur.ssid)
        || !same(&t.user, &cur.user)
        || !same(&t.description, &cur.description)
        || password.is_some();
    if !recreate && !changed {
        return Ok(Plan::nothing(&cur.name, cur.name.clone()));
    }
    if recreate && t.name == cur.name {
        return Err(bad(SAME_NAME_WORK));
    }
    password_needed(t, password)?;
    let mut p = Plan::new(&format!(
        "Tarea «{}» del planificador de tareas administrativas de Db2 for z/OS",
        cur.name
    ));
    p.id = Some(t.name.clone());
    if recreate {
        for line in RECREATE.split(". ").filter(|l| !l.is_empty()) {
            p.comment(line.trim_end_matches('.'));
        }
        // The new task first: if Db2 refuses it, the old one is untouched.
        add(&mut p, t, password, reveal, cur.begin, now)?;
        remove(&mut p, &cur.name, check_old_removed);
        return Ok(p);
    }
    let (begin, end) = window(&mut p, t, cur.begin, now)?;
    let mut params = Vec::new();
    let mut args = vec![
        text_arg(t.user.as_deref()),
        password_arg(password, reveal, &mut params),
        begin,
        end,
        int_arg(t.max),
    ];
    args.extend(trigger_args(t));
    args.extend([
        text_arg(t.ssid.as_deref()),
        lit(&t.name),
        text_arg(t.description.as_deref()),
        "?".into(),
        "?".into(),
    ]);
    params.extend([Param::Out, Param::Out]);
    if password.is_some() {
        p.comment(&markers_note(true, None));
    }
    p.call_out(call("ADMIN_TASK_UPDATE", args), params, check);
    Ok(p)
}

/// The tasks another task's end triggers: a new name would leave them
/// pointing at a task that's gone.
fn triggered_by(s: &Set, name: &str) -> Vec<String> {
    (0..s.rows.len())
        .filter(|r| s.get(*r, "TRIGGER_TASK_NAME") == Some(name))
        .filter_map(|r| s.get(r, "TASK_NAME").map(str::to_string))
        .collect()
}

pub(super) fn plan(
    src: &mut dyn JobSource,
    id: Option<&str>,
    values: &FormValues,
    reveal: Reveal,
) -> Result<Plan> {
    let (t, password) = from_values(values)?;
    let now = Tz::query(src, NOW).now();
    match id {
        None => create_plan(&t, password.as_deref(), reveal, now),
        Some(id) => {
            let (cur, s, _) = read(src, id)?;
            let after = triggered_by(&s, &cur.name);
            if t.name != cur.name && !after.is_empty() {
                let list: Vec<String> = after.iter().map(|n| format!("«{n}»")).collect();
                return Err(bad(format!(
                    "No se le puede cambiar el nombre mientras la usan como disparador {}: quedarían apuntando a una tarea borrada. Cambiales antes la «Tarea que la dispara».",
                    list.join(", ")
                )));
            }
            alter_plan(&cur, &t, password.as_deref(), reveal, now)
        }
    }
}

pub(super) fn drop_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra la tarea «{name}» del planificador de tareas administrativas de Db2 for z/OS"
    ));
    remove(&mut p, name, check);
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (t, _, _) = read(src, id)?;
    Ok(drop_plan(&t.name))
}

pub(super) fn logs(src: &mut dyn JobSource) -> Result<Vec<MonitorTable>> {
    let s = src.query(ZOS_STATUS, &[])?;
    let mut t = MonitorTable::new(
        "admin_task_status",
        "Estado de las tareas",
        &[
            "Tarea",
            "Estado",
            "Ejecuciones",
            "Inicio",
            "Fin",
            "Job",
            "Código de retorno",
            "SQLCODE",
            "Abend",
            "Mensaje",
        ],
    );
    let mut order: Vec<usize> = (0..s.rows.len()).collect();
    order.sort_by(|a, b| {
        s.get(*b, "START_TIMESTAMP")
            .cmp(&s.get(*a, "START_TIMESTAMP"))
    });
    for r in order {
        let abend = [s.get(r, "SYSTEM_ABENDCD"), s.get(r, "USER_ABENDCD")]
            .into_iter()
            .flatten()
            .filter(|v| !v.trim_start_matches(['0', 'x', 'X']).is_empty())
            .collect::<Vec<_>>()
            .join(" / ");
        t.rows.push(vec![
            jtext(s.get(r, "TASK_NAME")),
            jtext(s.get(r, "STATUS")),
            jtext(s.get(r, "NUM_INVOCATIONS")),
            jtext(s.get(r, "START_TIMESTAMP")),
            jtext(s.get(r, "END_TIMESTAMP")),
            jtext(s.get(r, "JOB_ID")),
            jtext(s.get(r, "MAXRC")),
            jtext(s.get(r, "SQLCODE")),
            jtext(Some(abend.as_str()).filter(|a| !a.is_empty())),
            jtext(s.get(r, "MSG")),
        ]);
    }
    Ok(vec![t])
}
