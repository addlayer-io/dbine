//! pgAgent in the designer: jobs (general, steps, schedules with their
//! exceptions) and job classes, as pgAdmin's dialogs edit them, written to
//! pgAgent's tables (`pgagent.pga_*`) the way pgAdmin does.
//!
//! - A job is created in one `DO` block (job, steps, schedules, exceptions)
//!   and changed in another with only what differs: rows updated, inserted
//!   and deleted by their ids, which come back with the rows and have to be
//!   this job's. `jobchanged` is set on every change, and a form older than
//!   the job is refused.
//! - pgAgent runs a job's steps in the order of their names (`ORDER BY
//!   jstname, jstid`, in the database's collation): the list's order is
//!   checked against it, so what the designer shows is what runs.
//! - A remote step's connection string comes to the form without its
//!   password, which is a secret field: kept as the server has it unless the
//!   user types another or asks to remove it, masked in the script shown.
//! - Schedules: pgAgent's minute, hour, weekday, month-day (and last day)
//!   and month flags, each "every" when none is picked, between a start and
//!   an optional end. Exceptions (a day, a time, or both) are their own list
//!   that names its schedule.

use super::{
    bad, code_lit, databases, name_of, pick, plain, rows, shown, ts_lit, utc_sql, valid_date,
    valid_time, with_current, Ctx, NewId, Plan, Vals,
};
use crate::catalog::{comment_text, lit};
use crate::jobs::pgagent::without_password;
use crate::jobs::{dollar_tagged, flag, text, JobId};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerKind, SchedulerList, SECRET_MASK,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashSet};

const NO_WRITE: &str = "Tu usuario no puede cambiar los jobs de pgAgent: necesita INSERT, UPDATE y DELETE en las tablas del esquema pgagent.";
const UNREADABLE: &str = "Tu usuario no puede leer los jobs de pgAgent: necesita USAGE en el esquema pgagent y SELECT en sus tablas.";
const STALE: &str = "El job cambió en el servidor desde que se abrió el formulario: volvé a abrirlo para no pisar esos cambios.";
const ORDER_NOTE: &str = "pgAgent ejecuta los pasos en el orden alfabético de sus nombres: el orden de la lista tiene que ser ese (por ejemplo, empezá los nombres con 01, 02…).";
const LAST_DAY: &str = "Último día";

/// What the user may do with pgAgent's tables.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    pub(crate) read: bool,
    pub(crate) job_insert: bool,
    pub(crate) job_update: bool,
    pub(crate) job_delete: bool,
    /// INSERT, UPDATE and DELETE on the steps, schedules and exceptions.
    pub(crate) parts: bool,
    pub(crate) class_insert: bool,
    pub(crate) class_update: bool,
    pub(crate) class_delete: bool,
    pub(crate) log_delete: bool,
    pub(crate) database: String,
}

impl Caps {
    pub(crate) fn may_create(&self) -> bool {
        self.read && self.job_insert && self.parts
    }
    pub(crate) fn may_edit(&self) -> bool {
        self.read && self.job_update && self.parts
    }
    pub(crate) fn may_delete(&self) -> bool {
        self.read && self.job_delete
    }
    pub(crate) fn notes(&self) -> Vec<String> {
        if !self.read {
            vec![UNREADABLE.into()]
        } else if !self.may_edit() {
            vec![NO_WRITE.into()]
        } else {
            Vec::new()
        }
    }
}

/// What the user may do with pgAgent's tables, by their oids: naming
/// them (`'pgagent.pga_job'`) fails without USAGE on the schema.
fn caps_sql() -> String {
    let rel = |name: &str, privileges: &str| {
        format!(
            "coalesce((SELECT has_table_privilege(c.oid, '{privileges}') FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'pgagent' AND c.relname = '{name}'), false)"
        )
    };
    let write = "INSERT, UPDATE, DELETE";
    format!(
        "SELECT current_database() AS db,
                has_schema_privilege('pgagent', 'USAGE') AND {job} AND {step} AND {sched} AND {exc} AND {class} AS read,
                {job_insert} AS job_insert, {job_update} AS job_update, {job_delete} AS job_delete,
                {step_w} AND {sched_w} AND {exc_w} AS parts,
                {class_insert} AS class_insert, {class_update} AS class_update, {class_delete} AS class_delete,
                {log_delete} AS log_delete",
        job = rel("pga_job", "SELECT"),
        step = rel("pga_jobstep", "SELECT"),
        sched = rel("pga_schedule", "SELECT"),
        exc = rel("pga_exception", "SELECT"),
        class = rel("pga_jobclass", "SELECT"),
        job_insert = rel("pga_job", "INSERT"),
        job_update = rel("pga_job", "UPDATE"),
        job_delete = rel("pga_job", "DELETE"),
        step_w = rel("pga_jobstep", write),
        sched_w = rel("pga_schedule", write),
        exc_w = rel("pga_exception", write),
        class_insert = rel("pga_jobclass", "INSERT"),
        class_update = rel("pga_jobclass", "UPDATE"),
        class_delete = rel("pga_jobclass", "DELETE"),
        log_delete = rel("pga_joblog", "DELETE"),
    )
}

pub(crate) async fn caps(s: &PgSession) -> Result<Caps> {
    let rows = s.text(&caps_sql()).await?;
    Ok(rows
        .first()
        .map(|r| {
            // INSERT/UPDATE on the steps, schedules and exceptions also
            // need their sequences (serial ids).
            Caps {
                read: flag(r, "read"),
                job_insert: flag(r, "job_insert"),
                job_update: flag(r, "job_update"),
                job_delete: flag(r, "job_delete"),
                parts: flag(r, "parts"),
                class_insert: flag(r, "class_insert"),
                class_update: flag(r, "class_update"),
                class_delete: flag(r, "class_delete"),
                log_delete: flag(r, "log_delete"),
                database: text(r, "db").unwrap_or_default(),
            }
        })
        .unwrap_or_default())
}

fn caps_of(c: &Ctx) -> Result<&Caps> {
    c.agent.as_ref().ok_or_else(|| {
        Error::Query(
            "pgAgent no está en esta base: sus jobs están en la base donde se instaló (casi siempre postgres)".into(),
        )
    })
}

pub(crate) fn class_kind(a: &Caps) -> SchedulerKind {
    SchedulerKind::new("job_class", "Clases de job", "clase de job").abilities(
        a.class_insert,
        a.class_update,
        a.class_delete,
    )
}

