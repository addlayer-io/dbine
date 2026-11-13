//! The UI's encrypted local store (crate::sealed_store): what it keeps
//! between sessions that can quote database content, such as the AI
//! assistant's conversations and untitled notebooks' drafts. Never
//! localStorage.

use crate::error::{CommandError, CommandResult};
use crate::sealed_store::{self, Item, Read, Slot};
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
    /// The tab, for a per-tab slot (a notebook's draft).
    #[serde(default)]
    pub id: Option<String>,
}

/// What the slot holds, and whether there's a vault to keep anything with.
#[tauri::command(rename_all = "camelCase")]
pub async fn sealed_get(args: SlotArgs) -> CommandResult<Read> {
    let item = Item::new(args.slot, args.id)?;
    blocking(move || Ok(sealed_store::get(&item))).await
}

#[derive(Deserialize)]
pub struct SetArgs {
    pub slot: Slot,
    #[serde(default)]
    pub id: Option<String>,
    pub value: String,
}

/// Keep `value` in the slot, encrypted. `false`: this system has no vault,
/// so nothing was written (the UI keeps it in memory).
#[tauri::command(rename_all = "camelCase")]
pub async fn sealed_set(args: SetArgs) -> CommandResult<bool> {
    let item = Item::new(args.slot, args.id)?;
    blocking(move || sealed_store::set(&item, &args.value)).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn sealed_delete(args: SlotArgs) -> CommandResult<()> {
    let item = Item::new(args.slot, args.id)?;
    blocking(move || {
        sealed_store::delete(&item);
        Ok(())
    })
    .await
}

#[derive(Deserialize)]
pub struct PruneArgs {
    pub slot: Slot,
    /// The tabs whose files stay.
    pub keep: Vec<String>,
}

/// Delete a per-tab slot's files of the tabs not in `keep`.
#[tauri::command(rename_all = "camelCase")]
pub async fn sealed_prune(args: PruneArgs) -> CommandResult<()> {
    blocking(move || {
        sealed_store::prune(args.slot, &args.keep);
        Ok(())
    })
    .await
}
