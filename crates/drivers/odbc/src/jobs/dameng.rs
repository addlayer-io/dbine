//! Dameng (DM): the job system in the `SYSJOB` schema, created with
//! `SP_INIT_JOB_SYS(1)`. A job (`SYSJOBS`) has steps (`SYSJOBSTEPS`) and
//! schedules (`SYSJOBSCHEDULES`) in SQL Server Agent's style; each finished
//! run leaves a row in `SYSJOBHISTORIES2`, and each step that reports its
//! result one in `SYSSTEPHISTORIES2`. The jobs `DBMS_JOB` submits live
//! there too (`SYS_FLAG` 2, a schedule of type 9), with their interval and
//! next date in `DBA_JOBS`.
//!
//! Reading the tables takes the SOI role; without it the `USER_*_VIEW`
//! views show the user's own jobs. Run now is `SP_DBMS_JOB_RUN_ASYNC`
//! (the trigger fires in the background; `SP_DBMS_JOB_RUN` would run the
//! steps in this session), stop is `SP_STOP_RUNNING_JOB` on a run
//! `V$JOBS_RUNNING` lists, and enable is `SP_ENABLE_JOB`: all of them for
//! DBA or the job's owner, and run now and stop only where the server has
//! those procedures. A DBMS_JOB job is also off while it's broken, so
//! enabling one takes it out of BROKEN too. The script is the
//! `SP_CREATE_JOB`, `SP_ADD_JOB_STEP_EX` and `SP_ADD_JOB_SCHEDULE` calls
//! (or `DBMS_JOB.ISUBMIT`) that make the job as it is.
//!
//! DBMS_SCHEDULER's jobs, a scheduler of their own, are listed next to
//! them ([`scheduler`]).
//!
//! Ids are job `ID`s (DBMS_SCHEDULER's, `"OWNER"."JOB"`). Times are the
//! server's local time.

use super::*;
use dbine_driver::{JobStep, JobStepRun, ServerJob};

mod scheduler;

const NOT_INIT: &str =
    "El sistema de jobs no está creado en este servidor (se crea con SP_INIT_JOB_SYS(1)).";
const NO_ACCESS: &str = "Este usuario no tiene permisos para ver los jobs del esquema SYSJOB.";
const OWN_ONLY: &str =
    "Se ven solo los jobs de este usuario: los de todos los ve quien tiene el rol SOI.";
const NOT_MINE: &str = "solo un DBA o el dueño del job lo ejecuta, detiene o habilita";
const NOT_RUNNING: &str = "el job no se está ejecutando";
const INCOMPLETE: &str = "el job no está completo (VALID = N): hay que terminar de configurarlo";
const NO_ASYNC: &str =
    "esta versión de DM no ejecuta un job en segundo plano (no tiene SP_DBMS_JOB_RUN_ASYNC)";
const NO_STOP: &str =
    "esta versión de DM no detiene un job en ejecución (no tiene SP_STOP_RUNNING_JOB)";

const KIND: &str = "Job";
const KIND_DBMS_JOB: &str = "DBMS_JOB";

pub(super) const NOW: &str =
    "SELECT SYSDATE AS LOCAL_NOW, SYS_EXTRACT_UTC(SYSTIMESTAMP) AS UTC_NOW FROM DUAL";
pub(super) const SCHEMA: &str =
    "SELECT COUNT(*) AS N FROM SYSOBJECTS WHERE TYPE$ = 'SCH' AND NAME = 'SYSJOB'";
pub(super) const RUNNING: &str = "SELECT JOBID, THIS_DATE FROM V$JOBS_RUNNING";
pub(super) const USER: &str = "SELECT USER AS U FROM DUAL";
pub(super) const ROLES: &str = "SELECT GRANTED_ROLE AS R FROM USER_ROLE_PRIVS";
pub(super) const OPERATORS: &str = "SELECT ID, NAME FROM SYSJOB.SYSOPERATORS";
/// The procedures run now and stop call, next to `SP_DBMS_JOB_RUN`, which
/// every DM8 with the job system has: when the catalog doesn't list that
/// one either, it doesn't say which the server has.
pub(super) const PROCS: &str = "SELECT NAME FROM SYSOBJECTS WHERE TYPE$ = 'SCHOBJ' AND SUBTYPE$ = 'PROC' AND NAME IN ('SP_DBMS_JOB_RUN', 'SP_DBMS_JOB_RUN_ASYNC', 'SP_STOP_RUNNING_JOB')";

