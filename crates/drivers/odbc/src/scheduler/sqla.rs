//! SQL Anywhere: events managed whole with `CREATE EVENT`, `ALTER EVENT`
//! and `DROP EVENT`: what fires one (its schedules, a system event type
//! with its conditions, or only `TRIGGER EVENT`), whether it's enabled,
//! where it fires (`AT`), its handler and its comment.
//!
//! Each change to an existing event is its own `ALTER EVENT` (one schedule
//! added, changed or deleted per statement); the name and `AT` can't be
//! altered, so changing them drops the event and creates it again (it gets
//! a new id; events keep no history to lose). DDL commits on its own:
//! the statements run one by one. The script separates them with `go`,
//! since a handler holds `;` of its own.

use super::*;
use crate::jobs::sqla::{location, q, sqla_lit, ONE, SCHEDULES, WEEKDAYS};
use crate::jobs::{int, int_id, not_found, parse_time, parse_ts, yes};
use dbine_driver::{FormField, FormKind, FormList, FormSection, SchedulerKind};
use serde_json::json;

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

const PRIVILEGES: &str = "Crear, cambiar y borrar eventos pide el privilegio MANAGE ANY EVENT (en versiones anteriores a la 16, autoridad DBA).";
const RECREATE: &str = "SQL Anywhere no cambia el nombre ni el lugar (AT) de un evento: se borra y se crea de nuevo; recibe otro id.";
const UNPARSED: &str = "Las condiciones del evento no se pudieron leer como pares de event_condition: se dejan como están en el servidor y la lista queda solo para lectura.";

/// The system event types, when the server doesn't list them.
const TYPES: &[&str] = &[
    "BackupEnd",
    "Connect",
    "ConnectFailed",
    "DatabaseStart",
    "DBDiskSpace",
    "Deadlock",
    "Disconnect",
    "GlobalAutoincrement",
    "GrowDB",
    "GrowLog",
    "GrowTemp",
    "LogDiskSpace",
    "MirrorFailover",
    "MirrorServerDisconnect",
    "RAISERROR",
    "ServerIdle",
    "TempDiskSpace",
];
const CONDITIONS: &[&str] = &[
    "DBFreePercent",
    "DBFreeSpace",
    "DBSize",
    "ErrorNumber",
    "IdleTime",
    "Interval",
    "LogFreePercent",
    "LogFreeSpace",
    "LogSize",
    "RemainingValues",
    "TempFreePercent",
    "TempFreeSpace",
    "TempSize",
];
const OPS: &[&str] = &["=", "<", ">", "!=", "<=", ">="];
const UNITS: &[&str] = &["HOURS", "MINUTES", "SECONDS"];
/// Where it fires; without `AT` it fires everywhere (SQL Anywhere keeps
/// that as `A`, the same as `AT ALL`).
const LOCATIONS: &[&str] = &["", "CONSOLIDATED", "REMOTE"];

/// A schedule of an event. Times: seconds after midnight; the date: ms.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Sched {
    pub(super) name: String,
    pub(super) start: i64,
    /// `BETWEEN start AND end`.
    pub(super) end: Option<i64>,
    pub(super) every: Option<i64>,
    pub(super) unit: String,
    /// Day names (`Monday`…) or days of the month (1–31, 0 the last one).
    pub(super) week: Vec<String>,
    pub(super) month: Vec<i64>,
    pub(super) start_date: Option<i64>,
}

/// An event as the form edits it.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Event {
    pub(super) name: String,
    /// "schedule", "type" or "manual".
    pub(super) mode: String,
    pub(super) event_type: String,
    /// `(condition, operator, value)`.
    pub(super) conditions: Vec<(String, String, String)>,
    /// The condition as the server has it, when it isn't made of pairs.
    pub(super) raw_condition: Option<String>,
    pub(super) schedules: Vec<Sched>,
    pub(super) enabled: bool,
    /// "", "CONSOLIDATED" or "REMOTE".
    pub(super) location: String,
    pub(super) handler: Option<String>,
    pub(super) comment: Option<String>,
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    src.query("SELECT FIRST event_id FROM SYS.SYSEVENT", &[])?;
    Ok(SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Eventos", "evento").abilities(true, true, true)],
        logs: false,
        notes: vec![PRIVILEGES.to_string()],
    })
}

