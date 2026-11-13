//! The functions the query editor completes (issue #97): the engine's SQL
//! functions, or what plays their part in its language (Redis commands,
//! MongoDB operators…), as `Session::function_catalog` gives them.
//!
//! A catalog is read once per connection and database for the app run, on
//! the database's metadata session, and kept until the connection closes,
//! reconnects or is edited (`AppState::close_connection_sessions`). An engine
//! without one answers an empty list: the editor then offers no functions.
//!
//! The metadata session is the explorer's too: while the read runs, the
//! explorer of that database waits. So the read has a shorter limit than the
//! explorer's own (`FUNCTIONS_LIMIT`), and one that runs out of time is not
//! tried again until the connection reconnects: its database keeps an empty
//! catalog, instead of stalling the explorer again on every keystroke.

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dashmap::DashMap;
use dbine_driver::{Error, FunctionInfo};
use serde::Deserialize;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tauri::State;

/// How long the catalog read may take on the metadata session (half the
/// explorer's `META_LIMIT`): the explorer waits behind it at most this long,
/// once per connection and database.
const FUNCTIONS_LIMIT: Duration = Duration::from_secs(30);

/// The catalogs read this run, per connection and database.
#[derive(Default)]
pub struct FunctionCatalogs {
    entries: DashMap<(String, String), Arc<Vec<FunctionInfo>>>,
    /// Moves on each `forget_connection`: a read that started before it
    /// isn't kept (it may come from the session that just closed).
    generation: AtomicU64,
}

impl FunctionCatalogs {
    pub fn get(&self, connection_id: &str, database: &str) -> Option<Arc<Vec<FunctionInfo>>> {
        self.entries
            .get(&(connection_id.to_string(), database.to_string()))
            .map(|e| e.clone())
    }

    /// Where reads stand now: pass it to `put` with what was read.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::SeqCst)
    }

    /// Keep a catalog read since `generation`, unless a connection was
    /// forgotten meanwhile; the caller gets it either way.
    pub fn put(
        &self,
        connection_id: &str,
        database: &str,
        generation: u64,
        catalog: Vec<FunctionInfo>,
    ) -> Arc<Vec<FunctionInfo>> {
        let catalog = Arc::new(catalog);
        let key = (connection_id.to_string(), database.to_string());
        // Under the entry's lock, so a forget can't slip in between the
        // check and the insert.
        if let dashmap::Entry::Vacant(slot) = self.entries.entry(key) {
            if self.generation() == generation {
                slot.insert(catalog.clone());
            }
        }
        catalog
    }

    /// Drop every catalog of a connection (closed, reconnected, edited).
    pub fn forget_connection(&self, connection_id: &str) {
        self.generation.fetch_add(1, Ordering::SeqCst);
        self.entries.retain(|(c, _), _| c != connection_id);
    }
}

/// An engine without a catalog has nothing to complete: an empty list, not
/// an error.
fn or_empty(
    read: dbine_driver::Result<Vec<FunctionInfo>>,
) -> dbine_driver::Result<Vec<FunctionInfo>> {
    match read {
        Err(Error::Unsupported(_)) => Ok(Vec::new()),
        other => other,
    }
}

#[derive(Deserialize)]
pub struct FunctionCatalogArgs {
    pub connection_id: String,
    /// The editor's database; absent or empty: the connection's default.
    #[serde(default)]
    pub database: Option<String>,
}

/// A read that failed: one that ran out of time on the server (it had the
/// session: `started`) is final, an empty catalog until the connection
/// reconnects; anything else (the session still busy with another read, a
/// broken connection…) is the editor's to ask again later.
fn after_failed_read(e: CommandError, started: bool) -> CommandResult<Vec<FunctionInfo>> {
    match e {
        // `meta_read` answers `Busy` once it has the session only when the
        // read itself timed out (no driver error converts to `Busy`).
        CommandError::Busy(_) if started => Ok(Vec::new()),
        e => Err(e),
    }
}

/// What the read on the session gave.
enum Read {
    /// Another editor's read of it ended while this one waited.
    Kept(Arc<Vec<FunctionInfo>>),
    Fresh(Vec<FunctionInfo>),
}

