//! Backups (docs/backups.md): DBine's copies of a database (a script with
//! its structure and data in a local file, restored by running it, for every
//! engine) and the engine's own backups (its history and the code that makes,
//! restores or deletes one, which the UI shows and runs on the user's click).

use crate::commands::schema::driver_of;
use crate::commands::scripts::{generate, GenerateArgs, ScriptOptions};
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::BackupCopy;
use dbine_driver::{BackupAction, BackupEntry, FieldKind, Language, ObjectRef};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;
use tauri::{AppHandle, Manager, State};

#[derive(Deserialize)]
pub struct ListArgs {
    pub connection_id: String,
    /// "" for the server-wide view (engines whose backups cover the server).
    #[serde(default)]
    pub database: String,
}

#[derive(Serialize)]
pub struct BackupList {
    pub copies: Vec<BackupCopy>,
    /// The server's history, when the engine has one.
    pub native: Vec<BackupEntry>,
    /// Reading the server's history failed (the copies still show).
    pub native_error: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn backup_list(state: State<'_, AppState>, args: ListArgs) -> CommandResult<BackupList> {
    let db = Some(args.database.as_str()).filter(|d| !d.is_empty());
    let copies = state.store.list_backups(&args.connection_id, db)?;
    let driver = driver_of(&state, &args.connection_id)?;
    let (native, native_error) = match driver.backup().filter(|s| s.history) {
        None => (Vec::new(), None),
        Some(_) => {
            let key = format!("backup:{}:{}", args.connection_id, args.database);
            let r = match state.session(&key, &args.connection_id, &args.database).await {
                Ok(entry) => entry.session.lock().await.backups(db).await.map_err(|e| e.to_string()),
                Err(e) => Err(e.to_string()),
            };
            match r {
                Ok(v) => (v, None),
                Err(e) => (Vec::new(), Some(e)),
            }
        }
    };
    Ok(BackupList { copies, native, native_error })
}

#[derive(Deserialize)]
pub struct ScriptArgs {
    pub connection_id: String,
    pub action: BackupAction,
}

#[derive(Serialize)]
pub struct BackupScript {
    pub script: String,
    /// The script with the secret options (keys, passwords) replaced by
    /// ••••••, for the preview and the copy.
    pub shown: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn backup_script(state: State<'_, AppState>, args: ScriptArgs) -> CommandResult<BackupScript> {
    let driver = driver_of(&state, &args.connection_id)?;
    let script = driver.backup_script(&args.action)?;
    let secret_keys: Vec<&str> = driver
        .backup()
        .map(|s| s.backup_options.iter().chain(&s.restore_options).filter(|f| f.secret || matches!(f.kind, FieldKind::Password)).map(|f| f.key).collect())
        .unwrap_or_default();
    let options = match &args.action {
        BackupAction::Backup { options, .. } | BackupAction::Restore { options, .. } => Some(options),
        BackupAction::Delete { .. } => None,
    };
    let mut shown = script.clone();
    for (k, v) in options.into_iter().flatten() {
        if secret_keys.contains(&k.as_str()) && !v.is_empty() {
            shown = crate::commands::security::mask(&shown, v);
        }
    }
    Ok(BackupScript { script, shown })
}

#[derive(Deserialize)]
pub struct DefaultPathArgs {
    pub connection_id: String,
    pub database: String,
}

/// Where a new copy goes unless the user picks another place:
/// Documents/DBine/Backups/<connection>/<database>-<date>.<ext>.
#[tauri::command(rename_all = "camelCase")]
pub async fn backup_default_path(app: AppHandle, state: State<'_, AppState>, args: DefaultPathArgs) -> CommandResult<String> {
    let conn = state.store.get_connection(&args.connection_id)?.ok_or_else(|| CommandError::NotFound("la conexión ya no existe".into()))?;
    let driver = driver_of(&state, &args.connection_id)?;
    let base = app
        .path()
        .document_dir()
        .or_else(|_| app.path().home_dir())
        .map_err(|e| CommandError::Internal(e.to_string()))?;
    // As "Generar script" names them.
    let ext = match driver.info().language {
        Language::Sql => "sql",
        Language::Cql => "cql",
        Language::Json => "js",
        _ => "txt",
    };
    let name = if args.database.is_empty() { conn.name.as_str() } else { args.database.as_str() };
    let file = format!("{}-{}.{ext}", safe(name), chrono::Local::now().format("%Y%m%d-%H%M%S"));
    let path: PathBuf = base.join("DBine").join("Backups").join(safe(&conn.name)).join(file);
    Ok(path.to_string_lossy().into_owned())
}

/// A name usable as a file or folder name on every OS.
fn safe(name: &str) -> String {
    let s: String = name
        .chars()
        .map(|c| if c.is_control() || matches!(c, '/' | '\\' | ':' | '*' | '?' | '"' | '<' | '>' | '|') { '_' } else { c })
        .collect();
    let s = s.trim().trim_matches('.').to_string();
    if s.is_empty() { "backup".into() } else { s }
}

#[derive(Deserialize)]
pub struct CopyArgs {
    /// Progress events come as `script-progress` with this id.
    pub backup_id: String,
    pub connection_id: String,
    pub database: String,
    pub objects: Vec<ObjectRef>,
    /// With the rows, not only the structure.
    pub data: bool,
    pub path: String,
}

/// Write a copy of the database (DROP IF EXISTS + CREATE of every object,
/// its indexes and keys, and the rows) and keep it in the list.
#[tauri::command(rename_all = "camelCase")]
pub async fn backup_copy(app: AppHandle, state: State<'_, AppState>, args: CopyArgs) -> CommandResult<BackupCopy> {
    let started = std::time::Instant::now();
    let created_at = chrono::Utc::now().to_rfc3339();
    if let Some(dir) = PathBuf::from(&args.path).parent() {
        std::fs::create_dir_all(dir).map_err(|e| CommandError::BadRequest(format!("no se pudo crear la carpeta {}: {e}", dir.display())))?;
    }
    let driver = driver_of(&state, &args.connection_id)?;
    let gen = GenerateArgs {
        script_id: args.backup_id.clone(),
        connection_id: args.connection_id.clone(),
        database: args.database.clone(),
        objects: args.objects.clone(),
        options: ScriptOptions {
            drop: true,
            if_exists: true,
            create: true,
            indexes: true,
            foreign_keys: true,
            definitions: true,
            data: args.data,
            data_limit: None,
        },
        path: Some(args.path.clone()),
        target_driver: None,
    };
    let key = format!("script:{}", args.backup_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;
    let res = generate(&app, driver, &entry, &gen).await;
    state.sessions.remove(&key);
    let done = match res {
        Ok(r) => r,
        Err(e) => {
            let _ = std::fs::remove_file(&args.path);
            return Err(e);
        }
    };
    let copy = BackupCopy {
        id: args.backup_id,
        connection_id: args.connection_id,
        database: args.database,
        size: std::fs::metadata(&args.path).map(|m| m.len()).unwrap_or(0),
        path: args.path,
        created_at,
        objects: done.objects as u64,
        rows: done.rows,
        data: args.data,
        duration_ms: started.elapsed().as_millis() as u64,
    };
    state.store.add_backup(&copy)?;
    Ok(copy)
}

#[derive(Deserialize)]
pub struct DeleteArgs {
    pub id: String,
    /// Also delete the file (otherwise it only leaves the list).
    pub delete_file: bool,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn backup_copy_delete(state: State<'_, AppState>, args: DeleteArgs) -> CommandResult<()> {
    if args.delete_file {
        if let Some(b) = state.store.get_backup(&args.id)? {
            match std::fs::remove_file(&b.path) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
                Err(e) => return Err(CommandError::BadRequest(format!("no se pudo borrar {}: {e}", b.path))),
            }
        }
    }
    state.store.delete_backup(&args.id)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::safe;

    #[test]
    fn file_names_are_safe_everywhere() {
        assert_eq!(safe("ventas/2024: Q1?"), "ventas_2024_ Q1_");
        assert_eq!(safe(" .. "), "backup");
        assert_eq!(safe("Producción"), "Producción");
    }
}
