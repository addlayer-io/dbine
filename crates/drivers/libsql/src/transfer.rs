//! Bulk transfer (see `dbine_driver::transfer`) for libSQL / Turso over
//! Hrana (HTTP).
//!
//! - **Read** ([`read_batches`]): pages by rowid (`WHERE rowid > last
//!   ORDER BY rowid LIMIT n`, ~4 MiB of response each) inside one read
//!   transaction on the Hrana stream, so every page sees the same
//!   snapshot; a page the server finds too large is asked again with half
//!   the rows. A table without rowid (a view, `WITHOUT ROWID`) is read
//!   with one `SELECT` through a Hrana 3 cursor (`POST /v3/cursor`), whose
//!   response streams the rows as the server steps them. Pages are the
//!   default because sqld sends a cursor one row per HTTP chunk: 1M rows
//!   took 20 s through the cursor and ~7 s in pages (measured on a local
//!   sqld). Responses are decoded straight into cells (no
//!   `serde_json::Value` tree), and values keep their Hrana type (integer
//!   → `Int`, float → `Float`, text → `Text`, blob → `Bytes`, whole).
//! - **Floats, exactly.** sqld writes a float as the shortest text that
//!   round-trips, but serde_json's default parse (this build has no
//!   `float_roundtrip`) can land one ULP off, so each float is re-read
//!   from the response's own text with `f64::from_str`, which is exact.
//!   sqld sends ±∞ as `null` (JSON has no infinity): its sign is asked
//!   again by rowid. Going the other way, sqld's JSON parse and SQLite's
//!   own literal parse both round (872 of 2 999 random values changed,
//!   measured), so a load binds each float as two integers, its own
//!   mantissa and exponent, and the statement rebuilds it with one exact
//!   product, `m*pow(2,e)`, once per column (see [`insert_request`]).
//!   On a server without SQLite's math functions each float is written
//!   as `CAST(m AS REAL)` scaled by powers of two up to 2^62 (exact, but
//!   slow for the server). SQLite can't store NaN: a load refuses it.
//! - **Load** ([`bulk_load`]): prepared multi-row `INSERT`s, one HTTP
//!   request per incoming batch, as a Hrana `batch` whose steps run only
//!   while the previous one succeeded. A commit window is one transaction
//!   on the Hrana stream (`BEGIN IMMEDIATE` … `COMMIT`), spanning as many
//!   requests as it needs; its `COMMIT` goes in a request of its own,
//!   after all its rows were acknowledged. Requests carry the stream's
//!   baton and are never replayed on another stream in the middle of a
//!   window: if the stream expired, the transaction is gone and the load
//!   fails. A failed load rolls its window back on that same stream; when
//!   it can't (a lost reply left the baton stale), it waits for the
//!   window's transaction to be gone before returning, so a retry doesn't
//!   find the write lock held. `table_lock` and `keep_identity` need nothing (SQLite locks
//!   the database for a writer; an `INTEGER PRIMARY KEY` takes the given
//!   value).
//! - **Cancel.** The orchestrator cancels by dropping the future, but a
//!   request already on the wire keeps running on the server. So each
//!   request runs in a task of its own, and dropping the load mid-window
//!   waits (in the background) for the request in flight, then rolls the
//!   window back on its stream and closes it: its rows are never
//!   committed and the write lock is freed at once, not when the stream
//!   expires. The only request that can still land after a drop is a
//!   window's lone `COMMIT`; its transaction already held the write lock
//!   before the drop, so anything written after the drop (the
//!   orchestrator emptying the table) runs after it.
//! - **Native copy**: not offered. Two libSQL databases are two servers
//!   (or two Turso databases) and neither can read the other: the rows have
//!   to pass through DBine.

use crate::hrana::{Client, B64};
use crate::LibsqlSession;
use base64::Engine;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use dbine_driver_sqlite::transfer::{column_list, qualified_columns, select_sql, table_name};
use serde::de::{self, Deserializer, MapAccess, Visitor};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::fmt::{self, Write as _};

/// Rows per `INSERT` statement (fewer when the columns would pass
/// SQLite's 32 766 bound parameters).
const ROWS_PER_STMT: usize = 100;
const MAX_PARAMS: usize = 32_766;
/// Rows of a read's first page; the next ones aim at [`PAGE_BYTES`] of
/// response (sqld refuses one past ~10 MB: the page is halved then).
const FIRST_PAGE: usize = 256;
const PAGE_BYTES: usize = 4 * 1024 * 1024;
const MAX_PAGE: usize = 50_000;

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

// ---------------------------------------------------------------------
// Pipelines sent and decoded here (not through `Client::pipeline`).
// ---------------------------------------------------------------------

/// Where a pipeline goes, detached from the client (so a task can own it).
#[derive(Clone)]
struct Wire {
    http: reqwest::Client,
    url: String,
    token: Option<String>,
}

impl Wire {
    fn of(c: &Client) -> Self {
        let (http, url, token) = c.wire();
        Wire { http, url, token }
    }
}

enum Fail {
    /// The baton was refused: the stream (and its transaction) is gone,
    /// and nothing of the request ran.
    Expired,
    Other(Error),
}

/// `{"baton": …, "requests": […]}`.
fn pipeline_body(baton: Option<&str>, requests: &[&str]) -> String {
    let mut body = String::with_capacity(64 + requests.iter().map(|r| r.len() + 1).sum::<usize>());
    body.push_str("{\"baton\":");
    body.push_str(&baton.map_or_else(|| "null".to_string(), json_str));
    body.push_str(",\"requests\":[");
    body.push_str(&requests.join(","));
    body.push_str("]}");
    body
}

