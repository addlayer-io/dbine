//! Oracle Database through `oracledb`, Oracle's pure-Rust thin driver: it
//! speaks TNS/TTC itself, so no Instant Client (OCI) library is needed at
//! build time or at run time.
//!
//! The client is synchronous: every call runs on a blocking thread. The
//! level below the connection is the schema (Oracle users that own
//! objects), so `connect(database)` sets `CURRENT_SCHEMA`.

mod backup;
mod blocking;
mod ddl;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod profiler;
mod script;
mod security;
mod structure;
mod transfer;

use dbine_driver::sql::{select_top, Limit, Quote};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate,
    DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, Message,
    MessageLevel, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn, ScriptDefaults, ScriptDialect,
    ScriptError, ScriptMode, Session, StatementEnd, TableSchema, TxState,
};
use oracledb::{Connection, Cursor, OracleNumber, OracleTimestamp, Row};
use serde_json::Value;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

const DEFAULT_PORT: u16 = 1521;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Characters of a CLOB / LONG shown in a cell.
const TEXT_CAP: usize = 64 * 1024;
const PACKAGE: &str = "package";

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(OracleDriver { info: info(), autonomous: false }),
        Arc::new(OracleDriver { info: autonomous_info(), autonomous: true }),
    ]
}

struct OracleDriver {
    info: DriverInfo,
    /// Oracle Autonomous Database (ADW / ATP / JSON): TLS, with or without
    /// the wallet (mTLS).
    autonomous: bool,
}

fn info() -> DriverInfo {
    let mut host = Field::host();
    host.required = false;
    DriverInfo {
        id: "oracle",
        name: "Oracle",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "oracle",
        default_port: DEFAULT_PORT,
        fields: vec![
            host.help("Se ignora si se completa el descriptor de conexión."),
            Field::port().placeholder("1521"),
            Field::new(
                "connect_by",
                "Conectar por",
                FieldKind::Select(vec![("service_name", "Nombre de servicio"), ("sid", "SID")]),
            )
            .default_value("service_name"),
            Field::new("service", "Servicio / SID", FieldKind::Text).placeholder("FREEPDB1"),
            Field::username().required(),
            Field::password(),
            Field::new(
                "role",
                "Rol",
                FieldKind::Select(vec![("", "Normal"), ("sysdba", "SYSDBA"), ("sysoper", "SYSOPER")]),
            ),
            Field::new("connect_descriptor", "Descriptor o alias TNS", FieldKind::Textarea)
                .placeholder("(DESCRIPTION=(ADDRESS=…)(CONNECT_DATA=…)) o MI_ALIAS")
                .help(
                    "Opcional. Reemplaza servidor, puerto y servicio. Acepta un descriptor completo, \
                     una cadena Easy Connect (host:puerto/servicio) o un alias de tnsnames.ora.",
                ),
            Field::new("tns_admin", "Carpeta de tnsnames.ora", FieldKind::Text)
                .placeholder("(la de TNS_ADMIN)")
                .help("Dónde buscar tnsnames.ora para resolver alias. No hace falta instalar Instant Client.")
                .advanced(),
            Field::encrypt().help("Usa TCPS (el puerto suele ser 2484)."),
            Field::new("autocommit", "Confirmar automáticamente (autocommit)", FieldKind::Bool)
                .default_value("true")
                .help("Si está apagado, los cambios esperan un COMMIT explícito.")
                .advanced(),
            Field::read_only(),
        ],
        databases_label: "Esquemas",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::materialized_views(),
            ObjectKindInfo::procedures(),
            ObjectKindInfo::functions(),
            ObjectKindInfo::new(PACKAGE, "Paquetes", false, false, true),
            ObjectKindInfo::triggers(),
            ObjectKindInfo::sequences(),
            ObjectKindInfo::synonyms(),
            ObjectKindInfo::types(),
        ],
    }
}

/// Oracle Autonomous Database: same client over TLS. With a wallet (mTLS)
/// the folder holds `tnsnames.ora` and `ewallet.pem`; without one (TLS
/// only, when the instance allows it) the console's connection string is
/// pasted as is.
fn autonomous_info() -> DriverInfo {
    let base = info();
    DriverInfo {
        id: "oracle_adb",
        name: "Oracle Autonomous Database",
        fields: vec![
            Field::new("wallet_dir", "Wallet (carpeta descomprimida)", FieldKind::File)
                .placeholder("/ruta/Wallet_MIBASE")
                .help(
                    "Para mTLS: la carpeta del wallet descomprimido (tnsnames.ora y ewallet.pem); podés elegir \
                     cualquier archivo de adentro. Vacío si la base acepta TLS sin wallet.",
                )
                .ssl(),
            Field::new("wallet_password", "Contraseña del wallet", FieldKind::Password)
                .secret()
                .help("La que se eligió al descargar el wallet (no la del usuario ADMIN).")
                .ssl(),
            Field::new("adb_name", "Nombre de la base", FieldKind::Text)
                .placeholder("mibase")
                .help("Con el wallet: arma el alias de tnsnames.ora (nombre_servicio)."),
            Field::new(
                "adb_service",
                "Servicio",
                FieldKind::Select(vec![
                    ("high", "high (máxima prioridad, paralelo)"),
                    ("medium", "medium"),
                    ("low", "low (más concurrencia)"),
                    ("tp", "tp (transaccional, ATP)"),
                    ("tpurgent", "tpurgent (ATP)"),
                ]),
            )
            .default_value("low"),
            Field::new("connect_descriptor", "Cadena de conexión o alias TNS", FieldKind::Textarea)
                .placeholder("(description=(retry_count=20)(address=(protocol=tcps)(port=1522)(host=…))…) o mibase_high")
                .help("Opcional con el wallet. Sin wallet (TLS), pegá la cadena de conexión TLS de la consola."),
            Field::username().required().default_value("ADMIN"),
            Field::password(),
            Field::new("autocommit", "Confirmar automáticamente (autocommit)", FieldKind::Bool)
                .default_value("true")
                .help("Si está apagado, los cambios esperan un COMMIT explícito.")
                .advanced(),
            Field::read_only(),
        ],
        default_port: 1522,
        ..base
    }
}

/// The wallet folder, also when a file inside it was picked.
fn wallet_dir(cfg: &ConnectionConfig) -> Option<String> {
    let p = std::path::Path::new(cfg.option("wallet_dir")?.trim());
    let dir = if p.is_file() { p.parent()? } else { p };
    Some(dir.to_string_lossy().into_owned())
}

/// Connect string for Autonomous Database: the pasted descriptor / alias,
/// or `<name>_<service>` from the wallet's tnsnames.ora.
fn autonomous_connect_string(cfg: &ConnectionConfig) -> Result<String> {
    if let Some(d) = cfg.option("connect_descriptor") {
        return Ok(d.trim().to_string());
    }
    let name = cfg.option("adb_name").map(str::trim).unwrap_or("");
    if name.is_empty() || wallet_dir(cfg).is_none() {
        return Err(Error::Connect(
            "Falta la cadena de conexión, o el wallet y el nombre de la base para armar el alias.".into(),
        ));
    }
    if !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_') {
        return Err(Error::Connect("El nombre de la base solo lleva letras, números y _.".into()));
    }
    Ok(format!("{}_{}", name.to_ascii_lowercase(), cfg.option("adb_service").unwrap_or("low")))
}

// ---------------------------------------------------------------- errors

fn err(e: oracledb::Error) -> Error {
    Error::Query(e.to_string())
}

fn db_code(e: &oracledb::Error) -> Option<usize> {
    match e.kind() {
        oracledb::ErrorKind::DbError(d) => Some(d.code()),
        _ => None,
    }
}

fn connect_err(e: oracledb::Error) -> Error {
    match db_code(&e) {
        // Invalid credentials, account locked / expired, no SYSDBA grant.
        Some(1017 | 1005 | 1031 | 1045 | 28000 | 28001 | 28009) => Error::AuthFailed(e.to_string()),
        _ => Error::Connect(e.to_string()),
    }
}

fn poisoned() -> Error {
    Error::State("la conexión quedó en un estado inválido".into())
}

fn join_err(e: tokio::task::JoinError) -> Error {
    Error::State(format!("la tarea de Oracle terminó inesperadamente: {e}"))
}

/// A caught panic's message.
fn panic_text(payload: &(dyn std::any::Any + Send)) -> String {
    payload
        .downcast_ref::<&str>()
        .map(|s| s.to_string())
        .or_else(|| payload.downcast_ref::<String>().cloned())
        .unwrap_or_else(|| "error interno".into())
}

// ------------------------------------------------------------ connecting

/// The connect string: the user's descriptor / alias / Easy Connect as is,
/// or a descriptor built from host, port and service name or SID.
fn connect_string(cfg: &ConnectionConfig) -> Result<String> {
    if let Some(d) = cfg.option("connect_descriptor") {
        return Ok(d.trim().to_string());
    }
    let host = cfg.host.trim();
    if host.is_empty() {
        return Err(Error::Connect("Falta el servidor o el descriptor de conexión.".into()));
    }
    let service = cfg.option("service").map(str::trim).unwrap_or("");
    if service.is_empty() {
        return Err(Error::Connect("Falta el nombre de servicio o el SID.".into()));
    }
    if [host, service].iter().any(|v| v.contains(['(', ')', '=', ' '])) {
        return Err(Error::Connect(
            "El servidor y el servicio no pueden llevar espacios, paréntesis ni '='. Para algo más \
             complejo usá el descriptor de conexión."
                .into(),
        ));
    }
    let key = if cfg.option("connect_by") == Some("sid") { "SID" } else { "SERVICE_NAME" };
    let protocol = if cfg.encrypt { "TCPS" } else { "TCP" };
    let port = cfg.port_or(DEFAULT_PORT);
    Ok(format!(
        "(DESCRIPTION=(ADDRESS=(PROTOCOL={protocol})(HOST={host})(PORT={port}))(CONNECT_DATA=({key}={service})))"
    ))
}

fn build_config(cfg: &ConnectionConfig, autonomous: bool) -> Result<oracledb::Config> {
    let connect_string = if autonomous { autonomous_connect_string(cfg)? } else { connect_string(cfg)? };
    let user = cfg.username_or_empty().trim();
    if user.is_empty() {
        return Err(Error::AuthFailed("Falta el usuario.".into()));
    }
    let mut config = oracledb::Config::default();
    if let Some(dir) = cfg.option("tns_admin") {
        config = config.set_config_dir(dir);
    }
    if autonomous {
        if let Some(dir) = wallet_dir(cfg) {
            // tnsnames.ora and ewallet.pem live together in the wallet.
            config = config.set_config_dir(&dir).set_wallet_location(dir);
            if let Some(pw) = cfg.option("wallet_password") {
                config = config.set_wallet_password(pw);
            }
        }
    }
    let mode = match cfg.option("role") {
        Some("sysdba") => oracledb::AUTH_MODE_SYSDBA,
        Some("sysoper") => oracledb::AUTH_MODE_SYSOPER,
        _ => oracledb::AUTH_MODE_DEFAULT,
    };
    config = config
        .set_credentials(user, cfg.password_or_empty())
        .set_auth_mode(mode)
        .set_program("DBine")
        .and_then(|c| c.set_connect_string(&connect_string))
        .map_err(|e| Error::Connect(e.to_string()))?;
    Ok(config)
}

