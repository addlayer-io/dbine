//! Bulk transfer (see `dbine_driver::transfer`) for Dremio, over the same
//! REST API v3 as the rest of the driver.
//!
//! Reading: one job (`SELECT <columns> FROM t [WHERE filter]`) and its
//! results paged by the API ([`PAGE`] rows per request, its maximum),
//! [`IN_FLIGHT`] pages fetched at once and handed over in order. DECIMAL,
//! DOUBLE and FLOAT columns are read as text (`CAST(… AS VARCHAR)`) and
//! typed back here: the JSON answer writes decimals through a double, and
//! a JSON number isn't parsed back bit for bit. Decimal text may come in
//! scientific notation (Java's `BigDecimal.toString`: `1E-10`), so it is
//! rewritten as plain digits with the column's scale. FLOAT goes through
//! DOUBLE first (`CAST(CAST(c AS DOUBLE) AS VARCHAR)`): the widening is
//! exact and the double's text parses back to the stored value, while a
//! float's own shortest text (`0.1`) is a different double. Values are
//! typed by the answer's schema:
//! integers, FLOAT/DOUBLE (also infinities and NaN, which come as text),
//! BOOLEAN, VARCHAR, DATE, TIME, TIMESTAMP (Dremio keeps milliseconds),
//! VARBINARY (base64 in the answer) as bytes, STRUCT/LIST/MAP as JSON.
//!
//! Loading: Dremio writes only to tables that take DML (Iceberg: `$scratch`,
//! filesystem and object storage sources, catalogs); other targets answer
//! the server's error. Each statement is
//! `INSERT INTO t (cols) SELECT CAST(c0 AS <type>), … FROM (VALUES (…), …) AS v(c0, …)`
//! with up to [`STMT_ROWS`] rows or [`STMT_BYTES`] bytes, one after the
//! other (each one is an Iceberg commit; concurrent commits on a table
//! conflict). Every value travels as text and is cast to the target
//! column's type: a VALUES list inserted directly goes through a double
//! (DECIMALs lose digits) and doesn't take binary literals; through the
//! SELECT, decimals stay exact and binaries go as `FROM_HEX`. FLOAT/DOUBLE
//! columns are the exception: they take double literals, which the planner
//! parses exactly (a run-time cast from text can miss the last digit).
//! STRUCT/LIST/MAP columns aren't loaded (`Unsupported`, see
//! [`is_complex`]). Into text columns each value is tagged: `p` and the text
//! when it's all Latin-1, `h` and its UTF-8 bytes in hex otherwise (the
//! planner rejects string literals with other characters: `AssertionError:
//! to VARCHAR(n) from _UTF-8'…'`), and the SELECT decodes it.
//!
//! Each INSERT job runs in its own task and is followed to its end, even
//! when the load is dropped (the run was cancelled, or the read failed):
//! the job is cancelled and the drop waits for its final state, so no
//! rows get committed after `bulk_load` returned.

use crate::ddl::{lit, path, q};
use crate::{text, Cancel, Conn, DremioSession};
use base64::Engine as _;
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result, Session};
use serde_json::Value;
use std::collections::VecDeque;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::task::JoinHandle;

/// Rows per results request (the API's maximum).
const PAGE: usize = 500;
/// Results requests in flight at once.
const IN_FLIGHT: usize = 8;
/// Rows of one INSERT at most…
const STMT_ROWS: usize = 5_000;
/// …or this many bytes of SQL.
const STMT_BYTES: usize = 2 * 1024 * 1024;
/// How long an INSERT job is followed after asking for its cancel, and
/// how long a dropped load waits for it.
const SETTLE: Duration = Duration::from_secs(120);
/// Status polls that may fail in a row before the job is cancelled.
const POLL_TRIES: u32 = 5;

/// The scale of a `DECIMAL(p,s)` type name.
fn scale_of(ty: &str) -> Option<usize> {
    let inner = ty.split_once('(')?.1.strip_suffix(')')?;
    inner.split_once(',')?.1.trim().parse().ok()
}

