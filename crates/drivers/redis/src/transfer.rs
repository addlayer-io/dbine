//! Bulk transfer (see `dbine_driver::transfer`).
//!
//! Reading: a key is read whole, a page at a time with the engine's own
//! paging (`HSCAN`, `SSCAN`, `ZSCAN`, `LRANGE`, `XRANGE`), in the shape
//! browsing gives (`field, value`, `member, score`, `id` + fields…). A name
//! that isn't a key reads the hashes under it (`<name>:<id>`, found with
//! `SCAN` and read with pipelined `HGETALL`, a few hashes at a time within
//! a byte budget sized with `MEMORY USAGE`): what a bulk load or an insert
//! script into `<name>` wrote. A key is a row of the table its id says (see
//! [`row_id`]): `T:a:b` holding `a:b` is a row of `T`, and otherwise one of
//! `T:a`, both ways round, whatever the order the hash's fields come in (a
//! load refuses a row that would read as another table's). Without columns asked for, the columns
//! are every field of every row (and of every stream entry), found in a
//! first pass; asked-for columns that no row has are an error.
//!
//! Loading: each row is `HSET <target>:<first cell> col value …` with null
//! cells left out, as in the insert script. Binaries and JSON go as they
//! are, byte for byte (the insert script, being text, writes binaries as
//! `0x…`). Rows go in windows of at most [`WINDOW_ROWS`] rows or
//! [`WINDOW_BYTES`] bytes, each a `MULTI`/`EXEC` transaction on a
//! connection of the load's own: the commands are queued first and `EXEC`
//! is sent only once they all were, so a load that fails or is dropped
//! leaves nothing queued to run later (closing the connection discards
//! it). The next window is filled while the previous `EXEC` runs. Redis
//! doesn't roll back: when a command of an `EXEC` fails (`WRONGTYPE`), the
//! rest of the window stays written, is counted and reported, and the load
//! fails naming the rows that failed.

