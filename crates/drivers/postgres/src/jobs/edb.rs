//! EDB Postgres Advanced Server's own jobs: `DBMS_JOB` (`sys.dba_jobs`, or
//! `all_jobs`) and `DBMS_SCHEDULER` (`dba_scheduler_jobs` and the rest of
//! its views, read as [`super::scheduler`] does).
//!
//! From EPAS 16 the edb_job_scheduler extension runs them, for the
//! databases in `edb_job_scheduler.database_list`: `sys.jobs` holds the
//! next run of each and `sys.job_run_details` its runs. Before, pgAgent ran
//! them: they are pgAgent jobs then, listed as such, and the list says so.
//!
//! "Run now" of a DBMS_JOB job moves its next date to now
//! (`DBMS_JOB.NEXT_DATE`), so the scheduler takes it and the call doesn't
//! wait (`DBMS_JOB.RUN` would run it here, to the end). DBMS_SCHEDULER only
//! has `RUN_JOB` in the current session (EDB refuses
//! `use_current_session => FALSE`): it isn't offered
//! ([`scheduler::RUN_WAITS`]). Neither package can stop a run. Enabling is
//! `DBMS_JOB.BROKEN` (a next date still ahead is kept) and
//! `DBMS_SCHEDULER.ENABLE` / `DISABLE`. A user changes their own DBMS_JOB
//! jobs (by number), a superuser anyone's. DBMS_SCHEDULER's calls take the
//! bare name, which Oracle, whose packages EDB copies, resolves in the
//! caller's schema: only the user's own jobs, with a name no other owner's
//! job has, are enabled or disabled.
//!
//! A job's runs are the rows of `sys.job_run_details` whose job runs its
//! command (`dbms_job.run(<n>, …)`, or the DBMS_SCHEDULER call with its
//! name, for a name only one owner has), or a `*_SCHEDULER_JOB_RUN_DETAILS`
//! view where the server has one.
//!
//! Unverified: no EPAS server was at hand. This follows EDB's documentation
//! (`sys.jobs`, `sys.job_run_details`, `DBA_JOBS`, the packages'
//! procedures); the command each job runs in `sys.jobs` and the columns of
//! the DBMS_SCHEDULER views are not documented.

use super::scheduler::{self, Catalog, Dialect, Program, SchedJob, Scope, View};
use super::{flag, message, not_found, prop, text, time, yes_no, Found, JobId, Named, Part, Row};
use crate::catalog::comment_text;
use crate::session::PgSession;
use dbine_driver::{Error, JobDetail, JobOutcome, JobRun, JobStep, Result, ServerJob};
use std::collections::HashMap;
use tokio_postgres::SimpleQueryRow;

const KIND: &str = "DBMS_JOB";
const D: Dialect = Dialect::Edb;

/// The views and tables read, by lower-case name.
const NAMES: &[&str] = &[
    "dba_jobs",
    "all_jobs",
    "dba_scheduler_jobs",
    "all_scheduler_jobs",
    "user_scheduler_jobs",
    "dba_scheduler_programs",
    "all_scheduler_programs",
    "user_scheduler_programs",
    "dba_scheduler_schedules",
    "all_scheduler_schedules",
    "user_scheduler_schedules",
    "dba_scheduler_job_run_details",
    "all_scheduler_job_run_details",
    "user_scheduler_job_run_details",
    "jobs",
    "job_run_details",
];

/// 4000-01-01 in epoch seconds: the next date of a broken DBMS_JOB job.
const BROKEN_DATE: f64 = 64_060_588_800.0;

/// The extension, and its setting: `loaded` when the server loaded the
/// library (the setting exists), which `pg_settings` shows only to who may
/// read it.
const STATE_SQL: &str =
    "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_extension WHERE extname = 'edb_job_scheduler') AS scheduler,
            EXISTS (SELECT 1 FROM pg_catalog.pg_settings WHERE name = 'edb_job_scheduler.database_list') AS loaded,
            (SELECT setting FROM pg_catalog.pg_settings WHERE name = 'edb_job_scheduler.database_list') AS database_list";

const ON_PGAGENT: &str =
    "En este servidor DBMS_JOB y DBMS_SCHEDULER guardan sus jobs en pgAgent: se ven entre los jobs de pgAgent.";
const PGAGENT_ELSEWHERE: &str = "En este servidor DBMS_JOB y DBMS_SCHEDULER funcionan con pgAgent, que no está en esta base: conectate a la base que lo tiene.";
const UNREADABLE: &str = "Tu usuario no puede leer los jobs de DBMS_JOB ni de DBMS_SCHEDULER: necesita SELECT en sys.all_jobs o en sys.all_scheduler_jobs.";
const DBMS_JOB_UNREADABLE: &str =
    "Tu usuario no puede leer los jobs de DBMS_JOB: necesita SELECT en sys.all_jobs.";
const SCHEDULER_UNREADABLE: &str = "Tu usuario no puede leer los jobs de DBMS_SCHEDULER: necesita SELECT en sys.all_scheduler_jobs.";
const NOT_OWN: &str = "EDB busca los jobs de DBMS_SCHEDULER por su nombre, sin su dueño: DBine solo habilita o deshabilita los de tu usuario";
const SAME_NAME: &str = "hay más de un job de DBMS_SCHEDULER con este nombre: EDB los busca por su nombre, sin su dueño, y podría cambiar otro";
const NO_PACKAGES: &str = "edb_job_scheduler está instalado, pero esta base no tiene DBMS_JOB ni DBMS_SCHEDULER (sus extensiones dbms_job y dbms_scheduler).";
const DBMS_JOB_PARTIAL: &str = "Se ven solo los jobs de DBMS_JOB a los que tiene acceso este usuario: para verlos todos hace falta poder leer sys.dba_jobs.";
const SCHEDULER_PARTIAL: &str = "Se ven solo los jobs de DBMS_SCHEDULER a los que tiene acceso este usuario: para verlos todos hace falta poder leer sys.dba_scheduler_jobs.";

/// What this database has of EDB's schedulers, and who's asking.
struct Edb {
    cat: Catalog,
    user: String,
    superuser: bool,
    database: String,
    reads_settings: bool,
    pgagent: bool,
    version: i32,
    /// The edb_job_scheduler extension is in this database.
    scheduler: bool,
    /// The server loaded edb_job_scheduler's library, as far as the user
    /// can see (`pg_settings` has its setting).
    loaded: bool,
    /// `edb_job_scheduler.database_list` (empty when it lists none), when
    /// the server has it and the user may read it.
    database_list: Option<String>,
}

