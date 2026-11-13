//! CockroachDB: its schedules (`SHOW SCHEDULES`) and the changefeeds that
//! are running or paused (`SHOW CHANGEFEED JOBS`).
//!
//! - A schedule's runs are the jobs it created (`SHOW JOBS FOR SCHEDULE`).
//!   It's paused and resumed with `PAUSE` / `RESUME SCHEDULE`. "Run now"
//!   moves its next run to now, and the scheduler starts it on its next
//!   pass (`jobs.scheduler.pace`, a minute by default): a backup schedule
//!   with `ALTER BACKUP SCHEDULE … EXECUTE [FULL] IMMEDIATELY` (24.1+), any
//!   other with `EXECUTE SCHEDULE` (26.1+). A backup schedule with full and
//!   incremental backups is a pair of schedules, and the plain form runs
//!   the pair's incremental one whichever id it's given: the full one is
//!   run with `FULL`. Stopping a schedule cancels the jobs it has running.
//!   Its script is `SHOW CREATE SCHEDULE` (backups and changefeeds; the
//!   row-level TTL ones belong to their table and the rest are the
//!   cluster's own).
//! - A changefeed is a job that runs until it's cancelled: pausing and
//!   resuming it (`PAUSE` / `RESUME JOB`) is disabling and enabling it,
//!   and stopping it (`CANCEL JOB`) ends it for good. It has no runs of its
//!   own.
//! - Other jobs (schema changes; a backup, restore or import run by hand;
//!   automatic statistics) are left out: they are one statement's work in
//!   progress, not something the server runs again.
//!
//! `SHOW SCHEDULES` needs `VIEWJOB` (or admin); without it the list has
//! the user's own changefeeds and a note.

use super::{flag, message, ms, not_found, prop, text, time, JobId, Row, HISTORY_LIMIT};
use crate::catalog::{comment_text, rows};
use crate::session::PgSession;
use dbine_driver::{Error, JobDetail, JobList, JobOutcome, JobRun, JobStep, Result, ServerJob};
use tokio_postgres::error::SqlState;
use tokio_postgres::SimpleQueryRow;

/// Jobs read per schedule for its last run in the list.
const LAST_JOBS: u32 = 20;

/// Who's asking and on what version.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
pub(crate) struct Who {
    pub(crate) user: String,
    pub(crate) admin: bool,
    /// (major, minor), e.g. (26, 3).
    pub(crate) version: Option<(u32, u32)>,
}

impl Who {
    /// May pause, resume or cancel what `owner` created.
    fn controls(&self, owner: Option<&str>) -> bool {
        self.admin || owner == Some(self.user.as_str())
    }

    /// `ALTER BACKUP SCHEDULE … EXECUTE [FULL] IMMEDIATELY` (24.1+).
    fn executes_backups(&self) -> bool {
        self.version.is_some_and(|v| v >= (24, 1))
    }

    /// `EXECUTE SCHEDULE` (26.1+), for every schedule but backups.
    fn executes_schedules(&self) -> bool {
        self.version.is_some_and(|v| v >= (26, 1))
    }
}

/// `CockroachDB CCL v26.3.2 (…)` → (26, 3).
pub(crate) fn version_of(text: &str) -> Option<(u32, u32)> {
    let at = text.find(" v")? + 2;
    let mut parts = text[at..]
        .split(|c: char| !c.is_ascii_digit())
        .filter(|p| !p.is_empty());
    Some((parts.next()?.parse().ok()?, parts.next()?.parse().ok()?))
}

pub(crate) async fn who(s: &PgSession) -> Result<Who> {
    let rows = s
        .text("SELECT current_user AS usr, pg_has_role('admin', 'MEMBER') AS admin, version() AS version")
        .await?;
    Ok(rows
        .first()
        .map(|r| Who {
            user: text(r, "usr").unwrap_or_default(),
            admin: flag(r, "admin"),
            version: text(r, "version").as_deref().and_then(version_of),
        })
        .unwrap_or_default())
}

/// What a schedule runs.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ScheduleKind {
    Backup,
    Changefeed,
    Ttl,
    System,
    Other,
}

