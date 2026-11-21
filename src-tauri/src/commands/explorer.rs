use crate::error::{CommandError, CommandResult};
use crate::state::{driver_info, meta_key, AppState, Lane, SessionEntry};
use dbine_core::cache::kinds as cache_kinds;
use dbine_driver::sql::{create_table_from_columns, Quote};
use dbine_driver::{ColumnInfo, DbObject, ObjectPage, ObjectQuery, ObjectRef, SchemaInfo};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::State;

#[derive(Deserialize)]
pub struct DatabaseArgs {
    pub connection_id: String,
    pub database: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn list_databases(
    state: State<'_, AppState>,
    args: DatabaseArgs,
) -> CommandResult<Vec<String>> {
    let dbs = state
        .meta_read(&args.connection_id, "", META_LIMIT, |s| {
            Box::pin(s.list_databases())
        })
        .await?;
    state.cache_put(&args.connection_id, "", cache_kinds::DATABASES, "", &dbs);
    Ok(dbs)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn list_objects(
    state: State<'_, AppState>,
    args: DatabaseArgs,
) -> CommandResult<Vec<DbObject>> {
    let objects = state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, |s| {
            Box::pin(s.list_objects())
        })
        .await?;
    state.cache_put(
        &args.connection_id,
        &args.database,
        cache_kinds::OBJECTS,
        "",
        &objects,
    );
    Ok(objects)
}

/// The explorer cache's entry for a database's schema list (next to
/// `objects`; absent in caches written before it: the UI then derives the
/// schemas from the objects until the server answers).
pub const CACHE_SCHEMAS: &str = "schemas";

/// A database's objects and, when the driver lists them, all its schemas
/// (empty ones included).
#[derive(Serialize)]
pub struct DatabaseObjects {
    pub objects: Vec<DbObject>,
    /// `None`: the driver doesn't list schemas.
    pub schemas: Option<Vec<SchemaInfo>>,
}

/// A whole database's objects plus the schema list, read on the same
/// session and cached together: for what truly needs every object (a
/// migration's tables, a backup's or a script's object list, the grants
/// screen). The tree reads by folder (`list_object_page`).
#[tauri::command(rename_all = "camelCase")]
pub async fn list_database_objects(
    state: State<'_, AppState>,
    args: DatabaseArgs,
) -> CommandResult<DatabaseObjects> {
    let read = state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, |s| {
            Box::pin(async move {
                let objects = s.list_objects().await?;
                // The objects are what matters: a schema list that fails
                // leaves the schemas derived from them.
                let schemas = s.list_schemas().await.unwrap_or_else(|e| {
                    tracing::warn!("list_schemas failed: {e}");
                    None
                });
                Ok(DatabaseObjects { objects, schemas })
            })
        })
        .await?;
    // A whole list read anyway: the tree's next pages come out of it.
    state.object_lists.insert(
        meta_key(&args.connection_id, &args.database),
        (std::time::Instant::now(), Arc::new(read.objects.clone())),
    );
    state.cache_put(
        &args.connection_id,
        &args.database,
        cache_kinds::OBJECTS,
        "",
        &read.objects,
    );
    state.cache_put(
        &args.connection_id,
        &args.database,
        CACHE_SCHEMAS,
        "",
        &read.schemas,
    );
    Ok(read)
}

/// The tree's schema level: every schema of the database, empty ones
/// included (`None`: the driver doesn't list them, and the tree shows its
/// kind folders across schemas). On the tree's lane; cached.
#[tauri::command(rename_all = "camelCase")]
pub async fn list_schemas(
    state: State<'_, AppState>,
    args: DatabaseArgs,
) -> CommandResult<Option<Vec<SchemaInfo>>> {
    let schemas = state
        .meta_read_on(
            Lane::Tree,
            &args.connection_id,
            &args.database,
            META_LIMIT,
            |s| Box::pin(s.list_schemas()),
        )
        .await?;
    state.cache_put(
        &args.connection_id,
        &args.database,
        CACHE_SCHEMAS,
        "",
        &schemas,
    );
    Ok(schemas)
}

/// How long a whole object list read for a driver without its own pages
/// serves the next pages (a folder's "Cargar más", the other folders, the
/// editor's searches) before it's read again.
pub const OBJECT_LIST_TTL: std::time::Duration = std::time::Duration::from_secs(60);

/// The explorer cache's entry for a folder's first page.
pub const CACHE_PAGE: &str = "page";

/// The cache item of a folder: its filters, not its page or search.
pub fn page_item(q: &ObjectQuery) -> String {
    serde_json::to_string(&(&q.schema, q.no_schema, &q.kind, &q.not_kinds)).unwrap_or_default()
}

