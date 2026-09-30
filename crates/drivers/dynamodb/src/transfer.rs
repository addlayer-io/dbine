//! Bulk transfer (see `dbine_driver::transfer`) for DynamoDB.
//!
//! Reading: a parallel `Scan` of the table (or of an index), [`SEGMENTS`]
//! segments at once, each paged by `LastEvaluatedKey`; pages are handed
//! over as they arrive (a bounded channel holds the segments back when the
//! consumer is behind). With a filter it's the PartiQL `SELECT * FROM "t"
//! WHERE <filter>` paged by `NextToken`. Without asked-for columns a first
//! pass reads every item to learn every attribute name (DynamoDB has no
//! schema beyond the key: a sample would drop attributes that only appear
//! later), and the second one hands the rows over; an attribute that shows
//! up only in the second pass (the table changed meanwhile) fails the read.
//!
//! Values, without loss: `N` as an integer when it fits 64 bits and else as
//! exact decimal text (never through a double), `S` text, `B` binary,
//! `BOOL`. `L`, `M`, the sets and an explicit `NULL` are JSON cells, with
//! numbers written with all their digits and what plain JSON can't tell
//! apart tagged with a one-key object: `{"$B":"<hex>"}` (a binary inside a
//! list or map), `{"$SS":[…]}`, `{"$NS":[…]}`, `{"$BS":["<hex>",…]}`, and
//! `{"$M":{…}}` around a user map whose only key is one of those tags. A
//! top-level `NULL` is the JSON cell `null`; a missing attribute is a null
//! cell. Written back, each comes out as the same type.
//!
//! The tags are this crate's own, so the read names each column's type
//! with DynamoDB's codes (`S | M`…; every code when the columns were asked
//! for and not seen), and a load reads the tags only in the JSON of such a
//! column. JSON from any other source (a PostgreSQL `jsonb`, a column
//! without a type…) is plain JSON: its `{"$B":"00ff"}` is the map it says.
//!
//! Loading: `TransactWriteItems` of up to [`TX_ITEMS`] conditional puts
//! (`attribute_not_exists` of the partition key), [`IN_FLIGHT`] at once and
//! at most [`MAX_INFLIGHT_BYTES`] of items in flight. A put never replaces an
//! item: an existing key (another writer's, or one repeated in the data)
//! cancels its whole transaction and fails the load. A transaction is all
//! or nothing, so the progress counts rows really written, the ones of
//! transactions that end while a failed load waits for them included. A
//! retry whose previous try may have been written (an internal error) goes
//! with the same idempotency token, so it doesn't fail on its own item. On
//! an error, or when the load is cancelled, no request is started again and
//! the ones already sent are waited for before returning: nothing is
//! written after the load ends. Items over 400 KB and numbers DynamoDB
//! can't hold are refused before sending, naming the key.
//!
//! A row means what it means in the insert script: nulls are left out;
//! numbers go as `N` (their exact text), text and temporal values as `S`,
//! binaries as `B`, JSON cells as the values above.

use crate::{err, target, DynamoSession};
use aws_sdk_dynamodb::error::ProvideErrorMetadata;
use aws_sdk_dynamodb::operation::transact_write_items::TransactWriteItemsError;
use aws_sdk_dynamodb::primitives::Blob;
use aws_sdk_dynamodb::types::{AttributeValue, Put, TransactWriteItem};
use aws_sdk_dynamodb::Client;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{kinds, Error, Result};
use std::collections::{BTreeMap, HashMap, HashSet};
use std::fmt::Write as _;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinSet;

type Item = HashMap<String, AttributeValue>;

/// Parallel scan segments.
const SEGMENTS: i32 = 4;
/// Puts per `TransactWriteItems` (the service's limit).
pub(crate) const TX_ITEMS: usize = 100;
/// Item bytes per transaction (the service allows 4 MB).
const TX_BYTES: usize = 3 << 20;
/// Requests in flight at once…
pub(crate) const IN_FLIGHT: usize = 16;
/// …and item bytes in them. The request's copy and its JSON body roughly
/// triple it: ~32 MiB per table with the batch at hand.
const MAX_INFLIGHT_BYTES: usize = 8 << 20;
/// DynamoDB's item size limit.
const MAX_ITEM: usize = 400 * 1024;
/// Tries of a throttled request.
const MAX_TRIES: u32 = 12;
/// Longest wait for the requests already sent when a load fails or is
/// cancelled.
const DRAIN: Duration = Duration::from_secs(120);
/// JSON tags of the values plain JSON can't tell apart.
const TAGS: [&str; 5] = ["$B", "$SS", "$NS", "$BS", "$M"];
/// DynamoDB's attribute types, as a read names a column's type (`S | M`).
const TYPE_CODES: [&str; 10] = ["S", "N", "B", "BOOL", "NULL", "L", "M", "SS", "NS", "BS"];

/// A column's type from the bits of the [`TYPE_CODES`] seen in it; every
/// code when none was seen.
fn column_type(seen: u16) -> String {
    let all = seen == 0;
    TYPE_CODES.iter().enumerate().filter(|(i, _)| all || seen & (1 << i) != 0).map(|(_, c)| *c).collect::<Vec<_>>().join(" | ")
}

fn type_bit(v: &AttributeValue) -> u16 {
    let code = crate::type_code(v);
    TYPE_CODES.iter().position(|c| *c == code).map_or(0, |i| 1 << i)
}

/// A column this crate read from DynamoDB (its type made only of
/// [`TYPE_CODES`]): the only JSON whose tags mean DynamoDB types.
fn dynamo_column(type_name: &str) -> bool {
    let mut parts = type_name.split('|').map(str::trim).peekable();
    parts.peek().is_some_and(|p| !p.is_empty()) && parts.all(|p| TYPE_CODES.contains(&p))
}

