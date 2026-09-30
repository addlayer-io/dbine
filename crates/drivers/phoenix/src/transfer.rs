//! Bulk transfer (see `dbine_driver::transfer`) over Avatica: Apache Phoenix
//! (Query Server) and generic Avatica servers.
//!
//! Reading: one `SELECT` run with `prepareAndExecute` and read with
//! `fetch`. Frames are sized by bytes: as many rows as fit in
//! [`FRAME_BYTES`] (at most [`FRAME_ROWS`]) if each were the largest row the
//! read can meet, known before the first frame from the column types and,
//! for values with no size limit (`VARCHAR`, `VARBINARY`, arrays), from one
//! aggregate query that measures the largest of them in the rows to read
//! (a column nothing can measure: one row a frame). Small rows followed by
//! big ones never pile up a big frame in memory. Values become cells by their
//! Avatica `Rep` and the column's SQL type: integers, floats, `DECIMAL` as
//! exact digits (in JSON serialization too: numbers are taken from their
//! text, never through an `f64`), dates and times, binaries whole, arrays
//! as JSON. Phoenix's `DATE` and `TIME` hold a date and a time to the
//! millisecond, but Avatica sends a `DATE` as days and a `TIME` as a time
//! of day: they (and arrays of them, element by element) are read `CAST` to
//! `TIMESTAMP`, and become `Date` / `Time` cells only when that loses
//! nothing (midnight; 1970-01-01). A name that repeats in two column
//! families is named with its family (from `SYSTEM.CATALOG`) wherever the
//! read names columns; if the catalog can't tell them apart the read fails.
//! The row bound is measured just before the read, in its own query: a row
//! written or grown in between (a concurrent writer) can make one frame
//! larger than [`FRAME_BYTES`]; the frames after it follow the rows seen.
//!
//! Loading: a prepared `UPSERT` (Phoenix; `… ON DUPLICATE KEY IGNORE` into
//! a table that already has rows) or `INSERT` (other servers), rows sent
//! with `executeBatch` as parameter sets typed after the statement's
//! parameters (Phoenix's own `UNSIGNED_*` type codes included); rows with
//! arrays go one `execute` each, the only request that turns an Avatica
//! array into a JDBC one. Auto-commit off and a
//! `commit` every `commit_rows` rows or `commit_bytes` bytes, never over
//! what Phoenix's mutation buffer takes ([`MAX_WINDOW_ROWS`],
//! [`MAX_WINDOW_BYTES`] as Phoenix measures it). A Phoenix load never
//! overwrites an existing row: a key already in the table is left alone,
//! and a key already in the table or repeated in the source fails the
//! load when it counts the table at the end. Every request runs in its own task: a load that fails or is
//! cancelled (its future dropped) waits for the request the server already
//! has, reports a commit that landed and rolls back the rest before it
//! returns, so no row lands after it. Waiting in `Drop` needs a
//! multi-threaded tokio runtime (the app's and the driver host's): on a
//! single-threaded one the load refuses to start.
//!
//! Limits of the protocol: times and timestamps cross it in milliseconds
//! (a finer fraction fails the load instead of being cut, and Phoenix
//! values finer than that are read to the millisecond); FLOAT parameters
//! go as DOUBLE (Avatica's FLOAT parameter reaches Phoenix as 0); arrays
//! can't be parameters in JSON serialization.