#[derive(Deserialize)]
pub struct ObjectPageArgs {
    pub connection_id: String,
    pub database: String,
    pub query: ObjectQuery,
    /// Read again from the server (Refresh, a structure change), not out of
    /// a whole list kept from a moment ago.
    #[serde(default)]
    pub fresh: bool,
    /// Asked by the explorer tree: on the tree's lane.
    #[serde(default)]
    pub tree: bool,
}

/// One page of a database's objects: a folder of the tree (a schema's
/// objects of one kind, with "Cargar más"), a name search (completion, the
/// filter, a picker) or a lookup by name. The driver's own narrow catalog
/// query when it has one; otherwise its whole list, read once and kept
/// `OBJECT_LIST_TTL` for the next pages. A folder's first page is cached.
#[tauri::command(rename_all = "camelCase")]
pub async fn list_object_page(
    state: State<'_, AppState>,
    args: ObjectPageArgs,
) -> CommandResult<ObjectPage> {
    object_page(&state, args).await
}

/// One page out of `s`: the driver's own narrow query, or, when it has none
/// (`Unsupported`), its whole list kept in `lists` under `key` for
/// `OBJECT_LIST_TTL` (a `fresh` read takes only a list read after `asked`).
async fn page_on(
    s: &mut Box<dyn dbine_driver::Session>,
    q: &ObjectQuery,
    lists: &dashmap::DashMap<String, crate::state::KeptObjectList>,
    key: String,
    fresh: bool,
    asked: std::time::Instant,
) -> dbine_driver::Result<ObjectPage> {
    match s.list_objects_page(q).await {
        Err(dbine_driver::Error::Unsupported(_)) => {}
        r => return r,
    }
    // Checked here, on the session: requests queued behind the one that
    // read the list take it from there.
    let kept = lists
        .get(&key)
        .filter(|e| {
            if fresh {
                e.0 >= asked
            } else {
                e.0.elapsed() < OBJECT_LIST_TTL
            }
        })
        .map(|e| e.1.clone());
    let all = match kept {
        Some(all) => all,
        None => {
            let all = Arc::new(s.list_objects().await?);
            lists.insert(key, (std::time::Instant::now(), all.clone()));
            all
        }
    };
    Ok(dbine_driver::objects::page_of(&all, q))
}

/// `list_object_page`'s work.
pub async fn object_page(state: &AppState, args: ObjectPageArgs) -> CommandResult<ObjectPage> {
    let lane = if args.tree { Lane::Tree } else { Lane::Default };
    let key = meta_key(&args.connection_id, &args.database);
    let lists = state.object_lists.clone();
    let q = args.query.clone();
    let fresh = args.fresh;
    // A fresh read takes a list read after it was asked for (several folders
    // refreshed at once share one read), never an older one.
    let asked = std::time::Instant::now();
    let page = state
        .meta_read_on(
            lane,
            &args.connection_id,
            &args.database,
            META_LIMIT,
            move |s| Box::pin(async move { page_on(s, &q, &lists, key, fresh, asked).await }),
        )
        .await?;
    let q = &args.query;
    if q.after.is_none() && q.pattern.is_none() && q.name.is_none() {
        state.cache_put(
            &args.connection_id,
            &args.database,
            CACHE_PAGE,
            &page_item(q),
            &page.objects,
        );
    }
    Ok(page)
}

#[derive(Deserialize)]
pub struct CachedArgs {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    /// `databases`, `objects`, `schemas`, `columns` or `page`.
    pub kind: String,
    /// For `columns`, the object (`cache_item`).
    #[serde(default)]
    pub item: String,
    /// For `page`, the folder (its first page's query).
    #[serde(default)]
    pub query: Option<ObjectQuery>,
}

/// What the explorer showed last time for this entry (see
/// `dbine_core::cache`), to show at once while the server is asked again;
/// `null` when there's nothing yet.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_cached(
    state: State<'_, AppState>,
    args: CachedArgs,
) -> CommandResult<Option<serde_json::Value>> {
    let Some(cache) = state.cache.get() else {
        return Ok(None);
    };
    // A name search (the explorer's filter): what every folder and whole
    // list kept for the database holds, at once and without a connection.
    if let Some(q) = args
        .query
        .as_ref()
        .filter(|q| args.kind == CACHE_PAGE && q.pattern.is_some())
    {
        let payloads = cache
            .payloads(
                &args.connection_id,
                &args.database,
                &[CACHE_PAGE, cache_kinds::OBJECTS],
            )
            .unwrap_or_else(|e| {
                tracing::warn!("explorer cache read failed: {e}");
                Vec::new()
            });
        let found = cached_matches(&payloads, q.pattern.as_deref().unwrap_or(""), q.limit);
        return Ok(Some(serde_json::to_value(found).unwrap_or_default()));
    }
    let item = match &args.query {
        Some(q) => page_item(q),
        None => args.item.clone(),
    };
    let raw = cache
        .get(&args.connection_id, &args.database, &args.kind, &item)
        .unwrap_or_else(|e| {
            tracing::warn!("explorer cache read failed: {e}");
            None
        });
    // An entry from another version that no longer parses is just a miss.
    Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
}