impl Edb {
    async fn read(s: &PgSession, f: &Found) -> Result<Edb> {
        let cat = Catalog::read(s, D, NAMES).await?;
        let rows = s.text(STATE_SQL).await?;
        let r = rows.first();
        Ok(Edb {
            cat,
            user: f.user.clone(),
            superuser: f.superuser,
            database: f.database.clone(),
            reads_settings: f.reads_settings,
            pgagent: f.pgagent,
            version: s.version,
            scheduler: r.is_some_and(|r| flag(r, "scheduler")),
            loaded: r.is_some_and(|r| flag(r, "loaded")),
            // An empty list is kept: it serves no database.
            database_list: r
                .and_then(|r| r.get("database_list"))
                .map(|l| l.trim().to_string()),
        })
    }

    fn dbms_jobs(&self) -> Option<(&View, Scope)> {
        self.cat
            .readable("dba_jobs")
            .map(|v| (v, Scope::All))
            .or_else(|| self.cat.readable("all_jobs").map(|v| (v, Scope::Visible)))
            .filter(|(v, _)| v.has("job"))
    }

    fn sched_jobs(&self) -> Option<(&View, Scope)> {
        self.cat
            .widest("scheduler_jobs")
            .filter(|(v, _)| v.has("job_name"))
    }

    fn schedules(&self) -> Option<&View> {
        self.cat.widest("scheduler_schedules").map(|(v, _)| v)
    }

    /// DBMS_JOB or DBMS_SCHEDULER is in this database.
    fn present(&self) -> bool {
        self.cat.get("dba_jobs").is_some()
            || self.cat.get("all_jobs").is_some()
            || self.cat.any("scheduler_jobs")
    }

    /// The packages keep their jobs in pgAgent: before EPAS 16, without
    /// edb_job_scheduler.
    fn on_pgagent(&self) -> bool {
        !self.scheduler && self.pgagent && self.version > 0 && self.version < 160000
    }

    fn off(&self) -> Option<String> {
        off_note(
            self.version,
            self.scheduler,
            self.loaded,
            self.database_list.as_deref(),
            self.reads_settings,
            &self.database,
        )
    }

    /// May change a DBMS_JOB job of `owner` (DBMS_JOB takes its number).
    fn may(&self, owner: Option<&str>) -> bool {
        self.superuser || owner == Some(self.user.as_str())
    }

    /// The owner of a DBMS_SCHEDULER job listed from a view of `scope`:
    /// a `USER_*` view has no owner column, and every job in it is the
    /// user's.
    fn owner_of(&self, j: &SchedJob, scope: Scope) -> Option<String> {
        j.owner
            .clone()
            .or_else(|| (scope == Scope::Own).then(|| self.user.clone()))
    }

    /// Whether the user's own: DBMS_SCHEDULER's calls take the bare name,
    /// which reaches the caller's job, a superuser's included.
    fn owns(&self, owner: Option<&str>) -> bool {
        owner == Some(self.user.as_str())
    }

    /// Where the runs of DBMS_SCHEDULER job `name` of `owner` are read:
    /// a run-details view that tells them apart (an `ALL_`/`DBA_` one with
    /// owners, or for a name only one owner has; a `USER_` one for the
    /// user's own), else `sys.jobs` for a name only one owner has.
    /// `unique`: no other owner's job of that name is in sight.
    fn sched_history(&self, owner: Option<&str>, unique: bool) -> Option<SchedHistory<'_>> {
        if let Some((v, scope)) = self.run_details() {
            let fits = match scope {
                Scope::Own => self.owns(owner),
                Scope::All | Scope::Visible => (owner.is_some() && v.has("owner")) || unique,
            };
            if fits {
                return Some(SchedHistory::Details(v));
            }
        }
        if !unique {
            return None;
        }
        self.history()
            .map(|(jobs, runs)| SchedHistory::Jobs(jobs, runs))
    }

    /// The list's notes on a package whose jobs the user can't read while
    /// the other's can (both unreadable is [`UNREADABLE`]).
    fn unreadable_notes(&self) -> Vec<&'static str> {
        let mut notes = Vec::new();
        let dbms_job = self.cat.get("dba_jobs").is_some() || self.cat.get("all_jobs").is_some();
        let unread_job = dbms_job
            && self.cat.readable("dba_jobs").is_none()
            && self.cat.readable("all_jobs").is_none();
        let unread_sched =
            self.cat.any("scheduler_jobs") && self.cat.widest("scheduler_jobs").is_none();
        if unread_job && self.sched_jobs().is_some() {
            notes.push(DBMS_JOB_UNREADABLE);
        }
        if unread_sched && self.dbms_jobs().is_some() {
            notes.push(SCHEDULER_UNREADABLE);
        }
        notes
    }

    /// `sys.jobs` and `sys.job_run_details`, when the user may read them.
    fn history(&self) -> Option<(&View, &View)> {
        let jobs = self
            .cat
            .readable("jobs")
            .filter(|v| v.has("jobid") && v.has("jobcommand"))?;
        let runs = self
            .cat
            .readable("job_run_details")
            .filter(|v| v.has("jobid"))?;
        Some((jobs, runs))
    }

    /// A `*_SCHEDULER_JOB_RUN_DETAILS` view, where there's one, and how
    /// much it shows.
    fn run_details(&self) -> Option<(&View, Scope)> {
        self.cat
            .widest("scheduler_job_run_details")
            .filter(|(v, _)| v.has("job_name"))
    }
}

/// Where a DBMS_SCHEDULER job's runs are read.
#[derive(Debug, Clone, Copy)]
enum SchedHistory<'a> {
    /// A `*_SCHEDULER_JOB_RUN_DETAILS` view.
    Details(&'a View),
    /// `sys.jobs` and `sys.job_run_details`.
    Jobs(&'a View, &'a View),
}

/// Why nothing starts the jobs of database `db`, if nothing does: before
/// EPAS 16 pgAgent runs them (in this database it's [`Edb::on_pgagent`]);
/// from 16, edb_job_scheduler, when it's installed, the server loads it
/// and it serves `db`.
fn off_note(
    version: i32,
    scheduler: bool,
    loaded: bool,
    database_list: Option<&str>,
    reads_settings: bool,
    db: &str,
) -> Option<String> {
    if version > 0 && version < 160000 {
        return Some(PGAGENT_ELSEWHERE.into());
    }
    if !scheduler {
        return Some(
            "edb_job_scheduler no está instalado en esta base: los jobs de DBMS_JOB y DBMS_SCHEDULER no se ejecutan solos."
                .into(),
        );
    }
    if !loaded {
        // Only who may read every setting can tell it isn't loaded.
        return reads_settings.then(|| {
            "edb_job_scheduler está instalado pero el servidor no lo carga (shared_preload_libraries): sus jobs no se ejecutan."
                .into()
        });
    }
    // Loaded: an empty or missing list serves no database.
    (!listed(database_list.unwrap_or_default(), db)).then(|| {
        format!(
            "edb_job_scheduler no atiende la base «{db}» (edb_job_scheduler.database_list): sus jobs no se ejecutan."
        )
    })
}

