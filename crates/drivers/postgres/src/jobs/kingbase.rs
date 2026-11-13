//! KingbaseES's jobs: the kdb_schedule plugin's DBMS_JOB and
//! DBMS_SCHEDULER.
//!
//! kdb_schedule descends from pgAgent: every job, whichever package created
//! it, is a row of `kdb_job` (in `kdb_schedule` at first, later in the
//! catalog), a DBMS_JOB job named `internal_job<n>:<created>` with `<n>`
//! its number. That name is the only documented link to the number: when
//! `kdb_job.jobid` says another, DBine doesn't run or change the job. The
//! plugin's views (`kdb_schedule.dba_scheduler_jobs` and the rest, read as
//! [`super::scheduler`] does) add its action, schedule and state, and
//! `*_scheduler_job_run_details` its runs. Without `kdb_job` (no access)
//! the list comes from the views alone.
//!
//! Jobs run while `job_queue_processes` is above 0. "Run now" of a DBMS_JOB
//! job moves its next date to now (`DBMS_JOB.NEXT_DATE`), so the scheduler
//! takes it. A DBMS_SCHEDULER job only has `RUN_JOB`, which runs it in the
//! current session (KingbaseES takes no other `use_current_session`): it
//! isn't offered ([`scheduler::RUN_WAITS`]). Neither package stops a run.
//! The views have no owner: a user changes the jobs `user_scheduler_jobs`
//! shows them, a superuser any.
//!
//! A connection string (`DBMS_JOB.INSTANCE`, a program's `acconnstr`, the
//! views' `destination`) can carry a password: it's never read.
//!
//! Unverified: no KingbaseES server was at hand. This follows Kingbase's
//! documentation (the plugin's procedures, its views from V9R4, `kdb_job`
//! in the administrator's guide); how DBMS_JOB jobs are named in `kdb_job`
//! comes from Kingbase's own technical articles.

use super::scheduler::{self, Catalog, Dialect, Program, SchedJob, Scope, View};
use super::{flag, not_found, prop, text, time, Found, JobId, Named, Part, Row};
use crate::session::PgSession;
use dbine_driver::{Error, JobDetail, JobRun, JobStep, Result, ServerJob};
use std::collections::HashSet;

const D: Dialect = Dialect::Kingbase;

/// The views and tables read, by lower-case name.
const NAMES: &[&str] = &[
    "kdb_job",
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
];

const EXTENSION_SQL: &str =
    "SELECT EXISTS (SELECT 1 FROM pg_catalog.pg_extension WHERE extname = 'kdb_schedule') AS extension";

const QUEUE_OFF: &str = "El programador de jobs de KingbaseES está apagado (job_queue_processes = 0): sus jobs no se ejecutan.";
const UNREADABLE: &str = "Tu usuario no puede leer los jobs de kdb_schedule: necesita SELECT en kdb_job o en kdb_schedule.dba_scheduler_jobs.";
const NOT_FOUND: &str = "kdb_schedule está creado en esta base, pero DBine no encuentra su tabla kdb_job ni sus vistas: no puede leer sus jobs.";
const NO_VIEWS: &str = "Esta versión de KingbaseES no tiene las vistas de DBMS_SCHEDULER (kdb_schedule.dba_scheduler_jobs): cada job se ve sin su acción ni su programación.";
const VIEWS_UNREADABLE: &str = "Tu usuario no puede leer las vistas de DBMS_SCHEDULER (kdb_schedule.all_scheduler_jobs): cada job se ve sin su acción ni su programación.";
const PARTIAL: &str = "Se ven solo los jobs a los que tiene acceso este usuario: para verlos todos hace falta poder leer kdb_job o kdb_schedule.dba_scheduler_jobs.";

/// What this database has of kdb_schedule, and who's asking.
pub(crate) struct Kdb {
    pub(crate) cat: Catalog,
    pub(crate) superuser: bool,
    pub(crate) database: String,
    /// The kdb_schedule extension is in this database.
    pub(crate) extension: bool,
    /// `job_queue_processes` is 0: no job runs.
    pub(crate) queue_off: bool,
    /// The jobs `user_scheduler_jobs` shows: the user's own.
    pub(crate) own: HashSet<String>,
}

