//! pg_cron's jobs and Apache Cloudberry's tasks (pg_cron built into the
//! server, `CREATE TASK`) in the designer: name, schedule (a cron
//! expression or `N seconds`), command, database, user, active.
//!
//! pg_cron:
//! - Create: `cron.schedule_in_database` (1.4+, which takes the database,
//!   the user and the state) where the user may run it; else
//!   `cron.schedule` (this database, this user, active), named from 1.3.
//!   pg_cron grants `schedule_in_database` and `alter_job` to its owner only:
//!   other users get the fields they can't set read-only.
//! - Edit: `cron.alter_job` with what changed; without it, `cron.schedule`
//!   with the job's own name (pg_cron replaces the user's job of that name,
//!   keeping its id) changes the schedule and the command. A new name is an
//!   `UPDATE cron.job` where the user may (its owner, a superuser);
//!   otherwise the job is scheduled again under the new name and the old one
//!   unscheduled: a new id, the history left under the old one.
//! - Delete: `cron.unschedule(id)`. History: `DELETE FROM
//!   cron.job_run_details` (row-level security keeps a user to their own).
//!
//! Tasks: `CREATE TASK` / `ALTER TASK … [SCHEDULE] [DATABASE] [USER]
//! [ACTIVE | NOT ACTIVE] [AS]` / `DROP TASK`. They find a task by its name
//! among the current user's, so another user's task is changed as that user
//! (`SET LOCAL ROLE`, a superuser's right). There is no rename (nor, as
//! another user, a change of user): the task is dropped and created again,
//! with a new id and without its history, which `DROP TASK` deletes. Its
//! history is a system catalog (`pg_task_run_history`) that can't be
//! cleared.

use super::{
    bad, code_lit, databases, id as ident, login_roles, name_of, pick, shown, transaction,
    with_current, Ctx, NewId, Plan, Vals,
};
use crate::catalog::{comment_text, lit};
use crate::jobs::{flag, text, Found, JobId};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use serde_json::json;
#[cfg(test)]
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Cron,
    Task,
}

impl Flavor {
    fn table(self) -> &'static str {
        match self {
            Flavor::Cron => "cron.job",
            Flavor::Task => "pg_catalog.pg_task",
        }
    }

    pub(crate) fn id(self, n: i64) -> JobId {
        match self {
            Flavor::Cron => JobId::Cron(n),
            Flavor::Task => JobId::Task(n),
        }
    }

    fn prefix(self) -> &'static str {
        match self {
            Flavor::Cron => "pg_cron:",
            Flavor::Task => "task:",
        }
    }
}

const CRON_UNREADABLE: &str =
    "Tu usuario no puede leer los jobs de pg_cron: necesita USAGE en el esquema cron.";
const TASK_UNREADABLE: &str = "Tu usuario no puede leer las tareas del servidor (pg_task).";
const NO_IN_DATABASE: &str = "Tu usuario no puede ejecutar cron.schedule_in_database ni cron.alter_job (pg_cron se los da solo a su dueño, salvo que se le dé EXECUTE): sus jobs se crean en esta base, como tu usuario y activos, y de los existentes cambia solo la programación y el comando.";
const NO_NAMES: &str = "Esta versión de pg_cron no les pone nombre a los jobs (llegó en la 1.3): se identifican por su id.";
const OTHERS_JOB: &str =
    "Este job es de otro usuario: solo su dueño o un superusuario puede cambiarlo.";
const OTHERS_TASK: &str =
    "Esta tarea es de otro usuario: solo su dueño o un superusuario puede cambiarla.";
const CRON_RENAME: &str = "pg_cron no permite cambiarle el nombre a un job (solo su dueño o un superusuario pueden, en cron.job): al guardar con otro nombre se programa de nuevo y se borra el actual; el job cambia de id y su historial queda con el id anterior.";
const TASK_RECREATE: &str = "El servidor no permite cambiarle el nombre a una tarea, ni el usuario a la de otro: al guardar esos cambios la tarea se borra y se crea de nuevo, con otro id y sin su historial de ejecuciones.";

/// What pg_cron (or the tasks) has here, and what the user may do.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Caps {
    pub(crate) flavor: Option<Flavor>,
    /// `USAGE` on the schema and `SELECT` on the jobs.
    pub(crate) readable: bool,
    /// `cron.job.jobname` (1.3+).
    pub(crate) name: bool,
    /// `cron.job.active` (1.3+).
    pub(crate) active: bool,
    /// The run history is there and readable.
    pub(crate) runs: bool,
    /// May run `cron.schedule` (named when `name`).
    pub(crate) schedule: bool,
    /// `cron.schedule_in_database` exists (1.4+) / may run it.
    pub(crate) has_in_database: bool,
    pub(crate) in_database: bool,
    /// `cron.alter_job` exists (1.4+) / may run it.
    pub(crate) has_alter: bool,
    pub(crate) alter: bool,
    pub(crate) unschedule: bool,
    /// May rename in place (`UPDATE` on `cron.job`).
    pub(crate) rename: bool,
    /// May delete run history (`DELETE` on `cron.job_run_details`).
    pub(crate) clear: bool,
    /// Takes `N seconds` (pg_cron 1.5+, every task).
    pub(crate) seconds: bool,
    pub(crate) superuser: bool,
    pub(crate) user: String,
    pub(crate) database: String,
    /// The time zone schedules are read in, when the user may see it.
    pub(crate) timezone: Option<String>,
}

