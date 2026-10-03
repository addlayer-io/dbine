//! Graph databases that speak Cypher: Neo4j and Memgraph over Bolt (a
//! small client of our own, `bolt.rs`), Amazon Neptune over its openCypher
//! HTTPS endpoint (`neptune.rs`).
//!
//! # Query language (`Language::Cypher`)
//!
//! A script holds Cypher statements separated by `;`, plus cypher-shell's
//! client commands, each on its own line where a statement would start
//! (no `;` needed): `:use nombre` switches the session's database (Neo4j,
//! Memgraph Enterprise); `:begin`, `:commit`, `:rollback` open and end an
//! explicit transaction that lasts across runs; `:param nombre => expr`
//! (or `:param {a: 1}`) sets a `$nombre` parameter, evaluated by the
//! server, for the rest of the session; `:params` lists them and `:params
//! clear` drops them. As cypher-shell, the script stops at the first error
//! (a failed statement inside a transaction rolls it back). Manual
//! transactions (Auto/Manual in the editor) open one before the first
//! statement that can run in it.
//!
//! # Results
//!
//! One result set per statement, columns as the statement returns them.
//! Nodes, relationships and paths go as compact JSON in the shape Neptune
//! uses (`~id`, `~labels`, `~type`, `~start`, `~end`, `~properties`, see
//! `value.rs`); temporal values as ISO text. A statement that returns no
//! rows reports the entities it changed (Neo4j and Memgraph counters) in
//! the messages, and its total as affected rows. Server notifications
//! (deprecations, performance hints) go to the messages.
//!
//! # Explorer
//!
//! "Nodos" are the labels and "Relaciones" the relationship types; their
//! columns are the properties of a sample of 100. Indexes, constraints,
//! procedures (and Memgraph triggers) are listed with their definition.
//!
//! # Read-only and cancel
//!
//! Read-only connections reject statements with write clauses (`CREATE`,
//! `MERGE`, `SET`, `DELETE`…) and calls to procedures outside a list of
//! read ones; on Neo4j the transactions also run in READ access mode, so
//! the server refuses writes too. The interrupter terminates the session's
//! transaction from a second connection (`TERMINATE TRANSACTIONS`, found by
//! the session's `tx_metadata` tag or its Bolt connection id), or cancels
//! it through Neptune's `/openCypher/status`.

mod backup;
mod blocking;
mod bolt;
mod cypher;
mod ddl;
mod index_usage;
mod monitor;
mod neptune;
mod packstream;
mod permissions;
mod plan;
mod processes;
mod profiler;
mod security;
mod steps;
mod sync;
mod transfer;
mod value;

use bolt::{Conn, Target};
use dbine_driver::{
    async_trait, kinds, Capabilities, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts,
    DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, IndexDef, Language, MonitorSnapshot, ObjectKindInfo,
    Message, MessageLevel, ObjectRef, QueryOutcome, Result, ResultColumn, ScriptError, Session, TableSchema, TxState,
};
use packstream::{map, Value as Bolt};
use steps::Step;
use serde_json::Value;
use std::collections::BTreeMap;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

pub use ddl::CONSTRAINT;

/// Object kinds of the explorer.
pub const LABEL: &str = "label";
pub const RELATIONSHIP: &str = "relationship";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Flavor {
    Neo4j,
    Memgraph,
    Neptune,
}

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    [Flavor::Neo4j, Flavor::Memgraph, Flavor::Neptune]
        .into_iter()
        .map(|f| Arc::new(GraphDriver { flavor: f, info: info(f) }) as Arc<dyn Driver>)
        .collect()
}

pub const QUERY_HELP: &str = "Cypher, con las sentencias separadas por «;»:\n\
MATCH (p:Persona)-[:CONOCE]->(q) WHERE p.edad > 30 RETURN p, q LIMIT 25;\n\
CREATE (:Persona {nombre: 'Ana'}) · MERGE (c:Ciudad {nombre: 'Rosario'}) · MATCH (n) DETACH DELETE n\n\
:param nombre => valor   define $nombre para las consultas siguientes (:params los lista).\n\
:begin · :commit · :rollback   transacción explícita, que sigue abierta entre ejecuciones.\n\
Los nodos y relaciones se muestran como JSON (~id, ~labels, ~properties).\n\
EXPLAIN / PROFILE delante de una consulta muestran su plan (o usá los botones de plan).\n\
:use base   cambia la base de datos de la sesión (Neo4j, Memgraph Enterprise).";

fn kinds_for(f: Flavor) -> Vec<ObjectKindInfo> {
    let mut v = vec![
        ObjectKindInfo::new(LABEL, "Nodos", true, true, true),
        ObjectKindInfo::new(RELATIONSHIP, "Relaciones", true, true, true),
    ];
    if f != Flavor::Neptune {
        v.push(ObjectKindInfo::new(kinds::INDEX, "Índices", false, false, true));
        v.push(ObjectKindInfo::new(CONSTRAINT, "Restricciones", false, false, true));
        v.push(ObjectKindInfo::procedures());
    }
    if f == Flavor::Memgraph {
        v.push(ObjectKindInfo::triggers());
    }
    v
}

fn info(f: Flavor) -> DriverInfo {
    let bolt_fields = |example: &'static str| {
        vec![
            Field::host().help(example),
            Field::port(),
            Field::database(),
            Field::username(),
            Field::password(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::read_only(),
        ]
    };
    match f {
        Flavor::Neo4j => DriverInfo {
            id: "neo4j",
            name: "Neo4j",
            family: Family::Graph,
            language: Language::Cypher,
            dialect: "neo4j",
            default_port: 7687,
            fields: {
                let mut v = bolt_fields("Un host o una URI: neo4j+s://xxxx.databases.neo4j.io (Aura), bolt://servidor:7687.");
                v[3] = Field::username().default_value("neo4j");
                v
            },
            databases_label: "Bases de datos",
            has_schemas: false,
            object_kinds: kinds_for(f),
        },
        Flavor::Memgraph => DriverInfo {
            id: "memgraph",
            name: "Memgraph",
            family: Family::Graph,
            language: Language::Cypher,
            dialect: "memgraph",
            default_port: 7687,
            fields: bolt_fields("Un host o una URI bolt://servidor:7687 (bolt+s:// con TLS)."),
            databases_label: "Bases de datos",
            has_schemas: false,
            object_kinds: kinds_for(f),
        },
        Flavor::Neptune => DriverInfo {
            id: "neptune",
            name: "Amazon Neptune",
            family: Family::Graph,
            language: Language::Cypher,
            dialect: "neptune",
            default_port: 8182,
            fields: {
                let mut v = vec![
                    Field::host()
                        .placeholder("mi-cluster.cluster-xxxx.us-east-1.neptune.amazonaws.com")
                        .help("El endpoint del cluster (o una URL completa, p. ej. un túnel https://localhost:8182)."),
                    Field::port(),
                    Field::encrypt().default_value("true").help("Neptune solo acepta HTTPS."),
                    Field::trust_cert().help("Útil con un túnel SSH, donde el certificado no coincide con localhost."),
                    Field::new("iam", "Autenticación IAM (SigV4)", FieldKind::Bool)
                        .help("Activala si el cluster tiene la autenticación IAM habilitada: firma cada pedido con las credenciales de abajo."),
                ];
                // AWS settings only sign requests with IAM on; credentials
                // only for the chosen authentication mode.
                v.extend(neptune::aws_fields().into_iter().map(|f| match f.key {
                    "region" | "auth_mode" => f.when("iam", &["true"]),
                    "profile" => f.when("auth_mode", &["profile"]),
                    "access_key_id" | "secret_access_key" | "session_token" => f.when("auth_mode", &["keys"]),
                    _ => f,
                }));
                v.push(Field::read_only());
                v
            },
            databases_label: "",
            has_schemas: false,
            object_kinds: kinds_for(f),
        },
    }
}

pub struct GraphDriver {
    flavor: Flavor,
    info: DriverInfo,
}

