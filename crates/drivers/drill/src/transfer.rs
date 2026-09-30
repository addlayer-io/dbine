//! Bulk transfer (see `dbine_driver::transfer`) for Apache Drill: reading
//! only. Drill has no `INSERT`: its tables are written by `CREATE TABLE AS
//! SELECT` from what Drill itself can read, so there is no way to load rows
//! that come from elsewhere, and the driver has no bulk load.
//!
//! Reading: one `SELECT` through `/query.json`, whose answer Drill streams
//! (1.19 and later): the columns and their types first, then one row per
//! line. The body is scanned as it arrives, row by row, so memory stays
//! bounded however big the table; each value is typed from its column's
//! Drill type and taken from its raw JSON text, so `DECIMAL`s keep every
//! digit. Temporal values come as epoch milliseconds, binaries as base64
//! (decoded whole), maps and lists as JSON. Older servers send the types
//! after the rows: those rows wait in memory up to a bound, then in a
//! temporary file.

use crate::{http_error, text, Conn, DrillSession};
use base64::Engine as _;
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, Cell, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use serde_json::Value;

impl DrillSession {
    pub(crate) async fn transfer_read(&mut self, spec: &ReadSpec, sink: BatchSinkRef) -> Result<u64> {
        let list = match &spec.columns {
            Some(c) if !c.is_empty() => {
                // Drill answers an unknown column with NULLs instead of an error.
                self.check_columns(spec, c).await?;
                c.iter().map(|n| crate::quote(n)).collect::<Vec<_>>().join(", ")
            }
            _ => "*".to_string(),
        };
        let mut sql = format!("SELECT {list} FROM {}", self.name(&spec.table));
        if let Some(f) = spec.filter.as_deref().map(str::trim).filter(|f| !f.is_empty()) {
            sql.push_str(&format!(" WHERE {f}"));
        }
        let body = self.body(&sql);
        let conn = self.conn.clone();
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = Some(sql.clone());
        let r = self.cancel.run(read_stream(&conn, &body, sink)).await;
        *self.cancel.current.lock().unwrap_or_else(|e| e.into_inner()) = None;
        r
    }

    /// Every requested column exists: it is in a one-row sample's columns
    /// or, for schemaless sources (JSON, …) where a column may appear only
    /// in later records, it holds at least one value somewhere in the table.
    async fn check_columns(&self, spec: &ReadSpec, cols: &[String]) -> Result<()> {
        let table = self.name(&spec.table);
        let sample = self.query(&format!("SELECT * FROM {table} LIMIT 1")).await?;
        let missing = unknown_columns(cols, sample.columns.iter().map(|(n, _)| n.as_str()));
        if missing.is_empty() {
            return Ok(());
        }
        let counts = missing.iter().map(|c| format!("COUNT({})", crate::quote(c))).collect::<Vec<_>>().join(", ");
        let answer = self.query(&format!("SELECT {counts} FROM {table}")).await?;
        let row = answer.rows.into_iter().next().unwrap_or_default();
        let absent: Vec<&str> =
            missing.iter().enumerate().filter(|(i, _)| row.get(*i).is_none_or(|v| text(v).parse::<u64>().unwrap_or(0) == 0)).map(|(_, c)| *c).collect();
        match absent.as_slice() {
            [] => Ok(()),
            [c] => Err(Error::Query(format!("La tabla {} no tiene la columna {c}.", spec.table.name))),
            cs => Err(Error::Query(format!("La tabla {} no tiene las columnas {}.", spec.table.name, cs.join(", ")))),
        }
    }
}

/// The requested columns missing from `known` (Drill's names are
/// case-insensitive).
fn unknown_columns<'a, 'b>(requested: &'a [String], known: impl Iterator<Item = &'b str>) -> Vec<&'a str> {
    let known: std::collections::HashSet<String> = known.map(str::to_lowercase).collect();
    requested.iter().filter(|c| !known.contains(&c.to_lowercase())).map(String::as_str).collect()
}

