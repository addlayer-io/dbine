//! DuckDB, embedded through the `duckdb` crate. Its native library is
//! downloaded on first use and loaded at runtime (`loader`). The
//! connection is synchronous, so every call runs on a blocking thread and
//! `interrupter` stops the statement in flight.
//!
//! A DuckDB file can be opened only once per process, so sessions on the same
//! file share one database instance (each gets its own connection to it).

mod backup;
mod files;
mod index_usage;
mod loader;
mod monitor;
mod permissions;
mod plan;
mod schema;
mod transfer;

use dbine_driver::sql::{
    leading_keyword, quote_ident, select_top, split_script, strip_comments, Limit, Quote, ScriptDefaults, ScriptDialect, ScriptMode,
    StatementKind,
};
use dbine_driver::{
    json_bytes, json_f64, json_i64, json_u64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, Language, ObjectKindInfo, ObjectRef,
    QueryOutcome, ResultColumn, Result, ScriptError, Session, TableSchema, TxState,
};
use async_trait::async_trait;
use duckdb::types::Value;
use duckdb::{AccessMode, Config, Connection, InterruptHandle};
use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock, Weak};

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(DuckDbDriver { info: info(), files: false }), Arc::new(DuckDbDriver { info: files::info(), files: true })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "duckdb",
        name: "DuckDB",
        family: Family::Analytical,
        language: Language::Sql,
        dialect: "standard",
        default_port: 0,
        fields: vec![
            Field::file("Archivo de la base")
                .placeholder("/ruta/datos.duckdb")
                .help("Vacío o :memory: abre una base en memoria.")
                .default_value(":memory:"),
            Field::read_only(),
        ],
        databases_label: "Bases adjuntas",
        has_schemas: true,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::new(kinds::FUNCTION, "Macros", false, false, true),
            ObjectKindInfo::sequences(),
            ObjectKindInfo::types(),
        ],
    }
}

pub struct DuckDbDriver {
    info: DriverInfo,
    /// The "Archivos CSV / Parquet / JSON" preset: a folder of data files
    /// as views of an in-memory database.
    files: bool,
}

/// One open database per file; sessions clone connections from it.
struct Database {
    root: Mutex<Connection>,
}

/// The database for `key` (a file path, `:memory:`, or `files:<folder>`
/// for the files preset, which lives in memory).
fn open_database(key: &str, read_only: bool) -> Result<Arc<Database>> {
    let path = if key.starts_with("files:") { ":memory:" } else { key };
    static OPEN: OnceLock<Mutex<HashMap<String, Weak<Database>>>> = OnceLock::new();
    let mut open = OPEN.get_or_init(Default::default).lock().map_err(|_| Error::State("caché de DuckDB envenenada".into()))?;
    open.retain(|_, w| w.strong_count() > 0);
    if let Some(db) = open.get(key).and_then(Weak::upgrade) {
        return Ok(db);
    }
    let conn = if path == ":memory:" {
        Connection::open_in_memory()
    } else {
        let mut config = Config::default();
        if read_only {
            config = config.access_mode(AccessMode::ReadOnly).map_err(Error::connect)?;
        }
        Connection::open_with_flags(path, config)
    }
    .map_err(Error::connect)?;
    let db = Arc::new(Database { root: Mutex::new(conn) });
    open.insert(key.to_string(), Arc::downgrade(&db));
    Ok(db)
}

#[async_trait]
impl Driver for DuckDbDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn script_dialect(&self) -> ScriptDialect {
        dialect()
    }

    /// One statement per call on the tab's connection, which keeps its
    /// state (`USE`, `SET`, temp tables, variables, an open transaction).
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// The duckdb CLI goes on after an error unless `-bail`.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    fn supports_manual_transactions(&self) -> bool {
        true
    }

    /// A "database" is an attached catalog: creating one attaches a new
    /// file next to the main one, dropping it detaches it and deletes the
    /// file.
    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: true, drop_database: true, foreign_keys: true, monitor: true, ..Default::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(schema::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        schema::templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(schema::table_ddl(table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    /// The ART indexes and keys, without counters (see [`index_usage`]);
    /// the files preset has no indexes.
    fn supports_index_usage(&self) -> bool {
        !self.files
    }

    /// The files preset has nothing of its own to back up: its tables are
    /// views over the folder's files.
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        (!self.files).then(backup::spec)
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        if self.files {
            return Err(Error::Unsupported("los datos ya son los archivos de la carpeta: se copian tal cual".into()));
        }
        backup::script(action)
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        schema::sync_script(changes)
    }

    /// Schemas of the attached catalog the session `USE`s. DuckDB has no
    /// logins, so no owner and no grants. The files preset's database lives
    /// in memory and is rebuilt on each connection: nothing to keep there.
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        (!self.files).then(|| dbine_driver::SchemaSpec { owner: false, owner_kinds: dbine_driver::SchemaOwnerKinds::Both, cascade: true, privileges: Vec::new(), grant_option: true })
    }

    fn create_schema_script(&self, database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        schema_guard(self.files, owner)?;
        Ok(format!("CREATE SCHEMA {}", schema_path(database, name)))
    }

    fn drop_schema_script(&self, database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        schema_guard(self.files, None)?;
        Ok(format!("DROP SCHEMA {}{}", schema_path(database, name), if cascade { " CASCADE" } else { "" }))
    }

    /// DuckDB's Appender, a transaction per commit window (see [`transfer`]).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Only between sessions of one database instance (see [`transfer`]).
    fn supports_native_copy(&self, target: &str) -> bool {
        target == self.info.id
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

    /// `cfg.host` is the database file (empty or `:memory:` for an in-memory
    /// one); `database` is an attached catalog to `USE`.
    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        loader::ensure().await?;
        if self.files {
            return connect_files(cfg).await;
        }
        let path = match cfg.host.trim() {
            "" => ":memory:".to_string(),
            p => p.to_string(),
        };
        let read_only = cfg.read_only;
        let database = database.filter(|d| !d.is_empty()).map(str::to_string);
        let (db, conn, catalog) = blocking(move || {
            let db = open_database(&path, read_only)?;
            let conn = db.root.lock().map_err(|_| Error::State("conexión DuckDB envenenada".into()))?.try_clone().map_err(Error::connect)?;
            if let Some(d) = &database {
                conn.execute_batch(&format!("USE {}", quote_ident(Quote::Double, d))).map_err(Error::query)?;
            }
            let catalog: String = conn.query_row("SELECT current_database()", [], |r| r.get(0)).map_err(Error::query)?;
            Ok((db, conn, catalog))
        })
        .await?;
        let interrupt = conn.interrupt_handle();
        Ok(Box::new(DuckDbSession { _db: db, conn: Arc::new(Mutex::new(conn)), interrupt, catalog, read_only, tx: Tx::default() }))
    }
}