use crate::{err, shape, RedisSession};
use dbine_driver::keys::glob_escape;
use dbine_driver::transfer::{self, BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use redis::aio::MultiplexedConnection;
use redis::{AsyncConnectionConfig, Value};
use serde_json::Value as J;
use std::collections::{HashMap, HashSet};
use std::time::Duration;

/// Elements asked per page (`COUNT`, `LRANGE` window).
const PAGE: usize = 1000;
/// Rows per load window (one `MULTI`/`EXEC`), at most: an `EXEC` holds the
/// server while it runs.
const WINDOW_ROWS: u64 = 10_000;
/// Bytes of a window's arguments, at most (plus the row that crosses it):
/// one window is queued while the next is filled, so a load keeps about
/// twice this in memory, far below the server's query buffer limit (1 GiB).
const WINDOW_BYTES: u64 = 8 * 1024 * 1024;
/// Largest row: Redis' `proto-max-bulk-len` default (512 MiB) for one
/// argument, and with it the window stays under the query buffer limit.
const ROW_MAX: u64 = 512 * 1024 * 1024;

fn text_or_bytes(b: Vec<u8>) -> Cell {
    match String::from_utf8(b) {
        Ok(s) => Cell::Text(s),
        Err(e) => Cell::Bytes(e.into_bytes()),
    }
}

/// A reply element as a cell: strings whole (binary as bytes), numbers as
/// numbers, anything nested as JSON.
fn cell(v: Value) -> Cell {
    match v {
        Value::Nil => Cell::Null,
        Value::BulkString(b) => text_or_bytes(b),
        Value::SimpleString(s) => Cell::Text(s),
        Value::VerbatimString { text, .. } => Cell::Text(text),
        Value::Okay => Cell::Text("OK".into()),
        Value::Int(i) => Cell::Int(i),
        Value::Double(f) => Cell::Float(f),
        Value::Boolean(b) => Cell::Bool(b),
        Value::Attribute { data, .. } => cell(*data),
        other => Cell::Json(shape::to_json(&other).to_string()),
    }
}

fn score(v: Value) -> Cell {
    match v {
        Value::Double(f) => Cell::Float(f),
        Value::Int(i) => Cell::Float(i as f64),
        other => {
            let s = shape::text_of(&other);
            s.parse().map_or(Cell::Text(s), Cell::Float)
        }
    }
}

fn items(v: Value) -> Vec<Value> {
    match v {
        Value::Array(a) | Value::Set(a) => a,
        Value::Map(m) => m.into_iter().flat_map(|(k, v)| [k, v]).collect(),
        Value::Attribute { data, .. } => items(*data),
        Value::Nil => Vec::new(),
        other => vec![other],
    }
}

/// The read's columns (optionally only some, in another order) and its
/// batches.
struct Out {
    sink: BatchSinkRef,
    builder: BatchBuilder,
    /// Position in the natural row of each column asked for.
    pick: Option<Vec<usize>>,
}

impl Out {
    fn begin(sink: BatchSinkRef, natural: &[(&str, &str)], wanted: Option<&[String]>) -> Result<Out> {
        let pick = match wanted {
            None => None,
            Some(w) => Some(
                w.iter()
                    .map(|c| {
                        natural.iter().position(|(n, _)| n == c).ok_or_else(|| Error::Query(format!("la key no tiene la columna {c}")))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        let cols: Vec<TransferColumn> = match &pick {
            None => natural.iter().map(|(n, t)| column(n, t)).collect(),
            Some(p) => p.iter().map(|&i| column(natural[i].0, natural[i].1)).collect(),
        };
        sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        Ok(Out { sink, builder: BatchBuilder::new(), pick })
    }

    fn rows(&mut self, rows: impl IntoIterator<Item = Vec<Cell>>) -> Result<()> {
        let mut sink = self.sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
        for row in rows {
            let row = match &self.pick {
                None => row,
                Some(p) => p.iter().map(|&i| row.get(i).cloned().unwrap_or(Cell::Null)).collect(),
            };
            self.builder.push(row, &mut *sink)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<u64> {
        let mut sink = self.sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
        self.builder.flush(&mut *sink)?;
        Ok(self.builder.rows)
    }
}

fn column(name: &str, type_name: &str) -> TransferColumn {
    TransferColumn { name: name.into(), type_name: type_name.into(), nullable: true }
}

/// `HSCAN`/`SSCAN`/`ZSCAN`/`SCAN` page: the next cursor and the elements.
async fn scan_page(s: &mut RedisSession, args: &[&[u8]]) -> Result<(String, Vec<Value>)> {
    let mut parts = items(s.run(args).await?);
    if parts.len() != 2 {
        return Err(Error::Query("respuesta inesperada de SCAN".into()));
    }
    let page = items(parts.pop().expect("two parts"));
    Ok((shape::text_of(&parts[0]), page))
}

pub async fn read(s: &mut RedisSession, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
    if spec.filter.is_some() {
        return Err(Error::Unsupported("Redis lee claves enteras: no filtra la lectura por lotes".into()));
    }
    let name = spec.table.name.clone();
    let key = name.as_bytes();
    let wanted = spec.columns.as_deref();
    let count = PAGE.to_string();
    let t = s.key_type(&name).await?;
    match t.as_str() {
        "string" => {
            let mut out = Out::begin(sink, &[("value", "string")], wanted)?;
            let v = s.run(&[b"GET", key]).await?;
            out.rows([vec![cell(v)]])?;
            out.finish()
        }
        "hash" | "set" | "zset" => {
            let (cmd, natural): (&[u8], &[(&str, &str)]) = match t.as_str() {
                "hash" => (b"HSCAN", &[("field", "string"), ("value", "string")]),
                "set" => (b"SSCAN", &[("value", "string")]),
                _ => (b"ZSCAN", &[("member", "string"), ("score", "double")]),
            };
            let mut out = Out::begin(sink, natural, wanted)?;
            let mut cursor = String::from("0");
            loop {
                let (next, page) = scan_page(s, &[cmd, key, cursor.as_bytes(), b"COUNT", count.as_bytes()]).await?;
                if t == "set" {
                    out.rows(page.into_iter().map(|v| vec![cell(v)]))?;
                } else {
                    let mut it = page.into_iter();
                    let mut rows = Vec::new();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        rows.push(vec![cell(k), if t == "zset" { score(v) } else { cell(v) }]);
                    }
                    out.rows(rows)?;
                }
                cursor = next;
                if cursor == "0" {
                    break;
                }
            }
            out.finish()
        }
        "list" => {
            let mut out = Out::begin(sink, &[("value", "string")], wanted)?;
            let mut start = 0usize;
            loop {
                let (from, to) = (start.to_string(), (start + PAGE - 1).to_string());
                let page = items(s.run(&[b"LRANGE", key, from.as_bytes(), to.as_bytes()]).await?);
                let n = page.len();
                out.rows(page.into_iter().map(|v| vec![cell(v)]))?;
                if n < PAGE {
                    break;
                }
                start += n;
            }
            out.finish()
        }
        "stream" => read_stream(s, key, wanted, sink).await,
        "ReJSON-RL" => {
            let mut out = Out::begin(sink, &[("value", "json")], wanted)?;
            let v = s.run(&[b"JSON.GET", key, b"$"]).await?;
            out.rows([vec![Cell::Json(shape::text_of(&v))]])?;
            out.finish()
        }
        "none" => read_hashes(s, &name, wanted, sink).await,
        // Time series and module types: what browsing gives.
        _ => transfer::read_via_execute(s, spec, sink).await,
    }
}

/// Field names in order of appearance.
#[derive(Default)]
struct Names {
    list: Vec<String>,
    set: HashSet<String>,
}

impl Names {
    fn add(&mut self, name: &str) {
        if !self.set.contains(name) {
            self.set.insert(name.to_string());
            self.list.push(name.to_string());
        }
    }
}

/// The row of one hash or stream entry in the read's columns. `id` is the
/// stream entry's id (streams only). Marks the columns found in `seen`;
/// with `strict` (columns found in a first pass), a field that isn't one
/// of them is an error: it appeared after that pass.
fn project(cols: &[String], dup: bool, id: Option<&str>, fields: Vec<(String, Value)>, seen: &mut [bool], strict: bool) -> Result<Vec<Cell>> {
    let mut h: HashMap<String, Value> = fields.into_iter().collect();
    let mut row = Vec::with_capacity(cols.len());
    for (i, c) in cols.iter().enumerate() {
        let v = match id {
            Some(id) if c == "id" => Some(Cell::Text(id.to_string())),
            // A column asked for twice gets the value twice.
            _ if dup && cols[i + 1..].contains(c) => h.get(c).cloned().map(cell),
            _ => h.remove(c).map(cell),
        };
        seen[i] |= v.is_some();
        row.push(v.unwrap_or(Cell::Null));
    }
    if strict {
        if let Some(extra) = h.keys().find(|k| !cols.contains(k)) {
            return Err(Error::Query(format!("El campo {extra} apareció durante la lectura: los datos cambiaron mientras se copiaban. Volvé a copiar.")));
        }
    }
    Ok(row)
}

/// An asked-for column that no row had.
fn unknown(cols: &[String], seen: &[bool], rows: u64) -> Result<()> {
    match seen.iter().position(|s| !s) {
        Some(i) if rows > 0 => Err(Error::Query(format!("la key no tiene la columna {}", cols[i]))),
        _ => Ok(()),
    }
}

/// A stream's entries (`id`, fields), a page at a time.
async fn stream_pages(s: &mut RedisSession, key: &[u8], mut f: impl FnMut(Vec<(String, Vec<(String, Value)>)>) -> Result<()>) -> Result<()> {
    let count = PAGE.to_string();
    let mut from = String::from("-");
    let mut last: Option<String> = None;
    loop {
        let page = items(s.run(&[b"XRANGE", key, from.as_bytes(), b"+", b"COUNT", count.as_bytes()]).await?);
        let full = page.len() >= PAGE;
        let mut entries = Vec::with_capacity(page.len());
        for e in page {
            let mut parts = items(e).into_iter();
            let id = parts.next().map(|i| shape::text_of(&i)).unwrap_or_default();
            // The page starts at the last id read (inclusive): skip it.
            if last.as_deref() == Some(id.as_str()) {
                continue;
            }
            let mut kv = Vec::new();
            let mut it = items(parts.next().unwrap_or(Value::Nil)).into_iter();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                kv.push((shape::text_of(&k), v));
            }
            last = Some(id.clone());
            entries.push((id, kv));
        }
        f(entries)?;
        match (&last, full) {
            (Some(id), true) => from = id.clone(),
            _ => return Ok(()),
        }
    }
}

/// `XRANGE` pages: `id` plus the fields of every entry, in order of
/// appearance (found in a first pass), or the columns asked for.
async fn read_stream(s: &mut RedisSession, key: &[u8], wanted: Option<&[String]>, sink: BatchSinkRef) -> Result<u64> {
    let (cols, strict) = match wanted {
        Some(w) => (w.to_vec(), false),
        None => {
            let mut names = Names::default();
            stream_pages(s, key, |page| {
                for (_, kv) in &page {
                    for (k, _) in kv {
                        names.add(k);
                    }
                }
                Ok(())
            })
            .await?;
            if names.set.contains("id") {
                return Err(Error::Unsupported(
                    "El stream tiene un campo llamado id, que se confunde con el id de cada entrada: pedí las columnas a leer.".into(),
                ));
            }
            (std::iter::once("id".to_string()).chain(names.list).collect(), true)
        }
    };
    let natural: Vec<(&str, &str)> = cols.iter().map(|c| (c.as_str(), if c == "id" { "stream id" } else { "string" })).collect();
    let mut out = Out::begin(sink, &natural, None)?;
    let mut seen = vec![false; cols.len()];
    let dup = cols.iter().collect::<HashSet<_>>().len() != cols.len();
    let mut rows = 0u64;
    stream_pages(s, key, |page| {
        let mut batch = Vec::with_capacity(page.len());
        for (id, kv) in page {
            batch.push(project(&cols, dup, Some(&id), kv, &mut seen, strict)?);
        }
        rows += batch.len() as u64;
        out.rows(batch)
    })
    .await?;
    unknown(&cols, &seen, rows)?;
    out.finish()
}

/// The id of the hash `key`, which says whose row it is (`<table>:<id>`):
/// the longest field value that the key ends with after a `:` (a load
/// writes the first cell both in the key and among the fields); without
/// one, what follows the key's last `:`. So `T:arch:5` holding `arch:5` is
/// a row of `T`, and holding only `5` (or nothing that matches), a row of
/// `T:arch`. Not the fields' order: it isn't the order they were written in
/// once a hash is large (Dragonfly, and Redis past its listpack limits). A
/// load into `T:arch` of a row that also holds `arch:5` is refused (see
/// [`shadowed`]), so what a load writes reads back as its table's.
fn row_id<'k>(key: &'k [u8], fields: &[(String, Value)]) -> &'k [u8] {
    let held = fields
        .iter()
        .filter_map(|(_, v)| match v {
            Value::BulkString(b) => ends_key(key, b).then_some(b.len()),
            _ => None,
        })
        .max();
    match held {
        Some(n) => &key[key.len() - n..],
        None => key.rsplit(|&c| c == b':').next().unwrap_or_default(),
    }
}

/// Whether `key` ends with `:<v>`.
fn ends_key(key: &[u8], v: &[u8]) -> bool {
    v.len() < key.len() && key.ends_with(v) && key[key.len() - v.len() - 1] == b':'
}

/// A value of the row `args` (`HSET` arguments) longer than its id that the
/// key also ends with: the hash would read as a row of a shorter table
/// (`T:arch:5` holding `arch:5` is a row of `T`, not of `T:arch`).
fn shadowed(args: &[Vec<u8>]) -> Option<&[u8]> {
    let (key, id) = (args.first()?, args.get(2)?);
    args.iter().skip(2).step_by(2).map(Vec::as_slice).find(|v| v.len() > id.len() && ends_key(key, v))
}

/// Whether the hash `key` (`<name>:<suffix>`) is a row of `<name>`.
fn row_of_table(key: &[u8], prefix: usize, fields: &[(String, Value)]) -> bool {
    key.get(prefix..).is_some_and(|suffix| row_id(key, fields) == suffix)
}

/// Estimated bytes of hashes fetched in one pipeline, at most (plus the
/// hash that crosses it): the replies, and the rows made of them, are held
/// until handed on, and a `SCAN` page may hold 1000 large hashes.
const HASH_BYTES: u64 = 4 * 1024 * 1024;
/// Estimated bytes of one field or value once held (`Value`, then `Cell`),
/// besides its data.
const ITEM_BYTES: u64 = 64;

/// Estimated bytes each of `keys` takes once read: `MEMORY USAGE … SAMPLES
/// 0` (exact) plus the items' own overhead (`HLEN`). A key the server
/// doesn't size (`MEMORY` not allowed) counts as a whole pipeline, so it is
/// fetched alone.
async fn hash_sizes(s: &mut RedisSession, keys: &[Vec<u8>]) -> Result<Vec<u64>> {
    let mut pipe = redis::pipe();
    pipe.ignore_errors();
    for k in keys {
        pipe.cmd("MEMORY").arg("USAGE").arg(k.as_slice()).arg("SAMPLES").arg(0);
        pipe.cmd("HLEN").arg(k.as_slice());
    }
    let replies: Vec<Value> = pipe.query_async(&mut s.conn).await.map_err(err)?;
    Ok(replies
        .chunks(2)
        .map(|r| match r {
            // Gone meanwhile, or not a hash (left out): nothing to hold.
            [Value::Nil, _] | [_, Value::ServerError(_)] => 0,
            [Value::Int(bytes), Value::Int(n)] => (*bytes).max(0) as u64 + (*n).max(0) as u64 * 2 * ITEM_BYTES,
            _ => HASH_BYTES,
        })
        .collect())
}

/// Consecutive runs of `sizes` of at most `budget` in all (a size larger
/// than it goes alone): the ranges of each pipeline.
fn cut(sizes: &[u64], budget: u64) -> Vec<std::ops::Range<usize>> {
    let mut out = Vec::new();
    let (mut start, mut sum) = (0, 0u64);
    for (i, &n) in sizes.iter().enumerate() {
        if i > start && sum.saturating_add(n) > budget {
            out.push(start..i);
            (start, sum) = (i, 0);
        }
        sum = sum.saturating_add(n);
    }
    if start < sizes.len() {
        out.push(start..sizes.len());
    }
    out
}

/// The hashes that are rows of `<name>`, a few at a time (at most about
/// [`HASH_BYTES`] held): their fields, with values if `values` (otherwise
/// `Nil`, except for keys with more than one `:`, which need them to tell
/// whose rows they are).
async fn hash_pages(s: &mut RedisSession, name: &str, values: bool, mut f: impl FnMut(Vec<Vec<(String, Value)>>) -> Result<()>) -> Result<()> {
    let prefix = name.len() + 1;
    let pattern = format!("{}:*", glob_escape(name));
    let count = PAGE.to_string();
    let mut cursor = String::from("0");
    loop {
        let (next, keys) = scan_page(s, &[b"SCAN", cursor.as_bytes(), b"MATCH", pattern.as_bytes(), b"COUNT", count.as_bytes()]).await?;
        cursor = next;
        if !keys.is_empty() {
            let keys: Vec<Vec<u8>> = keys
                .into_iter()
                .map(|k| match k {
                    Value::BulkString(b) => b,
                    other => shape::text_of(&other).into_bytes(),
                })
                .collect();
            let sizes = hash_sizes(s, &keys).await?;
            for range in cut(&sizes, HASH_BYTES) {
                let keys = &keys[range];
                let mut pipe = redis::pipe();
                pipe.ignore_errors();
                let mut whole = Vec::with_capacity(keys.len());
                for k in keys {
                    let w = values || k.iter().filter(|&&c| c == b':').count() > 1;
                    pipe.cmd(if w { "HGETALL" } else { "HKEYS" }).arg(k.as_slice());
                    whole.push(w);
                }
                let replies: Vec<Value> = pipe.query_async(&mut s.conn).await.map_err(err)?;
                let mut page = Vec::with_capacity(keys.len());
                for ((k, w), r) in keys.iter().zip(whole).zip(replies) {
                    // Keys of other types under the prefix (WRONGTYPE) are left out.
                    if matches!(r, Value::ServerError(_)) {
                        continue;
                    }
                    let mut it = items(r).into_iter();
                    let mut h = Vec::new();
                    if w {
                        while let (Some(k), Some(v)) = (it.next(), it.next()) {
                            h.push((shape::text_of(&k), v));
                        }
                    } else {
                        h.extend(it.map(|k| (shape::text_of(&k), Value::Nil)));
                    }
                    if !h.is_empty() && row_of_table(k, prefix, &h) {
                        page.push(h);
                    }
                }
                f(page)?;
            }
        }
        if cursor == "0" {
            return Ok(());
        }
    }
}

/// The hashes `<name>:<id>` (what a load into `<name>` writes), one row
/// per hash. Without columns asked for, they are the fields of every hash,
/// in order of appearance (found in a first pass).
async fn read_hashes(s: &mut RedisSession, name: &str, wanted: Option<&[String]>, sink: BatchSinkRef) -> Result<u64> {
    let (cols, strict) = match wanted {
        Some(w) => (w.to_vec(), false),
        None => {
            let mut names = Names::default();
            hash_pages(s, name, false, |page| {
                for h in &page {
                    for (k, _) in h {
                        names.add(k);
                    }
                }
                Ok(())
            })
            .await?;
            (names.list, true)
        }
    };
    let natural: Vec<(&str, &str)> = cols.iter().map(|c| (c.as_str(), "string")).collect();
    let mut out = Out::begin(sink, &natural, None)?;
    let mut seen = vec![false; cols.len()];
    let dup = cols.iter().collect::<HashSet<_>>().len() != cols.len();
    let mut rows = 0u64;
    hash_pages(s, name, true, |page| {
        let mut batch = Vec::with_capacity(page.len());
        for h in page {
            batch.push(project(&cols, dup, None, h, &mut seen, strict)?);
        }
        rows += batch.len() as u64;
        out.rows(batch)
    })
    .await?;
    unknown(&cols, &seen, rows)?;
    out.finish()
}

/// A cell as a Redis argument, as the insert script writes it (text as is,
/// the rest as JSON text), except binaries and JSON, which go whole.
fn arg(c: &Cell) -> Vec<u8> {
    match c {
        Cell::Bytes(b) => b.clone(),
        Cell::Json(s) => s.as_bytes().to_vec(),
        other => match other.to_json() {
            J::String(s) => s.into_bytes(),
            j => j.to_string().into_bytes(),
        },
    }
}

/// A row's `HSET` arguments (after the command name): the key
/// `<target>:<first cell>` and each non-null cell as `column value`.
pub fn hset_args(target: &str, columns: &[String], row: &[Cell]) -> std::result::Result<Vec<Vec<u8>>, String> {
    let id = match row.first() {
        Some(Cell::Null) | None => {
            return Err(format!("no tiene valor en la primera columna ({}), que forma el nombre de la key", columns.first().map_or("", String::as_str)))
        }
        Some(c) => arg(c),
    };
    let mut key = Vec::with_capacity(target.len() + 1 + id.len());
    key.extend_from_slice(target.as_bytes());
    key.push(b':');
    key.extend_from_slice(&id);
    let mut out = vec![key];
    for (c, v) in columns.iter().zip(row) {
        if !matches!(v, Cell::Null) {
            out.push(c.as_bytes().to_vec());
            out.push(arg(v));
        }
    }
    Ok(out)
}

/// One load window: `MULTI` and its rows' `HSET`s.
struct Window {
    pipe: redis::Pipeline,
    /// Number (from 1) of its first row in the load.
    first: u64,
    rows: u64,
    bytes: u64,
}

/// The rows of the current batch not yet in a window, and how many rows
/// were taken so far.
struct Rows<'a> {
    source: &'a mut dyn BatchSource,
    pending: std::vec::IntoIter<Vec<Cell>>,
    read: u64,
}

impl Rows<'_> {
    async fn next(&mut self) -> Option<Vec<Cell>> {
        loop {
            if let Some(r) = self.pending.next() {
                self.read += 1;
                return Some(r);
            }
            self.pending = self.source.next().await?.rows.into_iter();
        }
    }
}

/// The next window: up to `max_rows` rows or `max_bytes` bytes (the row
/// that crosses it included). Empty at the end of the rows.
async fn fill(rows: &mut Rows<'_>, target: &str, columns: &[String], max_rows: u64, max_bytes: u64) -> Result<Window> {
    let mut w = Window { pipe: redis::pipe(), first: rows.read + 1, rows: 0, bytes: 0 };
    w.pipe.cmd("MULTI");
    while w.rows < max_rows && w.bytes < max_bytes {
        let Some(row) = rows.next().await else { break };
        let n = rows.read;
        let args = hset_args(target, columns, &row).map_err(|e| Error::Query(format!("La fila {n} {e}.")))?;
        drop(row);
        if let Some(v) = shadowed(&args) {
            let key = &args[0];
            return Err(Error::Unsupported(format!(
                "La fila {n} guarda «{}», que es el final de su key {}: al leerla se tomaría como fila de {} y no de {target}. Poné primero otra columna, que forme keys sin esa ambigüedad.",
                String::from_utf8_lossy(v),
                String::from_utf8_lossy(key),
                String::from_utf8_lossy(&key[..key.len() - v.len() - 1])
            )));
        }
        let size: u64 = args.iter().map(|a| a.len() as u64).sum();
        if size > ROW_MAX {
            return Err(Error::Query(format!(
                "La fila {n} ocupa {} MiB: supera el máximo que Redis acepta en un comando ({} MiB).",
                size >> 20,
                ROW_MAX >> 20
            )));
        }
        let c = w.pipe.cmd("HSET");
        for a in &args {
            c.arg(a.as_slice());
        }
        w.rows += 1;
        w.bytes += size;
    }
    Ok(w)
}

/// What an `EXEC` wrote: rows committed and, if some failed, the error.
struct Committed {
    rows: u64,
    error: Option<Error>,
}

/// Queues a window (`MULTI` + `HSET`s): nothing is written until `EXEC`.
async fn queue(conn: &mut MultiplexedConnection, mut w: Window) -> Result<()> {
    w.pipe.ignore_errors();
    let replies: Vec<Value> = w.pipe.query_async(conn).await.map_err(err)?;
    // Refused while queueing (OOM, arguments): the connection is dropped
    // with the transaction open, which discards it.
    match replies.iter().enumerate().find_map(|(i, r)| match r {
        Value::ServerError(e) => Some((i, e)),
        _ => None,
    }) {
        Some((0, e)) => Err(Error::Query(format!("Redis rechazó MULTI: {e}"))),
        Some((i, e)) => Err(Error::Query(format!("Redis rechazó la fila {}: {e}. No se escribió ninguna fila de esa tanda.", w.first + i as u64 - 1))),
        None => Ok(()),
    }
}

/// Runs a queued window. Redis doesn't roll back a command that fails
/// inside `EXEC` (`WRONGTYPE`): the others stay written and are counted.
async fn exec(mut conn: MultiplexedConnection, first: u64, rows: u64) -> Result<Committed> {
    let mut p = redis::pipe();
    p.cmd("EXEC").ignore_errors();
    let mut replies: Vec<Value> = p.query_async(&mut conn).await.map_err(err)?;
    match replies.pop() {
        Some(Value::Array(results)) if results.len() as u64 == rows => {
            let failed: Vec<String> = results
                .iter()
                .enumerate()
                .filter_map(|(i, r)| match r {
                    Value::ServerError(e) => Some(format!("fila {}: {e}", first + i as u64)),
                    _ => None,
                })
                .collect();
            let error = (!failed.is_empty()).then(|| {
                let shown: Vec<&str> = failed.iter().take(5).map(String::as_str).collect();
                Error::Query(format!(
                    "Fallaron {} de {rows} filas de una tanda; las demás quedaron escritas y se contaron. {}",
                    failed.len(),
                    shown.join("; ")
                ))
            });
            Ok(Committed { rows: rows - failed.len() as u64, error })
        }
        Some(Value::ServerError(e)) => Err(Error::Query(format!("Redis no ejecutó la tanda que empieza en la fila {first}: {e}. No se escribió ninguna de sus filas."))),
        other => Err(Error::Query(format!("respuesta inesperada de EXEC: {other:?}"))),
    }
}

/// A connection of the load's own: its transactions can't mix with the
/// session's commands, and dropping it (the load failed or was dropped)
/// closes it, discarding a transaction left queued.
async fn load_connection(s: &RedisSession) -> Result<MultiplexedConnection> {
    let config = AsyncConnectionConfig::new().set_connection_timeout(Some(Duration::from_secs(15))).set_response_timeout(Some(Duration::from_secs(300)));
    let mut conn = tokio::time::timeout(Duration::from_secs(20), s.client.get_multiplexed_async_connection_with_config(&config))
        .await
        .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
        .map_err(err)?;
    if s.db != s.client.get_connection_info().redis_settings().db() {
        redis::cmd("SELECT").arg(s.db).query_async::<Value>(&mut conn).await.map_err(err)?;
    }
    Ok(conn)
}

pub async fn load(s: &mut RedisSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    if s.read_only {
        return Err(Error::Query("Conexión de solo lectura: no se puede cargar datos.".into()));
    }
    if spec.columns.is_empty() {
        return Err(Error::Query("No hay columnas para armar las keys.".into()));
    }
    let target = spec.table.name.as_str();
    let (max_rows, max_bytes) = (spec.commit_rows.clamp(1, WINDOW_ROWS), spec.commit_bytes.clamp(1, WINDOW_BYTES));
    let mut conn = load_connection(s).await?;
    let mut rows = Rows { source, pending: Vec::new().into_iter(), read: 0 };
    let mut flight = InFlight { exec: None, done: 0, progress, rt: tokio::runtime::Handle::current() };
    loop {
        // The next window fills while the previous one's EXEC runs; both
        // end before anything else happens, so nothing sent is left
        // unaccounted when the load returns.
        let (window, settled) = tokio::join!(fill(&mut rows, target, &spec.columns, max_rows, max_bytes), flight.settle());
        settled?;
        let window = window?;
        if window.rows == 0 {
            return Ok(flight.done);
        }
        let (first, n) = (window.first, window.rows);
        queue(&mut conn, window).await?;
        flight.exec = Some(tokio::spawn(exec(conn.clone(), first, n)));
    }
}

/// Longest a dropped load waits for its running `EXEC`.
const DROP_WAIT: Duration = Duration::from_secs(if cfg!(test) { 1 } else { 30 });

/// The window whose `EXEC` is running, in a task of its own, and the rows
/// committed so far. A load dropped meanwhile (cancelled) can't take the
/// `EXEC` back, as it was sent; so the drop waits for it: nothing commits
/// after the load is gone, and what it wrote is counted. That wait needs
/// the multi-threaded runtime (the app's); on a single-threaded one the
/// drop counts it only if its reply had arrived.
struct InFlight<'a> {
    exec: Option<tokio::task::JoinHandle<Result<Committed>>>,
    done: u64,
    progress: Progress<'a>,
    rt: tokio::runtime::Handle,
}

