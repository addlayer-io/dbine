//! PostgreSQL and the engines that speak its wire protocol, through
//! tokio-postgres: forks and distributions (Greenplum, Apache Cloudberry,
//! Greengage, EDB Postgres Advanced Server, Fujitsu Enterprise Postgres,
//! KingbaseES, openGauss, TimescaleDB, YugabyteDB), managed services
//! (AlloyDB, Cloud SQL, Aurora PostgreSQL, Redshift) and engines of their
//! own that took the protocol (CockroachDB, RisingWave, Materialize,
//! CrateDB, Yellowbrick, Denodo, H2 in PG server mode). Scripts run over the simple query
//! protocol, so every cell comes back as text and multi-statement scripts
//! keep their per-statement framing. The variants share the session; what
//! changes is the catalog they can be asked about (see [`Variant`]).
//!
//! openGauss only logs in when the server stores MD5 passwords
//! (`password_encryption_type` 0 or 1 and `md5` in `pg_hba.conf`): its
//! default SHA-256 / SM3 methods aren't PostgreSQL's and tokio-postgres
//! can't answer them. Huawei's managed GaussDB, which only offers those,
//! isn't a variant.

mod backup;
mod blocking;
mod catalog;
mod clone;
mod compare;
mod delta;
mod design;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod profiler;
mod schemas;
mod script;
mod security;
mod session;
mod structure;
mod transfer;

use dbine_driver::{
    async_trait, Capabilities, ConnectionConfig, CreateTemplate, DdlParts, DesignerSpec, Driver, DriverInfo, Error,
    Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, Result, Session, TableSchema,
};
use postgres_native_tls::MakeTlsConnector;
use session::PgSession;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_postgres::error::SqlState;
use tokio_postgres::AsyncMessage;

/// Object kinds of the streaming engines besides tables and views.
pub(crate) const SOURCE: &str = "source";
pub(crate) const SINK: &str = "sink";

/// A wire-compatible engine. Most of them keep PostgreSQL's catalog; the
/// ones that don't get their own queries (Redshift) or fall back to
/// `information_schema` (Denodo).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    Postgres,
    Cockroach,
    Redshift,
    Greenplum,
    Yugabyte,
    Timescale,
    Kingbase,
    Denodo,
    AlloyDb,
    CloudSql,
    Aurora,
    Edb,
    Fujitsu,
    OpenGauss,
    Cloudberry,
    Greengage,
    RisingWave,
    Materialize,
    CrateDb,
    Yellowbrick,
    H2,
}

impl Variant {
    pub(crate) const ALL: [Variant; 21] = [
        Variant::Postgres,
        Variant::Cockroach,
        Variant::Redshift,
        Variant::Greenplum,
        Variant::Yugabyte,
        Variant::Timescale,
        Variant::Kingbase,
        Variant::Denodo,
        Variant::AlloyDb,
        Variant::CloudSql,
        Variant::Aurora,
        Variant::Edb,
        Variant::Fujitsu,
        Variant::OpenGauss,
        Variant::Cloudberry,
        Variant::Greengage,
        Variant::RisingWave,
        Variant::Materialize,
        Variant::CrateDb,
        Variant::Yellowbrick,
        Variant::H2,
    ];

    fn default_port(self) -> u16 {
        match self {
            Variant::Cockroach => 26257,
            Variant::Redshift => 5439,
            Variant::Yugabyte => 5433,
            Variant::Kingbase => 54321,
            Variant::Denodo => 9996,
            Variant::Edb => 5444,
            Variant::Fujitsu => 27500,
            Variant::RisingWave => 4566,
            Variant::Materialize => 6875,
            Variant::H2 => 5435,
            _ => 5432,
        }
    }