/// A folder of data files: every file becomes a view (recreated on each
/// connection, so new files show up).
async fn connect_files(cfg: &ConnectionConfig) -> Result<Box<dyn Session>> {
    let dir = files::folder(&cfg.host);
    if cfg.host.trim().is_empty() || !dir.is_dir() {
        return Err(Error::Connect(format!("no existe la carpeta «{}»", dir.display())));
    }
    let recursive = cfg.option("recursive") == Some("true");
    let (db, conn, catalog) = blocking(move || {
        let found = files::scan(&dir, recursive).map_err(|e| Error::Connect(format!("no se pudo leer la carpeta: {e}")))?;
        let db = open_database(&format!("files:{}", dir.display()), false)?;
        let conn = db.root.lock().map_err(|_| Error::State("conexión DuckDB envenenada".into()))?.try_clone().map_err(Error::connect)?;
        conn.execute_batch(&files::session_sql(&dir)).map_err(Error::connect)?;
        for (name, path, format) in found {
            // A file DuckDB can't read (empty, corrupt…) is skipped; it can
            // still be queried by path to see the error.
            if let Err(e) = conn.execute_batch(&files::view_sql(&name, &path, format)) {
                tracing::debug!("duckdb files: {}: {e}", path.display());
            }
        }
        let catalog: String = conn.query_row("SELECT current_database()", [], |r| r.get(0)).map_err(Error::query)?;
        Ok((db, conn, catalog))
    })
    .await?;
    let interrupt = conn.interrupt_handle();
    Ok(Box::new(DuckDbSession { _db: db, conn: Arc::new(Mutex::new(conn)), interrupt, catalog, read_only: cfg.read_only, tx: Tx::default() }))
}

pub struct DuckDbSession {
    /// Keeps the database instance open while the session lives.
    _db: Arc<Database>,
    conn: Arc<Mutex<Connection>>,
    interrupt: Arc<InterruptHandle>,
    /// The attached database this session browses.
    catalog: String,
    /// Refuses attaching/detaching databases (and deleting their files)
    /// even when the session isn't wrapped by `ReadOnlySession`.
    read_only: bool,
    /// Its transaction, as the statements run left it (DuckDB doesn't say).
    tx: Tx,
}

/// A session's transaction. DuckDB's C API doesn't tell whether one is
/// open, and probing with `BEGIN` aborts an open one, so it's followed
/// statement by statement: `BEGIN`/`START` open it, `COMMIT`/`END`/
/// `ROLLBACK`/`ABORT` close it, and after an error inside one a harmless
/// `SELECT 1` tells whether DuckDB aborted it.
#[derive(Debug, Clone, Copy)]
struct Tx {
    /// Autocommit off: a statement that writes opens a transaction.
    manual: bool,
    state: TxState,
}

impl Default for Tx {
    fn default() -> Self {
        Self { manual: false, state: TxState::Idle }
    }
}

/// DuckDB's script rules: PostgreSQL's parser (dollar quotes, `E'…'`,
/// nested comments).
fn dialect() -> ScriptDialect {
    ScriptDialect::postgres()
}

/// Whether a statement of `text` is a `USE` (the only one that switches
/// the session's catalog).
fn switches_catalog(text: &str) -> bool {
    let d = dialect();
    dbine_driver::sql::split_script(text, &d)
        .iter()
        .any(|s| dbine_driver::sql::leading_keyword(&s.text, &d).as_deref() == Some("use"))
}

async fn blocking<T, F>(f: F) -> Result<T>
where
    T: Send + 'static,
    F: FnOnce() -> Result<T> + Send + 'static,
{
    tokio::task::spawn_blocking(f).await.map_err(|e| Error::State(e.to_string()))?
}

impl DuckDbSession {
    /// `COMMIT` / `ROLLBACK` when a transaction is open (a failed one
    /// commits as a rollback).
    async fn end_transaction(&mut self, sql: &'static str) -> Result<()> {
        if self.tx.state == TxState::Idle {
            return Ok(());
        }
        let r = self.with(move |c| c.execute_batch(sql).map_err(duck_error)).await;
        if !matches!(r, Err(Error::Cancelled)) {
            self.tx.state = TxState::Idle;
        }
        r
    }

