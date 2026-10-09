//! The messages between the app and a driver host, and their framing: a
//! little-endian `u32` length, then the message in MessagePack (cells are
//! `serde_json::Value`, which needs a self-describing format).

use dbine_driver::runtime::ComponentProgress;
use dbine_driver::{
    Capabilities, ColumnFilter, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec,
    BlockedSession, DriverInfo, Error, KeyPage, KeyScan, KeySearch, MonitorSnapshot, ObjectRef, ProfiledStatement, ProfilerOptions,
    ProfilerStarted, QueryOutcome, ResultColumn, RowChange, SyncScript, TableChange, TableSchema,
};
use dbine_driver::transfer::{BucketSum, CloneScript, CopySpec, DeltaResult, DeltaSpec, LoadSpec, ReadSpec, RowBatch, TransferColumn};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::io::{self, Read, Write};

/// The messages' version. Adding a `Call` variant doesn't change it (a host
/// that doesn't know a call answers `Unsupported`); changing an existing
/// message does, and then every driver is published again. The app refuses
/// a host that speaks another one.
pub const PROTOCOL: u32 = 1;

/// Biggest frame accepted (a result page, a schema…); anything bigger is a
/// broken stream.
const MAX_FRAME: usize = 512 * 1024 * 1024;

/// Batches of a transfer in flight between app and host, not yet taken by
/// the other side (`BatchAck`): what bounds a transfer's memory.
pub const BATCH_WINDOW: usize = 16;

/// First message, app → host.
#[derive(Debug, Serialize, Deserialize)]
pub struct Hello {
    pub protocol: u32,
    /// The app's version (for the host's log).
    pub version: String,
    /// Where drivers keep downloaded components (`runtime::components_dir`).
    pub components_dir: Option<String>,
}

/// First message, host → app.
#[derive(Debug, Serialize, Deserialize)]
pub struct Ready {
    pub protocol: u32,
    /// The driver's own version (`DBINE_DRIVER_VERSION` when it was built).
    pub version: String,
    /// Ids of the drivers it serves.
    pub drivers: Vec<String>,
}

/// App → host.
#[derive(Debug, Serialize, Deserialize)]
pub enum ToHost {
    Call { id: u64, call: Call },
    /// Stop what the session is running (its interrupter), if it can.
    Cancel { session: u64 },
    /// Stop a call: the app stopped waiting for it.
    Abort { id: u64 },
    /// The app dropped the session: close it (and whatever it runs).
    Close { session: u64 },
    /// The app's row sink failed for this call: drop the rest of its rows.
    SinkFailed { id: u64, error: String },
    /// A bulk load's next batch (`None`: that was the last). Only sent to
    /// hosts whose manifest says `supports_bulk_load`.
    Batch { id: u64, batch: Option<RowBatch> },
    /// The app took a batch of a `ReadBatches` call: the host may send another.
    BatchAck { id: u64 },
}

/// One `Driver` or `Session` method. Driver methods name the driver (a
/// crate may serve several engines); session methods name the session.
#[derive(Debug, Serialize, Deserialize)]
pub enum Call {
    Manifest,
    Connect { driver: String, config: ConnectionConfig, database: Option<String> },
    SyncScript { driver: String, changes: Vec<TableChange> },
    /// The statements that disable or enable an index. A host published
    /// before it answers `Unsupported` (and its manifest doesn't offer it).
    IndexToggleScript { driver: String, table: ObjectRef, index: dbine_driver::IndexUsage, enable: bool },
    TableDdl { driver: String, table: TableSchema, parts: DdlParts },
    InsertScript { driver: String, target: ObjectRef, columns: Vec<String>, rows: Vec<Vec<Value>> },
    FilteredBrowse { driver: String, browse: String, filters: Vec<ColumnFilter> },
    UpdateScript { driver: String, target: ObjectRef, changes: Vec<RowChange> },
    DeleteScript { driver: String, target: ObjectRef, keys: Vec<Vec<(String, Value)>> },
    SecurityScript { driver: String, action: dbine_driver::SecurityAction },
    BackupScript { driver: String, action: dbine_driver::BackupAction },
    DataLoadWrap { driver: String, table: TableSchema },

