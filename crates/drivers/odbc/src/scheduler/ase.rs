//! SAP ASE: Job Scheduler (`sybmgmtdb`) managed whole through its
//! procedures, the way its guide does it: scheduled jobs (a job, a
//! schedule and the server it runs on), the jobs themselves (the SQL
//! batch, stored with `sp_sjobcmd` in pieces) and the schedules, created
//! with `sp_sjobcreate` (named `sjname=`, `jname=` or `sname=`), changed
//! with `sp_sjobmodify` and removed with `sp_sjobdrop`; a scheduled job is
//! enabled and disabled with `sp_sjobcontrol`, as the tree's switch does.
//! `sp_sjobhistory … drop` clears a scheduled job's history and output.
//!
//! The procedures take their values as one `key=value,…` text: a value
//! can't hold a comma, and names begin with a letter (Job Scheduler's
//! rule), so a name made only of digits is always an id. A scheduled job
//! can't be pointed at another job or schedule: it is dropped and created
//! again, with a new id. Changes run one by one (the procedures manage
//! their own work in `sybmgmtdb`).
//!
//! `sp_sjobhelp`'s list columns aren't documented: they're read by name
//! with alternatives, as the jobs list does ([`crate::jobs::ase`]). Users
//! without `js_admin_role` see and change only their own objects.

use super::*;
use crate::jobs::ase::{access, options, Access, OFF};
use crate::jobs::{get, int, int_id, lit, not_found, parse_time, parse_ts};
use dbine_driver::{FormField, FormKind, FormSection, SchedulerKind};
use serde_json::json;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Kind {
    /// A scheduled job: the "job" kind of the tree.
    Scheduled,
    /// A job: the SQL a scheduled job runs.
    Job,
    Schedule,
}

pub(super) fn kind(kind: &str) -> Result<Kind> {
    match kind.trim() {
        "job" => Ok(Kind::Scheduled),
        "jobdef" => Ok(Kind::Job),
        "schedule" => Ok(Kind::Schedule),
        other => Err(bad_kind(other)),
    }
}

const SJ_ID: &[&str] = &["sjob_id", "id"];
const SJ_NAME: &[&str] = &["sjob_name", "name", "sjname"];
const SJ_OWNER: &[&str] = &["sjob_owner", "owner"];
const SJ_STATE: &[&str] = &["sjob_state", "state", "sjob_enabled", "enabled"];
const SJ_JOB: &[&str] = &["job_name", "sjob_jobname", "jname"];
const SJ_SCHED: &[&str] = &["sched_name", "sjob_schedname", "sname"];
const SJ_SERVER: &[&str] = &["sjob_server", "server"];
const SJ_TIMEOUT: &[&str] = &["sjob_timeout", "timeout"];
const SJ_PROPS: &[&str] = &["sjob_properties", "sjproperties", "properties"];
const SJ_LOCALE: &[&str] = &["sjob_locale", "locale"];

const J_ID: &[&str] = &["job_id", "id", "jid"];
const J_NAME: &[&str] = &["job_name", "name", "jname"];
const J_DESC: &[&str] = &["job_desc", "jdesc", "job_description", "description"];
const J_OWNER: &[&str] = &["job_owner", "owner"];
const J_PROPS: &[&str] = &["job_properties", "jproperties", "properties"];
const J_TIMEOUT: &[&str] = &[
    "job_default_timeout",
    "default_timeout",
    "job_timeout",
    "timeout",
];

const S_ID: &[&str] = &["sched_id", "id", "sid"];
const S_NAME: &[&str] = &["sched_name", "name", "sname"];
const S_DESC: &[&str] = &["sched_desc", "sdesc", "sched_description", "description"];
const S_OWNER: &[&str] = &["sched_owner", "owner"];
const S_PROPS: &[&str] = &["sched_properties", "sproperties", "properties"];
const S_REPEATS: &[&str] = &["sched_repeats", "repeats", "sched_interval", "interval"];
const S_UNITS: &[&str] = &["sched_units", "sched_interval_units", "units"];
const S_START_DATE: &[&str] = &["sched_startdate", "startdate"];
const S_START_TIME: &[&str] = &["sched_starttime", "starttime"];
const S_END_DATE: &[&str] = &["sched_enddate", "enddate"];
const S_END_TIME: &[&str] = &["sched_endtime", "endtime"];
const S_DAYS: &[&str] = &["sched_days", "days"];
const S_DATES: &[&str] = &["sched_dates", "dates"];

const DAYS: [(&str, &str); 7] = [
    ("Monday", "Lunes"),
    ("Tuesday", "Martes"),
    ("Wednesday", "Miércoles"),
    ("Thursday", "Jueves"),
    ("Friday", "Viernes"),
    ("Saturday", "Sábado"),
    ("Sunday", "Domingo"),
];
const UNITS: &[&str] = &["minutes", "hours", "days"];

const OWN_EDIT: &str = "Sin js_admin_role se cambian y borran solo los objetos propios: los jobs y calendarios compartidos de otros usuarios se usan, pero no se cambian.";
const NOT_YOURS: &str = "Es de otro usuario: lo cambia solo su dueño o quien tenga js_admin_role.";
const RELINK: &str = "Job Scheduler no cambia el job ni el calendario de un job programado: se borra y se crea de nuevo; recibe otro id y su historial queda con el id anterior.";
const UNREADABLE: &str = "sp_sjobhelp no devolvió la lista en un formato que DBine reconozca.";
const OWNER_ADMIN: &str = "Cambiar el dueño pide js_admin_role.";
/// The pieces `sp_sjobcmd … add` gets the SQL in (Job Scheduler joins
/// them when the job runs).
const CHUNK: usize = 255;

// ------------------------------------------------------------------ reading

fn help(src: &mut dyn JobSource, a: &Access, name: Option<&str>, what: &str) -> Result<Set> {
    let name = name
        .map(|n| format!("@name = {}, ", lit(n)))
        .unwrap_or_default();
    src.query(
        &format!(
            "exec sybmgmtdb..sp_sjobhelp {name}@option = {}",
            lit(&options(what, a.admin))
        ),
        &[],
    )
}

/// The login, to tell its own objects from others'.
fn me(src: &mut dyn JobSource) -> Option<String> {
    src.query("SELECT suser_name() AS ME", &[])
        .ok()
        .and_then(|s| s.text("ME"))
}

fn access_or(src: &mut dyn JobSource) -> Result<Access> {
    access(src)?.map_err(|note| Error::Unsupported(note.into()))
}

fn text(s: &Set, r: usize, cols: &[&str]) -> Option<String> {
    get(s, r, cols).map(str::to_string)
}

fn enabled(v: Option<&str>) -> bool {
    match v.map(|v| v.trim().to_ascii_lowercase()) {
        Some(v) => v.starts_with("enabl") || matches!(v.as_str(), "1" | "y" | "yes" | "true"),
        None => true,
    }
}

