//! The results store's commands (crate::results_store, docs/results-store.md):
//! what a restored editor reads back, and what the editor keeps or forgets.
//! Every one answers as if the store were empty when it isn't available:
//! the editor then keeps its results in memory only, as before.

use crate::error::CommandResult;
use crate::results_store::{self, RunMeta, Saved, Status, DEFAULT_MAX_BYTES};
use dbine_driver::QueryOutcome;
use serde::Deserialize;

/// What an editor run asks the store to keep (`execute_query`'s `store`).
#[derive(Deserialize, Clone, Debug)]
pub struct StoreArgs {
    pub run: RunMeta,
    /// The space all stored results may take (Settings).
    #[serde(default)]
    pub max_bytes: Option<u64>,
}

impl StoreArgs {
    pub fn max_bytes(&self) -> u64 {
        self.max_bytes
            .filter(|b| *b > 0)
            .unwrap_or(DEFAULT_MAX_BYTES)
    }
}

#[tauri::command(rename_all = "camelCase")]
pub async fn results_status() -> CommandResult<Status> {
    results_store::status().await
}

#[derive(Deserialize)]
pub struct TabArgs {
    pub tab_id: String,
}

/// The stored runs of an editor tab, oldest first (their rows aren't read).
#[tauri::command(rename_all = "camelCase")]
pub async fn results_runs(args: TabArgs) -> CommandResult<Vec<RunMeta>> {
    results_store::runs(args.tab_id).await
}

#[derive(Deserialize)]
pub struct RunArgs {
    pub id: String,
}

/// A stored run's outcome, rows included. `null`: not stored.
#[tauri::command(rename_all = "camelCase")]
pub async fn results_outcome(args: RunArgs) -> CommandResult<Option<QueryOutcome>> {
    results_store::outcome(args.id).await
}

#[derive(Deserialize)]
pub struct SaveArgs {
    #[serde(flatten)]
    pub store: StoreArgs,
    pub outcome: QueryOutcome,
}

/// Keep an outcome the editor put together itself (a run on several
/// databases) or changed (saved edits). `null`: no store.
#[tauri::command(rename_all = "camelCase")]
pub async fn results_save(args: SaveArgs) -> CommandResult<Option<Saved>> {
    let max = args.store.max_bytes();
    let started = results_store::generation();
    results_store::save(args.store.run, args.outcome, max, started).await
}

#[derive(Deserialize)]
pub struct PinArgs {
    pub id: String,
    pub pinned: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn results_pin(args: PinArgs) -> CommandResult<()> {
    results_store::set_pinned(args.id, args.pinned).await
}

#[derive(Deserialize)]
pub struct DeleteArgs {
    pub ids: Vec<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn results_delete(args: DeleteArgs) -> CommandResult<()> {
    results_store::delete(args.ids).await
}

/// An editor tab closed: its results go.
#[tauri::command(rename_all = "camelCase")]
pub async fn results_delete_tab(args: TabArgs) -> CommandResult<()> {
    results_store::delete_tab(args.tab_id).await
}

#[derive(Deserialize)]
pub struct KeepTabsArgs {
    pub tab_ids: Vec<String>,
}

/// At start: the results of editor tabs that are gone go.
#[tauri::command(rename_all = "camelCase")]
pub async fn results_keep_tabs(args: KeepTabsArgs) -> CommandResult<()> {
    results_store::keep_tabs(args.tab_ids).await
}

/// "Borrar resultados guardados", or keeping them turned off.
#[tauri::command(rename_all = "camelCase")]
pub async fn results_clear() -> CommandResult<()> {
    results_store::clear().await
}
