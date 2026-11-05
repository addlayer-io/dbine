//! The AI assistant's contexts: the "Contextos" node of a connection and of each database
//! (docs/ai-assistant.md). `commands/ai.rs` adds the active ones to new chats.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::SavedContext;
use serde::Deserialize;
use tauri::State;

#[derive(Deserialize)]
pub struct ListArgs {
    pub connection_id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn list_contexts(
    state: State<'_, AppState>,
    args: ListArgs,
) -> CommandResult<Vec<SavedContext>> {
    Ok(state.store.list_contexts(&args.connection_id)?)
}

#[derive(Deserialize)]
pub struct SaveArgs {
    pub context: SavedContext,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_context(
    state: State<'_, AppState>,
    args: SaveArgs,
) -> CommandResult<SavedContext> {
    let mut x = args.context;
    if x.id.is_empty() {
        x.id = uuid::Uuid::new_v4().to_string();
    }
    if x.name.trim().is_empty() {
        return Err(CommandError::BadRequest(
            "el contexto necesita un nombre".into(),
        ));
    }
    Ok(state.store.save_context(&x)?)
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn delete_context(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    Ok(state.store.delete_context(&args.id)?)
}