#[async_trait]
impl Driver for OracleDriver {
    /// Identity columns restart past the loaded values.
    fn data_load_wrap(&self, table: &dbine_driver::TableSchema) -> (String, String) {
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let name = match table.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => format!("{}.{}", q(s), q(&table.name)),
            None => q(&table.name),
        };
        let after: Vec<String> = table
            .columns
            .iter()
            .filter(|c| c.auto_increment)
            .map(|c| format!("ALTER TABLE {name} MODIFY ({} GENERATED BY DEFAULT AS IDENTITY (START WITH LIMIT VALUE));", q(&c.name)))
            .collect();
        (String::new(), after.join("\n"))
    }

    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::oracle()
    }

    /// The units SQL*Plus runs, cut by this driver's own splitter (the one
    /// `execute` uses), so the app's statement map, the statement at the
    /// cursor and the UPDATE/DELETE check see what runs: every SQL*Plus
    /// command line (`PROMPT`, `SET SERVEROUTPUT`, `SHOW ERRORS`, `REM`…)
    /// and every `EXEC` is a unit of its own, a PL/SQL unit ends at its `/`
    /// line. Each unit's text is the script's own (`EXEC` as written), which
    /// `execute` reads again as one unit. Commands and `EXEC` are `Block`s:
    /// they run, and their words are never read as DML.
    fn split_script(&self, text: &str) -> Vec<dbine_driver::ScriptStatement> {
        script_units(text)
    }

    /// Statement by statement, as SQL*Plus runs a script (units from
    /// [`Driver::split_script`]).
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// SQL*Plus and SQL Developer go on after a failed statement.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    /// The editor starts in Auto, as the connection's «autocommit» option
    /// does by default (SQL*Plus itself starts with AUTOCOMMIT OFF: Manual
    /// is the tab's toggle). In Auto every DML statement and PL/SQL block
    /// is committed after it runs.
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// ALL_INDEXES plus DBA_INDEX_USAGE / V$SEGSTAT (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Array DML (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Oracle to Oracle (either flavor): the same read and load, keeping
    /// time zone regions and extended JSON (see `transfer`).
    fn supports_native_copy(&self, target: &str) -> bool {
        matches!(target, "oracle" | "oracle_adb")
    }

    async fn copy_native(
        &self,
        source: &mut dyn Session,
        target: &mut dyn Session,
        spec: &dbine_driver::transfer::CopySpec,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::copy_native(source, target, spec, progress).await
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        Some(backup::spec())
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(action)
    }

    /// The "databases" are schemas: created as schema-only accounts
    /// (`CREATE USER … NO AUTHENTICATION`, 18c+), dropped with
    /// `DROP USER … CASCADE`. Both need the CREATE / DROP USER privilege.
    fn capabilities(&self) -> Capabilities {
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            // V$SESSION.BLOCKING_SESSION and ALTER SYSTEM KILL SESSION (also
            // in Autonomous Database, as ADMIN).
            blocking: true,
            kill_session: true,
        }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::create_templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(ddl::table_ddl(table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        ddl::sync_script(changes)
    }

    /// One INSERT per row (no multi-row VALUES before 23ai), booleans as 1 / 0.
    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        Ok(dbine_driver::ddl::insert_script(&ddl::FLAVOR, target.schema(), &target.name, columns, rows, 1))
    }

    /// `UPDATE … WHERE <key>` per changed row, with the INSERTs' literals.
    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        Ok(dbine_driver::ddl::update_script(&ddl::FLAVOR, target.schema(), &target.name, changes))
    }

    /// `DELETE … WHERE <key>` per row key, with the INSERTs' literals.
    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        Ok(dbine_driver::ddl::delete_script(&ddl::FLAVOR, target.schema(), &target.name, keys))
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let config = build_config(cfg, self.autonomous)?;
        let autocommit = cfg.option("autocommit").is_none_or(|v| v == "true");
        let schema = database.filter(|d| !d.is_empty()).map(str::to_string);
        let c2 = config.clone();
        // The thin client has no TCP connect timeout of its own: give up on
        // the blocking thread after a while (it ends when the OS gives up).
        let task = tokio::task::spawn_blocking(move || open(c2, schema.as_deref(), true));
        let (conn, ids, schema) = tokio::time::timeout(CONNECT_TIMEOUT, task)
            .await
            .map_err(|_| Error::Connect(format!("El servidor no respondió en {} s.", CONNECT_TIMEOUT.as_secs())))?
            .map_err(join_err)??;
        Ok(Box::new(OracleSession {
            shared: Arc::new(Shared {
                conn: Mutex::new(conn),
                ids: Mutex::new(ids),
                killed: AtomicBool::new(false),
                serveroutput: AtomicBool::new(true),
            }),
            config,
            schema,
            autocommit,
            last_compiled: None,
            metadata_ready: false,
            last_os: None,
            profiler: None,
        }))
    }
}

/// A logged-in connection on `schema` (the user's own when `None`): the
/// connection, its (SID, SERIAL#) and the current schema. `serveroutput`:
/// DBMS_OUTPUT enabled (SET SERVEROUTPUT ON).
fn open(config: oracledb::Config, schema: Option<&str>, serveroutput: bool) -> Result<(Connection, (usize, usize), String)> {
    let conn = oracledb::connect(config).map_err(connect_err)?;
    let ids = (conn.session_id().map_err(err)?, conn.serial_num().map_err(err)?);
    if let Some(s) = schema {
        conn.execute(&format!("ALTER SESSION SET CURRENT_SCHEMA = {}", quote(s)), &[]).map_err(err)?;
    }
    let schema: String = conn
        .query_row(CURRENT_SCHEMA, &[])
        .and_then(|r| r.get(0))
        .map_err(err)?;
    // Server output becomes messages; unlimited buffer.
    if serveroutput {
        if let Err(e) = conn.execute(SERVEROUTPUT_ON, &[]) {
            tracing::debug!("oracle: DBMS_OUTPUT.ENABLE failed: {e}");
        }
    }
    Ok((conn, ids, schema))
}

// --------------------------------------------------------------- session

/// What the session and its interrupter share.
struct Shared {
    conn: Mutex<Connection>,
    /// (SID, SERIAL#) of `conn`, for the interrupter.
    ids: Mutex<(usize, usize)>,
    /// The interrupter killed the server session: reconnect before the next
    /// call.
    killed: AtomicBool,
    /// SET SERVEROUTPUT: DBMS_OUTPUT lines are fetched after each statement
    /// (on by default, as the editor wants them); kept across a reconnect.
    serveroutput: AtomicBool,
}

struct OracleSession {
    shared: Arc<Shared>,
    /// To reconnect and to open the interrupter's side connection.
    config: oracledb::Config,
    /// The session's current schema (the "database" the UI picked).
    schema: String,
    autocommit: bool,
    /// The last PL/SQL unit or view created or altered: what a bare SHOW
    /// ERRORS lists.
    last_compiled: Option<script::Object>,
    /// DBMS_METADATA transform parameters already set in this session.
    metadata_ready: bool,
    /// V$OSSTAT (BUSY_TIME, IDLE_TIME) of the previous monitor snapshot.
    last_os: Option<(f64, f64)>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

impl OracleSession {
    /// Run `f` with the connection on a blocking thread, first replacing a
    /// connection the interrupter killed.
    async fn run<T, F>(&mut self, f: F) -> Result<T>
    where
        T: Send + 'static,
        F: FnOnce(&Connection) -> Result<T> + Send + 'static,
    {
        let shared = self.shared.clone();
        let config = self.config.clone();
        let schema = self.schema.clone();
        let (value, reconnected) = tokio::task::spawn_blocking(move || {
            let mut conn = shared.conn.lock().map_err(|_| poisoned())?;
            let mut reconnected = false;
            if shared.killed.swap(false, Ordering::SeqCst) {
                let (c, ids, _) = open(config, Some(&schema), shared.serveroutput.load(Ordering::SeqCst))?;
                *conn = c;
                *shared.ids.lock().map_err(|_| poisoned())? = ids;
                reconnected = true;
            }
            // The client panics on some values (a TIMESTAMP WITH TIME ZONE
            // with a region): caught here, the lock isn't poisoned, and the
            // connection (its protocol state unknown) is replaced before the
            // next call.
            match std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| f(&conn))) {
                Ok(r) => r.map(|v| (v, reconnected)),
                Err(p) => {
                    shared.killed.store(true, Ordering::SeqCst);
                    Err(Error::State(format!(
                        "El cliente de Oracle no pudo leer la respuesta ({}). Si el resultado tiene TIMESTAMP WITH TIME ZONE \
                         con región, convertilo con TO_CHAR(…, 'YYYY-MM-DD HH24:MI:SS.FF TZR').",
                        panic_text(&*p)
                    )))
                }
            }
        })
        .await
        .map_err(join_err)??;
        if reconnected {
            self.metadata_ready = false;
        }
        Ok(value)
    }

    fn owner(&self, obj: &ObjectRef) -> String {
        obj.schema().unwrap_or(&self.schema).to_string()
    }
}

/// `"NAME"`, doubling embedded quotes.
fn quote(name: &str) -> String {
    dbine_driver::sql::quote_ident(Quote::Double, name)
}

/// A new schema's name: simple identifiers in upper case (as Oracle folds
/// them unquoted), anything else exactly as written.
fn schema_name(name: &str) -> Result<String> {
    let name = name.trim();
    if name.is_empty() {
        return Err(Error::Query("Falta el nombre del esquema.".into()));
    }
    let simple = name.starts_with(|c: char| c.is_ascii_alphabetic())
        && name.chars().all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '$' | '#'));
    Ok(if simple { name.to_ascii_uppercase() } else { name.to_string() })
}

/// First column of every row, as text.
fn strings(cursor: Cursor) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for row in cursor {
        let row = row.map_err(err)?;
        if let Some(s) = row.get::<Option<String>>(0).map_err(err)? {
            out.push(s);
        }
    }
    Ok(out)
}

fn object_kind(oracle_type: &str) -> Option<&'static str> {
    Some(match oracle_type {
        "TABLE" => "table",
        "VIEW" => "view",
        "MATERIALIZED VIEW" => "materialized_view",
        "PROCEDURE" => "procedure",
        "FUNCTION" => "function",
        "PACKAGE" => PACKAGE,
        "TRIGGER" => "trigger",
        "SEQUENCE" => "sequence",
        "SYNONYM" => "synonym",
        "TYPE" => "type",
        _ => return None,
    })
}

/// DBMS_METADATA object type of a kind.
fn metadata_type(kind: &str) -> Option<&'static str> {
    Some(match kind {
        "table" => "TABLE",
        "view" => "VIEW",
        "materialized_view" => "MATERIALIZED_VIEW",
        "procedure" => "PROCEDURE",
        "function" => "FUNCTION",
        PACKAGE => "PACKAGE",
        "trigger" => "TRIGGER",
        "sequence" => "SEQUENCE",
        _ => return None,
    })
}

/// The explorer's level below the connection: Oracle's schemas are its
/// users, so they're listed here (and `Session::list_schemas` stays
/// `None`). Every user that isn't Oracle-maintained, with objects or
/// still empty (one just made in "Usuarios y permisos" shows at once);
/// the Oracle-maintained ones (SYS, SYSTEM, XDB…) are left out.
const LIST_SCHEMAS: &str = "SELECT u.username FROM all_users u
  WHERE u.oracle_maintained = 'N'
     OR u.username = SYS_CONTEXT('USERENV', 'SESSION_USER')
     OR u.username = SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')
  ORDER BY u.username";

/// Before 12c there's no ORACLE_MAINTAINED column.
const LIST_SCHEMAS_11G: &str = "SELECT u.username FROM all_users u
  WHERE EXISTS (SELECT 1 FROM all_objects o WHERE o.owner = u.username)
    AND u.username NOT IN ('SYS','SYSTEM','OUTLN','DBSNMP','APPQOSSYS','XDB','CTXSYS','MDSYS','ORDSYS',
        'ORDDATA','OLAPSYS','WMSYS','EXFSYS','LBACSYS','DVSYS','GSMADMIN_INTERNAL','AUDSYS','OJVMSYS',
        'SI_INFORMTN_SCHEMA','ORDPLUGINS','APEX_PUBLIC_USER','FLOWS_FILES','ANONYMOUS','DIP','ORACLE_OCM',
        'XS$NULL','MDDATA','SPATIAL_CSW_ADMIN_USR','SPATIAL_WFS_ADMIN_USR','SYSBACKUP','SYSDG','SYSKM')
  ORDER BY u.username";