// ------------------------------------------------------------------ reading

fn sched_of(s: &Set, r: usize) -> Sched {
    let secs = |c: &str| s.get(r, c).and_then(parse_time).map(|ms| ms / 1000);
    let week = int(s.get(r, "days_of_week")).unwrap_or(0);
    let month = int(s.get(r, "days_of_month")).unwrap_or(0);
    let mut days_of_month: Vec<i64> = (0..31i64)
        .filter(|i| month & (1i64 << i) != 0)
        .map(|i| i + 1)
        .collect();
    if month & (1i64 << 31) != 0 {
        days_of_month.push(0);
    }
    let unit = match s.get(r, "interval_units").map(str::to_ascii_uppercase) {
        Some(u) if u == "HH" => "HOURS",
        Some(u) if u == "SS" => "SECONDS",
        _ => "MINUTES",
    };
    Sched {
        name: s.get(r, "sched_name").unwrap_or_default().to_string(),
        start: secs("start_time").unwrap_or(0),
        end: secs("stop_time"),
        every: int(s.get(r, "interval_amt")).filter(|n| *n > 0),
        unit: unit.into(),
        week: (0..7usize)
            .filter(|i| week & (1i64 << i) != 0)
            .map(|i| WEEKDAYS[i].to_string())
            .collect(),
        month: if week > 0 { Vec::new() } else { days_of_month },
        start_date: s.get(r, "start_date").and_then(parse_ts),
    }
}

/// `event_condition('X') op value AND …` as pairs; `None` when it isn't
/// made only of those.
pub(super) fn parse_conditions(c: &str) -> Option<Vec<(String, String, String)>> {
    let mut out = Vec::new();
    let upper = c.to_ascii_uppercase();
    let mut parts = Vec::new();
    let mut at = 0;
    while let Some(i) = upper[at..].find(" AND ") {
        parts.push(&c[at..at + i]);
        at += i + 5;
    }
    parts.push(&c[at..]);
    for p in parts {
        let p = p.trim();
        let rest = p
            .get(..15)
            .filter(|h| h.eq_ignore_ascii_case("event_condition"))
            .map(|_| p[15..].trim_start())?;
        let rest = rest.strip_prefix('(')?.trim_start().strip_prefix('\'')?;
        let end = rest.find('\'')?;
        let name = &rest[..end];
        let rest = rest[end + 1..].trim_start().strip_prefix(')')?.trim_start();
        let op = ["<=", ">=", "!=", "<>", "=", "<", ">"]
            .iter()
            .find(|o| rest.starts_with(**o))?;
        let value = rest[op.len()..].trim();
        if value.is_empty() || value.parse::<f64>().is_err() {
            return None;
        }
        let cond = CONDITIONS.iter().find(|x| x.eq_ignore_ascii_case(name))?;
        let op = if *op == "<>" { "!=" } else { op };
        out.push((cond.to_string(), op.to_string(), value.to_string()));
    }
    Some(out)
}

