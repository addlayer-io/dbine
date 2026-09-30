//! Bulk transfer (see `dbine_driver::transfer`) for TDengine, over the same
//! REST API as the rest of the driver, always in UTC (the session's display
//! zone is left out, so every instant travels without a zone).
//!
//! Reading: normal tables, subtables and supertables are read in pages of
//! at most [`PAGE`] rows and about [`PAGE_BYTES`] (by the columns' declared
//! widths) ordered by their timestamp (the first column), walking
//! ahead by it (`WHERE ts >= …`), the next page requested while the
//! current one is handed over. Rows sharing the page's last instant (other
//! subtables of a supertable, a composite key) are left for the next page,
//! which starts at that instant, so none is lost or repeated; an instant
//! that fills a whole page is read in pages of its own, ordered by
//! subtable and composite key (`LIMIT … OFFSET …`). A supertable
//! is read with `tbname` first and its tags, so a load can recreate its
//! subtables. Values are typed by the answer's column types: TIMESTAMP as
//! date-time text with the database's precision (ms, µs or ns digits),
//! signed and unsigned integers, FLOAT/DOUBLE, BOOL, VARCHAR/BINARY/NCHAR as
//! text, VARBINARY/GEOMETRY as bytes, JSON tags as JSON, DECIMAL exact.
//! Fractional numbers are parsed from the answer's text bit for bit (see
//! [`quote_floats`]); FLOAT as the `f32` it is, widened exactly.
//! Views and the rest of the objects go through the browse query.
//!
//! Loading: multi-row `INSERT INTO t (cols) VALUES (…)(…)…`, the insert
//! script's statement, each one filled up to [`MAX_SQL`] bytes (under the
//! 1 MB default SQL length limit), [`IN_FLIGHT`] at once. Into a supertable
//! the rows name their subtable in `tbname` and carry its tags, as in the
//! insert script (TDengine creates the missing subtables), and each
//! statement holds one subtable's rows: one spanning several subtables
//! isn't atomic (a failure can leave part of it committed), one subtable's
//! is. Timestamps are
//! written as integers in the target database's precision, so the server's
//! zone never shifts them; an instant finer than that precision is an
//! error (TDengine would cut it, and rows cut to the same key overwrite
//! each other), and so is NaN or an infinity (TDengine can't store them).
//! Schemaless line protocol was left aside: it
//! decides the types and the tag/column split by itself, and the target's
//! structure is the one the migration already created.
//!
//! TDengine has no transactions: every statement commits by itself, and a
//! statement the server received commits even when its client stops
//! waiting. So the INSERTs in flight are never abandoned: a failed or
//! cancelled load waits for them (counting what they committed) before it
//! returns, and a load dropped half-way waits for them while it's dropped
//! (see [`Inserts`]), so no row is committed after it's gone. Progress
//! counts only rows of statements the server confirmed.

use crate::ddl::{lit, q, qualified, SUBTABLE, SUPERTABLE};
use crate::{encode, http_error, td_error, Answer, Cancel, Conn, TdSession};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{kinds, Error, Result};
use serde_json::Value;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::{JoinHandle, JoinSet};

/// Rows per read request at most…
const PAGE: usize = 50_000;
/// …and about this many bytes of answer, by the columns' declared widths
/// (the answer is held as text, then parsed, while the next page is on its
/// way: a few times this per table).
const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// An INSERT not answered in this long fails (a cancel waits for the
/// statements in flight, so it can't wait forever).
const INSERT_TIMEOUT: Duration = Duration::from_secs(600);
/// Bytes of one INSERT statement at most (one row may pass it).
const MAX_SQL: usize = 900_000;
/// INSERT requests in flight at once.
const IN_FLIGHT: usize = 4;
/// Bytes of statements being filled at once (one per subtable, into a
/// supertable) at most: past it, all of them are sent.
const MAX_BUFFERED: usize = 12 * 1024 * 1024;

/// The database's timestamp precision.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Precision {
    Ms,
    Us,
    Ns,
}

impl Precision {
    fn parse(s: &str) -> Precision {
        match s.trim() {
            "us" => Precision::Us,
            "ns" => Precision::Ns,
            _ => Precision::Ms,
        }
    }

    /// Nanoseconds per unit.
    fn unit(self) -> i128 {
        match self {
            Precision::Ms => 1_000_000,
            Precision::Us => 1_000,
            Precision::Ns => 1,
        }
    }
}

