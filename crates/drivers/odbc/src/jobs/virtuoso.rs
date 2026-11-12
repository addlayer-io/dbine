//! OpenLink Virtuoso: the scheduler's events, rows of
//! `DB.DBA.SYS_SCHEDULED_EVENT` (a name, a first start, an interval in
//! minutes, the SQL it runs, when it last completed and the error it
//! left). Every `SchedulerInterval` minutes (virtuoso.ini, 0 turns it off)
//! the server runs the events whose start has passed and whose interval
//! has elapsed since they last completed, skipping those with
//! `SE_DISABLED` set and those with no interval. Enabling is an UPDATE of
//! `SE_DISABLED` (servers from before the column can't disable one).
//!
//! Virtuoso has no call that runs one event now (`scheduler_do_round`
//! runs every event that's due), keeps no history beyond the last
//! completion and its error, and nothing stops a run. Only dba reads the
//! table. Ids are event names, always bound as parameters. Times are the
//! server's local time.

use super::*;
use dbine_driver::{JobStep, ServerJob};

pub const NO_RUN_NOW: &str =
    "Virtuoso no tiene una llamada que ejecute un evento programado fuera de su intervalo";
pub const NO_HISTORY: &str =
    "Virtuoso no guarda el historial de sus eventos: solo la última ejecución y su error";
const NO_ENABLE: &str =
    "esta versión de Virtuoso no permite deshabilitar un evento (no tiene la columna SE_DISABLED)";

const NO_ACCESS: &str =
    "Solo los usuarios dba ven el planificador de Virtuoso (DB.DBA.SYS_SCHEDULED_EVENT).";
const OFF: &str = "El planificador de Virtuoso está apagado (SchedulerInterval = 0 en virtuoso.ini): los eventos no se ejecutan.";

const KIND: &str = "Scheduler";

pub(super) const NOW: &str = "SELECT CAST(curdatetime() AS VARCHAR) AS LOCAL_NOW, CAST(curutcdatetime() AS VARCHAR) AS UTC_NOW";
pub(super) const INTERVAL: &str =
    "SELECT cfg_item_value(virtuoso_ini_path(), 'Parameters', 'SchedulerInterval') AS SI";
/// The flag goes as a number (a bound text makes Virtuoso warn about the
/// types); the name, bound.
pub(super) const UPDATE: &str = "UPDATE DB.DBA.SYS_SCHEDULED_EVENT SET SE_DISABLED = ";

/// The columns every version has.
const BASE: &str = "SE_NAME, CAST(SE_START AS VARCHAR) AS SE_START, SE_INTERVAL, CAST(SE_LAST_COMPLETED AS VARCHAR) AS SE_LAST_COMPLETED, SE_SQL";

/// The events (or the one named by the parameter), with the columns that
/// came later (`full`) or without them.
pub(super) fn select(full: bool, one: bool) -> String {
    let extra = if full {
        ", SE_LAST_ERROR, SE_ENABLE_NOTIFY, SE_NOTIFY, SE_DISABLED"
    } else {
        ""
    };
    let filter = if one {
        "WHERE SE_NAME = ?"
    } else {
        "ORDER BY SE_NAME"
    };
    format!("SELECT {BASE}{extra} FROM DB.DBA.SYS_SCHEDULED_EVENT {filter}")
}

/// The user isn't allowed to read the table (`SQ033: SELECT access
/// denied…`).
fn denied(e: &Error) -> bool {
    e.to_string().to_ascii_lowercase().contains("access denied")
}

/// The events, and whether the server has the later columns.
fn read(src: &mut dyn JobSource, name: Option<&str>) -> Result<(Set, bool)> {
    let params: Vec<&str> = name.into_iter().collect();
    match src.query(&select(true, name.is_some()), &params) {
        Ok(s) => Ok((s, true)),
        Err(e) if denied(&e) => Err(e),
        Err(e) => {
            tracing::debug!("odbc jobs: SYS_SCHEDULED_EVENT's later columns failed: {e}");
            src.query(&select(false, name.is_some()), &params)
                .map(|s| (s, false))
                .map_err(|_| e)
        }
    }
}

fn one(src: &mut dyn JobSource, id: &str) -> Result<(Set, bool)> {
    let name = name_id(id)?;
    let (s, full) = read(src, Some(name))?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok((s, full))
}

/// `Cada 10 minutos, desde 2026-11-12 03:00:00`; `None` when it has no
/// interval (the scheduler never runs it).
fn schedule(start: Option<&str>, interval: i64) -> Option<String> {
    if interval <= 0 {
        return None;
    }
    let every = if interval == 1 {
        "Cada minuto".to_string()
    } else {
        format!("Cada {interval} minutos")
    };
    Some(match start.and_then(parse_ts) {
        Some(ms) => format!("{every}, desde {}", plain(ms)),
        None => every,
    })
}

/// When the scheduler will run it next: its start, or its interval after
/// the last completion, whichever is later; only while that's ahead (an
/// event that's due runs at the scheduler's next round).
fn next(s: &Set, r: usize, tz: &Tz, enabled: bool, interval: i64) -> Option<String> {
    if !enabled || interval <= 0 {
        return None;
    }
    let start = s.get(r, "SE_START").and_then(parse_ts);
    let after = s
        .get(r, "SE_LAST_COMPLETED")
        .and_then(parse_ts)
        .map(|t| t + interval * 60_000);
    let due = start.into_iter().chain(after).max()?;
    match tz.now {
        Some(now) if due <= now => None,
        _ => Some(tz.utc_ms(due)),
    }
}

