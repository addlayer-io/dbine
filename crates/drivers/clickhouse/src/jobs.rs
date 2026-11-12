//! Server jobs ([`dbine_driver::Session::jobs`]).
//!
//! - ClickHouse: what the server runs on its own schedule are the
//!   refreshable materialized views (`REFRESH EVERY|AFTER …`, 23.12 on).
//!   `system.view_refreshes` has their state (its columns changed in 24.10:
//!   the list reads the ones the server has), `SYSTEM REFRESH|CANCEL|STOP|
//!   START VIEW` act on them and `SHOW CREATE TABLE` is their script. Past
//!   refreshes are in `system.query_log` (`client_name` "refreshable
//!   materialized view", `log_comment` "refresh of db.view", with
//!   " (attempt n/m)" on a retry) from 25.4 on, when the user can read it.
//!   Older servers log refreshes without saying which view they belong to,
//!   so there they have no history. A refresh asked of a stopped view waits
//!   until it's started, so "run now" is offered only on started ones.
//! - Timeplus Proton (3.0 on): its tasks (`CREATE TASK … SCHEDULE …`).
//!   `SHOW TASKS FROM db` with the `verbose` setting lists them with their
//!   state (`status` 1: paused); the setting goes on the request, which a
//!   read-only connection accepts where a `SETTINGS` clause is refused.
//!   `system.task_execution_state` keeps only the last run of each one (no
//!   history), `SYSTEM PAUSE|UNPAUSE TASK` disable and enable them, and
//!   `SYSTEM EXECUTE TASK` runs one and answers when it ends: DBine sends it
//!   on its own request and returns once the server took it.
//!
//! A job's id is its database and name, each in backticks
//! (`` `db`.`view` ``). An id that comes back is parsed back into the two
//! names (anything else is refused), looked up with query parameters, and
//! quoted again with [`q`] for the `SYSTEM` statements.

use crate::schema::q;
use crate::{text, ClickHouseSession, Flavor};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, PropertyInfo, Result, ServerJob,
};
use serde_json::Value;
use std::collections::{BTreeMap, BTreeSet};
use std::time::Duration;

const REFRESH_KIND: &str = "Refreshable MV";
const TASK_KIND: &str = "Task";
/// What the server puts in `query_log.client_name` for a refresh.
const REFRESH_CLIENT: &str = "refreshable materialized view";
/// Characters kept of an error message.
const MAX_MESSAGE: usize = 4000;
/// How long `run_job` waits on Proton's `EXECUTE TASK` for an early refusal
/// before leaving the run going on its own.
const EXECUTE_WAIT: Duration = Duration::from_secs(3);

/// A job's id: `` `db`.`name` ``.
pub(crate) fn job_id(db: &str, name: &str) -> String {
    format!("{}.{}", q(db), q(name))
}

/// One backtick-quoted name at the start of `s` (as [`q`] writes it), and
/// what follows it.
fn unquote(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('`')?;
    let mut out = String::new();
    let mut chars = rest.char_indices();
    while let Some((i, c)) = chars.next() {
        match c {
            '\\' => out.push(chars.next()?.1),
            '`' => return Some((out, &rest[i + 1..])),
            c => out.push(c),
        }
    }
    None
}

/// The database and name in an id [`job_id`] made; `None` for anything else.
pub(crate) fn parse_id(id: &str) -> Option<(String, String)> {
    if id.len() > 1024 {
        return None;
    }
    let (db, rest) = unquote(id)?;
    let (name, rest) = unquote(rest.strip_prefix('.')?)?;
    let ok = |s: &str| !s.is_empty() && !s.chars().any(char::is_control);
    (rest.is_empty() && ok(&db) && ok(&name)).then_some((db, name))
}

fn bad_id(id: &str) -> Error {
    Error::Query(format!(
        "«{}» no es un id de job de este servidor",
        id.chars().take(200).collect::<String>()
    ))
}

/// Seconds since 1970 as RFC 3339 in UTC.
pub(crate) fn rfc3339(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!(
        "{y:04}-{m:02}-{d:02}T{:02}:{:02}:{:02}Z",
        rem / 3600,
        rem % 3600 / 60,
        rem % 60
    )
}

/// `YYYY-MM-DD hh:mm:ss[.fff]` already in UTC (Proton's `datetime64(3,
/// 'UTC')`) as RFC 3339.
fn utc_text(s: &str) -> Option<String> {
    let s = s.trim();
    let b = s.as_bytes();
    let shape = b.len() >= 19
        && b[..19].iter().enumerate().all(|(i, c)| match i {
            4 | 7 => *c == b'-',
            10 => *c == b' ' || *c == b'T',
            13 | 16 => *c == b':',
            _ => c.is_ascii_digit(),
        });
    (shape && !s.starts_with("1970-01-01")).then(|| format!("{}T{}Z", &s[..10], &s[11..19]))
}

/// Milliseconds between two of those.
fn utc_millis(s: &str) -> Option<i64> {
    let s = s.trim();
    let n = |a: usize, b: usize| s.get(a..b)?.parse::<i64>().ok();
    let (y, mo, d) = (n(0, 4)?, n(5, 7)?, n(8, 10)?);
    let (h, mi, se) = (n(11, 13)?, n(14, 16)?, n(17, 19)?);
    let frac = s
        .get(19..)
        .and_then(|f| f.strip_prefix('.'))
        .map(|f| {
            let digits: String = f.chars().take(3).collect();
            format!("{digits:0<3}").parse::<i64>().unwrap_or(0)
        })
        .unwrap_or(0);
    // days_from_civil.
    let y = if mo <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (mo + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    let days = era * 146_097 + doe - 719_468;
    Some(((days * 86_400 + h * 3600 + mi * 60 + se) * 1000) + frac)
}

fn opt(v: Option<&Value>) -> Option<String> {
    v.map(text)
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
}

fn num(v: Option<&Value>) -> Option<f64> {
    match v? {
        Value::Number(n) => n.as_f64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}

fn time(v: Option<&Value>) -> Option<String> {
    num(v).filter(|s| *s > 0.0).map(|s| rfc3339(s as i64))
}

fn cut(s: &str) -> String {
    s.chars().take(MAX_MESSAGE).collect()
}

/// Splits a statement into words, keeping quoted names and strings (with
/// their backslash escapes) whole.
fn words(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        match c {
            '`' | '\'' | '"' => {
                cur.push(c);
                while let Some(d) = chars.next() {
                    cur.push(d);
                    if d == '\\' {
                        if let Some(e) = chars.next() {
                            cur.push(e);
                        }
                    } else if d == c {
                        break;
                    }
                }
            }
            c if c.is_whitespace() => {
                if !cur.is_empty() {
                    out.push(std::mem::take(&mut cur));
                }
            }
            c => cur.push(c),
        }
    }
    if !cur.is_empty() {
        out.push(cur);
    }
    out
}

/// A name as the server printed it (`db`.`t`, `` `a b` ``), without the
/// backticks.
fn plain(name: &str) -> String {
    let mut out = String::new();
    let mut rest = name;
    while !rest.is_empty() {
        if let Some((part, after)) = unquote(rest) {
            out.push_str(&part);
            rest = after;
        } else {
            let mut cs = rest.chars();
            out.extend(cs.next());
            rest = cs.as_str();
        }
    }
    out
}

/// What a refreshable view's `create_table_query` says about its refresh.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct RefreshClause {
    /// "EVERY 1 HOUR OFFSET 5 MINUTE", without `REFRESH`.
    pub schedule: Option<String>,
    pub depends_on: Option<String>,
    pub append: bool,
    pub target: Option<String>,
    pub definer: Option<String>,
}

