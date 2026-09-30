//! Bulk transfer (see `dbine_driver::transfer`) for Cassandra, ScyllaDB and
//! Amazon Keyspaces.
//!
//! Reading: one paged `SELECT` with the values typed from their CQL types:
//! integers, floats, `decimal` / `varint` as exact digits, `date`, `time`,
//! `timestamp` (UTC, with its `+00:00`; years outside 0000-9999 in ISO
//! 8601's expanded form, `-0001-01-01`, `+10000-01-01`), UUIDs, blobs
//! whole, and collections, tuples and UDTs as JSON. CQL's empty value (an
//! `int` holding zero bytes) reads as an empty text, which loads back as
//! an empty value. A filter goes to the `WHERE`, with `ALLOW FILTERING`.
//! Apache Cassandra pages by rows only (ScyllaDB also caps a page at about
//! 1 MiB), so the page size adapts to the widest row seen, to keep a page
//! near [`PAGE_BYTES`]; it starts at one row and grows fourfold per page.
//!
//! Loading: a prepared `INSERT`, with up to [`IN_FLIGHT`] requests and
//! [`IN_FLIGHT_BYTES`] of rows at once. The driver sends each one straight
//! to a replica of its partition (token-aware), so the load spreads over
//! the cluster. An `UNLOGGED BATCH` only pays off when many rows share a
//! partition; across partitions it puts all the work on one coordinator and
//! is slower. CQL has no transactions: a row is written when its request
//! answers, so `commit_rows` / `commit_bytes` only pace the progress
//! reports. On an error every request already sent is awaited before
//! returning, so no row lands after `bulk_load` returns. A cancelled load
//! (its future dropped) can't take back the requests already on the wire:
//! at most [`IN_FLIGHT_BYTES`] of rows may still land shortly after. The
//! in-flight bytes are measured on the converted values ([`row_cost`]),
//! not on the source cells: a `list<int>` is ~2 bytes per element as JSON
//! but ~72 as `CqlValue`s.
//! Counter tables can't be written with `INSERT` and answer `Unsupported`;
//! so do values the column can't hold without loss (a timestamp finer than
//! a millisecond).

use crate::{cql, CassandraSession};
use chrono::{NaiveTime, Timelike};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use futures::stream::{FuturesUnordered, StreamExt};
use scylla::frame::response::result::{CollectionType, ColumnType, NativeType};
use scylla::response::{PagingState, PagingStateResponse};
use scylla::statement::Statement;
use scylla::value::{Counter, CqlDate, CqlDecimal, CqlDuration, CqlTime, CqlTimestamp, CqlTimeuuid, CqlValue, CqlVarint, Row};
use serde_json::Value as J;
use std::str::FromStr;
use std::time::Duration;

/// Most rows per page of the read.
const PAGE: i32 = 5_000;
/// What a page of the read aims at.
const PAGE_BYTES: usize = 4 * 1024 * 1024;
/// Inserts in flight at once…
pub(crate) const IN_FLIGHT: usize = 256;
/// …and the memory they hold ([`row_cost`]: the `CqlValue`s plus their
/// serialized frame); a wider row still goes alone.
pub(crate) const IN_FLIGHT_BYTES: usize = 16 * 1024 * 1024;
/// Longest `decimal` / `varint` in digits (PostgreSQL's `numeric` tops at
/// 147,455): the conversions are quadratic, and past this a value only
/// costs time and memory.
const MAX_DIGITS: usize = 200_000;

impl CassandraSession {
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let ks = self.ks(&spec.table)?;
        let all = self.table_columns(&ks, &spec.table.name).await?;
        if all.is_empty() {
            return Err(Error::Query(format!("No existe la tabla {}.", cql::qualified(Some(&ks), &spec.table.name))));
        }
        let names: Vec<String> = match &spec.columns {
            Some(c) => c.clone(),
            None => all.iter().map(|c| c.name.clone()).collect(),
        };
        if let Some(n) = names.iter().find(|n| !all.iter().any(|c| &c.name == *n)) {
            return Err(Error::Query(format!("No existe la columna {n} en {}.", cql::qualified(Some(&ks), &spec.table.name))));
        }
        let cols: Vec<TransferColumn> = names
            .iter()
            .map(|n| {
                let c = all.iter().find(|c| &c.name == n);
                TransferColumn {
                    name: n.clone(),
                    type_name: c.map(|c| c.typ.clone()).unwrap_or_default(),
                    nullable: c.is_none_or(|c| c.kind == "regular" || c.kind == "static"),
                }
            })
            .collect();
        let list = names.iter().map(|n| cql::ident(n)).collect::<Vec<_>>().join(", ");
        let mut q = format!("SELECT {list} FROM {}", cql::qualified(Some(&ks), &spec.table.name));
        if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            q.push_str(&format!(" WHERE {f} ALLOW FILTERING"));
        }
        let mut st = Statement::new(q);
        st.set_request_timeout(Some(Duration::from_secs(300)));
        st.set_is_idempotent(true);

        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut builder = BatchBuilder::new();
        let mut paging = PagingState::start();
        let (mut page, mut widest) = (1i32, 0usize);
        loop {
            st.set_page_size(page);
            let (res, next) = self.session.query_single_page(st.clone(), (), paging).await.map_err(Error::query)?;
            let rows = res.into_rows_result().map_err(Error::query)?;
            if rows.rows_num() > 0 {
                widest = widest.max(rows.rows_bytes_size() / rows.rows_num());
            }
            {
                let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
                for row in rows.rows::<Row>().map_err(Error::query)? {
                    let row = row.map_err(Error::query)?;
                    let cells = row
                        .columns
                        .into_iter()
                        .zip(&names)
                        .map(|(v, n)| to_cell(v).map_err(|e| Error::Unsupported(format!("columna {n}: {e}"))))
                        .collect::<Result<Vec<_>>>()?;
                    widest = widest.max(cells.iter().map(Cell::size).sum());
                    builder.push(cells, &mut *s)?;
                }
            }
            page = next_page(page, widest);
            match next {
                PagingStateResponse::HasMorePages { state } => paging = state,
                PagingStateResponse::NoMorePages => break,
            }
        }
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(&mut self, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        if self.read_only {
            return Err(Error::Query("Conexión de solo lectura: no se pueden cargar datos.".into()));
        }
        let ks = self.ks(&spec.table)?;
        let cols = spec.columns.iter().map(|c| cql::ident(c)).collect::<Vec<_>>().join(", ");
        let marks = vec!["?"; spec.columns.len()].join(", ");
        let q = format!("INSERT INTO {} ({cols}) VALUES ({marks})", cql::qualified(Some(&ks), &spec.table.name));
        let mut prepared = self.session.prepare(q).await.map_err(Error::query)?;
        prepared.set_request_timeout(Some(Duration::from_secs(60)));
        // Same values, same row: safe to retry after a timeout.
        prepared.set_is_idempotent(true);
        let types: Vec<ColumnType<'static>> = prepared.get_variable_col_specs().iter().map(|c| c.typ().clone()).collect();
        if types.iter().any(|t| matches!(t, ColumnType::Native(NativeType::Counter))) {
            return Err(Error::Unsupported("las tablas de contadores no se cargan con INSERT (solo con UPDATE … SET c = c + n)".into()));
        }
        // A partition with only its static values reads as a row whose
        // clustering columns are null: it's written with the partition key
        // and the static columns alone (a null clustering key is refused).
        let kinds: Vec<String> = {
            let cols = self.table_columns(&ks, &spec.table.name).await?;
            spec.columns.iter().map(|n| cols.iter().find(|c| c.name == *n).map(|c| c.kind.clone()).unwrap_or_default()).collect()
        };
        let static_only = StaticOnly::of(&kinds);
        let static_prepared = match &static_only {
            Some(so) => {
                let cols = so.keep.iter().map(|&i| cql::ident(&spec.columns[i])).collect::<Vec<_>>().join(", ");
                let marks = vec!["?"; so.keep.len()].join(", ");
                let q = format!("INSERT INTO {} ({cols}) VALUES ({marks})", cql::qualified(Some(&ks), &spec.table.name));
                let mut p = self.session.prepare(q).await.map_err(Error::query)?;
                p.set_request_timeout(Some(Duration::from_secs(60)));
                p.set_is_idempotent(true);
                Some(p)
            }
            None => None,
        };

        let (every, every_bytes) = (spec.commit_rows.max(1), spec.commit_bytes.max(1));
        // Rows and bytes written, and at the last report.
        let (mut done, mut done_bytes, mut reported, mut reported_bytes) = (0u64, 0u64, 0u64, 0u64);
        let mut report = |done: u64, done_bytes: u64| {
            if done - reported >= every || done_bytes - reported_bytes >= every_bytes {
                (reported, reported_bytes) = (done, done_bytes);
                progress(done);
            }
        };
        let mut inflight = FuturesUnordered::new();
        let mut inflight_bytes = 0usize;
        let mut result = Ok(());
        'read: while let Some(batch) = source.next().await {
            for row in batch.rows {
                let values = match row_values(&row, &types, &spec.columns) {
                    Ok(v) => v,
                    Err(e) => {
                        result = Err(e);
                        break 'read;
                    }
                };
                let (statement, values) = match (&static_only, &static_prepared) {
                    (Some(so), Some(sp)) if so.applies(&values) => match so.values(values, &spec.columns) {
                        Ok(v) => (sp, v),
                        Err(e) => {
                            result = Err(e);
                            break 'read;
                        }
                    },
                    _ => (&prepared, values),
                };
                // `size` paces the progress (source bytes); `cost` bounds
                // the memory the request holds until it answers.
                let size: usize = row.iter().map(Cell::size).sum();
                let cost = row_cost(&values);
                drop(row);
                while !inflight.is_empty() && (inflight.len() >= IN_FLIGHT || inflight_bytes + cost > IN_FLIGHT_BYTES) {
                    let Some((r, n, c)) = inflight.next().await else { break };
                    inflight_bytes -= c;
                    if let Err(e) = r {
                        result = Err(Error::query(e));
                        break 'read;
                    }
                    done += 1;
                    done_bytes += n as u64;
                    report(done, done_bytes);
                }
                let request = self.session.execute_unpaged(statement, values);
                inflight.push(async move { (request.await, size, cost) });
                inflight_bytes += cost;
            }
        }
        // Every request already sent answers before returning: after a
        // failure no row may land once `bulk_load` has returned.
        while let Some((r, n, _)) = inflight.next().await {
            match r {
                Ok(_) => {
                    done += 1;
                    done_bytes += n as u64;
                    report(done, done_bytes);
                }
                Err(e) if result.is_ok() => result = Err(Error::query(e)),
                Err(_) => {}
            }
        }
        result?;
        if done != reported || done == 0 {
            progress(done);
        }
        Ok(done)
    }
}

