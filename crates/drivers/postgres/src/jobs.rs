//! Server jobs ([`dbine_driver::Session::jobs`] and the calls on one job)
//! per variant.
//!
//! - PostgreSQL and the distributions and managed services that keep its
//!   catalog (Aurora, RDS, Azure, Cloud SQL, AlloyDB, Supabase, Neon,
//!   TimescaleDB, YugabyteDB, EDB, Greenplum and its forks…): what the
//!   current database has installed, side by side. pg_cron (`cron.job`,
//!   `cron.job_run_details`), pgAgent (`pgagent.pga_*`) and TimescaleDB
//!   (`timescaledb_information.jobs`, its policies included). pg_cron and
//!   pgAgent keep their jobs in a single database of the server: from
//!   another one the list is empty, with a note naming it when the server
//!   says which (`cron.database_name`).
//! - Apache Cloudberry: its tasks (`CREATE TASK`, pg_cron built into the
//!   server: `pg_task`, `pg_task_run_history`), besides the above.
//! - openGauss: its own DBMS_JOB jobs (`pg_job`, `pkg_service`), besides
//!   the above.
//! - CockroachDB: its schedules (`SHOW SCHEDULES`: backups, changefeeds,
//!   row-level TTL and its own maintenance) and the changefeeds running or
//!   paused (`SHOW CHANGEFEED JOBS`), which are long-running work of their
//!   own that can be paused, resumed and cancelled. One-off jobs (a schema
//!   change, a backup or an import run by hand) are left out: they are a
//!   statement's work in progress, not something the server runs again.
//! - EDB Postgres Advanced Server and KingbaseES: the same detection, and a
//!   note that their own DBMS_JOB / DBMS_SCHEDULER jobs aren't read yet.
//! - Redshift, Materialize, RisingWave, CrateDB, Yellowbrick, Denodo and H2:
//!   no scheduler in the database (see [`unsupported_reason`]).
//!
//! When a scheduler is installed but starts nothing (pg_cron not loaded,
//! TimescaleDB without workers or restoring, openGauss's
//! `job_queue_processes` at 0), the list says so and offers no "run now".
//!
//! Ids are `<source>:<number>` ([`JobId`]); the number is the only part
//! that reaches a statement, and only after it parsed as one.

mod cockroach;
mod cron;
mod dbms_job;
mod pgagent;
mod timescale;

use crate::catalog::{cell, comment_text};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{Error, JobDetail, JobList, JobRun, PropertyInfo, Result};
use std::fmt;
use std::time::Duration;
use tokio_postgres::SimpleQueryRow;

/// Longest the run-history queries may take when the list adds the last
/// run of each job (pg_cron's history table isn't purged by itself).
const HISTORY_LIMIT: Duration = Duration::from_secs(10);
/// Characters kept of a run's message or a step's output.
const MAX_MESSAGE: usize = 8000;

/// Variants whose sessions answer `jobs` (what each job allows is per job).
pub(crate) fn supported(v: Variant) -> bool {
    unsupported_reason(v).is_none()
}

/// Why a variant has no server jobs, in Spanish (for the error and the
/// docs).
pub(crate) fn unsupported_reason(v: Variant) -> Option<&'static str> {
    match v {
        Variant::Redshift => Some(
            "Amazon Redshift no tiene jobs en la base: sus consultas programadas viven en Query Editor v2 y EventBridge",
        ),
        Variant::Materialize | Variant::RisingWave => Some(
            "este motor no tiene jobs programados: sus vistas materializadas se mantienen solas",
        ),
        Variant::CrateDb => Some("CrateDB no tiene un programador de jobs"),
        Variant::Yellowbrick => Some("Yellowbrick no tiene un programador de jobs"),
        Variant::Denodo => Some("Denodo programa sus tareas en Denodo Scheduler, fuera de la base"),
        Variant::H2 => Some("H2 no tiene un programador de jobs"),
        _ => None,
    }
}

fn check_supported(v: Variant) -> Result<()> {
    match unsupported_reason(v) {
        Some(why) => Err(Error::Unsupported(why.into())),
        None => Ok(()),
    }
}

