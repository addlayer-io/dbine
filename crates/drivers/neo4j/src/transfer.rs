//! Bulk transfer (see `dbine_driver::transfer`) for Neo4j, Memgraph and
//! Neptune. A "table" is a label (or a relationship type, for reading);
//! its columns are the properties.
//!
//! Reading: the property names come from the whole label (`keys()` over
//! every node; asked-for names must be among them), their types from a
//! sample. Rows are `RETURN n.a, n.b…` (nodes without properties too, as
//! rows without cells),
//! streamed over Bolt with the values typed: temporal values in the cells'
//! ISO forms (`date` → date, `localtime` → time, `localdatetime` →
//! date-time, `datetime` with an offset → date-time with zone, a zone id
//! → its UTC instant), byte arrays whole, lists and maps as JSON. Neptune
//! answers over HTTP JSON, so its values are what JSON can say, paged by
//! id. A filter is a Cypher condition on `n`; it may not write.
//!
//! Loading (labels only; relationships need their end nodes):
//! `UNWIND $rows AS r CREATE (n:Label) SET n += r`, one statement per
//! window of at most [`WINDOW`] rows / [`WINDOW_BYTES`] (and `commit_rows` /
//! `commit_bytes`), each in an explicit transaction (BEGIN, RUN, COMMIT): a
//! load dropped mid-window never sends its COMMIT, and closes its
//! connection right then (rolled back, its locks freed); the session opens
//! a new one for its next statement.
//! Values go as Bolt types: dates and times as Cypher temporal values,
//! exact decimals and text as text (Cypher has no decimal type; nothing is
//! coerced to the label's existing types), JSON arrays of integers, of
//! floats that print back the same, of booleans or of strings as lists, and
//! other JSON as text (properties can't hold maps). Nulls are left out (a
//! missing property is Cypher's null). Memgraph has no byte arrays (a Bolt
//! byte array drops the connection): they go as `0x…` text, like the script
//! path; its dates only go from year 0 to 9999. Leap seconds (`:60`) have
//! no Cypher value: refused. Column names must be distinct and non-empty.
//! Neptune takes each HTTP request as its own transaction, so a cancelled
//! load could still commit after it returned: no bulk load there.

use crate::packstream::{tag, Value as Bolt};
use crate::{cypher, infer_columns, value, Conn, Flavor, GraphSession, Transport, LABEL, RELATIONSHIP, USER_AGENT};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result};
use serde_json::{json, Value as J};
use std::collections::HashMap;

/// Rows per statement (one transaction each).
pub(crate) const WINDOW: u64 = 10_000;
/// Estimated bytes per statement. In flight at once: the rows as Bolt
/// values, their encoding (one buffer of the exact size, framed a chunk at
/// a time) and the source's batch, so a window of this size keeps a load
/// well under 32 MiB.
pub(crate) const WINDOW_BYTES: u64 = 4 * 1024 * 1024;
/// Rows per page of a Neptune read.
const NEPTUNE_PAGE: usize = 10_000;

fn pattern(obj: &ObjectRef) -> Result<String> {
    let n = cypher::ident(&obj.name);
    match obj.kind.as_str() {
        LABEL => Ok(format!("(n:{n})")),
        RELATIONSHIP => Ok(format!("()-[n:{n}]->()")),
        other => Err(Error::Unsupported(format!("no se leen filas de un objeto de tipo {other}"))),
    }
}

impl GraphSession {
    /// One Bolt statement with parameters, the raw values to `on_row`.
    async fn bolt_run(&mut self, q: &str, params: Bolt, on_row: &mut (dyn FnMut(&[String], Vec<Bolt>) + Send)) -> Result<()> {
        let extra = self.run_extra(None);
        if self.dirty {
            if let (Transport::Bolt(_), Some(t)) = (&self.transport, &self.target) {
                self.transport = Transport::Bolt(Conn::open(t, USER_AGENT).await?);
            }
            self.dirty = false;
        }
        let Transport::Bolt(c) = &mut self.transport else {
            return Err(Error::State("no es una conexión Bolt".into()));
        };
        self.dirty = true;
        let mut g = CloseIfDropped(Some(c));
        let r = async { g.conn().run(q, params, extra, on_row).await }.await;
        g.0 = None;
        self.dirty = matches!(r, Err(Error::Connect(_)));
        r.map(|_| ())
    }