    /// Database to log in to when the connection names none.
    fn default_database(self) -> &'static str {
        match self {
            Variant::Cockroach => "defaultdb",
            Variant::Redshift | Variant::RisingWave => "dev",
            Variant::Yugabyte => "yugabyte",
            Variant::Kingbase => "test",
            Variant::Denodo => "admin",
            Variant::Edb => "edb",
            Variant::Materialize => "materialize",
            Variant::CrateDb => "doc",
            Variant::Yellowbrick => "yellowbrick",
            Variant::H2 => "test",
            _ => "postgres",
        }
    }

    /// User to log in as when the connection names none.
    fn default_user(self) -> Option<&'static str> {
        match self {
            Variant::Cockroach | Variant::RisingWave => Some("root"),
            Variant::Materialize => Some("materialize"),
            Variant::CrateDb => Some("crate"),
            Variant::Edb => Some("enterprisedb"),
            Variant::H2 => Some("sa"),
            _ => None,
        }
    }

    /// Engines whose `pg_catalog` is PostgreSQL's (give or take).
    pub(crate) fn has_pg_catalog(self) -> bool {
        !matches!(self, Variant::Redshift | Variant::Denodo | Variant::H2)
    }

    /// Engines that only answer through `information_schema` (their
    /// `pg_catalog`, if any, is too thin for the explorer).
    pub(crate) fn info_schema_only(self) -> bool {
        matches!(self, Variant::Denodo | Variant::H2)
    }

    /// Greenplum and its forks: MPP on a coordinator and segments.
    pub(crate) fn mpp(self) -> bool {
        matches!(self, Variant::Greenplum | Variant::Cloudberry | Variant::Greengage)
    }

    /// Managed PostgreSQL: the engine is PostgreSQL's, the host isn't ours.
    pub(crate) fn managed(self) -> bool {
        matches!(self, Variant::AlloyDb | Variant::CloudSql | Variant::Aurora)
    }

    /// Streaming databases: sources, sinks and incrementally maintained
    /// materialized views instead of procedures and triggers.
    pub(crate) fn streaming(self) -> bool {
        matches!(self, Variant::RisingWave | Variant::Materialize)
    }

    /// PostgreSQL itself or a distribution that keeps its server-side
    /// language (PL/pgSQL `DO` blocks, procedures, triggers).
    pub(crate) fn plpgsql(self) -> bool {
        matches!(
            self,
            Variant::Postgres
                | Variant::Timescale
                | Variant::Yugabyte
                | Variant::Kingbase
                | Variant::AlloyDb
                | Variant::CloudSql
                | Variant::Aurora
                | Variant::Edb
                | Variant::Fujitsu
                | Variant::OpenGauss
        )
    }

    /// `EXPLAIN ANALYZE` (actual figures) on a query.
    pub(crate) fn can_analyze(self) -> bool {
        !matches!(
            self,
            Variant::Redshift | Variant::RisingWave | Variant::Materialize | Variant::CrateDb | Variant::Denodo | Variant::H2
        )
    }

    /// `EXPLAIN` takes INSERT / UPDATE / DELETE.
    pub(crate) fn explains_writes(self) -> bool {
        !matches!(self, Variant::Materialize | Variant::CrateDb)
    }

    pub(crate) fn info(self) -> DriverInfo {
        let (id, name, family) = match self {
            Variant::Postgres => ("postgres", "PostgreSQL", Family::Relational),
            Variant::Cockroach => ("cockroachdb", "CockroachDB", Family::Relational),
            Variant::Redshift => ("redshift", "Amazon Redshift", Family::Analytical),
            Variant::Greenplum => ("greenplum", "Greenplum", Family::Analytical),
            Variant::Yugabyte => ("yugabytedb", "YugabyteDB", Family::Relational),
            Variant::Timescale => ("timescaledb", "TimescaleDB", Family::TimeSeries),
            Variant::Kingbase => ("kingbase", "KingbaseES", Family::Relational),
            Variant::Denodo => ("denodo", "Denodo", Family::Relational),
            Variant::AlloyDb => ("alloydb", "AlloyDB para PostgreSQL", Family::Relational),
            Variant::CloudSql => ("cloudsql_postgres", "Cloud SQL para PostgreSQL", Family::Relational),
            Variant::Aurora => ("aurora_postgres", "Amazon Aurora PostgreSQL", Family::Relational),
            Variant::Edb => ("edb", "EDB Postgres Advanced Server", Family::Relational),
            Variant::Fujitsu => ("fujitsu", "Fujitsu Enterprise Postgres", Family::Relational),
            Variant::OpenGauss => ("opengauss", "openGauss", Family::Relational),
            Variant::Cloudberry => ("cloudberry", "Apache Cloudberry", Family::Analytical),
            Variant::Greengage => ("greengage", "Greengage DB", Family::Analytical),
            Variant::RisingWave => ("risingwave", "RisingWave", Family::Streaming),
            Variant::Materialize => ("materialize", "Materialize", Family::Streaming),
            Variant::CrateDb => ("cratedb", "CrateDB", Family::Analytical),
            Variant::Yellowbrick => ("yellowbrick", "Yellowbrick", Family::Analytical),
            Variant::H2 => ("h2", "H2 (servidor PostgreSQL)", Family::Relational),
        };
        let mut object_kinds = match self {
            Variant::Redshift => vec![
                ObjectKindInfo::tables(),
                ObjectKindInfo::views(),
                ObjectKindInfo::procedures(),
                ObjectKindInfo::functions(),
            ],
            Variant::Denodo | Variant::CrateDb | Variant::H2 => vec![ObjectKindInfo::tables(), ObjectKindInfo::views()],
            Variant::RisingWave | Variant::Materialize => vec![
                ObjectKindInfo::tables(),
                ObjectKindInfo::views(),
                ObjectKindInfo::materialized_views(),
                ObjectKindInfo::new(SOURCE, "Fuentes", true, true, true),
                ObjectKindInfo::new(SINK, "Sinks", false, false, true),
            ],
            Variant::Yellowbrick => vec![ObjectKindInfo::tables(), ObjectKindInfo::views(), ObjectKindInfo::functions()],
            _ => vec![
                ObjectKindInfo::tables(),
                ObjectKindInfo::views(),
                ObjectKindInfo::materialized_views(),
                ObjectKindInfo::procedures(),
                ObjectKindInfo::functions(),
                ObjectKindInfo::triggers(),
            ],
        };
        object_kinds.extend(compare::object_kinds(self));
        DriverInfo {
            id,
            name,
            family,
            language: Language::Sql,
            dialect: "postgres",
            default_port: self.default_port(),
            fields: self.fields(),
            databases_label: if matches!(self, Variant::CrateDb | Variant::H2) { "" } else { "Bases de datos" },
            has_schemas: self != Variant::Denodo,
            object_kinds,
        }
    }

    /// The server form, plus the CA certificate; managed services start
    /// with TLS on (they ask for it, or should).
    fn fields(self) -> Vec<Field> {
        let mut fields = Field::server_set();
        // H2's PostgreSQL server has no TLS.
        if self == Variant::H2 {
            fields.retain(|f| !matches!(f.key, "encrypt" | "trust_server_certificate"));
            for f in fields.iter_mut().filter(|f| f.key == "database") {
                *f = f.clone().help("La base de H2: un nombre bajo el directorio base del servidor (-baseDir), como «test».");
            }
            return fields;
        }
        if self == Variant::Denodo {
            return fields;
        }
        if self.managed() || self == Variant::Redshift {
            for f in fields.iter_mut().filter(|f| f.key == "encrypt") {
                *f = f.clone().default_value("true");
            }
        }
        let help = match self {
            Variant::Aurora | Variant::Redshift => "Para verificar el servidor: el global-bundle.pem de AWS (RDS).",
            Variant::CloudSql => "Para verificar el servidor: el server-ca.pem de la instancia (consola de Cloud SQL).",
            Variant::AlloyDb => "Para verificar el servidor: la CA del clúster (gcloud alloydb clusters describe).",
            _ => "Opcional: la CA que firmó el certificado del servidor, en PEM.",
        };
        let at = fields.iter().position(|f| f.key == "trust_server_certificate").map_or(fields.len(), |i| i + 1);
        fields.insert(at, Field::new("ssl_root_cert", "Certificado de CA (PEM)", FieldKind::File).help(help).ssl());
        if self.managed() {
            for f in fields.iter_mut().filter(|f| f.key == "host") {
                *f = f.clone().help(match self {
                    Variant::Aurora => "El endpoint del clúster (escritor) o el de lectura.",
                    Variant::CloudSql => "La IP de la instancia, o 127.0.0.1 si usás Cloud SQL Auth Proxy.",
                    _ => "La IP de la instancia, o 127.0.0.1 si usás AlloyDB Auth Proxy.",
                });
            }
        }
        fields
    }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    Variant::ALL.iter().map(|&v| Arc::new(PgDriver { variant: v, info: v.info() }) as Arc<dyn Driver>).collect()
}

