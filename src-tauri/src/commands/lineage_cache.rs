//! The lineage's disk cache (see `.plan/architecture/background-cache.md`): the last `LineageGraph` of
//! a database per scope, kept in `cache.db` next to the explorer's entries. The UI asks the cache first
//! (`lineage_graph_cached`, instant, no server call), draws it with its age and refreshes from the server
//! (`get_lineage_graph` writes the answer back). Only names, structure and the lines of code that name an
//! object: no rows, no secrets.

use crate::error::CommandResult;
use crate::state::AppState;
use chrono::{DateTime, Utc};
use dbine_core::cache::kinds;
use dbine_driver::dependencies::{LineageGraph, LineageScope};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;
use tauri::State;

/// How long a cached lineage is taken as fresh enough to skip reading it again in the background.
pub const TTL: Duration = Duration::from_secs(6 * 60 * 60);

/// The setting that turns the precompute after connecting off.
pub const PRECOMPUTE_SETTING: &str = "lineage_precompute";

/// What `scope` is stored under: a canonical, deterministic string. The lists are sorted and without
/// repeats, so the order the UI picked schemas in doesn't matter. `silent`, `sources_only` and `tenant_sources` are never
/// part of it: they change how a read is done, not what it is of.
pub fn scope_key(scope: &LineageScope) -> String {
    #[derive(Serialize)]
    struct Key {
        schemas: Vec<String>,
        kinds: Vec<String>,
        limit: u32,
        jobs: bool,
    }
    fn canon(v: &[String]) -> Vec<String> {
        let mut v = v.to_vec();
        v.sort();
        v.dedup();
        v
    }
    serde_json::to_string(&Key {
        schemas: canon(&scope.schemas),
        kinds: canon(&scope.kinds),
        limit: scope.limit,
        jobs: scope.jobs,
    })
    .unwrap_or_default()
}

/// The scope a read really runs with: jobs only where the driver has them. The cache key is built from
/// this, so a scope asked with jobs on an engine without them finds what the precompute stored.
pub fn effective_scope(mut scope: LineageScope, driver_has_jobs: bool) -> LineageScope {
    scope.jobs = scope.jobs && driver_has_jobs;
    scope
}

/// A graph read for a partial purpose (a background search of sources, no consents asked) isn't the
/// database's lineage: it is never stored under the scope's key.
pub fn is_cacheable(scope: &LineageScope) -> bool {
    !scope.silent && !scope.sources_only && !scope.tenant_sources
}

/// Whether an entry stored at `updated_at` (RFC 3339) is younger than `ttl` at `now`. An entry from the
/// future (a clock that moved back) or one that can't be read counts as stale.
pub fn is_fresh(updated_at: &str, now: DateTime<Utc>, ttl: Duration) -> bool {
    let Ok(at) = DateTime::parse_from_rfc3339(updated_at) else {
        return false;
    };
    match (now - at.with_timezone(&Utc)).to_std() {
        Ok(age) => age < ttl,
        Err(_) => false,
    }
}

/// The value of a stored entry when it is still fresh.
pub fn fresh_value<T>(entry: Option<(T, String)>, now: DateTime<Utc>, ttl: Duration) -> Option<T> {
    entry.and_then(|(v, at)| is_fresh(&at, now, ttl).then_some(v))
}

/// Whether the setting leaves the precompute on: it is, unless the user turned it off.
pub fn precompute_enabled(value: Option<&serde_json::Value>) -> bool {
    !matches!(value, Some(serde_json::Value::Bool(false)))
}

/// The order a connection's databases are warmed in: the default database first, then the others as the
/// server lists them, without repeats or empty names.
pub fn warm_order(databases: &[String], default_database: &str) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    if !default_database.is_empty() && databases.iter().any(|d| d == default_database) {
        out.push(default_database.to_string());
    }
    for d in databases {
        if !d.is_empty() && !out.contains(d) {
            out.push(d.clone());
        }
    }
    out
}

/// Reads the user asked for that are running: the precompute waits for them between databases.
static USER_READS: AtomicUsize = AtomicUsize::new(0);

/// Held while a lineage read the user asked for runs.
pub struct UserRead;

impl UserRead {
    pub fn begin() -> Self {
        USER_READS.fetch_add(1, Ordering::SeqCst);
        UserRead
    }
}

impl Drop for UserRead {
    fn drop(&mut self) {
        USER_READS.fetch_sub(1, Ordering::SeqCst);
    }
}

/// Whether a read the user asked for is running.
pub fn user_is_reading() -> bool {
    USER_READS.load(Ordering::SeqCst) > 0
}