/// A time of day in seconds: `23:00`, `11:00PM`, or a date and time
/// (`1900-01-01 23:00:00`).
pub(super) fn day_time(v: Option<&str>) -> Option<i64> {
    let v = v?.trim();
    if let Some(ms) = parse_ts(v).filter(|_| v.len() > 10) {
        return Some(ms.rem_euclid(86_400_000) / 1000);
    }
    let mut secs = parse_time(v)? / 1000;
    let up = v.to_ascii_uppercase();
    if up.ends_with("PM") && secs < 12 * 3600 {
        secs += 12 * 3600;
    } else if up.ends_with("AM") && secs >= 12 * 3600 {
        secs -= 12 * 3600;
    }
    Some(secs)
}

/// The days of a schedule (`Monday:Wednesday`, abbreviations too).
pub(super) fn parse_days(v: Option<&str>) -> Vec<String> {
    let Some(v) = v else {
        return Vec::new();
    };
    let mut out: Vec<String> = Vec::new();
    for w in v.split(|c: char| !c.is_alphabetic()) {
        // By characters: a word may hold letters of any script, and its
        // third byte may fall inside one.
        let head: String = w.chars().take(3).collect::<String>().to_ascii_lowercase();
        if head.chars().count() < 3 {
            continue;
        }
        if let Some((d, _)) = DAYS
            .iter()
            .find(|(d, _)| d.to_ascii_lowercase().starts_with(&head))
        {
            if !out.iter().any(|x| x == d) {
                out.push(d.to_string());
            }
        }
    }
    out
}

/// The dates of a month (1–31, 32 the last day).
pub(super) fn parse_dates(v: Option<&str>) -> Vec<i64> {
    let mut out: Vec<i64> = v
        .unwrap_or_default()
        .split(|c: char| !c.is_ascii_digit())
        .filter_map(|p| p.parse::<i64>().ok())
        .filter(|d| (1..=32).contains(d))
        .collect();
    out.sort_unstable();
    out.dedup();
    out
}

/// `repeats`: the number and the unit, from `2hours` or from the number and
/// the units column.
pub(super) fn parse_repeats(v: Option<&str>, units: Option<&str>) -> (i64, String) {
    let v = v.unwrap_or_default().trim();
    let digits: String = v.chars().take_while(char::is_ascii_digit).collect();
    let n = digits.parse::<i64>().unwrap_or(0);
    let rest = v[digits.len()..].trim();
    let unit = if rest.is_empty() {
        units.unwrap_or_default()
    } else {
        rest
    };
    let unit = match unit.trim().to_ascii_lowercase().chars().next() {
        Some('d') => "days",
        Some('h') => "hours",
        _ => "minutes",
    };
    (n, unit.to_string())
}

// ------------------------------------------------------------------ models

/// A scheduled job: a job, a schedule, where it runs and how.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Scheduled {
    pub(super) name: String,
    pub(super) enabled: bool,
    /// The job's and the schedule's names.
    pub(super) job: String,
    pub(super) schedule: String,
    pub(super) server: Option<String>,
    pub(super) owner: Option<String>,
    /// Minutes.
    pub(super) timeout: Option<i64>,
    pub(super) locale: Option<String>,
    pub(super) properties: Option<String>,
}

/// A job: the SQL and its defaults.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct JobDef {
    pub(super) name: String,
    pub(super) description: Option<String>,
    pub(super) properties: Option<String>,
    /// Minutes.
    pub(super) timeout: Option<i64>,
    pub(super) owner: Option<String>,
    pub(super) command: String,
}

/// A schedule. Times: seconds after midnight; dates: ms at midnight.
#[derive(Debug, Clone, PartialEq, Default)]
pub(super) struct Schedule {
    pub(super) name: String,
    pub(super) description: Option<String>,
    pub(super) properties: Option<String>,
    pub(super) owner: Option<String>,
    /// 0: it doesn't repeat.
    pub(super) repeats: i64,
    /// "minutes", "hours" or "days".
    pub(super) unit: String,
    pub(super) start_date: Option<i64>,
    pub(super) start_time: Option<i64>,
    pub(super) end_date: Option<i64>,
    pub(super) end_time: Option<i64>,
    /// Day names (`Monday`…) or dates (1–31, 32 the last day): one or the
    /// other.
    pub(super) days: Vec<String>,
    pub(super) dates: Vec<i64>,
}

impl Schedule {
    fn on(&self) -> &'static str {
        if !self.days.is_empty() {
            "days"
        } else if !self.dates.is_empty() {
            "dates"
        } else {
            "all"
        }
    }

    /// In Spanish, for the tree.
    fn summary(&self) -> String {
        let mut out = if self.repeats > 0 {
            let unit = match (self.unit.as_str(), self.repeats == 1) {
                ("days", true) => "día",
                ("days", false) => "días",
                ("hours", true) => "hora",
                ("hours", false) => "horas",
                (_, true) => "minuto",
                (_, false) => "minutos",
            };
            format!("Cada {} {unit}", self.repeats)
        } else {
            "Una vez".to_string()
        };
        if let Some(t) = self.start_time {
            out.push_str(&format!(", desde las {}", &time_text(t)[..5]));
        }
        if let Some(t) = self.end_time {
            out.push_str(&format!(" hasta las {}", &time_text(t)[..5]));
        }
        if !self.days.is_empty() {
            let names: Vec<&str> = self
                .days
                .iter()
                .filter_map(|d| DAYS.iter().find(|(e, _)| e == d).map(|(_, s)| *s))
                .collect();
            out.push_str(&format!(" ({})", names.join(", ")));
        } else if !self.dates.is_empty() {
            let d: Vec<String> = self.dates.iter().map(i64::to_string).collect();
            out.push_str(&format!(" (días {})", d.join(", ")));
        }
        out
    }
}

fn scheduled_of(s: &Set, r: usize) -> Scheduled {
    Scheduled {
        name: text(s, r, SJ_NAME).unwrap_or_default(),
        enabled: enabled(get(s, r, SJ_STATE)),
        job: text(s, r, SJ_JOB).unwrap_or_default(),
        schedule: text(s, r, SJ_SCHED).unwrap_or_default(),
        server: text(s, r, SJ_SERVER),
        owner: text(s, r, SJ_OWNER),
        timeout: int(get(s, r, SJ_TIMEOUT)).filter(|n| *n > 0),
        locale: text(s, r, SJ_LOCALE),
        properties: text(s, r, SJ_PROPS),
    }
}

fn job_of(s: &Set, r: usize) -> JobDef {
    JobDef {
        name: text(s, r, J_NAME).unwrap_or_default(),
        description: text(s, r, J_DESC),
        properties: text(s, r, J_PROPS),
        timeout: int(get(s, r, J_TIMEOUT)).filter(|n| *n > 0),
        owner: text(s, r, J_OWNER),
        command: String::new(),
    }
}

