//! MongoDB through the official `mongodb` crate (async, tokio).
//!
//! # Query language (`Language::Json`)
//!
//! A script is a sequence of statements, separated by `;` or just by a new
//! line (a line starting with `.` continues the call chain above it):
//!
//! - **Shell calls**, as in `mongosh`:
//!   - `db.<coll>.find(<filter>, <projection>)` with `.sort({…})`,
//!     `.limit(n)`, `.skip(n)`, `.projection({…})`, `.hint(…)`,
//!     `.collation(…)`, `.maxTimeMS(n)`, `.count()`, `.explain()`;
//!     `findOne(…)`.
//!   - `db.<coll>.aggregate([…], {options})`, `countDocuments(filter)`,
//!     `estimatedDocumentCount()`, `distinct("field", filter)`,
//!     `getIndexes()`, `stats()`.
//!   - Writes: `insertOne`, `insertMany`, `updateOne`, `updateMany`,
//!     `replaceOne`, `deleteOne`, `deleteMany`, `drop` (a missing
//!     collection is not an error, as in mongosh).
//!   - Indexes: `createIndex(keys, options)` (the shell's default name when
//!     `name` is missing), `createIndexes([keys…], options)`,
//!     `dropIndex(name | keys)`, `dropIndexes()`.
//!   - Creation: `db.createCollection(name, options)`,
//!     `db.createView(name, source, pipeline, options)`,
//!     `db.dropDatabase()`. DBine extension: a third argument
//!     `{ ifNotExists: true }` to `createCollection` leaves an existing
//!     collection alone instead of failing (mongosh ignores it). Likewise
//!     `db.runCommand({ grantRolesToUser: "x", … }, {}, { ifExists: true })`
//!     skips a user/role command whose user or role doesn't exist (the
//!     "Usuarios y permisos" scripts use it; see security.rs).
//!   - Database: `db.getCollectionNames()`, `db.getCollectionInfos(filter)`,
//!     `db.runCommand({…})`, `db.adminCommand({…})`, `db.stats()`,
//!     `db.version()`, `db.getCollection("name").<method>(…)`,
//!     `db["name"].<method>(…)`.
//! - **Raw commands**: a JSON document run with `runCommand`, e.g.
//!   `{ "find": "users", "filter": { "age": { "$gt": 30 } }, "limit": 10 }`.
//!
//! Arguments are relaxed JSON: unquoted keys, single quotes, trailing
//! commas, `//` and `/* */` comments, and the shell helpers `ObjectId("…")`,
//! `ISODate("…")`/`new Date(…)`, `NumberLong(…)`, `NumberInt(…)`,
//! `NumberDecimal("…")`, `Timestamp(t, i)`, `UUID("…")`, `BinData(t, "…")`
//! and `/regex/flags`. Extended JSON (`{"$oid": "…"}`) works too.
//!
//! # Results
//!
//! One row per document; columns are the union of top-level keys with `_id`
//! first; nested values are compact JSON (ObjectId as hex, dates as ISO).
//! A cursor is read up to `max_rows + 1` documents and then closed (the
//! result is marked truncated), so a huge collection isn't streamed in full.
//!
//! # Read-only and cancel
//!
//! Read-only connections only run a whitelist of read commands (find,
//! aggregate without `$out`/`$merge`, count, distinct, list*, stats…).
//! Cancel: every find/aggregate carries a per-session `comment`; the
//! interrupter finds those operations with `currentOp` and `killOp`s them
//! through the same client (the driver keeps other pooled connections free
//! for it). Dropping the session also closes open cursors.

mod blocking;
mod convert;
mod ddl;
mod monitor;
mod permissions;
mod plan;
mod profiler;
mod security;
mod shell;
mod sync;
mod transfer;

use dbine_driver::async_trait;
use dbine_driver::{
    kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec, Driver,
    DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, QueryOutcome, Result, ResultColumn,
    MonitorSnapshot, Session, TableSchema,
};
use futures::TryStreamExt;
use mongodb::bson::{doc, Bson, Document};
use mongodb::error::ErrorKind;
use mongodb::options::{ClientOptions, Credential, ServerAddress, Tls, TlsOptions};
use mongodb::{Client, Database};
use shell::{Shape, Stmt};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::Duration;

