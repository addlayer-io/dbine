//! Server jobs ([`dbine_driver::Session::jobs`]): SQL Server Agent's jobs,
//! in msdb, on SQL Server and Azure SQL Managed Instance.
//!
//! Everything is read through the Agent's own procedures (`sp_help_job`,
//! `sp_help_jobstep`, `sp_help_jobschedule`, `sp_help_jobserver`,
//! `sp_help_jobactivity`, `sp_help_jobhistory`) rather than msdb's tables
//! (`sysjobs`, `sysjobsteps`, `sysschedules`, `sysjobactivity`,
//! `sysjobhistory`…): the Agent roles (SQLAgentUserRole, SQLAgentReaderRole,
//! SQLAgentOperatorRole) may run those procedures but not read the tables,
//! and the procedures apply the roles' rules (a SQLAgentUserRole member sees
//! only the jobs it owns). sysadmin gets the same answers.
//!
//! - Run now is `sp_start_job` (the Agent queues the run and the call
//!   returns), stop is `sp_stop_job`, enable is `sp_update_job @enabled`.
//!   They need the Agent running (enable doesn't) and, for a login that
//!   isn't sysadmin nor SQLAgentOperatorRole, owning the job. On a target
//!   server, a job its master sent runs and stops there but can't be
//!   changed; on the master, a multiserver job is started (posted to its
//!   targets) by sysadmin only.
//! - The script is the T-SQL that creates the job again: `sp_add_job`, its
//!   steps (`sp_add_jobstep`), schedules (`sp_add_schedule` and
//!   `sp_attach_schedule`, reusing a schedule other jobs share) and servers
//!   (`sp_add_jobserver`), in one transaction.
//! - Times: msdb keeps the server's local time (`run_date` / `run_time` as
//!   `yyyymmdd` / `hhmmss` integers, the rest as `datetime`). They become
//!   UTC with the server's offset; from SQL Server 2022, and on Managed
//!   Instance, with its time zone (`CURRENT_TIMEZONE_ID()` and `AT TIME
//!   ZONE`), so a run from before a daylight-saving change gets its own
//!   offset.
//! - Azure SQL Database, Synapse and SQL database in Fabric have no Agent:
//!   an empty list with a note. Azure's elastic jobs live in a job database
//!   of their own, which DBine doesn't read. Fabric warehouses and Babelfish
//!   don't have jobs (`Capabilities::jobs` false); Babelfish reached through
//!   a `sqlserver` connection gets the note too.
//! - Versions: everything here works from SQL Server 2005 (the probe needs
//!   `HAS_PERMS_BY_NAME` and `sys.configurations`), which covers every
//!   server the driver and the `sqlserver_legacy` host reach (2008 R2 /
//!   2012 on TLS 1.0). The batches still leave out what 2000's msdb lacked
//!   (`sysjobactivity`, shared schedules: the script falls back to
//!   `sp_add_jobschedule`) by the version the probe reads.

use crate::create_db::literal;
use crate::variant::Variant;
use crate::{comment_text, err, is_desync, SqlServerSession};
use chrono::{Duration, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{
    Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, JobStepRun, PropertyInfo, Result,
    ServerJob,
};
use futures::TryStreamExt;
use std::collections::{BTreeSet, HashMap};
use std::fmt::Write as _;
use tiberius::{QueryItem, Row, Uuid};

/// The tag every job carries.
const KIND: &str = "SQL Server Agent";

/// What msdb stores as the description of a job created without one.
const NO_DESCRIPTION: &str = "No description available.";

const AZURE_SQL_NOTE: &str = "Azure SQL Database no tiene el Agente SQL Server. Los trabajos elásticos (Elastic Jobs) viven en una base de trabajos aparte, que DBine todavía no lee.";
const NO_AGENT_NOTE: &str = "Este servicio no tiene el Agente SQL Server.";
const NO_ACCESS_NOTE: &str = "Este usuario no puede ver los jobs del Agente SQL Server: necesita el rol SQLAgentUserRole, SQLAgentReaderRole o SQLAgentOperatorRole en msdb, o ser sysadmin.";
const OWN_JOBS_NOTE: &str = "Se ven solo los jobs de los que este usuario es dueño (rol SQLAgentUserRole): para ver todos hace falta SQLAgentReaderRole o SQLAgentOperatorRole en msdb.";
const AGENT_STOPPED_NOTE: &str = "El Agente SQL Server no está en ejecución: los jobs no corren según su programación ni se pueden ejecutar desde DBine.";
const EXPRESS_NOTE: &str =
    "SQL Server Express no incluye el Agente SQL Server: estos jobs no se ejecutan.";
const NOT_FOUND: &str = "El job no existe o este usuario no puede verlo.";
const OPEN_TRANSACTION: &str = "Hay una transacción abierta en esta sesión: confirmala o deshacela antes de ejecutar, detener o cambiar un job.";
const UNSUPPORTED: &str = "este motor no tiene jobs del servidor";

/// msdb's error for a job id that doesn't exist (or the login can't see).
const JOB_NOT_FOUND: u32 = 14262;

/// The variants whose servers can have Agent jobs: SQL Server (and Managed
/// Instance) and Azure SQL (which may point at a Managed Instance; Azure
/// SQL Database answers with a note).
pub(crate) fn supported(v: Variant) -> bool {
    matches!(v, Variant::SqlServer | Variant::AzureSql)
}

fn check_variant(s: &SqlServerSession) -> Result<()> {
    if supported(s.variant) {
        Ok(())
    } else {
        Err(Error::Unsupported(UNSUPPORTED.into()))
    }
}

/// The id as the other calls take it back: a job's GUID, lowercase. Anything
/// else is refused before it reaches the server.
fn job_id(id: &str) -> Result<String> {
    let t = id.trim();
    let ok = t.len() == 36
        && t.char_indices().all(|(i, c)| match i {
            8 | 13 | 18 | 23 => c == '-',
            _ => c.is_ascii_hexdigit(),
        });
    if ok {
        Ok(t.to_ascii_lowercase())
    } else {
        let shown: String = comment_text(t).chars().take(60).collect();
        Err(Error::Query(format!(
            "«{shown}» no es un id de job del Agente SQL Server"
        )))
    }
}

// ---------------------------------------------------------------------------
// Reading result sets
// ---------------------------------------------------------------------------

/// One result set with its column names (an empty one still says what it
/// is: the procedures are told apart by their columns).
struct ResultSet {
    columns: Vec<String>,
    rows: Vec<Row>,
}

impl ResultSet {
    fn has(&self, column: &str) -> bool {
        self.columns.iter().any(|c| c == column)
    }
}

fn first_with<'a>(sets: &'a [ResultSet], column: &str) -> Option<&'a ResultSet> {
    sets.iter().find(|s| s.has(column))
}

/// Every result set of a batch. `runs_cap`: in `sp_help_jobhistory`'s set
/// (newest first), keep no rows past that many runs (job outcome rows). The
/// rest of the stream is still read to its end: the bytes come either way,
/// and a stream left unread makes the next query flush it with a warning
/// per packet.
async fn try_sets(
    s: &mut SqlServerSession,
    sql: &str,
    params: &[&str],
    runs_cap: Option<usize>,
) -> tiberius::Result<Vec<ResultSet>> {
    let p: Vec<&dyn tiberius::ToSql> = params.iter().map(|v| v as &dyn tiberius::ToSql).collect();
    let mut stream = s.client.query(sql, &p).await?;
    let mut sets: Vec<ResultSet> = Vec::new();
    let mut runs = 0usize;
    let mut full = false;
    while let Some(item) = stream.try_next().await? {
        match item {
            QueryItem::Metadata(m) => sets.push(ResultSet {
                columns: m.columns().iter().map(|c| c.name().to_string()).collect(),
                rows: Vec::new(),
            }),
            QueryItem::Row(row) => {
                let Some(set) = sets.last_mut() else {
                    continue;
                };
                if let Some(cap) = runs_cap.filter(|_| set.has("instance_id")) {
                    if !full && int(&row, "step_id") == Some(0) {
                        runs += 1;
                        full = runs > cap;
                    }
                    if full {
                        continue;
                    }
                }
                set.rows.push(row);
            }
        }
    }
    Ok(sets)
}

async fn sets(
    s: &mut SqlServerSession,
    sql: &str,
    params: &[&str],
    runs_cap: Option<usize>,
) -> Result<Vec<ResultSet>> {
    let mut r = try_sets(s, sql, params, runs_cap).await;
    if matches!(&r, Err(e) if is_desync(e)) {
        s.reconnect().await?;
        r = try_sets(s, sql, params, runs_cap).await;
    }
    r.map_err(job_error)
}

fn job_error(e: tiberius::error::Error) -> Error {
    match &e {
        tiberius::error::Error::Server(t) if t.code() == JOB_NOT_FOUND => {
            Error::Query(NOT_FOUND.into())
        }
        _ => err(e),
    }
}

/// An integer column, whatever its width; text such as `0 (unknown)` reads
/// its leading number.
fn int(r: &Row, c: &str) -> Option<i64> {
    if let Ok(Some(v)) = r.try_get::<i32, _>(c) {
        return Some(v.into());
    }
    if let Ok(Some(v)) = r.try_get::<u8, _>(c) {
        return Some(v.into());
    }
    if let Ok(Some(v)) = r.try_get::<i16, _>(c) {
        return Some(v.into());
    }
    if let Ok(Some(v)) = r.try_get::<i64, _>(c) {
        return Some(v);
    }
    if let Ok(Some(v)) = r.try_get::<bool, _>(c) {
        return Some(v.into());
    }
    if let Ok(Some(v)) = r.try_get::<&str, _>(c) {
        return leading_int(v);
    }
    None
}

fn leading_int(v: &str) -> Option<i64> {
    let t = v.trim_start();
    let end = t
        .char_indices()
        .find(|&(i, c)| !(c.is_ascii_digit() || (i == 0 && c == '-')))
        .map_or(t.len(), |(i, _)| i);
    t[..end].parse().ok()
}

fn text(r: &Row, c: &str) -> Option<String> {
    r.try_get::<&str, _>(c)
        .ok()
        .flatten()
        .map(str::trim)
        .filter(|v| !v.is_empty())
        .map(str::to_string)
}

fn datetime(r: &Row, c: &str) -> Option<NaiveDateTime> {
    r.try_get::<NaiveDateTime, _>(c).ok().flatten()
}

fn guid(r: &Row, c: &str) -> Option<Uuid> {
    if let Ok(Some(v)) = r.try_get::<Uuid, _>(c) {
        return Some(v);
    }
    text(r, c).and_then(|t| Uuid::parse_str(&t).ok())
}

/// An operator's name as `sp_help_job` gives it: `(unknown)` is none.
fn operator(r: &Row, c: &str) -> Option<String> {
    text(r, c).filter(|v| v != "(unknown)")
}

// ---------------------------------------------------------------------------
// msdb's encodings
// ---------------------------------------------------------------------------

/// `yyyymmdd` and `hhmmss` integers (`run_date` / `run_time`, server local
/// time). A zero date is "never".
fn agent_time(date: i64, time: i64) -> Option<NaiveDateTime> {
    if date <= 0 || time < 0 {
        return None;
    }
    let d = NaiveDate::from_ymd_opt(
        i32::try_from(date / 10000).ok()?,
        u32::try_from(date / 100 % 100).ok()?,
        u32::try_from(date % 100).ok()?,
    )?;
    let t = NaiveTime::from_hms_opt(
        u32::try_from(time / 10000).ok()?,
        u32::try_from(time / 100 % 100).ok()?,
        u32::try_from(time % 100).ok()?,
    )?;
    Some(d.and_time(t))
}

fn agent_time_of(r: &Row, date: &str, time: &str) -> Option<NaiveDateTime> {
    agent_time(int(r, date)?, int(r, time).unwrap_or(0))
}

/// `hhmmss` (`run_duration`; the hours go past 99) in ms.
fn agent_duration(v: i64) -> Option<u64> {
    if v < 0 {
        return None;
    }
    let secs = (v / 10000) * 3600 + (v / 100 % 100) * 60 + v % 100;
    u64::try_from(secs).ok().map(|s| s * 1000)
}

/// `run_status` / `last_run_outcome`.
fn outcome(code: i64) -> JobOutcome {
    match code {
        0 => JobOutcome::Failed,
        1 => JobOutcome::Succeeded,
        2 => JobOutcome::Retrying,
        3 => JobOutcome::Canceled,
        4 => JobOutcome::Running,
        _ => JobOutcome::Unknown,
    }
}

/// `current_execution_status`: executing, waiting for a thread, between
/// retries, performing completion actions.
fn is_running(status: i64) -> bool {
    matches!(status, 1 | 2 | 3 | 7)
}

// ---------------------------------------------------------------------------
// Server local time to UTC
// ---------------------------------------------------------------------------

