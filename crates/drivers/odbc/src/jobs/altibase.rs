//! Altibase: the jobs `CREATE JOB` makes (6.3.1 on), rows of
//! `SYSTEM_.SYS_JOBS_`: the procedure call it runs, its start, end and
//! interval, whether it's running now, when it last ran, how many times it
//! has and the error code of its last run. `ALTER JOB … SET ENABLE |
//! DISABLE` (6.5.1 on, the `IS_ENABLE` column) turns one on and off; only
//! SYS creates and alters jobs. The job scheduler runs them while
//! `JOB_SCHEDULER_ENABLE` is 1 and `JOB_THREAD_COUNT` isn't 0.
//!
//! Altibase has no call that runs a job now (running its procedure by
//! hand isn't a run of the job: the job's columns don't change), keeps no
//! history beyond the last run, and nothing stops a run. Ids are
//! `JOB_ID`s. Times are the server's local time.

use super::*;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{JobStep, ServerJob};

pub const NO_RUN_NOW: &str = "Altibase no tiene una forma de ejecutar un job fuera de su calendario: ejecutar su procedimiento a mano no cuenta como una ejecución del job";
pub const NO_HISTORY: &str =
    "Altibase no guarda el historial de sus jobs: solo la última ejecución y su código de error";
const ONLY_SYS: &str = "en Altibase solo el usuario SYS modifica los jobs";
const NO_ENABLE: &str =
    "esta versión de Altibase no habilita ni deshabilita jobs (llega con la 6.5.1)";

const NOT_HERE: &str = "Este servidor no tiene jobs (CREATE JOB llega con Altibase 6.3.1).";
const OFF: &str = "El planificador de jobs está apagado (JOB_SCHEDULER_ENABLE o JOB_THREAD_COUNT en 0): los jobs no se ejecutan.";

const KIND: &str = "Job";

pub(super) const NOW: &str = "SELECT TO_CHAR(SYSDATE, 'YYYY-MM-DD HH24:MI:SS') AS LOCAL_NOW, TO_CHAR(UNIX_DATE, 'YYYY-MM-DD HH24:MI:SS') AS UTC_NOW FROM DUAL";
pub(super) const USER: &str = "SELECT USER_NAME() AS U FROM DUAL";
pub(super) const PROPS: &str = "SELECT NAME, VALUE1 FROM V$PROPERTY WHERE NAME IN ('JOB_SCHEDULER_ENABLE', 'JOB_THREAD_COUNT')";
pub(super) const EXISTS: &str =
    "SELECT COUNT(*) AS N FROM SYSTEM_.SYS_TABLES_ WHERE TABLE_NAME = 'SYS_JOBS_'";

/// The columns every version has.
const BASE: &str = "JOB_ID, JOB_NAME, EXEC_QUERY,
       TO_CHAR(START_TIME, 'YYYY-MM-DD HH24:MI:SS') AS START_TIME,
       TO_CHAR(END_TIME, 'YYYY-MM-DD HH24:MI:SS') AS END_TIME,
       \"INTERVAL\", INTERVAL_TYPE, STATE,
       TO_CHAR(LAST_EXEC_TIME, 'YYYY-MM-DD HH24:MI:SS') AS LAST_EXEC_TIME,
       EXEC_COUNT, ERROR_CODE";

/// The jobs (or the one whose `JOB_ID` is the parameter), with the columns
/// 6.5.1 added (`full`) or without them.
pub(super) fn select(full: bool, one: bool) -> String {
    let extra = if full { ", IS_ENABLE, \"COMMENT\"" } else { "" };
    let filter = if one {
        "WHERE JOB_ID = ?"
    } else {
        "ORDER BY JOB_NAME"
    };
    format!("SELECT {BASE}{extra}\n  FROM SYSTEM_.SYS_JOBS_ {filter}")
}

