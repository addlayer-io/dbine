//! Amazon Aurora DSQL: the PostgreSQL wire protocol with IAM
//! authentication. The password is a short-lived token that the AWS SDK
//! signs (SigV4 presigned `DbConnect` / `DbConnectAdmin`); TLS is mandatory
//! and the only database is `postgres`.
//!
//! Scripts run statement by statement: each one is prepared first (only to
//! learn its column types) and then run over the simple query protocol,
//! whose text cells become JSON according to those types.

#[path = "../../dynamodb/src/aws.rs"]
mod aws;
mod index_usage;
mod monitor;
mod permissions;
mod plan;
mod security;
mod structure;
mod sync;
mod transfer;

use aws_sdk_dsql::auth_token::{AuthTokenGenerator, Config as TokenConfig};
use aws_sdk_dsql::config::Region;
use dbine_driver::sql::{
    leading_keyword, qualified_name, select_top, split_script, split_statements, Limit, Quote, ScriptDefaults, ScriptDialect, ScriptMode,
    StatementKind,
};
use dbine_driver::{
    async_trait, ddl, json_bytes, json_f64, json_i64, kinds, ColumnDef, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo,
    Message, MessageLevel, ObjectRef, QueryOutcome, ResultColumn, Result, SchemaInfo, ScriptError, Session, TableSchema, TxState,
};
use futures::StreamExt;
use postgres_native_tls::MakeTlsConnector;
use serde_json::Value as Json;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio_postgres::config::SslMode;
use tokio_postgres::error::SqlState;
use tokio_postgres::types::Type;
use tokio_postgres::{AsyncMessage, Client, SimpleQueryMessage};

const PORT: u16 = 5432;
const DATABASE: &str = "postgres";
/// Seconds the signed token stays valid; it's only needed to log in.
const TOKEN_TTL: u64 = 900;
/// Schemas that aren't the user's.
const SYSTEM_SCHEMAS: &str = "('pg_catalog', 'information_schema', 'sys', 'pg_toast')";

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(DsqlDriver { info: info() })]
}

fn info() -> DriverInfo {
    let mut fields = vec![
        Field::new("host", "Endpoint del clúster", FieldKind::Text)
            .required()
            .placeholder("abcdefghijklmnopqrst.dsql.us-east-1.on.aws"),
        Field::port(),
        Field::new("username", "Rol de base de datos", FieldKind::Text)
            .default_value("admin")
            .help("\"admin\" usa un token DbConnectAdmin; cualquier otro rol, un token DbConnect."),
    ];
    // Only the credentials of the chosen AWS authentication mode.
    fields.extend(aws::fields().into_iter().map(|f| match f.key {
        "profile" => f.when("auth_mode", &["profile"]),
        "access_key_id" | "secret_access_key" | "session_token" => f.when("auth_mode", &["keys"]),
        _ => f,
    }));
    fields.push(Field::read_only());
    DriverInfo {
        id: "dsql",
        name: "Amazon Aurora DSQL",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "postgres",
        default_port: PORT,
        fields,
        databases_label: "",
        has_schemas: true,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::sequences(),
            ObjectKindInfo::types(),
            ObjectKindInfo::functions(),
        ],
    }
}

pub struct DsqlDriver {
    info: DriverInfo,
}

/// Where and as whom to log in.
#[derive(Debug, Clone, PartialEq)]
struct Target {
    host: String,
    port: u16,
    user: String,
    region: String,
    admin: bool,
}

/// `<id>.dsql.<region>.on.aws` → `<region>`.
fn region_from_host(host: &str) -> Option<String> {
    let parts: Vec<&str> = host.split('.').collect();
    let i = parts.iter().position(|p| p.starts_with("dsql"))?;
    parts.get(i + 1).filter(|r| !r.is_empty() && r.contains('-')).map(|r| r.to_string())
}

fn target(cfg: &ConnectionConfig) -> Result<Target> {
    let host = cfg.host.trim().to_string();
    if host.is_empty() {
        return Err(Error::Connect("falta el endpoint del clúster".into()));
    }
    let region = cfg
        .option("region")
        .map(|r| r.trim().to_string())
        .or_else(|| region_from_host(&host))
        .ok_or_else(|| Error::Connect("falta la región (no se pudo deducir del endpoint)".into()))?;
    let user = cfg.username.as_deref().map(str::trim).filter(|u| !u.is_empty()).unwrap_or("admin").to_string();
    Ok(Target { admin: user == "admin", host, port: cfg.port_or(PORT), user, region })
}

/// The IAM token that goes as the password.
async fn auth_token(cfg: &ConnectionConfig, t: &Target) -> Result<String> {
    let mut cfg = cfg.clone();
    cfg.options.insert("region".into(), t.region.clone());
    let conf = aws::sdk_config(&cfg).await?;
    let generator = AuthTokenGenerator::new(
        TokenConfig::builder()
            .hostname(&t.host)
            .region(Region::new(t.region.clone()))
            .expires_in(TOKEN_TTL)
            .build()
            .map_err(|e| Error::Connect(e.to_string()))?,
    );
    let token = if t.admin {
        generator.db_connect_admin_auth_token(&conf).await
    } else {
        generator.db_connect_auth_token(&conf).await
    }
    .map_err(|e| Error::AuthFailed(format!("no se pudo firmar el token de IAM: {e}")))?;
    Ok(token.as_str().to_string())
}

