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
pub(crate) mod cloud_auth;
mod compare;
mod create_db;
mod delta;
mod design;
mod health;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod properties;
mod rename;
mod rustls_connect;
mod schemas;
mod script;
mod search;
mod security;
mod session;
mod stats;
mod structure;
mod transfer;

use dbine_driver::{
    async_trait, Capabilities, ConnectionConfig, CreateTemplate, DdlParts, DesignerSpec, Driver,
    DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, Result,
    Session, TableSchema, TlsLevel, TlsSupport,
};
use rustls_connect::MakeRustlsConnect;
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
        matches!(
            self,
            Variant::Greenplum | Variant::Cloudberry | Variant::Greengage
        )
    }

    /// The cloud sign-ins the form offers: PostgreSQL itself (RDS, Azure
    /// Database for PostgreSQL, Cloud SQL by IP) gets the three; each
    /// managed variant, its own cloud.
    pub(crate) fn cloud_offer(self) -> cloud_auth::Offer {
        match self {
            Variant::Postgres => cloud_auth::Offer {
                aws: true,
                gcp: true,
                azure: true,
            },
            Variant::Aurora => cloud_auth::Offer {
                aws: true,
                ..Default::default()
            },
            Variant::CloudSql | Variant::AlloyDb => cloud_auth::Offer {
                gcp: true,
                ..Default::default()
            },
            _ => cloud_auth::Offer::default(),
        }
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

    /// [`dbine_driver::Session::run_read_only`]: the engine has read-only
    /// transactions the server enforces (`START TRANSACTION READ ONLY`,
    /// `transaction_read_only`) and refuses a second statement in a Parse
    /// message. Before each read the session also checks that the
    /// transaction it opened is read-only; when the engine can't say so,
    /// the read answers `Unsupported` (the app asks for approval).
    ///
    /// Left out:
    /// - EDB, KingbaseES and openGauss: their Oracle-compatible
    ///   autonomous transactions (`PRAGMA AUTONOMOUS_TRANSACTION`) commit
    ///   on their own, outside the read-only transaction, so a function
    ///   the statement calls could write.
    /// - Denodo, RisingWave, Materialize, CrateDB and H2: no read-only
    ///   transaction mode the server enforces (or none at all).
    pub(crate) fn enforces_read_only(self) -> bool {
        matches!(
            self,
            Variant::Postgres
                | Variant::Cockroach
                | Variant::Redshift
                | Variant::Greenplum
                | Variant::Yugabyte
                | Variant::Timescale
                | Variant::AlloyDb
                | Variant::CloudSql
                | Variant::Aurora
                | Variant::Fujitsu
                | Variant::Cloudberry
                | Variant::Greengage
                | Variant::Yellowbrick
        )
    }

    /// `EXPLAIN ANALYZE` (actual figures) on a query.
    pub(crate) fn can_analyze(self) -> bool {
        !matches!(
            self,
            Variant::Redshift
                | Variant::RisingWave
                | Variant::Materialize
                | Variant::CrateDb
                | Variant::Denodo
                | Variant::H2
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
            Variant::CloudSql => (
                "cloudsql_postgres",
                "Cloud SQL para PostgreSQL",
                Family::Relational,
            ),
            Variant::Aurora => (
                "aurora_postgres",
                "Amazon Aurora PostgreSQL",
                Family::Relational,
            ),
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
            Variant::Denodo | Variant::CrateDb | Variant::H2 => {
                vec![ObjectKindInfo::tables(), ObjectKindInfo::views()]
            }
            Variant::RisingWave | Variant::Materialize => vec![
                ObjectKindInfo::tables(),
                ObjectKindInfo::views(),
                ObjectKindInfo::materialized_views(),
                ObjectKindInfo::new(SOURCE, "Fuentes", true, true, true),
                ObjectKindInfo::new(SINK, "Sinks", false, false, true),
            ],
            Variant::Yellowbrick => vec![
                ObjectKindInfo::tables(),
                ObjectKindInfo::views(),
                ObjectKindInfo::functions(),
            ],
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
            // H2's PostgreSQL server has no TLS.
            tls: if self == Variant::H2 {
                TlsSupport::None
            } else {
                TlsSupport::Configurable
            },
            tls_port: None,
            id,
            name,
            family,
            language: Language::Sql,
            dialect: "postgres",
            default_port: self.default_port(),
            fields: self.fields(),
            databases_label: if matches!(self, Variant::CrateDb | Variant::H2) {
                ""
            } else {
                "Bases de datos"
            },
            has_schemas: self != Variant::Denodo,
            object_kinds,
        }
    }

    /// libpq's `sslmode`, with its values. New connections verify the
    /// certificate: chain and name (`verify-full`), or only the chain on
    /// Cloud SQL and AlloyDB, whose certificates name the instance, not the
    /// address dialed. A connection saved before the option reads it from
    /// its `encrypt` / `trust_server_certificate` pair (see [`tls_plan`]).
    /// H2 has no TLS.
    pub(crate) fn sslmode_field(self) -> Option<Field> {
        if self == Variant::H2 {
            return None;
        }
        let instance_ca = matches!(self, Variant::CloudSql | Variant::AlloyDb);
        let verified = if instance_ca {
            "verify-ca"
        } else {
            "verify-full"
        };
        let f = Field::tls_mode(
            SSLMODE,
            SSLMODE,
            &[
                ("disable", "disable (sin TLS)", TlsLevel::Off),
                (
                    "prefer",
                    "prefer (TLS si el servidor lo ofrece, sin verificar)",
                    TlsLevel::Optional,
                ),
                (
                    "require",
                    "require (TLS sin verificar el certificado)",
                    TlsLevel::Unverified,
                ),
                (
                    "verify-ca",
                    "verify-ca (verifica la CA, no el nombre)",
                    TlsLevel::VerifyCa,
                ),
                (
                    "verify-full",
                    "verify-full (verifica la CA y el nombre)",
                    TlsLevel::VerifyFull,
                ),
            ],
            verified,
            ["prefer", "prefer", verified, "require"],
        );
        Some(if instance_ca {
            f.help(
                "Su certificado nombra la instancia, no la IP: verify-ca con la CA de la instancia en sslrootcert. \
                 Con el Auth Proxy en 127.0.0.1, disable: el proxy ya cifra la conexión.",
            )
        } else {
            f
        })
    }

    /// The server form, with libpq's TLS options.
    fn fields(self) -> Vec<Field> {
        let mut fields = self.server_fields();
        fields.push(
            Field::new(
                PROTOCOL_OPTION,
                "Protocolo de consultas",
                FieldKind::Select(vec![("auto", "Automático"), ("simple", "Solo protocolo simple")]),
            )
            .default_value("auto")
            .help(
                "Automático usa el protocolo extendido y pasa al simple si el servidor no lo acepta. \
                 «Solo protocolo simple» es para gateways y proxies que rechazan el extendido.",
            )
            .advanced(),
        );
        fields
    }

    fn server_fields(self) -> Vec<Field> {
        let mut fields = vec![
            Field::host(),
            Field::port(),
            Field::database(),
            Field::username(),
            Field::password(),
        ];
        // H2's PostgreSQL server has no TLS.
        if self == Variant::H2 {
            fields[2] = Field::database().help(
                "La base de H2: un nombre bajo el directorio base del servidor (-baseDir), como «test».",
            );
            fields.push(Field::read_only());
            return fields;
        }
        fields.extend(self.sslmode_field());
        let help = match self {
            Variant::Aurora | Variant::Redshift => "Para verificar el servidor: el global-bundle.pem de AWS (RDS).",
            Variant::CloudSql => "Para verificar el servidor: el server-ca.pem de la instancia (consola de Cloud SQL).",
            Variant::AlloyDb => "Para verificar el servidor: la CA del clúster (gcloud alloydb clusters describe).",
            _ => "La CA que firmó el certificado del servidor, en PEM: obligatoria con verify-ca.",
        };
        fields.push(Field::tls_ca_file(SSL_ROOT_CERT, "sslrootcert").help(help));
        fields.push(Field::read_only());
        if self.managed() {
            for f in fields.iter_mut().filter(|f| f.key == "host") {
                *f = f.clone().help(match self {
                    Variant::Aurora => "El endpoint del clúster (escritor) o el de lectura.",
                    Variant::CloudSql => {
                        "La IP de la instancia, o 127.0.0.1 si usás Cloud SQL Auth Proxy."
                    }
                    _ => "La IP de la instancia, o 127.0.0.1 si usás AlloyDB Auth Proxy.",
                });
            }
        }
        cloud_auth::adapt(&mut fields, self.cloud_offer());
        fields
    }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    Variant::ALL
        .iter()
        .map(|&v| {
            Arc::new(PgDriver {
                variant: v,
                info: v.info(),
            }) as Arc<dyn Driver>
        })
        .collect()
}

