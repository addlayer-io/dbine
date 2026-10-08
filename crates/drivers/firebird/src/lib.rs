//! Firebird 3+ over its wire protocol, implemented in pure Rust by
//! `rsfbclient-rust` (SRP authentication, wire encryption): no fbclient
//! library at build time or at run time.
//!
//! The client is synchronous: every call runs on a blocking thread. A
//! Firebird connection is one database file, so there's a single namespace.

mod create_db;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod properties;
mod schema;
mod script;
mod security;
mod steps;
mod transfer;

use dbine_driver::sql::{quote_ident, Quote, ScriptDefaults, ScriptDialect};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef,
    Plan, QueryOutcome, Result, ResultColumn, ScriptError, Session, TableSchema, TxState,
};
use rsfbclient_core::{
    Charset, Column, Dialect, FbError, FirebirdClientDbOps, FirebirdClientSqlOps, FreeStmtOp, SqlType, StmtType,
    TrDataAccessMode, TrIsolationLevel, TrLockResolution, TrOp, TrRecordVersion, TransactionConfiguration,
};
use rsfbclient_rust::{RustFbClient, RustFbClientAttachmentConfig};
use serde_json::Value;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_PORT: u16 = 3050;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
const PACKAGE: &str = "package";

type DbHandle = <RustFbClient as FirebirdClientDbOps>::DbHandle;
type TrHandle = <RustFbClient as FirebirdClientSqlOps>::TrHandle;
type StmtHandle = <RustFbClient as FirebirdClientSqlOps>::StmtHandle;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(FirebirdDriver { info: info() })]
}

struct FirebirdDriver {
    info: DriverInfo,
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "firebird",
        name: "Firebird",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "standard",
        default_port: DEFAULT_PORT,
        fields: vec![
            Field::host(),
            Field::port().placeholder("3050"),
            Field::new("database", "Base de datos", FieldKind::Text)
                .required()
                .placeholder("/var/lib/firebird/data/base.fdb")
                .help("Ruta del archivo en el servidor o alias de databases.conf."),
            Field::username().default_value("SYSDBA"),
            Field::password(),
            Field::new("role", "Rol", FieldKind::Text).placeholder("(ninguno)").advanced(),
            Field::new(
                "charset",
                "Juego de caracteres",
                FieldKind::Select(vec![
                    ("UTF8", "UTF8"),
                    ("ISO8859_1", "ISO8859_1"),
                    ("ISO8859_2", "ISO8859_2"),
                    ("WIN1250", "WIN1250"),
                    ("WIN1251", "WIN1251"),
                    ("WIN1252", "WIN1252"),
                    ("KOI8R", "KOI8R"),
                ]),
            )
            .default_value("UTF8")
            .advanced(),
            Field::new("autocommit", "Confirmar automáticamente (autocommit)", FieldKind::Bool)
                .default_value("true")
                .help("Si está apagado, los cambios esperan un COMMIT explícito.")
                .advanced(),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::procedures(),
            ObjectKindInfo::functions(),
            ObjectKindInfo::new(PACKAGE, "Paquetes", false, false, true),
            ObjectKindInfo::triggers(),
            ObjectKindInfo::sequences(),
            // Domains: Firebird's user-defined types.
            ObjectKindInfo::new(dbine_driver::kinds::TYPE, "Dominios", false, false, true),
        ],
    }
}

// ---------------------------------------------------------------- errors

fn message(e: &FbError) -> String {
    match e {
        FbError::Sql { msg, .. } => msg.clone(),
        FbError::Io(e) => e.to_string(),
        FbError::Other(m) => m.clone(),
    }
}

fn err(e: FbError) -> Error {
    let m = message(&e);
    if is_cancel(&m) {
        return Error::Cancelled;
    }
    Error::Query(m)
}

/// The server's answer to a statement stopped by the interrupter
/// (isc_cancelled).
fn is_cancel(m: &str) -> bool {
    m.contains("operation was cancelled")
}

/// SQLCODE and SQLSTATE of errors whose status vector carries no
/// `isc_sqlerr` (the pure-Rust client only reads the SQLCODE from there and
/// drops the SQLSTATE), from their message as isql reports them.
fn known_error(m: &str) -> Option<(i32, &'static str)> {
    const KNOWN: &[(&str, i32, &str)] = &[
        ("violation of PRIMARY or UNIQUE KEY constraint", -803, "23000"),
        ("attempt to store duplicate value", -803, "23000"),
        ("violation of FOREIGN KEY constraint", -530, "23000"),
        ("violates CHECK constraint", -297, "23000"),
        ("validation error for column", -625, "42000"),
        ("lock conflict on no wait transaction", -913, "40001"),
        ("deadlock", -913, "40001"),
        ("update conflicts with concurrent update", -913, "40001"),
        ("arithmetic exception, numeric overflow, or string truncation", -802, "22000"),
        ("conversion error from string", -413, "22018"),
        ("unsuccessful metadata update", -607, "42000"),
    ];
    KNOWN.iter().find(|(text, ..)| m.contains(text)).map(|(_, code, state)| (*code, *state))
}

/// A statement of `script` (starting at byte `start`) failed: the SQLCODE
/// as the code, and where when Firebird says so ("line 2, column 8", in
/// the statement).
fn stmt_err(e: FbError, script: &str, start: usize) -> Error {
    let FbError::Sql { msg, code } = e else { return err(e) };
    if is_cancel(&msg) {
        return Error::Cancelled;
    }
    let mut se = ScriptError::new(msg.clone());
    let known = known_error(&msg);
    if code != -1 && code != 0 {
        se = se.with_code(code.to_string());
    } else if let Some((code, _)) = known {
        se = se.with_code(code.to_string());
    }
    if let Some((_, state)) = known {
        se = se.with_sqlstate(state);
    }
    let pos = msg.split("line ").nth(1).and_then(|r| {
        let (l, rest) = r.split_once(", column ")?;
        let c: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some((l.trim().parse::<usize>().ok()?, c.parse::<usize>().ok()?))
    });
    let stmt = &script[start.min(script.len())..];
    if let Some((line, col)) = pos.filter(|(l, c)| *l >= 1 && *c >= 1) {
        // The column counts characters.
        let line_start: usize = stmt.split_inclusive('\n').take(line - 1).map(str::len).sum();
        if line_start <= stmt.len() {
            let rest = &stmt[line_start..];
            let col_bytes = rest.char_indices().nth(col - 1).map_or(rest.len(), |(b, _)| b);
            let offset = start + line_start + col_bytes;
            se = se.at_offset(offset).at_line(script[..offset].matches('\n').count() as u32 + 1);
        }
    } else {
        se = se.at_line(script[..start.min(script.len())].matches('\n').count() as u32 + 1);
    }
    se.into()
}

fn connect_err(e: FbError) -> Error {
    let m = message(&e);
    if m.contains("user name and password") || m.contains("login") {
        Error::AuthFailed(m)
    } else {
        Error::Connect(m)
    }
}

fn join_err(e: tokio::task::JoinError) -> Error {
    Error::State(format!("la tarea de Firebird terminó inesperadamente: {e}"))
}

fn poisoned() -> Error {
    Error::State("la conexión quedó en un estado inválido".into())
}

// --------------------------------------------------------------- charset

/// Decodes like the connection charset, but never fails: text that isn't
/// valid in it (binary `CHARACTER SET OCTETS` columns) comes back as `0x…`
/// hex instead of failing the whole row.
struct Lenient(encoding::EncodingRef);