#[async_trait]
impl Driver for DsqlDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect::postgres()
    }

    /// One simple query per statement on the tab's connection.
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// psql goes on after an error (ON_ERROR_STOP off).
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    fn supports_manual_transactions(&self) -> bool {
        true
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// Multi-row `INSERT`s in windows that fit DSQL's transaction limits,
    /// on parallel connections (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// One database (`postgres`), no foreign keys; the monitor.
    fn capabilities(&self) -> dbine_driver::Capabilities {
        dbine_driver::Capabilities { monitor: true, ..Default::default() }
    }

    /// No foreign keys, sequences-backed identity nor comments: DSQL
    /// rejects them. Keys are usually `uuid DEFAULT gen_random_uuid()`.
    fn designer(&self) -> Option<DesignerSpec> {
        Some(DesignerSpec {
            schemas: true,
            auto_increment: false,
            comments: false,
            foreign_keys: false,
            ..DesignerSpec::sql_table(DATA_TYPES.to_vec())
        })
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        vec![
            CreateTemplate {
                kind: kinds::VIEW,
                label: "Nueva vista",
                template: "CREATE VIEW \"{schema}\".\"{name}\" AS\nSELECT\n    id,\n    nombre\nFROM \"{schema}\".\"tabla\";".into(),
            },
            CreateTemplate {
                kind: kinds::SEQUENCE,
                label: "Nueva secuencia",
                template: "CREATE SEQUENCE \"{schema}\".\"{name}\"\n    START WITH 1\n    INCREMENT BY 1\n    CACHE 1;".into(),
            },
            // Domains are DSQL's only user-defined types (no CREATE TYPE).
            CreateTemplate {
                kind: kinds::TYPE,
                label: "Nuevo dominio",
                template: "CREATE DOMAIN \"{schema}\".\"{name}\" AS numeric(12,2)\n    CHECK (VALUE >= 0);".into(),
            },
            // DSQL only takes SQL-language functions (no PL/pgSQL).
            CreateTemplate {
                kind: kinds::FUNCTION,
                label: "Nueva función",
                template: "CREATE FUNCTION \"{schema}\".\"{name}\"(p_id integer)\nRETURNS integer\nLANGUAGE sql\nAS $$\n    SELECT p_id * 2\n$$;".into(),
            },
        ]
    }

    /// The indexes, without usage counters (see `index_usage`).
    fn supports_index_usage(&self) -> bool {
        true
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(table_ddl(table, parts))
    }

    /// PostgreSQL roles signed in through IAM (no passwords).
    fn security(&self) -> Option<dbine_driver::SecuritySpec> {
        Some(security::spec())
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        security::script(action)
    }

    /// Schemas of the only database (see `security::schema_spec`).
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        Some(security::schema_spec())
    }

    /// Never with an owner: it's handed over after the grants
    /// (`schema_owner_script`).
    fn create_schema_script(&self, _database: Option<&str>, name: &str, _owner: Option<&str>) -> Result<String> {
        security::create_schema(name)
    }

    /// `ALTER SCHEMA … OWNER TO`, after the grants: DSQL's admin isn't a
    /// superuser, so once the schema is another role's the creator can't
    /// grant on it.
    fn schema_owner_script(&self, _database: Option<&str>, name: &str, owner: &str) -> Result<Option<String>> {
        security::schema_owner(name, owner).map(Some)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, _cascade: bool) -> Result<String> {
        security::drop_schema(name)
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let t = target(cfg)?;
        let token = auth_token(cfg, &t).await?;
        open(cfg, &t, &token, SslMode::Require).await
    }
}

/// Column types DSQL takes in tables (json/jsonb only exist at runtime).
const DATA_TYPES: &[&str] = &[
    "uuid", "integer", "bigint", "smallint", "numeric(18,2)", "real", "double precision", "boolean",
    "varchar(255)", "char(10)", "text", "bytea", "date", "time", "timestamp", "timestamptz", "interval",
];

/// PostgreSQL DDL without what DSQL refuses: no foreign keys, no
/// `COMMENT ON`, and indexes built with `CREATE INDEX ASYNC` (a plain
/// CREATE INDEX only works on empty tables). The session runs each
/// statement in its own transaction, as DSQL wants one DDL per transaction.
fn table_ddl(t: &TableSchema, parts: DdlParts) -> String {
    let flavor = ddl::SqlFlavor { comment_on: false, ..ddl::SqlFlavor::ansi() };
    let mut out = ddl::table_ddl(&flavor, t, DdlParts { indexes: false, foreign_keys: false, ..parts });
    if parts.indexes {
        for ix in &t.indexes {
            let s = structure::index_ddl(t, ix, parts.if_exists);
            if !out.is_empty() {
                out.push('\n');
            }
            out.push_str(&s);
        }
    }
    out
}

/// Test hook: the same session over plain password auth, so the wire and
/// catalog code can run against a local PostgreSQL (DSQL has no emulator).
#[doc(hidden)]
pub async fn connect_with_password(cfg: &ConnectionConfig, password: &str) -> Result<Box<dyn Session>> {
    let t = Target {
        host: cfg.host.clone(),
        port: cfg.port_or(PORT),
        user: cfg.username_or_empty().to_string(),
        region: String::new(),
        admin: false,
    };
    open(cfg, &t, password, if cfg.encrypt { SslMode::Require } else { SslMode::Disable }).await
}