/// A new job's form for another scheduler came with pgAgent's lists filled.
pub(crate) fn no_lists(values: &FormValues) -> Result<()> {
    if ["steps", "schedules", "exceptions"]
        .iter()
        .any(|l| values.lists.get(*l).is_some_and(|r| !r.is_empty()))
    {
        return Err(bad(
            "Los pasos y las programaciones son de los jobs de pgAgent: quitalos o elegí pgAgent como planificador.".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// The job as the server has it
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Job {
    pub(crate) id: i32,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) host_agent: String,
    pub(crate) enabled: bool,
    pub(crate) class_id: i32,
    pub(crate) class: String,
    pub(crate) created: String,
    pub(crate) changed: String,
    /// `jobchanged` to the microsecond, for the stale check.
    pub(crate) changed_at: String,
    pub(crate) next_run: String,
    pub(crate) last_run: String,
    pub(crate) agent: Option<String>,
    pub(crate) steps: Vec<Step>,
    pub(crate) schedules: Vec<Schedule>,
    pub(crate) exceptions: Vec<Exception>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Step {
    pub(crate) id: i32,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) enabled: bool,
    /// `s` (SQL) or `b` (batch).
    pub(crate) kind: char,
    pub(crate) code: String,
    /// As the server has it, password included.
    pub(crate) connstr: String,
    pub(crate) dbname: String,
    /// `f`, `s` or `i`.
    pub(crate) on_error: char,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Schedule {
    pub(crate) id: i32,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) enabled: bool,
    /// UTC, RFC 3339.
    pub(crate) start: String,
    pub(crate) end: Option<String>,
    pub(crate) minutes: Vec<bool>,
    pub(crate) hours: Vec<bool>,
    pub(crate) weekdays: Vec<bool>,
    pub(crate) monthdays: Vec<bool>,
    pub(crate) months: Vec<bool>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Exception {
    pub(crate) id: i32,
    pub(crate) schedule: i32,
    pub(crate) date: Option<String>,
    pub(crate) time: Option<String>,
}

/// A schedule's `{t,f,…}` array, `len` long.
pub(crate) fn bools(array: &str, len: usize) -> Vec<bool> {
    let mut out: Vec<bool> = array
        .trim()
        .trim_start_matches('{')
        .trim_end_matches('}')
        .split(',')
        .map(|v| matches!(v.trim(), "t" | "true"))
        .collect();
    out.resize(len, false);
    out
}

fn int(r: &dyn crate::jobs::Row, name: &str) -> Option<i32> {
    text(r, name).and_then(|v| v.parse().ok())
}

async fn read(s: &PgSession, n: i32) -> Result<Job> {
    let job = s
        .text(&format!(
            "SELECT j.jobid::text AS id, j.jobname, j.jobdesc, j.jobhostagent, j.jobenabled,
                    j.jobjclid::text AS class_id, c.jclname AS class,
                    {created} AS created, {changed} AS changed,
                    extract(epoch FROM j.jobchanged)::text AS changed_at,
                    {next} AS next_run, {last} AS last_run, j.jobagentid::text AS agent
             FROM pgagent.pga_job j LEFT JOIN pgagent.pga_jobclass c ON c.jclid = j.jobjclid
             WHERE j.jobid = {n}",
            created = utc_sql("j.jobcreated"),
            changed = utc_sql("j.jobchanged"),
            next = utc_sql("j.jobnextrun"),
            last = utc_sql("j.joblastrun"),
        ))
        .await?;
    let r = job.first().ok_or_else(|| {
        Error::Query(format!(
            "no existe el job {} (o tu usuario no lo ve)",
            JobId::Agent(n)
        ))
    })?;
    let mut out = Job {
        id: n,
        name: r.get("jobname").unwrap_or_default().to_string(),
        description: r.get("jobdesc").unwrap_or_default().to_string(),
        host_agent: r.get("jobhostagent").unwrap_or_default().to_string(),
        enabled: flag(r, "jobenabled"),
        class_id: int(r, "class_id").unwrap_or_default(),
        class: r.get("class").unwrap_or_default().to_string(),
        created: text(r, "created").unwrap_or_default(),
        changed: text(r, "changed").unwrap_or_default(),
        changed_at: text(r, "changed_at").unwrap_or_default(),
        next_run: text(r, "next_run").unwrap_or_default(),
        last_run: text(r, "last_run").unwrap_or_default(),
        agent: text(r, "agent"),
        ..Job::default()
    };
    for r in s
        .text(&format!(
            "SELECT jstid::text AS id, jstname, jstdesc, jstenabled, jstkind::text AS kind, jstcode, jstconnstr,
                    jstdbname::text AS dbname, jstonerror::text AS onerror
             FROM pgagent.pga_jobstep WHERE jstjobid = {n} ORDER BY jstname, jstid"
        ))
        .await?
    {
        out.steps.push(Step {
            id: int(&r, "id").unwrap_or_default(),
            name: r.get("jstname").unwrap_or_default().to_string(),
            description: r.get("jstdesc").unwrap_or_default().to_string(),
            enabled: flag(&r, "jstenabled"),
            kind: text(&r, "kind").and_then(|k| k.chars().next()).unwrap_or('s'),
            code: r.get("jstcode").unwrap_or_default().to_string(),
            connstr: r.get("jstconnstr").unwrap_or_default().to_string(),
            dbname: r.get("dbname").unwrap_or_default().to_string(),
            on_error: text(&r, "onerror").and_then(|k| k.chars().next()).unwrap_or('f'),
        });
    }
    for r in s
        .text(&format!(
            "SELECT jscid::text AS id, jscname, jscdesc, jscenabled, {start} AS start, {end} AS until,
                    jscminutes::text AS minutes, jschours::text AS hours, jscweekdays::text AS weekdays,
                    jscmonthdays::text AS monthdays, jscmonths::text AS months
             FROM pgagent.pga_schedule WHERE jscjobid = {n} ORDER BY jscname, jscid",
            start = utc_sql("jscstart"),
            end = utc_sql("jscend"),
        ))
        .await?
    {
        let b = |name: &str, len: usize| bools(r.get(name).unwrap_or_default(), len);
        out.schedules.push(Schedule {
            id: int(&r, "id").unwrap_or_default(),
            name: r.get("jscname").unwrap_or_default().to_string(),
            description: r.get("jscdesc").unwrap_or_default().to_string(),
            enabled: flag(&r, "jscenabled"),
            start: text(&r, "start").unwrap_or_default(),
            end: text(&r, "until"),
            minutes: b("minutes", 60),
            hours: b("hours", 24),
            weekdays: b("weekdays", 7),
            monthdays: b("monthdays", 32),
            months: b("months", 12),
        });
    }
    for r in s
        .text(&format!(
            "SELECT e.jexid::text AS id, e.jexscid::text AS schedule, to_char(e.jexdate, 'YYYY-MM-DD') AS day,
                    left(e.jextime::text, 8) AS at
             FROM pgagent.pga_exception e JOIN pgagent.pga_schedule s ON s.jscid = e.jexscid
             WHERE s.jscjobid = {n} ORDER BY e.jexid"
        ))
        .await?
    {
        out.exceptions.push(Exception {
            id: int(&r, "id").unwrap_or_default(),
            schedule: int(&r, "schedule").unwrap_or_default(),
            date: text(&r, "day"),
            time: text(&r, "at"),
        });
    }
    Ok(out)
}

/// The job classes, by name, with their ids.
async fn classes(s: &PgSession) -> Result<Vec<(i32, String)>> {
    Ok(
        s.text("SELECT jclid::text AS id, jclname FROM pgagent.pga_jobclass ORDER BY jclid")
            .await?
            .iter()
            .filter_map(|r| Some((int(r, "id")?, r.get("jclname")?.to_string())))
            .collect(),
    )
}

/// The connected agents' hosts, when the user may read them.
async fn agents(s: &PgSession) -> Vec<String> {
    match s
        .text("SELECT DISTINCT jagstation FROM pgagent.pga_jobagent ORDER BY 1")
        .await
    {
        Ok(rows) => rows.iter().filter_map(|r| text(r, "jagstation")).collect(),
        Err(e) => {
            tracing::debug!("pgagent: agents: {e}");
            Vec::new()
        }
    }
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn minute_options() -> FormKind {
    FormKind::MultiSelect {
        options: (0..60)
            .map(|m| (m.to_string(), format!("{m:02}")))
            .collect(),
    }
}

fn hour_options() -> FormKind {
    FormKind::MultiSelect {
        options: (0..24)
            .map(|h| (h.to_string(), format!("{h:02}")))
            .collect(),
    }
}

fn monthday_options() -> FormKind {
    let mut options: Vec<(String, String)> =
        (1..=31).map(|d| (d.to_string(), d.to_string())).collect();
    options.push(("32".into(), LAST_DAY.into()));
    FormKind::MultiSelect { options }
}

/// The step editor: pgAdmin's step dialog.
pub(crate) fn step_fields(database: &str) -> Vec<FormField> {
    vec![
        FormField::new("name", "Nombre", FormKind::Text)
            .required()
            .help("pgAgent ejecuta los pasos en el orden alfabético de sus nombres."),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new(
            "kind",
            "Tipo",
            FormKind::select(&[
                ("sql", "SQL en una base de este servidor"),
                ("remote", "SQL en otro servidor (cadena de conexión)"),
                ("batch", "Lote (comandos del sistema operativo)"),
            ]),
        )
        .required()
        .default(json!("sql")),
        FormField::new("dbname", "Base de datos", FormKind::reference("database", false))
            .required()
            .when("kind", &["sql"])
            .default(json!(database)),
        FormField::new("connstr", "Cadena de conexión", FormKind::Text)
            .required()
            .when("kind", &["remote"])
            .placeholder("host=db2.example.com port=5432 dbname=app user=etl")
            .help("Una cadena de conexión de libpq, sin la contraseña: va en su propio campo."),
        FormField::new("password", "Contraseña", FormKind::Secret)
            .when("kind", &["remote"])
            .help("Vacía: se mantiene la que tiene guardada la cadena de conexión."),
        FormField::new("drop_password", "Quitar la contraseña guardada", FormKind::Bool)
            .when("kind", &["remote"]),
        FormField::new(
            "on_error",
            "Si falla",
            FormKind::select(&[
                ("f", "Fallar: detener el job"),
                ("s", "Marcarlo como correcto y seguir"),
                ("i", "Ignorar el error y seguir"),
            ]),
        )
        .required()
        .default(json!("f")),
        FormField::new("code", "Código", FormKind::code("sql"))
            .required()
            .when("kind", &["sql", "remote"]),
        FormField::new("code_batch", "Código", FormKind::code("shell"))
            .required()
            .when("kind", &["batch"])
            .help("Lo ejecuta el sistema operativo del equipo del agente (sh en Unix, un .bat en Windows)."),
        FormField::new("description", "Comentario", FormKind::Textarea),
    ]
}

pub(crate) fn schedule_fields() -> Vec<FormField> {
    let every = "Sin elegir ninguno: todos.";
    vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("enabled", "Habilitada", FormKind::Bool).default(json!(true)),
        FormField::new("start", "Inicio", FormKind::DateTime)
            .help("Desde cuándo rige. Vacío: ahora."),
        FormField::new("end", "Fin", FormKind::DateTime).help("Hasta cuándo rige. Vacío: sin fin."),
        FormField::new("minutes", "Minutos", minute_options()).help(every),
        FormField::new("hours", "Horas", hour_options()).help(every),
        FormField::new(
            "weekdays",
            "Días de la semana",
            FormKind::multi(&[
                ("0", "Domingo"),
                ("1", "Lunes"),
                ("2", "Martes"),
                ("3", "Miércoles"),
                ("4", "Jueves"),
                ("5", "Viernes"),
                ("6", "Sábado"),
            ]),
        )
        .help(every),
        FormField::new("monthdays", "Días del mes", monthday_options()).help(every),
        FormField::new(
            "months",
            "Meses",
            FormKind::multi(&[
                ("1", "Enero"),
                ("2", "Febrero"),
                ("3", "Marzo"),
                ("4", "Abril"),
                ("5", "Mayo"),
                ("6", "Junio"),
                ("7", "Julio"),
                ("8", "Agosto"),
                ("9", "Septiembre"),
                ("10", "Octubre"),
                ("11", "Noviembre"),
                ("12", "Diciembre"),
            ]),
        )
        .help(every),
        FormField::new("description", "Comentario", FormKind::Textarea),
    ]
}

pub(crate) fn exception_fields() -> Vec<FormField> {
    vec![
        FormField::new(
            "schedule",
            "Programación",
            FormKind::reference("list:schedules", false),
        )
        .required(),
        FormField::new("date", "Fecha", FormKind::Date)
            .help("Ese día el job no se ejecuta (a ninguna hora si no hay hora)."),
        FormField::new("time", "Hora", FormKind::Time)
            .help("A esa hora el job no se ejecuta (ningún día si no hay fecha)."),
    ]
}

/// A schedule's flags as the multi-select's values (`first` is the value
/// of the first flag).
fn picked(flags: &[bool], first: usize) -> Value {
    json!(flags
        .iter()
        .enumerate()
        .filter(|(_, b)| **b)
        .map(|(i, _)| (i + first).to_string())
        .collect::<Vec<_>>())
}

pub(crate) fn step_row(st: &Step) -> BTreeMap<String, Value> {
    let (connstr, _) = without_password(&st.connstr);
    let kind = match (st.kind, st.connstr.trim().is_empty()) {
        ('b', _) => "batch",
        (_, false) => "remote",
        _ => "sql",
    };
    let mut r = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        r.insert(k.to_string(), v);
    };
    put("step_id", json!(st.id));
    put("name", json!(st.name));
    put("enabled", json!(st.enabled));
    put("kind", json!(kind));
    put("dbname", json!(st.dbname));
    put(
        "connstr",
        json!(if kind == "remote" {
            connstr
        } else {
            String::new()
        }),
    );
    put("password", Value::Null);
    put("drop_password", json!(false));
    put("on_error", json!(st.on_error.to_string()));
    if kind == "batch" {
        put("code_batch", json!(st.code));
    } else {
        put("code", json!(st.code));
    }
    put("description", json!(st.description));
    r
}

pub(crate) fn schedule_row(sc: &Schedule) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("schedule_id".to_string(), json!(sc.id)),
        ("name".to_string(), json!(sc.name)),
        ("enabled".to_string(), json!(sc.enabled)),
        ("start".to_string(), json!(sc.start)),
        ("end".to_string(), json!(sc.end.clone().unwrap_or_default())),
        ("minutes".to_string(), picked(&sc.minutes, 0)),
        ("hours".to_string(), picked(&sc.hours, 0)),
        ("weekdays".to_string(), picked(&sc.weekdays, 0)),
        ("monthdays".to_string(), picked(&sc.monthdays, 1)),
        ("months".to_string(), picked(&sc.months, 1)),
        ("description".to_string(), json!(sc.description)),
    ])
}

