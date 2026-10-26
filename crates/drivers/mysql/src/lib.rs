//! MySQL and the engines that speak its protocol (MariaDB, TiDB,
//! OceanBase, SingleStore, StarRocks, Doris, Databend, Manticore Search,
//! GreptimeDB) and the managed services built on them (Amazon Aurora
//! MySQL, Cloud SQL for MySQL, VeloDB), through mysql_async on a single connection (no pool).
//! Catalog queries use the text protocol with escaped literals because
//! several of these engines don't support prepared statements.

mod backup;
mod blocking;
mod cells;
/// Cloud sign-in (AWS IAM, Google Cloud IAM, Entra ID), shared with the
/// PostgreSQL driver.
#[path = "../../postgres/src/cloud_auth.rs"]
pub(crate) mod cloud_auth;
mod create_db;
mod design;
mod health;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod properties;
mod read_only;
mod rename;
mod rename_db;
mod security;
mod session;
mod stats;
mod structure;
mod transfer;

use dbine_driver::{
    async_trait, Capabilities, ConnectionConfig, CreateTemplate, DdlParts, DesignerSpec, Driver,
    DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, Result,
    ScriptDefaults, ScriptDialect, ScriptError, ScriptMode, Session, TableSchema,
};
use mysql_async::prelude::Queryable;
use mysql_async::{Conn, Opts, OptsBuilder, SslOpts};
use session::MySqlSession;
use std::sync::Arc;
use std::time::Duration;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Variant {
    MySql,
    MariaDb,
    TiDb,
    OceanBase,
    SingleStore,
    StarRocks,
    Doris,
    Databend,
    Manticore,
    GreptimeDb,
    /// Amazon Aurora MySQL: MySQL with Aurora's storage and replica views.
    AuroraMySql,
    /// Cloud SQL for MySQL: managed MySQL on Google Cloud.
    CloudSqlMySql,
    /// VeloDB: managed Apache Doris.
    VeloDb,
}

impl Variant {
    /// Prepared statements with bound values behave as MySQL's
    /// (`Session::execute_bound`); the rest get the values as literals.
    pub(crate) fn binds(self) -> bool {
        matches!(
            self,
            Variant::MySql
                | Variant::MariaDb
                | Variant::AuroraMySql
                | Variant::CloudSqlMySql
                | Variant::TiDb
                | Variant::SingleStore
                | Variant::OceanBase
        )
    }

    const ALL: [Variant; 13] = [
        Variant::MySql,
        Variant::MariaDb,
        Variant::TiDb,
        Variant::OceanBase,
        Variant::SingleStore,
        Variant::StarRocks,
        Variant::Doris,
        Variant::Databend,
        Variant::Manticore,
        Variant::GreptimeDb,
        Variant::AuroraMySql,
        Variant::CloudSqlMySql,
        Variant::VeloDb,
    ];

    /// The engine a managed product behaves as; everything but the
    /// connection form and the monitor goes by it.
    pub(crate) fn base(self) -> Variant {
        match self {
            Variant::AuroraMySql | Variant::CloudSqlMySql => Variant::MySql,
            Variant::VeloDb => Variant::Doris,
            v => v,
        }
    }

    /// Managed services that authenticate with short-lived IAM tokens sent
    /// as the password (mysql_clear_password, over TLS).
    fn iam_tokens(self) -> bool {
        matches!(self, Variant::AuroraMySql | Variant::CloudSqlMySql)
    }

    /// The cloud sign-ins the form offers: MySQL itself (RDS, Azure
    /// Database for MySQL, Cloud SQL by IP) gets the three; RDS for MariaDB
    /// takes AWS IAM; each managed product, its own cloud.
    fn cloud_offer(self) -> cloud_auth::Offer {
        match self {
            Variant::MySql => cloud_auth::Offer {
                aws: true,
                gcp: true,
                azure: true,
            },
            Variant::MariaDb | Variant::AuroraMySql => cloud_auth::Offer {
                aws: true,
                ..Default::default()
            },
            Variant::CloudSqlMySql => cloud_auth::Offer {
                gcp: true,
                ..Default::default()
            },
            _ => cloud_auth::Offer::default(),
        }
    }

    fn default_port(self) -> u16 {
        match self.base() {
            Variant::MySql | Variant::MariaDb | Variant::SingleStore => 3306,
            Variant::TiDb => 4000,
            Variant::OceanBase => 2881,
            Variant::StarRocks | Variant::Doris => 9030,
            Variant::Databend => 3307,
            Variant::Manticore => 9306,
            Variant::GreptimeDb => 4002,
            _ => unreachable!("managed variants map to their base"),
        }
    }

