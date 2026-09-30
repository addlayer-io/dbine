//! Bulk transfer (see `dbine_driver::transfer`).
//!
//! Reading: `/v3/kv/range` pages, each one starting after the last key of
//! the one before, all at the first page's revision (one consistent
//! snapshot). etcd can't bound a page by bytes, only by keys, so a reply
//! is never held whole: it's read as it arrives and each key-value becomes
//! a row as soon as its JSON object closes ([`KvScanner`]), so what a read
//! holds is one key-value and the batch being filled, whatever the page.
//! The page size still adapts, to spare the server huge replies: the first
//! asks for [`PAGE_FIRST`] keys, and each next one for as many as fit in
//! [`PAGE_BYTES`] at the largest key-value of the page before (at most
//! twice the keys of the page before, and [`PAGE_MAX`]).
//! Rows have browsing's shape (`key, value, create_revision, mod_revision,
//! version, lease`). A name ending in `/` reads the keys under it; any
//! other name reads that key or, when it doesn't exist, the keys under
//! `<name>/` (what a load into it writes).
//!
//! Loading: rows become puts in two ways.
//! - Rows with only etcd's own columns, `key` and `value` among them (what
//!   a read gives): each one is its key. A key already under the target
//!   (or the target itself) goes as it is; any other goes under the target
//!   (`/a/src/0` into `/a/dst/` is `/a/dst/a/src/0`), so a load never
//!   writes outside its target. A `lease` is kept (the lease has to exist
//!   in the target cluster); revisions and versions are the server's. A
//!   null or empty key, or a null value, is an error: etcd has neither.
//!   Values go byte for byte: text and JSON as they come (a JSON document
//!   is never re-encoded), binaries whole.
//! - Any other rows: `<target>/<first column>`, the row as a JSON object
//!   with its keys in the columns' order.
//!   Values keep their type: numbers as numbers, JSON nested as its own
//!   text (checked, never re-encoded: big numbers and key order kept), binaries as
//!   `{"$binary": {"base64": …, "subType": "00"}}` and non-finite floats as
//!   `{"$numberDouble": …}` (MongoDB's Extended JSON), the rest as text. A
//!   null first column is an error.
//!
//! Puts go in transactions of up to [`TXN_OPS`] puts (etcd's default
//! `--max-txn-ops`; a server with fewer gets smaller ones), [`TXN_BYTES`]
//! and the load's commit window, with at most [`IN_FLIGHT`] transactions
//! and [`IN_FLIGHT_BYTES`] in flight. Each transaction only puts when none
//! of its keys exists (`create_revision = 0`): a load never overwrites a
//! key, so a key that already exists or repeats in the load fails it (the
//! source, on the same cluster, is never written). Whatever ends a load (an
//! error, a cancel, its future dropped), the transactions already sent are
//! awaited before it returns, so nothing commits after it.

use crate::command::parse_lease;
use crate::{b64, int, is_auth_error, lease_hex, unb64, Cancel, Conn, EtcdSession};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use tokio::task::JoinSet;

/// Keys asked for by a read's first page.
const PAGE_FIRST: usize = 16;
/// Keys per range page, at most.
const PAGE_MAX: usize = 1000;
/// Keys and values a range page aims for (the server builds each reply
/// whole; the client reads it as it arrives, a key-value at a time).
const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// A range reply's bytes outside its key-values (header, `more`, `count`),
/// at most: a reply with more is not what the gateway sends.
const REPLY_REST_MAX: usize = 64 * 1024;
/// Puts per transaction: etcd's default `--max-txn-ops`.
const TXN_OPS: usize = 128;
/// Bytes per transaction, at most, counting each key three times (its
/// put, its compare and its lookup on failure); etcd's default
/// `--max-request-bytes` is 1.5 MiB.
const TXN_BYTES: usize = 1024 * 1024;
/// Transactions sent and not yet answered, at most…
const IN_FLIGHT: usize = 16;
/// …and their bytes (each one is held as its puts and as the request's
/// base64 body while in flight: under ~2.5× this in memory).
const IN_FLIGHT_BYTES: usize = 2 * 1024 * 1024;

const COLUMNS: &[(&str, &str)] =
    &[("key", "bytes"), ("value", "bytes"), ("create_revision", "int64"), ("mod_revision", "int64"), ("version", "int64"), ("lease", "lease")];

fn text_or_bytes(b: Vec<u8>) -> Cell {
    match String::from_utf8(b) {
        Ok(s) => Cell::Text(s),
        Err(e) => Cell::Bytes(e.into_bytes()),
    }
}

/// A range's key-value as a row, typed.
fn kv_cells(kv: &Value) -> Vec<Cell> {
    let lease = int(kv.get("lease"));
    vec![
        text_or_bytes(unb64(kv.get("key"))),
        text_or_bytes(unb64(kv.get("value"))),
        Cell::Int(int(kv.get("create_revision"))),
        Cell::Int(int(kv.get("mod_revision"))),
        Cell::Int(int(kv.get("version"))),
        if lease == 0 { Cell::Null } else { Cell::Text(lease_hex(lease)) },
    ]
}

/// The keys the page after one of `limit` keys asks for, when its largest
/// key-value took `largest` bytes.
fn next_page(limit: usize, largest: usize) -> usize {
    (PAGE_BYTES / largest.max(1)).clamp(1, (limit * 2).min(PAGE_MAX))
}

/// Splits a `/v3/kv/range` reply, fed as it arrives, into its key-values
/// (each one handed over as soon as its object closes) and the rest (the
/// reply with each key-value as `0`, parsed at the end). Only one
/// key-value is held at a time.
#[derive(Default)]
struct KvScanner {
    depth: usize,
    in_str: bool,
    escaped: bool,
    /// A string of the reply's top object (its keys, `count`'s value).
    top_str: Vec<u8>,
    in_kvs: bool,
    /// The key-value being read.
    kv: Option<Vec<u8>>,
    rest: Vec<u8>,
}

