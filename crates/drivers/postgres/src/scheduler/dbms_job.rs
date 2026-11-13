//! DBMS_JOB jobs in the designer: openGauss (through `pkg_service`), EDB
//! Postgres Advanced Server and KingbaseES (the `DBMS_JOB` package).
//!
//! A job is a command (`what`), its next run and an interval (a date
//! expression evaluated after each run, such as `sysdate + 1`; none: it
//! runs once), and whether it's broken (disabled).
//!
//! - openGauss: `pkg_service.job_submit` / `job_update` / `job_finish` /
//!   `job_cancel`, in `BEGIN` … `COMMIT`. Its times are the server's local
//!   time. openGauss declares those functions `IMMUTABLE`: called with
//!   constant arguments, the planner runs them while it plans the statement,
//!   which fails ("destMax is too small") or brings the server down
//!   (openGauss 7.0.0-RC3). The job's id always goes in a subquery
//!   ([`gauss_id`]), which keeps the call for execution.
//! - EDB: `DBMS_JOB.SUBMIT` / `WHAT` / `NEXT_DATE` / `INTERVAL` / `BROKEN`
//!   / `REMOVE` in one anonymous block. Before EPAS 16 the packages keep
//!   their jobs in pgAgent: those are pgAgent jobs and change there.
//! - KingbaseES: the same procedures with `CALL`, in `BEGIN` … `COMMIT`; a
//!   new job is submitted in an anonymous block. Its jobs are known by
//!   their name in `kdb_job` (`internal_job<n>:…`), `<n>` the number the
//!   package takes.
//!
//! A user changes their own jobs; a superuser (openGauss: a system
//! administrator) anyone's. EDB and KingbaseES weren't at hand: their part
//! follows the vendors' documentation.

use super::{bad, shown, valid_datetime, Vals};
use crate::catalog::{comment_text, lit};
use crate::jobs::edb::Edb;
use crate::jobs::kingbase::{KJob, Kdb};
use crate::jobs::scheduler::Dialect;
use crate::jobs::{flag, text, time, Found, JobId, Row};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result};
use serde_json::json;

const OTHERS: &str =
    "Este job es de otro usuario: solo su dueño o un superusuario pueden cambiarlo.";
const GAUSS_TIMES: &str =
    "openGauss guarda las fechas de sus jobs en la hora local del servidor, sin zona horaria.";
const NAME_ONLY: &str = "El nombre del job lo pone el servidor: DBMS_JOB no lo cambia.";
const EDB_PGAGENT: &str = "En este servidor DBMS_JOB guarda sus jobs en pgAgent: se crean y se cambian como jobs de pgAgent.";
const KDB_NUMBER: &str =
    "DBine no puede saber el número de DBMS_JOB de este job: se ve tal como está.";

/// The engine and what DBine reads its jobs with.
pub(crate) enum Engine {
    Gauss,
    Edb(Box<Edb>),
    Kdb(Box<Kdb>),
}

/// DBMS_JOB here, and what the user may do.
pub(crate) struct Caps {
    pub(crate) engine: Engine,
    pub(crate) user: String,
    pub(crate) admin: bool,
    pub(crate) database: String,
    /// The user can read the jobs.
    pub(crate) readable: bool,
    /// Why the scheduler starts nothing, if it doesn't.
    pub(crate) off: Option<String>,
    /// EDB: the user may delete edb_job_scheduler's runs (`DELETE` on
    /// `sys.job_run_details`).
    pub(crate) history_delete: bool,
}

impl Caps {
    pub(crate) fn may_change(&self, owner: Option<&str>) -> bool {
        self.admin || owner == Some(self.user.as_str())
    }

    /// Whether the user may change job `id` of `owner` (as the jobs list
    /// has it: openGauss's `log_user`, EDB's `log_user`; KingbaseES's
    /// views have none, its own jobs are by name). The one rule the tree
    /// and every script follow.
    pub(crate) fn may_job(&self, id: &JobId, owner: Option<&str>) -> bool {
        match (&self.engine, id) {
            (Engine::Kdb(k), JobId::Kingbase(n)) => k.may(&n.name),
            _ => self.may_change(owner),
        }
    }

    pub(crate) fn notes(&self) -> Vec<String> {
        let mut out = Vec::new();
        if !self.readable {
            out.push(match self.engine {
                Engine::Gauss => "Tu usuario no puede leer los jobs de openGauss (pg_job).".to_string(),
                Engine::Edb(_) => "Tu usuario no puede leer los jobs de DBMS_JOB: necesita SELECT en sys.all_jobs.".into(),
                Engine::Kdb(_) => "Tu usuario no puede leer los jobs de kdb_schedule: necesita SELECT en kdb_job o en kdb_schedule.dba_scheduler_jobs.".into(),
            });
        }
        out.extend(self.off.clone());
        out
    }

    fn dialect(&self) -> D {
        match self.engine {
            Engine::Gauss => D::Gauss,
            Engine::Edb(_) => D::Edb,
            Engine::Kdb(_) => D::Kdb,
        }
    }

    /// "Ejecutar ahora" is `DBMS_JOB.RUN` (EDB, KingbaseES), which runs
    /// the job in the session; openGauss has no such call
    /// (`pkg_service` has none), so its jobs are "programar para ahora".
    pub(crate) fn runs_here(&self) -> bool {
        !matches!(self.engine, Engine::Gauss)
    }

    /// edb_job_scheduler keeps the runs (`sys.job_run_details`), and the
    /// user can read them and delete them.
    pub(crate) fn edb_history(&self) -> bool {
        match &self.engine {
            Engine::Edb(e) => self.history_delete && e.history().is_some(),
            _ => false,
        }
    }
}

/// The user may delete the runs in edb_job_scheduler's
/// `sys.job_run_details` (false when they can't read it).
async fn edb_history_delete(s: &PgSession, e: &Edb) -> Result<bool> {
    let Some((_, runs)) = e.history() else {
        return Ok(false);
    };
    let table = format!(
        "{}.{}",
        crate::jobs::ident(&runs.schema),
        crate::jobs::ident(&runs.name)
    );
    Ok(s.text(&format!(
        "SELECT has_table_privilege({}, 'DELETE') AS may",
        lit(Variant::Edb, &table)
    ))
    .await?
    .first()
    .is_some_and(|r| flag(r, "may")))
}

