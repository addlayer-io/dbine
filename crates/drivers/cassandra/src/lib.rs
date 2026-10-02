//! Apache Cassandra and ScyllaDB over the CQL native protocol, with the
//! `scylla` crate. A session is one driver session (one connection per
//! node) with the keyspace selected; the explorer reads `system_schema`.
//! Scripts are split on `;` as cqlsh does (batches stay whole), with
//! cqlsh's `CONSISTENCY`, `SERIAL CONSISTENCY` and `PAGING` commands, each
//! on its line, kept for the rest of the session; every `SELECT` is paged
//! until `max_rows`. Server errors carry their code and position.

mod backup;
mod cql;
mod ddl;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod profiler;
mod security;
mod steps;
mod sync;
mod transfer;
mod value;

use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, IndexDef, KeyDef, Language, MonitorSnapshot,
    ObjectKindInfo, ObjectRef, Plan, QueryOutcome, Result, ResultColumn, ScriptError, Session as DbSession, TableSchema,
};
use scylla::errors::{ExecutionError, RequestAttemptError};
use scylla::frame::types::{Consistency, SerialConsistency};
use std::collections::BTreeMap;
use steps::Step;
use scylla::observability::tracing::TracingInfo;
use scylla::client::session::Session;
use scylla::client::session_builder::SessionBuilder;
use scylla::client::PoolSize;
use scylla::errors::TranslationError;
use scylla::policies::address_translator::{AddressTranslator, UntranslatedPeer};
use scylla::response::{PagingState, PagingStateResponse};
use scylla::serialize::row::SerializeRow;
use scylla::statement::Statement;
use scylla::value::{CqlValue, Row};
use std::net::SocketAddr;
use std::num::NonZeroUsize;
use std::sync::Arc;
use std::time::Duration;

/// Object kind for user-defined types.
const TYPE: &str = "type";

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(CassandraDriver { info: info("cassandra", "Apache Cassandra"), flavor: Flavor::Cassandra }),
        Arc::new(CassandraDriver { info: info("scylladb", "ScyllaDB"), flavor: Flavor::Scylla }),
        Arc::new(CassandraDriver { info: keyspaces_info(), flavor: Flavor::Keyspaces }),
    ]
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Flavor {
    Cassandra,
    Scylla,
    /// Amazon Keyspaces: TLS on 9142, service-specific credentials
    /// (plain-text auth). SigV4 would need a custom authenticator plus
    /// the AWS SDK's credential chain; the `scylla` crate has neither.
    Keyspaces,
}

fn keyspaces_info() -> DriverInfo {
    DriverInfo {
        id: "keyspaces",
        name: "Amazon Keyspaces",
        family: Family::WideColumn,
        language: Language::Cql,
        dialect: "",
        default_port: 9142,
        fields: vec![
            Field::host()
                .placeholder("cassandra.us-east-1.amazonaws.com")
                .help("El endpoint de la región: cassandra.<región>.amazonaws.com."),
            Field::port().placeholder("9142"),
            Field::new("database", "Keyspace", FieldKind::Text).placeholder("(ninguno)"),
            Field::username().required().help("Usuario de las credenciales específicas del servicio (IAM → Credenciales de Amazon Keyspaces)."),
            Field::password().required().help("Contraseña de las credenciales específicas del servicio."),
            Field::new("datacenter", "Región (datacenter)", FieldKind::Text)
                .placeholder("us-east-1")
                .help("En Keyspaces el datacenter es la región.")
                .advanced(),
            Field::encrypt().default_value("true").help("Keyspaces solo acepta conexiones TLS."),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Keyspaces",
        has_schemas: false,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::new(TYPE, "Tipos", true, false, true)],
    }
}

fn info(id: &'static str, name: &'static str) -> DriverInfo {
    DriverInfo {
        id,
        name,
        family: Family::WideColumn,
        language: Language::Cql,
        dialect: "",
        default_port: 9042,
        fields: vec![
            Field::host().placeholder("node1, node2…").help("Uno o más nodos de contacto, separados por comas."),
            Field::port().placeholder("9042"),
            Field::new("database", "Keyspace", FieldKind::Text).placeholder("(ninguno)"),
            Field::username(),
            Field::password(),
            Field::new("datacenter", "Datacenter local", FieldKind::Text)
                .placeholder("(cualquiera)")
                .help("Nodos preferidos para las consultas, p. ej. datacenter1.")
                .advanced(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Keyspaces",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::materialized_views(),
            ObjectKindInfo::new(TYPE, "Tipos", true, false, true),
            ObjectKindInfo::functions(),
        ],
    }
}

pub struct CassandraDriver {
    info: DriverInfo,
    flavor: Flavor,
}

#[async_trait]
impl Driver for CassandraDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_profiler(&self) -> bool {
        profiler::supported(self.flavor)
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// The primary key and the secondary indexes, without counters (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    /// For "run the statement at the cursor": `$$` bodies, and no `BEGIN …
    /// END` blocks (a CQL batch ends with `APPLY BATCH`). Editor scripts
    /// are split by the driver itself, as cqlsh does.
    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        dbine_driver::ScriptDialect { dollar_quotes: true, backtick_idents: false, compound_blocks: false, ..dbine_driver::ScriptDialect::generic() }
    }

    /// As cqlsh -f: a failed statement doesn't stop the script (the tab's
    /// toggle overrides it).
    fn script_defaults(&self) -> dbine_driver::ScriptDefaults {
        dbine_driver::ScriptDefaults { continue_on_error: true, ..dbine_driver::ScriptDefaults::for_language(self.info().language) }
    }

    /// Keyspaces are created and dropped; CQL has no foreign keys.
    fn capabilities(&self) -> Capabilities {
        Capabilities { create_database: true, drop_database: true, foreign_keys: false, monitor: true, ..Default::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer(self.info.id))
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates(self.info.id)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    /// Prepared `INSERT`s, many in flight (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(self.flavor == Flavor::Keyspaces, changes)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<serde_json::Value>]) -> Result<String> {
        ddl::insert_script(target, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::update_script(target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, serde_json::Value)>]) -> Result<String> {
        ddl::delete_script(target, keys)
    }

    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        security::spec(self.flavor)
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(self.flavor, action)
    }

    fn backup(&self) -> Option<dbine_driver::BackupSpec> {
        backup::spec(self.flavor)
    }

    fn backup_script(&self, action: &dbine_driver::BackupAction) -> Result<String> {
        backup::script(self.flavor, action)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn DbSession>> {
        let port = cfg.port_or(self.info.default_port);
        let nodes: Vec<String> = cfg
            .host
            .split(',')
            .map(str::trim)
            .filter(|h| !h.is_empty())
            .map(|h| if has_port(h) { h.to_string() } else { format!("{h}:{port}") })
            .collect();
        let nodes = if nodes.is_empty() { vec![format!("localhost:{port}")] } else { nodes };
        let keyspace = database.filter(|d| !d.is_empty()).or(Some(cfg.database.as_str()).filter(|d| !d.is_empty()));

        let mut b = SessionBuilder::new()
            .known_nodes(&nodes)
            .connection_timeout(Duration::from_secs(15))
            .pool_size(PoolSize::PerHost(NonZeroUsize::MIN))
            // The explorer reads system_schema itself.
            .fetch_schema_metadata(false);
        if let Some(u) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
            b = b.user(u, cfg.password_or_empty());
        }
        if let Some(dc) = cfg.option("datacenter") {
            b = b.prefer_datacenter(dc.to_string());
        }
        // Keyspaces only takes TLS.
        if cfg.encrypt || self.flavor == Flavor::Keyspaces {
            b = b.tls_context(Some(tls_config(cfg.trust_server_certificate)?));
        }
        if let Some(ks) = keyspace {
            b = b.use_keyspace(ks, true);
        }
        if let Ok(Some(first)) = tokio::net::lookup_host(nodes[0].as_str()).await.map(|mut a| a.next()) {
            b = b.address_translator(Arc::new(NatTranslator { fallback: first }));
        }
        let session = tokio::time::timeout(Duration::from_secs(20), b.build())
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
            .map_err(|e| connect_err(e.to_string()))?;
        Ok(Box::new(CassandraSession {
            session,
            keyspace: keyspace.map(str::to_string),
            read_only: cfg.read_only,
            flavor: self.flavor,
            user: cfg.username.clone().filter(|u| !u.is_empty()),
            profiler: None,
            consistency: None,
            serial: None,
            paging: Paging::Default,
        }))
    }
}

