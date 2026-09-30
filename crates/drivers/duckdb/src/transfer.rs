//! Bulk transfer (see `dbine_driver::transfer`) for DuckDB.
//!
//! - **Read** ([`read`]): one streaming `SELECT` (chunks fetched as the rows
//!   are consumed, never the whole result at once) on the session's
//!   blocking thread. Each column is read by its type (from `DESCRIBE`):
//!   booleans, integers, floats, text and blobs as they come (blobs
//!   whole); `DECIMAL`, `HUGEINT`, `UHUGEINT` and `BIGNUM` as their exact
//!   digits (`Decimal`); `UUID`, `DATE`, `TIME` and `TIMESTAMP[_S|_MS|_NS]`
//!   as DuckDB's own text (ISO, full precision; years past 9999 and BC as
//!   DuckDB writes them); `TIMESTAMPTZ` as the UTC instant in that same
//!   text with `+00:00`; `LIST`, arrays, `STRUCT`, `MAP` and `UNION` as
//!   JSON (see [`Ty`]); `JSON` as is; anything else (`INTERVAL`, `BIT`,
//!   `TIMETZ`, enums, user types) as DuckDB's text. The read runs in a
//!   `READ ONLY` transaction, and its filter must parse as one `SELECT`
//!   together with the rest of the statement: the source is only read.
//! - **Load** ([`bulk_load`]): DuckDB's Appender, one per commit window,
//!   inside `BEGIN … COMMIT`, with the load's column list (the others take
//!   their defaults). Integers, floats, booleans and blobs are appended as
//!   such; every other cell as text, which the appender casts to the
//!   column's type exactly (decimals, dates, UUIDs…). DuckDB has no
//!   table locks nor identity columns: `table_lock` and `keep_identity`
//!   need nothing (a column filled by a sequence takes the given value).
//!   The next batch is only asked for once the loading thread took the
//!   last one (nothing queued beyond the orchestrator's window), and a
//!   dropped load never commits afterwards (see [`CancelOnDrop`]). A
//!   `JSON` inside a nested column takes a JSON string holding the
//!   document's text, as the read sends it: a string that isn't a document
//!   (`{"j": "x"}` from another engine) fails the load with that reason,
//!   and one that is (`"12"`) loads as that document.
//! - **Native copy** ([`copy_native`]): only between two sessions of the
//!   same database instance (the same file, or `:memory:`, possibly
//!   different attached catalogs) and without a filter: one
//!   `INSERT INTO … SELECT …` in a transaction, rows never leave DuckDB.
//!   Two different files are two instances in this process, and one
//!   instance can't safely attach a file that another instance has open,
//!   so that case answers `Unsupported` and the migration reads and
//!   appends; so does a filtered copy, whose read then runs read-only.

use crate::{duck_error, DuckDbSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, BatchSource, Cell, CopySpec, LoadSpec, Progress, ReadSpec, RowBatch, TransferColumn};
use dbine_driver::{Error, ObjectRef, Result, Session};
use duckdb::types::{ToSqlOutput, ValueRef};
use duckdb::{Connection, InterruptHandle, ToSql};
use std::fmt;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, TryLockError};
use tokio::sync::mpsc;

/// How a column's values become cells.
#[derive(Debug, Clone, Copy, PartialEq)]
enum Kind {
    /// Booleans, integers, floats, text and blobs: straight from the value.
    Raw,
    Json,
    Decimal,
    Uuid,
    Date,
    Time,
    DateTime,
    /// Microseconds since the epoch (UTC), or DuckDB's text for ±infinity.
    DateTimeTz,
    Text,
}

/// The kind of a `DESCRIBE` type and the select expression that reads it.
fn classify(ty: &str) -> Kind {
    let t = ty.trim().to_ascii_uppercase();
    let nested = t.ends_with(']') || ["STRUCT(", "MAP(", "UNION("].iter().any(|p| t.starts_with(p));
    if nested {
        return Kind::Json;
    }
    let base = t.split('(').next().unwrap_or("").trim();
    match base {
        "BOOLEAN" | "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "UTINYINT" | "USMALLINT" | "UINTEGER" | "UBIGINT" | "FLOAT"
        | "DOUBLE" | "VARCHAR" | "BLOB" | "GEOMETRY" => Kind::Raw,
        "JSON" => Kind::Json,
        "DECIMAL" | "NUMERIC" | "HUGEINT" | "UHUGEINT" | "BIGNUM" | "VARINT" => Kind::Decimal,
        "UUID" => Kind::Uuid,
        "DATE" => Kind::Date,
        "TIME" | "TIME_NS" => Kind::Time,
        "TIMESTAMP" | "TIMESTAMP_S" | "TIMESTAMP_MS" | "TIMESTAMP_NS" | "DATETIME" => Kind::DateTime,
        "TIMESTAMP WITH TIME ZONE" | "TIMESTAMPTZ" => Kind::DateTimeTz,
        _ => Kind::Text,
    }
}

fn expr(col: &str, kind: Kind, ty: &str) -> Result<String> {
    let q = quote_ident(Quote::Double, col);
    Ok(match kind {
        Kind::Raw => q,
        Kind::Json if ty.trim().eq_ignore_ascii_case("JSON") => q,
        Kind::Json => format!("CAST(to_json({}) AS VARCHAR)", enc(&q, &nested_type(ty)?, 0)),
        Kind::DateTimeTz => tz_text(&q),
        _ => format!("CAST({q} AS VARCHAR)"),
    })
}

