//! Each engine's scheduler managed whole through the scheduler contract
//! ([`dbine_driver::Session::scheduler_info`] and the rest), per preset,
//! on top of the jobs code ([`crate::jobs`]) that lists and runs them:
//!
//! - Db2 LUW ([`db2`]): the Administrative Task Scheduler's tasks, with
//!   `SYSPROC.ADMIN_TASK_ADD` / `ADMIN_TASK_UPDATE` / `ADMIN_TASK_REMOVE`;
//!   the status records are its log.
//! - Db2 for z/OS ([`db2zos`]): the same scheduler's tasks (a stored
//!   procedure or JCL, run by time, cron or another task's end), with the
//!   z/OS procedures and their return code and message.
//! - Db2 for i ([`db2i`]): the job scheduler's entries, with `ADDJOBSCDE`,
//!   `CHGJOBSCDE`, `RMVJOBSCDE`, `HLDJOBSCDE` and `RLSJOBSCDE` through
//!   `QSYS2.QCMDEXC`.
//! - Informix and GBase 8s ([`informix`]): the DB Scheduler's tasks and
//!   sensors (`sysadmin:ph_task`, their parameters in `ph_threshold`) and
//!   groups (`ph_group`), by INSERT / UPDATE / DELETE; its alerts are its
//!   log.
//! - Hive 4 and Cloudera ([`hive`]): scheduled queries, with `CREATE` /
//!   `ALTER` / `DROP SCHEDULED QUERY`.
//! - Vertica 11.1.1 on ([`vertica`]): scheduled triggers and the schedules
//!   they run on, with `CREATE` / `ALTER` / `DROP TRIGGER | SCHEDULE`.
//! - SAP ASE ([`ase`]): Job Scheduler's scheduled jobs, jobs and schedules,
//!   with `sp_sjobcreate`, `sp_sjobmodify`, `sp_sjobcmd` and `sp_sjobdrop`;
//!   `sp_sjobhistory … drop` clears a scheduled job's history.
//! - SQL Anywhere ([`sqla`]): events, with `CREATE` / `ALTER` / `DROP
//!   EVENT` (schedules, system event type and conditions, handler).
//! - InterSystems IRIS and Caché ([`iris`]): the Task Manager's tasks, read
//!   only: they are written with ObjectScript, which SQL doesn't reach.
//! - Dameng ([`dameng`], [`dameng_dbms`]): the job system's jobs (steps,
//!   schedules, notifications), operators and alerts, with the `SP_*JOB*`
//!   procedures; DBMS_JOB's and DBMS_SCHEDULER's jobs.
//! - Altibase ([`altibase`]): `CREATE` / `ALTER` / `DROP JOB`.
//! - Virtuoso ([`virtuoso`]): the scheduler's events, by INSERT / UPDATE /
//!   DELETE on `DB.DBA.SYS_SCHEDULED_EVENT`.
//!
//! The job kind is "job" everywhere and its ids are the ones
//! [`crate::jobs`] gives the same objects. What else a kind offers ("run",
//! "stop", clearing the history) is [`offers`], from each engine's
//! documentation; an action's script is the statement the jobs list runs
//! for the same thing, shown and confirmed first. Objects the engine
//! ships or another user owns say what can't be done on them
//! ([`job_item`]). Every save is a [`Plan`]: the
//! script the designer shows, step by step, run in one transaction where
//! the engine's changes are transactional (Db2 LUW, Informix, Dameng,
//! Virtuoso). What a plan makes the user lose or risk (a drop and create,
//! the history it takes, statements that can stop half way) is a `-- ⚠`
//! line of its script ([`Plan::warn`]), only when the plan does it: the
//! app asks to continue before it runs. Values and
//! ids come from the app as untrusted text: ids are parsed and looked up,
//! names checked against the engine's rules, and every value goes into the
//! script as a literal of the engine (quotes doubled) or a checked
//! identifier.
//!
//! None of these engines has a container here: everything is built from
//! each engine's documentation and checked with canned result sets.

mod altibase;
mod ase;
mod dameng;
mod dameng_dbms;
mod db2;
mod db2i;
mod db2zos;
mod hive;
mod informix;
mod iris;
mod sqla;
#[cfg(test)]
mod tests;
mod vertica;
mod virtuoso;

use crate::design::{eng, Eng};
use crate::jobs::{self, JobSource};
use crate::monitor::Set;
use crate::presets::Preset;
use dbine_driver::{
    Error, FormValues, MonitorTable, ObjectForm, Result, SchedulerAction, SchedulerInfo,
    SchedulerItem, SchedulerList,
};
use serde_json::Value;
use std::collections::BTreeMap;

pub(crate) use jobs::Ctx;

const NOT_HERE: &str = "este motor no administra su planificador desde DBine";
const NO_STEPS: &str =
    "las tareas de este planificador tienen un solo paso: no se pueden empezar en otro";
/// What a save refuses while the session has changes of its own pending.
pub(crate) const OPEN_TRANSACTION: &str = "Hay una transacción abierta en esta sesión: confirmala o deshacela antes de cambiar el planificador.";
/// The warning of a plan whose statements run one by one ([`Plan::one_by_one`]).
pub(crate) const ONE_BY_ONE: &str =
    "Las sentencias corren una por una, sin transacción: si una falla, las anteriores quedan hechas.";
/// What starts a warning's comment line ([`Plan::warn`]).
const WARNING: &str = "⚠";

/// The presets whose scheduler DBine manages.
pub fn supported(p: &Preset) -> bool {
    matches!(
        eng(p),
        Eng::Db2
            | Eng::Db2zos
            | Eng::Db2i
            | Eng::Informix
            | Eng::Hive
            | Eng::Vertica
            | Eng::Ase
            | Eng::Sqla
            | Eng::Iris
            | Eng::Dameng
            | Eng::Altibase
            | Eng::Virtuoso
    )
}