    /// Property name → its most frequent type in a sample.
    async fn property_types(&mut self, obj: &ObjectRef) -> Result<HashMap<String, String>> {
        let s = self.sample(obj).await?;
        Ok(infer_columns(&s).into_iter().map(|c| (c.name, c.data_type)).collect())
    }

    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let pat = pattern(&spec.table)?;
        let filter = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty());
        if let Some(w) = filter.and_then(cypher::write_reason) {
            return Err(Error::Query(format!("El filtro de la lectura no puede escribir (`{w}`).")));
        }
        let where_ = filter.map(|f| format!(" WHERE {f}")).unwrap_or_default();
        let names: Vec<String> = match &spec.columns {
            Some(c) => {
                // A name the label doesn't have would read as nulls.
                let known = self.property_keys(&pat, "").await?;
                if let Some(bad) = c.iter().find(|n| !known.contains(n)) {
                    return Err(Error::Query(format!("«{}» no tiene la propiedad «{bad}»", spec.table.name)));
                }
                c.clone()
            }
            None => self.property_keys(&pat, &where_).await?,
        };
        let list = names.iter().map(|n| format!("n.{}", cypher::ident(n))).collect::<Vec<_>>().join(", ");
        let types = if self.flavor == Flavor::Neptune {
            self.property_types(&spec.table).await.unwrap_or_default()
        } else if names.is_empty() {
            HashMap::new()
        } else {
            // Bolt types of a sample: the first value that isn't null.
            let mut seen: Vec<Option<&'static str>> = vec![None; names.len()];
            self.bolt_run(&format!("MATCH {pat} WITH n LIMIT 100 RETURN {list}"), Bolt::Map(Vec::new()), &mut |_, row| {
                for (s, v) in seen.iter_mut().zip(&row) {
                    if s.is_none() {
                        *s = bolt_type(v);
                    }
                }
            })
            .await?;
            names.iter().cloned().zip(seen).filter_map(|(n, t)| Some((n, t?.to_string()))).collect()
        };
        let cols: Vec<TransferColumn> = names
            .iter()
            .map(|n| TransferColumn { name: n.clone(), type_name: types.get(n).cloned().unwrap_or_default(), nullable: true })
            .collect();
        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        let mut builder = BatchBuilder::new();

        if self.flavor == Flavor::Neptune {
            let Transport::Http(c) = &self.transport else { return Err(Error::State("Neptune sin cliente HTTP".into())) };
            // Keyset pages by id: SKIP would rescan from the start each page.
            let mut after: Option<J> = None;
            loop {
                let mut conds: Vec<String> = Vec::new();
                if after.is_some() {
                    conds.push("id(n) > $after".into());
                }
                if let Some(f) = filter {
                    conds.push(format!("({f})"));
                }
                let w = if conds.is_empty() { String::new() } else { format!(" WHERE {}", conds.join(" AND ")) };
                let sep = if list.is_empty() { "" } else { ", " };
                let q = format!("MATCH {pat}{w} RETURN id(n) AS dbine_id{sep}{list} ORDER BY id(n) LIMIT {NEPTUNE_PAGE}");
                let reply = c.query(&q, Some(&json!({ "after": after }))).await?;
                let n = reply.rows.len();
                let mut s = sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
                for mut r in reply.rows {
                    if r.is_empty() {
                        continue;
                    }
                    after = Some(r.remove(0));
                    builder.push(r.iter().map(json_cell).collect(), &mut *s)?;
                }
                if n < NEPTUNE_PAGE {
                    break;
                }
            }
        } else {
            // Without properties, a row per node all the same (no cells).
            let ret = if list.is_empty() { "0 AS dbine_row" } else { list.as_str() };
            let q = format!("MATCH {pat}{where_} RETURN {ret}");
            let width = names.len();
            let mut failed: Option<std::io::Error> = None;
            let sink2 = sink.clone();
            let b = &mut builder;
            self.bolt_run(&q, Bolt::Map(Vec::new()), &mut |_, row| {
                if failed.is_some() {
                    return;
                }
                let r = match sink2.lock() {
                    Ok(mut s) => b.push(row.into_iter().take(width).map(bolt_cell).collect(), &mut *s),
                    Err(_) => Err(std::io::Error::other("destino de lotes")),
                };
                if let Err(e) = r {
                    failed = Some(e);
                }
            })
            .await?;
            if let Some(e) = failed {
                return Err(e.into());
            }
        }
        builder.flush(&mut *sink.lock().map_err(|_| Error::State("destino de lotes".into()))?)?;
        Ok(builder.rows)
    }

    pub(crate) async fn transfer_load(&mut self, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
        self.refuse_if_read_only("cargar datos")?;
        if spec.table.kind != LABEL {
            return Err(Error::Unsupported("la carga masiva crea nodos: las relaciones necesitan sus nodos de origen y destino".into()));
        }
        if self.flavor == Flavor::Neptune {
            return Err(Error::Unsupported(
                "Neptune toma cada pedido HTTP como una transacción propia: una carga cancelada podría confirmar filas después de terminar".into(),
            ));
        }
        // A repeated or empty key in a Bolt map drops Memgraph's connection.
        let mut seen = std::collections::HashSet::new();
        if let Some(c) = spec.columns.iter().find(|c| c.is_empty() || !seen.insert(c.as_str())) {
            return Err(Error::Query(if c.is_empty() {
                "la carga tiene una columna sin nombre".into()
            } else {
                format!("la columna «{c}» está repetida en la carga")
            }));
        }
        let q = format!("UNWIND $rows AS r CREATE (n:{}) SET n += r", cypher::ident(&spec.table.name));
        let max_rows = spec.commit_rows.clamp(1, WINDOW) as usize;
        let max_bytes = spec.commit_bytes.clamp(1, WINDOW_BYTES) as usize;
        // Bolt 5 sends date-times as UTC seconds; Bolt 4, as local seconds.
        let utc = match &self.transport {
            Transport::Bolt(c) => c.version.0 >= 5,
            Transport::Http(_) => true,
        };
        let memgraph = self.flavor == Flavor::Memgraph;

        let mut done = 0u64;
        let mut rows: Vec<Bolt> = Vec::new();
        let mut bytes = 0usize;
        while let Some(batch) = source.next().await {
            for row in batch.rows {
                if row.len() != spec.columns.len() {
                    return Err(Error::Query(format!("la fila tiene {} valores y la carga {} columnas", row.len(), spec.columns.len())));
                }
                let mut m = Vec::with_capacity(row.len());
                // Moved, not copied: the batch's text goes into the window.
                for (n, c) in spec.columns.iter().zip(row) {
                    if c == Cell::Null {
                        continue;
                    }
                    let v = cell_bolt_owned(c, utc, memgraph).map_err(|e| match e {
                        Error::Query(e) => Error::Query(format!("propiedad {n}: {e}")),
                        Error::Unsupported(e) => Error::Unsupported(format!("propiedad {n}: {e}")),
                        e => e,
                    })?;
                    bytes += n.len() + bolt_size(&v);
                    m.push((n.clone(), v));
                }
                bytes += ROW_OVERHEAD;
                rows.push(Bolt::Map(m));
                if rows.len() >= max_rows || bytes >= max_bytes {
                    done += rows.len() as u64;
                    self.load_window(&q, std::mem::take(&mut rows)).await?;
                    bytes = 0;
                    progress(done);
                }
            }
        }
        if !rows.is_empty() {
            done += rows.len() as u64;
            self.load_window(&q, rows).await?;
            progress(done);
        }
        Ok(done)
    }

    /// One statement in its own explicit transaction. If this future is
    /// dropped before COMMIT is sent, nothing is committed: the connection
    /// is closed right then (the server rolls the transaction back and
    /// frees its locks, instead of holding them until the session's next
    /// statement), and stays marked dirty, so that statement opens another.
    async fn load_window(&mut self, q: &str, rows: Vec<Bolt>) -> Result<()> {
        let extra = self.run_extra(None);
        if self.dirty {
            if let (Transport::Bolt(_), Some(t)) = (&self.transport, &self.target) {
                self.transport = Transport::Bolt(Conn::open(t, USER_AGENT).await?);
            }
            self.dirty = false;
        }
        let Transport::Bolt(c) = &mut self.transport else {
            return Err(Error::State("no es una conexión Bolt".into()));
        };
        self.dirty = true;
        let params = Bolt::Map(vec![("rows".into(), Bolt::List(rows))]);
        let mut g = CloseIfDropped(Some(c));
        let r = async {
            let c = g.conn();
            c.begin(extra).await?;
            c.run(q, params, Bolt::Map(Vec::new()), &mut |_, _| {}).await?;
            c.commit().await
        }
        .await;
        g.0 = None;
        self.dirty = matches!(r, Err(Error::Connect(_)));
        r.map_err(crate::security::edition_hint)
    }

    /// The properties the label's nodes (or the type's relationships) have,
    /// sorted.
    async fn property_keys(&mut self, pat: &str, where_: &str) -> Result<Vec<String>> {
        let (_, rows) = self.query(&format!("MATCH {pat}{where_} UNWIND keys(n) AS k RETURN DISTINCT k")).await?;
        let mut k: Vec<String> = rows.into_iter().filter_map(|r| r.into_iter().next()).filter_map(|v| v.as_str().map(str::to_string)).collect();
        k.sort();
        Ok(k)
    }
}

