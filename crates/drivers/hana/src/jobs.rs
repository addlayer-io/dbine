//! Server jobs ([`dbine_driver::Session::jobs`]): SAP HANA's scheduler
//! jobs (`CREATE SCHEDULER JOB … CRON … PROCEDURE …`, SAP HANA Cloud and
//! the HANA 2.0 versions that have them).
//!
//! - `SYS.SCHEDULER_JOBS`: the jobs the user created or may alter or drop
//!   (all of them with CATALOG READ), with their cron, time window,
//!   procedure and whether they're enabled; `SYS.SCHEDULER_JOB_PARAMETERS`,
//!   the procedure's arguments.
//! - `SYS.M_SCHEDULER_JOBS`: the status and its history (`SCHEDULED` with
//!   the time it's due, `RUNNING`, `SUCCESS`, `ERROR` with the message),
//!   the runs and the next start.
//! - `ALTER SCHEDULER JOB … ENABLE | DISABLE`; stopping a run cancels the
//!   statement of the connection running it (`ALTER SYSTEM CANCEL
//!   SESSION`), where the view reports it. HANA has no statement to run a
//!   scheduler job now.
//!
//! The views' columns differ between versions: the queries read the ones
//! `SYS.VIEW_COLUMNS` lists. Times are the server's local time, turned
//! into UTC by `LOCALTOUTC` with the server's time zone, so a time from the
//! other side of a daylight saving change gets its own offset (SAP HANA
//! Cloud runs in UTC). A version without the views answers an empty list
//! with a note.
//!
//! The id is `"SCHEMA"."JOB"` (quoted as HANA quotes names); the names it
//! holds go into the catalog queries as string literals and into `ALTER`
//! quoted again.

use crate::monitor::{query, Set};
use crate::quote;
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, PropertyInfo, Result, ServerJob,
};
use hdbconnect_async::Connection;
use std::collections::{BTreeMap, BTreeSet};

const KIND: &str = "Scheduler job";
const MAX_MESSAGE: usize = 4000;

const NO_SCHEDULER: &str =
    "esta versión de SAP HANA no tiene scheduler jobs (CREATE SCHEDULER JOB)";
const PARTIAL: &str = "se ven los scheduler jobs que creó este usuario o que puede modificar o borrar; para verlos todos hace falta CATALOG READ";
const NO_RUN: &str = "SAP HANA no tiene una sentencia para ejecutar un scheduler job a mano";

/// The columns each view has on this server.
#[derive(Debug, Default, Clone)]
pub(crate) struct Columns {
    pub jobs: BTreeSet<String>,
    pub status: BTreeSet<String>,
    pub params: BTreeSet<String>,
}

pub(crate) const COLUMNS: &str = "SELECT VIEW_NAME, COLUMN_NAME FROM SYS.VIEW_COLUMNS
 WHERE SCHEMA_NAME = 'SYS' AND VIEW_NAME IN ('SCHEDULER_JOBS', 'M_SCHEDULER_JOBS', 'SCHEDULER_JOB_PARAMETERS')";

