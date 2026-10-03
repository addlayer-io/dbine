//! Apache Phoenix through the Phoenix Query Server (Avatica over HTTP, port
//! 8765). The server speaks protobuf by default
//! (`phoenix.queryserver.serialization`); JSON is an option in the form.
//!
//! One Avatica connection per session: `openConnection`, then per statement
//! `prepareAndExecute` and `fetch` until the frame says done. The catalog
//! comes from `SYSTEM.CATALOG` and `SYSTEM."SEQUENCE"`.
//!
//! The same client also serves "Apache Calcite Avatica", any Avatica
//! server (see [`avatica`]).

mod avatica;
mod ddl;
mod index_usage;
mod monitor;
mod plan;
mod proto;
mod sync;
mod transfer;

use base64::Engine as _;
use dbine_driver::sql::{
    leading_keyword, select_top, split_script, Limit, Quote, ScriptDefaults, ScriptDialect, ScriptMode, StatementKind,
};
use dbine_driver::{
    json_bytes, json_f64, json_i64, kinds, Capabilities, ColumnInfo, ConnectionConfig, CreateTemplate, DbObject, DdlParts, DesignerSpec,
    Driver, DriverInfo, Error, Family, Field, FieldKind, Language, ObjectKindInfo, ObjectRef, QueryOutcome, ResultColumn,
    Result, RowChange, ScriptError, Session, TableSchema, TxState,
};
use async_trait::async_trait;
use prost::Message;
use serde_json::{json, Value};
use std::sync::Arc;
use std::time::Duration;

/// Rows per frame.
const FRAME: i32 = 500;

pub fn drivers() -> Vec<Arc<dyn Driver>> {
    vec![Arc::new(PhoenixDriver { info: info(), generic: false }), Arc::new(PhoenixDriver { info: avatica::info(), generic: true })]
}

fn info() -> DriverInfo {
    DriverInfo {
        id: "phoenix",
        name: "Apache Phoenix",
        family: Family::WideColumn,
        language: Language::Sql,
        dialect: "phoenix",
        default_port: 8765,
        fields: vec![
            Field::host(),
            Field::port().placeholder("8765"),
            Field::username(),
            Field::password(),
            Field::encrypt(),
            Field::trust_cert(),
            Field::new(
                "serialization",
                "Serialización",
                FieldKind::Select(vec![("protobuf", "Protobuf (predeterminada)"), ("json", "JSON")]),
            )
            .default_value("protobuf")
            .help("La del Query Server (phoenix.queryserver.serialization).")
            .advanced(),
            Field::new("hbase_master", "Master de HBase", FieldKind::Text)
                .placeholder("http://<host>:16010")
                .help("Opcional, para el monitor: la interfaz web del Master de HBase (su /jmx). Vacío = el mismo host, puerto 16010.")
                .advanced(),
            Field::new("hbase_regionservers", "RegionServers de HBase", FieldKind::Text)
                .placeholder("http://rs1:16030, http://rs2:16030")
                .help("Opcional, para el monitor: si los nombres que informa el Master no resuelven desde esta máquina.")
                .advanced(),
            Field::read_only(),
        ],
        databases_label: "",
        has_schemas: true,
        object_kinds: vec![ObjectKindInfo::tables(), ObjectKindInfo::views(), ObjectKindInfo::sequences()],
    }
}

pub struct PhoenixDriver {
    info: DriverInfo,
    /// A generic Avatica server rather than Phoenix.
    generic: bool,
}

#[async_trait]
impl Driver for PhoenixDriver {
    fn info(&self) -> &DriverInfo {
        &self.info
    }

    fn supports_explain(&self) -> bool {
        true
    }

    fn script_dialect(&self) -> ScriptDialect {
        dialect()
    }

    /// One `prepareAndExecute` per statement on the session's server
    /// connection, which keeps its state.
    fn script_mode(&self) -> ScriptMode {
        ScriptMode::PerStatement
    }

    /// sqlline / psql.py stop at the first error.
    fn script_defaults(&self) -> ScriptDefaults {
        ScriptDefaults { continue_on_error: false, confirm_unsafe_dml: true }
    }

    /// Autocommit off keeps UPSERTs and DELETEs on the server connection
    /// until COMMIT (atomic only on TRANSACTIONAL tables).
    fn supports_manual_transactions(&self) -> bool {
        true
    }

    /// A prepared `UPSERT` / `INSERT` with `executeBatch` (see `transfer.rs`).
    fn supports_bulk_load(&self) -> bool {
        true
    }

    /// The connection is a single namespace ("default"); schemas live below
    /// it and are created by script (CREATE SCHEMA), and Phoenix has no
    /// foreign keys. The monitor reads HBase's JMX.
    /// A generic Avatica server has nothing to monitor, and its DDL is the
    /// backend's.
    fn capabilities(&self) -> Capabilities {
        Capabilities { monitor: !self.generic, ..Capabilities::default() }
    }

    fn designer(&self) -> Option<DesignerSpec> {
        (!self.generic).then(ddl::designer)
    }

    fn create_templates(&self) -> Vec<CreateTemplate> {
        if self.generic {
            Vec::new()
        } else {
            ddl::templates()
        }
    }

    fn supports_schema_sync(&self) -> bool {
        !self.generic
    }

    /// The row key and the secondary indexes, without counters (see
    /// [`index_usage`]); Avatica's protocol has no index metadata.
    fn supports_index_usage(&self) -> bool {
        !self.generic
    }

    fn sync_script(&self, changes: &[dbine_driver::TableChange]) -> Result<dbine_driver::SyncScript> {
        if self.generic {
            return Err(Error::Unsupported("el DDL depende de la base detrás del servidor Avatica".into()));
        }
        sync::sync_script(changes)
    }

    fn table_ddl(&self, table: &TableSchema, parts: DdlParts) -> Result<String> {
        if self.generic {
            return Err(Error::Unsupported("el DDL depende de la base detrás del servidor Avatica".into()));
        }
        ddl::table_ddl(table, parts)
    }

