//! SQL Server over TDS (tiberius).

mod babelfish;
mod backup;
mod clone;
mod create_db;
mod delta;
mod dependencies;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod properties;
mod profiler;
mod schema;
mod script;
mod search;
mod security;
mod structure;
mod transfer;
mod variant;

use dbine_driver::sql::{qualified_name, select_top, Limit, Quote};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig,
    CreateTemplate, DbObject, DdlParts, DesignerSpec, Driver, DriverInfo, Error, ObjectRef, QueryOutcome, ResultColumn, Result, Session, TableSchema,
};
#[cfg(test)]
use futures::TryStreamExt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tiberius::{AuthMethod, Client, ColumnData, Config, EncryptionLevel, FromSql, Row, SqlBrowser};
#[cfg(test)]
use tiberius::QueryItem;
use tokio::net::TcpStream;
use tokio_util::compat::{Compat, TokioAsyncWriteCompatExt};

use variant::{Login, Variant, DEFAULT_PORT};

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    Variant::ALL.iter().map(|v| Arc::new(SqlServerDriver { info: variant::info(*v), variant: *v }) as Arc<dyn Driver>).collect()
}

pub struct SqlServerDriver {
    info: DriverInfo,
    variant: Variant,
}

pub struct SqlServerSession {
    client: Client<Compat<TcpStream>>,
    /// The config the session logged in with (after any Azure redirect),
    /// for reconnecting.
    config: Config,
    spid: i32,
    variant: Variant,
    /// Set by the interrupter so the error of the stopped batch reads as a
    /// cancellation.
    cancelled: Arc<AtomicBool>,
    /// What the interrupter stops: the CURRENT connection, replaced on
    /// reconnect, so the interrupter (taken once per tab) never aims at an
    /// old one.
    cancel: Arc<std::sync::Mutex<CancelTarget>>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// The database a script switched to (`USE`), restored on reconnect.
    database: Option<String>,
    /// Off: `SET IMPLICIT_TRANSACTIONS ON` (manual transactions), restored
    /// on reconnect.
    autocommit: bool,
}

/// How to stop what runs on the session's current connection.
struct CancelTarget {
    /// A TDS attention on the connection itself.
    attention: tiberius::CancelHandle,
    /// Babelfish: its `@@SPID` and login time. Babelfish reads the attention
    /// only once the batch is over (and refuses `pg_cancel_backend` from
    /// T-SQL), so the session is ended with KILL from a second connection,
    /// after checking on the server that the session with that id is still
    /// this one (same login time).
    backend: Option<(i32, String)>,
}