impl encoding::Encoding for Lenient {
    fn name(&self) -> &'static str {
        self.0.name()
    }
    fn raw_encoder(&self) -> Box<dyn encoding::RawEncoder> {
        self.0.raw_encoder()
    }
    fn raw_decoder(&self) -> Box<dyn encoding::RawDecoder> {
        self.0.raw_decoder()
    }
    fn decode(
        &self,
        input: &[u8],
        _trap: encoding::DecoderTrap,
    ) -> std::result::Result<String, std::borrow::Cow<'static, str>> {
        Ok(match self.0.decode(input, encoding::DecoderTrap::Strict) {
            Ok(s) => s,
            Err(_) => json_bytes(input).as_str().unwrap_or_default().to_string(),
        })
    }
}

/// The connection charset with a lenient decoder. One leaked `Lenient` per
/// charset name, at most a handful.
fn lenient_charset(name: &str) -> Result<Charset> {
    use std::collections::HashMap;
    use std::sync::OnceLock;
    static CACHE: OnceLock<Mutex<HashMap<&'static str, encoding::EncodingRef>>> = OnceLock::new();
    let base: Charset = name.parse().map_err(|e: FbError| Error::Connect(message(&e)))?;
    // UTF8 has no `encoding` codec in rsfbclient (it uses `String::from_utf8`).
    let on_rust: encoding::EncodingRef = base.on_rust.unwrap_or(encoding::all::UTF_8);
    let mut cache = CACHE.get_or_init(Default::default).lock().map_err(|_| poisoned())?;
    let enc = *cache
        .entry(base.on_firebird)
        .or_insert_with(|| Box::leak(Box::new(Lenient(on_rust))) as &'static Lenient as encoding::EncodingRef);
    Ok(Charset { on_firebird: base.on_firebird, on_rust: Some(enc) })
}

// ------------------------------------------------------------ connecting

/// Settings that make the server hand FB4+ types over as text the pure-Rust
/// client can read: exact NUMERIC/DECIMAL, INT128, DECFLOAT, and dates and
/// times already formatted. Firebird 3 rejects them (harmless).
const SET_BINDS: &[&str] = &[
    "SET BIND OF NUMERIC TO VARCHAR",
    "SET BIND OF DECIMAL TO VARCHAR",
    "SET BIND OF INT128 TO VARCHAR",
    "SET BIND OF DECFLOAT TO VARCHAR",
    "SET BIND OF TIMESTAMP WITH TIME ZONE TO VARCHAR(60)",
    "SET BIND OF TIME WITH TIME ZONE TO VARCHAR(40)",
    "SET BIND OF TIMESTAMP TO VARCHAR(30)",
    "SET BIND OF TIME TO VARCHAR(20)",
    "SET BIND OF DATE TO VARCHAR(12)",
];

/// Everything needed to open (again) a connection.
#[derive(Clone)]
struct Target {
    attach: RustFbClientAttachmentConfig,
    charset: Charset,
    tr_conf: TransactionConfiguration,
}

fn target(cfg: &ConnectionConfig) -> Result<Target> {
    let db = cfg.database.trim();
    if db.is_empty() {
        return Err(Error::Connect("Falta la ruta o el alias de la base de datos.".into()));
    }
    // The server upper-cases unquoted user names and the SRP proof must use
    // the same spelling; "Name" keeps its case.
    let user = cfg.username.as_deref().map(str::trim).filter(|u| !u.is_empty()).unwrap_or("SYSDBA");
    let user = match user.strip_prefix('"').and_then(|u| u.strip_suffix('"')) {
        Some(quoted) => quoted.to_string(),
        None => user.to_uppercase(),
    };
    Ok(Target {
        attach: RustFbClientAttachmentConfig {
            host: if cfg.host.trim().is_empty() { "localhost".into() } else { cfg.host.trim().into() },
            port: cfg.port_or(DEFAULT_PORT),
            db_name: db.into(),
            user,
            pass: cfg.password_or_empty().into(),
            role_name: cfg.option("role").map(str::to_string),
        },
        charset: lenient_charset(cfg.option("charset").unwrap_or("UTF8"))?,
        tr_conf: TransactionConfiguration {
            data_access: if cfg.read_only { TrDataAccessMode::ReadOnly } else { TrDataAccessMode::ReadWrite },
            isolation: TrIsolationLevel::ReadCommited(TrRecordVersion::RecordVersion),
            lock_resolution: TrLockResolution::Wait(None),
        },
    })
}

/// A live attachment with its current transaction.
struct Conn {
    client: RustFbClient,
    db: DbHandle,
    tr: TrHandle,
    tr_conf: TransactionConfiguration,
}

impl Conn {
    fn open(t: &Target) -> Result<Self> {
        let mut client = RustFbClient::new(t.charset.clone());
        let mut db = client.attach_database(&t.attach, Dialect::D3, false).map_err(connect_err)?;
        let tr = client.begin_transaction(&mut db, t.tr_conf).map_err(connect_err)?;
        let mut conn = Conn { client, db, tr, tr_conf: t.tr_conf };
        for sql in SET_BINDS {
            if let Err(e) = conn.client.exec_immediate(&mut conn.db, &mut conn.tr, Dialect::D3, sql) {
                tracing::debug!("firebird: {sql}: {}", message(&e));
            }
        }
        Ok(conn)
    }

    fn end_transaction(&mut self, op: TrOp) -> std::result::Result<(), FbError> {
        self.client.transaction_operation(&mut self.tr, op)?;
        self.tr = self.client.begin_transaction(&mut self.db, self.tr_conf)?;
        Ok(())
    }

    fn prepare(&mut self, sql: &str) -> std::result::Result<(StmtType, StmtHandle), FbError> {
        self.client.prepare_statement(&mut self.db, &mut self.tr, Dialect::D3, sql)
    }

    fn free(&mut self, stmt: &mut StmtHandle) {
        if let Err(e) = self.client.free_statement(stmt, FreeStmtOp::Drop) {
            tracing::debug!("firebird: free statement: {}", message(&e));
        }
    }

    /// Rows of a catalog query.
    fn rows(&mut self, sql: &str, params: Vec<SqlType>) -> Result<Vec<Vec<Column>>> {
        let (_, mut stmt) = self.prepare(sql).map_err(err)?;
        let result = (|| {
            self.client.execute(&mut self.db, &mut self.tr, &mut stmt, params)?;
            let mut rows = Vec::new();
            while let Some(row) = self.client.fetch(&mut self.db, &mut self.tr, &mut stmt)? {
                rows.push(row);
            }
            Ok(rows)
        })();
        self.free(&mut stmt);
        result.map_err(err)
    }
}

impl Drop for Conn {
    fn drop(&mut self) {
        let _ = self.client.transaction_operation(&mut self.tr, TrOp::Rollback);
        let _ = self.client.detach_database(&mut self.db);
    }
}

fn text(c: &Column) -> Option<String> {
    match &c.value {
        SqlType::Text(s) => Some(s.trim_end().to_string()),
        SqlType::Integer(i) => Some(i.to_string()),
        SqlType::Binary(b) => Some(String::from_utf8_lossy(b).into_owned()),
        SqlType::Null => None,
        other => Some(format!("{other:?}")),
    }
}

fn int(c: &Column) -> Option<i64> {
    match &c.value {
        SqlType::Integer(i) => Some(*i),
        SqlType::Floating(f) => Some(*f as i64),
        SqlType::Text(s) => s.trim().parse().ok(),
        _ => None,
    }
}

#[async_trait]
impl Driver for FirebirdDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// isql's: `SET TERM` switches the terminator.
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::firebird()
    }

    // `script_mode` stays `Whole`: the shared lexer doesn't yet keep a PSQL
    // unit written without SET TERM whole when it declares variables before
    // its BEGIN (or is a RECREATE, an EXECUTE BLOCK, a package), which this
    // driver's splitter (`script`) does. The driver runs the script
    // statement by statement itself, as the app would: on editor runs it
    // goes on after errors (`out.continue_on_error`) and reports each
    // statement live (`out.progress_sink`).

    /// isql goes on after an error unless `SET BAIL ON`.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    fn supports_manual_transactions(&self) -> bool {
        true
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<Field> {
        create_db::fields()
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(name, options)
    }

    /// The database is the file the statements run on (see [`properties`]).
    fn alter_database_script(&self, _database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        properties::script(changes)
    }

    fn capabilities(&self) -> Capabilities {
        // A database is a file the client asks the server to create or
        // drop (op_create / op_drop_database), not SQL over a connection.
        // Processes from MON$ATTACHMENTS; cancel and kill delete from
        // MON$STATEMENTS / MON$ATTACHMENTS.
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            processes: true,
            cancel_query: true,
            kill_session: true,
            // MON$DATABASE and ALTER DATABASE (see `properties`).
            database_properties: true,
            ..Default::default()
        }
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

    /// The indexes and keys, without counters (see [`index_usage`]).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        schema::sync_script(changes)
    }

    /// `ALTER INDEX … INACTIVE` / `ACTIVE` (see [`index_usage::toggle_script`]).
    fn supports_index_toggle(&self) -> bool {
        true
    }

    fn index_toggle_script(&self, _table: &ObjectRef, index: &dbine_driver::IndexUsage, enable: bool) -> Result<dbine_driver::SyncScript> {
        index_usage::toggle_script(index, enable)
    }

    /// INSERTs of many rows per `EXECUTE BLOCK` (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        // No multi-row VALUES in Firebird: one INSERT per row.
        Ok(dbine_driver::ddl::insert_script(&schema::flavor(), None, &target.name, columns, rows, 1))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        // No schemas in Firebird: the bare table name, as in the INSERTs.
        Ok(dbine_driver::ddl::update_script(&schema::flavor(), None, &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        // No schemas in Firebird: the bare table name, as in the INSERTs.
        Ok(dbine_driver::ddl::delete_script(&schema::flavor(), None, &target.name, keys))
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let mut target = target(cfg)?;
        // Another database file of the same server (one created from DBine).
        let database = database.map(str::trim).filter(|d| !d.is_empty()).unwrap_or(cfg.database.trim()).to_string();
        target.attach.db_name = database.clone();
        let t = target.clone();
        // The client has no connect timeout of its own.
        let task = tokio::task::spawn_blocking(move || -> Result<(Conn, i64)> {
            let mut conn = Conn::open(&t)?;
            let id = conn
                .rows("SELECT CURRENT_CONNECTION FROM RDB$DATABASE", vec![])?
                .first()
                .and_then(|r| r.first())
                .and_then(int)
                .unwrap_or_default();
            Ok((conn, id))
        });
        let (conn, attachment_id) = tokio::time::timeout(CONNECT_TIMEOUT, task)
            .await
            .map_err(|_| Error::Connect(format!("El servidor no respondió en {} s.", CONNECT_TIMEOUT.as_secs())))?
            .map_err(join_err)??;
        Ok(Box::new(FirebirdSession {
            conn: Arc::new(Mutex::new(conn)),
            target,
            attachment_id,
            autocommit: cfg.option("autocommit").is_none_or(|v| v == "true"),
            dirty: Arc::new(std::sync::atomic::AtomicBool::new(false)),
            database,
            profiler: None,
        }))
    }
}