/// Where the jobs are read from: the tables (SOI role) or the views of
/// the user's own jobs.
#[derive(Clone, Copy)]
pub(super) struct Tables {
    pub jobs: &'static str,
    pub steps: &'static str,
    pub schedules: &'static str,
    pub histories: &'static str,
    pub step_histories: &'static str,
    pub dbms_jobs: &'static str,
    pub own: bool,
}

pub(super) const ALL: Tables = Tables {
    jobs: "SYSJOB.SYSJOBS",
    steps: "SYSJOB.SYSJOBSTEPS",
    schedules: "SYSJOB.SYSJOBSCHEDULES",
    histories: "SYSJOB.SYSJOBHISTORIES2",
    step_histories: "SYSJOB.SYSSTEPHISTORIES2",
    dbms_jobs: "DBA_JOBS",
    own: false,
};

pub(super) const OWN: Tables = Tables {
    jobs: "SYSJOB.USER_JOBS_VIEW",
    steps: "SYSJOB.USER_JOBSTEPS_VIEW",
    schedules: "SYSJOB.USER_JOBSCHEDULES_VIEW",
    histories: "SYSJOB.USER_JOBHISTORIES_VIEW",
    step_histories: "SYSJOB.USER_STEPHISTORIES_VIEW",
    dbms_jobs: "USER_JOBS",
    own: true,
};

fn best_effort(src: &mut dyn JobSource, sql: &str, params: &[&str]) -> Set {
    src.query(sql, params).unwrap_or_else(|e| {
        tracing::debug!("odbc jobs: {sql} failed: {e}");
        Set::default()
    })
}

/// The jobs (all of them, or the one whose `ID` is the parameter) from
/// the first source this user can read.
fn read(src: &mut dyn JobSource, id: Option<&str>) -> Result<(Set, Tables)> {
    let params: Vec<&str> = id.into_iter().collect();
    let sql = |t: &Tables| match id {
        Some(_) => format!("SELECT * FROM {} WHERE ID = ?", t.jobs),
        None => format!("SELECT * FROM {} ORDER BY NAME", t.jobs),
    };
    match src.query(&sql(&ALL), &params) {
        Ok(s) => Ok((s, ALL)),
        Err(e) => {
            tracing::debug!("odbc jobs: SYSJOBS failed, trying the user's views: {e}");
            src.query(&sql(&OWN), &params)
                .map(|s| (s, OWN))
                .map_err(|_| e)
        }
    }
}

/// Who's asking: the user and whether they're a DBA (best effort).
struct Me {
    user: Option<String>,
    dba: bool,
}

fn me(src: &mut dyn JobSource) -> Me {
    let user = best_effort(src, USER, &[]).text("U");
    let roles = best_effort(src, ROLES, &[]);
    // SYSDBA is DBA even where USER_ROLE_PRIVS can't say it.
    let dba = user
        .as_deref()
        .is_some_and(|u| u.eq_ignore_ascii_case("SYSDBA"))
        || (0..roles.rows.len()).any(|r| {
            roles
                .get(r, "R")
                .is_some_and(|v| v.eq_ignore_ascii_case("DBA"))
        });
    Me { user, dba }
}

impl Me {
    fn manages(&self, owner: Option<&str>) -> bool {
        self.dba || matches!((&self.user, owner), (Some(u), Some(o)) if u.eq_ignore_ascii_case(o))
    }
}

/// Whether the server has the procedures run now and stop call.
#[derive(Clone, Copy)]
struct Procs {
    run: bool,
    stop: bool,
}

/// [`Procs`] from the catalog; both when it can't tell (it fails, or it
/// doesn't list `SP_DBMS_JOB_RUN` either).
fn procs(src: &mut dyn JobSource) -> Procs {
    let s = match src.query(PROCS, &[]) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("odbc jobs: DM's procedures unknown: {e}");
            return Procs {
                run: true,
                stop: true,
            };
        }
    };
    let has = |n: &str| {
        (0..s.rows.len()).any(|r| s.get(r, "NAME").is_some_and(|v| v.eq_ignore_ascii_case(n)))
    };
    if !has("SP_DBMS_JOB_RUN") {
        return Procs {
            run: true,
            stop: true,
        };
    }
    Procs {
        run: has("SP_DBMS_JOB_RUN_ASYNC"),
        stop: has("SP_STOP_RUNNING_JOB"),
    }
}

/// What the list needs besides the jobs.
struct Around {
    tz: Tz,
    schedules: Set,
    last: Set,
    running: Set,
    dbms_jobs: Set,
    me: Me,
    procs: Procs,
}

