//! Bulk transfer (see `dbine_driver::transfer`) for the three InfluxDB
//! APIs. The sessions (`v1`, `v2`, `v3`) build the requests; this module
//! streams the answers into batches and writes line protocol.
//!
//! Reading, with the browse's shape (time, tags, fields) and the columns'
//! types from the catalog; one streamed request, never the whole answer in
//! memory:
//! - 1.x: `SELECT *` with `chunked=true` and `epoch=ns`, JSON chunks. 1.x
//!   sizes chunks by points only, so a chunk (one JSON line) can be any
//!   size: its points are cut out of it as they arrive
//!   ([`InfluxQlStream`]). A filter is an InfluxQL `WHERE`.
//! - 2.x: a Flux `from |> range |> filter |> pivot` over all of time
//!   ([`FLUX_ALL_TIME`]) in annotated CSV, parsed as it arrives
//!   ([`flux_read_script`]); a filter is a Flux predicate on `r` applied
//!   after the pivot (`r.host == "a"`).
//! - 3.x: SQL `SELECT` answered as JSON lines; a filter is a SQL `WHERE`.
//!
//! JSON is parsed here ([`parse_json`]) so every double comes back exactly
//! as the server wrote it. Times become `DateTimeTz` in UTC with their
//! nanoseconds.
//!
//! Loading: line protocol in requests of at most [`POINTS`] points
//! ([`POINTS_V3`] on 3.x, which answers each request after its WAL flush)
//! and [`REQUEST_BYTES`] bytes (or the load's commit window, if smaller),
//! up to [`IN_FLIGHT`] at once, with nanosecond timestamps (1.x `/write`,
//! 2.x `/api/v2/write`, 3.x `/api/v3/write_lp` without partial writes). A
//! load never returns (failed, cancelled or dropped) while a request is
//! still out: the server commits what it received whole, so it waits for
//! the answers. The requests go uncompressed: the HTTP client only
//! decompresses answers. InfluxDB has no insert script, so a row becomes a
//! point like this:
//! - the `time` (or `_time`) column is the timestamp (a date-time, text in
//!   ISO 8601, or an integer in nanoseconds); a row without it fails;
//! - tags are the columns read as tags (`tag`, `Dictionary(…)`) and the
//!   tags the target measurement already has; empty tags are left out;
//! - every other column is a field, typed like the target's field when it
//!   exists (an integer into a `float` field goes as a float…), otherwise
//!   by its value: integers `i` (`u` on 2.x and 3.x for unsigned), floats,
//!   booleans, everything else (exact decimals too) as strings;
//! - null fields are left out. What InfluxDB can't store faithfully fails
//!   the load with `Unsupported` instead of being dropped: a point whose
//!   fields are all null, NaN or infinite floats, an unsigned integer past
//!   `i64` on 1.x, names or tag values line protocol can't carry (a line
//!   break, a trailing backslash, on 1.x and 2.x a backslash before `,`,
//!   `=`, a space or `"`), a measurement starting with `#`, and on 2.x the
//!   names Flux reserves.

use crate::Api;
use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, TransferColumn};
use dbine_driver::{ColumnInfo, Error, Result};
use serde_json::{Map, Value};
use std::fmt::Write as _;
use std::sync::Arc;
use tokio::task::JoinSet;

/// Points per chunk of a 1.x chunked read (its default).
pub(crate) const CHUNK_SIZE: usize = 10_000;
/// Bytes of 3.x JSON lines handed to the sink at once.
pub(crate) const CHUNK_BYTES: usize = 2 * 1024 * 1024;
/// Most bytes of a 1.x answer line held at once, once its whole points are
/// cut out (see [`InfluxQlStream`]): its head and the point arriving.
pub(crate) const LINE_BYTES: usize = 16 * 1024 * 1024;
/// Points per write request (1.x, 2.x).
pub(crate) const POINTS: usize = 5_000;
/// Points per write request on 3.x, which answers each one only after its
/// WAL flush (every second by default): fewer, larger requests.
pub(crate) const POINTS_V3: usize = 50_000;
/// Bytes per write request: under every version's default limit (3.x:
/// 10 MiB, 1.x: 25 MB) and, with [`IN_FLIGHT`] of them plus the one being
/// built, within the ~32 MiB a table may hold.
pub(crate) const REQUEST_BYTES: usize = 4 * 1024 * 1024;
/// Write requests in flight at once.
pub(crate) const IN_FLIGHT: usize = 4;

/// All of time as a Flux range: InfluxDB's timestamps go from
/// 1677-09-21 to 2262-04-11, and `stop` is exclusive (the default stop is
/// `now()`, which leaves out future points).
pub(crate) const FLUX_ALL_TIME: &str = "start: time(v: -9223372036854775806), stop: time(v: 9223372036854775807)";

/// Prefix of the 2.x read's columns that carry a string value Flux's CSV
/// can't (see [`flux_read_script`]).
const MARK: &str = "\u{1}_";

/// A 2.x tag value with a `\r`: Flux's CSV drops it and a tag can't be
/// marked like a field (see [`flux_check_script`]).
const CR_TAG: &str = "InfluxDB 2 no puede devolver sin pérdida un tag con retorno de carro (\\r): su CSV lo descarta.";

fn is_time(name: &str) -> bool {
    name.eq_ignore_ascii_case("time") || name == "_time"
}

fn is_tag_type(t: &str) -> bool {
    t == "tag" || t.starts_with("Dictionary")
}

/// The columns of a read: the asked-for ones, in their order, or all of the
/// catalog's. A column the measurement doesn't have is an error.
pub(crate) fn read_columns(catalog: &[ColumnInfo], asked: &Option<Vec<String>>) -> Result<Vec<TransferColumn>> {
    let col = |c: &ColumnInfo| TransferColumn { name: c.name.clone(), type_name: c.data_type.clone(), nullable: c.nullable };
    match asked {
        None => Ok(catalog.iter().map(col).collect()),
        Some(names) => names
            .iter()
            .map(|n| {
                catalog
                    .iter()
                    .find(|c| &c.name == n)
                    .map(col)
                    .ok_or_else(|| Error::Query(format!("La medida no tiene la columna «{n}».")))
            })
            .collect(),
    }
}

// ---------------------------------------------------------------- JSON

/// JSON with every double exactly as written: serde_json without its
/// `float_roundtrip` feature can read some 1 ULP off. Strings are decoded
/// by serde_json.
pub(crate) fn parse_json(s: &[u8]) -> Result<Value> {
    let mut p = Json { s, i: 0 };
    let v = p.value(0)?;
    p.ws();
    if p.i != s.len() {
        return Err(p.err());
    }
    Ok(v)
}

struct Json<'a> {
    s: &'a [u8],
    i: usize,
}

impl Json<'_> {
    fn err(&self) -> Error {
        Error::Query(format!("InfluxDB respondió un JSON inválido (en el byte {}).", self.i))
    }

    fn ws(&mut self) {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\n' | b'\r')) {
            self.i += 1;
        }
    }

    fn eat(&mut self, b: u8) -> bool {
        self.ws();
        let hit = self.s.get(self.i) == Some(&b);
        if hit {
            self.i += 1;
        }
        hit
    }

    fn value(&mut self, depth: usize) -> Result<Value> {
        if depth > 64 {
            return Err(self.err());
        }
        self.ws();
        match self.s.get(self.i) {
            Some(b'{') => {
                self.i += 1;
                let mut m = Map::new();
                if self.eat(b'}') {
                    return Ok(Value::Object(m));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return Err(self.err());
                    }
                    let v = self.value(depth + 1)?;
                    m.insert(k, v);
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b'}') {
                        return Ok(Value::Object(m));
                    }
                    return Err(self.err());
                }
            }
            Some(b'[') => {
                self.i += 1;
                let mut a = Vec::new();
                if self.eat(b']') {
                    return Ok(Value::Array(a));
                }
                loop {
                    a.push(self.value(depth + 1)?);
                    if self.eat(b',') {
                        continue;
                    }
                    if self.eat(b']') {
                        return Ok(Value::Array(a));
                    }
                    return Err(self.err());
                }
            }
            Some(b'"') => Ok(Value::String(self.string()?)),
            Some(b't') => self.literal("true", Value::Bool(true)),
            Some(b'f') => self.literal("false", Value::Bool(false)),
            Some(b'n') => self.literal("null", Value::Null),
            Some(b'-' | b'0'..=b'9') => self.number(),
            _ => Err(self.err()),
        }
    }

    fn literal(&mut self, word: &str, v: Value) -> Result<Value> {
        if self.s[self.i..].starts_with(word.as_bytes()) {
            self.i += word.len();
            Ok(v)
        } else {
            Err(self.err())
        }
    }

    fn string(&mut self) -> Result<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return Err(self.err());
        }
        let start = self.i;
        let mut j = start + 1;
        let mut escaped = false;
        loop {
            match self.s.get(j) {
                None => return Err(self.err()),
                Some(b'\\') => {
                    escaped = true;
                    j += 2;
                }
                Some(b'"') => break,
                Some(_) => j += 1,
            }
        }
        self.i = j + 1;
        let raw = &self.s[start..=j];
        if escaped {
            serde_json::from_slice(raw).map_err(|_| self.err())
        } else {
            String::from_utf8(raw[1..raw.len() - 1].to_vec()).map_err(|_| self.err())
        }
    }

    fn number(&mut self) -> Result<Value> {
        let start = self.i;
        while matches!(self.s.get(self.i), Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9')) {
            self.i += 1;
        }
        let t = std::str::from_utf8(&self.s[start..self.i]).map_err(|_| self.err())?;
        if !t.contains(['.', 'e', 'E']) {
            // `-0` is a float's negative zero (1.x writes whole floats
            // without a point): as an integer it'd lose its sign.
            if t.len() > 1 && t.starts_with('-') && t[1..].bytes().all(|b| b == b'0') {
                return Ok(serde_json::Number::from_f64(-0.0).map_or(Value::Null, Value::Number));
            }
            if let Ok(i) = t.parse::<i64>() {
                return Ok(Value::from(i));
            }
            if let Ok(u) = t.parse::<u64>() {
                return Ok(Value::from(u));
            }
        }
        // Rust's parse rounds correctly.
        let f: f64 = t.parse().map_err(|_| self.err())?;
        Ok(serde_json::Number::from_f64(f).map_or(Value::Null, Value::Number))
    }
}