fn unsupported(p: &Preset) -> Error {
    Error::Unsupported(jobs::unsupported_reason(p).unwrap_or(NOT_HERE).to_string())
}

// ---------------------------------------------------------------------------
// The contract's calls, per engine
// ---------------------------------------------------------------------------

pub fn info(p: &Preset, src: &mut dyn JobSource, ctx: &Ctx) -> Result<SchedulerInfo> {
    let mut info = match eng(p) {
        Eng::Db2 => db2::info(src),
        Eng::Db2zos => db2zos::info(src),
        Eng::Db2i => db2i::info(src),
        Eng::Informix => informix::info(src, ctx),
        Eng::Hive => hive::info(src),
        Eng::Vertica => vertica::info(src),
        Eng::Ase => ase::info(src),
        Eng::Sqla => sqla::info(src),
        Eng::Iris => iris::info(src),
        Eng::Dameng => dameng::info(src),
        Eng::Altibase => altibase::info(src),
        Eng::Virtuoso => virtuoso::info(src),
        _ => Err(unsupported(p)),
    }?;
    for k in &mut info.kinds {
        let o = offers(p, &k.key);
        k.actions = o.actions.iter().map(|a| action_of(a)).collect();
        k.clear_history = o.clear_history;
        // No engine of this family starts a job at one of its steps.
        k.run_from_step = false;
    }
    Ok(info)
}

// ---------------------------------------------------------------------------
// What each kind offers besides create, edit and delete
// ---------------------------------------------------------------------------

/// "Ejecutar ahora": the job runs now, outside its schedule.
pub(crate) const RUN: &str = "run";
/// "Detener": its current run stops.
pub(crate) const STOP: &str = "stop";

/// What a kind offers: its actions' keys and whether its objects' run
/// history can be deleted ([`clear_history`]). None of these engines
/// starts a job at a step (most have one; Dameng always starts at the
/// first).
pub(crate) struct Offers {
    pub(crate) actions: &'static [&'static str],
    pub(crate) clear_history: bool,
}

/// Each engine's kinds, from its documentation:
///
/// - Db2 LUW: no run now, stop or enable (tasks run on their schedule
///   only); `ADMIN_TASK_REMOVE` with an end time deletes the status
///   records up to it and keeps the task.
/// - Db2 for z/OS: no run now; `ADMIN_TASK_CANCEL` stops a running task;
///   nothing clears its status.
/// - Db2 for i: the scheduler doesn't submit an entry from SQL (option 10
///   of WRKJOBSCDE has no command) nor stops it; it keeps no history.
/// - Informix: `exectask_async` runs a task now; its runs are `ph_run`
///   rows. Groups have no actions.
/// - Hive: `ALTER SCHEDULED QUERY … EXECUTE` runs one now, `KILL QUERY`
///   stops its executions; the metastore ages them out on its own.
/// - Vertica: `EXECUTE_TRIGGER` runs a trigger's procedure now; no stop,
///   no history. Schedules have no actions.
/// - SAP ASE: `sp_sjobcontrol` `run_now` and `terminate` on a scheduled
///   job; `sp_sjobhistory … drop` clears its history.
/// - SQL Anywhere: `TRIGGER EVENT`; events keep no history.
/// - IRIS: read only (ObjectScript runs and suspends tasks).
/// - Dameng: `SP_DBMS_JOB_RUN_ASYNC` / `SP_STOP_RUNNING_JOB` and
///   `SP_JOB_CLEAR_HISTORIES` (job system and DBMS_JOB);
///   `DBMS_SCHEDULER.RUN_JOB` / `STOP_JOB` / `PURGE_LOG`.
/// - Altibase and Virtuoso: no run now, no stop, only the last run kept.
pub(crate) fn offers(p: &Preset, kind: &str) -> Offers {
    let (actions, clear_history): (&'static [&'static str], bool) = match (eng(p), kind.trim()) {
        (Eng::Db2, "job") => (&[], true),
        (Eng::Db2zos, "job") => (&[STOP], false),
        (Eng::Informix, "job") => (&[RUN], true),
        (Eng::Hive, "job") => (&[RUN, STOP], false),
        (Eng::Vertica, "job") => (&[RUN], false),
        (Eng::Ase, "job") => (&[RUN, STOP], true),
        (Eng::Sqla, "job") => (&[RUN], false),
        (Eng::Dameng, "job" | "dbms_job" | "scheduler_job") => (&[RUN, STOP], true),
        _ => (&[], false),
    };
    Offers {
        actions,
        clear_history,
    }
}

/// An action with its label (literals, for the message extractor).
fn action_of(key: &str) -> SchedulerAction {
    match key {
        RUN => SchedulerAction::new("run", "Ejecutar ahora"),
        _ => SchedulerAction::new("stop", "Detener"),
    }
}

/// The kind is one of the engine's (its key parsed as each engine does).
fn check_kind(p: &Preset, kind: &str) -> Result<()> {
    match eng(p) {
        Eng::Db2 => db2::kind(kind),
        Eng::Db2zos => db2zos::kind(kind),
        Eng::Db2i => db2i::kind(kind),
        Eng::Informix => informix::kind(kind).map(|_| ()),
        Eng::Hive => hive::kind(kind),
        Eng::Vertica => vertica::kind(kind).map(|_| ()),
        Eng::Ase => ase::kind(kind).map(|_| ()),
        Eng::Sqla => sqla::kind(kind),
        Eng::Iris => iris::kind(kind),
        Eng::Dameng => dameng::kind(kind).map(|_| ()),
        Eng::Altibase => altibase::kind(kind),
        Eng::Virtuoso => virtuoso::kind(kind),
        _ => Err(unsupported(p)),
    }
}