const LIST_OBJECTS: &str = "SELECT o.object_type, o.object_name, t.table_name
   FROM all_objects o
   LEFT JOIN all_triggers t
     ON o.object_type = 'TRIGGER' AND t.owner = o.owner AND t.trigger_name = o.object_name
  WHERE o.owner = :1
    AND o.object_type IN ('TABLE','VIEW','MATERIALIZED VIEW','PROCEDURE','FUNCTION','PACKAGE','TRIGGER','SEQUENCE',
                          'SYNONYM','TYPE')
    AND o.generated = 'N' AND o.secondary = 'N'
    AND o.object_name NOT LIKE 'BIN$%'
    AND NOT (o.object_type = 'TABLE' AND EXISTS (
          SELECT 1 FROM all_mviews m WHERE m.owner = o.owner AND m.mview_name = o.object_name))
  ORDER BY o.object_name";

const COLUMNS: &str = "SELECT c.column_name, c.data_type, c.char_length, c.data_length, c.data_precision, c.data_scale,
        c.nullable, c.data_default, c.identity_column,
        CASE WHEN EXISTS (
          SELECT 1 FROM all_constraints k
            JOIN all_cons_columns kc ON kc.owner = k.owner AND kc.constraint_name = k.constraint_name
           WHERE k.owner = c.owner AND k.table_name = c.table_name
             AND k.constraint_type = 'P' AND kc.column_name = c.column_name
        ) THEN 1 ELSE 0 END, c.char_used
   FROM all_tab_columns c
  WHERE c.owner = :1 AND c.table_name = :2
  ORDER BY c.column_id";

const SET_METADATA_TRANSFORMS: &str = "BEGIN
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SQLTERMINATOR', TRUE);
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'PRETTY', TRUE);
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'SEGMENT_ATTRIBUTES', FALSE);
  DBMS_METADATA.SET_TRANSFORM_PARAM(DBMS_METADATA.SESSION_TRANSFORM, 'STORAGE', FALSE);
END;";

/// Pulls pending DBMS_OUTPUT lines through a ref cursor (no size limit on
/// an out bind that way).
const GET_OUTPUT: &str = "DECLARE l DBMSOUTPUT_LINESARRAY; n INTEGER := 10000;
BEGIN DBMS_OUTPUT.GET_LINES(l, n); OPEN :1 FOR SELECT column_value FROM TABLE(l) WHERE ROWNUM <= n; END;";

#[async_trait]
impl Session for OracleSession {
    async fn server_version(&mut self) -> Result<String> {
        self.run(|c| {
            let banner = c
                .query_row("SELECT banner FROM v$version WHERE banner LIKE 'Oracle%'", &[])
                .and_then(|r| r.get::<String>(0));
            Ok(match banner {
                Ok(b) => b,
                Err(_) => format!("Oracle Database {}", c.version().map_err(err)?),
            })
        })
        .await
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        self.run(|c| match c.query(LIST_SCHEMAS, &[]) {
            Ok(cur) => strings(cur),
            // ORA-00904: ORACLE_MAINTAINED doesn't exist (11g).
            Err(e) if db_code(&e) == Some(904) => strings(c.query(LIST_SCHEMAS_11G, &[]).map_err(err)?),
            Err(e) => Err(err(e)),
        })
        .await
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let owner = self.schema.clone();
        self.run(move |c| {
            let mut out = Vec::new();
            for row in c.query(LIST_OBJECTS, &[&owner]).map_err(err)? {
                let row = row.map_err(err)?;
                let ty: String = row.get(0).map_err(err)?;
                let Some(kind) = object_kind(&ty) else { continue };
                out.push(DbObject {
                    kind: kind.to_string(),
                    schema: None,
                    name: row.get(1).map_err(err)?,
                    parent: row.get(2).map_err(err)?,
                });
            }
            // Public synonyms of this schema's objects (owned by PUBLIC).
            match c.query(structure::PUBLIC_SYNONYMS, &[&owner]) {
                Ok(rows) => {
                    for name in strings(rows)? {
                        out.push(DbObject { kind: kinds::SYNONYM.into(), schema: Some("PUBLIC".into()), name, parent: None });
                    }
                }
                Err(e) => tracing::debug!("oracle: public synonyms not listed: {e}"),
            }
            Ok(out)
        })
        .await
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let owner = self.owner(obj);
        let name = obj.name.clone();
        self.run(move |c| {
            let mut out = Vec::new();
            for row in c.query(COLUMNS, &[&owner, &name]).map_err(err)? {
                let row = row.map_err(err)?;
                let ty: String = row.get(1).map_err(err)?;
                let len: Option<i64> = row.get(2).map_err(err)?;
                let data_len: Option<i64> = row.get(3).map_err(err)?;
                let precision: Option<i64> = row.get(4).map_err(err)?;
                let scale: Option<i64> = row.get(5).map_err(err)?;
                let nullable: Option<String> = row.get(6).map_err(err)?;
                let default: Option<String> = row.get(7).map_err(err)?;
                let identity: Option<String> = row.get(8).map_err(err)?;
                let pk: i64 = row.get(9).map_err(err)?;
                let char_used: Option<String> = row.get(10).map_err(err)?;
                out.push(ColumnInfo {
                    name: row.get(0).map_err(err)?,
                    data_type: char_semantics(format_type(&ty, len, data_len, precision, scale), &ty, char_used.as_deref()),
                    nullable: nullable.as_deref() != Some("N"),
                    primary_key: pk == 1,
                    auto_increment: identity.as_deref() == Some("YES"),
                    default_value: default.map(|d| d.trim().to_string()).filter(|d| !d.is_empty()),
                });
            }
            Ok(out)
        })
        .await
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let owner = self.owner(obj);
        let name = obj.name.clone();
        // Built from the dictionary without the owner, so two schemas compare
        // equal and the DDL runs on either.
        match obj.kind.as_str() {
            kinds::SEQUENCE => return self.run(move |c| structure::sequence(c, &owner, &name)).await,
            kinds::SYNONYM => {
                let schema = self.schema.clone();
                return self.run(move |c| structure::synonym(c, &owner, &name, &schema)).await;
            }
            kinds::TYPE => return self.run(move |c| definition_fallback(c, kinds::TYPE, &owner, &name)).await,
            _ => {}
        }
        let Some(meta) = metadata_type(&obj.kind) else { return Ok(None) };
        let kind = obj.kind.clone();
        let prepare = !self.metadata_ready;
        self.metadata_ready = true;
        self.run(move |c| {
            if prepare {
                if let Err(e) = c.execute(SET_METADATA_TRANSFORMS, &[]) {
                    tracing::debug!("oracle: DBMS_METADATA transforms: {e}");
                }
            }
            match c
                .query_row("SELECT DBMS_METADATA.GET_DDL(:1, :2, :3) FROM dual", &[&meta, &name, &owner])
                .and_then(|r| r.get::<Option<String>>(0))
            {
                Ok(Some(ddl)) if kind == kinds::TABLE => Ok(Some(with_indexes(c, ddl.trim(), &owner, &name))),
                Ok(Some(ddl)) => Ok(Some(ddl.trim().to_string())),
                // No privilege on DBMS_METADATA for someone else's object:
                // rebuild what the dictionary views give.
                _ => definition_fallback(c, &kind, &owner, &name),
            }
        })
        .await
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let owner = self.schema.clone();
        self.run(move |c| ddl::load_schema(c, &owner)).await
    }

    /// A schema-only account with quota on the default tablespace. Simple
    /// names fold to upper case, as Oracle does unquoted.
    async fn create_database(&mut self, name: &str) -> Result<()> {
        let name = schema_name(name)?;
        self.run(move |c| {
            let ts: Option<String> = c
                .query_row(
                    "SELECT property_value FROM database_properties WHERE property_name = 'DEFAULT_PERMANENT_TABLESPACE'",
                    &[],
                )
                .and_then(|r| r.get(0))
                .unwrap_or(None);
            let quota = ts.map(|t| format!(" QUOTA UNLIMITED ON {}", quote(&t))).unwrap_or_default();
            c.execute(&format!("CREATE USER {} NO AUTHENTICATION{quota}", quote(&name)), &[]).map_err(err)?;
            Ok(())
        })
        .await
    }

    /// `DROP USER … CASCADE`: the schema and everything in it.
    async fn drop_database(&mut self, name: &str) -> Result<()> {
        let name = name.trim().to_string();
        if name.is_empty() {
            return Err(Error::Query("Falta el nombre del esquema.".into()));
        }
        let current = self.schema.clone();
        self.run(move |c| {
            let user: String = c
                .query_row("SELECT SYS_CONTEXT('USERENV', 'SESSION_USER') FROM dual", &[])
                .and_then(|r| r.get(0))
                .map_err(err)?;
            if name == current || name == user {
                return Err(Error::Query(format!(
                    "No se puede borrar el esquema {name}: es el de la sesión actual. Conectate a otro esquema primero."
                )));
            }
            c.execute(&format!("DROP USER {} CASCADE", quote(&name)), &[]).map_err(err)?;
            Ok(())
        })
        .await
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::FetchFirst, Some(obj.schema().unwrap_or(&self.schema)), &obj.name, limit)
    }

    /// SQL*Plus-like: statements end at `;` or a `/` line, PL/SQL units at a
    /// `/` line; PROMPT, SET SERVEROUTPUT and SHOW ERRORS run here; the
    /// DBMS_OUTPUT lines of each statement follow it as messages.
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let statements = script::split(text);
        let switches_schema = switches_schema(&statements);
        let text = text.to_string();
        let autocommit = self.autocommit;
        let last_compiled = self.last_compiled.clone();
        let shared = self.shared.clone();
        let fork = out.fork();
        let (local, result, last_compiled) = self
            .run(move |c| {
                let mut local = fork;
                let mut cx = Ctx { text: &text, autocommit, serveroutput: &shared.serveroutput, killed: &shared.killed, last_compiled };
                let result = run_script(c, &statements, max_rows, &mut cx, &mut local);
                Ok((local, result, cx.last_compiled))
            })
            .await?;
        self.last_compiled = last_compiled;
        out.merge(local);
        // The statement died because the interrupter killed the session.
        if result.is_err() && self.shared.killed.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        // ALTER SESSION SET CURRENT_SCHEMA, Oracle's USE (also from a block's
        // EXECUTE IMMEDIATE, even one that failed later): the tab follows
        // the schema, and a reconnect goes back to it.
        if switches_schema && !self.shared.killed.load(Ordering::SeqCst) {
            let current: Result<String> = self.run(|c| c.query_row(CURRENT_SCHEMA, &[]).and_then(|r| r.get(0)).map_err(err)).await;
            match current {
                Ok(schema) => {
                    self.schema = schema.clone();
                    out.database = Some(schema);
                }
                Err(e) => tracing::debug!("oracle: reading CURRENT_SCHEMA failed: {e}"),
            }
        }
        result
    }

    /// Plans per statement (SELECT, WITH and DML; the rest get none).
    ///
    /// Estimated: `EXPLAIN PLAN SET STATEMENT_ID = … FOR <stmt>`, read back
    /// from `PLAN_TABLE` (and `DBMS_XPLAN.DISPLAY` for the raw text), then
    /// deleted. Nothing runs.
    ///
    /// Actual: the session switches to `STATISTICS_LEVEL = ALL` for the
    /// run (back to TYPICAL after), each statement runs once as with
    /// `execute`, and its plan comes from the cursor that ran it
    /// (`V$SESSION.PREV_SQL_ID` → `V$SQL_PLAN_STATISTICS_ALL`, raw text
    /// from `DBMS_XPLAN.DISPLAY_CURSOR(…, 'ALLSTATS LAST')`). Nothing runs
    /// twice, so DML gets actual figures too. Without access to those
    /// views (SELECT_CATALOG_ROLE) it falls back to estimated plans.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let statements: Vec<_> = script::split(text).into_iter().filter(|s| s.command.is_none()).collect();
        let autocommit = self.autocommit;
        let fork = out.fork();
        let (local, result) = self
            .run(move |c| {
                let mut local = fork;
                let result = explain_script(c, &statements, analyze, max_rows, autocommit, &mut local);
                Ok((local, result))
            })
            .await?;
        out.merge(local);
        if result.is_err() && self.shared.killed.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        result
    }

    /// `DBMS_TRANSACTION.LOCAL_TRANSACTION_ID`: set while a transaction is
    /// open (Oracle has no failed-transaction state: a failed statement
    /// only rolls itself back).
    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        self.run(|c| {
            let id: Option<String> = c.query_row(TRANSACTION_ID, &[]).and_then(|r| r.get(0)).map_err(err)?;
            Ok(Some(if id.is_some() { TxState::Open } else { TxState::Idle }))
        })
        .await
    }

    /// Back to Auto commits what's pending, as JDBC's setAutoCommit(true)
    /// does (the editor asks Confirmar / Deshacer before switching).
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if on && !self.autocommit {
            self.run(|c| c.commit().map_err(err)).await?;
        }
        self.autocommit = on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.run(|c| c.commit().map_err(err)).await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.run(|c| c.rollback().map_err(err)).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        let mut last = self.last_os;
        let (snap, last) = self
            .run(move |c| {
                let snap = monitor::snapshot(c, &mut last);
                Ok((snap, last))
            })
            .await?;
        self.last_os = last;
        Ok(snap)
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        let database = database.map(str::to_string);
        self.run(move |c| backup::history(c, database.as_deref())).await
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        self.run(security::principals).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        let principal = principal.to_string();
        self.run(move |c| security::grants(c, &principal)).await
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        self.run(blocking::blocking).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        let id = id.to_string();
        self.run(move |c| blocking::kill(c, &id)).await
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, columns, source, progress).await
    }

    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    async fn profiler_start(&mut self, opts: &dbine_driver::ProfilerOptions) -> Result<dbine_driver::ProfilerStarted> {
        let (state, started) = profiler::start(self, opts).await?;
        self.profiler = Some(state);
        Ok(started)
    }

    async fn profiler_poll(&mut self) -> Result<Vec<dbine_driver::ProfiledStatement>> {
        let mut state = self.profiler.take().ok_or_else(|| Error::State("el profiler no está iniciado".into()))?;
        let r = profiler::poll(self, &mut state).await;
        self.profiler = Some(state);
        r
    }

    async fn profiler_stop(&mut self) -> Result<()> {
        self.profiler = None;
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        // The thin client has no break call, and it hangs when the server
        // interrupts a call (ALTER SYSTEM CANCEL SQL). Killing the session
        // from a second connection does stop it; the next call reconnects.
        // Needs the ALTER SYSTEM privilege; without it this only logs.
        let config = self.config.clone();
        let shared = self.shared.clone();
        Some(Arc::new(move || {
            let config = config.clone();
            let shared = shared.clone();
            std::thread::spawn(move || {
                let Ok((sid, serial)) = shared.ids.lock().map(|ids| *ids) else { return };
                // Set first: the killed statement may return before KILL does.
                shared.killed.store(true, Ordering::SeqCst);
                let killed = oracledb::connect(config).and_then(|c| {
                    c.execute(&format!("ALTER SYSTEM KILL SESSION '{sid}, {serial}' IMMEDIATE"), &[])
                });
                match killed {
                    // ORA-00031: marked for kill, it goes away shortly.
                    Err(e) if db_code(&e) != Some(31) => {
                        shared.killed.store(false, Ordering::SeqCst);
                        tracing::warn!("oracle: no se pudo cancelar la sentencia: {e}");
                    }
                    _ => {}
                }
            });
        }))
    }

    /// The dictionary plus DBA_INDEX_USAGE (see `index_usage`).
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        let owner = self.owner(table);
        let name = table.name.clone();
        self.run(move |c| index_usage::report(c, &owner, &name).map(Some)).await
    }

    /// One query: SESSION_PRIVS and SESSION_ROLES (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        let database = database.map(str::to_string);
        self.run(move |c| permissions::check(c, database.as_deref())).await
    }
}