    async fn with<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        blocking(move || {
            let c = conn.lock().map_err(|_| Error::State("conexión DuckDB envenenada".into()))?;
            f(&c)
        })
        .await
    }

    /// Rows of a catalog query with one string parameter per `?`.
    async fn strings(&self, sql: &'static str, params: Vec<String>, ncols: usize) -> Result<Vec<Vec<Option<String>>>> {
        self.with(move |c| {
            let mut stmt = c.prepare(sql).map_err(Error::query)?;
            let rows = stmt
                .query_map(duckdb::params_from_iter(params.iter()), |r| {
                    (0..ncols).map(|i| r.get::<_, Option<String>>(i)).collect::<duckdb::Result<Vec<_>>>()
                })
                .map_err(Error::query)?;
            rows.collect::<duckdb::Result<Vec<_>>>().map_err(Error::query)
        })
        .await
    }

    fn params(&self, obj: &ObjectRef) -> Vec<String> {
        vec![self.catalog.clone(), obj.schema().unwrap_or("main").to_string(), obj.name.clone()]
    }

    /// `CREATE SEQUENCE` with the schema (the catalog's `sql` leaves it out).
    async fn sequence_definition(&self, obj: &ObjectRef) -> Result<Option<String>> {
        let rows = self
            .strings(
                "SELECT start_value::VARCHAR, increment_by::VARCHAR, min_value::VARCHAR, max_value::VARCHAR, cycle::VARCHAR
                 FROM duckdb_sequences() WHERE database_name = ?1 AND schema_name = ?2 AND sequence_name = ?3",
                self.params(obj),
                5,
            )
            .await?;
        Ok(rows.into_iter().next().map(|r| {
            let v = |i: usize| r[i].clone().unwrap_or_default();
            schema::sequence_sql(obj.schema().unwrap_or("main"), &obj.name, &v(0), &v(1), &v(2), &v(3), v(4) == "true")
        }))
    }

    /// `CREATE TYPE … AS <type>`: what the type stands for, as `typeof`
    /// spells it (ENUM labels, STRUCT fields…).
    async fn type_definition(&self, obj: &ObjectRef) -> Result<Option<String>> {
        let exists = self
            .strings(
                "SELECT type_name FROM duckdb_types() WHERE database_name = ?1 AND schema_name = ?2 AND type_name = ?3 AND NOT internal",
                self.params(obj),
                1,
            )
            .await?;
        if exists.is_empty() {
            return Ok(None);
        }
        let name = dbine_driver::sql::qualified_name(Quote::Double, Some(obj.schema().unwrap_or("main")), &obj.name);
        let catalog = quote_ident(Quote::Double, &self.catalog);
        let sql = format!("SELECT typeof(CAST(NULL AS {catalog}.{name}))");
        let ty: Option<String> = self.with(move |c| c.query_row(&sql, [], |r| r.get(0)).map_err(Error::query)).await?;
        Ok(ty.map(|t| format!("CREATE TYPE {name} AS {t};")))
    }
}

