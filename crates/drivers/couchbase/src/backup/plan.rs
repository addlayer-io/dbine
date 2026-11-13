//! Backup plans: the services a backup takes and the tasks (backups,
//! merges, prunes) with their schedules. `POST /plan/{name}` creates one,
//! `PUT /plan/{name}` replaces it whole (the service has no partial
//! update), `DELETE /plan/{name}` deletes it. The plans that come with
//! Couchbase (`default: true`) are read-only, and so is a plan an active
//! repository uses: the service refuses to change or delete it.

use super::{
    bad, comment_text, form_time, new_name, parse_time, rows, shown, Call, Change, Ctx, Vals,
};
use crate::jobs::s as field;
use crate::{encode, CbSession};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use reqwest::Method;
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

const SYSTEM: &str = "Es un plan que viene con Couchbase: no se puede cambiar ni borrar.";
const NOT_FOUND: &str = "El plan de backup no existe.";
const RENAME: &str = "El servicio de backup no permite cambiar el nombre de un plan.";

/// The services a plan may back up: `(key, Spanish label)`.
pub(crate) const SERVICES: &[&str] = &["data", "gsi", "views", "ft", "eventing", "cbas", "query"];
const PERIODS: &[&str] = &[
    "MINUTES",
    "HOURS",
    "DAYS",
    "WEEKS",
    "MONDAY",
    "TUESDAY",
    "WEDNESDAY",
    "THURSDAY",
    "FRIDAY",
    "SATURDAY",
    "SUNDAY",
];
/// Most tasks a plan holds (the console's limit).
const MAX_TASKS: usize = 24;
/// Most runs a frequency spaces (the console's limit).
const MAX_FREQUENCY: i64 = 200;
const MAX_DESCRIPTION: usize = 140;

/// A task of a plan as the service keeps it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Task {
    pub(crate) name: String,
    /// `BACKUP`, `MERGE` or `PRUNE`.
    pub(crate) kind: String,
    pub(crate) full_backup: bool,
    pub(crate) frequency: i64,
    pub(crate) period: String,
    /// `HH:MM`; `None` for minutes.
    pub(crate) time: Option<String>,
    /// A merge's `(offset_start, offset_end)` in days.
    pub(crate) merge: Option<(i64, i64)>,
}

/// A plan as the service keeps it.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct PlanDef {
    pub(crate) name: String,
    pub(crate) description: String,
    /// `None`: every service.
    pub(crate) services: Option<Vec<String>>,
    pub(crate) tasks: Vec<Task>,
    /// It comes with Couchbase.
    pub(crate) default: bool,
}

pub(crate) fn task_of(t: &Value) -> Option<Task> {
    let sch = t.get("schedule");
    Some(Task {
        name: field(t, "name")?,
        kind: field(t, "task_type")?.to_ascii_uppercase(),
        full_backup: t.get("full_backup").and_then(Value::as_bool) == Some(true),
        frequency: sch
            .and_then(|x| x.get("frequency"))
            .and_then(Value::as_i64)
            .unwrap_or(1),
        period: sch
            .and_then(|x| field(x, "period"))
            .unwrap_or_default()
            .to_ascii_uppercase(),
        time: sch.and_then(|x| field(x, "time")),
        merge: t.get("merge_options").and_then(|m| {
            Some((
                m.get("offset_start")?.as_i64()?,
                m.get("offset_end")?.as_i64()?,
            ))
        }),
    })
}

pub(crate) fn plan_of(p: &Value) -> Option<PlanDef> {
    Some(PlanDef {
        name: field(p, "name")?,
        description: p
            .get("description")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string(),
        services: p.get("services").and_then(Value::as_array).map(|a| {
            a.iter()
                .filter_map(Value::as_str)
                .map(str::to_string)
                .collect()
        }),
        tasks: p
            .get("tasks")
            .and_then(Value::as_array)
            .map(|a| a.iter().filter_map(task_of).collect())
            .unwrap_or_default(),
        default: p.get("default").and_then(Value::as_bool) == Some(true),
    })
}

