use crate::error::{CommandError, CommandResult};
use crate::state::{driver_info, meta_key, AppState, SessionEntry};
use dbine_driver::sql::{create_table_from_columns, Quote};
use dbine_core::cache::kinds as cache_kinds;
use dbine_driver::{ColumnInfo, DbObject, ObjectRef, SchemaInfo};
use serde::{Deserialize, Serialize};
use std::sync::Arc;
use tauri::State;

#[derive(Deserialize)]
pub struct DatabaseArgs {
    pub connection_id: String,
    pub database: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn list_databases(state: State<'_, AppState>, args: DatabaseArgs) -> CommandResult<Vec<String>> {
    let dbs = state.meta_read(&args.connection_id, "", META_LIMIT, |s| Box::pin(s.list_databases())).await?;
    state.cache_put(&args.connection_id, "", cache_kinds::DATABASES, "", &dbs);
    Ok(dbs)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn list_objects(state: State<'_, AppState>, args: DatabaseArgs) -> CommandResult<Vec<DbObject>> {
    let objects = state.meta_read(&args.connection_id, &args.database, META_LIMIT, |s| Box::pin(s.list_objects())).await?;
    state.cache_put(&args.connection_id, &args.database, cache_kinds::OBJECTS, "", &objects);
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

/// What the explorer tree loads for a database: `list_objects` plus the
/// schema list, read on the same session and cached together.
#[tauri::command(rename_all = "camelCase")]
pub async fn list_database_objects(state: State<'_, AppState>, args: DatabaseArgs) -> CommandResult<DatabaseObjects> {
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
    state.cache_put(&args.connection_id, &args.database, cache_kinds::OBJECTS, "", &read.objects);
    state.cache_put(&args.connection_id, &args.database, CACHE_SCHEMAS, "", &read.schemas);
    Ok(read)
}

#[derive(Deserialize)]
pub struct CachedArgs {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
    /// `databases`, `objects`, `schemas` or `columns`.
    pub kind: String,
    /// For `columns`, the object (`cache_item`).
    #[serde(default)]
    pub item: String,
}

/// What the explorer showed last time for this entry (see
/// `dbine_core::cache`), to show at once while the server is asked again;
/// `null` when there's nothing yet.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_cached(state: State<'_, AppState>, args: CachedArgs) -> CommandResult<Option<serde_json::Value>> {
    let Some(cache) = state.cache.get() else { return Ok(None) };
    let raw = cache.get(&args.connection_id, &args.database, &args.kind, &args.item).unwrap_or_else(|e| {
        tracing::warn!("explorer cache read failed: {e}");
        None
    });
    // An entry from another version that no longer parses is just a miss.
    Ok(raw.and_then(|r| serde_json::from_str(&r).ok()))
}

/// The cache's name for an object's columns.
pub fn cache_item(object: &ObjectRef) -> String {
    format!("{}\u{1}{}\u{1}{}", object.kind, object.schema().unwrap_or(""), object.name)
}

/// What the login may do (backups, profiler, ending sessions…), for
/// turning off the actions the server would refuse. `database` "": the
/// connection's server-level actions.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_permissions(state: State<'_, AppState>, args: DatabaseArgs) -> CommandResult<dbine_driver::Permissions> {
    let db = Some(args.database.clone()).filter(|d| !d.is_empty());
    state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| Box::pin(async move { s.permissions(db.as_deref()).await }))
        .await
}

/// A table's indexes and how they're used, with the derived numbers
/// (reads, read share, unused) filled here for every driver. `None`: the
/// driver doesn't report it.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_index_usage(state: State<'_, AppState>, args: ObjectArgs) -> CommandResult<Option<dbine_driver::IndexUsageReport>> {
    let object = args.object;
    let report = state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| Box::pin(async move { s.index_usage(&object).await }))
        .await?;
    Ok(report.map(dbine_driver::IndexUsageReport::derived))
}

#[derive(Deserialize)]
pub struct ScanKeysArgs {
    pub connection_id: String,
    pub database: String,
    pub scan: dbine_driver::KeyScan,
}

/// One page of a server-side key search (Redis, etcd).
#[tauri::command(rename_all = "camelCase")]
pub async fn scan_keys(state: State<'_, AppState>, args: ScanKeysArgs) -> CommandResult<dbine_driver::KeyPage> {
    let scan = args.scan;
    state.meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| Box::pin(async move { s.scan_keys(&scan).await })).await
}

#[derive(Deserialize)]
pub struct ObjectArgs {
    pub connection_id: String,
    pub database: String,
    pub object: ObjectRef,
}

/// Time limit of the explorer's structure reads.
pub const META_LIMIT: std::time::Duration = std::time::Duration::from_secs(60);
/// Time limit of reading a whole database's structure (big databases).
pub const SCHEMA_LIMIT: std::time::Duration = std::time::Duration::from_secs(300);

async fn meta(state: &AppState, connection_id: &str, database: &str) -> CommandResult<Arc<SessionEntry>> {
    state.session(&meta_key(connection_id, database), connection_id, database).await
}

#[tauri::command(rename_all = "camelCase")]
pub async fn get_columns(state: State<'_, AppState>, args: ObjectArgs) -> CommandResult<Vec<ColumnInfo>> {
    let object = args.object.clone();
    let columns = state.meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| Box::pin(async move { s.columns(&object).await })).await?;
    state.cache_put(&args.connection_id, &args.database, cache_kinds::COLUMNS, &cache_item(&args.object), &columns);
    Ok(columns)
}

