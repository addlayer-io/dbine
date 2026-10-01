//! Bulk transfer (see `dbine_driver::transfer`) for Couchbase.
//!
//! Reading: one SQL++ request, `SELECT META(d).id, d FROM <keyspace> AS d
//! [WHERE <filter>]`, whose reply is parsed **as it streams in**: each
//! element of `results` becomes a row as soon as its bytes arrive, so a
//! collection of any size is read in one pass, with bounded memory and no
//! index needed for paging (a primary index or 7.6's sequential scan, as
//! any `SELECT` there). The request is `readonly` (the source is never
//! written). Without asked-for columns, a first request gathers every
//! top-level field name of the documents the read will return (grouped on
//! the server, so only the distinct names travel), and the columns are the
//! key and then those names in order; a field that shows up only during
//! the read (the collection changed meanwhile) fails the read instead of
//! being dropped. The key column is `_id`, or `meta_id` when documents
//! have a field `_id` of their own (both: unsupported); it always goes
//! first, and that's how a load tells it from a field with the other name
//! (see [`key_column`]). Values keep their
//! JSON type; nested objects and arrays become JSON cells. An explicit
//! `null` is the JSON cell `null` and a missing field a null cell, so the
//! two stay apart on the way back. A document that isn't a JSON object (an
//! array, a scalar, a binary value) fails the read: it has no fields to
//! make a row of. The filter is a SQL++ condition over the document's
//! fields (alias `d`; unqualified names work too, also in the field
//! discovery, which filters in a subquery of its own). The request carries
//! a `client_context_id` the session's interrupter stops on the server.
//!
//! Loading: SQL++ `INSERT INTO <keyspace> (KEY …, VALUE …) SELECT … FROM
//! $docs`, the documents as a named parameter, at most [`CHUNK`] documents
//! and [`CHUNK_BYTES`] per statement (a larger document goes alone, up to
//! Couchbase's [`MAX_DOC`]) and [`IN_FLIGHT`] statements holding at most
//! [`IN_FLIGHT_BYTES`] at once; the Query service fans each one out to the
//! data service. The REST services are the only way the driver talks to
//! Couchbase (no KV / SDK connection), and a multi-document `INSERT` is how
//! they write in bulk. Every statement commits on its own (there are no
//! multi-statement transactions here), so `commit_rows` only paces the
//! progress, which counts the documents the server reports written, also
//! from statements that failed half-way. JSON cells that were whole floats
//! (`1.0`) come back as integers: JSON has a single number type. A row
//! means what it means in the insert script: the key is the `_id` or
//! `meta_id` column (the first of them when both are there, the other one
//! then being a field, as a read gives them), else `id` or `key`; `UUID()`
//! when there's none or it's null. `_id` / `meta_id` are left out of the
//! document when they're the key, and so are null cells; JSON cells go
//! nested (the JSON cell `null` is an explicit `null`), whole numbers stay
//! numbers (also past 2^53), binaries as `0x…` hex. `INSERT` never replaces
//! a document: an existing key fails the load (`Duplicate Key`). All of a
//! load's statements share a `client_context_id`. A failed load waits for
//! every statement in flight before it returns (counting what they wrote),
//! so no document lands after it. The interrupter only stops new statements
//! from starting: the ones in flight (at most [`IN_FLIGHT`] × [`CHUNK`]
//! documents) are let finish, so the interrupted load's progress is exact
//! (a statement stopped half-way reports fewer documents than it wrote).
//! Each transfer clears the session's interrupt flag when it starts, so a
//! late interrupt of an earlier one doesn't cancel it. A
//! cancelled load (its future dropped) stops them and waits for them, up
//! to [`DROP_WAIT`]: on a multi-thread runtime for the requests themselves,
//! on a current-thread one (which can't run them while it waits) for the
//! server to have none left under the load's id.

use crate::ddl::path;
use crate::{http_error, query_error, Cancel, CbSession, Conn};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{kinds, Error, Result};
use serde_json::{json, Value};
use std::collections::HashSet;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::JoinSet;

/// Documents per `INSERT` statement: at most this many (measured: 250 × 32
/// beat 500 × 16, 1,000 × 32 and 250 × 64 on a local node)…
pub(crate) const CHUNK: usize = 250;
/// …and this many bytes of documents (the Query service refuses requests
/// past its `request-size-cap`, 64 MiB by default).
pub(crate) const CHUNK_BYTES: usize = 1024 * 1024;
/// `INSERT` statements in flight at once…
pub(crate) const IN_FLIGHT: usize = 32;
/// …holding at most this many bytes (a larger document goes alone).
pub(crate) const IN_FLIGHT_BYTES: usize = 24 * 1024 * 1024;
/// Couchbase's largest document value (20 MiB).
pub(crate) const MAX_DOC: usize = 20 * 1024 * 1024;
/// The load statement: the documents go as a named parameter, so the
/// Query service parses a short statement instead of the documents as
/// SQL++ literals (about 4 times faster).
fn insert_statement(keyspace: &str) -> String {
    format!("INSERT INTO {keyspace} (KEY IFMISSINGORNULL(_k, UUID()), VALUE _v) SELECT x.k AS _k, x.v AS _v FROM $docs AS x")
}
/// How long a dropped load waits for its statements to end.
pub(crate) const DROP_WAIT: std::time::Duration = std::time::Duration::from_secs(120);