pub(crate) fn exception_row(e: &Exception, schedules: &[Schedule]) -> BTreeMap<String, Value> {
    BTreeMap::from([
        ("exception_id".to_string(), json!(e.id)),
        (
            "schedule".to_string(),
            json!(schedules
                .iter()
                .find(|s| s.id == e.schedule)
                .map(|s| s.name.clone())
                .unwrap_or_default()),
        ),
        (
            "date".to_string(),
            json!(e.date.clone().unwrap_or_default()),
        ),
        (
            "time".to_string(),
            json!(e.time.clone().unwrap_or_default()),
        ),
    ])
}

pub(crate) async fn form(s: &PgSession, c: &Ctx, n: Option<i32>) -> Result<ObjectForm> {
    let caps = caps_of(c)?;
    let job = match n {
        Some(n) => Some(read(s, n).await?),
        None => None,
    };
    let classes = classes(s).await?;
    let agents = agents(s).await;
    let default_class = classes.first().map(|(_, n)| n.clone()).unwrap_or_default();
    let host_help = if agents.is_empty() {
        "Vacío: lo ejecuta cualquier agente. Ahora no hay ningún agente conectado.".to_string()
    } else {
        format!(
            "Vacío: lo ejecuta cualquier agente. Agentes conectados ahora: {}.",
            agents.join(", ")
        )
    };
    let mut general = FormSection::new("general", "General");
    if job.is_some() {
        general = general.field(FormField::new("jobid", "Id", FormKind::Text).read_only(true));
    }
    general = general.fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
        FormField::new(
            "job_class",
            "Clase",
            FormKind::reference("job_class", false),
        )
        .required()
        .default(json!(default_class)),
        FormField::new("host_agent", "Agente", FormKind::Text)
            .placeholder("servidor-etl")
            .help(&host_help),
        FormField::new("description", "Comentario", FormKind::Textarea),
    ]);
    if job.is_some() {
        general = general.fields(vec![
            FormField::new("created", "Creado", FormKind::Text).read_only(true),
            FormField::new("changed", "Modificado", FormKind::Text).read_only(true),
            FormField::new("next_run", "Próxima ejecución", FormKind::Text).read_only(true),
            FormField::new("last_run", "Última ejecución", FormKind::Text).read_only(true),
            FormField::new("agent", "Ejecutándose en el agente (PID)", FormKind::Text)
                .read_only(true),
        ]);
    }
    let mut values = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        values.insert(k.to_string(), v);
    };
    let (mut step_rows, mut schedule_rows, mut exception_rows) =
        (Vec::new(), Vec::new(), Vec::new());
    match &job {
        None => {
            put("enabled", json!(true));
            put("job_class", json!(default_class));
            put("host_agent", json!(""));
            put("description", json!(""));
        }
        Some(j) => {
            put("jobid", json!(j.id.to_string()));
            put("name", json!(j.name));
            put("enabled", json!(j.enabled));
            put("job_class", json!(j.class));
            put("host_agent", json!(j.host_agent));
            put("description", json!(j.description));
            put("created", json!(j.created));
            put("changed", json!(j.changed));
            put("next_run", json!(j.next_run));
            put("last_run", json!(j.last_run));
            put("agent", json!(j.agent.clone().unwrap_or_default()));
            // No field shows it: it comes back with the values, and a save
            // from a form older than the job is refused.
            put("changed_at", json!(j.changed_at));
            step_rows = j.steps.iter().map(step_row).collect();
            schedule_rows = j.schedules.iter().map(schedule_row).collect();
            exception_rows = j
                .exceptions
                .iter()
                .map(|e| exception_row(e, &j.schedules))
                .collect();
        }
    }
    let steps = FormSection::new("steps", "Pasos").list(
        FormList::new("steps", "Pasos", "name", step_fields(&caps.database))
            .orderable()
            .rows(step_rows),
    );
    let schedules = FormSection::new("schedules", "Programaciones")
        .list(
            FormList::new("schedules", "Programaciones", "name", schedule_fields())
                .rows(schedule_rows),
        )
        .list(
            FormList::new("exceptions", "Excepciones", "schedule", exception_fields())
                .rows(exception_rows),
        );
    let mut choices = BTreeMap::new();
    let current_class = job.as_ref().map_or("", |j| j.class.as_str());
    choices.insert(
        "job_class".to_string(),
        with_current(
            classes
                .iter()
                .map(|(_, n)| (n.clone(), n.clone()))
                .collect(),
            current_class,
        ),
    );
    let mut dbs = databases(s).await?;
    for st in job.iter().flat_map(|j| j.steps.iter()) {
        dbs = with_current(dbs, &st.dbname);
    }
    choices.insert("database".to_string(), dbs);
    let mut notes = caps.notes();
    notes.push(ORDER_NOTE.into());
    let read_only = match &job {
        Some(_) => !caps.may_edit(),
        None => !caps.may_create(),
    };
    Ok(ObjectForm {
        sections: vec![general, steps, schedules],
        values,
        choices,
        notes,
        read_only,
    })
}

