//! Server jobs ([`dbine_driver::Session::jobs`] and the calls next to it)
//! per preset, from each engine's own scheduler:
//!
//! - Db2 LUW: the Administrative Task Scheduler (`SYSTOOLS.ADMIN_TASK_LIST`
//!   and `ADMIN_TASK_STATUS`). It has no run now, stop or enable: tasks run
//!   only on their schedule. Ids are `TASKID`s.
//! - Db2 for z/OS: the same scheduler through `DSNADM.ADMIN_TASK_LIST()` and
//!   `ADMIN_TASK_STATUS()`. Ids are task names.
//! - Db2 for i: the job scheduler's entries (`QSYS2.SCHEDULED_JOB_INFO`),
//!   held and released with `HLDJOBSCDE` / `RLSJOBSCDE` through
//!   `QSYS2.QCMDEXC`. Ids are `000123/NAME` (entry number and job name).
//! - Informix and GBase 8s: the DB Scheduler in the `sysadmin` database
//!   (`ph_task`, `ph_run`, `ph_alert`); run now with `exectask_async`.
//!   Ids are `tk_id`s.
//! - SAP ASE: Job Scheduler (`sybmgmtdb`), through its `sp_sjob*`
//!   procedures. Ids are scheduled job ids.
//! - SQL Anywhere: events with a schedule or a system event type
//!   (`SYS.SYSEVENT`, `SYS.SYSSCHEDULE`); `TRIGGER EVENT` runs one now.
//!   Ids are `event_id`s.
//! - InterSystems IRIS and Caché: the Task Manager (`%SYS.Task` and its
//!   history), read only: running and suspending a task are ObjectScript
//!   methods SQL doesn't reach.
//! - Hive 4 and Cloudera's Hive: scheduled queries
//!   (`information_schema.scheduled_queries`, executions from
//!   `sys.scheduled_executions` or, without access to `sys`, the
//!   information_schema view),
//!   run now, enabled and stopped with `ALTER SCHEDULED QUERY` and
//!   `KILL QUERY`. Ids are `scheduled_query_id`s.
//!
//! Every id comes back from the app as untrusted text: each engine checks
//! it has the shape it produced (digits, an IBM i name…) and binds it or
//! puts it in a literal only after that. Times are the server's local time
//! in these catalogs: [`Tz`] turns them into UTC with the offset the server
//! reports at the moment of the call.

mod ase;
mod db2;
mod db2i;
mod hive;
mod informix;
mod iris;
mod sqla;
#[cfg(test)]
mod tests;

use crate::design::{eng, Eng};
use crate::monitor::Set;
use crate::presets::Preset;
use dbine_driver::{Error, JobDetail, JobList, JobOutcome, JobRun, PropertyInfo, Result};

/// Where the SQL goes: the ODBC connection, or canned sets in tests.
pub trait JobSource {
    /// The first result set of `sql` (empty when it has none), with text
    /// parameters bound in order.
    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Set>;
    /// Runs a statement that returns no rows; the rows it changed, when the
    /// driver says.
    fn exec(&mut self, sql: &str, params: &[&str]) -> Result<Option<u64>>;
}

impl JobSource for crate::ConnSource<'_> {
    fn query(&mut self, sql: &str, params: &[&str]) -> Result<Set> {
        let st = self.c.stmt(self.slot)?;
        let rc = if params.is_empty() {
            st.exec(sql)?
        } else {
            st.exec_params(sql, params)?
        };
        if rc == crate::ffi::SQL_NO_DATA {
            return Ok(Set::default());
        }
        loop {
            let n = st.num_cols()?;
            if n > 0 {
                let cols = st.describe(n)?.into_iter().map(|c| c.name).collect();
                return Ok(Set {
                    cols,
                    rows: st.text_rows()?,
                });
            }
            if !st.more_results()? {
                return Ok(Set::default());
            }
        }
    }

    fn exec(&mut self, sql: &str, params: &[&str]) -> Result<Option<u64>> {
        let st = self.c.stmt(self.slot)?;
        let rc = if params.is_empty() {
            st.exec(sql)?
        } else {
            st.exec_params(sql, params)?
        };
        if rc == crate::ffi::SQL_NO_DATA {
            return Ok(Some(0));
        }
        let n = st.row_count();
        // Procedures (CALL, exec) may leave result sets behind.
        while st.more_results()? {}
        Ok(n)
    }
}