fn around(src: &mut dyn JobSource, t: &Tables, id: Option<&str>) -> Around {
    let tz = Tz::query(src, NOW);
    let params: Vec<&str> = id.into_iter().collect();
    let filter = if id.is_some() { " WHERE JOBID = ?" } else { "" };
    let schedules = best_effort(
        src,
        &format!("SELECT * FROM {}{filter} ORDER BY JOBID, NAME", t.schedules),
        &params,
    );
    let last = best_effort(
        src,
        &format!(
            "SELECT * FROM {h} WHERE EXEC_ID IN (SELECT MAX(EXEC_ID) FROM {h} GROUP BY NAME)",
            h = t.histories
        ),
        &[],
    );
    let running = best_effort(src, RUNNING, &[]);
    let dbms_jobs = best_effort(src, &format!("SELECT * FROM {}", t.dbms_jobs), &[]);
    Around {
        tz,
        schedules,
        last,
        running,
        dbms_jobs,
        me: me(src),
        procs: procs(src),
    }
}

const WEEKDAYS: [&str; 7] = [
    "domingo",
    "lunes",
    "martes",
    "miércoles",
    "jueves",
    "viernes",
    "sábado",
];

fn enumerate(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} y {last}", rest.join(", ")),
    }
}

/// The date part of a `DURING_*_DATE`.
fn date_part(v: &str) -> &str {
    v.get(..10).filter(|d| parse_ts(d).is_some()).unwrap_or(v)
}

/// A `SYSJOBSCHEDULES` row as the manager's dialog states it, in Spanish,
/// with the server's local times. `None` for DBMS_JOB's (type 9).
pub(super) fn describe(s: &Set, r: usize) -> Option<String> {
    let t = int(s.get(r, "TYPE"))?;
    let n = int(s.get(r, "FREQ_INTERVAL")).unwrap_or(1);
    let sub = int(s.get(r, "FREQ_SUB_INTERVAL")).unwrap_or(0);
    let every = |one: &str, many: &str| {
        if n <= 1 {
            one.to_string()
        } else {
            format!("Cada {n} {many}")
        }
    };
    let mut d = match t {
        0 => format!(
            "Una vez, el {}",
            s.get(r, "DURING_START_DATE").unwrap_or("?")
        ),
        1 => every("Todos los días", "días"),
        2 => {
            let days: Vec<&str> = (0..7)
                .filter(|i| sub & (1 << i) != 0)
                .map(|i| WEEKDAYS[i])
                .collect();
            let base = every("Todas las semanas", "semanas");
            if days.is_empty() {
                base
            } else {
                format!("{base}: {}", enumerate(&days))
            }
        }
        3 => {
            if n <= 1 {
                format!("El día {sub} de cada mes")
            } else {
                format!("El día {sub}, cada {n} meses")
            }
        }
        4..=8 => {
            let which = ["primer", "segundo", "tercer", "cuarto", "último"][(t - 4) as usize];
            // As SQL Server Agent: 1-7 Sunday to Saturday, 8 a day, 9 a
            // weekday, 10 a weekend day.
            let what = match sub {
                1..=7 => Some(WEEKDAYS[(sub - 1) as usize]),
                8 => Some("día"),
                9 => Some("día hábil"),
                10 => Some("día de fin de semana"),
                _ => None,
            };
            match what {
                Some(what) if n <= 1 => format!("El {which} {what} de cada mes"),
                Some(what) => format!("El {which} {what}, cada {n} meses"),
                None => format!("Tipo {t}"),
            }
        }
        9 => return None,
        other => format!("Tipo {other}"),
    };
    if (1..=8).contains(&t) {
        let start = s.get(r, "STARTTIME").unwrap_or("00:00:00");
        let minutes = int(s.get(r, "FREQ_MINUTE_INTERVAL")).unwrap_or(0);
        if minutes > 0 {
            let each = if minutes == 1 {
                "cada minuto".to_string()
            } else {
                format!("cada {minutes} minutos")
            };
            match s.get(r, "ENDTIME") {
                Some(end) => d.push_str(&format!(", {each} entre las {start} y las {end}")),
                None => d.push_str(&format!(", {each} desde las {start}")),
            }
        } else {
            d.push_str(&format!(", a las {start}"));
        }
        if let Some(end) = s.get(r, "DURING_END_DATE") {
            d.push_str(&format!(", hasta el {}", date_part(end)));
        }
    }
    if int(s.get(r, "ENABLE")) == Some(0) {
        d.push_str(" (deshabilitada)");
    }
    Some(d)
}

/// The rows of `set` whose `col` is `id`.
fn rows_of<'a>(set: &'a Set, col: &'a str, id: &'a str) -> impl Iterator<Item = usize> + 'a {
    let n = int(Some(id));
    (0..set.rows.len()).filter(move |r| {
        set.get(*r, col)
            .is_some_and(|v| v == id || (n.is_some() && int(Some(v)) == n))
    })
}