// --------------------------------------------------------------- session

struct FirebirdSession {
    conn: Arc<Mutex<Conn>>,
    /// For the interrupter's side connection.
    target: Target,
    /// CURRENT_CONNECTION, to find the running statement in MON$STATEMENTS.
    attachment_id: i64,
    autocommit: bool,
    /// Without autocommit: statements that change something ran since the
    /// last commit or rollback (Firebird always has a transaction open; this
    /// is whether it holds work).
    dirty: Arc<std::sync::atomic::AtomicBool>,
    database: String,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

impl FirebirdSession {
    /// Commit or roll back the session's transaction (a new one starts).
    async fn end(&mut self, op: TrOp) -> Result<()> {
        let conn = self.conn.clone();
        tokio::task::spawn_blocking(move || conn.lock().map_err(|_| poisoned())?.end_transaction(op).map_err(err))
            .await
            .map_err(join_err)??;
        self.dirty.store(false, Ordering::SeqCst);
        Ok(())
    }

    /// Run `f` with the connection on a blocking thread. With autocommit,
    /// the transaction ends afterwards, so the next call sees fresh data.
    async fn run<T, F>(&self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&mut Conn) -> Result<T> + Send + 'static,
    {
        let conn = self.conn.clone();
        let autocommit = self.autocommit;
        tokio::task::spawn_blocking(move || {
            let mut c = conn.lock().map_err(|_| poisoned())?;
            let r = f(&mut c);
            if autocommit {
                if let Err(e) = c.end_transaction(TrOp::Commit) {
                    tracing::debug!("firebird: commit: {}", message(&e));
                }
            }
            r
        })
        .await
        .map_err(join_err)?
    }

