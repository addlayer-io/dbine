//! MySQL and the engines that speak its protocol (MariaDB, TiDB,
//! OceanBase, SingleStore, StarRocks, Doris, Databend, Manticore Search,
//! GreptimeDB) and the managed services built on them (Amazon Aurora
//! MySQL, Cloud SQL for MySQL, VeloDB), through mysql_async on a single connection (no pool).
//! Catalog queries use the text protocol with escaped literals because
//! several of these engines don't support prepared statements.

mod backup;
mod blocking;
mod cells;
mod design;
mod monitor;
mod permissions;
mod plan;
mod profiler;
mod security;
mod session;
mod structure;
mod transfer;

use dbine_driver::{
    async_trait, Capabilities, ConnectionConfig, CreateTemplate, DdlParts, DesignerSpec, Driver, DriverInfo, Error,
    Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, Result, Session, TableSchema,
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
        !matches!(self, Variant::Databend | Variant::Manticore | Variant::GreptimeDb)
    }

    /// Reports who waits on whose locks (`Session::blocking`) and ends
    /// sessions with KILL: InnoDB's lock waits, TiDB's DATA_LOCK_WAITS.
    pub(crate) fn has_lock_waits(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb | Variant::TiDb)
    }

    /// Stored procedures and functions in information_schema.ROUTINES.
    pub(crate) fn has_routines(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb | Variant::OceanBase | Variant::SingleStore)
    }

    /// Enforced foreign keys (TiDB since 6.6).
    pub(crate) fn has_foreign_keys(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase)
    }

    pub(crate) fn has_triggers(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb | Variant::OceanBase)
    }

    /// `CREATE SEQUENCE` objects (MariaDB 10.3+, TiDB, OceanBase, Databend).
    pub(crate) fn has_sequences(self) -> bool {
        matches!(self, Variant::MariaDb | Variant::TiDb | Variant::OceanBase | Variant::Databend)
    }

    /// CHECK constraints (MySQL 8.0.16+, MariaDB, OceanBase, TiDB 7.2+).
    pub(crate) fn has_checks(self) -> bool {
        matches!(self, Variant::MySql | Variant::MariaDb | Variant::TiDb | Variant::OceanBase)
    }

    /// A single namespace: no databases to pick.
    pub(crate) fn single_namespace(self) -> bool {
        self == Variant::Manticore
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
            Variant::CloudSqlMySql => ("cloudsql-mysql", "Cloud SQL para MySQL", Family::Relational),
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
            Field::server_set().into_iter().filter(|f| f.key != "database").collect()
        } else {
            Field::server_set()
        };
        // A CA bundle for servers whose certificate isn't in the system store
        // (Amazon RDS, Cloud SQL, self-signed…).
        let at = fields.iter().position(|f| f.key == "trust_server_certificate").map_or(fields.len(), |i| i + 1);
        fields.insert(
            at,
            Field::new("ca_cert", "Certificado de la CA (PEM)", FieldKind::File)
                .help("Opcional, con TLS: el certificado raíz que firma el del servidor.")
                .ssl(),
        );
        managed_fields(product, &mut fields);
        DriverInfo {
            id,
            name,
            family,
            language: Language::Sql,
            dialect: "mysql",
            default_port: v.default_port(),
            fields,
            databases_label: if v.single_namespace() { "" } else { "Bases de datos" },
            has_schemas: false,
            object_kinds,
        }
    }
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    Variant::ALL
        .iter()
        .map(|&p| Arc::new(MySqlDriver { variant: p.base(), product: p, info: Variant::info(p) }) as Arc<dyn Driver>)
        .collect()
}