/// Turns the server's local times into UTC: by the server's current offset,
/// or, where the server names its time zone, by the offset each hour had.
#[derive(Debug, Clone, Default)]
struct Clock {
    /// Local minus UTC, now.
    offset_min: i64,
    /// The server's time zone (`CURRENT_TIMEZONE_ID()`), when it isn't UTC.
    zone: Option<String>,
    /// Local hour → minutes to add to get UTC, as the server answered.
    add: HashMap<NaiveDateTime, i64>,
}

fn hour(t: NaiveDateTime) -> NaiveDateTime {
    t.date().and_hms_opt(t.hour(), 0, 0).unwrap_or(t)
}

impl Clock {
    fn new(p: &Probe) -> Self {
        Clock {
            offset_min: p.offset_min,
            zone: p.zone.clone().filter(|z| !z.eq_ignore_ascii_case("UTC")),
            add: HashMap::new(),
        }
    }

    fn to_utc(&self, local: NaiveDateTime) -> NaiveDateTime {
        let add = self
            .add
            .get(&hour(local))
            .copied()
            .unwrap_or(-self.offset_min);
        local + Duration::minutes(add)
    }

    /// RFC 3339 in UTC, whole seconds.
    fn utc(&self, local: NaiveDateTime) -> String {
        self.to_utc(local).format("%Y-%m-%dT%H:%M:%SZ").to_string()
    }

    /// Ask the server the offset of each hour these times fall in (one
    /// query per 500 hours). On any failure the current offset stays.
    async fn learn(
        &mut self,
        s: &mut SqlServerSession,
        locals: impl IntoIterator<Item = NaiveDateTime>,
    ) {
        let Some(zone) = self.zone.clone() else {
            return;
        };
        let hours: Vec<NaiveDateTime> = locals
            .into_iter()
            .map(hour)
            .filter(|h| !self.add.contains_key(h))
            .collect::<BTreeSet<_>>()
            .into_iter()
            .collect();
        for chunk in hours.chunks(500) {
            match s.rows(&zone_sql(chunk), &[zone.as_str()]).await {
                Ok(rows) => {
                    for r in rows {
                        if let (Some(v), Some(m)) = (datetime(&r, "v"), int(&r, "add_min")) {
                            self.add.insert(v, m);
                        }
                    }
                }
                Err(e) => {
                    tracing::debug!("sqlserver jobs: time zone {zone}: {e}");
                    self.zone = None;
                    return;
                }
            }
        }
    }
}

/// The minutes to add to each local hour to get UTC, in the zone `@P1`.
/// The values are formatted dates (digits, `-`, `:` and `T`), never text
/// from elsewhere.
fn zone_sql(hours: &[NaiveDateTime]) -> String {
    let values: Vec<String> = hours
        .iter()
        .map(|h| {
            format!(
                "(CAST('{}' AS datetime2(0)))",
                h.format("%Y-%m-%dT%H:%M:%S")
            )
        })
        .collect();
    format!(
        "SELECT v, DATEDIFF(MINUTE, v, CAST(SWITCHOFFSET(v AT TIME ZONE @P1, '+00:00') AS datetime2(0))) AS add_min
           FROM (VALUES {}) AS t(v)",
        values.join(", ")
    )
}

// ---------------------------------------------------------------------------
// What the server and the login allow
// ---------------------------------------------------------------------------

/// The engine, first and alone (the query `profiler.rs` asks too): Azure
/// SQL Database, Synapse's pools, SQL database in Fabric and Babelfish
/// (which a `sqlserver` connection may reach, with an msdb but no Agent)
/// don't get the rest of the probe, which reads msdb and
/// `sys.configurations`.
const ENGINE_SQL: &str = "SELECT CAST(SERVERPROPERTY('EngineEdition') AS int) AS edition, CAST(SERVERPROPERTY('Babelfish') AS int) AS babelfish";

/// `EngineEdition`s without the Agent: Azure SQL Database, Synapse's
/// dedicated and serverless pools, SQL database in Fabric.
fn edition_without_agent(edition: i64) -> bool {
    matches!(edition, 5 | 6 | 11 | 12)
}

/// One round trip: the edition and version, the login, whether the Agent
/// runs, the offset, the server's name, msdb access, and (by dynamic SQL,
/// so 2005–2019 still compile the batch) the time zone and the Agent roles.
///
/// Whether the Agent runs: 'Agent XPs' is on while it runs, but stays on
/// after it stops uncleanly (or when someone turns it on by hand), so with
/// VIEW SERVER STATE (sysadmin has it) the Agent's own sessions decide.
const PROBE_SQL: &str = "SET NOCOUNT ON;
DECLARE @edition int, @major int, @agent int, @msdb_exec nvarchar(100), @roles nvarchar(400);
SET @edition = CAST(SERVERPROPERTY('EngineEdition') AS int);
SET @major = CAST(PARSENAME(CAST(SERVERPROPERTY('ProductVersion') AS nvarchar(32)), 4) AS int);
SELECT @agent = CAST(value_in_use AS int) FROM sys.configurations WHERE name = N'Agent XPs';
IF @agent = 1 AND HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER STATE') = 1
   AND NOT EXISTS (SELECT 1 FROM sys.dm_exec_sessions WHERE program_name LIKE N'SQLAgent - %')
    SET @agent = 0;
SELECT @edition AS edition, @major AS major,
       CAST(IS_SRVROLEMEMBER('sysadmin') AS int) AS sysadmin,
       @agent AS agent_running,
       DATEDIFF(SECOND, GETUTCDATE(), GETDATE()) AS offset_seconds,
       SUSER_SNAME() AS login_name,
       UPPER(CAST(SERVERPROPERTY('ServerName') AS nvarchar(128))) AS server_name,
       CAST(COALESCE(HAS_PERMS_BY_NAME(N'msdb.dbo.sp_help_job', N'OBJECT', N'EXECUTE'), 0) AS int)
         * CAST(COALESCE(HAS_PERMS_BY_NAME(N'msdb.dbo.sysjobs_view', N'OBJECT', N'SELECT'), 0) AS int) AS agent_access;
IF @edition = 8 OR @major >= 16
    EXEC sys.sp_executesql N'SELECT CAST(CURRENT_TIMEZONE_ID() AS nvarchar(128)) AS dbine_zone';
IF COALESCE(IS_SRVROLEMEMBER('sysadmin'), 0) = 0
   AND HAS_PERMS_BY_NAME(N'msdb.dbo.sp_help_job', N'OBJECT', N'EXECUTE') = 1
BEGIN
    SET @msdb_exec = N'msdb.sys.sp_executesql';
    SET @roles = N'SELECT CAST(IS_MEMBER(N''SQLAgentReaderRole'') AS int) AS dbine_reader, CAST(IS_MEMBER(N''SQLAgentOperatorRole'') AS int) AS dbine_operator';
    EXEC @msdb_exec @roles;
END;";

#[derive(Debug, Clone, Default)]
struct Probe {
    edition: i64,
    /// Babelfish for PostgreSQL, reached as SQL Server.
    babelfish: bool,
    major: i64,
    sysadmin: bool,
    /// The Agent service runs (see [`PROBE_SQL`]).
    agent_running: bool,
    offset_min: i64,
    login: String,
    /// `SERVERPROPERTY('ServerName')`, uppercase: what msdb names the jobs
    /// made here as their originating server.
    server: String,
    access: bool,
    zone: Option<String>,
    reader: bool,
    operator: bool,
}

impl Probe {
    fn read(sets: &[ResultSet]) -> Probe {
        let mut p = Probe::default();
        if let Some(r) = first_with(sets, "agent_access").and_then(|s| s.rows.first()) {
            p.edition = int(r, "edition").unwrap_or(0);
            p.major = int(r, "major").unwrap_or(0);
            p.sysadmin = int(r, "sysadmin") == Some(1);
            p.agent_running = int(r, "agent_running") == Some(1);
            // Real offsets are whole quarters of an hour; the two clocks are
            // read a moment apart.
            let secs = int(r, "offset_seconds").unwrap_or(0);
            p.offset_min = ((secs as f64) / 900.0).round() as i64 * 15;
            p.login = text(r, "login_name").unwrap_or_default();
            p.server = text(r, "server_name").unwrap_or_default();
            p.access = int(r, "agent_access") == Some(1);
        }
        p.zone = first_with(sets, "dbine_zone")
            .and_then(|s| s.rows.first())
            .and_then(|r| text(r, "dbine_zone"));
        if let Some(r) = first_with(sets, "dbine_reader").and_then(|s| s.rows.first()) {
            p.reader = int(r, "dbine_reader") == Some(1);
            p.operator = int(r, "dbine_operator") == Some(1);
        }
        p
    }

    /// The server has no Agent at all.
    fn no_agent(&self) -> bool {
        self.babelfish || edition_without_agent(self.edition)
    }

    /// Why there are no Agent jobs to show here, when there are none.
    fn missing(&self) -> Option<&'static str> {
        match self.edition {
            _ if self.babelfish => Some(NO_AGENT_NOTE),
            5 => Some(AZURE_SQL_NOTE),
            e if edition_without_agent(e) => Some(NO_AGENT_NOTE),
            _ if !self.access => Some(NO_ACCESS_NOTE),
            _ => None,
        }
    }

    /// Jobs start here: the Agent runs (Express has none, whatever 'Agent
    /// XPs' says).
    fn agent_runs(&self) -> bool {
        self.agent_running && self.edition != 4
    }

    /// What the list should say about itself.
    fn notes(&self) -> Vec<String> {
        let mut n = Vec::new();
        if self.edition == 4 {
            n.push(EXPRESS_NOTE.to_string());
        } else if !self.agent_running {
            n.push(AGENT_STOPPED_NOTE.to_string());
        }
        if !self.sysadmin && !self.reader && !self.operator {
            n.push(OWN_JOBS_NOTE.to_string());
        }
        n
    }

    /// The login may start, stop and enable this job: sysadmin and
    /// SQLAgentOperatorRole any of them, the rest the ones they own.
    fn manages(&self, owner: Option<&str>) -> bool {
        self.sysadmin
            || self.operator
            || owner.is_some_and(|o| !self.login.is_empty() && o.eq_ignore_ascii_case(&self.login))
    }

    /// The job came from a master server (this one is its target): msdb
    /// refuses to change it here (`sp_update_job`, error 14274).
    fn sent_by_master(&self, job: &JobRow) -> bool {
        !self.server.is_empty()
            && job
                .origin
                .as_deref()
                .is_some_and(|o| !o.eq_ignore_ascii_case(&self.server))
    }
}

async fn probe(s: &mut SqlServerSession) -> Result<Probe> {
    let engine = sets(s, ENGINE_SQL, &[], None).await?;
    let row = engine.first().and_then(|s| s.rows.first());
    let edition = row.and_then(|r| int(r, "edition")).unwrap_or(0);
    let babelfish = row.and_then(|r| int(r, "babelfish")) == Some(1);
    if babelfish || edition_without_agent(edition) {
        return Ok(Probe {
            edition,
            babelfish,
            ..Probe::default()
        });
    }
    Ok(Probe::read(&sets(s, PROBE_SQL, &[], None).await?))
}

/// The probe, refusing a server without Agent jobs to read.
async fn probe_for_job(s: &mut SqlServerSession) -> Result<Probe> {
    let p = probe(s).await?;
    match p.missing() {
        Some(note) if p.no_agent() => Err(Error::Unsupported(note.into())),
        Some(note) => Err(Error::Query(note.into())),
        None => Ok(p),
    }
}

// ---------------------------------------------------------------------------
// Rows
// ---------------------------------------------------------------------------

/// A job as `sp_help_job` describes it.
#[derive(Debug, Clone, Default)]
struct JobRow {
    id: Uuid,
    name: String,
    enabled: bool,
    description: Option<String>,
    start_step: i64,
    category: Option<String>,
    owner: Option<String>,
    origin: Option<String>,
    notify_eventlog: i64,
    notify_email: i64,
    notify_netsend: i64,
    notify_page: i64,
    email_operator: Option<String>,
    netsend_operator: Option<String>,
    page_operator: Option<String>,
    delete_level: i64,
    created: Option<NaiveDateTime>,
    modified: Option<NaiveDateTime>,
    version: Option<i64>,
    /// The last finished run (sysjobservers), local time.
    last_run: Option<NaiveDateTime>,
    last_outcome: i64,
    next_run: Option<NaiveDateTime>,
    status: i64,
    current_step: Option<String>,
    steps: i64,
    targets: i64,
    /// 1 local, 2 multiserver.
    kind: i64,
}