impl ScheduleKind {
    pub(crate) fn of(r: &dyn Row) -> ScheduleKind {
        let command: Option<serde_json::Value> = text(r, "command")
            .as_deref()
            .and_then(|c| serde_json::from_str(c).ok());
        let has = |key: &str| command.as_ref().is_some_and(|c| c.get(key).is_some());
        if has("backup_statement") {
            ScheduleKind::Backup
        } else if has("changefeed_statement") {
            ScheduleKind::Changefeed
        } else if text(r, "label").is_some_and(|l| l.starts_with("row-level-ttl")) {
            ScheduleKind::Ttl
        } else if text(r, "owner").as_deref() == Some("node") {
            ScheduleKind::System
        } else {
            ScheduleKind::Other
        }
    }

    fn tag(self) -> &'static str {
        match self {
            ScheduleKind::Backup => "Schedule · backup",
            ScheduleKind::Changefeed => "Schedule · changefeed",
            ScheduleKind::Ttl => "Schedule · TTL",
            ScheduleKind::System => "Schedule · sistema",
            ScheduleKind::Other => "Schedule",
        }
    }

    /// Whether this version runs it now, and how.
    fn runs_now(self, who: &Who) -> bool {
        match self {
            ScheduleKind::Backup => who.executes_backups(),
            ScheduleKind::Changefeed | ScheduleKind::Ttl | ScheduleKind::System => {
                who.executes_schedules()
            }
            ScheduleKind::Other => false,
        }
    }

    /// The statement it runs, from `command`.
    pub(crate) fn statement(self, r: &dyn Row) -> Option<String> {
        let key = match self {
            ScheduleKind::Backup => "backup_statement",
            ScheduleKind::Changefeed => "changefeed_statement",
            _ => return None,
        };
        let command: serde_json::Value = serde_json::from_str(&text(r, "command")?).ok()?;
        command.get(key)?.as_str().map(str::to_string)
    }
}

/// The incremental schedule of a full + incremental backup pair
/// (`backup_type` 1, or `INCREMENTAL` in JSON that names the enum).
pub(crate) fn incremental(r: &dyn Row) -> bool {
    let command: Option<serde_json::Value> = text(r, "command")
        .as_deref()
        .and_then(|c| serde_json::from_str(c).ok());
    match command.as_ref().and_then(|c| c.get("backup_type")) {
        Some(serde_json::Value::Number(n)) => n.as_i64() == Some(1),
        Some(serde_json::Value::String(t)) => t.eq_ignore_ascii_case("INCREMENTAL"),
        _ => false,
    }
}

pub(crate) fn schedules_sql(id: Option<i64>) -> String {
    let filter = id.map(|n| format!(" WHERE id = {n}")).unwrap_or_default();
    // `*`: the columns vary by version (on_previous_running and
    // on_execution_failure are newer).
    format!(
        "SELECT *, id::STRING AS sid,
                extract(epoch FROM next_run)::STRING AS next_run_epoch,
                extract(epoch FROM created)::STRING AS created_epoch
         FROM [SHOW SCHEDULES]{filter} ORDER BY label, id"
    )
}

pub(crate) fn changefeeds_sql(id: Option<i64>) -> String {
    let filter = match id {
        Some(n) => format!("WHERE job_id = {n}"),
        // The ones still going: running, pending, paused, being paused…
        None => "WHERE status NOT IN ('succeeded', 'failed', 'canceled', 'revert-failed')".into(),
    };
    format!(
        "SELECT *, job_id::STRING AS jid, full_table_names::STRING AS tables,
                extract(epoch FROM created)::STRING AS created_epoch,
                extract(epoch FROM started)::STRING AS started_epoch
         FROM [SHOW CHANGEFEED JOBS] {filter} ORDER BY created, job_id"
    )
}

fn jobs_of_schedule_sql(n: i64, limit: u32) -> String {
    format!(
        "SELECT job_id::STRING AS id, status, error,
                extract(epoch FROM coalesce(started, created))::STRING AS started,
                ((extract(epoch FROM finished) - extract(epoch FROM started)) * 1000)::STRING AS ms
         FROM [SHOW JOBS FOR SCHEDULE {n}] ORDER BY created DESC, job_id DESC LIMIT {limit}"
    )
}

/// A job status as an outcome.
fn outcome(status: Option<&str>) -> JobOutcome {
    match status {
        Some("succeeded") => JobOutcome::Succeeded,
        Some("failed" | "revert-failed") => JobOutcome::Failed,
        Some("canceled") => JobOutcome::Canceled,
        Some("running" | "pending" | "reverting" | "pause-requested" | "cancel-requested") => {
            JobOutcome::Running
        }
        _ => JobOutcome::Unknown,
    }
}

fn finished(o: JobOutcome) -> bool {
    matches!(
        o,
        JobOutcome::Succeeded | JobOutcome::Failed | JobOutcome::Canceled
    )
}