/// Nodes advertise the address they see (a container's or a private
/// network's); when that isn't reachable from here (Docker, Kubernetes,
/// NAT), connect through the first contact point instead.
struct NatTranslator {
    fallback: SocketAddr,
}

#[async_trait]
impl AddressTranslator for NatTranslator {
    async fn translate_address(&self, peer: &UntranslatedPeer) -> std::result::Result<SocketAddr, TranslationError> {
        let addr = peer.untranslated_address();
        let probe = tokio::time::timeout(Duration::from_millis(1500), tokio::net::TcpStream::connect(addr)).await;
        Ok(if matches!(probe, Ok(Ok(_))) { addr } else { self.fallback })
    }
}

/// `host:port`, `[v6]:port` (a bare IPv6 address has colons but no port).
fn has_port(h: &str) -> bool {
    if h.starts_with('[') {
        return h.contains("]:");
    }
    h.matches(':').count() == 1
}

fn connect_err(msg: String) -> Error {
    let l = msg.to_lowercase();
    if l.contains("authenticat") || l.contains("bad credentials") || l.contains("username and/or password") {
        Error::AuthFailed(msg)
    } else {
        Error::Connect(msg)
    }
}

fn tls_config(trust_any: bool) -> Result<Arc<rustls::ClientConfig>> {
    let provider = Arc::new(rustls::crypto::ring::default_provider());
    let builder = rustls::ClientConfig::builder_with_provider(provider.clone())
        .with_safe_default_protocol_versions()
        .map_err(|e| Error::Connect(format!("TLS: {e}")))?;
    let config = if trust_any {
        builder
            .dangerous()
            .with_custom_certificate_verifier(Arc::new(AcceptAny(provider)))
            .with_no_client_auth()
    } else {
        let mut roots = rustls::RootCertStore::empty();
        for cert in rustls_native_certs::load_native_certs().certs {
            let _ = roots.add(cert);
        }
        builder.with_root_certificates(roots).with_no_client_auth()
    };
    Ok(Arc::new(config))
}

/// "Trust the server certificate": any certificate, signatures still checked.
#[derive(Debug)]
struct AcceptAny(Arc<rustls::crypto::CryptoProvider>);

impl rustls::client::danger::ServerCertVerifier for AcceptAny {
    fn verify_server_cert(
        &self,
        _: &rustls::pki_types::CertificateDer<'_>,
        _: &[rustls::pki_types::CertificateDer<'_>],
        _: &rustls::pki_types::ServerName<'_>,
        _: &[u8],
        _: rustls::pki_types::UnixTime,
    ) -> std::result::Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &rustls::pki_types::CertificateDer<'_>,
        dss: &rustls::DigitallySignedStruct,
    ) -> std::result::Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(message, cert, dss, &self.0.signature_verification_algorithms)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.0.signature_verification_algorithms.supported_schemes()
    }
}

pub struct CassandraSession {
    session: Session,
    keyspace: Option<String>,
    read_only: bool,
    flavor: Flavor,
    /// The login's role name (`None`: anonymous), for `permissions`.
    user: Option<String>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// cqlsh's `CONSISTENCY` / `SERIAL CONSISTENCY` for the editor's
    /// statements (`None`: the driver's default, LOCAL_QUORUM).
    consistency: Option<Consistency>,
    serial: Option<SerialConsistency>,
    /// cqlsh's `PAGING`.
    paging: Paging,
}

/// Page size of the editor's SELECTs (cqlsh `PAGING`).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Paging {
    /// From the row limit (100 to 5000 rows a page).
    Default,
    /// `PAGING OFF`: as few pages as possible.
    Off,
    /// `PAGING n`.
    Size(i32),
}