    /// Real MySQL servers; the rest only emulate parts of it.
    pub(crate) fn is_mysql_server(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb)
    }

    /// `KILL QUERY <connection id>` stops a statement.
    pub(crate) fn has_kill_query(self) -> bool {
        !matches!(
            self,
            Variant::Databend | Variant::Manticore | Variant::GreptimeDb
        )
    }

    /// Reports who waits on whose locks (`Session::blocking`) and ends
    /// sessions with KILL: InnoDB's lock waits, TiDB's DATA_LOCK_WAITS.
    pub(crate) fn has_lock_waits(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb | Variant::TiDb)
    }

    /// Stored procedures and functions in information_schema.ROUTINES.
    pub(crate) fn has_routines(self) -> bool {
        matches!(
            self,
            Variant::MySql | Variant::MariaDb | Variant::OceanBase | Variant::SingleStore
        )
    }

    /// Enforced foreign keys (TiDB since 6.6).
    pub(crate) fn has_foreign_keys(self) -> bool {
        matches!(
            self,
            Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase
        )
    }

    pub(crate) fn has_triggers(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb | Variant::OceanBase)
    }

    /// `CREATE SEQUENCE` objects (MariaDB 10.3+, TiDB, OceanBase, Databend).
    pub(crate) fn has_sequences(self) -> bool {
        matches!(
            self,
            Variant::MariaDb | Variant::TiDb | Variant::OceanBase | Variant::Databend
        )
    }

    /// CHECK constraints (MySQL 8.0.16+, MariaDB, OceanBase, TiDB 7.2+).
    pub(crate) fn has_checks(self) -> bool {
        matches!(
            self,
            Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase
        )
    }

    /// A single namespace: no databases to pick.
    pub(crate) fn single_namespace(self) -> bool {
        self == Variant::Manticore
    }

    /// `SET autocommit = 0` keeps a transaction open until COMMIT /
    /// ROLLBACK, and OK packets carry SERVER_STATUS_IN_TRANS.
    /// A notebook write's staging path: temporary tables, transactions
    /// and a bulk load.
    pub(crate) fn writes_staged(self) -> bool {
        matches!(
            self.base(),
            Variant::MySql | Variant::MariaDb | Variant::TiDb
        ) && transfer::supported(self)
    }

    pub(crate) fn has_transactions(self) -> bool {
        matches!(
            self,
            Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase
        )
    }

    fn info(product: Variant) -> DriverInfo {
        let (id, name, family) = match product {
            Variant::MySql => ("mysql", "MySQL", Family::Relational),
            Variant::MariaDb => ("mariadb", "MariaDB", Family::Relational),
            Variant::TiDb => ("tidb", "TiDB", Family::Relational),
            Variant::OceanBase => ("oceanbase", "OceanBase (MySQL)", Family::Relational),
            Variant::SingleStore => ("singlestore", "SingleStore", Family::Relational),
            Variant::StarRocks => ("starrocks", "StarRocks", Family::Analytical),
            Variant::Doris => ("doris", "Apache Doris", Family::Analytical),
            Variant::Databend => ("databend", "Databend", Family::Analytical),
            Variant::Manticore => ("manticore", "Manticore Search", Family::Search),
            Variant::GreptimeDb => ("greptimedb", "GreptimeDB", Family::TimeSeries),
            Variant::AuroraMySql => ("aurora-mysql", "Amazon Aurora MySQL", Family::Relational),
            Variant::CloudSqlMySql => {
                ("cloudsql-mysql", "Cloud SQL para MySQL", Family::Relational)
            }
            Variant::VeloDb => ("velodb", "VeloDB", Family::Analytical),
        };
        let v = product.base();
        let mut object_kinds = vec![ObjectKindInfo::tables()];
        if v != Variant::Manticore {
            object_kinds.push(ObjectKindInfo::views());
        }
        if v == Variant::StarRocks {
            object_kinds.push(ObjectKindInfo::materialized_views());
        }
        if v.has_routines() {
            object_kinds.push(ObjectKindInfo::procedures());
            object_kinds.push(ObjectKindInfo::functions());
        }
        if v.has_triggers() {
            object_kinds.push(ObjectKindInfo::triggers());
        }
        if v.has_sequences() {
            object_kinds.push(ObjectKindInfo::sequences());
        }
        let mut fields: Vec<Field> = if v.single_namespace() {
            Field::server_set()
                .into_iter()
                .filter(|f| f.key != "database")
                .collect()
        } else {
            Field::server_set()
        };
        // A CA bundle for servers whose certificate isn't in the system store
        // (Amazon RDS, Cloud SQL, self-signed…).
        let at = fields
            .iter()
            .position(|f| f.key == "trust_server_certificate")
            .map_or(fields.len(), |i| i + 1);
        fields.insert(
            at,
            Field::new("ca_cert", "Certificado de la CA (PEM)", FieldKind::File)
                .help("Opcional, con TLS: el certificado raíz que firma el del servidor.")
                .ssl(),
        );
        managed_fields(product, &mut fields);
        cloud_auth::adapt(&mut fields, product.cloud_offer());
        DriverInfo {
            tls: Default::default(),
            tls_port: None,
            id,
            name,
            family,
            language: Language::Sql,
            dialect: "mysql",
            default_port: v.default_port(),
            fields,
            databases_label: if v.single_namespace() {
                ""
            } else {
                "Bases de datos"
            },
            has_schemas: false,
            object_kinds,
        }
    }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    Variant::ALL
        .iter()
        .map(|&p| {
            Arc::new(MySqlDriver {
                variant: p.base(),
                product: p,
                info: Variant::info(p),
            }) as Arc<dyn Driver>
        })
        .collect()
}