impl KvScanner {
    fn feed(&mut self, chunk: &[u8], on_kv: &mut (dyn FnMut(Value) -> Result<()> + Send)) -> Result<()> {
        for &b in chunk {
            // Strings first: brackets inside them are text.
            if self.in_str {
                if self.escaped {
                    self.escaped = false;
                } else if b == b'\\' {
                    self.escaped = true;
                } else if b == b'"' {
                    self.in_str = false;
                }
                match &mut self.kv {
                    Some(kv) => kv.push(b),
                    None => {
                        if self.depth == 1 && self.in_str {
                            self.top_str.push(b);
                        }
                        self.rest.push(b);
                    }
                }
                continue;
            }
            match b {
                b'"' => {
                    self.in_str = true;
                    if self.kv.is_none() && self.depth == 1 {
                        self.top_str.clear();
                    }
                }
                b'{' if self.in_kvs && self.depth == 2 && self.kv.is_none() => self.kv = Some(Vec::new()),
                b'[' if self.depth == 1 && self.kv.is_none() && self.top_str == b"kvs" => self.in_kvs = true,
                _ => {}
            }
            match b {
                b'{' | b'[' => self.depth += 1,
                b'}' | b']' => self.depth = self.depth.checked_sub(1).ok_or_else(bad_reply)?,
                _ => {}
            }
            match &mut self.kv {
                Some(kv) => {
                    kv.push(b);
                    if self.depth == 2 {
                        let kv = self.kv.take().unwrap_or_default();
                        on_kv(serde_json::from_slice(&kv).map_err(|_| bad_reply())?)?;
                        self.rest.push(b'0');
                    }
                }
                None => {
                    if b == b']' && self.in_kvs && self.depth == 1 {
                        self.in_kvs = false;
                    }
                    self.rest.push(b);
                    if self.rest.len() > REPLY_REST_MAX {
                        return Err(bad_reply());
                    }
                }
            }
        }
        Ok(())
    }

    /// The reply without its key-values.
    fn finish(self) -> Result<Value> {
        if self.kv.is_some() || self.depth != 0 {
            return Err(bad_reply());
        }
        serde_json::from_slice(&self.rest).map_err(|_| bad_reply())
    }
}

fn bad_reply() -> Error {
    Error::Query("etcd devolvió una respuesta de rango que no se pudo leer (JSON incompleto o inesperado).".into())
}

/// One `/v3/kv/range` call, its key-values handed to `on_kv` as they
/// arrive; the rest of the reply (header, `more`). On an expired token it
/// authenticates and retries once (an error comes before any key-value).
async fn range_page(conn: &Conn, body: &Value, on_kv: &mut (dyn FnMut(Value) -> Result<()> + Send)) -> Result<Value> {
    let mut retried = false;
    loop {
        let mut rb = conn.http.post(format!("{}/v3/kv/range", conn.base)).json(body);
        if let Some(t) = conn.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            rb = rb.header("Authorization", t);
        }
        let mut resp = rb.send().await.map_err(crate::http_error)?;
        let status = resp.status();
        if !status.is_success() {
            let text = resp.text().await.map_err(crate::http_error)?;
            let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text.trim()}));
            let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
            let msg = v.get("message").or_else(|| v.get("error")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"));
            if is_auth_error(code, &msg) {
                if conn.user.is_some() && !retried {
                    retried = true;
                    conn.authenticate().await?;
                    continue;
                }
                return Err(Error::AuthFailed(msg));
            }
            return Err(Error::Query(msg));
        }
        let mut scan = KvScanner::default();
        while let Some(chunk) = resp.chunk().await.map_err(crate::http_error)? {
            scan.feed(&chunk, on_kv)?;
        }
        let rest = scan.finish()?;
        if let Some(e) = rest.get("error") {
            return Err(Error::Query(e.as_str().map(str::to_string).unwrap_or_else(|| e.to_string())));
        }
        return Ok(rest);
    }
}

/// Keys from `from` to `end` (exclusive), a page at a time.
async fn read_range(s: &EtcdSession, from: Vec<u8>, end: Vec<u8>, pick: &Option<Vec<usize>>, sink: &BatchSinkRef) -> Result<u64> {
    let mut builder = BatchBuilder::new();
    let mut from = from;
    let mut revision: Option<i64> = None;
    let mut limit = PAGE_FIRST;
    loop {
        let mut body = json!({"key": b64(&from), "range_end": b64(&end), "limit": limit.to_string()});
        if let Some(r) = revision {
            body["revision"] = json!(r.to_string());
        }
        let mut last: Option<Vec<u8>> = None;
        let mut largest = 0;
        let mut on_kv = |kv: Value| -> Result<()> {
            let row = kv_cells(&kv);
            drop(kv);
            largest = largest.max(row[0].size() + row[1].size());
            last = Some(match &row[0] {
                Cell::Text(t) => t.as_bytes().to_vec(),
                Cell::Bytes(b) => b.clone(),
                _ => Vec::new(),
            });
            let row = match pick {
                None => row,
                Some(p) => p.iter().map(|&i| row[i].clone()).collect(),
            };
            let mut sink = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
            builder.push(row, &mut *sink)?;
            Ok(())
        };
        let r = s.cancel.run(range_page(&s.conn, &body, &mut on_kv)).await?;
        revision.get_or_insert(int(r.pointer("/header/revision")));
        let more = r.get("more").and_then(Value::as_bool) == Some(true);
        let Some(last) = last else { break };
        if !more {
            break;
        }
        from = last;
        from.push(0);
        limit = next_page(limit, largest);
    }
    let mut sink = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
    builder.flush(&mut *sink)?;
    Ok(builder.rows)
}

