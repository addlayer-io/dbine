//! The scheduler designer ([`dbine_driver::Session::scheduler_info`] and the
//! rest) on PostgreSQL and the engines that keep its catalog: each
//! scheduler a database has, managed whole.
//!
//! - pg_cron (PostgreSQL and its managed services, YugabyteDB, Greenplum
//!   and its forks where it's built, Fujitsu Enterprise Postgres…) and
//!   Apache Cloudberry's tasks (`CREATE TASK`, pg_cron built into the
//!   server): [`cron`].
//! - pgAgent: jobs with their steps, schedules and exceptions, and job
//!   classes: [`pgagent`].
//! - TimescaleDB: user-defined jobs and its policies (compression,
//!   retention, continuous aggregate refresh, reorder, compaction):
//!   [`timescale`].
//!
//! A database may have several at once. The "job" kind is every job
//! [`crate::jobs::list`] lists, with the same ids; a new job's form starts
//! with the scheduler to create it in when more than one is here.
//!
//! - Scripts: SQL in one transaction (`BEGIN` … `COMMIT`, or one `DO`
//!   block), refused while the session has a transaction of its own open
//!   ([`run_script`]). An edit is the scheduler's own in-place call
//!   (`cron.alter_job`, `ALTER TASK`, `UPDATE pgagent.…`, `alter_job`)
//!   with only what changed. Where the engine can only drop and create the
//!   object again (renaming a task, a TimescaleDB job's procedure…), the
//!   script says so at its top, with what is lost, and so does the form.
//! - Values and ids come from the app as untrusted text: ids parse as
//!   [`JobId`] or numbers and are looked up on the server, names picked
//!   from lists are checked against the server's ([`pick`]), and text
//!   reaches a statement only as a literal ([`lit`], [`dollar`]) or a
//!   quoted identifier ([`ident`]).
//! - A pgAgent step's connection string never carries its password back to
//!   the form: the password is a secret field, masked in the script shown
//!   and written only by the save.
//!
//! - CockroachDB: backup and changefeed schedules, changefeeds, row-level
//!   TTL and the cluster's own schedules: [`crdb`], which takes the calls
//!   whole on that engine.
//! - DBMS_JOB jobs of openGauss (`pkg_service`), EDB Postgres Advanced
//!   Server and KingbaseES: [`dbms_job`].
//! - DBMS_SCHEDULER jobs, programs and schedules of EDB Postgres Advanced
//!   Server and KingbaseES: [`dbms_scheduler`].

mod crdb;
mod cron;
mod dbms_job;
mod dbms_scheduler;
mod pgagent;
mod timescale;

use crate::catalog::{comment_text, lit};
use crate::jobs::{self, dollar, ident, JobId, Named};
use crate::session::PgSession;
use crate::Variant;
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerAction,
    SchedulerInfo, SchedulerItem, SchedulerList, ServerJob, Session, TxState,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, HashMap};
use tokio_postgres::SimpleQueryMessage;

// ---------------------------------------------------------------------------
// Notes and messages
// ---------------------------------------------------------------------------

pub(crate) const OPEN_TRANSACTION: &str = "Hay una transacción abierta en esta sesión: confirmala (COMMIT) o deshacela (ROLLBACK) antes de cambiar el planificador.";
const NO_SOURCE: &str = "Esta base no tiene un planificador en el que tu usuario pueda crear jobs (pg_cron, pgAgent, TimescaleDB, las tareas del servidor, DBMS_JOB o DBMS_SCHEDULER).";

// ---------------------------------------------------------------------------
// Actions
// ---------------------------------------------------------------------------

/// Runs the object here, in DBine's session, until it ends (TimescaleDB's
/// `run_job`, `DBMS_JOB.RUN`, `DBMS_SCHEDULER.RUN_JOB`).
pub(crate) const RUN: &str = "run";
/// Moves the object's next run to now: its scheduler starts it on its next
/// pass and DBine doesn't wait (pgAgent, CockroachDB, openGauss).
pub(crate) const RUN_NOW: &str = "run_now";

pub(crate) fn run_action() -> SchedulerAction {
    SchedulerAction::new("run", "Ejecutar ahora")
}

pub(crate) fn run_now_action() -> SchedulerAction {
    SchedulerAction::new("run_now", "Programar para ahora")
}

/// An item's actions, as the keys it offers now.
pub(crate) fn offers(keys: &[&str]) -> Option<Vec<String>> {
    Some(keys.iter().map(|k| k.to_string()).collect())
}

/// An action this object doesn't have.
pub(crate) fn no_action(action: &str) -> Error {
    bad(format!(
        "«{}» no es una acción de este objeto del planificador.",
        shown(action)
    ))
}

pub(crate) fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
pub(crate) fn shown(s: &str) -> String {
    let t = s.trim();
    let mut out: String = comment_text(t).chars().take(60).collect();
    if t.chars().count() > 60 {
        out.push('…');
    }
    out
}

/// Variants whose sessions manage their scheduler here.
pub(crate) fn supported(v: Variant) -> bool {
    jobs::supported(v)
}

fn check(v: Variant) -> Result<()> {
    if let Some(why) = jobs::unsupported_reason(v) {
        return Err(Error::Unsupported(why.into()));
    }
    if !supported(v) {
        return Err(Error::Unsupported(
            "este motor no administra su planificador desde DBine".into(),
        ));
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Kinds and sources
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Every job of every scheduler here (the tree's jobs).
    Job,
    /// pgAgent's job classes.
    JobClass,
    /// One kind of TimescaleDB policy.
    Policy(timescale::Policy),
    /// CockroachDB's row-level TTL of a table.
    Ttl,
    /// DBMS_SCHEDULER's programs (EDB, KingbaseES).
    Program,
    /// DBMS_SCHEDULER's named schedules (EDB, KingbaseES).
    Schedule,
}

impl Kind {
    fn parse(kind: &str) -> Result<Kind> {
        Ok(match kind.trim() {
            "job" => Kind::Job,
            "job_class" => Kind::JobClass,
            "ttl" => Kind::Ttl,
            "program" => Kind::Program,
            "schedule" => Kind::Schedule,
            other => match timescale::Policy::from_key(other) {
                Some(p) => Kind::Policy(p),
                None => {
                    return Err(bad(format!(
                        "«{}» no es un tipo de objeto del planificador de este servidor",
                        shown(other)
                    )))
                }
            },
        })
    }
}

/// The schedulers a job can be created in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Source {
    Cron,
    Task,
    Agent,
    Timescale,
    /// CockroachDB: a backup schedule.
    CrdbBackup,
    /// CockroachDB: a changefeed schedule.
    CrdbChangefeed,
    /// CockroachDB: a changefeed.
    CrdbFeed,
    /// openGauss's DBMS_JOB (`pkg_service`).
    GaussJob,
    EdbJob,
    EdbScheduler,
    KdbJob,
    KdbScheduler,
}

impl Source {
    /// The key of the "Planificador" field: the prefix of its jobs' ids
    /// where the scheduler has one of its own.
    pub(crate) fn key(self) -> &'static str {
        match self {
            Source::Cron => "pg_cron",
            Source::Task => "task",
            Source::Agent => "pgagent",
            Source::Timescale => "timescaledb",
            Source::CrdbBackup => "crdb_backup",
            Source::CrdbChangefeed => "crdb_changefeed",
            Source::CrdbFeed => "crdb_job",
            Source::GaussJob => "dbms_job",
            Source::EdbJob => "edb_job",
            Source::EdbScheduler => "edb_scheduler",
            Source::KdbJob => "kdb_job",
            Source::KdbScheduler => "kdb_scheduler",
        }
    }

    const ALL: [Source; 12] = [
        Source::Cron,
        Source::Task,
        Source::Agent,
        Source::Timescale,
        Source::CrdbBackup,
        Source::CrdbChangefeed,
        Source::CrdbFeed,
        Source::GaussJob,
        Source::EdbJob,
        Source::EdbScheduler,
        Source::KdbJob,
        Source::KdbScheduler,
    ];

    fn parse(key: &str) -> Option<Source> {
        Source::ALL.into_iter().find(|s| s.key() == key.trim())
    }

    fn label(self) -> &'static str {
        match self {
            Source::Cron => "pg_cron",
            Source::Task => "Tareas del servidor (CREATE TASK)",
            Source::Agent => "pgAgent",
            Source::Timescale => "TimescaleDB",
            Source::CrdbBackup => "Backup programado",
            Source::CrdbChangefeed => "Changefeed programado",
            Source::CrdbFeed => "Changefeed",
            Source::GaussJob | Source::EdbJob | Source::KdbJob => "DBMS_JOB",
            Source::EdbScheduler | Source::KdbScheduler => "DBMS_SCHEDULER",
        }
    }
}

