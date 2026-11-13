//! Db2 for i: the job scheduler's entries (what WRKJOBSCDE shows) managed
//! whole with the CL commands, run through `QSYS2.QCMDEXC`: `ADDJOBSCDE`
//! adds one, `CHGJOBSCDE` changes it in place, `RMVJOBSCDE` removes it,
//! `HLDJOBSCDE` / `RLSJOBSCDE` hold and release it. Entries are read from
//! `QSYS2.SCHEDULED_JOB_INFO`. CL commands aren't under commitment control:
//! a save runs its commands one after the other.
//!
//! An entry has no new-name parameter: renaming it adds it under the new
//! name and then removes the old one (with a new entry number; its last
//! submission dates go). A new held entry whose name others share is held
//! once its number is known, after the add; if that fails the save fails
//! and says the entry was left released. Dates go
//! as `YYMMDD` after `CHGJOB DATFMT(*YMD)` (the server job's date format,
//! for that connection), so they are kept between 1940 and 2039. Every
//! value is checked for its CL shape (IBM i names, special values, a
//! command with its quotes and parentheses balanced) before it goes into
//! the command, which goes into a SQL literal.
//!
//! An entry IBM i supplies (Operational Assistant's backup entries, or one
//! an IBM-supplied profile such as QSYS added: see
//! [`crate::jobs::db2i::ibm_supplied`]) is only read. A name that begins
//! with Q is not that evidence: IBM i only recommends leaving those names
//! to its own objects and ADDJOBSCDE takes them, so a user's QUARTERLY is
//! changed and deleted like any other entry, and a new or renamed entry
//! may take such a name (the script says IBM's recommendation). The
//! scheduler has no command that submits an entry now (WRKJOBSCDE's option
//! 10 is a panel option), so there is no "run now".

use super::*;
use crate::jobs::db2i::{
    ibm_supplied, name_ok, parse_id, q_name, qcmdexc, row_id, special, tokens, COMMAND, DATE,
    DATE_VALUE, DAYS, DESCRIPTION, FREQUENCY, JOBD, JOBD_LIB, JOBQ, JOBQ_LIB, KEEP, LAST_JOB,
    LAST_STATUS, LAST_SUBMIT, LIST, MSGQ, MSGQ_LIB, NEXT, NO_READ, NO_VIEW, OMITTED, PARTIAL,
    RECOVERY, REL_DAYS, SCHEDULED_BY, STATUS, TIME, USER,
};
use crate::jobs::{code, get, not_found, parse_time};
use dbine_driver::{FormField, FormKind, FormList, FormSection, SchedulerKind};
use serde_json::json;

const JOBCTL: &str = "IBM i cambia, retiene o borra las entradas que agregaron otros usuarios solo con la autorización especial *JOBCTL.";
const RENAME: &str = "IBM i no cambia el nombre de una entrada: se agrega con el nombre nuevo y después se borra la anterior (si el alta falla, la anterior queda), con otro número de entrada; se pierden las fechas de su último envío.";
const IBM_SUPPLIED: &str = "Es una entrada que trae IBM i (la agregó un perfil del sistema, como QSYS, o es del respaldo de Operational Assistant, que se cambia desde GO BACKUP): se ve solo para lectura; se retiene o se libera desde la lista de jobs.";
const Q_NAME: &str = "IBM i recomienda dejar los nombres que empiezan con Q para sus propios objetos: la entrada se agrega igual con este nombre.";
const Q_OWN: &str = "Su nombre empieza con Q, como los objetos de IBM i, pero no la agregó un perfil del sistema: se cambia como cualquier otra entrada. Si la creó un producto, conviene cambiarla desde ese producto.";

const FREQS: &[&str] = &["*ONCE", "*WEEKLY", "*MONTHLY"];
const DATE_MODES: &[&str] = &["date", "*CURRENT", "*MONTHSTR", "*MONTHEND", "*NONE"];
const WEEK: &[&str] = &[
    "*ALL", "*MON", "*TUE", "*WED", "*THU", "*FRI", "*SAT", "*SUN",
];
const RELATIVE: &[&str] = &["1", "2", "3", "4", "5", "*LAST"];
const RECOVERIES: &[&str] = &["*SBMRLS", "*SBMHLD", "*NOSBM"];

