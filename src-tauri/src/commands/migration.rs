//! Migrating a database to another engine (docs/migracion.md): read the
//! source tables, convert them (`dbine-schema`), then — only when the user
//! runs it — create them on the chosen target connection, copy the data and
//! add the foreign keys. `migration_plan` previews (report + script);
//! `migration_run` does it, the data through the bulk transfer engine
//! (`dbine-transfer`): tables in parallel, native bulk loads, resumable
//! after a cut (docs/transferencia-masiva.md).
//!
//! Three modes ([`MigrationMode`]): convert (the above, between any engines), clone (same
//! engine: the driver's `CloneScript` leaves the target identical) and sync (same driver: only
//! the rows that changed, `TransferMode::Delta`, no DDL).

use crate::error::{CommandError, CommandResult};
use crate::state::AppState;
use dbine_driver::sql::{qualified_name, Quote};
use dbine_driver::{
    async_trait, kinds, CloneScript, ColumnDef, DdlParts, DeltaDepth, Driver, Language, LoadSpec, ObjectRef, QueryOutcome, ReadSpec, Session,
    TableSchema, TransferColumn,
};
use dbine_schema::{ColumnMapping, Conversion, Issue, IssueCode, Options, Severity};
use dbine_transfer::{
    Control, CopyOrder, CopyStats, Endpoints, Engine, Event, LogLevel, RunOptions, RunStatus, Store, TableState, TableStatus, TransferJob, TransferMode,
};
use serde::{Deserialize, Serialize};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;
use tauri::{AppHandle, Emitter, State};

#[derive(Serialize)]
pub struct MigrationTarget {
    pub id: String,
    pub name: String,
    pub family: String,
    /// Tables can be converted to it.
    pub supported: bool,
    /// Why not, when it isn't.
    pub reason: Option<String>,
    /// Clone from the given source's engine.
    pub clone: ModeSupport,
    /// Sync only what changed from the given source's engine.
    pub sync: ModeSupport,
}

/// A mode the pair (source, target) can use, or why not (Spanish).
#[derive(Serialize, Clone, Default)]
pub struct ModeSupport {
    pub available: bool,
    pub reason: Option<String>,
}

impl ModeSupport {
    fn of(r: Result<(), String>) -> Self {
        match r {
            Ok(()) => ModeSupport { available: true, reason: None },
            Err(why) => ModeSupport { available: false, reason: Some(why) },
        }
    }
}

/// How the tables reach the target.
#[derive(Serialize, Deserialize, Clone, Copy, PartialEq, Eq, Default, Debug)]
#[serde(rename_all = "snake_case")]
pub enum MigrationMode {
    /// Convert the structure (`dbine-schema`), create it and copy the rows.
    #[default]
    Convert,
    /// Same engine: the driver's script leaves the target identical.
    Clone,
    /// Same driver: only the rows that differ, into existing tables.
    Sync,
}

/// Both drivers come from the same known driver crate (strict: a downloaded driver only with itself).
fn same_crate(a: &str, b: &str) -> bool {
    if a == b {
        return true;
    }
    // Built-in crates don't change while the app runs.
    static CRATES: std::sync::OnceLock<HashMap<&'static str, usize>> = std::sync::OnceLock::new();
    let crates = CRATES.get_or_init(|| {
        dbine_drivers::packages().iter().enumerate().flat_map(|(k, (_, ds))| ds.iter().map(move |d| (d.info().id, k))).collect()
    });
    matches!((crates.get(a), crates.get(b)), (Some(x), Some(y)) if x == y)
}

/// Whether `target` can clone from `source` ("clonar"), or why not.
fn clone_support(source: &dyn Driver, target: &dyn Driver) -> Result<(), String> {
    let (s, t) = (source.info(), target.info());
    if !same_crate(s.id, t.id) {
        return Err(format!("clonar necesita el mismo motor en origen y destino ({} → {})", s.name, t.name));
    }
    if !target.supports_clone() {
        return Err(format!("{} no clona bases", t.name));
    }
    Ok(())
}

/// Whether the pair can sync by rows ("sincronizar solo lo que cambió"), or why not.
fn sync_support(source: &dyn Driver, target: &dyn Driver) -> Result<(), String> {
    let (s, t) = (source.info(), target.info());
    if s.id != t.id {
        return Err(format!("sincronizar por filas necesita el mismo motor en origen y destino ({} → {})", s.name, t.name));
    }
    if !source.supports_delta() {
        return Err(format!("{} no sincroniza por filas", s.name));
    }
    Ok(())
}

/// The mode can't run between these engines: the reason, as a request error.
fn check_mode(mode: MigrationMode, source: &dyn Driver, target: &dyn Driver) -> CommandResult<()> {
    let r = match mode {
        MigrationMode::Convert => Ok(()),
        MigrationMode::Clone => clone_support(source, target),
        MigrationMode::Sync => sync_support(source, target),
    };
    r.map_err(CommandError::BadRequest)
}

#[derive(Deserialize, Default)]
pub struct TargetsArgs {
    /// The source connection: which targets can clone or sync from it.
    #[serde(default)]
    pub source_connection_id: Option<String>,
}

/// Every engine, and whether it can be a migration target (and, given the source, a clone or
/// sync one).
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_targets(state: State<'_, AppState>, args: Option<TargetsArgs>) -> CommandResult<Vec<MigrationTarget>> {
    let source = args
        .and_then(|a| a.source_connection_id)
        .and_then(|id| crate::commands::schema::driver_of(&state, &id).ok());
    Ok(targets_for(source.map(|d| d.as_ref())))
}

fn targets_for(source: Option<&dyn Driver>) -> Vec<MigrationTarget> {
    dbine_drivers::all()
        .iter()
        .map(|d| {
            let info = d.info();
            let reason = match dbine_schema::dialect::for_driver(info.id) {
                None => Some("todavía no hay conversión de esquemas para este motor".to_string()),
                Some(dialect) => dialect.target_refusal(info.id).map(str::to_string),
            };
            let (clone, sync) = match source {
                Some(s) => (ModeSupport::of(clone_support(s, d.as_ref())), ModeSupport::of(sync_support(s, d.as_ref()))),
                None => Default::default(),
            };
            MigrationTarget {
                id: info.id.into(),
                name: info.name.into(),
                family: serde_json::to_value(info.family).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default(),
                supported: reason.is_none(),
                reason,
                clone,
                sync,
            }
        })
        .collect()
}

#[derive(Serialize, Deserialize, Clone)]
pub struct TableRef {
    pub schema: Option<String>,
    pub name: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PlanOptions {
    /// Refold regular names to the target's case.
    #[serde(default = "yes")]
    pub fold_case: bool,
    /// Schema / keyspace in the target; empty = its default.
    #[serde(default)]
    pub target_schema: Option<String>,
    #[serde(default)]
    pub drop: bool,
    #[serde(default = "yes")]
    pub if_exists: bool,
    #[serde(default = "yes")]
    pub indexes: bool,
    #[serde(default = "yes")]
    pub foreign_keys: bool,
    /// Copy the rows too (migration_run).
    #[serde(default = "yes")]
    pub data: bool,
    /// Keep each table in its source schema (creating the schemas), when the
    /// target has schemas and no single `target_schema` was given.
    #[serde(default = "yes")]
    pub keep_schemas: bool,
}

fn yes() -> bool {
    true
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PlanArgs {
    pub connection_id: String,
    pub database: String,
    /// Tables to migrate; empty = all of them.
    #[serde(default)]
    pub tables: Vec<TableRef>,
    pub target_driver: String,
    pub options: PlanOptions,
    #[serde(default)]
    pub mode: MigrationMode,
    /// Sync mode's settings.
    #[serde(default)]
    pub sync: SyncOptions,
    /// Clone preview: the target connection (the script adapts to what it supports; it's only
    /// queried). A run uses its own target connection.
    #[serde(default)]
    pub target: Option<PlanTarget>,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct PlanTarget {
    pub connection_id: String,
    #[serde(default)]
    pub database: String,
}

#[derive(Serialize, Deserialize, Clone)]
pub struct SyncOptions {
    #[serde(default = "full_depth")]
    pub depth: DeltaDepth,
    /// Cores the engine may use for the summary (0: the server decides).
    #[serde(default)]
    pub max_cores: u32,
    /// Keys chosen for tables (otherwise, the primary key).
    #[serde(default)]
    pub keys: Vec<SyncKey>,
}

fn full_depth() -> DeltaDepth {
    DeltaDepth::Full
}

impl Default for SyncOptions {
    fn default() -> Self {
        SyncOptions { depth: DeltaDepth::Full, max_cores: 0, keys: Vec::new() }
    }
}

/// A table's key for the sync, chosen by the user (a unique key's columns).
#[derive(Serialize, Deserialize, Clone)]
pub struct SyncKey {
    pub schema: Option<String>,
    pub name: String,
    pub columns: Vec<String>,
}

/// A table of a sync plan: its keys and the one used, or why it can't be synced.
#[derive(Serialize)]
pub struct SyncTable {
    pub schema: Option<String>,
    pub name: String,
    pub primary_key: Option<Vec<String>>,
    /// Unique keys (without a filter) that can be picked instead.
    pub unique_keys: Vec<Vec<String>>,
    pub key: Option<Vec<String>>,
    pub reason: Option<String>,
}

#[derive(Serialize)]
pub struct PlannedTable {
    pub source: String,
    pub target: String,
    pub columns: usize,
}

#[derive(Serialize)]
pub struct MigrationPlan {
    pub tables: Vec<PlannedTable>,
    pub columns: Vec<ColumnMapping>,
    pub issues: Vec<Issue>,
    /// The target's DDL, in order: DROP (if asked), tables and indexes, then
    /// foreign keys.
    pub script: String,
    /// The source tables available (for the picker).
    pub available: Vec<SourceTable>,
    /// Sync mode: each table's key, or why it can't be synced.
    pub sync_tables: Vec<SyncTable>,
}

#[derive(Serialize)]
pub struct SourceTable {
    pub schema: Option<String>,
    pub name: String,
    pub columns: usize,
}

fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{s}.{name}"),
        None => name.to_string(),
    }
}

/// The DDL of a migration, as statements to run one by one (and to print as
/// a script).
struct Ddl {
    /// `CREATE SCHEMA` for the schemas the tables go to.
    schemas: Vec<String>,
    drops: Vec<(usize, String)>,
    creates: Vec<(usize, String)>,
    /// Apart from the CREATE: a failing index doesn't lose the table.
    indexes: Vec<(usize, String)>,
    fks: Vec<(usize, String)>,
}

struct Prepared {
    source: &'static Arc<dyn Driver>,
    target: &'static Arc<dyn Driver>,
    chosen: Vec<TableSchema>,
    conv: Conversion,
    available: Vec<SourceTable>,
    ddl: Ddl,
}

/// The two drivers and the source tables (the chosen ones and all of them).
struct Picked {
    source: &'static Arc<dyn Driver>,
    target: &'static Arc<dyn Driver>,
    chosen: Vec<TableSchema>,
    available: Vec<SourceTable>,
}

async fn pick(state: &AppState, args: &PlanArgs) -> CommandResult<Picked> {
    let source = crate::commands::schema::driver_of(state, &args.connection_id)?;
    let target = dbine_drivers::find(&args.target_driver)
        .ok_or_else(|| CommandError::BadRequest(format!("esta versión no incluye el driver '{}'", args.target_driver)))?;
    let all: Vec<TableSchema> = state
        .meta_read(&args.connection_id, &args.database, crate::commands::explorer::SCHEMA_LIMIT, |s| Box::pin(s.database_schema()))
        .await?;
    let tables_only: Vec<TableSchema> = all.into_iter().filter(|t| t.kind.is_empty() || t.kind == kinds::TABLE).collect();
    let available = tables_only
        .iter()
        .map(|t| SourceTable { schema: t.schema.clone(), name: t.name.clone(), columns: t.columns.len() })
        .collect();
    let chosen: Vec<TableSchema> = if args.tables.is_empty() {
        tables_only
    } else {
        tables_only
            .into_iter()
            .filter(|t| args.tables.iter().any(|r| r.name == t.name && (r.schema.is_none() || r.schema == t.schema)))
            .collect()
    };
    Ok(Picked { source, target, chosen, available })
}

async fn prepare(state: &AppState, args: &PlanArgs) -> CommandResult<Prepared> {
    let Picked { source, target, chosen, available } = pick(state, args).await?;
    let single_schema = args.options.target_schema.clone().filter(|s| !s.trim().is_empty());
    let opts = Options {
        fold_case: args.options.fold_case,
        keep_schemas: args.options.keep_schemas && single_schema.is_none() && target.info().has_schemas,
        rename_schemas: default_schema_rename(source.info().dialect, target.info().dialect),
        target_schema: single_schema,
        ..Options::default()
    };
    let conv = dbine_schema::convert(&chosen, source.info().id, &args.target_driver, &opts)
        .map_err(|e| CommandError::BadRequest(e.to_string()))?;
    let o = &args.options;
    let mut ddl = Ddl { schemas: vec![], drops: vec![], creates: vec![], indexes: vec![], fks: vec![] };
    if target.info().has_schemas {
        let mut seen: Vec<String> = Vec::new();
        for t in &conv.tables {
            if let Some(sc) = t.schema.as_deref().filter(|s| !s.is_empty()) {
                if !seen.iter().any(|x| x == sc) {
                    seen.push(sc.to_string());
                    if let Some(stmt) = create_schema(target.info().dialect, sc) {
                        ddl.schemas.push(stmt);
                    }
                }
            }
        }
    }
    if o.drop {
        for (i, t) in conv.tables.iter().enumerate().rev() {
            ddl.drops.push((i, target.table_ddl(t, DdlParts { drop: true, if_exists: o.if_exists, ..Default::default() })?));
        }
    }
    for (i, t) in conv.tables.iter().enumerate() {
        let parts = DdlParts { create: true, if_exists: o.if_exists && !o.drop, ..Default::default() };
        ddl.creates.push((i, target.table_ddl(t, parts)?));
        if o.indexes && !t.indexes.is_empty() {
            let ix = target.table_ddl(t, DdlParts { indexes: true, if_exists: o.if_exists && !o.drop, ..Default::default() })?;
            if !ix.trim().is_empty() {
                ddl.indexes.push((i, ix));
            }
        }
    }
    if o.foreign_keys {
        for (i, t) in conv.tables.iter().enumerate().filter(|(_, t)| !t.foreign_keys.is_empty()) {
            // Engines without separate FK statements return an error or
            // nothing: the FKs are then inside CREATE TABLE already.
            if let Ok(fk) = target.table_ddl(t, DdlParts { foreign_keys: true, ..Default::default() }) {
                if !fk.trim().is_empty() {
                    ddl.fks.push((i, fk));
                }
            }
        }
    }
    Ok(Prepared { source, target, chosen, conv, available, ddl })
}

/// `CREATE SCHEMA` if missing, in the target's dialect (`None` for engines
/// where a schema is a user or a database: those are created apart).
/// An engine's default schema (where unqualified tables go).
pub(crate) fn default_schema(dialect: &str) -> Option<&'static str> {
    match dialect {
        "mssql" | "sybase" => Some("dbo"),
        "postgres" => Some("public"),
        _ => None,
    }
}