async fn open(cfg: &ConnectionConfig, t: &Target, password: &str, ssl: SslMode) -> Result<Box<dyn Session>> {
    Ok(Box::new(open_session(cfg, t, password, ssl).await?))
}

/// What it takes to open more connections like a session's (the bulk
/// load's parallel windows). An IAM login signs a fresh token each time.
struct Reopen {
    cfg: ConnectionConfig,
    target: Target,
    password: String,
    ssl: SslMode,
}

async fn open_session(cfg: &ConnectionConfig, t: &Target, password: &str, ssl: SslMode) -> Result<DsqlSession> {
    let mut pg = tokio_postgres::Config::new();
    pg.host(&t.host)
        .port(t.port)
        .user(&t.user)
        .password(password)
        .dbname(DATABASE)
        .application_name("DBine")
        .connect_timeout(Duration::from_secs(15))
        .ssl_mode(ssl);
    let connector = native_tls::TlsConnector::builder()
        .danger_accept_invalid_certs(cfg.trust_server_certificate)
        .danger_accept_invalid_hostnames(cfg.trust_server_certificate)
        .build()
        .map_err(|e| Error::Connect(format!("TLS: {e}")))?;
    let tls = MakeTlsConnector::new(connector);

    let (client, mut connection) = tokio::time::timeout(Duration::from_secs(20), pg.connect(tls.clone()))
        .await
        .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
        .map_err(connect_error)?;

    let (tx, notices) = mpsc::unbounded_channel();
    tokio::spawn(async move {
        loop {
            match futures::future::poll_fn(|cx| connection.poll_message(cx)).await {
                Some(Ok(AsyncMessage::Notice(n))) => {
                    let level = match n.severity() {
                        "WARNING" => MessageLevel::Warning,
                        _ => MessageLevel::Info,
                    };
                    let text = format!("{}: {}", n.severity(), n.message());
                    let _ = tx.send(Message { level, text, code: Some(n.code().code().to_string()), ..Default::default() });
                }
                Some(Ok(_)) => {}
                Some(Err(e)) => {
                    tracing::debug!("dsql connection closed: {e}");
                    break;
                }
                None => break,
            }
        }
    });

    if cfg.read_only {
        // On top of the registry's ReadOnlySession; DSQL may refuse it.
        if let Err(e) = client.batch_execute("SET SESSION CHARACTERISTICS AS TRANSACTION READ ONLY").await {
            tracing::debug!("dsql: read-only session setting refused: {e}");
        }
    }
    let reopen = Reopen { cfg: cfg.clone(), target: t.clone(), password: password.to_string(), ssl };
    Ok(DsqlSession { client, tls, notices, reopen, manual: false, tx: TxState::Idle })
}

fn connect_error(e: tokio_postgres::Error) -> Error {
    match e.code() {
        Some(c) if *c == SqlState::INVALID_PASSWORD || *c == SqlState::INVALID_AUTHORIZATION_SPECIFICATION => {
            Error::AuthFailed(db_message(&e))
        }
        Some(_) => Error::Connect(db_message(&e)),
        None => Error::Connect(e.to_string()),
    }
}