    fn insert_script(&self, target: &ObjectRef, columns: &[String], rows: &[Vec<Value>]) -> Result<String> {
        if self.generic {
            let flavor = dbine_driver::ddl::SqlFlavor::ansi();
            return Ok(dbine_driver::ddl::insert_script(&flavor, target.schema(), &target.name, columns, rows, 100));
        }
        Ok(ddl::insert_script(target.schema(), &target.name, columns, rows))
    }

    fn update_script(&self, target: &ObjectRef, changes: &[RowChange]) -> Result<String> {
        if self.generic {
            let flavor = dbine_driver::ddl::SqlFlavor::ansi();
            return Ok(dbine_driver::ddl::update_script(&flavor, target.schema(), &target.name, changes));
        }
        ddl::update_script(target.schema(), &target.name, changes)
    }

    fn filtered_browse(&self, browse: &str, filters: &[dbine_driver::ColumnFilter]) -> Result<String> {
        ddl::filtered_browse(browse, filters, !self.generic)
    }

    /// Phoenix schemas (namespace mapping on): no owner, dropped only when
    /// empty, and HBase-ACL permissions (R, W, X, C, A) granted on them. A
    /// generic Avatica server's DDL is the backend's.
    fn schema_spec(&self) -> Option<dbine_driver::SchemaSpec> {
        (!self.generic).then(|| dbine_driver::SchemaSpec { owner: false, owner_kinds: dbine_driver::SchemaOwnerKinds::Both, cascade: false, privileges: ddl::SCHEMA_PERMISSIONS.to_vec(), grant_option: true })
    }

    fn create_schema_script(&self, _database: Option<&str>, name: &str, owner: Option<&str>) -> Result<String> {
        self.phoenix_only()?;
        ddl::create_schema(name, owner)
    }

    fn drop_schema_script(&self, _database: Option<&str>, name: &str, cascade: bool) -> Result<String> {
        self.phoenix_only()?;
        ddl::drop_schema(name, cascade)
    }

    fn security_script(&self, action: &dbine_driver::SecurityAction) -> Result<String> {
        self.phoenix_only()?;
        ddl::schema_security(action)
    }

    async fn connect(&self, cfg: &ConnectionConfig, _database: Option<&str>) -> Result<Box<dyn Session>> {
        let scheme = if cfg.encrypt { "https" } else { "http" };
        let host = if cfg.host.trim().is_empty() { "localhost" } else { cfg.host.trim() };
        let http = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .danger_accept_invalid_certs(cfg.trust_server_certificate)
            .build()
            .map_err(Error::connect)?;
        let client = Client {
            http,
            url: format!(
                "{scheme}://{host}:{}{}",
                cfg.port_or(8765),
                if self.generic { avatica::url_path(cfg.option("path")) } else { "/".into() }
            ),
            user: cfg.username.clone().filter(|u| !u.is_empty()),
            password: cfg.password.clone(),
            json: match cfg.option("serialization") {
                Some(s) => s == "json",
                None => self.generic,
            },
            connection_id: uuid::Uuid::new_v4().to_string(),
        };
        let open = async {
            client.call(Req::Open).await?;
            client.call(Req::Sync { read_only: cfg.read_only }).await?;
            match client.call(Req::CreateStatement).await? {
                Resp::Created(id) => Ok(id),
                _ => Err(Error::Query("respuesta inesperada a createStatement".into())),
            }
        };
        let statement = tokio::time::timeout(Duration::from_secs(20), open)
            .await
            .map_err(|_| Error::Connect("tiempo de espera agotado".into()))??;
        let hbase = monitor::HBaseUis::from_config(host, cfg.option("hbase_master"), cfg.option("hbase_regionservers"));
        Ok(Box::new(PhoenixSession { client, statement, hbase, generic: self.generic, manual: false, dirty: false }))
    }
}

impl PhoenixDriver {
    fn phoenix_only(&self) -> Result<()> {
        if self.generic {
            return Err(Error::Unsupported("el DDL depende de la base detrás del servidor Avatica".into()));
        }
        Ok(())
    }
}

struct Client {
    http: reqwest::Client,
    url: String,
    user: Option<String>,
    password: Option<String>,
    json: bool,
    connection_id: String,
}

/// The requests the driver sends.
enum Req {
    Open,
    Close,
    Sync { read_only: bool },
    CreateStatement,
    CloseStatement(u32),
    Execute { statement: u32, sql: String },
    Fetch { statement: u32, offset: u64 },
    DatabaseProperties,
    /// Metadata: every table (Avatica `getTables`).
    Tables,
    /// Metadata: columns (Avatica `getColumns`); `None` = any.
    Columns { schema: Option<String>, table: Option<String> },
}

/// Responses, the same whatever the serialization.
enum Resp {
    Done,
    Created(u32),
    Execute(Vec<ResultSet>),
    Fetch(Option<FrameData>),
    Properties(Vec<(String, Value)>),
}

struct ResultSet {
    statement: u32,
    columns: Vec<Col>,
    frame: Option<FrameData>,
    update_count: Option<u64>,
}

struct Col {
    name: String,
    type_name: String,
    /// Avatica `Rep` name ("STRING", "JAVA_SQL_TIMESTAMP"…).
    rep: String,
}

struct FrameData {
    done: bool,
    /// Cells already converted for the UI.
    rows: Vec<Vec<Value>>,
}