/// What the current database has, and what the user may do with it.
pub(crate) struct Ctx {
    pub(crate) v: Variant,
    pub(crate) found: jobs::Found,
    pub(crate) cron: Option<cron::Caps>,
    pub(crate) task: Option<cron::Caps>,
    pub(crate) agent: Option<pgagent::Caps>,
    pub(crate) ts: Option<timescale::Caps>,
    /// CockroachDB's schedules.
    pub(crate) crdb: Option<crdb::Caps>,
    /// DBMS_JOB (openGauss, EDB, KingbaseES).
    pub(crate) dbms: Option<dbms_job::Caps>,
    /// DBMS_SCHEDULER (EDB, KingbaseES).
    pub(crate) sched: Option<dbms_scheduler::Caps>,
    /// What the user can't do with a scheduler that's here.
    notes: Vec<String>,
}

impl Ctx {
    /// The schedulers a new job can go to, in the order the form offers
    /// them.
    pub(crate) fn job_sources(&self) -> Vec<Source> {
        let mut out = Vec::new();
        if self.cron.as_ref().is_some_and(|c| c.may_create()) {
            out.push(Source::Cron);
        }
        if self.task.as_ref().is_some_and(|c| c.may_create()) {
            out.push(Source::Task);
        }
        if self.agent.as_ref().is_some_and(|a| a.may_create()) {
            out.push(Source::Agent);
        }
        if self.ts.is_some() {
            out.push(Source::Timescale);
        }
        if let Some(x) = &self.crdb {
            out.extend(crdb::sources(x));
        }
        if let Some(d) = self.dbms.as_ref().filter(|d| d.readable) {
            out.push(match d.engine {
                dbms_job::Engine::Gauss => Source::GaussJob,
                dbms_job::Engine::Edb(_) => Source::EdbJob,
                dbms_job::Engine::Kdb(_) => Source::KdbJob,
            });
        }
        if self.sched.is_some() {
            out.push(if self.v == Variant::Edb {
                Source::EdbScheduler
            } else {
                Source::KdbScheduler
            });
        }
        out
    }

    fn any_job_edit(&self) -> bool {
        self.cron.as_ref().is_some_and(|c| c.may_edit())
            || self.task.as_ref().is_some_and(|c| c.may_edit())
            || self.agent.as_ref().is_some_and(|a| a.may_edit())
            || self.ts.is_some()
            || self.crdb.is_some()
            || self.dbms.as_ref().is_some_and(|d| d.readable)
            || self.sched.is_some()
    }

    fn any_job_delete(&self) -> bool {
        self.cron.as_ref().is_some_and(|c| c.may_delete())
            || self.task.as_ref().is_some_and(|c| c.may_delete())
            || self.agent.as_ref().is_some_and(|a| a.may_delete())
            || self.ts.is_some()
            || self.crdb.is_some()
            || self.dbms.as_ref().is_some_and(|d| d.readable)
            || self.sched.is_some()
    }

    /// The actions of the jobs here (pg_cron and the server's tasks have
    /// none: they run only on their schedule).
    fn job_actions(&self) -> Vec<SchedulerAction> {
        let run = self.ts.is_some()
            || self.sched.is_some()
            || self
                .dbms
                .as_ref()
                .is_some_and(|d| d.readable && d.runs_here());
        // openGauss: not while its job scheduler is off (nothing would take
        // the job).
        let run_now = self.agent.as_ref().is_some_and(|a| a.job_update)
            || self
                .dbms
                .as_ref()
                .is_some_and(|d| d.readable && !d.runs_here() && d.off.is_none());
        let mut out = Vec::new();
        if run {
            out.push(run_action());
        }
        if run_now {
            out.push(run_now_action());
        }
        out
    }

    /// Some job here keeps a run history the user may delete: pg_cron's
    /// (`cron.job_run_details`), pgAgent's (`pga_joblog`), TimescaleDB's
    /// (`bgw_job_stat_history` / `job_errors`), edb_job_scheduler's
    /// (`sys.job_run_details`). A job of another scheduler refuses with
    /// the reason ([`clear_history`]).
    fn job_history(&self) -> bool {
        self.cron.as_ref().is_some_and(|c| c.runs && c.clear)
            || self.agent.as_ref().is_some_and(|a| a.log_delete)
            || self
                .ts
                .as_ref()
                .is_some_and(|t| t.history.is_some() && t.clear)
            || self.dbms.as_ref().is_some_and(|d| d.edb_history())
    }
}

pub(crate) async fn ctx(s: &PgSession) -> Result<Ctx> {
    check(s.variant)?;
    if s.variant == Variant::Cockroach {
        let caps = crdb::caps(s).await?;
        return Ok(Ctx {
            v: s.variant,
            found: jobs::Found::default(),
            cron: None,
            task: None,
            agent: None,
            ts: None,
            notes: caps.notes(),
            crdb: Some(caps),
            dbms: None,
            sched: None,
        });
    }
    let found = jobs::Found::detect(s).await?;
    let mut c = Ctx {
        v: s.variant,
        cron: None,
        task: None,
        agent: None,
        ts: None,
        crdb: None,
        dbms: None,
        sched: None,
        notes: Vec::new(),
        found,
    };
    match s.variant {
        Variant::OpenGauss => {
            let d = dbms_job::gauss_caps(s, &c.found).await?;
            c.notes.extend(d.notes());
            c.dbms = Some(d);
        }
        Variant::Edb => {
            let (d, note) = dbms_job::edb_caps(s, &c.found).await?;
            c.notes.extend(note.clone());
            if let Some(d) = d {
                c.notes.extend(d.notes());
                c.dbms = Some(d);
            }
            if note.is_none() {
                c.sched = dbms_scheduler::caps(s, &c.found, jobs::scheduler::Dialect::Edb).await?;
            }
        }
        Variant::Kingbase => {
            if let Some(d) = dbms_job::kdb_caps(s, &c.found).await? {
                c.notes.extend(d.notes());
                c.dbms = Some(d);
            }
            c.sched = dbms_scheduler::caps(s, &c.found, jobs::scheduler::Dialect::Kingbase).await?;
        }
        _ => {}
    }
    if let Some(sc) = &c.sched {
        c.notes.extend(sc.notes());
    }
    if c.found.cron {
        let caps = cron::caps(s, &c.found, cron::Flavor::Cron).await?;
        c.notes.extend(caps.notes());
        c.cron = caps.readable.then_some(caps);
    }
    if c.found.task {
        let caps = cron::caps(s, &c.found, cron::Flavor::Task).await?;
        c.notes.extend(caps.notes());
        c.task = caps.readable.then_some(caps);
    }
    if c.found.pgagent {
        let caps = pgagent::caps(s).await?;
        c.notes.extend(caps.notes());
        c.agent = caps.read.then_some(caps);
    }
    if let Some(schema) = c.found.timescale.clone() {
        c.ts = Some(timescale::caps(s, &schema).await?);
    }
    Ok(c)
}

/// The kinds, in the order of the tree, and the notes.
fn info_of(c: &Ctx) -> SchedulerInfo {
    // None of these schedulers starts a job at one of its steps: pgAgent
    // runs every enabled step of a job, the rest have no steps.
    let mut job = dbine_driver::SchedulerKind::new("job", "Jobs", "job")
        .abilities(
            !c.job_sources().is_empty(),
            c.any_job_edit(),
            c.any_job_delete(),
        )
        .run_from_step(false)
        .clear_history(c.job_history());
    for a in c.job_actions() {
        job = job.action(a);
    }
    let mut kinds = vec![job];
    if let Some(a) = &c.agent {
        kinds.push(pgagent::class_kind(a));
    }
    if let Some(t) = &c.ts {
        for p in t.policies() {
            kinds.push(
                p.kind()
                    .action(run_action())
                    .clear_history(t.history.is_some() && t.clear),
            );
        }
    }
    if let Some(x) = &c.crdb {
        kinds = x.kinds();
    }
    if let Some(sc) = &c.sched {
        kinds.extend(sc.kinds());
    }
    let mut notes = c.notes.clone();
    notes.dedup();
    SchedulerInfo {
        kinds,
        logs: false,
        notes,
    }
}