#[async_trait]
impl Session for DuckDbSession {
    async fn server_version(&mut self) -> Result<String> {
        let v = self.with(|c| c.version().map_err(Error::query)).await?;
        Ok(format!("DuckDB {v}"))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self
            .strings("SELECT database_name FROM duckdb_databases() WHERE NOT internal ORDER BY database_name", vec![], 1)
            .await?;
        Ok(rows.into_iter().filter_map(|mut r| r.remove(0)).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let rows = self
            .strings(
                "SELECT 'table', schema_name, table_name FROM duckdb_tables() WHERE database_name = ?1 AND NOT internal
                 UNION ALL
                 SELECT 'view', schema_name, view_name FROM duckdb_views() WHERE database_name = ?1 AND NOT internal
                 UNION ALL
                 SELECT DISTINCT 'function', schema_name, function_name FROM duckdb_functions()
                   WHERE database_name = ?1 AND NOT internal AND function_type IN ('macro', 'table_macro')
                 UNION ALL
                 SELECT 'sequence', schema_name, sequence_name FROM duckdb_sequences() WHERE database_name = ?1
                 UNION ALL
                 SELECT 'type', schema_name, type_name FROM duckdb_types() WHERE database_name = ?1 AND NOT internal
                 ORDER BY 2, 3",
                vec![self.catalog.clone()],
                3,
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let mut r = r.into_iter();
                let kind = r.next().flatten().unwrap_or_default();
                DbObject { kind, schema: r.next().flatten(), name: r.next().flatten().unwrap_or_default(), parent: None }
            })
            .collect())
    }

    /// Every schema of the open database, so an empty one (just made with
    /// "Nuevo esquema…") shows too. DuckDB's own (`information_schema`,
    /// `pg_catalog`) live in the `system` catalog, not here.
    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        let rows = self.strings("SELECT schema_name FROM duckdb_schemas() WHERE database_name = ?1 ORDER BY 1", vec![self.catalog.clone()], 1).await?;
        Ok(Some(rows.into_iter().filter_map(|mut r| r.remove(0)).map(|name| dbine_driver::SchemaInfo { name, system: false }).collect()))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let rows = self
            .strings(
                "SELECT c.column_name, c.data_type, c.is_nullable::VARCHAR, c.column_default,
                        (c.column_name IN (SELECT unnest(k.constraint_column_names) FROM duckdb_constraints() k
                          WHERE k.database_name = ?1 AND k.schema_name = ?2 AND k.table_name = ?3
                            AND k.constraint_type = 'PRIMARY KEY'))::VARCHAR
                 FROM duckdb_columns() c
                 WHERE c.database_name = ?1 AND c.schema_name = ?2 AND c.table_name = ?3
                 ORDER BY c.column_index",
                self.params(obj),
                5,
            )
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| {
                let default_value = r[3].clone();
                ColumnInfo {
                    name: r[0].clone().unwrap_or_default(),
                    data_type: r[1].clone().unwrap_or_default(),
                    nullable: r[2].as_deref() == Some("true"),
                    primary_key: r[4].as_deref() == Some("true"),
                    auto_increment: default_value.as_deref().is_some_and(|d| d.starts_with("nextval(")),
                    default_value,
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        match obj.kind.as_str() {
            kinds::SEQUENCE => return self.sequence_definition(obj).await,
            kinds::TYPE => return self.type_definition(obj).await,
            _ => {}
        }
        let sql = match obj.kind.as_str() {
            kinds::TABLE => {
                "SELECT sql FROM duckdb_tables() WHERE database_name = ?1 AND schema_name = ?2 AND table_name = ?3"
            }
            kinds::VIEW => "SELECT sql FROM duckdb_views() WHERE database_name = ?1 AND schema_name = ?2 AND view_name = ?3",
            kinds::FUNCTION => {
                "SELECT 'CREATE MACRO ' || ?2 || '.' || function_name || '(' || array_to_string(parameters, ', ') || ') AS '
                        || CASE WHEN function_type = 'table_macro' THEN 'TABLE ' ELSE '' END || macro_definition || ';'
                 FROM duckdb_functions()
                 WHERE database_name = ?1 AND schema_name = ?2 AND function_name = ?3 AND NOT internal
                   AND function_type IN ('macro', 'table_macro')"
            }
            _ => return Ok(None),
        };
        let rows = self.strings(sql, self.params(obj), 1).await?;
        let defs: Vec<String> = rows.into_iter().filter_map(|mut r| r.remove(0)).collect();
        Ok((!defs.is_empty()).then(|| defs.join("\n\n")))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let text = text.to_string();
        let fork = out.fork();
        let mut tx = self.tx;
        let catalog = self.catalog.clone();
        let (mut local, res, tx) = self
            .with(move |c| {
                let mut local = fork;
                let res = run_script_tx(c, &text, max_rows, &mut tx, &mut local);
                // `USE other` switches the catalog: the tab follows it.
                if switches_catalog(&text) {
                    if let Ok(now) = c.query_row("SELECT current_database()", [], |r| r.get::<_, String>(0)) {
                        if now != catalog {
                            local.database = Some(now);
                        }
                    }
                }
                Ok((local, res, tx))
            })
            .await?;
        self.tx = tx;
        if let Some(db) = local.database.take() {
            self.catalog = db.clone();
            local.database = Some(db);
        }
        out.merge(local);
        res
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        Ok(Some(self.tx.state))
    }

    /// Off: the next statement that writes opens a transaction, which stays
    /// open until Commit / Rollback. On: one still open is committed (the
    /// UI asks Commit / Rollback first), so later statements don't join it.
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if on {
            self.end_transaction("COMMIT").await?;
        }
        self.tx.manual = !on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.end_transaction("COMMIT").await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.end_transaction("ROLLBACK").await
    }

    /// Plans per statement. Estimated: `EXPLAIN (FORMAT JSON)`, nothing
    /// runs. Actual: each statement runs as with `execute`; a read then
    /// runs again under `EXPLAIN (ANALYZE, FORMAT JSON)` for its figures,
    /// while a write gets its estimated plan before running (EXPLAIN
    /// ANALYZE would apply it twice).
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let text = text.to_string();
        let fork = out.fork();
        let (local, res) = self
            .with(move |c| {
                let mut local = fork;
                let res = explain_script(c, &text, analyze, max_rows, &mut local);
                Ok((local, res))
            })
            .await?;
        out.merge(local);
        res
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        // The registry's weak reference doesn't count: the others are sessions.
        let sessions = Arc::strong_count(&self._db);
        self.with(move |c| Ok(monitor::snapshot(c, sessions))).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let h = self.interrupt.clone();
        Some(Arc::new(move || h.interrupt()))
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let db = vec![self.catalog.clone()];
        let tables = self
            .strings(
                "SELECT schema_name, table_name, comment FROM duckdb_tables()
                 WHERE database_name = ?1 AND NOT internal AND NOT temporary ORDER BY 1, 2",
                db.clone(),
                3,
            )
            .await?;
        let columns = self
            .strings(
                "SELECT schema_name, table_name, column_name, data_type, is_nullable::VARCHAR, column_default, comment
                 FROM duckdb_columns() WHERE database_name = ?1 AND NOT internal ORDER BY 1, 2, column_index",
                db.clone(),
                7,
            )
            .await?;
        let constraints = self
            .strings(
                "SELECT schema_name, table_name, constraint_type, constraint_name,
                        array_to_string(constraint_column_names, chr(31)), referenced_table,
                        array_to_string(referenced_column_names, chr(31)), expression
                 FROM duckdb_constraints()
                 WHERE database_name = ?1 AND constraint_type IN ('PRIMARY KEY', 'UNIQUE', 'FOREIGN KEY', 'CHECK')
                 ORDER BY 1, 2, constraint_index",
                db.clone(),
                8,
            )
            .await?;
        let indexes = self
            .strings(
                "SELECT schema_name, table_name, index_name, is_unique::VARCHAR, expressions::VARCHAR, sql
                 FROM duckdb_indexes() WHERE database_name = ?1 AND NOT is_primary ORDER BY 1, 2, 3",
                db,
                6,
            )
            .await?;
        Ok(schema::assemble(schema::Catalog { tables, columns, constraints, indexes }))
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let tables = self.database_schema().await?;
        let found = tables.iter().find(|t| t.name == table.name && (table.schema().is_none() || t.schema.as_deref() == table.schema()));
        Ok(Some(index_usage::report(found)))
    }

    /// Attaches `<dir of the main file>/<name>.duckdb` (a new in-memory
    /// catalog when the main database is in memory).
    async fn create_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
        }
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Query("falta el nombre de la base".into()));
        }
        let main_path = self
            .strings("SELECT path FROM duckdb_databases() WHERE database_name = current_database()", vec![], 1)
            .await?
            .into_iter()
            .next()
            .and_then(|mut r| r.remove(0))
            .filter(|p| !p.is_empty() && p != ":memory:");
        let target = match main_path {
            Some(p) => {
                let dir = std::path::Path::new(&p).parent().map(|d| d.to_path_buf()).unwrap_or_default();
                let file = dir.join(format!("{name}.duckdb"));
                if file.exists() {
                    return Err(Error::Query(format!("ya existe el archivo {}", file.display())));
                }
                file.to_string_lossy().to_string()
            }
            None => ":memory:".to_string(),
        };
        let sql = format!("ATTACH '{}' AS {}", target.replace('\'', "''"), quote_ident(Quote::Double, &name));
        self.with(move |c| c.execute_batch(&sql).map_err(Error::query)).await
    }

    /// Detaches the catalog and deletes its file (and WAL).
    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden borrar bases.".into()));
        }
        if name == self.catalog {
            return Err(Error::Query("no se puede borrar la base de esta sesión".into()));
        }
        let rows = self
            .strings(
                "SELECT path, (database_name = (SELECT database_name FROM duckdb_databases() WHERE NOT internal
                                                ORDER BY database_oid LIMIT 1))::VARCHAR
                 FROM duckdb_databases() WHERE database_name = ?1 AND NOT internal",
                vec![name.to_string()],
                2,
            )
            .await?;
        let Some(row) = rows.into_iter().next() else {
            return Err(Error::Query(format!("no existe la base {name}")));
        };
        if row[1].as_deref() == Some("true") {
            return Err(Error::Query("no se puede borrar la base principal: es el archivo de la conexión".into()));
        }
        let sql = format!("DETACH DATABASE {}", quote_ident(Quote::Double, name));
        self.with(move |c| c.execute_batch(&sql).map_err(Error::query)).await?;
        if let Some(p) = row[0].clone().filter(|p| !p.is_empty() && p != ":memory:") {
            std::fs::remove_file(&p).map_err(|e| Error::Query(format!("se desadjuntó, pero no se pudo borrar {p}: {e}")))?;
            let _ = std::fs::remove_file(format!("{p}.wal"));
        }
        Ok(())
    }

    /// Typed cells read by column type, streaming (see [`transfer`]).
    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        let spec = spec.clone();
        let catalog = self.catalog.clone();
        self.with(move |c| transfer::read(c, &catalog, &spec, &sink)).await
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

    /// No logins: only a read-only database stops the writes (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        let database = database.map(str::to_string);
        self.with(move |c| Ok(permissions::check(c, database.as_deref()))).await
    }
}