impl Kdb {
    pub(crate) async fn read(s: &PgSession, f: &Found) -> Result<Kdb> {
        let cat = Catalog::read(s, D, NAMES).await?;
        let extension = s
            .text(EXTENSION_SQL)
            .await?
            .first()
            .is_some_and(|r| flag(r, "extension"));
        let mut k = Kdb {
            cat,
            superuser: f.superuser,
            database: f.database.clone(),
            extension,
            queue_off: f.job_queue_processes.as_deref() == Some("0"),
            own: HashSet::new(),
        };
        // A superuser's too: `user_scheduler_job_run_details` has only
        // their own runs.
        if let Some(sql) = k.own_sql() {
            match s.text(&sql).await {
                Ok(rows) => k.own = rows.iter().filter_map(|r| text(r, "name")).collect(),
                Err(e) => tracing::debug!("kdb_schedule: own jobs: {e}"),
            }
        }
        Ok(k)
    }

    pub(crate) fn own_sql(&self) -> Option<String> {
        let v = self.cat.readable("user_scheduler_jobs")?;
        Some(format!(
            "SELECT DISTINCT {}::text AS name FROM {} u",
            v.col("u", "job_name")?,
            v.from()
        ))
    }

    pub(crate) fn kdb_job(&self) -> Option<&View> {
        self.cat
            .readable("kdb_job")
            .filter(|v| v.has("jobid") && v.has("jobname"))
    }

    pub(crate) fn jobs(&self) -> Option<(&View, Scope)> {
        self.cat
            .widest("scheduler_jobs")
            .filter(|(v, _)| v.has("job_name"))
    }

    pub(crate) fn present(&self) -> bool {
        self.extension || self.cat.get("kdb_job").is_some() || self.cat.any("scheduler_jobs")
    }

    /// A run-details view and how much it shows.
    pub(crate) fn run_details(&self) -> Option<(&View, Scope)> {
        self.cat
            .widest("scheduler_job_run_details")
            .filter(|(v, _)| v.has("job_name"))
    }

    /// The run-details view that has job `name`'s runs: a `USER_` one only
    /// has the user's own.
    pub(crate) fn runs_of(&self, name: &str) -> Option<&View> {
        match self.run_details()? {
            (_, Scope::Own) if !self.own.contains(name) => None,
            (v, _) => Some(v),
        }
    }

    pub(crate) fn may(&self, name: &str) -> bool {
        self.superuser || self.own.contains(name)
    }

    /// What the list says about the jobs views when it reads `kdb_job`
    /// without them: missing, or not readable.
    pub(crate) fn views_note(&self) -> Option<&'static str> {
        match (self.kdb_job(), self.jobs()) {
            (Some(_), None) if self.cat.any("scheduler_jobs") => Some(VIEWS_UNREADABLE),
            (Some(_), None) => Some(NO_VIEWS),
            (None, Some((_, scope))) if scope != Scope::All => Some(PARTIAL),
            _ => None,
        }
    }
}

/// The number of a DBMS_JOB job from its name in `kdb_job`
/// (`internal_job<n>:<created>`).
pub(crate) fn dbms_job_number(name: &str) -> Option<i64> {
    let (digits, _) = name.strip_prefix("internal_job")?.split_once(':')?;
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    digits.parse().ok().filter(|n| *n > 0)
}

/// Every job (or the one named `name`): `kdb_job` with its row of the jobs
/// view, or the view alone. `k_*` are `kdb_job`'s columns.
pub(crate) fn jobs_sql(k: &Kdb, name: Option<&str>) -> Option<String> {
    let view = k.jobs().map(|(v, _)| v);
    let schedules = k.cat.widest("scheduler_schedules").map(|(v, _)| v);
    let now = "extract(epoch FROM now())::text AS now";
    if let Some(kj) = k.kdb_job() {
        let id = kj.col("k", "jobid")?;
        let jobname = kj.col("k", "jobname")?;
        let cols = [
            format!("{id}::text AS k_id"),
            format!("{jobname}::text AS k_name"),
            format!("{} AS k_enabled", kj.raw("k", "jobenabled")),
            format!("{} AS k_last", kj.read("k", "joblastrun")),
            format!("{} AS k_next", kj.read("k", "jobnextrun")),
            format!("{} AS k_next_iso", kj.iso("k", "jobnextrun")),
            now.to_string(),
        ]
        .join(", ");
        let view_part = view.and_then(|v| {
            let filter = format!("{} = {jobname}", v.col("v", "job_name")?);
            scheduler::jobs_sql(v, schedules, Some(filter))
        });
        let (rest, join) = match view_part {
            Some(sql) => (
                "j.*".to_string(),
                format!(" LEFT JOIN LATERAL ({sql} LIMIT 1) j ON true"),
            ),
            None => {
                // No view: its columns are NULL.
                let mut nulls = scheduler::job_select(&View::default(), "v");
                nulls.extend(scheduler::schedule_join(&View::default(), "v", None).0);
                (nulls.join(", "), String::new())
            }
        };
        let filter = name
            .map(|n| format!(" WHERE {jobname} = {}", D.lit(n)))
            .unwrap_or_default();
        return Some(format!(
            "SELECT {cols}, {rest} FROM {} k{join}{filter} ORDER BY {id}",
            kj.from()
        ));
    }
    let v = view?;
    let filter = match name {
        Some(n) => Some(scheduler::job_filter(D, v, "v", None, n)?),
        None => None,
    };
    Some(format!(
        "SELECT NULL::text AS k_id, NULL::text AS k_name, NULL::text AS k_enabled, NULL::text AS k_last,
                NULL::text AS k_next, NULL::text AS k_next_iso, {now}, j.*
         FROM ({}) j",
        scheduler::jobs_sql(v, schedules, filter)?
    ))
}