/// DBA_JOBS's interval: seconds (`86400`) as words, an expression
/// (`SYSDATE + 1/24`) as it is.
fn interval_text(v: &str) -> String {
    let Ok(secs) = v.trim().parse::<i64>() else {
        return v.to_string();
    };
    let every = |n: i64, one: &str, many: &str| {
        if n == 1 {
            one.to_string()
        } else {
            format!("Cada {n} {many}")
        }
    };
    match secs {
        s if s > 0 && s % 86_400 == 0 => every(s / 86_400, "Todos los días", "días"),
        s if s > 0 && s % 3_600 == 0 => every(s / 3_600, "Cada hora", "horas"),
        s if s > 0 && s % 60 == 0 => every(s / 60, "Cada minuto", "minutos"),
        s => every(s, "Cada segundo", "segundos"),
    }
}

/// A history row's outcome: error code 0 is success.
fn outcome(code: Option<i64>) -> JobOutcome {
    match code {
        Some(0) | None => JobOutcome::Succeeded,
        Some(_) => JobOutcome::Failed,
    }
}

/// The job of `SYSJOBS` row `r` may be DBMS_JOB's: `SYS_FLAG` 2, or no
/// such column (older servers), where only `DBA_JOBS` tells.
fn maybe_dbms_job(s: &Set, r: usize) -> bool {
    int(s.get(r, "SYS_FLAG")).is_none_or(|f| f == 2)
}

fn job(s: &Set, r: usize, a: &Around) -> Option<ServerJob> {
    let id = int(s.get(r, "ID"))?.to_string();
    let name = s.get(r, "NAME").unwrap_or_default().to_string();
    let owner = s.get(r, "USERNAME");
    let dj = rows_of(&a.dbms_jobs, "JOB", &id)
        .next()
        .filter(|_| maybe_dbms_job(s, r));
    let dbms_job = int(s.get(r, "SYS_FLAG")) == Some(2) || dj.is_some();
    let broken = dj.is_some_and(|i| yes(a.dbms_jobs.get(i, "BROKEN")));
    let enabled = int(s.get(r, "ENABLE")) == Some(1) && !broken;
    let schedule = if dbms_job {
        dj.and_then(|i| a.dbms_jobs.get(i, "INTERVAL"))
            .map(interval_text)
    } else {
        let all: Vec<(bool, String)> = rows_of(&a.schedules, "JOBID", &id)
            .filter_map(|i| {
                Some((
                    int(a.schedules.get(i, "ENABLE")) != Some(0),
                    describe(&a.schedules, i)?,
                ))
            })
            .collect();
        let on: Vec<&str> = all
            .iter()
            .filter(|(e, _)| *e)
            .map(|(_, d)| d.as_str())
            .collect();
        let shown: Vec<&str> = if on.is_empty() {
            all.iter().map(|(_, d)| d.as_str()).collect()
        } else {
            on
        };
        (!shown.is_empty()).then(|| shown.join("; "))
    };
    let run = rows_of(&a.running, "JOBID", &id).next();
    let running = run.is_some();
    // The last finished run (the history gets a run when it ends).
    let fin = (0..a.last.rows.len()).find(|i| {
        a.last
            .get(*i, "NAME")
            .is_some_and(|n| n.eq_ignore_ascii_case(&name))
    });
    let started = fin
        .and_then(|i| a.tz.utc(a.last.get(i, "START_TIME")))
        .or_else(|| run.and_then(|i| a.tz.utc(a.running.get(i, "THIS_DATE"))));
    let manages = a.me.manages(owner);
    let valid = s
        .get(r, "VALID")
        .is_none_or(|v| !v.eq_ignore_ascii_case("N"));
    Some(ServerJob {
        id,
        name,
        kind: if dbms_job { KIND_DBMS_JOB } else { KIND }.into(),
        group: None,
        description: owned(s.get(r, "DESCRIBE")),
        owner: owned(owner),
        enabled,
        schedule,
        running,
        last_outcome: last(fin.map(|i| outcome(int(a.last.get(i, "ERRCODE")))), running),
        last_started: started,
        last_duration_ms: fin
            .and_then(|i| duration_ms(a.last.get(i, "START_TIME"), a.last.get(i, "END_TIME"))),
        next_run: dj.and_then(|i| {
            let next = a.dbms_jobs.get(i, "NEXT_DATE");
            (enabled && !a.tz.past(next))
                .then(|| a.tz.utc(next))
                .flatten()
        }),
        can_run: manages && valid && a.procs.run,
        can_stop: manages && running && a.procs.stop,
        can_enable: manages,
        has_history: true,
        has_script: true,
        system: false,
    })
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let mut out = JobList::default();
    match read(src, None) {
        Ok((s, t)) => {
            let a = around(src, &t, None);
            out.jobs = (0..s.rows.len()).filter_map(|r| job(&s, r, &a)).collect();
            if t.own {
                out.notes.push(OWN_ONLY.into());
            }
        }
        Err(e) => {
            let schema = src.query(SCHEMA, &[]).ok().and_then(|s| int(s.get(0, "N")));
            let note = match schema {
                Some(0) => NOT_INIT,
                Some(_) => NO_ACCESS,
                None => return Err(e),
            };
            out.notes.push(note.into());
        }
    }
    let (more, note) = scheduler::list(src);
    out.jobs.extend(more);
    out.notes.extend(note.map(String::from));
    Ok(out)
}

