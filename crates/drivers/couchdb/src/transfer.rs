//! Bulk transfer (see `dbine_driver::transfer`) for CouchDB.
//!
//! Reading: the session's database. The ids are walked ahead ([`ID_PAGE`]
//! per request, no documents: cheap): `_all_docs` by `startkey`, or, with a
//! filter (a Mango selector, e.g. `{"type": "book"}`), `_find` with only
//! `_id` paged by its bookmark. The documents are fetched by those ids
//! (`POST _all_docs?include_docs=true&attachments=true`), up to
//! [`READ_AHEAD`] fetches at once, each sized to about [`FETCH_BYTES`] by
//! the last reply's document size (growing at most twofold per reply, since
//! small documents often come before large ones) and cut off at
//! [`FETCH_MAX_BYTES`] of reply (its ids are then fetched again in halves),
//! so memory is bounded by bytes, not rows: a document larger than that is
//! fetched whole, one such fetch at a time and with no other fetch started
//! next to it. Design documents are left out, as the browse does.
//!
//! Replies are scanned without going through `serde_json::Value`, so every
//! value arrives exactly as CouchDB stored it: integers past 64 bits and
//! numbers past a double's range become JSON cells with their exact text,
//! doubles are parsed correctly rounded, nested objects and arrays are
//! their exact JSON text. An explicit `null` is the JSON cell `null`; a
//! missing field is a null cell, so the two stay apart on the way back.
//!
//! Without asked-for columns, a first pass (fields only) gathers every
//! top-level field of every document, `_id` first and then in order of
//! appearance (CouchDB has no schema to read them from); a field that shows
//! up only during the copy (the database changed meanwhile) fails the read
//! instead of being dropped. Asked-for columns that no document has fail
//! the read too, before the first row. Attachments travel inline (`_attachments` with each one's
//! content as base64). Views are read through their browse request.
//!
//! Loading: `_bulk_docs` with at most [`CHUNK`] documents and
//! [`CHUNK_BYTES`] of body per request (and never more than a commit
//! window), up to [`IN_FLIGHT`] at once. A row means what it means in the
//! insert script: `_rev` and null cells are left out, JSON cells go nested
//! as their exact text, binaries as `0x…` hex, whole numbers stay numbers;
//! `_id` is always sent as text (CouchDB only takes string ids);
//! attachments are sent with their content (a stub without it fails the
//! load). Each reply is checked document by document and the load fails
//! with the first errors (a conflict: the `_id` already exists). A failed
//! or cancelled load waits for the requests the server already has before
//! it returns (the server commits them even if the client stops waiting),
//! so no row lands after it.

use crate::CouchSession;
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{kinds, Error, Result};
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet, VecDeque};
use std::time::Duration;
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::{JoinHandle, JoinSet};

/// Ids per `_all_docs` / `_find` id page (ids only, cheap).
const ID_PAGE: usize = 5_000;
/// Documents per fetch: at most this many…
const FETCH_DOCS: usize = 1_000;
/// …and about this many bytes of reply, going by the last reply's
/// documents.
const FETCH_BYTES: usize = 2 * 1024 * 1024;
/// A fetch whose reply grows past this is dropped half-read and its ids
/// fetched again in halves (documents vary: the last reply's size is only a
/// guess), so a fetch never holds more, unless it's a single document.
const FETCH_MAX_BYTES: usize = 6 * 1024 * 1024;
/// Fetches in flight at once (the one being handed over included).
const READ_AHEAD: usize = 4;
/// Documents in the first fetch, before any size is known.
const FIRST_FETCH: usize = 50;
/// Documents per `_bulk_docs` request: at most this many…
pub(crate) const CHUNK: usize = 1_000;
/// …and this many bytes of body (a larger document goes alone).
pub(crate) const CHUNK_BYTES: usize = 4 * 1024 * 1024;
/// `_bulk_docs` requests in flight at once.
pub(crate) const IN_FLIGHT: usize = 4;
/// One read page or one `_bulk_docs` request.
const REQUEST_TIMEOUT: Duration = Duration::from_secs(300);

/// A document as its top-level fields, in order.
type Doc = Vec<(String, Cell)>;

/// A document id as a JSON key in a query string.
fn key_param(id: &str) -> Result<String> {
    let key = serde_json::to_string(id)?;
    Ok(percent_encoding::utf8_percent_encode(&key, percent_encoding::NON_ALPHANUMERIC).to_string())
}

fn is_design(id: &str) -> bool {
    id.starts_with("_design/")
}

/// A minimal scanner over CouchDB's JSON replies: values come out as their
/// exact text (serde_json, without `arbitrary_precision`, turns integers
/// past 64 bits into doubles, and its default float parsing may be off by
/// one unit in the last place).
struct Scan<'a> {
    b: &'a [u8],
    i: usize,
}

impl<'a> Scan<'a> {
    fn new(b: &'a [u8]) -> Self {
        Scan { b, i: 0 }
    }

    fn bad(&self) -> Error {
        Error::Query(format!("Respuesta inesperada de CouchDB: JSON inválido cerca del byte {}", self.i))
    }

    fn peek(&mut self) -> Option<u8> {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.b.get(self.i) {
            self.i += 1;
        }
        self.b.get(self.i).copied()
    }

    fn eat(&mut self, c: u8) -> Result<()> {
        if self.peek() != Some(c) {
            return Err(self.bad());
        }
        self.i += 1;
        Ok(())
    }

    /// Past a string (at its opening quote).
    fn skip_string(&mut self) -> Result<()> {
        self.i += 1;
        loop {
            match self.b.get(self.i) {
                None => return Err(self.bad()),
                Some(b'\\') => self.i += 2,
                Some(b'"') => {
                    self.i += 1;
                    return Ok(());
                }
                Some(_) => self.i += 1,
            }
        }
    }