fn http_error(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

fn server_error(message: String, code: u32, state: &str) -> Error {
    let msg = if message.is_empty() { format!("error {code} ({state})") } else { message };
    // The Query Server wraps Phoenix's "ERROR 601 (42P00): …" in Java
    // exception names and leaves its own code at -1.
    let phoenix = msg.find("ERROR ").and_then(|i| {
        let rest = &msg[i..];
        let (head, _) = rest.split_once("): ")?;
        let (n, st) = head.strip_prefix("ERROR ")?.split_once(" (")?;
        let n: u32 = n.parse().ok()?;
        let text = rest.split(" -> ").next().unwrap_or(rest).trim().to_string();
        Some((n, st.to_string(), text))
    });
    let (code, state, msg) = match phoenix {
        Some((n, st, text)) => (n, st, text),
        None => (code, state.trim().to_string(), msg),
    };
    let mut e = ScriptError::new(msg);
    if code != 0 && code != u32::MAX {
        e = e.with_code(code.to_string());
    }
    if !state.is_empty() && state != "00000" {
        e = e.with_sqlstate(state);
    }
    e.into()
}

/// Phoenix's SQL: `;` outside quotes and comments, no procedural bodies.
fn dialect() -> ScriptDialect {
    ScriptDialect { backtick_idents: false, compound_blocks: false, ..ScriptDialect::generic() }
}

/// The statements of a script with their byte offsets.
fn statements(sql: &str) -> Vec<(String, usize)> {
    split_script(sql, &dialect()).into_iter().filter(|s| s.kind != StatementKind::ClientCommand).map(|s| (s.text, s.start)).collect()
}

/// A failed statement of `script` (at byte `start`), placed where Phoenix
/// says ("… at line 2, column 7.").
fn place(e: Error, script: &str, start: usize) -> Error {
    let Error::Statement(mut se) = e else { return e };
    let start = start.min(script.len());
    let pos = se.message.rsplit_once("at line ").and_then(|(_, r)| {
        let (l, rest) = r.split_once(", column ")?;
        let c: String = rest.chars().take_while(char::is_ascii_digit).collect();
        Some((l.trim().parse::<usize>().ok()?, c.parse::<usize>().ok()?))
    });
    let stmt = &script[start..];
    let offset = match pos.filter(|(l, c)| *l >= 1 && *c >= 1) {
        Some((line, col)) => {
            let line_start: usize = stmt.split_inclusive('\n').take(line - 1).map(str::len).sum();
            let rest = &stmt[line_start.min(stmt.len())..];
            start + line_start.min(stmt.len()) + rest.char_indices().nth(col - 1).map_or(rest.len(), |(b, _)| b)
        }
        None => start,
    };
    se.offset = Some(offset);
    se.line = Some(script[..offset].matches('\n').count() as u32 + 1);
    Error::Statement(se)
}

impl Client {
    async fn call(&self, req: Req) -> Result<Resp> {
        let rb = self.http.post(&self.url);
        let rb = match &self.user {
            Some(u) => rb.basic_auth(u, self.password.as_deref()),
            None => rb,
        };
        let rb = if self.json {
            rb.header("Content-Type", "application/json").body(self.json_request(&req).to_string())
        } else {
            rb.header("Content-Type", "application/octet-stream").body(self.proto_request(&req))
        };
        let resp = rb.send().await.map_err(http_error)?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(Error::AuthFailed(format!("HTTP {status}")));
        }
        let body = resp.bytes().await.map_err(http_error)?;
        if self.json {
            let v: Value = serde_json::from_slice(&body)
                .map_err(|_| Error::Query(format!("HTTP {status}: {}", String::from_utf8_lossy(&body).trim())))?;
            parse_json(&req, v)
        } else {
            parse_proto(&req, &body).map_err(|e| match e {
                Error::Serde(_) => Error::Query(format!("HTTP {status}: respuesta que no es protobuf; ¿el servidor usa JSON?")),
                e => e,
            })
        }
    }

    fn json_request(&self, req: &Req) -> Value {
        let c = &self.connection_id;
        match req {
            Req::Open => json!({"request": "openConnection", "connectionId": c, "info": {}}),
            Req::Close => json!({"request": "closeConnection", "connectionId": c}),
            Req::Sync { read_only } => json!({"request": "connectionSync", "connectionId": c, "connProps": {
                "connProps": "connPropsImpl", "autoCommit": true, "readOnly": read_only, "dirty": true}}),
            Req::CreateStatement => json!({"request": "createStatement", "connectionId": c}),
            Req::CloseStatement(s) => json!({"request": "closeStatement", "connectionId": c, "statementId": s}),
            Req::Execute { statement, sql } => json!({"request": "prepareAndExecute", "connectionId": c,
                "statementId": statement, "sql": sql, "maxRowCount": -1, "maxRowsTotal": -1, "maxRowsInFirstFrame": FRAME}),
            Req::Fetch { statement, offset } => json!({"request": "fetch", "connectionId": c, "statementId": statement,
                "offset": offset, "fetchMaxRowCount": FRAME, "frameMaxSize": FRAME}),
            Req::DatabaseProperties => json!({"request": "databaseProperties", "connectionId": c}),
            Req::Tables => json!({"request": "getTables", "connectionId": c, "catalog": null, "schemaPattern": null,
                "tableNamePattern": null, "typeList": null}),
            Req::Columns { schema, table } => json!({"request": "getColumns", "connectionId": c, "catalog": null,
                "schemaPattern": schema, "tableNamePattern": table, "columnNamePattern": null}),
        }
    }

    fn proto_request(&self, req: &Req) -> Vec<u8> {
        let c = self.connection_id.clone();
        let id = || proto::ConnectionIdRequest { connection_id: c.clone() }.encode_to_vec();
        let (name, bytes) = match req {
            Req::Open => ("OpenConnectionRequest", proto::OpenConnectionRequest { connection_id: c.clone(), info: Default::default() }.encode_to_vec()),
            Req::Close => ("CloseConnectionRequest", id()),
            Req::Sync { read_only } => (
                "ConnectionSyncRequest",
                proto::ConnectionSyncRequest {
                    connection_id: c.clone(),
                    conn_props: Some(proto::ConnectionProperties {
                        is_dirty: true,
                        auto_commit: true,
                        has_auto_commit: true,
                        read_only: *read_only,
                        has_read_only: true,
                    }),
                }
                .encode_to_vec(),
            ),
            Req::CreateStatement => ("CreateStatementRequest", id()),
            Req::CloseStatement(s) => {
                ("CloseStatementRequest", proto::CloseStatementRequest { connection_id: c.clone(), statement_id: *s }.encode_to_vec())
            }
            Req::Execute { statement, sql } => (
                "PrepareAndExecuteRequest",
                proto::PrepareAndExecuteRequest {
                    connection_id: c.clone(),
                    sql: sql.clone(),
                    max_row_count: u64::MAX,
                    statement_id: *statement,
                    max_rows_total: -1,
                    first_frame_max_size: FRAME,
                }
                .encode_to_vec(),
            ),
            Req::Fetch { statement, offset } => (
                "FetchRequest",
                proto::FetchRequest {
                    connection_id: c.clone(),
                    statement_id: *statement,
                    offset: *offset,
                    fetch_max_row_count: FRAME as u32,
                    frame_max_size: FRAME,
                }
                .encode_to_vec(),
            ),
            Req::DatabaseProperties => ("DatabasePropertyRequest", id()),
            Req::Tables => ("TablesRequest", proto::TablesRequest { connection_id: c.clone(), ..Default::default() }.encode_to_vec()),
            Req::Columns { schema, table } => (
                "ColumnsRequest",
                proto::ColumnsRequest {
                    connection_id: c.clone(),
                    schema_pattern: schema.clone().unwrap_or_default(),
                    table_name_pattern: table.clone().unwrap_or_default(),
                    ..Default::default()
                }
                .encode_to_vec(),
            ),
        };
        proto::WireMessage { name: format!("{}{name}", proto::REQ), wrapped_message: bytes }.encode_to_vec()
    }
}