/// Where a job id leads: one of the schedulers here.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Target {
    Cron(cron::Flavor, i64),
    Agent(i32),
    Timescale(i32),
    /// A CockroachDB schedule or changefeed.
    Crdb(crdb::Obj),
    /// A DBMS_JOB job (openGauss, EDB, KingbaseES).
    Dbms(JobId),
    /// A DBMS_SCHEDULER job (EDB, KingbaseES).
    Sched(Named),
}

/// A job id of this server, refused when its scheduler isn't here (or
/// isn't one this designer manages).
pub(crate) fn job_target(c: &Ctx, id: &str) -> Result<Target> {
    let missing = |what: &str| {
        Error::Query(format!(
            "{what} no está en esta base (o tu usuario no puede leerlo)"
        ))
    };
    match JobId::parse_for(c.v, id)? {
        JobId::Cron(n) => c
            .cron
            .as_ref()
            .map(|_| Target::Cron(cron::Flavor::Cron, n))
            .ok_or_else(|| missing("pg_cron")),
        JobId::Task(n) => c
            .task
            .as_ref()
            .map(|_| Target::Cron(cron::Flavor::Task, n))
            .ok_or_else(|| missing("pg_task")),
        JobId::Agent(n) => c
            .agent
            .as_ref()
            .map(|_| Target::Agent(n))
            .ok_or_else(|| missing("pgAgent")),
        JobId::Timescale(n) => {
            c.ts.as_ref()
                .map(|_| Target::Timescale(n))
                .ok_or_else(|| missing("TimescaleDB"))
        }
        JobId::CrdbSchedule(n) => Ok(Target::Crdb(crdb::Obj::Schedule(n))),
        JobId::CrdbJob(n) => Ok(Target::Crdb(crdb::Obj::Feed(n))),
        JobId::GaussJob(_) | JobId::EdbJob(_) => match &c.dbms {
            Some(_) => Ok(Target::Dbms(JobId::parse_for(c.v, id)?)),
            None => Err(missing("DBMS_JOB")),
        },
        JobId::EdbScheduler(n) => match &c.sched {
            Some(_) => Ok(Target::Sched(n)),
            None => Err(missing("DBMS_SCHEDULER")),
        },
        JobId::Kingbase(n) => {
            if jobs::kingbase::dbms_job_number(&n.name).is_some() {
                match &c.dbms {
                    Some(_) => Ok(Target::Dbms(JobId::Kingbase(n))),
                    None => Err(missing("DBMS_JOB")),
                }
            } else {
                match &c.sched {
                    Some(_) => Ok(Target::Sched(n)),
                    None => Err(missing("DBMS_SCHEDULER")),
                }
            }
        }
    }
}

/// What the user may do with one job of the list: edit it, delete it, and
/// which of the kind's actions it offers now.
pub(crate) type Abilities = (Option<bool>, Option<bool>, Option<Vec<String>>);

/// A job of the list as the tree shows it, with what the user may do with
/// it (another user's job, the extension's own, a cluster schedule…).
pub(crate) fn job_item(c: &Ctx, j: ServerJob) -> SchedulerItem {
    let (can_edit, can_delete, actions) = match job_target(c, &j.id) {
        Ok(t) => job_abilities(c, &t, &j),
        // Not a scheduler this designer manages here: shown only.
        Err(_) => (Some(false), Some(false), offers(&[])),
    };
    SchedulerItem {
        id: j.id,
        name: j.name,
        description: j.description,
        enabled: Some(j.enabled),
        system: j.system,
        can_edit,
        can_delete,
        actions,
        run_from_step: None,
        clear_history: None,
    }
}

pub(crate) fn job_abilities(c: &Ctx, t: &Target, j: &ServerJob) -> Abilities {
    let owner = j.owner.as_deref().unwrap_or_default();
    match t {
        Target::Cron(flavor, _) => {
            let caps = match flavor {
                cron::Flavor::Cron => c.cron.as_ref(),
                cron::Flavor::Task => c.task.as_ref(),
            };
            let Some(caps) = caps else {
                return (Some(false), Some(false), offers(&[]));
            };
            let mine = caps.may_change(owner);
            // pg_cron and the tasks run a job only on its schedule.
            (
                Some(mine && caps.may_edit()),
                Some(mine && caps.may_delete()),
                offers(&[]),
            )
        }
        Target::Agent(_) => {
            let a = c.agent.clone().unwrap_or_default();
            // A disabled job isn't taken, and a running one gets its next
            // run computed again when the run ends.
            let run_now = a.job_update && j.enabled && !j.running;
            (
                Some(a.may_edit()),
                Some(a.may_delete()),
                offers(if run_now { &[RUN_NOW] } else { &[] }),
            )
        }
        Target::Timescale(_) => {
            // `can_enable`: the job's owner's rights (a superuser's too).
            let mine = j.can_enable;
            (
                Some(mine),
                Some(mine && !j.system),
                offers(if mine && !j.running { &[RUN] } else { &[] }),
            )
        }
        Target::Dbms(id) => {
            let Some(d) = c.dbms.as_ref() else {
                return (Some(false), Some(false), offers(&[]));
            };
            // The scripts' rule: openGauss's `log_user` (the list also
            // counts `priv_user`, which pkg_service doesn't take as the
            // owner).
            let mine = d.may_job(id, j.owner.as_deref());
            let action = if d.runs_here() {
                (mine && !j.running).then_some(RUN)
            } else {
                // openGauss: enabled and idle (`can_run`), while its job
                // scheduler is on.
                (mine && j.can_run && d.off.is_none()).then_some(RUN_NOW)
            };
            (Some(mine), Some(mine), offers(action.as_slice()))
        }
        Target::Sched(_) => {
            // EDB: the user's own, with a name no other owner has;
            // KingbaseES: the user's own (or a superuser).
            let mine = j.can_enable;
            (
                Some(mine),
                Some(mine),
                offers(if mine && !j.running { &[RUN] } else { &[] }),
            )
        }
        Target::Crdb(obj) => match &c.crdb {
            Some(x) => crdb::job_abilities(x, *obj, j),
            None => (Some(false), Some(false), offers(&[])),
        },
    }
}

// ---------------------------------------------------------------------------
// The contract's calls
// ---------------------------------------------------------------------------

pub(crate) async fn info(s: &PgSession) -> Result<SchedulerInfo> {
    let c = ctx(s).await?;
    Ok(info_of(&c))
}

pub(crate) async fn items(s: &PgSession, kind: &str) -> Result<SchedulerList> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    match kind {
        Kind::Job => {
            let list = jobs::list(s).await?;
            Ok(SchedulerList {
                items: list.jobs.into_iter().map(|j| job_item(&c, j)).collect(),
                notes: list.notes,
            })
        }
        Kind::JobClass => pgagent::class_items(s, &c).await,
        Kind::Policy(p) => timescale::policy_items(s, &c, p).await,
        Kind::Ttl => crdb::ttl_items(s, crdb_caps(&c)?).await,
        Kind::Program => dbms_scheduler::items(s, sched_caps(&c)?, "program").await,
        Kind::Schedule => dbms_scheduler::items(s, sched_caps(&c)?, "schedule").await,
    }
}

fn crdb_caps(c: &Ctx) -> Result<&crdb::Caps> {
    c.crdb
        .as_ref()
        .ok_or_else(|| bad("Este servidor no tiene TTL por filas (es de CockroachDB).".into()))
}

fn dbms_caps(c: &Ctx) -> Result<&dbms_job::Caps> {
    c.dbms
        .as_ref()
        .ok_or_else(|| bad("Esta base no tiene DBMS_JOB.".into()))
}

fn sched_caps(c: &Ctx) -> Result<&dbms_scheduler::Caps> {
    c.sched.as_ref().ok_or_else(|| {
        bad("Esta base no tiene DBMS_SCHEDULER (o tu usuario no puede leer sus vistas).".into())
    })
}