fn task_json(t: &Task) -> Value {
    let mut schedule = Map::new();
    schedule.insert("job_type".into(), json!(t.kind));
    schedule.insert("frequency".into(), json!(t.frequency));
    schedule.insert("period".into(), json!(t.period));
    if let Some(time) = &t.time {
        schedule.insert("time".into(), json!(time));
    }
    let mut o = Map::new();
    o.insert("name".into(), json!(t.name));
    o.insert("task_type".into(), json!(t.kind));
    o.insert("full_backup".into(), json!(t.full_backup));
    o.insert("schedule".into(), Value::Object(schedule));
    if let Some((a, b)) = t.merge {
        o.insert(
            "merge_options".into(),
            json!({"offset_start": a, "offset_end": b}),
        );
    }
    Value::Object(o)
}

/// The body of `POST` / `PUT /plan/{name}`.
pub(crate) fn plan_json(p: &PlanDef) -> Value {
    json!({
        "name": p.name,
        "description": p.description,
        "services": p.services,
        "tasks": p.tasks.iter().map(task_json).collect::<Vec<_>>(),
    })
}

/// A plan's services with every one taken as `None`, and the order the
/// service's list has: two plans that back up the same compare equal.
fn services_key(s: &Option<Vec<String>>) -> Option<Vec<String>> {
    let v = s.as_ref()?;
    let mut v: Vec<String> = SERVICES
        .iter()
        .filter(|k| v.iter().any(|x| x == *k))
        .map(|k| k.to_string())
        .collect();
    if v.len() == SERVICES.len() {
        return None;
    }
    v.dedup();
    Some(v)
}

/// What differs between two plans, in Spanish, for the script's comment.
pub(crate) fn changes(old: &PlanDef, new: &PlanDef) -> Vec<&'static str> {
    let mut out = Vec::new();
    if old.description != new.description {
        out.push("descripción");
    }
    if services_key(&old.services) != services_key(&new.services) {
        out.push("servicios");
    }
    if old.tasks != new.tasks {
        out.push("tareas");
    }
    out
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// A plan's id is its name: built-in ones start with `_`.
pub(crate) fn parse_id(id: &str) -> Result<String> {
    let t = id.trim();
    let ok = !t.is_empty()
        && t.len() <= 50
        && t.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-');
    if ok {
        Ok(t.to_string())
    } else {
        Err(bad(format!(
            "«{}» no es un id de plan de backup de Couchbase",
            shown(id)
        )))
    }
}

async fn read(s: &CbSession, id: &str) -> Result<PlanDef> {
    let name = parse_id(id)?;
    let v = s
        .backup_call(Method::GET, &format!("/plan/{}", encode(&name)), None)
        .await
        .map_err(|e| match e {
            Error::Query(m) if m.contains("not found") || m.contains("does not exist") => {
                Error::Query(NOT_FOUND.into())
            }
            e => e,
        })?;
    plan_of(&v).ok_or_else(|| Error::Query(NOT_FOUND.into()))
}

/// Every plan, as the service lists them.
pub(crate) async fn all(s: &CbSession) -> Result<Vec<PlanDef>> {
    let v = s.backup_call(Method::GET, "/plan", None).await?;
    Ok(v.as_array()
        .map(|a| a.iter().filter_map(plan_of).collect())
        .unwrap_or_default())
}

/// The active repositories that use the plan (the service refuses to
/// change or delete it while there is one).
async fn users(s: &CbSession, name: &str) -> Result<Vec<String>> {
    let v = s
        .backup_call(Method::GET, "/cluster/self/repository/active", None)
        .await?;
    Ok(v.as_array()
        .into_iter()
        .flatten()
        .filter(|r| field(r, "plan_name").as_deref() == Some(name))
        .filter_map(|r| field(r, "id"))
        .collect())
}