/// What the engines need to know about the session.
pub struct Ctx {
    /// The session's database (Informix runs `exectask_async` only from
    /// `sysadmin`).
    pub database: String,
}

/// Why a preset has no server jobs, in Spanish (for the error and the
/// docs). `None`: it has them.
pub fn unsupported_reason(p: &Preset) -> Option<&'static str> {
    match eng(p) {
        Eng::Db2
        | Eng::Db2zos
        | Eng::Db2i
        | Eng::Informix
        | Eng::Ase
        | Eng::Sqla
        | Eng::Iris
        | Eng::Hive => None,
        Eng::Generic => Some("el preset ODBC genérico no conoce el planificador del motor: usá el preset del motor"),
        Eng::Access | Eng::DBase => Some("es una base de archivos sin servidor: no tiene jobs propios"),
        Eng::NetSuite => Some("SuiteAnalytics Connect es un servicio de solo lectura de NetSuite: no expone tareas programadas"),
        Eng::Teradata => Some("Teradata no tiene un planificador en la base: los jobs se programan afuera (Viewpoint, cron)"),
        Eng::Exasol => Some("Exasol no tiene un planificador en la base: los jobs se programan afuera"),
        Eng::Netezza => Some("Netezza no tiene un planificador en la base: los jobs se programan afuera (cron)"),
        Eng::Impala | Eng::Spark => Some("este motor no tiene un planificador propio: los jobs se programan afuera (Oozie, Airflow…)"),
        Eng::MaxDb => Some("SAP MaxDB programa sus tareas con el DBM Server, que no se consulta por SQL"),
        Eng::Cubrid => Some("CUBRID no tiene un planificador en la base: las tareas automáticas son de CUBRID Manager"),
        Eng::Vertica | Eng::Dameng | Eng::Altibase | Eng::Virtuoso => {
            Some("DBine todavía no lee el planificador de este motor")
        }
        Eng::MonetDb
        | Eng::Ingres
        | Eng::Mimer
        | Eng::OpenEdge
        | Eng::Zen
        | Eng::Sqream
        | Eng::NuoDb
        | Eng::HeavyDb
        | Eng::Machbase
        | Eng::Ignite
        | Eng::Ignite3
        | Eng::Ocient
        | Eng::SqlServer => Some("este motor no tiene un planificador de jobs en la base"),
    }
}

pub fn supported(p: &Preset) -> bool {
    unsupported_reason(p).is_none()
}

fn unsupported(p: &Preset) -> Error {
    Error::Unsupported(
        unsupported_reason(p)
            .unwrap_or("este motor no tiene jobs del servidor")
            .into(),
    )
}

pub fn list(p: &Preset, src: &mut dyn JobSource, ctx: &Ctx) -> Result<JobList> {
    match eng(p) {
        Eng::Db2 => db2::list(src),
        Eng::Db2zos => db2::zos_list(src),
        Eng::Db2i => db2i::list(src),
        Eng::Informix => informix::list(src, ctx),
        Eng::Ase => ase::list(src),
        Eng::Sqla => sqla::list(src),
        Eng::Iris => iris::list(src),
        Eng::Hive => hive::list(src),
        _ => Err(unsupported(p)),
    }
}

pub fn detail(p: &Preset, src: &mut dyn JobSource, ctx: &Ctx, id: &str) -> Result<JobDetail> {
    match eng(p) {
        Eng::Db2 => db2::detail(src, id),
        Eng::Db2zos => db2::zos_detail(src, id),
        Eng::Db2i => db2i::detail(src, id),
        Eng::Informix => informix::detail(src, ctx, id),
        Eng::Ase => ase::detail(src, id),
        Eng::Sqla => sqla::detail(src, id),
        Eng::Iris => iris::detail(src, id),
        Eng::Hive => hive::detail(src, id),
        _ => Err(unsupported(p)),
    }
}

