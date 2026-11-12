//! Dameng's DBMS_SCHEDULER (created with `SP_INIT_DBMS_SCHEDULER_SYS(1)`),
//! a scheduler of its own next to the job system: Oracle's package, with
//! its jobs in `DBA_SCHEDULER_JOBS` (`USER_SCHEDULER_JOBS` without the
//! rights), the finished runs in `DBA_SCHEDULER_JOB_RUN_DETAILS` and the
//! ones going on in `DBA_SCHEDULER_RUNNING_JOBS`. Run now is `RUN_JOB`
//! with `USE_CURRENT_SESSION` false (a task in the background), stop
//! `STOP_JOB`, enable `ENABLE` and `DISABLE`: for DBA or the job's owner.
//!
//! Ids are `"OWNER"."JOB"`, looked up with both names bound; the package
//! gets the names read from the catalog.

use super::*;

pub(in crate::jobs) const KIND: &str = "DBMS_SCHEDULER";
const OWN_ONLY: &str = "De DBMS_SCHEDULER se ven solo los jobs de este usuario.";
const NO_ARGS_SCRIPT: &str = "el job toma argumentos que DBine no lee: el script no se puede armar";

/// Where DBMS_SCHEDULER's jobs are read from.
#[derive(Clone, Copy)]
pub(in crate::jobs) struct Views {
    pub jobs: &'static str,
    pub runs: &'static str,
    pub running: &'static str,
    pub own: bool,
}

pub(in crate::jobs) const DBA: Views = Views {
    jobs: "DBA_SCHEDULER_JOBS",
    runs: "DBA_SCHEDULER_JOB_RUN_DETAILS",
    running: "DBA_SCHEDULER_RUNNING_JOBS",
    own: false,
};

pub(in crate::jobs) const USER: Views = Views {
    jobs: "USER_SCHEDULER_JOBS",
    runs: "USER_SCHEDULER_JOB_RUN_DETAILS",
    running: "USER_SCHEDULER_RUNNING_JOBS",
    own: true,
};

/// The user's own views may not have `OWNER`: they're filtered by name
/// only.
fn filter(v: &Views) -> &'static str {
    if v.own {
        "WHERE JOB_NAME = ?"
    } else {
        "WHERE OWNER = ? AND JOB_NAME = ?"
    }
}

fn params<'a>(v: &Views, owner: &'a str, name: &'a str) -> Vec<&'a str> {
    if v.own {
        vec![name]
    } else {
        vec![owner, name]
    }
}

/// The jobs (all, or the one named) from the first view this user reads.
fn read(src: &mut dyn JobSource, key: Option<(&str, &str)>) -> Result<(Set, Views)> {
    let sql = |v: &Views| match key {
        Some(_) => format!("SELECT * FROM {} {}", v.jobs, filter(v)),
        None => format!("SELECT * FROM {} ORDER BY JOB_NAME", v.jobs),
    };
    let args = |v: &Views| key.map(|(o, n)| params(v, o, n)).unwrap_or_default();
    match src.query(&sql(&DBA), &args(&DBA)) {
        Ok(s) => Ok((s, DBA)),
        Err(e) => {
            tracing::debug!("odbc jobs: DBA_SCHEDULER_JOBS failed, trying USER_: {e}");
            src.query(&sql(&USER), &args(&USER))
                .map(|s| (s, USER))
                .map_err(|_| e)
        }
    }
}

/// The owner of row `r`: its OWNER, or the user for their own views.
fn owner_of<'a>(s: &'a Set, r: usize, me: &'a Me) -> Option<&'a str> {
    s.get(r, "OWNER").or(me.user.as_deref())
}

/// The rows of `set` that are job `owner`.`name`.
fn rows_for<'a>(set: &'a Set, owner: &'a str, name: &'a str) -> impl Iterator<Item = usize> + 'a {
    (0..set.rows.len()).filter(move |r| {
        set.get(*r, "JOB_NAME") == Some(name) && set.get(*r, "OWNER").is_none_or(|o| o == owner)
    })
}

