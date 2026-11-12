//! MySQL's and MariaDB's Event Scheduler, from `information_schema.EVENTS`.
//!
//! An event runs one statement (or a `BEGIN … END` block) once (`AT`) or
//! every so often (`EVERY … STARTS … ENDS`). The server keeps when it last
//! started, not how it ended nor a history, and can't run one on demand.
//! `ALTER EVENT … ENABLE | DISABLE` turns it on and off, naming the
//! definer: a plain `ALTER EVENT` makes whoever runs it the event's
//! definer, and the event would then run with that user's privileges.
//!
//! `STARTS`, `ENDS`, `EXECUTE_AT` and `LAST_EXECUTED` are wall-clock times
//! of the event's zone (`TIME_ZONE`). The next run is worked out as the
//! server does it (`get_next_time` in the server's event_data_objects.cc):
//! `STARTS` while it's ahead, then the first `STARTS + k × interval` later
//! than now, stepped on the event zone's clock (months clamped to the
//! month's end) and read in UTC; past `ENDS`, none.
//!
//! A running event shows in performance_schema (MySQL; MariaDB with it
//! on) as statements of object type EVENT on a worker thread; stopping the
//! run ends that thread with `KILL`.
//!
//! OceanBase (MySQL mode) has the same events and `ALTER EVENT`, with these
//! differences: an event's times are those of the tenant's global zone
//! (see [`events_sql`]), it creates events only in the current database,
//! it has no `SHOW CREATE EVENT` (the script is rebuilt from the row), and
//! it runs each event as a DBMS_SCHEDULER job whose views say how each run
//! ended and which run now, on which session.

use super::clock::{self, Civil};
use super::{bad_id, number, prop, quoted, text, to_utc, unquote};
use crate::session::{lit, MySqlSession};
use crate::Variant;
use dbine_driver::jobs::{JobDetail, JobList, JobOutcome, JobRun, JobStep, ServerJob};
use dbine_driver::{Error, Result};
use std::collections::HashMap;

pub(super) const KIND: &str = "Event";

/// Longest schema or event name MySQL allows.
const MAX_NAME: usize = 64;

/// `{tz}` and `{now}`: the zone the event's times are read in and now on
/// its clock; `{t:COL}`: a time column on that clock (see [`events_sql`]).
const EVENTS: &str =
    "SELECT EVENT_SCHEMA AS ev_schema, EVENT_NAME AS ev_name, DEFINER AS ev_definer,
       {tz} AS ev_tz, EVENT_DEFINITION AS ev_body, EVENT_TYPE AS ev_type,
       {t:EXECUTE_AT} AS ev_at, INTERVAL_VALUE AS ev_every, INTERVAL_FIELD AS ev_unit,
       {t:STARTS} AS ev_starts, {t:ENDS} AS ev_ends, STATUS AS ev_status,
       ON_COMPLETION AS ev_completion, {t:CREATED} AS ev_created,
       {t:LAST_ALTERED} AS ev_altered, {t:LAST_EXECUTED} AS ev_last,
       EVENT_COMMENT AS ev_comment, ORIGINATOR AS ev_originator, SQL_MODE AS ev_sql_mode,
       CHARACTER_SET_CLIENT AS ev_charset, COLLATION_CONNECTION AS ev_collation,
       DATABASE_COLLATION AS ev_db_collation,
       {now} AS ev_now
  FROM information_schema.EVENTS{filter}
 ORDER BY EVENT_SCHEMA, EVENT_NAME";

/// The time columns of [`EVENTS`].
const TIME_COLUMNS: [&str; 6] = [
    "EXECUTE_AT",
    "STARTS",
    "ENDS",
    "CREATED",
    "LAST_ALTERED",
    "LAST_EXECUTED",
];

/// OceanBase's zone at `scope` (`session`, `global`): it names its `SYSTEM`
/// zone in `@@system_time_zone`, and `CONVERT_TZ` doesn't take `SYSTEM`.
fn ob_zone(scope: &str) -> String {
    format!("IF(@@{scope}.time_zone = 'SYSTEM', @@system_time_zone, @@{scope}.time_zone)")
}

/// The events query. MySQL and MariaDB give each event's times on the
/// clock of the zone it was defined in (`TIME_ZONE`). OceanBase reads the
/// times of `CREATE EVENT` on the tenant's global zone whatever the
/// session's, shows them on the session's clock and has `TIME_ZONE` always
/// read `SYSTEM`: they're read back on the global zone's clock, as they
/// were written.
fn events_sql(v: Variant, filter: &str) -> String {
    let mut sql = EVENTS.replace("{filter}", filter);
    if v == Variant::OceanBase {
        let global = ob_zone("global");
        let session = ob_zone("session");
        for col in TIME_COLUMNS {
            sql = sql.replace(
                &format!("{{t:{col}}}"),
                &format!(
                    "DATE_FORMAT(CONVERT_TZ({col}, {session}, {global}), '%Y-%m-%d %H:%i:%s')"
                ),
            );
        }
        sql.replace("{tz}", &global).replace(
            "{now}",
            &format!("CONVERT_TZ(UTC_TIMESTAMP(), '+00:00', {global})"),
        )
    } else {
        for col in TIME_COLUMNS {
            sql = sql.replace(&format!("{{t:{col}}}"), col);
        }
        sql.replace("{tz}", "TIME_ZONE")
            .replace("{now}", "CONVERT_TZ(UTC_TIMESTAMP(), '+00:00', TIME_ZONE)")
    }
}

/// OceanBase's ENDS for an event without one.
const OB_NO_END: &str = "4000-01-01";

/// The events running now: the connection id of each worker thread running
/// one (a recurring event's next run starts even while the last one goes on).
const RUNNING: &str = "SELECT DISTINCT e.OBJECT_SCHEMA AS ev_schema, e.OBJECT_NAME AS ev_name,
       t.PROCESSLIST_ID AS ev_pid
  FROM performance_schema.events_statements_current e
  JOIN performance_schema.threads t ON t.THREAD_ID = e.THREAD_ID
 WHERE e.OBJECT_TYPE = 'EVENT' AND t.PROCESSLIST_ID IS NOT NULL";

/// An event's id: `` `schema`.`event` ``.
pub(super) fn id(schema: &str, name: &str) -> String {
    format!("{}.{}", quoted(schema), quoted(name))
}

/// The schema and name in an id, or the error for one that isn't ours.
pub(super) fn event_id(id: &str) -> Result<(String, String)> {
    let bad = || bad_id(id, "evento");
    let (schema, rest) = unquote(id).ok_or_else(bad)?;
    let rest = rest.strip_prefix('.').ok_or_else(bad)?;
    let (name, rest) = unquote(rest).ok_or_else(bad)?;
    if !rest.is_empty() || schema.chars().count() > MAX_NAME || name.chars().count() > MAX_NAME {
        return Err(bad());
    }
    Ok((schema, name))
}

fn not_found(schema: &str, name: &str) -> Error {
    Error::Query(format!(
        "no existe el evento {schema}.{name} o este usuario no lo ve"
    ))
}

/// One row of `information_schema.EVENTS`.
#[derive(Debug, Clone, Default, PartialEq)]
struct Event {
    schema: String,
    name: String,
    definer: Option<String>,
    tz: String,
    body: Option<String>,
    recurring: bool,
    at: Option<String>,
    every: Option<String>,
    unit: Option<String>,
    starts: Option<String>,
    ends: Option<String>,
    status: String,
    preserve: bool,
    created: Option<String>,
    altered: Option<String>,
    last: Option<String>,
    comment: Option<String>,
    originator: Option<String>,
    sql_mode: Option<String>,
    charset: Option<String>,
    collation: Option<String>,
    db_collation: Option<String>,
    /// Now on the event zone's clock, when the server could say it.
    now: Option<String>,
}