/// openGauss's DBMS_JOB: always there.
pub(crate) async fn gauss_caps(s: &PgSession, f: &Found) -> Result<Caps> {
    let rows = s
        .text(
            "SELECT current_user AS usr,
                    coalesce((SELECT rolsuper OR rolsystemadmin FROM pg_catalog.pg_roles WHERE rolname = current_user), false) AS admin,
                    has_table_privilege('pg_catalog.pg_job', 'SELECT') AS readable",
        )
        .await?;
    let r = rows.first();
    Ok(Caps {
        engine: Engine::Gauss,
        user: r.and_then(|r| text(r, "usr")).unwrap_or_default(),
        admin: r.is_some_and(|r| flag(r, "admin")),
        database: f.database.clone(),
        readable: r.is_some_and(|r| flag(r, "readable")),
        off: (f.job_queue_processes.as_deref() == Some("0")).then(|| {
            "El programador de jobs de openGauss está apagado (job_queue_processes = 0): sus jobs no se ejecutan.".to_string()
        }),
        history_delete: false,
    })
}

/// EDB's DBMS_JOB, when this database has the package and it isn't
/// pgAgent's (before EPAS 16).
pub(crate) async fn edb_caps(s: &PgSession, f: &Found) -> Result<(Option<Caps>, Option<String>)> {
    let e = Edb::read(s, f).await?;
    let present = e.cat.get("dba_jobs").is_some() || e.cat.get("all_jobs").is_some();
    if !present {
        return Ok((None, None));
    }
    if e.on_pgagent() {
        return Ok((None, Some(EDB_PGAGENT.into())));
    }
    let history_delete = edb_history_delete(s, &e).await?;
    Ok((
        Some(Caps {
            user: f.user.clone(),
            admin: f.superuser,
            database: f.database.clone(),
            readable: e.dbms_jobs().is_some(),
            off: e.off(),
            history_delete,
            engine: Engine::Edb(Box::new(e)),
        }),
        None,
    ))
}

/// KingbaseES's DBMS_JOB (kdb_schedule), when this database has it.
pub(crate) async fn kdb_caps(s: &PgSession, f: &Found) -> Result<Option<Caps>> {
    let k = Kdb::read(s, f).await?;
    if !k.present() {
        return Ok(None);
    }
    Ok(Some(Caps {
        user: f.user.clone(),
        admin: f.superuser,
        database: f.database.clone(),
        readable: k.kdb_job().is_some() || k.jobs().is_some(),
        off: k.queue_off.then(|| {
            "El programador de jobs de KingbaseES está apagado (job_queue_processes = 0): sus jobs no se ejecutan.".to_string()
        }),
        history_delete: false,
        engine: Engine::Kdb(Box::new(k)),
    }))
}

/// The dialect of the calls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum D {
    Gauss,
    Edb,
    Kdb,
}

impl D {
    fn variant(self) -> Variant {
        match self {
            D::Gauss => Variant::OpenGauss,
            D::Edb => Variant::Edb,
            D::Kdb => Variant::Kingbase,
        }
    }

    fn lit(self, s: &str) -> String {
        lit(self.variant(), s)
    }

    fn engine(self) -> &'static str {
        match self {
            D::Gauss => "openGauss",
            D::Edb => "EDB Postgres Advanced Server",
            D::Kdb => "KingbaseES",
        }
    }
}

// ---------------------------------------------------------------------------
// The job as the server has it
// ---------------------------------------------------------------------------

/// A DBMS_JOB job.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub(crate) struct Job {
    pub(crate) number: i64,
    pub(crate) what: String,
    /// Its next run as the form shows it; `None` while it's broken.
    pub(crate) next: Option<String>,
    /// `None`: it runs once.
    pub(crate) interval: Option<String>,
    pub(crate) enabled: bool,
    pub(crate) owner: Option<String>,
    pub(crate) schema: Option<String>,
    pub(crate) database: Option<String>,
    pub(crate) name: Option<String>,
    pub(crate) last: Option<String>,
    pub(crate) failures: Option<String>,
    pub(crate) error: Option<String>,
}

/// An interval as the job keeps it: `null` means none.
fn interval_of(i: Option<String>) -> Option<String> {
    i.filter(|i| !i.trim().eq_ignore_ascii_case("null"))
}

fn gauss_sql(n: i64) -> String {
    format!(
        "SELECT j.job_id::text AS id, j.log_user, j.priv_user, j.dbname, j.nspname, j.job_status, j.job_name,
                to_char(j.next_run_date, 'YYYY-MM-DD\"T\"HH24:MI:SS') AS next_local, j.interval,
                extract(epoch FROM (j.last_start_date AT TIME ZONE current_setting('TimeZone')))::text AS last_start,
                j.failure_count::text AS failures, j.failure_msg, p.what
         FROM pg_catalog.pg_job j LEFT JOIN pg_catalog.pg_job_proc p ON p.job_id = j.job_id
         WHERE j.job_id = {n}"
    )
}

pub(crate) fn gauss_job(r: &dyn Row) -> Option<Job> {
    let enabled = text(r, "job_status").as_deref() != Some("d");
    Some(Job {
        number: text(r, "id")?.parse().ok()?,
        what: r.get("what").unwrap_or_default(),
        next: text(r, "next_local").filter(|_| enabled),
        interval: interval_of(text(r, "interval")),
        enabled,
        owner: text(r, "log_user"),
        schema: text(r, "nspname"),
        database: text(r, "dbname"),
        name: text(r, "job_name"),
        last: time(r, "last_start"),
        failures: text(r, "failures"),
        error: text(r, "failure_msg"),
    })
}

