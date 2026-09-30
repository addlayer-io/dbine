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
    Execute { session: u64, text: String, max_rows: u64, sink: bool },
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
}

/// Host → app.
#[derive(Debug, Serialize, Deserialize)]
pub enum FromHost {
    Reply { id: u64, result: Result<Reply, WireError> },
    SinkBegin { id: u64, index: u64, columns: Vec<ResultColumn> },
    SinkRow { id: u64, index: u64, row: Vec<Value> },
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
        }
    }
}

/// A driver error across the wire: its kind, so the app rebuilds the same
/// variant (code matches on `Cancelled`, `Connect`, `Unsupported`…).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct WireError {
    pub kind: String,
    pub message: String,
}

impl From<&Error> for WireError {
    fn from(e: &Error) -> Self {
        let (kind, message) = match e {
            Error::Connect(m) => ("connect", m.clone()),
            Error::AuthFailed(m) => ("auth_failed", m.clone()),
            Error::Unsupported(m) => ("unsupported", m.clone()),
            Error::Query(m) => ("query", m.clone()),
            Error::State(m) => ("state", m.clone()),
            Error::Secrets(m) => ("secrets", m.clone()),
            Error::Cancelled => ("cancelled", String::new()),
            Error::Io(e) => ("io", e.to_string()),
            Error::Serde(e) => ("serde", e.to_string()),
        };
        WireError { kind: kind.into(), message }
    }
}

impl From<WireError> for Error {
    fn from(w: WireError) -> Self {
        match w.kind.as_str() {
            "connect" => Error::Connect(w.message),
            "auth_failed" => Error::AuthFailed(w.message),
            "unsupported" => Error::Unsupported(w.message),
            "query" => Error::Query(w.message),
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
}