/// A job as `kdb_job` and the views say it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct KJob {
    /// `kdb_job.jobid`.
    pub(crate) id: Option<i64>,
    pub(crate) name: String,
    /// `kdb_job.jobenabled`.
    pub(crate) enabled: Option<bool>,
    pub(crate) last: Option<String>,
    pub(crate) next: Option<String>,
    pub(crate) next_iso: Option<String>,
    pub(crate) next_epoch: Option<f64>,
    pub(crate) now: Option<f64>,
    /// Its row of the jobs view.
    pub(crate) view: Option<SchedJob>,
}

pub(crate) fn epoch(r: &dyn Row, name: &str) -> Option<f64> {
    r.get(name).and_then(|v| v.trim().parse::<f64>().ok())
}

impl KJob {
    pub(crate) fn from_row(r: &dyn Row) -> Option<KJob> {
        let view = SchedJob::from_row(r);
        let name = text(r, "k_name").or_else(|| view.as_ref().map(|v| v.name.clone()))?;
        Some(KJob {
            id: text(r, "k_id").and_then(|n| n.parse().ok()),
            name,
            enabled: text(r, "k_enabled").map(|e| scheduler::yes(&e)),
            last: time(r, "k_last"),
            next: time(r, "k_next"),
            next_iso: text(r, "k_next_iso"),
            next_epoch: epoch(r, "k_next").or_else(|| epoch(r, "v_next_run_date")),
            now: epoch(r, "now"),
            view,
        })
    }

    /// Its DBMS_JOB number, if DBMS_JOB created it: the one in its name.
    pub(crate) fn number(&self) -> Option<i64> {
        dbms_job_number(&self.name)
    }

    /// Its name's DBMS_JOB number `n`, to call DBMS_JOB with, unless
    /// `kdb_job.jobid` says another (then which one DBMS_JOB takes can't be
    /// told, and the job isn't run or changed).
    pub(crate) fn checked(&self, n: i64) -> Result<i64> {
        match self.id {
            Some(id) if id != n => {
                tracing::debug!(
                    "kdb_schedule: {} has jobid {id}, not its name's {n}",
                    self.name
                );
                Err(Error::Unsupported(format!(
                    "DBine no puede saber el número de DBMS_JOB de este job: kdb_job dice {id} y su nombre, {n}"
                )))
            }
            _ => Ok(n),
        }
    }

    pub(crate) fn enabled(&self) -> bool {
        self.enabled
            .or_else(|| self.view.as_ref().map(SchedJob::is_enabled))
            .unwrap_or(true)
    }

    pub(crate) fn action(&self) -> Option<&str> {
        self.view.as_ref().and_then(|v| v.action.as_deref())
    }

    /// Its next run as ISO text, when it's still ahead.
    pub(crate) fn next_ahead(&self) -> Option<String> {
        let (next, now) = (self.next_epoch?, self.now?);
        (next > now)
            .then(|| {
                self.next_iso
                    .clone()
                    .or_else(|| self.view.as_ref().and_then(|v| v.next_iso.clone()))
            })
            .flatten()
    }

    pub(crate) fn id(&self) -> JobId {
        JobId::Kingbase(Named {
            owner: None,
            name: self.name.clone(),
        })
    }

    /// Whether DBine can write its script with what the views show.
    pub(crate) fn scriptable(&self) -> bool {
        match (self.number(), &self.view) {
            (Some(_), _) => self.action().is_some(),
            (None, Some(v)) => {
                v.program.is_some()
                    && v.schedule_name.is_some()
                    && (v.interval.is_some() || v.start_iso.is_some())
            }
            (None, None) => false,
        }
    }