/// The prefix of a DBMS_SCHEDULER job's id here.
fn sched_prefix(c: &Ctx) -> &'static str {
    if c.v == Variant::Edb {
        "edb_scheduler:"
    } else {
        "kdb_schedule:"
    }
}

/// CockroachDB's object for a kind and an id.
fn crdb_obj(c: &Ctx, kind: Kind, id: Option<&str>) -> Result<crdb::Obj> {
    match (kind, id) {
        (Kind::Job, None) => Err(bad("Elegí el tipo de schedule.".into())),
        (Kind::Job, Some(id)) => match job_target(c, id)? {
            Target::Crdb(o) => Ok(o),
            _ => Err(bad(format!(
                "«{}» no es un id de job de este servidor",
                shown(id)
            ))),
        },
        (Kind::Ttl, id) => crdb::ttl_obj(id),
        _ => Err(bad("Este tipo de objeto no es de CockroachDB.".into())),
    }
}

/// CockroachDB's object for a new "job": the source picked.
fn crdb_new(c: &Ctx, values: &FormValues) -> Result<crdb::Obj> {
    let sources = c.job_sources();
    let picked = Vals(&values.values).opt("source");
    let source = match picked.as_deref() {
        Some(k) => Source::parse(k)
            .filter(|s| sources.contains(s))
            .ok_or_else(|| {
                bad(format!(
                    "«{}» no es un planificador en el que se pueda crear un job aquí.",
                    shown(k)
                ))
            })?,
        None => *sources
            .first()
            .ok_or_else(|| Error::Unsupported(NO_SOURCE.into()))?,
    };
    Ok(crdb::Obj::New(source))
}

pub(crate) async fn form(s: &PgSession, kind: &str, id: Option<&str>) -> Result<ObjectForm> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    if let Some(x) = &c.crdb {
        return match (kind, id) {
            (Kind::Job, None) => crdb::new_job_form(s, x).await,
            _ => crdb::form(s, x, crdb_obj(&c, kind, id)?).await,
        };
    }
    match (kind, id) {
        (Kind::Job, None) => new_job_form(s, &c).await,
        (Kind::Job, Some(id)) => match job_target(&c, id)? {
            Target::Cron(flavor, n) => cron::form(s, &c, flavor, Some(n)).await,
            Target::Agent(n) => pgagent::form(s, &c, Some(n)).await,
            Target::Timescale(n) => timescale::job_form(s, &c, n).await,
            Target::Dbms(job) => dbms_job::form(s, dbms_caps(&c)?, Some(&job)).await,
            Target::Sched(n) => dbms_scheduler::job_form(s, sched_caps(&c)?, Some(&n)).await,
            Target::Crdb(_) => Err(bad("Este id es de CockroachDB.".into())),
        },
        (Kind::JobClass, id) => pgagent::class_form(s, &c, id).await,
        (Kind::Policy(p), id) => timescale::policy_form(s, &c, p, id).await,
        (Kind::Program, id) => dbms_scheduler::program_form(s, sched_caps(&c)?, id).await,
        (Kind::Schedule, id) => dbms_scheduler::schedule_form(s, sched_caps(&c)?, id).await,
        (Kind::Ttl, _) => Err(bad(
            "Este servidor no tiene TTL por filas (es de CockroachDB).".into(),
        )),
    }
}

/// A new job's form: the one scheduler's, or every scheduler's fields after
/// a "Planificador" field that shows each one's.
async fn new_job_form(s: &PgSession, c: &Ctx) -> Result<ObjectForm> {
    let sources = c.job_sources();
    let mut forms = Vec::new();
    for source in &sources {
        let f = match source {
            Source::Cron => cron::form(s, c, cron::Flavor::Cron, None).await?,
            Source::Task => cron::form(s, c, cron::Flavor::Task, None).await?,
            Source::Agent => pgagent::form(s, c, None).await?,
            Source::Timescale => timescale::new_job_form(s, c).await?,
            Source::GaussJob | Source::EdbJob | Source::KdbJob => {
                dbms_job::form(s, dbms_caps(c)?, None).await?
            }
            Source::EdbScheduler | Source::KdbScheduler => {
                dbms_scheduler::job_form(s, sched_caps(c)?, None).await?
            }
            Source::CrdbBackup | Source::CrdbChangefeed | Source::CrdbFeed => continue,
        };
        forms.push((*source, f));
    }
    match forms.len() {
        0 => Err(Error::Unsupported(NO_SOURCE.into())),
        1 => {
            let (source, mut f) = forms.remove(0);
            f.values.insert("source".into(), json!(source.key()));
            Ok(f)
        }
        _ => Ok(combine(forms)),
    }
}

/// Between a scheduler's key and a field's key in the combined form: the
/// field of that scheduler that differs from another's with the same key.
const OWN: char = '.';

/// The key a combined form gives `source`'s own `key`.
fn own_key(source: Source, key: &str) -> String {
    format!("{}{OWN}{key}", source.key())
}

/// How a scheduler's form has a key: its fields with that key and the
/// value it starts with.
type Shape = (Vec<FormField>, Option<Value>);

fn shape_of(f: &ObjectForm, key: &str) -> Shape {
    (
        f.sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .filter(|x| x.key == key)
            .cloned()
            .collect(),
        f.values.get(key).cloned(),
    )
}

/// A field's conditions: its `when` and the rest (`when_all`).
fn conditions(field: &mut FormField) -> impl Iterator<Item = &mut (String, Vec<String>)> {
    field.when.iter_mut().chain(field.when_all.iter_mut())
}

/// `f`'s keys in `renamed` as [`own_key`]s: its fields, the conditions that
/// name them (its rows', lists' and tabs' too) and its values.
fn rename_keys(source: Source, f: &mut ObjectForm, renamed: &[String]) {
    if renamed.is_empty() {
        return;
    }
    let to = |k: &str| own_key(source, k);
    let hit = |k: &str| renamed.iter().any(|r| r == k);
    for section in &mut f.sections {
        if let Some((k, _)) = section.when.as_mut().filter(|(k, _)| hit(k)) {
            *k = to(k);
        }
        for field in &mut section.fields {
            if hit(&field.key) {
                field.key = to(&field.key);
            }
            for (k, _) in conditions(field).filter(|(k, _)| hit(k)) {
                *k = to(k);
            }
        }
        for list in &mut section.lists {
            if let Some((k, _)) = list.when.as_mut().filter(|(k, _)| hit(k)) {
                *k = to(k);
            }
            let row_keys: Vec<String> = list.fields.iter().map(|x| x.key.clone()).collect();
            for field in &mut list.fields {
                // A row's condition names the top level only when the row
                // has no field of that key.
                for (k, _) in conditions(field).filter(|(k, _)| hit(k) && !row_keys.contains(k)) {
                    *k = to(k);
                }
            }
        }
    }
    // Each own key goes with a value (its field's default, else null): the
    // designer sends back the keys it has, and without it the first
    // scheduler's value of the plain key would stand in for this one's.
    for k in renamed {
        let v = f.values.remove(k).unwrap_or_else(|| {
            f.sections
                .iter()
                .flat_map(|s| s.fields.iter())
                .find_map(|x| (x.key == to(k)).then(|| x.default.clone()).flatten())
                .unwrap_or(Value::Null)
        });
        f.values.insert(to(k), v);
    }
}

/// A field's, list's or tab's condition: a key and the values it shows for.
type Cond = (String, Vec<String>);

/// The schedulers (by key) whose forms have each tab and each list.
type Holders = (
    HashMap<String, Vec<&'static str>>,
    HashMap<String, Vec<&'static str>>,
);

fn holders(forms: &[(Source, ObjectForm)]) -> Holders {
    let (mut tabs, mut lists): Holders = Default::default();
    for (source, f) in forms {
        for section in &f.sections {
            let who = tabs.entry(section.key.clone()).or_default();
            if !who.contains(&source.key()) {
                who.push(source.key());
            }
            for list in &section.lists {
                let who = lists.entry(list.key.clone()).or_default();
                if !who.contains(&source.key()) {
                    who.push(source.key());
                }
            }
        }
    }
    (tabs, lists)
}

