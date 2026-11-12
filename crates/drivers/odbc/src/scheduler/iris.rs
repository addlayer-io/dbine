//! InterSystems IRIS and Caché: the Task Manager's tasks (`%SYS.Task`),
//! shown whole and read only. A task is created, changed, run, suspended
//! and deleted with `%SYS.Task`'s ObjectScript methods, which check the
//! task class and its settings, work out the next run and ask for the
//! `%Admin_Task` resource; IRIS doesn't publish them as SQL procedures,
//! and writing the `%SYS.Task` table with SQL would skip all of that. So
//! the designer shows every property of a task, and the changes are made
//! in the Management Portal (or `^TASKMGR`).

use super::*;
use crate::jobs::iris::{day_secs, note, when, LIST, NOT_VISIBLE, ONE};
use crate::jobs::{int, int_id, not_found};
use dbine_driver::{FormField, FormKind, FormSection, SchedulerKind};
use serde_json::json;

pub(super) const NO_WRITE: &str = "IRIS crea, cambia y borra sus tareas con los métodos de %SYS.Task (ObjectScript), que validan la clase de la tarea, calculan la próxima ejecución y piden el recurso %Admin_Task: no están publicados como procedimientos SQL, y escribir la tabla %SYS.Task por SQL se saltearía esas validaciones. Se cambian desde el Portal de Administración o con ^TASKMGR.";
pub(super) const NO_CLEAR: &str = "IRIS borra el historial de sus tareas con su propia tarea de purga (los días se fijan en el Portal de Administración): no tiene una sentencia SQL para borrarlo.";

pub(super) fn kind(kind: &str) -> Result<()> {
    (kind.trim() == "job")
        .then_some(())
        .ok_or_else(|| bad_kind(kind))
}

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    if let Err(e) = src.query(LIST, &[]) {
        return match note(&e) {
            Some(n) => Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![n.to_string()],
            }),
            None => Err(e),
        };
    }
    Ok(SchedulerInfo {
        kinds: vec![SchedulerKind::new("job", "Tareas", "tarea").abilities(false, false, false)],
        logs: false,
        notes: vec![NO_WRITE.to_string()],
    })
}

/// The properties the form shows, in order: column, kind of value.
const TIME_COLS: &[&str] = &["DailyStartTime", "DailyEndTime", "TimeNextScheduled"];
const WHEN_COLS: &[&str] = &[
    "LastStarted",
    "LastFinished",
    "LastSchedule",
    "StartDate",
    "EndDate",
    "DayNextScheduled",
];
const BOOL_COLS: &[&str] = &[
    "Suspended",
    "RescheduleOnStart",
    "OpenOutputFile",
    "IsBatch",
    "DeleteAfterRun",
];