/// Reads the refresh clause of `CREATE MATERIALIZED VIEW db.v REFRESH …`.
pub(crate) fn refresh_clause(create: &str) -> RefreshClause {
    let w = words(create);
    let up = |i: usize| w.get(i).map(|s| s.to_ascii_uppercase()).unwrap_or_default();
    let mut out = RefreshClause::default();
    // CREATE MATERIALIZED VIEW <name> REFRESH … (as the server writes it).
    let start = 4;
    if up(start) != "REFRESH" {
        return out;
    }
    const AFTER_CLAUSE: &[&str] = &[
        "APPEND", "TO", "ENGINE", "EMPTY", "DEFINER", "SQL", "COMMENT", "AS", "POPULATE",
        "SETTINGS",
    ];
    let mut i = start + 1;
    let mut schedule = Vec::new();
    let mut depends = Vec::new();
    while i < w.len() && !AFTER_CLAUSE.contains(&up(i).as_str()) && !w[i].starts_with('(') {
        if up(i) == "DEPENDS" && up(i + 1) == "ON" {
            i += 2;
            while i < w.len() && !AFTER_CLAUSE.contains(&up(i).as_str()) && !w[i].starts_with('(') {
                depends.push(plain(&w[i]));
                i += 1;
            }
            continue;
        }
        schedule.push(w[i].clone());
        i += 1;
    }
    out.schedule = (!schedule.is_empty()).then(|| schedule.join(" "));
    out.depends_on = (!depends.is_empty()).then(|| depends.join(" "));
    while i < w.len() && up(i) != "AS" {
        match up(i).as_str() {
            "APPEND" => out.append = true,
            "TO" if out.target.is_none() => {
                out.target = w.get(i + 1).map(|t| plain(t));
                i += 1;
            }
            "DEFINER" if w.get(i + 1).map(String::as_str) == Some("=") => {
                out.definer = w.get(i + 2).map(|t| plain(t));
                i += 2;
            }
            _ => {}
        }
        i += 1;
    }
    out
}

/// The `system.view_refreshes` columns the list reads; each one is `NULL`
/// (or empty) when the server doesn't have it.
const REFRESH_COLUMNS: &[&str] = &[
    "status",
    "last_success_time",
    "last_refresh_time",
    "next_refresh_time",
    "exception",
    "last_success_duration_ms",
    "duration_ms",
    "last_refresh_result",
    "retry",
    "progress",
    "read_rows",
    "written_rows",
    "last_refresh_replica",
];

/// The query over `system.view_refreshes` (with the view's definition from
/// `system.tables`), for the columns `has`; `one`: a single view, by the
/// `db` and `v` parameters.
pub(crate) fn refreshes_sql(has: &BTreeSet<String>, one: bool) -> String {
    let col = |c: &str, missing: &str| {
        if has.contains(c) {
            match c {
                "last_success_time" | "last_refresh_time" | "next_refresh_time" => {
                    format!("toUnixTimestamp(r.{c})")
                }
                _ => format!("r.{c}"),
            }
        } else {
            missing.to_string()
        }
    };
    let cols: Vec<String> = REFRESH_COLUMNS
        .iter()
        .map(|c| match *c {
            "status" | "exception" | "last_refresh_result" | "last_refresh_replica" => col(c, "''"),
            _ => col(c, "NULL"),
        })
        .collect();
    let filter = if one {
        "WHERE r.database = {db:String} AND r.view = {v:String}"
    } else {
        ""
    };
    format!(
        "SELECT r.database, r.view, {},
                t.create_table_query, t.comment, t.as_select
         FROM system.view_refreshes AS r
         LEFT JOIN (SELECT database, name, create_table_query, comment, as_select
                    FROM system.tables WHERE engine = 'MaterializedView') AS t
           ON t.database = r.database AND t.name = r.view
         {filter}
         ORDER BY r.database, r.view",
        cols.join(", ")
    )
}

/// One row of [`refreshes_sql`], by column name.
pub(crate) struct Refresh {
    pub database: String,
    pub view: String,
    pub status: String,
    pub last_success: Option<String>,
    pub last_refresh: Option<String>,
    pub next_refresh: Option<String>,
    pub exception: Option<String>,
    pub duration_ms: Option<u64>,
    pub result: Option<String>,
    pub retry: Option<u64>,
    pub progress: Option<f64>,
    pub read_rows: Option<u64>,
    pub written_rows: Option<u64>,
    pub replica: Option<String>,
    pub create: String,
    pub comment: Option<String>,
    pub select: Option<String>,
}

impl Refresh {
    pub(crate) fn from_row(r: &[Value]) -> Self {
        // database, view, then REFRESH_COLUMNS, then the three from tables.
        let at = |name: &str| r.get(2 + REFRESH_COLUMNS.iter().position(|c| *c == name)?);
        let n = REFRESH_COLUMNS.len() + 2;
        Refresh {
            database: opt(r.first()).unwrap_or_default(),
            view: opt(r.get(1)).unwrap_or_default(),
            status: opt(at("status")).unwrap_or_default(),
            last_success: time(at("last_success_time")),
            last_refresh: time(at("last_refresh_time")),
            next_refresh: time(at("next_refresh_time")),
            exception: opt(at("exception")).map(|e| cut(&e)),
            duration_ms: num(at("last_success_duration_ms"))
                .or_else(|| num(at("duration_ms")))
                .map(|v| v.max(0.0) as u64),
            result: opt(at("last_refresh_result")),
            retry: num(at("retry")).map(|v| v.max(0.0) as u64),
            progress: num(at("progress")),
            read_rows: num(at("read_rows")).map(|v| v.max(0.0) as u64),
            written_rows: num(at("written_rows")).map(|v| v.max(0.0) as u64),
            replica: opt(at("last_refresh_replica")),
            create: opt(r.get(n)).unwrap_or_default(),
            comment: opt(r.get(n + 1)),
            select: opt(r.get(n + 2)),
        }
    }