/// Syntax help for the editor (Spanish, as all UI text).
pub const QUERY_HELP: &str = "Sintaxis tipo mongosh, una sentencia por línea o separadas con `;`:\n\
db.coleccion.find({ edad: { $gt: 30 } }, { nombre: 1 }).sort({ edad: -1 }).limit(10)\n\
db.coleccion.aggregate([{ $group: { _id: \"$ciudad\", n: { $sum: 1 } } }])\n\
db.coleccion.countDocuments({}) · db.coleccion.distinct(\"campo\", {})\n\
db.coleccion.insertOne({…}) · updateMany(filtro, { $set: {…} }) · deleteOne(filtro)\n\
db.getCollectionNames() · db.runCommand({ dbStats: 1 })\n\
db.createCollection(\"c\", { capped: true, size: 4096 }) · db.createView(\"v\", \"c\", [{ $match: {} }])\n\
db.getCollection(\"c\").createIndex({ campo: 1, fecha: -1 }, { unique: true }) · dropIndex(\"campo_1\") · db.dropDatabase()\n\
Extensión de DBine: db.createCollection(\"c\", {…}, { ifNotExists: true }) no falla si la colección ya existe.\n\
También se acepta un comando crudo en JSON: { \"find\": \"coleccion\", \"filter\": {} }.\n\
Claves sin comillas, comillas simples, ObjectId(\"…\"), ISODate(\"…\"), NumberLong(…) y /regex/i.";

const TIMEOUT: Duration = Duration::from_secs(15);

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![
        Arc::new(MongoDriver { flavor: Flavor::Mongo }),
        Arc::new(MongoDriver { flavor: Flavor::Ferret }),
        Arc::new(MongoDriver { flavor: Flavor::DocumentDb }),
    ]
}

/// The servers this crate talks to: all speak the MongoDB wire protocol.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Mongo,
    /// FerretDB 2 (MongoDB protocol over PostgreSQL + DocumentDB extension).
    /// No `killOp`, `top`, `replSetGetStatus`; a reduced `serverStatus`.
    Ferret,
    /// Amazon DocumentDB: TLS with the AWS CA bundle, no retryable writes.
    DocumentDb,
}

pub struct MongoDriver {
    flavor: Flavor,
}

fn info(flavor: Flavor) -> &'static DriverInfo {
    static MONGO: OnceLock<DriverInfo> = OnceLock::new();
    static FERRET: OnceLock<DriverInfo> = OnceLock::new();
    static DOCDB: OnceLock<DriverInfo> = OnceLock::new();
    let cell = match flavor {
        Flavor::Mongo => &MONGO,
        Flavor::Ferret => &FERRET,
        Flavor::DocumentDb => &DOCDB,
    };
    cell.get_or_init(|| build_info(flavor))
}