pub(crate) fn edb_job(r: &dyn Row) -> Option<Job> {
    let enabled = !crate::jobs::edb::broken(r);
    Some(Job {
        number: text(r, "job")?.parse().ok()?,
        what: r.get("what").unwrap_or_default(),
        next: text(r, "next_iso").filter(|_| enabled && crate::jobs::edb::next(r).is_some()),
        interval: interval_of(text(r, "job_interval")),
        enabled,
        owner: text(r, "log_user"),
        schema: text(r, "schema_user"),
        database: None,
        name: None,
        last: time(r, "last_date"),
        failures: text(r, "failures"),
        error: None,
    })
}

fn kdb_job(j: &KJob, n: i64) -> Job {
    let enabled = j.enabled();
    Job {
        number: n,
        what: j.action().unwrap_or_default().to_string(),
        next: j
            .next_iso
            .clone()
            .or_else(|| j.view.as_ref().and_then(|v| v.next_iso.clone()))
            .filter(|_| enabled),
        interval: interval_of(j.view.as_ref().and_then(|v| v.interval.clone())),
        enabled,
        owner: None,
        schema: None,
        database: None,
        name: Some(j.name.clone()),
        last: j.last.clone(),
        failures: j.view.as_ref().and_then(|v| v.failure_count.clone()),
        error: None,
    }
}

/// The job, and who owns it (to tell whether the user may change it).
pub(crate) async fn read(s: &PgSession, c: &Caps, id: &JobId) -> Result<Job> {
    let missing = || bad(format!("no existe el job {id} (o tu usuario no lo ve)"));
    match (&c.engine, id) {
        (Engine::Gauss, JobId::GaussJob(n)) => s
            .text(&gauss_sql(*n))
            .await?
            .first()
            .and_then(|r| gauss_job(r))
            .ok_or_else(missing),
        (Engine::Edb(e), JobId::EdbJob(n)) => {
            let r = crate::jobs::edb::job_row(s, e, *n).await?;
            edb_job(&r).ok_or_else(missing)
        }
        (Engine::Kdb(k), JobId::Kingbase(named)) => {
            let j = crate::jobs::kingbase::row(s, k, named).await?;
            let n = j.number().ok_or_else(|| bad(KDB_NUMBER.into()))?;
            let n = j.checked(n)?;
            Ok(kdb_job(&j, n))
        }
        _ => Err(missing()),
    }
}