    fn running(&self) -> bool {
        self.status.starts_with("Running")
    }

    fn enabled(&self) -> bool {
        self.status != "Disabled"
    }

    /// How the last finished refresh went.
    fn outcome(&self) -> Option<JobOutcome> {
        // Before 24.10: `last_refresh_result`.
        if let Some(r) = self.result.as_deref() {
            return match r {
                "Finished" => Some(JobOutcome::Succeeded),
                "Error" | "Exception" => Some(JobOutcome::Failed),
                "Cancelled" | "Canceled" => Some(JobOutcome::Canceled),
                "Skipped" => Some(JobOutcome::Skipped),
                _ if self.running() => Some(JobOutcome::Running),
                _ => None,
            };
        }
        match self.exception.as_deref() {
            Some(e) if e.eq_ignore_ascii_case("cancelled") => Some(JobOutcome::Canceled),
            Some(_) if self.retry.unwrap_or(0) > 0 && self.status == "Scheduled" => {
                Some(JobOutcome::Retrying)
            }
            Some(_) => Some(JobOutcome::Failed),
            None if self.last_success.is_some() => Some(JobOutcome::Succeeded),
            None if self.running() => Some(JobOutcome::Running),
            None => None,
        }
    }

    pub(crate) fn job(&self, history: History) -> ServerJob {
        let clause = refresh_clause(&self.create);
        let outcome = self.outcome();
        let succeeded = outcome == Some(JobOutcome::Succeeded);
        ServerJob {
            id: job_id(&self.database, &self.view),
            name: self.view.clone(),
            kind: REFRESH_KIND.into(),
            group: Some(self.database.clone()),
            description: self.comment.clone(),
            owner: clause.definer.clone(),
            enabled: self.enabled(),
            schedule: clause.schedule.clone(),
            running: self.running(),
            last_outcome: outcome,
            last_started: if succeeded {
                self.last_success.clone()
            } else {
                self.last_refresh.clone()
            },
            last_duration_ms: if succeeded || self.result.is_some() {
                self.duration_ms
            } else {
                None
            },
            next_run: self
                .next_refresh
                .clone()
                .filter(|_| self.enabled() && !self.running()),
            // A refresh asked of a stopped view waits for `START VIEW`.
            can_run: self.enabled(),
            can_stop: true,
            can_enable: true,
            has_history: history == History::Readable,
            has_script: true,
        }
    }

    pub(crate) fn detail(&self, history: History) -> JobDetail {
        let clause = refresh_clause(&self.create);
        let mut props = Vec::new();
        let mut add = |group: &str, label: &str, value: Option<String>| {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                props.push(PropertyInfo {
                    group: group.into(),
                    label: label.into(),
                    value,
                });
            }
        };
        add("", "Estado", Some(self.status.clone()));
        add(
            "",
            "Destino",
            Some(
                clause
                    .target
                    .clone()
                    .unwrap_or_else(|| "Tabla interna de la vista".into()),
            ),
        );
        add(
            "",
            "Al refrescar",
            Some(
                if clause.append {
                    "Agrega las filas (APPEND)"
                } else {
                    "Reemplaza el contenido"
                }
                .into(),
            ),
        );
        add("", "Depende de", clause.depends_on.clone());
        const LAST: &str = "Último refresco";
        add(LAST, "Último correcto", self.last_success.clone());
        add(LAST, "Último intento", self.last_refresh.clone());
        add(LAST, "Resultado", self.result.clone());
        add(
            LAST,
            "Reintentos",
            self.retry.filter(|r| *r > 0).map(|r| r.to_string()),
        );
        add(
            LAST,
            "Progreso",
            self.progress
                .filter(|_| self.running())
                .map(|p| format!("{:.0} %", p * 100.0)),
        );
        add(LAST, "Filas leídas", self.read_rows.map(|v| v.to_string()));
        add(
            LAST,
            "Filas escritas",
            self.written_rows.map(|v| v.to_string()),
        );
        add(LAST, "Réplica", self.replica.clone());
        add(LAST, "Error", self.exception.clone());
        JobDetail {
            job: self.job(history),
            steps: vec![JobStep {
                name: self.view.clone(),
                kind: Some("SQL".into()),
                target: clause.target,
                command: self.select.clone(),
            }],
            schedules: Vec::new(),
            properties: props,
        }
    }
}

/// `system.query_log` rows of a view's refreshes, newest first. The server
/// tags each one with `log_comment` "refresh of db.view", and a retry with
/// " (attempt n/m)" after it: parameter `lc` is the first and `lca` the
/// start of the second ("refresh of db.view (attempt ").
pub(crate) fn runs_sql(limit: u32) -> String {
    format!(
        "SELECT query_id, toUnixTimestamp(query_start_time), query_duration_ms, toString(type),
                exception_code, substring(exception, 1, {MAX_MESSAGE})
         FROM system.query_log
         WHERE client_name = '{REFRESH_CLIENT}' AND type != 'QueryStart'
           AND (log_comment = {{lc:String}}
                OR (startsWith(log_comment, {{lca:String}})
                    AND match(substring(log_comment, length({{lca:String}}) + 1), '^[0-9]+/[0-9]+[)]$')))
         ORDER BY event_time_microseconds DESC
         LIMIT {limit}"
    )
}

/// The `log_comment` of a view's refreshes, and the start of a retry's.
pub(crate) fn refresh_comments(db: &str, view: &str) -> (String, String) {
    let lc = format!("refresh of {db}.{view}");
    let lca = format!("{lc} (attempt ");
    (lc, lca)
}

/// Whether past refreshes can be read.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum History {
    Readable,
    /// Before 25.4: refreshes aren't tagged with their view.
    Untagged,
    /// `system.query_log` can't be read.
    NoAccess,
}

impl History {
    fn note(self) -> Option<&'static str> {
        match self {
            History::Readable => None,
            History::Untagged => Some(UNTAGGED),
            History::NoAccess => Some(NO_QUERY_LOG),
        }
    }
}

/// `major.minor` of a `version()` at least the one given.
pub(crate) fn version_at_least(version: &str, major: u64, minor: u64) -> bool {
    let mut parts = version
        .trim()
        .split('.')
        .map(|p| p.parse::<u64>().unwrap_or(0));
    let (a, b) = (parts.next().unwrap_or(0), parts.next().unwrap_or(0));
    (a, b) >= (major, minor)
}

/// `QUERY_WAS_CANCELLED`.
const CANCELLED: f64 = 394.0;

