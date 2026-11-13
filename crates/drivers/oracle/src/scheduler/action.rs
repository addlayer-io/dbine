//! What the Scheduler's objects offer besides being edited and deleted
//! ([`dbine_driver::SchedulerAction`]), each one a block the user sees
//! first and confirms:
//!
//! - Windows: open one now for its own duration (`OPEN_WINDOW`; only one
//!   window is open at a time, so the one open now is closed, `force =>
//!   TRUE`), or close it before its time (`CLOSE_WINDOW`). Both take
//!   MANAGE SCHEDULER; Oracle doesn't open a disabled window.
//! - Programs and chains: run once now, in a run-once job in the login's
//!   schema that Oracle drops after it ends: `CREATE_JOB` of the program
//!   (named by `GENERATE_JOB_NAME`, `RUN_PROG$…`), `RUN_CHAIN` from the
//!   chain's start (its own `RUN_CHAIN$…`). A program runs so only when it
//!   needs no credential (PL/SQL block, stored procedure) and each of its
//!   arguments has a value of its own. Takes CREATE JOB, and EXECUTE on
//!   another user's program or chain (or EXECUTE ANY PROGRAM).
//! - Jobs: stop a running one by force (`STOP_JOB` with `force => TRUE`),
//!   for when the jobs list's "Detener" didn't. Oracle asks only for
//!   MANAGE SCHEDULER, whoever owns the job: Oracle's own too (a runaway
//!   maintenance job is what it's for), but on Autonomous Database, which
//!   locks them down. And end the run of a job whose program is detached
//!   (`END_DETACHED_JOB_RUN`, without error), where the login manages the
//!   job. Running a job now, its graceful stop, enabling and disabling it
//!   are the jobs list's ([`crate::jobs`]).
//!
//! - Jobs running a chain (the two ask for a step first, from the chain's
//!   steps as `*_SCHEDULER_RUNNING_CHAINS` has them now; the steps of a
//!   nested chain aren't listed): steer one step (`ALTER_RUNNING_CHAIN`:
//!   its state to SUCCEEDED, FAILED with an error number or NOT_STARTED,
//!   SKIP, PAUSE, PAUSE_BEFORE, or its pause taken off), or stop one step
//!   that is running (`STOP_JOB` of `"OWNER"."JOB"."STEP"`, by force with
//!   MANAGE SCHEDULER). Both where the login manages the job (its owner,
//!   ALTER on it or CREATE ANY JOB). Oracle doesn't change the state of a
//!   step that is running, and evaluates the chain's rules again after a
//!   step's state changes or its pause is taken off: other steps may
//!   start, or the chain may end, stopping the steps still running (the
//!   script warns so).
//!
//! Oracle's own programs and chains run from their own jobs.

use super::read::{self, Rec};
use super::script::{self, Block, A};
use super::{bad, parse_obj, program, shown, window, Ctx, Kind, Vals, OWNER_ONLY};
use crate::jobs::{self, JobId};
use dbine_driver::{
    Error, FormField, FormKind, FormSection, FormValues, ObjectForm, Result, SchedulerAction,
};
use oracledb::Connection;
use std::collections::{BTreeMap, HashSet};

pub(crate) const RUN: &str = "run";
pub(crate) const OPEN_WINDOW: &str = "open_window";
pub(crate) const CLOSE_WINDOW: &str = "close_window";
pub(crate) const FORCE_STOP: &str = "force_stop";
pub(crate) const END_DETACHED: &str = "end_detached";
pub(crate) const STEER_CHAIN: &str = "steer_chain";
pub(crate) const STOP_STEP: &str = "stop_step";

const NEEDS_MANAGE: &str = "Esta acción requiere el privilegio MANAGE SCHEDULER.";
const NEEDS_CREATE_JOB: &str =
    "Ejecutarlo ahora crea un job de una sola vez: hace falta el privilegio CREATE JOB.";
const ORACLE_RUNS: &str =
    "Es un objeto de Oracle: lo ejecutan sus propios jobs, no se ejecuta aparte.";
const CLOUD_ORACLE_JOB: &str = "Es un job de Oracle: Oracle Autonomous Database no deja detenerlo.";

/// Jobs running now whose program is detached: they stay running until
/// someone ends their run.
const DETACHED_RUNS: &str = "SELECT j.owner, j.job_name FROM {v}_scheduler_jobs j
  JOIN {v}_scheduler_programs p ON p.owner = j.program_owner AND p.program_name = j.program_name
 WHERE p.detached = 'TRUE' AND j.state = 'RUNNING'";

/// Every action a kind has, whatever the login may do.
fn known(kind: Kind, action: &str) -> bool {
    matches!(
        (kind, action),
        (
            Kind::Job,
            FORCE_STOP | END_DETACHED | STEER_CHAIN | STOP_STEP
        ) | (Kind::Program | Kind::Chain, RUN)
            | (Kind::Window, OPEN_WINDOW | CLOSE_WINDOW)
    )
}