fn read(src: &mut dyn JobSource, id: &str) -> Result<(u64, Event)> {
    let n = int_id(id)?;
    let s = src.query(ONE, &[&n.to_string()])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let sch = src.query(SCHEDULES, &[])?;
    let schedules: Vec<Sched> = (0..sch.rows.len())
        .filter(|r| int(sch.get(*r, "event_id")) == Some(n as i64))
        .map(|r| sched_of(&sch, r))
        .collect();
    let event_type = s.get(0, "event_type").map(str::to_string);
    let condition = s.get(0, "condition");
    let (conditions, raw_condition) = match condition {
        None => (Vec::new(), None),
        Some(c) => match parse_conditions(c) {
            Some(p) => (p, None),
            None => (Vec::new(), Some(c.to_string())),
        },
    };
    let mode = if !schedules.is_empty() {
        "schedule"
    } else if event_type.is_some() {
        "type"
    } else {
        "manual"
    };
    Ok((
        n,
        Event {
            name: s.get(0, "event_name").unwrap_or_default().to_string(),
            mode: mode.into(),
            event_type: event_type.unwrap_or_default(),
            conditions,
            raw_condition,
            schedules,
            enabled: yes(s.get(0, "enabled")),
            location: location(s.get(0, "location")).unwrap_or("").into(),
            handler: s.get(0, "action").map(str::to_string),
            comment: s.get(0, "remarks").map(str::to_string),
        },
    ))
}

// ------------------------------------------------------------------ the form

pub(super) fn sections(existing: bool, conditions_ok: bool) -> Vec<FormSection> {
    let mut month: Vec<(String, String)> =
        (1..=31).map(|d| (d.to_string(), d.to_string())).collect();
    month.push(("0".into(), "Último día".into()));
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Cambiarlo vuelve a crear el evento."),
            FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
            FormField::new(
                "location",
                "Dónde se dispara (AT)",
                FormKind::select(&[
                    ("", "En todas (sin AT)"),
                    ("CONSOLIDATED", "Solo en la consolidada"),
                    ("REMOTE", "Solo en las remotas"),
                ]),
            )
            .default(json!(""))
            .help("Para bases de SQL Remote o MobiLink. Cambiarlo vuelve a crear el evento."),
            FormField::new("comment", "Comentario", FormKind::Textarea),
        ]),
        FormSection::new("firing", "Disparo")
            .fields(vec![
                FormField::new(
                    "mode",
                    "Qué lo dispara",
                    FormKind::select(&[
                        ("schedule", "Un calendario"),
                        ("type", "Un evento del sistema"),
                        ("manual", "Solo TRIGGER EVENT"),
                    ]),
                )
                .default(json!("schedule")),
                FormField::new(
                    "event_type",
                    "Evento del sistema",
                    FormKind::reference("event_type", false),
                )
                .when("mode", &["type"])
                .help("Lo que pasa en el servidor que dispara el evento."),
            ])
            .list(
                FormList::new(
                    "conditions",
                    "Condiciones (WHERE)",
                    "condition",
                    vec![
                        FormField::new(
                            "condition",
                            "Condición",
                            FormKind::select(&[
                                ("DBFreePercent", "Espacio libre de la base (%)"),
                                ("DBFreeSpace", "Espacio libre de la base (MB)"),
                                ("DBSize", "Tamaño de la base (MB)"),
                                ("ErrorNumber", "Número de error"),
                                ("IdleTime", "Segundos sin actividad"),
                                ("Interval", "Segundos desde el último disparo"),
                                ("LogFreePercent", "Espacio libre del log (%)"),
                                ("LogFreeSpace", "Espacio libre del log (MB)"),
                                ("LogSize", "Tamaño del log (MB)"),
                                (
                                    "RemainingValues",
                                    "Valores que quedan (autoincremento global)",
                                ),
                                ("TempFreePercent", "Espacio libre de temporales (%)"),
                                ("TempFreeSpace", "Espacio libre de temporales (MB)"),
                                ("TempSize", "Tamaño de temporales (MB)"),
                            ]),
                        )
                        .required(),
                        FormField::new(
                            "op",
                            "Operador",
                            FormKind::select(&[
                                ("=", "="),
                                ("<", "<"),
                                (">", ">"),
                                ("!=", "!="),
                                ("<=", "<="),
                                (">=", ">="),
                            ]),
                        )
                        .default(json!("<")),
                        FormField::new("value", "Valor", FormKind::number(None, None)).required(),
                    ],
                )
                .read_only(!conditions_ok),
            ),
        FormSection::new("schedules", "Calendarios").list(
            FormList::new(
                "schedules",
                "Calendarios (SCHEDULE)",
                "name",
                vec![
                    FormField::new("name", "Nombre", FormKind::Text)
                        .help("Vacío con un solo calendario: el nombre del evento."),
                    FormField::new(
                        "window",
                        "Horario",
                        FormKind::select(&[
                            ("start", "Desde una hora (START TIME)"),
                            ("between", "Entre dos horas (BETWEEN)"),
                        ]),
                    )
                    .default(json!("start")),
                    FormField::new("start_time", "Hora de inicio", FormKind::Time)
                        .required()
                        .default(json!("00:00:00")),
                    FormField::new("end_time", "Hora de fin", FormKind::Time)
                        .when("window", &["between"]),
                    FormField::new("every", "Cada", FormKind::number(Some(1.0), None))
                        .help("Vacío: una vez por día elegido."),
                    FormField::new(
                        "unit",
                        "Unidad",
                        FormKind::select(&[
                            ("HOURS", "Horas"),
                            ("MINUTES", "Minutos"),
                            ("SECONDS", "Segundos"),
                        ]),
                    )
                    .default(json!("HOURS")),
                    FormField::new(
                        "on",
                        "Qué días",
                        FormKind::select(&[
                            ("all", "Todos"),
                            ("week", "Días de la semana"),
                            ("month", "Días del mes"),
                        ]),
                    )
                    .default(json!("all")),
                    FormField::new(
                        "week",
                        "Días de la semana",
                        FormKind::multi(&[
                            ("Monday", "Lunes"),
                            ("Tuesday", "Martes"),
                            ("Wednesday", "Miércoles"),
                            ("Thursday", "Jueves"),
                            ("Friday", "Viernes"),
                            ("Saturday", "Sábado"),
                            ("Sunday", "Domingo"),
                        ]),
                    )
                    .when("on", &["week"]),
                    FormField::new(
                        "month",
                        "Días del mes",
                        FormKind::MultiSelect { options: month },
                    )
                    .when("on", &["month"]),
                    FormField::new("start_date", "Desde el día", FormKind::Date),
                ],
            )
            .rows(Vec::new()),
        ),
        FormSection::new("handler", "Acción").field(
            FormField::new("handler", "Acción (HANDLER)", FormKind::code("sql")).help(
                "Un bloque BEGIN … END (si falta, DBine lo agrega). Vacío: el evento no hace nada.",
            ),
        ),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id", FormKind::Text).read_only(true),
            FormField::new("raw_condition", "Condición en el servidor", FormKind::Text)
                .read_only(true),
        ]));
    }
    out
}