fn outcome(status: Option<&str>) -> JobOutcome {
    match status.map(|s| s.trim().to_ascii_uppercase()).as_deref() {
        Some("SUCCEEDED") => JobOutcome::Succeeded,
        Some("FAILED" | "BROKEN") => JobOutcome::Failed,
        Some("STOPPED") => JobOutcome::Canceled,
        Some("RUNNING") => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

/// ms between two times of a run (both with their offset or both local).
fn span(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    let (a, b) = (parse_ts_utc(start?)?, parse_ts_utc(end?)?);
    u64::try_from(b - a).ok()
}

fn message(s: &Set, r: usize) -> Option<String> {
    let code = int(s.get(r, "ERROR#")).filter(|c| *c != 0);
    match (s.get(r, "ADDITIONAL_INFO"), code) {
        (Some(i), _) => Some(i.to_string()),
        (None, Some(c)) => Some(format!("Código de error {c}")),
        (None, None) => None,
    }
}

struct Seen {
    tz: Tz,
    last: Set,
    running: Set,
    me: Me,
}

fn seen(src: &mut dyn JobSource, v: &Views) -> Seen {
    let group = if v.own { "JOB_NAME" } else { "OWNER, JOB_NAME" };
    Seen {
        tz: Tz::query(src, NOW),
        last: best_effort(
            src,
            &format!(
                "SELECT * FROM {r} WHERE LOG_ID IN (SELECT MAX(LOG_ID) FROM {r} GROUP BY {group})",
                r = v.runs
            ),
            &[],
        ),
        running: best_effort(src, &format!("SELECT * FROM {}", v.running), &[]),
        me: me(src),
    }
}

fn job(s: &Set, r: usize, a: &Seen) -> Option<ServerJob> {
    let owner = owner_of(s, r, &a.me)?;
    let name = s.get(r, "JOB_NAME")?;
    let enabled = yes(s.get(r, "ENABLED"));
    let running = rows_for(&a.running, owner, name).next().is_some()
        || s.get(r, "STATE")
            .is_some_and(|st| st.eq_ignore_ascii_case("RUNNING"));
    let fin = rows_for(&a.last, owner, name).next();
    let schedule = s.get(r, "REPEAT_INTERVAL").map(str::to_string).or_else(|| {
        let n = s.get(r, "SCHEDULE_NAME")?;
        Some(match s.get(r, "SCHEDULE_OWNER") {
            Some(o) => format!("{o}.{n}"),
            None => n.to_string(),
        })
    });
    let manages = a.me.manages(Some(owner));
    Some(ServerJob {
        id: quote_pair(owner, name),
        name: name.to_string(),
        kind: KIND.into(),
        group: Some(owner.to_string()),
        description: owned(s.get(r, "COMMENTS")),
        owner: owned(s.get(r, "JOB_CREATOR").or(Some(owner))),
        enabled,
        schedule,
        running,
        last_outcome: last(fin.map(|i| outcome(a.last.get(i, "STATUS"))), running),
        last_started: fin.and_then(|i| a.tz.utc_any(a.last.get(i, "ACTUAL_START_DATE"))),
        last_duration_ms: fin.and_then(|i| {
            span(
                a.last.get(i, "ACTUAL_START_DATE"),
                a.last.get(i, "LOG_DATE"),
            )
        }),
        next_run: enabled
            .then(|| a.tz.utc_any(s.get(r, "NEXT_RUN_DATE")))
            .flatten(),
        can_run: manages && enabled,
        can_stop: manages && running,
        can_enable: manages,
        has_history: true,
        // The arguments' values aren't read: such a job has no script.
        has_script: int(s.get(r, "NUMBER_OF_ARGUMENTS")).unwrap_or(0) == 0,
    })
}

/// DBMS_SCHEDULER's jobs and a note when they're only the user's own
/// (even none: others' may be there); nothing when it isn't created or
/// can't be read.
pub(super) fn list(src: &mut dyn JobSource) -> (Vec<ServerJob>, Option<&'static str>) {
    let (s, v) = match read(src, None) {
        Ok(x) => x,
        Err(e) => {
            tracing::debug!("odbc jobs: DBMS_SCHEDULER unreadable: {e}");
            return (Vec::new(), None);
        }
    };
    let note = v.own.then_some(OWN_ONLY);
    if s.rows.is_empty() {
        return (Vec::new(), note);
    }
    let a = seen(src, &v);
    let jobs: Vec<ServerJob> = (0..s.rows.len()).filter_map(|r| job(&s, r, &a)).collect();
    (jobs, note)
}

/// This is one of DBMS_SCHEDULER's ids (the job system's are digits).
pub(super) fn is_mine(id: &str) -> bool {
    id.trim_start().starts_with('"')
}

fn one(src: &mut dyn JobSource, id: &str) -> Result<(Set, Views, String, String)> {
    let (owner, name) = parse_pair(id)?;
    let (s, v) = read(src, Some((&owner, &name)))?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok((s, v, owner, name))
}

pub(super) fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let (s, v, _, _) = one(src, id)?;
    let a = seen(src, &v);
    let job = job(&s, 0, &a).ok_or_else(|| not_found(id))?;
    let steps = match (s.get(0, "PROGRAM_NAME"), s.get(0, "JOB_ACTION")) {
        (Some(p), _) => vec![JobStep {
            name: job.name.clone(),
            kind: Some("Programa".into()),
            target: None,
            command: Some(match s.get(0, "PROGRAM_OWNER") {
                Some(o) => format!("{o}.{p}"),
                None => p.to_string(),
            }),
        }],
        (None, Some(action)) => vec![JobStep {
            name: job.name.clone(),
            kind: owned(s.get(0, "JOB_TYPE")),
            target: None,
            command: Some(action.to_string()),
        }],
        (None, None) => Vec::new(),
    };
    let properties = props(vec![
        prop("", "Tipo", s.get(0, "JOB_TYPE")),
        prop("", "Inicio", s.get(0, "START_DATE")),
        prop("", "Fin", s.get(0, "END_DATE")),
        prop("", "Estado", s.get(0, "STATE")),
        prop("", "Borrar al terminar", s.get(0, "AUTO_DROP")),
        prop("", "Duración máxima", s.get(0, "MAX_RUN_DURATION")),
        prop("", "Argumentos", s.get(0, "NUMBER_OF_ARGUMENTS")),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
    })
}

pub(super) fn runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let (_, v, owner, name) = one(src, id)?;
    let tz = Tz::query(src, NOW);
    let h = src.query(
        &format!(
            "SELECT * FROM {} {} ORDER BY LOG_ID DESC LIMIT {limit}",
            v.runs,
            filter(&v)
        ),
        &params(&v, &owner, &name),
    )?;
    let mut out: Vec<JobRun> = (0..h.rows.len())
        .map(|r| JobRun {
            id: owned(h.get(r, "LOG_ID")),
            started: tz.utc_any(h.get(r, "ACTUAL_START_DATE")),
            duration_ms: span(h.get(r, "ACTUAL_START_DATE"), h.get(r, "LOG_DATE")),
            outcome: outcome(h.get(r, "STATUS")),
            trigger: None,
            message: message(&h, r),
            steps: Vec::new(),
        })
        .collect();
    newest_first(&mut out);
    // The run going on (its start isn't reported) goes first.
    let running = best_effort(src, &format!("SELECT * FROM {}", v.running), &[]);
    if rows_for(&running, &owner, &name).next().is_some() {
        out.insert(
            0,
            JobRun {
                outcome: JobOutcome::Running,
                ..Default::default()
            },
        );
    }
    Ok(out)
}