async fn read_stream(conn: &Conn, body: &Value, sink: BatchSinkRef) -> Result<u64> {
    let mut resp = None;
    for attempt in 0..2 {
        let r = conn.req(conn.http.post(format!("{}/query.json", conn.base)).json(body)).send().await.map_err(http_error)?;
        if Conn::needs_login(r.status(), r.headers()) {
            if attempt == 0 && conn.password.is_some() {
                conn.login().await?;
                continue;
            }
            return Err(Error::AuthFailed("Drill pide iniciar sesión: revisá el usuario y la contraseña.".into()));
        }
        resp = Some(r);
        break;
    }
    let mut resp = resp.ok_or_else(|| Error::AuthFailed("Drill rechazó la sesión.".into()))?;
    let mut scan = Scanner::default();
    let mut reader = RowReader::new(sink);
    while let Some(chunk) = resp.chunk().await.map_err(http_error)? {
        scan.feed(&chunk, &mut reader)?;
    }
    scan.finish(&mut reader)
}

/// Where the scan of the answer is.
#[derive(Default, PartialEq)]
enum Phase {
    /// Before `"rows":[`.
    #[default]
    Header,
    Rows,
    /// After the rows' `]`.
    Tail,
}

/// An incremental scan of `/query.json`'s answer: the header, each row
/// object, the tail.
#[derive(Default)]
struct Scanner {
    buf: Vec<u8>,
    /// Scanned up to here.
    pos: usize,
    depth: i32,
    in_str: bool,
    esc: bool,
    phase: Phase,
    /// Start of the row object being read.
    row_start: Option<usize>,
    /// The header's text (with `{`), once the rows start.
    header: Option<Vec<u8>>,
    /// Start of the last key string at depth 1.
    key_start: Option<usize>,
    last_key: Vec<u8>,
    tail_start: usize,
}

impl Scanner {
    fn feed(&mut self, chunk: &[u8], rows: &mut RowReader) -> Result<()> {
        self.buf.extend_from_slice(chunk);
        while self.pos < self.buf.len() {
            let c = self.buf[self.pos];
            let i = self.pos;
            self.pos += 1;
            if self.in_str {
                if self.esc {
                    self.esc = false;
                } else if c == b'\\' {
                    self.esc = true;
                } else if c == b'"' {
                    self.in_str = false;
                    if let Some(k) = self.key_start.take() {
                        self.last_key = self.buf[k..=i].to_vec();
                    }
                }
                continue;
            }
            match c {
                b'"' => {
                    self.in_str = true;
                    if self.depth == 1 && self.phase == Phase::Header {
                        self.key_start = Some(i);
                    }
                }
                b'{' | b'[' => {
                    self.depth += 1;
                    if c == b'[' && self.depth == 2 && self.phase == Phase::Header && self.last_key == b"\"rows\"" {
                        // The header: everything before `,"rows"`.
                        let cut = self.buf[..i].iter().rposition(|&b| b == b'"').unwrap_or(0);
                        let key = cut.saturating_sub(5); // the `"rows"` key's opening quote
                        let mut h = self.buf[..key].to_vec();
                        while h.last().is_some_and(|b| b.is_ascii_whitespace() || *b == b',') {
                            h.pop();
                        }
                        h.push(b'}');
                        rows.header(&h)?;
                        self.header = Some(h);
                        self.phase = Phase::Rows;
                    } else if c == b'{' && self.depth == 3 && self.phase == Phase::Rows {
                        self.row_start = Some(i);
                    }
                }
                b'}' | b']' => {
                    self.depth -= 1;
                    if c == b'}' && self.depth == 2 && self.phase == Phase::Rows {
                        if let Some(s) = self.row_start.take() {
                            rows.row(&self.buf[s..=i])?;
                            // Drop what's been read.
                            self.buf.drain(..=i);
                            self.pos = 0;
                        }
                    } else if c == b']' && self.depth == 1 && self.phase == Phase::Rows {
                        self.phase = Phase::Tail;
                        self.tail_start = i + 1;
                    }
                }
                _ => {}
            }
        }
        Ok(())
    }

