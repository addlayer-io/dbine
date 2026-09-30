//! Bulk transfer (see `dbine_driver::transfer`).
//!
//! - Reading: a base table without a filter is read with `tabledata.list`
//!   (free: no query job, no bytes billed), page by page. A view, a
//!   materialized view, an external table or a read with a filter runs one
//!   `SELECT` (only the given columns, plus the filter; nothing else runs
//!   on the source) and pages its results with `jobs.getQueryResults`; that
//!   one is billed as any query (bytes of the columns read). Either way one
//!   page (up to [`PAGE`] rows, ~10 MB) is in memory at a time. Each value
//!   becomes its typed cell by the schema field: `INT64` → integer,
//!   `NUMERIC` / `BIGNUMERIC` → exact decimal (the server's digits),
//!   `FLOAT64` → float, `BYTES` → bytes (whole), `DATE` / `TIME` /
//!   `DATETIME` → date, time, date-time, `TIMESTAMP` → date-time with
//!   `+00:00` (microseconds, exact), `JSON` → JSON, `STRUCT` and `ARRAY`
//!   → JSON (bytes inside them in base64, timestamps as UTC text: the same
//!   shape a load takes back), anything else (`GEOGRAPHY` as WKT,
//!   `INTERVAL`, `RANGE`) → text.
//! - Loading: a load job per commit window, from newline-delimited JSON
//!   sent with a resumable upload (`/upload/bigquery/v2/…/jobs`, one `PUT`
//!   per window). Load jobs are free (they use the shared slot pool),
//!   atomic (a window lands whole or not at all) and leave the rows ready
//!   for DML right away. Streaming inserts (`tabledata.insertAll`) were not
//!   used: they are billed per byte, are only best-effort deduplicated, may
//!   land part of a request, and keep the rows in the streaming buffer,
//!   where `UPDATE` / `DELETE` / `MERGE` / truncation can't touch them for
//!   up to ~90 minutes.
//! - Windows: `commit_rows` / `commit_bytes`, with at most [`LOAD_BYTES`]
//!   of JSON per window (it's held in memory and sent in one request; two
//!   windows at most per table, ~16 MiB, inside the transfer's ~32 MiB
//!   per table). While a window's job runs, the next window is read and
//!   encoded. `progress` gets the total after each job finishes. Mind the
//!   quota of 1,500 load jobs per table per day: with 100,000-row windows
//!   that's 150 million rows (and at most ~12 GB of JSON) per table per day.
//! - Each job gets its id before the upload starts, so it can be cancelled
//!   (interrupter, or the load dropped) and found again when the upload's
//!   answer is lost. A load never returns while a job it started may still
//!   run, as far as the API answers; and before loading, DBine's own load
//!   jobs still running on the table (left by a killed process) are
//!   cancelled and waited for, and the load fails if one of them landed.
//! - The job appends (`WRITE_APPEND`) and never creates the table
//!   (`CREATE_NEVER`), with no bad records allowed. Values go by the
//!   target column's type: bytes in base64, big integers and decimals as
//!   JSON strings (exact), non-finite floats as `"NaN"` / `"Infinity"`,
//!   JSON / `STRUCT` / `ARRAY` as nested JSON written as the source's text
//!   (never re-encoded: numbers keep every digit), NULLs left out. Values
//!   that can't go faithfully are an error, not a guess: bytes that aren't
//!   UTF-8 into `STRING`, text that isn't JSON into `JSON` / `STRUCT` /
//!   `ARRAY`, numbers into `BYTES`. Temporal values keep up to
//!   microseconds (BigQuery's precision); a date-time with an offset loaded
//!   into `DATETIME`, `DATE` or `TIME` is taken to UTC first.
//! - A filtered read is refused unless the filter is one condition: no
//!   `;` outside literals, and a dry run (free) must see one `SELECT`
//!   (`jobs.query` runs scripts, and the source is read-only).
//! - `table_lock` and `keep_identity` have no equivalent: BigQuery has no
//!   table locks nor identity columns.

use super::{is_record, is_repeated, str_of, BigQuerySession, JobRef, PAGE, WAIT_MS};
use crate::ddl::ident;
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{json_i64, json_u64, Error, Result};
use serde_json::{json, Value as Json};
use std::time::Duration;

/// JSON bytes of one load window at most (sent in one request). The
/// window being loaded and the next one being encoded stay under the
/// transfer's ~32 MiB per table.
pub(crate) const LOAD_BYTES: usize = 8 * 1024 * 1024;

/// Prefix of the ids of DBine's load jobs (to find its own orphans).
const JOB_PREFIX: &str = "dbine_load_";

// ---------------------------------------------------------------- reading

/// How a result value is read.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Read {
    Int,
    Float,
    Decimal,
    Bool,
    Bytes,
    Date,
    Time,
    DateTime,
    Timestamp,
    Json,
    /// `STRUCT` / `ARRAY`.
    Nested,
    Text,
}

fn base(f: &Json) -> String {
    str_of(f, "type").to_ascii_uppercase()
}

pub(crate) fn read_kind(f: &Json) -> Read {
    if is_repeated(f) || is_record(f) {
        return Read::Nested;
    }
    scalar_kind(f)
}

/// The kind of the field's values, ignoring `REPEATED` (an array's
/// elements).
fn scalar_kind(f: &Json) -> Read {
    match base(f).as_str() {
        "INTEGER" | "INT64" => Read::Int,
        "FLOAT" | "FLOAT64" => Read::Float,
        "NUMERIC" | "BIGNUMERIC" | "BIGDECIMAL" | "DECIMAL" => Read::Decimal,
        "BOOLEAN" | "BOOL" => Read::Bool,
        "BYTES" => Read::Bytes,
        "DATE" => Read::Date,
        "TIME" => Read::Time,
        "DATETIME" => Read::DateTime,
        "TIMESTAMP" => Read::Timestamp,
        "JSON" => Read::Json,
        _ => Read::Text,
    }
}