/// An entry as `SCHEDULED_JOB_INFO` shows it and ADDJOBSCDE takes it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Entry {
    pub(super) name: String,
    pub(super) command: String,
    /// `*ONCE`, `*WEEKLY`, `*MONTHLY`.
    pub(super) frequency: String,
    /// "date" (then `date`) or a special value of SCDDATE.
    pub(super) date_mode: String,
    /// ms at midnight.
    pub(super) date: Option<i64>,
    pub(super) days: Vec<String>,
    /// Seconds after midnight; `None`: `*CURRENT`.
    pub(super) time: Option<i64>,
    pub(super) relative: Vec<String>,
    pub(super) keep: bool,
    pub(super) omit: Vec<i64>,
    pub(super) recovery: String,
    pub(super) jobd: String,
    pub(super) jobq: String,
    pub(super) user: String,
    pub(super) msgq: String,
    pub(super) text: String,
    pub(super) held: bool,
}

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

/// The view's rows, or the note saying why there are none.
fn read_all(src: &mut dyn JobSource) -> Result<std::result::Result<Set, &'static str>> {
    match src.query(LIST, &[]) {
        Ok(s) => Ok(Ok(s)),
        Err(e) => match code(&e) {
            Some(-204) => Ok(Err(NO_VIEW)),
            Some(-551 | -552 | -443) => Ok(Err(NO_READ)),
            _ => Err(e),
        },
    }
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    let s = match read_all(src)? {
        Ok(s) => s,
        Err(note) => {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![note.to_string()],
            })
        }
    };
    let mut notes = vec![JOBCTL.to_string()];
    if (0..s.rows.len()).any(|r| get(&s, r, COMMAND).is_none()) {
        notes.push(PARTIAL.to_string());
    }
    Ok(SchedulerInfo {
        kinds: vec![SchedulerKind::new(
            "job",
            "Entradas de planificación",
            "entrada de planificación",
        )
        .abilities(true, true, true)],
        logs: false,
        notes,
    })
}

/// `LIB/OBJ`, or the object alone when the library is `*LIBL` or missing.
fn object(s: &Set, r: usize, lib: &[&str], obj: &[&str], default: &str) -> String {
    let Some(o) = get(s, r, obj) else {
        return default.to_string();
    };
    let o = o.to_ascii_uppercase();
    if o.starts_with('*') {
        return o;
    }
    match get(s, r, lib).map(str::to_ascii_uppercase) {
        Some(l) if l != "*LIBL" => format!("{l}/{o}"),
        _ => o,
    }
}

pub(super) fn entry_of(s: &Set, r: usize) -> Entry {
    let mut days: Vec<String> = tokens(get(s, r, DAYS))
        .into_iter()
        .filter(|d| d != "*NONE")
        .collect();
    days.sort_by_key(|d| WEEK.iter().position(|w| w == d));
    let mut relative: Vec<String> = tokens(get(s, r, REL_DAYS))
        .into_iter()
        .filter(|d| d != "*NONE")
        .collect();
    relative.sort_by_key(|d| RELATIVE.iter().position(|w| w == d));
    let (date_mode, date) = match get(s, r, DATE_VALUE).and_then(special) {
        Some(d) if ["*CURRENT", "*MONTHSTR", "*MONTHEND", "*NONE"].contains(&d.as_str()) => {
            (d, None)
        }
        _ => match get(s, r, DATE).and_then(parse_date) {
            Some(ms) => ("date".to_string(), Some(ms)),
            None if !days.is_empty() => ("*NONE".to_string(), None),
            None => ("*CURRENT".to_string(), None),
        },
    };
    let time = get(s, r, TIME).and_then(parse_time).map(|ms| ms / 1000);
    Entry {
        name: get(s, r, crate::jobs::db2i::NAME)
            .unwrap_or_default()
            .to_ascii_uppercase(),
        command: get(s, r, COMMAND).unwrap_or_default().to_string(),
        frequency: get(s, r, FREQUENCY)
            .and_then(special)
            .unwrap_or_else(|| "*ONCE".into()),
        date_mode,
        date,
        days,
        time,
        relative,
        keep: crate::jobs::yes(get(s, r, KEEP))
            || get(s, r, KEEP).and_then(special).as_deref() == Some("*YES"),
        omit: get(s, r, OMITTED)
            .unwrap_or_default()
            .split([' ', ','])
            .filter_map(parse_date)
            .collect(),
        recovery: get(s, r, RECOVERY)
            .and_then(special)
            .unwrap_or_else(|| "*SBMRLS".into()),
        jobd: object(s, r, JOBD_LIB, JOBD, "*USRPRF"),
        jobq: object(s, r, JOBQ_LIB, JOBQ, "*JOBD"),
        user: get(s, r, USER)
            .map(str::to_ascii_uppercase)
            .unwrap_or_else(|| "*CURRENT".into()),
        msgq: object(s, r, MSGQ_LIB, MSGQ, "*USRPRF"),
        text: get(s, r, DESCRIPTION).unwrap_or_default().to_string(),
        held: get(s, r, STATUS).is_some_and(|v| v.eq_ignore_ascii_case("HELD")),
    }
}