/// What "Nuevo esquema…" / "Borrar esquema…" can't do in DuckDB.
/// `"catalog"."schema"`: the catalog the explorer menu was opened on, so the
/// script lands there whatever the session `USE`s (a bare name without it).
fn schema_path(database: Option<&str>, name: &str) -> String {
    match database {
        Some(d) => format!("{}.{}", quote_ident(Quote::Double, d), quote_ident(Quote::Double, name)),
        None => quote_ident(Quote::Double, name),
    }
}

fn schema_guard(files: bool, owner: Option<&str>) -> Result<()> {
    if files {
        return Err(Error::Unsupported("la base de una carpeta de archivos vive en memoria: sus esquemas no se guardan".into()));
    }
    if owner.is_some() {
        return Err(Error::Unsupported("DuckDB no tiene usuarios: sus esquemas no tienen dueño".into()));
    }
    Ok(())
}

/// Statements that always return a result set.
const QUERIES: &[&str] =
    &["select", "with", "from", "values", "table", "show", "describe", "desc", "summarize", "pragma", "explain", "call", "pivot", "unpivot"];

fn first_keyword(stmt: &str) -> String {
    leading_keyword(stmt, &dialect()).unwrap_or_default()
}

/// The statements of a script, comments dropped (for plans).
fn split_statements(sql: &str) -> Vec<String> {
    let d = dialect();
    split_script(sql, &d)
        .into_iter()
        .filter(|s| s.kind != StatementKind::ClientCommand)
        .map(|s| strip_comments(&s.text, &d, false).trim().to_string())
        .filter(|s| !s.is_empty())
        .collect()
}

/// Words that never open a transaction in manual mode: reads, transaction
/// control, and what DuckDB refuses inside one.
const NO_TRANSACTION: &[&str] = &[
    "select", "with", "from", "values", "table", "show", "describe", "desc", "summarize", "explain", "pivot", "unpivot", "begin", "start",
    "commit", "end", "rollback", "abort", "use", "set", "reset", "pragma", "checkpoint", "vacuum", "attach", "detach", "load", "install",
    "prepare", "deallocate",
];

