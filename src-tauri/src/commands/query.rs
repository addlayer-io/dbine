use crate::error::CommandResult;
use crate::state::AppState;
use dbine_driver::QueryOutcome;
use serde::Deserialize;
use std::sync::Arc;
use tauri::State;

/// Rows kept per result set when the UI doesn't say.
const DEFAULT_MAX_ROWS: usize = 5_000;

#[derive(Deserialize)]
pub struct ExecuteArgs {
    /// The editor tab's session: its connection persists between runs.
    pub session_id: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    pub max_rows: Option<usize>,
    /// Saved query being run, to record when it last ran.
    pub query_id: Option<String>,
    #[serde(default)]
    pub plan: PlanMode,
    /// Keep it in the history (what the user ran from the editor; not a
    /// table's data being browsed).
    #[serde(default)]
    pub record: bool,
}

#[derive(Deserialize, Default, Clone, Copy, PartialEq)]
#[serde(rename_all = "snake_case")]
pub enum PlanMode {
    /// Just run it.
    #[default]
    None,
    /// The estimated plan; nothing runs.
    Estimated,
    /// Run it and bring the actual plan with the results.
    Actual,
}

/// Run a script. Server errors don't fail the call: they come back in
/// `error` next to whatever ran before them. Connection problems do fail it.
#[tauri::command(rename_all = "camelCase")]
pub async fn execute_query(state: State<'_, AppState>, args: ExecuteArgs) -> CommandResult<QueryOutcome> {
    let entry = state.session(&args.session_id, &args.connection_id, &args.database).await?;
    if let Some(id) = &args.query_id {
        if let Err(e) = state.store.mark_query_run(id) {
            tracing::warn!(%e, "could not record query run");
        }
    }
    let started = std::time::Instant::now();
    let started_at = chrono::Utc::now().to_rfc3339();
    let max_rows = args.max_rows.unwrap_or(DEFAULT_MAX_ROWS).max(1);
    let mut out = QueryOutcome::default();
    let mut session = entry.session.lock().await;
    let run = async {
        match args.plan {
            PlanMode::None => session.execute(&args.sql, max_rows, &mut out).await,
            PlanMode::Estimated => session.explain(&args.sql, false, max_rows, &mut out).await,
            PlanMode::Actual => session.explain(&args.sql, true, max_rows, &mut out).await,
        }
    };
    let finished = tokio::select! {
        r = run => Some(r),
        _ = entry.cancel.notified() => None,
    };
    drop(session);
    match finished {
        Some(Ok(())) => {}
        Some(Err(e)) => out.error = Some(e.to_string()),
        None => {
            // The statement was dropped mid-flight: the connection is in an
            // unknown state, so the next run opens a new one.
            forget_session(&state, &args.session_id, &entry);
            out.error = Some("Ejecución cancelada.".into());
        }
    }
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    if args.record {
        crate::commands::history::record(&state, &args.connection_id, &args.database, &args.sql, started_at, &out);
    }
    Ok(out)
}

fn forget_session(state: &AppState, key: &str, entry: &Arc<crate::state::SessionEntry>) {
    state.sessions.remove_if(key, |_, e| Arc::ptr_eq(e, entry));
}

#[derive(Deserialize)]
pub struct SessionArgs {
    pub session_id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn cancel_query(state: State<'_, AppState>, args: SessionArgs) -> CommandResult<()> {
    if let Some(entry) = state.sessions.get(&args.session_id).map(|e| e.clone()) {
        if let Some(interrupt) = &entry.interrupter {
            interrupt();
        }
        entry.cancelled.store(true, std::sync::atomic::Ordering::SeqCst);
        entry.cancel.notify_waiters();
    }
    Ok(())
}

/// The tab closed: release its connection.
#[tauri::command(rename_all = "camelCase")]
pub async fn close_session(state: State<'_, AppState>, args: SessionArgs) -> CommandResult<()> {
    if let Some((_, entry)) = state.sessions.remove(&args.session_id) {
        entry.cancel.notify_waiters();
    }
    Ok(())
}
