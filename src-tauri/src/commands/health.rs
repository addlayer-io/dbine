use crate::error::CommandResult;

#[tauri::command]
pub async fn health() -> CommandResult<&'static str> {
    Ok("ok")
}

/// Where `app.log` lives, for the "abrir carpeta de logs" action.
#[tauri::command]
pub async fn get_log_dir() -> CommandResult<Option<String>> {
    Ok(crate::log_dir().map(|p| p.display().to_string()))
}

/// UI errors (uncaught exceptions, rejected promises) into app.log, so a
/// broken screen leaves evidence like a Rust panic does.
#[tauri::command]
pub async fn log_ui_error(message: String) -> CommandResult<()> {
    tracing::error!(target: "ui", "{message}");
    Ok(())
}

#[derive(serde::Deserialize)]
pub struct SaveTextFileArgs {
    /// Chosen by the user in the native save dialog.
    pub path: String,
    pub contents: String,
}

/// Write a text file the user picked a path for (exported plans, results…).
#[tauri::command(rename_all = "camelCase")]
pub async fn save_text_file(args: SaveTextFileArgs) -> CommandResult<()> {
    tokio::fs::write(&args.path, args.contents)
        .await
        .map_err(|e| crate::error::CommandError::Internal(format!("no se pudo escribir {}: {e}", args.path)))
}

#[derive(serde::Deserialize)]
pub struct SaveBinaryFileArgs {
    pub path: String,
    pub base64: String,
}

/// Write a binary file (chart images…) the user picked a path for.
#[tauri::command(rename_all = "camelCase")]
pub async fn save_binary_file(args: SaveBinaryFileArgs) -> CommandResult<()> {
    use base64::Engine;
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(args.base64.as_bytes())
        .map_err(|e| crate::error::CommandError::BadRequest(format!("imagen inválida: {e}")))?;
    tokio::fs::write(&args.path, bytes)
        .await
        .map_err(|e| crate::error::CommandError::Internal(format!("no se pudo escribir {}: {e}", args.path)))
}