    /// One value, as its text.
    fn value(&mut self) -> Result<&'a [u8]> {
        let c = self.peek().ok_or_else(|| self.bad())?;
        let start = self.i;
        match c {
            b'"' => self.skip_string()?,
            b'{' | b'[' => {
                let mut depth = 0usize;
                loop {
                    match self.b.get(self.i) {
                        None => return Err(self.bad()),
                        Some(b'"') => {
                            self.skip_string()?;
                            continue;
                        }
                        Some(b'{' | b'[') => depth += 1,
                        Some(b'}' | b']') => {
                            depth -= 1;
                            if depth == 0 {
                                self.i += 1;
                                break;
                            }
                        }
                        Some(_) => {}
                    }
                    self.i += 1;
                }
            }
            _ => {
                while let Some(x) = self.b.get(self.i) {
                    if matches!(x, b',' | b'}' | b']' | b' ' | b'\t' | b'\n' | b'\r') {
                        break;
                    }
                    self.i += 1;
                }
            }
        }
        if self.i == start {
            return Err(self.bad());
        }
        Ok(&self.b[start..self.i])
    }

    /// Each member of an object; `f` must take the member's value.
    fn object(&mut self, mut f: impl FnMut(String, &mut Self) -> Result<()>) -> Result<()> {
        self.eat(b'{')?;
        if self.peek() == Some(b'}') {
            self.i += 1;
            return Ok(());
        }
        loop {
            if self.peek() != Some(b'"') {
                return Err(self.bad());
            }
            let key: String = serde_json::from_slice(self.value()?)?;
            self.eat(b':')?;
            f(key, self)?;
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b'}') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(self.bad()),
            }
        }
    }

    /// Each element of an array; `f` must take the element.
    fn array(&mut self, mut f: impl FnMut(&mut Self) -> Result<()>) -> Result<()> {
        self.eat(b'[')?;
        if self.peek() == Some(b']') {
            self.i += 1;
            return Ok(());
        }
        loop {
            f(self)?;
            match self.peek() {
                Some(b',') => self.i += 1,
                Some(b']') => {
                    self.i += 1;
                    return Ok(());
                }
                _ => return Err(self.bad()),
            }
        }
    }
}

/// A number's text as a cell, exactly: 64-bit integers as integers,
/// doubles correctly rounded, anything wider as a JSON cell with its text.
pub(crate) fn number_cell(t: &str) -> Cell {
    if !t.contains(['.', 'e', 'E']) {
        if let Ok(i) = t.parse::<i64>() {
            return Cell::Int(i);
        }
        if let Ok(u) = t.parse::<u64>() {
            return Cell::UInt(u);
        }
    } else if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() {
            return Cell::Float(f);
        }
    }
    Cell::Json(t.to_string())
}

/// A JSON value's text as a cell. An explicit `null` is the JSON cell
/// `null` (a null cell is a missing field); nested values keep their text.
fn raw_cell(v: &[u8]) -> Result<Cell> {
    let text = std::str::from_utf8(v).map_err(|e| Error::Query(format!("Respuesta inesperada de CouchDB: {e}")))?;
    Ok(match v.first() {
        Some(b'"') => Cell::Text(serde_json::from_str(text)?),
        Some(b'{' | b'[') => Cell::Json(text.to_string()),
        _ => match text {
            "null" => Cell::Json("null".into()),
            "true" => Cell::Bool(true),
            "false" => Cell::Bool(false),
            n if is_number(n) => number_cell(n),
            _ => return Err(Error::Query(format!("Respuesta inesperada de CouchDB: valor «{text}»"))),
        },
    })
}

/// JSON number syntax: `-?(0|[1-9]d*)(.d+)?([eE][+-]?d+)?`.
fn is_number(t: &str) -> bool {
    let b = t.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let start = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - start
    };
    let int = i;
    if digits(&mut i) == 0 || (b[int] == b'0' && i - int > 1) {
        return false;
    }
    if b.get(i) == Some(&b'.') {
        i += 1;
        if digits(&mut i) == 0 {
            return false;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        i += 1;
        if matches!(b.get(i), Some(b'+' | b'-')) {
            i += 1;
        }
        if digits(&mut i) == 0 {
            return false;
        }
    }
    i == b.len()
}

/// Walk a whole value, checking it.
fn check_value(s: &mut Scan) -> Result<()> {
    match s.peek() {
        Some(b'{') => s.object(|_, s| check_value(s)),
        Some(b'[') => s.array(check_value),
        _ => raw_cell(s.value()?).map(drop),
    }
}

/// Valid JSON (numbers of any size, which serde_json can't take past a
/// double's range).
fn is_json(t: &str) -> bool {
    let mut s = Scan::new(t.as_bytes());
    check_value(&mut s).is_ok() && s.peek().is_none()
}

/// A document's fields; with `keys_only` the values are left as null cells
/// (only `_id` is read).
fn parse_doc(s: &mut Scan, keys_only: bool) -> Result<Doc> {
    let mut d = Vec::new();
    s.object(|k, s| {
        let v = s.value()?;
        let cell = if keys_only && k != "_id" { Cell::Null } else { raw_cell(v)? };
        d.push((k, cell));
        Ok(())
    })?;
    Ok(d)
}

fn doc_id(d: &Doc) -> Option<&str> {
    d.iter().find(|(k, _)| k == "_id").and_then(|(_, c)| if let Cell::Text(s) = c { Some(s.as_str()) } else { None })
}