use crate::{proto, Client, PhoenixSession};
use base64::Engine as _;
use chrono::{NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dbine_driver::sql::{qualified_name, quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use prost::Message;
use serde_json::{json, Value};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::JoinHandle;

/// A read's frames hold as many rows as fit in this much body, counting
/// every row as the largest the read can meet (see [`Width`])…
const FRAME_BYTES: u64 = 2 * 1024 * 1024;
/// …and never more than this many.
const FRAME_ROWS: i32 = 5_000;
/// What a value takes on the wire and in memory beyond its own bytes
/// (twice in protobuf, see [`width`]).
const VALUE_OVERHEAD: u64 = 48;
/// A declared length (`VARCHAR(n)`, `VARBINARY(n)`) above this many bytes
/// is measured in the data instead.
const DECLARED_MAX: u64 = 64 * 1024;
/// The largest uncommitted window in rows (Phoenix's
/// `phoenix.mutate.maxSize` is 500,000 by default)…
const MAX_WINDOW_ROWS: u64 = 250_000;
/// …and in bytes, as Phoenix sizes its mutation buffer
/// (`phoenix.mutate.maxSizeBytes`, 100 MB by default; see [`mutation_size`]).
const MAX_WINDOW_BYTES: u64 = 48 * 1024 * 1024;

// ---- Avatica messages the transfer adds (see `proto.rs`) ----

#[derive(Clone, PartialEq, Message)]
struct PrepareAndExecuteRequest {
    #[prost(string, tag = "1")]
    connection_id: String,
    #[prost(string, tag = "2")]
    sql: String,
    #[prost(uint64, tag = "3")]
    max_row_count: u64,
    #[prost(uint32, tag = "4")]
    statement_id: u32,
    #[prost(int64, tag = "5")]
    max_rows_total: i64,
    #[prost(int32, tag = "6")]
    first_frame_max_size: i32,
}

#[derive(Clone, PartialEq, Message)]
struct FetchRequest {
    #[prost(string, tag = "1")]
    connection_id: String,
    #[prost(uint32, tag = "2")]
    statement_id: u32,
    #[prost(uint64, tag = "3")]
    offset: u64,
    #[prost(uint32, tag = "4")]
    fetch_max_row_count: u32,
    #[prost(int32, tag = "5")]
    frame_max_size: i32,
}

#[derive(Clone, PartialEq, Message)]
struct PrepareRequest {
    #[prost(string, tag = "1")]
    connection_id: String,
    #[prost(string, tag = "2")]
    sql: String,
    #[prost(uint64, tag = "3")]
    max_row_count: u64,
    #[prost(int64, tag = "4")]
    max_rows_total: i64,
}

/// Avatica's `TypedValue` as a parameter (with the arrays' component type,
/// which `proto::TypedValue` doesn't declare).
#[derive(Clone, PartialEq, Message)]
struct TypedParam {
    #[prost(int32, tag = "1")]
    r#type: i32,
    #[prost(bool, tag = "2")]
    bool_value: bool,
    #[prost(string, tag = "3")]
    string_value: String,
    #[prost(sint64, tag = "4")]
    number_value: i64,
    #[prost(bytes = "vec", tag = "5")]
    bytes_value: Vec<u8>,
    #[prost(double, tag = "6")]
    double_value: f64,
    #[prost(bool, tag = "7")]
    null: bool,
    #[prost(message, repeated, tag = "8")]
    array_value: Vec<TypedParam>,
    #[prost(int32, tag = "9")]
    component_type: i32,
}

#[derive(Clone, PartialEq, Message)]
struct UpdateBatch {
    #[prost(message, repeated, tag = "1")]
    parameter_values: Vec<TypedParam>,
}

#[derive(Clone, PartialEq, Message)]
struct ExecuteBatchRequest {
    #[prost(string, tag = "1")]
    connection_id: String,
    #[prost(uint32, tag = "2")]
    statement_id: u32,
    #[prost(message, repeated, tag = "3")]
    updates: Vec<UpdateBatch>,
}

#[derive(Clone, PartialEq, Message)]
struct ConnectionIdRequest {
    #[prost(string, tag = "1")]
    connection_id: String,
}

#[derive(Clone, PartialEq, Message)]
struct ConnProps {
    #[prost(bool, tag = "1")]
    is_dirty: bool,
    #[prost(bool, tag = "2")]
    auto_commit: bool,
    #[prost(bool, tag = "7")]
    has_auto_commit: bool,
}

#[derive(Clone, PartialEq, Message)]
struct ConnectionSyncRequest {
    #[prost(string, tag = "1")]
    connection_id: String,
    #[prost(message, optional, tag = "2")]
    conn_props: Option<ConnProps>,
}

#[derive(Clone, PartialEq, Message)]
struct CloseStatementRequest {
    #[prost(string, tag = "1")]
    connection_id: String,
    #[prost(uint32, tag = "2")]
    statement_id: u32,
}

#[derive(Clone, PartialEq, Message)]
struct ColumnMeta {
    #[prost(uint32, tag = "6")]
    nullable: u32,
    #[prost(string, tag = "9")]
    label: String,
    #[prost(string, tag = "10")]
    column_name: String,
    #[prost(uint32, tag = "12")]
    precision: u32,
    #[prost(uint32, tag = "13")]
    scale: u32,
    #[prost(message, optional, tag = "20")]
    r#type: Option<proto::AvaticaType>,
}

#[derive(Clone, PartialEq, Message)]
struct Parameter {
    #[prost(uint32, tag = "4")]
    parameter_type: u32,
    #[prost(string, tag = "5")]
    type_name: String,
}

#[derive(Clone, PartialEq, Message)]
struct Signature {
    #[prost(message, repeated, tag = "1")]
    columns: Vec<ColumnMeta>,
    #[prost(message, repeated, tag = "3")]
    parameters: Vec<Parameter>,
}

#[derive(Clone, PartialEq, Message)]
struct StatementHandle {
    #[prost(uint32, tag = "2")]
    id: u32,
    #[prost(message, optional, tag = "3")]
    signature: Option<Signature>,
}

#[derive(Clone, PartialEq, Message)]
struct PrepareResponse {
    #[prost(message, optional, tag = "1")]
    statement: Option<StatementHandle>,
}

#[derive(Clone, PartialEq, Message)]
struct ResultSetResponse {
    #[prost(uint32, tag = "2")]
    statement_id: u32,
    #[prost(message, optional, tag = "4")]
    signature: Option<Signature>,
    #[prost(message, optional, tag = "5")]
    first_frame: Option<proto::Frame>,
}

#[derive(Clone, PartialEq, Message)]
struct ExecuteResponse {
    #[prost(message, repeated, tag = "1")]
    results: Vec<ResultSetResponse>,
    #[prost(bool, tag = "2")]
    missing_statement: bool,
}

/// A statement handle with its signature kept as the server sent it.
#[derive(Clone, PartialEq, Message)]
struct RawHandle {
    #[prost(string, tag = "1")]
    connection_id: String,
    #[prost(uint32, tag = "2")]
    id: u32,
    #[prost(bytes = "vec", tag = "3")]
    signature: Vec<u8>,
}

#[derive(Clone, PartialEq, Message)]
struct RawPrepareResponse {
    #[prost(message, optional, tag = "1")]
    statement: Option<RawHandle>,
}

/// `execute` of a prepared statement, one parameter set.
#[derive(Clone, PartialEq, Message)]
struct ExecuteRequest {
    #[prost(message, optional, tag = "1")]
    statement_handle: Option<RawHandle>,
    #[prost(message, repeated, tag = "2")]
    parameter_values: Vec<TypedParam>,
    #[prost(bool, tag = "4")]
    has_parameter_values: bool,
    #[prost(int32, tag = "5")]
    first_frame_max_size: i32,
}

#[derive(Clone, PartialEq, Message)]
struct ExecuteBatchResponse {
    #[prost(uint64, repeated, tag = "3")]
    update_counts: Vec<u64>,
    #[prost(bool, tag = "4")]
    missing_statement: bool,
}

/// A request, built in the session's serialization.
enum Body {
    Proto(&'static str, Vec<u8>),
    Json(String),
}

/// A response: the protobuf message's bytes, or the JSON (and its size).
enum Raw {
    Proto(Vec<u8>),
    Json(Value, usize),
}

impl Raw {
    fn len(&self) -> usize {
        match self {
            Raw::Proto(b) => b.len(),
            Raw::Json(_, n) => *n,
        }
    }
}

impl Client {
    /// A copy that owns its fields, for a request run in its own task.
    fn detached(&self) -> Client {
        Client {
            http: self.http.clone(),
            url: self.url.clone(),
            user: self.user.clone(),
            password: self.password.clone(),
            json: self.json,
            connection_id: self.connection_id.clone(),
        }
    }

    /// A request built only in the session's serialization.
    fn body(&self, proto_req: impl FnOnce() -> (&'static str, Vec<u8>), json_req: impl FnOnce() -> Value) -> Body {
        if self.json {
            Body::Json(json_req().to_string())
        } else {
            let (name, bytes) = proto_req();
            Body::Proto(name, bytes)
        }
    }

    async fn raw(&self, proto_req: impl FnOnce() -> (&'static str, Vec<u8>), json_req: impl FnOnce() -> Value) -> Result<Raw> {
        self.send(self.body(proto_req, json_req), false).await
    }

    /// One Avatica call. `exact`: JSON numbers that an `f64` can't hold
    /// exactly are kept as their text (see [`exact_numbers`]).
    async fn send(&self, body: Body, exact: bool) -> Result<Raw> {
        let rb = self.http.post(&self.url);
        let rb = match &self.user {
            Some(u) => rb.basic_auth(u, self.password.as_deref()),
            None => rb,
        };
        let rb = match body {
            Body::Json(text) => rb.header("Content-Type", "application/json").body(text),
            Body::Proto(name, bytes) => {
                let wire = proto::WireMessage { name: format!("{}{name}", proto::REQ), wrapped_message: bytes };
                rb.header("Content-Type", "application/octet-stream").body(wire.encode_to_vec())
            }
        };
        let resp = rb.send().await.map_err(crate::http_error)?;
        let status = resp.status();
        if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
            return Err(Error::AuthFailed(format!("HTTP {status}")));
        }
        let body = resp.bytes().await.map_err(crate::http_error)?;
        if self.json {
            let len = body.len();
            let parsed = if exact { serde_json::from_slice::<Value>(&exact_numbers(&body)) } else { serde_json::from_slice::<Value>(&body) };
            let v = parsed.map_err(|_| Error::Query(format!("HTTP {status}: {}", String::from_utf8_lossy(&body).trim())))?;
            drop(body);
            if v.get("response").and_then(Value::as_str) == Some("error") {
                return Err(crate::server_error(
                    v.get("errorMessage").and_then(Value::as_str).unwrap_or_default().to_string(),
                    v.get("errorCode").and_then(Value::as_u64).unwrap_or(0) as u32,
                    v.get("sqlState").and_then(Value::as_str).unwrap_or_default(),
                ));
            }
            return Ok(Raw::Json(v, len));
        }
        let wire = proto::WireMessage::decode(&*body).map_err(|_| Error::Query(format!("HTTP {status}: respuesta que no es protobuf")))?;
        drop(body);
        if wire.name.ends_with("ErrorResponse") {
            let e = proto::ErrorResponse::decode(wire.wrapped_message.as_slice()).map_err(crate::decode_err)?;
            return Err(crate::server_error(e.error_message, e.error_code, &e.sql_state));
        }
        Ok(Raw::Proto(wire.wrapped_message))
    }

    fn auto_commit_body(&self, on: bool) -> Body {
        let c = &self.connection_id;
        self.body(
            || {
                let props = ConnProps { is_dirty: true, auto_commit: on, has_auto_commit: true };
                ("ConnectionSyncRequest", ConnectionSyncRequest { connection_id: c.clone(), conn_props: Some(props) }.encode_to_vec())
            },
            || json!({"request": "connectionSync", "connectionId": c, "connProps": {"connProps": "connPropsImpl", "autoCommit": on, "dirty": true}}),
        )
    }

    async fn set_auto_commit(&self, on: bool) -> Result<()> {
        self.send(self.auto_commit_body(on), false).await.map(|_| ())
    }

    /// `commit` or `rollback`.
    fn end_body(&self, commit: bool) -> Body {
        let c = &self.connection_id;
        let (name, req) = if commit { ("CommitRequest", "commit") } else { ("RollbackRequest", "rollback") };
        self.body(|| (name, ConnectionIdRequest { connection_id: c.clone() }.encode_to_vec()), || json!({"request": req, "connectionId": c}))
    }

    async fn end(&self, commit: bool) -> Result<()> {
        self.send(self.end_body(commit), false).await.map(|_| ())
    }

    async fn close_statement(&self, statement: u32) -> Result<()> {
        let c = &self.connection_id;
        self.raw(
            || ("CloseStatementRequest", CloseStatementRequest { connection_id: c.clone(), statement_id: statement }.encode_to_vec()),
            || json!({"request": "closeStatement", "connectionId": c, "statementId": statement}),
        )
        .await
        .map(|_| ())
    }
}

/// Numbers in a JSON body that an `f64` can't be trusted with (a fraction,
/// an exponent, or more digits than an `i64`/`u64` keeps) become
/// `{"\u0000n": "<the number's text>"}`: serde_json without
/// `arbitrary_precision` would round them (a `DECIMAL` such as
/// `12345678901234567.89`). Strings are left as they are.
fn exact_numbers(body: &[u8]) -> std::borrow::Cow<'_, [u8]> {
    let mut out: Option<Vec<u8>> = None;
    let (mut i, mut last) = (0, 0);
    while i < body.len() {
        match body[i] {
            b'"' => {
                i += 1;
                while i < body.len() && body[i] != b'"' {
                    i += if body[i] == b'\\' { 2 } else { 1 };
                }
                i += 1;
            }
            b'-' | b'0'..=b'9' => {
                let start = i;
                i += 1;
                while i < body.len() && matches!(body[i], b'0'..=b'9' | b'.' | b'e' | b'E' | b'+' | b'-') {
                    i += 1;
                }
                let n = &body[start..i];
                let digits = n.iter().filter(|b| b.is_ascii_digit()).count();
                if n.iter().any(|b| matches!(b, b'.' | b'e' | b'E')) || digits > 18 {
                    let o = out.get_or_insert_with(|| Vec::with_capacity(body.len() + 256));
                    o.extend_from_slice(&body[last..start]);
                    o.extend_from_slice(br#"{"\u0000n":""#);
                    o.extend_from_slice(n);
                    o.extend_from_slice(br#""}"#);
                    last = i;
                }
            }
            _ => i += 1,
        }
    }
    match out {
        Some(mut o) => {
            o.extend_from_slice(&body[last.min(body.len())..]);
            std::borrow::Cow::Owned(o)
        }
        None => std::borrow::Cow::Borrowed(body),
    }
}

/// A number [`exact_numbers`] kept as text.
fn exact_text(v: &Value) -> Option<&str> {
    match v {
        Value::Object(m) if m.len() == 1 => m.get("\u{0}n").and_then(Value::as_str),
        _ => None,
    }
}

/// A JSON value as text, with the kept numbers written back as numbers.
fn json_text(v: &Value, out: &mut String) {
    if let Some(t) = exact_text(v) {
        out.push_str(t);
        return;
    }
    match v {
        Value::Array(a) => {
            out.push('[');
            for (i, x) in a.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                json_text(x, out);
            }
            out.push(']');
        }
        Value::Object(m) => {
            out.push('{');
            for (i, (k, x)) in m.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                out.push_str(&Value::String(k.clone()).to_string());
                out.push(':');
                json_text(x, out);
            }
            out.push('}');
        }
        v => out.push_str(&v.to_string()),
    }
}

/// A Phoenix `DATE` / `TIME` read as `TIMESTAMP` (Avatica would cut it).
#[derive(Clone, Copy, PartialEq, Debug)]
enum Widened {
    Date,
    Time,
}

/// A result column: name, SQL type, `Rep`.
struct Col {
    name: String,
    type_name: String,
    nullable: bool,
    rep: String,
    widened: Option<Widened>,
    /// Its column family, when the read has to name it (see
    /// [`PhoenixSession::qualify`]).
    family: Option<String>,
}

/// How a query names `col`: `"CF"."COL"` when its family is known.
fn col_ref(col: &Col) -> String {
    let q = quote_ident(Quote::Double, &col.name);
    match &col.family {
        Some(f) => format!("{}.{q}", quote_ident(Quote::Double, f)),
        None => q,
    }
}

/// Rows of a frame: what fits in [`FRAME_BYTES`] with every row as large as
/// `bound` (the largest row the read can meet; `None`: unknown, one row a
/// frame) or as the last frame's rows (`rows` in `bytes`) if those were
/// larger (rows grown since the bound was measured).
fn frame_rows(bound: Option<u64>, rows: usize, bytes: usize) -> i32 {
    let Some(bound) = bound else { return 1 };
    let seen = bytes.checked_div(rows).unwrap_or(0) as u64;
    (FRAME_BYTES / bound.max(seen).max(1)).clamp(1, FRAME_ROWS as u64) as i32
}

/// How many bytes a column's values can take at most.
#[derive(Debug, PartialEq)]
enum Width {
    /// Known from the type.
    Fixed(u64),
    /// Only the data tells: this SQL expression gives a value's bytes.
    Data(String),
    /// Nothing tells (a type this reader can't measure).
    Unknown,
}

fn is_array(type_name: &str) -> bool {
    type_name.trim().to_ascii_uppercase().ends_with(" ARRAY")
}

/// A type's name without `UNSIGNED_` and its length: `VARCHAR(5)` →
/// (`VARCHAR`, 5).
fn base_type(name: &str) -> (String, Option<u64>) {
    let name = name.trim().to_ascii_uppercase();
    let name = name.strip_prefix("UNSIGNED_").unwrap_or(&name);
    match name.split_once('(') {
        Some((b, rest)) => (b.trim().to_string(), rest.split([',', ')']).next().and_then(|n| n.trim().parse().ok())),
        None => (name.trim().to_string(), None),
    }
}

/// Bytes of a value of a fixed-size type (as text in JSON too).
fn fixed_width(name: &str, phoenix: bool) -> Option<u64> {
    let (base, len) = base_type(name);
    Some(match base.as_str() {
        "BOOLEAN" => 5,
        "TINYINT" | "SMALLINT" | "INTEGER" | "INT" | "BIGINT" | "LONG" | "FLOAT" | "REAL" | "DOUBLE" | "DATE" | "TIME" | "TIMESTAMP" => 24,
        // Phoenix's DECIMAL holds at most 38 digits.
        "DECIMAL" | "NUMERIC" => len.unwrap_or(if phoenix { 38 } else { return None }) + 8,
        _ => return None,
    })
}

/// The most bytes a value of `col` can take, in this serialization.
fn width(col: &Col, phoenix: bool, json: bool) -> Width {
    // JSON: a character takes up to 6 bytes (an escape), a byte 4/3 (base64).
    // Protobuf: Avatica writes every value twice (`value` and `scalar_value`),
    // a character in up to 4 bytes of UTF-8 and a byte both as itself and in
    // base64 (measured: 100 KB of VARBINARY take 467 KB in a frame).
    let (per_char, per_byte) = if json { (6, 2) } else { (8, 5) };
    let q = col_ref(col);
    let name = col.type_name.trim().to_ascii_uppercase();
    if let Some(elem) = name.strip_suffix(" ARRAY") {
        if !phoenix {
            return Width::Unknown;
        }
        if let Some(w) = fixed_width(elem, true) {
            return Width::Data(format!("CAST(ARRAY_LENGTH({q}) AS BIGINT) * {}", w + VALUE_OVERHEAD));
        }
        return match base_type(elem) {
            // Joined with a separator: Phoenix takes '' for NULL.
            (b, _) if b == "VARCHAR" || b == "CHAR" => Width::Data(format!(
                "CAST(LENGTH(ARRAY_TO_STRING({q}, ',')) AS BIGINT) * {per_char} + CAST(ARRAY_LENGTH({q}) AS BIGINT) * {VALUE_OVERHEAD}"
            )),
            (b, Some(n)) if b == "BINARY" => Width::Data(format!("CAST(ARRAY_LENGTH({q}) AS BIGINT) * {}", n * per_byte + VALUE_OVERHEAD)),
            _ => Width::Unknown,
        };
    }
    if let Some(w) = fixed_width(&name, phoenix) {
        return Width::Fixed(w);
    }
    let (base, len) = base_type(&name);
    let (factor, measure) = match base.as_str() {
        "VARCHAR" | "CHAR" => (per_char, if phoenix { "LENGTH" } else { "CHAR_LENGTH" }),
        "VARBINARY" | "BINARY" => (per_byte, "OCTET_LENGTH"),
        _ => return Width::Unknown,
    };
    match len {
        Some(n) if n > 0 && n.saturating_mul(factor) <= DECLARED_MAX => Width::Fixed(n * factor),
        _ => Width::Data(format!("CAST({measure}({q}) AS BIGINT) * {factor}")),
    }
}

/// The largest row `cols` can have: what the types fix, plus the query
/// (on the rows the read will read) that measures the rest, if any.
/// `None`: a column nothing can measure.
fn row_bound_parts(cols: &[Col], phoenix: bool, json: bool, from: &str) -> Option<(u64, Option<String>)> {
    let mut fixed = 0u64;
    let mut data = Vec::new();
    for c in cols {
        fixed += VALUE_OVERHEAD;
        match width(c, phoenix, json) {
            Width::Fixed(w) => fixed += w,
            Width::Data(e) => data.push(format!("COALESCE({e}, 0)")),
            Width::Unknown => return None,
        }
    }
    let probe = (!data.is_empty()).then(|| format!("SELECT MAX({}) {from}", data.join(" + ")));
    Some((fixed, probe))
}

impl PhoenixSession {
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let list = match &spec.columns {
            Some(c) if !c.is_empty() => c.iter().map(|n| quote_ident(Quote::Double, n)).collect::<Vec<_>>().join(", "),
            _ => "*".to_string(),
        };
        let mut from = format!("FROM {}", qualified_name(Quote::Double, spec.table.schema(), &spec.table.name));
        if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            from.push_str(&format!(" WHERE {f}"));
        }
        let mut sql = format!("SELECT {list} {from}");
        // Phoenix: DATE and TIME columns (and arrays of them) are read as TIMESTAMP.
        let mut described = match self.describe(&sql).await {
            Ok(cols) => cols,
            // A generic server that can't prepare it: read without the bound.
            Err(e) if self.generic => {
                tracing::debug!("avatica: no se pudo preparar la lectura ({e}); se lee de a una fila");
                Vec::new()
            }
            Err(e) => return Err(e),
        };
        self.qualify(spec, &mut described).await?;
        if described.iter().any(|c| c.widened.is_some()) {
            let list = described
                .iter()
                .map(|c| {
                    let (r, q) = (col_ref(c), quote_ident(Quote::Double, &c.name));
                    match c.widened {
                        Some(_) if is_array(&c.type_name) => format!("CAST({r} AS TIMESTAMP ARRAY) AS {q}"),
                        Some(_) => format!("CAST({r} AS TIMESTAMP) AS {q}"),
                        None => r,
                    }
                })
                .collect::<Vec<_>>()
                .join(", ");
            sql = format!("SELECT {list} {from}");
        }
        // Frames by bytes: the largest row the read can meet, before any row comes.
        let bound = if described.is_empty() { None } else { self.row_bound(&described, &from).await };
        let described = Some(described);
        // The columns as the table has them (types and nullability before any CAST).
        let merge = |mut cols: Vec<Col>| -> Vec<Col> {
            if let Some(d) = &described {
                if d.len() == cols.len() {
                    for (c, d) in cols.iter_mut().zip(d) {
                        c.type_name = d.type_name.clone();
                        c.nullable = d.nullable;
                        c.widened = d.widened;
                    }
                }
            }
            cols
        };
        let first = frame_rows(bound, 0, 0);
        let (c, st) = (self.client.connection_id.clone(), self.statement);
        let body = self.client.body(
            || {
                let r = PrepareAndExecuteRequest {
                    connection_id: c.clone(),
                    sql: sql.clone(),
                    max_row_count: u64::MAX,
                    statement_id: st,
                    max_rows_total: -1,
                    first_frame_max_size: first,
                };
                ("PrepareAndExecuteRequest", r.encode_to_vec())
            },
            || {
                json!({"request": "prepareAndExecute", "connectionId": c, "statementId": st, "sql": sql,
                       "maxRowCount": -1, "maxRowsTotal": -1, "maxRowsInFirstFrame": first})
            },
        );
        let resp = self.client.send(body, true).await?;
        let mut builder = BatchBuilder::new();
        let lock_err = || Error::State("destino de lotes".into());
        let mut bytes = resp.len();
        match resp {
            Raw::Proto(b) => {
                let r = ExecuteResponse::decode(b.as_slice()).map_err(crate::decode_err)?;
                drop(b);
                if r.missing_statement {
                    return Err(Error::Query("el servidor perdió la sentencia; reconectá".into()));
                }
                let rs = r.results.into_iter().next().ok_or_else(|| Error::Query("la lectura no devolvió filas".into()))?;
                let cols = merge(proto_cols(rs.signature.unwrap_or_default()));
                sink.lock().map_err(|_| lock_err())?.begin(&transfer_columns(&cols))?;
                let mut frame = rs.first_frame;
                let mut offset = 0u64;
                while let Some(f) = frame.take() {
                    let n = f.rows.len();
                    offset += n as u64;
                    {
                        let mut s = sink.lock().map_err(|_| lock_err())?;
                        for row in f.rows {
                            let cells = row.value.into_iter().zip(&cols).map(|(v, c)| proto_cell(v, c)).collect();
                            builder.push(cells, &mut *s)?;
                        }
                    }
                    if f.done {
                        break;
                    }
                    let resp = self.fetch(rs.statement_id, offset, frame_rows(bound, n, bytes)).await?;
                    bytes = resp.len();
                    frame = match resp {
                        Raw::Proto(b) => {
                            let r = proto::FetchResponse::decode(b.as_slice()).map_err(crate::decode_err)?;
                            drop(b);
                            if r.missing_statement || r.missing_results {
                                return Err(Error::Query("el servidor perdió el resultado; reconectá".into()));
                            }
                            r.frame
                        }
                        Raw::Json(..) => None,
                    };
                }
            }
            Raw::Json(mut v, _) => {
                let mut rs = v.get_mut("results").and_then(Value::as_array_mut).filter(|r| !r.is_empty()).map(|r| r.swap_remove(0)).unwrap_or(Value::Null);
                drop(v);
                let cols = merge(json_cols(&rs));
                sink.lock().map_err(|_| lock_err())?.begin(&transfer_columns(&cols))?;
                let statement = rs.get("statementId").and_then(Value::as_u64).unwrap_or(st as u64) as u32;
                let mut frame = rs.get_mut("firstFrame").map(Value::take).filter(|f| !f.is_null());
                drop(rs);
                let mut offset = 0u64;
                while let Some(mut f) = frame.take() {
                    let rows = match f.get_mut("rows").map(Value::take) {
                        Some(Value::Array(r)) => r,
                        _ => Vec::new(),
                    };
                    let n = rows.len();
                    offset += n as u64;
                    {
                        let mut s = sink.lock().map_err(|_| lock_err())?;
                        for row in rows {
                            let cells = match row {
                                Value::Array(r) => r.iter().zip(&cols).map(|(v, c)| json_cell(v, c)).collect(),
                                _ => Vec::new(),
                            };
                            builder.push(cells, &mut *s)?;
                        }
                    }
                    if f.get("done").and_then(Value::as_bool).unwrap_or(true) {
                        break;
                    }
                    let resp = self.fetch(statement, offset, frame_rows(bound, n, bytes)).await?;
                    bytes = resp.len();
                    if let Raw::Json(mut n, _) = resp {
                        frame = n.get_mut("frame").map(Value::take).filter(|f| !f.is_null());
                    }
                }
            }
        }
        builder.flush(&mut *sink.lock().map_err(|_| lock_err())?)?;
        Ok(builder.rows)
    }

    /// The largest row the read (`from`: its `FROM … WHERE …`) can meet, in
    /// wire bytes: from the column types, and for the columns whose values
    /// have no size limit (`VARCHAR`, `VARBINARY`, arrays) the largest of
    /// them in the rows it reads, measured by the server with one aggregate
    /// query. `None` when nothing tells (one row a frame).
    async fn row_bound(&mut self, cols: &[Col], from: &str) -> Option<u64> {
        let (fixed, probe) = row_bound_parts(cols, !self.generic, self.client.json, from)?;
        let Some(probe) = probe else { return Some(fixed) };
        let v = match self.rows(&probe).await {
            Ok(rows) => rows.first().and_then(|r| r.first()).cloned().unwrap_or(Value::Null),
            Err(e) => {
                tracing::debug!("avatica: no se pudo medir las filas ({e}); se leen de a una");
                return None;
            }
        };
        let data = match &v {
            Value::Null => 0,
            v => v.as_u64().or_else(|| v.as_f64().map(|f| f as u64)).or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))?,
        };
        Some(fixed + data)
    }

    /// Phoenix allows the same column name in two families (`A.V`, `B.V`):
    /// a bare name is then ambiguous in the queries that name the columns
    /// (the `CAST` of a widened `DATE` / `TIME`, the row bound). When a name
    /// repeats in a `SELECT *`, each column gets its family from
    /// `SYSTEM.CATALOG`, in the table's order; if the catalog doesn't match
    /// the columns one by one, the read is refused rather than guessed.
    async fn qualify(&mut self, spec: &ReadSpec, cols: &mut [Col]) -> Result<()> {
        // Asked-for columns name themselves (a repeated one is the same
        // column); generic servers have no families.
        if self.generic || spec.columns.as_ref().is_some_and(|c| !c.is_empty()) {
            return Ok(());
        }
        let mut seen = std::collections::HashSet::new();
        let Some(dup) = cols.iter().map(|c| c.name.clone()).find(|n| !seen.insert(n.clone())) else { return Ok(()) };
        let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
        let schema = match spec.table.schema().filter(|s| !s.is_empty()) {
            Some(s) => format!("TABLE_SCHEM = {}", lit(s)),
            None => "TABLE_SCHEM IS NULL".to_string(),
        };
        let sql = format!(
            "SELECT COLUMN_FAMILY, COLUMN_NAME FROM SYSTEM.CATALOG WHERE TENANT_ID IS NULL AND {schema} AND TABLE_NAME = {} \
             AND COLUMN_NAME IS NOT NULL ORDER BY ORDINAL_POSITION",
            lit(&spec.table.name)
        );
        let rows = self.rows(&sql).await?;
        let text = |v: Option<&Value>| v.and_then(Value::as_str).map(str::to_string);
        let catalog: Vec<(Option<String>, Option<String>)> = rows.iter().map(|r| (text(r.first()), text(r.get(1)))).collect();
        if catalog.len() != cols.len() || catalog.iter().zip(cols.iter()).any(|((_, n), c)| n.as_deref() != Some(c.name.as_str())) {
            return Err(Error::Unsupported(format!(
                "la tabla tiene varias columnas «{dup}» en distintas familias y el catálogo no dice a qué familia corresponde cada una: \
                 leerla por nombre podría confundirlas"
            )));
        }
        for ((family, _), c) in catalog.into_iter().zip(cols.iter_mut()) {
            c.family = family.filter(|f| !f.is_empty());
        }
        Ok(())
    }

    /// The columns `sql` gives (prepared, not run), each Phoenix `DATE` /
    /// `TIME` (and arrays of them) marked to be read as `TIMESTAMP`.
    async fn describe(&self, sql: &str) -> Result<Vec<Col>> {
        let c = &self.client.connection_id;
        let r = self
            .client
            .raw(
                || ("PrepareRequest", PrepareRequest { connection_id: c.clone(), sql: sql.to_string(), max_row_count: u64::MAX, max_rows_total: -1 }.encode_to_vec()),
                || json!({"request": "prepare", "connectionId": c, "sql": sql, "maxRowCount": -1, "maxRowsTotal": -1}),
            )
            .await?;
        let (id, mut cols) = match r {
            Raw::Proto(b) => {
                let h = PrepareResponse::decode(b.as_slice()).map_err(crate::decode_err)?.statement.unwrap_or_default();
                (h.id, proto_cols(h.signature.unwrap_or_default()))
            }
            Raw::Json(v, _) => {
                let id = v.pointer("/statement/id").and_then(Value::as_u64).unwrap_or(0) as u32;
                (id, json_cols(v.get("statement").unwrap_or(&Value::Null)))
            }
        };
        let _ = self.client.close_statement(id).await;
        if !self.generic {
            for c in &mut cols {
                let name = c.type_name.trim().to_ascii_uppercase();
                c.widened = match base_type(name.strip_suffix(" ARRAY").unwrap_or(&name)).0.as_str() {
                    "DATE" => Some(Widened::Date),
                    "TIME" => Some(Widened::Time),
                    _ => None,
                };
            }
        }
        Ok(cols)
    }

    async fn fetch(&self, statement: u32, offset: u64, rows: i32) -> Result<Raw> {
        let c = &self.client.connection_id;
        let body = self.client.body(
            || {
                let r = FetchRequest { connection_id: c.clone(), statement_id: statement, offset, fetch_max_row_count: rows as u32, frame_max_size: rows };
                ("FetchRequest", r.encode_to_vec())
            },
            || json!({"request": "fetch", "connectionId": c, "statementId": statement, "offset": offset, "fetchMaxRowCount": rows, "frameMaxSize": rows}),
        );
        self.client.send(body, true).await
    }

    /// `SELECT COUNT(*)` of a table.
    async fn count_rows(&mut self, table: &str) -> Result<u64> {
        let rows = self.rows(&format!("SELECT COUNT(*) FROM {table}")).await?;
        let v = rows.first().and_then(|r| r.first()).cloned().unwrap_or(Value::Null);
        v.as_u64()
            .or_else(|| v.as_str().and_then(|s| s.trim().parse().ok()))
            .ok_or_else(|| Error::Query(format!("no se pudo contar las filas de {table}: {v}")))
    }

    pub(crate) async fn transfer_load(&mut self, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        // A cancelled load waits in `Drop` for the request the server already
        // has (a commit may land); only a multi-threaded runtime can wait there.
        if !multi_thread() {
            return Err(Error::Unsupported(
                "la carga masiva por Avatica necesita un runtime de tokio multi-hilo: en uno de un solo hilo, una carga cancelada \
                 no podría esperar el commit que el servidor ya recibió y ese commit podría llegar después"
                    .into(),
            ));
        }
        let table = qualified_name(Quote::Double, spec.table.schema(), &spec.table.name);
        let cols = spec.columns.iter().map(|c| quote_ident(Quote::Double, c)).collect::<Vec<_>>().join(", ");
        let marks = vec!["?"; spec.columns.len()].join(", ");
        // Phoenix: a load never overwrites a row. Into an empty table a plain
        // UPSERT does (a key repeated in the source can only overwrite a row of
        // this same load, and the count at the end fails it); into a table
        // with rows, ON DUPLICATE KEY IGNORE leaves the existing ones alone
        // (and the count tells). It is 2 to 3 times slower, hence only there.
        let before = if self.generic {
            None
        } else if self.rows(&format!("SELECT 1 FROM {table} LIMIT 1")).await?.is_empty() {
            Some(0)
        } else {
            Some(self.count_rows(&table).await?)
        };
        let sql = match before {
            None => format!("INSERT INTO {table} ({cols}) VALUES ({marks})"),
            Some(0) => format!("UPSERT INTO {table} ({cols}) VALUES ({marks})"),
            Some(_) => format!("UPSERT INTO {table} ({cols}) VALUES ({marks}) ON DUPLICATE KEY IGNORE"),
        };
        let (statement, mut params, signature) = match self.prepare(&sql).await {
            Err(Error::Query(e)) if matches!(before, Some(n) if n > 0) => {
                return Err(Error::Unsupported(format!(
                    "{table} ya tiene filas, y Phoenix no permite cargarla sin pisar las de igual clave ({e}); vaciala antes de copiar"
                )))
            }
            r => r?,
        };
        if params.len() != spec.columns.len() {
            tracing::debug!("avatica: {} parámetros para {} columnas; se tipan por el valor", params.len(), spec.columns.len());
        }
        let phoenix = !self.generic;
        for p in &mut params {
            p.full_date = phoenix;
        }
        if let Err(e) = self.client.set_auto_commit(false).await {
            let _ = self.client.close_statement(statement).await;
            return Err(e);
        }
        let mut load = Load { client: self.client.detached(), statement, signature, progress, inflight: None, armed: true, json: self.client.json };
        let r = load_batches(&mut load, &params, spec, source).await;
        let loaded = load.finish(r).await?;
        if let Some(before) = before {
            let after = self.count_rows(&table).await?;
            let added = after.saturating_sub(before);
            if added != loaded {
                return Err(Error::Query(format!(
                    "se enviaron {loaded} filas pero la tabla tiene {added} filas nuevas: {} filas tienen una clave que ya estaba en la tabla o se repite en el origen, \
                     y Phoenix no pisa filas existentes en una carga (esas filas no se cargaron)",
                    loaded.abs_diff(added)
                )));
            }
        }
        Ok(loaded)
    }

    /// `prepare`: the statement id, its parameters' types and (protobuf) its
    /// signature as the server sent it.
    async fn prepare(&self, sql: &str) -> Result<(u32, Vec<Param>, Vec<u8>)> {
        let c = &self.client.connection_id;
        let r = self
            .client
            .raw(
                || ("PrepareRequest", PrepareRequest { connection_id: c.clone(), sql: sql.to_string(), max_row_count: u64::MAX, max_rows_total: -1 }.encode_to_vec()),
                || json!({"request": "prepare", "connectionId": c, "sql": sql, "maxRowCount": -1, "maxRowsTotal": -1}),
            )
            .await?;
        Ok(match r {
            Raw::Proto(b) => {
                let h = PrepareResponse::decode(b.as_slice()).map_err(crate::decode_err)?.statement.unwrap_or_default();
                let raw = RawPrepareResponse::decode(b.as_slice()).map_err(crate::decode_err)?.statement.unwrap_or_default();
                (h.id, h.signature.unwrap_or_default().parameters.iter().map(|p| Param::of(p.parameter_type as i32, &p.type_name)).collect(), raw.signature)
            }
            Raw::Json(v, _) => {
                let id = v.pointer("/statement/id").and_then(Value::as_u64).unwrap_or(0) as u32;
                let params = v
                    .pointer("/statement/signature/parameters")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|p| {
                        Param::of(
                            p.get("parameterType").and_then(Value::as_i64).unwrap_or(0) as i32,
                            p.get("typeName").and_then(Value::as_str).unwrap_or_default(),
                        )
                    })
                    .collect();
                (id, params, Vec::new())
            }
        })
    }
}