/// A decimal's text as plain digits (Java's `BigDecimal.toString` uses an
/// exponent for small scales: `1E-10`, `-1.000E-7`, `0E-10`), with its
/// fraction padded to `scale`. Text that isn't a number stays as it is.
pub(crate) fn plain_decimal(s: &str, scale: Option<usize>) -> String {
    let t = s.trim();
    let (m, exp) = match t.find(['E', 'e']) {
        Some(i) => match t[i + 1..].trim_start_matches('+').parse::<i64>() {
            Ok(e) => (&t[..i], e),
            Err(_) => return s.to_string(),
        },
        None => (t, 0),
    };
    let (neg, m) = match m.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, m.strip_prefix('+').unwrap_or(m)),
    };
    let (ip, fp) = m.split_once('.').unwrap_or((m, ""));
    let digits = format!("{ip}{fp}");
    if digits.is_empty() || !digits.bytes().all(|b| b.is_ascii_digit()) {
        return s.to_string();
    }
    // The point sits `point` digits into `digits`.
    let point = ip.len() as i64 + exp;
    let (int, mut frac) = if point <= 0 {
        ("0".to_string(), format!("{}{digits}", "0".repeat(point.unsigned_abs() as usize)))
    } else if point as usize >= digits.len() {
        (format!("{digits}{}", "0".repeat(point as usize - digits.len())), String::new())
    } else {
        (digits[..point as usize].to_string(), digits[point as usize..].to_string())
    };
    let int = match int.trim_start_matches('0') {
        "" => "0",
        i => i,
    };
    if let Some(sc) = scale {
        while frac.len() < sc {
            frac.push('0');
        }
    }
    let zero = int == "0" && frac.bytes().all(|b| b == b'0');
    let mut out = String::with_capacity(int.len() + frac.len() + 2);
    if neg && !zero {
        out.push('-');
    }
    out.push_str(int);
    if !frac.is_empty() {
        out.push('.');
        out.push_str(&frac);
    }
    out
}

/// A value of the answer as a cell, given its column's type (the answer's
/// schema; DECIMAL, DOUBLE and FLOAT columns come as text).
pub(crate) fn to_cell(v: Value, ty: &str) -> Cell {
    match v {
        Value::Null => Cell::Null,
        Value::String(s) if ty.starts_with("DECIMAL") => Cell::Decimal(plain_decimal(&s, scale_of(ty))),
        Value::Bool(b) => Cell::Bool(b),
        Value::Number(n) if ty == "FLOAT" || ty == "DOUBLE" => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        Value::Number(n) if ty.starts_with("DECIMAL") => Cell::Decimal(plain_decimal(&n.to_string(), scale_of(ty))),
        Value::Number(n) => match (n.as_i64(), n.as_u64()) {
            (Some(i), _) => Cell::Int(i),
            (None, Some(u)) => Cell::UInt(u),
            _ => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
        },
        Value::String(s) => match ty {
            "FLOAT" | "DOUBLE" => Cell::Float(match s.as_str() {
                "Infinity" => f64::INFINITY,
                "-Infinity" => f64::NEG_INFINITY,
                other => other.parse().unwrap_or(f64::NAN),
            }),
            "VARBINARY" => match base64::engine::general_purpose::STANDARD.decode(&s) {
                Ok(b) => Cell::Bytes(b),
                Err(_) => Cell::Text(s),
            },
            "DATE" => Cell::Date(s),
            "TIME" => Cell::Time(s),
            "TIMESTAMP" => Cell::DateTime(s.replacen('T', " ", 1)),
            _ => Cell::Text(s),
        },
        other => Cell::Json(other.to_string()),
    }
}

/// Days since 1970-01-01 of a civil date.
fn days_from_civil(y: i64, m: i64, d: i64) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y - era * 400;
    let doy = (153 * ((m + 9) % 12) + 2) / 5 + d - 1;
    era * 146_097 + yoe * 365 + yoe / 4 - yoe / 100 + doy - 719_468
}

/// The civil date of a day count since 1970-01-01.
fn civil_from_days(z: i64) -> (i64, i64, i64) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    (yoe + era * 400 + i64::from(m <= 2), m, d)
}

/// `YYYY-MM-DD HH:MM:SS[.f]±HH:MM` as the UTC wall time (Dremio's
/// TIMESTAMP has no zone); `None` when it doesn't parse.
pub(crate) fn to_utc(s: &str) -> Option<String> {
    let s = s.trim().replacen('T', " ", 1);
    let (local, zone) = match s.strip_suffix('Z') {
        Some(l) => (l.to_string(), 0i64),
        None => {
            let i = s.rfind(['+', '-']).filter(|&i| i > 10)?;
            let z = s[i + 1..].replace(':', "");
            let (h, m): (i64, i64) = (z.get(..2)?.parse().ok()?, if z.len() > 2 { z[2..].parse().ok()? } else { 0 });
            let sign = if &s[i..i + 1] == "-" { -1 } else { 1 };
            (s[..i].trim().to_string(), sign * (h * 60 + m))
        }
    };
    let (date, time) = local.split_once(' ')?;
    let mut dp = date.splitn(3, '-').map(|x| x.parse::<i64>().ok());
    let (y, mo, d) = (dp.next()??, dp.next()??, dp.next()??);
    let (hms, frac) = time.split_once('.').map_or((time, None), |(a, b)| (a, Some(b)));
    let mut tp = hms.splitn(3, ':').map(|x| x.parse::<i64>().ok());
    let (h, mi, sec) = (tp.next()??, tp.next()??, tp.next().flatten().unwrap_or(0));
    let minutes = days_from_civil(y, mo, d) * 1440 + h * 60 + mi - zone;
    let (days, mins) = (minutes.div_euclid(1440), minutes.rem_euclid(1440));
    let (y, mo, d) = civil_from_days(days);
    let mut out = format!("{y:04}-{mo:02}-{d:02} {:02}:{:02}:{sec:02}", mins / 60, mins % 60);
    if let Some(f) = frac {
        out.push('.');
        out.push_str(f);
    }
    Some(out)
}