fn one(src: &mut dyn JobSource, id: &str) -> Result<(Set, Tables, String)> {
    let n = int_id(id)?.to_string();
    let (s, t) = read(src, Some(&n))?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok((s, t, n))
}

fn steps_sql(t: &Tables) -> String {
    format!(
        "SELECT * FROM {} WHERE JOBID = ? ORDER BY SEQNO, ID",
        t.steps
    )
}

/// What a step of `TYPE` runs, in Spanish.
fn step_kind(t: Option<i64>) -> Option<&'static str> {
    Some(match t? {
        0 => "SQL",
        1 | 5 => "Copia de seguridad (V1.0)",
        2 => "Reorganizar la base",
        3 => "Actualizar estadísticas",
        4 => "Migración DTS",
        6 => "Copia de seguridad (V2.0)",
        _ => return None,
    })
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    if scheduler::is_mine(id) {
        return scheduler::detail(src, id);
    }
    let (s, t, n) = one(src, id)?;
    let a = around(src, &t, Some(&n));
    let job = job(&s, 0, &a).ok_or_else(|| not_found(id))?;
    let st = src.query(&steps_sql(&t), &[&n])?;
    let dj = rows_of(&a.dbms_jobs, "JOB", &n)
        .next()
        .filter(|_| job.kind == KIND_DBMS_JOB);
    let dbms = |c: &str| dj.and_then(|i| a.dbms_jobs.get(i, c));
    let mut steps: Vec<JobStep> = rows_of(&st, "JOBID", &n)
        .map(|r| JobStep {
            name: st.get(r, "NAME").unwrap_or_default().to_string(),
            kind: step_kind(int(st.get(r, "TYPE"))).map(str::to_string),
            target: owned(st.get(r, "DBNAME")),
            command: owned(st.get(r, "COMMAND")),
        })
        .collect();
    if steps.is_empty() {
        // A DBMS_JOB job: what it runs is its WHAT.
        steps.extend(dbms("WHAT").map(|w| JobStep {
            name: job.name.clone(),
            kind: Some("PL/SQL".into()),
            target: None,
            command: Some(w.to_string()),
        }));
    }
    let schedules: Vec<String> = rows_of(&a.schedules, "JOBID", &n)
        .filter_map(|r| {
            let name = a.schedules.get(r, "NAME")?;
            Some(format!("{name}: {}", describe(&a.schedules, r)?))
        })
        .collect();
    let properties = props(vec![
        prop("", "Creado", s.get(0, "CREATETIME")),
        prop("", "Modificado", s.get(0, "MODIFYTIME")),
        prop("", "Completo", s.get(0, "VALID")),
        prop("", "Nodo de ejecución", s.get(0, "EP_SEQNO")),
        prop("", "Grupo RAFT", s.get(0, "RAFT_NAME")),
        prop("DBMS_JOB", "Qué ejecuta", dbms("WHAT")),
        prop("DBMS_JOB", "Intervalo", dbms("INTERVAL")),
        prop("DBMS_JOB", "Próxima ejecución", dbms("NEXT_DATE")),
        prop("DBMS_JOB", "Última ejecución correcta", dbms("LAST_DATE")),
        prop("DBMS_JOB", "Interrumpido (BROKEN)", dbms("BROKEN")),
        prop("DBMS_JOB", "Fallas seguidas", dbms("FAILURES")),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: if schedules.len() > 1 {
            schedules
        } else {
            Vec::new()
        },
        properties,
        notes: Vec::new(),
    })
}

pub(super) fn runs_sql(t: &Tables, limit: u32) -> String {
    format!(
        "SELECT * FROM {} WHERE NAME = ? ORDER BY EXEC_ID DESC LIMIT {limit}",
        t.histories
    )
}