fn build_info(flavor: Flavor) -> DriverInfo {
    let (id, name) = match flavor {
        Flavor::Mongo => ("mongodb", "MongoDB"),
        Flavor::Ferret => ("ferretdb", "FerretDB"),
        Flavor::DocumentDb => ("documentdb", "Amazon DocumentDB"),
    };
    let mut fields = vec![
        Field::new("host", "Servidor", FieldKind::Text)
            .placeholder(match flavor {
                Flavor::DocumentDb => "mi-cluster.cluster-xxxx.us-east-1.docdb.amazonaws.com",
                _ => "localhost",
            })
            .help("Varios miembros separados por coma: host1:27017,host2:27017."),
        Field::port(),
        Field::database().help("Base inicial; vacía = la de la cadena de conexión o «test»."),
        Field::username(),
        Field::password(),
        Field::new("auth_source", "Base de autenticación", FieldKind::Text).placeholder("admin").advanced(),
    ];
    match flavor {
        Flavor::Mongo => {
            fields.push(
                Field::new("replica_set", "Replica set", FieldKind::Text)
                    .help("Nombre del replica set; vacío = conexión directa al servidor.")
                    .advanced(),
            );
            fields.push(Field::encrypt());
            fields.push(Field::trust_cert());
        }
        Flavor::Ferret => {
            fields.push(Field::encrypt());
            fields.push(Field::trust_cert());
        }
        Flavor::DocumentDb => {
            fields.push(
                Field::new("replica_set", "Replica set", FieldKind::Text)
                    .placeholder("rs0")
                    .help("rs0 para conectarse al cluster; vacío = conexión directa a la instancia (útil con un túnel SSH).")
                    .advanced(),
            );
            fields.push(Field::encrypt().default_value("true"));
            fields.push(Field::trust_cert().help("Necesario si te conectás por un túnel (el certificado no coincide con localhost)."));
            fields.push(
                Field::new("ca_file", "Certificado de la CA de AWS", FieldKind::File)
                    .placeholder("global-bundle.pem")
                    .help("El paquete de CA de Amazon RDS/DocumentDB (global-bundle.pem), descargable desde la documentación de AWS.")
                    .ssl(),
            );
        }
    }
    fields.push(
        Field::new("connection_string", "Cadena de conexión", FieldKind::Password)
            .secret()
            .placeholder(match flavor {
                Flavor::DocumentDb => "mongodb://usuario:clave@cluster.docdb.amazonaws.com:27017/?tls=true&replicaSet=rs0&retryWrites=false",
                _ => "mongodb+srv://usuario:clave@cluster.example.net/base",
            })
            .help(match flavor {
                Flavor::Mongo => "Si se completa, reemplaza a los demás campos. Sirve también para Azure Cosmos DB \
                     (API de MongoDB) y MongoDB Atlas.",
                _ => "Si se completa, reemplaza a los demás campos.",
            }),
    );
    fields.push(Field::read_only());
    DriverInfo {
        id,
        name,
        family: Family::Document,
        language: Language::Json,
        dialect: "",
        default_port: 27017,
        fields,
        databases_label: "Bases de datos",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::new(kinds::COLLECTION, "Colecciones", true, true, true),
            ObjectKindInfo::views(),
        ],
    }
}

fn err(e: mongodb::error::Error) -> Error {
    match e.kind.as_ref() {
        ErrorKind::Authentication { message, .. } => Error::AuthFailed(message.clone()),
        ErrorKind::Command(c) if c.code == 18 => Error::AuthFailed(c.message.clone()),
        ErrorKind::ServerSelection { message, .. } => Error::Connect(message.clone()),
        ErrorKind::Io(io) => Error::Connect(io.to_string()),
        ErrorKind::DnsResolve { message, .. } => Error::Connect(message.clone()),
        ErrorKind::Command(c) => Error::Query(format!("{} ({})", c.message, c.code_name)),
        _ => Error::Query(e.to_string()),
    }
}

async fn client_options(cfg: &ConnectionConfig, flavor: Flavor) -> Result<ClientOptions> {
    let mut o = base_options(cfg, flavor).await?;
    if flavor == Flavor::DocumentDb {
        // DocumentDB rejects retryable writes.
        o.retry_writes = Some(false);
        if let Some(ca) = cfg.option("ca_file") {
            let mut t = match o.tls.take() {
                Some(Tls::Enabled(t)) => t,
                _ => TlsOptions::default(),
            };
            t.ca_file_path = Some(ca.into());
            o.tls = Some(Tls::Enabled(t));
        }
    }
    Ok(o)
}