/// A failed statement: DuckDB's error class as the code ("Parser",
/// "Catalog", "Constraint"…) and, from its `LINE n:` excerpt, the line and
/// the position of the caret. `base`: where the statement starts in `script`.
fn statement_error(e: duckdb::Error, script: &str, base: usize) -> Error {
    let err = duck_error(e);
    let Error::Query(msg) = err else { return err };
    let mut se = ScriptError::new(msg.clone());
    if let Some((class, _)) = msg.split_once(" Error: ") {
        if !class.is_empty() && class.chars().all(|c| c.is_ascii_alphanumeric()) {
            se = se.with_code(class);
        }
    }
    let base_line = script[..base].matches('\n').count() as u32;
    let mut lines = msg.lines();
    while let Some(l) = lines.next() {
        let Some((n, shown)) = l.strip_prefix("LINE ").and_then(|r| r.split_once(": ")) else { continue };
        let Ok(n) = n.parse::<u32>() else { continue };
        se = se.at_line(base_line + n);
        // The caret's column, when the excerpt is the line itself (DuckDB
        // cuts long lines).
        let prefix = l.len() - shown.len();
        let caret = lines.next().and_then(|c| c.find('^')).and_then(|c| c.checked_sub(prefix));
        let line_start = script[base..].split_inclusive('\n').take(n as usize - 1).map(str::len).sum::<usize>() + base;
        let actual = script[line_start..].lines().next().unwrap_or("");
        if let Some(col) = caret.filter(|&c| actual.starts_with(shown.trim_end()) && c <= actual.len()) {
            se = se.at_offset(line_start + col);
        }
        break;
    }
    se.into()
}

fn run_script(c: &Connection, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    run_script_tx(c, sql, max_rows, &mut Tx::default(), out)
}

/// Run every statement of `sql`, following the session's transaction (see
/// [`Tx`]).
fn run_script_tx(c: &Connection, sql: &str, max_rows: usize, tx: &mut Tx, out: &mut QueryOutcome) -> Result<()> {
    for unit in split_script(sql, &dialect()).into_iter().filter(|s| s.kind != StatementKind::ClientCommand) {
        let kw = first_keyword(&unit.text);
        if tx.manual && tx.state == TxState::Idle && !kw.is_empty() && !NO_TRANSACTION.contains(&kw.as_str()) {
            c.execute_batch("BEGIN TRANSACTION").map_err(duck_error)?;
            tx.state = TxState::Open;
        }
        let before = out.results.len();
        let r = run_statement(c, &unit.text, &kw, max_rows, out).map_err(|e| statement_error(e, sql, unit.start));
        // A query cancelled or failed mid-way leaves no empty grid.
        if r.is_err() && out.results.len() > before && out.results[before..].iter().all(|r| r.total_rows == 0 && r.rows_affected.is_none()) {
            out.results.truncate(before);
        }
        match (&r, kw.as_str()) {
            (Ok(()), "begin" | "start") => tx.state = TxState::Open,
            (_, "commit" | "end" | "rollback" | "abort") => tx.state = TxState::Idle,
            (Err(Error::Cancelled), _) => {}
            (Err(_), _) if tx.state == TxState::Open => {
                // Execution errors abort the transaction; parser and binder
                // errors don't.
                if c.execute_batch("SELECT 1").is_err() {
                    tx.state = TxState::Failed;
                }
            }
            _ => {}
        }
        r?;
    }
    Ok(())
}

fn run_statement(c: &Connection, stmt_sql: &str, kw: &str, max_rows: usize, out: &mut QueryOutcome) -> duckdb::Result<()> {
    {
        let mut stmt = c.prepare(stmt_sql)?;
        let changed = stmt.execute([])?;
        let names = stmt.column_names();
        // DML without RETURNING and DDL come back as a lone "Count" (or
        // "Success") column, or no columns at all.
        let status_only = names.is_empty() || (names.len() == 1 && (names[0] == "Count" || names[0] == "Success"));
        if status_only && !QUERIES.contains(&kw) {
            out.push_affected(changed as u64);
            return Ok(());
        }
        let types: Vec<String> = (0..names.len()).map(|i| type_name(&stmt, i)).collect();
        out.begin_result(names.into_iter().zip(types).map(|(name, type_name)| ResultColumn { name, type_name }).collect());
        let n = stmt.column_count();
        let mut rows = stmt.raw_query();
        while let Some(row) = rows.next()? {
            out.push_row((0..n).map(|i| cell(row.get_ref_unwrap(i).to_owned())).collect(), max_rows);
        }
    }
    Ok(())
}

fn explain_script(c: &Connection, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    use plan::StmtKind;
    for stmt in split_statements(sql) {
        match (analyze, plan::classify(&stmt)) {
            (false, StmtKind::Other) => out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt))),
            (false, _) => out.plans.push(plan_of(c, &stmt, false)?),
            (true, StmtKind::Read) => {
                run_script(c, &stmt, max_rows, out)?;
                out.plans.push(plan_of(c, &stmt, true)?);
            }
            (true, StmtKind::Other) => run_script(c, &stmt, max_rows, out)?,
            (true, StmtKind::Write) => {
                out.plans.push(plan_of(c, &stmt, false)?);
                run_script(c, &stmt, max_rows, out)?;
            }
        }
    }
    Ok(())
}

/// One statement's plan: the `explain_value` column of the EXPLAIN row.
fn plan_of(c: &Connection, stmt: &str, actual: bool) -> Result<dbine_driver::Plan> {
    let q = if actual { format!("EXPLAIN (ANALYZE, FORMAT JSON) {stmt}") } else { format!("EXPLAIN (FORMAT JSON) {stmt}") };
    let raw: String = c.query_row(&q, [], |r| r.get(1)).map_err(duck_error)?;
    plan::duck_json(stmt, &raw, actual).map_err(Error::Query)
}

fn type_name(stmt: &duckdb::Statement<'_>, i: usize) -> String {
    format!("{:?}", stmt.column_logical_type(i).id()).to_uppercase()
}