/// The source's default schema goes to the target's (`dbo` → `public`).
pub(crate) fn default_schema_rename(from: &str, to: &str) -> Vec<(String, String)> {
    match (default_schema(from), default_schema(to)) {
        (Some(a), Some(b)) if a != b => vec![(a.to_string(), b.to_string())],
        _ => Vec::new(),
    }
}

pub(crate) fn create_schema(dialect: &str, name: &str) -> Option<String> {
    let dq = format!("\"{}\"", name.replace('"', "\"\""));
    match dialect {
        "oracle" | "mysql" | "hive" | "sparksql" | "databricks" | "bigquery" | "clickhouse" => None,
        // The engine's own default schema always exists.
        _ if default_schema(dialect) == Some(name) => None,
        "mssql" | "sybase" => Some(format!(
            "IF SCHEMA_ID(N'{0}') IS NULL EXEC(N'CREATE SCHEMA [{1}]')",
            name.replace('\'', "''"),
            name.replace(']', "]]").replace('\'', "''")
        )),
        "db2" | "hana" | "informix" | "teradata" | "sybase_iq" => Some(format!("CREATE SCHEMA {dq}")),
        _ => Some(format!("CREATE SCHEMA IF NOT EXISTS {dq}")),
    }
}

/// Statements as a script for `target`: a header (two comment lines), then each statement ended
/// the engine's way.
fn script_text<'a>(target: &dyn Driver, header: [String; 2], statements: impl IntoIterator<Item = &'a String>) -> String {
    let sep = target.script_separator();
    let sql = target.info().language == Language::Sql;
    let end_block = |text: &str| -> String {
        let t = text.trim_end();
        if t.is_empty() {
            return String::new();
        }
        match (sep.is_empty(), sql && !t.ends_with(';')) {
            (false, _) => format!("{t}\n{sep}\n\n"),
            (true, true) => format!("{t};\n\n"),
            (true, false) => format!("{t}\n\n"),
        }
    };
    let comment = if sql { "--" } else { "//" };
    let mut script = format!("{comment} {}\n{comment} {}\n\n", header[0], header[1]);
    for s in statements {
        script.push_str(&end_block(s));
    }
    script
}

fn database_label(args: &PlanArgs) -> &str {
    if args.database.is_empty() {
        "la base"
    } else {
        &args.database
    }
}

fn script_of(p: &Prepared, args: &PlanArgs) -> String {
    let header = [
        format!("Estructura de {} ({}) convertida a {}", database_label(args), p.source.info().name, p.target.info().name),
        format!("{} tabla(s). Generado por DBine.", p.conv.tables.len()),
    ];
    let statements = p.ddl.schemas.iter().chain(p.ddl.drops.iter().chain(&p.ddl.creates).chain(&p.ddl.indexes).chain(&p.ddl.fks).map(|(_, s)| s));
    script_text(p.target.as_ref(), header, statements)
}

/// The clone script as the user reads it: before, each table (create, before and after its
/// data), after.
fn clone_script_text(p: &Picked, args: &PlanArgs, c: &CloneScript) -> String {
    let header = [
        format!("Clonado de {} ({})", database_label(args), p.source.info().name),
        format!("{} tabla(s). Generado por DBine.", c.tables.len()),
    ];
    let tables = c.tables.iter().flat_map(|t| std::iter::once(&t.create).chain(&t.before_data).chain(&t.after_data));
    script_text(p.target.as_ref(), header, c.before.iter().chain(tables).chain(&c.after))
}

/// The tables the driver's clone script is asked for.
fn table_refs(tables: &[TableSchema]) -> Vec<ObjectRef> {
    tables.iter().map(|t| ObjectRef { kind: kinds::TABLE.into(), schema: t.schema.clone(), name: t.name.clone() }).collect()
}

/// The driver's clone script: the source is only read (a read-only connection); `target` is only
/// asked what it supports.
async fn read_clone_script(state: &AppState, p: &Picked, args: &PlanArgs, key: &str, target: &mut Box<dyn Session>) -> CommandResult<CloneScript> {
    let skey = format!("{key}:src");
    let r = match state.dedicated_session(&skey, &args.connection_id, &args.database, true).await {
        Ok(src) => {
            let mut s = src.session.lock().await;
            p.target.clone_script(&mut **s, &mut **target, &table_refs(&p.chosen)).await.map_err(CommandError::from)
        }
        Err(e) => Err(e),
    };
    state.sessions.remove(&skey);
    r
}

/// Unique keys of a table (not the primary key, no filter), for the sync's key picker.
fn unique_keys(t: &TableSchema) -> Vec<Vec<String>> {
    let pk = t.primary_key.as_ref().map(|k| &k.columns);
    let mut out: Vec<Vec<String>> = Vec::new();
    for ix in t.indexes.iter().filter(|i| i.unique && i.filter.is_none() && !i.columns.is_empty()) {
        if Some(&ix.columns) != pk && !out.contains(&ix.columns) {
            out.push(ix.columns.clone());
        }
    }
    out
}

/// The key a table syncs by: the one the user chose (the primary key or a unique key), else
/// the primary key; or why it can't be synced.
fn sync_key(t: &TableSchema, o: &SyncOptions) -> Result<Vec<String>, String> {
    let pk = t.primary_key.as_ref().map(|k| k.columns.clone()).filter(|c| !c.is_empty());
    let uniques = unique_keys(t);
    let chosen = o
        .keys
        .iter()
        .find(|k| k.name == t.name && (k.schema.is_none() || k.schema == t.schema) && !k.columns.is_empty());
    if let Some(k) = chosen {
        if pk.as_ref() == Some(&k.columns) || uniques.contains(&k.columns) {
            return Ok(k.columns.clone());
        }
        return Err("la clave elegida no es la clave primaria ni una clave única de la tabla".into());
    }
    match pk {
        Some(pk) => Ok(pk),
        None if uniques.is_empty() => Err("no tiene clave primaria ni claves únicas: no se puede sincronizar".into()),
        None => Err("no tiene clave primaria: elegí una clave única para sincronizarla".into()),
    }
}

/// Preview: convert (report + script), clone (the driver's script and its notes) or sync (each
/// table's key). Nothing runs on either side.
async fn plan(state: &AppState, args: &PlanArgs) -> CommandResult<MigrationPlan> {
    let same = |p: &Picked| -> Vec<PlannedTable> {
        p.chosen
            .iter()
            .map(|t| {
                let name = qualified(t.schema.as_deref(), &t.name);
                PlannedTable { source: name.clone(), target: name, columns: t.columns.len() }
            })
            .collect()
    };
    match args.mode {
        MigrationMode::Convert => {
            let p = prepare(state, args).await?;
            let script = script_of(&p, args);
            let planned = p
                .chosen
                .iter()
                .zip(p.conv.tables.iter())
                .map(|(s, t)| PlannedTable {
                    source: qualified(s.schema.as_deref(), &s.name),
                    target: qualified(t.schema.as_deref(), &t.name),
                    columns: t.columns.len(),
                })
                .collect();
            Ok(MigrationPlan {
                tables: planned,
                columns: p.conv.columns,
                issues: p.conv.issues,
                script,
                available: p.available,
                sync_tables: Vec::new(),
            })
        }
        MigrationMode::Clone => {
            let p = pick(state, args).await?;
            check_mode(args.mode, p.source.as_ref(), p.target.as_ref())?;
            let target = args
                .target
                .as_ref()
                .ok_or_else(|| CommandError::BadRequest("para ver el script del clonado, elegí la conexión de destino".into()))?;
            let key = format!("migrate-plan:{}", uuid_like());
            let tkey = format!("{key}:tgt");
            let tgt = state.dedicated_session(&tkey, &target.connection_id, &target.database, true).await?;
            let script = {
                let mut t = tgt.session.lock().await;
                read_clone_script(state, &p, args, &key, &mut t).await
            };
            state.sessions.remove(&tkey);
            let script = script?;
            let issues = script
                .notes
                .iter()
                .map(|n| Issue { severity: Severity::Warning, code: IssueCode::OptionDropped, table: String::new(), object: None, message: n.clone() })
                .collect();
            Ok(MigrationPlan {
                tables: same(&p),
                columns: Vec::new(),
                issues,
                script: clone_script_text(&p, args, &script),
                available: p.available,
                sync_tables: Vec::new(),
            })
        }
        MigrationMode::Sync => {
            let p = pick(state, args).await?;
            check_mode(args.mode, p.source.as_ref(), p.target.as_ref())?;
            let sync_tables = p
                .chosen
                .iter()
                .map(|t| {
                    let key = sync_key(t, &args.sync);
                    SyncTable {
                        schema: t.schema.clone(),
                        name: t.name.clone(),
                        primary_key: t.primary_key.as_ref().map(|k| k.columns.clone()).filter(|c| !c.is_empty()),
                        unique_keys: unique_keys(t),
                        key: key.clone().ok(),
                        reason: key.err(),
                    }
                })
                .collect();
            Ok(MigrationPlan { tables: same(&p), columns: Vec::new(), issues: Vec::new(), script: String::new(), available: p.available, sync_tables })
        }
    }
}

/// A key for a preview's own connections.
fn uuid_like() -> String {
    static SEQ: AtomicU64 = AtomicU64::new(0);
    format!("{}-{}", chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0), SEQ.fetch_add(1, Ordering::Relaxed))
}

/// Read the source structure and preview the migration in the chosen mode: convert (report +
/// target script), clone (the driver's script) or sync (each table's key).
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_plan(state: State<'_, AppState>, args: PlanArgs) -> CommandResult<MigrationPlan> {
    plan(&state, &args).await
}

// -- running it ---------------------------------------------------------------------------------
//
// A run goes in three stages, saved in its record (`<config>/migrations/<id>.json`) so it can be
// resumed after the app closes:
// 1. `structure`: schemas, DROP (if asked) and CREATE TABLE with columns and primary key only.
// 2. `data`: the bulk transfer engine, one job per table; each table's indexes are its job's
//    `post`, created as soon as its copy ends while other tables go on copying. The engine keeps
//    its own state per table (`dbine-transfer.sqlite`).
// 3. `constraints`: foreign keys, then (SQL Server → SQL Server) the identity's next value.

/// The engine's advanced options, as the UI sends them (`None`: the engine's default).
#[derive(Serialize, Deserialize, Clone, Default)]
pub struct TransferOptions {
    /// Tables copying at once (1 to 32).
    #[serde(default)]
    pub parallel: Option<usize>,
    #[serde(default)]
    pub order: Option<CopyOrder>,
    /// Commit every this many rows.
    #[serde(default)]
    pub commit_rows: Option<u64>,
}

impl TransferOptions {
    fn run_options(&self) -> RunOptions {
        let mut o = RunOptions::default();
        if let Some(n) = self.parallel {
            o.parallel = n.clamp(1, dbine_transfer::MAX_PARALLEL);
        }
        if let Some(order) = self.order {
            o.order = order;
        }
        if let Some(n) = self.commit_rows.filter(|n| *n > 0) {
            o.commit_rows = n;
        }
        o
    }
}

#[derive(Deserialize)]
pub struct RunArgs {
    /// The run's id (letters, digits, `-` and `_`): its events and its record.
    pub migration_id: String,
    #[serde(flatten)]
    pub plan: PlanArgs,
    pub target_connection_id: String,
    #[serde(default)]
    pub target_database: String,
    #[serde(default)]
    pub transfer: TransferOptions,
}

/// A table of a run, as the UI shows it.
#[derive(Serialize, Clone)]
pub struct RunTable {
    /// The job's name (the source table, qualified): the key for cancel / run now.
    pub name: String,
    pub source: String,
    pub target: String,
    /// `pending`, `running`, `copied` (indexes missing), `done`, `failed`, `cancelled`.
    pub status: String,
    pub rows_done: u64,
    pub rows_total: Option<u64>,
    /// The way its rows travel (`native`, `bulk_load`, `insert_script`): the measured one once
    /// copied, the expected one before.
    pub path: String,
    pub stats: Option<CopyStats>,
    pub attempts: u32,
    pub error: Option<String>,
}