fn json_str(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

/// An `execute` request.
fn execute_req(sql: &str, want_rows: bool) -> String {
    format!(r#"{{"type":"execute","stmt":{{"sql":{},"want_rows":{want_rows}}}}}"#, json_str(sql))
}

/// POST a pipeline; `parse` decodes the response's bytes.
async fn post<T>(w: Wire, body: String, with_baton: bool, parse: fn(&[u8]) -> Result<T>) -> std::result::Result<T, Fail> {
    let mut req = w.http.post(&w.url).header(reqwest::header::CONTENT_TYPE, "application/json").body(body);
    if let Some(t) = &w.token {
        req = req.bearer_auth(t);
    }
    let resp = req.send().await.map_err(|e| Fail::Other(Error::Connect(format!("no se pudo llegar al servidor: {e}"))))?;
    let status = resp.status();
    let bytes = resp.bytes().await.map_err(|e| Fail::Other(Error::Connect(format!("se cortó la respuesta del servidor: {e}"))))?;
    if status == reqwest::StatusCode::UNAUTHORIZED || status == reqwest::StatusCode::FORBIDDEN {
        let text = String::from_utf8_lossy(&bytes);
        return Err(Fail::Other(Error::AuthFailed(format!("el servidor rechazó el token ({status}): {}", text.trim()))));
    }
    if !status.is_success() {
        let text = String::from_utf8_lossy(&bytes);
        // "Received an invalid baton" or {"code": "STREAM_EXPIRED"}.
        let lower = text.to_ascii_lowercase();
        if with_baton && (lower.contains("baton") || lower.contains("stream_expired")) {
            return Err(Fail::Expired);
        }
        return Err(Fail::Other(Error::Query(format!("HTTP {status}: {}", text.trim()))));
    }
    parse(&bytes).map_err(Fail::Other)
}

#[derive(Deserialize)]
struct Reply<R> {
    baton: Option<String>,
    base_url: Option<String>,
    #[serde(default = "Vec::new")]
    results: Vec<Entry<R>>,
}

#[derive(Deserialize)]
struct Entry<R> {
    #[serde(rename = "type")]
    kind: String,
    response: Option<Resp<R>>,
    error: Option<HError>,
}

impl<R> Entry<R> {
    fn into_result(self) -> std::result::Result<Option<R>, String> {
        match self.kind.as_str() {
            "ok" => Ok(self.response.and_then(|r| r.result)),
            _ => Err(self.error.map_or_else(|| "error del servidor".into(), HError::text)),
        }
    }
}

#[derive(Deserialize)]
struct Resp<R> {
    result: Option<R>,
}

#[derive(Deserialize)]
struct HError {
    message: Option<String>,
}

impl HError {
    fn text(self) -> String {
        let msg = self.message.unwrap_or_else(|| "error del servidor".into());
        msg.strip_prefix("SQLite error: ").map(str::to_string).unwrap_or(msg)
    }
}

/// An `execute` result.
#[derive(Deserialize, Default)]
struct Rows {
    #[serde(default)]
    cols: Vec<Col>,
    #[serde(default)]
    rows: Vec<Vec<HCell>>,
}

#[derive(Deserialize)]
struct Col {
    name: Option<String>,
    decltype: Option<String>,
}

/// A `batch` result (other results decode as an empty one).
#[derive(Deserialize, Default)]
struct BatchResult {
    #[serde(default)]
    step_errors: Vec<Option<HError>>,
}

/// A Hrana value decoded straight into a cell. A float holds serde_json's
/// parse until [`exact_floats`] replaces it; a float without value (±∞)
/// is NaN until its sign is known (sqld never sends NaN: SQLite has none).
struct HCell(Cell);

enum Scalar {
    Null,
    Str(String),
    Int(i64),
    Num(f64),
}

impl<'de> Deserialize<'de> for Scalar {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = Scalar;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Hrana value")
            }
            fn visit_unit<E>(self) -> std::result::Result<Scalar, E> {
                Ok(Scalar::Null)
            }
            fn visit_none<E>(self) -> std::result::Result<Scalar, E> {
                Ok(Scalar::Null)
            }
            fn visit_str<E>(self, s: &str) -> std::result::Result<Scalar, E> {
                Ok(Scalar::Str(s.to_string()))
            }
            fn visit_string<E>(self, s: String) -> std::result::Result<Scalar, E> {
                Ok(Scalar::Str(s))
            }
            fn visit_i64<E>(self, i: i64) -> std::result::Result<Scalar, E> {
                Ok(Scalar::Int(i))
            }
            fn visit_u64<E>(self, u: u64) -> std::result::Result<Scalar, E> {
                Ok(i64::try_from(u).map_or(Scalar::Num(u as f64), Scalar::Int))
            }
            fn visit_f64<E>(self, f: f64) -> std::result::Result<Scalar, E> {
                Ok(Scalar::Num(f))
            }
        }
        d.deserialize_any(V)
    }
}

#[derive(Deserialize)]
#[serde(field_identifier, rename_all = "lowercase")]
enum Key {
    Type,
    Value,
    Base64,
    #[serde(other)]
    Other,
}

impl<'de> Deserialize<'de> for HCell {
    fn deserialize<D: Deserializer<'de>>(d: D) -> std::result::Result<Self, D::Error> {
        struct V;
        impl<'de> Visitor<'de> for V {
            type Value = HCell;
            fn expecting(&self, f: &mut fmt::Formatter) -> fmt::Result {
                f.write_str("a Hrana value")
            }
            fn visit_map<A: MapAccess<'de>>(self, mut m: A) -> std::result::Result<HCell, A::Error> {
                let (mut ty, mut value, mut b64) = (None::<String>, Scalar::Null, None::<String>);
                while let Some(k) = m.next_key::<Key>()? {
                    match k {
                        Key::Type => ty = Some(m.next_value()?),
                        Key::Value => value = m.next_value()?,
                        Key::Base64 => b64 = m.next_value()?,
                        Key::Other => {
                            m.next_value::<de::IgnoredAny>()?;
                        }
                    }
                }
                let cell = match (ty.as_deref(), value) {
                    (Some("null"), _) => Cell::Null,
                    (Some("integer"), Scalar::Str(s)) => Cell::Int(s.parse().map_err(|_| de::Error::custom(format!("entero inválido: {s}")))?),
                    (Some("integer"), Scalar::Int(i)) => Cell::Int(i),
                    (Some("float"), Scalar::Num(f)) => Cell::Float(f),
                    (Some("float"), Scalar::Int(i)) => Cell::Float(i as f64),
                    (Some("float"), Scalar::Null) => Cell::Float(f64::NAN),
                    (Some("text"), Scalar::Str(s)) => Cell::Text(s),
                    (Some("blob"), _) => match b64 {
                        Some(b) => Cell::Bytes(B64.decode(b).map_err(|e| de::Error::custom(format!("blob inválido: {e}")))?),
                        None => Cell::Bytes(Vec::new()),
                    },
                    (ty, _) => return Err(de::Error::custom(format!("valor inesperado del servidor (tipo {ty:?})"))),
                };
                Ok(HCell(cell))
            }
        }
        d.deserialize_map(V)
    }
}

const FLOAT_TAG: &[u8] = br#"{"type":"float","value":"#;

/// The texts of the float values in a Hrana JSON response, in order (`null`
/// for ±∞). The sequence can't occur inside a JSON string: its quotes
/// would be escaped there.
fn float_texts(json: &[u8]) -> Vec<&[u8]> {
    let mut out = Vec::new();
    let mut i = 0;
    while i < json.len() {
        let Some(p) = json[i..].iter().position(|&b| b == b'{') else { break };
        let at = i + p;
        if json[at..].starts_with(FLOAT_TAG) {
            let start = at + FLOAT_TAG.len();
            let end = json[start..].iter().position(|&b| b == b',' || b == b'}').map_or(json.len(), |e| start + e);
            out.push(&json[start..end]);
            i = end;
        } else {
            i = at + 1;
        }
    }
    out
}

/// Each float cell (in the response's order) takes its exact value from
/// its own text; ±∞ (no value) stays NaN.
fn exact_floats<'a>(cells: impl Iterator<Item = &'a mut Cell>, texts: &[&[u8]]) -> Result<()> {
    let mismatch = || Error::Query("respuesta inesperada del servidor: no se pudieron leer los valores REAL exactos".into());
    let mut texts = texts.iter();
    for c in cells {
        if let Cell::Float(f) = c {
            let t = texts.next().ok_or_else(mismatch)?;
            *f = match *t {
                b"null" => f64::NAN,
                t => std::str::from_utf8(t).ok().and_then(|s| s.trim().parse::<f64>().ok()).ok_or_else(mismatch)?,
            };
        }
    }
    if texts.next().is_some() {
        return Err(mismatch());
    }
    Ok(())
}