fn run_of(r: &dyn Row) -> JobRun {
    JobRun {
        id: text(r, "id"),
        started: time(r, "started"),
        duration_ms: ms(r, "ms"),
        outcome: outcome(text(r, "status").as_deref()),
        trigger: None,
        message: message(r, "error"),
        steps: Vec::new(),
    }
}

fn schedule_job(r: &dyn Row, who: &Who, runs: &[JobRun]) -> Option<ServerJob> {
    let n: i64 = text(r, "sid")?.parse().ok()?;
    let kind = ScheduleKind::of(r);
    let owner = text(r, "owner");
    let active = text(r, "schedule_status").as_deref() == Some("ACTIVE");
    let running_count: u64 = text(r, "jobsrunning")
        .and_then(|c| c.parse().ok())
        .unwrap_or(0);
    let current = runs.iter().find(|j| j.outcome == JobOutcome::Running);
    let last = runs.iter().find(|j| finished(j.outcome));
    let running = running_count > 0 || current.is_some();
    let controls = who.controls(owner.as_deref());
    Some(ServerJob {
        id: JobId::CrdbSchedule(n).to_string(),
        name: text(r, "label").unwrap_or_else(|| format!("schedule {n}")),
        kind: kind.tag().into(),
        group: None,
        description: kind.statement(r),
        owner,
        enabled: active,
        schedule: text(r, "recurrence"),
        running,
        last_outcome: last
            .map(|j| j.outcome)
            .or(running.then_some(JobOutcome::Running)),
        last_started: current.or(last).and_then(|j| j.started.clone()),
        last_duration_ms: last.and_then(|j| j.duration_ms),
        next_run: time(r, "next_run_epoch").filter(|_| active),
        can_run: kind.runs_now(who) && active && !running && controls,
        can_stop: running && controls,
        can_enable: controls,
        has_history: true,
        has_script: matches!(kind, ScheduleKind::Backup | ScheduleKind::Changefeed),
        // The cluster's own (SQL stats compaction, schema telemetry): their
        // owner is `node`.
        system: kind == ScheduleKind::System,
    })
}

/// `{a,b}` → `a, b`.
fn tables(r: &dyn Row) -> Option<String> {
    text(r, "tables").map(|t| {
        t.trim_start_matches('{')
            .trim_end_matches('}')
            .split(',')
            .map(|p| p.trim().trim_matches('"'))
            .collect::<Vec<_>>()
            .join(", ")
    })
}

fn changefeed_job(r: &dyn Row, who: &Who) -> Option<ServerJob> {
    let n: i64 = text(r, "jid")?.parse().ok()?;
    let status = text(r, "status");
    let outcome = outcome(status.as_deref());
    let paused = matches!(status.as_deref(), Some("paused" | "pause-requested"));
    let running = outcome == JobOutcome::Running && !paused;
    let owner = text(r, "user_name");
    let controls = who.controls(owner.as_deref());
    Some(ServerJob {
        id: JobId::CrdbJob(n).to_string(),
        name: tables(r).unwrap_or_else(|| format!("changefeed {n}")),
        kind: "Changefeed".into(),
        group: text(r, "database_name"),
        description: text(r, "description"),
        owner,
        enabled: !paused && !finished(outcome),
        schedule: None,
        running,
        last_outcome: if finished(outcome) {
            Some(outcome)
        } else {
            running.then_some(JobOutcome::Running)
        },
        last_started: time(r, "started_epoch").or_else(|| time(r, "created_epoch")),
        last_duration_ms: None,
        next_run: None,
        can_run: false,
        // Already being cancelled: nothing more to ask.
        can_stop: controls && !finished(outcome) && status.as_deref() != Some("cancel-requested"),
        can_enable: controls && matches!(status.as_deref(), Some("running" | "paused")),
        has_history: false,
        has_script: true,
        system: false,
    })
}

/// A query that may need `VIEWJOB`: `None` when the user lacks it.
pub(crate) async fn guarded(s: &PgSession, sql: &str) -> Result<Option<Vec<SimpleQueryRow>>> {
    match s.client.simple_query(sql).await {
        Ok(m) => Ok(Some(rows(m))),
        Err(e) if e.code() == Some(&SqlState::INSUFFICIENT_PRIVILEGE) => Ok(None),
        Err(e) => Err(crate::err(e)),
    }
}