fn hex(b: &[u8]) -> String {
    b.iter().map(|x| format!("{x:02X}")).collect()
}

/// How a target column takes its VALUES cells.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Kind {
    /// Hexadecimal text, `FROM_HEX` in the SELECT.
    Binary,
    /// A double literal.
    Float,
    /// Tagged text (see [`value_sql`]), decoded in the SELECT.
    Text,
    /// Text the SELECT casts.
    Other,
}

/// The kind of a column given its INFORMATION_SCHEMA `DATA_TYPE`.
pub(crate) fn kind_of(data_type: &str) -> Kind {
    match data_type.trim().to_ascii_uppercase().as_str() {
        "BINARY VARYING" | "VARBINARY" | "BINARY" => Kind::Binary,
        "DOUBLE" | "FLOAT" | "REAL" => Kind::Float,
        "CHARACTER VARYING" | "VARCHAR" | "CHAR" | "CHARACTER" => Kind::Text,
        _ => Kind::Other,
    }
}

/// A cell in the VALUES list for a column of `kind`: text for the SELECT
/// to cast, except into FLOAT/DOUBLE columns, which take a double literal
/// (the planner parses it exactly; casting text at run time can be off in
/// the last digit). Text columns take a tag and the value: `p` and the
/// text as a literal when every character is Latin-1, `h` and its UTF-8
/// bytes in hex otherwise: the planner rejects a string literal with any
/// other character (`AssertionError: to VARCHAR(n) from _UTF-8'…'`), and
/// the two forms can't be mixed in one VALUES column (different charsets).
pub(crate) fn value_sql(c: &Cell, kind: Kind) -> String {
    if kind == Kind::Float {
        let f = match c {
            Cell::Float(f) => Some(*f),
            Cell::Int(i) => Some(*i as f64),
            Cell::UInt(u) => Some(*u as f64),
            Cell::Bool(b) => Some(if *b { 1.0 } else { 0.0 }),
            Cell::Decimal(s) | Cell::Text(s) => s.trim().parse().ok(),
            _ => None,
        };
        match f {
            // The planner folds any constant -0.0 (`-0e0`, `CAST('-0.0' AS
            // DOUBLE)`…) to 0.0; a run-time negation keeps the sign.
            Some(f) if f == 0.0 && f.is_sign_negative() => return "-(RAND() * 0e0)".into(),
            Some(f) if f.is_finite() => return format!("{f:e}"),
            Some(f) => return format!("CAST({} AS DOUBLE)", lit(&value_text(&Cell::Float(f), false).unwrap_or_default())),
            None => {}
        }
    }
    match value_text(c, kind == Kind::Binary) {
        Some(t) if kind == Kind::Text && t.chars().all(|ch| u32::from(ch) <= 0xFF) => lit(&format!("p{t}")),
        Some(t) if kind == Kind::Text => format!("'h{}'", hex(t.as_bytes())),
        Some(t) => lit(&t),
        None => "NULL".into(),
    }
}

/// A cell as the VALUES text the target column's expression casts;
/// `binary` targets take hexadecimal.
pub(crate) fn value_text(c: &Cell, binary: bool) -> Option<String> {
    let s = match c {
        Cell::Null => return None,
        Cell::Bytes(b) => return Some(hex(b)),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) if f.is_nan() => "NaN".into(),
        Cell::Float(f) if f.is_infinite() => if *f > 0.0 { "Infinity" } else { "-Infinity" }.into(),
        Cell::Float(f) => format!("{f:e}"),
        Cell::DateTime(s) => s.replacen('T', " ", 1),
        Cell::DateTimeTz(s) => to_utc(s).unwrap_or_else(|| s.clone()),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::Uuid(s) | Cell::Json(s) => s.clone(),
    };
    Some(if binary { hex(s.as_bytes()) } else { s })
}