#[derive(Serialize)]
pub struct RunResult {
    pub run_id: String,
    /// `running`, `interrupted`, `done`, `failed`, `cancelled`.
    pub status: String,
    pub tables: Vec<RunTable>,
    pub foreign_key_errors: Vec<String>,
    /// Clone: the final statements (constraints, code…) that still failed after their passes.
    pub after_errors: Vec<String>,
    /// What else happened (identity reseeds, tables that can't be emptied, the clone's notes…).
    pub notes: Vec<String>,
    pub elapsed_ms: u64,
    pub cancelled: bool,
    pub mode: MigrationMode,
}

/// A run in `migration_runs`.
#[derive(Serialize)]
pub struct RunInfo {
    pub id: String,
    pub status: String,
    /// `structure`, `data`, `constraints`, `finished`.
    pub stage: String,
    pub created_at: String,
    pub finished_at: Option<String>,
    pub source_connection_id: String,
    pub source_database: String,
    pub target_connection_id: String,
    pub target_database: String,
    pub target_driver: String,
    pub parallel: usize,
    /// `resume` can go on with it.
    pub resumable: bool,
    pub tables: Vec<RunTable>,
    pub foreign_key_errors: Vec<String>,
    pub after_errors: Vec<String>,
    pub notes: Vec<String>,
    pub mode: MigrationMode,
}

/// A run's record: what `resume` needs to go on after the app closes.
#[derive(Serialize, Deserialize, Clone)]
struct RunMeta {
    id: String,
    created_at: String,
    #[serde(default)]
    finished_at: Option<String>,
    /// `running`, `interrupted`, `done`, `failed`, `cancelled`.
    status: String,
    /// `structure`, `data`, `constraints`, `finished`.
    stage: String,
    plan: PlanArgs,
    target_connection_id: String,
    target_database: String,
    options: RunOptions,
    #[serde(default)]
    tables: Vec<MetaTable>,
    #[serde(default)]
    fks: Vec<FkStep>,
    #[serde(default)]
    reseeds: Vec<Reseed>,
    #[serde(default)]
    foreign_key_errors: Vec<String>,
    /// Clone: the script's final statements, run after the data.
    #[serde(default)]
    after: Vec<AfterStep>,
    #[serde(default)]
    after_errors: Vec<String>,
    #[serde(default)]
    notes: Vec<String>,
    /// Time spent in earlier segments (before a cut or a resume).
    #[serde(default)]
    elapsed_ms: u64,
}

#[derive(Serialize, Deserialize, Clone)]
struct AfterStep {
    sql: String,
    done: bool,
}

#[derive(Serialize, Deserialize, Clone)]
struct MetaTable {
    name: String,
    source: String,
    target: String,
    path: String,
    created: bool,
    /// Its CREATE failed (no copy), or, without data, its indexes did.
    error: Option<String>,
}

#[derive(Serialize, Deserialize, Clone)]
struct FkStep {
    table: String,
    label: String,
    sql: String,
    done: bool,
}

/// SQL Server → SQL Server: the target's identity continues where the source's does.
#[derive(Serialize, Deserialize, Clone)]
struct Reseed {
    table: String,
    /// `SELECT IDENT_CURRENT(…)` on the source (a read).
    source_sql: String,
    /// The target table, for `DBCC CHECKIDENT`.
    target: String,
    done: bool,
}

type Emit = Arc<dyn Fn(Value) + Send + Sync>;

fn now() -> String {
    chrono::Utc::now().to_rfc3339()
}

/// An event for the UI: the engine's own (`{event: "table_progress", …}`) or a stage step
/// (`{event: "step", …}`), with the run's id.
fn tagged(id: &str, e: &impl Serialize) -> Value {
    let mut v = serde_json::to_value(e).unwrap_or(Value::Null);
    if let Value::Object(m) = &mut v {
        m.insert("id".into(), Value::String(id.to_string()));
    }
    v
}

fn step(emit: &Emit, id: &str, phase: &str, table: &str, done: usize, total: usize) {
    emit(json!({ "id": id, "event": "step", "phase": phase, "table": table, "done": done, "total": total }));
}

// -- the runs' registry ---------------------------------------------------------------------------

/// The migrations' state: the engine's store, the runs' records and the runs going on now.
pub struct MigrationRuns {
    store: Arc<Store>,
    dir: PathBuf,
    live: Mutex<HashMap<String, Arc<LiveRun>>>,
}

/// A run going on in this process.
#[derive(Default)]
struct LiveRun {
    cancelled: AtomicBool,
    control: Mutex<Option<Control>>,
}

impl LiveRun {
    fn control(&self) -> Option<Control> {
        self.control.lock().unwrap_or_else(|p| p.into_inner()).clone()
    }
    fn set_control(&self, c: Option<Control>) {
        *self.control.lock().unwrap_or_else(|p| p.into_inner()) = c;
    }
    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

/// Takes the run out of the live map however its command ends.
struct LiveGuard<'a> {
    runs: &'a MigrationRuns,
    id: String,
}

impl Drop for LiveGuard<'_> {
    fn drop(&mut self) {
        self.runs.live().remove(&self.id);
    }
}

fn valid_id(id: &str) -> CommandResult<()> {
    if id.is_empty() || id.len() > 64 || !id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_') {
        return Err(CommandError::BadRequest("id de migración inválido".into()));
    }
    Ok(())
}

impl MigrationRuns {
    /// At startup: the engine's state next to the app's (`dbine-transfer.sqlite`) and the runs'
    /// records in `migrations/`. Runs left running by a process that ended become interrupted.
    pub fn open(config_dir: &Path) -> Self {
        let store = Store::open(config_dir.join("dbine-transfer.sqlite")).unwrap_or_else(|e| {
            tracing::error!(%e, "transfer state: using a temporary one");
            Store::open(":memory:").expect("in-memory transfer state")
        });
        match store.mark_running_as_interrupted() {
            Ok(n) if n > 0 => tracing::info!(runs = n, "transfers interrupted by the last exit"),
            Ok(_) => {}
            Err(e) => tracing::warn!(%e, "transfer state"),
        }
        let runs = Self::at(Arc::new(store), config_dir.join("migrations"));
        for mut m in runs.records() {
            if m.status == "running" {
                m.status = "interrupted".into();
                runs.save(&m);
            }
        }
        runs
    }

    fn at(store: Arc<Store>, dir: PathBuf) -> Self {
        let _ = std::fs::create_dir_all(&dir);
        MigrationRuns { store, dir, live: Mutex::new(HashMap::new()) }
    }

    fn live(&self) -> std::sync::MutexGuard<'_, HashMap<String, Arc<LiveRun>>> {
        self.live.lock().unwrap_or_else(|p| p.into_inner())
    }

    fn path(&self, id: &str) -> PathBuf {
        self.dir.join(format!("{id}.json"))
    }

    fn load(&self, id: &str) -> CommandResult<RunMeta> {
        valid_id(id)?;
        let text = std::fs::read_to_string(self.path(id)).map_err(|_| CommandError::NotFound("la migración no existe".into()))?;
        serde_json::from_str(&text).map_err(|e| CommandError::State(format!("registro de la migración: {e}")))
    }

    fn save(&self, m: &RunMeta) {
        let tmp = self.dir.join(format!("{}.json.tmp", m.id));
        let r = serde_json::to_vec_pretty(m)
            .map_err(std::io::Error::other)
            .and_then(|b| std::fs::write(&tmp, b))
            .and_then(|_| std::fs::rename(&tmp, self.path(&m.id)));
        if let Err(e) = r {
            tracing::warn!(%e, id = %m.id, "migration record");
        }
    }

    fn records(&self) -> Vec<RunMeta> {
        let Ok(rd) = std::fs::read_dir(&self.dir) else { return Vec::new() };
        let mut out: Vec<RunMeta> = rd
            .flatten()
            .filter(|e| e.path().extension().is_some_and(|x| x == "json"))
            .filter_map(|e| std::fs::read_to_string(e.path()).ok())
            .filter_map(|t| serde_json::from_str(&t).ok())
            .collect();
        out.sort_by(|a, b| b.created_at.cmp(&a.created_at));
        out
    }

    /// Register a run as going on (one command per run at a time).
    fn begin(&self, id: &str) -> CommandResult<(Arc<LiveRun>, LiveGuard<'_>)> {
        let mut live = self.live();
        if live.contains_key(id) {
            return Err(CommandError::BadRequest("la migración ya está corriendo".into()));
        }
        let run = Arc::new(LiveRun::default());
        live.insert(id.to_string(), run.clone());
        Ok((run, LiveGuard { runs: self, id: id.to_string() }))
    }

    fn running(&self, id: &str) -> CommandResult<Arc<LiveRun>> {
        self.live().get(id).cloned().ok_or_else(|| CommandError::NotFound("la migración no está corriendo".into()))
    }

    fn control(&self, id: &str) -> CommandResult<Control> {
        self.running(id)?.control().ok_or_else(|| CommandError::BadRequest("la migración todavía no está copiando datos".into()))
    }

    /// The run's tables: its record merged with the engine's state.
    fn tables(&self, m: &RunMeta) -> Vec<RunTable> {
        let states: HashMap<String, TableState> =
            self.store.tables(&m.id).unwrap_or_default().into_iter().map(|t| (t.name.clone(), t)).collect();
        m.tables
            .iter()
            .map(|t| {
                let mut r = RunTable {
                    name: t.name.clone(),
                    source: t.source.clone(),
                    target: t.target.clone(),
                    status: "pending".into(),
                    rows_done: 0,
                    rows_total: None,
                    path: t.path.clone(),
                    stats: None,
                    attempts: 0,
                    error: t.error.clone(),
                };
                if !t.created {
                    r.status = if t.error.is_some() { "failed" } else { "pending" }.into();
                } else if let Some(s) = states.get(&t.name) {
                    r.status = status_str(s.status);
                    r.rows_done = s.rows_done;
                    r.rows_total = s.rows_total;
                    r.attempts = s.attempts;
                    r.error = s.error.clone().or(r.error);
                    if let Some(p) = s.stats.as_ref().and_then(|st| st.path) {
                        r.path = serde_json::to_value(p).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or(r.path);
                    }
                    r.stats = s.stats.clone();
                } else if !m.plan.options.data && m.stage == "finished" {
                    r.status = if t.error.is_some() { "failed" } else { "done" }.into();
                }
                r
            })
            .collect()
    }

    fn result(&self, m: &RunMeta) -> RunResult {
        RunResult {
            run_id: m.id.clone(),
            status: m.status.clone(),
            tables: self.tables(m),
            foreign_key_errors: m.foreign_key_errors.clone(),
            after_errors: m.after_errors.clone(),
            notes: m.notes.clone(),
            elapsed_ms: m.elapsed_ms,
            cancelled: m.status == "cancelled",
            mode: m.plan.mode,
        }
    }
}

fn status_str(s: TableStatus) -> String {
    serde_json::to_value(s).ok().and_then(|v| v.as_str().map(str::to_string)).unwrap_or_default()
}

// -- connections for the engine -------------------------------------------------------------------

/// The run's two databases, one new connection per table and attempt (the source read-only).
struct MigrationEndpoints {
    state: AppState,
    run_id: String,
    source: (String, String),
    target: (String, String),
    source_driver: Arc<dyn Driver>,
    target_driver: Arc<dyn Driver>,
    seq: AtomicU64,
}

impl MigrationEndpoints {
    fn new(state: &AppState, m: &RunMeta) -> CommandResult<Self> {
        let source_driver = crate::commands::schema::driver_of(state, &m.plan.connection_id)?.clone();
        let target_driver = dbine_drivers::find(&m.plan.target_driver)
            .ok_or_else(|| CommandError::BadRequest(format!("esta versión no incluye el driver '{}'", m.plan.target_driver)))?
            .clone();
        Ok(MigrationEndpoints {
            state: state.clone(),
            run_id: m.id.clone(),
            source: (m.plan.connection_id.clone(), m.plan.database.clone()),
            target: (m.target_connection_id.clone(), m.target_database.clone()),
            source_driver,
            target_driver,
            seq: AtomicU64::new(0),
        })
    }

    async fn open(&self, side: &str, (conn, db): &(String, String), read_only: bool) -> dbine_driver::Result<Box<dyn Session>> {
        let key = format!("migrate:{}:{side}:{}", self.run_id, self.seq.fetch_add(1, Ordering::Relaxed));
        let entry = self.state.dedicated_session(&key, conn, db, read_only).await.map_err(driver_error);
        // The engine owns the connection (and interrupts it itself): it leaves the app's map.
        self.state.sessions.remove(&key);
        let entry = Arc::try_unwrap(entry?).map_err(|_| dbine_driver::Error::State("la conexión quedó compartida".into()))?;
        Ok(entry.session.into_inner())
    }
}

fn driver_error(e: CommandError) -> dbine_driver::Error {
    use dbine_driver::Error;
    match e {
        CommandError::Connect(m) => Error::Connect(m),
        CommandError::AuthFailed(m) => Error::AuthFailed(m),
        CommandError::PasswordRequired(_) => Error::AuthFailed(e.to_string()),
        CommandError::Cancelled => Error::Cancelled,
        other => Error::State(other.to_string()),
    }
}

/// Both drivers come from the same driver crate (the one that can copy between them).
fn same_package(a: &str, b: &str) -> bool {
    let packages = dbine_drivers::packages();
    let of = |id: &str| packages.iter().position(|(_, ds)| ds.iter().any(|d| d.info().id == id));
    match (of(a), of(b)) {
        (Some(x), Some(y)) => x == y,
        // Downloaded drivers: each plugin host serves one crate; the driver's own answer decides.
        _ => true,
    }
}