/// Database `db` is in a comma-separated list of names.
fn listed(list: &str, db: &str) -> bool {
    list.split(',')
        .map(|d| d.trim().trim_matches('"'))
        .any(|d| d == db)
}

/// A job's id, unless its name can't make one (a control character, too
/// long): such a job is listed but offers nothing.
fn usable(job: &mut ServerJob) {
    if JobId::parse(&job.id).is_err() {
        job.can_run = false;
        job.can_enable = false;
        job.has_history = false;
        job.has_script = false;
    }
}

// ---------------------------------------------------------------------------
// The list

pub(super) async fn list(s: &PgSession, f: &Found) -> Result<Option<Part>> {
    let e = Edb::read(s, f).await?;
    if !e.present() && !e.scheduler {
        return Ok(None);
    }
    let mut part = Part::default();
    if e.on_pgagent() {
        if e.present() {
            part.notes.push(ON_PGAGENT.into());
        }
        return Ok(Some(part));
    }
    if !e.present() {
        part.notes.push(NO_PACKAGES.into());
        return Ok(Some(part));
    }
    let (dbms_jobs, sched_jobs) = (e.dbms_jobs(), e.sched_jobs());
    if dbms_jobs.is_none() && sched_jobs.is_none() {
        part.notes.push(UNREADABLE.into());
        return Ok(Some(part));
    }
    part.notes
        .extend(e.unreadable_notes().into_iter().map(String::from));
    let mut failed: Vec<(&str, Error)> = Vec::new();
    if let Some((v, scope)) = dbms_jobs {
        match s.text(&dbms_jobs_sql(v, None).unwrap_or_default()).await {
            Ok(rows) => {
                part.jobs
                    .extend(rows.iter().filter_map(|r| dbms_job(r, &e)));
                if scope != Scope::All {
                    part.notes.push(DBMS_JOB_PARTIAL.into());
                }
            }
            Err(err) => failed.push(("DBMS_JOB", err)),
        }
    }
    if let Some((v, scope)) = sched_jobs {
        match s
            .text(&scheduler::jobs_sql(v, e.schedules(), None).unwrap_or_default())
            .await
        {
            Ok(rows) => {
                let jobs: Vec<SchedJob> =
                    rows.iter().filter_map(|r| SchedJob::from_row(r)).collect();
                // The owners each name has: one row per owner and name.
                let mut owners: HashMap<&str, usize> = HashMap::new();
                for j in &jobs {
                    *owners.entry(j.name.as_str()).or_default() += 1;
                }
                part.jobs.extend(
                    jobs.iter()
                        .map(|j| sched_job(j, &e, scope, owners[j.name.as_str()] == 1)),
                );
                if scope != Scope::All {
                    part.notes.push(SCHEDULER_PARTIAL.into());
                }
                if !jobs.is_empty() {
                    part.notes.push(scheduler::RUN_WAITS.into());
                }
            }
            Err(err) => failed.push(("DBMS_SCHEDULER", err)),
        }
    }
    let sources = usize::from(dbms_jobs.is_some()) + usize::from(sched_jobs.is_some());
    if failed.len() == sources {
        return Err(failed.remove(0).1);
    }
    for (source, err) in failed {
        part.notes
            .push(format!("No se pudieron leer los jobs de {source}: {err}"));
    }
    part.notes.extend(e.off());
    Ok(Some(part))
}

async fn ready(s: &PgSession) -> Result<Edb> {
    let f = Found::detect(s).await?;
    let e = Edb::read(s, &f).await?;
    if e.on_pgagent() {
        return Err(Error::Query(
            "en este servidor DBMS_JOB y DBMS_SCHEDULER guardan sus jobs en pgAgent: están entre los jobs de pgAgent"
                .into(),
        ));
    }
    Ok(e)
}

// ---------------------------------------------------------------------------
// DBMS_JOB

/// `DBA_JOBS` (or `ALL_JOBS`), every job or job `id`. Its times are
/// `timestamptz`; `now` is there to tell a next date still ahead.
fn dbms_jobs_sql(v: &View, id: Option<i64>) -> Option<String> {
    let job = v.col("j", "job")?;
    let filter = id
        .map(|n| format!(" WHERE {job} = {n}"))
        .unwrap_or_default();
    Some(format!(
        "SELECT {job}::text AS job, {log_user} AS log_user, {schema_user} AS schema_user,
                {last} AS last_date, {this} AS this_date, {next} AS next_date, {next_iso} AS next_iso,
                {total} AS total_time, {broken} AS broken, {interval} AS job_interval,
                {failures} AS failures, {what} AS what, extract(epoch FROM now())::text AS now
         FROM {from} j{filter} ORDER BY {job}",
        log_user = v.read("j", "log_user"),
        schema_user = v.read("j", "schema_user"),
        last = v.read("j", "last_date"),
        this = v.read("j", "this_date"),
        next = v.read("j", "next_date"),
        next_iso = v.iso("j", "next_date"),
        total = v.raw("j", "total_time"),
        broken = v.raw("j", "broken"),
        interval = v.read("j", "interval"),
        failures = v.read("j", "failures"),
        what = v.read("j", "what"),
        from = v.from(),
    ))
}

fn epoch(r: &dyn Row, name: &str) -> Option<f64> {
    r.get(name).and_then(|v| v.trim().parse::<f64>().ok())
}

/// The `interval` that sets its next date; `None` for a job that runs once.
fn interval(r: &dyn Row) -> Option<String> {
    text(r, "job_interval").filter(|i| !i.eq_ignore_ascii_case("null"))
}

fn broken(r: &dyn Row) -> bool {
    text(r, "broken").is_some_and(|b| scheduler::yes(&b))
}

/// Its next date, unless it's the far one of a broken job.
fn next(r: &dyn Row) -> Option<String> {
    time(r, "next_date").filter(|_| epoch(r, "next_date").is_some_and(|t| t < BROKEN_DATE))
}