const CONSISTENCIES: &[(&str, Consistency)] = &[
    ("ANY", Consistency::Any),
    ("ONE", Consistency::One),
    ("TWO", Consistency::Two),
    ("THREE", Consistency::Three),
    ("QUORUM", Consistency::Quorum),
    ("ALL", Consistency::All),
    ("LOCAL_QUORUM", Consistency::LocalQuorum),
    ("EACH_QUORUM", Consistency::EachQuorum),
    ("LOCAL_ONE", Consistency::LocalOne),
    ("SERIAL", Consistency::Serial),
    ("LOCAL_SERIAL", Consistency::LocalSerial),
];

/// A failed statement with the server's error code (hex, as cqlsh shows
/// it: 2000 syntax, 2200 invalid…) and, from `line L:C` in the message,
/// its place in `stmt`.
fn cql_err(e: ExecutionError, stmt: &str) -> Error {
    let ExecutionError::LastAttemptError(RequestAttemptError::DbError(db, msg)) = &e else {
        return Error::Query(e.to_string());
    };
    let code = db.code(&scylla::frame::protocol_features::ProtocolFeatures::default());
    let mut se = ScriptError::new(msg.clone()).with_code(format!("{code:04X}"));
    if let Some((line, col)) = position(msg) {
        se = se.at_line(line).at_offset(steps::offset_of(stmt, line, col + 1));
    }
    Error::Statement(Box::new(se))
}

/// `line 1:7 no viable alternative…`: line (1-based) and column (0-based).
fn position(msg: &str) -> Option<(u32, u32)> {
    let rest = &msg[msg.find("line ")? + 5..];
    let (l, rest) = rest.split_once(':')?;
    let c: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((l.trim().parse().ok()?, c.parse().ok()?))
}
fn is_system_keyspace(name: &str) -> bool {
    name == "system" || name.starts_with("system_") || matches!(name, "data_endpoint_auth" | "datastax_sla")
}

fn text(row: &Row, i: usize) -> String {
    match row.columns.get(i) {
        Some(Some(CqlValue::Text(s) | CqlValue::Ascii(s))) => s.clone(),
        Some(Some(v)) => value::to_json(v).as_str().map(str::to_string).unwrap_or_else(|| value::to_json(v).to_string()),
        _ => String::new(),
    }
}

fn texts(row: &Row, i: usize) -> Vec<String> {
    match row.columns.get(i) {
        Some(Some(CqlValue::List(items) | CqlValue::Set(items))) => items
            .iter()
            .filter_map(|v| match v {
                CqlValue::Text(s) | CqlValue::Ascii(s) => Some(s.clone()),
                _ => None,
            })
            .collect(),
        _ => Vec::new(),
    }
}

fn int(row: &Row, i: usize) -> i64 {
    match row.columns.get(i) {
        Some(Some(CqlValue::Int(n))) => i64::from(*n),
        Some(Some(CqlValue::BigInt(n))) => *n,
        _ => 0,
    }
}

/// A value of a `map<text, text>` column.
fn map_value(row: &Row, i: usize, key: &str) -> Option<String> {
    let Some(Some(CqlValue::Map(m))) = row.columns.get(i) else { return None };
    m.iter().find_map(|(k, v)| match (k, v) {
        (CqlValue::Text(k), CqlValue::Text(v)) if k == key => Some(v.clone()),
        _ => None,
    })
}

/// The entries of a `map<text, text>` column.
fn map_entries(row: &Row, i: usize) -> Vec<(String, String)> {
    let Some(Some(CqlValue::Map(m))) = row.columns.get(i) else { return Vec::new() };
    m.iter()
        .filter_map(|(k, v)| match (k, v) {
            (CqlValue::Text(k), CqlValue::Text(v)) => Some((k.clone(), v.clone())),
            _ => None,
        })
        .collect()
}

fn boolean(row: &Row, i: usize) -> bool {
    matches!(row.columns.get(i), Some(Some(CqlValue::Boolean(true))))
}

pub(crate) struct Column {
    name: String,
    typ: String,
    kind: String,
    position: i64,
    desc: bool,
}

impl CassandraSession {
    /// A catalog query, all rows at once.
    async fn rows(&self, cql: &str, values: impl SerializeRow) -> Result<Vec<Row>> {
        let res = self.session.query_unpaged(cql, values).await.map_err(|e| Error::Query(e.to_string()))?;
        let rows = res.into_rows_result().map_err(Error::query)?;
        let out = rows.rows::<Row>().map_err(Error::query)?.collect::<std::result::Result<Vec<_>, _>>();
        out.map_err(Error::query)
    }

    fn ks(&self, obj: &ObjectRef) -> Result<String> {
        obj.schema()
            .map(str::to_string)
            .or_else(|| self.keyspace.clone())
            .ok_or_else(|| Error::Query("No hay un keyspace seleccionado.".into()))
    }

    /// Columns of a table or view, in CQL key order: partition key,
    /// clustering columns, then the rest by name.
    async fn table_columns(&self, ks: &str, table: &str) -> Result<Vec<Column>> {
        let rows = self
            .rows(
                "SELECT column_name, type, kind, position, clustering_order FROM system_schema.columns \
                 WHERE keyspace_name = ? AND table_name = ?",
                (ks, table),
            )
            .await?;
        let mut cols: Vec<Column> = rows
            .iter()
            .map(|r| Column {
                name: text(r, 0),
                typ: text(r, 1),
                kind: text(r, 2),
                position: int(r, 3),
                desc: text(r, 4).eq_ignore_ascii_case("desc"),
            })
            .collect();
        let rank = |k: &str| match k {
            "partition_key" => 0,
            "clustering" => 1,
            "static" => 2,
            _ => 3,
        };
        cols.sort_by(|a, b| (rank(&a.kind), a.position, &a.name).cmp(&(rank(&b.kind), b.position, &b.name)));
        Ok(cols)
    }