/// Closes the connection it holds when dropped still holding it: a
/// statement whose future is dropped leaves no open transaction behind.
struct CloseIfDropped<'a>(Option<&'a mut Conn>);

impl CloseIfDropped<'_> {
    fn conn(&mut self) -> &mut Conn {
        self.0.as_deref_mut().expect("conexión Bolt")
    }
}

impl Drop for CloseIfDropped<'_> {
    fn drop(&mut self) {
        if let Some(c) = self.0.take() {
            c.close_now();
        }
    }
}

/// Bytes a row takes besides its properties (the map, its vector…).
const ROW_OVERHEAD: usize = 64;

/// Estimated bytes of a property value in memory (`Bolt` enum included).
fn bolt_size(v: &Bolt) -> usize {
    48 + match v {
        Bolt::String(s) => s.len(),
        Bolt::Bytes(b) => b.len(),
        Bolt::List(l) => l.iter().map(bolt_size).sum(),
        Bolt::Struct(_, f) => f.iter().map(bolt_size).sum(),
        _ => 0,
    }
}

// ---- Bolt → cell ----

pub(crate) fn bolt_cell(v: Bolt) -> Cell {
    match v {
        Bolt::Null => Cell::Null,
        Bolt::Bool(b) => Cell::Bool(b),
        Bolt::Int(i) => Cell::Int(i),
        Bolt::Float(f) => Cell::Float(f),
        Bolt::Bytes(b) => Cell::Bytes(b),
        Bolt::String(s) => Cell::Text(s),
        Bolt::Struct(t, _) if matches!(t, tag::DATE | tag::LOCAL_TIME | tag::LOCAL_DATE_TIME | tag::DATE_TIME | tag::LEGACY_DATE_TIME | tag::DATE_TIME_ZONE_ID) => {
            let s = match value::to_json(&v) {
                J::String(s) => s,
                _ => return Cell::Null,
            };
            match t {
                tag::DATE => Cell::Date(s),
                tag::LOCAL_TIME => Cell::Time(s),
                tag::LOCAL_DATE_TIME => Cell::DateTime(s.replacen('T', " ", 1)),
                // `…Z[Europe/Madrid]`: the instant, in UTC.
                tag::DATE_TIME_ZONE_ID => {
                    Cell::DateTimeTz(s.split('[').next().unwrap_or(&s).replacen('T', " ", 1).replace('Z', "+00:00"))
                }
                _ => Cell::DateTimeTz(s.replacen('T', " ", 1).replace('Z', "+00:00")),
            }
        }
        // Time with offset, local date-time with a zone id, durations: text.
        Bolt::Struct(tag::TIME | tag::LEGACY_DATE_TIME_ZONE_ID | tag::DURATION, _) => match value::to_json(&v) {
            J::String(s) => Cell::Text(s),
            other => Cell::Json(other.to_string()),
        },
        other => Cell::Json(value::to_json(&other).to_string()),
    }
}