/// The load's columns by their role, for a partition that has only its
/// static values (its row reads with null clustering columns).
#[derive(Debug, PartialEq)]
struct StaticOnly {
    /// Positions of the clustering columns.
    clustering: Vec<usize>,
    /// Positions of the regular columns.
    regular: Vec<usize>,
    /// Positions written for such a row: partition key and static columns.
    keep: Vec<usize>,
}

impl StaticOnly {
    /// From each load column's `system_schema.columns` kind; `None` when
    /// the table has no static column or no clustering column is loaded.
    fn of(kinds: &[String]) -> Option<StaticOnly> {
        let at = |k: &[&str]| kinds.iter().enumerate().filter(|(_, x)| k.contains(&x.as_str())).map(|(i, _)| i).collect::<Vec<_>>();
        let (clustering, regular, keep) = (at(&["clustering"]), at(&["regular"]), at(&["partition_key", "static"]));
        (!clustering.is_empty() && kinds.iter().any(|k| k == "static")).then_some(StaticOnly { clustering, regular, keep })
    }

    /// A row with a null clustering column.
    fn applies(&self, values: &[Option<CqlValue>]) -> bool {
        self.clustering.iter().any(|&i| values.get(i).is_some_and(Option::is_none))
    }

    /// The partition key and static values of such a row; a regular value
    /// there has no row to go to.
    fn values(&self, mut values: Vec<Option<CqlValue>>, columns: &[String]) -> Result<Vec<Option<CqlValue>>> {
        if let Some(&i) = self.regular.iter().find(|&&i| values.get(i).is_some_and(Option::is_some)) {
            return Err(Error::Query(format!(
                "la columna {} tiene un valor en una fila sin clave de clustering; solo las columnas estáticas pueden tenerlo",
                columns[i]
            )));
        }
        Ok(self.keep.iter().map(|&i| values[i].take()).collect())
    }
}

/// A source row as the `INSERT`'s values.
fn row_values(row: &[Cell], types: &[ColumnType], columns: &[String]) -> Result<Vec<Option<CqlValue>>> {
    if row.len() != types.len() {
        return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), types.len())));
    }
    row.iter()
        .zip(types)
        .zip(columns)
        .map(|((c, t), name)| {
            cell_to_cql(c, t).map_err(|e| match e {
                ConvError::Bad(m) => Error::Query(format!("columna {name}: {m}")),
                ConvError::Lossy(m) => Error::Unsupported(format!("columna {name}: {m}")),
            })
        })
        .collect()
}

/// Memory a pending `INSERT` holds for a row: its values as `CqlValue`s
/// (72 bytes each, plus what they point to) and, once serialized, the frame,
/// which is never larger than the values, so twice the values.
fn row_cost(values: &[Option<CqlValue>]) -> usize {
    let mem = std::mem::size_of_val(values) + values.iter().flatten().map(heap).sum::<usize>();
    mem.saturating_mul(2)
}

/// Heap bytes behind a `CqlValue` (its own size not included).
fn heap(v: &CqlValue) -> usize {
    use std::mem::size_of;
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => s.capacity(),
        CqlValue::Blob(b) => b.capacity(),
        CqlValue::Decimal(d) => d.as_signed_be_bytes_slice_and_exponent().0.len(),
        CqlValue::Varint(n) => n.as_signed_bytes_be_slice().len(),
        CqlValue::List(i) | CqlValue::Set(i) | CqlValue::Vector(i) => i.capacity() * size_of::<CqlValue>() + i.iter().map(heap).sum::<usize>(),
        CqlValue::Map(p) => p.capacity() * size_of::<(CqlValue, CqlValue)>() + p.iter().map(|(k, v)| heap(k) + heap(v)).sum::<usize>(),
        CqlValue::Tuple(i) => i.capacity() * size_of::<Option<CqlValue>>() + i.iter().flatten().map(heap).sum::<usize>(),
        CqlValue::UserDefinedType { keyspace, name, fields } => {
            keyspace.capacity()
                + name.capacity()
                + fields.capacity() * size_of::<(String, Option<CqlValue>)>()
                + fields.iter().map(|(n, v)| n.capacity() + v.as_ref().map_or(0, heap)).sum::<usize>()
        }
        _ => 0,
    }
}

/// The next page's rows: fourfold the last one, at most [`PAGE`], and as
/// many of the widest row seen as fit in [`PAGE_BYTES`].
fn next_page(page: i32, widest: usize) -> i32 {
    let fit = i32::try_from(PAGE_BYTES / widest.max(1)).unwrap_or(PAGE);
    page.saturating_mul(4).min(PAGE).min(fit).max(1)
}

// ---- CQL → cell ----

/// A CQL value as a cell; `Err` (the reason) when it can't travel whole.
pub(crate) fn to_cell(v: Option<CqlValue>) -> std::result::Result<Cell, String> {
    let Some(v) = v else { return Ok(Cell::Null) };
    Ok(match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => Cell::Text(s),
        CqlValue::Boolean(b) => Cell::Bool(b),
        CqlValue::Blob(b) => Cell::Bytes(b),
        CqlValue::Counter(c) => Cell::Int(c.0),
        CqlValue::BigInt(i) => Cell::Int(i),
        CqlValue::Int(i) => Cell::Int(i.into()),
        CqlValue::SmallInt(i) => Cell::Int(i.into()),
        CqlValue::TinyInt(i) => Cell::Int(i.into()),
        CqlValue::Double(f) => Cell::Float(f),
        // Through its shortest text, so 1.1f stays 1.1.
        CqlValue::Float(f) => Cell::Float(f.to_string().parse().unwrap_or(f64::from(f))),
        CqlValue::Decimal(d) => {
            let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
            Cell::Decimal(decimal(bytes, scale)?)
        }
        CqlValue::Varint(v) => Cell::Decimal(decimal(v.as_signed_bytes_be_slice(), 0)?),
        CqlValue::Date(d) => Cell::Date(date(d.0)),
        CqlValue::Time(t) => Cell::Time(crate::value::time(t.0)),
        CqlValue::Timestamp(t) => Cell::DateTimeTz(format!("{}+00:00", timestamp(t.0))),
        CqlValue::Uuid(u) => Cell::Uuid(u.to_string()),
        CqlValue::Timeuuid(u) => Cell::Uuid(u.to_string()),
        CqlValue::Inet(ip) => Cell::Text(ip.to_string()),
        CqlValue::Duration(d) => Cell::Text(crate::value::duration(d.months, d.days, d.nanoseconds)),
        // CQL's empty value (not NULL): an empty text, which
        // [`cell_to_cql`] writes back as the empty value.
        CqlValue::Empty => Cell::Text(String::new()),
        other => Cell::Json(whole_json(&other)?.to_string()),
    })
}

/// A value as JSON, keeping collections as arrays and objects: blobs whole
/// (`0x…`), and every value exactly as its cell would be.
fn whole_json(v: &CqlValue) -> std::result::Result<J, String> {
    Ok(match v {
        CqlValue::Blob(b) => J::String(hex(b)),
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => J::Array(items.iter().map(whole_json).collect::<std::result::Result<_, _>>()?),
        CqlValue::Tuple(items) => {
            J::Array(items.iter().map(|i| i.as_ref().map_or(Ok(J::Null), whole_json)).collect::<std::result::Result<_, _>>()?)
        }
        CqlValue::Map(pairs) => {
            if pairs.iter().all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_))) {
                let mut o = serde_json::Map::new();
                for (k, v) in pairs {
                    if let CqlValue::Text(k) | CqlValue::Ascii(k) = k {
                        o.insert(k.clone(), whole_json(v)?);
                    }
                }
                J::Object(o)
            } else {
                J::Array(pairs.iter().map(|(k, v)| Ok(J::Array(vec![whole_json(k)?, whole_json(v)?]))).collect::<std::result::Result<_, String>>()?)
            }
        }
        CqlValue::UserDefinedType { fields, .. } => {
            let mut o = serde_json::Map::new();
            for (n, v) in fields {
                o.insert(n.clone(), v.as_ref().map_or(Ok(J::Null), whole_json)?);
            }
            J::Object(o)
        }
        CqlValue::Decimal(d) => {
            let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
            J::String(decimal(bytes, scale)?)
        }
        CqlValue::Varint(v) => J::String(decimal(v.as_signed_bytes_be_slice(), 0)?),
        CqlValue::Date(d) => J::String(date(d.0)),
        CqlValue::Timestamp(t) => J::String(timestamp(t.0)),
        CqlValue::Empty => J::String(String::new()),
        other => crate::value::to_json(other),
    })
}

