//! Server jobs ([`dbine_driver::Session::jobs`] and the calls after it)
//! from Oracle's Scheduler, `DBMS_SCHEDULER`.
//!
//! The jobs come from `DBA_SCHEDULER_JOBS` when the login can read it, else
//! from `ALL_SCHEDULER_JOBS` (and the list says it's partial). Runs come
//! from `*_SCHEDULER_JOB_RUN_DETAILS`, with `*_SCHEDULER_JOB_LOG` for what
//! started them (`REASON="manual slave run"`); a chain's steps from
//! `*_SCHEDULER_CHAIN_STEPS` and `*_CHAIN_RULES`, and their runs from the
//! run details that carry the chain's `CHAIN_LOG_ID`; the run going on from
//! `*_SCHEDULER_RUNNING_JOBS`. Times are converted to UTC on the server
//! (`SYS_EXTRACT_UTC`), which also keeps time zone regions away from the
//! client (it panics on some).
//!
//! "Ejecutar ahora" is `RUN_JOB(use_current_session => FALSE)`: a
//! background run, so the call returns as soon as the Scheduler takes it
//! (disabled jobs run too). The script is `DBMS_METADATA.GET_DDL('PROCOBJ')`
//! when it answers (own jobs, or SELECT_CATALOG_ROLE), else one built from
//! the dictionary.
//!
//! A job can be managed by its owner, with ALTER on it, or with CREATE ANY
//! JOB, which doesn't reach SYS, system jobs or the other schemas Oracle
//! maintains (but SYSTEM). None offers to run while the Scheduler can't
//! start jobs (`SCHEDULER_DISABLED`, `job_queue_processes` 0).
//!
//! Legacy `DBMS_JOB` jobs are Scheduler jobs named `DBMS_JOB$_n` from 19c
//! on (listed with the rest, kind "DBMS_JOB"). Before 19c they live apart,
//! in `*_JOBS`, with ids `DBMS_JOB:<n>`; `DBMS_JOB` only manages the
//! caller's own, keeps no history and can't stop a run.
//!
//! Ids are `OWNER.NAME`, or `"Owner"."Name"` when either isn't a plain
//! upper-case identifier. One that comes back is parsed against that shape
//! and only ever bound: as two values in the dictionary queries, and as the
//! quoted `"OWNER"."NAME"` in the `DBMS_SCHEDULER` calls (which take a
//! comma-separated list, so a name with a comma is refused there).

use crate::monitor::txt;
use crate::processes::within;
use crate::{cell, db_code, err, quote};
use dbine_driver::jobs::{
    JobDetail, JobList, JobOutcome, JobRun, JobStep, JobStepRun, ServerJob, MAX_JOB_RUNS,
};
use dbine_driver::{Error, PropertyInfo, Result};
use oracledb::{Connection, ToDbValue};
use std::collections::{HashMap, HashSet};
use std::time::Duration;

const KIND: &str = "DBMS_SCHEDULER";
const LEGACY_KIND: &str = "DBMS_JOB";
const LEGACY_ID: &str = "DBMS_JOB:";
/// Longest a read may take.
const QUERY_LIMIT: Duration = Duration::from_secs(20);
/// Longest a call that changes a job may take: STOP_JOB waits for the run
/// to stop.
const CALL_LIMIT: Duration = Duration::from_secs(60);
/// Jobs listed at most.
const MAX_JOBS: usize = 5000;
/// Chain step runs read for one job's history at most.
const MAX_STEP_RUNS: usize = 5000;
/// Longest identifier, in bytes (12.2+; 30 before).
const MAX_NAME: usize = 128;

/// RFC 3339 in UTC of a `TIMESTAMP WITH TIME ZONE` column.
macro_rules! utc {
    ($col:literal) => {
        concat!(
            "TO_CHAR(SYS_EXTRACT_UTC(",
            $col,
            "), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
    };
}

/// Milliseconds of an `INTERVAL DAY TO SECOND` column.
macro_rules! millis {
    ($col:literal) => {
        concat!(
            "ROUND((EXTRACT(DAY FROM ",
            $col,
            ") * 86400 + EXTRACT(HOUR FROM ",
            $col,
            ") * 3600 + EXTRACT(MINUTE FROM ",
            $col,
            ") * 60 + EXTRACT(SECOND FROM ",
            $col,
            ")) * 1000)"
        )
    };
}

/// RFC 3339 in UTC of a `DATE` column of `DBMS_JOB`, which is in the
/// server's clock (`SYSDATE`): read with the offset the server has now.
macro_rules! utc_date {
    ($col:literal) => {
        concat!(
            "TO_CHAR(SYS_EXTRACT_UTC(FROM_TZ(CAST(",
            $col,
            " AS TIMESTAMP), TO_CHAR(SYSTIMESTAMP, 'TZH:TZM'))), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')"
        )
    };
}

/// Every job's columns, in [`JobRow::from_cells`]'s order. `{v}` is the
/// views' prefix (`dba` / `all`).
const JOBS: &str = concat!(
    "SELECT j.owner, j.job_name, j.job_type, j.job_action, j.program_owner, j.program_name, j.comments,
       j.enabled, j.state, j.schedule_type, j.repeat_interval, j.schedule_owner, j.schedule_name,
       s.repeat_interval, j.event_condition, j.event_queue_owner, j.event_queue_name, j.event_queue_agent, ",
    utc!("j.start_date"),
    ", ",
    utc!("j.last_start_date"),
    ", ",
    millis!("j.last_run_duration"),
    ", ",
    utc!("j.next_run_date"),
    ", ",
    utc!("j.end_date"),
    ",
       j.job_creator, j.job_class, j.system, j.job_style, j.auto_drop, j.restartable, j.job_priority,
       j.run_count, j.failure_count, j.retry_count, j.max_runs, j.max_failures, ",
    millis!("j.max_run_duration"),
    ",
       j.logging_level, j.stop_on_window_close, j.instance_id, j.destination, j.credential_owner,
       j.credential_name, j.number_of_arguments,
       TO_CHAR(j.start_date, 'YYYY-MM-DD HH24:MI:SS.FF TZR'), TO_CHAR(j.end_date, 'YYYY-MM-DD HH24:MI:SS.FF TZR'),
       p.program_type, p.program_action
  FROM {v}_scheduler_jobs j
  LEFT JOIN {v}_scheduler_schedules s ON s.owner = j.schedule_owner AND s.schedule_name = j.schedule_name
  LEFT JOIN {v}_scheduler_programs p ON p.owner = j.program_owner AND p.program_name = j.program_name
 WHERE j.job_subname IS NULL"
);

/// How the last finished run of each job ended.
const LAST_OUTCOMES: &str =
    "SELECT d.owner, d.job_name, MAX(d.status) KEEP (DENSE_RANK LAST ORDER BY d.log_id)
  FROM {v}_scheduler_job_run_details d
 WHERE d.job_subname IS NULL
 GROUP BY d.owner, d.job_name";

const LAST_OUTCOME: &str = "SELECT status FROM (
  SELECT d.status FROM {v}_scheduler_job_run_details d
   WHERE d.owner = :1 AND d.job_name = :2 AND d.job_subname IS NULL
   ORDER BY d.log_id DESC)
 WHERE ROWNUM = 1";

/// What a call needs to know of one job; no row: it doesn't exist or the
/// login doesn't see it.
const STATE: &str = concat!(
    "SELECT j.state, j.schedule_type, j.job_type, ",
    utc!("j.last_start_date"),
    "
  FROM {v}_scheduler_jobs j
 WHERE j.owner = :1 AND j.job_name = :2 AND j.job_subname IS NULL"
);

/// Finished runs, newest first. `d.errors, d.output` are 12c+.
const RUNS: &str = concat!(
    "SELECT * FROM (
SELECT d.log_id AS run_id, ",
    utc!("d.actual_start_date"),
    " AS started, ",
    millis!("d.run_duration"),
    " AS ms, d.status, d.error#, d.additional_info AS info, d.errors, d.output,
       l.log_id AS logged, l.additional_info AS log_info
  FROM {v}_scheduler_job_run_details d
  LEFT JOIN {v}_scheduler_job_log l ON l.log_id = d.log_id
 WHERE d.owner = :1 AND d.job_name = :2 AND d.job_subname IS NULL
 ORDER BY d.actual_start_date DESC NULLS LAST, d.log_id DESC)
 WHERE ROWNUM <= :3"
);

/// A chain job's step runs, newest first.
const STEP_RUNS: &str = concat!(
    "SELECT * FROM (
SELECT d.job_subname, d.status, ",
    utc!("d.actual_start_date"),
    " AS started, ",
    millis!("d.run_duration"),
    " AS ms, d.error#, d.additional_info, d.errors, d.output
  FROM {v}_scheduler_job_run_details d
 WHERE d.owner = :1 AND d.job_name = :2 AND d.job_subname IS NOT NULL
 ORDER BY d.log_id DESC)
 WHERE ROWNUM <= 5000"
);

/// The run going on (no sub-name) and its chain steps running now.
const RUNNING: &str = concat!(
    "SELECT r.job_subname, ",
    millis!("r.elapsed_time"),
    ", TO_CHAR(SYS_EXTRACT_UTC(SYSTIMESTAMP - r.elapsed_time), 'YYYY-MM-DD\"T\"HH24:MI:SS\"Z\"')
  FROM {v}_scheduler_running_jobs r
 WHERE r.owner = :1 AND r.job_name = :2"
);

/// The log id of the chain run going on: its steps carry it.
const CHAIN_START: &str = "SELECT MAX(l.log_id) FROM {v}_scheduler_job_log l
 WHERE l.owner = :1 AND l.job_name = :2 AND l.operation = 'CHAIN_START'";

const CHAIN_STEPS: &str = "SELECT s.step_name, s.step_type, s.program_owner, s.program_name, p.program_type,
       p.program_action, s.event_condition, s.destination
  FROM {v}_scheduler_chain_steps s
  LEFT JOIN {v}_scheduler_programs p ON p.owner = s.program_owner AND p.program_name = s.program_name
 WHERE s.owner = :1 AND s.chain_name = :2
 ORDER BY s.step_name";

const CHAIN_RULES: &str = "SELECT r.rule_name, r.condition, r.action
  FROM {v}_scheduler_chain_rules r
 WHERE r.owner = :1 AND r.chain_name = :2
 ORDER BY LENGTH(r.rule_name), r.rule_name";

const ARGS: &str = "SELECT a.argument_position, a.argument_name, a.argument_type, a.value
  FROM {v}_scheduler_job_args a
 WHERE a.owner = :1 AND a.job_name = :2
 ORDER BY a.argument_position";

const ACCESS: &str = "SELECT SYS_CONTEXT('USERENV', 'SESSION_USER'),
       (SELECT COUNT(*) FROM session_privs WHERE privilege = 'CREATE ANY JOB')
  FROM dual";

/// Objects the login was granted ALTER on, directly, through PUBLIC or an
/// enabled role: on a job it lets it run, stop, enable and disable it.
/// Not filtered by TYPE, which says UNKNOWN for a job; a job's name is
/// unique among its schema's objects.
const GRANTED: &str = "SELECT p.table_schema, p.table_name FROM all_tab_privs p
 WHERE p.privilege = 'ALTER'
   AND (p.grantee IN (USER, 'PUBLIC') OR p.grantee IN (SELECT role FROM session_roles))";

/// The schemas Oracle installs and maintains (12.1+).
const MAINTAINED: &str = "SELECT username FROM all_users WHERE oracle_maintained = 'Y'";

const SCHEDULER_DISABLED: &str = "SELECT value FROM {v}_scheduler_global_attribute
 WHERE attribute_name = 'SCHEDULER_DISABLED'";

const JOB_QUEUE_PROCESSES: &str =
    "SELECT value FROM v$parameter WHERE name = 'job_queue_processes'";

/// `DBMS_JOB` jobs before 19c, in [`LegacyRow::from_cells`]'s order.
const LEGACY_JOBS: &str = concat!(
    "SELECT j.job, j.log_user, j.schema_user, j.what, j.interval, j.broken, ",
    utc_date!("j.last_date"),
    ", ",
    utc_date!("j.this_date"),
    ", ",
    utc_date!("j.next_date"),
    ", j.failures, j.total_time, j.instance, TO_CHAR(j.next_date, 'YYYY-MM-DD HH24:MI:SS')
  FROM {v}_jobs j"
);

