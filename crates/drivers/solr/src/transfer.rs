//! Bulk transfer (see `dbine_driver::transfer`) for Apache Solr, standalone
//! and SolrCloud.
//!
//! Reading: `select` with `cursorMark` paging, sorted by the collection's
//! uniqueKey. The response is parsed as it arrives ([`DocScanner`]): only
//! one document is held at a time, whatever the page size, so wide
//! documents don't blow the memory bound. Pages start at [`PAGE_FIRST`]
//! documents and are sized after the previous page's bytes (about
//! [`PAGE_BYTES`], at most [`PAGE`] documents), which keeps each request
//! short for the server. Rows have the
//! browse's shape: the schema's fields (uniqueKey included) and the
//! dynamic-field instances in the index. Values are typed by the field
//! type's class: integers, floats, booleans, dates (`DateTimeTz` in UTC),
//! `BinaryField` as bytes, `UUIDField` as UUIDs; multi-valued fields become
//! JSON arrays. Asked-for columns the schema can't hold are an error. A
//! filter is a Solr query, applied as `fq`.
//!
//! Loading: `/update` with JSON arrays of documents of about
//! [`REQUEST_BYTES`], [`IN_FLIGHT`] at once. The load only inserts: every
//! document goes with `_version_: -1` (Solr answers 409 if its key exists)
//! and, before each request, a real-time get checks that none of its keys
//! exists yet, so an existing document is never replaced and a key repeated
//! in the input is an error. Every `commit_rows` rows or `commit_bytes`
//! bytes the requests in flight are awaited and committed, and progress
//! reports the committed rows (always a prefix of the input). If the load
//! fails, the documents this load sent in the window not yet committed are
//! deleted by key (nothing is left in flight to be committed later by
//! autoCommit); documents that existed before are never among them. Each
//! value is written for the target field's type and is checked to fit it:
//! Solr truncates fractions and wraps integers without complaint, so a
//! value that doesn't fit exactly is an error, never a silent change.
//! Fields the schema doesn't define are refused (a schemaless collection
//! would guess their type from the first value and truncate the next ones),
//! and so are numbers into field types DBine doesn't know. Dates go as UTC
//! instants (zone-less values taken as UTC, Solr has no other kind), finer
//! than milliseconds only if the extra digits are zeros; binaries as
//! Base64; JSON arrays as multi-valued fields (an empty list, or a null
//! item, can't be stored: refused); numeric text and JSON scalars go
//! through the same exactness check as numbers; a value for a field that
//! is neither stored nor docValues (Solr would drop it) is refused; nulls, `_version_` and `score` are left out, and so is
//! a copyField destination whose value is exactly what Solr copies into it
//! from the row's source columns (any other value, or NULL, is refused:
//! Solr would replace or fill it). Solr copies a number as Java prints the
//! object it parsed (`100` for an integer, `100.0` or `1.0E-7` for a
//! double), so a float source goes as the one that gives the row's copy;
//! when the server's Java prints a double with digits DBine can't know
//! (Java 17 outside 10⁻³⁰..10¹⁵), the copy is refused. A negative zero
//! keeps its sign, and a float field takes a decimal only if Solr prints
//! the float back as that decimal.

use crate::{solr_error_message, SolrSession};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result, Session};
use serde_json::{Map, Value};
use std::collections::HashSet;
use std::io::Write;
use std::time::Duration;
use tokio::task::JoinSet;

/// Most documents per page of the read.
pub const PAGE: usize = 5_000;
/// Documents in the first page, before any size is known.
pub const PAGE_FIRST: usize = 10;
/// Target size of a page's response.
pub const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// An `/update` request is sent once its body reaches this size.
pub const REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// `/update` requests in flight at once.
pub const IN_FLIGHT: usize = 4;
/// Ids per delete request when undoing a failed window.
const DELETE_IDS: usize = 5_000;
/// Ids per real-time get checking that a request's keys are new.
const GET_IDS: usize = 2_000;
/// One page or one `/update` request.
const TIMEOUT: Duration = Duration::from_secs(300);
/// Never written: Solr assigns them.
const SKIP: [&str; 2] = ["_version_", "score"];

/// What a field holds, from its field type's class.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    Int32,
    Int64,
    Float32,
    Float64,
    Bool,
    Date,
    Binary,
    Uuid,
    Text,
    /// Unknown field (schemaless, or a type DBine doesn't know).
    Other,
}

impl Kind {
    pub fn from_class(class: &str) -> Kind {
        let c = class.rsplit('.').next().unwrap_or(class);
        match c {
            "IntPointField" | "TrieIntField" => Kind::Int32,
            "LongPointField" | "TrieLongField" => Kind::Int64,
            "FloatPointField" | "TrieFloatField" => Kind::Float32,
            "DoublePointField" | "TrieDoubleField" => Kind::Float64,
            "BoolField" => Kind::Bool,
            "DatePointField" | "TrieDateField" | "DateRangeField" => Kind::Date,
            "BinaryField" => Kind::Binary,
            "UUIDField" => Kind::Uuid,
            "StrField" | "TextField" | "SortableTextField" | "ICUCollationField" | "CollationField" => Kind::Text,
            _ => Kind::Other,
        }
    }

    fn is_int(self) -> bool {
        matches!(self, Kind::Int32 | Kind::Int64)
    }

    fn is_float(self) -> bool {
        matches!(self, Kind::Float32 | Kind::Float64)
    }
}

/// A resolved field: its type name (`[]` when multi-valued) and kind.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FieldType {
    pub type_name: String,
    pub kind: Kind,
    pub multi: bool,
}

/// The parts of a collection's schema a transfer needs.
#[derive(Debug, Default)]
pub struct Schema {
    pub key: Option<String>,
    /// `(name, type, multiValued)`.
    fields: Vec<(String, String, Option<bool>)>,
    /// `(pattern, type, multiValued)`, longest pattern first.
    dynamic: Vec<(String, String, Option<bool>)>,
    /// `(name, class, multiValued)`.
    types: Vec<(String, String, Option<bool>)>,
    /// `(source, dest, maxChars)` of the copyField rules.
    copies: Vec<(String, String, Option<usize>)>,
    /// The `stored`/`docValues`/`useDocValuesAsStored` flags of fields
    /// (`f:name`), dynamic fields (`d:pattern`) and field types (`t:name`).
    flags: std::collections::HashMap<String, [Option<bool>; 3]>,
    /// The schema's `version` (docValues default on from 1.7).
    version: f64,
}

impl Schema {
    /// From `GET /solr/<c>/schema`.
    pub fn parse(v: &Value) -> Schema {
        let s = v.get("schema").unwrap_or(v);
        let list = |k: &str, second: &str| -> Vec<(String, String, Option<bool>)> {
            s.get(k)
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|f| {
                    Some((
                        f.get("name")?.as_str()?.to_string(),
                        f.get(second).and_then(Value::as_str).unwrap_or_default().to_string(),
                        f.get("multiValued").and_then(Value::as_bool),
                    ))
                })
                .collect()
        };
        let mut dynamic = list("dynamicFields", "type");
        dynamic.sort_by_key(|d| std::cmp::Reverse(d.0.len()));
        let mut flags = std::collections::HashMap::new();
        for (k, p) in [("fields", "f:"), ("dynamicFields", "d:"), ("fieldTypes", "t:")] {
            for f in s.get(k).and_then(Value::as_array).into_iter().flatten() {
                let Some(name) = f.get("name").and_then(Value::as_str) else { continue };
                let b = |k: &str| f.get(k).and_then(|v| v.as_bool().or_else(|| v.as_str()?.parse().ok()));
                flags.insert(format!("{p}{name}"), [b("stored"), b("docValues"), b("useDocValuesAsStored")]);
            }
        }
        Schema {
            flags,
            version: s.get("version").and_then(|v| v.as_f64().or_else(|| v.as_str()?.parse().ok())).unwrap_or(0.0),
            key: s.get("uniqueKey").and_then(Value::as_str).map(str::to_string),
            fields: list("fields", "type"),
            dynamic,
            types: list("fieldTypes", "class"),
            copies: s
                .get("copyFields")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
                .filter_map(|c| {
                    let max = c.get("maxChars").and_then(|m| m.as_u64().or_else(|| m.as_str()?.parse().ok())).map(|m| m as usize);
                    Some((c.get("source")?.as_str()?.to_string(), c.get("dest")?.as_str()?.to_string(), max))
                })
                .collect(),
        }
    }

    /// Whether the schema defines `name` (explicitly or by a dynamic pattern).
    pub fn defines(&self, name: &str) -> bool {
        self.fields.iter().any(|f| f.0 == name) || self.dynamic.iter().any(|d| dynamic_match(&d.0, name))
    }

    /// The fields Solr fills from `name` by copyField.
    pub fn copy_dests(&self, name: &str) -> Vec<String> {
        self.copy_rules(name).into_iter().map(|(d, _)| d).collect()
    }

    /// The fields Solr fills from `name` by copyField, with each rule's
    /// `maxChars` (the copy keeps only that many characters).
    pub fn copy_rules(&self, name: &str) -> Vec<(String, Option<usize>)> {
        self.copies
            .iter()
            .filter(|(src, _, _)| dynamic_match(src, name))
            .filter_map(|(src, dest, max)| {
                if !dest.contains('*') {
                    return Some((dest.clone(), *max));
                }
                // `*_t` → `*_s`: the part the source's `*` matched.
                let part = if src == "*" {
                    name
                } else if let Some(suffix) = src.strip_prefix('*') {
                    &name[..name.len() - suffix.len()]
                } else {
                    &name[src.strip_suffix('*')?.len()..]
                };
                Some((dest.replacen('*', part, 1), *max))
            })
            .filter(|(d, _)| d != name)
            .collect()
    }

    /// A field by name: explicit, else the longest dynamic pattern.
    pub fn field(&self, name: &str) -> Option<FieldType> {
        let (ty, mv) = match self.fields.iter().find(|f| f.0 == name) {
            Some(f) => (&f.1, f.2),
            None => {
                let d = self.dynamic.iter().find(|d| dynamic_match(&d.0, name))?;
                (&d.1, d.2)
            }
        };
        let t = self.types.iter().find(|t| t.0 == *ty);
        let multi = mv.or(t.and_then(|t| t.2)).unwrap_or(false);
        let kind = t.map_or(Kind::Other, |t| Kind::from_class(&t.1));
        Some(FieldType { type_name: if multi { format!("{ty}[]") } else { ty.clone() }, kind, multi })
    }

    /// Whether a value written to `name` can be read back: stored, or with
    /// docValues returned as stored. A field that is neither (the
    /// `ignored_*` of the default schema, `stored="false"` without
    /// docValues) takes the value and drops it without an error.
    pub fn retrievable(&self, name: &str) -> bool {
        let (key, ty) = match self.fields.iter().find(|f| f.0 == name) {
            Some(f) => (format!("f:{}", f.0), f.1.clone()),
            None => match self.dynamic.iter().find(|d| dynamic_match(&d.0, name)) {
                Some(d) => (format!("d:{}", d.0), d.1.clone()),
                None => return true,
            },
        };
        let none = [None; 3];
        let f = self.flags.get(&key).unwrap_or(&none);
        let t = self.flags.get(&format!("t:{ty}")).unwrap_or(&none);
        let flag = |i: usize| f[i].or(t[i]);
        if flag(0).unwrap_or(true) {
            return true;
        }
        // docValues are on by default from schema 1.7 for the types that
        // have them (not text or binary); unsure cases count as off, so
        // the worst outcome is a refusal, never a silent loss.
        let class = self.types.iter().find(|x| x.0 == ty).map(|x| x.1.as_str()).unwrap_or_default();
        let kind = Kind::from_class(class);
        let default_dv = self.version >= 1.7 && !matches!(kind, Kind::Other | Kind::Binary) && !class.ends_with("TextField");
        flag(1).unwrap_or(default_dv) && flag(2).unwrap_or(true)
    }
}

/// Solr's dynamic field patterns: `*_s`, `attr_*` or `*`.
pub fn dynamic_match(pattern: &str, name: &str) -> bool {
    if pattern == "*" {
        return true;
    }
    if let Some(suffix) = pattern.strip_prefix('*') {
        return name.len() > suffix.len() && name.ends_with(suffix);
    }
    if let Some(prefix) = pattern.strip_suffix('*') {
        return name.len() > prefix.len() && name.starts_with(prefix);
    }
    pattern == name
}

// ---- Base64 (what BinaryField takes and gives) ----

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub fn b64_encode(data: &[u8]) -> String {
    let mut out = String::with_capacity(data.len().div_ceil(3) * 4);
    for c in data.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        for i in 0..4 {
            if i <= c.len() {
                out.push(B64[(n >> (18 - 6 * i) & 63) as usize] as char);
            } else {
                out.push('=');
            }
        }
    }
    out
}

pub fn b64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for &b in s.as_bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' => 62,
            b'/' => 63,
            b'=' => break,
            b'\r' | b'\n' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
        }
    }
    Some(out)
}

// ---- Dates ----

/// Solr's `YYYY-MM-DDTHH:MM:SS[.fff]Z` as `YYYY-MM-DD HH:MM:SS[.fff]+00:00`;
/// `None` for anything else (partial DateRange values, years past 9999).
pub fn solr_date_to_cell(s: &str) -> Option<String> {
    let b = s.as_bytes();
    if b.len() < 20 || b[10] != b'T' || !s.ends_with('Z') || b[4] != b'-' || b[13] != b':' {
        return None;
    }
    Some(format!("{} {}+00:00", &s[..10], &s[11..s.len() - 1]))
}

fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// A cell's date as the UTC instant Solr takes (`…T…Z`). Values without a
/// zone are taken as UTC. `None` when the text isn't a date.
pub fn cell_date_to_solr(c: &Cell) -> Option<String> {
    let num = |s: &str| s.parse::<i64>().ok();
    match c {
        Cell::Date(s) => (s.len() == 10).then(|| format!("{s}T00:00:00Z")),
        Cell::DateTime(s) => (s.len() >= 19 && s.as_bytes()[10] == b' ').then(|| format!("{}T{}Z", &s[..10], &s[11..])),
        Cell::DateTimeTz(s) => {
            if s.len() < 25 || s.as_bytes()[10] != b' ' {
                return None;
            }
            let (dt, off) = s.split_at(s.len() - 6);
            let sign = match off.as_bytes()[0] {
                b'+' => 1,
                b'-' => -1,
                _ => return None,
            };
            let offset = sign * (num(&off[1..3])? * 60 + num(&off[4..6])?);
            let (y, mo, d) = (num(&dt[0..4])?, num(&dt[5..7])?, num(&dt[8..10])?);
            let (h, mi) = (num(&dt[11..13])?, num(&dt[14..16])?);
            let rest = &dt[16..]; // `:SS[.fff]`
            let minutes = days_from_civil(y, mo, d) * 1440 + h * 60 + mi - offset;
            let (y, mo, d) = civil_from_days(minutes.div_euclid(1440));
            let m = minutes.rem_euclid(1440);
            Some(format!("{y:04}-{mo:02}-{d:02}T{:02}:{:02}{rest}Z", m / 60, m % 60))
        }
        _ => None,
    }
}

// ---- Reading ----

/// A document's value as a cell, for a field of kind `kind`.
pub fn to_cell(v: &Value, kind: Kind) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => {
            if kind.is_float() {
                Cell::Float(n.as_f64().unwrap_or(f64::NAN))
            } else if let Some(i) = n.as_i64() {
                Cell::Int(i)
            } else if let Some(u) = n.as_u64() {
                Cell::UInt(u)
            } else {
                Cell::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => match kind {
            Kind::Date => solr_date_to_cell(s).map_or_else(|| Cell::Text(s.clone()), Cell::DateTimeTz),
            Kind::Binary => b64_decode(s).map_or_else(|| Cell::Text(s.clone()), Cell::Bytes),
            Kind::Uuid => Cell::Uuid(s.clone()),
            _ => Cell::Text(s.clone()),
        },
        other => Cell::Json(other.to_string()),
    }
}

pub fn doc_row(doc: &Map<String, Value>, cols: &[(String, Kind)]) -> Vec<Cell> {
    cols.iter().map(|(n, k)| doc.get(n).map_or(Cell::Null, |v| to_cell(v, *k))).collect()
}