fn event(r: &mysql_async::Row) -> Event {
    Event {
        schema: text(r, &["ev_schema"]).unwrap_or_default(),
        name: text(r, &["ev_name"]).unwrap_or_default(),
        definer: text(r, &["ev_definer"]),
        tz: text(r, &["ev_tz"]).unwrap_or_else(|| "SYSTEM".into()),
        // The body keeps its own spacing.
        body: crate::session::named(r, &["ev_body"]).filter(|b| !b.trim().is_empty()),
        recurring: text(r, &["ev_type"]).is_some_and(|t| t.eq_ignore_ascii_case("RECURRING")),
        at: text(r, &["ev_at"]),
        every: text(r, &["ev_every"]),
        unit: text(r, &["ev_unit"]),
        starts: text(r, &["ev_starts"]),
        ends: text(r, &["ev_ends"]),
        status: text(r, &["ev_status"]).unwrap_or_default(),
        preserve: text(r, &["ev_completion"]).is_some_and(|c| c.eq_ignore_ascii_case("PRESERVE")),
        created: text(r, &["ev_created"]),
        altered: text(r, &["ev_altered"]),
        last: text(r, &["ev_last"]),
        comment: text(r, &["ev_comment"]),
        originator: text(r, &["ev_originator"]),
        sql_mode: text(r, &["ev_sql_mode"]),
        charset: text(r, &["ev_charset"]),
        collation: text(r, &["ev_collation"]),
        db_collation: text(r, &["ev_db_collation"]),
        now: text(r, &["ev_now"]),
    }
}

impl Event {
    fn enabled(&self) -> bool {
        self.status.eq_ignore_ascii_case("ENABLED")
    }

    /// `SLAVESIDE_DISABLED` (`REPLICA_SIDE_DISABLED` in MySQL 8.4): created
    /// on the source, kept off on this replica.
    fn replica_side(&self) -> bool {
        self.status.to_ascii_uppercase().ends_with("SIDE_DISABLED")
    }

    /// The schedule as `CREATE EVENT` states it.
    fn schedule(&self) -> Option<String> {
        if !self.recurring {
            return self.at.as_ref().map(|at| format!("AT '{at}'"));
        }
        let mut s = format!("EVERY {} {}", self.every.as_deref()?, self.unit.as_deref()?);
        if let Some(st) = &self.starts {
            s.push_str(&format!(" STARTS '{st}'"));
        }
        if let Some(e) = &self.ends {
            s.push_str(&format!(" ENDS '{e}'"));
        }
        Some(s)
    }

    /// Now on the event zone's clock: the server's answer, or worked out
    /// for a fixed offset.
    fn now_local(&self, now_utc: Option<Civil>) -> Option<Civil> {
        match clock::fixed_offset(&self.tz) {
            Some(off) => now_utc.map(|n| n.add_seconds(off)),
            None => self.now.as_deref().and_then(Civil::parse),
        }
    }

    /// The wall-clock times the next run can be at, in order: the first of
    /// them later than now, read in UTC, is it.
    fn candidates(&self, now_local: Civil) -> Vec<Civil> {
        if !self.recurring {
            return self
                .at
                .as_deref()
                .and_then(Civil::parse)
                .into_iter()
                .collect();
        }
        let Some(start) = self.starts.as_deref().and_then(Civil::parse) else {
            return Vec::new();
        };
        let mut out = vec![start];
        match step(self.every.as_deref(), self.unit.as_deref()) {
            Some(Step::Seconds(n)) => {
                let d = now_local.seconds() - start.seconds();
                if d >= 0 {
                    let k = d - d % n + n;
                    out.push(start.add_seconds(k));
                    out.push(start.add_seconds(k + n));
                } else {
                    out.push(start.add_seconds(n));
                }
            }
            Some(Step::Months(m)) => {
                let diff = start.months_until(now_local).max(0);
                let k = diff - diff % m;
                out.push(start.add_months(k));
                out.push(start.add_months(k + m));
            }
            None => out.clear(),
        }
        out
    }
}

/// An event's interval, as the server steps it.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Step {
    Seconds(i64),
    Months(i64),
}

/// `INTERVAL_VALUE` and `INTERVAL_FIELD` as a step: `1` `HOUR`, `'1:30'`
/// `HOUR_MINUTE`, `'1-6'` `YEAR_MONTH`… A composite value with fewer parts
/// than its unit has fills from the right, as the server reads it.
fn step(value: Option<&str>, unit: Option<&str>) -> Option<Step> {
    let parts: Vec<i64> = value?
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty())
        .map(|p| p.parse().ok())
        .collect::<Option<_>>()?;
    // Each unit's parts, from the largest, in seconds or months.
    let (sizes, months): (&[i64], bool) = match unit?.to_ascii_uppercase().as_str() {
        "YEAR" => (&[12], true),
        "QUARTER" => (&[3], true),
        "MONTH" => (&[1], true),
        "YEAR_MONTH" => (&[12, 1], true),
        "WEEK" => (&[604_800], false),
        "DAY" => (&[86_400], false),
        "HOUR" => (&[3600], false),
        "MINUTE" => (&[60], false),
        "SECOND" => (&[1], false),
        "DAY_HOUR" => (&[86_400, 3600], false),
        "DAY_MINUTE" => (&[86_400, 3600, 60], false),
        "DAY_SECOND" => (&[86_400, 3600, 60, 1], false),
        "HOUR_MINUTE" => (&[3600, 60], false),
        "HOUR_SECOND" => (&[3600, 60, 1], false),
        "MINUTE_SECOND" => (&[60, 1], false),
        _ => return None,
    };
    if parts.is_empty() || parts.len() > sizes.len() {
        return None;
    }
    let total = sizes[sizes.len() - parts.len()..]
        .iter()
        .zip(&parts)
        .try_fold(0i64, |acc, (size, n)| {
            acc.checked_add(size.checked_mul(*n)?)
        })?;
    (total > 0).then_some(if months {
        Step::Months(total)
    } else {
        Step::Seconds(total)
    })
}

/// What the list needs besides the events: the scheduler's state, who is
/// asking and with which privileges, and what runs now.
#[derive(Debug, Clone, Default)]
struct Context {
    /// `@@event_scheduler`: ON, OFF or DISABLED.
    scheduler: String,
    /// `CURRENT_USER()`, as `DEFINER` reads (`user@host`).
    user: String,
    now_utc: Option<Civil>,
    /// The user's global privileges (its own and its active roles'), upper
    /// case.
    privileges: Vec<String>,
    /// Running events: (schema, name) → the connection ids of the workers
    /// running it.
    running: HashMap<(String, String), Vec<u64>>,
    /// OceanBase: its scheduler's views, when this user can read them.
    ob: Option<ObJobs>,
    /// OceanBase, and this user can't read them.
    ob_hidden: bool,
    /// OceanBase: how each event's last run ended.
    last: HashMap<(String, String), JobOutcome>,
}

impl Context {
    fn has(&self, any: &[&str]) -> bool {
        self.privileges
            .iter()
            .any(|p| p == "ALL" || p == "ALL PRIVILEGES" || any.contains(&p.as_str()))
    }

    fn scheduler_on(&self) -> bool {
        self.scheduler.eq_ignore_ascii_case("ON") || self.scheduler == "1"
    }

    /// `ALTER DEFINER = <someone else>` is allowed.
    fn sets_any_definer(&self) -> bool {
        self.has(&["SUPER", "SET_USER_ID", "SET_ANY_DEFINER", "SET USER"])
    }

    /// `KILL` reaches other users' threads.
    fn kills_any(&self) -> bool {
        self.has(&["SUPER", "CONNECTION_ADMIN", "CONNECTION ADMIN"])
    }

    fn owns(&self, e: &Event) -> bool {
        e.definer.as_deref() == Some(self.user.as_str())
    }
}

/// The global privileges a `SHOW GRANTS` line gives (`GRANT EVENT, SUPER ON
/// *.* TO …`, `GRANT SET_USER_ID,CONNECTION_ADMIN ON *.* TO …`), upper case.
fn global_privileges(grant: &str) -> Vec<String> {
    let g = grant.trim();
    let Some(rest) = g
        .get(..6)
        .filter(|p| p.eq_ignore_ascii_case("GRANT "))
        .map(|_| &g[6..])
    else {
        return Vec::new();
    };
    let Some(i) = rest.to_ascii_uppercase().find(" ON *.* TO ") else {
        return Vec::new();
    };
    rest[..i]
        .split(',')
        .map(|p| p.trim().to_ascii_uppercase())
        .filter(|p| !p.is_empty())
        .collect()
}