// ---------------------------------------------------------------------------
// What the form asks for
// ---------------------------------------------------------------------------

/// What a remote step's password becomes.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Password {
    /// What the server has (none for a new step).
    Keep,
    Set(String),
    Drop,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct StepSpec {
    pub(crate) id: Option<i32>,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) enabled: bool,
    pub(crate) kind: char,
    pub(crate) code: String,
    /// Without the password (nor the key's passphrase).
    pub(crate) connstr: String,
    pub(crate) password: Password,
    /// The key's passphrase (`sslpassword`) typed into the string; `None`:
    /// the server's, kept; empty: removed.
    pub(crate) ssl_password: Option<String>,
    pub(crate) dbname: String,
    pub(crate) on_error: char,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ScheduleSpec {
    pub(crate) id: Option<i32>,
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) enabled: bool,
    pub(crate) start: Option<String>,
    pub(crate) end: Option<String>,
    pub(crate) minutes: Vec<bool>,
    pub(crate) hours: Vec<bool>,
    pub(crate) weekdays: Vec<bool>,
    pub(crate) monthdays: Vec<bool>,
    pub(crate) months: Vec<bool>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct ExceptionSpec {
    pub(crate) id: Option<i32>,
    /// The schedule's name in the form.
    pub(crate) schedule: String,
    pub(crate) date: Option<String>,
    pub(crate) time: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct JobSpec {
    pub(crate) name: String,
    pub(crate) description: String,
    pub(crate) host_agent: String,
    pub(crate) enabled: bool,
    pub(crate) class_id: i32,
    pub(crate) steps: Vec<StepSpec>,
    pub(crate) schedules: Vec<ScheduleSpec>,
    pub(crate) exceptions: Vec<ExceptionSpec>,
}

/// A row id the app sent back: one of `known`, or none (a new row).
fn row_id(v: Vals, key: &str, known: &[i32], what: &str, name: &str) -> Result<Option<i32>> {
    match v.0.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::String(s)) if s.trim().is_empty() => Ok(None),
        Some(x) => {
            let n = match x {
                Value::Number(n) => n.as_i64(),
                Value::String(s) => s.trim().parse::<i64>().ok(),
                _ => None,
            };
            match n.and_then(|n| i32::try_from(n).ok()) {
                Some(n) if known.contains(&n) => Ok(Some(n)),
                _ => Err(bad(format!("{what} «{}» no es de este job.", shown(name)))),
            }
        }
    }
}

/// A multi-select's flags: values `first..first+len`, each once.
fn flags(v: Vals, key: &str, label: &str, len: usize, first: usize) -> Result<Vec<bool>> {
    let mut out = vec![false; len];
    for x in v.strings(key) {
        match x.parse::<usize>() {
            Ok(i) if (first..first + len).contains(&i) => out[i - first] = true,
            _ => {
                return Err(bad(format!(
                    "«{label}»: «{}» no es un valor válido.",
                    shown(&x)
                )))
            }
        }
    }
    Ok(out)
}

