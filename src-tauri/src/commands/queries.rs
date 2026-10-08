use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::{QueryVersion, SavedQuery};
use serde::Deserialize;
use tauri::State;

#[derive(Deserialize)]
pub struct ListArgs {
    pub connection_id: String,
    pub database: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn list_queries(state: State<'_, AppState>, args: ListArgs) -> CommandResult<Vec<SavedQuery>> {
    Ok(state.store.list_queries(&args.connection_id, &args.database)?)
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_query(state: State<'_, AppState>, args: IdArgs) -> CommandResult<SavedQuery> {
    state.store.get_query(&args.id)?.ok_or_else(|| CommandError::NotFound("la query ya no existe".into()))
}

#[derive(Deserialize)]
pub struct SaveArgs {
    pub query: SavedQuery,
    /// Always keep the text as a version (an explicit save); otherwise at
    /// most one version a minute while typing.
    #[serde(default)]
    pub checkpoint: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_query(state: State<'_, AppState>, args: SaveArgs) -> CommandResult<SavedQuery> {
    let mut q = args.query;
    if q.id.is_empty() {
        q.id = uuid::Uuid::new_v4().to_string();
    }
    if q.name.trim().is_empty() {
        return Err(CommandError::BadRequest("la query necesita un nombre".into()));
    }
    let before = state.store.get_query(&q.id)?;
    let saved = state.store.save_query(&q)?;
    // The timeline never fails a save.
    if let Err(e) = state.store.version_query_save(&q.id, before.as_ref(), &q.sql, args.checkpoint) {
        tracing::warn!(%e, "could not record the query's version");
    }
    Ok(saved)
}

// -- versions (the query tab's timeline, docs/historial.md) ------------------------------

#[derive(Deserialize)]
pub struct VersionsArgs {
    pub query_id: String,
}

/// The saved query's versions, newest first (without their text).
#[tauri::command(rename_all = "camelCase")]
pub async fn query_versions(state: State<'_, AppState>, args: VersionsArgs) -> CommandResult<Vec<QueryVersion>> {
    Ok(state.store.list_query_versions(&args.query_id)?)
}

#[derive(Deserialize)]
pub struct VersionArgs {
    pub id: i64,
}

/// One version, with its text.
#[tauri::command(rename_all = "camelCase")]
pub async fn query_version(state: State<'_, AppState>, args: VersionArgs) -> CommandResult<QueryVersion> {
    state.store.get_query_version(args.id)?.ok_or_else(|| CommandError::NotFound("esa versión ya no existe".into()))
}

/// Keep the saved query's current text as a version (closing its tab,
/// running it, before restoring an older one). `None` when it already is.
#[tauri::command(rename_all = "camelCase")]
pub async fn query_version_checkpoint(state: State<'_, AppState>, args: VersionsArgs) -> CommandResult<Option<QueryVersion>> {
    let q = state.store.get_query(&args.query_id)?.ok_or_else(|| CommandError::NotFound("la query ya no existe".into()))?;
    Ok(state.store.add_query_version(&q.id, &q.sql, &chrono::Utc::now().to_rfc3339(), None)?)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn delete_query(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    Ok(state.store.delete_query(&args.id)?)
}