fn decode<T: serde::de::DeserializeOwned>(b: &[u8]) -> Result<T> {
    serde_json::from_slice(b).map_err(|e| Error::Query(format!("respuesta inválida del servidor: {e}")))
}

/// A page's response, floats exact, and its size in bytes.
fn parse_rows(b: &[u8]) -> Result<(Reply<Rows>, usize)> {
    let mut reply: Reply<Rows> = decode(b)?;
    let cells = reply
        .results
        .iter_mut()
        .filter_map(|e| e.response.as_mut().and_then(|r| r.result.as_mut()))
        .flat_map(|r| r.rows.iter_mut().flatten().map(|h| &mut h.0));
    exact_floats(cells, &float_texts(b))?;
    Ok((reply, b.len()))
}

fn parse_batch(b: &[u8]) -> Result<Reply<BatchResult>> {
    decode(b)
}

fn parse_nothing(_: &[u8]) -> Result<()> {
    Ok(())
}

// ---------------------------------------------------------------------
// Read
// ---------------------------------------------------------------------

/// `name → NOT NULL` of the table's columns (empty for a view).
async fn not_null(client: &mut Client, spec: &ReadSpec) -> Result<HashMap<String, bool>> {
    let lit = |s: &str| format!("'{}'", s.replace('\'', "''"));
    let sql = format!(
        "SELECT name, \"notnull\" OR pk > 0 FROM pragma_table_info({}, {})",
        lit(&spec.table.name),
        lit(spec.table.schema().unwrap_or("main"))
    );
    let r = client.execute(&sql).await?;
    Ok(r.rows
        .into_iter()
        .filter_map(|row| {
            let name = row.first()?.as_str()?.to_string();
            let nn = row.get(1).and_then(Value::as_i64).unwrap_or(0) != 0;
            Some((name, nn))
        })
        .collect())
}

fn columns(cols: &[Col], not_null: &HashMap<String, bool>) -> Vec<TransferColumn> {
    cols.iter()
        .map(|c| {
            let name = c.name.clone().unwrap_or_default();
            TransferColumn {
                type_name: c.decltype.clone().unwrap_or_default(),
                nullable: !not_null.get(&name).copied().unwrap_or(false),
                name,
            }
        })
        .collect()
}

pub(crate) async fn read_batches(s: &mut LibsqlSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let not_null = not_null(&mut s.client, spec).await?;
    let mut builder = BatchBuilder::new();
    // The rowid under a name no column takes.
    let key = ["rowid", "_rowid_", "oid"].into_iter().find(|k| !not_null.keys().any(|n| n.eq_ignore_ascii_case(k)));
    let paged = match key {
        Some(key) => read_paged(s, spec, key, &not_null, &sink, &mut builder).await?,
        None => false,
    };
    if !paged {
        let sql = select_sql(spec);
        match s.client.cursor(&sql).await? {
            Some(resp) => read_cursor(resp, &not_null, &sink, &mut builder).await?,
            None => return Err(Error::Unsupported("el servidor no tiene cursores (Hrana 3) y la tabla no tiene rowid".into())),
        }
    }
    builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
    Ok(builder.rows)
}

/// `no such column: rowid`, and not some other column that starts alike.
fn no_such(msg: &str, key: &str) -> bool {
    let needle = format!("no such column: {key}");
    msg.match_indices(&needle).any(|(i, _)| {
        msg[i + needle.len()..].chars().next().is_none_or(|c| !(c.is_alphanumeric() || c == '_' || c == '.' || c == '"'))
    })
}

/// Pages by rowid (`WHERE rowid > last ORDER BY rowid LIMIT n`) inside one
/// read transaction, so every page sees the same snapshot. `false`: the
/// table has no rowid (a view, `WITHOUT ROWID`), nothing was read.
async fn read_paged(
    s: &mut LibsqlSession,
    spec: &ReadSpec,
    key: &str,
    not_null: &HashMap<String, bool>,
    sink: &BatchSinkRef,
    builder: &mut BatchBuilder,
) -> Result<bool> {
    let table = table_name(&spec.table);
    // Qualified: a bare `"nope"` that names no column would be read as a
    // string literal.
    let cols = spec.columns.as_deref().map_or_else(|| "*".to_string(), |c| qualified_columns(&table, c));
    let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
    let in_tx = s.client.execute("BEGIN").await.is_ok();
    let client = &mut s.client;
    let r: Result<bool> = async {
        let (mut last, mut limit, mut begun) = (None::<i64>, FIRST_PAGE, false);
        let mut names: Vec<String> = Vec::new();
        loop {
            let mut cond: Vec<String> = Vec::new();
            if let Some(l) = last {
                cond.push(format!("{key} > {l}"));
            }
            if let Some(f) = filter {
                cond.push(format!("({f})"));
            }
            let where_ = if cond.is_empty() { String::new() } else { format!(" WHERE {}", cond.join(" AND ")) };
            let sql = format!("SELECT {key}, {cols} FROM {table}{where_} ORDER BY {key} LIMIT {limit}");
            let baton = client.baton().map(str::to_string);
            let body = pipeline_body(baton.as_deref(), &[&execute_req(&sql, true)]);
            let (reply, len) = match post(Wire::of(client), body, baton.is_some(), parse_rows).await {
                Ok(r) => r,
                Err(Fail::Expired) => {
                    client.set_stream(None, None);
                    if in_tx {
                        return Err(Error::Query("la sesión en el servidor venció en medio de la lectura".into()));
                    }
                    continue;
                }
                Err(Fail::Other(e)) => return Err(e),
            };
            client.set_stream(reply.baton, reply.base_url.as_deref());
            let result = match reply.results.into_iter().next().map(Entry::into_result) {
                Some(Ok(r)) => r.unwrap_or_default(),
                Some(Err(m)) if m.to_ascii_lowercase().contains("too large") && limit > 1 => {
                    limit = (limit / 2).max(1);
                    continue;
                }
                Some(Err(m)) if !begun && no_such(&m, key) => return Ok(false),
                Some(Err(m)) => return Err(Error::Query(m)),
                None => return Err(Error::Query("el servidor no respondió la lectura".into())),
            };
            if !begun {
                let all = columns(&result.cols, not_null);
                names = all.iter().skip(1).map(|c| c.name.clone()).collect();
                sink.lock().map_err(lock_err)?.begin(&all[1.min(all.len())..])?;
                begun = true;
            }
            let n = result.rows.len();
            for row in result.rows {
                let mut cells: Vec<Cell> = row.into_iter().map(|h| h.0).collect();
                if cells.is_empty() {
                    continue;
                }
                let k = match cells.remove(0) {
                    Cell::Int(k) => k,
                    other => return Err(Error::Query(format!("rowid inesperado: {other:?}"))),
                };
                last = Some(k);
                for (j, c) in cells.iter_mut().enumerate() {
                    if matches!(c, Cell::Float(f) if f.is_nan()) {
                        // ±∞ came without value: ask its sign.
                        let col = match spec.columns.as_deref() {
                            Some(cs) => cs.get(j).cloned(),
                            None => names.get(j).cloned(),
                        }
                        .unwrap_or_default();
                        let q = format!("SELECT {table}.{} > 0 FROM {table} WHERE {key} = {k}", quote_ident(Quote::Double, &col));
                        let r = client.execute(&q).await?;
                        *c = match r.rows.first().and_then(|r| r.first()).and_then(Value::as_i64) {
                            Some(1) => Cell::Float(f64::INFINITY),
                            Some(0) => Cell::Float(f64::NEG_INFINITY),
                            _ => return Err(Error::Query(format!("no se pudo leer el valor infinito de la columna {col}"))),
                        };
                    }
                }
                builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
            }
            if n < limit {
                return Ok(true);
            }
            limit = ((PAGE_BYTES as u128 * n as u128 / len.max(1) as u128) as usize).clamp(1, MAX_PAGE);
        }
    }
    .await;
    if in_tx {
        let _ = client.execute("ROLLBACK").await;
    }
    r
}