/// A two's-complement big-endian integer scaled by 10^-scale, as exact
/// digits (no exponent: that's [`Cell::Decimal`]).
fn decimal(bytes: &[u8], scale: i32) -> std::result::Result<String, String> {
    let too_long = || format!("un decimal de más de {MAX_DIGITS} dígitos no se copia (sería enorme sin exponente)");
    // ~2.41 digits per byte: past this it's surely over the limit.
    if bytes.len() > MAX_DIGITS * 10 / 24 + 8 {
        return Err(too_long());
    }
    let digits = big_to_string(bytes);
    let (neg, digits) = match digits.strip_prefix('-') {
        Some(d) => (true, d.to_string()),
        None => (false, digits),
    };
    let zero = digits == "0";
    let len = if scale <= 0 { if zero { 1 } else { digits.len() + scale.unsigned_abs() as usize } } else { digits.len().max(scale as usize + 1) };
    if len > MAX_DIGITS {
        return Err(too_long());
    }
    let body = if scale <= 0 {
        if zero {
            digits
        } else {
            digits + &"0".repeat(scale.unsigned_abs() as usize)
        }
    } else {
        let scale = scale as usize;
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = padded.split_at(padded.len() - scale);
        format!("{int}.{frac}")
    };
    Ok(if neg { format!("-{body}") } else { body })
}

/// Decimal digits of a signed big-endian integer, by 9-digit chunks.
fn big_to_string(bytes: &[u8]) -> String {
    let Some(first) = bytes.first() else { return "0".into() };
    let neg = first & 0x80 != 0;
    let mut mag = bytes.to_vec();
    if neg {
        for b in mag.iter_mut() {
            *b = !*b;
        }
        for b in mag.iter_mut().rev() {
            let (r, carry) = b.overflowing_add(1);
            *b = r;
            if !carry {
                break;
            }
        }
    }
    // Big-endian 32-bit limbs.
    let pad = (4 - mag.len() % 4) % 4;
    let mut padded = vec![0u8; pad];
    padded.extend_from_slice(&mag);
    let mut limbs: Vec<u32> = padded.chunks(4).map(|c| u32::from_be_bytes([c[0], c[1], c[2], c[3]])).collect();
    let mut start = limbs.iter().position(|l| *l != 0).unwrap_or(limbs.len());
    let mut chunks = Vec::new();
    while start < limbs.len() {
        let mut rem = 0u64;
        for l in &mut limbs[start..] {
            let cur = (rem << 32) | u64::from(*l);
            *l = (cur / 1_000_000_000) as u32;
            rem = cur % 1_000_000_000;
        }
        chunks.push(rem as u32);
        while start < limbs.len() && limbs[start] == 0 {
            start += 1;
        }
    }
    let mut s = String::with_capacity(chunks.len() * 9 + 1);
    if neg {
        s.push('-');
    }
    match chunks.split_last() {
        None => return "0".into(),
        Some((top, rest)) => {
            s.push_str(&top.to_string());
            for c in rest.iter().rev() {
                s.push_str(&format!("{c:09}"));
            }
        }
    }
    s
}

/// Days since 1970-01-01 as a proleptic Gregorian (year, month, day), for
/// any day count (chrono stops at ±262,143 years; CQL goes much further).
fn civil_from_days(z: i64) -> (i64, u32, u32) {
    let z = z + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z.rem_euclid(146_097);
    let yoe = (doe - doe / 1_460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = (doy - (153 * mp + 2) / 5 + 1) as u32;
    let m = if mp < 10 { mp + 3 } else { mp - 9 } as u32;
    let y = yoe + era * 400;
    (if m <= 2 { y + 1 } else { y }, m, d)
}

/// The inverse of [`civil_from_days`].
fn days_from_civil(y: i64, m: u32, d: u32) -> i64 {
    let y = if m <= 2 { y - 1 } else { y };
    let era = y.div_euclid(400);
    let yoe = y.rem_euclid(400);
    let (m, d) = (i64::from(m), i64::from(d));
    let doy = (153 * (if m > 2 { m - 3 } else { m + 9 }) + 2) / 5 + d - 1;
    let doe = yoe * 365 + yoe / 4 - yoe / 100 + doy;
    era * 146_097 + doe - 719_468
}

/// `YYYY-MM-DD`; a year outside 0000-9999 in ISO 8601's expanded form
/// (`-0001-01-01`, `+10000-01-01`), as chrono writes it.
fn fmt_days(days: i64) -> String {
    let (y, m, d) = civil_from_days(days);
    let year = match y {
        0..=9999 => format!("{y:04}"),
        y if y < 0 => format!("-{:04}", y.unsigned_abs()),
        y => format!("+{y}"),
    };
    format!("{year}-{m:02}-{d:02}")
}

/// CQL `date`: days since the epoch, offset by 2^31.
fn date(raw: u32) -> String {
    fmt_days(i64::from(raw) - (1i64 << 31))
}

/// CQL `timestamp`: milliseconds since the epoch, in UTC, over its whole
/// range.
fn timestamp(ms: i64) -> String {
    let (days, ms) = (ms.div_euclid(86_400_000), ms.rem_euclid(86_400_000));
    let (h, m, s, frac) = (ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60, ms % 1000);
    let day = fmt_days(days);
    if frac == 0 {
        format!("{day} {h:02}:{m:02}:{s:02}")
    } else {
        format!("{day} {h:02}:{m:02}:{s:02}.{frac:03}")
    }
}

fn hex(b: &[u8]) -> String {
    let mut s = String::with_capacity(2 + b.len() * 2);
    s.push_str("0x");
    for x in b {
        s.push_str(&format!("{x:02X}"));
    }
    s
}

// ---- cell → CQL ----

/// Why a cell doesn't convert.
#[derive(Debug)]
pub(crate) enum ConvError {
    /// Not a value of the column's type.
    Bad(String),
    /// A value the column can't hold without loss.
    Lossy(String),
}

impl From<String> for ConvError {
    fn from(s: String) -> Self {
        ConvError::Bad(s)
    }
}

impl std::fmt::Display for ConvError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            ConvError::Bad(m) | ConvError::Lossy(m) => f.write_str(m),
        }
    }
}

type Conv<T> = std::result::Result<T, ConvError>;

/// A cell as a value of the column's CQL type.
pub(crate) fn cell_to_cql(c: &Cell, t: &ColumnType) -> Conv<Option<CqlValue>> {
    match (c, t) {
        (Cell::Null, _) => Ok(None),
        // CQL's empty value (what the read gives for it) where a type has
        // one besides the empty text or blob.
        (Cell::Text(s), _)
            if s.is_empty()
                && t.supports_special_empty_value()
                && !matches!(t, ColumnType::Native(NativeType::Ascii | NativeType::Text | NativeType::Blob)) =>
        {
            Ok(Some(CqlValue::Empty))
        }
        (_, ColumnType::Native(n)) => native(c, n).map(Some),
        (Cell::Json(s) | Cell::Text(s), _) => {
            let j: J = serde_json::from_str(s).map_err(|e| format!("no es JSON válido para {t:?}: {e}"))?;
            // A JSON object keeps only the last of a repeated key.
            if let Some(k) = repeated_key(s) {
                return Err(ConvError::Lossy(format!("la clave «{k}» está repetida en el JSON y solo quedaría su último valor")));
            }
            json_to_cql(&j, t)
        }
        _ => Err(format!("{c:?} no se convierte a una colección").into()),
    }
}