/// A number's exact text without exponent (`1.5E+3` → `1500`).
pub(crate) fn plain_number(n: &str) -> String {
    let n = n.trim().trim_start_matches('+');
    let Some((m, e)) = n.split_once(['e', 'E']) else { return n.to_string() };
    let Ok(exp) = e.trim_start_matches('+').parse::<i64>() else { return n.to_string() };
    // Far beyond DynamoDB's range: left as is (and refused) instead of
    // spelling out millions of zeros.
    if exp.abs() > 1_000 {
        return n.to_string();
    }
    let (neg, m) = match m.strip_prefix('-') {
        Some(rest) => (true, rest),
        None => (false, m.trim_start_matches('+')),
    };
    let (int, frac) = m.split_once('.').unwrap_or((m, ""));
    let mut digits = format!("{int}{frac}");
    if digits.chars().all(|c| c == '0') {
        return "0".into();
    }
    let mut point = int.len() as i64 + exp;
    while digits.len() > 1 && digits.starts_with('0') {
        digits.remove(0);
        point -= 1;
    }
    let len = digits.len() as i64;
    let body = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point >= len {
        format!("{digits}{}", "0".repeat((point - len) as usize))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    let zero = body.chars().all(|c| c == '0' || c == '.');
    if neg && !zero {
        format!("-{body}")
    } else {
        body
    }
}

/// A number as DynamoDB takes it (plain digits), or why it can't: up to 38
/// significant digits, from 1E-130 to 9.99…E+125.
fn check_number(n: &str) -> Result<String> {
    let p = plain_number(n);
    let body = p.strip_prefix('-').unwrap_or(&p);
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    let digit = |s: &str| s.bytes().all(|b| b.is_ascii_digit());
    if int.is_empty() || !digit(int) || !digit(frac) {
        return Err(Error::Query(format!("«{n}» no es un número")));
    }
    let digits = format!("{int}{frac}");
    let Some(first) = digits.bytes().position(|b| b != b'0') else { return Ok("0".into()) };
    let last = digits.bytes().rposition(|b| b != b'0').unwrap_or(first);
    let exp = int.len() as i64 - 1 - first as i64;
    if last - first + 1 > 38 || !(-130..=125).contains(&exp) {
        return Err(Error::Query(format!(
            "DynamoDB no guarda el número {n}: admite hasta 38 dígitos significativos, de 1E-130 a 9.99E+125"
        )));
    }
    Ok(p)
}

/// An `N` as a cell: 64-bit integers as such, the rest as exact decimal text.
fn number_cell(n: &str) -> Cell {
    if let Ok(i) = n.parse::<i64>() {
        return Cell::Int(i);
    }
    if let Ok(u) = n.parse::<u64>() {
        return Cell::UInt(u);
    }
    Cell::Decimal(plain_number(n))
}

fn hex(b: &[u8]) -> String {
    const H: &[u8; 16] = b"0123456789abcdef";
    let mut s = String::with_capacity(b.len() * 2);
    for x in b {
        s.push(H[(x >> 4) as usize] as char);
        s.push(H[(x & 15) as usize] as char);
    }
    s
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    if !s.len().is_multiple_of(2) {
        return None;
    }
    let v = |c: u8| (c as char).to_digit(16).map(|d| d as u8);
    s.as_bytes().chunks(2).map(|p| Some(v(p[0])? << 4 | v(p[1])?)).collect()
}

fn unknown_type() -> Error {
    Error::Unsupported("DynamoDB devolvió un tipo de atributo que esta versión de DBine no conoce".into())
}

/// An attribute as exact JSON (see the module's notes).
fn write_json(v: &AttributeValue, out: &mut String) -> Result<()> {
    let text = |s: &str, out: &mut String| out.push_str(&serde_json::Value::from(s).to_string());
    fn list<T>(out: &mut String, tag: &str, xs: &[T], mut each: impl FnMut(&T, &mut String) -> Result<()>) -> Result<()> {
        if !tag.is_empty() {
            let _ = write!(out, "{{\"{tag}\":");
        }
        out.push('[');
        for (i, x) in xs.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            each(x, out)?;
        }
        out.push(']');
        if !tag.is_empty() {
            out.push('}');
        }
        Ok(())
    }
    match v {
        AttributeValue::S(s) => text(s, out),
        AttributeValue::N(n) => out.push_str(&plain_number(n)),
        AttributeValue::B(b) => {
            let _ = write!(out, "{{\"$B\":\"{}\"}}", hex(b.as_ref()));
        }
        AttributeValue::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        AttributeValue::Null(_) => out.push_str("null"),
        AttributeValue::L(l) => list(out, "", l, write_json)?,
        AttributeValue::M(m) => {
            let escaped = m.len() == 1 && m.keys().all(|k| TAGS.contains(&k.as_str()));
            if escaped {
                out.push_str("{\"$M\":");
            }
            // Sorted: a HashMap has no order.
            let mut keys: Vec<&String> = m.keys().collect();
            keys.sort();
            out.push('{');
            for (i, k) in keys.into_iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                text(k, out);
                out.push(':');
                write_json(&m[k], out)?;
            }
            out.push('}');
            if escaped {
                out.push('}');
            }
        }
        AttributeValue::Ss(s) => list(out, "$SS", s, |s, out| {
            text(s, out);
            Ok(())
        })?,
        AttributeValue::Ns(n) => list(out, "$NS", n, |n, out| {
            out.push_str(&plain_number(n));
            Ok(())
        })?,
        AttributeValue::Bs(b) => list(out, "$BS", b, |b, out| {
            let _ = write!(out, "\"{}\"", hex(b.as_ref()));
            Ok(())
        })?,
        _ => return Err(unknown_type()),
    }
    Ok(())
}

fn json_text(v: &AttributeValue) -> Result<String> {
    let mut s = String::new();
    write_json(v, &mut s)?;
    Ok(s)
}

/// A top-level attribute as a cell.
pub(crate) fn attr_cell(v: &AttributeValue) -> Result<Cell> {
    Ok(match v {
        AttributeValue::S(s) => Cell::Text(s.clone()),
        AttributeValue::N(n) => number_cell(n),
        AttributeValue::B(b) => Cell::Bytes(b.as_ref().to_vec()),
        AttributeValue::Bool(b) => Cell::Bool(*b),
        other => Cell::Json(json_text(other)?),
    })
}

fn item_row(item: &Item, cols: &[TransferColumn]) -> Result<Vec<Cell>> {
    cols.iter().map(|c| item.get(&c.name).map_or(Ok(Cell::Null), attr_cell)).collect()
}

/// A JSON value with its numbers' text kept (serde_json here would turn
/// them into doubles).
#[derive(Debug)]
enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

/// A JSON number's grammar: `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`.
fn json_number(s: &str) -> bool {
    let s = s.strip_prefix('-').unwrap_or(s);
    let (m, e) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, Some(e.strip_prefix(['+', '-']).unwrap_or(e))),
        None => (s, None),
    };
    let (int, frac) = match m.split_once('.') {
        Some((i, f)) => (i, Some(f)),
        None => (m, None),
    };
    let digits = |s: &str| !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit());
    digits(int) && (int == "0" || !int.starts_with('0')) && frac.is_none_or(digits) && e.is_none_or(digits)
}

struct Parser<'a> {
    s: &'a [u8],
    i: usize,
}

impl Parser<'_> {
    fn peek(&mut self) -> Option<u8> {
        while matches!(self.s.get(self.i), Some(b' ' | b'\n' | b'\r' | b'\t')) {
            self.i += 1;
        }
        self.s.get(self.i).copied()
    }

    fn eat(&mut self, lit: &str) -> bool {
        let ok = self.s[self.i..].starts_with(lit.as_bytes());
        if ok {
            self.i += lit.len();
        }
        ok
    }

    fn string(&mut self) -> Option<String> {
        let start = self.i;
        self.i += 1;
        loop {
            match self.s.get(self.i)? {
                b'\\' => self.i += 2,
                b'"' => break,
                _ => self.i += 1,
            }
        }
        self.i += 1;
        serde_json::from_slice(self.s.get(start..self.i)?).ok()
    }

    fn value(&mut self, depth: usize) -> Option<J> {
        if depth > 64 {
            return None;
        }
        Some(match self.peek()? {
            b'n' => self.eat("null").then_some(J::Null)?,
            b't' => self.eat("true").then_some(J::Bool(true))?,
            b'f' => self.eat("false").then_some(J::Bool(false))?,
            b'"' => J::Str(self.string()?),
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                if self.peek()? == b']' {
                    self.i += 1;
                    return Some(J::Arr(v));
                }
                loop {
                    v.push(self.value(depth + 1)?);
                    match self.peek()? {
                        b',' => self.i += 1,
                        b']' => break,
                        _ => return None,
                    }
                }
                self.i += 1;
                J::Arr(v)
            }
            b'{' => {
                self.i += 1;
                let mut o = Vec::new();
                if self.peek()? == b'}' {
                    self.i += 1;
                    return Some(J::Obj(o));
                }
                loop {
                    if self.peek()? != b'"' {
                        return None;
                    }
                    let k = self.string()?;
                    if self.peek()? != b':' {
                        return None;
                    }
                    self.i += 1;
                    o.push((k, self.value(depth + 1)?));
                    match self.peek()? {
                        b',' => self.i += 1,
                        b'}' => break,
                        _ => return None,
                    }
                }
                self.i += 1;
                J::Obj(o)
            }
            _ => {
                let start = self.i;
                while matches!(self.s.get(self.i), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
                    self.i += 1;
                }
                let n = std::str::from_utf8(&self.s[start..self.i]).ok()?;
                json_number(n).then(|| J::Num(n.to_string()))?
            }
        })
    }
}