    /// Rows of a catalog query with text parameters.
    async fn rows(&self, sql: &'static str, params: Vec<String>) -> Result<Vec<Vec<Column>>> {
        self.run(move |c| c.rows(sql, params.into_iter().map(SqlType::Text).collect())).await
    }
}

fn q(name: &str) -> String {
    quote_ident(Quote::Double, name)
}

const LIST_OBJECTS: &str = "
SELECT CAST('table' AS VARCHAR(20)), TRIM(RDB$RELATION_NAME), CAST(NULL AS VARCHAR(63))
  FROM RDB$RELATIONS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$VIEW_BLR IS NULL
UNION ALL
SELECT 'view', TRIM(RDB$RELATION_NAME), NULL
  FROM RDB$RELATIONS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$VIEW_BLR IS NOT NULL
UNION ALL
SELECT 'procedure', TRIM(RDB$PROCEDURE_NAME), NULL
  FROM RDB$PROCEDURES WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL
UNION ALL
SELECT 'function', TRIM(RDB$FUNCTION_NAME), NULL
  FROM RDB$FUNCTIONS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$PACKAGE_NAME IS NULL
UNION ALL
SELECT 'package', TRIM(RDB$PACKAGE_NAME), NULL
  FROM RDB$PACKAGES WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0
UNION ALL
SELECT 'trigger', TRIM(RDB$TRIGGER_NAME), TRIM(RDB$RELATION_NAME)
  FROM RDB$TRIGGERS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0
UNION ALL
SELECT 'sequence', TRIM(RDB$GENERATOR_NAME), NULL
  FROM RDB$GENERATORS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0
UNION ALL
SELECT 'type', TRIM(RDB$FIELD_NAME), NULL
  FROM RDB$FIELDS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0 AND RDB$FIELD_NAME NOT STARTING WITH 'RDB$'
ORDER BY 2";

/// A domain with its character set and collation (named only when it
/// isn't the character set's default).
const DOMAIN: &str = "
SELECT f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION,
       f.RDB$FIELD_SCALE, f.RDB$DEFAULT_SOURCE, f.RDB$NULL_FLAG, f.RDB$VALIDATION_SOURCE,
       TRIM(cs.RDB$CHARACTER_SET_NAME), TRIM(co.RDB$COLLATION_NAME), TRIM(cs.RDB$DEFAULT_COLLATE_NAME)
  FROM RDB$FIELDS f
  LEFT JOIN RDB$CHARACTER_SETS cs ON cs.RDB$CHARACTER_SET_ID = f.RDB$CHARACTER_SET_ID
  LEFT JOIN RDB$COLLATIONS co ON co.RDB$CHARACTER_SET_ID = f.RDB$CHARACTER_SET_ID AND co.RDB$COLLATION_ID = f.RDB$COLLATION_ID
 WHERE f.RDB$FIELD_NAME = ?";

/// `CREATE DOMAIN` from a [`DOMAIN`] row.
fn domain_sql(name: &str, r: &[Column]) -> String {
    let g = |i: usize| r.get(i).and_then(int);
    let t = |i: usize| r.get(i).and_then(text).map(|s| s.trim().to_string()).filter(|s| !s.is_empty());
    let mut s = format!("CREATE DOMAIN {} AS {}", q(name), field_type(g(0), g(1), g(2), g(3), g(4), g(5)));
    let textual = matches!(g(0), Some(14 | 37 | 40)) || (g(0) == Some(261) && g(1) == Some(1));
    if let Some(cs) = t(9).filter(|_| textual) {
        s.push_str(&format!(" CHARACTER SET {cs}"));
    }
    if let Some(d) = t(6) {
        s.push_str(&format!(" {d}"));
    }
    if g(7) == Some(1) {
        s.push_str(" NOT NULL");
    }
    if let Some(c) = t(8) {
        s.push_str(&format!(" {c}"));
    }
    if let (Some(co), true) = (t(10), textual) {
        if Some(&co) != t(11).as_ref() {
            s.push_str(&format!(" COLLATE {co}"));
        }
    }
    s.push(';');
    s
}

const COLUMNS: &str = "
SELECT TRIM(rf.RDB$FIELD_NAME), f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH,
       f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE,
       COALESCE(rf.RDB$NULL_FLAG, f.RDB$NULL_FLAG, 0),
       COALESCE(rf.RDB$DEFAULT_SOURCE, f.RDB$DEFAULT_SOURCE),
       rf.RDB$IDENTITY_TYPE,
       (SELECT COUNT(*) FROM RDB$RELATION_CONSTRAINTS rc
          JOIN RDB$INDEX_SEGMENTS s ON s.RDB$INDEX_NAME = rc.RDB$INDEX_NAME
         WHERE rc.RDB$RELATION_NAME = rf.RDB$RELATION_NAME AND rc.RDB$CONSTRAINT_TYPE = 'PRIMARY KEY'
           AND s.RDB$FIELD_NAME = rf.RDB$FIELD_NAME),
       f.RDB$COMPUTED_SOURCE
  FROM RDB$RELATION_FIELDS rf
  JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = rf.RDB$FIELD_SOURCE
 WHERE rf.RDB$RELATION_NAME = ?
 ORDER BY rf.RDB$FIELD_POSITION";

/// Parameters of a procedure (type 0 = in, 1 = out) or function (position
/// 0 = return value), with their types.
const PROCEDURE_PARAMS: &str = "
SELECT TRIM(p.RDB$PARAMETER_NAME), p.RDB$PARAMETER_TYPE, f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE,
       f.RDB$FIELD_LENGTH, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE
  FROM RDB$PROCEDURE_PARAMETERS p
  JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = p.RDB$FIELD_SOURCE
 WHERE p.RDB$PROCEDURE_NAME = ? AND p.RDB$PACKAGE_NAME IS NULL
 ORDER BY p.RDB$PARAMETER_TYPE, p.RDB$PARAMETER_NUMBER";

const FUNCTION_ARGS: &str = "
SELECT TRIM(a.RDB$ARGUMENT_NAME), a.RDB$ARGUMENT_POSITION, f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE,
       f.RDB$FIELD_LENGTH, f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE
  FROM RDB$FUNCTION_ARGUMENTS a
  JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = a.RDB$FIELD_SOURCE
 WHERE a.RDB$FUNCTION_NAME = ? AND a.RDB$PACKAGE_NAME IS NULL
 ORDER BY a.RDB$ARGUMENT_POSITION";

#[async_trait]
impl Session for FirebirdSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.rows("SELECT RDB$GET_CONTEXT('SYSTEM', 'ENGINE_VERSION') FROM RDB$DATABASE", vec![]).await?;
        let v = rows.first().and_then(|r| r.first()).and_then(text).unwrap_or_default();
        Ok(format!("Firebird {v}"))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec![self.database.clone()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let rows = self.rows(LIST_OBJECTS, vec![]).await?;
        Ok(rows
            .iter()
            .filter_map(|r| {
                Some(DbObject {
                    kind: text(r.first()?)?,
                    schema: None,
                    name: text(r.get(1)?)?,
                    parent: r.get(2).and_then(text).filter(|p| !p.is_empty()),
                })
            })
            .collect())
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let rows = self.rows(COLUMNS, vec![obj.name.clone()]).await?;
        Ok(rows
            .iter()
            .map(|r| {
                let g = |i: usize| r.get(i).and_then(int);
                let computed = r.get(11).and_then(text);
                let mut data_type = field_type(g(1), g(2), g(3), g(4), g(5), g(6));
                if let Some(expr) = &computed {
                    data_type = format!("COMPUTED BY {expr}");
                }
                ColumnInfo {
                    name: r.first().and_then(text).unwrap_or_default(),
                    data_type,
                    nullable: g(7).unwrap_or(0) == 0,
                    primary_key: g(10).unwrap_or(0) > 0,
                    auto_increment: g(9).is_some(),
                    default_value: r
                        .get(8)
                        .and_then(text)
                        .map(|d| d.trim().trim_start_matches("DEFAULT").trim_start_matches("default").trim().to_string())
                        .filter(|d| !d.is_empty()),
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let name = obj.name.clone();
        let kind = obj.kind.clone();
        self.run(move |c| definition(c, &kind, &name)).await
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        format!("SELECT FIRST {limit} *\nFROM {}", q(&obj.name))
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let script = text.to_string();
        let autocommit = self.autocommit;
        let dirty = self.dirty.clone();
        let conn = self.conn.clone();
        let fork = out.fork();
        // The app didn't number the statements (`Whole`): this driver does,
        // and on editor runs goes on after errors and reports each one live.
        let own = out.current_statement.is_none();
        let (local, result) = tokio::task::spawn_blocking(move || {
            let mut local = fork;
            let result = match conn.lock() {
                Ok(mut c) => script::pieces(&script).iter().enumerate().try_for_each(|(i, p)| {
                    let step = steps::Step::start(&mut local, own, i, p.start, steps::line_at(&script, p.start));
                    let r = if p.skipped {
                        local.info(format!("Comando de isql omitido (no es SQL del servidor): {}", first_line(&p.text)));
                        Ok(())
                    } else {
                        let r = run_statement(&mut c, &p.text, max_rows, autocommit, &mut local);
                        match &r {
                            Ok(Some(StmtType::Commit | StmtType::Rollback)) => dirty.store(false, Ordering::SeqCst),
                            Ok(Some(StmtType::Select)) | Ok(None) => {}
                            Ok(Some(_)) if !autocommit => dirty.store(true, Ordering::SeqCst),
                            _ => {}
                        }
                        // Placed in the statement; the step moves it into the script.
                        r.map(|_| ()).map_err(|e| match e {
                            StmtFailure::Fb(e) => stmt_err(e, &p.text, 0),
                            StmtFailure::Other(e) => e,
                        })
                    };
                    step.end(&mut local, r)
                }),
                Err(_) => Err(poisoned()),
            };
            (local, result)
        })
        .await
        .map_err(join_err)?;
        out.merge(local);
        result
    }

    /// Firebird always works inside a transaction: `Open` when, without
    /// autocommit, something changed since the last commit or rollback.
    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        let open = !self.autocommit && self.dirty.load(Ordering::SeqCst);
        Ok(Some(if open { TxState::Open } else { TxState::Idle }))
    }

    /// On: each statement commits (as the connection's autocommit option).
    /// Off: the session's transaction keeps its work until Commit / Rollback.
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        self.autocommit = on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.end(TrOp::Commit).await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.end(TrOp::Rollback).await
    }

    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let statements = script::split(text);
        let autocommit = self.autocommit;
        let conn = self.conn.clone();
        let fork = out.fork();
        let (local, result) = tokio::task::spawn_blocking(move || {
            let mut local = fork;
            let result = match conn.lock() {
                Ok(mut c) => statements.iter().try_for_each(|s| explain_statement(&mut c, s, analyze, max_rows, autocommit, &mut local)),
                Err(_) => Err(poisoned()),
            };
            (local, result)
        })
        .await
        .map_err(join_err)?;
        out.merge(local);
        result
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        self.run(monitor::snapshot).await
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        self.run(processes::processes).await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        let (own, id) = (self.attachment_id, id.to_string());
        self.run(move |c| processes::cancel(c, own, &id)).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        let (own, id) = (self.attachment_id, id.to_string());
        self.run(move |c| processes::kill(c, own, &id)).await
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let state = self.profiler.as_mut().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        profiler::poll(state).await
    }

    /// Nothing was switched on: dropping the profiler's attachment is all.
    async fn profiler_stop(&mut self) -> Result<()> {
        self.profiler = None;
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        // Deleting the attachment's row in MON$STATEMENTS cancels what it
        // runs (Firebird 2.5+; own attachments, or any with SYSDBA).
        let target = self.target.clone();
        let id = self.attachment_id;
        Some(Arc::new(move || {
            let target = target.clone();
            std::thread::spawn(move || {
                let cancelled = Conn::open(&target).and_then(|mut c| {
                    let sql = format!("DELETE FROM MON$STATEMENTS WHERE MON$ATTACHMENT_ID = {id} AND MON$STATE = 1");
                    c.client.exec_immediate(&mut c.db, &mut c.tr, Dialect::D3, &sql).map_err(err)?;
                    c.end_transaction(TrOp::Commit).map_err(err)
                });
                if let Err(e) = cancelled {
                    tracing::warn!("firebird: no se pudo cancelar la sentencia: {e}");
                }
            });
        }))
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        self.run(schema::database_schema).await
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let tables = self.run(schema::database_schema).await?;
        Ok(Some(index_usage::report(tables.iter().find(|t| t.name == table.name))))
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
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

    /// `name` is the new file's path on the server (or an alias from
    /// databases.conf), created with the session's credentials.
    async fn create_database(&mut self, name: &str) -> Result<()> {
        let mut t = self.target.clone();
        t.attach.db_name = name.trim().to_string();
        tokio::task::spawn_blocking(move || {
            let mut client = RustFbClient::new(t.charset.clone());
            let mut db = client.create_database(&t.attach, None, Dialect::D3).map_err(err)?;
            client.detach_database(&mut db).map_err(err)
        })
        .await
        .map_err(join_err)?
    }

    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        self.create_database_choices_impl().await
    }

    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.create_database_with_impl(name, options).await
    }

    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        self.properties(database).await
    }

    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.alter_database_impl(database, changes).await
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        let name = name.trim().to_string();
        if name == self.database {
            return Err(Error::Query("No se puede borrar la base de datos de la conexión actual.".into()));
        }
        let mut t = self.target.clone();
        t.attach.db_name = name;
        tokio::task::spawn_blocking(move || {
            let mut client = RustFbClient::new(t.charset.clone());
            let mut db = client.attach_database(&t.attach, Dialect::D3, true).map_err(err)?;
            client.drop_database(&mut db).map_err(err)
        })
        .await
        .map_err(join_err)?
    }

    /// SYSDBA / RDB$ADMIN, the owner, the security database's flags and the
    /// Firebird 4+ system privileges (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