fn schedule_of(s: &Set, r: usize) -> Schedule {
    let (repeats, unit) = parse_repeats(get(s, r, S_REPEATS), get(s, r, S_UNITS));
    let days = parse_days(get(s, r, S_DAYS));
    Schedule {
        name: text(s, r, S_NAME).unwrap_or_default(),
        description: text(s, r, S_DESC),
        properties: text(s, r, S_PROPS),
        owner: text(s, r, S_OWNER),
        repeats,
        unit,
        start_date: get(s, r, S_START_DATE).and_then(parse_date),
        start_time: day_time(get(s, r, S_START_TIME)),
        end_date: get(s, r, S_END_DATE).and_then(parse_date),
        end_time: day_time(get(s, r, S_END_TIME)),
        dates: if days.is_empty() {
            parse_dates(get(s, r, S_DATES))
        } else {
            Vec::new()
        },
        days,
    }
}

/// The row of `set` whose id column is `n`.
fn row_of(set: &Set, cols: &[&str], n: u64) -> Option<usize> {
    (0..set.rows.len()).find(|r| int(get(set, *r, cols)) == Some(n as i64))
}

fn scheduled_read(src: &mut dyn JobSource, a: &Access, id: &str) -> Result<(u64, Scheduled)> {
    let n = int_id(id)?;
    let s = help(src, a, Some(&n.to_string()), "list")?;
    let r = row_of(&s, SJ_ID, n).ok_or_else(|| not_found(id))?;
    Ok((n, scheduled_of(&s, r)))
}

/// The SQL of job `n` (`jname=<id>`), as `sp_sjobcmd` lists it: the last
/// column of each row, joined as stored.
fn command(src: &mut dyn JobSource, n: u64) -> String {
    match src.query(
        &format!("exec sybmgmtdb..sp_sjobcmd @name = 'jname={n}', @option = 'list'"),
        &[],
    ) {
        Ok(c) => {
            let last = c.cols.len().saturating_sub(1);
            c.rows
                .iter()
                .filter_map(|row| row.get(last).cloned().flatten())
                .collect::<String>()
                .trim_end()
                .to_string()
        }
        Err(e) => {
            tracing::debug!("odbc scheduler: sp_sjobcmd failed: {e}");
            String::new()
        }
    }
}

fn job_read(src: &mut dyn JobSource, a: &Access, id: &str) -> Result<(u64, JobDef)> {
    let n = int_id(id)?;
    let s = help(src, a, None, "list,jobs")?;
    let r = row_of(&s, J_ID, n).ok_or_else(|| not_found(id))?;
    let mut j = job_of(&s, r);
    j.command = command(src, n);
    Ok((n, j))
}

fn schedule_read(src: &mut dyn JobSource, a: &Access, id: &str) -> Result<(u64, Schedule)> {
    let n = int_id(id)?;
    let s = help(src, a, None, "list,schedules")?;
    let r = row_of(&s, S_ID, n).ok_or_else(|| not_found(id))?;
    Ok((n, schedule_of(&s, r)))
}

/// The names of the scheduled jobs that use job or schedule `name`.
fn users_of(src: &mut dyn JobSource, a: &Access, k: Kind, name: &str) -> Vec<String> {
    let cols = if k == Kind::Job { SJ_JOB } else { SJ_SCHED };
    match help(src, a, None, "list,scheduled") {
        Ok(s) => (0..s.rows.len())
            .filter(|r| get(&s, *r, cols) == Some(name))
            .filter_map(|r| text(&s, r, SJ_NAME))
            .collect(),
        Err(e) => {
            tracing::debug!("odbc scheduler: sp_sjobhelp failed: {e}");
            Vec::new()
        }
    }
}

// ------------------------------------------------------------------ the contract

pub(super) fn info(src: &mut dyn JobSource) -> Result<SchedulerInfo> {
    let a = match access(src)? {
        Ok(a) => a,
        Err(note) => {
            return Ok(SchedulerInfo {
                kinds: Vec::new(),
                logs: false,
                notes: vec![note.to_string()],
            })
        }
    };
    let mut notes = Vec::new();
    if a.off {
        notes.push(OFF.to_string());
    }
    if !a.admin {
        notes.push(OWN_EDIT.to_string());
    }
    Ok(SchedulerInfo {
        kinds: vec![
            SchedulerKind::new("job", "Jobs programados", "job programado")
                .abilities(true, true, true),
            SchedulerKind::new("jobdef", "Jobs", "job").abilities(true, true, true),
            SchedulerKind::new("schedule", "Calendarios", "calendario").abilities(true, true, true),
        ],
        logs: false,
        notes,
    })
}

pub(super) fn items(src: &mut dyn JobSource, k: Kind) -> Result<SchedulerList> {
    let a = match access(src)? {
        Ok(a) => a,
        Err(note) => {
            return Ok(SchedulerList {
                items: Vec::new(),
                notes: vec![note.to_string()],
            })
        }
    };
    let (what, id_cols) = match k {
        Kind::Job => ("list,jobs", J_ID),
        _ => ("list,schedules", S_ID),
    };
    let s = help(src, &a, None, what)?;
    let mut items: Vec<SchedulerItem> = (0..s.rows.len())
        .filter_map(|r| {
            let id = int(get(&s, r, id_cols))?.to_string();
            Some(match k {
                Kind::Job => {
                    let j = job_of(&s, r);
                    SchedulerItem {
                        id,
                        name: j.name,
                        description: j.description,
                        enabled: None,
                        system: false,
                        can_edit: None,
                        can_delete: None,
                        actions: None,
                    }
                }
                _ => {
                    let sc = schedule_of(&s, r);
                    SchedulerItem {
                        id,
                        description: Some(sc.summary()),
                        name: sc.name,
                        enabled: None,
                        system: false,
                        can_edit: None,
                        can_delete: None,
                        actions: None,
                    }
                }
            })
        })
        .collect();
    items.sort_by(|x, y| x.name.cmp(&y.name));
    items.dedup_by(|x, y| x.id == y.id);
    let mut notes = Vec::new();
    if items.is_empty() && !s.rows.is_empty() {
        tracing::debug!("odbc scheduler: sp_sjobhelp columns: {:?}", s.cols);
        notes.push(UNREADABLE.to_string());
    }
    if !a.admin {
        notes.push(OWN_EDIT.to_string());
    }
    Ok(SchedulerList { items, notes })
}