/// A table's indexes GET_DDL('TABLE') leaves out: every one that doesn't
/// back its primary key or a unique constraint (those are in the table's
/// DDL), including domain indexes such as Oracle Text's. LOB, IOT and
/// cluster indexes come with the table.
const TABLE_INDEXES: &str = "SELECT i.owner, i.index_name FROM all_indexes i
  WHERE i.table_owner = :1 AND i.table_name = :2
    AND i.generated = 'N'
    AND i.index_type NOT IN ('LOB', 'IOT - TOP', 'CLUSTER')
    AND NOT EXISTS (
          SELECT 1 FROM all_constraints k
           WHERE k.owner = i.table_owner AND k.table_name = i.table_name
             AND k.index_owner = i.owner AND k.index_name = i.index_name
             AND k.constraint_type IN ('P', 'U'))
  ORDER BY i.index_name";

/// The table's DDL followed by its standalone indexes' (see
/// [`TABLE_INDEXES`]). An index DBMS_METADATA can't describe is left out.
fn with_indexes(c: &Connection, table_ddl: &str, owner: &str, name: &str) -> String {
    let mut out = table_ddl.to_string();
    let indexes: Vec<(String, String)> = match c.query(TABLE_INDEXES, &[&owner, &name]) {
        Ok(rows) => rows
            .filter_map(|r| r.ok())
            .filter_map(|r| Some((r.get::<String>(0).ok()?, r.get::<String>(1).ok()?)))
            .collect(),
        Err(e) => {
            tracing::debug!("oracle: indexes of {owner}.{name}: {e}");
            return out;
        }
    };
    for (ix_owner, ix) in indexes {
        match c
            .query_row("SELECT DBMS_METADATA.GET_DDL('INDEX', :1, :2) FROM dual", &[&ix, &ix_owner])
            .and_then(|r| r.get::<Option<String>>(0))
        {
            Ok(Some(d)) if !d.trim().is_empty() => {
                out.push_str("\n\n");
                out.push_str(d.trim());
            }
            Ok(_) => {}
            Err(e) => tracing::debug!("oracle: DDL of index {ix_owner}.{ix}: {e}"),
        }
    }
    out
}

fn definition_fallback(c: &Connection, kind: &str, owner: &str, name: &str) -> Result<Option<String>> {
    let qualified = format!("{}.{}", quote(owner), quote(name));
    match kind {
        "view" => {
            let text = c
                .query_row("SELECT text FROM all_views WHERE owner = :1 AND view_name = :2", &[&owner, &name])
                .and_then(|r| r.get::<Option<String>>(0))
                .map_err(err)?;
            Ok(text.map(|t| format!("CREATE OR REPLACE VIEW {qualified} AS\n{}", t.trim())))
        }
        "materialized_view" => {
            let text = c
                .query_row("SELECT query FROM all_mviews WHERE owner = :1 AND mview_name = :2", &[&owner, &name])
                .and_then(|r| r.get::<Option<String>>(0))
                .map_err(err)?;
            Ok(text.map(|t| format!("CREATE MATERIALIZED VIEW {qualified} AS\n{}", t.trim())))
        }
        "procedure" | "function" | "trigger" | "type" | PACKAGE => {
            let body = match kind {
                PACKAGE => "PACKAGE BODY",
                "type" => "TYPE BODY",
                _ => "",
            };
            let ty = kind.to_uppercase();
            let mut src = String::new();
            let rows = c
                .query(
                    "SELECT line, text FROM all_source WHERE owner = :1 AND name = :2 AND type IN (:3, :4)
                      ORDER BY CASE WHEN type LIKE '% BODY' THEN 2 ELSE 1 END, line",
                    &[&owner, &name, &ty, &body],
                )
                .map_err(err)?;
            for row in rows {
                let row = row.map_err(err)?;
                let line: i64 = row.get(0).map_err(err)?;
                if line == 1 {
                    if !src.is_empty() {
                        src.push_str("\n/\n\n");
                    }
                    src.push_str("CREATE OR REPLACE ");
                }
                src.push_str(&row.get::<Option<String>>(1).map_err(err)?.unwrap_or_default());
            }
            Ok((!src.is_empty()).then(|| format!("{}\n/", src.trim_end())))
        }
        _ => Ok(None),
    }
}

// ------------------------------------------------------------- execution

/// What `execute` keeps between statements.
struct Ctx<'a> {
    /// The text `execute` got: error positions and lines are relative to it.
    text: &'a str,
    autocommit: bool,
    serveroutput: &'a AtomicBool,
    /// The interrupter killed the session: the script stops there.
    killed: &'a AtomicBool,
    last_compiled: Option<script::Object>,
}

const SERVEROUTPUT_ON: &str = "BEGIN DBMS_OUTPUT.ENABLE(NULL); END;";
const SERVEROUTPUT_OFF: &str = "BEGIN DBMS_OUTPUT.DISABLE; END;";
const CURRENT_SCHEMA: &str = "SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual";
const TRANSACTION_ID: &str = "SELECT DBMS_TRANSACTION.LOCAL_TRANSACTION_ID FROM dual";

/// A statement of the script may switch the current schema: an `ALTER
/// SESSION` or a PL/SQL unit that names `CURRENT_SCHEMA`.
fn switches_schema(statements: &[script::Statement]) -> bool {
    statements.iter().any(|s| {
        s.command.is_none()
            && (s.plsql || script::first_word(&s.text) == "ALTER")
            && s.text.to_ascii_uppercase().contains("CURRENT_SCHEMA")
    })
}

/// The units of `statements` in order. When the app hands over the whole
/// script (`Whole`: no running statement), this driver numbers them, stamps
/// their results, reports each one live to `out.progress_sink` and, with
/// `out.continue_on_error == Some(true)`, records a failure and goes on, as
/// SQL*Plus does; a fatal error or a cancel still ends the script. Every
/// other caller stops at the first failure, as before.
fn run_script(c: &Connection, statements: &[script::Statement], max_rows: usize, cx: &mut Ctx, out: &mut QueryOutcome) -> Result<()> {
    let own = out.current_statement.is_none();
    for (index, s) in statements.iter().enumerate() {
        let offset = s.start.min(cx.text.len());
        let line = line_of(cx.text, offset);
        if own {
            out.current_statement = Some(index);
        }
        out.adopt_plain_messages();
        let (r0, l0, e0) = (out.results.len(), out.log.len(), out.errors.len());
        let started = std::time::Instant::now();
        let r = run_unit(c, s, max_rows, cx, out);
        // Its DBMS_OUTPUT lines, failed or not (what a block printed before
        // raising), after every statement as SQL*Plus does: DDL and DML
        // print too through their triggers.
        if s.command.is_none() && cx.serveroutput.load(Ordering::SeqCst) && !cx.killed.load(Ordering::SeqCst) {
            drain_output(c, out);
        }
        if !own {
            r?;
            continue;
        }
        let elapsed_ms = started.elapsed().as_millis() as u64;
        for res in &mut out.results[r0..] {
            res.statement = Some(index);
            res.offset = Some(offset);
            res.line = Some(line);
            res.elapsed_ms.get_or_insert(elapsed_ms);
        }
        // Killed by the interrupter: whatever the statement says, it's a cancel.
        if r.is_err() && cx.killed.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let failed = r.err().map(|e| match e {
            Error::Statement(_) | Error::Cancelled | Error::Connect(_) | Error::AuthFailed(_) | Error::Io(_) => e,
            // No place given (SHOW ERRORS reading ALL_ERRORS…): the unit's.
            other => ScriptError::new(other.to_string()).at_offset(offset).at_line(line).into(),
        });
        let editor = out.continue_on_error.is_some() || out.progress_sink.is_some();
        if let Some(e) = &failed {
            if matches!(e, Error::Cancelled) {
                return Err(Error::Cancelled);
            }
            if editor {
                out.push_error(e.to_script_error());
            }
        }
        out.adopt_plain_messages();
        if let Some(sink) = out.progress_sink.clone() {
            (sink.0)(&StatementEnd {
                statement: index,
                offset,
                line,
                elapsed_ms,
                results: out.results[r0..].to_vec(),
                log: out.log[l0..].to_vec(),
                errors: out.errors[e0..].to_vec(),
            });
        }
        match failed {
            Some(e) if out.continue_on_error != Some(true) || e.ends_script() => return Err(e),
            _ => {}
        }
    }
    Ok(())
}

