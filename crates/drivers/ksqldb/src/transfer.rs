//! Bulk transfer (see `dbine_driver::transfer`) for ksqlDB.
//!
//! Reading: a pull query (`SELECT … FROM x;`, no `EMIT CHANGES`) through
//! `/query-stream`, which reads a stream from its beginning to its current
//! end and then finishes. Values are typed from the source's `DESCRIBE`,
//! each taken from the row's raw JSON text, so `DECIMAL`s keep every digit
//! and `BYTES` come whole. `TIME` is read through `FORMAT_TIME` (the query's
//! own output drops the milliseconds) and `ARRAY` / `MAP` / `STRUCT` through
//! `TO_JSON_STRING` (the query's own output turns a `STRUCT`'s decimals into
//! doubles); a `TIME` nested in them comes from the plain column, where
//! times are milliseconds. Tables must be queryable (`CREATE SOURCE TABLE`
//! or `CREATE TABLE … AS SELECT`): a plain `CREATE TABLE` only answers push
//! queries, which never end.
//!
//! Loading: `/inserts-stream` (HTTP/2): the target and then one JSON object
//! per row, acknowledged row by row. Names go quoted (ksqlDB upper-cases
//! bare ones, in `STRUCT`s too). Every value is checked against its column's
//! type before it's sent: ksqlDB acknowledges some values it then loses (a
//! `DECIMAL` too wide, extra decimals it rounds, an unknown `STRUCT` field).
//!
//! Kafka has no transaction to roll back, and ksqlDB goes on inserting a
//! request's rows after the client leaves. So rows go in small requests
//! (about [`CHUNK_TIME`] of work each), one at a time so they keep their
//! order, and every request is read to its end: a cancel takes effect
//! between requests, and the progress reported is what the stream holds.
//! A load dropped mid-request (a multi-threaded runtime) waits for that
//! request before it's gone, so no row lands after it. Nothing is undone:
//! the rows already in stay in.

use crate::{check, http_error, KsqlSession, Lines, Stop};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{kinds, Error, Result};
use serde_json::{json, Value};
use std::sync::atomic::Ordering;
use std::time::{Duration, Instant};
use tokio::runtime::{Handle, RuntimeFlavor};
use tokio::task::JoinHandle;

/// Rows of the first `/inserts-stream` request; the next ones grow or
/// shrink to take about [`CHUNK_TIME`], between [`MIN_ROWS`] and
/// [`MAX_ROWS`] rows…
const FIRST_ROWS: usize = 100;
const MIN_ROWS: usize = 10;
const MAX_ROWS: usize = 2_000;
const CHUNK_TIME: Duration = Duration::from_millis(250);
/// …and at most this many bytes of JSON.
const CHUNK_BYTES: usize = 1024 * 1024;
/// Longest wait for the next acknowledgement.
const ACK_WAIT: Duration = Duration::from_secs(60);

/// A column of a stream or table, from its `DESCRIBE`.
pub(crate) struct Column {
    name: String,
    ty: Ty,
    /// The type as `Session::columns` spells it.
    type_name: String,
    key: bool,
}

fn source_columns(d: &Value) -> Vec<Column> {
    d.get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|f| {
            let schema = f.get("schema").cloned().unwrap_or(Value::Null);
            Column {
                name: crate::text(&f["name"]),
                ty: Ty::of(&schema),
                type_name: crate::schema_type(&schema),
                key: matches!(f.get("type").and_then(Value::as_str), Some("KEY" | "PRIMARY_KEY")),
            }
        })
        .collect()
}

/// `name` among `cols`: as written, or else ignoring case.
fn find<'a>(cols: &'a [Column], name: &str) -> Option<&'a Column> {
    cols.iter().find(|c| c.name == name).or_else(|| cols.iter().find(|c| c.name.eq_ignore_ascii_case(name)))
}

/// The pull query of a read: `TIME` and nested values in the forms that
/// keep them whole (see the module's doc), then the plain columns whose
/// nested `TIME`s are needed.
fn read_sql(table: &str, filter: Option<&str>, cols: &[&Column]) -> String {
    let mut list = Vec::with_capacity(cols.len());
    let mut plain = Vec::new();
    for c in cols {
        let q = quote_ident(Quote::Backtick, &c.name);
        list.push(match &c.ty {
            Ty::Time => format!("FORMAT_TIME({q}, 'HH:mm:ss.SSS') AS {q}"),
            t if t.is_nested() => {
                if t.has_time() {
                    plain.push(format!("{q} AS {}", quote_ident(Quote::Backtick, &format!("dbine plain {}", plain.len()))));
                }
                format!("TO_JSON_STRING({q}) AS {q}")
            }
            _ => q,
        });
    }
    list.extend(plain);
    let mut sql = format!("SELECT {} FROM {}", list.join(", "), quote_ident(Quote::Backtick, table));
    if let Some(f) = filter.map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(&format!(" WHERE {f}"));
    }
    sql.push(';');
    sql
}

/// A bulk load under way.
struct Load<'a> {
    h2: reqwest::Client,
    url: String,
    /// The request's first line (the target).
    head: String,
    cols: Vec<&'a Column>,
    /// Each column's quoted name, as a JSON key.
    keys: Vec<String>,
    progress: Progress<'a>,
    every_rows: u64,
    every_bytes: u64,
    /// Rows in the target.
    done: u64,
    reported: u64,
    /// Bytes loaded since the last report.
    bytes: u64,
    /// Rows of the next request.
    limit: usize,
}

impl Load<'_> {
    /// `n` more rows (`bytes` of JSON) are in the target.
    fn loaded(&mut self, n: usize, bytes: usize) {
        self.done += n as u64;
        self.bytes += bytes as u64;
        if self.done - self.reported >= self.every_rows || self.bytes >= self.every_bytes {
            self.report();
        }
    }

    fn report(&mut self) {
        if self.done != self.reported {
            self.reported = self.done;
            self.bytes = 0;
            (self.progress)(self.done);
        }
    }
}

/// An error in a row's value, with where it is.
fn at_value(e: Error, row: u64, col: &str) -> Error {
    match e {
        Error::Query(m) => Error::Query(format!("fila {row}, columna {col}: {m}")),
        Error::Unsupported(m) => Error::Unsupported(format!("fila {row}, columna {col}: {m}")),
        e => e,
    }
}

/// ksqlDB may have inserted part of a request that failed half way.
fn uncertain(e: Error, rows: usize) -> Error {
    let note = format!(" (ksqlDB puede haber cargado parte de las {rows} filas del último envío: Kafka no deshace)");
    match e {
        Error::Query(m) => Error::Query(m + &note),
        Error::Connect(m) => Error::Connect(m + &note),
        e => e,
    }
}