/// A `TIMESTAMPTZ` as the UTC instant in DuckDB's own `TIMESTAMP` text plus
/// `+00:00` (`0044-03-15 (BC) 00:00:00+00:00`, `12000-01-01 …`), which a
/// cast to `TIMESTAMPTZ` reads back exactly. Not `::VARCHAR` (the session's
/// zone, whose historic offsets DuckDB writes without their seconds) nor
/// `timezone('UTC', …)` (wrong near the top of the range): the epoch's
/// microseconds as a `TIMESTAMP` are exact over the whole range.
fn tz_text(x: &str) -> String {
    format!("CASE WHEN isfinite({x}) THEN CAST(make_timestamp(epoch_us({x})) AS VARCHAR) || '+00:00' ELSE CAST({x} AS VARCHAR) END")
}

fn is_tz(leaf: &str) -> bool {
    let t = leaf.trim().to_ascii_uppercase();
    t == "TIMESTAMP WITH TIME ZONE" || t == "TIMESTAMPTZ"
}

/// A nested DuckDB type as `DESCRIBE` spells it, broken down.
///
/// Nested values travel as JSON, but `to_json` and a cast from `JSON` are
/// only exact for some scalars: decimals and 128-bit integers go through
/// `double`, blobs come back as their escaped text, `BIGNUM` isn't read at
/// all, a `MAP`'s keys are strings, a `TIMESTAMPTZ` is written in the
/// session's zone, an embedded `JSON` is re-encoded (its numbers through
/// `double`) and a float's NaN or infinity isn't JSON at all. So the read
/// turns every other scalar inside into its exact text first ([`enc`]; a
/// float's non-finite values only, see [`is_float`]), map keys too, and
/// the load casts the JSON to
/// that same "wire" shape ([`wire`]) and then to the column's type, which
/// reads each text back exactly (`'1.5'` → `DECIMAL`, `'\x00'` → `BLOB`,
/// `'[x]'` → `VARCHAR[]`…).
#[derive(Debug, Clone, PartialEq)]
enum Ty {
    /// A scalar, as spelled (`DECIMAL(38,10)`, `ENUM('a', 'b')`…).
    Leaf(String),
    /// `T[]` or `T[n]`: the element and the suffix.
    List(Box<Ty>, String),
    /// Field names as spelled (quoted or not) and their types.
    Struct(Vec<(String, Ty)>),
    Map(Box<Ty>, Box<Ty>),
    Union(Vec<(String, Ty)>),
}

impl fmt::Display for Ty {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let fields = |fs: &[(String, Ty)]| fs.iter().map(|(n, t)| format!("{n} {t}")).collect::<Vec<_>>().join(", ");
        match self {
            Ty::Leaf(s) => f.write_str(s),
            Ty::List(t, suffix) => write!(f, "{t}{suffix}"),
            Ty::Struct(fs) => write!(f, "STRUCT({})", fields(fs)),
            Ty::Map(k, v) => write!(f, "MAP({k}, {v})"),
            Ty::Union(fs) => write!(f, "UNION({})", fields(fs)),
        }
    }
}

fn nested_type(ty: &str) -> Result<Ty> {
    let mut p = TypeParser { s: ty, i: 0 };
    let t = p.ty();
    p.ws();
    match t {
        Some(t) if p.i == ty.len() => Ok(t),
        _ => Err(Error::Unsupported(format!("no se reconoce el tipo anidado «{ty}» para copiarlo sin pérdida"))),
    }
}

struct TypeParser<'a> {
    s: &'a str,
    i: usize,
}

impl TypeParser<'_> {
    fn peek(&self) -> Option<u8> {
        self.s.as_bytes().get(self.i).copied()
    }

    fn ws(&mut self) {
        while self.peek().is_some_and(|c| c.is_ascii_whitespace()) {
            self.i += 1;
        }
    }

    fn eat(&mut self, c: u8) -> Option<()> {
        self.ws();
        (self.peek() == Some(c)).then(|| self.i += 1)
    }

    fn ty(&mut self) -> Option<Ty> {
        self.ws();
        let rest = &self.s[self.i..];
        let head = |k: &str| rest.len() >= k.len() && rest.as_bytes()[..k.len()].eq_ignore_ascii_case(k.as_bytes());
        let mut t = if head("STRUCT(") {
            self.i += 7;
            Ty::Struct(self.fields()?)
        } else if head("UNION(") {
            self.i += 6;
            Ty::Union(self.fields()?)
        } else if head("MAP(") {
            self.i += 4;
            let k = self.ty()?;
            self.eat(b',')?;
            let v = self.ty()?;
            self.eat(b')')?;
            Ty::Map(Box::new(k), Box::new(v))
        } else {
            self.leaf()?
        };
        loop {
            self.ws();
            if self.peek() != Some(b'[') {
                return Some(t);
            }
            let start = self.i;
            self.i += self.s[start..].find(']')? + 1;
            t = Ty::List(Box::new(t), self.s[start..self.i].to_string());
        }
    }

    fn leaf(&mut self) -> Option<Ty> {
        let start = self.i;
        let mut depth = 0u32;
        while let Some(c) = self.peek() {
            match c {
                b'\'' | b'"' => {
                    self.quoted(c)?;
                    continue;
                }
                b'(' => depth += 1,
                b')' if depth == 0 => break,
                b')' => depth -= 1,
                b',' | b'[' if depth == 0 => break,
                _ => {}
            }
            self.i += 1;
        }
        let raw = self.s[start..self.i].trim();
        (depth == 0 && !raw.is_empty()).then(|| Ty::Leaf(raw.to_string()))
    }

    /// Past a quoted token (the quote doubled inside).
    fn quoted(&mut self, q: u8) -> Option<()> {
        self.i += 1;
        loop {
            let c = self.peek()?;
            self.i += 1;
            if c == q {
                if self.peek() != Some(q) {
                    return Some(());
                }
                self.i += 1;
            }
        }
    }

    fn fields(&mut self) -> Option<Vec<(String, Ty)>> {
        let mut out = Vec::new();
        loop {
            self.ws();
            let start = self.i;
            if self.peek() == Some(b'"') {
                self.quoted(b'"')?;
            } else {
                while self.peek().is_some_and(|c| !c.is_ascii_whitespace() && c != b',' && c != b')') {
                    self.i += 1;
                }
            }
            let name = self.s[start..self.i].to_string();
            if name.is_empty() {
                return None;
            }
            out.push((name, self.ty()?));
            self.ws();
            match self.peek()? {
                b',' => self.i += 1,
                b')' => {
                    self.i += 1;
                    return Some(out);
                }
                _ => return None,
            }
        }
    }
}