/// The jobs, and whether the server has `IS_ENABLE` and `COMMENT`.
fn read(src: &mut dyn JobSource, id: Option<&str>) -> Result<(Set, bool)> {
    let params: Vec<&str> = id.into_iter().collect();
    match src.query(&select(true, id.is_some()), &params) {
        Ok(s) => Ok((s, true)),
        Err(e) => {
            tracing::debug!("odbc jobs: SYS_JOBS_ with IS_ENABLE failed: {e}");
            src.query(&select(false, id.is_some()), &params)
                .map(|s| (s, false))
                .map_err(|_| e)
        }
    }
}

fn one(src: &mut dyn JobSource, id: &str) -> Result<(Set, bool)> {
    let n = int_id(id)?.to_string();
    let (s, full) = read(src, Some(&n))?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok((s, full))
}

/// `INTERVAL_TYPE`'s unit as CREATE JOB writes it.
fn unit(t: Option<&str>) -> Option<&'static str> {
    match t.map(|t| t.trim().to_ascii_uppercase()).as_deref() {
        Some("YY") => Some("YEAR"),
        Some("MM") => Some("MONTH"),
        Some("DD") => Some("DAY"),
        Some("HH") => Some("HOUR"),
        Some("MI") => Some("MINUTE"),
        _ => None,
    }
}

/// `INTERVAL 1 DAY`, when it repeats.
fn every(s: &Set, r: usize) -> Option<String> {
    let n = int(s.get(r, "INTERVAL")).filter(|n| *n > 0)?;
    Some(format!("INTERVAL {n} {}", unit(s.get(r, "INTERVAL_TYPE"))?))
}

/// The schedule in CREATE JOB's terms, with the server's local times:
/// `START 2026-11-12 03:00:00 END … INTERVAL 1 DAY`.
fn schedule(s: &Set, r: usize) -> Option<String> {
    let mut parts: Vec<String> = Vec::new();
    if let Some(t) = s.get(r, "START_TIME") {
        parts.push(format!("START {t}"));
    }
    if let Some(t) = s.get(r, "END_TIME") {
        parts.push(format!("END {t}"));
    }
    parts.extend(every(s, r));
    (!parts.is_empty()).then(|| parts.join(" "))
}

fn job(s: &Set, r: usize, tz: &Tz, full: bool, sys: bool) -> Option<ServerJob> {
    let id = int(s.get(r, "JOB_ID"))?;
    let running = int(s.get(r, "STATE")) == Some(1);
    let ran = s.get(r, "LAST_EXEC_TIME");
    let outcome = ran.map(|_| {
        if s.get(r, "ERROR_CODE").is_some() {
            JobOutcome::Failed
        } else {
            JobOutcome::Succeeded
        }
    });
    Some(ServerJob {
        id: id.to_string(),
        name: s.get(r, "JOB_NAME").unwrap_or_default().to_string(),
        kind: KIND.into(),
        group: None,
        description: owned(s.get(r, "COMMENT")),
        owner: None,
        enabled: !full || yes(s.get(r, "IS_ENABLE")),
        schedule: schedule(s, r),
        running,
        last_outcome: last(outcome, running),
        last_started: tz.utc(ran),
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        can_stop: false,
        can_enable: full && sys,
        has_history: false,
        has_script: true,
    })
}

/// The connected user is SYS, the only one that alters jobs.
fn is_sys(src: &mut dyn JobSource) -> bool {
    match src.query(USER, &[]) {
        Ok(s) => s.get(0, "U").is_some_and(|u| u.eq_ignore_ascii_case("SYS")),
        Err(e) => {
            tracing::debug!("odbc jobs: USER_NAME() failed: {e}");
            false
        }
    }
}

