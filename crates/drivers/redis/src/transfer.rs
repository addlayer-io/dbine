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
//! Vectors: a hash field that a search index's `VECTOR` field covers is
//! read as the canonical vector text (its blob decoded by the index's
//! `TYPE`), and a vector set as its elements with their vector and
//! attributes. A load writes a vector (text holding a JSON array of
//! numbers) as the blob of the type of the index that covers the target,
//! or, into a column the source says is a vector (`vector(3)`,
//! `vector<float, 3>`…), of that type (`FLOAT32` mostly).
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

use crate::vectors::{self, FtField, FtIndex, FtVector, VType};
use crate::{err, shape, RedisSession};
use dbine_driver::keys::glob_escape;
use dbine_driver::transfer::{
    self, BatchBuilder, BatchSinkRef, BatchSource, Cell, LoadSpec, Progress, ReadSpec,
    TransferColumn,
};
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

/// A hash field's value as a cell: a vector field of a search index that
/// covers the hash (`vf`, by field name) as its text, the rest as [`cell`].
fn vector_cell(vf: &[(String, FtVector)], field: &str, v: Value) -> Cell {
    if let (Some((_, f)), Value::BulkString(b)) = (vf.iter().find(|(n, _)| n == field), &v) {
        if let Some(t) = f.ty.decode(b, Some(f.dims)) {
            return Cell::Text(t);
        }
    }
    cell(v)
}

/// A vector set's elements (`VRANGE` pages, where the server has it; else
/// all of them with `VRANDMEMBER`), each with its vector and attributes.
async fn read_vectorset(
    s: &mut RedisSession,
    key: &[u8],
    wanted: Option<&[String]>,
    sink: BatchSinkRef,
) -> Result<u64> {
    let (dims, _) = s.vset_shape(key).await;
    let vector = dims
        .map(|d| vectors::spell(d as u32, VType::F32))
        .unwrap_or_else(|| "VECTOR".into());
    let mut out = Out::begin(
        sink,
        &[
            ("element", "string"),
            ("vector", vector.as_str()),
            ("attributes", "json"),
        ],
        wanted,
    )?;
    let to_rows = |rows: Vec<(Vec<u8>, Option<String>, Option<String>)>| {
        rows.into_iter()
            .map(|(e, v, a)| {
                vec![
                    text_or_bytes(e),
                    v.map_or(Cell::Null, Cell::Text),
                    a.map_or(Cell::Null, Cell::Json),
                ]
            })
            .collect::<Vec<_>>()
    };
    let elements = |v: Value| -> Vec<Vec<u8>> {
        items(v)
            .into_iter()
            .filter_map(|e| match e {
                Value::BulkString(b) => Some(b),
                _ => None,
            })
            .collect()
    };
    if s.has_vrange().await {
        let count = PAGE.to_string();
        let mut start = b"-".to_vec();
        loop {
            let page = elements(
                s.run(&[b"VRANGE", key, &start, b"+", count.as_bytes()])
                    .await?,
            );
            let n = page.len();
            let Some(last) = page.last() else { break };
            start = [b"(".as_slice(), last].concat();
            let rows = s.vset_rows(key, &page).await;
            out.rows(to_rows(rows))?;
            if n < PAGE {
                break;
            }
        }
    } else {
        let card = s
            .int(&[b"VCARD", key])
            .await
            .unwrap_or(0)
            .max(0)
            .to_string();
        let all = elements(s.run(&[b"VRANDMEMBER", key, card.as_bytes()]).await?);
        for page in all.chunks(PAGE) {
            let rows = s.vset_rows(key, page).await;
            out.rows(to_rows(rows))?;
        }
    }
    out.finish()
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
    fn begin(
        sink: BatchSinkRef,
        natural: &[(&str, &str)],
        wanted: Option<&[String]>,
    ) -> Result<Out> {
        let pick = match wanted {
            None => None,
            Some(w) => Some(
                w.iter()
                    .map(|c| {
                        natural
                            .iter()
                            .position(|(n, _)| n == c)
                            .ok_or_else(|| Error::Query(format!("la key no tiene la columna {c}")))
                    })
                    .collect::<Result<Vec<_>>>()?,
            ),
        };
        let cols: Vec<TransferColumn> = match &pick {
            None => natural.iter().map(|(n, t)| column(n, t)).collect(),
            Some(p) => p
                .iter()
                .map(|&i| column(natural[i].0, natural[i].1))
                .collect(),
        };
        sink.lock()
            .map_err(|_| Error::State("destino de lotes".into()))?
            .begin(&cols)?;
        Ok(Out {
            sink,
            builder: BatchBuilder::new(),
            pick,
        })
    }

    fn rows(&mut self, rows: impl IntoIterator<Item = Vec<Cell>>) -> Result<()> {
        let mut sink = self
            .sink
            .lock()
            .map_err(|_| Error::State("destino de lotes".into()))?;
        for row in rows {
            let row = match &self.pick {
                None => row,
                Some(p) => p
                    .iter()
                    .map(|&i| row.get(i).cloned().unwrap_or(Cell::Null))
                    .collect(),
            };
            self.builder.push(row, &mut *sink)?;
        }
        Ok(())
    }

    fn finish(mut self) -> Result<u64> {
        let mut sink = self
            .sink
            .lock()
            .map_err(|_| Error::State("destino de lotes".into()))?;
        self.builder.flush(&mut *sink)?;
        Ok(self.builder.rows)
    }
}

