use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::SavedQuery;
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
    Ok(state.store.save_query(&q)?)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn delete_query(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    Ok(state.store.delete_query(&args.id)?)
}