/// A JSON value (a collection's element) as a value of type `t`.
fn json_to_cql(j: &J, t: &ColumnType) -> Conv<Option<CqlValue>> {
    let item = |j: &J, t: &ColumnType| json_to_cql(j, t)?.ok_or_else(|| ConvError::from("nulo dentro de una colección".to_string()));
    let array = |j: &J| match j {
        J::Array(a) => Ok(a.clone()),
        other => Err(format!("se esperaba una lista: {other}")),
    };
    Ok(Some(match (j, t) {
        (J::Null, _) => return Ok(None),
        (_, ColumnType::Native(n)) => {
            let cell = match j {
                J::Bool(b) => Cell::Bool(*b),
                J::Number(n) => Cell::from_json(&J::Number(n.clone())),
                J::String(s) => Cell::Text(s.clone()),
                other => Cell::Json(other.to_string()),
            };
            native(&cell, n)?
        }
        (_, ColumnType::Collection { typ: CollectionType::List(i), .. }) => {
            CqlValue::List(array(j)?.iter().map(|x| item(x, i)).collect::<Conv<_>>()?)
        }
        (_, ColumnType::Collection { typ: CollectionType::Set(i), .. }) => {
            CqlValue::Set(array(j)?.iter().map(|x| item(x, i)).collect::<Conv<_>>()?)
        }
        (_, ColumnType::Collection { typ: CollectionType::Map(k, v), .. }) => {
            let pairs = match j {
                J::Object(o) => o.iter().map(|(key, x)| Ok((item(&J::String(key.clone()), k)?, item(x, v)?))).collect::<Conv<Vec<_>>>()?,
                J::Array(a) => a
                    .iter()
                    .map(|p| match p {
                        J::Array(kv) if kv.len() == 2 => Ok((item(&kv[0], k)?, item(&kv[1], v)?)),
                        other => Err(format!("se esperaba un par [clave, valor]: {other}").into()),
                    })
                    .collect::<Conv<Vec<_>>>()?,
                other => return Err(format!("se esperaba un mapa: {other}").into()),
            };
            // Keys that convert to the same value (`1` and `01` as ints) would
            // leave only one entry.
            let mut seen = std::collections::HashSet::new();
            if let Some((k, _)) = pairs.iter().find(|(k, _)| !seen.insert(format!("{k:?}"))) {
                return Err(ConvError::Lossy(format!("la clave {k:?} está repetida en el mapa y solo quedaría una")));
            }
            CqlValue::Map(pairs)
        }
        (_, ColumnType::Vector { typ, .. }) => CqlValue::Vector(array(j)?.iter().map(|x| item(x, typ)).collect::<Conv<_>>()?),
        (_, ColumnType::Tuple(types)) => {
            let a = array(j)?;
            if a.len() > types.len() {
                return Err(ConvError::Lossy(format!("la tupla tiene {} elementos y el tipo solo {}; se perderían los demás", a.len(), types.len())));
            }
            CqlValue::Tuple(types.iter().enumerate().map(|(i, t)| json_to_cql(a.get(i).unwrap_or(&J::Null), t)).collect::<Conv<_>>()?)
        }
        (J::Object(o), ColumnType::UserDefinedType { definition, .. }) => {
            if let Some(k) = o.keys().find(|k| !definition.field_types.iter().any(|(n, _)| n == k.as_str())) {
                return Err(ConvError::Lossy(format!("el tipo {} no tiene el campo «{k}»; su valor se perdería", definition.name)));
            }
            CqlValue::UserDefinedType {
            keyspace: definition.keyspace.to_string(),
            name: definition.name.to_string(),
            fields: definition
                .field_types
                .iter()
                .map(|(n, ft)| Ok((n.to_string(), json_to_cql(o.get(n.as_ref()).unwrap_or(&J::Null), ft)?)))
                .collect::<Conv<_>>()?,
            }
        }
        (other, t) => return Err(format!("{other} no se convierte a {t:?}").into()),
    }))
}