pub fn runs(p: &Preset, src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let limit = limit.min(dbine_driver::jobs::MAX_JOB_RUNS);
    let mut out = match eng(p) {
        // Engines without history say so below, whatever the limit.
        Eng::Db2 | Eng::Db2zos | Eng::Informix | Eng::Ase | Eng::Iris | Eng::Hive if limit == 0 => {
            return Ok(Vec::new())
        }
        Eng::Db2 => db2::runs(src, id, limit)?,
        Eng::Db2zos => db2::zos_runs(src, id, limit)?,
        Eng::Informix => informix::runs(src, id, limit)?,
        Eng::Ase => ase::runs(src, id, limit)?,
        Eng::Iris => iris::runs(src, id, limit)?,
        Eng::Hive => hive::runs(src, id, limit)?,
        Eng::Db2i => return Err(Error::Unsupported(db2i::NO_HISTORY.into())),
        Eng::Sqla => return Err(Error::Unsupported(sqla::NO_HISTORY.into())),
        _ => return Err(unsupported(p)),
    };
    out.truncate(limit as usize);
    Ok(out)
}

pub fn run(p: &Preset, src: &mut dyn JobSource, ctx: &Ctx, id: &str) -> Result<()> {
    match eng(p) {
        Eng::Db2 | Eng::Db2zos => Err(Error::Unsupported(db2::NO_RUN_NOW.into())),
        Eng::Db2i => Err(Error::Unsupported(db2i::NO_RUN_NOW.into())),
        Eng::Informix => informix::run(src, ctx, id),
        Eng::Ase => ase::control(src, id, "run_now"),
        Eng::Sqla => sqla::run(src, id),
        Eng::Iris => Err(Error::Unsupported(iris::NO_CONTROL.into())),
        Eng::Hive => hive::run(src, id),
        _ => Err(unsupported(p)),
    }
}

pub fn stop(p: &Preset, src: &mut dyn JobSource, id: &str) -> Result<()> {
    match eng(p) {
        Eng::Ase => ase::control(src, id, "terminate"),
        Eng::Hive => hive::stop(src, id),
        Eng::Iris => Err(Error::Unsupported(iris::NO_CONTROL.into())),
        Eng::Db2 | Eng::Db2zos | Eng::Db2i | Eng::Informix | Eng::Sqla => Err(Error::Unsupported(
            "el planificador de este motor no detiene una ejecución en curso".into(),
        )),
        _ => Err(unsupported(p)),
    }
}

pub fn set_enabled(p: &Preset, src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    match eng(p) {
        Eng::Db2i => db2i::set_enabled(src, id, enabled),
        Eng::Informix => informix::set_enabled(src, id, enabled),
        Eng::Ase => ase::control(src, id, if enabled { "enable" } else { "disable" }),
        Eng::Sqla => sqla::set_enabled(src, id, enabled),
        Eng::Hive => hive::set_enabled(src, id, enabled),
        Eng::Db2 | Eng::Db2zos => Err(Error::Unsupported(db2::NO_ENABLE.into())),
        Eng::Iris => Err(Error::Unsupported(iris::NO_CONTROL.into())),
        _ => Err(unsupported(p)),
    }
}

pub fn script(p: &Preset, src: &mut dyn JobSource, id: &str) -> Result<String> {
    match eng(p) {
        Eng::Db2 => db2::script(src, id),
        Eng::Db2zos => db2::zos_script(src, id),
        Eng::Db2i => db2i::script(src, id),
        Eng::Informix => informix::script(src, id),
        Eng::Sqla => sqla::script(src, id),
        Eng::Hive => hive::script(src, id),
        Eng::Ase => Err(Error::Unsupported(ase::NO_SCRIPT.into())),
        Eng::Iris => Err(Error::Unsupported(iris::NO_SCRIPT.into())),
        _ => Err(unsupported(p)),
    }
}

// ------------------------------------------------------------- helpers

