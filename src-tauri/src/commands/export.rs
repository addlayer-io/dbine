use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_core::export::{export_rows, ExportOptions, Exporter};
use dbine_driver::{QueryOutcome, ResultColumn, RowSinkRef};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tauri::{AppHandle, Emitter, State};

#[derive(Deserialize)]
pub struct ExportRowsArgs {
    pub path: String,
    pub options: ExportOptions,
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
}

#[derive(Serialize)]
pub struct ExportResult {
    pub rows: u64,
    pub elapsed_ms: u64,
}

/// Export the rows the grid already has.
#[tauri::command(rename_all = "camelCase")]
pub async fn export_rows_to_file(args: ExportRowsArgs) -> CommandResult<ExportResult> {
    let started = std::time::Instant::now();
    let path = PathBuf::from(&args.path);
    let rows = tokio::task::spawn_blocking(move || export_rows(&path, args.options, &args.columns, &args.rows))
        .await
        .map_err(|e| CommandError::Internal(e.to_string()))?
        .map_err(|e| CommandError::Internal(format!("no se pudo escribir el archivo: {e}")))?;
    Ok(ExportResult { rows, elapsed_ms: started.elapsed().as_millis() as u64 })
}

#[derive(Deserialize)]
pub struct ExportQueryArgs {
    /// Chosen by the UI; progress events carry it and `cancel_query` takes
    /// `export:<id>` to stop it.
    pub export_id: String,
    pub connection_id: String,
    pub database: String,
    pub sql: String,
    /// Which result set of the script to write.
    pub result_index: usize,
    pub path: String,
    pub options: ExportOptions,
}

#[derive(Clone, Serialize)]
struct Progress {
    id: String,
    rows: u64,
}

/// Run the script again and stream the chosen result set to the file, all
/// rows, without holding them in memory. The session is read-only whatever
/// the connection says: an export never writes to the database (SQL
/// engines refuse writing statements; the others enforce read-only in
/// their drivers).
#[tauri::command(rename_all = "camelCase")]
pub async fn export_query_to_file(
    app: AppHandle,
    state: State<'_, AppState>,
    args: ExportQueryArgs,
) -> CommandResult<ExportResult> {
    let started = std::time::Instant::now();
    let key = format!("export:{}", args.export_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, true).await?;

    let path = PathBuf::from(&args.path);
    let id = args.export_id.clone();
    let emitter = app.clone();
    let exporter = Arc::new(Mutex::new(
        Exporter::new(&path, args.result_index, args.options).on_progress(move |rows| {
            let _ = emitter.emit("export-progress", Progress { id: id.clone(), rows });
        }),
    ));
    let mut out = QueryOutcome { sink: Some(RowSinkRef(exporter.clone())), ..Default::default() };

    let finished = {
        let mut s = entry.session.lock().await;
        tokio::select! {
            r = s.execute(&args.sql, usize::MAX, &mut out) => Some(r),
            _ = entry.cancel.notified() => None,
        }
    };
    state.sessions.remove(&key);
    out.sink = None;

    let failure = match finished {
        None => Some("Exportación cancelada.".to_string()),
        Some(Err(e)) => Some(e.to_string()),
        Some(Ok(())) => out
            .sink_error
            .clone()
            .map(|e| format!("no se pudo escribir el archivo: {e}"))
            .or_else(|| (out.results.len() <= args.result_index).then(|| "la consulta no devolvió ese resultado".to_string()))
            .or_else(|| out.results[args.result_index].columns.is_empty().then(|| "ese resultado no tiene filas para exportar".to_string())),
    };
    let rows = exporter.lock().map_err(|_| CommandError::Internal("exportador".into()))?.finish();
    if let Some(msg) = failure {
        // Don't leave half a file behind.
        let _ = std::fs::remove_file(&path);
        return Err(CommandError::Sql(msg));
    }
    let rows = rows.map_err(|e| CommandError::Internal(format!("no se pudo cerrar el archivo: {e}")))?;
    Ok(ExportResult { rows, elapsed_ms: started.elapsed().as_millis() as u64 })
}