#[async_trait]
impl Driver for GraphDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn query_help(&self) -> &'static str {
        QUERY_HELP
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Cypher strings take backslash escapes, and there are no `BEGIN …
    /// END` bodies (for "run the statement at the cursor"; the driver
    /// splits editor scripts itself, with the client commands).
    fn script_dialect(&self) -> dbine_driver::ScriptDialect {
        dbine_driver::ScriptDialect { backslash_escapes: true, compound_blocks: false, ..dbine_driver::ScriptDialect::generic() }
    }

    /// `:begin` … `:commit` and the Auto/Manual switch (Neo4j, Memgraph).
    fn supports_manual_transactions(&self) -> bool {
        self.flavor != Flavor::Neptune
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    /// `SHOW INDEXES` with `readCount` (Neo4j 5); Memgraph lists them
    /// without counters; Neptune has no indexes (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        self.flavor != Flavor::Neptune
    }

    /// `UNWIND $rows … CREATE` by windows, each an explicit Bolt transaction
    /// (see `transfer.rs`). Not Neptune: each HTTP request commits on its
    /// own, so a cancelled load could commit after it returned.
    fn supports_bulk_load(&self) -> bool {
        self.flavor != Flavor::Neptune
    }

    fn capabilities(&self) -> Capabilities {
        let dbs = self.flavor != Flavor::Neptune;
        // Only Neo4j makes one transaction wait on another's lock (see blocking.rs).
        let locks = self.flavor == Flavor::Neo4j;
        Capabilities {
            create_database: dbs,
            drop_database: dbs,
            foreign_keys: false,
            monitor: true,
            blocking: locks,
            kill_session: locks,
            // Every flavor lists its transactions (Neptune its queries) and
            // stops one (see processes.rs).
            processes: true,
            cancel_query: true,
            ..Default::default()
        }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        ddl::designer(self.flavor)
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        ddl::templates(self.flavor)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        ddl::table_ddl(self.flavor, table, parts)
    }

    /// Neptune has no user-defined indexes or constraints: nothing to sync.
    fn supports_schema_sync(&self) -> bool {
        self.flavor != Flavor::Neptune
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(self.flavor, changes)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        ddl::insert_script(self.flavor, target, columns, rows)
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        ddl::update_script(self.flavor, target, changes)
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        ddl::delete_script(self.flavor, target, keys)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters)
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

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let db = database.filter(|d| !d.is_empty()).unwrap_or(cfg.database.trim()).to_string();
        let tag = uuid::Uuid::new_v4().to_string();
        let (transport, target) = if self.flavor == Flavor::Neptune {
            let c = neptune::Client::new(cfg).await?;
            // Proves the endpoint and the credentials.
            c.json("/status").await.map_err(|e| match e {
                Error::Query(m) => Error::Connect(m),
                e => e,
            })?;
            (Transport::Http(c), None)
        } else {
            let t = bolt::target(cfg, 7687);
            (Transport::Bolt(Conn::open(&t, USER_AGENT).await?), Some(t))
        };
        let mut s = GraphSession {
            flavor: self.flavor,
            transport,
            target,
            db,
            read_only: cfg.read_only,
            tag,
            dirty: false,
            current: Arc::new(Mutex::new(String::new())),
            profiler: None,
            params: Vec::new(),
            tx: false,
            manual: false,
            interrupted: Arc::default(),
            line_base: 0,
        };
        if self.flavor == Flavor::Memgraph && !s.db.is_empty() && s.db != MEMGRAPH_DEFAULT_DB {
            let q = format!("USE DATABASE {}", cypher::ident(&s.db));
            s.query(&q).await?;
        }
        Ok(Box::new(s))
    }
}

const USER_AGENT: &str = concat!("DBine/", env!("CARGO_PKG_VERSION"));
const MEMGRAPH_DEFAULT_DB: &str = "memgraph";

enum Transport {
    Bolt(Conn),
    Http(neptune::Client),
}

pub struct GraphSession {
    flavor: Flavor,
    transport: Transport,
    /// Bolt address and credentials, to reconnect and for the interrupter.
    target: Option<Target>,
    /// Neo4j: sent as `db` with each statement ("" = the user's home database).
    db: String,
    read_only: bool,
    /// Sent as `tx_metadata.dbine`, so the interrupter finds our transaction.
    tag: String,
    /// A statement was abandoned mid-stream (its future dropped): the Bolt
    /// connection is out of step and is replaced before the next one.
    dirty: bool,
    /// The statement Neptune is running, for the interrupter.
    current: Arc<Mutex<String>>,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// `:param` values, sent with every editor statement.
    params: Vec<(String, Bolt)>,
    /// An explicit transaction is open on the Bolt connection (`:begin`,
    /// or manual mode): statements go in it until commit / rollback.
    tx: bool,
    /// Manual transactions: the first statement opens one.
    manual: bool,
    /// Set by the interrupter: the server fails the terminated statement
    /// with an ordinary error (Neo.ClientError.Transaction.Terminated), so
    /// `execute` turns it into a cancel and runs nothing more, even on a
    /// run that continues on errors.
    interrupted: Arc<AtomicBool>,
    /// Lines before the running statement in the text `execute` numbers
    /// itself (`Whole`), so its notifications carry their script line;
    /// 0 otherwise (the app moves them).
    line_base: u32,
}

/// What a statement returned, besides its rows.
#[derive(Default)]
struct Ran {
    fields: Vec<String>,
    /// Bolt summary (`stats`, `plan`, `profile`, `notifications`…).
    meta: Option<Bolt>,
}

impl GraphSession {
    fn run_extra(&self, db: Option<&str>) -> Bolt {
        let mut extra = vec![("tx_metadata".to_string(), map([("dbine", Bolt::from(self.tag.as_str()))]))];
        let db = db.unwrap_or(&self.db);
        let send_db = match self.flavor {
            Flavor::Neo4j => !db.is_empty(),
            _ => !db.is_empty() && db != MEMGRAPH_DEFAULT_DB && db != self.db,
        };
        if send_db {
            extra.push(("db".into(), Bolt::from(db)));
        }
        if self.read_only && self.flavor == Flavor::Neo4j {
            extra.push(("mode".into(), Bolt::from("r")));
        }
        Bolt::Map(extra)
    }

    /// Run one statement, rows (as JSON values) to `on_row`.
    async fn run_with(
        &mut self,
        q: &str,
        db: Option<&str>,
        params: Bolt,
        on_row: &mut (dyn FnMut(&[String], Vec<Value>) + Send),
    ) -> Result<Ran> {
        // Inside an explicit transaction RUN takes no extra: the database,
        // access mode and metadata went with BEGIN.
        let extra = if self.tx { Bolt::Map(Vec::new()) } else { self.run_extra(db) };
        self.reconnect_if_dirty().await?;
        match &mut self.transport {
            Transport::Bolt(c) => {
                self.dirty = true;
                let r = c.run(q, params, extra, &mut |f, row| on_row(f, row.iter().map(value::to_json).collect())).await;
                self.dirty = matches!(r, Err(Error::Connect(_)));
                // A failure ends the transaction (the server rolls it back
                // on the RESET that follows).
                if r.is_err() {
                    self.tx = false;
                }
                let s = r?;
                Ok(Ran { fields: s.fields, meta: Some(s.meta) })
            }
            Transport::Http(c) => {
                *self.current.lock().expect("current") = q.to_string();
                let reply = c.query(q, None).await;
                self.current.lock().expect("current").clear();
                let reply = reply?;
                for r in reply.rows {
                    on_row(&reply.columns, r);
                }
                Ok(Ran { fields: reply.columns, meta: None })
            }
        }
    }

