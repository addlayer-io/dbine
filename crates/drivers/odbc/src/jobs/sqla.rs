//! SQL Anywhere: events (`SYS.SYSEVENT`), with their schedules
//! (`SYS.SYSSCHEDULE`) or the system event type that fires them.
//! `TRIGGER EVENT` runs the handler on its own connection and returns at
//! once; `ALTER EVENT … ENABLE | DISABLE` turns the schedule on and off.
//! The server keeps no run history (only its message log does).

use super::*;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{JobStep, ServerJob};

pub const NO_HISTORY: &str =
    "SQL Anywhere no guarda el historial de sus eventos: las ejecuciones se ven en el registro de mensajes del servidor";

const KIND: &str = "Event";

pub(super) const LIST: &str =
    "SELECT E.event_id, E.event_name, E.enabled, E.location, E.condition, E.remarks,
       USER_NAME(E.creator) AS creator_name, T.name AS event_type
  FROM SYS.SYSEVENT E
  LEFT OUTER JOIN SYS.SYSEVENTTYPE T ON T.event_type_id = E.event_type_id
 ORDER BY E.event_name";

pub(super) const ONE: &str =
    "SELECT E.event_id, E.event_name, E.enabled, E.location, E.condition, E.remarks, E.action,
       USER_NAME(E.creator) AS creator_name, T.name AS event_type
  FROM SYS.SYSEVENT E
  LEFT OUTER JOIN SYS.SYSEVENTTYPE T ON T.event_type_id = E.event_type_id
 WHERE E.event_id = ?";

pub(super) const SCHEDULES: &str =
    "SELECT event_id, sched_name, recurring, start_time, stop_time, start_date,
       days_of_week, days_of_month, interval_units, interval_amt
  FROM SYS.SYSSCHEDULE ORDER BY event_id, sched_name";

const WEEKDAYS: [&str; 7] = [
    "Sunday",
    "Monday",
    "Tuesday",
    "Wednesday",
    "Thursday",
    "Friday",
    "Saturday",
];

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

/// `'HH:MM:SS'` rebuilt from the value (never the text as it came).
fn time_lit(v: Option<&str>) -> Option<String> {
    let s = parse_time(v?)? / 1000;
    Some(format!(
        "'{:02}:{:02}:{:02}'",
        s / 3600,
        s / 60 % 60,
        s % 60
    ))
}

fn date_lit(v: Option<&str>) -> Option<String> {
    let iso = rfc3339(parse_ts(v?)?);
    Some(format!("'{}'", &iso[..10]))
}

/// One SYSSCHEDULE row as CREATE EVENT's schedule clause, without its
/// name: `START TIME '03:00:00' EVERY 1 HOURS ON ('Monday')…`.
fn clause(s: &Set, r: usize) -> String {
    let mut out = match (
        time_lit(s.get(r, "start_time")),
        time_lit(s.get(r, "stop_time")),
    ) {
        (Some(a), Some(b)) => format!("BETWEEN {a} AND {b}"),
        (Some(a), None) => format!("START TIME {a}"),
        (None, _) => "START TIME '00:00:00'".to_string(),
    };
    if let Some(n) = int(s.get(r, "interval_amt")).filter(|n| *n > 0) {
        let unit = match s.get(r, "interval_units").map(|u| u.to_ascii_uppercase()) {
            Some(u) if u == "HH" => "HOURS",
            Some(u) if u == "SS" => "SECONDS",
            _ => "MINUTES",
        };
        out.push_str(&format!(" EVERY {n} {unit}"));
    }
    let week = int(s.get(r, "days_of_week")).unwrap_or(0);
    let month = int(s.get(r, "days_of_month")).unwrap_or(0);
    if week > 0 {
        let days: Vec<String> = (0..7)
            .filter(|i| week & (1 << i) != 0)
            .map(|i| format!("'{}'", WEEKDAYS[i]))
            .collect();
        out.push_str(&format!(" ON ({})", days.join(", ")));
    } else if month > 0 {
        // Bits 0–30 are days 1–31; bit 31 the last day (0 in the syntax).
        let mut days: Vec<String> = (0..31)
            .filter(|i| month & (1 << i) != 0)
            .map(|i| (i + 1).to_string())
            .collect();
        if month & (1 << 31) != 0 {
            days.push("0".into());
        }
        out.push_str(&format!(" ON ({})", days.join(", ")));
    }
    if let Some(d) = date_lit(s.get(r, "start_date")) {
        out.push_str(&format!(" START DATE {d}"));
    }
    out
}

/// The schedules of event `id`: (name, clause).
fn schedules_of(sch: &Set, id: &str) -> Vec<(String, String)> {
    (0..sch.rows.len())
        .filter(|r| sch.get(*r, "event_id") == Some(id))
        .map(|r| {
            (
                sch.get(r, "sched_name").unwrap_or_default().to_string(),
                clause(sch, r),
            )
        })
        .collect()
}

/// What fires it: its schedules, or its system event type.
fn firing(s: &Set, r: usize, scheds: &[(String, String)]) -> Option<String> {
    if !scheds.is_empty() {
        return Some(
            scheds
                .iter()
                .map(|(_, c)| c.as_str())
                .collect::<Vec<_>>()
                .join("; "),
        );
    }
    s.get(r, "event_type").map(|t| match s.get(r, "condition") {
        Some(c) => format!("TYPE {t} WHERE {c}"),
        None => format!("TYPE {t}"),
    })
}

