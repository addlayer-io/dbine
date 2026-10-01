//! SAP HANA through `hdbconnect_async`, a pure-Rust client of HANA's SQL
//! Command Network Protocol: no SAP client (hdbclient / ODBC) needed.
//!
//! The level below the connection is the schema, so `connect(database)`
//! runs `SET SCHEMA`.

mod backup;
mod blocking;
mod monitor;
mod permissions;
mod plan;
mod profiler;
mod schema;
mod script;
mod security;
mod structure;
mod transfer;

use dbine_driver::sql::{leading_keyword, select_top, Limit, Quote, ScriptDefaults, ScriptDialect};
use dbine_driver::{
    async_trait, json_bytes, json_f64, json_i64, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject,
    DdlParts, DesignerSpec, Driver, DriverInfo, Error, Family, Field, FieldKind, Language, MonitorSnapshot, ObjectKindInfo,
    Message, MessageLevel, ObjectRef, Plan, QueryOutcome, Result, ResultColumn, ScriptError, Session, TableSchema, TxState,
};
use hdbconnect_async::{
    ConnectParams, ConnectParamsBuilder, Connection, HdbError, HdbResponse, HdbReturnValue, HdbValue, ResultSet,
    ServerCerts,
};
use serde_json::Value;
use std::sync::Arc;
use std::time::Duration;

const DEFAULT_PORT: u16 = 30015;
const CONNECT_TIMEOUT: Duration = Duration::from_secs(20);
/// Characters of a CLOB / NCLOB shown in a cell.
const TEXT_CAP: u32 = 64 * 1024;
/// Bytes of a BLOB read for a cell (json_bytes shows 1 KiB).
const BLOB_CAP: u32 = 1025;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(HanaDriver { info: info() })]
}

struct HanaDriver {
    info: DriverInfo,
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "hana",
        name: "SAP HANA",
        family: Family::Relational,
        language: Language::Sql,
        dialect: "standard",
        default_port: DEFAULT_PORT,
        fields: vec![
            Field::host(),
            Field::port().placeholder("30015").help(
                "3<instancia>15 en una base única; 3<instancia>41 (SQL del tenant) o 3<instancia>13 (SYSTEMDB) \
                 en contenedores; 443 en SAP HANA Cloud.",
            ),
            Field::new("database", "Base de datos tenant", FieldKind::Text)
                .placeholder("(ninguna)")
                .help("Opcional: nombre del tenant cuando el puerto es el de SYSTEMDB (3<instancia>13)."),
            Field::username().required(),
            Field::password(),
            Field::encrypt().help("Obligatorio en SAP HANA Cloud."),
            Field::trust_cert(),
            Field::read_only(),
        ],
        databases_label: "Esquemas",
        has_schemas: false,
        object_kinds: vec![
            ObjectKindInfo::tables(),
            ObjectKindInfo::views(),
            ObjectKindInfo::procedures(),
            ObjectKindInfo::functions(),
            ObjectKindInfo::triggers(),
            ObjectKindInfo::sequences(),
            ObjectKindInfo::synonyms(),
            ObjectKindInfo::types(),
        ],
    }
}

// ---------------------------------------------------------------- errors

fn message(e: &HdbError) -> String {
    if let Some(s) = e.server_error() {
        return format!("[{}] {}", s.code(), s.text());
    }
    // The useful text is usually in the source chain.
    let mut m = e.to_string();
    let mut src = std::error::Error::source(e);
    while let Some(s) = src {
        m.push_str(": ");
        m.push_str(&s.to_string());
        src = s.source();
    }
    m
}

fn err(e: HdbError) -> Error {
    Error::Query(message(&e))
}

/// A statement of `script` (starting at byte `start`) failed: HANA's code
/// and SQLSTATE, and where (`position`, the 1-based character in the
/// statement). A fatal error ends the script.
fn stmt_err(e: HdbError, script: &str, start: usize) -> Error {
    let Some(s) = e.server_error() else { return err(e) };
    let state = String::from_utf8_lossy(s.sqlstate()).trim().to_string();
    let fatal = matches!(s.severity(), hdbconnect_async::Severity::Fatal);
    server_error(s.code(), s.text(), &state, s.position(), fatal, script, start).into()
}