const RUN_JOB: &str =
    "BEGIN DBMS_SCHEDULER.RUN_JOB(job_name => :1, use_current_session => FALSE); END;";
const STOP_JOB: &str = "BEGIN DBMS_SCHEDULER.STOP_JOB(job_name => :1, force => FALSE); END;";
const ENABLE_JOB: &str = "BEGIN DBMS_SCHEDULER.ENABLE(name => :1); END;";
/// `force`: a job that is running is disabled too (the run goes on).
const DISABLE_JOB: &str = "BEGIN DBMS_SCHEDULER.DISABLE(name => :1, force => TRUE); END;";
const GET_DDL: &str = "SELECT DBMS_METADATA.GET_DDL('PROCOBJ', :1, :2) FROM dual";

/// "Ejecutar ahora" of a legacy job: due now, so a job queue process takes
/// it (`DBMS_JOB.RUN` would run it in this session, to the end).
const LEGACY_RUN: &str =
    "DECLARE j BINARY_INTEGER := :1; BEGIN DBMS_JOB.NEXT_DATE(j, SYSDATE); COMMIT; END;";
const LEGACY_DISABLE: &str =
    "DECLARE j BINARY_INTEGER := :1; BEGIN DBMS_JOB.BROKEN(j, TRUE); COMMIT; END;";
/// Fixed again, a broken job is due at once (its next date was set to
/// 4000-01-01 when it broke), as `DBMS_JOB.BROKEN` does by default; a next
/// date still ahead is kept.
const LEGACY_ENABLE: &str = "DECLARE j BINARY_INTEGER := :1; n DATE;
BEGIN
  SELECT next_date INTO n FROM user_jobs WHERE job = j;
  IF n IS NULL OR n < SYSDATE OR n >= DATE '4000-01-01' THEN n := SYSDATE; END IF;
  DBMS_JOB.BROKEN(j, FALSE, n);
  COMMIT;
END;";

const PARTIAL: &str =
    "Se ven solo los jobs a los que tiene acceso este usuario: para verlos todos \
     hace falta poder leer DBA_SCHEDULER_JOBS (SELECT_CATALOG_ROLE).";
const LEGACY_PARTIAL: &str = "De los jobs de DBMS_JOB se ven solo los de este usuario: para \
     verlos todos hace falta poder leer DBA_JOBS.";

type Cells = Vec<Option<String>>;
/// A chain rule: its name, condition and action.
type Rule = (String, String, String);

// ------------------------------------------------------------------ reading

/// Every row of `sql` (at most `max`), each column as text (the client
/// doesn't turn a NUMBER into a string itself). Out of the statement cache,
/// as the monitor's queries: a parse that failed for lack of a grant would
/// come back as ORA-01003.
fn texts(
    c: &Connection,
    sql: &str,
    params: &[&dyn ToDbValue],
    max: usize,
) -> std::result::Result<Vec<Cells>, oracledb::Error> {
    let cursor = c
        .statement(sql)?
        .exclude_from_cache()
        .build()?
        .query(params)?;
    let types: Vec<&'static oracledb::DbType> =
        cursor.columns().iter().map(|m| m.db_type()).collect();
    let mut out = Vec::new();
    for row in cursor {
        let row = row?;
        out.push(
            types
                .iter()
                .enumerate()
                .map(|(i, t)| txt(&cell(&row, i, t)))
                .collect(),
        );
        if out.len() >= max {
            break;
        }
    }
    Ok(out)
}

/// A column, `None` when null or blank.
fn col(r: &[Option<String>], i: usize) -> Option<String> {
    r.get(i).cloned().flatten().filter(|s| !s.trim().is_empty())
}

fn number(s: &Option<String>) -> Option<f64> {
    s.as_deref().and_then(|s| s.trim().parse::<f64>().ok())
}

fn millis(s: &Option<String>) -> Option<u64> {
    number(s).filter(|v| *v >= 0.0).map(|v| v.round() as u64)
}

/// An integer as the dictionary has it, for a script: digits only.
fn integer(s: &Option<String>) -> Option<i64> {
    s.as_deref().and_then(|s| s.trim().parse::<i64>().ok())
}

/// Without the grants on a view (ORA-00942 / 01031 / 01039).
fn denied(e: &oracledb::Error) -> bool {
    matches!(db_code(e), Some(942 | 1031 | 1039))
}

fn first_line(e: &oracledb::Error) -> String {
    e.to_string().lines().next().unwrap_or_default().to_string()
}

/// Which dictionary views the login reads.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum Views {
    Dba,
    All,
}

impl Views {
    /// `sql` with `{v}` replaced by the views' prefix.
    fn sql(self, sql: &str) -> String {
        sql.replace(
            "{v}",
            match self {
                Views::Dba => "dba",
                Views::All => "all",
            },
        )
    }
}

fn views(c: &Connection) -> Result<Views> {
    match texts(c, "SELECT 1 FROM dba_scheduler_jobs WHERE 1 = 0", &[], 1) {
        Ok(_) => Ok(Views::Dba),
        Err(e) if denied(&e) => Ok(Views::All),
        Err(e) => Err(err(e)),
    }
}

// ----------------------------------------------------------------------- ids

#[derive(Debug, Clone, PartialEq, Eq)]
enum JobId {
    Scheduler { owner: String, name: String },
    Legacy(i64),
}

/// An identifier Oracle takes unquoted, in the case it stores it.
fn plain(s: &str) -> bool {
    s.len() <= MAX_NAME
        && s.starts_with(|c: char| c.is_ascii_uppercase())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '$' | '#'))
}

/// An identifier that can be written quoted.
fn quotable(s: &str) -> bool {
    !s.is_empty() && s.len() <= MAX_NAME && !s.chars().any(|c| c == '"' || c.is_control())
}

fn job_id(owner: &str, name: &str) -> String {
    if plain(owner) && plain(name) {
        format!("{owner}.{name}")
    } else {
        format!("\"{owner}\".\"{name}\"")
    }
}

fn parse_id(id: &str) -> Result<JobId> {
    let bad = || {
        let shown: String = id.chars().take(200).collect();
        Error::Query(format!(
            "«{shown}» no es un id de job de Oracle (se espera «ESQUEMA.NOMBRE»)"
        ))
    };
    if let Some(n) = id.strip_prefix(LEGACY_ID) {
        if n.is_empty() || n.len() > 10 || !n.bytes().all(|b| b.is_ascii_digit()) {
            return Err(bad());
        }
        let n: i64 = n.parse().map_err(|_| bad())?;
        // Only the form the list gives (no leading zeros).
        if format!("{LEGACY_ID}{n}") != id {
            return Err(bad());
        }
        return Ok(JobId::Legacy(n));
    }
    let (owner, name) = match id.strip_prefix('"') {
        Some(rest) => {
            let (owner, rest) = rest.split_once("\".\"").ok_or_else(bad)?;
            let name = rest.strip_suffix('"').ok_or_else(bad)?;
            if !quotable(owner) || !quotable(name) {
                return Err(bad());
            }
            (owner, name)
        }
        None => {
            let (owner, name) = id.split_once('.').ok_or_else(bad)?;
            if !plain(owner) || !plain(name) {
                return Err(bad());
            }
            (owner, name)
        }
    };
    // Only the form the list gives: a pair of plain names is never quoted.
    if job_id(owner, name) != id {
        return Err(bad());
    }
    Ok(JobId::Scheduler {
        owner: owner.into(),
        name: name.into(),
    })
}

/// The DBMS_SCHEDULER calls take a list of names separated by commas.
fn callable(owner: &str, name: &str) -> bool {
    !owner.contains(',') && !name.contains(',')
}

/// The name a `DBMS_SCHEDULER` call is given (bound, never pasted).
fn scheduler_name(owner: &str, name: &str) -> Result<String> {
    if !callable(owner, name) {
        return Err(Error::Query(format!(
            "el nombre del job «{}» tiene una coma: DBMS_SCHEDULER no lo puede administrar por nombre",
            job_id(owner, name)
        )));
    }
    Ok(format!("{}.{}", quote(owner), quote(name)))
}

fn not_found(shown: &str) -> Error {
    Error::Query(format!(
        "no existe el job «{shown}» o este usuario no lo puede ver"
    ))
}

// -------------------------------------------------------------------- access

/// What the login may do to other users' jobs.
#[derive(Debug, Default)]
struct Access {
    user: String,
    /// CREATE ANY JOB: other users' jobs, but not Oracle's own (see
    /// [`Access::any_job_reaches`]).
    any_job: bool,
    /// ALTER granted on these jobs (owner, name).
    granted: HashSet<(String, String)>,
    /// The schemas Oracle installs and maintains (12.1+).
    maintained: HashSet<String>,
}

impl Access {
    fn read(c: &Connection) -> Access {
        let mut a = Access::default();
        match texts(c, ACCESS, &[], 1) {
            Ok(rows) => {
                if let Some(r) = rows.first() {
                    a.user = col(r, 0).unwrap_or_default();
                    a.any_job = number(&col(r, 1)).unwrap_or(0.0) > 0.0;
                }
            }
            Err(e) => tracing::debug!("oracle jobs: privileges: {e}"),
        }
        match texts(c, GRANTED, &[], 10_000) {
            Ok(rows) => {
                a.granted = rows
                    .iter()
                    .filter_map(|r| Some((col(r, 0)?, col(r, 1)?)))
                    .collect()
            }
            Err(e) => tracing::debug!("oracle jobs: grants: {e}"),
        }
        // Also without CREATE ANY JOB: it says which jobs are Oracle's own.
        match texts(c, MAINTAINED, &[], 10_000) {
            Ok(rows) => a.maintained = rows.iter().filter_map(|r| col(r, 0)).collect(),
            Err(e) => tracing::debug!("oracle jobs: Oracle's schemas: {e}"),
        }
        a
    }

    /// A job in this schema is Oracle's own: SYS, or a schema Oracle
    /// maintains (`ORACLE_MAINTAINED = 'Y'`, 12.1+), but for SYSTEM, where
    /// DBAs make their own jobs.
    fn oracle_schema(&self, owner: &str) -> bool {
        owner == "SYS" || (owner != "SYSTEM" && self.maintained.contains(owner))
    }

    /// RUN_JOB, STOP_JOB, ENABLE and DISABLE work on it: its owner, ALTER
    /// on it, or CREATE ANY JOB where it reaches.
    fn manages(&self, owner: &str, name: &str, system: bool) -> bool {
        owner == self.user
            || (self.any_job && self.any_job_reaches(owner, system))
            || self
                .granted
                .contains(&(owner.to_string(), name.to_string()))
    }

    /// CREATE ANY JOB doesn't reach SYS's jobs, and Oracle's own jobs (a
    /// system job, or one in a schema Oracle maintains) may be locked down
    /// (Autonomous Database refuses changes to them): left out, but for
    /// SYSTEM's, where DBAs make their own jobs.
    fn any_job_reaches(&self, owner: &str, system: bool) -> bool {
        !system && owner != "SYS" && (owner == "SYSTEM" || !self.maintained.contains(owner))
    }
}

// ---------------------------------------------------------------- the jobs

/// One row of [`JOBS`].
#[derive(Debug, Default, Clone)]
struct JobRow {
    owner: String,
    name: String,
    job_type: Option<String>,
    job_action: Option<String>,
    program_owner: Option<String>,
    program_name: Option<String>,
    comments: Option<String>,
    enabled: bool,
    state: Option<String>,
    schedule_type: Option<String>,
    repeat_interval: Option<String>,
    schedule_owner: Option<String>,
    schedule_name: Option<String>,
    named_repeat: Option<String>,
    event_condition: Option<String>,
    event_queue_owner: Option<String>,
    event_queue_name: Option<String>,
    event_queue_agent: Option<String>,
    start: Option<String>,
    last_start: Option<String>,
    last_duration: Option<String>,
    next_run: Option<String>,
    end: Option<String>,
    creator: Option<String>,
    class: Option<String>,
    system: Option<String>,
    style: Option<String>,
    auto_drop: Option<String>,
    restartable: Option<String>,
    priority: Option<String>,
    run_count: Option<String>,
    failure_count: Option<String>,
    retry_count: Option<String>,
    max_runs: Option<String>,
    max_failures: Option<String>,
    max_run_duration: Option<String>,
    logging_level: Option<String>,
    stop_on_window_close: Option<String>,
    instance_id: Option<String>,
    destination: Option<String>,
    credential_owner: Option<String>,
    credential_name: Option<String>,
    arguments: Option<String>,
    start_tz: Option<String>,
    end_tz: Option<String>,
    program_type: Option<String>,
    program_action: Option<String>,
}

