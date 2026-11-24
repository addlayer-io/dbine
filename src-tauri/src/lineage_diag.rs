//! The lineage's diagnostic log in the app: when it is on, where it lives and the Settings commands
//! around it. The writing itself is `dbine_driver::diag` (docs/lineage-diagnostic.md).
//!
//! On by default only in builds whose version has `-test` in it (the ones testers get); the setting
//! `lineage_diag` forces it either way. `lineage_diag_hide_names` hashes the object names.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::diag;
use serde::Serialize;
use serde_json::Value;
use tauri::State;

pub const DIAG_SETTING: &str = "lineage_diag";
pub const HIDE_NAMES_SETTING: &str = "lineage_diag_hide_names";

/// Whether the build is a test build (its version carries `-test`).
pub fn is_test_build(version: &str) -> bool {
    version.contains("-test")
}

/// Whether the log is on: the setting when the user set it, else on in test builds only.
pub fn enabled_for(setting: Option<&Value>, version: &str) -> bool {
    match setting {
        Some(Value::Bool(b)) => *b,
        _ => is_test_build(version),
    }
}

fn setting_bool(state: &AppState, key: &str) -> Option<Value> {
    state.store.get_setting(key).ok().flatten()
}

/// Reads the settings and turns the log on or off, writing a header line when it starts.
pub fn apply(state: &AppState) {
    let version = env!("CARGO_PKG_VERSION");
    let on = enabled_for(setting_bool(state, DIAG_SETTING).as_ref(), version);
    let (Some(dir), true) = (crate::log_dir(), on) else {
        diag::configure(None);
        return;
    };
    let hide = matches!(
        setting_bool(state, HIDE_NAMES_SETTING),
        Some(Value::Bool(true))
    );
    diag::configure(Some(diag::Config::new(dir.join(diag::FILE_NAME), hide)));
    let get = |k: &str| {
        state
            .store
            .get_setting(k)
            .ok()
            .flatten()
            .map_or("default".to_string(), |v| v.to_string())
    };
    diag::event(
        "-",
        "header",
        &[
            ("app", version.to_string()),
            (
                "os",
                format!("{} {}", std::env::consts::OS, std::env::consts::ARCH),
            ),
            ("names", if hide { "hashed" } else { "visible" }.into()),
            ("open_timeout_secs", get("lineage_open_timeout_secs")),
            ("model_timeout_secs", get("lineage_model_timeout_secs")),
            ("scan_parallel", get("lineage_scan_parallel")),
            ("search_budget_secs", get("lineage_search_budget_secs")),
            ("precompute", get("lineage_precompute")),
            ("databricks_driver", "builtin".into()),
        ],
    );
}

/// A command that starts an operation of the log: its id and its connection's short id and engine.
pub fn begin(state: &AppState, stage: &str, connection_id: &str, database: &str) -> Option<String> {
    if !diag::enabled() {
        return None;
    }
    let op = diag::new_op();
    let engine = state
        .store
        .get_connection(connection_id)
        .ok()
        .flatten()
        .map(|c| c.config.driver)
        .unwrap_or_default();
    diag::event(
        &op,
        stage,
        &[
            ("engine", engine),
            ("conn", diag::short_id(connection_id)),
            ("database", diag::name(database)),
        ],
    );
    Some(op)
}

#[derive(Serialize)]
pub struct DiagInfo {
    pub enabled: bool,
    /// Whether it is on without the setting (a test build).
    pub default_on: bool,
    pub path: Option<String>,
}

fn file_path() -> Option<std::path::PathBuf> {
    crate::log_dir().map(|d| d.join(diag::FILE_NAME))
}

/// What Settings shows: whether the log is on and where its file is.
#[tauri::command]
pub async fn get_lineage_diag(state: State<'_, AppState>) -> CommandResult<DiagInfo> {
    let version = env!("CARGO_PKG_VERSION");
    Ok(DiagInfo {
        enabled: enabled_for(setting_bool(&state, DIAG_SETTING).as_ref(), version),
        default_on: is_test_build(version),
        path: file_path().map(|p| p.display().to_string()),
    })
}

/// Opens the folder of `lineage-diagnostic.log` in the file manager.
#[tauri::command]
pub async fn open_lineage_diag_folder(app: tauri::AppHandle) -> CommandResult<()> {
    use tauri_plugin_opener::OpenerExt;
    let dir = crate::log_dir()
        .ok_or_else(|| CommandError::Internal("no hay carpeta de registros".into()))?;
    let path = file_path().filter(|p| p.exists());
    match path {
        Some(p) => app.opener().reveal_item_in_dir(p),
        None => app
            .opener()
            .open_path(dir.display().to_string(), None::<&str>),
    }
    .map_err(|e| CommandError::Internal(format!("no se pudo abrir la carpeta: {e}")))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn on_by_default_only_in_test_builds() {
        assert!(enabled_for(None, "1.7.0-test.13"));
        assert!(!enabled_for(None, "1.7.0"));
        assert!(!enabled_for(Some(&Value::Null), "1.7.0"));
    }

    #[test]
    fn the_setting_forces_it_either_way() {
        assert!(enabled_for(Some(&json!(true)), "1.7.0"));
        assert!(!enabled_for(Some(&json!(false)), "1.7.0-test.13"));
    }
}