pub async fn read(s: &mut EtcdSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    if spec.filter.is_some() {
        return Err(Error::Unsupported("etcd lee por clave o prefijo: no filtra la lectura por lotes".into()));
    }
    s.cancel.flag.store(false, Ordering::SeqCst);
    let pick = match &spec.columns {
        None => None,
        Some(w) => Some(
            w.iter()
                .map(|c| COLUMNS.iter().position(|(n, _)| n.eq_ignore_ascii_case(c)).ok_or_else(|| Error::Query(format!("etcd no tiene la columna {c}"))))
                .collect::<Result<Vec<_>>>()?,
        ),
    };
    let cols: Vec<TransferColumn> = match &pick {
        None => COLUMNS.iter().map(|(n, t)| column(n, t)).collect(),
        Some(p) => p.iter().map(|&i| column(COLUMNS[i].0, COLUMNS[i].1)).collect(),
    };
    sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
    let name = spec.table.name.as_bytes();
    let prefix = |p: &[u8]| {
        let end = crate::command::prefix_end(p);
        (if p.is_empty() { vec![0] } else { p.to_vec() }, end)
    };
    if name.ends_with(b"/") {
        let (from, end) = prefix(name);
        return read_range(s, from, end, &pick, &sink).await;
    }
    let exact = s.call("/v3/kv/range", json!({"key": b64(name), "count_only": true})).await?;
    if int(exact.get("count")) > 0 {
        let mut end = name.to_vec();
        end.push(0);
        return read_range(s, name.to_vec(), end, &pick, &sink).await;
    }
    let (from, end) = prefix(format!("{}/", spec.table.name).as_bytes());
    read_range(s, from, end, &pick, &sink).await
}

fn column(name: &str, type_name: &str) -> TransferColumn {
    TransferColumn { name: name.into(), type_name: type_name.into(), nullable: name == "lease" }
}

/// A float as JSON text: the shortest one that reads back the same
/// number; `None` for NaN and the infinities.
fn float_text(f: f64) -> Option<String> {
    serde_json::Number::from_f64(f).map(|n| n.to_string())
}

/// A key or a value's bytes: text and JSON documents as they come (never
/// re-encoded), binaries whole, numbers and booleans as their text. Never
/// called on a null.
fn text(c: &Cell) -> Vec<u8> {
    match c {
        Cell::Bytes(b) => b.clone(),
        Cell::Json(s) | Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => {
            s.as_bytes().to_vec()
        }
        Cell::Bool(b) => b.to_string().into_bytes(),
        Cell::Int(i) => i.to_string().into_bytes(),
        Cell::UInt(u) => u.to_string().into_bytes(),
        Cell::Float(f) => float_text(*f).unwrap_or_else(|| f.to_string()).into_bytes(),
        Cell::Null => Vec::new(),
    }
}

/// Appends a cell to a row's JSON object text, keeping its type (see the
/// module). A JSON document goes as its own text once checked: parsing it
/// into a `Value` would round big numbers and long decimals to `f64` and
/// sort its keys.
fn json_value(column: &str, c: &Cell, out: &mut String) -> Result<()> {
    match c {
        Cell::Null => out.push_str("null"),
        Cell::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        Cell::Int(i) => out.push_str(&i.to_string()),
        Cell::UInt(u) => out.push_str(&u.to_string()),
        Cell::Float(f) => match float_text(*f) {
            Some(n) => out.push_str(&n),
            None => {
                let s = if f.is_nan() {
                    "NaN"
                } else if *f > 0.0 {
                    "Infinity"
                } else {
                    "-Infinity"
                };
                out.push_str(&json!({"$numberDouble": s}).to_string());
            }
        },
        Cell::Bytes(b) => out.push_str(&json!({"$binary": {"base64": b64(b), "subType": "00"}}).to_string()),
        Cell::Json(s) => {
            // Only checked (one JSON value, nothing around it): what goes
            // in is the text itself.
            serde_json::from_str::<Value>(s).map_err(|e| Error::Query(format!("La columna «{column}» trae un JSON inválido: {e}")))?;
            out.push_str(s);
        }
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => {
            out.push_str(&serde_json::to_string(s.as_str()).map_err(Error::query)?)
        }
    }
    Ok(())
}

fn shown(b: &[u8]) -> String {
    String::from_utf8_lossy(b).into_owned()
}

/// One put of a load.
#[derive(Debug, PartialEq)]
pub struct Put {
    pub key: Vec<u8>,
    pub value: Vec<u8>,
    /// 0: none.
    pub lease: i64,
}

/// How rows become keys: by their `key` / `value` columns, or as JSON
/// objects under the target.
pub enum Rows {
    KeyValue { base: Vec<u8>, key: usize, value: usize, lease: Option<usize> },
    Json { base: String, columns: Vec<String> },
}

impl Rows {
    pub fn new(target: &str, columns: &[String]) -> Result<Rows> {
        let pos = |n: &str| columns.iter().position(|c| c.eq_ignore_ascii_case(n));
        let own = columns.iter().all(|c| COLUMNS.iter().any(|(n, _)| c.eq_ignore_ascii_case(n)));
        let base = target.trim_end_matches('/').to_string();
        if let Some(c) = columns.iter().enumerate().find_map(|(i, c)| columns[..i].contains(c).then_some(c)) {
            return Err(Error::Query(format!("La columna «{c}» se repite: cada fila se guarda como un objeto JSON y sus claves no pueden repetirse.")));
        }
        if let (true, Some(key), Some(value)) = (own, pos("key"), pos("value")) {
            return Ok(Rows::KeyValue { base: base.into_bytes(), key, value, lease: pos("lease") });
        }
        if columns.is_empty() {
            return Err(Error::Query("No hay columnas para armar las claves.".into()));
        }
        Ok(Rows::Json { base, columns: columns.to_vec() })
    }