/// The kind's actions this login may use (labels as literals: the
/// message catalog finds them by `SchedulerAction::new`).
pub(crate) fn of(ctx: &Ctx, kind: Kind) -> Vec<SchedulerAction> {
    let mut out = Vec::new();
    match kind {
        Kind::Job => {
            if ctx.manage() {
                out.push(SchedulerAction::new("force_stop", "Forzar la detención"));
            }
            out.push(SchedulerAction::new(
                "end_detached",
                "Dar por terminada la ejecución",
            ));
            out.push(
                SchedulerAction::new("steer_chain", "Cambiar un paso de la cadena en curso")
                    .with_params(),
            );
            out.push(SchedulerAction::new("stop_step", "Detener un paso en curso").with_params());
        }
        Kind::Program | Kind::Chain if ctx.may_create(Kind::Job) => {
            out.push(SchedulerAction::new("run", "Ejecutar ahora"));
        }
        Kind::Window if ctx.manage() => {
            out.push(SchedulerAction::new("open_window", "Abrir ahora"));
            out.push(SchedulerAction::new("close_window", "Cerrar ahora"));
        }
        _ => {}
    }
    out
}

/// `action`, checked: one of the kind's, that this login may use.
fn checked(ctx: &Ctx, kind: Kind, action: &str) -> Result<()> {
    if !known(kind, action) {
        return Err(Error::Unsupported(format!(
            "«{}» no es una acción de este tipo de objeto del Scheduler de Oracle",
            shown(action)
        )));
    }
    if !of(ctx, kind).iter().any(|a| a.key == action) {
        return Err(bad(match kind {
            Kind::Program | Kind::Chain => NEEDS_CREATE_JOB,
            _ => NEEDS_MANAGE,
        }
        .into()));
    }
    Ok(())
}

/// The block of `action` on the object `id`, checked against what the
/// server has now (the item's actions said what it offered when listed);
/// `values`, what its form asked for (empty for the others).
pub(crate) fn block(
    c: &Connection,
    ctx: &Ctx,
    kind: Kind,
    id: &str,
    action: &str,
    values: &FormValues,
) -> Result<Block> {
    let action = action.trim();
    checked(ctx, kind, action)?;
    match (kind, action) {
        (Kind::Window, OPEN_WINDOW) => open_window(c, ctx, id),
        (Kind::Window, _) => close_window(c, ctx, id),
        (Kind::Job, FORCE_STOP) => force_stop(c, ctx, id),
        (Kind::Job, STEER_CHAIN) => steer_chain(c, ctx, id, values),
        (Kind::Job, STOP_STEP) => stop_step(c, ctx, id, values),
        (Kind::Job, _) => end_detached(c, ctx, id),
        (Kind::Program, _) => run_program(c, ctx, id),
        _ => run_chain(c, ctx, id),
    }
}

