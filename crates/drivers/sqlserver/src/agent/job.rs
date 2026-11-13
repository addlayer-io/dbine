//! Jobs: SSMS's "New Job" / "Job Properties" dialog (General, Steps,
//! Schedules, Alerts, Notifications, Targets) as a form, and the script
//! that creates the job or changes it in place.
//!
//! Steps keep their place in msdb: a new step is inserted where it goes
//! (`sp_add_jobstep @step_id`, which moves the later ones and their "go to
//! step" references up), a removed one is deleted (`sp_delete_jobstep`,
//! which moves them down and turns references to it into "quit"), and the
//! rest are updated with what changed. The planner replays those
//! adjustments as msdb makes them, so each step's final update sets exactly
//! what still differs. When the remaining steps change order, the steps
//! are rewritten by position (msdb has no move). A name another step still
//! holds is freed first with a temporary name.

use super::choices::{self, Catalog};
use super::schedule::{self, Freq};
use super::{
    bad, fits, guid_literal, pick, quoted, rows, shown, Ctx, Plan, Script, Vals, A, SYSNAME,
};
use crate::jobs::{self, first_with, text, JobRow, Raw, StepRow};
use crate::SqlServerSession;
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, ObjectForm, Result,
    SchedulerItem, SchedulerList,
};
use serde_json::{json, Value};
use std::collections::{BTreeMap, BTreeSet, HashMap};

const OWNER_ONLY: &str = "Solo el propietario del job o un miembro de sysadmin puede cambiarlo.";
const FROM_MASTER: &str =
    "Este job lo envió un servidor maestro (MSX): se cambia allí, no en este servidor de destino.";
const NO_TARGET: &str = "Este job no tiene un servidor de destino: al guardarlo se asigna al servidor local, sin lo cual el Agente no lo ejecuta.";
const ALERTS_READ_ONLY: &str =
    "Solo un miembro de sysadmin puede cambiar qué alertas ejecutan este job.";
const NOT_MSX: &str =
    "Este servidor no es un servidor maestro (MSX): el job se ejecuta en el servidor local.";
const NO_ALERTS: &str =
    "Este usuario no puede ver las alertas (hace falta SQLAgentOperatorRole o sysadmin).";
const CHANGED: &str =
    "El job cambió en el servidor desde que se abrió el formulario: recargalo antes de guardar.";
const UNCATEGORIZED: &str = "[Uncategorized (Local)]";
/// Warnings (`-- ⚠ …` lines) of a delete or an edit.
pub(crate) const DELETES_HISTORY: &str = "Se borra también el historial de ejecuciones del job.";
pub(crate) const STEP_LOGS_MOVE: &str = "Los pasos cambian de orden y se reescriben por posición (msdb no puede moverlos): la salida que un paso registró en la tabla queda con el paso que ocupa ahora su lugar.";
/// msdb's "no job" for an alert's response.
const NO_JOB: &str = "N'00000000-0000-0000-0000-000000000000'";

// ---------------------------------------------------------------------------
// Subsystems
// ---------------------------------------------------------------------------

/// msdb's subsystem names, in SSMS's order.
const SUBSYSTEMS: [&str; 12] = [
    "TSQL",
    "CmdExec",
    "PowerShell",
    "SSIS",
    "ANALYSISQUERY",
    "ANALYSISCOMMAND",
    "Snapshot",
    "LogReader",
    "Distribution",
    "Merge",
    "QueueReader",
    "ActiveScripting",
];

/// The subsystem as the form's options spell it.
pub(crate) fn canonical(sub: &str) -> String {
    SUBSYSTEMS
        .iter()
        .find(|s| s.eq_ignore_ascii_case(sub.trim()))
        .map_or_else(|| sub.trim().to_string(), |s| s.to_string())
}

fn is(sub: &str, name: &str) -> bool {
    sub.eq_ignore_ascii_case(name)
}

fn analysis(sub: &str) -> bool {
    is(sub, "ANALYSISQUERY") || is(sub, "ANALYSISCOMMAND")
}

/// msdb takes an output file and a log table for these.
fn writes_output(sub: &str) -> bool {
    [
        "TSQL",
        "CmdExec",
        "PowerShell",
        "SSIS",
        "ANALYSISQUERY",
        "ANALYSISCOMMAND",
    ]
    .iter()
    .any(|s| is(sub, s))
}

/// The row field with the step's proxy, per subsystem.
fn proxy_key(sub: &str) -> &'static str {
    match canonical(sub).as_str() {
        "CmdExec" => "proxy_cmdexec",
        "PowerShell" => "proxy_powershell",
        "SSIS" => "proxy_ssis",
        "ANALYSISQUERY" => "proxy_analysisquery",
        "ANALYSISCOMMAND" => "proxy_analysiscommand",
        "Snapshot" => "proxy_snapshot",
        "LogReader" => "proxy_logreader",
        "Distribution" => "proxy_distribution",
        "Merge" => "proxy_merge",
        "QueueReader" => "proxy_queuereader",
        _ => "proxy_activescripting",
    }
}

/// The row field with the step's command, per subsystem (each with its
/// editor language).
fn command_key(sub: &str) -> &'static str {
    match canonical(sub).as_str() {
        "TSQL" => "command",
        "PowerShell" => "command_powershell",
        "CmdExec" => "command_cmdexec",
        _ => "command_other",
    }
}

const ACTIONS: [(&str, i64); 4] = [
    ("quit_success", 1),
    ("quit_failure", 2),
    ("next", 3),
    ("goto", 4),
];

fn action_key(code: i64) -> &'static str {
    ACTIONS
        .iter()
        .find(|(_, c)| *c == code)
        .map_or("quit_success", |(k, _)| *k)
}

const LEVELS: [(&str, i64); 3] = [("success", 1), ("failure", 2), ("completion", 3)];

fn level_key(level: i64) -> &'static str {
    LEVELS
        .iter()
        .find(|(_, c)| *c == level)
        .map_or("failure", |(k, _)| *k)
}

// ---------------------------------------------------------------------------
// Steps
// ---------------------------------------------------------------------------

/// A step as msdb keeps it (or should), `on_*_step` as step ids.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Step {
    pub(crate) name: String,
    pub(crate) subsystem: String,
    pub(crate) command: String,
    pub(crate) database: Option<String>,
    pub(crate) database_user: Option<String>,
    pub(crate) server: Option<String>,
    pub(crate) proxy: Option<String>,
    /// The step runs as a proxy this login can't name: left as it is.
    pub(crate) proxy_hidden: bool,
    pub(crate) success_code: i64,
    pub(crate) on_success: i64,
    pub(crate) on_success_step: i64,
    pub(crate) on_fail: i64,
    pub(crate) on_fail_step: i64,
    pub(crate) retry_attempts: i64,
    pub(crate) retry_interval: i64,
    pub(crate) output_file: Option<String>,
    pub(crate) flags: i64,
    pub(crate) os_priority: i64,
}

impl Step {
    pub(crate) fn of(r: &StepRow, proxies: &HashMap<i64, String>) -> Step {
        let proxy = r.proxy_id.and_then(|p| proxies.get(&p).cloned());
        Step {
            name: r.name.clone(),
            subsystem: canonical(&r.subsystem),
            command: r.command.clone().unwrap_or_default(),
            database: r.database.clone(),
            database_user: r.database_user.clone(),
            server: r.server.clone(),
            proxy_hidden: r.proxy_id.is_some() && proxy.is_none(),
            proxy,
            success_code: r.success_code,
            on_success: r.on_success,
            on_success_step: r.on_success_step,
            on_fail: r.on_fail,
            on_fail_step: r.on_fail_step,
            retry_attempts: r.retry_attempts,
            retry_interval: r.retry_interval,
            output_file: r.output_file.clone(),
            flags: r.flags,
            os_priority: r.os_priority,
        }
    }

    fn tsql(&self) -> bool {
        is(&self.subsystem, "TSQL")
    }
}

/// The flags the form's checkboxes ask for, keeping the bits they don't
/// show (and the shape a checkbox already had) from `cur`.
pub(crate) fn merge_flags(
    cur: i64,
    sub: &str,
    append: bool,
    history: bool,
    table: bool,
    append_table: bool,
) -> i64 {
    let tsql = is(sub, "TSQL");
    let mut f = cur;
    if append != (cur & 2 != 0) {
        f = if append { f | 2 } else { f & !2 };
    }
    // Step output in history: 4 for T-SQL, 32 (all output) for the rest.
    // Only when the box changes: a step left alone keeps its bits (a 4 on
    // another type is turned into 32 only when the step is written, see
    // `stored_flags`).
    if history != (cur & (4 | 32) != 0) {
        f &= !(4 | 32);
        if history {
            f |= if tsql { 4 } else { 32 };
        }
    }
    let had_table = cur & (8 | 16) != 0;
    if table != had_table || (table && append_table != (cur & 16 != 0)) {
        f &= !(8 | 16);
        if table {
            f |= if append_table { 16 } else { 8 };
        }
    }
    if !writes_output(sub) {
        f &= !(8 | 16);
    }
    if !is(sub, "CmdExec") {
        f &= !64;
    }
    f
}

/// The flags a step is written with: `sp_verify_jobstep` takes 4 (output
/// in history) only on T-SQL, so another type's 4 (a step that was T-SQL,
/// or older data) is written as 32, the same box for the rest.
fn stored_flags(d: &Step) -> i64 {
    if !d.tsql() && d.flags & 4 != 0 {
        (d.flags & !4) | 32
    } else {
        d.flags
    }
}

/// A step row from the designer, before its "go to step" names become ids.
#[derive(Debug, Clone)]
pub(crate) struct Wanted {
    /// The step it was (`step_id` as read), if any.
    pub(crate) orig: Option<i64>,
    pub(crate) step: Step,
    pub(crate) success_target: Option<String>,
    pub(crate) fail_target: Option<String>,
}

fn action(v: Vals, k: &str, default: i64, step: &str) -> Result<i64> {
    match v.opt(k) {
        None => Ok(default),
        Some(a) => ACTIONS
            .iter()
            .find(|(key, _)| *key == a)
            .map(|(_, c)| *c)
            .ok_or_else(|| {
                bad(format!(
                    "El paso «{}»: «{}» no es una acción válida.",
                    shown(step),
                    shown(&a)
                ))
            }),
    }
}