pub(super) fn step_runs_sql(t: &Tables) -> String {
    format!(
        "SELECT * FROM {} WHERE NAME = ? AND EXEC_ID >= ? ORDER BY EXEC_ID, START_TIME",
        t.step_histories
    )
}

fn message(code: Option<i64>, info: Option<&str>) -> Option<String> {
    match (code.filter(|c| *c != 0), info) {
        (_, Some(i)) => Some(i.to_string()),
        (Some(c), None) => Some(format!("Código de error {c}")),
        (None, None) => None,
    }
}

pub fn runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    if scheduler::is_mine(id) {
        return scheduler::runs(src, id, limit);
    }
    let (s, t, n) = one(src, id)?;
    let name = s.get(0, "NAME").ok_or_else(|| not_found(id))?.to_string();
    let tz = Tz::query(src, NOW);
    let h = src.query(&runs_sql(&t, limit), &[&name])?;
    let first = (0..h.rows.len())
        .filter_map(|r| int(h.get(r, "EXEC_ID")))
        .min();
    let sh = match first {
        Some(f) => best_effort(src, &step_runs_sql(&t), &[&name, &f.to_string()]),
        None => Set::default(),
    };
    let mut out: Vec<JobRun> = (0..h.rows.len())
        .map(|r| {
            let exec = h.get(r, "EXEC_ID");
            let code = int(h.get(r, "ERRCODE"));
            let steps = (0..sh.rows.len())
                .filter(|i| exec.is_some() && sh.get(*i, "EXEC_ID") == exec)
                .map(|i| {
                    let c = int(sh.get(i, "ERRCODE"));
                    let mut msg = message(c, sh.get(i, "ERRINFO"));
                    if let Some(k) = int(sh.get(i, "RETRY_ATTEMPTS")).filter(|k| *k > 0) {
                        let retry = format!("Reintento {k}");
                        msg = Some(match msg {
                            Some(m) => format!("{retry}: {m}"),
                            None => retry,
                        });
                    }
                    JobStepRun {
                        name: sh.get(i, "STEPNAME").unwrap_or_default().to_string(),
                        outcome: outcome(c),
                        started: tz.utc(sh.get(i, "START_TIME")),
                        duration_ms: duration_ms(sh.get(i, "START_TIME"), sh.get(i, "END_TIME")),
                        message: msg,
                    }
                })
                .collect();
            JobRun {
                id: owned(exec),
                started: tz.utc(h.get(r, "START_TIME")),
                duration_ms: duration_ms(h.get(r, "START_TIME"), h.get(r, "END_TIME")),
                outcome: outcome(code),
                trigger: None,
                message: message(code, h.get(r, "ERRINFO")),
                steps,
            }
        })
        .collect();
    // The run going on now, which the history gets when it ends.
    let running = best_effort(src, RUNNING, &[]);
    if let Some(i) = rows_of(&running, "JOBID", &n).next() {
        out.push(JobRun {
            id: None,
            started: tz.utc(running.get(i, "THIS_DATE")),
            duration_ms: None,
            outcome: JobOutcome::Running,
            trigger: None,
            message: None,
            steps: Vec::new(),
        });
    }
    newest_first(&mut out);
    Ok(out)
}

/// The job, for a DBA or its owner.
fn managed(src: &mut dyn JobSource, id: &str) -> Result<(Set, Tables, String)> {
    let (s, t, n) = one(src, id)?;
    if !me(src).manages(s.get(0, "USERNAME")) {
        return Err(Error::Unsupported(NOT_MINE.into()));
    }
    Ok((s, t, n))
}

pub fn run(src: &mut dyn JobSource, id: &str) -> Result<()> {
    if scheduler::is_mine(id) {
        return scheduler::run(src, id);
    }
    let (s, _, n) = managed(src, id)?;
    if s.get(0, "VALID")
        .is_some_and(|v| v.eq_ignore_ascii_case("N"))
    {
        return Err(Error::Query(INCOMPLETE.into()));
    }
    if !procs(src).run {
        return Err(Error::Unsupported(NO_ASYNC.into()));
    }
    src.exec(&format!("CALL SP_DBMS_JOB_RUN_ASYNC({n})"), &[])
        .map(|_| ())
}

pub fn stop(src: &mut dyn JobSource, id: &str) -> Result<()> {
    if scheduler::is_mine(id) {
        return scheduler::stop(src, id);
    }
    let (_, _, n) = managed(src, id)?;
    if !procs(src).stop {
        return Err(Error::Unsupported(NO_STOP.into()));
    }
    let running = best_effort(src, RUNNING, &[]);
    if rows_of(&running, "JOBID", &n).next().is_none() {
        return Err(Error::Query(NOT_RUNNING.into()));
    }
    src.exec(&format!("CALL SP_STOP_RUNNING_JOB({n})"), &[])
        .map(|_| ())
}