pub struct PgDriver {
    variant: Variant,
    info: DriverInfo,
}

#[async_trait]
impl Driver for PgDriver {
    /// Loading explicit ids leaves identity / serial sequences behind the
    /// data: move each past the loaded maximum.
    fn data_load_wrap(&self, table: &dbine_driver::TableSchema) -> (String, String) {
        // Engines without sequences behind their columns.
        let v = self.variant;
        if matches!(v, Variant::Redshift | Variant::Denodo | Variant::CrateDb | Variant::H2) || v.streaming() {
            return (String::new(), String::new());
        }
        let q = |s: &str| format!("\"{}\"", s.replace('"', "\"\""));
        let name = match table.schema.as_deref().filter(|s| !s.is_empty()) {
            Some(s) => format!("{}.{}", q(s), q(&table.name)),
            None => q(&table.name),
        };
        let after: Vec<String> = table
            .columns
            .iter()
            .filter(|c| c.auto_increment)
            .map(|c| {
                format!(
                    "SELECT setval(pg_get_serial_sequence('{}', '{}'), COALESCE((SELECT max({}) FROM {name}), 0) + 1, false);",
                    name.replace('\'', "''"),
                    c.name.replace('\'', "''"),
                    q(&c.name)
                )
            })
            .collect();
        (String::new(), after.join("\n"))
    }

    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn script_dialect(&self) -> dbine_driver::sql::ScriptDialect {
        script::DIALECT
    }

