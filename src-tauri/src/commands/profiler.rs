//! The profiler (docs/engine-support.md): every statement run against a
//! database, live. Each open profiler tab has its own session (key
//! `profiler:<id>`), started once and then polled by the tab; stopping it
//! puts back any server setting the driver switched on.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::{ProfiledStatement, ProfilerOptions, ProfilerStarted};
use serde::Deserialize;
use tauri::State;

fn key(id: &str) -> String {
    format!("profiler:{id}")
}

#[derive(Deserialize)]
pub struct StartArgs {
    /// Chosen by the UI (the tab's id).
    pub profiler_id: String,
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub profiler_id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn profiler_start(state: State<'_, AppState>, args: StartArgs) -> CommandResult<ProfilerStarted> {
    stop(&state, &args.profiler_id).await;
    let key = key(&args.profiler_id);
    // Server settings switched on by one profiler would be put back by the
    // other's stop while the first still runs: one per connection.
    let busy = state.sessions.iter().find(|e| e.key().starts_with("profiler:") && e.connection_id == args.connection_id).map(|e| e.database.clone());
    if let Some(db) = busy {
        let on = if db.is_empty() { String::new() } else { format!(" (sobre «{db}»)") };
        return Err(CommandError::BadRequest(format!(
            "ya hay un profiler capturando en esta conexión{on}: detenelo antes de iniciar otro"
        )));
    }
    let entry = state.session(&key, &args.connection_id, &args.database).await?;
    // A read-only connection's session refuses to change server settings.
    let opts = ProfilerOptions { database: args.database.clone(), change_server: true };
    let started = entry.session.lock().await.profiler_start(&opts).await;
    if started.is_err() {
        state.sessions.remove(&key);
    }
    Ok(started?)
}

/// What arrived since the last poll.
#[tauri::command(rename_all = "camelCase")]
pub async fn profiler_poll(state: State<'_, AppState>, args: IdArgs) -> CommandResult<Vec<ProfiledStatement>> {
    let entry = state
        .sessions
        .get(&key(&args.profiler_id))
        .map(|e| e.clone())
        .ok_or_else(|| CommandError::BadRequest("el profiler no está iniciado".into()))?;
    let polled = entry.session.lock().await.profiler_poll().await;
    Ok(polled?)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn profiler_stop(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    stop(&state, &args.profiler_id).await;
    Ok(())
}

async fn stop(state: &AppState, id: &str) {
    if let Some((_, entry)) = state.sessions.remove(&key(id)) {
        if let Err(e) = entry.session.lock().await.profiler_stop().await {
            tracing::warn!(%e, "profiler stop");
        }
    }
}

/// Stop every running profiler (the app is closing): server settings go
/// back to how they were.
pub async fn stop_all(state: &AppState) {
    let ids: Vec<String> =
        state.sessions.iter().filter_map(|e| e.key().strip_prefix("profiler:").map(str::to_string)).collect();
    for id in ids {
        stop(state, &id).await;
    }
}