    /// Run and collect (catalog and monitor queries).
    async fn query(&mut self, q: &str) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        self.query_on(q, None).await
    }

    async fn query_on(&mut self, q: &str, db: Option<&str>) -> Result<(Vec<String>, Vec<Vec<Value>>)> {
        let mut rows = Vec::new();
        let ran = self.run_with(q, db, Bolt::Map(Vec::new()), &mut |_, r| rows.push(r)).await?;
        Ok((ran.fields, rows))
    }

    /// Rows as maps by column name.
    async fn records(&mut self, q: &str) -> Result<Vec<serde_json::Map<String, Value>>> {
        let (cols, rows) = self.query(q).await?;
        Ok(rows.into_iter().map(|r| cols.iter().cloned().zip(r).collect()).collect())
    }

    /// First column of each row, as text.
    async fn strings(&mut self, q: &str) -> Result<Vec<String>> {
        let (_, rows) = self.query(q).await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).map(|v| as_text(&v)).collect())
    }

    fn check_read_only(&self, stmt: &str) -> Result<()> {
        if self.read_only {
            if let Some(w) = cypher::write_reason(stmt) {
                return Err(Error::Query(format!(
                    "Conexión de solo lectura: se bloqueó una sentencia con `{w}`. Solo se permiten lecturas (MATCH, RETURN, SHOW…)."
                )));
            }
        }
        Ok(())
    }

    fn refuse_if_read_only(&self, what: &str) -> Result<()> {
        if self.read_only {
            return Err(Error::Query(format!("Conexión de solo lectura: no se puede {what}.")));
        }
        Ok(())
    }

    /// Run a statement into `out`: its rows, its counters and notifications
    /// as messages, and its plan when the server sent one.
    async fn run_into(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<Ran> {
        let mut begun = false;
        let params = Bolt::Map(self.params.clone());
        let ran = {
            let out_ref = &mut *out;
            self.run_with(stmt, None, params, &mut |fields, row| {
                if !begun {
                    out_ref.begin_result(columns(fields));
                    begun = true;
                }
                out_ref.push_row(row.iter().map(value::cell).collect(), max_rows);
            })
            .await?
        };
        let meta = ran.meta.as_ref();
        let (summary, changed) = meta.map(stats).unwrap_or_default();
        if !begun {
            if ran.fields.is_empty() {
                out.push_affected(changed);
            } else {
                out.begin_result(columns(&ran.fields));
            }
        }
        if !summary.is_empty() {
            out.info(summary);
        }
        if let Some(m) = meta {
            for mut n in notification_messages(m) {
                n.line = n.line.map(|l| l + self.line_base);
                out.message(n);
            }
            if let Some(p) = plan::neo4j(stmt, m) {
                out.plans.push(p);
            }
        }
        Ok(ran)
    }

    /// Indexes and constraints, from the server's catalog.
    async fn catalog(&mut self) -> Result<Vec<CatalogEntry>> {
        match self.flavor {
            Flavor::Neo4j => {
                let mut out = Vec::new();
                // `YIELD *`: the columns (options, propertyType…) vary by version.
                let ix = self.records("SHOW INDEXES YIELD *").await?;
                for r in ix {
                    if !r.get("owningConstraint").is_none_or(Value::is_null) {
                        continue;
                    }
                    out.push(CatalogEntry::neo4j(kinds::INDEX, &r));
                }
                let cs = self.records("SHOW CONSTRAINTS YIELD *").await?;
                out.extend(cs.iter().map(|r| CatalogEntry::neo4j(CONSTRAINT, r)));
                Ok(out)
            }
            Flavor::Memgraph => {
                let mut out = Vec::new();
                for r in self.records("SHOW INDEX INFO").await? {
                    // Vector indexes: with their name and settings below.
                    if !r.get("index type").map(as_text).unwrap_or_default().contains("vector") {
                        out.push(CatalogEntry::memgraph_index(&r));
                    }
                }
                if let Ok(rows) = self.records("SHOW VECTOR INDEX INFO").await {
                    out.extend(rows.iter().map(CatalogEntry::memgraph_vector));
                }
                for r in self.records("SHOW CONSTRAINT INFO").await? {
                    if let Some(e) = CatalogEntry::memgraph_constraint(&r) {
                        out.push(e);
                    }
                }
                Ok(out)
            }
            Flavor::Neptune => Ok(Vec::new()),
        }
    }

    async fn labels(&mut self, rel: bool) -> Result<Vec<String>> {
        let mut names = match self.flavor {
            Flavor::Neo4j => {
                let q = if rel {
                    "CALL db.relationshipTypes() YIELD relationshipType RETURN relationshipType"
                } else {
                    "CALL db.labels() YIELD label RETURN label"
                };
                self.strings(q).await?
            }
            Flavor::Memgraph => {
                let q = if rel { "SHOW EDGE_TYPES INFO" } else { "SHOW NODE_LABELS INFO" };
                match self.strings(q).await {
                    Ok(v) => v,
                    // Needs --storage-enable-schema-metadata: scan a sample instead.
                    Err(Error::Query(_)) => self.strings(scan_query(rel)).await?,
                    Err(e) => return Err(e),
                }
            }
            Flavor::Neptune => match self.neptune_summary().await {
                Some(s) => {
                    let key = if rel { "edgeLabels" } else { "nodeLabels" };
                    s.get(key).and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
                }
                None => self.strings(scan_query(rel)).await?,
            },
        };
        names.retain(|n| !n.is_empty());
        names.sort();
        names.dedup();
        Ok(names)
    }

    /// Neptune's graph summary (`/propertygraph/statistics/summary`), when
    /// statistics are on.
    async fn neptune_summary(&mut self) -> Option<Value> {
        let Transport::Http(c) = &self.transport else { return None };
        let v = c.json("/propertygraph/statistics/summary").await.ok()?;
        v.get("payload").and_then(|p| p.get("graphSummary")).cloned()
    }

    async fn sample(&mut self, obj: &ObjectRef) -> Result<Vec<Value>> {
        let q = if obj.kind == RELATIONSHIP {
            format!("MATCH ()-[e:{}]->() WITH e LIMIT 100 RETURN properties(e) AS p", cypher::ident(&obj.name))
        } else {
            format!("MATCH (e:{}) WITH e LIMIT 100 RETURN properties(e) AS p", cypher::ident(&obj.name))
        };
        let (_, rows) = self.query(&q).await?;
        Ok(rows.into_iter().filter_map(|r| r.into_iter().next()).collect())
    }

    /// `:use db` switches the session's database.
    async fn use_database(&mut self, name: &str) -> Result<()> {
        let name = name.trim().trim_matches('`').to_string();
        match self.flavor {
            Flavor::Neo4j => {
                // Proves it exists. The system database only takes
                // administration commands, so `RETURN 1` fails there.
                let probe = if name.eq_ignore_ascii_case("system") { "SHOW DEFAULT DATABASE YIELD name" } else { "RETURN 1" };
                self.query_on(probe, Some(&name)).await?;
            }
            Flavor::Memgraph => {
                self.query(&format!("USE DATABASE {}", cypher::ident(&name))).await?;
            }
            Flavor::Neptune => return Err(Error::Unsupported("Neptune tiene una sola base por cluster".into())),
        }
        self.db = name;
        Ok(())
    }

    /// The Bolt connection is replaced when a statement was abandoned
    /// mid-stream; an open transaction goes with the old one.
    async fn reconnect_if_dirty(&mut self) -> Result<()> {
        if !self.dirty {
            return Ok(());
        }
        if let (Transport::Bolt(_), Some(t)) = (&self.transport, &self.target) {
            self.transport = Transport::Bolt(Conn::open(t, USER_AGENT).await?);
        }
        self.dirty = false;
        if std::mem::take(&mut self.tx) {
            return Err(Error::Query("La transacción abierta se perdió al reiniciar la conexión: sus cambios no se guardaron.".into()));
        }
        Ok(())
    }

    async fn bolt(&mut self) -> Result<&mut Conn> {
        self.reconnect_if_dirty().await?;
        match &mut self.transport {
            Transport::Bolt(c) => Ok(c),
            Transport::Http(_) => Err(Error::Unsupported("Neptune no tiene transacciones explícitas ni parámetros de sesión.".into())),
        }
    }

    async fn begin_tx(&mut self) -> Result<()> {
        let extra = self.run_extra(None);
        self.bolt().await?.begin(extra).await?;
        self.tx = true;
        Ok(())
    }

    /// Commit (or roll back) the open transaction; it ends either way.
    async fn end_tx(&mut self, commit: bool) -> Result<()> {
        if !self.tx {
            return Ok(());
        }
        let c = self.bolt().await?;
        let r = if commit { c.commit().await } else { c.rollback().await };
        self.tx = false;
        r
    }

    /// An editor Cypher statement: in the open transaction, opening one
    /// first in manual mode.
    async fn cypher(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.check_read_only(stmt)?;
        if self.manual && !self.tx && self.flavor != Flavor::Neptune && !cypher::implicit_only(stmt) {
            self.begin_tx().await?;
            out.info("Transacción iniciada.");
        }
        self.run_into(stmt, max_rows, out).await.map_err(security::edition_hint)?;
        Ok(())
    }

    /// A cypher-shell client command (a line starting with `:`).
    async fn command(&mut self, line: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let body = line.trim_start_matches(':');
        let (name, arg) = body.split_once(char::is_whitespace).map_or((body, ""), |(n, a)| (n, a.trim()));
        match name.to_ascii_lowercase().as_str() {
            "use" => {
                if self.tx {
                    return Err(Error::Query("Hay una transacción abierta: confirmala (:commit) o deshacela (:rollback) antes de cambiar de base.".into()));
                }
                self.use_database(arg).await?;
                if !self.db.is_empty() {
                    // The tab's database selector follows it.
                    out.database = Some(self.db.clone());
                }
                out.info(format!("Base de datos actual: {}", if self.db.is_empty() { "(la predeterminada)" } else { &self.db }));
            }
            "begin" => {
                if self.tx {
                    return Err(Error::Query("Ya hay una transacción abierta.".into()));
                }
                self.begin_tx().await?;
                out.info("Transacción iniciada.");
            }
            "commit" | "rollback" => {
                if !self.tx {
                    return Err(Error::Query("No hay ninguna transacción abierta.".into()));
                }
                let commit = name.eq_ignore_ascii_case("commit");
                self.end_tx(commit).await?;
                out.info(if commit { "Transacción confirmada." } else { "Transacción deshecha." });
            }
            "param" | "params" if arg.is_empty() || arg.eq_ignore_ascii_case("list") => {
                out.begin_result(columns(&["nombre".to_string(), "valor".to_string()]));
                for (k, v) in &self.params {
                    out.push_row(vec![Value::String(format!("${k}")), value::cell(&value::to_json(v))], max_rows);
                }
            }
            "param" | "params" if arg.eq_ignore_ascii_case("clear") => {
                self.params.clear();
                out.info("Se borraron los parámetros.");
            }
            "param" | "params" => self.set_param(arg, out).await?,
            other => {
                return Err(Error::Unsupported(format!(
                    "El comando :{other} no está disponible en DBine (sí :use, :begin, :commit, :rollback, :param y :params)."
                )))
            }
        }
        Ok(())
    }

    /// `:param nombre => expr`, `:param nombre: expr` or `:param {a: 1}`:
    /// the server evaluates the expression (it may use earlier parameters).
    async fn set_param(&mut self, arg: &str, out: &mut QueryOutcome) -> Result<()> {
        let (name, expr) = if arg.starts_with('{') {
            (None, arg)
        } else {
            let split = arg.split_once("=>").or_else(|| arg.split_once(':'));
            let Some((n, e)) = split.filter(|(n, e)| !n.trim().is_empty() && !e.trim().is_empty()) else {
                return Err(Error::Query("Usá :param nombre => valor (o :param {nombre: valor}).".into()));
            };
            (Some(n.trim().trim_matches('`').to_string()), e.trim())
        };
        let q = format!("RETURN {expr} AS value");
        self.check_read_only(&q)?;
        let params = Bolt::Map(self.params.clone());
        let extra = if self.tx { Bolt::Map(Vec::new()) } else { self.run_extra(None) };
        let r = self.bolt().await?.query(&q, params, extra).await;
        if r.is_err() {
            // As any failed RUN, it ended an open transaction.
            self.tx = false;
        }
        let (_, rows) = r?;
        let v = rows.into_iter().next().and_then(|r| r.into_iter().next()).unwrap_or(Bolt::Null);
        let set: Vec<(String, Bolt)> = match (name, v) {
            (Some(n), v) => vec![(n, v)],
            (None, Bolt::Map(m)) => m,
            (None, _) => return Err(Error::Query(":param {…} necesita un mapa.".into())),
        };
        for (k, v) in set {
            out.info(format!("${k} = {}", value::to_json(&v)));
            match self.params.iter_mut().find(|(n, _)| *n == k) {
                Some(p) => p.1 = v,
                None => self.params.push((k, v)),
            }
        }
        Ok(())
    }
}