    pub(crate) fn server_job(&self, k: &Kdb) -> ServerJob {
        let base = self
            .view
            .as_ref()
            .map(SchedJob::server_job)
            .unwrap_or_default();
        let number = self.number();
        let enabled = self.enabled();
        // DBMS_JOB's calls only with a number that's sure; DBMS_SCHEDULER's
        // RUN_JOB never (it would hold DBine's session to the end).
        let callable = number.is_none_or(|n| self.checked(n).is_ok());
        let may = k.may(&self.name) && callable;
        let mut job = ServerJob {
            id: self.id().to_string(),
            name: match number {
                Some(n) => format!("Job {n}"),
                None => self.name.clone(),
            },
            kind: if number.is_some() {
                "DBMS_JOB"
            } else {
                scheduler::KIND
            }
            .into(),
            group: None,
            description: match number {
                Some(_) => self.action().map(str::to_string),
                None => base.description.clone(),
            },
            owner: None,
            enabled,
            schedule: base.schedule.clone(),
            running: base.running,
            last_outcome: base.last_outcome,
            last_started: base.last_started.clone().or_else(|| self.last.clone()),
            last_duration_ms: base.last_duration_ms,
            next_run: base
                .next_run
                .clone()
                .or_else(|| self.next.clone())
                .filter(|_| enabled),
            can_run: number.is_some() && may && enabled && !base.running && !k.queue_off,
            can_stop: false,
            can_enable: may,
            has_history: k.runs_of(&self.name).is_some(),
            has_script: self.scriptable(),
            system: false,
        };
        if JobId::parse(&job.id).is_err() {
            job.can_run = false;
            job.can_enable = false;
            job.has_history = false;
            job.has_script = false;
        }
        job
    }
}

// ---------------------------------------------------------------------------
// The calls

pub(crate) async fn list(s: &PgSession, f: &Found) -> Result<Option<Part>> {
    let k = Kdb::read(s, f).await?;
    if !k.present() {
        return Ok(None);
    }
    let mut part = Part::default();
    let Some(sql) = jobs_sql(&k, None) else {
        part.notes.push(
            if k.cat.get("kdb_job").is_some() || k.cat.any("scheduler_jobs") {
                UNREADABLE
            } else {
                NOT_FOUND
            }
            .into(),
        );
        return Ok(Some(part));
    };
    let rows = s.text(&sql).await?;
    part.jobs = rows
        .iter()
        .filter_map(|r| KJob::from_row(r))
        .map(|j| j.server_job(&k))
        .collect();
    part.notes.extend(k.views_note().map(String::from));
    if part.jobs.iter().any(|j| j.kind == scheduler::KIND) {
        part.notes.push(scheduler::RUN_WAITS.into());
    }
    if k.queue_off {
        part.notes.push(QUEUE_OFF.into());
    }
    Ok(Some(part))
}

pub(crate) async fn ready(s: &PgSession) -> Result<Kdb> {
    let f = Found::detect(s).await?;
    let k = Kdb::read(s, &f).await?;
    if !k.present() {
        return Err(Error::Query(
            "esta base no tiene el plugin kdb_schedule, que guarda los jobs de KingbaseES".into(),
        ));
    }
    Ok(k)
}

pub(crate) async fn row(s: &PgSession, k: &Kdb, id: &Named) -> Result<KJob> {
    let sql = jobs_sql(k, Some(&id.name)).ok_or_else(|| Error::Query(UNREADABLE.into()))?;
    s.text(&sql)
        .await?
        .iter()
        .filter_map(|r| KJob::from_row(r))
        .find(|j| j.name == id.name)
        .ok_or_else(|| not_found(JobId::Kingbase(id.clone())))
}

pub(crate) async fn program(s: &PgSession, k: &Kdb, j: &SchedJob) -> Option<Program> {
    let (v, _) = k.cat.widest("scheduler_programs")?;
    let sql = scheduler::program_sql(D, v, j.program.as_deref()?)?;
    match s.text(&sql).await {
        Ok(rows) => rows.iter().find_map(|r| Program::from_row(r)),
        Err(e) => {
            tracing::debug!("kdb_schedule: program of {}: {e}", j.name);
            None
        }
    }
}