fn in_use(users: &[String]) -> String {
    format!(
        "Lo usan los repositorios activos {}: el servicio de backup no permite cambiarlo ni borrarlo. Para cambiar las tareas de un repositorio, creá otro plan y elegilo en el repositorio.",
        users
            .iter()
            .map(|u| format!("«{u}»"))
            .collect::<Vec<_>>()
            .join(", ")
    )
}

pub(crate) async fn items(s: &CbSession) -> Result<SchedulerList> {
    // The plans the active repositories use: the service refuses to change
    // or delete them, as it does the built-in ones.
    let used: Vec<String> = s
        .backup_call(Method::GET, "/cluster/self/repository/active", None)
        .await?
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| field(r, "plan_name"))
        .collect();
    let items = all(s)
        .await?
        .into_iter()
        .map(|p| item_of(p, &used))
        .collect();
    Ok(SchedulerList {
        items,
        notes: Vec::new(),
    })
}

/// A plan as the tree lists it: a built-in one, or one an active
/// repository uses, is neither changed nor deleted (the service refuses).
pub(crate) fn item_of(p: PlanDef, used: &[String]) -> SchedulerItem {
    let fixed = p.default || used.contains(&p.name);
    SchedulerItem {
        id: p.name.clone(),
        description: Some(p.description.clone()).filter(|d| !d.is_empty()),
        name: p.name,
        enabled: None,
        system: p.default,
        can_edit: fixed.then_some(false),
        can_delete: fixed.then_some(false),
        actions: None,
        run_from_step: None,
        clear_history: None,
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn task_fields(c: &Ctx) -> Vec<FormField> {
    let types = if c.v8() {
        FormKind::select(&[
            ("BACKUP", "Backup"),
            ("MERGE", "Merge (une backups)"),
            ("PRUNE", "Prune (borra los backups vencidos)"),
        ])
    } else {
        FormKind::select(&[("BACKUP", "Backup"), ("MERGE", "Merge (une backups)")])
    };
    vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .placeholder("backup_diario")
            .help("Letras, dígitos, guion y guion bajo, hasta 50."),
        FormField::new("task_type", "Tipo", types)
            .required()
            .default(json!("BACKUP")),
        FormField::new("full_backup", "Backup completo", FormKind::Bool)
            .when("task_type", &["BACKUP"])
            .default(json!(false))
            .help("Si no, incremental (el primero de un repositorio es completo igual)."),
        FormField::new(
            "frequency",
            "Cada",
            FormKind::number(Some(1.0), Some(MAX_FREQUENCY as f64)),
        )
        .required()
        .default(json!(1))
        .help("Cuántos períodos entre una ejecución y la siguiente."),
        FormField::new(
            "period",
            "Período",
            FormKind::select(&[
                ("MINUTES", "Minutos"),
                ("HOURS", "Horas"),
                ("DAYS", "Días"),
                ("WEEKS", "Semanas"),
                ("MONDAY", "Lunes"),
                ("TUESDAY", "Martes"),
                ("WEDNESDAY", "Miércoles"),
                ("THURSDAY", "Jueves"),
                ("FRIDAY", "Viernes"),
                ("SATURDAY", "Sábado"),
                ("SUNDAY", "Domingo"),
            ]),
        )
        .required()
        .default(json!("DAYS")),
        FormField::new("time", "Hora", FormKind::Time)
            .when(
                "period",
                &[
                    "HOURS",
                    "DAYS",
                    "WEEKS",
                    "MONDAY",
                    "TUESDAY",
                    "WEDNESDAY",
                    "THURSDAY",
                    "FRIDAY",
                    "SATURDAY",
                    "SUNDAY",
                ],
            )
            .default(json!("22:00:00"))
            .help("Hora del servidor, en horas y minutos. Cada hora o cada varias horas solo cuentan los minutos (00:30: a la media hora)."),
        FormField::new("offset_start", "Desde (días atrás)", FormKind::number(Some(0.0), None))
            .when("task_type", &["MERGE"])
            .help("Une los backups hechos entre «Hasta» y «Desde» días atrás (0: hoy). Vacíos los dos: los que el servicio elija."),
        FormField::new("offset_end", "Hasta (días atrás)", FormKind::number(Some(0.0), None))
            .when("task_type", &["MERGE"]),
    ]
}

fn task_row(t: &Task) -> BTreeMap<String, Value> {
    let mut r = BTreeMap::new();
    r.insert("name".into(), json!(t.name));
    r.insert("task_type".into(), json!(t.kind));
    r.insert("full_backup".into(), json!(t.full_backup));
    r.insert("frequency".into(), json!(t.frequency));
    r.insert("period".into(), json!(t.period));
    r.insert(
        "time".into(),
        t.time
            .as_deref()
            .map(form_time)
            .map_or(Value::Null, Value::from),
    );
    r.insert(
        "offset_start".into(),
        t.merge.map_or(Value::Null, |(a, _)| json!(a)),
    );
    r.insert(
        "offset_end".into(),
        t.merge.map_or(Value::Null, |(_, b)| json!(b)),
    );
    r
}

pub(crate) fn build_form(c: &Ctx, plan: Option<&PlanDef>, users: &[String]) -> ObjectForm {
    let existing = plan.is_some();
    let mut f = ObjectForm::default();
    let services_kind = FormKind::multi(&[
        ("data", "Datos"),
        ("gsi", "Índices"),
        ("views", "Vistas"),
        ("ft", "Búsqueda"),
        ("eventing", "Eventing"),
        ("cbas", "Analytics"),
        ("query", "Funciones de usuario (Query)"),
    ]);
    f.sections = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .read_only(existing)
                .placeholder("plan_diario")
                .help("Letras, dígitos, guion y guion bajo, hasta 50; empieza con letra o dígito. No se puede cambiar después."),
            FormField::new("description", "Descripción", FormKind::Textarea)
                .help("Hasta 140 caracteres."),
            FormField::new("services", "Servicios", services_kind)
                .default(json!(SERVICES))
                .help("Lo que copian los backups del plan, además de los datos de los buckets elegidos."),
        ]),
        FormSection::new("tasks", "Tareas").list(
            FormList::new("tasks", "Tareas", "name", task_fields(c))
                .rows(plan.map(|p| p.tasks.iter().map(task_row).collect()).unwrap_or_default())
                .min_rows(1),
        ),
    ];
    match plan {
        Some(p) => {
            f.values.insert("name".into(), json!(p.name));
            f.values.insert("description".into(), json!(p.description));
            f.values.insert(
                "services".into(),
                json!(p
                    .services
                    .clone()
                    .unwrap_or_else(|| SERVICES.iter().map(|x| x.to_string()).collect())),
            );
            if p.default {
                f.read_only = true;
                f.notes.push(SYSTEM.into());
            } else if !users.is_empty() {
                f.read_only = true;
                f.notes.push(in_use(users));
            }
        }
        None => {
            f.values.insert("name".into(), json!(""));
            f.values.insert("description".into(), json!(""));
            f.values.insert("services".into(), json!(SERVICES));
        }
    }
    if c.v8() {
        f.notes.push(
            "Una tarea Prune borra los backups vencidos según la retención de cada repositorio."
                .into(),
        );
    }
    f
}