    fn finish(self, rows: &mut RowReader) -> Result<u64> {
        // The rest of the answer: the state, an error, the metadata of older
        // servers (which send it after the rows).
        let rest: Value = match self.phase {
            Phase::Header => serde_json::from_slice(&self.buf).map_err(|_| {
                Error::Query(format!("respuesta inesperada de Drill: {}", String::from_utf8_lossy(&self.buf).chars().take(500).collect::<String>()))
            })?,
            Phase::Rows => return Err(Error::Query("la respuesta de Drill terminó antes que las filas".into())),
            Phase::Tail => {
                let tail = String::from_utf8_lossy(&self.buf[self.tail_start.min(self.buf.len())..]).into_owned();
                serde_json::from_str(&format!("{{{}", tail.trim_start().trim_start_matches(','))).unwrap_or(Value::Null)
            }
        };
        let state = rest.get("queryState").and_then(Value::as_str).unwrap_or("");
        if state == "CANCELED" {
            return Err(Error::Cancelled);
        }
        if state == "FAILED" || rest.get("errorMessage").is_some() {
            return Err(Error::Query(rest.get("errorMessage").map(text).unwrap_or_else(|| "La consulta falló durante la ejecución.".into())));
        }
        if self.phase == Phase::Header {
            // No rows key at all (an empty answer).
            rows.header(&serde_json::to_vec(&rest).unwrap_or_default())?;
        }
        if let Some(m) = rest.get("metadata").and_then(Value::as_array) {
            rows.late_types(m.iter().map(text).collect());
        }
        rows.finish()
    }
}

/// Rows of the scan turned into cells.
struct RowReader {
    sink: BatchSinkRef,
    builder: BatchBuilder,
    /// Name (as a JSON string) and Drill type.
    columns: Vec<(String, String)>,
    begun: bool,
    /// Rows seen before their types (servers that send the metadata last).
    pending: Pending,
}

impl RowReader {
    fn new(sink: BatchSinkRef) -> RowReader {
        RowReader { sink, builder: BatchBuilder::new(), columns: Vec::new(), begun: false, pending: Pending::new(SPILL_BYTES) }
    }

    fn header(&mut self, h: &[u8]) -> Result<()> {
        let v: Value = serde_json::from_slice(h).map_err(Error::query)?;
        if let Some(m) = v.get("errorMessage") {
            return Err(Error::Query(text(m)));
        }
        let names: Vec<String> = v.get("columns").and_then(Value::as_array).into_iter().flatten().map(text).collect();
        let types: Vec<String> = v.get("metadata").and_then(Value::as_array).into_iter().flatten().map(text).collect();
        self.columns = names.into_iter().enumerate().map(|(i, n)| (n, types.get(i).cloned().unwrap_or_default())).collect();
        if !types.is_empty() {
            self.begin()?;
        }
        Ok(())
    }

    fn late_types(&mut self, types: Vec<String>) {
        if !self.begun {
            for (i, c) in self.columns.iter_mut().enumerate() {
                c.1 = types.get(i).cloned().unwrap_or_default();
            }
        }
    }

    fn begin(&mut self) -> Result<()> {
        let cols: Vec<TransferColumn> =
            self.columns.iter().map(|(n, t)| TransferColumn { name: n.clone(), type_name: t.clone(), nullable: true }).collect();
        self.sink.lock().map_err(|_| Error::State("destino de lotes".into()))?.begin(&cols)?;
        self.begun = true;
        Ok(())
    }

    fn row(&mut self, raw: &[u8]) -> Result<()> {
        if !self.begun {
            return self.pending.push(raw);
        }
        let cells = row_cells(raw, &self.columns)?;
        let mut s = self.sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
        self.builder.push(cells, &mut *s)?;
        Ok(())
    }

    fn finish(&mut self) -> Result<u64> {
        if !self.begun {
            self.begin()?;
            let mut pending = std::mem::replace(&mut self.pending, Pending::new(0));
            pending.drain(|raw| self.row(raw))?;
        }
        let mut s = self.sink.lock().map_err(|_| Error::State("destino de lotes".into()))?;
        self.builder.flush(&mut *s)?;
        Ok(self.builder.rows)
    }
}

/// Rows kept in memory, at most, while waiting for their types; the rest
/// go to a temporary file so memory stays bounded however big the table.
const SPILL_BYTES: usize = 8 << 20;

/// Rows waiting for their types, in arrival order: in memory up to
/// `limit` bytes, then (all of them) in a temporary file, removed on drop.
struct Pending {
    limit: usize,
    mem: Vec<Vec<u8>>,
    bytes: usize,
    spill: Option<Spill>,
}