impl KsqlSession {
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        if spec.table.kind == kinds::TOPIC {
            return Err(Error::Unsupported("un topic no tiene columnas: se lee a través de un stream".into()));
        }
        self.begin();
        let all = source_columns(&self.describe(&spec.table.name).await?);
        let cols: Vec<&Column> = match &spec.columns {
            Some(names) if !names.is_empty() => names
                .iter()
                .map(|n| find(&all, n).ok_or_else(|| Error::Query(format!("{} no tiene la columna «{n}»", spec.table.name))))
                .collect::<Result<_>>()?,
            _ => all.iter().collect(),
        };
        let sql = read_sql(&spec.table.name, spec.filter.as_deref(), &cols);
        let body = json!({ "sql": sql, "properties": self.props() });
        let resp = self
            .conn
            .post("/query-stream")
            .header("Content-Type", "application/json")
            .header("Accept", crate::DELIMITED)
            .body(body.to_string())
            .send()
            .await
            .map_err(http_error)?;
        let resp = match check(resp).await {
            Err(Error::Query(m)) if m.contains("isn't queryable") => {
                return Err(Error::Unsupported(format!(
                    "{} no se puede leer entera: ksqlDB solo lee hasta el final las tablas consultables (CREATE SOURCE TABLE o CREATE TABLE … AS SELECT); de una tabla común solo da consultas push, que no terminan",
                    spec.table.name
                )))
            }
            r => r?,
        };
        let mut lines = Lines::new(resp);
        let mut header = true;
        let mut builder = BatchBuilder::new();
        let r = loop {
            let line = match self.next_line(&mut lines, None).await {
                Ok(Some(l)) if l.trim().is_empty() => continue,
                Ok(Some(l)) => l,
                Ok(None) => break Ok(()),
                Err(Stop::Cancelled) => break Err(Error::Cancelled),
                Err(Stop::Failed(e)) => break Err(e),
                Err(Stop::Timeout) => continue,
            };
            let l = line.trim();
            if l.starts_with('[') && !header {
                let cells = match split_array(l) {
                    Some(raw) if raw.len() >= cols.len() => {
                        let mut plain = raw[cols.len()..].iter();
                        cols.iter()
                            .zip(&raw)
                            .map(|(c, v)| to_cell(v, &c.ty, if c.ty.is_nested() && c.ty.has_time() { plain.next().copied() } else { None }))
                            .collect()
                    }
                    _ => break Err(Error::Query(format!("fila ilegible: {l}"))),
                };
                let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
                if let Err(e) = builder.push(cells, &mut *s) {
                    break Err(e.into());
                }
                continue;
            }
            let v: Value = match serde_json::from_str(l) {
                Ok(v) => v,
                Err(_) => break Err(Error::Query(line)),
            };
            if let Some(m) = v.get("message").or(v.get("errorMessage")) {
                break Err(Error::Query(crate::text(m)));
            }
            if header {
                header = false;
                if let Some(id) = v.get("queryId").and_then(Value::as_str) {
                    *self.in_flight.query_id.lock().unwrap_or_else(|e| e.into_inner()) = Some(id.to_string());
                }
                let tc: Vec<TransferColumn> =
                    cols.iter().map(|c| TransferColumn { name: c.name.clone(), type_name: c.type_name.clone(), nullable: true }).collect();
                sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&tc)?;
            }
            // Anything else: final messages ("Query Completed"…).
        };
        drop(lines);
        self.close_query();
        r?;
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(&mut self, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        if spec.table.kind == kinds::TOPIC {
            return Err(Error::Unsupported("ksqlDB no inserta en topics: hay que hacerlo en un stream".into()));
        }
        self.begin();
        // The target's columns, to write each value as ksqlDB takes it.
        let all = source_columns(&self.describe(&spec.table.name).await?);
        let mut cols = Vec::with_capacity(spec.columns.len());
        for c in &spec.columns {
            match find(&all, c) {
                Some(f) => cols.push(f),
                None => return Err(Error::Query(format!("{} no tiene la columna {c}.", spec.table.name))),
            }
        }
        // ksqlDB rejects every row that lacks the key.
        if let Some(k) = all.iter().find(|k| k.key && !cols.iter().any(|c| c.name == k.name)) {
            return Err(Error::Query(format!("la carga no trae la columna clave {} de {}: ksqlDB la exige en cada fila", k.name, spec.table.name)));
        }
        // /inserts-stream only answers over HTTP/2: h2c on plain HTTP, ALPN on TLS.
        let h2 = if self.conn.base.starts_with("https") {
            self.conn.http.clone()
        } else {
            reqwest::Client::builder().http2_prior_knowledge().connect_timeout(Duration::from_secs(15)).build().map_err(Error::connect)?
        };
        let mut load = Load {
            h2,
            url: format!("{}/inserts-stream", self.conn.base),
            // Quoted, like the columns: ksqlDB upper-cases bare names.
            head: format!("{}\n", json!({ "target": quote_ident(Quote::Backtick, &spec.table.name) })),
            keys: cols.iter().map(|c| json_text(&quote_ident(Quote::Backtick, &c.name))).collect(),
            cols,
            progress,
            every_rows: spec.commit_rows.max(1),
            every_bytes: spec.commit_bytes.max(1),
            done: 0,
            reported: 0,
            bytes: 0,
            limit: FIRST_ROWS,
        };
        let r = self.load_rows(&mut load, source).await;
        // Whatever happened, what's in the target is reported.
        load.report();
        r.map(|_| load.done)
    }

    fn cancelled(&self) -> bool {
        self.in_flight.cancelled.load(Ordering::SeqCst)
    }

    /// The source's next batch, unless the load is cancelled first.
    async fn next_batch(&self, source: &mut dyn BatchSource) -> Result<Option<RowBatch>> {
        let notified = self.in_flight.notify.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        tokio::select! {
            b = source.next() => Ok(b),
            _ = notified => Err(Error::Cancelled),
        }
    }

    async fn load_rows(&self, load: &mut Load<'_>, source: &mut dyn BatchSource) -> Result<()> {
        let mut body = load.head.clone();
        let mut rows = 0usize;
        let mut line = String::new();
        while let Some(b) = self.next_batch(source).await? {
            for row in &b.rows {
                if row.len() != load.cols.len() {
                    return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), load.cols.len())));
                }
                // A row that can't go stops the load before it's sent, with
                // the rows before it in the same request.
                let at = load.done + rows as u64 + 1;
                line.clear();
                line.push('{');
                for (i, (cell, col)) in row.iter().zip(&load.cols).enumerate() {
                    if i > 0 {
                        line.push(',');
                    }
                    if col.key && *cell == Cell::Null {
                        return Err(at_value(Error::Query("ksqlDB no acepta una clave nula".into()), at, &col.name));
                    }
                    line.push_str(&load.keys[i]);
                    line.push(':');
                    write_value(&mut line, cell, &col.ty).map_err(|e| at_value(e, at, &col.name))?;
                }
                line.push_str("}\n");
                body.push_str(&line);
                rows += 1;
                if rows >= load.limit || body.len() >= CHUNK_BYTES {
                    let chunk = std::mem::replace(&mut body, load.head.clone());
                    self.send_rows(load, chunk, std::mem::take(&mut rows)).await?;
                }
            }
        }
        if rows > 0 {
            self.send_rows(load, body, rows).await?;
        }
        Ok(())
    }

    /// One request, and the size of the next one.
    async fn send_rows(&self, load: &mut Load<'_>, body: String, rows: usize) -> Result<()> {
        if self.cancelled() {
            return Err(Error::Cancelled);
        }
        let (t, bytes) = (Instant::now(), body.len());
        let (loaded, err) = self.insert_chunk(load, body, rows).await;
        load.loaded(loaded, bytes * loaded / rows.max(1));
        if let Some(e) = err {
            return Err(e);
        }
        let took = t.elapsed();
        if took < CHUNK_TIME / 2 {
            load.limit = (load.limit * 2).min(MAX_ROWS);
        } else if took > CHUNK_TIME * 2 {
            load.limit = (load.limit / 2).max(MIN_ROWS);
        }
        Ok(())
    }

    /// One `/inserts-stream` request, in its own task (see [`Request`]).
    async fn insert_chunk(&self, load: &Load<'_>, body: String, rows: usize) -> (usize, Option<Error>) {
        let rb = self.conn.auth(load.h2.post(&load.url)).header("Content-Type", crate::DELIMITED).body(body);
        let mut req = Request(Some(tokio::spawn(send_chunk(rb, rows, load.done + 1))));
        let r = match req.0.as_mut() {
            Some(t) => t.await.unwrap_or_else(|e| (0, Some(uncertain(Error::State(format!("envío interrumpido: {e}")), rows)))),
            None => (0, None),
        };
        req.0 = None;
        r
    }
}

/// A request's task: dropped before it ends (a load cancelled by dropping
/// it), it's waited for, so its rows are in before the load is gone and
/// none lands during whatever runs after it. The wait is bounded: one
/// request of at most [`MAX_ROWS`] rows, [`ACK_WAIT`] a line. It needs a
/// multi-threaded tokio runtime (the app's and the driver host's); on a
/// single-threaded one it can't wait.
struct Request(Option<JoinHandle<(usize, Option<Error>)>>);

impl Drop for Request {
    fn drop(&mut self) {
        let Some(t) = self.0.take() else { return };
        if t.is_finished() {
            return;
        }
        let Ok(h) = Handle::try_current() else { return };
        if h.runtime_flavor() != RuntimeFlavor::MultiThread {
            return;
        }
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        h.spawn(async move {
            let _ = t.await;
            drop(tx);
        });
        // Off the worker, so the request keeps going while this waits (a
        // runtime shutting down drops the task and ends the wait).
        tokio::task::block_in_place(|| {
            let _ = rx.recv();
        });
    }
}