impl InFlight<'_> {
    /// Waits for the running `EXEC`, if any, and counts what it wrote.
    async fn settle(&mut self) -> Result<()> {
        let Some(h) = self.exec.as_mut() else { return Ok(()) };
        let r = h.await;
        self.exec = None;
        self.count(r)
    }

    fn count(&mut self, r: std::result::Result<Result<Committed>, tokio::task::JoinError>) -> Result<()> {
        let c = r.map_err(|e| Error::State(format!("la tanda en curso terminó mal: {e}")))??;
        if c.rows > 0 {
            self.done += c.rows;
            (self.progress)(self.done);
        }
        c.error.map_or(Ok(()), Err)
    }
}

impl Drop for InFlight<'_> {
    fn drop(&mut self) {
        let Some(mut h) = self.exec.take() else { return };
        let r = if h.is_finished() {
            use std::future::Future;
            match std::pin::Pin::new(&mut h).poll(&mut std::task::Context::from_waker(std::task::Waker::noop())) {
                std::task::Poll::Ready(r) => Some(r),
                std::task::Poll::Pending => None,
            }
        } else {
            let rt = self.rt.clone();
            let abort = h.abort_handle();
            let wait = move || rt.block_on(tokio::time::timeout(DROP_WAIT, h)).ok();
            let r = match tokio::runtime::Handle::try_current().map(|c| c.runtime_flavor()) {
                Ok(tokio::runtime::RuntimeFlavor::MultiThread) => tokio::task::block_in_place(wait),
                Ok(_) => None,
                Err(_) => wait(),
            };
            // Not waited for, or for too long: the task stops here, so
            // it sends nothing later (an EXEC already sent can't be taken back).
            if r.is_none() {
                abort.abort();
            }
            r
        };
        if let Some(r) = r {
            let _ = self.count(r);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::{kinds, ObjectRef};

    fn line(args: Vec<Vec<u8>>) -> Vec<Vec<u8>> {
        std::iter::once(b"HSET".to_vec()).chain(args).collect()
    }

    #[test]
    fn rows_map_to_the_insert_scripts_commands() {
        let target = ObjectRef { kind: kinds::KEY.into(), schema: None, name: "user".into() };
        let cols = vec!["id".to_string(), "name".into(), "age".into(), "tags".into(), "price".into(), "at".into()];
        let rows = [
            vec![
                Cell::Int(1),
                Cell::Text("Ana \"A\" ñ".into()),
                Cell::Float(30.5),
                Cell::Json("[\"x\",\"y\"]".into()),
                Cell::Decimal("12.3400".into()),
                Cell::DateTime("2024-01-02 03:04:05".into()),
            ],
            vec![Cell::Text("b 2".into()), Cell::Null, Cell::Bool(true), Cell::Null, Cell::UInt(u64::MAX), Cell::Null],
        ];
        let json: Vec<Vec<J>> = rows.iter().map(|r| r.iter().map(Cell::to_json).collect()).collect();
        let script = crate::ddl::insert_script(&target, &cols, &json).unwrap();
        let expected = crate::command::parse_script(&script).unwrap();
        let got: Vec<_> = rows.iter().map(|r| line(hset_args("user", &cols, r).unwrap())).collect();
        assert_eq!(got, expected);
        assert!(hset_args("user", &cols, &[Cell::Null, Cell::Int(1)]).is_err());
    }

    #[test]
    fn binaries_go_whole() {
        let cols = vec!["id".to_string(), "blob".into()];
        let args = hset_args("t", &cols, &[Cell::Bytes(vec![0xff, 0]), Cell::Bytes(vec![1, 2, 0x80])]).unwrap();
        assert_eq!(args, vec![b"t:\xff\x00".to_vec(), b"id".to_vec(), vec![0xff, 0], b"blob".to_vec(), vec![1, 2, 0x80]]);
    }

    #[test]
    fn replies_become_typed_cells() {
        assert_eq!(cell(Value::BulkString(b"hola".to_vec())), Cell::Text("hola".into()));
        assert_eq!(cell(Value::BulkString(vec![0xff, 0xfe])), Cell::Bytes(vec![0xff, 0xfe]));
        assert_eq!(cell(Value::Int(7)), Cell::Int(7));
        assert_eq!(cell(Value::Nil), Cell::Null);
        assert_eq!(score(Value::BulkString(b"1.5".to_vec())), Cell::Float(1.5));
        assert_eq!(score(Value::BulkString(b"inf".to_vec())), Cell::Float(f64::INFINITY));
        assert_eq!(cell(Value::Array(vec![Value::Int(1)])), Cell::Json("[1]".into()));
    }

    #[test]
    fn json_goes_byte_for_byte() {
        let doc = r#"{"b":1,"a":123456789012345678901234567890,"c":1.10}"#;
        let args = hset_args("t", &["id".into(), "doc".into()], &[Cell::Json("[1.0]".into()), Cell::Json(doc.into())]).unwrap();
        assert_eq!(args, vec![b"t:[1.0]".to_vec(), b"id".to_vec(), b"[1.0]".to_vec(), b"doc".to_vec(), doc.as_bytes().to_vec()]);
    }

    #[test]
    fn nested_tables_are_not_rows() {
        let f = |pairs: &[(&str, &str)]| pairs.iter().map(|(k, v)| (k.to_string(), Value::BulkString(v.as_bytes().to_vec()))).collect::<Vec<_>>();
        // `T:1` is a row of `T`; `T:arch:1` (id 1) is a row of `T:arch`; `T:a:b` with id `a:b` is a row of `T`.
        assert!(row_of_table(b"T:1", 2, &f(&[("id", "1")])));
        assert!(!row_of_table(b"T:arch:1", 2, &f(&[("id", "1"), ("x", "arch")])));
        assert!(row_of_table(b"T:arch:1", 7, &f(&[("id", "1"), ("x", "arch")])));
        assert!(row_of_table(b"T:a:b", 2, &f(&[("id", "a:b")])));
        // The other way: `T:arch:5` holding `arch:5` is a row of `T`, not of `T:arch`.
        assert!(row_of_table(b"T:arch:5", 2, &f(&[("id", "arch:5"), ("v", "x")])));
        assert!(!row_of_table(b"T:arch:5", 7, &f(&[("id", "arch:5"), ("v", "x")])));
        // Nothing held (written by hand): the last part is the id.
        assert!(row_of_table(b"T:arch:5", 7, &f(&[("v", "x")])));
        assert!(!row_of_table(b"T:arch:5", 2, &f(&[("v", "x")])));
        // Several held: the longest decides, in any field order (Dragonfly
        // returns a large hash's fields in another order than written).
        for fields in [&[("id", "arch:5"), ("n", "5"), ("big", "x")], &[("big", "x"), ("n", "5"), ("id", "arch:5")]] {
            assert!(row_of_table(b"T:arch:5", 2, &f(fields)));
            assert!(!row_of_table(b"T:arch:5", 7, &f(fields)));
        }
        // So a load into `T:arch` of a row that also holds `arch:5` is refused.
        let cols = ["id".to_string(), "note".into()];
        assert!(shadowed(&hset_args("T:arch", &cols, &[Cell::Int(5), Cell::Text("arch:5".into())]).unwrap()).is_some());
        assert!(shadowed(&hset_args("T", &cols, &[Cell::Text("arch:5".into()), Cell::Int(5)]).unwrap()).is_none());
        assert!(shadowed(&hset_args("T:arch", &cols, &[Cell::Int(5), Cell::Text("5".into())]).unwrap()).is_none());
        // A value that only ends the key without a `:` before it isn't an id.
        assert!(!row_of_table(b"T:arch:15", 2, &f(&[("x", "rch:15")])));
    }

    #[test]
    fn hashes_are_fetched_within_a_byte_budget() {
        assert_eq!(cut(&[1, 1, 1], 10), vec![0..3]);
        assert_eq!(cut(&[6, 6, 6], 10), vec![0..1, 1..2, 2..3]);
        // One larger than the budget goes alone.
        assert_eq!(cut(&[2, 50, 2, 2], 10), vec![0..1, 1..2, 2..4]);
        assert_eq!(cut(&[4, 4, 4, u64::MAX, 1], 10), vec![0..2, 2..3, 3..4, 4..5]);
        assert!(cut(&[], 10).is_empty());
        // 1000 hashes of 1 MiB: no pipeline holds more than the budget plus one hash.
        let sizes = vec![1 << 20; 1000];
        let ranges = cut(&sizes, HASH_BYTES);
        assert!(ranges.iter().all(|r| sizes[r.clone()].iter().sum::<u64>() <= HASH_BYTES + (1 << 20)));
        assert_eq!(ranges.iter().map(|r| r.len()).sum::<usize>(), 1000);
    }

    #[test]
    fn columns_are_projected_strictly() {
        let v = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let cols = vec!["id".to_string(), "a".into(), "a".into(), "nope".into()];
        let mut seen = vec![false; 4];
        let row = project(&cols, true, None, vec![("id".into(), v("1")), ("a".into(), v("x"))], &mut seen, false).unwrap();
        assert_eq!(row, vec![Cell::Text("1".into()), Cell::Text("x".into()), Cell::Text("x".into()), Cell::Null]);
        assert_eq!(seen, [true, true, true, false]);
        assert!(unknown(&cols, &seen, 1).is_err());
        assert!(unknown(&cols, &seen, 0).is_ok());
        // Stream entries: `id` is the entry's; a field unknown to the first pass is an error.
        let cols = vec!["id".to_string(), "a".into()];
        let mut seen = vec![false; 2];
        let row = project(&cols, false, Some("1-0"), vec![("a".into(), v("x"))], &mut seen, true).unwrap();
        assert_eq!(row, vec![Cell::Text("1-0".into()), Cell::Text("x".into())]);
        assert!(project(&cols, false, Some("2-0"), vec![("late".into(), v("y"))], &mut seen, true).is_err());
    }

    struct Wide(u64);

    #[dbine_driver::async_trait]
    impl BatchSource for Wide {
        async fn next(&mut self) -> Option<transfer::RowBatch> {
            if self.0 == 0 {
                return None;
            }
            // Batches of 16 rows of 64 KiB: valid (below CHUNK_BYTES).
            let rows: Vec<Vec<Cell>> = (0..16).map(|i| vec![Cell::Int((self.0 * 100 + i) as i64), Cell::Text("x".repeat(64 * 1024))]).collect();
            self.0 -= 1;
            Some(transfer::RowBatch { rows, bytes: 0 })
        }
    }

    #[tokio::test]
    async fn windows_are_cut_by_bytes() {
        let cols = vec!["id".to_string(), "blob".into()];
        let mut source = Wide(64);
        let mut rows = Rows { source: &mut source, pending: Vec::new().into_iter(), read: 0 };
        let mut total = 0;
        loop {
            let w = fill(&mut rows, "t", &cols, WINDOW_ROWS, WINDOW_BYTES).await.unwrap();
            if w.rows == 0 {
                break;
            }
            assert_eq!(w.first, total + 1);
            assert!(w.bytes < WINDOW_BYTES + 70 * 1024, "{} bytes", w.bytes);
            total += w.rows;
        }
        assert_eq!(total, 64 * 16);
        // Few rows per commit: windows of that many rows.
        let mut source = Wide(2);
        let mut rows = Rows { source: &mut source, pending: Vec::new().into_iter(), read: 0 };
        assert_eq!(fill(&mut rows, "t", &cols, 5, WINDOW_BYTES).await.unwrap().rows, 5);
    }

    /// A dropped load whose EXEC outlasts the wait stops it: nothing it
    /// holds can commit later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_exec_is_aborted() {
        let h = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Committed { rows: 1, error: None })
        });
        let task = h.abort_handle();
        drop(InFlight { exec: Some(h), done: 0, progress: &|_| {}, rt: tokio::runtime::Handle::current() });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(task.is_finished(), "the EXEC task still runs after the drop gave up on it");
    }
}
