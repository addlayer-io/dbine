//! SQLite through rusqlite. The connection is synchronous, so every call
//! runs on a blocking thread; `interrupter` stops the one in flight.

// Public for the libSQL driver, which reuses the designer, the DDL, the
// catalog reader, the plans and the monitor over HTTP.
pub mod backup;
pub mod index_usage;
pub mod monitor;
mod permissions;
pub mod plan;
mod read_only;
pub mod properties;
pub mod rename;
pub mod schema;
pub mod stats;
pub mod transfer;

use dbine_driver::sql::{leading_keyword, quote_ident, select_top, split_statements, Limit, Quote, ScriptDefaults, ScriptDialect, ScriptMode};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, Language, ObjectKindInfo, ObjectRef,
    QueryOutcome, ResultColumn, Result, ScriptError, Session, TableSchema, TxState,
};
use rusqlite::types::ValueRef;
use rusqlite::{Batch, Connection, ErrorCode, InterruptHandle, OpenFlags, OptionalExtension};
use std::sync::{Arc, Mutex};
use std::time::Duration;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(SqliteDriver { info: info() })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "sqlite",
        name: "SQLite",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "sqlite",
        default_port: 0,
        fields: vec![Field::file("Archivo de base de datos"), Field::read_only()],
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::tables(), schema::virtual_tables(), ObjectKindInfo::views(), ObjectKindInfo::triggers()],
    }
}

pub struct SqliteDriver {
    info: DriverInfo,
}

pub struct SqliteSession {
    conn: Arc<Mutex<Connection>>,
    interrupt: Arc<InterruptHandle>,
    /// Manual transactions: the first statement that writes opens one.
    manual: bool,
    /// `run_read_only`'s connection to the same file, opened read-only on
    /// first use, and its interrupt handle (see [`read_only`]).
    ro: Arc<Mutex<Option<Connection>>>,
    ro_interrupt: Arc<Mutex<Option<InterruptHandle>>>,
}

/// An interrupted statement reads as a cancellation; anything else is the
/// engine rejecting the statement.
fn err(e: rusqlite::Error) -> Error {
    if e.sqlite_error_code() == Some(ErrorCode::OperationInterrupted) {
        Error::Cancelled
    } else {
        Error::Query(e.to_string())
    }
}

/// A statement of `script` failed: SQLite's extended result code as the
/// code ("1" for a syntax error, "2067" for a UNIQUE violation…), and where
/// the bad token is when SQLite says (`sqlite3_error_offset`, relative to
/// the rest of `script` the statement was prepared from).
fn stmt_err(e: rusqlite::Error, script: &str) -> Error {
    match e {
        rusqlite::Error::SqlInputError { error, msg, sql, offset } => {
            let at = script.len().saturating_sub(sql.len()) + offset.max(0) as usize;
            ScriptError::new(msg).with_code(error.extended_code.to_string()).at_offset(at.min(script.len())).into()
        }
        rusqlite::Error::SqliteFailure(f, msg) if f.code != ErrorCode::OperationInterrupted => {
            ScriptError::new(msg.unwrap_or_else(|| f.to_string())).with_code(f.extended_code.to_string()).into()
        }
        other => err(other),
    }
}