/// The form of an action that asks for values: a step of the chain the
/// job runs now.
pub(crate) fn form(
    c: &Connection,
    ctx: &Ctx,
    kind: Kind,
    id: &str,
    action: &str,
) -> Result<ObjectForm> {
    let action = action.trim();
    checked(ctx, kind, action)?;
    match (kind, action) {
        (Kind::Job, STEER_CHAIN) => {
            let run = running_chain(c, ctx, id)?;
            Ok(steer_form(&run))
        }
        (Kind::Job, STOP_STEP) => {
            let run = running_chain(c, ctx, id)?;
            stop_form(&run, ctx.manage())
        }
        _ => Err(Error::Unsupported(format!(
            "«{}» no pide valores: se ejecuta tal cual",
            shown(action)
        ))),
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

/// The running jobs that stop when the window `:1` closes: their schedule
/// is the window or a window group it's in, and they have «Detener al
/// cerrar la ventana».
const STOPPING_ON_CLOSE: &str = "SELECT owner, job_name FROM {v}_scheduler_jobs
 WHERE state = 'RUNNING' AND stop_on_window_close = 'TRUE'
   AND schedule_type IN ('WINDOW', 'WINDOW_ONCE', 'WINDOW_GROUP')
   AND (schedule_name = :1 OR schedule_name IN
        (SELECT window_group_name FROM {v}_scheduler_wingroup_members WHERE window_name = :2))
 ORDER BY 1, 2";

/// The running jobs that stop when the window `name` closes.
fn stopping_on_close(c: &Connection, ctx: &Ctx, name: &str) -> Vec<String> {
    super::affected(c, ctx, STOPPING_ON_CLOSE, &[&name, &name])
}

/// Warns of the running jobs that stop when a window closes, if any.
fn warn_stopping(b: &mut Block, stopping: &[String]) {
    if !stopping.is_empty() {
        b.warn(&format!(
            "Se detienen ahora estos jobs, que se están ejecutando con «Detener al cerrar la ventana»: {}.",
            super::listed(stopping)
        ));
    }
}

fn open_window(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let name = window::sys_name(id, "ventana")?;
    let (_, r) = window::read_state(c, ctx, &name)?;
    let target = window::sys(&name);
    if r.yes("ACTIVE") {
        return Err(bad(format!("La ventana {target} ya está abierta.")));
    }
    if !r.yes("ENABLED") {
        return Err(bad(format!(
            "La ventana {target} está deshabilitada: Oracle no abre una ventana deshabilitada. Habilitala primero."
        )));
    }
    // Only one window is open at a time: the one open now closes.
    let open = read::rows(
        c,
        &ctx.sql("{v}_scheduler_windows"),
        "WHERE active = 'TRUE'",
        &[],
        10,
    )
    .map_err(crate::err)?
    .iter()
    .filter_map(|w| w.raw("WINDOW_NAME"))
    .find(|w| *w != name);
    let duration = u64::try_from(r.int("DURATION").unwrap_or(0)).unwrap_or(0);
    let closing = open.map(|o| {
        let stopping = stopping_on_close(c, ctx, &o);
        (o, stopping)
    });
    Ok(open_block(&target, duration, closing.as_ref()))
}

/// Opens the window `target` (SYS's) for its `duration` (seconds),
/// closing the window open now, with the jobs that stop with it, if any.
pub(crate) fn open_block(
    target: &str,
    duration: u64,
    closing: Option<&(String, Vec<String>)>,
) -> Block {
    let mut b = Block::new(&format!(
        "Abre la ventana {target} ahora, por su duración ({}).",
        jobs::duration_text(duration * 1000)
    ));
    b.note("Mientras está abierta rige su plan de recursos y pueden correr los jobs que la usan.");
    if let Some((o, stopping)) = closing {
        b.warn(&format!(
            "Solo puede haber una ventana abierta: cierra la que está abierta ahora, {} (force => TRUE).",
            window::sys(o)
        ));
        warn_stopping(&mut b, stopping);
        // OPEN_WINDOW with force => TRUE, as Oracle documents it.
        b.warn("Mientras esté abierta, Oracle no abre ninguna otra ventana programada, aunque tenga más prioridad.");
    }
    b.call(
        "DBMS_SCHEDULER.OPEN_WINDOW",
        vec![
            ("window_name", A::Name(target.to_string())),
            ("duration", A::Null),
            ("force", A::B(closing.is_some())),
        ],
    );
    b
}

fn close_window(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let name = window::sys_name(id, "ventana")?;
    let (_, r) = window::read_state(c, ctx, &name)?;
    let target = window::sys(&name);
    if !r.yes("ACTIVE") {
        return Err(bad(format!("La ventana {target} no está abierta.")));
    }
    Ok(close_block(&target, &stopping_on_close(c, ctx, &name)))
}

/// Closes the window `target` (SYS's) now; `stopping`: the running jobs
/// that stop with it.
pub(crate) fn close_block(target: &str, stopping: &[String]) -> Block {
    let mut b = Block::new(&format!(
        "Cierra la ventana {target} ahora, antes de su hora."
    ));
    b.note("Vuelve el plan de recursos de antes; los jobs de la ventana que tienen «Detener al cerrar la ventana» se detienen.");
    warn_stopping(&mut b, stopping);
    b.call(
        "DBMS_SCHEDULER.CLOSE_WINDOW",
        vec![("window_name", A::Name(target.to_string()))],
    );
    b
}

// ---------------------------------------------------------------------------
// Jobs
// ---------------------------------------------------------------------------

/// The ids ([`jobs::job_id`]) of the jobs running a detached program now.
pub(crate) fn detached_runs(c: &Connection, ctx: &Ctx) -> HashSet<String> {
    match jobs::texts(c, &ctx.sql(DETACHED_RUNS), &[], 10_000) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| Some(jobs::job_id(&jobs::col(r, 0)?, &jobs::col(r, 1)?)))
            .collect(),
        Err(e) => {
            tracing::debug!("oracle scheduler: detached runs: {e}");
            HashSet::new()
        }
    }
}

/// A DBMS_SCHEDULER job (not a DBMS_JOB one: those stop with the session
/// that runs them) as (owner, name, its state, whether it's Oracle's
/// own).
fn scheduler_job(c: &Connection, ctx: &Ctx, id: &str) -> Result<(String, String, String, bool)> {
    if let Ok(JobId::Legacy(_)) = jobs::parse_id(id) {
        return Err(Error::Unsupported(
            "un job de DBMS_JOB no se puede detener ni dar por terminado: hay que terminar la sesión que lo ejecuta"
                .into(),
        ));
    }
    let (owner, name) = parse_obj(id, "job")?;
    let st = super::job::read_state(c, ctx, &owner, &name)?;
    Ok((
        owner,
        name,
        st.rec.t("STATE").unwrap_or_default(),
        st.system,
    ))
}

/// A job the login manages (as the jobs list's "Detener" asks), with its
/// id and state.
fn running_job(c: &Connection, ctx: &Ctx, id: &str) -> Result<(String, String)> {
    let (owner, name, state, system) = scheduler_job(c, ctx, id)?;
    if !ctx.may_change(Kind::Job, &owner, &name, system) {
        return Err(bad(OWNER_ONLY.into()));
    }
    Ok((jobs::job_id(&owner, &name), state))
}