/// The values of the form, checked against the job as the server has it
/// (`cur`) and the server's classes and databases.
pub(crate) fn spec(
    values: &FormValues,
    cur: Option<&Job>,
    classes: &[(i32, String)],
    databases: &[(String, String)],
) -> Result<JobSpec> {
    let v = Vals(&values.values);
    let name = name_of(v, "name", "Nombre")?;
    let class = v.opt("job_class").unwrap_or_default();
    let class_id = classes
        .iter()
        .find(|(_, n)| *n == class)
        .map(|(id, _)| *id)
        .ok_or_else(|| {
            bad(format!(
                "La clase «{}» no existe en pgAgent.",
                shown(&class)
            ))
        })?;
    let host_agent = v.text("host_agent");
    plain(&host_agent, "Agente")?;
    let step_ids: Vec<i32> = cur.map_or(Vec::new(), |j| j.steps.iter().map(|s| s.id).collect());
    let mut steps = Vec::new();
    let mut seen = HashSet::new();
    for r in rows(values, "steps") {
        let name = name_of(r, "name", "Nombre del paso")?;
        if !seen.insert(name.clone()) {
            return Err(bad(format!(
                "Hay dos pasos llamados «{}»: cada paso necesita su nombre.",
                shown(&name)
            )));
        }
        let id = row_id(r, "step_id", &step_ids, "El paso", &name)?;
        let kind = r.opt("kind").unwrap_or_else(|| "sql".into());
        let (kind, code_key) = match kind.as_str() {
            "sql" | "remote" => ('s', "code"),
            "batch" => ('b', "code_batch"),
            other => {
                return Err(bad(format!(
                    "«{}» no es un tipo de paso de pgAgent.",
                    shown(other)
                )))
            }
        };
        let remote = r.text("kind") == "remote";
        let code = r.raw(code_key);
        if code.trim().is_empty() {
            return Err(bad(format!("El paso «{}» no tiene código.", shown(&name))));
        }
        let (dbname, connstr, password, ssl_password) = if kind == 'b' {
            (String::new(), String::new(), Password::Keep, None)
        } else if remote {
            let typed = r.text("connstr");
            if typed.is_empty() {
                return Err(bad(format!(
                    "El paso «{}» no tiene cadena de conexión.",
                    shown(&name)
                )));
            }
            if typed.chars().any(char::is_control) {
                return Err(bad(format!(
                    "La cadena de conexión del paso «{}» no puede tener saltos de línea.",
                    shown(&name)
                )));
            }
            // A password typed into the string itself is taken as typed;
            // so is the key's passphrase.
            let (connstr, inline) = split_password(&typed);
            let password = match (r.secret("password"), inline) {
                (Some(p), _) => Password::Set(p),
                (None, Some(p)) => Password::Set(p),
                (None, None) if r.bool("drop_password") => Password::Drop,
                (None, None) => Password::Keep,
            };
            (String::new(), connstr, password, ssl_password(&typed))
        } else {
            let db = r.opt("dbname").unwrap_or_default();
            let current = id
                .and_then(|i| cur.and_then(|j| j.steps.iter().find(|s| s.id == i)))
                .map(|s| s.dbname.as_str());
            let db = if current == Some(db.as_str()) {
                db
            } else {
                pick(&db, databases, |n| {
                    format!("La base «{n}» del paso no existe en este servidor (o no acepta conexiones).")
                })?
                .to_string()
            };
            (db, String::new(), Password::Keep, None)
        };
        let on_error = match r.opt("on_error").as_deref().unwrap_or("f") {
            "f" => 'f',
            "s" => 's',
            "i" => 'i',
            other => {
                return Err(bad(format!(
                    "«{}» no es una acción de pgAgent para un paso que falla.",
                    shown(other)
                )))
            }
        };
        steps.push(StepSpec {
            id,
            name,
            description: r.raw("description"),
            enabled: r.bool_or("enabled", true),
            kind,
            code,
            connstr,
            password,
            ssl_password,
            dbname,
            on_error,
        });
    }
    let schedule_ids: Vec<i32> =
        cur.map_or(Vec::new(), |j| j.schedules.iter().map(|s| s.id).collect());
    let mut schedules = Vec::new();
    let mut seen = HashSet::new();
    for r in rows(values, "schedules") {
        let name = name_of(r, "name", "Nombre de la programación")?;
        if !seen.insert(name.clone()) {
            return Err(bad(format!(
                "Hay dos programaciones llamadas «{}»: cada una necesita su nombre (las excepciones las nombran).",
                shown(&name)
            )));
        }
        let id = row_id(r, "schedule_id", &schedule_ids, "La programación", &name)?;
        let start = super::datetime_field(r, "start", "Inicio")?;
        let end = super::datetime_field(r, "end", "Fin")?;
        schedules.push(ScheduleSpec {
            id,
            description: r.raw("description"),
            enabled: r.bool_or("enabled", true),
            start,
            end,
            minutes: flags(r, "minutes", "Minutos", 60, 0)?,
            hours: flags(r, "hours", "Horas", 24, 0)?,
            weekdays: flags(r, "weekdays", "Días de la semana", 7, 0)?,
            monthdays: flags(r, "monthdays", "Días del mes", 32, 1)?,
            months: flags(r, "months", "Meses", 12, 1)?,
            name,
        });
    }
    let exception_ids: Vec<i32> =
        cur.map_or(Vec::new(), |j| j.exceptions.iter().map(|e| e.id).collect());
    let mut exceptions = Vec::new();
    let mut seen = HashSet::new();
    for r in rows(values, "exceptions") {
        let schedule = r.text("schedule");
        if !schedules.iter().any(|s| s.name == schedule) {
            return Err(bad(format!(
                "La excepción es de la programación «{}», que el job no tiene.",
                shown(&schedule)
            )));
        }
        let id = row_id(r, "exception_id", &exception_ids, "La excepción", &schedule)?;
        let date = r.opt("date");
        let time = r
            .opt("time")
            .map(|t| if t.len() == 5 { format!("{t}:00") } else { t });
        if date.is_none() && time.is_none() {
            return Err(bad(format!(
                "Una excepción de «{}» no tiene fecha ni hora: necesita al menos una.",
                shown(&schedule)
            )));
        }
        if let Some(d) = &date {
            if !valid_date(d) {
                return Err(bad(format!(
                    "«Fecha»: «{}» no es una fecha válida (AAAA-MM-DD).",
                    shown(d)
                )));
            }
        }
        if let Some(t) = &time {
            if !valid_time(t) {
                return Err(bad(format!(
                    "«Hora»: «{}» no es una hora válida (HH:MM:SS).",
                    shown(t)
                )));
            }
        }
        // pgAgent keeps one exception per date and time in the whole server.
        if !seen.insert((date.clone(), time.clone())) {
            return Err(bad(
                "Hay dos excepciones con la misma fecha y hora: pgAgent guarda una sola por fecha y hora.".into(),
            ));
        }
        exceptions.push(ExceptionSpec {
            id,
            schedule,
            date,
            time,
        });
    }
    Ok(JobSpec {
        name,
        description: v.raw("description"),
        host_agent,
        enabled: v.bool_or("enabled", true),
        class_id,
        steps,
        schedules,
        exceptions,
    })
}

// ---------------------------------------------------------------------------
// Connection strings and their password
// ---------------------------------------------------------------------------

/// A connection string without its secrets (see [`without_password`]),
/// and its password (unquoted, unescaped), if it had one.
pub(crate) fn split_password(conn: &str) -> (String, Option<String>) {
    (without_password(conn).0, param(conn, "password"))
}

/// The passphrase of the client's key (`sslpassword`), if it has one.
pub(crate) fn ssl_password(conn: &str) -> Option<String> {
    param(conn, "sslpassword")
}

/// The value of `key` in a connection string (unquoted, unescaped; a
/// URI's password may be in its user info).
fn param(conn: &str, key: &str) -> Option<String> {
    let t = conn.trim();
    if t.starts_with("postgres://") || t.starts_with("postgresql://") {
        return uri_param(t, key);
    }
    let chars: Vec<char> = t.chars().collect();
    let mut found = None;
    let mut i = 0;
    while i < chars.len() {
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let start = i;
        while i < chars.len() && chars[i] != '=' && !chars[i].is_whitespace() {
            i += 1;
        }
        let k: String = chars[start..i].iter().collect();
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        if i < chars.len() && chars[i] == '=' {
            i += 1;
        }
        while i < chars.len() && chars[i].is_whitespace() {
            i += 1;
        }
        let mut value = String::new();
        if i < chars.len() && chars[i] == '\'' {
            i += 1;
            while i < chars.len() && chars[i] != '\'' {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                value.push(chars[i]);
                i += 1;
            }
            i += 1;
        } else {
            while i < chars.len() && !chars[i].is_whitespace() {
                if chars[i] == '\\' && i + 1 < chars.len() {
                    i += 1;
                }
                value.push(chars[i]);
                i += 1;
            }
        }
        if k.eq_ignore_ascii_case(key) {
            found = Some(value);
        }
    }
    found
}

fn uri_param(t: &str, key: &str) -> Option<String> {
    let mut found = None;
    let (_, rest) = t.split_once("://").unwrap_or(("postgresql", t));
    let authority_end = rest.find(['/', '?']).unwrap_or(rest.len());
    if key.eq_ignore_ascii_case("password") {
        if let Some((userinfo, _)) = rest[..authority_end].rsplit_once('@') {
            if let Some((_, p)) = userinfo.split_once(':') {
                found = Some(pct_decode(p));
            }
        }
    }
    if let Some((_, query)) = rest.split_once('?') {
        for kv in query.split('&') {
            if let Some((k, v)) = kv.split_once('=') {
                if k.eq_ignore_ascii_case(key) {
                    found = Some(pct_decode(v));
                }
            }
        }
    }
    found
}

fn pct_decode(s: &str) -> String {
    let hex = |b: u8| (b as char).to_digit(16).map(|d| d as u8);
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    while i < b.len() {
        if b[i] == b'%' && i + 2 < b.len() {
            if let (Some(h), Some(l)) = (hex(b[i + 1]), hex(b[i + 2])) {
                out.push(h * 16 + l);
                i += 3;
                continue;
            }
        }
        out.push(b[i]);
        i += 1;
    }
    String::from_utf8_lossy(&out).into_owned()
}

fn pct_encode(s: &str) -> String {
    s.bytes()
        .map(|b| {
            if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'.' | b'_' | b'~') {
                (b as char).to_string()
            } else {
                format!("%{b:02X}")
            }
        })
        .collect()
}

/// `conn` (without a password) with `password` added; `mask`: the
/// password shown as [`SECRET_MASK`].
pub(crate) fn with_password(conn: &str, password: Option<&str>, mask: bool) -> String {
    with_param(conn, "password", password, mask)
}

