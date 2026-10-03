use crate::error::CommandResult;
use crate::state::AppState;
use dbine_driver::{Error, MonitorSnapshot};
use serde::Deserialize;
use tauri::State;

#[derive(Deserialize)]
pub struct MonitorArgs {
    pub connection_id: String,
}

/// One snapshot for the monitor dashboard. The dashboard polls every few
/// seconds, so it gets its own session: polling never waits behind the
/// explorer or a running query.
#[tauri::command(rename_all = "camelCase")]
pub async fn monitor_snapshot(state: State<'_, AppState>, args: MonitorArgs) -> CommandResult<MonitorSnapshot> {
    let key = format!("monitor:{}", args.connection_id);
    let entry = state.session(&key, &args.connection_id, "").await?;
    let result = entry.session.lock().await.monitor().await;
    if matches!(result, Err(Error::Connect(_) | Error::Io(_))) {
        // A dropped connection would fail every poll: reconnect next time.
        state.sessions.remove(&key);
    }
    Ok(result?)
}

/// The sessions in blocking chains (the Monitor's "Bloqueos"), on the
/// monitor's own session.
#[tauri::command(rename_all = "camelCase")]
pub async fn monitor_blocking(state: State<'_, AppState>, args: MonitorArgs) -> CommandResult<Vec<dbine_driver::BlockedSession>> {
    let key = format!("monitor:{}", args.connection_id);
    let entry = state.session(&key, &args.connection_id, "").await?;
    let result = entry.session.lock().await.blocking().await;
    if matches!(result, Err(Error::Connect(_) | Error::Io(_))) {
        state.sessions.remove(&key);
    }
    Ok(result?)
}

#[derive(Deserialize)]
pub struct KillSessionArgs {
    pub connection_id: String,
    /// The engine's session id, as `monitor_blocking` reported it.
    pub id: String,
}

/// End a server session (the user confirmed it). Refused on read-only
/// connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn monitor_kill_session(state: State<'_, AppState>, args: KillSessionArgs) -> CommandResult<()> {
    let key = format!("monitor:{}", args.connection_id);
    let entry = state.session(&key, &args.connection_id, "").await?;
    let result = entry.session.lock().await.kill_session(&args.id).await;
    Ok(result?)
}

/// The server's sessions (the Monitor's "Procesos"). The list polls on its
/// own session, so it never waits behind a dashboard snapshot.
#[tauri::command(rename_all = "camelCase")]
pub async fn monitor_processes(state: State<'_, AppState>, args: MonitorArgs) -> CommandResult<Vec<dbine_driver::ServerProcess>> {
    let key = format!("processes:{}", args.connection_id);
    let entry = state.session(&key, &args.connection_id, "").await?;
    let result = entry.session.lock().await.processes().await;
    if matches!(result, Err(Error::Connect(_) | Error::Io(_))) {
        state.sessions.remove(&key);
    }
    Ok(result?)
}

/// Stop another session's statement and leave the session open (the user
/// confirmed it). Refused on read-only connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn monitor_cancel_query(state: State<'_, AppState>, args: KillSessionArgs) -> CommandResult<()> {
    let key = format!("monitor:{}", args.connection_id);
    let entry = state.session(&key, &args.connection_id, "").await?;
    let result = entry.session.lock().await.cancel_query(&args.id).await;
    Ok(result?)
}