/// Entry `id` (`000123/NAME`): its number, the entry, every row and its row.
fn read(src: &mut dyn JobSource, id: &str) -> Result<(String, Entry, Set, usize)> {
    let (n, name) = parse_id(id)?;
    let s = read_all(src)?.map_err(|note| Error::Query(note.into()))?;
    let r = (0..s.rows.len())
        .find(|r| row_id(&s, *r).as_ref() == Some(&(n.clone(), name.clone())))
        .ok_or_else(|| not_found(id))?;
    Ok((n, entry_of(&s, r), s, r))
}

pub(super) fn values_of(e: &Entry) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(e.name)),
        ("text", json!(e.text)),
        ("command", json!(e.command)),
        ("enabled", json!(!e.held)),
        ("frequency", json!(e.frequency)),
        ("date_mode", json!(e.date_mode)),
        ("date", e.date.map_or(Value::Null, |d| json!(date_value(d)))),
        ("days", json!(e.days)),
        ("time", e.time.map_or(Value::Null, |t| json!(time_text(t)))),
        ("relative", json!(e.relative)),
        ("keep", json!(e.keep)),
        ("recovery", json!(e.recovery)),
        ("jobd", json!(e.jobd)),
        ("jobq", json!(e.jobq)),
        ("user", json!(e.user)),
        ("msgq", json!(e.msgq)),
    ])
}

fn omit_rows(e: &Entry) -> Vec<BTreeMap<String, Value>> {
    e.omit
        .iter()
        .map(|d| BTreeMap::from([("date".to_string(), json!(date_value(*d)))]))
        .collect()
}