/// A server failure of an editor statement with its code
/// (`Neo.ClientError…`, `Memgraph.ClientError…`, which the Bolt client
/// appends to the message) and its place in `stmt` (`(line 1, column 8
/// (offset: 7))`), relative to the statement.
fn structured(e: Error, stmt: &str) -> Error {
    let Error::Query(m) = e else { return e };
    let coded = m.strip_suffix(')').and_then(|s| s.rsplit_once(" (")).filter(|(_, code)| {
        let parts: Vec<&str> = code.split('.').collect();
        parts.len() >= 3 && !code.contains(' ') && parts[1].ends_with("Error")
    });
    let mut se = match coded {
        Some((msg, code)) => ScriptError::new(msg).with_code(code),
        None => ScriptError::new(m.clone()),
    };
    let offset = m.rfind("(offset: ").and_then(|i| m[i + 9..].split(')').next()?.trim().parse::<usize>().ok());
    if let Some(chars) = offset {
        let at = stmt.char_indices().nth(chars).map_or(stmt.len(), |(i, _)| i);
        se = se.at_offset(at).at_line(steps::line_at(stmt, at));
    } else if let Some((line, col)) = memgraph_position(&m) {
        // Memgraph: "Error on line 3 position 13" (1-based, in the statement).
        let at = steps::offset_of(stmt, line, col);
        se = se.at_offset(at).at_line(line);
    }
    Error::Statement(Box::new(se))
}

/// Memgraph's place of a syntax error: `line L position P`.
fn memgraph_position(m: &str) -> Option<(u32, u32)> {
    let i = m.find("on line ")?;
    let mut w = m[i + 8..].split_whitespace();
    let line = w.next()?.trim_end_matches([',', ':']).parse().ok()?;
    if w.next()? != "position" {
        return None;
    }
    let col = w.next()?.trim_end_matches(|c: char| !c.is_ascii_digit()).parse().ok()?;
    Some((line, col))
}

/// Notifications of a Bolt summary as messages: warnings and information,
/// with their code and line in the statement.
fn notification_messages(meta: &Bolt) -> Vec<Message> {
    let list = meta.get("notifications").map(Bolt::as_list).unwrap_or_default();
    list.iter()
        .zip(notifications(meta))
        .map(|(n, text)| {
            let sev = n.get("severity").and_then(Bolt::as_str).unwrap_or_default();
            Message {
                level: if sev.eq_ignore_ascii_case("WARNING") { MessageLevel::Warning } else { MessageLevel::Info },
                text,
                code: n.get("code").and_then(Bolt::as_str).map(str::to_string),
                line: n.get("position").and_then(|p| p.get("line")).and_then(Bolt::as_i64).filter(|l| *l > 0).map(|l| l as u32),
                ..Default::default()
            }
        })
        .collect()
}

fn scan_query(rel: bool) -> &'static str {
    if rel {
        "MATCH ()-[r]->() WITH r LIMIT 100000 RETURN DISTINCT type(r)"
    } else {
        "MATCH (n) WITH n LIMIT 100000 UNWIND labels(n) AS l RETURN DISTINCT l"
    }
}

fn columns(fields: &[String]) -> Vec<ResultColumn> {
    fields.iter().map(|f| ResultColumn { name: f.clone(), type_name: String::new() }).collect()
}

fn as_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        other => other.to_string(),
    }
}

/// Counters of a Bolt summary: a Spanish line and the total changed.
fn stats(meta: &Bolt) -> (String, u64) {
    const NAMES: &[(&str, &str)] = &[
        ("nodes-created", "nodos creados"),
        ("nodes-deleted", "nodos borrados"),
        ("relationships-created", "relaciones creadas"),
        ("relationships-deleted", "relaciones borradas"),
        ("properties-set", "propiedades asignadas"),
        ("labels-added", "etiquetas agregadas"),
        ("labels-removed", "etiquetas quitadas"),
        ("indexes-added", "índices creados"),
        ("indexes-removed", "índices borrados"),
        ("constraints-added", "restricciones creadas"),
        ("constraints-removed", "restricciones borradas"),
        ("system-updates", "cambios de sistema"),
    ];
    let Some(s) = meta.get("stats") else { return (String::new(), 0) };
    let mut parts = Vec::new();
    let mut total = 0u64;
    for (k, label) in NAMES {
        if let Some(n) = s.get(k).and_then(Bolt::as_i64).filter(|n| *n > 0) {
            parts.push(format!("{n} {label}"));
            if !k.starts_with("properties") && !k.starts_with("labels") {
                total += n as u64;
            }
        }
    }
    // Only property / label changes: those are the "rows" affected.
    if total == 0 {
        total = NAMES.iter().filter_map(|(k, _)| s.get(k).and_then(Bolt::as_i64)).filter(|n| *n > 0).map(|n| n as u64).sum();
    }
    (if parts.is_empty() { String::new() } else { format!("{}.", parts.join(", ")) }, total)
}

fn notifications(meta: &Bolt) -> Vec<String> {
    meta.get("notifications")
        .map(Bolt::as_list)
        .unwrap_or_default()
        .iter()
        .map(|n| {
            let sev = n.get("severity").and_then(Bolt::as_str).unwrap_or_default();
            let title = n.get("title").and_then(Bolt::as_str).unwrap_or_default();
            let desc = n.get("description").and_then(Bolt::as_str).unwrap_or_default();
            format!("{}{title}{}{desc}", if sev.is_empty() { String::new() } else { format!("[{sev}] ") }, if desc.is_empty() { "" } else { " — " })
        })
        .collect()
}

/// An index or constraint of the catalog.
#[derive(Debug, Clone)]
struct CatalogEntry {
    kind: &'static str,
    name: String,
    target: String,
    relationship: bool,
    /// RANGE, TEXT, POINT, FULLTEXT, VECTOR, LOOKUP, UNIQUE, EXISTS, KEY (or the server's word).
    spec_kind: String,
    properties: Vec<String>,
    definition: String,
    /// See [`ddl::IndexSpec::options`].
    options: BTreeMap<String, String>,
}

fn strs(v: Option<&Value>) -> Vec<String> {
    v.and_then(Value::as_array).map(|a| a.iter().filter_map(Value::as_str).map(str::to_string).collect()).unwrap_or_default()
}

