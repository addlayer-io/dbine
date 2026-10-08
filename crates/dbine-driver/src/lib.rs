//! The contract between DBine and its database drivers. Each engine lives
//! in its own crate under `crates/drivers/` and implements [`Driver`]; the
//! `dbine-drivers` crate registers them.
//!
//! One [`Driver`] per engine describes itself ([`DriverInfo`]) and opens
//! [`Session`]s, each a single live connection to one database (keyspace,
//! index group… whatever the engine's namespace is). Sessions are not
//! pooled on purpose: an editor tab needs its own connection so `USE`, temp
//! tables, `SET`s and open transactions persist between runs, and
//! cancelling can simply drop it.
//!
//! Results are always tabular ([`QueryOutcome`]); document and key-value
//! engines flatten what they return (top-level fields as columns).

pub mod alter;
pub mod backup;
pub mod config;
pub mod dependencies;
pub mod ddl;
pub mod error;
pub mod index_usage;
pub mod info;
pub mod keys;
pub mod filter;
pub mod model;
pub mod monitor;
pub mod permissions;
pub mod security;
pub mod plan;
pub mod profiler;
pub mod read_only;
pub mod runtime;
pub mod schema;
pub mod health;
pub mod search;
pub mod serde_static;
pub mod sql;
pub mod transfer;

pub use alter::{SyncScript, TableChange};
pub use backup::{BackupAction, BackupEntry, BackupSpec};
pub use config::ConnectionConfig;
pub use dependencies::{Confidence, DependencyReport, DependencyScan, DependencyTarget, Dependent, Mention, Relation};
pub use error::{Error, Result};
pub use index_usage::{IndexUsage, IndexUsageReport};
pub use info::{
    kinds, DatabaseProperties, DriverInfo, Family, Field, FieldChoices, FieldKind, FieldSection, FieldWhen, Language, ObjectKindInfo,
    PropertyInfo,
};
pub use filter::{ColumnFilter, FilterOp};
pub use keys::{KeyEntry, KeyPage, KeyScan, KeySearch, KeySyntax};
pub use model::{
    json_bytes, json_f64, json_i64, json_u64, ColumnInfo, DbObject, Message, MessageLevel, MessageSinkRef, ObjectRef, Plan,
    PlanNode, ProgressSinkRef, QueryOutcome, ResultColumn, RowSink, RowSinkRef, ScriptError, StatementEnd, StatementResult,
    TxState, RowChange, SchemaInfo,
};

pub use security::{Grant, Principal, PrincipalKind, SchemaOwnerKinds, SchemaSpec, SecurityAction, SecuritySpec};
pub use monitor::{BlockedSession, Metric, MetricUnit, MonitorSnapshot, MonitorTable, ServerProcess};
pub use permissions::{Access, Permissions};
pub use profiler::{ProfiledStatement, ProfilerMode, ProfilerOptions, ProfilerStarted};
pub use schema::{
    Capabilities, CheckDef, ColumnDef, CreateTemplate, DdlParts, DesignerSpec, ForeignKeyDef, IndexDef, KeyDef, TableSchema,
};

pub use sql::{ScriptDefaults, ScriptDialect, ScriptMode, ScriptStatement, StatementKind};

pub use transfer::{
    BatchBuilder, BatchSink, BatchSinkRef, BatchSource, BucketSum, Buckets, Cell, CloneScript, CloneTable, CopySpec, DeltaDepth, DeltaResult, DeltaSpec,
    LoadSpec, ReadSpec, RowBatch, TransferColumn,
};

pub use async_trait::async_trait;
use std::sync::Arc;

#[async_trait]
pub trait Driver: Send + Sync {
    fn info(&self) -> &DriverInfo;