pub(super) fn sections(existing: bool, omit: Vec<BTreeMap<String, Value>>) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre del trabajo", FormKind::Text)
                .required()
                .help("Nombre de IBM i: hasta 10 caracteres (A–Z, 0–9, $ # @ _ .), sin empezar con un número. Cambiarlo vuelve a crear la entrada."),
            FormField::new("text", "Descripción", FormKind::Text)
                .help("Hasta 50 caracteres."),
            FormField::new("command", "Comando", FormKind::code("cl"))
                .required()
                .help("El comando CL que ejecuta el trabajo enviado, hasta 512 caracteres.")
                .placeholder("CALL PGM(MIBIB/CIERRE)"),
            FormField::new("enabled", "Habilitada", FormKind::Bool)
                .default(json!(true))
                .help("Deshabilitada: la entrada queda retenida (HLDJOBSCDE) y no envía el trabajo."),
        ]),
        FormSection::new("schedule", "Programación")
            .fields(vec![
                FormField::new(
                    "frequency",
                    "Frecuencia",
                    FormKind::select(&[
                        ("*ONCE", "Una vez"),
                        ("*WEEKLY", "Semanal"),
                        ("*MONTHLY", "Mensual"),
                    ]),
                )
                .default(json!("*ONCE")),
                FormField::new(
                    "date_mode",
                    "Fecha",
                    FormKind::select(&[
                        ("date", "Una fecha"),
                        ("*CURRENT", "Hoy"),
                        ("*MONTHSTR", "Primer día del mes"),
                        ("*MONTHEND", "Último día del mes"),
                        ("*NONE", "Ninguna: por días de la semana"),
                    ]),
                )
                .default(json!("*CURRENT")),
                FormField::new("date", "Fecha de envío", FormKind::Date)
                    .when("date_mode", &["date"])
                    .help("Entre 1940 y 2039."),
                FormField::new(
                    "days",
                    "Días",
                    FormKind::multi(&[
                        ("*ALL", "Todos"),
                        ("*MON", "Lunes"),
                        ("*TUE", "Martes"),
                        ("*WED", "Miércoles"),
                        ("*THU", "Jueves"),
                        ("*FRI", "Viernes"),
                        ("*SAT", "Sábado"),
                        ("*SUN", "Domingo"),
                    ]),
                )
                .when("date_mode", &["*NONE"]),
                FormField::new(
                    "relative",
                    "Semanas del mes",
                    FormKind::multi(&[
                        ("1", "Primera"),
                        ("2", "Segunda"),
                        ("3", "Tercera"),
                        ("4", "Cuarta"),
                        ("5", "Quinta"),
                        ("*LAST", "Última"),
                    ]),
                )
                .when("frequency", &["*MONTHLY"])
                .help("Con frecuencia mensual y días de la semana: en qué semanas del mes (el primer lunes, el último viernes…)."),
                FormField::new("time", "Hora", FormKind::Time)
                    .help("Vacío: la hora en que se agrega la entrada."),
                FormField::new("keep", "Conservar la entrada", FormKind::Bool)
                    .when("frequency", &["*ONCE"])
                    .help("Una entrada de una sola vez queda guardada después de enviar el trabajo."),
                FormField::new(
                    "recovery",
                    "Si el sistema estaba apagado",
                    FormKind::select(&[
                        ("*SBMRLS", "Enviar el trabajo"),
                        ("*SBMHLD", "Enviarlo retenido"),
                        ("*NOSBM", "No enviarlo"),
                    ]),
                )
                .default(json!("*SBMRLS"))
                .help("Qué hacer cuando el envío no se pudo hacer a su hora (sistema apagado o en estado restringido)."),
            ])
            .list(
                FormList::new(
                    "omit",
                    "Fechas omitidas",
                    "date",
                    vec![FormField::new("date", "Fecha", FormKind::Date).required()],
                )
                .rows(omit),
            ),
        FormSection::new("job", "Trabajo").fields(vec![
            FormField::new("jobd", "Descripción de trabajo", FormKind::Text)
                .default(json!("*USRPRF"))
                .help("*USRPRF (la del perfil del usuario) o BIBLIOTECA/NOMBRE."),
            FormField::new("jobq", "Cola de trabajos", FormKind::Text)
                .default(json!("*JOBD"))
                .help("*JOBD (la de la descripción de trabajo) o BIBLIOTECA/NOMBRE."),
            FormField::new("user", "Usuario", FormKind::Text)
                .default(json!("*CURRENT"))
                .help("*CURRENT (quien agrega la entrada), *JOBD (el de la descripción de trabajo) o un perfil de usuario."),
            FormField::new("msgq", "Cola de mensajes", FormKind::Text)
                .default(json!("*USRPRF"))
                .help("Dónde avisa el envío: *USRPRF, *NONE o BIBLIOTECA/NOMBRE."),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("entry", "Número de entrada", FormKind::Text).read_only(true),
            FormField::new("status", "Estado", FormKind::Text).read_only(true),
            FormField::new("scheduled_by", "Agregada por", FormKind::Text).read_only(true),
            FormField::new("next", "Próximo envío", FormKind::Text).read_only(true),
            FormField::new("last_submit", "Último envío", FormKind::Text).read_only(true),
            FormField::new("last_status", "Estado del último envío", FormKind::Text)
                .read_only(true),
            FormField::new("last_job", "Último trabajo enviado", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        let e = Entry {
            frequency: "*ONCE".into(),
            date_mode: "*CURRENT".into(),
            recovery: "*SBMRLS".into(),
            jobd: "*USRPRF".into(),
            jobq: "*JOBD".into(),
            user: "*CURRENT".into(),
            msgq: "*USRPRF".into(),
            ..Entry::default()
        };
        return Ok(ObjectForm {
            sections: sections(false, Vec::new()),
            values: values_of(&e),
            notes: vec![JOBCTL.to_string()],
            ..ObjectForm::default()
        });
    };
    let (n, e, s, r) = read(src, id)?;
    let mut v = values_of(&e);
    v.insert("entry".into(), json!(n));
    for (k, c) in [
        ("status", STATUS),
        ("scheduled_by", SCHEDULED_BY),
        ("next", NEXT),
        ("last_submit", LAST_SUBMIT),
        ("last_status", LAST_STATUS),
        ("last_job", LAST_JOB),
    ] {
        v.insert(k.into(), jtext(get(&s, r, c)));
    }
    let supplied = ibm_supplied(&e.name, get(&s, r, SCHEDULED_BY));
    let mut notes = if supplied {
        vec![IBM_SUPPLIED.to_string()]
    } else {
        vec![RENAME.to_string(), JOBCTL.to_string()]
    };
    if !supplied && q_name(&e.name) {
        notes.push(Q_OWN.to_string());
    }
    if get(&s, r, COMMAND).is_none() {
        notes.push(PARTIAL.to_string());
    }
    Ok(ObjectForm {
        sections: sections(true, omit_rows(&e)),
        values: v,
        notes,
        read_only: supplied,
        ..ObjectForm::default()
    })
}

/// An IBM i object: `LIB/NAME`, `NAME` (in the library list) or one of
/// `specials`, upper-cased.
fn object_field(v: Vals, k: &str, label: &str, specials: &[&str], default: &str) -> Result<String> {
    let t = v
        .opt(k)
        .unwrap_or_else(|| default.to_string())
        .to_ascii_uppercase();
    let invalid = || {
        bad(format!(
            "«{label}»: «{}» no es {} ni BIBLIOTECA/NOMBRE.",
            short(&t),
            specials.join(", ")
        ))
    };
    if t.starts_with('*') {
        return specials
            .contains(&t.as_str())
            .then(|| t.clone())
            .ok_or_else(invalid);
    }
    let ok = match t.split_once('/') {
        Some((lib, obj)) => (name_ok(lib) || lib == "*LIBL" || lib == "*CURLIB") && name_ok(obj),
        None => name_ok(&t),
    };
    ok.then(|| t.clone()).ok_or_else(invalid)
}

/// A CL command for CMD(…): one line, at most 512 characters, its quotes
/// closed and its parentheses balanced outside them, so nothing in it
/// reaches past the parameter.
pub(super) fn cl_command(cmd: &str) -> Result<()> {
    if cmd.chars().any(char::is_control) {
        return Err(bad(
            "«Comando» tiene que ir en una sola línea, sin caracteres de control.",
        ));
    }
    if cmd.chars().count() > 512 {
        return Err(bad("«Comando» admite hasta 512 caracteres."));
    }
    let (mut depth, mut quoted) = (0i32, false);
    for c in cmd.chars() {
        match c {
            '\'' => quoted = !quoted,
            '(' if !quoted => depth += 1,
            ')' if !quoted => {
                depth -= 1;
                if depth < 0 {
                    break;
                }
            }
            _ => {}
        }
    }
    if quoted || depth != 0 {
        return Err(bad("«Comando» tiene comillas o paréntesis sin cerrar."));
    }
    Ok(())
}

/// The entry the form describes, checked.
pub(super) fn from_values(values: &FormValues) -> Result<Entry> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre del trabajo", 10)?.to_ascii_uppercase();
    if !name_ok(&name) {
        return Err(bad(format!(
            "«Nombre del trabajo»: «{}» no es un nombre de IBM i válido.",
            short(&name)
        )));
    }
    let command = required(v, "command", "Comando", 512)?;
    cl_command(&command)?;
    let text = optional(v, "text", "Descripción", 50)?.unwrap_or_default();
    let frequency = choice(v, "frequency", "Frecuencia", FREQS, "*ONCE")?;
    let date_mode = choice(v, "date_mode", "Fecha", DATE_MODES, "*CURRENT")?;
    let date = if date_mode == "date" {
        let d = super::date(v, "date", "Fecha de envío")?
            .ok_or_else(|| bad("Falta «Fecha de envío»."))?;
        in_range("Fecha de envío", d)?;
        Some(d)
    } else {
        None
    };
    let mut days = Vec::new();
    for d in v.strings("days") {
        let d = d.to_ascii_uppercase();
        if !WEEK.contains(&d.as_str()) {
            return Err(bad(format!("«{}» no es un día de la semana.", short(&d))));
        }
        if !days.contains(&d) {
            days.push(d);
        }
    }
    days.sort_by_key(|d| WEEK.iter().position(|w| w == d));
    if date_mode == "*NONE" {
        if days.is_empty() {
            return Err(bad(
                "Sin fecha, la entrada necesita los «Días» en que se envía.",
            ));
        }
        if days.contains(&"*ALL".to_string()) && days.len() > 1 {
            return Err(bad("«Todos» los días va solo."));
        }
    } else {
        days.clear();
    }
    let mut relative = Vec::new();
    if frequency == "*MONTHLY" && date_mode == "*NONE" {
        for d in v.strings("relative") {
            let d = d.to_ascii_uppercase();
            if !RELATIVE.contains(&d.as_str()) {
                return Err(bad(format!("«{}» no es una semana del mes.", short(&d))));
            }
            if !relative.contains(&d) {
                relative.push(d);
            }
        }
    }
    relative.sort_by_key(|d| RELATIVE.iter().position(|w| w == d));
    let time = super::time(v, "time", "Hora")?;
    let keep = frequency == "*ONCE" && v.bool_or("keep", false);
    let mut omit = Vec::new();
    for row in rows(values, "omit") {
        if let Some(d) = super::date(row, "date", "Fecha omitida")? {
            in_range("Fecha omitida", d)?;
            if !omit.contains(&d) {
                omit.push(d);
            }
        }
    }
    if omit.len() > 20 {
        return Err(bad("IBM i admite hasta 20 «Fechas omitidas»."));
    }
    let recovery = choice(
        v,
        "recovery",
        "Si el sistema estaba apagado",
        RECOVERIES,
        "*SBMRLS",
    )?;
    let jobd = object_field(v, "jobd", "Descripción de trabajo", &["*USRPRF"], "*USRPRF")?;
    let jobq = object_field(v, "jobq", "Cola de trabajos", &["*JOBD"], "*JOBD")?;
    let msgq = object_field(
        v,
        "msgq",
        "Cola de mensajes",
        &["*USRPRF", "*NONE"],
        "*USRPRF",
    )?;
    let user = v
        .opt("user")
        .unwrap_or_else(|| "*CURRENT".into())
        .to_ascii_uppercase();
    if !(user == "*CURRENT" || user == "*JOBD" || name_ok(&user)) {
        return Err(bad(format!(
            "«Usuario»: «{}» no es *CURRENT, *JOBD ni un perfil de usuario.",
            short(&user)
        )));
    }
    Ok(Entry {
        name,
        command,
        frequency,
        date_mode,
        date,
        days,
        time,
        relative,
        keep,
        omit,
        recovery,
        jobd,
        jobq,
        user,
        msgq,
        text,
        held: !v.bool_or("enabled", true),
    })
}