/// The job `id` (Oracle's own when `system`) can be stopped by force:
/// `STOP_JOB` with `force => TRUE` takes MANAGE SCHEDULER, whoever owns
/// the job, but Autonomous Database locks Oracle's own jobs down. Not a
/// DBMS_JOB job, nor a name `DBMS_SCHEDULER` can't take.
pub(crate) fn force_stoppable(ctx: &Ctx, id: &str, system: bool) -> bool {
    ctx.manage() && parse_obj(id, "job").is_ok() && !(ctx.cloud && system)
}

fn force_stop(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name, state, system) = scheduler_job(c, ctx, id)?;
    if !ctx.manage() {
        return Err(bad(NEEDS_MANAGE.into()));
    }
    if ctx.cloud && system {
        return Err(bad(CLOUD_ORACLE_JOB.into()));
    }
    let job = jobs::job_id(&owner, &name);
    if !matches!(state.as_str(), "RUNNING" | "CHAIN_STALLED") {
        return Err(bad(format!("El job {job} no se está ejecutando.")));
    }
    let mut b = Block::new(&format!(
        "Detiene el job {job} en el acto (STOP_JOB con force => TRUE)."
    ));
    b.note("Oracle termina el proceso que lo ejecuta sin esperar a que se detenga solo: es para cuando «Detener» no alcanzó.");
    b.call(
        "DBMS_SCHEDULER.STOP_JOB",
        vec![("job_name", A::Name(job)), ("force", A::B(true))],
    );
    Ok(b)
}

fn end_detached(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (job, _) = running_job(c, ctx, id)?;
    if !detached_runs(c, ctx).contains(&job) {
        return Err(bad(format!(
            "El job {job} no está ejecutando un programa desacoplado: no hay una ejecución que dar por terminada."
        )));
    }
    let mut b = Block::new(&format!(
        "Da por terminada la ejecución en curso del job {job}, que ejecuta un programa desacoplado."
    ));
    b.note("Oracle la registra como terminada sin error (error_number => 0).");
    b.call(
        "DBMS_SCHEDULER.END_DETACHED_JOB_RUN",
        vec![("job_name", A::Name(job)), ("error_number", A::I(0))],
    );
    Ok(b)
}

// ---------------------------------------------------------------------------
// Running chains: steer a step, stop a step
// ---------------------------------------------------------------------------

/// The jobs (ids) whose chain has a step running now, at its top level.
const STEPS_RUNNING: &str = "SELECT DISTINCT owner, job_name FROM {v}_scheduler_running_chains
 WHERE state = 'RUNNING' AND job_subname IS NULL";

/// What Oracle says after a change of a running chain, said in terms of
/// the action (the server's message kept under it).
const RULES_AGAIN: &str = "Oracle evalúa de nuevo las reglas de la cadena: pueden empezar otros pasos, o terminar la cadena y con ella los pasos que sigan ejecutándose.";

/// The ids ([`jobs::job_id`]) of the jobs whose chain has a step running.
pub(crate) fn steps_running(c: &Connection, ctx: &Ctx) -> HashSet<String> {
    match jobs::texts(c, &ctx.sql(STEPS_RUNNING), &[], 10_000) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| Some(jobs::job_id(&jobs::col(r, 0)?, &jobs::col(r, 1)?)))
            .collect(),
        Err(e) => {
            tracing::debug!("oracle scheduler: chain steps running: {e}");
            HashSet::new()
        }
    }
}

/// One step of a running chain, as `*_SCHEDULER_RUNNING_CHAINS` has it.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct RunStep {
    pub(crate) name: String,
    /// NOT_STARTED, SCHEDULED, RUNNING, PAUSED, STALLED, SUCCEEDED,
    /// FAILED, STOPPED…
    pub(crate) state: String,
    pub(crate) error: Option<i64>,
    pub(crate) skip: bool,
    pub(crate) pause: bool,
    /// `None`: the version has no `PAUSE_BEFORE` (before 12.1).
    pub(crate) pause_before: Option<bool>,
}

impl RunStep {
    /// `with_before`: the view has `PAUSE_BEFORE`.
    pub(crate) fn of(r: &Rec, with_before: bool) -> Option<RunStep> {
        Some(RunStep {
            name: r.raw("STEP_NAME").filter(|n| !n.is_empty())?,
            state: r.t("STATE").unwrap_or_default(),
            error: r.int("ERROR_CODE"),
            skip: r.yes("SKIP"),
            pause: r.yes("PAUSE"),
            pause_before: with_before.then(|| r.yes("PAUSE_BEFORE")),
        })
    }

    fn running(&self) -> bool {
        self.state == "RUNNING"
    }

    fn not_started(&self) -> bool {
        self.state == "NOT_STARTED"
    }

