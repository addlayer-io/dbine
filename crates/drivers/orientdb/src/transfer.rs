//! Bulk transfer (see `dbine_driver::transfer`) for OrientDB.
//!
//! A "table" is a class (document, vertex or edge class); only its own
//! records are read and written (`@class = …`), not those of its
//! subclasses, which are tables of their own.
//!
//! Reading: `SELECT <columns>, @rid, @class FROM C WHERE @class = 'C' AND
//! @rid > last AND @rid <= end ORDER BY @rid LIMIT n` over the read-only
//! `/query` endpoint (the server refuses anything that writes, whatever
//! the filter says), paged by record id (the server seeks to the id, so
//! every page costs the same), the next page requested while the current
//! one is handed over. Pages are sized by bytes, record by record: a
//! probe (`@rid, @size` of the next [`PAGE`] records, a few dozen bytes
//! each) says how large each one is stored, and a page takes the records
//! whose expected reply fits in [`PAGE_BYTES`] (see [`fit`]), so a run of
//! small records can't open the door to a page of large ones. A reply
//! that still grows past [`REPLY_CAP`] while it arrives is dropped and
//! its window cut again smaller. A window read up to its `LIMIT` but
//! short of its end (records inserted inside it meanwhile) goes on from
//! its last row, so no record that was there is skipped. Every row
//! is checked: its `@rid` after the one before, inside the page's window,
//! and of the class read, so a filter can't bring duplicates or rows of
//! another class.
//! Each column is projected by its declared type so nothing is lost on
//! the way: `DATETIME` as `format('yyyy-MM-dd HH:mm:ss.SSSXXX')` (the
//! instant with its offset), `DATE` as `yyyy-MM-dd`, `DECIMAL` / `FLOAT` /
//! `DOUBLE` through `asString()` (exact digits, and NaN / infinities that
//! JSON can't carry), `BINARY` as its base64 decoded whole. `LINK`s are
//! `#c:p` text; embedded documents, lists, sets and maps (and link lists)
//! are JSON. Undeclared (schemaless) fields keep their JSON type. Without
//! asked-for columns they're the class's properties plus the fields of a
//! sample (their names and type names only, never their values), with
//! `out` / `in` first for edges. A filter is an OrientDB SQL condition,
//! one closed condition without comments (see [`check_filter`]).
//!
//! Loading: SQL scripts `BEGIN; …; COMMIT;` over `POST /batch/{db}`, one
//! transaction per [`CHUNK`] records (at most `commit_rows` and
//! [`MAX_SCRIPT`] bytes of request body), [`IN_FLIGHT`] scripts at once
//! (edges one at a time: each one updates both of its vertices). A row
//! means what it means in the insert script: `CREATE VERTEX C SET …`,
//! `CREATE EDGE C FROM #out TO #in SET …`, `INSERT INTO C SET …`; `@`
//! fields are left out, and so are the graph bookkeeping fields of
//! vertices and edges (see [`bookkeeping`]). NULLs are written as `x =
//! null` (leaving them out would store a property's default). Values are
//! typed: dates through `date(…, 'yyyy-MM-dd HH:mm:ss.SSS[XXX]')` (keeps
//! the milliseconds and the offset), decimals through `asDecimal()`,
//! binaries as base64 (a `BINARY` property decodes it), JSON as embedded
//! maps / lists; text into `STRING` properties. When a load fails, the
//! scripts still in flight are awaited before returning, so nothing
//! commits after the error is reported.

use crate::{ident, request, OrientSession, EDGE, VERTEX};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{kinds, Error, Result};
use reqwest::Method;
use serde_json::{json, Value};
use std::collections::HashMap;
use tokio::task::JoinSet;

/// Records per read page (and per size probe), at most.
const PAGE: usize = 5_000;
/// Reply bytes a page aims for. A reply is held as text, then parsed,
/// while the next one arrives: about 5× this in memory per table, well
/// under the ~32 MiB per table of the design.
const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Reply bytes past which a page of several records is dropped while it
/// arrives and cut again smaller ([`Pager::overflowed`]): records whose
/// reply grows more per stored byte than the ones before (control
/// characters escaped as `\u0001` after plain text) can't take a page
/// past this.
const REPLY_CAP: usize = 2 * PAGE_BYTES;
/// Reply bytes per stored byte (`@size`) assumed at first: base64 of a
/// binary is 4/3, JSON text about 1. Raised by what the replies show
/// ([`calibrate`]) and doubled by a page that passes [`REPLY_CAP`],
/// never lowered.
const FACTOR_MIN: usize = 2;
/// The most reply bytes per stored byte assumed (escaped control
/// characters are 6).
const FACTOR_MAX: usize = 16;
/// Reply bytes per column and per record besides the values (names,
/// quotes, the record id and class).
const PER_COLUMN: usize = 24;
const PER_RECORD: usize = 96;
/// Records sampled for the fields of a schemaless class.
const SAMPLE: usize = 100;
/// Fields whose types one sample query asks for.
const TYPES_PER_QUERY: usize = 50;
/// The page's record class, next to the columns.
const CLASS: &str = "dbineClass";
/// The probe's stored size of a record.
const SIZE: &str = "dbineSize";
/// Records per `/batch` script (one transaction each).
pub(crate) const CHUNK: u64 = 1_000;
/// A script's request body (its JSON encoding) closes before this many
/// bytes. It is our bound, not the server's (a 1.7 MB body was accepted
/// in testing): it keeps [`IN_FLIGHT`] requests, and the transactions the
/// server holds for them, around a few MB.
pub(crate) const MAX_SCRIPT: usize = 900_000;
/// Scripts in flight at once.
pub(crate) const IN_FLIGHT: usize = 4;
/// The page's record id, next to the columns.
const RID: &str = "dbineRid";

/// How a column is read, from its declared type.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Undeclared: by the JSON type.
    Any,
    Bool,
    Int,
    Float,
    Decimal,
    Text,
    Binary,
    Date,
    DateTime,
    Link,
    Json,
}

impl Kind {
    pub(crate) fn of(ty: &str) -> Kind {
        match ty.to_ascii_uppercase().as_str() {
            "BOOLEAN" => Kind::Bool,
            "INTEGER" | "SHORT" | "LONG" | "BYTE" => Kind::Int,
            "FLOAT" | "DOUBLE" => Kind::Float,
            "DECIMAL" => Kind::Decimal,
            "STRING" => Kind::Text,
            "BINARY" => Kind::Binary,
            "DATE" => Kind::Date,
            "DATETIME" => Kind::DateTime,
            "LINK" => Kind::Link,
            "EMBEDDED" | "EMBEDDEDLIST" | "EMBEDDEDSET" | "EMBEDDEDMAP" | "LINKLIST" | "LINKSET" | "LINKMAP" | "LINKBAG" => Kind::Json,
            _ => Kind::Any,
        }
    }
}

/// A class's declared properties (inherited ones too): name → (type, nullable).
pub(crate) fn properties(all: &[Value], class: &str) -> HashMap<String, (String, bool)> {
    let mut out = HashMap::new();
    let mut stack = vec![class.to_string()];
    let mut seen: Vec<String> = Vec::new();
    while let Some(n) = stack.pop() {
        if seen.contains(&n) {
            continue;
        }
        let Some(c) = all.iter().find(|c| c.get("name").and_then(Value::as_str) == Some(n.as_str())) else {
            seen.push(n);
            continue;
        };
        for p in c.get("properties").and_then(Value::as_array).into_iter().flatten() {
            let Some(name) = p.get("name").and_then(Value::as_str) else { continue };
            let ty = p.get("type").and_then(Value::as_str).unwrap_or_default().to_string();
            let nullable = !p.get("notNull").and_then(Value::as_bool).unwrap_or(false) && !p.get("mandatory").and_then(Value::as_bool).unwrap_or(false);
            // A subclass's own declaration wins over its parents'.
            out.entry(name.to_string()).or_insert((ty, nullable));
        }
        if let Some(s) = c.get("superClasses").and_then(Value::as_array) {
            stack.extend(s.iter().filter_map(Value::as_str).map(str::to_string));
        }
        if let Some(s) = c.get("superClass").and_then(Value::as_str).filter(|s| !s.is_empty()) {
            stack.push(s.to_string());
        }
        seen.push(n);
    }
    out
}

/// The projection that reads a column without loss.
pub(crate) fn projection(name: &str, kind: Kind, alias: &str) -> String {
    let f = if name.starts_with('@') && name[1..].chars().all(|c| c.is_ascii_alphanumeric()) { name.to_string() } else { ident(name) };
    let e = match kind {
        // With the offset: a wall-clock time alone repeats in a DST
        // fall-back hour of the database's time zone.
        Kind::DateTime => format!("{f}.format('yyyy-MM-dd HH:mm:ss.SSSXXX')"),
        Kind::Date => format!("{f}.format('yyyy-MM-dd')"),
        Kind::Decimal | Kind::Float => format!("{f}.asString()"),
        _ => f,
    };
    format!("{e} AS {alias}")
}