    /// Short help on the query syntax, shown next to the editor (Spanish,
    /// plain text). Worth writing for non-SQL languages.
    fn query_help(&self) -> &'static str {
        ""
    }

    /// Its sessions implement [`Session::explain`] (the UI offers the plan
    /// buttons only then).
    fn supports_explain(&self) -> bool {
        false
    }

    /// Its sessions implement the profiler ([`Session::profiler_start`]);
    /// the UI offers "Profiler" on its databases only then.
    fn supports_profiler(&self) -> bool {
        false
    }

    /// "Ver dependencias…" works: the engine reports foreign keys or has
    /// objects with source to search ([`Session::dependents`]).
    fn supports_dependencies(&self) -> bool {
        self.capabilities().foreign_keys || !dependencies::code_kinds(self.info()).is_empty()
    }

    /// Indexes can be disabled and enabled again ([`Driver::index_toggle_script`]);
    /// the explorer and the "Índices" tab offer "Deshabilitar índice…" /
    /// "Habilitar índice…" on what [`Session::index_usage`] lists.
    fn supports_index_toggle(&self) -> bool {
        false
    }

    /// The statements that disable (`enable` false) or enable `index` of
    /// `table`, in the driver's language, with what the user should know
    /// first (a clustered index makes the table unreadable…). `index` is as
    /// [`Session::index_usage`] reported it: what can't be disabled (a
    /// primary key on most engines) is refused with [`Error::Unsupported`].
    fn index_toggle_script(&self, table: &ObjectRef, index: &IndexUsage, enable: bool) -> Result<SyncScript> {
        let _ = (table, index, enable);
        Err(Error::Unsupported("este motor no deshabilita índices".into()))
    }

    /// Its sessions implement [`Session::index_usage`]: the explorer lists a
    /// table's indexes with their usage, and "Índices…" opens the details.
    fn supports_index_usage(&self) -> bool {
        false
    }

    /// Its databases hold keys, too many to list at once: the explorer
    /// searches them on the server a page at a time ([`Session::scan_keys`])
    /// instead of listing them with `list_objects`. `None` for engines
    /// whose objects are a list of tables.
    fn key_search(&self) -> Option<KeySearch> {
        None
    }

    /// Database-level operations it offers (create / drop database,
    /// foreign keys for the ER diagram).
    fn capabilities(&self) -> Capabilities {
        Capabilities::default()
    }

    /// The table designer for this engine ("Nueva tabla", "Nueva
    /// colección", "Nuevo índice"…), or `None` when objects are only
    /// created by script.
    fn designer(&self) -> Option<DesignerSpec> {
        None
    }

    /// Starting scripts for creating the other kinds of objects (views,
    /// routines, triggers, sequences…), in the driver's language.
    fn create_templates(&self) -> Vec<CreateTemplate> {
        Vec::new()
    }

    /// Whether [`Driver::sync_script`] works: "Comparar esquemas" can apply
    /// the differences it finds.
    fn supports_schema_sync(&self) -> bool {
        false
    }

    /// The statements that apply schema changes (create, drop and alter
    /// tables), in an order that respects dependencies. SQL engines build it
    /// with [`alter::sync_script`] and their [`alter::AlterStyle`].
    fn sync_script(&self, _changes: &[TableChange]) -> Result<SyncScript> {
        Err(Error::Unsupported("este motor no aplica cambios de esquema".into()))
    }

    /// DDL in the driver's language for a table (or collection, index…):
    /// what the designer runs and what the script generator writes.
    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        let _ = (table, parts);
        Err(Error::Unsupported("este motor no genera DDL de tablas".into()))
    }

    /// Rows as a script in the driver's language that inserts them (SQL
    /// INSERTs, `insertMany`, `_bulk`, Redis commands…): copy, import and
    /// the database script use it. `target` is the table / collection.
    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        if self.info().language != Language::Sql {
            return Err(Error::Unsupported("este motor no genera scripts de inserción".into()));
        }
        let quote = match self.info().dialect {
            "mssql" | "sybase" => sql::Quote::Bracket,
            "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" => sql::Quote::Backtick,
            _ => sql::Quote::Double,
        };
        let flavor = ddl::SqlFlavor { quote, ..ddl::SqlFlavor::ansi() };
        Ok(ddl::insert_script(&flavor, target.schema(), &target.name, columns, rows, 100))
    }

    /// The browse query (`Session::browse_query`) restricted by the data
    /// grid's column filters. SQL engines add a WHERE with their own quoting
    /// and literals; other engines build their own filter, or say they can't
    /// (the grid then filters the rows it loaded).
    fn filtered_browse(&self, browse: &str, filters: &[ColumnFilter]) -> Result<String> {
        if filters.is_empty() {
            return Ok(browse.to_string());
        }
        if !matches!(self.info().language, Language::Sql | Language::Cql) {
            return Err(Error::Unsupported("este motor no filtra en el servidor".into()));
        }
        let dialect = self.info().dialect;
        let quote = match dialect {
            "mssql" | "sybase" => sql::Quote::Bracket,
            "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" => sql::Quote::Backtick,
            _ => sql::Quote::Double,
        };
        let bits = matches!(dialect, "mssql" | "sybase" | "oracle" | "db2" | "informix");
        let unicode = matches!(dialect, "mssql" | "sybase");
        let flavor = ddl::SqlFlavor { quote, ..ddl::SqlFlavor::ansi() };
        let literal = |v: &serde_json::Value| match v {
            serde_json::Value::String(t) if unicode => format!("N'{}'", t.replace('\'', "''")),
            serde_json::Value::Bool(b) if bits => (if *b { "1" } else { "0" }).to_string(),
            other => ddl::sql_literal(&flavor, other),
        };
        let style = filter::SqlFilterStyle {
            quote,
            literal: &literal,
            like: if dialect == "postgres" { "ILIKE" } else { "LIKE" },
            true_literal: if bits { "1" } else { "TRUE" },
            false_literal: if bits { "0" } else { "FALSE" },
        };
        let cond = filter::sql_condition(filters, &style)?;
        filter::insert_where(browse, &cond).ok_or_else(|| Error::Unsupported("no se pudo agregar el filtro a la consulta de este objeto".into()))
    }

    /// Rows edited in the results grid as code in the driver's language
    /// that applies the changes (`UPDATE … WHERE <key>` in SQL, `updateOne`
    /// in MongoDB, `HSET` in Redis…). DBine only shows / inserts it: the
    /// user runs it. `target` is the table / collection.
    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        if self.info().language != Language::Sql {
            return Err(Error::Unsupported("este motor no genera scripts de actualización".into()));
        }
        let quote = match self.info().dialect {
            "mssql" | "sybase" => sql::Quote::Bracket,
            "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" => sql::Quote::Backtick,
            _ => sql::Quote::Double,
        };
        let flavor = ddl::SqlFlavor { quote, ..ddl::SqlFlavor::ansi() };
        Ok(ddl::update_script(&flavor, target.schema(), &target.name, changes))
    }

    /// What the "Usuarios y permisos" tab offers (see [`security`]);
    /// `None`: the engine has no users/permissions DBine can manage.
    fn security(&self) -> Option<security::SecuritySpec> {
        None
    }

    /// The code that makes a change to users, roles or permissions, in the
    /// driver's language (`CREATE LOGIN…`, `GRANT…`, `db.createUser(…)`,
    /// `ACL SETUSER…`). DBine shows it and runs it only on the user's click.
    fn security_script(&self, action: &security::SecurityAction) -> Result<String> {
        let _ = action;
        Err(Error::Unsupported("este motor no administra usuarios desde DBine".into()))
    }

    /// What "Nuevo esquema…" / "Borrar esquema…" offer (see
    /// [`security::SchemaSpec`]); `None`: the engine has no schemas DBine
    /// creates as plain objects (none at all, or a schema is a user or a
    /// database there). With `Some`, [`Driver::create_schema_script`] and
    /// [`Driver::drop_schema_script`] work, and so does
    /// [`Driver::schema_grant_script`] when `privileges` isn't empty.
    ///
    /// The schema methods get `database`: the database the explorer menu
    /// was opened on (`None` when there's none), for engines where a
    /// schema's path depends on it (a Dremio source, a Flight SQL catalog).
    ///
    /// "Nuevo esquema…" builds one script, in this order: the create, each
    /// grant, then the owner change when [`Driver::schema_owner_script`]
    /// gives one (see there).
    fn schema_spec(&self) -> Option<security::SchemaSpec> {
        None
    }

    /// The advanced options of "Nueva base de datos": the clauses of the
    /// engine's CREATE DATABASE (collation, files, owner, encoding…), in
    /// order. Empty: the database is created with just its name. Their
    /// values reach [`Driver::create_database_script`] and
    /// [`Session::create_database_with`] by `key`.
    fn create_database_fields(&self) -> Vec<Field> {
        Vec::new()
    }

    /// The code that creates database `name` with `options` (field key →
    /// value; a missing or empty value means the server's default), as
    /// [`Session::create_database_with`] runs it: what "Ver script" shows.
    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        let _ = (name, options);
        Err(Error::Unsupported("este motor no genera el script de creación de una base".into()))
    }

    /// The code that applies `changes` (field key → new value, only the
    /// ones the user changed; see [`Session::database_properties`]) to
    /// `database`, as [`Session::alter_database`] runs it.
    fn alter_database_script(&self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        let _ = (database, changes);
        Err(Error::Unsupported("este motor no modifica las propiedades de una base".into()))
    }

    /// The code that creates schema `name` (quoted as the engine needs) in
    /// `database`, owned by `owner` when given. `owner` comes only when the
    /// spec says `owner` and [`Driver::schema_owner_script`] returned
    /// `None` for it. DBine shows it and runs it only on the user's click.
    fn create_schema_script(&self, database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        let _ = (database, name, owner);
        Err(Error::Unsupported("este motor no crea esquemas desde DBine".into()))
    }

    /// The code that hands the new schema `name` to `owner`, run after the
    /// create and the grants. `None` (the default): the owner goes inside
    /// [`Driver::create_schema_script`] (`AUTHORIZATION`), right where the
    /// creator keeps the right to grant on it afterwards (PostgreSQL run by
    /// a superuser or by a member of the owner, SQL Server).
    ///
    /// Engines where the creator loses the right to grant once the schema
    /// is someone else's return the change here, and DBine then creates the
    /// schema without owner: Snowflake (`GRANT OWNERSHIP`), Databricks
    /// (`ALTER SCHEMA … OWNER TO`), Trino (`ALTER SCHEMA … SET
    /// AUTHORIZATION`), Aurora DSQL and PostgreSQL run by someone who isn't
    /// superuser (`ALTER SCHEMA … OWNER TO`). An invalid owner is an error,
    /// never `Unsupported` (an older plugin host answers that, read as
    /// `None`).
    fn schema_owner_script(&self, database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        let _ = (database, name, owner);
        Ok(None)
    }

    /// The code that grants `privileges` (names from the spec's
    /// `privileges`) on schema `name` of `database` to `to`. By default
    /// [`Driver::security_script`] with a `SecurityAction::Grant` whose
    /// object is `ObjectRef { kind: "schema", schema: None, name }`;
    /// engines whose schema path depends on the database override it.
    fn schema_grant_script(&self, database: Option<&str>, name: &str, privileges: &[String], to: &str, grantable: bool) -> Result<String> {
        let _ = database;
        let object = ObjectRef { kind: "schema".into(), schema: None, name: name.to_string() };
        self.security_script(&security::SecurityAction::Grant { privileges: privileges.to_vec(), object: Some(object), to: to.to_string(), grantable })
    }

    /// The code that drops schema `name` of `database`; `cascade`: with its
    /// objects (only asked when the spec says `cascade`).
    fn drop_schema_script(&self, database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        let _ = (database, name, cascade);
        Err(Error::Unsupported("este motor no borra esquemas desde DBine".into()))
    }

    /// Code in the driver's language that deletes the rows with these keys
    /// (`DELETE … WHERE <key>` in SQL, `deleteOne` in MongoDB, `DEL` in
    /// Redis…): data compare's sync script uses it. Each key is the row's
    /// key columns with their values. `target` is the table / collection.
    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        if self.info().language != Language::Sql {
            return Err(Error::Unsupported("este motor no genera scripts de borrado".into()));
        }
        let quote = match self.info().dialect {
            "mssql" | "sybase" => sql::Quote::Bracket,
            "mysql" | "bigquery" | "hive" | "clickhouse" | "sparksql" | "databricks" => sql::Quote::Backtick,
            _ => sql::Quote::Double,
        };
        let flavor = ddl::SqlFlavor { quote, ..ddl::SqlFlavor::ansi() };
        Ok(ddl::delete_script(&flavor, target.schema(), &target.name, keys))
    }

    /// Statements around a table's data in a generated script, for engines
    /// that need them to load explicit key values: SQL Server's
    /// `SET IDENTITY_INSERT … ON/OFF`, PostgreSQL's sequence resync after
    /// the load, Oracle's identity restart… Empty by default.
    fn data_load_wrap(&self, table: &TableSchema) -> (String, String) {
        let _ = table;
        (String::new(), String::new())
    }

    /// Put between objects of a generated script (`GO` for SQL Server,
    /// `/` after Oracle PL/SQL…). Empty when `;` already separates them.
    fn script_separator(&self) -> &'static str {
        ""
    }

    /// How the engine's own tool reads a script: quotes, comments, blocks,
    /// batch lines, terminator switches (see [`sql::ScriptDialect`]). The
    /// app splits editor scripts with it ([`Driver::split_script`]), and so
    /// do "run the statement at the cursor", the read-only guard and the
    /// UPDATE/DELETE check. Plugin drivers send it in their manifest. By
    /// default, the preset for [`DriverInfo::dialect`]
    /// ([`sql::ScriptDialect::for_hint`]: PostgreSQL's dollar quotes, MySQL's
    /// `#` comments, T-SQL's `GO`, PL/SQL blocks…), else the generic one.
    fn script_dialect(&self) -> sql::ScriptDialect {
        sql::ScriptDialect::for_hint(self.info().dialect)
    }

    /// The script cut into the units the app runs one by one, with their
    /// positions. Express the engine's rules through
    /// [`Driver::script_dialect`] rather than overriding this when it can:
    /// a plugin driver's override reaches the app through a call to its
    /// host (`SplitScript`), and a host published before that call leaves
    /// the app splitting with the dialect.
    fn split_script(&self, text: &str) -> Vec<sql::ScriptStatement> {
        sql::split_script(text, &self.script_dialect())
    }

    /// How the app runs an editor script on this engine (see
    /// [`sql::ScriptMode`]). `Whole` by default: the driver gets the script
    /// in one `execute`, as before. Drivers switch to `PerStatement` /
    /// `Batches` once their `execute` takes single statements well.
    fn script_mode(&self) -> sql::ScriptMode {
        sql::ScriptMode::Whole
    }

    /// The engine tool's defaults for scripts: continue after an error or
    /// stop, and whether to ask before an UPDATE/DELETE without WHERE.
    fn script_defaults(&self) -> sql::ScriptDefaults {
        sql::ScriptDefaults::for_language(self.info().language)
    }

    /// Its sessions implement manual transactions
    /// ([`Session::set_autocommit`], [`Session::commit`],
    /// [`Session::rollback`]): the editor offers Auto/Manual, Commit and
    /// Rollback only then.
    fn supports_manual_transactions(&self) -> bool {
        false
    }

    /// Open a session on `database` (the config's default when `None`).
    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>>;

    /// Its sessions implement [`Session::bulk_load`], the engine's native
    /// bulk load (see [`transfer`]). Without it the migration writes
    /// [`Driver::insert_script`] batches.
    fn supports_bulk_load(&self) -> bool {
        false
    }

    /// [`Driver::copy_native`] works from this driver's sessions to those
    /// of `target` (a driver id served by the same crate).
    fn supports_native_copy(&self, target: &str) -> bool {
        let _ = target;
        false
    }

    /// Copy a table from `source` to `target` inside the driver, rows never
    /// decoded (see [`transfer`]). `source` is only read from, never written
    /// to. Returns the rows copied; `progress` gets the committed rows.
    async fn copy_native(
        &self,
        source: &mut dyn Session,
        target: &mut dyn Session,
        spec: &transfer::CopySpec,
        progress: transfer::Progress<'_>,
    ) -> Result<u64> {
        let _ = (source, target, spec, progress);
        Err(Error::Unsupported("este motor no copia tablas directamente entre bases".into()))
    }

    /// [`Driver::clone_script`] works: a same-engine migration can leave
    /// the target identical to the source ("clonar").
    fn supports_clone(&self) -> bool {
        false
    }

    /// Everything that makes `target` identical to `source` for these
    /// tables (see [`transfer::CloneScript`]), adapted to what `target`
    /// supports. `source` is only read from; nothing runs on `target` (it's
    /// asked about its capabilities only).
    async fn clone_script(&self, source: &mut dyn Session, target: &mut dyn Session, tables: &[ObjectRef]) -> Result<transfer::CloneScript> {
        let _ = (source, target, tables);
        Err(Error::Unsupported("este motor no clona bases".into()))
    }

    /// Sync by rows works between two sessions of this driver
    /// ([`Session::delta_summary`], [`Session::delta_apply`]).
    fn supports_delta(&self) -> bool {
        false
    }

    /// The condition (for [`transfer::ReadSpec::filter`]) that selects the
    /// rows of these buckets.
    fn delta_filter(&self, spec: &transfer::DeltaSpec, buckets: &[i64]) -> Result<String> {
        let _ = (spec, buckets);
        Err(Error::Unsupported("este motor no sincroniza por filas".into()))
    }

    /// What the Backups tab offers for the engine's own backups (see
    /// [`backup`]); `None`: only DBine's copies (a script with the
    /// structure and the data), which need nothing from the driver.
    fn backup(&self) -> Option<backup::BackupSpec> {
        None
    }

    /// The code that backs up, restores or deletes a backup, in the
    /// driver's language (`BACKUP DATABASE…`, `BACKUP … TO Disk(…)`,
    /// `PUT _snapshot/…`). DBine shows it and runs it only on the user's
    /// click, in [`backup::BackupSpec::script_database`].
    fn backup_script(&self, action: &backup::BackupAction) -> Result<String> {
        let _ = action;
        Err(Error::Unsupported("este motor no tiene backups propios".into()))
    }
}