// ---------------------------------------------------------------- reading

/// A timestamp as a cell, UTC with its nanoseconds.
fn time_cell(t: DateTime<Utc>) -> Cell {
    Cell::DateTimeTz(t.format("%Y-%m-%d %H:%M:%S%.f+00:00").to_string())
}

fn ns_cell(ns: i64) -> Cell {
    time_cell(DateTime::from_timestamp_nanos(ns))
}

/// An RFC 3339 / ISO text (3.x leaves out the zone: UTC) as a time cell.
fn text_time_cell(s: &str) -> Cell {
    match parse_time(s) {
        Some(ns) => ns_cell(ns),
        None => Cell::Text(s.to_string()),
    }
}

fn number_cell(n: &serde_json::Number) -> Cell {
    if let Some(i) = n.as_i64() {
        Cell::Int(i)
    } else if let Some(u) = n.as_u64() {
        Cell::UInt(u)
    } else {
        Cell::Float(n.as_f64().unwrap_or(f64::NAN))
    }
}

/// A JSON value (1.x, 3.x) as a cell of a column of type `ty`.
pub(crate) fn json_cell(v: &Value, name: &str, ty: &str) -> Cell {
    let ty = ty.to_ascii_lowercase();
    match v {
        Value::Null => Cell::Null,
        Value::Number(n) if is_time(name) && n.is_i64() => ns_cell(n.as_i64().unwrap_or_default()),
        Value::String(s) if is_time(name) || ty.starts_with("timestamp") => text_time_cell(s),
        // 1.x writes whole floats without a point.
        Value::Number(n) if ty == "float" || ty == "float64" => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        Value::Number(n) => number_cell(n),
        Value::Bool(b) => Cell::Bool(*b),
        Value::String(s) => Cell::Text(s.clone()),
        other => Cell::Json(other.to_string()),
    }
}

/// A Flux CSV value of `#datatype` `ty` as a cell; empty is null (empty
/// strings come marked, see [`flux_read_script`]).
pub(crate) fn flux_cell(s: &str, ty: &str) -> Cell {
    if s.is_empty() {
        return Cell::Null;
    }
    let text = || Cell::Text(s.to_string());
    match ty {
        "long" => s.parse().map_or_else(|_| text(), Cell::Int),
        "unsignedLong" => s.parse().map_or_else(|_| text(), Cell::UInt),
        "double" => match s {
            "+Inf" => Cell::Float(f64::INFINITY),
            "-Inf" => Cell::Float(f64::NEG_INFINITY),
            _ => s.parse().map_or_else(|_| text(), Cell::Float),
        },
        "boolean" => Cell::Bool(s == "true"),
        t if t.starts_with("dateTime") => text_time_cell(s),
        _ => text(),
    }
}

/// Hands rows to the sink as they're parsed.
struct Rows<'a> {
    sink: &'a BatchSinkRef,
    builder: BatchBuilder,
}

impl Rows<'_> {
    fn push_all(&mut self, rows: Vec<Vec<Cell>>) -> Result<()> {
        if rows.is_empty() {
            return Ok(());
        }
        let mut s = self.sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
        for r in rows {
            self.builder.push(r, &mut *s)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<u64> {
        let mut s = self.sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
        self.builder.flush(&mut *s)?;
        Ok(self.builder.rows)
    }
}

/// Send a read and fail with the server's error, before streaming.
async fn open(req: reqwest::RequestBuilder) -> Result<reqwest::Response> {
    let resp = req.send().await.map_err(crate::http::send_err)?;
    if resp.status().is_success() {
        return Ok(resp);
    }
    // Consumes the body into the error.
    crate::http::text(resp).await.and_then(|b| Err(Error::Query(b)))
}

/// Each complete line of a streamed answer.
async fn each_line(mut resp: reqwest::Response, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = resp.chunk().await.map_err(crate::http::send_err)? {
        buf.extend_from_slice(&chunk);
        let mut start = 0;
        while let Some(p) = buf[start..].iter().position(|b| *b == b'\n') {
            let line = &buf[start..start + p];
            if !line.iter().all(u8::is_ascii_whitespace) {
                f(line)?;
            }
            start += p + 1;
        }
        buf.drain(..start);
    }
    if !buf.iter().all(u8::is_ascii_whitespace) {
        f(&buf)?;
    }
    Ok(())
}

/// A 1.x answer's error, if it has one.
fn influxql_error(v: &Value) -> Option<Error> {
    let top = v.get("error").and_then(Value::as_str);
    let result = || v.get("results")?.as_array()?.iter().find_map(|r| r.get("error")?.as_str());
    top.or_else(result).map(|e| Error::Query(e.to_string()))
}

/// 1.x: a chunked `/query` (JSON objects, one per line), its points handed
/// out as they arrive (see [`InfluxQlStream`]).
pub(crate) async fn read_influxql(req: reqwest::RequestBuilder, cols: &[TransferColumn], sink: &BatchSinkRef) -> Result<u64> {
    sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(cols)?;
    let mut resp = open(req).await?;
    let mut rows = Rows { sink, builder: BatchBuilder::new() };
    let mut stream = InfluxQlStream::new(cols);
    while let Some(chunk) = resp.chunk().await.map_err(crate::http::send_err)? {
        let mut out = Vec::new();
        stream.feed(&chunk, &mut out)?;
        rows.push_all(out)?;
    }
    let mut out = Vec::new();
    stream.finish(&mut out)?;
    rows.push_all(out)?;
    rows.finish()
}

/// 1.x's chunked answer, read with bounded memory. A chunk is one JSON line
/// of any size (1.x sizes chunks by points, never by bytes), so each whole
/// point of a series' `values` is cut out of the line as soon as it and the
/// separator after it have arrived: what's held is the line's head
/// (`results`, `columns`, `tags`) and the point still arriving, never the
/// chunk. A point needing more than [`LINE_BYTES`] fails the read.
pub(crate) struct InfluxQlStream<'c> {
    cols: &'c [TransferColumn],
    /// The line being received, without the points already handed out.
    buf: Vec<u8>,
}

impl<'c> InfluxQlStream<'c> {
    pub(crate) fn new(cols: &'c [TransferColumn]) -> Self {
        Self { cols, buf: Vec::new() }
    }

    /// The next bytes of the answer; the rows they complete go to `out`.
    pub(crate) fn feed(&mut self, data: &[u8], out: &mut Vec<Vec<Cell>>) -> Result<()> {
        self.buf.extend_from_slice(data);
        let mut start = 0;
        while let Some(p) = self.buf[start..].iter().position(|b| *b == b'\n') {
            influxql_line(&self.buf[start..start + p], self.cols, out)?;
            start += p + 1;
        }
        self.buf.drain(..start);
        let mut walk = Walk { s: &self.buf, i: 0, cols: self.cols, out, cut: Vec::new() };
        // `None`: the line stops (or isn't the expected shape) here; the
        // rest waits for more bytes, or for the whole line.
        let _ = walk.answer();
        let cut = walk.cut;
        for (a, b) in cut.into_iter().rev() {
            self.buf.drain(a..b);
        }
        if self.buf.len() > LINE_BYTES {
            return Err(Error::Unsupported(format!(
                "InfluxDB 1 respondió un punto de más de {} MiB: no se puede leer con memoria acotada.",
                LINE_BYTES / (1024 * 1024)
            )));
        }
        Ok(())
    }

    /// The end of the answer: its last line, if it has no line break.
    pub(crate) fn finish(&mut self, out: &mut Vec<Vec<Cell>>) -> Result<()> {
        influxql_line(&std::mem::take(&mut self.buf), self.cols, out)
    }

    #[cfg(test)]
    pub(crate) fn held(&self) -> usize {
        self.buf.len()
    }
}

/// A whole 1.x answer line's rows (those not already cut out of it).
fn influxql_line(line: &[u8], cols: &[TransferColumn], out: &mut Vec<Vec<Cell>>) -> Result<()> {
    if line.iter().all(u8::is_ascii_whitespace) {
        return Ok(());
    }
    let v = parse_json(line)?;
    if let Some(e) = influxql_error(&v) {
        return Err(e);
    }
    for r in v.get("results").and_then(Value::as_array).into_iter().flatten() {
        for s in r.get("series").and_then(Value::as_array).into_iter().flatten() {
            out.extend(series_rows(s, cols));
        }
    }
    Ok(())
}

/// The end of the JSON value starting at `s[i]`, if it has arrived whole.
/// Only finds the value's extent (strings and nesting); [`parse_json`]
/// validates it.
fn value_end(s: &[u8], i: usize) -> Option<usize> {
    match s.get(i)? {
        b'"' => string_end(s, i),
        b'[' | b'{' => {
            let (mut depth, mut j) = (0usize, i);
            loop {
                match s.get(j)? {
                    b'"' => {
                        j = string_end(s, j)?;
                        continue;
                    }
                    b'[' | b'{' => depth += 1,
                    b']' | b'}' => {
                        depth = depth.checked_sub(1)?;
                        if depth == 0 {
                            return Some(j + 1);
                        }
                    }
                    _ => {}
                }
                j += 1;
            }
        }
        // A scalar ends at a delimiter; at the end of `s` it may go on.
        _ => match s[i..].iter().position(|b| matches!(b, b',' | b']' | b'}' | b' ' | b'\t' | b'\r' | b'\n'))? {
            0 => None,
            n => Some(i + n),
        },
    }
}

fn string_end(s: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    loop {
        match s.get(j)? {
            b'\\' => j += 2,
            b'"' => return Some(j + 1),
            _ => j += 1,
        }
    }
}

/// A walk over a 1.x answer line received in part, handing out the whole
/// points of each series' `values` and noting their bytes (with the comma
/// after them) in `cut`. Every step answers `None` where the line stops.
struct Walk<'a> {
    s: &'a [u8],
    i: usize,
    cols: &'a [TransferColumn],
    out: &'a mut Vec<Vec<Cell>>,
    cut: Vec<(usize, usize)>,
}