/// A number the API gives as a string (`"10"`) or a number.
fn num(f: &Json, key: &str) -> Option<String> {
    match f.get(key)? {
        Json::String(s) if !s.is_empty() => Some(s.clone()),
        Json::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// A schema field's type in GoogleSQL (`INT64`, `NUMERIC(10, 2)`,
/// `ARRAY<STRUCT<a INT64, b STRING>>`…).
pub(crate) fn field_type(f: &Json) -> String {
    let t = base(f);
    let one = match t.as_str() {
        "INTEGER" | "INT64" => "INT64".to_string(),
        "FLOAT" | "FLOAT64" => "FLOAT64".to_string(),
        "BOOLEAN" | "BOOL" => "BOOL".to_string(),
        "RECORD" | "STRUCT" => {
            let sub = f.get("fields").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
            let parts: Vec<String> = sub.iter().map(|s| format!("{} {}", ident(str_of(s, "name")), field_type(s))).collect();
            format!("STRUCT<{}>", parts.join(", "))
        }
        "NUMERIC" | "BIGNUMERIC" => match (num(f, "precision"), num(f, "scale")) {
            (Some(p), Some(s)) => format!("{t}({p}, {s})"),
            (Some(p), None) => format!("{t}({p})"),
            _ => t,
        },
        "STRING" | "BYTES" => num(f, "maxLength").map_or(t.clone(), |l| format!("{t}({l})")),
        "RANGE" => format!("RANGE<{}>", f.pointer("/rangeElementType/type").and_then(Json::as_str).unwrap_or("DATE")),
        _ => t,
    };
    if is_repeated(f) {
        format!("ARRAY<{one}>")
    } else {
        one
    }
}

pub(crate) fn transfer_column(f: &Json) -> TransferColumn {
    TransferColumn { name: str_of(f, "name").to_string(), type_name: field_type(f), nullable: str_of(f, "mode") != "REQUIRED" }
}

/// Epoch microseconds from a TIMESTAMP value: integer microseconds (with
/// `useInt64Timestamp`) or seconds with a fraction (exact, not through a
/// float).
pub(crate) fn epoch_micros(s: &str) -> Option<i64> {
    if s.contains(['e', 'E']) {
        return Some((s.parse::<f64>().ok()? * 1e6).round() as i64);
    }
    let Some((sec, frac)) = s.split_once('.') else { return s.parse().ok() };
    let secs: i64 = sec.parse().ok()?;
    let f: i64 = if frac.is_empty() { 0 } else { format!("{frac:0<6}").get(..6)?.parse().ok()? };
    let f = if sec.starts_with('-') { -f } else { f };
    secs.checked_mul(1_000_000)?.checked_add(f)
}

/// `.ffffff` without trailing zeros (nothing for whole seconds).
fn fraction(micros: u32) -> String {
    if micros == 0 {
        return String::new();
    }
    format!(".{micros:06}").trim_end_matches('0').to_string()
}

/// `YYYY-MM-DD HH:MM:SS[.ffffff]` in UTC.
fn utc_text(micros: i64) -> Option<String> {
    let t = chrono::DateTime::from_timestamp_micros(micros)?;
    Some(format!("{}{}", t.format("%Y-%m-%d %H:%M:%S"), fraction(t.timestamp_subsec_micros())))
}

fn b64_decode(s: &str) -> Option<Vec<u8>> {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.decode(s).ok()
}

fn b64(b: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(b)
}

/// One API value (text, or null) as a lossless cell.
pub(crate) fn read_cell(kind: Read, v: &Json, f: &Json) -> Cell {
    if v.is_null() {
        return Cell::Null;
    }
    if kind == Read::Nested {
        let mut out = String::new();
        nested(v, f, is_repeated(f), &mut out);
        return Cell::Json(out);
    }
    let Some(s) = v.as_str() else { return Cell::from_json(v) };
    let text = || Cell::Text(s.to_string());
    match kind {
        Read::Int => s.parse::<i64>().map_or_else(|_| Cell::Decimal(s.to_string()), Cell::Int),
        Read::Float => s.parse::<f64>().map_or_else(|_| text(), Cell::Float),
        Read::Decimal => Cell::Decimal(s.to_string()),
        Read::Bool => Cell::Bool(s.eq_ignore_ascii_case("true")),
        Read::Bytes => b64_decode(s).map_or_else(text, Cell::Bytes),
        Read::Date => Cell::Date(s.to_string()),
        Read::Time => Cell::Time(s.to_string()),
        Read::DateTime => Cell::DateTime(s.replacen('T', " ", 1)),
        Read::Timestamp => epoch_micros(s).and_then(utc_text).map_or_else(text, |t| Cell::DateTimeTz(format!("{t}+00:00"))),
        Read::Json => Cell::Json(s.to_string()),
        Read::Nested | Read::Text => text(),
    }
}

/// Whether `s` is one JSON document (checked without building it, so
/// nothing is re-encoded).
fn is_json(s: &str) -> bool {
    serde_json::from_str::<serde::de::IgnoredAny>(s).is_ok()
}

/// A valid JSON document's text as is, on one line: a raw line break in
/// valid JSON can only be whitespace (inside strings it's escaped).
fn push_raw_json(out: &mut String, s: &str) {
    if s.contains(['\n', '\r']) {
        out.extend(s.chars().map(|c| if c == '\n' || c == '\r' { ' ' } else { c }));
    } else {
        out.push_str(s);
    }
}

fn push_json(out: &mut String, v: &Json) {
    out.push_str(&v.to_string());
}

/// A `STRUCT` / `ARRAY` value as JSON text, in the shape a load takes
/// back: records as objects (fields in the schema's order), bytes in
/// base64, timestamps as UTC text, integers as numbers (strings past
/// 2^53), JSON fields nested with their text as the server gave it (no
/// digit lost to a float).
pub(crate) fn nested(v: &Json, f: &Json, repeated: bool, out: &mut String) {
    if v.is_null() {
        out.push_str("null");
        return;
    }
    if repeated {
        out.push('[');
        for (i, e) in v.as_array().into_iter().flatten().enumerate() {
            if i > 0 {
                out.push(',');
            }
            nested(e.get("v").unwrap_or(&Json::Null), f, false, out);
        }
        out.push(']');
        return;
    }
    if is_record(f) {
        let sub = f.get("fields").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
        let vals = v.get("f").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
        out.push('{');
        for (i, sf) in sub.iter().enumerate() {
            if i > 0 {
                out.push(',');
            }
            push_json(out, &Json::String(str_of(sf, "name").to_string()));
            out.push(':');
            let sv = vals.get(i).and_then(|c| c.get("v")).unwrap_or(&Json::Null);
            nested(sv, sf, is_repeated(sf), out);
        }
        out.push('}');
        return;
    }
    let Some(s) = v.as_str() else { return push_json(out, v) };
    let value = match scalar_kind(f) {
        Read::Int => s.parse::<i64>().map_or_else(|_| s.into(), json_i64),
        Read::Float => match s.parse::<f64>() {
            Ok(x) if x.is_finite() => dbine_driver::json_f64(x),
            _ => s.into(),
        },
        Read::Bool => Json::Bool(s.eq_ignore_ascii_case("true")),
        Read::Timestamp => epoch_micros(s).and_then(utc_text).map_or_else(|| s.into(), |t| Json::String(format!("{t}+00:00"))),
        Read::DateTime => s.replacen('T', " ", 1).into(),
        Read::Json if is_json(s) => return push_raw_json(out, s),
        // Bytes stay in the API's base64.
        _ => s.into(),
    };
    push_json(out, &value);
}

/// `SELECT` of the read: only the given columns, in their order, only the
/// filter's rows.
pub(crate) fn select_sql(dataset: &str, spec: &ReadSpec) -> String {
    let cols = match &spec.columns {
        Some(c) if !c.is_empty() => c.iter().map(|c| ident(c)).collect::<Vec<_>>().join(", "),
        _ => "*".to_string(),
    };
    let mut sql = format!("SELECT {cols} FROM {}.{}", ident(dataset), ident(&spec.table.name));
    if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
        sql.push_str(&format!(" WHERE {f}"));
    }
    sql
}

/// A read's filter must be one condition of the `SELECT`: `jobs.query`
/// runs multi-statement scripts, so a `;` outside literals, quoted names
/// and comments (`1=1; DELETE …`) would run more on the read-only source.
/// Unclosed literals or comments are refused too (they'd hide the rest).
pub(crate) fn check_filter(filter: &str) -> Result<()> {
    let unclosed = || Error::Query("el filtro tiene un texto, un nombre entre comillas o un comentario sin cerrar".into());
    let b = filter.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b';' => {
                return Err(Error::Query(
                    "el filtro no puede tener «;»: la lectura del origen es una sola consulta SELECT y el filtro, solo su condición".into(),
                ))
            }
            b'#' => i = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p),
            b'-' if b.get(i + 1) == Some(&b'-') => i = b[i..].iter().position(|&c| c == b'\n').map_or(b.len(), |p| i + p),
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i = b[i + 2..].windows(2).position(|w| w == b"*/").map(|p| i + 2 + p + 1).ok_or_else(unclosed)?;
            }
            q @ (b'\'' | b'"' | b'`') => {
                // Raw literals keep `\x` as is, but a `\` still keeps the
                // next quote from closing them, as in the others.
                let triple = q != b'`' && b.get(i..i + 3) == Some(&[q, q, q][..]);
                let mut j = if triple { i + 3 } else { i + 1 };
                loop {
                    match b.get(j) {
                        None => return Err(unclosed()),
                        Some(b'\\') => j += 2,
                        Some(&c) if c == q && (!triple || b.get(j..j + 3) == Some(&[q, q, q][..])) => {
                            i = if triple { j + 2 } else { j };
                            break;
                        }
                        Some(_) => j += 1,
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    Ok(())
}

/// Positions in the table's schema of the read's columns (all, or the
/// given ones in their order; names are case-insensitive in BigQuery).
pub(crate) fn pick(fields: &[Json], columns: Option<&[String]>) -> Result<Vec<usize>> {
    match columns {
        Some(names) if !names.is_empty() => names
            .iter()
            .map(|n| {
                fields
                    .iter()
                    .position(|f| str_of(f, "name").eq_ignore_ascii_case(n))
                    .ok_or_else(|| Error::Query(format!("la tabla no tiene la columna «{n}»")))
            })
            .collect(),
        _ => Ok((0..fields.len()).collect()),
    }
}

/// `tabledata.list`'s `selectedFields` for a subset of the columns.
#[derive(Debug)]
pub(crate) struct Selected {
    /// Comma-separated, in the schema's order.
    names: String,
    /// Their positions in the schema, ascending.
    idx: Vec<usize>,
}

/// `None` when every column is read (or a name can't be listed).
pub(crate) fn selected_fields(fields: &[Json], pick: &[usize]) -> Option<Selected> {
    let mut idx = pick.to_vec();
    idx.sort_unstable();
    idx.dedup();
    if idx.len() == fields.len() {
        return None;
    }
    let names: Vec<&str> = idx.iter().map(|&i| str_of(&fields[i], "name")).collect();
    if names.iter().any(|n| n.is_empty() || n.contains([',', '.'])) {
        return None;
    }
    Some(Selected { names: names.join(","), idx })
}

impl Selected {
    /// Rows of the selected fields back at their schema positions (the
    /// rest NULL, never read), so the read indexes them as a whole row. A
    /// server that ignores `selectedFields` sends whole rows: kept as they
    /// are.
    pub(crate) fn narrow(&self, page: &mut Json, width: usize) -> Result<()> {
        let Some(rows) = page.get_mut("rows").and_then(Json::as_array_mut) else { return Ok(()) };
        for row in rows {
            let Some(vals) = row.get_mut("f").and_then(Json::as_array_mut) else { continue };
            if vals.len() == width {
                continue;
            }
            if vals.len() != self.idx.len() {
                return Err(Error::Query(format!(
                    "BigQuery devolvió filas de {} columnas y se pidieron {} de {width}",
                    vals.len(),
                    self.idx.len()
                )));
            }
            let mut full = vec![Json::Null; width];
            for (&i, v) in self.idx.iter().zip(vals.drain(..)) {
                full[i] = v;
            }
            *vals = full;
        }
        Ok(())
    }
}

fn lock(sink: &BatchSinkRef) -> Result<std::sync::MutexGuard<'_, dyn dbine_driver::transfer::BatchSink + 'static>> {
    sink.lock().map_err(|_| Error::State("destino de lotes".into()))
}

/// Rows of one page into the builder (the sink may block: never across an
/// await).
fn push_page(page: &mut Json, fields: &[Json], pick: &[usize], kinds: &[Read], builder: &mut BatchBuilder, sink: &BatchSinkRef) -> Result<()> {
    let rows = match page.get_mut("rows").map(Json::take) {
        Some(Json::Array(rows)) => rows,
        _ => return Ok(()),
    };
    let mut g = lock(sink)?;
    for row in rows {
        let vals = row.get("f").and_then(Json::as_array).map_or(&[][..], Vec::as_slice);
        let cells = pick
            .iter()
            .zip(kinds)
            .map(|(&i, &k)| read_cell(k, vals.get(i).and_then(|c| c.get("v")).unwrap_or(&Json::Null), &fields[i]))
            .collect();
        builder.push(cells, &mut *g)?;
    }
    Ok(())
}

fn next_token(page: &Json) -> Option<String> {
    page.get("pageToken").and_then(Json::as_str).filter(|t| !t.is_empty()).map(str::to_string)
}

pub(crate) async fn read_batches(s: &BigQuerySession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    let ds = s.dataset(&spec.table)?;
    let filtered = spec.filter.as_deref().is_some_and(|f| !f.trim().is_empty());
    if !filtered {
        let meta = s.api.get(&["datasets", &ds, "tables", &spec.table.name], &[]).await?;
        if meta.get("type").and_then(Json::as_str).unwrap_or("TABLE") == "TABLE" {
            return read_table(s, &ds, spec, &meta, sink).await;
        }
    }
    let sql = select_sql(&ds, spec);
    if filtered {
        check_filter(spec.filter.as_deref().unwrap_or_default())?;
        // The dry run is free; a script shows as `SCRIPT`, DML as its verb.
        let dry = s.dry_run(&sql).await?;
        let kind = dry.pointer("/statistics/query/statementType").and_then(Json::as_str).unwrap_or("");
        if kind != "SELECT" {
            return Err(Error::Query(format!(
                "el filtro tiene que ser la condición de un solo SELECT y BigQuery lo lee como «{}»: el origen es solo lectura",
                if kind.is_empty() { "desconocido" } else { kind }
            )));
        }
    }
    s.set_job(None);
    let r = read_query(s, &sql, sink).await;
    s.set_job(None);
    r
}

/// A base table through `tabledata.list`.
async fn read_table(s: &BigQuerySession, ds: &str, spec: &ReadSpec, meta: &Json, sink: BatchSinkRef) -> Result<u64> {
    let fields = meta.pointer("/schema/fields").and_then(Json::as_array).cloned().unwrap_or_default();
    let pick = pick(&fields, spec.columns.as_deref())?;
    let kinds: Vec<Read> = pick.iter().map(|&i| read_kind(&fields[i])).collect();
    let columns: Vec<TransferColumn> = pick.iter().map(|&i| transfer_column(&fields[i])).collect();
    // Only the read's columns are downloaded (in the schema's order).
    let selected = selected_fields(&fields, &pick);
    lock(&sink)?.begin(&columns)?;
    let mut builder = BatchBuilder::new();
    let mut token: Option<String> = None;
    loop {
        let mut q = vec![("maxResults", PAGE.to_string()), ("formatOptions.useInt64Timestamp", "true".to_string())];
        if let Some(sel) = &selected {
            q.push(("selectedFields", sel.names.clone()));
        }
        if let Some(t) = token.take() {
            q.push(("pageToken", t));
        }
        let mut page = s.api.get(&["datasets", ds, "tables", &spec.table.name, "data"], &q).await?;
        if let Some(sel) = &selected {
            sel.narrow(&mut page, fields.len())?;
        }
        push_page(&mut page, &fields, &pick, &kinds, &mut builder, &sink)?;
        match next_token(&page) {
            Some(t) => token = Some(t),
            None => break,
        }
    }
    builder.flush(&mut *lock(&sink)?)?;
    Ok(builder.rows)
}

/// A `SELECT` job, its results paged with `jobs.getQueryResults`.
async fn read_query(s: &BigQuerySession, sql: &str, sink: BatchSinkRef) -> Result<u64> {
    let mut body = json!({
        "query": sql,
        "useLegacySql": false,
        "timeoutMs": WAIT_MS,
        "maxResults": PAGE,
        "formatOptions": { "useInt64Timestamp": true },
    });
    if let Some(l) = &s.api.location {
        body["location"] = json!(l);
    }
    let mut page = s.api.post(&["queries"], &[], &body).await?;
    let job = JobRef {
        id: page.pointer("/jobReference/jobId").and_then(Json::as_str).unwrap_or_default().to_string(),
        location: page.pointer("/jobReference/location").and_then(Json::as_str).map(str::to_string).or_else(|| s.api.location.clone()),
    };
    s.set_job(Some(job.clone()));
    let results = |token: Option<String>| {
        let mut q = vec![
            ("timeoutMs", WAIT_MS.to_string()),
            ("maxResults", PAGE.to_string()),
            ("formatOptions.useInt64Timestamp", "true".to_string()),
        ];
        if let Some(l) = &job.location {
            q.push(("location", l.clone()));
        }
        if let Some(t) = token {
            q.push(("pageToken", t));
        }
        q
    };
    while page.get("jobComplete").and_then(Json::as_bool) == Some(false) {
        page = s.api.get(&["queries", &job.id], &results(None)).await?;
    }
    let fields = page.pointer("/schema/fields").and_then(Json::as_array).cloned().unwrap_or_default();
    let pick: Vec<usize> = (0..fields.len()).collect();
    let kinds: Vec<Read> = fields.iter().map(read_kind).collect();
    let columns: Vec<TransferColumn> = fields.iter().map(transfer_column).collect();
    lock(&sink)?.begin(&columns)?;
    let mut builder = BatchBuilder::new();
    loop {
        push_page(&mut page, &fields, &pick, &kinds, &mut builder, &sink)?;
        match next_token(&page) {
            Some(t) => page = s.api.get(&["queries", &job.id], &results(Some(t))).await?,
            None => break,
        }
    }
    builder.flush(&mut *lock(&sink)?)?;
    Ok(builder.rows)
}

// ---------------------------------------------------------------- loading

/// How a value goes into a target column.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Load {
    Int,
    Float,
    Numeric,
    Bool,
    Str,
    Bytes,
    Date,
    Time,
    DateTime,
    Timestamp,
    Json,
    /// `STRUCT` / `ARRAY`: nested JSON.
    Nested,
    /// `GEOGRAPHY` (WKT / GeoJSON), `INTERVAL`, `RANGE`: text.
    Other,
}

pub(crate) fn load_kind(f: &Json) -> Load {
    if is_repeated(f) || is_record(f) {
        return Load::Nested;
    }
    match read_kind(f) {
        Read::Int => Load::Int,
        Read::Float => Load::Float,
        Read::Decimal => Load::Numeric,
        Read::Bool => Load::Bool,
        Read::Bytes => Load::Bytes,
        Read::Date => Load::Date,
        Read::Time => Load::Time,
        Read::DateTime => Load::DateTime,
        Read::Timestamp => Load::Timestamp,
        Read::Json => Load::Json,
        Read::Nested => Load::Nested,
        Read::Text => {
            if base(f) == "STRING" {
                Load::Str
            } else {
                Load::Other
            }
        }
    }
}

/// A target column: its `"name":` key, ready to write, and how it loads.
#[derive(Debug, Clone)]
pub(crate) struct Target {
    key: Vec<u8>,
    load: Load,
}

/// The load's columns in the target's schema (case-insensitive), in the
/// order of the batches' cells.
pub(crate) fn targets(fields: &[Json], columns: &[String]) -> Result<Vec<Target>> {
    columns
        .iter()
        .map(|c| {
            let f = fields
                .iter()
                .find(|f| str_of(f, "name").eq_ignore_ascii_case(c))
                .ok_or_else(|| Error::Query(format!("la tabla destino no tiene la columna «{c}»")))?;
            let mut key = serde_json::to_vec(str_of(f, "name"))?;
            key.push(b':');
            Ok(Target { key, load: load_kind(f) })
        })
        .collect()
}

/// Temporal text cut to microseconds (`12:00:00.123456789+02:00` →
/// `12:00:00.123456+02:00`).
pub(crate) fn micros_text(s: &str) -> String {
    let Some(dot) = s.find('.') else { return s.to_string() };
    let digits = s[dot + 1..].bytes().take_while(u8::is_ascii_digit).count();
    if digits <= 6 {
        return s.to_string();
    }
    format!("{}{}", &s[..dot + 7], &s[dot + 1 + digits..])
}

/// A date-time with an offset (`+02:00`, `+0200`, `Z`; a space or a `T`
/// before the time) taken to UTC, without it.
fn to_utc(s: &str) -> Option<String> {
    let t = chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%:z")
        .or_else(|_| chrono::DateTime::parse_from_str(s, "%Y-%m-%d %H:%M:%S%.f%#z"))
        .or_else(|_| chrono::DateTime::parse_from_rfc3339(&s.replacen(' ', "T", 1)))
        .ok()?;
    utc_text(t.timestamp_micros())
}

fn uuid_bytes(s: &str) -> Option<Vec<u8>> {
    let h: String = s.chars().filter(|c| *c != '-').collect();
    if h.len() != 32 {
        return None;
    }
    (0..32).step_by(2).map(|i| u8::from_str_radix(&h[i..i + 2], 16).ok()).collect()
}

/// A value ready for its NDJSON line.
#[derive(Debug, PartialEq)]
pub(crate) enum Out {
    /// Encoded by serde.
    Val(Json),
    /// A valid JSON document's text, written as is (never re-encoded, so
    /// no number loses digits).
    Raw(String),
}

/// A cell as the NDJSON value of a column of `load`; `None` for NULL (left
/// out of the line). What can't go faithfully is an error, never a guess.
pub(crate) fn load_value(cell: Cell, load: Load) -> Result<Option<Out>> {
    let json = matches!(load, Load::Json | Load::Nested);
    let v: Json = match cell {
        Cell::Null => return Ok(None),
        Cell::Bool(_) | Cell::Int(_) | Cell::UInt(_) | Cell::Float(_) if load == Load::Bytes => {
            return Err(Error::Query(
                "un número o un booleano no se carga en una columna BYTES: no hay una única forma de pasarlo a bytes".into(),
            ))
        }
        Cell::Bool(b) => match load {
            Load::Int | Load::Float | Load::Numeric => json!(b as i64),
            Load::Str | Load::Other => b.to_string().into(),
            _ => Json::Bool(b),
        },
        Cell::Int(i) => match load {
            Load::Bool => Json::Bool(i != 0),
            Load::Str | Load::Other | Load::Numeric => i.to_string().into(),
            _ => json_i64(i),
        },
        Cell::UInt(u) => match load {
            Load::Bool => Json::Bool(u != 0),
            Load::Str | Load::Other | Load::Numeric => u.to_string().into(),
            _ => json_u64(u),
        },
        Cell::Float(f) => match load {
            Load::Str | Load::Other => f.to_string().into(),
            _ if f.is_nan() => "NaN".into(),
            _ if f.is_infinite() => (if f > 0.0 { "Infinity" } else { "-Infinity" }).into(),
            _ => dbine_driver::json_f64(f),
        },
        Cell::Bytes(b) => match load {
            // One encoding for the whole column: text, or an error (never
            // text for some rows and base64 for others).
            Load::Str | Load::Other => String::from_utf8(b)
                .map_err(|_| {
                    Error::Query("hay bytes que no son texto UTF-8 y la columna destino es de texto: no se pueden cargar sin cambiarlos".into())
                })?
                .into(),
            _ => b64(&b).into(),
        },
        Cell::Uuid(s) if load == Load::Bytes => uuid_bytes(&s).map_or_else(|| b64(s.as_bytes()), |b| b64(&b)).into(),
        // Any text into BYTES is its UTF-8 bytes (never read as base64).
        Cell::Text(s) | Cell::Decimal(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Json(s)
            if load == Load::Bytes =>
        {
            b64(s.as_bytes()).into()
        }
        // Text into JSON / STRUCT / ARRAY is a JSON document: always, so
        // the value's type never depends on its content.
        Cell::Text(s) | Cell::Json(s) if json => {
            if !is_json(&s) {
                return Err(Error::Query(format!(
                    "hay un valor que no es JSON válido y la columna destino es {}",
                    if load == Load::Json { "JSON" } else { "STRUCT o ARRAY" }
                )));
            }
            return Ok(Some(Out::Raw(s)));
        }
        Cell::Time(s) => micros_text(&s).into(),
        Cell::DateTime(s) => match load {
            Load::Date => s.get(..10).unwrap_or(&s).into(),
            _ => micros_text(&s).into(),
        },
        Cell::DateTimeTz(s) => match load {
            // The same instant whatever the target: its UTC date and time.
            Load::DateTime | Load::Date | Load::Time => {
                let utc = to_utc(&s).ok_or_else(|| Error::Query("no se pudo interpretar una fecha y hora con zona para pasarla a UTC".into()))?;
                match load {
                    Load::Date => utc[..10].into(),
                    Load::Time => utc[11..].into(),
                    _ => utc.into(),
                }
            }
            _ => micros_text(&s).into(),
        },
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Uuid(s) | Cell::Json(s) => s.into(),
    };
    Ok(Some(Out::Val(v)))
}

/// One NDJSON line (NULLs left out).
pub(crate) fn write_row(buf: &mut Vec<u8>, targets: &[Target], row: Vec<Cell>) -> Result<()> {
    if row.len() != targets.len() {
        return Err(Error::Query(format!("el lote trae {} valores por fila y la carga espera {}", row.len(), targets.len())));
    }
    buf.push(b'{');
    let mut first = true;
    for (cell, t) in row.into_iter().zip(targets) {
        let Some(v) = load_value(cell, t.load)? else { continue };
        if !first {
            buf.push(b',');
        }
        first = false;
        buf.extend_from_slice(&t.key);
        match v {
            Out::Val(v) => serde_json::to_writer(&mut *buf, &v)?,
            // A raw line break in valid JSON is whitespace: one line.
            Out::Raw(s) => buf.extend(s.bytes().map(|c| if c == b'\n' || c == b'\r' { b' ' } else { c })),
        }
    }
    buf.extend_from_slice(b"}\n");
    Ok(())
}

/// The job configuration of one window.
pub(crate) fn load_config(project: &str, dataset: &str, table: &str) -> Json {
    json!({
        "configuration": {
            "load": {
                "destinationTable": { "projectId": project, "datasetId": dataset, "tableId": table },
                "sourceFormat": "NEWLINE_DELIMITED_JSON",
                "writeDisposition": "WRITE_APPEND",
                "createDisposition": "CREATE_NEVER",
                "maxBadRecords": 0,
                "ignoreUnknownValues": false,
            }
        }
    })
}

/// The reason a finished job failed, if it did.
pub(crate) fn job_error(job: &Json) -> Option<String> {
    let first = job.pointer("/status/errorResult")?;
    let mut msg = first.get("message").and_then(Json::as_str).unwrap_or("error desconocido").to_string();
    let detail: Vec<&str> = job
        .pointer("/status/errors")
        .and_then(Json::as_array)
        .into_iter()
        .flatten()
        .filter_map(|e| e.get("message").and_then(Json::as_str))
        .filter(|m| *m != msg)
        .take(3)
        .collect();
    if !detail.is_empty() {
        msg.push_str(&format!(" ({})", detail.join("; ")));
    }
    Some(msg)
}

fn is_done(job: &Json) -> bool {
    job.pointer("/status/state").or_else(|| job.get("state")).and_then(Json::as_str) == Some("DONE")
}

/// Rows read from the source into load windows, a batch possibly split
/// between two windows.
struct Windows<'a> {
    source: &'a mut dyn BatchSource,
    /// Rows of a batch that didn't fit the last window.
    pending: Option<Vec<Vec<Cell>>>,
    targets: Vec<Target>,
    max_rows: u64,
    max_bytes: usize,
}

#[derive(Default)]
struct Window {
    buf: Vec<u8>,
    rows: u64,
}

impl Windows<'_> {
    /// Fill `w` up to the window's bounds; `true` when the source ended.
    async fn fill(&mut self, w: &mut Window) -> Result<bool> {
        loop {
            let rows = match self.pending.take() {
                Some(p) => p,
                None => match self.source.next().await {
                    Some(b) => b.rows,
                    None => return Ok(true),
                },
            };
            let mut rows = rows.into_iter();
            for row in rows.by_ref() {
                write_row(&mut w.buf, &self.targets, row)?;
                w.rows += 1;
                if w.rows >= self.max_rows || w.buf.len() >= self.max_bytes {
                    let rest: Vec<_> = rows.collect();
                    if !rest.is_empty() {
                        self.pending = Some(rest);
                    }
                    return Ok(false);
                }
            }
        }
    }
}