pub(crate) fn columns(set: &Set) -> Columns {
    let mut c = Columns::default();
    for r in 0..set.rows.len() {
        let (Some(view), Some(col)) = (set.get(r, "VIEW_NAME"), set.get(r, "COLUMN_NAME")) else {
            continue;
        };
        let col = col.to_string();
        match view {
            "SCHEDULER_JOBS" => c.jobs.insert(col),
            "M_SCHEDULER_JOBS" => c.status.insert(col),
            "SCHEDULER_JOB_PARAMETERS" => c.params.insert(col),
            _ => false,
        };
    }
    c
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `"SCHEMA"."JOB"`.
pub(crate) fn job_id(schema: &str, name: &str) -> String {
    format!("{}.{}", quote(schema), quote(name))
}

fn unquote(s: &str) -> Option<(String, &str)> {
    let rest = s.strip_prefix('"')?;
    let mut out = String::new();
    let mut chars = rest.char_indices().peekable();
    while let Some((i, c)) = chars.next() {
        if c == '"' {
            if chars.peek().map(|(_, n)| *n) == Some('"') {
                chars.next();
                out.push('"');
            } else {
                return Some((out, &rest[i + 1..]));
            }
        } else {
            out.push(c);
        }
    }
    None
}

/// The schema and name of an id [`job_id`] made; `None` for anything else.
pub(crate) fn parse_id(id: &str) -> Option<(String, String)> {
    if id.len() > 1024 {
        return None;
    }
    let (schema, rest) = unquote(id)?;
    let (name, rest) = unquote(rest.strip_prefix('.')?)?;
    let ok = |s: &str| !s.is_empty() && !s.chars().any(char::is_control);
    (rest.is_empty() && ok(&schema) && ok(&name)).then_some((schema, name))
}

fn bad_id(id: &str) -> Error {
    Error::Query(format!(
        "«{}» no es un id de scheduler job de SAP HANA",
        id.chars().take(200).collect::<String>()
    ))
}

/// A local timestamp column as RFC 3339 UTC text. `LOCALTOUTC` without a
/// time zone converts from the server's, with the offset in force at that
/// time (not today's).
fn utc(col: &str) -> String {
    format!("REPLACE(TO_VARCHAR(LOCALTOUTC({col}), 'YYYY-MM-DD HH24:MI:SS'), ' ', 'T') || 'Z'")
}

/// `col AS col` when the view has it, else `NULL AS col`.
fn pick(has: &BTreeSet<String>, col: &str, expr: impl FnOnce() -> String) -> String {
    if has.contains(col) {
        format!("{} AS {col}", expr())
    } else {
        format!("NULL AS {col}")
    }
}

/// The jobs (one, with `one`: schema and name).
pub(crate) fn jobs_sql(c: &Columns, one: Option<(&str, &str)>) -> String {
    let mut cols = vec!["SCHEMA_NAME".to_string(), "SCHEDULER_JOB_NAME".to_string()];
    for col in [
        "USER_NAME",
        "PROCEDURE_SCHEMA_NAME",
        "PROCEDURE_NAME",
        "CRON",
        "IS_ENABLED",
        "IS_VALID",
        "COMMENTS",
    ] {
        cols.push(pick(&c.jobs, col, || col.to_string()));
    }
    for col in ["START_TIME", "END_TIME", "CREATE_TIME"] {
        cols.push(pick(&c.jobs, col, || utc(col)));
    }
    // The window as written, for the script.
    for col in ["START_TIME", "END_TIME"] {
        let alias = format!("{col}_LOCAL");
        cols.push(if c.jobs.contains(col) {
            format!("TO_VARCHAR({col}, 'YYYY-MM-DD HH24:MI:SS') AS {alias}")
        } else {
            format!("NULL AS {alias}")
        });
    }
    let filter = one
        .map(|(s, n)| {
            format!(
                " WHERE SCHEMA_NAME = {} AND SCHEDULER_JOB_NAME = {}",
                lit(s),
                lit(n)
            )
        })
        .unwrap_or_default();
    format!(
        "SELECT {} FROM SYS.SCHEDULER_JOBS{filter} ORDER BY SCHEMA_NAME, SCHEDULER_JOB_NAME",
        cols.join(", ")
    )
}

fn status_cols(c: &Columns) -> Vec<String> {
    let mut cols = vec![
        "SCHEMA_NAME".to_string(),
        "SCHEDULER_JOB_NAME".to_string(),
        "STATUS".to_string(),
    ];
    cols.push(
        pick(&c.status, "START_TIME", || utc("START_TIME"))
            .replace(" AS START_TIME", " AS START_UTC"),
    );
    cols.push(
        pick(&c.status, "END_TIME", || utc("END_TIME")).replace(" AS END_TIME", " AS END_UTC"),
    );
    cols.push(
        if c.status.contains("START_TIME") && c.status.contains("END_TIME") {
            "NANO100_BETWEEN(START_TIME, END_TIME) / 10000 AS DURATION_MS".into()
        } else {
            "NULL AS DURATION_MS".into()
        },
    );
    cols.push(pick(&c.status, "ERROR_MESSAGE", || {
        format!("TO_NVARCHAR(SUBSTR(ERROR_MESSAGE, 1, {MAX_MESSAGE}))")
    }));
    cols.push(pick(&c.status, "CONNECTION_ID", || "CONNECTION_ID".into()));
    cols.push(if c.status.contains("START_TIME") {
        "CASE WHEN START_TIME > CURRENT_TIMESTAMP THEN 1 ELSE 0 END AS FUTURE".into()
    } else {
        "0 AS FUTURE".into()
    });
    cols
}

fn status_order(c: &Columns) -> &'static str {
    if c.status.contains("START_TIME") {
        "START_TIME DESC"
    } else {
        "STATUS"
    }
}