/// The current user's global privileges. `SHOW GRANTS` lists the user's own
/// and, on MySQL, those of its active roles; MariaDB lists its current
/// role's apart.
async fn privileges(s: &mut MySqlSession) -> Vec<String> {
    let mut grants: Vec<String> = s
        .optional_rows("SHOW GRANTS")
        .await
        .iter()
        .filter_map(|r| crate::session::at(r, 0))
        .collect();
    if s.variant == Variant::MariaDb {
        let role = s
            .optional_rows("SELECT CURRENT_ROLE()")
            .await
            .first()
            .and_then(|r| crate::session::at(r, 0));
        if role.is_some_and(|r| !r.is_empty()) {
            grants.extend(
                s.optional_rows("SHOW GRANTS FOR CURRENT_ROLE")
                    .await
                    .iter()
                    .filter_map(|r| crate::session::at(r, 0)),
            );
        }
    }
    let mut out: Vec<String> = grants.iter().flat_map(|g| global_privileges(g)).collect();
    out.sort();
    out.dedup();
    out
}

/// The events running now, with their workers' connection ids: from
/// performance_schema, or from OceanBase's scheduler (`ob`).
async fn running(s: &mut MySqlSession, ob: Option<&ObJobs>) -> HashMap<(String, String), Vec<u64>> {
    let mut out: HashMap<(String, String), Vec<u64>> = HashMap::new();
    let mut add = |key: (String, String), pid: u64| {
        let pids = out.entry(key).or_default();
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    };
    if s.variant == Variant::OceanBase {
        let Some(ob) = ob else {
            return out;
        };
        for r in s.optional_rows(OB_RUNNING).await.iter() {
            if let (Some(key), Some(pid)) = (
                text(r, &["JOB_NAME"]).and_then(|j| ob.event(&j)),
                number(r, &["SESSION_ID"]),
            ) {
                add(key, pid);
            }
        }
        return out;
    }
    for r in s.optional_rows(RUNNING).await.iter() {
        if let (Some(schema), Some(name), Some(pid)) = (
            text(r, &["ev_schema"]),
            text(r, &["ev_name"]),
            number(r, &["ev_pid"]),
        ) {
            add((schema, name), pid);
        }
    }
    out
}

// ---- OceanBase's scheduler ----

/// OceanBase runs each event as a DBMS_SCHEDULER job of the class
/// MYSQL_EVENT_JOB_CLASS named `<database id>.<event>`: its views say which
/// runs go on (and their session) and how each one ended, though not when
/// it started.
#[derive(Debug, Clone, Default)]
struct ObJobs {
    /// Database id → name.
    dbs: HashMap<String, String>,
}

const OB_DATABASES: &str = "SELECT database_id, database_name FROM oceanbase.__all_database";
const OB_RUNNING: &str = "SELECT JOB_NAME, SESSION_ID FROM oceanbase.DBA_SCHEDULER_RUNNING_JOBS";
const OB_LAST: &str = "SELECT JOB_NAME, CODE, MESSAGE FROM (SELECT JOB_NAME, CODE, MESSAGE, ROW_NUMBER() OVER (PARTITION BY JOB_NAME ORDER BY TIME DESC) AS dbine_rn FROM oceanbase.DBA_SCHEDULER_JOB_RUN_DETAILS WHERE JOB_CLASS = 'MYSQL_EVENT_JOB_CLASS') x WHERE dbine_rn = 1";

impl ObJobs {
    /// The scheduler job of an event.
    fn job_name(&self, schema: &str, name: &str) -> Option<String> {
        self.dbs
            .iter()
            .find(|(_, n)| n.as_str() == schema)
            .map(|(id, _)| format!("{id}.{name}"))
    }

    /// The event a scheduler job runs.
    fn event(&self, job: &str) -> Option<(String, String)> {
        let (id, name) = job.split_once('.')?;
        Some((self.dbs.get(id)?.clone(), name.to_string()))
    }
}

/// The databases the scheduler's job names refer to, or `None` when this
/// user may not read them.
async fn ob_databases(s: &mut MySqlSession) -> Option<ObJobs> {
    let dbs: HashMap<String, String> = s
        .optional_rows(OB_DATABASES)
        .await
        .iter()
        .filter_map(|r| Some((text(r, &["database_id"])?, text(r, &["database_name"])?)))
        .collect();
    (!dbs.is_empty()).then_some(ObJobs { dbs })
}

/// The scheduler's views and how each event's last run ended, or `None`
/// when this user may not read them.
async fn ob_jobs(s: &mut MySqlSession) -> Option<(ObJobs, HashMap<(String, String), JobOutcome>)> {
    let ob = ob_databases(s).await?;
    let rows = match s.rows(OB_LAST).await {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("OceanBase scheduler runs: {e}");
            return None;
        }
    };
    let last = rows
        .iter()
        .filter_map(|r| {
            let key = ob.event(&text(r, &["JOB_NAME"])?)?;
            Some((
                key,
                ob_outcome(&ob_code(r), text(r, &["MESSAGE"]).as_deref()),
            ))
        })
        .collect();
    Some((ob, last))
}

fn ob_code(r: &mysql_async::Row) -> String {
    text(r, &["CODE"]).unwrap_or_else(|| "0".into())
}

/// How a run ended, from its `CODE` (0 or the error's, negative) and
/// `MESSAGE`.
fn ob_outcome(code: &str, message: Option<&str>) -> JobOutcome {
    let m = message.unwrap_or("").to_ascii_lowercase();
    match code.trim() {
        "0" if m.contains("missed") => JobOutcome::Skipped,
        "0" => JobOutcome::Succeeded,
        // OB_TIMEOUT: the run hit its time limit.
        "-4012" => JobOutcome::Canceled,
        _ if m.contains("interrupt") || m.contains("kill") => JobOutcome::Canceled,
        _ => JobOutcome::Failed,
    }
}

/// An OceanBase event's runs, newest first.
async fn ob_runs(
    s: &mut MySqlSession,
    schema: &str,
    name: &str,
    limit: usize,
) -> Result<Vec<JobRun>> {
    let (ob, _) = ob_jobs(s).await.ok_or_else(|| {
        Error::Unsupported("este usuario no puede ver el historial de los eventos".into())
    })?;
    let job = ob
        .job_name(schema, name)
        .ok_or_else(|| not_found(schema, name))?;
    let rows = s
        .rows(&format!(
            "SELECT CODE, MESSAGE FROM oceanbase.DBA_SCHEDULER_JOB_RUN_DETAILS WHERE JOB_NAME = {} ORDER BY TIME DESC LIMIT {limit}",
            lit(&job)
        ))
        .await?;
    Ok(rows.iter().map(ob_run).collect())
}

fn ob_run(r: &mysql_async::Row) -> JobRun {
    let message = text(r, &["MESSAGE"]);
    let outcome = ob_outcome(&ob_code(r), message.as_deref());
    JobRun {
        id: None,
        // The scheduler records when a run was logged, not when it started.
        started: None,
        duration_ms: None,
        outcome,
        trigger: Some("programada".into()),
        message: message.filter(|_| outcome != JobOutcome::Succeeded),
        steps: Vec::new(),
    }
}

async fn context(s: &mut MySqlSession) -> Result<Context> {
    let rows = s
        .rows("SELECT @@global.event_scheduler AS ev_scheduler, CURRENT_USER() AS ev_user, UTC_TIMESTAMP() AS ev_utc")
        .await?;
    let row = rows.first();
    let user = row.and_then(|r| text(r, &["ev_user"])).unwrap_or_default();
    let privileges = privileges(s).await;
    let (ob, last) = match s.variant {
        Variant::OceanBase => match ob_jobs(s).await {
            Some((ob, last)) => (Some(ob), last),
            None => (None, HashMap::new()),
        },
        _ => (None, HashMap::new()),
    };
    let running = running(s, ob.as_ref()).await;
    Ok(Context {
        ob_hidden: s.variant == Variant::OceanBase && ob.is_none(),
        ob,
        last,
        running,
        scheduler: row
            .and_then(|r| text(r, &["ev_scheduler"]))
            .unwrap_or_default(),
        now_utc: row
            .and_then(|r| text(r, &["ev_utc"]))
            .and_then(|v| Civil::parse(&v)),
        user,
        privileges,
    })
}