#[async_trait]
pub trait Session: Send {
    /// Server product and version, for the status bar.
    async fn server_version(&mut self) -> Result<String>;

    /// Namespaces below the connection (databases, keyspaces, datasets…).
    /// Engines with a single namespace return one entry (e.g. `["main"]`).
    async fn list_databases(&mut self) -> Result<Vec<String>>;

    /// Objects of the session's database, of the kinds the driver declares.
    async fn list_objects(&mut self) -> Result<Vec<DbObject>>;

    /// Every schema of the session's database, including those without
    /// objects (a schema just made with `CREATE SCHEMA` shows in the
    /// explorer). `None`: the driver doesn't list schemas; the UI falls back
    /// to deriving them from [`Session::list_objects`].
    async fn list_schemas(&mut self) -> Result<Option<Vec<SchemaInfo>>> {
        Ok(None)
    }

    /// Columns (fields) of an object, in ordinal order. Schemaless engines
    /// infer them from a sample.
    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>>;

    /// Source of an object (view, routine, trigger, index mapping…); for a
    /// table, its CREATE statement when the engine gives one. `None` when
    /// there's nothing to show (the UI builds a CREATE TABLE from columns).
    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>>;

    /// Query text, in the driver's language, that shows the first `limit`
    /// rows / documents / entries of an object.
    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String;