/// A REST value as a cell, given its TDengine type (`column_meta`).
pub(crate) fn to_cell(v: Value, ty: &str) -> Cell {
    let base = ty.split('(').next().unwrap_or(ty).trim().to_ascii_uppercase();
    match v {
        Value::Null => Cell::Null,
        Value::Bool(b) => Cell::Bool(b),
        Value::Number(n) => {
            if base == "DECIMAL" {
                Cell::Decimal(n.to_string())
            } else if base == "FLOAT" {
                float_cell(n.to_string())
            } else if base == "DOUBLE" {
                Cell::Float(n.as_f64().unwrap_or(f64::NAN))
            } else if let Some(i) = n.as_i64().filter(|_| !base.ends_with("UNSIGNED")) {
                Cell::Int(i)
            } else if let Some(u) = n.as_u64() {
                Cell::UInt(u)
            } else if let Some(i) = n.as_i64() {
                Cell::Int(i)
            } else {
                Cell::Float(n.as_f64().unwrap_or(f64::NAN))
            }
        }
        Value::String(s) => match base.as_str() {
            "TIMESTAMP" => datetime_cell(&s),
            "VARBINARY" | "GEOMETRY" | "BLOB" => match unhex(&s) {
                Some(b) => Cell::Bytes(b),
                None => Cell::Text(s),
            },
            "DECIMAL" => Cell::Decimal(s),
            "FLOAT" => float_cell(s),
            "DOUBLE" => s.parse().map_or(Cell::Text(s), Cell::Float),
            "TINYINT" | "SMALLINT" | "INT" | "BIGINT" => s.parse().map_or(Cell::Text(s), Cell::Int),
            b if b.ends_with("UNSIGNED") => s.parse().map_or(Cell::Text(s), Cell::UInt),
            "JSON" => Cell::Json(s),
            "BOOL" => Cell::Bool(s == "true"),
            _ => Cell::Text(s),
        },
        other => Cell::Json(other.to_string()),
    }
}

/// A FLOAT as REST prints it (the shortest text of the `f32`, like
/// `3.4028235e+38`): parsed as the `f32` it is and widened, exact. Parsed
/// as an `f64` it would be another number, above `FLT_MAX` there.
fn float_cell(s: String) -> Cell {
    s.parse::<f32>().map_or(Cell::Text(s), |f| Cell::Float(f64::from(f)))
}

/// `2024-01-31T13:45:00.123456Z` → `2024-01-31 13:45:00.123456` (the
/// digits of the database's precision kept); with an offset, a zoned value.
fn datetime_cell(s: &str) -> Cell {
    let t = s.replacen('T', " ", 1);
    if let Some(u) = t.strip_suffix('Z') {
        return Cell::DateTime(u.to_string());
    }
    match t.rfind(['+', '-']).filter(|&i| i > 18) {
        Some(_) => Cell::DateTimeTz(t),
        None => Cell::DateTime(t),
    }
}

fn unhex(s: &str) -> Option<Vec<u8>> {
    let s = s.strip_prefix("\\x").or_else(|| s.strip_prefix("0x")).unwrap_or(s);
    if !s.len().is_multiple_of(2) {
        return None;
    }
    (0..s.len()).step_by(2).map(|i| s.get(i..i + 2).and_then(|h| u8::from_str_radix(h, 16).ok())).collect()
}

/// Days since 1970-01-01 of a civil date (proleptic Gregorian).
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let mp = (m + 9) % 12;
    let doy = (153 * mp + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// Nanoseconds since the epoch of `YYYY-MM-DD[ T]HH:MM:SS[.f…][Z|±HH:MM]`
/// (or a bare date); a value without a zone is UTC.
pub(crate) fn epoch_nanos(s: &str) -> Option<i128> {
    let s = s.trim();
    let num = |t: &str| -> Option<i64> { (!t.is_empty() && t.bytes().all(|b| b.is_ascii_digit())).then(|| t.parse().ok()).flatten() };
    let (date, rest) = match s.find([' ', 'T']) {
        Some(i) => (&s[..i], s[i + 1..].trim()),
        None => (s, ""),
    };
    let neg = date.starts_with('-');
    let mut dp = date.trim_start_matches('-').splitn(3, '-');
    let (y, m, d) = (num(dp.next()?)?, num(dp.next()?)?, num(dp.next()?)?);
    let y = if neg { -y } else { y };
    if !(1..=12).contains(&m) || !(1..=31).contains(&d) {
        return None;
    }
    let mut nanos = days_from_civil(y, m, d) as i128 * 86_400_000_000_000;
    if rest.is_empty() {
        return Some(nanos);
    }
    // Zone: `Z`, or `±HH[:MM]` after the time.
    let (time, offset) = if let Some(t) = rest.strip_suffix('Z') {
        (t.trim(), 0i128)
    } else if let Some(i) = rest.rfind(['+', '-']) {
        let z = rest[i + 1..].replace(':', "");
        let (h, mi) = (num(z.get(..2)?)?, if z.len() > 2 { num(&z[2..])? } else { 0 });
        let sign = if rest.as_bytes()[i] == b'-' { -1 } else { 1 };
        (rest[..i].trim(), sign * (h as i128 * 3600 + mi as i128 * 60) * 1_000_000_000)
    } else {
        (rest, 0)
    };
    let (hms, frac) = match time.split_once('.') {
        Some((a, b)) => (a, b),
        None => (time, ""),
    };
    let mut tp = hms.splitn(3, ':');
    let (h, mi) = (num(tp.next()?)?, num(tp.next()?)?);
    let sec = match tp.next() {
        Some(x) => num(x)?,
        None => 0,
    };
    if h > 23 || mi > 59 || sec > 60 || !frac.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let frac_ns: i128 = format!("{:0<9}", &frac[..frac.len().min(9)]).parse().ok()?;
    nanos += (h as i128 * 3600 + mi as i128 * 60 + sec as i128) * 1_000_000_000 + frac_ns;
    Some(nanos - offset)
}

/// Whether the fraction of a date-time has non-zero digits past the
/// nanosecond (`epoch_nanos` keeps nine).
fn sub_nanos(s: &str) -> bool {
    let Some(i) = s.find('.') else { return false };
    s[i + 1..].bytes().take_while(u8::is_ascii_digit).skip(9).any(|b| b != b'0')
}

impl Precision {
    fn name(self) -> &'static str {
        match self {
            Precision::Ms => "ms",
            Precision::Us => "us",
            Precision::Ns => "ns",
        }
    }
}