impl JobRow {
    fn from_cells(r: &[Option<String>]) -> JobRow {
        let mut i = 0;
        let mut next = || {
            i += 1;
            col(r, i - 1)
        };
        JobRow {
            owner: next().unwrap_or_default(),
            name: next().unwrap_or_default(),
            job_type: next(),
            job_action: next(),
            program_owner: next(),
            program_name: next(),
            comments: next(),
            enabled: next().is_some_and(|v| v.trim() == "TRUE"),
            state: next(),
            schedule_type: next(),
            repeat_interval: next(),
            schedule_owner: next(),
            schedule_name: next(),
            named_repeat: next(),
            event_condition: next(),
            event_queue_owner: next(),
            event_queue_name: next(),
            event_queue_agent: next(),
            start: next(),
            last_start: next(),
            last_duration: next(),
            next_run: next(),
            end: next(),
            creator: next(),
            class: next(),
            system: next(),
            style: next(),
            auto_drop: next(),
            restartable: next(),
            priority: next(),
            run_count: next(),
            failure_count: next(),
            retry_count: next(),
            max_runs: next(),
            max_failures: next(),
            max_run_duration: next(),
            logging_level: next(),
            stop_on_window_close: next(),
            instance_id: next(),
            destination: next(),
            credential_owner: next(),
            credential_name: next(),
            arguments: next(),
            start_tz: next(),
            end_tz: next(),
            program_type: next(),
            program_action: next(),
        }
    }

    fn state(&self) -> &str {
        self.state.as_deref().map(str::trim).unwrap_or("")
    }

    fn running(&self) -> bool {
        running_state(self.state())
    }

    /// The schedule as the dictionary states it.
    fn schedule(&self) -> Option<String> {
        let named = qualified(&self.schedule_owner, &self.schedule_name);
        match self.schedule_type.as_deref().unwrap_or("") {
            "IMMEDIATE" => None,
            "ONCE" => self.start.as_ref().map(|s| format!("Una vez: {s}")),
            "EVENT" => Some(match (&self.event_condition, named) {
                (Some(c), _) => format!("Evento: {}", c.trim()),
                (None, Some(n)) => format!("Evento: {n}"),
                (None, None) => "Evento".into(),
            }),
            "WINDOW" | "WINDOW_ONCE" => named.map(|n| format!("Ventana {n}")),
            "WINDOW_GROUP" => named.map(|n| format!("Grupo de ventanas {n}")),
            _ => self
                .repeat_interval
                .clone()
                .or_else(|| self.named_repeat.clone())
                .or(named),
        }
        .map(|s| s.trim().to_string())
    }

    /// A job Oracle made (`SYSTEM = TRUE`).
    fn system(&self) -> bool {
        self.system.as_deref().map(str::trim) == Some("TRUE")
    }

    fn server_job(&self, access: &Access, last_status: Option<&str>) -> ServerJob {
        let running = self.running();
        let manages = access.manages(&self.owner, &self.name, self.system())
            && callable(&self.owner, &self.name);
        let last_outcome = match (self.state(), last_status) {
            ("RETRY SCHEDULED", _) => Some(JobOutcome::Retrying),
            (_, Some(s)) => Some(outcome(s)),
            ("BROKEN" | "FAILED", None) => Some(JobOutcome::Failed),
            ("SUCCEEDED", None) => Some(JobOutcome::Succeeded),
            _ if running => Some(JobOutcome::Running),
            _ => None,
        };
        ServerJob {
            id: job_id(&self.owner, &self.name),
            name: self.name.clone(),
            kind: if self.name.starts_with("DBMS_JOB$_") {
                LEGACY_KIND
            } else {
                KIND
            }
            .into(),
            group: Some(self.owner.clone()),
            description: self.comments.as_deref().map(|s| s.trim().to_string()),
            owner: Some(self.owner.clone()),
            enabled: self.enabled,
            schedule: self.schedule(),
            running,
            last_outcome,
            last_started: self.last_start.clone(),
            last_duration_ms: millis(&self.last_duration),
            next_run: self.next_run.clone().filter(|_| self.enabled),
            can_run: manages && !running,
            can_stop: manages && running,
            can_enable: manages,
            has_history: true,
            has_script: true,
            system: self.system() || access.oracle_schema(&self.owner),
        }
    }

    /// The chain a chain job runs (`job_action`), as (owner, name).
    fn chain(&self) -> Option<(String, String)> {
        if self.job_type.as_deref() != Some("CHAIN") {
            return None;
        }
        object_name(self.job_action.as_deref()?, &self.owner)
    }

    /// What a job that isn't a chain runs: its action, or its program's.
    fn own_step(&self) -> Option<JobStep> {
        let (kind, command, name) = match &self.job_type {
            Some(t) => (Some(t.as_str()), self.job_action.clone(), self.name.clone()),
            None => (
                self.program_type.as_deref(),
                self.program_action.clone(),
                qualified(&self.program_owner, &self.program_name)?,
            ),
        };
        Some(JobStep {
            name,
            kind: kind.map(step_kind),
            target: self.destination.clone(),
            command: command.map(|s| s.trim().to_string()),
        })
    }

    fn properties(&self) -> Vec<PropertyInfo> {
        const SCHEDULE: &str = "Programación";
        const RUNS: &str = "Ejecución";
        let queue = qualified(&self.event_queue_owner, &self.event_queue_name).map(|q| match &self
            .event_queue_agent
        {
            Some(a) => format!("{q} ({a})"),
            None => q,
        });
        [
            ("", "Tipo", self.job_type.clone()),
            (
                "",
                "Programa",
                qualified(&self.program_owner, &self.program_name),
            ),
            ("", "Clase", self.class.clone()),
            ("", "Estilo", self.style.clone()),
            ("", "Estado", self.state.clone()),
            ("", "Creado por", self.creator.clone()),
            ("", "Del sistema", yes_no(&self.system)),
            ("", "Se borra al terminar", yes_no(&self.auto_drop)),
            ("", "Destino", self.destination.clone()),
            (
                "",
                "Credencial",
                qualified(&self.credential_owner, &self.credential_name),
            ),
            (SCHEDULE, "Tipo de programación", self.schedule_type.clone()),
            (
                SCHEDULE,
                "Programación",
                qualified(&self.schedule_owner, &self.schedule_name),
            ),
            (
                SCHEDULE,
                "Intervalo",
                self.repeat_interval
                    .clone()
                    .or_else(|| self.named_repeat.clone()),
            ),
            (SCHEDULE, "Inicio", self.start.clone()),
            (SCHEDULE, "Fin", self.end.clone()),
            (
                SCHEDULE,
                "Condición del evento",
                self.event_condition.clone(),
            ),
            (SCHEDULE, "Cola del evento", queue),
            (
                SCHEDULE,
                "Detener al cerrar la ventana",
                yes_no(&self.stop_on_window_close),
            ),
            (RUNS, "Ejecuciones", self.run_count.clone()),
            (RUNS, "Fallas", self.failure_count.clone()),
            (RUNS, "Reintentos", self.retry_count.clone()),
            (RUNS, "Máximo de ejecuciones", self.max_runs.clone()),
            (RUNS, "Máximo de fallas", self.max_failures.clone()),
            (
                RUNS,
                "Duración máxima",
                millis(&self.max_run_duration).map(duration_text),
            ),
            (RUNS, "Prioridad", self.priority.clone()),
            (RUNS, "Reiniciable", yes_no(&self.restartable)),
            (RUNS, "Nivel de registro", self.logging_level.clone()),
            (RUNS, "Instancia", self.instance_id.clone()),
        ]
        .into_iter()
        .filter_map(|(group, label, value)| {
            Some(PropertyInfo {
                group: group.into(),
                label: label.into(),
                value: value?.trim().to_string(),
            })
        })
        .collect()
    }
}

fn running_state(state: &str) -> bool {
    matches!(state, "RUNNING" | "CHAIN_STALLED")
}

/// `OWNER.NAME`, or `NAME` without an owner.
fn qualified(owner: &Option<String>, name: &Option<String>) -> Option<String> {
    let name = name.as_deref()?.trim();
    Some(match owner.as_deref().map(str::trim) {
        Some(o) if !o.is_empty() => format!("{o}.{name}"),
        _ => name.to_string(),
    })
}

fn yes_no(v: &Option<String>) -> Option<String> {
    match v.as_deref().map(str::trim) {
        Some("TRUE") => Some("Sí".into()),
        Some("FALSE") => Some("No".into()),
        _ => None,
    }
}