/// One line of a cursor's response.
#[derive(Deserialize)]
struct CursorLine {
    #[serde(rename = "type")]
    kind: Option<String>,
    #[serde(default)]
    cols: Vec<Col>,
    #[serde(default)]
    row: Vec<HCell>,
    error: Option<HError>,
}

/// The cursor's lines: a header, then `step_begin`, `row`s and `step_end`
/// (or `step_error` / `error`).
async fn read_cursor(mut resp: reqwest::Response, not_null: &HashMap<String, bool>, sink: &BatchSinkRef, builder: &mut BatchBuilder) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    let (mut header, mut ended) = (false, false);
    let mut handle = |line: &[u8]| -> Result<()> {
        if line.iter().all(u8::is_ascii_whitespace) {
            return Ok(());
        }
        if !header {
            header = true;
            return Ok(());
        }
        let mut v: CursorLine = serde_json::from_slice(line).map_err(|e| Error::Query(format!("respuesta inválida del cursor: {e}")))?;
        match v.kind.as_deref() {
            Some("step_begin") => sink.lock().map_err(lock_err)?.begin(&columns(&v.cols, not_null))?,
            Some("row") => {
                exact_floats(v.row.iter_mut().map(|h| &mut h.0), &float_texts(line))?;
                let cells: Vec<Cell> = v.row.into_iter().map(|h| h.0).collect();
                if cells.iter().any(|c| matches!(c, Cell::Float(f) if f.is_nan())) {
                    return Err(Error::Unsupported(
                        "la tabla no tiene rowid y trae un REAL infinito: el servidor lo envía sin valor ni signo y no hay \
                         forma de volver a pedir esa fila"
                            .into(),
                    ));
                }
                builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
            }
            Some("step_end") => ended = true,
            Some("step_error") | Some("error") => {
                return Err(Error::Query(v.error.take().map_or_else(|| "error del servidor".into(), HError::text)));
            }
            _ => {}
        }
        Ok(())
    };
    while let Some(chunk) = resp.chunk().await.map_err(|e| Error::Connect(format!("se cortó la lectura: {e}")))? {
        buf.extend_from_slice(&chunk);
        let mut start = 0;
        while let Some(nl) = buf[start..].iter().position(|&b| b == b'\n') {
            handle(&buf[start..start + nl])?;
            start += nl + 1;
        }
        buf.drain(..start);
    }
    handle(&buf)?;
    if !ended {
        return Err(Error::Query("el servidor cortó la lectura antes de terminar".into()));
    }
    Ok(())
}

// ---------------------------------------------------------------------
// Load
// ---------------------------------------------------------------------

/// A float as `m · 2^e`, both exact integers (`m` odd, `e` its own
/// exponent). ±∞ as `±1 · 2^2000` and -0 as `-1 · 2^-2000`: `pow`
/// overflows and underflows to exactly those. NaN is refused (SQLite
/// would store NULL).
fn float_parts(f: f64) -> Result<(i64, i32)> {
    if f.is_nan() {
        return Err(Error::Unsupported("SQLite no puede guardar NaN (lo convertiría en NULL): la fila trae un REAL NaN".into()));
    }
    let sign = if f.is_sign_negative() { -1 } else { 1 };
    if f.is_infinite() {
        return Ok((sign, 2000));
    }
    if f == 0.0 {
        return Ok(if sign < 0 { (-1, -2000) } else { (0, 0) });
    }
    let bits = f.to_bits();
    let exp = ((bits >> 52) & 0x7ff) as i32;
    let frac = bits & ((1u64 << 52) - 1);
    let (mut m, mut e) = if exp == 0 { (frac, -1074) } else { (frac | (1u64 << 52), exp - 1075) };
    let tz = m.trailing_zeros();
    m >>= tz;
    e += tz as i32;
    Ok((sign * m as i64, e))
}

/// A float as an exact SQL expression, for a statement that can't bind it
/// in pairs (see [`insert_request`]). With `pow`, `m*pow(2,e)`: one exact
/// product. Without it (a server built without SQLite's math functions),
/// `CAST(m AS REAL)` times (or over) powers of two up to 2^62, every step
/// exact but slow to evaluate. ±∞ as `±9e999`; NaN is refused.
fn real_sql(f: f64, pow: bool) -> Result<String> {
    let (m, e) = float_parts(f)?;
    if f.is_infinite() {
        return Ok(if f > 0.0 { "9e999" } else { "-9e999" }.into());
    }
    if f == 0.0 {
        return Ok(if f.is_sign_negative() { "-0.0" } else { "0.0" }.into());
    }
    if pow {
        return Ok(format!("{m}*pow(2,{e})"));
    }
    let mut s = format!("CAST({m} AS REAL)");
    let op = if e > 0 { '*' } else { '/' };
    let mut k = e.unsigned_abs();
    while k > 0 {
        let step = k.min(62);
        let _ = write!(s, "{op}{}", 1u64 << step);
        k -= step;
    }
    Ok(s)
}