/// The first of `names` the set has with a value in row `r`.
fn get<'a>(s: &'a Set, r: usize, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| s.get(r, n))
}

fn owned(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

/// `'text'`, doubling embedded quotes.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// An id as the error shows it: trimmed and short.
fn shown(id: &str) -> String {
    let t = id.trim();
    match t.char_indices().nth(60) {
        Some((at, _)) => format!("{}…", &t[..at]),
        None => t.to_string(),
    }
}

fn bad_id(id: &str) -> Error {
    Error::Query(format!("«{}» no es un id de job válido", shown(id)))
}

fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe el job «{}» o este usuario no lo ve",
        shown(id)
    ))
}

/// A whole number id (`TASKID`, `tk_id`, `event_id`…): digits only.
fn int_id(id: &str) -> Result<u64> {
    let t = id.trim();
    if t.is_empty() || t.len() > 18 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad_id(id));
    }
    t.parse().map_err(|_| bad_id(id))
}

/// A whole number in a column.
fn int(s: Option<&str>) -> Option<i64> {
    let t = s?.trim();
    t.parse::<i64>().ok().or_else(|| {
        t.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v as i64)
    })
}

fn yes(s: Option<&str>) -> bool {
    matches!(
        s.map(|v| v.trim().to_ascii_uppercase()).as_deref(),
        Some("1" | "T" | "Y" | "YES" | "TRUE" | "ON")
    )
}

/// The engine's own error number, when the driver reported one.
fn code(e: &Error) -> Option<i64> {
    match e {
        Error::Statement(s) => s.code.as_deref().and_then(|c| c.trim().parse().ok()),
        _ => None,
    }
}

/// Keeps `n` characters at most.
fn clip(s: &str, n: usize) -> String {
    match s.char_indices().nth(n) {
        Some((at, _)) => s[..at].to_string(),
        None => s.to_string(),
    }
}

fn prop(group: &str, label: &str, value: Option<&str>) -> Option<PropertyInfo> {
    let v = value?.trim();
    (!v.is_empty()).then(|| PropertyInfo {
        group: group.into(),
        label: label.into(),
        value: v.to_string(),
    })
}

/// Properties with a value, in order.
fn props(list: Vec<Option<PropertyInfo>>) -> Vec<PropertyInfo> {
    list.into_iter().flatten().collect()
}

/// Newest first by start time (RFC 3339 sorts as text), then by id.
fn newest_first(runs: &mut [JobRun]) {
    runs.sort_by(|a, b| {
        b.started
            .cmp(&a.started)
            .then_with(|| int(b.id.as_deref()).cmp(&int(a.id.as_deref())))
            .then_with(|| b.id.cmp(&a.id))
    });
}

// ---------------------------------------------------------------- time

/// Days from 1970-01-01 to a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = if y >= 0 { y } else { y - 399 } / 400;
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146097 + doe - 719468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719468;
    let era = if z >= 0 { z } else { z - 146096 } / 146097;
    let doe = z - era * 146097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Leading digits of `s` as a number and the rest.
fn digits(s: &str, max: usize) -> Option<(i64, &str)> {
    let n = s
        .bytes()
        .take(max)
        .take_while(|b| b.is_ascii_digit())
        .count();
    if n == 0 {
        return None;
    }
    Some((s[..n].parse().ok()?, &s[n..]))
}

/// `HH:MM[:SS[.fff]]` (or with dots, as Db2 writes it) in ms after
/// midnight.
fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let (h, rest) = digits(s, 2)?;
    let rest = rest.strip_prefix([':', '.'])?;
    let (m, rest) = digits(rest, 2)?;
    let (sec, rest) = match rest.strip_prefix([':', '.']) {
        Some(r) => digits(r, 2)?,
        None => (0, rest),
    };
    let ms = match rest.strip_prefix(['.', ',']) {
        Some(f) => {
            let f: String = f.chars().take_while(char::is_ascii_digit).collect();
            let f3: String = f.chars().chain("000".chars()).take(3).collect();
            f3.parse::<i64>().ok()?
        }
        None => 0,
    };
    (h < 24 && m < 60 && sec < 61).then_some(((h * 60 + m) * 60 + sec) * 1000 + ms)
}

