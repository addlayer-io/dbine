//! Bulk transfer (see `dbine_driver::transfer`) for Elasticsearch,
//! OpenSearch and Open Distro.
//!
//! Reading: Elasticsearch pages with a point in time and `search_after` on
//! `_shard_doc`; OpenSearch, Open Distro and Elasticsearch clusters without
//! point in time use a scroll sorted by `_doc`. Pages are sized by bytes,
//! not only by hits: each one aims at [`PAGE_BYTES`] of response (after the
//! documents seen so far) and none is held beyond [`MAX_RESPONSE`] — with a
//! point in time the page is asked again with fewer hits; a scroll's page
//! size is fixed when it opens, so it is sized after a probe of the
//! largest documents, and a page that still goes over fails the read.
//! Responses are never parsed into a tree (one costs 20-30 times its text
//! for arrays of numbers): [`Scan`] walks the page's text hit by hit and
//! each hit becomes a row straight from slices of that text, so a page in
//! flight is its text plus one row. Nothing is lost: numbers keep their
//! text (a number `f64` can't hold exactly becomes a JSON cell with that
//! text) and keys their order.
//!
//! A full read's rows are `_id`, `_routing`, the index's top-level fields
//! (from its mapping) and a `_source` column: the `_source` keys no other
//! column holds, as a JSON object (fields the mapping doesn't list, and
//! dotted keys such as `"host.name"`, which the mapping shows as an object
//! but `_source` keeps as they came). Asked-for columns may be dotted paths
//! (`author.name`) and must be metadata, `_source` or fields `_source` can
//! hold (not multi-fields such as `title.keyword`, nor aliases). A filter is
//! a Query DSL clause (`{"term": {"genre": "scifi"}}`). A field that is
//! missing is NULL; one that holds an explicit `null` is the JSON cell
//! `null` (they index differently: `null_value`). `binary` fields become
//! bytes.
//!
//! Loading: `_bulk` NDJSON built straight from the cells (never through
//! script text). A request is one commit window: at most `commit_rows` rows
//! and `commit_bytes` bytes (and [`REQUEST_BYTES`]), [`IN_FLIGHT`] at once;
//! progress is reported as each one is acknowledged. The rows mean what
//! they mean in the insert script: `_id` and `_routing` go to the action
//! line, `_index` and `_score` are dropped, a `_source` object's fields are
//! merged into the document, NULLs are left out (a missing field). Only
//! JSON cells are written as nested JSON; text is always a string.
//! Binaries go as Base64 (what a `binary` field takes). Each response is
//! checked item by item and the load fails with the first errors. When the
//! load fails, or is dropped, it waits for the requests in flight before it
//! returns (see [`InFlight`]): the server indexes a request it got whole
//! even if the client stops waiting. After a failure, the rows of those
//! that still commit are reported too, so the last progress is what's
//! committed. The index is refreshed once, at the end.

use crate::{es_error_message, http, json::J, EsSession};
use base64::Engine;
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{kinds, Error, Result};
use serde_json::{json, Value};
use std::borrow::Cow;
use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Condvar, Mutex};
use std::time::Duration;
use tokio::task::JoinSet;

/// Most hits per page of the read.
const PAGE: usize = 5_000;
/// Hits of the first point-in-time page, before document sizes are known.
const FIRST_PAGE: usize = 500;
/// Response bytes a read page aims at.
const PAGE_BYTES: usize = 3 * 1024 * 1024;
/// A read page's response is never held beyond this.
const MAX_RESPONSE: usize = 8 * 1024 * 1024;
/// Hits of the probe that sizes a scroll's pages.
const PROBE: usize = 100;
/// Bytes of an error response kept for its message.
const MAX_ERROR_BODY: usize = 64 * 1024;
/// How long the point in time / scroll stays open between pages.
const KEEP_ALIVE: &str = "5m";
/// Largest `_bulk` request body (a single larger row goes alone).
pub(crate) const REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// `_bulk` requests in flight at once.
pub(crate) const IN_FLIGHT: usize = 4;
/// One page or one `_bulk` request.
const TIMEOUT: Duration = Duration::from_secs(300);
/// Metadata columns: not part of `_source`.
const META: [&str; 4] = ["_id", "_routing", "_index", "_score"];
/// The column with the `_source` fields no other column holds.
const REST: &str = "_source";
/// Nesting a parsed JSON value may have.
const MAX_DEPTH: usize = 1_000;

type Parsed<T> = std::result::Result<T, String>;

/// A JSON scanner: validates text and hands out slices of it (values as
/// their raw text, keys as the raw text between their quotes) without
/// building a tree, so what it reads costs no more than its text.
struct Scan<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl<'a> Scan<'a> {
    fn new(text: &'a str) -> Self {
        Scan { s: text.as_bytes(), text, i: 0 }
    }

    fn err(&self, what: &str) -> String {
        format!("JSON inválido en la posición {}: {what}", self.i)
    }

    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.i += 1;
        }
    }

    fn peek(&self) -> Option<u8> {
        self.s.get(self.i).copied()
    }

    /// Past `c` (`{` / `[`) if that's what comes next.
    fn open(&mut self, c: u8) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    /// Nothing but blanks is left.
    fn done(&mut self) -> Parsed<()> {
        self.ws();
        if self.i == self.s.len() {
            Ok(())
        } else {
            Err(self.err("sobra texto después del valor"))
        }
    }

    /// Inside an object: the next key's raw text (past its `:`), or `None`
    /// past the closing `}`.
    fn key(&mut self, first: bool) -> Parsed<Option<&'a str>> {
        self.ws();
        match self.peek() {
            Some(b'}') => {
                self.i += 1;
                return Ok(None);
            }
            Some(b',') if !first => {
                self.i += 1;
                self.ws();
            }
            _ if first => {}
            _ => return Err(self.err("se esperaba «,» o «}»")),
        }
        if self.peek() != Some(b'"') {
            return Err(self.err("se esperaba una clave"));
        }
        let k = self.string()?;
        self.ws();
        if self.peek() != Some(b':') {
            return Err(self.err("se esperaba «:»"));
        }
        self.i += 1;
        Ok(Some(k))
    }

    /// Inside an array: whether another item follows (false past `]`).
    fn item(&mut self, first: bool) -> Parsed<bool> {
        self.ws();
        match self.peek() {
            Some(b']') => {
                self.i += 1;
                Ok(false)
            }
            Some(b',') if !first => {
                self.i += 1;
                Ok(true)
            }
            _ if first => Ok(true),
            _ => Err(self.err("se esperaba «,» o «]»")),
        }
    }

    /// The next value's raw text.
    fn value(&mut self, depth: usize) -> Parsed<&'a str> {
        self.ws();
        let start = self.i;
        self.skip(depth)?;
        Ok(&self.text[start..self.i])
    }

    fn lit(&mut self, word: &[u8]) -> Parsed<()> {
        if self.s[self.i..].starts_with(word) {
            self.i += word.len();
            Ok(())
        } else {
            Err(self.err("valor desconocido"))
        }
    }

    /// Past one value, checking it.
    fn skip(&mut self, depth: usize) -> Parsed<()> {
        if depth > MAX_DEPTH {
            return Err(self.err("anidamiento demasiado profundo"));
        }
        self.ws();
        match self.peek() {
            None => Err(self.err("falta un valor")),
            Some(b'n') => self.lit(b"null"),
            Some(b't') => self.lit(b"true"),
            Some(b'f') => self.lit(b"false"),
            Some(b'"') => self.string().map(|_| ()),
            Some(b'[') => {
                self.i += 1;
                let mut first = true;
                while self.item(first)? {
                    first = false;
                    self.skip(depth + 1)?;
                }
                Ok(())
            }
            Some(b'{') => {
                self.i += 1;
                let mut first = true;
                while self.key(first)?.is_some() {
                    first = false;
                    self.skip(depth + 1)?;
                }
                Ok(())
            }
            Some(_) => self.number(),
        }
    }

    /// A string, checked; its raw text between the quotes.
    fn string(&mut self) -> Parsed<&'a str> {
        let start = self.i;
        self.i += 1;
        let mut escaped = false;
        loop {
            match self.peek() {
                None => return Err(self.err("texto sin cerrar")),
                Some(b'"') => break,
                Some(b'\\') => {
                    escaped = true;
                    self.i += 2;
                }
                Some(c) if c < 0x20 => return Err(self.err("carácter de control dentro de un texto")),
                Some(_) => self.i += 1,
            }
        }
        self.i += 1;
        // Both ends are quotes: char boundaries.
        let quoted = &self.text[start..self.i];
        if escaped {
            serde_json::from_str::<serde::de::IgnoredAny>(quoted).map_err(|e| self.err(&e.to_string()))?;
        }
        Ok(&quoted[1..quoted.len() - 1])
    }

    fn digits(&mut self) -> usize {
        let from = self.i;
        while matches!(self.peek(), Some(b'0'..=b'9')) {
            self.i += 1;
        }
        self.i - from
    }

    fn number(&mut self) -> Parsed<()> {
        if self.peek() == Some(b'-') {
            self.i += 1;
        }
        let int = self.i;
        let n = self.digits();
        if n == 0 || (n > 1 && self.s[int] == b'0') {
            return Err(self.err("número inválido"));
        }
        if self.peek() == Some(b'.') {
            self.i += 1;
            if self.digits() == 0 {
                return Err(self.err("número inválido"));
            }
        }
        if matches!(self.peek(), Some(b'e' | b'E')) {
            self.i += 1;
            if matches!(self.peek(), Some(b'+' | b'-')) {
                self.i += 1;
            }
            if self.digits() == 0 {
                return Err(self.err("número inválido"));
            }
        }
        Ok(())
    }
}

