//! InterSystems IRIS and Caché: the Task Manager's tasks (`%SYS.Task`)
//! and their history (`%SYS_Task.History`). Their data lives in the `%SYS`
//! namespace: from another namespace the table answers empty, so an empty
//! list says where to connect. Running, suspending and resuming a task are
//! ObjectScript methods SQL doesn't reach, and a task is created with
//! ObjectScript too: this is read only.
//!
//! Columns are read by name, so a version without one leaves its field
//! empty; times come as ODBC text or as `$HOROLOG` (`67000,3600`).

use super::*;
use dbine_driver::{JobStep, ServerJob};

pub const NO_CONTROL: &str = "IRIS ejecuta, suspende y reanuda sus tareas con métodos de %SYS.Task que SQL no alcanza: se hace desde el Portal de Administración";
pub const NO_SCRIPT: &str = "las tareas de IRIS se crean con ObjectScript (%SYS.Task), no con SQL";

const KIND: &str = "Task Manager";
pub(super) const NOW: &str = "SELECT CURRENT_TIMESTAMP AS LOCAL_NOW, GETUTCDATE() AS UTC_NOW";
pub(super) const LIST: &str = "SELECT %ID AS JOB_ID, * FROM %SYS.Task";
const ONE: &str = "SELECT %ID AS JOB_ID, * FROM %SYS.Task WHERE %ID = ?";
const HAS_HISTORY: &str = "SELECT TOP 1 %ID AS RUN_ID FROM %SYS_Task.History";

const NOT_VISIBLE: &str = "Las tareas del Task Manager se ven conectando al namespace %SYS con un usuario que tenga el recurso %Admin_Task.";

/// `$HOROLOG` (`days,seconds`; day 0 is 1840-12-31) as local ms.
pub(super) fn horolog(v: &str) -> Option<i64> {
    let t = v.trim();
    let (d, s) = match t.split_once(',') {
        Some((d, s)) => (d, s),
        None => (t, "0"),
    };
    let days: i64 = d.trim().parse().ok().filter(|d| *d > 0)?;
    let secs: f64 = s
        .trim()
        .parse()
        .ok()
        .filter(|s: &f64| s.is_finite() && *s >= 0.0)?;
    Some((days - 47117) * 86_400_000 + (secs * 1000.0) as i64)
}

/// A time in either form, local ms.
fn when(v: Option<&str>) -> Option<i64> {
    let v = v?.trim();
    if v.is_empty() || v == "0" {
        return None;
    }
    parse_ts(v).or_else(|| horolog(v))
}

/// Seconds after midnight, as a %Time text or a number.
fn day_secs(v: Option<&str>) -> Option<i64> {
    let v = v?.trim();
    parse_time(v)
        .map(|ms| ms / 1000)
        .or_else(|| v.parse::<i64>().ok().filter(|s| (0..86_400).contains(s)))
}

fn hhmm(secs: i64) -> String {
    format!("{:02}:{:02}", secs / 3600, secs / 60 % 60)
}

/// The next run: `DayNextScheduled` (a date or a `$H` day) and
/// `TimeNextScheduled`.
fn next(s: &Set, r: usize) -> Option<i64> {
    let day = s.get(r, "DayNextScheduled")?;
    let day = parse_ts(day).or_else(|| horolog(day))?;
    Some(day + day_secs(s.get(r, "TimeNextScheduled")).unwrap_or(0) * 1000)
}

/// The schedule in iCalendar-like terms: `FREQ=DAILY;AT=03:00`,
/// `FREQ=WEEKLY;INTERVAL=2;EVERY=15MIN;BETWEEN=08:00-18:00`… `None` for
/// tasks run on demand.
fn schedule(s: &Set, r: usize) -> Option<String> {
    let freq = match int(s.get(r, "TimePeriod")) {
        Some(0) => "DAILY",
        Some(1) => "WEEKLY",
        Some(2 | 3) => "MONTHLY",
        Some(4) => return Some("AFTER=TASK".into()),
        Some(5) => return None,
        _ => return None,
    };
    let mut out = format!("FREQ={freq}");
    if let Some(n) = int(s.get(r, "TimePeriodEvery")).filter(|n| *n > 1) {
        out.push_str(&format!(";INTERVAL={n}"));
    }
    if let Some(d) = s.get(r, "TimePeriodDay") {
        out.push_str(&format!(";DAY={d}"));
    }
    let start = day_secs(s.get(r, "DailyStartTime"));
    if int(s.get(r, "DailyFrequency")) == Some(1) {
        let unit = if int(s.get(r, "DailyFrequencyTime")) == Some(1) {
            "H"
        } else {
            "MIN"
        };
        if let Some(n) = int(s.get(r, "DailyIncrement")) {
            out.push_str(&format!(";EVERY={n}{unit}"));
        }
        if let (Some(a), Some(b)) = (start, day_secs(s.get(r, "DailyEndTime"))) {
            out.push_str(&format!(";BETWEEN={}-{}", hhmm(a), hhmm(b)));
        }
    } else if let Some(a) = start {
        out.push_str(&format!(";AT={}", hhmm(a)));
    }
    Some(out)
}

fn failed(status: Option<&str>, error: Option<&str>) -> bool {
    error.is_some_and(|e| !e.trim().is_empty())
        || status.is_some_and(|s| !matches!(s.trim(), "" | "0" | "1"))
}