#[async_trait]
impl Endpoints for MigrationEndpoints {
    fn source_driver(&self) -> Arc<dyn Driver> {
        self.source_driver.clone()
    }
    fn target_driver(&self) -> Arc<dyn Driver> {
        self.target_driver.clone()
    }
    async fn open_source(&self) -> dbine_driver::Result<Box<dyn Session>> {
        self.open("src", &self.source, true).await
    }
    async fn open_target(&self) -> dbine_driver::Result<Box<dyn Session>> {
        let mut s = self.open("tgt", &self.target, false).await?;
        // SQLite keeps its foreign keys inside CREATE TABLE: with tables copied in parallel a
        // child may load before its parent, so this connection doesn't enforce them (the source
        // already did).
        if matches!(self.target_driver.info().id, "sqlite" | "libsql") {
            exec(&mut s, "PRAGMA foreign_keys = OFF").await.map_err(dbine_driver::Error::Query)?;
        }
        Ok(s)
    }
    fn native_copy_allowed(&self) -> bool {
        let (s, t) = (self.source_driver.info().id, self.target_driver.info().id);
        same_package(s, t) && self.source_driver.supports_native_copy(t)
    }
}

// -- the jobs ---------------------------------------------------------------------------------------

fn quote_of(dialect: &str) -> Quote {
    match dialect {
        "mssql" | "sybase" => Quote::Bracket,
        "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" => Quote::Backtick,
        _ => Quote::Double,
    }
}

/// The statement that empties a target table, in its engine's language: `TRUNCATE TABLE`, or
/// `DELETE` where there's no TRUNCATE; for other languages, the driver's drop + create. `None`:
/// no way to empty it (a copy cut halfway can't be redone by itself).
fn truncate_sql(target: &dyn Driver, t: &TableSchema) -> Option<String> {
    let info = target.info();
    // Engines that can't remove rows: dropping and recreating a ksqlDB stream
    // keeps its Kafka topic (and its rows), so it wouldn't empty anything.
    if info.id == "ksqldb" {
        return None;
    }
    match info.language {
        Language::Sql | Language::Cql => {
            let name = qualified_name(quote_of(info.dialect), t.schema.as_deref(), &t.name);
            Some(match info.id {
                "sqlite" | "libsql" | "firebird" | "access" | "dbase" | "dsql" | "iris" | "cache" | "openedge" | "zen" => {
                    format!("DELETE FROM {name}")
                }
                "spanner" => format!("DELETE FROM {name} WHERE TRUE"),
                _ => format!("TRUNCATE TABLE {name}"),
            })
        }
        _ => target
            .table_ddl(t, DdlParts { drop: true, if_exists: true, create: true, ..Default::default() })
            .ok()
            .filter(|s| !s.trim().is_empty()),
    }
}

/// A target column the engine fills itself (computed, row version): never loaded.
fn generated(c: &ColumnDef, dialect: &str) -> bool {
    let t = c.data_type.trim().to_ascii_lowercase();
    t.starts_with("as ") || t.starts_with("as(") || t.contains("generated always as") || (matches!(dialect, "mssql" | "sybase") && matches!(t.as_str(), "rowversion" | "timestamp"))
}

/// Source and target columns of a table's copy, paired by the conversion's mapping (`mappings`
/// are the table's, one per target column, in order) and in the source's column order.
fn column_pairs(mappings: &[ColumnMapping], source_t: &TableSchema, target_t: &TableSchema, target_dialect: &str) -> (Vec<String>, Vec<String>) {
    let mut pairs: Vec<(usize, String, String)> = Vec::new();
    for (m, tc) in mappings.iter().zip(&target_t.columns) {
        if m.column.is_empty() || generated(tc, target_dialect) {
            continue;
        }
        let Some(pos) = source_t.columns.iter().position(|c| c.name == m.column) else { continue };
        pairs.push((pos, m.column.clone(), tc.name.clone()));
    }
    pairs.sort_by_key(|p| p.0);
    pairs.into_iter().map(|(_, s, t)| (s, t)).unzip()
}

/// Catalog row estimates of the source's tables (engines that keep them), by (schema, name).
async fn row_estimates(s: &mut Box<dyn Session>, dialect: &str) -> HashMap<(String, String), u64> {
    let sql = match dialect {
        "mssql" => {
            "SELECT s.name, t.name, SUM(p.rows) FROM sys.tables t JOIN sys.schemas s ON s.schema_id = t.schema_id \
             JOIN sys.partitions p ON p.object_id = t.object_id AND p.index_id IN (0, 1) GROUP BY s.name, t.name"
        }
        "postgres" => {
            "SELECT n.nspname, c.relname, c.reltuples::bigint FROM pg_class c JOIN pg_namespace n ON n.oid = c.relnamespace \
             WHERE c.relkind IN ('r', 'p')"
        }
        "mysql" => "SELECT table_schema, table_name, table_rows FROM information_schema.tables WHERE table_schema = DATABASE()",
        _ => return HashMap::new(),
    };
    let mut out = QueryOutcome::default();
    if s.execute(sql, usize::MAX, &mut out).await.is_err() || out.error.is_some() {
        return HashMap::new();
    }
    let text = |v: &Value| v.as_str().map(str::to_string).unwrap_or_default();
    let num = |v: &Value| v.as_i64().or_else(|| v.as_str().and_then(|s| s.trim().parse::<f64>().ok()).map(|f| f as i64));
    out.results
        .first()
        .map(|r| {
            r.rows
                .iter()
                .filter(|row| row.len() >= 3)
                .filter_map(|row| num(&row[2]).filter(|n| *n >= 0).map(|n| ((text(&row[0]), text(&row[1])), n as u64)))
                .collect()
        })
        .unwrap_or_default()
}

fn estimate_of(est: &HashMap<(String, String), u64>, t: &TableSchema) -> Option<u64> {
    if let Some(n) = est.get(&(t.schema.clone().unwrap_or_default(), t.name.clone())) {
        return Some(*n);
    }
    // Engines whose tables carry no schema (MySQL): by name, when it's unique.
    let mut by_name = est.iter().filter(|((_, n), _)| *n == t.name);
    match (by_name.next(), by_name.next()) {
        (Some((_, n)), None) => Some(*n),
        _ => None,
    }
}

fn bracket_literal(schema: Option<&str>, name: &str) -> String {
    qualified_name(Quote::Bracket, schema, name).replace('\'', "''")
}

/// The way a table's rows are expected to travel between these drivers.
fn copy_path(source: &dyn Driver, target: &dyn Driver) -> &'static str {
    let (s, t) = (source.info().id, target.info().id);
    if same_package(s, t) && source.supports_native_copy(t) {
        "native"
    } else if target.supports_bulk_load() {
        "bulk_load"
    } else {
        "insert_script"
    }
}

/// Row estimates, from the source's catalog (a read), for engines that keep them.
async fn source_estimates(state: &AppState, id: &str, plan: &PlanArgs, dialect: &str) -> HashMap<(String, String), u64> {
    if !matches!(dialect, "mssql" | "postgres" | "mysql") {
        return HashMap::new();
    }
    let skey = format!("migrate:{id}:est");
    let mut out = HashMap::new();
    if let Ok(src) = state.dedicated_session(&skey, &plan.connection_id, &plan.database, true).await {
        out = row_estimates(&mut *src.session.lock().await, dialect).await;
    }
    state.sessions.remove(&skey);
    out
}

/// The source columns a copy reads and writes: all but those the engine fills itself.
fn loaded_columns(t: &TableSchema, dialect: &str) -> Vec<String> {
    t.columns.iter().filter(|c| !generated(c, dialect)).map(|c| c.name.clone()).collect()
}

fn transfer_columns(cols: Vec<dbine_driver::ColumnInfo>) -> Vec<TransferColumn> {
    cols.into_iter().map(|c| TransferColumn { name: c.name, type_name: c.data_type, nullable: c.nullable }).collect()
}

/// A statement's first line, short (for the list of those that failed).
fn short_sql(sql: &str) -> String {
    let line = sql.trim().lines().next().unwrap_or("");
    match line.char_indices().nth(120) {
        Some((i, _)) => format!("{}…", &line[..i]),
        None => line.to_string(),
    }
}

/// What a failed clone "before" statement is, for the notes: its first CREATE / ALTER / EXEC
/// line (the guard `IF NOT EXISTS …` says little), else its first line.
fn before_label(sql: &str) -> String {
    let key = sql.lines().map(str::trim).find(|l| {
        let u = l.get(..6).unwrap_or("").to_ascii_uppercase();
        u.starts_with("CREATE") || u.starts_with("ALTER ") || u.starts_with("EXEC")
    });
    short_sql(key.unwrap_or(sql).trim_end_matches(';'))
}

/// The names an error quotes, in '…' or […]: the whole name and, when qualified, its last part.
fn quoted_names(error: &str) -> Vec<String> {
    let mut out = Vec::new();
    for (open, close) in [('\'', '\''), ('[', ']')] {
        let mut rest = error;
        while let Some(i) = rest.find(open) {
            let after = &rest[i + open.len_utf8()..];
            let Some(j) = after.find(close) else { break };
            let name = after[..j].trim();
            if !name.is_empty() && name.len() <= 128 && !name.contains(char::is_whitespace) {
                out.push(name.to_string());
                if let Some((_, last)) = name.rsplit_once('.') {
                    let last = last.trim_matches(|c| c == '[' || c == ']' || c == '"');
                    if !last.is_empty() {
                        out.push(last.to_string());
                    }
                }
            }
            rest = &after[j + close.len_utf8()..];
        }
    }
    out
}

/// Whether `sql` names `name` as a whole identifier (case-insensitive).
fn mentions(sql: &str, name: &str) -> bool {
    let (sql, name) = (sql.to_lowercase(), name.to_lowercase());
    let ident = |c: char| c.is_alphanumeric() || c == '_' || c == '@' || c == '#' || c == '$';
    let mut from = 0;
    while let Some(i) = sql[from..].find(&name) {
        let (s, e) = (from + i, from + i + name.len());
        let before = sql[..s].chars().next_back().is_none_or(|c| !ident(c));
        let after = sql[e..].chars().next().is_none_or(|c| !ident(c));
        if before && after {
            return true;
        }
        from = e;
    }
    false
}

/// A table's creation error explained by an earlier failed "before" statement: when the error
/// quotes a name (other than the table's) that a failed statement creates or uses, the cause
/// is that statement's error.
fn before_cause(error: &str, table: &str, failed: &[(String, String)]) -> Option<String> {
    quoted_names(error).into_iter().filter(|n| !n.eq_ignore_ascii_case(table)).find_map(|n| {
        failed.iter().find(|(sql, _)| mentions(sql, &n)).map(|(_, e)| format!("no se pudo crear {n}: {e}"))
    })
}

// -- stage 1: structure ---------------------------------------------------------------------------