/// A JSON document, numbers as their text; `None` when it isn't JSON.
fn parse_json(s: &str) -> Option<J> {
    let mut p = Parser { s: s.as_bytes(), i: 0 };
    let v = p.value(0)?;
    p.peek().is_none().then_some(v)
}

/// A tagged value (`{"$SS": […]}`…), when `k` / `v` are one.
fn tagged(k: &str, v: &J) -> Option<Result<AttributeValue>> {
    let strs = |v: &J| match v {
        J::Arr(a) if !a.is_empty() => a.iter().map(|x| if let J::Str(s) = x { Some(s.clone()) } else { None }).collect::<Option<Vec<_>>>(),
        _ => None,
    };
    let unique = |xs: &[String]| xs.iter().collect::<HashSet<_>>().len() == xs.len();
    match (k, v) {
        ("$B", J::Str(h)) => unhex(h).map(|b| Ok(AttributeValue::B(Blob::new(b)))),
        ("$SS", v) => strs(v).filter(|s| unique(s)).map(|s| Ok(AttributeValue::Ss(s))),
        ("$BS", v) => {
            let s = strs(v).filter(|s| unique(s))?;
            s.iter().map(|h| unhex(h).map(Blob::new)).collect::<Option<Vec<_>>>().map(|b| Ok(AttributeValue::Bs(b)))
        }
        ("$NS", J::Arr(a)) if !a.is_empty() => {
            let ns = a.iter().map(|x| if let J::Num(n) = x { Some(n.as_str()) } else { None }).collect::<Option<Vec<_>>>()?;
            let ns = match ns.into_iter().map(check_number).collect::<Result<Vec<_>>>() {
                Ok(n) => n,
                Err(e) => return Some(Err(e)),
            };
            let canon: HashSet<String> = ns.iter().map(|n| canon_number(n)).collect();
            (canon.len() == ns.len()).then_some(Ok(AttributeValue::Ns(ns)))
        }
        ("$M", J::Obj(o)) => Some(map_attr(o, true)),
        _ => None,
    }
}

fn map_attr(o: &[(String, J)], tags: bool) -> Result<AttributeValue> {
    // A repeated key: the last one wins, as in serde_json.
    Ok(AttributeValue::M(o.iter().map(|(k, v)| Ok((k.clone(), j_attr(v, tags)?))).collect::<Result<_>>()?))
}

/// A JSON value as an attribute; with `tags`, its one-key tag objects as
/// the types they stand for (JSON read from DynamoDB only).
fn j_attr(v: &J, tags: bool) -> Result<AttributeValue> {
    Ok(match v {
        J::Null => AttributeValue::Null(true),
        J::Bool(b) => AttributeValue::Bool(*b),
        J::Num(n) => AttributeValue::N(check_number(n)?),
        J::Str(s) => AttributeValue::S(s.clone()),
        J::Arr(a) => AttributeValue::L(a.iter().map(|x| j_attr(x, tags)).collect::<Result<_>>()?),
        J::Obj(o) => {
            if let (true, [(k, v)]) = (tags, o.as_slice()) {
                if let Some(t) = tagged(k, v) {
                    return t;
                }
            }
            map_attr(o, tags)?
        }
    })
}

/// A cell as the attribute the insert script would write; `None` for null
/// (left out). `tags`: the cell comes from a DynamoDB column (see
/// [`dynamo_column`]), and its JSON's tags are DynamoDB types.
pub(crate) fn cell_attr(c: &Cell, tags: bool) -> Result<Option<AttributeValue>> {
    Ok(Some(match c {
        Cell::Null => return Ok(None),
        Cell::Bool(b) => AttributeValue::Bool(*b),
        Cell::Int(i) => AttributeValue::N(i.to_string()),
        Cell::UInt(u) => AttributeValue::N(u.to_string()),
        Cell::Float(f) if f.is_finite() => AttributeValue::N(check_number(&f.to_string())?),
        Cell::Float(f) => return Err(Error::Query(format!("DynamoDB no guarda el número {f}"))),
        Cell::Decimal(d) => AttributeValue::N(check_number(d)?),
        Cell::Bytes(b) => AttributeValue::B(Blob::new(b.clone())),
        Cell::Json(s) => match parse_json(s) {
            Some(v) => j_attr(&v, tags)?,
            None => AttributeValue::S(s.clone()),
        },
        Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => AttributeValue::S(s.clone()),
    }))
}

/// A row as the item a put writes (nulls left out); `tags[i]`: column `i`
/// was read from DynamoDB.
pub(crate) fn row_item(names: &[String], row: &[Cell], tags: &[bool]) -> Result<Item> {
    let mut item = Item::with_capacity(names.len());
    for (i, (n, c)) in names.iter().zip(row).enumerate() {
        let v = cell_attr(c, tags.get(i).copied().unwrap_or(false)).map_err(|e| match e {
            Error::Query(m) => Error::Query(format!("atributo «{n}»: {m}")),
            e => e,
        })?;
        if let Some(v) = v {
            item.insert(n.clone(), v);
        }
    }
    Ok(item)
}

/// A number's canonical text (`1.50` and `1.5` are the same number).
fn canon_number(n: &str) -> String {
    let p = plain_number(n);
    if p.contains('.') {
        p.trim_end_matches('0').trim_end_matches('.').to_string()
    } else {
        p
    }
}

/// An item's size as DynamoDB counts it (names plus values; numbers take
/// about a byte per two significant digits).
fn attr_size(v: &AttributeValue) -> usize {
    let num = |n: &str| canon_number(n).bytes().filter(u8::is_ascii_digit).count().div_ceil(2) + 1;
    match v {
        AttributeValue::S(s) => s.len(),
        AttributeValue::N(n) => num(n),
        AttributeValue::B(b) => b.as_ref().len(),
        AttributeValue::L(l) => 3 + l.iter().map(|x| 1 + attr_size(x)).sum::<usize>(),
        AttributeValue::M(m) => 3 + m.iter().map(|(k, x)| 1 + k.len() + attr_size(x)).sum::<usize>(),
        AttributeValue::Ss(s) => s.iter().map(String::len).sum(),
        AttributeValue::Ns(n) => n.iter().map(|n| num(n)).sum(),
        AttributeValue::Bs(b) => b.iter().map(|b| b.as_ref().len()).sum(),
        _ => 1,
    }
}

fn item_size(item: &Item) -> usize {
    item.iter().map(|(k, v)| k.len() + attr_size(v)).sum()
}

/// `pk=…, sk=…` of an item, for messages and to spot repeated keys.
fn key_text(item: &Item, keys: &[String]) -> String {
    keys.iter()
        .filter_map(|k| {
            let v = match item.get(k)? {
                AttributeValue::N(n) => canon_number(n),
                v => json_text(v).unwrap_or_default(),
            };
            Some(format!("{k}={v}"))
        })
        .collect::<Vec<_>>()
        .join(", ")
}

/// How a failed request is tried again.
#[derive(Debug, PartialEq)]
enum Retry {
    No,
    /// It wasn't written (throttled, conflict): a new try.
    Fresh,
    /// It may have been written (an internal error), or a try with its
    /// token is still running: again with the same idempotency token, so
    /// that DynamoDB answers the first one's result instead of failing on
    /// its own items.
    Same,
}

/// Request errors worth another try.
fn retry_of(code: Option<&str>) -> Retry {
    match code {
        Some("ProvisionedThroughputExceededException" | "ThrottlingException" | "RequestLimitExceeded") => Retry::Fresh,
        Some("InternalServerError" | "TransactionInProgressException") => Retry::Same,
        _ => Retry::No,
    }
}

/// A new idempotency token (32 hex digits, DynamoDB takes up to 36).
fn request_token() -> String {
    use std::hash::{BuildHasher, Hasher};
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let state = std::collections::hash_map::RandomState::new();
    let mut h = state.build_hasher();
    h.write_u64(SEQ.fetch_add(1, Ordering::Relaxed));
    h.write_u128(std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map_or(0, |d| d.as_nanos()));
    let a = h.finish();
    h.write_u64(std::process::id().into());
    format!("{a:016x}{:016x}", h.finish())
}