    /// The lexer's units, plus what psql reads outside SQL: meta-command
    /// lines and `COPY … FROM stdin` data (see [`script::split`]).
    fn split_script(&self, text: &str) -> Vec<dbine_driver::sql::ScriptStatement> {
        script::split(text)
    }

    /// One statement per simple query, as psql sends them; the session
    /// (SET, temp tables, an open transaction) carries over.
    fn script_mode(&self) -> dbine_driver::sql::ScriptMode {
        dbine_driver::sql::ScriptMode::PerStatement
    }

    fn script_defaults(&self) -> dbine_driver::sql::ScriptDefaults {
        dbine_driver::sql::ScriptDefaults { continue_on_error: self.variant.continue_on_error(), confirm_unsafe_dml: true }
    }

    fn supports_manual_transactions(&self) -> bool {
        self.variant.manual_transactions()
    }

    /// `COPY … FROM STDIN` (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        transfer::bulk_capable(self.variant)
    }

    fn supports_native_copy(&self, target: &str) -> bool {
        Variant::ALL.iter().any(|t| t.info().id == target && transfer::native_capable(self.variant, *t))
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

    /// Variants that keep PostgreSQL's catalog and DDL (see `clone`).
    fn supports_clone(&self) -> bool {
        clone::capable(self.variant)
    }

    async fn clone_script(
        &self,
        source: &mut dyn Session,
        target: &mut dyn Session,
        tables: &[ObjectRef],
    ) -> Result<dbine_driver::CloneScript> {
        clone::script(self.variant, source, target, tables).await
    }

    fn supports_delta(&self) -> bool {
        delta::capable(self.variant)
    }

    fn delta_filter(&self, spec: &dbine_driver::DeltaSpec, buckets: &[i64]) -> Result<String> {
        if !delta::capable(self.variant) {
            return Err(Error::Unsupported(format!("{} no sincroniza por filas", self.info.name)));
        }
        delta::filter(spec, buckets)
    }

    /// Every variant but Denodo, which has no EXPLAIN.
    fn supports_explain(&self) -> bool {
        self.variant != Variant::Denodo
    }

    fn supports_profiler(&self) -> bool {
        profiler::supported(self.variant)
    }

    /// Every variant but Denodo (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        index_usage::supported(self.variant)
    }

    /// CockroachDB only: `ALTER INDEX … NOT VISIBLE` (see `index_usage`).
    fn supports_index_toggle(&self) -> bool {
        index_usage::toggle_supported(self.variant)
    }

    fn index_toggle_script(&self, table: &ObjectRef, index: &dbine_driver::IndexUsage, enable: bool) -> Result<dbine_driver::SyncScript> {
        index_usage::toggle_script(self.variant, table, index, enable)
    }

    fn capabilities(&self) -> Capabilities {
        design::capabilities(self.variant)
    }

    fn designer(&self) -> Option<DesignerSpec> {
        design::designer(self.variant)
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        design::create_templates(self.variant)
    }

    fn supports_schema_sync(&self) -> bool {
        self.variant != Variant::Denodo
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        if self.variant == Variant::Denodo {
            return Err(Error::Unsupported("Denodo no modifica tablas por SQL: las vistas base se definen en Denodo".into()));
        }
        design::sync_script(self.variant, changes)
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        security::spec(self.variant)
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(self.variant, action)
    }

    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        schemas::spec(self.variant)
    }

    fn create_schema_script(&self, _database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        schemas::create_script(self.variant, name, owner)
    }

    fn schema_owner_script(&self, _database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        schemas::owner_script(self.variant, name, owner)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        schemas::drop_script(self.variant, name, cascade)
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        backup::spec(self.variant)
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(self.variant, action)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        if self.variant == Variant::Denodo {
            return Err(Error::Unsupported("Denodo no crea tablas por SQL: las vistas base se definen en Denodo".into()));
        }
        Ok(design::table_ddl(self.variant, table, parts))
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        Ok(design::insert_script(self.variant, target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        Ok(design::update_script(self.variant, target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        Ok(design::delete_script(self.variant, target.schema(), &target.name, keys))
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let v = self.variant;
        let dbname = database
            .filter(|d| !d.is_empty())
            .or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()))
            .unwrap_or(v.default_database())
            .to_string();

        let mut pg = tokio_postgres::Config::new();
        pg.host(if cfg.host.is_empty() { "localhost" } else { &cfg.host })
            .port(cfg.port_or(v.default_port()))
            .dbname(&dbname)
            .connect_timeout(Duration::from_secs(15))
            .ssl_mode(if cfg.encrypt {
                tokio_postgres::config::SslMode::Require
            } else {
                tokio_postgres::config::SslMode::Prefer
            });
        if v != Variant::Denodo {
            pg.application_name("DBine");
        }
        if let Some(u) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
            pg.user(u);
        } else if let Some(u) = v.default_user() {
            pg.user(u);
        }
        if let Some(p) = &cfg.password {
            pg.password(p.as_str());
        }

        let tls = MakeTlsConnector::new(tls_connector(v, cfg)?);

        let (client, mut connection) = tokio::time::timeout(Duration::from_secs(20), pg.connect(tls.clone()))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(|e| connect_error(v, e))?;

        // Drive the connection, forwarding notices (RAISE NOTICE, warnings…).
        let (tx, notices) = mpsc::unbounded_channel();
        tokio::spawn(async move {
            loop {
                match futures::future::poll_fn(|cx| connection.poll_message(cx)).await {
                    Some(Ok(AsyncMessage::Notice(n))) => {
                        let _ = tx.send(n);
                    }
                    Some(Ok(_)) => {}
                    Some(Err(e)) => {
                        tracing::debug!("postgres connection closed: {e}");
                        break;
                    }
                    None => break,
                }
            }
        });

        if cfg.read_only {
            // Server-side guard on top of the ReadOnlySession wrapper; not
            // every variant takes it, and the wrapper still applies.
            let set = match v {
                Variant::Cockroach => "SET default_transaction_read_only = on",
                _ => "SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY",
            };
            if let Err(e) = client.batch_execute(set).await {
                tracing::debug!("{}: read-only session setting refused: {e}", self.info.id);
            }
        }
        // `server_version_num` picks catalog columns (prokind, attidentity);
        // engines that don't report it get the oldest queries.
        let version = if v.has_pg_catalog() {
            match client.simple_query("SELECT current_setting('server_version_num')").await {
                Ok(msgs) => catalog::first_cell(&msgs).and_then(|s| s.parse().ok()).unwrap_or(0),
                Err(_) => 0,
            }
        } else {
            0
        };

        Ok(Box::new(PgSession::new(client, tls, v, version, dbname, notices)))
    }
}