/// A name `fl` takes as a plain field: anything else (`score`, `[docid]`,
/// commas, spaces, functions) would be read as something else.
pub fn plain_fl_name(n: &str) -> bool {
    n != "score"
        && n.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// The next page's size, after a page of `rows` documents that took
/// `bytes`: about [`PAGE_BYTES`], growing at most fourfold.
pub fn next_page(rows: usize, bytes: usize) -> usize {
    if rows == 0 {
        return PAGE_FIRST;
    }
    let fit = rows.saturating_mul(PAGE_BYTES) / bytes.max(1);
    fit.clamp(1, rows.saturating_mul(4).min(PAGE))
}

/// A path segment, percent-encoded.
pub fn seg(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for b in s.bytes() {
        if b.is_ascii_alphanumeric() || matches!(b, b'-' | b'_' | b'.' | b'~') {
            out.push(b as char);
        } else {
            out.push_str(&format!("%{b:02X}"));
        }
    }
    out
}

/// Reads a `select` response as it arrives: hands out each document of
/// `response.docs` as soon as it's complete and keeps `nextCursorMark`.
/// Only the document being read (plus a chunk) is held in memory.
#[derive(Default)]
pub struct DocScanner {
    buf: Vec<u8>,
    /// Next byte of `buf` to scan.
    pos: usize,
    /// Open containers: `(b'{' or b'[', expecting a key)`.
    stack: Vec<(u8, bool)>,
    /// The current key of the two outer objects.
    keys: [String; 2],
    in_str: bool,
    esc: bool,
    str_start: usize,
    str_is_key: bool,
    doc_start: Option<usize>,
    pub cursor: Option<String>,
    /// Largest `buf` seen (for tests).
    pub peak: usize,
}

impl DocScanner {
    /// Scan one more chunk, calling `doc` with the JSON text of each
    /// document completed in it.
    pub fn feed(&mut self, chunk: &[u8], doc: &mut dyn FnMut(&[u8]) -> Result<()>) -> Result<()> {
        self.buf.extend_from_slice(chunk);
        self.peak = self.peak.max(self.buf.len());
        while self.pos < self.buf.len() {
            let at = self.pos;
            let b = self.buf[at];
            self.pos += 1;
            if self.in_str {
                if self.esc {
                    self.esc = false;
                } else if b == b'\\' {
                    self.esc = true;
                } else if b == b'"' {
                    self.in_str = false;
                    self.end_string(at);
                }
                continue;
            }
            match b {
                b'"' => {
                    self.in_str = true;
                    self.str_start = at;
                    self.str_is_key = matches!(self.stack.last(), Some((b'{', true)));
                }
                b'{' | b'[' => {
                    if b == b'{'
                        && self.doc_start.is_none()
                        && self.stack.len() == 3
                        && self.stack[2].0 == b'['
                        && self.keys[0] == "response"
                        && self.keys[1] == "docs"
                    {
                        self.doc_start = Some(at);
                    }
                    self.stack.push((b, b == b'{'));
                }
                b'}' | b']' => {
                    self.stack.pop();
                    if b == b'}' && self.stack.len() == 3 {
                        if let Some(start) = self.doc_start.take() {
                            doc(&self.buf[start..=at])?;
                        }
                    }
                }
                b':' => {
                    if let Some(top) = self.stack.last_mut() {
                        top.1 = false;
                    }
                }
                b',' => {
                    if let Some(top) = self.stack.last_mut() {
                        top.1 = top.0 == b'{';
                    }
                }
                _ => {}
            }
        }
        // Drop what's been read, except a document or a string in progress.
        let keep = self.doc_start.unwrap_or(if self.in_str { self.str_start } else { self.pos });
        if keep > 0 {
            self.buf.drain(..keep);
            self.pos -= keep;
            self.str_start = self.str_start.saturating_sub(keep);
            self.doc_start = self.doc_start.map(|s| s - keep);
        }
        Ok(())
    }

    fn end_string(&mut self, end: usize) {
        let depth = self.stack.len();
        if self.doc_start.is_some() || depth == 0 || depth > 2 {
            return;
        }
        let text = || serde_json::from_slice::<String>(&self.buf[self.str_start..=end]).unwrap_or_default();
        if self.str_is_key {
            self.keys[depth - 1] = text();
        } else if depth == 1 && self.keys[0] == "nextCursorMark" {
            self.cursor = Some(text());
        }
    }
}

// ---- Loading ----

/// A target field of the load.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Col {
    pub name: String,
    pub kind: Kind,
    /// Multi-valued (or unknown to the schema: Solr decides).
    pub multi: bool,
    /// The load's columns that fill this one by copyField, with the rule's
    /// `maxChars`: when any of them has a value in the row, Solr fills this
    /// one from them.
    pub fed_by: Vec<(usize, Option<usize>)>,
    /// Neither stored nor docValues: Solr accepts a value and drops it.
    pub hidden: bool,
    /// How the server's Java prints numbers (see [`java_double_on`]).
    pub java: JavaText,
}

impl Col {
    pub fn new(name: &str, kind: Kind, multi: bool) -> Col {
        Col { name: name.to_string(), kind, multi, fed_by: Vec::new(), hidden: false, java: JavaText::Unknown }
    }
}

/// Which `Double.toString`/`Float.toString` the server's Java has.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum JavaText {
    /// Unknown version: only the texts both kinds agree on are predicted.
    #[default]
    Unknown,
    /// Java 17 or 18 (Solr 9's usual JVM): [`java17_double`]/[`java17_float`].
    Java17,
    /// Java 19 or later: [`java_double`]/[`java_float`].
    Java19,
}

// ---- Java's number texts ----