    /// A row's put.
    pub fn put(&self, row: &[Cell]) -> Result<Put> {
        match self {
            Rows::KeyValue { base, key, value, lease } => {
                let k = match row.get(*key) {
                    None | Some(Cell::Null) => return Err(Error::Query("Una fila viene sin clave (la columna key es nula): etcd no tiene claves nulas.".into())),
                    Some(c) => text(c),
                };
                if k.is_empty() {
                    return Err(Error::Query("Una fila viene con la clave vacía: etcd no admite claves vacías.".into()));
                }
                let v = match row.get(*value) {
                    None | Some(Cell::Null) => {
                        return Err(Error::Unsupported(format!(
                            "La clave «{}» viene con valor nulo: etcd no tiene valores nulos (una clave siempre tiene un valor, aunque sea vacío).",
                            shown(&k)
                        )))
                    }
                    Some(c) => text(c),
                };
                let lease = match lease.and_then(|i| row.get(i)) {
                    None | Some(Cell::Null) => 0,
                    Some(Cell::Int(i)) => *i,
                    Some(Cell::UInt(u)) => *u as i64,
                    Some(Cell::Text(t)) => parse_lease(t).map_err(Error::Query)?,
                    Some(other) => return Err(Error::Query(format!("La clave «{}» trae un lease que no es un id: {other:?}", shown(&k)))),
                };
                Ok(Put { key: placed(base, k), value: v, lease })
            }
            Rows::Json { base, columns } => {
                let first = match row.first() {
                    None | Some(Cell::Null) => {
                        return Err(Error::Query(format!(
                            "La primera columna («{}») es la clave de cada fila en etcd y viene nula en una fila.",
                            columns[0]
                        )))
                    }
                    Some(c) => c,
                };
                let mut k = format!("{base}/").into_bytes();
                k.extend(text(first));
                let mut obj = String::from("{");
                for (i, (c, v)) in columns.iter().zip(row).enumerate() {
                    if i > 0 {
                        obj.push(',');
                    }
                    obj.push_str(&serde_json::to_string(c.as_str()).map_err(Error::query)?);
                    obj.push(':');
                    json_value(c, v, &mut obj)?;
                }
                obj.push('}');
                Ok(Put { key: k, value: obj.into_bytes(), lease: 0 })
            }
        }
    }
}

/// A read key's place under the target `base` (no trailing `/`): as it is
/// when it's already there, else under it.
fn placed(base: &[u8], key: Vec<u8>) -> Vec<u8> {
    let under = |k: &[u8]| k == base || (k.starts_with(base) && k.get(base.len()) == Some(&b'/'));
    if base.is_empty() || under(&key) {
        return key;
    }
    let mut k = base.to_vec();
    if !key.starts_with(b"/") {
        k.push(b'/');
    }
    k.extend(key);
    k
}

/// The transaction being put together: its puts, each one only when its
/// key doesn't exist, and a lookup of each key for when one does. Held as
/// the puts themselves; the request's JSON is written when it's sent.
#[derive(Default)]
struct Pending {
    puts: Vec<Put>,
    keys: HashSet<Vec<u8>>,
    bytes: usize,
}

impl Pending {
    fn size(p: &Put) -> usize {
        3 * p.key.len() + p.value.len() + 64
    }

    fn len(&self) -> usize {
        self.puts.len()
    }

    fn is_empty(&self) -> bool {
        self.puts.is_empty()
    }

    fn add(&mut self, p: Put) -> Result<()> {
        if self.keys.contains(&p.key) {
            return Err(dup_error(&p.key));
        }
        self.keys.insert(p.key.clone());
        self.bytes += Self::size(&p);
        self.puts.push(p);
        Ok(())
    }

    fn body(self) -> Txn {
        Txn { puts: self.puts, bytes: self.bytes }
    }
}

fn dup_error(key: &[u8]) -> Error {
    Error::Query(format!(
        "La clave «{}» ya existe en el destino o se repite en la carga: una carga en etcd no pisa claves (en filas que no son clave/valor, la clave es la primera columna y tiene que ser única).",
        shown(key)
    ))
}

struct Txn {
    puts: Vec<Put>,
    bytes: usize,
}

impl Txn {
    fn rows(&self) -> u64 {
        self.puts.len() as u64
    }

    /// In halves.
    fn split(mut self) -> (Txn, Txn) {
        let second = self.puts.split_off(self.puts.len() / 2);
        let bytes = self.bytes / 2;
        (Txn { puts: self.puts, bytes }, Txn { puts: second, bytes: self.bytes - bytes })
    }