/// The plan of one of the kind's actions on object `id`: the statements
/// the jobs list runs for the same thing ([`jobs::run`], [`jobs::stop`]),
/// checked against the server the same way.
fn action_plan(
    p: &Preset,
    src: &mut dyn JobSource,
    ctx: &Ctx,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<Plan> {
    check_kind(p, kind)?;
    let action = action.trim();
    if !offers(p, kind).actions.contains(&action) {
        return Err(Error::Unsupported(format!(
            "«{}» no es una acción de este tipo de objeto del planificador",
            short(action)
        )));
    }
    if matches!(eng(p), Eng::Dameng) {
        dameng::check_id(kind, id)?;
    }
    if matches!(eng(p), Eng::Db2zos) {
        return db2zos::cancel_plan(src, id);
    }
    let stmts = if action == RUN {
        jobs::run_stmts(p, src, ctx, id)?
    } else {
        jobs::stop_stmts(p, src, id)?
    };
    let mut plan = Plan::new(action_note(p, kind, action));
    plan.go = matches!(eng(p), Eng::Ase | Eng::Sqla);
    if matches!(eng(p), Eng::Dameng) && kind.trim() == "scheduler_job" && action == STOP {
        dameng_dbms::stop_warning(src, id, &mut plan);
    }
    for c in stmts {
        if c.rows {
            plan.query(c.sql);
        } else {
            plan.exec(c.sql);
        }
    }
    Ok(plan)
}

/// What the action does, as the script's first line says it.
fn action_note(p: &Preset, kind: &str, action: &str) -> &'static str {
    match (eng(p), kind.trim(), action) {
        (Eng::Informix, _, RUN) => {
            "Ejecuta la tarea ahora, en segundo plano (exectask_async), fuera de su programación."
        }
        (Eng::Hive, _, RUN) => {
            "Pasa la próxima ejecución de la consulta programada a ahora: la corre el planificador de Hive."
        }
        (Eng::Hive, _, _) => "Corta las ejecuciones en curso de la consulta programada (KILL QUERY).",
        (Eng::Vertica, _, _) => {
            "Ejecuta ahora el procedimiento del trigger (EXECUTE_TRIGGER), en esta sesión: termina cuando termina el procedimiento."
        }
        (Eng::Ase, _, RUN) => "Job Scheduler corre el job programado ahora (run_now).",
        (Eng::Ase, _, _) => "Job Scheduler corta la ejecución en curso del job programado (terminate).",
        (Eng::Sqla, _, _) => {
            "Dispara el evento ahora (TRIGGER EVENT): su acción corre en una conexión propia."
        }
        (Eng::Dameng, "scheduler_job", RUN) => {
            "Ejecuta el job ahora, en segundo plano (RUN_JOB sin la sesión actual)."
        }
        (Eng::Dameng, "scheduler_job", _) => "Detiene la ejecución en curso del job (STOP_JOB).",
        (Eng::Dameng, _, RUN) => "Ejecuta el job ahora, en segundo plano (SP_DBMS_JOB_RUN_ASYNC).",
        (Eng::Dameng, _, _) => {
            "Corta la sesión del job en ejecución (SP_STOP_RUNNING_JOB); su programación sigue igual."
        }
        _ => "Acción del planificador.",
    }
}

