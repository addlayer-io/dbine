//! Db2 for i: the job scheduler's entries (what WRKJOBSCDE shows), from
//! `QSYS2.SCHEDULED_JOB_INFO` (IBM i 7.1 TR10 / 7.2 TR2 and later). The
//! view's columns are read by name, with their short names as fallback,
//! so a release that lacks one leaves that field empty instead of failing.
//! Holding and releasing an entry are `HLDJOBSCDE` / `RLSJOBSCDE` through
//! `QSYS2.QCMDEXC`, built only from a validated name and entry number.

use super::*;
use dbine_driver::{JobStep, ServerJob};

pub const NO_RUN_NOW: &str = "el planificador de trabajos de IBM i no envía una entrada fuera de su calendario desde SQL (es la opción 10 de WRKJOBSCDE)";
pub const NO_HISTORY: &str = "el planificador de trabajos de IBM i no guarda el historial de sus entradas: se ve en el registro histórico (QHST)";

const KIND: &str = "WRKJOBSCDE";
pub(super) const LIST: &str = "SELECT * FROM QSYS2.SCHEDULED_JOB_INFO";
const NOW: &str = "SELECT CURRENT TIMESTAMP AS LOCAL_NOW, CURRENT TIMESTAMP - CURRENT TIMEZONE AS UTC_NOW FROM SYSIBM.SYSDUMMY1";

const NO_VIEW: &str =
    "Este sistema no tiene QSYS2.SCHEDULED_JOB_INFO (llegó con IBM i 7.2 TR2 y 7.1 TR10).";
const NO_READ: &str = "Este usuario no puede leer el planificador de trabajos (necesita *USE sobre QUSRSYS/QDFTJOBSCD).";
const PARTIAL: &str = "Sin la autorización especial *JOBCTL no se ven todos los datos de las entradas de otros usuarios.";

const ENTRY: &[&str] = &["SCHEDULED_JOB_ENTRY_NUMBER", "ENTRY_NUMBER", "ENTRYNO"];
const NAME: &[&str] = &["SCHEDULED_JOB_NAME", "SCDJOBNAME", "JOB_NAME"];
const STATUS: &[&str] = &["STATUS"];
const DESCRIPTION: &[&str] = &["DESCRIPTION", "TEXT"];
const SCHEDULED_BY: &[&str] = &["SCHEDULED_BY", "SCDBY"];
const FREQUENCY: &[&str] = &["FREQUENCY", "FRQ"];
const DATE_VALUE: &[&str] = &["SCHEDULED_DATE_VALUE", "SCDDATEV"];
const DATE: &[&str] = &["SCHEDULED_DATE", "SCDDATE"];
const TIME: &[&str] = &["SCHEDULED_TIME", "SCDTIME"];
const DAYS: &[&str] = &["SCHEDULED_DAYS", "SCDDAYS"];
const REL_DAYS: &[&str] = &["RELATIVE_DAYS_OF_MONTH", "RELDAYMON"];
const NEXT: &[&str] = &["NEXT_SUBMISSION_DATE", "NXTSUBDATE"];
const OMITTED: &[&str] = &["DATES_OMITTED", "OMITDATES", "OMITDATE"];
const RECOVERY: &[&str] = &["RECOVERY_ACTION", "RCYACN"];
const JOBQ: &[&str] = &["JOB_QUEUE_NAME", "JOBQ"];
const JOBQ_LIB: &[&str] = &["JOB_QUEUE_LIBRARY_NAME", "JOBQLIB"];
const JOBD: &[&str] = &["JOB_DESCRIPTION_NAME", "JOBD"];
const JOBD_LIB: &[&str] = &["JOB_DESCRIPTION_LIBRARY_NAME", "JOBDLIB"];
const USER: &[&str] = &["USER_PROFILE_FOR_SUBMITTED_JOB", "USER_PROFILE", "SBMUSER"];
const MSGQ: &[&str] = &["MESSAGE_QUEUE_NAME", "MSGQ"];
const MSGQ_LIB: &[&str] = &["MESSAGE_QUEUE_LIBRARY_NAME", "MSGQLIB"];
const KEEP: &[&str] = &["KEEP_ENTRY", "SAVE"];
const COMMAND: &[&str] = &["COMMAND_STRING", "COMMAND"];
/// The last attempt first: its status is the one the list shows.
const LAST_SUBMIT: &[&str] = &[
    "LAST_ATTEMPTED_SUBMISSION_TIMESTAMP",
    "LAST_SUCCESSFUL_SUBMISSION_TIMESTAMP",
    "SBMTIMSTMP",
];
const LAST_STATUS: &[&str] = &["LAST_ATTEMPTED_SUBMISSION_STATUS", "SBMJOBSTS"];
const LAST_JOB: &[&str] = &[
    "LAST_SUBMITTED_JOB_NAME",
    "LAST_SUCCESSFUL_SUBMISSION_JOB_NAME",
];