/// A STRUCT/LIST/MAP column (INFORMATION_SCHEMA says ROW/ARRAY/MAP).
/// Dremio's `CONVERT_FROM(…, 'JSON')` only takes literals and table
/// fields, not a VALUES column, and types the value by its JSON (numbers
/// as BIGINT, a table's INT field then mismatches), so loading into one
/// is `Unsupported`.
pub(crate) fn is_complex(data_type: &str) -> bool {
    matches!(data_type.trim().to_ascii_uppercase().as_str(), "STRUCT" | "LIST" | "MAP" | "ARRAY" | "ROW")
}

/// The SELECT expression that turns VALUES column `c` into the target
/// column's type (INFORMATION_SCHEMA's DATA_TYPE, with DECIMAL's precision).
pub(crate) fn cast_expr(c: &str, data_type: &str) -> String {
    let t = data_type.trim().to_ascii_uppercase();
    // Tagged text (see `value_sql`) back to the value. The column goes
    // through CAST first (here and for FROM_HEX): when every value of it
    // in one statement is NULL, Calcite types it NULL and Dremio rejects
    // a function on it ("does not support casting or coercing null to
    // varchar"), while an explicit CAST of that NULL is accepted. FROM_HEX
    // turns NULL into empty bytes, hence its CASE.
    let v = format!("CAST({c} AS VARCHAR)");
    let text = format!("CASE WHEN SUBSTR({v}, 1, 1) = 'h' THEN CONVERT_FROM(FROM_HEX(SUBSTR({v}, 2)), 'UTF8') ELSE SUBSTR({v}, 2) END");
    match t.as_str() {
        "CHARACTER VARYING" | "VARCHAR" | "CHAR" | "CHARACTER" => text,
        "BINARY VARYING" | "VARBINARY" | "BINARY" => format!("CASE WHEN {v} IS NULL THEN NULL ELSE FROM_HEX({v}) END"),
        "" => c.to_string(),
        _ => format!("CAST({c} AS {t})"),
    }
}