pub fn action_script(
    p: &Preset,
    src: &mut dyn JobSource,
    ctx: &Ctx,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<String> {
    Ok(action_plan(p, src, ctx, kind, id, action)?.script())
}

pub fn action(
    p: &Preset,
    src: &mut dyn Src,
    ctx: &Ctx,
    kind: &str,
    id: &str,
    action: &str,
) -> Result<()> {
    let plan = action_plan(p, src, ctx, kind, id, action)?;
    execute(src, plan).map(|_| ())
}

pub fn items(p: &Preset, src: &mut dyn JobSource, ctx: &Ctx, kind: &str) -> Result<SchedulerList> {
    match eng(p) {
        Eng::Db2 => {
            db2::kind(kind)?;
            job_items(p, src, ctx, false)
        }
        Eng::Db2zos => {
            db2zos::kind(kind)?;
            job_items(p, src, ctx, false)
        }
        Eng::Db2i => {
            db2i::kind(kind)?;
            job_items(p, src, ctx, true)
        }
        Eng::Informix => match informix::kind(kind)? {
            informix::Kind::Task => job_items(p, src, ctx, true),
            informix::Kind::Group => informix::group_items(src),
        },
        Eng::Hive => {
            hive::kind(kind)?;
            job_items(p, src, ctx, true)
        }
        Eng::Vertica => match vertica::kind(kind)? {
            vertica::Kind::Trigger => job_items(p, src, ctx, true),
            vertica::Kind::Schedule => vertica::schedule_items(src),
        },
        Eng::Ase => match ase::kind(kind)? {
            ase::Kind::Scheduled => job_items(p, src, ctx, true),
            k => ase::items(src, k),
        },
        Eng::Sqla => {
            sqla::kind(kind)?;
            job_items(p, src, ctx, true)
        }
        Eng::Iris => {
            iris::kind(kind)?;
            job_items(p, src, ctx, true)
        }
        Eng::Dameng => dameng::items(p, src, ctx, dameng::kind(kind)?),
        Eng::Altibase => {
            altibase::kind(kind)?;
            job_items(p, src, ctx, true)
        }
        Eng::Virtuoso => {
            virtuoso::kind(kind)?;
            job_items(p, src, ctx, true)
        }
        _ => Err(unsupported(p)),
    }
}

pub fn form(
    p: &Preset,
    src: &mut dyn JobSource,
    ctx: &Ctx,
    kind: &str,
    id: Option<&str>,
) -> Result<ObjectForm> {
    match eng(p) {
        Eng::Db2 => {
            db2::kind(kind)?;
            db2::form(src, id)
        }
        Eng::Db2zos => {
            db2zos::kind(kind)?;
            db2zos::form(src, id)
        }
        Eng::Db2i => {
            db2i::kind(kind)?;
            db2i::form(src, id)
        }
        Eng::Informix => informix::form(src, ctx, informix::kind(kind)?, id),
        Eng::Hive => {
            hive::kind(kind)?;
            hive::form(src, id)
        }
        Eng::Vertica => vertica::form(src, vertica::kind(kind)?, id),
        Eng::Ase => ase::form(src, ase::kind(kind)?, id),
        Eng::Sqla => {
            sqla::kind(kind)?;
            sqla::form(src, id)
        }
        Eng::Iris => {
            iris::kind(kind)?;
            iris::form(src, id)
        }
        Eng::Dameng => dameng::form(src, dameng::kind(kind)?, id),
        Eng::Altibase => {
            altibase::kind(kind)?;
            altibase::form(src, id)
        }
        Eng::Virtuoso => {
            virtuoso::kind(kind)?;
            virtuoso::form(src, id)
        }
        _ => Err(unsupported(p)),
    }
}

/// The plan for a change; `reveal`: secrets as they are (only to run it).
fn plan(
    p: &Preset,
    src: &mut dyn JobSource,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
    reveal: Reveal,
) -> Result<Plan> {
    match eng(p) {
        Eng::Db2 => {
            db2::kind(kind)?;
            db2::plan(src, id, values)
        }
        Eng::Db2zos => {
            db2zos::kind(kind)?;
            db2zos::plan(src, id, values, reveal)
        }
        Eng::Db2i => {
            db2i::kind(kind)?;
            db2i::plan(src, id, values)
        }
        Eng::Informix => informix::plan(src, informix::kind(kind)?, id, values),
        Eng::Hive => {
            hive::kind(kind)?;
            hive::plan(src, id, values)
        }
        Eng::Vertica => vertica::plan(src, vertica::kind(kind)?, id, values),
        Eng::Ase => ase::plan(src, ase::kind(kind)?, id, values),
        Eng::Sqla => {
            sqla::kind(kind)?;
            sqla::plan(src, id, values)
        }
        Eng::Iris => {
            iris::kind(kind)?;
            Err(Error::Unsupported(iris::NO_WRITE.into()))
        }
        Eng::Dameng => dameng::plan(src, dameng::kind(kind)?, id, values),
        Eng::Altibase => {
            altibase::kind(kind)?;
            altibase::plan(src, id, values)
        }
        Eng::Virtuoso => {
            virtuoso::kind(kind)?;
            virtuoso::plan(src, id, values)
        }
        _ => Err(unsupported(p)),
    }
}

pub fn script(
    p: &Preset,
    src: &mut dyn JobSource,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<String> {
    Ok(plan(p, src, kind, id, values, Reveal::Mask)?.script())
}

pub fn save(
    p: &Preset,
    src: &mut dyn Src,
    kind: &str,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Option<String>> {
    let plan = plan(p, src, kind, id, values, Reveal::Real)?;
    execute(src, plan)
}

fn delete_plan(p: &Preset, src: &mut dyn JobSource, kind: &str, id: &str) -> Result<Plan> {
    match eng(p) {
        Eng::Db2 => {
            db2::kind(kind)?;
            db2::delete_plan(src, id)
        }
        Eng::Db2zos => {
            db2zos::kind(kind)?;
            db2zos::delete_plan(src, id)
        }
        Eng::Db2i => {
            db2i::kind(kind)?;
            db2i::delete_plan(src, id)
        }
        Eng::Informix => informix::delete_plan(src, informix::kind(kind)?, id),
        Eng::Hive => {
            hive::kind(kind)?;
            hive::delete_plan(src, id)
        }
        Eng::Vertica => vertica::delete_plan(src, vertica::kind(kind)?, id),
        Eng::Ase => ase::delete_plan(src, ase::kind(kind)?, id),
        Eng::Sqla => {
            sqla::kind(kind)?;
            sqla::delete_plan(src, id)
        }
        Eng::Iris => {
            iris::kind(kind)?;
            Err(Error::Unsupported(iris::NO_WRITE.into()))
        }
        Eng::Dameng => dameng::delete_plan(src, dameng::kind(kind)?, id),
        Eng::Altibase => {
            altibase::kind(kind)?;
            altibase::delete_plan(src, id)
        }
        Eng::Virtuoso => {
            virtuoso::kind(kind)?;
            virtuoso::delete_plan(src, id)
        }
        _ => Err(unsupported(p)),
    }
}

pub fn delete_script(p: &Preset, src: &mut dyn JobSource, kind: &str, id: &str) -> Result<String> {
    Ok(delete_plan(p, src, kind, id)?.script())
}

pub fn delete(p: &Preset, src: &mut dyn Src, kind: &str, id: &str) -> Result<()> {
    let plan = delete_plan(p, src, kind, id)?;
    execute(src, plan).map(|_| ())
}

/// None of these engines starts a job at a step: most have one step, and
/// Dameng's job system always runs a job from its first.
pub fn run_from(p: &Preset, _id: &str, _step: &str) -> Result<()> {
    if !supported(p) {
        return Err(unsupported(p));
    }
    if matches!(eng(p), Eng::Dameng) {
        return Err(Error::Unsupported(dameng::NO_START_AT.into()));
    }
    Err(Error::Unsupported(NO_STEPS.into()))
}

/// The plan that deletes the job's run history, where the engine keeps
/// one it lets go of.
fn clear_plan(p: &Preset, src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    match eng(p) {
        Eng::Db2 => db2::clear_plan(src, id),
        Eng::Db2zos => Err(Error::Unsupported(db2zos::NO_CLEAR.into())),
        Eng::Db2i => Err(Error::Unsupported(jobs::db2i::NO_HISTORY.into())),
        Eng::Informix => informix::clear_plan(src, id),
        Eng::Hive => Err(Error::Unsupported(hive::NO_CLEAR.into())),
        Eng::Vertica => Err(Error::Unsupported(jobs::vertica::NO_HISTORY.into())),
        Eng::Ase => ase::clear_plan(src, id),
        Eng::Sqla => Err(Error::Unsupported(jobs::sqla::NO_HISTORY.into())),
        Eng::Iris => Err(Error::Unsupported(iris::NO_CLEAR.into())),
        Eng::Dameng => dameng::clear_plan(src, id),
        Eng::Altibase => Err(Error::Unsupported(jobs::altibase::NO_HISTORY.into())),
        Eng::Virtuoso => Err(Error::Unsupported(jobs::virtuoso::NO_HISTORY.into())),
        _ => Err(unsupported(p)),
    }
}

pub fn clear_history(p: &Preset, src: &mut dyn Src, id: &str) -> Result<()> {
    let plan = clear_plan(p, src, id)?;
    execute(src, plan).map(|_| ())
}

pub fn logs(p: &Preset, src: &mut dyn JobSource) -> Result<Vec<MonitorTable>> {
    match eng(p) {
        Eng::Db2 => db2::logs(src),
        Eng::Db2zos => db2zos::logs(src),
        Eng::Informix => informix::logs(src),
        Eng::Db2i
        | Eng::Hive
        | Eng::Vertica
        | Eng::Ase
        | Eng::Sqla
        | Eng::Iris
        | Eng::Dameng
        | Eng::Altibase
        | Eng::Virtuoso => Err(Error::Unsupported(
            "el planificador de este motor no tiene un registro propio".into(),
        )),
        _ => Err(unsupported(p)),
    }
}

/// The jobs as the tree lists them, from the jobs list (same ids).
/// `enabled`: the engine has an enabled state for them.
fn job_items(
    p: &Preset,
    src: &mut dyn JobSource,
    ctx: &Ctx,
    enabled: bool,
) -> Result<SchedulerList> {
    let l = jobs::list(p, src, ctx)?;
    Ok(SchedulerList {
        items: l
            .jobs
            .into_iter()
            .map(|j| job_item(p, "job", j, enabled))
            .collect(),
        notes: l.notes,
    })
}

/// A job of the jobs list as the tree lists it under kind `kind`: the
/// actions it offers now (run where the list says it can run, stop while
/// it runs) and what can't be changed or deleted on it (Informix's
/// built-in tasks, IBM i's IBM-supplied entries, a Hive query of another
/// namespace).
pub(super) fn job_item(
    p: &Preset,
    kind: &str,
    j: dbine_driver::ServerJob,
    enabled: bool,
) -> SchedulerItem {
    let keys = offers(p, kind).actions;
    let actions = (!keys.is_empty()).then(|| {
        keys.iter()
            .filter(|k| match **k {
                RUN => j.can_run,
                STOP => j.can_stop,
                _ => false,
            })
            .map(|k| k.to_string())
            .collect()
    });
    let (can_edit, can_delete) = match eng(p) {
        Eng::Informix if j.system => (None, Some(false)),
        Eng::Db2i if j.system => (Some(false), Some(false)),
        // Another namespace's query or a name ALTER can't reach (Hive);
        // another user's job for someone who isn't a DBA (Dameng): the
        // jobs list's enable switch is off for the same reason.
        Eng::Hive | Eng::Dameng if !j.can_enable => (Some(false), Some(false)),
        _ => (None, None),
    };
    // Dameng clears the history of the jobs one manages only (a DBA's or
    // the owner's, [`dameng::clear_plan`]): the kind offers it, these
    // jobs refuse it.
    let clear_history = (matches!(eng(p), Eng::Dameng) && !j.can_enable).then_some(false);
    SchedulerItem {
        id: j.id,
        name: j.name,
        description: j.description,
        enabled: enabled.then_some(j.enabled),
        system: j.system,
        can_edit,
        can_delete,
        actions,
        run_from_step: None,
        clear_history,
    }
}

/// A kind key this engine doesn't have.
fn bad_kind(kind: &str) -> Error {
    bad(format!(
        "«{}» no es un tipo de objeto del planificador de este motor",
        short(kind)
    ))
}

// ---------------------------------------------------------------------------
// Plans: the script and how it runs
// ---------------------------------------------------------------------------

/// Secrets masked (the script the designer shows) or as they are (the one
/// that runs).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Reveal {
    Mask,
    Real,
}

/// How a CALL's values read back (its input-output and output markers', in
/// order: Db2 for z/OS's return code and message, and the name a new task
/// got) say whether it worked; `Some`: the saved object's id they give.
pub(crate) type OutCheck = fn(&[Option<String>]) -> Result<Option<String>>;

/// A `?` marker of a CALL step: a value bound (never spliced into the
/// text, as a password), bound and read back, or only read back. Not
/// `Debug`: it may hold a secret.
pub(crate) enum Param {
    In(Option<String>),
    InOut(Option<String>),
    Out,
}

pub(crate) enum Step {
    /// A comment line of the script.
    Comment(String),
    /// A statement without rows.
    Exec(String),
    /// A statement that answers rows (Vertica's meta-functions).
    Query(String),
    /// A CALL with its `?` markers, in order, and how the values it gives
    /// back say whether it worked.
    CallOut(String, Vec<Param>, OutCheck),
}

/// A new object's id, read after the save.
pub(crate) type Lookup = Box<dyn FnOnce(&mut dyn JobSource) -> Result<Option<String>>>;

/// A change: the steps the script shows and runs, whether they run in one
/// transaction, and the object's id afterwards.
pub(crate) struct Plan {
    pub(crate) steps: Vec<Step>,
    pub(crate) atomic: bool,
    pub(crate) id: Option<String>,
    pub(crate) lookup: Option<Lookup>,
    /// The lookup finishes the save (IBM i holds a new entry it can only
    /// name afterwards): its failure is the save's, not just a missing id.
    pub(crate) lookup_required: bool,
    /// The script ends each statement with a `go` line (SAP ASE and SQL
    /// Anywhere, whose statements may hold `;` themselves) instead of `;`.
    pub(crate) go: bool,
}

impl Plan {
    /// A plan whose first line says what it does.
    pub(crate) fn new(comment: &str) -> Plan {
        Plan {
            steps: vec![Step::Comment(comment.to_string())],
            atomic: false,
            id: None,
            lookup: None,
            lookup_required: false,
            go: false,
        }
    }

    /// Nothing differs from what the server has.
    pub(crate) fn nothing(name: &str, id: String) -> Plan {
        let mut p = Plan::new(&format!("«{name}»: no hay cambios que guardar."));
        p.id = Some(id);
        p
    }

    pub(crate) fn comment(&mut self, c: &str) {
        self.steps.push(Step::Comment(c.to_string()));
    }

    /// Something the user accepts before the plan runs (the object is
    /// dropped and created again, loses its history, can be left half
    /// changed): a `-- ⚠ …` line above the statements, after the warnings
    /// already there, which the app lists in its confirmation. `text` is a
    /// whole Spanish sentence of its own, a literal (or a literal format),
    /// so the message extractor takes it and `tb()` matches it.
    pub(crate) fn warn(&mut self, text: &str) {
        let at = self
            .steps
            .iter()
            .position(|s| !matches!(s, Step::Comment(_)))
            .unwrap_or(self.steps.len());
        self.steps
            .insert(at, Step::Comment(format!("{WARNING} {}", one_line(text))));
    }

    /// The statements it runs.
    pub(crate) fn statements(&self) -> usize {
        self.steps
            .iter()
            .filter(|s| !matches!(s, Step::Comment(_)))
            .count()
    }

    /// Warns [`ONE_BY_ONE`] when the plan runs more than one statement
    /// outside a transaction: a failure leaves the first ones done.
    pub(crate) fn one_by_one(&mut self) {
        if !self.atomic && self.statements() > 1 {
            self.warn(ONE_BY_ONE);
        }
    }

    pub(crate) fn exec(&mut self, sql: String) {
        self.steps.push(Step::Exec(sql));
    }

    pub(crate) fn query(&mut self, sql: String) {
        self.steps.push(Step::Query(sql));
    }

    pub(crate) fn call_out(&mut self, sql: String, params: Vec<Param>, check: OutCheck) {
        self.steps.push(Step::CallOut(sql, params, check));
    }

    /// It runs something.
    pub(crate) fn changes(&self) -> bool {
        self.steps.iter().any(|s| !matches!(s, Step::Comment(_)))
    }

    /// The script: comments as `-- …` lines, each statement ended by `;`
    /// (or followed by a `go` line).
    pub(crate) fn script(&self) -> String {
        let mut out = String::new();
        for s in &self.steps {
            match s {
                Step::Comment(c) => {
                    out.push_str("-- ");
                    out.push_str(&one_line(c));
                    out.push('\n');
                }
                Step::Exec(sql) | Step::Query(sql) | Step::CallOut(sql, _, _) => {
                    out.push_str(sql);
                    out.push_str(if self.go { "\ngo\n" } else { ";\n" });
                }
            }
        }
        out
    }
}

/// Text on one line, for a comment.
pub(crate) fn one_line(s: &str) -> String {
    s.split(['\r', '\n'])
        .map(str::trim)
        .filter(|l| !l.is_empty())
        .collect::<Vec<_>>()
        .join(" ")
}

/// Where a plan runs: the jobs' queries plus output parameters and the
/// transaction around an atomic plan.
pub(crate) trait Src: JobSource {
    /// A CALL whose `?` markers are `params`, in order: the values of the
    /// input-output and output ones, in order.
    fn call(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Option<String>>>;
    /// Opens a transaction for an atomic plan; false when the engine or
    /// the database has none (the steps then run one by one).
    fn begin(&mut self) -> bool;
    /// Ends what [`Src::begin`] or the session's manual mode opened:
    /// commits when `ok`, rolls back otherwise.
    fn finish(&mut self, ok: bool) -> Result<()>;
}

/// Runs a plan's steps and reads the saved object's id.
pub(crate) fn execute(src: &mut dyn Src, plan: Plan) -> Result<Option<String>> {
    if !plan.changes() {
        return Ok(plan.id);
    }
    if plan.atomic {
        src.begin();
    }
    let ran = run_steps(src, &plan.steps);
    let ended = src.finish(ran.is_ok());
    let given = ran?;
    ended?;
    let id = given.or(plan.id);
    let Some(lookup) = plan.lookup else {
        return Ok(id);
    };
    match lookup(src) {
        Ok(found) => Ok(found.or(id)),
        Err(e) if plan.lookup_required => Err(e),
        Err(e) => {
            tracing::debug!("odbc scheduler: the saved object's id wasn't found: {e}");
            Ok(id)
        }
    }
}

/// Runs the steps; the id the last CALL that gave one gave.
fn run_steps(src: &mut dyn Src, steps: &[Step]) -> Result<Option<String>> {
    let mut given = None;
    for s in steps {
        match s {
            Step::Comment(_) => {}
            Step::Exec(sql) => {
                src.exec(sql, &[])?;
            }
            Step::Query(sql) => {
                src.query(sql, &[])?;
            }
            Step::CallOut(sql, params, check) => {
                let got = src.call(sql, params)?;
                if let Some(id) = check(&got)? {
                    given = Some(id);
                }
            }
        }
    }
    Ok(given)
}

/// The session's connection, for a save: [`crate::ConnSource`] with the
/// session's transaction mode.
pub(crate) struct Conn<'a> {
    src: crate::ConnSource<'a>,
    /// The session runs with autocommit off: everything is already in a
    /// transaction, which the save commits.
    manual: bool,
    /// [`Src::begin`] turned autocommit off.
    opened: bool,
}

impl<'a> Conn<'a> {
    pub(crate) fn new(src: crate::ConnSource<'a>, manual: bool) -> Self {
        Conn {
            src,
            manual,
            opened: false,
        }
    }
}

impl JobSource for Conn<'_> {
    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Set> {
        self.src.query(sql, params)
    }

    fn exec(&mut self, sql: &str, params: &[&str]) -> Result<Option<u64>> {
        self.src.exec(sql, params)
    }
}

impl Src for Conn<'_> {
    fn call(&mut self, sql: &str, params: &[Param]) -> Result<Vec<Option<String>>> {
        use crate::odbc::CallParam;
        let bound: Vec<CallParam> = params
            .iter()
            .map(|p| match p {
                Param::In(v) => CallParam::In(v.as_deref()),
                Param::InOut(v) => CallParam::InOut(v.as_deref()),
                Param::Out => CallParam::Out,
            })
            .collect();
        let st = self.src.c.stmt(self.src.slot)?;
        st.exec_call(sql, &bound)
    }

    fn begin(&mut self) -> bool {
        if self.manual {
            return true;
        }
        match self.src.c.set_autocommit(false) {
            Ok(()) => {
                self.opened = true;
                true
            }
            Err(e) => {
                tracing::debug!("odbc scheduler: autocommit stays on for the save: {e}");
                false
            }
        }
    }

    fn finish(&mut self, ok: bool) -> Result<()> {
        let ended = if self.manual || self.opened {
            self.src.c.end_tran(ok)
        } else {
            Ok(())
        };
        if self.opened {
            self.opened = false;
            if let Err(e) = self.src.c.set_autocommit(true) {
                tracing::debug!("odbc scheduler: could not turn autocommit back on: {e}");
            }
        }
        ended
    }
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

    /// The value as typed (code keeps its spaces).
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

    /// A yes/no; `default` when the form didn't send it.
    pub(crate) fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            None => default,
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => matches!(
                s.trim().to_ascii_lowercase().as_str(),
                "true" | "1" | "yes" | "on" | "t" | "y"
            ),
            Some(_) => false,
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

    /// A multi-select's values, trimmed, without empties.
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
            Some(Value::String(s)) if !s.trim().is_empty() => s
                .split(',')
                .map(|p| p.trim().to_string())
                .filter(|p| !p.is_empty())
                .collect(),
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

pub(crate) fn bad(message: impl Into<String>) -> Error {
    Error::Query(message.into())
}

/// Text from the app, short and on one line, for a message.
pub(crate) fn short(s: &str) -> String {
    one_line(s).chars().take(60).collect()
}

/// A text field that has to be there: trimmed, at most `max` characters,
/// no control characters.
pub(crate) fn required(v: Vals, k: &str, label: &str, max: usize) -> Result<String> {
    match optional(v, k, label, max)? {
        Some(t) => Ok(t),
        None => Err(bad(format!("Falta «{label}»."))),
    }
}

/// An optional text field on one line: trimmed, at most `max` characters,
/// no control characters.
pub(crate) fn optional(v: Vals, k: &str, label: &str, max: usize) -> Result<Option<String>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    if t.chars().any(char::is_control) {
        return Err(bad(format!(
            "«{label}» no puede tener saltos de línea ni caracteres de control."
        )));
    }
    if t.chars().count() > max {
        return Err(bad(format!("«{label}» admite hasta {max} caracteres.")));
    }
    Ok(Some(t))
}