/// A cell as a TDengine literal; into a TIMESTAMP column, dates and
/// date-times become the instant's integer in the target's precision. An
/// instant finer than that precision, NaN and the infinities are errors:
/// TDengine would cut the first (rows cut to the same timestamp overwrite
/// each other) and can't store the others.
pub(crate) fn literal(c: &Cell, timestamp: Option<Precision>) -> Result<String> {
    if let Some(p) = timestamp {
        match c {
            Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Date(s) | Cell::Text(s) => {
                if let Some(n) = epoch_nanos(s) {
                    if n.rem_euclid(p.unit()) != 0 || sub_nanos(s) {
                        return Err(Error::Unsupported(format!(
                            "el instante {s} tiene más precisión que la base de destino (PRECISION '{}'); TDengine lo recortaría y las filas que quedan con la misma marca de tiempo se pisan entre sí. Creá la base de destino con una precisión más fina.",
                            p.name()
                        )));
                    }
                    return Ok(n.div_euclid(p.unit()).to_string());
                }
            }
            _ => {}
        }
    }
    Ok(match c {
        Cell::Null => "NULL".into(),
        Cell::Bool(b) => if *b { "true" } else { "false" }.into(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        // Every digit of the double: a FLOAT read here is its f32 widened,
        // and this text parses back to exactly it (the f32's own shortest
        // text, `3.4028235e38`, reads as a double above FLT_MAX).
        Cell::Float(f) if f.is_finite() => f.to_string(),
        Cell::Float(f) => {
            return Err(Error::Unsupported(format!(
                "TDengine no puede guardar {f} (NaN ni infinito) en FLOAT ni DOUBLE; guardarlo como NULL perdería el valor"
            )))
        }
        Cell::Bytes(b) => {
            let mut s = String::with_capacity(4 + b.len() * 2);
            s.push_str("'\\x");
            for x in b {
                s.push_str(&format!("{x:02X}"));
            }
            s.push('\'');
            s
        }
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => lit(s),
    })
}

/// A column name in a statement (`tbname` is a keyword, not a column).
fn col(name: &str) -> String {
    if name.eq_ignore_ascii_case("tbname") {
        "tbname".into()
    } else {
        q(name)
    }
}

/// Fractional JSON numbers at nesting `depth` (the rows' values) turned
/// into strings, so they're parsed here bit for bit: the JSON parser's
/// best-effort float parsing can miss the last digit of a double.
pub(crate) fn quote_floats(body: &str, depth: usize) -> String {
    let b = body.as_bytes();
    let mut out = String::with_capacity(body.len() + 256);
    let (mut level, mut i, mut from) = (0usize, 0usize, 0usize);
    while i < b.len() {
        match b[i] {
            b'"' => {
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
            }
            b'[' | b'{' => level += 1,
            b']' | b'}' => level = level.saturating_sub(1),
            b'-' | b'0'..=b'9' if level == depth => {
                let start = i;
                while i < b.len() && matches!(b[i], b'0'..=b'9' | b'-' | b'+' | b'.' | b'e' | b'E') {
                    i += 1;
                }
                let token = &body[start..i];
                if token.contains(['.', 'e', 'E']) {
                    out.push_str(&body[from..start]);
                    out.push('"');
                    out.push_str(token);
                    out.push('"');
                    from = i;
                }
                continue;
            }
            _ => {}
        }
        i += 1;
    }
    out.push_str(&body[from..]);
    out
}

/// A statement over REST, its answer's floats kept exact (see
/// [`quote_floats`]); like `Conn::sql` otherwise.
async fn exact_sql(conn: &Conn, db: &str, stmt: String, timeout: Option<Duration>) -> Result<Answer> {
    let url = format!("{}/rest/sql/{}", conn.base, encode(db));
    let mut rb = conn.http.post(url).basic_auth(&conn.user, Some(&conn.password)).body(stmt);
    if let Some(t) = timeout {
        rb = rb.timeout(t);
    }
    let resp = rb.send().await.map_err(http_error)?;
    let status = resp.status();
    let text = resp.text().await.map_err(http_error)?;
    // One copy of the page at a time: the text, its quoted copy, then the
    // parsed tree (the rows moved out of it, not cloned).
    let quoted = quote_floats(&text, 3);
    let parsed = serde_json::from_str::<Value>(&quoted);
    drop(quoted);
    let mut v = parsed.map_err(|_| {
        if status == reqwest::StatusCode::UNAUTHORIZED {
            Error::AuthFailed(text.trim().to_string())
        } else {
            Error::Query(format!("HTTP {status}: {}", text.trim()))
        }
    })?;
    drop(text);
    let code = v.get("code").and_then(Value::as_i64).unwrap_or(0);
    if code != 0 {
        return Err(td_error(code, v.get("desc").and_then(Value::as_str).unwrap_or("error").to_string()));
    }
    let columns = v
        .get("column_meta")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|c| (c.get(0).map(crate::text).unwrap_or_default(), c.get(1).map(crate::text).unwrap_or_default()))
        .collect();
    let data = match v.get_mut("data").map(Value::take) {
        Some(Value::Array(rows)) => rows
            .into_iter()
            .map(|r| match r {
                Value::Array(a) => a,
                _ => Vec::new(),
            })
            .collect(),
        _ => Vec::new(),
    };
    Ok(Answer { columns, data })
}