/// Managed services: TLS on by default, a CA bundle and IAM-token help.
fn managed_fields(product: Variant, fields: &mut [Field]) {
    let (encrypt_help, password_help) = match product {
        Variant::AuroraMySql => (
            "Aurora acepta TLS; para verificar el certificado, indicá el paquete de CA de Amazon RDS (global-bundle.pem).",
            "Contraseña o token de autenticación IAM (aws rds generate-db-auth-token); el token exige TLS.",
        ),
        Variant::CloudSqlMySql => (
            "Cloud SQL puede exigir TLS; indicá el server-ca.pem de la instancia para verificarlo. Los certificados de cliente no están soportados: usá el Cloud SQL Auth Proxy si la instancia los exige.",
            "Contraseña o token de IAM (gcloud sql generate-login-token); el token exige TLS.",
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

    /// `LOAD DATA LOCAL INFILE` or big multi-row INSERTs (see `transfer`);
    /// Manticore uses the INSERT script.
    fn supports_bulk_load(&self) -> bool {
        transfer::supported(self.variant)
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        security::supported(self.variant).then(|| security::spec_for(self.variant))
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        if !security::supported(self.variant) {
            return Err(Error::Unsupported("este motor no administra usuarios desde DBine".into()));
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

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        design::sync_script(self.variant, changes)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        Ok(design::insert_script(self.variant, target, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        design::update_script(self.variant, target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        design::delete_script(self.variant, target, keys)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        design::filtered_browse(self.variant, browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let v = self.variant;
        let db = database
            .or(Some(cfg.database.as_str()))
            .filter(|d| !d.is_empty() && !v.single_namespace())
            .map(str::to_string);
        let mut builder = OptsBuilder::default()
            .ip_or_hostname(if cfg.host.is_empty() { "localhost" } else { cfg.host.as_str() })
            .tcp_port(cfg.port_or(v.default_port()))
            .user(cfg.username.as_deref().filter(|u| !u.is_empty()))
            .pass(cfg.password.as_deref())
            .db_name(db.clone())
            .prefer_socket(false);
        if !v.is_mysql_server() {
            // Skip mysql_async's `SELECT @@max_allowed_packet, @@wait_timeout`,
            // which not every emulation answers.
            builder = builder.max_allowed_packet(Some(64 * 1024 * 1024)).wait_timeout(Some(28_800));
        }
        if cfg.encrypt {
            let mut ssl = SslOpts::default()
                .with_danger_accept_invalid_certs(cfg.trust_server_certificate)
                .with_danger_skip_domain_validation(cfg.trust_server_certificate);
            if let Some(ca) = cfg.option("ca_cert").map(str::trim).filter(|p| !p.is_empty()) {
                ssl = ssl.with_root_certs(vec![std::path::PathBuf::from(ca).into()]);
            }
            builder = builder.ssl_opts(ssl);
            // IAM tokens go as mysql_clear_password, only over TLS.
            if self.product.iam_tokens() {
                builder = builder.enable_cleartext_plugin(true);
            }
        }
        let opts = Opts::from(builder);

        let mut conn = tokio::time::timeout(Duration::from_secs(20), Conn::new(opts.clone()))
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
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
        Ok(Box::new(MySqlSession::new(conn, opts, self.product, db)))
    }
}

fn connect_error(e: mysql_async::Error) -> Error {
    match e {
        // ER_ACCESS_DENIED_ERROR, ER_DBACCESS_DENIED_ERROR
        mysql_async::Error::Server(s) if s.code == 1045 || s.code == 1044 => Error::AuthFailed(s.message),
        mysql_async::Error::Server(s) => Error::Connect(s.message),
        other => Error::Connect(other.to_string()),
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

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::kinds;

    #[test]
    fn ids_are_unique_and_kinds_match_capabilities() {
        let ds = drivers();
        let mut ids: Vec<_> = ds.iter().map(|d| d.info().id).collect();
        ids.sort();
        ids.dedup();
        assert_eq!(ids.len(), ds.len());
        let info = |id: &str| ds.iter().find(|d| d.info().id == id).unwrap().info().clone();
        assert_eq!(info("starrocks").default_port, 9030);
        assert_eq!(info("tidb").default_port, 4000);
        let mc = info("manticore");
        assert_eq!(mc.databases_label, "");
        assert_eq!(mc.object_kinds.iter().map(|k| k.id).collect::<Vec<_>>(), vec![kinds::TABLE]);
        assert!(info("mysql").object_kinds.iter().any(|k| k.id == kinds::TRIGGER));
        assert!(!info("tidb").object_kinds.iter().any(|k| k.id == kinds::PROCEDURE));
        for (id, seq) in [("mariadb", true), ("tidb", true), ("oceanbase", true), ("databend", true), ("mysql", false), ("starrocks", false)] {
            assert_eq!(info(id).object_kinds.iter().any(|k| k.id == kinds::SEQUENCE), seq, "{id}");
        }
    }

    #[test]
    fn managed_products_behave_as_their_engine() {
        let ds = drivers();
        let get = |id: &str| ds.iter().find(|d| d.info().id == id).unwrap();
        for (managed, base) in [("aurora-mysql", "mysql"), ("cloudsql-mysql", "mysql"), ("velodb", "doris")] {
            let (m, b) = (get(managed), get(base));
            assert_eq!(m.info().default_port, b.info().default_port);
            assert_eq!(m.info().family, b.info().family);
            let kinds = |d: &Arc<dyn Driver>| d.info().object_kinds.iter().map(|k| k.id).collect::<Vec<_>>();
            assert_eq!(kinds(m), kinds(b));
            assert_eq!(m.capabilities().foreign_keys, b.capabilities().foreign_keys);
            assert!(m.capabilities().monitor);
            assert_eq!(m.info().fields.iter().find(|f| f.key == "encrypt").unwrap().default, "true");
            assert!(m.designer().is_some());
        }
        assert!(get("mysql").info().fields.iter().any(|f| f.key == "ca_cert"));
        assert_eq!(get("mysql").info().fields.iter().find(|f| f.key == "encrypt").unwrap().default, "");
    }
}