/// Its next date as ISO text, when it's still ahead (to keep it when the
/// job is enabled again).
fn next_ahead(r: &dyn Row) -> Option<String> {
    let (next, now) = (epoch(r, "next_date")?, epoch(r, "now")?);
    (next > now && next < BROKEN_DATE)
        .then(|| text(r, "next_iso"))
        .flatten()
}

fn dbms_job(r: &dyn Row, e: &Edb) -> Option<ServerJob> {
    let n: i64 = text(r, "job")?.parse().ok()?;
    let broken = broken(r);
    let running = time(r, "this_date").is_some();
    let failed = epoch(r, "failures").is_some_and(|f| f > 0.0);
    let last = time(r, "last_date");
    let owner = text(r, "log_user");
    let may = e.may(owner.as_deref());
    Some(ServerJob {
        id: JobId::EdbJob(n).to_string(),
        name: format!("Job {n}"),
        kind: KIND.into(),
        group: text(r, "schema_user"),
        description: text(r, "what"),
        owner,
        enabled: !broken,
        schedule: interval(r),
        running,
        // `last_date` is the last success; `failures`, those since.
        last_outcome: if failed {
            Some(JobOutcome::Failed)
        } else if last.is_some() {
            Some(JobOutcome::Succeeded)
        } else if running {
            Some(JobOutcome::Running)
        } else {
            None
        },
        last_started: if running {
            time(r, "this_date")
        } else {
            last.filter(|_| !failed)
        },
        last_duration_ms: None,
        next_run: next(r).filter(|_| !broken),
        can_run: may && !broken && !running && e.off().is_none(),
        can_stop: false,
        can_enable: may,
        has_history: e.history().is_some(),
        has_script: true,
        system: false,
    })
}

async fn job_row(s: &PgSession, e: &Edb, n: i64) -> Result<SimpleQueryRow> {
    let (v, _) = e.dbms_jobs().ok_or_else(|| {
        Error::Query("esta base no tiene DBMS_JOB, o tu usuario no puede leer sys.all_jobs".into())
    })?;
    s.text(&dbms_jobs_sql(v, Some(n)).unwrap_or_default())
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(JobId::EdbJob(n)))
}