impl JobRow {
    fn read(r: &Row) -> Option<JobRow> {
        Some(JobRow {
            id: guid(r, "job_id")?,
            name: text(r, "name")?,
            enabled: int(r, "enabled") == Some(1),
            description: text(r, "description").filter(|d| d != NO_DESCRIPTION),
            start_step: int(r, "start_step_id").unwrap_or(1),
            category: text(r, "category"),
            owner: text(r, "owner"),
            origin: text(r, "originating_server"),
            notify_eventlog: int(r, "notify_level_eventlog").unwrap_or(0),
            notify_email: int(r, "notify_level_email").unwrap_or(0),
            notify_netsend: int(r, "notify_level_netsend").unwrap_or(0),
            notify_page: int(r, "notify_level_page").unwrap_or(0),
            email_operator: operator(r, "notify_email_operator"),
            netsend_operator: operator(r, "notify_netsend_operator"),
            page_operator: operator(r, "notify_page_operator"),
            delete_level: int(r, "delete_level").unwrap_or(0),
            created: datetime(r, "date_created"),
            modified: datetime(r, "date_modified"),
            version: int(r, "version_number"),
            last_run: agent_time_of(r, "last_run_date", "last_run_time"),
            last_outcome: int(r, "last_run_outcome").unwrap_or(5),
            next_run: agent_time_of(r, "next_run_date", "next_run_time"),
            status: int(r, "current_execution_status").unwrap_or(4),
            current_step: text(r, "current_execution_step").filter(|v| !v.starts_with("0 ")),
            steps: int(r, "has_step").unwrap_or(0),
            targets: int(r, "has_target").unwrap_or(0),
            kind: int(r, "type").unwrap_or(1),
        })
    }
}

/// A job's row in `sp_help_jobactivity` (the Agent's current session).
#[derive(Debug, Clone, Default)]
struct ActivityRow {
    job: Uuid,
    /// `run_requested_source`: 1 schedule, 2 alert, 3 startup, 4 user, 6
    /// CPU idle.
    source: Option<i64>,
    started: Option<NaiveDateTime>,
    stopped: Option<NaiveDateTime>,
    next: Option<NaiveDateTime>,
    /// The history row (`instance_id`) of its last finished run.
    history_id: Option<i64>,
}

impl ActivityRow {
    fn read(r: &Row) -> Option<ActivityRow> {
        Some(ActivityRow {
            job: guid(r, "job_id")?,
            source: int(r, "run_requested_source"),
            started: datetime(r, "start_execution_date"),
            stopped: datetime(r, "stop_execution_date"),
            next: datetime(r, "next_scheduled_run_date"),
            history_id: int(r, "job_history_id"),
        })
    }

    /// The start of the run going on now.
    fn current(&self) -> Option<NaiveDateTime> {
        self.started.filter(|_| self.stopped.is_none())
    }
}

/// A schedule as `sp_help_jobschedule` gives it.
#[derive(Debug, Clone, Default)]
struct ScheduleRow {
    name: String,
    enabled: bool,
    freq_type: i64,
    freq_interval: i64,
    subday_type: i64,
    subday_interval: i64,
    relative_interval: i64,
    recurrence_factor: i64,
    start_date: i64,
    end_date: i64,
    start_time: i64,
    end_time: i64,
    uid: Option<String>,
    /// Jobs that share it.
    job_count: i64,
}

impl ScheduleRow {
    fn read(r: &Row) -> Option<ScheduleRow> {
        Some(ScheduleRow {
            name: text(r, "schedule_name")?,
            enabled: int(r, "enabled") == Some(1),
            freq_type: int(r, "freq_type").unwrap_or(0),
            freq_interval: int(r, "freq_interval").unwrap_or(0),
            subday_type: int(r, "freq_subday_type").unwrap_or(0),
            subday_interval: int(r, "freq_subday_interval").unwrap_or(0),
            relative_interval: int(r, "freq_relative_interval").unwrap_or(0),
            recurrence_factor: int(r, "freq_recurrence_factor").unwrap_or(0),
            start_date: int(r, "active_start_date").unwrap_or(0),
            end_date: int(r, "active_end_date").unwrap_or(99991231),
            start_time: int(r, "active_start_time").unwrap_or(0),
            end_time: int(r, "active_end_time").unwrap_or(235959),
            uid: guid(r, "schedule_uid").map(|u| u.to_string()),
            job_count: int(r, "job_count").unwrap_or(1),
        })
    }
}

/// A target server of a job (`sp_help_jobserver`, last run details).
#[derive(Debug, Clone, Default)]
struct ServerRow {
    /// 0: this server.
    id: i64,
    name: String,
    duration_ms: Option<u64>,
    message: Option<String>,
}

impl ServerRow {
    fn read(r: &Row) -> Option<ServerRow> {
        Some(ServerRow {
            id: int(r, "server_id")?,
            name: text(r, "server_name").unwrap_or_default(),
            duration_ms: int(r, "last_run_duration").and_then(agent_duration),
            message: text(r, "last_outcome_message"),
        })
    }
}

/// A step as `sp_help_jobstep` gives it.
#[derive(Debug, Clone, Default)]
struct StepRow {
    id: i64,
    name: String,
    subsystem: String,
    command: Option<String>,
    flags: i64,
    success_code: i64,
    on_success: i64,
    on_success_step: i64,
    on_fail: i64,
    on_fail_step: i64,
    server: Option<String>,
    database: Option<String>,
    database_user: Option<String>,
    retry_attempts: i64,
    retry_interval: i64,
    os_priority: i64,
    output_file: Option<String>,
    proxy_id: Option<i64>,
}

impl StepRow {
    fn read(r: &Row) -> Option<StepRow> {
        Some(StepRow {
            id: int(r, "step_id")?,
            name: text(r, "step_name").unwrap_or_default(),
            subsystem: text(r, "subsystem").unwrap_or_else(|| "TSQL".into()),
            // The command as it is, spaces and all.
            command: r
                .try_get::<&str, _>("command")
                .ok()
                .flatten()
                .map(str::to_string),
            flags: int(r, "flags").unwrap_or(0),
            success_code: int(r, "cmdexec_success_code").unwrap_or(0),
            on_success: int(r, "on_success_action").unwrap_or(1),
            on_success_step: int(r, "on_success_step_id").unwrap_or(0),
            on_fail: int(r, "on_fail_action").unwrap_or(2),
            on_fail_step: int(r, "on_fail_step_id").unwrap_or(0),
            server: text(r, "server"),
            database: text(r, "database_name"),
            database_user: text(r, "database_user_name"),
            retry_attempts: int(r, "retry_attempts").unwrap_or(0),
            retry_interval: int(r, "retry_interval").unwrap_or(0),
            os_priority: int(r, "os_run_priority").unwrap_or(0),
            output_file: text(r, "output_file_name"),
            proxy_id: int(r, "proxy_id").filter(|p| *p > 0),
        })
    }
}

/// One row of `sp_help_jobhistory @mode = 'FULL'`: a step's result, or the
/// job's (step 0).
#[derive(Debug, Clone, Default, PartialEq)]
struct HistoryRow {
    instance: i64,
    step_id: i64,
    step_name: String,
    message: Option<String>,
    status: i64,
    started: Option<NaiveDateTime>,
    duration_ms: Option<u64>,
}

impl HistoryRow {
    fn read(r: &Row) -> Option<HistoryRow> {
        Some(HistoryRow {
            instance: int(r, "instance_id")?,
            step_id: int(r, "step_id").unwrap_or(0),
            step_name: text(r, "step_name").unwrap_or_default(),
            message: text(r, "message"),
            status: int(r, "run_status").unwrap_or(5),
            started: agent_time_of(r, "run_date", "run_time"),
            duration_ms: int(r, "run_duration").and_then(agent_duration),
        })
    }
}

/// A job with what the list shows of it.
#[derive(Debug, Clone, Default)]
struct Raw {
    job: JobRow,
    schedules: Vec<ScheduleRow>,
    servers: Vec<ServerRow>,
    activity: Option<ActivityRow>,
}

impl Raw {
    /// Every local time it carries, for the clock.
    fn times(&self) -> Vec<NaiveDateTime> {
        let j = &self.job;
        let a = self.activity.as_ref();
        [
            j.last_run,
            j.next_run,
            j.created,
            j.modified,
            a.and_then(|a| a.started),
            a.and_then(|a| a.next),
        ]
        .into_iter()
        .flatten()
        .collect()
    }
}

fn read_all<T>(set: Option<&ResultSet>, read: fn(&Row) -> Option<T>) -> Vec<T> {
    set.map(|s| s.rows.iter().filter_map(read).collect())
        .unwrap_or_default()
}

fn activities(sets: &[ResultSet]) -> HashMap<Uuid, ActivityRow> {
    read_all(first_with(sets, "start_execution_date"), ActivityRow::read)
        .into_iter()
        .map(|a| (a.job, a))
        .collect()
}

// ---------------------------------------------------------------------------
// The list
// ---------------------------------------------------------------------------

/// Every job the login sees, then, per job (each after a `dbine_job` marker
/// row), its schedules and its servers with their last run.
fn list_sql(major: i64) -> String {
    let activity = if major >= 9 {
        "EXEC msdb.dbo.sp_help_jobactivity;\n"
    } else {
        ""
    };
    format!(
        "SET NOCOUNT ON;
EXEC msdb.dbo.sp_help_job;
{activity}DECLARE @dbine_job uniqueidentifier;
DECLARE dbine_jobs CURSOR LOCAL FAST_FORWARD FOR SELECT job_id FROM msdb.dbo.sysjobs_view;
OPEN dbine_jobs;
FETCH NEXT FROM dbine_jobs INTO @dbine_job;
WHILE @@FETCH_STATUS = 0
BEGIN
    SELECT @dbine_job AS dbine_job;
    EXEC msdb.dbo.sp_help_jobschedule @job_id = @dbine_job;
    EXEC msdb.dbo.sp_help_jobserver @job_id = @dbine_job, @show_last_run_details = 1;
    FETCH NEXT FROM dbine_jobs INTO @dbine_job;
END;
CLOSE dbine_jobs;
DEALLOCATE dbine_jobs;"
    )
}

fn read_list(sets: &[ResultSet]) -> Vec<Raw> {
    let mut per: HashMap<Uuid, (Vec<ScheduleRow>, Vec<ServerRow>)> = HashMap::new();
    let mut current: Option<Uuid> = None;
    for set in sets {
        if set.has("dbine_job") {
            current = set.rows.first().and_then(|r| guid(r, "dbine_job"));
        } else if let Some(id) = current {
            let entry = per.entry(id).or_default();
            if set.has("freq_type") {
                entry
                    .0
                    .extend(set.rows.iter().filter_map(ScheduleRow::read));
            } else if set.has("enlist_date") {
                entry.1.extend(set.rows.iter().filter_map(ServerRow::read));
            }
        }
    }
    let mut activity = activities(sets);
    let mut out: Vec<Raw> = read_all(first_with(sets, "current_execution_status"), JobRow::read)
        .into_iter()
        .map(|job| {
            let (schedules, servers) = per.remove(&job.id).unwrap_or_default();
            let activity = activity.remove(&job.id);
            Raw {
                job,
                schedules,
                servers,
                activity,
            }
        })
        .collect();
    out.sort_by(|a, b| {
        a.job
            .name
            .to_lowercase()
            .cmp(&b.job.name.to_lowercase())
            .then_with(|| a.job.name.cmp(&b.job.name))
    });
    out
}

pub(crate) async fn list(s: &mut SqlServerSession) -> Result<JobList> {
    check_variant(s)?;
    let p = probe(s).await?;
    if let Some(note) = p.missing() {
        return Ok(JobList {
            jobs: Vec::new(),
            notes: vec![note.to_string()],
        });
    }
    let raw = read_list(&sets(s, &list_sql(p.major), &[], None).await?);
    let mut clock = Clock::new(&p);
    clock.learn(s, raw.iter().flat_map(Raw::times)).await;
    Ok(JobList {
        jobs: raw.iter().map(|r| server_job(r, &p, &clock)).collect(),
        notes: p.notes(),
    })
}

/// A job as the list shows it, with what the login may do with it.
fn server_job(raw: &Raw, p: &Probe, clock: &Clock) -> ServerJob {
    let j = &raw.job;
    let running = is_running(j.status);
    let here = raw
        .servers
        .iter()
        .find(|s| s.id == 0)
        .or(raw.servers.first());
    let current = raw.activity.as_ref().and_then(ActivityRow::current);
    let (last_outcome, last_started, last_duration_ms) = match j.last_run {
        Some(t) => (
            Some(outcome(j.last_outcome)),
            Some(clock.utc(t)),
            here.and_then(|s| s.duration_ms),
        ),
        None if running => (
            Some(JobOutcome::Running),
            current.map(|t| clock.utc(t)),
            None,
        ),
        None => (None, None, None),
    };
    let scheduled = raw.schedules.iter().any(|s| s.enabled);
    // Without a running Agent nothing starts on time, and msdb's next run
    // is the one it worked out before it stopped.
    let next_run = if j.enabled && scheduled && p.agent_runs() {
        j.next_run
            .or_else(|| raw.activity.as_ref().and_then(|a| a.next))
            .map(|t| clock.utc(t))
    } else {
        None
    };
    let manage = p.manages(j.owner.as_deref());
    // `sp_start_job` runs a job that targets this server (also one a master
    // sent) for its owner, SQLAgentOperatorRole and sysadmin; one that only
    // targets other servers (a multiserver job on its master) it posts to
    // them, for sysadmin only.
    let here_too = if raw.servers.is_empty() {
        j.kind != 2
    } else {
        raw.servers.iter().any(|s| s.id == 0)
    };
    let start = if here_too { manage } else { p.sysadmin };
    ServerJob {
        id: j.id.to_string(),
        name: j.name.clone(),
        kind: KIND.into(),
        group: j.category.clone(),
        description: j.description.clone(),
        owner: j.owner.clone(),
        enabled: j.enabled,
        schedule: schedule_text(&raw.schedules),
        running,
        last_outcome,
        last_started,
        last_duration_ms,
        next_run,
        can_run: start && p.agent_runs() && !running && j.steps > 0 && j.targets > 0,
        can_stop: start && p.agent_runs() && running,
        can_enable: manage && !p.sent_by_master(j),
        has_history: true,
        has_script: true,
    }
}