/// The windows loaded one after another by `load`, the next one read
/// while the current one loads. `progress` gets the total after each
/// window lands, also when reading the next one failed meanwhile (that
/// window is already in the table).
async fn run_windows<L, F>(windows: &mut Windows<'_>, mut load: L, progress: Progress<'_>) -> Result<u64>
where
    L: FnMut(Vec<u8>) -> F,
    F: std::future::Future<Output = Result<()>>,
{
    let mut cur = Window::default();
    let mut ended = windows.fill(&mut cur).await?;
    let mut total = 0u64;
    while cur.rows > 0 {
        let mut next = Window::default();
        let rows = cur.rows;
        let body = std::mem::take(&mut cur.buf);
        let filled = if ended {
            load(body).await?;
            Ok(true)
        } else {
            // Both finish before anything returns: no job left running.
            let (loaded, filled) = tokio::join!(load(body), windows.fill(&mut next));
            loaded?;
            filled
        };
        total += rows;
        progress(total);
        ended = filled?;
        cur = next;
    }
    Ok(total)
}

/// A load job id, chosen before the upload so the job can be cancelled or
/// found again whatever happens to the upload's answer.
fn new_job_id() -> String {
    static SEQ: std::sync::atomic::AtomicU64 = std::sync::atomic::AtomicU64::new(0);
    let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
    let seq = SEQ.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    format!("{JOB_PREFIX}{nanos}_{}_{seq}", std::process::id())
}