impl Walk<'_> {
    fn peek(&mut self) -> Option<u8> {
        while matches!(self.s.get(self.i), Some(b' ' | b'\t' | b'\r' | b'\n')) {
            self.i += 1;
        }
        self.s.get(self.i).copied()
    }

    fn byte(&mut self, b: u8) -> Option<()> {
        (self.peek()? == b).then(|| self.i += 1)
    }

    /// A whole value, parsed.
    fn take(&mut self) -> Option<Value> {
        self.peek()?;
        let end = value_end(self.s, self.i)?;
        let v = parse_json(&self.s[self.i..end]).ok()?;
        self.i = end;
        Some(v)
    }

    fn skip(&mut self) -> Option<()> {
        self.peek()?;
        self.i = value_end(self.s, self.i)?;
        Some(())
    }

    /// An object, calling `f` with each key and the walk at its value.
    fn object(&mut self, mut f: impl FnMut(&mut Self, &str) -> Option<()>) -> Option<()> {
        self.byte(b'{')?;
        if self.peek()? == b'}' {
            self.i += 1;
            return Some(());
        }
        loop {
            let key = self.take()?;
            self.byte(b':')?;
            f(self, key.as_str()?)?;
            match self.peek()? {
                b',' => self.i += 1,
                b'}' => {
                    self.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// An array, calling `f` with the walk at each element.
    fn array(&mut self, mut f: impl FnMut(&mut Self) -> Option<()>) -> Option<()> {
        self.byte(b'[')?;
        if self.peek()? == b']' {
            self.i += 1;
            return Some(());
        }
        loop {
            f(self)?;
            match self.peek()? {
                b',' => self.i += 1,
                b']' => {
                    self.i += 1;
                    return Some(());
                }
                _ => return None,
            }
        }
    }

    /// `{"results": [{"series": [{…}, …]}, …]}`.
    fn answer(&mut self) -> Option<()> {
        self.object(|w, k| match k {
            "results" => w.array(|w| w.object(|w, k| if k == "series" { w.array(Self::series) } else { w.skip() })),
            _ => w.skip(),
        })
    }

    /// A series: `values` comes after `columns` and `tags` (1.x writes
    /// them in that order); if it didn't, nothing is cut from it here.
    fn series(&mut self) -> Option<()> {
        let (mut columns, mut tags) = (None::<Value>, None::<Value>);
        self.object(|w, k| match k {
            "columns" => {
                columns = Some(w.take()?);
                Some(())
            }
            "tags" => {
                tags = Some(w.take()?);
                Some(())
            }
            "values" => match &columns {
                Some(c) => {
                    let shape = Shape::new(Some(c), tags.as_ref(), w.cols);
                    w.points(&shape)
                }
                None => w.skip(),
            },
            _ => w.skip(),
        })
    }

    /// A series' `values`: each point followed by `,` or `]` is handed out
    /// and cut (with its comma), so the array stays valid JSON.
    fn points(&mut self, shape: &Shape<'_>) -> Option<()> {
        self.byte(b'[')?;
        loop {
            if self.peek()? == b']' {
                self.i += 1;
                return Some(());
            }
            let a = self.i;
            let end = value_end(self.s, a)?;
            self.i = end;
            let last = match self.peek()? {
                b',' => false,
                b']' => true,
                _ => return None,
            };
            let point = parse_json(&self.s[a..end]).ok()?;
            self.out.push(shape.row(&point));
            if last {
                self.cut(a, end);
                self.i += 1;
                return Some(());
            }
            self.i += 1;
            self.cut(a, self.i);
        }
    }

    /// Notes bytes to cut, joined to the previous cut when they follow it.
    fn cut(&mut self, a: usize, b: usize) {
        match self.cut.last_mut() {
            Some(last) if last.1 == a => last.1 = b,
            _ => self.cut.push((a, b)),
        }
    }
}

/// Where each column of a 1.x series' points is: a position in its
/// `columns`, or else one of its `tags` (those of a `GROUP BY`).
struct Shape<'a> {
    cols: &'a [TransferColumn],
    at: Vec<Option<usize>>,
    tags: Option<&'a Map<String, Value>>,
}

impl<'a> Shape<'a> {
    fn new(columns: Option<&Value>, tags: Option<&'a Value>, cols: &'a [TransferColumn]) -> Self {
        let names: Vec<&str> = columns.and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).collect();
        let at = cols.iter().map(|c| names.iter().position(|n| *n == c.name)).collect();
        Self { cols, at, tags: tags.and_then(Value::as_object) }
    }

    fn row(&self, point: &Value) -> Vec<Cell> {
        let v = point.as_array().map(Vec::as_slice).unwrap_or(&[]);
        self.cols
            .iter()
            .zip(&self.at)
            .map(|(c, i)| match i {
                Some(i) => v.get(*i).map_or(Cell::Null, |x| json_cell(x, &c.name, &c.type_name)),
                None => self.tags.and_then(|t| t.get(&c.name)).map_or(Cell::Null, |x| json_cell(x, &c.name, "tag")),
            })
            .collect()
    }
}

/// The rows of a 1.x series, in `cols`' order (tags of a `GROUP BY` too).
pub(crate) fn series_rows(series: &Value, cols: &[TransferColumn]) -> Vec<Vec<Cell>> {
    let shape = Shape::new(series.get("columns"), series.get("tags"), cols);
    series.get("values").and_then(Value::as_array).into_iter().flatten().map(|p| shape.row(p)).collect()
}

/// 3.x: `query_sql` answered in JSON lines (nulls left out).
pub(crate) async fn read_jsonl(req: reqwest::RequestBuilder, cols: &[TransferColumn], sink: &BatchSinkRef) -> Result<u64> {
    sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(cols)?;
    let resp = open(req).await?;
    let mut rows = Rows { sink, builder: BatchBuilder::new() };
    let (mut pending, mut bytes): (Vec<Vec<Cell>>, usize) = (Vec::new(), 0);
    each_line(resp, |line| {
        let Value::Object(v) = parse_json(line)? else {
            return Err(Error::Query("InfluxDB 3 respondió una línea JSON que no es un objeto.".into()));
        };
        pending.push(cols.iter().map(|c| v.get(&c.name).map_or(Cell::Null, |x| json_cell(x, &c.name, &c.type_name))).collect());
        bytes += line.len();
        if pending.len() >= 1_000 || bytes >= CHUNK_BYTES {
            rows.push_all(std::mem::take(&mut pending))?;
            bytes = 0;
        }
        Ok(())
    })
    .await?;
    rows.push_all(pending)?;
    rows.finish()
}

/// 2.x: a measurement's points, once per branch of a script (a shared
/// source isn't pushed down to storage, and a regex on `_value` only works
/// pushed down).
fn flux_source(bucket: &str, measurement: &str) -> String {
    let q = crate::v2::flux_str;
    format!("from(bucket: {})\n  |> range({FLUX_ALL_TIME})\n  |> filter(fn: (r) => r._measurement == {})", q(bucket), q(measurement))
}

/// A string value Flux's CSV can't give back: empty (the same as a null
/// there) or with a `\r` (its CSV writer drops it). Pushed down to storage
/// it only matches strings.
const LOSSY: &str = r"r._value =~ /^$|\r/";

/// 2.x: a script whose answer has rows only if the measurement has string
/// values the CSV can't give back (see [`flux_read_script`]); it fails
/// with [`CR_TAG`] if a tag value (of `tags`, the measurement's tag keys)
/// has a `\r`.
pub(crate) fn flux_check_script(bucket: &str, measurement: &str, tags: &[&str]) -> String {
    let src = flux_source(bucket, measurement);
    let mut s = format!("lossy = {src}\n  |> filter(fn: (r) => {LOSSY})\n  |> limit(n: 1)\n  |> keep(columns: [\"_field\"])\n");
    if tags.is_empty() {
        s.push_str("lossy");
        return s;
    }
    let q = crate::v2::flux_str;
    let cond: Vec<String> = tags.iter().map(|t| format!("r[{}] =~ /\\r/", q(t))).collect();
    let _ = write!(
        s,
        "crtags = {src}\n  |> filter(fn: (r) => {})\n  |> limit(n: 1)\n  |> map(fn: (r) => ({{r with _value: die(msg: {})}}))\n  \
         |> keep(columns: [\"_field\"])\nunion(tables: [lossy, crtags])",
        cond.join(" or "),
        q(CR_TAG)
    );
    s
}

/// 2.x: the Flux script of a read. With `lossy` strings (see
/// [`flux_check_script`]) those values also come apart, as a string column
/// named [`MARK`] + the field with the value escaped (`\\`, `\r`) and a
/// `;` after it: the pivot names columns by `_dbine_m` (`\u{1}` for them,
/// empty for the rest) and `_field`. Only `_dbine_m` changes, never `_field`
/// (a group key column): the pivot panics on that.
pub(crate) fn flux_read_script(bucket: &str, measurement: &str, lossy: bool, filter: Option<&str>) -> String {
    let src = flux_source(bucket, measurement);
    let mut s = if lossy {
        format!(
            "import \"strings\"\n\
             plain = {src}\n  |> set(key: \"_dbine_m\", value: \"\")\n\
             marked = {src}\n  |> filter(fn: (r) => {LOSSY})\n  \
             |> map(fn: (r) => ({{r with _value: strings.replaceAll(v: strings.replaceAll(v: string(v: r._value), t: \"\\\\\", u: \"\\\\\\\\\"), t: \"\\r\", u: \"\\\\r\") + \";\"}}))\n  \
             |> set(key: \"_dbine_m\", value: \"\\x01\")\n\
             union(tables: [plain, marked])\n  |> pivot(rowKey: [\"_time\"], columnKey: [\"_dbine_m\", \"_field\"], valueColumn: \"_value\")"
        )
    } else {
        format!("{src}\n  |> pivot(rowKey: [\"_time\"], columnKey: [\"_field\"], valueColumn: \"_value\")")
    };
    if let Some(f) = filter {
        let _ = write!(s, "\n  |> filter(fn: (r) => {f})");
    }
    s
}

/// A Flux error that's [`CR_TAG`] as `Unsupported`.
pub(crate) fn flux_err(e: Error) -> Error {
    match e {
        Error::Query(m) if m.contains(CR_TAG) => Error::Unsupported(CR_TAG.into()),
        e => e,
    }
}

/// CSV records as they arrive, split across chunks at any byte.
#[derive(Default)]
pub(crate) struct CsvStream {
    fields: Vec<String>,
    cur: Vec<u8>,
    quoted: bool,
    /// A quote inside a quoted field: `""` or the field's end.
    quote_pending: bool,
    content: bool,
}

impl CsvStream {
    /// Records completed by `data`; `None` for a blank line.
    pub(crate) fn feed(&mut self, data: &[u8], out: &mut Vec<Option<Vec<String>>>) {
        for &b in data {
            if self.quote_pending {
                self.quote_pending = false;
                if b == b'"' {
                    self.cur.push(b'"');
                    continue;
                }
                self.quoted = false;
            }
            if self.quoted {
                match b {
                    b'"' => self.quote_pending = true,
                    // Flux's CSV writer (CRLF) writes a value's `\n` as
                    // `\r\n` and drops its `\r`: a `\r` here is never data.
                    b'\r' => {}
                    _ => self.cur.push(b),
                }
                continue;
            }
            match b {
                b'"' => {
                    self.quoted = true;
                    self.content = true;
                }
                b',' => {
                    self.end_field();
                    self.content = true;
                }
                b'\r' => {}
                b'\n' => self.end_record(out),
                _ => {
                    self.cur.push(b);
                    self.content = true;
                }
            }
        }
    }

    pub(crate) fn finish(&mut self, out: &mut Vec<Option<Vec<String>>>) {
        self.quoted = false;
        self.quote_pending = false;
        if self.content || !self.cur.is_empty() {
            self.end_record(out);
        }
    }

    fn end_field(&mut self) {
        let f = String::from_utf8(std::mem::take(&mut self.cur)).unwrap_or_else(|e| String::from_utf8_lossy(e.as_bytes()).into_owned());
        self.fields.push(f);
    }

    fn end_record(&mut self, out: &mut Vec<Option<Vec<String>>>) {
        if self.content || !self.cur.is_empty() {
            self.end_field();
            out.push(Some(std::mem::take(&mut self.fields)));
        } else {
            out.push(None);
        }
        self.content = false;
    }
}

/// Where a CSV column of a Flux table goes.
#[derive(Debug, Clone, Copy)]
enum Slot {
    Skip,
    Col(usize),
    /// A marked string (see [`flux_read_script`]) for that read column.
    Mark(usize),
}

/// Flux annotated CSV tables into rows of `cols`.
#[derive(Default)]
pub(crate) struct FluxTables {
    types: Vec<String>,
    /// CSV column → read column, once the table's header is in.
    map: Option<Vec<Slot>>,
    /// The table is Flux's error table (`,error,reference`).
    error: bool,
}

impl FluxTables {
    pub(crate) fn record(&mut self, rec: Option<Vec<String>>, cols: &[TransferColumn], out: &mut Vec<Vec<Cell>>) -> Result<()> {
        let Some(r) = rec else {
            self.map = None;
            return Ok(());
        };
        if r.first().is_some_and(|f| f.starts_with('#')) {
            if r[0] == "#datatype" {
                self.types = r;
            }
            self.map = None;
            return Ok(());
        }
        let Some(map) = &self.map else {
            // Exactly Flux's error table: a measurement may well have tags
            // named `error` and `reference`.
            self.error = r.len() == 3 && r[1] == "error" && r[2] == "reference";
            // The annotation column, then Flux's own `result` and `table`
            // (fields or tags with those names come later).
            let own = if r.len() >= 3 && r[1] == "result" && r[2] == "table" { 3 } else { 1 };
            let at = |n: &str| cols.iter().position(|c| c.name == n);
            self.map = Some(
                r.iter()
                    .enumerate()
                    .map(|(i, n)| match n.strip_prefix(MARK) {
                        _ if i < own => Slot::Skip,
                        Some(f) => at(f).map_or(Slot::Skip, Slot::Mark),
                        None => at(n).map_or(Slot::Skip, Slot::Col),
                    })
                    .collect(),
            );
            return Ok(());
        };
        if self.error {
            return Err(flux_err(Error::Query(r.get(1).cloned().unwrap_or_default())));
        }
        let mut row = vec![Cell::Null; cols.len()];
        let mut marks = Vec::new();
        for (i, (v, slot)) in r.iter().zip(map).enumerate() {
            match slot {
                Slot::Col(j) => row[*j] = flux_cell(v, self.types.get(i).map_or("string", String::as_str)),
                Slot::Mark(j) if !v.is_empty() => marks.push((*j, v)),
                _ => {}
            }
        }
        for (j, v) in marks {
            row[j] = Cell::Text(unmark(v)?);
        }
        out.push(row);
        Ok(())
    }
}

/// A marked string's value (see [`flux_read_script`]).
fn unmark(v: &str) -> Result<String> {
    let bad = || Error::Query(format!("valor marcado inválido en la respuesta de Flux: {v:?}"));
    let v = v.strip_suffix(';').ok_or_else(bad)?;
    let mut out = String::with_capacity(v.len());
    let mut it = v.chars();
    while let Some(c) = it.next() {
        if c != '\\' {
            out.push(c);
            continue;
        }
        match it.next() {
            Some('\\') => out.push('\\'),
            Some('r') => out.push('\r'),
            _ => return Err(bad()),
        }
    }
    Ok(out)
}

/// 2.x: a Flux query ([`flux_read_script`]) answered in annotated CSV.
pub(crate) async fn read_flux(req: reqwest::RequestBuilder, cols: &[TransferColumn], sink: &BatchSinkRef) -> Result<u64> {
    sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(cols)?;
    let mut resp = open(req).await.map_err(flux_err)?;
    let mut rows = Rows { sink, builder: BatchBuilder::new() };
    let (mut csv, mut tables) = (CsvStream::default(), FluxTables::default());
    let mut recs = Vec::new();
    let mut out = Vec::new();
    loop {
        let chunk = resp.chunk().await.map_err(crate::http::send_err)?;
        match &chunk {
            Some(c) => csv.feed(c, &mut recs),
            None => csv.finish(&mut recs),
        }
        for r in recs.drain(..) {
            tables.record(r, cols, &mut out)?;
        }
        rows.push_all(std::mem::take(&mut out))?;
        if chunk.is_none() {
            break;
        }
    }
    rows.finish()
}

// ---------------------------------------------------------------- loading

/// Where points are written.
pub(crate) struct Endpoint {
    pub http: reqwest::Client,
    /// The write URL with its query string.
    pub url: String,
    pub auth: Auth,
    pub api: Api,
}

#[derive(Clone)]
pub(crate) enum Auth {
    None,
    Basic(String, String),
    /// `Authorization: Token …` (2.x).
    Token(String),
    Bearer(String),
}

impl Auth {
    fn apply(&self, req: reqwest::RequestBuilder) -> reqwest::RequestBuilder {
        match self {
            Auth::None => req,
            Auth::Basic(u, p) => req.basic_auth(u, Some(p)),
            Auth::Token(t) => req.header("Authorization", format!("Token {t}")),
            Auth::Bearer(t) => req.bearer_auth(t),
        }
    }
}

/// How a field is written.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Kind {
    /// No field of that name yet: by the value.
    Any,
    Float,
    Int,
    UInt,
    Str,
    Bool,
}