fn schedule_rows(e: &Event) -> Vec<BTreeMap<String, Value>> {
    e.schedules
        .iter()
        .map(|s| {
            values(vec![
                ("name", json!(s.name)),
                (
                    "window",
                    json!(if s.end.is_some() { "between" } else { "start" }),
                ),
                ("start_time", json!(time_text(s.start))),
                (
                    "end_time",
                    s.end.map_or(Value::Null, |t| json!(time_text(t))),
                ),
                ("every", s.every.map_or(Value::Null, |n| json!(n))),
                ("unit", json!(s.unit)),
                (
                    "on",
                    json!(if !s.week.is_empty() {
                        "week"
                    } else if !s.month.is_empty() {
                        "month"
                    } else {
                        "all"
                    }),
                ),
                ("week", json!(s.week)),
                (
                    "month",
                    json!(s.month.iter().map(i64::to_string).collect::<Vec<_>>()),
                ),
                (
                    "start_date",
                    s.start_date.map_or(Value::Null, |d| json!(date_value(d))),
                ),
            ])
        })
        .collect()
}

fn condition_rows(e: &Event) -> Vec<BTreeMap<String, Value>> {
    e.conditions
        .iter()
        .map(|(c, o, v)| {
            let n: Value = v
                .parse::<i64>()
                .map(|n| json!(n))
                .or_else(|_| v.parse::<f64>().map(|f| json!(f)))
                .unwrap_or_else(|_| json!(v));
            values(vec![
                ("condition", json!(c)),
                ("op", json!(o)),
                ("value", n),
            ])
        })
        .collect()
}