/// The forms of several schedulers as one: a "Planificador" select first,
/// each scheduler's fields, lists and tabs shown only while it's the one
/// picked (what every scheduler has stays always shown; a field with
/// conditions of its own keeps them and adds this one). A key two
/// schedulers use for fields that differ (kind, read-only, help, starting
/// value…) stays the first one's; the others' fields with it get
/// [`own_key`]s, and [`picked_values`] gives them their keys back for the
/// scheduler picked.
pub(crate) fn combine(forms: Vec<(Source, ObjectForm)>) -> ObjectForm {
    let all: Vec<Source> = forms.iter().map(|(s, _)| *s).collect();
    let options: Vec<(String, String)> = all
        .iter()
        .map(|s| (s.key().to_string(), s.label().to_string()))
        .collect();
    let picker = FormField::new("source", "Planificador", FormKind::Select { options })
        .required()
        .default(json!(all[0].key()));
    let (tab_holders, list_holders) = holders(&forms);
    // Each key as its first scheduler has it; the schedulers that have it
    // the same way share it, the rest get their own.
    let mut first: HashMap<String, Shape> = HashMap::new();
    let mut owners: HashMap<String, Vec<&'static str>> = HashMap::new();
    let mut forms = forms;
    for (source, f) in &mut forms {
        let mut keys: Vec<String> = f
            .sections
            .iter()
            .flat_map(|s| s.fields.iter())
            .map(|x| x.key.clone())
            .chain(f.values.keys().cloned())
            .collect();
        keys.sort();
        keys.dedup();
        let mut renamed = Vec::new();
        for k in keys {
            let shape = shape_of(f, &k);
            match first.get(&k) {
                None => {
                    first.insert(k.clone(), shape);
                    owners.entry(k).or_default().push(source.key());
                }
                Some(s) if *s == shape => owners.entry(k).or_default().push(source.key()),
                Some(_) => {
                    owners
                        .entry(own_key(*source, &k))
                        .or_default()
                        .push(source.key());
                    renamed.push(k);
                }
            }
        }
        rename_keys(*source, f, &renamed);
    }
    // Each tab's own conditions, one per scheduler that has it (after the
    // renames), and the keys that have a field.
    let mut tab_whens: HashMap<String, Vec<Option<Cond>>> = HashMap::new();
    let mut field_keys: Vec<String> = Vec::new();
    for (_, f) in &forms {
        for section in &f.sections {
            tab_whens
                .entry(section.key.clone())
                .or_default()
                .push(section.when.clone());
            field_keys.extend(section.fields.iter().map(|x| x.key.clone()));
        }
    }
    let mut out = ObjectForm::default();
    out.values.insert("source".into(), json!(all[0].key()));
    // The scheduler whose fields of each key are in the form.
    let mut seen: HashMap<String, usize> = HashMap::new();
    for (i, (_, f)) in forms.into_iter().enumerate() {
        for section in f.sections {
            let idx = match out.sections.iter().position(|x| x.key == section.key) {
                Some(i) => i,
                None => {
                    out.sections.push(FormSection {
                        fields: Vec::new(),
                        lists: Vec::new(),
                        ..section.clone()
                    });
                    out.sections.len() - 1
                }
            };
            for mut field in section.fields {
                // A shared key's fields come from its first scheduler (all
                // of them: one form may have several under exclusive
                // `when`s).
                if *seen.entry(field.key.clone()).or_insert(i) != i {
                    continue;
                }
                let who = owners.get(&field.key).cloned().unwrap_or_default();
                if who.len() < all.len() {
                    field = field.and_when("source", &who);
                }
                out.sections[idx].fields.push(field);
            }
            for mut list in section.lists {
                if out.sections[idx].lists.iter().any(|l| l.key == list.key) {
                    continue;
                }
                let who = list_holders.get(&list.key).cloned().unwrap_or_default();
                // A list takes one condition: the scheduler's comes first
                // (another scheduler's rows would be refused anyway).
                if who.len() < all.len() {
                    list = list.when("source", &who);
                }
                out.sections[idx].lists.push(list);
            }
        }
        for (k, v) in f.values {
            out.values.entry(k).or_insert(v);
        }
        for (k, v) in f.choices {
            out.choices.entry(k).or_insert(v);
        }
        for n in f.notes {
            if !out.notes.contains(&n) {
                out.notes.push(n);
            }
        }
    }
    // A tab only some schedulers have shows while one of them is picked.
    // Its own condition stays when every scheduler that has the tab has it
    // and it names a field only they have: with another scheduler picked
    // that field hides, and a condition on a hidden field doesn't hold, so
    // the tab hides too. Otherwise the picker's condition takes its place
    // (the fields in it keep theirs). The picker's tab always shows.
    for section in &mut out.sections {
        let who = tab_holders.get(&section.key).cloned().unwrap_or_default();
        if section.key == "general" || who.len() == all.len() {
            continue;
        }
        let keeps = section.when.as_ref().is_some_and(|(k, _)| {
            tab_whens
                .get(&section.key)
                .is_some_and(|w| w.iter().all(|x| x == &section.when))
                && field_keys.contains(k)
                && owners
                    .get(k)
                    .is_some_and(|o| !o.is_empty() && o.iter().all(|s| who.contains(s)))
        });
        if !keeps {
            section.when = Some(("source".into(), who.iter().map(|w| w.to_string()).collect()));
        }
    }
    if let Some(general) = out.sections.iter_mut().find(|s| s.key == "general") {
        general.when = None;
        general.fields.insert(0, picker);
    } else {
        out.sections
            .insert(0, FormSection::new("general", "General").field(picker));
    }
    out
}

/// A key [`combine`] gave a scheduler's own field.
fn own_of(key: &str) -> Option<(Source, &str)> {
    let (source, key) = key.split_once(OWN)?;
    Some((Source::parse(source)?, key))
}

/// A combined new-job form's values as the picked scheduler's own form has
/// them: its fields with [`own_key`]s get their keys back, the other
/// schedulers' own fields are left out.
pub(crate) fn picked_values(values: &FormValues, source: Source) -> FormValues {
    let mut out = FormValues {
        values: BTreeMap::new(),
        lists: values.lists.clone(),
    };
    for (k, v) in &values.values {
        if own_of(k).is_none() {
            out.values.insert(k.clone(), v.clone());
        }
    }
    for (k, v) in &values.values {
        if let Some((_, key)) = own_of(k).filter(|(s, _)| *s == source) {
            out.values.insert(key.to_string(), v.clone());
        }
    }
    out
}

/// What a change writes: the script, whether it changes anything, and how
/// the saved object's id is known afterwards.
pub(crate) struct Plan {
    pub(crate) script: String,
    /// Nothing differs from what the server has.
    pub(crate) empty: bool,
    /// The object's id when it already exists (and keeps it).
    pub(crate) id: Option<String>,
    /// How a new object's id is learned once the script ran.
    pub(crate) new_id: Option<NewId>,
}