    /// The request: each put only when no key exists, else a lookup of
    /// each key. Written straight to its bytes (keys and values in base64,
    /// which never needs escaping), sized up front: no JSON tree, no
    /// growing buffer.
    fn body(&self) -> Vec<u8> {
        use base64::Engine as _;
        let b64len = |n: usize| n.div_ceil(3) * 4;
        let cap = 64 + self.puts.iter().map(|p| 3 * b64len(p.key.len()) + b64len(p.value.len()) + 200).sum::<usize>();
        let mut out = Vec::with_capacity(cap);
        let enc = |out: &mut Vec<u8>, b: &[u8]| {
            let at = out.len();
            out.resize(at + b64len(b.len()), 0);
            let n = base64::engine::general_purpose::STANDARD.encode_slice(b, &mut out[at..]).unwrap_or(0);
            out.truncate(at + n);
        };
        out.extend_from_slice(b"{\"compare\":[");
        for (i, p) in self.puts.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"{\"key\":\"");
            enc(&mut out, &p.key);
            out.extend_from_slice(b"\",\"target\":\"CREATE\",\"result\":\"EQUAL\",\"create_revision\":\"0\"}");
        }
        out.extend_from_slice(b"],\"success\":[");
        for (i, p) in self.puts.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"{\"request_put\":{\"key\":\"");
            enc(&mut out, &p.key);
            out.extend_from_slice(b"\",\"value\":\"");
            enc(&mut out, &p.value);
            out.push(b'"');
            if p.lease != 0 {
                out.extend_from_slice(format!(",\"lease\":\"{}\"", p.lease).as_bytes());
            }
            out.extend_from_slice(b"}}");
        }
        out.extend_from_slice(b"],\"failure\":[");
        for (i, p) in self.puts.iter().enumerate() {
            if i > 0 {
                out.push(b',');
            }
            out.extend_from_slice(b"{\"request_range\":{\"key\":\"");
            enc(&mut out, &p.key);
            out.extend_from_slice(b"\",\"count_only\":true}}");
        }
        out.extend_from_slice(b"]}");
        out
    }
}

/// A transaction's call (its body written again for a retry after
/// authenticating).
async fn post(conn: &Conn, t: &Txn) -> Result<Value> {
    let once = |body: Vec<u8>| async move {
        let mut rb = conn.http.post(format!("{}/v3/kv/txn", conn.base)).header("Content-Type", "application/json").body(body);
        if let Some(tok) = conn.token.lock().unwrap_or_else(|e| e.into_inner()).clone() {
            rb = rb.header("Authorization", tok);
        }
        let resp = rb.send().await.map_err(crate::http_error)?;
        let status = resp.status();
        let text = resp.text().await.map_err(crate::http_error)?;
        let v: Value = serde_json::from_str(&text).unwrap_or_else(|_| json!({"message": text.trim()}));
        if status.is_success() && v.get("error").is_none() {
            return Ok(Ok(v));
        }
        let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
        let msg = v.get("message").or_else(|| v.get("error")).and_then(Value::as_str).map(str::to_string).unwrap_or_else(|| format!("HTTP {status}"));
        Ok::<_, Error>(Err((code, msg)))
    };
    match once(t.body()).await? {
        Ok(v) => Ok(v),
        Err((code, msg)) if is_auth_error(code, &msg) && conn.user.is_some() => {
            conn.authenticate().await?;
            match once(t.body()).await? {
                Ok(v) => Ok(v),
                Err((code, msg)) if is_auth_error(code, &msg) => Err(Error::AuthFailed(msg)),
                Err((_, msg)) => Err(Error::Query(msg)),
            }
        }
        Err((code, msg)) if is_auth_error(code, &msg) => Err(Error::AuthFailed(msg)),
        Err((_, msg)) => Err(Error::Query(msg)),
    }
}

/// One transaction; when the server allows fewer operations per
/// transaction, it lowers `max_ops` and goes in halves. Its rows.
fn txn(conn: Arc<Conn>, t: Txn, max_ops: Arc<AtomicUsize>) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<u64>> + Send>> {
    Box::pin(async move {
        match post(&conn, &t).await {
            Err(Error::Query(m)) if m.contains("too many operations") && t.rows() > 1 => {
                max_ops.fetch_min((t.rows() / 2) as usize, Ordering::SeqCst);
                let (a, b) = t.split();
                let n = txn(conn.clone(), a, max_ops.clone()).await?;
                Ok(n + txn(conn, b, max_ops).await?)
            }
            Err(Error::Query(m)) if m.contains("lease not found") => Err(Error::Unsupported(
                "Una clave de la carga tiene un lease que no existe en el destino: etcd no copia leases entre clusters. Leé sin la columna lease para copiar las claves sin vencimiento."
                    .into(),
            )),
            Err(e) => Err(e),
            Ok(r) if r.get("succeeded").and_then(Value::as_bool) == Some(true) => Ok(t.rows()),
            Ok(r) => {
                // Some key exists: the lookups say which.
                let found = r.get("responses").and_then(Value::as_array).and_then(|rs| {
                    rs.iter().position(|x| int(x.pointer("/response_range/count")) > 0)
                });
                let key = found.and_then(|i| t.puts.get(i)).map(|p| p.key.clone()).unwrap_or_default();
                Err(dup_error(&key))
            }
        }
    })
}

/// Transactions sent and not yet answered. Dropped with some in flight
/// (the load's future dropped), it waits for them off the runtime's
/// workers, so none commits after the load is gone.
#[derive(Default)]
struct InFlight {
    set: JoinSet<(Result<u64>, usize)>,
    bytes: usize,
}

impl Drop for InFlight {
    fn drop(&mut self) {
        use tokio::runtime::{Handle, RuntimeFlavor};
        if self.set.is_empty() {
            return;
        }
        if let Ok(h) = Handle::try_current() {
            if h.runtime_flavor() == RuntimeFlavor::MultiThread {
                let set = &mut self.set;
                tokio::task::block_in_place(|| h.block_on(async { while set.join_next().await.is_some() {} }));
            }
        }
        // A current-thread runtime can't be waited on from inside: the
        // tasks are aborted (what's already on the wire may still land).
    }
}

/// Committed rows and bytes, and what progress was told.
struct Tally<'a> {
    spec: &'a LoadSpec,
    progress: Progress<'a>,
    done: u64,
    done_bytes: u64,
    reported: u64,
    reported_bytes: u64,
}