/// A field name as spelled in a type, unquoted.
fn unquote(name: &str) -> String {
    match name.strip_prefix('"').and_then(|n| n.strip_suffix('"')) {
        Some(inner) => inner.replace("\"\"", "\""),
        None => name.to_string(),
    }
}

fn literal(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// Scalars whose `to_json` and cast back from `JSON` are exact. Not
/// `JSON` itself: `to_json` re-encodes an embedded document (its numbers
/// through `double`), so it travels as its own text. Not the floats
/// either (see [`is_float`]).
fn json_exact(leaf: &str) -> bool {
    matches!(
        leaf.trim().to_ascii_uppercase().as_str(),
        "BOOLEAN" | "TINYINT" | "SMALLINT" | "INTEGER" | "BIGINT" | "UTINYINT" | "USMALLINT" | "UINTEGER" | "UBIGINT" | "VARCHAR"
    )
}

/// `FLOAT` and `DOUBLE`: finite values are exact JSON numbers, but
/// `to_json` writes NaN and ±infinity as bare `NaN`/`Infinity` tokens,
/// which aren't JSON. They travel as a `JSON` leaf: the number, or the
/// strings `"NaN"`, `"Infinity"` and `"-Infinity"`, which a cast from
/// `JSON` reads back into the same float.
fn is_float(leaf: &str) -> bool {
    matches!(leaf.trim().to_ascii_uppercase().as_str(), "FLOAT" | "DOUBLE" | "REAL" | "FLOAT4" | "FLOAT8")
}

/// The shape a nested type travels in: other scalars and map keys as text.
fn wire(t: &Ty) -> Ty {
    let fields = |fs: &[(String, Ty)]| fs.iter().map(|(n, t)| (n.clone(), wire(t))).collect();
    match t {
        Ty::Leaf(s) if json_exact(s) => t.clone(),
        Ty::Leaf(s) if is_float(s) => Ty::Leaf("JSON".into()),
        Ty::Leaf(_) => Ty::Leaf("VARCHAR".into()),
        Ty::List(e, suffix) => Ty::List(Box::new(wire(e)), suffix.clone()),
        Ty::Struct(fs) => Ty::Struct(fields(fs)),
        Ty::Map(_, v) => Ty::Map(Box::new(Ty::Leaf("VARCHAR".into())), Box::new(wire(v))),
        Ty::Union(fs) => Ty::Union(fields(fs)),
    }
}

/// `x` (of type `t`) turned into its [`wire`] shape.
fn enc(x: &str, t: &Ty, depth: usize) -> String {
    if wire(t) == *t {
        return x.to_string();
    }
    let e = format!("e{depth}");
    match t {
        Ty::Leaf(s) if is_tz(s) => tz_text(x),
        Ty::Leaf(s) if is_float(s) => format!(
            "CASE WHEN isnan({x}) THEN '\"NaN\"'::JSON WHEN isinf({x}) THEN (CASE WHEN {x} > 0 THEN '\"Infinity\"' ELSE '\"-Infinity\"' END)::JSON ELSE to_json({x}) END"
        ),
        Ty::Leaf(_) => format!("CAST({x} AS VARCHAR)"),
        Ty::List(inner, _) => format!("list_transform({x}, lambda {e}: {})", enc(&e, inner, depth + 1)),
        Ty::Struct(fs) => {
            let packed: Vec<String> = fs
                .iter()
                .map(|(n, ft)| {
                    let name = unquote(n);
                    let field = format!("struct_extract({x}, {})", literal(&name));
                    format!("{} := {}", quote_ident(Quote::Double, &name), enc(&field, ft, depth + 1))
                })
                .collect();
            format!("CASE WHEN {x} IS NULL THEN NULL ELSE struct_pack({}) END", packed.join(", "))
        }
        Ty::Map(k, v) => {
            // A float key goes as its own text (`nan`, `-inf`, `-0.0`),
            // which the cast back reads exactly: not the JSON leaf of a
            // float value, whose `"NaN"` would reach the key with its quotes.
            let key = format!("struct_extract({e}, 'key')");
            let key = match &**k {
                Ty::Leaf(s) if is_float(s) => key,
                _ => enc(&key, k, depth + 1),
            };
            format!(
                "CASE WHEN {x} IS NULL THEN NULL ELSE map_from_entries(list_transform(map_entries({x}), lambda {e}: {{'key': CAST({key} AS VARCHAR), 'value': {}}})) END",
                enc(&format!("struct_extract({e}, 'value')"), v, depth + 1)
            )
        }
        Ty::Union(fs) => {
            let w = wire(t);
            let arms: Vec<String> = fs
                .iter()
                .map(|(n, ft)| {
                    let name = unquote(n);
                    let member = format!("union_extract({x}, {})", literal(&name));
                    format!(
                        "WHEN union_tag({x}) = {} THEN CAST(union_value({} := {}) AS {w})",
                        literal(&name),
                        quote_ident(Quote::Double, &name),
                        enc(&member, ft, depth + 1)
                    )
                })
                .collect();
            format!("CASE {} END", arms.join(" "))
        }
    }
}

/// Whether a `JSON` sits inside the nested type `t`. It travels as a JSON
/// string holding the document's text (a cast from `JSON` to `VARCHAR`
/// and back): a plain string there from another engine (`"x"`) isn't a
/// document and fails the load ([`JSON_LEAF`]).
fn has_json_leaf(t: &Ty) -> bool {
    match t {
        Ty::Leaf(s) => s.trim().eq_ignore_ascii_case("JSON"),
        Ty::List(e, _) => has_json_leaf(e),
        Ty::Struct(fs) | Ty::Union(fs) => fs.iter().any(|(_, t)| has_json_leaf(t)),
        Ty::Map(k, v) => has_json_leaf(k) || has_json_leaf(v),
    }
}

/// Why a load into a nested `JSON` failed with DuckDB's "Malformed JSON".
const JSON_LEAF: &str = "un JSON dentro de un tipo anidado (STRUCT, lista, MAP o UNION) se carga desde una cadena JSON con el texto del \
     documento, como lo lee DBine; un texto suelto en esa posición no es un documento JSON y no se carga para no cambiar su significado";

/// The load's cast of a staged JSON text into the nested column's type.
fn dec(col: &str, ty: &str) -> Result<String> {
    let t = nested_type(ty)?;
    let w = wire(&t);
    Ok(if w == t { format!("CAST(CAST({col} AS JSON) AS {ty})") } else { format!("CAST(CAST(CAST({col} AS JSON) AS {w}) AS {ty})") })
}

/// `"catalog"."schema"."name"` in the session's catalog.
fn qualified(catalog: &str, t: &ObjectRef) -> String {
    format!(
        "{}.{}.{}",
        quote_ident(Quote::Double, catalog),
        quote_ident(Quote::Double, t.schema().unwrap_or("main")),
        quote_ident(Quote::Double, &t.name)
    )
}

fn column_list(cols: &[String]) -> String {
    cols.iter().map(|c| quote_ident(Quote::Double, c)).collect::<Vec<_>>().join(", ")
}

fn text(b: &[u8]) -> std::result::Result<String, Vec<u8>> {
    String::from_utf8(b.to_vec()).map_err(|e| e.into_bytes())
}

fn cell(v: ValueRef<'_>, kind: Kind) -> Cell {
    if matches!(v, ValueRef::Null) {
        return Cell::Null;
    }
    if kind != Kind::Raw {
        let s = match v {
            ValueRef::Text(b) => String::from_utf8_lossy(b).into_owned(),
            other => format!("{other:?}"),
        };
        return match kind {
            Kind::Json => Cell::Json(s),
            Kind::Decimal => Cell::Decimal(s),
            Kind::Uuid => Cell::Uuid(s),
            Kind::Date => Cell::Date(s),
            Kind::Time => Cell::Time(s),
            Kind::DateTime => Cell::DateTime(s),
            Kind::DateTimeTz => Cell::DateTimeTz(s),
            _ => Cell::Text(s),
        };
    }
    match v {
        ValueRef::Boolean(b) => Cell::Bool(b),
        ValueRef::TinyInt(i) => Cell::Int(i.into()),
        ValueRef::SmallInt(i) => Cell::Int(i.into()),
        ValueRef::Int(i) => Cell::Int(i.into()),
        ValueRef::BigInt(i) => Cell::Int(i),
        ValueRef::UTinyInt(i) => Cell::Int(i.into()),
        ValueRef::USmallInt(i) => Cell::Int(i.into()),
        ValueRef::UInt(i) => Cell::Int(i.into()),
        ValueRef::UBigInt(i) => Cell::UInt(i),
        ValueRef::Float(f) => Cell::Float(f.into()),
        ValueRef::Double(f) => Cell::Float(f),
        ValueRef::Text(b) => text(b).map_or_else(Cell::Bytes, Cell::Text),
        ValueRef::Blob(b) | ValueRef::Geometry(b) => Cell::Bytes(b.to_vec()),
        other => Cell::Text(format!("{other:?}")),
    }
}

/// Columns of the read (name, type, nullable) from `DESCRIBE` of the table,
/// in the spec's order. Names match without regard to case, as DuckDB's
/// identifiers do (an exact match first).
fn describe(c: &Connection, table: &str, columns: Option<&[String]>) -> Result<Vec<TransferColumn>> {
    let mut stmt = c.prepare(&format!("DESCRIBE {table}")).map_err(duck_error)?;
    let all = stmt
        .query_map([], |r| {
            Ok(TransferColumn { name: r.get(0)?, type_name: r.get(1)?, nullable: r.get::<_, Option<String>>(2)?.as_deref() != Some("NO") })
        })
        .map_err(duck_error)?
        .collect::<duckdb::Result<Vec<_>>>()
        .map_err(duck_error)?;
    let Some(wanted) = columns else { return Ok(all) };
    wanted
        .iter()
        .map(|w| {
            all.iter()
                .find(|c| &c.name == w)
                .or_else(|| all.iter().find(|c| c.name.eq_ignore_ascii_case(w)))
                .cloned()
                .ok_or_else(|| Error::Query(format!("no existe la columna «{w}»")))
        })
        .collect()
}

/// The filter of a read, if any.
fn filter_of(spec: &ReadSpec) -> Option<&str> {
    spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty())
}