fn kind_of(data_type: &str) -> Kind {
    match data_type.to_ascii_lowercase().as_str() {
        "float" | "float64" => Kind::Float,
        "integer" | "int64" => Kind::Int,
        "unsigned" | "uint64" => Kind::UInt,
        "string" | "utf8" => Kind::Str,
        "boolean" => Kind::Bool,
        _ => Kind::Any,
    }
}

/// 2.x: names a tag can't have (the server refuses `_field` and
/// `_measurement`; the others would clash with Flux's columns on reading).
const V2_TAGS: [&str; 6] = ["_measurement", "_field", "_value", "_start", "_stop", "_time"];
/// 2.x: names a field can't have: the pivot of a read fails on them.
const V2_FIELDS: [&str; 4] = ["_measurement", "_start", "_stop", "_time"];

fn unsupported(what: String, reason: &str) -> Error {
    Error::Unsupported(format!("No se puede escribir en InfluxDB {what}: {reason}."))
}

/// Which column is what, from the load's columns and the target's catalog.
pub(crate) struct Plan {
    measurement: String,
    time: usize,
    /// Column, escaped key, the key as given.
    tags: Vec<(usize, String, String)>,
    fields: Vec<(usize, String, String, Kind)>,
    api: Api,
}

impl Plan {
    pub(crate) fn new(measurement: &str, names: &[String], read: &[TransferColumn], target: &[ColumnInfo], api: Api) -> Result<Plan> {
        let time = names
            .iter()
            .position(|n| is_time(n))
            .ok_or_else(|| Error::Query("Para escribir puntos en InfluxDB hace falta una columna time (la marca de tiempo).".into()))?;
        if measurement.starts_with('#') {
            return Err(unsupported(format!("la medida «{measurement}»"), "una línea que empieza con # es un comentario"));
        }
        let m = escape(measurement, &[',', ' '], api).map_err(|r| unsupported(format!("la medida «{measurement}»"), r))?;
        let read_type = |n: &str| read.iter().find(|c| c.name == n).map_or("", |c| c.type_name.as_str());
        let (mut tags, mut fields) = (Vec::new(), Vec::new());
        for (i, n) in names.iter().enumerate() {
            if i == time {
                continue;
            }
            let existing = target.iter().find(|c| &c.name == n);
            let tag = match existing {
                Some(c) => c.primary_key || is_tag_type(&c.data_type),
                None => is_tag_type(read_type(n)),
            };
            let what = || format!("la columna «{n}»");
            let key = escape(n, &[',', '=', ' '], api).map_err(|r| unsupported(what(), r))?;
            if matches!(api, Api::Flux) && (if tag { &V2_TAGS[..] } else { &V2_FIELDS[..] }).contains(&n.as_str()) {
                let kind = if tag { "tag" } else { "campo" };
                return Err(unsupported(what(), &format!("InfluxDB 2 reserva ese nombre y no lo admite como {kind}")));
            }
            if tag {
                tags.push((i, key, n.clone()));
            } else {
                fields.push((i, key, n.clone(), existing.map_or(Kind::Any, |c| kind_of(&c.data_type))));
            }
        }
        // Line protocol wants tags sorted by key for the fastest writes.
        tags.sort_by(|a, b| a.1.cmp(&b.1));
        Ok(Plan { measurement: m, time, tags, fields, api })
    }