pub(crate) fn run_row(r: &[Value]) -> JobRun {
    let failed = !opt(r.get(3)).is_some_and(|t| t == "QueryFinish");
    JobRun {
        id: opt(r.first()),
        started: time(r.get(1)),
        duration_ms: num(r.get(2)).map(|v| v.max(0.0) as u64),
        outcome: match (failed, num(r.get(4))) {
            (false, _) => JobOutcome::Succeeded,
            (true, Some(c)) if c == CANCELLED => JobOutcome::Canceled,
            (true, _) => JobOutcome::Failed,
        },
        trigger: None,
        message: opt(r.get(5)).filter(|_| failed),
        steps: Vec::new(),
    }
}

/// What `SHOW CREATE TASK` says of a Proton task.
#[derive(Debug, Default, PartialEq)]
pub(crate) struct TaskDef {
    pub schedule: Option<String>,
    pub timeout: Option<String>,
    pub into: Option<String>,
    pub query: Option<String>,
}

pub(crate) fn task_def(create: &str) -> TaskDef {
    let w = words(create);
    let up = |i: usize| w.get(i).map(|s| s.to_ascii_uppercase()).unwrap_or_default();
    const KEYS: &[&str] = &["SCHEDULE", "TIMEOUT", "INTO", "AS", "SETTINGS"];
    let mut out = TaskDef::default();
    let mut i = 0;
    while i < w.len() {
        let key = up(i);
        if key == "AS" && i > 2 {
            // The query is the rest of the text, as written.
            let mut at = 0;
            for word in &w[..=i] {
                at = create[at..].find(word.as_str()).map_or(at, |p| at + p) + word.len();
            }
            out.query = Some(create[at..].trim().to_string()).filter(|s| !s.is_empty());
            break;
        }
        if KEYS.contains(&key.as_str()) && i > 2 {
            let mut j = i + 1;
            let mut val = Vec::new();
            while j < w.len() && !KEYS.contains(&up(j).as_str()) {
                val.push(w[j].clone());
                j += 1;
            }
            let val = (!val.is_empty()).then(|| val.join(" "));
            match key.as_str() {
                "SCHEDULE" => out.schedule = val,
                "TIMEOUT" => out.timeout = val,
                "INTO" => out.into = val.map(|v| plain(&v)),
                _ => {}
            }
            i = j;
            continue;
        }
        i += 1;
    }
    out
}

/// A Proton task as `SHOW TASKS … verbose` and the other two sources tell
/// it.
pub(crate) struct Task {
    pub database: String,
    /// name, uuid, version, status, last_modified, last_modified_by,
    /// created, created_by.
    pub row: Vec<Value>,
    pub def: TaskDef,
    /// last_execution_start, last_execution_end, last_execution_result.
    pub last: Option<Vec<Value>>,
}

impl Task {
    fn name(&self) -> String {
        opt(self.row.first()).unwrap_or_default()
    }

    fn enabled(&self) -> bool {
        // `status` 1: paused.
        num(self.row.get(3)).is_none_or(|s| s == 0.0)
    }

    pub(crate) fn job(&self) -> ServerJob {
        let last = self.last.as_deref().unwrap_or(&[]);
        let start = opt(last.first());
        let end = opt(last.get(1));
        let result = opt(last.get(2));
        let started = start.as_deref().and_then(utc_text);
        let finished = end.as_deref().and_then(utc_text);
        let running = started.is_some() && finished.is_none();
        let outcome = match (&started, &finished, result.as_deref()) {
            (None, _, _) => None,
            (Some(_), None, _) => Some(JobOutcome::Running),
            (Some(_), Some(_), Some(r)) if r.eq_ignore_ascii_case("ok") => {
                Some(JobOutcome::Succeeded)
            }
            (Some(_), Some(_), Some(_)) => Some(JobOutcome::Failed),
            (Some(_), Some(_), None) => Some(JobOutcome::Unknown),
        };
        let duration = match (start.as_deref(), end.as_deref(), &finished) {
            (Some(s), Some(e), Some(_)) => utc_millis(e)
                .zip(utc_millis(s))
                .map(|(e, s)| (e - s).max(0) as u64),
            _ => None,
        };
        let enabled = self.enabled();
        ServerJob {
            id: job_id(&self.database, &self.name()),
            name: self.name(),
            kind: TASK_KIND.into(),
            group: Some(self.database.clone()),
            description: None,
            owner: opt(self.row.get(7)),
            enabled,
            schedule: self.def.schedule.clone(),
            running,
            last_outcome: outcome,
            last_started: started,
            last_duration_ms: duration,
            next_run: None,
            // A paused task refuses `EXECUTE TASK`.
            can_run: enabled,
            can_stop: false,
            can_enable: true,
            has_history: false,
            has_script: true,
        }
    }

    pub(crate) fn detail(&self) -> JobDetail {
        let mut props = Vec::new();
        let mut add = |group: &str, label: &str, value: Option<String>| {
            if let Some(value) = value.filter(|v| !v.is_empty()) {
                props.push(PropertyInfo {
                    group: group.into(),
                    label: label.into(),
                    value,
                });
            }
        };
        add("", "Tiempo máximo (TIMEOUT)", self.def.timeout.clone());
        add("", "Destino (INTO)", self.def.into.clone());
        add(
            "",
            "Creada",
            opt(self.row.get(6)).as_deref().and_then(utc_text),
        );
        add("", "Creada por", opt(self.row.get(7)));
        add(
            "",
            "Última modificación",
            opt(self.row.get(4)).as_deref().and_then(utc_text),
        );
        add("", "Modificada por", opt(self.row.get(5)));
        add("", "UUID", opt(self.row.get(1)));
        let last = self.last.as_deref().unwrap_or(&[]);
        add(
            "Última ejecución",
            "Fin",
            opt(last.get(1)).as_deref().and_then(utc_text),
        );
        add(
            "Última ejecución",
            "Resultado",
            opt(last.get(2)).map(|r| cut(&r)),
        );
        JobDetail {
            job: self.job(),
            steps: vec![JobStep {
                name: self.name(),
                kind: Some("SQL".into()),
                target: self.def.into.clone(),
                command: self.def.query.clone(),
            }],
            schedules: Vec::new(),
            properties: props,
        }
    }
}

/// The server has no refreshable views (before 23.12).
const NO_REFRESHABLE: &str =
    "esta versión de ClickHouse no tiene vistas materializadas refrescables (llegaron en la 23.12)";
const NONE_YET: &str = "en ClickHouse, los jobs son las vistas materializadas refrescables (REFRESH EVERY / AFTER) y este servidor no tiene ninguna";
const NO_QUERY_LOG: &str =
    "sin acceso a system.query_log: de cada vista se ve solo el último refresco";
const UNTAGGED: &str = "esta versión de ClickHouse no registra a qué vista pertenece cada refresco (llegó en la 25.4): de cada vista se ve solo el último refresco";
const TASKS_NO_STATUS: &str =
    "este usuario no puede cambiar ajustes (readonly): no se ve si cada tarea está pausada";