    /// As the form lists it: the step, its state and its flags (Oracle's
    /// words).
    fn label(&self) -> String {
        let mut out = format!("{} — {}", self.name, self.state);
        if let Some(e) = self.error.filter(|e| *e != 0) {
            out.push_str(&format!(" ({e})"));
        }
        let flags: Vec<&str> = [
            (self.skip, "SKIP"),
            (self.pause, "PAUSE"),
            (self.pause_before == Some(true), "PAUSE_BEFORE"),
        ]
        .iter()
        .filter(|(on, _)| *on)
        .map(|(_, f)| *f)
        .collect();
        if !flags.is_empty() {
            out.push_str(&format!(" · {}", flags.join(", ")));
        }
        out
    }
}

/// A job running a chain now, with the steps of its chain.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct RunningChain {
    pub(crate) owner: String,
    pub(crate) name: String,
    pub(crate) steps: Vec<RunStep>,
}

impl RunningChain {
    pub(crate) fn job(&self) -> String {
        jobs::job_id(&self.owner, &self.name)
    }

    /// The step the form picked: by its exact name, else by one that
    /// differs only in case.
    fn step(&self, picked: &str) -> Result<&RunStep> {
        let t = picked.trim();
        self.steps
            .iter()
            .find(|s| s.name == picked)
            .or_else(|| self.steps.iter().find(|s| s.name.eq_ignore_ascii_case(t)))
            .ok_or_else(|| {
                if t.is_empty() {
                    bad("Falta «Paso».".into())
                } else {
                    bad(format!(
                        "La cadena que ejecuta el job {} no tiene un paso llamado «{}».",
                        self.job(),
                        shown(picked)
                    ))
                }
            })
    }
}

/// The job `id`, which the login manages, running a chain now.
fn running_chain(c: &Connection, ctx: &Ctx, id: &str) -> Result<RunningChain> {
    let (owner, name, state, system) = scheduler_job(c, ctx, id)?;
    if !ctx.may_change(Kind::Job, &owner, &name, system) {
        return Err(bad(OWNER_ONLY.into()));
    }
    let job = jobs::job_id(&owner, &name);
    let not_running = || bad(format!("El job {job} no está ejecutando una cadena ahora."));
    if !matches!(state.as_str(), "RUNNING" | "CHAIN_STALLED") {
        return Err(not_running());
    }
    let view = ctx.sql("{v}_scheduler_running_chains");
    let with_before = read::columns(c, &view)
        .map_err(crate::err)?
        .contains("PAUSE_BEFORE");
    let steps: Vec<RunStep> = read::rows(
        c,
        &view,
        "WHERE owner = :1 AND job_name = :2 AND job_subname IS NULL ORDER BY step_name",
        &[&owner, &name],
        1000,
    )
    .map_err(crate::err)?
    .iter()
    .filter_map(|r| RunStep::of(r, with_before))
    .collect();
    if steps.is_empty() {
        return Err(not_running());
    }
    Ok(RunningChain { owner, name, steps })
}

/// The steps a form picks from: `(name, label)`.
fn step_choices<'a>(
    steps: impl Iterator<Item = &'a RunStep>,
) -> BTreeMap<String, Vec<(String, String)>> {
    BTreeMap::from([(
        "step".to_string(),
        steps.map(|s| (s.name.clone(), s.label())).collect(),
    )])
}

/// "Cambiar un paso de la cadena en curso": the step and the change.
pub(crate) fn steer_form(run: &RunningChain) -> ObjectForm {
    let before = run.steps.iter().any(|s| s.pause_before.is_some());
    let changes = if before {
        FormKind::select(&[
            ("succeeded", "Darlo por terminado bien (STATE = SUCCEEDED)"),
            ("failed", "Darlo por fallido (STATE = FAILED)"),
            (
                "not_started",
                "Volverlo a «sin empezar» (STATE = NOT_STARTED)",
            ),
            ("skip", "Saltearlo cuando le toque (SKIP)"),
            ("unskip", "Dejar de saltearlo (SKIP = FALSE)"),
            ("pause", "Pausar la cadena cuando este paso termine (PAUSE)"),
            (
                "pause_before",
                "Pausar la cadena antes de este paso (PAUSE_BEFORE)",
            ),
            ("resume", "Quitarle la pausa"),
        ])
    } else {
        FormKind::select(&[
            ("succeeded", "Darlo por terminado bien (STATE = SUCCEEDED)"),
            ("failed", "Darlo por fallido (STATE = FAILED)"),
            (
                "not_started",
                "Volverlo a «sin empezar» (STATE = NOT_STARTED)",
            ),
            ("skip", "Saltearlo cuando le toque (SKIP)"),
            ("unskip", "Dejar de saltearlo (SKIP = FALSE)"),
            ("pause", "Pausar la cadena cuando este paso termine (PAUSE)"),
            ("resume", "Quitarle la pausa"),
        ])
    };
    ObjectForm {
        sections: vec![FormSection::new("step", "Paso")
            .field(FormField::new("step", "Paso", FormKind::reference("step", false)).required())
            .field(FormField::new("change", "Cambio", changes).required())
            .field(
                FormField::new(
                    "error_code",
                    "Número de error",
                    FormKind::number(Some(1.0), Some(f64::from(i32::MAX))),
                )
                .required()
                .when("change", &["failed"])
                .help("El error con que queda registrado el paso (un número distinto de 0)."),
            )],
        choices: step_choices(run.steps.iter()),
        notes: vec![
            format!(
                "Los pasos de la cadena que ejecuta el job {} ahora, con su estado (los de una cadena anidada no se listan).",
                run.job()
            ),
            RULES_AGAIN.to_string(),
        ],
        ..ObjectForm::default()
    }
}