/// A filter must be one condition: its parentheses balanced outside
/// quoted text, no `;` and no comment (`/* */`, which OrientDB accepts,
/// and `--` / `//`, which other dialects do). Otherwise `… AND (a=1) OR
/// (a=1) …`, or the same hidden in comments the check can't see through,
/// would slip out of the class and record-id bounds around it.
pub(crate) fn check_filter(f: &str) -> Result<()> {
    let bad = || Err(Error::Query(format!("el filtro no es una condición cerrada (paréntesis o comillas sin cerrar, o un `;`): {f}")));
    let (mut depth, mut quote): (i64, Option<char>) = (0, None);
    let mut chars = f.chars().peekable();
    while let Some(c) = chars.next() {
        match (quote, c) {
            (Some(_), '\\') => {
                chars.next();
            }
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '"' | '`') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => {
                depth -= 1;
                if depth < 0 {
                    return bad();
                }
            }
            (None, ';') => return bad(),
            (None, '/') if matches!(chars.peek(), Some('*' | '/')) => {
                return Err(Error::Query(format!("el filtro no puede llevar comentarios (`/*`, `//`): {f}")));
            }
            (None, '-') if chars.peek() == Some(&'-') => {
                return Err(Error::Query(format!("el filtro no puede llevar comentarios (`--`; para restar un negativo, separá los signos: `a - -1`): {f}")));
            }
            _ => {}
        }
    }
    if depth != 0 || quote.is_some() {
        return bad();
    }
    Ok(())
}

/// A record id's (cluster, position), to check that paging advances.
pub(crate) fn rid_key(s: &str) -> Option<(i64, i64)> {
    let (c, p) = rid(s)?[1..].split_once(':')?;
    Some((c.parse().ok()?, p.parse().ok()?))
}

/// Reply bytes a record stored in `size` bytes is expected to take, with
/// `columns` columns, at `factor` reply bytes per stored byte.
pub(crate) fn estimate(size: usize, factor: usize, columns: usize) -> usize {
    size.saturating_mul(factor).saturating_add(PER_COLUMN * columns + PER_RECORD)
}

/// How many of the next records (their stored sizes, in order) one page
/// takes: as many as fit in [`PAGE_BYTES`], at least one (a record larger
/// than that comes alone), at most `max`. Each record is counted by its
/// own size, so a size change inside a class can't overflow a page.
pub(crate) fn fit(sizes: impl IntoIterator<Item = usize>, factor: usize, columns: usize, max: usize) -> usize {
    let (mut n, mut bytes) = (0usize, 0usize);
    for s in sizes.into_iter().take(max.max(1)) {
        let e = estimate(s, factor, columns);
        if n > 0 && bytes.saturating_add(e) > PAGE_BYTES {
            break;
        }
        bytes = bytes.saturating_add(e);
        n += 1;
    }
    n
}

/// The reply bytes per stored byte to assume from now on, after a reply
/// of `reply_bytes` for records stored in `stored` bytes: never lower
/// than before, within [`FACTOR_MIN`]..=[`FACTOR_MAX`].
pub(crate) fn calibrate(factor: usize, reply_bytes: usize, stored: usize) -> usize {
    if stored == 0 {
        return factor;
    }
    factor.max(reply_bytes.div_ceil(stored)).clamp(FACTOR_MIN, FACTOR_MAX)
}

/// The class and record-id bounds (and the filter) of a read.
fn bounds(class: &str, filter: Option<&str>, after: &str) -> String {
    let mut q = format!(" FROM {} WHERE @class = {}", ident(class), crate::ddl::string(class));
    if let Some(f) = filter {
        q.push_str(&format!(" AND ({f})"));
    }
    q.push_str(&format!(" AND @rid > {after}"));
    q
}

/// The size probe: the record ids, classes and stored sizes of the next
/// [`PAGE`] records (a few dozen reply bytes each).
pub(crate) fn probe_query(class: &str, filter: Option<&str>, after: &str) -> String {
    format!("SELECT @rid AS {RID}, @size AS {SIZE}, @class AS {CLASS}{} ORDER BY @rid LIMIT {PAGE}", bounds(class, filter, after))
}

/// One page's query: the records after `after` up to `end`, at most `limit`.
pub(crate) fn page_query(class: &str, projections: &[String], filter: Option<&str>, after: &str, end: &str, limit: usize) -> String {
    format!(
        "SELECT {}, @rid AS {RID}, @class AS {CLASS}{} AND @rid <= {end} ORDER BY @rid LIMIT {limit}",
        projections.join(", "),
        bounds(class, filter, after)
    )
}

/// A reply row's record id (checked) and class.
fn rid_and_class(r: &[(String, Value)]) -> (Option<&str>, Option<&str>) {
    let get = |k: &str| r.iter().find(|(x, _)| x == k).and_then(|(_, v)| v.as_str());
    (get(RID).and_then(rid), get(CLASS))
}

/// Checks one reply's rows, in order: each record id after the one
/// before (and after `after`), none past `end`, all of class `class`.
/// Returns the last one's. A filter that slips out of the query's bounds
/// shows here, as a repeated or out-of-order id or a foreign class,
/// instead of as silent duplicates.
pub(crate) fn check_rows<'a>(rows: impl IntoIterator<Item = (Option<&'a str>, Option<&'a str>)>, class: &str, after: (i64, i64), end: Option<(i64, i64)>) -> Result<(i64, i64)> {
    let mut last = after;
    for (id, c) in rows {
        let Some(id) = id else {
            return Err(Error::Query("la página leída no trae el @rid de sus registros".into()));
        };
        match rid_key(id) {
            Some(k) if k > last && end.is_none_or(|e| k <= e) => last = k,
            _ => return Err(Error::Query(format!("la lectura por @rid no avanzó o se salió de su rango ({id}); revisá el filtro"))),
        }
        if c != Some(class) {
            return Err(Error::Query(format!("la lectura trajo un registro de otra clase ({}, {id}); revisá el filtro", c.unwrap_or("?"))));
        }
    }
    Ok(last)
}

/// A probed record: its id, key and stored size.
type Probed = (String, (i64, i64), usize);

/// One page to read: the probed records after the previous page's end,
/// never empty. The page asks for as many records as it has (`LIMIT`) up
/// to its last one's id; records inserted inside the window while it's
/// read can push some of its own out of that `LIMIT`, so a full page that
/// stops short of the end is followed by the rest of the window
/// ([`Window::rest`]) instead of jumping past it.
pub(crate) struct Window {
    records: Vec<Probed>,
}

impl Window {
    fn new(records: Vec<Probed>) -> Option<Window> {
        (!records.is_empty()).then_some(Window { records })
    }

    /// The last record's id and key: the page's upper bound.
    fn end(&self) -> (&str, (i64, i64)) {
        let last = &self.records[self.records.len() - 1];
        (&last.0, last.1)
    }

    fn limit(&self) -> usize {
        self.records.len()
    }

    fn stored(&self) -> usize {
        self.records.iter().fold(0usize, |a, r| a.saturating_add(r.2))
    }

    /// What's left of the window after a page of `got` rows whose last
    /// one was `last`: nothing when the page came back short (the records
    /// missing from it are gone) or reached the end; otherwise the
    /// window's records past `last`, still to read.
    pub(crate) fn rest(self, got: usize, last: (i64, i64)) -> Option<Window> {
        if got < self.limit() || last >= self.end().1 {
            return None;
        }
        Window::new(self.records.into_iter().filter(|r| r.1 > last).collect())
    }

    /// The reply bytes a page of this window may take before it's
    /// dropped and re-cut smaller ([`REPLY_CAP`]); a single record has
    /// no smaller page, so it has no cap.
    fn cap(&self) -> Option<usize> {
        (self.limit() > 1).then_some(REPLY_CAP)
    }
}

/// The size probes of one read and the pages cut from them.
pub(crate) struct Pager<'a> {
    class: &'a str,
    filter: Option<&'a str>,
    columns: usize,
    /// Probed records not paged yet.
    probed: std::collections::VecDeque<Probed>,
    /// The last probed record (the next probe starts after it).
    probed_to: (String, (i64, i64)),
    /// The last probe came back short: nothing after it.
    exhausted: bool,
    factor: usize,
    first: bool,
    /// At most this many records in the next page (after one overflowed).
    next_max: Option<usize>,
}

impl<'a> Pager<'a> {
    pub(crate) fn new(class: &'a str, filter: Option<&'a str>, columns: usize) -> Self {
        Pager {
            class,
            filter,
            columns,
            probed: Default::default(),
            probed_to: ("#-1:-1".into(), (-1, -1)),
            exhausted: false,
            factor: FACTOR_MIN,
            first: true,
            next_max: None,
        }
    }

    /// Records its probe reply's rows (checked like a page's).
    fn take_probe(&mut self, rows: &[Vec<(String, Value)>]) -> Result<()> {
        self.exhausted = rows.len() < PAGE;
        check_rows(rows.iter().map(|r| rid_and_class(r)), self.class, self.probed_to.1, None)?;
        for r in rows {
            let id = rid_and_class(r).0.unwrap_or_default().to_string();
            let key = rid_key(&id).unwrap_or_default();
            // A size the server doesn't give: as if it filled a page alone.
            let size = r.iter().find(|(k, _)| k == SIZE).and_then(|(_, v)| v.as_u64()).map_or(PAGE_BYTES, |s| s as usize);
            self.probed_to = (id.clone(), key);
            self.probed.push_back((id, key, size));
        }
        Ok(())
    }