pub(crate) async fn form(s: &CbSession, c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    match id {
        None => Ok(build_form(c, None, &[])),
        Some(id) => {
            let p = read(s, id).await?;
            let users = if p.default {
                Vec::new()
            } else {
                users(s, &p.name).await?
            };
            Ok(build_form(c, Some(&p), &users))
        }
    }
}

// ---------------------------------------------------------------------------
// Values → a plan
// ---------------------------------------------------------------------------

/// A task name: `^[A-Za-z0-9_-]{1,50}$`.
fn task_name(n: &str) -> bool {
    !n.is_empty()
        && n.len() <= 50
        && n.bytes()
            .all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'-')
}

fn task_from(c: &Ctx, i: usize, r: Vals) -> Result<Task> {
    let n = i + 1;
    let name = r
        .opt("name")
        .ok_or_else(|| bad(format!("La tarea {n} necesita un nombre.")))?;
    if !task_name(&name) {
        return Err(bad(format!(
            "«{}»: el nombre de una tarea lleva letras, dígitos, guion y guion bajo, hasta 50.",
            shown(&name)
        )));
    }
    let kind = r.text("task_type").to_ascii_uppercase();
    let kind = match kind.as_str() {
        "" | "BACKUP" => "BACKUP",
        "MERGE" => "MERGE",
        "PRUNE" if c.v8() => "PRUNE",
        "PRUNE" => {
            return Err(bad(
                "Las tareas Prune son de Couchbase Server 8.0 en adelante.".into(),
            ))
        }
        other => {
            return Err(bad(format!(
                "«{}» no es un tipo de tarea (Backup, Merge o Prune).",
                shown(other)
            )))
        }
    };
    let frequency = r
        .int("frequency", "Cada", 1, MAX_FREQUENCY)?
        .ok_or_else(|| bad(format!("La tarea «{name}» necesita «Cada».")))?;
    let period = r.text("period").to_ascii_uppercase();
    if !PERIODS.contains(&period.as_str()) {
        return Err(bad(format!(
            "La tarea «{name}» necesita un período (minutos, horas, días, semanas o un día de la semana)."
        )));
    }
    let time = if period == "MINUTES" {
        None
    } else {
        let t = r.opt("time").ok_or_else(|| {
            bad(format!(
                "La tarea «{name}» necesita la hora a la que corre."
            ))
        })?;
        Some(parse_time(&t).ok_or_else(|| {
            bad(format!(
                "«{}» no es una hora válida para la tarea «{name}» (HH:MM, sin segundos).",
                shown(&t)
            ))
        })?)
    };
    let merge = if kind == "MERGE" {
        let a = r.int("offset_start", "Desde (días atrás)", 0, 100_000)?;
        let b = r.int("offset_end", "Hasta (días atrás)", 0, 100_000)?;
        match (a, b) {
            (None, None) => None,
            (Some(a), Some(b)) if a <= b => Some((a, b)),
            (Some(_), Some(_)) => {
                return Err(bad(format!(
                    "En la tarea «{name}», «Desde» no puede ser más días atrás que «Hasta»."
                )))
            }
            _ => {
                return Err(bad(format!(
                    "La tarea «{name}» necesita «Desde» y «Hasta» juntos, o ninguno."
                )))
            }
        }
    } else {
        None
    };
    Ok(Task {
        name,
        kind: kind.to_string(),
        full_backup: kind == "BACKUP" && r.bool("full_backup"),
        frequency,
        period,
        time,
        merge,
    })
}

