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
//! - Vertica (11.1.1 on): stored procedures run by a trigger on a schedule
//!   (`STORED_PROC_TRIGGERS`, `USER_SCHEDULES`), run now (in the session:
//!   the only way Vertica has) and enabled with `EXECUTE_TRIGGER` and
//!   `ENABLE_TRIGGER`. Ids are the trigger's quoted `"schema"."trigger"`.
//! - Dameng (DM): the job system in the `SYSJOB` schema (`SYSJOBS`, its
//!   steps, schedules and histories; DBMS_JOB's jobs live there too), run
//!   now, stopped and enabled with `SP_DBMS_JOB_RUN_ASYNC`,
//!   `SP_STOP_RUNNING_JOB` and `SP_ENABLE_JOB`; and DBMS_SCHEDULER's jobs
//!   next to them. Ids are job `ID`s (DBMS_SCHEDULER's, `"OWNER"."JOB"`).
//! - Altibase: `CREATE JOB`'s jobs (`SYSTEM_.SYS_JOBS_`), enabled with
//!   `ALTER JOB … SET ENABLE | DISABLE`. Ids are `JOB_ID`s.
//! - Virtuoso: the scheduler's events (`DB.DBA.SYS_SCHEDULED_EVENT`),
//!   enabled through `SE_DISABLED`. Ids are event names, always bound.
//!
//! Every id comes back from the app as untrusted text: each engine checks
//! it has the shape it produced (digits, an IBM i name…) and binds it or
//! puts it in a literal only after that. Times are the server's local time
//! in these catalogs: [`Tz`] turns them into UTC with the offset the server
//! reports at the moment of the call.

pub(crate) mod altibase;
pub(crate) mod ase;
pub(crate) mod dameng;
pub(crate) mod db2;
pub(crate) mod db2i;
pub(crate) mod hive;
pub(crate) mod informix;
pub(crate) mod iris;
pub(crate) mod sqla;
#[cfg(test)]
mod tests;
pub(crate) mod vertica;
pub(crate) mod virtuoso;

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
        | Eng::Hive
        | Eng::Vertica
        | Eng::Dameng
        | Eng::Altibase
        | Eng::Virtuoso => None,
        Eng::Generic => Some("el preset ODBC genérico no conoce el planificador del motor: usá el preset del motor"),
        Eng::Access | Eng::DBase => Some("es una base de archivos sin servidor: no tiene jobs propios"),
        Eng::NetSuite => Some("SuiteAnalytics Connect es un servicio de solo lectura de NetSuite: no expone tareas programadas"),
        Eng::Teradata => Some("Teradata no tiene un planificador en la base: los jobs se programan afuera (Viewpoint, cron)"),
        Eng::Exasol => Some("Exasol no tiene un planificador en la base: los jobs se programan afuera"),
        Eng::Netezza => Some("Netezza no tiene un planificador en la base: los jobs se programan afuera (cron)"),
        Eng::Impala | Eng::Spark => Some("este motor no tiene un planificador propio: los jobs se programan afuera (Oozie, Airflow…)"),
        Eng::MaxDb => Some("SAP MaxDB programa sus tareas con el DBM Server, que no se consulta por SQL"),
        Eng::Cubrid => Some("CUBRID no tiene un planificador en la base: las tareas automáticas son de CUBRID Manager"),
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
        Eng::Vertica => vertica::list(src),
        Eng::Dameng => dameng::list(src),
        Eng::Altibase => altibase::list(src),
        Eng::Virtuoso => virtuoso::list(src),
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
        Eng::Vertica => vertica::detail(src, id),
        Eng::Dameng => dameng::detail(src, id),
        Eng::Altibase => altibase::detail(src, id),
        Eng::Virtuoso => virtuoso::detail(src, id),
        _ => Err(unsupported(p)),
    }
}