/// Splits a Query service reply into the elements of its `results` array
/// as the bytes arrive; the rest of the reply (status, errors) is checked
/// at the end.
#[derive(Default)]
pub(crate) struct ResultsScanner {
    buf: Vec<u8>,
    /// Where scanning resumes in `buf`.
    at: usize,
    /// Inside `results` (after its `[`), and whether it ended.
    inside: bool,
    ended: bool,
    depth: u32,
    in_str: bool,
    escaped: bool,
    /// Start of the element being read.
    start: usize,
}

impl ResultsScanner {
    /// Feed bytes; returns the complete elements found.
    pub(crate) fn feed(&mut self, bytes: &[u8]) -> Result<Vec<Value>> {
        self.buf.extend_from_slice(bytes);
        let mut out = Vec::new();
        if self.ended {
            return Ok(out);
        }
        if !self.inside {
            let Some(open) = find_results(&self.buf) else { return Ok(out) };
            self.inside = true;
            self.at = open + 1;
            self.start = self.at;
        }
        let mut i = self.at;
        while i < self.buf.len() {
            let b = self.buf[i];
            if self.in_str {
                if self.escaped {
                    self.escaped = false;
                } else if b == b'\\' {
                    self.escaped = true;
                } else if b == b'"' {
                    self.in_str = false;
                }
            } else {
                match b {
                    b'"' => self.in_str = true,
                    b'{' | b'[' => self.depth += 1,
                    b'}' | b']' if self.depth > 0 => self.depth -= 1,
                    b',' | b']' if self.depth == 0 => {
                        let elem = self.buf[self.start..i].trim_ascii();
                        if !elem.is_empty() {
                            out.push(serde_json::from_slice(elem).map_err(|e| Error::Query(format!("Respuesta inesperada de Couchbase: {e}")))?);
                        }
                        self.start = i + 1;
                        if b == b']' {
                            self.ended = true;
                            i += 1;
                            break;
                        }
                    }
                    _ => {}
                }
            }
            i += 1;
        }
        // Drop what's consumed; the tail after `]` is kept for `finish`.
        let cut = if self.ended { i } else { self.start };
        self.buf.drain(..cut);
        self.at = i - cut;
        self.start -= cut.min(self.start);
        Ok(out)
    }

    /// The reply ended: fail with its errors, if any.
    pub(crate) fn finish(self) -> Result<()> {
        let tail: Value = if self.inside {
            if !self.ended {
                return Err(Error::Query("Couchbase cortó la respuesta antes de terminar los resultados".into()));
            }
            // What follows `results: [...]` is the rest of the object.
            let mut obj = b"{\"_\":0".to_vec();
            obj.extend_from_slice(&self.buf);
            serde_json::from_slice(&obj).unwrap_or(Value::Null)
        } else {
            serde_json::from_slice(&self.buf).map_err(|_| Error::Query(format!("Respuesta inesperada de Couchbase: {}", String::from_utf8_lossy(&self.buf).trim())))?
        };
        if let Some(errs) = tail.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
            return Err(query_error(errs, None));
        }
        match tail.get("status").and_then(Value::as_str) {
            Some("stopped") => Err(Error::Cancelled),
            Some("success") | None => Ok(()),
            Some(s) => Err(Error::Query(format!("la lectura terminó con estado «{s}»"))),
        }
    }
}

/// The `[` that opens the top-level `results` array.
fn find_results(buf: &[u8]) -> Option<usize> {
    const KEY: &[u8] = b"\"results\"";
    let mut from = 0;
    while let Some(p) = buf[from..].windows(KEY.len()).position(|w| w == KEY) {
        let mut i = from + p + KEY.len();
        while buf.get(i).is_some_and(u8::is_ascii_whitespace) {
            i += 1;
        }
        if buf.get(i) == Some(&b':') {
            i += 1;
            while buf.get(i).is_some_and(u8::is_ascii_whitespace) {
                i += 1;
            }
            if buf.get(i) == Some(&b'[') {
                return Some(i);
            }
        }
        from += p + 1;
    }
    None
}

/// A JSON value as a cell: nested values as JSON.
pub(crate) fn to_cell(v: &Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                Cell::Int(i)
            } else if let Some(u) = n.as_u64() {
                Cell::UInt(u)
            } else {
                Cell::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => Cell::Text(s.clone()),
        other => Cell::Json(other.to_string()),
    }
}

/// How a read fills its rows: the column that is the document's key, and
/// whether every field must have a column (the columns came from the
/// documents themselves, not asked for).
struct RowShape {
    names: Vec<String>,
    key: Option<usize>,
    all_fields: Option<HashSet<String>>,
}

impl RowShape {
    /// Asked-for columns: the key is the first of `_id` / `meta_id` (the
    /// same rule a load uses).
    fn asked(names: Vec<String>) -> Self {
        let key = doc_key_column(&names);
        RowShape { names, key, all_fields: None }
    }

    /// The key and then every field the documents have (sorted). The key
    /// goes first, so a load takes it for the key and a field `meta_id`
    /// (or `_id`) after it for a field.
    fn discovered(mut fields: Vec<String>) -> Result<Self> {
        fields.sort();
        fields.dedup();
        let key = match ["_id", "meta_id"].into_iter().find(|k| !fields.iter().any(|f| f == k)) {
            Some(k) => k.to_string(),
            None => {
                return Err(Error::Unsupported(
                    "los documentos tienen campos «_id» y «meta_id» propios, y la clave no tiene con qué nombre ir: pedí las columnas a copiar".into(),
                ))
            }
        };
        let all: HashSet<String> = fields.iter().cloned().collect();
        let names = std::iter::once(key).chain(fields).collect();
        Ok(RowShape { names, key: Some(0), all_fields: Some(all) })
    }

