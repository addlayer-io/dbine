//! Bulk transfer over the HTTP interface (see `dbine_driver::transfer`).
//!
//! - Reading: `SELECT … FORMAT RowBinaryWithNamesAndTypes`, decoded into
//!   typed cells as the body streams in. Types the decoder doesn't know
//!   (JSON, AggregateFunction…) are read as `toString`; Dynamic and Variant
//!   are Unsupported (their text loses each value's subtype and NULL).
//! - Loading: `INSERT … FORMAT RowBinary`, one HTTP request per commit
//!   window with its body streamed (a window is one insert block: atomic on
//!   MergeTree). Columns that need the server's help (unknown types, a
//!   `DateTime` in a zone other than UTC) go as text through `input()`.
//! - Native copy: the source's RowBinary piped into the target's INSERT.
//!   Rows are walked only to find where they end, never decoded.
//!
//! Cancelling: a window is cut by dropping its body before the end, so the
//! server discards it. Once the whole body was sent the server commits it
//! even if the caller gives up waiting (HTTP has no two-phase commit), so a
//! load dropped in that gap waits for the answer in its drop and reports
//! the window's rows if they were committed: nothing commits after the
//! load returned. Only a server that doesn't answer in [`SETTLE`] is left
//! to a best-effort `KILL QUERY` (logged).
//!
//! An exception after the server started streaming comes inside the body,
//! after the `X-ClickHouse-Exception-Tag`; the reader looks for it before
//! trusting the bytes around it.

use super::{body_exception, server_error, ClickHouseSession, Error, Flavor, Result};
use dbine_driver::kinds;
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, CopySpec, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{ObjectRef, Session};
use futures::channel::mpsc;
use futures::SinkExt;
use std::borrow::Cow;
use std::io;
use std::net::{Ipv4Addr, Ipv6Addr};
use std::ops::Range;

/// Bytes handed to the HTTP body at a time.
const SEND_CHUNK: usize = 1024 * 1024;
/// With an exception tag, the last bytes received aren't decoded until more
/// arrive: they could be the start of the exception block.
const MARGIN: usize = 64;
const MARK: &[u8] = b"__exception__\r\n";

// ---------------------------------------------------------------- types

/// A column type as RowBinary lays it out.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ty {
    Nullable(Box<Ty>),
    Bool,
    Int { bytes: usize, signed: bool },
    F32,
    F64,
    BF16,
    Decimal { bytes: usize, scale: u32 },
    Str,
    Fixed(usize),
    Date,
    Date32,
    DateTime { tz: Option<String> },
    DateTime64 { scale: u32, tz: Option<String> },
    Uuid,
    Ipv4,
    Ipv6,
    Enum { bytes: usize, values: Vec<(i16, String)> },
    Array(Box<Ty>),
    Tuple(Vec<(Option<String>, Ty)>),
    Map(Box<Ty>, Box<Ty>),
    /// Only in load plans: a zoned `DateTime` (the boxed type) nested in an
    /// Array, Map or Tuple, sent as `String` for the server to `CAST`.
    DtText(Box<Ty>),
}

/// A ClickHouse identifier in backticks. Inside them the server applies
/// backslash escapes too, so `\` is escaped as well as the backtick.
pub(crate) fn ident(name: &str) -> String {
    format!("`{}`", name.replace('\\', "\\\\").replace('`', "\\`"))
}

fn qualified(schema: Option<&str>, name: &str) -> String {
    match schema.filter(|s| !s.is_empty()) {
        Some(s) => format!("{}.{}", ident(s), ident(name)),
        None => ident(name),
    }
}

/// A type as ClickHouse spells it (for an `input()` structure).
fn render(ty: &Ty) -> String {
    match ty {
        Ty::Nullable(t) => format!("Nullable({})", render(t)),
        Ty::Bool => "Bool".into(),
        Ty::Int { bytes, signed } => format!("{}Int{}", if *signed { "" } else { "U" }, bytes * 8),
        Ty::F32 => "Float32".into(),
        Ty::F64 => "Float64".into(),
        Ty::BF16 => "BFloat16".into(),
        // The widest precision of the same width: the same bytes.
        Ty::Decimal { bytes, scale } => format!("Decimal({}, {scale})", match bytes { 4 => 9, 8 => 18, 16 => 38, _ => 76 }),
        Ty::Str | Ty::DtText(_) => "String".into(),
        Ty::Fixed(n) => format!("FixedString({n})"),
        Ty::Date => "Date".into(),
        Ty::Date32 => "Date32".into(),
        Ty::DateTime { tz: None } => "DateTime".into(),
        Ty::DateTime { tz: Some(z) } => format!("DateTime('{}')", literal(z)),
        Ty::DateTime64 { scale, tz: None } => format!("DateTime64({scale})"),
        Ty::DateTime64 { scale, tz: Some(z) } => format!("DateTime64({scale}, '{}')", literal(z)),
        Ty::Uuid => "UUID".into(),
        Ty::Ipv4 => "IPv4".into(),
        Ty::Ipv6 => "IPv6".into(),
        Ty::Enum { bytes, values } => format!(
            "Enum{}({})",
            bytes * 8,
            values.iter().map(|(n, s)| format!("'{}' = {n}", literal(s))).collect::<Vec<_>>().join(", ")
        ),
        Ty::Array(t) => format!("Array({})", render(t)),
        Ty::Map(k, v) => format!("Map({}, {})", render(k), render(v)),
        Ty::Tuple(e) => format!(
            "Tuple({})",
            e.iter()
                .map(|(n, t)| match n {
                    Some(n) => format!("{} {}", ident(n), render(t)),
                    None => render(t),
                })
                .collect::<Vec<_>>()
                .join(", ")
        ),
    }
}

/// A type as the user knows it, for messages (`DtText` shows what it holds).
fn type_name(ty: &Ty) -> String {
    fn plain(ty: &Ty) -> Ty {
        match ty {
            Ty::DtText(t) => plain(t),
            Ty::Nullable(t) => Ty::Nullable(Box::new(plain(t))),
            Ty::Array(t) => Ty::Array(Box::new(plain(t))),
            Ty::Map(k, v) => Ty::Map(Box::new(plain(k)), Box::new(plain(v))),
            Ty::Tuple(e) => Ty::Tuple(e.iter().map(|(n, t)| (n.clone(), plain(t))).collect()),
            t => t.clone(),
        }
    }
    render(&plain(ty))
}

/// `ty` with its zoned `DateTime`s turned into `DtText`.
fn text_leaves(ty: &Ty, server_utc: bool) -> Ty {
    match ty {
        Ty::DateTime { .. } | Ty::DateTime64 { .. } if zoned(ty, server_utc) => Ty::DtText(Box::new(ty.clone())),
        Ty::Nullable(t) => Ty::Nullable(Box::new(text_leaves(t, server_utc))),
        Ty::Array(t) => Ty::Array(Box::new(text_leaves(t, server_utc))),
        Ty::Map(k, v) => Ty::Map(Box::new(text_leaves(k, server_utc)), Box::new(text_leaves(v, server_utc))),
        Ty::Tuple(e) => Ty::Tuple(e.iter().map(|(n, t)| (n.clone(), text_leaves(t, server_utc))).collect()),
        t => t.clone(),
    }
}

/// Top-level arguments of `Name(a, b, …)`, respecting quotes and parens.
fn split_args(s: &str) -> Vec<&str> {
    let (mut out, mut depth, mut start, mut quote, mut escaped) = (Vec::new(), 0i32, 0, None::<char>, false);
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '`' | '"' => quote = Some(c),
            '(' => depth += 1,
            ')' => depth -= 1,
            ',' if depth == 0 => {
                out.push(s[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if !s[start..].trim().is_empty() {
        out.push(s[start..].trim());
    }
    out
}

/// `'it\'s'` → `it's`, and the rest after it.
fn unquote(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    let body = s.strip_prefix('\'')?;
    let (mut out, mut escaped) = (String::new(), false);
    for (i, c) in body.char_indices() {
        if escaped {
            out.push(match c {
                'n' => '\n',
                't' => '\t',
                'r' => '\r',
                '0' => '\0',
                c => c,
            });
            escaped = false;
        } else if c == '\\' {
            escaped = true;
        } else if c == '\'' {
            if body[i + 1..].starts_with('\'') {
                // '' inside a literal
                continue;
            }
            return Some((out, &body[i + 1..]));
        } else {
            out.push(c);
        }
    }
    None
}

fn decimal_bytes(precision: u32) -> Option<usize> {
    match precision {
        1..=9 => Some(4),
        10..=18 => Some(8),
        19..=38 => Some(16),
        39..=76 => Some(32),
        _ => None,
    }
}

fn point() -> Ty {
    Ty::Tuple(vec![(None, Ty::F64), (None, Ty::F64)])
}

/// A type as the server spells it (ClickHouse's `Nullable(Int64)` or
/// Proton's `nullable(int64)`); `None` for what the decoder doesn't know.
pub(crate) fn parse_type(s: &str) -> Option<Ty> {
    let s = s.trim();
    let (name, args) = match open_paren(s) {
        Some(i) if s.ends_with(')') => (&s[..i], split_args(&s[i + 1..s.len() - 1])),
        Some(_) => return None,
        None => (s, Vec::new()),
    };
    let key: String = name.trim().chars().filter(|c| *c != '_').collect::<String>().to_ascii_lowercase();
    let int = |bytes, signed| Ty::Int { bytes, signed };
    let num = |i: usize| -> Option<u32> { args.get(i)?.trim().parse().ok() };
    let tz = |i: usize| -> Option<Option<String>> {
        match args.get(i) {
            None => Some(None),
            Some(a) => Some(Some(unquote(a)?.0)),
        }
    };
    Some(match (key.as_str(), args.len()) {
        ("nullable", 1) => Ty::Nullable(Box::new(parse_type(args[0])?)),
        ("lowcardinality", 1) => parse_type(args[0])?,
        ("simpleaggregatefunction", 2) => parse_type(args[1])?,
        ("bool" | "boolean", 0) => Ty::Bool,
        ("int8", 0) => int(1, true),
        ("int16", 0) => int(2, true),
        ("int32", 0) => int(4, true),
        ("int64", 0) => int(8, true),
        ("int128", 0) => int(16, true),
        ("int256", 0) => int(32, true),
        ("uint8", 0) => int(1, false),
        ("uint16", 0) => int(2, false),
        ("uint32", 0) => int(4, false),
        ("uint64", 0) => int(8, false),
        ("uint128", 0) => int(16, false),
        ("uint256", 0) => int(32, false),
        ("float32", 0) => Ty::F32,
        ("float64", 0) => Ty::F64,
        ("bfloat16", 0) => Ty::BF16,
        ("decimal", 1 | 2) => Ty::Decimal { bytes: decimal_bytes(num(0)?)?, scale: if args.len() == 2 { num(1)? } else { 0 } },
        ("decimal32", 1) => Ty::Decimal { bytes: 4, scale: num(0)? },
        ("decimal64", 1) => Ty::Decimal { bytes: 8, scale: num(0)? },
        ("decimal128", 1) => Ty::Decimal { bytes: 16, scale: num(0)? },
        ("decimal256", 1) => Ty::Decimal { bytes: 32, scale: num(0)? },
        ("string", 0) => Ty::Str,
        ("fixedstring", 1) => Ty::Fixed(num(0)? as usize),
        ("date", 0) => Ty::Date,
        ("date32", 0) => Ty::Date32,
        ("datetime", 0 | 1) => Ty::DateTime { tz: tz(0)? },
        ("datetime64", 1 | 2) => Ty::DateTime64 { scale: num(0)?, tz: tz(1)? },
        ("uuid", 0) => Ty::Uuid,
        ("ipv4", 0) => Ty::Ipv4,
        ("ipv6", 0) => Ty::Ipv6,
        ("enum8" | "enum16", n) if n > 0 => {
            let values = args
                .iter()
                .map(|a| {
                    let (name, rest) = unquote(a)?;
                    Some((rest.trim().strip_prefix('=')?.trim().parse().ok()?, name))
                })
                .collect::<Option<Vec<_>>>()?;
            Ty::Enum { bytes: if key == "enum8" { 1 } else { 2 }, values }
        }
        ("array", 1) => Ty::Array(Box::new(parse_type(args[0])?)),
        ("map", 2) => Ty::Map(Box::new(parse_type(args[0])?), Box::new(parse_type(args[1])?)),
        ("tuple", n) if n > 0 => Ty::Tuple(args.iter().map(|a| tuple_element(a)).collect::<Option<_>>()?),
        ("point", 0) => point(),
        ("ring" | "linestring", 0) => Ty::Array(Box::new(point())),
        ("polygon" | "multilinestring", 0) => Ty::Array(Box::new(Ty::Array(Box::new(point())))),
        ("multipolygon", 0) => Ty::Array(Box::new(Ty::Array(Box::new(Ty::Array(Box::new(point())))))),
        _ => return None,
    })
}

/// The first `(` outside quotes: a tuple element's name like `` `a(` ``
/// is not where its type's arguments start.
fn open_paren(s: &str) -> Option<usize> {
    let (mut quote, mut escaped) = (None, false);
    for (i, c) in s.char_indices() {
        if let Some(q) = quote {
            if escaped {
                escaped = false;
            } else if c == '\\' {
                escaped = true;
            } else if c == q {
                quote = None;
            }
            continue;
        }
        match c {
            '\'' | '`' | '"' => quote = Some(c),
            '(' => return Some(i),
            _ => {}
        }
    }
    None
}

/// `T` or `name T` (a named tuple's element).
fn tuple_element(a: &str) -> Option<(Option<String>, Ty)> {
    if let Some(t) = parse_type(a) {
        return Some((None, t));
    }
    let (name, rest) = match a.strip_prefix('`') {
        Some(r) => {
            let end = r.find('`')?;
            (r[..end].to_string(), &r[end + 1..])
        }
        None => {
            let (n, r) = a.split_once(char::is_whitespace)?;
            (n.to_string(), r)
        }
    };
    Some((Some(name), parse_type(rest)?))
}

/// `Dynamic` or `Variant` anywhere in a type. Their text loses what they
/// keep: the subtype of each value (42 comes back a String) and NULL.
fn variable_type(t: &str) -> bool {
    let t = t.trim();
    let (name, args) = match open_paren(t) {
        Some(i) if t.ends_with(')') => (&t[..i], split_args(&t[i + 1..t.len() - 1])),
        _ => (t, Vec::new()),
    };
    // A named tuple element is `name Type`: the type is the last word.
    let key = name.rsplit(char::is_whitespace).next().unwrap_or("").to_ascii_lowercase();
    key == "dynamic" || key == "variant" || args.iter().any(|a| variable_type(a))
}

fn variable_unsupported(name: &str, t: &str) -> Error {
    Error::Unsupported(format!(
        "la columna «{name}» ({t}) guarda valores de tipo variable (Dynamic o Variant): \
         no se pueden transferir sin perder el tipo de cada valor ni los NULL"
    ))
}

/// JSON-like columns, read with `toString` and handed over as JSON.
fn is_json(t: &str) -> bool {
    let t = t.trim().to_ascii_lowercase();
    t.starts_with("json") || t.starts_with("object(")
}

fn utc(tz: &str) -> bool {
    matches!(
        tz.trim().trim_start_matches("Etc/"),
        "UTC" | "UCT" | "GMT" | "GMT0" | "GMT+0" | "GMT-0" | "Universal" | "Zulu" | "Greenwich"
    )
}

/// A `DateTime` somewhere in `ty` whose zone isn't UTC: its naive values
/// need the server to place them in that zone.
fn zoned(ty: &Ty, server_utc: bool) -> bool {
    match ty {
        Ty::DateTime { tz } | Ty::DateTime64 { tz, .. } => !tz.as_deref().map_or(server_utc, utc),
        Ty::Nullable(t) | Ty::Array(t) => zoned(t, server_utc),
        Ty::Map(k, v) => zoned(k, server_utc) || zoned(v, server_utc),
        Ty::Tuple(e) => e.iter().any(|(_, t)| zoned(t, server_utc)),
        _ => false,
    }
}

// ---------------------------------------------------------------- decoding

/// Why a value couldn't be read yet.
#[derive(Debug, PartialEq)]
pub(crate) enum Short {
    /// The buffer ends inside it.
    More,
    Bad(String),
    /// A value that can't be handed over faithfully.
    Unsupported(String),
}

type D<T> = std::result::Result<T, Short>;

pub(crate) struct Rd<'a> {
    b: &'a [u8],
    p: usize,
}

impl<'a> Rd<'a> {
    #[cfg(test)]
    pub(crate) fn new(b: &'a [u8]) -> Self {
        Rd { b, p: 0 }
    }
    fn take(&mut self, n: usize) -> D<&'a [u8]> {
        if self.b.len() - self.p < n {
            return Err(Short::More);
        }
        let s = &self.b[self.p..self.p + n];
        self.p += n;
        Ok(s)
    }
    fn arr<const N: usize>(&mut self) -> D<[u8; N]> {
        Ok(self.take(N)?.try_into().expect("length"))
    }
    fn u8(&mut self) -> D<u8> {
        Ok(self.take(1)?[0])
    }
    fn leb(&mut self) -> D<u64> {
        let (mut v, mut shift) = (0u64, 0);
        loop {
            let b = self.u8()?;
            v |= u64::from(b & 0x7f) << shift;
            if b & 0x80 == 0 {
                return Ok(v);
            }
            shift += 7;
            if shift > 63 {
                return Err(Short::Bad("longitud inválida".into()));
            }
        }
    }
    fn bytes(&mut self) -> D<&'a [u8]> {
        let n = self.leb()?;
        self.take(usize::try_from(n).map_err(|_| Short::Bad("longitud inválida".into()))?)
    }
    fn str(&mut self) -> D<String> {
        String::from_utf8(self.bytes()?.to_vec()).map_err(|_| Short::Bad("texto no UTF-8 en el encabezado".into()))
    }
}