/// JSON bytes of one text byte (VARCHAR) or character (NCHAR) at most in
/// a REST answer: control characters travel as `\u00XX`; the rest raw
/// (UTF-8, up to 4 bytes per character).
const JSON_ESCAPE: usize = 6;

/// Bytes of one value of a column in a REST answer at most, by its
/// declared type (`VARCHAR(60000)`…): text as JSON (every byte escaped in
/// the worst case), binaries as hex.
fn value_width(ty: &str) -> usize {
    let base = ty.split('(').next().unwrap_or(ty).trim().to_ascii_uppercase();
    let n: usize = ty.split_once('(').and_then(|(_, r)| r.split([')', ',']).next()?.trim().parse().ok()).unwrap_or(0);
    32 + match base.trim_end_matches(" TAG") {
        "VARCHAR" | "BINARY" | "NCHAR" => JSON_ESCAPE * n,
        "VARBINARY" | "GEOMETRY" => 2 * n,
        "JSON" => JSON_ESCAPE * 4096,
        "BLOB" | "MEDIUMBLOB" => 2 * 4 * 1024 * 1024,
        _ => 0,
    }
}

/// Rows per read page for these column types: [`PAGE_BYTES`] of answer at
/// most, [`PAGE`] rows at most, one at least.
fn page_rows(types: &[String]) -> usize {
    let width: usize = types.iter().map(|t| value_width(t)).sum::<usize>() + 32;
    (PAGE_BYTES / width.max(1)).clamp(1, PAGE)
}

/// Where the read goes next.
enum Cursor {
    Start,
    /// `ts >= key` (the last page's last instant, left out of it).
    From(String),
    /// The rows at exactly `key` (a page that was one single instant), from
    /// this many on, in subtable and composite key order.
    At(String, usize),
    /// `ts > key`.
    After(String),
}

impl TdSession {
    /// The same connection, without the display zone: instants in UTC.
    fn utc(&self) -> Arc<Conn> {
        Arc::new(Conn {
            http: self.conn.http.clone(),
            base: self.conn.base.clone(),
            user: self.conn.user.clone(),
            password: self.conn.password.clone(),
            tz: None,
        })
    }