async fn base_options(cfg: &ConnectionConfig, flavor: Flavor) -> Result<ClientOptions> {
    if let Some(uri) = cfg.option("connection_string") {
        return ClientOptions::parse(uri).await.map_err(|e| Error::Connect(format!("cadena de conexión inválida: {e}")));
    }
    let mut o = ClientOptions::default();
    let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
    o.hosts = host
        .split(',')
        .filter(|h| !h.trim().is_empty())
        .map(|h| {
            let h = h.trim();
            if h.contains(':') && !h.starts_with('[') {
                ServerAddress::parse(h)
            } else {
                Ok(ServerAddress::Tcp { host: h.to_string(), port: Some(cfg.port_or(27017)) })
            }
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Connect(e.to_string()))?;
    if let Some(user) = cfg.username.as_deref().filter(|u| !u.is_empty()) {
        let mut c = Credential::default();
        c.username = Some(user.to_string());
        c.password = cfg.password.clone();
        c.source = cfg.option("auth_source").map(str::to_string);
        o.credential = Some(c);
    }
    match cfg.option("replica_set") {
        Some(rs) => o.repl_set_name = Some(rs.to_string()),
        None if o.hosts.len() == 1 => o.direct_connection = Some(true),
        None => {}
    }
    let tls = cfg.encrypt || cfg.option("tls").is_some_and(|v| v == "true") || (flavor == Flavor::DocumentDb && cfg.option("ca_file").is_some());
    if tls {
        let mut t = TlsOptions::default();
        if cfg.trust_server_certificate {
            t.allow_invalid_certificates = Some(true);
        }
        o.tls = Some(Tls::Enabled(t));
    }
    Ok(o)
}

#[async_trait]
impl Driver for MongoDriver {
    fn info(&self) -> &DriverInfo {
        info(self.flavor)
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn supports_profiler(&self) -> bool {
        profiler::supported(self.flavor)
    }

    /// Unordered `insertMany` (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// Between MongoDB, FerretDB and DocumentDB: documents as raw BSON.
    fn supports_native_copy(&self, target: &str) -> bool {
        matches!(target, "mongodb" | "ferretdb" | "documentdb")
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

    fn capabilities(&self) -> Capabilities {
        // FerretDB reports no lock waits and has no killOp (see blocking.rs).
        let locks = self.flavor != Flavor::Ferret;
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: false,
            monitor: true,
            blocking: locks,
            kill_session: locks,
            ..Default::default()
        }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(ddl::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates()
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(table, parts)
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
        Some(security::spec(self.flavor))
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(self.flavor, action)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let mut o = client_options(cfg, self.flavor).await?;
        o.app_name = Some("DBine".into());
        o.connect_timeout = Some(TIMEOUT);
        o.server_selection_timeout = Some(TIMEOUT);
        o.max_pool_size = Some(4);
        let db_name = database
            .filter(|d| !d.is_empty())
            .map(str::to_string)
            .or_else(|| Some(cfg.database.clone()).filter(|d| !d.is_empty()))
            .or_else(|| o.default_database.clone())
            .unwrap_or_else(|| "test".into());
        let client = Client::with_options(o).map_err(err)?;
        // The client is lazy: a ping proves the server and the login.
        client.database("admin").run_command(doc! { "ping": 1 }).await.map_err(err)?;
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let tag = format!("dbine-{}-{}", std::process::id(), SEQ.fetch_add(1, Ordering::Relaxed));
        Ok(Box::new(MongoSession { db: client.database(&db_name), client, read_only: cfg.read_only, tag, flavor: self.flavor, profiler: None }))
    }
}

pub struct MongoSession {
    client: Client,
    db: Database,
    read_only: bool,
    /// `comment` on this session's find/aggregate, so cancel can find them.
    tag: String,
    flavor: Flavor,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
}

impl MongoSession {
    fn refuse_if_read_only(&self, what: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(format!("Conexión de solo lectura: no se puede {what}.")));
        }
        Ok(())
    }

    async fn first_batch(&self, cmd: Document, limit: usize) -> Result<Vec<Document>> {
        let mut cur = self.db.run_cursor_command(cmd).await.map_err(err)?;
        let mut out = Vec::new();
        while out.len() < limit {
            match cur.try_next().await.map_err(err)? {
                Some(d) => out.push(d),
                None => break,
            }
        }
        Ok(out)
    }

    async fn run(&self, stmt: Stmt, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let db = if stmt.admin { self.client.database("admin") } else { self.db.clone() };
        let mut cmd = stmt.cmd;
        if stmt.shape == Shape::IfExists {
            if let Some(skip) = security::missing_principal(&db, &cmd).await? {
                out.messages.push(skip);
                return Ok(());
            }
        }
        match stmt.shape {
            Shape::Cursor | Shape::CountAgg => {
                let first = cmd.keys().next().cloned().unwrap_or_default();
                if matches!(first.as_str(), "find" | "aggregate") && !cmd.contains_key("comment") {
                    cmd.insert("comment", self.tag.as_str());
                }
                let mut cur = db.run_cursor_command(cmd).await.map_err(err)?;
                if stmt.shape == Shape::CountAgg {
                    let n = match cur.try_next().await.map_err(err)? {
                        Some(d) => d.get("count").cloned().unwrap_or(Bson::Int32(0)),
                        None => Bson::Int32(0),
                    };
                    return scalar(out, "count", &n);
                }
                let mut docs = Vec::new();
                let mut more = false;
                while let Some(d) = cur.try_next().await.map_err(err)? {
                    if docs.len() == max_rows {
                        more = true;
                        break;
                    }
                    docs.push(d);
                }
                push_docs(out, &docs, max_rows);
                if more {
                    let r = out.results.last_mut().expect("a result set");
                    r.truncated = true;
                    r.total_rows += 1;
                    out.messages.push(format!(
                        "Se muestran los primeros {max_rows} documentos; el cursor se cerró sin leer el resto."
                    ));
                }
            }
            Shape::Count => {
                let r = db.run_command(cmd).await.map_err(err)?;
                scalar(out, "count", r.get("n").unwrap_or(&Bson::Int32(0)))?;
            }
            Shape::Distinct(key) => {
                let r = db.run_command(cmd).await.map_err(err)?;
                out.begin_result(vec![ResultColumn { name: key, type_name: String::new() }]);
                for v in r.get_array("values").map(|a| a.as_slice()).unwrap_or(&[]) {
                    out.push_row(vec![convert::cell(v)], max_rows);
                }
            }
            Shape::Write => {
                let upd = cmd.contains_key("update");
                let r = db.run_command(cmd).await.map_err(err)?;
                check_write_errors(&r)?;
                let n = if upd {
                    count_of(&r, "nModified") + r.get_array("upserted").map(|a| a.len() as u64).unwrap_or(0)
                } else {
                    count_of(&r, "n")
                };
                out.push_affected(n);
            }
            Shape::Reply | Shape::CreateIfMissing | Shape::IfExists => {
                let first = cmd.keys().next().cloned().unwrap_or_default();
                let mut r = match db.run_command(cmd).await {
                    Ok(r) => r,
                    // As in mongosh, dropping a missing collection is not an
                    // error (servers before 7.0 answer NamespaceNotFound).
                    Err(e) if first == "drop" && command_code(&e) == Some(26) => {
                        out.messages.push("La colección no existía; no se borró nada.".into());
                        return Ok(());
                    }
                    Err(e) if stmt.shape == Shape::CreateIfMissing && command_code(&e) == Some(48) => {
                        out.messages.push("La colección ya existía; se dejó como estaba.".into());
                        return Ok(());
                    }
                    Err(e) => return Err(err(e)),
                };
                check_write_errors(&r)?;
                for k in ["$clusterTime", "operationTime", "$db"] {
                    r.remove(k);
                }
                push_docs(out, std::slice::from_ref(&r), max_rows);
            }
        }
        Ok(())
    }
}

fn command_code(e: &mongodb::error::Error) -> Option<i32> {
    match e.kind.as_ref() {
        ErrorKind::Command(c) => Some(c.code),
        _ => None,
    }
}

fn count_of(r: &Document, key: &str) -> u64 {
    match r.get(key) {
        Some(Bson::Int32(n)) => *n as u64,
        Some(Bson::Int64(n)) => *n as u64,
        Some(Bson::Double(n)) => *n as u64,
        _ => 0,
    }
}

fn check_write_errors(r: &Document) -> Result<()> {
    let msg = |d: &Document| d.get_str("errmsg").unwrap_or("error de escritura").to_string();
    if let Some(e) = r.get_array("writeErrors").ok().and_then(|a| a.first()).and_then(Bson::as_document) {
        return Err(Error::Query(msg(e)));
    }
    if let Ok(e) = r.get_document("writeConcernError") {
        return Err(Error::Query(msg(e)));
    }
    Ok(())
}

fn scalar(out: &mut QueryOutcome, name: &str, v: &Bson) -> Result<()> {
    out.begin_result(vec![ResultColumn { name: name.into(), type_name: convert::type_name(v).into() }]);
    out.push_row(vec![convert::cell(v)], usize::MAX);
    Ok(())
}

/// One row per document, columns = the union of their keys.
fn push_docs(out: &mut QueryOutcome, docs: &[Document], max_rows: usize) {
    let keys = convert::union_keys(docs);
    out.begin_result(keys.iter().map(|k| ResultColumn { name: k.clone(), type_name: String::new() }).collect());
    for d in docs {
        out.push_row(keys.iter().map(|k| d.get(k).map_or(serde_json::Value::Null, convert::cell)).collect(), max_rows);
    }
}

/// `db.<name>.find({}).limit(n)`, or `db.getCollection("…")` for names
/// that aren't plain identifiers.
fn browse_text(name: &str, limit: u32) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        format!("db.{name}.find({{}}).limit({limit})")
    } else {
        format!("db.getCollection({}).find({{}}).limit({limit})", serde_json::Value::String(name.into()))
    }
}

