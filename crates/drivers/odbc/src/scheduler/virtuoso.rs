//! OpenLink Virtuoso: the scheduler's events managed whole by INSERT,
//! UPDATE and DELETE on `DB.DBA.SYS_SCHEDULED_EVENT`, the way Conductor
//! does it: the name, the first start, the interval in minutes, the SQL it
//! runs, and on servers that have them the disabled flag and the mail
//! notice. A save is one transaction. Only dba reads and writes the table.
//! Texts are literals with quotes and backslashes doubled.

use super::*;
use crate::jobs::virtuoso::{denied, off, one, read as read_all, vlit, NO_ACCESS, OFF};
use crate::jobs::{int, name_id, parse_ts, plain};
use dbine_driver::{FormField, FormKind, FormSection, SchedulerKind};
use serde_json::json;

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

const OLD_VERSION: &str = "Esta versión de Virtuoso no tiene SE_DISABLED ni el aviso por correo: un evento no se deshabilita (con intervalo 0 no corre).";
const TABLE: &str = "DB.DBA.SYS_SCHEDULED_EVENT";

/// An event as the form edits it. The start: the server's local time, ms.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Event {
    pub(super) name: String,
    pub(super) start: Option<i64>,
    /// Minutes; 0: it never runs.
    pub(super) interval: i64,
    pub(super) sql: String,
    pub(super) enabled: bool,
    pub(super) notify: bool,
    pub(super) notify_to: Option<String>,
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    match read_all(src, Some("")) {
        Ok(_) => {}
        Err(e) if denied(&e) => {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![NO_ACCESS.to_string()],
            })
        }
        Err(e) => return Err(e),
    }
    let mut notes = Vec::new();
    if off(src) {
        notes.push(OFF.to_string());
    }
    Ok(SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Eventos programados", "evento programado")
                .abilities(true, true, true),
        ],
        logs: false,
        notes,
    })
}

fn read(src: &mut dyn JobSource, id: &str) -> Result<(Event, Set, bool)> {
    let (s, full) = one(src, id)?;
    let e = Event {
        name: s.get(0, "SE_NAME").unwrap_or_default().to_string(),
        start: s.get(0, "SE_START").and_then(parse_ts),
        interval: int(s.get(0, "SE_INTERVAL")).unwrap_or(0),
        sql: s
            .rows
            .first()
            .and_then(|r| {
                s.cols
                    .iter()
                    .position(|c| c.eq_ignore_ascii_case("SE_SQL"))
                    .and_then(|i| r.get(i).cloned().flatten())
            })
            .unwrap_or_default(),
        enabled: !full || int(s.get(0, "SE_DISABLED")).unwrap_or(0) == 0,
        notify: full && int(s.get(0, "SE_ENABLE_NOTIFY")) == Some(1),
        notify_to: if full {
            s.get(0, "SE_NOTIFY").map(str::to_string)
        } else {
            None
        },
    };
    Ok((e, s, full))
}

/// The server has the later columns (best effort: yes when it can't tell).
fn full(src: &mut dyn JobSource) -> bool {
    match read_all(src, Some("")) {
        Ok((_, full)) => full,
        Err(_) => true,
    }
}

pub(super) fn sections(existing: bool, full: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text).required(),
            FormField::new("enabled", "Habilitado", FormKind::Bool)
                .default(json!(true))
                .read_only(!full),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            FormField::new("start", "Inicio", FormKind::DateTime)
                .help("Desde cuándo corre. Vacío al crearlo: ahora."),
            FormField::new("interval", "Cada (minutos)", FormKind::number(Some(0.0), None))
                .required()
                .default(json!(60))
                .help("El planificador lo corre cuando pasaron estos minutos desde que terminó la vez anterior. 0: no corre."),
        ]),
        FormSection::new("sql", "SQL").field(
            FormField::new("sql", "SQL", FormKind::code("sql"))
                .required()
                .help("Lo que corre: una sentencia o la llamada a un procedimiento."),
        ),
        FormSection::new("notify", "Aviso").fields(vec![
            FormField::new("notify", "Avisar por correo si falla", FormKind::Bool)
                .read_only(!full),
            FormField::new("notify_to", "Correo", FormKind::Text)
                .when("notify", &["true"])
                .read_only(!full),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("last_completed", "Última ejecución completada", FormKind::Text)
                .read_only(true),
            FormField::new("last_error", "Último error", FormKind::Textarea).read_only(true),
        ]));
    }
    out
}

pub(super) fn values_of(e: &Event) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(e.name)),
        ("enabled", json!(e.enabled)),
        (
            "start",
            e.start.map_or(Value::Null, |ms| json!(dt_value(ms))),
        ),
        ("interval", json!(e.interval)),
        ("sql", json!(e.sql)),
        ("notify", json!(e.notify)),
        ("notify_to", jtext(e.notify_to.as_deref())),
    ])
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        let full = full(src);
        let e = Event {
            interval: 60,
            enabled: true,
            ..Event::default()
        };
        return Ok(ObjectForm {
            sections: sections(false, full),
            values: values_of(&e),
            notes: if full {
                Vec::new()
            } else {
                vec![OLD_VERSION.to_string()]
            },
            ..ObjectForm::default()
        });
    };
    let (e, s, full) = read(src, id)?;
    let mut v = values_of(&e);
    v.insert(
        "last_completed".into(),
        jtext(s.get(0, "SE_LAST_COMPLETED")),
    );
    v.insert("last_error".into(), jtext(s.get(0, "SE_LAST_ERROR")));
    Ok(ObjectForm {
        sections: sections(true, full),
        values: v,
        notes: if full {
            Vec::new()
        } else {
            vec![OLD_VERSION.to_string()]
        },
        ..ObjectForm::default()
    })
}