/// An interrupted statement reads as a cancellation, not an SQL error.
fn duck_error(e: duckdb::Error) -> Error {
    let msg = e.to_string();
    if msg.contains("INTERRUPT") || msg.contains("Interrupted") {
        Error::Cancelled
    } else {
        Error::Query(msg)
    }
}

/// A cell: scalars as JSON scalars, nested values as a compact JSON string.
fn cell(v: Value) -> serde_json::Value {
    match v {
        Value::List(_) | Value::Array(_) | Value::Struct(_) | Value::Map(_) => nested(v).to_string().into(),
        Value::Union(inner) => cell(*inner),
        v => nested(v),
    }
}

fn nested(v: Value) -> serde_json::Value {
    use serde_json::Value as J;
    match v {
        Value::Null => J::Null,
        Value::Boolean(b) => b.into(),
        Value::TinyInt(i) => i.into(),
        Value::SmallInt(i) => i.into(),
        Value::Int(i) => i.into(),
        Value::BigInt(i) => json_i64(i),
        Value::HugeInt(i) => i64::try_from(i).map_or_else(|_| i.to_string().into(), json_i64),
        Value::UHugeInt(i) => u64::try_from(i).map_or_else(|_| i.to_string().into(), json_u64),
        Value::UTinyInt(i) => i.into(),
        Value::USmallInt(i) => i.into(),
        Value::UInt(i) => i.into(),
        Value::UBigInt(i) => json_u64(i),
        Value::Float(f) => json_f64(f as f64),
        Value::Double(f) => json_f64(f),
        Value::Decimal(d) => d.to_string().into(),
        Value::Timestamp(unit, t) => timestamp(unit.to_micros(t)).into(),
        Value::Text(s) | Value::Enum(s) => s.into(),
        Value::Blob(b) | Value::Geometry(b) => json_bytes(&b),
        Value::Date32(d) => chrono::DateTime::from_timestamp(d as i64 * 86_400, 0)
            .map_or_else(|| d.to_string(), |t| t.format("%Y-%m-%d").to_string())
            .into(),
        Value::Time64(unit, t) => time_of_day(unit.to_micros(t)).into(),
        Value::Interval { months, days, nanos } => interval(months, days, nanos).into(),
        Value::List(items) | Value::Array(items) => J::Array(items.into_iter().map(nested).collect()),
        Value::Struct(fields) => {
            J::Object(fields.iter().map(|(k, v)| (k.clone(), nested(v.clone()))).collect())
        }
        Value::Map(entries) => {
            // Keys may be any type; an object when they are all strings.
            if entries.keys().all(|k| matches!(k, Value::Text(_))) {
                J::Object(
                    entries
                        .iter()
                        .map(|(k, v)| (if let Value::Text(s) = k { s.clone() } else { String::new() }, nested(v.clone())))
                        .collect(),
                )
            } else {
                J::Array(entries.iter().map(|(k, v)| J::Array(vec![nested(k.clone()), nested(v.clone())])).collect())
            }
        }
        Value::Union(inner) => nested(*inner),
        other => format!("{other:?}").into(),
    }
}

/// `2024-01-31 13:45:00[.ffffff]` (UTC for TIMESTAMPTZ).
fn timestamp(micros: i64) -> String {
    match chrono::DateTime::from_timestamp_micros(micros) {
        Some(t) if micros % 1_000_000 == 0 => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        Some(t) => t.format("%Y-%m-%d %H:%M:%S%.6f").to_string(),
        None => micros.to_string(),
    }
}

fn time_of_day(micros: i64) -> String {
    let secs = micros.div_euclid(1_000_000);
    let frac = micros.rem_euclid(1_000_000);
    let s = format!("{:02}:{:02}:{:02}", secs / 3600, secs / 60 % 60, secs % 60);
    if frac == 0 {
        s
    } else {
        format!("{s}.{frac:06}")
    }
}