/// The objects in cached lists (`payloads`, newest first) whose name holds
/// `pattern` (ignoring case), each once, at most `limit` (0: no cap); an
/// entry that no longer parses is skipped.
fn cached_matches(payloads: &[String], pattern: &str, limit: u32) -> Vec<DbObject> {
    let p = pattern.to_lowercase();
    let mut seen = std::collections::HashSet::new();
    let mut out = Vec::new();
    for raw in payloads {
        let Ok(objs) = serde_json::from_str::<Vec<DbObject>>(raw) else {
            continue;
        };
        for o in objs {
            if !o.name.to_lowercase().contains(&p) {
                continue;
            }
            if seen.insert((o.kind.clone(), o.schema.clone(), o.name.clone())) {
                out.push(o);
                if limit > 0 && out.len() >= limit as usize {
                    return out;
                }
            }
        }
    }
    out
}

/// The cache's name for an object's columns.
pub fn cache_item(object: &ObjectRef) -> String {
    format!(
        "{}\u{1}{}\u{1}{}",
        object.kind,
        object.schema().unwrap_or(""),
        object.name
    )
}

/// What the login may do (backups, profiler, ending sessions…), for
/// turning off the actions the server would refuse. `database` "": the
/// connection's server-level actions.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_permissions(
    state: State<'_, AppState>,
    args: DatabaseArgs,
) -> CommandResult<dbine_driver::Permissions> {
    let db = Some(args.database.clone()).filter(|d| !d.is_empty());
    // Asked when the tree opens a database: on the tree's lane.
    state
        .meta_read_on(
            Lane::Tree,
            &args.connection_id,
            &args.database,
            META_LIMIT,
            move |s| Box::pin(async move { s.permissions(db.as_deref()).await }),
        )
        .await
}

/// A table's indexes and how they're used, with the derived numbers
/// (reads, read share, unused) filled here for every driver. `None`: the
/// driver doesn't report it.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_index_usage(
    state: State<'_, AppState>,
    args: ObjectArgs,
) -> CommandResult<Option<dbine_driver::IndexUsageReport>> {
    let lane = args.lane();
    let object = args.object;
    let report = state
        .meta_read_on(
            lane,
            &args.connection_id,
            &args.database,
            META_LIMIT,
            move |s| Box::pin(async move { s.index_usage(&object).await }),
        )
        .await?;
    Ok(report.map(dbine_driver::IndexUsageReport::derived))
}

#[derive(Deserialize)]
pub struct IndexToggleArgs {
    pub connection_id: String,
    pub table: ObjectRef,
    /// As `get_index_usage` returned it.
    pub index: dbine_driver::IndexUsage,
    pub enable: bool,
}

/// "Deshabilitar / Habilitar índice": the engine's statements, with what to
/// know first. They run like a schema sync (`schema_sync_run`).
#[tauri::command(rename_all = "camelCase")]
pub async fn index_toggle_script(
    state: State<'_, AppState>,
    args: IndexToggleArgs,
) -> CommandResult<dbine_driver::SyncScript> {
    let driver = crate::commands::schema::driver_of(&state, &args.connection_id)?;
    Ok(driver.index_toggle_script(&args.table, &args.index, args.enable)?)
}

#[derive(Deserialize)]
pub struct DependentsArgs {
    pub connection_id: String,
    pub database: String,
    pub target: dbine_driver::DependencyTarget,
}

/// What depends on a table, column, view or routine ("Ver dependencias…"),
/// and on an engine with server jobs, the jobs whose steps run or name it
/// (`DependencyReport::jobs`). The generic scan reads every definition one
/// by one, so it gets longer than other catalog reads.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_dependents(
    state: State<'_, AppState>,
    args: DependentsArgs,
) -> CommandResult<dbine_driver::DependencyReport> {
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
    let jobs = driver.capabilities().jobs;
    let target = args.target;
    let database = args.database.clone();
    state
        .meta_read_on(
            Lane::Long,
            &args.connection_id,
            &args.database,
            DEPENDENTS_LIMIT,
            move |s| {
                Box::pin(async move {
                    let mut report = s.dependents(&target, &scan).await?;
                    if jobs {
                        with_jobs(
                            &mut report,
                            s.job_dependents(&database, &target, &scan).await,
                        );
                    }
                    Ok(report)
                })
            },
        )
        .await
}