fn server_error(code: i32, text: &str, state: &str, position: i32, fatal: bool, script: &str, start: usize) -> ScriptError {
    let mut se = ScriptError::new(format!("[{code}] {text}")).with_code(code.to_string());
    if !state.is_empty() && state != "HY000" {
        se = se.with_sqlstate(state);
    }
    let start = start.min(script.len());
    let offset = match usize::try_from(position) {
        Ok(p) if p >= 1 => script[start..].char_indices().nth(p - 1).map_or(start, |(b, _)| start + b),
        _ => start,
    };
    se = se.at_offset(offset).at_line(script[..offset].matches('\n').count() as u32 + 1);
    if fatal {
        se = se.fatal();
    }
    se
}

fn connect_err(e: HdbError) -> Error {
    // 10: authentication failed; 414: password must be changed.
    let auth = matches!(e, HdbError::Authentication { .. })
        || e.server_error().is_some_and(|s| matches!(s.code(), 10 | 414 | 663));
    if auth {
        Error::AuthFailed(message(&e))
    } else {
        Error::Connect(message(&e))
    }
}

// ------------------------------------------------------------ connecting

fn params(cfg: &ConnectionConfig) -> Result<ConnectParams> {
    let host = cfg.host.trim();
    if host.is_empty() {
        return Err(Error::Connect("Falta el servidor.".into()));
    }
    let user = cfg.username_or_empty().trim();
    if user.is_empty() {
        return Err(Error::AuthFailed("Falta el usuario.".into()));
    }
    let mut b = ConnectParamsBuilder::new();
    b.hostname(host).port(cfg.port_or(DEFAULT_PORT)).dbuser(user).password(cfg.password_or_empty());
    if !cfg.database.trim().is_empty() {
        b.dbname(cfg.database.trim());
    }
    if cfg.encrypt {
        if cfg.trust_server_certificate {
            b.tls_without_server_verification();
        } else {
            b.tls_with(ServerCerts::RootCertificates);
        }
    }
    b.build().map_err(|e| Error::Connect(message(&e)))
}

#[async_trait]
impl Driver for HanaDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_profiler(&self) -> bool {
        true
    }

    fn supports_explain(&self) -> bool {
        true
    }

    /// hdbsql's: `;` outside quotes and comments; procedure, function and
    /// trigger bodies whole.
    fn script_dialect(&self) -> ScriptDialect {
        ScriptDialect { backtick_idents: false, ..ScriptDialect::generic() }
    }

    // `script_mode` stays `Whole`: the shared lexer doesn't yet keep an
    // anonymous `DO BEGIN … END` block whole, which this driver's splitter
    // (`script`) does. The driver runs the script statement by statement
    // itself and stops at the first error.

    /// hdbsql goes on after an error unless told to stop.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: true, confirm_unsafe_dml: true }
    }

    fn supports_manual_transactions(&self) -> bool {
        true
    }

    fn capabilities(&self) -> Capabilities {
        // The "databases" below a connection are schemas.
        Capabilities {
            create_database: true,
            drop_database: true,
            foreign_keys: true,
            monitor: true,
            // M_BLOCKED_TRANSACTIONS and ALTER SYSTEM DISCONNECT SESSION.
            blocking: true,
            kill_session: true,
        }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        Some(schema::designer())
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        schema::create_templates()
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        Ok(schema::table_ddl(table, parts))
    }

    fn supports_schema_sync(&self) -> bool {
        true
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        schema::sync_script(changes)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        // No multi-row VALUES in HANA: one INSERT per row.
        Ok(dbine_driver::ddl::insert_script(&schema::flavor(), target.schema(), &target.name, columns, rows, 1))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[dbine_driver::RowChange]) -> Result<String> {
        Ok(dbine_driver::ddl::update_script(&schema::flavor(), target.schema(), &target.name, changes))
    }

    fn delete_script(&self, target: &ObjectRef, keys: &[Vec<(String, Value)>]) -> Result<String> {
        Ok(dbine_driver::ddl::delete_script(&schema::flavor(), target.schema(), &target.name, keys))
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

    /// Batched prepared `INSERT` (see `transfer`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    async fn connect(&self, cfg: &ConnectionConfig, database: Option<&str>) -> Result<Box<dyn Session>> {
        let params = params(cfg)?;
        let conn = tokio::time::timeout(CONNECT_TIMEOUT, Connection::new(params.clone()))
            .await
            .map_err(|_| Error::Connect(format!("El servidor no respondió en {} s.", CONNECT_TIMEOUT.as_secs())))?
            .map_err(connect_err)?;
        conn.set_application("DBine").await;
        if let Some(schema) = database.filter(|d| !d.is_empty()) {
            conn.exec(format!("SET SCHEMA {}", quote(schema))).await.map_err(err)?;
        }
        let schema = single_text(&conn, "SELECT CURRENT_SCHEMA FROM DUMMY").await?.unwrap_or_default();
        Ok(Box::new(HanaSession { id: conn.id().await, conn, params, schema, profiler: None, dirty: false }))
    }
}