/// A load in progress on the server. Each request runs in its own task, so
/// that dropping the load (a cancel) doesn't forget one the server already
/// has: `Drop` waits for it, reports the commit if that's what landed, and
/// rolls back what's left uncommitted, all before the load returns.
struct Load<'a> {
    client: Client,
    statement: u32,
    /// The statement's signature (protobuf), for `execute`.
    signature: Vec<u8>,
    progress: Progress<'a>,
    /// The request on the server, and the rows committed once it's done if
    /// it's a commit.
    inflight: Option<(JoinHandle<Result<Raw>>, Option<u64>)>,
    /// Auto-commit is off and the statement open (not finished yet).
    armed: bool,
    json: bool,
}

impl Load<'_> {
    /// Run one request and wait for it; `commits`: the rows committed when
    /// it's done (a commit).
    async fn call(&mut self, body: Body, commits: Option<u64>) -> Result<Raw> {
        let client = self.client.detached();
        let task = tokio::spawn(async move { client.send(body, false).await });
        let (task, _) = self.inflight.insert((task, commits));
        let r = task.await;
        self.inflight = None;
        r.map_err(|e| Error::Query(format!("la carga se interrumpió: {e}")))?
    }

    async fn execute_batch(&mut self, rows: &[Vec<PValue>]) -> Result<()> {
        let (c, statement) = (self.client.connection_id.clone(), self.statement);
        let body = self.client.body(
            || {
                let updates = rows.iter().map(|r| UpdateBatch { parameter_values: r.iter().map(PValue::proto).collect() }).collect();
                ("ExecuteBatchRequest", ExecuteBatchRequest { connection_id: c.clone(), statement_id: statement, updates }.encode_to_vec())
            },
            || {
                let values: Vec<Value> = rows.iter().map(|r| Value::Array(r.iter().map(PValue::json).collect())).collect();
                json!({"request": "executeBatch", "connectionId": c, "statementId": statement, "parameterValues": values})
            },
        );
        if let Raw::Proto(bytes) = self.call(body, None).await? {
            let r = ExecuteBatchResponse::decode(bytes.as_slice()).map_err(crate::decode_err)?;
            if r.missing_statement {
                return Err(Error::Query("el servidor perdió la sentencia; reconectá".into()));
            }
        }
        Ok(())
    }

    /// Rows with arrays, one `execute` each: Avatica turns an array
    /// parameter into a JDBC array only with the statement's signature,
    /// which `executeBatch` doesn't carry.
    async fn execute_rows(&mut self, rows: &[Vec<PValue>]) -> Result<()> {
        for r in rows {
            let handle = RawHandle { connection_id: self.client.connection_id.clone(), id: self.statement, signature: self.signature.clone() };
            let req = ExecuteRequest {
                statement_handle: Some(handle),
                parameter_values: r.iter().map(PValue::proto).collect(),
                has_parameter_values: true,
                first_frame_max_size: 0,
            };
            self.call(Body::Proto("ExecuteRequest", req.encode_to_vec()), None).await?;
        }
        Ok(())
    }

    /// Send rows: in one `executeBatch`, or row by row when they have arrays.
    async fn send_rows(&mut self, rows: &[Vec<PValue>]) -> Result<()> {
        if rows.iter().flatten().any(|v| matches!(v, PValue::Array(..))) {
            self.execute_rows(rows).await
        } else {
            self.execute_batch(rows).await
        }
    }

    /// Commit the window: `done` rows are committed after it.
    async fn commit(&mut self, done: u64) -> Result<()> {
        let body = self.client.end_body(true);
        self.call(body, Some(done)).await?;
        (self.progress)(done);
        Ok(())
    }

    /// Roll back a failed load, turn auto-commit back on, close the statement.
    async fn finish(mut self, r: Result<u64>) -> Result<u64> {
        if r.is_err() {
            let _ = self.client.end(false).await;
        }
        let _ = self.client.set_auto_commit(true).await;
        let _ = self.client.close_statement(self.statement).await;
        self.armed = false;
        r
    }
}