pub fn runs(p: &Preset, src: &mut dyn JobSource, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let limit = limit.min(dbine_driver::jobs::MAX_JOB_RUNS);
    let mut out = match eng(p) {
        // Engines without history say so below, whatever the limit.
        Eng::Db2 | Eng::Db2zos | Eng::Informix | Eng::Ase | Eng::Iris | Eng::Hive | Eng::Dameng
            if limit == 0 =>
        {
            return Ok(Vec::new())
        }
        Eng::Db2 => db2::runs(src, id, limit)?,
        Eng::Db2zos => db2::zos_runs(src, id, limit)?,
        Eng::Informix => informix::runs(src, id, limit)?,
        Eng::Ase => ase::runs(src, id, limit)?,
        Eng::Iris => iris::runs(src, id, limit)?,
        Eng::Hive => hive::runs(src, id, limit)?,
        Eng::Dameng => dameng::runs(src, id, limit)?,
        Eng::Db2i => return Err(Error::Unsupported(db2i::NO_HISTORY.into())),
        Eng::Sqla => return Err(Error::Unsupported(sqla::NO_HISTORY.into())),
        Eng::Vertica => return Err(Error::Unsupported(vertica::NO_HISTORY.into())),
        Eng::Altibase => return Err(Error::Unsupported(altibase::NO_HISTORY.into())),
        Eng::Virtuoso => return Err(Error::Unsupported(virtuoso::NO_HISTORY.into())),
        _ => return Err(unsupported(p)),
    };
    out.truncate(limit as usize);
    Ok(out)
}

/// A statement that runs a job now or stops its run: what [`run`] and
/// [`stop`] send, and what the scheduler's actions show first.
pub(crate) struct Control {
    pub(crate) sql: String,
    /// It answers rows (a function called with `SELECT` or `EXECUTE
    /// FUNCTION`), so it goes as a query.
    pub(crate) rows: bool,
}

impl Control {
    pub(crate) fn exec(sql: String) -> Control {
        Control { sql, rows: false }
    }
    pub(crate) fn query(sql: String) -> Control {
        Control { sql, rows: true }
    }
}

/// Sends the statements in order.
fn send(src: &mut dyn JobSource, stmts: Vec<Control>) -> Result<()> {
    for c in stmts {
        if c.rows {
            src.query(&c.sql, &[])?;
        } else {
            src.exec(&c.sql, &[])?;
        }
    }
    Ok(())
}

const NO_STOP: &str = "el planificador de este motor no detiene una ejecución en curso";

pub fn run(p: &Preset, src: &mut dyn JobSource, ctx: &Ctx, id: &str) -> Result<()> {
    let stmts = run_stmts(p, src, ctx, id)?;
    send(src, stmts)
}

/// What runs job `id` now, checked against the server (it exists, the
/// session may run it).
pub(crate) fn run_stmts(
    p: &Preset,
    src: &mut dyn JobSource,
    ctx: &Ctx,
    id: &str,
) -> Result<Vec<Control>> {
    let one = match eng(p) {
        Eng::Db2 | Eng::Db2zos => return Err(Error::Unsupported(db2::NO_RUN_NOW.into())),
        Eng::Db2i => return Err(Error::Unsupported(db2i::NO_RUN_NOW.into())),
        Eng::Informix => informix::run(src, ctx, id)?,
        Eng::Ase => ase::control(src, id, "run_now")?,
        Eng::Sqla => sqla::run(src, id)?,
        Eng::Iris => return Err(Error::Unsupported(iris::NO_CONTROL.into())),
        Eng::Hive => hive::run(src, id)?,
        Eng::Vertica => vertica::run(src, id)?,
        Eng::Dameng => dameng::run(src, id)?,
        Eng::Altibase => return Err(Error::Unsupported(altibase::NO_RUN_NOW.into())),
        Eng::Virtuoso => return Err(Error::Unsupported(virtuoso::NO_RUN_NOW.into())),
        _ => return Err(unsupported(p)),
    };
    Ok(vec![one])
}

pub fn stop(p: &Preset, src: &mut dyn JobSource, id: &str) -> Result<()> {
    let stmts = stop_stmts(p, src, id)?;
    send(src, stmts)
}

/// What stops job `id`'s current run, checked against the server (it is
/// running, the session may stop it).
pub(crate) fn stop_stmts(p: &Preset, src: &mut dyn JobSource, id: &str) -> Result<Vec<Control>> {
    match eng(p) {
        Eng::Ase => Ok(vec![ase::control(src, id, "terminate")?]),
        Eng::Hive => hive::stop(src, id),
        Eng::Dameng => Ok(vec![dameng::stop(src, id)?]),
        Eng::Iris => Err(Error::Unsupported(iris::NO_CONTROL.into())),
        // ADMIN_TASK_CANCEL answers through output parameters, which plain
        // statements can't read: the session stops a z/OS task with the
        // scheduler's plan (`scheduler::action`, `db2zos::cancel_plan`),
        // and so does the scheduler's own "stop". Nothing reaches here.
        Eng::Db2zos => Err(unsupported(p)),
        Eng::Db2
        | Eng::Db2i
        | Eng::Informix
        | Eng::Sqla
        | Eng::Vertica
        | Eng::Altibase
        | Eng::Virtuoso => Err(Error::Unsupported(NO_STOP.into())),
        _ => Err(unsupported(p)),
    }
}