/// An IBM i object name: up to ten of A–Z, 0–9, `$ # @ _ .`, not starting
/// with a digit (upper-cased).
fn name_ok(s: &str) -> bool {
    (1..=10).contains(&s.len())
        && !s.as_bytes()[0].is_ascii_digit()
        && s.bytes().all(|b| {
            b.is_ascii_uppercase()
                || b.is_ascii_digit()
                || matches!(b, b'$' | b'#' | b'@' | b'_' | b'.')
        })
}

/// `000123/NAME` → the six-digit entry number and the job name.
fn parse_id(id: &str) -> Result<(String, String)> {
    let t = id.trim().to_ascii_uppercase();
    let (n, name) = t.split_once('/').ok_or_else(|| bad_id(id))?;
    let ok = (1..=6).contains(&n.len()) && n.bytes().all(|b| b.is_ascii_digit()) && name_ok(name);
    if !ok {
        return Err(bad_id(id));
    }
    let n: u32 = n.parse().map_err(|_| bad_id(id))?;
    Ok((format!("{n:06}"), name.to_string()))
}

fn row_id(s: &Set, r: usize) -> Option<(String, String)> {
    let n = int(get(s, r, ENTRY))?;
    let name = get(s, r, NAME)?.to_ascii_uppercase();
    (n >= 0 && name_ok(&name)).then(|| (format!("{n:06}"), name))
}

fn status(s: &Set, r: usize) -> String {
    get(s, r, STATUS).unwrap_or_default().to_ascii_uppercase()
}

/// A special value (`*MON`, `*CURRENT`…) with its asterisk.
fn special(v: &str) -> Option<String> {
    let t = v.trim().trim_start_matches('*').to_ascii_uppercase();
    (!t.is_empty() && t.bytes().all(|b| b.is_ascii_alphanumeric())).then(|| format!("*{t}"))
}

/// The tokens of a list column (`*MON *WED`, `1 3`, `*LAST`…), each one
/// kept only if it's a special value or a number.
fn tokens(v: Option<&str>) -> Vec<String> {
    v.unwrap_or_default()
        .split([' ', ','])
        .filter(|t| !t.is_empty())
        .filter_map(|t| {
            if t.bytes().all(|b| b.is_ascii_digit()) {
                Some(t.to_string())
            } else {
                special(t)
            }
        })
        .collect()
}

/// `HHMMSS` from the view's time.
fn hhmmss(v: Option<&str>) -> Option<String> {
    let ms = parse_time(v?)?;
    let s = ms / 1000;
    Some(format!("{:02}{:02}{:02}", s / 3600, s / 60 % 60, s % 60))
}