/// One statement or SQL*Plus command of the script.
fn run_unit(c: &Connection, s: &script::Statement, max_rows: usize, cx: &mut Ctx, out: &mut QueryOutcome) -> Result<()> {
    use script::Command;
    match &s.command {
        Some(Command::Prompt(text)) => out.info(text.clone()),
        Some(Command::ServerOutput(on)) => {
            c.execute(if *on { SERVEROUTPUT_ON } else { SERVEROUTPUT_OFF }, &[]).map_err(|e| statement_error(&e, cx.text, s))?;
            cx.serveroutput.store(*on, Ordering::SeqCst);
        }
        Some(Command::ShowErrors(target)) => show_errors(c, target.as_ref().or(cx.last_compiled.as_ref()), out)?,
        Some(Command::Ignored) => {}
        Some(Command::Unsupported) if script::first_word(&s.text) == "WHENEVER" => {
            out.warning(format!("SQL*Plus: «{}» no se aplica; para eso está «Seguir si hay un error».", s.text));
        }
        Some(Command::Unsupported) => out.warning(format!("SQL*Plus: DBine no ejecuta «{}»; se omite.", s.text)),
        None => {
            let before = out.results.len();
            run_statement(c, &s.text, max_rows, cx.autocommit, out).map_err(|e| statement_error(&e, cx.text, s))?;
            let tag = script::tag(&s.text);
            for r in &mut out.results[before..] {
                r.tag.get_or_insert_with(|| tag.clone());
            }
            if let Some(obj) = script::compiled_object(&s.text) {
                report_compile(c, &obj, cx.text, s, out);
                cx.last_compiled = Some(obj);
            }
        }
    }
    Ok(())
}

/// A failed statement with Oracle's code and, when it says, the place:
/// PL/SQL's `line L, column C` (ORA-06550) or `at line L` (ORA-06512 in an
/// anonymous block), else the parse offset. Offsets and lines are relative
/// to `text`, what `execute` got.
fn statement_error(e: &oracledb::Error, text: &str, s: &script::Statement) -> Error {
    match e.kind() {
        oracledb::ErrorKind::DbError(d) => db_statement_error(d.code(), d.offset(), d.message(), text, s),
        kind => {
            let mut se = ScriptError::new(e.to_string());
            if matches!(kind, oracledb::ErrorKind::DeadConnection | oracledb::ErrorKind::NotConnected | oracledb::ErrorKind::UnableToRecover) {
                se = se.fatal();
            }
            let at = s.start.min(text.len());
            se.at_offset(at).at_line(line_of(text, at)).into()
        }
    }
}

/// [`statement_error`] for a server error: its code, parse offset and text.
fn db_statement_error(code: usize, offset: usize, message: &str, text: &str, s: &script::Statement) -> Error {
    let message = message.trim_end();
    let mut se = ScriptError::new(message).with_code(format!("ORA-{code:05}"));
    // The session is gone (killed, connection lost, instance down).
    if matches!(code, 28 | 1012 | 1089 | 1092 | 3113 | 3114 | 3135) {
        se = se.fatal();
    }
    let place = if s.verbatim { error_place(&s.text, message, offset) } else { None };
    let at = (s.start + place.unwrap_or(0)).min(text.len());
    se.at_offset(at).at_line(line_of(text, at)).into()
}

/// The script cut as SQL*Plus runs it (see [`Driver::split_script`]).
fn script_units(text: &str) -> Vec<dbine_driver::ScriptStatement> {
    use dbine_driver::sql::StatementKind;
    script::split(text)
        .into_iter()
        .filter_map(|s| {
            let (start, end) = (s.start.min(text.len()), s.end.min(text.len()));
            let written = text.get(start..end)?.trim_end();
            (!written.is_empty()).then(|| dbine_driver::ScriptStatement {
                text: written.to_string(),
                start,
                end: start + written.len(),
                line: line_of(text, start),
                kind: if s.plsql || s.command.is_some() { StatementKind::Block } else { StatementKind::Sql },
                repeat: 1,
                error: None,
            })
        })
        .collect()
}

/// 1-based line of byte `at` in `text`.
fn line_of(text: &str, at: usize) -> u32 {
    text.as_bytes()[..at.min(text.len())].iter().filter(|&&b| b == b'\n').count() as u32 + 1
}

/// Byte offset in `stmt` of where the server says it failed (see
/// [`statement_error`]). `offset`: the server's parse offset, in bytes of
/// the UTF-8 text (0 when it gives none).
fn error_place(stmt: &str, message: &str, offset: usize) -> Option<usize> {
    if let Some((line, col)) = plsql_line_col(message) {
        return line_col_offset(stmt, line, col);
    }
    if let Some(line) = anonymous_block_line(message) {
        if matches!(script::first_word(stmt).as_str(), "BEGIN" | "DECLARE") {
            return line_col_offset(stmt, line, 1);
        }
    }
    (offset > 0).then(|| (0..=offset.min(stmt.len())).rev().find(|&i| stmt.is_char_boundary(i)).unwrap_or(0))
}

/// `ORA-06550: line 3, column 7:` → (3, 7).
fn plsql_line_col(message: &str) -> Option<(usize, usize)> {
    let rest = &message[message.find("ORA-06550: line ")? + "ORA-06550: line ".len()..];
    let (line, rest) = rest.split_once(", column ")?;
    let col: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((line.parse().ok()?, col.parse().ok()?))
}

/// `ORA-06512: at line 4` (no object name: the anonymous block itself) → 4.
fn anonymous_block_line(message: &str) -> Option<usize> {
    let rest = &message[message.find("ORA-06512: at line ")? + "ORA-06512: at line ".len()..];
    rest.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok()
}

/// Byte offset of 1-based `line`, `col` (characters) in `text`.
fn line_col_offset(text: &str, line: usize, col: usize) -> Option<usize> {
    let start = if line <= 1 { 0 } else { text.match_indices('\n').nth(line - 2)?.0 + 1 };
    let row = &text[start..];
    let row = &row[..row.find('\n').unwrap_or(row.len())];
    Some(start + row.char_indices().nth(col.saturating_sub(1)).map_or(row.len(), |(i, _)| i))
}

/// The compile errors and warnings of `obj`, by ALL_ERRORS: (line,
/// position, text, is a warning).
const OBJECT_ERRORS: &str = "SELECT line, position, text, attribute FROM all_errors
  WHERE owner = NVL(:1, SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA')) AND name = :2 AND type = :3
  ORDER BY sequence";

struct CompileError {
    line: usize,
    position: usize,
    text: String,
    warning: bool,
}

fn object_errors(c: &Connection, obj: &script::Object) -> std::result::Result<Vec<CompileError>, oracledb::Error> {
    // '' binds as NULL: the current schema.
    let owner = obj.owner.clone().unwrap_or_default();
    let mut errors = Vec::new();
    for row in c.query(OBJECT_ERRORS, &[&owner, &obj.name, &obj.kind])? {
        let row = row?;
        errors.push(CompileError {
            line: row.get::<i64>(0)?.max(0) as usize,
            position: row.get::<i64>(1)?.max(0) as usize,
            text: row.get::<Option<String>>(2)?.unwrap_or_default().trim_end().to_string(),
            warning: row.get::<Option<String>>(3)?.as_deref() == Some("WARNING"),
        });
    }
    Ok(errors)
}

fn object_label(obj: &script::Object) -> String {
    match &obj.owner {
        Some(o) => format!("{} {o}.{}", obj.kind, obj.name),
        None => format!("{} {}", obj.kind, obj.name),
    }
}

/// `PLS-00201: …` → `PLS-00201`.
fn message_code(text: &str) -> Option<String> {
    let (code, _) = text.split_once(':')?;
    let ok = code.len() == 9 && code.as_bytes()[3] == b'-' && code[..3].bytes().all(|b| b.is_ascii_uppercase()) && code[4..].bytes().all(|b| b.is_ascii_digit());
    ok.then(|| code.to_string())
}

/// After a CREATE / ALTER of PL/SQL or a view: its compile errors, as
/// SQL Developer shows them. The object exists (invalid), so the statement
/// itself didn't fail: a warning says so, and each error is recorded at its
/// line of the script, with PL/SQL's code. Warnings (PLW-) are messages.
fn report_compile(c: &Connection, obj: &script::Object, text: &str, s: &script::Statement, out: &mut QueryOutcome) {
    let errors = match object_errors(c, obj) {
        Ok(e) => e,
        Err(e) => {
            tracing::debug!("oracle: ALL_ERRORS unreadable: {e}");
            return;
        }
    };
    if errors.iter().any(|e| !e.warning) {
        out.message(Message {
            level: MessageLevel::Warning,
            text: format!("{} se creó con errores de compilación.", object_label(obj)),
            code: Some("ORA-24344".into()),
            line: Some(line_of(text, s.start)),
            ..Default::default()
        });
    }
    for e in errors {
        // Error lines count from the object's kind keyword (its source's
        // first line); a trigger's from its PL/SQL block.
        let at = obj
            .source_line
            .filter(|_| s.verbatim && e.line > 0)
            .and_then(|l| line_col_offset(&s.text, l as usize + e.line - 1, e.position.max(1)))
            .map_or(s.start, |o| s.start + o);
        let code = message_code(&e.text);
        if e.warning {
            out.message(Message { level: MessageLevel::Warning, text: e.text, code, line: Some(line_of(text, at)), ..Default::default() });
        } else {
            let mut se = ScriptError::new(e.text).at_offset(at).at_line(line_of(text, at));
            se.code = code;
            out.push_error(se);
        }
    }
}

/// SHOW ERRORS: `obj`'s compile errors as messages, SQL*Plus style.
fn show_errors(c: &Connection, obj: Option<&script::Object>, out: &mut QueryOutcome) -> Result<()> {
    let Some(obj) = obj else {
        out.info("No se compiló ningún objeto en esta sesión.");
        return Ok(());
    };
    let errors = object_errors(c, obj).map_err(err)?;
    if errors.is_empty() {
        out.info(format!("{}: sin errores.", object_label(obj)));
        return Ok(());
    }
    out.info(format!("Errores de {}:", object_label(obj)));
    for e in errors {
        out.message(Message {
            level: MessageLevel::Warning,
            text: format!("Línea {}, columna {}: {}", e.line, e.position, e.text),
            code: message_code(&e.text),
            ..Default::default()
        });
    }
    Ok(())
}

fn run_statement(
    c: &Connection,
    sql: &str,
    max_rows: usize,
    autocommit: bool,
    out: &mut QueryOutcome,
) -> std::result::Result<(), oracledb::Error> {
    match run_statement_with(c, sql, max_rows, autocommit, out, true) {
        // The thin client keeps a statement whose parse failed in its cache,
        // with a cursor the server never opened: running it again reports
        // ORA-01003 instead of the real error. Taking it out of the cache
        // (building it uncached, not running it) evicts that entry; the
        // retry then parses it afresh.
        Err(e) if db_code(&e) == Some(1003) => {
            drop(c.statement(sql).map(|b| b.exclude_from_cache()).and_then(|b| b.build()));
            run_statement_with(c, sql, max_rows, autocommit, out, false)
        }
        other => other,
    }
}

fn run_statement_with(
    c: &Connection,
    sql: &str,
    max_rows: usize,
    autocommit: bool,
    out: &mut QueryOutcome,
    cached: bool,
) -> std::result::Result<(), oracledb::Error> {
    // The client looks for binds in DDL too, so a trigger's `:new.x` would
    // ask for bind values: hand such DDL to EXECUTE IMMEDIATE as a string.
    let is_ddl = matches!(script::first_word(sql).as_str(), "CREATE" | "ALTER");
    if is_ddl && script::has_bind_like(sql) {
        c.execute(&execute_immediate(sql), &[])?;
        out.results.push(Default::default());
        return Ok(());
    }
    let mut stmt = c.statement(sql).map(|b| if cached { b } else { b.exclude_from_cache() }).and_then(|b| b.build())?;
    if stmt.is_query() {
        let cursor = stmt.query(&[])?;
        let types: Vec<&'static oracledb::DbType> = cursor.columns().iter().map(|m| m.db_type()).collect();
        // WITH LOCAL TIME ZONE values come in the database's time zone.
        let dbtz = if types.iter().any(|t| t.name() == "DB_TYPE_TIMESTAMP_LTZ") { db_time_zone(c) } else { None };
        out.begin_result(
            cursor
                .columns()
                .iter()
                .map(|m| ResultColumn { name: m.name().to_string(), type_name: m.data_type() })
                .collect(),
        );
        for row in cursor {
            let row = row?;
            out.push_row(types.iter().enumerate().map(|(i, t)| cell_in(&row, i, t, dbtz)).collect(), max_rows);
        }
        return Ok(());
    }
    let (dml, plsql) = (stmt.is_dml(), stmt.is_plsql());
    let res = stmt.execute(&[])?;
    if dml {
        out.push_affected(res.rows_affected());
    } else {
        out.results.push(Default::default());
    }
    // Auto: what a DML statement or a block (and CALL) changed is committed
    // at once.
    if autocommit && (dml || plsql) {
        c.commit()?;
    }
    Ok(())
}

// ------------------------------------------------------------------ plans

fn explain_script(
    c: &Connection,
    statements: &[script::Statement],
    analyze: bool,
    max_rows: usize,
    autocommit: bool,
    out: &mut QueryOutcome,
) -> Result<()> {
    let stats = analyze && cursor_stats_available(c);
    if analyze && !stats {
        out.messages.push(
            "Sin acceso a V$SESSION / V$SQL_PLAN_STATISTICS_ALL (SELECT_CATALOG_ROLE): se muestran los planes estimados."
                .into(),
        );
    }
    if stats {
        c.execute("ALTER SESSION SET statistics_level = ALL", &[]).map_err(err)?;
    }
    let result = statements.iter().try_for_each(|s| {
        let explainable = !s.plsql && plan::explainable(&script::first_word(&s.text));
        if !analyze {
            if explainable {
                out.plans.push(estimated_plan(c, &s.text, autocommit)?);
            } else {
                out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&s.text)));
            }
            return Ok(());
        }
        if explainable && stats {
            run_statement(c, &s.text, max_rows, autocommit, out).map_err(err)?;
            match cursor_plan(c, &s.text) {
                Ok(Some(p)) => {
                    if !p.actual {
                        out.messages.push(format!(
                            "Oracle reutilizó un cursor sin estadísticas de ejecución; el plan no trae cifras reales: {}",
                            plan::short(&s.text)
                        ));
                    }
                    out.plans.push(p);
                }
                other => {
                    if let Err(e) = other {
                        tracing::debug!("oracle: cursor plan unavailable: {e}");
                    }
                    out.plans.push(estimated_plan(c, &s.text, autocommit)?);
                }
            }
        } else if explainable {
            out.plans.push(estimated_plan(c, &s.text, autocommit)?);
            run_statement(c, &s.text, max_rows, autocommit, out).map_err(err)?;
        } else {
            let r = run_statement(c, &s.text, max_rows, autocommit, out).map_err(err);
            if s.plsql {
                drain_output(c, out);
            }
            r?;
        }
        Ok(())
    });
    if stats {
        if let Err(e) = c.execute("ALTER SESSION SET statistics_level = TYPICAL", &[]) {
            tracing::debug!("oracle: could not restore statistics_level: {e}");
        }
    }
    if analyze {
        drain_output(c, out);
    }
    result
}