    ServerVersion { session: u64 },
    ListDatabases { session: u64 },
    ListObjects { session: u64 },
    Columns { session: u64, obj: ObjectRef },
    Definition { session: u64, obj: ObjectRef },
    BrowseQuery { session: u64, obj: ObjectRef, limit: u32 },
    /// `sink`: the app streams the rows (export, migration): they come back
    /// as `SinkBegin` / `SinkRow` events instead of in the outcome.
    ///
    /// `continue_on_error`: [`QueryOutcome::continue_on_error`] of an editor
    /// run. `live`: messages and ended statements come back as `Message` /
    /// `StatementEnded` events as they happen. Hosts published before them
    /// ignore both (one call that stops at the first error).
    Execute {
        session: u64,
        text: String,
        max_rows: u64,
        sink: bool,
        #[serde(default)]
        continue_on_error: Option<bool>,
        #[serde(default)]
        live: bool,
    },
    Explain { session: u64, text: String, analyze: bool, max_rows: u64, sink: bool },
    DatabaseSchema { session: u64 },
    CreateDatabase { session: u64, name: String },
    DropDatabase { session: u64, name: String },
    Monitor { session: u64 },
    Blocking { session: u64 },
    Principals { session: u64 },
    Grants { session: u64, principal: String },
    Backups { session: u64, database: Option<String> },
    KillSession { session: u64, id: String },
    ProfilerStart { session: u64, opts: ProfilerOptions },
    ProfilerPoll { session: u64 },
    ProfilerStop { session: u64 },
    ScanKeys { session: u64, scan: KeyScan },
    /// A table's rows in batches: `BatchBegin` / `Batch` events, at most
    /// `BATCH_WINDOW` not yet acknowledged; the reply is the row count.
    ReadBatches { session: u64, spec: ReadSpec },
    /// A bulk load: the app sends the batches (`ToHost::Batch`), at most
    /// `BATCH_WINDOW` not yet taken (`FromHost::BatchAck`); the reply is the
    /// row count, `Committed` events the progress.
    BulkLoad { session: u64, spec: LoadSpec, columns: Vec<TransferColumn> },
    /// A table copied between two sessions of this host.
    CopyNative { driver: String, from: u64, to: u64, spec: CopySpec },
    /// What makes the target identical to the source (two sessions of this host).
    CloneScript { driver: String, from: u64, to: u64, tables: Vec<ObjectRef> },
    DeltaFilter { driver: String, spec: DeltaSpec, buckets: Vec<i64> },
    KeyRange { session: u64, table: ObjectRef, column: String },
    DeltaSummary { session: u64, spec: DeltaSpec },
    /// Like `BulkLoad`: the app sends the source rows as `ToHost::Batch`.
    DeltaApply { session: u64, spec: DeltaSpec, buckets: Vec<i64>, columns: Vec<TransferColumn> },
    Permissions { session: u64, database: Option<String> },
    /// "Nuevo esquema…" / "Borrar esquema…". A host published before them
    /// answers `Unsupported` (see `unknown_call`). `database` came later: a
    /// host that doesn't know it ignores it, and an app that doesn't send
    /// it reads as `None`.
    CreateSchemaScript {
        driver: String,
        name: String,
        owner: Option<String>,
        #[serde(default)]
        database: Option<String>,
    },
    DropSchemaScript {
        driver: String,
        name: String,
        cascade: bool,
        #[serde(default)]
        database: Option<String>,
    },
    /// A host published before it answers `Unsupported`, which the app
    /// reads as `None` (the owner goes in the create).
    SchemaOwnerScript { driver: String, database: Option<String>, name: String, owner: String },
    /// A host published before it answers `Unsupported`, and the app asks
    /// `SecurityScript` instead (what that host did).
    SchemaGrantScript { driver: String, database: Option<String>, name: String, privileges: Vec<String>, to: String, grantable: bool },
    /// Every schema, even empty ones. A host published before it answers
    /// `Unsupported`, which the app reads as "not listed" (`None`).
    ListSchemas { session: u64 },
    /// Manual transactions. A host published before them answers
    /// `Unsupported`: `TransactionState` then reads as "not tracked" (`None`).
    TransactionState { session: u64 },
    SetAutocommit { session: u64, on: bool },
    Commit { session: u64 },
    Rollback { session: u64 },
    /// The driver's own `split_script` (SQL*Plus lines, `EXEC`…), which the
    /// app's lexer can't reproduce from the dialect. A host published before
    /// it answers `Unsupported`, and the app splits with the dialect.
    SplitScript { driver: String, text: String },
    /// A table's indexes and their usage. A host published before it
    /// answers `Unsupported`, which the app reads as "not reported" (`None`).
    IndexUsage { session: u64, table: ObjectRef },
    /// What depends on an object. A host published before it answers
    /// `Unsupported`, and the app runs the generic scan through the host's
    /// other calls.
    Dependents { session: u64, target: dbine_driver::DependencyTarget, scan: dbine_driver::DependencyScan },
    /// The server's sessions (the Monitor's "Procesos"). A host published
    /// before it answers `Unsupported`; the app only asks drivers whose
    /// capabilities say `processes`.
    Processes { session: u64 },
    /// Stop another session's statement. Same as `Processes` for older hosts.
    CancelQuery { session: u64, id: String },
    /// "Nueva base de datos" with options: the script, the server's
    /// suggestions, and the creation. A host published before them
    /// answers `Unsupported`; the app only asks drivers whose manifest
    /// lists `create_database_fields`.
    CreateDatabaseScript { driver: String, name: String, options: std::collections::BTreeMap<String, String> },
    CreateDatabaseChoices { session: u64 },
    CreateDatabaseWith { session: u64, name: String, options: std::collections::BTreeMap<String, String> },
    /// "Propiedades" of a database: read, the script, apply. A host
    /// published before them answers `Unsupported`; the app only asks
    /// drivers whose capabilities say `database_properties`.
    DatabaseProperties { session: u64, database: String },
    AlterDatabaseScript { driver: String, database: String, changes: std::collections::BTreeMap<String, String> },
    AlterDatabase { session: u64, database: String, changes: std::collections::BTreeMap<String, String> },
    /// "Buscar en la base" from the catalog. A host published before it
    /// answers `Unsupported`, and the app scans the definitions itself.
    SearchCode { session: u64, query: dbine_driver::search::CodeSearch },
    /// "Chequeo de salud": the engine's own findings. A host published
    /// before it answers `Unsupported` (none of its own).
    HealthChecks { session: u64, database: String },
    /// "Documentar la base": rows from statistics and comments on views,
    /// routines… A host published before them answers `Unsupported`
    /// (none known).
    RowEstimates { session: u64 },
    ObjectComments { session: u64 },
    /// "Renombrar…": the statements that rename an object. A host published
    /// before it answers `Unsupported` (and its manifest doesn't offer it).
    RenameScript { driver: String, request: dbine_driver::RenameRequest },
    /// "Renombrar…" on a database. A host published before it answers
    /// `Unsupported` (and its manifest doesn't offer it).
    RenameDatabaseScript { driver: String, database: String, new_name: String, objects: Vec<dbine_driver::rename::DatabaseObject> },
    /// "Asignar login…": the server's logins with no user in the session's
    /// database. A host published before it answers `Unsupported`, and the
    /// dialog lets the user type the login.
    UnmappedLogins { session: u64 },
    /// "Asignar login…": the statement that maps a server login to a user
    /// of the current database. A host published before it answers
    /// `Unsupported` (and its manifest doesn't offer it).
    MapLoginScript { driver: String, login: String, user: String, default_schema: Option<String> },
}