pub fn set_enabled(p: &Preset, src: &mut dyn JobSource, id: &str, enabled: bool) -> Result<()> {
    match eng(p) {
        Eng::Db2i => db2i::set_enabled(src, id, enabled),
        Eng::Informix => informix::set_enabled(src, id, enabled),
        Eng::Ase => {
            let c = ase::control(src, id, if enabled { "enable" } else { "disable" })?;
            send(src, vec![c])
        }
        Eng::Sqla => sqla::set_enabled(src, id, enabled),
        Eng::Hive => hive::set_enabled(src, id, enabled),
        Eng::Vertica => vertica::set_enabled(src, id, enabled),
        Eng::Dameng => dameng::set_enabled(src, id, enabled),
        Eng::Altibase => altibase::set_enabled(src, id, enabled),
        Eng::Virtuoso => virtuoso::set_enabled(src, id, enabled),
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
        Eng::Vertica => vertica::script(src, id),
        Eng::Dameng => dameng::script(src, id),
        Eng::Altibase => altibase::script(src, id),
        Eng::Virtuoso => virtuoso::script(src, id),
        Eng::Ase => Err(Error::Unsupported(ase::NO_SCRIPT.into())),
        Eng::Iris => Err(Error::Unsupported(iris::NO_SCRIPT.into())),
        _ => Err(unsupported(p)),
    }
}

// ------------------------------------------------------------- helpers

/// The first of `names` the set has with a value in row `r`.
pub(crate) fn get<'a>(s: &'a Set, r: usize, names: &[&str]) -> Option<&'a str> {
    names.iter().find_map(|n| s.get(r, n))
}

fn owned(s: Option<&str>) -> Option<String> {
    s.map(str::to_string)
}

/// `'text'`, doubling embedded quotes.
pub(crate) fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// An id as the error shows it: trimmed and short.
pub(crate) fn shown(id: &str) -> String {
    let t = id.trim();
    match t.char_indices().nth(60) {
        Some((at, _)) => format!("{}…", &t[..at]),
        None => t.to_string(),
    }
}

pub(crate) fn bad_id(id: &str) -> Error {
    Error::Query(format!("«{}» no es un id de job válido", shown(id)))
}

pub(crate) fn not_found(id: &str) -> Error {
    Error::Query(format!(
        "no existe el job «{}» o este usuario no lo ve",
        shown(id)
    ))
}

/// A whole number id (`TASKID`, `tk_id`, `event_id`…): digits only.
pub(crate) fn int_id(id: &str) -> Result<u64> {
    let t = id.trim();
    if t.is_empty() || t.len() > 18 || !t.bytes().all(|b| b.is_ascii_digit()) {
        return Err(bad_id(id));
    }
    t.parse().map_err(|_| bad_id(id))
}

/// A name used as the id as it is (Virtuoso's event names): any text
/// without control characters, of a sane length. It's only ever bound.
pub(crate) fn name_id(id: &str) -> Result<&str> {
    let t = id.trim();
    if t.is_empty() || t.chars().count() > 1000 || t.chars().any(char::is_control) {
        return Err(bad_id(id));
    }
    Ok(t)
}

/// `"a"."b"`: a two-part name with both parts in double quotes (inner
/// quotes doubled), as Vertica's ids are made.
pub(crate) fn quote_pair(a: &str, b: &str) -> String {
    use dbine_driver::sql::{quote_ident, Quote};
    format!(
        "{}.{}",
        quote_ident(Quote::Double, a),
        quote_ident(Quote::Double, b)
    )
}

/// A two-part name inside a string a function takes (`'schema.name'`):
/// plain when both parts are plain identifiers (in capitals, where the
/// engine folds unquoted names to them), quoted otherwise.
pub(crate) fn dotted(a: &str, b: &str, folds_upper: bool) -> String {
    let plain = |s: &str| {
        let mut it = s.bytes();
        it.next()
            .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
            && it.all(|c| c.is_ascii_alphanumeric() || c == b'_' || c == b'$')
            && !(folds_upper && s.bytes().any(|c| c.is_ascii_lowercase()))
    };
    if plain(a) && plain(b) {
        format!("{a}.{b}")
    } else {
        quote_pair(a, b)
    }
}