/// A job's id as the other calls take it back: the source and the
/// engine's number for it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JobId {
    /// `cron.job.jobid`.
    Cron(i64),
    /// Apache Cloudberry's `pg_task.jobid`.
    Task(i64),
    /// `pgagent.pga_job.jobid`.
    Agent(i32),
    /// TimescaleDB's `job_id`.
    Timescale(i32),
    /// A CockroachDB schedule id.
    CrdbSchedule(i64),
    /// A CockroachDB job id (a changefeed).
    CrdbJob(i64),
    /// openGauss's `pg_job.job_id`.
    GaussJob(i64),
}

impl JobId {
    /// Only the exact text [`fmt::Display`] writes: a known source, a
    /// colon and a positive number without sign, spaces or leading zeros.
    pub(crate) fn parse(id: &str) -> Result<JobId> {
        let bad = || {
            Error::Query(format!(
                "«{}» no es un id de job de este servidor",
                shown(id)
            ))
        };
        let (source, num) = id.split_once(':').ok_or_else(bad)?;
        let n: i64 = num
            .parse()
            .ok()
            .filter(|n: &i64| *n > 0 && n.to_string() == num)
            .ok_or_else(bad)?;
        let small = || i32::try_from(n).map_err(|_| bad());
        Ok(match source {
            "pg_cron" => JobId::Cron(n),
            "task" => JobId::Task(n),
            "pgagent" => JobId::Agent(small()?),
            "timescaledb" => JobId::Timescale(small()?),
            "crdb_schedule" => JobId::CrdbSchedule(n),
            "crdb_job" => JobId::CrdbJob(n),
            "dbms_job" => JobId::GaussJob(n),
            _ => return Err(bad()),
        })
    }

    /// The id, refused when it belongs to the other family of engines.
    fn parse_for(v: Variant, id: &str) -> Result<JobId> {
        let job = JobId::parse(id)?;
        let crdb = matches!(job, JobId::CrdbSchedule(_) | JobId::CrdbJob(_));
        if crdb != (v == Variant::Cockroach) {
            return Err(Error::Query(format!(
                "«{}» no es un id de job de este servidor",
                shown(id)
            )));
        }
        Ok(job)
    }
}

impl fmt::Display for JobId {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            JobId::Cron(n) => write!(f, "pg_cron:{n}"),
            JobId::Task(n) => write!(f, "task:{n}"),
            JobId::Agent(n) => write!(f, "pgagent:{n}"),
            JobId::Timescale(n) => write!(f, "timescaledb:{n}"),
            JobId::CrdbSchedule(n) => write!(f, "crdb_schedule:{n}"),
            JobId::CrdbJob(n) => write!(f, "crdb_job:{n}"),
            JobId::GaussJob(n) => write!(f, "dbms_job:{n}"),
        }
    }
}

/// An id as an error message shows it: short, one line.
fn shown(id: &str) -> String {
    let mut s: String = comment_text(id).chars().take(60).collect();
    if id.chars().count() > 60 {
        s.push('…');
    }
    s
}

fn not_found(id: JobId) -> Error {
    Error::Query(format!("no existe el job {id} (o tu usuario no lo ve)"))
}

// ---------------------------------------------------------------------------
// What a PostgreSQL database has installed

/// The schedulers the current database has, and who's asking.
pub(super) struct Found {
    pub database: String,
    pub user: String,
    pub superuser: bool,
    /// Superuser or `BYPASSRLS`: pg_cron's row-level security shows them
    /// every user's jobs.
    pub sees_all: bool,
    /// Superuser or `pg_read_all_settings`: `pg_settings` shows them the
    /// settings only superusers may read (pg_cron's are).
    pub reads_settings: bool,
    pub cron: bool,
    /// Apache Cloudberry's tasks (`pg_catalog.pg_task`).
    pub task: bool,
    /// `cron.database_name`, when pg_cron is loaded and the user may read
    /// it.
    pub cron_database: Option<String>,
    pub pgagent: bool,
    /// The schema TimescaleDB's functions are in (`public` by default).
    pub timescale: Option<String>,
    /// What keeps a scheduler from starting jobs: openGauss's
    /// `job_queue_processes` and TimescaleDB's
    /// `timescaledb.max_background_workers` and `timescaledb.restoring`.
    pub job_queue_processes: Option<String>,
    pub timescale_workers: Option<String>,
    pub timescale_restoring: bool,
}

impl Found {
    pub(super) async fn detect(s: &PgSession) -> Result<Found> {
        Ok(Found::from_row(
            s.text(&detect_sql(s.version))
                .await?
                .first()
                .ok_or_else(|| Error::Query("el servidor no respondió".into()))?,
        ))
    }