/// `conn` with `key=value` added, `value` a secret (`mask`: shown as
/// [`SECRET_MASK`]).
fn with_param(conn: &str, key: &str, value: Option<&str>, mask: bool) -> String {
    let t = conn.trim();
    let Some(p) = value else {
        return t.to_string();
    };
    let uri = t.starts_with("postgres://") || t.starts_with("postgresql://");
    let value = if mask {
        SECRET_MASK.to_string()
    } else if uri {
        pct_encode(p)
    } else {
        format!("'{}'", p.replace('\\', "\\\\").replace('\'', "\\'"))
    };
    if uri {
        let sep = if t.contains('?') { '&' } else { '?' };
        format!("{t}{sep}{key}={value}")
    } else if t.is_empty() {
        format!("{key}={value}")
    } else {
        format!("{t} {key}={value}")
    }
}

/// The connection string a step will have (`mask`: as the script shows
/// it), from the form's and the server's: the password as the form says,
/// the key's passphrase as typed or else the server's.
fn final_connstr(st: &StepSpec, cur: Option<&Step>, mask: bool) -> String {
    if st.kind == 'b' || st.connstr.is_empty() {
        return String::new();
    }
    let kept = cur.and_then(|c| split_password(&c.connstr).1);
    let password = match &st.password {
        Password::Set(p) => Some(p.as_str()),
        Password::Drop => None,
        Password::Keep => kept.as_deref(),
    };
    let kept_ssl = cur.and_then(|c| ssl_password(&c.connstr));
    let ssl = match &st.ssl_password {
        Some(p) => Some(p.as_str()).filter(|p| !p.is_empty()),
        None => kept_ssl.as_deref(),
    };
    with_password(
        &with_param(&st.connstr, "sslpassword", ssl, mask),
        password,
        mask,
    )
}