/// `text` is one valid JSON value.
fn check_json(text: &str) -> Parsed<()> {
    let mut sc = Scan::new(text);
    sc.skip(0)?;
    sc.done()
}

/// An object's entries (raw key, raw value); an error if `raw` isn't one.
fn entries(raw: &str) -> Parsed<Vec<(&str, &str)>> {
    let mut sc = Scan::new(raw);
    if !sc.open(b'{') {
        return Err(sc.err("no es un objeto"));
    }
    let mut out = Vec::new();
    let mut first = true;
    while let Some(k) = sc.key(first)? {
        first = false;
        out.push((k, sc.value(1)?));
    }
    sc.done()?;
    Ok(out)
}

/// A checked string's raw text (between its quotes), unescaped.
fn unescape(raw: &str) -> Cow<'_, str> {
    if !raw.contains('\\') {
        return Cow::Borrowed(raw);
    }
    let mut quoted = String::with_capacity(raw.len() + 2);
    quoted.push('"');
    quoted.push_str(raw);
    quoted.push('"');
    Cow::Owned(serde_json::from_str::<String>(&quoted).unwrap_or_else(|_| raw.to_string()))
}

/// Checked JSON text without the blanks between tokens (on one line: a
/// line break inside a string is always escaped).
fn write_compact(out: &mut Vec<u8>, raw: &str) {
    out.reserve(raw.len());
    let mut in_str = false;
    let mut esc = false;
    for &b in raw.as_bytes() {
        if in_str {
            if esc {
                esc = false;
            } else if b == b'\\' {
                esc = true;
            } else if b == b'"' {
                in_str = false;
            }
        } else if matches!(b, b' ' | b'\n' | b'\r' | b'\t') {
            continue;
        } else if b == b'"' {
            in_str = true;
        }
        out.push(b);
    }
}

fn compact(raw: &str) -> String {
    let mut out = Vec::new();
    write_compact(&mut out, raw);
    // Checked JSON minus ASCII blanks: still UTF-8.
    String::from_utf8(out).unwrap_or_default()
}

/// `"raw key":`, the key as it came (already valid JSON string text).
fn write_key(out: &mut Vec<u8>, raw: &str) {
    out.push(b'"');
    out.extend_from_slice(raw.as_bytes());
    out.extend_from_slice(b"\":");
}

/// How a page ended: its cursor ids, hits and the last hit's sort values.
#[derive(Default)]
struct PageEnd {
    scroll_id: Option<String>,
    pit_id: Option<String>,
    hits: usize,
    /// Raw text of the largest hit.
    largest: usize,
    last_sort: Option<String>,
}

/// One hit, as slices of the page's text.
#[derive(Default)]
struct Hit<'a> {
    index: Option<String>,
    id: Option<String>,
    score: Option<&'a str>,
    routing: Option<String>,
    /// `_source`'s top-level entries: raw key, raw value.
    source: Vec<(&'a str, &'a str)>,
    sort: Option<&'a str>,
}

/// A string value as text; any other value, `None`.
fn text_value(sc: &mut Scan<'_>, depth: usize) -> Parsed<Option<String>> {
    let v = sc.value(depth)?;
    Ok(v.strip_prefix('"').and_then(|v| v.strip_suffix('"')).map(|s| unescape(s).into_owned()))
}

/// One hit; `source` is an empty vector to fill (reused between hits).
fn parse_hit<'a>(sc: &mut Scan<'a>, source: Vec<(&'a str, &'a str)>, depth: usize) -> Parsed<Hit<'a>> {
    let mut h = Hit { source, ..Hit::default() };
    if !sc.open(b'{') {
        sc.skip(depth)?;
        return Ok(h);
    }
    let mut first = true;
    while let Some(k) = sc.key(first)? {
        first = false;
        match k {
            "_index" => h.index = text_value(sc, depth + 1)?,
            "_id" => h.id = text_value(sc, depth + 1)?,
            "_routing" => h.routing = text_value(sc, depth + 1)?,
            "_score" => h.score = Some(sc.value(depth + 1)?),
            "sort" => h.sort = Some(sc.value(depth + 1)?),
            "_source" if sc.open(b'{') => {
                let mut first = true;
                while let Some(k) = sc.key(first)? {
                    first = false;
                    h.source.push((k, sc.value(depth + 2)?));
                }
            }
            _ => sc.skip(depth + 1)?,
        }
    }
    Ok(h)
}

/// Walk a `_search` response: each hit goes to `each` as it's read (and
/// is dropped right after), never the whole page at once.
fn walk_page<'a>(text: &'a str, mut each: impl FnMut(&Hit<'a>) -> Result<()>) -> Result<PageEnd> {
    let bad = |e: String| Error::Query(format!("Respuesta inesperada de _search: {e}"));
    let mut sc = Scan::new(text);
    if !sc.open(b'{') {
        return Err(Error::Query("Respuesta inesperada de _search: no es un objeto".into()));
    }
    let mut end = PageEnd::default();
    let mut last_sort: Option<&str> = None;
    let mut source = Vec::new();
    let mut first = true;
    while let Some(k) = sc.key(first).map_err(bad)? {
        first = false;
        match k {
            "_scroll_id" => end.scroll_id = text_value(&mut sc, 1).map_err(bad)?,
            "pit_id" => end.pit_id = text_value(&mut sc, 1).map_err(bad)?,
            "hits" if sc.open(b'{') => {
                let mut first = true;
                while let Some(k) = sc.key(first).map_err(bad)? {
                    first = false;
                    if k != "hits" || !sc.open(b'[') {
                        sc.skip(2).map_err(bad)?;
                        continue;
                    }
                    let mut first = true;
                    while sc.item(first).map_err(bad)? {
                        first = false;
                        sc.ws();
                        let start = sc.i;
                        let hit = parse_hit(&mut sc, std::mem::take(&mut source), 3).map_err(bad)?;
                        end.hits += 1;
                        end.largest = end.largest.max(sc.i - start);
                        last_sort = hit.sort;
                        each(&hit)?;
                        source = hit.source;
                        source.clear();
                    }
                }
            }
            _ => sc.skip(1).map_err(bad)?,
        }
    }
    sc.done().map_err(bad)?;
    end.last_sort = last_sort.map(str::to_string);
    Ok(end)
}

/// How the read pages.
enum Cursor {
    /// `after`: the last hit's sort values, as JSON text.
    Pit { id: String, after: Option<String> },
    Scroll { id: Option<String> },
}

/// Whether a raw key is `name`.
fn key_is(raw: &str, name: &str) -> bool {
    if raw.contains('\\') {
        unescape(raw) == name
    } else {
        raw == name
    }
}

/// A field of a document: which top-level entry holds it, its raw value
/// and the raw keys that lead to it within that entry (none: the entry is
/// the field). `name` is a key, or a dotted path through nested objects
/// whose keys may be dotted themselves (`{"a.b": {"c": 1}}` holds `a.b.c`).
fn lookup<'a>(src: &[(&'a str, &'a str)], name: &str) -> Option<(usize, &'a str, Vec<&'a str>)> {
    if let Some(i) = src.iter().position(|(k, _)| key_is(k, name)) {
        return Some((i, src[i].1, Vec::new()));
    }
    for (i, (k, v)) in src.iter().enumerate() {
        let Some(rest) = name.strip_prefix(&*unescape(k)).and_then(|r| r.strip_prefix('.')).map(str::to_string) else { continue };
        if !v.starts_with('{') {
            continue;
        }
        let Ok(inner) = entries(v) else { continue };
        if let Some((j, found, mut path)) = lookup(&inner, &rest) {
            path.insert(0, inner[j].0);
            return Some((i, found, path));
        }
    }
    None
}

/// The same slice of the page's text (not an equal key elsewhere).
fn same(a: &str, b: &str) -> bool {
    std::ptr::eq(a.as_ptr(), b.as_ptr()) && a.len() == b.len()
}