/// The documents of an `_all_docs?include_docs=true` reply: deleted and
/// missing ids, and design documents, left out.
fn reply_docs(body: &[u8], keys_only: bool) -> Result<Vec<Doc>> {
    let mut out = Vec::new();
    let mut s = Scan::new(body);
    s.object(|k, s| {
        if k != "rows" {
            return s.value().map(drop);
        }
        s.array(|s| {
            let mut doc = None;
            s.object(|k, s| {
                if k == "doc" && s.peek() == Some(b'{') {
                    doc = Some(parse_doc(s, keys_only)?);
                    Ok(())
                } else {
                    s.value().map(drop)
                }
            })?;
            if let Some(d) = doc.filter(|d| !doc_id(d).is_some_and(is_design)) {
                out.push(d);
            }
            Ok(())
        })
    })?;
    Ok(out)
}

/// Every top-level field, `_id` first, then in order of appearance.
#[derive(Default)]
struct KeyUnion {
    keys: Vec<String>,
    seen: HashSet<String>,
    has_id: bool,
}

impl KeyUnion {
    fn add(&mut self, d: &Doc) {
        for (k, _) in d {
            if k == "_id" {
                self.has_id = true;
            } else if !self.seen.contains(k) {
                self.seen.insert(k.clone());
                self.keys.push(k.clone());
            }
        }
    }

    fn finish(mut self) -> Vec<String> {
        if self.has_id {
            self.keys.insert(0, "_id".into());
        }
        self.keys
    }
}

/// Where the id walk is.
enum Walk {
    All { next: Option<String>, more: bool },
    Find { selector: Value, bookmark: Option<String>, more: bool },
}

impl Walk {
    fn new(selector: Option<&Value>) -> Walk {
        match selector {
            Some(s) => Walk::Find { selector: s.clone(), bookmark: None, more: true },
            None => Walk::All { next: None, more: true },
        }
    }
}

/// A spawned fetch, aborted when dropped (a failed or cancelled read
/// leaves nothing running; aborting a read has no effect on the server).
struct Task<T>(JoinHandle<T>);

impl<T> Drop for Task<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

/// A fetch: its documents and the reply's bytes, or `None` when the reply
/// grew past [`FETCH_MAX_BYTES`] (it was dropped half-read).
type Fetch = Task<Result<Option<(Vec<Doc>, usize)>>>;

/// A fetch in order: its ids, and the request once it's started.
struct Slot {
    ids: Vec<String>,
    task: Option<Fetch>,
    /// A single document whose reply was cut off: it's fetched again whole.
    cut: bool,
    /// Started without a cut-off (see [`Reader::fill`]).
    whole: bool,
}

impl Slot {
    fn new(ids: Vec<String>) -> Slot {
        Slot { ids, task: None, cut: false, whole: false }
    }
}

/// The documents of a database, fetch by fetch, in id order.
struct Reader<'s> {
    s: &'s CouchSession,
    db: String,
    walk: Walk,
    ids: VecDeque<String>,
    per_fetch: usize,
    keys_only: bool,
    pending: VecDeque<Slot>,
    /// The last document seen was past [`FETCH_MAX_BYTES`] (so is the next
    /// one, likely): single documents are fetched whole, one at a time.
    big: bool,
}

impl<'s> Reader<'s> {
    fn new(s: &'s CouchSession, db: &str, walk: Walk, keys_only: bool) -> Self {
        Reader {
            s,
            db: db.to_string(),
            walk,
            ids: VecDeque::new(),
            per_fetch: FIRST_FETCH,
            keys_only,
            pending: VecDeque::new(),
            big: false,
        }
    }

    /// Whether a fetch goes without a cut-off: a single document that was
    /// cut off, or that's likely past the cut-off.
    fn wants_whole(&self, s: &Slot) -> bool {
        s.ids.len() == 1 && (s.cut || self.big)
    }

    /// Start the `i`th fetch.
    fn start(&mut self, i: usize) {
        let whole = self.wants_whole(&self.pending[i]);
        let t = self.spawn(&self.pending[i].ids, (!whole).then_some(FETCH_MAX_BYTES));
        let s = &mut self.pending[i];
        s.task = Some(t);
        s.whole = whole;
    }

    /// The next fetch's ids; `None` past the last one.
    async fn take(&mut self) -> Result<Option<Vec<String>>> {
        while self.ids.len() < self.per_fetch {
            match self.s.next_ids(&self.db, &mut self.walk).await? {
                Some(ids) => self.ids.extend(ids),
                None => break,
            }
        }
        if self.ids.is_empty() {
            return Ok(None);
        }
        let n = self.per_fetch.min(self.ids.len());
        Ok(Some(self.ids.drain(..n).collect()))
    }

    fn spawn(&self, ids: &[String], cap: Option<usize>) -> Fetch {
        // Fields only: attachments as stubs; otherwise with their content.
        let q = if self.keys_only { "include_docs=true" } else { "include_docs=true&attachments=true" };
        let rq = self.s.request(reqwest::Method::POST, &format!("{}/_all_docs?{q}", self.db)).json(&json!({ "keys": ids }));
        let keys_only = self.keys_only;
        Task(tokio::spawn(async move {
            Ok(match CouchSession::fetch_capped(rq, cap).await? {
                Some(body) => Some((reply_docs(&body, keys_only)?, body.len())),
                None => None,
            })
        }))
    }