/// Whether this login can read the executed cursor's plan statistics.
fn cursor_stats_available(c: &Connection) -> bool {
    ["SELECT 1 FROM v$session WHERE 1 = 0", "SELECT 1 FROM v$sql_plan_statistics_all WHERE 1 = 0"]
        .iter()
        .all(|q| c.query(q, &[]).is_ok())
}

/// A `STATEMENT_ID` for PLAN_TABLE, unique enough for one session.
fn statement_id() -> String {
    use std::sync::atomic::AtomicU64;
    static SEQ: AtomicU64 = AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos() % 1_000_000_000_000);
    format!("DBINE_{nanos}_{}", SEQ.fetch_add(1, Ordering::Relaxed) % 10_000)
}

/// Every row of a plan query, columns by lower-case name, NULLs left out.
fn plan_rows(c: &Connection, sql: &str, params: &[&dyn oracledb::ToDbValue]) -> Result<Vec<plan::PlanRow>> {
    let cursor = c.query(sql, params).map_err(err)?;
    let names: Vec<String> = cursor.columns().iter().map(|m| m.name().to_ascii_lowercase()).collect();
    let mut out = Vec::new();
    for row in cursor {
        let row = row.map_err(err)?;
        let mut r = Vec::new();
        for (i, n) in names.iter().enumerate() {
            if let Some(v) = row.get::<Option<String>>(i).map_err(err)? {
                r.push((n.clone(), v));
            }
        }
        out.push(r);
    }
    Ok(out)
}

/// DBMS_XPLAN's text, or empty when it can't be had.
fn xplan(c: &Connection, sql: &str, params: &[&dyn oracledb::ToDbValue]) -> String {
    match c.query(sql, params).and_then(|cur| {
        cur.map(|r| r.and_then(|r| r.get::<Option<String>>(0)).map(Option::unwrap_or_default)).collect::<std::result::Result<Vec<_>, _>>()
    }) {
        Ok(lines) => lines.join("\n"),
        Err(e) => {
            tracing::debug!("oracle: DBMS_XPLAN unavailable: {e}");
            String::new()
        }
    }
}

fn estimated_plan(c: &Connection, stmt: &str, autocommit: bool) -> Result<dbine_driver::Plan> {
    let id = statement_id();
    let sql = format!("EXPLAIN PLAN SET STATEMENT_ID = '{id}' FOR {stmt}");
    if script::has_bind_like(stmt) {
        c.execute(&execute_immediate(&sql), &[]).map_err(err)?;
    } else {
        c.execute(&sql, &[]).map_err(err)?;
    }
    let result = plan_rows(
        c,
        &format!("SELECT {} FROM plan_table WHERE statement_id = :1 ORDER BY id", plan::select_list(&[])),
        &[&id],
    )
    .map(|rows| {
        let raw = xplan(c, "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY('PLAN_TABLE', :1, 'TYPICAL'))", &[&id]);
        plan::tree(stmt, &rows, false, &raw)
    });
    if let Err(e) = c.execute("DELETE FROM plan_table WHERE statement_id = :1", &[&id]) {
        tracing::debug!("oracle: could not clean PLAN_TABLE: {e}");
    }
    if autocommit {
        c.commit().map_err(err)?;
    }
    result
}

/// The plan of the statement that just ran, with its row-source figures;
/// `None` when the cursor is already gone from the shared pool.
fn cursor_plan(c: &Connection, stmt: &str) -> Result<Option<dbine_driver::Plan>> {
    let row = c
        .query_row(
            "SELECT prev_sql_id, TO_CHAR(prev_child_number) FROM v$session WHERE sid = SYS_CONTEXT('USERENV', 'SID')",
            &[],
        )
        .map_err(err)?;
    let (Some(sql_id), Some(child)) = (row.get::<Option<String>>(0).map_err(err)?, row.get::<Option<String>>(1).map_err(err)?)
    else {
        return Ok(None);
    };
    let rows = plan_rows(
        c,
        &format!(
            "SELECT {} FROM v$sql_plan_statistics_all WHERE sql_id = :1 AND child_number = TO_NUMBER(:2) ORDER BY id",
            plan::select_list(plan::STAT_COLUMNS)
        ),
        &[&sql_id, &child],
    )?;
    if rows.is_empty() {
        return Ok(None);
    }
    let raw = xplan(
        c,
        "SELECT plan_table_output FROM TABLE(DBMS_XPLAN.DISPLAY_CURSOR(:1, TO_NUMBER(:2), 'ALLSTATS LAST'))",
        &[&sql_id, &child],
    );
    let mut p = plan::tree(stmt, &rows, plan::has_stats(&rows), &raw);
    p.root.props.insert(0, ("sql_id".into(), sql_id));
    Ok(Some(p))
}

/// A PL/SQL block that runs `sql` with EXECUTE IMMEDIATE, the text built
/// from quoted chunks (a CLOB, so there's no 32 KB limit).
fn execute_immediate(sql: &str) -> String {
    let chars: Vec<char> = sql.chars().collect();
    let mut block = String::from("DECLARE s CLOB;\nBEGIN\n  s := '';\n");
    for chunk in chars.chunks(4000) {
        let text: String = chunk.iter().collect();
        block.push_str(&format!("  s := s || '{}';\n", text.replace('\'', "''")));
    }
    // Created with compilation errors (ORA-24344) is raised here: the
    // object exists, as with the statement run directly.
    block.push_str("  EXECUTE IMMEDIATE s;\nEXCEPTION WHEN OTHERS THEN\n  IF SQLCODE != -24344 THEN RAISE; END IF;\nEND;");
    block
}

/// Move pending DBMS_OUTPUT lines to `out.messages`. Best effort: a user
/// without EXECUTE on DBMS_OUTPUT just gets no messages.
fn drain_output(c: &Connection, out: &mut QueryOutcome) {
    for _ in 0..100 {
        let lines = c.execute(GET_OUTPUT, &[&oracledb::DB_TYPE_CURSOR]).and_then(|mut r| {
            let cursor: Cursor = r.out_bind_data().take(0)?;
            let mut lines = Vec::new();
            for row in cursor {
                lines.push(row?.get::<Option<String>>(0)?.unwrap_or_default());
            }
            Ok(lines)
        });
        match lines {
            Ok(lines) => {
                let n = lines.len();
                for line in lines {
                    out.info(line);
                }
                if n < 10000 {
                    return;
                }
            }
            Err(e) => {
                tracing::debug!("oracle: DBMS_OUTPUT.GET_LINES failed: {e}");
                return;
            }
        }
    }
}

// ----------------------------------------------------------------- values

/// A cell as JSON, by the column's Oracle type.
fn cell(row: &Row, i: usize, ty: &oracledb::DbType) -> Value {
    cell_in(row, i, ty, None)
}

/// [`cell`], knowing the database's time zone (offset in minutes), in
/// which WITH LOCAL TIME ZONE values come: shown with it (without it, with
/// no offset).
fn cell_in(row: &Row, i: usize, ty: &oracledb::DbType, dbtz: Option<i32>) -> Value {
    fn val<T>(r: std::result::Result<Option<T>, oracledb::Error>, f: impl FnOnce(T) -> Value) -> Value {
        match r {
            Ok(Some(v)) => f(v),
            Ok(None) => Value::Null,
            Err(e) => format!("<{e}>").into(),
        }
    }
    match ty.name() {
        "DB_TYPE_NUMBER" | "DB_TYPE_BINARY_INTEGER" => val(row.get::<Option<OracleNumber>>(i), |n| number(&n.to_string())),
        "DB_TYPE_BINARY_FLOAT" => val(row.get::<Option<f32>>(i), |v| json_f64(v as f64)),
        "DB_TYPE_BINARY_DOUBLE" => val(row.get::<Option<f64>>(i), json_f64),
        "DB_TYPE_BOOLEAN" => val(row.get::<Option<bool>>(i), Value::Bool),
        "DB_TYPE_DATE" => val(row.get::<Option<OracleTimestamp>>(i), |t| timestamp(&t, false, false).into()),
        "DB_TYPE_TIMESTAMP" => val(row.get::<Option<OracleTimestamp>>(i), |t| timestamp(&t, true, false).into()),
        "DB_TYPE_TIMESTAMP_LTZ" => val(row.get::<Option<OracleTimestamp>>(i), |t| {
            let s = timestamp(&t, true, false);
            dbtz.map_or(s.clone(), |off| format!("{s} {}", offset_text(off))).into()
        }),
        "DB_TYPE_TIMESTAMP_TZ" => val(row.get::<Option<OracleTimestamp>>(i), |t| timestamp(&t, true, true).into()),
        "DB_TYPE_RAW" | "DB_TYPE_LONG_RAW" | "DB_TYPE_BLOB" => val(row.get::<Option<Vec<u8>>>(i), |b| json_bytes(&b)),
        "DB_TYPE_INTERVAL_DS" => val(row.get::<Option<oracledb::OracleIntervalDS>>(i), |v| v.to_string().into()),
        "DB_TYPE_INTERVAL_YM" => val(row.get::<Option<oracledb::OracleIntervalYM>>(i), |v| v.to_string().into()),
        "DB_TYPE_JSON" => val(row.get::<Option<oracledb::JsonValue>>(i), |j| json_text(&j).into()),
        "DB_TYPE_VECTOR" => val(row.get::<Option<oracledb::Vector>>(i), |v| vector(&v).into()),
        "DB_TYPE_CURSOR" => "<cursor>".into(),
        // VARCHAR2, CHAR, NCHAR, NVARCHAR2, LONG, CLOB, NCLOB, ROWID…
        _ => val(row.get::<Option<String>>(i), |s| cap(s).into()),
    }
}