/// A cell as a Hrana argument (floats never go as such, see
/// [`insert_request`]).
/// Like SQLite's binding: integers and blobs as such, the rest as text for
/// the column's affinity; a `UInt` above `i64::MAX` as its digits.
fn push_arg(out: &mut String, c: &Cell) {
    let int = |out: &mut String, i: i64| {
        let _ = write!(out, r#"{{"type":"integer","value":"{i}"}}"#);
    };
    match c {
        Cell::Null | Cell::Float(_) => out.push_str(r#"{"type":"null"}"#),
        Cell::Bool(b) => int(out, *b as i64),
        Cell::Int(i) => int(out, *i),
        Cell::UInt(u) => match i64::try_from(*u) {
            Ok(i) => int(out, i),
            Err(_) => {
                let _ = write!(out, r#"{{"type":"text","value":"{u}"}}"#);
            }
        },
        Cell::Bytes(b) => {
            out.push_str(r#"{"type":"blob","base64":""#);
            out.push_str(&B64.encode(b));
            out.push_str("\"}");
        }
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => {
            out.push_str(r#"{"type":"text","value":"#);
            out.push_str(&json_str(s));
            out.push('}');
        }
    }
}

/// A batch step: `{"stmt": {…}, "condition": previous ok}`.
fn push_step(steps: &mut Vec<String>, sql: &str, args: &str) {
    let mut step = String::with_capacity(sql.len() + args.len() + 96);
    step.push_str(r#"{"stmt":{"sql":"#);
    step.push_str(&json_str(sql));
    step.push_str(r#","args":["#);
    step.push_str(args);
    step.push_str(r#"],"want_rows":false}"#);
    if let Some(prev) = steps.len().checked_sub(1) {
        let _ = write!(step, r#","condition":{{"type":"ok","step":{prev}}}"#);
    }
    step.push('}');
    steps.push(step);
}

/// Columns a `VALUES` row may have (SQLite's `SQLITE_MAX_COLUMN`).
const MAX_COLUMNS: usize = 2000;

/// The `batch` request with `rows` as multi-row INSERTs, after `BEGIN
/// IMMEDIATE` when `begin`; and the index of its first INSERT step.
///
/// Floats go exactly: sqld's JSON parse and SQLite's literal parse both
/// round. With `pow`, a statement whose rows carry floats binds each
/// column that has one as two integers, `m` and `e` ([`float_parts`]), so
/// its `VALUES` list holds only parameters, and rebuilds `m·2^e` once per
/// column in the `SELECT`: `INSERT INTO t (a, r) SELECT column1, CASE
/// WHEN column3 IS NULL THEN column2 ELSE column2*pow(2,column3) END FROM
/// (VALUES (?, ?, ?), …)` (another type in that column goes as itself,
/// with a NULL exponent). An expression per cell costs far more: SQLite
/// compiles each such `VALUES` row as a `SELECT` of its own. Measured on
/// sqld, 1000 rows × 60 REALs: 0.07–0.1 s this way, 1.4–3 s with
/// `m*pow(2,e)` per cell, 1.9–39 s with a chain of powers per cell.
/// Without `pow`, or when the pairs would pass [`MAX_COLUMNS`], each float
/// is an expression of its own ([`real_sql`]).
fn insert_request(spec: &LoadSpec, rows: &[Vec<Cell>], begin: bool, per_stmt: usize, pow: bool) -> Result<(String, usize)> {
    let into = format!("INSERT INTO {} ({}) ", table_name(&spec.table), column_list(&spec.columns));
    let ncols = spec.columns.len();
    let mut steps = Vec::new();
    if begin {
        push_step(&mut steps, "BEGIN IMMEDIATE", "");
    }
    let first = steps.len();
    let arg = |args: &mut String, c: &Cell| {
        if !args.is_empty() {
            args.push(',');
        }
        push_arg(args, c);
    };
    for chunk in rows.chunks(per_stmt) {
        let mut pairs: Vec<bool> = (0..ncols).map(|i| pow && chunk.iter().any(|r| matches!(r.get(i), Some(Cell::Float(_))))).collect();
        if ncols + pairs.iter().filter(|p| **p).count() > MAX_COLUMNS {
            pairs.fill(false);
        }
        let paired = pairs.contains(&true);
        let mut sql = into.clone();
        if paired {
            sql.push_str("SELECT ");
            let mut k = 1;
            for (i, p) in pairs.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                if *p {
                    let _ = write!(sql, "CASE WHEN column{e} IS NULL THEN column{k} ELSE column{k}*pow(2,column{e}) END", e = k + 1);
                    k += 2;
                } else {
                    let _ = write!(sql, "column{k}");
                    k += 1;
                }
            }
            sql.push_str(" FROM (VALUES ");
        } else {
            sql.push_str("VALUES ");
        }
        let mut args = String::new();
        for (r, row) in chunk.iter().enumerate() {
            sql.push_str(if r == 0 { "(" } else { ", (" });
            for (i, c) in row.iter().enumerate() {
                if i > 0 {
                    sql.push_str(", ");
                }
                let pair = pairs.get(i).copied().unwrap_or(false);
                match c {
                    Cell::Float(f) if pair => {
                        let (m, e) = float_parts(*f)?;
                        sql.push_str("?, ?");
                        arg(&mut args, &Cell::Int(m));
                        arg(&mut args, &Cell::Int(e.into()));
                    }
                    Cell::Float(f) => sql.push_str(&real_sql(*f, pow)?),
                    c => {
                        sql.push('?');
                        arg(&mut args, c);
                        if pair {
                            sql.push_str(", ?");
                            arg(&mut args, &Cell::Null);
                        }
                    }
                }
            }
            sql.push(')');
        }
        if paired {
            sql.push(')');
        }
        push_step(&mut steps, &sql, &args);
    }
    Ok((format!(r#"{{"type":"batch","batch":{{"steps":[{}]}}}}"#, steps.join(",")), first))
}

type InFlight = tokio::task::JoinHandle<std::result::Result<Reply<BatchResult>, Fail>>;

/// A load's hold on the stream. Dropped mid-window (the orchestrator
/// cancels by dropping the future), it rolls the window back in the
/// background, after the request in flight, if any, is done.
struct Loader<'a> {
    client: &'a mut Client,
    /// A request whose steps may still be running on the server.
    inflight: Option<InFlight>,
    /// A transaction (the window) may be open on the stream.
    open: bool,
    /// Whether the server has SQLite's math functions, once asked.
    pow: Option<bool>,
}

impl Loader<'_> {
    /// Runs one `batch` request on the stream, in a task of its own (see
    /// [`Drop`]). `fresh`: the window's first request, which may start a
    /// new stream (with the session's setup) if the old one expired.
    async fn run(&mut self, batch: &str, fresh: bool) -> Result<BatchResult> {
        let mut replaced = false;
        loop {
            let baton = self.client.baton().map(str::to_string);
            let init: Vec<String> = if baton.is_none() { self.client.init.iter().map(|s| execute_req(s, false)).collect() } else { Vec::new() };
            let mut reqs: Vec<&str> = init.iter().map(String::as_str).collect();
            reqs.push(batch);
            let body = pipeline_body(baton.as_deref(), &reqs);
            drop(reqs);
            self.inflight = Some(tokio::spawn(post(Wire::of(self.client), body, baton.is_some(), parse_batch)));
            let joined = match self.inflight.as_mut() {
                Some(h) => h.await,
                None => unreachable!(),
            };
            self.inflight = None;
            let r = joined.map_err(|e| Error::State(format!("la carga se interrumpió: {e}")))?;
            match r {
                Ok(reply) => {
                    self.client.set_stream(reply.baton, reply.base_url.as_deref());
                    return match reply.results.into_iter().nth(init.len()).map(Entry::into_result) {
                        Some(Ok(r)) => Ok(r.unwrap_or_default()),
                        Some(Err(m)) => Err(Error::Query(m)),
                        None => Err(Error::Query("el servidor no respondió el lote".into())),
                    };
                }
                Err(Fail::Expired) if fresh && !replaced => {
                    // Nothing of the request ran: again on a new stream.
                    self.client.set_stream(None, None);
                    self.client.notices.push(
                        "La sesión en el servidor venció por inactividad y se abrió otra: se perdieron las transacciones \
                         abiertas y las tablas temporales."
                            .into(),
                    );
                    replaced = true;
                }
                Err(Fail::Expired) => {
                    self.client.set_stream(None, None);
                    self.open = false;
                    return Err(Error::Query("la sesión en el servidor venció en medio de la carga y se perdió la transacción abierta".into()));
                }
                Err(Fail::Other(e)) => return Err(e),
            }
        }
    }

    /// `rows` into the window (opening it first when it isn't); `done`:
    /// rows of the load before them (for error messages).
    async fn insert(&mut self, spec: &LoadSpec, rows: &[Vec<Cell>], per_stmt: usize, done: u64) -> Result<()> {
        let pow = match self.pow {
            Some(p) => p,
            None if rows.iter().flatten().any(|c| matches!(c, Cell::Float(_))) => self.has_pow().await?,
            None => false,
        };
        let begin = !self.open;
        let (req, first) = insert_request(spec, rows, begin, per_stmt, pow)?;
        self.open = true;
        let r = self.run(&req, begin).await?;
        if let Some((i, e)) = r.step_errors.into_iter().enumerate().find_map(|(i, e)| e.map(|e| (i, e))) {
            let msg = e.text();
            return Err(if i >= first {
                let row = done + ((i - first) * per_stmt) as u64 + 1;
                Error::Query(format!("filas desde la {row}: {msg}"))
            } else {
                Error::Query(msg)
            });
        }
        Ok(())
    }

    /// Whether the server has `pow` (SQLite's math functions are a build
    /// option), asked once, with the load's first float.
    async fn has_pow(&mut self) -> Result<bool> {
        let mut steps = Vec::new();
        push_step(&mut steps, "SELECT pow(2, -1074)", "");
        let req = format!(r#"{{"type":"batch","batch":{{"steps":[{}]}}}}"#, steps.join(","));
        let r = self.run(&req, !self.open).await?;
        let pow = match r.step_errors.into_iter().flatten().next() {
            None => true,
            Some(e) => {
                let msg = e.text();
                if !msg.contains("no such function") {
                    return Err(Error::Query(msg));
                }
                false
            }
        };
        self.pow = Some(pow);
        Ok(pow)
    }

    /// Rolls the window back after an error, on the window's own stream.
    /// When a request's reply was lost (cut, or undecodable) the request
    /// may have run and moved the stream on: the client's baton is then
    /// stale, and a `ROLLBACK` through the client would land on a new
    /// stream while the window stays open on the old one, holding the
    /// write lock until the server expires it. So the `ROLLBACK` goes with
    /// the baton as it is, never on another stream, and when it can't
    /// reach the window's stream the load waits for that transaction to
    /// be gone ([`wait_unlocked`]) before it returns.
    async fn abort(&mut self) {
        self.open = false;
        let reached = match self.client.baton().map(str::to_string) {
            Some(b) => {
                let rollback = execute_req("ROLLBACK", false);
                match post(Wire::of(self.client), pipeline_body(Some(&b), &[&rollback]), true, parse_batch).await {
                    // Rolled back, or no transaction was left (a failed
                    // statement can end it): either way, the stream's own.
                    Ok(reply) => {
                        self.client.set_stream(reply.baton, reply.base_url.as_deref());
                        true
                    }
                    Err(_) => {
                        self.client.set_stream(None, None);
                        false
                    }
                }
            }
            None => false,
        };
        if !reached {
            wait_unlocked(Wire::of(self.client)).await;
        }
    }

    /// Closes the window: a request with only `COMMIT`, sent once all its
    /// rows were acknowledged.
    async fn commit(&mut self) -> Result<()> {
        let mut steps = Vec::new();
        push_step(&mut steps, "COMMIT", "");
        let req = format!(r#"{{"type":"batch","batch":{{"steps":[{}]}}}}"#, steps.join(","));
        let r = self.run(&req, false).await?;
        if let Some(e) = r.step_errors.into_iter().flatten().next() {
            return Err(Error::Query(e.text()));
        }
        self.open = false;
        Ok(())
    }
}

/// How long a failed load waits for a window it couldn't roll back.
const ORPHAN_WAIT: std::time::Duration = std::time::Duration::from_secs(30);

/// Waits, up to [`ORPHAN_WAIT`], until no transaction holds the write
/// lock: a `BEGIN IMMEDIATE` on a stream of its own (rolled back and
/// closed at once) succeeds only then. Stops as soon as the server
/// answers otherwise (another error, or no answer).
async fn wait_unlocked(w: Wire) {
    let deadline = std::time::Instant::now() + ORPHAN_WAIT;
    let reqs = [execute_req("BEGIN IMMEDIATE", false), execute_req("ROLLBACK", false), r#"{"type":"close"}"#.to_string()];
    let reqs: Vec<&str> = reqs.iter().map(String::as_str).collect();
    loop {
        let Ok(reply) = post(w.clone(), pipeline_body(None, &reqs), false, parse_batch).await else { return };
        let Some(Err(msg)) = reply.results.into_iter().next().map(Entry::into_result) else { return };
        let msg = msg.to_ascii_lowercase();
        if !(msg.contains("locked") || msg.contains("busy")) || std::time::Instant::now() >= deadline {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(200)).await;
    }
}

impl Drop for Loader<'_> {
    fn drop(&mut self) {
        let inflight = self.inflight.take();
        if inflight.is_none() && !self.open {
            return;
        }
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        // With a request in flight, the client's baton is stale; either
        // way this stream is left to the rollback below and the session's
        // next request starts a new one.
        let baton = self.client.baton().map(str::to_string);
        let wire = Wire::of(self.client);
        self.client.set_stream(None, None);
        rt.spawn(async move {
            let baton = match inflight {
                Some(h) => match h.await {
                    Ok(Ok(reply)) => reply.baton,
                    _ => None,
                },
                None => baton,
            };
            if let Some(b) = baton {
                let rollback = execute_req("ROLLBACK", false);
                let body = pipeline_body(Some(&b), &[&rollback, r#"{"type":"close"}"#]);
                let _ = post(wire, body, true, parse_nothing).await;
            }
        });
    }
}

pub(crate) async fn bulk_load(s: &mut LibsqlSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if s.read_only {
        return Err(Error::Query("La conexión es de solo lectura: no se permite cargar datos.".into()));
    }
    let ncols = spec.columns.len();
    if ncols == 0 {
        return Err(Error::Query("la carga no tiene columnas".into()));
    }
    // Up to two parameters per cell (a float as `m`, `e`).
    let per_stmt = ROWS_PER_STMT.min(MAX_PARAMS / (2 * ncols)).max(1);
    let max_rows = if spec.commit_rows == 0 { u64::MAX } else { spec.commit_rows };
    let max_bytes = if spec.commit_bytes == 0 { u64::MAX } else { spec.commit_bytes };
    let (mut total, mut rows, mut bytes) = (0u64, 0u64, 0u64);
    let mut ld = Loader { client: &mut s.client, inflight: None, open: false, pow: None };
    let r: Result<()> = async {
        while let Some(batch) = source.next().await {
            let mut start = 0;
            for (i, row) in batch.rows.iter().enumerate() {
                if row.len() != ncols {
                    return Err(Error::Query(format!("una fila trae {} valores y la carga tiene {ncols} columnas", row.len())));
                }
                rows += 1;
                bytes += row.iter().map(Cell::size).sum::<usize>() as u64;
                if rows >= max_rows || bytes >= max_bytes {
                    ld.insert(spec, &batch.rows[start..=i], per_stmt, total + rows - (i + 1 - start) as u64).await?;
                    ld.commit().await?;
                    total += rows;
                    (rows, bytes) = (0, 0);
                    start = i + 1;
                    progress(total);
                }
            }
            if start < batch.rows.len() {
                let pending = &batch.rows[start..];
                ld.insert(spec, pending, per_stmt, total + rows - pending.len() as u64).await?;
            }
        }
        if ld.open {
            ld.commit().await?;
            total += rows;
            progress(total);
        }
        Ok(())
    }
    .await;
    if let Err(e) = r {
        if ld.open {
            // Whatever the window had (the failing request's steps already
            // stopped at its error).
            ld.abort().await;
        }
        return Err(e);
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cells(json: &str) -> Vec<Vec<Cell>> {
        let (reply, _) = parse_rows(json.as_bytes()).unwrap();
        let rows = reply.results.into_iter().next().unwrap().into_result().unwrap().unwrap();
        rows.rows.into_iter().map(|r| r.into_iter().map(|h| h.0).collect()).collect()
    }

    #[test]
    fn values_decode() {
        let body = r#"{"baton":"b","base_url":null,"results":[{"type":"ok","response":{"type":"execute","result":{"cols":[{"name":"a","decltype":"INTEGER"}],"rows":[[{"type":"integer","value":"-9223372036854775808"},{"type":"null"},{"type":"text","value":"ñ {\"type\":\"float\",\"value\":1}"},{"type":"blob","base64":"AP8"},{"type":"blob","base64":""},{"type":"float","value":-2.5}]],"affected_row_count":0,"query_duration_ms":0.25}}}]}"#;
        assert_eq!(
            cells(body),
            vec![vec![
                Cell::Int(i64::MIN),
                Cell::Null,
                Cell::Text(r#"ñ {"type":"float","value":1}"#.into()),
                Cell::Bytes(vec![0, 255]),
                Cell::Bytes(vec![]),
                Cell::Float(-2.5),
            ]]
        );
    }

    /// serde_json's default parse gives 1.0715660391465823e-75 for this
    /// text (one ULP off); the cell must hold the value the text names.
    #[test]
    fn floats_are_exact() {
        let texts = ["1.0715660391465826e-75", "5e-324", "1.7976931348623157e308", "-0.0", "0.1", "2.2250738585072014e-308"];
        let row = texts.iter().map(|t| format!(r#"{{"type":"float","value":{t}}}"#)).collect::<Vec<_>>().join(",");
        let body = format!(r#"{{"baton":null,"results":[{{"type":"ok","response":{{"type":"execute","result":{{"cols":[],"rows":[[{row}]]}}}}}}]}}"#);
        let got = cells(&body).remove(0);
        for (t, c) in texts.iter().zip(got) {
            let Cell::Float(f) = c else { panic!("{c:?}") };
            assert_eq!(f.to_bits(), t.parse::<f64>().unwrap().to_bits(), "{t}");
        }
        // Random bit patterns: every one exact.
        let mut x = 0x9E37_79B9_7F4A_7C15u64;
        let vals: Vec<f64> = (0..20_000)
            .filter_map(|_| {
                x ^= x << 13;
                x ^= x >> 7;
                x ^= x << 17;
                Some(f64::from_bits(x)).filter(|f| f.is_finite())
            })
            .collect();
        let row = vals.iter().map(|f| format!(r#"{{"type":"float","value":{}}}"#, serde_json::to_string(f).unwrap())).collect::<Vec<_>>().join(",");
        let body = format!(r#"{{"results":[{{"type":"ok","response":{{"result":{{"rows":[[{row}]]}}}}}}]}}"#);
        let got = cells(&body).remove(0);
        for (v, c) in vals.iter().zip(got) {
            assert_eq!(c, Cell::Float(*v));
            let Cell::Float(f) = c else { unreachable!() };
            assert_eq!(f.to_bits(), v.to_bits());
        }
    }

    #[test]
    fn infinity_without_value_is_marked() {
        let body = r#"{"results":[{"type":"ok","response":{"result":{"rows":[[{"type":"float","value":null},{"type":"float","value":1.5}]]}}}]}"#;
        let row = cells(body).remove(0);
        assert!(matches!(row[0], Cell::Float(f) if f.is_nan()));
        assert_eq!(row[1], Cell::Float(1.5));
    }

    /// 2^e, exactly (e in [-1074, 1023]).
    fn pow2(e: i32) -> f64 {
        if e >= -1022 {
            f64::from_bits(((e + 1023) as u64) << 52)
        } else {
            f64::from_bits(1u64 << (e + 1074))
        }
    }

    #[test]
    fn float_parts_are_exact() {
        assert!(matches!(float_parts(f64::NAN), Err(Error::Unsupported(_))));
        assert_eq!(float_parts(f64::INFINITY).unwrap(), (1, 2000));
        assert_eq!(float_parts(f64::NEG_INFINITY).unwrap(), (-1, 2000));
        assert_eq!(float_parts(0.0).unwrap(), (0, 0));
        assert_eq!(float_parts(-0.0).unwrap(), (-1, -2000));
        assert_eq!(float_parts(1.5).unwrap(), (3, -1));
        assert_eq!(float_parts(-8.0).unwrap(), (-1, 3));
        assert_eq!(float_parts(5e-324).unwrap(), (1, -1074));
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let f = f64::from_bits(x);
            if !f.is_finite() || f == 0.0 {
                continue;
            }
            let (m, e) = float_parts(f).unwrap();
            assert!(m % 2 != 0 && m.unsigned_abs() < 1 << 53 && (-1074..=1023).contains(&e), "{f:e}: {m} {e}");
            // One product, as the server does it (pow(2, e) is exact).
            assert_eq!((m as f64 * pow2(e)).to_bits(), f.to_bits(), "{f:e}");
        }
    }

    #[test]
    fn real_expressions() {
        assert_eq!(real_sql(f64::INFINITY, true).unwrap(), "9e999");
        assert_eq!(real_sql(f64::NEG_INFINITY, false).unwrap(), "-9e999");
        assert_eq!(real_sql(-0.0, true).unwrap(), "-0.0");
        assert!(matches!(real_sql(f64::NAN, true), Err(Error::Unsupported(_))));
        assert_eq!(real_sql(1.5, true).unwrap(), "3*pow(2,-1)");
        assert_eq!(real_sql(5e-324, true).unwrap(), "1*pow(2,-1074)");
        assert_eq!(real_sql(1.5, false).unwrap(), "CAST(3 AS REAL)/2");
        assert_eq!(real_sql(-8.0, false).unwrap(), "CAST(-1 AS REAL)*8");
        assert_eq!(real_sql(1.0, false).unwrap(), "CAST(1 AS REAL)");
        // Without pow, 5e-324 = 1 · 2^-1074: 17 divisions by 2^62 and one by 2^20.
        let tiny = real_sql(5e-324, false).unwrap();
        assert_eq!(tiny.matches("/4611686018427387904").count(), 17);
        assert!(tiny.ends_with("/1048576"));
        // The chain evaluated in f64 (as SQLite does) gives the value back.
        let mut x = 0x2545_F491_4F6C_DD1Du64;
        for _ in 0..20_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let f = f64::from_bits(x);
            if !f.is_finite() || f == 0.0 {
                continue;
            }
            let s = real_sql(f, false).unwrap();
            let (cast, ops) = s.strip_prefix("CAST(").unwrap().split_once(" AS REAL)").unwrap();
            let mut v = cast.parse::<i64>().unwrap() as f64;
            let mut rest = ops;
            while !rest.is_empty() {
                let op = rest.as_bytes()[0];
                let end = rest[1..].find(['*', '/']).map_or(rest.len(), |e| e + 1);
                let p = rest[1..end].parse::<u64>().unwrap() as f64;
                v = if op == b'*' { v * p } else { v / p };
                rest = &rest[end..];
            }
            assert_eq!(v.to_bits(), f.to_bits(), "{f:e}: {s}");
        }
    }

    fn spec(cols: &[&str]) -> LoadSpec {
        LoadSpec {
            table: dbine_driver::ObjectRef { kind: "table".into(), schema: None, name: "t\"x".into() },
            columns: cols.iter().map(|c| c.to_string()).collect(),
            table_lock: false,
            keep_identity: false,
            commit_rows: 10,
            commit_bytes: 10,
        }
    }

    #[test]
    fn insert_request_is_json() {
        let spec = spec(&["a", "b"]);
        let rows = vec![
            vec![Cell::Float(f64::INFINITY), Cell::Text("x\"','y".into())],
            vec![Cell::Bytes(vec![1, 2]), Cell::UInt(u64::MAX)],
            vec![Cell::Null, Cell::Bool(true)],
        ];
        let (req, first) = insert_request(&spec, &rows, true, 2, false).unwrap();
        assert_eq!(first, 1);
        let v: Value = serde_json::from_str(&req).unwrap();
        let steps = v["batch"]["steps"].as_array().unwrap();
        assert_eq!(steps.len(), 3);
        assert_eq!(steps[0]["stmt"]["sql"], "BEGIN IMMEDIATE");
        assert_eq!(steps[1]["stmt"]["sql"], r#"INSERT INTO "t""x" ("a", "b") VALUES (9e999, ?), (?, ?)"#);
        assert_eq!(steps[1]["condition"]["step"], 0);
        assert_eq!(
            steps[1]["stmt"]["args"],
            serde_json::json!([
                {"type": "text", "value": "x\"','y"},
                {"type": "blob", "base64": "AQI="},
                {"type": "text", "value": u64::MAX.to_string()},
            ])
        );
        assert_eq!(steps[2]["stmt"]["args"], serde_json::json!([{"type": "null"}, {"type": "integer", "value": "1"}]));
        assert!(matches!(insert_request(&spec, &[vec![Cell::Float(f64::NAN), Cell::Null]], false, 2, false), Err(Error::Unsupported(_))));
        assert!(matches!(insert_request(&spec, &[vec![Cell::Float(f64::NAN), Cell::Null]], false, 2, true), Err(Error::Unsupported(_))));
    }

    /// With `pow`, a float column goes as (m, e) parameters and the value
    /// is rebuilt once per column: the `VALUES` list has no expressions.
    #[test]
    fn floats_bind_in_pairs() {
        let spec = spec(&["a", "r", "s"]);
        let rows = vec![
            vec![Cell::Int(1), Cell::Float(1.5), Cell::Text("x".into())],
            vec![Cell::Int(2), Cell::Text("not a float".into()), Cell::Null],
            vec![Cell::Int(3), Cell::Float(-0.0), Cell::Null],
            vec![Cell::Int(4), Cell::Float(f64::NEG_INFINITY), Cell::Null],
            // A statement without floats stays a plain VALUES.
            vec![Cell::Int(5), Cell::Null, Cell::Null],
        ];
        let (req, first) = insert_request(&spec, &rows, false, 4, true).unwrap();
        assert_eq!(first, 0);
        let v: Value = serde_json::from_str(&req).unwrap();
        let steps = v["batch"]["steps"].as_array().unwrap();
        assert_eq!(
            steps[0]["stmt"]["sql"],
            r#"INSERT INTO "t""x" ("a", "r", "s") SELECT column1, CASE WHEN column3 IS NULL THEN column2 ELSE column2*pow(2,column3) END, column4 FROM (VALUES (?, ?, ?, ?), (?, ?, ?, ?), (?, ?, ?, ?), (?, ?, ?, ?))"#
        );
        let int = |i: i64| serde_json::json!({"type": "integer", "value": i.to_string()});
        let nul = serde_json::json!({"type": "null"});
        assert_eq!(
            steps[0]["stmt"]["args"],
            serde_json::json!([
                int(1), int(3), int(-1), {"type": "text", "value": "x"},
                int(2), {"type": "text", "value": "not a float"}, nul, nul,
                int(3), int(-1), int(-2000), nul,
                int(4), int(-1), int(2000), nul,
            ])
        );
        assert_eq!(steps[1]["stmt"]["sql"], r#"INSERT INTO "t""x" ("a", "r", "s") VALUES (?, ?, ?)"#);

        // Pairs that would pass SQLite's column limit: an expression per float.
        let names: Vec<String> = (0..1500).map(|i| format!("c{i}")).collect();
        let wide = spec_owned(names);
        let row: Vec<Cell> = (0..1500).map(|i| Cell::Float(i as f64 + 0.5)).collect();
        let (req, _) = insert_request(&wide, &[row], false, 1, true).unwrap();
        let v: Value = serde_json::from_str(&req).unwrap();
        let sql = v["batch"]["steps"][0]["stmt"]["sql"].as_str().unwrap();
        assert!(sql.contains(" VALUES (1*pow(2,-1), 3*pow(2,-1), "), "{}", &sql[..200]);
        assert!(!sql.contains("SELECT"));
    }

    fn spec_owned(columns: Vec<String>) -> LoadSpec {
        LoadSpec { columns, ..spec(&[]) }
    }

    #[test]
    fn missing_rowid_message() {
        assert!(no_such("SQL input error: no such column: rowid (at offset 7)", "rowid"));
        assert!(no_such("no such column: rowid", "rowid"));
        assert!(!no_such("no such column: rowid_x", "rowid"));
        assert!(!no_such("no such column: \"t\".\"nope\"", "rowid"));
    }
}
