//! Database structure, DDL, and database-level operations
//! (docs/api-commands.md).

use crate::commands::typed_scripts;
use crate::error::{CommandError, CommandResult};
use crate::state::{meta_key, AppState};
use dbine_driver::{DdlParts, Driver, Language, ObjectRef, ResultColumn, RowChange, TableSchema};
use serde::Deserialize;
use serde_json::Value;
use std::sync::Arc;
use tauri::{AppHandle, Emitter, State};

/// The driver of a saved connection.
pub fn driver_of(state: &AppState, connection_id: &str) -> CommandResult<&'static Arc<dyn Driver>> {
    let id = state
        .store
        .get_connection(connection_id)?
        .ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?
        .config
        .driver;
    dbine_drivers::find(&id).ok_or_else(|| {
        CommandError::BadRequest(format!("esta versión no incluye el driver '{id}'"))
    })
}

#[derive(Deserialize)]
pub struct DatabaseArgs {
    pub connection_id: String,
    pub database: String,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn database_schema(
    state: State<'_, AppState>,
    args: DatabaseArgs,
) -> CommandResult<Vec<TableSchema>> {
    state
        .meta_read_on(
            crate::state::Lane::Long,
            &args.connection_id,
            &args.database,
            crate::commands::explorer::SCHEMA_LIMIT,
            |s| Box::pin(s.database_schema()),
        )
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
    /// The columns' types as the result gives them (`type_name`, `vector`),
    /// matched to `columns` by name: the driver writes each value as its
    /// column takes it. Empty (older callers): untyped.
    #[serde(default)]
    pub types: Vec<ResultColumn>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn insert_script(
    state: State<'_, AppState>,
    args: InsertScriptArgs,
) -> CommandResult<String> {
    insert_script_with(&state, &args)
}

/// [`insert_script`]'s work.
pub(crate) fn insert_script_with(
    state: &AppState,
    args: &InsertScriptArgs,
) -> CommandResult<String> {
    insert_script_for(driver_of(state, &args.connection_id)?.as_ref(), args)
}

/// [`insert_script_with`] on its connection's `driver`.
fn insert_script_for(driver: &dyn Driver, args: &InsertScriptArgs) -> CommandResult<String> {
    Ok(typed_scripts::insert(
        driver,
        &args.target,
        &args.columns,
        &args.types,
        &args.rows,
    )?)
}

#[derive(Deserialize)]
pub struct UpdateScriptArgs {
    pub connection_id: String,
    pub target: ObjectRef,
    pub changes: Vec<RowChange>,
    /// Rows marked for deletion in the grid: each one's key columns with
    /// their values. `None` (older callers): only the UPDATEs.
    #[serde(default)]
    pub deletes: Option<Vec<Vec<(String, Value)>>>,
    /// New rows ("Agregar fila", "Agregar documento"): the values set, by
    /// column / field. `None` (older callers): no INSERTs.
    #[serde(default)]
    pub inserts: Option<Vec<Vec<(String, Value)>>>,
    /// The table's identity columns: new rows that set one get the
    /// engine's wrap (SQL Server's IDENTITY_INSERT).
    #[serde(default)]
    pub identity: Vec<String>,
    /// The result's columns: an empty `inserts` asks with them whether the
    /// engine writes new rows there.
    #[serde(default)]
    pub columns: Vec<String>,
    /// The result's columns with their types (`type_name`, `vector`): the
    /// updates, deletes and inserts are written for them (by name). Empty
    /// (older callers): untyped.
    #[serde(default)]
    pub types: Vec<ResultColumn>,
}

/// Cells edited (and rows marked for deletion) in the results grid as code
/// that applies them (it's only generated: the user runs it). With
/// `deletes`, the DELETEs go first and then the UPDATEs, like data
/// compare's sync script: the rows are disjoint (a deleted row's edits are
/// dropped by the UI) and deleting first means no UPDATE can make a row
/// match a DELETE's WHERE (by all columns when there's no key) or collide
/// with a unique value that's about to go. An empty `deletes` still asks
/// the driver: it's how the UI learns the engine can't delete rows. With
/// `inserts`, the new rows' INSERTs go last (an empty list only asks
/// whether the engine writes them, the same way).
#[tauri::command(rename_all = "camelCase")]
pub async fn update_script(
    state: State<'_, AppState>,
    args: UpdateScriptArgs,
) -> CommandResult<String> {
    update_script_with(&state, &args)
}

/// [`update_script`]'s work.
pub(crate) fn update_script_with(
    state: &AppState,
    args: &UpdateScriptArgs,
) -> CommandResult<String> {
    update_script_for(driver_of(state, &args.connection_id)?.as_ref(), args)
}

/// [`update_script_with`] on its connection's `driver`.
fn update_script_for(driver: &dyn Driver, args: &UpdateScriptArgs) -> CommandResult<String> {
    let types = &args.types;
    if args.deletes.is_none() && args.inserts.is_none() {
        return Ok(typed_scripts::update(
            driver,
            &args.target,
            &args.changes,
            types,
        )?);
    }
    let mut parts = Vec::new();
    if let Some(deletes) = &args.deletes {
        parts.push(typed_scripts::delete(driver, &args.target, deletes, types)?);
    }
    if !args.changes.is_empty() {
        parts.push(typed_scripts::update(
            driver,
            &args.target,
            &args.changes,
            types,
        )?);
    }
    if let Some(inserts) = &args.inserts {
        parts.extend(super::data_compare::new_rows_parts(
            driver,
            &args.target,
            inserts,
            &args.identity,
            &args.columns,
            types,
        )?);
    }
    let sep = driver.script_separator();
    Ok(parts
        .into_iter()
        .filter(|p| !p.trim().is_empty())
        .collect::<Vec<_>>()
        .join(&format!("\n{sep}\n")))
}

#[derive(Deserialize)]
pub struct DatabaseNameArgs {
    pub connection_id: String,
    pub name: String,
}

#[derive(Deserialize)]
pub struct CreateDatabaseArgs {
    pub connection_id: String,
    pub name: String,
    /// The advanced options (`Driver::create_database_fields`) by key.
    #[serde(default)]
    pub options: std::collections::BTreeMap<String, String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn create_database(
    state: State<'_, AppState>,
    args: CreateDatabaseArgs,
) -> CommandResult<()> {
    if args.name.trim().is_empty() {
        return Err(CommandError::BadRequest(
            "la base necesita un nombre".into(),
        ));
    }
    let entry = state
        .session(&meta_key(&args.connection_id, ""), &args.connection_id, "")
        .await?;
    let mut s = entry.session.lock().await;
    Ok(s.create_database_with(args.name.trim(), &args.options)
        .await?)
}

/// What "Ver script" shows for "Nueva base de datos".
#[tauri::command(rename_all = "camelCase")]
pub async fn create_database_script(
    state: State<'_, AppState>,
    args: CreateDatabaseArgs,
) -> CommandResult<String> {
    if args.name.trim().is_empty() {
        return Err(CommandError::BadRequest(
            "la base necesita un nombre".into(),
        ));
    }
    Ok(driver_of(&state, &args.connection_id)?
        .create_database_script(args.name.trim(), &args.options)?)
}

#[derive(Deserialize)]
pub struct ConnectionArgs {
    pub connection_id: String,
}

/// "Propiedades" of a database: what can be changed, its current values,
/// facts and warnings. Read on the server-level session, as creating and
/// dropping databases are (some changes can't run from inside the
/// database: PostgreSQL's SET TABLESPACE).
#[tauri::command(rename_all = "camelCase")]
pub async fn database_properties(
    state: State<'_, AppState>,
    args: DatabaseArgs,
) -> CommandResult<dbine_driver::DatabaseProperties> {
    let entry = state
        .session(&meta_key(&args.connection_id, ""), &args.connection_id, "")
        .await?;
    let mut s = entry.session.lock().await;
    Ok(s.database_properties(&args.database).await?)
}

#[derive(Deserialize)]
pub struct AlterDatabaseArgs {
    pub connection_id: String,
    pub database: String,
    /// Field key → new value: only what the user changed.
    pub changes: std::collections::BTreeMap<String, String>,
}

/// The script "Aplicar" shows before it runs.
#[tauri::command(rename_all = "camelCase")]
pub async fn alter_database_script(
    state: State<'_, AppState>,
    args: AlterDatabaseArgs,
) -> CommandResult<String> {
    Ok(driver_of(&state, &args.connection_id)?
        .alter_database_script(&args.database, &args.changes)?)
}

/// Apply the confirmed changes. Refused on read-only connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn alter_database(
    state: State<'_, AppState>,
    args: AlterDatabaseArgs,
) -> CommandResult<()> {
    if args.changes.is_empty() {
        return Ok(());
    }
    let entry = state
        .session(&meta_key(&args.connection_id, ""), &args.connection_id, "")
        .await?;
    let mut s = entry.session.lock().await;
    Ok(s.alter_database(&args.database, &args.changes).await?)
}

/// The server's suggestions for "Nueva base de datos"'s options
/// (collations, default paths, users…). Empty when it has none.
#[tauri::command(rename_all = "camelCase")]
pub async fn create_database_choices(
    state: State<'_, AppState>,
    args: ConnectionArgs,
) -> CommandResult<Vec<dbine_driver::FieldChoices>> {
    let entry = state
        .session(&meta_key(&args.connection_id, ""), &args.connection_id, "")
        .await?;
    let mut s = entry.session.lock().await;
    Ok(s.create_database_choices().await?)
}

#[tauri::command(rename_all = "camelCase")]
pub async fn drop_database(
    state: State<'_, AppState>,
    args: DatabaseNameArgs,
) -> CommandResult<()> {
    // Sessions on that database would keep it busy (and some engines refuse
    // to drop a database with connections): close ours first.
    state
        .sessions
        .retain(|_, e| !(e.connection_id == args.connection_id && e.database == args.name));
    let entry = state
        .session(&meta_key(&args.connection_id, ""), &args.connection_id, "")
        .await?;
    let mut s = entry.session.lock().await;
    Ok(s.drop_database(&args.name).await?)
}

#[derive(Deserialize)]
pub struct DropObjectsArgs {
    pub connection_id: String,
    pub database: String,
    pub objects: Vec<ObjectRef>,
    /// Progress events (`drop-objects-progress`) carry this id; none are
    /// sent without it (older callers).
    #[serde(default)]
    pub id: Option<String>,
}

#[derive(Clone, serde::Serialize)]
struct DropProgress {
    id: String,
    /// Objects settled: dropped, or given up on (failed in a pass where
    /// nothing else could go, or not droppable from DBine).
    done: usize,
    total: usize,
}

#[derive(serde::Serialize)]
pub struct DropObjectsResult {
    pub dropped: Vec<ObjectRef>,
    /// Objects that couldn't be dropped, with the reason.
    pub errors: Vec<(ObjectRef, String)>,
}

/// The statement that drops `o`: a table or collection by the driver's
/// DDL; a view, routine, trigger… by its engine's DROP
/// (`scripts::drop_other`); on an engine whose language isn't SQL or CQL,
/// another kind it lists (Redis' search indexes, `FT.DROPINDEX`) by its DDL
/// for that kind (`table_ddl` of a `TableSchema` of that kind, with
/// `drop`). `None`: DBine can't drop it.
pub(crate) fn drop_statement(driver: &dyn Driver, o: &ObjectRef) -> Option<String> {
    let by_ddl = |kind: &str| {
        let t = TableSchema {
            kind: kind.to_string(),
            schema: o.schema.clone(),
            name: o.name.clone(),
            ..Default::default()
        };
        driver
            .table_ddl(
                &t,
                DdlParts {
                    drop: true,
                    ..Default::default()
                },
            )
            .ok()
            .filter(|s| !s.trim().is_empty())
    };
    if o.kind == dbine_driver::kinds::TABLE || o.kind == dbine_driver::kinds::COLLECTION {
        return by_ddl(&o.kind);
    }
    if let Some(s) = crate::commands::scripts::drop_other(driver, o, false) {
        return Some(s).filter(|s| !s.trim().is_empty());
    }
    let info = driver.info();
    // SQL and CQL engines' DDL is for tables only: another kind there would
    // come out as a table's DROP.
    if matches!(info.language, Language::Sql | Language::Cql)
        || !info.object_kinds.iter().any(|k| k.id == o.kind)
    {
        return None;
    }
    let stmt = by_ddl(&o.kind)?;
    // A DDL that doesn't know the kind writes its first kind's drop (an
    // Elasticsearch alias as `DELETE /name`, which names an index): that
    // isn't this object's.
    let first = info.object_kinds.first().map(|k| k.id);
    if first.is_some_and(|f| f != o.kind && by_ddl(f).as_deref() == Some(stmt.as_str())) {
        return None;
    }
    Some(stmt)
}

/// Drop tables / collections (and, in SQL engines, views, routines,
/// triggers…) with the driver's own DDL. In passes: an object others depend
/// on goes once they're gone. Refused on read-only connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn drop_objects(
    app: AppHandle,
    state: State<'_, AppState>,
    args: DropObjectsArgs,
) -> CommandResult<DropObjectsResult> {
    let conn = state
        .store
        .get_connection(&args.connection_id)?
        .ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if conn.config.read_only {
        return Err(CommandError::BadRequest(format!(
            "«{}» es de solo lectura: no se pueden eliminar objetos",
            conn.name
        )));
    }
    let driver = driver_of(&state, &args.connection_id)?;
    let mut work: Vec<(ObjectRef, String)> = Vec::new();
    let mut errors: Vec<(ObjectRef, String)> = Vec::new();
    for o in &args.objects {
        match drop_statement(driver.as_ref(), o) {
            Some(s) => work.push((o.clone(), s)),
            None => errors.push((
                o.clone(),
                "este motor no permite eliminar este tipo de objeto desde DBine".into(),
            )),
        }
    }
    let total = args.objects.len();
    // Throttled; `force` for the first and the last event.
    let mut last_emit: Option<std::time::Instant> = None;
    let mut progress = |done: usize, force: bool| {
        let Some(id) = &args.id else { return };
        let now = std::time::Instant::now();
        if !force
            && last_emit
                .is_some_and(|t| now.duration_since(t) < std::time::Duration::from_millis(150))
        {
            return;
        }
        last_emit = Some(now);
        let _ = app.emit(
            "drop-objects-progress",
            DropProgress {
                id: id.clone(),
                done,
                total,
            },
        );
    };
    progress(errors.len(), true);
    let key = format!("drop:{}", uuid::Uuid::new_v4());
    let entry = state
        .dedicated_session(&key, &args.connection_id, &args.database, false)
        .await?;
    let mut dropped = Vec::new();
    async {
        let mut pending = work;
        while !pending.is_empty() {
            let mut failed = Vec::new();
            for (o, sql) in pending.iter() {
                let mut s = entry.session.lock().await;
                let mut out = dbine_driver::QueryOutcome::default();
                let r = s
                    .execute(sql, 0, &mut out)
                    .await
                    .map_err(|e| e.to_string())
                    .and_then(|_| match out.error.take() {
                        Some(e) => Err(e),
                        None => Ok(()),
                    });
                match r {
                    Ok(()) => {
                        dropped.push(o.clone());
                        progress(dropped.len() + errors.len(), false);
                    }
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
    progress(dropped.len() + errors.len(), true);
    state.sessions.remove(&key);
    Ok(DropObjectsResult { dropped, errors })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn drop_of(driver: &str, kind: &str) -> Option<String> {
        let d = dbine_drivers::find(driver).unwrap();
        let o = ObjectRef {
            kind: kind.into(),
            schema: None,
            name: "x".into(),
        };
        drop_statement(d.as_ref(), &o).map(|s| s.trim().to_string())
    }

    /// Other kinds of non-SQL engines drop by the driver's DDL for that
    /// kind (Redis' search index); a kind that DDL doesn't know (it writes
    /// its first kind's drop: an alias as an index) and SQL engines' other
    /// kinds are refused, as before.
    #[test]
    fn non_sql_kinds_drop_by_their_ddl() {
        for redis in ["redis", "valkey"] {
            assert_eq!(drop_of(redis, "index").as_deref(), Some("FT.DROPINDEX x"));
            assert_eq!(drop_of(redis, "key").as_deref(), Some("DEL x"));
        }
        assert_eq!(
            drop_of("elasticsearch", "index").as_deref(),
            Some("DELETE /x")
        );
        assert_eq!(drop_of("elasticsearch", "alias"), None);
        assert_eq!(drop_of("opensearch", "stream"), None);
        assert_eq!(drop_of("kdb", "view"), None);
        assert_eq!(drop_of("neo4j", "label"), None);
        // A kind the engine doesn't list.
        assert_eq!(drop_of("redis", "stream"), None);
        // As before: tables and collections by the DDL, the rest of SQL by
        // its DROP, and nothing else on SQL engines.
        assert!(drop_of("mongodb", "collection")
            .unwrap()
            .contains(".drop()"));
        assert!(drop_of("postgres", "view")
            .unwrap()
            .starts_with("DROP VIEW"));
        assert!(drop_of("postgres", "table")
            .unwrap()
            .starts_with("DROP TABLE"));
        assert_eq!(drop_of("postgres", "index"), None);
        assert_eq!(drop_of("postgres", "extension"), None);
    }

    /// The grid's copy and save: with the result's types, every part of
    /// the script (deletes, updates, new rows, and a copied INSERT) goes to
    /// the driver's typed method with them; without types, the untyped
    /// methods, as older callers get.
    #[test]
    fn the_grids_scripts_are_typed_by_the_results_columns() {
        use crate::commands::typed_scripts::tests::{table, Recorder};
        use dbine_driver::{VectorElement, VectorMeta};
        use serde_json::json;
        let rec = Recorder::default();
        let types = vec![
            ResultColumn::new("id", "int4"),
            ResultColumn::new("emb", "halfvec(2)")
                .with_vector(VectorMeta::dense(Some(2), VectorElement::F16)),
        ];
        let change = RowChange {
            key: vec![("id".into(), json!(1))],
            set: vec![("emb".into(), json!([1.0, 2.0]))],
            row: vec![],
        };
        let save = |types: Vec<ResultColumn>, deletes: bool, inserts: bool| UpdateScriptArgs {
            connection_id: String::new(),
            target: table("t"),
            changes: vec![change.clone()],
            deletes: deletes.then(|| vec![vec![("id".to_string(), json!(2))]]),
            inserts: inserts.then(|| vec![vec![("emb".to_string(), json!([3.0, 4.0]))]]),
            identity: vec![],
            columns: vec![],
            types,
        };
        let kinds = |calls: &[(String, Vec<ResultColumn>)]| -> Vec<String> {
            calls.iter().map(|c| c.0.clone()).collect()
        };
        let sig = |cols: &[ResultColumn]| -> Vec<(String, String, Option<VectorMeta>)> {
            cols.iter()
                .map(|c| (c.name.clone(), c.type_name.clone(), c.vector.clone()))
                .collect()
        };
        update_script_for(&rec, &save(types.clone(), true, true)).unwrap();
        let calls = rec.take();
        assert_eq!(
            kinds(&calls),
            ["delete_typed", "update_typed", "insert_typed"]
        );
        for (what, cols) in &calls[..2] {
            assert_eq!(sig(cols), sig(&types), "{what}");
        }
        assert_eq!(sig(&calls[2].1), sig(&types[1..]));
        // Only edits (an older UI's save): the typed update alone.
        update_script_for(&rec, &save(types.clone(), false, false)).unwrap();
        assert_eq!(kinds(&rec.take()), ["update_typed"]);
        // No types: untyped, every part.
        update_script_for(&rec, &save(vec![], true, true)).unwrap();
        assert_eq!(kinds(&rec.take()), ["delete", "update", "insert"]);

        // "Copiar como INSERT".
        let copy = |types: Vec<ResultColumn>| InsertScriptArgs {
            connection_id: String::new(),
            target: table("t"),
            columns: vec!["id".into(), "emb".into()],
            rows: vec![vec![json!(1), json!([1.0, 2.0])]],
            types,
        };
        insert_script_for(&rec, &copy(types.clone())).unwrap();
        insert_script_for(&rec, &copy(vec![])).unwrap();
        let calls = rec.take();
        assert_eq!(kinds(&calls), ["insert_typed", "insert"]);
        assert_eq!(sig(&calls[0].1), sig(&types));
    }
}