    fn from_row(r: &dyn Row) -> Found {
        Found {
            database: text(r, "db").unwrap_or_default(),
            user: text(r, "usr").unwrap_or_default(),
            superuser: flag(r, "superuser"),
            sees_all: flag(r, "sees_all"),
            reads_settings: flag(r, "reads_settings"),
            cron: flag(r, "cron"),
            task: flag(r, "task"),
            cron_database: text(r, "cron_db"),
            pgagent: flag(r, "pgagent"),
            timescale: text(r, "timescale"),
            job_queue_processes: text(r, "job_queue_processes"),
            timescale_workers: text(r, "timescale_workers"),
            timescale_restoring: text(r, "timescale_restoring").as_deref() == Some("on"),
        }
    }

    /// Whether this database has any of them.
    fn any(&self) -> bool {
        self.cron || self.task || self.pgagent || self.timescale.is_some()
    }

    /// Why TimescaleDB's scheduler starts no job, if it doesn't.
    fn timescale_off(&self) -> Option<String> {
        if self.timescale_restoring {
            Some(
                "TimescaleDB está en modo restauración (timescaledb.restoring = on): sus jobs no se ejecutan."
                    .into(),
            )
        } else if self.timescale_workers.as_deref() == Some("0") {
            Some(
                "TimescaleDB no tiene procesos para sus jobs (timescaledb.max_background_workers = 0): no se ejecutan."
                    .into(),
            )
        } else {
            None
        }
    }

    /// Why openGauss's job scheduler starts no job, if it doesn't.
    fn gauss_off(&self) -> Option<String> {
        (self.job_queue_processes.as_deref() == Some("0")).then(|| {
            "El programador de jobs de openGauss está apagado (job_queue_processes = 0): sus jobs no se ejecutan."
                .into()
        })
    }
}

/// One row: the database, the user, which schedulers are installed and
/// the settings that keep them from running. Settings are read from
/// `pg_settings`, which leaves out what the user may not read instead of
/// failing as `current_setting` does (pg_cron's are superuser-only).
fn detect_sql(version: i32) -> String {
    // `rolbypassrls` is 9.5+.
    let bypass = if version >= 90500 {
        "rolsuper OR rolbypassrls"
    } else {
        "rolsuper"
    };
    let setting =
        |name: &str| format!("(SELECT setting FROM pg_catalog.pg_settings WHERE name = '{name}')");
    format!(
        "SELECT current_database() AS db, current_user AS usr,
                (SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user) AS superuser,
                (SELECT {bypass} FROM pg_catalog.pg_roles WHERE rolname = current_user) AS sees_all,
                coalesce((SELECT rolsuper FROM pg_catalog.pg_roles WHERE rolname = current_user), false)
                  OR EXISTS (SELECT 1 FROM pg_catalog.pg_roles r WHERE r.rolname = 'pg_read_all_settings'
                                AND pg_catalog.pg_has_role(current_user, r.oid, 'USAGE')) AS reads_settings,
                EXISTS (SELECT 1 FROM pg_catalog.pg_extension WHERE extname = 'pg_cron') AS cron,
                {cron_db} AS cron_db,
                EXISTS (SELECT 1 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                        WHERE n.nspname = 'pg_catalog' AND c.relname = 'pg_task') AS task,
                EXISTS (SELECT 1 FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                        WHERE n.nspname = 'pgagent' AND c.relname = 'pga_job') AS pgagent,
                (SELECT n.nspname FROM pg_catalog.pg_extension e
                   JOIN pg_catalog.pg_namespace n ON n.oid = e.extnamespace
                  WHERE e.extname = 'timescaledb') AS timescale,
                {queue} AS job_queue_processes,
                {workers} AS timescale_workers,
                {restoring} AS timescale_restoring",
        cron_db = setting("cron.database_name"),
        queue = setting("job_queue_processes"),
        workers = setting("timescaledb.max_background_workers"),
        restoring = setting("timescaledb.restoring"),
    )
}