/// The step keeps the server's connection string as it is: the same one
/// without its secrets, and the secrets kept.
fn same_connstr(st: &StepSpec, cur: &Step) -> bool {
    st.kind != 'b'
        && st.password == Password::Keep
        && st.ssl_password.is_none()
        && !cur.connstr.trim().is_empty()
        && st.connstr == without_password(&cur.connstr).0
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// A `{t,f,…}::boolean[]` literal.
fn array_lit(flags: &[bool]) -> String {
    let items: Vec<&str> = flags.iter().map(|b| if *b { "t" } else { "f" }).collect();
    format!("'{{{}}}'::boolean[]", items.join(","))
}

fn date_lit(v: Variant, d: &Option<String>) -> String {
    d.as_ref()
        .map(|d| format!("{}::date", lit(v, d)))
        .unwrap_or_else(|| "NULL".into())
}

fn time_lit(v: Variant, t: &Option<String>) -> String {
    t.as_ref()
        .map(|t| format!("{}::time", lit(v, t)))
        .unwrap_or_else(|| "NULL".into())
}

fn start_lit(v: Variant, sc: &ScheduleSpec) -> String {
    sc.start
        .as_ref()
        .map(|t| ts_lit(v, t))
        .unwrap_or_else(|| "now()".into())
}

fn end_lit(v: Variant, sc: &ScheduleSpec) -> String {
    sc.end
        .as_ref()
        .map(|t| ts_lit(v, t))
        .unwrap_or_else(|| "NULL".into())
}

fn insert_step(v: Variant, job: &str, st: &StepSpec, mask: bool) -> String {
    format!(
        "    INSERT INTO pgagent.pga_jobstep (jstjobid, jstname, jstdesc, jstenabled, jstkind, jstcode, jstconnstr, jstdbname, jstonerror)\n    VALUES ({job}, {}, {}, {}, {}, {}, {}, {}, {});\n",
        lit(v, &st.name),
        lit(v, &st.description),
        st.enabled,
        lit(v, &st.kind.to_string()),
        code_lit(&st.code),
        lit(v, &final_connstr(st, None, mask)),
        lit(v, &st.dbname),
        lit(v, &st.on_error.to_string()),
    )
}

fn insert_schedule(v: Variant, job: &str, sc: &ScheduleSpec) -> String {
    format!(
        "    INSERT INTO pgagent.pga_schedule (jscjobid, jscname, jscdesc, jscenabled, jscstart, jscend,\n                                      jscminutes, jschours, jscweekdays, jscmonthdays, jscmonths)\n    VALUES ({job}, {}, {}, {}, {}, {},\n            {},\n            {},\n            {}, {}, {})\n    RETURNING jscid INTO scid;\n",
        lit(v, &sc.name),
        lit(v, &sc.description),
        sc.enabled,
        start_lit(v, sc),
        end_lit(v, sc),
        array_lit(&sc.minutes),
        array_lit(&sc.hours),
        array_lit(&sc.weekdays),
        array_lit(&sc.monthdays),
        array_lit(&sc.months),
    )
}

fn insert_exception(v: Variant, schedule: &str, e: &ExceptionSpec) -> String {
    format!(
        "    INSERT INTO pgagent.pga_exception (jexscid, jexdate, jextime) VALUES ({schedule}, {}, {});\n",
        date_lit(v, &e.date),
        time_lit(v, &e.time)
    )
}

/// The `DO` block around `body`, with the variables it uses.
fn do_block(comment: &str, body: &str) -> String {
    let block = format!("\nDECLARE\n    jid integer;\n    scid integer;\nBEGIN\n{body}END\n");
    let mut out = String::new();
    for line in comment.lines() {
        out.push_str(&format!("-- {}\n", comment_text(line)));
    }
    out.push_str(&format!("DO {};\n", dollar_tagged(&block, "job")));
    out
}

/// The script that creates the job.
pub(crate) fn create_script(v: Variant, j: &JobSpec, mask: bool) -> String {
    let mut body = format!(
        "    INSERT INTO pgagent.pga_job (jobjclid, jobname, jobdesc, jobhostagent, jobenabled)\n    VALUES ({}, {}, {}, {}, {})\n    RETURNING jobid INTO jid;\n",
        j.class_id,
        lit(v, &j.name),
        lit(v, &j.description),
        lit(v, &j.host_agent),
        j.enabled
    );
    for st in &j.steps {
        body.push('\n');
        body.push_str(&insert_step(v, "jid", st, mask));
    }
    for sc in &j.schedules {
        body.push('\n');
        body.push_str(&insert_schedule(v, "jid", sc));
        for e in j.exceptions.iter().filter(|e| e.schedule == sc.name) {
            body.push_str(&insert_exception(v, "scid", e));
        }
    }
    do_block(&format!("pgAgent: nuevo job «{}»", j.name), &body)
}

/// The step's changed columns.
fn step_changes(v: Variant, st: &StepSpec, cur: &Step, mask: bool) -> Vec<String> {
    let mut set = Vec::new();
    if st.name != cur.name {
        set.push(format!("jstname = {}", lit(v, &st.name)));
    }
    if st.description != cur.description {
        set.push(format!("jstdesc = {}", lit(v, &st.description)));
    }
    if st.enabled != cur.enabled {
        set.push(format!("jstenabled = {}", st.enabled));
    }
    if st.kind != cur.kind {
        set.push(format!("jstkind = {}", lit(v, &st.kind.to_string())));
    }
    if st.code != cur.code {
        set.push(format!("jstcode = {}", code_lit(&st.code)));
    }
    if !same_connstr(st, cur) && final_connstr(st, Some(cur), false) != cur.connstr {
        set.push(format!(
            "jstconnstr = {}",
            lit(v, &final_connstr(st, Some(cur), mask))
        ));
    }
    if st.dbname != cur.dbname {
        set.push(format!("jstdbname = {}", lit(v, &st.dbname)));
    }
    if st.on_error != cur.on_error {
        set.push(format!("jstonerror = {}", lit(v, &st.on_error.to_string())));
    }
    set
}

fn schedule_changes(v: Variant, sc: &ScheduleSpec, cur: &Schedule) -> Vec<String> {
    let mut set = Vec::new();
    if sc.name != cur.name {
        set.push(format!("jscname = {}", lit(v, &sc.name)));
    }
    if sc.description != cur.description {
        set.push(format!("jscdesc = {}", lit(v, &sc.description)));
    }
    if sc.enabled != cur.enabled {
        set.push(format!("jscenabled = {}", sc.enabled));
    }
    if sc.start.as_deref().unwrap_or_default() != cur.start {
        set.push(format!("jscstart = {}", start_lit(v, sc)));
    }
    if sc.end != cur.end {
        set.push(format!("jscend = {}", end_lit(v, sc)));
    }
    for (col, new, old) in [
        ("jscminutes", &sc.minutes, &cur.minutes),
        ("jschours", &sc.hours, &cur.hours),
        ("jscweekdays", &sc.weekdays, &cur.weekdays),
        ("jscmonthdays", &sc.monthdays, &cur.monthdays),
        ("jscmonths", &sc.months, &cur.months),
    ] {
        if new != old {
            set.push(format!("{col} = {}", array_lit(new)));
        }
    }
    set
}

/// The script that changes the job to `j`, or `None` when nothing changed.
pub(crate) fn alter_script(v: Variant, cur: &Job, j: &JobSpec, mask: bool) -> Option<String> {
    let n = cur.id;
    let mut body = String::new();
    let mut job_set = Vec::new();
    if j.name != cur.name {
        job_set.push(format!("jobname = {}", lit(v, &j.name)));
    }
    if j.description != cur.description {
        job_set.push(format!("jobdesc = {}", lit(v, &j.description)));
    }
    if j.host_agent != cur.host_agent {
        job_set.push(format!("jobhostagent = {}", lit(v, &j.host_agent)));
    }
    if j.enabled != cur.enabled {
        job_set.push(format!("jobenabled = {}", j.enabled));
    }
    if j.class_id != cur.class_id {
        job_set.push(format!("jobjclid = {}", j.class_id));
    }
    let schedule_id = |name: &str| -> Option<i32> {
        j.schedules
            .iter()
            .find(|s| s.name == name)
            .and_then(|s| s.id)
    };
    // Exceptions: removed or moving to a new schedule go first; changed ones
    // on a schedule that stays are updated before schedules go.
    let mut parts = String::new();
    for e in &cur.exceptions {
        let kept = j.exceptions.iter().find(|x| x.id == Some(e.id));
        let delete = match kept {
            None => true,
            Some(x) => schedule_id(&x.schedule).is_none(),
        };
        if delete {
            parts.push_str(&format!(
                "    DELETE FROM pgagent.pga_exception WHERE jexid = {};\n",
                e.id
            ));
        }
    }
    for x in &j.exceptions {
        let (Some(id), Some(sid)) = (x.id, schedule_id(&x.schedule)) else {
            continue;
        };
        let Some(e) = cur.exceptions.iter().find(|e| e.id == id) else {
            continue;
        };
        if e.schedule != sid || e.date != x.date || e.time != x.time {
            parts.push_str(&format!(
                "    UPDATE pgagent.pga_exception SET jexscid = {sid}, jexdate = {}, jextime = {} WHERE jexid = {id};\n",
                date_lit(v, &x.date),
                time_lit(v, &x.time)
            ));
        }
    }
    for sc in &cur.schedules {
        if !j.schedules.iter().any(|x| x.id == Some(sc.id)) {
            parts.push_str(&format!(
                "    DELETE FROM pgagent.pga_schedule WHERE jscid = {} AND jscjobid = {n};\n",
                sc.id
            ));
        }
    }
    for sc in &j.schedules {
        match sc
            .id
            .and_then(|id| cur.schedules.iter().find(|c| c.id == id))
        {
            Some(c) => {
                let set = schedule_changes(v, sc, c);
                if !set.is_empty() {
                    parts.push_str(&format!(
                        "    UPDATE pgagent.pga_schedule SET {} WHERE jscid = {} AND jscjobid = {n};\n",
                        set.join(", "),
                        c.id
                    ));
                }
            }
            None => {
                parts.push_str(&insert_schedule(v, &n.to_string(), sc));
                for e in j.exceptions.iter().filter(|e| e.schedule == sc.name) {
                    parts.push_str(&insert_exception(v, "scid", e));
                }
            }
        }
    }
    // New exceptions of schedules that stay (those of new schedules, and
    // those moved to one, went in with it).
    for x in &j.exceptions {
        if x.id.is_none() {
            if let Some(sid) = schedule_id(&x.schedule) {
                parts.push_str(&insert_exception(v, &sid.to_string(), x));
            }
        }
    }
    for st in &cur.steps {
        if !j.steps.iter().any(|x| x.id == Some(st.id)) {
            parts.push_str(&format!(
                "    DELETE FROM pgagent.pga_jobstep WHERE jstid = {} AND jstjobid = {n};\n",
                st.id
            ));
        }
    }
    for st in &j.steps {
        match st.id.and_then(|id| cur.steps.iter().find(|c| c.id == id)) {
            Some(c) => {
                let set = step_changes(v, st, c, mask);
                if !set.is_empty() {
                    parts.push_str(&format!(
                        "    UPDATE pgagent.pga_jobstep SET {} WHERE jstid = {} AND jstjobid = {n};\n",
                        set.join(", "),
                        c.id
                    ));
                }
            }
            None => parts.push_str(&insert_step(v, &n.to_string(), st, mask)),
        }
    }
    if job_set.is_empty() && parts.is_empty() {
        return None;
    }
    // Every change marks the job as changed (the stale check reads it).
    job_set.push("jobchanged = now()".into());
    body.push_str(&format!(
        "    UPDATE pgagent.pga_job SET {} WHERE jobid = {n};\n",
        job_set.join(", ")
    ));
    body.push_str(&parts);
    Some(do_block(&format!("pgAgent: job «{}»", j.name), &body))
}

/// The steps' names in the order the server sorts them (its collation):
/// the list's order has to be that one.
async fn check_order(s: &PgSession, steps: &[StepSpec]) -> Result<()> {
    if steps.len() < 2 {
        return Ok(());
    }
    let names: Vec<String> = steps.iter().map(|st| lit(s.variant, &st.name)).collect();
    let sorted: Vec<usize> = s
        .text(&format!(
            "SELECT t.i::text AS i FROM unnest(ARRAY[{}]::text[]) WITH ORDINALITY AS t(n, i) ORDER BY t.n, t.i",
            names.join(", ")
        ))
        .await?
        .iter()
        .filter_map(|r| text(r, "i").and_then(|i| i.parse::<usize>().ok()))
        .collect();
    order_problem(steps, &sorted).map_or(Ok(()), |m| Err(bad(m)))
}

/// What's wrong with the list's order, given the 1-based positions in the
/// order pgAgent runs them.
pub(crate) fn order_problem(steps: &[StepSpec], sorted: &[usize]) -> Option<String> {
    let first_off = sorted.iter().enumerate().find(|(i, p)| **p != i + 1)?;
    let (i, p) = first_off;
    let runs = &steps.get(p - 1)?.name;
    let listed = &steps.get(i)?.name;
    Some(format!(
        "pgAgent ejecuta los pasos en el orden alfabético de sus nombres: «{}» iría antes que «{}». Cambiales el nombre (por ejemplo, empezalos con 01, 02…) o reordenalos.",
        shown(runs),
        shown(listed)
    ))
}

pub(crate) async fn plan(
    s: &PgSession,
    c: &Ctx,
    n: Option<i32>,
    values: &FormValues,
    reveal: bool,
) -> Result<Plan> {
    let caps = caps_of(c)?;
    let cur = match n {
        Some(n) => {
            if !caps.may_edit() {
                return Err(bad(NO_WRITE.into()));
            }
            Some(read(s, n).await?)
        }
        None => {
            if !caps.may_create() {
                return Err(bad(NO_WRITE.into()));
            }
            None
        }
    };
    if let Some(j) = &cur {
        let seen = Vals(&values.values).opt("changed_at");
        if seen.is_some_and(|t| t != j.changed_at) {
            return Err(bad(STALE.into()));
        }
    }
    let classes = classes(s).await?;
    let dbs = databases(s).await?;
    let j = spec(values, cur.as_ref(), &classes, &dbs)?;
    check_order(s, &j.steps).await?;
    let v = s.variant;
    let mask = !reveal;
    Ok(match &cur {
        None => Plan {
            script: create_script(v, &j, mask),
            empty: false,
            id: None,
            new_id: Some(NewId::Query(
                "SELECT currval(pg_catalog.pg_get_serial_sequence('pgagent.pga_job', 'jobid'))::text AS id".into(),
                "pgagent:",
            )),
        },
        Some(cur) => match alter_script(v, cur, &j, mask) {
            None => Plan::nothing(&cur.name, JobId::Agent(cur.id).to_string()),
            Some(script) => Plan {
                script,
                empty: false,
                id: Some(JobId::Agent(cur.id).to_string()),
                new_id: None,
            },
        },
    })
}

pub(crate) async fn delete_script(s: &PgSession, c: &Ctx, n: i32) -> Result<String> {
    let caps = caps_of(c)?;
    if !caps.may_delete() {
        return Err(bad(NO_WRITE.into()));
    }
    let j = read(s, n).await?;
    Ok(format!(
        "-- pgAgent: borrar el job «{}», con sus pasos, programaciones e historial\nDELETE FROM pgagent.pga_job WHERE jobid = {n};\n",
        comment_text(&j.name)
    ))
}

pub(crate) async fn clear_script(s: &PgSession, c: &Ctx, n: i32) -> Result<String> {
    let caps = caps_of(c)?;
    if !caps.log_delete {
        return Err(bad(
            "Tu usuario no puede borrar el historial de pgAgent (DELETE en pgagent.pga_joblog)."
                .into(),
        ));
    }
    let j = read(s, n).await?;
    Ok(format!(
        "-- pgAgent: borrar el historial del job «{}» (sus pasos incluidos), salvo la ejecución en curso\nDELETE FROM pgagent.pga_joblog WHERE jlgjobid = {n} AND jlgstatus <> 'r';\n",
        comment_text(&j.name)
    ))
}

// ---------------------------------------------------------------------------
// Job classes
// ---------------------------------------------------------------------------

/// A class id as the tree has it: its number.
pub(crate) fn class_id(id: &str) -> Result<i32> {
    id.parse::<i32>()
        .ok()
        .filter(|n| *n > 0 && n.to_string() == id)
        .ok_or_else(|| {
            bad(format!(
                "«{}» no es un id de clase de job de pgAgent",
                shown(id)
            ))
        })
}

pub(crate) async fn class_items(s: &PgSession, c: &Ctx) -> Result<SchedulerList> {
    if c.agent.is_none() {
        return Ok(SchedulerList {
            items: Vec::new(),
            notes: vec!["pgAgent no está en esta base.".into()],
        });
    }
    Ok(SchedulerList {
        items: classes(s)
            .await?
            .into_iter()
            .map(|(id, name)| SchedulerItem {
                id: id.to_string(),
                name,
                description: None,
                enabled: None,
                system: false,
                can_edit: None,
                can_delete: None,
                actions: None,
            })
            .collect(),
        notes: Vec::new(),
    })
}

async fn class_row(s: &PgSession, n: i32) -> Result<(String, i64)> {
    let rows = s
        .text(&format!(
            "SELECT c.jclname, (SELECT count(*) FROM pgagent.pga_job j WHERE j.jobjclid = c.jclid)::text AS jobs
             FROM pgagent.pga_jobclass c WHERE c.jclid = {n}"
        ))
        .await?;
    let r = rows
        .first()
        .ok_or_else(|| Error::Query(format!("no existe la clase de job {n}")))?;
    Ok((
        r.get("jclname").unwrap_or_default().to_string(),
        text(r, "jobs").and_then(|j| j.parse().ok()).unwrap_or(0),
    ))
}

pub(crate) async fn class_form(s: &PgSession, c: &Ctx, id: Option<&str>) -> Result<ObjectForm> {
    let caps = caps_of(c)?;
    let mut general = FormSection::new("general", "General");
    let mut values = BTreeMap::new();
    let read_only = match id {
        Some(id) => {
            let n = class_id(id)?;
            let (name, jobs) = class_row(s, n).await?;
            general = general.fields(vec![
                FormField::new("classid", "Id", FormKind::Text).read_only(true),
                FormField::new("name", "Nombre", FormKind::Text).required(),
                FormField::new("jobs", "Jobs de esta clase", FormKind::Text).read_only(true),
            ]);
            values.insert("classid".to_string(), json!(n.to_string()));
            values.insert("name".to_string(), json!(name));
            values.insert("jobs".to_string(), json!(jobs.to_string()));
            !caps.class_update
        }
        None => {
            general = general.field(FormField::new("name", "Nombre", FormKind::Text).required());
            !caps.class_insert
        }
    };
    let mut notes = Vec::new();
    if read_only {
        notes.push(
            "Tu usuario no puede cambiar las clases de job de pgAgent (pgagent.pga_jobclass)."
                .to_string(),
        );
    }
    Ok(ObjectForm {
        sections: vec![general],
        values,
        choices: BTreeMap::new(),
        notes,
        read_only,
    })
}

pub(crate) async fn class_plan(
    s: &PgSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let caps = caps_of(c)?;
    let name = name_of(Vals(&values.values), "name", "Nombre")?;
    let v = s.variant;
    let current = match id {
        Some(id) => {
            let n = class_id(id)?;
            Some((n, class_row(s, n).await?.0))
        }
        None => None,
    };
    if current.as_ref().is_none_or(|(_, cur)| *cur != name)
        && classes(s).await?.iter().any(|(_, n)| *n == name)
    {
        return Err(bad(format!(
            "Ya hay una clase de job llamada «{}».",
            shown(&name)
        )));
    }
    let lookup = format!(
        "SELECT jclid::text AS id FROM pgagent.pga_jobclass WHERE jclname = {}",
        lit(v, &name)
    );
    match current {
        None => {
            if !caps.class_insert {
                return Err(bad(
                    "Tu usuario no puede crear clases de job de pgAgent.".into()
                ));
            }
            Ok(Plan {
                script: format!(
                    "-- pgAgent: nueva clase de job «{}»\nINSERT INTO pgagent.pga_jobclass (jclname) VALUES ({});\n",
                    comment_text(&name),
                    lit(v, &name)
                ),
                empty: false,
                id: None,
                new_id: Some(NewId::Query(lookup, "")),
            })
        }
        Some((n, cur)) if cur == name => Ok(Plan::nothing(&cur, n.to_string())),
        Some((n, cur)) => {
            if !caps.class_update {
                return Err(bad(
                    "Tu usuario no puede cambiar clases de job de pgAgent.".into()
                ));
            }
            Ok(Plan {
                script: format!(
                    "-- pgAgent: la clase de job «{}» pasa a llamarse «{}»\nUPDATE pgagent.pga_jobclass SET jclname = {} WHERE jclid = {n};\n",
                    comment_text(&cur),
                    comment_text(&name),
                    lit(v, &name)
                ),
                empty: false,
                id: Some(n.to_string()),
                new_id: None,
            })
        }
    }
}

pub(crate) async fn class_delete_script(s: &PgSession, c: &Ctx, id: &str) -> Result<String> {
    let caps = caps_of(c)?;
    if !caps.class_delete {
        return Err(bad(
            "Tu usuario no puede borrar clases de job de pgAgent.".into()
        ));
    }
    let n = class_id(id)?;
    let (name, jobs) = class_row(s, n).await?;
    if jobs > 0 {
        return Err(bad(format!(
            "La clase «{}» tiene {jobs} job(s): cambiales la clase antes de borrarla.",
            shown(&name)
        )));
    }
    Ok(format!(
        "-- pgAgent: borrar la clase de job «{}»\nDELETE FROM pgagent.pga_jobclass WHERE jclid = {n};\n",
        comment_text(&name)
    ))
}