/// NUMBER: a JSON number when it's an integer that fits i64, else the exact
/// decimal as a string.
fn number(s: &str) -> Value {
    match s.parse::<i64>() {
        Ok(i) => json_i64(i),
        Err(_) => s.into(),
    }
}

/// `2024-01-31 13:45:00[.123][ +02:00]` (BC years with a `-`). `tz`: a
/// WITH TIME ZONE value, which the client gives as its UTC clock and its
/// offset: shown as its own local clock with that offset.
fn timestamp(t: &OracleTimestamp, fraction: bool, tz: bool) -> String {
    let off = if tz { t.tz_hour_offset() as i32 * 60 + t.tz_minute_offset() as i32 } else { 0 };
    let ((y, mo, d), h, mi) = shift((t.year() as i32, t.month() as u32, t.day() as u32), t.hour() as u32, t.minute() as u32, off);
    let mut s = format!(
        "{}{:04}-{:02}-{:02} {:02}:{:02}:{:02}",
        if y < 0 { "-" } else { "" },
        y.unsigned_abs(),
        mo,
        d,
        h,
        mi,
        t.second()
    );
    if fraction && t.nanoseconds() != 0 {
        let f = format!("{:09}", t.nanoseconds());
        s.push('.');
        s.push_str(f.trim_end_matches('0'));
    }
    if tz {
        s.push(' ');
        s.push_str(&offset_text(off));
    }
    s
}

/// `±HH:MM` of an offset in minutes.
fn offset_text(off: i32) -> String {
    format!("{}{:02}:{:02}", if off < 0 { '-' } else { '+' }, off.abs() / 60, off.abs() % 60)
}

/// The database's time zone as an offset in minutes (`DBTIMEZONE`); `None`
/// when it's a region other than UTC (its offset changes with the date).
fn db_time_zone(c: &Connection) -> Option<i32> {
    let tz: String = c.query_row("SELECT DBTIMEZONE FROM dual", &[]).and_then(|r| r.get(0)).ok()?;
    parse_offset(&tz)
}

fn parse_offset(tz: &str) -> Option<i32> {
    let tz = tz.trim();
    if ["UTC", "GMT", "Z", "ETC/UTC", "ETC/GMT"].iter().any(|z| tz.eq_ignore_ascii_case(z)) {
        return Some(0);
    }
    let sign = match tz.as_bytes().first()? {
        b'+' => 1,
        b'-' => -1,
        _ => return None,
    };
    let (h, m) = tz[1..].split_once(':')?;
    Some(sign * (h.parse::<i32>().ok()? * 60 + m.parse::<i32>().ok()?))
}

/// A date and time moved by `minutes` (less than a day either way), on
/// Oracle's calendar: Julian until 1582-10-04, Gregorian from 1582-10-15,
/// no year 0 (-1 is 1 BC, and BC leap years are the multiples of 4).
fn shift(date: (i32, u32, u32), h: u32, mi: u32, minutes: i32) -> ((i32, u32, u32), u32, u32) {
    fn month_days(y: i32, m: u32) -> u32 {
        let leap = if y > 1582 { y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) } else { y % 4 == 0 };
        match m {
            4 | 6 | 9 | 11 => 30,
            2 if leap => 29,
            2 => 28,
            _ => 31,
        }
    }
    let total = (h * 60 + mi) as i32 + minutes;
    let mut date = date;
    let days = total.div_euclid(1440);
    for _ in 0..days.max(0) {
        let (y, m, d) = date;
        date = match (y, m, d) {
            (1582, 10, 4) => (1582, 10, 15),
            _ if d < month_days(y, m) => (y, m, d + 1),
            _ if m < 12 => (y, m + 1, 1),
            _ => (if y == -1 { 1 } else { y + 1 }, 1, 1),
        };
    }
    for _ in days.min(0)..0 {
        let (y, m, d) = date;
        date = match (y, m, d) {
            (1582, 10, 15) => (1582, 10, 4),
            _ if d > 1 => (y, m, d - 1),
            _ if m > 1 => (y, m - 1, month_days(y, m - 1)),
            _ => (if y == 1 { -1 } else { y - 1 }, 12, 31),
        };
    }
    let rem = total.rem_euclid(1440);
    (date, (rem / 60) as u32, (rem % 60) as u32)
}

fn cap(mut s: String) -> String {
    if let Some((i, _)) = s.char_indices().nth(TEXT_CAP) {
        s.truncate(i);
        s.push('…');
    }
    s
}

/// A JSON value as text with its numbers exact (through `serde_json`
/// they'd be rounded to `f64`).
fn json_text(j: &oracledb::JsonValue) -> String {
    fn write(j: &oracledb::JsonValue, out: &mut String) {
        use oracledb::JsonValue as J;
        match j {
            J::Number(n) => out.push_str(&n.to_string()),
            J::JsonArray(a) => {
                out.push('[');
                for (i, v) in a.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    write(v, out);
                }
                out.push(']');
            }
            J::JsonObject(o) => {
                out.push('{');
                for (i, (k, v)) in o.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    out.push_str(&Value::String(k.clone()).to_string());
                    out.push(':');
                    write(v, out);
                }
                out.push('}');
            }
            other => out.push_str(&json_value(other).to_string()),
        }
    }
    let mut out = String::new();
    write(j, &mut out);
    out
}

fn json_value(j: &oracledb::JsonValue) -> Value {
    use oracledb::JsonValue as J;
    match j {
        J::Null => Value::Null,
        J::Boolean(b) => Value::Bool(*b),
        J::BinaryDouble(v) => json_f64(*v),
        J::BinaryFloat(v) => json_f64(*v as f64),
        J::Number(n) => {
            let s = n.to_string();
            serde_json::from_str::<serde_json::Number>(&s).map(Value::Number).unwrap_or(Value::String(s))
        }
        J::String(s) => Value::String(s.clone()),
        J::Timestamp(t) => timestamp(t, true, false).into(),
        J::IntervalDS(v) => v.to_string().into(),
        J::IntervalYM(v) => v.to_string().into(),
        J::Raw(b) | J::JsonId(b) => json_bytes(b.as_slice()),
        J::Vector(v) => vector(v).into(),
        J::JsonArray(a) => Value::Array(a.iter().map(json_value).collect()),
        J::JsonObject(o) => Value::Object(o.iter().map(|(k, v)| (k.clone(), json_value(v))).collect()),
    }
}

fn vector(v: &oracledb::Vector) -> String {
    use oracledb::VectorData as D;
    fn list<T: ToString>(xs: &[T]) -> String {
        format!("[{}]", xs.iter().map(T::to_string).collect::<Vec<_>>().join(", "))
    }
    match v {
        oracledb::Vector::Dense(d) => match d {
            D::Float32(x) => list(x),
            D::Float64(x) => list(x),
            D::Int8(x) => list(x),
            D::Binary(x) => list(x),
        },
        oracledb::Vector::Sparse(s) => format!("{s:?}"),
    }
}

/// `VARCHAR2(20 CHAR)` for a column with character length semantics
/// (ALL_TAB_COLUMNS.CHAR_USED = 'C'): without it the length would be read
/// back, and recreated, as bytes (the default), a narrower column.
/// NCHAR / NVARCHAR2 are always in characters and take no qualifier.
fn char_semantics(formatted: String, ty: &str, char_used: Option<&str>) -> String {
    match (ty, char_used) {
        ("VARCHAR2" | "CHAR" | "VARCHAR", Some("C")) if formatted.ends_with(')') => {
            format!("{} CHAR)", &formatted[..formatted.len() - 1])
        }
        _ => formatted,
    }
}

/// `VARCHAR2(20)`, `NUMBER(10,2)`, `NUMBER`, `TIMESTAMP(6)`… from
/// ALL_TAB_COLUMNS (character lengths in characters; `CHAR` semantics are
/// added by [`char_semantics`]).
fn format_type(ty: &str, char_len: Option<i64>, data_len: Option<i64>, precision: Option<i64>, scale: Option<i64>) -> String {
    match ty {
        "RAW" => match data_len {
            Some(n) if n > 0 => format!("RAW({n})"),
            _ => ty.to_string(),
        },
        "VARCHAR2" | "NVARCHAR2" | "CHAR" | "NCHAR" | "VARCHAR" => match char_len {
            Some(n) if n > 0 => format!("{ty}({n})"),
            _ => ty.to_string(),
        },
        "NUMBER" => match (precision, scale) {
            (None, Some(0)) => "INTEGER".to_string(),
            (None, _) => "NUMBER".to_string(),
            (Some(p), Some(0) | None) => format!("NUMBER({p})"),
            (Some(p), Some(s)) => format!("NUMBER({p},{s})"),
        },
        "FLOAT" => match precision {
            Some(p) => format!("FLOAT({p})"),
            None => ty.to_string(),
        },
        _ => ty.to_string(),
    }
}