/// The first key repeated within one object of `s`, valid JSON.
fn repeated_key(s: &str) -> Option<String> {
    let b = s.as_bytes();
    // One entry per open object (its keys) or array (`None`).
    let mut open: Vec<Option<std::collections::HashSet<String>>> = Vec::new();
    let mut key_next = false;
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'{' => {
                open.push(Some(Default::default()));
                key_next = true;
            }
            b'[' => {
                open.push(None);
                key_next = false;
            }
            b'}' | b']' => {
                open.pop();
                key_next = false;
            }
            b',' => key_next = matches!(open.last(), Some(Some(_))),
            b'"' => {
                let start = i;
                i += 1;
                while i < b.len() && b[i] != b'"' {
                    i += if b[i] == b'\\' { 2 } else { 1 };
                }
                if key_next {
                    let k: String = serde_json::from_str(s.get(start..=i)?).ok()?;
                    if let Some(Some(keys)) = open.last_mut() {
                        if !keys.insert(k.clone()) {
                            return Some(k);
                        }
                    }
                    key_next = false;
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

/// The cell as text (what a text column stores).
fn text(c: &Cell) -> String {
    match c {
        Cell::Null => String::new(),
        Cell::Bool(b) => b.to_string(),
        Cell::Int(i) => i.to_string(),
        Cell::UInt(u) => u.to_string(),
        Cell::Float(f) => f.to_string(),
        Cell::Bytes(b) => hex(b),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) | Cell::Json(s) => {
            s.clone()
        }
    }
}

fn native(c: &Cell, n: &NativeType) -> Conv<CqlValue> {
    let bad = || format!("{c:?} no se convierte a {}", format!("{n:?}").to_lowercase());
    Ok(match n {
        NativeType::Ascii => CqlValue::Ascii(text(c)),
        NativeType::Text => CqlValue::Text(text(c)),
        NativeType::Boolean => CqlValue::Boolean(match c {
            Cell::Bool(b) => *b,
            // A 0/1 flag; any other number isn't a boolean.
            Cell::Int(0) | Cell::UInt(0) => false,
            Cell::Int(1) | Cell::UInt(1) => true,
            Cell::Int(_) | Cell::UInt(_) => return Err(bad().into()),
            _ => match text(c).trim().to_ascii_lowercase().as_str() {
                "true" | "t" | "1" | "yes" => true,
                "false" | "f" | "0" | "no" => false,
                _ => return Err(bad().into()),
            },
        }),
        NativeType::Blob => CqlValue::Blob(match c {
            Cell::Bytes(b) => b.clone(),
            _ => {
                let s = text(c);
                match s.strip_prefix("0x").or_else(|| s.strip_prefix("0X")) {
                    Some(h) => unhex(h).ok_or_else(bad)?,
                    None => s.into_bytes(),
                }
            }
        }),
        NativeType::TinyInt => CqlValue::TinyInt(i8::try_from(int(c).ok_or_else(bad)?).map_err(|_| bad())?),
        NativeType::SmallInt => CqlValue::SmallInt(i16::try_from(int(c).ok_or_else(bad)?).map_err(|_| bad())?),
        NativeType::Int => CqlValue::Int(i32::try_from(int(c).ok_or_else(bad)?).map_err(|_| bad())?),
        NativeType::BigInt => CqlValue::BigInt(i64::try_from(int(c).ok_or_else(bad)?).map_err(|_| bad())?),
        NativeType::Counter => CqlValue::Counter(Counter(i64::try_from(int(c).ok_or_else(bad)?).map_err(|_| bad())?)),
        NativeType::Double => {
            let f = float(c).ok_or_else(bad)?;
            exact_int(c, f as i128, "double")?;
            CqlValue::Double(f)
        }
        NativeType::Float => {
            let f = float(c).ok_or_else(bad)? as f32;
            exact_int(c, f as i128, "float")?;
            CqlValue::Float(f)
        }
        NativeType::Decimal => {
            let (bytes, scale) = parse_decimal(&text(c)).ok_or_else(bad)?;
            CqlValue::Decimal(CqlDecimal::from_signed_be_bytes_and_exponent(bytes, scale))
        }
        NativeType::Varint => {
            let s = text(c);
            let (bytes, scale) = parse_decimal(&s).ok_or_else(bad)?;
            let bytes = match scale {
                0 => bytes,
                // `12e3`, `1.5e2`: the digits and the exponent's zeros.
                s_ if s_ < 0 => {
                    let mant = s.trim().split(['e', 'E']).next().unwrap_or_default().replace('.', "");
                    if mant.len() + s_.unsigned_abs() as usize > MAX_DIGITS {
                        return Err(bad().into());
                    }
                    parse_decimal(&format!("{mant}{}", "0".repeat(s_.unsigned_abs() as usize))).ok_or_else(bad)?.0
                }
                // `12.000`
                _ => parse_decimal(&int(c).ok_or_else(bad)?.to_string()).ok_or_else(bad)?.0,
            };
            CqlValue::Varint(CqlVarint::from_signed_bytes_be(bytes))
        }
        NativeType::Date => {
            let s = text(c);
            if carries_time(&s) {
                return Err(ConvError::Lossy(format!("{s}: la columna date no guarda la hora, que se perdería")));
            }
            let days = parse_date(&s).ok_or_else(bad)?;
            CqlValue::Date(CqlDate(u32::try_from(days + (1i64 << 31)).map_err(|_| bad())?))
        }
        NativeType::Time => {
            let s = text(c);
            // A date-time's date would be dropped: only a bare time fits.
            if split_date(s.trim()).is_some() {
                return Err(ConvError::Lossy(format!("{s}: la columna time no guarda la fecha, que se perdería")));
            }
            // A zone would be dropped too: `time` has none.
            let bare = s.trim();
            if bare.ends_with(['Z', 'z']) || bare.get(1..).is_some_and(|r| r.contains(['+', '-'])) {
                return Err(ConvError::Lossy(format!("{s}: la columna time no guarda la zona horaria, que se perdería")));
            }
            let t = parse_time(&s).ok_or_else(bad)?;
            CqlValue::Time(CqlTime(i64::from(t.num_seconds_from_midnight()) * 1_000_000_000 + i64::from(t.nanosecond())))
        }
        NativeType::Timestamp => CqlValue::Timestamp(CqlTimestamp(match c {
            Cell::Int(ms) => *ms,
            _ => {
                let s = text(c);
                let (ms, finer) = parse_timestamp(&s).ok_or_else(bad)?;
                if finer {
                    return Err(ConvError::Lossy(format!(
                        "{s}: el timestamp de Cassandra guarda milisegundos y este valor tiene más precisión, que se perdería"
                    )));
                }
                ms
            }
        })),
        NativeType::Uuid => CqlValue::Uuid(*uuid(c).ok_or_else(bad)?.as_ref()),
        NativeType::Timeuuid => CqlValue::Timeuuid(uuid(c).ok_or_else(bad)?),
        NativeType::Inet => CqlValue::Inet(text(c).trim().parse().map_err(|_| bad())?),
        NativeType::Duration => CqlValue::Duration(parse_duration(&text(c)).ok_or_else(bad)?),
        _ => return Err(bad().into()),
    })
}

/// An integer cell must be exactly `as_float` (`9007199254740993` isn't a
/// double).
fn exact_int(c: &Cell, as_float: i128, typ: &str) -> Conv<()> {
    let i = match c {
        Cell::Int(i) => i128::from(*i),
        Cell::UInt(u) => i128::from(*u),
        _ => return Ok(()),
    };
    if i == as_float {
        Ok(())
    } else {
        Err(ConvError::Lossy(format!("{i} no cabe exacto en un {typ}; se redondearía")))
    }
}

/// A date-time whose time isn't midnight (or carries a zone): a `date`
/// would drop it.
fn carries_time(s: &str) -> bool {
    let Some((_, rest)) = split_date(s.trim()) else { return false };
    let Some(time) = rest.strip_prefix([' ', 'T', 't']) else { return false };
    let time = time.trim().trim_end_matches(['Z', 'z']);
    !(parse_time(time).is_some() && time.bytes().all(|b| matches!(b, b'0' | b':' | b'.')))
}

fn int(c: &Cell) -> Option<i128> {
    match c {
        Cell::Int(i) => Some((*i).into()),
        Cell::UInt(u) => Some((*u).into()),
        Cell::Bool(b) => Some((*b).into()),
        Cell::Float(f) if f.fract() == 0.0 && f.is_finite() => Some(*f as i128),
        _ => {
            let s = text(c);
            let s = s.trim();
            // `12.000` is still an integer.
            let s = match s.split_once('.') {
                Some((i, f)) if f.bytes().all(|b| b == b'0') => i,
                _ => s,
            };
            s.parse().ok()
        }
    }
}

fn float(c: &Cell) -> Option<f64> {
    match c {
        Cell::Float(f) => Some(*f),
        Cell::Int(i) => Some(*i as f64),
        Cell::UInt(u) => Some(*u as f64),
        _ => text(c).trim().parse().ok(),
    }
}

fn uuid(c: &Cell) -> Option<CqlTimeuuid> {
    match c {
        Cell::Bytes(b) if b.len() == 16 => {
            let h: String = b.iter().map(|x| format!("{x:02x}")).collect();
            CqlTimeuuid::from_str(&h).ok()
        }
        _ => CqlTimeuuid::from_str(text(c).trim()).ok(),
    }
}

fn unhex(h: &str) -> Option<Vec<u8>> {
    if !h.len().is_multiple_of(2) {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
}

/// `-12.345`, `1e-3`… as a two's-complement big-endian integer and its scale.
pub(crate) fn parse_decimal(s: &str) -> Option<(Vec<u8>, i32)> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s.strip_prefix('+').unwrap_or(s)),
    };
    let (mant, exp) = match s.split_once(['e', 'E']) {
        Some((m, e)) => (m, e.parse::<i32>().ok()?),
        None => (s, 0),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    if int.is_empty() && frac.is_empty() {
        return None;
    }
    let digits = format!("{int}{frac}");
    let scale = i32::try_from(frac.len()).ok()?.checked_sub(exp)?;
    Some((big_bytes(neg, &digits)?, scale))
}

/// Decimal digits as a two's-complement big-endian integer, shortest form.
fn big_bytes(neg: bool, digits: &str) -> Option<Vec<u8>> {
    if digits.is_empty() || digits.len() > MAX_DIGITS || !digits.bytes().all(|d| d.is_ascii_digit()) {
        return None;
    }
    // Little-endian 32-bit limbs, 9 digits at a time.
    let mut limbs: Vec<u32> = vec![0];
    let head = match digits.len() % 9 {
        0 => 9,
        n => n,
    };
    let mut rest = digits;
    let mut take = head;
    while !rest.is_empty() {
        let (chunk, tail) = rest.split_at(take);
        let mul = 10u64.pow(chunk.len() as u32);
        let mut carry: u64 = chunk.parse().ok()?;
        for l in limbs.iter_mut() {
            let v = u64::from(*l) * mul + carry;
            *l = v as u32;
            carry = v >> 32;
        }
        if carry > 0 {
            limbs.push(carry as u32);
        }
        (rest, take) = (tail, 9);
    }
    let mut mag: Vec<u8> = limbs.iter().rev().flat_map(|l| l.to_be_bytes()).collect();
    let lead = mag.iter().take_while(|b| **b == 0).count().min(mag.len() - 1);
    mag.drain(..lead);
    // Room for the sign bit.
    if mag[0] & 0x80 != 0 {
        mag.insert(0, 0);
    }
    if neg {
        for b in mag.iter_mut() {
            *b = !*b;
        }
        for b in mag.iter_mut().rev() {
            let (r, carry) = b.overflowing_add(1);
            *b = r;
            if !carry {
                break;
            }
        }
    }
    while mag.len() > 1 && ((mag[0] == 0 && mag[1] & 0x80 == 0) || (mag[0] == 0xFF && mag[1] & 0x80 != 0)) {
        mag.remove(0);
    }
    Some(mag)
}

/// `[±]YYYY-MM-DD` (the year may have more digits, as ISO 8601's expanded
/// form) and what follows it.
fn split_date(s: &str) -> Option<((i64, u32, u32), &str)> {
    let (sign, body) = match s.as_bytes().first()? {
        b'-' => (-1, &s[1..]),
        b'+' => (1, &s[1..]),
        _ => (1, s),
    };
    let (y, rest) = body.split_at(body.find('-')?);
    if !(4..=12).contains(&y.len()) || !y.bytes().all(|b| b.is_ascii_digit()) {
        return None;
    }
    let two = |s: &str| if s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit()) { s.parse::<u32>().ok() } else { None };
    let m = two(rest.get(1..3)?)?;
    let d = two(rest.get(4..6)?)?;
    if rest.get(..1)? != "-" || rest.get(3..4)? != "-" {
        return None;
    }
    let y = sign * y.parse::<i64>().ok()?;
    let leap = y.rem_euclid(4) == 0 && (y.rem_euclid(100) != 0 || y.rem_euclid(400) == 0);
    let last = match m {
        1 | 3 | 5 | 7 | 8 | 10 | 12 => 31,
        4 | 6 | 9 | 11 => 30,
        2 if leap => 29,
        2 => 28,
        _ => return None,
    };
    (1..=last).contains(&d).then_some(((y, m, d), &rest[6..]))
}

/// A date (a date-time's date) as days since the epoch, over CQL's whole
/// range.
fn parse_date(s: &str) -> Option<i64> {
    let ((y, m, d), rest) = split_date(s.trim())?;
    (rest.is_empty() || rest.starts_with([' ', 'T', 't'])).then(|| days_from_civil(y, m, d))
}

fn parse_time(s: &str) -> Option<NaiveTime> {
    // Drop a zone the time may carry (`12:00:00+01:00`, `12:00:00Z`).
    let s = s.trim().trim_end_matches('Z');
    let s = match s.rfind(['+', '-']) {
        Some(i) => &s[..i],
        None => s,
    };
    NaiveTime::parse_from_str(s, "%H:%M:%S%.f").or_else(|_| NaiveTime::parse_from_str(s, "%H:%M")).ok()
}