/// The newest status row of each kind (finished, running, scheduled) per
/// job: what the list shows.
pub(crate) fn last_status_sql(c: &Columns) -> String {
    format!(
        "SELECT * FROM (SELECT {}, ROW_NUMBER() OVER (PARTITION BY SCHEMA_NAME, SCHEDULER_JOB_NAME,
                CASE WHEN STATUS IN ('SUCCESS', 'ERROR') THEN 1 WHEN STATUS = 'RUNNING' THEN 2 ELSE 3 END
                ORDER BY {}) AS RN
           FROM SYS.M_SCHEDULER_JOBS) WHERE RN = 1",
        status_cols(c).join(", "),
        status_order(c)
    )
}

/// One job's runs, newest first (`RUNNING` rows of a run that then ended
/// are dropped after reading).
pub(crate) fn runs_sql(c: &Columns, schema: &str, name: &str, limit: u32) -> String {
    format!(
        "SELECT TOP {} {} FROM SYS.M_SCHEDULER_JOBS
          WHERE SCHEMA_NAME = {} AND SCHEDULER_JOB_NAME = {} AND STATUS IN ('RUNNING', 'SUCCESS', 'ERROR')
          ORDER BY {}",
        limit.saturating_mul(2),
        status_cols(c).join(", "),
        lit(schema),
        lit(name),
        status_order(c)
    )
}

pub(crate) fn params_sql(c: &Columns, schema: &str, name: &str) -> Option<String> {
    let value = ["PARAMETER_VALUE", "VALUE", "PARAMETER_VALUE_STRING"]
        .into_iter()
        .find(|v| c.params.contains(*v))?;
    c.params.contains("PARAMETER_NAME").then(|| {
        let order = if c.params.contains("POSITION") {
            "POSITION"
        } else {
            "PARAMETER_NAME"
        };
        format!(
            "SELECT PARAMETER_NAME, TO_NVARCHAR({value}) AS PARAMETER_VALUE FROM SYS.SCHEDULER_JOB_PARAMETERS
              WHERE SCHEMA_NAME = {} AND SCHEDULER_JOB_NAME = {} ORDER BY {order}",
            lit(schema),
            lit(name)
        )
    })
}

fn s(set: &Set, r: usize, col: &str) -> Option<String> {
    set.get(r, col).map(str::to_string)
}

fn n(set: &Set, r: usize, col: &str) -> Option<f64> {
    set.get(r, col).and_then(|v| v.parse::<f64>().ok())
}