/// One step row, checked. `current`: the step it was. `admin`: the login
/// may set what only sysadmin may (run as user, output file).
pub(crate) fn wanted_step(
    v: Vals,
    orig: Option<i64>,
    current: Option<&Step>,
    catalog: &Catalog,
    admin: bool,
) -> Result<Wanted> {
    let name = v
        .opt("name")
        .ok_or_else(|| bad("Cada paso del job necesita un nombre.".into()))?;
    fits("Nombre del paso", &name, SYSNAME)?;
    let n = shown(&name);
    let typed = v.opt("subsystem").unwrap_or_else(|| "TSQL".into());
    let sub = canonical(&typed);
    let available = catalog.subsystems();
    let known = SUBSYSTEMS.iter().any(|s| *s == sub);
    let offered = available.is_empty() || available.iter().any(|a| is(a, &sub));
    let kept = current.is_some_and(|c| is(&c.subsystem, &sub));
    if !known || !(offered || kept) {
        return Err(bad(format!(
            "El paso «{n}»: el tipo «{}» no está disponible en este servidor.",
            shown(&typed)
        )));
    }
    let tsql = is(&sub, "TSQL");
    let command = v.raw(command_key(&sub));
    let database = if tsql {
        let db = v.opt("database").unwrap_or_else(|| "master".into());
        let dbs = catalog.get("database");
        Some(if dbs.is_empty() {
            db
        } else {
            pick(&db, dbs, |d| {
                format!("El paso «{n}»: la base de datos «{d}» no existe en este servidor.")
            })?
            .to_string()
        })
    } else if analysis(&sub) {
        v.opt("as_database")
    } else {
        None
    };
    let database_user = match (tsql, admin) {
        (true, true) => v.opt("database_user"),
        (true, false) => current.and_then(|c| c.database_user.clone()),
        _ => None,
    };
    let server = analysis(&sub).then(|| v.opt("server")).flatten();
    let proxy = if tsql {
        None
    } else {
        match v.opt(proxy_key(&sub)) {
            None => None,
            Some(p) => {
                let source = format!("proxy:{sub}");
                let list = catalog.get(&source);
                Some(if list.is_empty() {
                    p
                } else {
                    pick(&p, list, |p| {
                        format!("El paso «{n}»: el proxy «{p}» no existe o no puede ejecutar pasos de este tipo.")
                    })?
                    .to_string()
                })
            }
        }
    };
    let proxy_hidden = !tsql && proxy.is_none() && current.is_some_and(|c| c.proxy_hidden);
    let on_success = action(v, "on_success", 3, &name)?;
    let on_fail = action(v, "on_fail", 2, &name)?;
    let target = |k: &str, a: i64| -> Result<Option<String>> {
        if a != 4 {
            return Ok(None);
        }
        v.opt(k)
            .map(Some)
            .ok_or_else(|| bad(format!("El paso «{n}» va a otro paso: elegí a cuál.")))
    };
    let success_target = target("on_success_step", on_success)?;
    let fail_target = target("on_fail_step", on_fail)?;
    let cur_flags = current.map_or(0, |c| c.flags);
    let flags = merge_flags(
        cur_flags,
        &sub,
        v.bool("append_output"),
        v.bool("output_in_history"),
        v.bool("log_to_table"),
        v.bool("append_to_table"),
    );
    let output_file = if !writes_output(&sub) {
        None
    } else if admin {
        v.opt("output_file")
    } else {
        current.and_then(|c| c.output_file.clone())
    };
    // What msdb takes (`sysname`, the output file's 200).
    for (label, value, max) in [
        ("Archivo de salida", &output_file, 200),
        ("Ejecutar como usuario", &database_user, SYSNAME),
        ("Base de datos", &database, SYSNAME),
        ("Servidor", &server, SYSNAME),
        ("Ejecutar como", &proxy, SYSNAME),
    ] {
        if let Some(t) = value {
            fits(label, t, max)?;
        }
    }
    let step = Step {
        name: name.clone(),
        subsystem: sub.clone(),
        command,
        database,
        database_user,
        server,
        proxy,
        proxy_hidden,
        success_code: if is(&sub, "CmdExec") {
            v.int(
                "success_code",
                "Código de salida de un comando correcto",
                i64::from(i32::MIN),
                i64::from(i32::MAX),
            )?
            .unwrap_or(0)
        } else {
            current.map_or(0, |c| c.success_code)
        },
        on_success,
        on_success_step: 0,
        on_fail,
        on_fail_step: 0,
        retry_attempts: v.int("retry_attempts", "Reintentos", 0, 9999)?.unwrap_or(0),
        retry_interval: v
            .int(
                "retry_interval",
                "Intervalo de reintento (minutos)",
                0,
                9999,
            )?
            .unwrap_or(0),
        output_file,
        flags,
        os_priority: v
            .int("os_run_priority", "Prioridad", -15, 15)?
            .or(current.map(|c| c.os_priority))
            .unwrap_or(0),
    };
    Ok(Wanted {
        orig,
        step,
        success_target,
        fail_target,
    })
}

/// The steps in their new order, checked together: names unique, every
/// "go to step" to another step of the job (its id resolved to the step's
/// final place), a row's id that of a step the job has (a row repeating
/// one is a new step). `current`: the job's step names, by id. A reference
/// still naming a step as it was called before a rename in the same edit
/// reaches it. Returns the steps and the start step's place.
pub(crate) fn resolve_steps(
    mut wanted: Vec<Wanted>,
    current: &[String],
    start: Option<&str>,
) -> Result<(Vec<Wanted>, i64)> {
    let mut seen_ids = BTreeSet::new();
    for w in &mut wanted {
        if let Some(id) = w.orig {
            if id < 1 || id as usize > current.len() || !seen_ids.insert(id) {
                w.orig = None;
            }
        }
    }
    let mut names: HashMap<String, usize> = HashMap::new();
    for (i, w) in wanted.iter().enumerate() {
        if names.insert(w.step.name.to_lowercase(), i + 1).is_some() {
            return Err(bad(format!(
                "Hay dos pasos llamados «{}»: cada paso del job necesita un nombre distinto.",
                shown(&w.step.name)
            )));
        }
    }
    let mut renamed: HashMap<String, usize> = HashMap::new();
    for (i, w) in wanted.iter().enumerate() {
        if let Some(old) = w.orig.and_then(|id| current.get(id as usize - 1)) {
            renamed.entry(old.to_lowercase()).or_insert(i + 1);
        }
    }
    let place_of = |to: &str| {
        let k = to.to_lowercase();
        names.get(&k).or_else(|| renamed.get(&k)).map(|p| *p as i64)
    };
    let place = |from: &str, to: &str| -> Result<i64> {
        place_of(to).ok_or_else(|| {
            bad(format!(
                "El paso «{}» va al paso «{}», que no está en el job.",
                shown(from),
                shown(to)
            ))
        })
    };
    for (i, w) in wanted.iter_mut().enumerate() {
        for success in [true, false] {
            let target = if success {
                &w.success_target
            } else {
                &w.fail_target
            };
            let Some(t) = target else { continue };
            let p = place(&w.step.name, t)?;
            if p == i as i64 + 1 {
                return Err(bad(format!(
                    "El paso «{}» no puede ir a sí mismo.",
                    shown(&w.step.name)
                )));
            }
            if success {
                w.step.on_success_step = p;
            } else {
                w.step.on_fail_step = p;
            }
        }
    }
    let start = match start.map(str::trim).filter(|s| !s.is_empty()) {
        _ if wanted.is_empty() => 0,
        None => 1,
        Some(s) => place_of(s)
            .ok_or_else(|| bad(format!("El paso inicial «{}» no está en el job.", shown(s))))?,
    };
    Ok((wanted, start))
}

/// One change to the job's steps, in the order the script makes them.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum StepOp {
    /// `sp_delete_jobstep @step_id`.
    Delete(i64),
    /// `sp_update_jobstep @step_id, @step_name` with a temporary name, so
    /// another step can take this one's.
    Rename(i64, String),
    /// `sp_add_jobstep @step_id`: the step, whole.
    Add(i64, Box<Step>),
    /// `sp_update_jobstep @step_id`: from what msdb has (second) to what it
    /// should (first).
    Update(i64, Box<Step>, Box<Step>),
}

/// msdb after `sp_delete_jobstep @step_id = id`.
fn sim_delete(sim: &mut Vec<Step>, id: i64) {
    sim.remove(id as usize - 1);
    for s in sim.iter_mut() {
        if s.on_success_step > id {
            s.on_success_step -= 1;
        }
        if s.on_fail_step > id {
            s.on_fail_step -= 1;
        }
        if s.on_success_step == id {
            s.on_success_step = 0;
            s.on_success = 1;
        }
        if s.on_fail_step == id {
            s.on_fail_step = 0;
            s.on_fail = 2;
        }
    }
}

/// msdb after `sp_add_jobstep @step_id = id`.
fn sim_insert(sim: &mut Vec<Step>, id: i64, step: Step) {
    if id as usize <= sim.len() {
        for s in sim.iter_mut() {
            if s.on_success_step >= id {
                s.on_success_step += 1;
            }
            if s.on_fail_step >= id {
                s.on_fail_step += 1;
            }
            if s.on_success_step == id {
                s.on_success_step = 0;
                s.on_success = 1;
            }
            if s.on_fail_step == id {
                s.on_fail_step = 0;
                s.on_fail = 2;
            }
        }
    }
    sim.insert(id as usize - 1, step);
}

/// Before step `at` (or a new one, `at` 0) takes `name`: another step that
/// still holds it gets a temporary name.
fn free_name(sim: &mut [Step], at: i64, name: &str, finals: &[Wanted], ops: &mut Vec<StepOp>) {
    let Some(q) = sim
        .iter()
        .enumerate()
        .position(|(i, s)| i as i64 + 1 != at && s.name.eq_ignore_ascii_case(name))
    else {
        return;
    };
    let taken = |t: &str| {
        sim.iter().any(|s| s.name.eq_ignore_ascii_case(t))
            || finals.iter().any(|w| w.step.name.eq_ignore_ascii_case(t))
    };
    let mut tmp = format!("dbine_tmp_{}", q + 1);
    while taken(&tmp) {
        tmp.push('_');
    }
    ops.push(StepOp::Rename(q as i64 + 1, tmp.clone()));
    sim[q].name = tmp;
}