pub(super) fn values_of(e: &Event) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(e.name)),
        ("enabled", json!(e.enabled)),
        ("location", json!(e.location)),
        ("comment", jtext(e.comment.as_deref())),
        ("mode", json!(e.mode)),
        (
            "event_type",
            jtext((!e.event_type.is_empty()).then_some(e.event_type.as_str())),
        ),
        ("handler", jtext(e.handler.as_deref())),
    ])
}

/// The form with its lists' rows.
fn with_rows(mut sections: Vec<FormSection>, e: &Event) -> Vec<FormSection> {
    for s in &mut sections {
        for l in &mut s.lists {
            l.rows = match l.key.as_str() {
                "schedules" => schedule_rows(e),
                "conditions" => condition_rows(e),
                _ => Vec::new(),
            };
        }
    }
    sections
}

fn types(src: &mut dyn JobSource) -> Vec<(String, String)> {
    let listed: Vec<String> = src
        .query("SELECT name FROM SYS.SYSEVENTTYPE ORDER BY name", &[])
        .map(|s| {
            (0..s.rows.len())
                .filter_map(|r| s.get(r, "name"))
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_else(|e| {
            tracing::debug!("odbc scheduler: SYSEVENTTYPE failed: {e}");
            Vec::new()
        });
    let names = if listed.is_empty() {
        TYPES.iter().map(|t| t.to_string()).collect()
    } else {
        listed
    };
    names.into_iter().map(|n| (n.clone(), n)).collect()
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let choices = BTreeMap::from([("event_type".to_string(), types(src))]);
    let Some(id) = id else {
        let e = Event {
            mode: "schedule".into(),
            enabled: true,
            schedules: vec![Sched {
                unit: "HOURS".into(),
                ..Sched::default()
            }],
            ..Event::default()
        };
        return Ok(ObjectForm {
            sections: with_rows(sections(false, true), &e),
            values: values_of(&e),
            choices,
            ..ObjectForm::default()
        });
    };
    let (n, e) = read(src, id)?;
    let mut v = values_of(&e);
    v.insert("id".into(), json!(n.to_string()));
    v.insert("raw_condition".into(), jtext(e.raw_condition.as_deref()));
    let mut notes = vec![RECREATE.to_string()];
    if e.raw_condition.is_some() {
        notes.insert(0, UNPARSED.to_string());
    }
    Ok(ObjectForm {
        sections: with_rows(sections(true, e.raw_condition.is_none()), &e),
        values: v,
        choices,
        notes,
        read_only: false,
    })
}

// ------------------------------------------------------------------ values

/// A name for the SQL: one line, at most 128 characters.
fn ident_text(v: Vals, k: &str, label: &str) -> Result<Option<String>> {
    optional(v, k, label, 128)
}

fn sched_from(row: Vals, i: usize) -> Result<Sched> {
    let at = |label: &str| format!("Calendario {}: «{label}»", i + 1);
    let window = choice(
        row,
        "window",
        &at("Horario"),
        &["start", "between"],
        "start",
    )?;
    let start = time(row, "start_time", &at("Hora de inicio"))?.unwrap_or(0);
    let end = if window == "between" {
        Some(
            time(row, "end_time", &at("Hora de fin"))?
                .ok_or_else(|| bad(format!("Falta {}.", at("Hora de fin"))))?,
        )
    } else {
        None
    };
    let mut s = Sched {
        name: ident_text(row, "name", &at("Nombre"))?.unwrap_or_default(),
        start,
        end,
        every: row.int("every", &at("Cada"), 1, 1_000_000)?,
        unit: choice(row, "unit", &at("Unidad"), UNITS, "HOURS")?,
        week: Vec::new(),
        month: Vec::new(),
        start_date: date(row, "start_date", &at("Desde el día"))?,
    };
    match choice(row, "on", &at("Qué días"), &["all", "week", "month"], "all")?.as_str() {
        "week" => {
            for d in row.strings("week") {
                let day = WEEKDAYS
                    .iter()
                    .find(|w| w.eq_ignore_ascii_case(&d))
                    .ok_or_else(|| bad(format!("«{}» no es un día de la semana.", short(&d))))?;
                if !s.week.iter().any(|x| x == day) {
                    s.week.push(day.to_string());
                }
            }
            if s.week.is_empty() {
                return Err(bad(format!(
                    "{}: elegí al menos un día de la semana.",
                    at("Qué días")
                )));
            }
        }
        "month" => {
            for d in row.strings("month") {
                let n = d
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (0..=31).contains(n))
                    .ok_or_else(|| bad(format!("«{}» no es un día del mes.", short(&d))))?;
                s.month.push(n);
            }
            s.month.sort_unstable();
            s.month.dedup();
            if s.month.is_empty() {
                return Err(bad(format!(
                    "{}: elegí al menos un día del mes.",
                    at("Qué días")
                )));
            }
        }
        _ => {}
    }
    Ok(s)
}

/// The event the form describes, checked. `types`: the server's system
/// event types.
pub(super) fn from_values(values: &FormValues, types: &[(String, String)]) -> Result<Event> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre", 128)?;
    let mode = choice(
        v,
        "mode",
        "Qué lo dispara",
        &["schedule", "type", "manual"],
        "schedule",
    )?;
    let location = match v.opt("location") {
        None => String::new(),
        Some(_) => choice(v, "location", "Dónde se dispara (AT)", LOCATIONS, "")?,
    };
    let mut e = Event {
        name: name.clone(),
        mode: mode.clone(),
        enabled: v.bool_or("enabled", true),
        location,
        comment: code_text(v, "comment", "Comentario", 32_767)?,
        ..Event::default()
    };
    if let Some(h) = code_text(v, "handler", "Acción (HANDLER)", 1_000_000)? {
        let starts = h.get(..5).is_some_and(|b| b.eq_ignore_ascii_case("BEGIN"))
            && h[5..].starts_with(|c: char| c.is_whitespace());
        e.handler = Some(if starts {
            h
        } else {
            format!("BEGIN\n{h}\nEND")
        });
    }
    match mode.as_str() {
        "type" => {
            let t = required(v, "event_type", "Evento del sistema", 128)?;
            e.event_type = types
                .iter()
                .map(|(x, _)| x.as_str())
                .find(|x| x.eq_ignore_ascii_case(&t))
                .filter(|x| x.bytes().all(|b| b.is_ascii_alphanumeric() || b == b'_'))
                .map(str::to_string)
                .ok_or_else(|| bad(format!("«{}» no es un evento del sistema.", short(&t))))?;
            for (i, row) in rows(values, "conditions").into_iter().enumerate() {
                let at = |label: &str| format!("Condición {}: «{label}»", i + 1);
                let c = choice(row, "condition", &at("Condición"), CONDITIONS, "")?;
                let op = choice(row, "op", &at("Operador"), OPS, "<")?;
                let raw = row.text("value");
                let value = raw
                    .parse::<f64>()
                    .ok()
                    .filter(|f| f.is_finite())
                    .map(|_| raw.clone())
                    .ok_or_else(|| bad(format!("{} tiene que ser un número.", at("Valor"))))?;
                e.conditions.push((c, op, value));
            }
        }
        "schedule" => {
            let list = rows(values, "schedules");
            if list.is_empty() {
                return Err(bad("Agregá al menos un calendario."));
            }
            for (i, row) in list.into_iter().enumerate() {
                e.schedules.push(sched_from(row, i)?);
            }
            if e.schedules.len() == 1 && e.schedules[0].name.is_empty() {
                e.schedules[0].name = name;
            }
            let mut seen: Vec<String> = Vec::new();
            for s in &e.schedules {
                if s.name.is_empty() {
                    return Err(bad("Con más de un calendario, cada uno lleva su nombre."));
                }
                if seen.iter().any(|x| x.eq_ignore_ascii_case(&s.name)) {
                    return Err(bad(format!(
                        "El calendario «{}» está repetido.",
                        short(&s.name)
                    )));
                }
                seen.push(s.name.clone());
            }
        }
        _ => {}
    }
    Ok(e)
}