    /// The server's own `DESCRIBE` (Cassandra 4+, recent Scylla).
    async fn describe(&self, what: &str, ks: &str, name: &str) -> Option<String> {
        let cql = format!("DESCRIBE {what} {}", cql::qualified(Some(ks), name));
        let rows = self.rows(&cql, ()).await.ok()?;
        let parts: Vec<String> = rows.iter().map(|r| text(r, 3)).filter(|s| !s.is_empty()).collect();
        (!parts.is_empty()).then(|| parts.join("\n\n"))
    }

    async fn rebuild_table(&self, ks: &str, name: &str, view: bool) -> Result<Option<String>> {
        let cols = self.table_columns(ks, name).await?;
        if cols.is_empty() {
            return Ok(None);
        }
        let key_list = |kind: &str| -> Vec<&Column> { cols.iter().filter(|c| c.kind == kind).collect() };
        let pk = key_list("partition_key");
        let ck = key_list("clustering");
        let pk_text = if pk.len() == 1 {
            cql::ident(&pk[0].name)
        } else {
            format!("({})", pk.iter().map(|c| cql::ident(&c.name)).collect::<Vec<_>>().join(", "))
        };
        let key = std::iter::once(pk_text).chain(ck.iter().map(|c| cql::ident(&c.name))).collect::<Vec<_>>().join(", ");
        let order = if ck.iter().any(|c| c.desc) {
            let o = ck.iter().map(|c| format!("{} {}", cql::ident(&c.name), if c.desc { "DESC" } else { "ASC" }));
            format!("\nWITH CLUSTERING ORDER BY ({})", o.collect::<Vec<_>>().join(", "))
        } else {
            String::new()
        };
        let q = cql::qualified(Some(ks), name);
        if view {
            let rows = self
                .rows(
                    "SELECT base_table_name, where_clause, include_all_columns FROM system_schema.views \
                     WHERE keyspace_name = ? AND view_name = ?",
                    (ks, name),
                )
                .await?;
            let Some(r) = rows.first() else { return Ok(None) };
            let select = if boolean(r, 2) {
                "*".to_string()
            } else {
                cols.iter().map(|c| cql::ident(&c.name)).collect::<Vec<_>>().join(", ")
            };
            return Ok(Some(format!(
                "CREATE MATERIALIZED VIEW {q} AS\nSELECT {select}\nFROM {}\nWHERE {}\nPRIMARY KEY ({key}){order};",
                cql::qualified(Some(ks), &text(r, 0)),
                text(r, 1),
            )));
        }
        let mut lines: Vec<String> = cols
            .iter()
            .map(|c| format!("    {} {}{}", cql::ident(&c.name), c.typ, if c.kind == "static" { " STATIC" } else { "" }))
            .collect();
        lines.push(format!("    PRIMARY KEY ({key})"));
        Ok(Some(format!("CREATE TABLE {q} (\n{}\n){order};", lines.join(",\n"))))
    }

    /// A cqlsh command: `CONSISTENCY [level]`, `SERIAL CONSISTENCY
    /// [level]`, `PAGING [ON|OFF|n]`; the rest are refused.
    fn shell(&mut self, line: &str, out: &mut QueryOutcome) -> Result<()> {
        let words: Vec<String> = line.split_whitespace().map(str::to_ascii_uppercase).collect();
        let w = |i: usize| words.get(i).map(String::as_str).unwrap_or("");
        let name = |c: Consistency| CONSISTENCIES.iter().find(|(_, x)| *x == c).map_or("?", |(n, _)| n);
        match (w(0), w(1)) {
            ("CONSISTENCY", "") => out.info(format!("Nivel de consistencia actual: {}.", name(self.consistency.unwrap_or(Consistency::LocalQuorum)))),
            ("CONSISTENCY", level) => {
                let Some((n, c)) = CONSISTENCIES.iter().find(|(n, _)| *n == level) else {
                    return Err(Error::Query(format!("Nivel de consistencia desconocido: {level}.")));
                };
                self.consistency = Some(*c);
                out.info(format!("Nivel de consistencia: {n}."));
            }
            ("SERIAL", "CONSISTENCY") => match w(2) {
                "" => out.info(format!(
                    "Consistencia serial actual: {}.",
                    if self.serial == Some(SerialConsistency::Serial) { "SERIAL" } else { "LOCAL_SERIAL" }
                )),
                "SERIAL" => {
                    self.serial = Some(SerialConsistency::Serial);
                    out.info("Consistencia serial: SERIAL.");
                }
                "LOCAL_SERIAL" => {
                    self.serial = Some(SerialConsistency::LocalSerial);
                    out.info("Consistencia serial: LOCAL_SERIAL.");
                }
                other => return Err(Error::Query(format!("Consistencia serial desconocida: {other} (SERIAL o LOCAL_SERIAL)."))),
            },
            ("PAGING", arg) => {
                self.paging = match arg {
                    "" => {
                        out.info(match self.paging {
                            Paging::Default => "Paginación activada.".to_string(),
                            Paging::Off => "Paginación desactivada.".to_string(),
                            Paging::Size(n) => format!("Paginación activada, de a {n} filas."),
                        });
                        return Ok(());
                    }
                    "ON" => Paging::Default,
                    "OFF" => Paging::Off,
                    n => match n.parse::<i32>() {
                        Ok(n) if n > 0 => Paging::Size(n),
                        _ => return Err(Error::Query(format!("PAGING: «{n}» no es ON, OFF ni un tamaño de página."))),
                    },
                };
                out.info(match self.paging {
                    Paging::Off => "Paginación desactivada.".to_string(),
                    Paging::Size(n) => format!("Tamaño de página: {n}."),
                    Paging::Default => "Paginación activada.".to_string(),
                });
            }
            ("TRACING", _) => return Err(Error::Unsupported("TRACING no está en DBine: «Ejecutar + plan» muestra el trace de la consulta.".into())),
            (other, _) => {
                return Err(Error::Unsupported(format!(
                    "{other} es un comando de cqlsh que DBine no tiene (sí CONSISTENCY, SERIAL CONSISTENCY y PAGING)."
                )))
            }
        }
        Ok(())
    }

    /// Run one statement, paging a SELECT until `max_rows`.
    async fn run(&self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.run_traced(stmt, max_rows, out, false).await.map(|_| ())
    }