/// Whether the user may change `job`.
pub(crate) fn may(c: &Caps, id: &JobId, job: &Job) -> bool {
    c.may_job(id, job.owner.as_deref())
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

pub(crate) fn fields(d: D, job: Option<&Job>) -> Vec<FormSection> {
    let mut general = Vec::new();
    if let Some(j) = job {
        general.push(FormField::new("number", "Número", FormKind::Text).read_only(true));
        if j.name.is_some() {
            general.push(
                FormField::new("job_name", "Nombre", FormKind::Text)
                    .read_only(true)
                    .help(NAME_ONLY),
            );
        }
        general.push(FormField::new("owner", "Usuario", FormKind::Text).read_only(true));
        if j.schema.is_some() {
            general.push(FormField::new("schema", "Esquema", FormKind::Text).read_only(true));
        }
        if d == D::Gauss {
            general.push(
                FormField::new("database", "Base de datos", FormKind::Text)
                    .read_only(true)
                    .help("openGauss ejecuta el job en la base en la que se creó."),
            );
        }
        general.push(
            FormField::new("last_run", "Última ejecución", FormKind::DateTime).read_only(true),
        );
        general.push(FormField::new("failures", "Fallas seguidas", FormKind::Text).read_only(true));
        if d == D::Gauss {
            general.push(
                FormField::new("last_error", "Último error", FormKind::Textarea).read_only(true),
            );
        }
    } else if d == D::Gauss {
        general.push(
            FormField::new("database", "Base de datos", FormKind::Text)
                .read_only(true)
                .help("openGauss crea el job en la base de la sesión: para otra base, conectate a ella."),
        );
    }
    general.push(
        FormField::new("enabled", "Habilitado", FormKind::Bool)
            .default(json!(true))
            .help("Deshabilitado (broken), el job no se ejecuta hasta habilitarlo."),
    );
    let command = FormSection::new("command", "Comando").field(
        FormField::new(
            "what",
            "Comando",
            FormKind::code(if d == D::Gauss { "sql" } else { "plsql" }),
        )
        .required()
        .placeholder("CALL purgar_logs();")
        .help("Lo que ejecuta el job: sentencias terminadas en punto y coma, como en el cuerpo de un bloque."),
    );
    let (next_help, interval_help) = match d {
        D::Gauss => (
            "En la hora local del servidor. Vacía: ahora (sysdate).",
            "Una expresión de fecha que da la próxima ejecución después de cada una: sysdate + 1 (cada día), trunc(sysdate) + 1 + 3/24 (cada día a las 3), sysdate + interval '1 hour'. Vacía: se ejecuta una sola vez.",
        ),
        _ => (
            "Vacía: ahora (SYSDATE).",
            "Una expresión de fecha que da la próxima ejecución después de cada una: SYSDATE + 1 (cada día), TRUNC(SYSDATE) + 1 + 3/24 (cada día a las 3). Vacía: se ejecuta una sola vez.",
        ),
    };
    let schedule = FormSection::new("schedule", "Programación").fields(vec![
        FormField::new("next_date", "Próxima ejecución", FormKind::DateTime).help(next_help),
        FormField::new("interval", "Intervalo", FormKind::Text)
            .placeholder(if d == D::Gauss {
                "sysdate + 1"
            } else {
                "SYSDATE + 1"
            })
            .help(interval_help),
    ]);
    vec![
        FormSection::new("general", "General").fields(general),
        command,
        schedule,
    ]
}

pub(crate) fn values(
    d: D,
    job: Option<&Job>,
    database: &str,
) -> std::collections::BTreeMap<String, serde_json::Value> {
    let mut m = std::collections::BTreeMap::new();
    match job {
        None => {
            m.insert("enabled".into(), json!(true));
            if d == D::Gauss {
                m.insert("database".into(), json!(database));
            }
        }
        Some(j) => {
            m.insert("number".into(), json!(j.number.to_string()));
            if let Some(n) = &j.name {
                m.insert("job_name".into(), json!(n));
            }
            m.insert("owner".into(), json!(j.owner.clone().unwrap_or_default()));
            m.insert("schema".into(), json!(j.schema.clone().unwrap_or_default()));
            m.insert(
                "database".into(),
                json!(j.database.clone().unwrap_or_default()),
            );
            m.insert("last_run".into(), json!(j.last.clone().unwrap_or_default()));
            m.insert(
                "failures".into(),
                json!(j.failures.clone().unwrap_or_default()),
            );
            m.insert(
                "last_error".into(),
                json!(j.error.clone().unwrap_or_default()),
            );
            m.insert("enabled".into(), json!(j.enabled));
            m.insert("what".into(), json!(j.what));
            m.insert(
                "next_date".into(),
                json!(j.next.clone().unwrap_or_default()),
            );
            m.insert(
                "interval".into(),
                json!(j.interval.clone().unwrap_or_default()),
            );
        }
    }
    m
}

pub(crate) async fn form(s: &PgSession, c: &Caps, id: Option<&JobId>) -> Result<ObjectForm> {
    let d = c.dialect();
    let job = match id {
        Some(id) => Some(read(s, c, id).await?),
        None => None,
    };
    let mut f = ObjectForm {
        sections: fields(d, job.as_ref()),
        values: values(d, job.as_ref(), &c.database),
        notes: c.notes(),
        ..Default::default()
    };
    if d == D::Gauss {
        f.notes.push(GAUSS_TIMES.into());
    }
    if let (Some(id), Some(j)) = (id, &job) {
        if !may(c, id, j) {
            f.read_only = true;
            f.notes.insert(0, OTHERS.into());
        }
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// What the form asks for, and the scripts
// ---------------------------------------------------------------------------

/// A job as the form asks for it, checked.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Spec {
    pub(crate) what: String,
    pub(crate) next: Option<String>,
    pub(crate) interval: Option<String>,
    pub(crate) enabled: bool,
}

pub(crate) fn spec(values: &FormValues) -> Result<Spec> {
    let v = Vals(&values.values);
    let what = v.raw("what");
    if what.trim().is_empty() {
        return Err(bad("«Comando» no puede quedar vacío.".into()));
    }
    let next = match v.opt("next_date") {
        None => None,
        Some(t) if valid_datetime(&t) => Some(t),
        Some(t) => {
            return Err(bad(format!(
                "«Próxima ejecución»: «{}» no es una fecha y hora válida (AAAA-MM-DDTHH:MM:SS).",
                shown(&t)
            )))
        }
    };
    let interval = interval_of(v.opt("interval"));
    if let Some(i) = &interval {
        if i.chars().any(char::is_control) {
            return Err(bad(
                "«Intervalo» no puede tener saltos de línea ni caracteres de control.".into(),
            ));
        }
        if i.chars().count() > 4000 {
            return Err(bad("«Intervalo» es demasiado largo.".into()));
        }
    }
    Ok(Spec {
        what,
        next,
        interval,
        enabled: v.bool_or("enabled", true),
    })
}

/// A time for the call: openGauss takes the server's local time (an
/// offset is turned into it); EDB and KingbaseES a `timestamptz`.
pub(crate) fn time_sql(d: D, t: &str) -> String {
    match d {
        D::Gauss => {
            let (_, rest) = t.split_once(['T', ' ']).unwrap_or((t, ""));
            let zoned = rest.contains(['Z', 'z', '+']) || rest.contains('-');
            if zoned {
                format!("({}::timestamptz)::timestamp", d.lit(t))
            } else {
                format!("{}::timestamp", d.lit(&t.replacen('T', " ", 1)))
            }
        }
        D::Edb => Dialect::Edb.time_lit(t),
        D::Kdb => Dialect::Kingbase.time_lit(t),
    }
}

fn now_sql(d: D) -> &'static str {
    match d {
        D::Gauss => "sysdate",
        D::Edb => "SYSDATE",
        D::Kdb => "now()",
    }
}

fn next_sql(d: D, next: Option<&str>) -> String {
    next.map(|t| time_sql(d, t))
        .unwrap_or_else(|| now_sql(d).to_string())
}

fn interval_sql(d: D, i: Option<&str>) -> String {
    d.lit(i.unwrap_or("null"))
}

/// A job id for a `pkg_service` call: in a subquery, so the call isn't a
/// constant the planner runs (see the module's notes).
pub(crate) fn gauss_id(n: Option<i64>) -> String {
    match n {
        Some(n) => format!("(SELECT {n}::bigint)"),
        None => "(SELECT NULL::bigint)".into(),
    }
}

const GAUSS_PLAN: &str = "pkg_service es IMMUTABLE en openGauss: el id va en una subconsulta para que el servidor no ejecute la llamada al planificar la sentencia.";

/// A new job. `id`: the number an openGauss job created disabled takes
/// (`job_finish` doesn't see a job submitted in the same statement).
pub(crate) fn create(d: D, j: &Spec, id: Option<i64>) -> String {
    let what = d.lit(&j.what);
    let next = next_sql(d, j.next.as_deref());
    let interval = interval_sql(d, j.interval.as_deref());
    match d {
        D::Gauss => {
            let submit = format!(
                "pkg_service.job_submit({}, {what}, {next}, {interval})",
                gauss_id(id)
            );
            if j.enabled {
                format!("-- openGauss: nuevo job (DBMS_JOB)\n-- {GAUSS_PLAN}\nSELECT {submit} AS job;\n")
            } else {
                super::transaction(
                    &format!("openGauss: nuevo job (DBMS_JOB), deshabilitado\n{GAUSS_PLAN}"),
                    &[
                        format!("SELECT {submit} AS job"),
                        format!("SELECT pkg_service.job_finish({}, true)", gauss_id(id)),
                    ],
                )
            }
        }
        D::Edb | D::Kdb => {
            let broken = if j.enabled {
                String::new()
            } else {
                "  DBMS_JOB.BROKEN(j, TRUE);\n".to_string()
            };
            format!(
                "-- {}: nuevo job (DBMS_JOB)\nDECLARE\n  j INTEGER;\nBEGIN\n  DBMS_JOB.SUBMIT(j, {what}, {next}, {interval});\n{broken}END;\n",
                d.engine()
            )
        }
    }
}

/// The change of job `n` in place, only what changed; `None`: nothing did.
pub(crate) fn alter(d: D, n: i64, cur: &Job, j: &Spec) -> Option<String> {
    let what_changed = j.what != cur.what;
    let interval_changed = j.interval != cur.interval;
    // A broken job's next run is far away: enabling it sets the one asked
    // for (or now).
    let next_changed = j.next.is_some() && j.next != cur.next;
    let enable_changed = j.enabled != cur.enabled;
    if !what_changed && !interval_changed && !next_changed && !enable_changed {
        return None;
    }
    let title = format!("{}: job {n} (DBMS_JOB)", d.engine());
    let next = next_sql(d, j.next.as_deref());
    match d {
        D::Gauss => {
            let mut st = Vec::new();
            let id = gauss_id(Some(n));
            // Enabling sets the next run with job_finish; disabling drops it.
            let pass_next = next_changed && !enable_changed && j.enabled;
            if what_changed || interval_changed || pass_next {
                let content = what_changed.then(|| d.lit(&j.what));
                st.push(format!(
                    "SELECT pkg_service.job_update({id}, {}, {}, {})",
                    if pass_next {
                        next.clone()
                    } else {
                        "NULL".into()
                    },
                    if interval_changed {
                        interval_sql(d, j.interval.as_deref())
                    } else {
                        "NULL".into()
                    },
                    content.unwrap_or_else(|| "NULL".into())
                ));
            }
            if enable_changed {
                st.push(if j.enabled {
                    format!("SELECT pkg_service.job_finish({id}, false, {next})")
                } else {
                    format!("SELECT pkg_service.job_finish({id}, true)")
                });
            }
            Some(super::transaction(&format!("{title}\n{GAUSS_PLAN}"), &st))
        }
        D::Edb | D::Kdb => {
            let mut calls = Vec::new();
            if what_changed {
                calls.push(format!("DBMS_JOB.WHAT({n}, {})", d.lit(&j.what)));
            }
            if interval_changed {
                calls.push(format!(
                    "DBMS_JOB.INTERVAL({n}, {})",
                    interval_sql(d, j.interval.as_deref())
                ));
            }
            if enable_changed {
                calls.push(if j.enabled {
                    format!("DBMS_JOB.BROKEN({n}, FALSE, {next})")
                } else {
                    format!("DBMS_JOB.BROKEN({n}, TRUE)")
                });
            } else if next_changed {
                calls.push(format!("DBMS_JOB.NEXT_DATE({n}, {next})"));
            }
            Some(match d {
                D::Edb => format!(
                    "-- {}\nBEGIN\n{}END;\n",
                    comment_text(&title),
                    calls
                        .iter()
                        .map(|c| format!("  {c};\n"))
                        .collect::<String>()
                ),
                _ => super::transaction(
                    &title,
                    &calls
                        .iter()
                        .map(|c| format!("CALL {c}"))
                        .collect::<Vec<_>>(),
                ),
            })
        }
    }
}

pub(crate) fn delete(d: D, n: i64) -> String {
    match d {
        D::Gauss => format!(
            "-- openGauss: borrar el job {n} (DBMS_JOB)\n-- {GAUSS_PLAN}\nSELECT pkg_service.job_cancel({});\n",
            gauss_id(Some(n))
        ),
        D::Edb => format!(
            "-- EDB Postgres Advanced Server: borrar el job {n} (DBMS_JOB)\nBEGIN\n  DBMS_JOB.REMOVE({n});\nEND;\n"
        ),
        D::Kdb => format!("-- KingbaseES: borrar el job {n} (DBMS_JOB)\nCALL DBMS_JOB.REMOVE({n});\n"),
    }
}

// ---------------------------------------------------------------------------
// Plans
// ---------------------------------------------------------------------------

/// How a new job's id is learned.
fn new_id(c: &Caps) -> Option<super::NewId> {
    match &c.engine {
        Engine::Gauss => Some(super::NewId::Column("job", "dbms_job:")),
        Engine::Edb(e) => {
            let (v, _) = e.dbms_jobs()?;
            Some(super::NewId::Query(
                format!(
                    "SELECT max({})::text AS id FROM {} j WHERE {} = current_user",
                    v.col("j", "job")?,
                    v.from(),
                    v.col("j", "log_user")?
                ),
                "edb_job:",
            ))
        }
        Engine::Kdb(k) => {
            let kj = k.kdb_job()?;
            let name = kj.col("k", "jobname")?;
            Some(super::NewId::Query(
                format!(
                    "SELECT '\"' || replace({name}::text, '\"', '\"\"') || '\"' AS id FROM {} k
                     WHERE {name}::text LIKE 'internal\\_job%' ORDER BY {} DESC LIMIT 1",
                    kj.from(),
                    kj.col("k", "jobid")?
                ),
                "kdb_schedule:",
            ))
        }
    }
}

pub(crate) async fn plan(
    s: &PgSession,
    c: &Caps,
    id: Option<&JobId>,
    values: &FormValues,
) -> Result<super::Plan> {
    let d = c.dialect();
    let j = spec(values)?;
    match id {
        None => {
            // A job created disabled takes a number chosen here.
            let id = if d == D::Gauss && !j.enabled {
                let free = s
                    .text(
                        "SELECT min(g)::text AS id FROM generate_series(1, 32767) g
                         WHERE g NOT IN (SELECT job_id FROM pg_catalog.pg_job)",
                    )
                    .await?
                    .first()
                    .and_then(|r| text(r, "id"))
                    .and_then(|n| n.parse::<i64>().ok())
                    .ok_or_else(|| bad("openGauss no tiene números de job libres.".into()))?;
                Some(free)
            } else {
                None
            };
            Ok(super::Plan {
                script: create(d, &j, id),
                empty: false,
                id: id.map(|n| format!("dbms_job:{n}")),
                new_id: if id.is_some() { None } else { new_id(c) },
            })
        }
        Some(id) => {
            let cur = read(s, c, id).await?;
            if !may(c, id, &cur) {
                return Err(bad(OTHERS.into()));
            }
            match alter(d, cur.number, &cur, &j) {
                None => Ok(super::Plan::nothing(
                    &format!("job {}", cur.number),
                    id.to_string(),
                )),
                Some(script) => Ok(super::Plan {
                    script,
                    empty: false,
                    id: Some(id.to_string()),
                    new_id: None,
                }),
            }
        }
    }
}

pub(crate) async fn delete_script(s: &PgSession, c: &Caps, id: &JobId) -> Result<String> {
    let cur = read(s, c, id).await?;
    if !may(c, id, &cur) {
        return Err(bad(OTHERS.into()));
    }
    Ok(delete(c.dialect(), cur.number))
}

/// An action on job `j`: "Ejecutar ahora" (`DBMS_JOB.RUN`, which runs it in
/// this session even while it's broken: EDB, KingbaseES) or, on openGauss,
/// "Programar para ahora": its next run moved to a moment already past in
/// every time zone, which the job scheduler takes within a second (as the
/// jobs list's "run now" does).
pub(crate) fn action_sql(d: D, j: &Job, action: &str) -> Result<String> {
    let n = j.number;
    match (d, action) {
        (D::Gauss, super::RUN_NOW) => {
            if !j.enabled {
                return Err(bad(
                    "El job está deshabilitado: openGauss no lo ejecuta hasta habilitarlo.".into(),
                ));
            }
            Ok(format!(
                "-- openGauss: programar el job {n} para ahora (DBMS_JOB): su próxima ejecución pasa a una hora ya pasada y el programador de jobs lo toma enseguida; DBine no espera a que termine.\n-- {GAUSS_PLAN}\nSELECT pkg_service.job_update({}, (now() AT TIME ZONE 'UTC') - interval '1 day', NULL, NULL);\n",
                gauss_id(Some(n))
            ))
        }
        (D::Edb, super::RUN) => Ok(format!(
            "-- EDB Postgres Advanced Server: ejecutar ahora el job {n} (DBMS_JOB.RUN), en esta sesión y aunque esté deshabilitado: DBine espera a que termine.\nBEGIN\n  DBMS_JOB.RUN({n});\nEND;\n"
        )),
        (D::Kdb, super::RUN) => Ok(format!(
            "-- KingbaseES: ejecutar ahora el job {n} (DBMS_JOB.RUN).\nCALL DBMS_JOB.RUN({n});\n"
        )),
        _ => Err(super::no_action(action)),
    }
}

pub(crate) async fn action_script(
    s: &PgSession,
    c: &Caps,
    id: &JobId,
    action: &str,
) -> Result<String> {
    let cur = read(s, c, id).await?;
    if !may(c, id, &cur) {
        return Err(bad(OTHERS.into()));
    }
    let d = c.dialect();
    if d == D::Gauss && action == super::RUN_NOW {
        if let Some(why) = &c.off {
            return Err(bad(why.clone()));
        }
    }
    action_sql(d, &cur, action)
}

/// Unsupported: DBMS_JOB has no steps.
pub(crate) fn no_steps() -> Error {
    Error::Unsupported("un job de DBMS_JOB es un solo comando: no tiene pasos".into())
}

// ---------------------------------------------------------------------------
// EDB's run history
// ---------------------------------------------------------------------------

/// How a job's rows in edb_job_scheduler's `sys.jobs` are told apart: by
/// the command it runs for it (as the jobs list reads its runs).
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Needle {
    /// `dbms_job.run(<n>, …)`.
    Job(i64),
    /// A DBMS_SCHEDULER call with the job's name as a literal.
    Scheduler(String),
}

/// `DELETE` of the job's finished runs in `sys.job_run_details`.
pub(crate) fn edb_clear_sql(
    jobs: &crate::jobs::scheduler::View,
    runs: &crate::jobs::scheduler::View,
    needle: &Needle,
) -> Option<String> {
    let d = Dialect::Edb;
    let cmd = jobs.col("j", "jobcommand")?;
    let matches = match needle {
        Needle::Job(n) => format!(
            "{cmd} ~* {}",
            d.lit(&format!(r"\mdbms_job\.run\s*\(\s*{n}\s*[,)]"))
        ),
        Needle::Scheduler(name) => format!(
            "{cmd} ILIKE '%dbms_scheduler%' AND strpos(lower({cmd}), lower({})) > 0",
            d.lit(&format!("'{}'", name.replace('\'', "''")))
        ),
    };
    // A run still going stays.
    let running = runs
        .col("d", "status")
        .map(|c| format!(" AND coalesce({c}::text, '') <> 'r'"))
        .unwrap_or_default();
    Some(format!(
        "-- EDB Postgres Advanced Server: borrar el historial del job (edb_job_scheduler)\nDELETE FROM {} d WHERE {} IN (SELECT {} FROM {} j WHERE {matches}){running};\n",
        runs.from(),
        runs.col("d", "jobid")?,
        jobs.col("j", "jobid")?,
        jobs.from(),
    ))
}

pub(crate) async fn edb_clear_script(s: &PgSession, f: &Found, needle: Needle) -> Result<String> {
    let e = Edb::read(s, f).await?;
    let no = || {
        Error::Unsupported(
            "no se puede borrar el historial de edb_job_scheduler (sys.job_run_details) con este usuario".into(),
        )
    };
    let (jobs, runs) = e.history().ok_or_else(no)?;
    if !edb_history_delete(s, &e).await? {
        return Err(bad(
            "Tu usuario no puede borrar el historial de edb_job_scheduler (DELETE en sys.job_run_details).".into(),
        ));
    }
    if let Needle::Job(n) = needle {
        let r = crate::jobs::edb::job_row(s, &e, n).await?;
        let owner = text(&r, "log_user");
        if !f.superuser && owner.as_deref() != Some(f.user.as_str()) {
            return Err(bad(OTHERS.into()));
        }
    }
    edb_clear_sql(jobs, runs, &needle).ok_or_else(no)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::jobs::fixture;
    use serde_json::Value;
    use std::collections::BTreeMap;

    fn fv(v: Value) -> FormValues {
        FormValues {
            values: serde_json::from_value(v).unwrap(),
            lists: BTreeMap::new(),
        }
    }

    fn spec_of(what: &str, next: Option<&str>, interval: Option<&str>, enabled: bool) -> Spec {
        Spec {
            what: what.into(),
            next: next.map(Into::into),
            interval: interval.map(Into::into),
            enabled,
        }
    }

    #[test]
    fn forms_are_checked() {
        let s = spec(&fv(
            json!({"what": "CALL p();", "next_date": "2031-01-02T03:04:05", "interval": " null "}),
        ))
        .unwrap();
        assert_eq!(
            s,
            spec_of("CALL p();", Some("2031-01-02T03:04:05"), None, true)
        );
        assert!(spec(&fv(json!({"what": "  "}))).is_err());
        assert!(spec(&fv(json!({"what": "x;", "next_date": "now()"}))).is_err());
        assert!(spec(&fv(json!({"what": "x;", "interval": "sysdate\n+ 1"}))).is_err());
        let off = spec(&fv(json!({"what": "x;", "enabled": false}))).unwrap();
        assert!(!off.enabled && off.next.is_none() && off.interval.is_none());
    }

    #[test]
    fn times_as_each_engine_takes_them() {
        assert_eq!(
            time_sql(D::Gauss, "2031-01-02T03:04:05"),
            "E'2031-01-02 03:04:05'::timestamp"
        );
        assert_eq!(
            time_sql(D::Gauss, "2031-01-02T03:04:05-03:00"),
            "(E'2031-01-02T03:04:05-03:00'::timestamptz)::timestamp"
        );
        assert_eq!(
            time_sql(D::Gauss, "2031-01-02T03:04:05Z"),
            "(E'2031-01-02T03:04:05Z'::timestamptz)::timestamp"
        );
        assert_eq!(
            time_sql(D::Edb, "2031-01-02T03:04:05"),
            "CAST(E'2031-01-02T03:04:05' AS TIMESTAMP WITH TIME ZONE)"
        );
        assert_eq!(gauss_id(Some(7)), "(SELECT 7::bigint)");
        assert_eq!(gauss_id(None), "(SELECT NULL::bigint)");
    }

    #[test]
    fn new_jobs() {
        let j = spec_of(
            "insert into t values ('a');",
            Some("2031-01-02T03:04:05"),
            Some("sysdate + 1"),
            true,
        );
        assert_eq!(
            create(D::Gauss, &j, None),
            "-- openGauss: nuevo job (DBMS_JOB)\n-- pkg_service es IMMUTABLE en openGauss: el id va en una subconsulta para que el servidor no ejecute la llamada al planificar la sentencia.\nSELECT pkg_service.job_submit((SELECT NULL::bigint), E'insert into t values (''a'');', E'2031-01-02 03:04:05'::timestamp, E'sysdate + 1') AS job;\n"
        );
        let off = spec_of("x;", None, None, false);
        let g = create(D::Gauss, &off, Some(12));
        assert!(g.contains("BEGIN;\nSELECT pkg_service.job_submit((SELECT 12::bigint), E'x;', sysdate, E'null') AS job;\nSELECT pkg_service.job_finish((SELECT 12::bigint), true);\nCOMMIT;"), "{g}");
        assert_eq!(
            create(D::Edb, &j, None),
            "-- EDB Postgres Advanced Server: nuevo job (DBMS_JOB)\nDECLARE\n  j INTEGER;\nBEGIN\n  DBMS_JOB.SUBMIT(j, E'insert into t values (''a'');', CAST(E'2031-01-02T03:04:05' AS TIMESTAMP WITH TIME ZONE), E'sysdate + 1');\nEND;\n"
        );
        let k = create(D::Kdb, &off, None);
        assert!(
            k.contains(
                "DBMS_JOB.SUBMIT(j, E'x;', now(), E'null');\n  DBMS_JOB.BROKEN(j, TRUE);\nEND;"
            ),
            "{k}"
        );
        assert!(create(D::Edb, &off, None).contains("SYSDATE, E'null'"));
    }

    fn job() -> Job {
        Job {
            number: 104,
            what: "x;".into(),
            next: Some("2031-01-02T03:04:05".into()),
            interval: Some("sysdate + 1".into()),
            enabled: true,
            owner: Some("app".into()),
            ..Default::default()
        }
    }

    #[test]
    fn jobs_change_in_place() {
        let cur = job();
        let same = spec_of("x;", cur.next.as_deref(), cur.interval.as_deref(), true);
        for d in [D::Gauss, D::Edb, D::Kdb] {
            assert_eq!(alter(d, 104, &cur, &same), None, "{d:?}");
        }
        // Only the interval: openGauss gets the id in a subquery.
        let iv = spec_of("x;", cur.next.as_deref(), Some("sysdate + 2"), true);
        let g = alter(D::Gauss, 104, &cur, &iv).unwrap();
        assert!(
            g.contains(
                "SELECT pkg_service.job_update((SELECT 104::bigint), NULL, E'sysdate + 2', NULL);"
            ),
            "{g}"
        );
        assert!(g.starts_with("-- openGauss: job 104 (DBMS_JOB)\n"), "{g}");
        assert_eq!(
            alter(D::Edb, 104, &cur, &iv).unwrap(),
            "-- EDB Postgres Advanced Server: job 104 (DBMS_JOB)\nBEGIN\n  DBMS_JOB.INTERVAL(104, E'sysdate + 2');\nEND;\n"
        );
        let k = alter(D::Kdb, 104, &cur, &iv).unwrap();
        assert!(
            k.contains("BEGIN;\nCALL DBMS_JOB.INTERVAL(104, E'sysdate + 2');\nCOMMIT;"),
            "{k}"
        );
        // Everything, then disabled.
        let all = spec_of("y;", Some("2031-05-06T07:08:09"), None, false);
        let e = alter(D::Edb, 104, &cur, &all).unwrap();
        assert!(e.contains("DBMS_JOB.WHAT(104, E'y;');\n  DBMS_JOB.INTERVAL(104, E'null');\n  DBMS_JOB.BROKEN(104, TRUE);"), "{e}");
        assert!(!e.contains("NEXT_DATE"), "{e}");
        let g = alter(D::Gauss, 104, &cur, &all).unwrap();
        assert!(
            g.contains("job_update((SELECT 104::bigint), NULL, E'null', E'y;')")
                && g.contains("job_finish((SELECT 104::bigint), true)"),
            "{g}"
        );
        // Enabled again at a date.
        let off = Job {
            enabled: false,
            next: None,
            ..cur.clone()
        };
        let on = spec_of("x;", Some("2031-05-06T07:08:09"), Some("sysdate + 1"), true);
        let e = alter(D::Edb, 104, &off, &on).unwrap();
        assert!(e.contains("DBMS_JOB.BROKEN(104, FALSE, CAST(E'2031-05-06T07:08:09' AS TIMESTAMP WITH TIME ZONE));"), "{e}");
        let g = alter(D::Gauss, 104, &off, &on).unwrap();
        assert!(
            g.contains(
                "job_finish((SELECT 104::bigint), false, E'2031-05-06 07:08:09'::timestamp)"
            ) && !g.contains("job_update"),
            "{g}"
        );
        // Only the next run.
        let nx = spec_of("x;", Some("2031-05-06T07:08:09"), Some("sysdate + 1"), true);
        assert!(alter(D::Kdb, 104, &cur, &nx).unwrap().contains("CALL DBMS_JOB.NEXT_DATE(104, CAST(E'2031-05-06T07:08:09' AS TIMESTAMP WITH TIME ZONE));"));
    }

    #[test]
    fn deleting() {
        assert!(delete(D::Gauss, 5).contains("SELECT pkg_service.job_cancel((SELECT 5::bigint));"));
        assert!(delete(D::Edb, 5).contains("BEGIN\n  DBMS_JOB.REMOVE(5);\nEND;"));
        assert!(delete(D::Kdb, 5).ends_with("CALL DBMS_JOB.REMOVE(5);\n"));
    }

    #[test]
    fn rows_read_as_jobs() {
        let g = gauss_job(&fixture(&[
            ("id", "14371"),
            ("log_user", "app"),
            ("dbname", "postgres"),
            ("nspname", "app"),
            ("job_status", "s"),
            ("next_local", "2031-01-02T03:04:05"),
            ("interval", "null"),
            ("what", "x;"),
            ("failures", "0"),
        ]))
        .unwrap();
        assert_eq!(g.number, 14371);
        assert_eq!(g.interval, None);
        assert_eq!(g.next.as_deref(), Some("2031-01-02T03:04:05"));
        assert!(g.enabled);
        let broken = gauss_job(&fixture(&[
            ("id", "1"),
            ("job_status", "d"),
            ("next_local", "4000-01-01T00:00:00"),
        ]))
        .unwrap();
        assert!(!broken.enabled && broken.next.is_none());
        let e = edb_job(&fixture(&[
            ("job", "104"),
            ("log_user", "app"),
            ("broken", "N"),
            ("next_date", "1794533465"),
            ("next_iso", "2026-11-13T01:31:05+00:00"),
            ("job_interval", "SYSDATE + 1"),
            ("what", "p;"),
        ]))
        .unwrap();
        assert_eq!(e.next.as_deref(), Some("2026-11-13T01:31:05+00:00"));
        assert_eq!(e.interval.as_deref(), Some("SYSDATE + 1"));
        let v = values(D::Gauss, Some(&g), "postgres");
        assert_eq!(v["what"], json!("x;"));
        assert_eq!(v["interval"], json!(""));
        assert_eq!(v["database"], json!("postgres"));
        // A form's values give the job back unchanged.
        let back = spec(&FormValues {
            values: v,
            lists: BTreeMap::new(),
        })
        .unwrap();
        assert_eq!(alter(D::Gauss, 14371, &g, &back), None);
        let new = values(D::Gauss, None, "app");
        assert_eq!(new["database"], json!("app"));
        let f = fields(D::Gauss, None);
        assert!(f
            .iter()
            .flat_map(|s| s.fields.iter())
            .any(|x| x.key == "database" && x.read_only));
        let e = fields(D::Edb, Some(&e));
        assert!(!e
            .iter()
            .flat_map(|s| s.fields.iter())
            .any(|x| x.key == "database"));
    }

    #[test]
    fn edb_history_is_cleared_by_command() {
        use crate::jobs::scheduler::{Kind, View};
        let jobs = View::fake(
            "sys",
            "jobs",
            &[("jobid", Kind::Other), ("jobcommand", Kind::Other)],
        );
        let runs = View::fake(
            "sys",
            "job_run_details",
            &[("jobid", Kind::Other), ("status", Kind::Other)],
        );
        let sql = edb_clear_sql(&jobs, &runs, &Needle::Job(104)).unwrap();
        assert!(sql.contains("DELETE FROM \"sys\".\"job_run_details\" d WHERE d.\"jobid\" IN (SELECT j.\"jobid\" FROM \"sys\".\"jobs\" j WHERE j.\"jobcommand\" ~* E'\\\\mdbms_job\\\\.run\\\\s*\\\\(\\\\s*104\\\\s*[,)]') AND coalesce(d.\"status\"::text, '') <> 'r';"), "{sql}");
        let sql = edb_clear_sql(&jobs, &runs, &Needle::Scheduler("it's".into())).unwrap();
        assert!(sql.contains("ILIKE '%dbms_scheduler%' AND strpos(lower(j.\"jobcommand\"), lower(E'''it''''s''')) > 0"), "{sql}");
        let bare = View::fake("sys", "jobs", &[("jobid", Kind::Other)]);
        assert!(edb_clear_sql(&bare, &runs, &Needle::Job(1)).is_none());
    }
}