/// Source of the object. For a table the engine gives none for, the
/// driver's DDL from the table's structure (keys, indexes, foreign keys,
/// checks); failing that, a CREATE TABLE built from its columns.
#[tauri::command(rename_all = "camelCase")]
pub async fn get_definition(state: State<'_, AppState>, args: ObjectArgs) -> CommandResult<String> {
    let object = args.object.clone();
    let def = state
        .meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| Box::pin(async move { s.definition(&object).await }))
        .await?;
    if let Some(def) = def {
        return Ok(def);
    }
    let not_found = || CommandError::NotFound("el servidor no devolvió la definición (¿sin permisos?)".into());
    if args.object.kind != dbine_driver::kinds::TABLE {
        return Err(not_found());
    }
    if let Some(ddl) = table_ddl_from_schema(&state, &args).await {
        return Ok(ddl);
    }
    let driver = state.store.get_connection(&args.connection_id)?.map(|c| c.config.driver).unwrap_or_default();
    let info = driver_info(&driver)?;
    if info.language != dbine_driver::Language::Sql {
        return Err(not_found());
    }
    let object = args.object.clone();
    let cols = state.meta_read(&args.connection_id, &args.database, META_LIMIT, move |s| Box::pin(async move { s.columns(&object).await })).await?;
    Ok(create_table_from_columns(quote_of(info.dialect), args.object.schema(), &args.object.name, &cols))
}

/// The table's full DDL (create, indexes, foreign keys) from the driver's
/// structure read. `None` when the engine can't describe or script it, so
/// the caller falls back to the columns.
async fn table_ddl_from_schema(state: &AppState, args: &ObjectArgs) -> Option<String> {
    let driver = crate::commands::schema::driver_of(state, &args.connection_id).ok()?;
    // There's no per-table structure read: the database's, within the
    // explorer's time limit.
    let tables = match state.meta_read(&args.connection_id, &args.database, META_LIMIT, |s| Box::pin(s.database_schema())).await {
        Ok(t) => t,
        Err(e) => {
            tracing::debug!(error = %e.to_string(), "definition: database schema");
            return None;
        }
    };
    let table = find_table(&tables, &args.object)?;
    let parts = dbine_driver::DdlParts { create: true, indexes: true, foreign_keys: true, ..Default::default() };
    driver.table_ddl(table, parts).ok().filter(|d| !d.trim().is_empty())
}

/// The object's table among the database's: same schema, or, when the
/// object names none, the first one with that name.
fn find_table<'a>(tables: &'a [dbine_driver::TableSchema], object: &ObjectRef) -> Option<&'a dbine_driver::TableSchema> {
    let named = |t: &&dbine_driver::TableSchema| t.name == object.name && t.kind == dbine_driver::kinds::TABLE;
    match object.schema() {
        Some(s) => tables.iter().filter(named).find(|t| t.schema.as_deref() == Some(s)),
        None => {
            let mut it = tables.iter().filter(named);
            let first = it.clone().find(|t| t.schema.as_deref().is_none_or(str::is_empty));
            first.or_else(|| it.next())
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::TableSchema;

    fn t(schema: Option<&str>, name: &str) -> TableSchema {
        TableSchema { kind: "table".into(), schema: schema.map(str::to_string), name: name.into(), ..Default::default() }
    }

    fn obj(schema: Option<&str>, name: &str) -> ObjectRef {
        ObjectRef { kind: "table".into(), schema: schema.map(str::to_string), name: name.into() }
    }

    #[test]
    fn finds_the_table_by_schema_and_name() {
        let tables = vec![t(Some("dbo"), "Person"), t(Some("hr"), "Person"), t(Some("hr"), "Other")];
        assert_eq!(find_table(&tables, &obj(Some("hr"), "Person")).unwrap().schema.as_deref(), Some("hr"));
        assert!(find_table(&tables, &obj(Some("x"), "Person")).is_none());
        assert!(find_table(&tables, &obj(Some("dbo"), "person")).is_none());
        assert_eq!(find_table(&tables, &obj(None, "Person")).unwrap().schema.as_deref(), Some("dbo"));
        let plain = vec![t(Some("a"), "T"), t(None, "T")];
        assert!(find_table(&plain, &obj(None, "T")).unwrap().schema.is_none());
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
}

#[derive(serde::Serialize)]
pub struct FilteredBrowse {
    pub query: String,
    /// The filters are in `query` (the server filters). When `false`, the
    /// engine can't: `query` is the plain browse and `reason` says why; the
    /// grid filters the rows it loaded.
    pub server_side: bool,
    pub reason: Option<String>,
}

/// The browse query of an object with the data grid's column filters.
#[tauri::command(rename_all = "camelCase")]
pub async fn filtered_browse_query(state: State<'_, AppState>, args: FilteredBrowseArgs) -> CommandResult<FilteredBrowse> {
    let entry = meta(&state, &args.connection_id, &args.database).await?;
    let browse = {
        let s = entry.session.lock().await;
        s.browse_query(&args.object, args.limit)
    };
    let driver = crate::commands::schema::driver_of(&state, &args.connection_id)?;
    Ok(match driver.filtered_browse(&browse, &args.filters) {
        Ok(query) => FilteredBrowse { query, server_side: true, reason: None },
        Err(dbine_driver::Error::Unsupported(why)) => FilteredBrowse { query: browse, server_side: false, reason: Some(why) },
        Err(e) => return Err(e.into()),
    })
}