#[cfg(test)]
mod tests {
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
            "UPDATE \"APP\".\"CLIENTES\" SET \"NOMBRE\" = \'O\'\'Brien\', \"BAJA\" = NULL WHERE \"ID\" = 7 AND \"REGION\" IS NULL;"
        );
    }


    #[test]
    fn delete_script_by_composite_key() {
        let t = ObjectRef { kind: "table".into(), schema: Some("APP".into()), name: "CLIENTES".into() };
        let keys = vec![vec![("NOMBRE".into(), serde_json::json!("O'Brien")), ("REGION".into(), Value::Null)], vec![]];
        assert_eq!(
            drivers()[0].delete_script(&t, &keys).unwrap(),
            "DELETE FROM \"APP\".\"CLIENTES\" WHERE \"NOMBRE\" = 'O''Brien' AND \"REGION\" IS NULL;"
        );
    }

    #[test]
    fn script_contract() {
        for d in drivers() {
            assert_eq!(d.script_dialect(), ScriptDialect::oracle());
            assert_eq!(d.script_defaults(), ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true });
            assert!(d.supports_manual_transactions());
            assert_eq!(d.script_mode(), ScriptMode::PerStatement);
        }
    }

    #[test]
    fn split_script_cuts_sqlplus_lines() {
        use dbine_driver::sql::StatementKind;
        let sql = "SET SERVEROUTPUT ON\nPROMPT Creando tabla;\nCREATE TABLE t (a NUMBER);\n-- c\nBEGIN\n  NULL;\nEND;\n/\nSHOW ERRORS\nSELECT 1 FROM dual\n/\nEXEC p(1, -\n  2);\nSELECT * FROM no_such_table;\nselect q'[a;b]' from dual;\n";
        let d = drivers().into_iter().next().unwrap();
        let units = d.split_script(sql);
        let got: Vec<_> = units.iter().map(|u| (u.text.as_str(), u.line, u.kind)).collect();
        assert_eq!(
            got,
            vec![
                ("SET SERVEROUTPUT ON", 1, StatementKind::Block),
                ("PROMPT Creando tabla;", 2, StatementKind::Block),
                ("CREATE TABLE t (a NUMBER)", 3, StatementKind::Sql),
                ("BEGIN\n  NULL;\nEND;", 5, StatementKind::Block),
                ("SHOW ERRORS", 9, StatementKind::Block),
                ("SELECT 1 FROM dual", 10, StatementKind::Sql),
                ("EXEC p(1, -\n  2);", 12, StatementKind::Block),
                ("SELECT * FROM no_such_table", 14, StatementKind::Sql),
                ("select q'[a;b]' from dual", 15, StatementKind::Sql),
            ]
        );
        for u in &units {
            assert_eq!(&sql[u.start..u.end], u.text);
            // `execute` reads each unit again as one unit, the same one.
            let again = script::split(&u.text);
            assert_eq!(again.len(), 1, "{u:?}");
        }
        // The units are the ones `execute` runs.
        assert_eq!(units.len(), script::split(sql).len());
    }

    #[test]
    fn schema_switches() {
        let yes = |sql: &str| switches_schema(&script::split(sql));
        assert!(yes("alter session set current_schema = hr"));
        assert!(yes("BEGIN EXECUTE IMMEDIATE 'ALTER SESSION SET CURRENT_SCHEMA = HR'; END;"));
        assert!(!yes("SELECT SYS_CONTEXT('USERENV', 'CURRENT_SCHEMA') FROM dual"));
        assert!(!yes("ALTER SESSION SET NLS_DATE_FORMAT = 'YYYY-MM-DD'"));
        assert!(!yes("PROMPT current_schema"));
    }

    #[test]
    fn error_places() {
        // PL/SQL compile error inside an anonymous block: line 2, column 3.
        let block = "begin\n  x := 1;\nend;";
        let msg = "ORA-06550: line 2, column 3:\nPLS-00201: identifier 'X' must be declared\nORA-06550: line 2, column 3:\nPL/SQL: Statement ignored";
        assert_eq!(error_place(block, msg, 0), Some(8));
        // Runtime error raised in the block itself.
        let block = "declare\n  n number;\nbegin\n  n := 1 / 0;\nend;";
        assert_eq!(error_place(block, "ORA-01476: divisor is equal to zero\nORA-06512: at line 4", 0), Some(block.find("  n := 1").unwrap()));
        // …but not in a called object.
        assert_eq!(error_place("begin p; end;", "ORA-01476: x\nORA-06512: at \"HR.P\", line 4\nORA-06512: at line 1", 0), Some(0));
        // Parse offsets count bytes.
        assert_eq!(error_place("select 'é', nope from dual", "ORA-00904: \"NOPE\": invalid identifier", 13), Some(13));
        assert_eq!(error_place("select 'é'", "ORA-01756", 9), Some(8));
        assert_eq!(error_place("select 1 from dual", "ORA-00001: x", 0), None);
        assert_eq!(line_col_offset("a\nbc\nd", 2, 2), Some(3));
        assert_eq!(line_col_offset("a\nbc", 9, 1), None);
        assert_eq!(line_of("a\nb\nc", 4), 3);
        assert_eq!(message_code("PLS-00201: identifier"), Some("PLS-00201".into()));
        assert_eq!(message_code("PL/SQL: Statement ignored"), None);
    }

    #[test]
    fn statement_errors_point_into_the_text() {
        let text = "select 1 from dual;\nselect nope from dual";
        let st = script::split(text);
        let Error::Statement(se) = db_statement_error(904, 7, "ORA-00904: \"NOPE\": invalid identifier\n", text, &st[1]) else {
            panic!()
        };
        assert_eq!(se.code.as_deref(), Some("ORA-00904"));
        assert_eq!(se.message, "ORA-00904: \"NOPE\": invalid identifier");
        assert_eq!(se.offset, Some(text.find("nope").unwrap()));
        assert_eq!(se.line, Some(2));
        assert!(!se.fatal);
        let Error::Statement(se) = db_statement_error(3113, 0, "ORA-03113: end-of-file on communication channel", text, &st[0]) else {
            panic!()
        };
        assert!(se.fatal);
        assert_eq!((se.offset, se.line), (Some(0), Some(1)));
        // An EXEC (rewritten) points at its line.
        let text = "select 1 from dual;\nexec p(1)";
        let st = script::split(text);
        let Error::Statement(se) = db_statement_error(6550, 0, "ORA-06550: line 1, column 7:\nPLS-00201", text, &st[1]) else {
            panic!()
        };
        assert_eq!((se.offset, se.line), (Some(20), Some(2)));
    }

    #[test]
    fn numbers_stay_exact() {
        assert_eq!(number("42"), serde_json::json!(42));
        assert_eq!(number("-7"), serde_json::json!(-7));
        assert_eq!(number("12345678901234567890"), serde_json::json!("12345678901234567890"));
        assert_eq!(number("3.14"), serde_json::json!("3.14"));
        assert_eq!(number("9007199254740993"), serde_json::json!("9007199254740993"));
    }

    #[test]
    fn types_carry_length_and_precision() {
        assert_eq!(format_type("VARCHAR2", Some(20), Some(20), None, None), "VARCHAR2(20)");
        assert_eq!(format_type("NUMBER", None, Some(22), None, None), "NUMBER");
        assert_eq!(format_type("NUMBER", None, Some(22), None, Some(0)), "INTEGER");
        assert_eq!(format_type("NUMBER", None, Some(22), Some(10), Some(2)), "NUMBER(10,2)");
        assert_eq!(format_type("NUMBER", None, Some(22), Some(5), Some(0)), "NUMBER(5)");
        assert_eq!(format_type("TIMESTAMP(6)", Some(0), Some(11), None, Some(6)), "TIMESTAMP(6)");
        assert_eq!(format_type("RAW", Some(0), Some(4), None, None), "RAW(4)");
    }

    #[test]
    fn char_length_semantics_are_kept() {
        // VARCHAR2(40 CHAR): CHAR_LENGTH 40, DATA_LENGTH 160 (AL32UTF8).
        let t = |ty: &str, used: Option<&str>| char_semantics(format_type(ty, Some(40), Some(160), None, None), ty, used);
        assert_eq!(t("VARCHAR2", Some("C")), "VARCHAR2(40 CHAR)");
        assert_eq!(t("CHAR", Some("C")), "CHAR(40 CHAR)");
        assert_eq!(t("VARCHAR2", Some("B")), "VARCHAR2(40)");
        // National types are always in characters: `NVARCHAR2(40 CHAR)` is invalid.
        assert_eq!(t("NVARCHAR2", Some("C")), "NVARCHAR2(40)");
        assert_eq!(t("NCHAR", Some("C")), "NCHAR(40)");
        assert_eq!(char_semantics("NUMBER(10)".into(), "NUMBER", None), "NUMBER(10)");
    }

    #[test]
    fn timestamps_are_iso() {
        let t = OracleTimestamp::new_timestamp(2024, 1, 31, 13, 45, 0, 120_000_000);
        assert_eq!(timestamp(&t, true, false), "2024-01-31 13:45:00.12");
        assert_eq!(timestamp(&t, false, false), "2024-01-31 13:45:00");
        let d = OracleTimestamp::new_date(2024, 2, 1);
        assert_eq!(timestamp(&d, true, false), "2024-02-01 00:00:00");
        let bc = OracleTimestamp::new_timestamp(-44, 3, 15, 12, 0, 0, 0);
        assert_eq!(timestamp(&bc, false, false), "-0044-03-15 12:00:00");
    }

    #[test]
    fn time_zone_values_show_their_local_clock() {
        // The client gives 2024-01-31 13:45:07 -03:00 as 16:45:07 UTC and -3 h.
        let t = OracleTimestamp::new_timestamp_tz(2024, 1, 31, 16, 45, 7, 500_000_000, -3, 0);
        assert_eq!(timestamp(&t, true, true), "2024-01-31 13:45:07.5 -03:00");
        // Across a day, a year, and half-hour offsets both ways.
        let t = OracleTimestamp::new_timestamp_tz(2024, 1, 1, 1, 0, 0, 0, -5, -30);
        assert_eq!(timestamp(&t, false, true), "2023-12-31 19:30:00 -05:30");
        let t = OracleTimestamp::new_timestamp_tz(2023, 12, 31, 22, 0, 0, 0, 5, 45);
        assert_eq!(timestamp(&t, false, true), "2024-01-01 03:45:00 +05:45");
        let t = OracleTimestamp::new_timestamp_tz(2024, 2, 28, 23, 0, 0, 0, 2, 0);
        assert_eq!(timestamp(&t, false, true), "2024-02-29 01:00:00 +02:00");
        // Oracle's calendar: the Gregorian jump, BC leap years, no year 0.
        assert_eq!(shift((1582, 10, 4), 23, 0, 120), ((1582, 10, 15), 1, 0));
        assert_eq!(shift((1582, 10, 15), 0, 30, -60), ((1582, 10, 4), 23, 30));
        assert_eq!(shift((1500, 2, 28), 23, 0, 60), ((1500, 2, 29), 0, 0));
        assert_eq!(shift((1700, 2, 28), 23, 0, 60), ((1700, 3, 1), 0, 0));
        assert_eq!(shift((-4, 2, 28), 23, 0, 60), ((-4, 2, 29), 0, 0));
        assert_eq!(shift((-1, 12, 31), 23, 0, 60), ((1, 1, 1), 0, 0));
        assert_eq!(shift((1, 1, 1), 0, 0, -1), ((-1, 12, 31), 23, 59));
        assert_eq!(parse_offset("+00:00"), Some(0));
        assert_eq!(parse_offset("-03:30"), Some(-210));
        assert_eq!(parse_offset("UTC"), Some(0));
        assert_eq!(parse_offset("Europe/Madrid"), None);
    }

    #[test]
    fn json_numbers_stay_exact() {
        use oracledb::JsonValue as J;
        let n: OracleNumber = "12345678901234567890123.456789".parse().unwrap();
        let doc = J::JsonArray(vec![J::Number(n), J::String("x\"y".into()), J::Null, J::Boolean(true)]);
        assert_eq!(json_text(&doc), r#"[12345678901234567890123.456789,"x\"y",null,true]"#);
    }

    #[test]
    fn autonomous_database_alias_and_wallet() {
        let dir = std::env::temp_dir().join(format!("dbine-wallet-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let pem = dir.join("ewallet.pem");
        std::fs::write(&pem, "x").unwrap();
        let mut cfg = ConnectionConfig { username: Some("ADMIN".into()), ..Default::default() };
        assert!(autonomous_connect_string(&cfg).is_err());
        // A file inside the wallet stands for its folder.
        cfg.options.insert("wallet_dir".into(), pem.to_string_lossy().into());
        assert_eq!(wallet_dir(&cfg).as_deref(), Some(dir.to_string_lossy().as_ref()));
        cfg.options.insert("adb_name".into(), "MiBase".into());
        assert_eq!(autonomous_connect_string(&cfg).unwrap(), "mibase_low");
        cfg.options.insert("adb_service".into(), "tp".into());
        assert_eq!(autonomous_connect_string(&cfg).unwrap(), "mibase_tp");
        cfg.options.insert("connect_descriptor".into(), " (description=(address=(protocol=tcps)(port=1522)(host=h))) ".into());
        assert!(autonomous_connect_string(&cfg).unwrap().starts_with("(description="));
        assert!(build_config(&cfg, true).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
        let i = autonomous_info();
        assert_eq!((i.id, i.default_port), ("oracle_adb", 1522));
    }

    #[test]
    fn connect_string_from_fields() {
        let mut cfg = ConnectionConfig { host: "db.local".into(), ..Default::default() };
        cfg.options.insert("service".into(), "FREEPDB1".into());
        assert_eq!(
            connect_string(&cfg).unwrap(),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCP)(HOST=db.local)(PORT=1521))(CONNECT_DATA=(SERVICE_NAME=FREEPDB1)))"
        );
        cfg.options.insert("connect_by".into(), "sid".into());
        cfg.port = 1522;
        cfg.encrypt = true;
        assert_eq!(
            connect_string(&cfg).unwrap(),
            "(DESCRIPTION=(ADDRESS=(PROTOCOL=TCPS)(HOST=db.local)(PORT=1522))(CONNECT_DATA=(SID=FREEPDB1)))"
        );
        cfg.options.insert("service".into(), "x)(y=z".into());
        assert!(connect_string(&cfg).is_err());
        cfg.options.insert("connect_descriptor".into(), " MY_ALIAS ".into());
        assert_eq!(connect_string(&cfg).unwrap(), "MY_ALIAS");
    }

    #[test]
    fn long_text_is_capped() {
        let s = "é".repeat(TEXT_CAP + 5);
        let c = cap(s);
        assert_eq!(c.chars().count(), TEXT_CAP + 1);
        assert!(c.ends_with('…'));
    }
}