impl Drop for Load<'_> {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        let Ok(h) = Handle::try_current() else { return };
        let inflight = self.inflight.take();
        let client = self.client.detached();
        let statement = self.statement;
        let cleanup = async move {
            let mut committed = None;
            if let Some((task, commits)) = inflight {
                if let (Ok(Ok(_)), Some(n)) = (task.await, commits) {
                    committed = Some(n);
                }
            }
            let _ = client.end(false).await;
            let _ = client.set_auto_commit(true).await;
            let _ = client.close_statement(statement).await;
            committed
        };
        // Always multi-threaded: `transfer_load` refuses to start otherwise.
        if h.runtime_flavor() == RuntimeFlavor::MultiThread {
            // Off the worker, so the request keeps going while this waits.
            if let Some(n) = tokio::task::block_in_place(|| h.block_on(cleanup)) {
                (self.progress)(n);
            }
        }
    }
}

/// Running on a multi-threaded tokio runtime.
fn multi_thread() -> bool {
    Handle::try_current().is_ok_and(|h| h.runtime_flavor() == RuntimeFlavor::MultiThread)
}

/// Send the source's rows in commit windows.
async fn load_batches(load: &mut Load<'_>, params: &[Param], spec: &LoadSpec, source: &mut dyn BatchSource) -> Result<u64> {
    let every_rows = spec.commit_rows.clamp(1, MAX_WINDOW_ROWS);
    let every_bytes = spec.commit_bytes.clamp(1, MAX_WINDOW_BYTES);
    // Committed rows, and the window's rows and bytes (sent or in `chunk`).
    let (mut done, mut window_rows, mut window_bytes) = (0u64, 0u64, 0u64);
    let mut chunk: Vec<Vec<PValue>> = Vec::new();
    while let Some(b) = source.next().await {
        for row in &b.rows {
            if row.len() != spec.columns.len() {
                return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), spec.columns.len())));
            }
            let mut values = Vec::with_capacity(row.len());
            for (i, cell) in row.iter().enumerate() {
                let p = params.get(i).copied().unwrap_or_default();
                let v = param_value(cell, p).map_err(|e| match e {
                    Error::Unsupported(m) => Error::Unsupported(format!("columna {}: {m}", spec.columns[i])),
                    e => Error::Query(format!("columna {}: {e}", spec.columns[i])),
                })?;
                if load.json && matches!(v, PValue::Array(..)) {
                    return Err(Error::Unsupported(format!(
                        "columna {}: en serialización JSON, Avatica no acepta arreglos como parámetros; conectá con serialización protobuf",
                        spec.columns[i]
                    )));
                }
                values.push(v);
            }
            let size = mutation_size(&values);
            if window_rows > 0 && (window_rows + 1 > every_rows || window_bytes + size > every_bytes) {
                if !chunk.is_empty() {
                    load.send_rows(&chunk).await?;
                    chunk.clear();
                }
                done += window_rows;
                load.commit(done).await?;
                (window_rows, window_bytes) = (0, 0);
            }
            chunk.push(values);
            window_rows += 1;
            window_bytes += size;
        }
        if !chunk.is_empty() {
            load.send_rows(&chunk).await?;
            chunk.clear();
        }
    }
    if window_rows > 0 {
        done += window_rows;
        load.commit(done).await?;
    }
    Ok(done)
}