impl Caps {
    fn flavor(&self) -> Flavor {
        self.flavor.unwrap_or(Flavor::Cron)
    }

    pub(crate) fn may_create(&self) -> bool {
        match self.flavor() {
            Flavor::Cron => self.readable && (self.in_database || self.schedule),
            Flavor::Task => self.readable,
        }
    }

    pub(crate) fn may_edit(&self) -> bool {
        match self.flavor() {
            Flavor::Cron => self.readable && (self.alter || (self.schedule && self.name)),
            Flavor::Task => self.readable,
        }
    }

    pub(crate) fn may_delete(&self) -> bool {
        match self.flavor() {
            Flavor::Cron => self.readable && self.unschedule,
            Flavor::Task => self.readable,
        }
    }

    pub(crate) fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        match self.flavor() {
            Flavor::Cron if !self.readable => out.push(CRON_UNREADABLE.into()),
            Flavor::Task if !self.readable => out.push(TASK_UNREADABLE.into()),
            Flavor::Cron => {
                if !self.name {
                    out.push(NO_NAMES.into());
                }
                if self.has_in_database && !self.in_database && self.schedule {
                    out.push(NO_IN_DATABASE.into());
                }
            }
            Flavor::Task => {}
        }
        out
    }

    /// The user may change a job of `owner`.
    pub(crate) fn may_change(&self, owner: &str) -> bool {
        self.superuser || owner == self.user
    }
}

/// pg_cron's capabilities and the user's rights on it, in one row.
fn caps_sql(flavor: Flavor) -> String {
    let setting =
        |name: &str| format!("(SELECT setting FROM pg_catalog.pg_settings WHERE name = '{name}')");
    let common = "current_user AS usr, current_database() AS db,
         coalesce((SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user), false) AS superuser";
    if flavor == Flavor::Task {
        return format!(
            "SELECT {common}, has_table_privilege('pg_catalog.pg_task', 'SELECT') AS readable,
                    has_table_privilege('pg_catalog.pg_task_run_history', 'SELECT') AS runs,
                    {tz} AS timezone",
            tz = setting("task_timezone"),
        );
    }
    let rel = |name: &str, privilege: &str| {
        format!(
            "coalesce((SELECT has_table_privilege(c.oid, '{privilege}') FROM pg_catalog.pg_class c
               JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
              WHERE n.nspname = 'cron' AND c.relname = '{name}'), false)"
        )
    };
    let col = |name: &str| {
        format!(
            "EXISTS (SELECT 1 FROM pg_catalog.pg_attribute a JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                       JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                      WHERE n.nspname = 'cron' AND c.relname = 'job' AND a.attname = '{name}' AND NOT a.attisdropped)"
        )
    };
    let func = |name: &str, args: &str, and: &str| {
        format!(
            "EXISTS (SELECT 1 FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                     WHERE n.nspname = 'cron' AND p.proname = '{name}'{args}{and})"
        )
    };
    let exec = " AND has_function_privilege(p.oid, 'EXECUTE')";
    format!(
        "SELECT {common},
                has_schema_privilege('cron', 'USAGE') AND {job} AS readable,
                {name} AS has_name, {active} AS has_active, {runs} AS runs,
                {rename} AS may_rename, {clear} AS may_clear,
                {schedule} AS schedule, {has_in} AS has_in_database, {in_db} AS in_database,
                {has_alter} AS has_alter, {alter} AS alter_job, {unschedule} AS unschedule,
                (SELECT extversion FROM pg_catalog.pg_extension WHERE extname = 'pg_cron') AS version,
                {tz} AS timezone",
        job = rel("job", "SELECT"),
        name = col("jobname"),
        active = col("active"),
        runs = rel("job_run_details", "SELECT"),
        rename = rel("job", "UPDATE"),
        clear = rel("job_run_details", "DELETE"),
        schedule = func("schedule", "", exec),
        has_in = func("schedule_in_database", "", ""),
        in_db = func("schedule_in_database", "", exec),
        has_alter = func("alter_job", "", ""),
        alter = func("alter_job", "", exec),
        unschedule = func("unschedule", " AND p.pronargs = 1", exec),
        tz = setting("cron.timezone"),
    )
}

/// `1.5` and later read `N seconds`.
fn takes_seconds(version: Option<&str>) -> bool {
    let Some(v) = version else { return false };
    let mut parts = v.split('.').map(|p| p.trim().parse::<u32>().unwrap_or(0));
    let (major, minor) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (major, minor) >= (1, 5)
}