    /// The next page, cut from the probed records (the first page takes
    /// one record, to calibrate the reply bytes per stored byte).
    pub(crate) fn cut(&mut self) -> Option<Window> {
        let max = if self.first { 1 } else { self.next_max.take().unwrap_or(PAGE) };
        let n = fit(self.probed.iter().map(|p| p.2), self.factor, self.columns, max);
        if n == 0 {
            return None;
        }
        self.first = false;
        Window::new(self.probed.drain(..n).collect())
    }

    /// A page whose reply passed [`REPLY_CAP`] (its records answer with
    /// more bytes per stored byte than assumed so far): its records go
    /// back, to be cut again at twice the factor, or, at the highest
    /// factor, into half as many records.
    pub(crate) fn overflowed(&mut self, w: Window) {
        let n = w.limit();
        for r in w.records.into_iter().rev() {
            self.probed.push_front(r);
        }
        if self.factor < FACTOR_MAX {
            self.factor = (self.factor * 2).min(FACTOR_MAX);
        } else {
            self.next_max = Some((n / 2).max(1));
        }
    }
}

/// A record id (`#c:p`), checked: it goes into statements as is.
pub(crate) fn rid(s: &str) -> Option<&str> {
    let (c, p) = s.strip_prefix('#')?.split_once(':')?;
    let num = |x: &str| {
        let x = x.strip_prefix('-').unwrap_or(x);
        !x.is_empty() && x.bytes().all(|b| b.is_ascii_digit())
    };
    (num(c) && num(p)).then_some(s)
}

/// `@type`, `@version` and `@fieldTypes` left out, at every depth.
fn clean(v: &Value) -> Value {
    match v {
        Value::Object(o) => Value::Object(o.iter().filter(|(k, _)| !crate::HIDDEN.contains(&k.as_str())).map(|(k, v)| (k.clone(), clean(v))).collect()),
        Value::Array(a) => Value::Array(a.iter().map(clean).collect()),
        other => other.clone(),
    }
}

fn number(n: &serde_json::Number) -> Cell {
    if let Some(i) = n.as_i64() {
        Cell::Int(i)
    } else if let Some(u) = n.as_u64() {
        Cell::UInt(u)
    } else {
        Cell::Float(n.as_f64().unwrap_or(f64::NAN))
    }
}

/// A JSON value by its own type.
fn any(v: &Value) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(*b),
        Value::Number(n) => number(n),
        Value::String(s) => Cell::Text(s.clone()),
        other => Cell::Json(clean(other).to_string()),
    }
}

/// A read value as a cell, by its column's kind.
pub(crate) fn to_cell(kind: Kind, v: &Value) -> Cell {
    match (kind, v) {
        (_, Value::Null) => Cell::Null,
        (Kind::Bool, Value::Bool(b)) => Cell::Bool(*b),
        (Kind::Int, Value::Number(n)) => number(n),
        (Kind::Float, Value::String(s)) => s.parse::<f64>().map(Cell::Float).unwrap_or_else(|_| Cell::Text(s.clone())),
        (Kind::Float, Value::Number(n)) => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        (Kind::Decimal, Value::String(s)) => plain_decimal(s).map(Cell::Decimal).unwrap_or_else(|| Cell::Text(s.clone())),
        (Kind::Decimal, Value::Number(n)) => plain_decimal(&n.to_string()).map_or_else(|| number(n), Cell::Decimal),
        (Kind::Text, Value::String(s)) => Cell::Text(s.clone()),
        (Kind::Binary, Value::String(s)) => base64_decode(s).map(Cell::Bytes).unwrap_or_else(|| Cell::Text(s.clone())),
        (Kind::Date, Value::String(s)) => Cell::Date(s.clone()),
        // Read with its offset (`…SSSXXX`): the exact instant.
        (Kind::DateTime, Value::String(s)) => match timestamp(s) {
            Some((t, Some(z))) => Cell::DateTimeTz(format!("{t}{}", if z == "Z" { "+00:00" } else { z.as_str() })),
            Some((t, None)) => Cell::DateTime(t),
            None => Cell::Text(s.clone()),
        },
        (Kind::Link, Value::String(s)) => Cell::Text(s.clone()),
        // A link the server expanded into its record: its id.
        (Kind::Link, Value::Object(o)) if o.get("@rid").is_some_and(Value::is_string) => Cell::Text(o["@rid"].as_str().unwrap_or_default().to_string()),
        (Kind::Json, Value::Object(_) | Value::Array(_)) => Cell::Json(clean(v).to_string()),
        _ => any(v),
    }
}

/// Java's `BigDecimal.toString()` (`1.5E+7`, `-2E-3`) as plain digits.
pub(crate) fn plain_decimal(s: &str) -> Option<String> {
    let s = s.trim();
    let (neg, body) = match s.strip_prefix('-') {
        Some(b) => (true, b),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mant, exp) = match body.find(['e', 'E']) {
        Some(i) => (&body[..i], body[i + 1..].parse::<i64>().ok()?),
        None => (body, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty() || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    if exp == 0 {
        return Some(format!("{}{}{}{}", if neg { "-" } else { "" }, if int.is_empty() { "0" } else { int }, if frac.is_empty() { "" } else { "." }, frac));
    }
    let digits = format!("{int}{frac}");
    let point = int.len() as i64 + exp;
    let (i, f) = if point <= 0 {
        ("0".to_string(), format!("{}{digits}", "0".repeat((-point) as usize)))
    } else if point as usize >= digits.len() {
        (format!("{digits}{}", "0".repeat(point as usize - digits.len())), String::new())
    } else {
        (digits[..point as usize].to_string(), digits[point as usize..].to_string())
    };
    let i = i.trim_start_matches('0');
    let i = if i.is_empty() { "0" } else { i };
    Some(format!("{}{i}{}{f}", if neg { "-" } else { "" }, if f.is_empty() { "" } else { "." }))
}

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
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

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let mut out = Vec::with_capacity(s.len() / 4 * 3);
    let (mut acc, mut bits) = (0u32, 0u32);
    for b in s.bytes() {
        let v = match b {
            b'A'..=b'Z' => b - b'A',
            b'a'..=b'z' => b - b'a' + 26,
            b'0'..=b'9' => b - b'0' + 52,
            b'+' | b'-' => 62,
            b'/' | b'_' => 63,
            b'=' | b'\n' | b'\r' => continue,
            _ => return None,
        };
        acc = acc << 6 | v as u32;
        bits += 6;
        if bits >= 8 {
            bits -= 8;
            out.push((acc >> bits) as u8);
            acc &= (1 << bits) - 1;
        }
    }
    Some(out)
}

/// A string literal (JSON's escapes are OrientDB SQL's).
pub(crate) fn text(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "''".into())
}

/// `YYYY-MM-DD[ T]HH:MM:SS[.f…][zone]` → (`YYYY-MM-DD HH:MM:SS.fff`, zone).
pub(crate) fn timestamp(s: &str) -> Option<(String, Option<String>)> {
    let s = s.trim();
    if s.len() < 19 || !s.is_char_boundary(19) {
        return None;
    }
    let (base, rest) = s.split_at(19);
    let b = base.as_bytes();
    let shape = b.iter().enumerate().all(|(i, c)| match i {
        4 | 7 => *c == b'-',
        10 => *c == b' ' || *c == b'T',
        13 | 16 => *c == b':',
        _ => c.is_ascii_digit(),
    });
    if !shape {
        return None;
    }
    let (frac, zone) = match rest.strip_prefix('.') {
        Some(r) => {
            let n = r.bytes().take_while(u8::is_ascii_digit).count();
            (&r[..n], &r[n..])
        }
        None => ("", rest),
    };
    let ms: String = frac.chars().chain("000".chars()).take(3).collect();
    let zone = zone.trim();
    let zone = match zone {
        "" => None,
        "Z" | "z" => Some("Z".to_string()),
        z if z.len() == 6 && (z.starts_with('+') || z.starts_with('-')) && z.as_bytes()[3] == b':' => Some(z.to_string()),
        z if z.len() == 5 && (z.starts_with('+') || z.starts_with('-')) => Some(format!("{}:{}", &z[..3], &z[3..])),
        z if z.len() == 3 && (z.starts_with('+') || z.starts_with('-')) => Some(format!("{z}:00")),
        _ => return None,
    };
    Some((format!("{} {}.{ms}", &base[..10], &base[11..]), zone))
}