/// Bytes a row takes in Phoenix's mutation buffer, on the safe side of how
/// Phoenix counts them (measured on Phoenix 5.0: a row of 11 BIGINT columns
/// takes about 4 KB there, so its 100 MB fill at ~26,000 such rows): a few
/// hundred bytes of bookkeeping per row and per column, plus the value.
fn mutation_size(values: &[PValue]) -> u64 {
    400 + values.iter().map(|v| 400 + 2 * v.size()).sum::<u64>()
}

fn transfer_columns(cols: &[Col]) -> Vec<TransferColumn> {
    cols.iter().map(|c| TransferColumn { name: c.name.clone(), type_name: c.type_name.clone(), nullable: c.nullable }).collect()
}

fn type_text(name: &str, precision: u32, scale: u32) -> String {
    match name {
        "DECIMAL" | "UNSIGNED_DECIMAL" if precision > 0 => format!("{name}({precision},{scale})"),
        "VARCHAR" | "CHAR" | "BINARY" | "VARBINARY" if precision > 0 => format!("{name}({precision})"),
        _ => name.to_string(),
    }
}

fn proto_cols(sig: Signature) -> Vec<Col> {
    sig.columns
        .into_iter()
        .map(|c| {
            let t = c.r#type.unwrap_or_default();
            Col {
                name: if c.label.is_empty() { c.column_name } else { c.label },
                rep: crate::temporal_rep(t.rep, &t.name).to_string(),
                type_name: type_text(&t.name, c.precision, c.scale),
                // java.sql.ResultSetMetaData.columnNoNulls = 0
                nullable: c.nullable != 0,
                widened: None,
                family: None,
            }
        })
        .collect()
}

fn json_cols(rs: &Value) -> Vec<Col> {
    rs.pointer("/signature/columns")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| {
            let name = c.pointer("/type/name").and_then(Value::as_str).unwrap_or_default();
            let n = |k: &str| c.get(k).and_then(Value::as_u64).unwrap_or(0) as u32;
            let rep = c.pointer("/type/rep").and_then(Value::as_str).unwrap_or_default();
            // Temporal columns may come with a plain number `Rep`: the type says which.
            let temporal = crate::temporal_rep(-1, name);
            Col {
                name: c.get("label").or(c.get("columnName")).and_then(Value::as_str).unwrap_or_default().to_string(),
                type_name: type_text(name, n("precision"), n("scale")),
                nullable: n("nullable") != 0,
                rep: if temporal.is_empty() { rep.to_string() } else { temporal.to_string() },
                widened: None,
                family: None,
            }
        })
        .collect()
}

// ---- Avatica values → cells ----

/// `1E+2`, `1.5E-3` (Java's `BigDecimal.toString`) as plain digits.
pub(crate) fn plain_decimal(s: &str) -> String {
    let s = s.trim();
    let Some(e) = s.find(['E', 'e']) else { return s.to_string() };
    let (mant, exp) = (&s[..e], s[e + 1..].parse::<i64>().unwrap_or(0));
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant.strip_prefix('+').unwrap_or(mant)),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = format!("{int}{frac}");
    let point = int.len() as i64 + exp; // position of the point in `digits`
    let mut out = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    let t = out.trim_start_matches('0');
    out = if t.is_empty() || t.starts_with('.') { format!("0{t}") } else { t.to_string() };
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

fn days_date(days: i64) -> Cell {
    Cell::Date(crate::date_from_days(days))
}

fn millis_time(ms: i64) -> Cell {
    let ms = ms.rem_euclid(86_400_000);
    let s = ms / 1000;
    let base = format!("{:02}:{:02}:{:02}", s / 3600, s / 60 % 60, s % 60);
    Cell::Time(if ms % 1000 == 0 { base } else { format!("{base}.{:03}", ms % 1000) })
}

fn millis_timestamp(ms: i64) -> Cell {
    Cell::DateTime(crate::timestamp_from_millis(ms))
}

/// A number in a temporal column (the `Rep` the column type implies).
fn temporal(n: i64, rep: &str) -> Option<Cell> {
    Some(match rep {
        "JAVA_SQL_DATE" => days_date(n),
        "JAVA_SQL_TIME" => millis_time(n),
        "JAVA_SQL_TIMESTAMP" | "JAVA_UTIL_DATE" => millis_timestamp(n),
        _ => return None,
    })
}

/// A widened `DATE` / `TIME` back to a date or a time when that loses nothing.
fn narrow(c: Cell, col: &Col) -> Cell {
    match (col.widened, c) {
        (Some(Widened::Date), Cell::DateTime(s)) if s.len() == 19 && s.ends_with(" 00:00:00") => Cell::Date(s[..10].to_string()),
        (Some(Widened::Time), Cell::DateTime(s)) if s.starts_with("1970-01-01 ") => Cell::Time(s[11..].to_string()),
        (_, c) => c,
    }
}

fn proto_value(v: proto::TypedValue, col: &Col) -> Cell {
    use proto::rep;
    if v.null || v.r#type == rep::NULL {
        return Cell::Null;
    }
    let c = match v.r#type {
        rep::BOOLEAN | rep::PRIMITIVE_BOOLEAN => Cell::Bool(v.bool_value),
        rep::PRIMITIVE_FLOAT | rep::FLOAT => Cell::Float(single(v.double_value)),
        // A FLOAT column may come as DOUBLE: its value is still a single.
        rep::DOUBLE | rep::PRIMITIVE_DOUBLE if is_single(col) => Cell::Float(single(v.double_value)),
        rep::DOUBLE | rep::PRIMITIVE_DOUBLE => Cell::Float(v.double_value),
        rep::BIG_DECIMAL | rep::BIG_INTEGER => Cell::Decimal(plain_decimal(&v.string_value)),
        rep::BYTE_STRING => Cell::Bytes(v.bytes_value),
        rep::STRING | CHARACTER => Cell::Text(v.string_value),
        rep::JAVA_SQL_DATE => days_date(v.number_value),
        rep::JAVA_SQL_TIME => millis_time(v.number_value),
        rep::JAVA_SQL_TIMESTAMP | rep::JAVA_UTIL_DATE => millis_timestamp(v.number_value),
        rep::ARRAY => array_cell(v.array_value, col),
        _ => temporal(v.number_value, &col.rep).unwrap_or(Cell::Int(v.number_value)),
    };
    narrow(c, col)
}

/// A single-precision value widened to `f64`, by its shortest digits
/// (`1.1`, not `1.100000023841858`).
fn single(f: f64) -> f64 {
    (f as f32).to_string().parse().unwrap_or(f)
}

fn is_single(col: &Col) -> bool {
    matches!(col.type_name.trim_start_matches("UNSIGNED_"), "FLOAT" | "REAL")
}

/// An array as JSON; the elements of a widened `DATE` / `TIME` array are
/// narrowed like a column of that type.
fn array_cell(items: Vec<proto::TypedValue>, col: &Col) -> Cell {
    let elem = Col { name: String::new(), type_name: String::new(), nullable: true, rep: String::new(), widened: col.widened, family: None };
    Cell::Json(Value::Array(items.into_iter().map(|x| proto_value(x, &elem).to_json()).collect()).to_string())
}

const CHARACTER: i32 = 10;

fn proto_cell(v: proto::ColumnValue, col: &Col) -> Cell {
    if v.has_array_value {
        return array_cell(v.array_value, col);
    }
    match v.scalar_value.or(v.value.into_iter().next()) {
        Some(x) => proto_value(x, col),
        None => Cell::Null,
    }
}

/// A JSON number (its exact text) in column `col`.
fn json_number(text: &str, col: &Col, t: &str) -> Cell {
    if let Some(c) = text.parse::<i64>().ok().and_then(|i| temporal(i, &col.rep)) {
        return narrow(c, col);
    }
    if col.rep == "BIG_DECIMAL" || matches!(t, "DECIMAL" | "NUMERIC") {
        return Cell::Decimal(plain_decimal(text));
    }
    if let Ok(i) = text.parse::<i64>() {
        return Cell::Int(i);
    }
    if let Ok(u) = text.parse::<u64>() {
        return Cell::UInt(u);
    }
    if !text.contains(['.', 'e', 'E']) {
        // An integer too big for 64 bits (a BIGINT-like type of the backend).
        return Cell::Decimal(plain_decimal(text));
    }
    let f: f64 = text.parse().unwrap_or(f64::NAN);
    if matches!(col.rep.as_str(), "FLOAT" | "PRIMITIVE_FLOAT") || matches!(t, "FLOAT" | "REAL") {
        return Cell::Float(single(f));
    }
    Cell::Float(f)
}