pub struct PgDriver {
    variant: Variant,
    info: DriverInfo,
}

#[async_trait]
impl Driver for PgDriver {
    /// Other databases as `db.table` / `db.schema.table` in one query.
    fn cross_database(&self) -> bool {
        matches!(self.info.id, "redshift" | "cockroachdb" | "materialize")
    }

    /// Text the way this variant reads it (`E'…'` where
    /// `standard_conforming_strings` may be off, Redshift's backslashes);
    /// the rest as the contract writes it. Values go as literals: the
    /// session reads results over the simple protocol, which takes no
    /// bound values (docs/engine-support.md).
    fn param_literal(&self, value: &dbine_driver::ParamValue) -> dbine_driver::Result<String> {
        match value {
            dbine_driver::ParamValue::Text(s) => Ok(crate::catalog::lit(self.variant, s)),
            other => dbine_driver::params::literal(
                self.info.language,
                self.info.dialect,
                self.info.id,
                other,
            ),
        }
    }

    /// Loading explicit ids leaves identity / serial sequences behind the
    /// data: move each past the loaded maximum.
    fn data_load_wrap(&self, table: &dbine_driver::TableSchema) -> (String, String) {
        // Engines without sequences behind their columns.
        let v = self.variant;
        if matches!(
            v,
            Variant::Redshift | Variant::Denodo | Variant::CrateDb | Variant::H2
        ) || v.streaming()
        {
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
                    "SELECT setval(pg_get_serial_sequence({}, {}), COALESCE((SELECT max({}) FROM {name}), 0) + 1, false);",
                    catalog::lit(v, &name),
                    catalog::lit(v, &c.name),
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
        dbine_driver::sql::ScriptDefaults {
            continue_on_error: self.variant.continue_on_error(),
            confirm_unsafe_dml: true,
        }
    }

    fn supports_manual_transactions(&self) -> bool {
        self.variant.manual_transactions()
    }

    /// `COPY … FROM STDIN` (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        transfer::bulk_capable(self.variant)
    }