// ------------------------------------------------------------- execution

/// One statement's plan (read while it's only prepared) and, with
/// `analyze`, its run. Firebird has no per-operator runtime figures: the
/// run adds the elapsed time and the rows returned to the plan's root.
fn explain_statement(c: &mut Conn, sql: &str, analyze: bool, max_rows: usize, autocommit: bool, out: &mut QueryOutcome) -> Result<()> {
    let text = explained_plan(c, sql)?;
    let before = out.results.len();
    let started = std::time::Instant::now();
    if analyze {
        run_statement(c, sql, max_rows, autocommit, out)?;
    }
    let Some(text) = text else {
        if !analyze {
            out.messages.push(format!("Sin plan para «{}»: la sentencia no lee tablas.", sql.trim()));
        }
        return Ok(());
    };
    let mut root = plan::tree(&text);
    if analyze {
        let ms = started.elapsed().as_secs_f64() * 1000.0;
        root.props.push(("Tiempo de ejecución (ms)".into(), format!("{ms:.1}")));
        if let Some(r) = out.results.get(before..).and_then(|r| r.iter().find(|r| !r.columns.is_empty())) {
            root.props.push(("Filas devueltas".into(), r.total_rows.to_string()));
        }
        if !out.messages.iter().any(|m| m.starts_with("Firebird no da")) {
            out.messages.push("Firebird no da cifras reales por operador: se muestra el plan estimado con el tiempo y las filas de la ejecución.".into());
        }
    }
    out.plans.push(Plan { statement: sql.trim().to_string(), root, actual: false, raw_format: "text".into(), raw: text.trim().to_string() });
    Ok(())
}

/// The statement's explained plan: prepared (never executed), then looked
/// up in MON$STATEMENTS from a transaction of its own (the monitoring
/// snapshot is taken once per transaction).
fn explained_plan(c: &mut Conn, sql: &str) -> Result<Option<String>> {
    let (_, mut stmt) = c.prepare(sql).map_err(err)?;
    let conf = TransactionConfiguration {
        data_access: TrDataAccessMode::ReadOnly,
        isolation: TrIsolationLevel::Concurrency,
        lock_resolution: TrLockResolution::NoWait,
    };
    let result = (|| -> std::result::Result<Vec<Vec<Column>>, FbError> {
        let mut tr = c.client.begin_transaction(&mut c.db, conf)?;
        let rows = (|| {
            let (_, mut mon) = c.client.prepare_statement(&mut c.db, &mut tr, Dialect::D3, MON_PLANS)?;
            let rows = (|| {
                c.client.execute(&mut c.db, &mut tr, &mut mon, vec![])?;
                let mut rows = Vec::new();
                while let Some(row) = c.client.fetch(&mut c.db, &mut tr, &mut mon)? {
                    rows.push(row);
                }
                Ok(rows)
            })();
            let _ = c.client.free_statement(&mut mon, FreeStmtOp::Drop);
            rows
        })();
        let _ = c.client.transaction_operation(&mut tr, TrOp::Commit);
        rows
    })();
    c.free(&mut stmt);
    let rows = result.map_err(|e| {
        let m = message(&e);
        if m.contains("MON$EXPLAINED_PLAN") {
            Error::Unsupported("Este servidor no da planes explicados (MON$EXPLAINED_PLAN llegó en Firebird 3).".into())
        } else {
            Error::Query(m)
        }
    })?;
    let want = sql.trim();
    let pick = rows
        .iter()
        .find(|r| r.first().and_then(text).is_some_and(|t| t.trim() == want))
        .or_else(|| rows.iter().find(|r| r.first().and_then(text).is_some_and(|t| !t.contains("MON$EXPLAINED_PLAN"))));
    Ok(pick.and_then(|r| r.get(1)).and_then(text).filter(|t| !t.trim().is_empty()))
}

const MON_PLANS: &str = "SELECT MON$SQL_TEXT, MON$EXPLAINED_PLAN FROM MON$STATEMENTS \
                         WHERE MON$ATTACHMENT_ID = CURRENT_CONNECTION ORDER BY MON$STATEMENT_ID DESC";

/// Why a statement failed: the server's error (placed by the caller in
/// the script) or anything else.
enum StmtFailure {
    Fb(FbError),
    Other(Error),
}

impl From<FbError> for StmtFailure {
    fn from(e: FbError) -> Self {
        Self::Fb(e)
    }
}

impl From<Error> for StmtFailure {
    fn from(e: Error) -> Self {
        Self::Other(e)
    }
}