fn backoff(attempt: u32) -> Duration {
    Duration::from_millis((25u64 << attempt.min(8)).min(5_000))
}

fn cancelled() -> Error {
    Error::Query("carga cancelada".into())
}

/// One transaction of conditional puts, retried while throttled; its rows
/// and bytes when written.
async fn put_tx(client: Client, table: String, pk: String, items: Vec<Item>, keys: Vec<String>, bytes: usize, stop: Arc<AtomicBool>) -> Result<(u64, usize)> {
    let rows = items.len() as u64;
    let actions: Vec<TransactWriteItem> = items
        .into_iter()
        .map(|i| {
            Put::builder()
                .table_name(&table)
                .set_item(Some(i))
                .condition_expression("attribute_not_exists(#k)")
                .expression_attribute_names("#k", &pk)
                .build()
                .map(|p| TransactWriteItem::builder().put(p).build())
        })
        .collect::<std::result::Result<_, _>>()
        .map_err(|e| Error::Query(e.to_string()))?;
    let mut attempt = 0;
    // The SDK's own retries reuse the token; ours keep it only when the
    // last try may have been written (see [`Retry::Same`]).
    let mut token = request_token();
    loop {
        if stop.load(Ordering::SeqCst) {
            return Err(cancelled());
        }
        let e = match client.transact_write_items().set_transact_items(Some(actions.clone())).client_request_token(&token).send().await {
            Ok(_) => return Ok((rows, bytes)),
            Err(e) => e,
        };
        let retry = match e.as_service_error() {
            Some(TransactWriteItemsError::TransactionCanceledException(tc)) => {
                let reasons = tc.cancellation_reasons();
                if let Some(i) = reasons.iter().position(|r| r.code() == Some("ConditionalCheckFailed")) {
                    return Err(Error::Query(format!(
                        "Ya hay un ítem con la clave {} (en la tabla o antes en los mismos datos): la carga nunca reemplaza ítems.",
                        keys.get(i).map_or("?", String::as_str)
                    )));
                }
                let busy = |c: Option<&str>| {
                    matches!(c, None | Some("None" | "ThrottlingError" | "TransactionConflict" | "ProvisionedThroughputExceeded" | "RequestLimitExceeded"))
                };
                if !reasons.iter().all(|r| busy(r.code())) {
                    let why: Vec<String> = reasons
                        .iter()
                        .enumerate()
                        .filter(|(_, r)| !busy(r.code()))
                        .take(3)
                        .map(|(i, r)| format!("{}: {} {}", keys.get(i).map_or("?", String::as_str), r.code().unwrap_or_default(), r.message().unwrap_or_default()))
                        .collect();
                    return Err(Error::Query(format!("DynamoDB canceló la escritura de {rows} ítems. {}", why.join("; "))));
                }
                Retry::Fresh
            }
            _ => retry_of(e.code()),
        };
        if retry == Retry::No || attempt >= MAX_TRIES {
            return Err(if retry == Retry::No {
                err(e)
            } else {
                Error::Query(format!("DynamoDB siguió rechazando la escritura después de {MAX_TRIES} intentos ({})", e.code().unwrap_or("?")))
            });
        }
        if retry == Retry::Fresh {
            token = request_token();
        }
        attempt += 1;
        tokio::time::sleep(backoff(attempt)).await;
    }
}

type Done = std::result::Result<Result<(u64, usize)>, tokio::task::JoinError>;

/// The transaction tasks still alive (running, or not yet dropped): what a
/// cancelled load waits for without tokio (no timer, no `block_on`), so the
/// wait also works while the runtime shuts down, when its tasks are dropped
/// and this count falls with them.
#[derive(Default)]
struct Alive {
    n: std::sync::Mutex<usize>,
    zero: std::sync::Condvar,
}

impl Alive {
    fn count(&self) -> usize {
        *self.n.lock().unwrap_or_else(|p| p.into_inner())
    }

    /// Wait until no task is alive, for at most `limit`; how many still are.
    fn wait(&self, limit: Duration) -> usize {
        let n = self.n.lock().unwrap_or_else(|p| p.into_inner());
        let (n, _) = self.zero.wait_timeout_while(n, limit, |n| *n > 0).unwrap_or_else(|p| p.into_inner());
        *n
    }
}

/// Held by a transaction task: dropped when it ends or is dropped.
struct AliveGuard(Arc<Alive>);

impl AliveGuard {
    fn new(a: &Arc<Alive>) -> Self {
        *a.n.lock().unwrap_or_else(|p| p.into_inner()) += 1;
        Self(a.clone())
    }
}

impl Drop for AliveGuard {
    fn drop(&mut self) {
        let mut n = self.0.n.lock().unwrap_or_else(|p| p.into_inner());
        *n -= 1;
        if *n == 0 {
            self.0.zero.notify_all();
        }
    }
}

/// The transactions in flight of one load. Dropped with requests still out
/// (the load was cancelled), it waits for them: a request already sent is
/// written by the server whether or not its answer is awaited.
struct Writer {
    client: Client,
    table: String,
    pk: String,
    set: JoinSet<Result<(u64, usize)>>,
    alive: Arc<Alive>,
    bytes: usize,
    /// Rows of the transactions written.
    written: u64,
    stop: Arc<AtomicBool>,
    /// A failed load already waited (and reported what it couldn't).
    drained: bool,
}

impl Writer {
    fn new(client: Client, table: String, pk: String) -> Self {
        Writer {
            client,
            table,
            pk,
            set: JoinSet::new(),
            alive: Arc::default(),
            bytes: 0,
            written: 0,
            stop: Arc::new(AtomicBool::new(false)),
            drained: false,
        }
    }

    fn spawn(&mut self, items: Vec<Item>, keys: Vec<String>, bytes: usize) {
        self.bytes += bytes;
        let (c, t, pk, stop) = (self.client.clone(), self.table.clone(), self.pk.clone(), self.stop.clone());
        let guard = AliveGuard::new(&self.alive);
        self.set.spawn(async move {
            let _guard = guard;
            put_tx(c, t, pk, items, keys, bytes, stop).await
        });
    }

    fn done(&mut self, r: Done) -> Result<()> {
        let (rows, bytes) = r.map_err(|e| Error::State(format!("carga interrumpida: {e}")))??;
        self.bytes -= bytes;
        self.written += rows;
        Ok(())
    }

    /// Wait until `bytes` more fit.
    async fn room(&mut self, bytes: usize) -> Result<()> {
        while !self.set.is_empty() && (self.set.len() >= IN_FLIGHT || self.bytes + bytes > MAX_INFLIGHT_BYTES) {
            if let Some(r) = self.set.join_next().await {
                self.done(r)?;
            }
        }
        Ok(())
    }

    /// Stop retrying and wait for what was sent (the rows of the ones that
    /// get written are counted); how many requests still hadn't answered
    /// after [`DRAIN`]. Those are left running, not aborted: aborting only
    /// stops waiting for the answer, the server writes them anyway.
    async fn drain(&mut self) -> usize {
        self.stop.store(true, Ordering::SeqCst);
        self.drained = true;
        let deadline = tokio::time::Instant::now() + DRAIN;
        while let Ok(Some(r)) = tokio::time::timeout_at(deadline, self.set.join_next()).await {
            let _ = self.done(r);
        }
        let left = self.set.len();
        self.set.detach_all();
        left
    }
}