/// A date the job's `*YMD` format writes without doubt.
fn in_range(label: &str, ms: i64) -> Result<()> {
    let y: i64 = date_value(ms)[..4].parse().unwrap_or(0);
    (1940..=2039).contains(&y).then_some(()).ok_or_else(|| {
        bad(format!(
            "«{label}» tiene que estar entre 1940 y 2039 (IBM i la toma como AAMMDD)."
        ))
    })
}

/// `YYMMDD`.
fn ymd(ms: i64) -> String {
    let d = date_value(ms);
    format!("{}{}{}", &d[2..4], &d[5..7], &d[8..10])
}

/// `HHMMSS`.
fn hms(secs: i64) -> String {
    format!("{:02}{:02}{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// A CL quoted string.
fn cl_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The schedule's parameters (FRQ to RELDAYMON); whether a date goes in.
fn schedule_params(e: &Entry, out: &mut Vec<String>, clearing: bool) -> bool {
    out.push(format!("FRQ({})", e.frequency));
    let mut dated = false;
    match (e.date_mode.as_str(), e.date) {
        ("date", Some(d)) => {
            dated = true;
            out.push(format!("SCDDATE({})", ymd(d)));
        }
        (m, _) => out.push(format!("SCDDATE({m})")),
    }
    if !e.days.is_empty() {
        out.push(format!("SCDDAY({})", e.days.join(" ")));
    } else if clearing {
        out.push("SCDDAY(*NONE)".into());
    }
    out.push(match e.time {
        Some(t) => format!("SCDTIME({})", hms(t)),
        None => "SCDTIME(*CURRENT)".into(),
    });
    if !e.relative.is_empty() {
        out.push(format!("RELDAYMON({})", e.relative.join(" ")));
    } else if clearing {
        out.push("RELDAYMON(*NONE)".into());
    }
    dated
}

fn omit_param(e: &Entry) -> String {
    if e.omit.is_empty() {
        "OMITDATE(*NONE)".into()
    } else {
        format!(
            "OMITDATE({})",
            e.omit.iter().map(|d| ymd(*d)).collect::<Vec<_>>().join(" ")
        )
    }
}

fn text_param(e: &Entry) -> String {
    if e.text.is_empty() {
        "TEXT(*BLANK)".into()
    } else {
        format!("TEXT({})", cl_text(&e.text))
    }
}

/// `ADDJOBSCDE …`; whether it has a date.
pub(super) fn add_command(e: &Entry) -> (String, bool) {
    let mut p = vec![
        format!("ADDJOBSCDE JOB({})", e.name),
        format!("CMD({})", e.command),
    ];
    let mut dated = schedule_params(e, &mut p, false);
    if e.frequency == "*ONCE" {
        p.push(format!("SAVE({})", if e.keep { "*YES" } else { "*NO" }));
    }
    if !e.omit.is_empty() {
        dated = true;
        p.push(omit_param(e));
    }
    p.push(format!("RCYACN({})", e.recovery));
    p.push(format!("JOBD({})", e.jobd));
    p.push(format!("JOBQ({})", e.jobq));
    p.push(format!("USER({})", e.user));
    p.push(format!("MSGQ({})", e.msgq));
    p.push(text_param(e));
    (p.join(" "), dated)
}

/// `CHGJOBSCDE …` with what changed (the schedule's parameters together),
/// or `None`; whether it has a date.
pub(super) fn change_command(n: &str, cur: &Entry, e: &Entry) -> Option<(String, bool)> {
    let mut p = vec![format!("CHGJOBSCDE JOB({}) ENTRYNBR({n})", cur.name)];
    let mut dated = false;
    if e.command != cur.command {
        p.push(format!("CMD({})", e.command));
    }
    let schedule = e.frequency != cur.frequency
        || e.date_mode != cur.date_mode
        || e.date != cur.date
        || e.days != cur.days
        || e.time != cur.time
        || e.relative != cur.relative;
    if schedule {
        dated |= schedule_params(e, &mut p, true);
    }
    if e.frequency == "*ONCE" && (e.keep != cur.keep || e.frequency != cur.frequency) {
        p.push(format!("SAVE({})", if e.keep { "*YES" } else { "*NO" }));
    }
    if e.omit != cur.omit {
        dated |= !e.omit.is_empty();
        p.push(omit_param(e));
    }
    for (k, a, b) in [
        ("RCYACN", &e.recovery, &cur.recovery),
        ("JOBD", &e.jobd, &cur.jobd),
        ("JOBQ", &e.jobq, &cur.jobq),
        ("USER", &e.user, &cur.user),
        ("MSGQ", &e.msgq, &cur.msgq),
    ] {
        if a != b {
            p.push(format!("{k}({a})"));
        }
    }
    if e.text != cur.text {
        p.push(text_param(e));
    }
    (p.len() > 1).then(|| (p.join(" "), dated))
}

fn cl(plan: &mut Plan, command: &str) {
    plan.exec(qcmdexc(command));
}

/// The new entry's id: the entry named `name` that wasn't there before;
/// held afterwards when `hold` (the script couldn't name it in advance).
/// When it has to be held, not finding it or not holding it fails the
/// save: the entry was added released and would submit the job.
fn lookup(name: String, before: Vec<String>, hold: bool) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let found = src.query(LIST, &[]).map(|s| {
            (0..s.rows.len())
                .filter_map(|r| row_id(&s, r))
                .filter(|(n, nm)| *nm == name && !before.contains(n))
                .map(|(n, _)| n)
                .max()
        });
        if !hold {
            return Ok(found?.map(|n| format!("{n}/{name}")));
        }
        let n = match found {
            Ok(Some(n)) => n,
            Ok(None) => return Err(bad("La entrada se agregó pero no se encontró su número para retenerla: quedó liberada, así que va a enviar el trabajo. Retenela a mano.")),
            Err(e) => return Err(bad(format!("La entrada se agregó pero no se pudo leer su número para retenerla: quedó liberada, así que va a enviar el trabajo. Retenela a mano. {e}"))),
        };
        src.exec(
            &qcmdexc(&format!("HLDJOBSCDE JOB({name}) ENTRYNBR({n})")),
            &[],
        )
        .map_err(|e| bad(format!("La entrada {n} se agregó pero HLDJOBSCDE falló: quedó liberada, así que va a enviar el trabajo. Retenela a mano. {e}")))?;
        Ok(Some(format!("{n}/{name}")))
    })
}