/// Cancels the job unless it was seen done: when the load returns early
/// or is dropped (cancelled) mid-way.
struct CancelUnlessDone {
    api: Option<super::Api>,
    job: JobRef,
}

impl CancelUnlessDone {
    fn done(mut self) {
        self.api = None;
    }
}

impl Drop for CancelUnlessDone {
    fn drop(&mut self) {
        let Some(api) = self.api.take() else { return };
        let Ok(rt) = tokio::runtime::Handle::try_current() else { return };
        let job = self.job.clone();
        rt.spawn(async move { api.cancel(&job).await });
    }
}

/// What the upload's `PUT` came to.
enum Put {
    Job(Json),
    /// Refused: no job was created.
    Rejected(Error),
    /// No answer, or one that doesn't say: the job may exist.
    Unknown(Error),
}

/// The job, when the upload's answer was lost (it may have been created).
async fn find_job(s: &BigQuerySession, job: &JobRef) -> Option<Json> {
    for _ in 0..3 {
        if let Ok(j) = s.get_job(&job.id, job.location.as_deref()).await {
            return Some(j);
        }
        tokio::time::sleep(Duration::from_secs(2)).await;
    }
    None
}

/// One window as one load job: resumable upload of the lines, then the
/// job polled until it's done.
async fn load_window(s: &BigQuerySession, ds: &str, table: &str, body: Vec<u8>) -> Result<()> {
    let api = &s.api;
    let mut job_ref = JobRef { id: new_job_id(), location: api.location.clone() };
    let mut url = reqwest::Url::parse(&api.base).map_err(|e| Error::Connect(format!("endpoint inválido: {e}")))?;
    url.path_segments_mut()
        .map_err(|_| Error::Connect("endpoint inválido".into()))?
        .pop_if_empty()
        .extend(["upload", "bigquery", "v2", "projects", api.project.as_str(), "jobs"]);
    url.query_pairs_mut().append_pair("uploadType", "resumable");
    let mut config = load_config(&api.project, ds, table);
    config["jobReference"] = json!({ "projectId": api.project, "jobId": job_ref.id });
    if let Some(l) = &api.location {
        config["jobReference"]["location"] = json!(l);
    }
    // Cancellable from here on (interrupter, or this future dropped).
    s.set_job(Some(job_ref.clone()));
    let guard = CancelUnlessDone { api: Some(api.clone()), job: job_ref.clone() };
    let bearer = api.tokens.bearer().await?;
    let with_auth = |r: reqwest::RequestBuilder| match &bearer {
        Some(b) => r.header("Authorization", b),
        None => r,
    };
    let resp = with_auth(api.http.post(url.clone()).json(&config)).send().await.map_err(|e| Error::Connect(e.to_string()))?;
    let status = resp.status();
    let location = resp.headers().get("location").and_then(|v| v.to_str().ok()).map(str::to_string);
    if !status.is_success() {
        let text = resp.text().await.unwrap_or_default();
        return Err(super::gcp::api_error(status.as_u16(), &text));
    }
    let location = location.ok_or_else(|| Error::Query("BigQuery no devolvió la dirección de subida de la carga".into()))?;
    let mut upload = reqwest::Url::parse(&location).map_err(|e| Error::Query(format!("dirección de subida inválida: {e}")))?;
    if api.emulator {
        // The emulator answers with its own address inside the container.
        let _ = upload.set_scheme(url.scheme());
        let _ = upload.set_host(url.host_str());
        let _ = upload.set_port(url.port_or_known_default());
    }
    let put = match with_auth(api.http.put(upload).header("Content-Type", "application/octet-stream").body(body)).send().await {
        Err(e) => Put::Unknown(Error::Connect(e.to_string())),
        Ok(resp) => {
            let status = resp.status();
            match resp.text().await {
                Err(e) => Put::Unknown(Error::Connect(e.to_string())),
                Ok(text) if status.is_success() => serde_json::from_str(&text).map_or_else(|e| Put::Unknown(e.into()), Put::Job),
                Ok(text) => {
                    let e = super::gcp::api_error(status.as_u16(), &text);
                    if status.is_client_error() && status.as_u16() != 408 && status.as_u16() != 429 {
                        Put::Rejected(e)
                    } else {
                        Put::Unknown(e)
                    }
                }
            }
        }
    };
    let mut job = match put {
        Put::Job(j) => j,
        Put::Rejected(e) => return Err(e),
        Put::Unknown(e) => match find_job(s, &job_ref).await {
            Some(j) => j,
            None => return Err(e),
        },
    };
    if let Some(l) = job.pointer("/jobReference/location").and_then(Json::as_str) {
        job_ref.location = Some(l.to_string());
    }
    let mut wait = Duration::from_millis(100);
    let mut failures = 0;
    while !is_done(&job) {
        tokio::time::sleep(wait).await;
        wait = (wait * 2).min(Duration::from_secs(2));
        match s.get_job(&job_ref.id, job_ref.location.as_deref()).await {
            Ok(j) => {
                job = j;
                failures = 0;
            }
            Err(e) => {
                failures += 1;
                if failures >= 15 {
                    // The guard asks to cancel it.
                    return Err(Error::Query(format!(
                        "no se pudo saber cómo terminó la carga en «{table}» (trabajo {}), se pidió cancelarla: {e}",
                        job_ref.id
                    )));
                }
            }
        }
    }
    guard.done();
    s.set_job(None);
    match job_error(&job) {
        Some(e) => Err(Error::Query(format!("la carga en «{table}» falló: {e}"))),
        None => Ok(()),
    }
}