/// The entry's schedule as ADDJOBSCDE's parameters.
fn schedule(s: &Set, r: usize) -> Option<String> {
    let mut parts = Vec::new();
    if let Some(f) = get(s, r, FREQUENCY).and_then(special) {
        parts.push(format!("FRQ({f})"));
    }
    match get(s, r, DATE_VALUE).and_then(special) {
        Some(d) if d != "*SCHEDULE_DAYS" && d != "*SCDDAY" => parts.push(format!("SCDDATE({d})")),
        _ => {
            if let Some(d) = get(s, r, DATE).filter(|d| parse_ts(d).is_some()) {
                parts.push(format!("SCDDATE({d})"));
            }
        }
    }
    let days = tokens(get(s, r, DAYS));
    if !days.is_empty() {
        parts.push(format!("SCDDAY({})", days.join(" ")));
    }
    if let Some(t) = hhmmss(get(s, r, TIME)) {
        parts.push(format!("SCDTIME({t})"));
    }
    let rel = tokens(get(s, r, REL_DAYS));
    if !rel.is_empty() && rel != ["*NONE"] {
        parts.push(format!("RELDAYMON({})", rel.join(" ")));
    }
    (!parts.is_empty()).then(|| parts.join(" "))
}

/// LAST_ATTEMPTED_SUBMISSION_STATUS: `JOB SUCCESSFULLY SUBMITTED`, a
/// failed submission, or `JOB NOT SUBMITTED DUE TO HELD STATUS` (the
/// scheduler skipped the held entry).
pub(super) fn submission(status: &str) -> Option<JobOutcome> {
    let v = status.trim().to_ascii_uppercase();
    if v.contains("HELD") {
        Some(JobOutcome::Skipped)
    } else if v.contains("FAIL") {
        Some(JobOutcome::Failed)
    } else if v.contains("SUCCESS") {
        Some(JobOutcome::Succeeded)
    } else {
        None
    }
}

fn job(s: &Set, r: usize, tz: &Tz) -> Option<ServerJob> {
    let (n, name) = row_id(s, r)?;
    let st = status(s, r);
    let enabled = st == "SCHEDULED";
    let next = get(s, r, NEXT)
        .filter(|_| enabled)
        .map(|d| format!("{} {}", d, get(s, r, TIME).unwrap_or("00:00:00")));
    Some(ServerJob {
        id: format!("{n}/{name}"),
        name,
        kind: KIND.into(),
        group: None,
        description: owned(get(s, r, DESCRIPTION)),
        owner: owned(get(s, r, SCHEDULED_BY)),
        enabled,
        schedule: schedule(s, r),
        running: false,
        last_outcome: get(s, r, LAST_STATUS).and_then(submission),
        last_started: tz.utc(get(s, r, LAST_SUBMIT)),
        last_duration_ms: None,
        next_run: tz.utc(next.as_deref()),
        can_run: false,
        can_stop: false,
        can_enable: matches!(st.as_str(), "SCHEDULED" | "HELD"),
        has_history: false,
        has_script: true,
        system: false,
    })
}

/// The view's rows, or the note saying why there are none.
fn read(src: &mut dyn JobSource) -> Result<std::result::Result<Set, &'static str>> {
    match src.query(LIST, &[]) {
        Ok(s) => Ok(Ok(s)),
        Err(e) => match code(&e) {
            Some(-204) => Ok(Err(NO_VIEW)),
            Some(-551 | -552 | -443) => Ok(Err(NO_READ)),
            _ => Err(e),
        },
    }
}

pub fn list(src: &mut dyn JobSource) -> Result<JobList> {
    let tz = Tz::query(src, NOW);
    let s = match read(src)? {
        Ok(s) => s,
        Err(note) => {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![note.into()],
            })
        }
    };
    let mut jobs: Vec<ServerJob> = (0..s.rows.len()).filter_map(|r| job(&s, r, &tz)).collect();
    jobs.sort_by(|a, b| (&a.name, &a.id).cmp(&(&b.name, &b.id)));
    let partial = (0..s.rows.len()).any(|r| get(&s, r, COMMAND).is_none());
    Ok(JobList {
        jobs,
        notes: if partial {
            vec![PARTIAL.into()]
        } else {
            Vec::new()
        },
    })
}