fn json_cell(v: &Value, col: &Col) -> Cell {
    let t = col.type_name.split('(').next().unwrap_or_default().trim_start_matches("UNSIGNED_");
    if let Some(text) = exact_text(v) {
        return json_number(text, col, t);
    }
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => json_number(&n.to_string(), col, t),
        Value::String(s) if col.rep == "BYTE_STRING" || matches!(t, "BINARY" | "VARBINARY") => {
            base64::engine::general_purpose::STANDARD.decode(s).map(Cell::Bytes).unwrap_or_else(|_| Cell::Text(s.clone()))
        }
        Value::String(s) if col.rep == "BIG_DECIMAL" || matches!(t, "DECIMAL" | "NUMERIC") => Cell::Decimal(plain_decimal(s)),
        Value::String(s) => Cell::Text(s.clone()),
        Value::Array(items) if temporal_elements(col).is_some() => {
            let rep = temporal_elements(col).unwrap_or_default();
            let elem = Col { name: String::new(), type_name: String::new(), nullable: true, rep: rep.into(), widened: col.widened, family: None };
            let items = items.iter().map(|x| match x.as_i64() {
                Some(n) => temporal(n, rep).map(|c| narrow(c, &elem).to_json()).unwrap_or_else(|| x.clone()),
                None => x.clone(),
            });
            Cell::Json(Value::Array(items.collect()).to_string())
        }
        v => {
            let mut s = String::new();
            json_text(v, &mut s);
            Cell::Json(s)
        }
    }
}

/// The `Rep` of the elements of an array of dates or times (as read: a
/// widened one comes as `TIMESTAMP`).
fn temporal_elements(col: &Col) -> Option<&'static str> {
    let name = col.type_name.trim().to_ascii_uppercase();
    let elem = name.strip_suffix(" ARRAY")?;
    if col.widened.is_some() {
        return Some("JAVA_SQL_TIMESTAMP");
    }
    match base_type(elem).0.as_str() {
        "DATE" => Some("JAVA_SQL_DATE"),
        "TIME" => Some("JAVA_SQL_TIME"),
        "TIMESTAMP" => Some("JAVA_SQL_TIMESTAMP"),
        _ => None,
    }
}

// ---- cells → Avatica parameters ----

/// A parameter's type: its JDBC type (`java.sql.Types`), an array's
/// element type, and whether `DATE` / `TIME` hold a date and a time
/// (Phoenix) or only one of them.
#[derive(Debug, Clone, Copy, Default, PartialEq)]
pub(crate) struct Param {
    t: i32,
    elem: i32,
    full_date: bool,
}

/// JDBC's `ARRAY`, and Phoenix's arrays (3000 + the element's type).
const ARRAY: i32 = 2003;
const PHOENIX_ARRAY_BASE: i32 = 3000;