/// One `/inserts-stream` request (its rows numbered from `first`), always
/// read to its end: ksqlDB inserts the rows it received even if the client
/// goes away, so leaving early (on a cancel or on the first rejected row)
/// would leave rows loading after the load returned. The rows in the
/// target, and why not all.
async fn send_chunk(rb: reqwest::RequestBuilder, rows: usize, first: u64) -> (usize, Option<Error>) {
    let resp = match rb.send().await {
        Ok(r) => r,
        // Nothing got there.
        Err(e) if e.is_connect() => return (0, Some(http_error(e))),
        Err(e) => return (0, Some(uncertain(http_error(e), rows))),
    };
    // The whole request refused (unknown target…): nothing inserted.
    let mut lines = match check(resp).await {
        Ok(r) => Lines::new(r),
        Err(e) => return (0, Some(e)),
    };
    let mut ok: Vec<u64> = Vec::with_capacity(rows);
    let mut rejected: Option<(Option<u64>, String)> = None;
    loop {
        let l = match tokio::time::timeout(ACK_WAIT, lines.next()).await {
            Ok(Ok(Some(l))) => l,
            Ok(Ok(None)) => break,
            Ok(Err(e)) => return (ok.len(), Some(uncertain(e, rows))),
            Err(_) => return (ok.len(), Some(uncertain(Error::Query("ksqlDB dejó de confirmar filas".into()), rows))),
        };
        if l.trim().is_empty() {
            continue;
        }
        let v: Value = match serde_json::from_str(&l) {
            Ok(v) => v,
            Err(_) => {
                rejected.get_or_insert((None, l));
                continue;
            }
        };
        let seq = v.get("seq").and_then(Value::as_u64);
        match (v.get("status").and_then(Value::as_str), seq) {
            (Some("ok"), Some(s)) => ok.push(s),
            _ => {
                rejected.get_or_insert((seq, v.get("message").map(crate::text).unwrap_or(l)));
            }
        }
    }
    match rejected {
        None if ok.len() == rows => (rows, None),
        None => (ok.len(), Some(uncertain(Error::Query(format!("ksqlDB confirmó {} filas de {rows}", ok.len())), rows))),
        // ksqlDB inserts in order and may leave the rows before the
        // rejected one unacknowledged, yet they're in.
        Some((Some(s), msg)) => {
            let s = s as usize;
            let loaded = s + ok.iter().filter(|&&x| x as usize > s).count();
            let after = rows.saturating_sub(s + 1);
            let mut m = format!("fila {}: {msg}", first + s as u64);
            if after > 0 {
                m.push_str(&format!(
                    ". Las filas anteriores quedaron cargadas; de las {after} que venían después en el mismo envío, ksqlDB confirmó {}, y puede haber cargado otras sin confirmarlas (Kafka no deshace)",
                    loaded - s
                ));
            }
            (loaded, Some(Error::Query(m)))
        }
        Some((None, msg)) => (ok.len(), Some(uncertain(Error::Query(msg), rows))),
    }
}

// ---- rows → cells ----

/// The top-level elements of a JSON array, as their raw text.
pub(crate) fn split_array(s: &str) -> Option<Vec<&str>> {
    let inner = s.trim().strip_prefix('[')?.strip_suffix(']')?;
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut esc, mut start) = (0i32, false, false, 0usize);
    for (i, c) in inner.char_indices() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '[' | '{' => depth += 1,
            ']' | '}' => depth -= 1,
            ',' if depth == 0 => {
                out.push(inner[start..i].trim());
                start = i + 1;
            }
            _ => {}
        }
    }
    if !inner[start..].trim().is_empty() || !out.is_empty() {
        out.push(inner[start..].trim());
    }
    Some(out)
}

/// Most digits a decimal is expanded to when its column's precision is
/// not known, or when a read value is rendered without its exponent.
const MAX_DECIMAL_DIGITS: u64 = 1000;

/// A decimal's text as its sign, its significant digits (no leading or
/// trailing zeros) and the position of the point within them:
/// `-01.50E+2` → `(true, "15", 3)`. Nothing is expanded, so an exponent
/// like `1e300000000` costs nothing; one past an `i64` saturates to a
/// point far enough that any size check rejects it. `None`: not a number.
fn decimal_parts(s: &str) -> Option<(bool, String, i64)> {
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mant, exp) = match s.find(['E', 'e']) {
        Some(e) => (&s[..e], Some(&s[e + 1..])),
        None => (s, None),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if (int.is_empty() && frac.is_empty()) || !int.bytes().chain(frac.bytes()).all(|b| b.is_ascii_digit()) {
        return None;
    }
    let exp = match exp {
        None => 0,
        Some(x) => {
            let d = x.strip_prefix(['+', '-']).unwrap_or(x);
            if d.is_empty() || !d.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            // Only digits are left, so a failed parse is an overflow.
            x.parse::<i64>().unwrap_or(if x.starts_with('-') { i64::MIN / 4 } else { i64::MAX / 4 })
        }
    };
    // Leading zeros may run from the integer part into the fraction
    // (`0.005`): the point moves left past them. The digits are no longer
    // than the input.
    let all = format!("{int}{frac}");
    let sig = all.trim_start_matches('0');
    let point = int.len() as i64 - (all.len() - sig.len()) as i64;
    let digits = sig.trim_end_matches('0').to_string();
    let point = if digits.is_empty() { 0 } else { point.saturating_add(exp) };
    Some((neg, digits, point))
}

/// How many integer and fractional digits `digits` has with the point at
/// `point` once written without an exponent.
fn decimal_widths(digits: &str, point: i64) -> (u64, u64) {
    (point.max(0) as u64, (digits.len() as i64).saturating_sub(point).max(0) as u64)
}

/// Plain text of [`decimal_parts`]; the caller has bounded its width.
fn render_decimal(neg: bool, digits: &str, point: i64) -> String {
    if digits.is_empty() {
        return "0".into();
    }
    let (int, frac) = decimal_widths(digits, point);
    let mut out = String::with_capacity((int + frac) as usize + 3);
    if neg {
        out.push('-');
    }
    if point <= 0 {
        out.push_str("0.");
        out.extend(std::iter::repeat_n('0', (-point) as usize));
        out.push_str(digits);
    } else if point as usize >= digits.len() {
        out.push_str(digits);
        out.extend(std::iter::repeat_n('0', point as usize - digits.len()));
    } else {
        out.push_str(&digits[..point as usize]);
        out.push('.');
        out.push_str(&digits[point as usize..]);
    }
    out
}

/// `1E+2` → `100` (Java's `BigDecimal` text may carry an exponent). Text
/// that isn't a number, or that would expand past [`MAX_DECIMAL_DIGITS`],
/// is kept as is.
fn plain_decimal(s: &str) -> String {
    if !s.contains(['E', 'e']) {
        return s.to_string();
    }
    match decimal_parts(s) {
        Some((neg, digits, point)) => {
            let (int, frac) = decimal_widths(&digits, point);
            if int + frac > MAX_DECIMAL_DIGITS {
                s.to_string()
            } else {
                render_decimal(neg, &digits, point)
            }
        }
        None => s.to_string(),
    }
}

fn unquote(raw: &str) -> String {
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string())
}

/// `2024-01-31T13:45:00.000` → `2024-01-31 13:45:00` (a zero fraction dropped).
fn timestamp_text(s: &str) -> String {
    let s = s.replacen('T', " ", 1);
    match s.split_once('.') {
        Some((a, f)) if f.chars().all(|c| c == '0') => a.to_string(),
        _ => s,
    }
}

/// `13:45` → `13:45:00`; `13:45:00.000` → `13:45:00`.
fn time_text(s: &str) -> String {
    if s.len() == 5 {
        return format!("{s}:00");
    }
    match s.split_once('.') {
        Some((a, f)) if f.chars().all(|c| c == '0') => a.to_string(),
        _ => s.to_string(),
    }
}

/// Milliseconds of the day (how ksqlDB keeps a `TIME`) as `HH:MM:SS[.fff]`.
fn ms_time(ms: i64) -> String {
    let ms = ms.rem_euclid(86_400_000);
    let (h, m, s, f) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    if f == 0 {
        format!("{h:02}:{m:02}:{s:02}")
    } else {
        format!("{h:02}:{m:02}:{s:02}.{f:03}")
    }
}