/// The object `raw` without the values at `paths` (each the raw keys
/// [`lookup`] gave, taken from this same text), compact; `None` if nothing
/// is left of it. What a column reads of an entry, the rest of the entry
/// is still `_source`'s.
fn without(raw: &str, paths: &[&[&str]]) -> Option<Vec<u8>> {
    let Ok(fields) = entries(raw) else {
        let mut out = Vec::new();
        write_compact(&mut out, raw);
        return Some(out);
    };
    let mut out = Vec::new();
    for (k, v) in fields {
        let under: Vec<&[&str]> = paths.iter().filter(|p| same(p[0], k)).map(|p| &p[1..]).collect();
        let kept = if under.is_empty() {
            let mut b = Vec::new();
            write_compact(&mut b, v);
            Some(b)
        } else if under.iter().any(|p| p.is_empty()) {
            None
        } else {
            without(v, &under)
        };
        if let Some(b) = kept {
            out.push(if out.is_empty() { b'{' } else { b',' });
            write_key(&mut out, k);
            out.extend(b);
        }
    }
    (!out.is_empty()).then(|| {
        out.push(b'}');
        out
    })
}

/// A number's text as a cell: `Int` / `UInt` / `Float` when one holds it
/// and writes it back the same; otherwise a JSON cell with the text.
fn num_cell(t: &str) -> Cell {
    if !t.contains(['.', 'e', 'E']) {
        if let Ok(i) = t.parse::<i64>() {
            if i.to_string() == t {
                return Cell::Int(i);
            }
        } else if let Ok(u) = t.parse::<u64>() {
            return Cell::UInt(u);
        }
    } else if let Ok(f) = t.parse::<f64>() {
        if f.is_finite() && serde_json::to_string(&f).is_ok_and(|s| s == t) {
            return Cell::Float(f);
        }
    }
    Cell::Json(t.to_string())
}

/// A `_source` value (its raw text) as a cell; `ty` is the field's
/// mapping type.
pub(crate) fn to_cell(raw: &str, ty: &str) -> Cell {
    let b64 = &base64::engine::general_purpose::STANDARD;
    match raw.as_bytes().first() {
        None | Some(b'n') => Cell::Null,
        Some(b't') => Cell::Bool(true),
        Some(b'f') => Cell::Bool(false),
        Some(b'"') => {
            let s = unescape(&raw[1..raw.len() - 1]).into_owned();
            if ty == "binary" {
                if let Ok(b) = b64.decode(&s) {
                    // Only canonical Base64: it's written back the same.
                    if b64.encode(&b) == s {
                        return Cell::Bytes(b);
                    }
                }
            }
            Cell::Text(s)
        }
        Some(b'[' | b'{') => Cell::Json(compact(raw)),
        Some(_) => num_cell(raw),
    }
}

fn hit_row(hit: &Hit<'_>, cols: &[TransferColumn]) -> Vec<Cell> {
    let text = |s: &Option<String>| s.as_ref().map_or(Cell::Null, |s| Cell::Text(s.clone()));
    // What columns read of each top-level `_source` entry: the paths
    // within it (an empty one, the whole entry).
    let mut held: Vec<Vec<Vec<&str>>> = vec![Vec::new(); hit.source.len()];
    let mut rest_at = None;
    let mut row: Vec<Cell> = cols
        .iter()
        .enumerate()
        .map(|(i, c)| match c.name.as_str() {
            "_id" => text(&hit.id),
            "_index" => text(&hit.index),
            "_routing" => text(&hit.routing),
            "_score" => hit.score.map_or(Cell::Null, |v| to_cell(v, "")),
            REST => {
                rest_at = Some(i);
                Cell::Null
            }
            name => match lookup(&hit.source, name) {
                None => Cell::Null,
                Some((at, v, path)) => {
                    held[at].push(path);
                    // Not the same as missing: a `null_value` indexes it.
                    if v == "null" {
                        Cell::Json("null".into())
                    } else {
                        to_cell(v, &c.type_name)
                    }
                }
            },
        })
        .collect();
    // `_source`: every key no other column holds, as it came, and what
    // the columns don't read of the entries they read part of
    // (`metrics.disk` when a column reads `metrics.mem`).
    if let Some(at) = rest_at {
        let mut out: Vec<u8> = Vec::new();
        for ((k, v), paths) in hit.source.iter().zip(&held) {
            let value = if paths.is_empty() {
                let mut b = Vec::new();
                write_compact(&mut b, v);
                b
            } else if paths.iter().any(Vec::is_empty) {
                continue;
            } else {
                let paths: Vec<&[&str]> = paths.iter().map(Vec::as_slice).collect();
                match without(v, &paths) {
                    Some(b) => b,
                    None => continue,
                }
            };
            out.push(if out.is_empty() { b'{' } else { b',' });
            write_key(&mut out, k);
            out.extend(value);
        }
        if !out.is_empty() {
            out.push(b'}');
            // Slices of UTF-8 text between ASCII delimiters.
            row[at] = Cell::Json(String::from_utf8(out).unwrap_or_default());
        }
    }
    row
}

/// A field `_source` can hold: from the mapping's `properties`, without
/// multi-fields (`fields`, only indexed), aliases nor `copy_to` targets
/// (filled in the index, not in `_source`; a document that writes one
/// itself keeps it in `_source` of a full read).
#[derive(Debug, Clone, PartialEq)]
struct Field {
    name: String,
    ty: String,
    /// A top-level property.
    top: bool,
}

fn source_fields(mapping: &J, out: &mut Vec<Field>) {
    fn copy_to<'a>(props: Option<&'a J>, out: &mut Vec<&'a str>) {
        for (_, f) in props.and_then(J::as_obj).into_iter().flatten() {
            match f.get("copy_to") {
                Some(J::Str(t)) => out.push(t),
                Some(t) => out.extend(t.as_arr().into_iter().flatten().filter_map(J::as_str)),
                None => {}
            }
            copy_to(f.get("properties"), out);
        }
    }
    fn walk(props: Option<&J>, prefix: &str, targets: &[&str], out: &mut Vec<Field>) {
        let Some(o) = props.and_then(J::as_obj) else { return };
        for (name, f) in o {
            let ty = f.get("type").and_then(J::as_str).unwrap_or("object");
            if ty == "alias" {
                continue;
            }
            let full = if prefix.is_empty() { name.clone() } else { format!("{prefix}.{name}") };
            if targets.contains(&full.as_str()) {
                continue;
            }
            match out.iter_mut().find(|x| x.name == full) {
                Some(x) => x.top |= prefix.is_empty(),
                None => out.push(Field { name: full.clone(), ty: ty.to_string(), top: prefix.is_empty() }),
            }
            walk(f.get("properties"), &full, targets, out);
        }
    }
    let mut targets = Vec::new();
    copy_to(mapping.get("properties"), &mut targets);
    walk(mapping.get("properties"), "", &targets, out);
}

/// The read's columns: the asked-for ones (each must exist) or, for a full
/// read, `_id`, `_routing`, the top-level fields and `_source` (whatever
/// else the documents hold).
fn read_columns(asked: Option<&[String]>, fields: &[Field]) -> Result<Vec<TransferColumn>> {
    let names: Vec<String> = match asked {
        Some(c) => {
            if let Some(bad) = c.iter().find(|n| !META.contains(&n.as_str()) && *n != REST && !fields.iter().any(|f| f.name == **n)) {
                return Err(Error::Query(format!("la lectura no trae la columna «{bad}»")));
            }
            c.to_vec()
        }
        None => ["_id", "_routing"]
            .into_iter()
            .map(str::to_string)
            .chain(fields.iter().filter(|f| f.top && !META.contains(&f.name.as_str()) && f.name != REST).map(|f| f.name.clone()))
            .chain(std::iter::once(REST.to_string()))
            .collect(),
    };
    Ok(names
        .into_iter()
        .map(|n| {
            let ty = match n.as_str() {
                "_id" | "_index" | "_routing" => "keyword".to_string(),
                "_score" => "float".to_string(),
                REST => "object".to_string(),
                _ => fields.iter().find(|f| f.name == n).map(|f| f.ty.clone()).unwrap_or_default(),
            };
            TransferColumn { nullable: n != "_id", name: n, type_name: ty }
        })
        .collect())
}

/// The `_source` filter for the asked-for columns: each one and its root.
fn source_filter(names: &[String]) -> Value {
    if names.iter().any(|n| n == REST) {
        return Value::Bool(true);
    }
    let mut inc: Vec<&str> = Vec::new();
    for n in names.iter().filter(|n| !META.contains(&n.as_str())) {
        for p in [n.as_str(), n.split('.').next().unwrap_or(n)] {
            if !inc.contains(&p) {
                inc.push(p);
            }
        }
    }
    if inc.is_empty() {
        Value::Bool(false)
    } else {
        json!({ "includes": inc })
    }
}

/// `{"errors": true, …}`: how many items failed and the first reasons.
pub(crate) fn bulk_errors(body: &str, rows: u64) -> Option<String> {
    let resp: Value = serde_json::from_str(body).ok()?;
    if resp.get("errors").and_then(Value::as_bool) != Some(true) {
        return None;
    }
    let errors: Vec<&Value> = resp
        .get("items")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|i| i.as_object()?.values().next()?.get("error"))
        .collect();
    let first: Vec<String> =
        errors.iter().take(3).map(|e| es_error_message(400, &json!({ "error": e }).to_string())).collect();
    Some(format!("_bulk: fallaron {} de {rows} documentos. Primeros errores: {}", errors.len(), first.join("; ")))
}

