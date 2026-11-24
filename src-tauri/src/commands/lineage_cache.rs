//! The lineage's disk cache (see `.plan/architecture/background-cache.md`): the last `LineageGraph` of
//! a database per scope, kept in `cache.db` next to the explorer's entries. The UI asks the cache first
//! (`lineage_graph_cached`, instant, no server call), draws it with its age and refreshes from the server
//! (`get_lineage_graph` writes the answer back). Only names, structure and the lines of code that name an
//! object: no rows, no secrets.

use crate::error::CommandResult;
use crate::state::AppState;
use chrono::{DateTime, Utc};
use dbine_core::cache::kinds;
use dbine_driver::dependencies::{LineageGraph, LineageScope, LineageStep};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Mutex;
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

/// The scope a database's lineage is read and kept with, and the key it is kept under: the one asked
/// for, without the jobs where the engine has none. Every read that stores a lineage and the panel's
/// lookup go through here, so a lookup with the scope a read was asked with finds what it stored.
pub fn keyed_scope(scope: LineageScope, driver_has_jobs: bool) -> (LineageScope, String) {
    let scope = effective_scope(scope, driver_has_jobs);
    let key = scope_key(&scope);
    (scope, key)
}

/// The scope the lineage panel opens a whole database with (the web's `panelScope({})`): every schema
/// and kind, and the server's jobs. The precompute reads this one, so the panel finds it.
pub fn panel_scope() -> LineageScope {
    LineageScope {
        jobs: true,
        ..Default::default()
    }
}

/// The cached lineage of a database for the scope the panel asks with (`lineage_graph_cached`).
pub fn cached_lineage(
    state: &AppState,
    connection_id: &str,
    database: &str,
    scope: LineageScope,
    driver_has_jobs: bool,
) -> CachedLineage {
    let (_, key) = keyed_scope(scope, driver_has_jobs);
    match state.cache_get::<LineageGraph>(connection_id, database, kinds::LINEAGE, &key) {
        Some((graph, at)) => CachedLineage {
            graph: Some(graph),
            updated_at: Some(at),
        },
        None => CachedLineage {
            graph: None,
            updated_at: None,
        },
    }
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

// -- the graph read in chunks ----------------------------------------------------------------------

/// A chunk of a graph is its own call with its own limit: the user's setting that bounds opening a lineage
/// (Settings shows 900 s when it was never changed, so that is the default here too). The first stage
/// reads the whole model's structure in one call, so no hidden ceiling below the setting's own maximum.
pub const CHUNK_LIMIT_DEFAULT: u64 = 900;
pub const CHUNK_LIMIT_MIN: u64 = 10;
pub const CHUNK_LIMIT_MAX: u64 = 3600;
/// The setting `get_lineage_graph` also uses for its single call.
pub const OPEN_LIMIT_SETTING: &str = "lineage_open_timeout_secs";

pub fn chunk_limit(setting: Option<&serde_json::Value>) -> Duration {
    let secs = setting
        .and_then(|v| v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)))
        .map_or(CHUNK_LIMIT_DEFAULT, |n| {
            n.clamp(CHUNK_LIMIT_MIN, CHUNK_LIMIT_MAX)
        });
    Duration::from_secs(secs)
}

/// The graphs being read in chunks, by connection, database and scope: what the chunks so far add up to
/// (the disk cache has the same, when the company allows it).
static CHUNKED: Mutex<Option<HashMap<String, LineageGraph>>> = Mutex::new(None);

fn chunk_key(connection_id: &str, database: &str, scope_key: &str) -> String {
    format!("{connection_id}\u{0}{database}\u{0}{scope_key}")
}

/// What a chunk adds to the graph so far, or the graph itself when `at` starts the read.
///
/// - the start: the chunk is the graph;
/// - a step: absorbed, and `next` is the chunk's (what the driver says follows);
/// - a retry of a chunk that had failed: absorbed and no longer failed, `next` stays where it was;
/// - a chunk given up on (read with `skip`): it is added to `failed`, `next` is the one after it.
pub fn merge_chunk(
    so_far: Option<LineageGraph>,
    chunk: LineageGraph,
    at: Option<&LineageStep>,
    retry: bool,
) -> Result<LineageGraph, String> {
    let Some(at) = at else {
        return Ok(chunk);
    };
    let Some(mut g) = so_far else {
        return Err("no hay un linaje a medio leer para continuar: abrilo de nuevo".into());
    };
    let next = chunk.next.clone();
    g.absorb(chunk);
    if at.skip {
        let lost = at.as_failed();
        if !g.failed.contains(&lost) {
            g.failed.push(lost);
        }
        g.next = next;
    } else if retry {
        let done = at.as_failed();
        g.failed.retain(|f| *f != done);
    } else {
        g.next = next;
    }
    Ok(g)
}

