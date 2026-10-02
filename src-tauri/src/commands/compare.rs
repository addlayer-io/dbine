//! "Comparar esquemas": load two databases' schemas, compare them, and
//! apply the changes the user carried from one side to the other.
//!
//! Comparing and planning are pure (the UI keeps both schemas and edits its
//! copies); only loading and running touch the servers.

use crate::commands::schema::driver_of;
use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::{kinds, Driver, ObjectRef, QueryOutcome, SyncScript, TableChange, TableSchema};
use dbine_schema::compare::{CodeObject, CompareOptions, CompareResult, DbModel};
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};

/// Kinds compared by their source.
const CODE_KINDS: &[&str] = &[
    kinds::VIEW,
    kinds::MATERIALIZED_VIEW,
    kinds::PROCEDURE,
    kinds::FUNCTION,
    kinds::TRIGGER,
    kinds::SEQUENCE,
    kinds::SYNONYM,
    kinds::TYPE,
    DOMAIN,
    FULLTEXT_CATALOG,
    FULLTEXT_STOPLIST,
    // SQLite's FTS / R*Tree tables; ClickHouse's dictionaries.
    "virtual_table",
    "dictionary",
];
/// Domains, where the engine keeps them apart from types (H2).
const DOMAIN: &str = "domain";
/// SQL Server's full-text catalogs and stoplists.
const FULLTEXT_CATALOG: &str = "fulltext_catalog";
const FULLTEXT_STOPLIST: &str = "fulltext_stoplist";
/// What tables use (column types, defaults, full-text indexes): created
/// before the tables, dropped after them.
const TABLE_PREREQS: &[&str] = &[kinds::TYPE, DOMAIN, kinds::SEQUENCE, FULLTEXT_CATALOG, FULLTEXT_STOPLIST];
/// Their definition makes them as they should be whether they exist or not
/// (the server won't drop one an index still uses).
const IN_PLACE: &[&str] = &[FULLTEXT_CATALOG, FULLTEXT_STOPLIST];

#[derive(Deserialize)]
pub struct LoadArgs {
    pub connection_id: String,
    pub database: String,
    /// Only these schemas (all when empty).
    #[serde(default)]
    pub schemas: Vec<String>,
}