fn err(e: tokio_postgres::Error) -> Error {
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

/// A statement failed: its SQLSTATE (as the code too) and where
/// (`position`, the 1-based character in the statement).
fn stmt_err(e: tokio_postgres::Error, stmt: &str) -> Error {
    let Some(db) = e.as_db_error() else { return err(e) };
    if db.code() == &SqlState::QUERY_CANCELED {
        return Error::Cancelled;
    }
    let state = db.code().code().to_string();
    let mut se = ScriptError::new(db_message(&e)).with_code(state.clone()).with_sqlstate(state);
    if let Some(tokio_postgres::error::ErrorPosition::Original(p)) = db.position() {
        let p = *p as usize;
        if p >= 1 {
            se = se.at_offset(stmt.char_indices().nth(p - 1).map_or(stmt.len(), |(b, _)| b));
        }
    }
    if matches!(db.severity(), "FATAL" | "PANIC") {
        se = se.fatal();
    }
    se.into()
}

/// Words that open a transaction in manual mode: anything but reads,
/// transaction control and session settings.
fn opens_transaction(stmt: &str) -> bool {
    const NO: &[&str] = &[
        "select", "with", "values", "table", "show", "explain", "begin", "start", "commit", "end", "rollback", "abort", "set", "reset", "discard",
        "savepoint", "release", "prepare",
    ];
    leading_keyword(stmt, &ScriptDialect::postgres()).is_some_and(|k| !NO.contains(&k.as_str()))
}

fn db_message(e: &tokio_postgres::Error) -> String {
    let Some(db) = e.as_db_error() else {
        return e.to_string();
    };
    let mut m = format!("{}: {}", db.severity(), db.message());
    if let Some(d) = db.detail() {
        m.push_str(&format!("\nDetalle: {d}"));
    }
    if let Some(h) = db.hint() {
        m.push_str(&format!("\nSugerencia: {h}"));
    }
    m
}

pub struct DsqlSession {
    client: Client,
    tls: MakeTlsConnector,
    notices: mpsc::UnboundedReceiver<Message>,
    reopen: Reopen,
    /// Autocommit off: a statement that writes opens a transaction.
    manual: bool,
    /// The transaction as the statements left it (tokio-postgres doesn't
    /// say): BEGIN opens it, COMMIT/ROLLBACK end it, an error fails it.
    tx: TxState,
}

impl DsqlSession {
    async fn rows3(&self, sql: &str, schema: &str, name: &str) -> Result<Vec<(String, String, String)>> {
        let rows = self.client.query(sql, &[&schema, &name]).await.map_err(err)?;
        Ok(rows.iter().map(|r| (r.get(0), r.get(1), r.get(2))).collect())
    }

    async fn first_text(&self, sql: &str, schema: &str, name: &str) -> Option<String> {
        let rows = self.client.query(sql, &[&schema, &name]).await.ok()?;
        rows.first().and_then(|r| r.try_get::<_, Option<String>>(0).ok().flatten())
    }

    async fn run(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        // Types only; if it can't be prepared the real run reports why.
        let types: Vec<Type> = match self.client.prepare(stmt).await {
            Ok(s) => s.columns().iter().map(|c| c.type_().clone()).collect(),
            Err(_) => Vec::new(),
        };
        let stream = self.client.simple_query_raw(stmt).await.map_err(|e| stmt_err(e, stmt))?;
        futures::pin_mut!(stream);
        let mut in_result = false;
        while let Some(msg) = stream.next().await {
            // Notices show while the statement runs.
            while let Ok(n) = self.notices.try_recv() {
                out.message(n);
            }
            match msg.map_err(|e| stmt_err(e, stmt))? {
                SimpleQueryMessage::RowDescription(cols) => {
                    out.begin_result(
                        cols.iter()
                            .enumerate()
                            .map(|(i, c)| ResultColumn {
                                name: c.name().to_string(),
                                type_name: types.get(i).map(|t| t.name().to_string()).unwrap_or_default(),
                            })
                            .collect(),
                    );
                    in_result = true;
                }
                SimpleQueryMessage::Row(row) => {
                    let cells = (0..row.len()).map(|i| cell(types.get(i), row.get(i))).collect();
                    out.push_row(cells, max_rows);
                }
                SimpleQueryMessage::CommandComplete(n) => {
                    if !in_result {
                        out.push_affected(n);
                    }
                    in_result = false;
                }
                _ => {}
            }
        }
        Ok(())
    }

    /// One statement's plan; `actual` runs it under EXPLAIN ANALYZE.
    /// `FORMAT JSON` first; the text format if DSQL refuses an option.
    async fn plan_of(&self, stmt: &str, actual: bool) -> Result<dbine_driver::Plan> {
        let q = if actual { format!("EXPLAIN (ANALYZE, FORMAT JSON) {stmt}") } else { format!("EXPLAIN (FORMAT JSON) {stmt}") };
        match self.lines(&q).await {
            Ok(raw) => plan::pg_json(stmt, &raw, actual).map_err(Error::Query),
            Err(e) => {
                tracing::debug!("dsql: EXPLAIN FORMAT JSON refused, trying text: {e}");
                let q = if actual { format!("EXPLAIN ANALYZE {stmt}") } else { format!("EXPLAIN {stmt}") };
                Ok(plan::pg_text(stmt, &self.lines(&q).await?, actual))
            }
        }
    }

    /// The first cell of every row, one per line (EXPLAIN's output).
    async fn lines(&self, sql: &str) -> Result<String> {
        let msgs = self.client.simple_query(sql).await.map_err(err)?;
        Ok(msgs
            .iter()
            .filter_map(|m| match m {
                SimpleQueryMessage::Row(r) => r.get(0).map(str::to_string),
                _ => None,
            })
            .collect::<Vec<_>>()
            .join("\n"))
    }

    async fn explain_script(&mut self, sql: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        use plan::StmtKind;
        for stmt in split_statements(sql) {
            match (analyze, plan::classify(&stmt)) {
                (false, StmtKind::Other) => out.messages.push(format!("Sin plan (no se ejecutó): {}", plan::short(&stmt))),
                (false, _) => out.plans.push(self.plan_of(&stmt, false).await?),
                // Reads run twice: once for the results, once under EXPLAIN
                // ANALYZE for the figures.
                (true, StmtKind::Read) => {
                    self.run(&stmt, max_rows, out).await?;
                    out.plans.push(self.plan_of(&stmt, true).await?);
                }
                (true, StmtKind::Other) => self.run(&stmt, max_rows, out).await?,
                // A write runs once: its estimated plan, then the statement.
                (true, StmtKind::Write) => {
                    out.plans.push(self.plan_of(&stmt, false).await?);
                    self.run(&stmt, max_rows, out).await?;
                }
            }
        }
        Ok(())
    }

    fn drain_notices(&mut self, out: &mut QueryOutcome) {
        while let Ok(n) = self.notices.try_recv() {
            out.message(n);
        }
    }

    /// Run one statement, following the transaction (see [`Self::tx`]).
    async fn run_tracked(&mut self, stmt: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.manual && self.tx == TxState::Idle && opens_transaction(stmt) {
            self.client.batch_execute("BEGIN").await.map_err(err)?;
            self.tx = TxState::Open;
        }
        let r = self.run(stmt, max_rows, out).await;
        match (&r, leading_keyword(stmt, &ScriptDialect::postgres()).as_deref()) {
            (_, Some("commit" | "end" | "rollback" | "abort")) => self.tx = TxState::Idle,
            (Ok(()), Some("begin" | "start")) => self.tx = TxState::Open,
            (Err(_), _) if self.tx != TxState::Idle => self.tx = TxState::Failed,
            _ => {}
        }
        r
    }
}

#[async_trait]
impl Session for DsqlSession {
    async fn server_version(&mut self) -> Result<String> {
        let row = self.client.query_one("SELECT version()", &[]).await.map_err(err)?;
        let v: String = row.get(0);
        Ok(if v.to_ascii_lowercase().contains("dsql") { v } else { format!("Aurora DSQL ({v})") })
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec![DATABASE.into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let rel = format!(
            "SELECT n.nspname::text, c.relname::text, c.relkind::text
             FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             WHERE c.relkind IN ('r', 'p', 'v', 'm', 'S')
               AND n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
             ORDER BY 1, 2"
        );
        let mut out = Vec::new();
        match self.client.query(rel.as_str(), &[]).await {
            Ok(rows) => {
                for r in rows {
                    let kind = match r.get::<_, String>(2).as_str() {
                        "v" | "m" => kinds::VIEW,
                        "S" => kinds::SEQUENCE,
                        _ => kinds::TABLE,
                    };
                    out.push(DbObject { kind: kind.into(), schema: Some(r.get(0)), name: r.get(1), parent: None });
                }
            }
            // A catalog subset without pg_class: the standard views.
            Err(_) => {
                let q = format!(
                    "SELECT table_schema::text, table_name::text, table_type::text FROM information_schema.tables
                     WHERE table_schema NOT IN {SYSTEM_SCHEMAS} ORDER BY 1, 2"
                );
                for r in self.client.query(q.as_str(), &[]).await.map_err(err)? {
                    let kind = if r.get::<_, String>(2) == "VIEW" { kinds::VIEW } else { kinds::TABLE };
                    out.push(DbObject { kind: kind.into(), schema: Some(r.get(0)), name: r.get(1), parent: None });
                }
            }
        }
        // Routines are optional in DSQL (SQL functions only); skip if refused.
        let fns = format!(
            "SELECT n.nspname::text, p.proname::text
             FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
             WHERE n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
             ORDER BY 1, 2"
        );
        if let Ok(rows) = self.client.query(fns.as_str(), &[]).await {
            let mut last: Option<(String, String)> = None;
            for r in rows {
                let key: (String, String) = (r.get(0), r.get(1));
                if last.as_ref() != Some(&key) {
                    out.push(DbObject {
                        kind: kinds::FUNCTION.into(),
                        schema: Some(key.0.clone()),
                        name: key.1.clone(),
                        parent: None,
                    });
                }
                last = Some(key);
            }
        }
        out.extend(structure::list_domains(&self.client).await);
        Ok(out)
    }

    /// Every schema in `pg_namespace` but the temporary and TOAST ones;
    /// pg_catalog, information_schema and sys are the system ones.
    async fn list_schemas(&mut self) -> Result<Option<Vec<SchemaInfo>>> {
        let rows = self
            .client
            .query(
                "SELECT nspname::text FROM pg_catalog.pg_namespace
                 WHERE nspname NOT LIKE 'pg\\_toast%' AND nspname NOT LIKE 'pg\\_temp\\_%' ORDER BY 1",
                &[],
            )
            .await
            .map_err(err)?;
        Ok(Some(
            rows.into_iter()
                .map(|r| r.get::<_, String>(0))
                .map(|name| SchemaInfo { system: matches!(name.as_str(), "pg_catalog" | "information_schema" | "sys") || name.starts_with("pg_"), name })
                .collect(),
        ))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let schema = obj.schema().unwrap_or("public");
        let pg = "SELECT a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod), NOT a.attnotnull,
                    pg_catalog.pg_get_expr(d.adbin, d.adrelid), a.attidentity::text,
                    EXISTS (SELECT 1 FROM pg_catalog.pg_index i
                            WHERE i.indrelid = c.oid AND i.indisprimary AND a.attnum = ANY (i.indkey))
                  FROM pg_catalog.pg_attribute a
                  JOIN pg_catalog.pg_class c ON c.oid = a.attrelid
                  JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                  LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
                  WHERE n.nspname = $1 AND c.relname = $2 AND a.attnum > 0 AND NOT a.attisdropped
                  ORDER BY a.attnum";
        if let Ok(rows) = self.client.query(pg, &[&schema, &obj.name]).await {
            return Ok(rows
                .iter()
                .map(|r| {
                    let default_value: Option<String> = r.get(3);
                    let identity: String = r.get(4);
                    ColumnInfo {
                        name: r.get(0),
                        data_type: r.get(1),
                        nullable: r.get(2),
                        primary_key: r.get(5),
                        auto_increment: !identity.is_empty()
                            || default_value.as_deref().is_some_and(|d| d.starts_with("nextval(")),
                        default_value,
                    }
                })
                .collect());
        }
        let std = "SELECT c.column_name::text, c.data_type::text, c.character_maximum_length::int,
                     c.is_nullable::text, c.column_default::text, c.is_identity::text,
                     EXISTS (SELECT 1 FROM information_schema.table_constraints tc
                             JOIN information_schema.key_column_usage k
                               ON k.constraint_name = tc.constraint_name AND k.table_schema = tc.table_schema
                              AND k.table_name = tc.table_name
                             WHERE tc.constraint_type = 'PRIMARY KEY' AND tc.table_schema = c.table_schema
                               AND tc.table_name = c.table_name AND k.column_name = c.column_name)
                   FROM information_schema.columns c
                   WHERE c.table_schema = $1 AND c.table_name = $2
                   ORDER BY c.ordinal_position";
        let rows = self.client.query(std, &[&schema, &obj.name]).await.map_err(err)?;
        Ok(rows
            .iter()
            .map(|r| {
                let data_type: String = r.get(1);
                let len: Option<i32> = r.get(2);
                ColumnInfo {
                    name: r.get(0),
                    data_type: match len {
                        Some(n) => format!("{data_type}({n})"),
                        None => data_type,
                    },
                    nullable: r.get::<_, String>(3) == "YES",
                    default_value: r.get(4),
                    auto_increment: r.get::<_, Option<String>>(5).as_deref() == Some("YES"),
                    primary_key: r.get(6),
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let schema = obj.schema().unwrap_or("public");
        let q = qualified_name(Quote::Double, Some(schema), &obj.name);
        match obj.kind.as_str() {
            kinds::VIEW => {
                let body = match self
                    .first_text(
                        "SELECT pg_catalog.pg_get_viewdef(c.oid, true)
                         FROM pg_catalog.pg_class c JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
                         WHERE n.nspname = $1 AND c.relname = $2",
                        schema,
                        &obj.name,
                    )
                    .await
                {
                    Some(b) => Some(b),
                    None => {
                        self.first_text(
                            "SELECT view_definition::text FROM information_schema.views
                             WHERE table_schema = $1 AND table_name = $2",
                            schema,
                            &obj.name,
                        )
                        .await
                    }
                };
                Ok(body.map(|b| format!("CREATE OR REPLACE VIEW {q} AS\n{}", b.trim_end())))
            }
            kinds::FUNCTION => {
                let defs = self
                    .rows3(
                        "SELECT pg_catalog.pg_get_functiondef(p.oid), ''::text, ''::text
                         FROM pg_catalog.pg_proc p JOIN pg_catalog.pg_namespace n ON n.oid = p.pronamespace
                         WHERE n.nspname = $1 AND p.proname = $2 ORDER BY p.oid",
                        schema,
                        &obj.name,
                    )
                    .await?;
                let all: Vec<String> = defs.into_iter().map(|(d, _, _)| d.trim_end().to_string()).collect();
                Ok((!all.is_empty()).then(|| all.join(";\n\n") + ";"))
            }
            kinds::TYPE => structure::domain_definition(&self.client, schema, &obj.name).await,
            kinds::SEQUENCE => {
                let rows = self
                    .client
                    .query(
                        "SELECT start_value::text, increment_by::text, min_value::text, max_value::text,
                                cache_size::text, cycle
                         FROM pg_catalog.pg_sequences WHERE schemaname = $1 AND sequencename = $2",
                        &[&schema, &obj.name],
                    )
                    .await;
                Ok(rows.ok().and_then(|r| r.into_iter().next()).map(|r| {
                    let v = |i: usize| r.get::<_, Option<String>>(i).unwrap_or_default();
                    format!(
                        "CREATE SEQUENCE {q}\n    START WITH {}\n    INCREMENT BY {}\n    MINVALUE {}\n    MAXVALUE {}\n    CACHE {}{};",
                        v(0),
                        v(1),
                        v(2),
                        v(3),
                        v(4),
                        if r.get::<_, bool>(5) { "\n    CYCLE" } else { "" }
                    )
                }))
            }
            _ => Ok(None),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let mut result = Ok(());
        for unit in split_script(text, &ScriptDialect::postgres()).into_iter().filter(|u| u.kind != StatementKind::ClientCommand) {
            let before = out.results.len();
            result = self.run_tracked(&unit.text, max_rows, out).await.map_err(|e| match e {
                Error::Statement(mut se) => {
                    se.offset = se.offset.map(|o| unit.start + o);
                    Error::Statement(se)
                }
                e => e,
            });
            if result.is_err() {
                // A query cancelled or failed mid-way leaves no empty grid.
                if out.results.len() > before && out.results[before..].iter().all(|r| r.total_rows == 0 && r.rows_affected.is_none()) {
                    out.results.truncate(before);
                }
                break;
            }
        }
        self.drain_notices(out);
        if let Err(Error::Statement(se)) = &mut result {
            // A line always (the statement's when DSQL gives no position).
            let at = se.offset.unwrap_or(0).min(text.len());
            se.line = Some(text[..at].matches('\n').count() as u32 + 1);
        }
        result
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        Ok(Some(self.tx))
    }

    /// Off: the next statement that writes opens a transaction (`BEGIN`),
    /// which stays open until Commit / Rollback. On: one still open is
    /// committed (the UI asks Commit / Rollback first), so later statements
    /// don't join it.
    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        if on {
            self.commit().await?;
        }
        self.manual = !on;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        if self.tx != TxState::Idle {
            // A failed transaction commits as a rollback.
            self.client.batch_execute("COMMIT").await.map_err(err)?;
            self.tx = TxState::Idle;
        }
        Ok(())
    }

    async fn rollback(&mut self) -> Result<()> {
        if self.tx != TxState::Idle {
            self.client.batch_execute("ROLLBACK").await.map_err(err)?;
            self.tx = TxState::Idle;
        }
        Ok(())
    }

    /// PostgreSQL's EXPLAIN: `(FORMAT JSON)` estimated; reads measured with
    /// `EXPLAIN (ANALYZE, FORMAT JSON)` after running them (so they run
    /// twice); writes run once and show the estimated plan.
    async fn explain(&mut self, text: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let res = self.explain_script(text, analyze, max_rows, out).await;
        self.drain_notices(out);
        res
    }

    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        Ok(monitor::snapshot(&self.client).await)
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(&self.client).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(&self.client, principal).await
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        let token = self.client.cancel_token();
        let tls = self.tls.clone();
        let rt = tokio::runtime::Handle::try_current().ok()?;
        Some(Arc::new(move || {
            let (token, tls) = (token.clone(), tls.clone());
            rt.spawn(async move {
                if let Err(e) = token.cancel_query(tls).await {
                    tracing::debug!("dsql cancel failed: {e}");
                }
            });
        }))
    }

    /// Two catalog queries: every column of every table, then every index
    /// (the primary key among them). DSQL has no foreign keys nor comments.
    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        let cols = format!(
            "SELECT n.nspname::text, c.relname::text, a.attname::text, pg_catalog.format_type(a.atttypid, a.atttypmod),
                    NOT a.attnotnull, pg_catalog.pg_get_expr(d.adbin, d.adrelid), a.attidentity::text
             FROM pg_catalog.pg_class c
             JOIN pg_catalog.pg_namespace n ON n.oid = c.relnamespace
             JOIN pg_catalog.pg_attribute a ON a.attrelid = c.oid AND a.attnum > 0 AND NOT a.attisdropped
             LEFT JOIN pg_catalog.pg_attrdef d ON d.adrelid = a.attrelid AND d.adnum = a.attnum
             WHERE c.relkind IN ('r', 'p') AND n.nspname NOT IN {SYSTEM_SCHEMAS} AND n.nspname NOT LIKE 'pg\\_%'
             ORDER BY 1, 2, a.attnum"
        );
        let mut out: Vec<TableSchema> = Vec::new();
        for r in self.client.query(cols.as_str(), &[]).await.map_err(err)? {
            let (schema, name): (String, String) = (r.get(0), r.get(1));
            if out.last().is_none_or(|t| t.schema.as_deref() != Some(schema.as_str()) || t.name != name) {
                out.push(TableSchema { kind: kinds::TABLE.into(), schema: Some(schema), name, ..Default::default() });
            }
            let default_value: Option<String> = r.get(5);
            // A serial's nextval() default becomes an identity column.
            let serial = default_value.as_deref().is_some_and(|d| d.starts_with("nextval("));
            out.last_mut().unwrap().columns.push(ColumnDef {
                name: r.get(2),
                data_type: r.get(3),
                nullable: r.get(4),
                auto_increment: serial || !r.get::<_, String>(6).is_empty(),
                default_value: default_value.filter(|_| !serial),
                ..Default::default()
            });
        }
        // Indexes (the primary key among them) and CHECKs.
        structure::complete(&self.client, &mut out).await?;
        Ok(out)
    }

    /// Role attributes from `pg_roles` (see `permissions`).
    async fn permissions(&mut self, _database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(&self.client).await
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        index_usage::report(&self.client, table).await.map(Some)
    }
}

/// A text cell as JSON, by the column's type.
fn cell(ty: Option<&Type>, v: Option<&str>) -> Json {
    let Some(v) = v else { return Json::Null };
    match ty {
        Some(t) if *t == Type::BOOL => Json::Bool(v == "t"),
        Some(t) if [Type::INT2, Type::INT4, Type::INT8, Type::OID].contains(t) => {
            v.parse::<i64>().map_or_else(|_| v.into(), json_i64)
        }
        Some(t) if *t == Type::FLOAT4 || *t == Type::FLOAT8 => match v.parse::<f64>() {
            Ok(f) if f.is_finite() => json_f64(f),
            _ => v.into(),
        },
        Some(t) if *t == Type::BYTEA => match v.strip_prefix("\\x").and_then(unhex) {
            Some(b) => json_bytes(&b),
            None => v.into(),
        },
        _ => v.into(),
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| u8::from_str_radix(s.get(i..i + 2)?, 16).ok()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_open_transactions() {
        assert!(opens_transaction("insert into t values (1)") && opens_transaction("-- x\ncreate table t (a int)"));
        assert!(!opens_transaction("select 1") && !opens_transaction("BEGIN") && !opens_transaction("set x = 1"));
    }

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
    use dbine_driver::{IndexDef, KeyDef};

    fn cfg(host: &str, user: Option<&str>, region: Option<&str>) -> ConnectionConfig {
        let mut c = ConnectionConfig { host: host.into(), username: user.map(Into::into), ..Default::default() };
        if let Some(r) = region {
            c.options.insert("region".into(), r.into());
        }
        c
    }

    #[test]
    fn region_comes_from_the_endpoint() {
        assert_eq!(region_from_host("abc123.dsql.us-east-1.on.aws").as_deref(), Some("us-east-1"));
        assert_eq!(region_from_host("abc.dsql-fnh4.eu-west-2.on.aws").as_deref(), Some("eu-west-2"));
        assert_eq!(region_from_host("localhost"), None);
    }

    #[test]
    fn target_defaults_to_admin_and_port_5432() {
        let t = target(&cfg("x.dsql.us-east-2.on.aws", None, None)).unwrap();
        assert_eq!(
            t,
            Target {
                host: "x.dsql.us-east-2.on.aws".into(),
                port: 5432,
                user: "admin".into(),
                region: "us-east-2".into(),
                admin: true
            }
        );
        let t = target(&cfg("x.dsql.us-east-2.on.aws", Some("app_reader"), Some("eu-central-1"))).unwrap();
        assert!(!t.admin);
        assert_eq!(t.region, "eu-central-1");
        assert!(target(&cfg("myhost", None, None)).is_err());
        assert!(target(&cfg("", None, Some("us-east-1"))).is_err());
    }

    #[tokio::test]
    async fn signs_an_admin_token_offline() {
        let mut c = cfg("abc.dsql.us-east-1.on.aws", None, None);
        for (k, v) in [("auth_mode", "keys"), ("access_key_id", "AKIDEXAMPLE"), ("secret_access_key", "secret")] {
            c.options.insert(k.into(), v.into());
        }
        let t = target(&c).unwrap();
        let tok = auth_token(&c, &t).await.unwrap();
        assert!(tok.starts_with("abc.dsql.us-east-1.on.aws/?Action=DbConnectAdmin&"), "{tok}");
        assert!(tok.contains("X-Amz-Signature=") && tok.contains("X-Amz-Expires=900"), "{tok}");
        let c2 = {
            let mut c2 = c.clone();
            c2.username = Some("reader".into());
            c2
        };
        let tok = auth_token(&c2, &target(&c2).unwrap()).await.unwrap();
        assert!(tok.contains("Action=DbConnect&"), "{tok}");
    }

    #[test]
    fn ddl_skips_what_dsql_refuses() {
        let t = TableSchema {
            kind: kinds::TABLE.into(),
            schema: Some("app".into()),
            name: "pedidos".into(),
            columns: vec![
                ColumnDef { name: "id".into(), data_type: "uuid".into(), nullable: false, default_value: Some("gen_random_uuid()".into()), ..Default::default() },
                ColumnDef { name: "cliente_id".into(), data_type: "uuid".into(), comment: Some("x".into()), ..Default::default() },
            ],
            primary_key: Some(KeyDef { name: Some("pedidos_pkey".into()), columns: vec!["id".into()] }),
            foreign_keys: vec![dbine_driver::ForeignKeyDef {
                columns: vec!["cliente_id".into()],
                ref_table: "clientes".into(),
                ref_columns: vec!["id".into()],
                ..Default::default()
            }],
            indexes: vec![IndexDef { name: "ux_cliente".into(), columns: vec!["cliente_id".into()], unique: true, ..Default::default() }],
            comment: Some("Pedidos".into()),
            ..Default::default()
        };
        let all = DdlParts { drop: true, if_exists: true, create: true, indexes: true, foreign_keys: true };
        let s = table_ddl(&t, all);
        assert!(s.starts_with("DROP TABLE IF EXISTS \"app\".\"pedidos\";\nCREATE TABLE \"app\".\"pedidos\" (\n"), "{s}");
        assert!(s.contains("\"id\" uuid DEFAULT gen_random_uuid() NOT NULL,"));
        assert!(s.contains("CONSTRAINT \"pedidos_pkey\" PRIMARY KEY (\"id\")"));
        assert!(s.ends_with("CREATE UNIQUE INDEX ASYNC IF NOT EXISTS \"ux_cliente\" ON \"app\".\"pedidos\" (\"cliente_id\");"), "{s}");
        assert!(!s.contains("COMMENT") && !s.contains("FOREIGN KEY"), "{s}");
        let only_ix = table_ddl(&t, DdlParts { indexes: true, ..Default::default() });
        assert_eq!(only_ix, "CREATE UNIQUE INDEX ASYNC \"ux_cliente\" ON \"app\".\"pedidos\" (\"cliente_id\");");
        assert_eq!(table_ddl(&t, DdlParts { foreign_keys: true, ..Default::default() }), "");
        let d = DsqlDriver { info: info() }.designer().unwrap();
        assert!(d.schemas && !d.foreign_keys && !d.auto_increment && !d.comments);
        let kinds_declared: Vec<&str> = info().object_kinds.iter().filter(|k| k.id != kinds::TABLE).map(|k| k.id).collect();
        let templates: Vec<&str> = DsqlDriver { info: info() }.create_templates().iter().map(|t| t.kind).collect();
        assert_eq!(templates, kinds_declared);
    }

    #[test]
    fn text_cells_follow_types() {
        assert_eq!(cell(Some(&Type::BOOL), Some("t")), Json::Bool(true));
        assert_eq!(cell(Some(&Type::INT8), Some("9007199254740993")), Json::from("9007199254740993"));
        assert_eq!(cell(Some(&Type::INT4), Some("-7")), Json::from(-7));
        assert_eq!(cell(Some(&Type::FLOAT8), Some("1.5")), Json::from(1.5));
        assert_eq!(cell(Some(&Type::FLOAT8), Some("NaN")), Json::from("NaN"));
        assert_eq!(cell(Some(&Type::NUMERIC), Some("1.10")), Json::from("1.10"));
        assert_eq!(cell(Some(&Type::BYTEA), Some("\\xcafe")), Json::from("0xCAFE"));
        assert_eq!(cell(None, None), Json::Null);
    }
}