    async fn precision(&self, db: &str) -> Result<Precision> {
        let a = self.query(&format!("SELECT `precision` FROM information_schema.ins_databases WHERE name = {}", lit(db))).await?;
        Ok(Precision::parse(a.data.first().and_then(|r| r.first()).and_then(Value::as_str).unwrap_or("ms")))
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let kind = spec.table.kind.as_str();
        if !matches!(kind, kinds::TABLE | SUBTABLE | SUPERTABLE) {
            return dbine_driver::transfer::read_via_execute(self, spec, sink).await;
        }
        self.cancel.flag.store(false, Ordering::SeqCst);
        let db = self.obj_db(&spec.table)?;
        let desc = self.describe(&db, &spec.table.name).await?;
        let Some(ts) = desc.first().map(|(c, _)| c.name.clone()) else {
            return Err(Error::Query(format!("No se encontró la tabla {}.", spec.table.name)));
        };
        let names: Vec<String> = match &spec.columns {
            Some(c) => c.clone(),
            None => {
                let mut n: Vec<String> = if kind == SUPERTABLE { vec!["tbname".into()] } else { Vec::new() };
                n.extend(desc.iter().map(|(c, _)| c.name.clone()));
                n
            }
        };
        let mut cols: Vec<TransferColumn> = Vec::with_capacity(names.len());
        for n in &names {
            cols.push(match desc.iter().find(|(c, _)| c.name.eq_ignore_ascii_case(n)) {
                Some((c, tag)) => TransferColumn {
                    name: n.clone(),
                    type_name: if *tag { format!("{} TAG", c.data_type) } else { c.data_type.clone() },
                    nullable: c.nullable || *tag,
                },
                None if n.eq_ignore_ascii_case("tbname") => TransferColumn { name: n.clone(), type_name: "VARCHAR(192)".into(), nullable: false },
                None => return Err(Error::Query(format!("La tabla {} no tiene la columna {n}.", spec.table.name))),
            });
        }
        let mut types: Vec<String> = cols.iter().map(|c| c.type_name.clone()).collect();
        types.push("TIMESTAMP".into());
        let page = page_rows(&types);
        // Inside one instant, rows are told apart by subtable and composite
        // key (`COMPOSITE KEY` in DESCRIBE's note).
        let mut within: Vec<String> = if kind == SUPERTABLE { vec!["tbname".into()] } else { Vec::new() };
        for r in self.strings(&format!("DESCRIBE {}", qualified(Some(&db), &spec.table.name))).await? {
            if r.len() >= 4 && (r[3].contains("COMPOSITE KEY") || r[3].contains("PRIMARY KEY")) && !r[0].eq_ignore_ascii_case(&ts) {
                within.push(q(&r[0]));
            }
        }
        let list: Vec<String> = names.iter().map(|n| col(n)).collect();
        // The timestamp once more at the end: the page's walking key.
        let select = format!("SELECT {}, {} FROM {}", list.join(", "), q(&ts), qualified(Some(&db), &spec.table.name));
        let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()).map(str::to_string);
        let stmt = |cur: &Cursor| -> String {
            let mut conds: Vec<String> = filter.iter().map(|f| format!("({f})")).collect();
            let mut order = vec![q(&ts)];
            let mut offset = 0;
            match cur {
                Cursor::Start => {}
                Cursor::From(k) => conds.push(format!("{} >= {}", q(&ts), lit(k))),
                Cursor::After(k) => conds.push(format!("{} > {}", q(&ts), lit(k))),
                Cursor::At(k, from) => {
                    conds.push(format!("{} = {}", q(&ts), lit(k)));
                    order.extend(within.iter().cloned());
                    offset = *from;
                }
            }
            let mut s = select.clone();
            if !conds.is_empty() {
                s.push_str(" WHERE ");
                s.push_str(&conds.join(" AND "));
            }
            s.push_str(&format!(" ORDER BY {} LIMIT {page}", order.join(", ")));
            if offset > 0 {
                s.push_str(&format!(" OFFSET {offset}"));
            }
            s
        };
        let conn = self.utc();
        let fetch = |cur: &Cursor| -> JoinHandle<Result<crate::Answer>> {
            let (conn, db, s) = (conn.clone(), db.clone(), stmt(cur));
            tokio::spawn(async move { exact_sql(&conn, &db, s, None).await })
        };

        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut builder = BatchBuilder::new();
        let mut cursor = Cursor::Start;
        let mut pending = fetch(&cursor);
        loop {
            let got = self.cancel.run(async { (&mut pending).await.map_err(|e| Error::State(format!("lectura interrumpida: {e}")))? }).await;
            let answer = match got {
                Ok(a) => a,
                Err(e) => {
                    pending.abort();
                    return Err(e);
                }
            };
            let types: Vec<String> = answer.columns.iter().map(|c| c.1.clone()).collect();
            let mut rows = answer.data;
            let key = |r: &Vec<Value>| r.last().map(crate::text).unwrap_or_default();
            // Decide the next request before handing this page over.
            let full = rows.len() >= page;
            let next = match (&cursor, full) {
                (Cursor::At(k, from), true) => Some(Cursor::At(k.clone(), from + rows.len())),
                (Cursor::At(k, _), false) => Some(Cursor::After(k.clone())),
                (_, false) => None,
                (_, true) => {
                    let last = key(rows.last().expect("full page"));
                    let keep = rows.iter().rposition(|r| key(r) != last).map_or(0, |i| i + 1);
                    if keep == 0 {
                        // One instant fills the page: it's read in pages of
                        // its own.
                        rows.clear();
                        Some(Cursor::At(last, 0))
                    } else {
                        rows.truncate(keep);
                        Some(Cursor::From(last))
                    }
                }
            };
            if let Some(n) = &next {
                pending = fetch(n);
            }
            {
                let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
                for mut r in rows {
                    r.truncate(names.len());
                    let row: Vec<Cell> = r.into_iter().enumerate().map(|(i, v)| to_cell(v, types.get(i).map_or("", String::as_str))).collect();
                    builder.push(row, &mut *s)?;
                }
            }
            match next {
                Some(n) => cursor = n,
                None => break,
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
        let kind = spec.table.kind.as_str();
        if !matches!(kind, kinds::TABLE | SUBTABLE | SUPERTABLE) {
            return Err(Error::Unsupported("en TDengine solo se cargan filas en tablas, subtablas y supertablas".into()));
        }
        self.cancel.flag.store(false, Ordering::SeqCst);
        let db = self.obj_db(&spec.table)?;
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let desc = self.describe(&db, &spec.table.name).await?;
        if desc.is_empty() {
            return Err(Error::Query(format!("No se encontró la tabla {}.", spec.table.name)));
        }
        if kind == SUPERTABLE && !names.iter().any(|n| n.eq_ignore_ascii_case("tbname")) {
            return Err(Error::Query("Para cargar filas en una supertabla hace falta la columna tbname (la subtabla de cada fila).".into()));
        }
        let precision = self.precision(&db).await?;
        let mut stamps = Vec::with_capacity(names.len());
        for n in &names {
            if n.eq_ignore_ascii_case("tbname") {
                stamps.push(None);
                continue;
            }
            let Some((c, _)) = desc.iter().find(|(c, _)| c.name.eq_ignore_ascii_case(n)) else {
                return Err(Error::Query(format!("La tabla {} no tiene la columna {n}.", spec.table.name)));
            };
            stamps.push((c.data_type == "TIMESTAMP").then_some(precision));
        }
        let head = format!(
            "INSERT INTO {} ({}) VALUES ",
            qualified(Some(&db), &spec.table.name),
            names.iter().map(|n| col(n)).collect::<Vec<_>>().join(", ")
        );
        let conn = self.utc();
        // Into a supertable, each statement carries the rows of one
        // subtable only: a statement spanning several subtables (vgroups)
        // isn't atomic in TDengine, and one that fails can leave part of
        // its rows committed, uncounted. One subtable's statement is
        // all-or-nothing, like a normal table's.
        let subtable = if kind == SUPERTABLE { names.iter().position(|n| n.eq_ignore_ascii_case("tbname")) } else { None };
        // Before anything is sent: a load that can't wait for its requests
        // when dropped isn't started.
        let mut inserts = Inserts::new(spec.commit_rows.max(1), progress)?;
        let cancel = self.cancel.clone();
        let filled = async {
            // The statement being filled for each subtable (one, keyed "",
            // into a table), and its rows.
            let mut open: HashMap<String, (String, u64)> = HashMap::new();
            let mut buffered = 0usize;
            let mut row_sql = String::new();
            loop {
                let Some(batch) = cancel.run(async { Ok::<_, Error>(source.next().await) }).await? else { break };
                for row in &batch.rows {
                    if row.len() != names.len() {
                        return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                    }
                    row_sql.clear();
                    row_sql.push('(');
                    for (i, c) in row.iter().enumerate() {
                        if i > 0 {
                            row_sql.push_str(", ");
                        }
                        row_sql.push_str(&literal(c, stamps[i])?);
                    }
                    row_sql.push(')');
                    let key = match subtable {
                        Some(i) => literal(&row[i], None)?,
                        None => String::new(),
                    };
                    let (stmt, rows) = open.entry(key).or_default();
                    // Full: sent before this row would take it past the
                    // statement limit.
                    if *rows > 0 && stmt.len() + row_sql.len() > MAX_SQL {
                        buffered -= stmt.len();
                        let full = (std::mem::take(stmt), std::mem::take(rows));
                        inserts.send_waiting(&cancel, &conn, &db, full).await?;
                    }
                    if *rows == 0 {
                        stmt.push_str(&head);
                        buffered += head.len();
                    }
                    stmt.push_str(&row_sql);
                    *rows += 1;
                    buffered += row_sql.len();
                    // Many subtables filling at once: all of them sent.
                    if buffered >= MAX_BUFFERED {
                        for (_, g) in open.drain() {
                            inserts.send_waiting(&cancel, &conn, &db, g).await?;
                        }
                        buffered = 0;
                    }
                }
                while let Some(r) = inserts.set.try_join_next() {
                    inserts.add(joined(Some(r))?);
                }
            }
            for (_, g) in open.drain() {
                inserts.send_waiting(&cancel, &conn, &db, g).await?;
            }
            while let Some(r) = cancel.run(async { Ok::<_, Error>(inserts.set.join_next().await) }).await? {
                inserts.add(joined(Some(r))?);
            }
            Ok(())
        }
        .await;
        // Whatever ended it (the end, an error, a cancel), the statements
        // the server already has are waited for, not cancelled (they'd
        // commit anyway), and counted.
        let failed = inserts.drain().await;
        filled?;
        match failed {
            Some(e) => Err(e),
            None => Ok(inserts.done),
        }
    }
}

/// A load's INSERT requests in flight, each one answering its row count.
/// TDengine commits every statement it received, even when the client
/// stops waiting, so these are never abandoned: the load waits for them
/// ([`Inserts::drain`]) before it returns, and dropping them half-way (the
/// orchestrator cancelling the table drops the load) waits for them too,
/// off the runtime's workers. So no row is committed after the load is
/// gone, and a table emptied after a cancel stays empty. The rows of the
/// statements the server confirmed are reported every `every` rows and at
/// the end (a drop included).
struct Inserts<'a> {
    set: JoinSet<Result<u64>>,
    done: u64,
    reported: u64,
    every: u64,
    progress: Progress<'a>,
}