/// The cache's answer: the graph and when it was stored, both `None` when there is nothing (yet).
#[derive(Serialize)]
pub struct CachedLineage {
    pub graph: Option<LineageGraph>,
    pub updated_at: Option<String>,
}

#[derive(Deserialize)]
pub struct LineageCachedArgs {
    pub connection_id: String,
    pub database: String,
    #[serde(default)]
    pub scope: LineageScope,
}

/// The cached lineage of a database for a scope, at once: it never asks the server.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_graph_cached(
    state: State<'_, AppState>,
    args: LineageCachedArgs,
) -> CommandResult<CachedLineage> {
    let jobs = state
        .store
        .get_connection(&args.connection_id)?
        .and_then(|c| dbine_drivers::find(&c.config.driver))
        .is_some_and(|d| d.capabilities().jobs);
    let key = scope_key(&effective_scope(args.scope, jobs));
    Ok(
        match state.cache_get::<LineageGraph>(
            &args.connection_id,
            &args.database,
            kinds::LINEAGE,
            &key,
        ) {
            Some((graph, at)) => CachedLineage {
                graph: Some(graph),
                updated_at: Some(at),
            },
            None => CachedLineage {
                graph: None,
                updated_at: None,
            },
        },
    )
}

#[derive(Deserialize)]
pub struct LineageCacheClearArgs {
    /// `null`: every connection.
    pub connection_id: Option<String>,
}