/// Code or a long text: as typed but without blank lines around it, at
/// most `max` bytes, no control characters but line breaks and tabs.
pub(crate) fn code_text(v: Vals, k: &str, label: &str, max: usize) -> Result<Option<String>> {
    let raw = v.raw(k);
    let t = raw.trim_matches(|c: char| c.is_whitespace());
    if t.is_empty() {
        return Ok(None);
    }
    if t.chars()
        .any(|c| c.is_control() && !matches!(c, '\n' | '\r' | '\t'))
    {
        return Err(bad(format!("«{label}» tiene caracteres de control.")));
    }
    if t.len() > max {
        return Err(bad(format!("«{label}» admite hasta {max} bytes.")));
    }
    Ok(Some(t.to_string()))
}

/// One of `options` (the select's values).
pub(crate) fn choice(
    v: Vals,
    k: &str,
    label: &str,
    options: &[&str],
    default: &str,
) -> Result<String> {
    let t = v.opt(k).unwrap_or_else(|| default.to_string());
    options
        .iter()
        .find(|o| o.eq_ignore_ascii_case(&t))
        .map(|o| o.to_string())
        .ok_or_else(|| {
            bad(format!(
                "«{label}»: «{}» no es una opción válida.",
                short(&t)
            ))
        })
}

/// A date and time as the form writes it (`YYYY-MM-DDTHH:MM[:SS]`, the
/// server's local time), as ms read as if it were UTC.
pub(crate) fn datetime(v: Vals, k: &str, label: &str) -> Result<Option<i64>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    let date_part_ok = t.len() >= 10
        && t.as_bytes()[4] == b'-'
        && t.as_bytes()[7] == b'-'
        && t.get(..10).is_some_and(|d| {
            d.bytes()
                .enumerate()
                .all(|(i, b)| b.is_ascii_digit() || i == 4 || i == 7)
        });
    let ok = date_part_ok
        && jobs::parse_ts(&t).is_some_and(|ms| valid_day(&t[..10], ms))
        && t.len() <= 35;
    if !ok {
        return Err(bad(format!(
            "«{label}»: «{}» no es una fecha y hora válida (AAAA-MM-DD HH:MM:SS).",
            short(&t)
        )));
    }
    Ok(jobs::parse_ts(&t))
}