/// A ksqlDB type, from a `DESCRIBE`'s schema.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Ty {
    Boolean,
    Integer,
    Bigint,
    Double,
    /// Precision and scale (0, 0: not known).
    Decimal(u32, u32),
    Str,
    Bytes,
    Date,
    Time,
    Timestamp,
    Array(Box<Ty>),
    /// Keys are always `STRING`.
    Map(Box<Ty>),
    Struct(Vec<(String, Ty)>),
    Other(String),
}

impl Ty {
    fn of(s: &Value) -> Ty {
        let member = || Box::new(s.get("memberSchema").map_or(Ty::Other(String::new()), Ty::of));
        let param = |k: &str| s.pointer(k).map(crate::text).and_then(|t| t.parse().ok()).unwrap_or(0);
        match s.get("type").and_then(Value::as_str).unwrap_or_default() {
            "BOOLEAN" => Ty::Boolean,
            "INTEGER" | "INT" => Ty::Integer,
            "BIGINT" => Ty::Bigint,
            "DOUBLE" => Ty::Double,
            "DECIMAL" => Ty::Decimal(param("/parameters/precision"), param("/parameters/scale")),
            "STRING" | "VARCHAR" => Ty::Str,
            "BYTES" => Ty::Bytes,
            "DATE" => Ty::Date,
            "TIME" => Ty::Time,
            "TIMESTAMP" => Ty::Timestamp,
            "ARRAY" => Ty::Array(member()),
            "MAP" => Ty::Map(member()),
            "STRUCT" => Ty::Struct(
                s.get("fields")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .map(|f| (crate::text(&f["name"]), f.get("schema").map_or(Ty::Other(String::new()), Ty::of)))
                    .collect(),
            ),
            t => Ty::Other(t.to_string()),
        }
    }

    fn is_nested(&self) -> bool {
        matches!(self, Ty::Array(_) | Ty::Map(_) | Ty::Struct(_))
    }

    /// Holds a `TIME` (or is one).
    fn has_time(&self) -> bool {
        match self {
            Ty::Time => true,
            Ty::Array(t) | Ty::Map(t) => t.has_time(),
            Ty::Struct(f) => f.iter().any(|(_, t)| t.has_time()),
            _ => false,
        }
    }

    /// For messages.
    fn name(&self) -> String {
        match self {
            Ty::Boolean => "BOOLEAN".into(),
            Ty::Integer => "INTEGER".into(),
            Ty::Bigint => "BIGINT".into(),
            Ty::Double => "DOUBLE".into(),
            Ty::Decimal(p, s) => format!("DECIMAL({p}, {s})"),
            Ty::Str => "STRING".into(),
            Ty::Bytes => "BYTES".into(),
            Ty::Date => "DATE".into(),
            Ty::Time => "TIME".into(),
            Ty::Timestamp => "TIMESTAMP".into(),
            Ty::Array(t) => format!("ARRAY<{}>", t.name()),
            Ty::Map(t) => format!("MAP<STRING, {}>", t.name()),
            Ty::Struct(f) => format!("STRUCT<{}>", f.iter().map(|(n, t)| format!("{} {}", quote_ident(Quote::Backtick, n), t.name())).collect::<Vec<_>>().join(", ")),
            Ty::Other(t) => t.clone(),
        }
    }
}

/// JSON that keeps each number's text (a `serde_json::Value` would take
/// decimals through a double).
#[derive(Debug, Clone, PartialEq)]
enum J {
    Null,
    Bool(bool),
    Num(String),
    Str(String),
    Arr(Vec<J>),
    Obj(Vec<(String, J)>),
}

fn parse_j(s: &str) -> Option<J> {
    let mut p = Parser { s: s.as_bytes(), text: s, i: 0 };
    let v = p.value(0)?;
    p.ws();
    (p.i == p.s.len()).then_some(v)
}

struct Parser<'a> {
    s: &'a [u8],
    text: &'a str,
    i: usize,
}

impl Parser<'_> {
    fn ws(&mut self) {
        while let Some(b' ' | b'\t' | b'\n' | b'\r') = self.s.get(self.i) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> bool {
        self.ws();
        let hit = self.s.get(self.i) == Some(&c);
        if hit {
            self.i += 1;
        }
        hit
    }

    fn word(&mut self, w: &str, v: J) -> Option<J> {
        self.text[self.i..].starts_with(w).then(|| {
            self.i += w.len();
            v
        })
    }

    fn value(&mut self, depth: usize) -> Option<J> {
        if depth > 256 {
            return None;
        }
        self.ws();
        match *self.s.get(self.i)? {
            b'n' => self.word("null", J::Null),
            b't' => self.word("true", J::Bool(true)),
            b'f' => self.word("false", J::Bool(false)),
            b'"' => self.string().map(J::Str),
            b'[' => {
                self.i += 1;
                let mut v = Vec::new();
                if self.eat(b']') {
                    return Some(J::Arr(v));
                }
                loop {
                    v.push(self.value(depth + 1)?);
                    if !self.eat(b',') {
                        return self.eat(b']').then_some(J::Arr(v));
                    }
                }
            }
            b'{' => {
                self.i += 1;
                let mut v = Vec::new();
                if self.eat(b'}') {
                    return Some(J::Obj(v));
                }
                loop {
                    self.ws();
                    let k = self.string()?;
                    if !self.eat(b':') {
                        return None;
                    }
                    v.push((k, self.value(depth + 1)?));
                    if !self.eat(b',') {
                        return self.eat(b'}').then_some(J::Obj(v));
                    }
                }
            }
            b'-' | b'0'..=b'9' => {
                let start = self.i;
                while let Some(b'-' | b'+' | b'.' | b'e' | b'E' | b'0'..=b'9') = self.s.get(self.i) {
                    self.i += 1;
                }
                let t = &self.text[start..self.i];
                t.parse::<f64>().is_ok().then(|| J::Num(t.to_string()))
            }
            _ => None,
        }
    }

    fn string(&mut self) -> Option<String> {
        if self.s.get(self.i) != Some(&b'"') {
            return None;
        }
        let start = self.i;
        self.i += 1;
        let mut esc = false;
        loop {
            let c = *self.s.get(self.i)?;
            self.i += 1;
            match (esc, c) {
                (true, _) => esc = false,
                (false, b'\\') => esc = true,
                (false, b'"') => break,
                _ => {}
            }
        }
        serde_json::from_str(&self.text[start..self.i]).ok()
    }
}

fn put_j(out: &mut String, j: &J) {
    match j {
        J::Null => out.push_str("null"),
        J::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
        J::Num(t) => out.push_str(t),
        J::Str(s) => push_str(out, s),
        J::Arr(v) => {
            out.push('[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                put_j(out, x);
            }
            out.push(']');
        }
        J::Obj(v) => {
            out.push('{');
            for (i, (k, x)) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_str(out, k);
                out.push(':');
                put_j(out, x);
            }
            out.push('}');
        }
    }
}

/// `TO_JSON_STRING` writes a nested `TIME` without its milliseconds: they're
/// taken from the plain value, where a `TIME` is milliseconds of the day.
fn patch_times(j: &mut J, plain: &J, ty: &Ty) {
    match (ty, j, plain) {
        (Ty::Time, j, J::Num(ms)) => {
            if let Ok(ms) = ms.parse::<i64>() {
                *j = J::Str(ms_time(ms));
            }
        }
        (Ty::Array(t), J::Arr(a), J::Arr(b)) => {
            for (x, y) in a.iter_mut().zip(b) {
                patch_times(x, y, t);
            }
        }
        (Ty::Map(t), J::Obj(a), J::Obj(b)) => {
            for (k, x) in a.iter_mut() {
                if let Some((_, y)) = b.iter().find(|(kb, _)| kb == k) {
                    patch_times(x, y, t);
                }
            }
        }
        (Ty::Struct(f), J::Obj(a), J::Obj(b)) => {
            for (k, x) in a.iter_mut() {
                if let (Some((_, t)), Some((_, y))) = (f.iter().find(|(n, _)| n == k), b.iter().find(|(kb, _)| kb == k)) {
                    patch_times(x, y, t);
                }
            }
        }
        _ => {}
    }
}

