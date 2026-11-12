//! SAP ASE: Job Scheduler, whose jobs live in `sybmgmtdb` and are read
//! and controlled through its procedures, as its guide asks: the list
//! from `sp_sjobhelp`, the runs from `sp_sjobhistory`, and run now, stop,
//! enable and disable from `sp_sjobcontrol`. Users without
//! `js_admin_role` see only their own scheduled jobs.
//!
//! `sp_sjobhelp`'s list columns aren't documented, so they're read by
//! name with alternatives (`sjob_id` or `id`…); `sp_sjobhistory`'s are
//! (`job_runid`, `sjob_id`, `job_state`, `job_start`, `job_end`…). The
//! scheduled job's id goes to the procedures as `@name`, only as digits.

use super::*;
use dbine_driver::{JobStep, ServerJob};

pub const NO_SCRIPT: &str = "Job Scheduler no informa el calendario de un job en columnas que DBine pueda leer con seguridad: el script se arma con sp_sjobhelp";

const KIND: &str = "Job Scheduler";

pub(crate) const NOW: &str = "SELECT getdate() AS LOCAL_NOW, getutcdate() AS UTC_NOW";
pub(super) const INSTALLED: &str = "SELECT db_id('sybmgmtdb') AS DBID";
pub(super) const ENABLED: &str =
    "SELECT value AS VALUE FROM master..sysconfigures WHERE name = 'enable job scheduler'";
pub(super) const ROLES: &str =
    "SELECT proc_role('js_admin_role') AS ADM, proc_role('js_user_role') AS USR";

pub(crate) const NOT_INSTALLED: &str =
    "Este servidor no tiene Job Scheduler instalado (no existe la base sybmgmtdb).";
pub(crate) const OFF: &str =
    "Job Scheduler está apagado: los jobs no se ejecutan hasta activar «enable job scheduler».";
pub(crate) const NO_ROLE: &str =
    "Este usuario no tiene js_user_role ni js_admin_role: no ve los jobs de Job Scheduler.";
pub(crate) const OWN_ONLY: &str = "Sin js_admin_role se ven solo los jobs de este usuario.";
const UNREADABLE: &str = "sp_sjobhelp no devolvió la lista en un formato que DBine reconozca.";

const ID: &[&str] = &["sjob_id", "id"];
const NAME: &[&str] = &["sjob_name", "name", "sjname"];
const OWNER: &[&str] = &["sjob_owner", "owner"];
const STATE: &[&str] = &["sjob_state", "state", "sjob_enabled", "enabled"];
const JOB: &[&str] = &["job_name", "sjob_jobname", "jname"];
const SCHED: &[&str] = &["sched_name", "sjob_schedname", "sname"];
const SERVER: &[&str] = &["sjob_server", "server"];
const DESC: &[&str] = &["job_desc", "jdesc", "job_description", "description"];
const REPEATS: &[&str] = &["sched_repeats", "repeats", "sched_interval"];
const START_TIME: &[&str] = &["sched_starttime", "starttime"];
const END_TIME: &[&str] = &["sched_endtime", "endtime"];
const START_DATE: &[&str] = &["sched_startdate", "startdate"];
const END_DATE: &[&str] = &["sched_enddate", "enddate"];
const SCHED_DAYS: &[&str] = &["sched_days", "days"];
const SCHED_DATES: &[&str] = &["sched_dates", "dates"];

/// What the user can do in Job Scheduler, or the note saying why the list
/// is empty.
pub(crate) struct Access {
    pub(crate) admin: bool,
    pub(crate) off: bool,
}

pub(crate) fn access(src: &mut dyn JobSource) -> Result<std::result::Result<Access, &'static str>> {
    let installed = src.query(INSTALLED, &[])?;
    if installed.get(0, "DBID").is_none() {
        return Ok(Err(NOT_INSTALLED));
    }
    let roles = src.query(ROLES, &[])?;
    let admin = int(roles.get(0, "ADM")).unwrap_or(0) > 0;
    let user = int(roles.get(0, "USR")).unwrap_or(0) > 0;
    if !admin && !user {
        return Ok(Err(NO_ROLE));
    }
    let off = src
        .query(ENABLED, &[])
        .ok()
        .and_then(|s| int(s.get(0, "VALUE")))
        .is_some_and(|v| v == 0);
    Ok(Ok(Access { admin, off }))
}