/// A value's type as Cypher names it (`None` for null).
fn bolt_type(v: &Bolt) -> Option<&'static str> {
    Some(match v {
        Bolt::Null => return None,
        Bolt::Bool(_) => "BOOLEAN",
        Bolt::Int(_) => "INTEGER",
        Bolt::Float(_) => "FLOAT",
        Bolt::Bytes(_) => "BYTES",
        Bolt::String(_) => "STRING",
        Bolt::List(_) => "LIST",
        Bolt::Map(_) => "MAP",
        Bolt::Struct(t, _) => match *t {
            tag::DATE => "DATE",
            tag::LOCAL_TIME => "LOCAL TIME",
            tag::TIME => "ZONED TIME",
            tag::LOCAL_DATE_TIME => "LOCAL DATETIME",
            tag::DATE_TIME | tag::LEGACY_DATE_TIME | tag::DATE_TIME_ZONE_ID | tag::LEGACY_DATE_TIME_ZONE_ID => "ZONED DATETIME",
            tag::DURATION => "DURATION",
            tag::POINT_2D | tag::POINT_3D => "POINT",
            tag::NODE => "NODE",
            tag::RELATIONSHIP | tag::UNBOUND_RELATIONSHIP => "RELATIONSHIP",
            tag::PATH => "PATH",
            _ => "ANY",
        },
    })
}