/// "Detener un paso en curso": one of the steps running now; by force
/// with MANAGE SCHEDULER (`manage`).
pub(crate) fn stop_form(run: &RunningChain, manage: bool) -> Result<ObjectForm> {
    let running: Vec<&RunStep> = run.steps.iter().filter(|s| s.running()).collect();
    if running.is_empty() {
        return Err(bad(format!(
            "Ningún paso de la cadena que ejecuta el job {} se está ejecutando ahora.",
            run.job()
        )));
    }
    let mut section = FormSection::new("step", "Paso")
        .field(FormField::new("step", "Paso", FormKind::reference("step", false)).required());
    if manage {
        section = section.field(
            FormField::new("force", "Forzar (force => TRUE)", FormKind::Bool)
                .default(serde_json::json!(false))
                .help("Oracle termina el proceso del paso sin esperar a que se detenga solo."),
        );
    }
    Ok(ObjectForm {
        sections: vec![section],
        choices: step_choices(running.into_iter()),
        notes: vec![format!(
            "Los pasos de la cadena que ejecuta el job {} que se están ejecutando ahora.",
            run.job()
        )],
        ..ObjectForm::default()
    })
}

/// The step as `ALTER_RUNNING_CHAIN` takes it: a plain name as it is, any
/// other between double quotes (unquoted, Oracle folds it to upper case).
fn step_arg(name: &str) -> String {
    if jobs::plain(name) {
        name.to_string()
    } else {
        crate::quote(name)
    }
}

/// A step `DBMS_SCHEDULER` can name: its calls take lists separated by
/// commas.
fn nameable(step: &RunStep) -> Result<()> {
    if step.name.contains(',') || !jobs::quotable(&step.name) {
        return Err(bad(format!(
            "El paso «{}» tiene una coma o comillas en el nombre: DBMS_SCHEDULER no lo puede nombrar.",
            shown(&step.name)
        )));
    }
    Ok(())
}

fn steer_chain(c: &Connection, ctx: &Ctx, id: &str, values: &FormValues) -> Result<Block> {
    let run = running_chain(c, ctx, id)?;
    steer_block(&run, Vals(&values.values))
}

/// The change the form asked for, on the step it picked.
pub(crate) fn steer_block(run: &RunningChain, v: Vals) -> Result<Block> {
    let job = run.job();
    let step = run.step(&v.raw("step"))?;
    nameable(step)?;
    let n = &step.name;
    let change = v.text("change");
    let state_change = matches!(change.as_str(), "succeeded" | "failed" | "not_started");
    if state_change && step.running() {
        return Err(bad(format!(
            "El paso {n} se está ejecutando: Oracle no cambia el estado de un paso en ejecución (se detiene con «Detener un paso en curso»)."
        )));
    }
    // (attribute, value) of each call, and what the script says.
    let (calls, what): (Vec<(&str, A)>, String) = match change.as_str() {
        "succeeded" => (
            vec![("STATE", A::S("SUCCEEDED".into()))],
            "lo da por terminado bien (SUCCEEDED)".into(),
        ),
        "failed" => {
            let code = v
                .int("error_code", "Número de error", 1, i64::from(i32::MAX))?
                .ok_or_else(|| bad("Falta «Número de error».".into()))?;
            (
                vec![("STATE", A::S(format!("FAILED {code}")))],
                format!("lo da por fallido, con el error {code}"),
            )
        }
        "not_started" if step.not_started() => {
            return Err(bad(format!("El paso {n} todavía no empezó.")));
        }
        "not_started" => (
            vec![("STATE", A::S("NOT_STARTED".into()))],
            "lo vuelve a «sin empezar» (NOT_STARTED)".into(),
        ),
        "skip" if step.skip => {
            return Err(bad(format!("El paso {n} ya está marcado para saltearse.")));
        }
        "skip" if !step.not_started() => {
            return Err(bad(format!(
                "SKIP solo cambia un paso que todavía no empezó, y {n} está en {}.",
                step.state
            )));
        }
        "skip" => (
            vec![("SKIP", A::B(true))],
            "lo saltea cuando le toque (SKIP)".into(),
        ),
        "unskip" if !step.skip => {
            return Err(bad(format!("El paso {n} no está marcado para saltearse.")));
        }
        "unskip" => (
            vec![("SKIP", A::B(false))],
            "deja de saltearlo (SKIP = FALSE)".into(),
        ),
        "pause" if step.pause => {
            return Err(bad(format!("El paso {n} ya tiene PAUSE.")));
        }
        "pause" => (
            vec![("PAUSE", A::B(true))],
            "pausa la cadena cuando el paso termine (PAUSE)".into(),
        ),
        "pause_before" => match step.pause_before {
            None => {
                return Err(Error::Unsupported(
                    "esta versión de Oracle no tiene PAUSE_BEFORE (llegó en 12.1)".into(),
                ))
            }
            Some(true) => return Err(bad(format!("El paso {n} ya tiene PAUSE_BEFORE."))),
            Some(false) if !step.not_started() => {
                return Err(bad(format!(
                    "PAUSE_BEFORE pausa un paso antes de que empiece, y {n} está en {}.",
                    step.state
                )))
            }
            Some(false) => (
                vec![("PAUSE_BEFORE", A::B(true))],
                "pausa la cadena antes del paso (PAUSE_BEFORE)".into(),
            ),
        },
        "resume" => {
            let mut calls = Vec::new();
            if step.pause {
                calls.push(("PAUSE", A::B(false)));
            }
            if step.pause_before == Some(true) {
                calls.push(("PAUSE_BEFORE", A::B(false)));
            }
            if calls.is_empty() {
                return Err(bad(format!("El paso {n} no está pausado.")));
            }
            (calls, "le quita la pausa".into())
        }
        "" => return Err(bad("Falta «Cambio».".into())),
        other => {
            return Err(bad(format!(
                "«{}» no es un cambio de un paso de una cadena en curso.",
                shown(other)
            )))
        }
    };
    let mut b = Block::new(&format!(
        "Cambia el paso {n} de la cadena que ejecuta el job {job}: {what}."
    ));
    match change.as_str() {
        "not_started" => {
            b.note("El paso vuelve a ejecutarse si las reglas de la cadena lo empiezan de nuevo.");
            b.warn(RULES_AGAIN);
        }
        "succeeded" | "failed" | "resume" => b.warn(RULES_AGAIN),
        "skip" => b.note(
            "Cuando las reglas lo empiecen, Oracle lo da por terminado bien sin ejecutarlo.",
        ),
        "pause" => {
            if step.running() {
                b.note("El paso sigue ejecutándose; al terminar queda en PAUSED.");
            }
            b.warn("La cadena no sigue después de este paso hasta que se le quite la pausa.");
        }
        "pause_before" => b.warn(
            "La cadena no ejecuta este paso, ni sigue después de él, hasta que se le quite la pausa.",
        ),
        _ => {}
    }
    for (attribute, value) in calls {
        b.call(
            "DBMS_SCHEDULER.ALTER_RUNNING_CHAIN",
            vec![
                ("job_name", A::Name(job.clone())),
                ("step_name", A::S(step_arg(n))),
                ("attribute", A::S(attribute.into())),
                ("value", value),
            ],
        );
    }
    Ok(b)
}