#[async_trait]
impl Driver for SqlServerDriver {
    /// Explicit identity values need IDENTITY_INSERT (one table at a time).
    fn data_load_wrap(&self, table: &dbine_driver::TableSchema) -> (String, String) {
        if !table.columns.iter().any(|c| c.auto_increment) {
            return (String::new(), String::new());
        }
        let name = qualified_name(Quote::Bracket, table.schema.as_deref().filter(|s| !s.is_empty()), &table.name);
        (format!("SET IDENTITY_INSERT {name} ON;"), format!("SET IDENTITY_INSERT {name} OFF;"))
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// A Fabric warehouse has no indexes.
    fn supports_index_usage(&self) -> bool {
        self.variant != Variant::Fabric
    }

    /// Fabric has no indexes; Babelfish doesn't take `ALTER INDEX … DISABLE`.
    fn supports_index_toggle(&self) -> bool {
        !matches!(self.variant, Variant::Fabric | Variant::Babelfish)
    }

    fn index_toggle_script(&self, table: &ObjectRef, index: &dbine_driver::IndexUsage, enable: bool) -> Result<dbine_driver::SyncScript> {
        if !self.supports_index_toggle() {
            return Err(dbine_driver::Error::Unsupported("este motor no deshabilita índices".into()));
        }
        Ok(index_usage::toggle_script(table, index, enable))
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// `INSERT BULK` (TDS bulk load): SQL Server and Azure SQL. Fabric and
    /// Babelfish keep the generic `INSERT` script: neither is known to take
    /// `INSERT BULK` with the hints (`TABLOCK`, `KEEP_NULLS`), identity values
    /// and `SELECT TOP 0` wire declarations this path relies on, and there is
    /// no server to verify it against.
    fn supports_bulk_load(&self) -> bool {
        self.variant.bulk_load()
    }

    /// Sync by rows (see `delta`): SQL Server and Azure SQL, which take the
    /// staging bulk load it needs.
    fn supports_delta(&self) -> bool {
        self.variant.bulk_load()
    }

    fn delta_filter(&self, spec: &dbine_driver::transfer::DeltaSpec, buckets: &[i64]) -> Result<String> {
        if !self.variant.bulk_load() {
            return Err(Error::Unsupported("este motor no sincroniza por filas".into()));
        }
        delta::filter(spec, buckets)
    }

    fn supports_native_copy(&self, target: &str) -> bool {
        self.variant.bulk_load() && Variant::ALL.iter().any(|v| v.bulk_load() && variant::info(*v).id == target)
    }

    /// Raw TDS rows from a `SELECT` straight into `INSERT BULK` when both
    /// sides describe the columns the same way; decoded rows otherwise.
    async fn copy_native(
        &self,
        source: &mut dyn Session,
        target: &mut dyn Session,
        spec: &dbine_driver::transfer::CopySpec,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        fn own(s: &mut dyn Session) -> Option<&mut SqlServerSession> {
            s.as_any()?.downcast_mut::<SqlServerSession>()
        }
        let (Some(src), Some(dst)) = (own(source), own(target)) else {
            return Err(Error::Unsupported("la copia directa necesita dos sesiones de SQL Server".into()));
        };
        if !src.variant.bulk_load() || !dst.variant.bulk_load() {
            return Err(Error::Unsupported("este motor no copia tablas directamente entre bases".into()));
        }
        transfer::copy(src, dst, spec, progress).await
    }

    /// SQL Server and Azure SQL (see `clone` for why not Fabric nor
    /// Babelfish).
    fn supports_clone(&self) -> bool {
        matches!(self.variant, Variant::SqlServer | Variant::AzureSql)
    }

    async fn clone_script(
        &self,
        source: &mut dyn Session,
        target: &mut dyn Session,
        tables: &[ObjectRef],
    ) -> Result<dbine_driver::transfer::CloneScript> {
        clone::clone_script(self.variant, source, target, tables).await
    }

    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        create_db::fields(self.variant)
    }

    fn create_database_script(&self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<String> {
        create_db::script(self.variant, name, options)
    }

    fn alter_database_script(&self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<String> {
        properties::script(self.variant, database, changes)
    }

    /// Azure SQL Database creates and drops databases from `master` with
    /// plain T-SQL; a Fabric warehouse is created in the Fabric portal.
    fn capabilities(&self) -> Capabilities {
        let db = self.variant != Variant::Fabric;
        Capabilities {
            create_database: db,
            drop_database: db,
            foreign_keys: true,
            monitor: true,
            // SQL Server's DMVs (not Fabric's warehouse nor Babelfish).
            blocking: matches!(self.variant, Variant::SqlServer | Variant::AzureSql),
            // KILL ends a session everywhere; nothing stops another
            // session's statement and keeps it (see `processes`).
            kill_session: true,
            processes: true,
            cancel_query: false,
            database_properties: matches!(self.variant, Variant::SqlServer | Variant::AzureSql),
        }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(if self.variant == Variant::Fabric { schema::fabric_designer() } else { schema::designer() })
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        let mut t = schema::create_templates();
        if self.variant == Variant::Fabric {
            // No triggers in a Fabric warehouse.
            t.retain(|t| t.kind != kinds::TRIGGER);
        }
        t
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(if self.variant == Variant::Fabric { schema::fabric_table_ddl(table, parts) } else { schema::table_ddl(table, parts) })
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    /// `ALTER COLUMN` with the default constraints and indexes moved out of
    /// the way; Fabric only adds and drops columns.
    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        use dbine_driver::alter::{AlterStyle, ColumnAlter, DropIndex};
        let fabric = self.variant == Variant::Fabric;
        let cd = |t: &TableSchema, c: &dbine_driver::ColumnDef| dbine_driver::ddl::column_def(&schema::FLAVOR, t, c);
        let dd = |t: &TableSchema, p: DdlParts| self.table_ddl(t, p);
        let mut st = AlterStyle::from_flavor(&schema::FLAVOR, if fabric { ColumnAlter::None } else { ColumnAlter::SqlServer }, &cd, &dd);
        st.add_column = "ADD";
        st.drop_index = DropIndex::OnTable;
        // The primary key is planned as an index (a change of clustered
        // index remakes it); indexes that depend on a remade one are remade
        // too; UNIQUE constraints, the key and memory-optimized indexes drop
        // their own way; foreign keys on a remade key step aside meanwhile.
        let changes = structure::prepare_changes(changes, self.variant);
        // Comments are MS_Description extended properties (Fabric has none).
        let comments = |t: &TableSchema, c: Option<&dbine_driver::ColumnDef>, v: Option<&str>| Some(schema::comment_change(t, c, v));
        let mut script = dbine_driver::alter::sync_script_with_comments(&st, (!fabric).then_some(&comments as dbine_driver::alter::CommentSql), &changes)?;
        structure::fix_drops(&mut script.statements, &changes);
        if !fabric {
            structure::keep_referencing_fks(&mut script.statements, &mut script.warnings, &changes);
            script.warnings.extend(structure::clustering_warnings(&changes));
        }
        Ok(script)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        Ok(schema::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        Ok(schema::update_script(target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        Ok(schema::delete_script(target.schema(), &target.name, keys))
    }

    /// T-SQL: `GO` batches (`GO N`), not split inside comments or strings.
    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        dbine_driver::ScriptDialect::tsql()
    }

    /// The editor sends one `GO` batch per call, as SSMS does.
    fn script_mode(&self) -> dbine_driver::ScriptMode {
        dbine_driver::ScriptMode::Batches
    }

    /// SSMS goes on with the next batch after an error.
    fn script_defaults(&self) -> dbine_driver::ScriptDefaults {
        dbine_driver::ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    /// `SET IMPLICIT_TRANSACTIONS ON`, `@@TRANCOUNT` / `XACT_STATE()`.
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    fn script_separator(&self) -> &'static str {
        "GO"
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec(self.variant))
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(self.variant, action)
    }

    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(security::schema_spec(self.variant))
    }

    fn create_schema_script(&self, _database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        security::create_schema(self.variant, name, owner)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        security::drop_schema(self.variant, name, cascade)
    }

    /// SQL Server (and Managed Instance) only: Azure SQL Database's backups
    /// are the service's, and Fabric and Babelfish have no BACKUP.
    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        (self.variant == Variant::SqlServer).then(backup::spec)
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        match self.variant {
            Variant::SqlServer => backup::script(action),
            Variant::AzureSql => Err(Error::Unsupported(
                "Azure SQL Database hace sus propios backups: se restauran a un momento dado desde el portal o la API de Azure".into(),
            )),
            _ => Err(Error::Unsupported("este motor no tiene backups propios".into())),
        }
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        Ok(Box::new(self.open(cfg, database).await?))
    }
}

impl SqlServerDriver {
    async fn open(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<SqlServerSession> {
        let login = variant::login(cfg, self.variant).await?;
        let config = build_config(cfg, database, self.variant, login)?;
        let (client, config) = connect_routed(config).await.map_err(connect_error)?;
        let cancel = Arc::new(std::sync::Mutex::new(CancelTarget { attention: client.cancel_handle(), backend: None }));
        let mut s = SqlServerSession {
            client,
            config,
            spid: 0,
            variant: self.variant,
            cancelled: Arc::default(),
            cancel,
            profiler: None,
            database: None,
            autocommit: true,
        };
        let rows = s.rows("SELECT CAST(@@SPID AS int)", &[]).await?;
        s.spid = rows.first().and_then(|r| r.get::<i32, _>(0)).unwrap_or(0);
        s.arm_cancel().await.map_err(err)?;
        Ok(s)
    }
}

/// Connect, following an Azure SQL redirect to another node.
async fn connect_routed(config: Config) -> tiberius::Result<(Client<Compat<TcpStream>>, Config)> {
    match connect_once(config.clone()).await {
        Err(tiberius::error::Error::Routing { host, port }) => {
            let mut c = config;
            c.host(&host);
            c.port(port);
            Ok((connect_once(c.clone()).await?, c))
        }
        other => Ok((other?, config)),
    }
}

/// How long the server has to accept the TCP connection, per attempt.
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);
/// Attempts when the server doesn't answer at all (a dropped packet, a slow
/// name lookup): a transient blip shouldn't fail the connection.
const CONNECT_ATTEMPTS: u32 = 2;

async fn connect_once(config: Config) -> tiberius::Result<Client<Compat<TcpStream>>> {
    let mut attempt = 1;
    let tcp = loop {
        // `connect_named` asks the SQL Browser for the port when the host
        // names an instance (host\INSTANCE); otherwise it connects straight
        // away, to every address of the host at once (`multi_subnet_failover`).
        match tokio::time::timeout(CONNECT_TIMEOUT, TcpStream::connect_named(&config)).await {
            Ok(Ok(tcp)) => break tcp,
            Ok(Err(e)) => return Err(unreachable(&config, e)),
            Err(_) if attempt < CONNECT_ATTEMPTS => attempt += 1,
            Err(_) => {
                return Err(tiberius::error::Error::Io { kind: std::io::ErrorKind::TimedOut, message: no_answer(&config).into() })
            }
        }
    };
    tcp.set_nodelay(true)?;
    Client::connect(config, tcp.compat_write()).await
}

/// The OS's reason for not reaching the server, in Spanish and naming it.
fn unreachable(config: &Config, e: tiberius::error::Error) -> tiberius::error::Error {
    use std::io::ErrorKind as K;
    let addr = config.get_addr();
    let message = match &e {
        tiberius::error::Error::Io { kind: K::ConnectionRefused, .. } => {
            format!("el servidor {addr} rechazó la conexión: no hay un SQL Server escuchando en ese puerto (revisá el puerto y que el servicio esté iniciado)")
        }
        // A name that doesn't resolve: NotFound from tiberius, or the
        // resolver's own error (its kind varies by OS).
        tiberius::error::Error::Io { kind, message } if *kind == K::NotFound || message.contains("failed to lookup address") => {
            format!("no se encontró el servidor {addr}: revisá el nombre del host")
        }
        tiberius::error::Error::Io { kind: K::HostUnreachable | K::NetworkUnreachable, .. } => {
            format!("no hay ruta hasta el servidor {addr}: revisá la red o la VPN")
        }
        _ => return e,
    };
    tiberius::error::Error::Io { kind: K::Other, message }
}

fn no_answer(config: &Config) -> String {
    format!(
        "el servidor {} no respondió ({} intentos de {} s). Revisá que esté encendido y que la red, el firewall o la VPN permitan llegar a ese puerto",
        config.get_addr(),
        CONNECT_ATTEMPTS,
        CONNECT_TIMEOUT.as_secs()
    )
}

fn connect_error(e: tiberius::error::Error) -> Error {
    match &e {
        // 18456: login failed.
        tiberius::error::Error::Server(t) if t.code() == 18456 => Error::AuthFailed(t.message().to_string()),
        // 18452: a Windows login from a domain the server doesn't trust.
        tiberius::error::Error::Server(t) if t.code() == 18452 => Error::AuthFailed(format!(
            "{} El servidor no reconoce la cuenta de Windows: revisá que la computadora o el usuario sean del dominio \
             (o de uno de confianza) y que el host sea el nombre del servidor en el dominio.",
            t.message()
        )),
        // Kerberos on macOS and Linux: no ticket, an expired one, or no KDC.
        #[cfg(unix)]
        tiberius::error::Error::Gssapi(m) => Error::AuthFailed(kerberos_hint(m)),
        tiberius::error::Error::Server(t) => Error::Connect(t.message().to_string()),
        // The network's own message (or ours, for a timeout), without
        // tiberius' English prefix.
        tiberius::error::Error::Io { message, .. } => Error::Connect(message.clone()),
        _ => Error::Connect(e.to_string()),
    }
}

/// A GSSAPI failure, explained: what usually fixes it comes first, the
/// library's own text after.
#[cfg(unix)]
fn kerberos_hint(gss: &str) -> String {
    let lower = gss.to_ascii_lowercase();
    let what = if lower.contains("no kerberos credentials")
        || lower.contains("no credentials")
        || lower.contains("credentials cache")
        || lower.contains("expired")
    {
        "no hay un ticket de Kerberos vigente"
    } else if lower.contains("server not found") || lower.contains("unknown server") || lower.contains("not found in kerberos database") {
        "el dominio no conoce el servicio MSSQLSvc de ese servidor (usá el nombre completo del host, no la IP ni un alias)"
    } else if lower.contains("realm") || lower.contains("kdc") {
        "no se pudo llegar al controlador de dominio (KDC)"
    } else {
        "falló Kerberos"
    };
    let fix = if cfg!(target_os = "macos") {
        "Pedí un ticket con «kinit usuario@DOMINIO» en la Terminal o con la app Ticket Viewer, y revisá que el Mac vea la red del dominio."
    } else {
        "Pedí un ticket con «kinit usuario@DOMINIO» (klist muestra el actual) y revisá /etc/krb5.conf y que la red llegue al dominio."
    };
    format!("Windows (Kerberos): {what}. {fix} Detalle: {gss}")
}

/// A statement's failure: the server's own message when there is one. A
/// connection tiberius won't use anymore is a connection error, so the app
/// drops the session and the next call opens a new one.
fn err(e: tiberius::error::Error) -> Error {
    match &e {
        tiberius::error::Error::Server(t) => Error::Query(t.message().to_string()),
        // The server acknowledged the interrupter's attention.
        tiberius::error::Error::Cancelled => Error::Cancelled,
        _ if is_desync(&e) => Error::Connect(format!("se perdió la conexión con el servidor: {e}")),
        _ => Error::Query(e.to_string()),
    }
}

/// tiberius refuses every command on a connection it can't trust anymore:
/// one a command timeout left half-read ("out of sync"), or one a cancelled
/// multi-packet write left with half a message on the wire ("cancelled
/// write"). The refusal comes before anything is sent, so the command can
/// go again on a new connection.
fn is_desync(e: &tiberius::error::Error) -> bool {
    matches!(e, tiberius::error::Error::Protocol(m) if m.contains("can no longer be used"))
}

fn build_config(cfg: &ConnectionConfig, database: Option<&str>, variant: Variant, login: Login) -> Result<Config> {
    let mut c = Config::new();
    let (host, instance, port) = parse_host(&cfg.host, cfg.port_or(DEFAULT_PORT));
    c.host(host);
    c.port(port);
    if let Some(i) = instance {
        c.instance_name(i);
    }
    let db = database.filter(|d| !d.is_empty()).unwrap_or(&cfg.database);
    if !db.is_empty() {
        c.database(db);
    }
    c.application_name("DBine");
    // Every address of the host at once: one that doesn't answer (an IPv6
    // route that goes nowhere, a stale record) doesn't use up the timeout.
    c.multi_subnet_failover(true);
    // Bigger packets for bulk loads; the server may answer with less.
    c.packet_size(transfer::PACKET_SIZE);
    // tiberius 0.13 cuts commands at 30 s and the connection is unusable
    // afterwards. Long queries are the user's to cancel.
    c.command_timeout(None);
    // Entra ID tokens only travel encrypted; Azure and Fabric refuse plain
    // TDS anyway.
    let encrypt = cfg.encrypt || variant.forces_encryption() || matches!(login, Login::Token(_));
    c.encryption(match (encrypt, variant) {
        (true, _) => EncryptionLevel::Required,
        // Babelfish without SSL configured refuses even the login-only TLS
        // that `Off` asks for.
        (false, Variant::Babelfish) => EncryptionLevel::NotSupported,
        (false, _) => EncryptionLevel::Off,
    });
    // Without encryption TLS only wraps the login packet, and like ODBC's
    // Encrypt=no the certificate isn't checked (SQL Server's auto-generated
    // one is self-signed and too old for rustls).
    if cfg.trust_server_certificate || !encrypt {
        c.trust_cert();
    }
    if cfg.read_only {
        c.readonly(true);
    }
    c.authentication(auth_method(login)?);
    Ok(c)
}

fn auth_method(login: Login) -> Result<AuthMethod> {
    Ok(match login {
        Login::Sql { user, password } => AuthMethod::sql_server(user, password),
        Login::Token(t) => AuthMethod::aad_token(t),
        Login::Integrated => AuthMethod::Integrated,
        // NTLMv2 on every OS (vendor/tiberius PATCHES.md: winauth off Windows too).
        Login::Windows { user, password } => AuthMethod::windows(user, password),
    })
}

/// "host\INSTANCE" or "host,port" as SQL Server tools accept them.
fn parse_host(host: &str, port: u16) -> (&str, Option<&str>, u16) {
    let host = if host.is_empty() { "localhost" } else { host };
    let (host, instance) = match host.split_once('\\') {
        Some((h, i)) => (h, Some(i)),
        None => (host, None),
    };
    match host.split_once(',') {
        Some((h, p)) => (h, instance, p.trim().parse().unwrap_or(port)),
        None => (host, instance, port),
    }
}

pub(crate) fn cell(data: ColumnData<'static>) -> serde_json::Value {
    use serde_json::Value;
    fn opt<T>(v: Option<T>, f: impl FnOnce(T) -> Value) -> Value {
        v.map_or(Value::Null, f)
    }
    fn chrono_of<T: for<'a> FromSql<'a>>(d: &ColumnData<'static>, fmt: impl FnOnce(T) -> String) -> Value {
        match T::from_sql(d) {
            Ok(Some(v)) => fmt(v).into(),
            Ok(None) => Value::Null,
            Err(e) => format!("<{e}>").into(),
        }
    }
    match data {
        ColumnData::U8(v) => opt(v, |x| x.into()),
        ColumnData::I16(v) => opt(v, |x| x.into()),
        ColumnData::I32(v) => opt(v, |x| x.into()),
        ColumnData::I64(v) => opt(v, json_i64),
        ColumnData::F32(v) => opt(v, |x| json_f64(x as f64)),
        ColumnData::F64(v) => opt(v, json_f64),
        ColumnData::Bit(v) => opt(v, Value::Bool),
        ColumnData::String(v) => opt(v, |s| s.into_owned().into()),
        ColumnData::Guid(v) => opt(v, |g| g.to_string().to_uppercase().into()),
        ColumnData::Binary(v) => opt(v, |b| json_bytes(&b)),
        ColumnData::Numeric(v) => opt(v, |n| n.to_string().into()),
        ColumnData::Xml(v) => opt(v, |x| x.to_string().into()),
        d @ (ColumnData::DateTime(_) | ColumnData::SmallDateTime(_) | ColumnData::DateTime2(_)) => {
            chrono_of::<chrono::NaiveDateTime>(&d, |v| v.format("%Y-%m-%d %H:%M:%S%.f").to_string())
        }
        d @ ColumnData::Date(_) => chrono_of::<chrono::NaiveDate>(&d, |v| v.format("%Y-%m-%d").to_string()),
        d @ ColumnData::Time(_) => chrono_of::<chrono::NaiveTime>(&d, |v| v.format("%H:%M:%S%.f").to_string()),
        d @ ColumnData::DateTimeOffset(_) => chrono_of::<chrono::DateTime<chrono::FixedOffset>>(&d, |v| {
            v.format("%Y-%m-%d %H:%M:%S%.f %:z").to_string()
        }),
    }
}

/// A single-column result set that carries a plan while explaining.
fn is_plan_column(variant: Variant, name: &str) -> bool {
    match variant {
        // The plan, then the batch's parsing time under an unnamed column.
        Variant::Babelfish => name == babelfish::PLAN_COLUMN || name.is_empty(),
        _ => name == plan::SHOWPLAN_COLUMN,
    }
}

impl SqlServerSession {
    /// Rows of a catalog query with string parameters.
    /// Catalog reads depend on no session state, so a broken connection is
    /// replaced and the read retried.
    async fn rows(&mut self, sql: &str, params: &[&str]) -> Result<Vec<Row>> {
        match self.try_rows(sql, params).await {
            Err(e) if is_desync(&e) => {
                self.reconnect().await?;
                self.try_rows(sql, params).await.map_err(err)
            }
            other => other.map_err(err),
        }
    }

    async fn try_rows(&mut self, sql: &str, params: &[&str]) -> tiberius::Result<Vec<Row>> {
        let p: Vec<&dyn tiberius::ToSql> = params.iter().map(|s| s as &dyn tiberius::ToSql).collect();
        self.client.query(sql, &p).await?.into_first_result().await
    }

    /// A new connection with the config the session logged in with.
    async fn reconnect(&mut self) -> Result<()> {
        self.client = connect_once(self.config.clone()).await.map_err(connect_error)?;
        let rows = self.try_rows("SELECT CAST(@@SPID AS int)", &[]).await.map_err(err)?;
        self.spid = rows.first().and_then(|r| r.get::<i32, _>(0)).unwrap_or(0);
        self.arm_cancel().await.map_err(err)?;
        // What the tab sees stays true: the database it followed and its
        // manual-transaction mode (the transaction itself is gone).
        let mut restore = Vec::new();
        if let Some(db) = &self.database {
            restore.push(format!("USE [{}]", db.replace(']', "]]")));
        }
        if !self.autocommit {
            restore.push("SET IMPLICIT_TRANSACTIONS ON".into());
        }
        for sql in restore {
            let run = async { self.client.simple_query(sql.as_str()).await?.into_results().await };
            if let Err(e) = run.await {
                tracing::debug!("could not restore `{sql}` after reconnecting: {e}");
            }
        }
        Ok(())
    }

    /// A batch whose results don't matter (SET, COMMIT…).
    async fn batch(&mut self, sql: &str) -> Result<()> {
        let sent = self.client.simple_query(sql).await;
        if matches!(&sent, Err(e) if is_desync(e)) {
            drop(sent);
            return Err(self.lost_connection().await);
        }
        sent.map_err(err)?.into_results().await.map_err(err)?;
        Ok(())
    }

    /// Point the interrupter at the current connection.
    async fn arm_cancel(&mut self) -> tiberius::Result<()> {
        let backend = if self.variant == Variant::Babelfish {
            let rows = self.try_rows(BABELFISH_LOGIN_TIME, &[]).await?;
            rows.first().and_then(|r| text(r, 0)).map(|t| (self.spid, t))
        } else {
            None
        };
        *self.cancel.lock().unwrap_or_else(|e| e.into_inner()) = CancelTarget { attention: self.client.cancel_handle(), backend };
        Ok(())
    }

    /// A user statement found the connection broken: a new one is opened for
    /// the next run, but the statement is not retried, since the tab's
    /// transaction, #temp tables, USE and SET are gone.
    async fn lost_connection(&mut self) -> Error {
        match self.reconnect().await {
            Ok(()) => Error::Query(
                "La conexión con el servidor había quedado inutilizable y se abrió una nueva. \
                 Se perdió el estado de la sesión anterior (transacción abierta, tablas #temp y SET): \
                 volvé a ejecutar."
                    .into(),
            ),
            Err(re) => re,
        }
    }
}

/// The online databases, by name, with what decides whether the explorer
/// shows them: the login's access and the engine edition.
const DATABASES_SQL: &str = "SELECT name, CAST(database_id AS int), CAST(HAS_DBACCESS(name) AS int),
       CAST(SERVERPROPERTY('EngineEdition') AS int)
  FROM sys.databases
 WHERE state_desc = 'ONLINE'
 ORDER BY name";

struct DatabaseRow {
    name: String,
    id: i32,
    /// `HAS_DBACCESS`: 1, 0, or NULL.
    access: Option<i32>,
}

/// Engines with no cross-database access (EngineEdition 5 Azure SQL
/// Database, 6 Synapse dedicated pool, 11 Synapse serverless, 12 SQL
/// database in Fabric): `HAS_DBACCESS` only says yes for the connection's
/// own database, even for the server admin, while `sys.databases` (in
/// `master`) already lists only the databases the login may see. Every one
/// of them is opened on a connection of its own that logs into it. The
/// Fabric warehouse driver keeps the access check.
fn database_scoped(variant: Variant, edition: i32) -> bool {
    variant != Variant::Fabric && matches!(edition, 5 | 6 | 11 | 12)
}

/// The databases the explorer lists, user databases first and the system
/// ones last, each group by name (`rows` come sorted by name). On SQL
/// Server the system databases are ids 1–4; on the database-scoped engines
/// only `master` is, whatever ids the rest have.
fn visible_databases(rows: Vec<DatabaseRow>, scoped: bool) -> Vec<String> {
    let mut dbs: Vec<(bool, String)> = rows
        .into_iter()
        .filter(|d| scoped || d.access == Some(1))
        .map(|d| (if scoped { d.name.eq_ignore_ascii_case("master") } else { d.id <= 4 }, d.name))
        .collect();
    dbs.sort_by_key(|(system, _)| *system);
    dbs.into_iter().map(|(_, name)| name).collect()
}

fn text(r: &Row, i: usize) -> Option<String> {
    r.try_get::<&str, _>(i).ok().flatten().map(str::to_string)
}

#[async_trait]
impl Session for SqlServerSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.rows("SELECT CAST(@@VERSION AS nvarchar(4000))", &[]).await?;
        let v = rows.first().and_then(|r| text(r, 0)).unwrap_or_default();
        Ok(v.lines().next().unwrap_or("SQL Server").trim().to_string())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.rows(DATABASES_SQL, &[]).await?;
        let edition = rows.first().and_then(|r| r.get::<i32, _>(3)).unwrap_or(0);
        let dbs = rows
            .iter()
            .filter_map(|r| Some(DatabaseRow { name: text(r, 0)?, id: r.get::<i32, _>(1).unwrap_or(0), access: r.get::<i32, _>(2) }))
            .collect();
        Ok(visible_databases(dbs, database_scoped(self.variant, edition)))
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let rows = self
            .rows(
                "SELECT RTRIM(o.type), s.name, o.name, OBJECT_NAME(o.parent_object_id)
                   FROM sys.objects o
                   JOIN sys.schemas s ON s.schema_id = o.schema_id
                  WHERE o.is_ms_shipped = 0
                    AND o.type IN ('U','V','P','PC','FN','IF','TF','FS','FT','TR')
                  ORDER BY s.name, o.name",
                &[],
            )
            .await?;
        let mut objects: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                let kind = match text(r, 0)?.as_str() {
                    "U" => kinds::TABLE,
                    "V" => kinds::VIEW,
                    "P" | "PC" => kinds::PROCEDURE,
                    "TR" => kinds::TRIGGER,
                    _ => kinds::FUNCTION,
                };
                Some(DbObject {
                    kind: kind.into(),
                    schema: text(r, 1),
                    name: text(r, 2)?,
                    parent: if kind == kinds::TRIGGER { text(r, 3) } else { None },
                })
            })
            .collect();
        objects.extend(structure::list_objects(self).await);
        Ok(objects)
    }

    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        let rows = self.rows(&security::list_schemas_sql(self.variant), &[]).await?;
        Ok(Some(
            rows.iter()
                .filter_map(|r| Some(dbine_driver::SchemaInfo { name: text(r, 0)?, system: r.get::<i32, _>(1).unwrap_or(0) == 1 }))
                .collect(),
        ))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let object = qualified_name(Quote::Bracket, obj.schema(), &obj.name);
        let rows = self
            .rows(
                "SELECT c.name, TYPE_NAME(c.user_type_id), CAST(c.max_length AS int),
                        CAST(c.precision AS int), CAST(c.scale AS int),
                        c.is_nullable, c.is_identity, OBJECT_DEFINITION(c.default_object_id),
                        CAST(CASE WHEN EXISTS (
                            SELECT 1 FROM sys.index_columns ic
                              JOIN sys.indexes i ON i.object_id = ic.object_id AND i.index_id = ic.index_id
                             WHERE i.is_primary_key = 1 AND ic.object_id = c.object_id AND ic.column_id = c.column_id
                        ) THEN 1 ELSE 0 END AS bit)
                   FROM sys.columns c
                  WHERE c.object_id = OBJECT_ID(@P1)
                  ORDER BY c.column_id",
                &[&object],
            )
            .await?;
        Ok(rows
            .iter()
            .map(|r| {
                let ty = text(r, 1).unwrap_or_default();
                let max_len: i32 = r.get(2).unwrap_or(0);
                let precision: i32 = r.get(3).unwrap_or(0);
                let scale: i32 = r.get(4).unwrap_or(0);
                ColumnInfo {
                    name: text(r, 0).unwrap_or_default(),
                    data_type: format_type(&ty, max_len, precision, scale),
                    nullable: r.get(5).unwrap_or(true),
                    auto_increment: r.get(6).unwrap_or(false),
                    default_value: text(r, 7),
                    primary_key: r.get(8).unwrap_or(false),
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        // SQL Server has no CREATE TABLE of its own: the app builds one.
        if obj.kind == kinds::TABLE {
            return Ok(None);
        }
        // Objects without a stored source: rebuilt from the catalog.
        if structure::KINDS.contains(&obj.kind.as_str()) {
            return structure::definition(self, obj).await;
        }
        let object = qualified_name(Quote::Bracket, obj.schema(), &obj.name);
        let rows = self.rows("SELECT OBJECT_DEFINITION(OBJECT_ID(@P1))", &[&object]).await?;
        Ok(rows.first().and_then(|r| text(r, 0)))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Bracket, Limit::Top, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.cancelled.store(false, Ordering::SeqCst);
        let res = self.run_batches(sql, max_rows, out, None).await;
        match res {
            Err(_) if self.cancelled.swap(false, Ordering::SeqCst) => Err(Error::Cancelled),
            other => other,
        }
    }

    /// Estimated: `SET SHOWPLAN_XML ON` (nothing runs). Actual: `SET
    /// STATISTICS XML ON`, the script runs once and each statement's plan
    /// comes back with actual rows and times.
    async fn explain(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let babelfish = self.variant == Variant::Babelfish;
        let option = match (babelfish, analyze) {
            // PostgreSQL's EXPLAIN (ANALYZE) as text, per statement.
            (true, true) => "BABELFISH_STATISTICS PROFILE",
            (true, false) => "BABELFISH_SHOWPLAN_ALL",
            (false, true) => "STATISTICS XML",
            (false, false) => "SHOWPLAN_XML",
        };
        // Each SET must be alone in its batch.
        let sent = self.client.simple_query(format!("SET {option} ON")).await;
        if matches!(&sent, Err(e) if is_desync(e)) {
            drop(sent);
            return Err(self.lost_connection().await);
        }
        sent.map_err(err)?.into_results().await.map_err(err)?;
        self.cancelled.store(false, Ordering::SeqCst);
        let mut xmls = Vec::new();
        let res = self.run_batches(sql, max_rows, out, Some(&mut xmls)).await;
        // Always switch it off: the session stays with this tab.
        let off = async { self.client.simple_query(format!("SET {option} OFF")).await?.into_results().await };
        if let Err(e) = off.await {
            tracing::debug!("could not reset {option}: {e}");
        }
        if babelfish {
            let text = xmls.concat();
            out.plans.extend(babelfish::parse_text_plans(&text, analyze));
            xmls.clear();
        }
        for xml in &xmls {
            match plan::parse_showplan(xml, analyze) {
                Ok(p) => out.plans.extend(p),
                Err(e) => out.messages.push(format!("No se pudo leer el plan: {e}")),
            }
        }
        if !analyze {
            // SHOWPLAN puts an empty "done" per statement; they aren't results.
            out.results.retain(|r| !r.columns.is_empty());
        }
        match res {
            Err(_) if self.cancelled.swap(false, Ordering::SeqCst) => Err(Error::Cancelled),
            other => other,
        }
    }

    /// `@@TRANCOUNT` and `XACT_STATE()` (-1: only a rollback ends it).
    async fn transaction_state(&mut self) -> Result<Option<dbine_driver::TxState>> {
        let rows = self.rows("SELECT CAST(@@TRANCOUNT AS int), CAST(XACT_STATE() AS int)", &[]).await?;
        let r = rows.first();
        let open = r.and_then(|r| r.get::<i32, _>(0)).unwrap_or(0) > 0;
        let state = r.and_then(|r| r.get::<i32, _>(1)).unwrap_or(0);
        Ok(Some(match (open, state) {
            (_, -1) => dbine_driver::TxState::Failed,
            (true, _) => dbine_driver::TxState::Open,
            _ => dbine_driver::TxState::Idle,
        }))
    }

    /// Manual: `SET IMPLICIT_TRANSACTIONS ON`, so the first statement opens
    /// a transaction that stays open until Commit / Rollback (as SSMS's
    /// option of the same name).
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        self.batch(if on { "SET IMPLICIT_TRANSACTIONS OFF" } else { "SET IMPLICIT_TRANSACTIONS ON" }).await?;
        self.autocommit = on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.batch("WHILE @@TRANCOUNT > 0 COMMIT TRANSACTION").await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.batch("IF @@TRANCOUNT > 0 ROLLBACK TRANSACTION").await
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let mut b = schema::Builder::default();
        for r in self.rows(schema::TABLES_SQL, &[]).await? {
            b.table(text(&r, 0).unwrap_or_default(), text(&r, 1).unwrap_or_default(), text(&r, 2));
        }
        for r in self.rows(schema::COLUMNS_SQL, &[]).await? {
            let (s, t) = (text(&r, 0).unwrap_or_default(), text(&r, 1).unwrap_or_default());
            let ty = text(&r, 3).unwrap_or_default();
            let data_type = match text(&r, 10) {
                Some(expr) if r.get::<bool, _>(11).unwrap_or(false) => format!("AS {expr} PERSISTED"),
                Some(expr) => format!("AS {expr}"),
                None => format_type(&ty, r.get(4).unwrap_or(0), r.get(5).unwrap_or(0), r.get(6).unwrap_or(0)),
            };
            b.column(
                &s,
                &t,
                ColumnDef {
                    name: text(&r, 2).unwrap_or_default(),
                    data_type,
                    nullable: r.get(7).unwrap_or(true),
                    auto_increment: r.get(8).unwrap_or(false),
                    default_value: text(&r, 9),
                    comment: text(&r, 12),
                    ..Default::default()
                },
            );
        }
        // IDENTITY seed / increment (Fabric's IDENTITY has neither).
        if self.variant != Variant::Fabric {
            match self.rows(schema::IDENTITY_SQL, &[]).await {
                Ok(rows) => {
                    for r in rows {
                        let t = |i| text(&r, i).unwrap_or_default();
                        b.identity(&t(0), &t(1), &t(2), &t(3), &t(4));
                    }
                }
                Err(e) => tracing::warn!("sqlserver: identity seed / increment not read: {e}"),
            }
        }
        for r in self.rows(schema::INDEXES_SQL, &[]).await? {
            b.index_column(
                &text(&r, 0).unwrap_or_default(),
                &text(&r, 1).unwrap_or_default(),
                text(&r, 2).unwrap_or_default(),
                r.get(3).unwrap_or(false),
                r.get(4).unwrap_or(false),
                text(&r, 5).unwrap_or_default(),
                text(&r, 6),
                text(&r, 7).unwrap_or_default(),
                r.get(8).unwrap_or(false),
            );
        }
        for r in self.rows(schema::FOREIGN_KEYS_SQL, &[]).await? {
            b.fk_column(
                &text(&r, 0).unwrap_or_default(),
                &text(&r, 1).unwrap_or_default(),
                text(&r, 2).unwrap_or_default(),
                text(&r, 3).unwrap_or_default(),
                text(&r, 4).unwrap_or_default(),
                text(&r, 5).unwrap_or_default(),
                text(&r, 6).unwrap_or_default(),
                schema::fk_rule(text(&r, 7)),
                schema::fk_rule(text(&r, 8)),
            );
        }
        let mut tables = b.finish();
        structure::complete(self, &mut tables).await;
        Ok(tables)
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        let sql = format!("CREATE DATABASE {}", qualified_name(Quote::Bracket, None, name));
        let sent = self.client.simple_query(sql).await;
        if matches!(&sent, Err(e) if is_desync(e)) {
            drop(sent);
            return Err(self.lost_connection().await);
        }
        sent.map_err(err)?.into_results().await.map_err(err)?;
        Ok(())
    }

    async fn search_code(&mut self, query: &dbine_driver::search::CodeSearch) -> Result<Option<dbine_driver::search::CodeSearchReport>> {
        self.search_code_impl(query).await
    }

    async fn database_properties(&mut self, database: &str) -> Result<dbine_driver::DatabaseProperties> {
        self.properties(database).await
    }

    async fn alter_database(&mut self, database: &str, changes: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.alter_database_impl(database, changes).await
    }

    async fn create_database_choices(&mut self) -> Result<Vec<dbine_driver::FieldChoices>> {
        self.create_database_choices_impl().await
    }

    async fn create_database_with(&mut self, name: &str, options: &std::collections::BTreeMap<String, String>) -> Result<()> {
        self.create_database_with_impl(name, options).await
    }

    /// Other connections are rolled back and closed first (SINGLE_USER WITH
    /// ROLLBACK IMMEDIATE), as SSMS's "close existing connections" does.
    async fn drop_database(&mut self, name: &str) -> Result<()> {
        let current = self.rows("SELECT DB_NAME()", &[]).await?.first().and_then(|r| text(r, 0)).unwrap_or_default();
        if current.eq_ignore_ascii_case(name) {
            return Err(Error::Query(format!(
                "No se puede borrar «{name}»: es la base de esta conexión. Conectate a otra (por ejemplo master)."
            )));
        }
        let db = qualified_name(Quote::Bracket, None, name);
        // Not available everywhere (Azure SQL Database): the DROP still runs.
        let single = format!("ALTER DATABASE {db} SET SINGLE_USER WITH ROLLBACK IMMEDIATE");
        match self.client.simple_query(single).await {
            Ok(s) => {
                if let Err(e) = s.into_results().await {
                    tracing::debug!("sqlserver: SINGLE_USER refused: {e}");
                }
            }
            Err(e) => tracing::debug!("sqlserver: SINGLE_USER refused: {e}"),
        }
        self.client.simple_query(format!("DROP DATABASE {db}")).await.map_err(err)?.into_results().await.map_err(err)?;
        Ok(())
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        match self.variant {
            Variant::Babelfish => babelfish::monitor(self).await,
            _ => monitor::snapshot(self).await,
        }
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        match self.variant {
            Variant::SqlServer | Variant::AzureSql => monitor::blocking(self).await,
            _ => Err(Error::Unsupported("este motor no informa bloqueos entre sesiones".into())),
        }
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        match self.variant {
            Variant::SqlServer => backup::history(self, database).await,
            _ => Err(Error::Unsupported("este motor no lista sus backups".into())),
        }
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        monitor::kill(self, id).await
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        processes::processes(self).await
    }

    async fn cancel_query(&mut self, _id: &str) -> Result<()> {
        processes::cancel(self)
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
        match self.profiler.take() {
            Some(state) => profiler::stop(self, state).await,
            None => Ok(()),
        }
    }

    /// Typed, lossless: one unordered `SELECT` (see `transfer`).
    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        if !self.variant.bulk_load() {
            return Err(Error::Unsupported("este motor no tiene carga masiva".into()));
        }
        transfer::bulk_load(self, spec, columns, source, progress).await
    }

    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    async fn key_range(&mut self, table: &ObjectRef, column: &str) -> Result<Option<(i64, i64, u64)>> {
        if !self.variant.bulk_load() {
            return Err(Error::Unsupported("este motor no sincroniza por filas".into()));
        }
        delta::key_range(self, table, column).await
    }

    async fn delta_summary(&mut self, spec: &dbine_driver::transfer::DeltaSpec) -> Result<Vec<dbine_driver::transfer::BucketSum>> {
        if !self.variant.bulk_load() {
            return Err(Error::Unsupported("este motor no sincroniza por filas".into()));
        }
        delta::summary(self, spec).await
    }

    async fn delta_apply(
        &mut self,
        spec: &dbine_driver::transfer::DeltaSpec,
        buckets: &[i64],
        columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<dbine_driver::transfer::DeltaResult> {
        if !self.variant.bulk_load() {
            return Err(Error::Unsupported("este motor no sincroniza por filas".into()));
        }
        delta::apply(self, spec, buckets, columns, source, progress).await
    }

    /// A TDS attention on the session's own connection, as SSMS sends: the
    /// server stops the batch and the session lives on (transaction, #temp
    /// tables, SET options). Nothing goes to another connection, so no other
    /// session can be hit, and after a reconnect it reaches the new one.
    /// Babelfish: KILL of the verified session (see `CancelTarget`), which
    /// ends it.
    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let (target, flag, config) = (self.cancel.clone(), self.cancelled.clone(), self.config.clone());
        let rt = tokio::runtime::Handle::try_current().ok();
        Some(Arc::new(move || {
            flag.store(true, Ordering::SeqCst);
            let t = target.lock().unwrap_or_else(|e| e.into_inner());
            match (&t.backend, &rt) {
                (Some((spid, login)), Some(rt)) => {
                    rt.spawn(babelfish_kill(config.clone(), *spid, login.clone()));
                }
                _ => t.attention.cancel(),
            }
        }))
    }

    /// One query: IS_SRVROLEMEMBER and HAS_PERMS_BY_NAME (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }

    /// The catalog plus `sys.dm_db_index_usage_stats` (see `index_usage`).
    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        if self.variant == Variant::Fabric {
            return Ok(None);
        }
        index_usage::report(self, table).await.map(Some)
    }

    async fn dependents(&mut self, target: &dbine_driver::DependencyTarget, scan: &dbine_driver::DependencyScan) -> Result<dbine_driver::DependencyReport> {
        match dependencies::dependents(self, target, scan).await {
            // Babelfish and Fabric may lack sys.sql_expression_dependencies: the generic scan.
            Err(_) if matches!(self.variant, Variant::Babelfish | Variant::Fabric) => dbine_driver::dependencies::scan(self, target, scan).await,
            r => r,
        }
    }
}