/// A cell as the text of an `_id` / routing value.
fn key_text(c: &Cell) -> String {
    match c.to_json() {
        Value::String(s) => s,
        v => v.to_string(),
    }
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    // Writing into a Vec can't fail.
    let _ = serde_json::to_writer(&mut *buf, s);
}

/// A JSON cell written as is (its numbers keep their text), on one line.
fn write_json(buf: &mut Vec<u8>, col: &str, s: &str) -> Result<()> {
    check_json(s).map_err(|e| Error::Query(format!("«{col}» no es un JSON válido: {e}")))?;
    // Valid JSON has line breaks only between tokens (inside a string they
    // are escaped): as spaces, NDJSON's lines stay whole.
    buf.extend(s.trim().bytes().map(|b| if b == b'\n' || b == b'\r' { b' ' } else { b }));
    Ok(())
}

/// One row as an action line and a document line.
pub(crate) fn encode_row(buf: &mut Vec<u8>, op: &str, names: &[String], row: &[Cell]) -> Result<()> {
    buf.extend_from_slice(b"{\"");
    buf.extend_from_slice(op.as_bytes());
    buf.extend_from_slice(b"\":{");
    let mut first = true;
    for (n, c) in names.iter().zip(row) {
        let key = match n.as_str() {
            "_id" => "_id",
            "_routing" => "routing",
            _ => continue,
        };
        if matches!(c, Cell::Null) {
            continue;
        }
        if !first {
            buf.push(b',');
        }
        first = false;
        write_str(buf, key);
        buf.push(b':');
        write_str(buf, &key_text(c));
    }
    buf.extend_from_slice(b"}}\n{");
    let mut written: Vec<Cow<str>> = Vec::new();
    let mut rest: Option<(&str, &str)> = None;
    for (n, c) in names.iter().zip(row) {
        if matches!(c, Cell::Null) || META.contains(&n.as_str()) {
            continue;
        }
        if n == REST {
            match c {
                Cell::Json(s) | Cell::Text(s) => rest = Some((n, s)),
                _ => return Err(Error::Query(format!("«{REST}» tiene que ser un objeto JSON"))),
            }
            continue;
        }
        if !written.is_empty() {
            buf.push(b',');
        }
        written.push(Cow::Borrowed(n));
        write_str(buf, n);
        buf.push(b':');
        match c {
            Cell::Bool(b) => buf.extend_from_slice(if *b { b"true" } else { b"false" }),
            Cell::Int(i) => {
                let _ = write!(buf, "{i}");
            }
            Cell::UInt(u) => {
                let _ = write!(buf, "{u}");
            }
            Cell::Float(_) => {
                let _ = serde_json::to_writer(&mut *buf, &c.to_json());
            }
            Cell::Bytes(b) => write_str(buf, &base64::engine::general_purpose::STANDARD.encode(b)),
            // Only a JSON cell is nested JSON: text is text, whatever it holds.
            Cell::Json(s) => write_json(buf, n, s)?,
            Cell::Text(s) | Cell::Decimal(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => {
                write_str(buf, s)
            }
            Cell::Null => {}
        }
    }
    // The `_source` fields no column holds.
    if let Some((n, s)) = rest {
        // Walked as text, never as a tree (see `Scan`).
        let Ok(fields) = entries(s.trim()) else { return Err(Error::Query(format!("«{n}» tiene que ser un objeto JSON"))) };
        for (k, v) in fields {
            let key = unescape(k);
            if written.contains(&key) {
                return Err(Error::Query(format!("«{n}» repite el campo «{key}», que ya tiene su propia columna")));
            }
            if !written.is_empty() {
                buf.push(b',');
            }
            written.push(key);
            write_key(buf, k);
            write_compact(buf, v);
        }
    }
    buf.extend_from_slice(b"}\n");
    Ok(())
}

/// Where the `_bulk` requests go.
#[derive(Clone)]
struct Bulk {
    client: reqwest::Client,
    url: String,
    opaque: String,
}

/// Send one `_bulk` request, retrying while the cluster pushes back
/// (429; not once `stop` is set); fails with the items' first errors.
async fn send_bulk(to: Bulk, body: Vec<u8>, rows: u64, stop: Arc<AtomicBool>) -> Result<u64> {
    // The body is shared by the retries, not copied.
    let rb = to
        .client
        .post(&to.url)
        .header("X-Opaque-Id", &to.opaque)
        .header("Content-Type", "application/x-ndjson")
        .timeout(TIMEOUT)
        .body(body);
    let mut wait = Duration::from_millis(500);
    for attempt in 0.. {
        let Some(this) = rb.try_clone() else { return Err(Error::State("cuerpo de _bulk".into())) };
        let (status, text) = http::send(this).await?;
        if status == 429 && attempt < 6 {
            // A refused request indexed nothing.
            if stop.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            tokio::time::sleep(wait).await;
            if stop.load(Ordering::SeqCst) {
                return Err(Error::Cancelled);
            }
            wait = (wait * 2).min(Duration::from_secs(15));
            continue;
        }
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &text)));
        }
        if let Some(msg) = bulk_errors(&text, rows) {
            return Err(Error::Query(msg));
        }
        break;
    }
    Ok(rows)
}

/// `false` / `"false"` in a mapping.
fn is_false(v: Option<&J>) -> bool {
    matches!(v, Some(J::Bool(false))) || v.and_then(J::as_str) == Some("false")
}

/// The mapping doesn't keep `_source`.
fn unstored_source(mapping: &J) -> bool {
    is_false(mapping.get("_source").and_then(|s| s.get("enabled")))
}

fn uneven() -> Error {
    Error::Unsupported(format!(
        "una página de la lectura superó {} MiB: los documentos tienen tamaños muy desparejos y este servidor solo se lee con scroll, que no deja achicar la página a mitad de camino",
        MAX_RESPONSE >> 20
    ))
}

fn too_big() -> Error {
    Error::Unsupported(format!(
        "un solo documento ocupa más de {} MiB en la respuesta: no se puede leer con la memoria acotada de la transferencia",
        MAX_RESPONSE >> 20
    ))
}

impl EsSession {
    /// The fields `_source` can hold, from the mapping of every index
    /// `index` names.
    async fn field_types(&self, index: &str) -> Result<Vec<Field>> {
        let m = self.get_json(&format!("/{}/_mapping", crate::ddl::path_segment(index))).await?;
        let mut fields: Vec<Field> = Vec::new();
        for (name, idx) in m.as_obj().into_iter().flatten() {
            let mapping = idx.get("mappings").unwrap_or(&J::Null);
            if unstored_source(mapping) {
                return Err(Error::Unsupported(format!(
                    "el índice «{name}» no guarda _source (`_source.enabled: false`): no hay de dónde leer sus documentos completos"
                )));
            }
            source_fields(mapping, &mut fields);
        }
        Ok(fields)
    }

