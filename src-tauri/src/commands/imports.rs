//! Importing data files into a table / collection (docs/api-comandos.md).
//! The rows become the driver's own insert script (SQL INSERTs,
//! `insertMany`, `_bulk`…) run in batches, so every engine imports the same way.

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::import::{self, ImportFormat, ImportOptions, Preview};
use dbine_driver::{DdlParts, ObjectRef, QueryOutcome, TableSchema};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::atomic::Ordering;
use tauri::{AppHandle, Emitter, State};

#[derive(Deserialize)]
pub struct PreviewArgs {
    pub path: String,
    pub format: ImportFormat,
    #[serde(default)]
    pub options: ImportOptions,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn preview_import_file(args: PreviewArgs) -> CommandResult<Preview> {
    tokio::task::spawn_blocking(move || import::preview(&PathBuf::from(&args.path), args.format, &args.options, 50))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
        .map_err(|e| CommandError::BadRequest(format!("no se pudo leer el archivo: {e}")))
}

#[derive(Deserialize)]
pub struct Mapping {
    pub source: String,
    pub target: String,
}

#[derive(Deserialize)]
pub struct ImportArgs {
    pub import_id: String,
    pub connection_id: String,
    pub database: String,
    pub path: String,
    pub format: ImportFormat,
    #[serde(default)]
    pub options: ImportOptions,
    pub target: ObjectRef,
    /// Created before importing (the new-table option).
    pub create_table: Option<TableSchema>,
    pub mapping: Vec<Mapping>,
    #[serde(default = "default_batch")]
    pub batch: usize,
}

fn default_batch() -> usize {
    500
}

#[derive(Serialize)]
pub struct ImportResult {
    pub rows: u64,
    pub elapsed_ms: u64,
}

#[derive(Clone, Serialize)]
struct ImportProgress {
    id: String,
    rows: u64,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn import_file(app: AppHandle, state: State<'_, AppState>, args: ImportArgs) -> CommandResult<ImportResult> {
    let started = std::time::Instant::now();
    let driver = driver_of(&state, &args.connection_id)?;
    let key = format!("import:{}", args.import_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, false).await?;

    let run = async {
        let mut reader = tokio::task::block_in_place(|| import::open(&PathBuf::from(&args.path), args.format, &args.options))
            .map_err(|e| CommandError::BadRequest(format!("no se pudo leer el archivo: {e}")))?;
        // Source positions of the mapped columns, in target order.
        let picks: Vec<(usize, String)> = args
            .mapping
            .iter()
            .filter(|m| !m.target.is_empty())
            .filter_map(|m| reader.columns.iter().position(|c| *c == m.source).map(|i| (i, m.target.clone())))
            .collect();
        if picks.is_empty() {
            return Err(CommandError::BadRequest("no hay columnas para importar: revisá la correspondencia".into()));
        }
        let columns: Vec<String> = picks.iter().map(|(_, t)| t.clone()).collect();

        let exec = |sql: String| {
            let entry = entry.clone();
            async move {
                let mut out = QueryOutcome::default();
                let mut s = entry.session.lock().await;
                tokio::select! {
                    r = s.execute(&sql, 1, &mut out) => r.map_err(CommandError::from),
                    _ = entry.cancel.notified() => Err(CommandError::Cancelled),
                }
            }
        };

        if let Some(t) = &args.create_table {
            let ddl = driver.table_ddl(t, DdlParts { create: true, indexes: true, ..Default::default() })?;
            exec(ddl).await.map_err(|e| CommandError::Sql(format!("no se pudo crear {}: {e}", t.name)))?;
        }

        let mut rows = 0u64;
        let batch = args.batch.clamp(1, 10_000);
        loop {
            if entry.cancelled.load(Ordering::SeqCst) {
                return Err(CommandError::Cancelled);
            }
            let chunk: Vec<Vec<Value>> = tokio::task::block_in_place(|| {
                reader
                    .by_ref()
                    .take(batch)
                    .map(|r| r.map(|row| picks.iter().map(|(i, _)| row.get(*i).cloned().unwrap_or(Value::Null)).collect()))
                    .collect::<std::io::Result<_>>()
            })
            .map_err(|e| CommandError::BadRequest(format!("error leyendo el archivo (fila {}): {e}", rows + 1)))?;
            if chunk.is_empty() {
                break;
            }
            let n = chunk.len() as u64;
            let script = driver.insert_script(&args.target, &columns, &chunk)?;
            exec(script).await.map_err(|e| CommandError::Sql(format!("filas {}–{}: {e}", rows + 1, rows + n)))?;
            rows += n;
            let _ = app.emit("import-progress", ImportProgress { id: args.import_id.clone(), rows });
        }
        Ok(rows)
    };
    let res = run.await;
    state.sessions.remove(&key);
    Ok(ImportResult { rows: res?, elapsed_ms: started.elapsed().as_millis() as u64 })
}