struct Spill {
    path: std::path::PathBuf,
    file: std::io::BufWriter<std::fs::File>,
}

impl Drop for Spill {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

fn spill_error(e: std::io::Error) -> Error {
    Error::Query(format!("no se pudieron guardar las filas en un archivo temporal mientras llegan sus tipos: {e}"))
}

impl Pending {
    fn new(limit: usize) -> Pending {
        Pending { limit, mem: Vec::new(), bytes: 0, spill: None }
    }

    fn push(&mut self, raw: &[u8]) -> Result<()> {
        use std::io::Write as _;
        if self.spill.is_none() && self.bytes + raw.len() <= self.limit {
            self.bytes += raw.len();
            self.mem.push(raw.to_vec());
            return Ok(());
        }
        if self.spill.is_none() {
            self.spill = Some(Spill::create().map_err(spill_error)?);
        }
        let spill = self.spill.as_mut().expect("just created");
        for row in std::mem::take(&mut self.mem).iter().map(Vec::as_slice).chain([raw]) {
            spill.file.write_all(&(row.len() as u64).to_le_bytes()).and_then(|_| spill.file.write_all(row)).map_err(spill_error)?;
        }
        self.bytes = 0;
        Ok(())
    }

    /// Every row, in order.
    fn drain(&mut self, mut f: impl FnMut(&[u8]) -> Result<()>) -> Result<()> {
        use std::io::{Read as _, Seek as _};
        for row in std::mem::take(&mut self.mem) {
            f(&row)?;
        }
        let Some(spill) = self.spill.as_mut() else { return Ok(()) };
        std::io::Write::flush(&mut spill.file).map_err(spill_error)?;
        let mut file = std::io::BufReader::new(spill.file.get_ref());
        file.seek(std::io::SeekFrom::Start(0)).map_err(spill_error)?;
        let mut len = [0u8; 8];
        let mut row = Vec::new();
        loop {
            match file.read_exact(&mut len) {
                Ok(()) => {}
                Err(e) if e.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(()),
                Err(e) => return Err(spill_error(e)),
            }
            row.resize(u64::from_le_bytes(len) as usize, 0);
            file.read_exact(&mut row).map_err(spill_error)?;
            f(&row)?;
        }
    }
}

impl Spill {
    fn create() -> std::io::Result<Spill> {
        use std::sync::atomic::{AtomicU64, Ordering};
        static N: AtomicU64 = AtomicU64::new(0);
        let nanos = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).map(|d| d.as_nanos()).unwrap_or(0);
        let path = std::env::temp_dir().join(format!("dbine-drill-{}-{nanos}-{}.rows", std::process::id(), N.fetch_add(1, Ordering::Relaxed)));
        let mut open = std::fs::OpenOptions::new();
        open.read(true).write(true).create_new(true);
        #[cfg(unix)]
        std::os::unix::fs::OpenOptionsExt::mode(&mut open, 0o600);
        let file = open.open(&path)?;
        Ok(Spill { path, file: std::io::BufWriter::new(file) })
    }
}

/// `"key":value` → the key and the value's raw text.
fn pair(part: &str) -> Option<(String, &str)> {
    let part = part.trim();
    let rest = part.strip_prefix('"')?;
    let mut e = false;
    for (i, c) in rest.char_indices() {
        if e {
            e = false;
        } else if c == '\\' {
            e = true;
        } else if c == '"' {
            let key = serde_json::from_str::<String>(&part[..i + 2]).ok()?;
            return Some((key, rest[i + 1..].trim_start().trim_start_matches(':').trim()));
        }
    }
    None
}

/// A row object's values as raw JSON text, by key.
fn object_pairs<'a>(obj: &'a str) -> Vec<(String, &'a str)> {
    let inner = obj.trim().trim_start_matches('{').trim_end_matches('}');
    let mut out = Vec::new();
    let (mut depth, mut in_str, mut esc, mut start) = (0i32, false, false, 0usize);
    let mut push = |part: &'a str| {
        if let Some(p) = pair(part) {
            out.push(p);
        }
    };
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
                push(&inner[start..i]);
                start = i + 1;
            }
            _ => {}
        }
    }
    if !inner[start..].trim().is_empty() {
        push(&inner[start..]);
    }
    out
}