impl DremioSession {
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        self.cancel.flag.store(false, Ordering::SeqCst);
        let schema = self.schema_of(&spec.table)?;
        let described = self.columns(&spec.table).await?;
        let names: Vec<String> = match &spec.columns {
            Some(c) => c.clone(),
            None => described.iter().map(|c| c.name.clone()).collect(),
        };
        if names.is_empty() {
            return Err(Error::Query(format!("No se encontraron las columnas de {}.", spec.table.name)));
        }
        let info = |n: &str| described.iter().find(|c| c.name.eq_ignore_ascii_case(n));
        // Read as text (and typed back here): DECIMAL, which the JSON
        // answer writes through a double, and DOUBLE/FLOAT, whose JSON
        // numbers the client parses without an exact round trip.
        let textual: Vec<Option<&str>> = names
            .iter()
            .map(|n| match info(n).map(|c| c.data_type.as_str()) {
                // With its precision and scale (the text is padded to it).
                Some(t) if t.starts_with("DECIMAL") => Some(t),
                Some("DOUBLE") => Some("DOUBLE"),
                Some("FLOAT") => Some("FLOAT"),
                _ => None,
            })
            .collect();
        let cols: Vec<TransferColumn> = names
            .iter()
            .map(|n| TransferColumn {
                name: n.clone(),
                type_name: info(n).map(|c| c.data_type.clone()).unwrap_or_default(),
                nullable: info(n).is_none_or(|c| c.nullable),
            })
            .collect();
        let list: Vec<String> = names
            .iter()
            .zip(&textual)
            .map(|(n, t)| match t {
                // Widened first: a float's own text is a different double.
                Some("FLOAT") => format!("CAST(CAST({} AS DOUBLE) AS VARCHAR) AS {}", q(n), q(n)),
                Some(_) => format!("CAST({} AS VARCHAR) AS {}", q(n), q(n)),
                None => q(n),
            })
            .collect();
        let mut sql = format!("SELECT {} FROM {}", list.join(", "), path(Some(&schema), &spec.table.name));
        if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        let (id, st) = self.job(&sql).await?;
        let total = st.get("rowCount").and_then(Value::as_u64).unwrap_or(0) as usize;

        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut builder = BatchBuilder::new();
        let mut pending: VecDeque<JoinHandle<Result<Value>>> = VecDeque::new();
        let mut next = 0usize;
        let mut types: Option<Vec<String>> = None;
        loop {
            while pending.len() < IN_FLIGHT && next < total {
                let conn = self.conn.clone();
                let p = format!("/api/v3/job/{id}/results?offset={next}&limit={PAGE}");
                pending.push_back(tokio::spawn(async move { conn.send(reqwest::Method::GET, &p, None).await }));
                next += PAGE;
            }
            let Some(h) = pending.pop_front() else { break };
            let page = match self.cancel.run(async { h.await.map_err(|e| Error::State(format!("lectura interrumpida: {e}")))? }).await {
                Ok(p) => p,
                Err(e) => {
                    pending.iter().for_each(JoinHandle::abort);
                    return Err(e);
                }
            };
            let types = types.get_or_insert_with(|| {
                let schema: Vec<(String, String)> = page
                    .get("schema")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|c| (c.get("name").map(text).unwrap_or_default(), c.get("type").and_then(|t| t.get("name")).map(text).unwrap_or_default()))
                    .collect();
                names
                    .iter()
                    .zip(&textual)
                    .map(|(n, t)| match t {
                        Some(t) => t.to_string(),
                        None => schema.iter().find(|(s, _)| s == n).map(|(_, t)| t.clone()).unwrap_or_default(),
                    })
                    .collect()
            });
            let rows = match page.get("rows") {
                Some(Value::Array(r)) => r.clone(),
                _ => Vec::new(),
            };
            let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
            for mut r in rows {
                let row: Vec<Cell> = names
                    .iter()
                    .enumerate()
                    .map(|(i, n)| to_cell(r.get_mut(n).map(Value::take).unwrap_or(Value::Null), &types[i]))
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
        self.cancel.flag.store(false, Ordering::SeqCst);
        let schema = self.schema_of(&spec.table)?;
        let names: Vec<String> =
            if spec.columns.is_empty() { columns.iter().map(|c| c.name.clone()).collect() } else { spec.columns.clone() };
        let described = self.columns(&spec.table).await?;
        if described.is_empty() {
            return Err(Error::Query(format!("No se encontró la tabla {}.", spec.table.name)));
        }
        let mut exprs = Vec::with_capacity(names.len());
        let mut kinds = Vec::with_capacity(names.len());
        for (i, n) in names.iter().enumerate() {
            let Some(c) = described.iter().find(|c| c.name.eq_ignore_ascii_case(n)) else {
                return Err(Error::Query(format!("La tabla {} no tiene la columna {n}.", spec.table.name)));
            };
            if is_complex(&c.data_type) {
                return Err(Error::Unsupported(format!(
                    "Dremio no carga en bloque la columna {n} ({}): no convierte a ARRAY, ROW o MAP un valor de una lista VALUES, solo un literal, y lo tipa por el JSON (los números como BIGINT), así que el valor no llegaría fiel",
                    c.data_type
                )));
            }
            exprs.push(cast_expr(&format!("c{i}"), &c.data_type));
            kinds.push(kind_of(&c.data_type));
        }
        let head = format!(
            "INSERT INTO {} ({}) SELECT {} FROM (VALUES ",
            path(Some(&schema), &spec.table.name),
            names.iter().map(|n| q(n)).collect::<Vec<_>>().join(", "),
            exprs.join(", ")
        );
        let tail = format!(") AS v({})", (0..names.len()).map(|i| format!("c{i}")).collect::<Vec<_>>().join(", "));

        let every = spec.commit_rows.max(1);
        let (mut done, mut reported) = (0u64, 0u64);
        let mut stmt = String::new();
        let mut in_stmt = 0usize;
        loop {
            let batch = source.next().await;
            let finished = batch.is_none();
            for row in batch.map(|b| b.rows).unwrap_or_default() {
                if row.len() != names.len() {
                    if done > reported {
                        progress(done);
                    }
                    return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), names.len())));
                }
                stmt.push_str(if in_stmt == 0 { &head } else { ", " });
                stmt.push('(');
                for (i, c) in row.iter().enumerate() {
                    if i > 0 {
                        stmt.push_str(", ");
                    }
                    stmt.push_str(&value_sql(c, kinds[i]));
                }
                stmt.push(')');
                in_stmt += 1;
                if in_stmt >= STMT_ROWS || stmt.len() >= STMT_BYTES {
                    let committed = self.insert(&mut stmt, &tail).await;
                    if committed.is_ok() {
                        done += in_stmt as u64;
                    }
                    in_stmt = 0;
                    let r = committed.and_then(|()| self.stopped());
                    // Committed rows are reported before any error.
                    if done - reported >= every || (r.is_err() && done > reported) {
                        reported = done;
                        progress(done);
                    }
                    r?;
                }
            }
            if finished {
                break;
            }
        }
        let mut r = Ok(());
        if in_stmt > 0 {
            r = self.insert(&mut stmt, &tail).await;
            if r.is_ok() {
                done += in_stmt as u64;
            }
        }
        if done > reported {
            progress(done);
        }
        r.map(|()| done)
    }

    /// `Cancelled` once the session's interrupter fired.
    fn stopped(&self) -> Result<()> {
        if self.cancel.flag.load(Ordering::SeqCst) {
            Err(Error::Cancelled)
        } else {
            Ok(())
        }
    }

    /// Run one INSERT (`stmt` + `tail`) and empty `stmt`. `Ok` only once
    /// the job committed; it is followed to its end in its own task, also
    /// when this future is dropped (see [`InsertJob`]).
    async fn insert(&self, stmt: &mut String, tail: &str) -> Result<()> {
        // Interrupted between statements: nothing more is submitted.
        self.stopped()?;
        stmt.push_str(tail);
        let body = self.job_body(stmt);
        stmt.clear();
        let abandon = Arc::new(AtomicBool::new(false));
        let task = self.rt.spawn(run_insert(self.conn.clone(), body, self.cancel.clone(), abandon.clone()));
        let mut job = InsertJob { task: Some(task), abandon };
        let r = match job.task.as_mut() {
            Some(t) => t.await,
            None => return Err(Error::State("carga interrumpida".into())),
        };
        job.task = None;
        r.map_err(|e| Error::State(format!("carga interrumpida: {e}")))?
    }
}