impl Tally<'_> {
    fn ack(&mut self, rows: u64, bytes: usize) {
        self.done += rows;
        self.done_bytes += bytes as u64;
        if self.done - self.reported >= self.spec.commit_rows.max(1) || self.done_bytes - self.reported_bytes >= self.spec.commit_bytes.max(1) {
            self.report();
        }
    }

    fn report(&mut self) {
        if self.done != self.reported {
            (self.progress)(self.done);
        }
        self.reported = self.done;
        self.reported_bytes = self.done_bytes;
    }
}

impl InFlight {
    fn joined(&mut self, r: std::result::Result<(Result<u64>, usize), tokio::task::JoinError>, tally: &mut Tally) -> Result<()> {
        let (r, bytes) = r.map_err(|e| Error::State(format!("carga masiva interrumpida: {e}")))?;
        self.bytes -= bytes;
        tally.ack(r?, bytes);
        Ok(())
    }

    /// The transactions already answered.
    fn reap(&mut self, tally: &mut Tally) -> Result<()> {
        while let Some(r) = self.set.try_join_next() {
            self.joined(r, tally)?;
        }
        Ok(())
    }

    /// Wait for one transaction.
    async fn next(&mut self, tally: &mut Tally<'_>) -> Result<()> {
        match self.set.join_next().await {
            Some(r) => self.joined(r, tally),
            None => Ok(()),
        }
    }

    /// Wait for all of them, even after one fails; the first error.
    async fn drain(&mut self, tally: &mut Tally<'_>) -> Result<()> {
        let mut first = Ok(());
        while let Some(r) = self.set.join_next().await {
            let r = self.joined(r, tally);
            if first.is_ok() {
                first = r;
            }
        }
        first
    }

    async fn send(&mut self, conn: &Arc<Conn>, t: Txn, max_ops: &Arc<AtomicUsize>, tally: &mut Tally<'_>) -> Result<()> {
        while !self.set.is_empty() && (self.set.len() >= IN_FLIGHT || self.bytes + t.bytes > IN_FLIGHT_BYTES) {
            self.next(tally).await?;
        }
        let (conn, max_ops, bytes) = (conn.clone(), max_ops.clone(), t.bytes);
        self.bytes += bytes;
        self.set.spawn(async move { (txn(conn, t, max_ops).await, bytes) });
        self.reap(tally)
    }
}

pub async fn load(s: &mut EtcdSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if s.read_only {
        return Err(Error::Query("Conexión de solo lectura: no se puede cargar datos.".into()));
    }
    let rows = Rows::new(&spec.table.name, &spec.columns)?;
    s.cancel.flag.store(false, Ordering::SeqCst);
    let cancel = s.cancel.clone();
    load_puts(&s.conn, &rows, spec, source, progress, &cancel).await
}