/// Host → app.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromHost {
    Reply { id: u64, result: Result<Reply, WireError> },
    SinkBegin { id: u64, index: u64, columns: Vec<ResultColumn> },
    SinkRow { id: u64, index: u64, row: Vec<Value> },
    /// A message of a `live` `Execute`, as it arrives.
    Message { id: u64, message: dbine_driver::Message },
    /// A statement of a `live` `Execute` ended.
    StatementEnded { id: u64, end: dbine_driver::StatementEnd },
    /// A component download of a driver (DuckDB's library…).
    Progress(ComponentProgress),
    BatchBegin { id: u64, columns: Vec<TransferColumn> },
    Batch { id: u64, batch: RowBatch },
    /// A bulk load took a batch: the app may send another.
    BatchAck { id: u64 },
    /// Rows committed so far by a bulk load or native copy.
    Committed { id: u64, rows: u64 },
}

#[derive(Debug, Serialize, Deserialize)]
pub enum Reply {
    Unit,
    Text(String),
    MaybeText(Option<String>),
    Texts(Vec<String>),
    Pair(String, String),
    Objects(Vec<DbObject>),
    Columns(Vec<ColumnInfo>),
    Session { id: u64, interruptible: bool },
    /// A run (`execute` / `explain`): what it produced, and the error that
    /// stopped it (what ran before stays in the outcome).
    Run(QueryOutcome, Option<WireError>),
    Schema(Vec<TableSchema>),
    Sync(SyncScript),
    Monitor(MonitorSnapshot),
    Blocking(Vec<BlockedSession>),
    Principals(Vec<dbine_driver::Principal>),
    Grants(Vec<dbine_driver::Grant>),
    Backups(Vec<dbine_driver::BackupEntry>),
    ProfilerStarted(ProfilerStarted),
    Profiled(Vec<ProfiledStatement>),
    Keys(KeyPage),
    Manifest(Vec<DriverMeta>),
    Count(u64),
    Clone(CloneScript),
    KeyRange(Option<(i64, i64, u64)>),
    Buckets(Vec<BucketSum>),
    Delta(DeltaResult),
    Permissions(dbine_driver::Permissions),
    Schemas(Option<Vec<dbine_driver::SchemaInfo>>),
    TxState(Option<dbine_driver::TxState>),
    Units(Vec<dbine_driver::ScriptStatement>),
    IndexUsage(Option<dbine_driver::IndexUsageReport>),
    Dependents(dbine_driver::DependencyReport),
    Processes(Vec<dbine_driver::ServerProcess>),
    Choices(Vec<dbine_driver::FieldChoices>),
    DatabaseProperties(dbine_driver::DatabaseProperties),
    CodeSearch(Option<dbine_driver::search::CodeSearchReport>),
    HealthChecks(Vec<dbine_driver::health::HealthCheck>),
    RowEstimates(Vec<dbine_driver::stats::RowEstimate>),
    ObjectComments(Vec<dbine_driver::stats::ObjectComment>),
}