/// `[N d ]HH:MM:SS`.
fn duration_text(ms: u64) -> String {
    let s = ms / 1000;
    let (d, h, m, s) = (s / 86400, s / 3600 % 24, s / 60 % 60, s % 60);
    if d > 0 {
        format!("{d} d {h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// What runs a step, from a job, program or chain step type.
fn step_kind(t: &str) -> String {
    match t.trim() {
        "PLSQL_BLOCK" => "PL/SQL",
        "STORED_PROCEDURE" => "Procedimiento",
        "EXECUTABLE" => "Ejecutable",
        "EXTERNAL_SCRIPT" => "Script externo",
        "SQL_SCRIPT" => "Script de SQL*Plus",
        "BACKUP_SCRIPT" => "Script de RMAN",
        "CHAIN" | "SUBCHAIN" => "Cadena",
        "EVENT_SCHEDULE" | "INLINE_EVENT" => "Evento",
        other => other,
    }
    .into()
}

/// How a run ended, from `*_SCHEDULER_JOB_RUN_DETAILS.STATUS`.
fn outcome(status: &str) -> JobOutcome {
    match status.trim() {
        "SUCCEEDED" => JobOutcome::Succeeded,
        "FAILED" | "BROKEN" => JobOutcome::Failed,
        "STOPPED" | "KILLED" => JobOutcome::Canceled,
        "RETRY SCHEDULED" => JobOutcome::Retrying,
        "RUNNING" => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

/// A schema object name as a job's action names a chain (`CHAIN`,
/// `OWNER.CHAIN`, `"Owner"."Chain"`), as (owner, name): unquoted parts in
/// upper case, as Oracle stores them.
fn object_name(text: &str, default_owner: &str) -> Option<(String, String)> {
    let mut parts = Vec::new();
    let mut rest = text.trim();
    loop {
        let (part, after) = if let Some(q) = rest.strip_prefix('"') {
            let end = q.find('"')?;
            (q[..end].to_string(), &q[end + 1..])
        } else {
            let end = rest.find('.').unwrap_or(rest.len());
            (rest[..end].trim().to_ascii_uppercase(), &rest[end..])
        };
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        match after.trim_start().strip_prefix('.') {
            Some(next) => rest = next.trim_start(),
            None if after.trim().is_empty() => break,
            None => return None,
        }
    }
    match parts.as_slice() {
        [name] => Some((default_owner.to_string(), name.clone())),
        [owner, name] => Some((owner.clone(), name.clone())),
        _ => None,
    }
}

/// The identifiers in a chain rule's condition or action.
fn words(text: &str) -> impl Iterator<Item = &str> {
    text.split(|c: char| !(c.is_alphanumeric() || matches!(c, '_' | '$' | '#')))
        .filter(|w| !w.is_empty())
}

/// The chain's steps in the order its rules start them: a rule's steps
/// come after the ones its condition waits for. Steps no rule starts go
/// last.
fn step_order(steps: &[String], rules: &[Rule]) -> Vec<String> {
    let find = |w: &str| steps.iter().find(|s| s.eq_ignore_ascii_case(w)).cloned();
    let mut placed: Vec<String> = Vec::new();
    let mut progress = true;
    while progress {
        progress = false;
        for (_, condition, action) in rules {
            let mut act = words(action);
            if !act.next().is_some_and(|w| w.eq_ignore_ascii_case("START")) {
                continue;
            }
            let ready = words(condition)
                .filter_map(find)
                .all(|s| placed.contains(&s));
            if !ready {
                continue;
            }
            for s in act.filter_map(find) {
                if !placed.contains(&s) {
                    placed.push(s);
                    progress = true;
                }
            }
        }
    }
    for s in steps {
        if !placed.contains(s) {
            placed.push(s.clone());
        }
    }
    placed
}

// ------------------------------------------------------------------ the list

pub fn list(c: &Connection) -> Result<JobList> {
    within(c, QUERY_LIMIT, || {
        let v = views(c)?;
        let access = Access::read(c);
        let off = Off::read(c, v);
        let mut jobs = Vec::new();
        let mut notes = Vec::new();
        let sql = v.sql(&format!("{JOBS} ORDER BY j.owner, j.job_name"));
        match texts(c, &sql, &[], MAX_JOBS) {
            Ok(rows) => {
                let outcomes = last_outcomes(c, v);
                if rows.len() >= MAX_JOBS {
                    notes.push(format!("Se muestran los primeros {MAX_JOBS} jobs."));
                }
                jobs.extend(rows.iter().map(|r| {
                    let row = JobRow::from_cells(r);
                    let last = outcomes.get(&(row.owner.clone(), row.name.clone()));
                    off.scheduler_job(row.server_job(&access, last.map(String::as_str)))
                }));
            }
            Err(e) if denied(&e) => notes.push(format!(
                "Este usuario no puede leer los jobs del Scheduler: {}",
                first_line(&e)
            )),
            Err(e) => return Err(err(e)),
        }
        if v == Views::All {
            notes.push(PARTIAL.into());
        }
        if legacy_apart(c) {
            match legacy_rows(c, v, "ORDER BY j.job", &[], MAX_JOBS) {
                Ok((rows, partial)) => {
                    jobs.extend(
                        rows.iter()
                            .map(|r| off.legacy_job(LegacyRow::from_cells(r).server_job(&access))),
                    );
                    if partial {
                        notes.push(LEGACY_PARTIAL.into());
                    }
                }
                Err(e) => notes.push(format!(
                    "Este usuario no puede leer los jobs de DBMS_JOB: {}",
                    first_line(&e)
                )),
            }
        }
        notes.extend(off.notes());
        Ok(JobList { jobs, notes })
    })
}

/// (owner, job) → status of its last finished run. Empty when the run
/// details can't be read.
fn last_outcomes(c: &Connection, v: Views) -> HashMap<(String, String), String> {
    match texts(c, &v.sql(LAST_OUTCOMES), &[], usize::MAX) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| Some(((col(r, 0)?, col(r, 1)?), col(r, 2)?)))
            .collect(),
        Err(e) => {
            tracing::debug!("oracle jobs: last runs: {e}");
            HashMap::new()
        }
    }
}

/// What keeps jobs from running on this server. Unknown (the login can't
/// read it) counts as running.
#[derive(Debug, Default, Clone, Copy)]
struct Off {
    /// `SCHEDULER_DISABLED` is TRUE: no Scheduler job starts.
    disabled: bool,
    /// `job_queue_processes` is 0: no job starts, `DBMS_JOB`'s either.
    no_queue: bool,
}

impl Off {
    fn read(c: &Connection, v: Views) -> Off {
        let first = |sql: &str| {
            texts(c, sql, &[], 1)
                .ok()
                .and_then(|r| r.first().and_then(|r| col(r, 0)))
        };
        Off {
            disabled: first(&v.sql(SCHEDULER_DISABLED))
                .is_some_and(|s| s.trim().eq_ignore_ascii_case("TRUE")),
            no_queue: first(JOB_QUEUE_PROCESSES).is_some_and(|s| s.trim() == "0"),
        }
    }

    /// A Scheduler job: RUN_JOB in the background fails (ORA-27492).
    fn scheduler_job(self, mut job: ServerJob) -> ServerJob {
        if self.disabled || self.no_queue {
            job.can_run = false;
        }
        job
    }

    /// A `DBMS_JOB` job before 19c: its next date is taken, but nothing
    /// runs it.
    fn legacy_job(self, mut job: ServerJob) -> ServerJob {
        if self.no_queue {
            job.can_run = false;
        }
        job
    }

    fn notes(self) -> Vec<String> {
        let mut notes = Vec::new();
        if self.disabled {
            notes.push(
                "El Scheduler está deshabilitado (SCHEDULER_DISABLED): no ejecuta ningún job, ni \
                 programado ni con «Ejecutar ahora»."
                    .into(),
            );
        }
        if self.no_queue {
            notes.push(
                "job_queue_processes es 0: el servidor no ejecuta jobs, ni programados ni con \
                 «Ejecutar ahora»."
                    .into(),
            );
        }
        notes
    }
}

/// Before 19c, `DBMS_JOB` jobs aren't Scheduler jobs.
fn legacy_apart(c: &Connection) -> bool {
    c.version().map(|v| v.0 < 19).unwrap_or(false)
}

/// One job's row of [`JOBS`].
fn job_row(c: &Connection, v: Views, owner: &str, name: &str) -> Result<JobRow> {
    let sql = v.sql(&format!("{JOBS} AND j.owner = :1 AND j.job_name = :2"));
    let rows = texts(c, &sql, &[&owner, &name], 1).map_err(err)?;
    rows.first()
        .map(|r| JobRow::from_cells(r))
        .ok_or_else(|| not_found(&job_id(owner, name)))
}

// ---------------------------------------------------------------- the detail

pub fn detail(c: &Connection, id: &str) -> Result<JobDetail> {
    match parse_id(id)? {
        JobId::Legacy(n) => within(c, QUERY_LIMIT, || {
            let v = views(c)?;
            let row = legacy_row(c, v, n)?;
            let mut detail = row.detail(&Access::read(c));
            detail.job = Off::read(c, v).legacy_job(detail.job);
            Ok(detail)
        }),
        JobId::Scheduler { owner, name } => within(c, QUERY_LIMIT, || {
            let v = views(c)?;
            let row = job_row(c, v, &owner, &name)?;
            let access = Access::read(c);
            let off = Off::read(c, v);
            let last = texts(c, &v.sql(LAST_OUTCOME), &[&owner, &name], 1)
                .map_err(|e| tracing::debug!("oracle jobs: last run: {e}"))
                .ok()
                .and_then(|r| r.first().and_then(|r| col(r, 0)));
            let mut properties = row.properties();
            let steps = match row.chain() {
                Some((chain_owner, chain)) => {
                    let (steps, rules) = chain_steps(c, v, &chain_owner, &chain);
                    properties.extend(rules.into_iter().map(|(rule, condition, action)| {
                        PropertyInfo {
                            group: "Reglas de la cadena".into(),
                            label: rule,
                            value: format!("{} → {}", condition.trim(), action.trim()),
                        }
                    }));
                    steps
                }
                None => row.own_step().into_iter().collect(),
            };
            properties.extend(job_args(c, v, &owner, &name).into_iter().map(|a| {
                PropertyInfo {
                    group: "Argumentos".into(),
                    label: a
                        .name
                        .clone()
                        .unwrap_or_else(|| format!("Argumento {}", a.position)),
                    value: a.value.clone().unwrap_or_else(|| {
                        a.kind.clone().map(|k| format!("({k})")).unwrap_or_default()
                    }),
                }
            }));
            Ok(JobDetail {
                job: off.scheduler_job(row.server_job(&access, last.as_deref())),
                steps,
                schedules: Vec::new(),
                properties,
                notes: Vec::new(),
            })
        }),
    }
}

/// A chain's steps in order, and its rules (name, condition, action).
fn chain_steps(c: &Connection, v: Views, owner: &str, chain: &str) -> (Vec<JobStep>, Vec<Rule>) {
    let rules: Vec<Rule> = match texts(c, &v.sql(CHAIN_RULES), &[&owner, &chain], 1000) {
        Ok(rows) => rows
            .iter()
            .filter_map(|r| Some((col(r, 0)?, col(r, 1)?, col(r, 2)?)))
            .collect(),
        Err(e) => {
            tracing::debug!("oracle jobs: chain rules: {e}");
            Vec::new()
        }
    };
    let rows = match texts(c, &v.sql(CHAIN_STEPS), &[&owner, &chain], 1000) {
        Ok(rows) => rows,
        Err(e) => {
            tracing::debug!("oracle jobs: chain steps: {e}");
            return (Vec::new(), rules);
        }
    };
    let mut steps: HashMap<String, JobStep> = rows
        .iter()
        .filter_map(|r| {
            let name = col(r, 0)?;
            let step_type = col(r, 1).unwrap_or_default();
            let program = qualified(&col(r, 2), &col(r, 3));
            let (kind, command) = match step_type.trim() {
                "SUBCHAIN" => (step_kind("SUBCHAIN"), program),
                "EVENT_SCHEDULE" | "INLINE_EVENT" => (step_kind(&step_type), col(r, 6)),
                _ => (
                    col(r, 4).map(|t| step_kind(&t)).unwrap_or_default(),
                    col(r, 5).map(|s| s.trim().to_string()).or(program),
                ),
            };
            Some((
                name.clone(),
                JobStep {
                    name,
                    kind: Some(kind).filter(|k| !k.is_empty()),
                    target: col(r, 7),
                    command,
                },
            ))
        })
        .collect();
    let names: Vec<String> = rows.iter().filter_map(|r| col(r, 0)).collect();
    let ordered = step_order(&names, &rules)
        .into_iter()
        .filter_map(|n| steps.remove(&n))
        .collect();
    (ordered, rules)
}

struct JobArg {
    position: String,
    name: Option<String>,
    kind: Option<String>,
    value: Option<String>,
}

fn job_args(c: &Connection, v: Views, owner: &str, name: &str) -> Vec<JobArg> {
    match texts(c, &v.sql(ARGS), &[&owner, &name], 1000) {
        Ok(rows) => rows
            .iter()
            .map(|r| JobArg {
                position: col(r, 0).unwrap_or_default(),
                name: col(r, 1),
                kind: col(r, 2),
                value: r.get(3).cloned().flatten(),
            })
            .collect(),
        Err(e) => {
            tracing::debug!("oracle jobs: arguments: {e}");
            Vec::new()
        }
    }
}

// ------------------------------------------------------------------ the runs

pub fn runs(c: &Connection, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    let (owner, name) = match parse_id(id)? {
        JobId::Legacy(_) => {
            return Err(Error::Unsupported(
                "los jobs de DBMS_JOB no guardan el historial de sus ejecuciones".into(),
            ))
        }
        JobId::Scheduler { owner, name } => (owner, name),
    };
    let max = limit.min(MAX_JOB_RUNS) as usize;
    within(c, QUERY_LIMIT, || {
        let v = views(c)?;
        let state = texts(c, &v.sql(STATE), &[&owner, &name], 1)
            .map_err(err)?
            .into_iter()
            .next()
            .ok_or_else(|| not_found(&job_id(&owner, &name)))?;
        let mut out = Vec::new();
        if max == 0 {
            return Ok(out);
        }
        let schedule_type = col(&state, 1);
        let chain = col(&state, 2).as_deref() == Some("CHAIN");
        let mut step_runs = if chain {
            step_runs(c, v, &owner, &name)
        } else {
            HashMap::new()
        };
        if running_state(col(&state, 0).as_deref().unwrap_or("").trim()) {
            out.push(current_run(
                c,
                v,
                &owner,
                &name,
                col(&state, 3),
                chain,
                &mut step_runs,
            ));
        }
        let rows = finished_runs(c, v, &owner, &name, max - out.len())?;
        out.extend(rows.iter().map(|r| {
            let mut run = finished_run(r, schedule_type.as_deref());
            if let Some(steps) =
                chain_log_id(col(r, 5).as_deref()).and_then(|id| step_runs.remove(&id))
            {
                run.steps = steps;
            }
            run
        }));
        Ok(out)
    })
}

fn finished_runs(
    c: &Connection,
    v: Views,
    owner: &str,
    name: &str,
    max: usize,
) -> Result<Vec<Cells>> {
    if max == 0 {
        return Ok(Vec::new());
    }
    let n = max as i64;
    match texts(c, &v.sql(RUNS), &[&owner, &name, &n], max) {
        // Before 12c the run details have no ERRORS / OUTPUT.
        Err(e) if db_code(&e) == Some(904) => texts(
            c,
            &v.sql(&RUNS.replace("d.errors, d.output", "NULL AS errors, NULL AS output")),
            &[&owner, &name, &n],
            max,
        )
        .map_err(err),
        r => r.map_err(err),
    }
}

/// A row of [`RUNS`] as a run.
fn finished_run(r: &[Option<String>], schedule_type: Option<&str>) -> JobRun {
    JobRun {
        id: col(r, 0),
        started: col(r, 1),
        duration_ms: millis(&col(r, 2)),
        outcome: col(r, 3).map(|s| outcome(&s)).unwrap_or_default(),
        trigger: trigger(col(r, 8).is_some(), col(r, 9).as_deref(), schedule_type),
        message: run_message(
            integer(&col(r, 4)),
            col(r, 5).as_deref(),
            col(r, 6).as_deref(),
            col(r, 7).as_deref(),
        ),
        steps: Vec::new(),
    }
}

/// A chain job's step runs by the chain run they belong to, each in the
/// order they started.
fn step_runs(
    c: &Connection,
    v: Views,
    owner: &str,
    name: &str,
) -> HashMap<String, Vec<JobStepRun>> {
    let rows = match texts(c, &v.sql(STEP_RUNS), &[&owner, &name], MAX_STEP_RUNS) {
        Ok(rows) => rows,
        Err(e) if db_code(&e) == Some(904) => texts(
            c,
            &v.sql(&STEP_RUNS.replace("d.errors, d.output", "NULL AS errors, NULL AS output")),
            &[&owner, &name],
            MAX_STEP_RUNS,
        )
        .unwrap_or_default(),
        Err(e) => {
            tracing::debug!("oracle jobs: step runs: {e}");
            Vec::new()
        }
    };
    group_steps(&rows)
}

fn group_steps(rows: &[Cells]) -> HashMap<String, Vec<JobStepRun>> {
    let mut by_chain: HashMap<String, Vec<JobStepRun>> = HashMap::new();
    // Newest first: reversed, each chain run's steps go in the order they ran.
    for r in rows.iter().rev() {
        let Some(chain) = chain_log_id(col(r, 5).as_deref()) else {
            continue;
        };
        by_chain.entry(chain).or_default().push(JobStepRun {
            name: col(r, 0).unwrap_or_default(),
            outcome: col(r, 1).map(|s| outcome(&s)).unwrap_or_default(),
            started: col(r, 2),
            duration_ms: millis(&col(r, 3)),
            message: run_message(
                integer(&col(r, 4)),
                col(r, 5).as_deref(),
                col(r, 6).as_deref(),
                col(r, 7).as_deref(),
            ),
        });
    }
    by_chain
}

/// The run going on: from its start, with the chain steps done so far and
/// the ones running.
fn current_run(
    c: &Connection,
    v: Views,
    owner: &str,
    name: &str,
    started: Option<String>,
    chain: bool,
    step_runs: &mut HashMap<String, Vec<JobStepRun>>,
) -> JobRun {
    let running = texts(c, &v.sql(RUNNING), &[&owner, &name], 1000).unwrap_or_else(|e| {
        tracing::debug!("oracle jobs: running: {e}");
        Vec::new()
    });
    let mut run = JobRun {
        started,
        outcome: JobOutcome::Running,
        ..Default::default()
    };
    if let Some(r) = running.iter().find(|r| col(r, 0).is_none()) {
        run.duration_ms = millis(&col(r, 1));
    }
    if chain {
        let start = texts(c, &v.sql(CHAIN_START), &[&owner, &name], 1)
            .ok()
            .and_then(|r| r.first().and_then(|r| col(r, 0)));
        if let Some(steps) = start.and_then(|id| step_runs.remove(&id)) {
            run.steps = steps;
        }
        run.steps.extend(running.iter().filter_map(|r| {
            Some(JobStepRun {
                name: col(r, 0)?,
                outcome: JobOutcome::Running,
                started: col(r, 2),
                duration_ms: millis(&col(r, 1)),
                message: None,
            })
        }));
    }
    run
}

/// `KEY="value"` pairs at the start of an `ADDITIONAL_INFO`, and the text
/// after them.
fn info_parts(info: &str) -> (Vec<(String, String)>, String) {
    let mut pairs = Vec::new();
    let mut rest = info.trim_start();
    while let Some(eq) = rest.find("=\"") {
        let key = &rest[..eq];
        if key.is_empty()
            || !key
                .chars()
                .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || matches!(c, '_' | '#'))
        {
            break;
        }
        let value = &rest[eq + 2..];
        let Some(end) = value.find('"') else {
            break;
        };
        pairs.push((key.to_string(), value[..end].to_string()));
        rest = value[end + 1..].trim_start_matches([',', ' ', '\n']);
    }
    (pairs, rest.trim().to_string())
}

fn chain_log_id(info: Option<&str>) -> Option<String> {
    info_parts(info?)
        .0
        .into_iter()
        .find(|(k, _)| k == "CHAIN_LOG_ID")
        .map(|(_, v)| v)
}

/// `REASON="manual slave run"` (RUN_JOB in the background) or
/// `"manually run"` (in the caller's session).
fn manual(reason: &str) -> bool {
    reason.to_ascii_lowercase().contains("manual")
}

/// What started a run, from its entry in the job log.
fn trigger(logged: bool, info: Option<&str>, schedule_type: Option<&str>) -> Option<String> {
    if !logged {
        return None;
    }
    let (pairs, _) = info_parts(info.unwrap_or(""));
    let by_hand = pairs.iter().any(|(k, v)| k == "REASON" && manual(v));
    Some(
        match (by_hand, schedule_type) {
            (true, _) => "manual",
            (false, Some("EVENT")) => "evento",
            (false, _) => "programada",
        }
        .into(),
    )
}

/// A run's message: why it stopped, its error (or `ORA-n`) and its output.
fn run_message(
    error: Option<i64>,
    info: Option<&str>,
    errors: Option<&str>,
    output: Option<&str>,
) -> Option<String> {
    let (pairs, rest) = info.map(info_parts).unwrap_or_default();
    let mut parts: Vec<String> = pairs
        .into_iter()
        .filter(|(k, v)| k == "REASON" && !manual(v))
        .map(|(_, v)| v)
        .collect();
    match errors.map(str::trim).filter(|s| !s.is_empty()) {
        Some(e) => parts.push(e.to_string()),
        None if !rest.is_empty() => parts.push(rest),
        None => {
            if let Some(n) = error.filter(|n| *n != 0) {
                parts.push(format!("ORA-{n:05}"));
            }
        }
    }
    if let Some(o) = output.map(str::trim).filter(|s| !s.is_empty()) {
        parts.push(o.to_string());
    }
    (!parts.is_empty()).then(|| parts.join("\n"))
}

// --------------------------------------------------------------- the actions

fn call_err(e: oracledb::Error, shown: &str) -> Error {
    match db_code(&e) {
        Some(27478) => Error::Query(format!("el job «{shown}» ya se está ejecutando")),
        Some(27492) => Error::Query(format!(
            "el Scheduler no está disponible en este servidor (deshabilitado o \
             job_queue_processes = 0): no puede ejecutar el job «{shown}»"
        )),
        Some(27366) => Error::Query(format!("el job «{shown}» no se está ejecutando")),
        Some(27475 | 27476) => Error::Query(format!(
            "el job «{shown}» no existe o este usuario no lo puede administrar"
        )),
        Some(27486 | 1031) => Error::Query(format!(
            "este usuario no puede administrar el job «{shown}»: hace falta ser su dueño, tener \
             ALTER sobre él o el privilegio CREATE ANY JOB"
        )),
        _ => err(e),
    }
}

/// A DBMS_SCHEDULER call on a job the login sees.
fn scheduler_call(c: &Connection, owner: &str, name: &str, sql: &str) -> Result<()> {
    let target = scheduler_name(owner, name)?;
    let shown = job_id(owner, name);
    within(c, CALL_LIMIT, || {
        let v = views(c)?;
        if texts(c, &v.sql(STATE), &[&owner, &name], 1)
            .map_err(err)?
            .is_empty()
        {
            return Err(not_found(&shown));
        }
        c.execute(sql, &[&target])
            .map(|_| ())
            .map_err(|e| call_err(e, &shown))
    })
}

fn legacy_call(c: &Connection, job: i64, sql: &str) -> Result<()> {
    within(c, CALL_LIMIT, || {
        c.execute(sql, &[&job])
            .map(|_| ())
            .map_err(|e| match db_code(&e) {
                Some(23421 | 1403) => Error::Query(format!(
                    "el job {job} de DBMS_JOB no existe o no es de este usuario: DBMS_JOB solo \
                 administra los jobs propios"
                )),
                _ => err(e),
            })
    })
}

pub fn run(c: &Connection, id: &str) -> Result<()> {
    match parse_id(id)? {
        JobId::Legacy(n) => legacy_call(c, n, LEGACY_RUN),
        JobId::Scheduler { owner, name } => scheduler_call(c, &owner, &name, RUN_JOB),
    }
}

pub fn stop(c: &Connection, id: &str) -> Result<()> {
    match parse_id(id)? {
        JobId::Legacy(_) => Err(Error::Unsupported(
            "un job de DBMS_JOB no se puede detener: hay que terminar la sesión que lo ejecuta"
                .into(),
        )),
        JobId::Scheduler { owner, name } => scheduler_call(c, &owner, &name, STOP_JOB),
    }
}

pub fn set_enabled(c: &Connection, id: &str, enabled: bool) -> Result<()> {
    match (parse_id(id)?, enabled) {
        (JobId::Legacy(n), true) => legacy_call(c, n, LEGACY_ENABLE),
        (JobId::Legacy(n), false) => legacy_call(c, n, LEGACY_DISABLE),
        (JobId::Scheduler { owner, name }, true) => scheduler_call(c, &owner, &name, ENABLE_JOB),
        (JobId::Scheduler { owner, name }, false) => scheduler_call(c, &owner, &name, DISABLE_JOB),
    }
}

// ---------------------------------------------------------------- the script

pub fn script(c: &Connection, id: &str) -> Result<String> {
    match parse_id(id)? {
        JobId::Legacy(n) => within(c, QUERY_LIMIT, || Ok(legacy_row(c, views(c)?, n)?.script())),
        JobId::Scheduler { owner, name } => within(c, QUERY_LIMIT, || {
            let v = views(c)?;
            let row = job_row(c, v, &owner, &name)?;
            let ddl = c
                .query_row(GET_DDL, &[&name, &owner])
                .and_then(|r| r.get::<Option<String>>(0));
            match ddl {
                Ok(Some(ddl)) if !ddl.trim().is_empty() => Ok(qualify(&ddl, &owner, &name)),
                other => {
                    if let Err(e) = other {
                        tracing::debug!("oracle jobs: GET_DDL of {owner}.{name}: {e}");
                    }
                    Ok(build_script(&row, &job_args(c, v, &owner, &name)))
                }
            }
        }),
    }
}

/// `'text'`, doubling embedded quotes.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `GET_DDL('PROCOBJ')` names the job without its schema: qualified, the
/// script recreates it where it was whoever runs it. Ends in `/`.
fn qualify(ddl: &str, owner: &str, name: &str) -> String {
    let bare = format!("('\"{}\"'", name.replace('\'', "''"));
    let full = format!(
        "('\"{}\".\"{}\"'",
        owner.replace('\'', "''"),
        name.replace('\'', "''")
    );
    let mut out = ddl.trim().replace(&bare, &full);
    if !out.ends_with('/') {
        out.push_str("\n/");
    }
    out.push('\n');
    out
}

/// `"OWNER"."NAME"` (or `"NAME"`) as a string literal.
fn name_lit(owner: &Option<String>, name: &str) -> String {
    match owner.as_deref().map(str::trim).filter(|o| !o.is_empty()) {
        Some(o) => lit(&format!("{}.{}", quote(o), quote(name.trim()))),
        None => lit(&quote(name.trim())),
    }
}

fn timestamp_lit(tz: &Option<String>) -> String {
    match tz {
        Some(t) => format!(
            "TO_TIMESTAMP_TZ({}, 'YYYY-MM-DD HH24:MI:SS.FF TZR')",
            lit(t.trim())
        ),
        None => "NULL".into(),
    }
}

/// The job's script from the dictionary, when `GET_DDL` doesn't give it
/// (another user's job without SELECT_CATALOG_ROLE, Oracle's own jobs).
fn build_script(row: &JobRow, args: &[JobArg]) -> String {
    let job = name_lit(&Some(row.owner.clone()), &row.name);
    let opt = |v: &Option<String>| v.as_deref().map_or("NULL".to_string(), lit);
    // LIGHTWEIGHT, or an in-memory style (12.2+).
    let style = row
        .style
        .as_deref()
        .map(str::trim)
        .filter(|s| *s != "REGULAR");
    let mut params = vec![format!("job_name => {job}")];
    match &row.program_name {
        Some(program) => {
            params.push(format!(
                "program_name => {}",
                name_lit(&row.program_owner, program)
            ));
            // Those styles always run a program.
            if let Some(style) = style {
                params.push(format!("job_style => {}", lit(style)));
            }
        }
        None => {
            params.push(format!(
                "job_type => {}",
                opt(&row.job_type.as_deref().map(str::trim).map(String::from))
            ));
            params.push(format!("job_action => {}", opt(&row.job_action)));
            if let Some(n) = integer(&row.arguments).filter(|n| *n > 0) {
                params.push(format!("number_of_arguments => {n}"));
            }
        }
    }
    if let Some(schedule) = &row.schedule_name {
        params.push(format!(
            "schedule_name => {}",
            name_lit(&row.schedule_owner, schedule)
        ));
    } else if let Some(condition) = &row.event_condition {
        let queue = qualified(&row.event_queue_owner, &row.event_queue_name).map(|q| {
            match &row.event_queue_agent {
                Some(a) => format!("{q}, {}", a.trim()),
                None => q,
            }
        });
        params.push(format!("start_date => {}", timestamp_lit(&row.start_tz)));
        params.push(format!("event_condition => {}", lit(condition)));
        params.push(format!("queue_spec => {}", opt(&queue)));
        params.push(format!("end_date => {}", timestamp_lit(&row.end_tz)));
    } else {
        params.push(format!("start_date => {}", timestamp_lit(&row.start_tz)));
        params.push(format!("repeat_interval => {}", opt(&row.repeat_interval)));
        params.push(format!("end_date => {}", timestamp_lit(&row.end_tz)));
    }
    if let Some(class) = &row.class {
        params.push(format!("job_class => {}", lit(&quote(class.trim()))));
    }
    params.push("enabled => FALSE".into());
    // Not for a lightweight job, whose AUTO_DROP can't be set (ORA-27354).
    if style.is_none() {
        params.push(format!(
            "auto_drop => {}",
            if row.auto_drop.as_deref() == Some("TRUE") {
                "TRUE"
            } else {
                "FALSE"
            }
        ));
    }
    params.push(format!("comments => {}", opt(&row.comments)));

    let mut out = format!(
        "BEGIN\n  DBMS_SCHEDULER.CREATE_JOB(\n    {}\n  );\n",
        params.join(",\n    ")
    );
    let mut attribute = |name: &str, value: String| {
        out.push_str(&format!(
            "  DBMS_SCHEDULER.SET_ATTRIBUTE({job}, '{name}', {value});\n"
        ));
    };
    // OFF is a job's default: its class's level applies.
    let level = match row.logging_level.as_deref().map(str::trim) {
        Some("RUNS") => Some("LOGGING_RUNS"),
        Some("FAILED RUNS") => Some("LOGGING_FAILED_RUNS"),
        Some("FULL") => Some("LOGGING_FULL"),
        _ => None,
    };
    if let Some(level) = level {
        attribute("logging_level", format!("DBMS_SCHEDULER.{level}"));
    }
    // Lightweight and in-memory jobs can't have these (ORA-27354 /
    // ORA-27495).
    if style.is_none() {
        if let Some(p) = integer(&row.priority).filter(|p| *p != 3) {
            attribute("job_priority", p.to_string());
        }
        if let Some(n) = integer(&row.max_runs) {
            attribute("max_runs", n.to_string());
        }
        if let Some(n) = integer(&row.max_failures) {
            attribute("max_failures", n.to_string());
        }
        if let Some(ms) = millis(&row.max_run_duration) {
            attribute(
                "max_run_duration",
                format!("NUMTODSINTERVAL({}, 'SECOND')", ms / 1000),
            );
        }
    }
    if row.restartable.as_deref() == Some("TRUE") {
        attribute("restartable", "TRUE".into());
    }
    if row.stop_on_window_close.as_deref() == Some("TRUE") {
        attribute("stop_on_window_close", "TRUE".into());
    }
    if let Some(n) = integer(&row.instance_id) {
        attribute("instance_id", n.to_string());
    }
    if let Some(cred) = &row.credential_name {
        attribute("credential_name", name_lit(&row.credential_owner, cred));
    }
    if let Some(dest) = &row.destination {
        attribute("destination_name", lit(dest.trim()));
    }
    if row.program_name.is_none() {
        for a in args {
            let Some(position) = integer(&Some(a.position.clone())) else {
                continue;
            };
            match &a.value {
                Some(value) => out.push_str(&format!(
                    "  DBMS_SCHEDULER.SET_JOB_ARGUMENT_VALUE({job}, {position}, {});\n",
                    lit(value)
                )),
                None => out.push_str(&format!(
                    "  -- argumento {position}: su valor no es texto, hay que asignarlo a mano\n"
                )),
            }
        }
    }
    if row.enabled {
        out.push_str(&format!("  DBMS_SCHEDULER.ENABLE({job});\n"));
    }
    out.push_str("END;\n/\n");
    out
}

// -------------------------------------------------------------- legacy jobs

/// One row of [`LEGACY_JOBS`].
#[derive(Debug, Default, Clone)]
struct LegacyRow {
    job: i64,
    log_user: Option<String>,
    schema_user: Option<String>,
    what: Option<String>,
    interval: Option<String>,
    broken: bool,
    last: Option<String>,
    this: Option<String>,
    next: Option<String>,
    failures: Option<String>,
    total_time: Option<String>,
    instance: Option<String>,
    next_local: Option<String>,
}

impl LegacyRow {
    fn from_cells(r: &[Option<String>]) -> LegacyRow {
        LegacyRow {
            job: integer(&col(r, 0)).unwrap_or_default(),
            log_user: col(r, 1),
            schema_user: col(r, 2),
            what: col(r, 3),
            interval: col(r, 4),
            broken: col(r, 5).is_some_and(|b| b.trim() == "Y"),
            last: col(r, 6),
            this: col(r, 7),
            next: col(r, 8),
            failures: col(r, 9),
            total_time: col(r, 10),
            instance: col(r, 11),
            next_local: col(r, 12),
        }
    }

    fn server_job(&self, access: &Access) -> ServerJob {
        let own = self.log_user.as_deref() == Some(access.user.as_str());
        let running = self.this.is_some();
        let failed = number(&self.failures).unwrap_or(0.0) > 0.0;
        ServerJob {
            id: format!("{LEGACY_ID}{}", self.job),
            name: format!("Job {}", self.job),
            kind: LEGACY_KIND.into(),
            group: self.schema_user.clone(),
            description: self.what.as_deref().map(|s| s.trim().to_string()),
            owner: self.log_user.clone(),
            enabled: !self.broken,
            schedule: self.interval.as_deref().map(|s| s.trim().to_string()),
            running,
            last_outcome: if failed {
                Some(JobOutcome::Failed)
            } else if self.last.is_some() {
                Some(JobOutcome::Succeeded)
            } else if running {
                Some(JobOutcome::Running)
            } else {
                None
            },
            last_started: if running {
                self.this.clone()
            } else {
                self.last.clone().filter(|_| !failed)
            },
            last_duration_ms: None,
            next_run: self.next.clone().filter(|_| !self.broken),
            can_run: own && !self.broken && !running,
            can_stop: false,
            can_enable: own,
            has_history: false,
            has_script: true,
            system: self
                .log_user
                .as_deref()
                .is_some_and(|u| access.oracle_schema(u)),
        }
    }

    fn detail(&self, access: &Access) -> JobDetail {
        let job = self.server_job(access);
        let properties = [
            ("Usuario", self.log_user.clone()),
            ("Esquema", self.schema_user.clone()),
            ("Intervalo", self.interval.clone()),
            ("Última ejecución correcta", self.last.clone()),
            ("Fallas", self.failures.clone()),
            (
                "Tiempo total",
                number(&self.total_time).map(|s| duration_text((s * 1000.0) as u64)),
            ),
            (
                "Instancia",
                self.instance.clone().filter(|i| i.trim() != "0"),
            ),
        ]
        .into_iter()
        .filter_map(|(label, value)| {
            Some(PropertyInfo {
                group: String::new(),
                label: label.into(),
                value: value?.trim().to_string(),
            })
        })
        .collect();
        JobDetail {
            steps: vec![JobStep {
                name: job.name.clone(),
                kind: Some("PL/SQL".into()),
                target: None,
                command: self.what.as_deref().map(|s| s.trim().to_string()),
            }],
            job,
            schedules: Vec::new(),
            properties,
            notes: Vec::new(),
        }
    }

    /// `DBMS_JOB.SUBMIT` as its owner (the job is created in the schema of
    /// whoever runs it).
    fn script(&self) -> String {
        let next = self
            .next_local
            .as_deref()
            .filter(|n| !self.broken && !n.starts_with("4000"))
            .map_or("SYSDATE".to_string(), |n| {
                format!("TO_DATE({}, 'YYYY-MM-DD HH24:MI:SS')", lit(n.trim()))
            });
        let mut out = format!(
            "DECLARE\n  j BINARY_INTEGER;\nBEGIN\n  DBMS_JOB.SUBMIT(j, {}, {next}, {});\n",
            lit(self.what.as_deref().unwrap_or("NULL;")),
            self.interval.as_deref().map_or("NULL".to_string(), lit),
        );
        if self.broken {
            out.push_str("  DBMS_JOB.BROKEN(j, TRUE);\n");
        }
        out.push_str("  COMMIT;\nEND;\n/\n");
        out
    }
}

/// Rows of [`LEGACY_JOBS`] followed by `tail`, from `DBA_JOBS` or else from
/// `ALL_JOBS`, which has only the login's own jobs: `true` then.
fn legacy_rows(
    c: &Connection,
    v: Views,
    tail: &str,
    params: &[&dyn ToDbValue],
    max: usize,
) -> std::result::Result<(Vec<Cells>, bool), oracledb::Error> {
    let sql = |v: Views| v.sql(&format!("{LEGACY_JOBS} {tail}"));
    match texts(c, &sql(v), params, max) {
        // The Scheduler's DBA_ views readable, but not DBA_JOBS.
        Err(e) if denied(&e) && v == Views::Dba => {
            texts(c, &sql(Views::All), params, max).map(|rows| (rows, true))
        }
        r => r.map(|rows| (rows, v == Views::All)),
    }
}

fn legacy_row(c: &Connection, v: Views, job: i64) -> Result<LegacyRow> {
    let (rows, _) = legacy_rows(c, v, "WHERE j.job = :1", &[&job], 1).map_err(err)?;
    rows.first()
        .map(|r| LegacyRow::from_cells(r))
        .ok_or_else(|| not_found(&format!("{LEGACY_ID}{job}")))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(v: &[&str]) -> Cells {
        v.iter()
            .map(|s| (!s.is_empty()).then(|| s.to_string()))
            .collect()
    }

    /// A row of [`JOBS`]: a daily PL/SQL job of APP.
    fn daily() -> Cells {
        let mut r = vec![""; 47];
        for (i, v) in [
            (0, "APP"),
            (1, "NIGHTLY_LOAD"),
            (2, "PLSQL_BLOCK"),
            (3, "BEGIN load_all; END;"),
            (6, "Carga nocturna"),
            (7, "TRUE"),
            (8, "SCHEDULED"),
            (9, "CALENDAR"),
            (10, "FREQ=DAILY;BYHOUR=3"),
            (18, "2026-11-01T03:00:00Z"),
            (19, "2026-11-12T03:00:00Z"),
            (20, "1234.4"),
            (21, "2026-11-13T03:00:00Z"),
            (23, "APP"),
            (24, "DEFAULT_JOB_CLASS"),
            (25, "FALSE"),
            (26, "REGULAR"),
            (27, "FALSE"),
            (28, "FALSE"),
            (29, "3"),
            (30, "12"),
            (31, "1"),
            (35, "3600000"),
            (36, "RUNS"),
            (42, "0"),
            (
                43,
                "2026-11-01 00:00:00.000000000 AMERICA/ARGENTINA/BUENOS_AIRES",
            ),
        ] {
            r[i] = v;
        }
        cells(&r)
    }

    fn access(user: &str) -> Access {
        Access {
            user: user.into(),
            ..Default::default()
        }
    }

    #[test]
    fn ids_round_trip_and_odd_ones_are_refused() {
        assert_eq!(job_id("APP", "NIGHTLY_LOAD"), "APP.NIGHTLY_LOAD");
        assert_eq!(job_id("APP", "ORA$AT_1#"), "APP.ORA$AT_1#");
        assert_eq!(job_id("App", "my job"), "\"App\".\"my job\"");
        for (owner, name) in [("APP", "NIGHTLY_LOAD"), ("App", "my job"), ("A", "x.y")] {
            assert_eq!(
                parse_id(&job_id(owner, name)).unwrap(),
                JobId::Scheduler {
                    owner: owner.into(),
                    name: name.into()
                }
            );
        }
        assert_eq!(parse_id("DBMS_JOB:42").unwrap(), JobId::Legacy(42));
        let long = format!("APP.{}", "X".repeat(MAX_NAME + 1));
        for bad in [
            "",
            "APP",
            "APP.",
            ".JOB",
            "app.job",
            "APP.JOB.X",
            "APP.JOB'; DROP TABLE t --",
            "APP.JOB, SYS.PURGE_LOG",
            "APP JOB.X",
            "\"APP\".\"JOB",
            "\"APP\".\"JO\"B\"",
            "\"APP\".\"\"",
            "\"APP\".\"J\u{0}\"",
            "\"APP\" . \"JOB\"",
            "DBMS_JOB:",
            "DBMS_JOB:1a",
            "DBMS_JOB:-1",
            "DBMS_JOB:12345678901",
            // Shapes the list never gives: plain names quoted, leading zeros.
            "\"APP\".\"JOB\"",
            "\"APP\".\"my job\"x",
            "DBMS_JOB:0042",
            long.as_str(),
        ] {
            assert!(parse_id(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn calls_get_the_quoted_name_and_refuse_a_comma() {
        assert_eq!(
            scheduler_name("APP", "NIGHTLY").unwrap(),
            "\"APP\".\"NIGHTLY\""
        );
        let JobId::Scheduler { owner, name } = parse_id("\"APP\".\"A,SYS.PURGE_LOG\"").unwrap()
        else {
            panic!()
        };
        assert!(scheduler_name(&owner, &name).is_err());
    }

    #[test]
    fn views_are_picked_in_every_query() {
        for sql in [
            JOBS,
            LAST_OUTCOMES,
            STATE,
            RUNS,
            STEP_RUNS,
            RUNNING,
            CHAIN_STEPS,
            ARGS,
        ] {
            let all = Views::All.sql(sql);
            assert!(!all.contains("{v}") && !all.contains("dba_"), "{all}");
            assert!(Views::Dba.sql(sql).contains("dba_scheduler_"));
        }
        assert!(Views::Dba.sql(LEGACY_JOBS).contains("FROM dba_jobs"));
        assert!(RUNS.contains("ROWNUM <= :3") && STATE.contains("j.owner = :1"));
        assert!(STEP_RUNS.contains(&format!("ROWNUM <= {MAX_STEP_RUNS}")));
        assert!(utc!("x").contains("SYS_EXTRACT_UTC(x)"));
    }

    #[test]
    fn a_row_maps_to_a_job() {
        let row = JobRow::from_cells(&daily());
        assert_eq!(row.program_action, None);
        let job = row.server_job(&access("APP"), Some("FAILED"));
        assert_eq!(job.id, "APP.NIGHTLY_LOAD");
        assert_eq!(job.kind, "DBMS_SCHEDULER");
        assert_eq!(job.group.as_deref(), Some("APP"));
        assert_eq!(job.description.as_deref(), Some("Carga nocturna"));
        assert_eq!(job.schedule.as_deref(), Some("FREQ=DAILY;BYHOUR=3"));
        assert_eq!(job.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(job.last_started.as_deref(), Some("2026-11-12T03:00:00Z"));
        assert_eq!(job.last_duration_ms, Some(1234));
        assert_eq!(job.next_run.as_deref(), Some("2026-11-13T03:00:00Z"));
        assert!(job.enabled && !job.running);
        assert!(job.can_run && job.can_enable && !job.can_stop);
        assert!(job.has_history && job.has_script);

        // Someone else's, without privileges: read only.
        let other = row.server_job(&access("BOB"), None);
        assert!(!other.can_run && !other.can_enable && !other.can_stop);
        assert_eq!(other.last_outcome, None);
        // CREATE ANY JOB reaches it, but not SYS's jobs.
        let admin = Access {
            user: "SYSTEM".into(),
            any_job: true,
            ..Default::default()
        };
        assert!(row.server_job(&admin, None).can_run);
        let sys = JobRow {
            owner: "SYS".into(),
            ..row.clone()
        };
        assert!(!sys.server_job(&admin, None).can_enable);
        // Nor the jobs of other schemas Oracle maintains (Autonomous
        // Database's service jobs), nor system jobs; SYSTEM's own jobs yes.
        let admin = Access {
            user: "ADMIN".into(),
            any_job: true,
            maintained: ["SYS", "SYSTEM", "C##CLOUD$SERVICE"]
                .map(String::from)
                .into(),
            ..Default::default()
        };
        let service = JobRow {
            owner: "C##CLOUD$SERVICE".into(),
            ..row.clone()
        };
        let job = service.server_job(&admin, None);
        assert!(!job.can_run && !job.can_enable && !job.can_stop);
        let system = JobRow {
            system: Some("TRUE".into()),
            ..row.clone()
        };
        assert!(!system.server_job(&admin, None).can_run);
        assert!(row.server_job(&admin, None).can_run);
        let in_system = JobRow {
            owner: "SYSTEM".into(),
            ..row.clone()
        };
        assert!(in_system.server_job(&admin, None).can_run);
        // Its owner, or an ALTER grant, still manage them.
        assert!(
            service
                .server_job(&access("C##CLOUD$SERVICE"), None)
                .can_run
        );
        let granted = Access {
            user: "BOB".into(),
            granted: [("APP".to_string(), "NIGHTLY_LOAD".to_string())].into(),
            ..Default::default()
        };
        assert!(row.server_job(&granted, None).can_enable);
        assert!(system.server_job(&granted, None).can_enable);
    }

    #[test]
    fn oracles_own_jobs_are_system_ones() {
        let row = JobRow::from_cells(&daily());
        let maintained = Access {
            user: "APP".into(),
            maintained: ["SYS", "SYSTEM", "ORACLE_OCM", "C##CLOUD$SERVICE"]
                .map(String::from)
                .into(),
            ..Default::default()
        };
        // A user's job isn't.
        assert!(!row.server_job(&maintained, None).system);
        // SYSTEM = 'TRUE', whoever owns it.
        let flagged = JobRow {
            system: Some("TRUE".into()),
            ..row.clone()
        };
        assert!(flagged.server_job(&access("APP"), None).system);
        // A schema Oracle maintains, and SYS even where that can't be read
        // (before 12.1).
        for owner in ["ORACLE_OCM", "C##CLOUD$SERVICE", "SYS"] {
            let own = JobRow {
                owner: owner.into(),
                ..row.clone()
            };
            assert!(own.server_job(&maintained, None).system, "{owner}");
        }
        let sys = JobRow {
            owner: "SYS".into(),
            ..row.clone()
        };
        assert!(sys.server_job(&access("APP"), None).system);
        // SYSTEM is Oracle-maintained, but DBAs make their jobs there.
        let in_system = JobRow {
            owner: "SYSTEM".into(),
            ..row.clone()
        };
        assert!(!in_system.server_job(&maintained, None).system);
        // A DBMS_JOB job before 19c, by its user.
        let legacy = |user: &str| LegacyRow {
            job: 7,
            log_user: Some(user.into()),
            ..Default::default()
        };
        assert!(legacy("ORACLE_OCM").server_job(&maintained).system);
        assert!(!legacy("APP").server_job(&maintained).system);
    }

    #[test]
    fn nothing_runs_while_the_scheduler_is_off() {
        let job = JobRow::from_cells(&daily()).server_job(&access("APP"), None);
        let legacy = LegacyRow {
            job: 7,
            log_user: Some("APP".into()),
            ..Default::default()
        }
        .server_job(&access("APP"));
        assert!(job.can_run && legacy.can_run);

        let on = Off::default();
        assert!(on.scheduler_job(job.clone()).can_run && on.legacy_job(legacy.clone()).can_run);
        assert!(on.notes().is_empty());

        let disabled = Off {
            disabled: true,
            no_queue: false,
        };
        let j = disabled.scheduler_job(job.clone());
        assert!(!j.can_run && j.can_enable, "only running is off");
        assert!(
            disabled.legacy_job(legacy.clone()).can_run,
            "DBMS_JOB doesn't go through the Scheduler"
        );
        assert_eq!(disabled.notes().len(), 1);

        let no_queue = Off {
            disabled: false,
            no_queue: true,
        };
        assert!(!no_queue.scheduler_job(job).can_run);
        assert!(!no_queue.legacy_job(legacy).can_run);
        assert!(no_queue.notes()[0].starts_with("job_queue_processes es 0"));
    }

    #[test]
    fn running_disabled_and_legacy_scheduler_jobs() {
        let mut row = JobRow::from_cells(&daily());
        row.state = Some("RUNNING".into());
        let job = row.server_job(&access("APP"), None);
        assert!(job.running && job.can_stop && !job.can_run);
        assert_eq!(job.last_outcome, Some(JobOutcome::Running));
        assert_eq!(
            row.server_job(&access("APP"), Some("SUCCEEDED"))
                .last_outcome,
            Some(JobOutcome::Succeeded)
        );
        row.state = Some("RETRY SCHEDULED".into());
        assert_eq!(
            row.server_job(&access("APP"), Some("FAILED")).last_outcome,
            Some(JobOutcome::Retrying)
        );
        row.state = Some("DISABLED".into());
        row.enabled = false;
        assert_eq!(row.server_job(&access("APP"), None).next_run, None);
        row.name = "DBMS_JOB$_7".into();
        assert_eq!(row.server_job(&access("APP"), None).kind, "DBMS_JOB");
    }

    #[test]
    fn schedules_as_the_dictionary_states_them() {
        let base = JobRow::default();
        let with = |f: &dyn Fn(&mut JobRow)| {
            let mut r = base.clone();
            f(&mut r);
            r.schedule()
        };
        assert_eq!(
            with(&|r| {
                r.schedule_type = Some("NAMED".into());
                r.schedule_owner = Some("SYS".into());
                r.schedule_name = Some("DAILY_PURGE_SCHEDULE".into());
                r.named_repeat = Some("freq=daily;byhour=3".into());
            })
            .as_deref(),
            Some("freq=daily;byhour=3")
        );
        assert_eq!(
            with(&|r| {
                r.schedule_type = Some("WINDOW_GROUP".into());
                r.schedule_owner = Some("SYS".into());
                r.schedule_name = Some("MAINTENANCE_WINDOW_GROUP".into());
            })
            .as_deref(),
            Some("Grupo de ventanas SYS.MAINTENANCE_WINDOW_GROUP")
        );
        assert_eq!(
            with(&|r| {
                r.schedule_type = Some("WINDOW".into());
                r.schedule_owner = Some("SYS".into());
                r.schedule_name = Some("WEEKNIGHT_WINDOW".into());
            })
            .as_deref(),
            Some("Ventana SYS.WEEKNIGHT_WINDOW")
        );
        assert_eq!(
            with(&|r| {
                r.schedule_type = Some("ONCE".into());
                r.start = Some("2026-12-01T00:00:00Z".into());
            })
            .as_deref(),
            Some("Una vez: 2026-12-01T00:00:00Z")
        );
        assert_eq!(
            with(&|r| {
                r.schedule_type = Some("EVENT".into());
                r.event_condition = Some("tab.user_data.kind = 'X'".into());
            })
            .as_deref(),
            Some("Evento: tab.user_data.kind = 'X'")
        );
        assert_eq!(
            with(&|r| {
                r.schedule_type = Some("PLSQL".into());
                r.repeat_interval = Some("SYSDATE + 1".into());
            })
            .as_deref(),
            Some("SYSDATE + 1")
        );
        assert_eq!(with(&|r| r.schedule_type = Some("IMMEDIATE".into())), None);
    }

    #[test]
    fn detail_steps_and_properties() {
        let row = JobRow::from_cells(&daily());
        let step = row.own_step().unwrap();
        assert_eq!(step.name, "NIGHTLY_LOAD");
        assert_eq!(step.kind.as_deref(), Some("PL/SQL"));
        assert_eq!(step.command.as_deref(), Some("BEGIN load_all; END;"));
        let props = row.properties();
        let get = |l: &str| {
            props
                .iter()
                .find(|p| p.label == l)
                .map(|p| p.value.as_str())
        };
        assert_eq!(get("Clase"), Some("DEFAULT_JOB_CLASS"));
        assert_eq!(get("Del sistema"), Some("No"));
        assert_eq!(get("Duración máxima"), Some("01:00:00"));
        assert_eq!(
            props
                .iter()
                .find(|p| p.label == "Ejecuciones")
                .unwrap()
                .group,
            "Ejecución"
        );

        let program = JobRow {
            job_type: None,
            program_owner: Some("APP".into()),
            program_name: Some("LOAD_PROG".into()),
            program_type: Some("STORED_PROCEDURE".into()),
            program_action: Some("APP.LOAD".into()),
            ..row.clone()
        };
        let step = program.own_step().unwrap();
        assert_eq!(
            (step.name.as_str(), step.kind.as_deref()),
            ("APP.LOAD_PROG", Some("Procedimiento"))
        );

        let chain = JobRow {
            job_type: Some("CHAIN".into()),
            job_action: Some("load_chain".into()),
            ..row
        };
        assert_eq!(chain.chain(), Some(("APP".into(), "LOAD_CHAIN".into())));
        assert_eq!(
            object_name("\"Other\".\"Ch\"", "APP"),
            Some(("Other".into(), "Ch".into()))
        );
        assert_eq!(object_name("a.b.c", "APP"), None);
    }

    #[test]
    fn chain_steps_follow_their_rules() {
        let steps: Vec<String> = ["LOAD", "CHECK", "EXTRACT", "ORPHAN"]
            .map(String::from)
            .into();
        let rule = |c: &str, a: &str| (String::new(), c.to_string(), a.to_string());
        let rules = [
            rule("CHECK COMPLETED", "END"),
            rule("LOAD SUCCEEDED", "START \"CHECK\""),
            rule("EXTRACT COMPLETED", "START load"),
            rule("TRUE", "START \"EXTRACT\""),
        ];
        assert_eq!(
            step_order(&steps, &rules),
            ["EXTRACT", "LOAD", "CHECK", "ORPHAN"]
        );
    }

    #[test]
    fn run_messages_triggers_and_outcomes() {
        let r = cells(&[
            "13186",
            "2026-11-12T12:52:54Z",
            "0",
            "FAILED",
            "20001",
            "ORA-20001: dbine boom\nORA-06512: at line 1",
            "ORA-20001: dbine boom\nORA-06512: at line 1\n",
            "",
            "13186",
            "REASON=\"manual slave run\"",
        ]);
        let run = finished_run(&r, Some("CALENDAR"));
        assert_eq!(run.id.as_deref(), Some("13186"));
        assert_eq!(run.outcome, JobOutcome::Failed);
        assert_eq!(run.trigger.as_deref(), Some("manual"));
        assert_eq!(run.duration_ms, Some(0));
        assert_eq!(
            run.message.as_deref(),
            Some("ORA-20001: dbine boom\nORA-06512: at line 1")
        );

        assert_eq!(
            trigger(true, None, Some("CALENDAR")).as_deref(),
            Some("programada")
        );
        assert_eq!(
            trigger(true, None, Some("EVENT")).as_deref(),
            Some("evento")
        );
        assert_eq!(
            trigger(true, Some("REASON=\"manually run\""), None).as_deref(),
            Some("manual")
        );
        assert_eq!(trigger(false, None, None), None);

        assert_eq!(
            run_message(
                Some(0),
                Some("REASON=\"Stop job called by user: 'SYSTEM'\""),
                None,
                None
            )
            .as_deref(),
            Some("Stop job called by user: 'SYSTEM'")
        );
        assert_eq!(
            run_message(Some(0), None, None, Some("hola\n")).as_deref(),
            Some("hola")
        );
        assert_eq!(
            run_message(Some(1013), None, None, None).as_deref(),
            Some("ORA-01013")
        );
        assert_eq!(
            run_message(Some(0), Some("CHAIN_LOG_ID=\"1\""), None, None),
            None
        );

        assert_eq!(outcome("STOPPED"), JobOutcome::Canceled);
        assert_eq!(outcome("SUCCEEDED"), JobOutcome::Succeeded);
        assert_eq!(outcome("RETRY SCHEDULED"), JobOutcome::Retrying);
        assert_eq!(outcome("WHATEVER"), JobOutcome::Unknown);
    }

    #[test]
    fn step_runs_group_by_chain_run() {
        let step = |name: &str, status: &str, start: &str, info: &str| {
            cells(&[name, status, start, "0", "0", info, "", ""])
        };
        // Newest first, as STEP_RUNS reads them.
        let rows = vec![
            step(
                "S2",
                "FAILED",
                "2026-11-12T12:53:47Z",
                "CHAIN_LOG_ID=\"200\", STEP_NAME=\"S2\", ORA-20002: step boom",
            ),
            step(
                "S1",
                "SUCCEEDED",
                "2026-11-12T12:53:46Z",
                "CHAIN_LOG_ID=\"200\", STEP_NAME=\"S1\"",
            ),
            step(
                "S1",
                "SUCCEEDED",
                "2026-11-11T12:00:00Z",
                "CHAIN_LOG_ID=\"100\", STEP_NAME=\"S1\"",
            ),
        ];
        let by = group_steps(&rows);
        let run = &by["200"];
        assert_eq!(
            run.iter().map(|s| s.name.as_str()).collect::<Vec<_>>(),
            ["S1", "S2"]
        );
        assert_eq!(run[1].outcome, JobOutcome::Failed);
        assert_eq!(run[1].message.as_deref(), Some("ORA-20002: step boom"));
        assert_eq!(run[0].message, None);
        assert_eq!(by["100"].len(), 1);
        assert_eq!(
            chain_log_id(Some("CHAIN_LOG_ID=\"13200\", REASON=\"manual slave run\"")).as_deref(),
            Some("13200")
        );
    }

    #[test]
    fn get_ddl_is_qualified() {
        let ddl = "\nBEGIN\ndbms_scheduler.create_job('\"NIGHTLY\"',\njob_type=>'PLSQL_BLOCK', job_action=>\n'BEGIN x(''\"NIGHTLY\"''); END;'\n);\ndbms_scheduler.enable('\"NIGHTLY\"');\nCOMMIT;\nEND;\n";
        let out = qualify(ddl, "APP", "NIGHTLY");
        assert!(out.contains("create_job('\"APP\".\"NIGHTLY\"',"), "{out}");
        assert!(out.contains("enable('\"APP\".\"NIGHTLY\"')"), "{out}");
        assert!(
            out.contains("x(''\"NIGHTLY\"'')"),
            "the action is untouched: {out}"
        );
        assert!(out.ends_with("END;\n/\n"), "{out}");
    }

    #[test]
    fn the_script_built_from_the_dictionary() {
        let mut row = JobRow::from_cells(&daily());
        row.comments = Some("it's nightly".into());
        let args = [
            JobArg {
                position: "1".into(),
                name: None,
                kind: Some("VARCHAR2".into()),
                value: Some("O'Brien".into()),
            },
            JobArg {
                position: "2".into(),
                name: None,
                kind: Some("SYS.ANYDATA".into()),
                value: None,
            },
        ];
        let s = build_script(&row, &args);
        for part in [
            "DBMS_SCHEDULER.CREATE_JOB(",
            "job_name => '\"APP\".\"NIGHTLY_LOAD\"'",
            "job_type => 'PLSQL_BLOCK'",
            "job_action => 'BEGIN load_all; END;'",
            "start_date => TO_TIMESTAMP_TZ('2026-11-01 00:00:00.000000000 AMERICA/ARGENTINA/BUENOS_AIRES', 'YYYY-MM-DD HH24:MI:SS.FF TZR')",
            "repeat_interval => 'FREQ=DAILY;BYHOUR=3'",
            "end_date => NULL",
            "job_class => '\"DEFAULT_JOB_CLASS\"'",
            "enabled => FALSE",
            "auto_drop => FALSE",
            "comments => 'it''s nightly'",
            "'logging_level', DBMS_SCHEDULER.LOGGING_RUNS",
            "'max_run_duration', NUMTODSINTERVAL(3600, 'SECOND')",
            "SET_JOB_ARGUMENT_VALUE('\"APP\".\"NIGHTLY_LOAD\"', 1, 'O''Brien')",
            "-- argumento 2",
            "DBMS_SCHEDULER.ENABLE('\"APP\".\"NIGHTLY_LOAD\"');",
            "END;\n/\n",
        ] {
            assert!(s.contains(part), "{part}\n{s}");
        }
        assert!(!s.contains("job_priority"), "the default is left out");

        let named = JobRow {
            job_type: None,
            job_action: None,
            program_owner: Some("SYS".into()),
            program_name: Some("PURGE_LOG_PROG".into()),
            schedule_owner: Some("SYS".into()),
            schedule_name: Some("DAILY_PURGE_SCHEDULE".into()),
            enabled: false,
            logging_level: Some("OFF".into()),
            ..JobRow::from_cells(&daily())
        };
        let s = build_script(&named, &[]);
        assert!(
            s.contains("program_name => '\"SYS\".\"PURGE_LOG_PROG\"'"),
            "{s}"
        );
        assert!(
            s.contains("schedule_name => '\"SYS\".\"DAILY_PURGE_SCHEDULE\"'"),
            "{s}"
        );
        assert!(
            !s.contains("repeat_interval") && !s.contains("ENABLE("),
            "{s}"
        );
        assert!(!s.contains("logging_level"), "OFF is the default: {s}");
        assert!(!s.contains("job_style"), "REGULAR is the default: {s}");

        let lightweight = JobRow {
            style: Some("LIGHTWEIGHT".into()),
            max_runs: Some("5".into()),
            ..named
        };
        let s = build_script(&lightweight, &[]);
        assert!(s.contains("job_style => 'LIGHTWEIGHT'"), "{s}");
        assert!(
            !s.contains("auto_drop") && !s.contains("max_runs"),
            "a lightweight job can't set them: {s}"
        );
    }

    #[test]
    fn legacy_jobs() {
        let r = cells(&[
            "21",
            "APP",
            "APP",
            "purge_old;",
            "SYSDATE + 1",
            "N",
            "2026-11-11T03:00:00Z",
            "",
            "2026-11-13T03:00:00Z",
            "0",
            "12",
            "0",
            "2026-11-13 00:00:00",
        ]);
        let row = LegacyRow::from_cells(&r);
        let job = row.server_job(&access("APP"));
        assert_eq!(job.id, "DBMS_JOB:21");
        assert_eq!(job.kind, "DBMS_JOB");
        assert_eq!(job.schedule.as_deref(), Some("SYSDATE + 1"));
        assert_eq!(job.last_outcome, Some(JobOutcome::Succeeded));
        assert!(job.enabled && job.can_run && job.can_enable);
        assert!(!job.can_stop && !job.has_history && job.has_script);
        assert!(!row.server_job(&access("BOB")).can_enable, "only its owner");
        assert_eq!(parse_id(&job.id).unwrap(), JobId::Legacy(21));

        let s = row.script();
        assert!(
            s.contains("DBMS_JOB.SUBMIT(j, 'purge_old;', TO_DATE('2026-11-13 00:00:00', 'YYYY-MM-DD HH24:MI:SS'), 'SYSDATE + 1');"),
            "{s}"
        );
        let broken = LegacyRow {
            broken: true,
            failures: Some("16".into()),
            next_local: Some("4000-01-01 00:00:00".into()),
            ..row
        };
        let job = broken.server_job(&access("APP"));
        assert!(!job.enabled && !job.can_run && job.next_run.is_none());
        assert_eq!(job.last_outcome, Some(JobOutcome::Failed));
        let s = broken.script();
        assert!(
            s.contains(", SYSDATE, ") && s.contains("DBMS_JOB.BROKEN(j, TRUE);"),
            "{s}"
        );
        let props = broken.detail(&access("APP")).properties;
        assert!(props
            .iter()
            .any(|p| p.label == "Tiempo total" && p.value == "00:00:12"));
        assert!(!props.iter().any(|p| p.label == "Instancia"));
    }

    #[test]
    fn info_pairs_and_durations() {
        let (pairs, rest) = info_parts("CHAIN_LOG_ID=\"13200\", STEP_NAME=\"S2\", ORA-20002: x");
        assert_eq!(pairs.len(), 2);
        assert_eq!(rest, "ORA-20002: x");
        assert_eq!(info_parts("plain text").1, "plain text");
        assert_eq!(duration_text(90_061_000), "1 d 01:01:01");
        assert_eq!(millis(&Some("6.217".into())), Some(6));
        assert_eq!(millis(&Some("-1".into())), None);
    }
}