/// Managed services: TLS on by default, a CA bundle and IAM-token help.
fn managed_fields(product: Variant, fields: &mut [Field]) {
    let (encrypt_help, password_help) = match product {
        Variant::AuroraMySql => (
            "Aurora acepta TLS; para verificar el certificado, indicá el paquete de CA de Amazon RDS (global-bundle.pem).",
            "Contraseña o un token IAM pegado. Para no pegar tokens que vencen, elegí Autenticación → AWS IAM.",
        ),
        Variant::CloudSqlMySql => (
            "Cloud SQL puede exigir TLS; indicá el server-ca.pem de la instancia para verificarlo. Los certificados de cliente no están soportados: usá el Cloud SQL Auth Proxy si la instancia los exige.",
            "Contraseña o un token de IAM pegado. Para no pegar tokens que vencen, elegí Autenticación → Google Cloud IAM.",
        ),
        Variant::VeloDb => ("VeloDB Cloud exige TLS en los endpoints públicos.", ""),
        _ => return,
    };
    for f in fields.iter_mut() {
        match f.key {
            "encrypt" => {
                f.default = "true";
                f.help = encrypt_help;
            }
            "password" if !password_help.is_empty() => f.help = password_help,
            _ => {}
        }
    }
}

pub struct MySqlDriver {
    /// The engine it behaves as.
    variant: Variant,
    /// What the user picked (differs for managed services).
    product: Variant,
    info: DriverInfo,
}