pub(crate) async fn list(s: &PgSession) -> Result<JobList> {
    let who = who(s).await?;
    let mut out = JobList::default();
    match guarded(s, &schedules_sql(None)).await? {
        Some(schedules) => {
            for r in &schedules {
                let runs = match text(r, "sid") {
                    Some(id) => match s
                        .text_within(&jobs_of_schedule_sql(id.parse().unwrap_or(0), LAST_JOBS), HISTORY_LIMIT)
                        .await
                    {
                        Ok(rows) => rows.iter().map(|r| run_of(r)).collect(),
                        Err(e) => {
                            tracing::debug!("cockroach: jobs of schedule {id}: {e}");
                            Vec::new()
                        }
                    },
                    None => Vec::new(),
                };
                out.jobs.extend(schedule_job(r, &who, &runs));
            }
        }
        None => out.notes.push(
            "Tu usuario no puede ver los schedules: CockroachDB pide el privilegio VIEWJOB (o el rol admin). Se ven solo tus changefeeds."
                .into(),
        ),
    }
    let changefeeds = s.text(&changefeeds_sql(None)).await?;
    out.jobs
        .extend(changefeeds.iter().filter_map(|r| changefeed_job(r, &who)));
    Ok(out)
}

/// The schedule, refused when the user can't see schedules.
async fn schedule_row(s: &PgSession, n: i64) -> Result<SimpleQueryRow> {
    guarded(s, &schedules_sql(Some(n)))
        .await?
        .ok_or_else(|| {
            Error::Query(
                "Tu usuario no puede ver los schedules: CockroachDB pide el privilegio VIEWJOB (o el rol admin)."
                    .into(),
            )
        })?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(JobId::CrdbSchedule(n)))
}

async fn changefeed_row(s: &PgSession, n: i64) -> Result<SimpleQueryRow> {
    s.text(&changefeeds_sql(Some(n)))
        .await?
        .into_iter()
        .next()
        .ok_or_else(|| not_found(JobId::CrdbJob(n)))
}

async fn schedule_runs(s: &PgSession, n: i64, limit: u32) -> Result<Vec<JobRun>> {
    Ok(s.text(&jobs_of_schedule_sql(n, limit))
        .await?
        .iter()
        .map(|r| run_of(r))
        .collect())
}

/// What the scheduler does when the previous run is still going, or when a
/// run fails.
fn policy_text(value: &str) -> String {
    match value {
        "WAIT" => "esperar a que termine".into(),
        "SKIP" => "saltear esta ejecución".into(),
        "START" => "empezar otra igual".into(),
        "RETRY_SCHED" | "RESCHEDULE" => "reintentar en la próxima programación".into(),
        "RETRY_SOON" => "reintentar enseguida".into(),
        "PAUSE_SCHED" => "pausar el schedule".into(),
        other => other.into(),
    }
}