#[async_trait]
impl Driver for SqliteDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// sqlite3's: `;` outside quotes and comments (`[name]` and `` `name` ``
    /// too), trigger bodies kept whole until their `END`.
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect { bracket_idents: true, ..ScriptDialect::generic() }
    }

    /// One statement per call on the tab's connection, which keeps its
    /// state (temp tables, PRAGMAs, ATTACH, an open transaction).
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// sqlite3 goes on after an error unless `.bail on`.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    fn supports_manual_transactions(&self) -> bool {
        true
    }

    /// A database is a file: it's created by connecting to a new path and
    /// deleted from the file system, not by SQL.
    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: false, drop_database: false, foreign_keys: true, monitor: true, database_properties: true, ..Default::default() }
    }

    fn alter_database_script(&self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        properties::script(properties::Flavor::Sqlite, database, changes)
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(schema::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        schema::create_templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(schema::table_ddl(table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    /// The indexes, keys and sizes; SQLite has no usage counters (see [`index_usage`]).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    /// A prepared `INSERT` in a transaction per commit window (see [`transfer`]).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Another SQLite file: attached read-only to the target (see [`transfer`]).
    fn supports_native_copy(&self, target: &str) -> bool {
        target == "sqlite"
    }

    async fn copy_native(
        &self,
        source: &mut dyn Session,
        target: &mut dyn Session,
        spec: &dbine_driver::CopySpec,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::copy_native(source, target, spec, progress).await
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    /// Columns are added in place; any other change rebuilds the table.
    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        schema::sync_script(changes)
    }

    /// Tables and columns with `ALTER TABLE … RENAME`, which views and
    /// triggers follow; triggers and indexes created again (see [`rename`]).
    fn rename_spec(&self) -> Option<dbine_driver::RenameSpec> {
        Some(rename::spec(properties::Flavor::Sqlite))
    }

    fn rename_script(&self, req: &dbine_driver::RenameRequest) -> Result<dbine_driver::SyncScript> {
        rename::script(properties::Flavor::Sqlite, req)
    }

    /// `cfg.host` is the database file; `database` doesn't apply.
    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let path = cfg.host.trim().to_string();
        if path.is_empty() {
            return Err(Error::Connect("falta la ruta del archivo SQLite".into()));
        }
        let read_only = cfg.read_only;
        let conn = blocking(move || {
            let conn = if read_only {
                Connection::open_with_flags(
                    &path,
                    OpenFlags::SQLITE_OPEN_READ_ONLY | OpenFlags::SQLITE_OPEN_URI | OpenFlags::SQLITE_OPEN_NO_MUTEX,
                )
            } else {
                Connection::open(&path)
            }
            .map_err(|e| Error::Connect(e.to_string()))?;
            conn.busy_timeout(Duration::from_secs(5)).map_err(err)?;
            conn.execute_batch("PRAGMA foreign_keys = ON").map_err(err)?;
            Ok(conn)
        })
        .await?;
        let interrupt = Arc::new(conn.get_interrupt_handle());
        Ok(Box::new(SqliteSession {
            conn: Arc::new(Mutex::new(conn)),
            interrupt,
            manual: false,
            ro: Arc::new(Mutex::new(None)),
            ro_interrupt: Arc::new(Mutex::new(None)),
        }))
    }
}

async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::State(e.to_string()))?
}

impl SqliteSession {
    /// Run `f` on the connection, on a blocking thread.
    async fn with<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        blocking(move || {
            let c = conn.lock().map_err(|_| Error::State("conexión SQLite envenenada".into()))?;
            f(&c)
        })
        .await
    }
}

/// Why the process list and cancelling don't apply, in Spanish (for the
/// error and the docs).
const NO_PROCESSES: &str = "SQLite es una base embebida en un archivo: no hay un servidor con sesiones de otros clientes que listar ni consultas ajenas que cancelar";

#[async_trait]
impl Session for SqliteSession {
    async fn server_version(&mut self) -> Result<String> {
        Ok(format!("SQLite {}", rusqlite::version()))
    }