/// The jobs', schedules' and servers' names to pick from.
fn choices(src: &mut dyn JobSource, a: &Access) -> BTreeMap<String, Vec<(String, String)>> {
    let mut out = BTreeMap::new();
    for (key, what, cols) in [
        ("jobdef", "list,jobs", J_NAME),
        ("schedule", "list,schedules", S_NAME),
    ] {
        let names: Vec<(String, String)> = match help(src, a, None, what) {
            Ok(s) => {
                let mut n: Vec<String> = (0..s.rows.len())
                    .filter_map(|r| text(&s, r, cols))
                    .collect();
                n.sort();
                n.dedup();
                n.into_iter().map(|v| (v.clone(), v)).collect()
            }
            Err(e) => {
                tracing::debug!("odbc scheduler: sp_sjobhelp failed: {e}");
                Vec::new()
            }
        };
        out.insert(key.to_string(), names);
    }
    let servers = src
        .query(
            "SELECT srvname FROM master..sysservers ORDER BY srvname",
            &[],
        )
        .map(|s| {
            (0..s.rows.len())
                .filter_map(|r| s.get(r, "srvname"))
                .map(|v| (v.to_string(), v.to_string()))
                .collect()
        })
        .unwrap_or_default();
    out.insert("server".to_string(), servers);
    out
}

pub(super) fn scheduled_sections(existing: bool, admin: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Empieza con una letra; sin comas."),
            FormField::new("enabled", "Habilitado", FormKind::Bool).default(json!(true)),
            FormField::new("job", "Job", FormKind::reference("jobdef", false))
                .required()
                .help("El job (el SQL) que corre. Cambiarlo vuelve a crear el job programado."),
            FormField::new(
                "schedule",
                "Calendario",
                FormKind::reference("schedule", false),
            )
            .required()
            .help("Cuándo corre. Cambiarlo vuelve a crear el job programado."),
            FormField::new("server", "Servidor", FormKind::reference("server", false))
                .help("El servidor donde corre. Vacío: este servidor."),
        ]),
        FormSection::new("options", "Opciones").fields(vec![
            FormField::new(
                "timeout",
                "Tiempo máximo (minutos)",
                FormKind::number(Some(0.0), None),
            )
            .help("Cuánto puede durar una ejecución. Vacío: el del job."),
            FormField::new("locale", "Idioma de la conexión", FormKind::Text)
                .help("El idioma (locale) de la conexión con la que corre."),
            FormField::new("properties", "Propiedades", FormKind::Text)
                .help("Como las toma Job Scheduler (sjproperties), sin comas."),
            FormField::new("owner", "Dueño", FormKind::Text)
                .read_only(!admin)
                .help(OWNER_ADMIN),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id", FormKind::Text).read_only(true),
            FormField::new("state", "Estado", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn job_sections(existing: bool, admin: bool) -> Vec<FormSection> {
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Único; empieza con una letra; sin comas."),
            FormField::new("description", "Descripción", FormKind::Text).help("Sin comas."),
            FormField::new(
                "timeout",
                "Tiempo máximo (minutos)",
                FormKind::number(Some(0.0), None),
            )
            .help("Cuánto puede durar una ejecución, salvo que el job programado diga otro."),
            FormField::new("properties", "Propiedades", FormKind::Text)
                .help("Como las toma Job Scheduler (jproperties): por ejemplo shared, para que otros usuarios lo programen.")
                .placeholder("shared"),
            FormField::new("owner", "Dueño", FormKind::Text)
                .read_only(!admin)
                .help(OWNER_ADMIN),
        ]),
        FormSection::new("command", "SQL").field(
            FormField::new("command", "SQL", FormKind::code("sql"))
                .required()
                .help("El lote que corre, como en isql: puede tener varias sentencias y líneas go."),
        ),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id", FormKind::Text).read_only(true),
            FormField::new("used_by", "Lo usan", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn schedule_sections(existing: bool, admin: bool) -> Vec<FormSection> {
    let mut dates: Vec<(String, String)> =
        (1..=31).map(|d| (d.to_string(), d.to_string())).collect();
    dates.push(("32".into(), "Último día".into()));
    let mut out = vec![
        FormSection::new("general", "General").fields(vec![
            FormField::new("name", "Nombre", FormKind::Text)
                .required()
                .help("Único; empieza con una letra; sin comas."),
            FormField::new("description", "Descripción", FormKind::Text).help("Sin comas."),
            FormField::new("properties", "Propiedades", FormKind::Text)
                .help("Como las toma Job Scheduler (sproperties): por ejemplo shared.")
                .placeholder("shared"),
            FormField::new("owner", "Dueño", FormKind::Text)
                .read_only(!admin)
                .help(OWNER_ADMIN),
        ]),
        FormSection::new("timing", "Programación").fields(vec![
            FormField::new("repeats", "Cada", FormKind::number(Some(0.0), None))
                .default(json!(0))
                .help("0: no se repite."),
            FormField::new(
                "unit",
                "Unidad",
                FormKind::select(&[("minutes", "Minutos"), ("hours", "Horas"), ("days", "Días")]),
            )
            .default(json!("hours")),
            FormField::new("start_date", "Desde el día", FormKind::Date),
            FormField::new("start_time", "Desde la hora", FormKind::Time)
                .help("Vacío: medianoche."),
            FormField::new("end_date", "Hasta el día", FormKind::Date),
            FormField::new("end_time", "Hasta la hora", FormKind::Time)
                .help("Vacío: medianoche. No cuenta si se repite cada un día o más."),
            FormField::new(
                "on",
                "Qué días",
                FormKind::select(&[
                    ("all", "Todos"),
                    ("days", "Días de la semana"),
                    ("dates", "Fechas del mes"),
                ]),
            )
            .default(json!("all"))
            .help("Días o fechas, solo si se repite cada menos de un día."),
            FormField::new(
                "days",
                "Días de la semana",
                FormKind::multi(&[
                    ("Monday", "Lunes"),
                    ("Tuesday", "Martes"),
                    ("Wednesday", "Miércoles"),
                    ("Thursday", "Jueves"),
                    ("Friday", "Viernes"),
                    ("Saturday", "Sábado"),
                    ("Sunday", "Domingo"),
                ]),
            )
            .when("on", &["days"]),
            FormField::new(
                "dates",
                "Fechas del mes",
                FormKind::MultiSelect { options: dates },
            )
            .when("on", &["dates"]),
        ]),
    ];
    if existing {
        out.push(FormSection::new("state", "Estado").fields(vec![
            FormField::new("id", "Id", FormKind::Text).read_only(true),
            FormField::new("used_by", "Lo usan", FormKind::Text).read_only(true),
        ]));
    }
    out
}

pub(super) fn scheduled_values(j: &Scheduled) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(j.name)),
        ("enabled", json!(j.enabled)),
        ("job", json!(j.job)),
        ("schedule", json!(j.schedule)),
        ("server", jtext(j.server.as_deref())),
        ("timeout", j.timeout.map_or(Value::Null, |n| json!(n))),
        ("locale", jtext(j.locale.as_deref())),
        ("properties", jtext(j.properties.as_deref())),
        ("owner", jtext(j.owner.as_deref())),
    ])
}