/// What a driver says about itself without a connection: the connection
/// form, the explorer and the editor need it before the driver is
/// downloaded, so the app carries it (the plugins' manifest).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DriverMeta {
    /// The driver crate that serves it (`sqlserver` serves Azure SQL too):
    /// which host to download.
    pub package: String,
    #[serde(deserialize_with = "owned")]
    pub info: DriverInfo,
    pub query_help: String,
    pub supports_explain: bool,
    pub supports_profiler: bool,
    #[serde(deserialize_with = "owned")]
    pub key_search: Option<KeySearch>,
    pub capabilities: Capabilities,
    #[serde(deserialize_with = "owned")]
    pub designer: Option<DesignerSpec>,
    #[serde(deserialize_with = "owned")]
    pub create_templates: Vec<CreateTemplate>,
    pub supports_schema_sync: bool,
    pub script_separator: String,
    /// Users and permissions (absent in manifests of drivers published
    /// before it existed).
    #[serde(default, deserialize_with = "owned")]
    pub security: Option<dbine_driver::SecuritySpec>,
    /// The engine's own backups (absent in older manifests).
    #[serde(default, deserialize_with = "owned")]
    pub backup: Option<dbine_driver::BackupSpec>,
    /// Native bulk load (`Session::bulk_load`).
    #[serde(default)]
    pub supports_bulk_load: bool,
    /// Native copy between two sessions of its host (`Driver::copy_native`).
    #[serde(default)]
    pub native_copy: bool,
    /// Same-engine clone (`Driver::clone_script`).
    #[serde(default)]
    pub supports_clone: bool,
    /// Sync by rows (`Driver::supports_delta`).
    #[serde(default)]
    pub supports_delta: bool,
    /// Creating and dropping schemas (absent in older manifests: `None`,
    /// the explorer doesn't offer them).
    #[serde(default, deserialize_with = "owned")]
    pub schema_spec: Option<dbine_driver::SchemaSpec>,
    /// How the app splits and runs its scripts (absent in older manifests:
    /// the dialect hint's preset, the whole script in one call, stop on
    /// errors). Read leniently: a value this build can't read (a newer
    /// host's) falls back to that default instead of failing the manifest.
    #[serde(default, deserialize_with = "lenient")]
    pub script_dialect: Option<dbine_driver::ScriptDialect>,
    #[serde(default, deserialize_with = "lenient")]
    pub script_mode: Option<dbine_driver::ScriptMode>,
    #[serde(default, deserialize_with = "lenient")]
    pub script_defaults: Option<dbine_driver::ScriptDefaults>,
    /// Auto/Manual, Commit and Rollback in the editor.
    #[serde(default)]
    pub supports_manual_transactions: bool,
    /// A table's indexes and their usage (`Session::index_usage`; absent
    /// in older manifests: not offered).
    #[serde(default)]
    pub supports_index_usage: bool,
    /// "Deshabilitar / Habilitar índice" (`Driver::index_toggle_script`;
    /// absent in older manifests: not offered).
    #[serde(default)]
    pub supports_index_toggle: bool,
    /// "Renombrar…" (`Driver::rename_spec`; absent in older manifests: not
    /// offered).
    #[serde(default)]
    pub rename: Option<dbine_driver::RenameSpec>,
    /// "Nueva base de datos"'s advanced options (absent in older
    /// manifests: just the name).
    #[serde(default, deserialize_with = "owned")]
    pub create_database_fields: Vec<dbine_driver::Field>,
    /// "Asignar login…" (`Driver::supports_map_login`; absent in older
    /// manifests: not offered).
    #[serde(default)]
    pub supports_map_login: bool,
}

/// The contract's metadata types hold `&'static str` (interned when read),
/// which serde only reads from `'static` input: read them through an owned
/// JSON value, whatever the input.
fn owned<'de, D, T>(d: D) -> Result<T, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'static>,
{
    let v = Value::deserialize(d)?;
    T::deserialize(v).map_err(serde::de::Error::custom)
}

/// `None` instead of an error when the value doesn't read as a `T`.
fn lenient<'de, D, T>(d: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: serde::de::DeserializeOwned,
{
    let v = Value::deserialize(d)?;
    Ok(serde_json::from_value(v).ok())
}

impl DriverMeta {
    pub fn of(package: &str, d: &dyn dbine_driver::Driver) -> Self {
        DriverMeta {
            package: package.to_string(),
            info: d.info().clone(),
            query_help: d.query_help().to_string(),
            supports_explain: d.supports_explain(),
            supports_profiler: d.supports_profiler(),
            key_search: d.key_search(),
            capabilities: d.capabilities(),
            designer: d.designer(),
            create_templates: d.create_templates(),
            supports_schema_sync: d.supports_schema_sync(),
            script_separator: d.script_separator().to_string(),
            security: d.security(),
            backup: d.backup(),
            supports_bulk_load: d.supports_bulk_load(),
            native_copy: d.supports_native_copy(d.info().id),
            supports_clone: d.supports_clone(),
            supports_delta: d.supports_delta(),
            schema_spec: d.schema_spec(),
            script_dialect: Some(d.script_dialect()),
            script_mode: Some(d.script_mode()),
            script_defaults: Some(d.script_defaults()),
            supports_manual_transactions: d.supports_manual_transactions(),
            supports_index_usage: d.supports_index_usage(),
            supports_index_toggle: d.supports_index_toggle(),
            rename: d.rename_spec(),
            create_database_fields: d.create_database_fields(),
            supports_map_login: d.supports_map_login(),
        }
    }
}