/// The jobs that run the object into its dependents; when they couldn't be
/// read, a note that says why (the objects still show).
fn with_jobs(
    report: &mut dbine_driver::DependencyReport,
    jobs: dbine_driver::Result<dbine_driver::DependencyReport>,
) {
    match jobs {
        Ok(j) => dbine_driver::dependencies::merge_jobs(report, j),
        Err(e) => report
            .job_notes
            .push(format!("no se pudieron leer los jobs del servidor: {e}")),
    }
}

#[derive(Deserialize)]
pub struct JobReferencesArgs {
    pub connection_id: String,
    pub database: String,
    pub job_id: String,
}

/// What a server job's steps use (the lineage's sources of a job card): the
/// objects of `database` they name, and those of other databases as
/// `external`. On the long lane, like `get_references`.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_job_references(
    state: State<'_, AppState>,
    args: JobReferencesArgs,
) -> CommandResult<dbine_driver::DependencyReport> {
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
    let (database, job_id) = (args.database.clone(), args.job_id);
    state
        .meta_read_on(
            Lane::Long,
            &args.connection_id,
            &args.database,
            DEPENDENTS_LIMIT,
            move |s| Box::pin(async move { s.job_references(&database, &job_id, &scan).await }),
        )
        .await
}

#[derive(Deserialize)]
pub struct LineageGraphArgs {
    pub connection_id: String,
    pub database: String,
    #[serde(default)]
    pub scope: dbine_driver::dependencies::LineageScope,
}

/// The lineage of a whole database, a schema or a folder ("Ver linaje…" on
/// them): every object as a node and every dependency as an edge, with the
/// server's jobs when the engine has them. The generic scan reads every
/// definition, so it runs on the long lane like `get_dependents`.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_lineage_graph(
    state: State<'_, AppState>,
    args: LineageGraphArgs,
) -> CommandResult<dbine_driver::dependencies::LineageGraph> {
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
    let mut scope = args.scope;
    scope.jobs = scope.jobs && driver.capabilities().jobs;
    let database = args.database.clone();
    state
        .meta_read_on(
            Lane::Long,
            &args.connection_id,
            &args.database,
            DEPENDENTS_LIMIT,
            move |s| Box::pin(async move { s.lineage_graph(&database, &scope, &scan).await }),
        )
        .await
}

const DEPENDENTS_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

/// What an object itself uses (the lineage's sources): the tables a view
/// reads, the routines a procedure calls, a table's foreign keys' tables and
/// what it reads from outside the database. The other direction of
/// `get_dependents`, on the same long lane.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_references(
    state: State<'_, AppState>,
    args: DependentsArgs,
) -> CommandResult<dbine_driver::DependencyReport> {
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
    let target = args.target;
    state
        .meta_read_on(
            Lane::Long,
            &args.connection_id,
            &args.database,
            DEPENDENTS_LIMIT,
            move |s| Box::pin(async move { s.references(&target, &scan).await }),
        )
        .await
}

#[derive(Deserialize)]
pub struct ScanKeysArgs {
    pub connection_id: String,
    pub database: String,
    pub scan: dbine_driver::KeyScan,
}

/// One page of a server-side key search (Redis, etcd).
#[tauri::command(rename_all = "camelCase")]
pub async fn scan_keys(
    state: State<'_, AppState>,
    args: ScanKeysArgs,
) -> CommandResult<dbine_driver::KeyPage> {
    let scan = args.scan;
    // The tree's key folder: on the tree's lane.
    state
        .meta_read_on(
            Lane::Tree,
            &args.connection_id,
            &args.database,
            META_LIMIT,
            move |s| Box::pin(async move { s.scan_keys(&scan).await }),
        )
        .await
}

#[derive(Deserialize)]
pub struct ObjectArgs {
    pub connection_id: String,
    pub database: String,
    pub object: ObjectRef,
    /// Asked by the explorer tree (an object expanded there): read on the
    /// tree's lane, not behind the editor's autocomplete.
    #[serde(default)]
    pub tree: bool,
}

impl ObjectArgs {
    fn lane(&self) -> Lane {
        if self.tree {
            Lane::Tree
        } else {
            Lane::Default
        }
    }
}

/// Time limit of the explorer's structure reads.
pub const META_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
/// Time limit of reading a whole database's structure (big databases).
pub const SCHEMA_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