/// A Neptune (JSON) value.
fn json_cell(v: &J) -> Cell {
    Cell::from_json(v)
}

// ---- cell → Bolt ----

const EPOCH: NaiveDate = NaiveDate::from_ymd_opt(1970, 1, 1).expect("epoch");

/// [`cell_bolt`], moving the text and byte arrays instead of copying them.
fn cell_bolt_owned(c: Cell, utc: bool, memgraph: bool) -> Result<Bolt> {
    Ok(match c {
        Cell::Decimal(s) | Cell::Text(s) | Cell::Uuid(s) => Bolt::String(s),
        Cell::Bytes(b) if !memgraph => Bolt::Bytes(b),
        c => cell_bolt(&c, utc, memgraph)?,
    })
}

/// A cell as a property value, without loss: text and exact decimals stay
/// text. `memgraph`: byte arrays as `0x…` text, dates within years 0–9999.
pub(crate) fn cell_bolt(c: &Cell, utc: bool, memgraph: bool) -> Result<Bolt> {
    let bad = || Error::Query(format!("{c:?} no se convierte a un valor de Cypher"));
    // Memgraph's temporal types only go from year 0 to 9999.
    // chrono reads `:60` as a leap second (nanoseconds past 10⁹); Cypher's
    // temporal values have none, and Memgraph drops the connection on it.
    let leap = |nanos: u32| {
        if nanos >= 1_000_000_000 {
            Err(Error::Unsupported(format!("Cypher no guarda segundos intercalares (:60) ({c:?})")))
        } else {
            Ok(())
        }
    };
    let year = |y: i32| {
        if memgraph && !(0..=9999).contains(&y) {
            Err(Error::Unsupported(format!("Memgraph solo guarda fechas entre los años 0 y 9999 ({c:?})")))
        } else {
            Ok(())
        }
    };
    Ok(match c {
        Cell::Null => Bolt::Null,
        Cell::Bool(b) => Bolt::Bool(*b),
        Cell::Int(i) => Bolt::Int(*i),
        Cell::UInt(u) => i64::try_from(*u).map(Bolt::Int).unwrap_or_else(|_| Bolt::String(u.to_string())),
        Cell::Float(f) => Bolt::Float(*f),
        Cell::Bytes(b) if memgraph => Bolt::String(hex(b)),
        Cell::Bytes(b) => Bolt::Bytes(b.clone()),
        Cell::Decimal(s) | Cell::Text(s) | Cell::Uuid(s) => Bolt::String(s.clone()),
        Cell::Date(s) => {
            // The whole text: a signed or longer year is kept, trailing text refused.
            let d = NaiveDate::parse_from_str(s.trim(), "%Y-%m-%d").map_err(|_| bad())?;
            year(d.year())?;
            Bolt::Struct(tag::DATE, vec![Bolt::Int((d - EPOCH).num_days())])
        }
        Cell::Time(s) => {
            let t = NaiveTime::parse_from_str(s.trim(), "%H:%M:%S%.f").map_err(|_| bad())?;
            leap(t.nanosecond())?;
            Bolt::Struct(tag::LOCAL_TIME, vec![Bolt::Int(i64::from(t.num_seconds_from_midnight()) * 1_000_000_000 + i64::from(t.nanosecond()))])
        }
        Cell::DateTime(s) => {
            let t = NaiveDateTime::parse_from_str(&s.trim().replacen('T', " ", 1), "%Y-%m-%d %H:%M:%S%.f").map_err(|_| bad())?;
            year(t.year())?;
            leap(t.nanosecond())?;
            let t = t.and_utc();
            Bolt::Struct(tag::LOCAL_DATE_TIME, vec![Bolt::Int(t.timestamp()), Bolt::Int(i64::from(t.timestamp_subsec_nanos()))])
        }
        Cell::DateTimeTz(s) => {
            let t = parse_tz(s).ok_or_else(bad)?;
            year(t.year())?;
            year(t.naive_utc().year())?;
            leap(t.nanosecond())?;
            let off = i64::from(t.offset().local_minus_utc());
            let nanos = Bolt::Int(i64::from(t.timestamp_subsec_nanos()));
            if utc {
                Bolt::Struct(tag::DATE_TIME, vec![Bolt::Int(t.timestamp()), nanos, Bolt::Int(off)])
            } else {
                Bolt::Struct(tag::LEGACY_DATE_TIME, vec![Bolt::Int(t.timestamp() + off), nanos, Bolt::Int(off)])
            }
        }
        Cell::Json(s) => json_list(s).map(Bolt::List).unwrap_or_else(|| Bolt::String(s.clone())),
    })
}