fn job(s: &Set, r: usize, tz: &Tz, full: bool) -> Option<ServerJob> {
    let name = s.get(r, "SE_NAME")?.to_string();
    let enabled = !full || int(s.get(r, "SE_DISABLED")).unwrap_or(0) == 0;
    let interval = int(s.get(r, "SE_INTERVAL")).unwrap_or(0);
    let last = s.get(r, "SE_LAST_COMPLETED");
    let failed = s.get(r, "SE_LAST_ERROR").is_some();
    Some(ServerJob {
        id: name.clone(),
        name,
        kind: KIND.into(),
        group: None,
        description: None,
        owner: None,
        enabled,
        schedule: schedule(s.get(r, "SE_START"), interval),
        running: false,
        last_outcome: last.map(|_| {
            if failed {
                JobOutcome::Failed
            } else {
                JobOutcome::Succeeded
            }
        }),
        // The engine records when the last run completed, not when it
        // started: the closest it has.
        last_started: tz.utc(last),
        last_duration_ms: None,
        next_run: next(s, r, tz, enabled, interval),
        can_run: false,
        can_stop: false,
        can_enable: full,
        has_history: false,
        has_script: true,
        system: false,
    })
}

/// SchedulerInterval is 0: nothing runs (best effort).
fn off(src: &mut dyn JobSource) -> bool {
    match src.query(INTERVAL, &[]) {
        Ok(s) => int(s.get(0, "SI")) == Some(0),
        Err(e) => {
            tracing::debug!("odbc jobs: SchedulerInterval unreadable: {e}");
            false
        }
    }
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let (s, full) = match read(src, None) {
        Ok(v) => v,
        Err(e) if denied(&e) => {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![NO_ACCESS.into()],
            })
        }
        Err(e) => return Err(e),
    };
    let tz = Tz::query(src, NOW);
    let notes = if off(src) {
        vec![OFF.into()]
    } else {
        Vec::new()
    };
    Ok(JobList {
        jobs: (0..s.rows.len())
            .filter_map(|r| job(&s, r, &tz, full))
            .collect(),
        notes,
    })
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let (s, full) = one(src, id)?;
    let tz = Tz::query(src, NOW);
    let job = job(&s, 0, &tz, full).ok_or_else(|| not_found(id))?;
    let steps = s
        .get(0, "SE_SQL")
        .map(|sql| JobStep {
            name: job.name.clone(),
            kind: Some("SQL".into()),
            target: None,
            command: Some(sql.to_string()),
        })
        .into_iter()
        .collect();
    let interval = s.get(0, "SE_INTERVAL");
    let notify = (int(s.get(0, "SE_ENABLE_NOTIFY")) == Some(1))
        .then(|| s.get(0, "SE_NOTIFY"))
        .flatten();
    let properties = props(vec![
        prop("", "Inicio", s.get(0, "SE_START")),
        prop("", "Intervalo (minutos)", interval),
        prop(
            "",
            "Última ejecución completada",
            s.get(0, "SE_LAST_COMPLETED"),
        ),
        prop("", "Último error", s.get(0, "SE_LAST_ERROR")),
        prop("", "Aviso por correo a", notify),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let (s, full) = one(src, id)?;
    if !full {
        return Err(Error::Unsupported(NO_ENABLE.into()));
    }
    let name = s.get(0, "SE_NAME").ok_or_else(|| not_found(id))?;
    let sql = format!("{UPDATE}{} WHERE SE_NAME = ?", if enabled { 0 } else { 1 });
    let changed = src.exec(&sql, &[name])?;
    if changed == Some(0) {
        return Err(not_found(id));
    }
    Ok(())
}

/// A text literal for Virtuoso, where `\` escapes too.
fn vlit(v: &str) -> String {
    lit(&v.replace('\\', "\\\\"))
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let (s, full) = one(src, id)?;
    let name = s.get(0, "SE_NAME").ok_or_else(|| not_found(id))?;
    // The start rebuilt from its value, never the text as it came.
    let start = s
        .get(0, "SE_START")
        .and_then(parse_ts)
        .map(|ms| format!("CAST('{}' AS DATETIME)", plain(ms)))
        .unwrap_or_else(|| "NULL".into());
    let interval = int(s.get(0, "SE_INTERVAL")).unwrap_or(0).to_string();
    let sql = s.get(0, "SE_SQL").map(vlit).unwrap_or_else(|| "''".into());
    let mut cols: Vec<(&str, String)> = vec![
        ("SE_NAME", vlit(name)),
        ("SE_START", start),
        ("SE_INTERVAL", interval),
        ("SE_SQL", sql),
    ];
    if full {
        let disabled = int(s.get(0, "SE_DISABLED")).unwrap_or(0) != 0;
        cols.push(("SE_DISABLED", if disabled { "1" } else { "0" }.into()));
        if let (Some(1), Some(to)) = (int(s.get(0, "SE_ENABLE_NOTIFY")), s.get(0, "SE_NOTIFY")) {
            cols.push(("SE_ENABLE_NOTIFY", "1".into()));
            cols.push(("SE_NOTIFY", vlit(to)));
        }
    }
    Ok(format!(
        "INSERT INTO DB.DBA.SYS_SCHEDULED_EVENT ({})\nVALUES ({});\n",
        cols.iter().map(|(c, _)| *c).collect::<Vec<_>>().join(", "),
        cols.iter()
            .map(|(_, v)| v.as_str())
            .collect::<Vec<_>>()
            .join(", "),
    ))
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{select, INTERVAL, NOW, UPDATE};
}