    /// A single namespace (attached databases show up as schemas in scripts).
    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["main".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        self.with(|c| {
            let mut stmt = c
                .prepare(
                    "SELECT type, name, tbl_name, COALESCE(sql, '') FROM sqlite_master
                     WHERE type IN ('table', 'view', 'trigger') AND name NOT LIKE 'sqlite\\_%' ESCAPE '\\'
                     ORDER BY name",
                )
                .map_err(err)?;
            let rows = stmt
                .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?, r.get::<_, String>(2)?, r.get::<_, String>(3)?)))
                .map_err(err)?;
            let mut out = Vec::new();
            for row in rows {
                let (ty, name, tbl, sql) = row.map_err(err)?;
                let (kind, parent) = match ty.as_str() {
                    "table" if schema::is_virtual(&sql) => (schema::VIRTUAL_TABLE, None),
                    "table" => (kinds::TABLE, None),
                    "view" => (kinds::VIEW, None),
                    _ => (kinds::TRIGGER, Some(tbl)),
                };
                out.push(DbObject { kind: kind.into(), schema: None, name, parent });
            }
            Ok(out)
        })
        .await
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let schema = obj.schema().unwrap_or("main").to_string();
        let name = obj.name.clone();
        self.with(move |c| {
            let mut stmt = c
                .prepare("SELECT name, type, \"notnull\", dflt_value, pk FROM pragma_table_info(?1, ?2) ORDER BY cid")
                .map_err(err)?;
            let rows = stmt
                .query_map([&name, &schema], |r| {
                    Ok((
                        r.get::<_, String>(0)?,
                        r.get::<_, String>(1)?,
                        r.get::<_, bool>(2)?,
                        r.get::<_, Option<String>>(3)?,
                        r.get::<_, i64>(4)?,
                    ))
                })
                .map_err(err)?;
            let rows = rows.collect::<rusqlite::Result<Vec<_>>>().map_err(err)?;
            let pk_count = rows.iter().filter(|r| r.4 > 0).count();
            Ok(rows
                .into_iter()
                .map(|(name, data_type, notnull, default_value, pk)| ColumnInfo {
                    // A lone INTEGER PRIMARY KEY is the rowid alias.
                    auto_increment: pk > 0 && pk_count == 1 && data_type.eq_ignore_ascii_case("INTEGER"),
                    name,
                    data_type,
                    nullable: !notnull && pk == 0,
                    primary_key: pk > 0,
                    default_value,
                })
                .collect())
        })
        .await
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let ty = match obj.kind.as_str() {
            kinds::TABLE | schema::VIRTUAL_TABLE => "table",
            kinds::VIEW => "view",
            kinds::TRIGGER => "trigger",
            _ => return Ok(None),
        };
        let master = match obj.schema() {
            Some(s) => format!("{}.sqlite_master", quote_ident(Quote::Double, s)),
            None => "sqlite_master".to_string(),
        };
        let name = obj.name.clone();
        self.with(move |c| {
            let sql = c
                .query_row(&format!("SELECT sql FROM {master} WHERE type = ?1 AND name = ?2"), [ty, &name], |r| {
                    r.get::<_, Option<String>>(0)
                })
                .optional()
                .map_err(err)?;
            Ok(sql.flatten())
        })
        .await
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let sql = sql.to_string();
        let manual = self.manual;
        // The outcome can't cross into the blocking thread by reference;
        // fill a local one and move its results over, error or not.
        let fork = out.fork();
        let (local, res) = self
            .with(move |c| {
                let mut local = fork;
                let res = run_script_tx(c, &sql, max_rows, manual, &mut local);
                Ok((local, res))
            })
            .await?;
        out.merge(local);
        res
    }

    /// One statement on a connection that can't write, under an authorizer
    /// that only lets it read (see [`read_only`]).
    async fn run_read_only(&mut self, statement: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let sql = statement.to_string();
        let (main, ro, ro_interrupt) = (self.conn.clone(), self.ro.clone(), self.ro_interrupt.clone());
        let fork = out.fork();
        let (local, res) = blocking(move || {
            let mut local = fork;
            let res = read_only::run(&main, &ro, &ro_interrupt, &sql, max_rows, &mut local);
            Ok((local, res))
        })
        .await?;
        out.merge(local);
        res
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        self.with(|c| Ok(Some(if c.is_autocommit() { TxState::Idle } else { TxState::Open }))).await
    }

    /// Off: the next statement that writes opens a transaction (`BEGIN`),
    /// which stays open until Commit / Rollback. On: a transaction still
    /// open is committed, as turning autocommit on does in other engines
    /// (the UI asks Commit / Rollback first).
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if on {
            self.with(|c| if c.is_autocommit() { Ok(()) } else { c.execute_batch("COMMIT").map_err(err) }).await?;
        }
        self.manual = !on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.with(|c| if c.is_autocommit() { Ok(()) } else { c.execute_batch("COMMIT").map_err(err) }).await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.with(|c| if c.is_autocommit() { Ok(()) } else { c.execute_batch("ROLLBACK").map_err(err) }).await
    }

    /// `EXPLAIN QUERY PLAN` per statement. SQLite has no costs, row
    /// estimates or measured figures, so with `analyze` the script runs as
    /// with `execute` and the plans are still the estimated ones.
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let sql = sql.to_string();
        let fork = out.fork();
        let (local, res) = self
            .with(move |c| {
                let mut local = fork;
                let res = explain_script(c, &sql, analyze, max_rows, &mut local);
                Ok((local, res))
            })
            .await?;
        out.merge(local);
        res
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        self.with(|c| schema::read_schema(c).map_err(err)).await
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let name = table.name.clone();
        self.with(move |c| index_usage::report_with(&mut |sql| schema::query_rows(c, sql), &name).map_err(err)).await.map(Some)
    }

    /// From `sqlite_stat1` when `ANALYZE` has run; otherwise none (see [`stats`]).
    /// SQLite has no comments on objects, so `object_comments` stays empty.
    async fn row_estimates(&mut self) -> Result<Vec<dbine_driver::stats::RowEstimate>> {
        self.with(|c| stats::row_estimates_with(&mut |sql| schema::query_rows(c, sql)).map_err(err)).await
    }

    /// Embedded: no server, so no other sessions to list or stop.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        Err(Error::Unsupported(NO_PROCESSES.into()))
    }

    async fn cancel_query(&mut self, _id: &str) -> Result<()> {
        Err(Error::Unsupported(NO_PROCESSES.into()))
    }

    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        let database = database.to_string();
        self.with(move |c| Ok(properties::read(&mut |sql| schema::query_rows(c, sql), &database, properties::Flavor::Sqlite))).await
    }

    /// One statement at a time; then the settings SQLite may have accepted
    /// without applying (journal mode, page size, auto vacuum) are read back.
    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        let statements = properties::alter(properties::Flavor::Sqlite, database, changes)?;
        let (database, changes) = (database.to_string(), changes.clone());
        self.with(move |c| {
            for (i, sql) in statements.iter().enumerate() {
                if let Err(e) = schema::query_rows(c, sql) {
                    let e = err(e);
                    return Err(if i == 0 { e } else { Error::Query(format!("se aplicaron {i} de {} cambios; falló: {sql}\n{e}", statements.len())) });
                }
            }
            let now = properties::values(&mut |sql| schema::query_rows(c, sql), &database);
            let left = properties::not_applied(&changes, &now);
            if left.is_empty() {
                Ok(())
            } else {
                Err(Error::Query(format!("SQLite aceptó los cambios pero no aplicó todos: {}", left.join("; "))))
            }
        })
        .await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.with(|c| {
            let mut snap = monitor::pragma_snapshot(&mut |sql| schema::query_rows(c, sql));
            local_figures(c, &mut snap);
            Ok(snap)
        })
        .await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (h, ro) = (self.interrupt.clone(), self.ro_interrupt.clone());
        Some(Arc::new(move || {
            h.interrupt();
            if let Some(r) = ro.lock().ok().as_deref().and_then(Option::as_ref) {
                r.interrupt();
            }
        }))
    }

    /// Typed cells straight from SQLite's values, blobs whole (see [`transfer`]).
    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        let spec = spec.clone();
        self.with(move |c| transfer::read(c, &spec, &sink)).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
    }

    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    /// No logins: only `PRAGMA query_only` stops the backup (see `permissions`).
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        self.with(|c| Ok(permissions::check(c))).await
    }
}