fn job(s: &Set, r: usize, tz: &Tz, history: bool) -> Option<ServerJob> {
    let id = int(s.get(r, "JOB_ID"))?;
    let running = s
        .get(r, "RunningJobNumber")
        .is_some_and(|v| v.trim() != "0");
    let started = when(s.get(r, "LastStarted"));
    let finished = when(s.get(r, "LastFinished"));
    let outcome = finished.map(|_| {
        if failed(s.get(r, "Status"), s.get(r, "Error")) {
            JobOutcome::Failed
        } else {
            JobOutcome::Succeeded
        }
    });
    let suspended = int(s.get(r, "Suspended")).unwrap_or(0) != 0;
    Some(ServerJob {
        id: id.to_string(),
        name: s.get(r, "Name").unwrap_or_default().to_string(),
        kind: KIND.into(),
        group: owned(s.get(r, "NameSpace")),
        description: owned(s.get(r, "Description")),
        owner: owned(s.get(r, "RunAsUser")),
        enabled: !suspended,
        schedule: schedule(s, r),
        running,
        last_outcome: last(outcome, running),
        last_started: started.map(|ms| tz.utc_ms(ms)),
        last_duration_ms: started
            .zip(finished)
            .and_then(|(a, b)| u64::try_from(b - a).ok()),
        next_run: next(s, r).filter(|_| !suspended).map(|ms| tz.utc_ms(ms)),
        can_run: false,
        can_stop: false,
        can_enable: false,
        has_history: history,
        has_script: false,
    })
}

fn note(e: &Error) -> Option<&'static str> {
    matches!(code(e), Some(-30 | -99 | -112)).then_some(NOT_VISIBLE)
}

fn has_history(src: &mut dyn JobSource) -> bool {
    src.query(HAS_HISTORY, &[]).is_ok()
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let tz = Tz::query(src, NOW);
    let s = match src.query(LIST, &[]) {
        Ok(s) => s,
        Err(e) => match note(&e) {
            Some(n) => {
                return Ok(JobList {
                    jobs: Vec::new(),
                    notes: vec![n.into()],
                })
            }
            None => return Err(e),
        },
    };
    let history = has_history(src);
    let mut jobs: Vec<ServerJob> = (0..s.rows.len())
        .filter_map(|r| job(&s, r, &tz, history))
        .collect();
    jobs.sort_by(|a, b| a.name.cmp(&b.name));
    // IRIS always has system tasks: none means this namespace doesn't see
    // them.
    let notes = if jobs.is_empty() {
        vec![NOT_VISIBLE.into()]
    } else {
        Vec::new()
    };
    Ok(JobList { jobs, notes })
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let n = int_id(id)?.to_string();
    let tz = Tz::query(src, NOW);
    let s = src.query(ONE, &[&n])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    let history = has_history(src);
    let job = job(&s, 0, &tz, history).ok_or_else(|| not_found(id))?;
    let steps = s
        .get(0, "TaskClass")
        .map(|c| JobStep {
            name: c.to_string(),
            kind: Some("Clase".into()),
            target: owned(s.get(0, "NameSpace")),
            command: Some(c.to_string()),
        })
        .into_iter()
        .collect();
    let properties = props(vec![
        prop("", "Clase", s.get(0, "TaskClass")),
        prop("", "Namespace", s.get(0, "NameSpace")),
        prop("", "Ejecuta como", s.get(0, "RunAsUser")),
        prop("", "Tipo", s.get(0, "Type")),
        prop("", "Prioridad", s.get(0, "Priority")),
        prop("", "Suspendida", s.get(0, "Suspended")),
        prop("", "Desde", s.get(0, "StartDate")),
        prop("", "Hasta", s.get(0, "EndDate")),
        prop("", "Último error", s.get(0, "Error")),
        prop("", "Configuración", s.get(0, "Settings")),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
    })
}

pub fn runs(src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let n = int_id(id)?.to_string();
    let tz = Tz::query(src, NOW);
    let s = src.query(
        &format!(
            "SELECT TOP {limit} %ID AS RUN_ID, * FROM %SYS_Task.History WHERE Task = ? ORDER BY %ID DESC"
        ),
        &[&n],
    )?;
    let mut out: Vec<JobRun> = (0..s.rows.len())
        .map(|r| {
            let started = when(get(&s, r, &["DisplayStarted", "LastStarted"]));
            let finished = when(get(&s, r, &["DisplayFinished", "LastFinished"]));
            let status = s.get(r, "Status");
            let error = s.get(r, "Error");
            let outcome = if finished.is_none() {
                JobOutcome::Running
            } else if failed(status, error) {
                JobOutcome::Failed
            } else {
                JobOutcome::Succeeded
            };
            let message = [s.get(r, "DisplayStatus"), error]
                .into_iter()
                .flatten()
                .map(str::trim)
                .filter(|m| !m.is_empty() && *m != "1")
                .collect::<Vec<_>>();
            JobRun {
                id: owned(s.get(r, "RUN_ID")),
                started: started.map(|ms| tz.utc_ms(ms)),
                duration_ms: started
                    .zip(finished)
                    .and_then(|(a, b)| u64::try_from(b - a).ok()),
                outcome,
                trigger: None,
                message: (!message.is_empty()).then(|| {
                    let mut m = message;
                    m.dedup();
                    m.join("\n")
                }),
                steps: Vec::new(),
            }
        })
        .collect();
    newest_first(&mut out);
    Ok(out)
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{horolog, LIST, NOW};
}