impl From<StmtFailure> for Error {
    fn from(e: StmtFailure) -> Self {
        match e {
            StmtFailure::Fb(e) => err(e),
            StmtFailure::Other(e) => e,
        }
    }
}

fn first_line(s: &str) -> &str {
    s.lines().next().unwrap_or("").trim()
}

/// Run one statement; its type when it ran (a selectable `EXECUTE
/// PROCEDURE` reads as `Select`).
fn run_statement(c: &mut Conn, sql: &str, max_rows: usize, autocommit: bool, out: &mut QueryOutcome) -> std::result::Result<Option<StmtType>, StmtFailure> {
    let (mut ty, mut stmt) = c.prepare(sql)?;
    // The pure-Rust client can't read the single output row of EXECUTE
    // PROCEDURE (op_execute2): run it inside an EXECUTE BLOCK that returns
    // the procedure's outputs as a result set.
    if ty == StmtType::ExecProcedure {
        if let Some(block) = procedure_block(c, sql)? {
            c.free(&mut stmt);
            (ty, stmt) = c.prepare(&block)?;
        }
    }
    let result = run_prepared(c, ty, &mut stmt, max_rows, out);
    c.free(&mut stmt);
    let manual = matches!(ty, StmtType::Commit | StmtType::Rollback);
    if result.is_ok() && autocommit && !manual {
        c.end_transaction(TrOp::Commit)?;
    }
    result.map(|()| Some(ty))
}

fn run_prepared(c: &mut Conn, ty: StmtType, stmt: &mut StmtHandle, max_rows: usize, out: &mut QueryOutcome) -> std::result::Result<(), StmtFailure> {
    match ty {
        StmtType::Select | StmtType::SelectForUpd => {
            c.client.execute(&mut c.db, &mut c.tr, stmt, vec![])?;
            let mut started = false;
            while let Some(row) = c.client.fetch(&mut c.db, &mut c.tr, stmt)? {
                if !started {
                    out.begin_result(columns_of(&row));
                    started = true;
                }
                out.push_row(row.into_iter().map(|col| cell(col.value)).collect(), max_rows);
            }
            if !started {
                // The client only reports column names with the rows.
                out.begin_result(Vec::new());
            }
        }
        // EXECUTE PROCEDURE without outputs, or INSERT/UPDATE … RETURNING of
        // one row: the client can't read that output row (see
        // `procedure_block`), so it runs without it.
        StmtType::ExecProcedure => {
            let n = c.client.execute(&mut c.db, &mut c.tr, stmt, vec![])?;
            if n > 0 {
                out.push_affected(n as u64);
                out.info(
                    "Los valores de RETURNING no se muestran: el cliente de Firebird de DBine no los lee. \
                     Consultá las filas con un SELECT.",
                );
            } else {
                out.results.push(Default::default());
            }
        }
        StmtType::Insert | StmtType::Update | StmtType::Delete => {
            let n = c.client.execute(&mut c.db, &mut c.tr, stmt, vec![])?;
            out.push_affected(n as u64);
        }
        // COMMIT / ROLLBACK end our transaction handle: do it ourselves.
        StmtType::Commit => {
            c.end_transaction(TrOp::Commit)?;
            out.results.push(Default::default());
        }
        StmtType::Rollback => {
            c.end_transaction(TrOp::Rollback)?;
            out.results.push(Default::default());
        }
        StmtType::StartTrans => {
            return Err(Error::Unsupported(
                "SET TRANSACTION no está soportado: DBine abre la transacción de la sesión (usá COMMIT o ROLLBACK)."
                    .into(),
            )
            .into());
        }
        _ => {
            c.client.execute(&mut c.db, &mut c.tr, stmt, vec![])?;
            out.results.push(Default::default());
        }
    }
    Ok(())
}

/// `EXECUTE BLOCK RETURNS (<outputs>) AS BEGIN EXECUTE PROCEDURE p … RETURNING_VALUES …; SUSPEND; END`
/// for `EXECUTE PROCEDURE p …`, when the procedure has output parameters.
fn procedure_block(c: &mut Conn, sql: &str) -> Result<Option<String>> {
    let Some((name, args)) = script::execute_procedure(sql) else { return Ok(None) };
    let (package, proc) = match script::split_name(&name).as_slice() {
        [p] => (None, p.clone()),
        [pkg, p] => (Some(pkg.clone()), p.clone()),
        _ => return Ok(None),
    };
    let outputs: Vec<(String, String)> = c
        .rows(
            "SELECT TRIM(p.RDB$PARAMETER_NAME), f.RDB$FIELD_TYPE, f.RDB$FIELD_SUB_TYPE, f.RDB$FIELD_LENGTH,
                    f.RDB$CHARACTER_LENGTH, f.RDB$FIELD_PRECISION, f.RDB$FIELD_SCALE
               FROM RDB$PROCEDURE_PARAMETERS p
               JOIN RDB$FIELDS f ON f.RDB$FIELD_NAME = p.RDB$FIELD_SOURCE
              WHERE p.RDB$PROCEDURE_NAME = ? AND p.RDB$PARAMETER_TYPE = 1
                AND COALESCE(p.RDB$PACKAGE_NAME, '') = ?
              ORDER BY p.RDB$PARAMETER_NUMBER",
            vec![SqlType::Text(proc), SqlType::Text(package.unwrap_or_default())],
        )?
        .iter()
        .map(|r| {
            let g = |i: usize| r.get(i).and_then(int);
            (r.first().and_then(text).unwrap_or_default(), field_type(g(1), g(2), g(3), g(4), g(5), g(6)))
        })
        .collect();
    if outputs.is_empty() {
        return Ok(None);
    }
    let decl: Vec<String> = outputs.iter().map(|(n, t)| format!("{} {t}", q(n))).collect();
    let vars: Vec<String> = outputs.iter().map(|(n, _)| format!(":{}", q(n))).collect();
    Ok(Some(format!(
        "EXECUTE BLOCK RETURNS ({}) AS BEGIN EXECUTE PROCEDURE {name} {args} RETURNING_VALUES {}; SUSPEND; END",
        decl.join(", "),
        vars.join(", ")
    )))
}

fn columns_of(row: &[Column]) -> Vec<ResultColumn> {
    row.iter().map(|c| ResultColumn { name: c.name.clone(), type_name: String::new() }).collect()
}

/// A cell as JSON. On Firebird 4+ decimals, dates and times already come
/// as text (see `SET_BINDS`); on Firebird 3 they arrive as numbers and
/// timestamps.
fn cell(v: SqlType) -> Value {
    match v {
        SqlType::Null => Value::Null,
        SqlType::Text(s) => s.into(),
        SqlType::Integer(i) => json_i64(i),
        SqlType::Floating(f) => json_f64(f),
        SqlType::Boolean(b) => Value::Bool(b),
        SqlType::Binary(b) => json_bytes(&b),
        SqlType::Timestamp(t) => {
            // Firebird 3: DATE and TIME come as timestamps too.
            if t.date() == chrono_base_date() {
                t.format("%H:%M:%S%.f").to_string().into()
            } else if t.time() == Default::default() {
                t.format("%Y-%m-%d").to_string().into()
            } else {
                t.format("%Y-%m-%d %H:%M:%S%.f").to_string().into()
            }
        }
    }
}

/// The date the client gives TIME values (Firebird's day zero).
fn chrono_base_date() -> chrono::NaiveDate {
    chrono::NaiveDate::from_ymd_opt(1858, 11, 17).expect("valid date")
}

// ------------------------------------------------------------ definitions