// ---------------------------------------------------------------------------
// Schedules in words
// ---------------------------------------------------------------------------

const WEEKDAYS: [&str; 7] = [
    "domingo",
    "lunes",
    "martes",
    "miércoles",
    "jueves",
    "viernes",
    "sábado",
];

/// `hhmmss` as `HH:MM`, or `HH:MM:SS` when it has seconds.
fn time_of_day(t: i64) -> String {
    let (h, m, s) = (t / 10000, t / 100 % 100, t % 100);
    if s == 0 {
        format!("{h:02}:{m:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}")
    }
}

/// `yyyymmdd` as `yyyy-mm-dd`.
fn day(d: i64) -> String {
    format!("{:04}-{:02}-{:02}", d / 10000, d / 100 % 100, d % 100)
}

fn enumerate(items: &[&str]) -> String {
    match items {
        [] => String::new(),
        [one] => (*one).to_string(),
        [rest @ .., last] => format!("{} y {last}", rest.join(", ")),
    }
}

/// A schedule as SSMS's dialog states it, in Spanish, with the server's
/// local times.
fn describe(s: &ScheduleRow) -> String {
    let every = |n: i64, one: &str, many: &str| {
        if n <= 1 {
            one.to_string()
        } else {
            format!("Cada {n} {many}")
        }
    };
    let mut d = match s.freq_type {
        1 => format!(
            "Una vez, el {} a las {}",
            day(s.start_date),
            time_of_day(s.start_time)
        ),
        4 => every(s.freq_interval, "Todos los días", "días"),
        8 => {
            let days: Vec<&str> = (0..7)
                .filter(|i| s.freq_interval & (1 << i) != 0)
                .map(|i| WEEKDAYS[i])
                .collect();
            let base = every(s.recurrence_factor, "Todas las semanas", "semanas");
            if days.is_empty() {
                base
            } else {
                format!("{base}: {}", enumerate(&days))
            }
        }
        16 => {
            if s.recurrence_factor <= 1 {
                format!("El día {} de cada mes", s.freq_interval)
            } else {
                format!(
                    "El día {}, cada {} meses",
                    s.freq_interval, s.recurrence_factor
                )
            }
        }
        32 => {
            let which = match s.relative_interval {
                1 => "primer",
                2 => "segundo",
                4 => "tercer",
                8 => "cuarto",
                16 => "último",
                _ => "",
            };
            let what = match s.freq_interval {
                1..=7 => WEEKDAYS[(s.freq_interval - 1) as usize],
                8 => "día",
                9 => "día hábil",
                10 => "día de fin de semana",
                _ => "día",
            };
            if s.recurrence_factor <= 1 {
                format!("El {which} {what} de cada mes")
            } else {
                format!("El {which} {what}, cada {} meses", s.recurrence_factor)
            }
        }
        64 => "Al iniciar el Agente SQL Server".into(),
        128 => "Cuando la CPU está inactiva".into(),
        other => format!("Frecuencia {other}"),
    };
    if matches!(s.freq_type, 4 | 8 | 16 | 32) {
        let unit = match s.subday_type {
            2 => Some(("segundo", "segundos")),
            4 => Some(("minuto", "minutos")),
            8 => Some(("hora", "horas")),
            _ => None,
        };
        match unit {
            Some((one, many)) => {
                let n = s.subday_interval;
                let each = if n <= 1 {
                    format!("cada {one}")
                } else {
                    format!("cada {n} {many}")
                };
                let _ = write!(
                    d,
                    ", {each} entre las {} y las {}",
                    time_of_day(s.start_time),
                    time_of_day(s.end_time)
                );
            }
            None => {
                let _ = write!(d, ", a las {}", time_of_day(s.start_time));
            }
        }
        if s.end_date > 0 && s.end_date < 99991231 {
            let _ = write!(d, ", hasta el {}", day(s.end_date));
        }
    }
    if !s.enabled {
        d.push_str(" (deshabilitada)");
    }
    d
}

/// The list's schedule: its enabled schedules (or, when none is, all of
/// them, marked disabled). `None`: it only runs on demand.
fn schedule_text(schedules: &[ScheduleRow]) -> Option<String> {
    let enabled: Vec<String> = schedules
        .iter()
        .filter(|s| s.enabled)
        .map(describe)
        .collect();
    let all = if enabled.is_empty() {
        schedules.iter().map(describe).collect()
    } else {
        enabled
    };
    (!all.is_empty()).then(|| all.join("; "))
}

// ---------------------------------------------------------------------------
// Detail
// ---------------------------------------------------------------------------

fn activity_sql(major: i64) -> &'static str {
    if major >= 9 {
        "EXEC msdb.dbo.sp_help_jobactivity @job_id = @P1;\n"
    } else {
        ""
    }
}

/// One job, `@P1` its id.
fn detail_sql(major: i64) -> String {
    format!(
        "SET NOCOUNT ON;
EXEC msdb.dbo.sp_help_job @job_id = @P1, @job_aspect = N'JOB';
EXEC msdb.dbo.sp_help_jobstep @job_id = @P1;
EXEC msdb.dbo.sp_help_jobschedule @job_id = @P1;
EXEC msdb.dbo.sp_help_jobserver @job_id = @P1, @show_last_run_details = 1;
{}",
        activity_sql(major)
    )
}

/// The job and its steps, read whole.
async fn read_job(s: &mut SqlServerSession, id: &str) -> Result<(Probe, Raw, Vec<StepRow>, Clock)> {
    check_variant(s)?;
    let id = job_id(id)?;
    let p = probe_for_job(s).await?;
    let sets = sets(s, &detail_sql(p.major), &[&id], None).await?;
    let Some(job) = first_with(&sets, "current_execution_status")
        .and_then(|s| s.rows.first())
        .and_then(JobRow::read)
    else {
        return Err(Error::Query(NOT_FOUND.into()));
    };
    let mut steps = read_all(first_with(&sets, "subsystem"), StepRow::read);
    steps.sort_by_key(|s| s.id);
    let raw = Raw {
        schedules: read_all(first_with(&sets, "freq_type"), ScheduleRow::read),
        servers: read_all(first_with(&sets, "enlist_date"), ServerRow::read),
        activity: activities(&sets).remove(&job.id),
        job,
    };
    let mut clock = Clock::new(&p);
    clock.learn(s, raw.times()).await;
    Ok((p, raw, steps, clock))
}

pub(crate) async fn detail(s: &mut SqlServerSession, id: &str) -> Result<JobDetail> {
    let (p, raw, steps, clock) = read_job(s, id).await?;
    Ok(JobDetail {
        job: server_job(&raw, &p, &clock),
        steps: steps.iter().map(job_step).collect(),
        schedules: raw
            .schedules
            .iter()
            .map(|s| format!("{}: {}", s.name, describe(s)))
            .collect(),
        properties: properties(&raw, &steps, &clock),
    })
}

/// What runs a step, in words.
fn subsystem(name: &str) -> String {
    match name.to_ascii_uppercase().as_str() {
        "TSQL" => "T-SQL".into(),
        "CMDEXEC" => "CmdExec".into(),
        "POWERSHELL" => "PowerShell".into(),
        "SSIS" => "SSIS".into(),
        "ANALYSISQUERY" => "Analysis Services (consulta)".into(),
        "ANALYSISCOMMAND" => "Analysis Services (comando)".into(),
        "SNAPSHOT" => "Replicación (instantánea)".into(),
        "LOGREADER" => "Replicación (lector del registro)".into(),
        "DISTRIBUTION" => "Replicación (distribución)".into(),
        "MERGE" => "Replicación (mezcla)".into(),
        "QUEUEREADER" => "Replicación (lector de colas)".into(),
        "ACTIVESCRIPTING" => "ActiveX Script".into(),
        _ => name.to_string(),
    }
}

fn job_step(s: &StepRow) -> JobStep {
    JobStep {
        name: s.name.clone(),
        kind: Some(subsystem(&s.subsystem)),
        target: s.database.clone().or_else(|| s.server.clone()),
        command: s.command.clone(),
    }
}

/// `notify_level_*` / `delete_level`.
fn level_text(level: i64) -> &'static str {
    match level {
        1 => "Si se completa correctamente",
        2 => "Si falla",
        _ => "Siempre",
    }
}

fn status_text(status: i64) -> &'static str {
    match status {
        1 => "En ejecución",
        2 => "Esperando un hilo",
        3 => "Esperando para reintentar",
        4 => "Inactivo",
        5 => "Suspendido",
        7 => "Completando acciones",
        _ => "Desconocido",
    }
}

/// What a step does when it ends.
fn step_flow(s: &StepRow) -> String {
    let action = |a: i64, step: i64| match a {
        1 => "salir informando éxito".to_string(),
        2 => "salir informando error".to_string(),
        3 => "ir al paso siguiente".to_string(),
        4 => format!("ir al paso {step}"),
        other => format!("acción {other}"),
    };
    let mut v = format!(
        "Si se completa: {}; si falla: {}",
        action(s.on_success, s.on_success_step),
        action(s.on_fail, s.on_fail_step)
    );
    if s.retry_attempts > 0 {
        let _ = write!(
            v,
            "; reintentos: {} cada {} min",
            s.retry_attempts, s.retry_interval
        );
    }
    if let Some(u) = &s.database_user {
        let _ = write!(v, "; ejecutar como: {u}");
    }
    if let Some(f) = &s.output_file {
        let _ = write!(v, "; archivo de salida: {f}");
    }
    v
}

fn properties(raw: &Raw, steps: &[StepRow], clock: &Clock) -> Vec<PropertyInfo> {
    let j = &raw.job;
    let mut out = Vec::new();
    let mut add = |group: &str, label: &str, value: String| {
        out.push(PropertyInfo {
            group: group.into(),
            label: label.into(),
            value,
        })
    };
    if let Some(c) = &j.category {
        add("", "Categoría", c.clone());
    }
    add(
        "",
        "Tipo",
        if j.kind == 2 {
            "Multiservidor"
        } else {
            "Local"
        }
        .into(),
    );
    add("", "Estado", status_text(j.status).into());
    if is_running(j.status) {
        if let Some(step) = &j.current_step {
            add("", "Paso en curso", step.clone());
        }
    }
    if let Some(step) = steps.iter().find(|s| s.id == j.start_step) {
        add("", "Paso inicial", format!("{}. {}", step.id, step.name));
    }
    if let Some(origin) = &j.origin {
        add("", "Servidor de origen", origin.clone());
    }
    let targets: Vec<&str> = raw.servers.iter().map(|s| s.name.as_str()).collect();
    if !targets.is_empty() {
        add("", "Servidores de destino", targets.join(", "));
    }
    if let Some(m) = raw
        .servers
        .iter()
        .find(|s| s.id == 0)
        .or(raw.servers.first())
        .and_then(|s| s.message.clone())
    {
        add("", "Resultado de la última ejecución", m);
    }
    if let Some(t) = j.created {
        add("", "Creado", clock.utc(t));
    }
    if let Some(t) = j.modified {
        add("", "Modificado", clock.utc(t));
    }
    if let Some(v) = j.version {
        add("", "Versión", v.to_string());
    }
    let notify = [
        ("Registro de eventos", j.notify_eventlog, None),
        (
            "Correo electrónico",
            j.notify_email,
            j.email_operator.as_ref(),
        ),
        ("Localizador", j.notify_page, j.page_operator.as_ref()),
        ("Net send", j.notify_netsend, j.netsend_operator.as_ref()),
    ];
    for (label, level, op) in notify {
        if level > 0 {
            let when = level_text(level);
            add(
                "Notificaciones",
                label,
                match op {
                    Some(o) => format!("{when}: {o}"),
                    None => when.into(),
                },
            );
        }
    }
    if j.delete_level > 0 {
        add(
            "Notificaciones",
            "Eliminar el job",
            level_text(j.delete_level).into(),
        );
    }
    for s in steps {
        add("Pasos", &format!("{}. {}", s.id, s.name), step_flow(s));
    }
    out
}

// ---------------------------------------------------------------------------
// Runs
// ---------------------------------------------------------------------------