    /// The next documents; `None` at the end.
    async fn next(&mut self) -> Result<Option<Vec<Doc>>> {
        loop {
            self.fill().await?;
            // The next in order goes now, whole or not: whatever runs next
            // to it is cut off at FETCH_MAX_BYTES.
            if self.pending.front().is_some_and(|s| s.task.is_none()) {
                self.start(0);
            }
            let Some(mut slot) = self.pending.pop_front() else { return Ok(None) };
            let Some(mut t) = slot.task.take() else { return Err(Error::State("lectura sin pedido".into())) };
            match (&mut t.0).await.map_err(|e| Error::State(format!("lectura interrumpida: {e}")))?? {
                None => {
                    // Cut off: again in smaller fetches, before anything
                    // after them; a single document, whole.
                    if slot.ids.len() == 1 {
                        slot.cut = true;
                        self.big = true;
                    }
                    self.per_fetch = self.per_fetch.min(slot.ids.len().div_ceil(2));
                    self.pending.push_front(slot);
                    self.resize();
                }
                Some((docs, bytes)) => {
                    if !docs.is_empty() {
                        let each = (bytes / docs.len()).max(1);
                        self.big = each > FETCH_MAX_BYTES;
                        let fit = (FETCH_BYTES / each).clamp(1, FETCH_DOCS);
                        // Down at once, up at most twofold: a run of small
                        // documents says little about the next ones.
                        self.per_fetch = fit.min(self.per_fetch.saturating_mul(2));
                        self.resize();
                    }
                    // The next fetches run while these are handed over.
                    self.fill().await?;
                    return Ok(Some(docs));
                }
            }
        }
    }

    /// Split the fetches that are too large for the documents seen by now:
    /// a waiting one past [`Self::per_fetch`], a running one past twice that
    /// (dropped: a read has no effect on the server).
    fn resize(&mut self) {
        let n = self.per_fetch;
        for s in std::mem::take(&mut self.pending) {
            let limit = if s.task.is_some() { 2 * n } else { n };
            if s.ids.len() > limit {
                self.pending.extend(s.ids.chunks(n).map(|c| Slot::new(c.to_vec())));
            } else {
                self.pending.push_back(s);
            }
        }
    }

    /// Start fetches, in order, until [`READ_AHEAD`] are running. A fetch
    /// without a cut-off (a single document past [`FETCH_MAX_BYTES`]) holds
    /// what the document is, so it runs alone: it's started only when
    /// nothing else runs, and nothing is started next to it. The bytes in
    /// flight stay at one such document, or at most [`READ_AHEAD`] cut-offs.
    async fn fill(&mut self) -> Result<()> {
        if self.pending.iter().any(|s| s.task.is_some() && s.whole) {
            return Ok(());
        }
        let mut running = self.pending.iter().filter(|s| s.task.is_some()).count();
        let mut i = 0;
        while running < READ_AHEAD {
            if i == self.pending.len() {
                match self.take().await? {
                    Some(ids) => self.pending.push_back(Slot::new(ids)),
                    None => break,
                }
            }
            if self.pending[i].task.is_none() {
                if self.wants_whole(&self.pending[i]) {
                    if running == 0 {
                        self.start(i);
                    }
                    break;
                }
                self.start(i);
                running += 1;
            }
            i += 1;
        }
        Ok(())
    }
}

/// A `_bulk_docs` reply: how many documents failed and the first reasons.
pub(crate) fn bulk_errors(reply: &Value, docs: usize) -> Option<String> {
    let errors: Vec<String> = reply
        .as_array()?
        .iter()
        .filter_map(|r| {
            let e = r.get("error")?;
            let text = |v: Option<&Value>| match v {
                Some(Value::String(s)) => s.clone(),
                Some(v) => v.to_string(),
                None => String::new(),
            };
            Some(format!("{} ({}: {})", text(r.get("id")), text(Some(e)), text(r.get("reason"))))
        })
        .collect();
    if errors.is_empty() {
        return None;
    }
    Some(format!(
        "_bulk_docs: fallaron {} de {docs} documentos. Primeros errores: {}",
        errors.len(),
        errors.iter().take(3).cloned().collect::<Vec<_>>().join("; ")
    ))
}

/// `_attachments` as `_bulk_docs` takes them: each one's content type and
/// content (base64). A stub (the content left on the server) can't be
/// copied.
fn attachments(j: &str) -> Result<Value> {
    let v: Value = serde_json::from_str(j).map_err(|e| Error::Query(format!("_attachments no es JSON válido: {e}")))?;
    let Value::Object(m) = v else {
        return Err(Error::Query("_attachments tiene que ser un objeto con los adjuntos".into()));
    };
    let mut out = serde_json::Map::new();
    for (name, a) in m {
        let Some(data) = a.get("data").filter(|d| d.is_string()) else {
            return Err(Error::Query(format!(
                "el adjunto «{name}» llega sin su contenido (solo la referencia al original); no se puede copiar"
            )));
        };
        let mut o = serde_json::Map::new();
        if let Some(ct) = a.get("content_type") {
            o.insert("content_type".into(), ct.clone());
        }
        o.insert("data".into(), data.clone());
        out.insert(name, Value::Object(o));
    }
    Ok(Value::Object(out))
}

/// A row as the document the insert script would write, appended to `out`
/// as JSON.
pub(crate) fn write_doc(names: &[String], row: &[Cell], out: &mut Vec<u8>) -> Result<()> {
    out.push(b'{');
    let mut first = true;
    for (n, c) in names.iter().zip(row) {
        if matches!(c, Cell::Null) || n == "_rev" {
            continue;
        }
        if !first {
            out.push(b',');
        }
        first = false;
        serde_json::to_writer(&mut *out, n)?;
        out.push(b':');
        match (n.as_str(), c) {
            ("_id", _) => {
                let id = match c.to_json() {
                    Value::String(s) => s,
                    other => other.to_string(),
                };
                serde_json::to_writer(&mut *out, &id)?;
            }
            ("_attachments", Cell::Json(j)) => serde_json::to_writer(&mut *out, &attachments(j)?)?,
            // Whole numbers as numbers, even past 2^53 (the grid shows those as text).
            (_, Cell::Int(i)) => out.extend_from_slice(i.to_string().as_bytes()),
            (_, Cell::UInt(u)) => out.extend_from_slice(u.to_string().as_bytes()),
            // JSON as its exact text: numbers past 64 bits keep every digit.
            (_, Cell::Json(j)) if is_json(j) => out.extend_from_slice(j.trim().as_bytes()),
            _ => serde_json::to_writer(&mut *out, &c.to_json())?,
        }
    }
    out.push(b'}');
    Ok(())
}

