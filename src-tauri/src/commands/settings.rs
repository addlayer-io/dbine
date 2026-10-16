//! User preferences (synced with the cloud backup): a key → JSON map.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::state::LOCAL_PREFIX;
use serde::Deserialize;
use serde_json::Value;
use std::collections::BTreeMap;
use tauri::State;

#[tauri::command]
pub async fn list_settings(state: State<'_, AppState>) -> CommandResult<BTreeMap<String, Value>> {
    Ok(state.store.list_settings()?)
}

#[derive(Deserialize)]
pub struct SetSettingArgs {
    pub key: String,
    /// `null` removes it (back to the default).
    pub value: Option<Value>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn set_setting(state: State<'_, AppState>, args: SetSettingArgs) -> CommandResult<()> {
    if args.key.starts_with(LOCAL_PREFIX) || args.key.is_empty() {
        return Err(CommandError::BadRequest("clave de configuración inválida".into()));
    }
    if args.key == "telemetry.consent" {
        super::telemetry::OPTED_OUT.store(args.value == Some(Value::Bool(false)), std::sync::atomic::Ordering::Relaxed);
    }
    Ok(state.store.set_setting(&args.key, args.value.as_ref().filter(|v| !v.is_null()))?)
}

/// Where "Apoyar el proyecto" goes (Buy Me a Coffee, while GitHub Sponsors
/// is pending approval). Voluntary support: nothing in the app depends on it.
pub const SUPPORT_URL: &str = "https://buymeacoffee.com/addlayer";

/// Open the support page in the browser. The URL is fixed: the web side
/// can't open arbitrary links through this.
#[tauri::command]
pub async fn open_support_page(app: tauri::AppHandle) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    app.opener()
        .open_url(SUPPORT_URL, None::<&str>)
        .map_err(|e| CommandError::Internal(format!("no se pudo abrir el navegador: {e}")))
}