/// One raw JSON value of a read as a cell of type `ty`. `plain`: a nested
/// value's plain form, when `raw` is its `TO_JSON_STRING` and it holds a
/// `TIME`.
pub(crate) fn to_cell(raw: &str, ty: &Ty, plain: Option<&str>) -> Cell {
    if raw == "null" || raw.is_empty() {
        return Cell::Null;
    }
    match ty {
        Ty::Boolean => Cell::Bool(raw == "true"),
        Ty::Integer | Ty::Bigint => raw.parse().map(Cell::Int).unwrap_or_else(|_| Cell::Text(raw.to_string())),
        // `"Infinity"`, `"-Infinity"` and `"NaN"` come quoted.
        Ty::Double => {
            let t = unquote(raw);
            t.parse().map(Cell::Float).unwrap_or(Cell::Text(t))
        }
        Ty::Decimal(..) => Cell::Decimal(plain_decimal(raw.trim_matches('"'))),
        Ty::Bytes => base64_decode(&unquote(raw)).map(Cell::Bytes).unwrap_or_else(|| Cell::Text(unquote(raw))),
        Ty::Date => Cell::Date(unquote(raw)),
        // `FORMAT_TIME`'s text, or milliseconds of the day.
        Ty::Time => match raw.parse::<i64>() {
            Ok(ms) => Cell::Time(ms_time(ms)),
            Err(_) => Cell::Time(time_text(&unquote(raw))),
        },
        Ty::Timestamp => Cell::DateTime(timestamp_text(&unquote(raw))),
        Ty::Array(_) | Ty::Map(_) | Ty::Struct(_) => {
            // `TO_JSON_STRING`'s text (a JSON string), or the plain value.
            let text = if raw.starts_with('"') { unquote(raw) } else { raw.to_string() };
            if text == "null" {
                return Cell::Null;
            }
            match (plain.filter(|p| *p != "null").and_then(parse_j), parse_j(&text)) {
                (Some(p), Some(mut j)) => {
                    patch_times(&mut j, &p, ty);
                    let mut s = String::with_capacity(text.len() + 8);
                    put_j(&mut s, &j);
                    Cell::Json(s)
                }
                _ => Cell::Json(text),
            }
        }
        Ty::Str => Cell::Text(unquote(raw)),
        Ty::Other(_) if raw.starts_with('"') => Cell::Text(unquote(raw)),
        Ty::Other(_) => Cell::Json(raw.to_string()),
    }
}

// ---- cells → JSON rows ----

fn text_of(c: &Cell) -> String {
    match c {
        Cell::Null => String::new(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => String::from_utf8_lossy(b).into_owned(),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => s.clone(),
    }
}

fn push_str(out: &mut String, s: &str) {
    out.push_str(&json_text(s));
}

fn json_text(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_else(|_| "\"\"".into())
}

fn bad(m: String) -> Error {
    Error::Query(m)
}

fn put_int(out: &mut String, n: i64, ty: &Ty) -> Result<()> {
    if *ty == Ty::Integer && i32::try_from(n).is_err() {
        return Err(bad(format!("{n} no entra en INTEGER")));
    }
    out.push_str(&n.to_string());
    Ok(())
}

fn int_text(t: &str) -> Result<i64> {
    t.trim().parse().map_err(|_| bad(format!("«{t}» no es un entero")))
}

fn put_double(out: &mut String, f: f64) -> Result<()> {
    if f.is_nan() {
        return Err(Error::Unsupported(
            "ksqlDB no recibe NaN: /inserts-stream solo acepta JSON, que no tiene NaN, y el texto \"NaN\" no lo convierte a DOUBLE".into(),
        ));
    }
    if f.is_infinite() {
        // JSON has no infinity; ksqlDB reads a number past a double's
        // range as one.
        out.push_str(if f > 0.0 { "1e400" } else { "-1e400" });
        return Ok(());
    }
    out.push_str(&serde_json::Number::from_f64(f).map(|n| n.to_string()).unwrap_or_else(|| f.to_string()));
    Ok(())
}

/// Plain digits that fit `DECIMAL(p, s)` without rounding (`p == 0`: not
/// known). ksqlDB acknowledges values that don't fit and then loses them:
/// too many integer digits make the row unreadable, extra decimals are
/// rounded.
fn decimal_text(t: &str, p: u32, s: u32) -> Result<String> {
    let t = t.trim();
    let no = || bad(format!("«{t}» no es un número decimal"));
    if t.is_empty() || !t.bytes().all(|b| matches!(b, b'0'..=b'9' | b'+' | b'-' | b'.' | b'e' | b'E')) {
        return Err(no());
    }
    // Sizes are checked on the exponent form, before any digit is
    // expanded: `1e300000000` is 11 bytes and would be 300 MB of zeros.
    let (neg, digits, point) = decimal_parts(t).ok_or_else(no)?;
    let (int, frac) = decimal_widths(&digits, point);
    if p > 0 {
        if int > p.saturating_sub(s) as u64 {
            return Err(bad(format!("«{t}» no entra en DECIMAL({p}, {s}): admite {} dígitos enteros", p.saturating_sub(s))));
        }
        if frac > s as u64 {
            return Err(bad(format!("«{t}» no entra en DECIMAL({p}, {s}): tiene más de {s} decimales y se redondearía")));
        }
    } else if int + frac > MAX_DECIMAL_DIGITS {
        return Err(bad(format!("«{t}» tiene más de {MAX_DECIMAL_DIGITS} dígitos: no se puede cargar como DECIMAL")));
    }
    Ok(render_decimal(neg, &digits, point))
}

fn valid_date(d: &str) -> bool {
    let b = d.as_bytes();
    b.len() == 10
        && b[4] == b'-'
        && b[7] == b'-'
        && [0, 1, 2, 3, 5, 6, 8, 9].iter().all(|&i| b[i].is_ascii_digit())
        && {
            let (y, m, day) = (d[..4].parse::<u32>().unwrap_or(0), d[5..7].parse::<u32>().unwrap_or(0), d[8..10].parse::<u32>().unwrap_or(0));
            let leap = y % 4 == 0 && (y % 100 != 0 || y % 400 == 0);
            let last = match m {
                2 if leap => 29,
                2 => 28,
                4 | 6 | 9 | 11 => 30,
                1..=12 => 31,
                _ => 0,
            };
            (1..=last).contains(&day)
        }
}

/// `YYYY-MM-DD`, from a date, or a date and time when the time is zero.
fn date_text(t: &str) -> Result<String> {
    let t = t.trim();
    let d = t.get(..10).filter(|d| valid_date(d)).ok_or_else(|| bad(format!("«{t}» no es una fecha")))?;
    let rest = t[10..].trim_start_matches([' ', 'T']);
    if !rest.bytes().all(|b| matches!(b, b'0' | b':' | b'.')) {
        return Err(bad(format!("«{t}» tiene hora y DATE la perdería")));
    }
    Ok(d.to_string())
}

fn two(x: &str) -> Option<u32> {
    if x.len() == 2 && x.bytes().all(|b| b.is_ascii_digit()) {
        x.parse().ok()
    } else {
        None
    }
}

/// `HH:MM[:SS[.f…]]` as `HH:MM:SS[.fff]`; `None` if it isn't a time, an
/// error if it has digits past the millisecond, which ksqlDB would drop.
fn clock(t: &str) -> Option<Result<String>> {
    let (hms, frac) = t.split_once('.').unwrap_or((t, ""));
    let mut parts = hms.split(':');
    let (h, m) = (two(parts.next()?)?, two(parts.next()?)?);
    let s = match parts.next() {
        Some(x) => two(x)?,
        None => 0,
    };
    if parts.next().is_some() || h > 23 || m > 59 || s > 59 || !frac.bytes().all(|b| b.is_ascii_digit()) || (t.contains('.') && frac.is_empty()) {
        return None;
    }
    if frac.len() > 3 && frac.bytes().skip(3).any(|b| b != b'0') {
        return Some(Err(bad(format!("«{t}» tiene más precisión que el milisegundo que guarda ksqlDB"))));
    }
    let ms = format!("{:0<3}", &frac[..frac.len().min(3)]);
    Some(Ok(if ms == "000" { format!("{h:02}:{m:02}:{s:02}") } else { format!("{h:02}:{m:02}:{s:02}.{ms}") }))
}

fn time_value(t: &str) -> Result<String> {
    let t = t.trim();
    clock(t).unwrap_or_else(|| Err(bad(format!("«{t}» no es una hora"))))
}

/// `Z`, `±HH`, `±HHMM` or `±HH:MM` as `Z` or `±HH:MM`.
fn zone(z: &str) -> Option<String> {
    if z.eq_ignore_ascii_case("z") {
        return Some("Z".into());
    }
    let (sign, rest) = (z.get(..1)?, z.get(1..)?);
    if sign != "+" && sign != "-" {
        return None;
    }
    let (h, m) = match rest.len() {
        2 => (two(rest)?, 0),
        4 => (two(&rest[..2])?, two(&rest[2..])?),
        5 if &rest[2..3] == ":" => (two(&rest[..2])?, two(&rest[3..])?),
        _ => return None,
    };
    (h <= 18 && m <= 59).then(|| format!("{sign}{h:02}:{m:02}"))
}

/// ISO 8601 as ksqlDB reads it (with a zone, it converts to UTC).
fn timestamp_value(t: &str) -> Result<String> {
    let t = t.trim();
    let no = || bad(format!("«{t}» no es una fecha y hora"));
    // ksqlDB takes a DATE of year 0 but not a TIMESTAMP.
    let d = t.get(..10).filter(|d| valid_date(d) && !d.starts_with("0000")).ok_or_else(no)?;
    let rest = &t[10..];
    if rest.is_empty() {
        return Ok(format!("{d}T00:00:00"));
    }
    let rest = rest.strip_prefix(['T', ' ']).ok_or_else(no)?;
    let cut = rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len());
    let (time, z) = rest.split_at(cut);
    let z = if z.is_empty() { String::new() } else { zone(z).ok_or_else(no)? };
    let time = clock(time).ok_or_else(no)??;
    Ok(format!("{d}T{time}{z}"))
}