    /// [`Self::run`], with the server's trace of the first page when asked.
    async fn run_traced(&self, stmt: &str, max_rows: usize, out: &mut QueryOutcome, trace: bool) -> Result<Option<TracingInfo>> {
        let page = match self.paging {
            Paging::Default => max_rows.clamp(100, 5000) as i32,
            Paging::Off => max_rows.saturating_add(1).clamp(5000, i32::MAX as usize) as i32,
            Paging::Size(n) => n,
        };
        let mut statement = Statement::new(stmt).with_page_size(page);
        statement.set_request_timeout(Some(Duration::from_secs(300)));
        statement.set_tracing(trace);
        if let Some(c) = self.consistency {
            statement.set_consistency(c);
        }
        if self.serial.is_some() {
            statement.set_serial_consistency(self.serial);
        }
        let mut paging = PagingState::start();
        let mut started = false;
        let mut trace_id = None;
        loop {
            let (res, next) = self
                .session
                .query_single_page(statement.clone(), (), paging)
                .await
                .map_err(|e| cql_err(e, stmt))?;
            if trace && trace_id.is_none() {
                trace_id = res.tracing_id();
                statement.set_tracing(false);
            }
            for w in res.warnings() {
                out.warning(w);
            }
            if !res.is_rows() {
                // CQL doesn't count what a write or DDL changed: "done",
                // not "0 rows affected" (cqlsh prints nothing).
                out.results.push(dbine_driver::StatementResult::default());
                break;
            }
            let rows = res.into_rows_result().map_err(Error::query)?;
            if !started {
                out.begin_result(
                    rows.column_specs()
                        .iter()
                        .map(|c| ResultColumn { name: c.name().to_string(), type_name: type_name(c.typ()) })
                        .collect(),
                );
                started = true;
            }
            for row in rows.rows::<Row>().map_err(Error::query)? {
                let row = row.map_err(Error::query)?;
                out.push_row(row.columns.iter().map(|v| value::cell(v.as_ref())).collect(), max_rows);
            }
            match next {
                PagingStateResponse::HasMorePages { state } => {
                    // Past the limit: stop paging instead of reading the whole table.
                    let r = out.results.last_mut().expect("a result set");
                    if r.rows.len() >= max_rows {
                        r.truncated = true;
                        out.info(format!("Se muestran las primeras {max_rows} filas; la consulta tiene más."));
                        break;
                    }
                    paging = state;
                }
                PagingStateResponse::NoMorePages => break,
            }
        }
        let Some(id) = trace_id else { return Ok(None) };
        match self.session.get_tracing_info(&id).await {
            Ok(info) => Ok(Some(info)),
            Err(e) => {
                out.info(format!("No se pudo leer el trace de la ejecución: {e}"));
                Ok(None)
            }
        }
    }

    /// Keys and indexed columns of a table (none when it isn't one).
    async fn keys(&self, ks: &str, table: &str) -> Result<Option<plan::Keys>> {
        let cols = self.table_columns(ks, table).await?;
        if cols.is_empty() {
            return Ok(None);
        }
        let of = |kind: &str| cols.iter().filter(|c| c.kind == kind).map(|c| c.name.clone()).collect::<Vec<_>>();
        let mut keys = plan::Keys { partition: of("partition_key"), clustering: of("clustering"), indexed: Vec::new() };
        let rows = self
            .rows("SELECT options FROM system_schema.indexes WHERE keyspace_name = ? AND table_name = ?", (ks, table))
            .await
            .unwrap_or_default();
        for r in rows {
            if let Some(Some(CqlValue::Map(m))) = r.columns.first() {
                for (k, v) in m {
                    if matches!(k, CqlValue::Text(k) if k == "target") {
                        if let CqlValue::Text(t) = v {
                            // `values(tags)`, `"Name"`, `full(x)`…
                            let t = t.rsplit('(').next().unwrap_or(t).trim_end_matches(')').trim_matches('"');
                            keys.indexed.push(t.to_string());
                        }
                    }
                }
            }
        }
        Ok(Some(keys))
    }
}

/// `frozen<list<…>>`, `frozen<set<…>>` or `frozen<map<…>>`.
fn frozen_collection(data_type: &str) -> bool {
    let t = data_type.trim().to_ascii_lowercase();
    t.strip_prefix("frozen<").is_some_and(|i| ["list<", "set<", "map<"].iter().any(|p| i.trim_start().starts_with(p)))
}

/// A column type as CQL writes it (`text`, `list<int>`…).
fn type_name(t: &scylla::frame::response::result::ColumnType<'_>) -> String {
    use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
    match t {
        ColumnType::Native(n) => match n {
            NativeType::BigInt => "bigint".into(),
            NativeType::SmallInt => "smallint".into(),
            NativeType::TinyInt => "tinyint".into(),
            other => format!("{other:?}").to_lowercase(),
        },
        ColumnType::Collection { typ, .. } => match typ {
            CollectionType::List(i) => format!("list<{}>", type_name(i)),
            CollectionType::Set(i) => format!("set<{}>", type_name(i)),
            CollectionType::Map(k, v) => format!("map<{}, {}>", type_name(k), type_name(v)),
            _ => String::new(),
        },
        ColumnType::Vector { typ, dimensions } => format!("vector<{}, {dimensions}>", type_name(typ)),
        ColumnType::UserDefinedType { definition, .. } => definition.name.to_string(),
        ColumnType::Tuple(items) => format!("tuple<{}>", items.iter().map(type_name).collect::<Vec<_>>().join(", ")),
        _ => String::new(),
    }
}