/// TLS as the form asks: the CA file, when given, is trusted on top of the
/// system's. Cloud SQL and AlloyDB certificates name the instance, not the
/// IP the client dials, so with their CA only the chain is checked.
fn tls_connector(v: Variant, cfg: &ConnectionConfig) -> Result<native_tls::TlsConnector> {
    let mut b = native_tls::TlsConnector::builder();
    b.danger_accept_invalid_certs(cfg.trust_server_certificate)
        .danger_accept_invalid_hostnames(cfg.trust_server_certificate);
    if let Some(path) = cfg.option("ssl_root_cert").map(str::trim).filter(|p| !p.is_empty()) {
        let pem = std::fs::read(path).map_err(|e| Error::Connect(format!("no se pudo leer el certificado de CA «{path}»: {e}")))?;
        let certs = native_tls::Certificate::from_pem(&pem)
            .map_err(|e| Error::Connect(format!("el certificado de CA «{path}» no es un PEM válido: {e}")))?;
        b.add_root_certificate(certs);
        if matches!(v, Variant::CloudSql | Variant::AlloyDb) {
            b.danger_accept_invalid_hostnames(true);
        }
    }
    b.build().map_err(|e| Error::Connect(format!("TLS: {e}")))
}

fn connect_error(v: Variant, e: tokio_postgres::Error) -> Error {
    // openGauss answers a SHA-256 / SM3 login with an authentication code
    // that PostgreSQL uses for SASL, and the client can't parse it.
    if v == Variant::OpenGauss && e.code().is_none() && e.to_string().contains("sasl") {
        return Error::AuthFailed(
            "openGauss pidió autenticación SHA-256 o SM3, que el cliente de PostgreSQL no soporta. \
             Para este usuario, configurá password_encryption_type = 1 (o 0), método md5 en pg_hba.conf \
             y volvé a asignar la contraseña."
                .into(),
        );
    }
    match e.code() {
        Some(c) if *c == SqlState::INVALID_PASSWORD || *c == SqlState::INVALID_AUTHORIZATION_SPECIFICATION => {
            Error::AuthFailed(db_message(&e))
        }
        Some(_) => Error::Connect(db_message(&e)),
        None => Error::Connect(e.to_string()),
    }
}