#[async_trait]
impl Driver for MySqlDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    /// Manticore has no SQL EXPLAIN; the others at least a plain one.
    fn supports_explain(&self) -> bool {
        self.variant != Variant::Manticore
    }

    fn capabilities(&self) -> Capabilities {
        design::capabilities(self.variant)
    }

    /// "Nueva base de datos"'s options (see [`create_db`]).
    fn create_database_fields(&self) -> Vec<dbine_driver::Field> {
        create_db::fields(self.variant)
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

    /// `LOAD DATA LOCAL INFILE` or big multi-row INSERTs (see `transfer`);
    /// Manticore uses the INSERT script.
    fn supports_bulk_load(&self) -> bool {
        transfer::supported(self.variant)
    }

    /// Notebook writes stage into a `TEMPORARY` table and apply in one
    /// transaction: the engines with both (and a bulk load).
    fn write_modes(&self) -> Vec<dbine_driver::WriteMode> {
        if self.variant.writes_staged() {
            dbine_driver::WriteMode::ALL.to_vec()
        } else {
            Vec::new()
        }
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// Every variant with indexes to list (see `index_usage`); the ones
    /// without counters list them with a note.
    fn supports_index_usage(&self) -> bool {
        index_usage::supported(self.variant)
    }

    /// Invisible (MySQL 8.0+, Aurora, Cloud SQL, TiDB, OceanBase) or
    /// ignored (MariaDB 10.6+) indexes; see `index_usage::toggle_script`.
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

    /// Tables and views with `RENAME TABLE`, columns with `CHANGE COLUMN`,
    /// indexes with `RENAME INDEX`; the emulations, tables only (see `rename`).
    fn rename_spec(&self) -> Option<dbine_driver::RenameSpec> {
        rename::spec(self.variant)
    }

    fn rename_script(&self, req: &dbine_driver::RenameRequest) -> Result<dbine_driver::SyncScript> {
        rename::script(self.variant, req)
    }

    /// MySQL and MariaDB: a new database, the tables moved with
    /// `RENAME TABLE` and the code created again (see `rename_db`).
    fn rename_database_script(
        &self,
        database: &str,
        new_name: &str,
        objects: &[dbine_driver::rename::DatabaseObject],
    ) -> Result<dbine_driver::SyncScript> {
        rename_db::script(self.variant, database, new_name, objects)
    }

    fn script_dialect(&self) -> ScriptDialect {
        script_dialect(self.variant)
    }

    /// One statement per request on the tab's connection, as the mysql CLI
    /// sends them: USE, SET, @vars and temporary tables carry over.
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// The mysql CLI stops at the first error (`--force` goes on).
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults {
            continue_on_error: false,
            ..ScriptDefaults::for_language(Language::Sql)
        }
    }

    fn supports_manual_transactions(&self) -> bool {
        self.variant.has_transactions()
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        security::supported(self.variant).then(|| security::spec_for(self.variant))
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        if !security::supported(self.variant) {
            return Err(Error::Unsupported(
                "este motor no administra usuarios desde DBine".into(),
            ));
        }
        security::script_for(self.variant, action)
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        backup::spec(self.product)
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(self.product, action)
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(design::designer(self.variant))
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        let kinds: Vec<&'static str> = self.info.object_kinds.iter().map(|k| k.id).collect();
        design::templates(self.variant, &kinds)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        design::table_ddl(self.variant, table, parts)
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(
        &self,
        changes: &[dbine_driver::TableChange],
    ) -> Result<dbine_driver::SyncScript> {
        design::sync_script(self.variant, changes)
    }

    fn insert_script(
        &self,
        target: &ObjectRef,
        columns: &[String],
        rows: &[Vec<serde_json::Value>],
    ) -> Result<String> {
        Ok(design::insert_script(self.variant, target, columns, rows))
    }

    fn update_script(
        &self,
        target: &ObjectRef,
        changes: &[dbine_driver::RowChange],
    ) -> Result<String> {
        design::update_script(self.variant, target, changes)
    }

    fn delete_script(
        &self,
        target: &ObjectRef,
        keys: &[Vec<(String, serde_json::Value)>],
    ) -> Result<String> {
        design::delete_script(self.variant, target, keys)
    }

    fn filtered_browse(
        &self,
        browse: &str,
        filters: &[dbine_driver::ColumnFilter],
    ) -> Result<String> {
        design::filtered_browse(self.variant, browse, filters)
    }

    async fn connect(
        &self,
        cfg: &ConnectionConfig,
        database: Option<&str>,
    ) -> Result<Box<dyn Session>> {
        let v = self.variant;
        let db = database
            .or(Some(cfg.database.as_str()))
            .filter(|d| !d.is_empty() && !v.single_namespace())
            .map(str::to_string);
        let mut builder = OptsBuilder::default()
            .ip_or_hostname(if cfg.host.is_empty() {
                "localhost"
            } else {
                cfg.host.as_str()
            })
            .tcp_port(cfg.port_or(v.default_port()))
            .user(cfg.username.as_deref().filter(|u| !u.is_empty()))
            .db_name(db.clone())
            .prefer_socket(false);
        if !v.is_mysql_server() {
            // Skip mysql_async's `SELECT @@max_allowed_packet, @@wait_timeout`,
            // which not every emulation answers.
            builder = builder
                .max_allowed_packet(Some(64 * 1024 * 1024))
                .wait_timeout(Some(28_800));
        }
        // A cloud sign-in: a new token before every connect (the app opens
        // every session through here; the session signs again for its own
        // reconnects).
        let spec = cloud_auth::Spec {
            default_port: v.default_port(),
            gcp_access_token: false,
            mysql: true,
        };
        let cloud = cloud_auth::login(cfg, spec).await?;
        match &cloud {
            Some(login) => {
                builder = builder.pass(Some(login.password.clone()));
                if let (Some(u), true) = (
                    &login.user,
                    cfg.username.as_deref().is_none_or(|u| u.is_empty()),
                ) {
                    builder = builder.user(Some(u.clone()));
                }
            }
            None => builder = builder.pass(cfg.password.as_deref()),
        }
        if cfg.encrypt || cloud.is_some() {
            builder = builder.ssl_opts(ssl_opts(self.product, cfg, cloud.is_some()));
            // IAM and Entra ID tokens go as mysql_clear_password, only over
            // TLS whose peer was verified (never with "trust the server
            // certificate", which the cloud sign-in refuses anyway).
            if (self.product.iam_tokens() || cloud.is_some()) && !cfg.trust_server_certificate {
                builder = builder.enable_cleartext_plugin(true);
            }
        }
        let opts = Opts::from(builder);

        let mut conn = tokio::time::timeout(Duration::from_secs(20), Conn::new(opts.clone()))
            .await
            .map_err(|_| {
                Error::connect_failed(dbine_driver::ConnectFailure::new(
                    dbine_driver::ConnectFailureKind::Timeout,
                    "tiempo de espera agotado",
                ))
            })?
            .map_err(connect_error)?;

        // Best effort: an engine that refuses these still works, and the
        // ReadOnlySession wrapper guards read-only connections anyway.
        let mut setup = vec!["SET NAMES utf8mb4"];
        if cfg.read_only {
            setup.push("SET SESSION TRANSACTION READ ONLY");
        }
        for s in setup {
            if let Err(e) = conn.query_drop(s).await {
                tracing::debug!("{}: {s}: {e}", self.info.id);
            }
        }
        let mut session = MySqlSession::new(conn, opts, self.product, db);
        if let Some(login) = cloud {
            session.identity = login.identity;
            let mut again = cfg.clone();
            again.options.remove(cloud_auth::DESCRIBE);
            session.relogin = Some(Arc::new((again, spec)));
        }
        Ok(Box::new(session))
    }
}

/// TLS as the form asks. The CA file, when given, is trusted on top of the
/// system's, and the certificate must name the host dialed, except:
/// - through an SSH tunnel with a cloud sign-in (the client dials
///   127.0.0.1), where the name checked is the real endpoint's;
/// - Cloud SQL (or Google Cloud IAM) with its CA, whose certificate names
///   the instance, not the IP dialed: only the chain is checked, so only
///   that CA is trusted (the system roots are off), or any publicly
///   trusted certificate would get the password or the token.
fn ssl_opts(product: Variant, cfg: &ConnectionConfig, cloud: bool) -> SslOpts {
    let ca = cfg
        .option("ca_cert")
        .map(str::trim)
        .filter(|p| !p.is_empty());
    let cloud_sql = ca.is_some()
        && (product == Variant::CloudSqlMySql || cloud_auth::method(cfg) == Some(cloud_auth::GCP));
    let origin = cfg
        .option(cloud_auth::ORIGIN_HOST)
        .map(str::trim)
        .filter(|h| cloud && !h.is_empty());
    let mut ssl = SslOpts::default().with_danger_accept_invalid_certs(cfg.trust_server_certificate);
    if cfg.trust_server_certificate {
        ssl = ssl.with_danger_skip_domain_validation(true);
    } else if cloud_sql {
        ssl = ssl
            .with_danger_skip_domain_validation(true)
            .with_disable_built_in_roots(true);
    } else if let Some(origin) = origin {
        ssl = ssl.with_danger_tls_hostname_override(Some(origin.to_string()));
    }
    if let Some(ca) = ca {
        ssl = ssl.with_root_certs(vec![std::path::PathBuf::from(ca).into()]);
    } else if cloud_auth::method(cfg) == Some(cloud_auth::AWS) || product == Variant::AuroraMySql {
        // Amazon's RDS CAs are in no system store.
        ssl = ssl.with_root_certs(vec![cloud_auth::RDS_CA_BUNDLE.into()]);
    }
    ssl
}

fn connect_error(e: mysql_async::Error) -> Error {
    match e {
        // ER_ACCESS_DENIED_ERROR, ER_DBACCESS_DENIED_ERROR
        mysql_async::Error::Server(s) if s.code == 1045 || s.code == 1044 => {
            Error::AuthFailed(s.message)
        }
        // Classified for the app's explanation (same messages as before).
        mysql_async::Error::Server(s) => Error::connect_failed(
            dbine_driver::ConnectFailure::from_message(s.message).with_code(s.code),
        ),
        other => Error::connect_failed(dbine_driver::ConnectFailure::from_error(&other)),
    }
}

/// A statement's failure: the server's message; an interrupted statement
/// (KILL QUERY) reads as a cancellation.
pub(crate) fn err(e: mysql_async::Error) -> Error {
    match e {
        // ER_QUERY_INTERRUPTED
        mysql_async::Error::Server(s) if s.code == 1317 => Error::Cancelled,
        mysql_async::Error::Server(s) => Error::Query(s.message),
        mysql_async::Error::Io(e) => Error::Connect(e.to_string()),
        other => Error::Query(other.to_string()),
    }
}

/// How the mysql CLI reads a script: `DELIMITER`, backslash escapes, `#`
/// and `-- ` comments. Manticore has no routines (no DELIMITER, no bodies).
pub(crate) fn script_dialect(v: Variant) -> ScriptDialect {
    let d = ScriptDialect::mysql();
    if v == Variant::Manticore {
        ScriptDialect {
            delimiter_command: false,
            compound_blocks: false,
            ..d
        }
    } else {
        d
    }
}

/// A statement's failure with the server's error number, SQLSTATE and,
/// for syntax errors, where in `sql` (the text that was sent) it is.
pub(crate) fn stmt_err(sql: &str, e: mysql_async::Error) -> Error {
    match e {
        mysql_async::Error::Server(s) if s.code != 1317 => {
            let mut e = ScriptError::new(s.message.clone()).with_code(s.code.to_string());
            if !s.state.is_empty() && s.state != "HY000" {
                e = e.with_sqlstate(s.state.clone());
            }
            let (offset, line) = error_position(sql, &s.message);
            if let Some(o) = offset {
                e = e.at_offset(o);
            }
            if let Some(l) = line {
                e = e.at_line(l);
            }
            // ER_SERVER_SHUTDOWN, MariaDB's ER_CONNECTION_KILLED, MySQL's
            // ER_CLIENT_INTERACTION_TIMEOUT: the connection is gone.
            if matches!(s.code, 1053 | 1927 | 4031) {
                e = e.fatal();
            }
            e.into()
        }
        other => err(other),
    }
}

/// Where a syntax error is, from the server's message: MySQL / MariaDB say
/// `… near 'rest of the text' at line N`, TiDB `… line N column C near "…"`.
/// The offset is that of the quoted text in `sql` (the end of the text
/// when the server quotes nothing: the statement ended too soon).
pub(crate) fn error_position(sql: &str, message: &str) -> (Option<usize>, Option<u32>) {
    let digits = |s: &str| -> Option<u32> {
        let n: String = s.chars().take_while(char::is_ascii_digit).collect();
        n.parse().ok().filter(|&n| n > 0)
    };
    let (near, line, column) = if let Some(i) = message.rfind("' at line ") {
        let Some(start) = message[..i].find("near '") else {
            return (None, None);
        };
        (
            &message[start + "near '".len()..i],
            digits(&message[i + "' at line ".len()..]),
            None,
        )
    } else if let Some(i) = message
        .find("line ")
        .filter(|_| message.contains(" column "))
    {
        // TiDB: `line 1 column 13 near "t" `.
        let rest = &message[i + "line ".len()..];
        let line = digits(rest);
        let column = rest
            .find(" column ")
            .and_then(|c| digits(&rest[c + " column ".len()..]));
        let near = rest
            .find("near \"")
            .map(|n| &rest[n + "near \"".len()..])
            .map(|r| r.rfind('"').map_or(r, |q| &r[..q]))
            .unwrap_or("");
        (near, line, column)
    } else {
        return (None, None);
    };
    let Some(line) = line else {
        return (None, None);
    };
    let line_start = if line == 1 {
        0
    } else {
        match sql.match_indices('\n').nth(line as usize - 2) {
            Some((i, _)) => i + 1,
            None => return (None, Some(line)),
        }
    };
    let offset = if near.is_empty() {
        Some(sql.trim_end().len().max(line_start))
    } else if near.chars().count() < 80
        && sql.trim_end().ends_with(near)
        && sql.trim_end().len() - near.len() >= line_start
    {
        // The server quotes the rest of the statement, up to 80 characters: an uncut
        // tail sits at the end, even when the same text shows up earlier on the line.
        Some(sql.trim_end().len() - near.len())
    } else {
        // The server cuts the quoted text (at 80 characters): look for its start.
        let probe: String = near.chars().take(40).collect();
        sql[line_start..]
            .find(probe.as_str())
            .map(|i| line_start + i)
            .or_else(|| {
                // TiDB's column is where the parser stopped, past the token.
                column
                    .map(|c| (line_start + c as usize).min(sql.len()))
                    .filter(|&o| sql.is_char_boundary(o))
            })
    };
    (offset, Some(line))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::kinds;

    /// Nothing listens on the port: a classified, refused connect.
    #[tokio::test]
    async fn refused_connects_are_classified() {
        let d = drivers()
            .into_iter()
            .find(|d| d.info().id == "mysql")
            .unwrap();
        let cfg = ConnectionConfig {
            driver: "mysql".into(),
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

    #[test]
    fn ids_are_unique_and_kinds_match_capabilities() {
        let ds = drivers();
        let mut ids: Vec<_> = ds.iter().map(|d| d.info().id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), ds.len());
        let info = |id: &str| {
            ds.iter()
                .find(|d| d.info().id == id)
                .unwrap()
                .info()
                .clone()
        };
        assert_eq!(info("starrocks").default_port, 9030);
        assert_eq!(info("tidb").default_port, 4000);
        let mc = info("manticore");
        assert_eq!(mc.databases_label, "");
        assert_eq!(
            mc.object_kinds.iter().map(|k| k.id).collect::<Vec<_>>(),
            vec![kinds::TABLE]
        );
        assert!(info("mysql")
            .object_kinds
            .iter()
            .any(|k| k.id == kinds::TRIGGER));
        assert!(!info("tidb")
            .object_kinds
            .iter()
            .any(|k| k.id == kinds::PROCEDURE));
        for (id, seq) in [
            ("mariadb", true),
            ("tidb", true),
            ("oceanbase", true),
            ("databend", true),
            ("mysql", false),
            ("starrocks", false),
        ] {
            assert_eq!(
                info(id)
                    .object_kinds
                    .iter()
                    .any(|k| k.id == kinds::SEQUENCE),
                seq,
                "{id}"
            );
        }
    }

    #[test]
    fn syntax_errors_point_at_the_text() {
        let sql = "select 1,\n  frm t\nwhere x";
        let m = "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 't\nwhere x' at line 2";
        assert_eq!(
            error_position(sql, m),
            (Some(sql.find("t\nwhere").unwrap()), Some(2))
        );
        // Nothing quoted: the statement ended too soon.
        let m = "You have an error in your SQL syntax; check the manual that corresponds to your MariaDB server version for the right syntax to use near '' at line 1";
        assert_eq!(error_position("select (1  ", m), (Some(9), Some(1)));
        // Quotes inside the quoted text.
        let sql = "insert into t values ('it''s' x)";
        let m = "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'x)' at line 1";
        assert_eq!(error_position(sql, m), (Some(sql.len() - 2), Some(1)));
        let m = "[parser:1064]You have an error in your SQL syntax; check the manual that corresponds to your TiDB version for the right syntax to use line 1 column 11 near \"frm t\" ";
        assert_eq!(error_position("select 1, frm t", m), (Some(10), Some(1)));
        assert_eq!(
            error_position("select 1", "Table 'a.b' doesn't exist"),
            (None, None)
        );
        // A line past the text: the line alone.
        assert_eq!(error_position("x", "near 'y' at line 3"), (None, Some(3)));
        // A short tail that also shows up earlier on its line: the tail wins.
        let sql = "SELECT a FROM emp e\nWHERE e.id = 1 e";
        let m = "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'e' at line 2";
        assert_eq!(error_position(sql, m), (Some(sql.len() - 1), Some(2)));
        let sql = "SELECT 'x' AS x, 'x' AS y x";
        let m = "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'x' at line 1";
        assert_eq!(error_position(sql, m), (Some(sql.len() - 1), Some(1)));
    }

    #[test]
    fn server_errors_keep_number_state_and_position() {
        let server = |code: u16, state: &str, message: &str| {
            mysql_async::Error::Server(mysql_async::ServerError {
                code,
                state: state.into(),
                message: message.into(),
            })
        };
        match stmt_err("selec 1", server(1064, "42000", "You have an error in your SQL syntax; check the manual that corresponds to your MySQL server version for the right syntax to use near 'selec 1' at line 1")) {
            Error::Statement(e) => {
                assert_eq!((e.code.as_deref(), e.sqlstate.as_deref(), e.offset, e.line, e.fatal), (Some("1064"), Some("42000"), Some(0), Some(1), false));
            }
            other => panic!("{other:?}"),
        }
        match stmt_err("x", server(1146, "42S02", "Table 'd.x' doesn't exist")) {
            Error::Statement(e) => assert_eq!(
                (e.code.as_deref(), e.sqlstate.as_deref(), e.line),
                (Some("1146"), Some("42S02"), None)
            ),
            other => panic!("{other:?}"),
        }
        // HY000 says nothing; a shutdown ends the script.
        match stmt_err("x", server(1053, "HY000", "Server shutdown in progress")) {
            Error::Statement(e) => assert!(e.sqlstate.is_none() && e.fatal),
            other => panic!("{other:?}"),
        }
        assert!(matches!(
            stmt_err(
                "x",
                server(1317, "70100", "Query execution was interrupted")
            ),
            Error::Cancelled
        ));
    }

    #[test]
    fn scripts_run_statement_by_statement_and_stop_on_errors() {
        let ds = drivers();
        let get = |id: &str| ds.iter().find(|d| d.info().id == id).unwrap();
        for d in &ds {
            assert_eq!(d.script_mode(), ScriptMode::PerStatement, "{}", d.info().id);
            assert!(!d.script_defaults().continue_on_error);
            assert!(d.script_defaults().confirm_unsafe_dml);
        }
        for (id, tx) in [
            ("mysql", true),
            ("mariadb", true),
            ("tidb", true),
            ("aurora-mysql", true),
            ("starrocks", false),
            ("manticore", false),
            ("greptimedb", false),
        ] {
            assert_eq!(get(id).supports_manual_transactions(), tx, "{id}");
        }
        let script = "DELIMITER //\nCREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END//\nDELIMITER ;\nCALL p(); # done\nSELECT 'a;b'";
        let units = get("mysql").split_script(script);
        let kinds: Vec<_> = units.iter().map(|u| u.kind).collect();
        use dbine_driver::StatementKind::*;
        assert_eq!(
            kinds.iter().filter(|k| **k != ClientCommand).count(),
            3,
            "{units:?}"
        );
        assert!(
            units
                .iter()
                .any(|u| u.text == "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END"),
            "{units:?}"
        );
        // Manticore: comments don't split, no DELIMITER.
        let units = get("manticore").split_script("SELECT 1 /* ; */; -- x;\nSELECT 2");
        assert_eq!(units.len(), 2, "{units:?}");
    }

    #[test]
    fn cloud_sign_ins_match_the_product() {
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
            offered("mysql"),
            [
                cloud_auth::PASSWORD,
                cloud_auth::AWS,
                cloud_auth::GCP,
                cloud_auth::AZURE
            ]
        );
        assert_eq!(offered("mariadb"), [cloud_auth::PASSWORD, cloud_auth::AWS]);
        assert_eq!(
            offered("aurora-mysql"),
            [cloud_auth::PASSWORD, cloud_auth::AWS]
        );
        assert_eq!(
            offered("cloudsql-mysql"),
            [cloud_auth::PASSWORD, cloud_auth::GCP]
        );
        for id in ["tidb", "singlestore", "starrocks", "velodb", "manticore"] {
            assert!(offered(id).is_empty(), "{id}");
        }
    }

    #[test]
    fn managed_products_behave_as_their_engine() {
        let ds = drivers();
        let get = |id: &str| ds.iter().find(|d| d.info().id == id).unwrap();
        for (managed, base) in [
            ("aurora-mysql", "mysql"),
            ("cloudsql-mysql", "mysql"),
            ("velodb", "doris"),
        ] {
            let (m, b) = (get(managed), get(base));
            assert_eq!(m.info().default_port, b.info().default_port);
            assert_eq!(m.info().family, b.info().family);
            let kinds = |d: &Arc<dyn Driver>| {
                d.info()
                    .object_kinds
                    .iter()
                    .map(|k| k.id)
                    .collect::<Vec<_>>()
            };
            assert_eq!(kinds(m), kinds(b));
            assert_eq!(m.capabilities().foreign_keys, b.capabilities().foreign_keys);
            assert!(m.capabilities().monitor);
            assert_eq!(
                m.info()
                    .fields
                    .iter()
                    .find(|f| f.key == "encrypt")
                    .unwrap()
                    .default,
                "true"
            );
            assert!(m.designer().is_some());
        }
        assert!(get("mysql")
            .info()
            .fields
            .iter()
            .any(|f| f.key == "ca_cert"));
        assert_eq!(
            get("mysql")
                .info()
                .fields
                .iter()
                .find(|f| f.key == "encrypt")
                .unwrap()
                .default,
            ""
        );
    }

    fn tls_cfg(options: &[(&str, &str)]) -> ConnectionConfig {
        let mut cfg = ConnectionConfig {
            encrypt: true,
            ..Default::default()
        };
        for (k, v) in options {
            cfg.options.insert((*k).into(), (*v).into());
        }
        cfg
    }

    /// Whenever the certificate's name isn't checked, only the given CA
    /// anchors the chain (no system roots).
    #[test]
    fn relaxed_hostname_checks_trust_only_the_given_ca() {
        let cases = [
            (
                Variant::CloudSqlMySql,
                tls_cfg(&[("ca_cert", "/ca.pem")]),
                false,
            ),
            (
                Variant::MySql,
                tls_cfg(&[("ca_cert", "/ca.pem"), (cloud_auth::AUTH, cloud_auth::GCP)]),
                true,
            ),
            (
                Variant::MySql,
                tls_cfg(&[
                    ("ca_cert", "/ca.pem"),
                    (cloud_auth::AUTH, cloud_auth::GCP),
                    (cloud_auth::ORIGIN_HOST, "10.0.0.5"),
                ]),
                true,
            ),
        ];
        for (v, cfg, cloud) in cases {
            let ssl = ssl_opts(v, &cfg, cloud);
            assert!(ssl.skip_domain_validation(), "{v:?}");
            assert!(ssl.disable_built_in_roots(), "{v:?}");
            assert!(!ssl.accept_invalid_certs(), "{v:?}");
            assert_eq!(ssl.root_certs().len(), 1, "{v:?}");
        }
    }

    /// Through an SSH tunnel the client dials 127.0.0.1, but the name
    /// checked is the real endpoint's.
    #[test]
    fn a_tunneled_cloud_sign_in_checks_the_real_endpoint_name() {
        for auth in [cloud_auth::AWS, cloud_auth::AZURE, cloud_auth::GCP] {
            let cfg = tls_cfg(&[
                (cloud_auth::AUTH, auth),
                (cloud_auth::ORIGIN_HOST, " db.example.com "),
            ]);
            let ssl = ssl_opts(Variant::MySql, &cfg, true);
            assert!(!ssl.skip_domain_validation(), "{auth}");
            assert_eq!(
                ssl.tls_hostname_override(),
                Some("db.example.com"),
                "{auth}"
            );
        }
        // AWS keeps Amazon's RDS roots (on top of the system's: the name is checked).
        let cfg = tls_cfg(&[
            (cloud_auth::AUTH, cloud_auth::AWS),
            (cloud_auth::ORIGIN_HOST, "db.rds.amazonaws.com"),
        ]);
        assert_eq!(ssl_opts(Variant::MySql, &cfg, true).root_certs().len(), 1);
    }

    #[test]
    fn plain_tls_checks_the_name_with_the_system_roots() {
        for (v, cfg) in [
            (Variant::MySql, tls_cfg(&[])),
            (Variant::MySql, tls_cfg(&[("ca_cert", "/ca.pem")])),
            (Variant::AuroraMySql, tls_cfg(&[])),
            // Without a cloud sign-in the tunnel option says nothing.
            (
                Variant::MySql,
                tls_cfg(&[(cloud_auth::ORIGIN_HOST, "db.example.com")]),
            ),
        ] {
            let ssl = ssl_opts(v, &cfg, false);
            assert!(
                !ssl.skip_domain_validation()
                    && !ssl.disable_built_in_roots()
                    && !ssl.accept_invalid_certs(),
                "{v:?}"
            );
            assert_eq!(ssl.tls_hostname_override(), None, "{v:?}");
        }
        // "Trust the server certificate" is the user's explicit choice.
        let cfg = ConnectionConfig {
            trust_server_certificate: true,
            ..tls_cfg(&[])
        };
        let ssl = ssl_opts(Variant::MySql, &cfg, false);
        assert!(ssl.skip_domain_validation() && ssl.accept_invalid_certs());
    }
}