/// The row of `doc` for `names` (a missing field is a null cell). Fields
/// not asked for are left in `doc`.
fn doc_row(doc: &mut HashMap<String, Cell>, names: &[String], seen: &mut [bool]) -> Vec<Cell> {
    let mut row: Vec<Cell> = Vec::with_capacity(names.len());
    for (i, n) in names.iter().enumerate() {
        let cell = match doc.remove(n) {
            Some(c) => {
                seen[i] = true;
                c
            }
            // The same name asked for twice.
            None => names[..i].iter().position(|m| m == n).map_or(Cell::Null, |j| row[j].clone()),
        };
        row.push(cell);
    }
    row
}

/// Committed rows, reported by the load's windows.
struct Committed {
    rows: u64,
    bytes: u64,
    reported_rows: u64,
    reported_bytes: u64,
    every_rows: u64,
    every_bytes: u64,
}

impl Committed {
    fn add(&mut self, (rows, bytes): (u64, u64), progress: Progress<'_>, force: bool) {
        self.rows += rows;
        self.bytes += bytes;
        let window = self.rows - self.reported_rows >= self.every_rows || self.bytes - self.reported_bytes >= self.every_bytes;
        if self.rows > self.reported_rows && (window || force) {
            self.reported_rows = self.rows;
            self.reported_bytes = self.bytes;
            progress(self.rows);
        }
    }
}

/// The `_bulk_docs` requests sent and not yet answered. Dropped with some
/// still out (the load cancelled), it waits for them: the server commits
/// what it already received even when the client stops waiting, so rows
/// would land after the load is gone.
struct InFlight(JoinSet<Result<(u64, u64)>>);

impl InFlight {
    async fn drain(&mut self) {
        while self.0.join_next().await.is_some() {}
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let Ok(h) = Handle::try_current() else { return };
        if h.runtime_flavor() != RuntimeFlavor::MultiThread {
            return;
        }
        let mut set = std::mem::take(&mut self.0);
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        h.spawn(async move {
            while set.join_next().await.is_some() {}
            drop(tx);
        });
        // Off the worker, so the requests keep going while this waits (a
        // runtime shutting down drops the task and ends the wait).
        tokio::task::block_in_place(|| {
            let _ = rx.recv();
        });
    }
}

impl CouchSession {
    fn request(&self, method: reqwest::Method, path: &str) -> reqwest::RequestBuilder {
        let mut rq = self
            .http
            .request(method, format!("{}{path}", self.base))
            .header("Accept", "application/json")
            .timeout(REQUEST_TIMEOUT);
        if let Some((u, p)) = &self.auth {
            rq = rq.basic_auth(u, p.as_deref());
        }
        rq
    }

    /// Send and take the reply's body, failing with the server's error.
    async fn fetch_bytes(rq: reqwest::RequestBuilder) -> Result<Vec<u8>> {
        Ok(Self::fetch_capped(rq, None).await?.unwrap_or_default())
    }

    /// [`Self::fetch_bytes`], but `None` (the reply dropped half-read) as
    /// soon as the reply's body grows past `cap` bytes.
    async fn fetch_capped(rq: reqwest::RequestBuilder, cap: Option<usize>) -> Result<Option<Vec<u8>>> {
        let mut resp = rq.send().await.map_err(|e| Error::Connect(e.to_string()))?;
        let status = resp.status();
        if !status.is_success() {
            let bytes = resp.bytes().await.map_err(|e| Error::Connect(e.to_string()))?;
            let v: Value = serde_json::from_slice(&bytes).unwrap_or(Value::Null);
            let msg = match (v.get("error"), v.get("reason")) {
                (Some(e), Some(r)) => format!("{}: {}", crate::as_text(e), crate::as_text(r)),
                _ => format!("HTTP {status}: {}", String::from_utf8_lossy(&bytes)),
            };
            return Err(if status == reqwest::StatusCode::UNAUTHORIZED { Error::AuthFailed(msg) } else { Error::Query(msg) });
        }
        // Sized once, not doubled as it grows (that holds up to three times
        // the reply): from the reply's length when it says it, otherwise
        // gathered in its pieces and joined at the end.
        let cap = cap.unwrap_or(usize::MAX);
        let mut parts = Vec::new();
        let mut len = 0usize;
        let mut out = Vec::new();
        if let Some(n) = resp.content_length() {
            let n = usize::try_from(n).unwrap_or(usize::MAX);
            if n > cap {
                return Ok(None);
            }
            out.try_reserve_exact(n)
                .map_err(|_| Error::Query(format!("La respuesta de CouchDB ({n} bytes) no entra en memoria.")))?;
        }
        while let Some(c) = resp.chunk().await.map_err(|e| Error::Connect(e.to_string()))? {
            len += c.len();
            if len > cap {
                return Ok(None);
            }
            if out.capacity() > 0 {
                out.extend_from_slice(&c);
            } else {
                parts.push(c);
            }
        }
        if !parts.is_empty() {
            out.try_reserve_exact(len)
                .map_err(|_| Error::Query(format!("La respuesta de CouchDB ({len} bytes) no entra en memoria.")))?;
            for c in parts {
                out.extend_from_slice(&c);
            }
        }
        Ok(Some(out))
    }

    /// Send and parse the reply, failing with the server's error.
    async fn fetch(rq: reqwest::RequestBuilder) -> Result<Value> {
        let bytes = Self::fetch_bytes(rq).await?;
        serde_json::from_slice(&bytes).map_err(|e| Error::Query(format!("Respuesta inesperada de CouchDB: {e}")))
    }