/// The two parts of an id made by [`quote_pair`]; an error for anything
/// else (an unquoted name, a third part, text after the last quote…).
pub(crate) fn parse_pair(id: &str) -> Result<(String, String)> {
    fn part(it: &mut std::iter::Peekable<std::str::Chars>) -> Option<String> {
        if it.next()? != '"' {
            return None;
        }
        let mut s = String::new();
        loop {
            match it.next()? {
                '"' if it.peek() == Some(&'"') => {
                    it.next();
                    s.push('"');
                }
                '"' => break,
                c if c.is_control() => return None,
                c => s.push(c),
            }
        }
        (!s.is_empty() && s.chars().count() <= 256).then_some(s)
    }
    let mut it = id.trim().chars().peekable();
    let a = part(&mut it).ok_or_else(|| bad_id(id))?;
    if it.next() != Some('.') {
        return Err(bad_id(id));
    }
    let b = part(&mut it).ok_or_else(|| bad_id(id))?;
    if it.next().is_some() {
        return Err(bad_id(id));
    }
    Ok((a, b))
}

/// A whole number in a column.
pub(crate) fn int(s: Option<&str>) -> Option<i64> {
    let t = s?.trim();
    t.parse::<i64>().ok().or_else(|| {
        t.parse::<f64>()
            .ok()
            .filter(|v| v.is_finite())
            .map(|v| v as i64)
    })
}

pub(crate) fn yes(s: Option<&str>) -> bool {
    matches!(
        s.map(|v| v.trim().to_ascii_uppercase()).as_deref(),
        Some("1" | "T" | "Y" | "YES" | "TRUE" | "ON")
    )
}

/// The engine's own error number, when the driver reported one.
pub(crate) fn code(e: &Error) -> Option<i64> {
    match e {
        Error::Statement(s) => s.code.as_deref().and_then(|c| c.trim().parse().ok()),
        _ => None,
    }
}

/// Keeps `n` characters at most.
pub(crate) fn clip(s: &str, n: usize) -> String {
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
pub(crate) fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
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
pub(crate) fn parse_time(s: &str) -> Option<i64> {
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
pub(crate) fn parse_ts(s: &str) -> Option<i64> {
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

pub(crate) fn rfc3339(ms: i64) -> String {
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

/// A timestamp that may carry its offset from UTC at the end (`-00`,
/// `+05:30`, `-0300`, `Z`), as ms since the epoch in UTC. Without one it's
/// read as UTC.
fn parse_ts_utc(s: &str) -> Option<i64> {
    let local = parse_ts(s)?;
    Some(local - ts_offset(s).unwrap_or(0))
}

/// The offset from UTC at the end of a timestamp, when it has one.
fn ts_offset(s: &str) -> Option<i64> {
    // The offset follows the time, past the date's ten characters.
    let rest = s.trim().get(10..)?;
    rest.rfind(['+', '-'])
        .and_then(|i| offset_ms(rest[i..].trim()))
}

/// `+HH`, `-HH:MM` or `+HHMM` as ms; `None` for anything else.
fn offset_ms(s: &str) -> Option<i64> {
    let sign = match s.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let body = s[1..].replace(':', "");
    if !(body.len() == 2 || body.len() == 4) || !body.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let h: i64 = body[..2].parse().ok()?;
    let m: i64 = body
        .get(2..)
        .filter(|m| !m.is_empty())
        .map_or(Some(0), |m| m.parse().ok())?;
    (h <= 14 && m < 60).then_some(sign * (h * 60 + m) * 60_000)
}

/// ms since the epoch as `YYYY-MM-DD HH:MM:SS`, with no zone: how the
/// scripts write a server's local time.
pub(crate) fn plain(ms: i64) -> String {
    let iso = rfc3339(ms);
    format!("{} {}", &iso[..10], &iso[11..19])
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

    /// The server's local now, ms (as [`parse_ts`] reads it).
    pub(crate) fn now(&self) -> Option<i64> {
        self.now
    }

    /// `local` (a time of the server) is already past.
    pub(crate) fn past(&self, local: Option<&str>) -> bool {
        matches!((local.and_then(parse_ts), self.now), (Some(t), Some(n)) if t <= n)
    }

    /// Asks the server with `sql`, whose first row has the columns
    /// `LOCAL_NOW` and `UTC_NOW`. UTC when it fails.
    pub(crate) fn query(src: &mut dyn JobSource, sql: &str) -> Tz {
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

    /// A time that may carry its own offset (a TIMESTAMP WITH TIME ZONE
    /// as text), or else is the server's local time, RFC 3339 in UTC.
    fn utc_any(&self, v: Option<&str>) -> Option<String> {
        let v = v?;
        match ts_offset(v) {
            Some(_) => parse_ts_utc(v).map(rfc3339),
            None => self.utc(Some(v)),
        }
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