/// The event the form describes, checked.
pub(super) fn from_values(values: &FormValues) -> Result<Event> {
    let v = Vals(&values.values);
    let name = required(v, "name", "Nombre", 250)?;
    let sql = code_text(v, "sql", "SQL", 1_000_000)?.ok_or_else(|| bad("Falta «SQL»."))?;
    let notify = v.bool_or("notify", false);
    let notify_to = optional(v, "notify_to", "Correo", 250)?;
    if notify && notify_to.is_none() {
        return Err(bad("Falta «Correo» para avisar por correo."));
    }
    if notify_to
        .as_deref()
        .is_some_and(|m| !m.contains('@') || m.contains(char::is_whitespace))
    {
        return Err(bad("«Correo» no es una dirección de correo."));
    }
    Ok(Event {
        name,
        start: datetime(v, "start", "Inicio")?,
        interval: v
            .int("interval", "Cada (minutos)", 0, i64::from(i32::MAX))?
            .ok_or_else(|| bad("Falta «Cada (minutos)»."))?,
        sql,
        enabled: v.bool_or("enabled", true),
        notify,
        notify_to,
    })
}

fn start_lit(ms: Option<i64>) -> String {
    ms.map(|ms| format!("CAST('{}' AS DATETIME)", plain(ms)))
        .unwrap_or_else(|| "now()".into())
}

fn columns(e: &Event, full: bool) -> Vec<(&'static str, String)> {
    let mut cols = vec![
        ("SE_NAME", vlit(&e.name)),
        ("SE_START", start_lit(e.start)),
        ("SE_INTERVAL", e.interval.to_string()),
        ("SE_SQL", vlit(&e.sql)),
    ];
    if full {
        cols.push(("SE_DISABLED", if e.enabled { "0" } else { "1" }.into()));
        cols.push(("SE_ENABLE_NOTIFY", if e.notify { "1" } else { "0" }.into()));
        cols.push((
            "SE_NOTIFY",
            e.notify_to
                .as_deref()
                .map(vlit)
                .unwrap_or_else(|| "NULL".into()),
        ));
    }
    cols
}

pub(super) fn create_plan(e: &Event, full: bool) -> Plan {
    let cols = columns(e, full);
    let mut p = Plan::new(&format!("Evento programado «{}» de Virtuoso", e.name));
    p.atomic = true;
    p.exec(format!(
        "INSERT INTO {TABLE} ({})\nVALUES ({})",
        cols.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", "),
        cols.iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    ));
    p.id = Some(e.name.clone());
    p
}

pub(super) fn alter_plan(cur: &Event, e: &Event, full: bool) -> Plan {
    let old = columns(cur, full);
    let mut sets: Vec<String> = Vec::new();
    for (c, v) in columns(e, full) {
        let was = old.iter().find(|(o, _)| *o == c).map(|(_, w)| w.as_str());
        let changed = match c {
            // An empty start keeps the one the server has.
            "SE_START" => e.start.is_some() && e.start != cur.start,
            "SE_SQL" => e.sql.trim() != cur.sql.trim(),
            _ => was != Some(v.as_str()),
        };
        if changed {
            sets.push(format!("{c} = {v}"));
        }
    }
    if sets.is_empty() {
        return Plan::nothing(&cur.name, cur.name.clone());
    }
    let mut p = Plan::new(&format!("Evento programado «{}» de Virtuoso", cur.name));
    p.atomic = true;
    p.exec(format!(
        "UPDATE {TABLE}\n   SET {}\n WHERE SE_NAME = {}",
        sets.join(",\n       "),
        vlit(&cur.name)
    ));
    p.id = Some(e.name.clone());
    p
}

pub(super) fn plan(src: &mut dyn JobSource, id: Option<&str>, values: &FormValues) -> Result<Plan> {
    let e = from_values(values)?;
    match id {
        None => {
            let full = full(src);
            let taken = read_all(src, Some(&e.name)).is_ok_and(|(s, _)| !s.rows.is_empty());
            if taken {
                return Err(bad(format!(
                    "Ya hay un evento programado «{}».",
                    short(&e.name)
                )));
            }
            Ok(create_plan(&e, full))
        }
        Some(id) => {
            name_id(id)?;
            let (cur, _, full) = read(src, id)?;
            if e.name != cur.name
                && read_all(src, Some(&e.name)).is_ok_and(|(s, _)| !s.rows.is_empty())
            {
                return Err(bad(format!(
                    "Ya hay un evento programado «{}».",
                    short(&e.name)
                )));
            }
            Ok(alter_plan(&cur, &e, full))
        }
    }
}

pub(super) fn drop_plan(name: &str) -> Plan {
    let mut p = Plan::new(&format!("Borra el evento programado «{name}» de Virtuoso"));
    p.atomic = true;
    p.exec(format!(
        "DELETE FROM {TABLE} WHERE SE_NAME = {}",
        vlit(name)
    ));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let (e, _, _) = read(src, id)?;
    Ok(drop_plan(&e.name))
}