pub(super) fn sections() -> Vec<FormSection> {
    let ro = |f: FormField| f.read_only(true);
    vec![
        FormSection::new("general", "General").fields(vec![
            ro(FormField::new("Name", "Nombre", FormKind::Text)),
            ro(FormField::new(
                "Description",
                "Descripción",
                FormKind::Textarea,
            )),
            ro(FormField::new("TaskClass", "Clase", FormKind::Text)),
            ro(FormField::new("NameSpace", "Namespace", FormKind::Text)),
            ro(FormField::new("RunAsUser", "Ejecuta como", FormKind::Text)),
            ro(FormField::new("Type", "Tipo", FormKind::Text)),
            ro(FormField::new("Priority", "Prioridad", FormKind::Text)),
            ro(FormField::new("Suspended", "Suspendida", FormKind::Bool)),
            ro(FormField::new(
                "Settings",
                "Configuración de la clase",
                FormKind::Textarea,
            )),
        ]),
        FormSection::new("schedule", "Programación").fields(vec![
            ro(FormField::new(
                "TimePeriod",
                "Frecuencia",
                FormKind::select(&[
                    ("0", "Diaria"),
                    ("1", "Semanal"),
                    ("2", "Mensual"),
                    ("3", "Mensual, un día de la semana"),
                    ("4", "Después de otra tarea"),
                    ("5", "A pedido"),
                ]),
            )),
            ro(FormField::new("TimePeriodEvery", "Cada", FormKind::Text)),
            ro(FormField::new("TimePeriodDay", "Días", FormKind::Text)),
            ro(FormField::new(
                "DailyFrequency",
                "En el día",
                FormKind::select(&[("0", "Una vez"), ("1", "Varias veces")]),
            )),
            ro(FormField::new(
                "DailyFrequencyTime",
                "Unidad en el día",
                FormKind::select(&[("0", "Minutos"), ("1", "Horas")]),
            )),
            ro(FormField::new(
                "DailyIncrement",
                "Cada (en el día)",
                FormKind::Text,
            )),
            ro(FormField::new(
                "DailyStartTime",
                "Hora de inicio",
                FormKind::Time,
            )),
            ro(FormField::new(
                "DailyEndTime",
                "Hora de fin",
                FormKind::Time,
            )),
            ro(FormField::new("StartDate", "Desde", FormKind::DateTime)),
            ro(FormField::new("EndDate", "Hasta", FormKind::DateTime)),
            ro(FormField::new(
                "RunAfterGUID",
                "Corre después de la tarea",
                FormKind::Text,
            )),
            ro(FormField::new(
                "RescheduleOnStart",
                "Reprogramar al arrancar",
                FormKind::Bool,
            )),
            ro(FormField::new(
                "Expires",
                "Vence si no corre a tiempo",
                FormKind::Text,
            )),
            ro(FormField::new(
                "ExpiresDays",
                "Días de gracia",
                FormKind::Text,
            )),
            ro(FormField::new(
                "ExpiresHours",
                "Horas de gracia",
                FormKind::Text,
            )),
            ro(FormField::new(
                "ExpiresMinutes",
                "Minutos de gracia",
                FormKind::Text,
            )),
        ]),
        FormSection::new("output", "Salida y avisos").fields(vec![
            ro(FormField::new(
                "OpenOutputFile",
                "Guarda la salida en un archivo",
                FormKind::Bool,
            )),
            ro(FormField::new(
                "OutputDirectory",
                "Carpeta de salida",
                FormKind::Text,
            )),
            ro(FormField::new(
                "OutputFilename",
                "Archivo de salida",
                FormKind::Text,
            )),
            ro(FormField::new(
                "EmailOnCompletion",
                "Avisar al terminar",
                FormKind::Text,
            )),
            ro(FormField::new(
                "EmailOnError",
                "Avisar si falla",
                FormKind::Text,
            )),
            ro(FormField::new(
                "EmailOnExpiration",
                "Avisar si vence",
                FormKind::Text,
            )),
            ro(FormField::new(
                "EmailOutput",
                "Mandar la salida por correo",
                FormKind::Text,
            )),
            ro(FormField::new("IsBatch", "Por lotes", FormKind::Bool)),
            ro(FormField::new(
                "DeleteAfterRun",
                "Borrar después de correr",
                FormKind::Bool,
            )),
        ]),
        FormSection::new("state", "Estado").fields(vec![
            ro(FormField::new("id", "Id", FormKind::Text)),
            ro(FormField::new(
                "LastStarted",
                "Último inicio",
                FormKind::DateTime,
            )),
            ro(FormField::new(
                "LastFinished",
                "Último fin",
                FormKind::DateTime,
            )),
            ro(FormField::new(
                "LastSchedule",
                "Última programada",
                FormKind::DateTime,
            )),
            ro(FormField::new(
                "DayNextScheduled",
                "Próxima ejecución",
                FormKind::DateTime,
            )),
            ro(FormField::new(
                "TimeNextScheduled",
                "Hora de la próxima",
                FormKind::Time,
            )),
            ro(FormField::new(
                "Status",
                "Estado de la última",
                FormKind::Text,
            )),
            ro(FormField::new("Error", "Último error", FormKind::Textarea)),
            ro(FormField::new(
                "RunningJobNumber",
                "Proceso que la corre",
                FormKind::Text,
            )),
            ro(FormField::new("JobGUID", "GUID", FormKind::Text)),
            ro(FormField::new(
                "other",
                "Otras propiedades",
                FormKind::Textarea,
            )),
        ]),
    ]
}

/// The task's values, by the form's keys (the table's columns).
pub(super) fn values_of(s: &Set, n: u64) -> BTreeMap<String, Value> {
    let mut v = BTreeMap::new();
    let keys: Vec<String> = sections()
        .iter()
        .flat_map(|sec| sec.fields.iter().map(|f| f.key.clone()))
        .collect();
    for k in &keys {
        let raw = s.get(0, k);
        let value = if k == "id" {
            json!(n.to_string())
        } else if k == "other" {
            continue;
        } else if TIME_COLS.contains(&k.as_str()) {
            day_secs(raw).map_or(Value::Null, |t| json!(time_text(t)))
        } else if WHEN_COLS.contains(&k.as_str()) {
            when(raw).map_or(Value::Null, |ms| json!(dt_value(ms)))
        } else if BOOL_COLS.contains(&k.as_str()) {
            raw.map_or(Value::Null, |b| json!(int(Some(b)).unwrap_or(0) != 0))
        } else {
            jtext(raw)
        };
        v.insert(k.clone(), value);
    }
    // What this version has that the form doesn't name.
    let other: Vec<String> = s
        .cols
        .iter()
        .enumerate()
        .filter(|(_, c)| {
            let c = c.trim();
            !c.eq_ignore_ascii_case("JOB_ID")
                && !c.eq_ignore_ascii_case("ID")
                && !keys.iter().any(|k| k.eq_ignore_ascii_case(c))
        })
        .filter_map(|(i, c)| s.at(0, i).map(|x| format!("{}: {x}", c.trim())))
        .collect();
    v.insert(
        "other".into(),
        if other.is_empty() {
            Value::Null
        } else {
            json!(other.join("\n"))
        },
    );
    v
}

pub(super) fn form(src: &mut dyn JobSource, id: Option<&str>) -> Result<ObjectForm> {
    let Some(id) = id else {
        return Err(Error::Unsupported(NO_WRITE.into()));
    };
    let n = int_id(id)?;
    let s = src.query(ONE, &[&n.to_string()])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let mut notes = vec![NO_WRITE.to_string()];
    if s.get(0, "NameSpace").is_none() && s.get(0, "Name").is_none() {
        notes.push(NOT_VISIBLE.to_string());
    }
    Ok(ObjectForm {
        sections: sections(),
        values: values_of(&s, n),
        choices: BTreeMap::new(),
        notes,
        read_only: true,
    })
}
