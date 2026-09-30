//! Database structure, DDL, and database-level operations
//! (docs/api-comandos.md).

use crate::error::{CommandError, CommandResult};
use crate::state::{meta_key, AppState};
use dbine_driver::{DdlParts, Driver, ObjectRef, RowChange, TableSchema};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tauri::State;

/// The driver of a saved connection.
pub fn driver_of(state: &AppState, connection_id: &str) -> CommandResult<&'static Arc<dyn Driver>> {
    let id = state
        .store
        .get_connection(connection_id)?
        .ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?
        .config
        .driver;
    dbine_drivers::find(&id).ok_or_else(|| CommandError::BadRequest(format!("esta versión no incluye el driver '{id}'")))
}

#[derive(Deserialize)]
pub struct DatabaseArgs {
    pub connection_id: String,
    pub database: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn database_schema(state: State<'_, AppState>, args: DatabaseArgs) -> CommandResult<Vec<TableSchema>> {
    state
        .meta_read(&args.connection_id, &args.database, crate::commands::explorer::SCHEMA_LIMIT, |s| Box::pin(s.database_schema()))
        .await
}

#[derive(Deserialize)]
pub struct TableDdlArgs {
    pub connection_id: String,
    pub table: TableSchema,
    pub parts: DdlParts,
    /// The database the table goes to (the designer's).
    #[serde(default)]
    pub database: Option<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn table_ddl(state: State<'_, AppState>, args: TableDdlArgs) -> CommandResult<String> {
    let driver = driver_of(&state, &args.connection_id)?;
    let mut table = args.table;
    // IoTDB addresses a device by its full path: the database (root.x) is
    // the prefix the designer doesn't ask for.
    if driver.info().id == "iotdb" && table.schema.is_none() {
        table.schema = args.database.filter(|d| !d.is_empty());
    }
    Ok(driver.table_ddl(&table, args.parts)?)
}

#[derive(Deserialize)]
pub struct InsertScriptArgs {
    pub connection_id: String,
    pub target: ObjectRef,
    pub columns: Vec<String>,
    pub rows: Vec<Vec<Value>>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn insert_script(state: State<'_, AppState>, args: InsertScriptArgs) -> CommandResult<String> {
    Ok(driver_of(&state, &args.connection_id)?.insert_script(&args.target, &args.columns, &args.rows)?)
}

#[derive(Deserialize)]
pub struct UpdateScriptArgs {
    pub connection_id: String,
    pub target: ObjectRef,
    pub changes: Vec<RowChange>,
}

/// Cells edited in the results grid as code that applies them (it's only
/// generated: the user runs it).
#[tauri::command(rename_all = "camelCase")]
pub async fn update_script(state: State<'_, AppState>, args: UpdateScriptArgs) -> CommandResult<String> {
    Ok(driver_of(&state, &args.connection_id)?.update_script(&args.target, &args.changes)?)
}

#[derive(Deserialize)]
pub struct DatabaseNameArgs {
    pub connection_id: String,
    pub name: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn create_database(state: State<'_, AppState>, args: DatabaseNameArgs) -> CommandResult<()> {
    if args.name.trim().is_empty() {
        return Err(CommandError::BadRequest("la base necesita un nombre".into()));
    }
    let entry = state.session(&meta_key(&args.connection_id, ""), &args.connection_id, "").await?;
    let mut s = entry.session.lock().await;
    Ok(s.create_database(args.name.trim()).await?)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn drop_database(state: State<'_, AppState>, args: DatabaseNameArgs) -> CommandResult<()> {
    // Sessions on that database would keep it busy (and some engines refuse
    // to drop a database with connections): close ours first.
    state.sessions.retain(|_, e| !(e.connection_id == args.connection_id && e.database == args.name));
    let entry = state.session(&meta_key(&args.connection_id, ""), &args.connection_id, "").await?;
    let mut s = entry.session.lock().await;
    Ok(s.drop_database(&args.name).await?)
}

#[derive(Deserialize)]
pub struct DropObjectsArgs {
    pub connection_id: String,
    pub database: String,
    pub objects: Vec<ObjectRef>,
}

#[derive(serde::Serialize)]
pub struct DropObjectsResult {
    pub dropped: Vec<ObjectRef>,
    /// Objects that couldn't be dropped, with the reason.
    pub errors: Vec<(ObjectRef, String)>,
}

/// Drop tables / collections (and, in SQL engines, views, routines,
/// triggers…) with the driver's own DDL. In passes: an object others depend
/// on goes once they're gone. Refused on read-only connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn drop_objects(state: State<'_, AppState>, args: DropObjectsArgs) -> CommandResult<DropObjectsResult> {
    let conn = state.store.get_connection(&args.connection_id)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if conn.config.read_only {
        return Err(CommandError::BadRequest(format!("«{}» es de solo lectura: no se pueden eliminar objetos", conn.name)));
    }
    let driver = driver_of(&state, &args.connection_id)?;
    let mut work: Vec<(ObjectRef, String)> = Vec::new();
    let mut errors: Vec<(ObjectRef, String)> = Vec::new();
    for o in &args.objects {
        let stmt = if o.kind == dbine_driver::kinds::TABLE || o.kind == dbine_driver::kinds::COLLECTION {
            let t = TableSchema { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone(), ..Default::default() };
            driver.table_ddl(&t, DdlParts { drop: true, ..Default::default() }).ok()
        } else {
            crate::commands::scripts::drop_other(driver.as_ref(), o, false)
        };
        match stmt.filter(|s| !s.trim().is_empty()) {
            Some(s) => work.push((o.clone(), s)),
            None => errors.push((o.clone(), "este motor no permite eliminar este tipo de objeto desde DBine".into())),
        }
    }
    let key = format!("drop:{}", uuid::Uuid::new_v4());
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, false).await?;
    let mut dropped = Vec::new();
    let result = async {
        let mut pending = work;
        while !pending.is_empty() {
            let mut failed = Vec::new();
            for (o, sql) in pending.iter() {
                let mut s = entry.session.lock().await;
                let mut out = dbine_driver::QueryOutcome::default();
                let r = s.execute(sql, 0, &mut out).await.map_err(|e| e.to_string()).and_then(|_| match out.error.take() {
                    Some(e) => Err(e),
                    None => Ok(()),
                });
                match r {
                    Ok(()) => dropped.push(o.clone()),
                    Err(e) => failed.push((o.clone(), sql.clone(), e)),
                }
            }
            if failed.len() == pending.len() {
                errors.extend(failed.into_iter().map(|(o, _, e)| (o, e)));
                break;
            }
            pending = failed.into_iter().map(|(o, sql, _)| (o, sql)).collect();
        }
    }
    .await;
    let _ = result;
    state.sessions.remove(&key);
    Ok(DropObjectsResult { dropped, errors })
}