/// What the list says about pg_cron when it isn't here or isn't running.
fn cron_notes(f: &Found) -> Vec<String> {
    let mut notes = Vec::new();
    match (&f.cron_database, f.cron) {
        (Some(db), false) if *db != f.database => notes.push(format!(
            "pg_cron guarda sus jobs en la base «{db}»: conectate a esa base para verlos."
        )),
        (Some(db), true) if *db != f.database => notes.push(format!(
            "pg_cron ejecuta solo los jobs de la base «{db}»: los de esta base no se ejecutan."
        )),
        // The extension is here but the server didn't load the library
        // (only a user who may read every setting can tell).
        (None, true) if f.reads_settings => notes.push(
            "pg_cron está instalado pero el servidor no lo carga (shared_preload_libraries): sus jobs no se ejecutan."
                .into(),
        ),
        _ => {}
    }
    notes
}

/// The note for the servers with a scheduler of their own that DBine
/// doesn't read yet.
fn unread_scheduler_note(v: Variant) -> Option<&'static str> {
    matches!(v, Variant::Edb | Variant::Kingbase).then_some(
        "DBine todavía no muestra los jobs de DBMS_JOB y DBMS_SCHEDULER de este servidor.",
    )
}

/// Marks a source's jobs as not runnable now and says why.
fn scheduler_off(part: &mut Part, why: Option<String>) {
    if let Some(why) = why {
        for j in &mut part.jobs {
            j.can_run = false;
        }
        part.notes.push(why);
    }
}

/// The note for a database with none of them.
fn nothing_note(f: &Found) -> String {
    format!(
        "La base «{}» no tiene pg_cron, pgAgent ni TimescaleDB, que es donde DBine busca los jobs del servidor. \
         pg_cron y pgAgent guardan sus jobs en una sola base (casi siempre postgres): si están instalados en otra, conectate a esa.",
        f.database
    )
}

// ---------------------------------------------------------------------------
// The calls

pub(crate) async fn list(s: &PgSession) -> Result<JobList> {
    check_supported(s.variant)?;
    if s.variant == Variant::Cockroach {
        return cockroach::list(s).await;
    }
    let found = Found::detect(s).await?;
    let mut out = JobList {
        notes: cron_notes(&found),
        ..Default::default()
    };
    out.notes
        .extend(unread_scheduler_note(s.variant).map(String::from));
    // openGauss's own scheduler, always there.
    let gauss = s.variant == Variant::OpenGauss;
    if !found.any() && !gauss {
        // The pg_cron note already says where to look.
        if cron_notes(&found).is_empty() {
            out.notes.push(nothing_note(&found));
        }
        return Ok(out);
    }
    let mut failed: Vec<(&str, Error)> = Vec::new();
    let mut sources = 0;
    if gauss {
        sources += 1;
        match dbms_job::list(s).await {
            Ok(mut part) => {
                scheduler_off(&mut part, found.gauss_off());
                part.add_to(&mut out)
            }
            Err(e) => failed.push(("DBMS_JOB", e)),
        }
    }
    if found.cron {
        sources += 1;
        match cron::list(s, &found, cron::Flavor::Cron).await {
            Ok(part) => part.add_to(&mut out),
            Err(e) => failed.push(("pg_cron", e)),
        }
    }
    if found.task {
        sources += 1;
        match cron::list(s, &found, cron::Flavor::Task).await {
            Ok(part) => part.add_to(&mut out),
            Err(e) => failed.push(("las tareas (pg_task)", e)),
        }
    }
    if found.pgagent {
        sources += 1;
        match pgagent::list(s, &found).await {
            Ok(part) => part.add_to(&mut out),
            Err(e) => failed.push(("pgAgent", e)),
        }
    }
    if let Some(schema) = &found.timescale {
        sources += 1;
        match timescale::list(s, schema).await {
            Ok(mut part) => {
                scheduler_off(&mut part, found.timescale_off());
                part.add_to(&mut out)
            }
            Err(e) => failed.push(("TimescaleDB", e)),
        }
    }
    if failed.len() == sources {
        // Nothing could be read: that's an error, not a note.
        return Err(failed.remove(0).1);
    }
    for (source, e) in failed {
        out.notes
            .push(format!("No se pudieron leer los jobs de {source}: {e}"));
    }
    Ok(out)
}

/// One scheduler's share of the list.
#[derive(Default)]
pub(super) struct Part {
    pub jobs: Vec<dbine_driver::ServerJob>,
    pub notes: Vec<String>,
}

impl Part {
    fn add_to(self, out: &mut JobList) {
        out.jobs.extend(self.jobs);
        out.notes.extend(self.notes);
    }
}