#[async_trait]
impl DbSession for CassandraSession {
    async fn server_version(&mut self) -> Result<String> {
        let rows = self.rows("SELECT release_version FROM system.local", ()).await?;
        let version = rows.first().map(|r| text(r, 0)).unwrap_or_default();
        if self.flavor == Flavor::Keyspaces {
            return Ok(format!("Amazon Keyspaces (compatible con Cassandra {version})"));
        }
        // Scylla reports a Cassandra-compatible release_version; its own is in scylla_local / versions.
        if let Ok(r) = self.rows("SELECT version FROM system.versions", ()).await {
            if let Some(v) = r.first().map(|r| text(r, 0)).filter(|v| !v.is_empty()) {
                return Ok(format!("ScyllaDB {v}"));
            }
        }
        Ok(format!("Apache Cassandra {version}"))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.rows("SELECT keyspace_name FROM system_schema.keyspaces", ()).await?;
        let mut v: Vec<String> = rows.iter().map(|r| text(r, 0)).filter(|k| !is_system_keyspace(k)).collect();
        v.sort();
        Ok(v)
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let Some(ks) = self.keyspace.clone() else { return Ok(Vec::new()) };
        let obj = |kind: &str, name: String, parent: Option<String>| DbObject { kind: kind.into(), schema: None, name, parent };
        let mut out = Vec::new();
        for r in self.rows("SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?", (&ks,)).await? {
            out.push(obj(kinds::TABLE, text(&r, 0), None));
        }
        // Scylla keeps secondary indexes as views too; those aren't the user's.
        let indexes: Vec<String> = self
            .rows("SELECT index_name FROM system_schema.indexes WHERE keyspace_name = ?", (&ks,))
            .await
            .map(|rows| rows.iter().map(|r| format!("{}_index", text(r, 0))).collect())
            .unwrap_or_default();
        for r in self
            .rows("SELECT view_name, base_table_name FROM system_schema.views WHERE keyspace_name = ?", (&ks,))
            .await?
        {
            let name = text(&r, 0);
            if !indexes.contains(&name) {
                out.push(obj(kinds::MATERIALIZED_VIEW, name, Some(text(&r, 1))));
            }
        }
        for r in self.rows("SELECT type_name FROM system_schema.types WHERE keyspace_name = ?", (&ks,)).await? {
            out.push(obj(TYPE, text(&r, 0), None));
        }
        let mut functions: Vec<String> = self
            .rows("SELECT function_name FROM system_schema.functions WHERE keyspace_name = ?", (&ks,))
            .await?
            .iter()
            .map(|r| text(r, 0))
            .collect();
        functions.dedup();
        out.extend(functions.into_iter().map(|f| obj(kinds::FUNCTION, f, None)));
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let ks = self.ks(obj)?;
        if obj.kind == TYPE {
            let rows = self
                .rows(
                    "SELECT field_names, field_types FROM system_schema.types WHERE keyspace_name = ? AND type_name = ?",
                    (&ks, &obj.name),
                )
                .await?;
            let Some(r) = rows.first() else { return Ok(Vec::new()) };
            return Ok(texts(r, 0)
                .into_iter()
                .zip(texts(r, 1))
                .map(|(name, data_type)| ColumnInfo {
                    name,
                    data_type,
                    nullable: true,
                    primary_key: false,
                    auto_increment: false,
                    default_value: None,
                })
                .collect());
        }
        Ok(self
            .table_columns(&ks, &obj.name)
            .await?
            .into_iter()
            .map(|c| {
                let key = c.kind == "partition_key" || c.kind == "clustering";
                ColumnInfo {
                    name: c.name,
                    data_type: c.typ,
                    nullable: !key,
                    primary_key: key,
                    auto_increment: false,
                    default_value: None,
                }
            })
            .collect())
    }