    fn supports_native_copy(&self, target: &str) -> bool {
        Variant::ALL
            .iter()
            .any(|t| t.info().id == target && transfer::native_capable(self.variant, *t))
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

    /// Notebook writes stage through `COPY` into a temporary table: only
    /// where `COPY FROM STDIN` is.
    fn write_modes(&self) -> Vec<dbine_driver::WriteMode> {
        if transfer::bulk_capable(self.variant) {
            dbine_driver::WriteMode::ALL.to_vec()
        } else {
            Vec::new()
        }
    }

    fn delta_filter(&self, spec: &dbine_driver::DeltaSpec, buckets: &[i64]) -> Result<String> {
        if !delta::capable(self.variant) {
            return Err(Error::Unsupported(format!(
                "{} no sincroniza por filas",
                self.info.name
            )));
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

    fn index_toggle_script(
        &self,
        table: &ObjectRef,
        index: &dbine_driver::IndexUsage,
        enable: bool,
    ) -> Result<dbine_driver::SyncScript> {
        index_usage::toggle_script(self.variant, table, index, enable)
    }

    /// Every variant but Denodo (see `rename`).
    fn rename_spec(&self) -> Option<dbine_driver::RenameSpec> {
        rename::spec(self.variant)
    }

    fn rename_script(&self, req: &dbine_driver::RenameRequest) -> Result<dbine_driver::SyncScript> {
        rename::script(self.variant, req)
    }

    fn rename_database_script(
        &self,
        database: &str,
        new_name: &str,
        _objects: &[dbine_driver::rename::DatabaseObject],
    ) -> Result<dbine_driver::SyncScript> {
        rename::database_script(self.variant, database, new_name)
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

    fn sync_script(
        &self,
        changes: &[dbine_driver::TableChange],
    ) -> Result<dbine_driver::SyncScript> {
        if self.variant == Variant::Denodo {
            return Err(Error::Unsupported(
                "Denodo no modifica tablas por SQL: las vistas base se definen en Denodo".into(),
            ));
        }
        design::sync_script(self.variant, changes)
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        security::spec(self.variant)
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(self.variant, action)
    }

    fn create_database_fields(&self) -> Vec<Field> {
        create_db::fields(self.variant)
            .into_iter()
            .map(create_db::grouped)
            .collect()
    }

    fn create_database_script(
        &self,
        name: &str,
        options: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        create_db::script(self.variant, name, options)
    }

    fn alter_database_script(
        &self,
        database: &str,
        changes: &std::collections::BTreeMap<String, String>,
    ) -> Result<String> {
        properties::script(self.variant, database, changes)
    }

    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        schemas::spec(self.variant)
    }

    fn create_schema_script(
        &self,
        _database: Option<&str>,
        name: &str,
        owner: Option<&str>,
    ) -> Result<String> {
        schemas::create_script(self.variant, name, owner)
    }

    fn schema_owner_script(
        &self,
        _database: Option<&str>,
        name: &str,
        owner: &str,
    ) -> Result<Option<String>> {
        schemas::owner_script(self.variant, name, owner)
    }

    fn drop_schema_script(
        &self,
        _database: Option<&str>,
        name: &str,
        cascade: bool,
    ) -> Result<String> {
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
            return Err(Error::Unsupported(
                "Denodo no crea tablas por SQL: las vistas base se definen en Denodo".into(),
            ));
        }
        Ok(design::table_ddl(self.variant, table, parts))
    }

    fn insert_script(
        &self,
        target: &ObjectRef,
        columns: &[String],
        rows: &[Vec<serde_json::Value>],
    ) -> Result<String> {
        Ok(design::insert_script(
            self.variant,
            target.schema(),
            &target.name,
            columns,
            rows,
        ))
    }

    fn update_script(
        &self,
        target: &ObjectRef,
        changes: &[dbine_driver::RowChange],
    ) -> Result<String> {
        Ok(design::update_script(
            self.variant,
            target.schema(),
            &target.name,
            changes,
        ))
    }

    fn delete_script(
        &self,
        target: &ObjectRef,
        keys: &[Vec<(String, serde_json::Value)>],
    ) -> Result<String> {
        Ok(design::delete_script(
            self.variant,
            target.schema(),
            &target.name,
            keys,
        ))
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        database: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let v = self.variant;
        let dbname = database
            .filter(|d| !d.is_empty())
            .or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()))
            .unwrap_or(v.default_database())
            .to_string();

        // Before any token is asked for: a cloud sign-in needs a verified
        // server.
        let plan = tls_plan(v, cfg)?;
        // Through an SSH tunnel the client dials the local end but verifies
        // the certificate against the real server's name.
        let real = cfg.tls_server_name();
        let tunnel = (real != cfg.host.trim())
            .then(|| cfg.host.trim().parse::<std::net::IpAddr>().ok())
            .flatten();
        let mut pg = tokio_postgres::Config::new();
        match tunnel {
            Some(local) => pg.host(real).hostaddr(local),
            None => pg.host(if cfg.host.is_empty() {
                "localhost"
            } else {
                &cfg.host
            }),
        };
        pg.port(cfg.port_or(v.default_port()))
            .dbname(&dbname)
            .connect_timeout(Duration::from_secs(15))
            .ssl_mode(plan.mode);
        if v != Variant::Denodo {
            pg.application_name("DBine");
        }
        if let Some(u) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
            pg.user(u);
        } else if let Some(u) = v.default_user() {
            pg.user(u);
        }
        // A cloud sign-in: a new token before every connect (the app opens
        // every session, and every reconnect, through here).
        let spec = cloud_auth::Spec {
            default_port: v.default_port(),
            gcp_access_token: v == Variant::AlloyDb,
            mysql: false,
        };
        let cloud = cloud_auth::login(cfg, spec).await?;
        match &cloud {
            Some(login) => {
                pg.password(login.password.as_str());
                if let (Some(u), true) = (
                    &login.user,
                    cfg.username.as_deref().is_none_or(|u| u.is_empty()),
                ) {
                    pg.user(u);
                }
            }
            None => {
                if let Some(p) = &cfg.password {
                    pg.password(p.as_str());
                }
            }
        }

        let (tls, pin) = tls_connector(cfg, plan)?;
        let explain = |e: Error| match &pin {
            Some(w) => w.explain(e),
            None => e,
        };