/// Fails unless `sql` parses as exactly one `SELECT`. duckdb-rs runs every
/// statement of a string but the last one on `prepare`, so a filter like
/// `true; DELETE FROM t` would otherwise write. DuckDB's own parser decides
/// (`json_serialize_sql` only serializes `SELECT`s), parameters keep the
/// text out of the SQL.
fn one_select(c: &Connection, sql: &str) -> Result<()> {
    let out: String = c.query_row("SELECT CAST(json_serialize_sql(CAST(? AS VARCHAR)) AS VARCHAR)", [sql], |r| r.get(0)).map_err(duck_error)?;
    let v: serde_json::Value = serde_json::from_str(&out).map_err(|e| Error::State(format!("json_serialize_sql: {e}")))?;
    let why = if v["error"].as_bool() != Some(false) {
        v["error_message"].as_str().unwrap_or("no se pudo analizar").to_string()
    } else if v["statements"].as_array().map(Vec::len) != Some(1) {
        "hay más de una sentencia".to_string()
    } else {
        return Ok(());
    };
    Err(Error::Query(format!("el filtro tiene que ser una sola condición, sin otras sentencias ({why})")))
}

/// Read `spec` into `sink` on this (blocking) thread; the rows read. It
/// runs in a `READ ONLY` transaction (unless the session already has one
/// open): nothing in it, a filter's `nextval` included, can write.
pub(crate) fn read(c: &Connection, catalog: &str, spec: &ReadSpec, sink: &BatchSinkRef) -> Result<u64> {
    let table = qualified(catalog, &spec.table);
    let cols = describe(c, &table, spec.columns.as_deref())?;
    let kinds: Vec<Kind> = cols.iter().map(|c| classify(&c.type_name)).collect();
    let exprs = cols.iter().zip(&kinds).map(|(c, k)| expr(&c.name, *k, &c.type_name)).collect::<Result<Vec<String>>>()?;
    let mut sql = format!("SELECT {} FROM {table}", exprs.join(", "));
    if let Some(f) = filter_of(spec) {
        // The newline ends a trailing `--` comment before the parenthesis.
        sql.push_str(&format!(" WHERE ({f}\n)"));
        one_select(c, &sql)?;
    }
    let own = c.is_autocommit();
    if own {
        c.execute_batch("BEGIN TRANSACTION READ ONLY").map_err(duck_error)?;
    }
    let r = stream(c, &sql, &cols, &kinds, sink);
    if own {
        let end = c.execute_batch(if r.is_ok() { "COMMIT" } else { "ROLLBACK" });
        if r.is_ok() {
            end.map_err(duck_error)?;
        }
    }
    r
}