    /// Types, materialized views and functions come without their keyspace,
    /// so the schema compare runs them in the other keyspace (and two
    /// keyspaces' copies read the same).
    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let ks = self.ks(obj)?;
        let code = obj.kind != kinds::TABLE;
        let what = match obj.kind.as_str() {
            kinds::MATERIALIZED_VIEW => "MATERIALIZED VIEW",
            TYPE => "TYPE",
            kinds::FUNCTION => "FUNCTION",
            _ => "TABLE",
        };
        let local = |d: String| if code { cql::unqualify(&d, &ks) } else { d };
        if let Some(d) = self.describe(what, &ks, &obj.name).await {
            return Ok(Some(local(d)));
        }
        let d = match obj.kind.as_str() {
            kinds::MATERIALIZED_VIEW => self.rebuild_table(&ks, &obj.name, true).await,
            TYPE => {
                let cols = self.columns(obj).await?;
                if cols.is_empty() {
                    return Ok(None);
                }
                let fields: Vec<String> =
                    cols.iter().map(|c| format!("    {} {}", cql::ident(&c.name), c.data_type)).collect();
                Ok(Some(format!("CREATE TYPE {} (\n{}\n);", cql::qualified(Some(&ks), &obj.name), fields.join(",\n"))))
            }
            kinds::FUNCTION => {
                let rows = self
                    .rows(
                        "SELECT argument_names, argument_types, return_type, language, body, called_on_null_input \
                         FROM system_schema.functions WHERE keyspace_name = ? AND function_name = ?",
                        (&ks, &obj.name),
                    )
                    .await?;
                let defs: Vec<String> = rows
                    .iter()
                    .map(|r| {
                        let args: Vec<String> = texts(r, 0)
                            .iter()
                            .zip(texts(r, 1))
                            .map(|(n, t)| format!("{} {t}", cql::ident(n)))
                            .collect();
                        format!(
                            "CREATE FUNCTION {}({})\n    {} ON NULL INPUT\n    RETURNS {}\n    LANGUAGE {}\n    AS $${}$$;",
                            cql::qualified(Some(&ks), &obj.name),
                            args.join(", "),
                            if boolean(r, 5) { "CALLED" } else { "RETURNS NULL" },
                            text(r, 2),
                            text(r, 3),
                            text(r, 4),
                        )
                    })
                    .collect();
                Ok((!defs.is_empty()).then(|| defs.join("\n\n")))
            }
            _ => self.rebuild_table(&ks, &obj.name, false).await,
        };
        Ok(d?.map(local))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        let ks = obj.schema().map(str::to_string).or_else(|| self.keyspace.clone());
        format!("SELECT * FROM {} LIMIT {limit};", cql::qualified(ks.as_deref(), &obj.name))
    }

    /// As cqlsh: statements end at `;`, a batch goes whole, and its own
    /// commands (`CONSISTENCY`, `PAGING`…) take their line. The first
    /// failing statement stops the script unless the editor run continues
    /// on errors, as cqlsh does (see `Step::end`).
    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let units = cql::script(text);
        if self.read_only {
            let statements: Vec<String> = units.iter().filter(|u| !u.command).map(|u| u.text.clone()).collect();
            if let Some(kw) = cql::first_write(&statements) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó una sentencia {kw}. Solo se permiten lecturas (SELECT, DESCRIBE, USE, LIST)."
                )));
            }
        }
        let own = out.current_statement.is_none();
        for (i, u) in units.iter().enumerate() {
            let step = Step::start(out, own, i, u.start, u.line);
            let r = if u.command { self.shell(&u.text, out) } else { self.run(&u.text, max_rows, out).await };
            if let Some(ks) = self.session.get_keyspace() {
                if self.keyspace.as_deref() != Some(&*ks) {
                    // `USE ks`: the tab's database selector follows it.
                    out.database = Some(ks.to_string());
                }
                self.keyspace = Some(ks.to_string());
            }
            step.end(out, r)?;
        }
        Ok(())
    }

    /// Tables of the session's keyspace from `system_schema`: columns in key
    /// order with their role in the column options (`partition_key`,
    /// `clustering_key`, `clustering_order`, `static`), the primary key
    /// (partition then clustering columns), secondary / SAI / custom
    /// indexes and the table options the designer edits.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let Some(ks) = self.keyspace.clone() else { return Ok(Vec::new()) };
        let tables = self
            .rows(
                "SELECT table_name, comment, default_time_to_live, gc_grace_seconds, compaction \
                 FROM system_schema.tables WHERE keyspace_name = ?",
                (&ks,),
            )
            .await?;
        let mut out = Vec::new();
        for r in &tables {
            let name = text(r, 0);
            let cols = self.table_columns(&ks, &name).await?;
            let mut options = BTreeMap::new();
            let comment = text(r, 1);
            if !comment.is_empty() {
                options.insert("comment".to_string(), comment.clone());
            }
            let ttl = int(r, 2);
            if ttl != 0 {
                options.insert("default_time_to_live".to_string(), ttl.to_string());
            }
            let gc = int(r, 3);
            if gc != 864_000 {
                options.insert("gc_grace_seconds".to_string(), gc.to_string());
            }
            if let Some(class) = map_value(r, 4, "class") {
                options.insert("compaction".to_string(), class.rsplit('.').next().unwrap_or(&class).to_string());
            }
            let key: Vec<String> = cols
                .iter()
                .filter(|c| c.kind == "partition_key" || c.kind == "clustering")
                .map(|c| c.name.clone())
                .collect();
            let columns = cols
                .into_iter()
                .map(|c| {
                    let flag = |b: bool| if b { "true" } else { "false" }.to_string();
                    let mut o = BTreeMap::new();
                    o.insert("partition_key".to_string(), flag(c.kind == "partition_key"));
                    o.insert("clustering_key".to_string(), flag(c.kind == "clustering"));
                    o.insert("static".to_string(), flag(c.kind == "static"));
                    if c.kind == "clustering" {
                        o.insert("clustering_order".to_string(), if c.desc { "DESC" } else { "ASC" }.to_string());
                    }
                    ColumnDef {
                        nullable: c.kind != "partition_key" && c.kind != "clustering",
                        name: c.name,
                        data_type: c.typ,
                        options: o,
                        ..Default::default()
                    }
                })
                .collect();
            out.push(TableSchema {
                kind: kinds::TABLE.into(),
                schema: Some(ks.clone()),
                name: name.clone(),
                columns,
                primary_key: (!key.is_empty()).then_some(KeyDef { name: None, columns: key }),
                foreign_keys: Vec::new(),
                indexes: Vec::new(),
                comment: (!comment.is_empty()).then_some(comment),
                options,
                ..Default::default()
            });
        }
        let indexes = self
            .rows("SELECT table_name, index_name, kind, options FROM system_schema.indexes WHERE keyspace_name = ?", (&ks,))
            .await?;
        for r in &indexes {
            let Some(t) = out.iter_mut().find(|t| t.name == text(r, 0)) else { continue };
            let class = map_value(r, 3, "class_name");
            let kind = match class {
                Some(c) if c.eq_ignore_ascii_case("sai") || c.ends_with("StorageAttachedIndex") => Some("sai".to_string()),
                Some(c) => Some(c),
                None => None,
            };
            let mut target = map_value(r, 3, "target").unwrap_or_default();
            // Scylla reports an index on a set's values as `keys(col)`,
            // which its own CREATE INDEX refuses for sets.
            if let Some(col) = target.strip_prefix("keys(").and_then(|c| c.strip_suffix(')')) {
                let col = col.trim_matches('"');
                if t.columns.iter().any(|c| c.name == col && c.data_type.starts_with("set<")) {
                    target = format!("values({})", &target[5..target.len() - 1]);
                }
            }
            // An index on a whole frozen collection: Scylla stores the
            // target as the bare column, not `full(col)`, and a frozen
            // collection only takes FULL indexes.
            if !target.contains('(') {
                let col = target.trim_matches('"');
                if t.columns.iter().any(|c| c.name == col && frozen_collection(&c.data_type)) {
                    target = format!("full({target})");
                }
            }
            // The rest of the map is the index's `WITH OPTIONS` (SAI's
            // case_sensitive / normalize / similarity_function, SASI's mode…).
            let options = map_entries(r, 3).into_iter().filter(|(k, _)| k != "target" && k != "class_name").collect();
            t.indexes.push(IndexDef {
                name: text(r, 1),
                columns: vec![target],
                unique: false,
                kind,
                filter: None,
                options,
                ..Default::default()
            });
        }
        out.sort_by(|a, b| a.name.cmp(&b.name));
        Ok(out)
    }

    /// `CREATE KEYSPACE` with NetworkTopologyStrategy and one replica per
    /// datacenter (only a name arrives; `ALTER KEYSPACE` changes it later).
    /// Unlike SimpleStrategy, it also works on ScyllaDB's tablet keyspaces.
    async fn create_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden crear bases.".into()));
        }
        let ks = ddl::keyspace_name(name)?;
        let cql = format!("CREATE KEYSPACE {ks} WITH replication = {{'class': 'NetworkTopologyStrategy', 'replication_factor': 1}}");
        self.session.query_unpaged(cql, ()).await.map_err(|e| Error::Query(e.to_string()))?;
        Ok(())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden borrar bases.".into()));
        }
        if is_system_keyspace(name.trim()) {
            return Err(Error::Query(format!("{name} es un keyspace del sistema: no se borra.")));
        }
        let ks = ddl::keyspace_name(name)?;
        self.session.query_unpaged(format!("DROP KEYSPACE {ks}"), ()).await.map_err(|e| Error::Query(e.to_string()))?;
        if self.keyspace.as_deref() == Some(name.trim()) {
            self.keyspace = None;
        }
        Ok(())
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        monitor::snapshot(&self.session, self.flavor).await
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

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        backup::history(self, database).await
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, source, progress).await
    }

    /// Estimated: the access path from the WHERE clause and the table's
    /// keys (Cassandra has no optimizer plans). Actual: the server's trace.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let statements = cql::split(text);
        if analyze && self.read_only {
            if let Some(kw) = cql::first_write(&statements) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó una sentencia {kw}. Solo se permiten lecturas (SELECT, DESCRIBE, USE, LIST)."
                )));
            }
        }
        for stmt in statements {
            if analyze {
                let info = self.run_traced(&stmt, max_rows, out, true).await?;
                if let Some(ks) = self.session.get_keyspace() {
                    self.keyspace = Some(ks.to_string());
                }
                if let Some(info) = info {
                    let t = trace(&info);
                    out.plans.push(Plan {
                        statement: stmt.clone(),
                        root: plan::trace_tree(&t),
                        actual: true,
                        raw_format: "text".into(),
                        raw: plan::trace_text(&t),
                    });
                }
                continue;
            }
            let shape = plan::shape(&stmt);
            let ks = shape.keyspace.clone().or_else(|| self.keyspace.clone());
            let keys = match (&ks, &shape.table) {
                (Some(ks), Some(t)) => self.keys(ks, t).await?,
                _ => None,
            };
            if shape.table.is_some() && keys.is_none() && matches!(shape.kind.as_str(), "select" | "update" | "delete" | "insert") {
                return Err(Error::Query(format!(
                    "No se encontró la tabla {} en el keyspace {}.",
                    shape.table.as_deref().unwrap_or_default(),
                    ks.as_deref().unwrap_or("(ninguno)")
                )));
            }
            let mut root = plan::access(&shape, keys.as_ref());
            if root.object.is_some() && shape.keyspace.is_none() {
                root.object = ks.as_ref().zip(shape.table.as_ref()).map(|(k, t)| format!("{k}.{t}"));
            }
            let raw = format!(
                "{}{}{}",
                root.op,
                if root.detail.is_empty() { String::new() } else { format!(" ({})", root.detail) },
                root.warnings.iter().map(|w| format!("\n  ! {w}")).collect::<String>()
            );
            out.plans.push(Plan { statement: stmt.clone(), root, actual: false, raw_format: "text".into(), raw });
        }
        Ok(())
    }

    /// The role's own permissions and superuser status (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        self.index_usage_report(table).await
    }
}