async fn meta(
    state: &AppState,
    connection_id: &str,
    database: &str,
) -> CommandResult<Arc<SessionEntry>> {
    state
        .session(&meta_key(connection_id, database), connection_id, database)
        .await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_columns(
    state: State<'_, AppState>,
    args: ObjectArgs,
) -> CommandResult<Vec<ColumnInfo>> {
    let object = args.object.clone();
    let columns = state
        .meta_read_on(
            args.lane(),
            &args.connection_id,
            &args.database,
            META_LIMIT,
            move |s| Box::pin(async move { s.columns(&object).await }),
        )
        .await?;
    state.cache_put(
        &args.connection_id,
        &args.database,
        cache_kinds::COLUMNS,
        &cache_item(&args.object),
        &columns,
    );
    Ok(columns)
}

/// Source of the object. For a table the engine gives none for, the
/// driver's DDL from the table's structure (keys, indexes, foreign keys,
/// checks); failing that, a CREATE TABLE built from its columns.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_definition(state: State<'_, AppState>, args: ObjectArgs) -> CommandResult<String> {
    let object = args.object.clone();
    let def = state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| {
            Box::pin(async move { s.definition(&object).await })
        })
        .await?;
    if let Some(def) = def {
        return Ok(def);
    }
    let not_found =
        || CommandError::NotFound("el servidor no devolvió la definición (¿sin permisos?)".into());
    if args.object.kind != dbine_driver::kinds::TABLE {
        return Err(not_found());
    }
    if let Some(ddl) = table_ddl_from_schema(&state, &args).await {
        return Ok(ddl);
    }
    let driver = state
        .store
        .get_connection(&args.connection_id)?
        .map(|c| c.config.driver)
        .unwrap_or_default();
    let info = driver_info(&driver)?;
    if info.language != dbine_driver::Language::Sql {
        return Err(not_found());
    }
    let object = args.object.clone();
    let cols = state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| {
            Box::pin(async move { s.columns(&object).await })
        })
        .await?;
    Ok(create_table_from_columns(
        quote_of(info.dialect),
        args.object.schema(),
        &args.object.name,
        &cols,
    ))
}

/// The table's full DDL (create, indexes, foreign keys) from the driver's
/// structure read. `None` when the engine can't describe or script it, so
/// the caller falls back to the columns.
async fn table_ddl_from_schema(state: &AppState, args: &ObjectArgs) -> Option<String> {
    let driver = crate::commands::schema::driver_of(state, &args.connection_id).ok()?;
    // There's no per-table structure read: the database's, within the
    // explorer's time limit.
    let tables = match state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, |s| {
            Box::pin(s.database_schema())
        })
        .await
    {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e.to_string(), "definition: database schema");
            return None;
        }
    };
    let table = find_table(&tables, &args.object)?;
    let parts = dbine_driver::DdlParts {
        create: true,
        indexes: true,
        foreign_keys: true,
        ..Default::default()
    };
    driver
        .table_ddl(table, parts)
        .ok()
        .filter(|d| !d.trim().is_empty())
}