    /// One page: `None` if its response goes over [`MAX_RESPONSE`] (it's
    /// not read further), else its text (walked with [`walk_page`]).
    async fn page(&self, path: &str, body: &Value) -> Result<Option<String>> {
        let rb = self.request("POST", path).header("Content-Type", "application/json").timeout(TIMEOUT).body(body.to_string());
        let mut resp = rb.send().await.map_err(http::net_err)?;
        let status = resp.status().as_u16();
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = resp.chunk().await.map_err(http::net_err)? {
            if status >= 400 {
                buf.extend_from_slice(&chunk[..chunk.len().min(MAX_ERROR_BODY.saturating_sub(buf.len()))]);
                continue;
            }
            if buf.len() + chunk.len() > MAX_RESPONSE {
                return Ok(None);
            }
            buf.extend_from_slice(&chunk);
        }
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &String::from_utf8_lossy(&buf))));
        }
        String::from_utf8(buf).map(Some).map_err(|e| Error::Query(format!("Respuesta inesperada de _search: {e}")))
    }

    /// Hits per scroll page: after the largest of a probe's documents.
    /// `None`: the read finds nothing.
    async fn scroll_size(&self, index: &str, query: &Value, source: &Value) -> Result<Option<usize>> {
        let path = format!("/{}/_search", crate::ddl::path_segment(index));
        let mut n = PROBE;
        loop {
            let body = json!({ "size": n, "query": query, "_source": source, "sort": ["_doc"], "track_total_hits": false });
            match self.page(&path, &body).await? {
                Some(text) => {
                    let end = walk_page(&text, |_| Ok(()))?;
                    if end.hits == 0 {
                        return Ok(None);
                    }
                    return Ok(Some((PAGE_BYTES / end.largest.max(1)).clamp(1, PAGE)));
                }
                None if n > 1 => n = (n / 10).max(1),
                None => return Err(too_big()),
            }
        }
    }

    /// A point in time on `index`, if the cluster has them.
    async fn open_pit(&self, index: &str) -> Option<String> {
        if self.opensearch {
            return None;
        }
        let path = format!("/{}/_pit?keep_alive={KEEP_ALIVE}", crate::ddl::path_segment(index));
        let (status, text) = http::send(self.request("POST", &path).timeout(TIMEOUT)).await.ok()?;
        if status >= 400 {
            return None;
        }
        let v: Value = serde_json::from_str(&text).ok()?;
        v.get("id").and_then(Value::as_str).map(str::to_string)
    }

    /// Free the point in time or scroll; failing to is harmless (it expires).
    async fn close_cursor(&self, cursor: &Cursor) {
        let (path, body) = match cursor {
            Cursor::Pit { id, .. } => ("/_pit", json!({ "id": id })),
            Cursor::Scroll { id: Some(id) } => ("/_search/scroll", json!({ "scroll_id": [id] })),
            Cursor::Scroll { id: None } => return,
        };
        let rb = self.request("DELETE", path).header("Content-Type", "application/json").body(body.to_string());
        let _ = http::send(rb.timeout(Duration::from_secs(20))).await;
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let index = spec.table.name.trim();
        if index.is_empty() {
            return Err(Error::Query("Falta el índice de origen.".into()));
        }
        let fields = self.field_types(index).await?;
        let cols = read_columns(spec.columns.as_deref(), &fields)?;
        let names: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
        let query: Value = match spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            Some(f) => serde_json::from_str(f)
                .map_err(|e| Error::Query(format!("El filtro tiene que ser una consulta JSON del Query DSL: {e}")))?,
            None => json!({ "match_all": {} }),
        };
        let source = if spec.columns.is_some() { source_filter(&names) } else { Value::Bool(true) };

        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut cursor = match self.open_pit(index).await {
            Some(id) => Cursor::Pit { id, after: None },
            None => Cursor::Scroll { id: None },
        };
        let mut builder = BatchBuilder::new();
        let r = self.read_pages(index, &query, &source, &cols, &mut cursor, &mut builder, &sink).await;
        self.close_cursor(&cursor).await;
        r?;
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    #[allow(clippy::too_many_arguments)]
    async fn read_pages(
        &self,
        index: &str,
        query: &Value,
        source: &Value,
        cols: &[TransferColumn],
        cursor: &mut Cursor,
        builder: &mut BatchBuilder,
        sink: &BatchSinkRef,
    ) -> Result<()> {
        let mut first = true;
        // Hits asked per page: with a point in time, after the documents'
        // size; a scroll's is set when it opens.
        let mut size = FIRST_PAGE;
        loop {
            let asked = size;
            let text = match cursor {
                Cursor::Pit { id, after } => {
                    let mut body = json!({
                        "size": size,
                        "query": query,
                        "_source": source,
                        "track_total_hits": false,
                        "pit": { "id": id, "keep_alive": KEEP_ALIVE },
                        "sort": [{ "_shard_doc": "asc" }],
                    });
                    if let Some(a) = after {
                        body["search_after"] = serde_json::from_str(a).map_err(|e| Error::Query(format!("search_after: {e}")))?;
                    }
                    match self.page("/_search", &body).await {
                        Ok(Some(text)) => text,
                        // Too big: the same page again, with fewer hits.
                        Ok(None) if size > 1 => {
                            size = (size / 4).max(1);
                            continue;
                        }
                        Ok(None) => return Err(too_big()),
                        // A point in time without `_shard_doc` (before 7.12):
                        // start over with a scroll.
                        Err(e) if first => {
                            tracing::debug!(error = %e, "point in time search failed, using a scroll");
                            let old = std::mem::replace(cursor, Cursor::Scroll { id: None });
                            self.close_cursor(&old).await;
                            continue;
                        }
                        Err(e) => return Err(e),
                    }
                }
                Cursor::Scroll { id: None } => {
                    let Some(n) = self.scroll_size(index, query, source).await? else { return Ok(()) };
                    let body = json!({ "size": n, "query": query, "_source": source, "sort": ["_doc"] });
                    let path = format!("/{}/_search?scroll={KEEP_ALIVE}", crate::ddl::path_segment(index));
                    self.page(&path, &body).await?.ok_or_else(uneven)?
                }
                Cursor::Scroll { id: Some(id) } => {
                    self.page("/_search/scroll", &json!({ "scroll": KEEP_ALIVE, "scroll_id": id })).await?.ok_or_else(uneven)?
                }
            };
            first = false;
            // Each hit becomes a row as it's read: the page in flight is its
            // text and one row (plus the batch being filled).
            let page = {
                let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
                walk_page(&text, |hit| Ok(builder.push(hit_row(hit, cols), &mut *s)?))?
            };
            let n = page.hits;
            if let Some(per_hit) = text.len().checked_div(n) {
                size = (PAGE_BYTES / per_hit.max(1)).clamp(1, PAGE);
            }
            drop(text);
            match cursor {
                Cursor::Pit { id, after } => {
                    if let Some(p) = page.pit_id {
                        *id = p;
                    }
                    *after = page.last_sort;
                }
                Cursor::Scroll { id } => {
                    if page.scroll_id.is_some() {
                        *id = page.scroll_id;
                    }
                }
            }
            let done = match cursor {
                Cursor::Pit { after, .. } => n < asked || after.is_none(),
                Cursor::Scroll { .. } => n == 0,
            };
            if done {
                return Ok(());
            }
        }
    }

    pub(crate) async fn transfer_load(
        &mut self,
        spec: &LoadSpec,
        columns: &[TransferColumn],
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
    ) -> Result<u64> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden cargar datos.".into()));
        }
        let index = spec.table.name.trim();
        if index.is_empty() {
            return Err(Error::Query("Falta el índice de destino.".into()));
        }
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let op = if spec.table.kind == kinds::STREAM { "create" } else { "index" };
        let seg = crate::ddl::path_segment(index);
        let to = Bulk {
            client: self.client.clone(),
            url: format!("{}/{seg}/_bulk?filter_path=errors,items.*.error", self.base),
            opaque: self.opaque_id.clone(),
        };

        // A request is a commit window: `commit_rows` rows, `commit_bytes`
        // bytes, and never over REQUEST_BYTES (memory: IN_FLIGHT of them).
        let every = spec.commit_rows.max(1);
        let limit = usize::try_from(spec.commit_bytes).unwrap_or(usize::MAX).clamp(1, REQUEST_BYTES);
        let mut done = 0u64;
        let mut inflight = InFlight::new();
        let fed: Result<()> = async {
            let mut buf: Vec<u8> = Vec::new();
            let mut row_buf: Vec<u8> = Vec::new();
            let mut buffered = 0u64;
            while let Some(batch) = source.next().await {
                for row in batch.rows {
                    if row.len() != names.len() {
                        return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                    }
                    row_buf.clear();
                    encode_row(&mut row_buf, op, &names, &row)?;
                    // The row would take the request over its bytes: send
                    // what's there first.
                    if buffered > 0 && buf.len() + row_buf.len() > limit {
                        inflight.send(&to, take_body(&mut buf, limit), std::mem::take(&mut buffered), &mut done, progress).await?;
                    }
                    buf.extend_from_slice(&row_buf);
                    buffered += 1;
                    if buffered >= every || buf.len() >= limit {
                        inflight.send(&to, take_body(&mut buf, limit), std::mem::take(&mut buffered), &mut done, progress).await?;
                    }
                }
                inflight.reap(&mut done, progress)?;
            }
            if buffered > 0 {
                inflight.send(&to, take_body(&mut buf, limit), buffered, &mut done, progress).await?;
            }
            while let Some(r) = inflight.set.join_next().await {
                done += joined(Some(r))?;
                progress(done);
            }
            Ok(())
        }
        .await;
        if let Err(e) = fed {
            // Nothing may commit after the error is returned, and what
            // commits meanwhile is reported.
            inflight.drain(&mut done, progress).await;
            return Err(e);
        }
        let path = format!("/{seg}/_refresh");
        let (status, text) = http::send(self.request("POST", &path).timeout(TIMEOUT)).await?;
        if status >= 400 {
            return Err(Error::Query(es_error_message(status, &text)));
        }
        Ok(done)
    }
}

fn joined(r: Option<std::result::Result<Result<u64>, tokio::task::JoinError>>) -> Result<u64> {
    match r {
        None => Ok(0),
        Some(Ok(r)) => r,
        Some(Err(e)) => Err(Error::State(format!("envío de _bulk interrumpido: {e}"))),
    }
}

/// The request body built so far, without spare room (it's held until the
/// request ends); `buf` starts over empty.
fn take_body(buf: &mut Vec<u8>, limit: usize) -> Vec<u8> {
    let mut body = std::mem::replace(buf, Vec::with_capacity(limit.min(64 * 1024)));
    if body.capacity() - body.len() > body.len() / 4 {
        body.shrink_to_fit();
    }
    body
}

/// Requests still running, counted apart from the `JoinSet` so a drop can
/// wait for them without an async context.
type Live = Arc<(Mutex<usize>, Condvar)>;

/// Counts a request down when its task ends, however it ends.
struct LiveGuard(Live);