/// A date-time as ms since the epoch, and whether it had more precision
/// than a millisecond (that part isn't in the ms). With a zone (`Z`,
/// `±HH:MM`, `±HHMM`, `±HH`) it's that instant; without one, UTC; a bare
/// date, its midnight UTC. Years as [`split_date`] takes them, over the
/// whole range of a CQL `timestamp`.
pub(crate) fn parse_timestamp(s: &str) -> Option<(i64, bool)> {
    let s = s.trim();
    // Neo4j's `…[Europe/Madrid]`: the offset before it is enough.
    let s = s.split('[').next().unwrap_or(s).trim_end();
    let ((y, m, d), rest) = split_date(s)?;
    let days = i128::from(days_from_civil(y, m, d));
    let (mut ms, mut finer) = (days * 86_400_000, false);
    if !rest.is_empty() {
        let rest = rest.strip_prefix([' ', 'T', 't'])?;
        let (time, zone) = rest.split_at(rest.find(['Z', 'z', '+', '-']).unwrap_or(rest.len()));
        let time = time.trim_end();
        let (hms, frac) = time.split_once('.').unwrap_or((time, ""));
        let mut parts = hms.split(':');
        let num = |p: Option<&str>, max: u32| -> Option<u32> {
            let p = p?;
            (p.len() == 2 && p.bytes().all(|b| b.is_ascii_digit())).then(|| p.parse().ok()).flatten().filter(|v| *v <= max)
        };
        let h = num(parts.next(), 23)?;
        let mi = num(parts.next(), 59)?;
        let sec = match parts.next() {
            Some(p) => num(Some(p), 59)?,
            None if frac.is_empty() => 0,
            None => return None,
        };
        if parts.next().is_some() || !frac.bytes().all(|b| b.is_ascii_digit()) || time.ends_with('.') {
            return None;
        }
        let milli: i128 = format!("{:0<3}", frac.get(..3).unwrap_or(frac)).parse().ok()?;
        finer = frac.bytes().skip(3).any(|b| b != b'0');
        let offset_min: i128 = match zone {
            "" | "Z" | "z" => 0,
            z => {
                let sign = if z.starts_with('-') { -1 } else { 1 };
                let z = z[1..].replace(':', "");
                let two = |s: Option<&str>| s.filter(|s| s.len() == 2 && s.bytes().all(|b| b.is_ascii_digit())).and_then(|s| s.parse::<i128>().ok());
                let (oh, om) = match z.len() {
                    2 => (two(Some(&z))?, 0),
                    4 => (two(z.get(..2))?, two(z.get(2..))?),
                    _ => return None,
                };
                if oh > 23 || om > 59 {
                    return None;
                }
                sign * (oh * 60 + om)
            }
        };
        ms += i128::from(h * 3600 + mi * 60 + sec) * 1000 + milli - offset_min * 60_000;
    }
    Some((i64::try_from(ms).ok()?, finer))
}

/// CQL notation (`1y2mo3d4h5m6s7ms8us9ns`, what the read gives) or ISO
/// 8601 (`P1Y2M3DT4H5M6.5S`).
pub(crate) fn parse_duration(s: &str) -> Option<CqlDuration> {
    let s = s.trim();
    let (neg, s) = match s.strip_prefix('-') {
        Some(r) => (true, r),
        None => (false, s),
    };
    if s.is_empty() {
        return None;
    }
    let (mut months, mut days, mut nanos) = (0i64, 0i64, 0i128);
    if let Some(iso) = s.strip_prefix('P').or_else(|| s.strip_prefix('p')) {
        let (date, time) = iso.split_once(['T', 't']).unwrap_or((iso, ""));
        for (num, unit) in units(date)? {
            let n = num.parse::<i64>().ok()?;
            match unit.to_ascii_uppercase().as_str() {
                "Y" => months += n * 12,
                "M" => months += n,
                "W" => days += n * 7,
                "D" => days += n,
                _ => return None,
            }
        }
        for (num, unit) in units(time)? {
            let secs: f64 = num.parse().ok()?;
            let mult: i128 = match unit.to_ascii_uppercase().as_str() {
                "H" => 3_600_000_000_000,
                "M" => 60_000_000_000,
                "S" => 1_000_000_000,
                _ => return None,
            };
            nanos += match num.split_once('.') {
                // Exact fractional seconds.
                Some((i, f)) if mult == 1_000_000_000 => {
                    i.parse::<i128>().ok()? * mult + format!("{f:0<9}").get(..9)?.parse::<i128>().ok()?
                }
                _ => (secs * mult as f64) as i128,
            };
        }
    } else {
        for (num, unit) in units(s)? {
            let n = num.parse::<i64>().ok()?;
            match unit.to_ascii_lowercase().as_str() {
                "y" => months += n * 12,
                "mo" => months += n,
                "w" => days += n * 7,
                "d" => days += n,
                "h" => nanos += i128::from(n) * 3_600_000_000_000,
                "m" => nanos += i128::from(n) * 60_000_000_000,
                "s" => nanos += i128::from(n) * 1_000_000_000,
                "ms" => nanos += i128::from(n) * 1_000_000,
                "us" | "µs" => nanos += i128::from(n) * 1_000,
                "ns" => nanos += i128::from(n),
                _ => return None,
            }
        }
    }
    let sign = if neg { -1 } else { 1 };
    Some(CqlDuration {
        months: i32::try_from(months * sign).ok()?,
        days: i32::try_from(days * sign).ok()?,
        nanoseconds: i64::try_from(nanos * i128::from(sign)).ok()?,
    })
}

/// `12h30m` → [("12", "h"), ("30", "m")].
fn units(s: &str) -> Option<Vec<(&str, &str)>> {
    let mut out = Vec::new();
    let mut rest = s;
    while !rest.is_empty() {
        let n = rest.find(|c: char| !(c.is_ascii_digit() || c == '.')).unwrap_or(rest.len());
        let (num, tail) = rest.split_at(n);
        let u = tail.find(|c: char| c.is_ascii_digit()).unwrap_or(tail.len());
        let (unit, next) = tail.split_at(u);
        if num.is_empty() || unit.is_empty() {
            return None;
        }
        out.push((num, unit));
        rest = next;
    }
    Some(out)
}

#[cfg(test)]
mod tests {
    #[test]
    fn static_only_rows_keep_the_partition_and_static_values() {
        let kinds: Vec<String> = ["partition_key", "clustering", "regular", "static"].map(str::to_string).to_vec();
        let so = super::StaticOnly::of(&kinds).unwrap();
        let cols: Vec<String> = ["p", "c", "v", "s"].map(str::to_string).to_vec();
        let t = |s: &str| Some(CqlValue::Text(s.into()));
        let full = vec![t("full"), Some(CqlValue::Int(1)), t("x"), t("st2")];
        assert!(!so.applies(&full));
        let solo = vec![t("solo"), None, None, t("estatico")];
        assert!(so.applies(&solo));
        assert_eq!(so.values(solo, &cols).unwrap(), vec![t("solo"), t("estatico")]);
        assert!(so.values(vec![t("x"), None, t("v"), None], &cols).unwrap_err().to_string().contains("columna v"));
        // No static column: nothing to do.
        assert!(super::StaticOnly::of(&kinds[..3]).is_none());
    }

    use super::*;
    use std::borrow::Cow;
    use std::sync::Arc;