fn runs_sql(major: i64) -> String {
    format!(
        "SET NOCOUNT ON;
EXEC msdb.dbo.sp_help_job @job_id = @P1, @job_aspect = N'JOB';
{}EXEC msdb.dbo.sp_help_jobhistory @job_id = @P1, @oldest_first = 0, @mode = N'FULL';",
        activity_sql(major)
    )
}

/// A run before its times turn UTC.
#[derive(Debug, Clone, Default, PartialEq)]
struct RawRun {
    id: Option<i64>,
    started: Option<NaiveDateTime>,
    duration_ms: Option<u64>,
    outcome: JobOutcome,
    trigger: Option<&'static str>,
    message: Option<String>,
    steps: Vec<HistoryRow>,
}

/// What started a run, from `sp_help_jobactivity`.
fn source_trigger(source: i64) -> Option<&'static str> {
    match source {
        1 => Some("programada"),
        2 => Some("alerta"),
        3 => Some("al iniciar el Agente"),
        4 => Some("manual"),
        6 => Some("CPU inactiva"),
        _ => None,
    }
}

/// What started a run, from the Agent's message (`The Job was invoked by
/// Schedule 8 (nightly)`, `… by User sa`): servers installed in English.
fn message_trigger(message: &str) -> Option<&'static str> {
    let at = message.find("invoked by ")?;
    let by = &message[at + "invoked by ".len()..];
    if by.starts_with("Schedule") {
        Some("programada")
    } else if by.starts_with("User") {
        Some("manual")
    } else if by.starts_with("Alert") {
        Some("alerta")
    } else if by.starts_with("Start") || by.starts_with("Boot") {
        Some("al iniciar el Agente")
    } else if by.starts_with("Idle") {
        Some("CPU inactiva")
    } else {
        None
    }
}

/// History rows (any order) into runs, newest first: each job outcome row
/// (step 0) takes the step rows written before it; steps newer than every
/// outcome are the run going on (or one the Agent never closed).
fn group_runs(
    mut rows: Vec<HistoryRow>,
    running: bool,
    activity: Option<&ActivityRow>,
    limit: usize,
) -> Vec<RawRun> {
    rows.sort_by_key(|r| std::cmp::Reverse(r.instance));
    let mut runs: Vec<RawRun> = Vec::new();
    let mut loose: Vec<HistoryRow> = Vec::new();
    let mut current: Option<RawRun> = None;
    for r in rows {
        if r.step_id == 0 {
            if let Some(run) = current.take() {
                runs.push(run);
            }
            let trigger = activity
                .filter(|a| a.history_id == Some(r.instance))
                .and_then(|a| a.source)
                .and_then(source_trigger)
                .or_else(|| r.message.as_deref().and_then(message_trigger));
            current = Some(RawRun {
                id: Some(r.instance),
                started: r.started,
                duration_ms: r.duration_ms,
                outcome: outcome(r.status),
                trigger,
                message: r.message,
                steps: Vec::new(),
            });
        } else if let Some(run) = current.as_mut() {
            run.steps.push(r);
        } else {
            loose.push(r);
        }
    }
    if let Some(run) = current {
        runs.push(run);
    }
    if running || !loose.is_empty() {
        let now = activity.filter(|_| running);
        runs.insert(
            0,
            RawRun {
                id: None,
                started: now
                    .and_then(ActivityRow::current)
                    .or_else(|| loose.last().and_then(|s| s.started)),
                duration_ms: None,
                outcome: if running {
                    JobOutcome::Running
                } else {
                    JobOutcome::Unknown
                },
                trigger: now.and_then(|a| a.source).and_then(source_trigger),
                message: None,
                steps: loose,
            },
        );
    }
    runs.truncate(limit);
    for run in &mut runs {
        run.steps.reverse();
    }
    runs
}

fn job_run(r: RawRun, clock: &Clock) -> JobRun {
    JobRun {
        id: r.id.map(|i| i.to_string()),
        started: r.started.map(|t| clock.utc(t)),
        duration_ms: r.duration_ms,
        outcome: r.outcome,
        trigger: r.trigger.map(String::from),
        message: r.message,
        steps: r
            .steps
            .into_iter()
            .map(|s| JobStepRun {
                name: s.step_name,
                outcome: outcome(s.status),
                started: s.started.map(|t| clock.utc(t)),
                duration_ms: s.duration_ms,
                message: s.message,
            })
            .collect(),
    }
}

pub(crate) async fn runs(s: &mut SqlServerSession, id: &str, limit: u32) -> Result<Vec<JobRun>> {
    check_variant(s)?;
    let id = job_id(id)?;
    let limit = limit.min(MAX_JOB_RUNS) as usize;
    let p = probe_for_job(s).await?;
    let sets = sets(s, &runs_sql(p.major), &[&id], Some(limit)).await?;
    let Some(job) = first_with(&sets, "current_execution_status")
        .and_then(|s| s.rows.first())
        .and_then(JobRow::read)
    else {
        return Err(Error::Query(NOT_FOUND.into()));
    };
    let activity = activities(&sets).remove(&job.id);
    let rows = read_all(first_with(&sets, "instance_id"), HistoryRow::read);
    let runs = group_runs(rows, is_running(job.status), activity.as_ref(), limit);
    let times: Vec<NaiveDateTime> = runs
        .iter()
        .flat_map(|r| {
            r.started
                .into_iter()
                .chain(r.steps.iter().filter_map(|s| s.started))
        })
        .collect();
    let mut clock = Clock::new(&p);
    clock.learn(s, times).await;
    Ok(runs.into_iter().map(|r| job_run(r, &clock)).collect())
}

// ---------------------------------------------------------------------------
// Run, stop, enable
// ---------------------------------------------------------------------------

/// A procedure that changes msdb, outside any transaction of the session:
/// with one open it's refused (its locks on msdb would hold the Agent up),
/// and in manual-transaction mode it runs with implicit transactions off.
async fn act(s: &mut SqlServerSession, sql: &str, id: &str) -> Result<()> {
    check_variant(s)?;
    let id = job_id(id)?;
    let open = s
        .rows("SELECT CAST(@@TRANCOUNT AS int) AS n", &[])
        .await?
        .first()
        .and_then(|r| int(r, "n"))
        .unwrap_or(0);
    if open > 0 {
        return Err(Error::State(OPEN_TRANSACTION.into()));
    }
    let manual = !s.autocommit;
    if manual {
        s.batch("SET IMPLICIT_TRANSACTIONS OFF").await?;
    }
    let done = sets(s, sql, &[&id], None).await.map(|_| ());
    if manual {
        if let Err(e) = s.batch("SET IMPLICIT_TRANSACTIONS ON").await {
            tracing::debug!("sqlserver jobs: could not restore implicit transactions: {e}");
        }
    }
    done
}

/// `sp_start_job` returns once the Agent queued the run.
pub(crate) async fn run(s: &mut SqlServerSession, id: &str) -> Result<()> {
    act(s, "EXEC msdb.dbo.sp_start_job @job_id = @P1;", id).await
}

pub(crate) async fn stop(s: &mut SqlServerSession, id: &str) -> Result<()> {
    act(s, "EXEC msdb.dbo.sp_stop_job @job_id = @P1;", id).await
}

pub(crate) async fn set_enabled(s: &mut SqlServerSession, id: &str, enabled: bool) -> Result<()> {
    act(s, &enable_sql(enabled), id).await
}

fn enable_sql(enabled: bool) -> String {
    format!(
        "EXEC msdb.dbo.sp_update_job @job_id = @P1, @enabled = {};",
        u8::from(enabled)
    )
}

// ---------------------------------------------------------------------------
// Script
// ---------------------------------------------------------------------------

/// The names of the proxies the steps run as (`sp_help_proxy`, which the
/// Agent roles may run). One the login can't see stays unnamed.
async fn proxy_names(s: &mut SqlServerSession, steps: &[StepRow]) -> HashMap<i64, String> {
    let ids: BTreeSet<i64> = steps.iter().filter_map(|s| s.proxy_id).collect();
    let mut names = HashMap::new();
    for id in ids {
        // A number read from msdb, not text.
        let sql = format!("EXEC msdb.dbo.sp_help_proxy @proxy_id = {id};");
        match sets(s, &sql, &[], None).await {
            Ok(sets) => {
                if let Some(name) = first_with(&sets, "proxy_id")
                    .and_then(|s| s.rows.first())
                    .and_then(|r| text(r, "name"))
                {
                    names.insert(id, name);
                }
            }
            Err(e) => tracing::debug!("sqlserver jobs: proxy {id}: {e}"),
        }
    }
    names
}

pub(crate) async fn script(s: &mut SqlServerSession, id: &str) -> Result<String> {
    let (p, raw, steps, _) = read_job(s, id).await?;
    let proxies = proxy_names(s, &steps).await;
    Ok(job_script(&raw, &steps, &proxies, p.major))
}

/// A line of the script could read as a `GO` (SSMS and sqlcmd cut batches
/// there even inside a string).
fn has_go_line(text: &str) -> bool {
    text.lines().any(|l| {
        let t = l.trim();
        t.get(..2).is_some_and(|go| go.eq_ignore_ascii_case("GO"))
            && t[2..]
                .chars()
                .next()
                .is_none_or(|c| c.is_whitespace() || c == '-' || c == '/' || c.is_ascii_digit())
    })
}

/// Builds the script, naming each long text a variable when it can't go as
/// a literal.
struct ScriptWriter {
    out: String,
    vars: usize,
    /// Inside a `BEGIN … END`.
    nested: bool,
}

impl ScriptWriter {
    /// A text value for a procedure parameter: an `N'…'` literal, or, when a
    /// line of it could read as `GO`, a variable set on one line with the
    /// line breaks as `NCHAR(13)` / `NCHAR(10)`.
    fn text(&mut self, value: &str) -> String {
        if !has_go_line(value) {
            return literal(value);
        }
        self.vars += 1;
        let var = format!("@text{}", self.vars);
        let mut parts = vec!["CAST(N'' AS nvarchar(max))".to_string()];
        let mut piece = String::new();
        for c in value.chars() {
            match c {
                '\r' | '\n' => {
                    if !piece.is_empty() {
                        parts.push(literal(&std::mem::take(&mut piece)));
                    }
                    parts.push(if c == '\r' { "NCHAR(13)" } else { "NCHAR(10)" }.into());
                }
                _ => piece.push(c),
            }
        }
        if !piece.is_empty() {
            parts.push(literal(&piece));
        }
        let _ = writeln!(
            self.out,
            "DECLARE {var} nvarchar(max);\nSET {var} = {};",
            parts.join(" + ")
        );
        var
    }

    /// `EXEC @ReturnCode = proc @a = x, …;` and the rollback check.
    fn exec(&mut self, proc_name: &str, args: &[(&str, String)]) {
        let pad = if self.nested { "    " } else { "" };
        let args: Vec<String> = args.iter().map(|(k, v)| format!("@{k} = {v}")).collect();
        let _ = writeln!(
            self.out,
            "{pad}EXEC @ReturnCode = {proc_name} {};\n{pad}IF (@@ERROR <> 0 OR @ReturnCode <> 0) GOTO QuitWithRollback;",
            args.join(&format!(",\n{pad}    "))
        );
    }

    fn begin(&mut self) {
        self.out.push_str("BEGIN\n");
        self.nested = true;
    }

    fn end(&mut self) {
        self.out.push_str("END;\n");
        self.nested = false;
    }
}