fn clip(s: &str, n: usize) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() <= n {
        one
    } else {
        format!("{}…", one.chars().take(n).collect::<String>())
    }
}

async fn kill_tagged(client: Client, tag: String) {
    let admin = client.database("admin");
    let filter = doc! { "currentOp": 1, "$or": [
        { "command.comment": tag.as_str() },
        { "cursor.originatingCommand.comment": tag.as_str() },
    ] };
    let Ok(r) = admin.run_command(filter).await else { return };
    for op in r.get_array("inprog").map(|a| a.as_slice()).unwrap_or(&[]) {
        if let Some(id) = op.as_document().and_then(|d| d.get("opid")) {
            let _ = admin.run_command(doc! { "killOp": 1, "op": id.clone() }).await;
        }
    }
}

#[async_trait]
impl Session for MongoSession {
    async fn server_version(&mut self) -> Result<String> {
        let r = self.client.database("admin").run_command(doc! { "buildInfo": 1 }).await.map_err(err)?;
        let version = r.get_str("version").unwrap_or("?");
        Ok(match (self.flavor, r.get_document("ferretdb").and_then(|f| f.get_str("version"))) {
            (_, Ok(fv)) => format!("FerretDB {} (compatible con MongoDB {version})", fv.trim_start_matches('v')),
            (Flavor::DocumentDb, _) => format!("Amazon DocumentDB (compatible con MongoDB {version})"),
            _ => format!("MongoDB {version}"),
        })
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        monitor::snapshot(&self.client, self.flavor).await
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        blocking::blocking(&self.client, self.flavor).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        self.refuse_if_read_only("terminar operaciones")?;
        blocking::kill(&self.client, self.flavor, id).await
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
            Some(state) => profiler::stop(state).await,
            None => Ok(()),
        }
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