/// What a step update would set: nothing when it matches.
fn differs(d: &Step, o: &Step) -> bool {
    !step_changes(d, o).is_empty()
}

/// The changes that turn the job's `current` steps (by id) into `finals`
/// (in order, references resolved).
pub(crate) fn plan_steps(current: &[Step], finals: &[Wanted]) -> Vec<StepOp> {
    let mut ops = Vec::new();
    let mut sim: Vec<Step> = current.to_vec();
    let mut tags: Vec<Option<i64>> = (1..=current.len() as i64).map(Some).collect();
    let kept: BTreeSet<i64> = finals.iter().filter_map(|w| w.orig).collect();
    for id in (1..=current.len() as i64).rev() {
        if !kept.contains(&id) {
            ops.push(StepOp::Delete(id));
            sim_delete(&mut sim, id);
            tags.remove(id as usize - 1);
        }
    }
    let order: Vec<i64> = finals.iter().filter_map(|w| w.orig).collect();
    let in_order = order.windows(2).all(|p| p[0] < p[1]);
    if in_order {
        // New steps where they go, then each step's remaining changes.
        for (i, w) in finals.iter().enumerate() {
            let pos = i as i64 + 1;
            if w.orig.is_none() {
                free_name(&mut sim, 0, &w.step.name, finals, &mut ops);
                ops.push(StepOp::Add(pos, Box::new(w.step.clone())));
                sim_insert(&mut sim, pos, w.step.clone());
                tags.insert(i, None);
            }
        }
        for (i, w) in finals.iter().enumerate() {
            let pos = i as i64 + 1;
            if w.orig.is_some() && differs(&w.step, &sim[i]) {
                if !w.step.name.eq_ignore_ascii_case(&sim[i].name) {
                    free_name(&mut sim, pos, &w.step.name, finals, &mut ops);
                }
                ops.push(StepOp::Update(
                    pos,
                    Box::new(w.step.clone()),
                    Box::new(sim[i].clone()),
                ));
                sim[i] = w.step.clone();
            }
        }
        // A new step's references moved when later ones went in.
        for (i, w) in finals.iter().enumerate() {
            if w.orig.is_none() && differs(&w.step, &sim[i]) {
                ops.push(StepOp::Update(
                    i as i64 + 1,
                    Box::new(w.step.clone()),
                    Box::new(sim[i].clone()),
                ));
                sim[i] = w.step.clone();
            }
        }
    } else {
        // Reordered: each place takes its step's whole definition.
        let k = sim.len();
        for (i, w) in finals.iter().enumerate().take(k) {
            let pos = i as i64 + 1;
            if differs(&w.step, &sim[i]) {
                if !w.step.name.eq_ignore_ascii_case(&sim[i].name) {
                    free_name(&mut sim, pos, &w.step.name, finals, &mut ops);
                }
                ops.push(StepOp::Update(
                    pos,
                    Box::new(w.step.clone()),
                    Box::new(sim[i].clone()),
                ));
                sim[i] = w.step.clone();
            }
        }
        for (i, w) in finals.iter().enumerate().skip(k) {
            ops.push(StepOp::Add(i as i64 + 1, Box::new(w.step.clone())));
            sim.push(w.step.clone());
        }
    }
    ops
}

/// The steps change order, so [`plan_steps`] rewrites them by place (each
/// place keeps its step's uid), and a place whose step logged its output
/// to the table (`sysjobstepslogs`, by uid) now holds another step: that
/// log goes with the place, not with the step.
pub(crate) fn moves_table_logs(current: &[Step], finals: &[Wanted]) -> bool {
    let order: Vec<i64> = finals.iter().filter_map(|w| w.orig).collect();
    if order.windows(2).all(|p| p[0] < p[1]) {
        return false;
    }
    let kept: BTreeSet<i64> = order.iter().copied().collect();
    // The places left after the deletes, each with the step it holds.
    let places: Vec<i64> = (1..=current.len() as i64)
        .filter(|id| kept.contains(id))
        .collect();
    places
        .iter()
        .zip(finals)
        .any(|(id, w)| w.orig != Some(*id) && current[*id as usize - 1].flags & (8 | 16) != 0)
}

/// The `sp_update_jobstep` arguments that turn `o` into `d`.
pub(crate) fn step_changes(d: &Step, o: &Step) -> Vec<(&'static str, A)> {
    let mut a: Vec<(&'static str, A)> = Vec::new();
    let opt_eq = |x: &Option<String>, y: &Option<String>| {
        x.as_deref().unwrap_or("") == y.as_deref().unwrap_or("")
    };
    if d.name != o.name {
        a.push(("step_name", A::T(d.name.clone())));
    }
    if !is(&d.subsystem, &o.subsystem) {
        a.push(("subsystem", A::T(d.subsystem.clone())));
    }
    if d.command != o.command {
        a.push(("command", A::T(d.command.clone())));
    }
    if (d.tsql() || analysis(&d.subsystem)) && !opt_eq(&d.database, &o.database) {
        a.push((
            "database_name",
            A::T(d.database.clone().unwrap_or_default()),
        ));
    }
    if d.tsql() && !opt_eq(&d.database_user, &o.database_user) {
        a.push((
            "database_user_name",
            A::T(d.database_user.clone().unwrap_or_default()),
        ));
    }
    if analysis(&d.subsystem) && !opt_eq(&d.server, &o.server) {
        a.push(("server", A::T(d.server.clone().unwrap_or_default())));
    }
    let proxy_same = d
        .proxy
        .as_deref()
        .unwrap_or("")
        .eq_ignore_ascii_case(o.proxy.as_deref().unwrap_or(""));
    if d.tsql() {
        if o.proxy.is_some() || o.proxy_hidden {
            a.push(("proxy_name", A::T(String::new())));
        }
    } else if !proxy_same && !(d.proxy_hidden && d.proxy.is_none()) {
        a.push(("proxy_name", A::T(d.proxy.clone().unwrap_or_default())));
    }
    if is(&d.subsystem, "CmdExec") && d.success_code != o.success_code {
        a.push(("cmdexec_success_code", A::I(d.success_code)));
    }
    if d.on_success != o.on_success || (d.on_success == 4 && d.on_success_step != o.on_success_step)
    {
        a.push(("on_success_action", A::I(d.on_success)));
        a.push((
            "on_success_step_id",
            A::I(if d.on_success == 4 {
                d.on_success_step
            } else {
                0
            }),
        ));
    }
    if d.on_fail != o.on_fail || (d.on_fail == 4 && d.on_fail_step != o.on_fail_step) {
        a.push(("on_fail_action", A::I(d.on_fail)));
        a.push((
            "on_fail_step_id",
            A::I(if d.on_fail == 4 { d.on_fail_step } else { 0 }),
        ));
    }
    if d.retry_attempts != o.retry_attempts {
        a.push(("retry_attempts", A::I(d.retry_attempts)));
    }
    if d.retry_interval != o.retry_interval {
        a.push(("retry_interval", A::I(d.retry_interval)));
    }
    if d.os_priority != o.os_priority {
        a.push(("os_run_priority", A::I(d.os_priority)));
    }
    if !opt_eq(&d.output_file, &o.output_file) {
        a.push((
            "output_file_name",
            A::T(d.output_file.clone().unwrap_or_default()),
        ));
    }
    // msdb checks the flags on every update, the ones it keeps included:
    // a step updated for anything else gets valid ones too.
    let flags = stored_flags(d);
    if d.flags != o.flags || (flags != d.flags && !a.is_empty()) {
        a.push(("flags", A::I(flags)));
    }
    a
}

/// Every `sp_add_jobstep` argument the step's type takes.
fn step_args(d: &Step) -> Vec<(&'static str, A)> {
    let mut a: Vec<(&'static str, A)> = vec![
        ("step_name", A::T(d.name.clone())),
        ("subsystem", A::T(d.subsystem.clone())),
        ("command", A::T(d.command.clone())),
    ];
    if let Some(db) = d
        .database
        .clone()
        .filter(|_| d.tsql() || analysis(&d.subsystem))
    {
        a.push(("database_name", A::T(db)));
    }
    if let Some(u) = d.database_user.clone().filter(|_| d.tsql()) {
        a.push(("database_user_name", A::T(u)));
    }
    if let Some(sv) = d.server.clone().filter(|_| analysis(&d.subsystem)) {
        a.push(("server", A::T(sv)));
    }
    if let Some(p) = d.proxy.clone().filter(|_| !d.tsql()) {
        a.push(("proxy_name", A::T(p)));
    }
    if is(&d.subsystem, "CmdExec") {
        a.push(("cmdexec_success_code", A::I(d.success_code)));
    }
    a.extend([
        ("on_success_action", A::I(d.on_success)),
        (
            "on_success_step_id",
            A::I(if d.on_success == 4 {
                d.on_success_step
            } else {
                0
            }),
        ),
        ("on_fail_action", A::I(d.on_fail)),
        (
            "on_fail_step_id",
            A::I(if d.on_fail == 4 { d.on_fail_step } else { 0 }),
        ),
        ("retry_attempts", A::I(d.retry_attempts)),
        ("retry_interval", A::I(d.retry_interval)),
        ("os_run_priority", A::I(d.os_priority)),
    ]);
    if let Some(f) = d.output_file.clone() {
        a.push(("output_file_name", A::T(f)));
    }
    a.push(("flags", A::I(stored_flags(d))));
    a
}

pub(crate) fn write_steps(sc: &mut Script, ops: &[StepOp]) {
    for op in ops {
        match op {
            StepOp::Delete(id) => sc.call(
                "msdb.dbo.sp_delete_jobstep",
                vec![("job_id", A::R("@jobId".into())), ("step_id", A::I(*id))],
            ),
            StepOp::Rename(id, tmp) => sc.call(
                "msdb.dbo.sp_update_jobstep",
                vec![
                    ("job_id", A::R("@jobId".into())),
                    ("step_id", A::I(*id)),
                    ("step_name", A::T(tmp.clone())),
                ],
            ),
            StepOp::Add(id, step) => {
                let mut args = vec![("job_id", A::R("@jobId".into())), ("step_id", A::I(*id))];
                args.extend(step_args(step));
                sc.call("msdb.dbo.sp_add_jobstep", args);
            }
            StepOp::Update(id, d, o) => {
                let mut args = vec![("job_id", A::R("@jobId".into())), ("step_id", A::I(*id))];
                args.extend(step_changes(d, o));
                sc.call("msdb.dbo.sp_update_jobstep", args);
            }
        }
    }
}

// ---------------------------------------------------------------------------
// The job as the form says it
// ---------------------------------------------------------------------------

/// One notification: when (`notify_level_*`: 0 never, 1 success, 2
/// failure, 3 completion) and to whom.
#[derive(Debug, Clone, PartialEq, Default)]
pub(crate) struct Notify {
    pub(crate) level: i64,
    pub(crate) operator: Option<String>,
}

#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Targets {
    Local,
    Servers(Vec<String>),
}