/// The object's table among the database's: same schema, or, when the
/// object names none, the first one with that name.
fn find_table<'a>(
    tables: &'a [dbine_driver::TableSchema],
    object: &ObjectRef,
) -> Option<&'a dbine_driver::TableSchema> {
    let named = |t: &&dbine_driver::TableSchema| {
        t.name == object.name && t.kind == dbine_driver::kinds::TABLE
    };
    match object.schema() {
        Some(s) => tables
            .iter()
            .filter(named)
            .find(|t| t.schema.as_deref() == Some(s)),
        None => {
            let mut it = tables.iter().filter(named);
            let first = it
                .clone()
                .find(|t| t.schema.as_deref().is_none_or(str::is_empty));
            first.or_else(|| it.next())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::TableSchema;

    #[test]
    fn the_filter_finds_names_in_every_cached_folder_once() {
        let o = |kind: &str, schema: &str, name: &str| DbObject {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
            parent: None,
        };
        let tables =
            serde_json::to_string(&[o("table", "dbo", "Clientes"), o("table", "dbo", "Pedidos")])
                .unwrap();
        let views = serde_json::to_string(&[o("view", "dbo", "v_clientes_activos")]).unwrap();
        // An older whole list repeats a table; an entry from another version doesn't parse.
        let whole = serde_json::to_string(&[
            o("table", "dbo", "Clientes"),
            o("table", "ventas", "clientes"),
        ])
        .unwrap();
        let payloads = [tables, views, whole, "{not a list".to_string()];
        let found = cached_matches(&payloads, "CLIENTES", 0);
        let names: Vec<_> = found
            .iter()
            .map(|x| format!("{}.{}", x.schema.as_deref().unwrap(), x.name))
            .collect();
        assert_eq!(
            names,
            ["dbo.Clientes", "dbo.v_clientes_activos", "ventas.clientes"]
        );
        assert_eq!(cached_matches(&payloads, "clientes", 2).len(), 2);
        assert!(cached_matches(&payloads, "nada", 0).is_empty());
    }

    fn t(schema: Option<&str>, name: &str) -> TableSchema {
        TableSchema {
            kind: "table".into(),
            schema: schema.map(str::to_string),
            name: name.into(),
            ..Default::default()
        }
    }

    fn obj(schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef {
            kind: "table".into(),
            schema: schema.map(str::to_string),
            name: name.into(),
        }
    }

    #[test]
    fn finds_the_table_by_schema_and_name() {
        let tables = vec![
            t(Some("dbo"), "Person"),
            t(Some("hr"), "Person"),
            t(Some("hr"), "Other"),
        ];
        assert_eq!(
            find_table(&tables, &obj(Some("hr"), "Person"))
                .unwrap()
                .schema
                .as_deref(),
            Some("hr")
        );
        assert!(find_table(&tables, &obj(Some("x"), "Person")).is_none());
        assert!(find_table(&tables, &obj(Some("dbo"), "person")).is_none());
        assert_eq!(
            find_table(&tables, &obj(None, "Person"))
                .unwrap()
                .schema
                .as_deref(),
            Some("dbo")
        );
        let plain = vec![t(Some("a"), "T"), t(None, "T")];
        assert!(find_table(&plain, &obj(None, "T"))
            .unwrap()
            .schema
            .is_none());
    }

    struct Engine(dbine_driver::DriverInfo);

    #[dbine_driver::async_trait]
    impl dbine_driver::Driver for Engine {
        fn info(&self) -> &dbine_driver::DriverInfo {
            &self.0
        }
        async fn connect(
            &self,
            _: &dbine_driver::ConnectionConfig,
            _: Option<&str>,
        ) -> dbine_driver::Result<Box<dyn dbine_driver::Session>> {
            Err(dbine_driver::Error::Unsupported("test".into()))
        }
    }

    fn engine(language: dbine_driver::Language) -> Engine {
        Engine(dbine_driver::DriverInfo {
            tls: Default::default(),
            tls_port: None,
            id: "e",
            name: "E",
            family: dbine_driver::Family::Relational,
            language,
            dialect: "postgres",
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![],
        })
    }

    #[test]
    fn sort_falls_back_when_the_engine_cant() {
        let key = [dbine_driver::SortKey {
            column: "a".into(),
            descending: true,
        }];
        let q = "SELECT *\nFROM \"t\"\nLIMIT 5".to_string();
        let sql = engine(dbine_driver::Language::Sql);
        assert_eq!(
            sort_browse(&sql, q.clone(), &[]).unwrap(),
            (q.clone(), true, None)
        );
        assert_eq!(
            sort_browse(&sql, q.clone(), &key).unwrap(),
            (
                "SELECT *\nFROM \"t\"\nORDER BY \"a\" DESC\nLIMIT 5".to_string(),
                true,
                None
            )
        );
        let (query, server, why) = sort_browse(
            &engine(dbine_driver::Language::Json),
            "db.t.find({})".into(),
            &key,
        )
        .unwrap();
        assert_eq!((query.as_str(), server), ("db.t.find({})", false));
        assert!(why.is_some());
    }

    /// A driver with its own pages (SQLite): each page is one narrow catalog
    /// query, so nothing is kept and what was just made shows at once.
    #[tokio::test]
    async fn a_folder_pages_through_the_drivers_own_pages() {
        use dbine_core::StateStore;
        let dir = std::env::temp_dir().join(format!(
            "dbine-pages-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.sqlite");
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch(
            "CREATE TABLE a (id INTEGER); CREATE TABLE b (id INTEGER); CREATE TABLE c (id INTEGER);
             CREATE VIEW v1 AS SELECT 1; CREATE VIEW v2 AS SELECT 2;",
        )
        .unwrap();
        let state = AppState::new(StateStore::open_in_memory().unwrap());
        state
            .store
            .save_connection(&dbine_core::SavedConnection {
                id: "pages".into(),
                name: "pages".into(),
                color: None,
                config: dbine_driver::ConnectionConfig {
                    driver: "sqlite".into(),
                    host: file.to_string_lossy().into_owned(),
                    ..Default::default()
                },
                save_password: false,
                folder_id: None,
                tags: vec![],
                mcp_level: None,
                updated_at: String::new(),
            })
            .unwrap();
        let ask = |query: ObjectQuery, fresh: bool| ObjectPageArgs {
            connection_id: "pages".into(),
            database: String::new(),
            query,
            fresh,
            tree: true,
        };
        let tables = |after: Option<String>| ObjectQuery {
            kind: Some("table".into()),
            limit: 2,
            after,
            ..Default::default()
        };
        let names = |p: &ObjectPage| p.objects.iter().map(|o| o.name.clone()).collect::<Vec<_>>();
        let first = object_page(&state, ask(tables(None), false)).await.unwrap();
        assert_eq!(names(&first), ["a", "b"]);
        let second = object_page(&state, ask(tables(first.next.clone()), false))
            .await
            .unwrap();
        assert_eq!((names(&second), second.next), (vec!["c".to_string()], None));
        // The views' folder: the other kind only.
        let views = ObjectQuery {
            kind: Some("view".into()),
            ..Default::default()
        };
        let p = object_page(&state, ask(views, false)).await.unwrap();
        assert_eq!(names(&p), ["v1", "v2"]);
        // A table made meanwhile: each page is read from the catalog, so it's
        // there without a fresh read, and no whole list is kept.
        db.execute_batch("CREATE TABLE d (id INTEGER)").unwrap();
        let all = ObjectQuery {
            kind: Some("table".into()),
            ..Default::default()
        };
        let p = object_page(&state, ask(all, false)).await.unwrap();
        assert_eq!(names(&p), ["a", "b", "c", "d"]);
        // A lookup by name, ignoring case.
        let found = object_page(
            &state,
            ask(
                ObjectQuery {
                    name: Some("V2".into()),
                    ..Default::default()
                },
                false,
            ),
        )
        .await
        .unwrap();
        assert_eq!(names(&found), ["v2"]);
        assert!(state.object_lists.is_empty(), "the driver pages itself");
        state.close_connection_sessions("pages");
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }

    /// A real SQLite session whose driver has no pages of its own
    /// (`list_objects_page`'s default, `Unsupported`): what the kept list is
    /// for (engines that can't page, older driver hosts, a server that
    /// refuses the narrow query).
    struct Unpaged(Box<dyn dbine_driver::Session>);

    #[dbine_driver::async_trait]
    impl dbine_driver::Session for Unpaged {
        async fn server_version(&mut self) -> dbine_driver::Result<String> {
            self.0.server_version().await
        }
        async fn list_databases(&mut self) -> dbine_driver::Result<Vec<String>> {
            self.0.list_databases().await
        }
        async fn list_objects(&mut self) -> dbine_driver::Result<Vec<DbObject>> {
            self.0.list_objects().await
        }
        async fn columns(&mut self, obj: &ObjectRef) -> dbine_driver::Result<Vec<ColumnInfo>> {
            self.0.columns(obj).await
        }
        async fn definition(&mut self, obj: &ObjectRef) -> dbine_driver::Result<Option<String>> {
            self.0.definition(obj).await
        }
        fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
            self.0.browse_query(obj, limit)
        }
        async fn execute(
            &mut self,
            text: &str,
            max_rows: usize,
            out: &mut dbine_driver::QueryOutcome,
        ) -> dbine_driver::Result<()> {
            self.0.execute(text, max_rows, out).await
        }
    }

    /// A driver without its own pages: a folder's pages come out of one read
    /// of its whole list, kept for a while; "fresh" reads again; a
    /// disconnect drops it.
    #[tokio::test]
    async fn a_driver_without_pages_pages_through_a_kept_list() {
        use dbine_core::StateStore;
        let dir = std::env::temp_dir().join(format!(
            "dbine-kept-{}-{}",
            std::process::id(),
            chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)
        ));
        std::fs::create_dir_all(&dir).unwrap();
        let file = dir.join("t.sqlite");
        let db = rusqlite::Connection::open(&file).unwrap();
        db.execute_batch(
            "CREATE TABLE a (id INTEGER); CREATE TABLE b (id INTEGER); CREATE TABLE c (id INTEGER);
             CREATE VIEW v1 AS SELECT 1; CREATE VIEW v2 AS SELECT 2;",
        )
        .unwrap();
        let driver = dbine_drivers::all()
            .iter()
            .find(|d| d.info().id == "sqlite")
            .unwrap();
        let cfg = dbine_driver::ConnectionConfig {
            driver: "sqlite".into(),
            host: file.to_string_lossy().into_owned(),
            ..Default::default()
        };
        let mut s: Box<dyn dbine_driver::Session> =
            Box::new(Unpaged(driver.connect(&cfg, None).await.unwrap()));
        let state = AppState::new(StateStore::open_in_memory().unwrap());
        let key = meta_key("pages", "");
        let names = |p: &ObjectPage| p.objects.iter().map(|o| o.name.clone()).collect::<Vec<_>>();
        let tables = |after: Option<String>| ObjectQuery {
            kind: Some("table".into()),
            limit: 2,
            after,
            ..Default::default()
        };
        let now = std::time::Instant::now;
        let lists = &state.object_lists;
        let first = page_on(&mut s, &tables(None), lists, key.clone(), false, now())
            .await
            .unwrap();
        assert_eq!(names(&first), ["a", "b"]);
        let second = page_on(
            &mut s,
            &tables(first.next.clone()),
            lists,
            key.clone(),
            false,
            now(),
        )
        .await
        .unwrap();
        assert_eq!((names(&second), second.next), (vec!["c".to_string()], None));
        // The views' folder: the other kind only.
        let views = ObjectQuery {
            kind: Some("view".into()),
            ..Default::default()
        };
        let p = page_on(&mut s, &views, lists, key.clone(), false, now())
            .await
            .unwrap();
        assert_eq!(names(&p), ["v1", "v2"]);
        assert_eq!(lists.len(), 1, "one list for every page");
        // A table made meanwhile: the kept list doesn't have it yet; a fresh
        // read (Refresh, a structure change) does.
        db.execute_batch("CREATE TABLE d (id INTEGER)").unwrap();
        let all = ObjectQuery {
            kind: Some("table".into()),
            ..Default::default()
        };
        let kept = page_on(&mut s, &all, lists, key.clone(), false, now())
            .await
            .unwrap();
        assert_eq!(names(&kept), ["a", "b", "c"]);
        let fresh = page_on(&mut s, &all, lists, key.clone(), true, now())
            .await
            .unwrap();
        assert_eq!(names(&fresh), ["a", "b", "c", "d"]);
        // A lookup by name, ignoring case.
        let lookup = ObjectQuery {
            name: Some("V2".into()),
            ..Default::default()
        };
        let found = page_on(&mut s, &lookup, lists, key, false, now())
            .await
            .unwrap();
        assert_eq!(names(&found), ["v2"]);
        state.close_connection_sessions("pages");
        assert!(
            state.object_lists.is_empty(),
            "a disconnect drops the kept list"
        );
        drop(s);
        drop(db);
        std::fs::remove_dir_all(&dir).ok();
    }
}