/// `RowBinaryWithNamesAndTypes`' header: names and types.
fn read_header(r: &mut Rd) -> D<(Vec<String>, Vec<String>)> {
    let n = r.leb()? as usize;
    if n > 100_000 {
        return Err(Short::Bad("encabezado inválido".into()));
    }
    let names = (0..n).map(|_| r.str()).collect::<D<Vec<_>>>()?;
    let types = (0..n).map(|_| r.str()).collect::<D<Vec<_>>>()?;
    Ok((names, types))
}

/// Bytes of a fixed-width value.
fn width(ty: &Ty) -> Option<usize> {
    Some(match ty {
        Ty::Bool => 1,
        Ty::Int { bytes, .. } | Ty::Decimal { bytes, .. } | Ty::Enum { bytes, .. } => *bytes,
        Ty::F32 | Ty::Date32 | Ty::DateTime { .. } | Ty::Ipv4 => 4,
        Ty::F64 | Ty::DateTime64 { .. } => 8,
        Ty::BF16 | Ty::Date => 2,
        Ty::Fixed(n) => *n,
        Ty::Uuid | Ty::Ipv6 => 16,
        _ => return None,
    })
}

/// Walk over one value.
pub(crate) fn skip(r: &mut Rd, ty: &Ty) -> D<()> {
    match ty {
        Ty::Nullable(t) => {
            if r.u8()? == 0 {
                skip(r, t)?;
            }
        }
        Ty::Str | Ty::DtText(_) => {
            r.bytes()?;
        }
        Ty::Array(t) => {
            let n = r.leb()?;
            match width(t) {
                Some(w) => {
                    let total = n.checked_mul(w as u64).and_then(|t| usize::try_from(t).ok());
                    r.take(total.ok_or_else(|| Short::Bad("longitud inválida".into()))?)?;
                }
                None => {
                    for _ in 0..n {
                        skip(r, t)?;
                    }
                }
            }
        }
        Ty::Map(k, v) => {
            for _ in 0..r.leb()? {
                skip(r, k)?;
                skip(r, v)?;
            }
        }
        Ty::Tuple(e) => {
            for (_, t) in e {
                skip(r, t)?;
            }
        }
        t => {
            r.take(width(t).expect("fixed width"))?;
        }
    }
    Ok(())
}

fn i64_le(b: &[u8]) -> i64 {
    let mut a = [0u8; 8];
    a[..b.len()].copy_from_slice(b);
    // Sign-extend.
    if b.last().is_some_and(|x| x & 0x80 != 0) {
        a[b.len()..].fill(0xff);
    }
    i64::from_le_bytes(a)
}

fn u64_le(b: &[u8]) -> u64 {
    let mut a = [0u8; 8];
    a[..b.len()].copy_from_slice(b);
    u64::from_le_bytes(a)
}

const P19: u128 = 10_000_000_000_000_000_000;

/// A little-endian integer of any width as decimal digits.
pub(crate) fn big_text(le: &[u8], signed: bool) -> String {
    if le.len() <= 8 {
        return if signed { i64_le(le).to_string() } else { u64_le(le).to_string() };
    }
    let neg = signed && le.last().is_some_and(|x| x & 0x80 != 0);
    let mut limbs: Vec<u64> = le.chunks(8).map(u64_le).collect();
    if neg {
        let mut carry = true;
        for l in limbs.iter_mut() {
            *l = !*l;
            if carry {
                let (v, c) = l.overflowing_add(1);
                *l = v;
                carry = c;
            }
        }
    }
    let mut parts = Vec::new();
    while limbs.iter().any(|&l| l != 0) {
        let mut rem: u128 = 0;
        for l in limbs.iter_mut().rev() {
            let cur = (rem << 64) | u128::from(*l);
            *l = (cur / P19) as u64;
            rem = cur % P19;
        }
        parts.push(rem as u64);
    }
    let mut s = String::from(if neg { "-" } else { "" });
    match parts.split_last() {
        None => s.push('0'),
        Some((first, rest)) => {
            s.push_str(&first.to_string());
            for p in rest.iter().rev() {
                s.push_str(&format!("{p:019}"));
            }
        }
    }
    s
}

/// `12345` with scale 2 → `123.45`.
fn with_scale(int: String, scale: u32) -> String {
    if scale == 0 {
        return int;
    }
    let (neg, d) = match int.strip_prefix('-') {
        Some(d) => (true, d),
        None => (false, int.as_str()),
    };
    let s = scale as usize;
    let d = if d.len() <= s { format!("{}{d}", "0".repeat(s + 1 - d.len())) } else { d.to_string() };
    format!("{}{}.{}", if neg { "-" } else { "" }, &d[..d.len() - s], &d[d.len() - s..])
}

fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let y = yoe + era * 400;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// Days in month `m` of year `y` (proleptic Gregorian).
fn month_days(y: i64, m: u32) -> u32 {
    match m {
        2 if y % 4 == 0 && (y % 100 != 0 || y % 400 == 0) => 29,
        2 => 28,
        4 | 6 | 9 | 11 => 30,
        _ => 31,
    }
}

fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let mp = i64::from((m + 9) % 12);
    let doy = (153 * mp + 2) / 5 + i64::from(d) - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

fn fmt_date(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    format!("{y:04}-{m:02}-{d:02}")
}

/// `YYYY-MM-DD HH:MM:SS[.fff]` (UTC) from seconds and a fraction of
/// `scale` digits.
fn fmt_datetime(secs: i64, frac: u64, scale: u32) -> String {
    let (days, sod) = (secs.div_euclid(86_400), secs.rem_euclid(86_400));
    let mut s = format!("{} {:02}:{:02}:{:02}", fmt_date(days), sod / 3600, sod / 60 % 60, sod % 60);
    if scale > 0 {
        s.push_str(&format!(".{frac:0width$}", width = scale as usize));
    }
    s
}

fn fmt_uuid(b: &[u8; 16]) -> String {
    let hi = u64::from_le_bytes(b[..8].try_into().expect("8"));
    let lo = u64::from_le_bytes(b[8..].try_into().expect("8"));
    let h = format!("{hi:016x}{lo:016x}");
    format!("{}-{}-{}-{}-{}", &h[..8], &h[8..12], &h[12..16], &h[16..20], &h[20..])
}

/// One value as a cell.
pub(crate) fn read_cell(r: &mut Rd, ty: &Ty) -> D<Cell> {
    Ok(match ty {
        Ty::Nullable(t) => {
            if r.u8()? != 0 {
                Cell::Null
            } else {
                read_cell(r, t)?
            }
        }
        Ty::Bool => Cell::Bool(r.u8()? != 0),
        Ty::Int { bytes, signed } => {
            let b = r.take(*bytes)?;
            match (*bytes, *signed) {
                (1..=8, true) => Cell::Int(i64_le(b)),
                (1..=4, false) => Cell::Int(u64_le(b) as i64),
                (8, false) => Cell::UInt(u64_le(b)),
                _ => Cell::Decimal(big_text(b, *signed)),
            }
        }
        Ty::F32 => Cell::Float(f64::from(f32::from_le_bytes(r.arr()?))),
        Ty::F64 => Cell::Float(f64::from_le_bytes(r.arr()?)),
        Ty::BF16 => Cell::Float(f64::from(f32::from_bits(u32::from(u16::from_le_bytes(r.arr()?)) << 16))),
        Ty::Decimal { bytes, scale } => Cell::Decimal(with_scale(big_text(r.take(*bytes)?, true), *scale)),
        Ty::Str | Ty::DtText(_) => {
            let b = r.bytes()?;
            match std::str::from_utf8(b) {
                Ok(s) => Cell::Text(s.to_string()),
                Err(_) => Cell::Bytes(b.to_vec()),
            }
        }
        Ty::Fixed(n) => {
            // Padded with zero bytes: text without them (loading pads again).
            let b = r.take(*n)?;
            let trimmed = &b[..b.iter().rposition(|&x| x != 0).map_or(0, |i| i + 1)];
            match std::str::from_utf8(trimmed) {
                Ok(s) if !trimmed.contains(&0) => Cell::Text(s.to_string()),
                _ => Cell::Bytes(b.to_vec()),
            }
        }
        Ty::Date => Cell::Date(fmt_date(i64::from(u16::from_le_bytes(r.arr()?)))),
        Ty::Date32 => Cell::Date(fmt_date(i64::from(i32::from_le_bytes(r.arr()?)))),
        Ty::DateTime { .. } => Cell::DateTimeTz(format!("{}+00:00", fmt_datetime(i64::from(u32::from_le_bytes(r.arr()?)), 0, 0))),
        Ty::DateTime64 { scale, .. } => {
            let ticks = i64::from_le_bytes(r.arr()?);
            let div = 10i64.pow((*scale).min(18));
            Cell::DateTimeTz(format!(
                "{}+00:00",
                fmt_datetime(ticks.div_euclid(div), ticks.rem_euclid(div) as u64, (*scale).min(18))
            ))
        }
        Ty::Uuid => Cell::Uuid(fmt_uuid(&r.arr()?)),
        Ty::Ipv4 => Cell::Text(Ipv4Addr::from(u32::from_le_bytes(r.arr()?)).to_string()),
        Ty::Ipv6 => Cell::Text(Ipv6Addr::from(r.arr::<16>()?).to_string()),
        Ty::Enum { bytes, values } => {
            let v = i64_le(r.take(*bytes)?) as i16;
            Cell::Text(values.iter().find(|(n, _)| *n == v).map_or_else(|| v.to_string(), |(_, s)| s.clone()))
        }
        Ty::Array(_) | Ty::Tuple(_) | Ty::Map(..) => Cell::Json(read_json(r, ty)?.text()),
    })
}

// ---------------------------------------------------------------- JSON

/// A JSON value that keeps what `serde_json::Value` would lose: an
/// object's keys in their order, repeated keys (a `Map` can repeat them)
/// and a number's exact spelling (a 128-bit integer, a decimal).
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum J {
    Null,
    Bool(bool),
    /// The number as written.
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

fn json_str(s: &str, out: &mut String) {
    out.push_str(&serde_json::to_string(s).expect("a string serializes"));
}

impl J {
    fn write(&self, out: &mut String) {
        match self {
            J::Null => out.push_str("null"),
            J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            J::Num(n) => out.push_str(n),
            J::Str(s) => json_str(s, out),
            J::Arr(items) => {
                out.push('[');
                for (i, v) in items.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    v.write(out);
                }
                out.push(']');
            }
            J::Obj(m) => {
                out.push('{');
                for (i, (k, v)) in m.iter().enumerate() {
                    if i > 0 {
                        out.push(',');
                    }
                    json_str(k, out);
                    out.push(':');
                    v.write(out);
                }
                out.push('}');
            }
        }
    }

    pub(crate) fn text(&self) -> String {
        let mut s = String::new();
        self.write(&mut s);
        s
    }
}