/// The job scheduler is off (best effort).
fn off(src: &mut dyn JobSource) -> bool {
    let s = match src.query(PROPS, &[]) {
        Ok(s) => s,
        Err(e) => {
            tracing::debug!("odbc jobs: V$PROPERTY failed: {e}");
            return false;
        }
    };
    (0..s.rows.len()).any(|r| int(s.get(r, "VALUE1")) == Some(0))
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let (s, full) = match read(src, None) {
        Ok(v) => v,
        Err(e) => {
            let here = src
                .query(EXISTS, &[])
                .ok()
                .and_then(|s| int(s.get(0, "N")))
                .map(|n| n > 0);
            return match here {
                Some(false) => Ok(JobList {
                    jobs: Vec::new(),
                    notes: vec![NOT_HERE.into()],
                }),
                _ => Err(e),
            };
        }
    };
    let tz = Tz::query(src, NOW);
    let sys = is_sys(src);
    let jobs: Vec<ServerJob> = (0..s.rows.len())
        .filter_map(|r| job(&s, r, &tz, full, sys))
        .collect();
    let notes = if !jobs.is_empty() && off(src) {
        vec![OFF.into()]
    } else {
        Vec::new()
    };
    Ok(JobList { jobs, notes })
}

/// `EXEC_QUERY` as the procedure call, without an `EXEC` in front.
fn call(q: &str) -> &str {
    let t = q.trim();
    match t.get(..5) {
        Some(head) if head.eq_ignore_ascii_case("EXEC ") => t[5..].trim_start(),
        _ => t,
    }
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let (s, full) = one(src, id)?;
    let tz = Tz::query(src, NOW);
    let sys = is_sys(src);
    let job = job(&s, 0, &tz, full, sys).ok_or_else(|| not_found(id))?;
    let steps = s
        .get(0, "EXEC_QUERY")
        .map(|q| JobStep {
            name: job.name.clone(),
            kind: Some("Procedimiento".into()),
            target: None,
            command: Some(call(q).to_string()),
        })
        .into_iter()
        .collect();
    let interval = every(&s, 0).map(|i| i["INTERVAL ".len()..].to_string());
    let properties = props(vec![
        prop("", "Inicio", s.get(0, "START_TIME")),
        prop("", "Fin", s.get(0, "END_TIME")),
        prop("", "Intervalo", interval.as_deref()),
        prop("", "Última ejecución", s.get(0, "LAST_EXEC_TIME")),
        prop("", "Ejecuciones", s.get(0, "EXEC_COUNT")),
        prop(
            "",
            "Código de error de la última ejecución",
            s.get(0, "ERROR_CODE"),
        ),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
    })
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let (s, full) = one(src, id)?;
    if !full {
        return Err(Error::Unsupported(NO_ENABLE.into()));
    }
    if !is_sys(src) {
        return Err(Error::Unsupported(ONLY_SYS.into()));
    }
    let name = s.get(0, "JOB_NAME").ok_or_else(|| not_found(id))?;
    src.exec(
        &format!(
            "ALTER JOB {} SET {}",
            q(name),
            if enabled { "ENABLE" } else { "DISABLE" }
        ),
        &[],
    )
    .map(|_| ())
}

/// A DATE literal rebuilt from the value (never the text as it came).
fn date_lit(v: Option<&str>) -> Option<String> {
    let ms = parse_ts(v?)?;
    Some(format!("TO_DATE('{}', 'YYYY-MM-DD HH24:MI:SS')", plain(ms)))
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let (s, full) = one(src, id)?;
    let name = s.get(0, "JOB_NAME").ok_or_else(|| not_found(id))?;
    let exec = s.get(0, "EXEC_QUERY").ok_or_else(|| not_found(id))?;
    let mut out = format!("CREATE JOB {}\nEXEC {}", q(name), call(exec));
    let start = date_lit(s.get(0, "START_TIME")).unwrap_or_else(|| "SYSDATE".into());
    out.push_str(&format!("\nSTART {start}"));
    if let Some(end) = date_lit(s.get(0, "END_TIME")) {
        out.push_str(&format!("\nEND {end}"));
    }
    if let Some(i) = every(&s, 0) {
        out.push_str(&format!("\n{i}"));
    }
    if full {
        out.push_str(if yes(s.get(0, "IS_ENABLE")) {
            "\nENABLE"
        } else {
            "\nDISABLE"
        });
        if let Some(c) = s.get(0, "COMMENT") {
            out.push_str(&format!("\nCOMMENT {}", lit(c)));
        }
    }
    out.push_str(";\n");
    Ok(out)
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{select, EXISTS, NOW, PROPS, USER};
}