pub(crate) async fn detail(s: &PgSession, id: &Named) -> Result<JobDetail> {
    let k = ready(s).await?;
    let j = row(s, &k, id).await?;
    let mut properties = Vec::new();
    if let Some(n) = j.id.or(j.number()) {
        properties.push(prop("", "Id", n.to_string()));
    }
    properties.push(prop("", "Nombre", j.name.clone()));
    let steps = match (j.number(), &j.view) {
        (Some(_), _) => j
            .action()
            .map(|a| {
                vec![JobStep {
                    name: "Comando".into(),
                    kind: Some("PL/SQL".into()),
                    target: None,
                    command: Some(a.to_string()),
                }]
            })
            .unwrap_or_default(),
        (None, Some(v)) => v.steps(program(s, &k, v).await.as_ref()),
        (None, None) => Vec::new(),
    };
    if let Some(v) = &j.view {
        properties.extend(v.properties());
    }
    Ok(JobDetail {
        job: j.server_job(&k),
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub(crate) async fn runs(s: &PgSession, id: &Named, limit: u32) -> Result<Vec<JobRun>> {
    let k = ready(s).await?;
    let j = row(s, &k, id).await?;
    let sql = k
        .runs_of(&j.name)
        .and_then(|v| scheduler::runs_sql(D, v, None, &j.name, limit))
        .ok_or_else(|| {
            Error::Unsupported(
                "no se puede leer el historial de kdb_schedule (kdb_schedule.dba_scheduler_job_run_details) con este usuario"
                    .into(),
            )
        })?;
    Ok(s.text(&sql)
        .await?
        .iter()
        .map(|r| scheduler::run_of(r))
        .collect())
}

/// DBMS_JOB job `n` due now.
fn run_sql(n: i64) -> String {
    D.call(&format!("DBMS_JOB.NEXT_DATE({n}, now())"))
}

/// A DBMS_JOB job is made due now. A DBMS_SCHEDULER job never runs here
/// (see [`scheduler::RUN_WAITS`]); an unknown id still says so first.
pub(crate) async fn run(s: &PgSession, id: &Named) -> Result<()> {
    let k = ready(s).await?;
    let j = row(s, &k, id).await?;
    let Some(n) = j.number() else {
        return Err(scheduler::run_waits());
    };
    let n = j.checked(n)?;
    if !j.enabled() {
        return Err(Error::Query(
            "el job está deshabilitado: KingbaseES no lo ejecuta hasta habilitarlo".into(),
        ));
    }
    if k.queue_off {
        return Err(Error::Query(QUEUE_OFF.trim_end_matches('.').into()));
    }
    s.text(&run_sql(n)).await?;
    Ok(())
}

/// DBMS_JOB's `BROKEN` (fixed, its next date kept when it's still ahead,
/// else now, `BROKEN`'s default) or DBMS_SCHEDULER's `ENABLE` / `DISABLE`.
fn enable_sql(j: &KJob, enabled: bool) -> Result<String> {
    Ok(match (j.number(), enabled) {
        (Some(n), false) => D.call(&format!("DBMS_JOB.BROKEN({}, TRUE)", j.checked(n)?)),
        (Some(n), true) => D.call(&format!(
            "DBMS_JOB.BROKEN({}, FALSE, {})",
            j.checked(n)?,
            j.next_ahead()
                .map(|t| D.time_lit(&t))
                .unwrap_or_else(|| "now()".into())
        )),
        (None, _) => scheduler::enable_sql(D, &j.name, enabled),
    })
}

pub(crate) async fn set_enabled(s: &PgSession, id: &Named, enabled: bool) -> Result<()> {
    let k = ready(s).await?;
    let j = row(s, &k, id).await?;
    s.text(&enable_sql(&j, enabled)?).await?;
    Ok(())
}

/// `DBMS_JOB.SUBMIT` again, then `BROKEN` as it is (a new job starts
/// broken in some versions). Its connection string isn't copied.
fn dbms_job_script(j: &KJob, n: i64) -> Option<String> {
    let what = j.action()?;
    let next = j
        .next_ahead()
        .map(|t| D.time_lit(&t))
        .unwrap_or_else(|| "now()".into());
    let interval = j
        .view
        .as_ref()
        .and_then(|v| v.interval.as_deref())
        .map(|i| D.lit(i))
        .unwrap_or_else(|| "NULL".into());
    let broken = if j.enabled() {
        format!("DBMS_JOB.BROKEN(j, FALSE, {next})")
    } else {
        "DBMS_JOB.BROKEN(j, TRUE)".into()
    };
    Some(format!(
        "-- KingbaseES: Job {n} (DBMS_JOB)\n\
         -- DBMS_JOB le da un número nuevo. Si el job usaba DBMS_JOB.INSTANCE, volvé a darle su cadena de conexión: DBine no la copia.\n\
         DECLARE\n  j INT;\nBEGIN\n  DBMS_JOB.SUBMIT(j, {}, {next}, {interval});\n  {broken};\nEND;\n",
        D.lit(what)
    ))
}

pub(crate) async fn script(s: &PgSession, id: &Named) -> Result<String> {
    let k = ready(s).await?;
    let j = row(s, &k, id).await?;
    let unsupported = || {
        Error::Unsupported(
            "no se puede generar el script de este job: KingbaseES no muestra su acción o su programación"
                .into(),
        )
    };
    match (j.number(), &j.view) {
        (Some(n), _) => dbms_job_script(&j, n).ok_or_else(unsupported),
        (None, Some(v)) => {
            let program = program(s, &k, v).await;
            scheduler::kingbase_script(v, program.as_ref(), &k.database).ok_or_else(unsupported)
        }
        (None, None) => Err(unsupported()),
    }
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::super::scheduler::Kind;
    use super::*;

    fn kdb(superuser: bool) -> Kdb {
        Kdb {
            cat: Catalog::with(vec![
                View::fake(
                    "pg_catalog",
                    "kdb_job",
                    &[
                        ("jobid", Kind::Other),
                        ("jobname", Kind::Other),
                        ("jobenabled", Kind::Other),
                        ("joblastrun", Kind::TimeTz),
                        ("jobnextrun", Kind::TimeTz),
                    ],
                ),
                View::fake(
                    "kdb_schedule",
                    "dba_scheduler_jobs",
                    &[
                        ("job_name", Kind::Other),
                        ("job_type", Kind::Other),
                        ("job_action", Kind::Other),
                        ("program_name", Kind::Other),
                        ("schedule_name", Kind::Other),
                        ("start_date", Kind::Time),
                        ("repeat_interval", Kind::Other),
                        ("enabled", Kind::Other),
                        ("state", Kind::Other),
                        ("last_start_date", Kind::Time),
                        ("next_run_date", Kind::Time),
                        ("destination", Kind::Other),
                    ],
                ),
                View::fake(
                    "kdb_schedule",
                    "dba_scheduler_job_run_details",
                    &[("job_name", Kind::Other), ("log_id", Kind::Other)],
                ),
            ]),
            superuser,
            database: "test".into(),
            extension: true,
            queue_off: false,
            own: HashSet::from(["job1".to_string()]),
        }
    }

    #[test]
    fn dbms_job_numbers_come_from_their_names() {
        assert_eq!(
            dbms_job_number("internal_job1:2021-06-22 08:11:39.797681+08"),
            Some(1)
        );
        assert_eq!(dbms_job_number("internal_job42:x"), Some(42));
        for name in [
            "job1",
            "internal_job:x",
            "internal_job0:x",
            "internal_job1",
            "internal_job+1:x",
            "internal_job１:x",
            "xinternal_job1:x",
        ] {
            assert_eq!(dbms_job_number(name), None, "{name}");
        }
    }

    #[test]
    fn the_list_joins_kdb_job_and_the_views_latest_row() {
        let k = kdb(true);
        let sql = jobs_sql(&k, None).unwrap();
        assert!(sql.starts_with("SELECT k.\"jobid\"::text AS k_id"), "{sql}");
        assert!(sql.contains("FROM \"pg_catalog\".\"kdb_job\" k LEFT JOIN LATERAL (SELECT DISTINCT ON (v.\"job_name\")"));
        assert!(sql.contains("WHERE v.\"job_name\" = k.\"jobname\""));
        assert!(sql.contains("v.\"last_start_date\" DESC NULLS LAST LIMIT 1) j ON true"));
        assert!(sql.ends_with("ORDER BY k.\"jobid\""));
        // A connection string can carry a password: it's never read.
        assert!(!sql.contains("destination"));
        let one = jobs_sql(&k, Some("it's")).unwrap();
        assert!(one.contains("WHERE k.\"jobname\" = E'it''s' ORDER BY"));
        // Without kdb_job, the view alone.
        let mut views = kdb(true);
        views.cat = Catalog::with(vec![View::fake(
            "kdb_schedule",
            "all_scheduler_jobs",
            &[("job_name", Kind::Other)],
        )]);
        let sql = jobs_sql(&views, Some("job1")).unwrap();
        assert!(sql.contains("NULL::text AS k_id"));
        assert!(
            sql.contains(
                "FROM \"kdb_schedule\".\"all_scheduler_jobs\" v  WHERE v.\"job_name\" = E'job1'"
            ),
            "{sql}"
        );
        // Without the views, kdb_job alone.
        let mut bare = kdb(true);
        bare.cat = Catalog::with(vec![View::fake(
            "kdb_schedule",
            "kdb_job",
            &[("jobid", Kind::Other), ("jobname", Kind::Other)],
        )]);
        let sql = jobs_sql(&bare, None).unwrap();
        assert!(sql.contains("NULL::text AS v_job_name") && !sql.contains("LATERAL"));
        assert!(sql.contains("FROM \"kdb_schedule\".\"kdb_job\" k ORDER BY"));
        let mut nothing = kdb(true);
        nothing.cat = Catalog::default();
        assert!(jobs_sql(&nothing, None).is_none());
    }

    fn dbms_job_row(enabled: &str) -> super::super::Fixture {
        fixture(&[
            ("k_id", "1"),
            ("k_name", "internal_job1:2021-06-22 08:11:39.797681+08"),
            ("k_enabled", enabled),
            ("k_last", "1794488199"),
            ("k_next", "1794533465"),
            ("k_next_iso", "2026-11-13T09:31:05+08:00"),
            ("now", "1794488200"),
            ("v_job_name", "internal_job1:2021-06-22 08:11:39.797681+08"),
            ("v_job_action", "call p_test()"),
            ("v_repeat_interval", "Freq=Minutely;Interval=1"),
            ("v_state", "success"),
            ("v_last_start_date", "1794488199"),
        ])
    }

    #[test]
    fn a_dbms_job_job_maps_with_its_number() {
        let k = kdb(true);
        let j = KJob::from_row(&dbms_job_row("t")).unwrap();
        assert_eq!(j.number(), Some(1));
        let s = j.server_job(&k);
        assert_eq!(s.name, "Job 1");
        assert_eq!(s.kind, "DBMS_JOB");
        assert_eq!(s.description.as_deref(), Some("call p_test()"));
        assert_eq!(s.schedule.as_deref(), Some("Freq=Minutely;Interval=1"));
        assert_eq!(s.last_outcome, Some(dbine_driver::JobOutcome::Succeeded));
        assert_eq!(s.next_run.as_deref(), Some("2026-11-13T01:31:05Z"));
        assert!(s.enabled && s.can_run && s.can_enable && s.has_history && s.has_script);
        assert!(!s.can_stop && s.owner.is_none());
        assert!(JobId::parse(&s.id).is_ok());
        assert_eq!(run_sql(1), "CALL DBMS_JOB.NEXT_DATE(1, now())");
        assert_eq!(
            enable_sql(&j, true).unwrap(),
            "CALL DBMS_JOB.BROKEN(1, FALSE, CAST(E'2026-11-13T09:31:05+08:00' AS TIMESTAMP WITH TIME ZONE))"
        );
        assert_eq!(
            enable_sql(&j, false).unwrap(),
            "CALL DBMS_JOB.BROKEN(1, TRUE)"
        );
        let script = dbms_job_script(&j, 1).unwrap();
        assert!(script.contains("DBMS_JOB.SUBMIT(j, E'call p_test()', CAST(E'2026-11-13T09:31:05+08:00' AS TIMESTAMP WITH TIME ZONE), E'Freq=Minutely;Interval=1');"), "{script}");
        assert!(script.contains("DBMS_JOB.BROKEN(j, FALSE, CAST("));
        assert!(script.contains("DBMS_JOB.INSTANCE"));
        let off = KJob::from_row(&dbms_job_row("f")).unwrap();
        let s = off.server_job(&k);
        assert!(!s.enabled && !s.can_run && s.next_run.is_none());
        assert!(dbms_job_script(&off, 1)
            .unwrap()
            .contains("DBMS_JOB.BROKEN(j, TRUE);"));
        // A next date already past isn't kept: BROKEN's default, now.
        let past = KJob {
            now: Some(2e9),
            ..j.clone()
        };
        assert_eq!(
            enable_sql(&past, true).unwrap(),
            "CALL DBMS_JOB.BROKEN(1, FALSE, now())"
        );
        // kdb_job.jobid says another number than the name: which one
        // DBMS_JOB takes can't be told, so nothing is called with either.
        let odd = KJob {
            id: Some(5),
            ..j.clone()
        };
        assert_eq!(odd.number(), Some(1));
        let s = odd.server_job(&k);
        assert!(!s.can_run && !s.can_enable && s.has_history && s.has_script);
        assert_eq!(s.name, "Job 1");
        assert!(matches!(
            enable_sql(&odd, false),
            Err(Error::Unsupported(_))
        ));
        assert!(odd.checked(1).is_err() && j.checked(1).is_ok());
        // Without a jobid (the views alone), the name's number.
        assert_eq!(KJob { id: None, ..j }.checked(1).unwrap(), 1);
    }

    fn scheduler_row() -> super::super::Fixture {
        fixture(&[
            ("k_id", "7"),
            ("k_name", "job1"),
            ("k_enabled", "true"),
            ("v_job_name", "job1"),
            ("v_job_type", "SQL"),
            ("v_job_action", "create table t2(A int)"),
            ("v_program_name", "program1"),
            ("v_schedule_name", "schedule1"),
            ("v_repeat_interval", "Freq=daily;BYHOUR=10"),
            ("v_start_iso", "2026-11-01T10:00:00+08:00"),
            ("v_state", "running"),
            ("v_comments", "this is test job"),
        ])
    }

    #[test]
    fn a_scheduler_job_maps_by_name() {
        let j = KJob::from_row(&scheduler_row()).unwrap();
        assert_eq!(j.number(), None);
        let s = j.server_job(&kdb(false));
        assert_eq!(s.id, "kdb_schedule:\"job1\"");
        assert_eq!(s.name, "job1");
        assert_eq!(s.kind, "DBMS_SCHEDULER");
        assert_eq!(s.description.as_deref(), Some("this is test job"));
        assert_eq!(s.last_outcome, Some(dbine_driver::JobOutcome::Running));
        // Its own (user_scheduler_jobs).
        assert!(s.running && s.can_enable && s.has_script && s.has_history);
        assert_eq!(
            enable_sql(&j, false).unwrap(),
            "CALL DBMS_SCHEDULER.DISABLE(E'job1')"
        );
        // Someone else's, for a user who isn't a superuser.
        let other = KJob {
            name: "theirs".into(),
            ..j.clone()
        };
        let s = other.server_job(&kdb(false));
        assert!(!s.can_enable && !s.can_run);
        // Idle, enabled, a superuser's: still no "Ejecutar ahora", since
        // RUN_JOB would hold DBine's session until the run ends.
        let idle = KJob {
            view: j.view.clone().map(|v| SchedJob {
                state: Some("success".into()),
                ..v
            }),
            ..j
        };
        let s = idle.server_job(&kdb(true));
        assert!(s.enabled && !s.running && !s.can_run && s.can_enable);
    }

    #[test]
    fn the_scheduler_being_off_withholds_run_now() {
        let off = Kdb {
            queue_off: true,
            ..kdb(true)
        };
        let j = KJob::from_row(&dbms_job_row("t")).unwrap();
        assert!(j.server_job(&kdb(true)).can_run);
        assert!(!j.server_job(&off).can_run);
    }

    #[test]
    fn user_run_details_only_have_the_users_own() {
        let mut k = kdb(true);
        k.cat = Catalog::with(vec![View::fake(
            "kdb_schedule",
            "user_scheduler_job_run_details",
            &[("job_name", Kind::Other)],
        )]);
        assert!(k.runs_of("job1").is_some() && k.runs_of("theirs").is_none());
        let theirs = KJob {
            name: "theirs".into(),
            ..KJob::from_row(&scheduler_row()).unwrap()
        };
        // A superuser may change it, but its runs aren't in sight.
        let s = theirs.server_job(&k);
        assert!(s.can_enable && !s.has_history);
        // DBA_ and ALL_ have everyone's.
        assert!(kdb(false).runs_of("theirs").is_some());
    }

    #[test]
    fn the_list_says_why_a_job_shows_without_its_view() {
        let kdb_job = || {
            View::fake(
                "pg_catalog",
                "kdb_job",
                &[("jobid", Kind::Other), ("jobname", Kind::Other)],
            )
        };
        let with = |views: Vec<View>| Kdb {
            cat: Catalog::with(views),
            ..kdb(false)
        };
        assert_eq!(with(vec![kdb_job()]).views_note(), Some(NO_VIEWS));
        // The views are there, the user can't read them.
        let hidden = View::fake(
            "kdb_schedule",
            "dba_scheduler_jobs",
            &[("job_name", Kind::Other)],
        )
        .hidden();
        assert_eq!(
            with(vec![kdb_job(), hidden]).views_note(),
            Some(VIEWS_UNREADABLE)
        );
        // Without kdb_job, from ALL_: only what the user may see.
        let all = View::fake(
            "kdb_schedule",
            "all_scheduler_jobs",
            &[("job_name", Kind::Other)],
        );
        assert_eq!(with(vec![all]).views_note(), Some(PARTIAL));
        assert_eq!(kdb(true).views_note(), None);
    }

    #[test]
    fn a_job_without_its_view_row_still_lists() {
        let r = fixture(&[
            ("k_id", "3"),
            ("k_name", "job3"),
            ("k_enabled", "f"),
            ("k_last", "1794488199"),
        ]);
        let j = KJob::from_row(&r).unwrap();
        let s = j.server_job(&kdb(true));
        assert!(!s.enabled && !s.has_script);
        assert_eq!(s.last_started.as_deref(), Some("2026-11-12T12:56:39Z"));
    }
}