    /// For `copy_native`.
    fn as_any(&mut self) -> Option<&mut (dyn std::any::Any + Send)> {
        Some(self)
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let cmd = doc! { "listDatabases": 1, "nameOnly": true, "authorizedDatabases": true };
        match self.client.database("admin").run_command(cmd).await {
            Ok(r) => {
                let mut v: Vec<String> = r
                    .get_array("databases")
                    .map(|a| a.as_slice())
                    .unwrap_or(&[])
                    .iter()
                    .filter_map(|d| d.as_document()?.get_str("name").ok().map(str::to_string))
                    .collect();
                if !v.iter().any(|n| n == self.db.name()) {
                    v.push(self.db.name().to_string());
                }
                v.sort();
                Ok(v)
            }
            // Without the privilege, at least the session's database.
            Err(e) if matches!(e.kind.as_ref(), ErrorKind::Command(_)) => Ok(vec![self.db.name().to_string()]),
            Err(e) => Err(err(e)),
        }
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let cmd = doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true };
        let docs = self.first_batch(cmd, usize::MAX).await?;
        let mut v: Vec<DbObject> = docs
            .iter()
            .filter_map(|d| {
                let name = d.get_str("name").ok()?;
                if name.starts_with("system.") {
                    return None;
                }
                let kind = if d.get_str("type") == Ok("view") { kinds::VIEW } else { kinds::COLLECTION };
                Some(DbObject { kind: kind.into(), schema: None, name: name.into(), parent: None })
            })
            .collect();
        v.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(v)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let cmd = doc! { "aggregate": &obj.name, "pipeline": [{ "$sample": { "size": 100 } }], "cursor": {} };
        let docs = self.first_batch(cmd, 100).await?;
        Ok(convert::infer_columns(&docs))
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let spec = self.first_batch(doc! { "listCollections": 1, "filter": { "name": &obj.name } }, 1).await?;
        let Some(mut spec) = spec.into_iter().next() else { return Ok(None) };
        spec.remove("idIndex");
        // A view: the command that creates it (what the schema compare runs).
        if spec.get_str("type") == Ok("view") {
            return Ok(Some(ddl::view_definition(&obj.name, spec.get_document("options").ok())));
        }
        if let Ok(ix) = self.first_batch(doc! { "listIndexes": &obj.name }, usize::MAX).await {
            spec.insert("indexes", ix);
        }
        let v = Bson::Document(spec).into_relaxed_extjson();
        Ok(Some(serde_json::to_string_pretty(&v)?))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        browse_text(&obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = shell::parse_script(text).map_err(Error::Query)?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        for stmt in stmts {
            if self.read_only {
                if let Some(w) = shell::write_reason(&stmt.cmd) {
                    return Err(Error::Query(format!(
                        "Conexión de solo lectura: se bloqueó `{w}`. Solo se permiten lecturas (find, aggregate, count, distinct, list…)."
                    )));
                }
            }
            self.run(stmt, max_rows, out).await?;
        }
        Ok(())
    }