fn decode_err(e: prost::DecodeError) -> Error {
    Error::Serde(serde_json::Error::io(std::io::Error::new(std::io::ErrorKind::InvalidData, e)))
}

fn parse_proto(req: &Req, body: &[u8]) -> Result<Resp> {
    let wire = proto::WireMessage::decode(body).map_err(decode_err)?;
    let name = wire.name.strip_prefix(proto::RESP).unwrap_or(&wire.name);
    let msg = wire.wrapped_message.as_slice();
    if name == "ErrorResponse" {
        let e = proto::ErrorResponse::decode(msg).map_err(decode_err)?;
        return Err(server_error(e.error_message, e.error_code, &e.sql_state));
    }
    Ok(match req {
        Req::CreateStatement => Resp::Created(proto::CreateStatementResponse::decode(msg).map_err(decode_err)?.statement_id),
        Req::Execute { .. } => {
            let r = proto::ExecuteResponse::decode(msg).map_err(decode_err)?;
            if r.missing_statement {
                return Err(Error::Query("el servidor perdió la sentencia; reconectá".into()));
            }
            Resp::Execute(r.results.into_iter().map(proto_result_set).collect())
        }
        Req::Fetch { .. } => {
            let r = proto::FetchResponse::decode(msg).map_err(decode_err)?;
            if r.missing_statement || r.missing_results {
                return Err(Error::Query("el servidor perdió el resultado; reconectá".into()));
            }
            Resp::Fetch(r.frame.map(proto_frame))
        }
        Req::Tables | Req::Columns { .. } => {
            Resp::Execute(vec![proto_result_set(proto::ResultSetResponse::decode(msg).map_err(decode_err)?)])
        }
        Req::DatabaseProperties => {
            let r = proto::DatabasePropertyResponse::decode(msg).map_err(decode_err)?;
            Resp::Properties(
                r.props
                    .into_iter()
                    .filter_map(|p| Some((p.key?.name, p.value.map(|v| typed(&v)).unwrap_or(Value::Null))))
                    .collect(),
            )
        }
        _ => Resp::Done,
    })
}

fn proto_result_set(rs: proto::ResultSetResponse) -> ResultSet {
    let columns: Vec<Col> = rs
        .signature
        .map(|s| s.columns)
        .unwrap_or_default()
        .into_iter()
        .map(|c| {
            let t = c.r#type.unwrap_or_default();
            Col {
                name: if c.label.is_empty() { c.column_name } else { c.label },
                rep: temporal_rep(t.rep, &t.name).to_string(),
                type_name: t.name,
            }
        })
        .collect();
    ResultSet {
        statement: rs.statement_id,
        frame: rs.first_frame.map(proto_frame),
        update_count: (columns.is_empty()).then_some(if rs.update_count == u64::MAX { 0 } else { rs.update_count }),
        columns,
    }
}

fn proto_frame(f: proto::Frame) -> FrameData {
    FrameData {
        done: f.done,
        rows: f
            .rows
            .into_iter()
            .map(|r| {
                r.value
                    .into_iter()
                    .map(|cv| {
                        if cv.has_array_value {
                            Value::String(Value::Array(cv.array_value.iter().map(typed).collect()).to_string())
                        } else if let Some(v) = cv.scalar_value.as_ref().or(cv.value.first()) {
                            let v = typed(v);
                            if v.is_array() {
                                Value::String(v.to_string())
                            } else {
                                v
                            }
                        } else {
                            Value::Null
                        }
                    })
                    .collect()
            })
            .collect(),
    }
}

/// A protobuf `TypedValue` as a cell.
fn typed(v: &proto::TypedValue) -> Value {
    use proto::rep;
    if v.null || v.r#type == rep::NULL {
        return Value::Null;
    }
    match v.r#type {
        rep::BOOLEAN | rep::PRIMITIVE_BOOLEAN => v.bool_value.into(),
        rep::FLOAT | rep::DOUBLE | rep::PRIMITIVE_FLOAT | rep::PRIMITIVE_DOUBLE => json_f64(v.double_value),
        rep::STRING | rep::BIG_DECIMAL | rep::BIG_INTEGER => v.string_value.clone().into(),
        rep::BYTE_STRING => json_bytes(&v.bytes_value),
        rep::JAVA_SQL_DATE => date_from_days(v.number_value).into(),
        rep::JAVA_SQL_TIME => time_from_millis(v.number_value).into(),
        rep::JAVA_SQL_TIMESTAMP | rep::JAVA_UTIL_DATE => timestamp_from_millis(v.number_value).into(),
        rep::ARRAY => Value::Array(v.array_value.iter().map(typed).collect()),
        _ => json_i64(v.number_value),
    }
}