pub(crate) fn options(base: &str, admin: bool) -> String {
    if admin {
        format!("{base},all_users")
    } else {
        base.to_string()
    }
}

fn enabled(v: Option<&str>) -> bool {
    match v.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) => v.starts_with("enabl") || matches!(v.as_str(), "1" | "y" | "yes" | "true"),
        None => true,
    }
}

/// The schedule in `sp_sjobcreate`'s terms, from whatever columns the list
/// has.
fn schedule(s: &Set, r: usize) -> Option<String> {
    let parts: Vec<String> = [
        ("repeats", REPEATS),
        ("starttime", START_TIME),
        ("endtime", END_TIME),
        ("startdate", START_DATE),
        ("enddate", END_DATE),
        ("days", SCHED_DAYS),
        ("dates", SCHED_DATES),
    ]
    .iter()
    .filter_map(|(k, cols)| get(s, r, cols).map(|v| format!("{k}={v}")))
    .collect();
    if parts.is_empty() {
        return get(s, r, SCHED).map(str::to_string);
    }
    Some(parts.join(","))
}

fn job(s: &Set, r: usize) -> Option<ServerJob> {
    let id = int(get(s, r, ID))?;
    let state = get(s, r, STATE);
    let running = state.is_some_and(|v| {
        let v = v.trim().to_ascii_uppercase();
        v.starts_with("RUN") || v == "R1" || v == "R2"
    });
    Some(ServerJob {
        id: id.to_string(),
        name: get(s, r, NAME).unwrap_or_default().to_string(),
        kind: KIND.into(),
        group: owned(get(s, r, SERVER)),
        description: owned(get(s, r, DESC)),
        owner: owned(get(s, r, OWNER)),
        enabled: enabled(state),
        schedule: schedule(s, r),
        running,
        last_outcome: running.then_some(JobOutcome::Running),
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: true,
        // `terminate` acts on a run that's going on.
        can_stop: running,
        can_enable: true,
        has_history: true,
        has_script: false,
        system: false,
    })
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let a = match access(src)? {
        Ok(a) => a,
        Err(note) => {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![note.into()],
            })
        }
    };
    let mut notes: Vec<String> = Vec::new();
    if a.off {
        notes.push(OFF.into());
    }
    if !a.admin {
        notes.push(OWN_ONLY.into());
    }
    let s = src.query(
        &format!(
            "exec sybmgmtdb..sp_sjobhelp @option = {}",
            lit(&options("list,scheduled", a.admin))
        ),
        &[],
    )?;
    if !s.rows.is_empty() && !ID.iter().any(|c| s.has(c)) {
        tracing::debug!("odbc jobs: sp_sjobhelp columns: {:?}", s.cols);
        notes.push(UNREADABLE.into());
        return Ok(JobList {
            jobs: Vec::new(),
            notes,
        });
    }
    let mut jobs: Vec<ServerJob> = (0..s.rows.len()).filter_map(|r| job(&s, r)).collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    jobs.dedup_by(|a, b| a.id == b.id);
    // The last run of each, from one read of the history (best effort).
    let tz = Tz::query(src, NOW);
    let all = src
        .query(
            &format!(
                "exec sybmgmtdb..sp_sjobhistory @option = {}",
                lit(&options("list", a.admin))
            ),
            &[],
        )
        .map(|h| runs_of(&h, None, &tz))
        .unwrap_or_else(|e| {
            tracing::debug!("odbc jobs: sp_sjobhistory failed: {e}");
            Vec::new()
        });
    for j in jobs.iter_mut() {
        let mine: Vec<JobRun> = all
            .iter()
            .filter(|(id, _)| id.map(|i| i.to_string()).as_deref() == Some(j.id.as_str()))
            .map(|(_, r)| r.clone())
            .collect();
        with_last_run(j, &mine);
    }
    Ok(JobList { jobs, notes })
}