pub(crate) async fn detail(s: &PgSession, id: &str) -> Result<JobDetail> {
    check_supported(s.variant)?;
    match JobId::parse_for(s.variant, id)? {
        JobId::Cron(n) => cron::detail(s, cron::Flavor::Cron, n).await,
        JobId::Task(n) => cron::detail(s, cron::Flavor::Task, n).await,
        JobId::Agent(n) => pgagent::detail(s, n).await,
        JobId::Timescale(n) => timescale::detail(s, n).await,
        JobId::CrdbSchedule(n) => cockroach::schedule_detail(s, n).await,
        JobId::CrdbJob(n) => cockroach::job_detail(s, n).await,
        JobId::GaussJob(n) => dbms_job::detail(s, n).await,
    }
}

pub(crate) async fn runs(s: &PgSession, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    check_supported(s.variant)?;
    let job = JobId::parse_for(s.variant, id)?;
    let limit = limit.min(MAX_JOB_RUNS);
    if limit == 0 {
        return Ok(Vec::new());
    }
    match job {
        JobId::Cron(n) => cron::runs(s, cron::Flavor::Cron, n, limit).await,
        JobId::Task(n) => cron::runs(s, cron::Flavor::Task, n, limit).await,
        JobId::Agent(n) => pgagent::runs(s, n, limit).await,
        JobId::Timescale(n) => timescale::runs(s, n, limit).await,
        JobId::CrdbSchedule(n) => cockroach::runs(s, n, limit).await,
        JobId::CrdbJob(_) => Err(Error::Unsupported(
            "un changefeed es un solo job que sigue corriendo: no tiene ejecuciones anteriores"
                .into(),
        )),
        JobId::GaussJob(_) => Err(Error::Unsupported(
            "openGauss no guarda el historial de sus jobs: solo la última ejecución, que se ve en la lista".into(),
        )),
    }
}

pub(crate) async fn run(s: &PgSession, id: &str) -> Result<()> {
    check_supported(s.variant)?;
    match JobId::parse_for(s.variant, id)? {
        JobId::Cron(_) => Err(Error::Unsupported(
            "pg_cron no ejecuta un job fuera de su programación".into(),
        )),
        JobId::Task(_) => Err(Error::Unsupported(
            "el servidor no ejecuta una tarea fuera de su programación".into(),
        )),
        JobId::Agent(n) => pgagent::run(s, n).await,
        JobId::Timescale(n) => timescale::run(s, n).await,
        JobId::CrdbSchedule(n) => cockroach::run(s, n).await,
        JobId::CrdbJob(_) => Err(Error::Unsupported(
            "un changefeed ya está corriendo: se reanuda con «Habilitar»".into(),
        )),
        JobId::GaussJob(n) => dbms_job::run(s, n).await,
    }
}

pub(crate) async fn stop(s: &PgSession, id: &str) -> Result<()> {
    check_supported(s.variant)?;
    match JobId::parse_for(s.variant, id)? {
        JobId::Cron(_) => Err(Error::Unsupported(
            "pg_cron no permite detener un job en ejecución".into(),
        )),
        JobId::Task(_) => Err(Error::Unsupported(
            "el servidor no permite detener una tarea en ejecución".into(),
        )),
        JobId::Agent(_) => Err(Error::Unsupported(
            "pgAgent no permite detener un job en ejecución".into(),
        )),
        JobId::Timescale(_) => Err(Error::Unsupported(
            "TimescaleDB no permite detener un job en ejecución".into(),
        )),
        JobId::CrdbSchedule(n) => cockroach::stop_schedule(s, n).await,
        JobId::CrdbJob(n) => cockroach::cancel_job(s, n).await,
        JobId::GaussJob(_) => Err(Error::Unsupported(
            "openGauss no permite detener un job en ejecución".into(),
        )),
    }
}

pub(crate) async fn set_enabled(s: &PgSession, id: &str, enabled: bool) -> Result<()> {
    check_supported(s.variant)?;
    match JobId::parse_for(s.variant, id)? {
        JobId::Cron(n) => cron::set_enabled(s, cron::Flavor::Cron, n, enabled).await,
        JobId::Task(n) => cron::set_enabled(s, cron::Flavor::Task, n, enabled).await,
        JobId::Agent(n) => pgagent::set_enabled(s, n, enabled).await,
        JobId::Timescale(n) => timescale::set_enabled(s, n, enabled).await,
        JobId::CrdbSchedule(n) => cockroach::set_schedule_enabled(s, n, enabled).await,
        JobId::CrdbJob(n) => cockroach::set_job_enabled(s, n, enabled).await,
        JobId::GaussJob(n) => dbms_job::set_enabled(s, n, enabled).await,
    }
}