/// A schedule of the job's own, or one it shares.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct OwnSchedule {
    pub(crate) id: Option<i64>,
    pub(crate) name: String,
    pub(crate) freq: Freq,
}

#[derive(Debug, Clone)]
pub(crate) struct JobSpec {
    pub(crate) name: String,
    pub(crate) enabled: bool,
    pub(crate) description: String,
    pub(crate) category: Option<String>,
    /// Set only by sysadmin.
    pub(crate) owner: Option<String>,
    pub(crate) steps: Vec<Wanted>,
    pub(crate) start_step: i64,
    pub(crate) schedules: Vec<OwnSchedule>,
    pub(crate) shared: Vec<i64>,
    /// `None`: not the login's to change (left as they are).
    pub(crate) alerts: Option<Vec<String>>,
    pub(crate) email: Notify,
    pub(crate) page: Notify,
    pub(crate) netsend: Notify,
    pub(crate) eventlog: i64,
    pub(crate) delete: i64,
    pub(crate) targets: Targets,
}

/// The job as msdb has it now.
#[derive(Debug, Clone, Default)]
pub(crate) struct JobState {
    pub(crate) id: String,
    pub(crate) raw: Raw,
    pub(crate) steps: Vec<Step>,
    /// The alerts that run it; `None` when the login can't see alerts.
    pub(crate) alerts: Option<Vec<String>>,
    /// The owner of each of its schedules (by id), where it was read
    /// (`sp_help_jobschedule` doesn't say).
    pub(crate) schedule_owners: HashMap<i64, Option<String>>,
    /// msdb holds rows of its history (`sysjobhistory`); `None` where the
    /// login can't read them (the SQLAgent roles).
    pub(crate) history: Option<bool>,
}

impl JobState {
    /// The login may change (and delete) this schedule of the job:
    /// sysadmin, or its owner. `None`: its owner wasn't read.
    pub(crate) fn owns_schedule(&self, c: &Ctx, id: i64) -> Option<bool> {
        if c.p.sysadmin {
            return Some(true);
        }
        self.schedule_owners
            .get(&id)
            .map(|owner| c.is(owner.as_deref()))
    }
}

fn notify(
    v: Vals,
    on: &str,
    op: &str,
    when: &str,
    what: &str,
    catalog: &Catalog,
) -> Result<Notify> {
    if !v.bool(on) {
        return Ok(Notify::default());
    }
    let level = level(v, when)?;
    let operator = v
        .opt(op)
        .ok_or_else(|| bad(format!("Notificaciones: elegí el operador para «{what}».")))?;
    let ops = catalog.get("operator");
    let operator = if ops.is_empty() {
        operator
    } else {
        pick(&operator, ops, |o| {
            format!("Notificaciones: el operador «{o}» no existe.")
        })?
        .to_string()
    };
    Ok(Notify {
        level,
        operator: Some(operator),
    })
}

fn level(v: Vals, when: &str) -> Result<i64> {
    let w = v.opt(when).unwrap_or_else(|| "failure".into());
    LEVELS
        .iter()
        .find(|(k, _)| *k == w)
        .map(|(_, c)| *c)
        .ok_or_else(|| {
            bad(format!(
                "«{}» no es un momento válido para avisar.",
                shown(&w)
            ))
        })
}