/// Type of a column / parameter from RDB$FIELDS.
fn field_type(
    ty: Option<i64>,
    sub: Option<i64>,
    len: Option<i64>,
    char_len: Option<i64>,
    precision: Option<i64>,
    scale: Option<i64>,
) -> String {
    let sub = sub.unwrap_or(0);
    let scale = -scale.unwrap_or(0);
    let exact = |name: &str| -> String {
        if sub == 1 || sub == 2 || scale > 0 {
            let kind = if sub == 2 { "DECIMAL" } else { "NUMERIC" };
            let p = precision.filter(|p| *p > 0).unwrap_or(match ty {
                Some(7) => 4,
                Some(8) => 9,
                Some(26) => 38,
                _ => 18,
            });
            format!("{kind}({p},{scale})")
        } else {
            name.to_string()
        }
    };
    let chars = char_len.or(len).unwrap_or(0);
    match ty {
        Some(7) => exact("SMALLINT"),
        Some(8) => exact("INTEGER"),
        Some(16) => exact("BIGINT"),
        Some(26) => exact("INT128"),
        Some(10) => "FLOAT".into(),
        Some(27) => "DOUBLE PRECISION".into(),
        Some(12) => "DATE".into(),
        Some(13) => "TIME".into(),
        Some(35) => "TIMESTAMP".into(),
        Some(28) => "TIME WITH TIME ZONE".into(),
        Some(29) => "TIMESTAMP WITH TIME ZONE".into(),
        Some(14) => format!("CHAR({chars})"),
        Some(37) => format!("VARCHAR({chars})"),
        Some(40) => format!("CSTRING({chars})"),
        Some(23) => "BOOLEAN".into(),
        Some(24) => "DECFLOAT(16)".into(),
        Some(25) => "DECFLOAT(34)".into(),
        Some(261) => match sub {
            1 => "BLOB SUB_TYPE TEXT".into(),
            0 => "BLOB SUB_TYPE BINARY".into(),
            n => format!("BLOB SUB_TYPE {n}"),
        },
        Some(n) => format!("/* type {n} */"),
        None => String::new(),
    }
}

/// `BEFORE INSERT OR UPDATE`, `ON CONNECT`… from RDB$TRIGGER_TYPE.
fn trigger_event(t: i64) -> String {
    const DB: i64 = 0x2000;
    const DDL: i64 = 0x4000;
    if t & DB != 0 {
        return match t & !DB {
            0 => "ON CONNECT",
            1 => "ON DISCONNECT",
            2 => "ON TRANSACTION START",
            3 => "ON TRANSACTION COMMIT",
            4 => "ON TRANSACTION ROLLBACK",
            _ => "ON CONNECT",
        }
        .into();
    }
    if t & DDL != 0 {
        return format!("{} ANY DDL STATEMENT", if t & 1 == 0 { "BEFORE" } else { "AFTER" });
    }
    let prefix = if (t + 1) & 1 == 0 { "BEFORE" } else { "AFTER" };
    let events: Vec<&str> = (1..=3)
        .filter_map(|slot| match ((t + 1) >> (slot * 2 - 1)) & 3 {
            1 => Some("INSERT"),
            2 => Some("UPDATE"),
            3 => Some("DELETE"),
            _ => None,
        })
        .collect();
    format!("{prefix} {}", events.join(" OR "))
}

/// PSQL source as stored lacks the `AS` that precedes it in the DDL.
fn with_as(src: &str) -> String {
    let src = src.trim();
    let starts_with_as = src.get(..2).is_some_and(|w| w.eq_ignore_ascii_case("AS"))
        && src[2..].starts_with(|c: char| c.is_whitespace());
    if starts_with_as { src.to_string() } else { format!("AS\n{src}") }
}

fn params_list(c: &mut Conn, sql: &str, name: &str) -> Result<Vec<(String, i64, String)>> {
    Ok(c
        .rows(sql, vec![SqlType::Text(name.into())])?
        .iter()
        .map(|r| {
            let g = |i: usize| r.get(i).and_then(int);
            (
                r.first().and_then(text).unwrap_or_default(),
                g(1).unwrap_or(0),
                field_type(g(2), g(3), g(4), g(5), g(6), g(7)),
            )
        })
        .collect())
}

fn one_row(c: &mut Conn, sql: &str, name: &str) -> Result<Option<Vec<Column>>> {
    Ok(c.rows(sql, vec![SqlType::Text(name.into())])?.into_iter().next())
}

fn definition(c: &mut Conn, kind: &str, name: &str) -> Result<Option<String>> {
    let qn = q(name);
    match kind {
        "view" => {
            let Some(r) = one_row(c, "SELECT RDB$VIEW_SOURCE FROM RDB$RELATIONS WHERE RDB$RELATION_NAME = ?", name)?
            else {
                return Ok(None);
            };
            Ok(r.first().and_then(text).map(|src| format!("CREATE OR ALTER VIEW {qn} AS\n{};", src.trim())))
        }
        "procedure" => {
            let Some(r) =
                one_row(c, "SELECT RDB$PROCEDURE_SOURCE FROM RDB$PROCEDURES WHERE RDB$PROCEDURE_NAME = ?", name)?
            else {
                return Ok(None);
            };
            let Some(src) = r.first().and_then(text) else { return Ok(None) };
            let params = params_list(c, PROCEDURE_PARAMS, name)?;
            let list = |dir: i64| -> Vec<String> {
                params.iter().filter(|p| p.1 == dir).map(|p| format!("    {} {}", q(&p.0), p.2)).collect()
            };
            let (ins, outs) = (list(0), list(1));
            let mut s = format!("CREATE OR ALTER PROCEDURE {qn}");
            if !ins.is_empty() {
                s.push_str(&format!(" (\n{}\n)", ins.join(",\n")));
            }
            if !outs.is_empty() {
                s.push_str(&format!("\nRETURNS (\n{}\n)", outs.join(",\n")));
            }
            Ok(Some(format!("{s}\n{};", with_as(&src))))
        }
        "function" => {
            let Some(r) =
                one_row(c, "SELECT RDB$FUNCTION_SOURCE FROM RDB$FUNCTIONS WHERE RDB$FUNCTION_NAME = ?", name)?
            else {
                return Ok(None);
            };
            // Legacy UDFs and external functions have no PSQL source.
            let Some(src) = r.first().and_then(text) else { return Ok(None) };
            let args = params_list(c, FUNCTION_ARGS, name)?;
            let ins: Vec<String> =
                args.iter().filter(|a| a.1 > 0).map(|a| format!("    {} {}", q(&a.0), a.2)).collect();
            let ret = args.iter().find(|a| a.1 == 0).map(|a| a.2.clone()).unwrap_or_default();
            let mut s = format!("CREATE OR ALTER FUNCTION {qn}");
            if !ins.is_empty() {
                s.push_str(&format!(" (\n{}\n)", ins.join(",\n")));
            }
            Ok(Some(format!("{s}\nRETURNS {ret}\n{};", with_as(&src))))
        }
        PACKAGE => {
            let Some(r) = one_row(
                c,
                "SELECT RDB$PACKAGE_HEADER_SOURCE, RDB$PACKAGE_BODY_SOURCE FROM RDB$PACKAGES WHERE RDB$PACKAGE_NAME = ?",
                name,
            )?
            else {
                return Ok(None);
            };
            let header = r.first().and_then(text).unwrap_or_default();
            let mut s = format!("CREATE OR ALTER PACKAGE {qn}\nAS\n{};", header.trim());
            if let Some(body) = r.get(1).and_then(text) {
                s.push_str(&format!("\n\nRECREATE PACKAGE BODY {qn}\nAS\n{};", body.trim()));
            }
            Ok(Some(s))
        }
        "trigger" => {
            let Some(r) = one_row(
                c,
                "SELECT RDB$TRIGGER_SOURCE, TRIM(RDB$RELATION_NAME), RDB$TRIGGER_TYPE, RDB$TRIGGER_SEQUENCE,
                        RDB$TRIGGER_INACTIVE
                   FROM RDB$TRIGGERS WHERE RDB$TRIGGER_NAME = ?",
                name,
            )?
            else {
                return Ok(None);
            };
            let Some(src) = r.first().and_then(text) else { return Ok(None) };
            let table = r.get(1).and_then(text).filter(|t| !t.is_empty());
            let event = trigger_event(r.get(2).and_then(int).unwrap_or(1));
            let position = r.get(3).and_then(int).unwrap_or(0);
            let active = if r.get(4).and_then(int).unwrap_or(0) == 1 { "INACTIVE" } else { "ACTIVE" };
            let target = table.map(|t| format!(" FOR {}", q(&t))).unwrap_or_default();
            Ok(Some(format!(
                "CREATE OR ALTER TRIGGER {qn}{target}\n{active} {event} POSITION {position}\n{};",
                with_as(&src)
            )))
        }
        "sequence" => {
            let Some(r) = one_row(
                c,
                "SELECT RDB$INITIAL_VALUE, RDB$GENERATOR_INCREMENT FROM RDB$GENERATORS WHERE RDB$GENERATOR_NAME = ?",
                name,
            )?
            else {
                return Ok(None);
            };
            let start = r.first().and_then(int).unwrap_or(0);
            let step = r.get(1).and_then(int).unwrap_or(1);
            // Not the current value: "Comparar esquemas" compares this text,
            // and a sequence only used more on one side isn't a difference.
            Ok(Some(format!("CREATE SEQUENCE {qn} START WITH {start} INCREMENT BY {step};")))
        }
        "type" => Ok(one_row(c, DOMAIN, name)?.map(|r| domain_sql(name, &r))),
        // Firebird has no DDL extractor; the UI builds a CREATE TABLE from
        // the columns.
        _ => Ok(None),
    }
}