async fn load(s: &mut MySqlSession, only: Option<(&str, &str)>) -> Result<Vec<Event>> {
    let filter = match only {
        Some((schema, name)) => format!(
            "\n WHERE EVENT_SCHEMA = {} AND EVENT_NAME = {}",
            lit(schema),
            lit(name)
        ),
        None => String::new(),
    };
    let rows = s.rows(&events_sql(s.variant, &filter)).await?;
    let ob = s.variant == Variant::OceanBase;
    Ok(rows
        .iter()
        .map(event)
        .filter(|e| !e.name.is_empty())
        .map(|mut e| {
            if ob && e.ends.as_deref().is_some_and(|t| t.starts_with(OB_NO_END)) {
                e.ends = None;
            }
            e
        })
        .collect())
}

/// The UTC times the list shows: when each event last started, its next
/// run and its end. `missing` is set when the server couldn't convert a
/// named zone's time (its zone tables aren't loaded).
struct Times {
    utc: HashMap<(Civil, String), Civil>,
    missing: bool,
}

async fn times(s: &mut MySqlSession, events: &[Event], ctx: &Context) -> Times {
    let mut want = Vec::new();
    for e in events {
        let mut add = |v: Option<&str>| {
            if let Some(t) = v.and_then(Civil::parse) {
                want.push((t, e.tz.clone()));
            }
        };
        add(e.last.as_deref());
        add(e.ends.as_deref());
        if let Some(now) = e.now_local(ctx.now_utc) {
            for c in e.candidates(now) {
                want.push((c, e.tz.clone()));
            }
        }
    }
    let utc = to_utc(s, &want, true).await;
    let missing = want.iter().any(|(t, tz)| {
        clock::fixed_offset(tz).is_none()
            && t.in_timestamp_range()
            && !utc.contains_key(&(*t, tz.clone()))
    }) || events
        .iter()
        .any(|e| e.now.is_none() && clock::fixed_offset(&e.tz).is_none());
    Times { utc, missing }
}

impl Times {
    fn get(&self, e: &Event, local: Option<&str>) -> Option<Civil> {
        let t = Civil::parse(local?)?;
        self.utc.get(&(t, e.tz.clone())).copied()
    }

    /// The event's next start in UTC, when it's certain.
    fn next(&self, e: &Event, ctx: &Context) -> Option<Civil> {
        if !ctx.scheduler_on() || !e.enabled() {
            return None;
        }
        let now = ctx.now_utc?;
        let mut next = None;
        for c in e.candidates(e.now_local(Some(now))?) {
            match self.utc.get(&(c, e.tz.clone())) {
                Some(u) if *u > now => {
                    next = Some(*u);
                    break;
                }
                Some(_) => {}
                // A step the server couldn't read: no guess.
                None => return None,
            }
        }
        let next = next?;
        match &e.ends {
            Some(_) => (next <= self.get(e, e.ends.as_deref())?).then_some(next),
            None => Some(next),
        }
    }
}

fn job(e: &Event, ctx: &Context, t: &Times) -> ServerJob {
    let running = ctx
        .running
        .get(&(e.schema.clone(), e.name.clone()))
        .is_some_and(|pids| !pids.is_empty());
    let owns = ctx.owns(e);
    ServerJob {
        id: id(&e.schema, &e.name),
        name: e.name.clone(),
        kind: KIND.into(),
        group: Some(e.schema.clone()),
        description: e.comment.clone(),
        owner: e.definer.clone(),
        enabled: e.enabled(),
        schedule: e.schedule(),
        running,
        last_outcome: ctx.last.get(&(e.schema.clone(), e.name.clone())).copied(),
        last_started: t.get(e, e.last.as_deref()).map(Civil::utc),
        last_duration_ms: None,
        next_run: t.next(e, ctx).map(Civil::utc),
        can_run: false,
        can_stop: running && (owns || ctx.kills_any()),
        can_enable: owns || ctx.sets_any_definer(),
        has_history: ctx.ob.is_some(),
        has_script: true,
    }
}

fn notes(ctx: &Context, t: &Times) -> Vec<String> {
    let mut notes = Vec::new();
    match ctx.scheduler.to_ascii_uppercase().as_str() {
        "ON" | "1" => {}
        "DISABLED" => notes.push(
            "El programador de eventos está deshabilitado en el servidor (event_scheduler = DISABLED): los eventos no se ejecutan."
                .into(),
        ),
        _ => notes.push(
            "El programador de eventos del servidor está apagado (event_scheduler = OFF): los eventos no se ejecutan."
                .into(),
        ),
    }
    if !ctx.has(&["EVENT"]) {
        notes.push(
            "Se ven solo los eventos de las bases en las que este usuario tiene el privilegio EVENT."
                .into(),
        );
    }
    if t.missing {
        notes.push(
            "El servidor no tiene cargadas las zonas horarias de algunos eventos: no se muestran sus horas."
                .into(),
        );
    }
    if ctx.ob_hidden {
        notes.push(
            "Este usuario no puede leer las vistas del programador de OceanBase: no se muestran el historial ni las ejecuciones en curso."
                .into(),
        );
    }
    notes
}

pub(super) async fn list(s: &mut MySqlSession) -> Result<JobList> {
    let ctx = context(s).await?;
    let events = load(s, None).await?;
    let t = times(s, &events, &ctx).await;
    Ok(JobList {
        jobs: events.iter().map(|e| job(e, &ctx, &t)).collect(),
        notes: notes(&ctx, &t),
    })
}