    /// One row as a line (with its `\n`). A row InfluxDB can't store as it
    /// is fails, and leaves `out` as it was.
    pub(crate) fn line(&self, row: &[Cell], out: &mut String) -> Result<()> {
        let mark = out.len();
        let r = self.write_line(row, out);
        if r.is_err() {
            out.truncate(mark);
        }
        r
    }

    fn write_line(&self, row: &[Cell], out: &mut String) -> Result<()> {
        let ns = time_ns(&row[self.time]).ok_or_else(|| {
            Error::Query(format!("La marca de tiempo {:?} no es una fecha y hora ni un entero en nanosegundos.", row[self.time]))
        })?;
        out.push_str(&self.measurement);
        for (i, k, name) in &self.tags {
            let text = match &row[*i] {
                Cell::Null => continue,
                Cell::Text(s) => s.clone(),
                c => plain_text(c),
            };
            if text.is_empty() {
                continue;
            }
            let v = escape(&text, &[',', '=', ' '], self.api)
                .map_err(|r| unsupported(format!("el valor {text:?} del tag «{name}»"), r))?;
            out.push(',');
            out.push_str(k);
            out.push('=');
            out.push_str(&v);
        }
        let mut first = true;
        for (i, k, name, kind) in &self.fields {
            let c = &row[*i];
            match c {
                Cell::Null => continue,
                Cell::Float(f) if !f.is_finite() => {
                    return Err(unsupported(format!("el valor {f} de «{name}»"), "no admite NaN ni infinitos"));
                }
                _ => {}
            }
            out.push(if first { ' ' } else { ',' });
            first = false;
            out.push_str(k);
            out.push('=');
            field_value(c, *kind, self.api, out).map_err(|r| unsupported(format!("el valor de «{name}»"), r))?;
        }
        if first {
            return Err(unsupported(
                format!("el punto de {}", plain_text(&row[self.time])),
                "todos sus campos son nulos, y un punto sin campos no se guarda",
            ));
        }
        let _ = writeln!(out, " {ns}");
        Ok(())
    }
}

/// A cell as plain text (tag values, strings).
fn plain_text(c: &Cell) -> String {
    match c.to_json() {
        Value::String(s) => s,
        Value::Null => String::new(),
        v => v.to_string(),
    }
}

fn field_value(c: &Cell, kind: Kind, api: Api, out: &mut String) -> std::result::Result<(), &'static str> {
    // Plain digits, never an exponent; without a point it's still a float.
    let float = |f: f64, out: &mut String| {
        let _ = write!(out, "{f}");
    };
    let string = |s: &str, out: &mut String| {
        out.push('"');
        for ch in s.chars() {
            if ch == '"' || ch == '\\' {
                out.push('\\');
            }
            out.push(ch);
        }
        out.push('"');
    };
    let int = |i: i64, out: &mut String| {
        let _ = write!(out, "{i}i");
    };
    let uint = |u: u64, out: &mut String| {
        if !matches!(api, Api::InfluxQl) {
            let _ = write!(out, "{u}u");
        } else if let Ok(i) = i64::try_from(u) {
            let _ = write!(out, "{i}i");
        } else {
            return Err("InfluxDB 1 no tiene enteros sin signo y no entra en uno con signo");
        }
        Ok(())
    };
    let text_of = |c: &Cell| match c {
        Cell::Decimal(s) | Cell::Text(s) => Some(s.trim().to_string()),
        _ => None,
    };
    match (kind, c) {
        (Kind::Str, c) => string(&plain_text(c), out),
        (Kind::Float, Cell::Int(i)) => float(*i as f64, out),
        (Kind::Float, Cell::UInt(u)) => float(*u as f64, out),
        (Kind::Float, Cell::Decimal(_) | Cell::Text(_)) => match text_of(c).and_then(|s| s.parse::<f64>().ok()) {
            Some(f) => float(f, out),
            None => string(&plain_text(c), out),
        },
        (Kind::Int, Cell::Float(f)) if f.fract() == 0.0 && f.abs() < 9.2e18 => int(*f as i64, out),
        (Kind::Int, Cell::UInt(u)) if i64::try_from(*u).is_ok() => int(*u as i64, out),
        (Kind::Int, Cell::Decimal(_) | Cell::Text(_)) => match text_of(c).and_then(|s| s.parse::<i64>().ok()) {
            Some(i) => int(i, out),
            None => string(&plain_text(c), out),
        },
        (Kind::UInt, Cell::Int(i)) if *i >= 0 => uint(*i as u64, out)?,
        (Kind::UInt, Cell::Decimal(_) | Cell::Text(_)) => match text_of(c).and_then(|s| s.parse::<u64>().ok()) {
            Some(u) => uint(u, out)?,
            None => string(&plain_text(c), out),
        },
        (Kind::Bool, Cell::Text(s)) if s == "true" || s == "false" => out.push_str(s),
        (_, Cell::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
        (_, Cell::Int(i)) => int(*i, out),
        (_, Cell::UInt(u)) => uint(*u, out)?,
        (_, Cell::Float(f)) => float(*f, out),
        // An exact decimal into a new field stays exact: a string.
        (_, c) => string(&plain_text(c), out),
    }
    Ok(())
}

/// A measurement (`specials`: comma and space) or a tag key, tag value or
/// field key (comma, equals sign and space) in line protocol. 3.x reads
/// `\\` as one backslash; 1.x and 2.x keep every backslash except one
/// before a special (or `"`, in field keys), which they drop. None of them
/// takes a line break or a trailing backslash; what can't be written is
/// the reason why.
fn escape(s: &str, specials: &[char], api: Api) -> std::result::Result<String, &'static str> {
    if s.contains(['\n', '\r']) {
        return Err("el line protocol no admite saltos de línea en nombres ni en valores de tags");
    }
    if s.ends_with('\\') {
        return Err("el line protocol no admite una barra invertida al final de un nombre o valor de tag");
    }
    let v3 = matches!(api, Api::Sql);
    let mut out = String::with_capacity(s.len());
    let mut it = s.chars().peekable();
    while let Some(ch) = it.next() {
        if ch == '\\' && !v3 && it.peek().is_some_and(|n| matches!(n, '"' | ',' | '=' | ' ')) {
            return Err("InfluxDB 1 y 2 no conservan una barra invertida antes de «,», «=», « » o «\"»");
        }
        if specials.contains(&ch) || (v3 && ch == '\\') {
            out.push('\\');
        }
        out.push(ch);
    }
    Ok(out)
}

/// A date-time text (with or without a zone; UTC without) in nanoseconds.
pub(crate) fn parse_time(s: &str) -> Option<i64> {
    let s = s.trim();
    let t = s.replacen(' ', "T", 1);
    if let Ok(d) = DateTime::parse_from_rfc3339(&t) {
        return d.timestamp_nanos_opt();
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f%:z", "%Y-%m-%dT%H:%M:%S%.f%z", "%Y-%m-%dT%H:%M:%S%.f%#z"] {
        if let Ok(d) = DateTime::parse_from_str(&t, f) {
            return d.timestamp_nanos_opt();
        }
    }
    if let Ok(d) = NaiveDateTime::parse_from_str(t.trim_end_matches('Z'), "%Y-%m-%dT%H:%M:%S%.f") {
        return d.and_utc().timestamp_nanos_opt();
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok()?.and_hms_opt(0, 0, 0)?.and_utc().timestamp_nanos_opt()
}

fn time_ns(c: &Cell) -> Option<i64> {
    match c {
        Cell::Int(i) => Some(*i),
        Cell::UInt(u) => i64::try_from(*u).ok(),
        Cell::DateTimeTz(s) | Cell::DateTime(s) | Cell::Date(s) | Cell::Text(s) => parse_time(s),
        _ => None,
    }
}

async fn write(ep: &Endpoint, body: String, points: u64) -> Result<u64> {
    let req = ep.auth.apply(ep.http.post(&ep.url)).header("Content-Type", "text/plain; charset=utf-8").body(body);
    let resp = req.send().await.map_err(crate::http::send_err)?;
    crate::http::text(resp).await?;
    Ok(points)
}

/// Committed points, reported every `every`.
struct Done<'a> {
    points: u64,
    reported: u64,
    every: u64,
    progress: Progress<'a>,
}

impl Done<'_> {
    fn add(&mut self, n: u64) {
        self.points += n;
        if self.points - self.reported >= self.every {
            self.reported = self.points;
            (self.progress)(self.points);
        }
    }

    fn finish(&mut self) {
        if self.points > self.reported {
            self.reported = self.points;
            (self.progress)(self.points);
        }
    }
}

/// The write requests out, each answering its points.
#[derive(Default)]
struct InFlight(JoinSet<Result<u64>>);

impl InFlight {
    /// Send a request, first waiting for one if [`IN_FLIGHT`] are out.
    async fn send(&mut self, ep: &Arc<Endpoint>, body: String, points: usize, done: &mut Done<'_>) -> Result<()> {
        if self.0.len() >= IN_FLIGHT {
            done.add(joined(self.0.join_next().await)?);
        }
        let ep = ep.clone();
        self.0.spawn(async move { write(&ep, body, points as u64).await });
        Ok(())
    }

    /// The requests already answered.
    fn reap(&mut self, done: &mut Done<'_>) -> Result<()> {
        while let Some(r) = self.0.try_join_next() {
            done.add(joined(Some(r))?);
        }
        Ok(())
    }