/// The plan the form's values describe, checked.
pub(crate) fn plan_from(c: &Ctx, name: &str, values: &FormValues) -> Result<PlanDef> {
    let v = Vals(&values.values);
    let description = v.text("description");
    if description.chars().count() > MAX_DESCRIPTION {
        return Err(bad(format!(
            "La descripción de un plan tiene hasta {MAX_DESCRIPTION} caracteres."
        )));
    }
    let services = match v.strings("services") {
        None => None,
        Some(list) => {
            for x in &list {
                if !SERVICES.contains(&x.as_str()) {
                    return Err(bad(format!(
                        "«{}» no es un servicio que el plan pueda copiar.",
                        shown(x)
                    )));
                }
            }
            if list.is_empty() {
                return Err(bad("Elegí al menos un servicio para el plan.".into()));
            }
            services_key(&Some(list))
        }
    };
    let rows = rows(values, "tasks");
    if rows.is_empty() {
        return Err(bad("El plan necesita al menos una tarea.".into()));
    }
    if rows.len() > MAX_TASKS {
        return Err(bad(format!("Un plan tiene hasta {MAX_TASKS} tareas.")));
    }
    let mut tasks: Vec<Task> = Vec::new();
    for (i, r) in rows.into_iter().enumerate() {
        let t = task_from(c, i, r)?;
        if tasks.iter().any(|x| x.name == t.name) {
            return Err(bad(format!("Hay dos tareas con el nombre «{}».", t.name)));
        }
        tasks.push(t);
    }
    if !tasks.iter().any(|t| t.kind == "BACKUP") {
        return Err(bad("El plan necesita al menos una tarea de backup.".into()));
    }
    Ok(PlanDef {
        name: name.to_string(),
        description,
        services,
        tasks,
        default: false,
    })
}

