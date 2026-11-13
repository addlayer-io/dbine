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
//! Steering a running chain's steps (`ALTER_RUNNING_CHAIN`) or stopping
//! one step (`STOP_JOB` of `job.step`) takes the step as a parameter: not
//! offered. Oracle's own programs and chains run from their own jobs.

use super::read;
use super::script::{self, Block, A};
use super::{bad, parse_obj, program, shown, window, Ctx, Kind, OWNER_ONLY};
use crate::jobs::{self, JobId};
use dbine_driver::{Error, Result, SchedulerAction};
use oracledb::Connection;
use std::collections::HashSet;

pub(crate) const RUN: &str = "run";
pub(crate) const OPEN_WINDOW: &str = "open_window";
pub(crate) const CLOSE_WINDOW: &str = "close_window";
pub(crate) const FORCE_STOP: &str = "force_stop";
pub(crate) const END_DETACHED: &str = "end_detached";

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
        (Kind::Job, FORCE_STOP | END_DETACHED)
            | (Kind::Program | Kind::Chain, RUN)
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

/// The block of `action` on the object `id`, checked against what the
/// server has now (the item's actions said what it offered when listed).
pub(crate) fn block(
    c: &Connection,
    ctx: &Ctx,
    kind: Kind,
    id: &str,
    action: &str,
) -> Result<Block> {
    let action = action.trim();
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
    match (kind, action) {
        (Kind::Window, OPEN_WINDOW) => open_window(c, ctx, id),
        (Kind::Window, _) => close_window(c, ctx, id),
        (Kind::Job, FORCE_STOP) => force_stop(c, ctx, id),
        (Kind::Job, _) => end_detached(c, ctx, id),
        (Kind::Program, _) => run_program(c, ctx, id),
        _ => run_chain(c, ctx, id),
    }
}

// ---------------------------------------------------------------------------
// Windows
// ---------------------------------------------------------------------------

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
    let mut b = Block::new(&format!(
        "Abre la ventana {target} ahora, por su duración ({}).",
        jobs::duration_text(duration * 1000)
    ));
    b.note("Mientras está abierta rige su plan de recursos y pueden correr los jobs que la usan.");
    if let Some(o) = &open {
        b.note(&format!(
            "Solo puede haber una ventana abierta: cierra la que está abierta ahora, {} (force => TRUE).",
            window::sys(o)
        ));
    }
    b.call(
        "DBMS_SCHEDULER.OPEN_WINDOW",
        vec![
            ("window_name", A::Name(target)),
            ("duration", A::Null),
            ("force", A::B(open.is_some())),
        ],
    );
    Ok(b)
}

fn close_window(c: &Connection, ctx: &Ctx, id: &str) -> Result<Block> {
    let name = window::sys_name(id, "ventana")?;
    let (_, r) = window::read_state(c, ctx, &name)?;
    let target = window::sys(&name);
    if !r.yes("ACTIVE") {
        return Err(bad(format!("La ventana {target} no está abierta.")));
    }
    let mut b = Block::new(&format!(
        "Cierra la ventana {target} ahora, antes de su hora."
    ));
    b.note("Vuelve el plan de recursos de antes; los jobs de la ventana que tienen «Detener al cerrar la ventana» se detienen.");
    b.call(
        "DBMS_SCHEDULER.CLOSE_WINDOW",
        vec![("window_name", A::Name(target))],
    );
    Ok(b)
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
    let mut b = Block::new(&format!(
        "Ejecuta el programa {prog} ahora, en un job de una sola vez que Oracle borra al terminar."
    ));
    b.note(&format!(
        "El job se crea en el esquema {} con el nombre que da GENERATE_JOB_NAME; su ejecución queda en el registro de jobs.",
        ctx.user()
    ));
    b.call(
        "DBMS_SCHEDULER.CREATE_JOB",
        vec![
            (
                "job_name",
                A::R(format!(
                    "DBMS_SCHEDULER.GENERATE_JOB_NAME({})",
                    script::lit(&run_prefix(&name))
                )),
            ),
            ("program_name", A::Name(prog)),
            ("enabled", A::B(true)),
            ("auto_drop", A::B(true)),
        ],
    );
    Ok(b)
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
    let mut b = Block::new(&format!(
        "Ejecuta la cadena {chain} ahora, desde el principio: sus reglas eligen los primeros pasos."
    ));
    b.note(&format!(
        "RUN_CHAIN crea un job de una sola vez en el esquema {} (RUN_CHAIN$…) que Oracle borra al terminar. La cadena necesita dos procesos de jobs libres: el del job y el de cada paso.",
        ctx.user()
    ));
    b.call(
        "DBMS_SCHEDULER.RUN_CHAIN",
        vec![
            ("chain_name", A::Name(chain)),
            ("start_steps", A::Null),
            ("job_name", A::Null),
        ],
    );
    Ok(b)
}