    /// Run a script (one or more statements / commands), appending each
    /// one's result to `out` and keeping at most `max_rows` rows per result
    /// set. An `Err` stops the script; what ran before it stays in `out`.
    ///
    /// When the driver's [`Driver::script_mode`] isn't `Whole`, the app
    /// calls it with one statement (or batch) of the script at a time and
    /// decides itself whether to go on after an error. Either way: messages
    /// go through [`QueryOutcome::info`] / [`QueryOutcome::warning`] as they
    /// arrive (the UI shows them live), and a failure with a code or a
    /// position is returned as [`Error::Statement`].
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()>;

    /// Whether a transaction is open. `None`: the driver doesn't track it
    /// (the UI shows nothing). Asked after every editor run, so it must be
    /// cheap.
    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        Ok(None)
    }

    /// Autocommit on (each statement commits) or off (the first statement
    /// opens a transaction that stays open until [`Session::commit`] or
    /// [`Session::rollback`]). Sessions start in autocommit. Drivers that
    /// implement it set [`Driver::supports_manual_transactions`].
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if on {
            Ok(())
        } else {
            Err(Error::Unsupported("este motor no permite transacciones manuales desde DBine".into()))
        }
    }

    /// Commit the open transaction (nothing to do when there's none).
    async fn commit(&mut self) -> Result<()> {
        Err(Error::Unsupported("este motor no permite transacciones manuales desde DBine".into()))
    }

    /// Roll back the open transaction (nothing to do when there's none).
    async fn rollback(&mut self) -> Result<()> {
        Err(Error::Unsupported("este motor no permite transacciones manuales desde DBine".into()))
    }

    /// Execution plans of a script, pushed to `out.plans`.
    /// - `analyze = false`: estimated plans; nothing runs.
    /// - `analyze = true`: the script runs as with `execute` (its results go
    ///   to `out` too) and the plans carry actual figures. Engines that can
    ///   only get actual figures by running a statement again must not do so
    ///   for statements that write (give the estimated plan for those).
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let _ = (text, analyze, max_rows, out);
        Err(Error::Unsupported("este motor todavía no muestra planes de ejecución".into()))
    }

    /// Something that stops the statement in flight from another thread,
    /// for engines where dropping the session doesn't stop the server
    /// (it keeps running a query nobody reads) or where work runs on a
    /// blocking thread.
    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        None
    }

    /// Every table (collection…) of the session's database with columns,
    /// primary key, foreign keys and indexes: the ER diagram and the script
    /// generator. The default asks `columns` table by table and reports no
    /// foreign keys or indexes; drivers override it with catalog queries.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let objects = self.list_objects().await?;
        let mut out = Vec::new();
        for o in objects.into_iter().filter(|o| o.kind == kinds::TABLE || o.kind == kinds::COLLECTION) {
            let obj = ObjectRef { kind: o.kind.clone(), schema: o.schema.clone(), name: o.name.clone() };
            let cols = self.columns(&obj).await?;
            let pk: Vec<String> = cols.iter().filter(|c| c.primary_key).map(|c| c.name.clone()).collect();
            out.push(TableSchema {
                kind: o.kind,
                schema: o.schema,
                name: o.name,
                primary_key: (!pk.is_empty()).then(|| KeyDef { name: None, columns: pk }),
                columns: cols
                    .into_iter()
                    .map(|c| ColumnDef {
                        name: c.name,
                        data_type: c.data_type,
                        nullable: c.nullable,
                        default_value: c.default_value,
                        auto_increment: c.auto_increment,
                        ..Default::default()
                    })
                    .collect(),
                ..Default::default()
            });
        }
        Ok(out)
    }

    /// Create a database (keyspace, dataset…) on the server.
    async fn create_database(&mut self, name: &str) -> Result<()> {
        let _ = name;
        Err(Error::Unsupported("este motor no crea bases desde DBine".into()))
    }

    /// The server's suggestions and defaults for
    /// [`Driver::create_database_fields`]: the collations it has, its
    /// default data and log paths, its users and tablespaces…
    async fn create_database_choices(&mut self) -> Result<Vec<info::FieldChoices>> {
        Ok(Vec::new())
    }

    /// Create database `name` with `options` (see
    /// [`Driver::create_database_fields`]). Without options it's
    /// [`Session::create_database`].
    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        if options.values().all(|v| v.trim().is_empty()) {
            return self.create_database(name).await;
        }
        Err(Error::Unsupported("este motor no admite opciones al crear una base".into()))
    }

    /// "Propiedades" of `database`: what can be changed, its current
    /// values, read-only facts and warnings. Drivers that implement it set
    /// `Capabilities::database_properties`.
    async fn database_properties(&mut self, database: &str) -> Result<info::DatabaseProperties> {
        let _ = database;
        Err(Error::Unsupported("este motor no muestra las propiedades de una base".into()))
    }

    /// Apply `changes` to `database` (see [`Driver::alter_database_script`]).
    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        let _ = (database, changes);
        Err(Error::Unsupported("este motor no modifica las propiedades de una base".into()))
    }

    /// Drop a database (keyspace, dataset…) and everything in it.
    async fn drop_database(&mut self, name: &str) -> Result<()> {
        let _ = name;
        Err(Error::Unsupported("este motor no borra bases desde DBine".into()))
    }

    /// A snapshot of the server's health for the monitor dashboard (see
    /// [`monitor`]). Drivers that implement it set `Capabilities::monitor`.
    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        Err(Error::Unsupported("este motor todavía no ofrece monitoreo".into()))
    }

    /// The sessions in blocking chains right now: those waiting on another
    /// and the ones they wait for (see [`monitor::BlockedSession`]). Empty
    /// when nothing is blocked. Drivers that implement it set
    /// `Capabilities::blocking`.
    async fn blocking(&mut self) -> Result<Vec<monitor::BlockedSession>> {
        Err(Error::Unsupported("este motor no informa bloqueos entre sesiones".into()))
    }

    /// The server's (or, when `SecuritySpec::per_database`, the
    /// session's database's) users and roles.
    async fn principals(&mut self) -> Result<Vec<security::Principal>> {
        Err(Error::Unsupported("este motor no administra usuarios desde DBine".into()))
    }

    /// A user's or role's permissions, direct and through its roles.
    async fn grants(&mut self, principal: &str) -> Result<Vec<security::Grant>> {
        let _ = principal;
        Err(Error::Unsupported("este motor no administra usuarios desde DBine".into()))
    }

    /// End another session of the server (its id as `blocking` or the
    /// monitor reports it): its transaction rolls back. Drivers that
    /// implement it set `Capabilities::kill_session`.
    async fn kill_session(&mut self, id: &str) -> Result<()> {
        let _ = id;
        Err(Error::Unsupported("este motor no permite terminar sesiones desde DBine".into()))
    }

    /// The server's sessions and running requests (the Monitor's
    /// "Procesos"), lighter than [`Session::monitor`]: the list polls every
    /// few seconds. Drivers that implement it set `Capabilities::processes`.
    async fn processes(&mut self) -> Result<Vec<monitor::ServerProcess>> {
        Err(Error::Unsupported("este motor no lista sus procesos".into()))
    }

    /// Stop the statement another session is running (its id as
    /// `processes` reports it) and leave the session open. Drivers that
    /// implement it set `Capabilities::cancel_query`.
    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        let _ = id;
        Err(Error::Unsupported("este motor no permite cancelar consultas de otras sesiones".into()))
    }

    /// Start watching the statements run against a database (see
    /// [`profiler`]). The session is dedicated to it until `profiler_stop`.
    async fn profiler_start(&mut self, opts: &ProfilerOptions) -> Result<ProfilerStarted> {
        let _ = opts;
        Err(Error::Unsupported("este motor no permite ver las consultas de otros clientes".into()))
    }

    /// The statements seen since the last poll, oldest first.
    async fn profiler_poll(&mut self) -> Result<Vec<ProfiledStatement>> {
        Err(Error::Unsupported("el profiler no está iniciado".into()))
    }

    /// Stop, putting back any server setting `profiler_start` changed.
    async fn profiler_stop(&mut self) -> Result<()> {
        Ok(())
    }

    /// One page of the database's keys that match `scan` (see [`keys`]),
    /// on drivers whose [`Driver::key_search`] is `Some`.
    async fn scan_keys(&mut self, scan: &KeyScan) -> Result<KeyPage> {
        let _ = scan;
        Err(Error::Unsupported("este motor no busca claves en el servidor".into()))
    }

    /// Read a table in batches (see [`transfer`]), handing them to `sink`;
    /// returns the rows read. The default runs `browse_query` and turns its
    /// grid values into cells; drivers override it to read typed values.
    async fn read_batches(&mut self, spec: &transfer::ReadSpec, sink: transfer::BatchSinkRef) -> Result<u64> {
        transfer::read_via_execute(self, spec, sink).await
    }

    /// Bulk load `source`'s batches into `spec.table` with the engine's
    /// native mechanism, committing by `spec`'s windows; returns the rows
    /// loaded. Drivers that implement it set [`Driver::supports_bulk_load`].
    async fn bulk_load(
        &mut self,
        spec: &transfer::LoadSpec,
        columns: &[transfer::TransferColumn],
        source: &mut dyn transfer::BatchSource,
        progress: transfer::Progress<'_>,
    ) -> Result<u64> {
        let _ = (spec, columns, source, progress);
        Err(Error::Unsupported("este motor no tiene carga masiva".into()))
    }

    /// The concrete session, for [`Driver::copy_native`] (a driver finds
    /// its own sessions behind `dyn Session`).
    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        None
    }

    /// Rows and smallest / largest value of an integer column (to size a
    /// sync's range buckets); `None` when the table is empty.
    async fn key_range(&mut self, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
        let _ = (table, column);
        Err(Error::Unsupported("este motor no sincroniza por filas".into()))
    }

    /// Each bucket's row count and hash sum (see [`transfer::DeltaSpec`]).
    /// Only reads.
    async fn delta_summary(&mut self, spec: &transfer::DeltaSpec) -> Result<Vec<transfer::BucketSum>> {
        let _ = spec;
        Err(Error::Unsupported("este motor no sincroniza por filas".into()))
    }

    /// Make the rows of `buckets` equal to `source`'s (the source's rows of
    /// those buckets): staged, then inserted / updated / deleted in one
    /// transaction.
    async fn delta_apply(
        &mut self,
        spec: &transfer::DeltaSpec,
        buckets: &[i64],
        columns: &[transfer::TransferColumn],
        source: &mut dyn transfer::BatchSource,
        progress: transfer::Progress<'_>,
    ) -> Result<transfer::DeltaResult> {
        let _ = (spec, buckets, columns, source, progress);
        Err(Error::Unsupported("este motor no sincroniza por filas".into()))
    }

    /// The backups the server has (of `database`, or all of them when
    /// `None`), newest first. Drivers set [`backup::BackupSpec::history`].
    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<backup::BackupEntry>> {
        let _ = database;
        Err(Error::Unsupported("este motor no lista sus backups".into()))
    }

    /// What the login may do (see [`permissions`]): backups, restores, the
    /// profiler, ending sessions, creating and dropping `database`, managing
    /// users. The UI turns off what's denied. Actions left `Unknown` stay on.
    async fn permissions(&mut self, database: Option<&str>) -> Result<Permissions> {
        let _ = database;
        Ok(Permissions::default())
    }

    /// What depends on `target` (see [`dependencies`]). The default scans
    /// the catalog's foreign keys and every object's source; drivers whose
    /// engine tracks dependencies override it with catalog queries.
    async fn dependents(&mut self, target: &DependencyTarget, scan: &DependencyScan) -> Result<DependencyReport> {
        dependencies::scan(self, target, scan).await
    }

    /// The engine's own "Chequeo de salud" findings for `database` (see
    /// [`health`]): configuration, statistics, space, bloat… The app adds
    /// the checks every engine shares. Empty: none of its own.
    async fn health_checks(&mut self, database: &str) -> Result<Vec<health::HealthCheck>> {
        let _ = database;
        Ok(Vec::new())
    }

    /// "Buscar en la base" in one catalog query (see [`search`]). `None`:
    /// the app scans each object's definition itself, with progress.
    async fn search_code(&mut self, query: &search::CodeSearch) -> Result<Option<search::CodeSearchReport>> {
        let _ = query;
        Ok(None)
    }

    /// `table`'s indexes and how they're used (see [`index_usage`]). `None`:
    /// the engine doesn't report it ([`Driver::supports_index_usage`]).
    /// The app fills the derived numbers ([`IndexUsageReport::derive`]).
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<IndexUsageReport>> {
        let _ = table;
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A driver that only says who it is: every other method is the default.
    struct Bare(DriverInfo);

    #[async_trait]
    impl Driver for Bare {
        fn info(&self) -> &DriverInfo {
            &self.0
        }
        async fn connect(&self, _: &ConnectionConfig, _: Option<&str>) -> Result<Box<dyn Session>> {
            Err(Error::Unsupported("test".into()))
        }
    }

    fn bare() -> Bare {
        Bare(DriverInfo {
            id: "bare",
            name: "Bare",
            family: Family::Relational,
            language: Language::Sql,
            dialect: "standard",
            default_port: 0,
            fields: vec![],
            databases_label: "",
            has_schemas: true,
            object_kinds: vec![],
        })
    }

    #[test]
    fn schemas_are_unsupported_by_default() {
        let d = bare();
        assert!(d.schema_spec().is_none());
        assert!(matches!(d.create_schema_script(None, "ventas", Some("ana")), Err(Error::Unsupported(_))));
        assert!(matches!(d.schema_owner_script(None, "ventas", "ana"), Ok(None)));
        assert!(matches!(d.schema_grant_script(None, "ventas", &["USAGE".into()], "ana", false), Err(Error::Unsupported(_))));
        assert!(matches!(d.drop_schema_script(None, "ventas", true), Err(Error::Unsupported(_))));
    }

    /// A session that only implements the required methods.
    struct BareSession;

    #[async_trait]
    impl Session for BareSession {
        async fn server_version(&mut self) -> Result<String> {
            Ok(String::new())
        }
        async fn list_databases(&mut self) -> Result<Vec<String>> {
            Ok(vec![])
        }
        async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
            Ok(vec![])
        }
        async fn columns(&mut self, _: &ObjectRef) -> Result<Vec<ColumnInfo>> {
            Ok(vec![])
        }
        async fn definition(&mut self, _: &ObjectRef) -> Result<Option<String>> {
            Ok(None)
        }
        fn browse_query(&self, _: &ObjectRef, _: u32) -> String {
            String::new()
        }
        async fn execute(&mut self, _: &str, _: usize, _: &mut QueryOutcome) -> Result<()> {
            Ok(())
        }
    }

    /// The default's future is ready at once: poll it without a runtime.
    fn ready<T>(f: impl std::future::Future<Output = T>) -> T {
        let mut f = std::pin::pin!(f);
        match f.as_mut().poll(&mut std::task::Context::from_waker(std::task::Waker::noop())) {
            std::task::Poll::Ready(v) => v,
            std::task::Poll::Pending => panic!("the default list_schemas waited"),
        }
    }

    #[test]
    fn scripts_run_whole_by_default() {
        let d = bare();
        assert_eq!(d.script_mode(), sql::ScriptMode::Whole);
        assert_eq!(d.script_dialect(), sql::ScriptDialect::generic());
        assert_eq!(d.script_defaults(), sql::ScriptDefaults { continue_on_error: false, confirm_unsafe_dml: true });
        assert!(!d.supports_manual_transactions());
        let st = d.split_script("select 1; select 2");
        assert_eq!(st.iter().map(|s| s.text.as_str()).collect::<Vec<_>>(), vec!["select 1", "select 2"]);
    }

    #[test]
    fn transactions_are_not_tracked_by_default() {
        let mut s = BareSession;
        assert_eq!(ready(s.transaction_state()).unwrap(), None);
        assert!(ready(s.set_autocommit(true)).is_ok());
        assert!(matches!(ready(s.set_autocommit(false)), Err(Error::Unsupported(_))));
        assert!(matches!(ready(s.commit()), Err(Error::Unsupported(_))));
        assert!(matches!(ready(s.rollback()), Err(Error::Unsupported(_))));
        let mut ro = read_only::ReadOnlySession::new(Box::new(BareSession));
        assert_eq!(ready(ro.transaction_state()).unwrap(), None);
        assert!(matches!(ready(ro.commit()), Err(Error::Unsupported(_))));
    }

    #[test]
    fn schemas_are_not_listed_by_default() {
        assert_eq!(ready(BareSession.list_schemas()).unwrap(), None);
        // The read-only wrapper passes the driver's list through.
        let mut ro = read_only::ReadOnlySession::new(Box::new(BareSession));
        assert_eq!(ready(ro.list_schemas()).unwrap(), None);
    }
}