pub(super) async fn job_detail(s: &PgSession, n: i64) -> Result<JobDetail> {
    let e = ready(s).await?;
    let r = job_row(s, &e, n).await?;
    let job = dbms_job(&r, &e).ok_or_else(|| not_found(JobId::EdbJob(n)))?;
    let mut properties = vec![prop("", "Id", n.to_string())];
    let mut add = |label: &str, value: Option<String>| {
        if let Some(v) = value {
            properties.push(prop("", label, v));
        }
    };
    add("Usuario", text(&r, "log_user"));
    add("Esquema", text(&r, "schema_user"));
    add("Última ejecución correcta", time(&r, "last_date"));
    add("Fallas seguidas", text(&r, "failures"));
    add("Tiempo total", text(&r, "total_time"));
    add(
        "Se ejecuta una sola vez",
        Some(yes_no(interval(&r).is_none()).into()),
    );
    Ok(JobDetail {
        job,
        steps: vec![JobStep {
            name: "Comando".into(),
            kind: Some("PL/SQL".into()),
            target: None,
            command: text(&r, "what"),
        }],
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub(super) async fn job_runs(s: &PgSession, n: i64, limit: u32) -> Result<Vec<JobRun>> {
    let e = ready(s).await?;
    job_row(s, &e, n).await?;
    let (jobs, runs) = e.history().ok_or_else(no_history)?;
    let sql = history_sql(jobs, runs, Needle::Job(n), limit).ok_or_else(no_history)?;
    Ok(s.text(&sql).await?.iter().map(|r| history_run(r)).collect())
}

fn job_run_sql(n: i64) -> String {
    D.call(&format!("DBMS_JOB.NEXT_DATE({n}, SYSDATE)"))
}

pub(super) async fn job_run(s: &PgSession, n: i64) -> Result<()> {
    let e = ready(s).await?;
    let r = job_row(s, &e, n).await?;
    if broken(&r) {
        return Err(Error::Query(
            "el job está deshabilitado: EDB no lo ejecuta hasta habilitarlo".into(),
        ));
    }
    if let Some(why) = e.off() {
        return Err(Error::Query(why.trim_end_matches('.').into()));
    }
    s.text(&job_run_sql(n)).await?;
    Ok(())
}

/// Broken (disabled), or fixed with its next date kept when it's still
/// ahead (else `BROKEN`'s default, now).
fn job_enable_sql(n: i64, enabled: bool, keep: Option<&str>) -> String {
    D.call(&match (enabled, keep) {
        (false, _) => format!("DBMS_JOB.BROKEN({n}, TRUE)"),
        (true, Some(next)) => format!("DBMS_JOB.BROKEN({n}, FALSE, {})", D.time_lit(next)),
        (true, None) => format!("DBMS_JOB.BROKEN({n}, FALSE)"),
    })
}

pub(super) async fn job_set_enabled(s: &PgSession, n: i64, enabled: bool) -> Result<()> {
    let e = ready(s).await?;
    let r = job_row(s, &e, n).await?;
    let keep = if enabled { next_ahead(&r) } else { None };
    s.text(&job_enable_sql(n, enabled, keep.as_deref())).await?;
    Ok(())
}

/// `DBMS_JOB.SUBMIT` with its command, next date and interval, then
/// `BROKEN` when it's disabled.
fn job_script_of(r: &dyn Row) -> String {
    let n = text(r, "job").unwrap_or_default();
    let broken = broken(r);
    let next = match text(r, "next_iso").filter(|_| !broken && next(r).is_some()) {
        Some(t) => D.time_lit(&t),
        None => "SYSDATE".into(),
    };
    let interval = interval(r)
        .map(|i| D.lit(&i))
        .unwrap_or_else(|| "NULL".into());
    let mut out = format!("-- EDB Postgres Advanced Server: Job {n} ({KIND})\n");
    out.push_str(&format!(
        "-- DBMS_JOB le da un número nuevo y lo crea a nombre del usuario que lo ejecute (el original es de «{}»).\n",
        comment_text(&text(r, "log_user").unwrap_or_default())
    ));
    out.push_str(&format!(
        "DECLARE\n  j INTEGER;\nBEGIN\n  DBMS_JOB.SUBMIT(j, {}, {next}, {interval});\n",
        D.lit(&r.get("what").unwrap_or_default())
    ));
    if broken {
        out.push_str("  DBMS_JOB.BROKEN(j, TRUE);\n");
    }
    out.push_str("END;\n");
    out
}

pub(super) async fn job_script(s: &PgSession, n: i64) -> Result<String> {
    let e = ready(s).await?;
    let r = job_row(s, &e, n).await?;
    Ok(job_script_of(&r))
}

// ---------------------------------------------------------------------------
// DBMS_SCHEDULER

/// Job `j` as listed from a view of `scope`; `unique`: no other owner's
/// job of its name is in sight.
fn sched_job(j: &SchedJob, e: &Edb, scope: Scope, unique: bool) -> ServerJob {
    let owner = e.owner_of(j, scope);
    let mut job = j.server_job();
    // The id keeps the view's owner: none from a `USER_*` view.
    job.id = JobId::EdbScheduler(Named {
        owner: j.owner.clone(),
        name: j.name.clone(),
    })
    .to_string();
    job.can_run = false;
    job.can_enable = e.owns(owner.as_deref()) && unique;
    job.has_history = e.sched_history(owner.as_deref(), unique).is_some();
    job.has_script = scheduler::edb_script(j).is_some();
    usable(&mut job);
    job
}

fn no_scheduler() -> Error {
    Error::Query("esta base no tiene DBMS_SCHEDULER, o tu usuario no puede leer sus vistas".into())
}

/// Job `id` from the widest jobs view, and that view's scope.
async fn sched_row(s: &PgSession, e: &Edb, id: &Named) -> Result<(SchedJob, Scope)> {
    let (v, scope) = e.sched_jobs().ok_or_else(no_scheduler)?;
    let filter = scheduler::job_filter(D, v, "v", id.owner.as_deref(), &id.name);
    let sql = scheduler::jobs_sql(v, e.schedules(), filter).unwrap_or_default();
    let j = s
        .text(&sql)
        .await?
        .iter()
        .find_map(|r| SchedJob::from_row(r))
        .ok_or_else(|| not_found(JobId::EdbScheduler(id.clone())))?;
    Ok((j, scope))
}

/// How many owners have a job named `name` in view `v`; `None` when the
/// view has no owners (the name is the key there).
fn owners_sql(v: &View, name: &str) -> Option<String> {
    Some(format!(
        "SELECT count(DISTINCT {})::text AS owners FROM {} v WHERE {} = {}",
        v.col("v", "owner")?,
        v.from(),
        v.col("v", "job_name")?,
        D.lit(name)
    ))
}

/// No other owner's job named `name` is in sight.
async fn unique(s: &PgSession, e: &Edb, name: &str) -> Result<bool> {
    let Some(sql) = e.sched_jobs().and_then(|(v, _)| owners_sql(v, name)) else {
        return Ok(true);
    };
    let owners = s
        .text(&sql)
        .await?
        .first()
        .and_then(|r| text(r, "owners"))
        .and_then(|n| n.parse::<u64>().ok());
    Ok(owners.is_none_or(|n| n <= 1))
}

async fn program(s: &PgSession, e: &Edb, j: &SchedJob) -> Option<Program> {
    let (v, _) = e.cat.widest("scheduler_programs")?;
    let sql = scheduler::program_sql(D, v, j.program.as_deref()?)?;
    match s.text(&sql).await {
        Ok(rows) => rows.iter().find_map(|r| Program::from_row(r)),
        Err(err) => {
            tracing::debug!("DBMS_SCHEDULER program of {}: {err}", j.name);
            None
        }
    }
}

pub(super) async fn sched_detail(s: &PgSession, id: &Named) -> Result<JobDetail> {
    let e = ready(s).await?;
    let (j, scope) = sched_row(s, &e, id).await?;
    let unique = unique(s, &e, &j.name).await?;
    let program = program(s, &e, &j).await;
    let mut properties = vec![prop("", "Nombre", j.name.clone())];
    properties.extend(j.properties());
    Ok(JobDetail {
        job: sched_job(&j, &e, scope, unique),
        steps: j.steps(program.as_ref()),
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub(super) async fn sched_runs(s: &PgSession, id: &Named, limit: u32) -> Result<Vec<JobRun>> {
    let e = ready(s).await?;
    let (j, scope) = sched_row(s, &e, id).await?;
    let owner = e.owner_of(&j, scope);
    let unique = unique(s, &e, &j.name).await?;
    match e.sched_history(owner.as_deref(), unique) {
        Some(SchedHistory::Details(v)) => {
            let sql = scheduler::runs_sql(D, v, owner.as_deref(), &j.name, limit)
                .ok_or_else(no_history)?;
            Ok(s.text(&sql)
                .await?
                .iter()
                .map(|r| scheduler::run_of(r))
                .collect())
        }
        Some(SchedHistory::Jobs(jobs, runs)) => {
            let sql = history_sql(jobs, runs, Needle::Scheduler(&j.name), limit)
                .ok_or_else(no_history)?;
            Ok(s.text(&sql).await?.iter().map(|r| history_run(r)).collect())
        }
        None => Err(no_history()),
    }
}

/// Never runs it: see [`scheduler::RUN_WAITS`]. An unknown id still says
/// so first.
pub(super) async fn sched_run(s: &PgSession, id: &Named) -> Result<()> {
    let e = ready(s).await?;
    sched_row(s, &e, id).await?;
    Err(scheduler::run_waits())
}

pub(super) async fn sched_set_enabled(s: &PgSession, id: &Named, enabled: bool) -> Result<()> {
    let e = ready(s).await?;
    let (j, scope) = sched_row(s, &e, id).await?;
    if !e.owns(e.owner_of(&j, scope).as_deref()) {
        return Err(Error::Unsupported(NOT_OWN.into()));
    }
    if !unique(s, &e, &j.name).await? {
        return Err(Error::Unsupported(SAME_NAME.into()));
    }
    s.text(&scheduler::enable_sql(D, &j.name, enabled)).await?;
    Ok(())
}

pub(super) async fn sched_script(s: &PgSession, id: &Named) -> Result<String> {
    let e = ready(s).await?;
    let (j, _) = sched_row(s, &e, id).await?;
    scheduler::edb_script(&j).ok_or_else(|| {
        Error::Unsupported(
            "no se puede generar el script de este job: el servidor no muestra su acción ni su programa"
                .into(),
        )
    })
}

// ---------------------------------------------------------------------------
// edb_job_scheduler's history

fn no_history() -> Error {
    Error::Unsupported(
        "no se puede leer el historial de edb_job_scheduler (sys.job_run_details) con este usuario"
            .into(),
    )
}

/// How a job's rows in `sys.jobs` are told apart: by the command
/// edb_job_scheduler runs for it.
#[derive(Debug, Clone, Copy)]
enum Needle<'a> {
    /// `dbms_job.run(<n>, …)`.
    Job(i64),
    /// A DBMS_SCHEDULER call with the job's name as a literal.
    Scheduler(&'a str),
}

/// The newest `limit` runs in `sys.job_run_details` of the jobs in
/// `sys.jobs` whose command is the needle's.
fn history_sql(jobs: &View, runs: &View, needle: Needle, limit: u32) -> Option<String> {
    let cmd = jobs.col("j", "jobcommand")?;
    let matches = match needle {
        Needle::Job(n) => format!(
            "{cmd} ~* {}",
            D.lit(&format!(r"\mdbms_job\.run\s*\(\s*{n}\s*[,)]"))
        ),
        Needle::Scheduler(name) => format!(
            "{cmd} ILIKE '%dbms_scheduler%' AND strpos(lower({cmd}), lower({})) > 0",
            D.lit(&format!("'{}'", name.replace('\'', "''")))
        ),
    };
    let order = ["runid", "starttime"]
        .iter()
        .find_map(|c| runs.col("d", c))
        .map(|c| format!(" ORDER BY {c} DESC NULLS LAST"))
        .unwrap_or_default();
    Some(format!(
        "SELECT {} AS id, {} AS status, {} AS message, {} AS started, {} AS scheduled, {} AS ended
         FROM {} d WHERE {} IN (SELECT {} FROM {} j WHERE {matches}){order} LIMIT {limit}",
        runs.raw("d", "runid"),
        runs.raw("d", "status"),
        runs.read("d", "error"),
        runs.read("d", "starttime"),
        runs.read("d", "scheduletime"),
        runs.read("d", "endtime"),
        runs.from(),
        runs.col("d", "jobid")?,
        jobs.col("j", "jobid")?,
        jobs.from(),
    ))
}

fn history_run(r: &dyn Row) -> JobRun {
    let duration_ms = match (epoch(r, "started"), epoch(r, "ended")) {
        (Some(a), Some(b)) if b >= a => Some(((b - a) * 1000.0).round() as u64),
        _ => None,
    };
    JobRun {
        id: text(r, "id"),
        started: time(r, "started").or_else(|| time(r, "scheduled")),
        duration_ms,
        // r: running; s: success; f: failure.
        outcome: match text(r, "status").as_deref() {
            Some("r") => JobOutcome::Running,
            Some("s") => JobOutcome::Succeeded,
            Some("f") => JobOutcome::Failed,
            _ => JobOutcome::Unknown,
        },
        trigger: None,
        message: message(r, "message"),
        steps: Vec::new(),
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::super::scheduler::Kind;
    use super::*;

    fn edb(superuser: bool) -> Edb {
        Edb {
            cat: Catalog::with(vec![
                View::fake(
                    "sys",
                    "jobs",
                    &[("jobid", Kind::Other), ("jobcommand", Kind::Other)],
                ),
                View::fake(
                    "sys",
                    "job_run_details",
                    &[
                        ("jobid", Kind::Other),
                        ("runid", Kind::Other),
                        ("status", Kind::Other),
                        ("error", Kind::Other),
                        ("starttime", Kind::TimeTz),
                        ("scheduletime", Kind::TimeTz),
                        ("endtime", Kind::TimeTz),
                    ],
                ),
            ]),
            user: "app".into(),
            superuser,
            database: "edb".into(),
            reads_settings: true,
            pgagent: false,
            version: 170002,
            scheduler: true,
            loaded: true,
            database_list: Some("edb, \"other\"".into()),
        }
    }

    fn submitted(broken: &str) -> super::super::Fixture {
        fixture(&[
            ("job", "104"),
            ("log_user", "app"),
            ("schema_user", "public"),
            ("next_date", "1794533465"),
            ("next_iso", "2026-11-13T01:31:05+00:00"),
            ("now", "1794488199"),
            ("last_date", "1794488199"),
            ("broken", broken),
            ("job_interval", "SYSDATE + 1"),
            ("failures", "0"),
            ("what", "job_proc;"),
        ])
    }

    #[test]
    fn a_dbms_job_row_maps_to_a_job() {
        let e = edb(false);
        let j = dbms_job(&submitted("N"), &e).unwrap();
        assert_eq!(j.id, "edb_job:104");
        assert_eq!(j.name, "Job 104");
        assert_eq!(j.kind, "DBMS_JOB");
        assert_eq!(j.group.as_deref(), Some("public"));
        assert_eq!(j.description.as_deref(), Some("job_proc;"));
        assert_eq!(j.schedule.as_deref(), Some("SYSDATE + 1"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T01:31:05Z"));
        assert!(j.enabled && j.can_run && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_stop);
        // Someone else's: DBMS_JOB changes only the caller's jobs.
        let other = dbms_job(
            &submitted("N"),
            &Edb {
                user: "bob".into(),
                ..edb(false)
            },
        )
        .unwrap();
        assert!(!other.can_run && !other.can_enable);
        let admin = Edb {
            user: "bob".into(),
            ..edb(true)
        };
        assert!(dbms_job(&submitted("N"), &admin).unwrap().can_enable);
        let broken = dbms_job(&submitted("Y"), &e).unwrap();
        assert!(!broken.enabled && !broken.can_run && broken.next_run.is_none());
        // Failures since the last success: the last run failed.
        let failed = fixture(&[("job", "5"), ("failures", "2"), ("last_date", "1")]);
        let f = dbms_job(&failed, &e).unwrap();
        assert_eq!(f.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(f.last_started, None);
        let running = fixture(&[("job", "6"), ("this_date", "1794488199")]);
        let r = dbms_job(&running, &e).unwrap();
        assert!(r.running && !r.can_run);
        assert_eq!(r.last_started.as_deref(), Some("2026-11-12T12:56:39Z"));
        // A run-once job.
        let once = fixture(&[("job", "7"), ("job_interval", "null")]);
        assert_eq!(dbms_job(&once, &e).unwrap().schedule, None);
    }

    #[test]
    fn a_stopped_scheduler_runs_nothing_now() {
        let v = 170002;
        assert!(off_note(v, true, true, Some("edb"), true, "edb").is_none());
        assert!(off_note(v, true, true, Some("a, \"edb\" ,b"), true, "edb").is_none());
        assert!(off_note(v, false, false, None, true, "edb")
            .unwrap()
            .contains("no está instalado"));
        assert!(off_note(v, true, false, None, true, "edb")
            .unwrap()
            .contains("shared_preload_libraries"));
        // Without the right to read every setting nothing can be told.
        assert!(off_note(v, true, false, None, false, "edb").is_none());
        assert!(off_note(v, true, true, Some("postgres"), true, "edb")
            .unwrap()
            .contains("«edb»"));
        // Loaded with an empty list: it serves no database, it isn't
        // "not loaded".
        let empty = off_note(v, true, true, Some(""), false, "edb").unwrap();
        assert!(empty.contains("«edb»") && !empty.contains("shared_preload_libraries"));
        let e = Edb {
            database_list: Some("postgres".into()),
            ..edb(true)
        };
        assert!(!dbms_job(&submitted("N"), &e).unwrap().can_run);
    }

    #[test]
    fn pgagent_holds_them_before_16() {
        let old = Edb {
            scheduler: false,
            pgagent: true,
            version: 150004,
            ..edb(true)
        };
        assert!(old.on_pgagent());
        assert!(!Edb {
            version: 160000,
            ..old
        }
        .on_pgagent());
        assert!(!edb(true).on_pgagent());
        // Before 16 with pgAgent in another database: that's the reason,
        // not edb_job_scheduler (which doesn't exist there).
        let elsewhere = Edb {
            scheduler: false,
            pgagent: false,
            loaded: false,
            version: 150004,
            ..edb(true)
        };
        assert!(!elsewhere.on_pgagent());
        assert_eq!(elsewhere.off().as_deref(), Some(PGAGENT_ELSEWHERE));
        assert!(!dbms_job(&submitted("N"), &elsewhere).unwrap().can_run);
    }

    #[test]
    fn a_package_the_user_cant_read_is_named() {
        let fake = |name: &str, readable: bool| {
            let v = View::fake(
                "sys",
                name,
                &[("job", Kind::Other), ("job_name", Kind::Other)],
            );
            if readable {
                v
            } else {
                v.hidden()
            }
        };
        let with = |views: Vec<View>| Edb {
            cat: Catalog::with(views),
            ..edb(false)
        };
        // DBMS_JOB readable, DBMS_SCHEDULER not.
        let e = with(vec![
            fake("all_jobs", true),
            fake("all_scheduler_jobs", false),
        ]);
        assert_eq!(e.unreadable_notes(), vec![SCHEDULER_UNREADABLE]);
        // The other way round.
        let e = with(vec![
            fake("dba_jobs", false),
            fake("user_scheduler_jobs", true),
        ]);
        assert_eq!(e.unreadable_notes(), vec![DBMS_JOB_UNREADABLE]);
        // Both readable, or one package missing: nothing to say.
        let e = with(vec![
            fake("all_jobs", true),
            fake("all_scheduler_jobs", true),
        ]);
        assert!(e.unreadable_notes().is_empty());
        assert!(with(vec![fake("all_jobs", true)])
            .unreadable_notes()
            .is_empty());
        // Neither readable is UNREADABLE's, said by the list on its own.
        let e = with(vec![
            fake("all_jobs", false),
            fake("all_scheduler_jobs", false),
        ]);
        assert!(e.unreadable_notes().is_empty());
    }

    #[test]
    fn the_dbms_job_query_reads_the_documented_columns() {
        let v = View::fake(
            "sys",
            "dba_jobs",
            &[
                ("job", Kind::Other),
                ("log_user", Kind::Other),
                ("next_date", Kind::TimeTz),
                ("broken", Kind::Other),
                ("interval", Kind::Other),
                ("what", Kind::Other),
            ],
        );
        let sql = dbms_jobs_sql(&v, Some(104)).unwrap();
        assert!(sql.contains("FROM \"sys\".\"dba_jobs\" j WHERE j.\"job\" = 104"));
        assert!(sql.contains("extract(epoch FROM j.\"next_date\")::text AS next_date"));
        assert!(sql.contains("to_json(j.\"next_date\") #>> '{}' AS next_iso"));
        assert!(sql.contains("j.\"interval\"::text AS job_interval"));
        assert!(sql.contains("NULL::text AS this_date"));
        assert!(dbms_jobs_sql(&View::fake("sys", "dba_jobs", &[]), None).is_none());
    }

    #[test]
    fn dbms_job_calls() {
        assert_eq!(
            job_run_sql(104),
            "BEGIN DBMS_JOB.NEXT_DATE(104, SYSDATE); END;"
        );
        assert_eq!(
            job_enable_sql(104, false, None),
            "BEGIN DBMS_JOB.BROKEN(104, TRUE); END;"
        );
        assert_eq!(
            job_enable_sql(104, true, None),
            "BEGIN DBMS_JOB.BROKEN(104, FALSE); END;"
        );
        assert_eq!(
            job_enable_sql(104, true, Some("2026-11-13T01:31:05+00:00")),
            "BEGIN DBMS_JOB.BROKEN(104, FALSE, CAST(E'2026-11-13T01:31:05+00:00' AS TIMESTAMP WITH TIME ZONE)); END;"
        );
        assert_eq!(
            next_ahead(&submitted("N")).as_deref(),
            Some("2026-11-13T01:31:05+00:00")
        );
        let past = fixture(&[
            ("next_date", "1"),
            ("now", "2"),
            ("next_iso", "1970-01-01T00:00:01+00:00"),
        ]);
        assert_eq!(next_ahead(&past), None);
        let far = fixture(&[
            ("next_date", "64060588800"),
            ("now", "2"),
            ("next_iso", "4000-01-01T00:00:00+00:00"),
        ]);
        assert_eq!(next_ahead(&far), None);
    }

    #[test]
    fn the_dbms_job_script_submits_it_again() {
        let sql = job_script_of(&submitted("N"));
        assert!(sql.contains("«app»"), "{sql}");
        assert!(sql.contains(
            "DBMS_JOB.SUBMIT(j, E'job_proc;', CAST(E'2026-11-13T01:31:05+00:00' AS TIMESTAMP WITH TIME ZONE), E'SYSDATE + 1');"
        ));
        assert!(!sql.contains("BROKEN") && sql.ends_with("END;\n"));
        let broken = job_script_of(&submitted("Y"));
        assert!(broken.contains("DBMS_JOB.SUBMIT(j, E'job_proc;', SYSDATE, E'SYSDATE + 1');"));
        assert!(broken.contains("DBMS_JOB.BROKEN(j, TRUE);"));
        let once = job_script_of(&fixture(&[("job", "1"), ("what", "x;")]));
        assert!(once.contains("DBMS_JOB.SUBMIT(j, E'x;', SYSDATE, NULL);"));
    }

    #[test]
    fn history_is_found_by_the_command_each_job_runs() {
        let e = edb(true);
        let (jobs, runs) = e.history().unwrap();
        let sql = history_sql(jobs, runs, Needle::Job(104), 50).unwrap();
        assert!(
            sql.contains(
                "WHERE j.\"jobcommand\" ~* E'\\\\mdbms_job\\\\.run\\\\s*\\\\(\\\\s*104\\\\s*[,)]'"
            ),
            "{sql}"
        );
        assert!(sql.contains("FROM \"sys\".\"job_run_details\" d WHERE d.\"jobid\" IN (SELECT j.\"jobid\" FROM \"sys\".\"jobs\" j"));
        assert!(sql.ends_with("ORDER BY d.\"runid\" DESC NULLS LAST LIMIT 50"));
        let sql = history_sql(jobs, runs, Needle::Scheduler("it's"), 5).unwrap();
        assert!(
            sql.contains("ILIKE '%dbms_scheduler%' AND strpos(lower(j.\"jobcommand\"), lower(E'''it''''s''')) > 0"),
            "{sql}"
        );
        let run = history_run(&fixture(&[
            ("id", "9"),
            ("status", "f"),
            ("message", "division by zero"),
            ("started", "1794488199"),
            ("ended", "1794488200.25"),
        ]));
        assert_eq!(run.outcome, JobOutcome::Failed);
        assert_eq!(run.duration_ms, Some(1250));
        assert_eq!(run.message.as_deref(), Some("division by zero"));
        let going = history_run(&fixture(&[("status", "r"), ("scheduled", "0")]));
        assert_eq!(going.outcome, JobOutcome::Running);
        assert_eq!(going.started.as_deref(), Some("1970-01-01T00:00:00Z"));
        assert_eq!(going.duration_ms, None);
        // Without sys.jobs the runs can't be told apart.
        let blind = Edb {
            cat: Catalog::default(),
            ..edb(true)
        };
        assert!(blind.history().is_none());
        assert!(!dbms_job(&submitted("N"), &blind).unwrap().has_history);
    }

    #[test]
    fn a_scheduler_job_gets_its_id_and_rights() {
        let j = SchedJob {
            owner: Some("app".into()),
            name: "nightly \"x\"".into(),
            job_type: Some("PLSQL_BLOCK".into()),
            action: Some("BEGIN NULL; END;".into()),
            enabled: Some(true),
            ..Default::default()
        };
        let s = sched_job(&j, &edb(false), Scope::All, true);
        assert_eq!(s.id, "edb_scheduler:\"app\".\"nightly \"\"x\"\"\"");
        assert!(JobId::parse(&s.id).is_ok());
        assert!(s.can_enable && s.has_history && s.has_script);
        // RUN_JOB would hold DBine's session until the run ends.
        assert!(!s.can_run && !s.can_stop);
        let theirs = SchedJob {
            owner: Some("bob".into()),
            ..j.clone()
        };
        assert!(!sched_job(&theirs, &edb(false), Scope::All, true).can_enable);
        // Not even for a superuser: the calls take the bare name, which
        // reaches the caller's own job.
        assert!(!sched_job(&theirs, &edb(true), Scope::All, true).can_enable);
        // Another owner has a job of that name: not changed, nor its runs
        // read by name from sys.jobs.
        let shared = sched_job(&j, &edb(true), Scope::All, false);
        assert!(!shared.can_enable && !shared.has_history && shared.has_script);
        let odd = sched_job(
            &SchedJob {
                name: "a\u{1}b".into(),
                ..j.clone()
            },
            &edb(true),
            Scope::All,
            true,
        );
        assert!(!odd.can_run && !odd.can_enable && !odd.has_history && !odd.has_script);
        // From USER_SCHEDULER_JOBS, which has no owner: the user's own.
        let mine = sched_job(
            &SchedJob {
                owner: None,
                ..j.clone()
            },
            &edb(false),
            Scope::Own,
            true,
        );
        assert!(mine.can_enable && mine.owner.is_none());
        assert_eq!(mine.id, "edb_scheduler:\"nightly \"\"x\"\"\"");
        let unknown = sched_job(
            &SchedJob { owner: None, ..j },
            &edb(false),
            Scope::Visible,
            true,
        );
        assert!(!unknown.can_enable);
    }

    #[test]
    fn a_scheduler_jobs_runs_come_from_a_view_that_tells_it_apart() {
        let details = |name: &str, cols: &[(&str, Kind)]| View::fake("sys", name, cols);
        let named = [("job_name", Kind::Other)];
        let owned = [("job_name", Kind::Other), ("owner", Kind::Other)];
        let with = |extra: View| {
            let mut e = edb(false);
            let mut views = vec![
                View::fake(
                    "sys",
                    "jobs",
                    &[("jobid", Kind::Other), ("jobcommand", Kind::Other)],
                ),
                View::fake("sys", "job_run_details", &[("jobid", Kind::Other)]),
            ];
            views.push(extra);
            e.cat = Catalog::with(views);
            e
        };
        let is_details = |h: Option<SchedHistory>| matches!(h, Some(SchedHistory::Details(_)));
        let is_jobs = |h: Option<SchedHistory>| matches!(h, Some(SchedHistory::Jobs(..)));
        // USER_: only the user's own runs are there.
        let e = with(details("user_scheduler_job_run_details", &named));
        assert!(is_details(e.sched_history(Some("app"), true)));
        assert!(is_jobs(e.sched_history(Some("bob"), true)));
        assert!(e.sched_history(Some("bob"), false).is_none());
        // ALL_ with owners tells same-named jobs apart; without, only a
        // name one owner has.
        let e = with(details("all_scheduler_job_run_details", &owned));
        assert!(is_details(e.sched_history(Some("bob"), false)));
        let e = with(details("dba_scheduler_job_run_details", &named));
        assert!(is_details(e.sched_history(Some("bob"), true)));
        assert!(e.sched_history(Some("bob"), false).is_none());
    }

    #[test]
    fn same_named_jobs_are_counted_by_owner() {
        let v = View::fake(
            "sys",
            "all_scheduler_jobs",
            &[("owner", Kind::Other), ("job_name", Kind::Other)],
        );
        assert_eq!(
            owners_sql(&v, "it's").unwrap(),
            "SELECT count(DISTINCT v.\"owner\")::text AS owners FROM \"sys\".\"all_scheduler_jobs\" v WHERE v.\"job_name\" = E'it''s'"
        );
        // USER_SCHEDULER_JOBS: no owners, the name is the key.
        let own = View::fake("sys", "user_scheduler_jobs", &[("job_name", Kind::Other)]);
        assert!(owners_sql(&own, "x").is_none());
    }
}