fn stop_step(c: &Connection, ctx: &Ctx, id: &str, values: &FormValues) -> Result<Block> {
    let run = running_chain(c, ctx, id)?;
    stop_block(&run, Vals(&values.values), ctx.manage())
}

/// Stops the running step the form picked; by force with MANAGE
/// SCHEDULER (`manage`).
pub(crate) fn stop_block(run: &RunningChain, v: Vals, manage: bool) -> Result<Block> {
    let job = run.job();
    let step = run.step(&v.raw("step"))?;
    nameable(step)?;
    let n = &step.name;
    if !step.running() {
        return Err(bad(format!(
            "El paso {n} no se está ejecutando (está en {}).",
            step.state
        )));
    }
    let force = v.bool("force");
    if force && !manage {
        return Err(bad(NEEDS_MANAGE.into()));
    }
    let mut b = Block::new(&format!(
        "Detiene el paso {n} de la cadena que ejecuta el job {job}{}.",
        if force {
            " en el acto (force => TRUE)"
        } else {
            ""
        }
    ));
    b.warn("El paso termina como STOPPED y las reglas de la cadena siguen con ese estado: pueden empezar otros pasos o terminar la cadena.");
    b.call(
        "DBMS_SCHEDULER.STOP_JOB",
        vec![
            (
                "job_name",
                A::S(format!(
                    "{}.{}.{}",
                    crate::quote(&run.owner),
                    crate::quote(&run.name),
                    crate::quote(n)
                )),
            ),
            ("force", A::B(force)),
        ],
    );
    Ok(b)
}

/// The server's refusals of a change to a running chain, in its terms (its
/// message kept after).
pub(crate) fn explained(action: &str, e: Error) -> Error {
    if !matches!(action.trim(), STEER_CHAIN | STOP_STEP) {
        return e;
    }
    let Error::Query(m) = &e else {
        return e;
    };
    let why = if m.contains("ORA-27433") {
        "Oracle no deja poner ese estado al paso ahora."
    } else if m.contains("ORA-27366") {
        "El job ya no está ejecutando la cadena."
    } else if m.contains("ORA-27432") {
        "La cadena no tiene ese paso."
    } else {
        return e;
    };
    Error::Query(format!("{why} {m}"))
}

// ---------------------------------------------------------------------------
// Programs and chains
// ---------------------------------------------------------------------------