fn stream(c: &Connection, sql: &str, cols: &[TransferColumn], kinds: &[Kind], sink: &BatchSinkRef) -> Result<u64> {
    let lock = || sink.lock().map_err(|_| Error::State("destino de lotes".into()));
    lock()?.begin(cols)?;
    let mut stmt = c.prepare(sql).map_err(duck_error)?;
    // Streaming execution; the rows are then stepped chunk by chunk.
    drop(stmt.stream_arrow([]).map_err(duck_error)?);
    let mut rows = stmt.raw_query();
    let mut builder = BatchBuilder::new();
    let n = kinds.len();
    while let Some(row) = rows.next().map_err(duck_error)? {
        let cells = (0..n).map(|i| cell(row.get_ref_unwrap(i), kinds[i])).collect();
        builder.push(cells, &mut *lock()?)?;
    }
    builder.flush(&mut *lock()?)?;
    Ok(builder.rows)
}

/// A cell for the appender: native values where DuckDB has them, text
/// (cast by the appender to the column's type) for the rest.
struct Bind<'a>(&'a Cell);

impl ToSql for Bind<'_> {
    fn to_sql(&self) -> duckdb::Result<ToSqlOutput<'_>> {
        Ok(ToSqlOutput::Borrowed(match self.0 {
            Cell::Null => ValueRef::Null,
            Cell::Bool(b) => ValueRef::Boolean(*b),
            Cell::Int(i) => ValueRef::BigInt(*i),
            Cell::UInt(u) => ValueRef::UBigInt(*u),
            Cell::Float(f) => ValueRef::Double(*f),
            Cell::Bytes(b) => ValueRef::Blob(b),
            Cell::Decimal(s)
            | Cell::Text(s)
            | Cell::Date(s)
            | Cell::Time(s)
            | Cell::DateTime(s)
            | Cell::DateTimeTz(s)
            | Cell::Uuid(s)
            | Cell::Json(s) => ValueRef::Text(s.as_bytes()),
        }))
    }
}

enum Msg {
    Batch(RowBatch),
    /// The source ended: commit what's pending. Without it (the load was
    /// dropped halfway), the open window rolls back.
    End,
}