/// Java's layout of a value's digits, from Rust's `{:e}` of the digits
/// chosen and of the value rounded to two digits: at least two digits
/// (the closer of them when one would do), plain from 10⁻³ to 10⁷
/// (`100.0`, `0.001`), else `d.dddE±n` (`1.0E-7`, `1.6777216E7`).
fn java_layout(short: &str, two: &str, same: impl Fn(&str) -> bool) -> String {
    let parts = |s: &str| -> (bool, String, i32) {
        let (m, e) = s.split_once('e').unwrap_or((s, "0"));
        (m.starts_with('-'), m.bytes().filter(u8::is_ascii_digit).map(char::from).collect(), e.parse().unwrap_or(0))
    };
    let (neg, mut digits, mut n) = parts(short);
    if digits.len() == 1 && same(two) {
        (_, digits, n) = parts(two);
    }
    let d = digits.trim_end_matches('0');
    let body = if d.is_empty() {
        "0.0".to_string()
    } else if (-3..7).contains(&n) {
        if n < 0 {
            format!("0.{}{d}", "0".repeat((-n - 1) as usize))
        } else if d.len() as i32 > n + 1 {
            format!("{}.{}", &d[..=n as usize], &d[n as usize + 1..])
        } else {
            format!("{d}{}.0", "0".repeat((n + 1) as usize - d.len()))
        }
    } else {
        format!("{}.{}E{n}", &d[..1], if d.len() > 1 { &d[1..] } else { "0" })
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// The shortest digits of a value (Rust's `{:e}`), a tie between two of
/// that length going to the even one, as Java does.
fn shortest_even(short: String, exact: impl Fn(usize) -> String, same: impl Fn(&str) -> bool) -> String {
    let len = short.split('e').next().unwrap_or_default().bytes().filter(u8::is_ascii_digit).count();
    let even = exact(len.saturating_sub(1));
    if same(&even) {
        even
    } else {
        short
    }
}

/// Java 19+'s `Double.toString(v)`: what Solr prints for a double, and
/// what a copyField copies from a number Solr parsed as a double.
pub fn java_double(v: f64) -> String {
    let same = |s: &str| s.parse::<f64>() == Ok(v);
    let short = shortest_even(format!("{v:e}"), |p| format!("{v:.p$e}"), same);
    java_layout(&short, &format!("{v:.1e}"), same)
}

/// Java 19+'s `Float.toString(f)`: what Solr prints for a `pfloat`.
pub fn java_float(f: f32) -> String {
    let same = |s: &str| s.parse::<f32>() == Ok(f);
    let short = shortest_even(format!("{f:e}"), |p| format!("{:.p$e}", f as f64), same);
    java_layout(&short, &format!("{:.1e}", f as f64), same)
}

/// The text the server's Java gives a double; `None` when its version is
/// unknown and Java 17 and 19 print the value differently.
pub fn java_double_on(v: f64, java: JavaText) -> Option<String> {
    match java {
        JavaText::Java19 => Some(java_double(v)),
        JavaText::Java17 => Some(java17_double(v)),
        JavaText::Unknown => Some(java_double(v)).filter(|s| *s == java17_double(v)),
    }
}

/// The text the server's Java gives a float (see [`java_double_on`]).
pub fn java_float_on(f: f32, java: JavaText) -> Option<String> {
    match java {
        JavaText::Java19 => Some(java_float(f)),
        JavaText::Java17 => Some(java17_float(f)),
        JavaText::Unknown => Some(java_float(f)).filter(|s| *s == java17_float(f)),
    }
}

// Java 8-18's `FloatingDecimal`, ported as is (Java 19 replaced it). Its
// digits aren't always the shortest (`2⁻²⁴` is `5.9604644775390625E-8`,
// `1e23` is `9.999999999999999E22`). Checked against Java 17 on every
// float and on millions of doubles (every `m·2ᵉ` with odd `m` < 2048,
// subnormals, random bits, short decimals, powers of ten).

/// An unsigned big integer (32-bit words, least significant first): what
/// `FloatingDecimal` needs of its `FDBigInteger`.
#[derive(Clone)]
struct Big(Vec<u32>);

impl Big {
    /// `v · 5^e5 · 2^e2`.
    fn pow52(v: u64, e5: i32, e2: i32) -> Big {
        let mut b = Big(vec![v as u32, (v >> 32) as u32]);
        for _ in 0..e5 {
            b.mul_small(5);
        }
        b.trim();
        b.shl(e2.max(0) as usize);
        b
    }

    fn trim(&mut self) {
        while self.0.last() == Some(&0) {
            self.0.pop();
        }
    }

    fn mul_small(&mut self, m: u32) {
        let mut carry = 0u64;
        for w in &mut self.0 {
            let p = *w as u64 * m as u64 + carry;
            *w = p as u32;
            carry = p >> 32;
        }
        if carry > 0 {
            self.0.push(carry as u32);
        }
    }

    fn shl(&mut self, n: usize) {
        let bits = (n % 32) as u32;
        if bits > 0 {
            let mut carry = 0u32;
            for w in &mut self.0 {
                let next = *w >> (32 - bits);
                *w = (*w << bits) | carry;
                carry = next;
            }
            if carry > 0 {
                self.0.push(carry);
            }
        }
        if !self.0.is_empty() {
            self.0.splice(0..0, std::iter::repeat_n(0, n / 32));
        }
    }

    fn add(&self, o: &Big) -> Big {
        let mut out = Vec::with_capacity(self.0.len().max(o.0.len()) + 1);
        let mut carry = 0u64;
        for i in 0..self.0.len().max(o.0.len()) {
            let s = *self.0.get(i).unwrap_or(&0) as u64 + *o.0.get(i).unwrap_or(&0) as u64 + carry;
            out.push(s as u32);
            carry = s >> 32;
        }
        out.push(carry as u32);
        let mut b = Big(out);
        b.trim();
        b
    }

    /// `self -= o`, with `self >= o`.
    fn sub(&mut self, o: &Big) {
        let mut borrow = 0i64;
        for i in 0..self.0.len() {
            let d = self.0[i] as i64 - *o.0.get(i).unwrap_or(&0) as i64 - borrow;
            self.0[i] = d as u32;
            borrow = (d < 0) as i64;
        }
        self.trim();
    }

    fn cmp(&self, o: &Big) -> std::cmp::Ordering {
        self.0.len().cmp(&o.0.len()).then_with(|| self.0.iter().rev().cmp(o.0.iter().rev()))
    }

    /// `FDBigInteger.quoRemIteration`: `self / s`, leaving `10·(self % s)`.
    fn quo_rem(&mut self, s: &Big) -> i32 {
        let mut q = 0;
        while self.cmp(s).is_ge() {
            self.sub(s);
            q += 1;
        }
        self.mul_small(10);
        q
    }
}

const N_5_BITS: [i32; 27] = [0, 3, 5, 7, 10, 12, 14, 17, 19, 21, 24, 26, 28, 31, 33, 35, 38, 40, 42, 45, 47, 49, 52, 54, 56, 59, 61];
const INSIGNIFICANT_DIGITS: [i32; 64] = [
    0, 0, 0, 0, 1, 1, 1, 2, 2, 2, 3, 3, 3, 3, 4, 4, 4, 5, 5, 5, 6, 6, 6, 6, 7, 7, 7, 8, 8, 8, 9, 9, 9, 9, 10, 10, 10, 11, 11, 11, 12,
    12, 12, 12, 13, 13, 13, 14, 14, 14, 15, 15, 15, 15, 16, 16, 16, 17, 17, 17, 18, 18, 18, 19,
];

fn n5bits(e5: i32) -> i32 {
    N_5_BITS.get(e5 as usize).copied().unwrap_or(e5 * 3)
}

/// `FloatingDecimal.dtoa`: the digits and decimal exponent (`0.d₁d₂… ·
/// 10ᵉ`) of `fract · 2^(bin_exp-52)`, `fract` with its bit 52 set.
// Java's constant `0.301029995663981` is not `LOG10_2`: kept as Java has it.
#[allow(clippy::approx_constant)]
fn java17_dtoa(bin_exp: i32, mut fract: u64, n_sig: i32) -> (Vec<u8>, i32) {
    let tail_zeros = fract.trailing_zeros() as i32;
    let n_fract = 53 - tail_zeros;
    let n_tiny = (n_fract - bin_exp - 1).max(0);
    if (-21..=62).contains(&bin_exp) && n_tiny == 0 && n_fract < 64 {
        // An integer: its digits, the ones past the precision rounded off.
        let p2 = bin_exp - n_sig - 1;
        let insignificant = if bin_exp > n_sig && p2 > 1 && p2 < 64 { INSIGNIFICANT_DIGITS[p2 as usize] } else { 0 };
        let mut l = if bin_exp >= 52 { fract << (bin_exp - 52) } else { fract >> (52 - bin_exp) };
        if insignificant != 0 {
            let pow10 = 10u64.pow(insignificant as u32);
            let residue = l % pow10;
            l /= pow10;
            if residue >= pow10 >> 1 {
                l += 1;
            }
        }
        let s = l.to_string();
        return (s.trim_end_matches('0').as_bytes().to_vec(), insignificant + s.len() as i32);
    }
    let d2 = f64::from_bits((1023u64 << 52) | (fract & ((1u64 << 52) - 1)));
    let mut dec_exp = ((d2 - 1.5) * 0.289529654 + 0.176091259 + bin_exp as f64 * 0.301029995663981).floor() as i32;
    let b5 = (-dec_exp).max(0);
    let mut b2 = b5 + n_tiny + bin_exp;
    let s5 = dec_exp.max(0);
    let mut s2 = s5 + n_tiny;
    let m5 = b5;
    let mut m2 = b2 - n_sig;
    fract >>= tail_zeros;
    b2 -= n_fract - 1;
    let common = b2.min(s2);
    b2 -= common;
    s2 -= common;
    m2 -= common;
    // Java's own hack for exact powers of two.
    if n_fract == 1 {
        m2 -= 1;
    }
    if m2 < 0 {
        b2 -= m2;
        s2 -= m2;
        m2 = 0;
    }
    let b_bits = n_fract + b2 + n5bits(b5);
    let ten_s_bits = s2 + 1 + n5bits(s5 + 1);
    let mut digits = Vec::with_capacity(20);
    // At least two digits in E-form.
    let e_form = |e: i32| !(-3..8).contains(&e);
    let (low, high, low_diff);
    if b_bits < 64 && ten_s_bits < 64 {
        // Java does this in int or long arithmetic, overflows included.
        macro_rules! small {
            ($t:ty) => {{
                let p5 = |e: i32| 5i64.pow(e as u32) as $t;
                let mut b = (fract as $t).wrapping_mul(p5(b5)).wrapping_shl(b2 as u32);
                let s = p5(s5).wrapping_shl(s2 as u32);
                let mut m = p5(m5).wrapping_shl(m2 as u32);
                let tens = s.wrapping_mul(10);
                let mut q = b / s;
                b = (b % s).wrapping_mul(10);
                m = m.wrapping_mul(10);
                let (mut lo, mut hi) = (b < m, b.wrapping_add(m) > tens);
                if q == 0 && !hi {
                    dec_exp -= 1;
                } else {
                    digits.push(b'0' + q as u8);
                }
                if e_form(dec_exp) {
                    (lo, hi) = (false, false);
                }
                while !lo && !hi {
                    q = b / s;
                    b = (b % s).wrapping_mul(10);
                    m = m.wrapping_mul(10);
                    (lo, hi) = if m > 0 { (b < m, b.wrapping_add(m) > tens) } else { (true, true) };
                    digits.push(b'0' + q as u8);
                }
                (lo, hi, b.wrapping_shl(1).wrapping_sub(tens).signum() as i32)
            }};
        }
        (low, high, low_diff) = if b_bits < 32 && ten_s_bits < 32 { small!(i32) } else { small!(i64) };
    } else {
        let s = Big::pow52(1, s5, s2);
        let mut b = Big::pow52(fract, b5, b2);
        let mut m = Big::pow52(1, m5 + 1, m2 + 1);
        let ten_s = Big::pow52(1, s5 + 1, s2 + 1);
        let q = b.quo_rem(&s);
        let (mut lo, mut hi) = (b.cmp(&m).is_lt(), ten_s.cmp(&b.add(&m)).is_le());
        if q == 0 && !hi {
            dec_exp -= 1;
        } else {
            digits.push(b'0' + q as u8);
        }
        if e_form(dec_exp) {
            (lo, hi) = (false, false);
        }
        while !lo && !hi {
            let q = b.quo_rem(&s);
            m.mul_small(10);
            (lo, hi) = (b.cmp(&m).is_lt(), ten_s.cmp(&b.add(&m)).is_le());
            digits.push(b'0' + q as u8);
        }
        let diff = if hi && lo {
            b.shl(1);
            b.cmp(&ten_s) as i32
        } else {
            0
        };
        (low, high, low_diff) = (lo, hi, diff);
    }
    let mut dec = dec_exp + 1;
    // The last digit, rounded by how the loop stopped.
    let odd = digits.last().is_some_and(|d| d & 1 == 1);
    if high && !digits.is_empty() && (!low || low_diff > 0 || (low_diff == 0 && odd)) {
        let mut i = digits.len() - 1;
        while digits[i] == b'9' && i > 0 {
            digits[i] = b'0';
            i -= 1;
        }
        if digits[i] == b'9' {
            dec += 1;
            digits[0] = b'1';
        } else {
            digits[i] += 1;
        }
    }
    (digits, dec)
}

/// `FloatingDecimal`'s text of `0.d₁d₂… · 10ᵉ`.
fn java17_text(neg: bool, d: &[u8], e: i32) -> String {
    let d = std::str::from_utf8(d).unwrap_or_default();
    let body = if e > 0 && e < 8 {
        let int = d.len().min(e as usize);
        if int < e as usize {
            format!("{d}{}.0", "0".repeat(e as usize - int))
        } else if int < d.len() {
            format!("{}.{}", &d[..int], &d[int..])
        } else {
            format!("{d}.0")
        }
    } else if e <= 0 && e > -3 {
        format!("0.{}{d}", "0".repeat((-e) as usize))
    } else {
        let rest = if d.len() > 1 { &d[1..] } else { "0" };
        format!("{}.{rest}E{}", d.get(..1).unwrap_or("0"), e - 1)
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// Java 8-18's `Double.toString(v)`.
pub fn java17_double(v: f64) -> String {
    if !v.is_finite() || v == 0.0 {
        return java_double(v);
    }
    let bits = v.to_bits();
    let mut fract = bits & ((1u64 << 52) - 1);
    let mut bin_exp = ((bits >> 52) & 0x7ff) as i32;
    let n_sig;
    if bin_exp == 0 {
        let lz = fract.leading_zeros() as i32;
        fract <<= lz - 11;
        bin_exp = 12 - lz;
        n_sig = 64 - lz;
    } else {
        fract |= 1u64 << 52;
        n_sig = 53;
    }
    let (d, e) = java17_dtoa(bin_exp - 1023, fract, n_sig);
    java17_text(v.is_sign_negative(), &d, e)
}

/// Java 8-18's `Float.toString(f)`.
pub fn java17_float(f: f32) -> String {
    if !f.is_finite() || f == 0.0 {
        return java_float(f);
    }
    let bits = f.to_bits();
    let mut fract = bits & ((1u32 << 23) - 1);
    let mut bin_exp = ((bits >> 23) & 0xff) as i32;
    let n_sig;
    if bin_exp == 0 {
        let lz = fract.leading_zeros() as i32;
        fract <<= lz - 8;
        bin_exp = 9 - lz;
        n_sig = 32 - lz;
    } else {
        fract |= 1u32 << 23;
        n_sig = 24;
    }
    let (d, e) = java17_dtoa(bin_exp - 127, (fract as u64) << 29, n_sig);
    java17_text(f.is_sign_negative(), &d, e)
}

/// The load's target fields, from the target's schema. A field the schema
/// doesn't define is refused: a schemaless collection would guess its type
/// from the first value (a `plongs` from `5`) and truncate the next ones
/// (`12.75` → 12) without an error.
pub fn load_cols(schema: &Schema, names: &[String]) -> Result<Vec<Col>> {
    let mut cols = Vec::with_capacity(names.len());
    for n in names {
        match schema.field(n) {
            Some(f) => cols.push(Col { hidden: !schema.retrievable(n), ..Col::new(n, f.kind, f.multi) }),
            None if SKIP.contains(&n.as_str()) => cols.push(Col::new(n, Kind::Other, false)),
            None => {
                return Err(Error::Unsupported(format!(
                    "el campo «{n}» no está definido en el esquema de la colección de destino: Solr adivinaría su tipo con el primer valor y cambiaría los siguientes sin avisar (por ejemplo, 12.75 quedaría en 12). Definí el campo en el esquema antes de cargar."
                )))
            }
        }
    }
    for (i, n) in names.iter().enumerate() {
        for (d, max) in schema.copy_rules(n) {
            if let Some(j) = names.iter().position(|x| *x == d) {
                cols[j].fed_by.push((i, max));
            }
        }
    }
    Ok(cols)
}

/// The items of a JSON array of scalars, numbers kept with their exact
/// digits (as [`Cell::Decimal`]); `None` if `s` isn't one.
pub fn array_items(s: &str) -> Option<Vec<Cell>> {
    let b = s.trim().as_bytes();
    if b.first() != Some(&b'[') || b.last() != Some(&b']') {
        return None;
    }
    let mut items = Vec::new();
    let mut i = 1;
    let end = b.len() - 1;
    let mut want_item = true;
    while i < end {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
        } else if c == b',' && !want_item {
            want_item = true;
            i += 1;
        } else if !want_item {
            return None;
        } else if c == b'"' {
            let mut j = i + 1;
            while j < end && b[j] != b'"' {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let text: String = serde_json::from_slice(b.get(i..=j)?).ok()?;
            items.push(Cell::Text(text));
            i = j + 1;
            want_item = false;
        } else {
            let j = (i..end).find(|&j| matches!(b[j], b',' | b' ' | b'\t' | b'\r' | b'\n')).unwrap_or(end);
            let tok = std::str::from_utf8(&b[i..j]).ok()?;
            items.push(match tok {
                "true" => Cell::Bool(true),
                "false" => Cell::Bool(false),
                "null" => Cell::Null,
                t if t.bytes().all(|x| x.is_ascii_digit() || matches!(x, b'-' | b'+' | b'.' | b'e' | b'E')) => {
                    serde_json::from_str::<Value>(t).ok().filter(Value::is_number)?;
                    Cell::Decimal(t.to_string())
                }
                _ => return None,
            });
            i = j;
            want_item = false;
        }
    }
    (want_item == items.is_empty()).then_some(items)
}

/// A JSON number, string or boolean as its cell (numbers with their exact
/// digits); `None` for anything else.
fn json_scalar(s: &str) -> Option<Cell> {
    let t = s.trim();
    if plain_decimal(t).is_some() || (!t.is_empty() && t.bytes().all(|x| x.is_ascii_digit() || matches!(x, b'-' | b'+' | b'.' | b'e' | b'E'))) {
        // serde_json refuses `1e400`; it is still a number to check.
        if t.bytes().any(|x| x.is_ascii_digit()) {
            return Some(Cell::Decimal(t.to_string()));
        }
    }
    match serde_json::from_str::<Value>(s).ok()? {
        Value::Number(_) => Some(Cell::Decimal(t.to_string())),
        Value::String(t) => Some(Cell::Text(t)),
        Value::Bool(b) => Some(Cell::Bool(b)),
        _ => None,
    }
}

fn write_str(buf: &mut Vec<u8>, s: &str) {
    // Writing into a Vec can't fail.
    let _ = serde_json::to_writer(&mut *buf, s);
}

/// A JSON array of scalars (a multi-valued field's values); objects would
/// be read by Solr as child documents or atomic updates.
fn scalar_array(s: &str) -> Option<usize> {
    let t = s.trim_start();
    if !t.starts_with('[') {
        return None;
    }
    let a = serde_json::from_str::<Vec<Value>>(t).ok()?;
    a.iter().all(|v| !v.is_object() && !v.is_array()).then_some(a.len())
}

/// Plain decimal digits, normalised (`-012.50` → `-12.5`), to compare
/// numbers by their exact value.
fn norm_decimal(s: &str) -> String {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let int = int.trim_start_matches('0');
    let frac = frac.trim_end_matches('0');
    let mut out = if int.is_empty() { "0".to_string() } else { int.to_string() };
    if !frac.is_empty() {
        out.push('.');
        out.push_str(frac);
    }
    if neg && out != "0" {
        out.insert(0, '-');
    }
    out
}

fn plain_digits(s: &str) -> bool {
    let d = s.strip_prefix(['-', '+']).unwrap_or(s);
    let (i, f) = d.split_once('.').unwrap_or((d, ""));
    !(i.is_empty() && f.is_empty()) && i.bytes().all(|b| b.is_ascii_digit()) && f.bytes().all(|b| b.is_ascii_digit())
}

/// A decimal number as plain digits, the exponent applied (`1.25e2` →
/// `125`); `None` if it isn't a number.
pub fn plain_decimal(s: &str) -> Option<String> {
    let t = s.trim();
    let (m, e) = match t.find(['e', 'E']) {
        Some(i) => (&t[..i], t[i + 1..].parse::<i64>().ok()?),
        None => (t, 0),
    };
    if !plain_digits(m) || e.abs() > 4_000 {
        return None;
    }
    if e == 0 {
        return Some(m.to_string());
    }
    let (sign, m) = match m.strip_prefix('-') {
        Some(r) => ("-", r),
        None => ("", m.strip_prefix('+').unwrap_or(m)),
    };
    let (i, f) = m.split_once('.').unwrap_or((m, ""));
    let digits = format!("{i}{f}");
    let point = i.len() as i64 + e;
    let out = if point <= 0 {
        format!("0.{}{digits}", "0".repeat(point.unsigned_abs() as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    Some(format!("{sign}{out}"))
}

fn int_range(kind: Kind) -> (i128, i128) {
    if kind == Kind::Int32 {
        (i32::MIN as i128, i32::MAX as i128)
    } else {
        (i64::MIN as i128, i64::MAX as i128)
    }
}

/// `v` exactly as the float type of `kind` holds it (plain digits; a
/// negative zero as `-0.0`, since `-0` would be read as the integer 0), if
/// it holds it without loss; else why not.
fn exact_float(v: f64, kind: Kind, java: JavaText) -> std::result::Result<String, &'static str> {
    if !v.is_finite() {
        return Err("está fuera de rango");
    }
    let neg_zero = |s: String| if v == 0.0 && v.is_sign_negative() { "-0.0".to_string() } else { s };
    if kind != Kind::Float32 {
        return Ok(neg_zero(format!("{v}")));
    }
    let f = v as f32;
    if !f.is_finite() {
        return Err("está fuera de rango");
    }
    // Exactly a 32-bit value: stored as is.
    if f as f64 == v {
        return Ok(neg_zero(format!("{f}")));
    }
    // Or the decimal Solr prints that float as (Java's `Float.toString`,
    // what a read of a `pfloat` gives back): `0.1`. Not `1e-45`, which
    // Solr keeps as the float it prints as `1.4E-45`, nor `3.637979E-12`,
    // which Java 17 prints as `3.6379788E-12`.
    let Some(s) = java_float_on(f, java) else {
        return Err("Solr lo mostraría con otros dígitos según la versión de Java del servidor");
    };
    match s.parse::<f64>() {
        Ok(back) if back == v => Ok(plain_decimal(&s).unwrap_or(s)),
        _ => Err("tiene más dígitos de los que el tipo guarda"),
    }
}

/// Why a value doesn't fit its field.
fn misfit(col: &Col, value: &str, why: &str) -> Error {
    let ty = match col.kind {
        Kind::Int32 => "entero de 32 bits",
        Kind::Int64 => "entero de 64 bits",
        Kind::Float32 => "float de 32 bits",
        _ => "double de 64 bits",
    };
    Error::Query(format!(
        "El valor {value} no entra sin pérdida en el campo «{}» ({ty}) de Solr: {why}. Solr lo guardaría cambiado sin avisar, así que la carga se detiene.",
        col.name
    ))
}

/// A number for a numeric field, checked to fit it exactly (Solr would
/// truncate a fraction or wrap an overflowing integer without an error).
fn write_number(buf: &mut Vec<u8>, c: &Cell, col: &Col) -> Result<()> {
    let kind = col.kind;
    let int = |buf: &mut Vec<u8>, v: i128, shown: &str| -> Result<()> {
        let (lo, hi) = int_range(kind);
        if v < lo || v > hi {
            return Err(misfit(col, shown, "está fuera de rango"));
        }
        let _ = write!(buf, "{v}");
        Ok(())
    };
    let float = |buf: &mut Vec<u8>, v: f64, shown: &str| -> Result<()> {
        let s = exact_float(v, kind, col.java).map_err(|why| misfit(col, shown, why))?;
        buf.extend_from_slice(s.as_bytes());
        Ok(())
    };
    match c {
        Cell::Null => {
            buf.extend_from_slice(b"null");
            Ok(())
        }
        Cell::Int(i) if kind.is_int() => int(buf, *i as i128, &i.to_string()),
        Cell::UInt(u) if kind.is_int() => int(buf, *u as i128, &u.to_string()),
        Cell::Int(i) if kind.is_float() => {
            // Exact only if the float type holds the integer as is.
            let ok = if kind == Kind::Float32 { (*i as f32) as i128 == *i as i128 } else { (*i as f64) as i128 == *i as i128 };
            if !ok {
                return Err(misfit(col, &i.to_string(), "tiene más dígitos de los que el tipo guarda"));
            }
            let _ = write!(buf, "{i}");
            Ok(())
        }
        Cell::UInt(u) if kind.is_float() => {
            let ok = if kind == Kind::Float32 { (*u as f32) as u128 == *u as u128 } else { (*u as f64) as u128 == *u as u128 };
            if !ok {
                return Err(misfit(col, &u.to_string(), "tiene más dígitos de los que el tipo guarda"));
            }
            let _ = write!(buf, "{u}");
            Ok(())
        }
        Cell::Float(f) if kind.is_int() => {
            if !f.is_finite() || f.fract() != 0.0 {
                return Err(misfit(col, &f.to_string(), "tiene parte decimal"));
            }
            if f.abs() >= 1e38 {
                return Err(misfit(col, &f.to_string(), "está fuera de rango"));
            }
            int(buf, *f as i128, &f.to_string())
        }
        Cell::Float(f) => float(buf, *f, &f.to_string()),
        Cell::Decimal(s) if kind.is_int() && plain_decimal(s).is_some() => {
            let n = norm_decimal(&plain_decimal(s).unwrap_or_default());
            if n.contains('.') {
                return Err(misfit(col, s, "tiene parte decimal"));
            }
            match n.parse::<i128>() {
                Ok(v) => int(buf, v, s),
                Err(_) => Err(misfit(col, s, "está fuera de rango")),
            }
        }
        Cell::Decimal(s) if plain_decimal(s).is_some() => {
            let plain = plain_decimal(s).unwrap_or_default();
            let n = norm_decimal(&plain);
            // `-0.0` keeps its sign in a float field (norm_decimal drops it).
            if n == "0" && plain.trim_start().starts_with('-') {
                buf.extend_from_slice(b"-0.0");
                return Ok(());
            }
            let v: f64 = n.parse().map_err(|_| misfit(col, s, "no es un número"))?;
            let back = exact_float(v, kind, col.java).map_err(|why| misfit(col, s, why))?;
            if norm_decimal(&back) != n {
                return Err(misfit(col, s, "tiene más dígitos de los que el tipo guarda"));
            }
            // The decimal's own digits: they are the exact value.
            buf.extend_from_slice(n.as_bytes());
            Ok(())
        }
        // A decimal too large to spell out (`1e5000`): Solr would store an
        // infinity or wrap it.
        Cell::Decimal(s) => Err(misfit(col, s, "está fuera de rango")),
        // Text and anything else: Solr parses it and rejects what it can't.
        _ => {
            write_str(buf, c.to_json().as_str().map(str::to_string).unwrap_or_else(|| c.to_json().to_string()).as_str());
            Ok(())
        }
    }
}

/// Digits past milliseconds that aren't zero (Solr would drop them).
fn sub_millis(c: &Cell) -> bool {
    let s = match c {
        Cell::DateTime(s) => s.as_str(),
        Cell::DateTimeTz(s) => s.get(..s.len().saturating_sub(6)).unwrap_or_default(),
        _ => return false,
    };
    s.get(19..)
        .and_then(|f| f.strip_prefix('.'))
        .is_some_and(|f| f.len() > 3 && f.bytes().skip(3).any(|b| b != b'0'))
}

/// Digits past milliseconds in a Solr instant text (`…:05.123456Z`).
fn iso_sub_millis(s: &str) -> bool {
    s.get(19..)
        .and_then(|f| f.strip_prefix('.'))
        .map(|f| f.trim_end_matches(|c: char| !c.is_ascii_digit()))
        .is_some_and(|f| f.len() > 3 && f.bytes().skip(3).any(|b| b != b'0'))
}

fn too_precise(col: &Col, shown: &str) -> Error {
    Error::Unsupported(format!(
        "Solr guarda las fechas con precisión de milisegundos y «{}» trae {shown}: se perderían los dígitos de más.",
        col.name
    ))
}

/// A number into a field type DBine doesn't know: it can't check that the
/// value fits, so it's refused rather than risk a silent change.
fn unknown_numeric(col: &Col, shown: &str) -> Error {
    Error::Unsupported(format!(
        "el campo «{}» es de un tipo de Solr que DBine no conoce: no se puede verificar que el número {shown} se guarde sin cambios, así que no se carga.",
        col.name
    ))
}

/// A boolean for a `BoolField`, which reads anything not starting with
/// `1`, `t` or `T` as false (`"yes"` would be false): only clear values.
fn write_bool(buf: &mut Vec<u8>, c: &Cell, col: &Col) -> Result<()> {
    let v = match c {
        Cell::Bool(b) => Some(*b),
        Cell::Int(0) | Cell::UInt(0) => Some(false),
        Cell::Int(1) | Cell::UInt(1) => Some(true),
        Cell::Text(s) | Cell::Decimal(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Some(true),
            "false" | "f" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    };
    let Some(v) = v else {
        let shown = c.to_json().as_str().map(str::to_string).unwrap_or_else(|| c.to_json().to_string());
        return Err(Error::Query(format!(
            "El valor {shown} no es un booleano claro para el campo «{}» de Solr: lo guardaría como false sin avisar, así que la carga se detiene.",
            col.name
        )));
    };
    buf.extend_from_slice(if v { b"true" } else { b"false" });
    Ok(())
}

/// One value for the field `col`.
pub fn write_value(buf: &mut Vec<u8>, c: &Cell, col: &Col) -> Result<()> {
    let kind = col.kind;
    match c {
        Cell::Null => buf.extend_from_slice(b"null"),
        Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) if kind.is_int() || kind.is_float() => return write_number(buf, c, col),
        Cell::Decimal(_) if kind.is_int() || kind.is_float() => return write_number(buf, c, col),
        // Solr parses a numeric text into a float field rounding it
        // (`"0.1234567891234"` into a pfloat): checked like a number.
        Cell::Text(s) if (kind.is_int() || kind.is_float()) && plain_decimal(s).is_some() => {
            return write_number(buf, &Cell::Decimal(s.trim().to_string()), col)
        }
        // Any other text: Java's parser takes `1.5f`, `0x1p3`… rounding
        // them; only NaN and the infinities are left to it.
        Cell::Text(s) if (kind.is_int() || kind.is_float()) && !(kind.is_float() && matches!(s.trim(), "NaN" | "Infinity" | "-Infinity")) => {
            return Err(misfit(col, &format!("«{s}»"), "no es un número decimal"))
        }
        // A JSON scalar is its value: a number is checked like one (Solr
        // rounds `0.1234567891234` into a pfloat), a string or boolean
        // goes as that value.
        Cell::Json(s) if (kind.is_int() || kind.is_float() || kind == Kind::Bool) && json_scalar(s).is_some() => {
            return write_value(buf, &json_scalar(s).unwrap_or(Cell::Null), col)
        }
        Cell::Json(s) if kind == Kind::Other && matches!(json_scalar(s), Some(Cell::Decimal(_))) => {
            return Err(unknown_numeric(col, s.trim()))
        }
        Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) | Cell::Decimal(_) if kind == Kind::Other => {
            let shown = c.to_json().as_str().map(str::to_string).unwrap_or_else(|| c.to_json().to_string());
            return Err(unknown_numeric(col, &shown));
        }
        Cell::Bool(_) | Cell::Int(_) | Cell::UInt(_) | Cell::Text(_) | Cell::Decimal(_) | Cell::Float(_) if kind == Kind::Bool => {
            return write_bool(buf, c, col)
        }
        Cell::Bool(b) => buf.extend_from_slice(if *b { b"true" } else { b"false" }),
        Cell::Int(i) => {
            let _ = write!(buf, "{i}");
        }
        Cell::UInt(u) => {
            let _ = write!(buf, "{u}");
        }
        // Into a text field, as a string: sent as a number, Solr would
        // keep its Java text, which depends on the server's Java
        // (`1e-7` → `1.0E-7`, `2⁻²⁴` → `5.9604644775390625E-8` on Java 17).
        Cell::Float(_) if kind == Kind::Text => write_str(buf, &item_text(c)),
        Cell::Float(_) => {
            let _ = serde_json::to_writer(&mut *buf, &c.to_json());
        }
        Cell::Bytes(b) => write_str(buf, &b64_encode(b)),
        Cell::Date(_) | Cell::DateTime(_) | Cell::DateTimeTz(_) if matches!(kind, Kind::Date | Kind::Other) => {
            // `Other` too: a date-like type DBine doesn't know rounds the same.
            if sub_millis(c) {
                return Err(too_precise(col, c.to_json().as_str().unwrap_or_default()));
            }
            match cell_date_to_solr(c) {
                Some(d) => write_str(buf, &d),
                None => write_str(buf, c.to_json().as_str().unwrap_or_default()),
            }
        }
        Cell::Text(s) if kind == Kind::Date && iso_sub_millis(s) => return Err(too_precise(col, s)),
        Cell::Json(s) if col.multi && scalar_array(s).is_some() => {
            let items = array_items(s).unwrap_or_default();
            if items.is_empty() {
                return Err(Error::Unsupported(format!(
                    "Solr no guarda listas vacías: «{}» quedaría sin valor (NULL) en lugar de una lista vacía.",
                    col.name
                )));
            }
            // Each value checked like a single one (`[12.5]` into pints is
            // truncated just the same), numbers with their exact digits.
            buf.push(b'[');
            for (i, item) in items.iter().enumerate() {
                if i > 0 {
                    buf.push(b',');
                }
                match item {
                    // Solr drops a null item without a word (`[1,null,2]`
                    // reads back as `[1,2]`).
                    Cell::Null => {
                        return Err(Error::Unsupported(format!(
                            "Solr no guarda valores nulos dentro de una lista: en «{}» se descartarían y la lista quedaría más corta.",
                            col.name
                        )))
                    }
                    // A number into a text field: its own digits as text
                    // (Solr would print `2.50` back as `2.5`).
                    Cell::Decimal(d) if kind == Kind::Text => write_str(buf, d),
                    Cell::Text(t) if kind == Kind::Date && iso_sub_millis(t) => return Err(too_precise(col, t)),
                    // Numeric kinds and booleans: checked like one value
                    // (`"0.1234567891234"` into pfloats is rounded).
                    Cell::Text(t) if kind != Kind::Bool && !kind.is_int() && !kind.is_float() => write_str(buf, t),
                    other => write_value(buf, other, col)?,
                }
            }
            buf.push(b']');
        }
        Cell::Decimal(s) | Cell::Text(s) | Cell::Json(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => {
            write_str(buf, s)
        }
    }
    Ok(())
}

/// The items written for `c` (a multi-valued field's items, or the one
/// value) with their JSON tokens as written (numbers as [`Cell::Decimal`]
/// with their exact digits), and whether they went as a list.
fn written_items(c: &Cell, col: &Col) -> Result<(Vec<Cell>, bool)> {
    let mut buf = Vec::new();
    write_value(&mut buf, c, col)?;
    let text = String::from_utf8_lossy(&buf);
    let list = text.starts_with('[');
    let items = if list { array_items(&text) } else { array_items(&format!("[{text}]")) };
    Ok((items.unwrap_or_default(), list))
}

/// A written item as the text Solr keeps of it.
fn item_text(c: &Cell) -> String {
    match c {
        Cell::Text(s) | Cell::Decimal(s) => s.clone(),
        Cell::Bool(b) => b.to_string(),
        other => match other.to_json() {
            Value::String(s) => s,
            v => v.to_string(),
        },
    }
}

/// A value as the list of texts Solr keeps for it (a multi-valued field's
/// values, or the one value).
fn value_texts(c: &Cell, col: &Col) -> Result<Vec<String>> {
    Ok(written_items(c, col)?.0.iter().map(item_text).collect())
}

/// How a number goes in the JSON. Solr parses `100` as a Java `Long` and
/// `100.0` or `1e-7` as a `Double`, and a copyField copies that object as
/// its Java text (`100`, `100.0`, `1.0E-7`), while a float field stores
/// the same number either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Form {
    Long,
    Double,
}

/// The texts a copyField can copy from a written item, each with the form
/// the item must be written in for it (`None`: as written), and whether
/// the Java text of a double couldn't be known (see [`java_double_on`]).
fn copy_choices(item: &Cell, src: &Col, max: Option<usize>) -> (Vec<(Option<Form>, String)>, bool) {
    let Cell::Decimal(tok) = item else {
        // Solr cuts only string input, and counts UTF-16 units (half an
        // emoji is a lone surrogate, read back as U+FFFD).
        let text = match (item, max) {
            (Cell::Text(s), Some(m)) => String::from_utf16_lossy(&s.encode_utf16().take(m).collect::<Vec<u16>>()),
            _ => item_text(item),
        };
        return (vec![(None, text)], false);
    };
    let v: f64 = tok.parse().unwrap_or(f64::NAN);
    let integer = !tok.contains(['.', 'e', 'E']);
    let java = if v.is_finite() { java_double_on(v, src.java) } else { None };
    let known = java.is_some();
    let mut out = Vec::new();
    if src.kind.is_float() {
        // A float field's number can go either way (not `-0`: it would be
        // the integer 0, and the field would lose the sign).
        if v.fract() == 0.0 && v.abs() < 9.2e18 && !(v == 0.0 && v.is_sign_negative()) {
            out.push((Some(Form::Long), (v as i64).to_string()));
        }
        if let Some(s) = java {
            out.push((Some(Form::Double), s));
        }
        return (out, !known);
    }
    // Any other number goes as written. Past a long, the parser keeps the
    // digits as a string, which the update chain may still turn into a
    // number: not predictable.
    if integer {
        match tok.parse::<i64>() {
            Ok(i) => out.push((None, i.to_string())),
            Err(_) => return (out, true),
        }
    } else if let Some(s) = java {
        out.push((None, s));
    }
    (out, !integer && !known)
}

/// What a row's copyField destinations need: `skip[j]` when Solr fills
/// column `j` with exactly the row's value (it isn't written), and the form
/// each number of a float source must be written in for that.
struct CopyPlan {
    skip: Vec<bool>,
    forms: Vec<Vec<Option<Form>>>,
}

/// Checks every copyField destination of the row against what Solr copies
/// into it from the row's source columns. A destination with exactly that
/// value is left to Solr; one with any other value (or NULL, which Solr
/// would fill) is refused: Solr would replace it without a word.
fn copy_plan(cols: &[Col], row: &[Cell]) -> Result<CopyPlan> {
    let mut plan = CopyPlan { skip: vec![false; cols.len()], forms: vec![Vec::new(); cols.len()] };
    for (j, col) in cols.iter().enumerate() {
        if col.fed_by.is_empty() || SKIP.contains(&col.name.as_str()) {
            continue;
        }
        let sources: Vec<(usize, Option<usize>)> =
            col.fed_by.iter().copied().filter(|&(i, _)| row.get(i).is_some_and(|v| !matches!(v, Cell::Null))).collect();
        if sources.is_empty() {
            // Nothing to copy: the destination is written as it came.
            continue;
        }
        let names: Vec<&str> = col.fed_by.iter().filter_map(|&(i, _)| cols.get(i)).map(|c| c.name.as_str()).collect();
        let value = row.get(j).unwrap_or(&Cell::Null);
        if matches!(value, Cell::Null) {
            if col.hidden {
                // Not kept: it reads back as NULL anyway.
                continue;
            }
            return Err(Error::Unsupported(format!(
                "el campo «{}» viene sin valor (NULL) en la fila, pero se llena por copyField desde «{}»: Solr le pondría esa copia y dejaría de ser NULL. Dejá el campo fuera de la carga o quitá el copyField del esquema de destino.",
                col.name,
                names.join("», «")
            )));
        }
        let want = value_texts(value, col)?;
        let mut got = Vec::new();
        for (i, max) in sources {
            for (k, item) in written_items(&row[i], &cols[i])?.0.iter().enumerate() {
                let (choices, unsure) = copy_choices(item, &cols[i], max);
                got.push((i, k, choices, unsure));
            }
        }
        let mut unsure = false;
        let mut ok = want.len() == got.len();
        if ok {
            for (w, (i, k, choices, u)) in want.iter().zip(&got) {
                let Some((form, _)) = choices.iter().find(|(_, t)| t == w) else {
                    ok = false;
                    unsure = *u;
                    break;
                };
                let Some(form) = form else { continue };
                let forms = &mut plan.forms[*i];
                if forms.len() <= *k {
                    forms.resize(k + 1, None);
                }
                match forms[*k] {
                    // Another destination needs this number the other way.
                    Some(f) if f != *form => {
                        ok = false;
                        break;
                    }
                    _ => forms[*k] = Some(*form),
                }
            }
        }
        if ok {
            plan.skip[j] = true;
            continue;
        }
        let own = want.join(", ");
        return Err(Error::Unsupported(if unsure {
            format!(
                "el campo «{}» se llena por copyField desde «{}» y DBine no puede anticipar el texto exacto que Solr copiaría de ese número (depende de la versión de Java del servidor): no se carga para no arriesgar un cambio del valor propio de la fila ({own}).",
                col.name,
                names.join("», «")
            )
        } else {
            format!(
                "el campo «{}» se llena por copyField desde «{}»: Solr le pone esa copia y no puede guardar el valor propio de la fila ({own}), que se perdería.",
                col.name,
                names.join("», «")
            )
        }));
    }
    Ok(plan)
}

/// One value with the forms a copy needs for its numbers (the same
/// numbers, as `100` or `100.0`).
fn write_forms(buf: &mut Vec<u8>, c: &Cell, col: &Col, forms: &[Option<Form>]) -> Result<()> {
    let (items, list) = written_items(c, col)?;
    if list {
        buf.push(b'[');
    }
    for (k, item) in items.iter().enumerate() {
        if k > 0 {
            buf.push(b',');
        }
        let v = || match item {
            Cell::Decimal(t) => t.parse::<f64>().unwrap_or(f64::NAN),
            _ => f64::NAN,
        };
        match (item, forms.get(k).copied().flatten()) {
            (_, Some(Form::Long)) => {
                let _ = write!(buf, "{}", v() as i64);
            }
            (_, Some(Form::Double)) => buf.extend_from_slice(java_double(v()).as_bytes()),
            (Cell::Decimal(t), None) => buf.extend_from_slice(t.as_bytes()),
            (Cell::Text(t), None) => write_str(buf, t),
            (other, None) => {
                let _ = serde_json::to_writer(&mut *buf, &other.to_json());
            }
        }
    }
    if list {
        buf.push(b']');
    }
    Ok(())
}

/// One row as a document of the `/update` array (without separators).
pub fn encode_doc(buf: &mut Vec<u8>, cols: &[Col], row: &[Cell]) -> Result<()> {
    buf.push(b'{');
    encode_fields(buf, cols, row, true)?;
    buf.push(b'}');
    Ok(())
}

/// One row as a document that Solr only inserts: `_version_: -1` makes it
/// fail (409) if a document with its key already exists.
pub fn encode_new_doc(buf: &mut Vec<u8>, cols: &[Col], row: &[Cell]) -> Result<()> {
    buf.extend_from_slice(b"{\"_version_\":-1");
    encode_fields(buf, cols, row, false)?;
    buf.push(b'}');
    Ok(())
}

fn encode_fields(buf: &mut Vec<u8>, cols: &[Col], row: &[Cell], mut first: bool) -> Result<()> {
    let plan = if cols.iter().any(|c| !c.fed_by.is_empty()) { Some(copy_plan(cols, row)?) } else { None };
    for (j, (col, c)) in cols.iter().zip(row).enumerate() {
        if matches!(c, Cell::Null) || SKIP.contains(&col.name.as_str()) {
            continue;
        }
        // Solr fills it from the source field: writing it too would repeat
        // the values (or fail on a single-valued field).
        if plan.as_ref().is_some_and(|p| p.skip[j]) {
            continue;
        }
        if col.hidden {
            return Err(Error::Unsupported(format!(
                "el campo «{}» de la colección de destino no se guarda (stored=false y sin docValues): Solr aceptaría el valor y lo descartaría, así que después se leería como NULL. Cambiá el campo a stored=true o dejalo fuera de la carga.",
                col.name
            )));
        }
        if !first {
            buf.push(b',');
        }
        first = false;
        write_str(buf, &col.name);
        buf.push(b':');
        match plan.as_ref().map(|p| p.forms[j].as_slice()).filter(|f| f.iter().any(Option::is_some)) {
            Some(forms) => write_forms(buf, c, col, forms)?,
            None => write_value(buf, c, col)?,
        }
    }
    Ok(())
}

/// A key cell as the id text a delete takes.
fn key_text(c: &Cell) -> Option<String> {
    match c {
        Cell::Null => None,
        Cell::Text(s) | Cell::Uuid(s) | Cell::Decimal(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Json(s) => {
            Some(s.clone())
        }
        Cell::Bytes(b) => Some(b64_encode(b)),
        other => Some(other.to_json().to_string()),
    }
}

/// Why an `/update` request failed.
struct SendFail {
    err: Error,
    /// A key Solr refused because a document with it exists, on the first
    /// attempt: the keys were checked absent just before, so that document
    /// was written by someone else and must not be undone.
    foreign: Option<String>,
    /// Solr may have received the request and still be applying it (it
    /// timed out, or the connection broke after it was sent).
    unsure: bool,
}

type Sent = std::result::Result<u64, SendFail>;

/// The key in Solr's version-conflict message
/// (`version conflict for <key> expected=-1 actual=…`).
pub fn conflict_key(msg: &str) -> Option<String> {
    const HEAD: &str = "version conflict for ";
    let rest = &msg[msg.find(HEAD)? + HEAD.len()..];
    Some(rest[..rest.rfind(" expected=")?].to_string())
}

/// Send one `/update` request. It's sent again only when the server pushes
/// back (429, or 503 while a SolrCloud leader is elected) or the connection
/// couldn't be opened; never after a timeout, when Solr may still be
/// applying it.
async fn send_update(client: reqwest::Client, url: String, mut body: Vec<u8>, rows: u64) -> Sent {
    use dbine_driver_elasticsearch::http::net_err;
    body.push(b']');
    let rb = client.post(&url).header("Content-Type", "application/json").timeout(TIMEOUT).body(body);
    let fail = |err: Error, unsure: bool| SendFail { err, foreign: None, unsure };
    let mut wait = Duration::from_millis(500);
    let mut attempt = 0u32;
    loop {
        // A Vec body is reusable: the clone shares its bytes.
        let Some(try_rb) = rb.try_clone() else { return Err(fail(Error::State("cuerpo de /update no reutilizable".into()), false)) };
        let retry = attempt < 6;
        match try_rb.send().await {
            // Never reached the server: safe to send again.
            Err(e) if e.is_connect() && retry => {}
            Err(e) => return Err(fail(net_err(e), true)),
            Ok(resp) => {
                let status = resp.status().as_u16();
                let text = resp.text().await.map_err(|e| fail(net_err(e), true))?;
                if !(matches!(status, 429 | 503) && retry) {
                    if status < 400 {
                        return Ok(rows);
                    }
                    let msg = solr_error_message(status, &text);
                    if status == 409 {
                        let key = conflict_key(&msg);
                        return Err(SendFail {
                            err: Error::Query(format!(
                                "ya existe un documento con la clave «{}» en la colección: la carga solo agrega documentos y nunca escribe sobre los que ya están.",
                                key.as_deref().unwrap_or("?")
                            )),
                            foreign: if attempt == 0 { key } else { None },
                            unsure: false,
                        });
                    }
                    return Err(fail(Error::Query(format!("/update: {msg}")), false));
                }
            }
        }
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(15));
        attempt += 1;
    }
}

fn collection(name: &str, what: &str) -> Result<String> {
    let n = name.trim();
    if n.is_empty() {
        return Err(Error::Query(format!("Falta la colección de {what}.")));
    }
    Ok(n.to_string())
}

/// The load's state across windows.
#[derive(Default)]
struct Window {
    /// Keys of the window's rows: a repeated one is an error.
    seen: HashSet<String>,
    /// Keys of the documents this load sent since the last commit, each
    /// checked absent before sending (undone on failure).
    sent: Vec<String>,
    rows: u64,
    bytes: u64,
    /// Keys whose documents someone else wrote (never undone).
    foreign: HashSet<String>,
    /// A request may still be applied by Solr after the load returns.
    unsure: bool,
    /// A window was committed without opening a searcher (not visible
    /// until a later commit opens one).
    committed: bool,
}

impl Window {
    fn next(&mut self) {
        self.seen.clear();
        self.sent.clear();
        self.rows = 0;
        self.bytes = 0;
    }

    /// A finished request's outcome, noting what the undo needs to know.
    fn take(&mut self, r: Option<std::result::Result<Sent, tokio::task::JoinError>>) -> Result<u64> {
        match r {
            None => Ok(0),
            Some(Ok(Ok(n))) => Ok(n),
            Some(Ok(Err(f))) => {
                self.foreign.extend(f.foreign);
                self.unsure |= f.unsure;
                Err(f.err)
            }
            Some(Err(e)) => {
                self.unsure = true;
                Err(Error::State(format!("envío a /update interrumpido: {e}")))
            }
        }
    }
}

/// Where a load goes.
struct Target<'a> {
    collection: &'a str,
    key: &'a str,
    /// `…/update`.
    base: String,
}

impl SolrSession {
    async fn schema(&self, c: &str) -> Result<Schema> {
        let body = self.call(self.client.get(format!("{}/solr/{}/schema?wt=json", self.base, seg(c))).timeout(TIMEOUT)).await?;
        let v: Value = serde_json::from_str(&body).map_err(|e| Error::Query(format!("Respuesta inesperada del esquema: {e}")))?;
        Ok(Schema::parse(&v))
    }

    /// One `select` page, parsed as it arrives: each document becomes a row
    /// right away. Returns the documents read, the response's size and the
    /// next cursor.
    async fn read_page(
        &self,
        url: &str,
        params: &[(&str, &str)],
        kinds: &[(String, Kind)],
        builder: &mut BatchBuilder,
        sink: &BatchSinkRef,
    ) -> Result<(usize, usize, Option<String>)> {
        use dbine_driver_elasticsearch::http::net_err;
        let timeout = || Error::Connect("Tiempo de espera agotado leyendo de Solr.".into());
        // No overall limit (a page may be long): each wait is limited.
        let send = self.client.post(url).form(params).timeout(Duration::from_secs(24 * 3600)).send();
        let mut resp = tokio::time::timeout(TIMEOUT, send).await.map_err(|_| timeout())?.map_err(net_err)?;
        let status = resp.status().as_u16();
        if status >= 400 {
            let text = resp.text().await.map_err(net_err)?;
            return Err(Error::Query(solr_error_message(status, &text)));
        }
        let mut scan = DocScanner::default();
        let (mut n, mut bytes) = (0usize, 0usize);
        while let Some(chunk) = tokio::time::timeout(TIMEOUT, resp.chunk()).await.map_err(|_| timeout())?.map_err(net_err)? {
            bytes += chunk.len();
            let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
            scan.feed(&chunk, &mut |doc| {
                let d: Map<String, Value> =
                    serde_json::from_slice(doc).map_err(|e| Error::Query(format!("Respuesta inesperada de select: {e}")))?;
                n += 1;
                builder.push(doc_row(&d, kinds), &mut *s)?;
                Ok(())
            })?;
        }
        Ok((n, bytes, scan.cursor))
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let c = collection(&spec.table.name, "origen")?;
        let schema = self.schema(&c).await?;
        let Some(key) = schema.key.clone() else {
            return Err(Error::Unsupported(format!(
                "la colección {c} no tiene uniqueKey: Solr no puede recorrerla con cursorMark"
            )));
        };
        let names: Vec<String> = match &spec.columns {
            Some(cols) => {
                if let Some(n) = cols.iter().find(|n| schema.field(n).is_none()) {
                    return Err(Error::Query(format!("la colección {c} no tiene el campo «{n}»")));
                }
                cols.clone()
            }
            None => {
                // `columns` builds its URLs from the name as is.
                let o = ObjectRef { name: seg(&c), ..spec.table.clone() };
                self.columns(&o).await?.into_iter().map(|c| c.name).collect()
            }
        };
        let fields: Vec<Option<FieldType>> = names.iter().map(|n| schema.field(n)).collect();
        let cols: Vec<TransferColumn> = names
            .iter()
            .zip(&fields)
            .map(|(n, f)| TransferColumn {
                name: n.clone(),
                type_name: f.as_ref().map(|f| f.type_name.clone()).unwrap_or_default(),
                nullable: *n != key,
            })
            .collect();
        let kinds: Vec<(String, Kind)> = names
            .iter()
            .zip(&fields)
            .map(|(n, f)| (n.clone(), f.as_ref().map_or(Kind::Other, |f| if f.multi { Kind::Other } else { f.kind })))
            .collect();
        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;

        let url = format!("{}/solr/{}/select", self.base, seg(&c));
        // Names `fl` would misread: ask for every stored field and pick by name.
        let fl = if names.is_empty() {
            key.clone()
        } else if names.iter().all(|n| plain_fl_name(n)) {
            names.join(",")
        } else {
            "*".to_string()
        };
        let sort = format!("{key} asc");
        let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
        let mut cursor = "*".to_string();
        let mut page_rows = PAGE_FIRST;
        let mut builder = BatchBuilder::new();
        loop {
            let rows = page_rows.to_string();
            let mut params: Vec<(&str, &str)> = vec![
                ("q", "*:*"),
                ("fl", &fl),
                ("sort", &sort),
                ("rows", &rows),
                ("cursorMark", &cursor),
                ("wt", "json"),
                ("echoParams", "none"),
            ];
            if let Some(f) = filter {
                params.push(("fq", f));
            }
            let (n, bytes, next) = self.read_page(&url, &params, &kinds, &mut builder, &sink).await?;
            let next = next.unwrap_or_else(|| cursor.clone());
            if next == cursor || n == 0 {
                break;
            }
            cursor = next;
            page_rows = next_page(n, bytes);
        }
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
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden cargar datos.".into()));
        }
        let c = collection(&spec.table.name, "destino")?;
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let schema = self.schema(&c).await?;
        let Some(key) = schema.key.clone() else {
            return Err(Error::Unsupported(format!(
                "la colección {c} no tiene uniqueKey: una carga fallida no se podría deshacer ni reintentar sin duplicar documentos"
            )));
        };
        let Some(key_at) = names.iter().position(|n| *n == key) else {
            return Err(Error::Unsupported(format!(
                "la carga en {c} necesita la columna de clave «{key}»: sin ella una carga fallida no se podría deshacer ni reintentar sin duplicar documentos"
            )));
        };
        if schema.field("_version_").is_none() {
            return Err(Error::Unsupported(format!(
                "la colección {c} no tiene el campo _version_: Solr no puede asegurar que la carga solo agregue documentos sin reemplazar los que ya están"
            )));
        }
        let mut cols = load_cols(&schema, &names)?;
        let java = self.java_text().await;
        for col in &mut cols {
            col.java = java;
        }
        let t = Target { collection: &c, key: &key, base: format!("{}/solr/{}/update", self.base, seg(&c)) };

        let mut inflight: JoinSet<Sent> = JoinSet::new();
        let mut window = Window::default();
        let r = self.load_windows(spec, &cols, key_at, &t, source, progress, &mut inflight, &mut window).await;
        let Err(e) = r else { return r };
        // Nothing may land after returning: wait for what's in flight, then
        // delete the window's documents this load wrote, which autoCommit
        // would otherwise commit later.
        while let Some(r) = inflight.join_next().await {
            let _ = window.take(Some(r));
        }
        let mine: Vec<String> = window.sent.iter().filter(|k| !window.foreign.contains(*k)).cloned().collect();
        // Nothing to delete and no window committed: nothing to commit.
        let undo = if mine.is_empty() && !window.committed { Ok(()) } else { self.undo_window(&t.base, &mine).await };
        let e = match undo {
            Ok(()) => e,
            Err(u) if mine.is_empty() => Error::Query(format!(
                "{e} (además no se pudo hacer visible lo ya confirmado: {u})"
            )),
            Err(u) => Error::Query(format!(
                "{e} (además no se pudieron borrar los {} documentos sin confirmar de la tanda en curso: {u})",
                mine.len()
            )),
        };
        if !window.unsure {
            return Err(e);
        }
        // Solr can't cancel a request it's applying: say so.
        Err(Error::Query(format!(
            "{e} (Solr no respondió a un envío y puede que todavía lo esté aplicando: algunos de sus documentos podrían aparecer en {c} después de esta falla; revisá la colección antes de reintentar)"
        )))
    }

    /// How the server's Java prints numbers (see [`Col::java`]). Only
    /// Java 17 and 18 are taken as [`JavaText::Java17`] (the port was
    /// checked against 17); anything else unclear is unknown, where only the
    /// texts both kinds agree on are predicted: the worst outcome is a
    /// refusal, never a wrong guess.
    async fn java_text(&self) -> JavaText {
        let url = format!("{}/solr/admin/info/system?wt=json", self.base);
        let Ok(body) = self.call(self.client.get(url).timeout(Duration::from_secs(30))).await else { return JavaText::Unknown };
        let v: Value = serde_json::from_str(&body).unwrap_or(Value::Null);
        let spec = v.pointer("/jvm/spec/version").and_then(Value::as_str).unwrap_or_default();
        // `17`, `21`; `1.8` before Java 9.
        match spec.split('.').next().and_then(|m| m.parse::<u32>().ok()) {
            Some(m) if m >= 19 => JavaText::Java19,
            Some(17 | 18) => JavaText::Java17,
            _ => JavaText::Unknown,
        }
    }

    /// Delete the uncommitted window's documents by key, and commit that
    /// (the commit also opens a searcher: the windows committed before
    /// without one become visible, as after a load that ends well).
    async fn undo_window(&self, base: &str, keys: &[String]) -> Result<()> {
        for chunk in keys.chunks(DELETE_IDS) {
            let body = serde_json::json!({ "delete": chunk }).to_string();
            let rb = self.client.post(format!("{base}?wt=json")).header("Content-Type", "application/json").timeout(TIMEOUT).body(body);
            self.call(rb).await?;
        }
        self.call(self.client.get(format!("{base}?commit=true&wt=json")).timeout(TIMEOUT)).await?;
        Ok(())
    }

    /// The first of `keys` that already has a document (committed or not:
    /// a real-time get sees both).
    async fn first_existing(&self, t: &Target<'_>, keys: &[String]) -> Result<Option<String>> {
        let url = format!("{}/solr/{}/get", self.base, seg(t.collection));
        for chunk in keys.chunks(GET_IDS) {
            // `id` values are taken as is (`ids` would split them on commas).
            let mut params: Vec<(&str, &str)> = vec![("fl", t.key), ("wt", "json")];
            params.extend(chunk.iter().map(|k| ("id", k.as_str())));
            let body = self.call(self.client.post(&url).form(&params).timeout(TIMEOUT)).await?;
            let v: Value = serde_json::from_str(&body).map_err(|e| Error::Query(format!("Respuesta inesperada de get: {e}")))?;
            // One id answers `{"doc": …}`, several `{"response": {"docs": […]}}`.
            let doc = v
                .pointer("/response/docs/0")
                .or_else(|| v.get("doc"))
                .filter(|d| d.is_object())
                .and_then(|d| d.get(t.key));
            if let Some(k) = doc {
                return Ok(Some(key_text(&to_cell(k, Kind::Text)).unwrap_or_default()));
            }
        }
        Ok(None)
    }

    /// Check a request's keys are new, then send it.
    #[allow(clippy::too_many_arguments)]
    async fn dispatch(
        &self,
        t: &Target<'_>,
        body: Vec<u8>,
        keys: Vec<String>,
        rows: u64,
        inflight: &mut JoinSet<Sent>,
        w: &mut Window,
    ) -> Result<()> {
        if let Some(k) = self.first_existing(t, &keys).await? {
            return Err(Error::Query(format!(
                "ya existe un documento con la clave «{k}» en {}: la carga solo agrega documentos y nunca escribe sobre los que ya están (si la clave viene repetida en el origen, tampoco se puede cargar).",
                t.collection
            )));
        }
        if inflight.len() >= IN_FLIGHT {
            w.take(inflight.join_next().await)?;
        }
        w.sent.extend(keys);
        inflight.spawn(send_update(self.client.clone(), format!("{}?wt=json", t.base), body, rows));
        Ok(())
    }

    #[allow(clippy::too_many_arguments)]
    async fn load_windows(
        &self,
        spec: &LoadSpec,
        cols: &[Col],
        key_at: usize,
        t: &Target<'_>,
        source: &mut dyn BatchSource,
        progress: Progress<'_>,
        inflight: &mut JoinSet<Sent>,
        w: &mut Window,
    ) -> Result<u64> {
        let base = &t.base;
        let every_rows = spec.commit_rows.max(1);
        let every_bytes = spec.commit_bytes.max(1);
        let mut done = 0u64;
        let fresh = |hint: usize| {
            let mut b = Vec::with_capacity(hint.clamp(4096, REQUEST_BYTES + REQUEST_BYTES / 8));
            b.push(b'[');
            b
        };
        let mut buf = fresh(0);
        let mut keys: Vec<String> = Vec::new();
        let mut buffered = 0u64;
        loop {
            let batch = source.next().await;
            let finished = batch.is_none();
            for row in batch.map(|b| b.rows).unwrap_or_default() {
                if row.len() != cols.len() {
                    return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), cols.len())));
                }
                let Some(k) = key_text(&row[key_at]) else {
                    return Err(Error::Query(format!("una fila no tiene valor en la clave «{}»: Solr no la puede guardar.", t.key)));
                };
                if w.seen.contains(&k) {
                    return Err(Error::Query(format!(
                        "la clave «{k}» aparece más de una vez en los datos a cargar: Solr guardaría solo el último documento, y la carga nunca escribe sobre documentos ya cargados."
                    )));
                }
                let before = buf.len();
                if buffered > 0 {
                    buf.push(b',');
                }
                if let Err(e) = encode_new_doc(&mut buf, cols, &row) {
                    buf.truncate(before);
                    return Err(e);
                }
                w.seen.insert(k.clone());
                keys.push(k);
                buffered += 1;
                w.rows += 1;
                w.bytes += (buf.len() - before) as u64;
                let window_full = w.rows >= every_rows || w.bytes >= every_bytes;
                if buf.len() >= REQUEST_BYTES || window_full {
                    let hint = buf.len();
                    let body = std::mem::replace(&mut buf, fresh(hint));
                    self.dispatch(t, body, std::mem::take(&mut keys), buffered, inflight, w).await?;
                    buffered = 0;
                }
                if window_full {
                    // Everything sent is acknowledged, then committed: the
                    // committed rows are always a prefix of the input.
                    while let Some(r) = inflight.join_next().await {
                        w.take(Some(r))?;
                    }
                    self.call(self.client.get(format!("{base}?commit=true&openSearcher=false&wt=json")).timeout(TIMEOUT)).await?;
                    w.committed = true;
                    done += w.rows;
                    w.next();
                    progress(done);
                }
            }
            while let Some(r) = inflight.try_join_next() {
                w.take(Some(r))?;
            }
            if finished {
                break;
            }
        }
        if buffered > 0 {
            self.dispatch(t, std::mem::take(&mut buf), std::mem::take(&mut keys), buffered, inflight, w).await?;
        }
        while let Some(r) = inflight.join_next().await {
            w.take(Some(r))?;
        }
        // The last commit also opens a searcher: the documents are visible.
        self.call(self.client.get(format!("{base}?commit=true&wt=json")).timeout(TIMEOUT)).await?;
        if w.rows > 0 || done == 0 {
            done += w.rows;
            w.next();
            progress(done);
        }
        Ok(done)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn schema() -> Schema {
        Schema::parse(&json!({"schema": {
            "uniqueKey": "id",
            "fields": [
                {"name": "id", "type": "string"},
                {"name": "tags", "type": "string", "multiValued": true},
                {"name": "_version_", "type": "plong"}
            ],
            "dynamicFields": [
                {"name": "*_s", "type": "string"},
                {"name": "*_ss", "type": "strings"},
                {"name": "*_dt", "type": "pdate"},
                {"name": "*_i", "type": "pint"},
                {"name": "*_d", "type": "pdouble"},
                {"name": "*_bin", "type": "binary"},
                {"name": "attr_*", "type": "text_general", "multiValued": true}
            ],
            "fieldTypes": [
                {"name": "string", "class": "solr.StrField"},
                {"name": "strings", "class": "solr.StrField", "multiValued": true},
                {"name": "pdate", "class": "solr.DatePointField"},
                {"name": "pint", "class": "solr.IntPointField"},
                {"name": "plong", "class": "solr.LongPointField"},
                {"name": "pdouble", "class": "solr.DoublePointField"},
                {"name": "binary", "class": "solr.BinaryField"},
                {"name": "text_general", "class": "solr.TextField"}
            ]
        }}))
    }

    #[test]
    fn schema_resolves_fields_and_dynamic_patterns() {
        let s = schema();
        assert_eq!(s.key.as_deref(), Some("id"));
        assert_eq!(s.field("id"), Some(FieldType { type_name: "string".into(), kind: Kind::Text, multi: false }));
        assert_eq!(s.field("tags").unwrap().type_name, "string[]");
        // `*_ss` is longer than `*_s`: it wins.
        assert_eq!(s.field("x_ss"), Some(FieldType { type_name: "strings[]".into(), kind: Kind::Text, multi: true }));
        assert_eq!(s.field("when_dt").unwrap().kind, Kind::Date);
        assert_eq!(s.field("n_i").unwrap().kind, Kind::Int32);
        assert_eq!(s.field("b_bin").unwrap().kind, Kind::Binary);
        assert!(s.field("attr_color").unwrap().multi);
        assert_eq!(s.field("unknown"), None);
        assert!(dynamic_match("*", "x"));
        assert!(!dynamic_match("*_s", "_s"));
        assert_eq!(Kind::from_class("org.apache.solr.schema.UUIDField"), Kind::Uuid);
        assert_eq!(Kind::from_class("solr.BoolField"), Kind::Bool);
        assert_eq!(Kind::from_class("solr.LatLonPointSpatialField"), Kind::Other);
    }

    #[test]
    fn base64_round_trip() {
        for n in 0..70 {
            let data: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(b64_decode(&b64_encode(&data)).unwrap(), data);
        }
        assert_eq!(b64_encode(b"Man"), "TWFu");
        assert_eq!(b64_encode(&[1, 2, 3, 4]), "AQIDBA==");
        assert_eq!(b64_decode("AQID\nBA=="), Some(vec![1, 2, 3, 4]));
        assert_eq!(b64_decode("no válido"), None);
    }

    #[test]
    fn dates_both_ways() {
        assert_eq!(solr_date_to_cell("2024-01-02T03:04:05Z").as_deref(), Some("2024-01-02 03:04:05+00:00"));
        assert_eq!(solr_date_to_cell("2024-01-02T03:04:05.123Z").as_deref(), Some("2024-01-02 03:04:05.123+00:00"));
        assert_eq!(solr_date_to_cell("2024-01"), None);
        assert_eq!(cell_date_to_solr(&Cell::Date("2024-02-29".into())).as_deref(), Some("2024-02-29T00:00:00Z"));
        assert_eq!(cell_date_to_solr(&Cell::DateTime("2024-02-29 10:11:12.5".into())).as_deref(), Some("2024-02-29T10:11:12.5Z"));
        // Offsets move to UTC, across days, months and years.
        assert_eq!(
            cell_date_to_solr(&Cell::DateTimeTz("2024-01-01 01:30:00.250-03:00".into())).as_deref(),
            Some("2024-01-01T04:30:00.250Z")
        );
        assert_eq!(cell_date_to_solr(&Cell::DateTimeTz("2024-01-01 01:30:00+05:45".into())).as_deref(), Some("2023-12-31T19:45:00Z"));
        assert_eq!(cell_date_to_solr(&Cell::DateTimeTz("2024-02-28 23:00:00-02:00".into())).as_deref(), Some("2024-02-29T01:00:00Z"));
        assert_eq!(cell_date_to_solr(&Cell::DateTimeTz("2024-01-02 03:04:05+00:00".into())).as_deref(), Some("2024-01-02T03:04:05Z"));
        assert_eq!(cell_date_to_solr(&Cell::Text("x".into())), None);
        for d in [-1000, 0, 1, 59, 60, 365, 10_957, 19_723, 2_932_896] {
            let (y, m, dd) = civil_from_days(d);
            assert_eq!(days_from_civil(y, m, dd), d);
        }
    }

    #[test]
    fn docs_become_typed_rows() {
        let doc: Map<String, Value> = serde_json::from_value(json!({
            "id": "a1", "n_i": 5, "big_l": u64::MAX, "f_d": 2.0, "ok_b": true, "when_dt": "2024-01-02T03:04:05Z",
            "b_bin": "AQID", "u": "0f8fad5b-d9cb-469f-a165-70867728950e", "tags": ["x", "y"], "range_dt": "2024-01"
        }))
        .unwrap();
        let cols: Vec<(String, Kind)> = [
            ("id", Kind::Text),
            ("n_i", Kind::Int32),
            ("big_l", Kind::Int64),
            ("f_d", Kind::Float64),
            ("ok_b", Kind::Bool),
            ("when_dt", Kind::Date),
            ("b_bin", Kind::Binary),
            ("u", Kind::Uuid),
            ("tags", Kind::Other),
            ("range_dt", Kind::Date),
            ("missing", Kind::Int32),
        ]
        .iter()
        .map(|(n, k)| (n.to_string(), *k))
        .collect();
        assert_eq!(
            doc_row(&doc, &cols),
            vec![
                Cell::Text("a1".into()),
                Cell::Int(5),
                Cell::UInt(u64::MAX),
                Cell::Float(2.0),
                Cell::Bool(true),
                Cell::DateTimeTz("2024-01-02 03:04:05+00:00".into()),
                Cell::Bytes(vec![1, 2, 3]),
                Cell::Uuid("0f8fad5b-d9cb-469f-a165-70867728950e".into()),
                Cell::Json("[\"x\",\"y\"]".into()),
                Cell::Text("2024-01".into()),
                Cell::Null,
            ]
        );
    }

    fn cols(list: &[(&str, Kind, bool)]) -> Vec<Col> {
        list.iter().map(|(n, k, m)| Col::new(n, *k, *m)).collect()
    }

    fn encode(cols: &[Col], row: &[Cell]) -> Result<String> {
        let mut buf = Vec::new();
        encode_doc(&mut buf, cols, row)?;
        Ok(String::from_utf8(buf).unwrap())
    }

    #[test]
    fn rows_become_update_documents() {
        let cols = cols(&[
            ("id", Kind::Text, false),
            ("_version_", Kind::Int64, false),
            ("n_l", Kind::Int64, false),
            ("price_d", Kind::Float64, false),
            ("code_s", Kind::Text, false),
            ("when_dt", Kind::Date, false),
            ("day_s", Kind::Text, false),
            ("b_bin", Kind::Binary, false),
            ("tags", Kind::Text, true),
            ("meta_s", Kind::Text, false),
            ("gone", Kind::Text, false),
            ("f_d", Kind::Float64, false),
            ("new_field", Kind::Other, true),
            ("list_s", Kind::Text, false),
        ]);
        let row = vec![
            Cell::Text("a \"1\"".into()),
            Cell::Int(99),
            Cell::Int(i64::MAX),
            Cell::Decimal("12.50".into()),
            Cell::Decimal("0012".into()),
            Cell::DateTimeTz("2024-01-01 01:30:00-03:00".into()),
            Cell::Date("2024-01-01".into()),
            Cell::Bytes(vec![0, 255]),
            Cell::Json("[\"x\", 2]".into()),
            Cell::Json("{\"k\":1}".into()),
            Cell::Null,
            Cell::Float(1.5),
            Cell::DateTime("2024-05-06 07:08:09".into()),
            Cell::Json("[1,2]".into()),
        ];
        let text = encode(&cols, &row).unwrap();
        let doc: Value = serde_json::from_str(&text).unwrap();
        assert_eq!(
            doc,
            json!({
                "id": "a \"1\"", "n_l": i64::MAX, "price_d": 12.5, "code_s": "0012",
                "when_dt": "2024-01-01T04:30:00Z", "day_s": "2024-01-01", "b_bin": "AP8=",
                "tags": ["x", "2"], "meta_s": "{\"k\":1}", "f_d": 1.5, "new_field": "2024-05-06T07:08:09Z",
                // A list into a single-valued text field is its JSON text.
                "list_s": "[1,2]"
            })
        );
        // Arrays of objects would be child documents: they go as text.
        let a = encode(&cols_one("a_s", Kind::Text, true), &[Cell::Json("[{\"x\":1}]".into())]).unwrap();
        assert_eq!(a, r#"{"a_s":"[{\"x\":1}]"}"#);
    }

    fn cols_one(n: &str, k: Kind, multi: bool) -> Vec<Col> {
        vec![Col::new(n, k, multi)]
    }

    fn one(k: Kind, c: Cell) -> Result<String> {
        encode(&cols_one("f", k, false), &[c])
    }

    /// Solr truncates `12.50` into a pint to 12 and wraps 3000000000 to
    /// -1294967296 with status 0: such values are errors, never written.
    #[test]
    fn numbers_that_dont_fit_are_errors() {
        // Fractions into integer fields.
        assert!(one(Kind::Int32, Cell::Decimal("12.50".into())).is_err());
        assert!(one(Kind::Int32, Cell::Decimal("-7.99".into())).is_err());
        assert!(one(Kind::Int64, Cell::Float(1.9)).is_err());
        assert!(one(Kind::Int64, Cell::Float(f64::NAN)).is_err());
        // Out of range: int32 vs int64.
        assert!(one(Kind::Int32, Cell::Int(3_000_000_000)).is_err());
        assert!(one(Kind::Int32, Cell::UInt(3_000_000_000)).is_err());
        assert!(one(Kind::Int64, Cell::UInt(u64::MAX)).is_err());
        assert!(one(Kind::Int64, Cell::Decimal("9223372036854775808".into())).is_err());
        assert!(one(Kind::Int32, Cell::Float(3e9)).is_err());
        // Digits a float type can't hold.
        assert!(one(Kind::Float64, Cell::Int(i64::MAX)).is_err());
        assert!(one(Kind::Float64, Cell::Decimal("0.12345678901234567890".into())).is_err());
        assert!(one(Kind::Float32, Cell::Float(0.1234567891234)).is_err());
        assert!(one(Kind::Float32, Cell::Int(16_777_217)).is_err());
        // What fits is written as the exact number.
        assert_eq!(one(Kind::Int32, Cell::Int(i32::MAX as i64)).unwrap(), format!("{{\"f\":{}}}", i32::MAX));
        assert_eq!(one(Kind::Int32, Cell::Decimal("-12.000".into())).unwrap(), r#"{"f":-12}"#);
        assert_eq!(one(Kind::Int64, Cell::Float(-4.0)).unwrap(), r#"{"f":-4}"#);
        assert_eq!(one(Kind::Int64, Cell::UInt(i64::MAX as u64)).unwrap(), format!("{{\"f\":{}}}", i64::MAX));
        assert_eq!(one(Kind::Float64, Cell::Decimal("12.50".into())).unwrap(), r#"{"f":12.5}"#);
        assert_eq!(one(Kind::Float64, Cell::Int(1 << 53)).unwrap(), format!("{{\"f\":{}}}", 1u64 << 53));
        assert_eq!(one(Kind::Float32, Cell::Float(0.1f32 as f64)).unwrap(), r#"{"f":0.1}"#);
        assert_eq!(one(Kind::Float32, Cell::Decimal("0.1".into())).unwrap(), r#"{"f":0.1}"#);
        assert_eq!(one(Kind::Float64, Cell::Float(0.1)).unwrap(), r#"{"f":0.1}"#);
        // The pint/plong split comes from the class.
        assert_eq!(Kind::from_class("solr.IntPointField"), Kind::Int32);
        assert_eq!(Kind::from_class("solr.LongPointField"), Kind::Int64);
        assert_eq!(Kind::from_class("solr.FloatPointField"), Kind::Float32);
        assert_eq!(Kind::from_class("solr.TrieDoubleField"), Kind::Float64);
        assert_eq!(norm_decimal("-00.500"), "-0.5");
        assert_eq!(norm_decimal("-0.000"), "0");
    }

    #[test]
    fn multi_valued_numbers_are_checked_one_by_one() {
        let ints = cols_one("nums_is", Kind::Int32, true);
        assert_eq!(encode(&ints, &[Cell::Json("[1, -2]".into())]).unwrap(), r#"{"nums_is":[1,-2]}"#);
        assert!(encode(&ints, &[Cell::Json("[1, 12.5]".into())]).is_err());
        assert!(encode(&ints, &[Cell::Json("[3000000000]".into())]).is_err());
        let floats = cols_one("xs_fs", Kind::Float32, true);
        assert_eq!(encode(&floats, &[Cell::Json("[0.1, 2]".into())]).unwrap(), r#"{"xs_fs":[0.1,2]}"#);
    }

    #[test]
    fn engine_limits_are_refused() {
        // An empty list would read back as NULL.
        let e = encode(&cols_one("tags", Kind::Text, true), &[Cell::Json("[]".into())]).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        // Digits past milliseconds would be dropped…
        let e = one(Kind::Date, Cell::DateTime("2024-01-02 03:04:05.123456".into())).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        assert!(one(Kind::Date, Cell::DateTimeTz("2024-01-02 03:04:05.1234+01:00".into())).is_err());
        // …unless they are zeros.
        assert_eq!(one(Kind::Date, Cell::DateTime("2024-01-02 03:04:05.123000".into())).unwrap(), r#"{"f":"2024-01-02T03:04:05.123000Z"}"#);
        assert!(one(Kind::Date, Cell::DateTime("2024-01-02 03:04:05.12".into())).is_ok());
    }

    #[test]
    fn copy_field_destinations_are_left_to_solr() {
        let s = Schema::parse(&json!({"schema": {
            "uniqueKey": "id",
            "fields": [
                {"name": "id", "type": "string"},
                {"name": "title", "type": "string"},
                {"name": "all", "type": "strings"},
                {"name": "title_sort", "type": "string"}
            ],
            "dynamicFields": [{"name": "*_t", "type": "string"}, {"name": "*_s", "type": "string"}],
            "fieldTypes": [
                {"name": "string", "class": "solr.StrField"},
                {"name": "strings", "class": "solr.StrField", "multiValued": true}
            ],
            "copyFields": [
                {"source": "title", "dest": "all"},
                {"source": "title", "dest": "title_sort"},
                {"source": "*_t", "dest": "*_s"},
                {"source": "*_t", "dest": "all"}
            ]
        }}));
        assert_eq!(s.copy_dests("title"), vec!["all".to_string(), "title_sort".into()]);
        assert_eq!(s.copy_dests("name_t"), vec!["name_s".to_string(), "all".into()]);
        assert!(s.copy_dests("id").is_empty());
        let names: Vec<String> = ["id", "title", "all", "title_sort", "name_t", "name_s"].iter().map(|n| n.to_string()).collect();
        let cols = load_cols(&s, &names).unwrap();
        assert_eq!(cols[2].fed_by, vec![(1, None), (4, None)]);
        assert_eq!(cols[3].fed_by, vec![(1, None)]);
        assert_eq!(cols[5].fed_by, vec![(4, None)]);
        let row = |title: Cell, name: Cell| {
            vec![
                Cell::Text("1".into()),
                title,
                Cell::Json("[\"T\",\"N\"]".into()),
                Cell::Text("T".into()),
                name,
                Cell::Text("N".into()),
            ]
        };
        // The sources have values and the destinations hold exactly their
        // copies: Solr fills the destinations.
        let doc = encode(&cols, &row(Cell::Text("T".into()), Cell::Text("N".into()))).unwrap();
        assert_eq!(doc, r#"{"id":"1","title":"T","name_t":"N"}"#);
        // No source value: the destination is written as it came.
        let doc = encode(&cols, &row(Cell::Null, Cell::Null)).unwrap();
        assert_eq!(doc, r#"{"id":"1","all":["T","N"],"title_sort":"T","name_s":"N"}"#);
    }

    /// A copyField destination with its own value (PG → Solr, where a
    /// column happens to match a copyField) would be replaced by the copy:
    /// refused, never dropped.
    #[test]
    fn copy_field_destination_with_its_own_value_is_refused() {
        let s = Schema::parse(&json!({"schema": {
            "uniqueKey": "id",
            "fields": [{"name": "id", "type": "string"}],
            "dynamicFields": [{"name": "*_t", "type": "string"}, {"name": "*_s", "type": "string"}, {"name": "*_ss", "type": "strings"}],
            "fieldTypes": [
                {"name": "string", "class": "solr.StrField"},
                {"name": "strings", "class": "solr.StrField", "multiValued": true}
            ],
            "copyFields": [
                {"source": "title_t", "dest": "alt_s"},
                {"source": "title_t", "dest": "short_ss", "maxChars": 3}
            ]
        }}));
        let names: Vec<String> = ["id", "title_t", "alt_s"].iter().map(|n| n.to_string()).collect();
        let cols = load_cols(&s, &names).unwrap();
        let t = |v: &str| Cell::Text(v.into());
        let e = encode(&cols, &[t("1"), t("A"), t("B-distinto")]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("alt_s") && m.contains("B-distinto")), "{e:?}");
        assert_eq!(encode(&cols, &[t("1"), t("A"), t("A")]).unwrap(), r#"{"id":"1","title_t":"A"}"#);
        // maxChars: the copy keeps the first 3 characters.
        let names: Vec<String> = ["id", "title_t", "short_ss"].iter().map(|n| n.to_string()).collect();
        let cols = load_cols(&s, &names).unwrap();
        assert_eq!(cols[2].fed_by, vec![(1, Some(3))]);
        assert!(encode(&cols, &[t("1"), t("Hola"), Cell::Json("[\"Hol\"]".into())]).is_ok());
        assert!(encode(&cols, &[t("1"), t("Hola"), Cell::Json("[\"Hola\"]".into())]).is_err());
    }

    /// A field the schema doesn't define would get its type guessed by a
    /// schemaless collection (`5` → plongs, then `12.75` → 12): refused.
    #[test]
    fn unknown_fields_and_types_are_refused() {
        let s = schema();
        let names: Vec<String> = ["id", "precio_x"].iter().map(|n| n.to_string()).collect();
        assert!(matches!(load_cols(&s, &names), Err(Error::Unsupported(_))));
        // `_version_`/`score` are never written: not an error.
        let names: Vec<String> = ["id", "score"].iter().map(|n| n.to_string()).collect();
        assert!(load_cols(&s, &names).is_ok());
        // Numbers into a type DBine doesn't know: refused.
        for c in [Cell::Int(5), Cell::Float(12.75), Cell::Decimal("1.5".into())] {
            assert!(matches!(one(Kind::Other, c), Err(Error::Unsupported(_))));
        }
        let e = encode(&cols_one("v", Kind::Other, true), &[Cell::Json("[1, 2]".into())]).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
        // Dates into it lose no digits silently either.
        assert!(matches!(one(Kind::Other, Cell::DateTime("2024-01-02 03:04:05.123456".into())), Err(Error::Unsupported(_))));
        assert!(one(Kind::Other, Cell::DateTime("2024-01-02 03:04:05.123".into())).is_ok());
        assert!(matches!(one(Kind::Date, Cell::Text("2024-01-02T03:04:05.1234Z".into())), Err(Error::Unsupported(_))));
        // Booleans: only clear values (Solr stores "yes" or 5 as false).
        assert_eq!(one(Kind::Bool, Cell::Int(1)).unwrap(), r#"{"f":true}"#);
        assert_eq!(one(Kind::Bool, Cell::Text("F".into())).unwrap(), r#"{"f":false}"#);
        assert!(one(Kind::Bool, Cell::Int(5)).is_err());
        assert!(one(Kind::Bool, Cell::Text("yes".into())).is_err());
        // Numeric text into a float field is checked like a number.
        assert!(one(Kind::Float32, Cell::Text("0.1234567891234".into())).is_err());
        assert_eq!(one(Kind::Float64, Cell::Text("12.50".into())).unwrap(), r#"{"f":12.5}"#);
    }

    /// Items of a multi-valued numeric field keep their exact digits: a
    /// value a `pdoubles` can't hold is an error, as it is for one value.
    #[test]
    fn multi_valued_items_keep_their_digits() {
        let doubles = cols_one("xs_ds", Kind::Float64, true);
        assert!(encode(&doubles, &[Cell::Json("[0.12345678901234567890]".into())]).is_err());
        assert!(one(Kind::Float64, Cell::Decimal("0.12345678901234567890".into())).is_err());
        assert_eq!(encode(&doubles, &[Cell::Json("[1.25e2, -0.5]".into())]).unwrap(), r#"{"xs_ds":[125,-0.5]}"#);
        let ints = cols_one("xs_is", Kind::Int64, true);
        assert!(encode(&ints, &[Cell::Json("[1e0, 12.5e-1]".into())]).is_err());
        // Into text: the number's own digits.
        let texts = cols_one("xs_ss", Kind::Text, true);
        assert_eq!(encode(&texts, &[Cell::Json("[2.50, \"a\\\"b\", true]".into())]).unwrap(), r#"{"xs_ss":["2.50","a\"b",true]}"#);
        assert_eq!(array_items("[ ]"), Some(vec![]));
        assert_eq!(array_items("[1,]"), None);
        assert_eq!(array_items("[1 2]"), None);
        assert_eq!(array_items("[\"x,y\", null]"), Some(vec![Cell::Text("x,y".into()), Cell::Null]));
        assert_eq!(plain_decimal("-1.5E3").as_deref(), Some("-1500"));
        assert_eq!(plain_decimal("12e-4").as_deref(), Some("0.0012"));
        assert_eq!(plain_decimal("x"), None);
    }

    /// Numeric text in a list and JSON scalars are checked like numbers:
    /// Solr rounds `"0.1234567891234"` into pfloats without an error.
    #[test]
    fn numeric_text_and_json_scalars_are_checked() {
        let fs = cols_one("m_fs", Kind::Float32, true);
        assert!(encode(&fs, &[Cell::Json("[\"0.1234567891234\"]".into())]).is_err());
        assert_eq!(encode(&fs, &[Cell::Json("[\"0.5\", 2]".into())]).unwrap(), r#"{"m_fs":[0.5,2]}"#);
        let ds = cols_one("m_ds", Kind::Float64, true);
        assert!(encode(&ds, &[Cell::Json("[\"0.12345678901234567890\"]".into())]).is_err());
        assert!(encode(&cols_one("m_is", Kind::Int32, true), &[Cell::Json("[\"12.5\"]".into())]).is_err());
        assert!(encode(&fs, &[Cell::Json("[\"1.5f\"]".into())]).is_err());
        // One JSON scalar into a single-valued field.
        assert!(one(Kind::Float32, Cell::Json("0.1234567891234".into())).is_err());
        assert!(one(Kind::Float32, Cell::Json("\"0.1234567891234\"".into())).is_err());
        assert!(one(Kind::Float64, Cell::Json("1e400".into())).is_err());
        assert!(one(Kind::Int32, Cell::Json("3000000000".into())).is_err());
        assert_eq!(one(Kind::Float32, Cell::Json(" 0.5 ".into())).unwrap(), r#"{"f":0.5}"#);
        assert_eq!(one(Kind::Int64, Cell::Json("\"-12\"".into())).unwrap(), r#"{"f":-12}"#);
        assert_eq!(one(Kind::Bool, Cell::Json("true".into())).unwrap(), r#"{"f":true}"#);
        assert!(one(Kind::Bool, Cell::Json("\"yes\"".into())).is_err());
        assert!(matches!(one(Kind::Other, Cell::Json("1.5".into())), Err(Error::Unsupported(_))));
        // Text that isn't a plain decimal into a number field.
        assert!(one(Kind::Float32, Cell::Text("1.5f".into())).is_err());
        assert_eq!(one(Kind::Float64, Cell::Text("NaN".into())).unwrap(), r#"{"f":"NaN"}"#);
        // Text fields keep the JSON text as before.
        assert_eq!(one(Kind::Text, Cell::Json("1.50".into())).unwrap(), r#"{"f":"1.50"}"#);
    }

    /// Solr drops null items of a list (`[1,null,2]` reads back `[1,2]`).
    #[test]
    fn null_items_in_lists_are_refused() {
        let is = cols_one("m_is", Kind::Int32, true);
        for v in ["[1,null,2]", "[null]"] {
            let e = encode(&is, &[Cell::Json(v.into())]).unwrap_err();
            assert!(matches!(e, Error::Unsupported(_)), "{v}: {e:?}");
        }
        let e = encode(&cols_one("t_ss", Kind::Text, true), &[Cell::Json("[\"a\", null]".into())]).unwrap_err();
        assert!(matches!(e, Error::Unsupported(_)), "{e:?}");
    }

    /// A field neither stored nor docValues (`ignored_*`) drops the value.
    #[test]
    fn fields_that_keep_nothing_are_refused() {
        let s = Schema::parse(&json!({"schema": {
            "version": 1.7,
            "uniqueKey": "id",
            "fields": [
                {"name": "id", "type": "string"},
                {"name": "_text_", "type": "text_general", "multiValued": true, "stored": false},
                {"name": "dv_s", "type": "string", "stored": false},
                {"name": "nodv_s", "type": "string", "stored": false, "docValues": false},
                {"name": "hide_s", "type": "string", "stored": false, "useDocValuesAsStored": false}
            ],
            "dynamicFields": [{"name": "ignored_*", "type": "ignored"}, {"name": "*_t", "type": "text_general"}],
            "fieldTypes": [
                {"name": "string", "class": "solr.StrField"},
                {"name": "text_general", "class": "solr.TextField"},
                {"name": "ignored", "class": "solr.StrField", "indexed": false, "stored": false, "docValues": false, "multiValued": true}
            ]
        }}));
        assert!(s.retrievable("id"));
        assert!(s.retrievable("x_t"));
        assert!(s.retrievable("dv_s"));
        for n in ["ignored_note", "_text_", "nodv_s", "hide_s"] {
            assert!(!s.retrievable(n), "{n}");
        }
        let names: Vec<String> = ["id", "ignored_note"].iter().map(|n| n.to_string()).collect();
        let cols = load_cols(&s, &names).unwrap();
        let e = encode(&cols, &[Cell::Text("1".into()), Cell::Text("nota".into())]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("ignored_note")), "{e:?}");
        // No value: nothing is lost.
        assert_eq!(encode(&cols, &[Cell::Text("1".into()), Cell::Null]).unwrap(), r#"{"id":"1"}"#);
    }

    /// maxChars counts UTF-16 units and cuts only strings.
    #[test]
    fn copy_field_max_chars_counts_utf16_units() {
        let s = Schema::parse(&json!({"schema": {
            "uniqueKey": "id",
            "fields": [{"name": "id", "type": "string"}],
            "dynamicFields": [{"name": "*_t", "type": "string"}, {"name": "*_s", "type": "string"}, {"name": "*_l", "type": "plong"}],
            "fieldTypes": [{"name": "string", "class": "solr.StrField"}, {"name": "plong", "class": "solr.LongPointField"}],
            "copyFields": [{"source": "emo_t", "dest": "emo_s", "maxChars": 1}, {"source": "n_l", "dest": "n_s", "maxChars": 1}]
        }}));
        let names: Vec<String> = ["id", "emo_t", "emo_s"].iter().map(|n| n.to_string()).collect();
        let cols = load_cols(&s, &names).unwrap();
        let t = |v: &str| Cell::Text(v.into());
        // Solr keeps half the emoji: not what the row holds.
        assert!(matches!(encode(&cols, &[t("1"), t("😀😀"), t("😀")]), Err(Error::Unsupported(_))));
        assert_eq!(encode(&cols, &[t("1"), t("ab"), t("a")]).unwrap(), r#"{"id":"1","emo_t":"ab"}"#);
        // A number isn't cut.
        let names: Vec<String> = ["id", "n_l", "n_s"].iter().map(|n| n.to_string()).collect();
        let cols = load_cols(&s, &names).unwrap();
        assert_eq!(encode(&cols, &[t("1"), Cell::Int(123), t("123")]).unwrap(), r#"{"id":"1","n_l":123}"#);
    }

    /// Java's `Double.toString`/`Float.toString`, as Java 17 and 21 print
    /// them (checked against both on millions of values).
    #[test]
    fn java_number_texts() {
        for (v, s) in [
            (100.0, "100.0"),
            (1e-7, "1.0E-7"),
            (2.5, "2.5"),
            (0.001, "0.001"),
            (9999999.0, "9999999.0"),
            (1e7, "1.0E7"),
            (0.0, "0.0"),
            (-0.0, "-0.0"),
            (-1.5e-5, "-1.5E-5"),
            (0.12345678901234568, "0.12345678901234568"),
            (f64::MIN_POSITIVE / 4.0, "5.562684646268003E-309"),
            (5e-324, "4.9E-324"),
            // A tie between two shortest texts goes to the even digit.
            (f64::from_bits(0xc2dafff288c14b88), "-1.1874635212523012E14"),
        ] {
            assert_eq!(java_double(v), s, "{v:e}");
        }
        for (f, s) in [(0.1f32, "0.1"), (16777216.0, "1.6777216E7"), (f32::from_bits(0xc9800b12), "-1048930.2"), (1.4e-45, "1.4E-45"), (1e-3, "0.001")] {
            assert_eq!(java_float(f), s, "{f:e}");
        }
        // Java 17 prints some with other digits: as Java 17 does (checked
        // against it), and not predicted when the version is unknown.
        for (v, s) in [
            (1e23, "9.999999999999999E22"),
            (2f64.powi(-24), "5.9604644775390625E-8"),
            (2f64.powi(-31), "4.6566128730773926E-10"),
            (2f64.powi(-44), "5.6843418860808015E-14"),
            (6.310887241768095e-30, "6.3108872417680944E-30"),
            (100.0, "100.0"),
            (1e-7, "1.0E-7"),
            (-0.001, "-0.001"),
            (4.9e-324, "4.9E-324"),
            (1.2345678901234567e14, "1.2345678901234567E14"),
        ] {
            assert_eq!(java17_double(v), s, "{v:e}");
            assert_eq!(java_double_on(v, JavaText::Java17).as_deref(), Some(s));
            assert_eq!(java_double_on(v, JavaText::Java19), Some(java_double(v)));
            let same = java_double(v) == s;
            assert_eq!(java_double_on(v, JavaText::Unknown).is_some(), same, "{v:e}");
        }
        for (f, s) in [
            (2f32.powi(-38), "3.6379788E-12"),
            (1.2621775e-29, "1.26217745E-29"),
            (1.0918476e9, "1.09184755E9"),
            (1.4e-45, "1.4E-45"),
            (0.1, "0.1"),
            (16777216.0, "1.6777216E7"),
        ] {
            assert_eq!(java17_float(f), s, "{f:e}");
            assert_eq!(java_float_on(f, JavaText::Java17).as_deref(), Some(s));
            assert_eq!(java_float_on(f, JavaText::Unknown).is_some(), java_float(f) == s, "{f:e}");
        }
    }

    fn copy_schema() -> Schema {
        Schema::parse(&json!({"schema": {
            "uniqueKey": "id",
            "fields": [{"name": "id", "type": "string"}],
            "dynamicFields": [
                {"name": "*_d", "type": "pdouble"}, {"name": "*_ds", "type": "pdoubles"},
                {"name": "*_s", "type": "string"}, {"name": "*_ss", "type": "strings"}, {"name": "*_t", "type": "string"}
            ],
            "fieldTypes": [
                {"name": "string", "class": "solr.StrField"},
                {"name": "strings", "class": "solr.StrField", "multiValued": true},
                {"name": "pdouble", "class": "solr.DoublePointField"},
                {"name": "pdoubles", "class": "solr.DoublePointField", "multiValued": true}
            ],
            "copyFields": [
                {"source": "px_d", "dest": "px_s"}, {"source": "px_d", "dest": "px2_s"},
                {"source": "xs_ds", "dest": "xs_ss"}, {"source": "title_t", "dest": "alt_s"}
            ]
        }}))
    }

    fn load_names(s: &Schema, names: &[&str]) -> Vec<Col> {
        load_cols(s, &names.iter().map(|n| n.to_string()).collect::<Vec<_>>()).unwrap()
    }

    /// X1a/X1: Solr copies a double as Java prints it (`1.0E-7`, `100.0`,
    /// or `100` when it came as an integer): a row whose copy destination
    /// holds another text is refused, and one holding Solr's own copy (a
    /// Solr-to-Solr load) is loaded with the source written so that Solr
    /// copies exactly that.
    #[test]
    fn copies_of_doubles_follow_java() {
        let s = copy_schema();
        let cols = load_names(&s, &["id", "px_d", "px_s"]);
        let t = |v: &str| Cell::Text(v.into());
        // `1e-7` would be replaced by `1.0E-7`.
        let e = encode(&cols, &[t("x1a"), Cell::Float(1e-7), t("1e-7")]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("px_s") && m.contains("1e-7")), "{e:?}");
        // Solr's own copies: the source goes the way that gives them.
        assert_eq!(encode(&cols, &[t("a"), Cell::Float(100.0), t("100.0")]).unwrap(), r#"{"id":"a","px_d":100.0}"#);
        assert_eq!(encode(&cols, &[t("a"), Cell::Float(100.0), t("100")]).unwrap(), r#"{"id":"a","px_d":100}"#);
        assert_eq!(encode(&cols, &[t("a"), Cell::Float(1e-7), t("1.0E-7")]).unwrap(), r#"{"id":"a","px_d":1.0E-7}"#);
        assert_eq!(encode(&cols, &[t("a"), Cell::Float(2.5), t("2.5")]).unwrap(), r#"{"id":"a","px_d":2.5}"#);
        assert_eq!(encode(&cols, &[t("a"), Cell::Float(-0.0), t("-0.0")]).unwrap(), r#"{"id":"a","px_d":-0.0}"#);
        assert!(encode(&cols, &[t("a"), Cell::Float(-0.0), t("0")]).is_err());
        // Java 17 prints 1e23 as `9.999999999999999E22`: unsure, refused
        // with that reason; known from Java 19.
        let e = encode(&cols, &[t("a"), Cell::Float(1e23), t("1.0E23")]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("versión de Java")), "{e:?}");
        let modern: Vec<Col> = cols.iter().cloned().map(|c| Col { java: JavaText::Java19, ..c }).collect();
        assert_eq!(encode(&modern, &[t("a"), Cell::Float(1e23), t("1.0E23")]).unwrap(), r#"{"id":"a","px_d":1.0E23}"#);
        // Java 17 prints 2⁻²⁴ as `5.9604644775390625E-8`: the shortest
        // text is refused there (and when unsure), Solr's own copy loads.
        let old: Vec<Col> = cols.iter().cloned().map(|c| Col { java: JavaText::Java17, ..c }).collect();
        let p = Cell::Float(2f64.powi(-24));
        for c in [&old, &cols] {
            let e = encode(c, &[t("a"), p.clone(), t("5.960464477539063E-8")]).unwrap_err();
            assert!(matches!(&e, Error::Unsupported(m) if m.contains("px_s")), "{e:?}");
        }
        assert_eq!(
            encode(&old, &[t("a"), p.clone(), t("5.9604644775390625E-8")]).unwrap(),
            r#"{"id":"a","px_d":5.960464477539063E-8}"#
        );
        assert_eq!(
            encode(&modern, &[t("a"), p, t("5.960464477539063E-8")]).unwrap(),
            r#"{"id":"a","px_d":5.960464477539063E-8}"#
        );
        // Two destinations wanting the number written two ways: refused.
        let two = load_names(&s, &["id", "px_d", "px_s", "px2_s"]);
        assert!(encode(&two, &[t("a"), Cell::Float(100.0), t("100"), t("100.0")]).is_err());
        assert_eq!(encode(&two, &[t("a"), Cell::Float(100.0), t("100.0"), t("100.0")]).unwrap(), r#"{"id":"a","px_d":100.0}"#);
        // Each item of a list its own way.
        let list = load_names(&s, &["id", "xs_ds", "xs_ss"]);
        let doc = encode(&list, &[t("a"), Cell::Json("[100.0,5,1e-7]".into()), Cell::Json(r#"["100.0","5","1.0E-7"]"#.into())]).unwrap();
        assert_eq!(doc, r#"{"id":"a","xs_ds":[100.0,5,1.0E-7]}"#);
    }

    /// X2: a copyField destination left NULL would be filled by Solr.
    #[test]
    fn null_copy_destination_is_refused() {
        let s = copy_schema();
        let cols = load_names(&s, &["id", "title_t", "alt_s"]);
        let t = |v: &str| Cell::Text(v.into());
        let e = encode(&cols, &[t("1"), t("A"), Cell::Null]).unwrap_err();
        assert!(matches!(&e, Error::Unsupported(m) if m.contains("alt_s") && m.contains("NULL")), "{e:?}");
        // No source value: nothing is copied.
        assert_eq!(encode(&cols, &[t("1"), Cell::Null, Cell::Null]).unwrap(), r#"{"id":"1"}"#);
        // A destination that keeps nothing reads back NULL anyway.
        let hidden: Vec<Col> = cols.iter().cloned().map(|c| if c.name == "alt_s" { Col { hidden: true, ..c } } else { c }).collect();
        assert_eq!(encode(&hidden, &[t("1"), t("A"), Cell::Null]).unwrap(), r#"{"id":"1","title_t":"A"}"#);
    }

    /// A negative zero keeps its sign; a pfloat takes a decimal only if Solr
    /// prints that float back as it (`1e-45` is kept as `1.4E-45`).
    #[test]
    fn negative_zero_and_float_texts() {
        let floats = cols_one("xs_fs", Kind::Float32, true);
        assert_eq!(encode(&floats, &[Cell::Json("[16777216, -0.0]".into())]).unwrap(), r#"{"xs_fs":[16777216,-0.0]}"#);
        assert_eq!(one(Kind::Float64, Cell::Float(-0.0)).unwrap(), r#"{"f":-0.0}"#);
        assert_eq!(one(Kind::Float32, Cell::Decimal("-0".into())).unwrap(), r#"{"f":-0.0}"#);
        assert_eq!(one(Kind::Int32, Cell::Decimal("-0.0".into())).unwrap(), r#"{"f":0}"#);
        assert!(one(Kind::Float32, Cell::Decimal("1e-45".into())).is_err());
        // Solr prints this float as `1.4E-45` on Java 17 and 19 alike.
        let doc = format!(r#"{{"f":{}}}"#, plain_decimal("1.4e-45").unwrap());
        assert_eq!(one(Kind::Float32, Cell::Float(1.4e-45)).unwrap(), doc);
        let modern = vec![Col { java: JavaText::Java19, ..Col::new("f", Kind::Float32, false) }];
        assert_eq!(encode(&modern, &[Cell::Float(1.4e-45)]).unwrap(), doc);
        assert!(encode(&modern, &[Cell::Decimal("1e-45".into())]).is_err());
    }

    /// Round 2, problem 2: Java 17 prints the float `2⁻³⁸` as
    /// `3.6379788E-12` (Java 19: `3.637979E-12`), so that decimal would
    /// read back changed there: refused on Java 17 and when unsure.
    #[test]
    fn float_decimals_follow_the_servers_java() {
        let on = |java: JavaText| vec![Col { java, ..Col::new("f", Kind::Float32, false) }];
        for (dec, j17) in [("3.637979E-12", "3.6379788E-12"), ("1.2621775E-29", "1.26217745E-29")] {
            for java in [JavaText::Java17, JavaText::Unknown] {
                let e = encode(&on(java), &[Cell::Decimal(dec.into())]).unwrap_err();
                assert!(matches!(&e, Error::Query(m) if m.contains(dec)), "{e:?}");
            }
            assert!(encode(&on(JavaText::Java19), &[Cell::Decimal(dec.into())]).is_ok());
            // Java 17's own text is the float exactly as it prints it.
            let doc = encode(&on(JavaText::Java17), &[Cell::Decimal(j17.into())]).unwrap();
            assert_eq!(doc, format!(r#"{{"f":{}}}"#, plain_decimal(j17).unwrap()));
        }
    }

    /// Round 2, problem 3: a float into a text field goes as its text, not
    /// as a number Solr would keep in its Java form (`1.0E-7`).
    #[test]
    fn float_into_text_is_its_text() {
        assert_eq!(one(Kind::Text, Cell::Float(1e-7)).unwrap(), r#"{"f":"1e-7"}"#);
        assert_eq!(one(Kind::Text, Cell::Float(100.0)).unwrap(), r#"{"f":"100.0"}"#);
        assert_eq!(one(Kind::Text, Cell::Float(2f64.powi(-24))).unwrap(), r#"{"f":"5.960464477539063e-8"}"#);
    }

    /// The load's documents only insert.
    #[test]
    fn load_documents_are_insert_only() {
        let mut buf = Vec::new();
        encode_new_doc(&mut buf, &cols_one("id", Kind::Text, false), &[Cell::Text("k".into())]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), r#"{"_version_":-1,"id":"k"}"#);
        assert_eq!(conflict_key("version conflict for keep expected=-1 actual=1877680411802337280").as_deref(), Some("keep"));
        assert_eq!(
            conflict_key("Async exception during distributed update: version conflict for a b expected=-1 actual=1").as_deref(),
            Some("a b")
        );
        assert_eq!(conflict_key("other"), None);
    }

    /// The response is parsed as it arrives: whatever the chunking, each
    /// document comes out whole, and only about one is held at a time.
    #[test]
    fn select_responses_are_read_one_document_at_a_time() {
        let big = "x".repeat(300_000);
        let docs: Vec<Value> = (0..20).map(|i| json!({"id": format!("d{i}"), "t": if i % 5 == 0 { big.clone() } else { "a\"}]{".into() }})).collect();
        let text = json!({
            "responseHeader": {"status": 0, "params": {"docs": [{"no": 1}]}},
            "response": {"numFound": 20, "start": 0, "docs": docs},
            "nextCursorMark": "AoE\"x"
        })
        .to_string();
        for chunk in [1, 7, 4096, 1 << 20] {
            let mut scan = DocScanner::default();
            let mut got = Vec::new();
            for part in text.as_bytes().chunks(chunk) {
                scan.feed(part, &mut |d| {
                    got.push(serde_json::from_slice::<Value>(d).unwrap());
                    Ok(())
                })
                .unwrap();
            }
            assert_eq!(got, docs, "chunk {chunk}");
            assert_eq!(scan.cursor.as_deref(), Some("AoE\"x"));
            // One big document plus a chunk, never the whole page.
            assert!(scan.peak <= 300_100 + chunk, "chunk {chunk}: {}", scan.peak);
        }
    }

    #[test]
    fn read_helpers() {
        assert!(plain_fl_name("title_s"));
        assert!(plain_fl_name("_x"));
        for bad in ["score", "[docid]", "a,b", "a b", "1x", "", "sum(a,b)", "a:b"] {
            assert!(!plain_fl_name(bad), "{bad}");
        }
        assert_eq!(next_page(0, 0), PAGE_FIRST);
        // Small documents: grows fourfold, up to PAGE.
        assert_eq!(next_page(10, 1_000), 40);
        assert_eq!(next_page(4_000, 400_000), PAGE);
        // 3 MiB documents: one per page.
        assert_eq!(next_page(10, 40 * 1024 * 1024), 1);
        assert_eq!(next_page(1, 4 * 1024 * 1024), 1);
        // ~100 KiB documents: about PAGE_BYTES per page.
        assert_eq!(next_page(40, 40 * 100 * 1024), 40);
        assert_eq!(seg("my coll/1"), "my%20coll%2F1");
        assert_eq!(seg("books_v-2.x"), "books_v-2.x");
        assert_eq!(key_text(&Cell::Int(5)).as_deref(), Some("5"));
        assert_eq!(key_text(&Cell::Text("a\"b".into())).as_deref(), Some("a\"b"));
        assert_eq!(key_text(&Cell::Null), None);
    }

    #[test]
    fn read_back_what_was_written() {
        let s = schema();
        let names: Vec<String> = ["id", "n_i", "when_dt", "b_bin", "tags"].iter().map(|n| n.to_string()).collect();
        let cols = load_cols(&s, &names).unwrap();
        let row = vec![
            Cell::Text("k".into()),
            Cell::Int(-7),
            Cell::DateTimeTz("2024-01-02 03:04:05.123+00:00".into()),
            Cell::Bytes(vec![9; 300]),
            Cell::Json("[\"a\",\"b\"]".into()),
        ];
        let doc: Map<String, Value> = serde_json::from_str(&encode(&cols, &row).unwrap()).unwrap();
        // Multi-valued fields read as JSON (kind Other).
        let read: Vec<(String, Kind)> = cols.iter().map(|c| (c.name.clone(), if c.multi { Kind::Other } else { c.kind })).collect();
        let back = doc_row(&doc, &read);
        assert_eq!(back[..4], row[..4]);
        assert_eq!(back[4], Cell::Json("[\"a\",\"b\"]".into()));
    }
}