impl<'a> Inserts<'a> {
    /// Waiting while dropped takes the multi-thread runtime (the app's and
    /// the driver host's); on another one, the load isn't started.
    fn new(every: u64, progress: Progress<'a>) -> Result<Inserts<'a>> {
        use tokio::runtime::{Handle, RuntimeFlavor};
        match Handle::try_current() {
            Ok(h) if h.runtime_flavor() == RuntimeFlavor::MultiThread => Ok(Inserts { set: JoinSet::new(), done: 0, reported: 0, every, progress }),
            _ => Err(Error::Unsupported(
                "la carga masiva en TDengine necesita el runtime multihilo: sin él no puede esperar a los INSERT en vuelo si se cancela, y el servidor los confirmaría después".into(),
            )),
        }
    }

    /// Rows of a statement the server confirmed.
    fn add(&mut self, rows: u64) {
        self.done += rows;
        if self.done - self.reported >= self.every {
            self.report();
        }
    }

    fn report(&mut self) {
        if self.done > self.reported {
            self.reported = self.done;
            (self.progress)(self.done);
        }
    }

    /// Sends a statement and its row count (nothing when it has no rows),
    /// once fewer than [`IN_FLIGHT`] are on their way.
    async fn send_waiting(&mut self, cancel: &Cancel, conn: &Arc<Conn>, db: &str, (stmt, rows): (String, u64)) -> Result<()> {
        if rows == 0 {
            return Ok(());
        }
        while self.set.len() >= IN_FLIGHT {
            let r = cancel.run(async { Ok::<_, Error>(self.set.join_next().await) }).await?;
            self.add(joined(r)?);
        }
        let (conn, db) = (conn.clone(), db.to_string());
        self.set.spawn(async move { exact_sql(&conn, &db, stmt, Some(INSERT_TIMEOUT)).await.map(|_| rows) });
        Ok(())
    }

    /// Waits for every request, counting what they committed, and reports;
    /// the first error, if any.
    async fn drain(&mut self) -> Option<Error> {
        let mut failed = None;
        while let Some(r) = self.set.join_next().await {
            match joined(Some(r)) {
                Ok(n) => self.done += n,
                Err(e) => {
                    failed.get_or_insert(e);
                }
            }
        }
        self.report();
        failed
    }
}

impl Drop for Inserts<'_> {
    fn drop(&mut self) {
        if self.set.is_empty() {
            return;
        }
        use tokio::runtime::{Handle, RuntimeFlavor};
        if let Ok(h) = Handle::try_current() {
            if h.runtime_flavor() == RuntimeFlavor::MultiThread {
                let mut rows = 0;
                let set = &mut self.set;
                tokio::task::block_in_place(|| {
                    h.block_on(async {
                        while let Some(r) = set.join_next().await {
                            rows += joined(Some(r)).unwrap_or(0);
                        }
                    })
                });
                self.done += rows;
                self.report();
            }
        }
    }
}