/// Whether an `indexConfig` entry has Neo4j's default value (left out,
/// so only real differences show).
fn default_index_config(k: &str, v: &Value) -> bool {
    let nums = |want: &[f64]| v.as_array().is_some_and(|a| a.len() == want.len() && a.iter().zip(want).all(|(x, w)| x.as_f64() == Some(*w)));
    const M: f64 = 1_000_000.0;
    match k {
        "fulltext.analyzer" => v.as_str() == Some("standard-no-stop-words"),
        "fulltext.eventually_consistent" => v.as_bool() == Some(false),
        "vector.hnsw.m" => v.as_f64() == Some(16.0),
        "vector.hnsw.ef_construction" => v.as_f64() == Some(100.0),
        "vector.quantization.enabled" => v.as_bool() == Some(true),
        "spatial.cartesian.min" => nums(&[-M, -M]),
        "spatial.cartesian.max" => nums(&[M, M]),
        "spatial.cartesian-3d.min" => nums(&[-M, -M, -M]),
        "spatial.cartesian-3d.max" => nums(&[M, M, M]),
        "spatial.wgs-84.min" => nums(&[-180.0, -90.0]),
        "spatial.wgs-84.max" => nums(&[180.0, 90.0]),
        "spatial.wgs-84-3d.min" => nums(&[-180.0, -90.0, -M]),
        "spatial.wgs-84-3d.max" => nums(&[180.0, 90.0, M]),
        _ => false,
    }
}

/// An option's value as text: strings as they are, the rest as JSON.
fn option_text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// The options of a `SHOW INDEXES` / `SHOW CONSTRAINTS` row (`YIELD *`):
/// the non-default `indexConfig`, a type constraint's `propertyType` and
/// the labels of a full-text index over several.
fn neo4j_options(spec_kind: &str, r: &serde_json::Map<String, Value>) -> BTreeMap<String, String> {
    let mut o = BTreeMap::new();
    if spec_kind == "TYPE" {
        if let Some(t) = r.get(ddl::PROPERTY_TYPE).filter(|v| !v.is_null()) {
            o.insert(ddl::PROPERTY_TYPE.to_string(), option_text(t));
        }
    }
    if matches!(spec_kind, "FULLTEXT" | "VECTOR" | "POINT" | "TEXT" | "RANGE") {
        if let Some(cfg) = r.get("options").and_then(|x| x.get("indexConfig")).and_then(Value::as_object) {
            for (k, v) in cfg.iter().filter(|(k, v)| !v.is_null() && !default_index_config(k, v)) {
                o.insert(k.clone(), option_text(v));
            }
        }
    }
    let targets = strs(r.get("labelsOrTypes"));
    if targets.len() > 1 {
        o.insert(ddl::TARGETS.to_string(), targets.join(","));
    }
    o
}

impl CatalogEntry {
    fn neo4j(kind: &'static str, r: &serde_json::Map<String, Value>) -> Self {
        let t = r.get("type").map(as_text).unwrap_or_default();
        let spec_kind = match t.as_str() {
            "UNIQUENESS" | "RELATIONSHIP_UNIQUENESS" | "NODE_UNIQUENESS" => "UNIQUE".to_string(),
            "NODE_KEY" | "RELATIONSHIP_KEY" => "KEY".into(),
            "NODE_PROPERTY_EXISTENCE" | "RELATIONSHIP_PROPERTY_EXISTENCE" => "EXISTS".into(),
            "NODE_PROPERTY_TYPE" | "RELATIONSHIP_PROPERTY_TYPE" => "TYPE".into(),
            other => other.to_string(),
        };
        Self {
            kind,
            name: r.get("name").map(as_text).unwrap_or_default(),
            target: strs(r.get("labelsOrTypes")).join(","),
            relationship: r.get("entityType").map(as_text).as_deref() == Some("RELATIONSHIP"),
            options: neo4j_options(&spec_kind, r),
            spec_kind,
            properties: strs(r.get("properties")),
            definition: r.get("createStatement").map(as_text).unwrap_or_default(),
        }
    }

    /// `SHOW INDEX INFO`: `index type` (label, label+property, edge-type,
    /// edge-type+property, text, point, vector…), `label`, `property`.
    fn memgraph_index(r: &serde_json::Map<String, Value>) -> Self {
        let t = r.get("index type").map(as_text).unwrap_or_default();
        let target = r.get("label").map(as_text).unwrap_or_default();
        let properties = match r.get("property") {
            Some(Value::Array(_)) => strs(r.get("property")),
            Some(Value::String(s)) if !s.is_empty() => vec![s.clone()],
            _ => Vec::new(),
        };
        let relationship = t.starts_with("edge");
        let spec_kind = if t.contains("text") {
            "TEXT"
        } else if t.contains("point") {
            "POINT"
        } else if t.contains("vector") {
            "VECTOR"
        } else {
            "RANGE"
        };
        let name = if properties.is_empty() { format!(":{target}") } else { format!(":{target}({})", properties.join(", ")) };
        let spec = ddl::IndexSpec { name: String::new(), target: target.clone(), relationship, kind: spec_kind.into(), properties: properties.clone(), options: BTreeMap::new() };
        let definition = if spec_kind == "VECTOR" {
            format!("// índice vectorial {name}")
        } else {
            ddl::create(Flavor::Memgraph, &spec, false).map(|s| format!("{s};")).unwrap_or_default()
        };
        Self { kind: kinds::INDEX, name: format!("{} {name}", t), target, relationship, spec_kind: spec_kind.into(), properties, definition, options: BTreeMap::new() }
    }

    /// `SHOW VECTOR INDEX INFO`: `index_name`, `label` (`:L`), `property`,
    /// `dimension`, `capacity`, `metric`, `scalar_kind`, `index_type`.
    fn memgraph_vector(r: &serde_json::Map<String, Value>) -> Self {
        let g = |k: &str| r.get(k).filter(|v| !v.is_null());
        let target = g("label").map(as_text).unwrap_or_default().trim_start_matches(':').to_string();
        let relationship = g("index_type").map(as_text).unwrap_or_default().starts_with("edge");
        let properties = g("property").map(as_text).into_iter().filter(|p| !p.is_empty()).collect::<Vec<_>>();
        let mut options = BTreeMap::new();
        // Not `capacity`: the server reports what it reserved (rounded up,
        // and growing), not what was asked, so it would never read the same.
        for k in ["dimension", "metric", "scalar_kind"] {
            if let Some(v) = g(k).filter(|v| !(k == "scalar_kind" && v.as_str() == Some("f32"))) {
                options.insert(k.to_string(), option_text(v));
            }
        }
        let name = g("index_name").map(as_text).unwrap_or_default();
        let spec = ddl::IndexSpec { name: name.clone(), target: target.clone(), relationship, kind: "VECTOR".into(), properties: properties.clone(), options: options.clone() };
        let definition = ddl::create(Flavor::Memgraph, &spec, false).map(|s| format!("{s};")).unwrap_or_default();
        Self { kind: kinds::INDEX, name, target, relationship, spec_kind: "VECTOR".into(), properties, definition, options }
    }

    /// `SHOW CONSTRAINT INFO`: `constraint type` (unique, exists, data_type), `label`, `properties`.
    fn memgraph_constraint(r: &serde_json::Map<String, Value>) -> Option<Self> {
        let t = r.get("constraint type").map(as_text).unwrap_or_default();
        let target = r.get("label").map(as_text).unwrap_or_default();
        let properties = match r.get("properties") {
            Some(Value::Array(_)) => strs(r.get("properties")),
            Some(Value::String(s)) => s.split(',').map(|p| p.trim().to_string()).filter(|p| !p.is_empty()).collect(),
            _ => Vec::new(),
        };
        let mut options = BTreeMap::new();
        let spec_kind = match t.as_str() {
            "unique" => "UNIQUE",
            "exists" => "EXISTS",
            _ => {
                options.insert(ddl::PROPERTY_TYPE.to_string(), r.get("data_type").map(as_text).unwrap_or_default());
                "TYPE"
            }
        };
        let name = format!("{t} :{target}({})", properties.join(", "));
        let spec = ddl::IndexSpec { name: String::new(), target: target.clone(), relationship: false, kind: spec_kind.into(), properties: properties.clone(), options: options.clone() };
        let definition = ddl::create(Flavor::Memgraph, &spec, false).ok().map(|s| format!("{s};"))?;
        Some(Self { kind: CONSTRAINT, name, target, relationship: false, spec_kind: spec_kind.into(), properties, definition, options })
    }

    fn index_def(&self) -> Option<IndexDef> {
        matches!(self.spec_kind.as_str(), "RANGE" | "TEXT" | "POINT" | "FULLTEXT" | "VECTOR" | "UNIQUE" | "EXISTS" | "KEY" | "TYPE").then(|| IndexDef {
            name: self.name.clone(),
            columns: self.properties.clone(),
            unique: matches!(self.spec_kind.as_str(), "UNIQUE" | "KEY"),
            kind: Some(self.spec_kind.clone()),
            filter: None,
            options: self.options.clone(),
            ..Default::default()
        })
    }
}