        let (mut client, mut notices) = open(v, &pg, &tls).await.map_err(explain)?;
        // Gateways and proxies that only take the simple protocol answer a
        // Parse with an error (and may leave the connection out of step):
        // found out on a probe, the session goes on a fresh connection.
        let simple = match cfg.option(PROTOCOL_OPTION).map(str::trim) {
            Some("simple") => true,
            _ => {
                let refused =
                    match tokio::time::timeout(PROBE_LIMIT, client.prepare("SELECT 1")).await {
                        Ok(Ok(_)) => false,
                        Ok(Err(e)) => extended_refused(&e),
                        Err(_) => true,
                    };
                if refused {
                    tracing::info!(
                        "{}: the server refuses the extended protocol; using the simple one",
                        self.info.id
                    );
                    (client, notices) = open(v, &pg, &tls).await.map_err(explain)?;
                }
                refused
            }
        };

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
            match client
                .simple_query("SELECT current_setting('server_version_num')")
                .await
            {
                Ok(msgs) => catalog::first_cell(&msgs)
                    .and_then(|s| s.parse().ok())
                    .unwrap_or(0),
                Err(_) => 0,
            }
        } else {
            0
        };

        let mut session = PgSession::new(client, tls, v, version, dbname, notices);
        session.simple = simple;
        // First connection without a pin: the certificate it trusted, for
        // the app to pin.
        session.tls_pin_seen = pin
            .filter(|_| dbine_tls::pin_of(cfg).is_none())
            .and_then(|w| w.seen());
        session.identity = cloud.and_then(|l| l.identity);
        Ok(Box::new(session))
    }
}

/// libpq's TLS mode (`disable` … `verify-full`).
pub(crate) const SSLMODE: &str = "sslmode";
/// libpq's `sslrootcert`, stored under the key it always had.
pub(crate) const SSL_ROOT_CERT: &str = "ssl_root_cert";

/// The connection option that picks the query protocol: `auto` (extended,
/// falling back to simple when the server refuses it) or `simple`.
pub(crate) const PROTOCOL_OPTION: &str = "query_protocol";

/// Longest the extended-protocol probe may take on connect.
const PROBE_LIMIT: Duration = Duration::from_secs(5);

/// Connect and drive the connection, forwarding notices (RAISE NOTICE,
/// warnings…).
async fn open(
    v: Variant,
    pg: &tokio_postgres::Config,
    tls: &MakeRustlsConnect,
) -> Result<(
    tokio_postgres::Client,
    mpsc::UnboundedReceiver<tokio_postgres::error::DbError>,
)> {
    let (client, mut connection) =
        tokio::time::timeout(Duration::from_secs(20), pg.connect(tls.clone()))
            .await
            .map_err(|_| {
                Error::connect_failed(dbine_driver::ConnectFailure::new(
                    dbine_driver::ConnectFailureKind::Timeout,
                    "tiempo de espera agotado",
                ))
            })?
            .map_err(|e| connect_error(v, e))?;
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
    Ok((client, notices))
}

/// The server turned down the extended protocol (as opposed to failing the
/// probe statement for another reason): `0A000 feature_not_supported`, or a
/// message that says so, or the connection dropped on it.
fn extended_refused(e: &tokio_postgres::Error) -> bool {
    if e.is_closed() {
        return true;
    }
    if e.code() == Some(&SqlState::FEATURE_NOT_SUPPORTED) {
        return true;
    }
    let m = e.to_string().to_ascii_lowercase();
    m.contains("extended query") || m.contains("extended protocol") || m.contains("simple query")
}

/// What the connection's TLS options ask of tokio-postgres and rustls.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
struct TlsPlan {
    mode: tokio_postgres::config::SslMode,
    /// The chain isn't verified: the certificate is pinned instead (the
    /// first one seen, then only that one; `dbine_tls::PinVerifier`).
    pinned: bool,
    /// The certificate must name the server.
    hostname: bool,
    /// The system's roots anchor the chain (besides the CA file).
    system_roots: bool,
    /// Amazon's RDS CAs anchor it when there is no CA file.
    rds_roots: bool,
}

/// The `sslmode` values a cloud sign-in takes.
const VERIFIED_SSLMODES: &str = "sslmode=verify-ca / verify-full";

/// The connection's `sslmode` and `sslrootcert`, as libpq reads them:
/// `prefer` and `require` don't check the certificate, `verify-ca` checks the
/// chain against the CA file only, `verify-full` the chain and the name.
/// DBine's differences: `verify-full` trusts the CA file on top of the
/// system's roots (libpq, only the file), and on Cloud SQL, AlloyDB and with
/// Google Cloud IAM the CA file is the only anchor at both verify levels.
/// Aurora and AWS IAM trust Amazon's RDS CAs when there is no file. A cloud
/// sign-in needs a verify level.
///
/// A connection saved before `sslmode` existed has none: it keeps what its
/// `encrypt` / `trust_server_certificate` pair did. ff (shown as `prefer`)
/// negotiates TLS when offered and then verifies chain and name; ft
/// (`prefer`) doesn't verify; tf (`verify-full`, `verify-ca` on Cloud SQL and
/// AlloyDB) requires TLS and verifies, only the chain with the instance CA;
/// tt (`require`) doesn't verify. A cloud sign-in requires verified TLS.
fn tls_plan(v: Variant, cfg: &ConnectionConfig) -> Result<TlsPlan> {
    use tokio_postgres::config::SslMode;
    let off = TlsPlan {
        mode: SslMode::Disable,
        pinned: false,
        hostname: true,
        system_roots: true,
        rds_roots: false,
    };
    let Some(field) = v.sslmode_field() else {
        return Ok(off);
    };
    let cloud = cloud_auth::method(cfg);
    let instance_ca = ca_path(cfg).is_some()
        && (matches!(v, Variant::CloudSql | Variant::AlloyDb) || cloud == Some(cloud_auth::GCP));
    let aws = cloud == Some(cloud_auth::AWS);
    if cfg.option(SSLMODE).is_none() {
        if cloud.is_some() && cfg.trust_server_certificate {
            return Err(cloud_auth::unverified_tls(VERIFIED_SSLMODES));
        }
        let verify = !cfg.trust_server_certificate;
        return Ok(TlsPlan {
            mode: if cfg.encrypt || cloud.is_some() {
                SslMode::Require
            } else {
                SslMode::Prefer
            },
            pinned: !verify,
            hostname: verify && !instance_ca,
            system_roots: !(verify && instance_ca),
            rds_roots: aws || (v == Variant::Aurora && cfg.encrypt),
        });
    }
    let mode = cfg
        .tls_value(&field)
        .unwrap_or("")
        .trim()
        .to_ascii_lowercase();
    let level = field.tls_level_of(&mode).ok_or_else(|| {
        Error::Connect(format!(
            "sslmode: «{mode}» no es un valor de libpq (disable, prefer, require, verify-ca, verify-full)"
        ))
    })?;
    if cloud.is_some() && !level.verified() {
        return Err(cloud_auth::unverified_tls(VERIFIED_SSLMODES));
    }
    let rds_roots = aws || v == Variant::Aurora;
    let unverified = |mode| TlsPlan {
        mode,
        pinned: true,
        hostname: false,
        system_roots: true,
        rds_roots: false,
    };
    Ok(match level {
        TlsLevel::Off => off,
        TlsLevel::Optional => unverified(SslMode::Prefer),
        TlsLevel::Unverified => unverified(SslMode::Require),
        TlsLevel::VerifyCa => {
            if ca_path(cfg).is_none() && !rds_roots {
                return Err(Error::Connect(
                    "sslmode=verify-ca necesita la CA del servidor en sslrootcert".into(),
                ));
            }
            TlsPlan {
                mode: SslMode::Require,
                pinned: false,
                hostname: false,
                system_roots: false,
                rds_roots,
            }
        }
        TlsLevel::VerifyFull => TlsPlan {
            mode: SslMode::Require,
            pinned: false,
            hostname: true,
            system_roots: !instance_ca,
            rds_roots,
        },
    })
}