impl Drop for Writer {
    fn drop(&mut self) {
        if self.drained || self.set.is_empty() {
            return;
        }
        // Cancelled (the load's future dropped): no retry starts again, and
        // the tasks are detached instead of aborted — an abort wouldn't
        // stop a request the server already has, and a request that ends
        // is one that can't be written later.
        self.stop.store(true, Ordering::SeqCst);
        self.set.detach_all();
        if std::thread::panicking() {
            return;
        }
        use tokio::runtime::{Handle, RuntimeFlavor};
        let left = match Handle::try_current().map(|h| h.runtime_flavor()) {
            // Waiting blocks this worker only (`block_in_place` hands its
            // other tasks to another thread). No tokio timer or `block_on`:
            // this also runs while the runtime shuts down, when the tasks
            // are dropped (and `alive` reaches zero) instead of finishing.
            Ok(RuntimeFlavor::MultiThread) => {
                let alive = self.alive.clone();
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| tokio::task::block_in_place(|| alive.wait(DRAIN))))
                    .unwrap_or_else(|_| alive.count())
            }
            // Outside any runtime the tasks run on the runtime's own threads.
            Err(_) => self.alive.wait(DRAIN),
            // A single-thread runtime runs the tasks on this very thread:
            // they can't be waited for here.
            Ok(_) => self.alive.count(),
        };
        if left > 0 {
            // Nobody is left to return an error to: the log is the report.
            tracing::error!(
                table = %self.table,
                unanswered = left,
                "DynamoDB load cancelled with requests still unanswered: their items may be written after the load ended"
            );
        }
    }
}

/// Every page of one scan segment, into `tx`.
async fn scan_segment(client: Client, table: String, index: Option<String>, segment: i32, tx: mpsc::Sender<Result<Vec<Item>>>) {
    let mut start: Option<Item> = None;
    loop {
        let r = client
            .scan()
            .table_name(&table)
            .set_index_name(index.clone())
            .segment(segment)
            .total_segments(SEGMENTS)
            .set_exclusive_start_key(start.take())
            .send()
            .await;
        match r {
            Ok(out) => {
                let next = out.last_evaluated_key.filter(|k| !k.is_empty());
                if tx.send(Ok(out.items.unwrap_or_default())).await.is_err() {
                    return;
                }
                match next {
                    Some(k) => start = Some(k),
                    None => return,
                }
            }
            Err(e) => {
                let _ = tx.send(Err(err(e))).await;
                return;
            }
        }
    }
}

/// Every page of a PartiQL `SELECT`, into `tx`.
async fn select_pages(client: Client, stmt: String, tx: mpsc::Sender<Result<Vec<Item>>>) {
    let mut token: Option<String> = None;
    loop {
        match client.execute_statement().statement(&stmt).set_next_token(token.take()).send().await {
            Ok(out) => {
                let next = out.next_token.filter(|t| !t.is_empty());
                if tx.send(Ok(out.items.unwrap_or_default())).await.is_err() {
                    return;
                }
                match next {
                    Some(t) => token = Some(t),
                    None => return,
                }
            }
            Err(e) => {
                let _ = tx.send(Err(err(e))).await;
                return;
            }
        }
    }
}

struct AbortOnDrop(Vec<tokio::task::JoinHandle<()>>);

impl Drop for AbortOnDrop {
    fn drop(&mut self) {
        for h in &self.0 {
            h.abort();
        }
    }
}

/// The pages of a read, as they arrive (at most [`SEGMENTS`] waiting).
fn pages(client: &Client, table: &str, index: Option<&str>, filter: Option<&str>) -> (mpsc::Receiver<Result<Vec<Item>>>, AbortOnDrop) {
    let (tx, rx) = mpsc::channel::<Result<Vec<Item>>>(SEGMENTS as usize);
    let mut tasks = Vec::new();
    match filter {
        Some(f) => {
            let from = match index {
                Some(i) => format!("{}.{}", quote_ident(Quote::Double, table), quote_ident(Quote::Double, i)),
                None => quote_ident(Quote::Double, table),
            };
            tasks.push(tokio::spawn(select_pages(client.clone(), format!("SELECT * FROM {from} WHERE {f}"), tx)));
        }
        None => {
            for seg in 0..SEGMENTS {
                tasks.push(tokio::spawn(scan_segment(client.clone(), table.to_string(), index.map(str::to_string), seg, tx.clone())));
            }
        }
    }
    (rx, AbortOnDrop(tasks))
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

impl DynamoSession {
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let (table, index) = target(&spec.table)?;
        let (table, index) = (table.to_string(), index.map(str::to_string));
        let keys = self.key_names(&table).await;
        let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());

        // Without asked-for columns, every attribute name (and its types)
        // first.
        let (names, types, inferred) = match &spec.columns {
            Some(n) => (n.clone(), BTreeMap::new(), false),
            None => {
                let (mut rx, _guard) = pages(&self.client, &table, index.as_deref(), filter);
                let mut seen: BTreeMap<String, u16> = BTreeMap::new();
                while let Some(page) = rx.recv().await {
                    for item in page? {
                        for (k, v) in item {
                            *seen.entry(k).or_default() |= type_bit(&v);
                        }
                    }
                }
                let mut names = keys.clone();
                names.extend(seen.keys().filter(|n| !keys.contains(n)).cloned());
                (names, seen, true)
            }
        };
        // The type names DynamoDB's codes: what tells a load that the
        // column's JSON carries this crate's tags.
        let cols: Vec<TransferColumn> = names
            .iter()
            .map(|n| TransferColumn { name: n.clone(), type_name: column_type(types.get(n).copied().unwrap_or(0)), nullable: !keys.contains(n) })
            .collect();
        sink.lock().map_err(lock_err)?.begin(&cols)?;
        let known: HashSet<&str> = names.iter().map(String::as_str).collect();

        let (mut rx, _guard) = pages(&self.client, &table, index.as_deref(), filter);
        let mut builder = BatchBuilder::new();
        while let Some(page) = rx.recv().await {
            let items = page?;
            if inferred {
                if let Some(a) = items.iter().flat_map(|i| i.keys()).find(|k| !known.contains(k.as_str())) {
                    return Err(Error::Query(format!(
                        "El atributo «{a}» apareció mientras se leía {table} (la tabla cambió durante la copia): volvé a copiarla."
                    )));
                }
            }
            let rows = items.iter().map(|i| item_row(i, &cols)).collect::<Result<Vec<_>>>()?;
            let mut s = sink.lock().map_err(lock_err)?;
            for r in rows {
                builder.push(r, &mut *s)?;
            }
        }
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
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se cargan datos.".into()));
        }
        if spec.table.kind == kinds::INDEX {
            return Err(Error::Unsupported("en DynamoDB se cargan ítems en una tabla, no en un índice".into()));
        }
        let table = spec.table.name.clone();
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let keys = crate::key_order(self.describe(&table).await?.key_schema());
        let Some(pk) = keys.first().cloned() else {
            return Err(Error::Query(format!("no se pudo leer la clave de la tabla {table}")));
        };
        if let Some(k) = keys.iter().find(|k| !names.contains(k)) {
            return Err(Error::Query(format!("La carga no trae el atributo «{k}», que es clave de la tabla {table}.")));
        }

        // Tags only in the JSON of columns read from DynamoDB: in anyone
        // else's JSON a `{"$B": …}` is a plain map.
        let tags: Vec<bool> = (0..names.len()).map(|i| columns.get(i).is_some_and(|c| dynamo_column(&c.type_name))).collect();

        let every = spec.commit_rows.max(1);
        let mut reported = 0u64;
        let mut tick = |written: u64, force: bool| {
            if written - reported >= every || (force && written > reported) {
                reported = written;
                progress(written);
            }
        };
        let mut w = Writer::new(self.client.clone(), table.clone(), pk);
        let r = load(&mut w, &names, &tags, &keys, source, &mut tick).await;
        if let Err(e) = r {
            // Nothing may be written once the load has returned: what was
            // sent is waited for, and what it wrote is reported.
            let left = w.drain().await;
            tick(w.written, true);
            let mut note = format!(" (quedaron escritos {} ítems en {table}", w.written);
            if left > 0 {
                note.push_str(&format!("; {left} pedidos a DynamoDB siguieron sin respuesta: podrían escribirse más ítems después"));
            }
            note.push(')');
            return Err(noted(e, &note));
        }
        tick(w.written, true);
        Ok(w.written)
    }
}