/// The temporary table nested columns are staged in.
const STAGE: &str = "dbine_load_stage";

/// Where the appender writes. The appender casts text only to some types
/// (numbers, dates and times, UUIDs…) and into `TIMESTAMPTZ` it drops
/// the text's offset (a `-03:00` would be stored as UTC); nested target columns (`LIST`,
/// arrays, `STRUCT`, `MAP`, `UNION`) come as JSON, which a cast from
/// `VARCHAR` doesn't read (a `MAP` has its own literal syntax), and others
/// (`INTERVAL`, `BIT`, `TIMETZ`, enums, user types) it doesn't cast at
/// all. With any of those (or a `TIMESTAMPTZ`), the window is appended to a temporary table
/// where they are `VARCHAR`, and moved with one `INSERT … SELECT` that
/// casts them ([`dec`] for nested ones, `col::<type>` for the rest) before
/// its commit.
struct Plan {
    catalog: String,
    schema: String,
    table: String,
    /// The target's own spelling of the load's columns.
    columns: Vec<String>,
    /// The move from the stage, when there is one.
    insert: Option<String>,
    /// A nested column holds a `JSON` ([`has_json_leaf`]).
    json_leaf: bool,
}

fn plan(c: &Connection, catalog: &str, spec: &LoadSpec) -> Result<Plan> {
    let target = qualified(catalog, &spec.table);
    let cols = describe(c, &target, Some(&spec.columns))?;
    let columns: Vec<String> = cols.iter().map(|c| c.name.clone()).collect();
    let nested = |t: &str| classify(t) == Kind::Json && !t.trim().eq_ignore_ascii_case("JSON");
    let staged = |t: &str| nested(t) || matches!(classify(t), Kind::Text | Kind::DateTimeTz);
    if !cols.iter().any(|c| staged(&c.type_name)) {
        return Ok(Plan {
            catalog: catalog.to_string(),
            schema: spec.table.schema().unwrap_or("main").to_string(),
            table: spec.table.name.clone(),
            columns,
            insert: None,
            json_leaf: false,
        });
    }
    let json_leaf = cols.iter().any(|c| nested(&c.type_name) && nested_type(&c.type_name).is_ok_and(|t| has_json_leaf(&t)));
    // Before the stage exists: a type the load can't read fails here.
    let moved = cols
        .iter()
        .map(|c| {
            let q = quote_ident(Quote::Double, &c.name);
            if nested(&c.type_name) {
                dec(&q, &c.type_name)
            } else if staged(&c.type_name) {
                Ok(format!("CAST({q} AS {})", c.type_name))
            } else {
                Ok(q)
            }
        })
        .collect::<Result<Vec<String>>>()?;
    let stage_cols: Vec<String> = cols
        .iter()
        .map(|c| {
            let q = quote_ident(Quote::Double, &c.name);
            if staged(&c.type_name) {
                format!("NULL::VARCHAR AS {q}")
            } else {
                q
            }
        })
        .collect();
    c.execute_batch(&format!("CREATE OR REPLACE TEMP TABLE {STAGE} AS SELECT {} FROM {target} LIMIT 0", stage_cols.join(", ")))
        .map_err(duck_error)?;
    Ok(Plan {
        catalog: "temp".into(),
        schema: "main".into(),
        table: STAGE.into(),
        insert: Some(format!(
            "INSERT INTO {target} ({}) SELECT {} FROM temp.main.{STAGE}; DELETE FROM temp.main.{STAGE};",
            column_list(&columns),
            moved.join(", ")
        )),
        columns,
        json_leaf,
    })
}

fn appender<'c>(c: &'c Connection, plan: &Plan) -> Result<duckdb::Appender<'c>> {
    let cols: Vec<&str> = plan.columns.iter().map(String::as_str).collect();
    c.appender_with_columns_to_catalog_and_db(&plan.table, &plan.catalog, &plan.schema, &cols).map_err(duck_error)
}

/// From the loading thread to the async side.
enum Event {
    /// The batch handed over was appended (and committed, if it closed a
    /// window): the next one may be read.
    Taken,
    /// Rows committed so far.
    Committed(u64),
}

/// What a dropped `bulk_load` / `copy_native` future leaves to its blocking
/// thread, which keeps running: it must not commit after the call is gone
/// (a resume or a retry may already have emptied the table). Each commit
/// happens holding `gate` and only if `cancelled` is still false; the drop
/// sets `cancelled`, interrupts the statement in flight and then takes
/// `gate`, so a commit already under way ends (or fails) before the drop
/// returns, and none starts after.
#[derive(Default)]
struct Cancel {
    cancelled: AtomicBool,
    gate: Mutex<()>,
}

impl Cancel {
    /// Run `commit` unless the call was dropped.
    fn commit<T>(&self, commit: impl FnOnce() -> Result<T>) -> Result<T> {
        let _gate = self.gate.lock().unwrap_or_else(|p| p.into_inner());
        if self.cancelled.load(Ordering::SeqCst) {
            return Err(Error::Cancelled);
        }
        commit()
    }

    fn is_cancelled(&self) -> bool {
        self.cancelled.load(Ordering::SeqCst)
    }
}

struct CancelOnDrop {
    cancel: Arc<Cancel>,
    interrupt: Arc<InterruptHandle>,
    /// Interrupt even when no commit is under way (a long statement that
    /// isn't worth finishing).
    always_interrupt: bool,
    armed: bool,
}