/// A cell as the JSON ksqlDB takes for a column of type `ty`, checked so
/// that what's stored is the value given.
pub(crate) fn write_value(out: &mut String, c: &Cell, ty: &Ty) -> Result<()> {
    if *c == Cell::Null {
        out.push_str("null");
        return Ok(());
    }
    match ty {
        Ty::Integer | Ty::Bigint => {
            let n: i64 = match c {
                Cell::Int(i) => *i,
                Cell::UInt(u) => i64::try_from(*u).map_err(|_| bad(format!("{u} no entra en BIGINT")))?,
                Cell::Bool(b) => *b as i64,
                Cell::Float(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => *f as i64,
                other => int_text(&text_of(other))?,
            };
            put_int(out, n, ty)?;
        }
        Ty::Double => {
            let f: f64 = match c {
                Cell::Float(f) => *f,
                Cell::Int(i) if (*i as f64) as i64 == *i => *i as f64,
                Cell::UInt(u) if (*u as f64) as u64 == *u => *u as f64,
                Cell::Int(_) | Cell::UInt(_) => return Err(bad(format!("{} no entra exacto en DOUBLE", text_of(c)))),
                other => {
                    let t = text_of(other);
                    t.trim().parse().map_err(|_| bad(format!("«{t}» no es un número")))?
                }
            };
            put_double(out, f)?;
        }
        Ty::Boolean => match c {
            Cell::Bool(b) => out.push_str(if *b { "true" } else { "false" }),
            other => match text_of(other).trim().to_ascii_lowercase().as_str() {
                "1" | "true" | "t" => out.push_str("true"),
                "0" | "false" | "f" => out.push_str("false"),
                t => return Err(bad(format!("«{t}» no es un booleano"))),
            },
        },
        // As text: a JSON number would go through a double.
        Ty::Decimal(p, s) => {
            let t = match c {
                Cell::Bool(_) | Cell::Bytes(_) | Cell::Json(_) => return Err(bad(format!("«{}» no es un número decimal", text_of(c)))),
                other => text_of(other),
            };
            push_str(out, &decimal_text(&t, *p, *s)?);
        }
        Ty::Str => match c {
            Cell::Bytes(b) => push_str(out, std::str::from_utf8(b).map_err(|_| bad("el valor binario no es texto UTF-8".into()))?),
            other => push_str(out, &text_of(other)),
        },
        Ty::Bytes => match c {
            Cell::Bytes(b) => push_str(out, &base64_encode(b)),
            other => push_str(out, &base64_encode(text_of(other).as_bytes())),
        },
        Ty::Date => push_str(out, &date_text(&text_of(c))?),
        Ty::Time => push_str(out, &time_value(&text_of(c))?),
        Ty::Timestamp => push_str(out, &timestamp_value(&text_of(c))?),
        Ty::Array(_) | Ty::Map(_) | Ty::Struct(_) => match c {
            Cell::Json(s) | Cell::Text(s) => {
                let j = parse_j(s).ok_or_else(|| bad(format!("«{s}» no es un valor {}", ty.name())))?;
                write_j(out, &j, ty)?;
            }
            other => return Err(bad(format!("«{}» no es un valor {}", text_of(other), ty.name()))),
        },
        Ty::Other(_) => push_str(out, &text_of(c)),
    }
    Ok(())
}

/// A nested value, typed like a cell: decimals as text, `STRUCT` fields
/// by their quoted names (bare ones are upper-cased, and a field ksqlDB
/// doesn't find is stored as null without an error).
fn write_j(out: &mut String, j: &J, ty: &Ty) -> Result<()> {
    match (ty, j) {
        (_, J::Null) => out.push_str("null"),
        (Ty::Integer | Ty::Bigint, J::Num(t) | J::Str(t)) => put_int(out, int_text(t)?, ty)?,
        (Ty::Double, J::Num(t) | J::Str(t)) => put_double(out, t.trim().parse().map_err(|_| bad(format!("«{t}» no es un número")))?)?,
        (Ty::Boolean, J::Bool(b)) => out.push_str(if *b { "true" } else { "false" }),
        (Ty::Decimal(p, s), J::Num(t) | J::Str(t)) => push_str(out, &decimal_text(t, *p, *s)?),
        (Ty::Str, J::Str(t) | J::Num(t)) => push_str(out, t),
        (Ty::Str, J::Bool(b)) => push_str(out, if *b { "true" } else { "false" }),
        (Ty::Bytes, J::Str(t)) if base64_decode(t).is_some() => push_str(out, t),
        (Ty::Date, J::Str(t)) => push_str(out, &date_text(t)?),
        (Ty::Time, J::Str(t)) => push_str(out, &time_value(t)?),
        (Ty::Timestamp, J::Str(t)) => push_str(out, &timestamp_value(t)?),
        (Ty::Array(t), J::Arr(v)) => {
            out.push('[');
            for (i, x) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                write_j(out, x, t)?;
            }
            out.push(']');
        }
        (Ty::Map(t), J::Obj(v)) => {
            out.push('{');
            for (i, (k, x)) in v.iter().enumerate() {
                if i > 0 {
                    out.push(',');
                }
                push_str(out, k);
                out.push(':');
                write_j(out, x, t)?;
            }
            out.push('}');
        }
        (Ty::Struct(fields), J::Obj(v)) => {
            out.push('{');
            for (i, (k, x)) in v.iter().enumerate() {
                let (name, t) = fields
                    .iter()
                    .find(|(n, _)| n == k)
                    .or_else(|| fields.iter().find(|(n, _)| n.eq_ignore_ascii_case(k)))
                    .ok_or_else(|| bad(format!("{} no tiene el campo «{k}»", ty.name())))?;
                if i > 0 {
                    out.push(',');
                }
                push_str(out, &quote_ident(Quote::Backtick, name));
                out.push(':');
                write_j(out, x, t)?;
            }
            out.push('}');
        }
        (Ty::Other(_), j) => put_j(out, j),
        (ty, j) => {
            let mut s = String::new();
            put_j(&mut s, j);
            return Err(bad(format!("«{s}» no es un valor {}", ty.name())));
        }
    }
    Ok(())
}

// ---- base64 (ksqlDB's BYTES) ----

const B64: &[u8; 64] = b"ABCDEFGHIJKLMNOPQRSTUVWXYZabcdefghijklmnopqrstuvwxyz0123456789+/";

pub(crate) fn base64_encode(b: &[u8]) -> String {
    let mut out = String::with_capacity(b.len().div_ceil(3) * 4);
    for c in b.chunks(3) {
        let n = (c[0] as u32) << 16 | (*c.get(1).unwrap_or(&0) as u32) << 8 | *c.get(2).unwrap_or(&0) as u32;
        out.push(B64[(n >> 18) as usize & 63] as char);
        out.push(B64[(n >> 12) as usize & 63] as char);
        out.push(if c.len() > 1 { B64[(n >> 6) as usize & 63] as char } else { '=' });
        out.push(if c.len() > 2 { B64[n as usize & 63] as char } else { '=' });
    }
    out
}