/// A driver error across the wire: its kind, so the app rebuilds the same
/// variant (code matches on `Cancelled`, `Connect`, `Unsupported`…).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub kind: String,
    pub message: String,
    /// An `Error::Statement`'s details; its `kind` is "query", so an app
    /// that doesn't know them still gets the query error.
    #[serde(default)]
    pub detail: Option<dbine_driver::ScriptError>,
}

impl From<&Error> for WireError {
    fn from(e: &Error) -> Self {
        let (kind, message) = match e {
            Error::Connect(m) => ("connect", m.clone()),
            Error::AuthFailed(m) => ("auth_failed", m.clone()),
            Error::Unsupported(m) => ("unsupported", m.clone()),
            Error::Query(m) => ("query", m.clone()),
            Error::Statement(d) => return WireError { kind: "query".into(), message: d.message.clone(), detail: Some((**d).clone()) },
            Error::State(m) => ("state", m.clone()),
            Error::Secrets(m) => ("secrets", m.clone()),
            Error::Cancelled => ("cancelled", String::new()),
            Error::Io(e) => ("io", e.to_string()),
            Error::Serde(e) => ("serde", e.to_string()),
        };
        WireError { kind: kind.into(), message, detail: None }
    }
}

impl From<WireError> for Error {
    fn from(w: WireError) -> Self {
        match w.kind.as_str() {
            "connect" => Error::Connect(w.message),
            "auth_failed" => Error::AuthFailed(w.message),
            "unsupported" => Error::Unsupported(w.message),
            "query" => match w.detail {
                Some(d) => Error::Statement(Box::new(d)),
                None => Error::Query(w.message),
            },
            "secrets" => Error::Secrets(w.message),
            "cancelled" => Error::Cancelled,
            "io" => Error::Io(io::Error::other(w.message)),
            // Its text reads the same as the original's ("serde: …").
            "serde" => Error::State(format!("serde: {}", w.message)),
            _ => Error::State(w.message),
        }
    }
}

pub fn write_frame<T: Serialize>(w: &mut impl Write, msg: &T) -> io::Result<()> {
    let body = rmp_serde::to_vec_named(msg).map_err(io::Error::other)?;
    let len = u32::try_from(body.len()).map_err(|_| io::Error::other("mensaje demasiado grande"))?;
    w.write_all(&len.to_le_bytes())?;
    w.write_all(&body)?;
    w.flush()
}

/// The next message, or `None` at the end of the stream.
pub fn read_frame<T: for<'de> Deserialize<'de>>(r: &mut impl Read) -> io::Result<Option<T>> {
    match read_raw(r)? {
        Some(body) => rmp_serde::from_slice(&body).map(Some).map_err(io::Error::other),
        None => Ok(None),
    }
}

/// The next message's bytes, or `None` at the end of the stream.
pub fn read_raw(r: &mut impl Read) -> io::Result<Option<Vec<u8>>> {
    let mut len = [0u8; 4];
    match r.read_exact(&mut len) {
        Ok(()) => {}
        Err(e) if e.kind() == io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(e) => return Err(e),
    }
    let len = u32::from_le_bytes(len) as usize;
    if len > MAX_FRAME {
        return Err(io::Error::other(format!("mensaje de {len} bytes: el canal con el driver está roto")));
    }
    let mut body = vec![0u8; len];
    r.read_exact(&mut body)?;
    Ok(Some(body))
}