/// Properties of sample entities as columns: types seen (most frequent
/// first), nullable when missing from some.
pub fn infer_columns(samples: &[Value]) -> Vec<ColumnInfo> {
    let mut names: Vec<String> = Vec::new();
    for s in samples {
        for k in s.as_object().map(|o| o.keys().cloned().collect::<Vec<_>>()).unwrap_or_default() {
            if !names.contains(&k) {
                names.push(k);
            }
        }
    }
    names
        .into_iter()
        .map(|name| {
            let mut types: Vec<(&'static str, usize)> = Vec::new();
            let mut present = 0;
            for v in samples.iter().filter_map(|s| s.get(&name)).filter(|v| !v.is_null()) {
                present += 1;
                let t = value::type_name(v);
                match types.iter_mut().find(|(n, _)| *n == t) {
                    Some(e) => e.1 += 1,
                    None => types.push((t, 1)),
                }
            }
            types.sort_by(|a, b| b.1.cmp(&a.1));
            ColumnInfo {
                data_type: if types.is_empty() { "NULL".into() } else { types.iter().map(|t| t.0).collect::<Vec<_>>().join("|") },
                nullable: present < samples.len(),
                primary_key: false,
                auto_increment: false,
                default_value: None,
                name,
            }
        })
        .collect()
}

/// Strip a leading `EXPLAIN` / `PROFILE` the user wrote.
fn strip_plan_prefix(stmt: &str) -> &str {
    let t = stmt.trim_start();
    for p in ["EXPLAIN", "PROFILE"] {
        if t.len() > p.len() && t[..p.len()].eq_ignore_ascii_case(p) && t[p.len()..].starts_with(char::is_whitespace) {
            return t[p.len()..].trim_start();
        }
    }
    t
}

#[async_trait]
impl Session for GraphSession {
    async fn server_version(&mut self) -> Result<String> {
        match self.flavor {
            Flavor::Neo4j => {
                let r = self.records("CALL dbms.components() YIELD name, versions, edition").await?;
                let first = r.first().cloned().unwrap_or_default();
                let v = strs(first.get("versions")).join(", ");
                let ed = first.get("edition").map(as_text).unwrap_or_default();
                Ok(format!("Neo4j {v} {ed}").trim().to_string())
            }
            Flavor::Memgraph => {
                let v = self.strings("SHOW VERSION").await.ok().and_then(|v| v.into_iter().next()).unwrap_or_default();
                Ok(format!("Memgraph {v}").trim().to_string())
            }
            Flavor::Neptune => {
                let Transport::Http(c) = &self.transport else { unreachable!() };
                let st = c.json("/status").await?;
                let v = st.get("dbEngineVersion").map(as_text).unwrap_or_default();
                Ok(format!("Amazon Neptune {v}").trim().to_string())
            }
        }
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        match self.flavor {
            Flavor::Neo4j => {
                let r = self.query_on("SHOW DATABASES YIELD name, type RETURN DISTINCT name, type", Some("system")).await;
                match r {
                    Ok((_, rows)) => {
                        let mut v: Vec<String> = rows
                            .into_iter()
                            .filter(|r| r.get(1).map(as_text).as_deref() != Some("system") && r.first().map(as_text).as_deref() != Some("system"))
                            .filter_map(|r| r.into_iter().next().map(|v| as_text(&v)))
                            .collect();
                        v.sort();
                        Ok(v)
                    }
                    // No access to the system database: the session's.
                    Err(Error::Query(_)) => {
                        let (_, rows) = self.query("CALL db.info() YIELD name RETURN name").await?;
                        Ok(rows.into_iter().filter_map(|r| r.into_iter().next().map(|v| as_text(&v))).collect())
                    }
                    Err(e) => Err(e),
                }
            }
            Flavor::Memgraph => match self.records("SHOW DATABASES").await {
                Ok(rows) => {
                    let mut v: Vec<String> = rows.iter().filter_map(|r| r.values().next().map(as_text)).collect();
                    v.sort();
                    Ok(v)
                }
                // Community edition: a single database.
                Err(Error::Query(_)) => Ok(vec![if self.db.is_empty() { MEMGRAPH_DEFAULT_DB.into() } else { self.db.clone() }]),
                Err(e) => Err(e),
            },
            Flavor::Neptune => Ok(vec!["default".into()]),
        }
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let obj = |kind: &str, name: String, parent: Option<String>| DbObject { kind: kind.into(), schema: None, name, parent };
        let mut out: Vec<DbObject> = Vec::new();
        out.extend(self.labels(false).await?.into_iter().map(|n| obj(LABEL, n, None)));
        out.extend(self.labels(true).await?.into_iter().map(|n| obj(RELATIONSHIP, n, None)));
        if self.flavor == Flavor::Neptune {
            return Ok(out);
        }
        let mut cat = self.catalog().await.unwrap_or_default();
        cat.sort_by(|a, b| a.name.cmp(&b.name));
        out.extend(cat.into_iter().map(|e| obj(e.kind, e.name, Some(e.target))));
        let procs = match self.flavor {
            Flavor::Neo4j => self.strings("SHOW PROCEDURES YIELD name RETURN name ORDER BY name").await,
            _ => self.strings("CALL mg.procedures() YIELD name RETURN name ORDER BY name").await,
        };
        out.extend(procs.unwrap_or_default().into_iter().map(|n| obj(kinds::PROCEDURE, n, None)));
        if self.flavor == Flavor::Memgraph {
            if let Ok(rows) = self.records("SHOW TRIGGERS").await {
                out.extend(rows.iter().map(|r| obj(kinds::TRIGGER, r.get("trigger name").map(as_text).unwrap_or_default(), None)));
            }
        }
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if obj.kind != LABEL && obj.kind != RELATIONSHIP {
            return Ok(Vec::new());
        }
        let s = self.sample(obj).await?;
        Ok(infer_columns(&s))
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        match obj.kind.as_str() {
            LABEL | RELATIONSHIP => {
                let rel = obj.kind == RELATIONSHIP;
                let n = cypher::ident(&obj.name);
                let count_q =
                    if rel { format!("MATCH ()-[e:{n}]->() RETURN count(e)") } else { format!("MATCH (e:{n}) RETURN count(e)") };
                let count = self.strings(&count_q).await?.into_iter().next().unwrap_or_default();
                let cols = infer_columns(&self.sample(obj).await?);
                let mut text = if rel {
                    format!("// Tipo de relación :{n} — {count} relaciones\n")
                } else {
                    format!("// Etiqueta :{n} — {count} nodos\n")
                };
                if !cols.is_empty() {
                    text.push_str("// Propiedades (muestra de 100):\n");
                    for c in &cols {
                        text.push_str(&format!("//   {} {}{}\n", c.name, c.data_type, if c.nullable { "" } else { " (en todos)" }));
                    }
                }
                let defs: Vec<String> = self
                    .catalog()
                    .await
                    .unwrap_or_default()
                    .into_iter()
                    .filter(|e| e.target.split(',').any(|t| t == obj.name) && e.relationship == rel)
                    .map(|e| if e.definition.ends_with(';') { e.definition } else { format!("{};", e.definition) })
                    .collect();
                if !defs.is_empty() {
                    text.push_str("\n// Índices y restricciones\n");
                    text.push_str(&defs.join("\n"));
                    text.push('\n');
                }
                text.push_str(&format!("\n{}", self.browse_query(obj, 25)));
                Ok(Some(text))
            }
            k if k == kinds::INDEX || k == CONSTRAINT => {
                Ok(self.catalog().await?.into_iter().find(|e| e.kind == k && e.name == obj.name).map(|e| e.definition))
            }
            k if k == kinds::PROCEDURE => {
                let rows = match self.flavor {
                    Flavor::Neo4j => self.records("SHOW PROCEDURES YIELD name, signature, description, mode").await?,
                    _ => self.records("CALL mg.procedures() YIELD name, signature, is_write, path").await?,
                };
                Ok(rows.into_iter().find(|r| r.get("name").map(as_text).as_deref() == Some(obj.name.as_str())).map(|r| {
                    let mut t = format!("// {}\n", r.get("signature").map(as_text).unwrap_or_default());
                    for k in ["description", "mode", "is_write", "path"] {
                        if let Some(v) = r.get(k).filter(|v| !v.is_null()) {
                            t.push_str(&format!("// {k}: {}\n", as_text(v)));
                        }
                    }
                    t.push_str(&format!("CALL {}()", obj.name));
                    t
                }))
            }
            k if k == kinds::TRIGGER => {
                let rows = self.records("SHOW TRIGGERS").await?;
                Ok(rows.into_iter().find(|r| r.get("trigger name").map(as_text).as_deref() == Some(obj.name.as_str())).map(|r| {
                    let g = |k: &str| r.get(k).map(as_text).unwrap_or_default();
                    let event = g("event type");
                    let on = match event.as_str() {
                        "ANY" | "" => String::new(),
                        e => format!("ON {} ", e.replace('_', " ")),
                    };
                    format!("CREATE TRIGGER {}\n{on}{} COMMIT EXECUTE\n{}", cypher::ident(&obj.name), g("phase"), g("statement"))
                }))
            }
            _ => Ok(None),
        }
    }

    /// Labels and relationship types with their sampled properties and
    /// their indexes / constraints (for the script generator).
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let cat = self.catalog().await.unwrap_or_default();
        let mut out = Vec::new();
        for rel in [false, true] {
            for name in self.labels(rel).await? {
                let kind = if rel { RELATIONSHIP } else { LABEL };
                let obj = ObjectRef { kind: kind.into(), schema: None, name: name.clone() };
                let cols = infer_columns(&self.sample(&obj).await?);
                let indexes = cat
                    .iter()
                    .filter(|e| e.relationship == rel && e.target.split(',').any(|t| t == name))
                    .filter_map(CatalogEntry::index_def)
                    .collect();
                out.push(TableSchema {
                    kind: kind.into(),
                    name,
                    columns: cols
                        .into_iter()
                        .map(|c| ColumnDef { name: c.name, data_type: c.data_type, nullable: c.nullable, ..Default::default() })
                        .collect(),
                    indexes,
                    ..Default::default()
                });
            }
        }
        Ok(out)
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("crear una base")?;
        let n = cypher::ident(name.trim());
        match self.flavor {
            Flavor::Neo4j => self.query_on(&format!("CREATE DATABASE {n} IF NOT EXISTS WAIT"), Some("system")).await.map_err(enterprise_hint)?,
            Flavor::Memgraph => self.query(&format!("CREATE DATABASE {n}")).await?,
            Flavor::Neptune => return Err(Error::Unsupported("Neptune tiene una sola base por cluster".into())),
        };
        Ok(())
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        self.refuse_if_read_only("borrar una base")?;
        let n = cypher::ident(name.trim());
        match self.flavor {
            Flavor::Neo4j => self.query_on(&format!("DROP DATABASE {n} IF EXISTS WAIT"), Some("system")).await.map_err(enterprise_hint)?,
            Flavor::Memgraph => self.query(&format!("DROP DATABASE {n}")).await?,
            Flavor::Neptune => return Err(Error::Unsupported("Neptune tiene una sola base por cluster".into())),
        };
        Ok(())
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        let n = cypher::ident(&obj.name);
        if obj.kind == RELATIONSHIP {
            format!("MATCH ()-[r:{n}]->() RETURN r LIMIT {limit}")
        } else {
            format!("MATCH (n:{n}) RETURN n LIMIT {limit}")
        }
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let units = cypher::script(text);
        if units.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        let own = out.current_statement.is_none();
        self.interrupted.store(false, Ordering::SeqCst);
        for (i, u) in units.iter().enumerate() {
            if self.interrupted.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            let step = Step::start(out, own, i, u.start, u.line);
            let in_tx = self.tx;
            self.line_base = if own { u.line.saturating_sub(1) } else { 0 };
            let r = if u.command { self.command(&u.text, max_rows, out).await } else { self.cypher(&u.text, max_rows, out).await };
            self.line_base = 0;
            let r = r.map_err(|e| {
                if in_tx && !self.tx {
                    out.warning("La transacción se deshizo por el error: sus cambios no se guardaron.");
                }
                if self.interrupted.load(Ordering::SeqCst) {
                    return Error::Cancelled;
                }
                structured(e, &u.text)
            });
            step.end(out, r)?;
        }
        Ok(())
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        if self.flavor == Flavor::Neptune {
            return Ok(None);
        }
        Ok(Some(if self.tx { TxState::Open } else { TxState::Idle }))
    }

    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if !on && self.flavor == Flavor::Neptune {
            return Err(Error::Unsupported("Neptune no tiene transacciones explícitas.".into()));
        }
        self.manual = !on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.end_tx(true).await
    }

    async fn rollback(&mut self) -> Result<()> {
        self.end_tx(false).await
    }

    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let stmts = cypher::split(text);
        if stmts.is_empty() {
            return Err(Error::Query("No hay nada para ejecutar.".into()));
        }
        for stmt in stmts {
            let body = strip_plan_prefix(&stmt).to_string();
            if analyze {
                self.check_read_only(&body)?;
            }
            let writes = cypher::write_reason(&body).is_some();
            match self.flavor {
                Flavor::Neo4j => {
                    // PROFILE runs it once and returns rows plus the profile.
                    let q = format!("{} {body}", if analyze { "PROFILE" } else { "EXPLAIN" });
                    let before = out.plans.len();
                    match self.run_into(&q, max_rows, out).await {
                        Ok(_) if out.plans.len() > before => {
                            if let Some(p) = out.plans.last_mut() {
                                p.statement = body.clone();
                            }
                        }
                        Ok(_) => out.info(format!("`{}`: el servidor no devolvió un plan.", short(&body))),
                        // Administration commands (SHOW, CREATE INDEX…) have no plan.
                        Err(Error::Query(m)) => {
                            out.info(format!("`{}`: sin plan de ejecución ({m}).", short(&body)));
                            if analyze {
                                self.run_into(&body, max_rows, out).await?;
                            }
                        }
                        Err(e) => return Err(e),
                    }
                }
                Flavor::Memgraph => {
                    let profile = analyze && !writes;
                    let q = format!("{} {body}", if profile { "PROFILE" } else { "EXPLAIN" });
                    match self.query(&q).await {
                        Ok((_, rows)) => {
                            let lines: Vec<(String, Option<(f64, f64, f64)>)> = rows
                                .iter()
                                .map(|r| {
                                    let op = r.first().map(as_text).unwrap_or_default();
                                    let figures = profile.then(|| {
                                        let n = |i: usize| r.get(i).map(|v| v.as_f64().unwrap_or_else(|| plan::leading_number(&as_text(v)).unwrap_or(0.0))).unwrap_or(0.0);
                                        (n(1), n(2), n(3))
                                    });
                                    (op, figures)
                                })
                                .collect();
                            out.plans.push(plan::memgraph(&body, &lines, profile));
                        }
                        Err(Error::Query(m)) => out.info(format!("`{}`: sin plan de ejecución ({m}).", short(&body))),
                        Err(e) => return Err(e),
                    }
                    if analyze {
                        if writes {
                            out.info(format!("`{}` escribe: se muestra el plan estimado y se ejecutó una sola vez.", short(&body)));
                        }
                        self.run_into(&body, max_rows, out).await?;
                    }
                }
                Flavor::Neptune => {
                    let dynamic = analyze && !writes;
                    let Transport::Http(c) = &self.transport else { unreachable!() };
                    let c = c.clone();
                    match c.explain(&body, if dynamic { "dynamic" } else { "static" }).await {
                        Ok(t) => out.plans.push(plan::neptune(&body, &t, dynamic)),
                        Err(Error::Query(m)) => out.info(format!("`{}`: sin plan de ejecución ({m}).", short(&body))),
                        Err(e) => return Err(e),
                    }
                    if analyze {
                        if writes {
                            out.info(format!("`{}` escribe: se muestra el plan estimado y se ejecutó una sola vez.", short(&body)));
                        }
                        self.run_into(&body, max_rows, out).await?;
                    }
                }
            }
        }
        Ok(())
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

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let interrupted = self.interrupted.clone();
        match &self.transport {
            Transport::Bolt(c) => {
                let target = self.target.clone()?;
                let (flavor, tag, conn_id) = (self.flavor, self.tag.clone(), c.connection_id.clone());
                Some(Arc::new(move || {
                    interrupted.store(true, Ordering::SeqCst);
                    let (target, tag, conn_id) = (target.clone(), tag.clone(), conn_id.clone());
                    handle.spawn(async move {
                        if let Err(e) = terminate(flavor, &target, &tag, &conn_id).await {
                            tracing::warn!("no se pudo cancelar la consulta: {e}");
                        }
                    });
                }))
            }
            Transport::Http(c) => {
                let (c, current) = (c.clone(), self.current.clone());
                Some(Arc::new(move || {
                    interrupted.store(true, Ordering::SeqCst);
                    let q = current.lock().expect("current").clone();
                    if q.is_empty() {
                        return;
                    }
                    let c = c.clone();
                    handle.spawn(async move {
                        let _ = c.cancel_matching(&q).await;
                    });
                }))
            }
        }
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        match self.flavor {
            Flavor::Neo4j => monitor::neo4j(self).await,
            Flavor::Memgraph => monitor::memgraph(self).await,
            Flavor::Neptune => monitor::neptune(self).await,
        }
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        match self.flavor {
            Flavor::Neo4j => blocking::blocking(self).await,
            Flavor::Memgraph => Err(Error::Unsupported(
                "Memgraph no hace esperar a una transacción por otra: la segunda escritura falla enseguida con un error de serialización".into(),
            )),
            Flavor::Neptune => Err(Error::Unsupported("Neptune no informa bloqueos entre transacciones".into())),
        }
    }

    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        processes::processes(self).await
    }

    async fn cancel_query(&mut self, id: &str) -> Result<()> {
        processes::cancel(self, id).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        if self.flavor != Flavor::Neo4j {
            return Err(Error::Unsupported("este motor no permite terminar sesiones desde DBine".into()));
        }
        self.refuse_if_read_only("terminar transacciones")?;
        blocking::kill(self, id).await
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

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn backups(&mut self, database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        backup::history(self, database).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    /// The user's own privileges, per flavor (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        self.index_usage_report(table).await
    }
}