/// A date (`YYYY-MM-DD`, or the date of a date and time) as ms at midnight.
pub(crate) fn date(v: Vals, k: &str, label: &str) -> Result<Option<i64>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    parse_date(&t).map(Some).ok_or_else(|| {
        bad(format!(
            "«{label}»: «{}» no es una fecha válida (AAAA-MM-DD).",
            short(&t)
        ))
    })
}

/// `YYYY-MM-DD` (possibly followed by a time) as ms at midnight.
pub(crate) fn parse_date(t: &str) -> Option<i64> {
    let t = t.trim();
    let d = t.get(..10)?;
    if t.len() > 10 && !t[10..].starts_with(['T', ' ']) {
        return None;
    }
    let shape = d.bytes().enumerate().all(|(i, b)| match i {
        4 | 7 => b == b'-',
        _ => b.is_ascii_digit(),
    });
    if !shape {
        return None;
    }
    let ms = jobs::parse_ts(d)?;
    valid_day(d, ms).then_some(ms)
}

/// The date `d` (`YYYY-MM-DD`) is the day `ms` falls on (no 31 April).
fn valid_day(d: &str, ms: i64) -> bool {
    jobs::rfc3339(ms).get(..10) == Some(d)
}

/// A time as the form writes it (`HH:MM[:SS]`), as seconds after midnight.
pub(crate) fn time(v: Vals, k: &str, label: &str) -> Result<Option<i64>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    let parts: Vec<&str> = t.split(':').collect();
    let num = |p: &str| -> Option<i64> {
        (!p.is_empty() && p.len() <= 2 && p.bytes().all(|b| b.is_ascii_digit()))
            .then(|| p.parse().ok())
            .flatten()
    };
    let parsed = (2..=3).contains(&parts.len()).then(|| {
        let h = num(parts[0])?;
        let m = num(parts[1])?;
        let s = match parts.get(2) {
            Some(p) => num(p)?,
            None => 0,
        };
        (h < 24 && m < 60 && s < 60).then_some(h * 3600 + m * 60 + s)
    });
    parsed.flatten().map(Some).ok_or_else(|| {
        bad(format!(
            "«{label}»: «{}» no es una hora válida (HH:MM:SS).",
            short(&t)
        ))
    })
}