/// The form's values, checked against the server's lists and the job as it
/// is.
pub(crate) fn spec(
    values: &FormValues,
    catalog: &Catalog,
    c: &Ctx,
    state: Option<&JobState>,
) -> Result<JobSpec> {
    let v = Vals(&values.values);
    let name = v
        .opt("name")
        .ok_or_else(|| bad("El job necesita un nombre.".into()))?;
    fits("Nombre", &name, SYSNAME)?;
    let description = v.text("description");
    fits("Descripción", &description, 512)?;
    let enabled = if v.0.get("enabled").is_some_and(|x| !x.is_null()) {
        v.bool("enabled")
    } else {
        true
    };
    let category = match v.opt("category") {
        None => None,
        Some(cat) => {
            let list = catalog.get("category:job");
            Some(if list.is_empty() {
                cat
            } else {
                pick(&cat, list, |c| {
                    format!("La categoría de jobs «{c}» no existe.")
                })?
                .to_string()
            })
        }
    };
    let owner = match v.opt("owner").filter(|_| c.p.sysadmin) {
        None => None,
        Some(o) => {
            let list = catalog.get("login");
            Some(if list.is_empty() {
                o
            } else {
                pick(&o, list, |o| {
                    format!("«{o}» no es un inicio de sesión de este servidor.")
                })?
                .to_string()
            })
        }
    };
    let current = state.map_or(&[][..], |s| s.steps.as_slice());
    let mut wanted = Vec::new();
    for row in rows(values, "steps") {
        let orig = row
            .int("step_id", "Paso", 1, 32767)
            .ok()
            .flatten()
            .filter(|id| *id as usize <= current.len());
        let cur = orig.map(|id| &current[id as usize - 1]);
        wanted.push(wanted_step(row, orig, cur, catalog, c.p.sysadmin)?);
    }
    let start = v.opt("start_step");
    let names: Vec<String> = current.iter().map(|s| s.name.clone()).collect();
    let (steps, start_step) = resolve_steps(wanted, &names, start.as_deref())?;

    let today = schedule::today(c);
    let attached: HashMap<i64, Freq> = state
        .map(|s| {
            s.raw
                .schedules
                .iter()
                .filter_map(|r| Some((r.id?, Freq::of(r))))
                .collect()
        })
        .unwrap_or_default();
    let mut schedules = Vec::new();
    for row in rows(values, "schedules") {
        let id = row
            .int("schedule_id", "Programación", 1, i64::from(i32::MAX))
            .ok()
            .flatten()
            .filter(|id| attached.contains_key(id));
        let base = id.and_then(|i| attached.get(&i));
        let (name, freq) = schedule::from_values(row, base, today, c.managed())?;
        // msdb lets only the schedule's owner (or sysadmin) change it,
        // whoever owns the job.
        if let (Some(id), Some(st)) = (id, state) {
            let old = st.raw.schedules.iter().find(|r| r.id == Some(id));
            let changed = old.is_some_and(|r| r.name != name || Freq::of(r) != freq);
            if changed && st.owns_schedule(c, id) == Some(false) {
                return Err(bad(format!(
                    "La programación «{}» es de otro inicio de sesión: solo su propietario o un miembro de sysadmin puede cambiarla.",
                    shown(&old.map_or(name.clone(), |r| r.name.clone()))
                )));
            }
        }
        schedules.push(OwnSchedule { id, name, freq });
    }
    let known: BTreeSet<String> = catalog.values("schedule").into_iter().collect();
    let mut shared = Vec::new();
    for row in rows(values, "shared_schedules") {
        let Some(sid) = row.opt("schedule") else {
            continue;
        };
        let n = sid
            .parse::<i64>()
            .ok()
            .filter(|n| known.contains(&n.to_string()) || attached.contains_key(n))
            .ok_or_else(|| {
                bad(format!(
                    "La programación «{}» no existe o este usuario no puede usarla.",
                    shown(&sid)
                ))
            })?;
        if !shared.contains(&n) && !schedules.iter().any(|s| s.id == Some(n)) {
            shared.push(n);
        }
    }

    let alerts = if c.admin() && c.alerts_visible && state.is_none_or(|s| s.alerts.is_some()) {
        let list = catalog.get("alert");
        let mut names: Vec<String> = Vec::new();
        for row in rows(values, "alerts") {
            let Some(a) = row.opt("alert") else { continue };
            let a = pick(&a, list, |a| format!("La alerta «{a}» no existe."))?.to_string();
            if !names.iter().any(|n| n.eq_ignore_ascii_case(&a)) {
                names.push(a);
            }
        }
        Some(names)
    } else {
        None
    };

    let email = notify(
        v,
        "notify_email",
        "email_operator",
        "email_when",
        "Correo electrónico",
        catalog,
    )?;
    let page = notify(
        v,
        "notify_page",
        "page_operator",
        "page_when",
        "Localizador",
        catalog,
    )?;
    let netsend = notify(
        v,
        "notify_netsend",
        "netsend_operator",
        "netsend_when",
        "Net send",
        catalog,
    )?;
    let eventlog = if v.bool("notify_eventlog") {
        level(v, "eventlog_when")?
    } else {
        0
    };
    let delete = if v.bool("delete_job") {
        level(v, "delete_when")?
    } else {
        0
    };
    let targets = if c.msx && v.text("target") == "multi" {
        let list = catalog.get("target_server");
        let mut servers = Vec::new();
        for t in v.strings("target_servers") {
            let t = pick(&t, list, |t| {
                format!("«{t}» no es un servidor de destino de este servidor maestro.")
            })?
            .to_string();
            if !servers.contains(&t) {
                servers.push(t);
            }
        }
        if servers.is_empty() {
            return Err(bad(
                "Destinos: elegí al menos un servidor de destino.".into()
            ));
        }
        Targets::Servers(servers)
    } else if !c.msx && state.is_some_and(|s| s.raw.servers.iter().any(|x| x.id != 0)) {
        // A multiserver job seen from a server that isn't its master
        // anymore: its targets stay as they are.
        Targets::Servers(
            state
                .map(|s| s.raw.servers.iter().map(|x| x.name.clone()).collect())
                .unwrap_or_default(),
        )
    } else {
        Targets::Local
    };
    Ok(JobSpec {
        name,
        enabled,
        description,
        category,
        owner,
        steps,
        start_step,
        schedules,
        shared,
        alerts,
        email,
        page,
        netsend,
        eventlog,
        delete,
        targets,
    })
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// A schedule of the job's own: `sp_add_jobschedule`, as SSMS writes it,
/// which gives it to the job's owner (`sp_add_schedule` would give it to
/// whoever saves, and the owner couldn't change or delete it then).
fn new_schedule(sc: &mut Script, s: &OwnSchedule) {
    let mut args = vec![
        ("job_id", A::R("@jobId".into())),
        ("name", A::T(s.name.clone())),
    ];
    args.extend(s.freq.args());
    sc.call("msdb.dbo.sp_add_jobschedule", args);
}

fn attach(sc: &mut Script, id: i64) {
    sc.call(
        "msdb.dbo.sp_attach_schedule",
        vec![("job_id", A::R("@jobId".into())), ("schedule_id", A::I(id))],
    );
}

fn notify_args(a: &mut Vec<(&'static str, A)>, level: &'static str, op: &'static str, n: &Notify) {
    a.push((level, A::I(n.level)));
    if let Some(o) = n.operator.clone().filter(|_| n.level > 0) {
        a.push((op, A::T(o)));
    }
}

/// The T-SQL that creates the job: SSMS's "Script Job as CREATE" shape.
pub(crate) fn create_script(j: &JobSpec, login: &str) -> String {
    let mut sc = Script::new(
        &format!("Job del Agente SQL Server «{}»", j.name),
        &["@jobId uniqueidentifier"],
    );
    let mut args: Vec<(&'static str, A)> = vec![
        ("job_name", A::T(j.name.clone())),
        ("enabled", A::I(i64::from(j.enabled))),
        ("notify_level_eventlog", A::I(j.eventlog)),
    ];
    notify_args(
        &mut args,
        "notify_level_email",
        "notify_email_operator_name",
        &j.email,
    );
    notify_args(
        &mut args,
        "notify_level_page",
        "notify_page_operator_name",
        &j.page,
    );
    notify_args(
        &mut args,
        "notify_level_netsend",
        "notify_netsend_operator_name",
        &j.netsend,
    );
    args.push(("delete_level", A::I(j.delete)));
    if !j.description.is_empty() {
        args.push(("description", A::T(j.description.clone())));
    }
    args.push((
        "category_name",
        A::T(j.category.clone().unwrap_or_else(|| UNCATEGORIZED.into())),
    ));
    if let Some(o) = j.owner.clone().filter(|o| !o.eq_ignore_ascii_case(login)) {
        args.push(("owner_login_name", A::T(o)));
    }
    args.push(("job_id", A::R("@jobId OUTPUT".into())));
    sc.call("msdb.dbo.sp_add_job", args);
    write_steps(&mut sc, &plan_steps(&[], &j.steps));
    if !j.steps.is_empty() {
        sc.call(
            "msdb.dbo.sp_update_job",
            vec![
                ("job_id", A::R("@jobId".into())),
                ("start_step_id", A::I(j.start_step)),
            ],
        );
    }
    for s in &j.schedules {
        new_schedule(&mut sc, s);
    }
    for id in &j.shared {
        attach(&mut sc, *id);
    }
    for a in j.alerts.iter().flatten() {
        sc.call(
            "msdb.dbo.sp_update_alert",
            vec![("name", A::T(a.clone())), ("job_id", A::R("@jobId".into()))],
        );
    }
    match &j.targets {
        Targets::Local => sc.call(
            "msdb.dbo.sp_add_jobserver",
            vec![
                ("job_id", A::R("@jobId".into())),
                ("server_name", A::R("N'(local)'".into())),
            ],
        ),
        Targets::Servers(list) => {
            for t in list {
                sc.call(
                    "msdb.dbo.sp_add_jobserver",
                    vec![
                        ("job_id", A::R("@jobId".into())),
                        ("server_name", A::T(t.clone())),
                    ],
                );
            }
        }
    }
    sc.finish(Some("SELECT @jobId AS job_id;"))
}

/// The current notification of a kind.
fn cur_notify(level: i64, op: &Option<String>) -> Notify {
    Notify {
        level,
        operator: op.clone(),
    }
}

/// The `sp_update_job` arguments for a notification that changed.
fn notify_changes(
    a: &mut Vec<(&'static str, A)>,
    level: &'static str,
    op: &'static str,
    d: &Notify,
    o: &Notify,
) {
    if d.level != o.level {
        a.push((level, A::I(d.level)));
    }
    let same_op = d
        .operator
        .as_deref()
        .unwrap_or("")
        .eq_ignore_ascii_case(o.operator.as_deref().unwrap_or(""));
    if d.level > 0 && !same_op {
        a.push((op, A::T(d.operator.clone().unwrap_or_default())));
    } else if d.level == 0 && o.operator.is_some() && o.level > 0 {
        // As SSMS does when a box is unticked: no operator either.
        a.push((op, A::T(String::new())));
    }
}

/// The T-SQL that changes the job in place to `j`, or `None` when nothing
/// differs. `c`: who saves (a schedule taken off the job is deleted only
/// where msdb lets this login delete it).
pub(crate) fn alter_script(st: &JobState, j: &JobSpec, c: &Ctx) -> Option<String> {
    let cur: &JobRow = &st.raw.job;
    let mut sc = Script::new(
        &format!("Cambios en el job del Agente SQL Server «{}»", cur.name),
        &["@jobId uniqueidentifier"],
    );
    sc.line(&format!("SET @jobId = {};", guid_literal(&st.id)));
    let before = sc.calls();

    // Steps.
    write_steps(&mut sc, &plan_steps(&st.steps, &j.steps));
    if moves_table_logs(&st.steps, &j.steps) {
        sc.warn(STEP_LOGS_MOVE);
    }

    // The job itself, once its steps are in place (its start step must be
    // one of them).
    let mut a: Vec<(&'static str, A)> = vec![("job_id", A::R("@jobId".into()))];
    if j.name != cur.name {
        a.push(("new_name", A::T(j.name.clone())));
    }
    if j.enabled != cur.enabled {
        a.push(("enabled", A::I(i64::from(j.enabled))));
    }
    if j.description != cur.description.clone().unwrap_or_default() {
        a.push(("description", A::T(j.description.clone())));
    }
    if let Some(cat) = &j.category {
        if !cur
            .category
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(cat))
        {
            a.push(("category_name", A::T(cat.clone())));
        }
    }
    if let Some(o) = &j.owner {
        if !cur
            .owner
            .as_deref()
            .is_some_and(|c| c.eq_ignore_ascii_case(o))
        {
            a.push(("owner_login_name", A::T(o.clone())));
        }
    }
    if j.eventlog != cur.notify_eventlog {
        a.push(("notify_level_eventlog", A::I(j.eventlog)));
    }
    notify_changes(
        &mut a,
        "notify_level_email",
        "notify_email_operator_name",
        &j.email,
        &cur_notify(cur.notify_email, &cur.email_operator),
    );
    notify_changes(
        &mut a,
        "notify_level_page",
        "notify_page_operator_name",
        &j.page,
        &cur_notify(cur.notify_page, &cur.page_operator),
    );
    notify_changes(
        &mut a,
        "notify_level_netsend",
        "notify_netsend_operator_name",
        &j.netsend,
        &cur_notify(cur.notify_netsend, &cur.netsend_operator),
    );
    if j.delete != cur.delete_level {
        a.push(("delete_level", A::I(j.delete)));
    }
    // msdb leaves `start_step_id` as it was when steps move: the number
    // is compared, not the step.
    if !j.steps.is_empty() && j.start_step != cur.start_step {
        a.push(("start_step_id", A::I(j.start_step)));
    }
    // Without steps msdb keeps the old start step, and `sp_update_job`
    // checks the one it keeps (up to the last step + 1): any other change
    // of the job sets it back to 1.
    if j.steps.is_empty() && cur.start_step != 1 && a.len() > 1 {
        a.push(("start_step_id", A::I(1)));
    }
    if a.len() > 1 {
        sc.call("msdb.dbo.sp_update_job", a);
    }

    // Schedules: detach what went, update what changed, add what's new.
    let wanted: BTreeSet<i64> = j
        .schedules
        .iter()
        .filter_map(|s| s.id)
        .chain(j.shared.iter().copied())
        .collect();
    let mut deleted = Vec::new();
    for r in &st.raw.schedules {
        let Some(id) = r.id else { continue };
        if !wanted.contains(&id) {
            // Deleted when no other job uses it, where msdb lets this login
            // delete it (it refuses the whole detach otherwise, 14394): a
            // shared one, or another login's, is only detached.
            let delete = r.job_count <= 1 && st.owns_schedule(c, id) == Some(true);
            if delete {
                deleted.push(r.name.clone());
            }
            sc.call(
                "msdb.dbo.sp_detach_schedule",
                vec![
                    ("job_id", A::R("@jobId".into())),
                    ("schedule_id", A::I(id)),
                    ("delete_unused_schedule", A::I(i64::from(delete))),
                ],
            );
        }
    }
    if !deleted.is_empty() {
        sc.warn(&format!(
            "Las programaciones que se quitan y ningún otro job usa se borran: {}.",
            quoted(&deleted)
        ));
    }
    for s in &j.schedules {
        let Some(id) = s.id else { continue };
        let Some(r) = st.raw.schedules.iter().find(|r| r.id == Some(id)) else {
            continue;
        };
        if let Some(script) = schedule_update(id, &r.name, &s.name, &Freq::of(r), &s.freq) {
            sc.call("msdb.dbo.sp_update_schedule", script);
        }
    }
    let attached: BTreeSet<i64> = st.raw.schedules.iter().filter_map(|r| r.id).collect();
    for s in j.schedules.iter().filter(|s| s.id.is_none()) {
        new_schedule(&mut sc, s);
    }
    for id in j.shared.iter().filter(|id| !attached.contains(id)) {
        attach(&mut sc, *id);
    }

    // Alerts that run it.
    if let (Some(want), Some(have)) = (&j.alerts, &st.alerts) {
        for a in have {
            if !want.iter().any(|w| w.eq_ignore_ascii_case(a)) {
                sc.call(
                    "msdb.dbo.sp_update_alert",
                    vec![("name", A::T(a.clone())), ("job_id", A::R(NO_JOB.into()))],
                );
            }
        }
        for w in want {
            if !have.iter().any(|a| a.eq_ignore_ascii_case(w)) {
                sc.call(
                    "msdb.dbo.sp_update_alert",
                    vec![("name", A::T(w.clone())), ("job_id", A::R("@jobId".into()))],
                );
            }
        }
    }

    // Target servers.
    let have: Vec<String> = st
        .raw
        .servers
        .iter()
        .map(|s| {
            if s.id == 0 {
                "(local)".to_string()
            } else {
                s.name.clone()
            }
        })
        .collect();
    let want: Vec<String> = match &j.targets {
        Targets::Local => vec!["(local)".into()],
        Targets::Servers(list) => list.clone(),
    };
    for h in &have {
        if !want.iter().any(|w| w.eq_ignore_ascii_case(h)) {
            sc.call(
                "msdb.dbo.sp_delete_jobserver",
                vec![
                    ("job_id", A::R("@jobId".into())),
                    ("server_name", server_arg(h)),
                ],
            );
        }
    }
    for w in &want {
        if !have.iter().any(|h| h.eq_ignore_ascii_case(w)) {
            sc.call(
                "msdb.dbo.sp_add_jobserver",
                vec![
                    ("job_id", A::R("@jobId".into())),
                    ("server_name", server_arg(w)),
                ],
            );
        }
    }

    (sc.calls() > before).then(|| sc.finish(None))
}

fn server_arg(name: &str) -> A {
    if name == "(local)" {
        A::R("N'(local)'".into())
    } else {
        A::T(name.to_string())
    }
}

/// `sp_update_schedule`'s arguments for a schedule of the job that
/// changed.
fn schedule_update(
    id: i64,
    old_name: &str,
    name: &str,
    old: &Freq,
    f: &Freq,
) -> Option<Vec<(&'static str, A)>> {
    let mut args = vec![("schedule_id", A::I(id))];
    if name != old_name {
        args.push(("new_name", A::T(name.to_string())));
    }
    args.extend(f.changed(old));
    (args.len() > 1).then_some(args)
}

/// `sp_delete_job`; schedules no other job uses go with it, as in SSMS.
/// msdb also deletes the job's history and takes it off the alerts that
/// run it: each, where it applies, is a warning. The history is the rows
/// msdb holds, where they were read; else the job's last run (which
/// outlives a purge of its history).
pub(crate) fn drop_script(st: &JobState) -> String {
    let name = &st.raw.job.name;
    let mut sc = Script::new(&format!("Borra el job del Agente SQL Server «{name}»"), &[]);
    if st.history.unwrap_or(st.raw.job.last_run.is_some()) {
        sc.warn(DELETES_HISTORY);
    }
    let unused: Vec<String> = st
        .raw
        .schedules
        .iter()
        .filter(|r| r.job_count <= 1)
        .map(|r| r.name.clone())
        .collect();
    if !unused.is_empty() {
        sc.warn(&format!(
            "Se borran también las programaciones que solo usa este job: {}.",
            quoted(&unused)
        ));
    }
    if let Some(alerts) = st.alerts.as_ref().filter(|a| !a.is_empty()) {
        sc.warn(&format!(
            "Las alertas que ejecutan este job se quedan sin job que ejecutar: {}.",
            quoted(alerts)
        ));
    }
    sc.call(
        "msdb.dbo.sp_delete_job",
        vec![
            ("job_id", A::R(guid_literal(&st.id))),
            ("delete_unused_schedule", A::I(1)),
        ],
    );
    sc.finish(None)
}

// ---------------------------------------------------------------------------
// Reading
// ---------------------------------------------------------------------------

/// The alerts that run the job (`@P1`), whether msdb holds history of it,
/// where the login may read them, and the owners of its schedules `ids`
/// (integers msdb gave; the SQLAgent roles read
/// `sysschedules_localserver_view` but not `sysjobschedules`).
pub(crate) fn extra_sql(ids: &[i64]) -> String {
    let mut sql = String::from(
        "SET NOCOUNT ON;
IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysalerts', N'OBJECT', N'SELECT') = 1
    SELECT name AS dbine_alert FROM msdb.dbo.sysalerts WHERE job_id = @P1 ORDER BY name;
IF HAS_PERMS_BY_NAME(N'msdb.dbo.sysjobhistory', N'OBJECT', N'SELECT') = 1
    SELECT TOP (1) 1 AS dbine_history FROM msdb.dbo.sysjobhistory WHERE job_id = @P1;",
    );
    if !ids.is_empty() {
        let list: Vec<String> = ids.iter().map(i64::to_string).collect();
        sql.push_str(&format!(
            "\nIF HAS_PERMS_BY_NAME(N'msdb.dbo.sysschedules_localserver_view', N'OBJECT', N'SELECT') = 1
    SELECT schedule_id AS dbine_schedule, SUSER_SNAME(owner_sid) AS dbine_schedule_owner
      FROM msdb.dbo.sysschedules_localserver_view WHERE schedule_id IN ({});",
            list.join(", ")
        ));
    }
    sql
}

async fn read_state(s: &mut SqlServerSession, c: &Ctx, id: &str) -> Result<JobState> {
    let (raw, rows) = jobs::read_job_as(s, &c.p, id).await?;
    let proxies = jobs::proxy_names(s, &rows).await;
    let steps = rows.iter().map(|r| Step::of(r, &proxies)).collect();
    let ids: Vec<i64> = raw.schedules.iter().filter_map(|r| r.id).collect();
    let sets = jobs::sets(s, &extra_sql(&ids), &[id], None).await?;
    let alerts = first_with(&sets, "dbine_alert").map(|set| {
        set.rows
            .iter()
            .filter_map(|r| text(r, "dbine_alert"))
            .collect()
    });
    let schedule_owners = first_with(&sets, "dbine_schedule")
        .map(|set| {
            set.rows
                .iter()
                .filter_map(|r| {
                    Some((
                        jobs::int(r, "dbine_schedule")?,
                        text(r, "dbine_schedule_owner"),
                    ))
                })
                .collect()
        })
        .unwrap_or_default();
    // An empty set still has its column: no history.
    let history = first_with(&sets, "dbine_history").map(|set| !set.rows.is_empty());
    Ok(JobState {
        id: id.to_string(),
        raw,
        steps,
        alerts,
        schedule_owners,
        history,
    })
}

/// The form was read from an older version of the job (msdb counts every
/// change in `version_number`): its step ids may point at other steps now.
pub(crate) fn stale(values: &FormValues, st: &JobState) -> Result<()> {
    let seen = Vals(&values.values)
        .int("version_number", "Versión", 0, i64::MAX)
        .ok()
        .flatten();
    match (seen, st.raw.job.version) {
        (Some(seen), Some(now)) if seen != now => Err(Error::Query(CHANGED.into())),
        _ => Ok(()),
    }
}

/// The login may change or delete this job: msdb refuses a job a master
/// server sent (14274, in `sp_update_job` and `sp_delete_job` alike) and
/// one of another owner to whoever isn't sysadmin (14525).
pub(crate) fn may_change(c: &Ctx, job: &JobRow) -> std::result::Result<(), &'static str> {
    if c.p.sent_by_master(job) {
        return Err(FROM_MASTER);
    }
    if !(c.p.sysadmin || c.is(job.owner.as_deref())) {
        return Err(OWNER_ONLY);
    }
    Ok(())
}

const SOURCES: [&str; 9] = [
    "login",
    "database",
    "category",
    "operator",
    "alert",
    "schedule",
    "proxy",
    "subsystem",
    "target_server",
];

pub(crate) async fn plan(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let catalog = choices::load(s, c, &SOURCES).await?;
    match id {
        None => {
            let j = spec(values, &catalog, c, None)?;
            Ok(Plan {
                script: create_script(&j, &c.p.login),
                empty: false,
                id: None,
                output: Some("job_id"),
                lookup: None,
            })
        }
        Some(id) => {
            let id = jobs::job_id(id)?;
            let st = read_state(s, c, &id).await?;
            if let Err(why) = may_change(c, &st.raw.job) {
                return Err(Error::Query(why.into()));
            }
            stale(values, &st)?;
            let j = spec(values, &catalog, c, Some(&st))?;
            Ok(match alter_script(&st, &j, c) {
                Some(script) => Plan {
                    script,
                    empty: false,
                    id: Some(id),
                    output: None,
                    lookup: None,
                },
                None => Plan::nothing(&st.raw.job.name, id),
            })
        }
    }
}

pub(crate) async fn delete_script(s: &mut SqlServerSession, c: &Ctx, id: &str) -> Result<String> {
    let id = jobs::job_id(id)?;
    let st = read_state(s, c, &id).await?;
    if c.writable() {
        if let Err(why) = may_change(c, &st.raw.job) {
            return Err(Error::Query(why.into()));
        }
    }
    Ok(drop_script(&st))
}

/// This login may start the job at one of its steps: `sp_start_job
/// @step_name` applies to a local job (`sp_help_job`'s `type` 1; a
/// multiserver job is posted to its targets whole, one without a target
/// server doesn't start) that has steps, and runs it for its owner,
/// SQLAgentOperatorRole and sysadmin.
pub(crate) fn starts_at_step(c: &Ctx, j: &JobRow) -> bool {
    j.kind == 1 && j.steps > 0 && c.p.manages(j.owner.as_deref())
}

/// This login may purge the job's history (`sp_purge_jobhistory`, given
/// its EXECUTE): sysadmin and SQLAgentOperatorRole any job's; whoever else
/// was granted it (a SQLAgentReaderRole member sees every job), only the
/// jobs it owns (14392 otherwise). The procedure checks nothing else: a
/// multiserver job's history purges like a local one's.
pub(crate) fn purges_history(c: &Ctx, j: &JobRow) -> bool {
    c.p.manages(j.owner.as_deref())
}

/// A job in the tree, with what this login may do with it.
pub(crate) fn item(c: &Ctx, j: &JobRow) -> SchedulerItem {
    let mine = super::unless(c.writable(), may_change(c, j).is_ok());
    SchedulerItem {
        id: j.id.to_string(),
        name: j.name.clone(),
        description: j.description.clone(),
        enabled: Some(j.enabled),
        system: jobs::built_in(j),
        can_edit: mine,
        can_delete: mine,
        actions: None,
        run_from_step: super::unless(c.run_from_step(), starts_at_step(c, j)),
        clear_history: super::unless(c.writable() && c.purge_history, purges_history(c, j)),
    }
}

pub(crate) async fn items(s: &mut SqlServerSession, c: &Ctx) -> Result<SchedulerList> {
    let sets = jobs::sets(s, "SET NOCOUNT ON;\nEXEC msdb.dbo.sp_help_job;", &[], None).await?;
    let mut jobs_list: Vec<JobRow> =
        jobs::read_all(first_with(&sets, "current_execution_status"), JobRow::read);
    jobs_list.sort_by_key(|j| j.name.to_lowercase());
    Ok(SchedulerList {
        items: jobs_list.iter().map(|j| item(c, j)).collect(),
        notes: Vec::new(),
    })
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

fn level_select() -> FormKind {
    FormKind::select(&[
        ("success", "Cuando el job se complete correctamente"),
        ("failure", "Cuando el job genere un error"),
        ("completion", "Cuando el job se complete"),
    ])
}

fn action_select() -> FormKind {
    FormKind::select(&[
        (
            "quit_success",
            "Salir del job e informar de que se completó correctamente",
        ),
        ("quit_failure", "Salir del job e informar del error"),
        ("next", "Ir al paso siguiente"),
        ("goto", "Ir al paso…"),
    ])
}

/// The step editor: SSMS's "Job Step Properties" (General and Advanced).
pub(crate) fn step_fields(catalog: &Catalog, admin: bool, current: &[Step]) -> Vec<FormField> {
    let available = catalog.subsystems();
    let mut types = FormKind::select(&[
        ("TSQL", "Script de Transact-SQL (T-SQL)"),
        ("CmdExec", "Comando del sistema operativo (CmdExec)"),
        ("PowerShell", "PowerShell"),
        ("SSIS", "Paquete de SQL Server Integration Services"),
        ("ANALYSISQUERY", "Consulta de SQL Server Analysis Services"),
        ("ANALYSISCOMMAND", "Comando de SQL Server Analysis Services"),
        ("Snapshot", "Instantánea de replicación"),
        (
            "LogReader",
            "Lector del registro de transacciones de replicación",
        ),
        ("Distribution", "Distribuidor de replicación"),
        ("Merge", "Mezcla de replicación"),
        ("QueueReader", "Lector de cola de replicación"),
        ("ActiveScripting", "Script ActiveX"),
    ]);
    if let FormKind::Select { options } = &mut types {
        options.retain(|(v, _)| {
            available.is_empty()
                || available.iter().any(|a| is(a, v))
                || current.iter().any(|s| is(&s.subsystem, v))
        });
    }
    let others = &[
        "SSIS",
        "ANALYSISQUERY",
        "ANALYSISCOMMAND",
        "Snapshot",
        "LogReader",
        "Distribution",
        "Merge",
        "QueueReader",
        "ActiveScripting",
    ];
    vec![
        FormField::new("name", "Nombre del paso", FormKind::Text).required(),
        FormField::new("subsystem", "Tipo", types)
            .required()
            .default(json!("TSQL")),
        FormField::new("proxy_cmdexec", "Ejecutar como", FormKind::reference("proxy:CmdExec", false))
            .when("subsystem", &["CmdExec"])
            .default(json!("")),
        FormField::new("proxy_powershell", "Ejecutar como", FormKind::reference("proxy:PowerShell", false))
            .when("subsystem", &["PowerShell"])
            .default(json!("")),
        FormField::new("proxy_ssis", "Ejecutar como", FormKind::reference("proxy:SSIS", false))
            .when("subsystem", &["SSIS"])
            .default(json!("")),
        FormField::new("proxy_analysisquery", "Ejecutar como", FormKind::reference("proxy:ANALYSISQUERY", false))
            .when("subsystem", &["ANALYSISQUERY"])
            .default(json!("")),
        FormField::new("proxy_analysiscommand", "Ejecutar como", FormKind::reference("proxy:ANALYSISCOMMAND", false))
            .when("subsystem", &["ANALYSISCOMMAND"])
            .default(json!("")),
        FormField::new("proxy_snapshot", "Ejecutar como", FormKind::reference("proxy:Snapshot", false))
            .when("subsystem", &["Snapshot"])
            .default(json!("")),
        FormField::new("proxy_logreader", "Ejecutar como", FormKind::reference("proxy:LogReader", false))
            .when("subsystem", &["LogReader"])
            .default(json!("")),
        FormField::new("proxy_distribution", "Ejecutar como", FormKind::reference("proxy:Distribution", false))
            .when("subsystem", &["Distribution"])
            .default(json!("")),
        FormField::new("proxy_merge", "Ejecutar como", FormKind::reference("proxy:Merge", false))
            .when("subsystem", &["Merge"])
            .default(json!("")),
        FormField::new("proxy_queuereader", "Ejecutar como", FormKind::reference("proxy:QueueReader", false))
            .when("subsystem", &["QueueReader"])
            .default(json!("")),
        FormField::new("proxy_activescripting", "Ejecutar como", FormKind::reference("proxy:ActiveScripting", false))
            .when("subsystem", &["ActiveScripting"])
            .default(json!("")),
        FormField::new("database", "Base de datos", FormKind::reference("database", false))
            .when("subsystem", &["TSQL"])
            .default(json!("master")),
        FormField::new("server", "Servidor", FormKind::Text)
            .when("subsystem", &["ANALYSISQUERY", "ANALYSISCOMMAND"]),
        FormField::new("as_database", "Base de datos", FormKind::Text)
            .when("subsystem", &["ANALYSISQUERY", "ANALYSISCOMMAND"]),
        FormField::new("command", "Comando", FormKind::code("sql")).when("subsystem", &["TSQL"]),
        FormField::new("command_powershell", "Comando", FormKind::code("powershell"))
            .when("subsystem", &["PowerShell"]),
        FormField::new("command_cmdexec", "Comando", FormKind::code("shell"))
            .when("subsystem", &["CmdExec"]),
        FormField::new("command_other", "Comando", FormKind::code("text")).when("subsystem", others),
        FormField::new(
            "success_code",
            "Código de salida de un comando correcto",
            FormKind::number(Some(f64::from(i32::MIN)), Some(f64::from(i32::MAX))),
        )
        .when("subsystem", &["CmdExec"])
        .default(json!(0)),
        FormField::new("on_success", "Acción en caso de éxito", action_select())
            .default(json!("next")),
        FormField::new("on_success_step", "Paso al que ir si se completa", FormKind::reference("list:steps", false))
            .when("on_success", &["goto"]),
        FormField::new("retry_attempts", "Reintentos", FormKind::number(Some(0.0), Some(9999.0)))
            .default(json!(0)),
        FormField::new(
            "retry_interval",
            "Intervalo de reintento (minutos)",
            FormKind::number(Some(0.0), Some(9999.0)),
        )
        .default(json!(0)),
        FormField::new("on_fail", "Acción en caso de error", action_select())
            .default(json!("quit_failure")),
        FormField::new("on_fail_step", "Paso al que ir si falla", FormKind::reference("list:steps", false))
            .when("on_fail", &["goto"]),
        FormField::new("output_file", "Archivo de salida", FormKind::Text)
            .read_only(!admin)
            .help("Solo un miembro de sysadmin puede elegir el archivo de salida."),
        FormField::new("append_output", "Anexar la salida al archivo existente", FormKind::Bool),
        FormField::new("log_to_table", "Registrar en tabla", FormKind::Bool),
        FormField::new(
            "append_to_table",
            "Anexar la salida a la entrada existente en la tabla",
            FormKind::Bool,
        )
        .when("log_to_table", &["true"]),
        FormField::new(
            "output_in_history",
            "Incluir la salida del paso en el historial",
            FormKind::Bool,
        ),
        FormField::new("database_user", "Ejecutar como usuario", FormKind::Text)
            .when("subsystem", &["TSQL"])
            .read_only(!admin)
            .help("Usuario de la base de datos con el que se ejecuta el paso. Solo un miembro de sysadmin puede elegirlo."),
    ]
}

/// A step's row: its fields' values and its id (kept by the app).
pub(crate) fn step_row(s: &Step, id: i64, current: &[Step]) -> BTreeMap<String, Value> {
    let name_of = |id: i64| {
        current
            .get((id - 1).max(0) as usize)
            .filter(|_| id >= 1)
            .map(|s| s.name.clone())
            .unwrap_or_default()
    };
    let mut r = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        r.insert(k.to_string(), v);
    };
    put("step_id", json!(id));
    put("name", json!(s.name));
    put("subsystem", json!(s.subsystem));
    put(command_key(&s.subsystem), json!(s.command));
    if s.tsql() {
        put(
            "database",
            json!(s.database.clone().unwrap_or_else(|| "master".into())),
        );
        put(
            "database_user",
            json!(s.database_user.clone().unwrap_or_default()),
        );
    } else {
        put("database", json!("master"));
        put(
            proxy_key(&s.subsystem),
            json!(s.proxy.clone().unwrap_or_default()),
        );
    }
    if analysis(&s.subsystem) {
        put("server", json!(s.server.clone().unwrap_or_default()));
        put("as_database", json!(s.database.clone().unwrap_or_default()));
    }
    put("success_code", json!(s.success_code));
    put("on_success", json!(action_key(s.on_success)));
    if s.on_success == 4 {
        put("on_success_step", json!(name_of(s.on_success_step)));
    }
    put("on_fail", json!(action_key(s.on_fail)));
    if s.on_fail == 4 {
        put("on_fail_step", json!(name_of(s.on_fail_step)));
    }
    put("retry_attempts", json!(s.retry_attempts));
    put("retry_interval", json!(s.retry_interval));
    put(
        "output_file",
        json!(s.output_file.clone().unwrap_or_default()),
    );
    put("append_output", json!(s.flags & 2 != 0));
    put("log_to_table", json!(s.flags & (8 | 16) != 0));
    put("append_to_table", json!(s.flags & 16 != 0));
    put("output_in_history", json!(s.flags & (4 | 32) != 0));
    put("os_run_priority", json!(s.os_priority));
    r
}

/// A server-local time as the form shows it.
fn when_text(t: Option<chrono::NaiveDateTime>) -> String {
    t.map(|t| t.format("%Y-%m-%d %H:%M:%S").to_string())
        .unwrap_or_default()
}

pub(crate) async fn form(
    s: &mut SqlServerSession,
    c: &Ctx,
    id: Option<&str>,
) -> Result<ObjectForm> {
    let catalog = choices::load(s, c, &SOURCES).await?;
    let state = match id {
        Some(id) => Some(read_state(s, c, &jobs::job_id(id)?).await?),
        None => None,
    };
    let today = schedule::today(c);
    let current = state.as_ref().map_or(&[][..], |st| st.steps.as_slice());
    let mut f = ObjectForm::default();
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    let mut put = |k: &str, v: Value| {
        values.insert(k.to_string(), v);
    };

    // General.
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new("name", "Nombre", FormKind::Text).required(),
        FormField::new("owner", "Propietario", FormKind::reference("login", false))
            .read_only(!c.p.sysadmin)
            .default(json!(c.p.login)),
        FormField::new(
            "category",
            "Categoría",
            FormKind::reference("category:job", false),
        )
        .default(json!(UNCATEGORIZED)),
        FormField::new("description", "Descripción", FormKind::Textarea),
        FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
    ]);
    let mut step_rows = Vec::new();
    let mut own_rows = Vec::new();
    let mut shared_rows = Vec::new();
    let mut alert_rows = Vec::new();
    match &state {
        None => {
            put("owner", json!(c.p.login));
            put("category", json!(UNCATEGORIZED));
            put("enabled", json!(true));
            put("target", json!("local"));
            for (k, w) in [
                ("email_when", "failure"),
                ("page_when", "failure"),
                ("netsend_when", "failure"),
                ("eventlog_when", "failure"),
                ("delete_when", "success"),
            ] {
                put(k, json!(w));
            }
        }
        Some(st) => {
            let j = &st.raw.job;
            general = general.fields(vec![
                FormField::new("origin", "Servidor de origen", FormKind::Text).read_only(true),
                FormField::new("created", "Fecha de creación", FormKind::Text).read_only(true),
                FormField::new("modified", "Última modificación", FormKind::Text).read_only(true),
                FormField::new("last_run", "Última ejecución", FormKind::Text).read_only(true),
            ]);
            put("name", json!(j.name));
            put("owner", json!(j.owner.clone().unwrap_or_default()));
            put("category", json!(j.category.clone().unwrap_or_default()));
            put(
                "description",
                json!(j.description.clone().unwrap_or_default()),
            );
            put("enabled", json!(j.enabled));
            put("origin", json!(j.origin.clone().unwrap_or_default()));
            put("created", json!(when_text(j.created)));
            put("modified", json!(when_text(j.modified)));
            put("last_run", json!(when_text(j.last_run)));
            // No field shows it: it comes back with the values, and a save
            // from a form older than the job is refused.
            if let Some(v) = j.version {
                put("version_number", json!(v));
            }
            if let Some(start) = st.steps.get((j.start_step - 1).max(0) as usize) {
                put("start_step", json!(start.name));
            }
            for (key, level, op) in [
                ("email", j.notify_email, &j.email_operator),
                ("page", j.notify_page, &j.page_operator),
                ("netsend", j.notify_netsend, &j.netsend_operator),
            ] {
                put(&format!("notify_{key}"), json!(level > 0));
                put(
                    &format!("{key}_operator"),
                    json!(op.clone().unwrap_or_default()),
                );
                put(&format!("{key}_when"), json!(level_key(level)));
            }
            put("notify_eventlog", json!(j.notify_eventlog > 0));
            put("eventlog_when", json!(level_key(j.notify_eventlog)));
            put("delete_job", json!(j.delete_level > 0));
            put(
                "delete_when",
                json!(if j.delete_level > 0 {
                    level_key(j.delete_level)
                } else {
                    "success"
                }),
            );
            let remote: Vec<String> = st
                .raw
                .servers
                .iter()
                .filter(|s| s.id != 0)
                .map(|s| s.name.clone())
                .collect();
            put(
                "target",
                json!(if remote.is_empty() { "local" } else { "multi" }),
            );
            put("target_servers", json!(remote));
            for (i, step) in st.steps.iter().enumerate() {
                step_rows.push(step_row(step, i as i64 + 1, &st.steps));
            }
            for r in &st.raw.schedules {
                let Some(sid) = r.id else { continue };
                // Another login's schedule goes with the shared ones (the
                // login can take it off the job, not change it).
                if r.job_count > 1 || st.owns_schedule(c, sid) == Some(false) {
                    shared_rows.push(BTreeMap::from([
                        ("schedule".to_string(), json!(sid.to_string())),
                        ("summary".to_string(), json!(jobs::describe(r))),
                    ]));
                } else {
                    let mut row = schedule::values(&r.name, &Freq::of(r));
                    row.insert("schedule_id".into(), json!(sid));
                    own_rows.push(row);
                }
            }
            for a in st.alerts.iter().flatten() {
                alert_rows.push(BTreeMap::from([("alert".to_string(), json!(a))]));
            }
        }
    }
    let admin = c.p.sysadmin;
    let steps = FormSection::new("steps", "Pasos")
        .list(
            FormList::new(
                "steps",
                "Pasos",
                "name",
                step_fields(&catalog, admin, current),
            )
            .orderable()
            .rows(step_rows),
        )
        .field(
            FormField::new(
                "start_step",
                "Paso inicial",
                FormKind::reference("list:steps", false),
            )
            .help("El paso con el que empieza cada ejecución del job."),
        );
    let schedules = FormSection::new("schedules", "Programaciones")
        .list(
            FormList::new(
                "schedules",
                "Programaciones",
                "name",
                schedule::fields(c.managed(), today),
            )
            .rows(own_rows),
        )
        .list(
            FormList::new(
                "shared_schedules",
                "Programaciones compartidas",
                "schedule",
                vec![
                    FormField::new(
                        "schedule",
                        "Programación",
                        FormKind::reference("schedule", false),
                    )
                    .required(),
                    FormField::new("summary", "Descripción", FormKind::Text).read_only(true),
                ],
            )
            .rows(shared_rows),
        );
    let alerts_editable =
        c.admin() && c.alerts_visible && state.as_ref().is_none_or(|s| s.alerts.is_some());
    let alerts = FormSection::new("alerts", "Alertas").list(
        FormList::new(
            "alerts",
            "Alertas que ejecutan este job",
            "alert",
            vec![FormField::new("alert", "Alerta", FormKind::reference("alert", false)).required()],
        )
        .rows(alert_rows)
        .read_only(!alerts_editable),
    );
    let notifications = FormSection::new("notifications", "Notificaciones").fields(vec![
        FormField::new("notify_email", "Correo electrónico", FormKind::Bool),
        FormField::new(
            "email_operator",
            "Operador",
            FormKind::reference("operator", false),
        )
        .when("notify_email", &["true"]),
        FormField::new("email_when", "Cuándo", level_select()).when("notify_email", &["true"]),
        FormField::new("notify_page", "Localizador", FormKind::Bool),
        FormField::new(
            "page_operator",
            "Operador",
            FormKind::reference("operator", false),
        )
        .when("notify_page", &["true"]),
        FormField::new("page_when", "Cuándo", level_select()).when("notify_page", &["true"]),
        FormField::new("notify_netsend", "Net send", FormKind::Bool),
        FormField::new(
            "netsend_operator",
            "Operador",
            FormKind::reference("operator", false),
        )
        .when("notify_netsend", &["true"]),
        FormField::new("netsend_when", "Cuándo", level_select()).when("notify_netsend", &["true"]),
        FormField::new(
            "notify_eventlog",
            "Escribir en el registro de eventos de aplicación de Windows",
            FormKind::Bool,
        ),
        FormField::new("eventlog_when", "Cuándo", level_select())
            .when("notify_eventlog", &["true"]),
        FormField::new(
            "delete_job",
            "Eliminar el job automáticamente",
            FormKind::Bool,
        ),
        FormField::new("delete_when", "Cuándo", level_select()).when("delete_job", &["true"]),
    ]);
    let targets = FormSection::new("targets", "Destinos").fields(vec![
        FormField::new(
            "target",
            "Destino",
            FormKind::select(&[("local", "Servidor local"), ("multi", "Varios servidores")]),
        )
        .read_only(!c.msx)
        .default(json!("local")),
        FormField::new(
            "target_servers",
            "Servidores de destino",
            FormKind::reference("target_server", true),
        )
        .read_only(!c.msx)
        .when("target", &["multi"]),
    ]);
    f.sections = vec![general, steps, schedules, alerts, notifications, targets];
    f.values = values;
    f.choices = catalog.lists;
    if !c.msx {
        f.notes.push(NOT_MSX.into());
    }
    if !c.alerts_visible {
        f.notes.push(NO_ALERTS.into());
    } else if !alerts_editable && c.writable() {
        f.notes.push(ALERTS_READ_ONLY.into());
    }
    if let Some(st) = &state {
        if st.raw.servers.is_empty() {
            f.notes.push(NO_TARGET.into());
        }
        if let Err(why) = may_change(c, &st.raw.job) {
            f.read_only = true;
            f.notes.insert(0, why.into());
        }
    }
    Ok(f)
}