pub(crate) async fn schedule_detail(s: &PgSession, n: i64) -> Result<JobDetail> {
    let who = who(s).await?;
    let r = schedule_row(s, n).await?;
    let runs = schedule_runs(s, n, LAST_JOBS).await.unwrap_or_default();
    let job = schedule_job(&r, &who, &runs).ok_or_else(|| not_found(JobId::CrdbSchedule(n)))?;
    let kind = ScheduleKind::of(&r);
    let mut properties = vec![prop("", "Id", n.to_string())];
    if let Some(t) = time(&r, "created_epoch") {
        properties.push(prop("", "Creado", t));
    }
    if let Some(v) = text(&r, "on_previous_running") {
        properties.push(prop("", "Si la anterior sigue corriendo", policy_text(&v)));
    }
    if let Some(v) = text(&r, "on_execution_failure") {
        properties.push(prop("", "Si falla", policy_text(&v)));
    }
    if let Some(v) = text(&r, "state").filter(|v| v != "NULL") {
        properties.push(prop("", "Estado", v));
    }
    if let Some(c) = text(&r, "jobsrunning") {
        properties.push(prop("", "Jobs en curso", c));
    }
    let steps = match kind.statement(&r) {
        Some(stmt) => vec![JobStep {
            name: match kind {
                ScheduleKind::Backup => "Backup".into(),
                _ => "Changefeed".into(),
            },
            kind: Some("SQL".into()),
            target: None,
            command: Some(stmt),
        }],
        None => Vec::new(),
    };
    Ok(JobDetail {
        job,
        steps,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub(crate) async fn job_detail(s: &PgSession, n: i64) -> Result<JobDetail> {
    let who = who(s).await?;
    let r = changefeed_row(s, n).await?;
    let job = changefeed_job(&r, &who).ok_or_else(|| not_found(JobId::CrdbJob(n)))?;
    let mut properties = vec![prop("", "Id", n.to_string())];
    for (label, col) in [
        ("Estado", "status"),
        ("Detalle del estado", "running_status"),
        ("Formato", "format"),
        ("Tópicos", "topics"),
    ] {
        if let Some(v) = text(&r, col) {
            properties.push(prop("", label, v));
        }
    }
    if let Some(t) =
        text(&r, "readable_high_water_timestamptz").or_else(|| text(&r, "high_water_timestamp"))
    {
        properties.push(prop("", "Marca de agua", t));
    }
    if let Some(e) = message(&r, "error") {
        properties.push(prop("", "Error", e));
    }
    Ok(JobDetail {
        steps: vec![JobStep {
            name: "Changefeed".into(),
            kind: Some("SQL".into()),
            target: tables(&r),
            command: text(&r, "description"),
        }],
        job,
        schedules: Vec::new(),
        properties,
        notes: Vec::new(),
    })
}

pub(crate) async fn runs(s: &PgSession, n: i64, limit: u32) -> Result<Vec<JobRun>> {
    schedule_row(s, n).await?;
    schedule_runs(s, n, limit).await
}

/// The statement that runs schedule `n` now, or why it can't run now.
fn run_sql(r: &dyn Row, n: i64, who: &Who) -> Result<String> {
    let kind = ScheduleKind::of(r);
    let sql = match kind {
        ScheduleKind::Backup if who.executes_backups() => {
            // The plain form runs the pair's incremental schedule.
            let full = if incremental(r) { "" } else { " FULL" };
            format!("ALTER BACKUP SCHEDULE {n} EXECUTE{full} IMMEDIATELY")
        }
        ScheduleKind::Backup => {
            return Err(Error::Unsupported(
                "esta versión de CockroachDB no ejecuta un schedule en el momento (ALTER BACKUP SCHEDULE … EXECUTE IMMEDIATELY llegó en la 24.1)".into(),
            ))
        }
        ScheduleKind::Other => {
            return Err(Error::Unsupported(
                "CockroachDB no ejecuta en el momento este tipo de schedule".into(),
            ))
        }
        _ if who.executes_schedules() => format!("EXECUTE SCHEDULE {n}"),
        _ => {
            return Err(Error::Unsupported(
                "esta versión de CockroachDB solo ejecuta en el momento los schedules de backup (EXECUTE SCHEDULE llegó en la 26.1)".into(),
            ))
        }
    };
    if text(r, "schedule_status").as_deref() != Some("ACTIVE") {
        return Err(Error::Query(
            "el schedule está en pausa: CockroachDB no lo ejecuta hasta reanudarlo".into(),
        ));
    }
    Ok(sql)
}

pub(crate) async fn run(s: &PgSession, n: i64) -> Result<()> {
    let who = who(s).await?;
    let r = schedule_row(s, n).await?;
    s.text(&run_sql(&r, n, &who)?).await?;
    Ok(())
}

pub(crate) async fn stop_schedule(s: &PgSession, n: i64) -> Result<()> {
    schedule_row(s, n).await?;
    let going: Vec<String> = schedule_runs(s, n, LAST_JOBS)
        .await?
        .into_iter()
        .filter(|j| j.outcome == JobOutcome::Running)
        .filter_map(|j| j.id)
        .filter(|id| id.parse::<i64>().is_ok())
        .collect();
    if going.is_empty() {
        return Err(Error::Query(
            "el schedule no tiene ningún job en curso".into(),
        ));
    }
    s.text(&format!("CANCEL JOBS VALUES ({})", going.join("), (")))
        .await?;
    Ok(())
}

pub(crate) async fn cancel_job(s: &PgSession, n: i64) -> Result<()> {
    changefeed_row(s, n).await?;
    s.text(&format!("CANCEL JOB {n}")).await?;
    Ok(())
}

pub(crate) async fn set_schedule_enabled(s: &PgSession, n: i64, enabled: bool) -> Result<()> {
    schedule_row(s, n).await?;
    let verb = if enabled { "RESUME" } else { "PAUSE" };
    s.text(&format!("{verb} SCHEDULE {n}")).await?;
    Ok(())
}

pub(crate) async fn set_job_enabled(s: &PgSession, n: i64, enabled: bool) -> Result<()> {
    changefeed_row(s, n).await?;
    let verb = if enabled { "RESUME" } else { "PAUSE" };
    s.text(&format!("{verb} JOB {n}")).await?;
    Ok(())
}

pub(crate) async fn schedule_script(s: &PgSession, n: i64) -> Result<String> {
    let r = schedule_row(s, n).await?;
    if !matches!(
        ScheduleKind::of(&r),
        ScheduleKind::Backup | ScheduleKind::Changefeed
    ) {
        return Err(Error::Unsupported(
            "este schedule es de CockroachDB o de una tabla (TTL): no tiene un script propio"
                .into(),
        ));
    }
    let rows = s.text(&format!("SHOW CREATE SCHEDULE {n}")).await?;
    let stmt = rows
        .first()
        .and_then(|r| text(r, "create_statement"))
        .ok_or_else(|| not_found(JobId::CrdbSchedule(n)))?;
    Ok(script_text(
        &text(&r, "label").unwrap_or_default(),
        &stmt,
        text(&r, "schedule_status").as_deref() == Some("ACTIVE"),
    ))
}

/// The schedule's statement, with what it can't say as comments.
fn script_text(label: &str, stmt: &str, active: bool) -> String {
    let mut out = format!("-- CockroachDB: {}\n", comment_text(label));
    if stmt.contains("'*****'") {
        out.push_str(
            "-- CockroachDB oculta la ubicación (*****): completala antes de ejecutarlo.\n",
        );
    }
    out.push_str(stmt.trim_end_matches(';'));
    out.push_str(";\n");
    if !active {
        out.push_str("-- El schedule está en pausa: después de crearlo, PAUSE SCHEDULE <id>.\n");
    }
    out
}

pub(crate) async fn job_script(s: &PgSession, n: i64) -> Result<String> {
    let r = changefeed_row(s, n).await?;
    let stmt = text(&r, "description").ok_or_else(|| not_found(JobId::CrdbJob(n)))?;
    let mut out = format!("-- CockroachDB: changefeed {n}\n");
    out.push_str(stmt.trim_end_matches(';'));
    out.push_str(";\n");
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::super::fixture;
    use super::*;

    fn root() -> Who {
        Who {
            user: "root".into(),
            admin: true,
            version: Some((26, 3)),
        }
    }

    #[test]
    fn versions_read_from_the_banner() {
        assert_eq!(
            version_of("CockroachDB CCL v26.3.2 (aarch64-unknown-linux-gnu, built 2026/09/16)"),
            Some((26, 3))
        );
        assert_eq!(
            version_of("CockroachDB OSS v22.2.19 (x86_64)"),
            Some((22, 2))
        );
        assert_eq!(version_of("PostgreSQL 16"), None);
        let at = |major, minor| Who {
            version: Some((major, minor)),
            ..root()
        };
        // ALTER BACKUP SCHEDULE … EXECUTE IMMEDIATELY is 24.1, not 23.1.
        assert!(!at(23, 2).executes_backups() && at(24, 1).executes_backups());
        assert!(!at(25, 4).executes_schedules() && at(26, 1).executes_schedules());
        assert!(root().executes_backups() && root().executes_schedules());
    }

    fn backup() -> super::super::Fixture {
        fixture(&[
            ("sid", "1226891610358054913"),
            ("label", "dbine_test_jobs_backup"),
            ("schedule_status", "ACTIVE"),
            ("next_run_epoch", "1794528000"),
            ("recurrence", "@daily"),
            ("jobsrunning", "0"),
            ("owner", "app"),
            (
                "command",
                "{\"backup_statement\": \"BACKUP DATABASE d INTO 'nodelocal://1/x' WITH OPTIONS (detached)\"}",
            ),
        ])
    }

    #[test]
    fn a_backup_schedule_maps_to_a_server_job() {
        let runs = vec![
            JobRun {
                id: Some("2".into()),
                started: Some("2026-11-12T13:00:00Z".into()),
                outcome: JobOutcome::Failed,
                duration_ms: Some(20000),
                ..Default::default()
            },
            JobRun {
                id: Some("1".into()),
                outcome: JobOutcome::Succeeded,
                ..Default::default()
            },
        ];
        let j = schedule_job(&backup(), &root(), &runs).unwrap();
        assert_eq!(j.id, "crdb_schedule:1226891610358054913");
        assert_eq!(j.kind, "Schedule · backup");
        assert_eq!(j.schedule.as_deref(), Some("@daily"));
        assert_eq!(j.last_outcome, Some(JobOutcome::Failed));
        assert_eq!(j.last_duration_ms, Some(20000));
        assert_eq!(j.next_run.as_deref(), Some("2026-11-13T00:00:00Z"));
        assert!(j.description.unwrap().starts_with("BACKUP DATABASE d"));
        assert!(j.enabled && j.can_run && j.can_enable && j.has_history && j.has_script);
        assert!(!j.can_stop && !j.running);
        // Someone else's schedule, for a user who isn't admin.
        let other = Who {
            user: "bob".into(),
            admin: false,
            ..root()
        };
        let j = schedule_job(&backup(), &other, &[]).unwrap();
        assert!(!j.can_run && !j.can_enable);
        // One running.
        let going = vec![JobRun {
            id: Some("3".into()),
            started: Some("2026-11-12T14:00:00Z".into()),
            outcome: JobOutcome::Running,
            ..Default::default()
        }];
        let j = schedule_job(&backup(), &root(), &going).unwrap();
        assert!(j.running && j.can_stop && !j.can_run);
        assert_eq!(j.last_outcome, Some(JobOutcome::Running));
        assert_eq!(j.last_started.as_deref(), Some("2026-11-12T14:00:00Z"));
    }

    #[test]
    fn run_now_picks_the_statement_for_the_schedule() {
        let who = root();
        // A full backup (alone or the full one of a pair) runs with FULL:
        // the plain form would run the pair's incremental schedule.
        assert_eq!(
            run_sql(&backup(), 7, &who).unwrap(),
            "ALTER BACKUP SCHEDULE 7 EXECUTE FULL IMMEDIATELY"
        );
        let inc = |backup_type: &str| {
            fixture(&[
                ("schedule_status", "ACTIVE"),
                (
                    "command",
                    &format!(
                        "{{\"backup_statement\": \"BACKUP INTO LATEST IN 'x'\", \"backup_type\": {backup_type}}}"
                    ),
                ),
            ])
        };
        assert!(incremental(&inc("1")) && incremental(&inc("\"INCREMENTAL\"")));
        assert!(!incremental(&inc("0")) && !incremental(&backup()));
        assert_eq!(
            run_sql(&inc("1"), 8, &who).unwrap(),
            "ALTER BACKUP SCHEDULE 8 EXECUTE IMMEDIATELY"
        );
        // Other schedules: EXECUTE SCHEDULE, from 26.1.
        let cf = fixture(&[
            ("schedule_status", "ACTIVE"),
            (
                "command",
                "{\"changefeed_statement\": \"CREATE CHANGEFEED FOR t INTO 'null://'\"}",
            ),
        ]);
        assert_eq!(run_sql(&cf, 9, &who).unwrap(), "EXECUTE SCHEDULE 9");
        let older = Who {
            version: Some((25, 2)),
            ..root()
        };
        assert!(matches!(
            run_sql(&cf, 9, &older),
            Err(Error::Unsupported(_))
        ));
        assert!(run_sql(&backup(), 7, &older).is_ok());
        let oldest = Who {
            version: Some((23, 2)),
            ..root()
        };
        assert!(matches!(
            run_sql(&backup(), 7, &oldest),
            Err(Error::Unsupported(m)) if m.contains("24.1")
        ));
        // Paused, or a kind DBine doesn't know.
        let paused = fixture(&[
            ("schedule_status", "PAUSED"),
            ("command", "{\"backup_statement\": \"BACKUP INTO 'x'\"}"),
        ]);
        assert!(matches!(run_sql(&paused, 7, &who), Err(Error::Query(_))));
        let other = fixture(&[
            ("schedule_status", "ACTIVE"),
            ("owner", "app"),
            ("command", "{}"),
        ]);
        assert!(matches!(
            run_sql(&other, 7, &who),
            Err(Error::Unsupported(_))
        ));
    }

    #[test]
    fn other_schedules_dont_run_now_or_script() {
        let ttl = fixture(&[
            ("sid", "5"),
            ("label", "row-level-ttl: t [104]"),
            ("schedule_status", "PAUSED"),
            ("next_run_epoch", "1794528000"),
            ("owner", "root"),
            ("command", "{\"tableId\": 104}"),
        ]);
        let j = schedule_job(&ttl, &root(), &[]).unwrap();
        assert_eq!(j.kind, "Schedule · TTL");
        assert!(!j.enabled && !j.can_run && !j.has_script && j.next_run.is_none());
        // Active, it runs now with EXECUTE SCHEDULE (26.1+).
        let active = fixture(&[
            ("sid", "5"),
            ("label", "row-level-ttl: t [104]"),
            ("schedule_status", "ACTIVE"),
            ("owner", "root"),
            ("command", "{\"tableId\": 104}"),
        ]);
        assert!(schedule_job(&active, &root(), &[]).unwrap().can_run);
        let v25 = Who {
            version: Some((25, 4)),
            ..root()
        };
        assert!(!schedule_job(&active, &v25, &[]).unwrap().can_run);
        let compaction = fixture(&[
            ("sid", "6"),
            ("label", "sql-stats-compaction"),
            ("owner", "node"),
            ("command", "{}"),
        ]);
        assert_eq!(ScheduleKind::of(&compaction), ScheduleKind::System);
        assert!(schedule_job(&compaction, &root(), &[]).unwrap().system);
        assert!(!schedule_job(&ttl, &root(), &[]).unwrap().system);
        let cf = fixture(&[(
            "command",
            "{\"changefeed_statement\": \"CREATE CHANGEFEED FOR t INTO 'null://'\"}",
        )]);
        assert_eq!(ScheduleKind::of(&cf), ScheduleKind::Changefeed);
        assert_eq!(
            ScheduleKind::Changefeed.statement(&cf).as_deref(),
            Some("CREATE CHANGEFEED FOR t INTO 'null://'")
        );
    }

    #[test]
    fn a_changefeed_maps_to_a_server_job() {
        let cf = fixture(&[
            ("jid", "1226892742079512577"),
            (
                "description",
                "CREATE CHANGEFEED FOR TABLE d.t INTO 'null:'",
            ),
            ("status", "running"),
            ("user_name", "root"),
            ("tables", "{d.public.t,d.public.u}"),
            ("created_epoch", "1794488351"),
            ("database_name", "d"),
        ]);
        let j = changefeed_job(&cf, &root()).unwrap();
        assert_eq!(j.id, "crdb_job:1226892742079512577");
        assert_eq!(j.name, "d.public.t, d.public.u");
        assert_eq!(j.kind, "Changefeed");
        assert!(j.enabled && j.running && j.can_stop && j.can_enable && j.has_script);
        assert!(!j.can_run && !j.has_history);
        let paused = fixture(&[("jid", "1"), ("status", "paused"), ("user_name", "root")]);
        let p = changefeed_job(&paused, &root()).unwrap();
        assert!(!p.enabled && !p.running && p.can_enable && p.can_stop);
        let ending = fixture(&[
            ("jid", "1"),
            ("status", "cancel-requested"),
            ("user_name", "root"),
        ]);
        let e = changefeed_job(&ending, &root()).unwrap();
        assert!(!e.can_stop && !e.can_enable);
        let done = fixture(&[("jid", "1"), ("status", "canceled"), ("user_name", "root")]);
        let d = changefeed_job(&done, &root()).unwrap();
        assert!(!d.enabled && !d.can_stop && !d.can_enable);
        assert_eq!(d.last_outcome, Some(JobOutcome::Canceled));
    }

    #[test]
    fn statuses_map_to_outcomes() {
        assert_eq!(outcome(Some("succeeded")), JobOutcome::Succeeded);
        assert_eq!(outcome(Some("revert-failed")), JobOutcome::Failed);
        assert_eq!(outcome(Some("canceled")), JobOutcome::Canceled);
        assert_eq!(outcome(Some("pending")), JobOutcome::Running);
        assert_eq!(outcome(Some("paused")), JobOutcome::Unknown);
    }

    #[test]
    fn scripts_say_what_the_statement_hides() {
        let sql = script_text(
            "nightly",
            "CREATE SCHEDULE 'nightly' FOR BACKUP INTO '*****' RECURRING '@daily'",
            false,
        );
        assert!(sql.contains("oculta la ubicación"));
        assert!(sql.contains("RECURRING '@daily';\n"));
        assert!(sql.contains("en pausa"));
        let label = script_text("a\nDROP TABLE x", "CREATE SCHEDULE x", true);
        assert!(label.starts_with("-- CockroachDB: a?DROP TABLE x\n"));
    }

    #[test]
    fn statements_take_only_numbers() {
        assert!(schedules_sql(Some(5)).contains("WHERE id = 5"));
        assert!(changefeeds_sql(Some(7)).contains("WHERE job_id = 7"));
        assert!(changefeeds_sql(None).contains("NOT IN ('succeeded'"));
        assert!(jobs_of_schedule_sql(9, 3).contains("[SHOW JOBS FOR SCHEDULE 9]"));
    }
}