impl Drop for LiveGuard {
    fn drop(&mut self) {
        let (n, cv) = &*self.0;
        if let Ok(mut n) = n.lock() {
            *n -= 1;
        }
        cv.notify_all();
    }
}

/// The `_bulk` requests in flight. The server indexes a request it got
/// whole even if the client stops waiting, so neither a failed load nor a
/// dropped (cancelled) one lets go of them before they end: a failure
/// awaits them ([`InFlight::drain`]), a drop blocks until they end (on a
/// multi-threaded runtime; a single-threaded one couldn't run them
/// meanwhile, so there they are left to finish on their own). `stop` keeps
/// them from retrying a refused (429) request.
struct InFlight {
    set: JoinSet<Result<u64>>,
    stop: Arc<AtomicBool>,
    live: Live,
}

impl InFlight {
    fn new() -> Self {
        InFlight { set: JoinSet::new(), stop: Arc::new(AtomicBool::new(false)), live: Arc::new((Mutex::new(0), Condvar::new())) }
    }

    /// Send `body` once there's room, reporting what ends meanwhile.
    async fn send(&mut self, to: &Bulk, body: Vec<u8>, rows: u64, done: &mut u64, progress: Progress<'_>) -> Result<()> {
        if self.set.len() >= IN_FLIGHT {
            *done += joined(self.set.join_next().await)?;
            progress(*done);
        }
        if let Ok(mut n) = self.live.0.lock() {
            *n += 1;
        }
        let guard = LiveGuard(self.live.clone());
        let fut = send_bulk(to.clone(), body, rows, self.stop.clone());
        self.set.spawn(async move {
            let _guard = guard;
            fut.await
        });
        Ok(())
    }

    /// Report the requests that already ended.
    fn reap(&mut self, done: &mut u64, progress: Progress<'_>) -> Result<()> {
        while let Some(r) = self.set.try_join_next() {
            *done += joined(Some(r))?;
            progress(*done);
        }
        Ok(())
    }

    /// Wait for every request (after a failure), reporting the rows of
    /// those that commit meanwhile: `progress` ends at what's committed.
    async fn drain(&mut self, done: &mut u64, progress: Progress<'_>) {
        self.stop.store(true, Ordering::SeqCst);
        while let Some(r) = self.set.join_next().await {
            if let Ok(n) = joined(Some(r)) {
                *done += n;
                progress(*done);
            }
        }
    }
}