/// What only a local file has: its size on disk (and the WAL's), SQLite's
/// heap and this connection's page cache.
fn local_figures(c: &Connection, snap: &mut dbine_driver::MonitorSnapshot) {
    use dbine_driver::monitor::{Metric, MetricUnit as U};
    let file: Option<String> = c.query_row("SELECT file FROM pragma_database_list WHERE name = 'main'", [], |r| r.get(0)).ok();
    let file = file.filter(|f| !f.is_empty());
    let size = |p: &str| std::fs::metadata(p).ok().map(|m| m.len() as f64);
    let (db, wal) = match &file {
        Some(f) => (size(f), size(&format!("{f}-wal"))),
        None => (None, None),
    };
    // SAFETY: plain status calls on a live connection handle.
    let (heap, heap_peak, cache, hits, misses) = unsafe {
        let status = |op: i32| {
            let (mut cur, mut hi) = (0, 0);
            let rc = rusqlite::ffi::sqlite3_db_status(c.handle(), op, &mut cur, &mut hi, 0);
            (rc == rusqlite::ffi::SQLITE_OK).then_some(cur as f64)
        };
        (
            rusqlite::ffi::sqlite3_memory_used() as f64,
            rusqlite::ffi::sqlite3_memory_highwater(0) as f64,
            status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_USED),
            status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_HIT),
            status(rusqlite::ffi::SQLITE_DBSTATUS_CACHE_MISS),
        )
    };
    let hit = match (hits, misses) {
        (Some(h), Some(m)) if h + m > 0.0 => Some(h / (h + m) * 100.0),
        _ => None,
    };
    let m = &mut snap.metrics;
    m.insert(0, Metric::new("file_size", "Tamaño del archivo", "Almacenamiento", U::Bytes, db));
    m.push(Metric::new("wal_size", "Tamaño del WAL", "Almacenamiento", U::Bytes, wal));
    m.push(Metric::new("mem_used", "Memoria de SQLite (proceso)", "Memoria", U::Bytes, Some(heap)).max(Some(heap_peak)));
    m.push(Metric::new("mem_cache", "Caché de páginas de la conexión", "Memoria", U::Bytes, cache));
    m.push(Metric::new("cache_hit", "Aciertos de caché (esta conexión)", "Caché", U::Percent, hit).max(Some(100.0)));
    match file {
        Some(f) => snap.info.insert(0, ("Archivo".into(), f)),
        None => snap.info.insert(0, ("Archivo".into(), "(en memoria)".into())),
    }
    snap.notes.push("La caché y sus aciertos son de la conexión del monitor, no de las otras pestañas.".into());
}