/// The stage whose chunks are ranges of objects (`len` of them from `offset`, of `total`): the chunks
/// that follow one can be named before it is read.
const RANGED_STAGE: &str = "dependencies";

/// The chunks read together from `at`: it and, in a ranged stage, up to `n - 1` that follow it in that
/// stage. A chunk given up is never read, so it goes alone.
pub fn batch_from(at: &LineageStep, n: usize) -> Vec<LineageStep> {
    let mut steps = vec![at.clone()];
    if at.skip || at.stage != RANGED_STAGE || at.len == 0 {
        return steps;
    }
    let mut offset = at.offset.saturating_add(at.len);
    while steps.len() < n && offset < at.total {
        steps.push(LineageStep {
            stage: at.stage.clone(),
            offset,
            len: at.len.min(at.total - offset),
            total: at.total,
            skip: false,
        });
        offset = offset.saturating_add(at.len);
    }
    steps
}

/// The chunks read after the one asked for (`steps`, with what each read answered), merged in order
/// as reading them one after the other would: up to the first that failed (the next call starts from
/// it) or that isn't the one the graph says is next (a driver that cuts its chunks otherwise goes on
/// from its own `next`).
pub fn merge_following<E: std::fmt::Display>(
    mut graph: LineageGraph,
    steps: &[LineageStep],
    reads: impl IntoIterator<Item = Result<LineageGraph, E>>,
) -> Result<LineageGraph, String> {
    for (step, read) in steps.iter().zip(reads) {
        if graph.next.as_ref() != Some(step) {
            break;
        }
        match read {
            Ok(chunk) => graph = merge_chunk(Some(graph), chunk, Some(step), false)?,
            Err(e) => {
                tracing::debug!("lineage chunk {step:?}: {e}");
                break;
            }
        }
    }
    Ok(graph)
}

#[derive(Deserialize)]
pub struct LineageChunkArgs {
    pub connection_id: String,
    pub database: String,
    #[serde(default)]
    pub scope: LineageScope,
    /// The chunk to read (`LineageGraph::next` of what the last call answered); `None`: start over.
    #[serde(default)]
    pub step: Option<LineageStep>,
    /// Read again a chunk that is in `LineageGraph::failed`.
    #[serde(default)]
    pub retry: bool,
    /// Give `step` up (it failed again): it isn't read, it is left in `failed` and the next one is said.
    #[serde(default)]
    pub give_up: bool,
}

/// One chunk of a database's lineage: the graph so far plus what that chunk read, with `next` saying
/// which chunk to ask for to go on (`null`: it is all there). Every chunk is written to the cache, so a
/// lineage left half way continues from there at the next opening. A driver that doesn't read in
/// chunks answers the whole graph at the start, with nothing next. The chunks of a ranged stage are read
/// several at once ([`batch_from`]), so one call may answer more than the chunk asked for.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_lineage_chunk(
    state: State<'_, AppState>,
    args: LineageChunkArgs,
) -> CommandResult<LineageGraph> {
    let id = state
        .store
        .get_connection(&args.connection_id)?
        .map(|c| c.config.driver)
        .unwrap_or_default();
    let driver = dbine_drivers::find(&id)
        .ok_or_else(|| dbine_driver::Error::Unsupported(format!("driver desconocido: {id}")))?;
    let scan = dbine_driver::DependencyScan::new(
        driver.info(),
        driver.script_dialect(),
        driver.capabilities().foreign_keys,
    );
    let (scope, key) = keyed_scope(args.scope.clone(), driver.capabilities().jobs);
    let _reading = UserRead::begin();
    let limit = chunk_limit(
        state
            .store
            .get_setting(OPEN_LIMIT_SETTING)
            .ok()
            .flatten()
            .as_ref(),
    );
    let read = |step: LineageStep| {
        let mut scope = scope.clone();
        scope.step = Some(step);
        let (database, scan) = (args.database.clone(), scan.clone());
        state.meta_read_on(
            crate::state::Lane::Long,
            &args.connection_id,
            &args.database,
            limit,
            move |s| Box::pin(async move { s.lineage_graph(&database, &scope, &scan).await }),
        )
    };
    chunk_with(&state, &args, &key, is_cacheable(&scope), read).await
}