    fn nat(n: NativeType) -> ColumnType<'static> {
        ColumnType::Native(n)
    }

    /// Cell → CQL → cell.
    fn round(c: Cell, n: NativeType) -> Cell {
        to_cell(cell_to_cql(&c, &nat(n)).unwrap()).unwrap()
    }

    fn read(v: CqlValue) -> Cell {
        to_cell(Some(v)).unwrap()
    }

    #[test]
    fn dates_outside_four_digit_years() {
        // Before year 1 and past 9999: ISO 8601's expanded years, as chrono.
        for (raw, text) in [
            (2_147_483_648u32, "1970-01-01"),
            (2_146_763_755, "-0001-01-01"),
            (2_150_416_545, "+10000-01-01"),
            (2_146_764_120, "0000-01-01"),
            (2_150_416_544, "9999-12-31"),
        ] {
            assert_eq!(read(CqlValue::Date(CqlDate(raw))), Cell::Date(text.into()), "{raw}");
            assert_eq!(cell_to_cql(&Cell::Date(text.into()), &nat(NativeType::Date)).unwrap(), Some(CqlValue::Date(CqlDate(raw))), "{text}");
        }
        // CQL's whole range, far past chrono's.
        for raw in [0u32, 1, 12_345, u32::MAX - 1, u32::MAX] {
            let c = read(CqlValue::Date(CqlDate(raw)));
            assert_eq!(cell_to_cql(&c, &nat(NativeType::Date)).unwrap(), Some(CqlValue::Date(CqlDate(raw))), "{c:?}");
        }
        assert_eq!(read(CqlValue::Date(CqlDate(0))), Cell::Date("-5877641-06-23".into()));
        assert!(cell_to_cql(&Cell::Date("2023-02-29".into()), &nat(NativeType::Date)).is_err());
        assert!(cell_to_cql(&Cell::Date("-99999999-01-01".into()), &nat(NativeType::Date)).is_err());
    }

    #[test]
    fn timestamps_over_the_whole_range() {
        for ms in [0, 1, -1, 1_704_067_200_123, -62_167_219_200_001, 253_402_300_800_000, i64::MAX, i64::MIN, i64::MAX - 999] {
            let c = read(CqlValue::Timestamp(CqlTimestamp(ms)));
            let Cell::DateTimeTz(s) = &c else { panic!("{c:?}") };
            assert!(s.ends_with("+00:00") && s.contains(' '), "{s}");
            assert_eq!(cell_to_cql(&c, &nat(NativeType::Timestamp)).unwrap(), Some(CqlValue::Timestamp(CqlTimestamp(ms))), "{s}");
        }
        assert_eq!(read(CqlValue::Timestamp(CqlTimestamp(-1))), Cell::DateTimeTz("1969-12-31 23:59:59.999+00:00".into()));
        assert_eq!(read(CqlValue::Timestamp(CqlTimestamp(253_402_300_800_000))), Cell::DateTimeTz("+10000-01-01 00:00:00+00:00".into()));
        assert_eq!(read(CqlValue::Timestamp(CqlTimestamp(i64::MAX))), Cell::DateTimeTz("+292278994-08-17 07:12:55.807+00:00".into()));
        assert_eq!(parse_timestamp("2024-01-01T00:00:00Z[UTC]"), Some((1_704_067_200_000, false)));
        assert_eq!(parse_timestamp("2024-01-01 03:00:00+0300"), Some((1_704_067_200_000, false)));
        assert_eq!(parse_timestamp("2024-01-01 03:00+03"), Some((1_704_067_200_000, false)));
        assert_eq!(parse_timestamp("2024-01-01 00:00:00.123000+00:00"), Some((1_704_067_200_123, false)));
        assert_eq!(parse_timestamp("2024-01-01"), Some((1_704_067_200_000, false)));
        assert_eq!(parse_timestamp("2024-01-01 25:00:00"), None);
        assert_eq!(parse_timestamp("2024-01-01 10:00:00."), None);
        assert_eq!(parse_timestamp("+292278994-08-17 07:12:55.808"), None);
    }

    #[test]
    fn finer_than_a_millisecond_is_refused() {
        let e = cell_to_cql(&Cell::DateTimeTz("2024-01-01 00:00:00.123456+00:00".into()), &nat(NativeType::Timestamp)).unwrap_err();
        assert!(matches!(e, ConvError::Lossy(_)), "{e:?}");
        let e = cell_to_cql(&Cell::DateTime("2024-01-01 00:00:00.1234".into()), &nat(NativeType::Timestamp)).unwrap_err();
        assert!(matches!(e, ConvError::Lossy(_)), "{e:?}");
        let types = [nat(NativeType::Timestamp)];
        let err = row_values(&[Cell::DateTimeTz("2024-01-01 00:00:00.000001+00:00".into())], &types, &["ts".into()]).unwrap_err();
        assert!(matches!(err, Error::Unsupported(ref m) if m.contains("milisegundos")), "{err:?}");
        // Zeros past the millisecond lose nothing.
        assert!(cell_to_cql(&Cell::DateTimeTz("2024-01-01 00:00:00.123000000+00:00".into()), &nat(NativeType::Timestamp)).is_ok());
    }

    #[test]
    fn long_varints_stay_digits() {
        // 10,000 digits: over the 4,096 bytes the grid's formatter handles.
        let digits: String = (0..10_000).map(|i| char::from(b'1' + (i % 9) as u8)).collect();
        for d in [digits.clone(), format!("-{digits}")] {
            let v = cell_to_cql(&Cell::Decimal(d.clone()), &nat(NativeType::Varint)).unwrap().unwrap();
            assert!(matches!(&v, CqlValue::Varint(b) if b.as_signed_bytes_be_slice().len() > 4096));
            assert_eq!(read(v), Cell::Decimal(d.clone()));
        }
        let dec = format!("{digits}.{digits}");
        let v = cell_to_cql(&Cell::Decimal(dec.clone()), &nat(NativeType::Decimal)).unwrap().unwrap();
        assert_eq!(read(v), Cell::Decimal(dec));
        // In a collection too.
        let list = ColumnType::Collection { frozen: false, typ: CollectionType::List(Box::new(nat(NativeType::Varint))) };
        let v = cell_to_cql(&Cell::Json(format!("[\"{digits}\"]")), &list).unwrap().unwrap();
        assert_eq!(read(v), Cell::Json(format!("[\"{digits}\"]")));
        // Past the limit: a clear error, never hex.
        let huge = CqlValue::Varint(CqlVarint::from_signed_bytes_be(vec![0x7f; 100_000]));
        assert!(to_cell(Some(huge)).unwrap_err().contains("dígitos"));
        let wide = CqlValue::Decimal(CqlDecimal::from_signed_be_bytes_and_exponent(vec![1], i32::MIN));
        assert!(to_cell(Some(wide)).is_err());
        // Legal decimals whose plain digits pass the limit: refused, not cut.
        for e in [-i32::MAX, i32::MAX] {
            let d = CqlValue::Decimal(CqlDecimal::from_signed_be_bytes_and_exponent(vec![5], e));
            assert!(to_cell(Some(d.clone())).unwrap_err().contains("dígitos"), "{e}");
            let list = CqlValue::List(vec![d]);
            assert!(to_cell(Some(list)).unwrap_err().contains("dígitos"), "{e}");
        }
        assert!(cell_to_cql(&Cell::Decimal("1e2000000000".into()), &nat(NativeType::Varint)).is_err());
        assert!(parse_decimal("1e-2147483648").is_none());
    }

    #[test]
    fn in_flight_cost_is_the_converted_values() {
        // A list<int> of 500k ones: ~1 MB of JSON, ~36 MB of `CqlValue`s.
        let n = 500_000;
        let json = format!("[{}]", vec!["1"; n].join(","));
        let list = ColumnType::Collection { frozen: false, typ: CollectionType::List(Box::new(nat(NativeType::Int))) };
        let cell = Cell::Json(json);
        let values = vec![cell_to_cql(&cell, &list).unwrap()];
        let cost = row_cost(&values);
        assert!(cost >= n * std::mem::size_of::<CqlValue>(), "{cost}");
        assert!(cost > 30 * cell.size(), "{cost} vs {}", cell.size());
        // Such a row goes alone: the cap would hold none beside it.
        assert!(cost > IN_FLIGHT_BYTES);
        // Text and blobs count their bytes; nested values too.
        let t = vec![Some(CqlValue::Text("x".repeat(1000))), None];
        assert!(row_cost(&t) >= 2 * 1000);
        let udt = CqlValue::UserDefinedType { keyspace: "ks".into(), name: "u".into(), fields: vec![("f".into(), Some(CqlValue::Blob(vec![0; 4096])))] };
        assert!(row_cost(&[Some(CqlValue::Map(vec![(CqlValue::Int(1), udt)]))]) >= 2 * 4096);
    }

    #[test]
    fn empty_is_not_null() {
        for (n, v) in [(NativeType::Int, CqlValue::Empty), (NativeType::Timestamp, CqlValue::Empty), (NativeType::Uuid, CqlValue::Empty)] {
            let c = read(v);
            assert_eq!(c, Cell::Text(String::new()));
            assert_eq!(cell_to_cql(&c, &nat(n)).unwrap(), Some(CqlValue::Empty));
        }
        assert_eq!(cell_to_cql(&Cell::Text(String::new()), &nat(NativeType::Text)).unwrap(), Some(CqlValue::Text(String::new())));
        assert_eq!(cell_to_cql(&Cell::Text(String::new()), &nat(NativeType::Blob)).unwrap(), Some(CqlValue::Blob(vec![])));
        assert!(cell_to_cql(&Cell::Text(String::new()), &nat(NativeType::Duration)).is_err());
    }

    #[test]
    fn pages_follow_the_widest_row() {
        assert_eq!(next_page(1, 100), 4);
        assert_eq!(next_page(4096, 100), PAGE);
        // 100 KiB rows: about 40 per page, never 5,000 (~500 MB).
        assert_eq!(next_page(1024, 100 * 1024), 40);
        assert_eq!(next_page(1, 64 * 1024 * 1024), 1);
    }

    #[test]
    fn scalars_round_trip() {
        assert_eq!(round(Cell::Int(5), NativeType::Int), Cell::Int(5));
        assert_eq!(round(Cell::Int(i64::MAX), NativeType::BigInt), Cell::Int(i64::MAX));
        assert_eq!(round(Cell::Int(-7), NativeType::TinyInt), Cell::Int(-7));
        assert!(cell_to_cql(&Cell::Int(300), &nat(NativeType::TinyInt)).is_err());
        assert_eq!(round(Cell::Text("42".into()), NativeType::SmallInt), Cell::Int(42));
        assert_eq!(round(Cell::Float(1.1), NativeType::Float), Cell::Float(1.1));
        assert_eq!(round(Cell::Float(0.1), NativeType::Double), Cell::Float(0.1));
        assert_eq!(round(Cell::Bool(true), NativeType::Boolean), Cell::Bool(true));
        assert_eq!(round(Cell::Text("x".into()), NativeType::Text), Cell::Text("x".into()));
        assert_eq!(round(Cell::Int(3), NativeType::Text), Cell::Text("3".into()));
        let blob = vec![0u8, 1, 0xff, 0x80];
        assert_eq!(round(Cell::Bytes(blob.clone()), NativeType::Blob), Cell::Bytes(blob));
        assert_eq!(round(Cell::Text("0xCAFE".into()), NativeType::Blob), Cell::Bytes(vec![0xca, 0xfe]));
        assert_eq!(round(Cell::Text("10.0.0.1".into()), NativeType::Inet), Cell::Text("10.0.0.1".into()));
        assert_eq!(cell_to_cql(&Cell::Null, &nat(NativeType::Int)).unwrap(), None);
    }

    #[test]
    fn exact_numbers_round_trip() {
        for d in ["0", "12.34", "-12.34", "0.005", "-0.005", "123456789012345678901234567890.123456789", "-128", "127", "128", "-129", "255", "256"] {
            assert_eq!(round(Cell::Decimal(d.into()), NativeType::Decimal), Cell::Decimal(d.into()), "{d}");
        }
        assert_eq!(round(Cell::Decimal("1.5e2".into()), NativeType::Decimal), Cell::Decimal("150".into()));
        assert_eq!(round(Cell::Float(2.5), NativeType::Decimal), Cell::Decimal("2.5".into()));
        for v in ["0", "-1", "18446744073709551616", "-123456789012345678901234567890"] {
            assert_eq!(round(Cell::Decimal(v.into()), NativeType::Varint), Cell::Decimal(v.into()), "{v}");
        }
        assert_eq!(round(Cell::Decimal("12e3".into()), NativeType::Varint), Cell::Decimal("12000".into()));
        assert!(cell_to_cql(&Cell::Decimal("1.5".into()), &nat(NativeType::Varint)).is_err());
    }

    #[test]
    fn temporal_round_trip() {
        assert_eq!(round(Cell::Date("2024-01-31".into()), NativeType::Date), Cell::Date("2024-01-31".into()));
        assert_eq!(round(Cell::Date("1960-05-01".into()), NativeType::Date), Cell::Date("1960-05-01".into()));
        assert_eq!(round(Cell::DateTime("2024-01-31 00:00:00".into()), NativeType::Date), Cell::Date("2024-01-31".into()));
        assert_eq!(round(Cell::DateTimeTz("2024-01-31T00:00:00.000Z".into()), NativeType::Date), Cell::Date("2024-01-31".into()));
        assert_eq!(round(Cell::Time("01:02:03.5".into()), NativeType::Time), Cell::Time("01:02:03.500000000".into()));
        assert_eq!(round(Cell::Time("01:02:03".into()), NativeType::Time), Cell::Time("01:02:03".into()));
        assert_eq!(
            round(Cell::DateTimeTz("2024-01-31 13:45:00.123+00:00".into()), NativeType::Timestamp),
            Cell::DateTimeTz("2024-01-31 13:45:00.123+00:00".into())
        );
        assert_eq!(
            round(Cell::DateTimeTz("2024-01-31T10:45:00-03:00".into()), NativeType::Timestamp),
            Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into())
        );
        assert_eq!(round(Cell::DateTime("2024-01-31 13:45:00".into()), NativeType::Timestamp), Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()));
        assert_eq!(round(Cell::Date("2024-01-31".into()), NativeType::Timestamp), Cell::DateTimeTz("2024-01-31 00:00:00+00:00".into()));
        assert_eq!(round(Cell::Text("1y2mo3d4h5ms".into()), NativeType::Duration), Cell::Text("1y2mo3d4h5ms".into()));
        assert_eq!(round(Cell::Text("P1Y2M3DT1H2M5.5S".into()), NativeType::Duration), Cell::Text("1y2mo3d1h2m5s500ms".into()));
        assert_eq!(round(Cell::Text("-2d".into()), NativeType::Duration), Cell::Text("-2d".into()));
    }

    #[test]
    fn uuids_round_trip() {
        let u = "5a1c395e-b5d1-4ec5-9e1a-6f4a2f5f3e10";
        assert_eq!(round(Cell::Uuid(u.into()), NativeType::Uuid), Cell::Uuid(u.into()));
        let t = "d2177dd0-eaa2-11de-a572-001b779c76e3";
        assert_eq!(round(Cell::Uuid(t.into()), NativeType::Timeuuid), Cell::Uuid(t.into()));
        let bytes: Vec<u8> = (0..16).collect();
        assert_eq!(round(Cell::Bytes(bytes), NativeType::Uuid), Cell::Uuid("00010203-0405-0607-0809-0a0b0c0d0e0f".into()));
    }

    #[test]
    fn collections_from_json() {
        let list = ColumnType::Collection { frozen: false, typ: CollectionType::List(Box::new(nat(NativeType::Int))) };
        let v = cell_to_cql(&Cell::Json("[1,2]".into()), &list).unwrap().unwrap();
        assert_eq!(v, CqlValue::List(vec![CqlValue::Int(1), CqlValue::Int(2)]));
        assert_eq!(to_cell(Some(v)).unwrap(),Cell::Json("[1,2]".into()));

        let map = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(Box::new(nat(NativeType::Text)), Box::new(nat(NativeType::Blob))),
        };
        let v = cell_to_cql(&Cell::Json(r#"{"k":"0xCAFE"}"#.into()), &map).unwrap().unwrap();
        assert_eq!(v, CqlValue::Map(vec![(CqlValue::Text("k".into()), CqlValue::Blob(vec![0xca, 0xfe]))]));
        let imap = ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(Box::new(nat(NativeType::Int)), Box::new(nat(NativeType::Boolean))),
        };
        let v = cell_to_cql(&Cell::Json("[[1,true]]".into()), &imap).unwrap().unwrap();
        assert_eq!(to_cell(Some(v)).unwrap(),Cell::Json("[[1,true]]".into()));

        let udt = ColumnType::UserDefinedType {
            frozen: true,
            definition: Arc::new(scylla::frame::response::result::UserDefinedType {
                name: Cow::Borrowed("address"),
                keyspace: Cow::Borrowed("ks"),
                field_types: vec![(Cow::Borrowed("city"), nat(NativeType::Text)), (Cow::Borrowed("zip"), nat(NativeType::Int))],
            }),
        };
        let v = cell_to_cql(&Cell::Json(r#"{"city":"Rosario"}"#.into()), &udt).unwrap().unwrap();
        assert_eq!(to_cell(Some(v)).unwrap(),Cell::Json(r#"{"city":"Rosario","zip":null}"#.into()));

        let tuple = ColumnType::Tuple(vec![nat(NativeType::Int), nat(NativeType::Text)]);
        let v = cell_to_cql(&Cell::Json(r#"[1,"a"]"#.into()), &tuple).unwrap().unwrap();
        assert_eq!(to_cell(Some(v)).unwrap(),Cell::Json(r#"[1,"a"]"#.into()));
        assert!(cell_to_cql(&Cell::Int(1), &list).is_err());
    }

    #[test]
    fn nothing_is_dropped_on_the_way_in() {
        for zoned in ["12:00:00+01:00", "12:00:00Z", "12:00:00-0300"] {
            let e = cell_to_cql(&Cell::Time(zoned.into()), &nat(NativeType::Time)).unwrap_err();
            assert!(e.to_string().contains("zona horaria"), "{zoned}: {e}");
        }
        assert!(cell_to_cql(&Cell::Time("12:00:00.5".into()), &nat(NativeType::Time)).is_ok());
        let lossy = |c: Cell, t: &ColumnType| matches!(cell_to_cql(&c, t), Err(ConvError::Lossy(_)));
        let udt = ColumnType::UserDefinedType {
            frozen: true,
            definition: Arc::new(scylla::frame::response::result::UserDefinedType {
                name: Cow::Borrowed("address"),
                keyspace: Cow::Borrowed("ks"),
                field_types: vec![(Cow::Borrowed("city"), nat(NativeType::Text))],
            }),
        };
        // A field the target type doesn't have.
        assert!(lossy(Cell::Json(r#"{"city":"Rosario","zip":2000}"#.into()), &udt));
        // More elements than the tuple.
        assert!(lossy(Cell::Json(r#"[1,"lost"]"#.into()), &ColumnType::Tuple(vec![nat(NativeType::Int)])));
        // A repeated key, in the JSON or once converted.
        let map = |k| ColumnType::Collection {
            frozen: false,
            typ: CollectionType::Map(Box::new(nat(k)), Box::new(nat(NativeType::Int))),
        };
        assert!(lossy(Cell::Json(r#"{"a":1,"a":2}"#.into()), &map(NativeType::Text)));
        assert!(lossy(Cell::Json(r#"{"a":1,"b":{"x":1,"x":2}}"#.into()), &map(NativeType::Text)));
        assert!(lossy(Cell::Json(r#"{"1":1,"01":2}"#.into()), &map(NativeType::Int)));
        assert!(lossy(Cell::Json("[[1,1],[1,2]]".into()), &map(NativeType::Int)));
        assert!(repeated_key(r#"{"a,\"b":1,"b":2,"a":{"b":3}}"#).is_none());
        assert!(repeated_key(r#"{"a":"\"a\"","b":[{"a":1},{"a":2}],"c":{"a":1}}"#).is_none());
        // A date-time's time into a date.
        assert!(lossy(Cell::Text("2024-01-01 12:34:56".into()), &nat(NativeType::Date)));
        assert!(lossy(Cell::DateTimeTz("2024-01-01T00:00:00+03:00".into()), &nat(NativeType::Date)));
        // A date-time's date into a time, midnight or not.
        for dt in ["2024-01-01 12:34:56", "2024-01-01T00:00:00Z", "1970-01-01 00:00:00", "+10000-01-01T01:02:03"] {
            assert!(lossy(Cell::DateTime(dt.into()), &nat(NativeType::Time)), "{dt}");
            assert!(lossy(Cell::Text(dt.into()), &nat(NativeType::Time)), "{dt}");
        }
        assert_eq!(round(Cell::Text(" 12:34:56.5 ".into()), NativeType::Time), Cell::Time("12:34:56.500000000".into()));
        // Integers a double or float can't hold, and numbers that aren't flags.
        assert!(lossy(Cell::Int(9007199254740993), &nat(NativeType::Double)));
        assert!(lossy(Cell::Int(16777217), &nat(NativeType::Float)));
        assert_eq!(round(Cell::Int(9007199254740992), NativeType::Double), Cell::Float(9007199254740992.0));
        assert!(cell_to_cql(&Cell::Int(2), &nat(NativeType::Boolean)).is_err());
        assert_eq!(round(Cell::Int(1), NativeType::Boolean), Cell::Bool(true));
    }
}