fn enterprise_hint(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains("Unsupported administration command") || m.contains("not available in community") => {
            Error::Unsupported(format!("Crear y borrar bases requiere Neo4j Enterprise (o Aura Business Critical): {m}"))
        }
        e => e,
    }
}

fn short(s: &str) -> String {
    let one = s.split_whitespace().collect::<Vec<_>>().join(" ");
    if one.chars().count() > 80 {
        format!("{}…", one.chars().take(80).collect::<String>())
    } else {
        one
    }
}

/// Terminate the transactions a session tagged (or its Bolt connection
/// runs), from a second connection.
async fn terminate(flavor: Flavor, target: &Target, tag: &str, conn_id: &str) -> Result<()> {
    let mut c = Conn::open(target, USER_AGENT).await?;
    let none = || Bolt::Map(Vec::new());
    let (fields, rows) = c.query("SHOW TRANSACTIONS", none(), none()).await?;
    let col = |name: &str| fields.iter().position(|f| f == name);
    let ids: Vec<String> = rows
        .iter()
        .filter(|r| {
            let meta_col = col("metaData").or_else(|| col("metadata"));
            let tagged = meta_col.and_then(|i| r.get(i)).map(value::to_json).is_some_and(|m| m.get("dbine").and_then(Value::as_str) == Some(tag));
            let same_conn = !conn_id.is_empty()
                && col("connectionId").and_then(|i| r.get(i)).and_then(Bolt::as_str) == Some(conn_id);
            tagged || same_conn
        })
        .filter_map(|r| {
            let i = col("transactionId").or_else(|| col("transaction_id"))?;
            Some(as_text(&value::to_json(r.get(i)?)))
        })
        .collect();
    if !ids.is_empty() {
        let list = ids.iter().map(|i| cypher::string(i)).collect::<Vec<_>>().join(", ");
        let q = match flavor {
            Flavor::Memgraph => format!("TERMINATE TRANSACTIONS {list}"),
            _ => format!("TERMINATE TRANSACTIONS {list}"),
        };
        c.query(&q, none(), none()).await?;
    }
    c.close().await;
    Ok(())
}