/// "Borrar caché del linaje": drops the stored lineage (and the Power BI sources) of one connection or
/// of all. The explorer's own entries stay.
#[tauri::command(rename_all = "camelCase")]
pub async fn lineage_cache_clear(
    state: State<'_, AppState>,
    args: LineageCacheClearArgs,
) -> CommandResult<()> {
    state.cache_forget_lineage(args.connection_id.as_deref(), None);
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_core::state::StateStore;
    use dbine_core::ExplorerCache;

    fn scope(schemas: &[&str], kinds: &[&str], limit: u32, jobs: bool) -> LineageScope {
        LineageScope {
            schemas: schemas.iter().map(|s| s.to_string()).collect(),
            kinds: kinds.iter().map(|s| s.to_string()).collect(),
            limit,
            jobs,
            ..Default::default()
        }
    }

    #[test]
    fn the_scope_key_is_canonical() {
        let a = scope_key(&scope(&["b", "a", "a"], &["view", "table"], 50, true));
        let b = scope_key(&scope(&["a", "b"], &["table", "view"], 50, true));
        assert_eq!(a, b);
        assert_eq!(
            a,
            r#"{"schemas":["a","b"],"kinds":["table","view"],"limit":50,"jobs":true}"#
        );
        // Each part tells scopes apart.
        for other in [
            scope(&["a"], &["table", "view"], 50, true),
            scope(&["a", "b"], &["table"], 50, true),
            scope(&["a", "b"], &["table", "view"], 51, true),
            scope(&["a", "b"], &["table", "view"], 50, false),
        ] {
            assert_ne!(a, scope_key(&other));
        }
        assert_eq!(
            scope_key(&LineageScope::default()),
            r#"{"schemas":[],"kinds":[],"limit":0,"jobs":false}"#
        );
    }

    #[test]
    fn the_scope_key_ignores_how_the_read_is_done() {
        let plain = scope(&["a"], &[], 0, false);
        let mut partial = plain.clone();
        partial.silent = true;
        partial.sources_only = true;
        assert_eq!(scope_key(&plain), scope_key(&partial));
        assert!(is_cacheable(&plain));
        assert!(!is_cacheable(&partial));
        let mut tenant = plain.clone();
        tenant.tenant_sources = true;
        assert!(!is_cacheable(&tenant));
        let mut only_silent = plain;
        only_silent.silent = true;
        assert!(!is_cacheable(&only_silent));
    }

    #[test]
    fn jobs_only_count_where_the_driver_has_them() {
        let asked = scope(&[], &[], 0, true);
        assert!(effective_scope(asked.clone(), true).jobs);
        assert!(!effective_scope(asked, false).jobs);
    }

    #[test]
    fn freshness_follows_the_ttl() {
        let now = Utc::now();
        let ago = |s: u64| (now - chrono::Duration::seconds(s as i64)).to_rfc3339();
        assert!(is_fresh(&ago(60), now, TTL));
        assert!(is_fresh(&ago(TTL.as_secs() - 1), now, TTL));
        assert!(!is_fresh(&ago(TTL.as_secs() + 1), now, TTL));
        assert!(!is_fresh("not a date", now, TTL));
        // The future is not trusted.
        assert!(!is_fresh(
            &(now + chrono::Duration::hours(1)).to_rfc3339(),
            now,
            TTL
        ));
        assert_eq!(fresh_value(Some((7, ago(10))), now, TTL), Some(7));
        assert_eq!(fresh_value(Some((7, ago(7 * 3600))), now, TTL), None);
        assert_eq!(fresh_value::<i32>(None, now, TTL), None);
    }

    #[test]
    fn the_precompute_is_on_unless_turned_off() {
        assert!(precompute_enabled(None));
        assert!(precompute_enabled(Some(&serde_json::json!(true))));
        assert!(!precompute_enabled(Some(&serde_json::json!(false))));
    }

    #[test]
    fn the_default_database_is_warmed_first() {
        let dbs: Vec<String> = ["a", "b", "main", "c", "b", ""]
            .iter()
            .map(|s| s.to_string())
            .collect();
        assert_eq!(warm_order(&dbs, "main"), ["main", "a", "b", "c"]);
        // A default that the server doesn't list isn't invented.
        assert_eq!(warm_order(&dbs, "gone"), ["a", "b", "main", "c"]);
        assert_eq!(warm_order(&dbs, ""), ["a", "b", "main", "c"]);
        assert!(warm_order(&[], "x").is_empty());
    }

    #[test]
    fn a_read_the_user_asked_for_is_counted() {
        assert!(!user_is_reading());
        {
            let _a = UserRead::begin();
            assert!(user_is_reading());
        }
        assert!(!user_is_reading());
    }

    fn state_with_cache() -> AppState {
        let state = AppState::new(StateStore::open_in_memory().unwrap());
        let _ = state.cache.set(ExplorerCache::open_in_memory().unwrap());
        state
    }

    #[test]
    fn a_graph_round_trips_through_the_cache_with_its_time() {
        let state = state_with_cache();
        let key = scope_key(&LineageScope::default());
        assert!(state
            .cache_get::<LineageGraph>("c1", "db", kinds::LINEAGE, &key)
            .is_none());
        let graph = LineageGraph {
            notes: vec!["hello".into()],
            ..Default::default()
        };
        state.cache_put("c1", "db", kinds::LINEAGE, &key, &graph);
        let (got, at) = state
            .cache_get::<LineageGraph>("c1", "db", kinds::LINEAGE, &key)
            .unwrap();
        assert_eq!(got.notes, ["hello"]);
        assert!(is_fresh(&at, Utc::now(), TTL));
        // Another scope or database finds nothing.
        assert!(state
            .cache_get::<LineageGraph>("c1", "db", kinds::LINEAGE, "other")
            .is_none());
        assert!(state
            .cache_get::<LineageGraph>("c1", "db2", kinds::LINEAGE, &key)
            .is_none());
    }

    #[test]
    fn clearing_the_lineage_leaves_the_explorer_alone() {
        let state = state_with_cache();
        state.cache_put("c1", "db", kinds::OBJECTS, "", &vec!["t"]);
        state.cache_put("c1", "db", kinds::LINEAGE, "k", &LineageGraph::default());
        state.cache_put("c1", "db2", kinds::LINEAGE, "k", &LineageGraph::default());
        state.cache_put("c2", "db", kinds::LINEAGE, "k", &LineageGraph::default());
        state.cache_put("c1", "", kinds::LINEAGE_SOURCES, "m", &Vec::<String>::new());
        // A database's refresh.
        state.cache_forget_lineage(Some("c1"), Some("db"));
        assert!(state
            .cache_get::<LineageGraph>("c1", "db", kinds::LINEAGE, "k")
            .is_none());
        assert!(state
            .cache_get::<LineageGraph>("c1", "db2", kinds::LINEAGE, "k")
            .is_some());
        // One connection, then all.
        state.cache_forget_lineage(Some("c1"), None);
        assert!(state
            .cache_get::<LineageGraph>("c1", "db2", kinds::LINEAGE, "k")
            .is_none());
        assert!(state
            .cache_get::<Vec<String>>("c1", "", kinds::LINEAGE_SOURCES, "m")
            .is_none());
        assert!(state
            .cache_get::<LineageGraph>("c2", "db", kinds::LINEAGE, "k")
            .is_some());
        state.cache_forget_lineage(None, None);
        assert!(state
            .cache_get::<LineageGraph>("c2", "db", kinds::LINEAGE, "k")
            .is_none());
        assert!(state
            .cache_get::<Vec<String>>("c1", "db", kinds::OBJECTS, "")
            .is_some());
    }

    #[test]
    fn a_forgotten_connection_loses_its_lineage_too() {
        let state = state_with_cache();
        state.cache_put("c1", "db", kinds::LINEAGE, "k", &LineageGraph::default());
        state.cache_forget("c1");
        assert!(state
            .cache_get::<LineageGraph>("c1", "db", kinds::LINEAGE, "k")
            .is_none());
    }
}