const TASKS_NO_STATE_READONLY: &str =
    "en una conexión de solo lectura Timeplus Proton no deja leer la última ejecución de las tareas";
const TASKS_NO_STATE: &str =
    "no se pudo leer la última ejecución de las tareas (system.task_execution_state)";
const NO_TASKS: &str =
    "esta versión de Timeplus Proton no tiene tareas programadas (CREATE TASK llegó en la 3.0)";
const TASK_LAST_ONLY: &str = "Timeplus Proton guarda solo la última ejecución de cada tarea";

impl ClickHouseSession {
    pub(crate) async fn jobs_list(&self) -> Result<JobList> {
        match self.flavor {
            Flavor::ClickHouse => self.refresh_list().await,
            Flavor::Timeplus => self.task_list().await,
        }
    }

    pub(crate) async fn job_info(&self, id: &str) -> Result<JobDetail> {
        let (db, name) = parse_id(id).ok_or_else(|| bad_id(id))?;
        match self.flavor {
            Flavor::ClickHouse => {
                let r = self.refresh(&db, &name).await?;
                Ok(r.detail(self.history().await))
            }
            Flavor::Timeplus => Ok(self.task(&db, &name).await?.detail()),
        }
    }

    pub(crate) async fn job_history(&self, id: &str, limit: u32) -> Result<Vec<JobRun>> {
        let (db, name) = parse_id(id).ok_or_else(|| bad_id(id))?;
        if self.flavor == Flavor::Timeplus {
            return Err(Error::Unsupported(TASK_LAST_ONLY.into()));
        }
        let r = self.refresh(&db, &name).await?;
        if let Some(note) = self.history().await.note() {
            return Err(Error::Unsupported(note.into()));
        }
        let limit = limit.clamp(1, MAX_JOB_RUNS);
        let (lc, lca) = refresh_comments(&db, &name);
        let rows = self
            .rows(&runs_sql(limit), &[("lc", &lc), ("lca", &lca)])
            .await
            .map_err(|e| match e {
                Error::Query(m) => Error::Query(format!(
                    "no se pudo leer el historial de system.query_log: {m}"
                )),
                e => e,
            })?;
        let mut runs: Vec<JobRun> = rows.iter().map(|r| run_row(r)).collect();
        if r.running() {
            runs.insert(
                0,
                JobRun {
                    started: r.last_refresh.clone(),
                    outcome: JobOutcome::Running,
                    ..Default::default()
                },
            );
            runs.truncate(limit as usize);
        }
        Ok(runs)
    }

    pub(crate) async fn job_start(&self, id: &str) -> Result<()> {
        let (db, name) = parse_id(id).ok_or_else(|| bad_id(id))?;
        match self.flavor {
            Flavor::ClickHouse => {
                let r = self.refresh(&db, &name).await?;
                if !r.enabled() {
                    return Err(Error::Query(format!(
                        "la vista {db}.{name} está deshabilitada: hay que habilitarla para refrescarla"
                    )));
                }
                self.system(&format!("SYSTEM REFRESH VIEW {}", job_id(&db, &name)))
                    .await
            }
            Flavor::Timeplus => {
                self.task(&db, &name).await?;
                self.execute_detached(&format!("SYSTEM EXECUTE TASK {}", job_id(&db, &name)))
                    .await
            }
        }
    }

    pub(crate) async fn job_stop(&self, id: &str) -> Result<()> {
        let (db, name) = parse_id(id).ok_or_else(|| bad_id(id))?;
        match self.flavor {
            Flavor::ClickHouse => {
                self.refresh(&db, &name).await?;
                self.system(&format!("SYSTEM CANCEL VIEW {}", job_id(&db, &name)))
                    .await
            }
            Flavor::Timeplus => Err(Error::Unsupported(
                "Timeplus Proton no permite detener una tarea en curso".into(),
            )),
        }
    }

    pub(crate) async fn job_enable(&self, id: &str, enabled: bool) -> Result<()> {
        let (db, name) = parse_id(id).ok_or_else(|| bad_id(id))?;
        let target = job_id(&db, &name);
        match self.flavor {
            Flavor::ClickHouse => {
                self.refresh(&db, &name).await?;
                let verb = if enabled { "START" } else { "STOP" };
                self.system(&format!("SYSTEM {verb} VIEW {target}")).await
            }
            Flavor::Timeplus => {
                self.task(&db, &name).await?;
                let verb = if enabled { "UNPAUSE" } else { "PAUSE" };
                self.system(&format!("SYSTEM {verb} TASK {target}")).await
            }
        }
    }

    pub(crate) async fn job_create_script(&self, id: &str) -> Result<String> {
        let (db, name) = parse_id(id).ok_or_else(|| bad_id(id))?;
        let target = job_id(&db, &name);
        let sql = match self.flavor {
            Flavor::ClickHouse => {
                self.refresh(&db, &name).await?;
                format!("SHOW CREATE TABLE {target}")
            }
            Flavor::Timeplus => format!("SHOW CREATE TASK {target}"),
        };
        let rows = self.rows(&sql, &[]).await?;
        let create = rows
            .first()
            .and_then(|r| opt(r.first()))
            .ok_or_else(|| Error::Query(format!("no se encontró el job {db}.{name}")))?;
        Ok(format!("{};\n", create.trim_end().trim_end_matches(';')))
    }

    /// A statement whose answer has no rows.
    async fn system(&self, sql: &str) -> Result<()> {
        self.rows(sql, &[]).await.map(|_| ())
    }

    /// `system.view_refreshes`' columns on this server (empty: it has none).
    async fn refresh_columns(&self) -> Result<BTreeSet<String>> {
        let rows = self
            .rows(
                "SELECT name FROM system.columns WHERE database = 'system' AND table = 'view_refreshes'",
                &[],
            )
            .await?;
        Ok(rows.iter().filter_map(|r| opt(r.first())).collect())
    }

    /// Whether past refreshes can be read: the server tags them with their
    /// view (25.4 on) and the user can read `system.query_log`.
    async fn history(&self) -> History {
        let version = self
            .rows("SELECT version()", &[])
            .await
            .ok()
            .and_then(|r| r.first().and_then(|r| opt(r.first())))
            .unwrap_or_default();
        if !version_at_least(&version, 25, 4) {
            return History::Untagged;
        }
        match self
            .rows("SELECT 1 FROM system.query_log LIMIT 0", &[])
            .await
        {
            Ok(_) => History::Readable,
            Err(_) => History::NoAccess,
        }
    }