#[derive(Serialize)]
pub struct Loaded {
    #[serde(flatten)]
    pub model: DbModel,
    /// Objects whose source couldn't be read.
    pub warnings: Vec<String>,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn schema_compare_load(state: State<'_, AppState>, args: LoadArgs) -> CommandResult<Loaded> {
    let driver = driver_of(&state, &args.connection_id)?;
    let schemas = args.schemas.clone();
    let wanted = move |s: &Option<String>| schemas.is_empty() || s.as_deref().is_some_and(|s| schemas.iter().any(|w| w == s));
    let (tables, objects, warnings) = state
        .meta_read(&args.connection_id, &args.database, crate::commands::explorer::SCHEMA_LIMIT, move |s| {
            Box::pin(async move {
                let tables: Vec<TableSchema> = s.database_schema().await?.into_iter().filter(|t| wanted(&t.schema)).collect();
                let mut objects = Vec::new();
                let mut warnings = Vec::new();
                for o in s.list_objects().await? {
                    if !CODE_KINDS.contains(&o.kind.as_str()) || !wanted(&o.schema) {
                        continue;
                    }
                    let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
                    match s.definition(&r).await {
                        Ok(Some(definition)) => objects.push(CodeObject { kind: o.kind, schema: o.schema, name: o.name, definition }),
                        Ok(None) => {}
                        Err(e) => warnings.push(format!("{}: {e}", o.name)),
                    }
                }
                Ok((tables, objects, warnings))
            })
        })
        .await?;
    Ok(Loaded { model: DbModel { driver: driver.info().id.to_string(), tables, objects }, warnings })
}

#[derive(Deserialize)]
pub struct CompareArgs {
    pub left: DbModel,
    pub right: DbModel,
    #[serde(default)]
    pub options: CompareOptions,
}

#[tauri::command(rename_all = "camelCase")]
pub async fn schema_compare(args: CompareArgs) -> CommandResult<CompareResult> {
    Ok(dbine_schema::compare::compare(&args.left, &args.right, &args.options))
}

#[derive(Deserialize)]
pub struct ConvertArgs {
    pub from_driver: String,
    pub to_driver: String,
    pub tables: Vec<TableSchema>,
    /// The schema they go to on the other side.
    pub target_schema: Option<String>,
}

#[derive(Serialize)]
pub struct Converted {
    pub tables: Vec<TableSchema>,
    /// What didn't carry over exactly.
    pub warnings: Vec<String>,
}

/// Tables in another engine's terms, to carry them across engines.
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_compare_convert(args: ConvertArgs) -> CommandResult<Converted> {
    if args.from_driver == args.to_driver {
        let tables = args.tables.into_iter().map(|t| TableSchema { schema: args.target_schema.clone().or(t.schema.clone()), ..t }).collect();
        return Ok(Converted { tables, warnings: Vec::new() });
    }
    let opts = dbine_schema::Options { target_schema: args.target_schema, ..Default::default() };
    let c = dbine_schema::convert(&args.tables, &args.from_driver, &args.to_driver, &opts).map_err(|e| CommandError::BadRequest(format!("{e:?}")))?;
    let warnings = c.issues.iter().filter(|i| i.severity != dbine_schema::Severity::Info).map(|i| i.message.clone()).collect();
    Ok(Converted { tables: c.tables, warnings })
}

/// A view, procedure… to create, drop or replace.
#[derive(Deserialize, Clone)]
#[serde(tag = "op", rename_all = "snake_case")]
pub enum ObjectChange {
    Create { object: CodeObject },
    Drop { object: CodeObject },
    Replace { object: CodeObject },
}

#[derive(Deserialize)]
pub struct ScriptArgs {
    pub connection_id: String,
    #[serde(default)]
    pub tables: Vec<TableChange>,
    #[serde(default)]
    pub objects: Vec<ObjectChange>,
    /// The target's views as they'll be: the ones over a table whose columns
    /// change are dropped first and made again after (most engines refuse to
    /// change a column a view uses).
    #[serde(default)]
    pub views: Vec<CodeObject>,
}

/// The target's script for the changes: code objects that go first, the
/// tables, then the code objects that come (views over the new columns).
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_sync_script(state: State<'_, AppState>, args: ScriptArgs) -> CommandResult<SyncScript> {
    let driver = driver_of(&state, &args.connection_id)?;
    let mut objects = args.objects;
    let extra = dependent_views(&args.tables, &objects, &args.views);
    let note = (!extra.is_empty()).then(|| {
        format!(
            "Se borran y se vuelven a crear las vistas que usan las tablas modificadas: {}.",
            extra.iter().map(|o| match o { ObjectChange::Replace { object } | ObjectChange::Drop { object } | ObjectChange::Create { object } => object.name.clone() }).collect::<Vec<_>>().join(", ")
        )
    });
    objects.extend(extra);
    let mut script = plan(driver.as_ref(), &args.tables, &objects)?;
    script.warnings.extend(note);
    Ok(script)
}

/// Whether `text` names `name` as a whole word (any case).
/// Definitions ordered so that one naming another comes after it (a domain
/// over an enum, a composite using a domain…); a cycle keeps its order.
fn by_dependency(mut items: Vec<(String, String)>) -> Vec<String> {
    let mut out = Vec::with_capacity(items.len());
    while !items.is_empty() {
        let ready = (0..items.len())
            .find(|&i| !items.iter().enumerate().any(|(j, (name, _))| j != i && mentions(&items[i].1, name)))
            .unwrap_or(0);
        out.push(items.remove(ready).1);
    }
    out
}

fn mentions(text: &str, name: &str) -> bool {
    let (t, n) = (text.to_lowercase(), name.to_lowercase());
    let word = |c: Option<char>| c.is_some_and(|c| c.is_alphanumeric() || c == '_' || c == '$');
    let mut from = 0;
    while let Some(i) = t[from..].find(&n) {
        let at = from + i;
        if !word(t[..at].chars().last()) && !word(t[at + n.len()..].chars().next()) {
            return true;
        }
        from = at + n.len();
    }
    false
}

/// Views over tables whose columns are dropped or change type, not already
/// being changed: made again around the ALTERs. Views over dropped tables
/// only go.
fn dependent_views(tables: &[TableChange], objects: &[ObjectChange], views: &[CodeObject]) -> Vec<ObjectChange> {
    let touched: Vec<(&str, bool)> = tables
        .iter()
        .filter_map(|c| match c {
            TableChange::Drop { table } => Some((table.name.as_str(), true)),
            TableChange::Alter { old, new } => {
                let reshaped = old.columns.iter().any(|o| {
                    new.columns.iter().find(|n| n.name.eq_ignore_ascii_case(&o.name)).is_none_or(|n| n.data_type.to_lowercase().split_whitespace().collect::<String>() != o.data_type.to_lowercase().split_whitespace().collect::<String>())
                });
                reshaped.then_some((new.name.as_str(), false))
            }
            TableChange::Create { .. } => None,
        })
        .collect();
    let handled = |v: &CodeObject| {
        objects.iter().any(|o| match o {
            ObjectChange::Create { object } | ObjectChange::Drop { object } | ObjectChange::Replace { object } => {
                object.kind == v.kind && object.name.eq_ignore_ascii_case(&v.name) && object.schema == v.schema
            }
        })
    };
    views
        .iter()
        .filter(|v| (v.kind == kinds::VIEW || v.kind == kinds::MATERIALIZED_VIEW) && !handled(v))
        .filter_map(|v| {
            let hit: Vec<bool> = touched.iter().filter(|(t, _)| mentions(&v.definition, t)).map(|(_, dropped)| *dropped).collect();
            if hit.is_empty() {
                None
            } else if hit.iter().any(|d| *d) {
                Some(ObjectChange::Drop { object: v.clone() })
            } else {
                Some(ObjectChange::Replace { object: v.clone() })
            }
        })
        .collect()
}

fn plan(driver: &dyn Driver, tables: &[TableChange], objects: &[ObjectChange]) -> CommandResult<SyncScript> {
    let mut script = if tables.is_empty() {
        SyncScript::default()
    } else if driver.supports_schema_sync() {
        driver.sync_script(tables)?
    } else {
        return Err(CommandError::BadRequest(format!("{} no aplica cambios de esquema desde DBine", driver.info().name)));
    };
    let mut before = Vec::new();
    let mut early = Vec::new();
    let mut after = Vec::new();
    let mut late = Vec::new();
    for ch in objects {
        let (o, drop, create) = match ch {
            ObjectChange::Create { object } => (object, false, true),
            ObjectChange::Drop { object } => (object, true, false),
            ObjectChange::Replace { object } => (object, true, true),
        };
        let r = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
        let prereq = TABLE_PREREQS.contains(&o.kind.as_str());
        if drop && !(create && IN_PLACE.contains(&o.kind.as_str())) {
            match crate::commands::scripts::drop_other(driver, &r, true) {
                // Only dropped: after the tables that may still use it.
                Some(s) if prereq && !create => late.push(s),
                Some(s) => before.push(s),
                None => script.warnings.push(format!("{}: este motor no permite borrarlo desde DBine.", o.name)),
            }
        }
        if create {
            let d = o.definition.trim().to_string();
            if prereq {
                early.push((o.name.clone(), d));
            } else {
                after.push(d);
            }
        }
    }
    if objects.iter().any(|o| matches!(o, ObjectChange::Drop { .. })) {
        script.warnings.push("Se borran vistas, procedimientos o funciones: revisá que nada más dependa de ellos.".into());
    }
    let tables_part = std::mem::take(&mut script.statements);
    let early = by_dependency(early);
    script.statements = before.into_iter().chain(early).chain(tables_part).chain(after).chain(late).collect();
    Ok(script)
}

#[derive(Deserialize)]
pub struct RunArgs {
    pub connection_id: String,
    pub database: String,
    pub statements: Vec<String>,
    /// For `cancel_query` (`sync:<run_id>`).
    pub run_id: String,
}

#[derive(Serialize)]
pub struct RunResult {
    /// How many statements ran.
    pub done: usize,
    /// The one that failed, and why (the rest didn't run).
    pub failed: Option<(usize, String)>,
}

/// `schema-sync-progress`: statements run so far in `run_id`. The first one
/// (`done: 0`) also says the run's session is registered, so a cancel sent
/// from then on reaches it.
#[derive(Serialize, Clone)]
struct SyncProgress<'a> {
    run_id: &'a str,
    done: usize,
    total: usize,
}