/// A new object's id: a column the script answers, or a query run after it
/// (answering a column `id`), the number written after `prefix`.
pub(crate) enum NewId {
    Column(&'static str, &'static str),
    Query(String, &'static str),
}

impl Plan {
    pub(crate) fn nothing(name: &str, id: String) -> Plan {
        Plan {
            script: format!("-- «{}»: no hay cambios que guardar.\n", comment_text(name)),
            empty: true,
            id: Some(id),
            new_id: None,
        }
    }
}

/// The plan for a change. `reveal`: the script runs (secrets in it as
/// they are); otherwise it's shown (secrets masked).
async fn plan(
    s: &PgSession,
    c: &Ctx,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
    reveal: bool,
) -> Result<Plan> {
    match (kind, id) {
        (Kind::Job, None) => {
            let sources = c.job_sources();
            let picked = Vals(&values.values).opt("source");
            let source = match picked.as_deref() {
                Some(k) => Source::parse(k)
                    .filter(|s| sources.contains(s))
                    .ok_or_else(|| {
                        bad(format!(
                            "«{}» no es un planificador en el que se pueda crear un job aquí.",
                            shown(k)
                        ))
                    })?,
                None if sources.len() == 1 => sources[0],
                None if sources.is_empty() => return Err(Error::Unsupported(NO_SOURCE.into())),
                None => return Err(bad("Elegí el planificador del job.".into())),
            };
            let values = &picked_values(values, source);
            if source != Source::Agent {
                pgagent::no_lists(values)?;
            }
            if source != Source::EdbScheduler
                && values.lists.get("arguments").is_some_and(|r| !r.is_empty())
            {
                return Err(bad(
                    "Los argumentos son de los jobs de DBMS_SCHEDULER: quitalos o elegí DBMS_SCHEDULER como planificador.".into(),
                ));
            }
            match source {
                Source::Cron => cron::plan(s, c, cron::Flavor::Cron, None, values).await,
                Source::Task => cron::plan(s, c, cron::Flavor::Task, None, values).await,
                Source::Agent => pgagent::plan(s, c, None, values, reveal).await,
                Source::Timescale => timescale::job_plan(s, c, None, values).await,
                Source::GaussJob | Source::EdbJob | Source::KdbJob => {
                    dbms_job::plan(s, dbms_caps(c)?, None, values).await
                }
                Source::EdbScheduler | Source::KdbScheduler => {
                    dbms_scheduler::job_plan(s, sched_caps(c)?, None, values, sched_prefix(c)).await
                }
                Source::CrdbBackup | Source::CrdbChangefeed | Source::CrdbFeed => {
                    Err(bad("Este planificador es de CockroachDB.".into()))
                }
            }
        }
        (Kind::Job, Some(id)) => match job_target(c, id)? {
            Target::Cron(flavor, n) => cron::plan(s, c, flavor, Some(n), values).await,
            Target::Agent(n) => pgagent::plan(s, c, Some(n), values, reveal).await,
            Target::Timescale(n) => timescale::job_plan(s, c, Some(n), values).await,
            Target::Dbms(job) => dbms_job::plan(s, dbms_caps(c)?, Some(&job), values).await,
            Target::Sched(n) => {
                dbms_scheduler::job_plan(s, sched_caps(c)?, Some(&n), values, sched_prefix(c)).await
            }
            Target::Crdb(_) => Err(bad("Este id es de CockroachDB.".into())),
        },
        (Kind::JobClass, id) => pgagent::class_plan(s, c, id, values).await,
        (Kind::Policy(p), id) => timescale::policy_plan(s, c, p, id, values).await,
        (Kind::Program, id) => dbms_scheduler::program_plan(s, sched_caps(c)?, id, values).await,
        (Kind::Schedule, id) => dbms_scheduler::schedule_plan(s, sched_caps(c)?, id, values).await,
        (Kind::Ttl, _) => Err(bad(
            "Este servidor no tiene TTL por filas (es de CockroachDB).".into(),
        )),
    }
}

/// CockroachDB's plan for a change.
async fn crdb_plan(
    s: &PgSession,
    c: &Ctx,
    kind: Kind,
    id: Option<&str>,
    values: &FormValues,
    reveal: bool,
) -> Result<crdb::CPlan> {
    let x = crdb_caps(c)?;
    match (kind, id) {
        (Kind::Job, None) => {
            let obj = crdb_new(c, values)?;
            let crdb::Obj::New(source) = obj else {
                return Err(bad("Elegí el tipo de schedule.".into()));
            };
            crdb::plan(s, x, obj, &picked_values(values, source), reveal).await
        }
        _ => crdb::plan(s, x, crdb_obj(c, kind, id)?, values, reveal).await,
    }
}

pub(crate) async fn script(
    s: &PgSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    if c.crdb.is_some() {
        return Ok(crdb_plan(s, &c, kind, id, values, false).await?.script);
    }
    Ok(plan(s, &c, kind, id, values, false).await?.script)
}

pub(crate) async fn save(
    s: &mut PgSession,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    if c.crdb.is_some() {
        let p = crdb_plan(s, &c, kind, id, values, true).await?;
        return crdb::save(s, p).await;
    }
    let plan = plan(s, &c, kind, id, values, true).await?;
    if plan.empty {
        return Ok(plan.id);
    }
    let ran = run_script(s, &plan.script).await?;
    match plan.new_id {
        Some(NewId::Column(column, prefix)) => {
            Ok(ran.get(column).map(|n| format!("{prefix}{n}")).or(plan.id))
        }
        Some(NewId::Query(sql, prefix)) => {
            let found = s
                .text(&sql)
                .await?
                .first()
                .and_then(|r| jobs::text(r, "id"))
                .map(|n| format!("{prefix}{n}"));
            Ok(found.or(plan.id))
        }
        None => Ok(plan.id),
    }
}

pub(crate) async fn delete_script(s: &PgSession, kind: &str, id: &str) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    delete_plan(s, &c, kind, id).await
}

async fn delete_plan(s: &PgSession, c: &Ctx, kind: Kind, id: &str) -> Result<String> {
    if let Some(x) = &c.crdb {
        return crdb::delete_script(s, x, crdb_obj(c, kind, Some(id))?).await;
    }
    match kind {
        Kind::Job => match job_target(c, id)? {
            Target::Cron(flavor, n) => cron::delete_script(s, c, flavor, n).await,
            Target::Agent(n) => pgagent::delete_script(s, c, n).await,
            Target::Timescale(n) => timescale::delete_script(s, c, None, n).await,
            Target::Dbms(job) => dbms_job::delete_script(s, dbms_caps(c)?, &job).await,
            Target::Sched(n) => dbms_scheduler::job_delete_script(s, sched_caps(c)?, &n).await,
            Target::Crdb(_) => Err(bad("Este id es de CockroachDB.".into())),
        },
        Kind::JobClass => pgagent::class_delete_script(s, c, id).await,
        Kind::Policy(p) => {
            let n = timescale::policy_id(id)?;
            timescale::delete_script(s, c, Some(p), n).await
        }
        Kind::Program => {
            dbms_scheduler::object_delete_script(s, sched_caps(c)?, "program", id).await
        }
        Kind::Schedule => {
            dbms_scheduler::object_delete_script(s, sched_caps(c)?, "schedule", id).await
        }
        Kind::Ttl => Err(bad(
            "Este servidor no tiene TTL por filas (es de CockroachDB).".into(),
        )),
    }
}

pub(crate) async fn delete(s: &mut PgSession, kind: &str, id: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    let script = delete_plan(s, &c, kind, id).await?;
    if c.crdb.is_some() {
        let p = crdb::CPlan {
            stages: vec![crdb::Stage::Sql(script.clone())],
            script,
            id: None,
            new_id: None,
        };
        return crdb::save(s, p).await.map(|_| ());
    }
    run_script(s, &script).await.map(|_| ())
}

pub(crate) async fn run_from(s: &PgSession, id: &str, _step: &str) -> Result<()> {
    let c = ctx(s).await?;
    Err(match job_target(&c, id)? {
        Target::Dbms(_) => dbms_job::no_steps(),
        Target::Sched(_) => dbms_scheduler::no_steps(),
        other => Error::Unsupported(
            match other {
                Target::Agent(_) => "pgAgent ejecuta siempre todos los pasos habilitados del job, en orden: no puede empezar en un paso",
                Target::Cron(cron::Flavor::Cron, _) => "un job de pg_cron es un solo comando: no tiene pasos",
                Target::Cron(cron::Flavor::Task, _) => "una tarea es un solo comando: no tiene pasos",
                Target::Timescale(_) => "un job de TimescaleDB es un solo procedimiento: no tiene pasos",
                _ => "un schedule de CockroachDB ejecuta una sola sentencia: no tiene pasos",
            }
            .into(),
        ),
    })
}