/// The `DBA_JOBS` (or `USER_JOBS`) row of job `n` when it's DBMS_JOB's.
fn dbms_job_row(src: &mut dyn JobSource, s: &Set, t: &Tables, n: &str) -> Result<Option<Set>> {
    if !maybe_dbms_job(s, 0) {
        return Ok(None);
    }
    let d = src.query(
        &format!("SELECT * FROM {} WHERE JOB = ?", t.dbms_jobs),
        &[n],
    )?;
    Ok((!d.rows.is_empty()).then_some(d))
}

/// When a DBMS_JOB job taken out of BROKEN runs next: its `NEXT_DATE`
/// while that's ahead (and not the far-off date a broken job may get),
/// else one interval from now. `None` (DBMS_JOB.BROKEN's default, now)
/// when the interval isn't seconds: an expression from the catalog never
/// goes into the call.
fn resume_at(d: &Set, tz: &Tz) -> Option<String> {
    let next = d.get(0, "NEXT_DATE");
    let far = parse_ts("4000-01-01");
    if let (Some(at), Some(far)) = (next.and_then(parse_ts), far) {
        if at < far && !tz.past(next) {
            return Some(datetime(next));
        }
    }
    d.get(0, "INTERVAL")
        .and_then(|i| i.trim().parse::<u32>().ok())
        .filter(|secs| *secs > 0)
        .map(|secs| format!("SYSDATE + {secs}/86400"))
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    if scheduler::is_mine(id) {
        return scheduler::set_enabled(src, id, enabled);
    }
    let (s, t, n) = managed(src, id)?;
    let name = s.get(0, "NAME").ok_or_else(|| not_found(id))?.to_string();
    // A DBMS_JOB job is also off while broken (best effort: without its
    // row the job system's flag is all there is).
    let broken = if enabled {
        dbms_job_row(src, &s, &t, &n)
            .unwrap_or_else(|e| {
                tracing::debug!("odbc jobs: DBA_JOBS failed: {e}");
                None
            })
            .filter(|d| yes(d.get(0, "BROKEN")))
    } else {
        None
    };
    src.exec(
        &format!(
            "CALL SP_ENABLE_JOB({}, {})",
            lit(&name),
            if enabled { 1 } else { 0 }
        ),
        &[],
    )?;
    if let Some(d) = broken {
        let tz = Tz::query(src, NOW);
        let call = match resume_at(&d, &tz) {
            Some(at) => format!("CALL DBMS_JOB.BROKEN({n}, FALSE, {at})"),
            None => format!("CALL DBMS_JOB.BROKEN({n}, FALSE)"),
        };
        src.exec(&call, &[])?;
    }
    Ok(())
}

/// A text argument, or NULL.
fn arg(v: Option<&str>) -> String {
    v.map(lit).unwrap_or_else(|| "NULL".into())
}

/// A whole number argument (0 when missing).
fn num(v: Option<&str>) -> String {
    int(v).unwrap_or(0).to_string()
}

/// `CAST('…' AS DATETIME)` rebuilt from the value, or SYSDATE.
fn datetime(v: Option<&str>) -> String {
    v.and_then(parse_ts)
        .map(|ms| format!("CAST('{}' AS DATETIME)", plain(ms)))
        .unwrap_or_else(|| "SYSDATE".into())
}

/// DBMS_JOB.ISUBMIT's interval: seconds back to a date expression.
fn interval_expr(v: &str) -> String {
    match v.trim().parse::<i64>() {
        Ok(secs) => format!("SYSDATE + {secs}/86400"),
        Err(_) => v.to_string(),
    }
}