pub(crate) fn create_change(p: &PlanDef) -> Change {
    Change {
        comments: vec![format!("Crea el plan de backup «{}».", p.name)],
        calls: vec![Call::new(
            Method::POST,
            format!("/plan/{}", encode(&p.name)),
            Some(plan_json(p)),
        )],
        warnings: Vec::new(),
        undo: None,
        id: Some(p.name.clone()),
    }
}

pub(crate) fn update_change(old: &PlanDef, new: &PlanDef) -> Change {
    let what = changes(old, new);
    if what.is_empty() {
        return Change::nothing(&old.name, old.name.clone());
    }
    Change {
        comments: vec![
            format!(
                "Cambia el plan de backup «{}»: {}.",
                old.name,
                what.join(", ")
            ),
            "El servicio reemplaza el plan entero con lo que se le manda.".into(),
        ],
        calls: vec![Call::new(
            Method::PUT,
            format!("/plan/{}", encode(&old.name)),
            Some(plan_json(new)),
        )],
        warnings: Vec::new(),
        undo: None,
        id: Some(old.name.clone()),
    }
}

pub(crate) async fn change(
    s: &CbSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Change> {
    let v = Vals(&values.values);
    match id {
        None => {
            let name = v
                .opt("name")
                .ok_or_else(|| bad("El plan necesita un nombre.".into()))?;
            if !new_name(&name) {
                return Err(bad(format!(
                    "«{}»: el nombre de un plan lleva letras, dígitos, guion y guion bajo, hasta 50, y empieza con letra o dígito.",
                    shown(&name)
                )));
            }
            let p = plan_from(c, &name, values)?;
            if all(s).await?.iter().any(|x| x.name == name) {
                return Err(bad(format!("Ya hay un plan «{name}».")));
            }
            Ok(create_change(&p))
        }
        Some(id) => {
            let old = read(s, id).await?;
            if old.default {
                return Err(Error::Query(SYSTEM.into()));
            }
            if v.opt("name").is_some_and(|n| n != old.name) {
                return Err(bad(RENAME.into()));
            }
            let users = users(s, &old.name).await?;
            if !users.is_empty() {
                return Err(Error::Query(in_use(&users)));
            }
            let new = plan_from(c, &old.name, values)?;
            Ok(update_change(&old, &new))
        }
    }
}

pub(crate) fn delete_change(p: &PlanDef) -> Change {
    Change {
        comments: vec![format!(
            "Borra el plan de backup «{}».",
            comment_text(&p.name)
        )],
        calls: vec![Call::new(
            Method::DELETE,
            format!("/plan/{}", encode(&p.name)),
            None,
        )],
        warnings: Vec::new(),
        undo: None,
        id: None,
    }
}

pub(crate) async fn delete(s: &CbSession, id: &str) -> Result<Change> {
    let p = read(s, id).await?;
    if p.default {
        return Err(Error::Query(SYSTEM.into()));
    }
    let users = users(s, &p.name).await?;
    if !users.is_empty() {
        return Err(Error::Query(in_use(&users)));
    }
    Ok(delete_change(&p))
}