impl Drop for CancelOnDrop {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        self.cancel.cancelled.store(true, Ordering::SeqCst);
        if self.always_interrupt {
            self.interrupt.interrupt();
        }
        match self.cancel.gate.try_lock() {
            Ok(_) | Err(TryLockError::Poisoned(_)) => {}
            Err(TryLockError::WouldBlock) => {
                // A commit is under way: cut it short and wait for it.
                self.interrupt.interrupt();
                drop(self.cancel.gate.lock());
            }
        }
    }
}

fn load(c: &Connection, catalog: &str, spec: &LoadSpec, mut rx: mpsc::Receiver<Msg>, events: mpsc::UnboundedSender<Event>, cancel: &Cancel) -> Result<u64> {
    let max_rows = if spec.commit_rows == 0 { u64::MAX } else { spec.commit_rows };
    let max_bytes = if spec.commit_bytes == 0 { u64::MAX } else { spec.commit_bytes };
    let ncols = spec.columns.len();
    if cancel.is_cancelled() {
        return Err(Error::Cancelled);
    }
    // Fails early (missing table or column) before any transaction.
    let plan = plan(c, catalog, spec)?;
    let at = |total: u64, rows: u64, e: duckdb::Error| match duck_error(e) {
        Error::Query(m) => Error::Query(format!("fila {}: {m}", total + rows)),
        other => other,
    };
    // A flush (or the move from the stage) reports rows of the whole
    // window: constraints are checked then.
    let window = |total: u64, rows: u64, e: duckdb::Error| match duck_error(e) {
        Error::Query(m) if plan.json_leaf && m.contains("Malformed JSON") => {
            Error::Query(format!("filas {} a {}: {JSON_LEAF} ({m})", total + 1, total + rows))
        }
        Error::Query(m) => Error::Query(format!("filas {} a {}: {m}", total + 1, total + rows)),
        other => other,
    };
    let commit = |app: duckdb::Appender<'_>, total: u64, rows: u64| -> Result<()> {
        cancel.commit(|| {
            let mut app = app;
            app.flush().map_err(|e| window(total, rows, e))?;
            drop(app);
            if let Some(insert) = &plan.insert {
                c.execute_batch(insert).map_err(|e| window(total, rows, e))?;
            }
            c.execute_batch("COMMIT").map_err(duck_error)
        })
    };
    let mut run = || -> Result<u64> {
        let (mut total, mut rows, mut bytes) = (0u64, 0u64, 0u64);
        let mut app: Option<duckdb::Appender<'_>> = None;
        loop {
            let batch = match rx.blocking_recv() {
                Some(Msg::Batch(b)) => b,
                Some(Msg::End) => break,
                None => return Err(Error::Cancelled),
            };
            for row in &batch.rows {
                if row.len() != ncols {
                    return Err(Error::Query(format!("una fila trae {} valores y la carga tiene {ncols} columnas", row.len())));
                }
                if app.is_none() {
                    c.execute_batch("BEGIN TRANSACTION").map_err(duck_error)?;
                    app = Some(appender(c, &plan)?);
                }
                let a = app.as_mut().expect("open window");
                rows += 1;
                a.append_row(duckdb::appender_params_from_iter(row.iter().map(Bind))).map_err(|e| at(total, rows, e))?;
                bytes += row.iter().map(Cell::size).sum::<usize>() as u64;
                if rows >= max_rows || bytes >= max_bytes {
                    commit(app.take().expect("open window"), total, rows)?;
                    total += rows;
                    (rows, bytes) = (0, 0);
                    let _ = events.send(Event::Committed(total));
                }
            }
            // Freed before the next batch is asked for.
            drop(batch);
            let _ = events.send(Event::Taken);
        }
        if let Some(a) = app.take() {
            commit(a, total, rows)?;
            total += rows;
            let _ = events.send(Event::Committed(total));
        }
        Ok(total)
    };
    let r = run();
    if r.is_err() {
        // Nothing to roll back when the failure came between windows.
        let _ = c.execute_batch("ROLLBACK");
    }
    if plan.insert.is_some() {
        let _ = c.execute_batch(&format!("DROP TABLE IF EXISTS temp.main.{STAGE}"));
    }
    r
}

/// Batches go to the loading thread one at a time, and the next one is
/// only read from `source` once the thread took the last one: the rows in
/// flight are the orchestrator's window plus nothing (its slot for the
/// batch handed out is freed on the next `next()`).
pub(crate) async fn bulk_load(s: &DuckDbSession, spec: &LoadSpec, source: &mut dyn BatchSource, progress: Progress<'_>) -> Result<u64> {
    let (tx, rx) = mpsc::channel::<Msg>(1);
    let (etx, mut erx) = mpsc::unbounded_channel::<Event>();
    let conn = s.conn.clone();
    let catalog = s.catalog.clone();
    let spec_owned = spec.clone();
    let cancel = Arc::new(Cancel::default());
    let mut guard = CancelOnDrop { cancel: cancel.clone(), interrupt: s.interrupt.clone(), always_interrupt: false, armed: true };
    let worker = tokio::task::spawn_blocking(move || {
        let c = conn.lock().map_err(|_| Error::State("conexión DuckDB envenenada".into()))?;
        load(&c, &catalog, &spec_owned, rx, etx, &cancel)
    });
    'feed: while let Some(batch) = source.next().await {
        if tx.send(Msg::Batch(batch)).await.is_err() {
            break; // the thread stopped: its error comes below
        }
        loop {
            match erx.recv().await {
                Some(Event::Taken) => break,
                Some(Event::Committed(n)) => progress(n),
                None => break 'feed,
            }
        }
    }
    let _ = tx.send(Msg::End).await;
    drop(tx);
    while let Some(e) = erx.recv().await {
        if let Event::Committed(n) = e {
            progress(n);
        }
    }
    let r = worker.await.map_err(|e| Error::State(e.to_string()))?;
    guard.armed = false;
    r
}