impl Drop for InFlight {
    fn drop(&mut self) {
        if self.set.is_empty() {
            return;
        }
        self.stop.store(true, Ordering::SeqCst);
        // Dropping the set would abort the tasks, not the requests.
        self.set.detach_all();
        let live = self.live.clone();
        let wait = move || {
            let (n, cv) = &*live;
            if let Ok(g) = n.lock() {
                let _ = cv.wait_timeout_while(g, TIMEOUT + Duration::from_secs(30), |n| *n > 0);
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => tokio::task::block_in_place(wait),
            Ok(_) => {}
            Err(_) => wait(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    fn encode(names_: &[&str], row: &[Cell]) -> Result<String> {
        let mut buf = Vec::new();
        encode_row(&mut buf, "index", &names(names_), row)?;
        Ok(String::from_utf8(buf).unwrap())
    }

    fn doc(names_: &[&str], row: &[Cell]) -> String {
        encode(names_, row).unwrap().lines().nth(1).unwrap().to_string()
    }

    fn hit(json: &str) -> Hit<'_> {
        parse_hit(&mut Scan::new(json), Vec::new(), 0).unwrap()
    }

    /// Fields as `source_fields` gives them (dotted names are nested).
    fn fields(f: &[(&str, &str)]) -> Vec<Field> {
        f.iter().map(|(n, t)| Field { name: n.to_string(), ty: t.to_string(), top: !n.contains('.') }).collect()
    }

    fn col(n: &str, t: &str) -> TransferColumn {
        TransferColumn { name: n.into(), type_name: t.into(), nullable: true }
    }

    #[test]
    fn rows_become_bulk_lines() {
        let cols = ["_id", "_score", "title", "meta", "tags", "n", "blob", "gone", "price"];
        let row = vec![
            Cell::Int(7),
            Cell::Float(1.0),
            Cell::Text("Dune \"1\"".into()),
            Cell::Json("{\"x\": 1}".into()),
            Cell::Json("[1,2]".into()),
            Cell::UInt(u64::MAX),
            Cell::Bytes(vec![1, 2, 3]),
            Cell::Null,
            Cell::Decimal("12.50".into()),
        ];
        let text = encode(&cols, &row).unwrap();
        let lines: Vec<&str> = text.lines().collect();
        assert_eq!(lines[0], r#"{"index":{"_id":"7"}}"#);
        let doc: Value = serde_json::from_str(lines[1]).unwrap();
        assert_eq!(doc, json!({"title": "Dune \"1\"", "meta": {"x": 1}, "tags": [1, 2], "n": u64::MAX, "blob": "AQID", "price": "12.50"}));
        let mut buf = Vec::new();
        encode_row(&mut buf, "create", &names(&["a", "_routing"]), &[Cell::Text("{not json}".into()), Cell::Text("r1".into())]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"create\":{\"routing\":\"r1\"}}\n{\"a\":\"{not json}\"}\n");
    }

    /// Problem 2: text that holds valid JSON is still a string.
    #[test]
    fn text_is_never_nested() {
        let row = [Cell::Text("[1,2]".into()), Cell::Text("  [3]  ".into()), Cell::Text("{}".into())];
        assert_eq!(doc(&["k", "ws", "t"], &row), r#"{"k":"[1,2]","ws":"  [3]  ","t":"{}"}"#);
        // A JSON cell is nested as written (numbers keep their text), on one line.
        let row = [Cell::Json("{\n \"a\": 12345678901234567890123,\r\n \"b\": \"x\\ny\" }".into())];
        assert_eq!(doc(&["j"], &row), "{\"j\":{  \"a\": 12345678901234567890123,   \"b\": \"x\\ny\" }}");
        // …and an invalid one fails instead of being written raw.
        let err = encode(&["j"], &[Cell::Json("{\"a\":1}\n{\"index\":{}}".into())]).unwrap_err().to_string();
        assert!(err.contains("«j» no es un JSON válido"), "{err}");
        assert!(encode(&["j"], &[Cell::Json("[1,".into())]).is_err());
    }

    /// Problem 3: numbers an `f64` can't hold keep their text.
    #[test]
    fn numbers_are_lossless() {
        assert_eq!(num_cell("12345678901234567890123"), Cell::Json("12345678901234567890123".into()));
        assert_eq!(num_cell("3.14159265358979323846"), Cell::Json("3.14159265358979323846".into()));
        assert_eq!(num_cell("1.50"), Cell::Json("1.50".into()));
        assert_eq!(num_cell("1e400"), Cell::Json("1e400".into()));
        assert_eq!(num_cell("-0"), Cell::Json("-0".into()));
        assert_eq!(num_cell("1.5"), Cell::Float(1.5));
        assert_eq!(num_cell("-42"), Cell::Int(-42));
        assert_eq!(num_cell("18446744073709551615"), Cell::UInt(u64::MAX));
        // Read and written back: the same text.
        let h = hit(r#"{"_id":"1","_source":{"code":12345678901234567890123,"f":0.1,"big":[1e400]}}"#);
        let cols = [col("_id", "keyword"), col("code", "keyword"), col("f", "double"), col("big", "double")];
        let row = hit_row(&h, &cols);
        assert_eq!(doc(&["_id", "code", "f", "big"], &row), r#"{"code":12345678901234567890123,"f":0.1,"big":[1e400]}"#);
    }

    /// Problem 4: an asked-for column that doesn't exist is an error.
    #[test]
    fn unknown_columns_fail() {
        let fields = fields(&[("k", "keyword"), ("a.b", "long")]);
        let err = read_columns(Some(&names(&["_id", "no_such_column"])), &fields).unwrap_err().to_string();
        assert!(err.contains("la lectura no trae la columna «no_such_column»"), "{err}");
        let cols = read_columns(Some(&names(&["a.b", "_id", "k", "_source"])), &fields).unwrap();
        let got: Vec<(&str, &str)> = cols.iter().map(|c| (c.name.as_str(), c.type_name.as_str())).collect();
        assert_eq!(got, [("a.b", "long"), ("_id", "keyword"), ("k", "keyword"), ("_source", "object")]);
    }

    /// Round 2, problem 4: multi-fields (`title.keyword`) and aliases are
    /// never in `_source`: asking for one fails instead of reading NULLs.
    #[test]
    fn multi_fields_are_not_columns() {
        let m = J::parse(
            r#"{"properties":{"title":{"type":"text","fields":{"keyword":{"type":"keyword"}}},
                "t2":{"type":"alias","path":"title"},
                "host":{"properties":{"name":{"type":"keyword","fields":{"raw":{"type":"keyword"}}}}}}}"#,
        )
        .unwrap();
        let mut f = Vec::new();
        source_fields(&m, &mut f);
        let got: Vec<(&str, &str, bool)> = f.iter().map(|f| (f.name.as_str(), f.ty.as_str(), f.top)).collect();
        assert_eq!(got, [("title", "text", true), ("host", "object", true), ("host.name", "keyword", false)]);
        for bad in ["title.keyword", "host.name.raw", "t2"] {
            let err = read_columns(Some(&names(&["_id", bad])), &f).unwrap_err().to_string();
            assert!(err.contains(&format!("la lectura no trae la columna «{bad}»")), "{err}");
        }
        assert!(read_columns(Some(&names(&["host.name"])), &f).is_ok());
    }

    /// Round 2, problem 1: `_source` keys with dots (`"host.name"`, which
    /// the mapping shows as `host: {name}`) are neither lost nor reshaped.
    #[test]
    fn dotted_source_keys_survive_a_full_read() {
        let f = fields(&[("host", "object"), ("host.name", "keyword"), ("k", "keyword")]);
        let cols = read_columns(None, &f).unwrap();
        let names_: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names_, ["_id", "_routing", "host", "k", "_source"]);
        let h = hit(r#"{"_id":"1","_source":{"host.name":"web-1","k":"x","a.b":{"c":[1, 2]}}}"#);
        let row = hit_row(&h, &cols);
        assert_eq!(row[2], Cell::Null);
        assert_eq!(row[4], Cell::Json(r#"{"host.name":"web-1","a.b":{"c":[1,2]}}"#.into()));
        // Written back with the same keys.
        assert_eq!(doc(&names_, &row), r#"{"k":"x","host.name":"web-1","a.b":{"c":[1,2]}}"#);
        // Both shapes at once: each key where it belongs.
        let h = hit(r#"{"_id":"1","_source":{"host":{"ip":"1"},"host.name":"web-1"}}"#);
        let row = hit_row(&h, &cols);
        assert_eq!(row[2], Cell::Json(r#"{"ip":"1"}"#.into()));
        assert_eq!(row[4], Cell::Json(r#"{"host.name":"web-1"}"#.into()));
        // An asked-for dotted path finds a dotted key, and holds only it.
        let h = hit(r#"{"_id":"1","_source":{"a.b":{"c":7},"a":{"x":1},"host.name":"w"}}"#);
        let asked = [col("a.b.c", "long"), col("host.name", "keyword"), col("_source", "object")];
        assert_eq!(hit_row(&h, &asked), [Cell::Int(7), Cell::Text("w".into()), Cell::Json(r#"{"a":{"x":1}}"#.into())]);
        // A longer key isn't held by a shorter column: `host` doesn't read `host.name`.
        assert_eq!(lookup(&h.source, "host"), None);
        assert_eq!(lookup(&h.source, "a.x"), Some((1, "1", vec!["x"])));
    }

    /// Problem 5: a full read brings `_routing` and `_source` with the
    /// fields no column holds; both are written back.
    #[test]
    fn full_reads_keep_routing_and_unmapped_fields() {
        let fields = fields(&[("k", "keyword"), ("o", "object"), ("o.x", "long")]);
        let cols = read_columns(None, &fields).unwrap();
        let names_: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names_, ["_id", "_routing", "k", "o", "_source"]);
        let h = hit(r#"{"_id":"1","_routing":"r7","_source":{"k":"a","extra":"unmapped value","o":{"x":1,"y":2},"more":{"z":[1]}}}"#);
        let row = hit_row(&h, &cols);
        assert_eq!(row[1], Cell::Text("r7".into()));
        assert_eq!(row[4], Cell::Json(r#"{"extra":"unmapped value","more":{"z":[1]}}"#.into()));
        let text = encode(&names_, &row).unwrap();
        assert_eq!(
            text,
            "{\"index\":{\"_id\":\"1\",\"routing\":\"r7\"}}\n{\"k\":\"a\",\"o\":{\"x\":1,\"y\":2},\"extra\":\"unmapped value\",\"more\":{\"z\":[1]}}\n"
        );
        // Nothing unmapped: no `_source` value.
        assert_eq!(hit_row(&hit(r#"{"_id":"2","_source":{"k":"b"}}"#), &cols)[4], Cell::Null);
        // A dotted column covers its root.
        let h = hit(r#"{"_id":"1","_source":{"o":{"x":1},"extra":1}}"#);
        assert_eq!(hit_row(&h, &[col("o.x", "long"), col("_source", "object")])[1], Cell::Json(r#"{"extra":1}"#.into()));
        // `_source` repeating a column, or not an object, fails.
        assert!(encode(&["k", "_source"], &[Cell::Text("a".into()), Cell::Json(r#"{"k":"b"}"#.into())]).is_err());
        assert!(encode(&["_source"], &[Cell::Json("[1]".into())]).is_err());
        assert!(encode(&["_source"], &[Cell::Int(1)]).is_err());
        // The mapping flag.
        assert!(unstored_source(&J::parse(r#"{"_source":{"enabled":false}}"#).unwrap()));
        assert!(!unstored_source(&J::parse(r#"{"_source":{"excludes":["x"]}}"#).unwrap()));
    }

    /// Problem 6: an explicit null is not a missing field.
    #[test]
    fn explicit_nulls_survive() {
        let h = hit(r#"{"_id":"1","_source":{"nv":null,"arr":[null]}}"#);
        let cols = [col("_id", "keyword"), col("nv", "keyword"), col("gone", "keyword"), col("arr", "keyword")];
        let row = hit_row(&h, &cols);
        assert_eq!(row[1], Cell::Json("null".into()));
        assert_eq!(row[2], Cell::Null);
        assert_eq!(doc(&["_id", "nv", "gone", "arr"], &row), r#"{"nv":null,"arr":[null]}"#);
    }

    #[test]
    fn hits_become_rows() {
        let h = hit(
            r#"{"_index":"i","_id":"1","_score":null,"sort":[3],
                "_source":{"title":"a","author":{"name":"H"},"tags":["x"],"n":5,"f":1.5,"b":"AQID","nb":"AQI=\n","ok":true,"u":"é\"😀"}}"#,
        );
        assert_eq!(h.sort, Some("[3]"));
        let cols = [
            col("_id", "keyword"),
            col("title", "text"),
            col("author", "object"),
            col("author.name", "keyword"),
            col("tags", "keyword"),
            col("n", "long"),
            col("f", "double"),
            col("b", "binary"),
            col("nb", "binary"),
            col("ok", "boolean"),
            col("missing", "long"),
            col("u", "keyword"),
            col("_score", "float"),
        ];
        assert_eq!(
            hit_row(&h, &cols),
            vec![
                Cell::Text("1".into()),
                Cell::Text("a".into()),
                Cell::Json("{\"name\":\"H\"}".into()),
                Cell::Text("H".into()),
                Cell::Json("[\"x\"]".into()),
                Cell::Int(5),
                Cell::Float(1.5),
                Cell::Bytes(vec![1, 2, 3]),
                // Not canonical Base64: kept as the text it is.
                Cell::Text("AQI=\n".into()),
                Cell::Bool(true),
                Cell::Null,
                Cell::Text("é\"😀".into()),
                Cell::Null,
            ]
        );
    }

    #[test]
    fn round_trip_through_bulk() {
        let cols = ["_id", "title", "meta", "blob"];
        let row = vec![Cell::Text("a1".into()), Cell::Text("x".into()), Cell::Json("{\"k\":[1,2],\"a\":{}}".into()), Cell::Bytes(vec![0, 255])];
        let text = encode(&cols, &row).unwrap();
        let src = text.lines().nth(1).unwrap();
        let json = format!(r#"{{"_id":"a1","_source":{src}}}"#);
        let h = hit(&json);
        let tcols = [col("_id", "keyword"), col("title", "text"), col("meta", "object"), col("blob", "binary")];
        assert_eq!(hit_row(&h, &tcols), row);
    }

    #[test]
    fn parses_json_losslessly() {
        let text = r#" {"b":[1,-2.5e-3,true,false,null],"a":"x\\y","":{}} "#;
        check_json(text).unwrap();
        assert_eq!(compact(text.trim()), r#"{"b":[1,-2.5e-3,true,false,null],"a":"x\\y","":{}}"#);
        assert_eq!(compact("{ \"a b\" : [ 1 , \" x \\\" \" ] }"), "{\"a b\":[1,\" x \\\" \"]}");
        for bad in ["", "{", "[1,]", "{\"a\"}", "01", "1.", "-", "tru", "\"a", "{\"a\":1}x", "\"\u{1}\"", "[1 2]", "{,}", "[,1]", "\"\\q\"", "{\"a\":1,}"] {
            assert!(check_json(bad).is_err(), "{bad:?}");
        }
        let deep = "[".repeat(MAX_DEPTH + 2) + &"]".repeat(MAX_DEPTH + 2);
        assert!(check_json(&deep).is_err());
        let text = r#"{"_scroll_id":"s1","pit_id":"p1","hits":{"total":{"value":1},"hits":[{"_id":"1","_source":{"a":1},"sort":[5, 7]}]}}"#;
        let mut ids = Vec::new();
        let page = walk_page(text, |h| {
            ids.push(h.id.clone());
            Ok(())
        })
        .unwrap();
        assert_eq!((page.scroll_id.as_deref(), page.pit_id.as_deref(), page.hits), (Some("s1"), Some("p1"), 1));
        assert_eq!(page.last_sort.as_deref(), Some("[5, 7]"));
        assert_eq!(ids, [Some("1".to_string())]);
        assert!(walk_page(r#"{"hits":{"hits":[{"_id":"1"}"#, |_| Ok(())).is_err());
        // Escaped keys and values are unescaped, and a key's raw text is kept.
        let h = hit(r#"{"_id":"a\"b","_source":{"k\u00e9":"\u00e9"}}"#);
        assert_eq!(h.id.as_deref(), Some("a\"b"));
        assert_eq!(hit_row(&h, &[col("ké", "keyword")]), [Cell::Text("é".into())]);
    }

    // ---- heap measured per thread (round 2, problem 2)

    struct Counting;

    thread_local! {
        /// (live, peak) bytes of this thread.
        static HEAP: std::cell::Cell<(isize, isize)> = const { std::cell::Cell::new((0, 0)) };
    }

    fn heap_add(d: isize) {
        let _ = HEAP.try_with(|h| {
            let (live, peak) = h.get();
            h.set((live + d, peak.max(live + d)));
        });
    }

    unsafe impl std::alloc::GlobalAlloc for Counting {
        unsafe fn alloc(&self, l: std::alloc::Layout) -> *mut u8 {
            heap_add(l.size() as isize);
            unsafe { std::alloc::System.alloc(l) }
        }
        unsafe fn dealloc(&self, p: *mut u8, l: std::alloc::Layout) {
            heap_add(-(l.size() as isize));
            unsafe { std::alloc::System.dealloc(p, l) }
        }
        unsafe fn realloc(&self, p: *mut u8, l: std::alloc::Layout, new: usize) -> *mut u8 {
            heap_add(new as isize - l.size() as isize);
            unsafe { std::alloc::System.realloc(p, l, new) }
        }
    }

    #[global_allocator]
    static COUNTING: Counting = Counting;

    /// Peak heap growth on this thread while `f` runs.
    fn peak_of<T>(f: impl FnOnce() -> T) -> (T, usize) {
        let base = HEAP.with(|h| {
            let (live, _) = h.get();
            h.set((live, live));
            live
        });
        let r = f();
        let peak = HEAP.with(|h| h.get().1);
        (r, (peak - base).max(0) as usize)
    }

    /// Round 2, problem 2: a page in flight costs about its text, not a
    /// parsed tree of it (a dense vector's tree was ~25 times its text).
    #[test]
    fn pages_cost_about_their_text() {
        let vector = format!("[{}]", (0..1_000_000).map(|i| if i % 2 == 0 { "0" } else { "1" }).collect::<Vec<_>>().join(","));
        let page = format!(
            r#"{{"pit_id":"p","hits":{{"hits":[{{"_id":"1","_source":{{"v":{vector},"k":"x"}},"sort":[1]}},{{"_id":"2","_source":{{"n":{vector}}},"sort":[2]}}]}}}}"#
        );
        let cols = [col("_id", "keyword"), col("v", "dense_vector"), col("k", "keyword"), col("_source", "object")];
        let ((cells, end), peak) = peak_of(|| {
            let mut cells = Vec::new();
            let end = walk_page(&page, |h| {
                let row = hit_row(h, &cols);
                cells.push(row.iter().map(Cell::size).sum::<usize>());
                Ok(())
            })
            .unwrap();
            (cells, end)
        });
        assert_eq!(end.hits, 2);
        assert!(cells.iter().all(|c| *c >= vector.len()), "{cells:?}");
        // One row at a time, each about the vector's text.
        assert!(peak < 3 * vector.len(), "peak {peak} B for a {} B vector", vector.len());
        // The load side checks and merges `_source` without a tree either.
        let row = [Cell::Text("1".into()), Cell::Json(format!(r#"{{"n":{vector}}}"#))];
        // The request buffer (grown by doubling) is all it holds.
        let (_, peak) = peak_of(|| encode(&["_id", "_source"], &row).unwrap());
        assert!(peak < 3 * vector.len(), "peak {peak} B");
    }

    /// Round 2, problem 3: after a failure, the requests that still commit
    /// are counted: the last progress is what's committed.
    #[tokio::test]
    async fn drain_reports_what_commits() {
        let mut f = InFlight::new();
        f.set.spawn(async { Ok(300) });
        f.set.spawn(async { Err(Error::Query("x".into())) });
        f.set.spawn(async { Ok(200) });
        let seen = Mutex::new(Vec::new());
        let mut done = 1000;
        f.drain(&mut done, &|n| seen.lock().unwrap().push(n)).await;
        assert_eq!(done, 1500);
        assert_eq!(seen.into_inner().unwrap().last(), Some(&1500));
    }

    /// Problem 8: the body handed to a request carries no spare room.
    #[test]
    fn bodies_are_tight() {
        let mut buf = Vec::with_capacity(1024);
        buf.extend_from_slice(b"abc");
        let body = take_body(&mut buf, 4096);
        assert_eq!(body, b"abc");
        assert!(body.capacity() <= 4);
        assert!(buf.is_empty());
    }

    #[test]
    fn source_filter_and_errors() {
        assert_eq!(source_filter(&names(&["_id", "author.name", "title"])), json!({"includes": ["author.name", "author", "title"]}));
        assert_eq!(source_filter(&names(&["_id"])), json!(false));
        assert_eq!(source_filter(&names(&["_id", "k", "_source"])), json!(true));
        assert_eq!(bulk_errors(r#"{"errors":false}"#, 3), None);
        let body = r#"{"errors":true,"items":[{"index":{"status":400,"error":{"type":"mapper_parsing_exception","reason":"bad year"}}}]}"#;
        assert_eq!(bulk_errors(body, 10).unwrap(), "_bulk: fallaron 1 de 10 documentos. Primeros errores: mapper_parsing_exception: bad year");
    }

    /// Round 21: a column that reads part of an entry (`metrics.mem`, a
    /// `subobjects: false` field, of `{"metrics": {"mem", "disk"}}`) leaves
    /// the rest of it in `_source`.
    #[test]
    fn a_partly_read_entry_keeps_the_rest() {
        let f = fields(&[("k", "keyword")]).into_iter().chain([Field { name: "metrics.mem".into(), ty: "long".into(), top: true }]).collect::<Vec<_>>();
        let cols = read_columns(None, &f).unwrap();
        let names_: Vec<&str> = cols.iter().map(|c| c.name.as_str()).collect();
        assert_eq!(names_, ["_id", "_routing", "k", "metrics.mem", "_source"]);
        let h = hit(r#"{"_id":"1","_source":{"k":"x","metrics":{"mem":2,"disk":3}}}"#);
        let row = hit_row(&h, &cols);
        assert_eq!(row[3], Cell::Int(2));
        assert_eq!(row[4], Cell::Json(r#"{"metrics":{"disk":3}}"#.into()));
        assert_eq!(doc(&names_, &row), r#"{"k":"x","metrics.mem":2,"metrics":{"disk":3}}"#);
        // Deeper, several columns into one entry, dotted keys, and empty objects kept.
        let h = hit(r#"{"_id":"1","_source":{"a":{"b.c":{"d":1,"e":2},"f":{},"g":{"h":3}},"z":1}}"#);
        let cols = [col("a.b.c.d", "long"), col("a.g.h", "long"), col("_source", "object")];
        assert_eq!(hit_row(&h, &cols), [Cell::Int(1), Cell::Int(3), Cell::Json(r#"{"a":{"b.c":{"e":2},"f":{}},"z":1}"#.into())]);
        // Every part read: nothing left of the entry.
        let cols = [col("a.b.c.d", "long"), col("a.b.c.e", "long"), col("a.f", "object"), col("a.g", "object"), col("_source", "object")];
        assert_eq!(hit_row(&h, &cols)[4], Cell::Json(r#"{"z":1}"#.into()));
    }

    /// Round 21: a `copy_to` target isn't in `_source`: not a column.
    #[test]
    fn copy_to_targets_are_not_columns() {
        let m = J::parse(
            r#"{"properties":{"first":{"type":"text","copy_to":"full"},"last":{"type":"text","copy_to":["full","o.all"]},
                "full":{"type":"text"},"o":{"properties":{"all":{"type":"text"},"x":{"type":"long"}}}}}"#,
        )
        .unwrap();
        let mut f = Vec::new();
        source_fields(&m, &mut f);
        let got: Vec<&str> = f.iter().map(|f| f.name.as_str()).collect();
        assert_eq!(got, ["first", "last", "o", "o.x"]);
        for bad in ["full", "o.all"] {
            let err = read_columns(Some(&names(&["_id", bad])), &f).unwrap_err().to_string();
            assert!(err.contains(&format!("la lectura no trae la columna «{bad}»")), "{err}");
        }
    }
}