/// The driver's trace as [`plan::Trace`].
fn trace(info: &TracingInfo) -> plan::Trace {
    let ip = |a: &Option<std::net::IpAddr>| a.map(|a| a.to_string()).unwrap_or_default();
    let mut params: Vec<(String, String)> = info.parameters.clone().unwrap_or_default().into_iter().collect();
    params.sort();
    plan::Trace {
        request: info.request.clone().unwrap_or_default(),
        coordinator: ip(&info.coordinator),
        duration_us: info.duration,
        params,
        events: info
            .events
            .iter()
            .map(|e| plan::Event {
                activity: e.activity.clone().unwrap_or_default(),
                source: ip(&e.source),
                elapsed_us: e.source_elapsed,
                thread: e.thread.clone().unwrap_or_default(),
            })
            .collect(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn error_positions() {
        assert_eq!(position("line 1:7 no viable alternative at input 'x'"), Some((1, 7)));
        assert_eq!(position("line 3:0 mismatched input"), Some((3, 0)));
        assert_eq!(position("Undefined column name nope"), None);
    }

    #[test]
    fn frozen_collections_take_full_indexes() {
        assert!(frozen_collection("frozen<list<int>>"));
        assert!(frozen_collection("frozen<map<text, int>>"));
        assert!(!frozen_collection("frozen<tuple<int, int>>"));
        assert!(!frozen_collection("list<int>"));
    }

    #[test]
    fn hosts_with_and_without_port() {
        assert!(has_port("db:9142"));
        assert!(!has_port("db"));
        assert!(!has_port("::1"));
        assert!(has_port("[::1]:9042"));
    }

    #[test]
    fn system_keyspaces_are_hidden() {
        for k in ["system", "system_schema", "system_auth", "system_distributed_everywhere"] {
            assert!(is_system_keyspace(k));
        }
        assert!(!is_system_keyspace("systems"));
        assert!(!is_system_keyspace("app"));
    }

    #[test]
    fn auth_errors_are_recognised() {
        assert!(matches!(connect_err("Authentication failed: bad credentials".into()), Error::AuthFailed(_)));
        assert!(matches!(connect_err("Connection refused".into()), Error::Connect(_)));
    }
}

#[cfg(test)]
mod variant_tests {
    use super::*;

    #[test]
    fn keyspaces_variant() {
        let ids: Vec<&str> = drivers().iter().map(|d| d.info().id).collect();
        assert_eq!(ids, ["cassandra", "scylladb", "keyspaces"]);
        let d = drivers().into_iter().find(|d| d.info().id == "keyspaces").unwrap();
        assert_eq!(d.info().default_port, 9142);
        assert!(d.capabilities().monitor);
        // No materialized views, secondary indexes or UDFs on Keyspaces.
        assert!(d.create_templates().iter().all(|t| t.kind == TYPE));
        assert!(!d.designer().unwrap().indexes);
    }
}