fn parse_tz(s: &str) -> Option<DateTime<chrono::FixedOffset>> {
    let s = s.trim().replacen('T', " ", 1);
    let s = match s.strip_suffix('Z') {
        Some(r) => format!("{r}+00:00"),
        None => s,
    };
    ["%Y-%m-%d %H:%M:%S%.f%:z", "%Y-%m-%d %H:%M:%S%.f%z", "%Y-%m-%d %H:%M:%S%.f%#z"]
        .iter()
        .find_map(|f| DateTime::parse_from_str(&s, f).ok())
}

/// A JSON array a property can hold as a list without loss: one scalar
/// type (booleans, strings, integers that fit in 64 bits, or floats that
/// print back as written). Anything else stays text.
fn json_list(s: &str) -> Option<Vec<Bolt>> {
    let J::Array(a) = serde_json::from_str::<J>(s).ok()? else { return None };
    let kind = |v: &J| match v {
        J::Bool(_) => 1,
        J::Number(n) if n.is_i64() => 2,
        J::Number(n) if n.is_f64() => 3,
        J::String(_) => 4,
        // Integers past i64, maps, lists, nulls.
        _ => 0,
    };
    let first = a.first().map(kind).unwrap_or(4);
    if first == 0 || !a.iter().all(|v| kind(v) == first) {
        return None;
    }
    if first == 2 || first == 3 {
        // serde_json reads a number too long for i64 as a float, and a
        // float to its nearest f64: the text written must be what reads back.
        let body = s.trim().strip_prefix('[')?.strip_suffix(']')?;
        let written: Vec<&str> = body.split(',').map(str::trim).collect();
        if written.len() != a.len() || a.iter().zip(&written).any(|(v, w)| v.to_string().as_str() != *w) {
            return None;
        }
    }
    Some(
        a.into_iter()
            .map(|v| match v {
                J::Bool(b) => Bolt::Bool(b),
                J::Number(n) => n.as_i64().map(Bolt::Int).unwrap_or_else(|| Bolt::Float(n.as_f64().unwrap_or_default())),
                J::String(s) => Bolt::String(s),
                other => Bolt::String(other.to_string()),
            })
            .collect(),
    )
}