/// Parse a JSON document into a `J`.
pub(crate) fn parse_json(s: &str) -> std::result::Result<J, String> {
    let mut p = Parser { s, i: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.i != s.len() {
        return Err(p.bad());
    }
    Ok(v)
}

struct Parser<'a> {
    s: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn bad(&self) -> String {
        format!("no es JSON válido (posición {})", self.i)
    }
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }
    fn ws(&mut self) {
        while matches!(self.peek(), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }
    fn value(&mut self, depth: usize) -> std::result::Result<J, String> {
        if depth > 256 {
            return Err("JSON demasiado anidado".into());
        }
        self.ws();
        match self.peek().ok_or_else(|| self.bad())? {
            b'{' => {
                self.i += 1;
                let mut m = Vec::new();
                self.ws();
                if self.peek() == Some(b'}') {
                    self.i += 1;
                    return Ok(J::Obj(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    self.ws();
                    if self.peek() != Some(b':') {
                        return Err(self.bad());
                    }
                    self.i += 1;
                    m.push((k, self.value(depth + 1)?));
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b'}') => {
                            self.i += 1;
                            return Ok(J::Obj(m));
                        }
                        _ => return Err(self.bad()),
                    }
                }
            }
            b'[' => {
                self.i += 1;
                let mut items = Vec::new();
                self.ws();
                if self.peek() == Some(b']') {
                    self.i += 1;
                    return Ok(J::Arr(items));
                }
                loop {
                    items.push(self.value(depth + 1)?);
                    self.ws();
                    match self.peek() {
                        Some(b',') => self.i += 1,
                        Some(b']') => {
                            self.i += 1;
                            return Ok(J::Arr(items));
                        }
                        _ => return Err(self.bad()),
                    }
                }
            }
            b'"' => Ok(J::Str(self.string()?)),
            b't' => self.word("true", J::Bool(true)),
            b'f' => self.word("false", J::Bool(false)),
            b'n' => self.word("null", J::Null),
            _ => {
                let start = self.i;
                while matches!(self.peek(), Some(b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E')) {
                    self.i += 1;
                }
                let n = &self.s[start..self.i];
                if !json_number(n) {
                    return Err(self.bad());
                }
                Ok(J::Num(n.to_string()))
            }
        }
    }
    fn word(&mut self, w: &str, v: J) -> std::result::Result<J, String> {
        if self.s[self.i..].starts_with(w) {
            self.i += w.len();
            Ok(v)
        } else {
            Err(self.bad())
        }
    }
    fn string(&mut self) -> std::result::Result<String, String> {
        let start = self.i;
        if self.peek() != Some(b'"') {
            return Err(self.bad());
        }
        self.i += 1;
        loop {
            match self.peek() {
                None => return Err(self.bad()),
                Some(b'\\') => self.i += 2,
                Some(b'"') => break,
                Some(_) => self.i += 1,
            }
        }
        self.i += 1;
        serde_json::from_str(self.s.get(start..self.i).ok_or_else(|| self.bad())?).map_err(|_| self.bad())
    }
}

/// `-?(0|[1-9][0-9]*)(\.[0-9]+)?([eE][+-]?[0-9]+)?`
fn json_number(n: &str) -> bool {
    let b = n.as_bytes();
    let mut i = usize::from(b.first() == Some(&b'-'));
    let digits = |i: &mut usize| {
        let from = *i;
        while b.get(*i).is_some_and(u8::is_ascii_digit) {
            *i += 1;
        }
        *i - from
    };
    let int = i;
    match digits(&mut i) {
        0 => return false,
        n if n > 1 && b[int] == b'0' => return false,
        _ => {}
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

fn nested_bytes() -> Short {
    Short::Unsupported(
        "un valor binario (no UTF-8) dentro de un Array, Tuple o Map no se puede pasar a JSON sin perder bytes; \
         usá la copia directa entre ClickHouse o convertí la columna"
            .into(),
    )
}

fn cell_json(c: Cell) -> D<J> {
    Ok(match c {
        Cell::Null => J::Null,
        Cell::Bool(b) => J::Bool(b),
        Cell::Int(i) => J::Num(i.to_string()),
        Cell::UInt(u) => J::Num(u.to_string()),
        Cell::Float(f) => serde_json::Number::from_f64(f).map_or_else(|| J::Str(f.to_string()), |n| J::Num(n.to_string())),
        Cell::Bytes(_) => return Err(nested_bytes()),
        Cell::Json(s) => parse_json(&s).unwrap_or(J::Str(s)),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => J::Str(s),
    })
}

/// A nested value (inside an Array, Tuple or Map) as JSON. A `Map` is an
/// object with its keys in order, repeated ones included.
fn read_json(r: &mut Rd, ty: &Ty) -> D<J> {
    Ok(match ty {
        Ty::Nullable(t) => {
            if r.u8()? != 0 {
                J::Null
            } else {
                read_json(r, t)?
            }
        }
        Ty::Array(t) => {
            let n = r.leb()?;
            let mut items = Vec::with_capacity((n as usize).min(1024));
            for _ in 0..n {
                items.push(read_json(r, t)?);
            }
            J::Arr(items)
        }
        Ty::Tuple(e) if e.iter().all(|(n, _)| n.is_some()) => {
            J::Obj(e.iter().map(|(n, t)| Ok((n.clone().unwrap_or_default(), read_json(r, t)?))).collect::<D<_>>()?)
        }
        Ty::Tuple(e) => J::Arr(e.iter().map(|(_, t)| read_json(r, t)).collect::<D<_>>()?),
        Ty::Map(k, v) => {
            let n = r.leb()?;
            let mut m = Vec::with_capacity((n as usize).min(1024));
            for _ in 0..n {
                let key = match read_json(r, k)? {
                    J::Str(s) | J::Num(s) => s,
                    other => other.text(),
                };
                m.push((key, read_json(r, v)?));
            }
            J::Obj(m)
        }
        t => cell_json(read_cell(r, t)?)?,
    })
}

// ---------------------------------------------------------------- encoding

/// A value to encode, from a cell or from inside a JSON document.
#[derive(Debug)]
pub(crate) enum Sc<'a> {
    Null,
    Bool(bool),
    I(i64),
    U(u64),
    F(f64),
    S(Cow<'a, str>),
    B(&'a [u8]),
}

impl<'a> Sc<'a> {
    pub(crate) fn of(c: &'a Cell) -> Self {
        match c {
            Cell::Null => Sc::Null,
            Cell::Bool(b) => Sc::Bool(*b),
            Cell::Int(i) => Sc::I(*i),
            Cell::UInt(u) => Sc::U(*u),
            Cell::Float(f) => Sc::F(*f),
            Cell::Bytes(b) => Sc::B(b),
            Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => {
                Sc::S(Cow::Borrowed(s))
            }
        }
    }

    fn json(v: &'a J) -> Self {
        match v {
            J::Null => Sc::Null,
            J::Bool(b) => Sc::Bool(*b),
            // Other numbers keep their spelling (a decimal, a 128-bit integer).
            J::Num(n) => match (n.parse::<i64>(), n.parse::<u64>()) {
                (Ok(i), _) => Sc::I(i),
                (_, Ok(u)) => Sc::U(u),
                _ => Sc::S(Cow::Borrowed(n)),
            },
            J::Str(s) => Sc::S(Cow::Borrowed(s)),
            other => Sc::S(Cow::Owned(other.text())),
        }
    }

    /// For an error message: «text», NULL or a byte count.
    fn shown(&self) -> String {
        match (self, self.text()) {
            (Sc::Null, _) => "NULL".into(),
            (_, Some(t)) if t.chars().count() > 80 => format!("«{}…»", t.chars().take(80).collect::<String>()),
            (_, Some(t)) => format!("«{t}»"),
            (Sc::B(b), None) => format!("un valor binario de {} bytes", b.len()),
            (_, None) => "el valor".into(),
        }
    }

    /// As text (numbers in their plain spelling).
    fn text(&self) -> Option<Cow<'_, str>> {
        Some(match self {
            Sc::S(s) => Cow::Borrowed(s.as_ref()),
            Sc::I(i) => Cow::Owned(i.to_string()),
            Sc::U(u) => Cow::Owned(u.to_string()),
            Sc::F(f) => Cow::Owned(f.to_string()),
            Sc::Bool(b) => Cow::Borrowed(if *b { "true" } else { "false" }),
            Sc::B(b) => Cow::Owned(std::str::from_utf8(b).ok()?.to_string()),
            Sc::Null => return None,
        })
    }
}

fn leb(mut n: u64, out: &mut Vec<u8>) {
    loop {
        let b = (n & 0x7f) as u8;
        n >>= 7;
        if n == 0 {
            out.push(b);
            return;
        }
        out.push(b | 0x80);
    }
}

/// Decimal digits (optional sign) → little-endian two's complement of
/// `bytes`, or `None` when out of range.
pub(crate) fn int_le(text: &str, bytes: usize, signed: bool) -> Option<Vec<u8>> {
    let t = text.trim();
    let (neg, digits) = match t.as_bytes().first() {
        Some(b'-') => (true, &t[1..]),
        Some(b'+') => (false, &t[1..]),
        _ => (false, t),
    };
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let mut limbs = [0u64; 4];
    for d in digits.bytes() {
        let mut carry = u128::from(d - b'0');
        for l in limbs.iter_mut() {
            let v = u128::from(*l) * 10 + carry;
            *l = v as u64;
            carry = v >> 64;
        }
        if carry != 0 {
            return None;
        }
    }
    let zero = limbs.iter().all(|&l| l == 0);
    if neg && !signed && !zero {
        return None;
    }
    let bits = limbs.iter().enumerate().rev().find(|(_, l)| **l != 0).map_or(0, |(i, l)| i * 64 + 64 - l.leading_zeros() as usize);
    let max = if signed { bytes * 8 - 1 } else { bytes * 8 };
    let power_of_two = limbs.iter().map(|l| l.count_ones()).sum::<u32>() == 1;
    if !(bits <= max || (neg && signed && bits == max + 1 && power_of_two)) {
        return None;
    }
    if neg {
        let mut carry = true;
        for l in limbs.iter_mut() {
            *l = !*l;
            if carry {
                let (v, c) = l.overflowing_add(1);
                *l = v;
                carry = c;
            }
        }
    }
    let mut out: Vec<u8> = limbs.iter().flat_map(|l| l.to_le_bytes()).collect();
    out.truncate(bytes);
    Some(out)
}

/// `-12.345` at scale 2 → the scaled integer's bytes (rounded half away
/// from zero).
pub(crate) fn decimal_le(text: &str, scale: u32, bytes: usize) -> std::result::Result<Vec<u8>, String> {
    let bad = || format!("«{text}» no es un número decimal");
    let t = text.trim();
    let (sign, body) = match t.as_bytes().first() {
        Some(b'-') => ("-", &t[1..]),
        Some(b'+') => ("", &t[1..]),
        _ => ("", t),
    };
    let (int, frac) = body.split_once('.').unwrap_or((body, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return Err(bad());
    }
    let s = scale as usize;
    let mut digits = format!("{int}{}", &frac[..frac.len().min(s)]);
    digits.extend(std::iter::repeat_n('0', s.saturating_sub(frac.len())));
    if frac.as_bytes().get(s).is_some_and(|d| *d >= b'5') {
        // Round up: add one to the digit string.
        let mut v: Vec<u8> = digits.into_bytes();
        let mut i = v.len();
        loop {
            if i == 0 {
                v.insert(0, b'1');
                break;
            }
            i -= 1;
            if v[i] == b'9' {
                v[i] = b'0';
            } else {
                v[i] += 1;
                break;
            }
        }
        digits = String::from_utf8(v).expect("digits");
    }
    if digits.is_empty() {
        digits.push('0');
    }
    int_le(&format!("{sign}{digits}"), bytes, true).ok_or_else(|| format!("«{text}» no entra en el tipo decimal"))
}

/// `YYYY-MM-DD[ T]HH:MM[:SS[.fff]][Z|±HH[:MM]]` → (seconds since the epoch
/// as written, nanoseconds, the offset in seconds when there is one).
pub(crate) fn parse_datetime(s: &str) -> Option<(i64, u32, Option<i64>)> {
    let s = s.trim();
    let b = s.as_bytes();
    let num = |r: Range<usize>| -> Option<i64> { s.get(r.clone()).filter(|x| x.bytes().all(|c| c.is_ascii_digit()))?.parse().ok() };
    if b.len() < 10 || b[4] != b'-' || b[7] != b'-' {
        return None;
    }
    let (y, m, d) = (num(0..4)?, num(5..7)? as u32, num(8..10)? as u32);
    // An impossible date or time (Feb 31, 25:61) is refused, never rolled
    // over into another instant.
    if !(1..=12).contains(&m) || d < 1 || d > month_days(y, m) {
        return None;
    }
    let days = days_from_civil(y, m, d);
    let mut rest = &s[10..];
    let (mut sod, mut nanos, mut offset) = (0i64, 0u32, None);
    if let Some(t) = rest.strip_prefix([' ', 'T']) {
        let tb = t.as_bytes();
        let tnum = |r: Range<usize>| -> Option<i64> { t.get(r).filter(|x| x.bytes().all(|c| c.is_ascii_digit()))?.parse().ok() };
        if tb.len() < 5 || tb[2] != b':' {
            return None;
        }
        let h = tnum(0..2)?;
        let mi = tnum(3..5)?;
        let mut sec = 0;
        let mut i = 5;
        if tb.get(5) == Some(&b':') {
            sec = tnum(6..8)?;
            i = 8;
        }
        if h > 23 || mi > 59 || sec > 59 {
            return None;
        }
        if tb.get(i) == Some(&b'.') {
            let f: String = t[i + 1..].chars().take_while(char::is_ascii_digit).collect();
            i += 1 + f.len();
            // Digits past nanoseconds that aren't zeros would be lost.
            if f.bytes().skip(9).any(|c| c != b'0') {
                return None;
            }
            let f9: String = f.chars().chain(std::iter::repeat('0')).take(9).collect();
            nanos = f9.parse().ok()?;
        }
        sod = h * 3600 + mi * 60 + sec;
        rest = &t[i..];
    }
    let z = rest.trim();
    if !z.is_empty() {
        offset = Some(match z {
            "Z" | "z" | "UTC" => 0,
            _ => {
                let sign = match z.as_bytes()[0] {
                    b'+' => 1,
                    b'-' => -1,
                    _ => return None,
                };
                let digits: String = z[1..].chars().filter(|c| *c != ':').collect();
                if !digits.bytes().all(|c| c.is_ascii_digit()) {
                    return None;
                }
                let (hh, mm) = match digits.len() {
                    2 => (digits.parse::<i64>().ok()?, 0),
                    4 => (digits[..2].parse::<i64>().ok()?, digits[2..].parse::<i64>().ok()?),
                    _ => return None,
                };
                if hh > 23 || mm > 59 {
                    return None;
                }
                sign * (hh * 3600 + mm * 60)
            }
        });
    }
    Some((days * 86_400 + sod, nanos, offset))
}

fn parse_uuid(s: &str) -> Option<u128> {
    let hex: String = s.trim().trim_matches(['{', '}']).chars().filter(|c| *c != '-').collect();
    if hex.len() != 32 {
        return None;
    }
    u128::from_str_radix(&hex, 16).ok()
}

fn float_of(v: &Sc) -> Option<f64> {
    match v {
        Sc::F(f) => Some(*f),
        Sc::I(i) => Some(*i as f64),
        Sc::U(u) => Some(*u as f64),
        Sc::Bool(b) => Some(f64::from(u8::from(*b))),
        Sc::S(s) => s.trim().parse().ok(),
        _ => None,
    }
}

/// Encode one value of `ty`.
pub(crate) fn encode(ty: &Ty, v: Sc, out: &mut Vec<u8>) -> std::result::Result<(), String> {
    let bad = |v: &Sc| format!("{} no es un valor de tipo {}", v.shown(), type_name(ty));
    match ty {
        Ty::Nullable(t) => {
            if matches!(v, Sc::Null) {
                out.push(1);
            } else {
                out.push(0);
                encode(t, v, out)?;
            }
            return Ok(());
        }
        _ if matches!(v, Sc::Null) => return Err("NULL en una columna que no admite nulos".into()),
        Ty::Array(_) | Ty::Tuple(_) | Ty::Map(..) => {
            let text = v.text().ok_or_else(|| bad(&v))?;
            return encode_json(ty, &parse_json(&text)?, out);
        }
        _ => {}
    }
    match ty {
        Ty::Bool => {
            let b = match &v {
                Sc::Bool(b) => *b,
                Sc::I(0) | Sc::U(0) => false,
                Sc::I(1) | Sc::U(1) => true,
                Sc::S(s) => match s.trim().to_ascii_lowercase().as_str() {
                    "true" | "t" | "1" | "yes" | "y" | "on" => true,
                    "false" | "f" | "0" | "no" | "n" | "off" => false,
                    _ => return Err(bad(&v)),
                },
                _ => return Err(bad(&v)),
            };
            out.push(u8::from(b));
        }
        Ty::Int { bytes, signed } => {
            let text = match &v {
                Sc::F(f) if f.fract() == 0.0 && f.is_finite() => Cow::Owned(format!("{f}")),
                Sc::Bool(b) => Cow::Borrowed(if *b { "1" } else { "0" }),
                Sc::I(_) | Sc::U(_) | Sc::S(_) => v.text().expect("text"),
                _ => return Err(bad(&v)),
            };
            out.extend(int_le(&text, *bytes, *signed).ok_or_else(|| format!("«{text}» no entra en el tipo entero"))?);
        }
        Ty::F32 => out.extend((float_of(&v).ok_or_else(|| bad(&v))? as f32).to_le_bytes()),
        Ty::F64 => out.extend(float_of(&v).ok_or_else(|| bad(&v))?.to_le_bytes()),
        Ty::BF16 => out.extend((((float_of(&v).ok_or_else(|| bad(&v))? as f32).to_bits() >> 16) as u16).to_le_bytes()),
        Ty::Decimal { bytes, scale } => {
            if matches!(v, Sc::F(f) if !f.is_finite()) || matches!(v, Sc::B(_) | Sc::Bool(_)) {
                return Err(bad(&v));
            }
            out.extend(decimal_le(&v.text().ok_or_else(|| bad(&v))?, *scale, *bytes)?);
        }
        Ty::DtText(t) => {
            // Checked here (read as UTC); the server places it in its zone.
            let text = v.text().ok_or_else(|| bad(&v))?;
            encode(t, Sc::S(Cow::Borrowed(&text)), &mut Vec::new())?;
            leb(text.len() as u64, out);
            out.extend_from_slice(text.as_bytes());
        }
        Ty::Str => {
            let b: Cow<[u8]> = match &v {
                Sc::B(b) => Cow::Borrowed(b),
                _ => match v.text().ok_or_else(|| bad(&v))? {
                    Cow::Borrowed(s) => Cow::Borrowed(s.as_bytes()),
                    Cow::Owned(s) => Cow::Owned(s.into_bytes()),
                },
            };
            leb(b.len() as u64, out);
            out.extend_from_slice(&b);
        }
        Ty::Fixed(n) => {
            let b: Vec<u8> = match &v {
                Sc::B(b) => b.to_vec(),
                _ => v.text().ok_or_else(|| bad(&v))?.as_bytes().to_vec(),
            };
            if b.len() > *n {
                return Err(format!("{} bytes no entran en FixedString({n})", b.len()));
            }
            out.extend_from_slice(&b);
            out.extend(std::iter::repeat_n(0, n - b.len()));
        }
        Ty::Date | Ty::Date32 => {
            let (secs, nanos, _) = v.text().as_deref().and_then(parse_datetime).ok_or_else(|| bad(&v))?;
            // A time of day would be dropped: only midnight fits.
            if secs.rem_euclid(86_400) != 0 || nanos != 0 {
                return Err(format!("{} tiene hora y una columna {} guarda solo la fecha: la hora se perdería", v.shown(), type_name(ty)));
            }
            let days = secs.div_euclid(86_400);
            if *ty == Ty::Date {
                out.extend(u16::try_from(days).map_err(|_| format!("{} está fuera del rango de Date", v.shown()))?.to_le_bytes());
            } else {
                out.extend(i32::try_from(days).map_err(|_| format!("{} está fuera del rango de Date32", v.shown()))?.to_le_bytes());
            }
        }
        Ty::DateTime { .. } => {
            let (secs, nanos, off) = v.text().as_deref().and_then(parse_datetime).ok_or_else(|| bad(&v))?;
            if nanos != 0 {
                return Err(format!(
                    "{} tiene fracciones de segundo y una columna {} guarda segundos enteros: se perderían",
                    v.shown(),
                    type_name(ty)
                ));
            }
            let secs = secs - off.unwrap_or(0);
            out.extend(u32::try_from(secs).map_err(|_| format!("{} está fuera del rango de DateTime", v.shown()))?.to_le_bytes());
        }
        Ty::DateTime64 { scale, .. } => {
            let (secs, nanos, off) = v.text().as_deref().and_then(parse_datetime).ok_or_else(|| bad(&v))?;
            let scale = (*scale).min(9);
            if nanos % 10u32.pow(9 - scale) != 0 {
                return Err(format!(
                    "{} tiene más de {scale} decimales de segundo y una columna {} guarda {scale}: los demás se perderían",
                    v.shown(),
                    type_name(ty)
                ));
            }
            let ticks = i128::from(secs - off.unwrap_or(0)) * 10i128.pow(scale) + i128::from(nanos / 10u32.pow(9 - scale));
            out.extend(i64::try_from(ticks).map_err(|_| format!("{} está fuera del rango de DateTime64", v.shown()))?.to_le_bytes());
        }
        Ty::Uuid => {
            let u = match &v {
                Sc::B(b) if b.len() == 16 => u128::from_be_bytes((*b).try_into().expect("16")),
                _ => v.text().as_deref().and_then(parse_uuid).ok_or_else(|| bad(&v))?,
            };
            out.extend(((u >> 64) as u64).to_le_bytes());
            out.extend((u as u64).to_le_bytes());
        }
        Ty::Ipv4 => {
            let n = match &v {
                Sc::I(i) => u32::try_from(*i).map_err(|_| bad(&v))?,
                Sc::U(u) => u32::try_from(*u).map_err(|_| bad(&v))?,
                _ => u32::from(v.text().and_then(|s| s.trim().parse::<Ipv4Addr>().ok()).ok_or_else(|| bad(&v))?),
            };
            out.extend(n.to_le_bytes());
        }
        Ty::Ipv6 => {
            let octets = match &v {
                Sc::B(b) if b.len() == 16 => (*b).try_into().expect("16"),
                _ => {
                    let t = v.text().ok_or_else(|| bad(&v))?;
                    let t = t.trim();
                    match t.parse::<Ipv6Addr>() {
                        Ok(a) => a.octets(),
                        Err(_) => t.parse::<Ipv4Addr>().map_err(|_| bad(&v))?.to_ipv6_mapped().octets(),
                    }
                }
            };
            out.extend_from_slice(&octets);
        }
        Ty::Enum { bytes, values } => {
            let n = match &v {
                Sc::I(i) => i16::try_from(*i).ok().filter(|n| values.iter().any(|(x, _)| x == n)),
                Sc::S(s) => values
                    .iter()
                    .find(|(_, name)| name == s)
                    .map(|(n, _)| *n)
                    .or_else(|| s.trim().parse().ok().filter(|n| values.iter().any(|(x, _)| x == n))),
                _ => None,
            }
            .ok_or_else(|| format!("{} no es un valor de {}", v.shown(), type_name(ty)))?;
            out.extend(&n.to_le_bytes()[..*bytes]);
        }
        Ty::Nullable(_) | Ty::Array(_) | Ty::Tuple(_) | Ty::Map(..) => unreachable!("handled above"),
    }
    Ok(())
}

/// Encode a JSON value into a nested type.
fn encode_json(ty: &Ty, v: &J, out: &mut Vec<u8>) -> std::result::Result<(), String> {
    match (ty, v) {
        (Ty::Nullable(_), J::Null) => out.push(1),
        (Ty::Nullable(t), v) => {
            out.push(0);
            encode_json(t, v, out)?;
        }
        (Ty::Array(t), J::Arr(items)) => {
            leb(items.len() as u64, out);
            for i in items {
                encode_json(t, i, out)?;
            }
        }
        (Ty::Tuple(e), J::Arr(items)) if items.len() == e.len() => {
            for ((_, t), i) in e.iter().zip(items) {
                encode_json(t, i, out)?;
            }
        }
        (Ty::Tuple(e), J::Obj(m)) => {
            let keys: Vec<String> = e.iter().enumerate().map(|(i, (n, _))| n.clone().unwrap_or_else(|| (i + 1).to_string())).collect();
            if let Some((extra, _)) = m.iter().find(|(k, _)| !keys.contains(k)) {
                return Err(format!("«{extra}» no es un elemento de {}", type_name(ty)));
            }
            for (key, (_, t)) in keys.iter().zip(e) {
                match m.iter().find(|(k, _)| k == key) {
                    Some((_, v)) => encode_json(t, v, out)?,
                    // An absent element is NULL only where NULL fits.
                    None if matches!(t, Ty::Nullable(_)) => out.push(1),
                    None => return Err(format!("falta el elemento «{key}» de {}", type_name(ty))),
                }
            }
        }
        // In order, repeated keys included.
        (Ty::Map(k, t), J::Obj(m)) => {
            leb(m.len() as u64, out);
            for (key, val) in m {
                encode(k, Sc::S(Cow::Borrowed(key)), out)?;
                encode_json(t, val, out)?;
            }
        }
        (Ty::Map(k, t), J::Arr(pairs)) => {
            leb(pairs.len() as u64, out);
            for p in pairs {
                let J::Arr(kv) = p else {
                    return Err("un Map en JSON es un objeto o una lista de pares".into());
                };
                let [key, val] = kv.as_slice() else {
                    return Err("un Map en JSON es un objeto o una lista de pares".into());
                };
                encode_json(k, key, out)?;
                encode_json(t, val, out)?;
            }
        }
        // A nested document written as a string.
        (Ty::Array(_) | Ty::Tuple(_) | Ty::Map(..), J::Str(s)) => encode_json(ty, &parse_json(s)?, out)?,
        (Ty::Array(_) | Ty::Tuple(_) | Ty::Map(..), v) => return Err(format!("{} no es un valor de tipo {}", v.text(), type_name(ty))),
        (t, J::Null) => return Err(format!("null en un elemento de tipo {} que no admite nulos", type_name(t))),
        (t, v) => encode(t, Sc::json(v), out)?,
    }
    Ok(())
}

// ---------------------------------------------------------------- HTTP

fn net_err(e: reqwest::Error) -> Error {
    if e.is_connect() || e.is_timeout() {
        Error::Connect(e.to_string())
    } else {
        Error::Query(e.to_string())
    }
}

fn lock_err<T>(_: T) -> Error {
    Error::State("destino de lotes".into())
}

fn column_list(cols: &[String]) -> String {
    cols.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", ")
}

/// What to read from: Proton's streams through `table()` (their stored
/// rows, not a streaming query).
fn read_from(flavor: Flavor, t: &ObjectRef) -> String {
    let name = qualified(t.schema(), &t.name);
    if flavor == Flavor::Timeplus && t.kind == kinds::STREAM {
        format!("table({name})")
    } else {
        name
    }
}

fn where_clause(filter: Option<&str>) -> String {
    match filter.map(str::trim).filter(|f| !f.is_empty()) {
        Some(f) => format!(" WHERE ({f})"),
        None => String::new(),
    }
}

/// A SQL string literal's contents.
fn literal(s: &str) -> String {
    s.replace('\\', "\\\\").replace('\'', "\\'")
}

/// A streamed response body read in pieces, watching for an exception the
/// server writes into it after it started.
struct Stream {
    resp: reqwest::Response,
    buf: Vec<u8>,
    pos: usize,
    /// Where the last value read sits in `buf`.
    last: Range<usize>,
    eof: bool,
    /// `__exception__\r\n<tag>`: the start of an exception block.
    marker: Option<Vec<u8>>,
    scanned: usize,
}

pub(crate) fn find(hay: &[u8], needle: &[u8]) -> Option<usize> {
    hay.windows(needle.len()).position(|w| w == needle)
}

impl Stream {
    fn new(resp: reqwest::Response, flavor: Flavor) -> Self {
        let marker = resp.headers().get(flavor.header("exception-tag")).map(|t| [MARK, t.as_bytes()].concat());
        Stream { resp, buf: Vec::with_capacity(SEND_CHUNK), pos: 0, last: 0..0, eof: false, marker, scanned: 0 }
    }

    fn limit(&self) -> usize {
        if self.eof || self.marker.is_none() {
            self.buf.len()
        } else {
            self.buf.len().saturating_sub(MARGIN)
        }
    }

    async fn fill(&mut self) -> Result<()> {
        if self.pos > 0 && self.pos >= self.buf.len() / 2 {
            self.buf.drain(..self.pos);
            self.scanned = self.scanned.saturating_sub(self.pos);
            self.pos = 0;
        }
        match self.resp.chunk().await.map_err(net_err)? {
            Some(c) => self.buf.extend_from_slice(&c),
            None => self.eof = true,
        }
        if let Some(m) = &self.marker {
            let from = self.scanned.saturating_sub(m.len());
            if let Some(i) = find(&self.buf[from..], m) {
                return Err(self.failure(from + i, "").await);
            }
            self.scanned = self.buf.len();
        }
        Ok(())
    }

    /// The server's exception from `at` on, or `why`.
    async fn failure(&mut self, at: usize, why: &str) -> Error {
        while !self.eof && self.buf.len() - at < 1024 * 1024 {
            match self.resp.chunk().await {
                Ok(Some(c)) => self.buf.extend_from_slice(&c),
                _ => self.eof = true,
            }
        }
        let text = String::from_utf8_lossy(&self.buf[at.min(self.buf.len())..]).into_owned();
        body_exception(&text).unwrap_or_else(|| Error::Query(format!("respuesta RowBinary inválida: {why}")))
    }

    /// The next value `f` reads, `None` at the end.
    async fn next<T>(&mut self, mut f: impl FnMut(&mut Rd) -> D<T>) -> Result<Option<T>> {
        loop {
            let limit = self.limit();
            let mut r = Rd { b: &self.buf[..limit], p: self.pos };
            match f(&mut r) {
                Ok(v) => {
                    self.last = self.pos..r.p;
                    self.pos = r.p;
                    return Ok(Some(v));
                }
                Err(Short::More) if !self.eof => self.fill().await?,
                Err(Short::More) if self.pos == self.buf.len() => return Ok(None),
                Err(Short::More) => return Err(self.failure(self.pos, "la respuesta terminó en medio de una fila").await),
                Err(Short::Bad(m)) => return Err(self.failure(self.pos, &m).await),
                Err(Short::Unsupported(m)) => return Err(Error::Unsupported(m)),
            }
        }
    }
}

/// How long a window dropped with its commit in flight waits for the
/// server's answer before giving up on it (and asking it to stop).
const SETTLE: std::time::Duration = std::time::Duration::from_secs(120);

/// One commit window: an INSERT whose body is still being sent.
struct Window<'a> {
    tx: Option<mpsc::Sender<io::Result<Vec<u8>>>>,
    /// A sender that never carries data: its guaranteed slot takes the
    /// error that cuts the body when the window is dropped uncommitted.
    cut: Option<mpsc::Sender<io::Result<Vec<u8>>>>,
    task: Option<tokio::task::JoinHandle<Result<()>>>,
    /// `KILL QUERY` of this INSERT, for a window dropped while its commit
    /// was in flight and the server didn't answer in [`SETTLE`].
    kill: Option<Box<dyn FnOnce() + Send>>,
    /// The load's progress and its rows committed before this window: a
    /// window dropped while its commit is in flight reports its rows there
    /// if the server commits them.
    report: Option<(Progress<'a>, u64)>,
    buf: Vec<u8>,
    rows: u64,
    bytes: u64,
}

impl<'a> Window<'a> {
    /// Start the request `build` makes around the streamed body.
    fn start(build: impl FnOnce(reqwest::Body) -> reqwest::RequestBuilder, kill: Option<Box<dyn FnOnce() + Send>>) -> Window<'a> {
        let (tx, rx) = mpsc::channel::<io::Result<Vec<u8>>>(4);
        let req = build(reqwest::Body::wrap_stream(rx));
        let task = tokio::spawn(async move {
            let resp = req.send().await.map_err(net_err)?;
            let status = resp.status();
            if !status.is_success() {
                return Err(error_response(resp).await);
            }
            let text = resp.text().await.map_err(net_err)?;
            match body_exception(&text) {
                Some(e) => Err(e),
                None => Ok(()),
            }
        });
        Window {
            cut: Some(tx.clone()),
            tx: Some(tx),
            task: Some(task),
            kill,
            report: None,
            buf: Vec::with_capacity(SEND_CHUNK + 64 * 1024),
            rows: 0,
            bytes: 0,
        }
    }

    async fn flush(&mut self) -> Result<()> {
        if self.buf.is_empty() {
            return Ok(());
        }
        let chunk = std::mem::replace(&mut self.buf, Vec::with_capacity(SEND_CHUNK + 64 * 1024));
        let sent = match self.tx.as_mut() {
            Some(tx) => tx.send(Ok(chunk)).await.is_ok(),
            None => false,
        };
        if !sent {
            // The request ended early: its error says why.
            self.tx = None;
            return Err(match self.task.take() {
                Some(t) => match t.await {
                    Ok(Err(e)) => e,
                    Ok(Ok(())) => Error::Query("el servidor terminó la carga antes de tiempo".into()),
                    Err(e) => Error::Query(e.to_string()),
                },
                None => Error::State("carga cerrada".into()),
            });
        }
        Ok(())
    }

    /// Finish the body and wait for the server: the window is committed.
    /// `report`: the load's progress and its rows before this window, for
    /// a commit whose wait is dropped (see `Drop`).
    async fn commit(mut self, report: Option<(Progress<'a>, u64)>) -> Result<u64> {
        self.flush().await?;
        self.report = report;
        // Every sender gone: the body ends and the server commits.
        self.tx = None;
        self.cut = None;
        let task = self.task.as_mut().ok_or_else(|| Error::State("carga cerrada".into()))?;
        let done = task.await;
        self.task = None;
        done.map_err(|e| Error::Query(e.to_string()))??;
        Ok(self.rows)
    }
}

impl Drop for Window<'_> {
    /// Not committed: the body ends in an error, so the HTTP client aborts
    /// it instead of sending its last chunk, and the server discards the
    /// INSERT. Only aborting the task is not enough: the connection runs
    /// apart and would end the body cleanly (a late commit).
    fn drop(&mut self) {
        match self.cut.take() {
            Some(mut cut) => {
                let _ = cut.try_send(Err(io::Error::other("carga cancelada")));
                if let Some(t) = self.task.take() {
                    t.abort();
                }
            }
            // The body was complete and the commit in flight: the server
            // commits it even if nobody waits (HTTP has no way to take a
            // sent body back), so it's waited for here, and its rows
            // reported, before the load returns: nothing commits after.
            None => {
                use std::future::Future;
                let Some(mut t) = self.task.take() else { return };
                let wait = || {
                    let until = std::time::Instant::now() + SETTLE;
                    while !t.is_finished() && std::time::Instant::now() < until {
                        std::thread::sleep(std::time::Duration::from_millis(5));
                    }
                };
                match tokio::runtime::Handle::try_current().map(|h| h.runtime_flavor()) {
                    Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(wait),
                    // The request runs on this very thread: waiting would
                    // never end.
                    Ok(_) => {}
                    Err(_) => wait(),
                }
                let mut cx = std::task::Context::from_waker(std::task::Waker::noop());
                match std::pin::Pin::new(&mut t).poll(&mut cx) {
                    std::task::Poll::Ready(Ok(Ok(()))) => {
                        if let Some((progress, before)) = self.report {
                            progress(before + self.rows);
                        }
                    }
                    std::task::Poll::Ready(_) => {}
                    // No answer: ask the server to stop it (it may be too
                    // late).
                    std::task::Poll::Pending => {
                        t.abort();
                        if let Some(kill) = self.kill.take() {
                            kill();
                        }
                        tracing::warn!("ClickHouse: carga interrumpida con un envío sin respuesta; el servidor todavía puede guardarlo");
                    }
                }
            }
        }
    }
}

/// At most this much of a failed response's body is kept.
const ERROR_TAIL: usize = 64 * 1024;

/// A failed response's error. Its body can start with rows the server had
/// buffered before failing: only the exception at its end is kept.
async fn error_response(mut resp: reqwest::Response) -> Error {
    let status = resp.status().as_u16();
    let tag = ["x-clickhouse-exception-tag", "x-timeplus-exception-tag"]
        .iter()
        .find_map(|h| resp.headers().get(*h))
        .and_then(|v| v.to_str().ok())
        .map(str::to_string);
    let mut tail = Vec::new();
    while let Ok(Some(c)) = resp.chunk().await {
        tail.extend_from_slice(&c);
        if tail.len() > 2 * ERROR_TAIL {
            tail.drain(..tail.len() - ERROR_TAIL);
        }
    }
    server_error(status, &exception_text(&tail, tag.as_deref()))
}

/// The exception at the end of a body, without the rows before it.
pub(crate) fn exception_text(body: &[u8], tag: Option<&str>) -> String {
    const MAX: usize = 4096;
    let start = tag
        .and_then(|t| {
            let m = [MARK, t.as_bytes()].concat();
            find(body, &m).map(|i| i + m.len())
        })
        .or_else(|| body.windows(6).rposition(|w| w == b"Code: "));
    let text = match start {
        Some(i) => String::from_utf8_lossy(&body[i..]).into_owned(),
        None => match std::str::from_utf8(body) {
            Ok(s) if s.len() <= MAX => s.to_string(),
            _ => String::new(),
        },
    };
    let mut text = text.split("__exception__").next().unwrap_or_default().trim().to_string();
    // The block ends with `<length> <tag>`.
    if let Some(t) = tag {
        if let Some((head, last)) = text.rsplit_once('\n') {
            if last.trim().ends_with(t) && last.trim().starts_with(|c: char| c.is_ascii_digit()) {
                text = head.trim().to_string();
            }
        }
    }
    if text.len() > MAX {
        let mut end = MAX;
        while !text.is_char_boundary(end) {
            end -= 1;
        }
        text.truncate(end);
        text.push('…');
    }
    text
}

/// A read's column names, types and the server's time zone.
struct Header {
    names: Vec<String>,
    types: Vec<String>,
    server_utc: bool,
}

impl ClickHouseSession {
    fn request(&self, settings: &[(&str, String)]) -> (reqwest::RequestBuilder, String) {
        let query_id = uuid::Uuid::new_v4().to_string();
        let mut q: Vec<(&str, String)> = vec![("database", self.database.clone()), ("query_id", query_id.clone())];
        if self.sends_readonly() {
            q.push(("readonly", "1".into()));
        }
        q.extend(settings.iter().cloned());
        *self.in_flight.lock().unwrap_or_else(|e| e.into_inner()) = Some(query_id.clone());
        (self.conn.post().query(&q), query_id)
    }

    /// Run a query whose result streams back.
    async fn open(&self, sql: String) -> Result<reqwest::Response> {
        let resp = self.request(&[]).0.body(sql).send().await.map_err(net_err)?;
        if !resp.status().is_success() {
            return Err(error_response(resp).await);
        }
        Ok(resp)
    }

    /// Names and types of `select`'s columns (no rows read).
    async fn header(&self, select: &str) -> Result<Header> {
        let resp = self.open(format!("{select} LIMIT 0 FORMAT RowBinaryWithNamesAndTypes")).await?;
        let tz = resp.headers().get(self.flavor.header("timezone")).and_then(|v| v.to_str().ok()).map(str::to_string);
        let mut st = Stream::new(resp, self.flavor);
        let head = st.next(read_header).await;
        self.done();
        let (names, types) = head?.ok_or_else(|| Error::Query("la consulta no devolvió columnas".into()))?;
        Ok(Header { names, types, server_utc: tz.as_deref().is_none_or(utc) })
    }

    /// Start a commit window: `insert`, its body sent as it's produced.
    fn window(&self, insert: &str, settings: &[(&str, String)]) -> Window<'static> {
        let mut settings = settings.to_vec();
        settings.push(("query", insert.to_string()));
        let (req, id) = self.request(&settings);
        let (conn, rt) = (self.conn.clone(), self.rt.clone());
        let kill = move || {
            rt.spawn(async move {
                // The id is a UUID we generated: safe to inline.
                let sql = format!("KILL QUERY WHERE query_id = '{id}' ASYNC");
                if let Err(e) = conn.post().body(sql).send().await {
                    tracing::debug!("clickhouse kill query failed: {e}");
                }
            });
        };
        Window::start(|body| req.body(body), Some(Box::new(kill)))
    }

    /// Settings of a load's INSERTs: synchronous, no deduplication of
    /// identical windows, and a window in one block (one part: atomic).
    fn insert_settings(&self, spec: &LoadSpec) -> Vec<(&'static str, String)> {
        if self.flavor != Flavor::ClickHouse {
            return Vec::new();
        }
        let mut s = vec![("async_insert", "0".to_string()), ("insert_deduplicate", "0".to_string())];
        if spec.commit_rows > 0 {
            s.push(("max_insert_block_size", spec.commit_rows.max(1_048_576).to_string()));
        }
        s
    }
}

fn limits(spec: &LoadSpec) -> (u64, u64) {
    (
        if spec.commit_rows == 0 { u64::MAX } else { spec.commit_rows },
        if spec.commit_bytes == 0 { u64::MAX } else { spec.commit_bytes },
    )
}

// ---------------------------------------------------------------- reading

pub(crate) async fn read_batches(s: &mut ClickHouseSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let from = read_from(s.flavor, &spec.table);
    let cols = spec.columns.as_deref().map_or_else(|| "*".to_string(), column_list);
    let head = s.header(&format!("SELECT {cols} FROM {from}")).await?;
    if let Some((n, t)) = head.names.iter().zip(&head.types).find(|(_, t)| variable_type(t)) {
        return Err(variable_unsupported(n, t));
    }
    // Types the decoder doesn't know are read as text.
    let (exprs, json): (Vec<String>, Vec<bool>) = head
        .names
        .iter()
        .zip(&head.types)
        .map(|(n, t)| {
            let q = ident(n);
            match parse_type(t) {
                Some(_) => (q, false),
                None => (format!("toString({q}) AS {q}"), is_json(t)),
            }
        })
        .unzip();
    let columns: Vec<TransferColumn> = head
        .names
        .iter()
        .zip(&head.types)
        .map(|(n, t)| TransferColumn { name: n.clone(), type_name: t.clone(), nullable: super::is_nullable(t) })
        .collect();
    let select = format!(
        "SELECT {} FROM {from}{} FORMAT RowBinaryWithNamesAndTypes",
        exprs.join(", "),
        where_clause(spec.filter.as_deref())
    );
    let resp = s.open(select).await?;
    let mut st = Stream::new(resp, s.flavor);
    let result = async {
        let (_, types) = st.next(read_header).await?.ok_or_else(|| Error::Query("la respuesta no trae encabezado".into()))?;
        let tys = types
            .iter()
            .map(|t| parse_type(t).ok_or_else(|| Error::Query(format!("tipo no soportado en la lectura: {t}"))))
            .collect::<Result<Vec<_>>>()?;
        if tys.len() != columns.len() {
            return Err(Error::Query("la lectura devolvió otras columnas".into()));
        }
        sink.lock().map_err(lock_err)?.begin(&columns)?;
        let mut builder = BatchBuilder::new();
        let row = |r: &mut Rd| -> D<Vec<Cell>> {
            let mut cells = Vec::with_capacity(tys.len());
            for (t, j) in tys.iter().zip(&json) {
                let c = read_cell(r, t)?;
                cells.push(match c {
                    Cell::Text(s) if *j => Cell::Json(s),
                    c => c,
                });
            }
            Ok(cells)
        };
        while let Some(cells) = st.next(row).await? {
            builder.push(cells, &mut *sink.lock().map_err(lock_err)?)?;
        }
        builder.flush(&mut *sink.lock().map_err(lock_err)?)?;
        Ok(builder.rows)
    }
    .await;
    s.done();
    result
}

// ---------------------------------------------------------------- loading

/// How a target column is written.
enum Col {
    /// RowBinary of its own type.
    Direct(Ty),
    /// Through `input()`: sent as `send` and converted by the server.
    Input { send: Ty, how: Conv },
}

/// The server-side conversion of an `input()` column.
enum Conv {
    /// Implicit, into the column's type (unknown types, sent as text).
    Plain,
    /// `parseDateTime64BestEffort(c, scale[, tz])`: a zoned `DateTime`.
    BestEffort { scale: u32, tz: Option<String> },
    /// `CAST(c, '<the column's type>')`, with strings parsed best-effort: a
    /// zoned `DateTime` nested in an Array, Map or Tuple.
    Cast,
}

impl Col {
    fn encode(&self, c: &Cell, out: &mut Vec<u8>) -> std::result::Result<(), String> {
        match self {
            Col::Direct(t) | Col::Input { send: t, .. } => encode(t, Sc::of(c), out),
        }
    }
}

/// Text through `input()`: `String`, `Nullable(String)` when the column
/// takes NULL (so a NULL into one that doesn't is an error, never its
/// default).
fn text_of(nullable: bool) -> Ty {
    if nullable {
        Ty::Nullable(Box::new(Ty::Str))
    } else {
        Ty::Str
    }
}

fn load_plan(head: &Header, flavor: Flavor) -> Result<Vec<Col>> {
    head.names
        .iter()
        .zip(&head.types)
        .map(|(name, t)| {
            if variable_type(t) {
                return Err(variable_unsupported(name, t));
            }
            Ok(match parse_type(t) {
                None => Col::Input { send: text_of(super::is_nullable(t)), how: Conv::Plain },
                Some(ty) if zoned(&ty, head.server_utc) => {
                    let (inner, nullable) = match &ty {
                        Ty::Nullable(t) => (t.as_ref(), true),
                        t => (t, false),
                    };
                    // Sent as text, checked here first: the server's
                    // best-effort parser rolls `99:99:99` over.
                    let send = || {
                        let t = Ty::DtText(Box::new(inner.clone()));
                        if nullable {
                            Ty::Nullable(Box::new(t))
                        } else {
                            t
                        }
                    };
                    match inner {
                        Ty::DateTime { tz } => Col::Input { send: send(), how: Conv::BestEffort { scale: 0, tz: tz.clone() } },
                        Ty::DateTime64 { scale, tz } => Col::Input { send: send(), how: Conv::BestEffort { scale: *scale, tz: tz.clone() } },
                        _ if flavor != Flavor::ClickHouse => {
                            return Err(Error::Unsupported(format!(
                                "la columna «{name}» ({t}) tiene fechas con zona horaria dentro de un array, map o tuple, \
                                 y este servidor no puede convertirlas al cargarlas"
                            )))
                        }
                        _ => Col::Input { send: text_leaves(&ty, head.server_utc), how: Conv::Cast },
                    }
                }
                Some(ty) => Col::Direct(ty),
            })
        })
        .collect()
}

fn insert_sql(table: &str, cols: &str, plan: &[Col], types: &[String]) -> String {
    if plan.iter().all(|c| matches!(c, Col::Direct(_))) {
        return format!("INSERT INTO {table} ({cols}) FORMAT RowBinary");
    }
    let mut structure = Vec::new();
    let mut exprs = Vec::new();
    for (i, (c, t)) in plan.iter().zip(types).enumerate() {
        let name = format!("c{i}");
        match c {
            Col::Direct(_) => {
                structure.push(format!("{name} {t}"));
                exprs.push(name);
            }
            Col::Input { send, how } => {
                structure.push(format!("{name} {}", render(send)));
                exprs.push(match how {
                    Conv::Plain => name,
                    Conv::BestEffort { scale, tz } => {
                        let tz = tz.as_deref().map(|z| format!(", '{}'", literal(z))).unwrap_or_default();
                        if matches!(send, Ty::Nullable(_)) {
                            // Over a Nullable the function also parses the
                            // NULL slots ('') and throws; `if` short-circuits
                            // them. Not `…OrNull`: garbage must be an error.
                            format!("if(isNull({name}), NULL, parseDateTime64BestEffort(assumeNotNull({name}), {scale}{tz}))")
                        } else {
                            format!("parseDateTime64BestEffort({name}, {scale}{tz})")
                        }
                    }
                    Conv::Cast => format!("CAST({name}, '{}')", literal(t)),
                });
            }
        }
    }
    format!(
        "INSERT INTO {table} ({cols}) SELECT {} FROM input('{}') FORMAT RowBinary",
        exprs.join(", "),
        literal(&structure.join(", "))
    )
}

pub(crate) async fn bulk_load(s: &mut ClickHouseSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if spec.columns.is_empty() {
        return Err(Error::Query("la carga no tiene columnas".into()));
    }
    let cols = column_list(&spec.columns);
    let head = s.header(&format!("SELECT {cols} FROM {}", read_from(s.flavor, &spec.table))).await?;
    let plan = load_plan(&head, s.flavor)?;
    let table = qualified(spec.table.schema(), &spec.table.name);
    let insert = insert_sql(&table, &cols, &plan, &head.types);
    let mut settings = s.insert_settings(spec);
    if plan.iter().any(|c| matches!(c, Col::Input { how: Conv::Cast, .. })) {
        // Text with an offset (`…+00:00`) in the nested zoned DateTimes.
        settings.push(("cast_string_to_date_time_mode", "best_effort".into()));
    }
    if s.flavor == Flavor::ClickHouse && plan.iter().any(|c| matches!(c, Col::Input { send: Ty::Nullable(_), how: Conv::BestEffort { .. } })) {
        // The NULL guard above relies on `if` evaluating lazily (the
        // default, but a profile can turn it off).
        settings.push(("short_circuit_function_evaluation", "enable".into()));
    }
    let (max_rows, max_bytes) = limits(spec);

    let mut total = 0u64;
    let mut window: Option<Window> = None;
    let result = async {
        while let Some(batch) = source.next().await {
            for row in &batch.rows {
                if row.len() != plan.len() {
                    return Err(Error::Query(format!("una fila trae {} valores y la carga tiene {} columnas", row.len(), plan.len())));
                }
                let w = match &mut window {
                    Some(w) => w,
                    None => window.insert(s.window(&insert, &settings)),
                };
                let before = w.buf.len();
                for (i, (c, col)) in row.iter().zip(&plan).enumerate() {
                    col.encode(c, &mut w.buf)
                        .map_err(|e| Error::Query(format!("fila {}, columna «{}»: {e}", total + w.rows + 1, spec.columns[i])))?;
                }
                w.rows += 1;
                w.bytes += (w.buf.len() - before) as u64;
                if w.buf.len() >= SEND_CHUNK {
                    w.flush().await?;
                }
                if w.rows >= max_rows || w.bytes >= max_bytes {
                    total += window.take().expect("window").commit(Some((progress, total))).await?;
                    progress(total);
                }
            }
        }
        if let Some(w) = window.take() {
            total += w.commit(Some((progress, total))).await?;
            progress(total);
        }
        Ok(total)
    }
    .await;
    s.done();
    result
}

// ---------------------------------------------------------------- native copy

fn ch_session(s: &mut dyn Session) -> Option<&mut ClickHouseSession> {
    s.as_any()?.downcast_mut::<ClickHouseSession>()
}

pub(crate) async fn copy_native(source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
    let unsupported = |why: String| Error::Unsupported(format!("copia directa no disponible: {why}"));
    let (Some(src), Some(dst)) = (ch_session(source), ch_session(target)) else {
        return Err(unsupported("las sesiones no son de ClickHouse".into()));
    };
    if src.flavor != dst.flavor {
        return Err(unsupported("origen y destino son motores distintos".into()));
    }
    let src_from = read_from(src.flavor, &spec.source.table);
    let src_cols = spec.source.columns.as_deref().map_or_else(|| "*".to_string(), column_list);
    let a = src.header(&format!("SELECT {src_cols} FROM {src_from}")).await?;
    let dst_cols = column_list(&spec.target.columns);
    let b = dst.header(&format!("SELECT {dst_cols} FROM {}", read_from(dst.flavor, &spec.target.table))).await?;
    if a.types.len() != b.types.len() {
        return Err(unsupported("origen y destino tienen distinta cantidad de columnas".into()));
    }
    // Types the walker doesn't know (JSON…) travel as
    // their text and the target parses them back (through `input()`).
    let mut tys = Vec::with_capacity(a.types.len());
    let (mut exprs, mut structure, mut as_text) = (Vec::new(), Vec::new(), false);
    for (i, ((sn, st), (dn, dt))) in a.names.iter().zip(&a.types).zip(b.names.iter().zip(&b.types)).enumerate() {
        if st != dt {
            return Err(unsupported(format!("la columna «{sn}» ({st}) no tiene el mismo tipo en «{dn}» ({dt})")));
        }
        if variable_type(st) {
            return Err(variable_unsupported(sn, st));
        }
        let q = ident(sn);
        match parse_type(st) {
            Some(t) => {
                tys.push(t);
                exprs.push(q);
                structure.push(format!("c{i} {st}"));
            }
            None if st.to_ascii_lowercase().contains("aggregatefunction") => {
                return Err(unsupported(format!("la columna «{sn}» guarda estados de agregación ({st})")));
            }
            None => {
                let nullable = super::is_nullable(st);
                tys.push(if nullable { Ty::Nullable(Box::new(Ty::Str)) } else { Ty::Str });
                exprs.push(format!("toString({q})"));
                structure.push(format!("c{i} {}", if nullable { "Nullable(String)" } else { "String" }));
                as_text = true;
            }
        }
    }
    let select = format!(
        "SELECT {} FROM {src_from}{} FORMAT RowBinary",
        exprs.join(", "),
        where_clause(spec.source.filter.as_deref())
    );
    let table = qualified(spec.target.table.schema(), &spec.target.table.name);
    let insert = if as_text {
        let inputs: Vec<String> = (0..tys.len()).map(|i| format!("c{i}")).collect();
        format!(
            "INSERT INTO {table} ({dst_cols}) SELECT {} FROM input('{}') FORMAT RowBinary",
            inputs.join(", "),
            literal(&structure.join(", "))
        )
    } else {
        format!("INSERT INTO {table} ({dst_cols}) FORMAT RowBinary")
    };
    let settings = dst.insert_settings(&spec.target);
    let (max_rows, max_bytes) = limits(&spec.target);

    let resp = src.open(select).await?;
    let mut st = Stream::new(resp, src.flavor);
    let mut total = 0u64;
    let mut window: Option<Window> = None;
    let walk = |r: &mut Rd| -> D<()> {
        for t in &tys {
            skip(r, t)?;
        }
        Ok(())
    };
    let result = async {
        while st.next(walk).await?.is_some() {
            let w = match &mut window {
                Some(w) => w,
                None => window.insert(dst.window(&insert, &settings)),
            };
            let row = &st.buf[st.last.clone()];
            w.buf.extend_from_slice(row);
            w.rows += 1;
            w.bytes += row.len() as u64;
            if w.buf.len() >= SEND_CHUNK {
                w.flush().await?;
            }
            if w.rows >= max_rows || w.bytes >= max_bytes {
                total += window.take().expect("window").commit(Some((progress, total))).await?;
                progress(total);
            }
        }
        if let Some(w) = window.take() {
            total += w.commit(Some((progress, total))).await?;
            progress(total);
        }
        Ok(total)
    }
    .await;
    src.done();
    dst.done();
    result
}

#[cfg(test)]
mod tests {
    use super::*;

    fn round(ty: &str, cell: Cell) -> Cell {
        let t = parse_type(ty).unwrap_or_else(|| panic!("{ty}"));
        let mut out = Vec::new();
        encode(&t, Sc::of(&cell), &mut out).unwrap_or_else(|e| panic!("{ty}: {e}"));
        let mut r = Rd::new(&out);
        let back = read_cell(&mut r, &t).unwrap();
        assert_eq!(r.p, out.len(), "{ty}: bytes left");
        let mut r = Rd::new(&out);
        skip(&mut r, &t).unwrap();
        assert_eq!(r.p, out.len(), "{ty}: skip");
        back
    }

    fn same(ty: &str, cell: Cell) {
        assert_eq!(round(ty, cell.clone()), cell, "{ty}");
    }

    #[test]
    fn types_parse() {
        assert_eq!(parse_type("LowCardinality(Nullable(String))"), Some(Ty::Nullable(Box::new(Ty::Str))));
        assert_eq!(parse_type("nullable(int64)"), Some(Ty::Nullable(Box::new(Ty::Int { bytes: 8, signed: true }))));
        assert_eq!(parse_type("fixed_string(4)"), Some(Ty::Fixed(4)));
        assert_eq!(parse_type("Decimal(76, 3)"), Some(Ty::Decimal { bytes: 32, scale: 3 }));
        assert_eq!(parse_type("Decimal(10, 2)"), Some(Ty::Decimal { bytes: 8, scale: 2 }));
        assert_eq!(parse_type("DateTime64(6, 'Europe/Madrid')"), Some(Ty::DateTime64 { scale: 6, tz: Some("Europe/Madrid".into()) }));
        assert_eq!(
            parse_type(r"Enum8('c' = -2, 'a\'b' = 1)"),
            Some(Ty::Enum { bytes: 1, values: vec![(-2, "c".into()), (1, "a'b".into())] })
        );
        assert_eq!(
            parse_type("Tuple(p UInt8, `q r` String)"),
            Some(Ty::Tuple(vec![(Some("p".into()), Ty::Int { bytes: 1, signed: false }), (Some("q r".into()), Ty::Str)]))
        );
        assert_eq!(parse_type("Map(String, Array(UInt8))").map(|t| matches!(t, Ty::Map(..))), Some(true));
        assert_eq!(parse_type("Point"), Some(point()));
        assert_eq!(parse_type("SimpleAggregateFunction(sum, UInt64)"), Some(Ty::Int { bytes: 8, signed: false }));
        assert!(parse_type("JSON").is_none());
        assert!(parse_type("Dynamic").is_none());
        assert!(parse_type("Array(Variant(String, UInt8))").is_none());
        assert!(parse_type("AggregateFunction(uniq, String)").is_none());
    }

    #[test]
    fn every_type_round_trips() {
        same("Int8", Cell::Int(-128));
        same("Int64", Cell::Int(i64::MIN));
        same("UInt8", Cell::Int(255));
        same("UInt32", Cell::Int(4_294_967_295));
        same("UInt64", Cell::UInt(u64::MAX));
        same("Int128", Cell::Decimal("-170141183460469231731687303715884105728".into()));
        same("UInt128", Cell::Decimal("340282366920938463463374607431768211455".into()));
        same("Int256", Cell::Decimal("-57896044618658097711785492504343953926634992332820282019728792003956564819968".into()));
        same("Int256", Cell::Decimal("57896044618658097711785492504343953926634992332820282019728792003956564819967".into()));
        same("UInt256", Cell::Decimal("115792089237316195423570985008687907853269984665640564039457584007913129639935".into()));
        same("Int256", Cell::Decimal("-1".into()));
        same("Float64", Cell::Float(1.5e300));
        same("Float32", Cell::Float(0.5));
        same("Bool", Cell::Bool(true));
        same("Decimal(10, 2)", Cell::Decimal("-12345678.90".into()));
        same("Decimal(9, 9)", Cell::Decimal("0.000000001".into()));
        same("Decimal(76, 10)", Cell::Decimal("-123456789012345678901234567890123456789012345678901234567890.0123456789".into()));
        same("String", Cell::Text("héllo".into()));
        same("String", Cell::Bytes(vec![0xff, 0x00, 0xfe]));
        same("FixedString(4)", Cell::Text("ab".into()));
        same("FixedString(2)", Cell::Bytes(vec![0x00, 0xff]));
        same("Date", Cell::Date("2149-06-06".into()));
        same("Date32", Cell::Date("1900-01-01".into()));
        same("DateTime", Cell::DateTimeTz("2024-02-29 23:59:59+00:00".into()));
        same("DateTime64(3)", Cell::DateTimeTz("1969-12-31 23:59:59.999+00:00".into()));
        same("DateTime64(9, 'UTC')", Cell::DateTimeTz("2262-04-11 23:47:16.854775807+00:00".into()));
        same("UUID", Cell::Uuid("61f0c404-5cb3-11e7-907b-a6006ad3dba0".into()));
        same("IPv4", Cell::Text("116.106.34.242".into()));
        same("IPv6", Cell::Text("2001:db8::ff00:42:8329".into()));
        same("Enum16('a' = 1, 'b' = 1000)", Cell::Text("b".into()));
        same("Nullable(Int32)", Cell::Null);
        same("Array(Nullable(Int32))", Cell::Json("[1,null,3]".into()));
        same("Map(String, Array(UInt8))", Cell::Json(r#"{"a":[1,2]}"#.into()));
        same("Tuple(p UInt8, q String)", Cell::Json(r#"{"p":1,"q":"x"}"#.into()));
        same("Tuple(UInt8, Decimal(5, 2))", Cell::Json(r#"[1,"2.50"]"#.into()));
        same("Array(DateTime)", Cell::Json(r#"["2024-01-01 10:00:00+00:00"]"#.into()));
        same("MultiPolygon", Cell::Json("[[[[1.0,2.0],[3.0,4.0]]]]".into()));
    }

    #[test]
    fn values_convert_to_the_target_type() {
        assert_eq!(round("DateTime", Cell::DateTimeTz("2024-01-01 12:00:00+02:00".into())), Cell::DateTimeTz("2024-01-01 10:00:00+00:00".into()));
        assert_eq!(round("DateTime", Cell::DateTime("2024-01-01T12:00:00".into())), Cell::DateTimeTz("2024-01-01 12:00:00+00:00".into()));
        assert_eq!(round("DateTime64(3)", Cell::Text("2024-01-01 12:00:00.123000Z".into())), Cell::DateTimeTz("2024-01-01 12:00:00.123+00:00".into()));
        assert_eq!(round("Date", Cell::DateTimeTz("2024-05-06 00:00:00-05:00".into())), Cell::Date("2024-05-06".into()));
        assert_eq!(round("Decimal(10, 2)", Cell::Decimal("1.005".into())), Cell::Decimal("1.01".into()));
        assert_eq!(round("Decimal(10, 2)", Cell::Decimal("-1.004".into())), Cell::Decimal("-1.00".into()));
        assert_eq!(round("Decimal(10, 2)", Cell::Decimal("9.999".into())), Cell::Decimal("10.00".into()));
        assert_eq!(round("Decimal(10, 2)", Cell::Float(2.5)), Cell::Decimal("2.50".into()));
        assert_eq!(round("Decimal(10, 2)", Cell::Int(7)), Cell::Decimal("7.00".into()));
        assert_eq!(round("Int32", Cell::Text("42".into())), Cell::Int(42));
        assert_eq!(round("Int32", Cell::Float(3.0)), Cell::Int(3));
        assert_eq!(round("UInt8", Cell::Bool(true)), Cell::Int(1));
        assert_eq!(round("Int128", Cell::Int(-5)), Cell::Decimal("-5".into()));
        assert_eq!(round("String", Cell::Int(5)), Cell::Text("5".into()));
        assert_eq!(round("String", Cell::Json("{\"a\":1}".into())), Cell::Text("{\"a\":1}".into()));
        assert_eq!(round("UUID", Cell::Bytes((0u8..16).collect())), Cell::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into()));
        assert_eq!(round("IPv6", Cell::Text("1.2.3.4".into())), Cell::Text("::ffff:1.2.3.4".into()));
        assert_eq!(round("Enum8('a' = 1)", Cell::Int(1)), Cell::Text("a".into()));
        assert_eq!(round("Bool", Cell::Text("false".into())), Cell::Bool(false));
        assert_eq!(round("Float64", Cell::Decimal("0.1".into())), Cell::Float(0.1));
        assert_eq!(round("Map(UInt8, String)", Cell::Json(r#"[[1,"x"]]"#.into())), Cell::Json(r#"{"1":"x"}"#.into()));
    }

    #[test]
    fn bad_values_are_errors() {
        let enc = |ty: &str, c: Cell| encode(&parse_type(ty).unwrap(), Sc::of(&c), &mut Vec::new());
        assert!(enc("Int8", Cell::Int(128)).is_err());
        assert!(enc("UInt64", Cell::Int(-1)).is_err());
        assert!(enc("Int256", Cell::Decimal("57896044618658097711785492504343953926634992332820282019728792003956564819968".into())).is_err());
        assert!(enc("Int32", Cell::Null).is_err());
        assert!(enc("Decimal(4, 2)", Cell::Decimal("100.00".into())).is_ok());
        assert!(enc("Decimal(4, 2)", Cell::Decimal("1e5".into())).is_err());
        assert!(enc("FixedString(2)", Cell::Text("abc".into())).is_err());
        assert!(enc("Date", Cell::Text("1969-12-31".into())).is_err());
        assert!(enc("Enum8('a' = 1)", Cell::Text("z".into())).is_err());
        assert!(enc("Array(Int8)", Cell::Text("nope".into())).is_err());
        assert!(enc("UUID", Cell::Text("xyz".into())).is_err());
    }

    #[test]
    fn truncated_rows_ask_for_more() {
        let t = parse_type("Array(String)").unwrap();
        let mut out = Vec::new();
        encode(&t, Sc::of(&Cell::Json(r#"["abc","def"]"#.into())), &mut out).unwrap();
        for cut in 0..out.len() {
            assert_eq!(read_cell(&mut Rd::new(&out[..cut]), &t), Err(Short::More));
            assert_eq!(skip(&mut Rd::new(&out[..cut]), &t), Err(Short::More));
        }
    }

    #[test]
    fn plans_route_zoned_and_unknown_columns_through_input() {
        let head = Header {
            names: vec!["a".into(), "b".into(), "c".into(), "d".into(), "e".into()],
            types: vec!["Int32".into(), "DateTime('Europe/Madrid')".into(), "JSON".into(), "DateTime".into(), "Nullable(DateTime('Asia/Tokyo'))".into()],
            server_utc: true,
        };
        let plan = load_plan(&head, Flavor::ClickHouse).unwrap();
        let sql = insert_sql("`t`", "`a`, `b`, `c`, `d`, `e`", &plan, &head.types);
        assert_eq!(
            sql,
            "INSERT INTO `t` (`a`, `b`, `c`, `d`, `e`) SELECT c0, parseDateTime64BestEffort(c1, 0, 'Europe/Madrid'), c2, c3, \
             if(isNull(c4), NULL, parseDateTime64BestEffort(assumeNotNull(c4), 0, 'Asia/Tokyo')) \
             FROM input('c0 Int32, c1 String, c2 String, c3 DateTime, c4 Nullable(String)') FORMAT RowBinary"
        );
        let head = Header { names: vec!["a".into()], types: vec!["Nullable(DateTime64(3))".into()], server_utc: false };
        let plan = load_plan(&head, Flavor::ClickHouse).unwrap();
        assert!(insert_sql("t", "a", &plan, &head.types).contains("parseDateTime64BestEffort(assumeNotNull(c0), 3)"));
        let head = Header { names: vec!["a".into()], types: vec!["Int8".into()], server_utc: false };
        assert_eq!(insert_sql("t", "a", &load_plan(&head, Flavor::ClickHouse).unwrap(), &head.types), "INSERT INTO t (a) FORMAT RowBinary");
    }

    /// Dynamic and Variant can't be carried as text without losing NULL and
    /// each value's subtype: Unsupported, never silent.
    #[test]
    fn dynamic_and_variant_columns_are_unsupported() {
        for t in ["Dynamic", "Dynamic(max_types=8)", "Variant(String, UInt64)", "Array(Variant(String, UInt8))", "Tuple(a Int8, `b c` Dynamic)", "Map(String, Dynamic)"] {
            assert!(variable_type(t), "{t}");
            let head = Header { names: vec!["v".into()], types: vec![t.into()], server_utc: true };
            let Err(Error::Unsupported(m)) = load_plan(&head, Flavor::ClickHouse) else { panic!("{t}") };
            assert!(m.contains("«v»") && m.contains("Dynamic o Variant"), "{m}");
        }
        for t in ["String", "JSON", "Tuple(dynamic String, variant Int8)", "Enum8('Dynamic' = 1)", "Nullable(DateTime('Asia/Tokyo'))"] {
            assert!(!variable_type(t), "{t}");
        }
    }

    /// Messages name types as ClickHouse spells them, not Rust's Debug.
    #[test]
    fn nested_errors_speak_the_users_types() {
        let head = Header { names: vec!["a".into()], types: vec!["Array(DateTime('Asia/Tokyo'))".into()], server_utc: true };
        let plan = load_plan(&head, Flavor::ClickHouse).unwrap();
        let e = plan[0].encode(&Cell::Json("[null]".into()), &mut Vec::new()).unwrap_err();
        assert_eq!(e, "null en un elemento de tipo DateTime('Asia/Tokyo') que no admite nulos");
        let e = plan[0].encode(&Cell::Json("[[1]]".into()), &mut Vec::new()).unwrap_err();
        assert!(!e.contains("DtText") && !e.contains("Some("), "{e}");
        let e = encode(&parse_type("Array(Int8)").unwrap(), Sc::S("nope".into()), &mut Vec::new()).unwrap_err();
        assert!(!e.contains("S(") && !e.contains("Int {"), "{e}");
        let t = parse_type("Tuple(x Int8, y Nullable(Int8))").unwrap();
        assert_eq!(encode(&t, Sc::S(r#"{"y":1}"#.into()), &mut Vec::new()).unwrap_err(), "falta el elemento «x» de Tuple(`x` Int8, `y` Nullable(Int8))");
        let mut out = Vec::new();
        encode(&t, Sc::S(r#"{"x":1}"#.into()), &mut out).unwrap();
        assert_eq!(out, [1, 1]);
        let e = encode(&Ty::Int { bytes: 1, signed: true }, Sc::S("x".into()), &mut Vec::new()).unwrap_err();
        assert!(e.contains("«x»"), "{e}");
    }

    /// A nullable zoned DateTime guards its NULL slots: the parse function
    /// runs over the nested column too and throws on them.
    #[test]
    fn nullable_zoned_datetimes_skip_null_slots() {
        let head = Header { names: vec!["d".into()], types: vec!["Nullable(DateTime('Asia/Tokyo'))".into()], server_utc: true };
        let sql = insert_sql("t", "d", &load_plan(&head, Flavor::ClickHouse).unwrap(), &head.types);
        assert!(sql.contains("if(isNull(c0), NULL, parseDateTime64BestEffort(assumeNotNull(c0), 0, 'Asia/Tokyo'))"), "{sql}");
        assert!(!sql.contains("OrNull"));
        let head = Header { names: vec!["d".into()], types: vec!["DateTime('Asia/Tokyo')".into()], server_utc: true };
        let sql = insert_sql("t", "d", &load_plan(&head, Flavor::ClickHouse).unwrap(), &head.types);
        assert!(sql.contains("SELECT parseDateTime64BestEffort(c0, 0, 'Asia/Tokyo')"), "{sql}");
    }

    /// A NULL into a non-nullable column that goes through `input()` is an
    /// error, not the type's default.
    #[test]
    fn nulls_into_non_nullable_input_columns_are_errors() {
        let head = Header {
            names: vec!["d".into(), "j".into(), "n".into()],
            types: vec!["DateTime('Asia/Tokyo')".into(), "JSON".into(), "Nullable(DateTime('Asia/Tokyo'))".into()],
            server_utc: true,
        };
        let plan = load_plan(&head, Flavor::ClickHouse).unwrap();
        for c in &plan[..2] {
            assert_eq!(c.encode(&Cell::Null, &mut Vec::new()).unwrap_err(), "NULL en una columna que no admite nulos");
        }
        assert!(plan[2].encode(&Cell::Null, &mut Vec::new()).is_ok());
    }

    /// Zoned DateTimes nested in an Array/Map/Tuple go as `String` leaves
    /// and are `CAST` by the server (not `JSONExtract` over a Nullable).
    #[test]
    fn nested_zoned_columns_are_cast_from_string_leaves() {
        let types = vec![
            "UInt8".into(),
            "Array(DateTime('Asia/Tokyo'))".into(),
            "Map(UInt8, Nullable(DateTime64(3, 'Asia/Tokyo')))".into(),
            "Tuple(a DateTime('UTC'), `b c` DateTime)".into(),
        ];
        let head = Header { names: vec!["i".into(), "a".into(), "m".into(), "t".into()], types, server_utc: false };
        let plan = load_plan(&head, Flavor::ClickHouse).unwrap();
        let sql = insert_sql("t", "i, a, m, t", &plan, &head.types);
        assert_eq!(
            sql,
            r"INSERT INTO t (i, a, m, t) SELECT c0, CAST(c1, 'Array(DateTime(\'Asia/Tokyo\'))'), CAST(c2, 'Map(UInt8, Nullable(DateTime64(3, \'Asia/Tokyo\')))'), CAST(c3, 'Tuple(a DateTime(\'UTC\'), `b c` DateTime)') FROM input('c0 UInt8, c1 Array(String), c2 Map(UInt8, Nullable(String)), c3 Tuple(`a` DateTime(\'UTC\'), `b c` String)') FORMAT RowBinary"
        );
        // Leaves are checked on the client.
        let mut out = Vec::new();
        plan[1].encode(&Cell::Json(r#"["2024-01-01 10:00:00+00:00","2024-01-01 10:00:00"]"#.into()), &mut out).unwrap();
        assert!(plan[1].encode(&Cell::Json(r#"["garbage"]"#.into()), &mut Vec::new()).is_err());
        assert!(matches!(load_plan(&head, Flavor::Timeplus), Err(Error::Unsupported(_))));
    }

    #[test]
    fn identifiers_escape_backslashes_and_backticks() {
        assert_eq!(ident("a\\"), "`a\\\\`");
        assert_eq!(ident("x`y"), "`x\\`y`");
        assert_eq!(ident("x\\`) SELECT 1 --"), "`x\\\\\\`) SELECT 1 --`");
        assert_eq!(qualified(Some("d b"), "t`"), "`d b`.`t\\``");
    }

    /// A Map keeps repeated keys and their order; bytes that aren't UTF-8
    /// inside a nested value are refused, not replaced.
    #[test]
    fn nested_values_are_lossless() {
        let t = parse_type("Map(String, UInt8)").unwrap();
        let mut out = Vec::new();
        encode(&t, Sc::of(&Cell::Json(r#"{"b":1,"a":2,"b":3}"#.into())), &mut out).unwrap();
        assert_eq!(out, [3, 1, b'b', 1, 1, b'a', 2, 1, b'b', 3]);
        assert_eq!(read_cell(&mut Rd::new(&out), &t).unwrap(), Cell::Json(r#"{"b":1,"a":2,"b":3}"#.into()));
        same("Array(Map(String, Array(Int8)))", Cell::Json(r#"[{"z":[1],"a":[],"z":[-1]}]"#.into()));
        same("Array(Int128)", Cell::Json(r#"["-170141183460469231731687303715884105728"]"#.into()));
        same("Tuple(z UInt8, a String)", Cell::Json(r#"{"z":1,"a":"x"}"#.into()));
        assert_eq!(round("Array(Decimal(38, 20))", Cell::Json("[12345678901234567.12345678901234567891]".into())), Cell::Json(r#"["12345678901234567.12345678901234567891"]"#.into()));
        assert!(encode(&parse_type("Tuple(a UInt8)").unwrap(), Sc::of(&Cell::Json(r#"{"a":1,"x":2}"#.into())), &mut Vec::new()).is_err());
        let t = parse_type("Array(String)").unwrap();
        let bytes = [1u8, 3, 0xff, 0x00, 0xfe];
        assert!(matches!(read_cell(&mut Rd::new(&bytes), &t), Err(Short::Unsupported(_))));
        let t = parse_type("Map(String, UInt8)").unwrap();
        assert!(matches!(read_cell(&mut Rd::new(&[1, 1, 0xff, 7]), &t), Err(Short::Unsupported(_))));
    }

    #[test]
    fn json_parser_keeps_order_and_spelling() {
        let j = parse_json(r#" {"b": [1, -2.50e3, true, null, "\u00e9\""], "a": {}, "b": 1e400} "#).unwrap();
        assert_eq!(j.text(), r#"{"b":[1,-2.50e3,true,null,"é\""],"a":{},"b":1e400}"#);
        for bad in ["", "{", "[1,]", "{\"a\"}", "tru", "1 2", "\"x", "[01]", "[1.]", "[-]", "[1e]", "[.5]"] {
            assert!(parse_json(bad).is_err(), "{bad}");
        }
    }

    /// An error body that starts with buffered rows yields only the
    /// exception.
    #[test]
    fn error_bodies_keep_only_the_exception() {
        let mut body = vec![0xffu8; 100_000];
        body.extend_from_slice(b"Code: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)\n");
        assert_eq!(exception_text(&body, None), "Code: 395. DB::Exception: boom. (FUNCTION_THROW_IF_VALUE_IS_NON_ZERO)");
        let mut body = b"\x01\x02Code: 1 in data".to_vec();
        body.extend_from_slice(b"\r\n__exception__\r\nabcdef\r\nCode: 395. DB::Exception: boom\r\n30 abcdef\r\n__exception__\r\n");
        assert_eq!(exception_text(&body, Some("abcdef")), "Code: 395. DB::Exception: boom");
        assert_eq!(exception_text(&[0xff; 10_000], None), "");
        assert_eq!(exception_text(b"Syntax error", None), "Syntax error");
    }

    /// A local HTTP server that records what arrives; it answers 200 once
    /// the chunked body ends.
    async fn recorder() -> (String, tokio::task::JoinHandle<(Vec<u8>, bool)>) {
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let server = tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let mut got = Vec::new();
            let mut buf = [0u8; 65536];
            loop {
                match tokio::time::timeout(std::time::Duration::from_secs(5), sock.read(&mut buf)).await {
                    Ok(Ok(n)) if n > 0 => got.extend_from_slice(&buf[..n]),
                    _ => return (got, false),
                }
                if got.ends_with(b"\r\n0\r\n\r\n") {
                    let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
                    return (got, true);
                }
            }
        });
        (url, server)
    }

    /// A window dropped before its commit must not end its body cleanly
    /// (the server would commit it after the load returned).
    #[tokio::test]
    async fn dropped_window_never_finishes_its_body() {
        let client = reqwest::Client::new();
        for _ in 0..20 {
            let (url, server) = recorder().await;
            let mut w = Window::start(|b| client.post(&url).body(b), None);
            for i in 0..8u8 {
                w.buf.extend(std::iter::repeat_n(i, 300_000));
                w.flush().await.unwrap();
            }
            drop(w);
            let (got, finished) = server.await.unwrap();
            assert!(!finished && !got.ends_with(b"\r\n0\r\n\r\n"), "the body ended cleanly ({} bytes)", got.len());
        }
        // Committed, the body does end.
        let (url, server) = recorder().await;
        let mut w = Window::start(|b| client.post(&url).body(b), None);
        w.buf.extend_from_slice(b"rows");
        w.rows = 1;
        assert_eq!(w.commit(None).await.unwrap(), 1);
        assert!(server.await.unwrap().1);
    }

    /// A load dropped once a window's whole body was sent (the server will
    /// commit it) waits for the answer and reports its rows: the window
    /// never commits after the load has returned.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn dropped_commit_waits_for_the_server() {
        use std::sync::atomic::{AtomicBool, AtomicU64, Ordering::SeqCst};
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/", listener.local_addr().unwrap());
        let committed = std::sync::Arc::new(AtomicBool::new(false));
        let (body_done, body_rx) = tokio::sync::oneshot::channel::<()>();
        let c = committed.clone();
        tokio::spawn(async move {
            let (mut sock, _) = listener.accept().await.unwrap();
            let (mut got, mut buf) = (Vec::new(), [0u8; 65536]);
            while !got.ends_with(b"\r\n0\r\n\r\n") {
                let n = sock.read(&mut buf).await.unwrap();
                if n == 0 {
                    return;
                }
                got.extend_from_slice(&buf[..n]);
            }
            let _ = body_done.send(());
            // The server takes its time to write the part, then commits.
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
            c.store(true, SeqCst);
            let _ = sock.write_all(b"HTTP/1.1 200 OK\r\ncontent-length: 0\r\n\r\n").await;
        });
        let client = reqwest::Client::new();
        let reported = AtomicU64::new(0);
        let progress = |n: u64| reported.store(n, SeqCst);
        let mut w = Window::start(|b| client.post(&url).body(b), None);
        w.buf.extend_from_slice(b"rows");
        w.rows = 3;
        tokio::select! {
            _ = w.commit(Some((&progress, 10))) => panic!("the server answers after the body arrived"),
            _ = body_rx => {}
        }
        // The commit's future is dropped (a cancel) right after the body ended.
        assert!(committed.load(SeqCst), "the window committed after the load returned");
        assert_eq!(reported.load(SeqCst), 13);
    }

    /// Impossible dates and times are refused, never rolled over into
    /// another instant (Feb 31 isn't Mar 2).
    #[test]
    fn impossible_date_times_are_refused() {
        for bad in [
            "2024-02-31 00:00:00+00:00",
            "2023-02-29",
            "2024-04-31",
            "2024-01-01 99:99:99+00:00",
            "2024-01-01 24:00:00",
            "2024-01-01 23:60:00",
            "2024-01-01 23:59:60",
            "2024-01-01 +1:00:00",
            "2024-01-01 00:00:00+25:00",
            "2024-01-01 00:00:00+01:60",
            "2024-01-01 00:00:00.1234567891",
            "2024-01-01 00:00:00+0€",
        ] {
            assert!(parse_datetime(bad).is_none(), "{bad}");
            let mut out = Vec::new();
            assert!(encode(&Ty::DateTime { tz: Some("UTC".into()) }, Sc::S(Cow::Borrowed(bad)), &mut out).is_err(), "{bad}");
            let zoned = Ty::Array(Box::new(Ty::Nullable(Box::new(Ty::DtText(Box::new(Ty::DateTime { tz: Some("Asia/Tokyo".into()) }))))));
            let cell = format!("[\"{bad}\", null]");
            assert!(encode(&zoned, Sc::S(Cow::Borrowed(&cell)), &mut Vec::new()).is_err(), "{bad}");
        }
        assert_eq!(parse_datetime("2024-02-29 23:59:59"), Some((days_from_civil(2024, 2, 29) * 86_400 + 86_399, 0, None)));
        assert_eq!(parse_datetime("2000-02-29").map(|p| p.0), Some(days_from_civil(2000, 2, 29) * 86_400));
        assert_eq!(parse_datetime("2024-01-01 00:00:00.123456789000").map(|p| p.1), Some(123_456_789));
    }

    /// A part the column can't hold (a time of day into a Date, fractions
    /// into a DateTime, digits beyond the scale) is refused when it isn't
    /// zero, never dropped; a zero part still loads.
    #[test]
    fn date_time_parts_the_column_cant_hold_are_refused() {
        let enc = |ty: &str, s: &str| encode(&parse_type(ty).unwrap(), Sc::S(Cow::Borrowed(s)), &mut Vec::new());
        for (ty, bad) in [
            ("Date", "2024-05-06 23:00:00"),
            ("Date", "2024-05-06 00:00:00.5"),
            ("Date32", "2024-05-06T00:00:01Z"),
            ("Nullable(Date)", "2024-05-06 00:01"),
            ("DateTime", "2024-01-01 12:00:00.5"),
            ("DateTime('UTC')", "2024-01-01 12:00:00.000000001+02:00"),
            ("DateTime64(3)", "2024-01-01 12:00:00.1234"),
            ("DateTime64(6, 'UTC')", "2024-01-01 12:00:00.1234567"),
            ("DateTime64(0)", "2024-01-01 12:00:00.1"),
            ("Array(Date)", r#"["2024-01-01", "2024-01-02 10:00:00"]"#),
        ] {
            let e = enc(ty, bad).expect_err(&format!("{ty} {bad}"));
            assert!(e.contains("perder"), "{ty} {bad}: {e}");
        }
        // Zoned columns (text checked here, parsed by the server).
        let zoned = Ty::DtText(Box::new(Ty::DateTime64 { scale: 3, tz: Some("Asia/Tokyo".into()) }));
        assert!(encode(&zoned, Sc::S(Cow::Borrowed("2024-01-01 00:00:00.0001")), &mut Vec::new()).is_err());
        assert!(encode(&zoned, Sc::S(Cow::Borrowed("2024-01-01 00:00:00.1230")), &mut Vec::new()).is_ok());
        for (ty, ok) in [
            ("Date", "2024-05-06"),
            ("Date", "2024-05-06 00:00:00.000"),
            ("Date32", "1900-01-01T00:00:00+03:00"),
            ("DateTime", "2024-01-01 12:00:00.000"),
            ("DateTime64(3)", "2024-01-01 12:00:00.123000000"),
            ("DateTime64(9)", "2024-01-01 12:00:00.123456789"),
            ("DateTime64(0)", "2024-01-01 12:00:00"),
        ] {
            enc(ty, ok).unwrap_or_else(|e| panic!("{ty} {ok}: {e}"));
        }
    }

    /// A tuple element's quoted name with a `(` doesn't hide its type.
    #[test]
    fn quoted_names_with_parentheses() {
        assert!(variable_type("Tuple(`a(` Variant(String, UInt64))"));
        assert!(variable_type("Tuple(`a(` Dynamic)"));
        assert!(variable_type("Array(Tuple(`x)(` Nullable(String), `b(` Variant(String, UInt64)))"));
        assert!(!variable_type("Tuple(`a(` String, `variant` UInt8)"));
        assert_eq!(parse_type("Tuple(`a(` UInt8)"), Some(Ty::Tuple(vec![(Some("a(".into()), Ty::Int { bytes: 1, signed: false })])));
    }

    #[test]
    fn exception_marker_is_found() {
        let body = b"\x01\x02\r\n__exception__\r\nabcdef\r\nCode: 395. DB::Exception: boom\r\n30 abcdef\r\n__exception__\r\n";
        let marker = [MARK, b"abcdef"].concat();
        assert_eq!(find(body, &marker), Some(4));
        assert!(find(b"\x01\x02", &marker).is_none());
    }
}