fn run_script(c: &Connection, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    run_script_tx(c, sql, max_rows, false, out)
}

/// Run every statement of `sql`. `manual`: a statement that writes opens a
/// transaction first when none is open (transaction control, reads and
/// ATTACH don't), as autocommit off does in other engines.
fn run_script_tx(c: &Connection, sql: &str, max_rows: usize, manual: bool, out: &mut QueryOutcome) -> Result<()> {
    let mut batch = Batch::new(c, sql);
    while let Some(mut stmt) = batch.next().map_err(|e| stmt_err(e, sql))? {
        // Manual mode opens the transaction for the first statement that
        // writes; one opened here for a statement that then fails is
        // rolled back (it holds nothing else), so it isn't left open empty.
        let opened = manual && c.is_autocommit() && !stmt.readonly() && !stmt.expanded_sql().is_some_and(|s| outside_transaction(&s));
        if opened {
            c.execute_batch("BEGIN").map_err(err)?;
        }
        let before = out.results.len();
        let r = run_stmt(c, &mut stmt, sql, max_rows, out);
        if r.is_err() {
            // A query cancelled or failed mid-way leaves no empty grid.
            if out.results.len() > before && out.results[before..].iter().all(|r| r.total_rows == 0 && r.rows_affected.is_none()) {
                out.results.truncate(before);
            }
            if opened && !c.is_autocommit() {
                let _ = c.execute_batch("ROLLBACK");
            }
        }
        r?;
    }
    Ok(())
}

/// Statements SQLite refuses inside a transaction (`VACUUM`, `ATTACH`,
/// `DETACH`) or that manage it themselves: manual mode doesn't open one
/// for them.
fn outside_transaction(stmt: &str) -> bool {
    matches!(
        leading_keyword(stmt, &ScriptDialect::generic()).as_deref(),
        Some("vacuum" | "attach" | "detach" | "begin" | "commit" | "end" | "rollback" | "savepoint" | "release")
    )
}

fn run_stmt(c: &Connection, stmt: &mut rusqlite::Statement<'_>, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    {
        let n = stmt.column_count();
        if n > 0 {
            out.begin_result(
                stmt.columns()
                    .into_iter()
                    .map(|c| ResultColumn { name: c.name().to_string(), type_name: c.decl_type().unwrap_or("").to_string() })
                    .collect(),
            );
            let mut rows = stmt.raw_query();
            while let Some(row) = rows.next().map_err(|e| stmt_err(e, sql))? {
                out.push_row((0..n).map(|i| cell(row.get_ref_unwrap(i))).collect(), max_rows);
            }
        } else {
            // changes() keeps the last DML's count across DDL; only trust it
            // when this statement actually changed something.
            let before = total_changes(c)?;
            stmt.raw_execute().map_err(|e| stmt_err(e, sql))?;
            let changed = total_changes(c)? != before;
            out.push_affected(if changed { c.changes() } else { 0 });
        }
    }
    Ok(())
}

fn explain_script(c: &Connection, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    if analyze {
        out.messages.push("SQLite no mide cifras reales: se muestran los planes estimados.".into());
    }
    for stmt in split_statements(sql) {
        if plan::explainable(&stmt) {
            let mut q = c.prepare(&format!("EXPLAIN QUERY PLAN {stmt}")).map_err(err)?;
            let rows = q
                .query_map([], |r| Ok((r.get::<_, i64>(0)?, r.get::<_, i64>(1)?, r.get::<_, String>(3)?)))
                .map_err(err)?
                .collect::<rusqlite::Result<Vec<_>>>()
                .map_err(err)?;
            out.plans.push(plan::query_plan(&stmt, &rows));
        } else if !analyze {
            out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt)));
        }
        if analyze {
            run_script(c, &stmt, max_rows, out)?;
        }
    }
    Ok(())
}

fn total_changes(c: &Connection) -> Result<i64> {
    c.query_row("SELECT total_changes()", [], |r| r.get(0)).map_err(err)
}