/// The session's login time, as text, identifying its Babelfish backend.
const BABELFISH_LOGIN_TIME: &str =
    "SELECT CONVERT(varchar(40), login_time, 121) FROM sys.dm_exec_sessions WHERE session_id = @@SPID";

/// `KILL spid`, only if that session is still the one that logged in at
/// `login`: once that connection is gone the id can be another client's.
/// The check and the KILL go in one statement.
async fn babelfish_kill(config: Config, spid: i32, login: String) {
    let sql = format!(
        "IF EXISTS (SELECT 1 FROM sys.dm_exec_sessions \
                     WHERE session_id = {spid} AND CONVERT(varchar(40), login_time, 121) = @P1) \
         KILL {spid}"
    );
    let run = async {
        let mut c = connect_once(config).await?;
        c.execute(sql.as_str(), &[&login.as_str()]).await?;
        Ok::<_, tiberius::error::Error>(())
    };
    if let Err(e) = run.await {
        tracing::debug!("babelfish cancel failed: {e}");
    }
}

/// `nvarchar(50)`, `decimal(18,2)`, `varbinary(max)`… from sys.columns.
fn format_type(ty: &str, max_len: i32, precision: i32, scale: i32) -> String {
    let len = |bytes_per_char: i32| {
        if max_len == -1 {
            "max".to_string()
        } else {
            (max_len / bytes_per_char).to_string()
        }
    };
    match ty {
        "varchar" | "char" | "varbinary" | "binary" => format!("{ty}({})", len(1)),
        "nvarchar" | "nchar" => format!("{ty}({})", len(2)),
        "decimal" | "numeric" => format!("{ty}({precision},{scale})"),
        "datetime2" | "time" | "datetimeoffset" => format!("{ty}({scale})"),
        _ => ty.to_string(),
    }
}