/// The prefix `GENERATE_JOB_NAME` takes for a run of `name`, as
/// `RUN_CHAIN` names its own (`RUN_CHAIN$` and the chain's first eight
/// characters): `RUN_PROG$` and the program's first plain ones.
pub(crate) fn run_prefix(name: &str) -> String {
    let head: String = name
        .chars()
        .take_while(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || *c == '_')
        .take(8)
        .collect();
    format!("RUN_PROG${head}")
}

fn usable(c: &Connection, ctx: &Ctx, owner: &str, name: &str, view: &str, col: &str) -> Result<()> {
    if ctx.may_execute(owner, name, &super::executable(c, ctx, view, col)) {
        return Ok(());
    }
    Err(bad(format!(
        "Este usuario no puede usar {}: hace falta ser su dueño, tener EXECUTE sobre él o el privilegio EXECUTE ANY PROGRAM.",
        jobs::job_id(owner, name)
    )))
}

fn run_program(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "programa")?;
    let prog = jobs::job_id(&owner, &name);
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_programs"),
        "WHERE owner = :1 AND program_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe el programa «{prog}» o este usuario no lo puede ver"
        ))
    })?;
    if ctx.oracle_schema(&owner) {
        return Err(bad(ORACLE_RUNS.into()));
    }
    if !r.yes("ENABLED") {
        return Err(bad(format!(
            "El programa {prog} está deshabilitado: habilitalo para ejecutarlo."
        )));
    }
    let ptype = r.t("PROGRAM_TYPE").unwrap_or_default();
    if !program::RUNNABLE.contains(&ptype.as_str()) {
        return Err(bad(format!(
            "Un programa de tipo {ptype} necesita una credencial (y una conexión o un destino) para ejecutarse: creá un job que lo use."
        )));
    }
    let wanting = jobs::texts(
        c,
        &format!(
            "{} AND p.owner = :1 AND p.program_name = :2",
            ctx.sql(program::WANTING_ARGS)
        ),
        &[&owner, &name],
        1,
    )
    .map_err(crate::err)?;
    if !wanting.is_empty() {
        return Err(bad(format!(
            "El programa {prog} tiene argumentos sin valor propio: para ejecutarlo hace falta un job que se los dé."
        )));
    }
    usable(
        c,
        ctx,
        &owner,
        &name,
        "{v}_scheduler_programs",
        "program_name",
    )?;
    Ok(run_program_block(&prog, &name, ctx.user()))
}

/// Runs the program `prog` (id; `name`, its name) once now, in a run-once
/// job in `user`'s schema.
pub(crate) fn run_program_block(prog: &str, name: &str, user: &str) -> Block {
    let mut b = Block::new(&format!(
        "Ejecuta el programa {prog} ahora, en un job de una sola vez que Oracle borra al terminar."
    ));
    b.note(&format!(
        "El job se crea en el esquema {user} con el nombre que da GENERATE_JOB_NAME; su ejecución queda en el registro de jobs."
    ));
    b.call(
        "DBMS_SCHEDULER.CREATE_JOB",
        vec![
            (
                "job_name",
                A::R(format!(
                    "DBMS_SCHEDULER.GENERATE_JOB_NAME({})",
                    script::lit(&run_prefix(name))
                )),
            ),
            ("program_name", A::Name(prog.to_string())),
            ("enabled", A::B(true)),
            ("auto_drop", A::B(true)),
        ],
    );
    b
}

fn run_chain(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let (owner, name) = parse_obj(id, "cadena")?;
    let chain = jobs::job_id(&owner, &name);
    let r = read::row(
        c,
        &ctx.sql("{v}_scheduler_chains"),
        "WHERE owner = :1 AND chain_name = :2",
        &[&owner, &name],
    )
    .map_err(crate::err)?
    .ok_or_else(|| {
        bad(format!(
            "no existe la cadena «{chain}» o este usuario no la puede ver"
        ))
    })?;
    if ctx.oracle_schema(&owner) {
        return Err(bad(ORACLE_RUNS.into()));
    }
    if !r.yes("ENABLED") {
        return Err(bad(format!(
            "La cadena {chain} está deshabilitada: habilitala para ejecutarla."
        )));
    }
    usable(c, ctx, &owner, &name, "{v}_scheduler_chains", "chain_name")?;
    Ok(run_chain_block(&chain, ctx.user()))
}

/// Runs the chain `chain` (id) once now, from its start: `RUN_CHAIN`'s own
/// run-once job in `user`'s schema.
pub(crate) fn run_chain_block(chain: &str, user: &str) -> Block {
    let mut b = Block::new(&format!(
        "Ejecuta la cadena {chain} ahora, desde el principio: sus reglas eligen los primeros pasos."
    ));
    b.note(&format!(
        "RUN_CHAIN crea un job de una sola vez en el esquema {user} (RUN_CHAIN$…) que Oracle borra al terminar. La cadena necesita dos procesos de jobs libres: el del job y el de cada paso."
    ));
    b.call(
        "DBMS_SCHEDULER.RUN_CHAIN",
        vec![
            ("chain_name", A::Name(chain.to_string())),
            ("start_steps", A::Null),
            ("job_name", A::Null),
        ],
    );
    b
}