/// A statement's failure: the server's message (with detail and hint when
/// it gives them); a cancelled statement reads as a cancellation.
pub(crate) fn err(e: tokio_postgres::Error) -> Error {
    if e.code() == Some(&SqlState::QUERY_CANCELED) {
        return Error::Cancelled;
    }
    if e.as_db_error().is_some() {
        Error::Query(db_message(&e))
    } else if e.is_closed() {
        Error::Connect("se cerró la conexión con el servidor".into())
    } else {
        Error::Query(e.to_string())
    }
}

fn db_message(e: &tokio_postgres::Error) -> String {
    match e.as_db_error() {
        Some(db) => db_text(db),
        None => e.to_string(),
    }
}

/// "SEVERITY: message", then the detail and the hint when there are.
pub(crate) fn db_text(db: &tokio_postgres::error::DbError) -> String {
    let mut m = format!("{}: {}", db.severity(), db.message());
    if let Some(d) = db.detail() {
        m.push_str(&format!("\nDetalle: {d}"));
    }
    if let Some(h) = db.hint() {
        m.push_str(&format!("\nSugerencia: {h}"));
    }
    m
}

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
    fn ids_are_unique_and_ports_known() {
        let ds = drivers();
        let mut ids: Vec<_> = ds.iter().map(|d| d.info().id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), ds.len());
        let port = |id: &str| ds.iter().find(|d| d.info().id == id).unwrap().info().default_port;
        assert_eq!(port("postgres"), 5432);
        assert_eq!(port("cockroachdb"), 26257);
        assert_eq!(port("redshift"), 5439);
        assert_eq!(port("yugabytedb"), 5433);
        assert_eq!(port("kingbase"), 54321);
        assert_eq!(port("denodo"), 9996);
        assert_eq!(port("edb"), 5444);
        assert_eq!(port("fujitsu"), 27500);
        assert_eq!(port("risingwave"), 4566);
        assert_eq!(port("materialize"), 6875);
        assert_eq!(port("cratedb"), 5432);
        assert_eq!(port("h2"), 5435);
    }

    #[test]
    fn managed_services_start_with_tls_and_take_a_ca() {
        for v in Variant::ALL.into_iter().filter(|v| *v != Variant::H2) {
            let info = v.info();
            let encrypt = info.fields.iter().find(|f| f.key == "encrypt").unwrap();
            assert_eq!(encrypt.default == "true", v.managed() || v == Variant::Redshift, "{v:?}");
            assert_eq!(info.fields.iter().any(|f| f.key == "ssl_root_cert"), !v.info_schema_only(), "{v:?}");
        }
    }

    #[test]
    fn a_missing_ca_file_is_a_connect_error() {
        let mut cfg = ConnectionConfig::default();
        cfg.options.insert("ssl_root_cert".into(), "/nonexistent/ca.pem".into());
        assert!(matches!(tls_connector(Variant::Aurora, &cfg), Err(Error::Connect(_))));
        assert!(tls_connector(Variant::Aurora, &ConnectionConfig::default()).is_ok());
    }
}
