//! Saved migrations: the "Migraciones" node of a database in the explorer (docs/migracion.md).
//! Each entry keeps the Migrate screen's configuration and the ids of the runs started from it;
//! the runs themselves are the migration records (`migration_runs`).

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::SavedMigration;
use serde::Deserialize;
use tauri::State;

#[derive(Deserialize)]
pub struct ListArgs {
    pub connection_id: String,
    pub database: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn list_saved_migrations(state: State<'_, AppState>, args: ListArgs) -> CommandResult<Vec<SavedMigration>> {
    Ok(state.store.list_migrations(&args.connection_id, &args.database)?)
}

#[derive(Deserialize)]
pub struct IdArgs {
    pub id: String,
}

fn gone() -> CommandError {
    CommandError::NotFound("la migración ya no existe".into())
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_saved_migration(state: State<'_, AppState>, args: IdArgs) -> CommandResult<SavedMigration> {
    state.store.get_migration(&args.id)?.ok_or_else(gone)
}

#[derive(Deserialize)]
pub struct SaveArgs {
    pub migration: SavedMigration,
}

/// Create or update (the UI picks the id, so a draft's tab knows it before its first save).
#[tauri::command(rename_all = "camelCase")]
pub async fn save_saved_migration(state: State<'_, AppState>, args: SaveArgs) -> CommandResult<SavedMigration> {
    let mut m = args.migration;
    if m.id.is_empty() {
        m.id = uuid::Uuid::new_v4().to_string();
    }
    if m.name.trim().is_empty() {
        return Err(CommandError::BadRequest("la migración necesita un nombre".into()));
    }
    Ok(state.store.save_migration(&m)?)
}

#[derive(Deserialize)]
pub struct RenameArgs {
    pub id: String,
    pub name: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn rename_saved_migration(state: State<'_, AppState>, args: RenameArgs) -> CommandResult<SavedMigration> {
    let name = args.name.trim();
    if name.is_empty() {
        return Err(CommandError::BadRequest("la migración necesita un nombre".into()));
    }
    if state.store.get_migration(&args.id)?.is_none() {
        return Err(gone());
    }
    Ok(state.store.rename_migration(&args.id, name)?)
}

#[derive(Deserialize)]
pub struct LinkArgs {
    pub id: String,
    pub run_id: String,
}

/// A run started from it (run, resume or retry): it becomes the current one.
#[tauri::command(rename_all = "camelCase")]
pub async fn link_saved_migration_run(state: State<'_, AppState>, args: LinkArgs) -> CommandResult<SavedMigration> {
    if state.store.get_migration(&args.id)?.is_none() {
        return Err(gone());
    }
    Ok(state.store.link_migration_run(&args.id, &args.run_id)?)
}

#[derive(Deserialize)]
pub struct DuplicateArgs {
    pub id: String,
    pub name: String,
}

/// The same configuration as a new draft (without runs).
#[tauri::command(rename_all = "camelCase")]
pub async fn duplicate_saved_migration(state: State<'_, AppState>, args: DuplicateArgs) -> CommandResult<SavedMigration> {
    if state.store.get_migration(&args.id)?.is_none() {
        return Err(gone());
    }
    let id = uuid::Uuid::new_v4().to_string();
    Ok(state.store.duplicate_migration(&args.id, &id, args.name.trim())?)
}

/// Remove the entry only: the source, the target and the runs' records stay.
#[tauri::command(rename_all = "camelCase")]
pub async fn delete_saved_migration(state: State<'_, AppState>, args: IdArgs) -> CommandResult<()> {
    Ok(state.store.delete_migration(&args.id)?)
}