    /// The next page of ids (design documents left out); `None` past the
    /// last one.
    async fn next_ids(&self, db: &str, walk: &mut Walk) -> Result<Option<Vec<String>>> {
        let ids_of = |rows: Option<&Value>, key: &str| -> Vec<String> {
            rows.and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|r| r.get(key).and_then(Value::as_str).map(str::to_string))
                .collect()
        };
        let mut ids = match walk {
            Walk::All { next, more } => {
                if !*more {
                    return Ok(None);
                }
                let mut path = format!("{db}/_all_docs?limit={}", ID_PAGE + 1);
                if let Some(k) = next.as_deref() {
                    path.push_str("&startkey=");
                    path.push_str(&key_param(k)?);
                }
                let page = Self::fetch(self.request(reqwest::Method::GET, &path)).await?;
                let mut ids = ids_of(page.get("rows"), "id");
                // One more than a page: its id is the next page's start.
                *next = if ids.len() > ID_PAGE { ids.pop() } else { None };
                *more = next.is_some();
                ids
            }
            Walk::Find { selector, bookmark, more } => {
                if !*more {
                    return Ok(None);
                }
                let mut body = json!({ "selector": selector, "fields": ["_id"], "limit": ID_PAGE });
                if let Some(b) = bookmark.as_deref() {
                    body["bookmark"] = Value::String(b.to_string());
                }
                let page = Self::fetch(self.request(reqwest::Method::POST, &format!("{db}/_find")).json(&body)).await?;
                let ids = ids_of(page.get("docs"), "_id");
                *bookmark = page.get("bookmark").and_then(Value::as_str).map(str::to_string);
                *more = ids.len() >= ID_PAGE && bookmark.is_some();
                ids
            }
        };
        ids.retain(|i| !is_design(i));
        Ok(Some(ids))
    }

    /// Whether some document of the database has the top-level field `name`.
    async fn has_field(&self, db: &str, name: &str) -> Result<bool> {
        if name.is_empty() || name.contains('\\') || name.starts_with('$') {
            // Names Mango can't spell (it takes an empty one as no field at
            // all): look at every document's fields.
            let mut r = Reader::new(self, db, Walk::new(None), true);
            while let Some(docs) = r.next().await? {
                if docs.iter().any(|d| d.iter().any(|(k, _)| k == name)) {
                    return Ok(true);
                }
            }
            return Ok(false);
        }
        // Mango reads a dot as a path: escaped, it's part of the name.
        let mut field = serde_json::Map::new();
        field.insert(name.replace('.', "\\."), json!({ "$exists": true }));
        let body = json!({ "selector": field, "fields": ["_id"], "limit": 1 });
        let page = Self::fetch(self.request(reqwest::Method::POST, &format!("{db}/_find")).json(&body)).await?;
        Ok(page.get("docs").and_then(Value::as_array).is_some_and(|d| !d.is_empty()))
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        if spec.table.kind == kinds::VIEW {
            return dbine_driver::transfer::read_via_execute(self, spec, sink).await;
        }
        let db = self.db_path()?;
        let selector: Option<Value> = match spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => {
                let v: Value = serde_json::from_str(f)
                    .map_err(|e| Error::Query(format!("El filtro tiene que ser un selector Mango en JSON: {e}")))?;
                // A whole `_find` body works too.
                let sel = v.get("selector").cloned().unwrap_or(v);
                if !sel.is_object() {
                    return Err(Error::Query("El filtro tiene que ser un selector Mango (un objeto JSON).".into()));
                }
                Some(sel)
            }
            None => None,
        };
        let this: &CouchSession = self;
        // Without asked-for columns, every document's fields: CouchDB has
        // no schema, and the columns go out before the first row.
        let asked = spec.columns.is_some();
        let names: Vec<String> = match &spec.columns {
            Some(c) => c.clone(),
            None => {
                let mut keys = KeyUnion::default();
                let mut r = Reader::new(this, &db, Walk::new(selector.as_ref()), true);
                while let Some(docs) = r.next().await? {
                    docs.iter().for_each(|d| keys.add(d));
                }
                let keys = keys.finish();
                if keys.is_empty() {
                    vec!["_id".to_string(), "_rev".to_string()]
                } else {
                    keys
                }
            }
        };
        let cols: Vec<TransferColumn> = names
            .iter()
            .map(|n| TransferColumn {
                name: n.clone(),
                type_name: if n == "_id" || n == "_rev" { "string".into() } else { String::new() },
                nullable: n != "_id",
            })
            .collect();
        // An asked-for column no document has is a mistake, not nulls: it
        // fails before the first row goes out (a `$exists` query, cheap
        // when some document has it).
        if asked {
            for n in names.iter().filter(|n| *n != "_id" && *n != "_rev") {
                if !this.has_field(&db, n).await? {
                    return Err(Error::Query(format!("la lectura no trae la columna «{n}»: ningún documento de la base tiene ese campo")));
                }
            }
        }
        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut seen = vec![false; names.len()];
        let mut builder = BatchBuilder::new();
        let mut r = Reader::new(this, &db, Walk::new(selector.as_ref()), false);
        while let Some(docs) = r.next().await? {
            let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
            for d in docs {
                let mut d: HashMap<String, Cell> = d.into_iter().collect();
                let row = doc_row(&mut d, &names, &mut seen);
                if !asked {
                    if let Some(k) = d.keys().next() {
                        let id = row.first().map(|c| c.to_json().to_string()).unwrap_or_default();
                        return Err(Error::Query(format!(
                            "el documento {id} tiene el campo «{k}», que apareció mientras se copiaba (la base cambió durante la copia); volvé a copiarla"
                        )));
                    }
                }
                builder.push(row, &mut *s)?;
            }
        }
        drop(r);
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        self.refuse_if_read_only("cargar datos")?;
        if spec.table.kind == kinds::VIEW {
            return Err(Error::Unsupported("no se insertan documentos en una vista de CouchDB".into()));
        }
        // A cancelled load waits for its requests off the worker (see `InFlight`).
        if !Handle::try_current().is_ok_and(|h| h.runtime_flavor() == RuntimeFlavor::MultiThread) {
            return Err(Error::Unsupported(
                "la carga masiva de CouchDB necesita el runtime multihilo: sin él no puede esperar los envíos en curso si se cancela".into(),
            ));
        }
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let path = format!("{}/_bulk_docs", self.db_path()?);
        let mut sent = InFlight(JoinSet::new());
        let r = self.load_into(&path, &names, spec, source, progress, &mut sent).await;
        if r.is_err() {
            // What the server already has commits anyway: wait for it
            // before returning, so nothing lands after the error.
            sent.drain().await;
        }
        r
    }

    async fn load_into(
        &self,
        path: &str,
        names: &[String],
        spec: &LoadSpec,
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
        sent: &mut InFlight,
    ) -> Result<u64> {
        let as_usize = |n: u64| usize::try_from(n).unwrap_or(usize::MAX).max(1);
        // A request is never more than a commit window.
        let max_docs = CHUNK.min(as_usize(spec.commit_rows));
        let max_bytes = CHUNK_BYTES.min(as_usize(spec.commit_bytes));
        let mut committed = Committed {
            rows: 0,
            bytes: 0,
            reported_rows: 0,
            reported_bytes: 0,
            every_rows: spec.commit_rows.max(1),
            every_bytes: spec.commit_bytes.max(1),
        };
        const OPEN: &[u8] = b"{\"docs\":[";
        let mut body: Vec<u8> = OPEN.to_vec();
        let mut n = 0usize;
        let mut one: Vec<u8> = Vec::new();
        let send = |body: Vec<u8>, n: usize, sent: &mut InFlight| {
            let mut body = body;
            body.extend_from_slice(b"]}");
            let rq = self.request(reqwest::Method::POST, path).header(reqwest::header::CONTENT_TYPE, "application/json");
            sent.0.spawn(send_docs(rq, body, n));
        };
        while let Some(batch) = source.next().await {
            for row in batch.rows {
                if row.len() != names.len() {
                    return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                }
                one.clear();
                write_doc(names, &row, &mut one)?;
                drop(row);
                if n > 0 && body.len() + one.len() + 3 > max_bytes {
                    while sent.0.len() >= IN_FLIGHT {
                        committed.add(joined(sent.0.join_next().await)?, progress, false);
                    }
                    send(std::mem::replace(&mut body, OPEN.to_vec()), n, sent);
                    n = 0;
                }
                if n > 0 {
                    body.push(b',');
                }
                body.extend_from_slice(&one);
                if one.capacity() > CHUNK_BYTES {
                    one = Vec::new();
                }
                n += 1;
                if n >= max_docs || body.len() >= max_bytes {
                    while sent.0.len() >= IN_FLIGHT {
                        committed.add(joined(sent.0.join_next().await)?, progress, false);
                    }
                    send(std::mem::replace(&mut body, OPEN.to_vec()), n, sent);
                    n = 0;
                }
            }
            while let Some(r) = sent.0.try_join_next() {
                committed.add(joined(Some(r))?, progress, false);
            }
        }
        if n > 0 {
            while sent.0.len() >= IN_FLIGHT {
                committed.add(joined(sent.0.join_next().await)?, progress, false);
            }
            send(body, n, sent);
        }
        while let Some(r) = sent.0.join_next().await {
            committed.add(joined(Some(r))?, progress, false);
        }
        committed.add((0, 0), progress, true);
        Ok(committed.rows)
    }
}