/// Schemas, DROP and CREATE (columns and primary key; indexes go with each table's copy). Fills
/// the record's tables, foreign keys and reseeds, and returns the jobs.
async fn structure(state: &AppState, p: &Prepared, args: &RunArgs, meta: &mut RunMeta, live: &LiveRun, emit: &Emit) -> CommandResult<Vec<TransferJob>> {
    let id = meta.id.clone();
    let o = &args.plan.options;
    let (sinfo, tinfo) = (p.source.info(), p.target.info());
    let names: Vec<(String, String)> = p
        .chosen
        .iter()
        .zip(p.conv.tables.iter())
        .map(|(s, t)| (qualified(s.schema.as_deref(), &s.name), qualified(t.schema.as_deref(), &t.name)))
        .collect();
    let path = copy_path(p.source.as_ref(), p.target.as_ref());
    meta.tables = names
        .iter()
        .map(|(s, t)| MetaTable { name: s.clone(), source: s.clone(), target: t.clone(), path: path.into(), created: false, error: None })
        .collect();

    let estimates = if o.data { source_estimates(state, &id, &args.plan, sinfo.dialect).await } else { HashMap::new() };

    let tkey = format!("migrate:{id}:ddl");
    let tgt = state.dedicated_session(&tkey, &args.target_connection_id, &args.target_database, false).await?;
    let r = async {
        let mut s = tgt.session.lock().await;
        let cancelled = || live.is_cancelled();
        // Schemas (one that already exists or can't be created: the CREATE TABLE says so).
        for (k, sql) in p.ddl.schemas.iter().enumerate() {
            step(emit, &id, "schemas", "", k, p.ddl.schemas.len());
            let _ = exec(&mut s, sql).await;
        }
        // DROP, in passes: a table others depend on goes once they're gone. A table that still
        // can't be dropped stops the migration (its CREATE would fail anyway).
        let mut pending: Vec<&(usize, String)> = p.ddl.drops.iter().collect();
        let mut done = 0;
        while !pending.is_empty() {
            let mut failed = Vec::new();
            let mut errors = Vec::new();
            for item in pending.iter() {
                if cancelled() {
                    return Err(CommandError::Cancelled);
                }
                let (i, sql) = *item;
                step(emit, &id, "drop", &names[*i].1, done, p.ddl.drops.len());
                match exec(&mut s, sql).await {
                    Ok(()) => done += 1,
                    Err(e) => {
                        failed.push(*item);
                        errors.push(format!("{}: {e}", names[*i].1));
                    }
                }
            }
            if failed.len() == pending.len() {
                return Err(CommandError::Sql(format!("no se pudieron borrar en el destino:\n{}", errors.join("\n"))));
            }
            pending = failed;
        }
        // CREATE each table (columns and primary key). A table that fails is reported and
        // skipped (no data, no FKs); the others go on.
        let mut jobs = Vec::new();
        let per_table_columns = p.conv.columns.len() == p.conv.tables.iter().map(|t| t.columns.len()).sum::<usize>();
        let mut offset = 0;
        for (k, (i, sql)) in p.ddl.creates.iter().enumerate() {
            let (source_t, target_t) = (&p.chosen[*i], &p.conv.tables[*i]);
            let mappings: Vec<ColumnMapping> = if per_table_columns {
                p.conv.columns[offset..offset + target_t.columns.len()].to_vec()
            } else {
                // Unexpected shape: pair by the table's name.
                p.conv.columns.iter().filter(|c| c.table == source_t.name).cloned().collect()
            };
            offset += target_t.columns.len();
            if cancelled() {
                return Err(CommandError::Cancelled);
            }
            step(emit, &id, "create", &names[*i].1, k, p.ddl.creates.len());
            let target_ref = ObjectRef { kind: kinds::TABLE.into(), schema: target_t.schema.clone(), name: target_t.name.clone() };
            let existed = !o.drop && s.columns(&target_ref).await.map(|c| !c.is_empty()).unwrap_or(false);
            if let Err(e) = exec(&mut s, sql).await {
                meta.tables[*i].error = Some(format!("al crear la tabla: {e}"));
                continue;
            }
            meta.tables[*i].created = true;
            if !o.data {
                if let Some((_, ix)) = p.ddl.indexes.iter().find(|(j, _)| j == i) {
                    if let Err(e) = exec(&mut s, ix).await {
                        meta.tables[*i].error = Some(format!("índices: {e}"));
                    }
                }
                continue;
            }
            // The structure just created (or found): checked again before each copy.
            let expected: Vec<TransferColumn> = s
                .columns(&target_ref)
                .await
                .unwrap_or_default()
                .into_iter()
                .map(|c| TransferColumn { name: c.name, type_name: c.data_type, nullable: c.nullable })
                .collect();
            let (src_cols, tgt_cols) = column_pairs(&mappings, source_t, target_t, tinfo.dialect);
            if src_cols.len() < source_t.columns.len() {
                let left: Vec<&str> =
                    source_t.columns.iter().map(|c| c.name.as_str()).filter(|c| !src_cols.iter().any(|x| x == c)).collect();
                meta.notes.push(format!("{}: no se copian las columnas {}", names[*i].0, left.join(", ")));
            }
            let truncate = truncate_sql(p.target.as_ref(), target_t);
            if truncate.is_none() {
                meta.notes.push(format!("{}: el destino no tiene cómo vaciarla; si la copia se corta, hay que vaciarla a mano antes de reanudar", names[*i].1));
            }
            // Indexes right after the copy, idempotent (they run again after a cut).
            let post = if o.indexes && !target_t.indexes.is_empty() {
                p.target
                    .table_ddl(target_t, DdlParts { indexes: true, if_exists: true, ..Default::default() })
                    .ok()
                    .filter(|s| !s.trim().is_empty())
                    .into_iter()
                    .collect()
            } else {
                Vec::new()
            };
            let (before, after) = p.target.data_load_wrap(target_t);
            jobs.push(TransferJob {
                name: names[*i].0.clone(),
                source: ReadSpec {
                    table: ObjectRef { kind: kinds::TABLE.into(), schema: source_t.schema.clone(), name: source_t.name.clone() },
                    columns: Some(src_cols),
                    filter: None,
                },
                target: LoadSpec {
                    table: target_ref,
                    columns: tgt_cols,
                    table_lock: true,
                    keep_identity: true,
                    commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                    commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
                },
                row_estimate: estimate_of(&estimates, source_t),
                truncate,
                empty_first: false,
                before,
                after,
                post,
                preexisting: existed,
                expected_columns: expected,
                mode: TransferMode::Copy,
            });
            if sinfo.dialect == "mssql" && tinfo.dialect == "mssql" && source_t.columns.iter().any(|c| c.auto_increment) {
                meta.reseeds.push(Reseed {
                    table: names[*i].0.clone(),
                    source_sql: format!(
                        "SELECT CAST(IDENT_CURRENT(N'{}') AS bigint)",
                        bracket_literal(source_t.schema.as_deref(), &source_t.name)
                    ),
                    target: bracket_literal(target_t.schema.as_deref(), &target_t.name),
                    done: false,
                });
            }
        }
        meta.fks = p
            .ddl
            .fks
            .iter()
            .filter(|(i, _)| meta.tables[*i].created)
            .map(|(i, sql)| FkStep { table: names[*i].0.clone(), label: names[*i].1.clone(), sql: sql.clone(), done: false })
            .collect();
        Ok(jobs)
    }
    .await;
    state.sessions.remove(&tkey);
    r
}

/// Clone: the driver's script. `before`, then each table's `create` and what goes right before
/// its data (a native copy doesn't run a job's `before`, so it runs here, on the table just
/// created); one job per table, its `after_data` (indexes, identity) as the job's `post`. The
/// script's `after` goes to the record, for the last stage. Its notes go to the run's log.
async fn clone_structure(state: &AppState, p: &Picked, args: &RunArgs, meta: &mut RunMeta, live: &LiveRun, emit: &Emit) -> CommandResult<Vec<TransferJob>> {
    let id = meta.id.clone();
    let o = &args.plan.options;
    let sdialect = p.source.info().dialect;
    let path = copy_path(p.source.as_ref(), p.target.as_ref());
    let estimates = if o.data { source_estimates(state, &id, &args.plan, sdialect).await } else { HashMap::new() };

    let tkey = format!("migrate:{id}:ddl");
    let tgt = state.dedicated_session(&tkey, &args.target_connection_id, &args.target_database, false).await?;
    let r = async {
        let mut s = tgt.session.lock().await;
        let cancelled = || live.is_cancelled();
        step(emit, &id, "script", "", 0, 0);
        let script = read_clone_script(state, p, &args.plan, &format!("migrate:{id}:clone"), &mut s).await?;
        meta.tables = script
            .tables
            .iter()
            .map(|t| {
                let name = qualified(t.table.schema.as_deref(), &t.table.name);
                MetaTable { name: name.clone(), source: name.clone(), target: name, path: path.into(), created: false, error: None }
            })
            .collect();
        for n in &script.notes {
            meta.notes.push(n.clone());
            emit(tagged(&id, &Event::Log { level: LogLevel::Warn, text: n.clone() }));
        }
        // Before any table (schemas, types…). One that fails is noted: the tables that need it
        // fail to create and say so (with this failure as the cause, when their error names it).
        let mut failed_before: Vec<(String, String)> = Vec::new();
        for (k, sql) in script.before.iter().enumerate() {
            if cancelled() {
                return Err(CommandError::Cancelled);
            }
            step(emit, &id, "before", "", k, script.before.len());
            if let Err(e) = exec(&mut s, sql).await {
                meta.notes.push(format!("falló antes de las tablas: {}: {e}", before_label(sql)));
                failed_before.push((sql.clone(), e));
            }
        }
        let mut jobs = Vec::new();
        for (k, ct) in script.tables.iter().enumerate() {
            if cancelled() {
                return Err(CommandError::Cancelled);
            }
            let name = meta.tables[k].name.clone();
            step(emit, &id, "create", &name, k, script.tables.len());
            let table = ObjectRef { kind: kinds::TABLE.into(), schema: ct.table.schema.clone(), name: ct.table.name.clone() };
            let existed = s.columns(&table).await.map(|c| !c.is_empty()).unwrap_or(false);
            if let Err(e) = exec(&mut s, &ct.create).await {
                meta.tables[k].error = Some(match before_cause(&e, &ct.table.name, &failed_before) {
                    Some(cause) => format!("al crear la tabla: {e} — causa: {cause}"),
                    None => format!("al crear la tabla: {e}"),
                });
                continue;
            }
            let mut failed = None;
            for sql in &ct.before_data {
                if let Err(e) = exec(&mut s, sql).await {
                    failed = Some(format!("antes de los datos: {e}"));
                    break;
                }
            }
            if failed.is_some() {
                meta.tables[k].error = failed;
                continue;
            }
            meta.tables[k].created = true;
            if !o.data {
                for sql in &ct.after_data {
                    if let Err(e) = exec(&mut s, sql).await {
                        meta.tables[k].error = Some(format!("índices: {e}"));
                        break;
                    }
                }
                continue;
            }
            let expected = transfer_columns(s.columns(&table).await.unwrap_or_default());
            let source_t = p.chosen.iter().find(|t| t.name == ct.table.name && t.schema == ct.table.schema);
            let columns = match source_t {
                Some(t) => loaded_columns(t, sdialect),
                None => expected.iter().map(|c| c.name.clone()).collect(),
            };
            let as_schema = TableSchema { schema: ct.table.schema.clone(), name: ct.table.name.clone(), ..Default::default() };
            let truncate = truncate_sql(p.target.as_ref(), &as_schema);
            jobs.push(TransferJob {
                name,
                source: ReadSpec { table: table.clone(), columns: Some(columns.clone()), filter: None },
                target: LoadSpec {
                    table,
                    columns,
                    table_lock: true,
                    keep_identity: true,
                    commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                    commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
                },
                row_estimate: source_t.and_then(|t| estimate_of(&estimates, t)),
                truncate,
                empty_first: false,
                before: String::new(),
                after: String::new(),
                post: ct.after_data.clone(),
                preexisting: existed,
                expected_columns: expected,
                mode: TransferMode::Copy,
            });
        }
        meta.after = script.after.iter().map(|sql| AfterStep { sql: sql.clone(), done: false }).collect();
        Ok(jobs)
    }
    .await;
    state.sessions.remove(&tkey);
    r
}

/// Sync: no DDL. Each table must exist on the target with the source's columns (checked again
/// before each sync) and have a key; the others are listed with the reason.
async fn sync_structure(state: &AppState, p: &Picked, args: &RunArgs, meta: &mut RunMeta, live: &LiveRun, emit: &Emit) -> CommandResult<Vec<TransferJob>> {
    let id = meta.id.clone();
    let sdialect = p.source.info().dialect;
    let sync = &args.plan.sync;
    meta.tables = p
        .chosen
        .iter()
        .map(|t| {
            let name = qualified(t.schema.as_deref(), &t.name);
            MetaTable { name: name.clone(), source: name.clone(), target: name, path: "delta".into(), created: false, error: None }
        })
        .collect();
    let estimates = source_estimates(state, &id, &args.plan, sdialect).await;

    // Both sides are only read here.
    let (skey, tkey) = (format!("migrate:{id}:cols-src"), format!("migrate:{id}:cols-tgt"));
    let r = async {
        let src = state.dedicated_session(&skey, &args.plan.connection_id, &args.plan.database, true).await?;
        let tgt = state.dedicated_session(&tkey, &args.target_connection_id, &args.target_database, true).await?;
        let (mut src, mut tgt) = (src.session.lock().await, tgt.session.lock().await);
        let mut jobs = Vec::new();
        for (i, t) in p.chosen.iter().enumerate() {
            if live.is_cancelled() {
                return Err(CommandError::Cancelled);
            }
            let name = meta.tables[i].name.clone();
            step(emit, &id, "check", &name, i, p.chosen.len());
            let key = match sync_key(t, sync) {
                Ok(k) => k,
                Err(why) => {
                    meta.tables[i].error = Some(why);
                    continue;
                }
            };
            let table = ObjectRef { kind: kinds::TABLE.into(), schema: t.schema.clone(), name: t.name.clone() };
            let target_cols = tgt.columns(&table).await.unwrap_or_default();
            if target_cols.is_empty() {
                meta.tables[i].error = Some("no existe en el destino: sincronizar no crea tablas".into());
                continue;
            }
            meta.tables[i].created = true;
            // The source's own columns, as the target's catalog spells them: the check compares
            // names and types.
            let expected = match src.columns(&table).await {
                Ok(c) if !c.is_empty() => transfer_columns(c),
                _ => transfer_columns(target_cols),
            };
            let columns = loaded_columns(t, sdialect);
            jobs.push(TransferJob {
                name,
                source: ReadSpec { table: table.clone(), columns: Some(columns.clone()), filter: None },
                target: LoadSpec {
                    table,
                    columns,
                    table_lock: false,
                    keep_identity: true,
                    commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
                    commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
                },
                row_estimate: estimate_of(&estimates, t),
                truncate: None,
                empty_first: false,
                before: String::new(),
                after: String::new(),
                post: Vec::new(),
                preexisting: true,
                expected_columns: expected,
                mode: TransferMode::Delta { key, depth: sync.depth, max_cores: sync.max_cores },
            });
        }
        Ok(jobs)
    }
    .await;
    state.sessions.remove(&skey);
    state.sessions.remove(&tkey);
    r
}

// -- stages 2 and 3 -------------------------------------------------------------------------------

enum Mode {
    Run(Vec<TransferJob>),
    Resume,
    Retry,
}