    async fn refresh_list(&self) -> Result<JobList> {
        let cols = self.refresh_columns().await?;
        if cols.is_empty() {
            return Ok(JobList {
                jobs: Vec::new(),
                notes: vec![NO_REFRESHABLE.into()],
            });
        }
        let rows = self.rows(&refreshes_sql(&cols, false), &[]).await?;
        let history = self.history().await;
        let jobs: Vec<ServerJob> = rows
            .iter()
            .map(|r| Refresh::from_row(r).job(history))
            .filter(|j| !j.name.is_empty())
            .collect();
        let mut notes = Vec::new();
        if jobs.is_empty() {
            notes.push(NONE_YET.to_string());
        } else if let Some(note) = history.note() {
            notes.push(note.to_string());
        }
        Ok(JobList { jobs, notes })
    }

    /// One refreshable view, or an error saying there's none by that name.
    async fn refresh(&self, db: &str, view: &str) -> Result<Refresh> {
        let cols = self.refresh_columns().await?;
        if cols.is_empty() {
            return Err(Error::Unsupported(NO_REFRESHABLE.into()));
        }
        let rows = self
            .rows(&refreshes_sql(&cols, true), &[("db", db), ("v", view)])
            .await?;
        rows.first()
            .map(|r| Refresh::from_row(r))
            .ok_or_else(|| Error::Query(format!("no existe la vista refrescable {db}.{view}")))
    }

    /// Every database's tasks (`SHOW TASKS` lists one database).
    async fn task_list(&self) -> Result<JobList> {
        let dbs = self
            .rows(
                "SELECT name FROM system.databases
                 WHERE name NOT IN ('system', 'INFORMATION_SCHEMA', 'information_schema') ORDER BY name",
                &[],
            )
            .await?;
        let mut notes = Vec::new();
        let last = match self.task_states().await {
            Ok(last) => last,
            Err(note) => {
                notes.push(note.to_string());
                BTreeMap::new()
            }
        };
        let mut jobs = Vec::new();
        let mut status_known = true;
        for db in dbs.iter().filter_map(|r| opt(r.first())) {
            let (tasks, known) = match self.tasks_in(&db).await {
                Ok(t) => t,
                Err(e) if no_tasks(&e) => {
                    return Ok(JobList {
                        jobs: Vec::new(),
                        notes: vec![NO_TASKS.into()],
                    })
                }
                Err(e) => return Err(e),
            };
            status_known &= known;
            for row in tasks {
                let name = opt(row.first()).unwrap_or_default();
                if name.is_empty() {
                    continue;
                }
                let def = self.task_create(&db, &name).await.unwrap_or_default();
                let uuid = opt(row.get(1)).unwrap_or_default();
                jobs.push(
                    Task {
                        last: last.get(&uuid).cloned(),
                        database: db.clone(),
                        row,
                        def,
                    }
                    .job(),
                );
            }
        }
        if jobs.is_empty() {
            notes.clear();
        } else {
            if !status_known {
                notes.push(TASKS_NO_STATUS.into());
            }
            notes.push(TASK_LAST_ONLY.into());
        }
        Ok(JobList { jobs, notes })
    }

    /// One database's tasks, and whether their state came with them. The
    /// `verbose` setting goes on the request (a read-only connection takes
    /// it there); a user whose profile is read-only can't set it at all and
    /// gets only the names.
    async fn tasks_in(&self, db: &str) -> Result<(Vec<Vec<Value>>, bool)> {
        let sql = format!("SHOW TASKS FROM {}", q(db));
        match self.rows_with(&sql, &[], &[("verbose", "1")]).await {
            Ok(rows) => Ok((rows, true)),
            Err(e) if readonly_refusal(&e) => Ok((self.rows(&sql, &[]).await?, false)),
            Err(e) => Err(e),
        }
    }

    async fn task_create(&self, db: &str, name: &str) -> Result<TaskDef> {
        let rows = self
            .rows(&format!("SHOW CREATE TASK {}", job_id(db, name)), &[])
            .await?;
        Ok(rows
            .first()
            .and_then(|r| opt(r.first()))
            .map(|c| task_def(&c))
            .unwrap_or_default())
    }

    /// The last run of every task, by the task's uuid (the state of a
    /// dropped task with the same name stays behind); `Err` with the note
    /// when it can't be read (Proton refuses it in read-only mode).
    async fn task_states(&self) -> std::result::Result<BTreeMap<String, Vec<Value>>, &'static str> {
        let rows = self
            .rows(
                "SELECT to_string(uuid), last_execution_start, last_execution_end, last_execution_result
                 FROM table(system.task_execution_state)",
                &[],
            )
            .await
            .map_err(|e| {
                if readonly_refusal(&e) {
                    TASKS_NO_STATE_READONLY
                } else {
                    TASKS_NO_STATE
                }
            })?;
        Ok(rows
            .into_iter()
            .filter_map(|r| Some((opt(r.first())?, r[1..].to_vec())))
            .collect())
    }

    async fn task(&self, db: &str, name: &str) -> Result<Task> {
        let row = self
            .tasks_in(db)
            .await
            .map_err(|e| {
                if no_tasks(&e) {
                    Error::Unsupported(NO_TASKS.into())
                } else {
                    e
                }
            })?
            .0
            .into_iter()
            .find(|r| opt(r.first()).as_deref() == Some(name))
            .ok_or_else(|| Error::Query(format!("no existe la tarea {db}.{name}")))?;
        let def = self.task_create(db, name).await.unwrap_or_default();
        let last = self
            .task_states()
            .await
            .ok()
            .and_then(|mut s| s.remove(&opt(row.get(1)).unwrap_or_default()));
        Ok(Task {
            database: db.into(),
            row,
            def,
            last,
        })
    }

    /// Sends `sql` on its own request and returns once the server took it:
    /// an early refusal comes back as the error, and a run still going after
    /// [`EXECUTE_WAIT`] goes on without DBine waiting for it.
    async fn execute_detached(&self, sql: &str) -> Result<()> {
        let conn = self.conn.clone();
        let mut query: Vec<(String, String)> = vec![("database".into(), self.database.clone())];
        if self.sends_readonly() {
            query.push(("readonly".into(), "1".into()));
        }
        let sql = sql.to_string();
        let mut run = tokio::spawn(async move {
            let resp = conn
                .post()
                .query(&query)
                .body(sql)
                .send()
                .await
                .map_err(Error::query)?;
            let status = resp.status();
            let text = resp.text().await.unwrap_or_default();
            if status.is_success() {
                match crate::body_exception(&text) {
                    Some(e) => Err(e),
                    None => Ok(()),
                }
            } else {
                Err(crate::server_error(status.as_u16(), text.trim()))
            }
        });
        match tokio::time::timeout(EXECUTE_WAIT, &mut run).await {
            Ok(joined) => joined.map_err(|e| Error::Query(e.to_string()))?,
            // Still running: it goes on by itself.
            Err(_) => Ok(()),
        }
    }
}