/// Used by the monitor: a bolt connection's server string.
impl GraphSession {
    fn bolt_server(&self) -> String {
        match &self.transport {
            Transport::Bolt(c) => format!("{} (Bolt {}.{})", c.server, c.version.0, c.version.1),
            Transport::Http(_) => String::new(),
        }
    }

    fn neptune_client(&self) -> Option<neptune::Client> {
        match &self.transport {
            Transport::Http(c) => Some(c.clone()),
            Transport::Bolt(_) => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn infos() {
        let d = drivers();
        let ids: Vec<&str> = d.iter().map(|d| d.info().id).collect();
        assert_eq!(ids, ["neo4j", "memgraph", "neptune"]);
        for x in &d {
            assert_eq!(x.info().family, Family::Graph);
            assert_eq!(x.info().language, Language::Cypher);
            assert!(x.capabilities().monitor);
        }
        assert!(d[2].designer().is_none() && d[0].designer().is_some());
        assert_eq!(d[2].info().databases_label, "");
    }

    #[test]
    fn inference() {
        let s = vec![json!({ "a": 1, "b": "x" }), json!({ "a": 2.5 }), json!({ "a": 3, "c": null })];
        let c = infer_columns(&s);
        assert_eq!(c.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c"]);
        assert_eq!(c[0].data_type, "INTEGER|FLOAT");
        assert!(!c[0].nullable && c[1].nullable && c[2].nullable);
        assert_eq!(c[2].data_type, "NULL");
    }

    #[test]
    fn errors_keep_code_and_place() {
        let m = "Invalid input 'RETRN': expected 'RETURN' (line 2, column 1 (offset: 10))\n\"RETRN 1\"\n ^ (Neo.ClientError.Statement.SyntaxError)";
        let e = structured(Error::Query(m.into()), "MATCH (n)\nRETRN 1").to_script_error();
        assert_eq!(e.code.as_deref(), Some("Neo.ClientError.Statement.SyntaxError"));
        assert!(e.message.starts_with("Invalid input 'RETRN'") && !e.message.contains("(Neo."), "{}", e.message);
        assert_eq!((e.offset, e.line), (Some(10), Some(2)));
        let e = structured(Error::Query("Unbound variable: x (Memgraph.ClientError.MemgraphError.MemgraphError)".into()), "RETURN x").to_script_error();
        assert_eq!((e.message.as_str(), e.code.as_deref(), e.offset), ("Unbound variable: x", Some("Memgraph.ClientError.MemgraphError.MemgraphError"), None));
        let stmt = "MATCH (n)\nWITH n\nRETURN n.a +  ;";
        let e = structured(Error::Query("Error on line 3 position 13. (Memgraph.ClientError.MemgraphError.MemgraphError)".into()), stmt).to_script_error();
        assert_eq!((e.line, e.offset), (Some(3), Some(29)));
        let e = structured(Error::Query("x (y z)".into()), "RETURN x").to_script_error();
        assert_eq!((e.message.as_str(), e.code), ("x (y z)", None));
        assert!(matches!(structured(Error::Cancelled, "x"), Error::Cancelled));
        let meta = map([(
            "notifications",
            Bolt::List(vec![map([
                ("severity", Bolt::from("WARNING")),
                ("code", Bolt::from("Neo.ClientNotification.Statement.CartesianProduct")),
                ("title", Bolt::from("t")),
                ("position", map([("line", Bolt::from(2i64))])),
            ])]),
        )]);
        let n = notification_messages(&meta);
        assert_eq!((n[0].level, n[0].code.as_deref(), n[0].line), (MessageLevel::Warning, Some("Neo.ClientNotification.Statement.CartesianProduct"), Some(2)));
    }

    #[test]
    fn plan_prefix_and_stats() {
        assert_eq!(strip_plan_prefix("  explain MATCH (n) RETURN n"), "MATCH (n) RETURN n");
        assert_eq!(strip_plan_prefix("PROFILE\nMATCH (n)"), "MATCH (n)");
        assert_eq!(strip_plan_prefix("EXPLAINED"), "EXPLAINED");
        let meta = map([("stats", map([("nodes-created", Bolt::Int(2)), ("properties-set", Bolt::Int(4))]))]);
        assert_eq!(stats(&meta), ("2 nodos creados, 4 propiedades asignadas.".to_string(), 2));
        let meta = map([("stats", map([("properties-set", Bolt::Int(3))]))]);
        assert_eq!(stats(&meta).1, 3);
    }

    #[test]
    fn catalog_entries() {
        let r: serde_json::Map<String, Value> = serde_json::from_value(json!({
            "name": "u", "type": "UNIQUENESS", "entityType": "NODE", "labelsOrTypes": ["P"], "properties": ["id"],
            "createStatement": "CREATE CONSTRAINT `u` FOR (n:`P`) REQUIRE (n.`id`) IS UNIQUE"
        }))
        .unwrap();
        let e = CatalogEntry::neo4j(CONSTRAINT, &r);
        assert_eq!((e.spec_kind.as_str(), e.target.as_str(), e.relationship), ("UNIQUE", "P", false));
        assert!(e.index_def().unwrap().unique);
        let m: serde_json::Map<String, Value> =
            serde_json::from_value(json!({ "index type": "label+property", "label": "P", "property": "name", "count": 3 })).unwrap();
        let e = CatalogEntry::memgraph_index(&m);
        assert_eq!(e.name, "label+property :P(name)");
        assert_eq!(e.definition, "CREATE INDEX ON :P(name);");
        let m: serde_json::Map<String, Value> =
            serde_json::from_value(json!({ "constraint type": "unique", "label": "P", "properties": ["a", "b"] })).unwrap();
        let e = CatalogEntry::memgraph_constraint(&m).unwrap();
        assert_eq!(e.definition, "CREATE CONSTRAINT ON (n:P) ASSERT n.a, n.b IS UNIQUE;");
    }
}