/// A parameter value: its `Rep` and the value.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum PValue {
    Null,
    Bool(bool),
    /// `Rep` (BYTE, SHORT, INTEGER, LONG, dates and times) and the number.
    Num(i32, &'static str, i64),
    /// DOUBLE.
    Double(i32, &'static str, f64),
    /// STRING or BIG_DECIMAL.
    Str(i32, &'static str, String),
    Bytes(Vec<u8>),
    /// An array: its elements' `Rep` and the elements.
    Array(i32, Vec<PValue>),
}

impl Param {
    fn of(jdbc: i32, type_name: &str) -> Param {
        let name = type_name.trim().to_ascii_uppercase();
        if let Some(elem) = name.strip_suffix(" ARRAY").or_else(|| name.strip_suffix("[]")) {
            return Param { t: ARRAY, elem: Param::of(0, elem).t, full_date: false };
        }
        if jdbc > PHOENIX_ARRAY_BASE && jdbc < PHOENIX_ARRAY_BASE + 100 {
            return Param { t: ARRAY, elem: Param::of(jdbc - PHOENIX_ARRAY_BASE, "").t, full_date: false };
        }
        let t = match jdbc {
            // Phoenix's own codes for its UNSIGNED_* types.
            9 => 4,
            10 => -5,
            11 => -6,
            13 => 5,
            14 => 6,
            15 => 8,
            18 => 92,
            19 => 91,
            20 => 93,
            0 => by_name(&name),
            j => j,
        };
        Param { t, elem: 0, full_date: false }
    }
}

/// Servers that give only the type's name.
fn by_name(name: &str) -> i32 {
    match name.trim_start_matches("UNSIGNED_").split('(').next().unwrap_or_default().trim() {
        "TINYINT" => -6,
        "SMALLINT" => 5,
        "INTEGER" | "INT" => 4,
        "BIGINT" | "LONG" => -5,
        "FLOAT" | "REAL" => 6,
        "DOUBLE" => 8,
        "DECIMAL" | "NUMERIC" => 3,
        "VARCHAR" | "CHAR" => 12,
        "DATE" => 91,
        "TIME" => 92,
        "TIMESTAMP" => 93,
        "BINARY" | "VARBINARY" => -3,
        "BOOLEAN" => 16,
        _ => 0,
    }
}

impl PValue {
    fn rep(&self) -> i32 {
        match self {
            PValue::Null => proto::rep::NULL,
            PValue::Bool(_) => proto::rep::BOOLEAN,
            PValue::Num(r, ..) | PValue::Double(r, ..) | PValue::Str(r, ..) => *r,
            PValue::Bytes(_) => proto::rep::BYTE_STRING,
            PValue::Array(..) => proto::rep::ARRAY,
        }
    }

    /// Bytes of the value.
    fn size(&self) -> u64 {
        match self {
            PValue::Null => 0,
            PValue::Bool(_) => 1,
            PValue::Num(..) | PValue::Double(..) => 8,
            PValue::Str(_, _, s) => s.len() as u64,
            PValue::Bytes(b) => b.len() as u64,
            PValue::Array(_, a) => a.iter().map(|x| 8 + x.size()).sum(),
        }
    }

    fn proto(&self) -> TypedParam {
        let mut t = TypedParam { r#type: self.rep(), ..Default::default() };
        match self {
            PValue::Null => t.null = true,
            PValue::Bool(b) => t.bool_value = *b,
            PValue::Num(_, _, n) => t.number_value = *n,
            PValue::Double(_, _, f) => t.double_value = *f,
            PValue::Str(_, _, s) => t.string_value = s.clone(),
            PValue::Bytes(b) => t.bytes_value = b.clone(),
            PValue::Array(rep, items) => {
                t.component_type = *rep;
                t.array_value = items.iter().map(PValue::proto).collect();
            }
        }
        t
    }

    fn json(&self) -> Value {
        match self {
            PValue::Null => json!({"type": "NULL", "value": null}),
            PValue::Bool(b) => json!({"type": "BOOLEAN", "value": b}),
            PValue::Num(_, name, n) => json!({"type": name, "value": n}),
            PValue::Double(_, name, f) => json!({"type": name, "value": f}),
            PValue::Str(_, name, s) => json!({"type": name, "value": s}),
            PValue::Bytes(b) => json!({"type": "BYTE_STRING", "value": base64::engine::general_purpose::STANDARD.encode(b)}),
            // Not sent: JSON serialization has no array parameters (see `load_batches`).
            PValue::Array(_, items) => json!({"type": "ARRAY", "value": items.iter().map(PValue::json).collect::<Vec<_>>()}),
        }
    }
}

const BYTE: i32 = 9;
const SHORT: i32 = 11;
const INTEGER: i32 = 12;
const LONG: i32 = 13;

fn text_of(c: &Cell) -> String {
    match c {
        Cell::Null => String::new(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => s.clone(),
    }
}

fn bad(m: String) -> Error {
    Error::Query(m)
}

fn int_of(c: &Cell) -> Result<i64> {
    match c {
        Cell::Int(i) => Ok(*i),
        Cell::UInt(u) => i64::try_from(*u).map_err(|_| bad(format!("{u} no entra en un entero de 64 bits"))),
        Cell::Bool(b) => Ok(*b as i64),
        Cell::Float(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Ok(*f as i64),
        other => {
            let t = text_of(other);
            let t = t.trim();
            t.parse().or_else(|_| t.strip_suffix(".0").unwrap_or(t).parse()).map_err(|_| bad(format!("«{t}» no es un entero")))
        }
    }
}

fn parse_datetime(s: &str) -> Result<NaiveDateTime> {
    let s = s.trim();
    if let Ok(t) = chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%:z").or_else(|_| chrono::DateTime::parse_from_rfc3339(s)) {
        return Ok(t.naive_utc());
    }
    NaiveDateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f")
        .or_else(|_| NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f"))
        .or_else(|_| NaiveDate::parse_from_str(s, "%Y-%m-%d").map(|d| d.and_time(NaiveTime::MIN)))
        .map_err(|_| bad(format!("«{s}» no es una fecha y hora")))
}

/// Milliseconds of a time value; a finer fraction would be cut.
fn millis_only(nanos: u32, s: &str) -> Result<()> {
    if !nanos.is_multiple_of(1_000_000) {
        return Err(Error::Unsupported(format!(
            "«{}» tiene una fracción de segundo más fina que el milisegundo, y Avatica transporta fechas y horas en milisegundos: se perdería",
            s.trim()
        )));
    }
    Ok(())
}

/// Milliseconds since the epoch of a date and time value.
fn epoch_millis(c: &Cell) -> Result<i64> {
    let s = text_of(c);
    let t = parse_datetime(&s)?;
    millis_only(t.nanosecond(), &s)?;
    Ok(t.and_utc().timestamp_millis())
}

fn timestamp_param(c: &Cell) -> Result<PValue> {
    Ok(PValue::Num(proto::rep::JAVA_SQL_TIMESTAMP, "JAVA_SQL_TIMESTAMP", epoch_millis(c)?))
}

/// An array's element from its JSON (as `Cell::to_json` writes it).
fn element(v: &Value, elem: i32, full_date: bool) -> Result<PValue> {
    let c = match (v, elem) {
        (Value::String(s), -2 | -3 | -4 | 2004) if s.starts_with("0x") || s.starts_with("0X") => {
            let hex = &s[2..];
            let bytes = (0..hex.len())
                .step_by(2)
                .map(|i| hex.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok()))
                .collect::<Option<Vec<u8>>>()
                .ok_or_else(|| bad(format!("«{s}» no es un binario en hexadecimal")))?;
            Cell::Bytes(bytes)
        }
        (Value::Array(_) | Value::Object(_), _) => return Err(bad(format!("el arreglo tiene un elemento que no es un valor simple: {v}"))),
        (v, _) => Cell::from_json(v),
    };
    param_value(&c, Param { t: elem, elem: 0, full_date })
}

/// A cell as the parameter `p` expects it.
pub(crate) fn param_value(c: &Cell, p: Param) -> Result<PValue> {
    if *c == Cell::Null {
        return Ok(PValue::Null);
    }
    Ok(match p.t {
        -6 => PValue::Num(BYTE, "BYTE", int_of(c)?),
        5 => PValue::Num(SHORT, "SHORT", int_of(c)?),
        4 => PValue::Num(INTEGER, "INTEGER", int_of(c)?),
        -5 => PValue::Num(LONG, "LONG", int_of(c)?),
        6..=8 => {
            let f = match c {
                Cell::Float(f) => *f,
                Cell::Int(i) => *i as f64,
                Cell::UInt(u) => *u as f64,
                other => text_of(other).trim().parse().map_err(|_| bad(format!("«{}» no es un número", text_of(other))))?,
            };
            if p.t != 8 && f.is_finite() && f.abs() > f32::MAX as f64 {
                return Err(bad(format!("{f} no entra en un FLOAT")));
            }
            // FLOAT goes as DOUBLE too: Avatica's FLOAT reaches Phoenix as 0.
            PValue::Double(proto::rep::DOUBLE, "DOUBLE", f)
        }
        2 | 3 => PValue::Str(proto::rep::BIG_DECIMAL, "BIG_DECIMAL", text_of(c)),
        16 | -7 => match c {
            Cell::Bool(b) => PValue::Bool(*b),
            other => match text_of(other).trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "t" => PValue::Bool(true),
                "0" | "false" | "f" => PValue::Bool(false),
                t => return Err(bad(format!("«{t}» no es un booleano"))),
            },
        },
        91 => {
            let s = text_of(c);
            let t = parse_datetime(&s)?;
            if t.time() == NaiveTime::MIN {
                let days = t.date().signed_duration_since(NaiveDate::from_ymd_opt(1970, 1, 1).unwrap_or_default()).num_days();
                PValue::Num(proto::rep::JAVA_SQL_DATE, "JAVA_SQL_DATE", days)
            } else if p.full_date {
                // Phoenix's DATE keeps the time to the millisecond.
                timestamp_param(c)?
            } else {
                return Err(bad(format!("«{}» tiene hora y la columna es DATE: se perdería", s.trim())));
            }
        }
        92 => {
            let s = text_of(c);
            match NaiveTime::parse_from_str(s.trim(), "%H:%M:%S%.f") {
                Ok(t) => {
                    millis_only(t.nanosecond(), &s)?;
                    let ms = t.num_seconds_from_midnight() as i64 * 1000 + (t.nanosecond() / 1_000_000) as i64;
                    PValue::Num(proto::rep::JAVA_SQL_TIME, "JAVA_SQL_TIME", ms)
                }
                // Phoenix's TIME keeps a date too.
                Err(_) if p.full_date && parse_datetime(&s).is_ok() => timestamp_param(c)?,
                Err(_) => return Err(bad(format!("«{s}» no es una hora"))),
            }
        }
        93 | 2014 => timestamp_param(c)?,
        -2 | -3 | -4 | 2004 => match c {
            Cell::Bytes(b) => PValue::Bytes(b.clone()),
            other => PValue::Bytes(text_of(other).into_bytes()),
        },
        1 | 12 | -1 | -9 | -15 | -16 | 2005 => PValue::Str(proto::rep::STRING, "STRING", text_of(c)),
        ARRAY => {
            let s = match c {
                Cell::Json(s) | Cell::Text(s) => s,
                other => return Err(bad(format!("«{}» no es un arreglo", text_of(other)))),
            };
            let items = match serde_json::from_str::<Value>(s) {
                Ok(Value::Array(a)) => a,
                _ => return Err(bad(format!("«{s}» no es un arreglo JSON"))),
            };
            let items = items.iter().map(|v| element(v, p.elem, p.full_date)).collect::<Result<Vec<_>>>()?;
            PValue::Array(component_rep(p.elem), items)
        }
        // Unknown type: by the value.
        _ => match c {
            Cell::Bool(b) => PValue::Bool(*b),
            Cell::Int(i) => PValue::Num(LONG, "LONG", *i),
            Cell::UInt(u) => match i64::try_from(*u) {
                Ok(i) => PValue::Num(LONG, "LONG", i),
                Err(_) => PValue::Str(proto::rep::BIG_DECIMAL, "BIG_DECIMAL", u.to_string()),
            },
            Cell::Float(f) => PValue::Double(proto::rep::DOUBLE, "DOUBLE", *f),
            Cell::Decimal(s) => PValue::Str(proto::rep::BIG_DECIMAL, "BIG_DECIMAL", s.clone()),
            Cell::Bytes(b) => PValue::Bytes(b.clone()),
            Cell::DateTime(_) | Cell::DateTimeTz(_) => timestamp_param(c)?,
            other => PValue::Str(proto::rep::STRING, "STRING", text_of(other)),
        },
    })
}

/// The `Rep` of an array's elements of JDBC type `elem`.
fn component_rep(elem: i32) -> i32 {
    use proto::rep;
    match elem {
        -6 => BYTE,
        5 => SHORT,
        4 => INTEGER,
        -5 => LONG,
        6..=8 => rep::DOUBLE,
        2 | 3 => rep::BIG_DECIMAL,
        16 | -7 => rep::BOOLEAN,
        91 => rep::JAVA_SQL_DATE,
        92 => rep::JAVA_SQL_TIME,
        93 | 2014 => rep::JAVA_SQL_TIMESTAMP,
        -2 | -3 | -4 | 2004 => rep::BYTE_STRING,
        _ => rep::STRING,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn col(type_name: &str, rep: &str) -> Col {
        Col { name: "c".into(), type_name: type_name.into(), nullable: true, rep: rep.into(), widened: None, family: None }
    }

    fn p(t: i32) -> Param {
        Param { t, elem: 0, full_date: false }
    }

    #[test]
    fn decimals_without_exponent() {
        assert_eq!(plain_decimal("1E+2"), "100");
        assert_eq!(plain_decimal("1.5E-3"), "0.0015");
        assert_eq!(plain_decimal("-12.345E1"), "-123.45");
        assert_eq!(plain_decimal("123.45"), "123.45");
        assert_eq!(plain_decimal("0E-10"), "0.0000000000");
    }

    #[test]
    fn avatica_values_to_cells() {
        use proto::rep;
        let tv = |t: i32| proto::TypedValue { r#type: t, ..Default::default() };
        let c = col("BIGINT", "");
        assert_eq!(proto_value(proto::TypedValue { number_value: 9007199254740993, ..tv(LONG) }, &c), Cell::Int(9007199254740993));
        assert_eq!(proto_value(proto::TypedValue { null: true, ..tv(rep::STRING) }, &c), Cell::Null);
        assert_eq!(proto_value(proto::TypedValue { string_value: "1E+2".into(), ..tv(rep::BIG_DECIMAL) }, &c), Cell::Decimal("100".into()));
        let big = vec![7u8; 4096];
        assert_eq!(proto_value(proto::TypedValue { bytes_value: big.clone(), ..tv(rep::BYTE_STRING) }, &c), Cell::Bytes(big));
        // Phoenix sends a TIMESTAMP column as a number with the column's type.
        let ts = col("TIMESTAMP", "JAVA_SQL_TIMESTAMP");
        assert_eq!(proto_value(proto::TypedValue { number_value: 1_706_708_700_123, ..tv(LONG) }, &ts), Cell::DateTime("2024-01-31 13:45:00.123".into()));
        assert_eq!(proto_value(proto::TypedValue { number_value: 19753, ..tv(rep::JAVA_SQL_DATE) }, &c), Cell::Date("2024-01-31".into()));
        assert_eq!(proto_value(proto::TypedValue { number_value: 49_500_250, ..tv(rep::JAVA_SQL_TIME) }, &c), Cell::Time("13:45:00.250".into()));
        assert_eq!(proto_value(proto::TypedValue { double_value: 1.100000023841858, ..tv(rep::FLOAT) }, &c), Cell::Float(1.1));
        assert_eq!(proto_value(proto::TypedValue { double_value: 1.100000023841858, ..tv(rep::DOUBLE) }, &col("FLOAT", "")), Cell::Float(1.1));
        assert_eq!(proto_value(proto::TypedValue { double_value: 1.100000023841858, ..tv(rep::DOUBLE) }, &col("DOUBLE", "")), Cell::Float(1.100000023841858));

        assert_eq!(json_cell(&json!("yv4="), &col("VARBINARY", "BYTE_STRING")), Cell::Bytes(vec![0xca, 0xfe]));
        assert_eq!(json_cell(&json!(19753), &col("DATE", "JAVA_SQL_DATE")), Cell::Date("2024-01-31".into()));
        assert_eq!(json_cell(&json!(12.5), &col("DECIMAL(10,2)", "BIG_DECIMAL")), Cell::Decimal("12.5".into()));
        assert_eq!(json_cell(&json!(5), &col("INTEGER", "PRIMITIVE_INT")), Cell::Int(5));
    }

    /// A Phoenix DATE / TIME read as TIMESTAMP keeps its time / date, and
    /// only becomes a date / time when nothing is lost.
    #[test]
    fn widened_dates_and_times() {
        let tv = |ms: i64| proto::TypedValue { r#type: proto::rep::JAVA_SQL_TIMESTAMP, number_value: ms, ..Default::default() };
        let date = Col { widened: Some(Widened::Date), ..col("DATE", "JAVA_SQL_TIMESTAMP") };
        assert_eq!(proto_value(tv(1_706_697_015_250), &date), Cell::DateTime("2024-01-31 10:30:15.250".into()));
        assert_eq!(proto_value(tv(1_706_659_200_000), &date), Cell::Date("2024-01-31".into()));
        let time = Col { widened: Some(Widened::Time), ..col("TIME", "JAVA_SQL_TIMESTAMP") };
        assert_eq!(proto_value(tv(49_500_250), &time), Cell::Time("13:45:00.250".into()));
        assert_eq!(proto_value(tv(1_706_708_700_000), &time), Cell::DateTime("2024-01-31 13:45:00".into()));
        assert_eq!(json_cell(&json!(1_706_697_015_250i64), &date), Cell::DateTime("2024-01-31 10:30:15.250".into()));
    }

    /// JSON numbers keep every digit (serde_json alone would go through f64).
    #[test]
    fn json_numbers_are_exact() {
        let body = br#"{"rows":[[12345678901234567.89,"a \"1.5\" b",-1.5E-3,7,123456789012345678901,true]],"offset":0}"#;
        let v: Value = serde_json::from_slice(&exact_numbers(body)).unwrap();
        let row = v["rows"][0].as_array().unwrap();
        assert_eq!(json_cell(&row[0], &col("DECIMAL(19,2)", "BIG_DECIMAL")), Cell::Decimal("12345678901234567.89".into()));
        assert_eq!(json_cell(&row[1], &col("VARCHAR", "STRING")), Cell::Text("a \"1.5\" b".into()));
        assert_eq!(json_cell(&row[2], &col("DECIMAL", "BIG_DECIMAL")), Cell::Decimal("-0.0015".into()));
        assert_eq!(json_cell(&row[3], &col("INTEGER", "PRIMITIVE_INT")), Cell::Int(7));
        assert_eq!(json_cell(&row[4], &col("DECIMAL", "")), Cell::Decimal("123456789012345678901".into()));
        assert_eq!(v["offset"], json!(0));
        assert_eq!(json_cell(&json!([1, {"\u{0}n": "1.25"}]), &col("DECIMAL ARRAY", "ARRAY")), Cell::Json("[1,1.25]".into()));
        // Nothing to keep: the body is used as it is.
        assert!(matches!(exact_numbers(br#"{"a":[1,2]}"#), std::borrow::Cow::Borrowed(_)));
    }

    #[test]
    fn frames_are_sized_by_bytes() {
        // Rows of at most 100 KB: about 20 per frame, the first one included.
        assert_eq!(frame_rows(Some(100_000), 0, 0), 20);
        // Small rows seen so far don't grow the frame past the bound: the
        // next rows may be the big ones.
        assert_eq!(frame_rows(Some(100_000), 8, 80), 20);
        // Rows larger than the bound (grown since it was measured) shrink it.
        assert_eq!(frame_rows(Some(1_000), 8, 1_600_000), 10);
        // Tiny rows: the row cap.
        assert_eq!(frame_rows(Some(40), 0, 0), FRAME_ROWS);
        // A row bigger than the frame, or nothing that tells: one at a time.
        assert_eq!(frame_rows(Some(10 * FRAME_BYTES), 0, 0), 1);
        assert_eq!(frame_rows(None, 100, 1_000), 1);
    }

    /// The row bound comes from the types, and what they don't limit is
    /// measured in the rows to read (never guessed from rows already seen).
    #[test]
    fn row_bound_from_types_and_data() {
        let c = |name: &str, t: &str| Col { name: name.into(), ..col(t, "") };
        let fixed = [c("ID", "BIGINT"), c("S", "VARCHAR(10)"), c("D", "DECIMAL(10,2)"), c("B", "BINARY(4)")];
        assert_eq!(row_bound_parts(&fixed, true, false, "FROM T"), Some((4 * VALUE_OVERHEAD + 24 + 80 + 18 + 20, None)));
        let (fixed_part, probe) = row_bound_parts(&[c("ID", "BIGINT"), c("B", "VARBINARY"), c("S\"x", "VARCHAR")], true, false, "FROM T WHERE ID > 1").unwrap();
        assert_eq!(fixed_part, 3 * VALUE_OVERHEAD + 24);
        assert_eq!(
            probe.unwrap(),
            r#"SELECT MAX(COALESCE(CAST(OCTET_LENGTH("B") AS BIGINT) * 5, 0) + COALESCE(CAST(LENGTH("S""x") AS BIGINT) * 8, 0)) FROM T WHERE ID > 1"#
        );
        // A declared length too big to trust as a bound is measured.
        assert!(matches!(width(&c("S", "VARCHAR(2147483647)"), true, false), Width::Data(_)));
        // JSON: escapes and base64 take more.
        assert_eq!(width(&c("S", "VARCHAR(10)"), true, true), Width::Fixed(60));
        assert_eq!(width(&c("B", "VARBINARY"), false, true), Width::Data(r#"CAST(OCTET_LENGTH("B") AS BIGINT) * 2"#.into()));
        assert_eq!(width(&c("S", "VARCHAR"), false, false), Width::Data(r#"CAST(CHAR_LENGTH("S") AS BIGINT) * 8"#.into()));
        // Arrays (Phoenix): by their length and elements.
        assert_eq!(width(&c("A", "INTEGER ARRAY"), true, false), Width::Data(format!(r#"CAST(ARRAY_LENGTH("A") AS BIGINT) * {}"#, 24 + VALUE_OVERHEAD)));
        assert!(matches!(width(&c("A", "VARCHAR ARRAY"), true, false), Width::Data(e) if e.contains(r#"ARRAY_TO_STRING("A", ',')"#)));
        // A column in a family is named with it (the name may repeat in
        // another family).
        let v = Col { family: Some("A".into()), ..c("V", "VARCHAR") };
        assert_eq!(width(&v, true, false), Width::Data(r#"CAST(LENGTH("A"."V") AS BIGINT) * 8"#.into()));
        assert_eq!(col_ref(&Col { family: Some("b\"f".into()), ..c("V", "DATE") }), r#""b""f"."V""#);
        // What nothing measures: one row a frame.
        assert_eq!(row_bound_parts(&[c("A", "VARCHAR ARRAY")], false, false, "FROM T"), None);
        assert_eq!(row_bound_parts(&[c("X", "OTHER")], true, false, "FROM T"), None);
    }

    /// The elements of a Phoenix DATE / TIME array, read as TIMESTAMP, keep
    /// their time (they used to come as days, the time cut).
    #[test]
    fn widened_date_arrays() {
        let tv = |ms: i64| proto::TypedValue { r#type: proto::rep::JAVA_SQL_TIMESTAMP, number_value: ms, ..Default::default() };
        let dates = Col { widened: Some(Widened::Date), ..col("DATE ARRAY", "ARRAY") };
        let v = proto::TypedValue { r#type: proto::rep::ARRAY, array_value: vec![tv(1_706_697_015_250), tv(1_706_659_200_000)], ..Default::default() };
        assert_eq!(proto_value(v, &dates), Cell::Json(r#"["2024-01-31 10:30:15.250","2024-01-31"]"#.into()));
        let times = Col { widened: Some(Widened::Time), ..col("TIME ARRAY", "ARRAY") };
        let v = proto::TypedValue { r#type: proto::rep::ARRAY, array_value: vec![tv(49_500_500)], ..Default::default() };
        assert_eq!(proto_value(v, &times), Cell::Json(r#"["13:45:00.500"]"#.into()));
        // JSON serialization: the numbers become dates and times too.
        assert_eq!(json_cell(&json!([1_706_697_015_250i64, null]), &dates), Cell::Json(r#"["2024-01-31 10:30:15.250",null]"#.into()));
        assert_eq!(json_cell(&json!([19753]), &col("DATE ARRAY", "ARRAY")), Cell::Json(r#"["2024-01-31"]"#.into()));
    }

    #[test]
    fn loads_need_a_multi_threaded_runtime() {
        let rt = tokio::runtime::Builder::new_current_thread().build().unwrap();
        assert!(!rt.block_on(async { multi_thread() }));
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(1).build().unwrap();
        assert!(rt.block_on(async { multi_thread() }));
    }

    #[test]
    fn cells_to_parameters() {
        assert_eq!(param_value(&Cell::Int(5), p(4)).unwrap(), PValue::Num(INTEGER, "INTEGER", 5));
        assert_eq!(param_value(&Cell::Text("7".into()), p(-5)).unwrap(), PValue::Num(LONG, "LONG", 7));
        assert!(param_value(&Cell::Text("x".into()), p(-5)).is_err());
        assert_eq!(param_value(&Cell::Decimal("12.30".into()), p(3)).unwrap(), PValue::Str(proto::rep::BIG_DECIMAL, "BIG_DECIMAL", "12.30".into()));
        assert_eq!(param_value(&Cell::Date("2024-01-31".into()), p(91)).unwrap(), PValue::Num(proto::rep::JAVA_SQL_DATE, "JAVA_SQL_DATE", 19753));
        assert_eq!(param_value(&Cell::Time("13:45:00.250".into()), p(92)).unwrap(), PValue::Num(proto::rep::JAVA_SQL_TIME, "JAVA_SQL_TIME", 49_500_250));
        assert_eq!(
            param_value(&Cell::DateTime("2024-01-31 13:45:00.123".into()), p(93)).unwrap(),
            PValue::Num(proto::rep::JAVA_SQL_TIMESTAMP, "JAVA_SQL_TIMESTAMP", 1_706_708_700_123)
        );
        assert_eq!(
            param_value(&Cell::DateTimeTz("2024-01-31 10:45:00.123-03:00".into()), p(93)).unwrap(),
            PValue::Num(proto::rep::JAVA_SQL_TIMESTAMP, "JAVA_SQL_TIMESTAMP", 1_706_708_700_123)
        );
        assert_eq!(param_value(&Cell::Null, p(4)).unwrap(), PValue::Null);
        assert_eq!(param_value(&Cell::Bytes(vec![1, 2]), p(-3)).unwrap(), PValue::Bytes(vec![1, 2]));
        assert_eq!(Param::of(0, "UNSIGNED_INT"), p(4));
        let v = param_value(&Cell::Bytes(vec![0xca, 0xfe]), p(-3)).unwrap();
        assert_eq!(v.json(), json!({"type": "BYTE_STRING", "value": "yv4="}));
        assert!(PValue::Null.proto().null);
    }

    /// FLOAT goes as DOUBLE (Avatica's FLOAT reaches Phoenix as 0), and a
    /// value out of its range fails.
    #[test]
    fn floats_go_as_doubles() {
        for t in [6, 7, 8] {
            assert_eq!(param_value(&Cell::Float(1.5), p(t)).unwrap(), PValue::Double(proto::rep::DOUBLE, "DOUBLE", 1.5));
            assert_eq!(param_value(&Cell::Int(3), p(t)).unwrap(), PValue::Double(proto::rep::DOUBLE, "DOUBLE", 3.0));
        }
        assert!(param_value(&Cell::Float(f64::MAX), p(6)).is_err());
        assert!(param_value(&Cell::Float(f64::MAX), p(8)).is_ok());
        // UNSIGNED_FLOAT (Phoenix's code 14) is a FLOAT too.
        assert_eq!(Param::of(14, "UNSIGNED_FLOAT"), p(6));
    }

    /// Phoenix's UNSIGNED_* codes are its types, and a UInt goes as a number.
    #[test]
    fn phoenix_unsigned_codes() {
        assert_eq!(Param::of(10, "UNSIGNED_LONG"), p(-5));
        assert_eq!(param_value(&Cell::UInt(5), Param::of(10, "UNSIGNED_LONG")).unwrap(), PValue::Num(LONG, "LONG", 5));
        assert!(param_value(&Cell::UInt(u64::MAX), Param::of(10, "UNSIGNED_LONG")).is_err());
        for (code, t) in [(9, 4), (11, -6), (13, 5), (15, 8), (18, 92), (19, 91), (20, 93)] {
            assert_eq!(Param::of(code, "").t, t, "{code}");
        }
        // An unknown type: by the value, a UInt as a number.
        assert_eq!(param_value(&Cell::UInt(5), p(1111)).unwrap(), PValue::Num(LONG, "LONG", 5));
        assert_eq!(param_value(&Cell::UInt(u64::MAX), p(1111)).unwrap(), PValue::Str(proto::rep::BIG_DECIMAL, "BIG_DECIMAL", u64::MAX.to_string()));
    }

    /// Arrays read as JSON go back as Avatica arrays.
    #[test]
    fn arrays_as_parameters() {
        let int_array = Param::of(2003, "INTEGER ARRAY");
        assert_eq!(int_array, Param { t: ARRAY, elem: 4, full_date: false });
        assert_eq!(Param::of(3004, "").elem, 4);
        let v = param_value(&Cell::Json("[1,2,null]".into()), int_array).unwrap();
        assert_eq!(v, PValue::Array(INTEGER, vec![PValue::Num(INTEGER, "INTEGER", 1), PValue::Num(INTEGER, "INTEGER", 2), PValue::Null]));
        let t = v.proto();
        assert_eq!((t.r#type, t.component_type, t.array_value.len()), (proto::rep::ARRAY, INTEGER, 3));
        assert!(param_value(&Cell::Json("{\"a\":1}".into()), int_array).is_err());
        let bin = Param::of(0, "VARBINARY ARRAY");
        assert_eq!(param_value(&Cell::Json("[\"0xCAFE\"]".into()), bin).unwrap(), PValue::Array(proto::rep::BYTE_STRING, vec![PValue::Bytes(vec![0xca, 0xfe])]));
    }

    /// A fraction finer than the millisecond can't cross Avatica: it fails
    /// instead of being cut.
    #[test]
    fn sub_millisecond_fractions_fail() {
        let e = param_value(&Cell::DateTime("2024-01-01 10:00:00.123456789".into()), p(93)).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e}");
        assert!(param_value(&Cell::DateTime("2024-01-01 10:00:00.123000".into()), p(93)).is_ok());
        assert!(matches!(param_value(&Cell::Time("10:00:00.1234".into()), p(92)), Err(Error::Unsupported(_))));
        assert!(matches!(param_value(&Cell::DateTime("2024-01-01 10:00:00.0000001".into()), p(1111)), Err(Error::Unsupported(_))));
    }

    /// A DATE with a time keeps it in Phoenix and fails elsewhere.
    #[test]
    fn dates_with_time() {
        let full = Param { t: 91, elem: 0, full_date: true };
        let dt = Cell::DateTime("2024-01-31 10:30:15.250".into());
        assert_eq!(param_value(&dt, full).unwrap(), PValue::Num(proto::rep::JAVA_SQL_TIMESTAMP, "JAVA_SQL_TIMESTAMP", 1_706_697_015_250));
        assert!(param_value(&dt, p(91)).is_err());
        assert_eq!(param_value(&Cell::DateTime("2024-01-31 00:00:00".into()), p(91)).unwrap(), PValue::Num(proto::rep::JAVA_SQL_DATE, "JAVA_SQL_DATE", 19753));
        let full_time = Param { t: 92, elem: 0, full_date: true };
        assert!(param_value(&dt, full_time).is_ok());
        assert!(param_value(&dt, p(92)).is_err());
    }
}