fn parse_json(req: &Req, v: Value) -> Result<Resp> {
    if v.get("response").and_then(Value::as_str) == Some("error") {
        return Err(server_error(
            v.get("errorMessage").and_then(Value::as_str).unwrap_or_default().to_string(),
            v.get("errorCode").and_then(Value::as_u64).unwrap_or(0) as u32,
            v.get("sqlState").and_then(Value::as_str).unwrap_or_default(),
        ));
    }
    Ok(match req {
        Req::CreateStatement => Resp::Created(v.get("statementId").and_then(Value::as_u64).unwrap_or(0) as u32),
        Req::Execute { .. } => {
            if v.get("missingStatement").and_then(Value::as_bool) == Some(true) {
                return Err(Error::Query("el servidor perdió la sentencia; reconectá".into()));
            }
            let results = v.get("results").and_then(Value::as_array).cloned().unwrap_or_default();
            Resp::Execute(results.iter().map(json_result_set).collect())
        }
        Req::Tables | Req::Columns { .. } => Resp::Execute(vec![json_result_set(&v)]),
        Req::Fetch { .. } => Resp::Fetch(v.get("frame").filter(|f| !f.is_null()).map(|f| json_frame(f, &[]))),
        Req::DatabaseProperties => {
            let map = v.get("map").and_then(Value::as_object).cloned().unwrap_or_default();
            Resp::Properties(map.into_iter().map(|(k, v)| (k, v.get("value").cloned().unwrap_or(v))).collect())
        }
        _ => Resp::Done,
    })
}

fn json_result_set(rs: &Value) -> ResultSet {
    let columns: Vec<Col> = rs
        .pointer("/signature/columns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| Col {
            name: c.get("label").or(c.get("columnName")).and_then(Value::as_str).unwrap_or_default().to_string(),
            type_name: c.pointer("/type/name").and_then(Value::as_str).unwrap_or_default().to_string(),
            rep: c.pointer("/type/rep").and_then(Value::as_str).unwrap_or_default().to_string(),
        })
        .collect();
    let reps: Vec<String> = columns.iter().map(|c| c.rep.clone()).collect();
    ResultSet {
        statement: rs.get("statementId").and_then(Value::as_u64).unwrap_or(0) as u32,
        frame: rs.get("firstFrame").filter(|f| !f.is_null()).map(|f| json_frame(f, &reps)),
        update_count: columns.is_empty().then(|| rs.get("updateCount").and_then(Value::as_i64).unwrap_or(0).max(0) as u64),
        columns,
    }
}

fn json_frame(f: &Value, reps: &[String]) -> FrameData {
    FrameData {
        done: f.get("done").and_then(Value::as_bool).unwrap_or(true),
        rows: f
            .get("rows")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
            .map(|r| {
                r.as_array()
                    .into_iter()
                    .flatten()
                    .enumerate()
                    .map(|(i, c)| json_cell(c.clone(), reps.get(i).map_or("", String::as_str)))
                    .collect()
            })
            .collect(),
    }
}

/// A JSON-serialized cell: temporal values come as numbers.
fn json_cell(v: Value, rep: &str) -> Value {
    match (v, rep) {
        (Value::Number(n), "JAVA_SQL_DATE") => n.as_i64().map_or(Value::Number(n.clone()), |d| date_from_days(d).into()),
        (Value::Number(n), "JAVA_SQL_TIME") => n.as_i64().map_or(Value::Number(n.clone()), |d| time_from_millis(d).into()),
        (Value::Number(n), "JAVA_SQL_TIMESTAMP" | "JAVA_UTIL_DATE") => {
            n.as_i64().map_or(Value::Number(n.clone()), |d| timestamp_from_millis(d).into())
        }
        (Value::Number(n), "BIG_DECIMAL") => Value::String(n.to_string()),
        (Value::String(s), "BYTE_STRING") => match base64::engine::general_purpose::STANDARD.decode(&s) {
            Ok(b) => json_bytes(&b),
            Err(_) => Value::String(s),
        },
        (Value::Number(n), _) => n.as_i64().map_or(Value::Number(n), json_i64),
        (v @ (Value::Array(_) | Value::Object(_)), _) => Value::String(v.to_string()),
        (v, _) => v,
    }
}

/// Temporal values arrive as plain numbers; the column type says which.
fn temporal_rep(rep: i32, type_name: &str) -> &'static str {
    use proto::rep;
    match (rep, type_name.trim_start_matches("UNSIGNED_")) {
        (rep::JAVA_SQL_TIMESTAMP, _) | (rep::JAVA_UTIL_DATE, _) | (_, "TIMESTAMP") => "JAVA_SQL_TIMESTAMP",
        (rep::JAVA_SQL_DATE, _) | (_, "DATE") => "JAVA_SQL_DATE",
        (rep::JAVA_SQL_TIME, _) | (_, "TIME") => "JAVA_SQL_TIME",
        _ => "",
    }
}

fn date_from_days(days: i64) -> String {
    chrono::DateTime::from_timestamp(days * 86_400, 0).map_or_else(|| days.to_string(), |t| t.format("%Y-%m-%d").to_string())
}

fn time_from_millis(ms: i64) -> String {
    let s = ms.rem_euclid(86_400_000) / 1000;
    format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60)
}

fn timestamp_from_millis(ms: i64) -> String {
    match chrono::DateTime::from_timestamp_millis(ms) {
        Some(t) if ms % 1000 == 0 => t.format("%Y-%m-%d %H:%M:%S").to_string(),
        Some(t) => t.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        None => ms.to_string(),
    }
}

pub struct PhoenixSession {
    client: Client,
    /// The statement handle every query runs on.
    statement: u32,
    /// HBase's web UIs, for the monitor.
    hbase: monitor::HBaseUis,
    /// A generic Avatica server: catalog from Avatica's metadata calls.
    generic: bool,
    /// Autocommit off (manual transactions).
    manual: bool,
    /// In manual mode: something changed since the last commit or rollback.
    dirty: bool,
}

impl Drop for PhoenixSession {
    fn drop(&mut self) {
        // Free the server side connection (best effort, if a runtime is around).
        if let Ok(rt) = tokio::runtime::Handle::try_current() {
            let client = Client {
                http: self.client.http.clone(),
                url: self.client.url.clone(),
                user: self.client.user.clone(),
                password: self.client.password.clone(),
                json: self.client.json,
                connection_id: self.client.connection_id.clone(),
            };
            let statement = self.statement;
            rt.spawn(async move {
                let _ = client.call(Req::CloseStatement(statement)).await;
                let _ = client.call(Req::Close).await;
            });
        }
    }
}

/// A string literal: quotes doubled.
fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// `TABLE_SCHEM = 'x'`, or `IS NULL` for the default schema.
fn schema_cond(col: &str, schema: Option<&str>) -> String {
    match schema {
        Some(s) => format!("{col} = {}", lit(s)),
        None => format!("{col} IS NULL"),
    }
}