pub(super) fn job_values(j: &JobDef) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(j.name)),
        ("description", jtext(j.description.as_deref())),
        ("timeout", j.timeout.map_or(Value::Null, |n| json!(n))),
        ("properties", jtext(j.properties.as_deref())),
        ("owner", jtext(j.owner.as_deref())),
        ("command", json!(j.command)),
    ])
}

pub(super) fn schedule_values(s: &Schedule) -> BTreeMap<String, Value> {
    values(vec![
        ("name", json!(s.name)),
        ("description", jtext(s.description.as_deref())),
        ("properties", jtext(s.properties.as_deref())),
        ("owner", jtext(s.owner.as_deref())),
        ("repeats", json!(s.repeats)),
        ("unit", json!(s.unit)),
        (
            "start_date",
            s.start_date.map_or(Value::Null, |d| json!(date_value(d))),
        ),
        (
            "start_time",
            s.start_time.map_or(Value::Null, |t| json!(time_text(t))),
        ),
        (
            "end_date",
            s.end_date.map_or(Value::Null, |d| json!(date_value(d))),
        ),
        (
            "end_time",
            s.end_time.map_or(Value::Null, |t| json!(time_text(t))),
        ),
        ("on", json!(s.on())),
        ("days", json!(s.days)),
        (
            "dates",
            json!(s.dates.iter().map(i64::to_string).collect::<Vec<_>>()),
        ),
    ])
}

/// Whether the login may change an object owned by `owner`.
fn mine(a: &Access, me: Option<&str>, owner: Option<&str>) -> bool {
    a.admin
        || match (me, owner) {
            (Some(m), Some(o)) => m.eq_ignore_ascii_case(o),
            _ => true,
        }
}

pub(super) fn form(src: &mut dyn JobSource, k: Kind, id: Option<&str>) -> Result<ObjectForm> {
    let a = access_or(src)?;
    let choices = choices(src, &a);
    let (sections, v, owner, mut notes) = match (k, id) {
        (Kind::Scheduled, None) => (
            scheduled_sections(false, a.admin),
            scheduled_values(&Scheduled {
                enabled: true,
                ..Scheduled::default()
            }),
            None,
            Vec::new(),
        ),
        (Kind::Job, None) => (
            job_sections(false, a.admin),
            job_values(&JobDef::default()),
            None,
            Vec::new(),
        ),
        (Kind::Schedule, None) => (
            schedule_sections(false, a.admin),
            schedule_values(&Schedule {
                unit: "hours".into(),
                ..Schedule::default()
            }),
            None,
            Vec::new(),
        ),
        (Kind::Scheduled, Some(id)) => {
            let (n, j) = scheduled_read(src, &a, id)?;
            let mut v = scheduled_values(&j);
            v.insert("id".into(), json!(n.to_string()));
            let s = help(src, &a, Some(&n.to_string()), "list")?;
            v.insert(
                "state".into(),
                jtext(row_of(&s, SJ_ID, n).and_then(|r| get(&s, r, SJ_STATE))),
            );
            (
                scheduled_sections(true, a.admin),
                v,
                j.owner,
                vec![RELINK.to_string()],
            )
        }
        (Kind::Job, Some(id)) => {
            let (n, j) = job_read(src, &a, id)?;
            let used = users_of(src, &a, Kind::Job, &j.name);
            let mut v = job_values(&j);
            v.insert("id".into(), json!(n.to_string()));
            v.insert("used_by".into(), json!(used.join(", ")));
            (job_sections(true, a.admin), v, j.owner, Vec::new())
        }
        (Kind::Schedule, Some(id)) => {
            let (n, s) = schedule_read(src, &a, id)?;
            let used = users_of(src, &a, Kind::Schedule, &s.name);
            let mut v = schedule_values(&s);
            v.insert("id".into(), json!(n.to_string()));
            v.insert("used_by".into(), json!(used.join(", ")));
            (schedule_sections(true, a.admin), v, s.owner, Vec::new())
        }
    };
    let me = if id.is_some() { me(src) } else { None };
    let read_only = id.is_some() && !mine(&a, me.as_deref(), owner.as_deref());
    if read_only {
        notes.insert(0, NOT_YOURS.to_string());
    }
    Ok(ObjectForm {
        sections,
        values: v,
        choices,
        notes,
        read_only,
    })
}

// ------------------------------------------------------------------ values

/// A value that goes into the procedures' `key=value,…` text: one line,
/// no commas.
fn opt_text(v: Vals, k: &str, label: &str, max: usize) -> Result<Option<String>> {
    let t = optional(v, k, label, max)?;
    if t.as_deref().is_some_and(|t| t.contains(',')) {
        return Err(bad(format!(
            "«{label}» no puede tener comas: Job Scheduler separa sus opciones con ellas."
        )));
    }
    Ok(t)
}

/// A name: required, begins with a letter, no commas or `=`.
fn name(v: Vals, label: &str) -> Result<String> {
    let n = required(v, "name", label, 255)?;
    check_name(&n, label)?;
    Ok(n)
}

fn check_name(n: &str, label: &str) -> Result<()> {
    if !n.chars().next().is_some_and(char::is_alphabetic) {
        return Err(bad(format!(
            "«{label}»: «{}» tiene que empezar con una letra.",
            short(n)
        )));
    }
    if n.contains([',', '=']) {
        return Err(bad(format!("«{label}» no puede tener comas ni «=».")));
    }
    Ok(())
}

fn timeout(v: Vals) -> Result<Option<i64>> {
    Ok(
        v.int("timeout", "Tiempo máximo (minutos)", 0, i64::from(i32::MAX))?
            .filter(|n| *n > 0),
    )
}

pub(super) fn scheduled_from(values: &FormValues) -> Result<Scheduled> {
    let v = Vals(&values.values);
    let job = required(v, "job", "Job", 255)?;
    check_name(&job, "Job")?;
    let schedule = required(v, "schedule", "Calendario", 255)?;
    check_name(&schedule, "Calendario")?;
    Ok(Scheduled {
        name: name(v, "Nombre")?,
        enabled: v.bool_or("enabled", true),
        job,
        schedule,
        server: opt_text(v, "server", "Servidor", 255)?,
        owner: opt_text(v, "owner", "Dueño", 255)?,
        timeout: timeout(v)?,
        locale: opt_text(v, "locale", "Idioma de la conexión", 64)?,
        properties: opt_text(v, "properties", "Propiedades", 255)?,
    })
}