pub(crate) async fn script(s: &PgSession, id: &str) -> Result<String> {
    check_supported(s.variant)?;
    match JobId::parse_for(s.variant, id)? {
        JobId::Cron(n) => cron::script(s, cron::Flavor::Cron, n).await,
        JobId::Task(n) => cron::script(s, cron::Flavor::Task, n).await,
        JobId::Agent(n) => pgagent::script(s, n).await,
        JobId::Timescale(n) => timescale::script(s, n).await,
        JobId::CrdbSchedule(n) => cockroach::schedule_script(s, n).await,
        JobId::CrdbJob(n) => cockroach::job_script(s, n).await,
        JobId::GaussJob(n) => dbms_job::script(s, n).await,
    }
}

// ---------------------------------------------------------------------------
// Helpers for the sources

/// A row's cells by column name: a query's row, or a fixture in the tests.
pub(super) trait Row {
    /// The cell as the server sent it, `None` when null or missing.
    fn get(&self, name: &str) -> Option<String>;
}

impl Row for SimpleQueryRow {
    fn get(&self, name: &str) -> Option<String> {
        cell(self, name)
    }
}

/// Column name → text, standing in for a query's row.
#[cfg(test)]
pub(super) struct Fixture(pub std::collections::HashMap<String, String>);

#[cfg(test)]
impl Row for Fixture {
    fn get(&self, name: &str) -> Option<String> {
        self.0.get(name).cloned()
    }
}

#[cfg(test)]
pub(super) fn fixture(cells: &[(&str, &str)]) -> Fixture {
    Fixture(
        cells
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
    )
}

/// A query's rows as [`Row`]s.
pub(super) fn refs(rows: &[SimpleQueryRow]) -> Vec<&dyn Row> {
    rows.iter().map(|r| r as &dyn Row).collect()
}

/// A trimmed, non-empty cell.
pub(super) fn text(r: &dyn Row, name: &str) -> Option<String> {
    r.get(name)
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// A cell that may be long, kept to [`MAX_MESSAGE`] characters.
pub(super) fn message(r: &dyn Row, name: &str) -> Option<String> {
    text(r, name).map(|m| {
        if m.chars().count() > MAX_MESSAGE {
            let mut cut: String = m.chars().take(MAX_MESSAGE).collect();
            cut.push('…');
            cut
        } else {
            m
        }
    })
}

/// A boolean cell (`t`, `true`, `1`).
pub(super) fn flag(r: &dyn Row, name: &str) -> bool {
    matches!(
        r.get(name)
            .map(|v| v.trim().to_ascii_lowercase())
            .as_deref(),
        Some("t" | "true" | "1")
    )
}

/// A time the query gave as `extract(epoch FROM …)`, as RFC 3339 in UTC.
pub(super) fn time(r: &dyn Row, name: &str) -> Option<String> {
    r.get(name)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .and_then(rfc3339)
}

/// Seconds since 1970 (UTC) as `2026-11-12T03:00:00Z`; `None` for
/// `±infinity` and years out of RFC 3339's range.
pub(super) fn rfc3339(secs: f64) -> Option<String> {
    if !secs.is_finite() {
        return None;
    }
    let secs = secs.floor() as i64;
    let (y, m, d) = crate::backup::civil(secs.div_euclid(86_400));
    if !(1..=9999).contains(&y) {
        return None;
    }
    let t = secs.rem_euclid(86_400);
    Some(format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    ))
}

/// Milliseconds the query gave as a number.
pub(super) fn ms(r: &dyn Row, name: &str) -> Option<u64> {
    r.get(name)
        .and_then(|v| v.trim().parse::<f64>().ok())
        .filter(|v| v.is_finite())
        .map(|v| v.max(0.0).round() as u64)
}

pub(super) fn prop(group: &str, label: &str, value: impl Into<String>) -> PropertyInfo {
    PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: value.into(),
    }
}

/// An identifier, always double-quoted.
pub(super) fn ident(name: &str) -> String {
    format!("\"{}\"", name.replace('"', "\"\""))
}

