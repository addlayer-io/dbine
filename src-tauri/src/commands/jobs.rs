//! The server's jobs ("Programadas › En los servidores", docs/server-jobs.md):
//! SQL Server Agent jobs, DBMS_SCHEDULER jobs, Snowflake tasks, Databricks
//! jobs, pg_cron… Each connection reads them on its own session, so the
//! list never waits behind the explorer or a running query.

use crate::error::CommandResult;
use crate::state::AppState;
use dbine_driver::jobs::MAX_JOB_RUNS;
use dbine_driver::{Error, JobDetail, JobList, JobRun};
use serde::Deserialize;
use tauri::State;

#[derive(Deserialize)]
pub struct JobsArgs {
    pub connection_id: String,
}

#[derive(Deserialize)]
pub struct JobArgs {
    pub connection_id: String,
    /// The job's id as `jobs_list` reported it.
    pub id: String,
}

#[derive(Deserialize)]
pub struct JobRunsArgs {
    pub connection_id: String,
    pub id: String,
    pub limit: u32,
}

#[derive(Deserialize)]
pub struct JobEnabledArgs {
    pub connection_id: String,
    pub id: String,
    pub enabled: bool,
}

/// Runs one call on the connection's jobs session. A dropped connection
/// would fail every refresh: the session is opened again next time.
macro_rules! on_jobs_session {
    ($state:expr, $conn:expr, |$s:ident| $call:expr) => {{
        let key = format!("jobs:{}", $conn);
        let entry = $state.session(&key, $conn, "").await?;
        let result = {
            let mut $s = entry.session.lock().await;
            $call.await
        };
        if matches!(
            result,
            Err(Error::Connect(_) | Error::ConnectFailed(_) | Error::Io(_))
        ) {
            $state.sessions.remove(&key);
        }
        result
    }};
}

/// The connection's jobs, with the notes that explain an empty or partial
/// list.
#[tauri::command(rename_all = "camelCase")]
pub async fn jobs_list(state: State<'_, AppState>, args: JobsArgs) -> CommandResult<JobList> {
    Ok(on_jobs_session!(state, &args.connection_id, |s| s.jobs())?)
}

/// One job whole: steps, schedules and the rest the engine says.
#[tauri::command(rename_all = "camelCase")]
pub async fn jobs_detail(state: State<'_, AppState>, args: JobArgs) -> CommandResult<JobDetail> {
    Ok(on_jobs_session!(state, &args.connection_id, |s| s.job_detail(&args.id))?)
}

/// A job's runs, newest first.
#[tauri::command(rename_all = "camelCase")]
pub async fn jobs_runs(
    state: State<'_, AppState>,
    args: JobRunsArgs,
) -> CommandResult<Vec<JobRun>> {
    let limit = args.limit.clamp(1, MAX_JOB_RUNS);
    Ok(on_jobs_session!(state, &args.connection_id, |s| s.job_runs(&args.id, limit))?)
}

/// Start a run now (the user confirmed it). Refused on read-only
/// connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn jobs_run(state: State<'_, AppState>, args: JobArgs) -> CommandResult<()> {
    Ok(on_jobs_session!(state, &args.connection_id, |s| s.run_job(&args.id))?)
}

/// Stop the job's current run (the user confirmed it). Refused on read-only
/// connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn jobs_stop(state: State<'_, AppState>, args: JobArgs) -> CommandResult<()> {
    Ok(on_jobs_session!(state, &args.connection_id, |s| s.stop_job(&args.id))?)
}

/// Enable or disable the job's schedule (the user confirmed it). Refused on
/// read-only connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn jobs_set_enabled(
    state: State<'_, AppState>,
    args: JobEnabledArgs,
) -> CommandResult<()> {
    Ok(on_jobs_session!(state, &args.connection_id, |s| s
        .set_job_enabled(&args.id, args.enabled))?)
}

/// The script that recreates the job, for a new query tab.
#[tauri::command(rename_all = "camelCase")]
pub async fn jobs_script(state: State<'_, AppState>, args: JobArgs) -> CommandResult<String> {
    Ok(on_jobs_session!(state, &args.connection_id, |s| s.job_script(&args.id))?)
}