fn quote_of(dialect: &str) -> Quote {
    match dialect {
        "mssql" | "sybase" => Quote::Bracket,
        "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" => Quote::Backtick,
        _ => Quote::Double,
    }
}

#[derive(Deserialize)]
pub struct BrowseArgs {
    pub connection_id: String,
    pub database: String,
    pub object: ObjectRef,
    pub limit: u32,
}

/// Query text that shows an object's first rows, in the driver's language.
#[tauri::command(rename_all = "camelCase")]
pub async fn browse_query(state: State<'_, AppState>, args: BrowseArgs) -> CommandResult<String> {
    let entry = meta(&state, &args.connection_id, &args.database).await?;
    let s = entry.session.lock().await;
    Ok(s.browse_query(&args.object, args.limit))
}

#[derive(Deserialize)]
pub struct FilteredBrowseArgs {
    pub connection_id: String,
    pub database: String,
    pub object: ObjectRef,
    pub limit: u32,
    #[serde(default)]
    pub filters: Vec<dbine_driver::ColumnFilter>,
    /// The grid's column sort, applied after the filters.
    #[serde(default)]
    pub sort: Vec<dbine_driver::SortKey>,
}

#[derive(serde::Serialize)]
pub struct FilteredBrowse {
    pub query: String,
    /// The filters are in `query` (the server filters). When `false`, the
    /// engine can't: `query` is the plain browse and `reason` says why; the
    /// grid filters the rows it loaded.
    pub server_side: bool,
    pub reason: Option<String>,
    /// The sort is in `query` (the server sorts; also when there's no
    /// sort). When `false`, the engine can't: `query` has no `ORDER BY` of
    /// the grid, `sort_reason` says why and the grid sorts the rows it loaded.
    pub sort_server_side: bool,
    pub sort_reason: Option<String>,
}