/// The data (engine), then the constraints; saves the record as it goes.
async fn transfer_and_finish(
    state: &AppState,
    runs: &MigrationRuns,
    meta: &mut RunMeta,
    mode: Mode,
    live: Arc<LiveRun>,
    emit: &Emit,
    started: Instant,
) -> CommandResult<RunResult> {
    let id = meta.id.clone();
    meta.stage = "data".into();
    meta.status = "running".into();
    runs.save(meta);
    emit(json!({ "id": id, "event": "plan", "tables": runs.tables(meta), "parallel": meta.options.parallel }));

    let endpoints: Arc<dyn Endpoints> = Arc::new(MigrationEndpoints::new(state, meta)?);
    let engine = Engine::new(runs.store.clone(), id.clone());
    live.set_control(Some(engine.control()));
    let (em, lv, rid) = (emit.clone(), live.clone(), id.clone());
    let events = move |e: Event| {
        // A cancel that came before the engine started.
        if matches!(e, Event::RunStarted { .. }) && lv.is_cancelled() {
            if let Some(c) = lv.control() {
                c.cancel_all();
            }
        }
        em(tagged(&rid, &e));
    };
    let report = match mode {
        Mode::Run(jobs) => engine.run(jobs, meta.options.clone(), endpoints, events).await,
        Mode::Resume => engine.resume(None, endpoints, events).await,
        Mode::Retry => engine.retry_failed(None, endpoints, events).await,
    };
    live.set_control(None);
    let report = match report {
        Ok(r) => r,
        Err(e) => {
            meta.status = "failed".into();
            meta.elapsed_ms += started.elapsed().as_millis() as u64;
            runs.save(meta);
            step(emit, &id, "done", "", 0, 0);
            return Err(e.into());
        }
    };

    let mut cancelled = live.is_cancelled() || report.summary.status == RunStatus::Cancelled;
    if !cancelled {
        meta.stage = "constraints".into();
        runs.save(meta);
        cancelled = !constraints(state, runs, meta, &live, emit).await;
        if !cancelled {
            meta.stage = "finished".into();
        }
    }
    let tables = runs.tables(meta);
    let failed = tables.iter().any(|t| matches!(t.status.as_str(), "failed" | "cancelled" | "pending" | "running" | "copied"))
        || !meta.foreign_key_errors.is_empty()
        || !meta.after_errors.is_empty();
    meta.status = if cancelled {
        "cancelled"
    } else if failed {
        "failed"
    } else {
        "done"
    }
    .into();
    meta.finished_at = Some(now());
    meta.elapsed_ms += started.elapsed().as_millis() as u64;
    runs.save(meta);
    step(emit, &id, "done", "", 0, 0);
    Ok(runs.result(meta))
}

/// Foreign keys of the copied tables, then the identities. `false`: cancelled.
async fn constraints(state: &AppState, runs: &MigrationRuns, meta: &mut RunMeta, live: &LiveRun, emit: &Emit) -> bool {
    let id = meta.id.clone();
    let done: HashSet<String> = runs.tables(meta).into_iter().filter(|t| t.status == "done").map(|t| t.name).collect();
    let ready = |table: &str| done.contains(table) || (!meta.plan.options.data && meta.tables.iter().any(|t| t.name == table && t.created));
    let fks: Vec<usize> = (0..meta.fks.len()).filter(|k| !meta.fks[*k].done && ready(&meta.fks[*k].table)).collect();
    let reseeds: Vec<usize> = (0..meta.reseeds.len()).filter(|k| !meta.reseeds[*k].done && done.contains(&meta.reseeds[*k].table)).collect();
    let afters: Vec<usize> = (0..meta.after.len()).filter(|k| !meta.after[*k].done).collect();
    meta.foreign_key_errors.clear();
    meta.after_errors.clear();
    if fks.is_empty() && reseeds.is_empty() && afters.is_empty() {
        return true;
    }
    let tkey = format!("migrate:{id}:fk");
    let tgt = match state.dedicated_session(&tkey, &meta.target_connection_id, &meta.target_database, false).await {
        Ok(t) => t,
        Err(e) => {
            meta.foreign_key_errors.push(format!("no se pudo conectar al destino: {e}"));
            return true;
        }
    };
    let mut ok = true;
    {
        let mut s = tgt.session.lock().await;
        for (n, k) in fks.iter().enumerate() {
            if live.is_cancelled() {
                ok = false;
                break;
            }
            step(emit, &id, "foreign_keys", &meta.fks[*k].label, n, fks.len());
            match exec(&mut s, &meta.fks[*k].sql).await {
                Ok(()) => meta.fks[*k].done = true,
                Err(e) => meta.foreign_key_errors.push(format!("{}: {e}", meta.fks[*k].label)),
            }
            runs.save(meta);
        }
        if ok && !reseeds.is_empty() {
            let skey = format!("migrate:{id}:ident");
            match state.dedicated_session(&skey, &meta.plan.connection_id, &meta.plan.database, true).await {
                Ok(src) => {
                    let mut src = src.session.lock().await;
                    for (n, k) in reseeds.iter().enumerate() {
                        if live.is_cancelled() {
                            ok = false;
                            break;
                        }
                        let r = meta.reseeds[*k].clone();
                        step(emit, &id, "identity", &r.table, n, reseeds.len());
                        let mut out = QueryOutcome::default();
                        let current = match src.execute(&r.source_sql, 1, &mut out).await {
                            Ok(()) if out.error.is_none() => out
                                .results
                                .first()
                                .and_then(|x| x.rows.first())
                                .and_then(|row| row.first())
                                .and_then(|v| v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok()))),
                            _ => None,
                        };
                        match current {
                            Some(n) => match exec(&mut s, &format!("DBCC CHECKIDENT (N'{}', RESEED, {n}) WITH NO_INFOMSGS", r.target)).await {
                                Ok(()) => meta.notes.push(format!("{}: identidad del destino en {n}, como en el origen", r.table)),
                                Err(e) => meta.notes.push(format!("{}: no se pudo ajustar la identidad: {e}", r.table)),
                            },
                            None => meta.notes.push(format!("{}: no se pudo leer la identidad del origen", r.table)),
                        }
                        meta.reseeds[*k].done = true;
                        runs.save(meta);
                    }
                }
                Err(e) => meta.notes.push(format!("no se pudo leer la identidad del origen: {e}")),
            }
            state.sessions.remove(&skey);
        }
        if ok && !afters.is_empty() {
            ok = after_passes(runs, meta, afters, &mut s, live, emit).await;
        }
    }
    state.sessions.remove(&tkey);
    ok
}

/// Passes over a clone's final statements: one that fails (it depends on another that hasn't
/// run yet) is tried again in the next pass, while a pass gets any through, up to this many.
const AFTER_PASSES: usize = 4;

/// Run the clone's `after` statements in passes; those still failing go to `after_errors`.
/// `false`: cancelled.
async fn after_passes(runs: &MigrationRuns, meta: &mut RunMeta, mut pending: Vec<usize>, s: &mut Box<dyn Session>, live: &LiveRun, emit: &Emit) -> bool {
    let id = meta.id.clone();
    let mut errors: Vec<String> = Vec::new();
    for _ in 0..AFTER_PASSES {
        let mut failed = Vec::new();
        errors.clear();
        for (n, k) in pending.iter().enumerate() {
            if live.is_cancelled() {
                return false;
            }
            step(emit, &id, "after", "", n, pending.len());
            match exec(s, &meta.after[*k].sql).await {
                Ok(()) => meta.after[*k].done = true,
                Err(e) => {
                    failed.push(*k);
                    errors.push(format!("{}: {e}", short_sql(&meta.after[*k].sql)));
                }
            }
        }
        runs.save(meta);
        let stuck = failed.len() == pending.len();
        pending = failed;
        if pending.is_empty() || stuck {
            break;
        }
    }
    meta.after_errors = errors;
    true
}

/// What the structure stage starts from, by mode.
enum Prep {
    Convert(Prepared),
    /// Clone and sync: the same engine on both sides, no conversion.
    Same(Picked),
}

/// A whole run: validate, structure, data, constraints.
async fn run_migration(state: &AppState, runs: &MigrationRuns, args: RunArgs, emit: Emit) -> CommandResult<RunResult> {
    let started = Instant::now();
    valid_id(&args.migration_id)?;
    let target_cfg = state
        .store
        .get_connection(&args.target_connection_id)?
        .ok_or_else(|| CommandError::NotFound("la conexión de destino no existe".into()))?;
    if target_cfg.config.driver != args.plan.target_driver {
        return Err(CommandError::BadRequest("la conexión de destino no es del motor elegido".into()));
    }
    if target_cfg.config.read_only {
        return Err(CommandError::BadRequest(format!("«{}» es de solo lectura: no se puede migrar hacia ella", target_cfg.name)));
    }
    let id = args.migration_id.clone();
    let (live, _guard) = runs.begin(&id)?;
    let p = match args.plan.mode {
        MigrationMode::Convert => Prep::Convert(prepare(state, &args.plan).await?),
        mode => {
            let p = pick(state, &args.plan).await?;
            check_mode(mode, p.source.as_ref(), p.target.as_ref())?;
            Prep::Same(p)
        }
    };
    let mut plan = args.plan.clone();
    // A sync always moves rows.
    plan.options.data |= plan.mode == MigrationMode::Sync;
    plan.target = None;
    let mut meta = RunMeta {
        id: id.clone(),
        created_at: now(),
        finished_at: None,
        status: "running".into(),
        stage: "structure".into(),
        plan,
        target_connection_id: args.target_connection_id.clone(),
        target_database: args.target_database.clone(),
        options: args.transfer.run_options(),
        tables: Vec::new(),
        fks: Vec::new(),
        reseeds: Vec::new(),
        foreign_key_errors: Vec::new(),
        after: Vec::new(),
        after_errors: Vec::new(),
        notes: Vec::new(),
        elapsed_ms: 0,
    };
    runs.save(&meta);
    let r = match &p {
        Prep::Convert(p) => structure(state, p, &args, &mut meta, &live, &emit).await,
        Prep::Same(p) if args.plan.mode == MigrationMode::Clone => clone_structure(state, p, &args, &mut meta, &live, &emit).await,
        Prep::Same(p) => sync_structure(state, p, &args, &mut meta, &live, &emit).await,
    };
    let jobs = match r {
        Ok(j) => j,
        Err(e) => {
            meta.status = if matches!(e, CommandError::Cancelled) { "cancelled" } else { "failed" }.into();
            meta.finished_at = Some(now());
            meta.elapsed_ms = started.elapsed().as_millis() as u64;
            if !matches!(e, CommandError::Cancelled) {
                meta.notes.push(e.to_string());
            }
            runs.save(&meta);
            step(&emit, &id, "done", "", 0, 0);
            return match e {
                CommandError::Cancelled => Ok(runs.result(&meta)),
                e => Err(e),
            };
        }
    };
    transfer_and_finish(state, runs, &mut meta, Mode::Run(jobs), live, &emit, started).await
}

/// Resume (or retry the failed tables of) a run: the engine goes on with what isn't done; a run
/// cut before its tables were created starts again.
async fn go_on(state: &AppState, runs: &MigrationRuns, run_id: &str, retry: bool, emit: Emit) -> CommandResult<RunResult> {
    let started = Instant::now();
    let mut meta = runs.load(run_id)?;
    // Ask for the passwords now, not once per table.
    state.resolve_config(&meta.plan.connection_id)?;
    state.resolve_config(&meta.target_connection_id)?;
    if runs.store.run_spec(run_id)?.is_none() {
        // Cut while creating the tables: nothing was copied, so it runs again.
        let args = RunArgs {
            migration_id: meta.id.clone(),
            plan: meta.plan.clone(),
            target_connection_id: meta.target_connection_id.clone(),
            target_database: meta.target_database.clone(),
            transfer: TransferOptions {
                parallel: Some(meta.options.parallel),
                order: Some(meta.options.order),
                commit_rows: Some(meta.options.commit_rows),
            },
        };
        return run_migration(state, runs, args, emit).await;
    }
    let (live, _guard) = runs.begin(run_id)?;
    transfer_and_finish(state, runs, &mut meta, if retry { Mode::Retry } else { Mode::Resume }, live, &emit, started).await
}

fn emitter(app: &AppHandle) -> Emit {
    let app = app.clone();
    Arc::new(move |v| {
        let _ = app.emit("migration-progress", v);
    })
}

/// Run the migration against the target connection: schemas, DROP (if asked) and CREATE, the
/// data with the bulk transfer engine (each table's indexes as soon as its copy ends), then the
/// foreign keys. Progress: `migration-progress` events.
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_run(app: AppHandle, state: State<'_, AppState>, runs: State<'_, MigrationRuns>, args: RunArgs) -> CommandResult<RunResult> {
    run_migration(&state, &runs, args, emitter(&app)).await
}

#[derive(Deserialize)]
pub struct RunIdArgs {
    pub run_id: String,
}

#[derive(Deserialize)]
pub struct ParallelArgs {
    pub run_id: String,
    pub n: usize,
}

#[derive(Deserialize)]
pub struct RunTableArgs {
    pub run_id: String,
    pub table: String,
}

#[derive(Deserialize, Default)]
pub struct RunsArgs {
    #[serde(default)]
    pub limit: Option<usize>,
    /// Only these runs (a saved migration's), whatever their age.
    #[serde(default)]
    pub ids: Option<Vec<String>>,
}

/// Tables copying at once, live (1 to 32).
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_set_parallel(runs: State<'_, MigrationRuns>, args: ParallelArgs) -> CommandResult<usize> {
    let c = runs.control(&args.run_id)?;
    c.set_parallel(args.n.clamp(1, dbine_transfer::MAX_PARALLEL));
    Ok(c.parallel())
}

/// Cancel one table (running: stops and its rows are removed; queued: leaves the queue).
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_cancel_table(runs: State<'_, MigrationRuns>, args: RunTableArgs) -> CommandResult<bool> {
    Ok(runs.control(&args.run_id)?.cancel_table(&args.table))
}

/// Start a queued table now, without waiting for a slot.
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_run_now(runs: State<'_, MigrationRuns>, args: RunTableArgs) -> CommandResult<bool> {
    Ok(runs.control(&args.run_id)?.run_now(&args.table))
}

/// Cancel the whole run: running tables stop, queued ones stay pending (resume picks them up).
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_cancel(runs: State<'_, MigrationRuns>, args: RunIdArgs) -> CommandResult<()> {
    let run = runs.running(&args.run_id)?;
    run.cancelled.store(true, Ordering::SeqCst);
    if let Some(c) = run.control() {
        c.cancel_all();
    }
    Ok(())
}

/// Recent runs, newest first, with their tables.
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_runs(runs: State<'_, MigrationRuns>, args: Option<RunsArgs>) -> CommandResult<Vec<RunInfo>> {
    let args = args.unwrap_or_default();
    let limit = args.limit.unwrap_or(30);
    Ok(list_runs(&runs, limit, args.ids.as_deref()))
}