// --------------------------------------------------------------- session

struct HanaSession {
    conn: Connection,
    /// For the interrupter's side connection.
    params: ConnectParams,
    /// Server connection id, to cancel from another session.
    id: u32,
    schema: String,
    /// The running profiler, if any.
    profiler: Option<profiler::State>,
    /// Without autocommit: something changed since the last commit or
    /// rollback.
    dirty: bool,
}

fn quote(name: &str) -> String {
    dbine_driver::sql::quote_ident(Quote::Double, name)
}

/// First column of the first row, as text.
async fn single_text(conn: &Connection, sql: &str) -> Result<Option<String>> {
    let rows = conn.query(sql).await.map_err(err)?.into_rows().await.map_err(err)?;
    Ok(rows.into_iter().next().and_then(|mut row| row.next_value()).and_then(|v| text(&v)))
}

fn text(v: &HdbValue) -> Option<String> {
    match v {
        HdbValue::NULL => None,
        HdbValue::STRING(s) => Some(s.clone()),
        HdbValue::STR(s) => Some(s.to_string()),
        other => Some(other.to_string()),
    }
}

fn num(v: &HdbValue) -> Option<f64> {
    match v {
        HdbValue::DOUBLE(f) => Some(*f),
        HdbValue::REAL(f) => Some(f64::from(*f)),
        HdbValue::DECIMAL(d) => d.to_string().parse().ok(),
        other => int(other).map(|i| i as f64),
    }
}

/// Statements `EXPLAIN PLAN` takes.
fn plannable(stmt: &str) -> bool {
    let head = script::strip_leading_comments(stmt).split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
    matches!(head.as_str(), "SELECT" | "WITH" | "INSERT" | "UPDATE" | "DELETE" | "UPSERT" | "REPLACE" | "MERGE")
}

fn int(v: &HdbValue) -> Option<i64> {
    match v {
        HdbValue::TINYINT(n) => Some(i64::from(*n)),
        HdbValue::SMALLINT(n) => Some(i64::from(*n)),
        HdbValue::INT(n) => Some(i64::from(*n)),
        HdbValue::BIGINT(n) => Some(*n),
        HdbValue::DECIMAL(d) => d.to_string().parse().ok(),
        HdbValue::STRING(s) => s.parse().ok(),
        _ => None,
    }
}

impl HanaSession {
    /// The server's warnings of the last statement, with their codes.
    async fn warnings(&self, out: &mut QueryOutcome) {
        for w in self.conn.pop_warnings().await.unwrap_or_default() {
            out.message(Message {
                level: MessageLevel::Warning,
                text: format!("[{}] {}", w.code(), w.text()),
                code: Some(w.code().to_string()),
                ..Default::default()
            });
        }
    }

    /// Rows of a catalog query with string parameters, as values.
    async fn rows(&self, sql: &str, params: &[&str]) -> Result<Vec<Vec<HdbValue<'static>>>> {
        let response = self.conn.prepare_and_execute(sql, &params.to_vec()).await.map_err(err)?;
        let rs = response.into_result_set().map_err(err)?;
        let rows = rs.into_rows().await.map_err(err)?;
        let mut out = Vec::new();
        for row in rows {
            let mut vals = Vec::new();
            for v in row {
                vals.push(read_lob(v).await);
            }
            out.push(vals);
        }
        Ok(out)
    }

    fn owner(&self, obj: &ObjectRef) -> String {
        obj.schema().unwrap_or(&self.schema).to_string()
    }
}