fn caps_of(flavor: Flavor, r: &dyn crate::jobs::Row) -> Caps {
    let base = Caps {
        flavor: Some(flavor),
        superuser: flag(r, "superuser"),
        user: text(r, "usr").unwrap_or_default(),
        database: text(r, "db").unwrap_or_default(),
        readable: flag(r, "readable"),
        runs: flag(r, "runs"),
        timezone: text(r, "timezone"),
        ..Caps::default()
    };
    match flavor {
        Flavor::Task => Caps {
            name: true,
            active: true,
            seconds: true,
            ..base
        },
        Flavor::Cron => Caps {
            name: flag(r, "has_name"),
            active: flag(r, "has_active"),
            rename: flag(r, "may_rename"),
            clear: flag(r, "may_clear"),
            schedule: flag(r, "schedule"),
            has_in_database: flag(r, "has_in_database"),
            in_database: flag(r, "in_database"),
            has_alter: flag(r, "has_alter"),
            alter: flag(r, "alter_job"),
            unschedule: flag(r, "unschedule"),
            seconds: takes_seconds(text(r, "version").as_deref()),
            ..base
        },
    }
}

pub(crate) async fn caps(s: &PgSession, _f: &Found, flavor: Flavor) -> Result<Caps> {
    let rows = s.text(&caps_sql(flavor)).await?;
    Ok(rows.first().map(|r| caps_of(flavor, r)).unwrap_or(Caps {
        flavor: Some(flavor),
        ..Caps::default()
    }))
}

fn caps_for(c: &Ctx, flavor: Flavor) -> Result<&Caps> {
    match flavor {
        Flavor::Cron => c.cron.as_ref(),
        Flavor::Task => c.task.as_ref(),
    }
    .ok_or_else(|| {
        Error::Query(match flavor {
            Flavor::Cron => "pg_cron no está en esta base (o tu usuario no puede leerlo)".into(),
            Flavor::Task => "este servidor no tiene tareas (CREATE TASK)".into(),
        })
    })
}

// ---------------------------------------------------------------------------
// The job as the server has it
// ---------------------------------------------------------------------------

/// A job (or task) as its table has it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Job {
    pub(crate) id: i64,
    pub(crate) name: Option<String>,
    pub(crate) schedule: String,
    pub(crate) command: String,
    pub(crate) database: String,
    pub(crate) username: String,
    pub(crate) active: bool,
    pub(crate) node: Option<String>,
}

fn job_sql(flavor: Flavor, caps: &Caps, n: i64) -> String {
    let name = if caps.name { "jobname" } else { "NULL::text" };
    let active = if caps.active { "active" } else { "true" };
    format!(
        "SELECT jobid::text AS id, {name} AS jobname, schedule, command, nodename, nodeport::text AS nodeport,
                database, username, {active} AS active
         FROM {} WHERE jobid = {n}",
        flavor.table()
    )
}

fn job_of(r: &dyn crate::jobs::Row) -> Option<Job> {
    Some(Job {
        id: text(r, "id")?.parse().ok()?,
        name: text(r, "jobname"),
        schedule: r.get("schedule").unwrap_or_default(),
        command: r.get("command").unwrap_or_default(),
        database: r.get("database").unwrap_or_default(),
        username: r.get("username").unwrap_or_default(),
        active: flag(r, "active"),
        node: match (text(r, "nodename"), text(r, "nodeport")) {
            (Some(h), Some(p)) => Some(format!("{h}:{p}")),
            (Some(h), None) => Some(h),
            _ => None,
        },
    })
}