fn job(s: &Set, r: usize, sch: &Set) -> Option<ServerJob> {
    let id = int(s.get(r, "event_id"))?.to_string();
    let enabled = yes(s.get(r, "enabled"));
    let scheds = schedules_of(sch, &id);
    Some(ServerJob {
        name: s.get(r, "event_name").unwrap_or_default().to_string(),
        id,
        kind: KIND.into(),
        group: None,
        description: owned(s.get(r, "remarks")),
        owner: owned(s.get(r, "creator_name")),
        enabled,
        schedule: firing(s, r, &scheds),
        running: false,
        last_outcome: None,
        last_started: None,
        last_duration_ms: None,
        next_run: None,
        can_run: enabled,
        can_stop: false,
        can_enable: true,
        has_history: false,
        has_script: true,
        system: false,
    })
}

fn schedules(src: &mut dyn JobSource) -> Set {
    src.query(SCHEDULES, &[]).unwrap_or_else(|e| {
        tracing::debug!("odbc jobs: SYSSCHEDULE failed: {e}");
        Set::default()
    })
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let s = src.query(LIST, &[])?;
    let sch = schedules(src);
    Ok(JobList {
        jobs: (0..s.rows.len()).filter_map(|r| job(&s, r, &sch)).collect(),
        notes: Vec::new(),
    })
}

fn one(src: &mut dyn JobSource, id: &str) -> Result<Set> {
    let n = int_id(id)?.to_string();
    let s = src.query(ONE, &[&n])?;
    if s.rows.is_empty() {
        return Err(not_found(id));
    }
    Ok(s)
}

fn location(v: Option<&str>) -> Option<&'static str> {
    match v.map(|v| v.trim().to_ascii_uppercase()).as_deref() {
        Some("C") => Some("CONSOLIDATED"),
        Some("R") => Some("REMOTE"),
        _ => None,
    }
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let s = one(src, id)?;
    let sch = schedules(src);
    let job = job(&s, 0, &sch).ok_or_else(|| not_found(id))?;
    let scheds = schedules_of(&sch, &job.id);
    let steps = s
        .get(0, "action")
        .map(|a| JobStep {
            name: job.name.clone(),
            kind: Some("SQL".into()),
            target: None,
            command: Some(a.to_string()),
        })
        .into_iter()
        .collect();
    let properties = props(vec![
        prop("", "Tipo de evento", s.get(0, "event_type")),
        prop("", "Condición", s.get(0, "condition")),
        prop("", "Dónde se dispara", location(s.get(0, "location"))),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: if scheds.len() > 1 {
            scheds
                .into_iter()
                .map(|(n, c)| format!("{} {c}", q(&n)))
                .collect()
        } else {
            Vec::new()
        },
        properties,
        notes: Vec::new(),
    })
}

pub fn run(src: &mut dyn JobSource, id: &str) -> Result<()> {
    let s = one(src, id)?;
    let name = s.get(0, "event_name").ok_or_else(|| not_found(id))?;
    src.exec(&format!("TRIGGER EVENT {}", q(name)), &[])
        .map(|_| ())
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let s = one(src, id)?;
    let name = s.get(0, "event_name").ok_or_else(|| not_found(id))?;
    src.exec(
        &format!(
            "ALTER EVENT {} {}",
            q(name),
            if enabled { "ENABLE" } else { "DISABLE" }
        ),
        &[],
    )
    .map(|_| ())
}

/// A text literal for SQL Anywhere, where `\` escapes too.
fn sqla_lit(v: &str) -> String {
    lit(&v.replace('\\', "\\\\"))
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let s = one(src, id)?;
    let sch = schedules(src);
    let name = s.get(0, "event_name").ok_or_else(|| not_found(id))?;
    let scheds = schedules_of(&sch, &int_id(id)?.to_string());
    let mut out = format!("CREATE EVENT {}", q(name));
    if !scheds.is_empty() {
        let specs: Vec<String> = scheds
            .iter()
            .map(|(n, c)| format!("{} {c}", q(n)))
            .collect();
        out.push_str(&format!("\nSCHEDULE {}", specs.join(",\n         ")));
    } else if let Some(t) = s.get(0, "event_type") {
        out.push_str(&format!("\nTYPE {}", q(t)));
        if let Some(c) = s.get(0, "condition") {
            out.push_str(&format!("\nWHERE {c}"));
        }
    }
    if !yes(s.get(0, "enabled")) {
        out.push_str("\nDISABLE");
    }
    if let Some(l) = location(s.get(0, "location")) {
        out.push_str(&format!("\nAT {l}"));
    }
    if let Some(a) = s.get(0, "action") {
        out.push_str(&format!("\nHANDLER\n{a}"));
    }
    out.push_str("\ngo\n");
    if let Some(r) = s.get(0, "remarks") {
        out.push_str(&format!(
            "COMMENT ON EVENT {} IS {}\ngo\n",
            q(name),
            sqla_lit(r)
        ));
    }
    Ok(out)
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{LIST, ONE, SCHEDULES};
}