async fn load_puts(conn: &Arc<Conn>, rows: &Rows, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>, cancel: &Cancel) -> Result<u64> {
    let woken = cancel.notify.notified();
    tokio::pin!(woken);
    woken.as_mut().enable();
    if cancel.flag.load(Ordering::SeqCst) {
        return Err(Error::Cancelled);
    }
    let mut tally = Tally { spec, progress, done: 0, done_bytes: 0, reported: 0, reported_bytes: 0 };
    let mut fl = InFlight::default();
    let max_ops = Arc::new(AtomicUsize::new(TXN_OPS.min(spec.commit_rows.clamp(1, TXN_OPS as u64) as usize)));
    let max_bytes = TXN_BYTES.min(spec.commit_bytes.max(1).try_into().unwrap_or(usize::MAX));
    let sent = async {
        let mut pending = Pending::default();
        while let Some(batch) = source.next().await {
            for row in batch.rows {
                let put = rows.put(&row)?;
                drop(row);
                if !pending.is_empty() && (pending.len() >= max_ops.load(Ordering::SeqCst) || pending.bytes + Pending::size(&put) > max_bytes) {
                    fl.send(conn, std::mem::take(&mut pending).body(), &max_ops, &mut tally).await?;
                }
                pending.add(put)?;
            }
            fl.reap(&mut tally)?;
        }
        if !pending.is_empty() {
            fl.send(conn, pending.body(), &max_ops, &mut tally).await?;
        }
        Ok(())
    };
    let r = tokio::select! {
        r = sent => r,
        _ = woken => Err(Error::Cancelled),
    };
    // Nothing commits after returning: every transaction sent is awaited,
    // whatever ended the load.
    let drained = fl.drain(&mut tally).await;
    tally.report();
    r.and(drained)?;
    Ok(tally.done)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn cols(n: &[&str]) -> Vec<String> {
        n.iter().map(|c| c.to_string()).collect()
    }

    fn put(k: &str, v: &str) -> Put {
        Put { key: k.as_bytes().to_vec(), value: v.as_bytes().to_vec(), lease: 0 }
    }

    #[test]
    fn key_value_rows_are_their_keys_under_the_target() {
        let c = cols(&["Key", "value", "version", "LEASE"]);
        let m = Rows::new("/t/", &c).unwrap();
        let row = |k: &str, v: Cell, lease: Cell| vec![Cell::Text(k.into()), v, Cell::Int(3), lease];
        // Under the target: as it is; elsewhere: under it, never outside.
        assert_eq!(m.put(&row("/t/a b", Cell::Text("x \"y\" ñ".into()), Cell::Null)).unwrap(), put("/t/a b", "x \"y\" ñ"));
        assert_eq!(m.put(&row("/src/0", Cell::Text("v".into()), Cell::Null)).unwrap(), put("/t/src/0", "v"));
        assert_eq!(m.put(&row("rel", Cell::Text("v".into()), Cell::Null)).unwrap(), put("/t/rel", "v"));
        assert_eq!(m.put(&row("/tx", Cell::Text("v".into()), Cell::Null)).unwrap(), put("/t/tx", "v"));
        assert_eq!(m.put(&row("/t", Cell::Text("v".into()), Cell::Null)).unwrap(), put("/t", "v"));
        assert_eq!(m.put(&row("/t/j", Cell::Json("{\"a\": 1}".into()), Cell::Null)).unwrap(), put("/t/j", "{\"a\": 1}"));
        // JSON goes as its text: no f64 rounding, no key sorting.
        let doc = r#"{"z":1,"n":123456789012345678901234567890,"d":0.10000000000000000000001,"a":2}"#;
        assert_eq!(m.put(&row("/t/big", Cell::Json(doc.into()), Cell::Null)).unwrap().value, doc.as_bytes().to_vec());
        assert_eq!(m.put(&row("/t/d", Cell::Decimal("0.10000000000000000000001".into()), Cell::Null)).unwrap().value, b"0.10000000000000000000001".to_vec());
        assert_eq!(m.put(&row("/t/i", Cell::Int(i64::MAX), Cell::Null)).unwrap().value, i64::MAX.to_string().into_bytes());
        assert_eq!(m.put(&row("/t/f", Cell::Float(0.1), Cell::Null)).unwrap().value, b"0.1".to_vec());
        assert_eq!(m.put(&row("/t/f", Cell::Float(1.5), Cell::Null)).unwrap().value, b"1.5".to_vec());
        assert_eq!(m.put(&row("/t/l", Cell::Text("".into()), Cell::Text("1a".into()))).unwrap().lease, 0x1a);
        // Null or empty keys, and null values, are errors, not skipped rows.
        assert!(m.put(&row("", Cell::Text("v".into()), Cell::Null)).is_err());
        assert!(m.put(&[Cell::Null, Cell::Text("v".into()), Cell::Null, Cell::Null]).is_err());
        assert!(matches!(m.put(&row("/t/n", Cell::Null, Cell::Null)), Err(Error::Unsupported(_))));
        // The root target keeps every key.
        let root = Rows::new("/", &cols(&["key", "value"])).unwrap();
        assert_eq!(root.put(&[Cell::Text("/x".into()), Cell::Text("".into())]).unwrap(), put("/x", ""));
    }

    #[test]
    fn other_columns_make_rows_json_objects() {
        // Columns that aren't etcd's own: nothing is left out.
        let c = cols(&["key", "value", "updated_at", "owner"]);
        let m = Rows::new("/cfg/", &c).unwrap();
        assert!(matches!(m, Rows::Json { .. }));
        let p = m.put(&[Cell::Text("a".into()), Cell::Text("1".into()), Cell::DateTime("2024-01-02 03:04:05".into()), Cell::Null]).unwrap();
        assert_eq!(p.key, b"/cfg/a".to_vec());
        assert_eq!(String::from_utf8(p.value).unwrap(), r#"{"key":"a","value":"1","updated_at":"2024-01-02 03:04:05","owner":null}"#);

        let c = cols(&["id", "nombre", "n"]);
        let m = Rows::new("/imp/", &c).unwrap();
        let p = m.put(&[Cell::Int(7), Cell::Text("O'Brien".into()), Cell::Decimal("1.50".into())]).unwrap();
        assert_eq!(p.key, b"/imp/7".to_vec());
        assert_eq!(String::from_utf8(p.value).unwrap(), r#"{"id":7,"nombre":"O'Brien","n":"1.50"}"#);
        assert!(Rows::new("/imp", &[]).is_err());
        // A repeated column would be a repeated key of the object.
        assert!(Rows::new("/imp", &cols(&["id", "a", "a"])).is_err());
        assert!(Rows::new("/imp", &cols(&["id", "a", "A"])).is_ok());
        // Column names are escaped as JSON keys.
        let m = Rows::new("/imp", &cols(&["id", "a\"b"])).unwrap();
        let p = m.put(&[Cell::Int(1), Cell::Text("\"}".into())]).unwrap();
        assert_eq!(String::from_utf8(p.value).unwrap(), r#"{"id":1,"a\"b":"\"}"}"#);
        // A null first column has no key.
        assert!(m.put(&[Cell::Null, Cell::Null, Cell::Null]).is_err());
    }

    #[test]
    fn json_values_keep_their_type() {
        let m = Rows::new("/d", &cols(&["id", "b", "t", "big", "u", "nan", "inf", "j"])).unwrap();
        let p = m
            .put(&[
                Cell::Int(1),
                Cell::Bytes(vec![0xAB]),
                Cell::Text("0xAB".into()),
                Cell::Int(i64::MAX),
                Cell::UInt(u64::MAX),
                Cell::Float(f64::NAN),
                Cell::Float(f64::NEG_INFINITY),
                Cell::Json("[1,{\"a\":null}]".into()),
            ])
            .unwrap();
        let v: Value = serde_json::from_slice(&p.value).unwrap();
        assert_eq!(v["b"], json!({"$binary": {"base64": "qw==", "subType": "00"}}));
        assert_eq!(v["t"], json!("0xAB"));
        assert_eq!(v["big"].as_i64(), Some(i64::MAX));
        assert_eq!(v["u"].as_u64(), Some(u64::MAX));
        assert_eq!(v["nan"], json!({"$numberDouble": "NaN"}));
        assert_eq!(v["inf"], json!({"$numberDouble": "-Infinity"}));
        assert_eq!(v["j"], json!([1, {"a": null}]));
        let nulls = |j: &str| {
            let mut r = vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null];
            r.push(Cell::Json(j.into()));
            r
        };
        assert!(m.put(&nulls("{")).is_err());
        // One JSON value, nothing around it: no way to add keys to the row.
        assert!(m.put(&nulls(r#"1,"id":3"#)).is_err());
        assert!(m.put(&nulls(r#"{"a":1}}"#)).is_err());
        // Nested JSON is its own text: big numbers, long decimals and key
        // order survive.
        let doc = r#"{"z":1,"n":123456789012345678901234567890,"d":0.10000000000000000000001,"a":2}"#;
        let p = m.put(&nulls(doc)).unwrap();
        let text = String::from_utf8(p.value).unwrap();
        assert!(text.ends_with(&format!(r#","j":{doc}}}"#)), "{text}");
    }

    #[test]
    fn a_repeated_key_in_a_transaction_is_an_error() {
        let mut p = Pending::default();
        p.add(put("/k/1", "a")).unwrap();
        assert!(p.add(put("/k/1", "b")).is_err());
        p.add(put("/k/2", "b")).unwrap();
        p.add(Put { key: vec![0xff, b'"'], value: vec![0; 5], lease: 0x1a }).unwrap();
        let t = p.body();
        assert_eq!(t.rows(), 3);
        let body: Value = serde_json::from_slice(&t.body()).unwrap();
        assert_eq!(body["compare"][0], json!({"key": b64(b"/k/1"), "target": "CREATE", "result": "EQUAL", "create_revision": "0"}));
        assert_eq!(body["success"][0], json!({"request_put": {"key": b64(b"/k/1"), "value": b64(b"a")}}));
        assert_eq!(body["success"][2], json!({"request_put": {"key": b64(&[0xff, b'"']), "value": b64(&[0; 5]), "lease": "26"}}));
        assert_eq!(body["failure"][1], json!({"request_range": {"key": b64(b"/k/2"), "count_only": true}}));
        let (a, b) = t.split();
        assert_eq!((a.rows(), b.rows()), (1, 2));
        for (t, n) in [(&a, 1), (&b, 2)] {
            let body: Value = serde_json::from_slice(&t.body()).unwrap();
            for list in ["compare", "success", "failure"] {
                assert_eq!(body[list].as_array().unwrap().len(), n);
            }
        }
        let body: Value = serde_json::from_slice(&b.body()).unwrap();
        assert_eq!(body["success"][0]["request_put"]["key"], json!(b64(b"/k/2")));
    }

    #[test]
    fn pages_are_sized_by_bytes() {
        assert_eq!(next_page(PAGE_FIRST, 100), 2 * PAGE_FIRST);
        assert_eq!(next_page(PAGE_MAX, 100), PAGE_MAX);
        assert_eq!(next_page(PAGE_MAX, 1_500_000), 2);
        assert_eq!(next_page(PAGE_FIRST, 100 * 1024), 2 * PAGE_FIRST);
        assert_eq!(next_page(PAGE_MAX, 100 * 1024), 40);
        assert_eq!(next_page(1, 10 * 1024 * 1024), 1);
    }

    /// A range reply cut anywhere gives the same key-values and rest.
    #[test]
    fn range_replies_are_read_a_key_value_at_a_time() {
        let kvs: Vec<Value> = (0..5)
            .map(|i| json!({"key": b64(format!("/k/{i}").as_bytes()), "value": b64(format!("v{i} ]}}\"[{{").as_bytes()), "create_revision": "2", "mod_revision": "3", "version": "1"}))
            .collect();
        let reply = json!({"header": {"cluster_id": "1", "revision": "42"}, "kvs": kvs, "more": true, "count": "9"}).to_string();
        let reply = reply.replace("\"more\"", " \"x\" : [ { \"kvs\": [ {} ] } , \"kvs\" ] ,\n\"more\"");
        for cut in [1, 2, 3, 7, 64, reply.len()] {
            let mut scan = KvScanner::default();
            let mut got = Vec::new();
            for chunk in reply.as_bytes().chunks(cut) {
                scan.feed(chunk, &mut |kv| {
                    got.push(kv);
                    Ok(())
                })
                .unwrap();
            }
            assert_eq!(got, kvs, "cut {cut}");
            let rest = scan.finish().unwrap();
            assert_eq!(int(rest.pointer("/header/revision")), 42);
            assert_eq!(rest["more"], json!(true));
            assert_eq!(rest["kvs"], json!([0, 0, 0, 0, 0]));
        }
        // No key-values, and a cut reply.
        let mut scan = KvScanner::default();
        scan.feed(br#"{"header":{"revision":"7"},"count":"0"}"#, &mut |_| panic!()).unwrap();
        assert_eq!(scan.finish().unwrap()["count"], json!("0"));
        let mut scan = KvScanner::default();
        scan.feed(br#"{"header":{"revision":"7"},"kvs":[{"key":"YQ=="#, &mut |_| panic!()).unwrap();
        assert!(scan.finish().is_err());
        // An error from a key-value's consumer stops the reading.
        let mut scan = KvScanner::default();
        let r = scan.feed(reply.as_bytes(), &mut |_| Err(Error::Cancelled));
        assert!(matches!(r, Err(Error::Cancelled)));
    }

    #[test]
    fn binaries_go_whole_and_reads_are_typed() {
        let m = Rows::new("", &cols(&["key", "value"])).unwrap();
        assert_eq!(m.put(&[Cell::Bytes(vec![0xff, 1]), Cell::Bytes(vec![0, 0x80])]).unwrap(), Put { key: vec![0xff, 1], value: vec![0, 0x80], lease: 0 });
        let kv = json!({"key": b64(b"/a"), "value": b64(&[0xff, 0x00]), "create_revision": "2", "mod_revision": "5", "version": "3", "lease": "26"});
        assert_eq!(
            kv_cells(&kv),
            vec![Cell::Text("/a".into()), Cell::Bytes(vec![0xff, 0]), Cell::Int(2), Cell::Int(5), Cell::Int(3), Cell::Text("1a".into())]
        );
        assert_eq!(kv_cells(&json!({"key": b64(b"k")}))[5], Cell::Null);
    }
}
