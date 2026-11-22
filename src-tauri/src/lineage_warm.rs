//! The lineage's precompute after connecting (see `.plan/architecture/background-cache.md`): `connect`
//! spawns it and never waits for it. It reads, on the long lane, one database at a time (the default one
//! first), what the user will ask for when opening "Ver linaje…", and stores it in the explorer cache. A
//! failure only costs the user the head start: it is logged at debug level and nothing else.
//!
//! Power BI connections warm each model's external sources instead (what the cross-connection search
//! reads), as many models at a time as the setting says.

use crate::commands::cross::{externals_of, models_of, scan_limits, scan_parallel};
use crate::commands::lineage_cache::{
    fresh_value, is_cacheable, precompute_enabled, scope_key, user_is_reading, warm_order,
    PRECOMPUTE_SETTING, TTL,
};
use crate::state::{meta_key, AppState, Lane};
use dbine_core::cache::kinds;
use dbine_driver::dependencies::{LineageGraph, LineageScope};
use futures_util::StreamExt;
use serde::de::IgnoredAny;
use std::collections::HashSet;
use std::sync::{Mutex, OnceLock};
use std::time::{Duration, Instant};

/// The longest one database's graph is waited for.
const PER_DATABASE: Duration = Duration::from_secs(5 * 60);
/// The longest a whole warm-up runs: what isn't read by then waits for the next connection or the user.
const TOTAL: Duration = Duration::from_secs(15 * 60);
/// A Power BI workspace has hundreds of models and runs unattended: it gets an hour, and what it reads is kept
/// for the next connection to continue from.
const TOTAL_POWER_BI: Duration = Duration::from_secs(60 * 60);

/// What a connection's warm-up does.
#[derive(Debug, PartialEq, Eq)]
pub enum Plan {
    Skip,
    /// Each model's external sources.
    PowerBi,
    /// Each database's whole graph.
    Databases,
}

/// What to do for a connection: nothing when the user turned it off, when the engine has no lineage or
/// when the company's connection may not reach the disk (nothing would be stored).
pub fn plan(enabled: bool, driver_id: &str, supports_dependencies: bool, may_store: bool) -> Plan {
    if !enabled || !may_store {
        Plan::Skip
    } else if driver_id == "powerbi" {
        Plan::PowerBi
    } else if supports_dependencies {
        Plan::Databases
    } else {
        Plan::Skip
    }
}

fn warming() -> &'static Mutex<HashSet<String>> {
    static SET: OnceLock<Mutex<HashSet<String>>> = OnceLock::new();
    SET.get_or_init(Default::default)
}

/// Held while a connection warms up: a second one for the same connection doesn't start.
struct Guard(String);

impl Guard {
    fn acquire(connection_id: &str) -> Option<Guard> {
        warming()
            .lock()
            .unwrap()
            .insert(connection_id.to_string())
            .then(|| Guard(connection_id.to_string()))
    }
}

impl Drop for Guard {
    fn drop(&mut self) {
        warming().lock().unwrap().remove(&self.0);
    }
}

/// Whether the connection still has its session open (a disconnect or a delete closes it).
fn is_live(state: &AppState, connection_id: &str) -> bool {
    state.sessions.contains_key(&meta_key(connection_id, ""))
}

/// Start the warm-up of a connection that just connected, in the background.
pub fn spawn(
    state: &AppState,
    connection_id: &str,
    databases: Vec<String>,
    default_database: String,
) {
    let state = state.clone();
    let id = connection_id.to_string();
    tauri::async_runtime::spawn(async move {
        warm_after_connect(state, id, databases, default_database).await;
    });
}

/// Warm what the connection's lineage will ask for. Never fails: the reasons to stop are logged.
pub async fn warm_after_connect(
    state: AppState,
    connection_id: String,
    databases: Vec<String>,
    default_database: String,
) {
    let enabled = precompute_enabled(
        state
            .store
            .get_setting(PRECOMPUTE_SETTING)
            .ok()
            .flatten()
            .as_ref(),
    );
    let Ok(Some(conn)) = state.store.get_connection(&connection_id) else {
        return;
    };
    let Some(driver) = dbine_drivers::find(&conn.config.driver) else {
        return;
    };
    let may_store = state.cache.get().is_some() && state.cache_allowed(&connection_id);
    let what = plan(
        enabled,
        &conn.config.driver,
        driver.supports_dependencies(),
        may_store,
    );
    if what == Plan::Skip {
        return;
    }
    let Some(_guard) = Guard::acquire(&connection_id) else {
        return;
    };
    let deadline = Instant::now() + TOTAL;
    match what {
        Plan::PowerBi => warm_power_bi(&state, &conn, Instant::now() + TOTAL_POWER_BI).await,
        Plan::Databases => {
            let scope = LineageScope {
                jobs: driver.capabilities().jobs,
                ..Default::default()
            };
            let scan = dbine_driver::DependencyScan::new(
                driver.info(),
                driver.script_dialect(),
                driver.capabilities().foreign_keys,
            );
            let mut order = warm_order(&databases, &default_database);
            if order.is_empty() {
                order.push(default_database);
            }
            warm_databases(&state, &connection_id, order, &scope, &scan, deadline).await;
        }
        Plan::Skip => {}
    }
}