// ------------------------------------------------------------------ scripts

fn time_lit(secs: i64) -> String {
    format!("'{}'", time_text(secs))
}

/// A schedule's spec, its name included.
pub(super) fn spec(s: &Sched) -> String {
    let mut out = q(&s.name);
    match s.end {
        Some(e) => out.push_str(&format!(
            " BETWEEN {} AND {}",
            time_lit(s.start),
            time_lit(e)
        )),
        None => out.push_str(&format!(" START TIME {}", time_lit(s.start))),
    }
    if let Some(n) = s.every {
        out.push_str(&format!(" EVERY {n} {}", s.unit));
    }
    if !s.week.is_empty() {
        let d: Vec<String> = s.week.iter().map(|d| format!("'{d}'")).collect();
        out.push_str(&format!(" ON ({})", d.join(", ")));
    } else if !s.month.is_empty() {
        let d: Vec<String> = s.month.iter().map(i64::to_string).collect();
        out.push_str(&format!(" ON ({})", d.join(", ")));
    }
    if let Some(d) = s.start_date {
        out.push_str(&format!(" START DATE '{}'", date_value(d)));
    }
    out
}

fn where_clause(e: &Event) -> Option<String> {
    if e.conditions.is_empty() {
        return None;
    }
    let c: Vec<String> = e
        .conditions
        .iter()
        .map(|(c, o, v)| format!("event_condition('{c}') {o} {v}"))
        .collect();
    Some(c.join(" AND "))
}