    /// A `{k, v}` result (key and document) as a row. A missing field is a
    /// null cell, an explicit `null` the JSON cell `null`.
    fn row(&self, key: &str, doc: &Value) -> Result<Vec<Cell>> {
        let Some(obj) = doc.as_object() else {
            let what = match doc {
                Value::Array(_) => "un array",
                Value::String(_) => "un texto",
                Value::Number(_) => "un número",
                Value::Bool(_) => "un booleano",
                Value::Null => "null",
                Value::Object(_) => unreachable!(),
            };
            return Err(Error::Unsupported(format!(
                "el documento «{key}» no es un objeto JSON sino {what} (o un valor binario): no tiene campos para armar una fila"
            )));
        };
        if let Some(k) = self.key.map(|i| &self.names[i]) {
            if obj.contains_key(k) {
                let other = if k == "_id" { "meta_id" } else { "_id" };
                return Err(Error::Unsupported(format!(
                    "el documento «{key}» tiene un campo «{k}» propio, que se confundiría con la clave: pedí primero la columna «{other}», que va a ser la clave"
                )));
            }
        }
        if let Some(all) = &self.all_fields {
            if let Some(f) = obj.keys().find(|f| !all.contains(*f)) {
                return Err(Error::Query(format!(
                    "el documento «{key}» tiene el campo «{f}», que apareció durante la lectura (la colección cambió): volvé a copiar la tabla"
                )));
            }
        }
        Ok(self
            .names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                if Some(i) == self.key {
                    Cell::Text(key.to_string())
                } else {
                    match obj.get(n) {
                        None => Cell::Null,
                        Some(Value::Null) => Cell::Json("null".into()),
                        Some(v) => to_cell(v),
                    }
                }
            })
            .collect())
    }
}

/// A row's key (`None`: `UUID()`) and document, what the insert script
/// writes.
fn row_parts(names: &[String], key_col: Option<usize>, row: &[Cell]) -> (Option<String>, serde_json::Map<String, Value>) {
    let drop_key = key_col.is_some_and(|i| names[i] == "_id" || names[i] == "meta_id");
    let doc: serde_json::Map<String, Value> = names
        .iter()
        .zip(row)
        .enumerate()
        .filter(|(i, (_, c))| !(drop_key && Some(*i) == key_col) && !matches!(c, Cell::Null))
        .map(|(_, (n, c))| {
            let v = match c {
                // Whole numbers as numbers, even past 2^53 (the grid shows those as text).
                Cell::Int(i) => Value::from(*i),
                Cell::UInt(u) => Value::from(*u),
                _ => c.to_json(),
            };
            (n.clone(), v)
        })
        .collect();
    let key = key_col.and_then(|i| row.get(i)).filter(|c| !matches!(c, Cell::Null)).map(|c| match c.to_json() {
        Value::String(s) => s,
        other => other.to_string(),
    });
    (key, doc)
}

/// A row as `{"k": key, "v": document}`; no `k` when there's no key.
#[cfg(test)]
fn row_entry(names: &[String], key_col: Option<usize>, row: &[Cell]) -> Value {
    let (key, doc) = row_parts(names, key_col, row);
    let mut entry = json!({ "v": doc });
    if let Some(k) = key {
        entry["k"] = Value::String(k);
    }
    entry
}