fn list_runs(runs: &MigrationRuns, limit: usize, ids: Option<&[String]>) -> Vec<RunInfo> {
    let live: HashSet<String> = runs.live().keys().cloned().collect();
    runs.records()
        .into_iter()
        .filter(|m| ids.is_none_or(|ids| ids.contains(&m.id)))
        .take(if ids.is_some() { usize::MAX } else { limit })
        .map(|m| {
            let status = if live.contains(&m.id) { "running".to_string() } else { m.status.clone() };
            RunInfo {
                resumable: status != "running" && status != "done",
                tables: runs.tables(&m),
                status,
                stage: m.stage.clone(),
                created_at: m.created_at.clone(),
                finished_at: m.finished_at.clone(),
                source_connection_id: m.plan.connection_id.clone(),
                source_database: m.plan.database.clone(),
                target_connection_id: m.target_connection_id.clone(),
                target_database: m.target_database.clone(),
                target_driver: m.plan.target_driver.clone(),
                parallel: m.options.parallel,
                foreign_key_errors: m.foreign_key_errors.clone(),
                after_errors: m.after_errors.clone(),
                notes: m.notes.clone(),
                mode: m.plan.mode,
                id: m.id,
            }
        })
        .collect()
}

/// Go on with a run cut by closing the app (or cancelled): what isn't done runs again; a table
/// copied halfway is emptied and copied again, a copied one never.
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_resume(app: AppHandle, state: State<'_, AppState>, runs: State<'_, MigrationRuns>, args: RunIdArgs) -> CommandResult<RunResult> {
    go_on(&state, &runs, &args.run_id, false, emitter(&app)).await
}

/// "Reintentar las que fallaron": failed and cancelled tables go back to the queue.
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_retry_failed(app: AppHandle, state: State<'_, AppState>, runs: State<'_, MigrationRuns>, args: RunIdArgs) -> CommandResult<RunResult> {
    go_on(&state, &runs, &args.run_id, true, emitter(&app)).await
}

/// Forget a run that isn't going on (its record; the target keeps what was copied).
#[tauri::command(rename_all = "camelCase")]
pub async fn migration_forget(runs: State<'_, MigrationRuns>, args: RunIdArgs) -> CommandResult<()> {
    valid_id(&args.run_id)?;
    if runs.live().contains_key(&args.run_id) {
        return Err(CommandError::BadRequest("la migración está corriendo".into()));
    }
    let _ = std::fs::remove_file(runs.path(&args.run_id));
    Ok(())
}

