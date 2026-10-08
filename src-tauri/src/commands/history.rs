//! The query history: every statement run from the editor, grouped in the
//! sidebar by the server it ran on. Local to this machine (not in the cloud
//! backup); the newest 20,000 are kept.

use crate::error::CommandResult;
use crate::state::AppState;
use dbine_core::HistoryEntry;
use dbine_driver::QueryOutcome;
use serde::Deserialize;
use tauri::State;

/// Where a run came from, for its tab's timeline.
#[derive(Default)]
pub struct Origin {
    pub query_id: Option<String>,
    pub project_id: Option<String>,
    pub file_path: Option<String>,
}

/// Record a run (never fails the query: a history error is only logged).
pub fn record(state: &AppState, connection_id: &str, database: &str, sql: &str, started_at: String, out: &QueryOutcome, origin: Origin) {
    let Ok(Some(conn)) = state.store.get_connection(connection_id) else { return };
    // A file is its project and its path: both or neither.
    let file = origin.project_id.zip(origin.file_path);
    let rows = (!out.results.is_empty()).then(|| out.results.iter().map(|r| r.rows_affected.unwrap_or(r.total_rows)).sum());
    let entry = HistoryEntry {
        id: 0,
        connection_id: connection_id.to_string(),
        connection_name: conn.name.clone(),
        driver: conn.config.driver.clone(),
        host: host_label(&conn.config.host),
        database: database.to_string(),
        sql: sql.trim().to_string(),
        started_at,
        duration_ms: out.elapsed_ms,
        rows,
        error: out.error.clone(),
        query_id: origin.query_id,
        project_id: file.as_ref().map(|f| f.0.clone()),
        file_path: file.map(|f| f.1),
    };
    if let Err(e) = state.store.add_history(&entry) {
        tracing::warn!(%e, "could not record the query in the history");
    }
}

/// The server's host (without scheme, user or path); a file's name for
/// embedded engines.
fn host_label(host: &str) -> String {
    let h = host.trim();
    let h = h.split_once("://").map(|(_, r)| r).unwrap_or(h);
    let h = h.rsplit_once('@').map(|(_, r)| r).unwrap_or(h);
    if h.contains(['/', '\\']) && !h.starts_with("//") {
        let file = h.rsplit(['/', '\\']).find(|p| !p.is_empty()).unwrap_or(h);
        // A path (a file) or a URL's path after the host.
        return if host.contains("://") { h.split('/').next().unwrap_or(h).to_string() } else { file.to_string() };
    }
    h.to_string()
}

#[derive(Deserialize)]
pub struct HistoryListArgs {
    pub search: Option<String>,
    /// An id from the previous page, to get older entries.
    pub before: Option<i64>,
    pub limit: Option<u32>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn history_list(state: State<'_, AppState>, args: HistoryListArgs) -> CommandResult<Vec<HistoryEntry>> {
    Ok(state.store.list_history(args.search.as_deref(), args.before, args.limit.unwrap_or(200).min(1000))?)
}

#[derive(Deserialize)]
pub struct HistoryOfArgs {
    pub query_id: Option<String>,
    pub project_id: Option<String>,
    pub file_path: Option<String>,
    pub limit: Option<u32>,
}

/// The runs of one saved query or one project file, newest first (the
/// tab's timeline).
#[tauri::command(rename_all = "camelCase")]
pub async fn history_of(state: State<'_, AppState>, args: HistoryOfArgs) -> CommandResult<Vec<HistoryEntry>> {
    let file = args.project_id.as_deref().zip(args.file_path.as_deref());
    Ok(state.store.list_history_of(args.query_id.as_deref(), file, args.limit.unwrap_or(200).min(1000))?)
}

#[derive(Deserialize)]
pub struct HistoryDeleteArgs {
    /// `None`: the whole history.
    pub ids: Option<Vec<i64>>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn history_delete(state: State<'_, AppState>, args: HistoryDeleteArgs) -> CommandResult<()> {
    Ok(state.store.delete_history(args.ids.as_deref())?)
}

#[cfg(test)]
mod tests {
    #[test]
    fn host_labels() {
        use super::host_label;
        assert_eq!(host_label("pg.interno"), "pg.interno");
        assert_eq!(host_label("sql01,1444"), "sql01,1444");
        assert_eq!(host_label("https://u:p@es.interno:9200/idx"), "es.interno:9200");
        assert_eq!(host_label("/Users/ana/datos/ventas.sqlite"), "ventas.sqlite");
        assert_eq!(host_label("C:\\datos\\ventas.duckdb"), "ventas.duckdb");
    }
}