/// A cell as an OrientDB SQL value for a property of type `ty` ("" when
/// undeclared).
pub(crate) fn literal(cell: &Cell, ty: &str) -> String {
    let ty = ty.to_ascii_uppercase();
    if ty == "STRING" {
        return match cell {
            Cell::Null => "null".into(),
            Cell::Text(s) | Cell::Decimal(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => text(s),
            other => match other.to_json() {
                Value::String(s) => text(&s),
                v => text(&v.to_string()),
            },
        };
    }
    match cell {
        Cell::Null => "null".into(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) if *u <= i64::MAX as u64 => u.to_string(),
        Cell::UInt(u) => format!("'{u}'.asDecimal()"),
        Cell::Float(f) if f.is_nan() => "'NaN'.asFloat()".into(),
        Cell::Float(f) if f.is_infinite() => format!("'{}Infinity'.asFloat()", if *f < 0.0 { "-" } else { "" }),
        Cell::Float(f) => format!("{f:?}"),
        Cell::Decimal(d) => format!("{}.asDecimal()", text(d)),
        Cell::Text(s) | Cell::Time(s) | Cell::Uuid(s) => text(s),
        Cell::Bytes(b) => text(&base64_encode(b)),
        Cell::Date(d) => format!("date({}, 'yyyy-MM-dd')", text(d)),
        // A zone in a zone-less cell is still a zone: kept.
        Cell::DateTime(s) | Cell::DateTimeTz(s) => match timestamp(s) {
            Some((t, Some(z))) => format!("date({}, 'yyyy-MM-dd HH:mm:ss.SSSXXX')", text(&format!("{t}{z}"))),
            Some((t, None)) => format!("date({}, 'yyyy-MM-dd HH:mm:ss.SSS')", text(&t)),
            None => text(s),
        },
        Cell::Json(j) => match serde_json::from_str::<Value>(j) {
            // JSON maps and lists are OrientDB SQL literals.
            Ok(v @ (Value::Object(_) | Value::Array(_))) => v.to_string(),
            Ok(Value::String(s)) => text(&s),
            Ok(v) if !v.is_null() => v.to_string(),
            _ => text(j),
        },
    }
}

/// A graph bookkeeping field (`out_Knows`, `in_`, a vertex's `out` / `in`):
/// only on vertex and edge classes, and only when it isn't declared or is
/// declared a `LINKBAG`. A document class's `in_stock`, or a vertex's
/// declared `out_date DATETIME`, is data. An edge's `out` / `in` are its
/// ends, not bookkeeping.
pub(crate) fn bookkeeping(kind: &str, name: &str, props: &HashMap<String, (String, bool)>) -> bool {
    (kind == VERTEX || kind == EDGE)
        && crate::is_graph_field(name)
        && !(kind == EDGE && (name == "out" || name == "in"))
        && props.get(name).is_none_or(|(t, _)| t.eq_ignore_ascii_case("LINKBAG"))
}

/// One row's statement.
pub(crate) fn statement(class: &str, kind: &str, names: &[String], types: &HashMap<String, (String, bool)>, row: &[Cell]) -> Result<String> {
    let class = ident(class);
    let (mut from, mut to) = (None, None);
    let mut sets: Vec<String> = Vec::new();
    for (n, c) in names.iter().zip(row) {
        match n.as_str() {
            "out" if kind == EDGE => from = Some(c),
            "in" if kind == EDGE => to = Some(c),
            n if n.starts_with('@') || bookkeeping(kind, n, types) => {}
            n => sets.push(format!("{} = {}", ident(n), literal(c, types.get(n).map_or("", |t| t.0.as_str())))),
        }
    }
    let set = if sets.is_empty() { String::new() } else { format!(" SET {}", sets.join(", ")) };
    Ok(match kind {
        EDGE => {
            let end = |c: Option<&Cell>| match c {
                Some(Cell::Text(s)) => rid(s).map(str::to_string),
                _ => None,
            };
            let (Some(f), Some(t)) = (end(from), end(to)) else {
                return Err(Error::Unsupported("para copiar aristas hacen falta sus columnas out e in (los #rid de sus vértices)".into()));
            };
            format!("CREATE EDGE {class} FROM {f} TO {t}{set};")
        }
        VERTEX => format!("CREATE VERTEX {class}{set};"),
        _ if sets.is_empty() => format!("INSERT INTO {class} CONTENT {{}};"),
        _ => format!("INSERT INTO {class}{set};"),
    })
}

/// The `/batch` body of one transaction.
pub(crate) fn batch_body(stmts: &str) -> Value {
    json!({ "transaction": false, "operations": [{ "type": "script", "language": "sql", "script": format!("BEGIN;\n{stmts}COMMIT;") }] })
}

/// Bytes `s` takes inside a JSON string (what the request body carries).
pub(crate) fn json_len(s: &str) -> usize {
    s.chars()
        .map(|c| match c {
            '"' | '\\' | '\n' | '\r' | '\t' | '\u{8}' | '\u{c}' => 2,
            c if (c as u32) < 0x20 => 6,
            c => c.len_utf8(),
        })
        .sum()
}

type Auth = Option<(String, Option<String>)>;

async fn send_script(http: reqwest::Client, base: String, auth: Auth, path: String, stmts: String, rows: u64) -> Result<u64> {
    request(&http, &base, &auth, Method::POST, &path, Some(&batch_body(&stmts)))
        .await
        .map_err(|e| match e {
            Error::Query(m) => Error::Query(format!("carga en lote: {m}")),
            e => e,
        })?;
    Ok(rows)
}

fn joined(r: Option<std::result::Result<Result<u64>, tokio::task::JoinError>>) -> Result<u64> {
    match r {
        Some(Ok(r)) => r,
        Some(Err(e)) => Err(Error::State(format!("carga interrumpida: {e}"))),
        None => Ok(0),
    }
}

impl OrientSession {
    /// The class's metadata (every class, for inheritance) and its kind.
    async fn class_meta(&self, name: &str) -> Result<(Vec<Value>, &'static str, Value)> {
        let meta = self.metadata().await?;
        let all: Vec<Value> = meta.get("classes").and_then(Value::as_array).cloned().unwrap_or_default();
        let found = crate::classify(&all).into_iter().find(|c| c.0 == name);
        let Some((_, kind, c)) = found else {
            return Err(Error::Query(format!("no existe la clase {name}")));
        };
        Ok((all, kind, c))
    }

    /// One page, always over `/query`: the server refuses anything that
    /// writes there, on read-only connections or not. With a `cap`, a
    /// reply that grows past it is dropped as it arrives (`None`).
    fn page(&self, q: String, cap: Option<usize>) -> tokio::task::JoinHandle<Result<Option<String>>> {
        let (http, base, auth) = (self.http.clone(), self.base.clone(), self.auth.clone());
        let db = crate::seg(&self.db);
        tokio::spawn(async move {
            let url = format!("{base}/query/{db}/sql/{}/-1", crate::seg(&q));
            let mut rq = http.get(url).header("Accept", "application/json");
            if let Some((u, p)) = &auth {
                rq = rq.basic_auth(u, p.as_deref());
            }
            let mut resp = rq.send().await.map_err(|e| Error::Connect(format!("no se pudo llegar a OrientDB: {e}")))?;
            let status = resp.status();
            let mut body: Vec<u8> = Vec::new();
            while let Some(c) = resp.chunk().await.map_err(|e| Error::Connect(e.to_string()))? {
                if status.is_success() && cap.is_some_and(|cap| body.len() + c.len() > cap) {
                    return Ok(None);
                }
                body.extend_from_slice(&c);
            }
            let text = String::from_utf8(body).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
            if status.is_success() {
                return Ok(Some(text));
            }
            let msg = crate::error_text(status, &text);
            Err(if status == reqwest::StatusCode::UNAUTHORIZED {
                Error::AuthFailed(msg)
            } else if msg.contains("non idempotent") {
                Error::Query("la lectura de origen solo admite consultas que no escriben".into())
            } else {
                Error::Query(msg)
            })
        })
    }

    /// A window's page, after `after`.
    fn window_page(&self, class: &str, projections: &[String], filter: Option<&str>, after: &str, w: &Window) -> tokio::task::JoinHandle<Result<Option<String>>> {
        self.page(page_query(class, projections, filter, after, w.end().0, w.limit()), w.cap())
    }

    /// A query's rows, over `/query`.
    async fn rows(&self, q: String) -> Result<Vec<Vec<(String, Value)>>> {
        let reply = self.page(q, None).await.map_err(|e| Error::State(format!("lectura interrumpida: {e}")))??.unwrap_or_default();
        Ok(crate::parse_reply(&reply).map_err(Error::Query)?.records)
    }

    /// The next page to read, probing the next records' sizes when the
    /// probed ones ran out. `None` when there's nothing left.
    async fn next_window(&self, pager: &mut Pager<'_>) -> Result<Option<Window>> {
        if pager.probed.is_empty() && !pager.exhausted {
            let rows = self.rows(probe_query(pager.class, pager.filter, &pager.probed_to.0)).await?;
            pager.take_probe(&rows)?;
        }
        Ok(pager.cut())
    }

    /// The fields of a sample of the class's records that aren't in
    /// `known`: their names (`@this.keys()`) and then their types
    /// (`.type()`), with how often each shows. Only names and type names
    /// travel, never the values, so a class of large records costs the
    /// same as one of small ones.
    async fn sampled_fields(&self, class: &str, known: &[TransferColumn]) -> Result<Vec<TransferColumn>> {
        let from = format!(" FROM {} WHERE @class = {} LIMIT {SAMPLE}", ident(class), crate::ddl::string(class));
        let mut names: Vec<String> = Vec::new();
        for r in self.rows(format!("SELECT @this.keys() AS k{from}")).await? {
            for k in r.iter().filter(|(k, _)| k == "k").filter_map(|(_, v)| v.as_array()).flatten().filter_map(Value::as_str) {
                if !k.starts_with('@') && !names.iter().any(|n| n == k) && !known.iter().any(|c| c.name == k) {
                    names.push(k.to_string());
                }
            }
        }
        let mut out = Vec::new();
        for group in names.chunks(TYPES_PER_QUERY) {
            let projections: Vec<String> = group.iter().enumerate().map(|(i, n)| format!("{}.type() AS t{i}", ident(n))).collect();
            let rows = self.rows(format!("SELECT {}{from}", projections.join(", "))).await?;
            for (i, name) in group.iter().enumerate() {
                let alias = format!("t{i}");
                let mut types: Vec<(String, usize)> = Vec::new();
                let mut present = 0;
                for t in rows.iter().filter_map(|r| r.iter().find(|(k, _)| *k == alias).and_then(|(_, v)| v.as_str())) {
                    present += 1;
                    match types.iter_mut().find(|(n, _)| n == t) {
                        Some(e) => e.1 += 1,
                        None => types.push((t.to_string(), 1)),
                    }
                }
                types.sort_by_key(|t| std::cmp::Reverse(t.1));
                let type_name = if types.is_empty() { "ANY".into() } else { types.iter().map(|t| t.0.as_str()).collect::<Vec<_>>().join("|") };
                out.push(TransferColumn { name: name.clone(), type_name, nullable: present < rows.len() });
            }
        }
        Ok(out)
    }

    /// The columns read when none are asked for: the class's properties
    /// (inherited ones after its own) and the fields of a sample, without
    /// graph bookkeeping.
    async fn default_columns(&self, class: &str, kind: &str, meta: &Value, props: &HashMap<String, (String, bool)>) -> Result<Vec<TransferColumn>> {
        let mut cols: Vec<TransferColumn> = Vec::new();
        let own = meta.get("properties").and_then(Value::as_array).into_iter().flatten().filter_map(|p| p.get("name").and_then(Value::as_str));
        let mut inherited: Vec<&String> = props.keys().collect();
        inherited.sort();
        for name in own.map(str::to_string).chain(inherited.into_iter().cloned()) {
            if cols.iter().any(|c| c.name == name) || bookkeeping(kind, &name, props) {
                continue;
            }
            let (ty, nullable) = props.get(&name).cloned().unwrap_or_else(|| (String::new(), true));
            cols.push(TransferColumn { name, type_name: ty, nullable });
        }
        for c in self.sampled_fields(class, &cols).await? {
            if !bookkeeping(kind, &c.name, props) {
                cols.push(c);
            }
        }
        if kind == EDGE {
            for end in ["in", "out"] {
                if let Some(i) = cols.iter().position(|x| x.name == end) {
                    let c = cols.remove(i);
                    cols.insert(0, c);
                } else {
                    cols.insert(0, TransferColumn { name: end.into(), type_name: "LINK".into(), nullable: false });
                }
            }
        }
        Ok(cols)
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        if ![VERTEX, EDGE, kinds::TABLE].contains(&spec.table.kind.as_str()) {
            return dbine_driver::transfer::read_via_execute(self, spec, sink).await;
        }
        self.db_seg()?;
        let class = spec.table.name.clone();
        let (all, kind, meta) = self.class_meta(&class).await?;
        let props = properties(&all, &class);
        let cols: Vec<TransferColumn> = match &spec.columns {
            Some(names) => names
                .iter()
                .map(|n| {
                    let (ty, nullable) = props.get(n).cloned().unwrap_or_default();
                    TransferColumn { name: n.clone(), type_name: ty, nullable: nullable || !props.contains_key(n) }
                })
                .collect(),
            None => self.default_columns(&class, kind, &meta, &props).await?,
        };
        let kinds: Vec<Kind> = cols.iter().map(|c| props.get(&c.name).map_or(Kind::Any, |p| Kind::of(&p.0))).collect();
        let aliases: Vec<String> = (0..cols.len()).map(|i| format!("dbineC{i}")).collect();
        let projections: Vec<String> = cols.iter().zip(&kinds).zip(&aliases).map(|((c, k), a)| projection(&c.name, *k, a)).collect();
        let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
        if let Some(f) = filter {
            check_filter(f)?;
        }

        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut builder = BatchBuilder::new();
        let mut pager = Pager::new(&class, filter, cols.len());
        // The previous page's end: the next one starts after it.
        let (mut after, mut after_key) = ("#-1:-1".to_string(), (-1i64, -1i64));
        let mut next = self.next_window(&mut pager).await?.map(|w| (self.window_page(&class, &projections, filter, &after, &w), w));
        while let Some((h, w)) = next.take() {
            let Some(reply) = h.await.map_err(|e| Error::State(format!("lectura interrumpida: {e}")))?? else {
                // Past the cap: nothing of it was kept; cut it again
                // smaller, from the same place.
                pager.overflowed(w);
                next = self.next_window(&mut pager).await?.map(|n| (self.window_page(&class, &projections, filter, &after, &n), n));
                continue;
            };
            let reply_bytes = reply.len();
            let rows = crate::parse_reply(&reply).map_err(Error::Query)?.records;
            drop(reply);
            if rows.len() > w.limit() {
                return Err(Error::Query(format!("la página trajo {} registros y se pidieron {}; revisá el filtro", rows.len(), w.limit())));
            }
            // Every row inside the window, in order, of this class: a
            // filter that slipped out of the bounds fails here.
            let last = check_rows(rows.iter().map(|r| rid_and_class(r)), &class, after_key, Some(w.end().1))?;
            pager.factor = calibrate(pager.factor, reply_bytes, w.stored());
            let end = (w.end().0.to_string(), w.end().1);
            next = match w.rest(rows.len(), last) {
                // Full but short of the end: records inserted inside the
                // window took the place of some of its own; go on after
                // the last row read.
                Some(rest) => {
                    (after, after_key) = (format!("#{}:{}", last.0, last.1), last);
                    Some((self.window_page(&class, &projections, filter, &after, &rest), rest))
                }
                None => {
                    (after, after_key) = end;
                    self.next_window(&mut pager).await?.map(|n| (self.window_page(&class, &projections, filter, &after, &n), n))
                }
            };
            let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
            for r in &rows {
                let row = aliases
                    .iter()
                    .zip(&kinds)
                    .map(|(a, k)| r.iter().find(|(x, _)| x == a).map_or(Cell::Null, |(_, v)| to_cell(*k, v)))
                    .collect();
                builder.push(row, &mut *s)?;
            }
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
        let names: Vec<String> = if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let class = spec.table.name.clone();
        let (all, kind, _) = self.class_meta(&class).await?;
        let types = properties(&all, &class);
        if kind == EDGE && !(names.iter().any(|n| n == "out") && names.iter().any(|n| n == "in")) {
            return Err(Error::Unsupported("para copiar aristas hacen falta sus columnas out e in (los #rid de sus vértices)".into()));
        }
        let path = format!("/batch/{}", self.db_seg()?);
        let chunk_rows = spec.commit_rows.clamp(1, CHUNK);
        let chunk_bytes = (spec.commit_bytes as usize).clamp(1, MAX_SCRIPT);
        let in_flight = if kind == EDGE { 1 } else { IN_FLIGHT };

        let mut inflight: JoinSet<Result<u64>> = JoinSet::new();
        let result = async {
            let mut done = 0u64;
            let (mut script, mut rows, mut bytes) = (String::new(), 0u64, 0usize);
            loop {
                let batch = source.next().await;
                let finished = batch.is_none();
                for row in batch.map(|b| b.rows).unwrap_or_default() {
                    if row.len() != names.len() {
                        return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                    }
                    let stmt = statement(&class, kind, &names, &types, &row)?;
                    let len = json_len(&stmt) + 2;
                    if rows > 0 && bytes + len > chunk_bytes {
                        if inflight.len() >= in_flight {
                            done += joined(inflight.join_next().await)?;
                            progress(done);
                        }
                        let s = std::mem::take(&mut script);
                        bytes = 0;
                        inflight.spawn(send_script(self.http.clone(), self.base.clone(), self.auth.clone(), path.clone(), s, std::mem::take(&mut rows)));
                    }
                    script.push_str(&stmt);
                    script.push('\n');
                    bytes += len;
                    rows += 1;
                    if rows >= chunk_rows {
                        if inflight.len() >= in_flight {
                            done += joined(inflight.join_next().await)?;
                            progress(done);
                        }
                        let s = std::mem::take(&mut script);
                        bytes = 0;
                        inflight.spawn(send_script(self.http.clone(), self.base.clone(), self.auth.clone(), path.clone(), s, std::mem::take(&mut rows)));
                    }
                }
                while let Some(r) = inflight.try_join_next() {
                    done += joined(Some(r))?;
                    progress(done);
                }
                if finished {
                    break;
                }
            }
            if rows > 0 {
                inflight.spawn(send_script(self.http.clone(), self.base.clone(), self.auth.clone(), path.clone(), script, rows));
            }
            while let Some(r) = inflight.join_next().await {
                done += joined(Some(r))?;
                progress(done);
            }
            Ok(done)
        }
        .await;
        if result.is_err() {
            // Scripts already sent may still commit: wait for them, so
            // nothing lands after the error is returned.
            while inflight.join_next().await.is_some() {}
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds_from_declared_types() {
        assert_eq!(Kind::of("datetime"), Kind::DateTime);
        assert_eq!(Kind::of("SHORT"), Kind::Int);
        assert_eq!(Kind::of("LINKLIST"), Kind::Json);
        assert_eq!(Kind::of("LONG|DOUBLE"), Kind::Any);
    }

    #[test]
    fn inherited_properties() {
        let all = vec![
            json!({ "name": "V", "superClass": "" }),
            json!({ "name": "Person", "superClasses": ["V"], "properties": [{ "name": "name", "type": "STRING", "mandatory": true }] }),
            json!({ "name": "Dev", "superClass": "Person", "properties": [{ "name": "lang", "type": "STRING" }, { "name": "name", "type": "EMBEDDED" }] }),
        ];
        let p = properties(&all, "Dev");
        assert_eq!(p["lang"], ("STRING".into(), true));
        assert_eq!(p["name"].0, "EMBEDDED");
        assert_eq!(properties(&all, "Person")["name"], ("STRING".into(), false));
    }

    #[test]
    fn projections_and_pages() {
        assert_eq!(projection("ts", Kind::DateTime, "c0"), "ts.format('yyyy-MM-dd HH:mm:ss.SSSXXX') AS c0");
        assert_eq!(projection("d", Kind::Date, "c1"), "d.format('yyyy-MM-dd') AS c1");
        assert_eq!(projection("a b", Kind::Decimal, "c2"), "`a b`.asString() AS c2");
        assert_eq!(projection("@rid", Kind::Any, "c3"), "@rid AS c3");
        assert_eq!(projection("x", Kind::Json, "c4"), "x AS c4");
        assert_eq!(
            page_query("P", &["a AS c0".into()], Some("a > 1"), "#12:40", "#12:90", 100),
            "SELECT a AS c0, @rid AS dbineRid, @class AS dbineClass FROM P WHERE @class = 'P' AND (a > 1) AND @rid > #12:40 AND @rid <= #12:90 ORDER BY @rid LIMIT 100"
        );
        assert_eq!(
            probe_query("P", None, "#-1:-1"),
            "SELECT @rid AS dbineRid, @size AS dbineSize, @class AS dbineClass FROM P WHERE @class = 'P' AND @rid > #-1:-1 ORDER BY @rid LIMIT 5000"
        );
    }

    #[test]
    fn record_ids() {
        assert_eq!(rid("#12:0"), Some("#12:0"));
        assert_eq!(rid("#-1:-1"), Some("#-1:-1"));
        assert_eq!(rid("#12:0 OR 1=1"), None);
        assert_eq!(rid("12:0"), None);
    }

    #[test]
    fn read_values_become_typed_cells() {
        // Date-times come with their offset: the exact instant.
        assert_eq!(to_cell(Kind::DateTime, &json!("2024-10-27 02:30:00.000+02:00")), Cell::DateTimeTz("2024-10-27 02:30:00.000+02:00".into()));
        assert_eq!(to_cell(Kind::DateTime, &json!("2024-10-27 02:30:00.000+01:00")), Cell::DateTimeTz("2024-10-27 02:30:00.000+01:00".into()));
        assert_eq!(to_cell(Kind::DateTime, &json!("2024-02-03 04:05:06.789Z")), Cell::DateTimeTz("2024-02-03 04:05:06.789+00:00".into()));
        assert_eq!(to_cell(Kind::DateTime, &json!("2024-02-03 04:05:06.789")), Cell::DateTime("2024-02-03 04:05:06.789".into()));
        assert_eq!(to_cell(Kind::Date, &json!("2024-02-03")), Cell::Date("2024-02-03".into()));
        assert_eq!(to_cell(Kind::Decimal, &json!("12345678901234567890.123456789")), Cell::Decimal("12345678901234567890.123456789".into()));
        assert_eq!(to_cell(Kind::Decimal, &json!("1.5E+3")), Cell::Decimal("1500".into()));
        assert!(matches!(to_cell(Kind::Float, &json!("NaN")), Cell::Float(f) if f.is_nan()));
        assert_eq!(to_cell(Kind::Float, &json!("-Infinity")), Cell::Float(f64::NEG_INFINITY));
        assert_eq!(to_cell(Kind::Float, &json!("1.1")), Cell::Float(1.1));
        assert_eq!(to_cell(Kind::Binary, &json!("AAEC/w==")), Cell::Bytes(vec![0, 1, 2, 255]));
        assert_eq!(to_cell(Kind::Link, &json!("#9:0")), Cell::Text("#9:0".into()));
        assert_eq!(to_cell(Kind::Link, &json!({ "@rid": "#9:1", "a": 1 })), Cell::Text("#9:1".into()));
        assert_eq!(to_cell(Kind::Json, &json!({ "@type": "d", "@version": 0, "x": [{ "@type": "d", "y": 1 }] })), Cell::Json(r#"{"x":[{"y":1}]}"#.into()));
        assert_eq!(to_cell(Kind::Int, &json!(9007199254740993i64)), Cell::Int(9007199254740993));
        assert_eq!(to_cell(Kind::Bool, &json!(true)), Cell::Bool(true));
        assert_eq!(to_cell(Kind::Text, &Value::Null), Cell::Null);
        // Undeclared fields keep their JSON type.
        assert_eq!(to_cell(Kind::Any, &json!(u64::MAX)), Cell::UInt(u64::MAX));
        assert_eq!(to_cell(Kind::Any, &json!("x")), Cell::Text("x".into()));
        assert_eq!(to_cell(Kind::Any, &json!([1, 2])), Cell::Json("[1,2]".into()));
        // A value that doesn't match its declared type isn't lost.
        assert_eq!(to_cell(Kind::Int, &json!("n/a")), Cell::Text("n/a".into()));
    }

    #[test]
    fn decimals_in_plain_digits() {
        assert_eq!(plain_decimal("1.2345678901234567E+19").unwrap(), "12345678901234567000");
        assert_eq!(plain_decimal("-2E-3").unwrap(), "-0.002");
        assert_eq!(plain_decimal("1.25E1").unwrap(), "12.5");
        assert_eq!(plain_decimal("0.00").unwrap(), "0.00");
        assert_eq!(plain_decimal("-12.340").unwrap(), "-12.340");
        assert_eq!(plain_decimal("abc"), None);
        assert_eq!(plain_decimal("NaN"), None);
    }

    #[test]
    fn base64_round_trip() {
        for n in 0..70usize {
            let b: Vec<u8> = (0..n).map(|i| (i * 37 % 256) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&b)).unwrap(), b);
        }
        assert_eq!(base64_encode(b"hola"), "aG9sYQ==");
        assert_eq!(base64_decode("not base64!"), None);
    }

    #[test]
    fn timestamps_normalized() {
        assert_eq!(timestamp("2024-02-03 04:05:06.789123"), Some(("2024-02-03 04:05:06.789".into(), None)));
        assert_eq!(timestamp("2024-02-03T04:05:06"), Some(("2024-02-03 04:05:06.000".into(), None)));
        assert_eq!(timestamp("2024-02-03 04:05:06.5+02:00"), Some(("2024-02-03 04:05:06.500".into(), Some("+02:00".into()))));
        assert_eq!(timestamp("2024-02-03 04:05:06-0300"), Some(("2024-02-03 04:05:06.000".into(), Some("-03:00".into()))));
        assert_eq!(timestamp("2024-02-03 04:05:06Z"), Some(("2024-02-03 04:05:06.000".into(), Some("Z".into()))));
        assert_eq!(timestamp("ayer"), None);
    }

    #[test]
    fn literals_by_cell_and_property() {
        assert_eq!(literal(&Cell::Bool(true), ""), "true");
        assert_eq!(literal(&Cell::Int(-5), "LONG"), "-5");
        assert_eq!(literal(&Cell::UInt(u64::MAX), ""), "'18446744073709551615'.asDecimal()");
        assert_eq!(literal(&Cell::Float(1.5), ""), "1.5");
        assert_eq!(literal(&Cell::Float(1e300), ""), "1e300");
        assert_eq!(literal(&Cell::Float(f64::NAN), ""), "'NaN'.asFloat()");
        assert_eq!(literal(&Cell::Float(f64::NEG_INFINITY), ""), "'-Infinity'.asFloat()");
        assert_eq!(literal(&Cell::Decimal("12.50".into()), "DECIMAL"), "\"12.50\".asDecimal()");
        assert_eq!(literal(&Cell::Text("a\n\"b\" \\ é".into()), ""), r#""a\n\"b\" \\ é""#);
        assert_eq!(literal(&Cell::Bytes(vec![0, 1, 2, 255]), "BINARY"), "\"AAEC/w==\"");
        assert_eq!(literal(&Cell::Date("2024-02-03".into()), "DATE"), "date(\"2024-02-03\", 'yyyy-MM-dd')");
        assert_eq!(literal(&Cell::DateTime("2024-02-03 04:05:06.789".into()), ""), "date(\"2024-02-03 04:05:06.789\", 'yyyy-MM-dd HH:mm:ss.SSS')");
        assert_eq!(
            literal(&Cell::DateTimeTz("2024-02-03 04:05:06+02:00".into()), ""),
            "date(\"2024-02-03 04:05:06.000+02:00\", 'yyyy-MM-dd HH:mm:ss.SSSXXX')"
        );
        assert_eq!(literal(&Cell::Time("04:05:06".into()), ""), "\"04:05:06\"");
        assert_eq!(literal(&Cell::Uuid("6f1c…".into()), ""), "\"6f1c…\"");
        assert_eq!(literal(&Cell::Json(r#"{"x": [1, "a"]}"#.into()), "EMBEDDED"), r#"{"x":[1,"a"]}"#);
        assert_eq!(literal(&Cell::Json("3".into()), ""), "3");
        assert_eq!(literal(&Cell::Json("not json".into()), ""), "\"not json\"");
        // Into a STRING property everything goes as text.
        assert_eq!(literal(&Cell::Int(5), "STRING"), "\"5\"");
        assert_eq!(literal(&Cell::DateTime("2024-02-03 04:05:06".into()), "string"), "\"2024-02-03 04:05:06\"");
        assert_eq!(literal(&Cell::Json(r#"{"a":1}"#.into()), "STRING"), r#""{\"a\":1}""#);
    }

    #[test]
    fn statements_per_class_kind() {
        let names: Vec<String> = ["@rid", "name", "n", "out_Knows", "extra"].iter().map(|s| s.to_string()).collect();
        let mut types = HashMap::new();
        types.insert("n".to_string(), ("STRING".to_string(), true));
        let row = vec![Cell::Text("#1:1".into()), Cell::Text("Ann".into()), Cell::Int(3), Cell::Json("[]".into()), Cell::Null];
        // A document class has no graph bookkeeping: `out_Knows` is data.
        // NULLs are written, not left out (that would store a default).
        assert_eq!(
            statement("Doc", kinds::TABLE, &names, &types, &row).unwrap(),
            "INSERT INTO Doc SET name = \"Ann\", n = \"3\", out_Knows = [], extra = null;"
        );
        assert_eq!(statement("Per son", VERTEX, &names, &types, &row).unwrap(), "CREATE VERTEX `Per son` SET name = \"Ann\", n = \"3\", extra = null;");
        let empty = vec![Cell::Null, Cell::Null, Cell::Null, Cell::Null, Cell::Null];
        assert_eq!(statement("Doc", kinds::TABLE, &names, &types, &empty).unwrap(), "INSERT INTO Doc SET name = null, n = null, out_Knows = null, extra = null;");
        assert_eq!(statement("P", VERTEX, &names, &types, &empty).unwrap(), "CREATE VERTEX P SET name = null, n = null, extra = null;");
        let only_rid: Vec<String> = vec!["@rid".into()];
        assert_eq!(statement("Doc", kinds::TABLE, &only_rid, &types, &[Cell::Null]).unwrap(), "INSERT INTO Doc CONTENT {};");
        assert_eq!(statement("P", VERTEX, &only_rid, &types, &[Cell::Null]).unwrap(), "CREATE VERTEX P;");

        // `in_stock` on a document class, and declared on a vertex, is kept.
        let names: Vec<String> = ["in_stock", "out_date"].iter().map(|s| s.to_string()).collect();
        let row = vec![Cell::Bool(true), Cell::Date("2024-02-03".into())];
        assert_eq!(
            statement("Doc", kinds::TABLE, &names, &HashMap::new(), &row).unwrap(),
            "INSERT INTO Doc SET in_stock = true, out_date = date(\"2024-02-03\", 'yyyy-MM-dd');"
        );
        let mut vt = HashMap::new();
        vt.insert("in_stock".to_string(), ("BOOLEAN".to_string(), true));
        vt.insert("out_date".to_string(), ("LINKBAG".to_string(), true));
        assert_eq!(statement("P", VERTEX, &names, &vt, &row).unwrap(), "CREATE VERTEX P SET in_stock = true;");

        let names: Vec<String> = ["out", "in", "since"].iter().map(|s| s.to_string()).collect();
        let row = vec![Cell::Text("#10:0".into()), Cell::Text("#10:1".into()), Cell::Int(2020)];
        assert_eq!(statement("Knows", EDGE, &names, &HashMap::new(), &row).unwrap(), "CREATE EDGE Knows FROM #10:0 TO #10:1 SET since = 2020;");
        let bad = vec![Cell::Text("#10:0; DELETE VERTEX V".into()), Cell::Text("#10:1".into()), Cell::Null];
        assert!(statement("Knows", EDGE, &names, &HashMap::new(), &bad).is_err());
    }

    #[test]
    fn graph_bookkeeping_only_on_graph_classes() {
        let mut p = HashMap::new();
        p.insert("in_stock".to_string(), ("BOOLEAN".to_string(), true));
        p.insert("out_Knows".to_string(), ("LINKBAG".to_string(), true));
        assert!(!bookkeeping(kinds::TABLE, "in_stock", &HashMap::new()));
        assert!(!bookkeeping(kinds::TABLE, "out_Knows", &HashMap::new()));
        assert!(!bookkeeping(kinds::TABLE, "in", &HashMap::new()));
        assert!(!bookkeeping(VERTEX, "in_stock", &p));
        assert!(bookkeeping(VERTEX, "out_Knows", &p));
        assert!(bookkeeping(VERTEX, "in_Likes", &p));
        assert!(!bookkeeping(EDGE, "out", &p));
        assert!(!bookkeeping(EDGE, "in", &p));
        assert!(!bookkeeping(VERTEX, "name", &p));
    }

    #[test]
    fn zoned_datetime_cells_keep_their_zone() {
        // A zone inside a zone-less cell goes through the zoned date().
        assert_eq!(
            literal(&Cell::DateTime("2024-02-03T04:05:06Z".into()), "DATETIME"),
            "date(\"2024-02-03 04:05:06.000Z\", 'yyyy-MM-dd HH:mm:ss.SSSXXX')"
        );
        assert_eq!(literal(&Cell::DateTime("ayer".into()), ""), "\"ayer\"");
    }

    #[test]
    fn filters_must_be_one_condition() {
        assert!(check_filter("a = 1").is_ok());
        assert!(check_filter("(a = 1 OR b = 2) AND c IN [1, 2]").is_ok());
        assert!(check_filter("name = 'a) OR (b' AND x = \"(\"").is_ok());
        assert!(check_filter("name = 'it\\'s ('").is_ok());
        assert!(check_filter("a=1) OR (a=1").is_err());
        assert!(check_filter("(a = 1").is_err());
        assert!(check_filter("a = 'x").is_err());
        assert!(check_filter("a = 1; DELETE FROM T").is_err());
        // Comments hide parentheses from the check: refused outside quotes.
        assert!(check_filter("id=1 /* ( */ ) OR (id=1 /* ) */").is_err());
        assert!(check_filter("id=1 /**/").is_err());
        assert!(check_filter("id=1 -- x").is_err());
        assert!(check_filter("id=1 // x").is_err());
        assert!(check_filter("a--1").is_err());
        assert!(check_filter("a - -1 = 0 AND b / 2 > 1 AND c = -1").is_ok());
        assert!(check_filter("s = '/* -- //' AND t = \"--\"").is_ok());
    }

    #[test]
    fn page_rows_checked() {
        let class = "D";
        let ok = [(Some("#10:1"), Some("D")), (Some("#10:2"), Some("D")), (Some("#11:0"), Some("D"))];
        assert_eq!(check_rows(ok, class, (-1, -1), Some((11, 0))).unwrap(), (11, 0));
        assert_eq!(check_rows([], class, (10, 5), Some((11, 0))).unwrap(), (10, 5));
        // The live case: D's record twice, then a subclass's.
        let dup = [(Some("#10:0"), Some("D")), (Some("#10:0"), Some("D")), (Some("#12:0"), Some("D2"))];
        assert!(check_rows(dup, class, (-1, -1), Some((12, 0))).is_err());
        assert!(check_rows([(Some("#12:0"), Some("D2"))], class, (-1, -1), None).is_err());
        // Before the previous page's end, or past this one's.
        assert!(check_rows([(Some("#10:3"), Some("D"))], class, (10, 3), None).is_err());
        assert!(check_rows([(Some("#10:9"), Some("D"))], class, (10, 3), Some((10, 8))).is_err());
        assert!(check_rows([(None, Some("D"))], class, (-1, -1), None).is_err());
    }

    #[test]
    fn record_id_order() {
        assert!(rid_key("#12:40").unwrap() > rid_key("#12:9").unwrap());
        assert!(rid_key("#13:0").unwrap() > rid_key("#12:99").unwrap());
        assert!(rid_key("#12:0").unwrap() > rid_key("#-1:-1").unwrap());
        assert_eq!(rid_key("#1:x"), None);
    }

    #[test]
    fn pages_sized_by_each_records_size() {
        // Small records: a full page.
        assert_eq!(fit(std::iter::repeat_n(10, 9_000), 2, 3, PAGE), PAGE);
        assert_eq!(fit(std::iter::repeat_n(10, 9_000), 2, 3, 1), 1);
        assert_eq!(fit(std::iter::repeat_n(10, 30), 2, 3, PAGE), 30);
        assert_eq!(fit([], 2, 3, PAGE), 0);
        // A record larger than a page comes alone.
        assert_eq!(fit([20_000_000, 1, 1], 2, 1, PAGE), 1);
        // The verifier's case: 90 records of 1 byte, then 300 of 120 KB.
        // Whatever the page starts on, it stays under the page bytes.
        let sizes: Vec<usize> = std::iter::repeat_n(1, 90).chain(std::iter::repeat_n(120_000, 300)).collect();
        let mut at = 0;
        let mut pages = 0;
        while at < sizes.len() {
            let n = fit(sizes[at..].iter().copied(), FACTOR_MIN, 2, PAGE);
            assert!(n >= 1);
            let bytes: usize = sizes[at..at + n].iter().map(|s| estimate(*s, FACTOR_MIN, 2)).sum();
            assert!(bytes <= PAGE_BYTES, "page of {n} records at {at}: {bytes} bytes");
            at += n;
            pages += 1;
        }
        assert!(pages >= 300 * 120_000 * FACTOR_MIN / PAGE_BYTES);
        // The factor only grows, within its bounds.
        assert_eq!(calibrate(2, 69_266, 50_016), 2);
        assert_eq!(calibrate(2, 300_000, 50_000), 6);
        assert_eq!(calibrate(6, 100, 100), 6);
        assert_eq!(calibrate(2, 10_000_000, 1), FACTOR_MAX);
        assert_eq!(calibrate(3, 10, 0), 3);
    }

    #[test]
    fn pager_cuts_from_the_probe() {
        let mut p = Pager::new("D", None, 2);
        let rows: Vec<Vec<(String, Value)>> = (0..60)
            .map(|i| vec![(RID.into(), json!(format!("#9:{i}"))), (SIZE.into(), json!(if i < 20 { 1 } else { 1_000_000 })), (CLASS.into(), json!("D"))])
            .collect();
        p.take_probe(&rows).unwrap();
        assert!(p.exhausted);
        // The first page is one record; then by bytes: the 19 small ones
        // and two large ones (2 MB each at factor 2), then two per page.
        let w = p.cut().unwrap();
        assert_eq!((w.end().0, w.limit(), w.stored()), ("#9:0", 1, 1));
        let w = p.cut().unwrap();
        assert_eq!((w.end().0, w.limit()), ("#9:21", 21));
        let w = p.cut().unwrap();
        assert_eq!((w.end().0, w.limit()), ("#9:23", 2));
        // A probe that goes backwards, or brings another class, fails.
        let mut p = Pager::new("D", None, 2);
        assert!(p.take_probe(&[vec![(RID.into(), json!("#9:1")), (CLASS.into(), json!("D2"))]]).is_err());
        let mut p = Pager::new("D", None, 2);
        assert!(p.take_probe(&[vec![(RID.into(), json!("#9:1")), (CLASS.into(), json!("D"))], vec![(RID.into(), json!("#9:1")), (CLASS.into(), json!("D"))]]).is_err());
    }

    fn probed(keys: &[(i64, i64)]) -> Vec<Probed> {
        keys.iter().map(|k| (format!("#{}:{}", k.0, k.1), *k, 10)).collect()
    }

    #[test]
    fn window_goes_on_when_inserts_push_records_out() {
        // A window over clusters 9 and 10: 4 records, LIMIT 4.
        let w = Window::new(probed(&[(9, 5), (9, 6), (10, 0), (10, 1)])).unwrap();
        assert_eq!((w.end(), w.limit()), (("#10:1", (10, 1)), 4));
        // Two records inserted into cluster 9 meanwhile (#9:7, #9:8) fill
        // the LIMIT: the page ends at #10:0, and #10:1 is still to read.
        let rest = w.rest(4, (10, 0)).unwrap();
        assert_eq!((rest.end(), rest.limit()), (("#10:1", (10, 1)), 1));
        // A full page that reached the end, or a short one: done.
        assert!(Window::new(probed(&[(9, 5), (9, 6)])).unwrap().rest(2, (9, 6)).is_none());
        assert!(Window::new(probed(&[(9, 5), (9, 6)])).unwrap().rest(1, (9, 5)).is_none());
        // Only a window of several records has a reply cap.
        assert_eq!(Window::new(probed(&[(9, 5)])).unwrap().cap(), None);
        assert_eq!(Window::new(probed(&[(9, 5), (9, 6)])).unwrap().cap(), Some(REPLY_CAP));
    }

    #[test]
    fn page_rows_after_an_insert_inside_the_window() {
        // The page_query of a window keeps its LIMIT; what makes up for
        // records pushed out of it is the rest of the window, read from
        // the last row: the first page's rows plus the rest's cover every
        // record that was there.
        let w = Window::new(probed(&[(9, 5), (10, 0), (10, 1), (10, 2)])).unwrap();
        // #9:6 and #9:7 are new: the page is #9:5, #9:6, #9:7, #10:0.
        let ids = ["#9:5", "#9:6", "#9:7", "#10:0"];
        let last = check_rows(ids.iter().map(|i| (Some(*i), Some("D"))), "D", (9, 4), Some(w.end().1)).unwrap();
        assert_eq!(last, (10, 0));
        let rest = w.rest(ids.len(), last).unwrap();
        let ids: Vec<&str> = rest.records.iter().map(|r| r.0.as_str()).collect();
        assert_eq!(ids, ["#10:1", "#10:2"]);
        assert!(page_query("D", &["a AS c0".into()], None, "#10:0", rest.end().0, rest.limit()).ends_with("AND @rid > #10:0 AND @rid <= #10:2 ORDER BY @rid LIMIT 2"));
    }

    #[test]
    fn overflowed_page_is_cut_again_smaller() {
        let mut p = Pager::new("D", None, 1);
        let rows: Vec<Vec<(String, Value)>> =
            (0..400).map(|i| vec![(RID.into(), json!(format!("#9:{i}"))), (SIZE.into(), json!(100_000)), (CLASS.into(), json!("D"))]).collect();
        p.take_probe(&rows).unwrap();
        let first = p.cut().unwrap();
        assert_eq!(first.limit(), 1);
        // ~20 records of 100 KB fit 4 MiB at factor 2.
        let w = p.cut().unwrap();
        let n = w.limit();
        assert!(n > 10, "{n}");
        // Past the cap (plain text first, then control characters at 6
        // bytes each): the same records come back, at twice the factor.
        p.overflowed(w);
        assert_eq!(p.factor, 4);
        let w = p.cut().unwrap();
        assert_eq!(w.records[0].0, "#9:1");
        assert!(w.limit() * 2 <= n + 1, "{} vs {n}", w.limit());
        // At the highest factor, half as many records each time, down to
        // one (which has no cap), so it always ends.
        p.factor = FACTOR_MAX;
        let m = w.limit();
        p.overflowed(w);
        let w = p.cut().unwrap();
        assert!(w.limit() <= (m / 2).max(1));
        let mut w = w;
        while w.limit() > 1 {
            p.overflowed(w);
            w = p.cut().unwrap();
        }
        assert_eq!(w.cap(), None);
        assert_eq!(w.records[0].0, "#9:1");
        // The page reply under the cap can't be more than ~2x the target.
        assert_eq!(REPLY_CAP, 2 * PAGE_BYTES);
    }

    #[test]
    fn body_bytes_counted_as_json() {
        assert_eq!(json_len("abc"), 3);
        assert_eq!(json_len("\"\\\n"), 6);
        assert_eq!(json_len("\u{1}"), 6);
        assert_eq!(json_len("é"), 2);
        let s = "INSERT INTO T SET a = \"x\\\"y\";\u{1}é";
        assert_eq!(json_len(s), serde_json::to_string(s).unwrap().len() - 2);
    }

    #[test]
    fn batch_is_one_transaction() {
        let b = batch_body("INSERT INTO T SET a = 1;\n");
        assert_eq!(b["transaction"], json!(false));
        assert_eq!(b["operations"][0]["type"], "script");
        assert_eq!(b["operations"][0]["script"], "BEGIN;\nINSERT INTO T SET a = 1;\nCOMMIT;");
    }
}