// ---------------------------------------------------------------------------
// Start at a step, clear the history
// ---------------------------------------------------------------------------

/// SSMS's "Start Job at Step": `sp_start_job @step_name`, the step one the
/// job has.
pub(crate) async fn run_from(s: &mut SqlServerSession, id: &str, step: &str) -> Result<()> {
    let c = super::ready(s).await?;
    let id = jobs::job_id(id)?;
    let (_, steps) = jobs::read_job_as(s, &c.p, &id).await?;
    let Some(name) = steps
        .iter()
        .find(|r| r.name == step)
        .or_else(|| {
            steps
                .iter()
                .find(|r| r.name.eq_ignore_ascii_case(step.trim()))
        })
        .map(|r| r.name.clone())
    else {
        return Err(bad(format!(
            "El job no tiene un paso llamado «{}».",
            shown(step)
        )));
    };
    jobs::change(
        s,
        "EXEC msdb.dbo.sp_start_job @job_id = @P1, @step_name = @P2;",
        &[&id, &name],
    )
    .await
    .map(|_| ())
}

/// `sp_purge_jobhistory`: the job's whole history.
pub(crate) async fn clear_history(s: &mut SqlServerSession, id: &str) -> Result<()> {
    let _ = super::ready(s).await?;
    let id = jobs::job_id(id)?;
    jobs::change(
        s,
        "EXEC msdb.dbo.sp_purge_jobhistory @job_id = @P1;",
        &[&id],
    )
    .await
    .map(|_| ())
}