#[cfg(test)]
mod tests {
    #[test]
    fn errors_without_sqlerr_get_isqls_codes() {
        let pk = FbError::Sql { code: -1, msg: "violation of PRIMARY or UNIQUE KEY constraint \"PK_T\" on table \"T\"".into() };
        let Error::Statement(se) = super::stmt_err(pk, "insert into t values (1)", 0) else { panic!("statement error") };
        assert_eq!((se.code.as_deref(), se.sqlstate.as_deref()), (Some("-803"), Some("23000")));
        let dyn_sql = FbError::Sql { code: -206, msg: "Dynamic SQL Error\nSQL error code = -206\nColumn unknown\nX".into() };
        let Error::Statement(se) = super::stmt_err(dyn_sql, "select x from t", 0) else { panic!("statement error") };
        assert_eq!((se.code.as_deref(), se.sqlstate), (Some("-206"), None));
        let cancel = FbError::Sql { code: -1, msg: "operation was cancelled".into() };
        assert!(matches!(super::stmt_err(cancel, "select 1", 0), Error::Cancelled));
    }

    use super::*;

    #[test]
    fn update_script_by_key() {
        let t = ObjectRef { kind: "table".into(), schema: Some("APP".into()), name: "CLIENTES".into() };
        let c = dbine_driver::RowChange {
            key: vec![("ID".into(), serde_json::json!(7)), ("REGION".into(), Value::Null)],
            set: vec![("NOMBRE".into(), serde_json::json!("O'Brien")), ("BAJA".into(), Value::Null)], ..Default::default()
        };
        assert_eq!(
            drivers()[0].update_script(&t, &[c]).unwrap(),
            "UPDATE \"CLIENTES\" SET \"NOMBRE\" = \'O\'\'Brien\', \"BAJA\" = NULL WHERE \"ID\" = 7 AND \"REGION\" IS NULL;"
        );
    }


    #[test]
    fn delete_script_by_composite_key() {
        let t = ObjectRef { kind: "table".into(), schema: Some("APP".into()), name: "CLIENTES".into() };
        let keys = vec![vec![("NOMBRE".into(), serde_json::json!("O'Brien")), ("REGION".into(), Value::Null)], vec![]];
        assert_eq!(
            drivers()[0].delete_script(&t, &keys).unwrap(),
            "DELETE FROM \"CLIENTES\" WHERE \"NOMBRE\" = 'O''Brien' AND \"REGION\" IS NULL;"
        );
    }

    #[test]
    fn domains() {
        let col = |v: SqlType| Column { value: v, raw_type: 0, name: String::new() };
        let (i, t, n) = (|x: i64| col(SqlType::Integer(x)), |x: &str| col(SqlType::Text(x.into())), || col(SqlType::Null));
        let email = [i(37), i(0), i(480), i(120), n(), i(0), t("DEFAULT 'x@y'"), i(1), t("CHECK (VALUE LIKE '%@%')"), t("UTF8"), t("UNICODE_CI"), t("UTF8")];
        assert_eq!(
            domain_sql("D_EMAIL", &email),
            "CREATE DOMAIN \"D_EMAIL\" AS VARCHAR(120) CHARACTER SET UTF8 DEFAULT 'x@y' NOT NULL CHECK (VALUE LIKE '%@%') COLLATE UNICODE_CI;"
        );
        let monto = [i(16), i(1), i(8), n(), i(18), i(-2), n(), n(), t("CHECK (VALUE >= 0)"), n(), n(), n()];
        assert_eq!(domain_sql("D_MONTO", &monto), "CREATE DOMAIN \"D_MONTO\" AS NUMERIC(18,2) CHECK (VALUE >= 0);");
    }

    #[test]
    fn field_types() {
        assert_eq!(field_type(Some(8), Some(0), Some(4), None, Some(0), Some(0)), "INTEGER");
        assert_eq!(field_type(Some(16), Some(1), Some(8), None, Some(18), Some(-2)), "NUMERIC(18,2)");
        assert_eq!(field_type(Some(8), Some(2), Some(4), None, Some(9), Some(-3)), "DECIMAL(9,3)");
        assert_eq!(field_type(Some(37), Some(0), Some(200), Some(50), None, None), "VARCHAR(50)");
        assert_eq!(field_type(Some(261), Some(1), Some(8), None, None, None), "BLOB SUB_TYPE TEXT");
        assert_eq!(field_type(Some(29), None, None, None, None, None), "TIMESTAMP WITH TIME ZONE");
    }

    #[test]
    fn trigger_events() {
        assert_eq!(trigger_event(1), "BEFORE INSERT");
        assert_eq!(trigger_event(2), "AFTER INSERT");
        assert_eq!(trigger_event(3), "BEFORE UPDATE");
        assert_eq!(trigger_event(6), "AFTER DELETE");
        // BEFORE INSERT OR UPDATE OR DELETE = 1 + (1 << 1) + (2 << 3) + (3 << 5) - 1… per isql.
        assert_eq!(trigger_event(113), "BEFORE INSERT OR UPDATE OR DELETE");
        assert_eq!(trigger_event(0x2000), "ON CONNECT");
    }

    #[test]
    fn lenient_decoding_turns_bytes_into_hex() {
        let cs = lenient_charset("UTF8").unwrap();
        assert_eq!(cs.decode(&b"hola"[..]).unwrap(), "hola");
        assert_eq!(cs.decode(&[0xBE, 0xEF][..]).unwrap(), "0xBEEF");
    }

    #[test]
    fn values() {
        assert_eq!(cell(SqlType::Integer(i64::MAX)), serde_json::json!("9223372036854775807"));
        let t = chrono_base_date().and_hms_opt(13, 45, 0).unwrap();
        assert_eq!(cell(SqlType::Timestamp(t)), serde_json::json!("13:45:00"));
    }

    #[test]
    fn browse_uses_first() {
        let o = ObjectRef { kind: "table".into(), schema: None, name: "my t".into() };
        let s = format!("SELECT FIRST {} *\nFROM {}", 5, q(&o.name));
        assert_eq!(s, "SELECT FIRST 5 *\nFROM \"my t\"");
    }
}