pub(crate) async fn clear_history(s: &mut PgSession, id: &str) -> Result<()> {
    let c = ctx(s).await?;
    let script = match job_target(&c, id)? {
        Target::Cron(flavor, n) => cron::clear_script(s, &c, flavor, n).await?,
        Target::Agent(n) => pgagent::clear_script(s, &c, n).await?,
        Target::Timescale(n) => timescale::clear_script(s, &c, n).await?,
        Target::Crdb(_) => {
            return Err(Error::Unsupported(
                "CockroachDB guarda las ejecuciones de un schedule como jobs del clúster, que no se pueden borrar".into(),
            ))
        }
        Target::Dbms(JobId::GaussJob(_)) => {
            return Err(Error::Unsupported(
                "openGauss no guarda el historial de sus jobs: solo la última ejecución".into(),
            ))
        }
        Target::Dbms(JobId::Kingbase(_)) | Target::Sched(_) if c.v == Variant::Kingbase => {
            return Err(Error::Unsupported(
                "el historial de kdb_schedule se lee en vistas (*_scheduler_job_run_details) cuya tabla no está documentada: DBine no lo borra".into(),
            ))
        }
        Target::Dbms(JobId::EdbJob(n)) => dbms_job::edb_clear_script(s, &c.found, dbms_job::Needle::Job(n)).await?,
        Target::Sched(n) => {
            let j = dbms_scheduler::read_job(s, sched_caps(&c)?, &n).await?;
            dbms_scheduler::edb_may_clear(s, sched_caps(&c)?, &j).await?;
            dbms_job::edb_clear_script(s, &c.found, dbms_job::Needle::Scheduler(j.job.name.clone())).await?
        }
        Target::Dbms(_) => return Err(bad("Este job no es de este servidor.".into())),
    };
    run_script(s, &script).await.map(|_| ())
}

pub(crate) async fn action_script(
    s: &PgSession,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<String> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    action_plan(s, &c, kind, id, action.trim()).await
}

/// Runs one of the kind's actions: its script, written again from what the
/// server has now.
pub(crate) async fn action(s: &mut PgSession, kind: &str, id: &str, action: &str) -> Result<()> {
    let kind = Kind::parse(kind)?;
    let c = ctx(s).await?;
    let script = action_plan(s, &c, kind, id, action.trim()).await?;
    run_script(s, &script).await.map(|_| ())
}

/// Why pg_cron and the server's tasks have no "run now".
fn runs_only_on_schedule(flavor: cron::Flavor) -> Error {
    Error::Unsupported(
        match flavor {
            cron::Flavor::Cron => {
                "pg_cron no ejecuta un job fuera de su programación: para probarlo, ejecutá su comando en una consulta"
            }
            cron::Flavor::Task => {
                "el servidor no ejecuta una tarea fuera de su programación: para probarla, ejecutá su comando en una consulta"
            }
        }
        .into(),
    )
}

async fn action_plan(s: &PgSession, c: &Ctx, kind: Kind, id: &str, action: &str) -> Result<String> {
    if let Some(x) = &c.crdb {
        return match (kind, action) {
            (Kind::Job | Kind::Ttl, RUN_NOW) => {
                crdb::run_now_script(s, x, crdb_obj(c, kind, Some(id))?).await
            }
            _ => Err(no_action(action)),
        };
    }
    match kind {
        Kind::Job => match job_target(c, id)? {
            Target::Cron(flavor, _) => Err(runs_only_on_schedule(flavor)),
            Target::Agent(n) if action == RUN_NOW => pgagent::run_now_script(s, c, n).await,
            Target::Timescale(n) if action == RUN => timescale::run_script(s, c, None, n).await,
            Target::Dbms(job) if action == RUN || action == RUN_NOW => {
                dbms_job::action_script(s, dbms_caps(c)?, &job, action).await
            }
            Target::Sched(n) if action == RUN => {
                dbms_scheduler::run_script(s, sched_caps(c)?, &n).await
            }
            _ => Err(no_action(action)),
        },
        Kind::Policy(p) if action == RUN => {
            timescale::run_script(s, c, Some(p), timescale::policy_id(id)?).await
        }
        _ => Err(no_action(action)),
    }
}

// ---------------------------------------------------------------------------
// Running a script
// ---------------------------------------------------------------------------

/// Runs a script (its own `BEGIN` … `COMMIT`, or one statement) and
/// answers the first value of each column it returned. Refused while the
/// session has a transaction open: the script's `BEGIN` would join it. A
/// failure leaves the script's transaction aborted: it's rolled back, so
/// nothing of it stays.
pub(crate) async fn run_script(s: &mut PgSession, script: &str) -> Result<HashMap<String, String>> {
    if matches!(
        Session::transaction_state(s).await?,
        Some(TxState::Open | TxState::Failed)
    ) {
        return Err(Error::State(OPEN_TRANSACTION.into()));
    }
    let res = s.client.simple_query(script).await;
    let out = match res {
        Ok(msgs) => {
            let mut values = HashMap::new();
            for m in msgs {
                if let SimpleQueryMessage::Row(r) = m {
                    for (i, col) in r.columns().iter().enumerate() {
                        if let Some(v) = r.get(i) {
                            values
                                .entry(col.name().to_string())
                                .or_insert_with(|| v.to_string());
                        }
                    }
                }
            }
            Ok(values)
        }
        Err(e) => {
            if let Err(r) = s.client.batch_execute("ROLLBACK").await {
                tracing::debug!("{:?}: scheduler script: rollback failed: {r}", s.variant);
            }
            // The server's detail may show a failing row: a step's
            // connection string, password included.
            Err(match crate::err(e) {
                Error::Query(m) => Error::Query(redact_passwords(&m)),
                other => other,
            })
        }
    };
    s.discard_notices();
    out
}

/// `text` with the passwords of the connection strings in it masked:
/// `password=…` (quoted or not) and a URI's `user:password@`.
pub(crate) fn redact_passwords(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    let lower = text.to_ascii_lowercase();
    let mut i = 0;
    while i < text.len() {
        if lower[i..].starts_with("password=") {
            let start = i + "password=".len();
            out.push_str(&text[i..start]);
            let rest = &text[start..];
            let mut end = start;
            if rest.starts_with('\'') {
                let mut chars = rest.char_indices().skip(1);
                end = text.len();
                while let Some((j, c)) = chars.next() {
                    if c == '\\' {
                        chars.next();
                    } else if c == '\'' {
                        end = start + j + 1;
                        break;
                    }
                }
            } else {
                for (j, c) in rest.char_indices() {
                    if c.is_whitespace() || matches!(c, '&' | ')' | ',' | '"' | '\'') {
                        break;
                    }
                    end = start + j + c.len_utf8();
                }
            }
            out.push_str(dbine_driver::SECRET_MASK);
            i = end;
            continue;
        }
        if lower[i..].starts_with("://") {
            let start = i + 3;
            out.push_str("://");
            let rest = &text[start..];
            let stop = rest
                .find(|c: char| c == '/' || c == '?' || c.is_whitespace())
                .unwrap_or(rest.len());
            match rest[..stop].rfind('@') {
                Some(at) => {
                    let userinfo = &rest[..at];
                    match userinfo.split_once(':') {
                        Some((user, _)) => {
                            out.push_str(user);
                            out.push(':');
                            out.push_str(dbine_driver::SECRET_MASK);
                        }
                        None => out.push_str(userinfo),
                    }
                    out.push('@');
                    i = start + at + 1;
                }
                None => i = start,
            }
            continue;
        }
        let c = text[i..].chars().next().unwrap_or(' ');
        out.push(c);
        i += c.len_utf8();
    }
    out
}

/// A script in one transaction, with a comment first.
pub(crate) fn transaction(comment: &str, statements: &[String]) -> String {
    let mut out = String::new();
    for line in comment.lines() {
        out.push_str(&format!("-- {}\n", comment_text(line)));
    }
    out.push_str("BEGIN;\n");
    for st in statements {
        out.push_str(st.trim_end());
        if !st.trim_end().ends_with(';') {
            out.push(';');
        }
        out.push('\n');
    }
    out.push_str("COMMIT;\n");
    out
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values (or a list row's) as the app sent them: untrusted, any
/// JSON type where a field expects another.
#[derive(Clone, Copy)]
pub(crate) struct Vals<'a>(pub(crate) &'a BTreeMap<String, Value>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    /// The value as typed (commands keep their spaces).
    pub(crate) fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    /// The value trimmed.
    pub(crate) fn text(&self, k: &str) -> String {
        self.raw(k).trim().to_string()
    }

    /// The value trimmed, `None` when empty.
    pub(crate) fn opt(&self, k: &str) -> Option<String> {
        Some(self.text(k)).filter(|t| !t.is_empty())
    }

    /// A secret: `None` unless the user typed one (the app sends `null` to
    /// keep what the server has).
    pub(crate) fn secret(&self, k: &str) -> Option<String> {
        match self.get(k) {
            Some(Value::String(s)) if !s.is_empty() => Some(s.clone()),
            _ => None,
        }
    }

    pub(crate) fn bool(&self, k: &str) -> bool {
        self.bool_or(k, false)
    }

    /// The flag, `default` when it didn't come.
    pub(crate) fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => {
                matches!(
                    s.trim().to_ascii_lowercase().as_str(),
                    "true" | "1" | "yes" | "on" | "t"
                )
            }
            None => default,
            _ => false,
        }
    }

    /// A whole number in `min..=max`; `None` when missing or empty.
    pub(crate) fn int(&self, k: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            Some(n) => Err(bad(format!(
                "«{label}» tiene que estar entre {min} y {max} (es {n})."
            ))),
            None => Err(bad(format!("«{label}» tiene que ser un número entero."))),
        }
    }

    /// A list of values (a multi-select's), trimmed, without empties.
    pub(crate) fn strings(&self, k: &str) -> Vec<String> {
        match self.get(k) {
            Some(Value::Array(a)) => a
                .iter()
                .filter_map(|v| match v {
                    Value::String(s) => Some(s.trim().to_string()),
                    Value::Null => None,
                    v => Some(v.to_string()),
                })
                .filter(|s| !s.is_empty())
                .collect(),
            Some(Value::String(s)) if !s.trim().is_empty() => vec![s.trim().to_string()],
            _ => Vec::new(),
        }
    }
}