async fn warm_databases(
    state: &AppState,
    connection_id: &str,
    order: Vec<String>,
    scope: &LineageScope,
    scan: &dbine_driver::DependencyScan,
    deadline: Instant,
) {
    debug_assert!(is_cacheable(scope));
    let key = scope_key(scope);
    for database in order {
        // A read the user asked for goes first.
        while user_is_reading() && Instant::now() < deadline && is_live(state, connection_id) {
            tokio::time::sleep(Duration::from_secs(1)).await;
        }
        let left = deadline.saturating_duration_since(Instant::now());
        if left.is_zero() || !is_live(state, connection_id) {
            tracing::debug!("lineage warm-up of {connection_id} stopped");
            return;
        }
        let stored = state.cache_get::<IgnoredAny>(connection_id, &database, kinds::LINEAGE, &key);
        if fresh_value(stored, chrono::Utc::now(), TTL).is_some() {
            continue;
        }
        let (db, sc, sn) = (database.clone(), scope.clone(), scan.clone());
        let read = tokio::time::timeout(
            PER_DATABASE.min(left),
            state.meta_read_on(
                Lane::Long,
                connection_id,
                &database,
                PER_DATABASE,
                move |s| Box::pin(async move { s.lineage_graph(&db, &sc, &sn).await }),
            ),
        )
        .await;
        match read {
            Ok(Ok(graph)) => {
                // The user may have disconnected while this was reading.
                if is_live(state, connection_id) {
                    state.cache_put::<LineageGraph>(
                        connection_id,
                        &database,
                        kinds::LINEAGE,
                        &key,
                        &graph,
                    );
                }
            }
            Ok(Err(e)) => tracing::debug!("lineage warm-up of {connection_id}/{database}: {e}"),
            Err(_) => tracing::debug!("lineage warm-up of {connection_id}/{database} timed out"),
        }
    }
}

async fn warm_power_bi(
    state: &AppState,
    pbi: &dbine_core::state::SavedConnection,
    deadline: Instant,
) {
    let models = match models_of(state, pbi).await {
        Ok(m) => m,
        Err(e) => {
            tracing::debug!("lineage warm-up of {}: models: {e}", pbi.id);
            return;
        }
    };
    let id = pbi.id.as_str();
    let (model_limit, _) = scan_limits(state);
    let parallel = scan_parallel(state);
    futures_util::stream::iter(models.iter().cloned())
        .map(|model| async move {
            let left = deadline.saturating_duration_since(Instant::now());
            if left.is_zero() || !is_live(state, id) {
                return;
            }
            // Reads the disk and memory first: a model already warm costs nothing.
            let r = tokio::time::timeout(
                left.min(model_limit),
                externals_of(state, pbi, &model, true),
            )
            .await;
            if !matches!(r, Ok(Ok(_))) {
                tracing::debug!("lineage warm-up of {id}/{model}: not read");
            }
        })
        .buffer_unordered(parallel)
        .collect::<Vec<()>>()
        .await;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_plan_follows_the_setting_the_engine_and_the_company() {
        assert_eq!(plan(true, "postgres", true, true), Plan::Databases);
        assert_eq!(plan(true, "powerbi", false, true), Plan::PowerBi);
        // The user turned it off.
        assert_eq!(plan(false, "postgres", true, true), Plan::Skip);
        assert_eq!(plan(false, "powerbi", false, true), Plan::Skip);
        // The engine has no lineage.
        assert_eq!(plan(true, "redis", false, true), Plan::Skip);
        // The company's connection may not reach the disk: nothing would be stored.
        assert_eq!(plan(true, "postgres", true, false), Plan::Skip);
        assert_eq!(plan(true, "powerbi", false, false), Plan::Skip);
    }

    #[test]
    fn a_connection_never_warms_twice_at_once() {
        let first = Guard::acquire("warm-test-1").unwrap();
        assert!(Guard::acquire("warm-test-1").is_none());
        // Others are independent.
        assert!(Guard::acquire("warm-test-2").is_some());
        drop(first);
        assert!(Guard::acquire("warm-test-1").is_some());
    }
}