/// The entry numbers of the entries named `name`.
fn numbers(s: &Set, name: &str) -> Vec<String> {
    (0..s.rows.len())
        .filter_map(|r| row_id(s, r))
        .filter(|(_, nm)| nm == name)
        .map(|(n, _)| n)
        .collect()
}

/// Adds `e` to `plan`: the date format first when a date goes in, the
/// hold after when it's held. `others`: entry numbers already named so.
fn add(plan: &mut Plan, e: &Entry, others: Vec<String>) {
    let (cmd, dated) = add_command(e);
    if dated {
        cl(plan, "CHGJOB DATFMT(*YMD)");
    }
    cl(plan, &cmd);
    let hold_later = e.held && !others.is_empty();
    if e.held && others.is_empty() {
        cl(plan, &format!("HLDJOBSCDE JOB({}) ENTRYNBR(*ONLY)", e.name));
    } else if hold_later {
        plan.comment(&format!(
            "Hay otras entradas «{}»: la nueva se retiene después, con su número (HLDJOBSCDE).",
            e.name
        ));
    }
    plan.lookup = Some(lookup(e.name.clone(), others, hold_later));
    plan.lookup_required = hold_later;
}

pub(super) fn create_plan(e: &Entry, all: &Set) -> Plan {
    let mut p = Plan::new(&format!(
        "Entrada «{}» del planificador de trabajos de IBM i",
        e.name
    ));
    if q_name(&e.name) {
        p.comment(Q_NAME);
    }
    add(&mut p, e, numbers(all, &e.name));
    p
}