/// One event, with what list shows about it.
async fn one(s: &mut MySqlSession, schema: &str, name: &str) -> Result<(Event, Context, Times)> {
    let ctx = context(s).await?;
    let e = load(s, Some((schema, name)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(schema, name))?;
    let t = times(s, std::slice::from_ref(&e), &ctx).await;
    Ok((e, ctx, t))
}

pub(super) async fn detail(s: &mut MySqlSession, id: &str) -> Result<JobDetail> {
    let (schema, name) = event_id(id)?;
    let (e, ctx, t) = one(s, &schema, &name).await?;
    let mut properties = properties(&e);
    // What performance_schema counted since the server started.
    let stats = s
        .optional_rows(&format!(
            "SELECT COUNT_STAR AS ev_count, AVG_TIMER_WAIT DIV 1000000000 AS ev_avg_ms,
                    MAX_TIMER_WAIT DIV 1000000000 AS ev_max_ms, SUM_ERRORS AS ev_errors
               FROM performance_schema.events_statements_summary_by_program
              WHERE OBJECT_TYPE = 'EVENT' AND OBJECT_SCHEMA = {} AND OBJECT_NAME = {}",
            lit(&schema),
            lit(&name)
        ))
        .await;
    if let Some(r) = stats.first() {
        properties.extend(statistics(
            number(r, &["ev_count"]),
            number(r, &["ev_avg_ms"]),
            number(r, &["ev_max_ms"]),
            number(r, &["ev_errors"]),
        ));
    }
    Ok(JobDetail {
        job: job(&e, &ctx, &t),
        steps: vec![JobStep {
            name: e.name.clone(),
            kind: Some("SQL".into()),
            target: None,
            command: e.body.clone(),
        }],
        schedules: Vec::new(),
        properties,
    })
}

fn properties(e: &Event) -> Vec<dbine_driver::PropertyInfo> {
    let mut p = vec![
        prop(
            "",
            "Tipo",
            if e.recurring {
                "Recurrente"
            } else {
                "Una sola vez"
            },
        ),
        prop("", "Zona horaria", e.tz.clone()),
        prop(
            "",
            "Al terminar",
            if e.preserve {
                "Se conserva"
            } else {
                "Se borra"
            },
        ),
    ];
    if e.replica_side() {
        p.push(prop("", "Estado", "Deshabilitado en la réplica"));
    }
    for (label, v) in [
        ("Creado", &e.created),
        ("Modificado", &e.altered),
        ("Servidor de origen", &e.originator),
    ] {
        if let Some(v) = v {
            p.push(prop("", label, v.clone()));
        }
    }
    for (label, v) in [
        ("Modo SQL", &e.sql_mode),
        ("Juego de caracteres del cliente", &e.charset),
        ("Intercalación de la conexión", &e.collation),
        ("Intercalación de la base", &e.db_collation),
    ] {
        if let Some(v) = v {
            p.push(prop("Contexto", label, v.clone()));
        }
    }
    p
}

fn statistics(
    count: Option<u64>,
    avg_ms: Option<u64>,
    max_ms: Option<u64>,
    errors: Option<u64>,
) -> Vec<dbine_driver::PropertyInfo> {
    let Some(count) = count else {
        return Vec::new();
    };
    const GROUP: &str = "Estadísticas";
    let mut p = vec![prop(
        GROUP,
        "Ejecuciones desde el arranque",
        count.to_string(),
    )];
    if count > 0 {
        for (label, v) in [("Duración media", avg_ms), ("Duración máxima", max_ms)] {
            if let Some(ms) = v {
                p.push(prop(GROUP, label, format!("{ms} ms")));
            }
        }
        if let Some(n) = errors {
            p.push(prop(GROUP, "Errores", n.to_string()));
        }
    }
    p
}

pub(super) async fn stop(s: &mut MySqlSession, id: &str) -> Result<()> {
    let (schema, name) = event_id(id)?;
    let ob = match s.variant {
        Variant::OceanBase => ob_databases(s).await,
        _ => None,
    };
    let running = running(s, ob.as_ref()).await;
    let own = u64::from(s.conn.id());
    let pids = running
        .get(&(schema.clone(), name.clone()))
        .filter(|pids| !pids.is_empty())
        .ok_or_else(|| Error::State(format!("el evento {schema}.{name} no se está ejecutando")))?;
    for pid in kill_targets(pids, own)? {
        match s.rows(&format!("KILL {pid}")).await {
            // That run ended meanwhile (ER_NO_SUCH_THREAD).
            Err(e)
                if e.to_string()
                    .to_ascii_lowercase()
                    .contains("unknown thread") => {}
            other => {
                other?;
            }
        }
    }
    Ok(())
}

/// An event's runs: OceanBase's scheduler logs them; MySQL and MariaDB keep
/// none.
pub(super) async fn runs(s: &mut MySqlSession, id: &str, limit: usize) -> Result<Vec<JobRun>> {
    let (schema, name) = event_id(id)?;
    if s.variant != Variant::OceanBase {
        return Err(Error::Unsupported(
            "el servidor no guarda el historial de sus eventos".into(),
        ));
    }
    if limit == 0 {
        return Ok(Vec::new());
    }
    ob_runs(s, &schema, &name, limit).await
}

/// The workers to end: every one running the event but the connection DBine
/// itself queries with.
fn kill_targets(pids: &[u64], own: u64) -> Result<Vec<u64>> {
    let out: Vec<u64> = pids.iter().copied().filter(|&p| p != own).collect();
    if out.is_empty() {
        return Err(Error::State(
            "esa es la sesión con la que DBine está consultando".into(),
        ));
    }
    Ok(out)
}

/// `DEFINER` (`user@host`) as `ALTER DEFINER =` takes it.
fn definer_clause(definer: &str) -> String {
    match definer.rsplit_once('@') {
        Some((u, h)) => format!("{}@{}", quoted(u), quoted(h)),
        // A MariaDB role.
        None => quoted(definer),
    }
}

fn alter_sql(e: &Event, enabled: bool) -> Result<String> {
    let definer = e
        .definer
        .as_deref()
        .filter(|d| !d.is_empty())
        .ok_or_else(|| {
            Error::Query(format!(
                "no se pudo leer el definidor del evento {}.{}",
                e.schema, e.name
            ))
        })?;
    Ok(format!(
        "ALTER DEFINER = {} EVENT {} {}",
        definer_clause(definer),
        id(&e.schema, &e.name),
        if enabled { "ENABLE" } else { "DISABLE" }
    ))
}

pub(super) async fn set_enabled(s: &mut MySqlSession, id: &str, enabled: bool) -> Result<()> {
    let (schema, name) = event_id(id)?;
    let e = load(s, Some((&schema, &name)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(&schema, &name))?;
    let sql = alter_sql(&e, enabled)?;
    if s.variant != Variant::OceanBase {
        return s.rows(&sql).await.map(|_| ());
    }
    // OceanBase alters events only in the current database: into the
    // event's for the ALTER, then back.
    let current = s
        .rows("SELECT DATABASE()")
        .await?
        .first()
        .and_then(|r| crate::session::at(r, 0));
    s.rows(&format!("USE {}", quoted(&schema))).await?;
    let done = s.rows(&sql).await.map(|_| ());
    if let Some(db) = current.filter(|db| *db != schema) {
        s.rows(&format!("USE {}", quoted(&db))).await?;
    }
    done
}

pub(super) async fn script(s: &mut MySqlSession, id: &str) -> Result<String> {
    let (schema, name) = event_id(id)?;
    if s.variant == Variant::OceanBase {
        // No SHOW CREATE EVENT: rebuilt from its row, its times on the
        // global zone's clock, where CREATE EVENT reads them.
        let e = load(s, Some((&schema, &name)))
            .await?
            .into_iter()
            .next()
            .ok_or_else(|| not_found(&schema, &name))?;
        let create = create_from_row(&e).ok_or_else(|| not_found(&schema, &name))?;
        // OceanBase creates events only in the current database.
        return Ok(wrapped(
            format!("USE {};\n", quoted(&schema)),
            &create,
            e.sql_mode.as_deref().unwrap_or(""),
            &e.tz,
        ));
    }
    let rows = s
        .rows(&format!(
            "SHOW CREATE EVENT {}",
            super::events::id(&schema, &name)
        ))
        .await?;
    let row = rows.first().ok_or_else(|| not_found(&schema, &name))?;
    let create = crate::session::named(row, &["Create Event"])
        .filter(|c| !c.trim().is_empty())
        .ok_or_else(|| not_found(&schema, &name))?;
    let sql_mode = text(row, &["sql_mode"]).unwrap_or_default();
    let tz = text(row, &["time_zone"]).unwrap_or_else(|| "SYSTEM".into());
    Ok(script_text(&schema, &name, &create, &sql_mode, &tz))
}

/// The script that recreates an event: its `CREATE` with the name
/// qualified, under the SQL mode and zone it was defined with (its times
/// are that zone's), as mysqldump writes it.
fn script_text(schema: &str, name: &str, create: &str, sql_mode: &str, tz: &str) -> String {
    let bare = format!(" EVENT {} ", quoted(name));
    let (out, create) = match create.find(&bare) {
        Some(i) => (
            String::new(),
            format!(
                "{} EVENT {} {}",
                &create[..i],
                id(schema, name),
                &create[i + bare.len()..]
            ),
        ),
        None => (format!("USE {};\n", quoted(schema)), create.to_string()),
    };
    wrapped(out, &create, sql_mode, tz)
}

/// The `CREATE EVENT` of an event's row, its name qualified (OceanBase has
/// no `SHOW CREATE EVENT`). Its times are those of the row, read on the
/// clock of `e.tz`.
fn create_from_row(e: &Event) -> Option<String> {
    let schedule = if e.recurring {
        // A composite interval ('1:30' HOUR_MINUTE) goes as a string.
        let every = e.every.as_deref()?.trim_matches('\'');
        let every = if every.bytes().all(|b| b.is_ascii_digit()) {
            every.to_string()
        } else {
            lit(every)
        };
        let unit = e.unit.as_deref()?;
        if !unit.bytes().all(|b| b.is_ascii_alphabetic() || b == b'_') {
            return None;
        }
        let mut s = format!("EVERY {every} {unit}");
        if let Some(st) = &e.starts {
            s.push_str(&format!(" STARTS {}", lit(st)));
        }
        if let Some(en) = &e.ends {
            s.push_str(&format!(" ENDS {}", lit(en)));
        }
        s
    } else {
        format!("AT {}", lit(e.at.as_deref()?))
    };
    let mut out = String::from("CREATE");
    if let Some(d) = e.definer.as_deref().filter(|d| !d.is_empty()) {
        out.push_str(&format!(" DEFINER = {}", definer_clause(d)));
    }
    out.push_str(&format!(
        " EVENT {}\nON SCHEDULE {schedule}\nON COMPLETION {}\n{}",
        id(&e.schema, &e.name),
        if e.preserve {
            "PRESERVE"
        } else {
            "NOT PRESERVE"
        },
        if e.enabled() {
            "ENABLE"
        } else if e.replica_side() {
            "DISABLE ON SLAVE"
        } else {
            "DISABLE"
        }
    ));
    if let Some(c) = &e.comment {
        out.push_str(&format!("\nCOMMENT {}", lit(c)));
    }
    let body = e.body.as_deref()?.trim().trim_end_matches(';').trim_end();
    // OceanBase keeps a BEGIN … END body as its statements alone.
    let block = body
        .get(..5)
        .is_some_and(|b| b.eq_ignore_ascii_case("BEGIN"));
    if body.contains(';') && !block {
        out.push_str(&format!("\nDO BEGIN {body}; END"));
    } else {
        out.push_str(&format!("\nDO {body}"));
    }
    Some(out)
}

/// `out` then a `CREATE EVENT` run under the SQL mode and zone it was
/// defined with, the session's own put back after it; between `DELIMITER`s
/// when its body has more than one statement.
fn wrapped(mut out: String, create: &str, sql_mode: &str, tz: &str) -> String {
    out.push_str("SET @saved_sql_mode = @@sql_mode, @saved_time_zone = @@time_zone;\n");
    out.push_str(&format!(
        "SET sql_mode = {}, time_zone = {};\n",
        lit(sql_mode),
        lit(tz)
    ));
    let create = create.trim_end().trim_end_matches(';');
    if create.contains(';') {
        let delim = ["$$", "//", ";;", "$$$$"]
            .into_iter()
            .find(|d| !create.contains(d))
            .unwrap_or("$$$$");
        out.push_str(&format!(
            "DELIMITER {delim}\n{create}{delim}\nDELIMITER ;\n"
        ));
    } else {
        out.push_str(&format!("{create};\n"));
    }
    out.push_str("SET sql_mode = @saved_sql_mode, time_zone = @saved_time_zone;\n");
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn c(s: &str) -> Civil {
        Civil::parse(s).unwrap()
    }

    fn recurring(every: &str, unit: &str, starts: &str) -> Event {
        Event {
            schema: "db".into(),
            name: "ev".into(),
            tz: "SYSTEM".into(),
            recurring: true,
            every: Some(every.into()),
            unit: Some(unit.into()),
            starts: Some(starts.into()),
            status: "ENABLED".into(),
            ..Default::default()
        }
    }

    #[test]
    fn ids_round_trip_and_foreign_ones_are_refused() {
        let i = id("my.db", "ev`1");
        assert_eq!(i, "`my.db`.`ev``1`");
        assert_eq!(event_id(&i).unwrap(), ("my.db".into(), "ev`1".into()));
        for bad in [
            "db.ev",
            "`db`",
            "`db`.`ev`; DROP DATABASE db",
            "`db`.`ev` ",
            "`db`.ev",
            "`db`..`ev`",
            "`db`.``",
            "' OR 1=1 --",
        ] {
            assert!(event_id(bad).is_err(), "{bad}");
        }
        let long = id(&"x".repeat(65), "ev");
        assert!(event_id(&long).is_err());
    }

    #[test]
    fn intervals_read_as_the_server_steps_them() {
        assert_eq!(step(Some("1"), Some("HOUR")), Some(Step::Seconds(3600)));
        assert_eq!(
            step(Some("'1:30'"), Some("HOUR_MINUTE")),
            Some(Step::Seconds(5400))
        );
        assert_eq!(
            step(Some("'30'"), Some("HOUR_MINUTE")),
            Some(Step::Seconds(1800))
        );
        assert_eq!(
            step(Some("'1 02:03:04'"), Some("DAY_SECOND")),
            Some(Step::Seconds(86_400 + 7200 + 180 + 4))
        );
        assert_eq!(
            step(Some("2"), Some("WEEK")),
            Some(Step::Seconds(1_209_600))
        );
        assert_eq!(step(Some("1"), Some("QUARTER")), Some(Step::Months(3)));
        assert_eq!(
            step(Some("'1-6'"), Some("YEAR_MONTH")),
            Some(Step::Months(18))
        );
        assert_eq!(step(Some("2"), Some("YEAR")), Some(Step::Months(24)));
        assert_eq!(step(Some("0"), Some("DAY")), None);
        assert_eq!(step(Some("1"), Some("MICROSECOND")), None);
        assert_eq!(step(Some("'1:2:3:4'"), Some("HOUR_MINUTE")), None);
        assert_eq!(step(None, Some("DAY")), None);
    }

    #[test]
    fn next_run_follows_the_starts_series() {
        // Every minute at :10, now 09:55:20 → 09:56:10, then 09:57:10.
        let e = recurring("1", "MINUTE", "2026-01-01 00:00:10");
        assert_eq!(
            e.candidates(c("2026-11-12 09:55:20")),
            vec![
                c("2026-01-01 00:00:10"),
                c("2026-11-12 09:56:10"),
                c("2026-11-12 09:57:10")
            ]
        );
        // Exactly on a step: the next one.
        assert_eq!(
            e.candidates(c("2026-11-12 09:56:10"))[1],
            c("2026-11-12 09:57:10")
        );
        // Monthly from January 31st: clamped per step from STARTS.
        let m = recurring("1", "MONTH", "2026-01-31 03:00:00");
        assert_eq!(
            m.candidates(c("2026-02-10 00:00:00"))[1..],
            [c("2026-02-28 03:00:00"), c("2026-03-31 03:00:00")]
        );
        assert_eq!(
            m.candidates(c("2026-03-31 04:00:00"))[1..],
            [c("2026-03-31 03:00:00"), c("2026-04-30 03:00:00")]
        );
        // Every three months, now in the second month of a period.
        let q = recurring("1", "QUARTER", "2026-01-15 00:00:00");
        assert_eq!(
            q.candidates(c("2026-05-20 00:00:00"))[1..],
            [c("2026-04-15 00:00:00"), c("2026-07-15 00:00:00")]
        );
        // STARTS ahead of now: STARTS itself comes first.
        let f = recurring("1", "DAY", "2027-01-01 03:00:00");
        assert_eq!(
            f.candidates(c("2026-11-12 00:00:00"))[0],
            c("2027-01-01 03:00:00")
        );
        // One-time: its AT.
        let once = Event {
            at: Some("2027-01-01 03:00:00".into()),
            ..Default::default()
        };
        assert_eq!(
            once.candidates(c("2026-11-12 00:00:00")),
            vec![c("2027-01-01 03:00:00")]
        );
    }

    fn times(pairs: &[(&str, &str)], tz: &str) -> Times {
        Times {
            utc: pairs
                .iter()
                .map(|(l, u)| ((c(l), tz.to_string()), c(u)))
                .collect(),
            missing: false,
        }
    }

    fn ctx(now_utc: &str) -> Context {
        Context {
            scheduler: "ON".into(),
            user: "root@localhost".into(),
            now_utc: Some(c(now_utc)),
            ..Default::default()
        }
    }

    #[test]
    fn next_run_is_the_first_step_after_now_in_utc() {
        let mut e = recurring("1", "HOUR", "2026-01-01 00:30:00");
        e.tz = "Europe/Madrid".into();
        e.now = Some("2026-11-12 10:05:00".into());
        let t = times(
            &[
                ("2026-01-01 00:30:00", "2025-12-31 23:30:00"),
                ("2026-11-12 10:30:00", "2026-11-12 09:30:00"),
                ("2026-11-12 11:30:00", "2026-11-12 10:30:00"),
            ],
            "Europe/Madrid",
        );
        let cx = ctx("2026-11-12 09:05:00");
        assert_eq!(t.next(&e, &cx), Some(c("2026-11-12 09:30:00")));
        // Off, disabled or the scheduler stopped: none.
        let mut off = cx.clone();
        off.scheduler = "OFF".into();
        assert_eq!(t.next(&e, &off), None);
        let mut disabled = e.clone();
        disabled.status = "DISABLED".into();
        assert_eq!(t.next(&disabled, &cx), None);
        // A step the server didn't convert: no guess.
        let partial = times(
            &[("2026-01-01 00:30:00", "2025-12-31 23:30:00")],
            "Europe/Madrid",
        );
        assert_eq!(partial.next(&e, &cx), None);
        // Past ENDS: none.
        let mut ending = e.clone();
        ending.ends = Some("2026-11-12 10:15:00".into());
        let mut t2 = times(
            &[
                ("2026-01-01 00:30:00", "2025-12-31 23:30:00"),
                ("2026-11-12 10:30:00", "2026-11-12 09:30:00"),
                ("2026-11-12 10:15:00", "2026-11-12 09:15:00"),
            ],
            "Europe/Madrid",
        );
        assert_eq!(t2.next(&ending, &cx), None);
        t2.utc.insert(
            (c("2026-11-12 12:00:00"), "Europe/Madrid".into()),
            c("2026-11-12 11:00:00"),
        );
        ending.ends = Some("2026-11-12 12:00:00".into());
        assert_eq!(t2.next(&ending, &cx), Some(c("2026-11-12 09:30:00")));
    }

    #[test]
    fn fixed_offsets_need_no_server() {
        let mut e = recurring("1", "DAY", "2026-01-01 03:00:00");
        e.tz = "-03:00".into();
        let cx = ctx("2026-11-12 12:00:00");
        // 09:00 local; next 03:00 local tomorrow = 06:00 UTC.
        assert_eq!(e.now_local(cx.now_utc), Some(c("2026-11-12 09:00:00")));
        let cands = e.candidates(c("2026-11-12 09:00:00"));
        let t = Times {
            utc: cands
                .iter()
                .map(|l| ((*l, e.tz.clone()), clock::to_utc(*l, -10_800)))
                .collect(),
            missing: false,
        };
        assert_eq!(
            t.next(&e, &cx).map(Civil::utc).as_deref(),
            Some("2026-11-13T06:00:00Z")
        );
    }

    #[test]
    fn rows_become_jobs() {
        let e = Event {
            schema: "db".into(),
            name: "nightly".into(),
            definer: Some("app@%".into()),
            tz: "+00:00".into(),
            body: Some("DELETE FROM t".into()),
            recurring: true,
            every: Some("1".into()),
            unit: Some("DAY".into()),
            starts: Some("2026-01-01 03:00:00".into()),
            ends: Some("2030-01-01 00:00:00".into()),
            status: "ENABLED".into(),
            last: Some("2026-11-12 03:00:00".into()),
            comment: Some("purga".into()),
            ..Default::default()
        };
        let mut cx = ctx("2026-11-12 12:00:00");
        cx.running
            .insert(("db".into(), "nightly".into()), vec![42, 43]);
        let utc = [
            "2026-11-12 03:00:00",
            "2026-01-01 03:00:00",
            "2026-11-13 03:00:00",
            "2026-11-14 03:00:00",
            "2030-01-01 00:00:00",
        ]
        .iter()
        .map(|l| ((c(l), "+00:00".to_string()), c(l)))
        .collect();
        let t = Times {
            utc,
            missing: false,
        };
        let j = job(&e, &cx, &t);
        assert_eq!(j.id, "`db`.`nightly`");
        assert_eq!(j.kind, "Event");
        assert_eq!(j.group.as_deref(), Some("db"));
        assert_eq!(j.owner.as_deref(), Some("app@%"));
        assert_eq!(j.description.as_deref(), Some("purga"));
        assert_eq!(
            j.schedule.as_deref(),
            Some("EVERY 1 DAY STARTS '2026-01-01 03:00:00' ENDS '2030-01-01 00:00:00'")
        );
        assert!(j.enabled && j.running && j.has_script);
        assert!(!j.can_run && !j.has_history);
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T03:00:00Z"));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T03:00:00Z"));
        // Someone else's event: no toggle or stop without the privileges.
        assert!(!j.can_enable && !j.can_stop);
        cx.privileges = vec!["SET_ANY_DEFINER".into(), "CONNECTION_ADMIN".into()];
        let j = job(&e, &cx, &t);
        assert!(j.can_enable && j.can_stop);
        cx.privileges.clear();
        cx.user = "app@%".into();
        assert!(job(&e, &cx, &t).can_enable);
    }

    #[test]
    fn notes_say_why_events_dont_run_or_are_missing() {
        let mut cx = ctx("2026-11-12 12:00:00");
        let t = Times {
            utc: HashMap::new(),
            missing: true,
        };
        cx.scheduler = "OFF".into();
        let n = notes(&cx, &t);
        assert!(n[0].contains("event_scheduler = OFF"), "{n:?}");
        assert!(n[1].contains("privilegio EVENT"), "{n:?}");
        assert!(n[2].contains("zonas horarias"), "{n:?}");
        cx.scheduler = "ON".into();
        cx.privileges = vec!["EVENT".into()];
        let t = Times {
            utc: HashMap::new(),
            missing: false,
        };
        assert!(notes(&cx, &t).is_empty());
    }

    #[test]
    fn alter_keeps_the_definer() {
        let e = Event {
            schema: "db".into(),
            name: "e`v".into(),
            definer: Some("app@10.0.0.%".into()),
            ..Default::default()
        };
        assert_eq!(
            alter_sql(&e, false).unwrap(),
            "ALTER DEFINER = `app`@`10.0.0.%` EVENT `db`.`e``v` DISABLE"
        );
        let role = Event {
            definer: Some("ops`role".into()),
            ..e.clone()
        };
        assert!(alter_sql(&role, true)
            .unwrap()
            .starts_with("ALTER DEFINER = `ops``role` EVENT"));
        let none = Event { definer: None, ..e };
        assert!(alter_sql(&none, true).is_err());
    }

    #[test]
    fn global_privileges_come_from_show_grants() {
        assert_eq!(
            global_privileges("GRANT SELECT, EVENT, SUPER ON *.* TO `u`@`%`"),
            vec!["SELECT", "EVENT", "SUPER"]
        );
        // MySQL's dynamic privileges, MariaDB's spelling and ALL.
        assert_eq!(
            global_privileges(
                "GRANT CONNECTION_ADMIN,SET_USER_ID ON *.* TO `root`@`localhost` WITH GRANT OPTION"
            ),
            vec!["CONNECTION_ADMIN", "SET_USER_ID"]
        );
        assert_eq!(
            global_privileges("grant set user, connection admin on *.* to `r`"),
            vec!["SET USER", "CONNECTION ADMIN"]
        );
        assert_eq!(
            global_privileges("GRANT ALL PRIVILEGES ON *.* TO `root`@`localhost` IDENTIFIED BY PASSWORD 'x' WITH GRANT OPTION"),
            vec!["ALL PRIVILEGES"]
        );
        // Not global: a database's, a table's, a role granted.
        for g in [
            "GRANT EVENT ON `db`.* TO `u`@`%`",
            "GRANT SELECT ON `db`.`t` TO `u`@`%`",
            "GRANT `ops`@`%` TO `u`@`%`",
            "REVOKE EVENT ON *.* FROM `u`",
        ] {
            assert!(global_privileges(g).is_empty(), "{g}");
        }
        let mut cx = ctx("2026-11-12 12:00:00");
        cx.privileges = vec!["ALL PRIVILEGES".into()];
        assert!(cx.has(&["EVENT"]) && cx.sets_any_definer() && cx.kills_any());
    }

    #[test]
    fn oceanbase_reads_times_on_the_session_clock() {
        let ob = events_sql(Variant::OceanBase, "");
        assert!(
            ob.contains("@@system_time_zone, @@global.time_zone) AS ev_tz"),
            "{ob}"
        );
        assert!(ob.contains("DATE_FORMAT(CONVERT_TZ(STARTS, IF(@@session.time_zone"));
        assert!(!ob.contains("{t:") && !ob.contains("{now}"), "{ob}");
        let my = events_sql(Variant::MySql, " WHERE 1");
        assert!(my.contains("TIME_ZONE AS ev_tz") && my.contains("EVENTS WHERE 1"));
        assert!(my.contains("STARTS AS ev_starts"));
        assert!(!my.contains('{'), "{my}");
    }

    #[test]
    fn oceanbase_scheduler_jobs_map_to_events() {
        let ob = ObJobs {
            dbs: [("500002".to_string(), "db".to_string())].into(),
        };
        assert_eq!(ob.job_name("db", "ev.1").as_deref(), Some("500002.ev.1"));
        assert_eq!(ob.job_name("other", "ev"), None);
        assert_eq!(
            ob.event("500002.ev.1"),
            Some(("db".to_string(), "ev.1".to_string()))
        );
        assert_eq!(ob.event("ASYNC_GATHER_STATS_JOB_PROC"), None);
        assert_eq!(ob.event("9.ev"), None);
        assert_eq!(ob_outcome("0", Some("SUCCESS")), JobOutcome::Succeeded);
        assert_eq!(
            ob_outcome("0", Some("check job missed")),
            JobOutcome::Skipped
        );
        assert_eq!(ob_outcome("-4012", Some("Timeout")), JobOutcome::Canceled);
        assert_eq!(
            ob_outcome("-5019", Some("Table 'db.nope' doesn't exist")),
            JobOutcome::Failed
        );
    }

    #[test]
    fn oceanbase_scripts_are_rebuilt_from_the_row() {
        let e = Event {
            schema: "db".into(),
            name: "e`v".into(),
            definer: Some("root@%".into()),
            tz: "+08:00".into(),
            body: Some("BEGIN INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); END;".into()),
            recurring: true,
            every: Some("1".into()),
            unit: Some("DAY".into()),
            starts: Some("2026-01-01 03:00:00".into()),
            ends: Some("2030-01-01 00:00:00".into()),
            status: "DISABLED".into(),
            preserve: true,
            comment: Some("it's".into()),
            ..Default::default()
        };
        let create = create_from_row(&e).unwrap();
        assert_eq!(
            create,
            "CREATE DEFINER = `root`@`%` EVENT `db`.`e``v`\nON SCHEDULE EVERY 1 DAY STARTS '2026-01-01 03:00:00' ENDS '2030-01-01 00:00:00'\nON COMPLETION PRESERVE\nDISABLE\nCOMMENT 'it''s'\nDO BEGIN INSERT INTO t VALUES (1); INSERT INTO t VALUES (2); END"
        );
        let s = wrapped(String::new(), &create, "STRICT_TRANS_TABLES", "+08:00");
        assert!(
            s.contains("time_zone = '+08:00'") && s.contains("DELIMITER $$\n"),
            "{s}"
        );
        let once = Event {
            recurring: false,
            at: Some("2030-01-01 00:00:00".into()),
            body: Some("INSERT INTO t VALUES (1);".into()),
            status: "ENABLED".into(),
            preserve: false,
            comment: None,
            definer: None,
            ..e.clone()
        };
        assert_eq!(
            create_from_row(&once).unwrap(),
            "CREATE EVENT `db`.`e``v`\nON SCHEDULE AT '2030-01-01 00:00:00'\nON COMPLETION NOT PRESERVE\nENABLE\nDO INSERT INTO t VALUES (1)"
        );
        // The statements OceanBase keeps of a BEGIN … END body.
        let bare = Event {
            body: Some("INSERT INTO t VALUES (2);INSERT INTO t VALUES (3);".into()),
            ..once.clone()
        };
        assert!(create_from_row(&bare)
            .unwrap()
            .ends_with("\nDO BEGIN INSERT INTO t VALUES (2);INSERT INTO t VALUES (3); END"));
        let composite = Event {
            every: Some("'1:30'".into()),
            unit: Some("HOUR_MINUTE".into()),
            ..e.clone()
        };
        assert!(create_from_row(&composite)
            .unwrap()
            .contains("EVERY '1:30' HOUR_MINUTE"));
        let odd = Event {
            unit: Some("DAY; DROP".into()),
            ..e
        };
        assert!(create_from_row(&odd).is_none());
    }

    #[test]
    fn every_worker_of_the_event_is_killed_but_our_own() {
        assert_eq!(kill_targets(&[7, 9], 1).unwrap(), vec![7, 9]);
        assert_eq!(kill_targets(&[7, 1], 1).unwrap(), vec![7]);
        assert!(matches!(kill_targets(&[1], 1), Err(Error::State(_))));
    }

    #[test]
    fn scripts_qualify_the_name_and_restore_the_session() {
        let create = "CREATE DEFINER=`root`@`localhost` EVENT `ev` ON SCHEDULE EVERY 1 HOUR STARTS '2026-11-12 09:54:37' ON COMPLETION NOT PRESERVE ENABLE DO BEGIN DECLARE x INT; SET x = 1; END";
        let s = script_text("db", "ev", create, "STRICT_TRANS_TABLES", "Europe/Madrid");
        assert_eq!(
            s,
            "SET @saved_sql_mode = @@sql_mode, @saved_time_zone = @@time_zone;\n\
             SET sql_mode = 'STRICT_TRANS_TABLES', time_zone = 'Europe/Madrid';\n\
             DELIMITER $$\n\
             CREATE DEFINER=`root`@`localhost` EVENT `db`.`ev` ON SCHEDULE EVERY 1 HOUR STARTS '2026-11-12 09:54:37' ON COMPLETION NOT PRESERVE ENABLE DO BEGIN DECLARE x INT; SET x = 1; END$$\n\
             DELIMITER ;\n\
             SET sql_mode = @saved_sql_mode, time_zone = @saved_time_zone;\n"
        );
        // One statement: no DELIMITER.
        let s = script_text(
            "db",
            "ev",
            "CREATE EVENT `ev` ON SCHEDULE AT '2027-01-01 00:00:00' DO SELECT 1",
            "",
            "SYSTEM",
        );
        assert!(
            s.contains(
                "CREATE EVENT `db`.`ev` ON SCHEDULE AT '2027-01-01 00:00:00' DO SELECT 1;\n"
            ),
            "{s}"
        );
        assert!(!s.contains("DELIMITER"));
        // A body using $$ gets another delimiter.
        let s = script_text(
            "db",
            "ev",
            "CREATE EVENT `ev` ON SCHEDULE EVERY 1 DAY DO BEGIN SELECT '$$'; END",
            "",
            "SYSTEM",
        );
        assert!(s.contains("DELIMITER //\n"), "{s}");
        // A CREATE that doesn't name it as expected: USE the schema.
        let s = script_text("d`b", "ev", "CREATE EVENT ev DO SELECT 1", "", "SYSTEM");
        assert!(s.starts_with("USE `d``b`;\n"), "{s}");
    }

    #[test]
    fn properties_and_statistics() {
        let e = Event {
            recurring: false,
            tz: "SYSTEM".into(),
            preserve: true,
            status: "SLAVESIDE_DISABLED".into(),
            sql_mode: Some("ANSI".into()),
            created: Some("2026-11-12 09:54:31".into()),
            ..Default::default()
        };
        let p = properties(&e);
        let get = |l: &str| p.iter().find(|x| x.label == l).map(|x| x.value.as_str());
        assert_eq!(get("Tipo"), Some("Una sola vez"));
        assert_eq!(get("Al terminar"), Some("Se conserva"));
        assert_eq!(get("Estado"), Some("Deshabilitado en la réplica"));
        assert_eq!(get("Creado"), Some("2026-11-12 09:54:31"));
        assert_eq!(
            p.iter().find(|x| x.label == "Modo SQL").unwrap().group,
            "Contexto"
        );
        assert!(!e.enabled());
        let s = statistics(Some(3), Some(12), Some(30), Some(1));
        assert_eq!(s.len(), 4);
        assert_eq!(s[1].value, "12 ms");
        assert_eq!(statistics(Some(0), None, None, None).len(), 1);
        assert!(statistics(None, None, None, None).is_empty());
    }
}