/// The browse query of an object with the data grid's column filters.
#[tauri::command(rename_all = "camelCase")]
pub async fn filtered_browse_query(
    state: State<'_, AppState>,
    args: FilteredBrowseArgs,
) -> CommandResult<FilteredBrowse> {
    let entry = meta(&state, &args.connection_id, &args.database).await?;
    let browse = {
        let s = entry.session.lock().await;
        s.browse_query(&args.object, args.limit)
    };
    let driver = crate::commands::schema::driver_of(&state, &args.connection_id)?;
    let (query, server_side, reason) = match driver.filtered_browse(&browse, &args.filters) {
        Ok(query) => (query, true, None),
        Err(dbine_driver::Error::Unsupported(why)) => (browse, false, Some(why)),
        Err(e) => return Err(e.into()),
    };
    let (query, sort_server_side, sort_reason) = sort_browse(driver.as_ref(), query, &args.sort)?;
    Ok(FilteredBrowse {
        query,
        server_side,
        reason,
        sort_server_side,
        sort_reason,
    })
}

/// `query` with the grid's sort, or as it was and why the engine can't.
fn sort_browse(
    driver: &dyn dbine_driver::Driver,
    query: String,
    sort: &[dbine_driver::SortKey],
) -> CommandResult<(String, bool, Option<String>)> {
    if sort.is_empty() {
        return Ok((query, true, None));
    }
    match driver.sorted_browse(&query, sort) {
        Ok(sorted) => Ok((sorted, true, None)),
        Err(dbine_driver::Error::Unsupported(why)) => Ok((query, false, Some(why))),
        Err(e) => Err(e.into()),
    }
}