fn row_cells(raw: &[u8], columns: &[(String, String)]) -> Result<Vec<Cell>> {
    let s = std::str::from_utf8(raw).map_err(Error::query)?;
    let pairs = object_pairs(s);
    Ok(columns
        .iter()
        .enumerate()
        .map(|(i, (name, ty))| {
            // By position first (the usual case), else by name.
            let v = match pairs.get(i) {
                Some((k, v)) if k == name => Some(*v),
                _ => pairs.iter().find(|(k, _)| k == name).map(|(_, v)| *v),
            };
            v.map_or(Cell::Null, |v| to_cell(v, ty))
        })
        .collect())
}

fn unquote(raw: &str) -> String {
    serde_json::from_str::<String>(raw).unwrap_or_else(|_| raw.to_string())
}

/// `1E+2` → `100` (Java's `BigDecimal` text may carry an exponent).
fn plain_decimal(s: &str) -> String {
    let Some(e) = s.find(['E', 'e']) else { return s.to_string() };
    let (mant, exp) = (&s[..e], s[e + 1..].parse::<i64>().unwrap_or(0));
    let (neg, mant) = match mant.strip_prefix('-') {
        Some(m) => (true, m),
        None => (false, mant),
    };
    let (int, frac) = mant.split_once('.').unwrap_or((mant, ""));
    let digits = format!("{int}{frac}");
    let point = int.len() as i64 + exp;
    let out = if point <= 0 {
        format!("0.{}{digits}", "0".repeat((-point) as usize))
    } else if point as usize >= digits.len() {
        format!("{digits}{}", "0".repeat(point as usize - digits.len()))
    } else {
        format!("{}.{}", &digits[..point as usize], &digits[point as usize..])
    };
    let t = out.trim_start_matches('0');
    let out = if t.is_empty() || t.starts_with('.') { format!("0{t}") } else { t.to_string() };
    if neg {
        format!("-{out}")
    } else {
        out
    }
}

/// One raw JSON value as a cell of Drill type `ty`.
pub(crate) fn to_cell(raw: &str, ty: &str) -> Cell {
    if raw == "null" || raw.is_empty() {
        return Cell::Null;
    }
    let base = ty.split('(').next().unwrap_or(ty).trim();
    if raw.starts_with('[') {
        // A repeated (list) column: Drill's metadata names the element type.
        return Cell::Json(list_json(raw, ty));
    }
    let ms = || raw.parse::<i64>().ok();
    match base {
        "BIT" | "BOOLEAN" => match raw {
            "true" => Cell::Bool(true),
            "false" => Cell::Bool(false),
            _ => Cell::Text(unquote(raw)),
        },
        "TINYINT" | "SMALLINT" | "INT" | "INTEGER" | "BIGINT" | "UINT1" | "UINT2" | "UINT4" => raw.parse().map(Cell::Int).unwrap_or_else(|_| Cell::Text(unquote(raw))),
        "UINT8" => raw.parse().map(Cell::UInt).unwrap_or_else(|_| Cell::Text(unquote(raw))),
        // NaN and ±Infinity come quoted (`"NaN"`, `"-Infinity"`).
        "FLOAT4" | "FLOAT" => {
            let s = unquote(raw);
            s.parse::<f32>().map(|f| Cell::Float(f.to_string().parse().unwrap_or(f64::from(f)))).unwrap_or(Cell::Text(s))
        }
        "FLOAT8" | "DOUBLE" => {
            let s = unquote(raw);
            s.parse().map(Cell::Float).unwrap_or(Cell::Text(s))
        }
        "VARDECIMAL" | "DECIMAL" | "DECIMAL9" | "DECIMAL18" | "DECIMAL28SPARSE" | "DECIMAL38SPARSE" => Cell::Decimal(plain_decimal(raw.trim_matches('"'))),
        "TIMESTAMP" => match ms().and_then(chrono::DateTime::from_timestamp_millis) {
            Some(d) if d.timestamp_subsec_millis() == 0 => Cell::DateTime(d.format("%Y-%m-%d %H:%M:%S").to_string()),
            Some(d) => Cell::DateTime(d.format("%Y-%m-%d %H:%M:%S%.3f").to_string()),
            None => Cell::DateTime(unquote(raw)),
        },
        "DATE" => match ms().and_then(chrono::DateTime::from_timestamp_millis) {
            Some(d) => Cell::Date(d.format("%Y-%m-%d").to_string()),
            None => Cell::Date(unquote(raw)),
        },
        "TIME" => match ms() {
            Some(ms) => {
                let base = format!("{:02}:{:02}:{:02}", ms / 3_600_000, ms / 60_000 % 60, ms / 1000 % 60);
                Cell::Time(if ms % 1000 == 0 { base } else { format!("{base}.{:03}", ms % 1000) })
            }
            None => Cell::Time(unquote(raw)),
        },
        "VARBINARY" | "BINARY" => {
            let s = unquote(raw);
            base64::engine::general_purpose::STANDARD.decode(&s).map(Cell::Bytes).unwrap_or(Cell::Text(s))
        }
        _ if raw.starts_with('{') || raw.starts_with('[') => Cell::Json(raw.to_string()),
        _ if raw.starts_with('"') => Cell::Text(unquote(raw)),
        _ => raw.parse::<i64>().map(Cell::Int).unwrap_or_else(|_| Cell::Text(raw.to_string())),
    }
}