#[cfg(test)]
mod cancel_live;

#[cfg(test)]
mod script_live;

#[cfg(test)]
mod tests {
    use super::*;

    /// "Con opción de otorgar" is offered on the new schema's grants
    /// exactly where the engine writes them (`SchemaSpec::grant_option`).
    #[test]
    fn schema_grant_option_matches_the_script() {
        for d in crate::drivers() {
            let Some(spec) = d.schema_spec() else { continue };
            let Some(p) = spec.privileges.first() else { continue };
            let grant = |grantable| d.schema_grant_script(None, "ventas", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }

    #[test]
    fn go_splits_batches() {
        let b: Vec<String> = script::batches("select 1\nGO\n  go  \nselect 2\n").into_iter().map(|u| u.text).collect();
        assert_eq!(b, vec!["select 1", "select 2"]);
    }

    #[test]
    fn types_carry_their_length() {
        assert_eq!(format_type("nvarchar", 100, 0, 0), "nvarchar(50)");
        assert_eq!(format_type("varchar", -1, 0, 0), "varchar(max)");
        assert_eq!(format_type("decimal", 9, 18, 2), "decimal(18,2)");
        assert_eq!(format_type("int", 4, 10, 0), "int");
    }

    #[test]
    fn desync_is_recognised() {
        let e = tiberius::error::Error::Protocol(
            "connection was left out of sync with the server by a command timeout (the previous response is only partly read) and can no longer be used; open a new connection".into(),
        );
        assert!(is_desync(&e));
        assert!(!is_desync(&tiberius::error::Error::Protocol("bad token".into())));
        // Half a message left on the wire by a cancelled write (the schema
        // compare report): the same, and a connection error for the app.
        let poisoned = tiberius::error::Error::Protocol(
            "connection was left in an inconsistent state by a cancelled write and can no longer be used; open a new connection".into(),
        );
        assert!(is_desync(&poisoned));
        assert!(matches!(err(poisoned), Error::Connect(_)));
        assert!(matches!(err(tiberius::error::Error::Protocol("bad token".into())), Error::Query(_)));
    }

    /// Each form login becomes the tiberius authentication it stands for.
    #[tokio::test]
    async fn logins_become_their_auth_method() {
        let mut cfg = ConnectionConfig {
            host: "sql01.contoso.local".into(),
            username: Some("CONTOSO\\ana".into()),
            password: Some("s3cret".into()),
            ..Default::default()
        };
        let method = |cfg: &ConnectionConfig| {
            let cfg = cfg.clone();
            async move { auth_method(variant::login(&cfg, Variant::SqlServer).await?) }
        };
        assert_eq!(method(&cfg).await.unwrap(), AuthMethod::sql_server("CONTOSO\\ana", "s3cret"));

        cfg.options.insert("auth".into(), variant::WINDOWS_INTEGRATED.into());
        assert_eq!(method(&cfg).await.unwrap(), AuthMethod::Integrated);
        // No user or password travels with the current user's login.
        let c = build_config(&cfg, None, Variant::SqlServer, Login::Integrated).unwrap();
        assert!(!format!("{c:?}").contains("s3cret"));

        cfg.options.insert("auth".into(), variant::WINDOWS_NTLM.into());
        assert_eq!(method(&cfg).await.unwrap(), AuthMethod::windows("CONTOSO\\ana", "s3cret"));
    }

    #[cfg(unix)]
    #[test]
    fn kerberos_failures_say_what_to_do() {
        let e = connect_error(tiberius::error::Error::Gssapi(
            "gss_acquire_cred: No Kerberos credentials available (default cache: API:1234)".into(),
        ));
        assert!(matches!(&e, Error::AuthFailed(m) if m.contains("no hay un ticket de Kerberos vigente") && m.contains("kinit usuario@DOMINIO")), "{e:?}");
        let e = connect_error(tiberius::error::Error::Gssapi("Server not found in Kerberos database".into()));
        assert!(matches!(&e, Error::AuthFailed(m) if m.contains("MSSQLSvc")), "{e:?}");
    }

    #[test]
    fn host_forms() {
        assert_eq!(parse_host("srv\\SQLEXPRESS", 1433), ("srv", Some("SQLEXPRESS"), 1433));
        assert_eq!(parse_host("srv,14330", 1433), ("srv", None, 14330));
        assert_eq!(parse_host("", 1433), ("localhost", None, 1433));
    }

    fn db(name: &str, id: i32, access: Option<i32>) -> DatabaseRow {
        DatabaseRow { name: name.into(), id, access }
    }

    #[test]
    fn sql_server_lists_what_the_login_may_open() {
        for edition in [1, 2, 3, 4, 8, 9] {
            assert!(!database_scoped(Variant::SqlServer, edition), "{edition}");
        }
        // Sorted by name, as the query returns them.
        let rows = vec![db("master", 1, Some(1)), db("model", 3, Some(1)), db("sales", 7, Some(1)), db("secret", 6, Some(0)), db("tempdb", 2, Some(1))];
        assert_eq!(visible_databases(rows, false), ["sales", "master", "model", "tempdb"]);
    }

    #[test]
    fn azure_sql_database_lists_every_database_from_master() {
        // Connected to master as the server admin: HAS_DBACCESS is 0 or NULL
        // for every other database.
        for v in [Variant::SqlServer, Variant::AzureSql] {
            for edition in [5, 6, 11, 12] {
                assert!(database_scoped(v, edition), "{v:?} {edition}");
            }
        }
        let rows = vec![db("app", 3, Some(0)), db("Hyper", 4, None), db("master", 1, Some(1)), db("reports", 5, None)];
        assert_eq!(visible_databases(rows, true), ["app", "Hyper", "reports", "master"]);
    }

    #[test]
    fn fabric_warehouse_keeps_the_access_check() {
        assert!(!database_scoped(Variant::Fabric, 11));
        assert!(!database_scoped(Variant::Fabric, 12));
    }

    #[test]
    fn browse_uses_top_and_brackets() {
        let s = SqlServerDriver { info: variant::info(Variant::SqlServer), variant: Variant::SqlServer };
        assert_eq!(s.info().default_port, 1433);
        let q = select_top(Quote::Bracket, Limit::Top, Some("dbo"), "t", 5);
        assert_eq!(q, "SELECT TOP (5) *\nFROM [dbo].[t]");
    }
}