/// `0x…`, whole.
fn hex(b: &[u8]) -> String {
    b.iter().fold(String::with_capacity(2 + b.len() * 2) + "0x", |mut s, x| {
        s.push_str(&format!("{x:02X}"));
        s
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Cell → Bolt → cell.
    fn round(c: Cell) -> Cell {
        bolt_cell(cell_bolt(&c, true, false).unwrap())
    }

    #[test]
    fn scalars_round_trip() {
        for c in [Cell::Bool(true), Cell::Int(-5), Cell::Int(i64::MAX), Cell::Float(0.1), Cell::Text("ñ".into()), Cell::Bytes(vec![0, 255, 7])] {
            assert_eq!(round(c.clone()), c);
        }
        assert_eq!(round(Cell::UInt(7)), Cell::Int(7));
        assert_eq!(round(Cell::UInt(u64::MAX)), Cell::Text(u64::MAX.to_string()));
        // No decimal type: exact text.
        assert_eq!(round(Cell::Decimal("12.34".into())), Cell::Text("12.34".into()));
        assert_eq!(round(Cell::Uuid("5a1c395e-b5d1-4ec5-9e1a-6f4a2f5f3e10".into())), Cell::Text("5a1c395e-b5d1-4ec5-9e1a-6f4a2f5f3e10".into()));
        assert_eq!(cell_bolt(&Cell::Null, true, false).unwrap(), Bolt::Null);
        // Memgraph: byte arrays as `0x…` text.
        assert_eq!(cell_bolt(&Cell::Bytes(vec![0xca, 0xfe]), true, true).unwrap(), Bolt::String("0xCAFE".into()));
    }

    #[test]
    fn text_and_decimals_are_never_coerced() {
        // Whatever type the label's property already has: text stays text.
        for s in ["007", " 12", "NaN", "TRUE", "0.30000000000000000001"] {
            assert_eq!(round(Cell::Text(s.into())), Cell::Text(s.into()));
            assert_eq!(round(Cell::Decimal(s.into())), Cell::Text(s.into()));
        }
    }

    #[test]
    fn temporal_round_trip() {
        for c in [
            Cell::Date("2024-01-31".into()),
            Cell::Date("1901-12-01".into()),
            Cell::Time("13:45:00".into()),
            Cell::Time("13:45:00.5".into()),
            Cell::DateTime("2024-01-31 13:45:00.123".into()),
            Cell::DateTimeTz("2024-01-31 13:45:00-03:00".into()),
            Cell::DateTimeTz("2024-01-31 13:45:00.25+00:00".into()),
        ] {
            assert_eq!(round(c.clone()), c);
        }
        assert_eq!(round(Cell::DateTime("2024-01-31T13:45:00".into())), Cell::DateTime("2024-01-31 13:45:00".into()));
        assert_eq!(round(Cell::DateTimeTz("2024-01-31T13:45:00Z".into())), Cell::DateTimeTz("2024-01-31 13:45:00+00:00".into()));
        // Bolt 4: local seconds.
        let legacy = cell_bolt(&Cell::DateTimeTz("2024-01-01 01:00:00+01:00".into()), false, false).unwrap();
        assert_eq!(legacy, Bolt::Struct(tag::LEGACY_DATE_TIME, vec![Bolt::Int(1_704_070_800), Bolt::Int(0), Bolt::Int(3_600)]));
        assert_eq!(bolt_cell(legacy), Cell::DateTimeTz("2024-01-01 01:00:00+01:00".into()));
        // A zone id: the UTC instant.
        let z = Bolt::Struct(tag::DATE_TIME_ZONE_ID, vec![Bolt::Int(1_704_067_200), Bolt::Int(0), Bolt::String("Europe/Madrid".into())]);
        assert_eq!(bolt_cell(z), Cell::DateTimeTz("2024-01-01 00:00:00+00:00".into()));
        let d = Bolt::Struct(tag::DURATION, vec![Bolt::Int(14), Bolt::Int(3), Bolt::Int(3_725), Bolt::Int(0)]);
        assert_eq!(bolt_cell(d), Cell::Text("P1Y2M3DT1H2M5S".into()));
        assert!(cell_bolt(&Cell::Date("mañana".into()), true, false).is_err());
    }

    #[test]
    fn dates_keep_signed_years_and_refuse_trailing_text() {
        let days = |c: &Cell| match cell_bolt(c, true, false).unwrap() {
            Bolt::Struct(_, f) => f[0].clone(),
            other => panic!("{other:?}"),
        };
        let bc = NaiveDate::from_ymd_opt(-44, 3, 15).unwrap();
        assert_eq!(days(&Cell::Date("-0044-03-15".into())), Bolt::Int((bc - EPOCH).num_days()));
        assert_eq!(days(&Cell::Date("+12024-01-31".into())), Bolt::Int((NaiveDate::from_ymd_opt(12024, 1, 31).unwrap() - EPOCH).num_days()));
        assert!(cell_bolt(&Cell::Date("2024-01-31garbage".into()), true, false).is_err());
        assert!(cell_bolt(&Cell::Date("2024-01-31 10:00:00".into()), true, false).is_err());
        assert_eq!(round(Cell::DateTime("-0044-03-15 10:00:00".into())), Cell::DateTime("-0044-03-15 10:00:00".into()));
        // Memgraph: only years 0 to 9999, refused instead of dropping the connection.
        assert!(matches!(cell_bolt(&Cell::Date("-0044-03-15".into()), true, true), Err(Error::Unsupported(_))));
        assert!(matches!(cell_bolt(&Cell::DateTime("+10000-01-01 00:00:00".into()), true, true), Err(Error::Unsupported(_))));
        assert!(matches!(cell_bolt(&Cell::DateTimeTz("0000-01-01 00:30:00+01:00".into()), true, true), Err(Error::Unsupported(_))));
        assert!(cell_bolt(&Cell::Date("0000-01-01".into()), true, true).is_ok());
        assert!(cell_bolt(&Cell::Date("9999-12-31".into()), true, true).is_ok());
    }

    #[test]
    fn leap_seconds_are_refused() {
        // chrono takes `:60` as a leap second; Memgraph drops the connection on it.
        for c in [
            Cell::Time("23:59:60".into()),
            Cell::Time("23:59:60.5".into()),
            Cell::DateTime("2016-12-31 23:59:60".into()),
            Cell::DateTimeTz("2016-12-31 23:59:60+00:00".into()),
            Cell::DateTimeTz("2016-12-31T20:59:60.25-03:00".into()),
        ] {
            for memgraph in [false, true] {
                assert!(matches!(cell_bolt(&c, true, memgraph), Err(Error::Unsupported(_))), "{c:?}");
                assert!(matches!(cell_bolt_owned(c.clone(), true, memgraph), Err(Error::Unsupported(_))), "{c:?}");
            }
        }
        assert!(cell_bolt(&Cell::Time("23:59:59.999999999".into()), true, true).is_ok());
    }

    #[test]
    fn owned_conversion_matches() {
        for c in [Cell::Text("ñ".into()), Cell::Decimal("1.50".into()), Cell::Bytes(vec![1, 2]), Cell::Date("2024-01-31".into()), Cell::Json("[1]".into())] {
            for memgraph in [false, true] {
                assert_eq!(cell_bolt_owned(c.clone(), true, memgraph).unwrap(), cell_bolt(&c, true, memgraph).unwrap());
            }
        }
    }

    #[test]
    fn json_values() {
        assert_eq!(cell_bolt(&Cell::Json("[1,2]".into()), true, false).unwrap(), Bolt::List(vec![Bolt::Int(1), Bolt::Int(2)]));
        assert_eq!(cell_bolt(&Cell::Json("[ 1.5 , -2.25 ]".into()), true, false).unwrap(), Bolt::List(vec![Bolt::Float(1.5), Bolt::Float(-2.25)]));
        assert_eq!(round(Cell::Json("[\"a\",\"b\"]".into())), Cell::Json("[\"a\",\"b\"]".into()));
        assert_eq!(round(Cell::Json("[true,false]".into())), Cell::Json("[true,false]".into()));
        // Maps and mixed lists aren't property values: text.
        assert_eq!(round(Cell::Json("{\"a\":1}".into())), Cell::Text("{\"a\":1}".into()));
        assert_eq!(round(Cell::Json("[1,\"a\"]".into())), Cell::Text("[1,\"a\"]".into()));
        let m = Bolt::Map(vec![("a".into(), Bolt::Int(1))]);
        assert_eq!(bolt_cell(m), Cell::Json("{\"a\":1}".into()));
    }

    #[test]
    fn json_numbers_that_would_change_stay_text() {
        // Past i64 (serde_json would make them floats), or floats that don't
        // print back as written: the exact text, not a rounded list.
        for s in ["[18446744073709551615]", "[123456789012345678901234567890]", "[1, 18446744073709551615]", "[0.30000000000000000001]", "[1e3]", "[-0]"] {
            assert_eq!(cell_bolt(&Cell::Json(s.into()), true, false).unwrap(), Bolt::String(s.into()), "{s}");
        }
        assert_eq!(cell_bolt(&Cell::Json("[9223372036854775807]".into()), true, false).unwrap(), Bolt::List(vec![Bolt::Int(i64::MAX)]));
    }

    #[test]
    fn window_size_counts_the_values() {
        let small = bolt_size(&Bolt::Int(1));
        let wide = bolt_size(&Bolt::String("x".repeat(1024 * 1024)));
        assert!(wide > 1024 * 1024 && small < 100);
        // A window of 1 MiB rows closes long before 10,000 rows.
        assert!((WINDOW_BYTES as usize) / wide < 10);
        // Rows, their encoding and a batch in flight stay under the 32 MiB rule.
        const _: () = assert!(WINDOW_BYTES <= 4 * 1024 * 1024);
    }
}