/// A call this host doesn't know (the app is newer): its id and name, so
/// it can answer `Unsupported` instead of dropping the channel.
pub fn unknown_call(body: &[u8]) -> Option<(u64, String)> {
    let v: Value = rmp_serde::from_slice(body).ok()?;
    let call = v.get("Call")?;
    let id = call.get("id")?.as_u64()?;
    let name = match call.get("call")? {
        Value::Object(m) => m.keys().next().cloned().unwrap_or_default(),
        Value::String(s) => s.clone(),
        _ => String::new(),
    };
    Some((id, name))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn script_settings_a_newer_host_sends_fall_back() {
        #[derive(Serialize)]
        #[serde(rename_all = "snake_case")]
        enum NewMode {
            Parallel,
        }
        #[derive(Serialize)]
        struct NewMeta {
            script_mode: NewMode,
            script_dialect: Value,
            script_defaults: Value,
        }
        #[derive(Deserialize)]
        struct Meta {
            #[serde(default, deserialize_with = "lenient")]
            script_mode: Option<dbine_driver::ScriptMode>,
            #[serde(default, deserialize_with = "lenient")]
            script_dialect: Option<dbine_driver::ScriptDialect>,
            #[serde(default, deserialize_with = "lenient")]
            script_defaults: Option<dbine_driver::ScriptDefaults>,
        }
        let body = rmp_serde::to_vec_named(&NewMeta {
            script_mode: NewMode::Parallel,
            script_dialect: json!({ "batch": "semicolon_line", "plsql_blocks": true, "future": 1 }),
            script_defaults: json!("not a struct"),
        })
        .unwrap();
        let m: Meta = rmp_serde::from_slice(&body).unwrap();
        assert_eq!(m.script_mode, Some(dbine_driver::ScriptMode::Whole));
        let d = m.script_dialect.unwrap();
        assert_eq!((d.batch, d.plsql_blocks), (dbine_driver::sql::BatchLine::None, true));
        assert!(m.script_defaults.is_none());
        // Known values still read.
        let body = rmp_serde::to_vec_named(&json!({ "script_mode": "batches" })).unwrap();
        let m: Meta = rmp_serde::from_slice(&body).unwrap();
        assert_eq!(m.script_mode, Some(dbine_driver::ScriptMode::Batches));
        assert!(m.script_dialect.is_none());
    }

    #[test]
    fn an_unknown_call_is_identified() {
        // What a newer app sends: a call this host's `Call` doesn't have.
        #[derive(Serialize)]
        enum NewCall {
            FutureThing { session: u64, depth: u32 },
        }
        #[derive(Serialize)]
        enum NewToHost {
            Call { id: u64, call: NewCall },
        }
        let body = rmp_serde::to_vec_named(&NewToHost::Call { id: 42, call: NewCall::FutureThing { session: 1, depth: 3 } }).unwrap();
        assert!(rmp_serde::from_slice::<ToHost>(&body).is_err());
        assert_eq!(unknown_call(&body), Some((42, "FutureThing".to_string())));
        // A known one still reads as such.
        let known = rmp_serde::to_vec_named(&ToHost::Call { id: 1, call: Call::Manifest }).unwrap();
        assert!(rmp_serde::from_slice::<ToHost>(&known).is_ok());
    }

    #[test]
    fn the_schema_calls_database_is_optional_both_ways() {
        // An app built before `database`: a new host reads `None`.
        #[derive(Serialize)]
        enum OldCall {
            CreateSchemaScript { driver: String, name: String, owner: Option<String> },
        }
        #[derive(Serialize)]
        enum OldToHost {
            Call { id: u64, call: OldCall },
        }
        let body = rmp_serde::to_vec_named(&OldToHost::Call {
            id: 1,
            call: OldCall::CreateSchemaScript { driver: "dremio".into(), name: "a.b".into(), owner: None },
        })
        .unwrap();
        match rmp_serde::from_slice::<ToHost>(&body).unwrap() {
            ToHost::Call { call: Call::CreateSchemaScript { database, name, .. }, .. } => assert_eq!((database, name.as_str()), (None, "a.b")),
            other => panic!("{other:?}"),
        }
        // A host built before `database` ignores it.
        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        enum HostCall {
            DropSchemaScript { driver: String, name: String, cascade: bool },
        }
        #[derive(Deserialize, Debug)]
        #[allow(dead_code)]
        enum HostToHost {
            Call { id: u64, call: HostCall },
        }
        let call = Call::DropSchemaScript { driver: "dremio".into(), name: "a.b".into(), cascade: true, database: Some("lake".into()) };
        let body = rmp_serde::to_vec_named(&ToHost::Call { id: 2, call }).unwrap();
        assert!(rmp_serde::from_slice::<HostToHost>(&body).is_ok());
    }

    fn rename_request() -> dbine_driver::RenameRequest {
        dbine_driver::RenameRequest {
            target: dbine_driver::RenameTarget::Column { table: ObjectRef { kind: "table".into(), schema: Some("public".into()), name: "t".into() }, column: "a".into() },
            new_name: "b".into(),
            table: Some(TableSchema { name: "t".into(), ..Default::default() }),
            definition: None,
        }
    }

    #[test]
    fn a_rename_round_trips() {
        let body = rmp_serde::to_vec_named(&ToHost::Call { id: 9, call: Call::RenameScript { driver: "postgres".into(), request: rename_request() } }).unwrap();
        match rmp_serde::from_slice::<ToHost>(&body).unwrap() {
            ToHost::Call { call: Call::RenameScript { driver, request }, .. } => {
                assert_eq!(driver, "postgres");
                assert_eq!(request.new_name, "b");
                assert!(matches!(request.target, dbine_driver::RenameTarget::Column { ref column, .. } if column == "a"));
                assert_eq!(request.table.unwrap().name, "t");
            }
            other => panic!("{other:?}"),
        }
        // The manifest's spec, and a manifest without it.
        let spec = dbine_driver::RenameSpec { kinds: vec!["table".into()], columns: true, fold: dbine_driver::Fold::Lower, transactional: true, ..Default::default() };
        let back: dbine_driver::RenameSpec = rmp_serde::from_slice(&rmp_serde::to_vec_named(&spec).unwrap()).unwrap();
        assert_eq!(back.kinds, vec!["table".to_string()]);
        assert_eq!(back.fold, dbine_driver::Fold::Lower);
        assert!(back.transactional && back.columns && !back.schemas);
    }

    #[test]
    fn a_host_older_than_schema_calls_answers_unsupported() {
        // A host published before the schema calls: its `Call` lacks them.
        #[derive(Deserialize)]
        #[allow(dead_code)]
        enum OldCall {
            Manifest,
            SecurityScript { driver: String, action: dbine_driver::SecurityAction },
        }
        #[derive(Deserialize)]
        #[allow(dead_code)]
        enum OldToHost {
            Call { id: u64, call: OldCall },
        }
        for (id, call, name) in [
            (5, Call::CreateSchemaScript { driver: "postgres".into(), name: "ventas".into(), owner: Some("ana".into()), database: None }, "CreateSchemaScript"),
            (6, Call::DropSchemaScript { driver: "postgres".into(), name: "ventas".into(), cascade: true, database: None }, "DropSchemaScript"),
            (7, Call::ListSchemas { session: 3 }, "ListSchemas"),
            (8, Call::SchemaOwnerScript { driver: "postgres".into(), database: None, name: "ventas".into(), owner: "ana".into() }, "SchemaOwnerScript"),
            (
                9,
                Call::SchemaGrantScript { driver: "postgres".into(), database: None, name: "v".into(), privileges: vec![], to: "ana".into(), grantable: false },
                "SchemaGrantScript",
            ),
            (10, Call::SplitScript { driver: "oracle".into(), text: "PROMPT a\n".into() }, "SplitScript"),
            (11, Call::IndexUsage { session: 3, table: ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "t".into() } }, "IndexUsage"),
            (
                12,
                Call::Dependents {
                    session: 3,
                    target: dbine_driver::DependencyTarget { object: ObjectRef { kind: "table".into(), schema: Some("dbo".into()), name: "t".into() }, column: None },
                    scan: dbine_driver::DependencyScan { source_kinds: vec!["view".into()], dialect: dbine_driver::ScriptDialect::generic(), foreign_keys: true },
                },
                "Dependents",
            ),
            (13, Call::Processes { session: 3 }, "Processes"),
            (14, Call::CancelQuery { session: 3, id: "53".into() }, "CancelQuery"),
            (15, Call::CreateDatabaseScript { driver: "sqlserver".into(), name: "v".into(), options: Default::default() }, "CreateDatabaseScript"),
            (16, Call::CreateDatabaseChoices { session: 3 }, "CreateDatabaseChoices"),
            (17, Call::CreateDatabaseWith { session: 3, name: "v".into(), options: Default::default() }, "CreateDatabaseWith"),
            (18, Call::DatabaseProperties { session: 3, database: "v".into() }, "DatabaseProperties"),
            (19, Call::AlterDatabaseScript { driver: "sqlserver".into(), database: "v".into(), changes: Default::default() }, "AlterDatabaseScript"),
            (20, Call::AlterDatabase { session: 3, database: "v".into(), changes: Default::default() }, "AlterDatabase"),
            (21, Call::SearchCode { session: 3, query: Default::default() }, "SearchCode"),
            (22, Call::HealthChecks { session: 3, database: "v".into() }, "HealthChecks"),
            (23, Call::RowEstimates { session: 3 }, "RowEstimates"),
            (24, Call::ObjectComments { session: 3 }, "ObjectComments"),
            (25, Call::RenameScript { driver: "postgres".into(), request: rename_request() }, "RenameScript"),
            (26, Call::RenameDatabaseScript { driver: "postgres".into(), database: "v".into(), new_name: "w".into(), objects: Vec::new() }, "RenameDatabaseScript"),
            (27, Call::UnmappedLogins { session: 3 }, "UnmappedLogins"),
            (28, Call::MapLoginScript { driver: "sqlserver".into(), login: "ana".into(), user: "ana".into(), default_schema: Some("dbo".into()) }, "MapLoginScript"),
        ] {
            let body = rmp_serde::to_vec_named(&ToHost::Call { id, call }).unwrap();
            assert!(rmp_serde::from_slice::<OldToHost>(&body).is_err());
            // What the host loop answers with: an `Unsupported` reply for that id.
            assert_eq!(unknown_call(&body), Some((id, name.to_string())));
        }
        // An older host's `Permissions` reply, without `create_schema`.
        #[derive(Serialize)]
        struct OldPermissions {
            backup: dbine_driver::Access,
        }
        let body = rmp_serde::to_vec_named(&OldPermissions { backup: dbine_driver::Access::Allowed }).unwrap();
        let p: dbine_driver::Permissions = rmp_serde::from_slice(&body).unwrap();
        assert_eq!(p.create_schema, dbine_driver::Access::Unknown);
    }

    #[test]
    fn database_properties_round_trip() {
        let p = dbine_driver::DatabaseProperties {
            fields: vec![dbine_driver::Field::new("recovery", "Modelo de recuperación", dbine_driver::FieldKind::Text).group("Opciones")],
            values: [("recovery".to_string(), "FULL".to_string())].into(),
            info: vec![dbine_driver::PropertyInfo { group: String::new(), label: "Tamaño".into(), value: "16 MB".into() }],
            choices: Vec::new(),
            warnings: [("recovery".to_string(), "rompe la cadena de backups".to_string())].into(),
        };
        let body = rmp_serde::to_vec_named(&Reply::DatabaseProperties(p)).unwrap();
        match rmp_serde::from_slice::<Reply>(&body).unwrap() {
            Reply::DatabaseProperties(back) => {
                assert_eq!(back.fields[0].group, "Opciones");
                assert_eq!(back.values["recovery"], "FULL");
                assert_eq!(back.warnings.len(), 1);
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn an_index_usage_report_round_trips() {
        let report = Some(dbine_driver::IndexUsageReport {
            since: Some("2026-01-02 03:04:05".into()),
            stats_available: true,
            seek_scan_split: true,
            writes_counted: false,
            note: None,
            indexes: vec![dbine_driver::IndexUsage {
                name: "ix".into(),
                kind: "NONCLUSTERED".into(),
                key_columns: vec!["a".into(), "b DESC".into()],
                size_kb: Some(16),
                seeks: 4,
                last_read: Some("2026-01-02 04:00:00".into()),
                ..Default::default()
            }],
            foreign_keys: vec![dbine_driver::ForeignKeyDef { columns: vec!["p".into()], ref_table: "p".into(), ref_columns: vec!["id".into()], ..Default::default() }],
        });
        let body = rmp_serde::to_vec_named(&Reply::IndexUsage(report.clone())).unwrap();
        match rmp_serde::from_slice::<Reply>(&body).unwrap() {
            Reply::IndexUsage(back) => assert_eq!(back, report),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_schema_list_round_trips() {
        let list = Some(vec![dbine_driver::SchemaInfo { name: "ventas".into(), system: false }, dbine_driver::SchemaInfo { name: "sys".into(), system: true }]);
        let body = rmp_serde::to_vec_named(&Reply::Schemas(list.clone())).unwrap();
        match rmp_serde::from_slice::<Reply>(&body).unwrap() {
            Reply::Schemas(back) => assert_eq!(back, list),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn frames_round_trip_with_json_cells() {
        let mut buf = Vec::new();
        let msg = FromHost::SinkRow { id: 7, index: 1, row: vec![json!(1), json!("a"), json!(null), json!({"k": [1.5, true]})] };
        write_frame(&mut buf, &msg).unwrap();
        write_frame(&mut buf, &FromHost::Reply { id: 7, result: Err(WireError::from(&Error::Cancelled)) }).unwrap();
        let mut r = buf.as_slice();
        match read_frame::<FromHost>(&mut r).unwrap().unwrap() {
            FromHost::SinkRow { id, index, row } => {
                assert_eq!((id, index), (7, 1));
                assert_eq!(row[3], json!({"k": [1.5, true]}));
            }
            other => panic!("{other:?}"),
        }
        match read_frame::<FromHost>(&mut r).unwrap().unwrap() {
            FromHost::Reply { result: Err(w), .. } => assert!(matches!(Error::from(w), Error::Cancelled)),
            other => panic!("{other:?}"),
        }
        assert!(read_frame::<FromHost>(&mut r).unwrap().is_none());
    }

    #[test]
    fn errors_keep_their_kind() {
        for e in [Error::Connect("c".into()), Error::Unsupported("u".into()), Error::Query("q".into()), Error::State("s".into())] {
            let back = Error::from(WireError::from(&e));
            assert_eq!(std::mem::discriminant(&back), std::mem::discriminant(&e));
            assert_eq!(back.to_string(), e.to_string());
        }
        let io = Error::Io(io::Error::other("x"));
        assert_eq!(Error::from(WireError::from(&io)).to_string(), io.to_string());
    }

    #[test]
    fn statement_errors_cross_with_their_details() {
        let e = Error::from(dbine_driver::ScriptError::new("Invalid object name 't'.").with_code("208").at_line(3));
        let body = rmp_serde::to_vec_named(&WireError::from(&e)).unwrap();
        let back = Error::from(rmp_serde::from_slice::<WireError>(&body).unwrap());
        let Error::Statement(d) = back else { panic!("{back:?}") };
        assert_eq!((d.code.as_deref(), d.line), (Some("208"), Some(3)));
        // An app built before the details reads a plain query error.
        #[derive(Deserialize)]
        struct OldWire {
            kind: String,
            message: String,
        }
        let old: OldWire = rmp_serde::from_slice(&body).unwrap();
        assert_eq!((old.kind.as_str(), old.message.as_str()), ("query", "Invalid object name 't'."));
        // A host built before them sends no detail.
        #[derive(Serialize)]
        struct OldHost {
            kind: String,
            message: String,
        }
        let body = rmp_serde::to_vec_named(&OldHost { kind: "query".into(), message: "x".into() }).unwrap();
        assert!(matches!(Error::from(rmp_serde::from_slice::<WireError>(&body).unwrap()), Error::Query(_)));
    }

    #[test]
    fn run_replies_of_older_hosts_still_read() {
        #[derive(Serialize)]
        struct OldOutcome {
            results: Vec<Value>,
            messages: Vec<String>,
            error: Option<String>,
            elapsed_ms: u64,
        }
        let old = OldOutcome { results: vec![], messages: vec!["PRINT".into()], error: None, elapsed_ms: 1 };
        let body = rmp_serde::to_vec_named(&Reply::Run(QueryOutcome::default(), None)).unwrap();
        assert!(rmp_serde::from_slice::<Reply>(&body).is_ok());
        let o: QueryOutcome = rmp_serde::from_slice(&rmp_serde::to_vec_named(&old).unwrap()).unwrap();
        assert_eq!(o.messages, vec!["PRINT"]);
        assert!(o.log.is_empty() && o.errors.is_empty());
    }
}