pub(crate) fn base64_decode(s: &str) -> Option<Vec<u8>> {
    let val = |c: u8| B64.iter().position(|&x| x == c).map(|p| p as u32);
    let s = s.trim_end_matches('=').as_bytes();
    let mut out = Vec::with_capacity(s.len() * 3 / 4);
    for c in s.chunks(4) {
        let mut n = 0u32;
        for (i, &x) in c.iter().enumerate() {
            n |= val(x)? << (18 - 6 * i);
        }
        out.push((n >> 16) as u8);
        if c.len() > 2 {
            out.push((n >> 8) as u8);
        }
        if c.len() > 3 {
            out.push(n as u8);
        }
        if c.len() == 1 {
            return None;
        }
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn arr(t: Ty) -> Ty {
        Ty::Array(Box::new(t))
    }

    #[test]
    fn raw_rows_become_typed_cells() {
        let line = r#"[1,1234567890123456.1234,"yv4=","2024-01-31","13:45","2024-01-31T13:45:00.120","[1,2]","{\"k\":\"a,b\"}","{\"X\":5}",true,0.1,"ñ \"x\", y",null,"01:02:03.456","12:00:00.000","Infinity","null"]"#;
        let types = [
            Ty::Bigint,
            Ty::Decimal(20, 4),
            Ty::Bytes,
            Ty::Date,
            Ty::Time,
            Ty::Timestamp,
            arr(Ty::Integer),
            Ty::Map(Box::new(Ty::Str)),
            Ty::Struct(vec![("X".into(), Ty::Integer)]),
            Ty::Boolean,
            Ty::Double,
            Ty::Str,
            Ty::Str,
            Ty::Time,
            Ty::Time,
            Ty::Double,
            arr(Ty::Integer),
        ];
        let raw = split_array(line).unwrap();
        let cells: Vec<Cell> = raw.iter().zip(&types).map(|(r, t)| to_cell(r, t, None)).collect();
        assert_eq!(
            cells,
            vec![
                Cell::Int(1),
                Cell::Decimal("1234567890123456.1234".into()),
                Cell::Bytes(vec![0xca, 0xfe]),
                Cell::Date("2024-01-31".into()),
                Cell::Time("13:45:00".into()),
                Cell::DateTime("2024-01-31 13:45:00.120".into()),
                Cell::Json("[1,2]".into()),
                Cell::Json(r#"{"k":"a,b"}"#.into()),
                Cell::Json(r#"{"X":5}"#.into()),
                Cell::Bool(true),
                Cell::Float(0.1),
                Cell::Text("ñ \"x\", y".into()),
                Cell::Null,
                Cell::Time("01:02:03.456".into()),
                Cell::Time("12:00:00".into()),
                Cell::Float(f64::INFINITY),
                // TO_JSON_STRING of a null.
                Cell::Null,
            ]
        );
        assert_eq!(split_array("[]").unwrap(), Vec::<&str>::new());
        assert_eq!(to_cell("1E+2", &Ty::Decimal(4, 0), None), Cell::Decimal("100".into()));
        assert_eq!(to_cell("\"2024-01-31T13:45:00.000\"", &Ty::Timestamp, None), Cell::DateTime("2024-01-31 13:45:00".into()));
        assert_eq!(to_cell("3723456", &Ty::Time, None), Cell::Time("01:02:03.456".into()));
    }

    #[test]
    fn nested_times_come_from_the_plain_value() {
        // TO_JSON_STRING drops a nested TIME's milliseconds; the plain value
        // has them (and turns a STRUCT's decimals into doubles).
        let ty = arr(Ty::Struct(vec![("t".into(), Ty::Time), ("D".into(), Ty::Decimal(30, 10))]));
        let json = r#""[{\"D\":12345678901234567890.1234567890,\"t\":\"01:02:03\"}]""#;
        let plain = r#"[{"t":3723456,"D":1.2345678901234567E19}]"#;
        assert_eq!(to_cell(json, &ty, Some(plain)), Cell::Json(r#"[{"D":12345678901234567890.1234567890,"t":"01:02:03.456"}]"#.into()));
    }

    #[test]
    fn describe_schemas_become_types() {
        let s: Value = serde_json::from_str(
            r#"{"type":"STRUCT","fields":[{"name":"lo","schema":{"type":"INTEGER"}},{"name":"M","schema":{"type":"MAP","memberSchema":{"type":"ARRAY","memberSchema":{"type":"DECIMAL","parameters":{"precision":30,"scale":10}}}}}]}"#,
        )
        .unwrap();
        let ty = Ty::of(&s);
        assert_eq!(ty, Ty::Struct(vec![("lo".into(), Ty::Integer), ("M".into(), Ty::Map(Box::new(arr(Ty::Decimal(30, 10)))))]));
        assert_eq!(ty.name(), "STRUCT<`lo` INTEGER, `M` MAP<STRING, ARRAY<DECIMAL(30, 10)>>>");
        assert!(!ty.has_time());
    }

    fn w(c: Cell, t: &Ty) -> Result<String> {
        let mut s = String::new();
        write_value(&mut s, &c, t).map(|_| s)
    }

    #[test]
    fn cells_become_insert_json() {
        assert_eq!(w(Cell::Decimal("12345678901234567.1234".into()), &Ty::Decimal(21, 4)).unwrap(), "\"12345678901234567.1234\"");
        assert_eq!(w(Cell::Bytes(vec![0xca, 0xfe]), &Ty::Bytes).unwrap(), "\"yv4=\"");
        assert_eq!(w(Cell::DateTimeTz("2024-01-31 10:45:00.123-03:00".into()), &Ty::Timestamp).unwrap(), "\"2024-01-31T10:45:00.123-03:00\"");
        assert_eq!(w(Cell::DateTime("2024-01-31 00:00:00".into()), &Ty::Date).unwrap(), "\"2024-01-31\"");
        assert_eq!(w(Cell::Text("42".into()), &Ty::Bigint).unwrap(), "42");
        assert!(w(Cell::Text("x".into()), &Ty::Integer).is_err());
        assert!(w(Cell::Int(1 << 40), &Ty::Integer).is_err());
        assert_eq!(w(Cell::Json("[1,2]".into()), &arr(Ty::Integer)).unwrap(), "[1,2]");
        assert_eq!(w(Cell::Null, &Ty::Str).unwrap(), "null");
        assert_eq!(w(Cell::Text("a\"b".into()), &Ty::Str).unwrap(), "\"a\\\"b\"");
        assert_eq!(w(Cell::Float(0.1), &Ty::Double).unwrap(), "0.1");
        for n in 0..8 {
            let b: Vec<u8> = (0..n).map(|i| (i * 37 + 200) as u8).collect();
            assert_eq!(base64_decode(&base64_encode(&b)), Some(b));
        }
        assert_eq!(base64_encode(b"hola"), "aG9sYQ==");
    }

    #[test]
    fn nested_values_keep_digits_and_quoted_names() {
        // Decimals as text: as JSON numbers ksqlDB takes them through a double.
        assert_eq!(
            w(Cell::Json("[12345678901234567890.1234567890, null]".into()), &arr(Ty::Decimal(30, 10))).unwrap(),
            r#"["12345678901234567890.123456789",null]"#
        );
        // STRUCT fields quoted: a bare `lo` would be `LO`, silently null.
        let st = Ty::Struct(vec![("lo".into(), Ty::Integer), ("HI".into(), Ty::Decimal(30, 10)), ("t".into(), Ty::Time)]);
        assert_eq!(w(Cell::Json(r#"{"lo":1,"HI":2.5,"t":"01:02:03.456"}"#.into()), &st).unwrap(), r#"{"`lo`":1,"`HI`":"2.5","`t`":"01:02:03.456"}"#);
        // …by the name the column has.
        assert_eq!(w(Cell::Json(r#"{"LO":1}"#.into()), &st).unwrap(), r#"{"`lo`":1}"#);
        // A field the STRUCT doesn't have would be dropped.
        assert!(w(Cell::Json(r#"{"otro":1}"#.into()), &st).is_err());
        // MAP keys are data, not names.
        assert_eq!(w(Cell::Json(r#"{"a b":["Infinity",1.5]}"#.into()), &Ty::Map(Box::new(arr(Ty::Double)))).unwrap(), r#"{"a b":[1e400,1.5]}"#);
        assert!(w(Cell::Json("[1".into()), &arr(Ty::Integer)).is_err());
        assert!(w(Cell::Json(r#"["x"]"#.into()), &arr(Ty::Integer)).is_err());
    }

    #[test]
    fn decimals_that_dont_fit_are_errors() {
        let d = Ty::Decimal(38, 10);
        assert!(w(Cell::Decimal("99999999999999999999999999999".into()), &d).is_err());
        assert_eq!(w(Cell::Decimal("9999999999999999999999999999".into()), &d).unwrap(), "\"9999999999999999999999999999\"");
        assert!(w(Cell::Decimal("1.00000000001".into()), &d).is_err());
        assert_eq!(w(Cell::Decimal("1.00000000000000".into()), &d).unwrap(), "\"1\"");
        assert_eq!(w(Cell::Decimal("-0.50".into()), &d).unwrap(), "\"-0.5\"");
        assert_eq!(w(Cell::Decimal("1E+2".into()), &d).unwrap(), "\"100\"");
        assert_eq!(w(Cell::Int(-7), &d).unwrap(), "\"-7\"");
        assert!(w(Cell::Text("12a".into()), &d).is_err());
        assert!(w(Cell::Text("ñe5".into()), &d).is_err());
        assert!(w(Cell::Float(f64::NAN), &d).is_err());
    }

    #[test]
    fn decimal_huge_exponents_are_rejected_not_zeroed_nor_expanded() {
        let d = Ty::Decimal(38, 10);
        // Exponents past an i64 used to parse as 0: `1e…` stored as 1.
        for t in ["1e99999999999999999999", "-5E30000000000000000000", "1e-99999999999999999999"] {
            let e = decimal_text(t, 38, 10).unwrap_err().to_string();
            assert!(e.contains("no entra en DECIMAL(38, 10)"), "{t}: {e}");
            assert!(w(Cell::Decimal(t.into()), &d).is_err(), "{t}");
        }
        // Nested: the JSON parser takes it as a number (a double's inf).
        assert!(w(Cell::Json("[1e99999999999999999999]".into()), &arr(d.clone())).is_err());
        // Checked before expanding: these would be ~300 MB of zeros each.
        for t in ["1e300000000", "1e-300000000", "-9.5E+2000000000"] {
            assert!(decimal_text(t, 38, 10).is_err(), "{t}");
            assert!(decimal_text(t, 0, 0).unwrap_err().to_string().contains("dígitos"), "{t}");
        }
        // A zero mantissa is zero whatever the exponent.
        assert_eq!(decimal_text("0e99999999999999999999", 38, 10).unwrap(), "0");
        assert_eq!(decimal_text("-0.000e-400000000", 38, 10).unwrap(), "0");
        // Exponent forms that fit still expand exactly.
        assert_eq!(decimal_text("1.25e27", 38, 10).unwrap(), "1250000000000000000000000000");
        assert_eq!(decimal_text("-12.5E-9", 38, 10).unwrap(), "-0.0000000125");
        assert!(decimal_text("-12.5E-11", 38, 10).is_err());
        assert_eq!(decimal_text("0.00123e+3", 38, 10).unwrap(), "1.23");
        // Malformed exponents are not numbers (`1e5e5` used to give 1).
        for t in ["1e5e5", "1e", "1e+", "e5", "1e-+3"] {
            assert!(decimal_text(t, 38, 10).unwrap_err().to_string().contains("no es un número"), "{t}");
        }
        // The read side keeps unexpandable text as is.
        assert_eq!(plain_decimal("1E+300000000"), "1E+300000000");
        assert_eq!(plain_decimal("1.50E+1"), "15");
        assert_eq!(plain_decimal("1E-7"), "0.0000001");
    }

    #[test]
    fn times_keep_milliseconds_or_fail() {
        assert_eq!(w(Cell::Time("23:59:59.999".into()), &Ty::Time).unwrap(), "\"23:59:59.999\"");
        assert_eq!(w(Cell::Time("12:00:00.5".into()), &Ty::Time).unwrap(), "\"12:00:00.500\"");
        assert_eq!(w(Cell::Time("12:00:00.500000000".into()), &Ty::Time).unwrap(), "\"12:00:00.500\"");
        assert_eq!(w(Cell::Time("13:45".into()), &Ty::Time).unwrap(), "\"13:45:00\"");
        assert!(w(Cell::Time("12:00:00.0001".into()), &Ty::Time).is_err());
        assert!(w(Cell::Time("25:00:00".into()), &Ty::Time).is_err());
        assert!(w(Cell::DateTime("2024-01-31 10:00:00.1234567".into()), &Ty::Timestamp).is_err());
        assert_eq!(w(Cell::DateTime("2024-01-31 10:00:00.123000".into()), &Ty::Timestamp).unwrap(), "\"2024-01-31T10:00:00.123\"");
        assert_eq!(w(Cell::DateTimeTz("2024-01-31 10:00:00+0300".into()), &Ty::Timestamp).unwrap(), "\"2024-01-31T10:00:00+03:00\"");
        assert_eq!(w(Cell::Date("2024-01-31".into()), &Ty::Timestamp).unwrap(), "\"2024-01-31T00:00:00\"");
        assert!(w(Cell::Text("garbage".into()), &Ty::Timestamp).is_err());
        assert!(w(Cell::DateTime("2024-01-31 10:45:00".into()), &Ty::Date).is_err());
        assert!(w(Cell::Text("2024-13-01".into()), &Ty::Date).is_err());
        // ksqlDB rejects these itself, after taking the rows before them.
        assert!(w(Cell::Date("2023-02-29".into()), &Ty::Date).is_err());
        assert_eq!(w(Cell::Date("2024-02-29".into()), &Ty::Date).unwrap(), "\"2024-02-29\"");
        assert!(w(Cell::Date("2024-04-31".into()), &Ty::Date).is_err());
        assert!(w(Cell::DateTime("0000-01-01 00:00:00".into()), &Ty::Timestamp).is_err());
        assert_eq!(w(Cell::Date("0000-01-01".into()), &Ty::Date).unwrap(), "\"0000-01-01\"");
    }

    #[test]
    fn infinities_load_and_nan_is_unsupported() {
        assert_eq!(w(Cell::Float(f64::INFINITY), &Ty::Double).unwrap(), "1e400");
        assert_eq!(w(Cell::Float(f64::NEG_INFINITY), &Ty::Double).unwrap(), "-1e400");
        assert!(matches!(w(Cell::Float(f64::NAN), &Ty::Double), Err(Error::Unsupported(_))));
        assert!(matches!(w(Cell::Json(r#"["NaN"]"#.into()), &arr(Ty::Double)), Err(Error::Unsupported(_))));
        assert!(w(Cell::Int((1 << 53) + 1), &Ty::Double).is_err());
    }

    #[test]
    fn reads_quote_names_and_keep_values_whole() {
        let c = |name: &str, ty: Ty| Column { name: name.into(), type_name: String::new(), ty, key: false };
        let cols = [c("id", Ty::Bigint), c("T", Ty::Time), c("a", arr(Ty::Time)), c("s", Ty::Struct(vec![("x".into(), Ty::Integer)]))];
        let refs: Vec<&Column> = cols.iter().collect();
        assert_eq!(
            read_sql("rev`v", Some(" `id` > 1 "), &refs),
            "SELECT `id`, FORMAT_TIME(`T`, 'HH:mm:ss.SSS') AS `T`, TO_JSON_STRING(`a`) AS `a`, TO_JSON_STRING(`s`) AS `s`, `a` AS `dbine plain 0` FROM `rev``v` WHERE `id` > 1;"
        );
        assert!(find(&cols, "ID").is_some());
        assert!(find(&cols, "nada").is_none());
    }

    #[test]
    fn json_keeps_number_text() {
        let j = parse_j(r#" {"a": [1.10, -2e3, "x\"y", true, null, {}], "b": []} "#).unwrap();
        let mut s = String::new();
        put_j(&mut s, &j);
        assert_eq!(s, r#"{"a":[1.10,-2e3,"x\"y",true,null,{}],"b":[]}"#);
        assert!(parse_j("[1,]").is_none());
        assert!(parse_j("{\"a\" 1}").is_none());
        assert!(parse_j("[1] x").is_none());
    }
}