/// "sí" / "no".
pub(super) fn yes_no(b: bool) -> &'static str {
    if b {
        "sí"
    } else {
        "no"
    }
}

/// `s` as a dollar-quoted literal (the command of a job reads better than
/// with every quote doubled), with a tag that can't end it early.
pub(super) fn dollar(s: &str) -> String {
    dollar_tagged(s, "cmd")
}

/// [`dollar`] with tags `$<base>$`, `$<base>1$`…: an outer block (a `DO`)
/// takes another base than the literals inside it.
pub(super) fn dollar_tagged(s: &str, base: &str) -> String {
    let mut n = 0;
    loop {
        let tag = if n == 0 {
            format!("${base}$")
        } else {
            format!("${base}{n}$")
        };
        // The literal ends at the first tag after the opening one: it must
        // be the closing one.
        if format!("{s}{tag}").find(&tag) == Some(s.len()) {
            return format!("{tag}{s}{tag}");
        }
        n += 1;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ids_round_trip() {
        for id in [
            JobId::Cron(12),
            JobId::Task(16384),
            JobId::Agent(3),
            JobId::Timescale(1000),
            JobId::CrdbSchedule(1226891610358054913),
            JobId::CrdbJob(i64::MAX),
            JobId::GaussJob(14371),
        ] {
            assert_eq!(JobId::parse(&id.to_string()).unwrap(), id);
        }
    }

    #[test]
    fn ids_that_were_not_written_here_are_refused() {
        for bad in [
            "",
            "12",
            "pg_cron:",
            "pg_cron:0",
            "pg_cron:-1",
            "pg_cron:+1",
            "pg_cron:012",
            "pg_cron: 12",
            "pg_cron:12 ",
            "pg_cron:12; DROP TABLE x",
            "pg_cron:1e3",
            "pg_cron:99999999999999999999",
            "PG_CRON:12",
            "pgagent:2147483648",
            "timescaledb:4294967296",
            "other:1",
            "pg_cron:12:1",
            "pg_cron:１２",
            "1; DROP TABLE x",
        ] {
            assert!(JobId::parse(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn ids_of_the_other_family_are_refused() {
        assert!(JobId::parse_for(Variant::Postgres, "crdb_schedule:1").is_err());
        assert!(JobId::parse_for(Variant::Cockroach, "pg_cron:1").is_err());
        assert!(JobId::parse_for(Variant::Cockroach, "crdb_job:1").is_ok());
        assert!(JobId::parse_for(Variant::Timescale, "timescaledb:1000").is_ok());
    }

    #[test]
    fn a_long_or_multiline_id_is_cut_in_the_error() {
        let e = JobId::parse(&format!("x\n{}", "a".repeat(500))).unwrap_err();
        let m = e.to_string();
        assert!(!m.contains('\n') && m.contains('…') && m.len() < 200, "{m}");
    }

    #[test]
    fn times_are_utc_rfc3339() {
        assert_eq!(rfc3339(0.0).as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(
            rfc3339(1_794_488_199.9).as_deref(),
            Some("2026-11-12T12:56:39Z")
        );
        assert_eq!(
            rfc3339(951_782_400.0).as_deref(),
            Some("2000-02-29T00:00:00Z")
        );
        assert_eq!(rfc3339(f64::NEG_INFINITY), None);
        assert_eq!(rfc3339(f64::NAN), None);
        assert_eq!(rfc3339(1e15), None);
    }

    #[test]
    fn dollar_quotes_never_end_early() {
        assert_eq!(dollar("SELECT 1"), "$cmd$SELECT 1$cmd$");
        assert_eq!(dollar("a $cmd$ b"), "$cmd1$a $cmd$ b$cmd1$");
        // Ending in part of the tag would close the literal one char early.
        assert_eq!(dollar("x$cmd"), "$cmd1$x$cmd$cmd1$");
        assert_eq!(dollar("x$"), "$cmd$x$$cmd$");
        for s in ["x$cmd", "$cmd$", "$cmd1$$cmd$", "a$cmd$cmd1"] {
            let q = dollar(s);
            let tag_end = q[1..].find('$').unwrap() + 2;
            let tag = &q[..tag_end];
            let body = &q[tag_end..];
            assert_eq!(body.find(tag), Some(s.len()), "{s:?} → {q}");
        }
    }

    #[test]
    fn detection_reads_settings_without_failing() {
        let new = detect_sql(160000);
        // `current_setting` fails on superuser-only settings (pg_cron's).
        assert!(!new.contains("current_setting"));
        for name in [
            "cron.database_name",
            "job_queue_processes",
            "timescaledb.max_background_workers",
            "timescaledb.restoring",
        ] {
            assert!(
                new.contains(&format!("pg_catalog.pg_settings WHERE name = '{name}'")),
                "{name}"
            );
        }
        assert!(new.contains("rolbypassrls") && new.contains("pg_read_all_settings"));
        assert!(!detect_sql(90400).contains("rolbypassrls"));
    }

    pub(super) fn found_for(user: &str, superuser: bool) -> Found {
        Found {
            database: "postgres".into(),
            user: user.into(),
            superuser,
            sees_all: superuser,
            reads_settings: superuser,
            cron: true,
            task: false,
            cron_database: Some("postgres".into()),
            pgagent: false,
            timescale: None,
            job_queue_processes: None,
            timescale_workers: None,
            timescale_restoring: false,
        }
    }

    fn found(db: &str, cron: bool, cron_db: Option<&str>) -> Found {
        Found {
            database: db.into(),
            cron,
            cron_database: cron_db.map(Into::into),
            ..found_for("u", true)
        }
    }

    #[test]
    fn the_notes_say_where_pg_cron_is() {
        let f = found("app", false, Some("postgres"));
        assert!(!f.any());
        let notes = cron_notes(&f);
        assert_eq!(notes.len(), 1);
        assert!(notes[0].contains("«postgres»"), "{notes:?}");
        assert!(cron_notes(&found("postgres", true, Some("postgres"))).is_empty());
        assert!(cron_notes(&found("app", true, Some("postgres")))[0].contains("no se ejecutan"));
        let none = found("app", false, None);
        assert!(cron_notes(&none).is_empty());
        let n = nothing_note(&none);
        assert!(n.contains("pg_cron") && n.contains("pgAgent") && n.contains("TimescaleDB"));
        // Installed but not loaded: only who reads every setting can tell.
        let unloaded = found("postgres", true, None);
        assert!(cron_notes(&unloaded)[0].contains("shared_preload_libraries"));
        let plain = Found {
            reads_settings: false,
            ..unloaded
        };
        assert!(cron_notes(&plain).is_empty());
    }

    #[test]
    fn a_stopped_scheduler_says_so_and_runs_nothing() {
        let f = found_for("u", true);
        assert!(f.timescale_off().is_none() && f.gauss_off().is_none());
        let none = Found {
            timescale_workers: Some("0".into()),
            job_queue_processes: Some("0".into()),
            ..found_for("u", true)
        };
        assert!(none
            .timescale_off()
            .unwrap()
            .contains("max_background_workers = 0"));
        assert!(none
            .gauss_off()
            .unwrap()
            .contains("job_queue_processes = 0"));
        let restoring = Found {
            timescale_restoring: true,
            timescale_workers: Some("16".into()),
            ..found_for("u", true)
        };
        assert!(restoring.timescale_off().unwrap().contains("restoring"));
        let mut part = Part {
            jobs: vec![dbine_driver::ServerJob {
                can_run: true,
                ..Default::default()
            }],
            notes: Vec::new(),
        };
        scheduler_off(&mut part, None);
        assert!(part.jobs[0].can_run && part.notes.is_empty());
        scheduler_off(&mut part, none.gauss_off());
        assert!(!part.jobs[0].can_run && part.notes.len() == 1);
    }

    #[test]
    fn edb_and_kingbase_say_what_isnt_read_yet() {
        for v in Variant::ALL {
            assert_eq!(
                unread_scheduler_note(v).is_some(),
                matches!(v, Variant::Edb | Variant::Kingbase),
                "{v:?}"
            );
        }
    }

    #[test]
    fn variants_without_a_scheduler_say_why() {
        for v in Variant::ALL {
            let expected = !matches!(
                v,
                Variant::Redshift
                    | Variant::Materialize
                    | Variant::RisingWave
                    | Variant::CrateDb
                    | Variant::Yellowbrick
                    | Variant::Denodo
                    | Variant::H2
            );
            assert_eq!(supported(v), expected, "{v:?}");
            assert_eq!(crate::design::capabilities(v).jobs, expected, "{v:?}");
        }
    }
}