fn session(s: &mut dyn Session) -> Option<&mut DuckDbSession> {
    s.as_any()?.downcast_mut::<DuckDbSession>()
}

pub(crate) async fn copy_native(source: &mut dyn Session, target: &mut dyn Session, spec: &CopySpec, progress: Progress<'_>) -> Result<u64> {
    let unsupported = |why: &str| Err(Error::Unsupported(format!("copia directa no disponible: {why}")));
    let Some(src) = session(source) else { return unsupported("el origen no es una sesión DuckDB") };
    let (src_db, src_catalog) = (src._db.clone(), src.catalog.clone());
    let Some(dst) = session(target) else { return unsupported("el destino no es una sesión DuckDB") };
    if !Arc::ptr_eq(&src_db, &dst._db) {
        return unsupported("origen y destino son archivos distintos (DuckDB no comparte un archivo abierto entre instancias)");
    }
    if filter_of(&spec.source).is_some() {
        // The `INSERT … SELECT` runs in a transaction that writes, where a
        // filter's side effects (`nextval`…) would reach the source; the
        // read by batches runs it read-only.
        return unsupported("con filtro, el origen se lee por lotes en una transacción de solo lectura");
    }
    let from = qualified(&src_catalog, &spec.source.table);
    let into = qualified(&dst.catalog, &spec.target.table);
    let spec = spec.clone();
    let cancel = Arc::new(Cancel::default());
    // Interrupted on drop even mid-`INSERT`: its rows would be rolled back anyway.
    let mut guard = CancelOnDrop { cancel: cancel.clone(), interrupt: dst.interrupt.clone(), always_interrupt: true, armed: true };
    let rows = dst
        .with(move |c| {
            if cancel.is_cancelled() {
                return Err(Error::Cancelled);
            }
            let src_cols: Vec<String> = describe(c, &from, spec.source.columns.as_deref())?.into_iter().map(|c| c.name).collect();
            let dst_cols: Vec<String> = describe(c, &into, Some(&spec.target.columns))?.into_iter().map(|c| c.name).collect();
            if src_cols.len() != dst_cols.len() {
                return Err(Error::Query(format!("el origen lee {} columnas y el destino carga {}", src_cols.len(), dst_cols.len())));
            }
            let sql = format!("INSERT INTO {into} ({}) SELECT {} FROM {from}", column_list(&dst_cols), column_list(&src_cols));
            c.execute_batch("BEGIN TRANSACTION").map_err(duck_error)?;
            let r = c.execute(&sql, []).map_err(duck_error).and_then(|n| cancel.commit(|| c.execute_batch("COMMIT").map_err(duck_error)).map(|_| n as u64));
            if r.is_err() {
                let _ = c.execute_batch("ROLLBACK");
            }
            r
        })
        .await;
    guard.armed = false;
    let rows = rows?;
    progress(rows);
    Ok(rows)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn kinds() {
        assert_eq!(classify("DECIMAL(38,10)"), Kind::Decimal);
        assert_eq!(classify("INTEGER[]"), Kind::Json);
        assert_eq!(classify("STRUCT(a INTEGER, b VARCHAR[])"), Kind::Json);
        assert_eq!(classify("MAP(VARCHAR, INTEGER)"), Kind::Json);
        assert_eq!(classify("TIMESTAMP WITH TIME ZONE"), Kind::DateTimeTz);
        assert_eq!(classify("TIMESTAMP_NS"), Kind::DateTime);
        assert_eq!(classify("INTERVAL"), Kind::Text);
        assert_eq!(classify("ENUM('a', 'b')"), Kind::Text);
        assert_eq!(classify("BLOB"), Kind::Raw);
    }

    #[test]
    fn nested_types_parse_and_travel_as_text() {
        let ty = r#"STRUCT("my f" DECIMAL(5,2), "q""x" MAP(VARCHAR[], ENUM('a,b', 'c''d')))[]"#;
        let t = nested_type(ty).unwrap();
        assert_eq!(t.to_string(), ty);
        assert_eq!(wire(&t).to_string(), r#"STRUCT("my f" VARCHAR, "q""x" MAP(VARCHAR, VARCHAR))[]"#);
        let t = nested_type("INTEGER[2][3]").unwrap();
        assert_eq!(wire(&t), t);
        assert_eq!(enc("x", &t, 0), "x");
        let t = nested_type("UNION(n DECIMAL(38,10), s VARCHAR)").unwrap();
        assert_eq!(wire(&t).to_string(), "UNION(n VARCHAR, s VARCHAR)");
        assert_eq!(wire(&nested_type("TIMESTAMP WITH TIME ZONE[]").unwrap()).to_string(), "VARCHAR[]");
        assert_eq!(wire(&nested_type("STRUCT(j JSON, d DOUBLE, f FLOAT)[]").unwrap()).to_string(), "STRUCT(j VARCHAR, d JSON, f JSON)[]");
        assert!(nested_type("STRUCT(a INTEGER").is_err());
        assert!(nested_type("MAP(INTEGER)").is_err());
        assert_eq!(unquote(r#""q""x""#), r#"q"x"#);
    }
}
