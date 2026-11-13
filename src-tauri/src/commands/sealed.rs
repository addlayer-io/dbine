//! The UI's encrypted local store (crate::sealed_store): what it keeps
//! between sessions that can quote database content, such as the AI
//! assistant's conversations. Never localStorage.

use crate::error::{CommandError, CommandResult};
use crate::sealed_store::{self, Read, Slot};
use serde::Deserialize;

async fn blocking<T: Send + 'static>(
    f: impl FnOnce() -> CommandResult<T> + Send + 'static,
) -> CommandResult<T> {
    tokio::task::spawn_blocking(f)
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
}

#[derive(Deserialize)]
pub struct SlotArgs {
    pub slot: Slot,
}

/// What `slot` holds, and whether there's a vault to keep anything with.
#[tauri::command(rename_all = "camelCase")]
pub async fn sealed_get(args: SlotArgs) -> CommandResult<Read> {
    blocking(move || Ok(sealed_store::get(args.slot))).await
}

#[derive(Deserialize)]
pub struct SetArgs {
    pub slot: Slot,
    pub value: String,
}

/// Keep `value` in `slot`, encrypted. `false`: this system has no vault,
/// so nothing was written (the UI keeps it in memory).
#[tauri::command(rename_all = "camelCase")]
pub async fn sealed_set(args: SetArgs) -> CommandResult<bool> {
    blocking(move || sealed_store::set(args.slot, &args.value)).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sealed_delete(args: SlotArgs) -> CommandResult<()> {
    blocking(move || {
        sealed_store::delete(args.slot);
        Ok(())
    })
    .await
}