/// `YYYY-MM-DD[ HH:MM:SS[.ffffff]]` (also with `T` or Db2's `-` before
/// the time) as ms since the epoch, read as if it were UTC.
fn parse_ts(s: &str) -> Option<i64> {
    let s = s.trim();
    let (y, rest) = digits(s, 4)?;
    let rest = rest.strip_prefix('-')?;
    let (m, rest) = digits(rest, 2)?;
    let rest = rest.strip_prefix('-')?;
    let (d, rest) = digits(rest, 2)?;
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) || y < 1000 {
        return None;
    }
    let day = days_from_civil(y, m, d) * 86_400_000;
    let rest = rest.trim_start_matches([' ', 'T', '-']);
    if rest.is_empty() {
        return Some(day);
    }
    Some(day + parse_time(rest)?)
}

fn rfc3339(ms: i64) -> String {
    let secs = ms.div_euclid(1000);
    let (y, m, d) = civil_from_days(secs.div_euclid(86_400));
    let t = secs.rem_euclid(86_400);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        t / 3600,
        t / 60 % 60,
        t % 60
    )
}

/// ms between two local times of the same server.
fn duration_ms(start: Option<&str>, end: Option<&str>) -> Option<u64> {
    let (a, b) = (parse_ts(start?)?, parse_ts(end?)?);
    u64::try_from(b - a).ok()
}

/// The server's offset from UTC, to turn its local times into UTC.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub struct Tz {
    /// Local minus UTC, ms.
    offset_ms: i64,
    /// The server's local now, ms (as [`parse_ts`] reads it).
    now: Option<i64>,
}

impl Tz {
    /// From the server's local now and its UTC now (a timestamp, or
    /// seconds since the epoch), rounded to a quarter hour.
    fn from_now(local: Option<&str>, utc: Option<&str>) -> Tz {
        let local = local.and_then(parse_ts);
        let utc = utc.and_then(|u| {
            let t = u.trim();
            if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
                t.parse::<i64>().ok().map(|s| s * 1000)
            } else {
                parse_ts(t)
            }
        });
        match (local, utc) {
            (Some(l), Some(u)) => {
                let q = 15 * 60 * 1000;
                Tz {
                    offset_ms: ((l - u) as f64 / q as f64).round() as i64 * q,
                    now: Some(l),
                }
            }
            (l, _) => Tz {
                offset_ms: 0,
                now: l,
            },
        }
    }

    /// `local` (a time of the server) is already past.
    fn past(&self, local: Option<&str>) -> bool {
        matches!((local.and_then(parse_ts), self.now), (Some(t), Some(n)) if t <= n)
    }

    /// Asks the server with `sql`, whose first row has the columns
    /// `LOCAL_NOW` and `UTC_NOW`. UTC when it fails.
    fn query(src: &mut dyn JobSource, sql: &str) -> Tz {
        match src.query(sql, &[]) {
            Ok(s) if !s.rows.is_empty() => Tz::from_now(s.get(0, "LOCAL_NOW"), s.get(0, "UTC_NOW")),
            Ok(_) => Tz::default(),
            Err(e) => {
                tracing::debug!("odbc jobs: server time failed, reading times as UTC: {e}");
                Tz::default()
            }
        }
    }

    /// A local time of the server, RFC 3339 in UTC.
    fn utc(&self, local: Option<&str>) -> Option<String> {
        local.and_then(parse_ts).map(|ms| self.utc_ms(ms))
    }

    /// A local time of the server in ms, RFC 3339 in UTC.
    fn utc_ms(&self, local_ms: i64) -> String {
        rfc3339(local_ms - self.offset_ms)
    }
}

/// The last finished run's outcome and the "running" flag together.
fn last(outcome: Option<JobOutcome>, running: bool) -> Option<JobOutcome> {
    match outcome {
        Some(o) => Some(o),
        None if running => Some(JobOutcome::Running),
        None => None,
    }
}