fn text(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

impl PhoenixSession {
    /// Run one statement to the end, appending its result to `out`.
    async fn run(&mut self, sql: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        self.run_req(Req::Execute { statement: self.statement, sql: sql.to_string() }, max_rows, out).await
    }

    /// A request that answers result sets (a statement, or a metadata
    /// call), read to the end into `out`.
    async fn run_req(&mut self, req: Req, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        let resp = self.client.call(req).await?;
        let Resp::Execute(results) = resp else { return Err(Error::Query("respuesta inesperada".into())) };
        for rs in results {
            if let Some(n) = rs.update_count {
                out.push_affected(n);
                continue;
            }
            out.begin_result(rs.columns.iter().map(|c| ResultColumn { name: c.name.clone(), type_name: c.type_name.clone() }).collect());
            let reps: Vec<String> = rs.columns.iter().map(|c| c.rep.clone()).collect();
            let mut frame = rs.frame;
            let mut offset = 0u64;
            while let Some(f) = frame.take() {
                offset += f.rows.len() as u64;
                for row in f.rows {
                    // Frames don't carry the column types: apply them here.
                    let row = row
                        .into_iter()
                        .enumerate()
                        .map(|(i, c)| match reps.get(i).map(String::as_str) {
                            Some(r) if self.client.json || r.starts_with("JAVA_") => json_cell(c, r),
                            _ => c,
                        })
                        .collect();
                    out.push_row(row, max_rows);
                }
                if f.done {
                    break;
                }
                match self.client.call(Req::Fetch { statement: rs.statement, offset }).await? {
                    Resp::Fetch(next) => frame = next,
                    _ => return Err(Error::Query("respuesta inesperada a fetch".into())),
                }
            }
        }
        Ok(())
    }

    async fn rows(&mut self, sql: &str) -> Result<Vec<Vec<Value>>> {
        let mut out = QueryOutcome::default();
        self.run(sql, 1_000_000, &mut out).await?;
        Ok(out.results.pop().map(|r| r.rows).unwrap_or_default())
    }
}

/// Why there's no process list (for the error and the docs).
const UNSUPPORTED_PROCESSES: &str = "Phoenix no informa las consultas en curso: ni Phoenix ni HBase llevan una lista que se pueda leer \
     o detener, y el Query Server (Avatica) solo conoce sus propias conexiones";

#[async_trait]
impl Session for PhoenixSession {
    async fn server_version(&mut self) -> Result<String> {
        if self.generic {
            return self.avatica_version().await;
        }
        let v = match self.client.call(Req::DatabaseProperties).await? {
            Resp::Properties(p) => {
                p.into_iter().find(|(k, _)| k.contains("PRODUCT_VERSION")).map(|(_, v)| text(&v)).unwrap_or_default()
            }
            _ => String::new(),
        };
        Ok(format!("Apache Phoenix {v}").trim().to_string())
    }

    async fn list_databases(&mut self) -> Result<Vec<String>> {
        Ok(vec!["default".into()])
    }

    async fn list_objects(&mut self) -> Result<Vec<DbObject>> {
        if self.generic {
            return self.avatica_objects().await;
        }
        let rows = self
            .rows(
                "SELECT DISTINCT TABLE_SCHEM, TABLE_NAME, TABLE_TYPE FROM SYSTEM.CATALOG
                 WHERE TENANT_ID IS NULL AND COLUMN_NAME IS NULL AND COLUMN_FAMILY IS NULL
                   AND TABLE_TYPE IN ('u', 'v')
                 ORDER BY TABLE_SCHEM, TABLE_NAME",
            )
            .await?;
        let mut out: Vec<DbObject> = rows
            .into_iter()
            .map(|r| DbObject {
                kind: if text(&r[2]) == "v" { kinds::VIEW } else { kinds::TABLE }.into(),
                schema: Some(text(&r[0])).filter(|s| !s.is_empty()),
                name: text(&r[1]),
                parent: None,
            })
            .collect();
        let seqs = self
            .rows("SELECT SEQUENCE_SCHEMA, SEQUENCE_NAME FROM SYSTEM.\"SEQUENCE\" WHERE TENANT_ID IS NULL ORDER BY 1, 2")
            .await?;
        out.extend(seqs.into_iter().map(|r| DbObject {
            kind: kinds::SEQUENCE.into(),
            schema: Some(text(&r[0])).filter(|s| !s.is_empty()),
            name: text(&r[1]),
            parent: None,
        }));
        Ok(out)
    }

    /// Every schema: those made with `CREATE SCHEMA` (a `SYSTEM.CATALOG` row
    /// of their own, so an empty one shows and can be dropped) and those
    /// only named by tables or sequences. A generic Avatica server: `None`.
    async fn list_schemas(&mut self) -> Result<Option<Vec<dbine_driver::SchemaInfo>>> {
        if self.generic {
            return Ok(None);
        }
        let mut names: Vec<String> = Vec::new();
        for sql in [
            "SELECT DISTINCT TABLE_SCHEM FROM SYSTEM.CATALOG WHERE TENANT_ID IS NULL AND TABLE_SCHEM IS NOT NULL",
            "SELECT DISTINCT SEQUENCE_SCHEMA FROM SYSTEM.\"SEQUENCE\" WHERE TENANT_ID IS NULL AND SEQUENCE_SCHEMA IS NOT NULL",
        ] {
            for r in self.rows(sql).await? {
                let n = r.first().map(text).unwrap_or_default();
                if !n.is_empty() && !names.contains(&n) {
                    names.push(n);
                }
            }
        }
        names.sort();
        Ok(Some(names.into_iter().map(|name| dbine_driver::SchemaInfo { system: name == "SYSTEM", name }).collect()))
    }

    async fn columns(&mut self, obj: &ObjectRef) -> Result<Vec<ColumnInfo>> {
        if self.generic {
            return self.avatica_columns(obj).await;
        }
        let rows = self
            .rows(&format!(
                "SELECT COLUMN_NAME, SQLTypeName(DATA_TYPE), COLUMN_SIZE, DECIMAL_DIGITS, NULLABLE, KEY_SEQ, COLUMN_DEF
                 FROM SYSTEM.CATALOG
                 WHERE TENANT_ID IS NULL AND {} AND TABLE_NAME = {} AND COLUMN_NAME IS NOT NULL
                 ORDER BY ORDINAL_POSITION",
                schema_cond("TABLE_SCHEM", obj.schema()),
                lit(&obj.name)
            ))
            .await?;
        Ok(rows
            .into_iter()
            .map(|r| ColumnInfo {
                name: text(&r[0]),
                data_type: ddl::type_name(&text(&r[1]), &text(&r[2]), &text(&r[3])),
                // java.sql.DatabaseMetaData.columnNoNulls = 0
                nullable: text(&r[4]) != "0",
                primary_key: !text(&r[5]).is_empty(),
                auto_increment: false,
                default_value: Some(text(&r[6])).filter(|d| !d.is_empty()),
            })
            .collect())
    }

    async fn definition(&mut self, obj: &ObjectRef) -> Result<Option<String>> {
        if self.generic {
            // Avatica's metadata calls carry no definitions.
            return Ok(None);
        }
        match obj.kind.as_str() {
            kinds::VIEW => {
                let rows = self
                    .rows(&format!(
                        "SELECT VIEW_STATEMENT FROM SYSTEM.CATALOG
                         WHERE TENANT_ID IS NULL AND {} AND TABLE_NAME = {} AND VIEW_STATEMENT IS NOT NULL LIMIT 1",
                        schema_cond("TABLE_SCHEM", obj.schema()),
                        lit(&obj.name)
                    ))
                    .await?;
                Ok(rows.first().map(|r| text(&r[0])).filter(|s| !s.is_empty()))
            }
            kinds::SEQUENCE => {
                let rows = self
                    .rows(&format!(
                        "SELECT START_WITH, INCREMENT_BY, CACHE_SIZE FROM SYSTEM.\"SEQUENCE\"
                         WHERE TENANT_ID IS NULL AND {} AND SEQUENCE_NAME = {}",
                        schema_cond("SEQUENCE_SCHEMA", obj.schema()),
                        lit(&obj.name)
                    ))
                    .await?;
                Ok(rows.first().map(|r| {
                    format!(
                        "CREATE SEQUENCE {} START WITH {} INCREMENT BY {} CACHE {};",
                        dbine_driver::sql::qualified_name(Quote::Double, obj.schema(), &obj.name),
                        text(&r[0]),
                        text(&r[1]),
                        text(&r[2])
                    )
                }))
            }
            // Phoenix keeps no DDL text for tables: the UI builds it from the columns.
            _ => Ok(None),
        }
    }

    /// Neither Phoenix nor HBase keep a list of running queries that a
    /// client can read or stop, and the Query Server only knows its own
    /// connections.
    async fn processes(&mut self) -> Result<Vec<dbine_driver::ServerProcess>> {
        Err(Error::Unsupported(UNSUPPORTED_PROCESSES.into()))
    }

    async fn cancel_query(&mut self, _id: &str) -> Result<()> {
        Err(Error::Unsupported(UNSUPPORTED_PROCESSES.into()))
    }

    async fn monitor(&mut self) -> Result<dbine_driver::MonitorSnapshot> {
        if self.generic {
            return Err(Error::Unsupported("un servidor Avatica genérico no informa métricas".into()));
        }
        self.snapshot().await
    }

    /// Typed frames (`fetch`) straight into cells.
    async fn read_batches(&mut self, spec: &dbine_driver::transfer::ReadSpec, sink: dbine_driver::transfer::BatchSinkRef) -> Result<u64> {
        self.transfer_read(spec, sink).await
    }

    async fn bulk_load(
        &mut self,
        spec: &dbine_driver::transfer::LoadSpec,
        _columns: &[dbine_driver::transfer::TransferColumn],
        source: &mut dyn dbine_driver::transfer::BatchSource,
        progress: dbine_driver::transfer::Progress<'_>,
    ) -> Result<u64> {
        self.transfer_load(spec, source, progress).await
    }

    async fn database_schema(&mut self) -> Result<Vec<TableSchema>> {
        if self.generic {
            return self.avatica_schema().await;
        }
        Ok(ddl::from_catalog(self.rows(ddl::CATALOG_QUERY).await?))
    }

    async fn index_usage(&mut self, table: &ObjectRef) -> Result<Option<dbine_driver::IndexUsageReport>> {
        if self.generic {
            return Ok(None);
        }
        let rows = self.rows(ddl::CATALOG_QUERY).await?;
        let covered = index_usage::covered(&rows);
        let tables = ddl::from_catalog(rows);
        let found = tables.iter().find(|t| t.name == table.name && t.schema.as_deref() == table.schema());
        Ok(Some(index_usage::report(found, &covered)))
    }

    fn browse_query(&self, obj: &ObjectRef, limit: u32) -> String {
        select_top(Quote::Double, Limit::Limit, obj.schema(), &obj.name, limit)
    }

    async fn execute(&mut self, text: &str, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        for (stmt, start) in statements(text) {
            let before = out.results.len();
            self.run(&stmt, max_rows, out).await.map_err(|e| place(e, text, start))?;
            match leading_keyword(&stmt, &dialect()).as_deref() {
                Some("commit" | "rollback") => self.dirty = false,
                _ if self.manual && out.results[before..].iter().any(|r| r.columns.is_empty()) => self.dirty = true,
                _ => {}
            }
        }
        Ok(())
    }

    async fn transaction_state(&mut self) -> Result<Option<TxState>> {
        Ok(Some(if self.manual && self.dirty { TxState::Open } else { TxState::Idle }))
    }

    async fn set_autocommit(&mut self, on: bool) -> Result<()> {
        self.client.set_auto_commit(on).await?;
        self.manual = !on;
        if on {
            self.dirty = false;
        }
        Ok(())
    }

    async fn commit(&mut self) -> Result<()> {
        self.client.end(true).await?;
        self.dirty = false;
        Ok(())
    }

    async fn rollback(&mut self) -> Result<()> {
        self.client.end(false).await?;
        self.dirty = false;
        Ok(())
    }

    async fn explain(&mut self, script: &str, analyze: bool, max_rows: usize, out: &mut QueryOutcome) -> Result<()> {
        if self.generic {
            return self.avatica_explain(script, analyze, max_rows, out).await;
        }
        let mut noted = false;
        for (stmt, _) in statements(script) {
            let first = stmt.split_whitespace().next().unwrap_or_default().to_ascii_uppercase();
            if matches!(first.as_str(), "SELECT" | "UPSERT" | "DELETE" | "WITH") {
                let mut local = QueryOutcome::default();
                self.run(&format!("EXPLAIN {stmt}"), 10_000, &mut local).await?;
                let rs = local.results.pop().unwrap_or_default();
                let col = |name: &str| rs.columns.iter().position(|c| c.name.eq_ignore_ascii_case(name));
                let plan_col = col("PLAN").unwrap_or(0);
                let steps: Vec<String> = rs.rows.iter().map(|r| r.get(plan_col).map(text).unwrap_or_default()).collect();
                let first_num = |name: &str| {
                    let i = col(name)?;
                    rs.rows.iter().find_map(|r| r.get(i).and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok())))
                };
                out.plans.push(plan::from_rows(&stmt, &steps, first_num("EST_ROWS_READ"), first_num("EST_BYTES_READ")));
                if analyze && !noted {
                    out.messages.push(
                        "Phoenix no da cifras reales por paso: se muestra el plan estimado junto al resultado.".into(),
                    );
                    noted = true;
                }
            } else if !analyze {
                out.messages.push(format!("Sin plan para «{}»: Phoenix solo explica SELECT, UPSERT y DELETE.", stmt.trim()));
            }
            if analyze {
                self.run(&stmt, max_rows, out).await?;
            }
        }
        Ok(())
    }
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
            let grant = |grantable| d.schema_grant_script(None, "VENTAS", &[p.to_string()], "ana", grantable);
            assert!(grant(false).is_ok(), "{}", d.info().id);
            assert_eq!(grant(true).is_ok(), spec.grant_option, "{}: {:?}", d.info().id, grant(true));
        }
    }

    #[test]
    fn typed_values_become_cells() {
        let tv = |r#type: i32| proto::TypedValue { r#type, ..Default::default() };
        assert_eq!(typed(&proto::TypedValue { number_value: 42, ..tv(12) }), json!(42));
        assert_eq!(typed(&proto::TypedValue { string_value: "1.50".into(), ..tv(proto::rep::BIG_DECIMAL) }), json!("1.50"));
        assert_eq!(typed(&proto::TypedValue { number_value: 19753, ..tv(proto::rep::JAVA_SQL_DATE) }), json!("2024-01-31"));
        assert_eq!(
            typed(&proto::TypedValue { number_value: 1_706_708_700_000, ..tv(proto::rep::JAVA_SQL_TIMESTAMP) }),
            json!("2024-01-31 13:45:00")
        );
        assert_eq!(typed(&proto::TypedValue { bytes_value: vec![0xCA, 0xFE], ..tv(proto::rep::BYTE_STRING) }), json!("0xCAFE"));
        assert_eq!(typed(&proto::TypedValue { null: true, ..tv(proto::rep::STRING) }), Value::Null);
    }

    #[test]
    fn wire_messages_round_trip() {
        let rs = proto::ExecuteResponse {
            results: vec![proto::ResultSetResponse {
                statement_id: 3,
                signature: Some(proto::Signature {
                    columns: vec![proto::ColumnMetaData {
                        label: "A".into(),
                        r#type: Some(proto::AvaticaType { name: "INTEGER".into(), ..Default::default() }),
                        ..Default::default()
                    }],
                }),
                first_frame: Some(proto::Frame {
                    done: true,
                    rows: vec![proto::Row {
                        value: vec![proto::ColumnValue {
                            scalar_value: Some(proto::TypedValue { r#type: 12, number_value: 7, ..Default::default() }),
                            ..Default::default()
                        }],
                    }],
                    ..Default::default()
                }),
                update_count: u64::MAX,
                ..Default::default()
            }],
            missing_statement: false,
        };
        let wire = proto::WireMessage { name: format!("{}ExecuteResponse", proto::RESP), wrapped_message: rs.encode_to_vec() };
        let Resp::Execute(r) = parse_proto(&Req::Execute { statement: 3, sql: String::new() }, &wire.encode_to_vec()).unwrap() else {
            panic!()
        };
        assert_eq!(r[0].columns[0].name, "A");
        assert!(r[0].update_count.is_none());
        assert_eq!(r[0].frame.as_ref().unwrap().rows, vec![vec![json!(7)]]);

        let err = proto::ErrorResponse { error_message: "ERROR 1012 (42M03): Table undefined.".into(), ..Default::default() };
        let wire = proto::WireMessage { name: format!("{}ErrorResponse", proto::RESP), wrapped_message: err.encode_to_vec() };
        let e = parse_proto(&Req::Close, &wire.encode_to_vec()).err().unwrap().to_script_error();
        assert_eq!((e.code.as_deref(), e.sqlstate.as_deref()), (Some("1012"), Some("42M03")));
        assert!(e.message.contains("Table undefined"));

        // Phoenix's own code inside the Query Server's wrapping, and where.
        let e = server_error("RuntimeException: x: ERROR 603 (42P00): Syntax error. Got \"x\" at line 2, column 3. -> y".into(), u32::MAX, "");
        let script = "SELECT 1;\nSELECT 2\n  x";
        let e = place(e, script, 10).to_script_error();
        assert_eq!(e.message, "ERROR 603 (42P00): Syntax error. Got \"x\" at line 2, column 3.");
        assert_eq!((e.code.as_deref(), e.offset, e.line), (Some("603"), Some(script.rfind('x').unwrap()), Some(3)));
    }

    #[test]
    fn json_cells_follow_reps() {
        assert_eq!(json_cell(json!(1_706_708_700_000i64), "JAVA_SQL_TIMESTAMP"), json!("2024-01-31 13:45:00"));
        assert_eq!(json_cell(json!(1.5), "BIG_DECIMAL"), json!("1.5"));
        assert_eq!(json_cell(json!([1, 2]), "ARRAY"), json!("[1,2]"));
        assert_eq!(schema_cond("S", Some("o'k")), "S = 'o''k'");
        assert_eq!(schema_cond("S", None), "S IS NULL");
    }
}