    /// `explain` with verbosity `queryPlanner` (estimated) or
    /// `executionStats` (actual). With `executionStats` MongoDB runs the
    /// query plan but does not apply the modifications of an update,
    /// delete or findAndModify, so a write's figures come from that dry run
    /// and the write itself runs once afterwards. Aggregations with
    /// `$out`/`$merge` only get the `queryPlanner` plan.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = shell::parse_script_text(text).map_err(Error::Query)?;
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        for (src, stmt) in stmts {
            // An explicit `.explain()` / `{ explain: … }`: plan its command.
            let stmt = match stmt.cmd.iter().next() {
                Some((k, Bson::Document(inner))) if k == "explain" => shell::command_stmt(inner.clone(), stmt.admin),
                _ => stmt,
            };
            let writes = shell::write_reason(&stmt.cmd);
            if analyze && self.read_only {
                if let Some(w) = &writes {
                    return Err(Error::Query(format!(
                        "Conexión de solo lectura: se bloqueó `{w}`. Solo se permiten lecturas (find, aggregate, count, distinct, list…)."
                    )));
                }
            }
            let name = stmt.cmd.keys().next().map(|k| k.to_ascii_lowercase()).unwrap_or_default();
            if !plan::explainable(&name) {
                out.messages.push(format!("`{}`: el comando {name} no tiene plan de ejecución.", clip(&src, 80)));
                if analyze {
                    self.run(stmt, max_rows, out).await?;
                }
                continue;
            }
            let pipeline_writes = name == "aggregate" && writes.is_some();
            let actual = analyze && !pipeline_writes;
            let verbosity = if actual { "executionStats" } else { "queryPlanner" };
            let db = if stmt.admin { self.client.database("admin") } else { self.db.clone() };
            let reply = db
                .run_command(doc! { "explain": stmt.cmd.clone(), "verbosity": verbosity })
                .await
                .map_err(err)?;
            let p = plan::from_explain(&src, &name, &reply, actual);
            if analyze {
                self.run(stmt, max_rows, out).await?;
            }
            out.plans.push(p);
        }
        Ok(())
    }

    /// Collections and views with the fields of a `$sample` of 100
    /// documents (plus those a `$jsonSchema` validator declares), `_id` as
    /// primary key, the indexes and the creation options.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let infos = self.first_batch(doc! { "listCollections": 1, "authorizedCollections": true }, usize::MAX).await?;
        let mut out = Vec::new();
        for info in infos {
            let Ok(name) = info.get_str("name") else { continue };
            if name.starts_with("system.") {
                continue;
            }
            let view = info.get_str("type") == Ok("view");
            let sample = doc! { "aggregate": name, "pipeline": [{ "$sample": { "size": 100 } }], "cursor": {} };
            let columns = self
                .first_batch(sample, 100)
                .await
                .map(|docs| convert::infer_columns(&docs))
                .unwrap_or_default()
                .into_iter()
                .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, ..Default::default() })
                .collect();
            let indexes = if view { Vec::new() } else { self.first_batch(doc! { "listIndexes": name }, usize::MAX).await.unwrap_or_default() };
            out.push(ddl::table_schema(&info, columns, &indexes));
        }
        out.sort_by(|a, b| a.name.to_lowercase().cmp(&b.name.to_lowercase()));
        Ok(out)
    }

    /// MongoDB creates databases lazily, when their first collection is
    /// created: this one gets an empty `_dbine` collection, which can be
    /// dropped once the database has collections of its own.
    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("crear una base")?;
        ddl::check_database_name(name)?;
        let names = self.client.list_database_names().await.map_err(err)?;
        if names.iter().any(|n| n.eq_ignore_ascii_case(name)) {
            return Err(Error::Query(format!("la base «{name}» ya existe")));
        }
        self.client.database(name).create_collection("_dbine").await.map_err(err)
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("borrar una base")?;
        if ddl::is_system_database(name) {
            return Err(Error::Query(format!("«{name}» es una base del sistema de MongoDB y no se borra")));
        }
        self.client.database(name).drop().await.map_err(err)
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let client = self.client.clone();
        let tag = self.tag.clone();
        Some(Arc::new(move || {
            handle.spawn(kill_tagged(client.clone(), tag.clone()));
        }))
    }

    /// `connectionStatus` with `showPrivileges` (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn browse_query_quotes_odd_names() {
        assert_eq!(browse_text("users", 50), "db.users.find({}).limit(50)");
        assert_eq!(browse_text("my-coll", 50), "db.getCollection(\"my-coll\").find({}).limit(50)");
        // What browse_query writes, the parser reads back.
        for n in ["users", "my-coll", "a.b", "stats", "getCollection"] {
            let st = shell::parse_script(&browse_text(n, 5)).unwrap();
            assert_eq!(st[0].cmd.get_str("find"), Ok(n));
        }
    }

    #[test]
    fn info_is_consistent() {
        let ids: Vec<&str> = drivers().iter().map(|d| d.info().id).collect();
        assert_eq!(ids, ["mongodb", "ferretdb", "documentdb"]);
        for f in [Flavor::Mongo, Flavor::Ferret, Flavor::DocumentDb] {
            assert!(info(f).fields.iter().any(|x| x.key == "connection_string" && x.secret));
        }
        assert!(info(Flavor::DocumentDb).fields.iter().any(|x| x.key == "ca_file"));
    }

    #[tokio::test]
    async fn documentdb_disables_retryable_writes() {
        let mut c = ConnectionConfig { driver: "documentdb".into(), host: "h".into(), ..Default::default() };
        c.options.insert("ca_file".into(), "/tmp/global-bundle.pem".into());
        let o = client_options(&c, Flavor::DocumentDb).await.unwrap();
        assert_eq!(o.retry_writes, Some(false));
        match o.tls {
            Some(Tls::Enabled(t)) => assert_eq!(t.ca_file_path.as_deref(), Some(std::path::Path::new("/tmp/global-bundle.pem"))),
            _ => panic!("TLS expected"),
        }
        let o = client_options(&c, Flavor::Mongo).await.unwrap();
        assert!(o.tls.is_none() && o.retry_writes.is_none());
    }
}