/// The catalog of the connection's engine for that database, read from the
/// server (or the driver's static catalog) the first time it's asked for.
#[tauri::command(rename_all = "camelCase")]
pub async fn function_catalog(
    state: State<'_, AppState>,
    args: FunctionCatalogArgs,
) -> CommandResult<Vec<FunctionInfo>> {
    let connection_id = args.connection_id;
    let database = args.database.unwrap_or_default();
    let catalogs = state.functions.clone();
    if let Some(kept) = catalogs.get(&connection_id, &database) {
        return Ok(kept.as_ref().clone());
    }
    let generation = catalogs.generation();
    let started = Arc::new(AtomicBool::new(false));
    let read = {
        let (catalogs, started) = (catalogs.clone(), started.clone());
        let (c, d) = (connection_id.clone(), database.clone());
        state
            .meta_read(&connection_id, &database, FUNCTIONS_LIMIT, move |s| {
                started.store(true, Ordering::SeqCst);
                Box::pin(async move {
                    if let Some(kept) = catalogs.get(&c, &d) {
                        return Ok(Read::Kept(kept));
                    }
                    or_empty(s.function_catalog().await).map(Read::Fresh)
                })
            })
            .await
    };
    let catalog = match read {
        Ok(Read::Kept(kept)) => return Ok(kept.as_ref().clone()),
        Ok(Read::Fresh(catalog)) => catalog,
        Err(e) => {
            let why = e.to_string();
            let catalog = after_failed_read(e, started.load(Ordering::SeqCst))?;
            tracing::warn!(
                connection_id,
                database,
                why,
                "the function catalog took too long to read: none until the connection reconnects"
            );
            catalog
        }
    };
    let kept = catalogs.put(&connection_id, &database, generation, catalog);
    Ok(kept.as_ref().clone())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::FunctionKind;

    fn catalog(name: &str) -> Vec<FunctionInfo> {
        vec![FunctionInfo::new(name, FunctionKind::Scalar)]
    }

    #[test]
    fn a_catalog_is_kept_per_connection_and_database() {
        let c = FunctionCatalogs::default();
        let g = c.generation();
        c.put("pg", "ventas", g, catalog("upper"));
        c.put("pg", "rrhh", g, catalog("lower"));
        assert_eq!(c.get("pg", "ventas").unwrap()[0].name, "upper");
        assert_eq!(c.get("pg", "rrhh").unwrap()[0].name, "lower");
        assert!(c.get("pg", "").is_none());
        assert!(c.get("mysql", "ventas").is_none());
        // The first one read stays: a second read of the same key doesn't
        // replace it mid-run.
        let back = c.put("pg", "ventas", g, catalog("initcap"));
        assert_eq!(back[0].name, "initcap");
        assert_eq!(c.get("pg", "ventas").unwrap()[0].name, "upper");
    }

    #[test]
    fn closing_a_connection_forgets_only_its_catalogs() {
        let c = FunctionCatalogs::default();
        let g = c.generation();
        c.put("pg", "ventas", g, catalog("upper"));
        c.put("pg", "rrhh", g, catalog("upper"));
        c.put("mysql", "ventas", g, catalog("upper"));
        c.forget_connection("pg");
        assert!(c.get("pg", "ventas").is_none());
        assert!(c.get("pg", "rrhh").is_none());
        assert!(c.get("mysql", "ventas").is_some());
    }

    #[test]
    fn a_read_that_started_before_a_close_is_not_kept() {
        let c = FunctionCatalogs::default();
        let before = c.generation();
        c.forget_connection("pg");
        // What it read still answers the editor that asked…
        let back = c.put("pg", "ventas", before, catalog("upper"));
        assert_eq!(back[0].name, "upper");
        // …but the next ask reads again, on the new session.
        assert!(c.get("pg", "ventas").is_none());
        let now = c.generation();
        c.put("pg", "ventas", now, catalog("upper"));
        assert!(c.get("pg", "ventas").is_some());
    }

    #[test]
    fn an_engine_without_a_catalog_answers_an_empty_list() {
        let none = or_empty(Err(Error::Unsupported("no".into()))).unwrap();
        assert!(none.is_empty());
        assert!(or_empty(Err(Error::Query("boom".into()))).is_err());
        assert_eq!(or_empty(Ok(catalog("abs"))).unwrap().len(), 1);
    }

    #[test]
    fn a_read_that_ran_out_of_time_is_final_and_the_rest_are_asked_again() {
        // It had the session and timed out: an empty catalog, kept.
        let busy = || CommandError::Busy("el servidor no respondió".into());
        assert!(after_failed_read(busy(), true).unwrap().is_empty());
        // It waited for the session (another read held it): asked again.
        assert!(matches!(
            after_failed_read(busy(), false),
            Err(CommandError::Busy(_))
        ));
        // A broken connection or a failed query: asked again too.
        assert!(after_failed_read(CommandError::Connect("x".into()), true).is_err());
        assert!(after_failed_read(CommandError::Sql("x".into()), true).is_err());
    }

    #[tokio::test]
    async fn closing_a_connections_sessions_forgets_its_catalogs() {
        let state = crate::state::AppState::new(dbine_core::StateStore::open_in_memory().unwrap());
        let g = state.functions.generation();
        state.functions.put("pg", "ventas", g, catalog("upper"));
        state.functions.put("pg", "", g, catalog("upper"));
        state.functions.put("mysql", "ventas", g, catalog("upper"));
        // A read of "pg" that started before the close…
        let before = state.functions.generation();
        state.close_connection_sessions("pg");
        assert!(state.functions.get("pg", "ventas").is_none());
        assert!(state.functions.get("pg", "").is_none());
        assert!(state.functions.get("mysql", "ventas").is_some());
        // …and ends after it isn't kept.
        state
            .functions
            .put("pg", "ventas", before, catalog("upper"));
        assert!(state.functions.get("pg", "ventas").is_none());
    }

    #[test]
    fn the_args_take_the_database_or_leave_it_out() {
        let a: FunctionCatalogArgs =
            serde_json::from_value(serde_json::json!({ "connection_id": "c1" })).unwrap();
        assert_eq!(a.connection_id, "c1");
        assert_eq!(a.database, None);
        let a: FunctionCatalogArgs = serde_json::from_value(
            serde_json::json!({ "connection_id": "c1", "database": "ventas" }),
        )
        .unwrap();
        assert_eq!(a.database.as_deref(), Some("ventas"));
    }
}
