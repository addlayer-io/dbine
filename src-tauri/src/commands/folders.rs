use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::ConnectionFolder;
use serde::Deserialize;
use tauri::State;

#[tauri::command]
pub async fn list_folders(state: State<'_, AppState>) -> CommandResult<Vec<ConnectionFolder>> {
    Ok(state.store.list_folders()?)
}

#[derive(Deserialize)]
pub struct SaveFolderArgs {
    pub folder: ConnectionFolder,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn save_folder(state: State<'_, AppState>, args: SaveFolderArgs) -> CommandResult<ConnectionFolder> {
    let mut f = args.folder;
    if f.id.is_empty() {
        f.id = uuid::Uuid::new_v4().to_string();
    }
    f.name = f.name.trim().to_string();
    if f.name.is_empty() {
        return Err(CommandError::BadRequest("la carpeta necesita un nombre".into()));
    }
    Ok(state.store.save_folder(&f)?)
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

/// Its connections and subfolders move up to its parent.
#[tauri::command(rename_all = "camelCase")]
pub async fn delete_folder(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    Ok(state.store.delete_folder(&args.id)?)
}

#[derive(Deserialize)]
pub struct MoveConnectionArgs {
    pub connection_id: String,
    /// `None` = top level.
    pub folder_id: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn move_connection(state: State<'_, AppState>, args: MoveConnectionArgs) -> CommandResult<()> {
    Ok(state.store.move_connection(&args.connection_id, args.folder_id.as_deref())?)
}