/// The job's name for the package, for a DBA or its owner.
fn managed(src: &mut dyn JobSource, id: &str) -> Result<(String, Set)> {
    let (s, _, owner, name) = one(src, id)?;
    if !me(src).manages(Some(&owner)) {
        return Err(Error::Unsupported(NOT_MINE.into()));
    }
    Ok((lit(&dotted(&owner, &name, true)), s))
}

pub(super) fn run(src: &mut dyn JobSource, id: &str) -> Result<()> {
    let (job, _) = managed(src, id)?;
    src.exec(&format!("CALL DBMS_SCHEDULER.RUN_JOB({job}, FALSE)"), &[])
        .map(|_| ())
}

pub(super) fn stop(src: &mut dyn JobSource, id: &str) -> Result<()> {
    let (job, _) = managed(src, id)?;
    src.exec(&format!("CALL DBMS_SCHEDULER.STOP_JOB({job})"), &[])
        .map(|_| ())
}

pub(super) fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let (job, _) = managed(src, id)?;
    let call = if enabled { "ENABLE" } else { "DISABLE" };
    src.exec(&format!("CALL DBMS_SCHEDULER.{call}({job})"), &[])
        .map(|_| ())
}

/// A TIMESTAMP WITH TIME ZONE literal rebuilt from the value.
fn ts_lit(v: Option<&str>) -> Option<String> {
    let v = v?;
    let local = parse_ts(v)?;
    Some(match ts_offset(v) {
        Some(off) => {
            let m = off.abs() / 60_000;
            let sign = if off < 0 { '-' } else { '+' };
            format!(
                "TIMESTAMP '{} {sign}{:02}:{:02}'",
                plain(local),
                m / 60,
                m % 60
            )
        }
        None => format!("TIMESTAMP '{}'", plain(local)),
    })
}