/// The view's row of entry `id`.
fn find(src: &mut dyn JobSource, id: &str) -> Result<(String, String, Set)> {
    let (n, name) = parse_id(id)?;
    let s = read(src)?.map_err(|note| Error::Query(note.into()))?;
    let r = (0..s.rows.len())
        .find(|r| row_id(&s, *r).as_ref() == Some(&(n.clone(), name.clone())))
        .ok_or_else(|| not_found(id))?;
    let one = Set {
        cols: s.cols.clone(),
        rows: vec![s.rows[r].clone()],
    };
    Ok((n, name, one))
}

fn qualified(s: &Set, lib: &[&str], obj: &[&str]) -> Option<String> {
    let o = get(s, 0, obj)?;
    Some(match get(s, 0, lib) {
        Some(l) => format!("{l}/{o}"),
        None => o.to_string(),
    })
}

pub fn detail(src: &mut dyn JobSource, id: &str) -> Result<JobDetail> {
    let tz = Tz::query(src, NOW);
    let (_, name, s) = find(src, id)?;
    let job = job(&s, 0, &tz).ok_or_else(|| not_found(id))?;
    let steps = get(&s, 0, COMMAND)
        .map(|c| JobStep {
            name: name.clone(),
            kind: Some("CL".into()),
            target: None,
            command: Some(c.to_string()),
        })
        .into_iter()
        .collect();
    let jobq = qualified(&s, JOBQ_LIB, JOBQ);
    let jobd = qualified(&s, JOBD_LIB, JOBD);
    let msgq = qualified(&s, MSGQ_LIB, MSGQ);
    let properties = props(vec![
        prop("", "Estado", get(&s, 0, STATUS)),
        prop("", "Frecuencia", get(&s, 0, FREQUENCY)),
        prop("", "Fecha", get(&s, 0, DATE_VALUE).or(get(&s, 0, DATE))),
        prop("", "Hora", get(&s, 0, TIME)),
        prop("", "Días", get(&s, 0, DAYS)),
        prop("", "Días relativos del mes", get(&s, 0, REL_DAYS)),
        prop("", "Próximo envío", get(&s, 0, NEXT)),
        prop("", "Fechas omitidas", get(&s, 0, OMITTED)),
        prop("", "Acción de recuperación", get(&s, 0, RECOVERY)),
        prop("", "Cola de trabajos", jobq.as_deref()),
        prop("", "Descripción de trabajo", jobd.as_deref()),
        prop("", "Usuario del trabajo", get(&s, 0, USER)),
        prop("", "Cola de mensajes", msgq.as_deref()),
        prop("", "Conservar la entrada", get(&s, 0, KEEP)),
        prop("", "Último envío", get(&s, 0, LAST_SUBMIT)),
        prop("", "Estado del último envío", get(&s, 0, LAST_STATUS)),
        prop(
            "",
            "Último envío correcto",
            get(&s, 0, &["LAST_SUCCESSFUL_SUBMISSION_TIMESTAMP"]),
        ),
        prop("", "Último trabajo enviado", get(&s, 0, LAST_JOB)),
    ]);
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

fn qcmdexc(cl: &str) -> String {
    format!("CALL QSYS2.QCMDEXC({})", lit(cl))
}

pub fn set_enabled(src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    let (n, name) = parse_id(id)?;
    let verb = if enabled { "RLSJOBSCDE" } else { "HLDJOBSCDE" };
    src.exec(&qcmdexc(&format!("{verb} JOB({name}) ENTRYNBR({n})")), &[])
        .map(|_| ())
}

/// A CL quoted string.
fn cl_text(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `LIB/OBJ` or a special value, when every part has the shape of a name.
fn cl_object(s: &Set, lib: &[&str], obj: &[&str]) -> Option<String> {
    let part = |v: &str| {
        let v = v.trim().to_ascii_uppercase();
        let ok = (1..=10).contains(&v.len())
            && v.bytes().all(|b| {
                b.is_ascii_uppercase()
                    || b.is_ascii_digit()
                    || matches!(b, b'$' | b'#' | b'@' | b'_' | b'.' | b'*')
            });
        ok.then_some(v)
    };
    let o = part(get(s, 0, obj)?)?;
    Some(match get(s, 0, lib).and_then(part) {
        Some(l) => format!("{l}/{o}"),
        None => o,
    })
}

/// `YYMMDD`, for a job whose date format is set to *YMD first.
fn ymd(v: &str) -> Option<String> {
    let iso = rfc3339(parse_ts(v)?);
    Some(format!("{}{}{}", &iso[2..4], &iso[5..7], &iso[8..10]))
}

pub fn script(src: &mut dyn JobSource, id: &str) -> Result<String> {
    let (_, name, s) = find(src, id)?;
    let g = |c: &[&str]| get(&s, 0, c);
    let mut need_ymd = false;
    let mut cl = vec![format!("ADDJOBSCDE JOB({name})")];
    if let Some(c) = g(COMMAND) {
        cl.push(format!("CMD({c})"));
    }
    if let Some(f) = g(FREQUENCY).and_then(special) {
        cl.push(format!("FRQ({f})"));
    }
    match g(DATE_VALUE).and_then(special) {
        Some(d) if matches!(d.as_str(), "*CURRENT" | "*MONTHSTR" | "*MONTHEND" | "*NONE") => {
            cl.push(format!("SCDDATE({d})"))
        }
        _ => {
            if let Some(d) = g(DATE).and_then(ymd) {
                need_ymd = true;
                cl.push(format!("SCDDATE({d})"));
            }
        }
    }
    let days = tokens(g(DAYS));
    if !days.is_empty() {
        cl.push(format!("SCDDAY({})", days.join(" ")));
    }
    if let Some(t) = hhmmss(g(TIME)) {
        cl.push(format!("SCDTIME({t})"));
    }
    let rel = tokens(g(REL_DAYS));
    if !rel.is_empty() && rel != ["*NONE"] {
        cl.push(format!("RELDAYMON({})", rel.join(" ")));
    }
    if let Some(k) = g(KEEP).and_then(special) {
        cl.push(format!("SAVE({k})"));
    }
    let omit: Vec<String> = g(OMITTED)
        .unwrap_or_default()
        .split([' ', ','])
        .filter_map(ymd)
        .collect();
    if !omit.is_empty() {
        need_ymd = true;
        cl.push(format!("OMITDATE({})", omit.join(" ")));
    }
    if let Some(a) = g(RECOVERY).and_then(special) {
        cl.push(format!("RCYACN({a})"));
    }
    for (key, lib, obj) in [
        ("JOBD", JOBD_LIB, JOBD),
        ("JOBQ", JOBQ_LIB, JOBQ),
        ("MSGQ", MSGQ_LIB, MSGQ),
    ] {
        if let Some(o) = cl_object(&s, lib, obj) {
            cl.push(format!("{key}({o})"));
        }
    }
    if let Some(u) = cl_object(&s, &[], USER) {
        cl.push(format!("USER({u})"));
    }
    if let Some(t) = g(DESCRIPTION) {
        cl.push(format!("TEXT({})", cl_text(t)));
    }
    let mut out = String::new();
    if need_ymd {
        out.push_str(&qcmdexc("CHGJOB DATFMT(*YMD)"));
        out.push_str(";\n");
    }
    out.push_str(&qcmdexc(&cl.join(" ")));
    out.push_str(";\n");
    if status(&s, 0) == "HELD" {
        out.push_str(&qcmdexc(&format!("HLDJOBSCDE JOB({name}) ENTRYNBR(*ONLY)")));
        out.push_str(";\n");
    }
    Ok(out)
}

#[cfg(test)]
pub(super) mod fixtures {
    pub(in crate::jobs) use super::{submission, LIST};
}