/// The `sslrootcert` file, if any.
fn ca_path(cfg: &ConnectionConfig) -> Option<&str> {
    cfg.option(SSL_ROOT_CERT)
        .map(str::trim)
        .filter(|p| !p.is_empty())
}

/// The TLS connector for a plan, and what its pinning saw when it pins.
/// Verified plans trust the CA file, when given, every certificate of it (a
/// bundle like AWS's global-bundle.pem), else Amazon's RDS CAs when the plan
/// wants them, besides the system's roots when it keeps them; without the
/// host name (`verify-ca`) the chain is still checked whole.
fn tls_connector(
    cfg: &ConnectionConfig,
    plan: TlsPlan,
) -> Result<(MakeRustlsConnect, Option<dbine_tls::PinWatch>)> {
    if plan.pinned {
        let (c, watch) = dbine_tls::pinned_config(dbine_tls::pin_of(cfg))?;
        return Ok((MakeRustlsConnect::new(c), Some(watch)));
    }
    let mut roots = if plan.system_roots {
        dbine_tls::system_roots()
    } else {
        dbine_tls::rustls::RootCertStore::empty()
    };
    if let Some(path) = ca_path(cfg) {
        let pem = std::fs::read(path).map_err(|e| {
            Error::Connect(format!(
                "no se pudo leer el certificado de CA «{path}»: {e}"
            ))
        })?;
        let bad = |e: String| {
            Error::Connect(format!(
                "el certificado de CA «{path}» no es un PEM válido: {e}"
            ))
        };
        let certs = dbine_tls::rustls::pki_types::pem::PemObject::pem_slice_iter(&pem)
            .collect::<std::result::Result<Vec<dbine_tls::rustls::pki_types::CertificateDer<'static>>, _>>()
            .map_err(|e| bad(e.to_string()))?;
        if certs.is_empty() {
            return Err(bad("no PEM certificates".into()));
        }
        for c in certs {
            roots.add(c).map_err(|e| bad(e.to_string()))?;
        }
    } else if plan.rds_roots {
        for c in rds_certificates()? {
            let _ = roots.add(c);
        }
    }
    let config = if plan.hostname {
        dbine_tls::verified_config(roots)?
    } else {
        dbine_tls::ca_only_config(roots)?
    };
    Ok((MakeRustlsConnect::new(config), None))
}

type Cert = dbine_tls::rustls::pki_types::CertificateDer<'static>;