pub(super) fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let (s, _, owner, name) = one(src, id)?;
    if int(s.get(0, "NUMBER_OF_ARGUMENTS")).unwrap_or(0) != 0 {
        return Err(Error::Unsupported(NO_ARGS_SCRIPT.into()));
    }
    let qualified = |o: Option<&str>, n: &str| lit(&dotted(o.unwrap_or(&owner), n, true));
    let mut args: Vec<(&str, String)> = vec![("JOB_NAME", lit(&dotted(&owner, &name, true)))];
    match s.get(0, "PROGRAM_NAME") {
        Some(p) => args.push(("PROGRAM_NAME", qualified(s.get(0, "PROGRAM_OWNER"), p))),
        None => {
            args.push((
                "JOB_TYPE",
                lit(s.get(0, "JOB_TYPE").unwrap_or("PLSQL_BLOCK")),
            ));
            args.push((
                "JOB_ACTION",
                lit(s.get(0, "JOB_ACTION").unwrap_or_default()),
            ));
        }
    }
    match s.get(0, "SCHEDULE_NAME") {
        Some(n) => args.push(("SCHEDULE_NAME", qualified(s.get(0, "SCHEDULE_OWNER"), n))),
        None => {
            if let Some(t) = ts_lit(s.get(0, "START_DATE")) {
                args.push(("START_DATE", t));
            }
            if let Some(i) = s.get(0, "REPEAT_INTERVAL") {
                args.push(("REPEAT_INTERVAL", lit(i)));
            }
            if let Some(t) = ts_lit(s.get(0, "END_DATE")) {
                args.push(("END_DATE", t));
            }
        }
    }
    let flag = |c: &str| if yes(s.get(0, c)) { "TRUE" } else { "FALSE" };
    args.push(("ENABLED", flag("ENABLED").into()));
    args.push(("AUTO_DROP", flag("AUTO_DROP").into()));
    if let Some(c) = s.get(0, "COMMENTS") {
        args.push(("COMMENTS", lit(c)));
    }
    let body: Vec<String> = args
        .iter()
        .map(|(k, v)| format!("    {k} => {v}"))
        .collect();
    Ok(format!(
        "BEGIN\n  DBMS_SCHEDULER.CREATE_JOB(\n{}\n  );\nEND;\n/\n",
        body.join(",\n")
    ))
}