/// `e` with `note` after its message (same kind).
fn noted(e: Error, note: &str) -> Error {
    match e {
        Error::Query(m) => Error::Query(m + note),
        Error::State(m) => Error::State(m + note),
        Error::Unsupported(m) => Error::Unsupported(m + note),
        Error::Connect(m) => Error::Connect(m + note),
        Error::AuthFailed(m) => Error::AuthFailed(m + note),
        Error::Secrets(m) => Error::Secrets(m + note),
        Error::Cancelled => Error::Cancelled,
        e => Error::Query(format!("{e}{note}")),
    }
}

/// The rows of `source` in transactions; `tick` gets the rows written so
/// far.
async fn load(
    w: &mut Writer,
    names: &[String],
    tags: &[bool],
    keys: &[String],
    source: &mut dyn BatchSource,
    tick: &mut (dyn FnMut(u64, bool) + Send),
) -> Result<()> {
    let (mut items, mut item_keys, mut bytes) = (Vec::with_capacity(TX_ITEMS), Vec::with_capacity(TX_ITEMS), 0usize);
    let mut in_tx: HashSet<String> = HashSet::new();
    while let Some(batch) = source.next().await {
        for row in batch.rows {
            if row.len() != names.len() {
                return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
            }
            let item = row_item(names, &row, tags)?;
            let key = key_text(&item, keys);
            if let Some(k) = keys.iter().find(|k| !item.contains_key(*k)) {
                return Err(Error::Query(format!("Una fila no trae valor en «{k}», que es clave de la tabla (resto de la clave: {key}).")));
            }
            let size = item_size(&item);
            if size > MAX_ITEM {
                return Err(Error::Query(format!(
                    "El ítem con la clave {key} ocupa unos {} KB y DynamoDB admite hasta 400 KB por ítem.",
                    size.div_ceil(1024)
                )));
            }
            if items.len() >= TX_ITEMS || bytes + size > TX_BYTES {
                w.room(bytes).await?;
                tick(w.written, false);
                w.spawn(std::mem::take(&mut items), std::mem::take(&mut item_keys), bytes);
                (bytes, in_tx) = (0, HashSet::new());
            }
            if !in_tx.insert(key.clone()) {
                return Err(Error::Query(format!("La clave {key} está repetida en los datos: la carga nunca reemplaza ítems.")));
            }
            items.push(item);
            item_keys.push(key);
            bytes += size;
        }
        while let Some(r) = w.set.try_join_next() {
            w.done(r)?;
            tick(w.written, false);
        }
    }
    if !items.is_empty() {
        w.room(bytes).await?;
        tick(w.written, false);
        w.spawn(items, item_keys, bytes);
    }
    while let Some(r) = w.set.join_next().await {
        w.done(r)?;
        tick(w.written, false);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn s(v: &str) -> AttributeValue {
        AttributeValue::S(v.into())
    }
    fn n(v: &str) -> AttributeValue {
        AttributeValue::N(v.into())
    }
    fn m(kv: &[(&str, AttributeValue)]) -> AttributeValue {
        AttributeValue::M(kv.iter().map(|(k, v)| (k.to_string(), v.clone())).collect())
    }
    fn json(c: &str) -> AttributeValue {
        cell_attr(&Cell::Json(c.into()), true).unwrap().unwrap()
    }
    /// Written back from what was read.
    fn round(v: &AttributeValue) -> AttributeValue {
        cell_attr(&attr_cell(v).unwrap(), true).unwrap().unwrap()
    }

    #[test]
    fn numbers_keep_their_exact_text() {
        assert_eq!(plain_number("1.5E+3"), "1500");
        assert_eq!(plain_number("-1.25e-3"), "-0.00125");
        assert_eq!(plain_number("12345678901234567890123456789012345678"), "12345678901234567890123456789012345678");
        assert_eq!(plain_number("1E125"), format!("1{}", "0".repeat(125)));
        assert_eq!(plain_number("0.00E5"), "0");
        assert_eq!(plain_number("123.456e1"), "1234.56");
        assert_eq!(plain_number("7"), "7");
        assert_eq!(plain_number("1e999999999"), "1e999999999");
        assert_eq!(number_cell("42"), Cell::Int(42));
        assert_eq!(number_cell("-9223372036854775808"), Cell::Int(i64::MIN));
        assert_eq!(number_cell("18446744073709551615"), Cell::UInt(u64::MAX));
        assert_eq!(number_cell("0.1"), Cell::Decimal("0.1".into()));
        assert_eq!(number_cell("99999999999999999999999999999999999999"), Cell::Decimal("99999999999999999999999999999999999999".into()));
    }

    #[test]
    fn numbers_out_of_range_are_refused_in_spanish() {
        assert_eq!(check_number("1.50").unwrap(), "1.50");
        assert_eq!(check_number("-0").unwrap(), "0");
        assert!(check_number(&"9".repeat(38)).is_ok());
        assert!(check_number(&format!("{}000", "9".repeat(38))).is_ok());
        assert!(check_number("1E125").is_ok() && check_number("1E-130").is_ok());
        for bad in [&"9".repeat(39), "1E126", "1E-131", "1e999999999", "abc", "1.2.3", "--1"] {
            let e = check_number(bad).unwrap_err().to_string();
            assert!(e.contains("DynamoDB no guarda") || e.contains("no es un número"), "{bad}: {e}");
        }
        let e = row_item(&["f".into()], &[Cell::Float(1e300)], &[]).unwrap_err().to_string();
        assert!(e.contains("atributo «f»") && e.contains("38 dígitos"), "{e}");
        assert!(row_item(&["d".into()], &[Cell::Decimal(format!("0.{}", "1".repeat(39)))], &[]).is_err());
        assert!(cell_attr(&Cell::Float(f64::NAN), false).is_err());
        assert!(cell_attr(&Cell::Float(1e-200), false).is_err());
    }

    #[test]
    fn attributes_become_cells() {
        assert_eq!(attr_cell(&s("x")).unwrap(), Cell::Text("x".into()));
        assert_eq!(attr_cell(&AttributeValue::B(Blob::new(vec![0, 255]))).unwrap(), Cell::Bytes(vec![0, 255]));
        assert_eq!(attr_cell(&AttributeValue::Bool(true)).unwrap(), Cell::Bool(true));
        assert_eq!(attr_cell(&AttributeValue::Null(true)).unwrap(), Cell::Json("null".into()));
        assert_eq!(attr_cell(&AttributeValue::L(vec![n("1"), s("a")])).unwrap(), Cell::Json("[1,\"a\"]".into()));
        let map = m(&[("b", n("2")), ("a", AttributeValue::Bool(false))]);
        assert_eq!(attr_cell(&map).unwrap(), Cell::Json("{\"a\":false,\"b\":2}".into()));
        assert_eq!(attr_cell(&AttributeValue::Ss(vec!["x".into(), "y".into()])).unwrap(), Cell::Json("{\"$SS\":[\"x\",\"y\"]}".into()));
        assert_eq!(attr_cell(&AttributeValue::Ns(vec!["1".into(), "2.5".into()])).unwrap(), Cell::Json("{\"$NS\":[1,2.5]}".into()));
        assert_eq!(attr_cell(&AttributeValue::Bs(vec![Blob::new(vec![1, 0xab])])).unwrap(), Cell::Json("{\"$BS\":[\"01ab\"]}".into()));
    }

    #[test]
    fn nested_values_round_trip_without_loss() {
        // Problems seen: nested binaries cut at 1024 bytes, nested numbers
        // over 15 digits turned to text, sets turned to lists, NULL lost.
        let big = AttributeValue::B(Blob::new((0..2000).map(|i| (i % 251) as u8).collect::<Vec<u8>>()));
        let v = m(&[
            ("bin", big.clone()),
            ("num", n("12345678901234567890.123")),
            ("tiny", n("-0.001")),
            ("l", AttributeValue::L(vec![big.clone(), AttributeValue::Null(true), n("-0.5")])),
            ("ss", AttributeValue::Ss(vec!["x".into(), "y".into()])),
            ("ns", AttributeValue::Ns(vec!["1".into(), "99999999999999999999999999999999999999".into()])),
            ("bs", AttributeValue::Bs(vec![Blob::new(vec![1]), Blob::new(vec![2, 3])])),
            ("tag", m(&[("$B", s("not hex"))])),
            ("tag2", m(&[("$SS", AttributeValue::L(vec![s("a")]))])),
            ("uni", s("ñ \"q\" \\ \u{1F600}")),
        ]);
        let Cell::Json(text) = attr_cell(&v).unwrap() else { panic!() };
        assert!(!text.contains('…') && text.contains(&hex(&(0..2000).map(|i| (i % 251) as u8).collect::<Vec<_>>())));
        assert!(text.contains("12345678901234567890.123"), "{text}");
        assert_eq!(round(&v), v);
        for top in [
            AttributeValue::Ss(vec!["x".into(), "y".into()]),
            AttributeValue::Ns(vec!["3".into(), "0.1234567890123456789".into()]),
            AttributeValue::Bs(vec![Blob::new(vec![0, 255])]),
            AttributeValue::Null(true),
            AttributeValue::L(vec![]),
            m(&[]),
            m(&[("$M", m(&[("a", n("1"))]))]),
        ] {
            assert_eq!(round(&top), top);
        }
    }

    #[test]
    fn json_cells_keep_every_digit() {
        let v = json("{\"x\":123456789012345678901234567890,\"y\":0.1234567890123456789,\"z\":-1.5e3}");
        assert_eq!(v, m(&[("x", n("123456789012345678901234567890")), ("y", n("0.1234567890123456789")), ("z", n("-1500"))]));
        assert_eq!(json(" [1, \"a\", null, true, {\"k\": []}] "), AttributeValue::L(vec![n("1"), s("a"), AttributeValue::Null(true), AttributeValue::Bool(true), m(&[("k", AttributeValue::L(vec![]))])]));
        // Not JSON: text, as the insert script writes it.
        for t in ["{\"a\":01}", "[1,]", "{a:1}", "nul", "1 2", "\"x", "[\"\\", "{\"a\":1}x"] {
            assert_eq!(json(t), s(t), "{t}");
        }
        // Tags that don't hold a valid value stay maps.
        assert_eq!(json("{\"$SS\":[\"a\",\"a\"]}"), m(&[("$SS", AttributeValue::L(vec![s("a"), s("a")]))]));
        assert_eq!(json("{\"$B\":\"xyz\"}"), m(&[("$B", s("xyz"))]));
        assert!(cell_attr(&Cell::Json("{\"x\":1e400}".into()), false).is_err());
    }

    #[test]
    fn rows_become_items() {
        let names: Vec<String> = ["pk", "n", "f", "d", "b", "j", "gone", "it's"].iter().map(|s| s.to_string()).collect();
        let row = vec![
            Cell::Text("a".into()),
            Cell::Int(-5),
            Cell::Float(1.5),
            Cell::Decimal("12345678901234567890.5".into()),
            Cell::Bytes(vec![1, 2]),
            Cell::Json("{\"x\":[1,\"y\",null,true]}".into()),
            Cell::Null,
            Cell::DateTime("2024-01-02 03:04:05".into()),
        ];
        let item = row_item(&names, &row, &[true; 8]).unwrap();
        assert_eq!(item.len(), 7);
        assert_eq!(item["n"], n("-5"));
        assert_eq!(item["f"], n("1.5"));
        assert_eq!(item["d"], n("12345678901234567890.5"));
        assert_eq!(item["b"], AttributeValue::B(Blob::new(vec![1, 2])));
        assert_eq!(item["j"], m(&[("x", AttributeValue::L(vec![n("1"), s("y"), AttributeValue::Null(true), AttributeValue::Bool(true)]))]));
        assert_eq!(item["it's"], s("2024-01-02 03:04:05"));
        assert!(!item.contains_key("gone"));
        assert!(row_item(&names[..1], &[Cell::Null], &[]).unwrap().is_empty());
        // And back: the cells read are the cells written.
        let cols: Vec<TransferColumn> = names.iter().map(|n| TransferColumn { name: n.clone(), type_name: String::new(), nullable: true }).collect();
        let back = item_row(&item, &cols).unwrap();
        assert_eq!(back[1], Cell::Int(-5));
        assert_eq!(back[2], Cell::Decimal("1.5".into()));
        assert_eq!(back[3], Cell::Decimal("12345678901234567890.5".into()));
        assert_eq!(back[4], Cell::Bytes(vec![1, 2]));
        assert_eq!(back[6], Cell::Null);
    }

    #[test]
    fn sizes_and_keys() {
        let item = Item::from([("id".to_string(), s("k1")), ("n".to_string(), n("1.50")), ("l".to_string(), AttributeValue::L(vec![s("ab")]))]);
        // id 2+2, n 1+(2 digits → 1)+1, l 1+3+1+2.
        assert_eq!(item_size(&item), 4 + 3 + 7);
        assert_eq!(key_text(&item, &["id".into(), "n".into()]), "id=\"k1\", n=1.5");
        let big = Item::from([("id".to_string(), s(&"x".repeat(MAX_ITEM)))]);
        assert!(item_size(&big) > MAX_ITEM);
    }

    #[test]
    fn json_from_other_engines_keeps_its_dollar_keys() {
        // A PostgreSQL jsonb or MySQL JSON value that happens to look like
        // a tag: the map it says, whatever its value.
        let plain = |c: &str| cell_attr(&Cell::Json(c.into()), false).unwrap().unwrap();
        assert_eq!(plain("{\"$M\":{\"k\":1}}"), m(&[("$M", m(&[("k", n("1"))]))]));
        assert_eq!(plain("{\"$B\":\"00ff\"}"), m(&[("$B", s("00ff"))]));
        assert_eq!(plain("{\"$B\":\"zz\"}"), m(&[("$B", s("zz"))]));
        assert_eq!(plain("{\"$SS\":[\"x\",\"y\"]}"), m(&[("$SS", AttributeValue::L(vec![s("x"), s("y")]))]));
        assert_eq!(plain("{\"$NS\":[1]}"), m(&[("$NS", AttributeValue::L(vec![n("1")]))]));
        assert_eq!(plain("[{\"$BS\":[\"01\"]}]"), AttributeValue::L(vec![m(&[("$BS", AttributeValue::L(vec![s("01")]))])]));
        // The same text read from DynamoDB is the tagged type.
        assert_eq!(json("{\"$B\":\"00ff\"}"), AttributeValue::B(Blob::new(vec![0, 255])));
        assert_eq!(json("{\"$M\":{\"k\":1}}"), m(&[("k", n("1"))]));
        // Per column: only the ones read from DynamoDB.
        let names: Vec<String> = vec!["a".into(), "b".into()];
        let cell = Cell::Json("{\"$SS\":[\"x\"]}".into());
        let item = row_item(&names, &[cell.clone(), cell], &[true, false]).unwrap();
        assert_eq!(item["a"], AttributeValue::Ss(vec!["x".into()]));
        assert_eq!(item["b"], m(&[("$SS", AttributeValue::L(vec![s("x")]))]));
    }

    #[test]
    fn read_columns_name_dynamodb_types() {
        assert_eq!(column_type(type_bit(&s("x")) | type_bit(&m(&[]))), "S | M");
        assert_eq!(column_type(0), TYPE_CODES.join(" | "));
        for t in ["S", "M", "S | M", "SS|NS", &column_type(0)] {
            assert!(dynamo_column(t), "{t}");
        }
        // Other engines' types (and no type) aren't DynamoDB's.
        for t in ["", "jsonb", "json", "JSON", "object", "s", "S | jsonb", "S |", "string | object", "map<string,string>"] {
            assert!(!dynamo_column(t), "{t}");
        }
    }

    #[test]
    fn retries_keep_the_token_only_when_the_try_may_have_been_written() {
        assert_eq!(retry_of(Some("InternalServerError")), Retry::Same);
        assert_eq!(retry_of(Some("TransactionInProgressException")), Retry::Same);
        assert_eq!(retry_of(Some("ThrottlingException")), Retry::Fresh);
        assert_eq!(retry_of(Some("ProvisionedThroughputExceededException")), Retry::Fresh);
        assert_eq!(retry_of(Some("ValidationException")), Retry::No);
        assert_eq!(retry_of(None), Retry::No);
        let tokens: HashSet<String> = (0..1000).map(|_| request_token()).collect();
        assert_eq!(tokens.len(), 1000);
        assert!(tokens.iter().all(|t| t.len() == 32 && t.bytes().all(|b| b.is_ascii_hexdigit())));
    }

    #[test]
    fn errors_say_what_was_written() {
        let e = noted(Error::Query("falló".into()), " (quedaron escritos 5 ítems en t)");
        assert!(matches!(&e, Error::Query(m) if m == "falló (quedaron escritos 5 ítems en t)"), "{e:?}");
        assert!(matches!(noted(Error::Unsupported("x".into()), "!"), Error::Unsupported(m) if m == "x!"));
        assert!(matches!(noted(Error::Cancelled, "!"), Error::Cancelled));
    }

    #[test]
    fn a_cancelled_load_waits_for_its_tasks_without_tokio() {
        let alive = Arc::new(Alive::default());
        let guards: Vec<AliveGuard> = (0..3).map(|_| AliveGuard::new(&alive)).collect();
        assert_eq!(alive.count(), 3);
        assert_eq!(alive.wait(Duration::from_millis(20)), 3);
        let t = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(50));
            drop(guards);
        });
        assert_eq!(alive.wait(Duration::from_secs(10)), 0);
        t.join().unwrap();
    }

    /// Cancelled while the runtime shuts down (the app closing): the tasks
    /// are dropped by the shutdown, and the writer's drop neither panics
    /// nor waits for [`DRAIN`].
    #[test]
    fn a_writer_dropped_during_runtime_shutdown_neither_panics_nor_hangs() {
        let rt = tokio::runtime::Builder::new_multi_thread().worker_threads(2).enable_all().build().unwrap();
        let alive = Arc::new(Alive::default());
        let a = alive.clone();
        rt.spawn(async move {
            let conf = aws_sdk_dynamodb::Config::builder()
                .behavior_version(aws_sdk_dynamodb::config::BehaviorVersion::latest())
                .http_client(plain_http_client())
                .build();
            let mut w = Writer::new(Client::from_conf(conf), "t".into(), "id".into());
            w.alive = a;
            // A request that never answers.
            let guard = AliveGuard::new(&w.alive);
            w.set.spawn(async move {
                let _guard = guard;
                std::future::pending::<()>().await;
                Ok((0, 0))
            });
            // Dropped with the task by the shutdown.
            std::future::pending::<()>().await;
            drop(w);
        });
        // Building the client can take a while: wait for the request to be up.
        let up = std::time::Instant::now();
        while alive.count() == 0 && up.elapsed() < Duration::from_secs(20) {
            std::thread::sleep(Duration::from_millis(20));
        }
        assert_eq!(alive.count(), 1);
        let start = std::time::Instant::now();
        rt.shutdown_timeout(Duration::from_secs(3));
        assert!(start.elapsed() < Duration::from_secs(3), "{:?}", start.elapsed());
        assert_eq!(alive.count(), 0);
    }

    /// A plain-HTTP client: the default HTTPS one loads the OS root
    /// certificates, and debug builds panic where none can be read.
    fn plain_http_client() -> aws_sdk_dynamodb::config::SharedHttpClient {
        aws_smithy_http_client::Builder::new().build_http()
    }

    /// Lets the first request through to the server and turns its answer
    /// into an internal error: written, but the client can't know.
    #[derive(Debug)]
    struct LoseFirstAnswer(std::sync::atomic::AtomicUsize);

    impl aws_sdk_dynamodb::config::Intercept for LoseFirstAnswer {
        fn name(&self) -> &'static str {
            "LoseFirstAnswer"
        }
        fn modify_before_deserialization(
            &self,
            ctx: &mut aws_sdk_dynamodb::config::interceptors::BeforeDeserializationInterceptorContextMut<'_>,
            _rc: &aws_sdk_dynamodb::config::RuntimeComponents,
            _cfg: &mut aws_sdk_dynamodb::config::ConfigBag,
        ) -> std::result::Result<(), Box<dyn std::error::Error + Send + Sync>> {
            if self.0.fetch_add(1, Ordering::SeqCst) == 0 {
                let r = ctx.response_mut();
                *r.status_mut() = 500u16.try_into().unwrap();
                r.headers_mut().insert("x-amzn-ErrorType", "InternalServerError");
            }
            Ok(())
        }
    }

    /// Against DynamoDB Local (`DBINE_TEST_DYNAMODB_URL`, as in
    /// `tests/transfer.rs`).
    #[tokio::test(flavor = "multi_thread")]
    #[ignore]
    async fn a_retry_after_a_lost_answer_is_not_a_repeated_key() {
        use aws_sdk_dynamodb::config::{retry::RetryConfig, BehaviorVersion, Credentials, Region};
        use aws_sdk_dynamodb::types::*;
        const T: &str = "dbine_xfer_token";
        let url = std::env::var("DBINE_TEST_DYNAMODB_URL").unwrap_or_else(|_| "http://localhost:25300".into());
        let base = aws_sdk_dynamodb::Config::builder()
            .behavior_version(BehaviorVersion::latest())
            .region(Region::new("us-east-1"))
            .endpoint_url(&url)
            .credentials_provider(Credentials::new("dummy", "dummy", None, None, "t"))
            .http_client(plain_http_client());
        let plain = Client::from_conf(base.clone().build());
        let lossy = Client::from_conf(base.retry_config(RetryConfig::disabled()).interceptor(LoseFirstAnswer(Default::default())).build());
        let _ = plain.delete_table().table_name(T).send().await;
        plain
            .create_table()
            .table_name(T)
            .billing_mode(BillingMode::PayPerRequest)
            .attribute_definitions(AttributeDefinition::builder().attribute_name("id").attribute_type(ScalarAttributeType::S).build().unwrap())
            .key_schema(KeySchemaElement::builder().attribute_name("id").key_type(KeyType::Hash).build().unwrap())
            .send()
            .await
            .unwrap();
        let items: Vec<Item> = (0..3).map(|i| Item::from([("id".to_string(), s(&format!("k{i}")))])).collect();
        let keys: Vec<String> = (0..3).map(|i| format!("id=\"k{i}\"")).collect();
        let r = put_tx(lossy, T.into(), "id".into(), items, keys, 7, Arc::default()).await;
        let n = plain.scan().table_name(T).select(Select::Count).send().await.unwrap().count;
        let _ = plain.delete_table().table_name(T).send().await;
        assert_eq!(r.unwrap(), (3, 7));
        assert_eq!(n, 3);
    }
}