/// Whether a `jobs.list` item is a DBine load into the table that hasn't
/// finished.
pub(crate) fn is_orphan(job: &Json, project: &str, ds: &str, table: &str) -> bool {
    let id = job.pointer("/jobReference/jobId").and_then(Json::as_str).unwrap_or("");
    let dest = job.pointer("/configuration/load/destinationTable");
    let is = |k: &str, v: &str| dest.and_then(|d| d.get(k)).and_then(Json::as_str) == Some(v);
    id.starts_with(JOB_PREFIX) && !is_done(job) && is("projectId", project) && is("datasetId", ds) && is("tableId", table)
}

/// DBine's load jobs into the table still pending or running (left by a
/// process killed mid-load, before this load emptied and reloads the
/// table): cancelled and waited for. One that lands anyway put its rows in
/// the table, and this load would duplicate them: that's an error.
async fn settle_orphans(s: &BigQuerySession, ds: &str, table: &str) -> Result<()> {
    let mut orphans = Vec::new();
    let mut token: Option<String> = None;
    for _ in 0..50 {
        let mut q = vec![
            ("stateFilter", "pending".to_string()),
            ("stateFilter", "running".to_string()),
            ("projection", "full".to_string()),
            ("maxResults", "1000".to_string()),
        ];
        if let Some(t) = token.take() {
            q.push(("pageToken", t));
        }
        // Listing jobs needs `bigquery.jobs.list`, which the "Job User"
        // role lacks: without it there's nothing to check, not a failure.
        let page = match s.api.get(&["jobs"], &q).await {
            Ok(p) => p,
            Err(e) => {
                tracing::warn!("bigquery: can't list running load jobs of {table}: {e}");
                break;
            }
        };
        for j in page.get("jobs").and_then(Json::as_array).into_iter().flatten() {
            if is_orphan(j, &s.api.project, ds, table) {
                orphans.push(JobRef {
                    id: j.pointer("/jobReference/jobId").and_then(Json::as_str).unwrap_or_default().to_string(),
                    location: j.pointer("/jobReference/location").and_then(Json::as_str).map(str::to_string),
                });
            }
        }
        match page.get("nextPageToken").and_then(Json::as_str).filter(|t| !t.is_empty()) {
            Some(t) => token = Some(t.to_string()),
            None => break,
        }
    }
    for o in orphans {
        s.api.cancel(&o).await;
        let mut wait = Duration::from_millis(200);
        loop {
            let j = s.get_job(&o.id, o.location.as_deref()).await?;
            if is_done(&j) {
                if job_error(&j).is_none() {
                    return Err(Error::Query(format!(
                        "una carga anterior de DBine en «{table}» (trabajo {}) siguió corriendo y terminó ahora: sus filas ya están en la tabla, así que hay que vaciarla y copiarla de nuevo",
                        o.id
                    )));
                }
                break;
            }
            tokio::time::sleep(wait).await;
            wait = (wait * 2).min(Duration::from_secs(2));
        }
    }
    Ok(())
}