/// Run one statement on a session; the error as text.
async fn exec(s: &mut Box<dyn Session>, sql: &str) -> Result<(), String> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 0, &mut out).await.map_err(|e| e.to_string())?;
    match out.error {
        Some(e) => Err(e),
        None => Ok(()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_core::{SavedConnection, StateStore};
    use dbine_driver::ConnectionConfig;

    fn dir(tag: &str) -> PathBuf {
        let d = std::env::temp_dir().join(format!("dbine-migration-{tag}-{}-{}", std::process::id(), chrono::Utc::now().timestamp_nanos_opt().unwrap_or(0)));
        std::fs::create_dir_all(&d).unwrap();
        d
    }

    fn sqlite(state: &AppState, id: &str, path: &Path) {
        let cfg = ConnectionConfig { driver: "sqlite".into(), host: path.to_string_lossy().into_owned(), ..Default::default() };
        let conn = SavedConnection {
            id: id.into(),
            name: id.into(),
            color: None,
            config: cfg,
            save_password: false,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        };
        state.store.save_connection(&conn).unwrap();
    }

    async fn run_sql(state: &AppState, conn: &str, sql: &str) -> QueryOutcome {
        let cfg = state.resolve_config(conn).unwrap();
        let mut s = dbine_drivers::open_session(&cfg, None).await.unwrap();
        let mut out = QueryOutcome::default();
        s.execute(sql, 100, &mut out).await.unwrap();
        assert!(out.error.is_none(), "{:?}", out.error);
        out
    }

    async fn count(state: &AppState, conn: &str, table: &str) -> i64 {
        let out = run_sql(state, conn, &format!("SELECT COUNT(*) FROM \"{table}\"")).await;
        out.results[0].rows[0][0].as_i64().unwrap()
    }

    fn args(id: &str, drop: bool) -> RunArgs {
        RunArgs {
            migration_id: id.into(),
            plan: PlanArgs {
                connection_id: "src".into(),
                database: String::new(),
                tables: vec![],
                target_driver: "sqlite".into(),
                options: PlanOptions {
                    fold_case: true,
                    target_schema: None,
                    drop,
                    if_exists: true,
                    indexes: true,
                    foreign_keys: true,
                    data: true,
                    keep_schemas: true,
                },
                mode: MigrationMode::Convert,
                sync: SyncOptions::default(),
                target: None,
            },
            target_connection_id: "tgt".into(),
            target_database: String::new(),
            transfer: TransferOptions { parallel: Some(4), order: None, commit_rows: None },
        }
    }

    type Seen = Arc<Mutex<Vec<Value>>>;

    fn collect() -> (Emit, Seen) {
        let seen: Seen = Arc::default();
        let s = seen.clone();
        (Arc::new(move |v| s.lock().unwrap().push(v)), seen)
    }

    async fn setup(tag: &str) -> (AppState, MigrationRuns, PathBuf) {
        let d = dir(tag);
        let state = AppState::new(StateStore::open(&d.join("state.sqlite")).unwrap());
        sqlite(&state, "src", &d.join("src.db"));
        sqlite(&state, "tgt", &d.join("tgt.db"));
        let mut rows = String::new();
        for i in 1..=3000 {
            rows.push_str(&format!("INSERT INTO child (id, parent_id, data, note) VALUES ({i}, {}, x'{}', 'n{i}');\n", i % 20 + 1, "AB".repeat(i % 700)));
        }
        let parents: String = (1..=20).map(|i| format!("INSERT INTO parent (id, name) VALUES ({i}, 'p{i}');\n")).collect();
        run_sql(
            &state,
            "src",
            &format!(
                "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT NOT NULL);
                 CREATE TABLE child (id INTEGER PRIMARY KEY, parent_id INTEGER NOT NULL REFERENCES parent(id), data BLOB, note TEXT);
                 CREATE INDEX ix_child_parent ON child (parent_id);
                 CREATE TABLE empty_one (k TEXT PRIMARY KEY);
                 BEGIN; {parents}{rows}COMMIT;"
            ),
        )
        .await;
        let runs = MigrationRuns::open(&d);
        (state, runs, d)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn migrates_sqlite_to_sqlite_through_the_engine() {
        let (state, runs, _d) = setup("run").await;
        let (emit, seen) = collect();
        let r = run_migration(&state, &runs, args("m1", false), emit).await.unwrap();
        assert_eq!(r.status, "done", "{:?}", r.tables.iter().map(|t| (&t.name, &t.status, &t.error)).collect::<Vec<_>>());
        assert_eq!(count(&state, "tgt", "parent").await, 20);
        assert_eq!(count(&state, "tgt", "child").await, 3000);
        assert_eq!(count(&state, "tgt", "empty_one").await, 0);
        // Blobs whole, the index created after the copy.
        let out = run_sql(&state, "tgt", "SELECT length(data) FROM child WHERE id = 699").await;
        assert_eq!(out.results[0].rows[0][0].as_i64(), Some(699));
        let out = run_sql(&state, "tgt", "SELECT name FROM sqlite_master WHERE type = 'index' AND name = 'ix_child_parent'").await;
        assert_eq!(out.results[0].rows.len(), 1);
        let child = r.tables.iter().find(|t| t.name == "child").unwrap();
        assert_eq!((child.status.as_str(), child.rows_done, child.path.as_str()), ("done", 3000, "native"));

        // Events: the plan, the engine's own (tagged with the run), the steps.
        let seen = seen.lock().unwrap();
        let kinds: HashSet<&str> = seen.iter().filter_map(|v| v["event"].as_str()).collect();
        for k in ["plan", "step", "run_started", "table_done", "run_finished"] {
            assert!(kinds.contains(k), "no {k} event: {kinds:?}");
        }
        assert!(seen.iter().all(|v| v["id"] == "m1"));

        // The record lists it, done.
        let list = list_runs(&runs, 10, None);
        assert_eq!((list[0].id.as_str(), list[0].status.as_str(), list[0].resumable), ("m1", "done", false));
        // A saved migration asks for its own runs by id.
        assert_eq!(list_runs(&runs, 10, Some(&["m1".to_string()])).len(), 1);
        assert!(list_runs(&runs, 10, Some(&["other".to_string()])).is_empty());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn failed_table_is_retried_and_interrupted_run_resumes() {
        let (state, runs, d) = setup("retry").await;
        // A target table with rows of its own: not copied into.
        run_sql(&state, "tgt", "CREATE TABLE parent (id INTEGER PRIMARY KEY, name TEXT NOT NULL); INSERT INTO parent VALUES (99, 'x');").await;
        let (emit, _) = collect();
        let r = run_migration(&state, &runs, args("m2", false), emit.clone()).await.unwrap();
        assert_eq!(r.status, "failed");
        let parent = r.tables.iter().find(|t| t.name == "parent").unwrap();
        assert_eq!(parent.status, "failed");
        assert!(parent.error.as_deref().unwrap_or("").contains("ya tiene filas"), "{:?}", parent.error);
        assert_eq!(count(&state, "tgt", "child").await, 3000);

        // The user empties it and retries: only that table runs again.
        run_sql(&state, "tgt", "DELETE FROM parent").await;
        let r = go_on(&state, &runs, "m2", true, emit.clone()).await.unwrap();
        assert_eq!(r.status, "done", "{:?}", r.tables.iter().map(|t| (&t.name, &t.status, &t.error)).collect::<Vec<_>>());
        assert_eq!(count(&state, "tgt", "parent").await, 20);
        assert_eq!(count(&state, "tgt", "child").await, 3000);

        // A process that ended mid-run: at the next start the run is interrupted and resumes.
        let mut m = runs.load("m2").unwrap();
        m.status = "running".into();
        m.stage = "data".into();
        runs.save(&m);
        drop(runs);
        let runs = MigrationRuns::open(&d);
        let list = list_runs(&runs, 10, None);
        assert_eq!((list[0].status.as_str(), list[0].resumable), ("interrupted", true));
        let r = go_on(&state, &runs, "m2", false, emit).await.unwrap();
        assert_eq!(r.status, "done");
        assert_eq!(count(&state, "tgt", "child").await, 3000);
    }

    fn message(e: CommandError) -> String {
        match e {
            CommandError::BadRequest(m) => m,
            other => panic!("expected a request error, got {other}"),
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    async fn clone_and_sync_are_refused_where_the_engine_lacks_them() {
        let (state, runs, _d) = setup("modes").await;
        let sqlite = dbine_drivers::find("sqlite").unwrap().as_ref();
        // The targets say why, per source.
        let t = targets_for(Some(sqlite)).into_iter().find(|t| t.id == "sqlite").unwrap();
        assert!(t.supported && !t.clone.available && !t.sync.available);
        assert_eq!(t.clone.reason.as_deref(), Some("SQLite no clona bases"));
        assert_eq!(t.sync.reason.as_deref(), Some("SQLite no sincroniza por filas"));
        let pg = targets_for(Some(sqlite)).into_iter().find(|t| t.id == "postgres").unwrap();
        assert!(pg.clone.reason.unwrap().contains("mismo motor"));
        assert!(pg.sync.reason.unwrap().contains("mismo motor"));

        // Preview and run refuse them, with the reason; nothing is created on the target.
        let mut a = args("c1", false);
        a.plan.mode = MigrationMode::Clone;
        a.plan.target = Some(PlanTarget { connection_id: "tgt".into(), database: String::new() });
        assert_eq!(message(plan(&state, &a.plan).await.err().unwrap()), "SQLite no clona bases");
        let (emit, _) = collect();
        assert_eq!(message(run_migration(&state, &runs, a, emit.clone()).await.err().unwrap()), "SQLite no clona bases");
        let mut a = args("s1", false);
        a.plan.mode = MigrationMode::Sync;
        assert_eq!(message(plan(&state, &a.plan).await.err().unwrap()), "SQLite no sincroniza por filas");
        assert_eq!(message(run_migration(&state, &runs, a, emit).await.err().unwrap()), "SQLite no sincroniza por filas");
        let out = run_sql(&state, "tgt", "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table'").await;
        assert_eq!(out.results[0].rows[0][0].as_i64(), Some(0));
        // No record left behind for a refused run.
        assert!(list_runs(&runs, 10, None).is_empty());
    }

    #[test]
    fn sync_key_is_the_primary_key_or_a_chosen_unique_one() {
        use dbine_driver::{IndexDef, KeyDef};
        let col = |n: &str| ColumnDef { name: n.into(), data_type: "int".into(), ..Default::default() };
        let mut t = TableSchema { name: "t".into(), columns: vec![col("id"), col("code"), col("x")], ..Default::default() };
        let o = SyncOptions::default();
        assert!(sync_key(&t, &o).unwrap_err().contains("ni claves únicas"));
        t.indexes.push(IndexDef { name: "ux".into(), columns: vec!["code".into()], unique: true, ..Default::default() });
        t.indexes.push(IndexDef { name: "ix".into(), columns: vec!["x".into()], unique: false, ..Default::default() });
        assert!(sync_key(&t, &o).unwrap_err().contains("elegí una clave única"));
        let chosen = |cols: &[&str]| SyncOptions {
            keys: vec![SyncKey { schema: None, name: "t".into(), columns: cols.iter().map(|c| c.to_string()).collect() }],
            ..Default::default()
        };
        assert_eq!(sync_key(&t, &chosen(&["code"])).unwrap(), vec!["code"]);
        assert!(sync_key(&t, &chosen(&["x"])).is_err());
        t.primary_key = Some(KeyDef { name: None, columns: vec!["id".into()] });
        assert_eq!(sync_key(&t, &o).unwrap(), vec!["id"]);
        assert_eq!(unique_keys(&t), vec![vec!["code".to_string()]]);
    }

    #[test]
    fn old_records_read_as_convert() {
        let v = json!({ "connection_id": "a", "database": "", "target_driver": "sqlite", "options": {} });
        let p: PlanArgs = serde_json::from_value(v).unwrap();
        assert_eq!(p.mode, MigrationMode::Convert);
        assert_eq!(p.sync.depth, DeltaDepth::Full);
        let v = json!({ "connection_id": "a", "database": "", "target_driver": "sqlite", "options": {}, "mode": "sync",
                        "sync": { "depth": "Sizes", "max_cores": 2, "keys": [] } });
        let p: PlanArgs = serde_json::from_value(v).unwrap();
        assert_eq!((p.mode, p.sync.depth, p.sync.max_cores), (MigrationMode::Sync, DeltaDepth::Sizes, 2));
    }

    // -- against dbine-test-postgres (docker, port 25010) ------------------------------------------

    const PG_DBS: [&str; 2] = ["dbine_mig_clone_src", "dbine_mig_clone_tgt"];

    fn postgres(state: &AppState, id: &str, database: &str) {
        let cfg = ConnectionConfig {
            driver: "postgres".into(),
            host: "localhost".into(),
            port: 25010,
            database: database.into(),
            username: Some("postgres".into()),
            ..Default::default()
        };
        let conn = SavedConnection {
            id: id.into(),
            name: id.into(),
            color: None,
            config: cfg,
            save_password: false,
            folder_id: None,
            tags: vec![],
            mcp_level: None,
            updated_at: String::new(),
        };
        state.store.save_connection(&conn).unwrap();
        state.typed_secrets.insert(id.into(), [("password".to_string(), "pw".to_string())].into_iter().collect());
    }

    /// Two fresh databases on dbine-test-postgres, the source with a parent / child pair (a foreign
    /// key, an index, an identity, a check and a view).
    async fn pg_setup(tag: &str) -> (AppState, MigrationRuns) {
        let d = dir(tag);
        let state = AppState::new(StateStore::open(&d.join("state.sqlite")).unwrap());
        postgres(&state, "admin", "postgres");
        for db in PG_DBS {
            run_sql(&state, "admin", &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
            run_sql(&state, "admin", &format!("CREATE DATABASE {db}")).await;
        }
        postgres(&state, "src", PG_DBS[0]);
        postgres(&state, "tgt", PG_DBS[1]);
        run_sql(
            &state,
            "src",
            "CREATE TABLE parent (id int GENERATED BY DEFAULT AS IDENTITY PRIMARY KEY, name text NOT NULL CHECK (name <> ''));
             CREATE TABLE child (id bigint PRIMARY KEY, parent_id int NOT NULL REFERENCES parent(id), note text, data bytea);
             CREATE INDEX ix_child_parent ON child (parent_id);
             CREATE VIEW v_child AS SELECT c.id, p.name FROM child c JOIN parent p ON p.id = c.parent_id;
             INSERT INTO parent (name) SELECT 'p' || g FROM generate_series(1, 20) g;
             INSERT INTO child SELECT g, g % 20 + 1, 'n' || g, decode(repeat('ab', g % 50), 'hex') FROM generate_series(1, 5000) g;",
        )
        .await;
        (state, MigrationRuns::open(&d))
    }

    async fn pg_scalar(state: &AppState, conn: &str, sql: &str) -> i64 {
        let v = &run_sql(state, conn, sql).await.results[0].rows[0][0];
        v.as_i64().or_else(|| v.as_str().and_then(|s| s.parse().ok())).unwrap_or_else(|| panic!("{sql}: {v}"))
    }

    fn pg_args(id: &str, mode: MigrationMode) -> RunArgs {
        let mut a = args(id, false);
        a.plan.target_driver = "postgres".into();
        a.plan.database = PG_DBS[0].into();
        a.target_database = PG_DBS[1].into();
        a.plan.mode = mode;
        a
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs dbine-test-postgres on localhost:25010"]
    async fn clones_then_syncs_postgres() {
        let (state, runs) = pg_setup("pg").await;

        // The preview: the driver's script, from the target's capabilities.
        let mut a = pg_args("pgc", MigrationMode::Clone);
        a.plan.target = Some(PlanTarget { connection_id: "tgt".into(), database: PG_DBS[1].into() });
        let p = plan(&state, &a.plan).await.unwrap();
        assert!(p.script.contains("CREATE TABLE") && p.script.contains("ix_child_parent"), "{}", p.script);
        assert_eq!(p.tables.len(), 2);

        // Clone: identical tables, rows, index, foreign key, check and view.
        let (emit, _) = collect();
        let r = run_migration(&state, &runs, a, emit.clone()).await.unwrap();
        assert_eq!(r.status, "done", "{:?} {:?} {:?}", r.tables.iter().map(|t| (&t.name, &t.status, &t.error)).collect::<Vec<_>>(), r.after_errors, r.notes);
        assert_eq!(r.mode, MigrationMode::Clone);
        assert_eq!(pg_scalar(&state, "tgt", "SELECT COUNT(*) FROM child").await, 5000);
        assert_eq!(pg_scalar(&state, "tgt", "SELECT COUNT(*) FROM pg_indexes WHERE indexname = 'ix_child_parent'").await, 1);
        assert_eq!(pg_scalar(&state, "tgt", "SELECT COUNT(*) FROM pg_constraint WHERE contype IN ('f', 'c') AND conrelid <> 0").await, 2);
        assert_eq!(pg_scalar(&state, "tgt", "SELECT COUNT(*) FROM v_child").await, 5000);
        let same = "SELECT COALESCE(SUM(hashtext(id || ':' || parent_id || ':' || COALESCE(note, '') || ':' || COALESCE(encode(data, 'hex'), ''))::bigint), 0) FROM child";
        assert_eq!(pg_scalar(&state, "tgt", same).await, pg_scalar(&state, "src", same).await);

        // The source changes; sync only what changed.
        run_sql(
            &state,
            "src",
            "INSERT INTO child SELECT g, 1, 'new', NULL FROM generate_series(5001, 5100) g;
             UPDATE child SET note = 'changed' WHERE id BETWEEN 10 AND 39;
             DELETE FROM child WHERE id BETWEEN 4000 AND 4009;",
        )
        .await;
        let mut a = pg_args("pgs", MigrationMode::Sync);
        a.plan.sync.depth = DeltaDepth::Full;
        let p = plan(&state, &a.plan).await.unwrap();
        assert!(p.sync_tables.iter().all(|t| t.key.is_some()), "{:?}", p.sync_tables.iter().map(|t| &t.reason).collect::<Vec<_>>());
        let r = run_migration(&state, &runs, a, emit).await.unwrap();
        assert_eq!(r.status, "done", "{:?}", r.tables.iter().map(|t| (&t.name, &t.status, &t.error)).collect::<Vec<_>>());
        let child = r.tables.iter().find(|t| t.name.ends_with("child")).unwrap();
        let d = child.stats.as_ref().and_then(|s| s.delta.clone()).unwrap();
        assert_eq!((d.inserted, d.updated, d.deleted), (100, 30, 10));
        assert_eq!(pg_scalar(&state, "tgt", same).await, pg_scalar(&state, "src", same).await);
        let parent = r.tables.iter().find(|t| t.name.ends_with("parent")).unwrap();
        assert_eq!(parent.stats.as_ref().and_then(|s| s.delta.clone()).unwrap(), Default::default());

        for db in PG_DBS {
            run_sql(&state, "admin", &format!("DROP DATABASE IF EXISTS {db} WITH (FORCE)")).await;
        }
    }

    /// A partition scheme the target can't create (a filegroup has its name): the table on it
    /// fails, and its error names that cause, not only "Invalid partition scheme".
    #[tokio::test(flavor = "multi_thread", worker_threads = 4)]
    #[ignore = "needs dbine-test-sqlserver on localhost:25013"]
    async fn clone_table_error_names_the_failed_before_mssql() {
        const DBS: [&str; 2] = ["dbine_mig_ps_src", "dbine_mig_ps_tgt"];
        let d = dir("mssql-ps");
        let state = AppState::new(StateStore::open(&d.join("state.sqlite")).unwrap());
        let mssql = |id: &str, database: &str| {
            let cfg = ConnectionConfig {
                driver: "sqlserver".into(),
                host: "localhost".into(),
                port: 25013,
                database: database.into(),
                username: Some("sa".into()),
                ..Default::default()
            };
            let conn = SavedConnection {
                id: id.into(),
                name: id.into(),
                color: None,
                config: cfg,
                save_password: false,
                folder_id: None,
                tags: vec![],
                mcp_level: None,
                updated_at: String::new(),
            };
            state.store.save_connection(&conn).unwrap();
            state.typed_secrets.insert(id.into(), [("password".to_string(), "Pw_12345!".to_string())].into_iter().collect());
        };
        mssql("admin", "master");
        let drop = |db: &str| format!("IF DB_ID('{db}') IS NOT NULL BEGIN ALTER DATABASE [{db}] SET SINGLE_USER WITH ROLLBACK IMMEDIATE; DROP DATABASE [{db}]; END");
        for db in DBS {
            run_sql(&state, "admin", &drop(db)).await;
            run_sql(&state, "admin", &format!("CREATE DATABASE [{db}]")).await;
        }
        run_sql(&state, "admin", &format!("ALTER DATABASE [{}] ADD FILEGROUP [ps_Monthly]", DBS[1])).await;
        mssql("src", DBS[0]);
        mssql("tgt", DBS[1]);
        run_sql(&state, "src", "CREATE PARTITION FUNCTION pf_Monthly (int) AS RANGE RIGHT FOR VALUES (10, 20)").await;
        run_sql(&state, "src", "CREATE PARTITION SCHEME ps_Monthly AS PARTITION pf_Monthly ALL TO ([PRIMARY])").await;
        run_sql(&state, "src", "CREATE TABLE dbo.sales (id int NOT NULL, amount int) ON ps_Monthly (id); INSERT INTO dbo.sales VALUES (1, 1), (15, 2)").await;

        let mut a = args("mps", false);
        a.plan.connection_id = "src".into();
        a.plan.database = DBS[0].into();
        a.plan.target_driver = "sqlserver".into();
        a.plan.mode = MigrationMode::Clone;
        a.plan.target = Some(PlanTarget { connection_id: "tgt".into(), database: DBS[1].into() });
        a.target_connection_id = "tgt".into();
        a.target_database = DBS[1].into();
        let (emit, _) = collect();
        let r = run_migration(&state, &MigrationRuns::open(&d), a, emit).await;
        for db in DBS {
            run_sql(&state, "admin", &drop(db)).await;
        }
        let r = r.unwrap();
        let sales = r.tables.iter().find(|t| t.name.ends_with("sales")).unwrap();
        let err = sales.error.clone().unwrap_or_default();
        eprintln!("table error: {err}\nnotes: {:?}", r.notes);
        assert!(err.contains("causa: no se pudo crear ps_Monthly: ") && err.contains("filegroup"), "{err}");
        assert!(r.notes.iter().any(|n| n.contains("CREATE PARTITION SCHEME [ps_Monthly]")), "{:?}", r.notes);
    }

    #[test]
    fn clone_before_failure_is_the_table_cause() {
        let scheme = "IF NOT EXISTS (SELECT 1 FROM sys.partition_schemes WHERE name = N'ps_Monthly')\nBEGIN\n    IF EXISTS (SELECT 1 FROM sys.data_spaces WHERE name = N'ps_Monthly')\n        THROW 50000, N'x', 1;\n    CREATE PARTITION SCHEME [ps_Monthly] AS PARTITION [pf_Monthly] TO ([PRIMARY], [PRIMARY]);\nEND";
        let failed = vec![(scheme.to_string(), "Ya hay un filegroup llamado ps_Monthly en el destino".to_string())];
        assert_eq!(before_label(scheme), "CREATE PARTITION SCHEME [ps_Monthly] AS PARTITION [pf_Monthly] TO ([PRIMARY], [PRIMARY])");

        let cause = before_cause("Invalid partition scheme 'ps_Monthly' specified.", "Sales", &failed);
        assert_eq!(cause.as_deref(), Some("no se pudo crear ps_Monthly: Ya hay un filegroup llamado ps_Monthly en el destino"));
        // Case-insensitive, and a qualified name matches by its last part.
        assert!(before_cause("Invalid partition scheme 'PS_MONTHLY' specified.", "Sales", &failed).is_some());
        assert!(before_cause("Cannot find the type 'dbo.ps_Monthly'.", "Sales", &failed).is_some());
        // Unrelated errors, partial names and the table's own name stay as they were.
        assert!(before_cause("Invalid column name 'amount'.", "Sales", &failed).is_none());
        assert!(before_cause("Invalid partition scheme 'ps_Month' specified.", "Sales", &failed).is_none());
        assert!(before_cause("There is already an object named 'ps_Monthly' in the database.", "ps_Monthly", &failed).is_none());
        assert!(before_cause("Invalid partition scheme 'ps_Monthly' specified.", "Sales", &[]).is_none());

        assert_eq!(quoted_names("x '[a].[b]' y [c] 'has space' ''"), vec!["[a].[b]", "b", "a", "b", "c"]);
        assert_eq!(before_label("ALTER TABLE t ADD c int"), "ALTER TABLE t ADD c int");
        assert_eq!(before_label("SELECT 1"), "SELECT 1");
    }
}