fn column(name: &str, type_name: &str) -> TransferColumn {
    TransferColumn {
        name: name.into(),
        type_name: type_name.into(),
        nullable: true,
    }
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
        return Err(Error::Unsupported(
            "Redis lee claves enteras: no filtra la lectura por lotes".into(),
        ));
    }
    let name = spec.table.name.clone();
    let key = name.as_bytes();
    let wanted = spec.columns.as_deref();
    // A search index (the object a vector collection is): its documents.
    if spec.table.kind == dbine_driver::kinds::INDEX {
        return read_index(s, &name, wanted, sink).await;
    }
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
            let vf = if t == "hash" {
                s.hash_vectors(key).await
            } else {
                Vec::new()
            };
            let (cmd, natural): (&[u8], &[(&str, &str)]) = match t.as_str() {
                "hash" => (b"HSCAN", &[("field", "string"), ("value", "string")]),
                "set" => (b"SSCAN", &[("value", "string")]),
                _ => (b"ZSCAN", &[("member", "string"), ("score", "double")]),
            };
            let mut out = Out::begin(sink, natural, wanted)?;
            let mut cursor = String::from("0");
            loop {
                let (next, page) = scan_page(
                    s,
                    &[cmd, key, cursor.as_bytes(), b"COUNT", count.as_bytes()],
                )
                .await?;
                if t == "set" {
                    out.rows(page.into_iter().map(|v| vec![cell(v)]))?;
                } else {
                    let mut it = page.into_iter();
                    let mut rows = Vec::new();
                    while let (Some(k), Some(v)) = (it.next(), it.next()) {
                        let value = if t == "zset" {
                            score(v)
                        } else {
                            vector_cell(&vf, &shape::text_of(&k), v)
                        };
                        rows.push(vec![cell(k), value]);
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
                let page = items(
                    s.run(&[b"LRANGE", key, from.as_bytes(), to.as_bytes()])
                        .await?,
                );
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
        "vectorset" => read_vectorset(s, key, wanted, sink).await,
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

/// A hash document's value of a search index field as a cell: a vector
/// field's blob as its text, a `NUMERIC` field as a number, the rest as
/// [`cell`].
fn field_cell(field: &FtField, v: Value) -> Cell {
    if let (Some(vf), Value::BulkString(b)) = (&field.vector, &v) {
        if let Some(t) = vf.ty.decode(b, Some(vf.dims)) {
            return Cell::Text(t);
        }
    }
    if field.kind == "NUMERIC" {
        let t = shape::text_of(&v);
        if let Ok(i) = t.trim().parse::<i64>() {
            return Cell::Int(i);
        }
        if let Ok(f) = t.trim().parse::<f64>() {
            return Cell::Float(f);
        }
    }
    cell(v)
}

/// A JSON document's value (what its path matched) as a cell.
fn json_cell(field: &FtField, j: J) -> Cell {
    if field.vector.is_some() {
        if let Some(v) = dbine_driver::vector::parse_value(&j) {
            return Cell::Text(dbine_driver::vector::text(v));
        }
    }
    match j {
        J::Null => Cell::Null,
        J::Bool(b) => Cell::Bool(b),
        J::Number(n) => match n.as_i64() {
            Some(i) => Cell::Int(i),
            None => n.as_f64().map_or(Cell::Text(n.to_string()), Cell::Float),
        },
        J::String(t) => Cell::Text(t),
        other => Cell::Json(other.to_string()),
    }
}

/// The values of `paths` in a reply of `JSON.GET key <paths…>`: with one
/// path, the array of its matches; with more, an object of each path's
/// matches. The first match of each path.
fn json_get_values(reply: &Value, paths: &[&str]) -> Option<Vec<Option<J>>> {
    let text = match reply {
        Value::BulkString(b) => std::str::from_utf8(b).ok()?.to_string(),
        Value::Nil => return None,
        other => shape::text_of(other),
    };
    let j: J = serde_json::from_str(&text).ok()?;
    let first = |m: Option<&J>| match m {
        Some(J::Array(a)) => a.first().cloned(),
        _ => None,
    };
    Some(if paths.len() == 1 {
        vec![first(Some(&j))]
    } else {
        paths.iter().map(|p| first(j.get(*p))).collect()
    })
}

/// A search index's documents (the keys under its prefixes, of its type),
/// one row each: `id` (the key) and each field of the index, as its
/// describe gives them. A hash's vector field comes as its text, decoded
/// by the field's type; a JSON document's values are read by their paths.
/// The index's `FILTER` isn't applied: every document under its prefixes
/// is read.
async fn read_index(
    s: &mut RedisSession,
    name: &str,
    wanted: Option<&[String]>,
    sink: BatchSinkRef,
) -> Result<u64> {
    let ix = s.ft_index(name).await.ok_or_else(|| {
        Error::Query(format!("No se pudo leer el índice de búsqueda {name}: no existe o el servidor no tiene el módulo de búsqueda."))
    })?;
    let cols = vectors::index_columns(&ix);
    let natural: Vec<(&str, &str)> = cols.iter().map(|(n, t)| (n.as_str(), t.as_str())).collect();
    let mut out = Out::begin(sink, &natural, wanted)?;
    let prefixes: Vec<String> = {
        let p: Vec<String> = ix
            .prefixes
            .iter()
            .filter(|p| !p.is_empty())
            .cloned()
            .collect();
        if p.is_empty() {
            vec![String::new()]
        } else {
            p
        }
    };
    let ids: Vec<&str> = ix.fields.iter().map(|f| f.identifier.as_str()).collect();
    let count = PAGE.to_string();
    for (pi, prefix) in prefixes.iter().enumerate() {
        let pattern = format!("{}*", glob_escape(prefix));
        let mut cursor = String::from("0");
        loop {
            let (next, page) = scan_page(
                s,
                &[
                    b"SCAN",
                    cursor.as_bytes(),
                    b"MATCH",
                    pattern.as_bytes(),
                    b"COUNT",
                    count.as_bytes(),
                ],
            )
            .await?;
            cursor = next;
            // A key under an earlier prefix too was read with it.
            let keys: Vec<Vec<u8>> = page
                .into_iter()
                .map(|k| match k {
                    Value::BulkString(b) => b,
                    other => shape::text_of(&other).into_bytes(),
                })
                .filter(|k| !prefixes[..pi].iter().any(|p| k.starts_with(p.as_bytes())))
                .collect();
            if !keys.is_empty() {
                let mut pipe = redis::pipe();
                pipe.ignore_errors();
                for k in &keys {
                    if ix.on_json {
                        let c = pipe.cmd("JSON.GET").arg(k.as_slice());
                        for id in &ids {
                            c.arg(*id);
                        }
                    } else {
                        let c = pipe.cmd("HMGET").arg(k.as_slice());
                        for id in &ids {
                            c.arg(*id);
                        }
                    }
                }
                let replies: Vec<Value> = pipe.query_async(&mut s.conn).await.map_err(err)?;
                let mut rows = Vec::with_capacity(keys.len());
                for (k, r) in keys.into_iter().zip(replies) {
                    // Keys of another type under the prefix aren't documents.
                    if matches!(r, Value::ServerError(_) | Value::Nil) {
                        continue;
                    }
                    let mut row = vec![text_or_bytes(k)];
                    if ix.on_json {
                        let Some(values) = json_get_values(&r, &ids) else {
                            continue;
                        };
                        for (f, v) in ix.fields.iter().zip(values) {
                            row.push(v.map_or(Cell::Null, |j| json_cell(f, j)));
                        }
                    } else {
                        let values = items(r);
                        if values.iter().all(|v| matches!(v, Value::Nil)) {
                            continue;
                        }
                        for (f, v) in ix.fields.iter().zip(values) {
                            row.push(match v {
                                Value::Nil => Cell::Null,
                                v => field_cell(f, v),
                            });
                        }
                    }
                    rows.push(row);
                }
                out.rows(rows)?;
            }
            if cursor == "0" {
                break;
            }
        }
    }
    out.finish()
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
fn project(
    cols: &[String],
    dup: bool,
    id: Option<&str>,
    fields: Vec<(String, Value)>,
    seen: &mut [bool],
    strict: bool,
) -> Result<Vec<Cell>> {
    project_vectors(cols, dup, id, fields, seen, strict, &[])
}

/// [`project`], with the hashes' vector fields (`vf`) as their text.
fn project_vectors(
    cols: &[String],
    dup: bool,
    id: Option<&str>,
    fields: Vec<(String, Value)>,
    seen: &mut [bool],
    strict: bool,
    vf: &[(String, FtVector)],
) -> Result<Vec<Cell>> {
    let mut h: HashMap<String, Value> = fields.into_iter().collect();
    let mut row = Vec::with_capacity(cols.len());
    for (i, c) in cols.iter().enumerate() {
        let v = match id {
            Some(id) if c == "id" => Some(Cell::Text(id.to_string())),
            // A column asked for twice gets the value twice.
            _ if dup && cols[i + 1..].contains(c) => {
                h.get(c).cloned().map(|v| vector_cell(vf, c, v))
            }
            _ => h.remove(c).map(|v| vector_cell(vf, c, v)),
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
        Some(i) if rows > 0 => Err(Error::Query(format!(
            "la key no tiene la columna {}",
            cols[i]
        ))),
        _ => Ok(()),
    }
}

/// A stream's entries (`id`, fields), a page at a time.
async fn stream_pages(
    s: &mut RedisSession,
    key: &[u8],
    mut f: impl FnMut(Vec<(String, Vec<(String, Value)>)>) -> Result<()>,
) -> Result<()> {
    let count = PAGE.to_string();
    let mut from = String::from("-");
    let mut last: Option<String> = None;
    loop {
        let page = items(
            s.run(&[
                b"XRANGE",
                key,
                from.as_bytes(),
                b"+",
                b"COUNT",
                count.as_bytes(),
            ])
            .await?,
        );
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
async fn read_stream(
    s: &mut RedisSession,
    key: &[u8],
    wanted: Option<&[String]>,
    sink: BatchSinkRef,
) -> Result<u64> {
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
            (
                std::iter::once("id".to_string())
                    .chain(names.list)
                    .collect(),
                true,
            )
        }
    };
    let natural: Vec<(&str, &str)> = cols
        .iter()
        .map(|c| (c.as_str(), if c == "id" { "stream id" } else { "string" }))
        .collect();
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
    args.iter()
        .skip(2)
        .step_by(2)
        .map(Vec::as_slice)
        .find(|v| v.len() > id.len() && ends_key(key, v))
}

/// Whether the hash `key` (`<name>:<suffix>`) is a row of `<name>`.
fn row_of_table(key: &[u8], prefix: usize, fields: &[(String, Value)]) -> bool {
    key.get(prefix..)
        .is_some_and(|suffix| row_id(key, fields) == suffix)
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
        pipe.cmd("MEMORY")
            .arg("USAGE")
            .arg(k.as_slice())
            .arg("SAMPLES")
            .arg(0);
        pipe.cmd("HLEN").arg(k.as_slice());
    }
    let replies: Vec<Value> = pipe.query_async(&mut s.conn).await.map_err(err)?;
    Ok(replies
        .chunks(2)
        .map(|r| match r {
            // Gone meanwhile, or not a hash (left out): nothing to hold.
            [Value::Nil, _] | [_, Value::ServerError(_)] => 0,
            [Value::Int(bytes), Value::Int(n)] => {
                (*bytes).max(0) as u64 + (*n).max(0) as u64 * 2 * ITEM_BYTES
            }
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
async fn hash_pages(
    s: &mut RedisSession,
    name: &str,
    values: bool,
    mut f: impl FnMut(Vec<Vec<(String, Value)>>) -> Result<()>,
) -> Result<()> {
    let prefix = name.len() + 1;
    let pattern = format!("{}:*", glob_escape(name));
    let count = PAGE.to_string();
    let mut cursor = String::from("0");
    loop {
        let (next, keys) = scan_page(
            s,
            &[
                b"SCAN",
                cursor.as_bytes(),
                b"MATCH",
                pattern.as_bytes(),
                b"COUNT",
                count.as_bytes(),
            ],
        )
        .await?;
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
                    pipe.cmd(if w { "HGETALL" } else { "HKEYS" })
                        .arg(k.as_slice());
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
async fn read_hashes(
    s: &mut RedisSession,
    name: &str,
    wanted: Option<&[String]>,
    sink: BatchSinkRef,
) -> Result<u64> {
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
    // The vector fields of a search index over the table's hashes.
    let vf = s.hash_vectors(format!("{name}:").as_bytes()).await;
    let types: Vec<String> = cols
        .iter()
        .map(|c| {
            vf.iter()
                .find(|(n, _)| n == c)
                .map_or_else(|| "string".into(), |(_, v)| vectors::spell(v.dims, v.ty))
        })
        .collect();
    let natural: Vec<(&str, &str)> = cols
        .iter()
        .zip(&types)
        .map(|(c, t)| (c.as_str(), t.as_str()))
        .collect();
    let mut out = Out::begin(sink, &natural, None)?;
    let mut seen = vec![false; cols.len()];
    let dup = cols.iter().collect::<HashSet<_>>().len() != cols.len();
    let mut rows = 0u64;
    hash_pages(s, name, true, |page| {
        let mut batch = Vec::with_capacity(page.len());
        for h in page {
            batch.push(project_vectors(
                &cols, dup, None, h, &mut seen, strict, &vf,
            )?);
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
#[cfg(test)]
pub fn hset_args(
    target: &str,
    columns: &[String],
    row: &[Cell],
) -> std::result::Result<Vec<Vec<u8>>, String> {
    hset_args_packed(target, columns, row, &[])
}

/// A vector column's type, and the dimension its index declares.
pub(crate) type Pack = Option<(VType, Option<u32>)>;

/// [`hset_args`], with each column's vector type (`pack`, by position;
/// `None` or past its end: not a vector): a vector cell goes as its blob,
/// once its length is the dimension its index declares.
pub fn hset_args_packed(
    target: &str,
    columns: &[String],
    row: &[Cell],
    pack: &[Pack],
) -> std::result::Result<Vec<Vec<u8>>, String> {
    let id = match row.first() {
        Some(Cell::Null) | None => {
            return Err(format!(
                "no tiene valor en la primera columna ({}), que forma el nombre de la key",
                columns.first().map_or("", String::as_str)
            ))
        }
        Some(c) => arg(c),
    };
    let mut key = Vec::with_capacity(target.len() + 1 + id.len());
    key.extend_from_slice(target.as_bytes());
    key.push(b':');
    key.extend_from_slice(&id);
    let mut out = vec![key];
    for (i, (c, v)) in columns.iter().zip(row).enumerate() {
        if !matches!(v, Cell::Null) {
            out.push(c.as_bytes().to_vec());
            let blob = match pack.get(i).copied().flatten() {
                Some((t, dims)) => packed_dims(v, t, dims, c)?,
                None => None,
            };
            out.push(blob.unwrap_or_else(|| arg(v)));
        }
    }
    Ok(out)
}

/// A vector cell (text or JSON holding a JSON array of numbers) as the blob
/// of `t`, refusing one whose length isn't `dims`; `None` when it isn't a
/// vector.
fn packed_dims(
    c: &Cell,
    t: VType,
    dims: Option<u32>,
    column: &str,
) -> std::result::Result<Option<Vec<u8>>, String> {
    let Some(v) = cell_vector(c) else {
        return Ok(None);
    };
    match dims {
        Some(d) if d as usize != v.len() => Err(format!(
            "tiene en {column} un vector de {} dimensiones y su índice lo declara de {d} (el servidor lo guardaría sin indexarlo)",
            v.len()
        )),
        _ => Ok(Some(t.encode(&v))),
    }
}

/// A cell that holds a vector (text or JSON with a JSON array of numbers).
fn cell_vector(c: &Cell) -> Option<Vec<f64>> {
    match c {
        Cell::Text(s) | Cell::Json(s) => dbine_driver::vector::parse(s),
        _ => None,
    }
}

/// A cell as a JSON value: JSON as the JSON it holds, the rest as
/// [`Cell::to_json`] gives it.
fn cell_json(c: &Cell) -> J {
    match c {
        Cell::Json(s) => serde_json::from_str(s).unwrap_or_else(|_| J::String(s.clone())),
        other => other.to_json(),
    }
}

/// How a load writes each row.
pub(crate) enum Writer {
    /// A table's rows: `HSET <target>:<first cell> col value …`.
    Hashes { target: String, pack: Vec<Pack> },
    /// A vector set's elements: `VADD <key> FP32 <blob> <element> <flag>
    /// [SETATTR <json>]`.
    VectorSet {
        key: String,
        element: usize,
        vector: usize,
        attrs: Attrs,
        /// The set's quantization (`NOQUANT` for a new one: a copy keeps
        /// the values whole).
        flag: String,
        /// The set's dimension (a new one takes its first vector's).
        dims: Option<usize>,
    },
    /// A search index's documents: `HSET` of each document's key, or
    /// `JSON.SET` on a JSON index.
    Index { index: FtIndex, id: usize },
}

/// Why a row can't be written: this row (`Row`), or any row like it
/// (`Never`: the load is refused as unsupported).
enum Bad {
    Row(String),
    Never(String),
}

impl From<String> for Bad {
    fn from(m: String) -> Bad {
        Bad::Row(m)
    }
}

/// Where a vector set element's attributes come from.
pub(crate) enum Attrs {
    /// An `attributes` column (JSON text).
    Column(usize),
    /// The other columns, as a JSON object.
    Others(Vec<usize>),
}

impl Writer {
    /// A row's command and its arguments, or why the row can't be
    /// written (completing "La fila n …"): `Unsupported` when no row like
    /// it can be.
    fn command(
        &mut self,
        columns: &[String],
        row: &[Cell],
    ) -> Result<(&'static str, Vec<Vec<u8>>)> {
        self.command_of(columns, row).map_err(|e| match e {
            Bad::Row(m) => Error::Query(m),
            Bad::Never(m) => Error::Unsupported(m),
        })
    }

    fn command_of(
        &mut self,
        columns: &[String],
        row: &[Cell],
    ) -> std::result::Result<(&'static str, Vec<Vec<u8>>), Bad> {
        match self {
            Writer::Hashes { target, pack } => {
                let args = hset_args_packed(target, columns, row, pack)?;
                if let Some(v) = shadowed(&args) {
                    let key = &args[0];
                    return Err(Bad::Never(format!(
                        "guarda «{}», que es el final de su key {}: al leerla se tomaría como fila de {} y no de {target}. Poné primero otra columna, que forme keys sin esa ambigüedad",
                        String::from_utf8_lossy(v),
                        String::from_utf8_lossy(key),
                        String::from_utf8_lossy(&key[..key.len() - v.len() - 1])
                    )));
                }
                Ok(("HSET", args))
            }
            Writer::VectorSet {
                key,
                element,
                vector,
                attrs,
                flag,
                dims,
            } => {
                let col = |i: usize| columns.get(i).map_or("", String::as_str);
                let e = match row.get(*element) {
                    None | Some(Cell::Null) => {
                        return Err(
                            format!("no tiene valor en {}, el elemento", col(*element)).into()
                        )
                    }
                    Some(c) => arg(c),
                };
                let v = row.get(*vector).and_then(cell_vector).ok_or_else(|| {
                    format!(
                        "no tiene un vector en {} (como [0.1, 0.2, 0.3])",
                        col(*vector)
                    )
                })?;
                match dims {
                    Some(d) if *d != v.len() => {
                        return Err(format!(
                            "tiene un vector de {} dimensiones y el vector set {key}, de {d}",
                            v.len()
                        )
                        .into())
                    }
                    Some(_) => {}
                    None => *dims = Some(v.len()),
                }
                let mut args = vec![
                    key.as_bytes().to_vec(),
                    b"FP32".to_vec(),
                    VType::F32.encode(&v),
                    e,
                    flag.as_bytes().to_vec(),
                ];
                let json = match attrs {
                    Attrs::Column(i) => match row.get(*i) {
                        None | Some(Cell::Null) => None,
                        Some(c) => Some(match cell_json(c) {
                            J::String(t) => t,
                            j => j.to_string(),
                        }),
                    },
                    Attrs::Others(cols) => {
                        let m: serde_json::Map<String, J> = cols
                            .iter()
                            .filter_map(|&i| {
                                let c = row.get(i).filter(|c| !matches!(c, Cell::Null))?;
                                Some((col(i).to_string(), cell_json(c)))
                            })
                            .collect();
                        (!m.is_empty()).then(|| J::Object(m).to_string())
                    }
                };
                if let Some(j) = json {
                    args.push(b"SETATTR".to_vec());
                    args.push(j.into_bytes());
                }
                Ok(("VADD", args))
            }
            Writer::Index { index, id } => {
                let id_col = columns.get(*id).map_or("", String::as_str);
                let key = match row.get(*id) {
                    None | Some(Cell::Null) => {
                        return Err(format!(
                            "no tiene valor en {id_col}, que forma la key del documento"
                        )
                        .into())
                    }
                    Some(c) => crate::ddl::document_key(index, &String::from_utf8_lossy(&arg(c))),
                };
                // The id column is the key; it's also a field when the
                // index has one of its name, or it isn't called `id`.
                let keep_id = id_col != "id" || index.fields.iter().any(|f| f.is(id_col));
                let cells: Vec<(usize, &String, &Cell)> = columns
                    .iter()
                    .zip(row)
                    .enumerate()
                    .filter(|(i, (_, c))| !matches!(c, Cell::Null) && (*i != *id || keep_id))
                    .map(|(i, (n, c))| (i, n, c))
                    .collect();
                if index.on_json {
                    let values: Vec<(&str, J, Option<&FtField>)> = cells
                        .iter()
                        .map(|(_, n, c)| {
                            (
                                n.as_str(),
                                cell_json(c),
                                index.fields.iter().find(|f| f.is(n)),
                            )
                        })
                        .collect();
                    let doc = crate::ddl::json_document(values.iter().map(|(n, v, f)| (*n, v, *f)))
                        .map_err(|e| e.to_string())?;
                    return Ok((
                        "JSON.SET",
                        vec![
                            key.into_bytes(),
                            b"$".to_vec(),
                            doc.to_string().into_bytes(),
                        ],
                    ));
                }
                let mut args = vec![key.into_bytes()];
                for (_, n, c) in &cells {
                    let field = index.fields.iter().find(|f| f.is(n));
                    args.push(
                        field
                            .map_or(n.as_str(), |f| f.identifier.as_str())
                            .as_bytes()
                            .to_vec(),
                    );
                    match field.and_then(|f| f.vector.as_ref()) {
                        Some(vf) => {
                            args.push(packed_dims(c, vf.ty, Some(vf.dims), n)?.ok_or_else(
                                || format!("no tiene un vector en {n} (como [0.1, 0.2, 0.3])"),
                            )?)
                        }
                        None => args.push(arg(c)),
                    }
                }
                if args.len() == 1 {
                    return Err(Bad::Row(
                        "no tiene ningún campo con valor: Redis no guarda hashes vacíos".into(),
                    ));
                }
                Ok(("HSET", args))
            }
        }
    }
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
async fn fill(
    rows: &mut Rows<'_>,
    writer: &mut Writer,
    columns: &[String],
    max_rows: u64,
    max_bytes: u64,
) -> Result<Window> {
    let mut w = Window {
        pipe: redis::pipe(),
        first: rows.read + 1,
        rows: 0,
        bytes: 0,
    };
    w.pipe.cmd("MULTI");
    while w.rows < max_rows && w.bytes < max_bytes {
        let Some(row) = rows.next().await else { break };
        let n = rows.read;
        let (name, args) = writer.command(columns, &row).map_err(|e| match e {
            Error::Unsupported(m) => Error::Unsupported(format!("La fila {n} {m}.")),
            other => Error::Query(format!("La fila {n} {other}.")),
        })?;
        drop(row);
        let size: u64 = args.iter().map(|a| a.len() as u64).sum();
        if size > ROW_MAX {
            return Err(Error::Query(format!(
                "La fila {n} ocupa {} MiB: supera el máximo que Redis acepta en un comando ({} MiB).",
                size >> 20,
                ROW_MAX >> 20
            )));
        }
        let c = w.pipe.cmd(name);
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
        Some((i, e)) => Err(Error::Query(format!(
            "Redis rechazó la fila {}: {e}. No se escribió ninguna fila de esa tanda.",
            w.first + i as u64 - 1
        ))),
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
    let config = AsyncConnectionConfig::new()
        .set_connection_timeout(Some(Duration::from_secs(15)))
        .set_response_timeout(Some(Duration::from_secs(300)));
    let mut conn = tokio::time::timeout(Duration::from_secs(20), async {
        match &s.pinned {
            Some(p) => p.connect(config).await,
            None => {
                s.client
                    .get_multiplexed_async_connection_with_config(&config)
                    .await
            }
        }
    })
    .await
    .map_err(|_| Error::Connect("tiempo de espera agotado".into()))?
    .map_err(err)?;
    if s.db != s.client.get_connection_info().redis_settings().db() {
        redis::cmd("SELECT")
            .arg(s.db)
            .query_async::<Value>(&mut conn)
            .await
            .map_err(err)?;
    }
    Ok(conn)
}

/// Each target column's vector type: that of the vector field of a search
/// index over the target's hashes, else the source column's when it is a
/// vector (`vector(3)`, `vector<float, 3>`, `VECTOR(3, FLOAT64)`…).
pub(crate) fn pack_types(
    columns: &[String],
    source: &[TransferColumn],
    covered: &[(String, FtVector)],
) -> Vec<Pack> {
    columns
        .iter()
        .enumerate()
        .map(|(i, c)| {
            covered
                .iter()
                .find(|(f, _)| f == c)
                .map(|(_, v)| (v.ty, Some(v.dims)))
                .or_else(|| {
                    source
                        .get(i)
                        .and_then(|t| vectors::parse_spelling(&t.type_name))
                        .map(|(_, t)| (t, None))
                })
        })
        .collect()
}

/// The writer of a load into `spec.table`: a search index's documents
/// (an `index` object), a vector set's elements (a key that is one, or a
/// new key loaded with a vector set's columns, `element` and `vector`, on
/// a server that has them), or else a table's rows as hashes.
async fn writer(
    s: &mut RedisSession,
    spec: &LoadSpec,
    columns: &[TransferColumn],
) -> Result<Writer> {
    let target = spec.table.name.clone();
    let cols = &spec.columns;
    let find = |n: &str| cols.iter().position(|c| c == n);
    if spec.table.kind == dbine_driver::kinds::INDEX {
        let index = s.ft_index(&target).await.ok_or_else(|| {
            Error::Query(format!(
                "No existe el índice de búsqueda {target}: crealo antes de cargarle documentos."
            ))
        })?;
        return Ok(Writer::Index {
            index,
            id: find("id").unwrap_or(0),
        });
    }
    let t = s.key_type(&target).await?;
    let set_shape = find("element").is_some()
        && find("vector").is_some()
        && cols
            .iter()
            .all(|c| matches!(c.as_str(), "element" | "vector" | "attributes"));
    if t == "vectorset" || (t == "none" && set_shape && s.server == vectors::Server::Redis) {
        let (dims, flag) = if t == "vectorset" {
            let (dims, quant) = s.vset_shape(target.as_bytes()).await;
            (
                dims,
                quant
                    .as_deref()
                    .and_then(vectors::quant_flag)
                    .unwrap_or("Q8"),
            )
        } else {
            (None, "NOQUANT")
        };
        // The element: `element`, or the first column; the vector:
        // `vector`, or the first column the source says is one.
        let element = find("element").unwrap_or(0);
        let vector = find("vector")
            .or_else(|| {
                columns
                    .iter()
                    .position(|c| vectors::parse_spelling(&c.type_name).is_some())
                    .filter(|&i| i != element && i < cols.len())
            })
            .or_else(|| (0..cols.len()).find(|&i| i != element && cols[i].to_ascii_lowercase().contains("vector")))
            .ok_or_else(|| {
                Error::Query(format!(
                    "Para cargar el vector set {target} hace falta una columna vector (o una de tipo vectorial)."
                ))
            })?;
        let attrs = match find("attributes") {
            Some(i) => Attrs::Column(i),
            None => Attrs::Others(
                (0..cols.len())
                    .filter(|&i| i != element && i != vector)
                    .collect(),
            ),
        };
        return Ok(Writer::VectorSet {
            key: target,
            element,
            vector,
            attrs,
            flag: flag.to_string(),
            dims,
        });
    }
    let covered = s.hash_vectors(format!("{target}:").as_bytes()).await;
    Ok(Writer::Hashes {
        pack: pack_types(cols, columns, &covered),
        target,
    })
}

pub async fn load(
    s: &mut RedisSession,
    spec: &LoadSpec,
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<u64> {
    if s.read_only {
        return Err(Error::Query(
            "Conexión de solo lectura: no se puede cargar datos.".into(),
        ));
    }
    if spec.columns.is_empty() {
        return Err(Error::Query("No hay columnas para armar las keys.".into()));
    }
    let mut writer = writer(s, spec, columns).await?;
    let (max_rows, max_bytes) = (
        spec.commit_rows.clamp(1, WINDOW_ROWS),
        spec.commit_bytes.clamp(1, WINDOW_BYTES),
    );
    let mut conn = load_connection(s).await?;
    let mut rows = Rows {
        source,
        pending: Vec::new().into_iter(),
        read: 0,
    };
    let mut flight = InFlight {
        exec: None,
        done: 0,
        progress,
        rt: tokio::runtime::Handle::current(),
    };
    loop {
        // The next window fills while the previous one's EXEC runs; both
        // end before anything else happens, so nothing sent is left
        // unaccounted when the load returns.
        let (window, settled) = tokio::join!(
            fill(&mut rows, &mut writer, &spec.columns, max_rows, max_bytes),
            flight.settle()
        );
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
        let Some(h) = self.exec.as_mut() else {
            return Ok(());
        };
        let r = h.await;
        self.exec = None;
        self.count(r)
    }

    fn count(
        &mut self,
        r: std::result::Result<Result<Committed>, tokio::task::JoinError>,
    ) -> Result<()> {
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
        let Some(mut h) = self.exec.take() else {
            return;
        };
        let r = if h.is_finished() {
            use std::future::Future;
            match std::pin::Pin::new(&mut h)
                .poll(&mut std::task::Context::from_waker(std::task::Waker::noop()))
            {
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
        let target = ObjectRef {
            kind: kinds::KEY.into(),
            schema: None,
            name: "user".into(),
        };
        let cols = vec![
            "id".to_string(),
            "name".into(),
            "age".into(),
            "tags".into(),
            "price".into(),
            "at".into(),
        ];
        let rows = [
            vec![
                Cell::Int(1),
                Cell::Text("Ana \"A\" ñ".into()),
                Cell::Float(30.5),
                Cell::Json("[\"x\",\"y\"]".into()),
                Cell::Decimal("12.3400".into()),
                Cell::DateTime("2024-01-02 03:04:05".into()),
            ],
            vec![
                Cell::Text("b 2".into()),
                Cell::Null,
                Cell::Bool(true),
                Cell::Null,
                Cell::UInt(u64::MAX),
                Cell::Null,
            ],
        ];
        let json: Vec<Vec<J>> = rows
            .iter()
            .map(|r| r.iter().map(Cell::to_json).collect())
            .collect();
        let script = crate::ddl::insert_script(&target, &cols, &json, &Default::default()).unwrap();
        let expected = crate::command::parse_script(&script).unwrap();
        let got: Vec<_> = rows
            .iter()
            .map(|r| line(hset_args("user", &cols, r).unwrap()))
            .collect();
        assert_eq!(got, expected);
        assert!(hset_args("user", &cols, &[Cell::Null, Cell::Int(1)]).is_err());
    }

    #[test]
    fn binaries_go_whole() {
        let cols = vec!["id".to_string(), "blob".into()];
        let args = hset_args(
            "t",
            &cols,
            &[Cell::Bytes(vec![0xff, 0]), Cell::Bytes(vec![1, 2, 0x80])],
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                b"t:\xff\x00".to_vec(),
                b"id".to_vec(),
                vec![0xff, 0],
                b"blob".to_vec(),
                vec![1, 2, 0x80]
            ]
        );
    }

    #[test]
    fn replies_become_typed_cells() {
        assert_eq!(
            cell(Value::BulkString(b"hola".to_vec())),
            Cell::Text("hola".into())
        );
        assert_eq!(
            cell(Value::BulkString(vec![0xff, 0xfe])),
            Cell::Bytes(vec![0xff, 0xfe])
        );
        assert_eq!(cell(Value::Int(7)), Cell::Int(7));
        assert_eq!(cell(Value::Nil), Cell::Null);
        assert_eq!(score(Value::BulkString(b"1.5".to_vec())), Cell::Float(1.5));
        assert_eq!(
            score(Value::BulkString(b"inf".to_vec())),
            Cell::Float(f64::INFINITY)
        );
        assert_eq!(
            cell(Value::Array(vec![Value::Int(1)])),
            Cell::Json("[1]".into())
        );
    }

    #[test]
    fn json_goes_byte_for_byte() {
        let doc = r#"{"b":1,"a":123456789012345678901234567890,"c":1.10}"#;
        let args = hset_args(
            "t",
            &["id".into(), "doc".into()],
            &[Cell::Json("[1.0]".into()), Cell::Json(doc.into())],
        )
        .unwrap();
        assert_eq!(
            args,
            vec![
                b"t:[1.0]".to_vec(),
                b"id".to_vec(),
                b"[1.0]".to_vec(),
                b"doc".to_vec(),
                doc.as_bytes().to_vec()
            ]
        );
    }

    #[test]
    fn nested_tables_are_not_rows() {
        let f = |pairs: &[(&str, &str)]| {
            pairs
                .iter()
                .map(|(k, v)| (k.to_string(), Value::BulkString(v.as_bytes().to_vec())))
                .collect::<Vec<_>>()
        };
        // `T:1` is a row of `T`; `T:arch:1` (id 1) is a row of `T:arch`; `T:a:b` with id `a:b` is a row of `T`.
        assert!(row_of_table(b"T:1", 2, &f(&[("id", "1")])));
        assert!(!row_of_table(
            b"T:arch:1",
            2,
            &f(&[("id", "1"), ("x", "arch")])
        ));
        assert!(row_of_table(
            b"T:arch:1",
            7,
            &f(&[("id", "1"), ("x", "arch")])
        ));
        assert!(row_of_table(b"T:a:b", 2, &f(&[("id", "a:b")])));
        // The other way: `T:arch:5` holding `arch:5` is a row of `T`, not of `T:arch`.
        assert!(row_of_table(
            b"T:arch:5",
            2,
            &f(&[("id", "arch:5"), ("v", "x")])
        ));
        assert!(!row_of_table(
            b"T:arch:5",
            7,
            &f(&[("id", "arch:5"), ("v", "x")])
        ));
        // Nothing held (written by hand): the last part is the id.
        assert!(row_of_table(b"T:arch:5", 7, &f(&[("v", "x")])));
        assert!(!row_of_table(b"T:arch:5", 2, &f(&[("v", "x")])));
        // Several held: the longest decides, in any field order (Dragonfly
        // returns a large hash's fields in another order than written).
        for fields in [
            &[("id", "arch:5"), ("n", "5"), ("big", "x")],
            &[("big", "x"), ("n", "5"), ("id", "arch:5")],
        ] {
            assert!(row_of_table(b"T:arch:5", 2, &f(fields)));
            assert!(!row_of_table(b"T:arch:5", 7, &f(fields)));
        }
        // So a load into `T:arch` of a row that also holds `arch:5` is refused.
        let cols = ["id".to_string(), "note".into()];
        assert!(shadowed(
            &hset_args(
                "T:arch",
                &cols,
                &[Cell::Int(5), Cell::Text("arch:5".into())]
            )
            .unwrap()
        )
        .is_some());
        assert!(shadowed(
            &hset_args("T", &cols, &[Cell::Text("arch:5".into()), Cell::Int(5)]).unwrap()
        )
        .is_none());
        assert!(shadowed(
            &hset_args("T:arch", &cols, &[Cell::Int(5), Cell::Text("5".into())]).unwrap()
        )
        .is_none());
        // A value that only ends the key without a `:` before it isn't an id.
        assert!(!row_of_table(b"T:arch:15", 2, &f(&[("x", "rch:15")])));
    }

    #[test]
    fn hashes_are_fetched_within_a_byte_budget() {
        assert_eq!(cut(&[1, 1, 1], 10), vec![0..3]);
        assert_eq!(cut(&[6, 6, 6], 10), vec![0..1, 1..2, 2..3]);
        // One larger than the budget goes alone.
        assert_eq!(cut(&[2, 50, 2, 2], 10), vec![0..1, 1..2, 2..4]);
        assert_eq!(
            cut(&[4, 4, 4, u64::MAX, 1], 10),
            vec![0..2, 2..3, 3..4, 4..5]
        );
        assert!(cut(&[], 10).is_empty());
        // 1000 hashes of 1 MiB: no pipeline holds more than the budget plus one hash.
        let sizes = vec![1 << 20; 1000];
        let ranges = cut(&sizes, HASH_BYTES);
        assert!(ranges
            .iter()
            .all(|r| sizes[r.clone()].iter().sum::<u64>() <= HASH_BYTES + (1 << 20)));
        assert_eq!(ranges.iter().map(|r| r.len()).sum::<usize>(), 1000);
    }

    #[test]
    fn columns_are_projected_strictly() {
        let v = |s: &str| Value::BulkString(s.as_bytes().to_vec());
        let cols = vec!["id".to_string(), "a".into(), "a".into(), "nope".into()];
        let mut seen = vec![false; 4];
        let row = project(
            &cols,
            true,
            None,
            vec![("id".into(), v("1")), ("a".into(), v("x"))],
            &mut seen,
            false,
        )
        .unwrap();
        assert_eq!(
            row,
            vec![
                Cell::Text("1".into()),
                Cell::Text("x".into()),
                Cell::Text("x".into()),
                Cell::Null
            ]
        );
        assert_eq!(seen, [true, true, true, false]);
        assert!(unknown(&cols, &seen, 1).is_err());
        assert!(unknown(&cols, &seen, 0).is_ok());
        // Stream entries: `id` is the entry's; a field unknown to the first pass is an error.
        let cols = vec!["id".to_string(), "a".into()];
        let mut seen = vec![false; 2];
        let row = project(
            &cols,
            false,
            Some("1-0"),
            vec![("a".into(), v("x"))],
            &mut seen,
            true,
        )
        .unwrap();
        assert_eq!(row, vec![Cell::Text("1-0".into()), Cell::Text("x".into())]);
        assert!(project(
            &cols,
            false,
            Some("2-0"),
            vec![("late".into(), v("y"))],
            &mut seen,
            true
        )
        .is_err());
    }

    struct Wide(u64);

    #[dbine_driver::async_trait]
    impl BatchSource for Wide {
        async fn next(&mut self) -> Option<transfer::RowBatch> {
            if self.0 == 0 {
                return None;
            }
            // Batches of 16 rows of 64 KiB: valid (below CHUNK_BYTES).
            let rows: Vec<Vec<Cell>> = (0..16)
                .map(|i| {
                    vec![
                        Cell::Int((self.0 * 100 + i) as i64),
                        Cell::Text("x".repeat(64 * 1024)),
                    ]
                })
                .collect();
            self.0 -= 1;
            Some(transfer::RowBatch { rows, bytes: 0 })
        }
    }

    fn hashes(target: &str) -> Writer {
        Writer::Hashes {
            target: target.into(),
            pack: Vec::new(),
        }
    }

    #[tokio::test]
    async fn windows_are_cut_by_bytes() {
        let cols = vec!["id".to_string(), "blob".into()];
        let mut source = Wide(64);
        let mut rows = Rows {
            source: &mut source,
            pending: Vec::new().into_iter(),
            read: 0,
        };
        let mut total = 0;
        loop {
            let w = fill(
                &mut rows,
                &mut hashes("t"),
                &cols,
                WINDOW_ROWS,
                WINDOW_BYTES,
            )
            .await
            .unwrap();
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
        let mut rows = Rows {
            source: &mut source,
            pending: Vec::new().into_iter(),
            read: 0,
        };
        assert_eq!(
            fill(&mut rows, &mut hashes("t"), &cols, 5, WINDOW_BYTES)
                .await
                .unwrap()
                .rows,
            5
        );
    }

    /// A dropped load whose EXEC outlasts the wait stops it: nothing it
    /// holds can commit later.
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_late_exec_is_aborted() {
        let h = tokio::spawn(async {
            tokio::time::sleep(Duration::from_secs(3600)).await;
            Ok(Committed {
                rows: 1,
                error: None,
            })
        });
        let task = h.abort_handle();
        drop(InFlight {
            exec: Some(h),
            done: 0,
            progress: &|_| {},
            rt: tokio::runtime::Handle::current(),
        });
        tokio::time::sleep(Duration::from_millis(100)).await;
        assert!(
            task.is_finished(),
            "the EXEC task still runs after the drop gave up on it"
        );
    }

    /// Loads into a vector set: `VADD … FP32 <blob> <element> <flag>`,
    /// with its attributes; the first vector sets a new set's dimension.
    #[test]
    fn vector_set_loads() {
        let cols: Vec<String> = ["id", "embedding", "lang", "n"].map(String::from).to_vec();
        let mut w = Writer::VectorSet {
            key: "vs".into(),
            element: 0,
            vector: 1,
            attrs: Attrs::Others(vec![2, 3]),
            flag: "NOQUANT".into(),
            dims: None,
        };
        let (name, args) = w
            .command(
                &cols,
                &[
                    Cell::Int(7),
                    Cell::Text("[0.5, 1, -2]".into()),
                    Cell::Text("es".into()),
                    Cell::Int(3),
                ],
            )
            .unwrap();
        assert_eq!(name, "VADD");
        assert_eq!(args[0], b"vs");
        assert_eq!(args[1], b"FP32");
        assert_eq!(args[2], VType::F32.encode(&[0.5, 1.0, -2.0]));
        assert_eq!(args[3], b"7");
        assert_eq!(args[4], b"NOQUANT");
        assert_eq!(args[5], b"SETATTR");
        assert_eq!(args[6], br#"{"lang":"es","n":3}"#);
        // Another dimension after the first: refused.
        let e = w
            .command(
                &cols,
                &[
                    Cell::Int(8),
                    Cell::Text("[1,2]".into()),
                    Cell::Null,
                    Cell::Null,
                ],
            )
            .unwrap_err()
            .to_string();
        assert!(e.contains("2 dimensiones") && e.contains("de 3"), "{e}");
        // No attributes, no SETATTR; no vector, refused.
        let (_, args) = w
            .command(
                &cols,
                &[
                    Cell::Int(9),
                    Cell::Json("[1,2,3]".into()),
                    Cell::Null,
                    Cell::Null,
                ],
            )
            .unwrap();
        assert_eq!(args.len(), 5);
        assert!(w
            .command(&cols, &[Cell::Int(9), Cell::Null, Cell::Null, Cell::Null])
            .is_err());
        // An `attributes` column (a vector set read) goes as it is.
        let cols: Vec<String> = ["element", "vector", "attributes"]
            .map(String::from)
            .to_vec();
        let mut w = Writer::VectorSet {
            key: "vs".into(),
            element: 0,
            vector: 1,
            attrs: Attrs::Column(2),
            flag: "Q8".into(),
            dims: Some(2),
        };
        let (_, args) = w
            .command(
                &cols,
                &[
                    Cell::Text("a".into()),
                    Cell::Text("[1,2]".into()),
                    Cell::Json(r#"{"x":1}"#.into()),
                ],
            )
            .unwrap();
        assert_eq!(args[4], b"Q8");
        assert_eq!(args[6], br#"{"x":1}"#);
    }

    fn docs_index(json: bool) -> FtIndex {
        let v = FtVector {
            algorithm: "HNSW".into(),
            ty: VType::F16,
            dims: 2,
            metric: "COSINE".into(),
            params: Vec::new(),
        };
        FtIndex {
            name: "docs".into(),
            on_json: json,
            prefixes: vec!["doc:".into()],
            filter: None,
            options: Vec::new(),
            fields: vec![
                FtField {
                    identifier: if json { "$.emb".into() } else { "emb".into() },
                    attribute: "emb".into(),
                    kind: "VECTOR".into(),
                    vector: Some(v),
                    args: Vec::new(),
                },
                FtField {
                    identifier: if json { "$.n".into() } else { "n".into() },
                    attribute: "n".into(),
                    kind: "NUMERIC".into(),
                    vector: None,
                    args: Vec::new(),
                },
            ],
        }
    }

    /// Loads into a search index: each row a document under its prefix,
    /// the `id` column its key.
    #[test]
    fn index_document_loads() {
        let cols: Vec<String> = ["id", "emb", "n", "title"].map(String::from).to_vec();
        let row = [
            Cell::Int(1),
            Cell::Text("[0.5, 1]".into()),
            Cell::Text("4".into()),
            Cell::Text("hola".into()),
        ];
        let mut w = Writer::Index {
            index: docs_index(false),
            id: 0,
        };
        let (name, args) = w.command(&cols, &row).unwrap();
        assert_eq!(name, "HSET");
        assert_eq!(args[0], b"doc:1");
        assert_eq!(args[1], b"emb");
        assert_eq!(args[2], VType::F16.encode(&[0.5, 1.0]));
        assert_eq!(
            &args[3..],
            [
                b"n".to_vec(),
                b"4".to_vec(),
                b"title".to_vec(),
                b"hola".to_vec()
            ]
        );
        let bad = [
            Cell::Int(2),
            Cell::Text("[1,2,3]".into()),
            Cell::Null,
            Cell::Null,
        ];
        assert!(w
            .command(&cols, &bad)
            .unwrap_err()
            .to_string()
            .contains("3 dimensiones"));
        // JSON: the whole document; NUMERIC text as a number.
        let mut w = Writer::Index {
            index: docs_index(true),
            id: 0,
        };
        let (name, args) = w.command(&cols, &row).unwrap();
        assert_eq!(name, "JSON.SET");
        assert_eq!(args[0], b"doc:1");
        assert_eq!(args[1], b"$");
        assert_eq!(
            String::from_utf8(args[2].clone()).unwrap(),
            r#"{"emb":[0.5,1],"n":4.0,"title":"hola"}"#
        );
    }

    /// What reading an index's documents makes of their values.
    #[test]
    fn index_document_reads() {
        let ix = docs_index(false);
        let blob = VType::F16.encode(&[0.5, 1.0]);
        assert_eq!(
            field_cell(&ix.fields[0], Value::BulkString(blob)),
            Cell::Text("[0.5,1]".into())
        );
        assert_eq!(
            field_cell(&ix.fields[1], Value::BulkString(b"4".to_vec())),
            Cell::Int(4)
        );
        assert_eq!(
            field_cell(&ix.fields[1], Value::BulkString(b"4.5".to_vec())),
            Cell::Float(4.5)
        );
        let one = Value::BulkString(b"[[0.5,1]]".to_vec());
        let got = json_get_values(&one, &["$.emb"]).unwrap();
        assert_eq!(
            json_cell(&ix.fields[0], got[0].clone().unwrap()),
            Cell::Text("[0.5,1]".into())
        );
        let two = Value::BulkString(br#"{"$.emb":[[0.5,1]],"$.n":[3]}"#.to_vec());
        let got = json_get_values(&two, &["$.emb", "$.n"]).unwrap();
        assert_eq!(
            json_cell(&ix.fields[1], got[1].clone().unwrap()),
            Cell::Int(3)
        );
        let missing = Value::BulkString(br#"{"$.emb":[],"$.n":[3]}"#.to_vec());
        assert_eq!(
            json_get_values(&missing, &["$.emb", "$.n"]).unwrap()[0],
            None
        );
    }

    /// A vector cell goes as the blob of the index's type, or of the
    /// source's vector type; other cells as before.
    #[test]
    fn vectors_load_as_blobs() {
        let cols: Vec<String> = ["id", "emb", "v64", "note"].map(String::from).to_vec();
        let source = |t: &str| TransferColumn {
            name: "x".into(),
            type_name: t.into(),
            nullable: true,
        };
        let covered = vec![(
            "emb".to_string(),
            FtVector {
                algorithm: "HNSW".into(),
                ty: VType::Bf16,
                dims: 2,
                metric: "COSINE".into(),
                params: Vec::new(),
            },
        )];
        let pack = pack_types(
            &cols,
            &[
                source("int4"),
                source("vector(2)"),
                source("VECTOR(2, FLOAT64)"),
                source("text"),
            ],
            &covered,
        );
        assert_eq!(
            pack,
            [
                None,
                Some((VType::Bf16, Some(2))),
                Some((VType::F64, None)),
                None
            ]
        );
        let args = hset_args_packed(
            "docs",
            &cols,
            &[
                Cell::Int(1),
                Cell::Text("[1, 0.5]".into()),
                Cell::Json("[0.1,2]".into()),
                Cell::Text("[1,2]".into()),
            ],
            &pack,
        )
        .unwrap();
        assert_eq!(args[0], b"docs:1");
        assert_eq!(args[4], VType::Bf16.encode(&[1.0, 0.5]));
        assert_eq!(args[6], VType::F64.encode(&[0.1, 2.0]));
        // Not a vector column: the text as it is.
        assert_eq!(args[8], b"[1,2]");
        // A vector column whose cell isn't one: as it is.
        let args =
            hset_args_packed("d", &cols, &[Cell::Int(2), Cell::Text("x".into())], &pack).unwrap();
        assert_eq!(args[4], b"x");
        // One of another length than the index's dimension: refused.
        let e = hset_args_packed(
            "d",
            &cols,
            &[Cell::Int(3), Cell::Text("[1,2,3]".into())],
            &pack,
        )
        .unwrap_err();
        assert!(e.contains("3 dimensiones") && e.contains("de 2"), "{e}");
        // Read back: the blob of a covered field is its text.
        let blob = VType::Bf16.encode(&[1.0, 0.5]);
        assert_eq!(
            vector_cell(&covered, "emb", Value::BulkString(blob)),
            Cell::Text("[1,0.5]".into())
        );
        assert_eq!(
            vector_cell(&covered, "other", Value::BulkString(b"x".to_vec())),
            Cell::Text("x".into())
        );
    }
}