/// A list's elements as raw JSON text (`raw` starts with `[`).
fn array_items(raw: &str) -> Vec<&str> {
    let inner = raw.trim().strip_prefix('[').and_then(|r| r.strip_suffix(']')).unwrap_or("");
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
    if !inner[start..].trim().is_empty() {
        out.push(inner[start..].trim());
    }
    out
}

/// A repeated column's value as JSON whose elements are typed like a
/// scalar of the element type `ty`: numbers, decimals (every digit) and
/// booleans as Drill wrote them; NaN/±Infinity, temporal values (as text,
/// not epoch milliseconds) and binaries (base64) as strings; nested lists
/// and maps recursively / as they come.
fn list_json(raw: &str, ty: &str) -> String {
    let items: Vec<String> = array_items(raw)
        .into_iter()
        .map(|item| match to_cell(item, ty) {
            Cell::Null => "null".to_string(),
            Cell::Bool(b) => b.to_string(),
            Cell::Int(i) => i.to_string(),
            Cell::UInt(u) => u.to_string(),
            Cell::Float(f) if f.is_finite() => item.to_string(),
            Cell::Decimal(d) => d,
            Cell::Json(j) => j,
            // Binaries stay as Drill's base64 text.
            Cell::Bytes(_) => item.to_string(),
            Cell::Float(f) => Value::String(if f.is_nan() { "NaN".into() } else if f > 0.0 { "Infinity".into() } else { "-Infinity".into() }).to_string(),
            Cell::Text(s) | Cell::Date(s) | Cell::Time(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Uuid(s) => Value::String(s).to_string(),
        })
        .collect();
    format!("[{}]", items.join(","))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::transfer::{BatchSink, RowBatch};
    use std::sync::{Arc, Mutex};

    #[derive(Default)]
    struct Collect {
        columns: Vec<TransferColumn>,
        rows: Vec<Vec<Cell>>,
    }

    impl BatchSink for Collect {
        fn begin(&mut self, columns: &[TransferColumn]) -> std::io::Result<()> {
            self.columns = columns.to_vec();
            Ok(())
        }
        fn batch(&mut self, b: RowBatch) -> std::io::Result<()> {
            self.rows.extend(b.rows);
            Ok(())
        }
    }

    const ANSWER: &str = r#"{"queryId":"15444160"
,"columns":["id","name","d","ts","dt","tm","b","ok","m","n"]
,"metadata":["BIGINT","VARCHAR","VARDECIMAL(38, 4)","TIMESTAMP","DATE","TIME","VARBINARY","BIT","MAP","VARCHAR(65535)"]
,"attemptedAutoLimit":0
,"rows":[
{"id":1,"name":"Sheri {\"x\"}, ]","d":12345678901234567890.1234,"ts":1706708700123,"dt":19753,"tm":49507123,"b":"yv4=","ok":true,"m":{"a":[1,2]},"n":null}
,{"id":2,"name":"b","d":0E-4,"ts":1706708700000,"dt":1706659200000,"tm":49507000,"b":"","ok":false,"m":{},"n":"x"}
]
,"queryState":"COMPLETED"
}"#;

    fn scan(answer: &str, chunk: usize) -> Result<(u64, Collect)> {
        scan_with(answer, chunk, SPILL_BYTES)
    }

    fn scan_with(answer: &str, chunk: usize, spill: usize) -> Result<(u64, Collect)> {
        let sink = Arc::new(Mutex::new(Collect::default()));
        let mut reader = RowReader::new(sink.clone());
        reader.pending = Pending::new(spill);
        let mut s = Scanner::default();
        for c in answer.as_bytes().chunks(chunk) {
            s.feed(c, &mut reader)?;
        }
        let n = s.finish(&mut reader)?;
        let got = std::mem::take(&mut *sink.lock().unwrap());
        Ok((n, got))
    }

    #[test]
    fn streamed_answer_becomes_typed_rows() {
        for chunk in [1, 7, 4096] {
            let (n, got) = scan(ANSWER, chunk).unwrap();
            assert_eq!(n, 2);
            assert_eq!(got.columns[2].type_name, "VARDECIMAL(38, 4)");
            assert_eq!(
                got.rows[0],
                vec![
                    Cell::Int(1),
                    Cell::Text("Sheri {\"x\"}, ]".into()),
                    Cell::Decimal("12345678901234567890.1234".into()),
                    Cell::DateTime("2024-01-31 13:45:00.123".into()),
                    Cell::Date("1970-01-01".into()),
                    Cell::Time("13:45:07.123".into()),
                    Cell::Bytes(vec![0xca, 0xfe]),
                    Cell::Bool(true),
                    Cell::Json("{\"a\":[1,2]}".into()),
                    Cell::Null,
                ]
            );
            assert_eq!(got.rows[1][2], Cell::Decimal("0.0000".into()));
            assert_eq!(got.rows[1][3], Cell::DateTime("2024-01-31 13:45:00".into()));
            assert_eq!(got.rows[1][4], Cell::Date("2024-01-31".into()));
            assert_eq!(got.rows[1][6], Cell::Bytes(vec![]));
        }
    }

    #[test]
    fn errors_and_old_layouts() {
        let failed = r#"{"queryId":"1","columns":["a"],"metadata":["INT"],"rows":[{"a":1}
],"queryState":"FAILED","errorMessage":"boom"}"#;
        match scan(failed, 5) {
            Err(Error::Query(m)) => assert_eq!(m, "boom"),
            r => panic!("{:?}", r.map(|x| x.0)),
        }
        match scan(r#"{"errorMessage":"PARSE ERROR"}"#, 3) {
            Err(Error::Query(m)) => assert_eq!(m, "PARSE ERROR"),
            r => panic!("{:?}", r.map(|x| x.0)),
        }
        // Metadata after the rows (older servers): the rows wait for it.
        let old = r#"{"queryId":"1","columns":["a","d"],"rows":[{"a":"1","d":1.50}],"metadata":["INT","VARDECIMAL(3, 2)"],"queryState":"COMPLETED"}"#;
        let (n, got) = scan(old, 4).unwrap();
        assert_eq!(n, 1);
        assert_eq!(got.rows[0], vec![Cell::Text("1".into()), Cell::Decimal("1.50".into())]);
        // No rows.
        let (n, got) = scan(r#"{"queryId":"1","columns":["a"],"metadata":["INT"],"rows":[],"queryState":"COMPLETED"}"#, 2).unwrap();
        assert_eq!((n, got.columns.len()), (0, 1));
    }

    #[test]
    fn unknown_columns_are_reported() {
        let req = vec!["ID".to_string(), "nope".to_string(), "name".to_string()];
        assert_eq!(unknown_columns(&req, ["id", "name"].into_iter()), ["nope"]);
        assert!(unknown_columns(&req[..1], ["id"].into_iter()).is_empty());
    }

    #[test]
    fn repeated_columns_are_json() {
        // Drill's metadata gives the element type of a repeated column.
        assert_eq!(to_cell("[true,false]", "BIT"), Cell::Json("[true,false]".into()));
        assert_eq!(to_cell("[true,true]", "BIT"), Cell::Json("[true,true]".into()));
        assert_eq!(to_cell("[1,3]", "BIGINT"), Cell::Json("[1,3]".into()));
        assert_eq!(to_cell("[]", "BIGINT"), Cell::Json("[]".into()));
        assert_eq!(to_cell("[1.5,2.25]", "FLOAT8"), Cell::Json("[1.5,2.25]".into()));
        assert_eq!(to_cell(r#"[1.5,"NaN","-Infinity"]"#, "FLOAT8"), Cell::Json(r#"[1.5,"NaN","-Infinity"]"#.into()));
        assert_eq!(
            to_cell("[12345678901234567890.1234,1E+2,null]", "VARDECIMAL(38, 4)"),
            Cell::Json("[12345678901234567890.1234,100,null]".into())
        );
        assert_eq!(to_cell("[1706708700123]", "TIMESTAMP"), Cell::Json(r#"["2024-01-31 13:45:00.123"]"#.into()));
        assert_eq!(to_cell("[1706659200000]", "DATE"), Cell::Json(r#"["2024-01-31"]"#.into()));
        assert_eq!(to_cell("[49507123]", "TIME"), Cell::Json(r#"["13:45:07.123"]"#.into()));
        assert_eq!(to_cell(r#"["yv4="]"#, "VARBINARY"), Cell::Json(r#"["yv4="]"#.into()));
        assert_eq!(to_cell("[[1,2],[3]]", "BIGINT"), Cell::Json("[[1,2],[3]]".into()));
        assert_eq!(to_cell(r#"["a,]","b"]"#, "VARCHAR"), Cell::Json(r#"["a,]","b"]"#.into()));
        // A list in a real answer (captured from Drill 1.22).
        let answer = r#"{"queryId":"1","columns":["ok","n"],"metadata":["BIT","BIGINT"],"rows":[{"ok":[true,false],"n":[1,3]}],"queryState":"COMPLETED"}"#;
        let (_, got) = scan(answer, 3).unwrap();
        assert_eq!(got.rows[0], vec![Cell::Json("[true,false]".into()), Cell::Json("[1,3]".into())]);
    }

    #[test]
    fn nan_and_infinity_are_floats() {
        let answer = r#"{"queryId":"1","columns":["nan","i","i4"],"metadata":["FLOAT8","FLOAT8","FLOAT4"],"rows":[{"nan":"NaN","i":"Infinity","i4":"-Infinity"}],"queryState":"COMPLETED"}"#;
        let (_, got) = scan(answer, 5).unwrap();
        assert!(matches!(got.rows[0][0], Cell::Float(f) if f.is_nan()), "{:?}", got.rows[0][0]);
        assert_eq!(got.rows[0][1], Cell::Float(f64::INFINITY));
        assert_eq!(got.rows[0][2], Cell::Float(f64::NEG_INFINITY));
        assert!(matches!(to_cell(r#""NaN""#, "FLOAT4"), Cell::Float(f) if f.is_nan()));
        assert_eq!(to_cell(r#""Infinity""#, "FLOAT4"), Cell::Float(f64::INFINITY));
    }

    #[test]
    fn late_metadata_spills_to_disk() {
        // Types after the rows: past the bound the rows wait in a file, in order.
        let rows: Vec<String> = (0..500).map(|i| format!(r#"{{"a":{i},"d":{i}.50}}"#)).collect();
        let answer = format!(r#"{{"queryId":"1","columns":["a","d"],"rows":[{}],"metadata":["BIGINT","VARDECIMAL(9, 2)"],"queryState":"COMPLETED"}}"#, rows.join(","));
        for spill in [0, 100, 1 << 20] {
            let (n, got) = scan_with(&answer, 13, spill).unwrap();
            assert_eq!(n, 500);
            for (i, r) in got.rows.iter().enumerate() {
                assert_eq!(r, &vec![Cell::Int(i as i64), Cell::Decimal(format!("{i}.50"))]);
            }
        }
        let mut p = Pending::new(4);
        p.push(b"12345").unwrap();
        let path = p.spill.as_ref().unwrap().path.clone();
        assert!(path.exists() && p.mem.is_empty());
        drop(p);
        assert!(!path.exists(), "the temporary file is removed");
    }
}