pub(super) fn job_from(values: &FormValues) -> Result<JobDef> {
    let v = Vals(&values.values);
    let command = code_text(v, "command", "SQL", 1_000_000)?.ok_or_else(|| bad("Falta «SQL»."))?;
    Ok(JobDef {
        name: name(v, "Nombre")?,
        description: opt_text(v, "description", "Descripción", 255)?,
        properties: opt_text(v, "properties", "Propiedades", 255)?,
        timeout: timeout(v)?,
        owner: opt_text(v, "owner", "Dueño", 255)?,
        command,
    })
}

pub(super) fn schedule_from(values: &FormValues) -> Result<Schedule> {
    let v = Vals(&values.values);
    let mut s = Schedule {
        name: name(v, "Nombre")?,
        description: opt_text(v, "description", "Descripción", 255)?,
        properties: opt_text(v, "properties", "Propiedades", 255)?,
        owner: opt_text(v, "owner", "Dueño", 255)?,
        repeats: v.int("repeats", "Cada", 0, 100_000)?.unwrap_or(0),
        unit: choice(v, "unit", "Unidad", UNITS, "hours")?,
        start_date: date(v, "start_date", "Desde el día")?,
        start_time: time(v, "start_time", "Desde la hora")?,
        end_date: date(v, "end_date", "Hasta el día")?,
        end_time: time(v, "end_time", "Hasta la hora")?,
        days: Vec::new(),
        dates: Vec::new(),
    };
    for (t, label) in [
        (s.start_time, "Desde la hora"),
        (s.end_time, "Hasta la hora"),
    ] {
        if t.is_some_and(|t| t % 60 != 0) {
            return Err(bad(format!(
                "«{label}»: Job Scheduler toma horas y minutos, sin segundos."
            )));
        }
    }
    match choice(v, "on", "Qué días", &["all", "days", "dates"], "all")?.as_str() {
        "days" => {
            for d in v.strings("days") {
                let day = DAYS
                    .iter()
                    .find(|(e, _)| e.eq_ignore_ascii_case(&d))
                    .ok_or_else(|| bad(format!("«{}» no es un día de la semana.", short(&d))))?;
                if !s.days.iter().any(|x| x == day.0) {
                    s.days.push(day.0.to_string());
                }
            }
            if s.days.is_empty() {
                return Err(bad("Elegí al menos un día de la semana."));
            }
        }
        "dates" => {
            for d in v.strings("dates") {
                let n = d
                    .parse::<i64>()
                    .ok()
                    .filter(|n| (1..=32).contains(n))
                    .ok_or_else(|| bad(format!("«{}» no es una fecha del mes.", short(&d))))?;
                s.dates.push(n);
            }
            s.dates.sort_unstable();
            s.dates.dedup();
            if s.dates.is_empty() {
                return Err(bad("Elegí al menos una fecha del mes."));
            }
        }
        _ => {}
    }
    let a_day_or_more = s.repeats > 0 && s.unit == "days";
    if a_day_or_more && (!s.days.is_empty() || !s.dates.is_empty()) {
        return Err(bad(
            "Un calendario que se repite cada un día o más corre todos los días: no lleva días ni fechas.",
        ));
    }
    if let (Some(a), Some(b)) = (s.start_date, s.end_date) {
        if b < a {
            return Err(bad("«Hasta el día» es anterior a «Desde el día»."));
        }
        if a == b {
            if let (Some(st), Some(et)) = (s.start_time, s.end_time) {
                if et < st {
                    return Err(bad(
                        "El mismo día, «Hasta la hora» no puede ser anterior a «Desde la hora».",
                    ));
                }
            }
        }
    }
    Ok(s)
}

// ------------------------------------------------------------------ scripts

fn exec(proc_: &str, name: &str, option: Option<String>) -> String {
    let mut out = format!("exec sybmgmtdb..{proc_} @name = {}", lit(name));
    if let Some(o) = option {
        out.push_str(&format!(", @option = {}", lit(&o)));
    }
    out
}

/// `key=value` pairs as the procedures take them.
fn joined(parts: &[(&str, String)]) -> Option<String> {
    (!parts.is_empty()).then(|| {
        parts
            .iter()
            .map(|(k, v)| format!("{k}={v}"))
            .collect::<Vec<_>>()
            .join(",")
    })
}

fn hhmm(secs: i64) -> String {
    time_text(secs)[..5].to_string()
}

/// `YYYYMMDD`, the date form ASE reads whatever its settings.
fn ymd(ms: i64) -> String {
    date_value(ms).replace('-', "")
}

fn repeats_text(s: &Schedule) -> String {
    if s.repeats == 0 {
        return "0".into();
    }
    let unit = if s.repeats == 1 {
        s.unit.trim_end_matches('s')
    } else {
        s.unit.as_str()
    };
    format!("{}{unit}", s.repeats)
}

/// A schedule's timing as `key=value` pairs (only what it has).
fn timing(s: &Schedule) -> Vec<(&'static str, String)> {
    let mut out = Vec::new();
    if s.repeats > 0 {
        out.push(("repeats", repeats_text(s)));
    }
    if let Some(d) = s.start_date {
        out.push(("startdate", ymd(d)));
    }
    if let Some(t) = s.start_time {
        out.push(("starttime", hhmm(t)));
    }
    if let Some(d) = s.end_date {
        out.push(("enddate", ymd(d)));
    }
    if let Some(t) = s.end_time {
        out.push(("endtime", hhmm(t)));
    }
    if !s.days.is_empty() {
        out.push(("days", s.days.join(":")));
    }
    if !s.dates.is_empty() {
        let d: Vec<String> = s.dates.iter().map(i64::to_string).collect();
        out.push(("dates", d.join(":")));
    }
    out
}

/// The SQL in pieces for `sp_sjobcmd … add`, ending in a line break (Job
/// Scheduler adds none).
fn add_command(p: &mut Plan, name: &str, text: &str) {
    let mut text = text.to_string();
    if !text.ends_with('\n') {
        text.push('\n');
    }
    let chars: Vec<char> = text.chars().collect();
    for piece in chars.chunks(CHUNK) {
        let piece: String = piece.iter().collect();
        p.exec(format!(
            "exec sybmgmtdb..sp_sjobcmd @name = {}, @option = 'add', @text = {}",
            lit(name),
            lit(&piece)
        ));
    }
}

fn new_plan(comment: &str) -> Plan {
    let mut p = Plan::new(comment);
    p.go = true;
    p
}

/// The new object's id, by name, from `sp_sjobhelp`.
fn lookup(
    what: &'static str,
    id_cols: &'static [&'static str],
    name_cols: &'static [&'static str],
    name: String,
) -> Lookup {
    Box::new(move |src: &mut dyn JobSource| {
        let a = access_or(src)?;
        let s = help(src, &a, None, what)?;
        Ok((0..s.rows.len())
            .filter(|r| get(&s, *r, name_cols) == Some(name.as_str()))
            .filter_map(|r| int(get(&s, r, id_cols)))
            .max()
            .map(|n| n.to_string()))
    })
}