/// `get_lineage_chunk` past the driver: the chunks to read (the one asked for and, when its stage's
/// chunks are known ahead, the ones after it), read at once through `read` (each on a session of the
/// long lane of its own), merged in order onto the graph so far, and kept in memory and in the cache.
pub(crate) async fn chunk_with<F, Fut>(
    state: &AppState,
    args: &LineageChunkArgs,
    key: &str,
    cacheable: bool,
    read: F,
) -> CommandResult<LineageGraph>
where
    F: Fn(LineageStep) -> Fut,
    Fut: std::future::Future<Output = CommandResult<LineageGraph>>,
{
    let mem_key = chunk_key(&args.connection_id, &args.database, key);
    let mut ask = args.step.clone().unwrap_or_else(LineageStep::start);
    ask.skip = args.give_up;
    let steps = if args.step.is_some() && !args.retry && !args.give_up {
        batch_from(&ask, crate::state::LONG_SESSIONS)
    } else {
        vec![ask]
    };
    let reads = futures_util::future::join_all(steps.iter().cloned().map(&read)).await;
    let mut reads = reads.into_iter();
    // The chunk asked for: its failure is the call's (the UI asks it again, then gives it up).
    let chunk = reads.next().expect("one read per step")?;
    let so_far = if args.step.is_some() {
        CHUNKED
            .lock()
            .ok()
            .and_then(|m| m.as_ref().and_then(|m| m.get(&mem_key).cloned()))
            .or_else(|| {
                state
                    .cache_get::<LineageGraph>(
                        &args.connection_id,
                        &args.database,
                        kinds::LINEAGE,
                        key,
                    )
                    .map(|(g, _)| g)
            })
    } else {
        None
    };
    let graph = merge_chunk(so_far, chunk, args.step.as_ref(), args.retry)
        .and_then(|g| merge_following(g, &steps[1..], reads))
        .map_err(dbine_driver::Error::Query)?;
    if let Ok(mut m) = CHUNKED.lock() {
        let m = m.get_or_insert_with(HashMap::new);
        if graph.next.is_none() && graph.failed.is_empty() {
            m.remove(&mem_key);
        } else {
            m.insert(mem_key, graph.clone());
        }
    }
    if cacheable {
        state.cache_put(
            &args.connection_id,
            &args.database,
            kinds::LINEAGE,
            key,
            &graph,
        );
    }
    Ok(graph)
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
    Ok(cached_lineage(
        &state,
        &args.connection_id,
        &args.database,
        args.scope,
        jobs,
    ))
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

    /// The scope the web panel sends (`panelScope` in lineageOpen.ts adds `jobs: true`), as JSON.
    fn from_panel(json: &str) -> LineageScope {
        serde_json::from_str(json).unwrap()
    }

    fn notes(c: CachedLineage) -> Option<Vec<String>> {
        c.graph.map(|g| g.notes)
    }

    /// Regression: the panel looked the cache up without the jobs it reads with, so on every engine
    /// that has jobs (Power BI, SQL Server, Oracle…) each opening read the whole lineage again, a
    /// half-read one started over, and the precompute was never shown.
    #[test]
    fn the_panel_finds_what_the_precompute_stored() {
        let state = state_with_cache();
        for (db, jobs) in [("with-jobs", true), ("without-jobs", false)] {
            let (_, key) = keyed_scope(panel_scope(), jobs);
            state.cache_put("c1", db, kinds::LINEAGE, &key, &chunk(None, "warm"));
            assert_eq!(
                notes(cached_lineage(
                    &state,
                    "c1",
                    db,
                    from_panel(r#"{"jobs":true}"#),
                    jobs
                )),
                Some(vec!["warm".to_string()]),
                "{db}"
            );
        }
        // Without the jobs it is another lineage where the engine has them: the panel must look up
        // with the scope it reads with.
        assert!(
            cached_lineage(&state, "c1", "with-jobs", from_panel("{}"), true)
                .graph
                .is_none()
        );
    }

    #[test]
    fn the_panel_finds_what_its_own_read_stored() {
        let state = state_with_cache();
        let asked = r#"{"schemas":["dbo"],"kinds":["view"],"limit":50,"jobs":true}"#;
        for (db, jobs) in [("with-jobs", true), ("without-jobs", false)] {
            // What get_lineage_graph and get_lineage_chunk store for the scope they were asked with.
            let (read, key) = keyed_scope(from_panel(asked), jobs);
            assert_eq!(read.jobs, jobs, "the jobs only where the engine has them");
            state.cache_put("c1", db, kinds::LINEAGE, &key, &chunk(None, "opened"));
            assert_eq!(
                notes(cached_lineage(&state, "c1", db, from_panel(asked), jobs)),
                Some(vec!["opened".to_string()]),
                "{db}"
            );
        }
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

    fn step(stage: &str, offset: u32) -> LineageStep {
        LineageStep {
            stage: stage.into(),
            offset,
            len: 2,
            total: 6,
            skip: false,
        }
    }

    fn chunk(next: Option<LineageStep>, note: &str) -> LineageGraph {
        LineageGraph {
            notes: vec![note.into()],
            next,
            ..Default::default()
        }
    }

    #[test]
    fn chunks_add_up_and_say_what_is_next() {
        // The start is the graph; a step is added to what there is.
        let g = merge_chunk(None, chunk(Some(step("dependencies", 0)), "a"), None, false).unwrap();
        assert_eq!(g.next.as_ref().unwrap().stage, "dependencies");
        let g = merge_chunk(
            Some(g),
            chunk(Some(step("dependencies", 2)), "b"),
            Some(&step("dependencies", 0)),
            false,
        )
        .unwrap();
        assert_eq!(g.notes, ["a", "b"]);
        assert_eq!(g.next.as_ref().unwrap().offset, 2);
        // A step with nothing to continue from asks to open it again.
        assert!(merge_chunk(
            None,
            chunk(None, "x"),
            Some(&step("dependencies", 2)),
            false
        )
        .is_err());
    }

    #[test]
    fn a_chunk_given_up_is_kept_as_failed_and_a_retry_clears_it() {
        let g = chunk(Some(step("dependencies", 2)), "a");
        let lost = LineageStep {
            skip: true,
            ..step("dependencies", 2)
        };
        let g = merge_chunk(
            Some(g),
            chunk(Some(step("dependencies", 4)), "b"),
            Some(&lost),
            false,
        )
        .unwrap();
        assert_eq!(
            g.failed,
            [step("dependencies", 2)],
            "no skip flag on the record"
        );
        assert_eq!(g.next.as_ref().unwrap().offset, 4, "goes on after it");
        // Reading it again later: no longer failed, and the read goes on where it was.
        let g = merge_chunk(
            Some(g),
            chunk(Some(step("dependencies", 4)), "c"),
            Some(&step("dependencies", 2)),
            true,
        )
        .unwrap();
        assert!(g.failed.is_empty());
        assert_eq!(g.next.as_ref().unwrap().offset, 4);
        assert_eq!(g.notes, ["a", "b", "c"]);
    }

    #[test]
    fn the_dependencies_after_a_chunk_are_read_with_it() {
        // 6 objects, 2 a chunk: from the first, the three of the stage.
        let from = step("dependencies", 0);
        assert_eq!(
            batch_from(&from, 3),
            [
                from.clone(),
                step("dependencies", 2),
                step("dependencies", 4)
            ]
        );
        assert_eq!(batch_from(&from, 2).len(), 2, "never more than asked");
        // The stage's end: the last chunk is shorter, and nothing follows it.
        let odd = LineageStep {
            total: 5,
            ..step("dependencies", 2)
        };
        let batch = batch_from(&odd, 3);
        assert_eq!(batch.len(), 2);
        assert_eq!((batch[1].offset, batch[1].len), (4, 1));
        // Other stages, and a chunk given up, go alone.
        assert_eq!(batch_from(&LineageStep::start(), 3).len(), 1);
        assert_eq!(batch_from(&step("consumers", 0), 3).len(), 1);
        let lost = LineageStep { skip: true, ..from };
        assert_eq!(batch_from(&lost, 3), [lost]);
    }

    #[test]
    fn chunks_read_together_merge_as_one_after_the_other() {
        let (b, c) = (step("dependencies", 2), step("dependencies", 4));
        let last = Some(step("consumers", 0));
        let reads = || {
            vec![
                Ok::<_, String>(chunk(Some(c.clone()), "b")),
                Ok(chunk(last.clone(), "c")),
            ]
        };
        // One after the other.
        let mut one = chunk(Some(b.clone()), "a");
        for (s, r) in [b.clone(), c.clone()].iter().zip(reads()) {
            one = merge_chunk(Some(one), r.unwrap(), Some(s), false).unwrap();
        }
        // Together.
        let together = merge_following(
            chunk(Some(b.clone()), "a"),
            &[b.clone(), c.clone()],
            reads(),
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&together).unwrap(),
            serde_json::to_value(&one).unwrap()
        );
        assert_eq!(together.notes, ["a", "b", "c"]);
        assert_eq!(together.next, last);
        // A chunk that failed ends the merge there: the next call reads it again, and what was read
        // after it is not taken (it would leave a hole).
        let failed = merge_following(
            chunk(Some(b.clone()), "a"),
            &[b.clone(), c.clone()],
            vec![Err("timeout".to_string()), Ok(chunk(last.clone(), "c"))],
        )
        .unwrap();
        assert_eq!(failed.notes, ["a"]);
        assert_eq!(failed.next, Some(b.clone()));
        // A driver whose next chunk isn't the one guessed: only what follows its own `next` is taken.
        let other = LineageStep {
            len: 3,
            ..b.clone()
        };
        let kept =
            merge_following(chunk(Some(other.clone()), "a"), &[b, c.clone()], reads()).unwrap();
        assert_eq!(kept.notes, ["a"]);
        assert_eq!(kept.next, Some(other));
    }

    // -- get_lineage_chunk past the driver (chunk_with), against a fake Power BI ------------------

    fn chunk_args(connection: &str, step: Option<LineageStep>, retry: bool) -> LineageChunkArgs {
        LineageChunkArgs {
            connection_id: connection.into(),
            database: "Indicadores".into(),
            scope: LineageScope::default(),
            step,
            retry,
            give_up: false,
        }
    }

    /// A model of 6 dependency objects, 2 a chunk, then the consumers: each chunk's notes say which it
    /// is and its `next` is the one after it, as the Power BI driver answers.
    fn fake_chunk(at: &LineageStep) -> LineageGraph {
        let next = if at.offset + 2 < 6 {
            Some(step("dependencies", at.offset + 2))
        } else {
            Some(step("consumers", 0))
        };
        chunk(next, &format!("d{}", at.offset))
    }

    fn notes_of(g: &LineageGraph) -> Vec<String> {
        g.notes.clone()
    }

    #[tokio::test]
    async fn the_chunks_after_the_one_asked_are_read_at_once_and_merged_in_order() {
        let state = state_with_cache();
        let (_, key) = keyed_scope(LineageScope::default(), true);
        // What the structure chunk left: the graph so far, next the first dependencies.
        state.cache_put(
            "pbi-batch",
            "Indicadores",
            kinds::LINEAGE,
            &key,
            &chunk(Some(step("dependencies", 0)), "s"),
        );
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        // Every read waits for the other two: read one after the other, this never ends.
        let together = std::sync::Arc::new(tokio::sync::Barrier::new(crate::state::LONG_SESSIONS));
        let read = |s: LineageStep| {
            let (asked, together) = (asked.clone(), together.clone());
            async move {
                asked.lock().unwrap().push(s.offset);
                together.wait().await;
                Ok(fake_chunk(&s))
            }
        };
        let args = chunk_args("pbi-batch", Some(step("dependencies", 0)), false);
        let g = tokio::time::timeout(
            Duration::from_secs(5),
            chunk_with(&state, &args, &key, true, read),
        )
        .await
        .expect("the three chunks are read at once")
        .unwrap();
        let mut offsets = asked.lock().unwrap().clone();
        offsets.sort();
        assert_eq!(offsets, [0, 2, 4]);
        assert_eq!(
            notes_of(&g),
            ["s", "d0", "d2", "d4"],
            "in order, onto what was there"
        );
        assert_eq!(g.next, Some(step("consumers", 0)));
        // Kept for the next call and the next opening.
        let (kept, _) = state
            .cache_get::<LineageGraph>("pbi-batch", "Indicadores", kinds::LINEAGE, &key)
            .unwrap();
        assert_eq!(notes_of(&kept), ["s", "d0", "d2", "d4"]);
    }

    #[tokio::test]
    async fn a_chunk_that_fails_in_the_batch_is_where_the_next_call_starts() {
        let state = state_with_cache();
        let (_, key) = keyed_scope(LineageScope::default(), true);
        state.cache_put(
            "pbi-fail",
            "Indicadores",
            kinds::LINEAGE,
            &key,
            &chunk(Some(step("dependencies", 0)), "s"),
        );
        let read = |s: LineageStep| async move {
            if s.offset == 2 {
                Err(crate::error::CommandError::Busy("timeout".into()))
            } else {
                Ok(fake_chunk(&s))
            }
        };
        let args = chunk_args("pbi-fail", Some(step("dependencies", 0)), false);
        let g = chunk_with(&state, &args, &key, true, read).await.unwrap();
        assert_eq!(
            notes_of(&g),
            ["s", "d0"],
            "what came after the failure is not taken"
        );
        assert_eq!(
            g.next,
            Some(step("dependencies", 2)),
            "the next call reads it again"
        );
        assert!(g.failed.is_empty(), "not given up: the UI retries it first");
    }

    #[tokio::test]
    async fn the_chunk_asked_failing_is_the_calls_error_and_nothing_is_kept() {
        let state = state_with_cache();
        let (_, key) = keyed_scope(LineageScope::default(), true);
        let before = chunk(Some(step("dependencies", 0)), "s");
        state.cache_put("pbi-first", "Indicadores", kinds::LINEAGE, &key, &before);
        let read = |s: LineageStep| async move {
            if s.offset == 0 {
                Err(crate::error::CommandError::Busy("timeout".into()))
            } else {
                Ok(fake_chunk(&s))
            }
        };
        let args = chunk_args("pbi-first", Some(step("dependencies", 0)), false);
        assert!(chunk_with(&state, &args, &key, true, read).await.is_err());
        let (kept, _) = state
            .cache_get::<LineageGraph>("pbi-first", "Indicadores", kinds::LINEAGE, &key)
            .unwrap();
        assert_eq!(notes_of(&kept), ["s"], "the graph so far is untouched");
    }

    #[tokio::test]
    async fn a_retry_and_the_start_read_only_the_chunk_asked() {
        let state = state_with_cache();
        let (_, key) = keyed_scope(LineageScope::default(), true);
        let mut so_far = chunk(Some(step("dependencies", 4)), "s");
        so_far.failed.push(step("dependencies", 2));
        state.cache_put("pbi-retry", "Indicadores", kinds::LINEAGE, &key, &so_far);
        let asked = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let read = |s: LineageStep| {
            let asked = asked.clone();
            async move {
                asked.lock().unwrap().push(s.clone());
                Ok(fake_chunk(&s))
            }
        };
        // "Reintentar" one chunk given up: only it, and it leaves `failed`.
        let args = chunk_args("pbi-retry", Some(step("dependencies", 2)), true);
        let g = chunk_with(&state, &args, &key, true, &read).await.unwrap();
        assert_eq!(*asked.lock().unwrap(), [step("dependencies", 2)]);
        assert!(g.failed.is_empty());
        assert_eq!(
            g.next,
            Some(step("dependencies", 4)),
            "the read goes on where it was"
        );
        // The start: only the structure.
        asked.lock().unwrap().clear();
        let args = chunk_args("pbi-retry", None, false);
        chunk_with(&state, &args, &key, true, &read).await.unwrap();
        assert_eq!(*asked.lock().unwrap(), [LineageStep::start()]);
    }

    #[test]
    fn a_chunk_has_its_own_short_limit() {
        use serde_json::json;
        assert_eq!(chunk_limit(None), Duration::from_secs(900));
        assert_eq!(chunk_limit(Some(&json!(30))), Duration::from_secs(30));
        // The setting bounds the whole opening (up to an hour): a chunk never waits that long.
        assert_eq!(chunk_limit(Some(&json!(3600))), Duration::from_secs(3600));
        assert_eq!(chunk_limit(Some(&json!(600))), Duration::from_secs(600));
        assert_eq!(chunk_limit(Some(&json!(99999))), Duration::from_secs(3600));
        assert_eq!(chunk_limit(Some(&json!(1))), Duration::from_secs(10));
    }

    #[test]
    fn a_partial_graph_round_trips_through_the_cache() {
        let state = state_with_cache();
        let key = scope_key(&LineageScope::default());
        let mut g = chunk(Some(step("dependencies", 2)), "a");
        g.failed.push(step("dependencies", 0));
        state.cache_put("c1", "db", kinds::LINEAGE, &key, &g);
        let (got, _) = state
            .cache_get::<LineageGraph>("c1", "db", kinds::LINEAGE, &key)
            .unwrap();
        assert_eq!(got.next, g.next);
        assert_eq!(got.failed, g.failed);
        // A graph cached before chunks existed has neither: it is complete.
        let old: LineageGraph = serde_json::from_str(r#"{"objects":[],"edges":[]}"#).unwrap();
        assert!(old.next.is_none() && old.failed.is_empty());
    }
}