/// Fills the last finished run and the running flag from `runs` (newest
/// first).
fn with_last_run(j: &mut ServerJob, runs: &[JobRun]) {
    if let Some(last) = runs.iter().find(|r| r.outcome != JobOutcome::Running) {
        j.last_outcome = Some(last.outcome);
        j.last_started = last.started.clone();
        j.last_duration_ms = last.duration_ms;
    }
    if runs.iter().any(|r| r.outcome == JobOutcome::Running) {
        j.running = true;
        j.can_stop = true;
        j.last_outcome.get_or_insert(JobOutcome::Running);
    }
}

/// sp_sjobhelp's row for scheduled job `n`.
fn one(src: &mut dyn JobSource, id: &str) -> Result<(u64, bool, Set)> {
    let n = int_id(id)?;
    let a = access(src)?.map_err(|note| Error::Query(note.into()))?;
    let s = src.query(
        &format!(
            "exec sybmgmtdb..sp_sjobhelp @name = '{n}', @option = {}",
            lit(&options("list", a.admin))
        ),
        &[],
    )?;
    let r = (0..s.rows.len())
        .find(|r| int(get(&s, *r, ID)) == Some(n as i64))
        .ok_or_else(|| not_found(id))?;
    Ok((
        n,
        a.admin,
        Set {
            cols: s.cols.clone(),
            rows: vec![s.rows[r].clone()],
        },
    ))
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let (n, admin, s) = one(src, id)?;
    let mut job = job(&s, 0).ok_or_else(|| not_found(id))?;
    let tz = Tz::query(src, NOW);
    if let Ok(runs) = history(src, n, admin, &tz) {
        with_last_run(&mut job, &runs);
    }
    // The job's SQL text (best effort: sp_sjobcmd lists it).
    let cmd = src
        .query(
            &format!("exec sybmgmtdb..sp_sjobcmd @name = '{n}', @option = 'list'"),
            &[],
        )
        .ok()
        .map(|c| {
            (0..c.rows.len())
                .filter_map(|r| c.at(r, c.cols.len().saturating_sub(1)))
                .collect::<Vec<_>>()
                .concat()
        })
        .filter(|t| !t.trim().is_empty());
    let steps = vec![JobStep {
        name: get(&s, 0, JOB).unwrap_or(&job.name).to_string(),
        kind: Some("SQL".into()),
        target: owned(get(&s, 0, SERVER)),
        command: cmd,
    }];
    let mut properties = props(vec![
        prop("", "Job", get(&s, 0, JOB)),
        prop("", "Calendario", get(&s, 0, SCHED)),
        prop("", "Servidor", get(&s, 0, SERVER)),
    ]);
    // The rest of what sp_sjobhelp says, as it says it.
    let shown = [ID, NAME, OWNER, STATE, JOB, SCHED, SERVER, DESC].concat();
    for (i, c) in s.cols.iter().enumerate() {
        let c = c.trim();
        if shown.iter().any(|k| k.eq_ignore_ascii_case(c)) {
            continue;
        }
        if let Some(v) = s.at(0, i).map(str::trim).filter(|v| !v.is_empty()) {
            properties.push(PropertyInfo {
                group: "Job Scheduler".into(),
                label: c.to_string(),
                value: v.to_string(),
            });
        }
    }
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

/// Job state codes (Job Scheduler User's Guide, table 4-1).
fn state(code: Option<&str>, exit: Option<i64>, atat: Option<i64>) -> JobOutcome {
    match code.map(|c| c.trim().to_ascii_uppercase()).as_deref() {
        Some("C2") => {
            if exit.is_some_and(|c| c != 0) || atat.is_some_and(|c| c != 0) {
                JobOutcome::Failed
            } else {
                JobOutcome::Succeeded
            }
        }
        Some("T2" | "X2") => JobOutcome::Canceled,
        Some("B" | "M") => JobOutcome::Skipped,
        Some("Q" | "R1" | "R2" | "C1" | "T1" | "X1") => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

/// The history of scheduled job `n`, newest first.
fn history(src: &mut dyn JobSource, n: u64, admin: bool, tz: &Tz) -> Result<Vec<JobRun>> {
    let s = src.query(
        &format!(
            "exec sybmgmtdb..sp_sjobhistory @name = '{n}', @option = {}",
            lit(&options("list", admin))
        ),
        &[],
    )?;
    Ok(runs_of(&s, Some(n as i64), tz)
        .into_iter()
        .map(|(_, r)| r)
        .collect())
}

/// sp_sjobhistory's rows (of scheduled job `only`, when given) with the
/// scheduled job each one belongs to, newest first.
fn runs_of(s: &Set, only: Option<i64>, tz: &Tz) -> Vec<(Option<i64>, JobRun)> {
    let mut out: Vec<(Option<i64>, JobRun)> = (0..s.rows.len())
        .filter(|r| match (only, int(s.get(*r, "sjob_id"))) {
            (Some(n), Some(v)) => v == n,
            _ => true,
        })
        .map(|r| {
            let exit = int(s.get(r, "job_exit_code"));
            let atat = int(s.get(r, "job_atat_error"));
            let mut msg: Vec<String> = Vec::new();
            for c in ["job_long_message", "job_short_message"] {
                if let Some(m) = s.get(r, c) {
                    if !msg.iter().any(|x| x == m) {
                        msg.push(m.to_string());
                    }
                }
            }
            if let Some(e) = atat.filter(|e| *e != 0) {
                msg.push(format!("@@error {e}"));
            }
            if let Some(e) = exit.filter(|e| *e != 0) {
                msg.push(format!("Código de salida {e}"));
            }
            let requested = s.get(r, "job_user_req");
            let ran_as = s.get(r, "job_user_run");
            let run = JobRun {
                id: owned(s.get(r, "job_runid")),
                started: tz.utc(s.get(r, "job_start")),
                duration_ms: duration_ms(s.get(r, "job_start"), s.get(r, "job_end")),
                outcome: state(s.get(r, "job_state"), exit, atat),
                trigger: requested
                    .filter(|u| Some(*u) != ran_as)
                    .map(|u| format!("pedida por {u}")),
                message: (!msg.is_empty()).then(|| msg.join("\n")),
                steps: Vec::new(),
            };
            (int(s.get(r, "sjob_id")), run)
        })
        .collect();
    // job_runid grows with each run, also for the queued ones that have no
    // job_start yet; rows without one go first, then those not started.
    out.sort_by(|a, b| {
        let (ra, rb) = (int(a.1.id.as_deref()), int(b.1.id.as_deref()));
        ra.is_some()
            .cmp(&rb.is_some())
            .then_with(|| rb.cmp(&ra))
            .then_with(|| a.1.started.is_some().cmp(&b.1.started.is_some()))
            .then_with(|| b.1.started.cmp(&a.1.started))
    });
    out
}

pub fn runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let n = int_id(id)?;
    let a = access(src)?.map_err(|note| Error::Query(note.into()))?;
    let tz = Tz::query(src, NOW);
    let mut out = history(src, n, a.admin, &tz)?;
    out.truncate(limit as usize);
    Ok(out)
}

/// `sp_sjobcontrol` with `run_now`, `terminate`, `enable` or `disable` on
/// scheduled job `id`.
pub fn control(src: &mut dyn JobSource, id: &str, action: &str) -> Result<()> {
    let n = int_id(id)?;
    let a = access(src)?.map_err(|note| Error::Query(note.into()))?;
    src.exec(
        &format!(
            "exec sybmgmtdb..sp_sjobcontrol @name = '{n}', @option = {}",
            lit(&options(action, a.admin))
        ),
        &[],
    )
    .map(|_| ())
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{ENABLED, INSTALLED, NOW, ROLES};
}