/// [`row_parts`] as the JSON text of a `$docs` element. `n` is the row's
/// number (for errors).
fn entry_bytes(names: &[String], key_col: Option<usize>, row: &[Cell], n: u64) -> Result<Vec<u8>> {
    let (key, doc) = row_parts(names, key_col, row);
    let doc = serde_json::to_vec(&doc).map_err(Error::query)?;
    if doc.len() > MAX_DOC {
        return Err(Error::Unsupported(format!(
            "la fila {n} da un documento de {} bytes, y Couchbase admite hasta {MAX_DOC} bytes por documento",
            doc.len()
        )));
    }
    let mut out = Vec::with_capacity(doc.len() + key.as_ref().map_or(0, |k| k.len() + 8) + 8);
    out.extend_from_slice(br#"{"v":"#);
    out.extend_from_slice(&doc);
    if let Some(k) = key {
        out.extend_from_slice(br#","k":"#);
        out.extend_from_slice(&serde_json::to_vec(&k).map_err(Error::query)?);
    }
    out.push(b'}');
    Ok(out)
}

/// The column that is the document's key (`META().id`) on both sides: the
/// first of `_id` / `meta_id`. A read puts the key first, so with both
/// there the other one is a field of the documents.
fn doc_key_column(names: &[String]) -> Option<usize> {
    names.iter().position(|c| c == "_id" || c == "meta_id")
}

/// A load's key column: [`doc_key_column`], else `id`, else `key` (those
/// two stay in the document).
fn key_column(names: &[String]) -> Option<usize> {
    doc_key_column(names).or_else(|| ["id", "key"].iter().find_map(|k| names.iter().position(|c| c == k)))
}

/// A transfer's hold on the session's interrupt state while this lives:
/// the interrupt flag is cleared when it starts (an interrupt left over from
/// an earlier operation doesn't cancel this one) and when it ends (this
/// one's doesn't reach the next operation). `id`, if any, is the statement
/// the interrupter stops on the server; without one, the interrupter only
/// sets the flag.
struct Current(Arc<Cancel>);

impl Current {
    fn set(cancel: &Arc<Cancel>, id: Option<&str>) -> Self {
        cancel.flag.store(false, Ordering::SeqCst);
        *cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = id.map(str::to_string);
        Current(cancel.clone())
    }
}

impl Drop for Current {
    fn drop(&mut self) {
        *self.0.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        self.0.flag.store(false, Ordering::SeqCst);
    }
}

/// Stop the requests with this `client_context_id` on the server.
async fn stop_requests(conn: &Conn, id: &str) {
    let body = json!({"statement": "DELETE FROM system:active_requests WHERE clientContextID = $id", "$id": id});
    if let Err(e) = conn.post_query(&body).await {
        tracing::debug!("couchbase stop failed: {e}");
    }
}

/// What an `INSERT` statement did: its bytes of documents, the documents
/// the server reports written (also when it failed half-way), and how it
/// ended.
struct Done {
    bytes: usize,
    written: u64,
    result: Result<()>,
}

/// A load's statements in flight. Dropped with statements still running
/// (the load's future dropped: cancelled), it stops them on the server and
/// waits for them all, so no document is written after the load is gone.
struct InFlight {
    set: JoinSet<Done>,
    bytes: usize,
    conn: Arc<Conn>,
    id: String,
}

impl InFlight {
    fn settle(&mut self, r: std::result::Result<Done, tokio::task::JoinError>) -> (u64, Result<()>) {
        match r {
            Ok(d) => {
                self.bytes = self.bytes.saturating_sub(d.bytes);
                (d.written, d.result)
            }
            Err(e) => (0, Err(Error::State(format!("INSERT interrumpido: {e}")))),
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.set.is_empty() {
            return;
        }
        let mut set = std::mem::take(&mut self.set);
        let (conn, id) = (self.conn.clone(), self.id.clone());
        match Handle::try_current() {
            Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => {
                let (tx, rx) = std::sync::mpsc::channel::<()>();
                h.spawn(async move {
                    stop_requests(&conn, &id).await;
                    while set.join_next().await.is_some() {}
                    drop(tx);
                });
                // Off the worker, so the requests keep going while this
                // waits (a runtime shutting down drops the task and ends
                // the wait).
                tokio::task::block_in_place(|| {
                    if rx.recv_timeout(DROP_WAIT).is_err() {
                        tracing::warn!("couchbase: a cancelled load's INSERTs didn't end in {DROP_WAIT:?}");
                    }
                });
            }
            _ => {
                // This runtime can't run the requests while it waits: they're
                // aborted (none is sent from here on) and the server is
                // watched from another thread until it runs none of the
                // load's statements.
                set.abort_all();
                drop(set);
                let (query, user, password) = (conn.query.clone(), conn.user.clone(), conn.password.clone());
                let waited = std::thread::spawn(move || stop_and_wait_blocking(&query, &user, &password, &id)).join();
                if !matches!(waited, Ok(true)) {
                    tracing::warn!("couchbase: couldn't confirm that a cancelled load's INSERTs ended");
                }
            }
        }
    }
}

/// Stop the statements with this `client_context_id` and wait until the
/// server runs none of them (none seen twice in a row, half a second apart,
/// so one still in transit shows up), up to [`DROP_WAIT`]. Own runtime and
/// HTTP client: the caller's are blocked. `true` once none is left.
fn stop_and_wait_blocking(query: &str, user: &str, password: &str, id: &str) -> bool {
    let Ok(rt) = tokio::runtime::Builder::new_current_thread().enable_all().build() else { return false };
    let Ok(http) = reqwest::Client::builder().connect_timeout(std::time::Duration::from_secs(10)).build() else { return false };
    let post = |stmt: &str| {
        let body = json!({"statement": stmt, "$id": id});
        let req = http.post(format!("{query}/query/service")).basic_auth(user, Some(password)).json(&body);
        async move { req.send().await.ok()?.json::<Value>().await.ok() }
    };
    rt.block_on(async {
        let deadline = tokio::time::Instant::now() + DROP_WAIT;
        let (mut quiet, mut failed) = (0, 0);
        while tokio::time::Instant::now() < deadline {
            post("DELETE FROM system:active_requests WHERE clientContextID = $id").await;
            let left = post("SELECT RAW COUNT(*) FROM system:active_requests WHERE clientContextID = $id").await;
            match left.as_ref().map(|v| v.pointer("/results/0").and_then(Value::as_u64)) {
                Some(Some(0)) => {
                    quiet += 1;
                    if quiet >= 2 {
                        return true;
                    }
                }
                Some(_) => quiet = 0,
                // The server can't be asked (e.g. a certificate this client
                // doesn't trust): no point in waiting the whole time.
                None => {
                    failed += 1;
                    if failed >= 3 {
                        return false;
                    }
                }
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
        false
    })
}

/// One `INSERT` statement: its body is sent as is.
async fn insert(conn: &Conn, body: Vec<u8>, docs: usize) -> (u64, Result<()>) {
    let sent = conn
        .http
        .post(format!("{}/query/service", conn.query))
        .basic_auth(&conn.user, Some(&conn.password))
        .header(reqwest::header::CONTENT_TYPE, "application/json")
        .body(body)
        .send()
        .await;
    let resp = match sent {
        Ok(r) => r,
        Err(e) => return (0, Err(http_error(e))),
    };
    let status = resp.status();
    let text = match resp.text().await {
        Ok(t) => t,
        Err(e) => return (0, Err(http_error(e))),
    };
    let Ok(v) = serde_json::from_str::<Value>(&text) else {
        return (
            0,
            Err(if status == reqwest::StatusCode::UNAUTHORIZED {
                Error::AuthFailed(text.trim().to_string())
            } else {
                Error::Query(format!("HTTP {status}: {}", text.trim()))
            }),
        );
    };
    let written = v.pointer("/metrics/mutationCount").and_then(Value::as_u64).unwrap_or(0);
    if let Some(errs) = v.get("errors").and_then(Value::as_array).filter(|e| !e.is_empty()) {
        return (written, Err(query_error(errs, None)));
    }
    if v.get("status").and_then(Value::as_str) == Some("stopped") {
        return (written, Err(Error::Cancelled));
    }
    if written as usize != docs {
        return (written, Err(Error::Query(format!("INSERT: se escribieron {written} de {docs} documentos"))));
    }
    (written, Ok(()))
}

/// A load in progress: the statement being filled, the ones in flight and
/// the rows counted so far.
struct Loader<'p> {
    /// The request body up to the `$docs` array's first element.
    prefix: Vec<u8>,
    buf: Vec<u8>,
    docs: usize,
    flight: InFlight,
    done: u64,
    reported: u64,
    every: u64,
    progress: Progress<'p>,
    /// The session's interrupt flag: no statement starts once it's set.
    cancel: Arc<Cancel>,
}

impl Loader<'_> {
    fn count(&mut self, rows: u64, force: bool) {
        self.done += rows;
        if self.done - self.reported >= self.every || (force && self.done > self.reported) {
            self.reported = self.done;
            (self.progress)(self.done);
        }
    }

    fn settled(&mut self, r: std::result::Result<Done, tokio::task::JoinError>) -> Result<()> {
        let (written, r) = self.flight.settle(r);
        self.count(written, false);
        r
    }

    /// Add a `$docs` element, sending the statement first when it's full.
    async fn add(&mut self, entry: &[u8]) -> Result<()> {
        if self.docs > 0 && (self.docs >= CHUNK || self.buf.len() + 1 + entry.len() > CHUNK_BYTES) {
            self.send().await?;
        }
        if self.docs > 0 {
            self.buf.push(b',');
        }
        self.buf.extend_from_slice(entry);
        self.docs += 1;
        Ok(())
    }

    /// Send the statement being filled, once there's room in flight.
    async fn send(&mut self) -> Result<()> {
        if self.docs == 0 {
            return Ok(());
        }
        let bytes = self.buf.len();
        while !self.flight.set.is_empty() && (self.flight.set.len() >= IN_FLIGHT || self.flight.bytes + bytes > IN_FLIGHT_BYTES) {
            let Some(r) = self.flight.set.join_next().await else { break };
            self.settled(r)?;
        }
        // Interrupted (maybe while waiting for room): nothing new starts.
        if self.cancel.flag.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        let mut body = Vec::with_capacity(self.prefix.len() + bytes + 2);
        body.extend_from_slice(&self.prefix);
        body.append(&mut self.buf);
        body.extend_from_slice(b"]}");
        let docs = std::mem::take(&mut self.docs);
        let conn = self.flight.conn.clone();
        self.flight.bytes += bytes;
        self.flight.set.spawn(async move {
            let (written, result) = insert(&conn, body, docs).await;
            Done { bytes, written, result }
        });
        Ok(())
    }

    /// Count the statements that already ended.
    fn reap(&mut self) -> Result<()> {
        while let Some(r) = self.flight.set.try_join_next() {
            self.settled(r)?;
        }
        Ok(())
    }

    /// Wait for every statement (they're let finish, so the documents
    /// they wrote are counted); the first error, if any.
    async fn drain(&mut self) -> Result<()> {
        let mut first = Ok(());
        while let Some(r) = self.flight.set.join_next().await {
            let r = self.settled(r);
            if first.is_ok() {
                first = r;
            }
        }
        first
    }
}

/// Every field of the documents the read returns, grouped on the server
/// (only the distinct names travel). The filter goes in a subquery with the
/// read's single keyspace `d`, as in the read itself: next to the `UNNEST`
/// an unqualified field would be ambiguous (error 3080). The subquery
/// streams (no materialization) and keeps the filter's index use.
fn discovery_statement(ks: &str, filter: &str) -> String {
    format!("SELECT RAW dbine_f FROM (SELECT RAW d FROM {ks} AS d{filter}) AS dbine_d UNNEST OBJECT_NAMES(dbine_d) AS dbine_f GROUP BY dbine_f")
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

impl CbSession {
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        if spec.table.kind != kinds::COLLECTION {
            return dbine_driver::transfer::read_via_execute(self, spec, sink).await;
        }
        let ks = path(spec.table.schema(), &spec.table.name);
        let filter = match spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => format!(" WHERE {f}"),
            None => String::new(),
        };
        // Every request of the read carries this id: the interrupter stops
        // it on the server.
        let id = crate::context_id();
        let cancel = self.cancel.clone();
        let _current = Current::set(&cancel, Some(&id));
        let conn = self.conn.clone();
        let read_body = |stmt: &str| {
            let mut body = self.body(stmt, Some(&id));
            // The source is only read from.
            body["readonly"] = Value::Bool(true);
            body["pretty"] = Value::Bool(false);
            body
        };

        let shape = match &spec.columns {
            Some(names) => RowShape::asked(names.clone()),
            None => {
                let body = read_body(&discovery_statement(&ks, &filter));
                let v = cancel.run(conn.post_query(&body)).await?;
                let fields = v
                    .get("results")
                    .and_then(Value::as_array)
                    .map(|r| r.iter().filter_map(Value::as_str).map(str::to_string).collect())
                    .unwrap_or_default();
                RowShape::discovered(fields)?
            }
        };
        let body = read_body(&format!("SELECT META(d).id AS k, d AS v FROM {ks} AS d{filter}"));
        let mut resp = cancel
            .run(async { conn.http.post(format!("{}/query/service", conn.query)).basic_auth(&conn.user, Some(&conn.password)).json(&body).send().await.map_err(http_error) })
            .await?;
        if resp.status() == reqwest::StatusCode::UNAUTHORIZED {
            return Err(Error::AuthFailed(resp.text().await.unwrap_or_default().trim().to_string()));
        }

        let cols: Vec<TransferColumn> = shape
            .names
            .iter()
            .enumerate()
            .map(|(i, n)| {
                let key = Some(i) == shape.key;
                TransferColumn { name: n.clone(), type_name: if key { "string".into() } else { String::new() }, nullable: !key }
            })
            .collect();
        sink.lock().map_err(lock_err)?.begin(&cols)?;
        let mut scanner = ResultsScanner::default();
        let mut builder = BatchBuilder::new();
        loop {
            let chunk = cancel.run(async { resp.chunk().await.map_err(http_error) }).await?;
            let Some(chunk) = chunk else { break };
            let found = scanner.feed(&chunk)?;
            if found.is_empty() {
                continue;
            }
            let mut s = sink.lock().map_err(lock_err)?;
            for r in found {
                let key = match r.get("k") {
                    Some(Value::String(s)) => s.clone(),
                    Some(v) => v.to_string(),
                    None => String::new(),
                };
                let doc = r.get("v").unwrap_or(&Value::Null);
                builder.push(shape.row(&key, doc)?, &mut *s)?;
            }
        }
        scanner.finish()?;
        builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        if self.conn.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se cargan datos.".into()));
        }
        if spec.table.kind != kinds::COLLECTION {
            return Err(Error::Unsupported("en Couchbase solo se cargan documentos en una colección".into()));
        }
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let key_col = key_column(&names);

        // Every statement of the load carries this id: a dropped load stops
        // them all on the server. The interrupter doesn't get it: it only
        // sets the flag, so no statement starts and the ones in flight end
        // on their own and are counted exactly (one stopped half-way
        // reports fewer documents than it wrote).
        let id = crate::context_id();
        let cancel = self.cancel.clone();
        let _current = Current::set(&cancel, None);
        let mut body = self.body(&insert_statement(&path(spec.table.schema(), &spec.table.name)), Some(&id));
        // Only the mutations are wanted back.
        body["pretty"] = Value::Bool(false);
        body["scan_consistency"] = json!("not_bounded");
        let mut prefix = serde_json::to_vec(&body).map_err(Error::query)?;
        prefix.pop(); // the closing `}`
        prefix.extend_from_slice(br#","$docs":["#);

        let mut l = Loader {
            prefix,
            buf: Vec::new(),
            docs: 0,
            flight: InFlight { set: JoinSet::new(), bytes: 0, conn: self.conn.clone(), id },
            done: 0,
            reported: 0,
            every: spec.commit_rows.max(1),
            progress,
            cancel: cancel.clone(),
        };
        let mut rows = 0u64;
        let r: Result<()> = async {
            loop {
                if cancel.flag.load(Ordering::SeqCst) {
                    return Err(Error::Cancelled);
                }
                let Some(batch) = source.next().await else { break };
                for row in &batch.rows {
                    rows += 1;
                    if row.len() != names.len() {
                        return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                    }
                    l.add(&entry_bytes(&names, key_col, row, rows)?).await?;
                }
                l.reap()?;
            }
            l.send().await?;
            Ok(())
        }
        .await;
        // Never return with statements running: every one is waited for
        // (and counted), also after an error. They're let finish rather
        // than stopped: a statement stopped half-way can report fewer
        // documents than it wrote (measured), and progress must be exact.
        let r = match r {
            Ok(()) => l.drain().await,
            Err(e) => {
                let _ = l.drain().await;
                Err(e)
            }
        };
        l.count(0, true);
        r.map(|()| l.done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn results_stream_in_any_split() {
        let reply = br#"{"requestID":"x","signature":{"k":"json","v":"json"},"results":[{"k":"a","v":{"s":"x,]}\"[","n":[1,{"z":2}]}},{"k":"b","v":7}
,{"k":"c","v":null}],"status":"success","metrics":{"resultCount":3}}"#;
        for cut in 0..reply.len() {
            let mut s = ResultsScanner::default();
            let mut got = s.feed(&reply[..cut]).unwrap();
            got.extend(s.feed(&reply[cut..]).unwrap());
            assert_eq!(got.len(), 3, "cut {cut}");
            assert_eq!(got[0]["v"]["s"], "x,]}\"[");
            assert_eq!(got[2], json!({"k": "c", "v": null}));
            s.finish().unwrap();
        }
        // Byte by byte.
        let mut s = ResultsScanner::default();
        let n: usize = reply.chunks(1).map(|c| s.feed(c).unwrap().len()).sum();
        assert_eq!(n, 3);
        s.finish().unwrap();
    }

    #[test]
    fn results_errors_are_reported() {
        let mut s = ResultsScanner::default();
        assert_eq!(s.feed(br#"{"requestID":"x","results":[{"k":"a"}],"errors":[{"code":5000,"msg":"boom"}],"status":"errors"}"#).unwrap().len(), 1);
        assert!(matches!(s.finish(), Err(e) if e.is_query() && e.to_string() == "boom"));
        let mut s = ResultsScanner::default();
        assert!(s.feed(br#"{"requestID":"x","errors":[{"code":3000,"msg":"syntax error"}],"status":"fatal"}"#).unwrap().is_empty());
        assert!(matches!(s.finish(), Err(e) if e.is_query() && e.to_string() == "syntax error"));
        let mut s = ResultsScanner::default();
        s.feed(br#"{"results":[{"k":1},"#).unwrap();
        assert!(s.finish().is_err());
        let mut s = ResultsScanner::default();
        assert!(s.feed(br#"{"results": [ ], "status": "success"}"#).unwrap().is_empty());
        s.finish().unwrap();
    }

    #[test]
    fn rows_become_insert_entries() {
        let n = names(&["_id", "title", "meta", "gone", "blob", "price", "big"]);
        let row = vec![
            Cell::Int(7),
            Cell::Text("O'Dune".into()),
            Cell::Json("{\"x\":[1,2]}".into()),
            Cell::Null,
            Cell::Bytes(vec![1, 255]),
            Cell::Decimal("1.50".into()),
            Cell::UInt(u64::MAX),
        ];
        assert_eq!(
            row_entry(&n, key_column(&n), &row),
            json!({"k": "7", "v": {"title": "O'Dune", "meta": {"x": [1, 2]}, "blob": "0x01FF", "price": "1.50", "big": u64::MAX}})
        );
        // `id` is the key and stays in the document; no key: none sent (UUID()).
        let n = names(&["id", "a"]);
        assert_eq!(row_entry(&n, key_column(&n), &[Cell::Text("k".into()), Cell::Bool(true)]), json!({"k": "k", "v": {"id": "k", "a": true}}));
        assert_eq!(row_entry(&n, key_column(&n), &[Cell::Null, Cell::Int(1)]), json!({"v": {"a": 1}}));
        let n = names(&["a"]);
        assert_eq!(row_entry(&n, key_column(&n), &[Cell::Float(1.5)]), json!({"v": {"a": 1.5}}));
        assert!(insert_statement("`b`.`s`.`c`").starts_with("INSERT INTO `b`.`s`.`c` (KEY IFMISSINGORNULL(_k, UUID()), VALUE _v)"));
    }

    #[test]
    fn documents_become_rows() {
        let doc = json!({"n": 5, "f": 1.5, "big": u64::MAX, "ok": false, "tags": ["p"], "o": {"k": null}, "s": "t", "nul": null});
        let shape = RowShape::discovered(doc.as_object().unwrap().keys().rev().cloned().collect()).unwrap();
        assert_eq!(shape.names, ["_id", "big", "f", "n", "nul", "o", "ok", "s", "tags"]);
        let row = shape.row("k1", &doc).unwrap();
        let by = |n: &str| row[shape.names.iter().position(|c| c == n).unwrap()].clone();
        assert_eq!(by("_id"), Cell::Text("k1".into()));
        assert_eq!(by("n"), Cell::Int(5));
        assert_eq!(by("f"), Cell::Float(1.5));
        assert_eq!(by("big"), Cell::UInt(u64::MAX));
        assert_eq!(by("ok"), Cell::Bool(false));
        assert_eq!(by("tags"), Cell::Json("[\"p\"]".into()));
        assert_eq!(by("o"), Cell::Json("{\"k\":null}".into()));
        assert_eq!(by("s"), Cell::Text("t".into()));
        // An explicit null isn't a missing field.
        assert_eq!(by("nul"), Cell::Json("null".into()));
        let row = shape.row("k2", &json!({"n": 1})).unwrap();
        assert_eq!(row[shape.names.iter().position(|c| c == "nul").unwrap()], Cell::Null);
    }

    #[test]
    fn explicit_nulls_and_missing_fields_round_trip() {
        let shape = RowShape::discovered(names(&["a", "b"])).unwrap();
        for doc in [json!({"a": null, "b": 1}), json!({"b": 1}), json!({"a": null})] {
            let row = shape.row("k", &doc).unwrap();
            let n = &shape.names;
            assert_eq!(row_entry(n, key_column(n), &row), json!({"k": "k", "v": doc}));
        }
    }

    #[test]
    fn fields_found_later_are_columns_or_fail() {
        // Every field the documents have is a column (the server gathers them).
        let shape = RowShape::discovered(names(&["a", "extra"])).unwrap();
        assert_eq!(shape.row("pr99999", &json!({"a": 1, "extra": "x"})).unwrap(), vec![Cell::Text("pr99999".into()), Cell::Int(1), Cell::Text("x".into())]);
        // A field that appears after they were gathered fails the read.
        let e = shape.row("late", &json!({"a": 1, "new": 2})).unwrap_err();
        assert!(matches!(&e, Error::Query(m) if m.contains("«new»")), "{e:?}");
        // Asked-for columns: other fields are left out on purpose.
        let asked = RowShape::asked(names(&["_id", "a"]));
        assert_eq!(asked.row("k", &json!({"a": 1, "new": 2})).unwrap(), vec![Cell::Text("k".into()), Cell::Int(1)]);
    }

    #[test]
    fn non_object_documents_fail() {
        let shape = RowShape::discovered(names(&["a"])).unwrap();
        for doc in [json!([1, 2, 3]), json!("hello"), json!(7), json!(true), Value::Null] {
            let e = shape.row("sh", &doc).unwrap_err();
            assert!(matches!(&e, Error::Unsupported(m) if m.contains("no es un objeto JSON")), "{doc}: {e:?}");
            assert!(RowShape::asked(names(&["_id", "a"])).row("sh", &doc).is_err());
        }
    }

    #[test]
    fn a_field_named_id_keeps_its_value() {
        // The key goes as `meta_id`, the document's own `_id` as a field.
        let shape = RowShape::discovered(names(&["_id", "x"])).unwrap();
        assert_eq!(shape.names, ["meta_id", "_id", "x"]);
        let doc = json!({"_id": "inner", "x": 1});
        let row = shape.row("sh_idfield", &doc).unwrap();
        assert_eq!(row, vec![Cell::Text("sh_idfield".into()), Cell::Text("inner".into()), Cell::Int(1)]);
        let n = &shape.names;
        assert_eq!(row_entry(n, key_column(n), &row), json!({"k": "sh_idfield", "v": {"_id": "inner", "x": 1}}));
        // Asking only for `_id` (the key) would hide the field: it fails.
        assert!(matches!(RowShape::asked(names(&["_id", "x"])).row("k", &doc), Err(Error::Unsupported(_))));
        assert_eq!(RowShape::asked(names(&["meta_id", "_id"])).row("k", &doc).unwrap(), vec![Cell::Text("k".into()), Cell::Text("inner".into())]);
        // Both names taken: no name left for the key.
        assert!(matches!(RowShape::discovered(names(&["_id", "meta_id"])), Err(Error::Unsupported(_))));
    }

    #[test]
    fn a_field_named_meta_id_keeps_its_value_and_the_key() {
        // The key goes as `_id` (first), the document's own `meta_id` as a field.
        let shape = RowShape::discovered(names(&["x", "meta_id"])).unwrap();
        assert_eq!(shape.names, ["_id", "meta_id", "x"]);
        let n = &shape.names;
        for (key, doc) in [("k1", json!({"meta_id": "m1", "x": 1})), ("k2", json!({"x": 2})), ("k3", json!({"meta_id": null}))] {
            let row = shape.row(key, &doc).unwrap();
            assert_eq!(row[0], Cell::Text(key.into()));
            // Same key, same document: nothing re-keyed, dropped or invented.
            assert_eq!(row_entry(n, key_column(n), &row), json!({"k": key, "v": doc}), "{key}");
        }
    }

    #[test]
    fn read_and_load_agree_on_the_key() {
        // With both names, the first one is the key on both sides.
        for cols in [&["_id", "meta_id", "x"][..], &["meta_id", "_id", "x"], &["x", "meta_id", "_id"], &["x", "_id"], &["meta_id"]] {
            let n = names(cols);
            assert_eq!(RowShape::asked(n.clone()).key, key_column(&n), "{cols:?}");
        }
        // `id` / `key` only when there's neither (they stay in the document).
        assert_eq!(key_column(&names(&["id", "meta_id"])), Some(1));
        assert_eq!(key_column(&names(&["key", "id"])), Some(1));
        assert_eq!(RowShape::asked(names(&["id"])).key, None);
        let n = names(&["meta_id", "_id", "x"]);
        assert_eq!(row_entry(&n, key_column(&n), &[Cell::Text("k".into()), Cell::Text("f".into()), Cell::Int(1)]), json!({"k": "k", "v": {"_id": "f", "x": 1}}));
        let n = names(&["_id", "meta_id", "x"]);
        assert_eq!(row_entry(&n, key_column(&n), &[Cell::Text("k".into()), Cell::Text("f".into()), Cell::Int(1)]), json!({"k": "k", "v": {"meta_id": "f", "x": 1}}));
    }

    #[test]
    fn a_stale_interrupt_is_cleared_when_a_transfer_starts_and_ends() {
        let cancel = Arc::new(Cancel::default());
        cancel.flag.store(true, Ordering::SeqCst);
        {
            let _c = Current::set(&cancel, Some("id1"));
            assert!(!cancel.flag.load(Ordering::SeqCst), "a stale interrupt cancels nothing");
            assert_eq!(cancel.current.lock().unwrap().as_deref(), Some("id1"));
            cancel.flag.store(true, Ordering::SeqCst);
        }
        assert!(!cancel.flag.load(Ordering::SeqCst));
        assert!(cancel.current.lock().unwrap().is_none());
        // A load: the interrupter gets no statement to stop, only the flag.
        *cancel.current.lock().unwrap() = Some("stale".into());
        {
            let _c = Current::set(&cancel, None);
            assert!(cancel.current.lock().unwrap().is_none());
        }
        assert!(!cancel.flag.load(Ordering::SeqCst));
        assert!(cancel.current.lock().unwrap().is_none());
    }

    #[test]
    fn field_discovery_filters_in_its_own_subquery() {
        let s = discovery_statement("`b`.`s`.`c`", " WHERE a = 1");
        assert_eq!(
            s,
            "SELECT RAW dbine_f FROM (SELECT RAW d FROM `b`.`s`.`c` AS d WHERE a = 1) AS dbine_d UNNEST OBJECT_NAMES(dbine_d) AS dbine_f GROUP BY dbine_f"
        );
        assert!(discovery_statement("`c`", "").contains("FROM `c` AS d) AS dbine_d"));
    }

    #[test]
    fn entries_are_json_and_bounded() {
        let n = names(&["_id", "s"]);
        let e = entry_bytes(&n, key_column(&n), &[Cell::Text("k\"1".into()), Cell::Text("a\u{1}".into())], 1).unwrap();
        assert_eq!(serde_json::from_slice::<Value>(&e).unwrap(), json!({"k": "k\"1", "v": {"s": "a\u{1}"}}));
        let big = "x".repeat(MAX_DOC);
        let e = entry_bytes(&n, key_column(&n), &[Cell::Text("k".into()), Cell::Text(big)], 3).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("fila 3")), "{e:?}");
    }
}