/// Minimum gap between two progress events of one run (the last one always goes).
const PROGRESS_EVERY: std::time::Duration = std::time::Duration::from_millis(150);

/// Run the script on the target, statement by statement; stops at the first
/// error. Refused on read-only connections.
#[tauri::command(rename_all = "camelCase")]
pub async fn schema_sync_run(app: AppHandle, state: State<'_, AppState>, args: RunArgs) -> CommandResult<RunResult> {
    let conn = state.store.get_connection(&args.connection_id)?.ok_or_else(|| CommandError::NotFound("conexión inexistente".into()))?;
    if conn.config.read_only {
        return Err(CommandError::BadRequest(format!("«{}» es de solo lectura: no se pueden aplicar cambios", conn.name)));
    }
    let key = format!("sync:{}", args.run_id);
    let entry = state.dedicated_session(&key, &args.connection_id, &args.database, false).await?;
    let total = args.statements.len();
    let emit = |done| {
        let _ = app.emit("schema-sync-progress", SyncProgress { run_id: &args.run_id, done, total });
    };
    emit(0);
    let mut last = std::time::Instant::now();
    let mut done = 0;
    let mut failed = None;
    for (i, sql) in args.statements.iter().enumerate() {
        if last.elapsed() >= PROGRESS_EVERY {
            emit(done);
            last = std::time::Instant::now();
        }
        if entry.cancelled.load(std::sync::atomic::Ordering::Relaxed) {
            failed = Some((i, "cancelado".to_string()));
            break;
        }
        let mut s = entry.session.lock().await;
        let mut out = QueryOutcome::default();
        let r = s.execute(sql, 0, &mut out).await.map_err(|e| e.to_string()).and_then(|_| match out.error.take() {
            Some(e) => Err(e),
            None => Ok(()),
        });
        match r {
            Ok(()) => done += 1,
            Err(e) => {
                failed = Some((i, e));
                break;
            }
        }
    }
    state.sessions.remove(&key);
    emit(done);
    Ok(RunResult { done, failed })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn types_come_after_the_ones_they_use() {
        let d = |n: &str, t: &str| (n.to_string(), t.to_string());
        let out = by_dependency(vec![
            d("t_comp", "CREATE TYPE t_comp AS (a d_pos, b estado)"),
            d("d_pos", "CREATE DOMAIN d_pos AS int CHECK (VALUE > 0)"),
            d("estado", "CREATE TYPE estado AS ENUM ('a')"),
        ]);
        assert_eq!(out.last().unwrap(), "CREATE TYPE t_comp AS (a d_pos, b estado)");
    }

    #[test]
    fn finds_dependent_views() {
        assert!(mentions("SELECT id FROM clientes;", "clientes"));
        assert!(mentions("from \"public\".\"Clientes\" c", "clientes"));
        assert!(!mentions("FROM clientes_viejos", "clientes"));
        let t = |cols: &[(&str, &str)]| TableSchema {
            name: "clientes".into(),
            columns: cols.iter().map(|(n, ty)| dbine_driver::ColumnDef { name: (*n).into(), data_type: (*ty).into(), ..Default::default() }).collect(),
            ..Default::default()
        };
        let view = |name: &str, def: &str| CodeObject { kind: "view".into(), schema: None, name: name.into(), definition: def.into() };
        let views = vec![view("v1", "SELECT nombre FROM clientes"), view("v2", "SELECT 1 FROM pedidos")];
        let alter = vec![TableChange::Alter { old: t(&[("nombre", "varchar(5)")]), new: t(&[("nombre", "varchar(10)")]) }];
        let got = dependent_views(&alter, &[], &views);
        assert!(matches!(got.as_slice(), [ObjectChange::Replace { object }] if object.name == "v1"));
        // Adding a column doesn't touch the views.
        let add = vec![TableChange::Alter { old: t(&[("nombre", "text")]), new: t(&[("nombre", "text"), ("x", "int")]) }];
        assert!(dependent_views(&add, &[], &views).is_empty());
        let drop = vec![TableChange::Drop { table: t(&[]) }];
        assert!(matches!(dependent_views(&drop, &[], &views).as_slice(), [ObjectChange::Drop { .. }]));
    }
}