pub(super) fn alter_plan(n: &str, cur: &Entry, e: &Entry, all: &Set) -> Plan {
    let id = format!("{n}/{}", cur.name);
    if e.name != cur.name {
        let mut p = Plan::new(&format!(
            "Entrada «{}» del planificador de trabajos de IBM i",
            cur.name
        ));
        for line in RENAME.split(": ") {
            p.comment(line);
        }
        if q_name(&e.name) {
            p.comment(Q_NAME);
        }
        // The new entry first: if IBM i refuses it, the old one stays.
        add(&mut p, e, numbers(all, &e.name));
        cl(
            &mut p,
            &format!("RMVJOBSCDE JOB({}) ENTRYNBR({n})", cur.name),
        );
        return p;
    }
    let change = change_command(n, cur, e);
    if change.is_none() && e.held == cur.held {
        return Plan::nothing(&cur.name, id);
    }
    let mut p = Plan::new(&format!(
        "Entrada «{}» del planificador de trabajos de IBM i",
        cur.name
    ));
    if let Some((cmd, dated)) = change {
        if dated {
            cl(&mut p, "CHGJOB DATFMT(*YMD)");
        }
        cl(&mut p, &cmd);
    }
    if e.held != cur.held {
        let verb = if e.held { "HLDJOBSCDE" } else { "RLSJOBSCDE" };
        cl(&mut p, &format!("{verb} JOB({}) ENTRYNBR({n})", cur.name));
    }
    p.id = Some(id);
    p
}

pub(super) fn plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let e = from_values(values)?;
    match id {
        None => {
            let all = read_all(src)?.unwrap_or_default();
            Ok(create_plan(&e, &all))
        }
        Some(id) => {
            let (n, cur, all, r) = read(src, id)?;
            if ibm_supplied(&cur.name, get(&all, r, SCHEDULED_BY)) {
                return Err(bad(IBM_SUPPLIED));
            }
            Ok(alter_plan(&n, &cur, &e, &all))
        }
    }
}

pub(super) fn drop_plan(n: &str, name: &str) -> Plan {
    let mut p = Plan::new(&format!(
        "Borra la entrada «{name}» ({n}) del planificador de trabajos de IBM i"
    ));
    cl(&mut p, &format!("RMVJOBSCDE JOB({name}) ENTRYNBR({n})"));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (n, e, s, r) = read(src, id)?;
    if ibm_supplied(&e.name, get(&s, r, SCHEDULED_BY)) {
        return Err(bad(IBM_SUPPLIED));
    }
    Ok(drop_plan(&n, &e.name))
}