    /// Every answer (the first error, if any).
    async fn settle(&mut self, done: &mut Done<'_>) -> Result<()> {
        let mut first = Ok(());
        while let Some(r) = self.0.join_next().await {
            match joined(Some(r)) {
                Ok(n) => done.add(n),
                Err(e) if first.is_ok() => first = Err(e),
                Err(_) => {}
            }
        }
        first
    }
}

impl Drop for InFlight {
    /// The load was dropped (cancelled) with requests out. The server
    /// commits a request it has whole whatever the client does, so their
    /// answers are awaited here: no point lands after the load is gone.
    fn drop(&mut self) {
        if self.0.is_empty() {
            return;
        }
        let set = &mut self.0;
        match tokio::runtime::Handle::try_current() {
            Ok(h) if matches!(h.runtime_flavor(), tokio::runtime::RuntimeFlavor::MultiThread) => {
                tokio::task::block_in_place(|| h.block_on(async { while set.join_next().await.is_some() {} }));
            }
            _ => {
                tracing::warn!("influxdb: bulk load dropped outside a multi-thread runtime; {} write requests may still commit", set.len());
                set.detach_all();
            }
        }
    }
}

/// Write `source`'s rows as points; answers the points committed.
pub(crate) async fn load(
    ep: Endpoint,
    measurement: &str,
    target: &[ColumnInfo],
    spec: &LoadSpec,
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<u64> {
    let names: Vec<String> =
        if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
    let plan = Plan::new(measurement, &names, columns, target, ep.api)?;
    let every = spec.commit_rows.max(1);
    let max_points = if matches!(ep.api, Api::Sql) { POINTS_V3 } else { POINTS }.min(usize::try_from(every).unwrap_or(usize::MAX));
    let max_bytes = usize::try_from(spec.commit_bytes.max(1)).unwrap_or(usize::MAX).min(REQUEST_BYTES);
    let ep = Arc::new(ep);
    let mut done = Done { points: 0, reported: 0, every, progress };
    let mut inflight = InFlight::default();
    let fed: Result<()> = async {
        let (mut body, mut points) = (String::new(), 0usize);
        while let Some(batch) = source.next().await {
            for row in batch.rows {
                if row.len() != names.len() {
                    return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                }
                let start = body.len();
                plan.line(&row, &mut body)?;
                points += 1;
                if body.len() > max_bytes && points > 1 {
                    // Past the request's size with this line: it starts the next one.
                    let next = body.split_off(start);
                    inflight.send(&ep, std::mem::replace(&mut body, next), points - 1, &mut done).await?;
                    points = 1;
                }
                if points >= max_points || body.len() >= max_bytes {
                    inflight.send(&ep, std::mem::take(&mut body), points, &mut done).await?;
                    points = 0;
                }
            }
            inflight.reap(&mut done)?;
        }
        if points > 0 {
            inflight.send(&ep, body, points, &mut done).await?;
        }
        Ok(())
    }
    .await;
    // Failed or not, no request is left out when this returns.
    let settled = inflight.settle(&mut done).await;
    fed?;
    settled?;
    done.finish();
    Ok(done.points)
}

fn joined(r: Option<std::result::Result<Result<u64>, tokio::task::JoinError>>) -> Result<u64> {
    match r {
        None => Ok(0),
        Some(Ok(r)) => r,
        Some(Err(e)) => Err(Error::State(format!("escritura interrumpida: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::transfer::RowBatch;
    use serde_json::json;
    use std::sync::Mutex;
    use tokio::io::{AsyncBufReadExt, AsyncReadExt, AsyncWriteExt, BufReader};

    fn tc(name: &str, ty: &str) -> TransferColumn {
        TransferColumn { name: name.into(), type_name: ty.into(), nullable: true }
    }

    fn ci(name: &str, ty: &str, key: bool) -> ColumnInfo {
        ColumnInfo { name: name.into(), data_type: ty.into(), nullable: !key, primary_key: key, auto_increment: false, default_value: None }
    }

    fn names(n: &[&str]) -> Vec<String> {
        n.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn rows_become_lines() {
        let read = [tc("time", "time"), tc("host", "tag"), tc("region", ""), tc("v", "float"), tc("n", ""), tc("s", ""), tc("ok", "")];
        let target = [ci("region", "tag", true), ci("v", "float", false)];
        let cols = names(&["time", "host", "region", "v", "n", "s", "ok"]);
        let plan = Plan::new("cpu load,x", &cols, &read, &target, Api::Flux).unwrap();
        let mut out = String::new();
        let row = vec![
            Cell::DateTimeTz("2024-01-31 13:45:00.000000001+00:00".into()),
            Cell::Text("a b".into()),
            Cell::Text("eu=1".into()),
            Cell::Int(2),
            Cell::UInt(u64::MAX),
            Cell::Text("say \"hi\"".into()),
            Cell::Bool(true),
        ];
        plan.line(&row, &mut out).unwrap();
        assert_eq!(
            out,
            "cpu\\ load\\,x,host=a\\ b,region=eu\\=1 v=2,n=18446744073709551615u,s=\"say \\\"hi\\\"\",ok=true 1706708700000000001\n"
        );
        // 1.x has no unsigned: past i64 fails instead of turning into a float.
        let plan = Plan::new("cpu", &cols, &read, &target, Api::InfluxQl).unwrap();
        assert!(matches!(plan.line(&row, &mut String::new()), Err(Error::Unsupported(_))));
        // Nulls left out; a naive time is UTC; a decimal into a new field
        // stays exact (a string).
        let plan = Plan::new("m", &names(&["_time", "n", "gone", "d", "_measurement"]), &[], &[], Api::Sql).unwrap();
        let mut out = String::new();
        let row = vec![
            Cell::DateTime("2024-01-31 13:45:00".into()),
            Cell::UInt(7),
            Cell::Null,
            Cell::Decimal("12345678901234567890.123456789".into()),
            Cell::Text("x".into()),
        ];
        plan.line(&row, &mut out).unwrap();
        assert_eq!(out, "m n=7u,d=\"12345678901234567890.123456789\",_measurement=\"x\" 1706708700000000000\n");
        // No fields: an error, and nothing written.
        let mut out = String::new();
        let r = plan.line(&[Cell::Int(5), Cell::Null, Cell::Null, Cell::Null, Cell::Null], &mut out);
        assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
        assert_eq!(out, "");
        // NaN isn't dropped either.
        assert!(matches!(plan.line(&[Cell::Int(5), Cell::Null, Cell::Float(f64::NAN), Cell::Null, Cell::Null], &mut out), Err(Error::Unsupported(_))));
        // No time column.
        assert!(Plan::new("m", &names(&["v"]), &[], &[], Api::Sql).is_err());
    }

    #[test]
    fn flux_bookkeeping_names_are_data() {
        // Fields named like Flux's columns are kept (1.x, 3.x, SQL sources).
        let cols = names(&["time", "result", "table", "_value", "v"]);
        for api in [Api::InfluxQl, Api::Flux, Api::Sql] {
            let plan = Plan::new("m", &cols, &[], &[], api).unwrap();
            let mut out = String::new();
            plan.line(&[Cell::Int(1), Cell::Int(2), Cell::Int(3), Cell::Int(4), Cell::Int(5)], &mut out).unwrap();
            assert_eq!(out, "m result=2i,table=3i,_value=4i,v=5i 1\n");
        }
        // What 2.x can't hold is said, not dropped.
        assert!(matches!(Plan::new("m", &names(&["time", "_start"]), &[], &[], Api::Flux), Err(Error::Unsupported(_))));
        let tag = [tc("_field", "tag")];
        assert!(matches!(Plan::new("m", &names(&["time", "_field"]), &tag, &[], Api::Flux), Err(Error::Unsupported(_))));
        assert!(Plan::new("m", &names(&["time", "_start"]), &[], &[], Api::Sql).is_ok());
    }

    #[test]
    fn names_and_tags_escape() {
        let sp = [',', '=', ' '];
        // 1.x and 2.x keep a backslash as it is; 3.x reads `\\` as one.
        assert_eq!(escape(r"a\b", &sp, Api::InfluxQl).unwrap(), r"a\b");
        assert_eq!(escape(r"a\b", &sp, Api::Flux).unwrap(), r"a\b");
        assert_eq!(escape(r"a\b", &sp, Api::Sql).unwrap(), r"a\\b");
        assert_eq!(escape("a,b=c d", &sp, Api::InfluxQl).unwrap(), r"a\,b\=c\ d");
        assert_eq!(escape(r"a\,b", &sp, Api::Sql).unwrap(), r"a\\\,b");
        // What none of them reads back as written.
        for api in [Api::InfluxQl, Api::Flux, Api::Sql] {
            assert!(escape(r"end\", &sp, api).is_err());
            assert!(escape("l1\nl2", &sp, api).is_err());
        }
        assert!(escape(r"a\,b", &sp, Api::Flux).is_err());
        assert!(escape(r"a\ b", &sp, Api::InfluxQl).is_err());
        // A tag value that can't go fails the row.
        let read = [tc("time", "time"), tc("t", "tag"), tc("v", "")];
        let plan = Plan::new("m", &names(&["time", "t", "v"]), &read, &[], Api::Flux).unwrap();
        let r = plan.line(&[Cell::Int(1), Cell::Text(r"end\".into()), Cell::Int(1)], &mut String::new());
        assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
        let mut out = String::new();
        plan.line(&[Cell::Int(1), Cell::Text(r"a\b".into()), Cell::Int(1)], &mut out).unwrap();
        assert_eq!(out, "m,t=a\\b v=1i 1\n");
        // A measurement starting with `#` would be a comment.
        assert!(matches!(Plan::new("#m", &names(&["time"]), &[], &[], Api::Sql), Err(Error::Unsupported(_))));
    }

    #[test]
    fn times_parse() {
        assert_eq!(parse_time("2024-01-31T13:45:00Z"), Some(1_706_708_700_000_000_000));
        assert_eq!(parse_time("2024-01-31 13:45:00.5+00:00"), Some(1_706_708_700_500_000_000));
        assert_eq!(parse_time("2024-01-31T10:45:00-03:00"), Some(1_706_708_700_000_000_000));
        assert_eq!(parse_time("2024-01-31T13:45:00.123456789"), Some(1_706_708_700_123_456_789));
        assert_eq!(parse_time("2024-01-31"), Some(1_706_659_200_000_000_000));
        assert_eq!(parse_time("1969-12-31T23:59:59Z"), Some(-1_000_000_000));
        assert_eq!(parse_time("nope"), None);
        assert_eq!(ns_cell(1_706_708_700_000_000_001), Cell::DateTimeTz("2024-01-31 13:45:00.000000001+00:00".into()));
        assert_eq!(ns_cell(1_706_708_700_000_000_000), Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()));
    }

    #[test]
    fn json_doubles_are_exact() {
        let v = parse_json(br#"{"a":[1.0715660391465826e-75, -0.1, 3, 18446744073709551615, "x\"\u00e9", true, null, {}, []], "b": "\u00f1", "c": "ok"}"#).unwrap();
        let a = v["a"].as_array().unwrap();
        assert_eq!(a[0].as_f64().unwrap().to_bits(), 1.0715660391465826e-75f64.to_bits());
        assert_eq!(a[1].as_f64(), Some(-0.1));
        assert_eq!(a[2].as_i64(), Some(3));
        assert_eq!(a[3].as_u64(), Some(u64::MAX));
        assert_eq!(a[4].as_str(), Some("x\"é"));
        assert_eq!(v["b"].as_str(), Some("ñ"));
        // Every double of a spread, exactly.
        let mut x = 0x1234_5678_9abc_def1u64;
        for _ in 0..5_000 {
            x ^= x << 13;
            x ^= x >> 7;
            x ^= x << 17;
            let f = f64::from_bits(x);
            if !f.is_finite() {
                continue;
            }
            let text = serde_json::to_string(&json!([f])).unwrap();
            assert_eq!(parse_json(text.as_bytes()).unwrap()[0].as_f64().unwrap().to_bits(), f.to_bits(), "{text}");
        }
        assert!(parse_json(b"{\"a\":1").is_err());
        assert!(parse_json(b"[1] x").is_err());
    }

    #[test]
    fn influxql_series_become_rows() {
        let cols = [tc("time", "time"), tc("host", "tag"), tc("v", "float"), tc("n", "integer")];
        let s = json!({"name": "cpu", "columns": ["time", "host", "n", "v"], "values": [[1706708700000000000i64, "a", 3, 2], [1706708700000000001i64, null, null, 2.5]]});
        assert_eq!(
            series_rows(&s, &cols),
            vec![
                vec![ns_cell(1706708700000000000), Cell::Text("a".into()), Cell::Float(2.0), Cell::Int(3)],
                vec![ns_cell(1706708700000000001), Cell::Null, Cell::Float(2.5), Cell::Null],
            ]
        );
        // GROUP BY tags come apart from the values.
        let s = json!({"name": "cpu", "tags": {"host": "b"}, "columns": ["time", "v"], "values": [[5, 1.0]]});
        assert_eq!(series_rows(&s, &cols)[0][1], Cell::Text("b".into()));
    }

    #[test]
    fn asked_columns_must_exist() {
        let catalog = [ci("time", "time", true), ci("host", "tag", true), ci("v", "float", false)];
        let got = read_columns(&catalog, &Some(names(&["v", "time"]))).unwrap();
        assert_eq!(got.iter().map(|c| c.name.as_str()).collect::<Vec<_>>(), ["v", "time"]);
        assert!(matches!(read_columns(&catalog, &Some(names(&["time", "nope"]))), Err(Error::Query(_))));
        assert_eq!(read_columns(&catalog, &None).unwrap().len(), 3);
    }

    /// Feeds `text` to an [`InfluxQlStream`] in pieces of `step` bytes: the
    /// rows, and the most bytes it held at once.
    fn influxql(text: &[u8], cols: &[TransferColumn], step: usize) -> Result<(Vec<Vec<Cell>>, usize)> {
        let (mut st, mut out, mut held) = (InfluxQlStream::new(cols), Vec::new(), 0);
        for piece in text.chunks(step) {
            st.feed(piece, &mut out)?;
            held = held.max(st.held());
        }
        st.finish(&mut out)?;
        Ok((out, held))
    }

    #[test]
    fn influxql_chunks_are_streamed() {
        let cols = [tc("time", "time"), tc("host", "tag"), tc("s", "string"), tc("v", "float")];
        // 20 small points, then 2,000 of ~6 KB, in one chunk (one line), as
        // 1.x sends it when a sample of the first points looked small.
        let wide = "w".repeat(6_000);
        let mut values: Vec<Value> = (0..20).map(|i| json!([i, format!("s{i}"), -0.0])).collect();
        values.extend((20..2_020).map(|i| json!([i, format!("{wide}],\"[{{,{i}"), i as f64 + 0.5])));
        let line = |values: &[Value], tags: Option<Value>| {
            let mut series = json!({"name": "m", "columns": ["time", "s", "v"], "values": values, "partial": true});
            if let Some(t) = tags {
                series["tags"] = t;
            }
            format!("{}\n", json!({"results": [{"statement_id": 0, "series": [series], "partial": true}]}))
        };
        let mut text = line(&values, None).replace("-0.0", "-0");
        assert!(text.len() > 12_000_000 && text.contains(",-0]"));
        // A second chunk, GROUP BY-like, with the tag before the columns.
        text.push_str(&line(&values[..3], Some(json!({"host": "h"}))).replacen("\"name\":\"m\",", "\"name\":\"m\",\"tags\":{\"host\":\"h\"},", 1));
        let whole = {
            let mut out = Vec::new();
            for l in text.lines() {
                influxql_line(l.as_bytes(), &cols, &mut out).unwrap();
            }
            out
        };
        assert_eq!(whole.len(), 2_023);
        let (rows, held) = influxql(text.as_bytes(), &cols, 64 * 1024).unwrap();
        assert_eq!(rows, whole);
        // Never the ~12 MB chunk: a piece and a point.
        assert!(held < 64 * 1024 + 8 * 1024, "{held}");
        assert!(matches!(rows[0][3], Cell::Float(f) if f == 0.0 && f.is_sign_negative()), "{:?}", rows[0][3]);
        assert_eq!(rows[2_021][1], Cell::Text("h".into()));
        // Byte by byte, every place a piece can end.
        let small = line(&values[..3], None) + &line(&values[18..22], None);
        let (rows, _) = influxql(small.as_bytes(), &cols, 1).unwrap();
        assert_eq!(rows.len(), 7);
        assert_eq!(rows[..3], whole[..3]);
        assert_eq!(rows[3..], whole[18..22]);
        // Errors, and a point too big to hold.
        assert!(influxql(b"{\"results\":[{\"statement_id\":0,\"error\":\"boom\"}]}\n", &cols, 7).is_err());
        assert!(influxql(b"{\"results\":[{\"series\":[{\"columns\":[\"time\"],\"values\":[[1,]]}]}]}\n", &cols, 5).is_err());
        let huge = format!("{{\"results\":[{{\"series\":[{{\"columns\":[\"time\",\"s\"],\"values\":[[1,\"{}", "x".repeat(LINE_BYTES));
        assert!(matches!(influxql(huge.as_bytes(), &cols, 1024 * 1024), Err(Error::Unsupported(_))));
    }

    #[test]
    fn negative_zero_keeps_its_sign() {
        let v = parse_json(b"[-0, 0, -00, -0.0, -1]").unwrap();
        let f = |i: usize| json_cell(&v[i], "v", "float");
        assert!(matches!(f(0), Cell::Float(x) if x == 0.0 && x.is_sign_negative()), "{:?}", f(0));
        assert!(matches!(f(1), Cell::Float(x) if x == 0.0 && x.is_sign_positive()));
        assert!(matches!(f(3), Cell::Float(x) if x.is_sign_negative()));
        assert_eq!(v[4].as_i64(), Some(-1));
        assert_eq!(v[1].as_i64(), Some(0));
    }

    fn flux(text: &str, cols: &[TransferColumn], cuts: &[usize]) -> Result<Vec<Vec<Cell>>> {
        let (mut csv, mut t, mut recs, mut rows) = (CsvStream::default(), FluxTables::default(), Vec::new(), Vec::new());
        let mut at = 0;
        for &c in cuts.iter().chain([text.len()].iter()) {
            csv.feed(&text.as_bytes()[at..c], &mut recs);
            at = c;
        }
        csv.finish(&mut recs);
        for r in recs {
            t.record(r, cols, &mut rows)?;
        }
        Ok(rows)
    }

    #[test]
    fn flux_csv_streams_across_chunks() {
        let text = "#datatype,string,long,dateTime:RFC3339,string,double,long\r\n\
#group,false,false,false,true,false,false\r\n\
#default,_result,,,,,\r\n\
,result,table,_time,host,v,n\r\n\
,,0,2024-01-31T13:45:00.000000001Z,\"a,\"\"b\"\"\",1.5,\r\n\
,,0,2024-01-31T13:46:00Z,a,,7\r\n\r\n";
        let cols = [tc("_time", "time"), tc("host", "tag"), tc("v", "field"), tc("n", "field")];
        // Every split point gives the same rows.
        for cut in [1, 7, 60, 150, 200, text.len() - 3] {
            assert_eq!(
                flux(text, &cols, &[cut]).unwrap(),
                vec![
                    vec![ns_cell(1706708700000000001), Cell::Text("a,\"b\"".into()), Cell::Float(1.5), Cell::Null],
                    vec![ns_cell(1706708760000000000), Cell::Text("a".into()), Cell::Null, Cell::Int(7)],
                ],
                "cut at {cut}"
            );
        }
        // An error table.
        let err = "#datatype,string,string\n#group,true,true\n#default,,\n,error,reference\n,boom,\n";
        assert!(matches!(flux(err, &cols, &[]), Err(Error::Query(m)) if m == "boom"));
        // A measurement with tags named error and reference is data.
        let text = "#datatype,string,long,dateTime:RFC3339,string,string,double\r\n\
#group,false,false,false,true,true,false\r\n\
#default,_result,,,,,\r\n\
,result,table,_time,error,reference,v\r\n\
,,0,2024-01-31T13:45:00Z,none,abc,1\r\n";
        let cols = [tc("_time", "time"), tc("error", "tag"), tc("reference", "tag"), tc("v", "field")];
        assert_eq!(flux(text, &cols, &[]).unwrap(), vec![vec![ns_cell(1706708700000000000), Cell::Text("none".into()), Cell::Text("abc".into()), Cell::Float(1.0)]]);
    }

    #[test]
    fn flux_csv_keeps_strings() {
        // `\n` in a value comes as `\r\n` (Flux's CSV is CRLF); fields named
        // result / table come after Flux's own; marked strings: "" and a
        // `\r` (see flux_read_script).
        let text = "#datatype,string,long,dateTime:RFC3339,string,string,string,string,string,string\r\n\
#group,false,false,false,false,false,false,false,false,false\r\n\
#default,_result,,,,,,,,\r\n\
,result,table,_time,\u{1}_e,\u{1}_c,s,result,table,e\r\n\
,,0,2024-01-31T13:45:00Z,;,p\\rq\\\\;,\"x\r\ny\",r1,t1,\r\n";
        let cols = [tc("_time", "time"), tc("s", "field"), tc("e", "field"), tc("c", "field"), tc("result", "field"), tc("table", "field")];
        for cut in [0, 100, 150, 170, 180, 190, text.len() - 1] {
            assert_eq!(
                flux(text, &cols, &[cut]).unwrap(),
                vec![vec![
                    ns_cell(1706708700000000000),
                    Cell::Text("x\ny".into()),
                    Cell::Text(String::new()),
                    Cell::Text("p\rq\\".into()),
                    Cell::Text("r1".into()),
                    Cell::Text("t1".into()),
                ]],
                "cut at {cut}"
            );
        }
    }

    #[test]
    fn flux_script_reads_all_of_time() {
        let all = "range(start: time(v: -9223372036854775806), stop: time(v: 9223372036854775807))";
        let s = flux_read_script("b", "m\"x", false, Some("r.v > 1"));
        assert!(s.contains(all) && s.contains("r._measurement == \"m\\\"x\""), "{s}");
        assert!(s.ends_with("|> pivot(rowKey: [\"_time\"], columnKey: [\"_field\"], valueColumn: \"_value\")\n  |> filter(fn: (r) => r.v > 1)"), "{s}");
        let s = flux_read_script("b", "m", true, None);
        assert!(s.contains("union(tables: [plain, marked])") && s.contains("columnKey: [\"_dbine_m\", \"_field\"]"), "{s}");
        assert_eq!(s.matches(all).count(), 2, "{s}");
        let s = flux_check_script("b", "m", &["host"]);
        assert!(s.contains("r._value =~ /^$|\\r/") && s.contains("r[\"host\"] =~ /\\r/") && s.contains("die(msg:"), "{s}");
        assert!(flux_check_script("b", "m", &[]).ends_with("\nlossy"));
        assert!(matches!(flux_err(Error::Query(format!("runtime error: {CR_TAG}"))), Error::Unsupported(_)));
    }

    #[test]
    fn jsonl_values() {
        assert_eq!(json_cell(&json!("2024-01-31T13:45:00"), "time", "Timestamp(Nanosecond, None)"), ns_cell(1706708700000000000));
        assert_eq!(json_cell(&json!(3), "v", "Float64"), Cell::Float(3.0));
        assert_eq!(json_cell(&json!(3), "n", "Int64"), Cell::Int(3));
        assert_eq!(json_cell(&json!("a"), "host", "Dictionary(Int32, Utf8)"), Cell::Text("a".into()));
    }

    // ------------------------------------------------ loads against a fake server

    /// Each request's body; a request "commits" `delay` after it arrives,
    /// then gets its answer (like 3.x's WAL flush).
    #[derive(Default)]
    struct Server {
        bodies: Mutex<Vec<String>>,
    }

    async fn serve(delay: std::time::Duration, fail_on: Option<&'static str>) -> (Arc<Server>, String) {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let url = format!("http://{}/write", listener.local_addr().unwrap());
        let server = Arc::new(Server::default());
        let s = server.clone();
        tokio::spawn(async move {
            loop {
                let Ok((conn, _)) = listener.accept().await else { return };
                let s = s.clone();
                tokio::spawn(async move {
                    let mut conn = BufReader::new(conn);
                    loop {
                        let mut len = 0usize;
                        loop {
                            let mut line = String::new();
                            if conn.read_line(&mut line).await.unwrap_or(0) == 0 {
                                return;
                            }
                            if line == "\r\n" {
                                break;
                            }
                            if let Some(v) = line.to_ascii_lowercase().strip_prefix("content-length:") {
                                len = v.trim().parse().unwrap();
                            }
                        }
                        let mut body = vec![0; len];
                        conn.read_exact(&mut body).await.unwrap();
                        let body = String::from_utf8(body).unwrap();
                        let bad = fail_on.is_some_and(|f| body.contains(f));
                        tokio::time::sleep(delay).await;
                        let answer: &[u8] = if bad {
                            b"HTTP/1.1 400 Bad Request\r\ncontent-length: 15\r\n\r\n{\"error\":\"bad\"}"
                        } else {
                            s.bodies.lock().unwrap().push(body);
                            b"HTTP/1.1 204 No Content\r\n\r\n"
                        };
                        conn.get_mut().write_all(answer).await.unwrap();
                    }
                });
            }
        });
        (server, url)
    }

    fn endpoint(url: String) -> Endpoint {
        Endpoint { http: reqwest::Client::new(), url, auth: Auth::None, api: Api::InfluxQl }
    }

    fn spec(commit_rows: u64) -> LoadSpec {
        let table = dbine_driver::ObjectRef { kind: "measurement".into(), schema: None, name: "m".into() };
        LoadSpec { table, columns: names(&["time", "s"]), table_lock: false, keep_identity: false, commit_rows, commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES }
    }

    /// Batches, then (optionally) nothing ever again.
    struct Source(Vec<RowBatch>, bool);

    #[dbine_driver::async_trait]
    impl BatchSource for Source {
        async fn next(&mut self) -> Option<RowBatch> {
            if self.0.is_empty() {
                if self.1 {
                    std::future::pending::<()>().await;
                }
                return None;
            }
            Some(self.0.remove(0))
        }
    }

    fn rows(n: usize, from: usize, text: &str) -> RowBatch {
        RowBatch { rows: (from..from + n).map(|i| vec![Cell::Int(i as i64 + 1), Cell::Text(text.into())]).collect(), bytes: 0 }
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn requests_stay_under_their_size() {
        let (server, url) = serve(std::time::Duration::ZERO, None).await;
        let wide = "x".repeat(6_000);
        let batches = (0..5).map(|b| rows(1_000, b * 1_000, &wide)).collect();
        let columns = [tc("time", "time"), tc("s", "")];
        let reports = Mutex::new(Vec::new());
        let n = load(endpoint(url), "m", &[], &spec(100_000), &columns, &mut Source(batches, false), &|n| reports.lock().unwrap().push(n)).await.unwrap();
        assert_eq!(n, 5_000);
        let bodies = server.bodies.lock().unwrap();
        assert!(bodies.len() > 1 && bodies.iter().all(|b| b.len() <= REQUEST_BYTES), "{:?}", bodies.iter().map(String::len).collect::<Vec<_>>());
        assert_eq!(bodies.iter().map(|b| b.lines().count()).sum::<usize>(), 5_000);
        assert_eq!(*reports.lock().unwrap().last().unwrap(), 5_000);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn a_failed_load_leaves_nothing_out() {
        let delay = std::time::Duration::from_millis(400);
        let columns = [tc("time", "time"), tc("s", "")];
        // Rejected by the server: the other requests are answered first.
        let (server, url) = serve(delay, Some("bad")).await;
        let batches = vec![rows(10, 0, "ok"), rows(10, 10, "ok"), rows(10, 20, "bad"), rows(10, 30, "ok")];
        let r = load(endpoint(url), "m", &[], &spec(10), &columns, &mut Source(batches, false), &|_| {}).await;
        assert!(r.is_err());
        let at_return = server.bodies.lock().unwrap().len();
        tokio::time::sleep(delay * 2).await;
        assert_eq!(server.bodies.lock().unwrap().len(), at_return);
        // A bad row of the source: the same.
        let (server, url) = serve(delay, None).await;
        let mut bad = rows(1, 20, "x");
        bad.rows[0][0] = Cell::Text("nope".into());
        let batches = vec![rows(10, 0, "ok"), bad];
        assert!(load(endpoint(url), "m", &[], &spec(10), &columns, &mut Source(batches, false), &|_| {}).await.is_err());
        assert_eq!(server.bodies.lock().unwrap().len(), 1);
        // Cancelled (the future dropped) while a request is out.
        let (server, url) = serve(delay, None).await;
        let ep = endpoint(url);
        let spec = spec(10);
        let fut = async {
            let mut source = Source(vec![rows(10, 0, "ok")], true);
            load(ep, "m", &[], &spec, &columns, &mut source, &|_| {}).await
        };
        assert!(tokio::time::timeout(std::time::Duration::from_millis(50), fut).await.is_err());
        assert_eq!(server.bodies.lock().unwrap().len(), 1);
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn fieldless_rows_fail() {
        let (server, url) = serve(std::time::Duration::ZERO, None).await;
        let columns = [tc("time", "time"), tc("s", "")];
        let mut empty = rows(10, 0, "");
        for r in &mut empty.rows {
            r[1] = Cell::Null;
        }
        let r = load(endpoint(url), "m", &[], &spec(10), &columns, &mut Source(vec![empty, rows(10, 10, "ok")], false), &|_| {}).await;
        assert!(matches!(r, Err(Error::Unsupported(_))), "{r:?}");
        assert!(server.bodies.lock().unwrap().iter().all(|b| !b.is_empty()));
    }
}