/// An INSERT job's task. Dropped before it ended (the load was dropped),
/// it asks for the job's cancel and waits for its final state, so its rows
/// can't be committed after the load returned.
struct InsertJob {
    task: Option<JoinHandle<Result<()>>>,
    abandon: Arc<AtomicBool>,
}

impl Drop for InsertJob {
    fn drop(&mut self) {
        let Some(task) = self.task.take() else { return };
        self.abandon.store(true, Ordering::SeqCst);
        let wait = async move {
            if tokio::time::timeout(SETTLE + Duration::from_secs(10), task).await.is_err() {
                tracing::warn!("dremio: an INSERT job didn't end after its load was dropped");
            }
        };
        match tokio::runtime::Handle::try_current() {
            Ok(h) if h.runtime_flavor() == tokio::runtime::RuntimeFlavor::MultiThread => {
                tokio::task::block_in_place(|| h.block_on(wait));
            }
            // A single-threaded runtime can't wait here: the task goes on,
            // cancels the job and follows it to its end.
            _ => {}
        }
    }
}

/// Submit an INSERT and follow it to its final state: `Ok` when it
/// committed. The session's interrupter or an abandoned load (`abandon`)
/// cancel it, and it's still followed until it ends; the job id is
/// published for the interrupter only after the submit answered, so the
/// flag is checked here too.
async fn run_insert(conn: Arc<Conn>, body: Value, cancel: Arc<Cancel>, abandon: Arc<AtomicBool>) -> Result<()> {
    let submitted = conn.send(reqwest::Method::POST, "/api/v3/sql", Some(&body)).await?;
    drop(body);
    let id = submitted.get("id").map(text).ok_or_else(|| Error::Query("Dremio no devolvió el id del trabajo.".into()))?;
    *cancel.job.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.clone());
    let r = follow(&conn, &id, &cancel, &abandon).await;
    let mut slot = cancel.job.lock().unwrap_or_else(|e| e.into_inner());
    if slot.as_deref() == Some(id.as_str()) {
        *slot = None;
    }
    r
}