fn interval(months: i32, days: i32, nanos: i64) -> String {
    let mut parts = Vec::new();
    if months != 0 {
        parts.push(format!("{} years {} months", months / 12, months % 12));
    }
    if days != 0 {
        parts.push(format!("{days} days"));
    }
    if nanos != 0 || parts.is_empty() {
        let neg = if nanos < 0 { "-" } else { "" };
        parts.push(format!("{neg}{}", time_of_day((nanos / 1000).abs())));
    }
    parts.join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn run(c: &Connection, sql: &str, max: usize) -> (QueryOutcome, Result<()>) {
        let mut out = QueryOutcome::default();
        let r = run_script(c, sql, max, &mut out);
        (out, r)
    }

    pub(crate) fn memory() -> Connection {
        loader::ensure_blocking();
        Connection::open_in_memory().unwrap()
    }

    #[test]
    fn script_yields_one_result_per_statement() {
        let c = memory();
        let (out, r) = run(
            &c,
            "CREATE TABLE t (id INTEGER PRIMARY KEY, b BLOB, d DATE, ts TIMESTAMP, l INTEGER[], s STRUCT(a INT), x DECIMAL(10,2));
             INSERT INTO t VALUES (1, '\\xCA\\xFE'::BLOB, DATE '2024-01-31', TIMESTAMP '2024-01-31 13:45:00', [1,2], {'a': 1}, 1.50), (2, NULL, NULL, NULL, NULL, NULL, NULL);
             UPDATE t SET x = 2 WHERE id = 2;
             SELECT * FROM t ORDER BY id;",
            1,
        );
        r.unwrap();
        let res = &out.results;
        assert_eq!(res.len(), 4, "{res:?}");
        assert_eq!(res[0].rows_affected, Some(0));
        assert_eq!(res[1].rows_affected, Some(2));
        assert_eq!(res[2].rows_affected, Some(1));
        assert_eq!(res[3].columns.len(), 7);
        assert_eq!(
            res[3].rows[0],
            vec![
                serde_json::json!(1),
                "0xCAFE".into(),
                "2024-01-31".into(),
                "2024-01-31 13:45:00".into(),
                "[1,2]".into(),
                "{\"a\":1}".into(),
                "1.50".into()
            ]
        );
        assert_eq!(res[3].total_rows, 2);
        assert!(res[3].truncated);
    }

    #[test]
    fn returning_and_show_are_result_sets() {
        let c = memory();
        let (out, r) = run(&c, "CREATE TABLE t (a INT); INSERT INTO t VALUES (7) RETURNING a; SHOW TABLES;", 10);
        r.unwrap();
        assert_eq!(out.results[1].rows, vec![vec![serde_json::json!(7)]]);
        assert_eq!(out.results[2].rows, vec![vec![serde_json::json!("t")]]);
    }

    #[test]
    fn a_failing_statement_keeps_earlier_results() {
        let c = memory();
        let (out, r) = run(&c, "SELECT 1; SELECT * FROM missing; SELECT 2;", 10);
        assert!(r.is_err_and(|e| e.is_query()));
        assert_eq!(out.results.len(), 1);
    }

    #[test]
    fn errors_carry_class_line_and_caret() {
        let c = memory();
        let script = "SELECT 1;\nSELECT *\n  FROM missing;";
        let e = run(&c, script, 10).1.unwrap_err().to_script_error();
        assert_eq!(e.code.as_deref(), Some("Catalog"), "{e:?}");
        assert_eq!(e.line, Some(3));
        assert_eq!(e.offset, Some(script.find("missing").unwrap()), "{e:?}");
        let e = run(&c, "selec 1", 10).1.unwrap_err().to_script_error();
        assert_eq!((e.code.as_deref(), e.line, e.offset), (Some("Parser"), Some(1), Some(0)));
    }

    #[test]
    fn dollar_quotes_and_escapes_stay_in_their_statement() {
        let c = memory();
        let (out, r) = run(&c, "SELECT $$a;b$$ AS x; SELECT $t$c;'d$t$; SELECT E'it\\'s;' AS y", 10);
        r.unwrap();
        assert_eq!(out.results.len(), 3, "{:?}", out.results);
        assert_eq!(out.results[0].rows[0][0], serde_json::json!("a;b"));
        assert_eq!(out.results[2].rows[0][0], serde_json::json!("it's;"));
    }

    #[test]
    fn transactions_are_followed() {
        let c = memory();
        let mut tx = Tx { manual: true, ..Default::default() };
        let mut out = QueryOutcome::default();
        let mut go = |sql: &str, tx: &mut Tx| run_script_tx(&c, sql, 10, tx, &mut out);
        go("SELECT 1", &mut tx).unwrap();
        assert_eq!(tx.state, TxState::Idle, "a read opens nothing");
        go("CREATE TABLE t (a INT PRIMARY KEY)", &mut tx).unwrap();
        assert_eq!(tx.state, TxState::Open);
        go("ROLLBACK", &mut tx).unwrap();
        assert_eq!(tx.state, TxState::Idle);
        assert!(go("SELECT * FROM t", &mut tx).is_err(), "rolled back");
        go("CREATE TABLE t (a INT PRIMARY KEY)", &mut tx).unwrap();
        go("INSERT INTO t VALUES (1)", &mut tx).unwrap();
        assert!(go("SELECT * FROM nope", &mut tx).is_err());
        assert_eq!(tx.state, TxState::Open, "a binder error doesn't abort");
        assert!(go("INSERT INTO t VALUES (1)", &mut tx).is_err());
        assert_eq!(tx.state, TxState::Failed);
        go("ROLLBACK", &mut tx).unwrap();
        let mut auto = Tx::default();
        go("BEGIN", &mut auto).unwrap();
        assert_eq!(auto.state, TxState::Open);
        go("COMMIT", &mut auto).unwrap();
        assert_eq!(auto.state, TxState::Idle);
    }

    #[test]
    fn temporal_formatting() {
        assert_eq!(time_of_day(3_723_000_001), "01:02:03.000001");
        assert_eq!(interval(14, 3, 0), "1 years 2 months 3 days");
        assert_eq!(timestamp(0), "1970-01-01 00:00:00");
    }

    #[test]
    fn schema_scripts() {
        let [db, files] = <[_; 2]>::try_from(drivers()).ok().unwrap();
        let spec = db.schema_spec().unwrap();
        assert!(!spec.owner && spec.cascade && spec.privileges.is_empty());
        assert_eq!(db.create_schema_script(None, "ven\"tas", None).unwrap(), r#"CREATE SCHEMA "ven""tas""#);
        assert!(matches!(db.create_schema_script(None, "v", Some("ana")), Err(Error::Unsupported(_))));
        assert_eq!(db.drop_schema_script(None, "v", false).unwrap(), r#"DROP SCHEMA "v""#);
        assert_eq!(db.drop_schema_script(None, "v", true).unwrap(), r#"DROP SCHEMA "v" CASCADE"#);
        // The menu's catalog qualifies the schema.
        assert_eq!(db.create_schema_script(Some("mi base"), "v", None).unwrap(), r#"CREATE SCHEMA "mi base"."v""#);
        assert_eq!(db.drop_schema_script(Some("b\"x"), "v", true).unwrap(), r#"DROP SCHEMA "b""x"."v" CASCADE"#);
        assert!(files.schema_spec().is_none());
        assert!(matches!(files.create_schema_script(None, "v", None), Err(Error::Unsupported(_))));
    }
}