/// Amazon's RDS CAs, parsed once.
fn rds_certificates() -> Result<Vec<Cert>> {
    static CERTS: std::sync::OnceLock<Vec<Cert>> = std::sync::OnceLock::new();
    if let Some(c) = CERTS.get() {
        return Ok(c.clone());
    }
    let parsed =
        dbine_tls::rustls::pki_types::pem::PemObject::pem_slice_iter(cloud_auth::RDS_CA_BUNDLE)
            .collect::<std::result::Result<Vec<Cert>, _>>()
            .map_err(|e| {
                Error::Connect(format!(
                    "TLS: el paquete de CA de Amazon RDS no se pudo leer: {e}"
                ))
            })?;
    Ok(CERTS.get_or_init(|| parsed).clone())
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
        Some(c)
            if *c == SqlState::INVALID_PASSWORD
                || *c == SqlState::INVALID_AUTHORIZATION_SPECIFICATION =>
        {
            Error::AuthFailed(db_message(&e))
        }
        // Classified for the app's explanation (same messages as before).
        Some(c) => Error::connect_failed(
            dbine_driver::ConnectFailure::from_message(db_message(&e)).with_code(c.code()),
        ),
        None => {
            let mut f = dbine_driver::ConnectFailure::from_error(&e);
            // tokio-postgres says only "error performing TLS handshake":
            // the reason is in its source.
            if f.kind.is_tls() {
                if let Some(cause) = std::error::Error::source(&e) {
                    f.message = format!("{}: {cause}", f.message);
                }
                // The platform's TLS (Security.framework, SChannel) words a
                // certificate it can't verify in ways the shared
                // classification doesn't know ("One or more certificates
                // required to validate this certificate cannot be found").
                if f.kind == dbine_driver::ConnectFailureKind::Tls
                    && f.message.to_ascii_lowercase().contains("certificate")
                {
                    f.kind = dbine_driver::ConnectFailureKind::TlsUntrusted;
                }
            }
            // What to change, in libpq's terms.
            let hint = match f.kind {
                dbine_driver::ConnectFailureKind::TlsUntrusted => {
                    "No se pudo verificar el certificado del servidor: indicá su CA en sslrootcert, o elegí \
                     sslmode=require si confiás en la red."
                }
                dbine_driver::ConnectFailureKind::TlsNotOffered => {
                    "El servidor no ofrece TLS: elegí sslmode=prefer o disable para conectar sin cifrar."
                }
                _ => "",
            };
            if !hint.is_empty() {
                f.message = format!("{}. {hint}", f.message);
            }
            Error::connect_failed(f)
        }
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

    /// Nothing listens on the port: a classified, refused connect.
    #[tokio::test]
    async fn refused_connects_are_classified() {
        let d = drivers()
            .into_iter()
            .find(|d| d.info().id == "postgres")
            .unwrap();
        let cfg = ConnectionConfig {
            driver: "postgres".into(),
            host: "127.0.0.1".into(),
            port: 1,
            username: Some("u".into()),
            password: Some("p".into()),
            ..Default::default()
        };
        match d.connect(&cfg, None).await.err() {
            Some(Error::ConnectFailed(f)) => {
                assert_eq!(f.kind, dbine_driver::ConnectFailureKind::Refused, "{f:?}")
            }
            other => panic!("{other:?}"),
        }
    }

    /// "Con opción de otorgar" is offered on the new schema's grants
    /// exactly where the engine writes them (`SchemaSpec::grant_option`).
    #[test]
    fn schema_grant_option_matches_the_script() {
        for d in crate::drivers() {
            let Some(spec) = d.schema_spec() else {
                continue;
            };
            let Some(p) = spec.privileges.first() else {
                continue;
            };
            let grant = |grantable| {
                d.schema_grant_script(None, "ventas", &[p.to_string()], "ana", grantable)
            };
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(
                grant(true).is_ok(),
                spec.grant_option,
                "{}: {:?}",
                d.info().id,
                grant(true)
            );
        }
    }

    #[test]
    fn ids_are_unique_and_ports_known() {
        let ds = drivers();
        let mut ids: Vec<_> = ds.iter().map(|d| d.info().id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), ds.len());
        let port = |id: &str| {
            ds.iter()
                .find(|d| d.info().id == id)
                .unwrap()
                .info()
                .default_port
        };
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

    /// The cloud sign-ins show only where that cloud runs the engine.
    #[test]
    fn cloud_sign_ins_match_the_variant() {
        let offered = |id: &str| -> Vec<&'static str> {
            let d = drivers().into_iter().find(|d| d.info().id == id).unwrap();
            match d
                .info()
                .fields
                .iter()
                .find(|f| f.key == cloud_auth::AUTH)
                .map(|f| f.kind.clone())
            {
                Some(FieldKind::Select(o)) => o.into_iter().map(|o| o.0).collect(),
                _ => vec![],
            }
        };
        assert_eq!(
            offered("postgres"),
            [
                cloud_auth::PASSWORD,
                cloud_auth::AWS,
                cloud_auth::GCP,
                cloud_auth::AZURE
            ]
        );
        assert_eq!(
            offered("aurora_postgres"),
            [cloud_auth::PASSWORD, cloud_auth::AWS]
        );
        assert_eq!(
            offered("cloudsql_postgres"),
            [cloud_auth::PASSWORD, cloud_auth::GCP]
        );
        assert_eq!(offered("alloydb"), [cloud_auth::PASSWORD, cloud_auth::GCP]);
        for id in ["redshift", "cockroachdb", "h2", "denodo", "timescaledb"] {
            assert!(offered(id).is_empty(), "{id}");
        }
    }

    /// A cloud sign-in with "trust the certificate" never sends the token.
    #[tokio::test]
    async fn cloud_tokens_need_a_verified_server() {
        let d = drivers()
            .into_iter()
            .find(|d| d.info().id == "aurora_postgres")
            .unwrap();
        let mut cfg = ConnectionConfig {
            driver: "aurora_postgres".into(),
            host: "127.0.0.1".into(),
            port: 1,
            username: Some("u".into()),
            trust_server_certificate: true,
            ..Default::default()
        };
        cfg.options
            .insert(cloud_auth::AUTH.into(), cloud_auth::AWS.into());
        assert!(matches!(
            d.connect(&cfg, None).await.err(),
            Some(Error::AuthFailed(_))
        ));
    }

    #[test]
    fn every_variant_declares_libpq_tls_options() {
        for v in Variant::ALL {
            let info = v.info();
            let mode = info.fields.iter().find(|f| f.key == SSLMODE);
            let ca = info.fields.iter().find(|f| f.key == SSL_ROOT_CERT);
            assert!(info.fields.iter().all(|f| !f.is_legacy_tls()), "{v:?}");
            if v == Variant::H2 {
                assert!(mode.is_none() && ca.is_none());
                assert_eq!(info.tls, TlsSupport::None);
                continue;
            }
            let mode = mode.unwrap();
            let cloud_ca = matches!(v, Variant::CloudSql | Variant::AlloyDb);
            assert_eq!(
                mode.default,
                if cloud_ca { "verify-ca" } else { "verify-full" },
                "{v:?}"
            );
            assert_eq!(ca.unwrap().label, "sslrootcert");
            assert_eq!(info.tls, TlsSupport::Configurable);
        }
    }

    fn tls_cfg(encrypt: bool, trust: bool, options: &[(&str, &str)]) -> ConnectionConfig {
        let mut cfg = ConnectionConfig {
            host: "db.example.com".into(),
            encrypt,
            trust_server_certificate: trust,
            ..Default::default()
        };
        for (k, v) in options {
            cfg.options.insert((*k).into(), (*v).into());
        }
        cfg
    }

    fn plan(
        mode: tokio_postgres::config::SslMode,
        pinned: bool,
        hostname: bool,
        system_roots: bool,
        rds_roots: bool,
    ) -> TlsPlan {
        TlsPlan {
            mode,
            pinned,
            hostname,
            system_roots,
            rds_roots,
        }
    }

    /// A connection saved before `sslmode` gets what it got before, for each
    /// `(encrypt, trust_server_certificate)` pair: ff `Prefer` and verified
    /// when TLS happens, ft `Prefer` unchecked, tf `Require` verified, tt
    /// `Require` unchecked; Cloud SQL / AlloyDB / GCP IAM with their CA check
    /// only the chain against it; Aurora with `encrypt` and AWS IAM add the
    /// RDS CAs.
    #[test]
    fn legacy_connections_keep_their_tls() {
        use tokio_postgres::config::SslMode::{Prefer, Require};
        let pairs = [(false, false), (false, true), (true, false), (true, true)];
        let check = |v: Variant, options: &[(&str, &str)], want: [TlsPlan; 4]| {
            for (i, (e, t)) in pairs.into_iter().enumerate() {
                assert_eq!(
                    tls_plan(v, &tls_cfg(e, t, options)).unwrap(),
                    want[i],
                    "{v:?} {options:?} encrypt={e} trust={t}"
                );
            }
        };
        for v in Variant::ALL.into_iter().filter(|v| *v != Variant::H2) {
            let rds = v == Variant::Aurora;
            check(
                v,
                &[],
                [
                    plan(Prefer, false, true, true, false),
                    plan(Prefer, true, false, true, false),
                    plan(Require, false, true, true, rds),
                    plan(Require, true, false, true, rds),
                ],
            );
        }
        for v in [Variant::CloudSql, Variant::AlloyDb] {
            check(
                v,
                &[(SSL_ROOT_CERT, "/ca.pem")],
                [
                    plan(Prefer, false, false, false, false),
                    plan(Prefer, true, false, true, false),
                    plan(Require, false, false, false, false),
                    plan(Require, true, false, true, false),
                ],
            );
        }
        // Google Cloud IAM on plain PostgreSQL with the instance CA: chain
        // only, TLS required even with encrypt off.
        let gcp = [
            (cloud_auth::AUTH, cloud_auth::GCP),
            (SSL_ROOT_CERT, "/ca.pem"),
        ];
        for e in [false, true] {
            assert_eq!(
                tls_plan(Variant::Postgres, &tls_cfg(e, false, &gcp)).unwrap(),
                plan(Require, false, false, false, false)
            );
            // A token never goes to an unverified server.
            assert!(matches!(
                tls_plan(Variant::Postgres, &tls_cfg(e, true, &gcp)),
                Err(Error::AuthFailed(_))
            ));
        }
        let aws = [(cloud_auth::AUTH, cloud_auth::AWS)];
        assert_eq!(
            tls_plan(Variant::Postgres, &tls_cfg(false, false, &aws)).unwrap(),
            plan(Require, false, true, true, true)
        );
        // The values the form shows for those pairs.
        let f = Variant::Postgres.sslmode_field().unwrap();
        let shown: Vec<_> = pairs
            .iter()
            .map(|(e, t)| tls_cfg(*e, *t, &[]).tls_value(&f).unwrap().to_string())
            .collect();
        assert_eq!(shown, ["prefer", "prefer", "verify-full", "require"]);
        let f = Variant::CloudSql.sslmode_field().unwrap();
        assert_eq!(tls_cfg(true, false, &[]).tls_value(&f), Some("verify-ca"));
    }

    /// A new connection's defaults verify: chain and name, or the chain
    /// against the instance CA on Cloud SQL and AlloyDB.
    #[test]
    fn new_connections_verify() {
        use tokio_postgres::config::SslMode::Require;
        for v in Variant::ALL.into_iter().filter(|v| *v != Variant::H2) {
            let f = v.sslmode_field().unwrap();
            let mut options = vec![(SSLMODE, f.default)];
            let cloud_ca = matches!(v, Variant::CloudSql | Variant::AlloyDb);
            if cloud_ca {
                // verify-ca needs the CA file.
                assert!(tls_plan(v, &tls_cfg(false, false, &options)).is_err());
                options.push((SSL_ROOT_CERT, "/ca.pem"));
            }
            let p = tls_plan(v, &tls_cfg(false, false, &options)).unwrap();
            let rds = v == Variant::Aurora;
            assert_eq!(
                p,
                if cloud_ca {
                    plan(Require, false, false, false, false)
                } else {
                    plan(Require, false, true, true, rds)
                },
                "{v:?}"
            );
            let level = tls_cfg(false, false, &options).tls_level(&v.info().fields);
            assert!(level.is_some_and(|l| l.verified()), "{v:?}");
        }
        // H2: no TLS at all.
        assert_eq!(
            tls_plan(Variant::H2, &tls_cfg(true, false, &[]))
                .unwrap()
                .mode,
            tokio_postgres::config::SslMode::Disable
        );
    }

    #[test]
    fn saved_sslmode_means_what_libpq_means() {
        use tokio_postgres::config::SslMode::{Disable, Prefer, Require};
        let v = Variant::Postgres;
        // The legacy bools don't count once sslmode is saved.
        let get = |o: &[(&str, &str)]| tls_plan(v, &tls_cfg(true, false, o));
        assert_eq!(
            get(&[(SSLMODE, "disable")]).unwrap(),
            plan(Disable, false, true, true, false)
        );
        assert_eq!(
            get(&[(SSLMODE, "prefer")]).unwrap(),
            plan(Prefer, true, false, true, false)
        );
        assert_eq!(
            get(&[(SSLMODE, "require")]).unwrap(),
            plan(Require, true, false, true, false)
        );
        // verify-ca: the chain against the CA file only, which it needs.
        assert!(matches!(
            get(&[(SSLMODE, "verify-ca")]),
            Err(Error::Connect(_))
        ));
        assert_eq!(
            get(&[(SSLMODE, "verify-ca"), (SSL_ROOT_CERT, "/ca.pem")]).unwrap(),
            plan(Require, false, false, false, false)
        );
        assert_eq!(
            get(&[(SSLMODE, "verify-full"), (SSL_ROOT_CERT, "/ca.pem")]).unwrap(),
            plan(Require, false, true, true, false)
        );
        assert_eq!(
            get(&[(SSLMODE, "VERIFY-FULL")]).unwrap(),
            plan(Require, false, true, true, false)
        );
        assert!(get(&[(SSLMODE, "allow")]).is_err());
        // Aurora: the RDS CAs at both verify levels when there's no file.
        assert_eq!(
            tls_plan(
                Variant::Aurora,
                &tls_cfg(false, false, &[(SSLMODE, "verify-ca")])
            )
            .unwrap(),
            plan(Require, false, false, false, true)
        );
        // Cloud SQL with its CA under verify-full: the name too, the CA only.
        assert_eq!(
            tls_plan(
                Variant::CloudSql,
                &tls_cfg(
                    false,
                    false,
                    &[(SSLMODE, "verify-full"), (SSL_ROOT_CERT, "/ca.pem")]
                )
            )
            .unwrap(),
            plan(Require, false, true, false, false)
        );
        // A cloud sign-in needs a verify level.
        for mode in ["disable", "prefer", "require"] {
            assert!(
                matches!(
                    get(&[(SSLMODE, mode), (cloud_auth::AUTH, cloud_auth::AZURE)]),
                    Err(Error::AuthFailed(_))
                ),
                "{mode}"
            );
        }
        assert!(get(&[
            (SSLMODE, "verify-full"),
            (cloud_auth::AUTH, cloud_auth::AZURE)
        ])
        .is_ok());
    }

    #[test]
    fn a_missing_ca_file_is_a_connect_error() {
        let cfg = tls_cfg(true, false, &[(SSL_ROOT_CERT, "/nonexistent/ca.pem")]);
        let p = tls_plan(Variant::Aurora, &cfg).unwrap();
        assert!(matches!(tls_connector(&cfg, p), Err(Error::Connect(_))));
        let cfg = tls_cfg(true, false, &[]);
        let p = tls_plan(Variant::Aurora, &cfg).unwrap();
        assert!(p.rds_roots && tls_connector(&cfg, p).is_ok());
    }

    /// Whenever the certificate's name isn't checked, only the CA file (or
    /// the RDS bundle) anchors the chain, unless the user chose to accept
    /// any certificate.
    #[test]
    fn relaxed_hostname_checks_never_use_the_system_roots() {
        let modes = [
            "",
            "disable",
            "prefer",
            "require",
            "verify-ca",
            "verify-full",
        ];
        let auths = ["", cloud_auth::AWS, cloud_auth::GCP, cloud_auth::AZURE];
        for v in Variant::ALL {
            for mode in modes {
                for auth in auths {
                    for ca in ["", "/ca.pem"] {
                        for (e, t) in [(false, false), (false, true), (true, false), (true, true)] {
                            let mut o = vec![];
                            for (k, val) in [
                                (SSLMODE, mode),
                                (cloud_auth::AUTH, auth),
                                (SSL_ROOT_CERT, ca),
                            ] {
                                if !val.is_empty() {
                                    o.push((k, val));
                                }
                            }
                            if let Ok(p) = tls_plan(v, &tls_cfg(e, t, &o)) {
                                assert!(
                                    p.hostname || !p.system_roots || p.pinned,
                                    "{v:?} {o:?} {e} {t}"
                                );
                                if !auth.is_empty() && v != Variant::H2 {
                                    assert!(
                                        !p.pinned
                                            && p.mode == tokio_postgres::config::SslMode::Require,
                                        "{v:?} {o:?} {e} {t}"
                                    );
                                }
                            }
                        }
                    }
                }
            }
        }
    }

    /// Live (needs the network): a publicly trusted certificate for another
    /// name is refused on the Cloud SQL path, where the name isn't checked.
    /// `cargo test -p dbine-driver-postgres -- --ignored public_cert`
    #[test]
    #[ignore]
    fn a_public_cert_for_another_name_is_refused_without_the_name_check() {
        let dir = std::env::temp_dir().join(format!("dbine-pg-ca-{}", std::process::id()));
        std::fs::create_dir_all(&dir).unwrap();
        let ca = dir.join("ca.pem");
        // Any CA that didn't sign the site's certificate: Amazon's RDS bundle.
        std::fs::write(&ca, cloud_auth::RDS_CA_BUNDLE).unwrap();
        let cfg = tls_cfg(true, false, &[(SSL_ROOT_CERT, ca.to_str().unwrap())]);
        let rt = tokio::runtime::Runtime::new().unwrap();
        let dial = |mut c: MakeRustlsConnect| {
            use tokio_postgres::tls::{MakeTlsConnect, TlsConnect};
            rt.block_on(async move {
                let tcp = tokio::net::TcpStream::connect("www.google.com:443")
                    .await
                    .unwrap();
                let t = MakeTlsConnect::<tokio::net::TcpStream>::make_tls_connect(
                    &mut c,
                    "not-this-host.invalid",
                )
                .unwrap();
                t.connect(tcp).await.map(|_| ())
            })
        };
        for v in [Variant::CloudSql, Variant::AlloyDb] {
            let p = tls_plan(v, &cfg).unwrap();
            assert!(
                dial(tls_connector(&cfg, p).unwrap().0).is_err(),
                "{v:?} accepted a public certificate"
            );
        }
        // Control: with the system roots and no name check, it would pass.
        let c = dbine_tls::ca_only_config(dbine_tls::system_roots()).unwrap();
        assert!(dial(MakeRustlsConnect::new(c)).is_ok());
        let _ = std::fs::remove_dir_all(&dir);
    }
}