fn outcome(status: &str) -> JobOutcome {
    match status {
        "SUCCESS" => JobOutcome::Succeeded,
        "ERROR" => JobOutcome::Failed,
        "RUNNING" => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

fn run_at(set: &Set, r: usize) -> JobRun {
    let status = s(set, r, "STATUS").unwrap_or_default();
    let finished = matches!(status.as_str(), "SUCCESS" | "ERROR");
    JobRun {
        id: None,
        started: s(set, r, "START_UTC"),
        duration_ms: n(set, r, "DURATION_MS")
            .filter(|_| finished)
            .map(|v| v.max(0.0) as u64),
        outcome: outcome(&status),
        trigger: None,
        message: s(set, r, "ERROR_MESSAGE"),
        steps: Vec::new(),
    }
}

/// The status rows ([`last_status_sql`]) by job.
#[derive(Debug, Default, Clone)]
pub(crate) struct Status {
    pub last: Option<JobRun>,
    pub running: Option<(Option<String>, Option<u64>)>,
    pub next: Option<String>,
}

pub(crate) fn statuses(set: &Set) -> BTreeMap<(String, String), Status> {
    let mut out: BTreeMap<(String, String), Status> = BTreeMap::new();
    let mut running: BTreeMap<(String, String), (Option<String>, Option<u64>)> = BTreeMap::new();
    for r in 0..set.rows.len() {
        let (Some(schema), Some(name)) =
            (s(set, r, "SCHEMA_NAME"), s(set, r, "SCHEDULER_JOB_NAME"))
        else {
            continue;
        };
        let key = (schema, name);
        let status = s(set, r, "STATUS").unwrap_or_default();
        let e = out.entry(key.clone()).or_default();
        match status.as_str() {
            "SUCCESS" | "ERROR" => e.last = Some(run_at(set, r)),
            "RUNNING" => {
                running.insert(
                    key,
                    (
                        s(set, r, "START_UTC"),
                        n(set, r, "CONNECTION_ID").map(|v| v as u64),
                    ),
                );
            }
            "SCHEDULED" if n(set, r, "FUTURE") == Some(1.0) => e.next = s(set, r, "START_UTC"),
            _ => {}
        }
    }
    // A run is going on when its RUNNING row is newer than the last end.
    for (key, (start, conn)) in running {
        let e = out.entry(key).or_default();
        let ended = e.last.as_ref().and_then(|l| l.started.clone());
        if ended.is_none() || start > ended {
            e.running = Some((start, conn));
        }
    }
    out
}

/// A job row ([`jobs_sql`]) and its status as a job.
pub(crate) fn job(set: &Set, r: usize, st: Option<&Status>, c: &Columns) -> ServerJob {
    let schema = s(set, r, "SCHEMA_NAME").unwrap_or_default();
    let name = s(set, r, "SCHEDULER_JOB_NAME").unwrap_or_default();
    let enabled = s(set, r, "IS_ENABLED").is_some_and(|v| v.eq_ignore_ascii_case("TRUE"));
    let running = st.and_then(|s| s.running.as_ref());
    let last = st.and_then(|s| s.last.as_ref());
    let owner = s(set, r, "USER_NAME");
    let system = hana_own(&schema) || owner.as_deref().is_some_and(hana_own);
    ServerJob {
        id: job_id(&schema, &name),
        name,
        kind: KIND.into(),
        group: Some(schema),
        description: s(set, r, "COMMENTS"),
        owner,
        enabled,
        schedule: s(set, r, "CRON"),
        running: running.is_some(),
        last_outcome: last
            .map(|l| l.outcome)
            .or(running.map(|_| JobOutcome::Running)),
        last_started: last.and_then(|l| l.started.clone()),
        last_duration_ms: last.and_then(|l| l.duration_ms),
        next_run: st.and_then(|s| s.next.clone()).filter(|_| enabled),
        can_run: false,
        can_stop: running.is_some_and(|(_, conn)| conn.is_some()),
        can_enable: true,
        has_history: !c.status.is_empty(),
        has_script: true,
        system,
    }
}

/// A schema or user of HANA's own: SYS, or a name starting with `_SYS`,
/// which HANA reserves for the database itself.
fn hana_own(name: &str) -> bool {
    name == "SYS" || name.starts_with("_SYS")
}

fn procedure(set: &Set, r: usize) -> Option<String> {
    let name = s(set, r, "PROCEDURE_NAME")?;
    Some(match s(set, r, "PROCEDURE_SCHEMA_NAME") {
        Some(ps) => format!("{}.{}", quote(&ps), quote(&name)),
        None => quote(&name),
    })
}

/// A parameter's value as SQL: numbers and literals as they are, the rest
/// quoted.
fn param_value(v: &str) -> String {
    let v = v.trim();
    let quoted = v.len() >= 2 && v.starts_with('\'') && v.ends_with('\'');
    if quoted || (!v.is_empty() && v.parse::<f64>().is_ok()) {
        v.to_string()
    } else {
        lit(v)
    }
}

fn param_name(n: &str) -> String {
    if !n.is_empty()
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !n.starts_with(|c: char| c.is_ascii_digit())
    {
        n.to_string()
    } else {
        quote(n)
    }
}

pub(crate) fn detail(
    set: &Set,
    r: usize,
    st: Option<&Status>,
    c: &Columns,
    params: &[(String, String)],
) -> JobDetail {
    let job = job(set, r, st, c);
    let mut p = Vec::new();
    let mut add = |group: &str, label: &str, value: Option<String>| {
        if let Some(value) = value.filter(|v| !v.is_empty()) {
            p.push(PropertyInfo {
                group: group.into(),
                label: label.into(),
                value,
            });
        }
    };
    add("", "Procedimiento", procedure(set, r));
    add("", "Usuario que lo ejecuta", s(set, r, "USER_NAME"));
    add("", "Desde", s(set, r, "START_TIME"));
    add("", "Hasta", s(set, r, "END_TIME"));
    add("", "Válido", s(set, r, "IS_VALID"));
    add("", "Creado", s(set, r, "CREATE_TIME"));
    for (k, v) in params {
        add("Parámetros", k, Some(v.clone()));
    }
    let args = params
        .iter()
        .map(|(k, v)| format!("{} => {}", param_name(k), param_value(v)))
        .collect::<Vec<_>>()
        .join(", ");
    JobDetail {
        steps: vec![JobStep {
            name: s(set, r, "PROCEDURE_NAME").unwrap_or_default(),
            kind: Some("Procedimiento".into()),
            target: None,
            command: procedure(set, r).map(|p| format!("CALL {p}({args})")),
        }],
        job,
        schedules: Vec::new(),
        properties: p,
        notes: Vec::new(),
    }
}

/// `CREATE SCHEDULER JOB` as the job is now.
pub(crate) fn script(set: &Set, r: usize, params: &[(String, String)]) -> String {
    let schema = s(set, r, "SCHEMA_NAME").unwrap_or_default();
    let name = s(set, r, "SCHEDULER_JOB_NAME").unwrap_or_default();
    let mut out = format!(
        "CREATE SCHEDULER JOB {}\n  CRON {}",
        job_id(&schema, &name),
        lit(&s(set, r, "CRON").unwrap_or_default())
    );
    if let Some(from) = s(set, r, "START_TIME_LOCAL") {
        out.push_str(&format!("\n  FROM {}", lit(&from)));
    }
    if let Some(until) = s(set, r, "END_TIME_LOCAL") {
        out.push_str(&format!("\n  UNTIL {}", lit(&until)));
    }
    let enabled = s(set, r, "IS_ENABLED").is_some_and(|v| v.eq_ignore_ascii_case("TRUE"));
    out.push_str(if enabled { "\n  ENABLE" } else { "\n  DISABLE" });
    if let Some(p) = procedure(set, r) {
        out.push_str(&format!("\n  PROCEDURE {p}"));
    }
    if !params.is_empty() {
        let list = params
            .iter()
            .map(|(k, v)| format!("{} = {}", param_name(k), param_value(v)))
            .collect::<Vec<_>>()
            .join(", ");
        out.push_str(&format!("\n  PARAMETERS {list}"));
    }
    out.push_str(";\n");
    out
}

// ------------------------------------------------------------- the calls

async fn columns_of(conn: &Connection) -> Result<Columns> {
    Ok(columns(&query(conn, COLUMNS).await?))
}

async fn status_map(conn: &Connection, c: &Columns) -> BTreeMap<(String, String), Status> {
    if c.status.is_empty() {
        return BTreeMap::new();
    }
    match query(conn, &last_status_sql(c)).await {
        Ok(set) => statuses(&set),
        Err(e) => {
            tracing::debug!("hana jobs: M_SCHEDULER_JOBS: {e}");
            BTreeMap::new()
        }
    }
}

async fn sees_all(conn: &Connection) -> bool {
    query(
        conn,
        "SELECT COUNT(*) AS N FROM SYS.EFFECTIVE_PRIVILEGES
          WHERE USER_NAME = CURRENT_USER AND PRIVILEGE IN ('CATALOG READ', 'DATA ADMIN')",
    )
    .await
    .ok()
    .and_then(|s| s.get(0, "N").and_then(|v| v.parse::<u64>().ok()))
    .is_some_and(|n| n > 0)
}

pub(crate) async fn list(conn: &Connection) -> Result<JobList> {
    let c = columns_of(conn).await?;
    if c.jobs.is_empty() {
        return Ok(JobList {
            jobs: Vec::new(),
            notes: vec![NO_SCHEDULER.into()],
        });
    }
    let set = query(conn, &jobs_sql(&c, None)).await?;
    let st = status_map(conn, &c).await;
    let jobs = (0..set.rows.len())
        .map(|r| {
            let key = (
                s(&set, r, "SCHEMA_NAME").unwrap_or_default(),
                s(&set, r, "SCHEDULER_JOB_NAME").unwrap_or_default(),
            );
            job(&set, r, st.get(&key), &c)
        })
        .collect();
    let mut notes = Vec::new();
    if !sees_all(conn).await {
        notes.push(PARTIAL.to_string());
    }
    Ok(JobList { jobs, notes })
}

/// The job's row (in a one-row set), the columns and its parameters.
async fn one(
    conn: &Connection,
    id: &str,
) -> Result<(Set, Columns, String, String, Vec<(String, String)>)> {
    let (schema, name) = parse_id(id).ok_or_else(|| bad_id(id))?;
    let c = columns_of(conn).await?;
    if c.jobs.is_empty() {
        return Err(Error::Unsupported(NO_SCHEDULER.into()));
    }
    let set = query(conn, &jobs_sql(&c, Some((&schema, &name)))).await?;
    if set.rows.is_empty() {
        return Err(Error::Query(format!(
            "no existe el scheduler job {schema}.{name}"
        )));
    }
    let params = match params_sql(&c, &schema, &name) {
        Some(sql) => match query(conn, &sql).await {
            Ok(p) => (0..p.rows.len())
                .filter_map(|r| {
                    Some((
                        s(&p, r, "PARAMETER_NAME")?,
                        s(&p, r, "PARAMETER_VALUE").unwrap_or_default(),
                    ))
                })
                .collect(),
            Err(e) => {
                tracing::debug!("hana jobs: SCHEDULER_JOB_PARAMETERS: {e}");
                Vec::new()
            }
        },
        None => Vec::new(),
    };
    Ok((set, c, schema, name, params))
}

pub(crate) async fn info(conn: &Connection, id: &str) -> Result<JobDetail> {
    let (set, c, schema, name, params) = one(conn, id).await?;
    let st = status_map(conn, &c).await;
    Ok(detail(&set, 0, st.get(&(schema, name)), &c, &params))
}

pub(crate) async fn runs(conn: &Connection, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let (_, c, schema, name, _) = one(conn, id).await?;
    if c.status.is_empty() {
        return Err(Error::Unsupported(
            "esta versión de SAP HANA no tiene M_SCHEDULER_JOBS: no hay historial".into(),
        ));
    }
    let limit = limit.clamp(1, MAX_JOB_RUNS);
    let set = query(conn, &runs_sql(&c, &schema, &name, limit)).await?;
    Ok(dedupe_runs(&set, limit))
}

/// The runs newest first, without the `RUNNING` row of a run that ended.
pub(crate) fn dedupe_runs(set: &Set, limit: u32) -> Vec<JobRun> {
    let all: Vec<JobRun> = (0..set.rows.len())
        .filter(|&r| matches!(set.get(r, "STATUS"), Some("RUNNING" | "SUCCESS" | "ERROR")))
        .map(|r| run_at(set, r))
        .collect();
    let ended: BTreeSet<Option<String>> = all
        .iter()
        .filter(|r| r.outcome != JobOutcome::Running)
        .map(|r| r.started.clone())
        .collect();
    all.into_iter()
        .filter(|r| r.outcome != JobOutcome::Running || !ended.contains(&r.started))
        .take(limit as usize)
        .collect()
}

pub(crate) async fn set_enabled(conn: &Connection, id: &str, enabled: bool) -> Result<()> {
    let (_, _, schema, name, _) = one(conn, id).await?;
    let verb = if enabled { "ENABLE" } else { "DISABLE" };
    conn.exec(format!(
        "ALTER SCHEDULER JOB {} {verb}",
        job_id(&schema, &name)
    ))
    .await
    .map_err(|e| {
        Error::Query(format!(
            "no se pudo cambiar el scheduler job {schema}.{name}: {e}"
        ))
    })
}

pub(crate) async fn stop(conn: &Connection, id: &str) -> Result<()> {
    let (_, c, schema, name, _) = one(conn, id).await?;
    let st = status_map(conn, &c).await;
    let Some((_, Some(conn_id))) = st
        .get(&(schema.clone(), name.clone()))
        .and_then(|s| s.running.clone())
    else {
        return Err(Error::Query(format!(
            "el scheduler job {schema}.{name} no tiene una ejecución en curso que se pueda detener"
        )));
    };
    // A number read from the view: safe to put in the statement.
    conn.exec(format!("ALTER SYSTEM CANCEL SESSION '{conn_id}'"))
        .await
        .map_err(|e| {
            Error::Query(format!(
                "no se pudo detener la ejecución (hace falta SESSION ADMIN): {e}"
            ))
        })
}

pub(crate) async fn create_script(conn: &Connection, id: &str) -> Result<String> {
    let (set, _, _, _, params) = one(conn, id).await?;
    Ok(script(&set, 0, &params))
}

pub(crate) fn no_run() -> Error {
    Error::Unsupported(NO_RUN.into())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn set(cols: &[&str], rows: &[&[Option<&str>]]) -> Set {
        Set {
            cols: cols.iter().map(|c| c.to_string()).collect(),
            rows: rows
                .iter()
                .map(|r| r.iter().map(|v| v.map(str::to_string)).collect())
                .collect(),
        }
    }

    fn all_columns() -> Columns {
        let c = set(
            &["VIEW_NAME", "COLUMN_NAME"],
            &[
                &[Some("SCHEDULER_JOBS"), Some("SCHEMA_NAME")],
                &[Some("SCHEDULER_JOBS"), Some("SCHEDULER_JOB_NAME")],
                &[Some("SCHEDULER_JOBS"), Some("CRON")],
                &[Some("SCHEDULER_JOBS"), Some("START_TIME")],
                &[Some("SCHEDULER_JOBS"), Some("IS_ENABLED")],
                &[Some("SCHEDULER_JOBS"), Some("PROCEDURE_NAME")],
                &[Some("SCHEDULER_JOBS"), Some("PROCEDURE_SCHEMA_NAME")],
                &[Some("M_SCHEDULER_JOBS"), Some("STATUS")],
                &[Some("M_SCHEDULER_JOBS"), Some("START_TIME")],
                &[Some("M_SCHEDULER_JOBS"), Some("END_TIME")],
                &[Some("M_SCHEDULER_JOBS"), Some("ERROR_MESSAGE")],
                &[Some("M_SCHEDULER_JOBS"), Some("CONNECTION_ID")],
                &[Some("SCHEDULER_JOB_PARAMETERS"), Some("PARAMETER_NAME")],
                &[Some("SCHEDULER_JOB_PARAMETERS"), Some("PARAMETER_VALUE")],
            ],
        );
        columns(&c)
    }

    #[test]
    fn ids_are_checked() {
        let id = job_id("MY \"S\"", "JOB");
        assert_eq!(id, "\"MY \"\"S\"\"\".\"JOB\"");
        assert_eq!(parse_id(&id), Some(("MY \"S\"".into(), "JOB".into())));
        for bad in [
            "",
            "S.J",
            "\"S\".\"J\"; DROP SCHEMA S",
            "\"S\".\"J",
            "\"S\".\"\"",
            "\"S\".\"J\".\"K\"",
            "\"S\".\"J\n\"",
        ] {
            assert_eq!(parse_id(bad), None, "{bad:?}");
        }
        // Names reach the catalog queries as literals.
        let sql = jobs_sql(&all_columns(), Some(("S", "x' OR '1'='1")));
        assert!(
            sql.contains("SCHEDULER_JOB_NAME = 'x'' OR ''1''=''1'"),
            "{sql}"
        );
    }

    #[test]
    fn queries_read_only_the_columns_there_are() {
        let c = all_columns();
        let sql = jobs_sql(&c, None);
        assert!(sql.contains("CRON AS CRON"));
        assert!(sql.contains("NULL AS COMMENTS") && sql.contains("NULL AS END_TIME"));
        // Each time with its own offset (daylight saving), not today's.
        assert!(sql.contains("TO_VARCHAR(LOCALTOUTC(START_TIME), 'YYYY-MM-DD HH24:MI:SS')"));
        assert!(!sql.contains("SECONDS_BETWEEN(CURRENT_TIMESTAMP"));
        assert!(sql.contains("TO_VARCHAR(START_TIME, 'YYYY-MM-DD HH24:MI:SS') AS START_TIME_LOCAL"));
        let st = last_status_sql(&c);
        assert!(
            st.contains("AS START_UTC") && st.contains("AS DURATION_MS") && st.contains("RN = 1")
        );
        assert!(st.contains("CONNECTION_ID AS CONNECTION_ID"));
        let runs = runs_sql(&c, "S", "J", 20);
        assert!(runs.starts_with("SELECT TOP 40 "));
        assert!(runs.contains("SCHEMA_NAME = 'S' AND SCHEDULER_JOB_NAME = 'J'"));
        assert!(params_sql(&c, "S", "J")
            .unwrap()
            .contains("TO_NVARCHAR(PARAMETER_VALUE)"));
        assert!(params_sql(&Columns::default(), "S", "J").is_none());
    }

    fn job_row(enabled: &str) -> Set {
        set(
            &[
                "SCHEMA_NAME",
                "SCHEDULER_JOB_NAME",
                "USER_NAME",
                "PROCEDURE_SCHEMA_NAME",
                "PROCEDURE_NAME",
                "CRON",
                "IS_ENABLED",
                "COMMENTS",
                "START_TIME",
                "START_TIME_LOCAL",
                "END_TIME_LOCAL",
            ],
            &[&[
                Some("DBADMIN"),
                Some("MINICHECKS"),
                Some("DBADMIN"),
                Some("DBADMIN"),
                Some("HANA_CONFIGURATION_MINICHECKS"),
                Some("* * * * 3 0 0"),
                Some(enabled),
                None,
                Some("2026-01-01T03:00:00Z"),
                Some("2026-01-01 00:00:00"),
                None,
            ]],
        )
    }

    fn status_rows() -> Set {
        set(
            &[
                "SCHEMA_NAME",
                "SCHEDULER_JOB_NAME",
                "STATUS",
                "START_UTC",
                "END_UTC",
                "DURATION_MS",
                "ERROR_MESSAGE",
                "CONNECTION_ID",
                "FUTURE",
            ],
            &[
                &[
                    Some("DBADMIN"),
                    Some("MINICHECKS"),
                    Some("ERROR"),
                    Some("2026-11-12T03:00:00Z"),
                    Some("2026-11-12T03:00:02Z"),
                    Some("2000"),
                    Some("[258] insufficient privilege"),
                    Some("300123"),
                    Some("0"),
                ],
                &[
                    Some("DBADMIN"),
                    Some("MINICHECKS"),
                    Some("RUNNING"),
                    Some("2026-11-12T03:00:00Z"),
                    None,
                    None,
                    None,
                    Some("300123"),
                    Some("0"),
                ],
                &[
                    Some("DBADMIN"),
                    Some("MINICHECKS"),
                    Some("SCHEDULED"),
                    Some("2026-11-13T03:00:00Z"),
                    None,
                    None,
                    None,
                    None,
                    Some("1"),
                ],
            ],
        )
    }

    #[test]
    fn rows_become_jobs() {
        let c = all_columns();
        let st = statuses(&status_rows());
        let key = ("DBADMIN".to_string(), "MINICHECKS".to_string());
        let rows = job_row("TRUE");
        let j = job(&rows, 0, st.get(&key), &c);
        assert_eq!(j.id, "\"DBADMIN\".\"MINICHECKS\"");
        assert_eq!(j.kind, "Scheduler job");
        assert_eq!(j.group.as_deref(), Some("DBADMIN"));
        assert_eq!(j.schedule.as_deref(), Some("* * * * 3 0 0"));
        assert!(j.enabled && !j.running && !j.can_stop && !j.can_run);
        assert!(j.can_enable && j.has_history && j.has_script);
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T03:00:00Z"));
        assert_eq!(j.last_duration_ms, Some(2000));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T03:00:00Z"));
        let off = job(&job_row("FALSE"), 0, st.get(&key), &c);
        assert!(!off.enabled && off.next_run.is_none());
        assert!(!j.system, "a user's job");
        // In HANA's own schemas (or run by its own users): a system job.
        for (schema, user) in [
            ("SYS", "DBADMIN"),
            ("_SYS_STATISTICS", "DBADMIN"),
            ("DBADMIN", "_SYS_TASK"),
        ] {
            let mut own = job_row("TRUE");
            own.rows[0][0] = Some(schema.into());
            own.rows[0][2] = Some(user.into());
            assert!(job(&own, 0, None, &c).system, "{schema} {user}");
        }

        // A RUNNING row newer than the last end: a run going on.
        let going = set(
            &[
                "SCHEMA_NAME",
                "SCHEDULER_JOB_NAME",
                "STATUS",
                "START_UTC",
                "CONNECTION_ID",
                "FUTURE",
            ],
            &[&[
                Some("DBADMIN"),
                Some("MINICHECKS"),
                Some("RUNNING"),
                Some("2026-11-13T03:00:00Z"),
                Some("300200"),
                Some("0"),
            ]],
        );
        let st = statuses(&going);
        let j = job(&rows, 0, st.get(&key), &c);
        assert!(j.running && j.can_stop);
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));

        let d = detail(
            &rows,
            0,
            None,
            &c,
            &[("P_COUNTRY".into(), "USA".into()), ("N".into(), "3".into())],
        );
        assert_eq!(
            d.steps[0].command.as_deref(),
            Some("CALL \"DBADMIN\".\"HANA_CONFIGURATION_MINICHECKS\"(P_COUNTRY => 'USA', N => 3)")
        );
        assert!(d
            .properties
            .iter()
            .any(|p| p.group == "Parámetros" && p.label == "P_COUNTRY"));
    }

    #[test]
    fn runs_drop_the_running_row_of_an_ended_run() {
        let runs = dedupe_runs(&status_rows(), 10);
        assert_eq!(runs.len(), 1, "{runs:?}");
        assert_eq!(runs[0].outcome, JobOutcome::Failed);
        assert_eq!(
            runs[0].message.as_deref(),
            Some("[258] insufficient privilege")
        );
        assert_eq!(runs[0].duration_ms, Some(2000));
    }

    #[test]
    fn the_script_recreates_the_job() {
        let s = script(&job_row("FALSE"), 0, &[("P_COUNTRY".into(), "USA".into())]);
        assert_eq!(
            s,
            "CREATE SCHEDULER JOB \"DBADMIN\".\"MINICHECKS\"\n  CRON '* * * * 3 0 0'\n  FROM '2026-01-01 00:00:00'\n  DISABLE\n  PROCEDURE \"DBADMIN\".\"HANA_CONFIGURATION_MINICHECKS\"\n  PARAMETERS P_COUNTRY = 'USA';\n"
        );
        assert_eq!(param_value("'x'"), "'x'");
        assert_eq!(param_value("it's"), "'it''s'");
        assert_eq!(param_name("a b"), "\"a b\"");
    }
}