async fn follow(conn: &Conn, id: &str, cancel: &Cancel, abandon: &AtomicBool) -> Result<()> {
    let mut wait = 50u64;
    let mut failed = 0u32;
    // Why the job was cancelled by us, when it was a failed status poll.
    let mut poll_error: Option<Error> = None;
    let mut asked: Option<Instant> = None;
    loop {
        let stop = cancel.flag.load(Ordering::SeqCst) || abandon.load(Ordering::SeqCst);
        if asked.is_none() && (stop || poll_error.is_some()) {
            if let Err(e) = conn.send(reqwest::Method::POST, &format!("/api/v3/job/{id}/cancel"), None).await {
                tracing::debug!("dremio cancel of {id} failed: {e}");
            }
            asked = Some(Instant::now());
        }
        if asked.is_some_and(|t| t.elapsed() > SETTLE) {
            return Err(poll_error.unwrap_or_else(|| {
                Error::Query(format!("Dremio no confirmó el fin del trabajo {id} después de cancelarlo; revisá la tabla de destino."))
            }));
        }
        match conn.send(reqwest::Method::GET, &format!("/api/v3/job/{id}"), None).await {
            Ok(st) => {
                failed = 0;
                match st.get("jobState").and_then(Value::as_str).unwrap_or("") {
                    "COMPLETED" => return Ok(()),
                    "FAILED" => return Err(Error::Query(st.get("errorMessage").map(text).unwrap_or_else(|| "El trabajo falló.".into()))),
                    "CANCELED" | "CANCELLED" => {
                        return Err(match poll_error {
                            Some(e) => Error::Query(format!("no se pudo seguir el estado del trabajo y se canceló: {e}")),
                            None => Error::Cancelled,
                        })
                    }
                    _ => {}
                }
            }
            Err(e) => {
                failed += 1;
                tracing::debug!("dremio status poll of {id} failed ({failed}): {e}");
                if failed >= POLL_TRIES && poll_error.is_none() {
                    poll_error = Some(e);
                }
            }
        }
        tokio::time::sleep(Duration::from_millis(wait)).await;
        wait = (wait * 2).min(500);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn answers_become_typed_cells() {
        assert_eq!(to_cell(json!(9007199254740993i64), "BIGINT"), Cell::Int(9007199254740993));
        assert_eq!(to_cell(json!(1.5), "DOUBLE"), Cell::Float(1.5));
        assert_eq!(to_cell(json!(2), "FLOAT"), Cell::Float(2.0));
        assert_eq!(to_cell(json!("Infinity"), "FLOAT"), Cell::Float(f64::INFINITY));
        // DOUBLE read as text keeps every bit (a JSON number may not).
        assert_eq!(to_cell(json!("90.33333333333333"), "DOUBLE"), Cell::Float(271.0 / 3.0));
        assert_eq!(to_cell(json!("1.0E300"), "DOUBLE"), Cell::Float(1e300));
        assert!(matches!(to_cell(json!("NaN"), "DOUBLE"), Cell::Float(f) if f.is_nan()));
        assert_eq!(to_cell(json!("12345678901234.5678"), "DECIMAL"), Cell::Decimal("12345678901234.5678".into()));
        assert_eq!(to_cell(json!("yv4A"), "VARBINARY"), Cell::Bytes(vec![0xCA, 0xFE, 0]));
        assert_eq!(to_cell(json!("2024-01-31"), "DATE"), Cell::Date("2024-01-31".into()));
        assert_eq!(to_cell(json!("13:45:00.500"), "TIME"), Cell::Time("13:45:00.500".into()));
        assert_eq!(to_cell(json!("2024-01-31 13:45:00.123"), "TIMESTAMP"), Cell::DateTime("2024-01-31 13:45:00.123".into()));
        assert_eq!(to_cell(json!(true), "BOOLEAN"), Cell::Bool(true));
        assert_eq!(to_cell(json!({"a": [1]}), "STRUCT"), Cell::Json("{\"a\":[1]}".into()));
        assert_eq!(to_cell(Value::Null, "INTEGER"), Cell::Null);
    }

    #[test]
    fn zoned_times_go_as_utc() {
        assert_eq!(to_utc("2024-01-31 22:30:00.5-03:00").as_deref(), Some("2024-02-01 01:30:00.5"));
        assert_eq!(to_utc("2024-03-01 00:10:00+01:00").as_deref(), Some("2024-02-29 23:10:00"));
        assert_eq!(to_utc("2023-12-31T23:00:00Z").as_deref(), Some("2023-12-31 23:00:00"));
        assert_eq!(to_utc("sin zona"), None);
        assert_eq!(civil_from_days(days_from_civil(1969, 12, 31)), (1969, 12, 31));
    }

    #[test]
    fn values_and_casts() {
        assert_eq!(value_text(&Cell::Float(1.5), false).as_deref(), Some("1.5e0"));
        assert_eq!(value_text(&Cell::Float(1e300), false).as_deref(), Some("1e300"));
        assert_eq!(value_text(&Cell::Float(f64::NEG_INFINITY), false).as_deref(), Some("-Infinity"));
        assert_eq!(value_text(&Cell::Bytes(vec![0xCA, 0xFE]), true).as_deref(), Some("CAFE"));
        assert_eq!(value_text(&Cell::Text("ab".into()), true).as_deref(), Some("6162"));
        assert_eq!(value_text(&Cell::DateTimeTz("2024-01-31 22:30:00-03:00".into()), false).as_deref(), Some("2024-02-01 01:30:00"));
        assert_eq!(value_text(&Cell::Null, false), None);
        assert_eq!(value_sql(&Cell::Float(90.33333333333333), Kind::Float), "9.033333333333333e1");
        assert_eq!(value_sql(&Cell::Int(3), Kind::Float), "3e0");
        assert_eq!(value_sql(&Cell::Float(f64::INFINITY), Kind::Float), "CAST('Infinity' AS DOUBLE)");
        assert_eq!(value_sql(&Cell::Null, Kind::Float), "NULL");
        assert_eq!(value_sql(&Cell::Text("O'B".into()), Kind::Other), "'O''B'");
        assert_eq!(cast_expr("c0", "DECIMAL(20,4)"), "CAST(c0 AS DECIMAL(20,4))");
        assert_eq!(
            cast_expr("c1", "CHARACTER VARYING"),
            "CASE WHEN SUBSTR(CAST(c1 AS VARCHAR), 1, 1) = 'h' THEN CONVERT_FROM(FROM_HEX(SUBSTR(CAST(c1 AS VARCHAR), 2)), 'UTF8') ELSE SUBSTR(CAST(c1 AS VARCHAR), 2) END"
        );
        assert_eq!(cast_expr("c2", "BINARY VARYING"), "CASE WHEN CAST(c2 AS VARCHAR) IS NULL THEN NULL ELSE FROM_HEX(CAST(c2 AS VARCHAR)) END");
        assert_eq!(cast_expr("c3", "timestamp"), "CAST(c3 AS TIMESTAMP)");
    }

    #[test]
    fn decimals_come_as_plain_digits_with_their_scale() {
        // Java's BigDecimal.toString, as Dremio's CAST(… AS VARCHAR) writes it.
        assert_eq!(to_cell(json!("1E-10"), "DECIMAL(38,10)"), Cell::Decimal("0.0000000001".into()));
        assert_eq!(to_cell(json!("-1.000E-7"), "DECIMAL(38,10)"), Cell::Decimal("-0.0000001000".into()));
        assert_eq!(to_cell(json!("0E-10"), "DECIMAL(38,10)"), Cell::Decimal("0.0000000000".into()));
        assert_eq!(to_cell(json!("12.5"), "DECIMAL(10,3)"), Cell::Decimal("12.500".into()));
        assert_eq!(to_cell(json!("-7"), "DECIMAL(10,0)"), Cell::Decimal("-7".into()));
        assert_eq!(plain_decimal("1.23E+3", Some(0)), "1230");
        assert_eq!(plain_decimal("12345678901234567890.1234", Some(4)), "12345678901234567890.1234");
        assert_eq!(plain_decimal("0.00", None), "0.00");
        assert_eq!(plain_decimal("abc", Some(2)), "abc");
        assert_eq!(scale_of("DECIMAL(38,10)"), Some(10));
        assert_eq!(scale_of("DECIMAL"), None);
    }

    #[test]
    fn text_outside_latin1_goes_as_utf8_bytes() {
        assert_eq!(value_sql(&Cell::Text("😀 a".into()), Kind::Text), "'hF09F98802061'");
        // Any character outside Latin-1, also in the BMP.
        assert_eq!(value_sql(&Cell::Text("Ω".into()), Kind::Text), "'hCEA9'");
        assert_eq!(value_sql(&Cell::Text("ñ'".into()), Kind::Text), "'pñ'''");
        assert_eq!(value_sql(&Cell::Text(String::new()), Kind::Text), "'p'");
        assert_eq!(value_sql(&Cell::Null, Kind::Text), "NULL");
        assert_eq!(value_sql(&Cell::Json("{\"a\":\"中\"}".into()), Kind::Text), "'h7B2261223A22E4B8AD227D'");
        // Into a binary column it's hex already.
        assert_eq!(value_sql(&Cell::Text("😀".into()), Kind::Binary), "'F09F9880'");
        assert_eq!(kind_of("CHARACTER VARYING"), Kind::Text);
        assert_eq!(kind_of("DECIMAL(10,2)"), Kind::Other);
        assert!(is_complex("ARRAY") && is_complex("ROW") && is_complex("map"));
        assert!(!is_complex("VARCHAR") && !is_complex("DECIMAL(10,2)"));
    }

    #[test]
    fn negative_zero_keeps_its_sign() {
        assert_eq!(value_sql(&Cell::Float(-0.0), Kind::Float), "-(RAND() * 0e0)");
        assert_eq!(value_sql(&Cell::Float(0.0), Kind::Float), "0e0");
    }
}