/// One `_bulk_docs` request (`body` is its JSON); the documents and bytes
/// committed.
async fn send_docs(rq: reqwest::RequestBuilder, body: Vec<u8>, docs: usize) -> Result<(u64, u64)> {
    let bytes = body.len() as u64;
    let reply = CouchSession::fetch(rq.body(body)).await?;
    if let Some(msg) = bulk_errors(&reply, docs) {
        return Err(Error::Query(msg));
    }
    Ok((docs as u64, bytes))
}

fn joined(r: Option<std::result::Result<Result<(u64, u64)>, tokio::task::JoinError>>) -> Result<(u64, u64)> {
    match r {
        None => Ok((0, 0)),
        Some(Ok(r)) => r,
        Some(Err(e)) => Err(Error::State(format!("envío de _bulk_docs interrumpido: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn doc_json(names: &[String], row: &[Cell]) -> String {
        let mut out = Vec::new();
        write_doc(names, row, &mut out).unwrap();
        String::from_utf8(out).unwrap()
    }

    fn read_doc(json: &str) -> Doc {
        parse_doc(&mut Scan::new(json.as_bytes()), false).unwrap()
    }

    #[test]
    fn rows_become_documents() {
        let row = vec![
            Cell::Int(7),
            Cell::Text("1-abc".into()),
            Cell::Text("Dune".into()),
            Cell::Json("{\"x\":[1,2]}".into()),
            Cell::Null,
            Cell::Bytes(vec![1, 255]),
            Cell::Decimal("1.50".into()),
            Cell::Json("null".into()),
        ];
        let doc = doc_json(&names(&["_id", "_rev", "title", "meta", "gone", "blob", "price", "none"]), &row);
        assert_eq!(doc, r#"{"_id":"7","title":"Dune","meta":{"x":[1,2]},"blob":"0x01FF","price":"1.50","none":null}"#);
    }

    #[test]
    fn documents_become_rows_and_back_exactly() {
        let src = r#"{"_id":"a","_rev":"1-x","n":5,"f":1.5,"big":18446744073709551615,"huge":123456789012345678901234567890,"neg":-9223372036854775809,"tiny":1.2345678901234568e-300,"ok":false,"tags":["p",1e400],"o":{"k":null,"b":123456789012345678901234567890},"z":null,"e":"","s":"ñ\"\\u"}"#;
        let d = read_doc(src);
        let get = |k: &str| d.iter().find(|(n, _)| n == k).map(|(_, c)| c.clone()).unwrap();
        assert_eq!(get("n"), Cell::Int(5));
        assert_eq!(get("f"), Cell::Float(1.5));
        assert_eq!(get("big"), Cell::UInt(u64::MAX));
        assert_eq!(get("huge"), Cell::Json("123456789012345678901234567890".into()));
        assert_eq!(get("neg"), Cell::Json("-9223372036854775809".into()));
        assert_eq!(get("tiny"), Cell::Float("1.2345678901234568e-300".parse().unwrap()));
        assert_eq!(get("o"), Cell::Json(r#"{"k":null,"b":123456789012345678901234567890}"#.into()));
        // An explicit null isn't a missing field.
        assert_eq!(get("z"), Cell::Json("null".into()));
        assert_eq!(get("s"), Cell::Text("ñ\"\\u".into()));
        // And back: the same document, less `_rev`, every digit kept.
        let names: Vec<String> = d.iter().map(|(n, _)| n.clone()).collect();
        let row: Vec<Cell> = d.iter().map(|(_, c)| c.clone()).collect();
        let back = doc_json(&names, &row);
        assert_eq!(back, src.replace(r#""_rev":"1-x","#, ""));
    }

    #[test]
    fn missing_fields_are_null_cells_and_unasked_ones_stay() {
        let mut d: HashMap<String, Cell> = read_doc(r#"{"_id":"a","b":1,"extra":2}"#).into_iter().collect();
        let n = names(&["_id", "b", "missing", "b"]);
        let mut seen = vec![false; n.len()];
        let row = doc_row(&mut d, &n, &mut seen);
        assert_eq!(row, vec![Cell::Text("a".into()), Cell::Int(1), Cell::Null, Cell::Int(1)]);
        assert_eq!(seen, vec![true, true, false, false]);
        assert_eq!(d.keys().collect::<Vec<_>>(), vec!["extra"]);
    }

    #[test]
    fn replies_leave_out_deleted_missing_and_design_documents() {
        let body = br#"{"total_rows":3,"offset":null,"rows":[
{"id":"b","key":"b","value":{"rev":"1-x"},"doc":{"_id":"b","n":1}},
{"key":"zz","error":"not_found"},
{"id":"c","key":"c","value":{"rev":"2-y","deleted":true},"doc":null},
{"id":"_design/v","key":"_design/v","value":{"rev":"1-z"},"doc":{"_id":"_design/v","views":{}}}
]}"#;
        let docs = reply_docs(body, false).unwrap();
        assert_eq!(docs, vec![vec![("_id".to_string(), Cell::Text("b".into())), ("n".to_string(), Cell::Int(1))]]);
        let keys = reply_docs(body, true).unwrap();
        assert_eq!(keys[0][1], ("n".to_string(), Cell::Null));
        assert!(reply_docs(b"{\"rows\":[{\"doc\":{\"a\":}}]}", false).is_err());
    }

    #[test]
    fn key_union_is_every_documents_fields() {
        let mut u = KeyUnion::default();
        u.add(&read_doc(r#"{"b":1,"_id":"x","_rev":"1"}"#));
        u.add(&read_doc(r#"{"_id":"y","late":1,"b":2}"#));
        assert_eq!(u.finish(), names(&["_id", "b", "_rev", "late"]));
    }

    #[test]
    fn attachments_travel_with_their_content() {
        let n = names(&["_id", "_attachments"]);
        let with = Cell::Json(r#"{"a.txt":{"content_type":"text/plain","revpos":1,"digest":"md5-x","data":"aG9sYQ=="}}"#.into());
        assert_eq!(
            doc_json(&n, &[Cell::Text("a1".into()), with]),
            r#"{"_id":"a1","_attachments":{"a.txt":{"content_type":"text/plain","data":"aG9sYQ=="}}}"#
        );
        let stub = Cell::Json(r#"{"a.txt":{"content_type":"text/plain","stub":true,"length":4}}"#.into());
        let mut out = Vec::new();
        let e = write_doc(&n, &[Cell::Text("a1".into()), stub], &mut out).unwrap_err();
        assert!(e.to_string().contains("sin su contenido"), "{e}");
    }

    #[test]
    fn json_cells_are_checked_without_losing_digits() {
        assert!(is_json(" [1e400, -12345678901234567890123, {\"a\": \"\\u00f1\"}, null] "));
        for bad in ["{\"a\":01}", "{\"a\":1} x", "[1.]", "[-]", "{\"a\" 1}", "[\"\\x\"]", "nul"] {
            assert!(!is_json(bad), "{bad}");
        }
        // Not JSON: sent as text, as the insert script does.
        assert_eq!(doc_json(&names(&["j"]), &[Cell::Json("{nope".into())]), r#"{"j":"{nope"}"#);
    }

    #[test]
    fn per_document_errors() {
        assert_eq!(bulk_errors(&json!([{"id": "a", "ok": true, "rev": "1-x"}]), 1), None);
        let reply = json!([{"id": "a", "ok": true}, {"id": "b", "error": "conflict", "reason": "Document update conflict."}]);
        assert_eq!(
            bulk_errors(&reply, 2).unwrap(),
            "_bulk_docs: fallaron 1 de 2 documentos. Primeros errores: b (conflict: Document update conflict.)"
        );
    }
}