/// `sp_sjobcontrol … enable | disable` on scheduled job `key` (its name or
/// id), as the tree's switch does it ([`crate::jobs::ase::control`]).
fn switch(p: &mut Plan, key: &str, enabled: bool, admin: bool) {
    p.exec(exec(
        "sp_sjobcontrol",
        key,
        Some(options(if enabled { "enable" } else { "disable" }, admin)),
    ));
}

fn create_scheduled(p: &mut Plan, j: &Scheduled, admin: bool) {
    let mut parts: Vec<(&str, String)> =
        vec![("jname", j.job.clone()), ("sname", j.schedule.clone())];
    if let Some(s) = &j.server {
        parts.push(("server", s.clone()));
    }
    if let Some(t) = j.timeout {
        parts.push(("timeout", t.to_string()));
    }
    if let Some(pr) = &j.properties {
        parts.push(("sjproperties", pr.clone()));
    }
    // `sjname=` says what is created, as `jname=` and `sname=` do.
    let key = format!("sjname={}", j.name);
    p.exec(exec("sp_sjobcreate", &key, joined(&parts)));
    // Disabled before another owner takes it (the creator owns it now).
    if !j.enabled {
        switch(p, &j.name, false, admin);
    }
    let mut after: Vec<(&str, String)> = Vec::new();
    if let Some(l) = &j.locale {
        after.push(("locale", l.clone()));
    }
    if let (true, Some(o)) = (admin, &j.owner) {
        after.push(("sjowner", o.clone()));
    }
    if let Some(o) = joined(&after) {
        p.exec(exec("sp_sjobmodify", &key, Some(o)));
    }
    p.lookup = Some(lookup("list,scheduled", SJ_ID, SJ_NAME, j.name.clone()));
}

pub(super) fn scheduled_create_plan(j: &Scheduled, admin: bool) -> Plan {
    let mut p = new_plan(&format!("Job programado «{}» de Job Scheduler", j.name));
    create_scheduled(&mut p, j, admin);
    p
}