/// A list's rows from the designer.
pub(crate) fn rows<'a>(values: &'a FormValues, list: &str) -> Vec<Vals<'a>> {
    values
        .lists
        .get(list)
        .map(|rows| rows.iter().map(Vals).collect())
        .unwrap_or_default()
}

/// Longest name DBine writes (jobs, steps, schedules, classes).
pub(crate) const MAX_NAME: usize = 1000;

/// A required name: trimmed, not empty, one line, not too long.
pub(crate) fn name_of(v: Vals, k: &str, label: &str) -> Result<String> {
    let name = v.text(k);
    if name.is_empty() {
        return Err(bad(format!("«{label}» no puede quedar vacío.")));
    }
    plain(&name, label)?;
    Ok(name)
}

/// One line of text, not too long.
pub(crate) fn plain(t: &str, label: &str) -> Result<()> {
    if t.chars().any(char::is_control) {
        return Err(bad(format!(
            "«{label}» no puede tener saltos de línea ni caracteres de control."
        )));
    }
    if t.chars().count() > MAX_NAME {
        return Err(bad(format!(
            "«{label}» puede tener hasta {MAX_NAME} caracteres."
        )));
    }
    Ok(())
}

/// A name the user picked from a list of the server's: it has to be one of
/// them. Returns the server's spelling.
pub(crate) fn pick<'a>(
    name: &str,
    choices: &'a [(String, String)],
    missing: impl FnOnce(&str) -> String,
) -> Result<&'a str> {
    choices
        .iter()
        .find(|(v, _)| v == name)
        .map(|(v, _)| v.as_str())
        .ok_or_else(|| bad(missing(&shown(name))))
}

/// `choices` with `current` added when the server's list lacks it (a
/// database that no longer takes connections, a role that can't log in).
pub(crate) fn with_current(
    mut choices: Vec<(String, String)>,
    current: &str,
) -> Vec<(String, String)> {
    if !current.is_empty() && !choices.iter().any(|(v, _)| v == current) {
        choices.push((current.to_string(), current.to_string()));
    }
    choices
}

/// A list of names as `(value, label)` pairs.
pub(crate) fn pairs(names: Vec<String>) -> Vec<(String, String)> {
    names.into_iter().map(|n| (n.clone(), n)).collect()
}

/// The first column of each row.
pub(crate) async fn names(s: &PgSession, sql: &str) -> Result<Vec<String>> {
    Ok(s.text(sql)
        .await?
        .iter()
        .filter_map(|r| r.get(0).map(str::to_string))
        .collect())
}

/// The databases a job can run in.
pub(crate) async fn databases(s: &PgSession) -> Result<Vec<(String, String)>> {
    Ok(pairs(
        names(
            s,
            "SELECT datname FROM pg_catalog.pg_database WHERE datallowconn AND NOT datistemplate ORDER BY 1",
        )
        .await?,
    ))
}

/// The roles that can log in (a job runs as one of them).
pub(crate) async fn login_roles(s: &PgSession) -> Result<Vec<(String, String)>> {
    Ok(pairs(
        names(
            s,
            "SELECT rolname FROM pg_catalog.pg_roles WHERE rolcanlogin ORDER BY 1",
        )
        .await?,
    ))
}

// ---------------------------------------------------------------------------
// Dates and times
// ---------------------------------------------------------------------------

fn digits(s: &str, n: usize) -> Option<u32> {
    (s.len() == n && s.bytes().all(|b| b.is_ascii_digit()))
        .then(|| s.parse().ok())
        .flatten()
}

fn days_in(y: u32, m: u32) -> u32 {
    match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        _ if (y.is_multiple_of(4) && !y.is_multiple_of(100)) || y.is_multiple_of(400) => 29,
        _ => 28,
    }
}

/// `YYYY-MM-DD`, a real day.
pub(crate) fn valid_date(t: &str) -> bool {
    let p: Vec<&str> = t.split('-').collect();
    if p.len() != 3 {
        return false;
    }
    match (digits(p[0], 4), digits(p[1], 2), digits(p[2], 2)) {
        (Some(y), Some(m), Some(d)) => {
            (1..=9999).contains(&y) && (1..=12).contains(&m) && d >= 1 && d <= days_in(y, m)
        }
        _ => false,
    }
}

/// `HH:MM[:SS]`.
pub(crate) fn valid_time(t: &str) -> bool {
    let p: Vec<&str> = t.split(':').collect();
    if !(2..=3).contains(&p.len()) {
        return false;
    }
    let h = digits(p[0], 2).filter(|h| *h < 24);
    let m = digits(p[1], 2).filter(|m| *m < 60);
    let s = match p.get(2) {
        Some(x) => digits(x, 2).filter(|s| *s < 60),
        None => Some(0),
    };
    h.is_some() && m.is_some() && s.is_some()
}

/// `YYYY-MM-DD[T ]HH:MM[:SS]`, with an optional `Z` or `±HH[:MM]` offset
/// (no offset: the server's session time zone).
pub(crate) fn valid_datetime(t: &str) -> bool {
    let Some((date, rest)) = t.split_once(['T', ' ']) else {
        return false;
    };
    if !valid_date(date) {
        return false;
    }
    let (time, offset) = match rest.find(['Z', 'z', '+', '-']) {
        Some(i) => (&rest[..i], &rest[i..]),
        None => (rest, ""),
    };
    let time = time.split('.').next().unwrap_or_default();
    let offset_ok = match offset {
        "" | "Z" | "z" => true,
        o => {
            let body = &o[1..];
            let (h, m) = match body.split_once(':') {
                Some(hm) => hm,
                None if body.len() == 4 && body.is_ascii() => body.split_at(2),
                None => (body, "00"),
            };
            digits(h, 2).is_some_and(|h| h <= 15) && digits(m, 2).is_some_and(|m| m < 60)
        }
    };
    valid_time(time) && offset_ok
}

/// A date-time field, checked: `None` when empty.
pub(crate) fn datetime_field(v: Vals, k: &str, label: &str) -> Result<Option<String>> {
    match v.opt(k) {
        None => Ok(None),
        Some(t) if valid_datetime(&t) => Ok(Some(t)),
        Some(t) => Err(bad(format!(
            "«{label}»: «{}» no es una fecha y hora válida (AAAA-MM-DDTHH:MM:SS).",
            shown(&t)
        ))),
    }
}

/// A `timestamptz` literal.
pub(crate) fn ts_lit(v: Variant, t: &str) -> String {
    format!("{}::timestamptz", lit(v, t))
}

/// A `timestamptz` column read back as the form shows it: UTC, RFC 3339.
pub(crate) fn utc_sql(expr: &str) -> String {
    format!("to_char({expr} AT TIME ZONE 'UTC', 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')")
}

/// A command's text as a literal: dollar-quoted (it reads better).
pub(crate) fn code_lit(code: &str) -> String {
    dollar(code)
}

/// An identifier.
pub(crate) fn id(name: &str) -> String {
    ident(name)
}

#[cfg(test)]
mod tests;