/// T-SQL that creates the job as it is now, in one transaction (the shape
/// SSMS's "Script Job as CREATE" has).
fn job_script(raw: &Raw, steps: &[StepRow], proxies: &HashMap<i64, String>, major: i64) -> String {
    let j = &raw.job;
    let mut w = ScriptWriter {
        out: String::new(),
        vars: 0,
        nested: false,
    };
    let _ = writeln!(
        w.out,
        "-- Job del Agente SQL Server «{}»\nBEGIN TRANSACTION;\nDECLARE @ReturnCode int, @jobId uniqueidentifier, @scheduleId int;\nSET @ReturnCode = 0;",
        comment_text(&j.name)
    );

    if let Some(category) = &j.category {
        let name = w.text(category);
        let kind = if j.kind == 2 { "MULTI-SERVER" } else { "LOCAL" };
        let _ = writeln!(
            w.out,
            "IF NOT EXISTS (SELECT name FROM msdb.dbo.syscategories WHERE name = {name} AND category_class = 1)"
        );
        w.begin();
        w.exec(
            "msdb.dbo.sp_add_category",
            &[
                ("class", "N'JOB'".into()),
                ("type", format!("N'{kind}'")),
                ("name", name.clone()),
            ],
        );
        w.end();
    }

    let mut args: Vec<(&str, String)> = vec![
        ("job_name", w.text(&j.name)),
        ("enabled", u8::from(j.enabled).to_string()),
        ("notify_level_eventlog", j.notify_eventlog.to_string()),
        ("notify_level_email", j.notify_email.to_string()),
        ("notify_level_netsend", j.notify_netsend.to_string()),
        ("notify_level_page", j.notify_page.to_string()),
        ("delete_level", j.delete_level.to_string()),
    ];
    for (key, level, op) in [
        (
            "notify_email_operator_name",
            j.notify_email,
            &j.email_operator,
        ),
        (
            "notify_netsend_operator_name",
            j.notify_netsend,
            &j.netsend_operator,
        ),
        ("notify_page_operator_name", j.notify_page, &j.page_operator),
    ] {
        if let Some(op) = op.as_deref().filter(|_| level > 0) {
            args.push((key, w.text(op)));
        }
    }
    if let Some(d) = &j.description {
        args.push(("description", w.text(d)));
    }
    if let Some(c) = &j.category {
        args.push(("category_name", w.text(c)));
    }
    if let Some(o) = &j.owner {
        args.push(("owner_login_name", w.text(o)));
    }
    args.push(("job_id", "@jobId OUTPUT".into()));
    w.exec("msdb.dbo.sp_add_job", &args);

    for s in steps {
        let mut args: Vec<(&str, String)> = vec![
            ("job_id", "@jobId".into()),
            ("step_name", w.text(&s.name)),
            ("step_id", s.id.to_string()),
            ("cmdexec_success_code", s.success_code.to_string()),
            ("on_success_action", s.on_success.to_string()),
            ("on_success_step_id", s.on_success_step.to_string()),
            ("on_fail_action", s.on_fail.to_string()),
            ("on_fail_step_id", s.on_fail_step.to_string()),
            ("retry_attempts", s.retry_attempts.to_string()),
            ("retry_interval", s.retry_interval.to_string()),
            ("os_run_priority", s.os_priority.to_string()),
            ("subsystem", w.text(&s.subsystem)),
        ];
        for (key, value) in [
            ("server", &s.server),
            ("database_name", &s.database),
            ("database_user_name", &s.database_user),
            ("output_file_name", &s.output_file),
        ] {
            if let Some(v) = value {
                args.push((key, w.text(v)));
            }
        }
        args.push(("flags", s.flags.to_string()));
        if let Some(proxy) = s.proxy_id {
            match proxies.get(&proxy) {
                Some(name) => args.push(("proxy_name", w.text(name))),
                None => {
                    let _ = writeln!(
                        w.out,
                        "-- El paso {} se ejecuta con el proxy {proxy}, que este usuario no puede ver.",
                        s.id
                    );
                }
            }
        }
        args.push(("command", w.text(s.command.as_deref().unwrap_or(""))));
        w.exec("msdb.dbo.sp_add_jobstep", &args);
    }
    if !steps.is_empty() {
        w.exec(
            "msdb.dbo.sp_update_job",
            &[
                ("job_id", "@jobId".into()),
                ("start_step_id", j.start_step.to_string()),
            ],
        );
    }

    for sch in &raw.schedules {
        let mut args: Vec<(&str, String)> = vec![
            ("enabled", u8::from(sch.enabled).to_string()),
            ("freq_type", sch.freq_type.to_string()),
            ("freq_interval", sch.freq_interval.to_string()),
            ("freq_subday_type", sch.subday_type.to_string()),
            ("freq_subday_interval", sch.subday_interval.to_string()),
            ("freq_relative_interval", sch.relative_interval.to_string()),
            ("freq_recurrence_factor", sch.recurrence_factor.to_string()),
            ("active_start_date", sch.start_date.to_string()),
            ("active_end_date", sch.end_date.to_string()),
            ("active_start_time", sch.start_time.to_string()),
            ("active_end_time", sch.end_time.to_string()),
        ];
        if major > 0 && major < 9 {
            // SQL Server 2000: schedules belong to one job.
            args.insert(0, ("name", w.text(&sch.name)));
            args.insert(0, ("job_id", "@jobId".into()));
            w.exec("msdb.dbo.sp_add_jobschedule", &args);
            continue;
        }
        args.insert(0, ("schedule_name", w.text(&sch.name)));
        let _ = writeln!(w.out, "SET @scheduleId = NULL;");
        // Shared with other jobs: attach the same one where it exists.
        let shared = sch.uid.as_deref().filter(|_| sch.job_count > 1);
        if let Some(uid) = shared {
            args.push(("schedule_uid", literal(uid)));
            let _ = writeln!(
                w.out,
                "SELECT @scheduleId = schedule_id FROM msdb.dbo.sysschedules_localserver_view WHERE schedule_uid = {};\nIF @scheduleId IS NULL",
                literal(uid)
            );
            w.begin();
        }
        args.push(("schedule_id", "@scheduleId OUTPUT".into()));
        w.exec("msdb.dbo.sp_add_schedule", &args);
        if shared.is_some() {
            w.end();
        }
        w.exec(
            "msdb.dbo.sp_attach_schedule",
            &[
                ("job_id", "@jobId".into()),
                ("schedule_id", "@scheduleId".into()),
            ],
        );
    }

    for server in &raw.servers {
        let name = if server.id == 0 {
            "N'(local)'".to_string()
        } else {
            w.text(&server.name)
        };
        w.exec(
            "msdb.dbo.sp_add_jobserver",
            &[("job_id", "@jobId".into()), ("server_name", name)],
        );
    }

    w.out.push_str(
        "COMMIT TRANSACTION;\nGOTO EndSave;\nQuitWithRollback:\n    IF (@@TRANCOUNT > 0) ROLLBACK TRANSACTION;\nEndSave:\n",
    );
    w.out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn at(s: &str) -> NaiveDateTime {
        NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S").unwrap()
    }

    fn uuid(s: &str) -> Uuid {
        Uuid::parse_str(s).unwrap()
    }

    const ID: &str = "35b8fd0b-0208-4795-a5b9-0fdc971a8414";

    #[test]
    fn ids_are_guids_and_nothing_else() {
        assert_eq!(job_id(ID).unwrap(), ID);
        assert_eq!(
            job_id(" 35B8FD0B-0208-4795-A5B9-0FDC971A8414 ").unwrap(),
            ID
        );
        for bad in [
            "",
            "35b8fd0b02084795a5b90fdc971a8414",
            "{35b8fd0b-0208-4795-a5b9-0fdc971a8414}",
            "35b8fd0b-0208-4795-a5b9-0fdc971a841'",
            "35b8fd0b-0208-4795-a5b9-0fdc971a8414'; DROP TABLE x; --",
            "x'); EXEC xp_cmdshell 'dir'; --",
            "35b8fd0b-0208-4795-a5b9_0fdc971a8414",
            "35b8fd0b-0208-4795-a5b9-0fdc971a841g",
            "1",
        ] {
            let e = job_id(bad).unwrap_err();
            assert!(matches!(e, Error::Query(_)), "{bad}: {e:?}");
        }
        // The refused id is echoed short and on one line.
        let long = format!("{}\nDROP", "a".repeat(200));
        let Error::Query(m) = job_id(&long).unwrap_err() else {
            panic!()
        };
        assert!(!m.contains('\n') && m.chars().count() < 120, "{m}");
    }

    #[test]
    fn agent_dates_times_and_durations() {
        assert_eq!(agent_time(20261112, 30000), Some(at("2026-11-12 03:00:00")));
        assert_eq!(
            agent_time(20261112, 235959),
            Some(at("2026-11-12 23:59:59"))
        );
        assert_eq!(agent_time(20260101, 5), Some(at("2026-01-01 00:00:05")));
        assert_eq!(agent_time(0, 0), None);
        assert_eq!(agent_time(20261340, 0), None);
        assert_eq!(agent_time(20261112, 250000), None);
        assert_eq!(agent_duration(0), Some(0));
        assert_eq!(agent_duration(3), Some(3_000));
        assert_eq!(agent_duration(10203), Some((3600 + 2 * 60 + 3) * 1000));
        // More than 99 hours: 123:45:06.
        assert_eq!(
            agent_duration(1234506),
            Some((123 * 3600 + 45 * 60 + 6) * 1000)
        );
        assert_eq!(agent_duration(-1), None);
        assert_eq!(leading_int("1 (quit with success)"), Some(1));
        assert_eq!(leading_int("0 (unknown)"), Some(0));
        assert_eq!(leading_int("x"), None);
    }

    #[test]
    fn local_times_turn_utc() {
        let clock = Clock {
            offset_min: -300,
            ..Default::default()
        };
        assert_eq!(clock.utc(at("2026-11-12 08:00:20")), "2026-11-12T13:00:20Z");
        assert_eq!(clock.utc(at("2026-12-31 22:30:00")), "2027-01-01T03:30:00Z");
        // Fractions are dropped.
        let t = at("2026-11-12 08:00:20") + Duration::milliseconds(997);
        assert_eq!(clock.utc(t), "2026-11-12T13:00:20Z");
        // The zone's answer for an hour wins over today's offset (a July run
        // in New York is 4 hours behind UTC, not 5).
        let mut dst = clock.clone();
        dst.add.insert(at("2026-07-01 03:00:00"), 240);
        assert_eq!(dst.utc(at("2026-07-01 03:15:09")), "2026-07-01T07:15:09Z");
        assert_eq!(dst.utc(at("2026-11-12 03:15:09")), "2026-11-12T08:15:09Z");
        let east = Clock {
            offset_min: 330,
            ..Default::default()
        };
        assert_eq!(east.utc(at("2026-11-12 05:30:00")), "2026-11-12T00:00:00Z");
    }

    #[test]
    fn the_zone_query_takes_only_formatted_dates() {
        let sql = zone_sql(&[at("2026-07-01 03:00:00"), at("2026-11-12 08:00:00")]);
        assert!(sql.contains("(CAST('2026-07-01T03:00:00' AS datetime2(0))), (CAST('2026-11-12T08:00:00' AS datetime2(0)))"), "{sql}");
        assert!(sql.contains("AT TIME ZONE @P1"));
        assert_eq!(hour(at("2026-07-01 03:59:59")), at("2026-07-01 03:00:00"));
        let p = Probe {
            offset_min: 0,
            zone: Some("UTC".into()),
            ..Default::default()
        };
        assert!(Clock::new(&p).zone.is_none(), "UTC needs no question");
    }

    #[test]
    fn the_probe_compiles_everywhere() {
        // Newer functions and msdb go through dynamic SQL, so SQL Server
        // 2005–2019 and Azure SQL Database compile the batch.
        let direct: String = PROBE_SQL
            .lines()
            .filter(|l| !l.contains("sp_executesql") && !l.contains("SET @roles"))
            .collect();
        assert!(!direct.contains("CURRENT_TIMEZONE_ID"));
        assert!(!direct.contains("IS_MEMBER(N'SQLAgent"));
        assert!(PROBE_SQL.contains("EXEC @msdb_exec @roles"));
        assert!(!PROBE_SQL.contains("SYSDATETIMEOFFSET"));
        // The Agent's sessions are only looked at where the login sees
        // every session.
        assert!(PROBE_SQL.contains(
            "IF @agent = 1 AND HAS_PERMS_BY_NAME(NULL, NULL, N'VIEW SERVER STATE') = 1\n   AND NOT EXISTS (SELECT 1 FROM sys.dm_exec_sessions WHERE program_name LIKE N'SQLAgent - %')"
        ));
        assert!(PROBE_SQL
            .contains("UPPER(CAST(SERVERPROPERTY('ServerName') AS nvarchar(128))) AS server_name"));
        // What every TDS server answers, before anything about the Agent.
        assert!(ENGINE_SQL.starts_with("SELECT CAST(SERVERPROPERTY('EngineEdition') AS int) AS edition, CAST(SERVERPROPERTY('Babelfish') AS int) AS babelfish"));
        assert!(!ENGINE_SQL.contains(';') && !ENGINE_SQL.contains("FROM"));
        for e in [5, 6, 11, 12] {
            assert!(edition_without_agent(e), "{e}");
        }
        for e in [1, 2, 3, 4, 8] {
            assert!(!edition_without_agent(e), "{e}");
        }
    }

    #[test]
    fn the_batches_follow_the_version() {
        assert!(list_sql(16).contains("EXEC msdb.dbo.sp_help_jobactivity;"));
        assert!(!list_sql(8).contains("sp_help_jobactivity"));
        assert!(list_sql(10).contains("SELECT @dbine_job AS dbine_job"));
        assert!(detail_sql(11).contains("sp_help_jobactivity @job_id = @P1"));
        assert!(!detail_sql(8).contains("sp_help_jobactivity"));
        assert!(runs_sql(15).contains("sp_help_jobhistory @job_id = @P1"));
        // The id is always a parameter.
        for sql in [detail_sql(16), runs_sql(16), enable_sql(true)] {
            assert!(
                !sql.contains("'@P1'") && sql.contains("@job_id = @P1"),
                "{sql}"
            );
        }
        assert!(enable_sql(false).ends_with("@enabled = 0;"));
    }

    fn sysadmin() -> Probe {
        Probe {
            edition: 3,
            major: 16,
            sysadmin: true,
            agent_running: true,
            login: "sa".into(),
            server: "SRV".into(),
            access: true,
            ..Default::default()
        }
    }

    fn user_role(login: &str) -> Probe {
        Probe {
            sysadmin: false,
            login: login.into(),
            ..sysadmin()
        }
    }

    fn daily() -> ScheduleRow {
        ScheduleRow {
            name: "nightly".into(),
            enabled: true,
            freq_type: 4,
            freq_interval: 1,
            subday_type: 1,
            start_date: 20261112,
            end_date: 99991231,
            start_time: 30000,
            end_time: 235959,
            uid: Some("a7293200-ed97-47f5-90cd-d99074d73aea".into()),
            job_count: 1,
            ..Default::default()
        }
    }

    fn fixture() -> Raw {
        Raw {
            job: JobRow {
                id: uuid(ID),
                name: "dbine nightly".into(),
                enabled: true,
                description: Some("load".into()),
                start_step: 1,
                category: Some("[Uncategorized (Local)]".into()),
                owner: Some("sa".into()),
                origin: Some("SRV".into()),
                notify_eventlog: 2,
                created: Some(at("2026-11-12 08:00:18")),
                last_run: Some(at("2026-11-12 08:00:20")),
                last_outcome: 0,
                next_run: Some(at("2026-11-13 03:00:00")),
                status: 4,
                steps: 2,
                targets: 1,
                kind: 1,
                ..Default::default()
            },
            schedules: vec![daily()],
            servers: vec![ServerRow {
                id: 0,
                name: "srv".into(),
                duration_ms: Some(3000),
                message: Some("The job failed.".into()),
            }],
            activity: None,
        }
    }

    #[test]
    fn a_job_reads_as_the_list_shows_it() {
        let clock = Clock {
            offset_min: -300,
            ..Default::default()
        };
        let j = server_job(&fixture(), &sysadmin(), &clock);
        assert_eq!(j.id, ID);
        assert_eq!(j.kind, "SQL Server Agent");
        assert_eq!(j.group.as_deref(), Some("[Uncategorized (Local)]"));
        assert_eq!(j.schedule.as_deref(), Some("Todos los días, a las 03:00"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T13:00:20Z"));
        assert_eq!(j.last_duration_ms, Some(3000));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T08:00:00Z"));
        assert!(j.enabled && !j.running);
        assert!(j.can_run && !j.can_stop && j.can_enable && j.has_history && j.has_script);
        let json = serde_json::to_value(&j).unwrap();
        assert_eq!(json["last_outcome"], "failed");
    }

    #[test]
    fn what_a_job_allows_follows_the_login_and_the_agent() {
        let clock = Clock::default();
        let raw = fixture();
        // Not its owner, without SQLAgentOperatorRole: only reads.
        let reader = Probe {
            reader: true,
            ..user_role("ana")
        };
        let j = server_job(&raw, &reader, &clock);
        assert!(!j.can_run && !j.can_stop && !j.can_enable && j.has_history);
        // Its owner (case aside) and an operator manage it.
        let owner = server_job(&raw, &user_role("SA"), &clock);
        assert!(owner.can_run && owner.can_enable);
        let operator = Probe {
            operator: true,
            ..user_role("ana")
        };
        assert!(server_job(&raw, &operator, &clock).can_run);
        // With the Agent stopped nothing runs, nor will on schedule;
        // enabling still works.
        let stopped = Probe {
            agent_running: false,
            ..sysadmin()
        };
        let j = server_job(&raw, &stopped, &clock);
        assert!(!j.can_run && !j.can_stop && j.can_enable);
        assert_eq!(j.next_run, None, "msdb's stale next run");
        // Express has no Agent, whatever 'Agent XPs' says.
        let express = Probe {
            edition: 4,
            ..sysadmin()
        };
        let j = server_job(&raw, &express, &clock);
        assert!(!j.can_run && !j.can_stop && j.can_enable, "{j:?}");
        assert_eq!(j.next_run, None);
        assert_eq!(express.notes(), vec![EXPRESS_NOTE.to_string()]);
        // Running: stop, not run.
        let mut busy = fixture();
        busy.job.status = 1;
        busy.activity = Some(ActivityRow {
            job: uuid(ID),
            started: Some(at("2026-11-12 09:00:00")),
            ..Default::default()
        });
        let j = server_job(&busy, &sysadmin(), &clock);
        assert!(j.running && !j.can_run && j.can_stop);
        assert_eq!(
            j.last_outcome,
            Some(JobOutcome::Failed),
            "the last finished run"
        );
        // Running for the first time: the run going on.
        busy.job.last_run = None;
        let j = server_job(&busy, &sysadmin(), &clock);
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T09:00:00Z"));
        // No steps or no target server: the Agent wouldn't run it.
        let mut empty = fixture();
        empty.job.steps = 0;
        assert!(!server_job(&empty, &sysadmin(), &clock).can_run);
        let mut untargeted = fixture();
        untargeted.job.targets = 0;
        assert!(!server_job(&untargeted, &sysadmin(), &clock).can_run);
        // On a target server, a job its master sent: it runs and stops
        // here, but msdb refuses to change it (14274).
        let mut sent = fixture();
        sent.job.origin = Some("MSX".into());
        let j = server_job(&sent, &sysadmin(), &clock);
        assert!(j.can_run && !j.can_enable && j.has_history, "{j:?}");
        let j = server_job(&sent, &operator, &clock);
        assert!(j.can_run && !j.can_enable, "{j:?}");
        sent.job.status = 1;
        let j = server_job(&sent, &sysadmin(), &clock);
        assert!(j.can_stop && !j.can_enable, "{j:?}");
        // Without the server's name nothing is taken for a master's.
        let unnamed = Probe {
            server: String::new(),
            ..sysadmin()
        };
        assert!(server_job(&fixture(), &unnamed, &clock).can_enable);
        // The origin is compared as msdb writes it (uppercase).
        let mut here = fixture();
        here.job.origin = Some("srv".into());
        assert!(server_job(&here, &sysadmin(), &clock).can_enable);
        // On its master, a multiserver job (targets elsewhere only): sysadmin
        // posts the run to its targets; its owner and operators may still
        // change it.
        let mut multi = fixture();
        multi.job.kind = 2;
        multi.servers = vec![ServerRow {
            id: 5,
            name: "TSX1".into(),
            ..Default::default()
        }];
        let j = server_job(&multi, &sysadmin(), &clock);
        assert!(j.can_run && j.can_enable && j.has_history, "{j:?}");
        let j = server_job(&multi, &operator, &clock);
        assert!(!j.can_run && j.can_enable, "{j:?}");
        let j = server_job(&multi, &user_role("SA"), &clock);
        assert!(!j.can_run && j.can_enable, "{j:?}");
        // Disabled: no next run.
        let mut off = fixture();
        off.job.enabled = false;
        assert_eq!(server_job(&off, &sysadmin(), &clock).next_run, None);
        // Never ran.
        let mut new = fixture();
        new.job.last_run = None;
        let j = server_job(&new, &sysadmin(), &clock);
        assert_eq!((j.last_outcome, j.last_started), (None, None));
    }

    #[test]
    fn notes_say_why_the_list_is_empty_or_partial() {
        assert_eq!(
            Probe {
                edition: 5,
                ..Default::default()
            }
            .missing(),
            Some(AZURE_SQL_NOTE)
        );
        assert!(AZURE_SQL_NOTE.contains("Elastic Jobs"));
        for edition in [6, 11, 12] {
            let p = Probe {
                edition,
                ..Default::default()
            };
            assert_eq!(p.missing(), Some(NO_AGENT_NOTE), "{edition}");
            assert!(p.no_agent());
        }
        // Babelfish through a `sqlserver` connection: no Agent, whatever
        // edition it reports and even if msdb answers.
        let babelfish = Probe {
            edition: 3,
            babelfish: true,
            access: true,
            ..Default::default()
        };
        assert_eq!(babelfish.missing(), Some(NO_AGENT_NOTE));
        assert!(babelfish.no_agent());
        assert!(!sysadmin().no_agent());
        let none = Probe {
            access: false,
            ..user_role("ana")
        };
        assert_eq!(none.missing(), Some(NO_ACCESS_NOTE));
        for role in [
            "SQLAgentUserRole",
            "SQLAgentReaderRole",
            "SQLAgentOperatorRole",
            "sysadmin",
        ] {
            assert!(NO_ACCESS_NOTE.contains(role));
        }
        assert_eq!(sysadmin().missing(), None);
        assert!(sysadmin().notes().is_empty());
        assert_eq!(user_role("ana").notes(), vec![OWN_JOBS_NOTE.to_string()]);
        let reader = Probe {
            reader: true,
            ..user_role("ana")
        };
        assert!(reader.notes().is_empty());
        let stopped = Probe {
            agent_running: false,
            ..sysadmin()
        };
        assert_eq!(stopped.notes(), vec![AGENT_STOPPED_NOTE.to_string()]);
        let express = Probe {
            edition: 4,
            agent_running: false,
            ..sysadmin()
        };
        assert_eq!(express.notes(), vec![EXPRESS_NOTE.to_string()]);
    }

    #[test]
    fn schedules_in_words() {
        let mut s = daily();
        assert_eq!(describe(&s), "Todos los días, a las 03:00");
        s.freq_interval = 2;
        s.start_time = 30030;
        assert_eq!(describe(&s), "Cada 2 días, a las 03:00:30");
        let weekly = ScheduleRow {
            freq_type: 8,
            freq_interval: 2 | 8 | 32,
            recurrence_factor: 1,
            ..daily()
        };
        assert_eq!(
            describe(&weekly),
            "Todas las semanas: lunes, miércoles y viernes, a las 03:00"
        );
        let biweekly = ScheduleRow {
            freq_type: 8,
            freq_interval: 1,
            recurrence_factor: 2,
            ..daily()
        };
        assert_eq!(describe(&biweekly), "Cada 2 semanas: domingo, a las 03:00");
        let monthly = ScheduleRow {
            freq_type: 16,
            freq_interval: 15,
            recurrence_factor: 1,
            ..daily()
        };
        assert_eq!(describe(&monthly), "El día 15 de cada mes, a las 03:00");
        let relative = ScheduleRow {
            freq_type: 32,
            freq_interval: 9,
            relative_interval: 16,
            recurrence_factor: 3,
            ..daily()
        };
        assert_eq!(
            describe(&relative),
            "El último día hábil, cada 3 meses, a las 03:00"
        );
        let first_monday = ScheduleRow {
            freq_type: 32,
            freq_interval: 2,
            relative_interval: 1,
            recurrence_factor: 1,
            ..daily()
        };
        assert_eq!(
            describe(&first_monday),
            "El primer lunes de cada mes, a las 03:00"
        );
        let every_15 = ScheduleRow {
            subday_type: 4,
            subday_interval: 15,
            start_time: 80000,
            end_time: 180000,
            end_date: 20271231,
            ..daily()
        };
        assert_eq!(
            describe(&every_15),
            "Todos los días, cada 15 minutos entre las 08:00 y las 18:00, hasta el 2027-12-31"
        );
        let hourly = ScheduleRow {
            subday_type: 8,
            subday_interval: 1,
            start_time: 0,
            end_time: 235959,
            ..daily()
        };
        assert_eq!(
            describe(&hourly),
            "Todos los días, cada hora entre las 00:00 y las 23:59:59"
        );
        let once = ScheduleRow {
            freq_type: 1,
            start_date: 20261201,
            start_time: 223000,
            ..daily()
        };
        assert_eq!(describe(&once), "Una vez, el 2026-12-01 a las 22:30");
        let boot = ScheduleRow {
            freq_type: 64,
            enabled: false,
            ..daily()
        };
        assert_eq!(
            describe(&boot),
            "Al iniciar el Agente SQL Server (deshabilitada)"
        );
        // The list: enabled ones; all of them when none is.
        assert_eq!(schedule_text(&[]), None);
        assert_eq!(
            schedule_text(&[daily(), boot.clone()]).as_deref(),
            Some("Todos los días, a las 03:00")
        );
        assert_eq!(
            schedule_text(&[boot]).as_deref(),
            Some("Al iniciar el Agente SQL Server (deshabilitada)")
        );
    }

    fn h(
        instance: i64,
        step_id: i64,
        status: i64,
        started: &str,
        dur: i64,
        message: &str,
    ) -> HistoryRow {
        HistoryRow {
            instance,
            step_id,
            step_name: if step_id == 0 {
                "(Job outcome)".into()
            } else {
                format!("s{step_id}")
            },
            message: Some(message.into()),
            status,
            started: Some(at(started)),
            duration_ms: agent_duration(dur),
        }
    }

    fn history() -> Vec<HistoryRow> {
        vec![
            h(1, 1, 1, "2026-11-11 03:00:00", 3, "Executed as user: x. The step succeeded."),
            h(2, 2, 0, "2026-11-11 03:00:03", 0, "boom [SQLSTATE 42000] (Error 50000).  The step failed."),
            h(3, 0, 0, "2026-11-11 03:00:00", 3, "The job failed.  The Job was invoked by Schedule 8 (nightly).  The last step to run was step 2 (s2)."),
            h(4, 1, 1, "2026-11-12 08:00:20", 3, "The step succeeded."),
            h(5, 2, 1, "2026-11-12 08:00:23", 0, "The step succeeded."),
            h(6, 0, 1, "2026-11-12 08:00:20", 3, "The job succeeded.  The Job was invoked by User sa.  The last step to run was step 2 (s2)."),
        ]
    }

    #[test]
    fn history_rows_group_into_runs_newest_first() {
        let mut rows = history();
        rows.reverse(); // the order doesn't matter
        let runs = group_runs(rows, false, None, 10);
        assert_eq!(runs.len(), 2);
        let (last, first) = (&runs[0], &runs[1]);
        assert_eq!(last.id, Some(6));
        assert_eq!(last.outcome, JobOutcome::Succeeded);
        assert_eq!(last.trigger, Some("manual"));
        assert_eq!(last.duration_ms, Some(3000));
        assert_eq!(
            last.steps.iter().map(|s| s.instance).collect::<Vec<_>>(),
            [4, 5],
            "steps in the order they ran"
        );
        assert_eq!(first.outcome, JobOutcome::Failed);
        assert_eq!(first.trigger, Some("programada"));
        assert_eq!(first.steps[1].status, 0);
        assert!(first.steps[1].message.as_deref().unwrap().contains("boom"));
        assert_eq!(group_runs(history(), false, None, 1).len(), 1);
        assert!(group_runs(history(), false, None, 0).is_empty());

        let clock = Clock {
            offset_min: -300,
            ..Default::default()
        };
        let run = job_run(runs[1].clone(), &clock);
        assert_eq!(run.id.as_deref(), Some("3"));
        assert_eq!(run.started.as_deref(), Some("2026-11-11T08:00:00Z"));
        assert_eq!(run.steps.len(), 2);
        assert_eq!(run.steps[1].outcome, JobOutcome::Failed);
        assert_eq!(
            run.steps[1].started.as_deref(),
            Some("2026-11-11T08:00:03Z")
        );
        assert_eq!(run.steps[0].duration_ms, Some(3000));
    }

    #[test]
    fn the_run_going_on_comes_first() {
        let mut rows = history();
        rows.push(h(7, 1, 1, "2026-11-12 09:00:00", 2, "The step succeeded."));
        let act = ActivityRow {
            job: uuid(ID),
            source: Some(1),
            started: Some(at("2026-11-12 09:00:00")),
            stopped: None,
            history_id: Some(6),
            ..Default::default()
        };
        let runs = group_runs(rows.clone(), true, Some(&act), 10);
        assert_eq!(runs.len(), 3);
        assert_eq!(runs[0].outcome, JobOutcome::Running);
        assert_eq!(runs[0].id, None);
        assert_eq!(runs[0].started, Some(at("2026-11-12 09:00:00")));
        assert_eq!(runs[0].trigger, Some("programada"));
        assert_eq!(runs[0].steps.len(), 1);
        // The activity's source names the run it recorded.
        assert_eq!(runs[1].trigger, Some("programada"));
        // Limit counts it.
        assert_eq!(group_runs(rows.clone(), true, Some(&act), 2).len(), 2);
        // Steps without an outcome and nothing running: an unfinished run.
        let runs = group_runs(rows, false, None, 10);
        assert_eq!(runs[0].outcome, JobOutcome::Unknown);
        assert_eq!(runs[0].started, Some(at("2026-11-12 09:00:00")));
        // Running before any step finished.
        let runs = group_runs(Vec::new(), true, Some(&act), 10);
        assert_eq!(runs.len(), 1);
        assert!(runs[0].steps.is_empty());
    }

    #[test]
    fn triggers_from_messages() {
        assert_eq!(
            message_trigger("The job succeeded.  The Job was invoked by Schedule 9 (x)."),
            Some("programada")
        );
        assert_eq!(
            message_trigger("The job failed.  The Job was invoked by User DOM\\ana."),
            Some("manual")
        );
        assert_eq!(
            message_trigger("The Job was invoked by Alert 3 (disk)."),
            Some("alerta")
        );
        assert_eq!(message_trigger("El trabajo se ejecutó."), None);
        assert_eq!(source_trigger(4), Some("manual"));
        assert_eq!(source_trigger(5), None);
    }

    fn steps() -> Vec<StepRow> {
        vec![
            StepRow {
                id: 1,
                name: "load 'sales'".into(),
                subsystem: "TSQL".into(),
                command: Some("EXEC dbo.load N'it''s';".into()),
                on_success: 3,
                on_fail: 2,
                database: Some("ventas".into()),
                retry_attempts: 2,
                retry_interval: 5,
                ..Default::default()
            },
            StepRow {
                id: 2,
                name: "cleanup".into(),
                subsystem: "CmdExec".into(),
                command: Some("del c:\\tmp\\*.bak\r\nGO\r\necho done".into()),
                on_success: 1,
                on_fail: 4,
                on_fail_step: 1,
                proxy_id: Some(3),
                ..Default::default()
            },
        ]
    }

    #[test]
    fn the_detail_names_steps_and_settings() {
        let clock = Clock::default();
        let st = steps();
        let s = job_step(&st[0]);
        assert_eq!(s.kind.as_deref(), Some("T-SQL"));
        assert_eq!(s.target.as_deref(), Some("ventas"));
        assert_eq!(subsystem("PowerShell"), "PowerShell");
        assert_eq!(subsystem("Custom"), "Custom");
        let props = properties(&fixture(), &st, &clock);
        let get = |label: &str| {
            props
                .iter()
                .find(|p| p.label == label)
                .map(|p| (p.group.as_str(), p.value.as_str()))
        };
        assert_eq!(get("Categoría"), Some(("", "[Uncategorized (Local)]")));
        assert_eq!(get("Paso inicial"), Some(("", "1. load 'sales'")));
        assert_eq!(
            get("Registro de eventos"),
            Some(("Notificaciones", "Si falla"))
        );
        assert_eq!(get("Creado"), Some(("", "2026-11-12T08:00:18Z")));
        assert_eq!(
            get("1. load 'sales'"),
            Some((
                "Pasos",
                "Si se completa: ir al paso siguiente; si falla: salir informando error; reintentos: 2 cada 5 min"
            ))
        );
        assert_eq!(
            get("2. cleanup").map(|p| p.1),
            Some("Si se completa: salir informando éxito; si falla: ir al paso 1")
        );
    }

    #[test]
    fn go_lines_are_spotted() {
        assert!(has_go_line("a\nGO\nb"));
        assert!(has_go_line("a\r\n  go 5\r\nb"));
        assert!(has_go_line("GO -- x"));
        assert!(!has_go_line("GOTO done"));
        assert!(!has_go_line("SELECT 'GO'"));
        assert!(!has_go_line("going"));
    }

    #[test]
    fn the_script_creates_the_job_again() {
        let mut raw = fixture();
        raw.job.name = "dbine's nightly".into();
        raw.job.notify_email = 2;
        raw.job.email_operator = Some("dba".into());
        let mut proxies = HashMap::new();
        proxies.insert(3, "files".to_string());
        let sql = job_script(&raw, &steps(), &proxies, 16);
        assert!(sql.starts_with("-- Job del Agente SQL Server «dbine's nightly»\n"));
        assert!(sql.contains("@job_name = N'dbine''s nightly'"), "{sql}");
        assert!(sql.contains("@notify_email_operator_name = N'dba'"));
        assert!(!sql.contains("notify_page_operator_name"));
        assert!(sql.contains("@category_name = N'[Uncategorized (Local)]'"));
        assert!(sql.contains("@owner_login_name = N'sa'"));
        assert!(sql.contains("@description = N'load'"));
        assert!(sql.contains("@command = N'EXEC dbo.load N''it''''s'';'"));
        assert!(sql.contains("@step_name = N'load ''sales'''"));
        assert!(sql.contains("@database_name = N'ventas'"));
        assert!(sql.contains("@proxy_name = N'files'"));
        assert!(sql.contains("msdb.dbo.sp_update_job @job_id = @jobId,\n    @start_step_id = 1"));
        assert!(sql.contains("msdb.dbo.sp_add_schedule @schedule_name = N'nightly'"));
        assert!(sql.contains("@active_start_time = 30000"));
        assert!(sql.contains(
            "msdb.dbo.sp_attach_schedule @job_id = @jobId,\n    @schedule_id = @scheduleId"
        ));
        assert!(sql.contains(
            "msdb.dbo.sp_add_jobserver @job_id = @jobId,\n    @server_name = N'(local)'"
        ));
        assert!(sql.trim_end().ends_with("EndSave:"));
        // Not shared: a schedule of its own.
        assert!(!sql.contains("schedule_uid"));
        // No line of it reads as GO: the command that has one is set on a
        // single line.
        assert!(!has_go_line(&sql), "{sql}");
        assert!(sql.contains("SET @text1 = CAST(N'' AS nvarchar(max)) + N'del c:\\tmp\\*.bak' + NCHAR(13) + NCHAR(10) + N'GO' + NCHAR(13) + NCHAR(10) + N'echo done';"), "{sql}");
        assert!(sql.contains("@command = @text1"));
        // One batch: the editor runs it whole.
        assert_eq!(crate::script::batches(&sql).len(), 1);
    }

    #[test]
    fn shared_schedules_proxies_and_old_servers() {
        let mut raw = fixture();
        raw.schedules[0].job_count = 3;
        let sql = job_script(&raw, &steps(), &HashMap::new(), 16);
        assert!(sql.contains("WHERE schedule_uid = N'a7293200-ed97-47f5-90cd-d99074d73aea';\nIF @scheduleId IS NULL\nBEGIN"), "{sql}");
        assert!(sql.contains("@schedule_uid = N'a7293200-ed97-47f5-90cd-d99074d73aea'"));
        assert!(
            sql.contains("-- El paso 2 se ejecuta con el proxy 3, que este usuario no puede ver.")
        );
        assert!(!sql.contains("@proxy_name"));
        // SQL Server 2000 has no shared schedules.
        let old = job_script(&fixture(), &steps(), &HashMap::new(), 8);
        assert!(
            old.contains("msdb.dbo.sp_add_jobschedule @job_id = @jobId,\n    @name = N'nightly'"),
            "{old}"
        );
        assert!(!old.contains("sp_add_schedule") && !old.contains("sp_attach_schedule"));
    }

    /// Babelfish reached as SQL Server (`DBINE_TEST_BABELFISH_URL`; skipped
    /// without it): msdb answers but there's no Agent, and the list says so
    /// instead of asking for Agent roles. A `sqlserver` connection wraps the
    /// login in TLS, which the test container doesn't have, so the session
    /// opens as Babelfish and then takes the `sqlserver` variant: the jobs
    /// code sees what it would through a `sqlserver` connection.
    #[tokio::test]
    #[ignore]
    async fn babelfish_as_sqlserver_live() {
        use crate::{variant, SqlServerDriver};
        use dbine_driver::ConnectionConfig;
        let Ok(url) = std::env::var("DBINE_TEST_BABELFISH_URL") else {
            eprintln!("DBINE_TEST_BABELFISH_URL not set; skipping");
            return;
        };
        let rest = url.split_once("://").map_or(url.as_str(), |(_, r)| r);
        let (auth, hostport) = rest.rsplit_once('@').unwrap();
        let (user, pass) = auth.split_once(':').unwrap();
        let (host, port) = hostport.rsplit_once(':').unwrap();
        let cfg = ConnectionConfig {
            driver: "babelfish".into(),
            host: host.into(),
            port: port.trim_end_matches('/').parse().unwrap(),
            username: Some(user.into()),
            password: Some(pass.into()),
            ..Default::default()
        };
        let d = SqlServerDriver {
            info: variant::info(Variant::Babelfish),
            variant: Variant::Babelfish,
        };
        let mut s = d.open(&cfg, None).await.expect("connect");
        assert!(matches!(list(&mut s).await, Err(Error::Unsupported(_))));
        s.variant = Variant::SqlServer;
        let p = probe(&mut s).await.expect("probe");
        eprintln!("{p:?}");
        assert!(p.babelfish && p.no_agent(), "{p:?}");
        let l = list(&mut s).await.expect("no error");
        assert!(l.jobs.is_empty());
        assert_eq!(l.notes, vec![NO_AGENT_NOTE.to_string()]);
        for r in [
            detail(&mut s, ID).await.map(|_| ()),
            runs(&mut s, ID, 5).await.map(|_| ()),
            script(&mut s, ID).await.map(|_| ()),
        ] {
            assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
        }
        // The session is still in step.
        assert_eq!(
            s.rows("SELECT 1 AS n", &[])
                .await
                .unwrap()
                .first()
                .and_then(|r| int(r, "n")),
            Some(1)
        );
    }
}