/// LOBs of catalog rows (definitions) read into strings.
async fn read_lob(v: HdbValue<'static>) -> HdbValue<'static> {
    match v {
        HdbValue::ASYNC_CLOB(c) => c.into_string().await.map_or(HdbValue::NULL, HdbValue::STRING),
        HdbValue::ASYNC_NCLOB(c) => c.into_string().await.map_or(HdbValue::NULL, HdbValue::STRING),
        other => other,
    }
}

const EXPLAIN_ROWS: &str = "SELECT OPERATOR_ID, PARENT_OPERATOR_ID, OPERATOR_NAME, OPERATOR_DETAILS, SCHEMA_NAME,
       TABLE_NAME, TABLE_TYPE, OUTPUT_SIZE, SUBTREE_COST, EXECUTION_ENGINE
  FROM EXPLAIN_PLAN_TABLE WHERE STATEMENT_NAME = ? ORDER BY OPERATOR_ID";

const LIST_SCHEMAS: &str = "SELECT SCHEMA_NAME FROM SYS.SCHEMAS
  WHERE HAS_PRIVILEGES = 'TRUE'
    AND ((SCHEMA_NAME NOT LIKE '\\_SYS%' ESCAPE '\\'
          AND SCHEMA_NAME NOT IN ('SYS', 'PUBLIC', 'UIS', 'HANA_XS_BASE', 'SAP_XS_LM', 'SAP_REST_API',
                                  'SAP_PA_APL', 'SAPHANADB_SYS', 'BROKER_PO_USER', 'SAP_HANA_ADMIN'))
         OR SCHEMA_NAME = CURRENT_SCHEMA)
  ORDER BY SCHEMA_NAME";

const LIST_OBJECTS: &str = "
SELECT 'table', TABLE_NAME, NULL FROM SYS.TABLES WHERE SCHEMA_NAME = ? AND IS_SYSTEM_TABLE = 'FALSE' AND IS_USER_DEFINED_TYPE = 'FALSE'
UNION ALL SELECT 'view', VIEW_NAME, NULL FROM SYS.VIEWS WHERE SCHEMA_NAME = ?
UNION ALL SELECT 'procedure', PROCEDURE_NAME, NULL FROM SYS.PROCEDURES WHERE SCHEMA_NAME = ?
UNION ALL SELECT 'function', FUNCTION_NAME, NULL FROM SYS.FUNCTIONS WHERE SCHEMA_NAME = ?
UNION ALL SELECT 'trigger', TRIGGER_NAME, SUBJECT_TABLE_NAME FROM SYS.TRIGGERS WHERE SCHEMA_NAME = ?
UNION ALL SELECT 'sequence', SEQUENCE_NAME, NULL FROM SYS.SEQUENCES WHERE SCHEMA_NAME = ?
ORDER BY 2";

const TABLE_COLUMNS: &str = "
SELECT c.COLUMN_NAME, c.DATA_TYPE_NAME, c.LENGTH, c.SCALE, c.IS_NULLABLE, c.DEFAULT_VALUE, c.GENERATION_TYPE,
       CASE WHEN EXISTS (SELECT 1 FROM SYS.CONSTRAINTS k
                          WHERE k.SCHEMA_NAME = c.SCHEMA_NAME AND k.TABLE_NAME = c.TABLE_NAME
                            AND k.COLUMN_NAME = c.COLUMN_NAME AND k.IS_PRIMARY_KEY = 'TRUE')
            THEN 1 ELSE 0 END
  FROM SYS.TABLE_COLUMNS c
 WHERE c.SCHEMA_NAME = ? AND c.TABLE_NAME = ?
 ORDER BY c.POSITION";

const VIEW_COLUMNS: &str = "
SELECT COLUMN_NAME, DATA_TYPE_NAME, LENGTH, SCALE, IS_NULLABLE, DEFAULT_VALUE, NULL, 0
  FROM SYS.VIEW_COLUMNS
 WHERE SCHEMA_NAME = ? AND VIEW_NAME = ?
 ORDER BY POSITION";

#[async_trait]
impl Session for HanaSession {
    async fn server_version(&mut self) -> Result<String> {
        let v = single_text(&self.conn, "SELECT VERSION FROM SYS.M_DATABASE").await?.unwrap_or_default();
        Ok(format!("SAP HANA {v}"))
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        let rows = self.rows(LIST_SCHEMAS, &[]).await?;
        Ok(rows.iter().filter_map(|r| r.first().and_then(text)).collect())
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        let s = self.schema.clone();
        let rows = self.rows(LIST_OBJECTS, &[&s, &s, &s, &s, &s, &s]).await?;
        let mut out: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                Some(DbObject {
                    kind: text(r.first()?)?,
                    schema: None,
                    name: text(r.get(1)?)?,
                    parent: r.get(2).and_then(text),
                })
            })
            .collect();
        out.extend(structure::list_objects(self).await);
        Ok(out)
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        let owner = self.owner(obj);
        let sql = if obj.kind == "view" { VIEW_COLUMNS } else { TABLE_COLUMNS };
        let rows = self.rows(sql, &[&owner, &obj.name]).await?;
        Ok(rows
            .iter()
            .map(|r| {
                let t = |i: usize| r.get(i).and_then(text);
                let n = |i: usize| r.get(i).and_then(int);
                let generation = t(6);
                ColumnInfo {
                    name: t(0).unwrap_or_default(),
                    data_type: format_type(&t(1).unwrap_or_default(), n(2), n(3)),
                    nullable: t(4).as_deref() != Some("FALSE"),
                    primary_key: n(7) == Some(1),
                    auto_increment: generation.as_deref().is_some_and(|g| g.contains("IDENTITY")),
                    default_value: t(5),
                }
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        let owner = self.owner(obj);
        let name = obj.name.as_str();
        let one = |rows: Vec<Vec<HdbValue<'static>>>| rows.into_iter().next().and_then(|r| r.first().and_then(text));
        let qualified = format!("{}.{}", quote(&owner), quote(name));
        // Built from the catalog without the schema, so two schemas compare
        // equal and the DDL runs on either.
        if matches!(obj.kind.as_str(), "sequence" | "synonym" | "type") {
            if let Some(ddl) = structure::definition(self, &obj.kind, &owner, name).await? {
                return Ok(Some(ddl));
            }
        }
        match obj.kind.as_str() {
            "view" => {
                let rows = self
                    .rows("SELECT DEFINITION FROM SYS.VIEWS WHERE SCHEMA_NAME = ? AND VIEW_NAME = ?", &[&owner, name])
                    .await?;
                Ok(one(rows).map(|d| format!("CREATE VIEW {qualified} AS\n{}", d.trim())))
            }
            "procedure" => Ok(one(self
                .rows("SELECT DEFINITION FROM SYS.PROCEDURES WHERE SCHEMA_NAME = ? AND PROCEDURE_NAME = ?", &[
                    &owner, name,
                ])
                .await?)),
            "function" => Ok(one(self
                .rows("SELECT DEFINITION FROM SYS.FUNCTIONS WHERE SCHEMA_NAME = ? AND FUNCTION_NAME = ?", &[
                    &owner, name,
                ])
                .await?)),
            "trigger" => Ok(one(self
                .rows("SELECT DEFINITION FROM SYS.TRIGGERS WHERE SCHEMA_NAME = ? AND TRIGGER_NAME = ?", &[
                    &owner, name,
                ])
                .await?)),
            "table" | "sequence" => {
                // GET_OBJECT_DEFINITION hands out the full CREATE statement.
                let ddl = async {
                    let mut resp = self
                        .conn
                        .prepare_and_execute("CALL SYS.GET_OBJECT_DEFINITION(?, ?)", &(&owner, name))
                        .await?;
                    let rs = resp.get_result_set()?;
                    let md = rs.metadata();
                    let col = md.iter().position(|f| f.columnname() == "OBJECT_CREATION_STATEMENT");
                    let rows = rs.into_rows().await?;
                    let mut ddl = None;
                    for mut row in rows {
                        if let Some(v) = row.nth(col.unwrap_or(0)) {
                            ddl = text(&read_lob(v).await);
                        }
                    }
                    Ok::<_, HdbError>(ddl)
                }
                .await;
                match ddl {
                    Ok(Some(d)) => Ok(Some(d)),
                    _ if obj.kind == "sequence" => {
                        let rows = self
                            .rows(
                                "SELECT START_NUMBER, INCREMENT_BY FROM SYS.SEQUENCES
                                  WHERE SCHEMA_NAME = ? AND SEQUENCE_NAME = ?",
                                &[&owner, name],
                            )
                            .await?;
                        Ok(rows.first().map(|r| {
                            let n = |i: usize| r.get(i).and_then(int).unwrap_or(1);
                            format!("CREATE SEQUENCE {qualified} START WITH {} INCREMENT BY {};", n(0), n(1))
                        }))
                    }
                    // The UI builds a CREATE TABLE from the columns.
                    _ => Ok(None),
                }
            }
            _ => Ok(None),
        }
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, Some(obj.schema().unwrap_or(&self.schema)), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for (stmt, start) in script::pieces(text) {
            let response = self.conn.statement(&stmt).await;
            self.warnings(out).await;
            let response = response.map_err(|e| stmt_err(e, text, start))?;
            let before = out.results.len();
            push_response(response, max_rows, out).await?;
            let reads = out.results[before..].iter().all(|r| !r.columns.is_empty());
            match leading_keyword(&stmt, &ScriptDialect::generic()).as_deref() {
                Some("commit" | "rollback") => self.dirty = false,
                _ if !reads => self.dirty = true,
                _ => {}
            }
        }
        Ok(())
    }

    /// `Open` when, without autocommit, something changed since the last
    /// commit or rollback (HANA keeps a transaction open for reads too).
    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        let open = self.dirty && !self.conn.is_auto_commit().await;
        Ok(Some(if open { TxState::Open } else { TxState::Idle }))
    }

    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        self.conn.set_auto_commit(on).await;
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.conn.commit().await.map_err(err)?;
        self.dirty = false;
        Ok(())
    }

    async fn rollback(&mut self) -> Result<()> {
        self.conn.rollback().await.map_err(err)?;
        self.dirty = false;
        Ok(())
    }

    async fn explain(&mut self, script: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let mut noted = false;
        for (i, stmt) in script::split(script).into_iter().enumerate() {
            if plannable(&stmt) {
                // EXPLAIN PLAN only compiles the statement; its rows are
                // removed right after reading them.
                let name = format!("DBINE_{}_{}", self.id, i);
                self.conn
                    .exec(format!("EXPLAIN PLAN SET STATEMENT_NAME = '{name}' FOR {stmt}"))
                    .await
                    .map_err(err)?;
                let rows = self.rows(EXPLAIN_ROWS, &[&name]).await;
                if let Err(e) = self.conn.exec(format!("DELETE FROM EXPLAIN_PLAN_TABLE WHERE STATEMENT_NAME = '{name}'")).await {
                    tracing::debug!("hana: EXPLAIN_PLAN_TABLE cleanup: {}", message(&e));
                }
                let ops: Vec<plan::Op> = rows?
                    .iter()
                    .map(|r| {
                        let t = |i: usize| r.get(i).and_then(text).unwrap_or_default();
                        plan::Op {
                            id: r.first().and_then(int).unwrap_or_default(),
                            parent: r.get(1).and_then(int),
                            name: t(2),
                            details: t(3),
                            schema: t(4),
                            table: t(5),
                            table_type: t(6),
                            output_size: r.get(7).and_then(num),
                            subtree_cost: r.get(8).and_then(num),
                            engine: t(9),
                        }
                    })
                    .collect();
                out.plans.push(Plan {
                    statement: stmt.clone(),
                    root: plan::tree(&ops),
                    actual: false,
                    raw_format: "text".into(),
                    raw: plan::raw(&ops),
                });
                if analyze && !noted {
                    out.messages.push(
                        "HANA no da cifras reales por operador por SQL (eso es PlanViz): se muestra el plan estimado junto al resultado."
                            .into(),
                    );
                    noted = true;
                }
            } else if !analyze {
                out.messages.push(format!("Sin plan para «{}»: HANA explica consultas y DML.", stmt.trim()));
            }
            if analyze {
                self.execute(&stmt, max_rows, out).await?;
            }
        }
        Ok(())
    }

    fn interrupter(&self) -> Option<Arc<dyn Fn() + Send + Sync>> {
        // Cancel from a second connection (needs SESSION ADMIN for another
        // user's session; one's own may need it too, depending on version).
        let handle = tokio::runtime::Handle::try_current().ok()?;
        let params = self.params.clone();
        let sql = format!("ALTER SYSTEM CANCEL SESSION '{}'", self.id);
        Some(Arc::new(move || {
            let params = params.clone();
            let sql = sql.clone();
            handle.spawn(async move {
                let r = async { Connection::new(params).await?.exec(sql).await }.await;
                if let Err(e) = r {
                    tracing::warn!("hana: no se pudo cancelar la sentencia: {}", message(&e));
                }
            });
        }))
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        schema::database_schema(self).await
    }

    async fn create_database(&mut self, name: &str) -> Result<()> {
        self.conn.exec(format!("CREATE SCHEMA {}", quote(name.trim()))).await.map_err(err)
    }

    async fn drop_database(&mut self, name: &str) -> Result<()> {
        let name = name.trim();
        if name == self.schema {
            return Err(Error::Query("No se puede borrar el esquema de la conexión actual.".into()));
        }
        self.conn.exec(format!("DROP SCHEMA {} CASCADE", quote(name))).await.map_err(err)
    }

    async fn monitor(&mut self) -> Result<MonitorSnapshot> {
        Ok(monitor::snapshot(&self.conn).await)
    }

    async fn blocking(&mut self) -> Result<Vec<dbine_driver::BlockedSession>> {
        blocking::blocking(&self.conn).await
    }

    async fn kill_session(&mut self, id: &str) -> Result<()> {
        blocking::kill(&self.conn, id).await
    }

    async fn principals(&mut self) -> Result<Vec<dbine_driver::Principal>> {
        security::principals(self).await
    }

    async fn grants(&mut self, principal: &str) -> Result<Vec<dbine_driver::Grant>> {
        security::grants(self, principal).await
    }

    /// Of the whole database (the tab opens from the connection): the
    /// schema DBine calls a database doesn't narrow it.
    async fn backups(&mut self, _database: Option<&str>) -> Result<Vec<dbine_driver::BackupEntry>> {
        self.backup_history().await
    }

    async fn read_batches(&mut self, spec: &dbine_driver::ReadSpec, sink: dbine_driver::BatchSinkRef) -> Result<u64> {
        transfer::read_batches(self, spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::LoadSpec,
        _columns: &[dbine_driver::TransferColumn],
        source: &mut dyn dbine_driver::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        transfer::bulk_load(self, spec, source, progress).await
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

    /// One query on EFFECTIVE_PRIVILEGES (see `permissions`).
    async fn permissions(&mut self, database: Option<&str>) -> Result<dbine_driver::Permissions> {
        permissions::check(self, database).await
    }
}

// ------------------------------------------------------------- execution

async fn push_response(response: HdbResponse, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    for rv in response {
        match rv {
            HdbReturnValue::ResultSet(rs) => push_result_set(rs, max_rows, out).await?,
            HdbReturnValue::AffectedRows(v) => out.push_affected(v.iter().map(|n| *n as u64).sum()),
            HdbReturnValue::OutputParameters(op) => {
                let (descriptors, values) = op.into_descriptors_and_values();
                out.begin_result(
                    descriptors
                        .iter()
                        .map(|d| ResultColumn {
                            name: d.name().unwrap_or_default().to_string(),
                            type_name: format!("{:?}", d.type_id()),
                        })
                        .collect(),
                );
                let mut row = Vec::with_capacity(values.len());
                for v in values {
                    row.push(cell(v).await);
                }
                out.push_row(row, max_rows);
            }
            HdbReturnValue::Success => out.results.push(Default::default()),
            #[allow(unreachable_patterns)]
            _ => out.results.push(Default::default()),
        }
    }
    Ok(())
}

async fn push_result_set(mut rs: ResultSet, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
    let md = rs.metadata();
    out.begin_result(
        md.iter()
            .map(|f| ResultColumn { name: f.displayname().to_string(), type_name: format!("{:?}", f.type_id()) })
            .collect(),
    );
    while let Some(row) = rs.next_row().await.map_err(err)? {
        let mut cells = Vec::with_capacity(row.len());
        for v in row {
            cells.push(cell(v).await);
        }
        out.push_row(cells, max_rows);
    }
    Ok(())
}

/// A cell as JSON.
async fn cell(v: HdbValue<'static>) -> Value {
    match v {
        HdbValue::NULL => Value::Null,
        HdbValue::TINYINT(n) => n.into(),
        HdbValue::SMALLINT(n) => n.into(),
        HdbValue::INT(n) => n.into(),
        HdbValue::BIGINT(n) => json_i64(n),
        HdbValue::DECIMAL(d) => d.to_string().into(),
        HdbValue::REAL(f) => json_f64(f64::from(f)),
        HdbValue::DOUBLE(f) => json_f64(f),
        HdbValue::BOOLEAN(b) => Value::Bool(b),
        HdbValue::STRING(s) => s.into(),
        HdbValue::STR(s) => s.into(),
        HdbValue::DBSTRING(b) => String::from_utf8_lossy(&b).into_owned().into(),
        HdbValue::BINARY(b) | HdbValue::GEOMETRY(b) | HdbValue::POINT(b) => json_bytes(&b),
        HdbValue::LONGDATE(d) => iso(&d.to_string()).into(),
        HdbValue::SECONDDATE(d) => iso(&d.to_string()).into(),
        HdbValue::DAYDATE(d) => d.to_string().into(),
        HdbValue::SECONDTIME(t) => t.to_string().into(),
        HdbValue::ASYNC_CLOB(mut c) => {
            let total = c.total_byte_length();
            match c.read_slice(0, TEXT_CAP).await {
                Ok(s) => capped(s.data, total > u64::from(TEXT_CAP)).into(),
                Err(e) => format!("<{}>", message(&e)).into(),
            }
        }
        HdbValue::ASYNC_NCLOB(mut c) => {
            let total = c.total_char_length();
            match c.read_slice(0, TEXT_CAP).await {
                Ok(s) => capped(s.data, total > u64::from(TEXT_CAP)).into(),
                Err(e) => format!("<{}>", message(&e)).into(),
            }
        }
        HdbValue::ASYNC_BLOB(mut b) => match b.read_slice(0, BLOB_CAP).await {
            Ok(bytes) => json_bytes(&bytes),
            Err(e) => format!("<{}>", message(&e)).into(),
        },
        HdbValue::ARRAY(items) => {
            let mut arr = Vec::with_capacity(items.len());
            for i in items {
                arr.push(Box::pin(cell(i)).await);
            }
            Value::Array(arr).to_string().into()
        }
        other => other.to_string().into(),
    }
}

fn capped(mut s: String, more: bool) -> String {
    if more {
        s.push('…');
    }
    s
}

/// `2024-01-31T13:45:00.1230000` → `2024-01-31 13:45:00.123`.
fn iso(s: &str) -> String {
    let s = s.replacen('T', " ", 1);
    match s.split_once('.') {
        Some((head, frac)) => {
            let frac = frac.trim_end_matches('0');
            if frac.is_empty() {
                head.to_string()
            } else {
                format!("{head}.{frac}")
            }
        }
        None => s,
    }
}

/// `NVARCHAR(50)`, `DECIMAL(10,2)`… from SYS.TABLE_COLUMNS.
fn format_type(ty: &str, len: Option<i64>, scale: Option<i64>) -> String {
    match ty {
        "VARCHAR" | "NVARCHAR" | "CHAR" | "NCHAR" | "ALPHANUM" | "SHORTTEXT" | "VARBINARY" | "BINARY" => match len {
            Some(n) if n > 0 => format!("{ty}({n})"),
            _ => ty.to_string(),
        },
        "DECIMAL" => match (len, scale) {
            (Some(p), Some(s)) => format!("DECIMAL({p},{s})"),
            _ => "DECIMAL".to_string(),
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
    fn dates_are_iso() {
        assert_eq!(iso("2024-01-31T13:45:00.1230000"), "2024-01-31 13:45:00.123");
        assert_eq!(iso("2024-01-31T13:45:00.0000000"), "2024-01-31 13:45:00");
        assert_eq!(iso("2024-01-31T13:45:00"), "2024-01-31 13:45:00");
    }

    #[test]
    fn types() {
        assert_eq!(format_type("NVARCHAR", Some(50), None), "NVARCHAR(50)");
        assert_eq!(format_type("DECIMAL", Some(10), Some(2)), "DECIMAL(10,2)");
        assert_eq!(format_type("DECIMAL", Some(34), None), "DECIMAL");
        assert_eq!(format_type("INTEGER", Some(10), Some(0)), "INTEGER");
    }

    #[test]
    fn connect_params() {
        let mut cfg = ConnectionConfig {
            host: "hana.local".into(),
            port: 39041,
            username: Some("SYSTEM".into()),
            password: Some("x".into()),
            ..Default::default()
        };
        let p = params(&cfg).unwrap();
        assert_eq!(p.addr(), "hana.local:39041");
        cfg.username = None;
        assert!(matches!(params(&cfg), Err(Error::AuthFailed(_))));
    }

    #[test]
    fn statement_errors_are_placed_in_the_script() {
        let e = stmt_err(HdbError::Evaluation("x"), "select 1", 0);
        assert!(matches!(e, Error::Query(_)));
        let script = "select 1 from dummy;\nselect ñ from\n dummy x y";
        let start = script.find("select ñ").unwrap();
        let e = server_error(257, "sql syntax error", "42000", 24, false, script, start);
        assert_eq!((e.code.as_deref(), e.sqlstate.as_deref()), (Some("257"), Some("42000")));
        assert_eq!(e.offset, Some(script.rfind('y').unwrap()));
        assert_eq!(e.line, Some(3));
        let e = server_error(129, "transaction rolled back", "HY000", 0, true, script, start);
        assert_eq!((e.sqlstate, e.offset, e.line, e.fatal), (None, Some(start), Some(2), true));
    }

    #[test]
    fn browse_uses_limit() {
        assert_eq!(select_top(Quote::Double, Limit::Limit, Some("S"), "T", 5), "SELECT *\nFROM \"S\".\"T\"\nLIMIT 5");
    }

    #[tokio::test]
    async fn cells() {
        assert_eq!(cell(HdbValue::BIGINT(i64::MAX)).await, serde_json::json!("9223372036854775807"));
        assert_eq!(cell(HdbValue::BINARY(vec![0xDE, 0xAD])).await, serde_json::json!("0xDEAD"));
        assert_eq!(cell(HdbValue::NULL).await, Value::Null);
    }

    #[test]
    fn info_is_complete() {
        let i = info();
        assert_eq!(i.id, "hana");
        assert_eq!(i.databases_label, "Esquemas");
        assert!(!i.has_schemas);
    }
}