/// Seconds after midnight as `HH:MM:SS`.
pub(crate) fn time_text(secs: i64) -> String {
    format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60)
}

/// ms as the form's date and time (`YYYY-MM-DDTHH:MM:SS`, no zone).
pub(crate) fn dt_value(ms: i64) -> String {
    let iso = jobs::rfc3339(ms);
    iso[..19].to_string()
}

/// ms as the form's date (`YYYY-MM-DD`).
pub(crate) fn date_value(ms: i64) -> String {
    jobs::rfc3339(ms)[..10].to_string()
}

/// A cron expression of `fields` fields (5 for UNIX cron, 6 or 7 for
/// Quartz): one line, fields of digits, names and `* , - / ? L W #`.
pub(crate) fn cron(
    v: Vals,
    k: &str,
    label: &str,
    fields: std::ops::RangeInclusive<usize>,
) -> Result<Option<String>> {
    let Some(t) = v.opt(k) else {
        return Ok(None);
    };
    let parts: Vec<&str> = t.split_whitespace().collect();
    let ok = fields.contains(&parts.len())
        && t.len() <= 400
        && parts.iter().all(|p| {
            p.bytes().all(|b| {
                b.is_ascii_alphanumeric() || matches!(b, b'*' | b',' | b'-' | b'/' | b'?' | b'#')
            })
        });
    if !ok {
        let n = if fields.start() == fields.end() {
            fields.start().to_string()
        } else {
            format!("{} o {}", fields.start(), fields.end())
        };
        return Err(bad(format!(
            "«{label}»: «{}» no es una expresión cron válida ({n} campos separados por espacios).",
            short(&t)
        )));
    }
    Ok(Some(parts.join(" ")))
}

/// The form's values map from pairs.
pub(crate) fn values(pairs: Vec<(&str, Value)>) -> BTreeMap<String, Value> {
    pairs.into_iter().map(|(k, v)| (k.to_string(), v)).collect()
}

/// A JSON value for an optional text.
pub(crate) fn jtext(v: Option<&str>) -> Value {
    v.map_or(Value::Null, |s| Value::String(s.to_string()))
}

/// The same object again: `a` and `b` equal as texts (both trimmed, empty
/// as nothing).
pub(crate) fn same(a: &Option<String>, b: &Option<String>) -> bool {
    let n = |x: &Option<String>| {
        x.as_deref()
            .map(str::trim)
            .filter(|t| !t.is_empty())
            .map(str::to_string)
    };
    n(a) == n(b)
}