/// A text option that changed (an empty value clears it).
fn changed_text(
    parts: &mut Vec<(&'static str, String)>,
    key: &'static str,
    cur: &Option<String>,
    new: &Option<String>,
) {
    if !same(cur, new) {
        parts.push((key, new.clone().unwrap_or_default()));
    }
}

pub(super) fn scheduled_alter_plan(
    n: u64,
    cur: &Scheduled,
    j: &Scheduled,
    admin: bool,
) -> Result<Plan> {
    if j.job != cur.job || j.schedule != cur.schedule {
        let mut p = new_plan(&format!("Job programado «{}» de Job Scheduler", cur.name));
        for line in RELINK.split("; ") {
            p.comment(line);
        }
        p.exec(exec(
            "sp_sjobdrop",
            &n.to_string(),
            admin.then(|| "all_users".to_string()),
        ));
        create_scheduled(&mut p, j, admin);
        return Ok(p);
    }
    let mut parts: Vec<(&str, String)> = Vec::new();
    if j.name != cur.name {
        parts.push(("sjname", j.name.clone()));
    }
    if !same(&cur.server, &j.server) {
        let s = j.server.clone().ok_or_else(|| {
            bad("«Servidor»: Job Scheduler no lo deja vacío; poné el nombre del servidor.")
        })?;
        parts.push(("server", s));
    }
    if j.timeout != cur.timeout {
        let t = j.timeout.ok_or_else(|| {
            bad("«Tiempo máximo (minutos)»: Job Scheduler no lo deja vacío; poné otro valor.")
        })?;
        parts.push(("timeout", t.to_string()));
    }
    changed_text(&mut parts, "locale", &cur.locale, &j.locale);
    changed_text(&mut parts, "sjproperties", &cur.properties, &j.properties);
    if admin && !same(&cur.owner, &j.owner) {
        if let Some(o) = &j.owner {
            parts.push(("sjowner", o.clone()));
        }
    }
    let switched = j.enabled != cur.enabled;
    let modify = joined(&parts);
    if modify.is_none() && !switched {
        return Ok(Plan::nothing(&cur.name, n.to_string()));
    }
    let mut p = new_plan(&format!("Job programado «{}» de Job Scheduler", cur.name));
    if let Some(o) = modify {
        p.exec(exec("sp_sjobmodify", &n.to_string(), Some(o)));
    }
    if switched {
        switch(&mut p, &n.to_string(), j.enabled, admin);
    }
    p.id = Some(n.to_string());
    Ok(p)
}

pub(super) fn job_create_plan(j: &JobDef, admin: bool) -> Plan {
    let mut p = new_plan(&format!("Job «{}» de Job Scheduler", j.name));
    let mut parts: Vec<(&str, String)> = Vec::new();
    if let Some(d) = &j.description {
        parts.push(("jdesc", d.clone()));
    }
    if let Some(pr) = &j.properties {
        parts.push(("jproperties", pr.clone()));
    }
    if let Some(t) = j.timeout {
        parts.push(("default_timeout", t.to_string()));
    }
    let key = format!("jname={}", j.name);
    p.exec(exec("sp_sjobcreate", &key, joined(&parts)));
    add_command(&mut p, &key, &j.command);
    if let (true, Some(o)) = (admin, &j.owner) {
        p.exec(exec("sp_sjobmodify", &key, Some(format!("jowner={o}"))));
    }
    p.lookup = Some(lookup("list,jobs", J_ID, J_NAME, j.name.clone()));
    p
}

pub(super) fn job_alter_plan(n: u64, cur: &JobDef, j: &JobDef, admin: bool) -> Result<Plan> {
    let mut parts: Vec<(&str, String)> = Vec::new();
    if j.name != cur.name {
        parts.push(("jname", j.name.clone()));
    }
    changed_text(&mut parts, "jdesc", &cur.description, &j.description);
    changed_text(&mut parts, "jproperties", &cur.properties, &j.properties);
    if j.timeout != cur.timeout {
        let t = j.timeout.ok_or_else(|| {
            bad("«Tiempo máximo (minutos)»: Job Scheduler no lo deja vacío; poné otro valor.")
        })?;
        parts.push(("default_timeout", t.to_string()));
    }
    if admin && !same(&cur.owner, &j.owner) {
        if let Some(o) = &j.owner {
            parts.push(("jowner", o.clone()));
        }
    }
    let command_changed = j.command.trim_end() != cur.command.trim_end();
    if parts.is_empty() && !command_changed {
        return Ok(Plan::nothing(&cur.name, n.to_string()));
    }
    let key = format!("jname={n}");
    let mut p = new_plan(&format!("Job «{}» de Job Scheduler", cur.name));
    if let Some(o) = joined(&parts) {
        p.exec(exec("sp_sjobmodify", &key, Some(o)));
    }
    if command_changed {
        p.exec(format!(
            "exec sybmgmtdb..sp_sjobcmd @name = {}, @option = 'drop'",
            lit(&key)
        ));
        add_command(&mut p, &key, &j.command);
    }
    p.id = Some(n.to_string());
    Ok(p)
}

pub(super) fn schedule_create_plan(s: &Schedule, admin: bool) -> Plan {
    let mut p = new_plan(&format!("Calendario «{}» de Job Scheduler", s.name));
    let mut parts: Vec<(&str, String)> = Vec::new();
    if let Some(d) = &s.description {
        parts.push(("sdesc", d.clone()));
    }
    if let Some(pr) = &s.properties {
        parts.push(("sproperties", pr.clone()));
    }
    parts.extend(timing(s));
    let key = format!("sname={}", s.name);
    p.exec(exec("sp_sjobcreate", &key, joined(&parts)));
    if let (true, Some(o)) = (admin, &s.owner) {
        p.exec(exec("sp_sjobmodify", &key, Some(format!("sowner={o}"))));
    }
    p.lookup = Some(lookup("list,schedules", S_ID, S_NAME, s.name.clone()));
    p
}

pub(super) fn schedule_alter_plan(n: u64, cur: &Schedule, s: &Schedule, admin: bool) -> Plan {
    let key = format!("sname={n}");
    let mut parts: Vec<(&str, String)> = Vec::new();
    if s.name != cur.name {
        parts.push(("sname", s.name.clone()));
    }
    changed_text(&mut parts, "sdesc", &cur.description, &s.description);
    changed_text(&mut parts, "sproperties", &cur.properties, &s.properties);
    if admin && !same(&cur.owner, &s.owner) {
        if let Some(o) = &s.owner {
            parts.push(("sowner", o.clone()));
        }
    }
    let (old, new) = (timing(cur), timing(s));
    // A timing value that goes away can only be cleared by resetting them
    // all (reset=true), then setting again those that stay.
    let cleared = old.iter().any(|(k, _)| !new.iter().any(|(n, _)| n == k))
        || (cur.repeats > 0 && s.repeats == 0);
    let mut p = new_plan(&format!("Calendario «{}» de Job Scheduler", cur.name));
    if cleared {
        p.exec(exec("sp_sjobmodify", &key, Some("reset=true".into())));
        parts.extend(new);
    } else {
        for (k, v) in new {
            if !old.iter().any(|(o, w)| *o == k && *w == v) {
                parts.push((k, v));
            }
        }
    }
    match joined(&parts) {
        Some(o) => p.exec(exec("sp_sjobmodify", &key, Some(o))),
        None if !cleared => return Plan::nothing(&cur.name, n.to_string()),
        None => {}
    }
    p.id = Some(n.to_string());
    p
}

pub(super) fn plan(
    src: &mut dyn JobSource,
    k: Kind,
    id: Option<&str>,
    values: &FormValues,
) -> Result<Plan> {
    let a = access_or(src)?;
    let check_mine = |src: &mut dyn JobSource, owner: Option<&str>| -> Result<()> {
        let me = me(src);
        if mine(&a, me.as_deref(), owner) {
            Ok(())
        } else {
            Err(Error::Unsupported(NOT_YOURS.into()))
        }
    };
    match (k, id) {
        (Kind::Scheduled, None) => Ok(scheduled_create_plan(&scheduled_from(values)?, a.admin)),
        (Kind::Scheduled, Some(id)) => {
            let j = scheduled_from(values)?;
            let (n, cur) = scheduled_read(src, &a, id)?;
            check_mine(src, cur.owner.as_deref())?;
            scheduled_alter_plan(n, &cur, &j, a.admin)
        }
        (Kind::Job, None) => Ok(job_create_plan(&job_from(values)?, a.admin)),
        (Kind::Job, Some(id)) => {
            let j = job_from(values)?;
            let (n, cur) = job_read(src, &a, id)?;
            check_mine(src, cur.owner.as_deref())?;
            job_alter_plan(n, &cur, &j, a.admin)
        }
        (Kind::Schedule, None) => Ok(schedule_create_plan(&schedule_from(values)?, a.admin)),
        (Kind::Schedule, Some(id)) => {
            let s = schedule_from(values)?;
            let (n, cur) = schedule_read(src, &a, id)?;
            check_mine(src, cur.owner.as_deref())?;
            Ok(schedule_alter_plan(n, &cur, &s, a.admin))
        }
    }
}

pub(super) fn drop_plan(k: Kind, n: u64, name: &str, admin: bool) -> Plan {
    let (what, key) = match k {
        Kind::Scheduled => ("el job programado", n.to_string()),
        Kind::Job => ("el job", format!("jname={n}")),
        Kind::Schedule => ("el calendario", format!("sname={n}")),
    };
    let mut p = new_plan(&format!("Borra {what} «{name}» de Job Scheduler"));
    p.exec(exec(
        "sp_sjobdrop",
        &key,
        admin.then(|| "all_users".to_string()),
    ));
    p
}

pub(super) fn delete_plan(src: &mut dyn JobSource, k: Kind, id: &str) -> Result<Plan> {
    let a = access_or(src)?;
    let (n, name, owner) = match k {
        Kind::Scheduled => {
            let (n, j) = scheduled_read(src, &a, id)?;
            (n, j.name, j.owner)
        }
        Kind::Job => {
            let (n, j) = job_read(src, &a, id)?;
            (n, j.name, j.owner)
        }
        Kind::Schedule => {
            let (n, s) = schedule_read(src, &a, id)?;
            (n, s.name, s.owner)
        }
    };
    let me = me(src);
    if !mine(&a, me.as_deref(), owner.as_deref()) {
        return Err(Error::Unsupported(NOT_YOURS.into()));
    }
    if k != Kind::Scheduled {
        let used = users_of(src, &a, k, &name);
        if !used.is_empty() {
            let list: Vec<String> = used.iter().map(|u| format!("«{u}»")).collect();
            return Err(bad(format!(
                "Lo usan los jobs programados {}: borralos antes.",
                list.join(", ")
            )));
        }
    }
    Ok(drop_plan(k, n, &name, a.admin))
}

pub(super) fn clear_history_plan(n: u64, name: &str, admin: bool) -> Plan {
    let mut p = new_plan(&format!(
        "Borra el historial y la salida de las ejecuciones terminadas del job programado «{name}» (las que siguen corriendo quedan)"
    ));
    p.exec(format!(
        "exec sybmgmtdb..sp_sjobhistory @name = {}, @option = {}",
        lit(&n.to_string()),
        lit(&options("drop", admin))
    ));
    p
}

pub(super) fn clear_plan(src: &mut dyn JobSource, id: &str) -> Result<Plan> {
    let a = access_or(src)?;
    let (n, j) = scheduled_read(src, &a, id)?;
    Ok(clear_history_plan(n, &j.name, a.admin))
}