fn joined(r: Option<std::result::Result<Result<u64>, tokio::task::JoinError>>) -> Result<u64> {
    match r {
        None => Ok(0),
        Some(Ok(r)) => r,
        Some(Err(e)) => Err(Error::State(format!("envío de INSERT interrumpido: {e}"))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn rest_values_become_typed_cells() {
        assert_eq!(to_cell(json!("2023-11-14T22:13:20.123456Z"), "TIMESTAMP"), Cell::DateTime("2023-11-14 22:13:20.123456".into()));
        assert_eq!(to_cell(json!("2023-11-14T22:13:20.123Z"), "TIMESTAMP"), Cell::DateTime("2023-11-14 22:13:20.123".into()));
        assert_eq!(
            to_cell(json!("2023-11-14T19:13:20.123456789-03:00"), "TIMESTAMP"),
            Cell::DateTimeTz("2023-11-14 19:13:20.123456789-03:00".into())
        );
        assert_eq!(to_cell(json!(-5), "INT"), Cell::Int(-5));
        assert_eq!(to_cell(json!(18446744073709551614u64), "BIGINT UNSIGNED"), Cell::UInt(18446744073709551614));
        assert_eq!(to_cell(json!(7), "TINYINT UNSIGNED"), Cell::UInt(7));
        assert_eq!(to_cell(json!(1.5), "FLOAT"), Cell::Float(1.5));
        assert_eq!(to_cell(json!("90.33333333333333"), "DOUBLE"), Cell::Float(271.0 / 3.0));
        assert_eq!(to_cell(json!("1e+300"), "DOUBLE"), Cell::Float(1e300));
        assert_eq!(to_cell(json!(3), "DOUBLE"), Cell::Float(3.0));
        assert_eq!(to_cell(json!(true), "BOOL"), Cell::Bool(true));
        assert_eq!(to_cell(json!("ñu"), "NCHAR"), Cell::Text("ñu".into()));
        assert_eq!(to_cell(json!("cafe"), "VARBINARY"), Cell::Bytes(vec![0xCA, 0xFE]));
        assert_eq!(to_cell(json!({"k": 1}), "JSON"), Cell::Json("{\"k\":1}".into()));
        assert_eq!(to_cell(json!("12.340"), "DECIMAL(10,3)"), Cell::Decimal("12.340".into()));
        assert_eq!(to_cell(Value::Null, "INT"), Cell::Null);
    }

    #[test]
    fn floats_are_quoted_for_an_exact_parse() {
        let body = r#"{"code":0,"column_meta":[["d","DOUBLE",8]],"data":[[90.33333333333333,-1e+300,5,"a]\"1.5",{"k":2.5}]],"rows":1}"#;
        let q = quote_floats(body, 3);
        assert_eq!(q, r#"{"code":0,"column_meta":[["d","DOUBLE",8]],"data":[["90.33333333333333","-1e+300",5,"a]\"1.5",{"k":2.5}]],"rows":1}"#);
        let v: Value = serde_json::from_str(&q).unwrap();
        assert_eq!(to_cell(v["data"][0][0].clone(), "DOUBLE"), Cell::Float(271.0 / 3.0));
        assert_eq!(v["data"][0][4], json!({"k": 2.5}));
    }

    #[test]
    fn instants_parse_to_epoch() {
        assert_eq!(epoch_nanos("1970-01-01 00:00:00"), Some(0));
        assert_eq!(epoch_nanos("2023-11-14 22:13:20.123456"), Some(1_700_000_000_123_456_000));
        assert_eq!(epoch_nanos("2023-11-14T22:13:20.123456789Z"), Some(1_700_000_000_123_456_789));
        assert_eq!(epoch_nanos("2023-11-14 19:13:20.5-03:00"), Some(1_700_000_000_500_000_000));
        assert_eq!(epoch_nanos("2023-11-14"), Some(1_699_920_000_000_000_000));
        assert_eq!(epoch_nanos("1969-12-31 23:59:59.999"), Some(-1_000_000));
        assert_eq!(epoch_nanos("no"), None);
        assert_eq!(epoch_nanos("2023-13-01"), None);
    }

    #[test]
    fn literals_for_insert() {
        let ms = Some(Precision::Ms);
        let us = Some(Precision::Us);
        let l = |c: Cell, p| literal(&c, p).unwrap();
        assert_eq!(l(Cell::DateTime("2023-11-14 22:13:20.123456".into()), us), "1700000000123456");
        assert_eq!(l(Cell::DateTime("2023-11-14 22:13:20.123".into()), ms), "1700000000123");
        assert_eq!(l(Cell::DateTime("2023-11-14 22:13:20.123000000".into()), ms), "1700000000123");
        assert_eq!(l(Cell::DateTime("1969-12-31 23:59:59.999".into()), ms), "-1");
        assert_eq!(l(Cell::Int(1700000000123), ms), "1700000000123");
        assert_eq!(l(Cell::Text("O'Brien \\".into()), None), "'O''Brien \\\\'");
        assert_eq!(l(Cell::Bytes(vec![0xCA, 0xFE]), None), "'\\xCAFE'");
        assert_eq!(l(Cell::Json("{\"k\":1}".into()), None), "'{\"k\":1}'");
        assert_eq!(l(Cell::Float(0.1), None), "0.1");
        assert_eq!(l(Cell::UInt(u64::MAX), None), "18446744073709551615");
        assert_eq!(l(Cell::Null, ms), "NULL");
    }

    #[test]
    fn finer_instants_are_refused_not_cut() {
        // 1 µs apart into a 'ms' database would collapse onto one key.
        for (s, p) in [
            ("2023-11-14 22:13:20.123456", Precision::Ms),
            ("1969-12-31 23:59:59.9995", Precision::Ms),
            ("2023-11-14 22:13:20.123456789", Precision::Us),
            ("2023-11-14 22:13:20.1234567891", Precision::Ns),
        ] {
            assert!(matches!(literal(&Cell::DateTime(s.into()), Some(p)), Err(Error::Unsupported(_))), "{s}");
        }
        assert_eq!(literal(&Cell::DateTime("2023-11-14 22:13:20.1234567890".into()), Some(Precision::Ns)).unwrap(), "1700000000123456789");
    }

    #[test]
    fn nan_and_infinities_are_refused_not_nulled() {
        for f in [f64::NAN, f64::INFINITY, f64::NEG_INFINITY] {
            assert!(matches!(literal(&Cell::Float(f), None), Err(Error::Unsupported(_))), "{f}");
        }
    }

    #[test]
    fn float_columns_round_trip_exactly() {
        // REST prints FLOAT as the f32's shortest text.
        let max = to_cell(json!("3.4028235e+38"), "FLOAT");
        assert_eq!(max, Cell::Float(f64::from(f32::MAX)));
        assert_eq!(to_cell(json!("-3.4028235e+38"), "FLOAT"), Cell::Float(f64::from(f32::MIN)));
        assert_eq!(to_cell(json!("1e-45"), "FLOAT"), Cell::Float(f64::from(f32::from_bits(1))));
        assert_eq!(to_cell(json!("0.1"), "FLOAT"), Cell::Float(f64::from(0.1f32)));
        assert_eq!(to_cell(json!(3), "FLOAT"), Cell::Float(3.0));
        // Written back, it's FLT_MAX itself, not the f32's text read as a
        // double (above FLT_MAX: TDengine's 'illegal float data').
        let text = literal(&max, None).unwrap();
        assert_eq!(text.parse::<f64>().unwrap(), f64::from(f32::MAX));
        assert!(text.parse::<f64>().unwrap() <= f64::from(f32::MAX));
        assert_eq!(to_cell(json!("x"), "FLOAT"), Cell::Text("x".into()));
    }

    #[test]
    fn pages_are_bounded_by_bytes() {
        let narrow: Vec<String> = ["TIMESTAMP", "INT", "DOUBLE", "VARCHAR(20)", "TIMESTAMP"].iter().map(|s| s.to_string()).collect();
        assert!((10_000..=PAGE).contains(&page_rows(&narrow)));
        assert_eq!(page_rows(&["TIMESTAMP".to_string()]), PAGE);
        let wide: Vec<String> = ["TIMESTAMP", "VARCHAR(60000)", "TIMESTAMP"].iter().map(|s| s.to_string()).collect();
        let n = page_rows(&wide);
        // Every byte a control character: `\u0001` is 6 bytes of JSON.
        assert!(n * 60_000 * 6 <= PAGE_BYTES && n >= 1, "{n}");
        assert_eq!(page_rows(&vec!["VARBINARY(65517)".to_string(); 64]), 1);
        assert_eq!(value_width("NCHAR(10) TAG"), 32 + 60);
        assert_eq!(value_width("VARCHAR(100)"), 32 + 600);
    }

    #[test]
    fn value_width_covers_escaped_text() {
        // As REST answers it: control characters escaped, the rest raw.
        for (text, ty) in [("\u{1}".repeat(100), "VARCHAR(100)"), ("😀\u{1}".repeat(5), "NCHAR(10)"), ("\u{1f}".repeat(10), "NCHAR(10)")] {
            let json = serde_json::to_string(&Value::String(text)).unwrap();
            assert!(json.len() <= value_width(ty), "{ty}: {} > {}", json.len(), value_width(ty));
        }
    }

    #[test]
    fn column_names() {
        assert_eq!(col("TBNAME"), "tbname");
        assert_eq!(col("v"), "`v`");
    }
}