async fn read(s: &PgSession, flavor: Flavor, caps: &Caps, n: i64) -> Result<Job> {
    s.text(&job_sql(flavor, caps, n))
        .await?
        .first()
        .and_then(|r| job_of(r))
        .ok_or_else(|| {
            Error::Query(format!(
                "no existe el job {} (o tu usuario no lo ve)",
                flavor.id(n)
            ))
        })
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// The fields of a job (or a task); `job` is the one being edited.
pub(crate) fn fields(flavor: Flavor, caps: &Caps, job: Option<&Job>) -> Vec<FormSection> {
    let edit = job.is_some();
    // New jobs: what the user may set; existing ones: what the in-place
    // change may.
    let (db_free, user_free, active_free) = match (flavor, edit) {
        (Flavor::Cron, false) => (
            caps.in_database,
            caps.in_database && caps.superuser,
            caps.in_database && caps.active,
        ),
        (Flavor::Cron, true) => (
            caps.alter,
            caps.alter && caps.superuser,
            caps.alter && caps.active,
        ),
        (Flavor::Task, _) => (true, caps.superuser, true),
    };
    let mut general = FormSection::new("general", "General");
    if let Some(j) = job {
        general = general.field(FormField::new("jobid", "Id", FormKind::Text).read_only(true));
        if j.node.is_some() {
            general = general.field(
                FormField::new("node", "Servidor", FormKind::Text)
                    .read_only(true)
                    .help("El servidor al que se conecta pg_cron para ejecutar el job."),
            );
        }
    }
    if caps.name {
        let name = FormField::new("name", "Nombre", FormKind::Text).required();
        general = general.field(match flavor {
            Flavor::Cron => name.help(
                "pg_cron identifica el job por su nombre y su usuario: no puede haber dos con el mismo nombre para el mismo usuario.",
            ),
            Flavor::Task => name.help("Cada usuario tiene sus tareas: el nombre no se repite entre las de un mismo usuario."),
        });
    }
    let schedule = FormField::new("schedule", "Programación", FormKind::Cron)
        .required()
        .placeholder("0 3 * * *");
    general = general.field(if caps.seconds {
        schedule.help("Una expresión cron de cinco campos (minuto, hora, día del mes, mes y día de la semana), o «N seconds» para ejecutarlo cada N segundos (de 1 a 59).")
    } else {
        schedule.help("Una expresión cron de cinco campos: minuto, hora, día del mes, mes y día de la semana.")
    });
    if caps.timezone.is_some() {
        let help = match flavor {
            Flavor::Cron => "La zona horaria en la que pg_cron lee las programaciones (cron.timezone, de la configuración del servidor).",
            Flavor::Task => "La zona horaria en la que el servidor lee las programaciones de las tareas (task_timezone, de la configuración del servidor).",
        };
        general = general.field(
            FormField::new("timezone", "Zona horaria", FormKind::Text)
                .read_only(true)
                .help(help),
        );
    }
    general = general.fields(vec![
        FormField::new("database", "Base de datos", FormKind::reference("database", false))
            .required()
            .default(json!(caps.database))
            .read_only(!db_free)
            .help("La base en la que se ejecuta el comando."),
        FormField::new("username", "Usuario", FormKind::reference("role", false))
            .required()
            .default(json!(caps.user))
            .read_only(!user_free)
            .help("El usuario con el que se ejecuta el comando. Solo un superusuario puede elegir otro que el suyo."),
        FormField::new("active", "Activo", FormKind::Bool)
            .default(json!(true))
            .read_only(!active_free),
    ]);
    let command = FormSection::new("command", "Comando").field(
        FormField::new("command", "Comando", FormKind::code("sql"))
            .required()
            .help("El SQL que se ejecuta en cada programación."),
    );
    vec![general, command]
}

pub(crate) async fn form(
    s: &PgSession,
    c: &Ctx,
    flavor: Flavor,
    n: Option<i64>,
) -> Result<ObjectForm> {
    let caps = caps_for(c, flavor)?;
    let job = match n {
        Some(n) => Some(read(s, flavor, caps, n).await?),
        None => None,
    };
    let mut f = ObjectForm {
        sections: fields(flavor, caps, job.as_ref()),
        ..ObjectForm::default()
    };
    let mut put = |k: &str, v: serde_json::Value| {
        f.values.insert(k.to_string(), v);
    };
    if let Some(tz) = &caps.timezone {
        put("timezone", json!(tz));
    }
    match &job {
        None => {
            put("database", json!(caps.database));
            put("username", json!(caps.user));
            put("active", json!(true));
        }
        Some(j) => {
            put("jobid", json!(j.id.to_string()));
            if let Some(node) = &j.node {
                put("node", json!(node));
            }
            put("name", json!(j.name.clone().unwrap_or_default()));
            put("schedule", json!(j.schedule));
            put("command", json!(j.command));
            put("database", json!(j.database));
            put("username", json!(j.username));
            put("active", json!(j.active));
        }
    }
    let current_db = job.as_ref().map_or(caps.database.as_str(), |j| &j.database);
    let current_user = job.as_ref().map_or(caps.user.as_str(), |j| &j.username);
    f.choices.insert(
        "database".into(),
        with_current(databases(s).await?, current_db),
    );
    f.choices.insert(
        "role".into(),
        with_current(login_roles(s).await?, current_user),
    );
    f.notes = caps.notes();
    if let Some(j) = &job {
        if !caps.may_change(&j.username) {
            f.read_only = true;
            f.notes.insert(
                0,
                match flavor {
                    Flavor::Cron => OTHERS_JOB,
                    Flavor::Task => OTHERS_TASK,
                }
                .into(),
            );
        } else if flavor == Flavor::Cron && !caps.may_edit() {
            f.read_only = true;
        }
        match flavor {
            Flavor::Cron if caps.name && !caps.rename => f.notes.push(CRON_RENAME.into()),
            Flavor::Task => f.notes.push(TASK_RECREATE.into()),
            _ => {}
        }
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// What the form asks for
// ---------------------------------------------------------------------------

/// A job as the form asks for it, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spec {
    pub(crate) name: Option<String>,
    pub(crate) schedule: String,
    pub(crate) command: String,
    pub(crate) database: String,
    pub(crate) username: String,
    pub(crate) active: bool,
}

/// A schedule as pg_cron reads it: five cron fields (names, `*`, lists,
/// ranges, steps, `$` and `L`-style tokens are left to the server), a
/// `@daily`-style macro, or `N seconds` (1 to 59) where it's taken.
pub(crate) fn check_schedule(schedule: &str, seconds: bool) -> Result<String> {
    let t = schedule.split_whitespace().collect::<Vec<_>>().join(" ");
    let fields: Vec<&str> = t.split(' ').collect();
    if fields.len() == 2
        && matches!(
            fields[1].to_ascii_lowercase().as_str(),
            "second" | "seconds"
        )
    {
        if !seconds {
            return Err(bad(
                "Esta versión de pg_cron no programa jobs cada N segundos (llegó en la 1.5): usá una expresión cron.".into(),
            ));
        }
        return match fields[0].parse::<u32>() {
            Ok(n) if (1..=59).contains(&n) => Ok(format!("{n} seconds")),
            _ => Err(bad(
                "«Programación»: los segundos tienen que estar entre 1 y 59.".into(),
            )),
        };
    }
    if fields.len() == 1
        && t.starts_with('@')
        && t.len() > 1
        && t[1..].bytes().all(|b| b.is_ascii_alphabetic())
    {
        return Ok(t);
    }
    let token_ok = |f: &str| {
        !f.is_empty()
            && f.bytes().all(|b| {
                b.is_ascii_alphanumeric()
                    || matches!(b, b'*' | b',' | b'/' | b'-' | b'$' | b'#' | b'?')
            })
    };
    if fields.len() != 5 || !fields.iter().all(|f| token_ok(f)) {
        return Err(bad(format!(
            "«Programación»: «{}» no es una expresión cron de cinco campos (minuto hora día mes día-de-la-semana){}.",
            shown(schedule),
            if seconds { " ni «N seconds»" } else { "" }
        )));
    }
    Ok(t)
}

/// The values of the form, checked. `choices`: the databases and roles
/// the server has.
pub(crate) fn spec(
    caps: &Caps,
    values: &FormValues,
    databases: &[(String, String)],
    roles: &[(String, String)],
    current: Option<&Job>,
) -> Result<Spec> {
    let v = Vals(&values.values);
    let name = if caps.name {
        Some(name_of(v, "name", "Nombre")?)
    } else {
        None
    };
    let schedule = check_schedule(&v.text("schedule"), caps.seconds)?;
    let command = v.raw("command");
    if command.trim().is_empty() {
        return Err(bad("«Comando» no puede quedar vacío.".into()));
    }
    let database = match v.opt("database") {
        Some(d) if current.is_some_and(|j| j.database == d) => d,
        Some(d) => pick(&d, databases, |n| {
            format!("La base «{n}» no existe en este servidor (o no acepta conexiones).")
        })?
        .to_string(),
        None => current.map_or(caps.database.clone(), |j| j.database.clone()),
    };
    let username = match v.opt("username") {
        Some(u) if current.is_some_and(|j| j.username == u) => u,
        Some(u) => pick(&u, roles, |n| {
            format!("El usuario «{n}» no existe en este servidor (o no puede iniciar sesión).")
        })?
        .to_string(),
        None => current.map_or(caps.user.clone(), |j| j.username.clone()),
    };
    Ok(Spec {
        name,
        schedule,
        command,
        database,
        username,
        active: v.bool_or("active", true),
    })
}

// ---------------------------------------------------------------------------
// Scripts: pg_cron
// ---------------------------------------------------------------------------

/// `cron.schedule_in_database` (or `cron.schedule`) for a new job.
pub(crate) fn cron_create(v: Variant, caps: &Caps, j: &Spec) -> Result<String> {
    let call = cron_schedule_call(v, caps, j)?;
    Ok(transaction(
        &format!(
            "pg_cron: nuevo job «{}»",
            j.name.as_deref().unwrap_or("(sin nombre)")
        ),
        &[format!("SELECT {call} AS jobid")],
    ))
}

/// The call that schedules `j`: `schedule_in_database` when the user may
/// and it says more than `schedule` can; refused when what it asks for
/// needs it and the user can't.
fn cron_schedule_call(v: Variant, caps: &Caps, j: &Spec) -> Result<String> {
    let plain = j.database == caps.database && j.username == caps.user && j.active;
    if caps.in_database && caps.name {
        let mut args = vec![
            format!(
                "job_name := {}",
                lit(v, j.name.as_deref().unwrap_or_default())
            ),
            format!("schedule := {}", lit(v, &j.schedule)),
            format!("command := {}", code_lit(&j.command)),
            format!("database := {}", lit(v, &j.database)),
            format!("username := {}", lit(v, &j.username)),
        ];
        if !j.active {
            args.push("active := false".into());
        }
        return Ok(format!("cron.schedule_in_database({})", args.join(", ")));
    }
    if !plain {
        return Err(bad(if j.username != caps.user {
            "Tu usuario no puede programar jobs de pg_cron para otro usuario.".into()
        } else if j.database != caps.database {
            "Tu usuario no puede programar jobs de pg_cron en otra base: necesita EXECUTE en cron.schedule_in_database.".into()
        } else {
            "Tu usuario no puede crear un job de pg_cron inactivo: necesita EXECUTE en cron.schedule_in_database.".into()
        }));
    }
    Ok(match &j.name {
        Some(name) => format!(
            "cron.schedule(job_name := {}, schedule := {}, command := {})",
            lit(v, name),
            lit(v, &j.schedule),
            code_lit(&j.command)
        ),
        None => format!(
            "cron.schedule(schedule := {}, command := {})",
            lit(v, &j.schedule),
            code_lit(&j.command)
        ),
    })
}

/// The change of a job in place, or `None` when nothing changed.
pub(crate) fn cron_alter(v: Variant, caps: &Caps, cur: &Job, j: &Spec) -> Result<Option<String>> {
    let renamed = caps.name && j.name != cur.name;
    let mut args = Vec::new();
    if j.schedule != cur.schedule {
        args.push(format!("schedule := {}", lit(v, &j.schedule)));
    }
    if j.command != cur.command {
        args.push(format!("command := {}", code_lit(&j.command)));
    }
    let mut extra = Vec::new();
    if j.database != cur.database {
        extra.push(format!("database := {}", lit(v, &j.database)));
    }
    if j.username != cur.username {
        extra.push(format!("username := {}", lit(v, &j.username)));
    }
    if caps.active && j.active != cur.active {
        extra.push(format!("active := {}", j.active));
    }
    if args.is_empty() && extra.is_empty() && !renamed {
        return Ok(None);
    }
    let title = j.name.clone().unwrap_or_else(|| format!("job {}", cur.id));
    // A new name the user can't write in place: schedule again, unschedule.
    if renamed && !caps.rename {
        let call = cron_schedule_call(v, caps, j)?;
        return Ok(Some(transaction(
            &format!(
                "pg_cron: «{}» pasa a llamarse «{}»\npg_cron no permite cambiarle el nombre a un job: se programa de nuevo y se borra el actual (el job cambia de id; su historial queda con el id anterior).",
                cur.name.as_deref().unwrap_or_default(),
                title
            ),
            &[
                format!("SELECT {call} AS jobid"),
                format!("SELECT cron.unschedule({})", cur.id),
            ],
        )));
    }
    let mut statements = Vec::new();
    if !args.is_empty() || !extra.is_empty() {
        if caps.alter {
            let mut all = vec![format!("job_id := {}", cur.id)];
            all.extend(args);
            all.extend(extra);
            statements.push(format!("SELECT cron.alter_job({})", all.join(", ")));
        } else if !extra.is_empty() {
            return Err(bad(
                "Tu usuario no puede cambiar la base, el usuario ni el estado de un job de pg_cron: necesita EXECUTE en cron.alter_job.".into(),
            ));
        } else {
            // pg_cron replaces the user's job of that name, keeping its id.
            let Some(name) = &cur.name else {
                return Err(bad(
                    "Tu usuario no puede cambiar un job de pg_cron sin nombre: necesita EXECUTE en cron.alter_job.".into(),
                ));
            };
            if cur.username != caps.user {
                return Err(bad(OTHERS_JOB.into()));
            }
            statements.push(format!(
                "SELECT cron.schedule(job_name := {}, schedule := {}, command := {})",
                lit(v, name),
                lit(v, &j.schedule),
                code_lit(&j.command)
            ));
        }
    }
    if renamed {
        statements.push(format!(
            "UPDATE cron.job SET jobname = {} WHERE jobid = {}",
            lit(v, j.name.as_deref().unwrap_or_default()),
            cur.id
        ));
    }
    Ok(Some(transaction(
        &format!("pg_cron: job «{title}»"),
        &statements,
    )))
}

// ---------------------------------------------------------------------------
// Scripts: tasks
// ---------------------------------------------------------------------------

/// `stmt` run as `owner` when that isn't the current user: `ALTER TASK` and
/// `DROP TASK` only find the current user's tasks.
fn as_owner(owner: &str, user: &str, stmt: String) -> Vec<String> {
    if owner == user {
        vec![stmt]
    } else {
        vec![
            format!("SET LOCAL ROLE {}", ident(owner)),
            stmt,
            "RESET ROLE".into(),
        ]
    }
}

fn task_create_statements(v: Variant, caps: &Caps, j: &Spec) -> Vec<String> {
    let name = j.name.as_deref().unwrap_or_default();
    let mut out = vec![format!(
        "CREATE TASK {} SCHEDULE {} DATABASE {} USER {} AS {}",
        ident(name),
        lit(v, &j.schedule),
        ident(&j.database),
        ident(&j.username),
        code_lit(&j.command)
    )];
    if !j.active {
        out.extend(as_owner(
            &j.username,
            &caps.user,
            format!("ALTER TASK {} NOT ACTIVE", ident(name)),
        ));
    }
    out
}

pub(crate) fn task_create(v: Variant, caps: &Caps, j: &Spec) -> String {
    transaction(
        &format!("Tarea: nueva «{}»", j.name.as_deref().unwrap_or_default()),
        &task_create_statements(v, caps, j),
    )
}

pub(crate) fn task_alter(v: Variant, caps: &Caps, cur: &Job, j: &Spec) -> Option<String> {
    let old = cur.name.clone().unwrap_or_default();
    let new = j.name.clone().unwrap_or_default();
    let renamed = new != old;
    let user_changed = j.username != cur.username;
    // No rename; another user's task changes as that user, who can't
    // change its user: drop and create.
    if renamed || (user_changed && cur.username != caps.user) {
        let mut statements = as_owner(
            &cur.username,
            &caps.user,
            format!("DROP TASK {}", ident(&old)),
        );
        statements.extend(task_create_statements(v, caps, j));
        return Some(transaction(
            &format!(
                "Tarea: «{old}»{}\nEl servidor no permite cambiarle el nombre a una tarea, ni el usuario a la de otro: se borra y se crea de nuevo (otro id, sin su historial de ejecuciones).",
                if renamed { format!(" pasa a llamarse «{new}»") } else { String::new() }
            ),
            &statements,
        ));
    }
    let mut clauses = Vec::new();
    if j.schedule != cur.schedule {
        clauses.push(format!("SCHEDULE {}", lit(v, &j.schedule)));
    }
    if j.database != cur.database {
        clauses.push(format!("DATABASE {}", ident(&j.database)));
    }
    if user_changed {
        clauses.push(format!("USER {}", ident(&j.username)));
    }
    if j.active != cur.active {
        clauses.push(if j.active { "ACTIVE" } else { "NOT ACTIVE" }.into());
    }
    if j.command != cur.command {
        clauses.push(format!("AS {}", code_lit(&j.command)));
    }
    if clauses.is_empty() {
        return None;
    }
    Some(transaction(
        &format!("Tarea: «{old}»"),
        &as_owner(
            &cur.username,
            &caps.user,
            format!("ALTER TASK {} {}", ident(&old), clauses.join(" ")),
        ),
    ))
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// Another job of the user with that name: pg_cron would replace it, the
/// server refuses a task.
async fn name_taken(
    s: &PgSession,
    flavor: Flavor,
    name: &str,
    user: &str,
    except: Option<i64>,
) -> Result<bool> {
    let not = except
        .map(|n| format!(" AND jobid <> {n}"))
        .unwrap_or_default();
    Ok(!s
        .text(&format!(
            "SELECT 1 FROM {} WHERE jobname = {} AND username = {}{not} LIMIT 1",
            flavor.table(),
            lit(s.variant, name),
            lit(s.variant, user)
        ))
        .await?
        .is_empty())
}

pub(crate) async fn plan(
    s: &PgSession,
    c: &Ctx,
    flavor: Flavor,
    n: Option<i64>,
    values: &FormValues,
) -> Result<Plan> {
    let caps = caps_for(c, flavor)?;
    let cur = match n {
        Some(n) => Some(read(s, flavor, caps, n).await?),
        None => None,
    };
    if let Some(j) = &cur {
        if !caps.may_change(&j.username) {
            return Err(bad(match flavor {
                Flavor::Cron => OTHERS_JOB.into(),
                Flavor::Task => OTHERS_TASK.into(),
            }));
        }
    }
    let dbs = databases(s).await?;
    let roles = login_roles(s).await?;
    let j = spec(caps, values, &dbs, &roles, cur.as_ref())?;
    if !caps.superuser
        && j.username != caps.user
        && cur.as_ref().is_none_or(|c| c.username != j.username)
    {
        return Err(bad(
            "Solo un superusuario puede programar jobs para otro usuario.".into(),
        ));
    }
    if let Some(name) = &j.name {
        let renamed = cur
            .as_ref()
            .is_none_or(|c| c.name.as_ref() != Some(name) || c.username != j.username);
        if renamed && name_taken(s, flavor, name, &j.username, n).await? {
            return Err(bad(format!(
                "Ya hay un {} llamado «{}» del usuario «{}»: elegí otro nombre.",
                match flavor {
                    Flavor::Cron => "job de pg_cron",
                    Flavor::Task => "tarea",
                },
                shown(name),
                shown(&j.username)
            )));
        }
    }
    let v = s.variant;
    match (flavor, &cur) {
        (Flavor::Cron, None) => Ok(Plan {
            script: cron_create(v, caps, &j)?,
            empty: false,
            id: None,
            new_id: Some(NewId::Column("jobid", flavor.prefix())),
        }),
        (Flavor::Cron, Some(cur)) => match cron_alter(v, caps, cur, &j)? {
            None => Ok(Plan::nothing(
                cur.name.as_deref().unwrap_or("job"),
                flavor.id(cur.id).to_string(),
            )),
            Some(script) => {
                let recreated = caps.name && j.name != cur.name && !caps.rename;
                Ok(Plan {
                    script,
                    empty: false,
                    id: Some(flavor.id(cur.id).to_string()),
                    new_id: recreated.then_some(NewId::Column("jobid", flavor.prefix())),
                })
            }
        },
        (Flavor::Task, None) => Ok(Plan {
            script: task_create(v, caps, &j),
            empty: false,
            id: None,
            new_id: Some(NewId::Query(task_lookup(v, &j), flavor.prefix())),
        }),
        (Flavor::Task, Some(cur)) => match task_alter(v, caps, cur, &j) {
            None => Ok(Plan::nothing(
                cur.name.as_deref().unwrap_or("tarea"),
                flavor.id(cur.id).to_string(),
            )),
            Some(script) => Ok(Plan {
                script,
                empty: false,
                id: Some(flavor.id(cur.id).to_string()),
                new_id: Some(NewId::Query(task_lookup(v, &j), flavor.prefix())),
            }),
        },
    }
}

/// The task's id once created: its name is unique among its user's.
fn task_lookup(v: Variant, j: &Spec) -> String {
    format!(
        "SELECT jobid::text AS id FROM pg_catalog.pg_task WHERE jobname = {} AND username = {}",
        lit(v, j.name.as_deref().unwrap_or_default()),
        lit(v, &j.username)
    )
}

pub(crate) async fn delete_script(
    s: &PgSession,
    c: &Ctx,
    flavor: Flavor,
    n: i64,
) -> Result<String> {
    let caps = caps_for(c, flavor)?;
    let j = read(s, flavor, caps, n).await?;
    if !caps.may_change(&j.username) {
        return Err(bad(match flavor {
            Flavor::Cron => OTHERS_JOB.into(),
            Flavor::Task => OTHERS_TASK.into(),
        }));
    }
    let name = j.name.clone().unwrap_or_else(|| format!("job {n}"));
    Ok(match flavor {
        Flavor::Cron => format!(
            "-- pg_cron: borrar el job «{}» (su historial queda en cron.job_run_details)\nSELECT cron.unschedule({n});\n",
            comment_text(&name)
        ),
        Flavor::Task => transaction(
            &format!("Tarea: borrar «{name}» (y su historial)"),
            &as_owner(&j.username, &caps.user, format!("DROP TASK {}", ident(&name))),
        ),
    })
}

pub(crate) async fn clear_script(s: &PgSession, c: &Ctx, flavor: Flavor, n: i64) -> Result<String> {
    let caps = caps_for(c, flavor)?;
    if flavor == Flavor::Task {
        return Err(Error::Unsupported(
            "el historial de las tareas es un catálogo del sistema (pg_task_run_history): el servidor no permite borrarlo".into(),
        ));
    }
    if !caps.runs {
        return Err(Error::Unsupported(
            "este pg_cron no guarda el historial de ejecuciones (cron.job_run_details)".into(),
        ));
    }
    if !caps.clear {
        return Err(bad(
            "Tu usuario no puede borrar el historial de pg_cron (DELETE en cron.job_run_details)."
                .into(),
        ));
    }
    let j = read(s, flavor, caps, n).await?;
    if !caps.may_change(&j.username) {
        return Err(bad(OTHERS_JOB.into()));
    }
    Ok(format!(
        "-- pg_cron: borrar el historial del job {n}\nDELETE FROM cron.job_run_details WHERE jobid = {n};\n"
    ))
}

/// A row of the tests: a job as `read` would answer it.
#[cfg(test)]
pub(crate) fn job_for_tests(
    id: i64,
    name: Option<&str>,
    schedule: &str,
    command: &str,
    database: &str,
    username: &str,
    active: bool,
) -> Job {
    Job {
        id,
        name: name.map(Into::into),
        schedule: schedule.into(),
        command: command.into(),
        database: database.into(),
        username: username.into(),
        active,
        node: Some("localhost:5432".into()),
    }
}

/// The values a form for `j` carries (the tests build edits from them).
#[cfg(test)]
pub(crate) fn values_of(j: &Job) -> BTreeMap<String, serde_json::Value> {
    BTreeMap::from([
        (
            "name".to_string(),
            json!(j.name.clone().unwrap_or_default()),
        ),
        ("schedule".to_string(), json!(j.schedule)),
        ("command".to_string(), json!(j.command)),
        ("database".to_string(), json!(j.database)),
        ("username".to_string(), json!(j.username)),
        ("active".to_string(), json!(j.active)),
    ])
}