/// DBMS_JOB.ISUBMIT, and BROKEN when the job is off: broken, or disabled
/// in the job system (`SP_ENABLE_JOB` 0), which the list reads the same
/// way and which, by the job's number, only BROKEN can bring back.
fn dbms_job_script(n: &str, s: &Set, d: &Set) -> String {
    let what = d.get(0, "WHAT").map(lit).unwrap_or_else(|| "''".into());
    let interval = d
        .get(0, "INTERVAL")
        .map(|i| lit(&interval_expr(i)))
        .unwrap_or_else(|| "NULL".into());
    let mut out = format!(
        "CALL DBMS_JOB.ISUBMIT({n}, {what}, {}, {interval});\n",
        datetime(d.get(0, "NEXT_DATE"))
    );
    if yes(d.get(0, "BROKEN")) || int(s.get(0, "ENABLE")) == Some(0) {
        out.push_str(&format!("CALL DBMS_JOB.BROKEN({n}, TRUE);\n"));
    }
    out
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    if scheduler::is_mine(id) {
        return scheduler::script(src, id);
    }
    let (s, t, n) = one(src, id)?;
    let name = s.get(0, "NAME").ok_or_else(|| not_found(id))?.to_string();
    if let Some(d) = dbms_job_row(src, &s, &t, &n)? {
        return Ok(dbms_job_script(&n, &s, &d));
    }
    let job = lit(&name);
    // Operators' names for the notices (best effort: without them the
    // notices are left off).
    let ops = if t.own {
        Set::default()
    } else {
        best_effort(src, OPERATORS, &[])
    };
    let operator = |enabled: &str, id_col: &str| -> (String, String) {
        let name = s
            .get(0, id_col)
            .and_then(|op| rows_of(&ops, "ID", op).next())
            .and_then(|r| ops.get(r, "NAME"));
        match (int(s.get(0, enabled)), name) {
            (Some(1), Some(op)) => ("1".into(), lit(op)),
            _ => ("0".into(), "''".into()),
        }
    };
    let (email, email_op) = operator("ENABLE_EMAIL", "EMAIL_OPERID");
    let (netsend, netsend_op) = operator("ENABLE_NETSEND", "NETSEND_OPERID");
    let mut out = format!(
        "SP_CREATE_JOB({job}, {}, {email}, {email_op}, {}, {netsend}, {netsend_op}, {}, {});\n",
        num(s.get(0, "ENABLE")),
        num(s.get(0, "EMAIL_TYPE")),
        num(s.get(0, "NETSEND_TYPE")),
        s.get(0, "DESCRIBE").map(lit).unwrap_or_else(|| "''".into()),
    );
    out.push_str(&format!("SP_JOB_CONFIG_START({job});\n"));
    let st = src.query(&steps_sql(&t), &[&n])?;
    let st_rows: Vec<usize> = rows_of(&st, "JOBID", &n).collect();
    for &r in &st_rows {
        let step = st.get(r, "NAME").unwrap_or_default();
        out.push_str(&format!(
            "SP_ADD_JOB_STEP_EX({job}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {});\n",
            lit(step),
            num(st.get(r, "TYPE")),
            arg(st.get(r, "COMMAND")),
            num(st.get(r, "SUCC_ACTION")),
            num(st.get(r, "FAIL_ACTION")),
            num(st.get(r, "RETRY_ATTEMPTS")),
            num(st.get(r, "RETRY_INTERVAL")),
            arg(st.get(r, "OUTPUT_FILE_PATH")),
            num(st.get(r, "APPEND_FLAG")),
            st.get(r, "DESCRIBE")
                .map(lit)
                .unwrap_or_else(|| "''".into()),
        ));
    }
    // The jumps, once every step exists.
    for &r in &st_rows {
        let (ok, fail) = (st.get(r, "SUCC_NEXT_STEP"), st.get(r, "FAIL_NEXT_STEP"));
        if ok.is_some() || fail.is_some() {
            out.push_str(&format!(
                "SP_JOB_STEP_SET_NEXT_STEP({job}, {}, {}, {});\n",
                lit(st.get(r, "NAME").unwrap_or_default()),
                arg(ok),
                arg(fail),
            ));
        }
    }
    let sch = src.query(
        &format!(
            "SELECT * FROM {} WHERE JOBID = ? ORDER BY NAME",
            t.schedules
        ),
        &[&n],
    )?;
    let sch_rows: Vec<usize> = rows_of(&sch, "JOBID", &n).collect();
    for r in sch_rows {
        out.push_str(&format!(
            "SP_ADD_JOB_SCHEDULE({job}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {}, {});\n",
            lit(sch.get(r, "NAME").unwrap_or_default()),
            num(sch.get(r, "ENABLE")),
            num(sch.get(r, "TYPE")),
            num(sch.get(r, "FREQ_INTERVAL")),
            num(sch.get(r, "FREQ_SUB_INTERVAL")),
            num(sch.get(r, "FREQ_MINUTE_INTERVAL")),
            arg(sch.get(r, "STARTTIME")),
            arg(sch.get(r, "ENDTIME")),
            arg(sch.get(r, "DURING_START_DATE")),
            arg(sch.get(r, "DURING_END_DATE")),
            sch.get(r, "DESCRIBE")
                .map(lit)
                .unwrap_or_else(|| "''".into()),
        ));
    }
    out.push_str(&format!("SP_JOB_CONFIG_COMMIT({job});\n"));
    Ok(out)
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{
        runs_sql, step_runs_sql, ALL, NOW, OPERATORS, OWN, PROCS, ROLES, RUNNING, SCHEMA, USER,
    };
}