pub(super) fn create_statement(e: &Event) -> String {
    let mut out = format!("CREATE EVENT {}", q(&e.name));
    match e.mode.as_str() {
        "schedule" => {
            let specs: Vec<String> = e.schedules.iter().map(spec).collect();
            out.push_str(&format!("\nSCHEDULE {}", specs.join(",\n         ")));
        }
        "type" => {
            out.push_str(&format!("\nTYPE {}", e.event_type));
            if let Some(w) = where_clause(e).or_else(|| e.raw_condition.clone()) {
                out.push_str(&format!("\nWHERE {w}"));
            }
        }
        _ => {}
    }
    if !e.enabled {
        out.push_str("\nDISABLE");
    }
    if !e.location.is_empty() {
        out.push_str(&format!("\nAT {}", e.location));
    }
    if let Some(h) = &e.handler {
        out.push_str(&format!("\nHANDLER\n{h}"));
    }
    out
}

fn comment_statement(name: &str, c: Option<&str>) -> String {
    format!(
        "COMMENT ON EVENT {} IS {}",
        q(name),
        c.map(sqla_lit).unwrap_or_else(|| "NULL".into())
    )
}

fn new_plan(comment: &str) -> Plan {
    let mut p = Plan::new(comment);
    p.go = true;
    p
}

fn lookup(name: String) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let s = src.query(
            "SELECT event_id FROM SYS.SYSEVENT WHERE event_name = ?",
            &[&name],
        )?;
        Ok(int(s.get(0, "event_id")).map(|n| n.to_string()))
    })
}

fn create_into(p: &mut Plan, e: &Event) {
    p.exec(create_statement(e));
    if let Some(c) = &e.comment {
        p.exec(comment_statement(&e.name, Some(c)));
    }
    p.lookup = Some(lookup(e.name.clone()));
}

pub(super) fn create_plan(e: &Event) -> Plan {
    let mut p = new_plan(&format!("Evento «{}» de SQL Anywhere", e.name));
    create_into(&mut p, e);
    p
}