pub(crate) async fn bulk_load(s: &BigQuerySession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    let ds = s.dataset(&spec.table)?;
    let ds = ds.as_str();
    let table = spec.table.name.as_str();
    let meta = s.api.get(&["datasets", ds, "tables", table], &[]).await?;
    if meta.get("type").and_then(Json::as_str).unwrap_or("TABLE") != "TABLE" {
        return Err(Error::Query(format!("«{table}» no es una tabla: la carga masiva solo escribe en tablas")));
    }
    let fields = meta.pointer("/schema/fields").and_then(Json::as_array).cloned().unwrap_or_default();
    let targets = targets(&fields, &spec.columns)?;
    settle_orphans(s, ds, table).await?;
    let mut windows = Windows {
        source,
        pending: None,
        targets,
        max_rows: spec.commit_rows.max(1),
        max_bytes: usize::try_from(spec.commit_bytes).unwrap_or(usize::MAX).clamp(1, LOAD_BYTES),
    };
    let r = run_windows(&mut windows, |body| load_window(s, ds, table, body), progress).await;
    s.set_job(None);
    r
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::transfer::RowBatch;
    use dbine_driver::{kinds, ObjectRef};

    fn fields() -> Vec<Json> {
        serde_json::from_str(
            r#"[{"name":"id","type":"INTEGER","mode":"REQUIRED"},{"name":"f","type":"FLOAT"},
                {"name":"n","type":"NUMERIC","precision":"10","scale":"2"},{"name":"bn","type":"BIGNUMERIC"},
                {"name":"ok","type":"BOOLEAN"},{"name":"s","type":"STRING","maxLength":"20"},{"name":"b","type":"BYTES"},
                {"name":"d","type":"DATE"},{"name":"tm","type":"TIME"},{"name":"dt","type":"DATETIME"},
                {"name":"ts","type":"TIMESTAMP"},{"name":"j","type":"JSON"},{"name":"g","type":"GEOGRAPHY"},
                {"name":"arr","type":"INTEGER","mode":"REPEATED"},
                {"name":"st","type":"RECORD","fields":[{"name":"x","type":"INTEGER"},{"name":"y","type":"BYTES"},
                  {"name":"t","type":"TIMESTAMP"},{"name":"k","type":"JSON"}]},
                {"name":"r","type":"RANGE","rangeElementType":{"type":"DATE"}}]"#,
        )
        .unwrap()
    }

    #[test]
    fn types_in_googlesql() {
        let got: Vec<String> = fields().iter().map(field_type).collect();
        assert_eq!(
            got,
            vec![
                "INT64",
                "FLOAT64",
                "NUMERIC(10, 2)",
                "BIGNUMERIC",
                "BOOL",
                "STRING(20)",
                "BYTES",
                "DATE",
                "TIME",
                "DATETIME",
                "TIMESTAMP",
                "JSON",
                "GEOGRAPHY",
                "ARRAY<INT64>",
                "STRUCT<`x` INT64, `y` BYTES, `t` TIMESTAMP, `k` JSON>",
                "RANGE<DATE>"
            ]
        );
        let c = transfer_column(&fields()[0]);
        assert_eq!((c.name.as_str(), c.nullable), ("id", false));
        assert!(transfer_column(&fields()[1]).nullable);
    }

    #[test]
    fn timestamps_are_exact() {
        assert_eq!(epoch_micros("1706708700123456"), Some(1_706_708_700_123_456));
        assert_eq!(epoch_micros("1706708700.123456"), Some(1_706_708_700_123_456));
        assert_eq!(epoch_micros("1706708700.5"), Some(1_706_708_700_500_000));
        assert_eq!(epoch_micros("-1.5"), Some(-1_500_000));
        assert_eq!(epoch_micros("1.7067087E9"), Some(1_706_708_700_000_000));
        // Near the epoch: integers are microseconds (useInt64Timestamp).
        assert_eq!(epoch_micros("5"), Some(5));
        assert_eq!(utc_text(1_706_708_700_123_450).as_deref(), Some("2024-01-31 13:45:00.12345"));
        assert_eq!(utc_text(-1).as_deref(), Some("1969-12-31 23:59:59.999999"));
    }

    #[test]
    fn a_recorded_row_becomes_typed_cells() {
        // tabledata.list of the emulator, with useInt64Timestamp.
        let row: Json = serde_json::from_str(
            r#"{"f":[{"v":"9223372036854775807"},{"v":"-Inf"},{"v":"12.34"},{"v":"123456789012345678901234567890.5"},
            {"v":"true"},{"v":"hola"},{"v":"AAH/"},{"v":"2024-01-31"},{"v":"12:34:56.789"},{"v":"2024-01-31T13:45:00.5"},
            {"v":"1706708700123456"},{"v":"{\"k\":[1,\"x\"]}"},{"v":"POINT(1 2)"},{"v":[{"v":"1"},{"v":"2"}]},
            {"v":{"f":[{"v":"5"},{"v":"YWI="},{"v":"0"},{"v":"{\"a\":1}"}]}},{"v":"[2024-01-01, UNBOUNDED)"}]}"#,
        )
        .unwrap();
        let f = fields();
        let vals = row["f"].as_array().unwrap();
        let cells: Vec<Cell> = f.iter().zip(vals).map(|(f, v)| read_cell(read_kind(f), &v["v"], f)).collect();
        assert_eq!(
            cells,
            vec![
                Cell::Int(i64::MAX),
                Cell::Float(f64::NEG_INFINITY),
                Cell::Decimal("12.34".into()),
                Cell::Decimal("123456789012345678901234567890.5".into()),
                Cell::Bool(true),
                Cell::Text("hola".into()),
                Cell::Bytes(vec![0, 1, 255]),
                Cell::Date("2024-01-31".into()),
                Cell::Time("12:34:56.789".into()),
                Cell::DateTime("2024-01-31 13:45:00.5".into()),
                Cell::DateTimeTz("2024-01-31 13:45:00.123456+00:00".into()),
                Cell::Json("{\"k\":[1,\"x\"]}".into()),
                Cell::Text("POINT(1 2)".into()),
                Cell::Json("[1,2]".into()),
                Cell::Json(r#"{"x":5,"y":"YWI=","t":"1970-01-01 00:00:00+00:00","k":{"a":1}}"#.into()),
                Cell::Text("[2024-01-01, UNBOUNDED)".into()),
            ]
        );
        assert_eq!(read_cell(Read::Int, &Json::Null, &f[0]), Cell::Null);
        assert_eq!(read_cell(Read::Float, &json!("NaN"), &f[1]).to_json(), json!("NaN"));
    }

    #[test]
    fn reads_select_only_the_columns_in_order() {
        let t = ObjectRef { kind: kinds::TABLE.into(), schema: Some("ds".into()), name: "t`x".into() };
        let spec = ReadSpec { table: t.clone(), columns: Some(vec!["b".into(), "a".into()]), filter: Some(" id > 5 ".into()) };
        assert_eq!(select_sql("ds", &spec), "SELECT `b`, `a` FROM `ds`.`t\\`x` WHERE id > 5");
        let spec = ReadSpec { table: t, columns: None, filter: Some("  ".into()) };
        assert_eq!(select_sql("ds", &spec), "SELECT * FROM `ds`.`t\\`x`");

        let f = fields();
        assert_eq!(pick(&f, Some(&["S".into(), "id".into()])).unwrap(), vec![5, 0]);
        assert_eq!(pick(&f, None).unwrap().len(), f.len());
        assert!(pick(&f, Some(&["nope".into()])).is_err());
    }

    #[test]
    fn values_follow_the_target_type() {
        use Load::*;
        // Raw JSON text parsed back only to compare its shape here.
        let v = |c: Cell, l: Load| {
            load_value(c, l).unwrap().map(|o| match o {
                Out::Val(j) => j,
                Out::Raw(s) => serde_json::from_str(&s).unwrap(),
            })
        };
        assert_eq!(v(Cell::Null, Int), None);
        assert_eq!(v(Cell::Int(5), Int), Some(json!(5)));
        assert_eq!(v(Cell::Int(i64::MAX), Int), Some(json!("9223372036854775807")));
        assert_eq!(v(Cell::Int(5), Numeric), Some(json!("5")));
        assert_eq!(v(Cell::UInt(u64::MAX), Numeric), Some(json!("18446744073709551615")));
        assert_eq!(v(Cell::Bool(true), Int), Some(json!(1)));
        assert_eq!(v(Cell::Int(0), Bool), Some(json!(false)));
        assert_eq!(v(Cell::Float(1.5), Float), Some(json!(1.5)));
        assert_eq!(v(Cell::Float(f64::NAN), Float), Some(json!("NaN")));
        assert_eq!(v(Cell::Float(f64::NEG_INFINITY), Float), Some(json!("-Infinity")));
        assert_eq!(v(Cell::Decimal("-12.3400".into()), Numeric), Some(json!("-12.3400")));
        assert_eq!(v(Cell::Bytes(vec![0, 1, 255]), Bytes), Some(json!("AAH/")));
        assert_eq!(v(Cell::Bytes(b"hi".to_vec()), Str), Some(json!("hi")));
        assert!(load_value(Cell::Bytes(vec![0xff]), Str).is_err());
        assert_eq!(v(Cell::Text("ab".into()), Bytes), Some(json!("YWI=")));
        assert_eq!(v(Cell::Uuid("00112233-4455-6677-8899-aabbccddeeff".into()), Bytes), Some(json!("ABEiM0RVZneImaq7zN3u/w==")));
        assert_eq!(v(Cell::Uuid("00112233-4455-6677-8899-aabbccddeeff".into()), Str), Some(json!("00112233-4455-6677-8899-aabbccddeeff")));
        assert_eq!(v(Cell::Json("{\"a\":[1]}".into()), Json), Some(json!({"a": [1]})));
        assert_eq!(v(Cell::Json("[1,2]".into()), Nested), Some(json!([1, 2])));
        assert_eq!(v(Cell::Json("{\"a\":1}".into()), Str), Some(json!("{\"a\":1}")));
        assert_eq!(v(Cell::Text("{\"x\":5}".into()), Nested), Some(json!({"x": 5})));
        assert_eq!(v(Cell::Time("12:00:00.123456789".into()), Time), Some(json!("12:00:00.123456")));
        assert_eq!(v(Cell::DateTime("2024-01-31 13:45:00.1234567".into()), DateTime), Some(json!("2024-01-31 13:45:00.123456")));
        assert_eq!(v(Cell::DateTime("2024-01-31 13:45:00".into()), Date), Some(json!("2024-01-31")));
        assert_eq!(
            v(Cell::DateTimeTz("2024-01-31 13:45:00.123456789+02:00".into()), Timestamp),
            Some(json!("2024-01-31 13:45:00.123456+02:00"))
        );
        assert_eq!(v(Cell::DateTimeTz("2024-01-31 13:45:00.5-03:00".into()), DateTime), Some(json!("2024-01-31 16:45:00.5")));
        assert_eq!(v(Cell::Date("2024-01-31".into()), Date), Some(json!("2024-01-31")));
        assert_eq!(v(Cell::Text("POINT(1 2)".into()), Other), Some(json!("POINT(1 2)")));
    }

    #[test]
    fn micros_text_keeps_the_offset() {
        assert_eq!(micros_text("12:00:00"), "12:00:00");
        assert_eq!(micros_text("12:00:00.5"), "12:00:00.5");
        assert_eq!(micros_text("2024-01-01 12:00:00.123456789-03:00"), "2024-01-01 12:00:00.123456-03:00");
    }

    #[test]
    fn rows_become_ndjson_lines() {
        let f = fields();
        let t = targets(&f, &["ID".into(), "b".into(), "st".into(), "s".into()]).unwrap();
        let mut buf = Vec::new();
        write_row(&mut buf, &t, vec![Cell::Int(1), Cell::Bytes(vec![1]), Cell::Json("{\"x\":2}".into()), Cell::Null]).unwrap();
        write_row(&mut buf, &t, vec![Cell::Int(2), Cell::Null, Cell::Null, Cell::Text("a\"b\n".into())]).unwrap();
        assert_eq!(String::from_utf8(buf).unwrap(), "{\"id\":1,\"b\":\"AQ==\",\"st\":{\"x\":2}}\n{\"id\":2,\"s\":\"a\\\"b\\n\"}\n");
        // A row of another width is an error, not a shifted load.
        assert!(write_row(&mut Vec::new(), &t, vec![Cell::Int(1)]).is_err());
        assert!(targets(&f, &["nope".into()]).is_err());
    }

    #[test]
    fn load_jobs_append_and_never_create() {
        let c = load_config("p", "d", "t");
        let l = &c["configuration"]["load"];
        assert_eq!(l["writeDisposition"], "WRITE_APPEND");
        assert_eq!(l["createDisposition"], "CREATE_NEVER");
        assert_eq!(l["sourceFormat"], "NEWLINE_DELIMITED_JSON");
        assert_eq!(l["maxBadRecords"], 0);
        assert_eq!(l["destinationTable"], json!({"projectId": "p", "datasetId": "d", "tableId": "t"}));
    }

    #[test]
    fn job_errors_are_reported() {
        assert_eq!(job_error(&json!({"status": {"state": "DONE"}})), None);
        let j = json!({"status": {"state": "DONE", "errorResult": {"message": "bad"},
            "errors": [{"message": "bad"}, {"message": "row 3: no such field"}]}});
        assert_eq!(job_error(&j).as_deref(), Some("bad (row 3: no such field)"));
    }

    struct Src(Vec<RowBatch>);
    #[dbine_driver::async_trait]
    impl BatchSource for Src {
        async fn next(&mut self) -> Option<RowBatch> {
            if self.0.is_empty() {
                None
            } else {
                Some(self.0.remove(0))
            }
        }
    }

    #[tokio::test]
    async fn windows_split_batches_by_rows() {
        let batch = |n: i64| RowBatch { rows: (0..n).map(|i| vec![Cell::Int(i)]).collect(), bytes: 0 };
        let mut src = Src(vec![batch(5), batch(5)]);
        let t = targets(&fields(), &["id".into()]).unwrap();
        let mut w = Windows { source: &mut src, pending: None, targets: t, max_rows: 3, max_bytes: usize::MAX };
        let mut sizes = Vec::new();
        loop {
            let mut win = Window::default();
            let ended = w.fill(&mut win).await.unwrap();
            if win.rows > 0 {
                sizes.push(win.rows);
            }
            if ended {
                break;
            }
        }
        assert_eq!(sizes, vec![3, 3, 3, 1]);
    }

    #[test]
    fn filters_cant_add_statements() {
        // `jobs.query` runs scripts: a `;` would run DML on the source.
        assert!(check_filter("1=1; DELETE FROM ds.t WHERE true; SELECT 1").is_err());
        assert!(check_filter("id > 1;").is_err());
        assert!(check_filter("s = 'x'; DROP TABLE t").is_err());
        // Inside literals, quoted names and comments it's harmless.
        for ok in [
            "id > 5",
            "s = 'a;b'",
            "s = \"a;b\"",
            "`a;b` = 1",
            r"s = 'it\'s; fine'",
            r"s = r'a\'; b'",
            "s = '''a;'b'''",
            r#"s = """x;"y""""#,
            "id > 1 -- ; comment",
            "id > 1 # ; comment",
            "id /* ; */ > 1",
        ] {
            assert!(check_filter(ok).is_ok(), "{ok}");
        }
        // Unclosed: what follows could hide a `;`.
        for bad in ["s = 'a", "`x", "id /* ; ", "s = '''a''", r"s = 'a\"] {
            assert!(check_filter(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn json_text_is_loaded_verbatim() {
        let f = fields();
        let t = targets(&f, &["id".into(), "j".into(), "st".into()]).unwrap();
        let big = r#"{"a":100000000000000000000,"p":0.12345678901234567890123}"#;
        let mut buf = Vec::new();
        write_row(&mut buf, &t, vec![Cell::Int(1), Cell::Json(big.into()), Cell::Text("{\n \"x\": 1e400\n}".into())]).unwrap();
        let line = String::from_utf8(buf).unwrap();
        assert_eq!(line, format!("{{\"id\":1,\"j\":{big},\"st\":{{  \"x\": 1e400 }}}}\n"));
        // Text into JSON is always a document: its type never depends on
        // the value, and text that isn't JSON is an error, not a string.
        assert_eq!(load_value(Cell::Text("123".into()), Load::Json).unwrap(), Some(Out::Raw("123".into())));
        assert!(load_value(Cell::Text("abc".into()), Load::Json).is_err());
        assert!(load_value(Cell::Json("{bad".into()), Load::Nested).is_err());
        // Reading keeps the digits of JSON nested in a STRUCT too.
        let st = &f[14];
        let v: Json =
            serde_json::from_str(r#"{"f":[{"v":"1"},{"v":null},{"v":null},{"v":"{\"p\":0.12345678901234567890123,\"a\":100000000000000000000}"}]}"#)
                .unwrap();
        assert_eq!(
            read_cell(Read::Nested, &v, st),
            Cell::Json(r#"{"x":1,"y":null,"t":null,"k":{"p":0.12345678901234567890123,"a":100000000000000000000}}"#.into())
        );
    }

    #[test]
    fn bytes_into_text_is_one_encoding() {
        assert_eq!(load_value(Cell::Bytes(b"hola".to_vec()), Load::Str).unwrap(), Some(Out::Val(json!("hola"))));
        // Never base64 for some rows and text for others.
        assert!(load_value(Cell::Bytes(vec![0x68, 0xff]), Load::Str).is_err());
        assert!(load_value(Cell::Bytes(vec![0xc3]), Load::Other).is_err());
    }

    #[test]
    fn text_into_bytes_is_never_read_as_base64() {
        // "1234" is valid base64: it must still load as its UTF-8 bytes.
        for c in [
            Cell::Decimal("1234".into()),
            Cell::Text("1234".into()),
            Cell::Date("1234".into()),
            Cell::Time("1234".into()),
            Cell::DateTime("1234".into()),
            Cell::DateTimeTz("1234".into()),
            Cell::Json("1234".into()),
        ] {
            assert_eq!(load_value(c.clone(), Load::Bytes).unwrap(), Some(Out::Val(json!("MTIzNA=="))), "{c:?}");
        }
        for c in [Cell::Int(1), Cell::UInt(1), Cell::Float(1.0), Cell::Bool(true)] {
            assert!(load_value(c, Load::Bytes).is_err());
        }
    }

    #[test]
    fn zoned_values_are_the_same_instant_in_every_target() {
        let v = |l: Load| load_value(Cell::DateTimeTz("2024-01-31 23:30:00-03:00".into()), l).unwrap();
        assert_eq!(v(Load::DateTime), Some(Out::Val(json!("2024-02-01 02:30:00"))));
        assert_eq!(v(Load::Date), Some(Out::Val(json!("2024-02-01"))));
        assert_eq!(v(Load::Time), Some(Out::Val(json!("02:30:00"))));
        assert_eq!(v(Load::Timestamp), Some(Out::Val(json!("2024-01-31 23:30:00-03:00"))));
        assert_eq!(to_utc("2024-01-31T23:30:00.5Z").as_deref(), Some("2024-01-31 23:30:00.5"));
        assert_eq!(to_utc("2024-01-31 23:30:00+0100").as_deref(), Some("2024-01-31 22:30:00"));
        assert!(load_value(Cell::DateTimeTz("ayer".into()), Load::Date).is_err());
    }

    #[tokio::test]
    async fn a_landed_window_is_reported_when_the_next_one_fails() {
        // The second batch has the wrong width: reading it fails while the
        // first window loads, and that window already landed.
        let mut src = Src(vec![
            RowBatch { rows: (0..3).map(|i| vec![Cell::Int(i)]).collect(), bytes: 0 },
            RowBatch { rows: vec![vec![Cell::Int(1), Cell::Int(2)]], bytes: 0 },
        ]);
        let t = targets(&fields(), &["id".into()]).unwrap();
        let mut w = Windows { source: &mut src, pending: None, targets: t, max_rows: 3, max_bytes: usize::MAX };
        let loads = std::sync::Mutex::new(0);
        let seen = std::sync::Mutex::new(Vec::new());
        let progress = |n: u64| seen.lock().unwrap().push(n);
        let r = run_windows(
            &mut w,
            |_body| {
                *loads.lock().unwrap() += 1;
                async { Ok(()) }
            },
            &progress,
        )
        .await;
        assert!(r.is_err());
        assert_eq!(*loads.lock().unwrap(), 1);
        assert_eq!(*seen.lock().unwrap(), vec![3]);
    }

    #[test]
    fn two_windows_fit_the_table_budget() {
        // The window loading plus the next one encoding: ~32 MiB per table.
        const { assert!(2 * LOAD_BYTES <= 16 * 1024 * 1024) };
    }

    #[test]
    fn only_the_read_columns_are_downloaded() {
        let f = fields();
        let p = pick(&f, Some(&["S".into(), "id".into(), "s".into()])).unwrap();
        let sel = selected_fields(&f, &p).unwrap();
        assert_eq!(sel.names, "id,s");
        assert!(selected_fields(&f, &(0..f.len()).collect::<Vec<_>>()).is_none());
        // Rows of the selected fields go back to their schema positions.
        let mut page = json!({"rows": [{"f": [{"v": "7"}, {"v": "x"}]}]});
        sel.narrow(&mut page, f.len()).unwrap();
        let row = &page["rows"][0]["f"];
        assert_eq!((row[0]["v"].as_str(), row[5]["v"].as_str(), row[1].is_null()), (Some("7"), Some("x"), true));
        // A server that ignores selectedFields sends whole rows: kept.
        let whole: Vec<Json> = (0..f.len()).map(|i| json!({"v": i.to_string()})).collect();
        let mut page = json!({"rows": [{"f": whole.clone()}]});
        sel.narrow(&mut page, f.len()).unwrap();
        assert_eq!(page["rows"][0]["f"], Json::Array(whole));
        // Any other width is an error, not a shifted row.
        let mut page = json!({"rows": [{"f": [{"v": "1"}]}]});
        assert!(sel.narrow(&mut page, f.len()).is_err());
    }

    #[test]
    fn load_jobs_have_their_id_before_the_upload() {
        let (a, b) = (new_job_id(), new_job_id());
        assert!(a.starts_with(JOB_PREFIX) && a != b);
        assert!(a.bytes().all(|c| c.is_ascii_alphanumeric() || c == b'_'));
        let job = |id: &str, state: &str, table: &str| {
            json!({"jobReference": {"jobId": id}, "state": state,
                "configuration": {"load": {"destinationTable": {"projectId": "p", "datasetId": "d", "tableId": table}}}})
        };
        assert!(is_orphan(&job(&a, "RUNNING", "t"), "p", "d", "t"));
        assert!(is_orphan(&job(&a, "PENDING", "t"), "p", "d", "t"));
        assert!(!is_orphan(&job(&a, "DONE", "t"), "p", "d", "t"));
        assert!(!is_orphan(&job(&a, "RUNNING", "u"), "p", "d", "t"));
        // Someone else's load into the table is never touched.
        assert!(!is_orphan(&job("bqjob_123", "RUNNING", "t"), "p", "d", "t"));
    }
}