/// The server refused something because the request or the profile is
/// read-only (`READONLY`, 164).
fn readonly_refusal(e: &Error) -> bool {
    let m = e.to_string();
    m.contains("(READONLY)") || m.contains("Code: 164.")
}

/// The server has no tasks (`SHOW TASKS` is a syntax error before 3.0).
fn no_tasks(e: &Error) -> bool {
    let m = e.to_string();
    m.contains("Syntax error") || m.contains("SYNTAX_ERROR")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn ids_round_trip_and_refuse_anything_else() {
        let id = job_id("my db", "v`1\\");
        assert_eq!(id, "`my db`.`v\\`1\\\\`");
        assert_eq!(parse_id(&id), Some(("my db".into(), "v`1\\".into())));
        assert_eq!(
            parse_id("`a`.`b`"),
            Some(("a".to_string(), "b".to_string()))
        );
        for bad in [
            "",
            "a.b",
            "`a`.`b`; DROP TABLE x",
            "`a`.`b",
            "`a``b`",
            "`a`.``",
            "`a`.`b`.`c`",
            "`a`.`b\n`",
            "x' OR 1=1 --",
        ] {
            assert_eq!(parse_id(bad), None, "{bad}");
        }
    }

    #[test]
    fn epoch_seconds_become_rfc3339() {
        assert_eq!(rfc3339(0), "1970-01-01T00:00:00Z");
        assert_eq!(rfc3339(1_794_488_400), "2026-11-12T13:00:00Z");
        assert_eq!(rfc3339(951_782_400), "2000-02-29T00:00:00Z");
        assert_eq!(
            utc_text("2026-11-12 12:53:21.593").as_deref(),
            Some("2026-11-12T12:53:21Z")
        );
        assert_eq!(utc_text("1970-01-01 00:00:00.000"), None);
        assert_eq!(utc_text("ayer"), None);
        assert_eq!(
            utc_millis("2026-11-12 12:53:22.001").unwrap()
                - utc_millis("2026-11-12 12:53:21.593").unwrap(),
            408
        );
        assert_eq!(
            utc_millis("2026-11-12 13:00:00").unwrap() / 1000,
            1_794_488_400
        );
    }

    #[test]
    fn the_refresh_clause_is_read_from_the_definition() {
        let c = refresh_clause(
            "CREATE MATERIALIZED VIEW db.mv3 REFRESH EVERY 1 DAY OFFSET 2 HOUR DEPENDS ON db.a, `x y`.b TO db.dst3 (`c` UInt64) DEFINER = dbine SQL SECURITY DEFINER COMMENT 'nightly AS x' AS SELECT count() AS c FROM db.src",
        );
        assert_eq!(c.schedule.as_deref(), Some("EVERY 1 DAY OFFSET 2 HOUR"));
        assert_eq!(c.depends_on.as_deref(), Some("db.a, x y.b"));
        assert_eq!(c.target.as_deref(), Some("db.dst3"));
        assert_eq!(c.definer.as_deref(), Some("dbine"));
        assert!(!c.append);
        let c = refresh_clause(
            "CREATE MATERIALIZED VIEW `REFRESH x`.`a b` REFRESH AFTER 30 MINUTE RANDOMIZE FOR 10 SECOND APPEND TO db.dst (`c` UInt64) AS SELECT 1",
        );
        assert_eq!(
            c.schedule.as_deref(),
            Some("AFTER 30 MINUTE RANDOMIZE FOR 10 SECOND")
        );
        assert!(c.append);
        let c = refresh_clause(
            "CREATE MATERIALIZED VIEW db.mv REFRESH EVERY 1 HOUR (`c` UInt64) ENGINE = MergeTree ORDER BY c AS SELECT 1",
        );
        assert_eq!(c.schedule.as_deref(), Some("EVERY 1 HOUR"));
        assert_eq!(c.target, None);
        assert_eq!(
            refresh_clause("CREATE MATERIALIZED VIEW db.mv TO db.t AS SELECT 1"),
            RefreshClause::default()
        );
    }

    #[test]
    fn the_list_reads_only_the_columns_the_server_has() {
        let new: BTreeSet<String> = [
            "status",
            "last_success_time",
            "last_success_duration_ms",
            "retry",
        ]
        .iter()
        .map(|s| s.to_string())
        .collect();
        let sql = refreshes_sql(&new, false);
        assert!(sql.contains("toUnixTimestamp(r.last_success_time)"));
        assert!(sql.contains("r.last_success_duration_ms"));
        assert!(!sql.contains("r.duration_ms"));
        assert!(!sql.contains("r.last_refresh_result"));
        assert!(!sql.contains("{db:String}"));
        let one = refreshes_sql(&new, true);
        assert!(one.contains("r.database = {db:String} AND r.view = {v:String}"));
    }

    fn refresh_row(status: &str, success: Value, exception: &str, retry: u64) -> Vec<Value> {
        // database, view, REFRESH_COLUMNS…, create, comment, select.
        vec![
            json!("db"),
            json!("mv"),
            json!(status),
            success,
            json!(1_794_487_900),
            json!(1_794_488_400),
            json!(exception),
            json!("12"),
            Value::Null,
            json!(""),
            json!(retry),
            json!(0.5),
            json!("10"),
            json!("1"),
            json!(""),
            json!("CREATE MATERIALIZED VIEW db.mv REFRESH EVERY 1 HOUR DEFINER = dbine SQL SECURITY DEFINER AS SELECT 1"),
            json!("cada hora"),
            json!("SELECT 1"),
        ]
    }

    #[test]
    fn rows_become_refresh_jobs() {
        let r = Refresh::from_row(&refresh_row("Scheduled", json!(1_794_487_852), "", 0));
        let j = r.job(History::Readable);
        assert_eq!(j.id, "`db`.`mv`");
        assert_eq!(j.kind, REFRESH_KIND);
        assert_eq!(j.group.as_deref(), Some("db"));
        assert_eq!(j.owner.as_deref(), Some("dbine"));
        assert_eq!(j.description.as_deref(), Some("cada hora"));
        assert_eq!(j.schedule.as_deref(), Some("EVERY 1 HOUR"));
        assert!(j.enabled && !j.running);
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T12:50:52Z"));
        assert_eq!(j.last_duration_ms, Some(12));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-12T13:00:00Z"));
        assert!(j.can_run && j.can_stop && j.can_enable && j.has_history && j.has_script);

        let off = Refresh::from_row(&refresh_row("Disabled", json!(1_794_487_852), "", 0))
            .job(History::NoAccess);
        assert!(!off.enabled && !off.can_run && off.next_run.is_none() && !off.has_history);

        let cancelled = Refresh::from_row(&refresh_row("Scheduled", Value::Null, "cancelled", 0));
        assert_eq!(
            cancelled.job(History::Readable).last_outcome,
            Some(JobOutcome::Canceled)
        );
        assert_eq!(
            cancelled.job(History::Readable).last_started.as_deref(),
            Some("2026-11-12T12:51:40Z")
        );
        let failing = Refresh::from_row(&refresh_row("Scheduled", Value::Null, "Code: 60", 1));
        assert_eq!(
            failing.job(History::Readable).last_outcome,
            Some(JobOutcome::Retrying)
        );
        let failed = Refresh::from_row(&refresh_row("Scheduled", Value::Null, "Code: 60", 0));
        assert_eq!(
            failed.job(History::Readable).last_outcome,
            Some(JobOutcome::Failed)
        );
        let running = Refresh::from_row(&refresh_row("Running", Value::Null, "", 0));
        let j = running.job(History::Readable);
        assert!(j.running && j.next_run.is_none());
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));

        let d = r.detail(History::Readable);
        assert_eq!(d.steps.len(), 1);
        assert_eq!(d.steps[0].command.as_deref(), Some("SELECT 1"));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Destino" && p.value == "Tabla interna de la vista"));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Filas leídas" && p.value == "10"));
    }

    #[test]
    fn old_servers_report_the_last_result() {
        // 23.12–24.9: `last_refresh_result` and `duration_ms`.
        let mut row = refresh_row("Scheduled", json!(1_794_487_852), "", 0);
        row[7] = Value::Null;
        row[8] = json!("40");
        row[9] = json!("Error");
        let j = Refresh::from_row(&row).job(History::Readable);
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_duration_ms, Some(40));
    }

    #[test]
    fn query_log_rows_become_runs() {
        let ok = run_row(&[
            json!("q1"),
            json!(1_794_487_852),
            json!("7"),
            json!("QueryFinish"),
            json!(0),
            json!(""),
        ]);
        assert_eq!(ok.id.as_deref(), Some("q1"));
        assert_eq!(ok.outcome, JobOutcome::Succeeded);
        assert_eq!(ok.started.as_deref(), Some("2026-11-12T12:50:52Z"));
        assert_eq!(ok.duration_ms, Some(7));
        assert!(ok.message.is_none());
        let cancelled = run_row(&[
            json!("q2"),
            json!(1_794_487_852),
            json!(2030),
            json!("ExceptionWhileProcessing"),
            json!(394),
            json!("Code: 394. Refresh cancelled"),
        ]);
        assert_eq!(cancelled.outcome, JobOutcome::Canceled);
        let failed = run_row(&[
            json!("q3"),
            json!(1_794_487_852),
            json!(1),
            json!("ExceptionBeforeStart"),
            json!(60),
            json!("Code: 60. Unknown table"),
        ]);
        assert_eq!(failed.outcome, JobOutcome::Failed);
        assert_eq!(failed.message.as_deref(), Some("Code: 60. Unknown table"));
        let sql = runs_sql(20);
        assert!(sql.contains("client_name = 'refreshable materialized view'"));
        assert!(sql.contains("log_comment = {lc:String}") && sql.ends_with("LIMIT 20"));
        // Only the view's own tag: a view that reads this one lists it in
        // `tables` too, so `tables` isn't looked at.
        assert!(!sql.contains("tables"));
        assert!(sql.contains("startsWith(log_comment, {lca:String})"));
        let (lc, lca) = refresh_comments("db", "mv");
        assert_eq!(lc, "refresh of db.mv");
        assert_eq!(lca, "refresh of db.mv (attempt ");
    }

    #[test]
    fn history_needs_tagged_refreshes() {
        assert!(version_at_least("25.4.1.2934", 25, 4));
        assert!(version_at_least("26.9.1.1", 25, 4));
        assert!(!version_at_least("25.3.14.14", 25, 4));
        assert!(!version_at_least("24.12.1.1614", 25, 4));
        assert!(!version_at_least("", 25, 4));
        let r = Refresh::from_row(&refresh_row("Scheduled", json!(1_794_487_852), "", 0));
        assert!(!r.job(History::Untagged).has_history);
        assert_eq!(History::Untagged.note(), Some(UNTAGGED));
        assert_eq!(History::NoAccess.note(), Some(NO_QUERY_LOG));
        assert_eq!(History::Readable.note(), None);
        assert!(readonly_refusal(&Error::Query(
            "Code: 164. DB::Exception: Cannot modify 'verbose' setting in readonly mode. (READONLY) (version 3.0.31)".into()
        )));
        assert!(!readonly_refusal(&Error::Query(
            "Code: 62. DB::Exception: Syntax error (SYNTAX_ERROR)".into()
        )));
    }

    #[test]
    fn proton_tasks_are_read() {
        let def = task_def(
            "CREATE TASK default.t\nSCHEDULE 1h\nTIMEOUT 30m\nINTO default.`out put`\nAS \n  SELECT 1 AS x\n  FROM numbers(3)",
        );
        assert_eq!(def.schedule.as_deref(), Some("1h"));
        assert_eq!(def.timeout.as_deref(), Some("30m"));
        assert_eq!(def.into.as_deref(), Some("default.out put"));
        assert_eq!(
            def.query.as_deref(),
            Some("SELECT 1 AS x\n  FROM numbers(3)")
        );
        let row = vec![
            json!("t"),
            json!("489504b7-812a-4ce9-a126-0e4e35c8d0cd"),
            json!(9),
            json!(1),
            json!("2026-11-12 12:53:53.074"),
            json!("default"),
            json!("2026-11-12 12:52:27.312"),
            json!("ana"),
        ];
        let task = Task {
            database: "default".into(),
            row,
            def,
            last: Some(vec![
                json!("2026-11-12 12:53:21.593"),
                json!("2026-11-12 12:53:21.594"),
                json!("OK"),
            ]),
        };
        let j = task.job();
        assert_eq!(j.id, "`default`.`t`");
        assert_eq!(j.kind, TASK_KIND);
        assert!(!j.enabled && !j.can_run && j.can_enable && !j.can_stop);
        assert!(!j.has_history && j.has_script);
        assert_eq!(j.owner.as_deref(), Some("ana"));
        assert_eq!(j.schedule.as_deref(), Some("1h"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Succeeded));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T12:53:21Z"));
        assert_eq!(j.last_duration_ms, Some(1));
        let d = task.detail();
        assert_eq!(d.steps[0].target.as_deref(), Some("default.out put"));
        assert!(d
            .properties
            .iter()
            .any(|p| p.label == "Creada por" && p.value == "ana"));
    }

    #[test]
    fn words_keep_quoted_text_whole() {
        assert_eq!(
            words("a `b c` 'd \\' e' \"f g\"  h"),
            ["a", "`b c`", "'d \\' e'", "\"f g\"", "h"]
        );
        assert_eq!(plain("`x y`.`b\\`c`"), "x y.b`c");
    }
}