pub(super) fn alter_plan(n: u64, cur: &Event, e: &Event) -> Plan {
    if e.name != cur.name || e.location != cur.location {
        let mut p = new_plan(&format!("Evento «{}» de SQL Anywhere", cur.name));
        for line in RECREATE.split("; ") {
            p.comment(line);
        }
        let mut e = e.clone();
        if e.mode == "type" && e.conditions.is_empty() && cur.mode == "type" {
            e.raw_condition = cur.raw_condition.clone();
        }
        p.exec(format!("DROP EVENT {}", q(&cur.name)));
        create_into(&mut p, &e);
        return p;
    }
    let alter = |what: String| format!("ALTER EVENT {} {what}", q(&cur.name));
    let mut steps: Vec<String> = Vec::new();
    // What fires it: the old firing goes first.
    if cur.mode == "schedule" {
        for s in &cur.schedules {
            let kept = e.mode == "schedule" && e.schedules.iter().any(|x| x.name == s.name);
            if !kept {
                steps.push(alter(format!("DELETE SCHEDULE {}", q(&s.name))));
            }
        }
    }
    if cur.mode == "type" && e.mode != "type" {
        steps.push(alter("DELETE TYPE".into()));
    }
    if e.mode == "type" {
        let new_where = where_clause(e);
        let keep_raw = cur.raw_condition.is_some() && e.conditions.is_empty();
        let where_changed = !keep_raw && new_where != where_clause(cur);
        let type_changed = cur.mode != "type" || cur.event_type != e.event_type;
        let mut clause = String::new();
        if type_changed {
            clause.push_str(&format!("TYPE {}", e.event_type));
        }
        if where_changed {
            if !clause.is_empty() {
                clause.push(' ');
            }
            clause.push_str(&format!(
                "WHERE {}",
                new_where.unwrap_or_else(|| "NULL".into())
            ));
        }
        if !clause.is_empty() {
            steps.push(alter(clause));
        }
    }
    if e.mode == "schedule" {
        for s in &e.schedules {
            match cur.schedules.iter().find(|x| x.name == s.name) {
                Some(old) if cur.mode == "schedule" => {
                    if old != s {
                        steps.push(alter(format!("MODIFY SCHEDULE {}", spec(s))));
                    }
                }
                _ => steps.push(alter(format!("ADD SCHEDULE {}", spec(s)))),
            }
        }
    }
    if e.enabled != cur.enabled {
        steps.push(alter(if e.enabled { "ENABLE" } else { "DISABLE" }.into()));
    }
    let norm = |h: &Option<String>| h.as_deref().map(str::trim).map(str::to_string);
    if norm(&e.handler) != norm(&cur.handler) {
        steps.push(match &e.handler {
            Some(h) => alter(format!("HANDLER\n{h}")),
            None => alter("DELETE HANDLER".into()),
        });
    }
    if !same(&e.comment, &cur.comment) {
        steps.push(comment_statement(&cur.name, e.comment.as_deref()));
    }
    if steps.is_empty() {
        return Plan::nothing(&cur.name, n.to_string());
    }
    let mut p = new_plan(&format!("Evento «{}» de SQL Anywhere", cur.name));
    for s in steps {
        p.exec(s);
    }
    p.id = Some(n.to_string());
    p
}

pub(super) fn plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let types = types(src);
    let e = from_values(values, &types)?;
    match id {
        None => Ok(create_plan(&e)),
        Some(id) => {
            let (n, cur) = read(src, id)?;
            Ok(alter_plan(n, &cur, &e))
        }
    }
}

pub(super) fn drop_plan(name: &str) -> Plan {
    let mut p = new_plan(&format!("Borra el evento «{name}» de SQL Anywhere"));
    p.exec(format!("DROP EVENT {}", q(name)));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (_, e) = read(src, id)?;
    Ok(drop_plan(&e.name))
}