fn cell(v: ValueRef<'_>) -> serde_json::Value {
    match v {
        ValueRef::Null => serde_json::Value::Null,
        ValueRef::Integer(i) => json_i64(i),
        ValueRef::Real(f) => json_f64(f),
        ValueRef::Text(t) => String::from_utf8_lossy(t).into_owned().into(),
        ValueRef::Blob(b) => json_bytes(b),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_yields_one_result_per_statement() {
        let c = Connection::open_in_memory().unwrap();
        let mut out = QueryOutcome::default();
        run_script(
            &c,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB);
             INSERT INTO t (b) VALUES (x'CAFE'), (NULL);
             CREATE INDEX i ON t (b);
             SELECT id, b, 1.5 AS f, 'x' AS s FROM t ORDER BY id;",
            1,
            &mut out,
        )
        .unwrap();
        let r = &out.results;
        assert_eq!(r.len(), 4);
        assert_eq!(r[1].rows_affected, Some(2));
        assert_eq!(r[2].rows_affected, Some(0));
        assert_eq!(r[3].rows, vec![vec![serde_json::json!(1), "0xCAFE".into(), serde_json::json!(1.5), "x".into()]]);
        assert_eq!(r[3].total_rows, 2);
        assert!(r[3].truncated);
    }

    #[test]
    fn a_failing_statement_keeps_earlier_results() {
        let c = Connection::open_in_memory().unwrap();
        let mut out = QueryOutcome::default();
        let e = run_script(&c, "SELECT 1; SELECT * FROM missing; SELECT 2;", 10, &mut out).unwrap_err();
        assert!(e.is_query());
        assert_eq!(out.results.len(), 1);
    }

    #[test]
    fn errors_carry_the_code_and_where_the_bad_token_is() {
        let c = Connection::open_in_memory().unwrap();
        let mut out = QueryOutcome::default();
        let script = "SELECT 1;\nSELEC 2;";
        let e = run_script(&c, script, 10, &mut out).unwrap_err().to_script_error();
        assert_eq!(e.code.as_deref(), Some("1"));
        assert_eq!(e.offset, Some(script.find("SELEC 2").unwrap()));
        assert!(e.message.contains("syntax error"), "{}", e.message);
        c.execute_batch("CREATE TABLE u (id INTEGER PRIMARY KEY); INSERT INTO u VALUES (1)").unwrap();
        let e = run_script(&c, "INSERT INTO u VALUES (1)", 10, &mut out).unwrap_err().to_script_error();
        assert_eq!(e.code.as_deref(), Some("1555"), "{e:?}");
    }

    #[test]
    fn manual_mode_opens_a_transaction_only_for_writes() {
        let c = Connection::open_in_memory().unwrap();
        let mut out = QueryOutcome::default();
        run_script_tx(&c, "CREATE TABLE t (a)", 10, true, &mut out).unwrap();
        assert!(!c.is_autocommit());
        c.execute_batch("ROLLBACK").unwrap();
        assert!(c.prepare("SELECT * FROM t").is_err(), "the CREATE was rolled back");
        run_script_tx(&c, "SELECT 1", 10, true, &mut out).unwrap();
        assert!(c.is_autocommit(), "a read opens nothing");
        run_script_tx(&c, "BEGIN; CREATE TABLE t (a); INSERT INTO t VALUES (1)", 10, true, &mut out).unwrap();
        run_script_tx(&c, "COMMIT", 10, true, &mut out).unwrap();
        assert!(c.is_autocommit());
        assert_eq!(c.query_row("SELECT count(*) FROM t", [], |r| r.get::<_, i64>(0)).unwrap(), 1);
    }

    #[test]
    fn dialect_keeps_trigger_bodies_and_brackets() {
        let d = SqliteDriver { info: info() };
        let units = d.split_script("CREATE TRIGGER tr AFTER INSERT ON t BEGIN UPDATE t SET a = 1; SELECT ';'; END; SELECT [a;b] FROM t;");
        assert_eq!(units.len(), 2, "{units:?}");
        assert_eq!(units[1].text, "SELECT [a;b] FROM t");
    }

    #[test]
    fn info_declares_what_list_objects_returns() {
        let i = info();
        assert_eq!(i.id, "sqlite");
        let ids: Vec<_> = i.object_kinds.iter().map(|k| k.id).collect();
        assert_eq!(ids, vec![kinds::TABLE, schema::VIRTUAL_TABLE, kinds::VIEW, kinds::TRIGGER]);
    }
}
