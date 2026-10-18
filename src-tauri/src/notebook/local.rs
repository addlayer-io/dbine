//! "Resultados del notebook": a cell that queries the results of the cells
//! above it as tables, with DuckDB (docs/notebooks.md › Notebook results).
//!
//! - Every SQL cell has a **name** ([`valid_name`]); a cell whose target is
//!   the notebook's results runs DuckDB SQL in which those names are tables.
//! - Each name the SQL mentions ([`references`]: words and quoted
//!   identifiers outside strings and comments) is **materialized** into an
//!   in-memory DuckDB database owned by the notebook tab (session key
//!   `nbx:<tab>`, closed with the tab): from the rows of the source cell's
//!   last run in this session when that run was complete, else by running
//!   the source's query again on its own connection through the read path
//!   the whole-result charts use (`chart::read`: a read-only session, the
//!   server's read-only mode where it has one, else the read-only guard; a
//!   statement the guard sees as a write is never run again). A source that
//!   is itself a "results" cell becomes `CREATE TABLE … AS <its query>`.
//!   Rows are capped per table ([`DEFAULT_ROW_CAP`], Settings › Notebooks)
//!   and the cap is said. A table is loaded again only when its source ran
//!   again or its text changed ([`source_key`]).
//! - Values go in through the driver's bulk load (DuckDB's appender), never
//!   through SQL that reads files, and each column gets a DuckDB type from
//!   the source's type name and its values ([`infer`], VARCHAR / JSON as
//!   the fallback).
//! - **Sandbox**: the database is opened in memory and, before anything is
//!   loaded, external access is switched off (no files, no network, no
//!   extensions, no spilling to a temp directory) and the configuration is
//!   locked, then checked ([`SANDBOX`]). A foreign `.ipynb` can't write
//!   local files or reach the network (`COPY … TO`, `ATTACH`, `INSTALL`) on
//!   the user's behalf, and reads only the files its reader calls name
//!   (below). No credentials or connection details ever reach it.
//! - **Local data files** (`read_csv('~/x.csv')`…) are read in a session of
//!   their own, granted exactly those files, and come in as tables like the
//!   cells' ([`super::files`]); the notebook's session is never granted a
//!   file. Statements that write, attach or configure anything are refused
//!   before running ([`super::files::refused`]).

use super::files::{self, DataFile, FileRead};
use crate::commands::chart::{read, Fail, Stop};
use crate::commands::projects::sha256_hex;
use crate::error::{CommandError, CommandResult};
use crate::state::{AppState, SessionEntry};
use dbine_driver::sql::{name_tokens, quote_ident, Quote, ScriptDialect, TokenKind};
use dbine_driver::transfer::{BatchSource, Cell, LoadSpec, RowBatch, TransferColumn};
use dbine_driver::{ConnectionConfig, ObjectRef, QueryOutcome, ResultColumn, RowSink, RowSinkRef, ScriptError, Session};
use serde::{Deserialize, Serialize};
use serde_json::Value;
use std::collections::{HashMap, HashSet};
use std::sync::atomic::Ordering;
use std::sync::{Arc, LazyLock, Mutex, Weak};
use tokio::sync::Notify;

/// Rows a materialized table keeps at most (Settings › Notebooks changes it).
pub const DEFAULT_ROW_CAP: u64 = 1_000_000;
/// What Settings › Notebooks accepts for that cap.
pub const ROW_CAP_RANGE: (u64, u64) = (1_000, 10_000_000);
/// Longest cell name.
const MAX_NAME: usize = 63;
/// Rows per batch handed to the bulk load.
const BATCH_ROWS: usize = 1_000;

/// Run once on the notebook's DuckDB session, before anything is loaded:
/// no files, network or extensions, nothing spilled to disk, and nothing a
/// cell's SQL can switch back on (`lock_configuration`).
pub const SANDBOX: &str = "SET autoinstall_known_extensions = false; \
SET autoload_known_extensions = false; \
SET allow_community_extensions = false; \
SET temp_directory = ''; \
SET enable_external_access = false; \
SET lock_configuration = true;";

/// The DuckDB dialect (PostgreSQL's parser).
fn dialect() -> ScriptDialect {
    ScriptDialect::postgres()
}

// ---------------------------------------------------------------------------
// Names

/// Words a cell can't be named: DuckDB would read them as SQL.
const RESERVED: &[&str] = &[
    "all", "and", "any", "array", "as", "asc", "between", "both", "by", "case", "cast", "check", "collate", "column", "constraint", "create",
    "cross", "current_date", "current_time", "current_timestamp", "current_user", "default", "delete", "desc", "distinct", "do", "drop",
    "else", "end", "except", "exists", "false", "fetch", "for", "foreign", "from", "full", "grant", "group", "having", "in", "inner",
    "insert", "intersect", "into", "is", "join", "lateral", "left", "like", "limit", "natural", "not", "null", "offset", "on", "only", "or",
    "order", "outer", "pivot", "primary", "qualify", "references", "returning", "right", "select", "set", "some", "table", "then", "to",
    "true", "union", "unique", "unpivot", "update", "user", "using", "values", "when", "where", "window", "with",
];

/// A cell name usable as a table: a letter or `_`, then letters, digits or
/// `_` (ASCII), at most 63, and not an SQL keyword.
pub fn valid_name(name: &str) -> bool {
    let first_ok = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_');
    first_ok
        && name.len() <= MAX_NAME
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        && !RESERVED.contains(&name.to_ascii_lowercase().as_str())
}

/// The names in `known` that `sql` mentions as tables, in the order they
/// first appear (each once, spelled as in `known`). A name is a word or a
/// quoted identifier outside strings and comments, and not right after a
/// `.` (`t.c1` is a column). DuckDB's identifiers are case-insensitive.
pub fn references(sql: &str, known: &[String]) -> Vec<String> {
    let by_lower: HashMap<String, &String> = known.iter().map(|n| (n.to_ascii_lowercase(), n)).collect();
    let toks = name_tokens(sql, &dialect());
    let mut seen = HashSet::new();
    let mut out = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.kind != TokenKind::Name {
            continue;
        }
        if i > 0 && toks[i - 1].kind == TokenKind::Punct && toks[i - 1].text == "." {
            continue;
        }
        if let Some(n) = by_lower.get(&t.text.to_ascii_lowercase()) {
            if seen.insert(n.to_ascii_lowercase()) {
                out.push((*n).clone());
            }
        }
    }
    out
}

// ---------------------------------------------------------------------------
// Types

/// The DuckDB type a materialized column gets.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum DuckType {
    Boolean,
    BigInt,
    /// Integers past `BIGINT` (ClickHouse `UInt64`, `numeric(38,0)` keys…).
    HugeInt,
    Double,
    Decimal(u8, u8),
    Date,
    Time,
    Timestamp,
    TimestampTz,
    Uuid,
    Json,
    Varchar,
}

impl DuckType {
    pub fn sql(&self) -> String {
        match self {
            DuckType::Boolean => "BOOLEAN".into(),
            DuckType::BigInt => "BIGINT".into(),
            DuckType::HugeInt => "HUGEINT".into(),
            DuckType::Double => "DOUBLE".into(),
            DuckType::Decimal(p, s) => format!("DECIMAL({p},{s})"),
            DuckType::Date => "DATE".into(),
            DuckType::Time => "TIME".into(),
            DuckType::Timestamp => "TIMESTAMP".into(),
            DuckType::TimestampTz => "TIMESTAMPTZ".into(),
            DuckType::Uuid => "UUID".into(),
            DuckType::Json => "JSON".into(),
            DuckType::Varchar => "VARCHAR".into(),
        }
    }
}

/// What a source's type name suggests. `None`: the source didn't say.
fn hint(type_name: &str) -> Option<DuckType> {
    let lower = type_name.trim().to_ascii_lowercase();
    if lower.is_empty() {
        return None;
    }
    // ClickHouse wraps: Nullable(Int32), LowCardinality(String).
    let mut t = lower.as_str();
    for w in ["nullable(", "lowcardinality(", "nullable("] {
        if let Some(inner) = t.strip_prefix(w) {
            t = inner.strip_suffix(')').unwrap_or(inner);
        }
    }
    let base = t.split('(').next().unwrap_or("").trim();
    let unsigned = base.contains("unsigned");
    let b = base.replace(" unsigned", "");
    let args: Vec<u32> = t
        .split_once('(')
        .and_then(|(_, r)| r.split(')').next())
        .map(|a| a.split(',').filter_map(|x| x.trim().parse().ok()).collect())
        .unwrap_or_default();
    Some(match b.as_str() {
        "bool" | "boolean" | "bit" => DuckType::Boolean,
        "tinyint" | "smallint" | "mediumint" | "int" | "integer" | "int2" | "int4" | "int8" | "smallserial" | "serial" | "bigserial" | "long"
        | "short" | "int16" | "int32" | "uint8" | "uint16" | "uint32" | "utinyint" | "usmallint" | "uinteger" => DuckType::BigInt,
        "bigint" | "int64" if !unsigned => DuckType::BigInt,
        "bigint" | "uint64" | "int128" | "uint128" | "int256" | "uint256" | "hugeint" | "ubigint" | "uhugeint" => DuckType::HugeInt,
        "real" | "float" | "float4" | "float8" | "double" | "double precision" | "float32" | "float64" | "binary_float" | "binary_double" => {
            DuckType::Double
        }
        "money" => DuckType::Decimal(19, 4),
        "smallmoney" => DuckType::Decimal(10, 4),
        "decimal" | "numeric" | "number" | "dec" | "decimal32" | "decimal64" | "decimal128" => match args.as_slice() {
            [p] if (1..=38).contains(p) => DuckType::Decimal(*p as u8, 0),
            [p, s] if (1..=38).contains(p) && s <= p => DuckType::Decimal(*p as u8, *s as u8),
            // No precision given: the values say it (see `infer`).
            _ => DuckType::Decimal(0, 0),
        },
        "date" | "date32" => DuckType::Date,
        "time" | "time without time zone" => DuckType::Time,
        "timestamp" | "datetime" | "datetime2" | "smalldatetime" | "timestamp without time zone" | "datetime64" => DuckType::Timestamp,
        "timestamptz" | "timestamp with time zone" | "timestamp with local time zone" | "datetimeoffset" => DuckType::TimestampTz,
        "uuid" | "uniqueidentifier" | "guid" => DuckType::Uuid,
        "json" | "jsonb" | "document" | "object" | "array" | "map" | "struct" | "variant" | "list" => DuckType::Json,
        _ => DuckType::Varchar,
    })
}

fn digits(s: &str, from: usize, n: usize) -> bool {
    s.as_bytes().get(from..from + n).is_some_and(|b| b.iter().all(u8::is_ascii_digit))
}
fn at(s: &str, i: usize, c: u8) -> bool {
    s.as_bytes().get(i) == Some(&c)
}
fn looks_date(s: &str) -> bool {
    s.len() == 10 && digits(s, 0, 4) && at(s, 4, b'-') && digits(s, 5, 2) && at(s, 7, b'-') && digits(s, 8, 2)
}
fn looks_time(s: &str) -> bool {
    digits(s, 0, 2) && at(s, 2, b':') && digits(s, 3, 2)
}
fn looks_timestamp(s: &str) -> bool {
    s.len() >= 10
        && s.is_char_boundary(10)
        && looks_date(&s[..10])
        && (s.len() == 10 || ((at(s, 10, b' ') || at(s, 10, b'T')) && s.is_char_boundary(11) && looks_time(&s[11..])))
}
fn looks_uuid(s: &str) -> bool {
    s.len() == 36 && s.char_indices().all(|(i, c)| if [8, 13, 18, 23].contains(&i) { c == '-' } else { c.is_ascii_hexdigit() })
}
fn as_bool(v: &Value) -> Option<bool> {
    match v {
        Value::Bool(b) => Some(*b),
        Value::Number(n) => match n.as_i64() {
            Some(0) => Some(false),
            Some(1) => Some(true),
            _ => None,
        },
        Value::String(s) => match s.trim().to_ascii_lowercase().as_str() {
            "true" | "t" | "1" => Some(true),
            "false" | "f" | "0" => Some(false),
            _ => None,
        },
        _ => None,
    }
}
fn as_i64(v: &Value) -> Option<i64> {
    match v {
        Value::Number(n) => n.as_i64(),
        Value::String(s) => s.trim().parse().ok(),
        _ => None,
    }
}
/// An integer written in full, of any size up to 38 digits.
fn is_integer(v: &Value) -> bool {
    match v {
        Value::Number(n) => n.as_i64().is_some() || n.as_u64().is_some(),
        Value::String(s) => {
            let s = s.trim();
            let s = s.strip_prefix('-').unwrap_or(s);
            !s.is_empty() && s.len() <= 38 && s.bytes().all(|b| b.is_ascii_digit())
        }
        _ => false,
    }
}
fn is_number(v: &Value) -> bool {
    match v {
        Value::Number(_) => true,
        Value::String(s) => s.trim().parse::<f64>().is_ok_and(f64::is_finite),
        _ => false,
    }
}
fn is_json(v: &Value) -> bool {
    match v {
        Value::String(s) => serde_json::from_str::<Value>(s).is_ok(),
        _ => true,
    }
}

/// Whether every value fits `ty` (nulls always do).
fn fits<'a>(ty: DuckType, mut values: impl Iterator<Item = &'a Value>) -> bool {
    values.all(|v| {
        v.is_null()
            || match ty {
                DuckType::Boolean => as_bool(v).is_some(),
                DuckType::BigInt => as_i64(v).is_some(),
                DuckType::HugeInt => is_integer(v),
                DuckType::Double | DuckType::Decimal(..) => is_number(v),
                DuckType::Date => v.as_str().is_some_and(looks_date),
                DuckType::Time => v.as_str().is_some_and(looks_time),
                DuckType::Timestamp | DuckType::TimestampTz => v.as_str().is_some_and(looks_timestamp),
                DuckType::Uuid => v.as_str().is_some_and(looks_uuid),
                DuckType::Json => is_json(v),
                DuckType::Varchar => true,
            }
    })
}

/// Digits before and after the point of a plain decimal number (`-12.50`:
/// 2 and 2), written in full: no exponent, no leading zeros (`007` is a
/// code, not a number).
fn decimal_shape(v: &Value) -> Option<(usize, usize)> {
    let text;
    let s = match v {
        Value::Number(n) if n.is_i64() || n.is_u64() => {
            text = n.to_string();
            text.as_str()
        }
        Value::String(s) => s.trim(),
        _ => return None,
    };
    let s = s.strip_prefix('-').unwrap_or(s);
    let (int, frac) = s.split_once('.').unwrap_or((s, ""));
    let digits = |p: &str| p.bytes().all(|b| b.is_ascii_digit());
    let ok = !int.is_empty() && digits(int) && digits(frac) && !(int.len() > 1 && int.starts_with('0')) && (frac.is_empty() || !s.ends_with('.'));
    ok.then(|| (int.len(), frac.len()))
}

/// The narrowest exact type for numbers written as text: `BIGINT`,
/// `HUGEINT` or `DECIMAL(p,s)` (up to 38 digits), else `None`.
fn exact_numeric<'a>(values: impl Iterator<Item = &'a Value>) -> Option<DuckType> {
    let (mut int, mut frac, mut any) = (0, 0, false);
    let mut fits_i64 = true;
    for v in values.filter(|v| !v.is_null()) {
        let (i, f) = decimal_shape(v)?;
        int = int.max(i);
        frac = frac.max(f);
        any = true;
        fits_i64 &= f == 0 && as_i64(v).is_some();
    }
    match (any, frac, int + frac) {
        (false, _, _) => None,
        (_, 0, _) if fits_i64 => Some(DuckType::BigInt),
        (_, 0, p) if p <= 38 => Some(DuckType::HugeInt),
        (_, s, p) if p <= 38 => Some(DuckType::Decimal(p.max(1) as u8, s as u8)),
        _ => None,
    }
}

fn has_zone(s: &str) -> bool {
    let tail = s.get(19..).unwrap_or("");
    tail.ends_with('Z') || tail.contains('+') || tail.contains('-')
}

/// The column's DuckDB type: what its type name says when every value fits
/// it (an integer column with values past `BIGINT` widens to `HUGEINT`,
/// then to `DOUBLE`), else, when the source gave no type (PostgreSQL's
/// results, MongoDB, Redis…), what the values are: booleans, numbers
/// (numbers written as text, like PostgreSQL's `numeric`, exactly), dates,
/// timestamps, UUIDs, JSON documents; else `VARCHAR`.
pub fn infer<'a, I>(type_name: &str, values: I) -> DuckType
where
    I: Iterator<Item = &'a Value> + Clone,
{
    let all_null = values.clone().all(Value::is_null);
    match hint(type_name) {
        Some(DuckType::Varchar) => return DuckType::Varchar,
        Some(DuckType::Decimal(0, 0)) => return exact_numeric(values.clone()).or_else(|| fits(DuckType::Double, values.clone()).then_some(DuckType::Double)).unwrap_or(if all_null { DuckType::Double } else { DuckType::Varchar }),
        Some(t) if all_null || fits(t, values.clone()) => return t,
        Some(DuckType::BigInt) if fits(DuckType::HugeInt, values.clone()) => return DuckType::HugeInt,
        Some(DuckType::BigInt | DuckType::HugeInt | DuckType::Decimal(..)) if fits(DuckType::Double, values.clone()) => return DuckType::Double,
        Some(_) => return DuckType::Varchar,
        None if all_null => return DuckType::Varchar,
        None => {}
    }
    let only = |f: &dyn Fn(&Value) -> bool| values.clone().all(|v| v.is_null() || f(v));
    if only(&Value::is_boolean) {
        DuckType::Boolean
    } else if only(&|v| v.as_i64().is_some()) {
        DuckType::BigInt
    } else if only(&Value::is_number) {
        DuckType::Double
    } else if let Some(t) = exact_numeric(values.clone()) {
        t
    } else if only(&|v| v.as_str().is_some_and(looks_date)) {
        DuckType::Date
    } else if only(&|v| v.as_str().is_some_and(looks_timestamp)) {
        if values.clone().any(|v| v.as_str().is_some_and(has_zone)) { DuckType::TimestampTz } else { DuckType::Timestamp }
    } else if only(&|v| v.as_str().is_some_and(looks_uuid)) {
        DuckType::Uuid
    } else if only(&|v| v.is_object() || v.is_array()) {
        DuckType::Json
    } else {
        DuckType::Varchar
    }
}

fn text_of(v: &Value) -> String {
    match v {
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

/// A grid value as the cell the appender gets for a column of type `ty`
/// (the others go as text, which DuckDB casts exactly).
pub fn to_cell(v: &Value, ty: DuckType) -> Cell {
    if v.is_null() {
        return Cell::Null;
    }
    match ty {
        DuckType::Boolean => as_bool(v).map_or_else(|| Cell::Text(text_of(v)), Cell::Bool),
        DuckType::BigInt => as_i64(v).map_or_else(|| Cell::Text(text_of(v)), Cell::Int),
        DuckType::Double => match v {
            Value::Number(n) => Cell::Float(n.as_f64().unwrap_or(f64::NAN)),
            other => other.as_str().and_then(|s| s.trim().parse().ok()).map_or_else(|| Cell::Text(text_of(other)), Cell::Float),
        },
        _ => Cell::Text(text_of(v)),
    }
}

/// Column names DuckDB takes: blanks named `col<n>`, repeats (DuckDB
/// compares them ignoring case) get `_2`, `_3`…
pub fn column_names(cols: &[ResultColumn]) -> Vec<String> {
    let mut seen = HashSet::new();
    cols.iter()
        .enumerate()
        .map(|(i, c)| {
            let base = if c.name.trim().is_empty() { format!("col{}", i + 1) } else { c.name.clone() };
            let mut name = base.clone();
            let mut n = 2;
            while !seen.insert(name.to_lowercase()) {
                name = format!("{base}_{n}");
                n += 1;
            }
            name
        })
        .collect()
}

// ---------------------------------------------------------------------------
// Sources and the cache

/// A cell above, as the UI sends it (in the notebook's order).
#[derive(Debug, Clone, Deserialize)]
pub struct LocalSource {
    /// Its name (the table).
    pub name: String,
    /// The cell runs on the notebook's results too: its table is its query
    /// over the tables above it.
    #[serde(default)]
    pub local: bool,
    pub sql: String,
    /// Where it runs (not for `local`): a local connection's id.
    #[serde(default)]
    pub connection_id: Option<String>,
    #[serde(default)]
    pub database: Option<String>,
    /// Its last run in this session (the notebook's run counter), if any.
    #[serde(default)]
    pub run: Option<u64>,
    /// The rows of that run, when it was complete (not cut by the row
    /// limit) and ran exactly `sql`: its first result set.
    #[serde(default)]
    pub rows: Option<LiveRows>,
    /// The files its SQL reads, as they are now (a "results" source): part
    /// of its key, so it's built again when one of them changes.
    #[serde(skip)]
    pub files_key: String,
}

#[derive(Debug, Clone, Deserialize)]
pub struct LiveRows {
    pub columns: Vec<ResultColumn>,
    pub rows: Vec<Vec<Value>>,
}

/// What identifies a source's table: its name, text, where it runs, the
/// run its rows come from (or that its query runs again), the cap, and its
/// own sources' keys. A table is loaded again only when this changes.
pub fn source_key(src: &LocalSource, cap: u64, deps: &[String]) -> String {
    let origin = match (&src.rows, src.run) {
        (Some(_), Some(run)) => format!("run:{run}"),
        (Some(_), None) => "rows".to_string(),
        _ => "query".to_string(),
    };
    let key = serde_json::json!([src.name.to_ascii_lowercase(), src.local, src.sql.trim(), src.connection_id, src.database, origin, cap, deps, src.files_key]);
    sha256_hex(key.to_string().as_bytes())
}

/// The sources `sql` needs, in loading order (each one's sources before
/// it): the ones it names and, through "results" sources, the ones those
/// name. Each source only sees the ones above it (`sources` is in the
/// notebook's order), so there are no cycles.
pub fn needed(sql: &str, sources: &[LocalSource]) -> Vec<usize> {
    fn direct(sql: &str, above: &[LocalSource]) -> Vec<usize> {
        let names: Vec<String> = above.iter().map(|s| s.name.clone()).collect();
        references(sql, &names).into_iter().filter_map(|n| above.iter().rposition(|s| s.name.eq_ignore_ascii_case(&n))).collect()
    }
    fn visit(i: usize, sources: &[LocalSource], out: &mut Vec<usize>, seen: &mut HashSet<usize>) {
        if !seen.insert(i) {
            return;
        }
        if sources[i].local {
            for j in direct(&sources[i].sql, &sources[..i]) {
                visit(j, sources, out, seen);
            }
        }
        out.push(i);
    }
    let mut out = Vec::new();
    let mut seen = HashSet::new();
    for i in direct(sql, sources) {
        visit(i, sources, &mut out, &mut seen);
    }
    out
}

#[derive(Debug, Clone)]
pub(crate) struct Loaded {
    key: String,
    rows: u64,
    capped: bool,
    as_text: bool,
}

/// The tables loaded into one notebook's DuckDB session.
struct Catalog {
    entry: Weak<SessionEntry>,
    tables: HashMap<String, Loaded>,
}

static CATALOGS: LazyLock<Mutex<HashMap<String, Catalog>>> = LazyLock::new(Default::default);

/// How one table got there, for the cell.
#[derive(Debug, Clone, Serialize)]
pub struct LoadedTable {
    pub name: String,
    pub rows: u64,
    /// Cut at the cap.
    pub capped: bool,
    /// Already there from an earlier run (nothing loaded now).
    pub cached: bool,
    /// `run`: the rows of its run in this session; `query`: its query ran
    /// again; `local`: built from the tables above it; `file`: read from
    /// local files (`files`, `name` is the file or the paths as written).
    pub origin: &'static str,
    /// Some of its columns had values that aren't of their type: they're text.
    pub as_text: bool,
    pub elapsed_ms: u64,
    /// The files read (`origin: "file"`).
    #[serde(skip_serializing_if = "Vec::is_empty")]
    pub files: Vec<DataFile>,
}

#[derive(Debug, Clone, Serialize)]
pub struct RunLocalResponse {
    pub outcome: QueryOutcome,
    pub loaded: Vec<LoadedTable>,
    /// Nothing ran: the cell reads files nobody allowed yet, and the UI
    /// asks ([`super::access`]); it runs again with the request's token.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub approval: Option<super::access::ApprovalRequest>,
}

/// What [`prepare`] got to.
#[derive(Debug)]
pub(crate) enum Prepared {
    /// The cell's SQL with each reader call replaced by its table, and
    /// every table loaded.
    Ready(String, Vec<LoadedTable>),
    /// Files to ask about; nothing was read or loaded.
    Ask(Vec<DataFile>),
}

// ---------------------------------------------------------------------------
// The DuckDB session

/// The session key of a notebook tab's DuckDB database.
pub fn session_key(tab_id: &str) -> String {
    format!("nbx:{tab_id}")
}

/// A new in-memory DuckDB database of its own (the driver's `isolated`
/// option: apart from every other DuckDB database of the app), locked
/// down. The first use downloads DuckDB (or its driver) as opening a
/// DuckDB connection does.
pub async fn open_sandboxed() -> CommandResult<Box<dyn Session>> {
    let mut cfg = ConnectionConfig { driver: "duckdb".into(), host: ":memory:".into(), ..Default::default() };
    cfg.options.insert("isolated".into(), "true".into());
    let mut s = dbine_drivers::open_session(&cfg, None).await?;
    sandbox(s.as_mut()).await?;
    Ok(s)
}

/// [`SANDBOX`], then a check that it holds.
pub async fn sandbox(s: &mut dyn Session) -> CommandResult<()> {
    let fail = |e: String| CommandError::Internal(format!("no se pudo aislar la base de los resultados del notebook: {e}"));
    let mut out = QueryOutcome::default();
    s.execute(SANDBOX, 10, &mut out).await.map_err(|e| fail(e.to_string()))?;
    if let Some(e) = out.error {
        return Err(fail(e));
    }
    let mut out = QueryOutcome::default();
    s.execute("SELECT current_setting('enable_external_access')::VARCHAR, current_setting('lock_configuration')::VARCHAR", 10, &mut out)
        .await
        .map_err(|e| fail(e.to_string()))?;
    let row = out.results.iter().find(|r| !r.columns.is_empty()).and_then(|r| r.rows.first()).cloned().unwrap_or_default();
    let is = |i: usize, want: &str| row.get(i).map(text_of).is_some_and(|v| v.eq_ignore_ascii_case(want));
    if !(is(0, "false") && is(1, "true")) {
        return Err(fail(format!("{row:?}")));
    }
    Ok(())
}

/// The tab's DuckDB session, opened (and locked down) the first time.
async fn duck_entry(state: &AppState, key: &str) -> CommandResult<Arc<SessionEntry>> {
    if let Some(e) = state.sessions.get(key).map(|e| e.clone()) {
        return Ok(e);
    }
    let session = open_sandboxed().await?;
    let entry = Arc::new(SessionEntry {
        connection_id: String::new(),
        database: String::new(),
        interrupter: session.interrupter(),
        session: tokio::sync::Mutex::new(session),
        cancel: Notify::new(),
        cancelled: Default::default(),
        autocommit: std::sync::atomic::AtomicBool::new(true),
        switched_to: Default::default(),
    });
    state.sessions.insert(key.to_string(), entry.clone());
    Ok(entry)
}

async fn exec(s: &mut dyn Session, sql: &str) -> CommandResult<QueryOutcome> {
    let mut out = QueryOutcome::default();
    s.execute(sql, 1_000, &mut out).await?;
    match out.error.take() {
        Some(e) => Err(CommandError::Sql(e)),
        None => Ok(out),
    }
}

fn first_value(out: &QueryOutcome) -> Option<&Value> {
    out.results.iter().find(|r| !r.columns.is_empty()).and_then(|r| r.rows.first()).and_then(|r| r.first())
}

/// The tables the session has now (lowercase).
pub(crate) async fn existing_tables(s: &mut dyn Session) -> CommandResult<HashSet<String>> {
    let out = exec(s, "SELECT lower(table_name) FROM duckdb_tables() WHERE schema_name = 'main'").await?;
    Ok(out.results.iter().flat_map(|r| r.rows.iter()).filter_map(|r| r.first().and_then(Value::as_str).map(String::from)).collect())
}

/// Batches of the rows as cells of the columns' types.
struct Rows<'a> {
    rows: &'a [Vec<Value>],
    types: &'a [DuckType],
    at: usize,
}

#[dbine_driver::async_trait]
impl BatchSource for Rows<'_> {
    async fn next(&mut self) -> Option<RowBatch> {
        if self.at >= self.rows.len() {
            return None;
        }
        let end = (self.at + BATCH_ROWS).min(self.rows.len());
        let rows: Vec<Vec<Cell>> = self.rows[self.at..end]
            .iter()
            .map(|r| self.types.iter().enumerate().map(|(i, t)| to_cell(r.get(i).unwrap_or(&Value::Null), *t)).collect())
            .collect();
        self.at = end;
        let bytes = rows.iter().flatten().map(Cell::size).sum();
        Some(RowBatch { rows, bytes })
    }
}

/// `CREATE OR REPLACE TABLE` the source's table and append its rows.
async fn load_table(s: &mut dyn Session, name: &str, columns: &[ResultColumn], rows: &[Vec<Value>], types: &[DuckType]) -> CommandResult<u64> {
    let names = column_names(columns);
    let defs: Vec<String> = names.iter().zip(types).map(|(n, t)| format!("{} {}", quote_ident(Quote::Double, n), t.sql())).collect();
    exec(s, &format!("CREATE OR REPLACE TABLE main.{} ({})", quote_ident(Quote::Double, name), defs.join(", "))).await?;
    let spec = LoadSpec {
        table: ObjectRef { kind: "table".into(), schema: Some("main".into()), name: name.to_string() },
        columns: names.clone(),
        table_lock: false,
        keep_identity: false,
        commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
        commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
    };
    let cols: Vec<TransferColumn> = names.iter().zip(types).map(|(n, t)| TransferColumn { name: n.clone(), type_name: t.sql(), nullable: true }).collect();
    let mut src = Rows { rows, types, at: 0 };
    Ok(s.bulk_load(&spec, &cols, &mut src, &|_| {}).await?)
}

/// Load rows with their inferred types. If DuckDB refuses a value (a date
/// it can't read…), they're loaded as text and each column is cast to its
/// type where every value converts; the others stay text. Returns the rows
/// and the columns left as text.
async fn load_rows(s: &mut dyn Session, name: &str, columns: &[ResultColumn], rows: &[Vec<Value>]) -> CommandResult<(u64, Vec<String>)> {
    let types: Vec<DuckType> = (0..columns.len()).map(|i| infer(&columns[i].type_name, rows.iter().map(move |r| r.get(i).unwrap_or(&Value::Null)))).collect();
    let first = match load_table(s, name, columns, rows, &types).await {
        Ok(n) => return Ok((n, Vec::new())),
        Err(CommandError::Cancelled) => return Err(CommandError::Cancelled),
        Err(e) if types.iter().all(|t| *t == DuckType::Varchar) => return Err(e),
        Err(e) => e,
    };
    tracing::debug!(%first, "notebook results: «{name}» loaded as text, then cast");
    let stage = format!("{name}__dbine_text");
    let text = vec![DuckType::Varchar; columns.len()];
    let n = load_table(s, &stage, columns, rows, &text).await?;
    let names = column_names(columns);
    let q = |n: &str| quote_ident(Quote::Double, n);
    let mut select = Vec::new();
    let mut as_text = Vec::new();
    for (c, t) in names.iter().zip(&types) {
        let ok = *t == DuckType::Varchar || {
            let bad = exec(s, &format!("SELECT count(*) FROM main.{} WHERE {c} IS NOT NULL AND TRY_CAST({c} AS {ty}) IS NULL", q(&stage), c = q(c), ty = t.sql())).await?;
            first_value(&bad).and_then(Value::as_u64) == Some(0)
        };
        if ok {
            select.push(format!("CAST({c} AS {ty}) AS {c}", c = q(c), ty = t.sql()));
        } else {
            select.push(q(c));
            as_text.push(c.clone());
        }
    }
    exec(s, &format!("CREATE OR REPLACE TABLE main.{} AS SELECT {} FROM main.{}", q(name), select.join(", "), q(&stage))).await?;
    exec(s, &format!("DROP TABLE main.{}", q(&stage))).await?;
    Ok((n, as_text))
}

/// Collects the first result set of a run, up to `cap` rows; one more
/// marks it cut and wakes `on_cap`, which stops the statement.
struct Collect {
    cap: u64,
    index: Option<usize>,
    columns: Vec<ResultColumn>,
    rows: Vec<Vec<Value>>,
    capped: bool,
    on_cap: Arc<Notify>,
}

impl RowSink for Collect {
    fn begin(&mut self, index: usize, columns: &[ResultColumn]) -> std::io::Result<()> {
        if self.index.is_none() && !columns.is_empty() {
            self.index = Some(index);
            self.columns = columns.to_vec();
        }
        Ok(())
    }
    fn row(&mut self, index: usize, row: &[Value]) -> std::io::Result<()> {
        if self.index != Some(index) || self.capped {
            return Ok(());
        }
        if self.rows.len() as u64 >= self.cap {
            self.capped = true;
            self.on_cap.notify_one();
            return Ok(());
        }
        self.rows.push(row.to_vec());
        Ok(())
    }
}

pub(crate) type Fetched = (Vec<ResultColumn>, Vec<Vec<Value>>, bool);

/// The rows of `query` read on `s` (read-only, see the module docs): its
/// first result set, up to `cap` rows, and whether it was cut.
pub(crate) async fn read_source(driver: &dyn dbine_driver::Driver, s: &mut dyn Session, name: &str, query: &str, cap: u64, stop: &Stop<'_>) -> CommandResult<Fetched> {
    let on_cap = Arc::new(Notify::new());
    let sink = Arc::new(Mutex::new(Collect { cap, index: None, columns: Vec::new(), rows: Vec::new(), capped: false, on_cap: on_cap.clone() }));
    let max = usize::try_from(cap.saturating_add(1)).unwrap_or(usize::MAX);
    match read(driver, s, query, query, max, Some(RowSinkRef(sink.clone())), Some(&on_cap), stop).await {
        Ok(_) => {}
        Err(Fail::Cancelled) => return Err(CommandError::Cancelled),
        Err(Fail::Refused(_)) => {
            return Err(CommandError::BadRequest(format!(
                "la celda «{name}» no es solo una lectura: DBine no la vuelve a ejecutar por su cuenta. Ejecutala y volvé a ejecutar esta celda"
            )))
        }
        Err(Fail::Failed(e)) => return Err(CommandError::Sql(format!("la celda «{name}» falló al volver a ejecutarse: {e}"))),
    }
    let mut c = sink.lock().map_err(|_| CommandError::Internal("filas de la celda".into()))?;
    if c.index.is_none() {
        return Err(CommandError::BadRequest(format!("la celda «{name}» no devolvió un resultado con columnas")));
    }
    Ok((std::mem::take(&mut c.columns), std::mem::take(&mut c.rows), c.capped))
}

/// The source's query run again on its connection, on a read-only session
/// of its own.
async fn rerun(state: &AppState, tab: &str, src: &LocalSource, cap: u64, duck: &SessionEntry) -> CommandResult<Fetched> {
    let Some(conn) = src.connection_id.as_deref().filter(|c| !c.is_empty()) else {
        return Err(CommandError::BadRequest(format!("la celda «{}» no tiene dónde ejecutarse: elegí su conexión", src.name)));
    };
    let database = src.database.clone().unwrap_or_default();
    let driver = crate::commands::schema::driver_of(state, conn)?;
    let key = format!("nbx-src:{tab}:{}", src.name.to_ascii_lowercase());
    let entry = state.dedicated_session(&key, conn, &database, true).await?;
    let r = {
        let mut s = entry.session.lock().await;
        // A cancel of the notebook's run stops this read too.
        let stop = Stop { cancel: &duck.cancel, cancelled: &duck.cancelled, interrupter: entry.interrupter.as_ref() };
        read_source(driver.as_ref(), &mut **s, &src.name, &src.sql, cap, &stop).await
    };
    state.sessions.remove_if(&key, |_, e| Arc::ptr_eq(e, &entry));
    r
}

/// Materialize the sources `sql` needs into `s` (see the module docs),
/// warnings into `out`. `rerun(i)` runs source `i`'s query again (it has
/// no complete rows of this session).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn materialize<F, Fut>(
    s: &mut dyn Session,
    tables: &mut HashMap<String, Loaded>,
    present: &HashSet<String>,
    sql: &str,
    sources: &[LocalSource],
    cap: u64,
    out: &mut QueryOutcome,
    mut rerun: F,
) -> CommandResult<Vec<LoadedTable>>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = CommandResult<Fetched>>,
{
    let mut loaded = Vec::new();
    let mut keys: HashMap<usize, String> = HashMap::new();
    for i in needed(sql, sources) {
        let src = &sources[i];
        let deps: Vec<String> = if src.local { needed(&src.sql, &sources[..i]).iter().filter_map(|j| keys.get(j).cloned()).collect() } else { Vec::new() };
        let k = source_key(src, cap, &deps);
        keys.insert(i, k.clone());
        let lname = src.name.to_ascii_lowercase();
        let t0 = std::time::Instant::now();
        let origin = if src.local {
            "local"
        } else if src.rows.is_some() {
            "run"
        } else {
            "query"
        };
        if let Some(l) = tables.get(&lname).filter(|l| l.key == k && present.contains(&lname)) {
            loaded.push(LoadedTable { name: src.name.clone(), rows: l.rows, capped: l.capped, cached: true, origin, as_text: l.as_text, elapsed_ms: 0, files: Vec::new() });
            continue;
        }
        let (rows, capped, as_text) = if src.local {
            let stmts = dbine_driver::sql::split_script(&src.sql, &dialect());
            if stmts.len() != 1 {
                return Err(CommandError::BadRequest(format!("la celda «{}» tiene varias sentencias: dejá una sola consulta para usarla como tabla", src.name)));
            }
            let body = stmts[0].text.trim().trim_end_matches(';').trim();
            let table = quote_ident(Quote::Double, &src.name);
            exec(s, &format!("CREATE OR REPLACE TABLE main.{table} AS SELECT * FROM ({body}) LIMIT {}", cap + 1))
                .await
                .map_err(|e| CommandError::Sql(format!("la celda «{}»: {e}", src.name)))?;
            let n = exec(s, &format!("SELECT count(*) FROM main.{table}")).await?;
            let n = first_value(&n).and_then(Value::as_u64).unwrap_or(0);
            if n > cap {
                exec(s, &format!("DELETE FROM main.{table} WHERE rowid >= {cap}")).await?;
            }
            (n.min(cap), n > cap, false)
        } else {
            let (columns, rows, capped) = match &src.rows {
                Some(live) => {
                    let take = (cap as usize).min(live.rows.len());
                    (live.columns.clone(), live.rows[..take].to_vec(), live.rows.len() as u64 > cap)
                }
                None => rerun(i).await?,
            };
            if columns.is_empty() {
                return Err(CommandError::BadRequest(format!("la celda «{}» no devolvió un resultado con columnas", src.name)));
            }
            let (n, text) = load_rows(s, &src.name, &columns, &rows).await?;
            if !text.is_empty() {
                out.warning(format!("«{}»: sus columnas {} tienen valores que no son de su tipo y se cargaron como texto", src.name, text.join(", ")));
            }
            (n, capped, !text.is_empty())
        };
        if capped {
            out.warning(format!("«{}» se cargó con las primeras {cap} filas (el tope por tabla de Configuración › Notebooks)", src.name));
        }
        tables.insert(lname, Loaded { key: k, rows, capped, as_text });
        loaded.push(LoadedTable { name: src.name.clone(), rows, capped, cached: false, origin, as_text, elapsed_ms: t0.elapsed().as_millis() as u64, files: Vec::new() });
    }
    Ok(loaded)
}

/// The rows of a reader call, read on the reader session: up to `cap`,
/// and whether there were more. A cancel of the run interrupts it.
async fn read_file(reader: &mut dyn Session, r: &FileRead, cap: u64, cancel: Option<&Notify>) -> CommandResult<Fetched> {
    let sql = format!("SELECT * FROM {} LIMIT {}", r.text, cap.saturating_add(1));
    let max = usize::try_from(cap.saturating_add(1)).unwrap_or(usize::MAX);
    let interrupt = reader.interrupter();
    let mut out = QueryOutcome::default();
    let never = Notify::new();
    let finished = tokio::select! {
        r = reader.execute(&sql, max, &mut out) => Some(r),
        _ = cancel.unwrap_or(&never).notified() => None,
    };
    let err = match finished {
        None => {
            if let Some(i) = interrupt {
                i();
            }
            return Err(CommandError::Cancelled);
        }
        Some(Err(dbine_driver::Error::Cancelled)) => return Err(CommandError::Cancelled),
        Some(Err(e)) => Some(e.to_string()),
        Some(Ok(())) => out.error.take(),
    };
    if let Some(e) = err {
        return Err(CommandError::Sql(format!("«{}»: {e}", r.display)));
    }
    let Some(res) = out.results.into_iter().find(|r| !r.columns.is_empty()) else {
        return Err(CommandError::BadRequest(format!("«{}» no devolvió columnas", r.display)));
    };
    let mut rows = res.rows;
    let capped = rows.len() as u64 > cap;
    rows.truncate(cap as usize);
    Ok((res.columns, rows, capped))
}

/// Load the files `reads` read into `s` as tables (each one once; again
/// only when its key changed), reading them on a reader session granted
/// exactly `allowed` ([`files::open_reader`]), opened only if something
/// has to be read.
#[allow(clippy::too_many_arguments)]
pub(crate) async fn load_files(
    s: &mut dyn Session,
    tables: &mut HashMap<String, Loaded>,
    present: &HashSet<String>,
    reads: &[FileRead],
    allowed: &[DataFile],
    cap: u64,
    out: &mut QueryOutcome,
    cancel: Option<&Notify>,
) -> CommandResult<Vec<LoadedTable>> {
    let mut loaded = Vec::new();
    let mut seen = HashSet::new();
    let mut reader: Option<Box<dyn Session>> = None;
    for r in reads {
        if !seen.insert(r.table.clone()) {
            continue;
        }
        let t0 = std::time::Instant::now();
        if let Some(l) = tables.get(&r.table).filter(|l| l.key == r.key && present.contains(&r.table)) {
            loaded.push(LoadedTable { name: r.display.clone(), rows: l.rows, capped: l.capped, cached: true, origin: "file", as_text: l.as_text, elapsed_ms: 0, files: r.files.clone() });
            continue;
        }
        if reader.is_none() {
            let excel = reads.iter().any(|r| r.call.function == "read_xlsx");
            reader = Some(files::open_reader(allowed, excel).await?);
        }
        let (columns, rows, capped) = read_file(reader.as_deref_mut().expect("reader"), r, cap, cancel).await?;
        let (n, text) = load_rows(s, &r.table, &columns, &rows).await?;
        if !text.is_empty() {
            out.warning(format!("«{}»: sus columnas {} tienen valores que no son de su tipo y se cargaron como texto", r.display, text.join(", ")));
        }
        if capped {
            out.warning(format!("«{}» se cargó con las primeras {cap} filas (el tope por tabla de Configuración › Notebooks)", r.display));
        }
        tables.insert(r.table.clone(), Loaded { key: r.key.clone(), rows: n, capped, as_text: !text.is_empty() });
        loaded.push(LoadedTable {
            name: r.display.clone(),
            rows: n,
            capped,
            cached: false,
            origin: "file",
            as_text: !text.is_empty(),
            elapsed_ms: t0.elapsed().as_millis() as u64,
            files: r.files.clone(),
        });
    }
    Ok(loaded)
}

/// What a results cell's run needs before its SQL runs on `s`: its
/// statements checked ([`files::refused`]), the files it and the results
/// sources it uses read (resolved against `base`, the notebook's folder)
/// loaded as tables, then the sources' tables ([`materialize`]). Returns
/// the cell's SQL with each reader call replaced by its table, and every
/// table loaded (the cells' first, then the files). Files `access` doesn't
/// let through stop it before anything is read ([`Prepared::Ask`]).
#[allow(clippy::too_many_arguments)]
pub(crate) async fn prepare<F, Fut>(
    s: &mut dyn Session,
    tables: &mut HashMap<String, Loaded>,
    present: &HashSet<String>,
    sql: &str,
    sources: &[LocalSource],
    cap: u64,
    base: Option<&std::path::Path>,
    access: &super::access::Access<'_>,
    out: &mut QueryOutcome,
    cancel: Option<&Notify>,
    rerun: F,
) -> CommandResult<Prepared>
where
    F: FnMut(usize) -> Fut,
    Fut: std::future::Future<Output = CommandResult<Fetched>>,
{
    let need = needed(sql, sources);
    let locals: Vec<usize> = need.iter().copied().filter(|&i| sources[i].local).collect();
    if let Some(e) = files::refused(sql) {
        return Err(CommandError::BadRequest(e));
    }
    for &i in &locals {
        if let Some(e) = files::refused(&sources[i].sql) {
            return Err(CommandError::BadRequest(format!("la celda «{}»: {e}", sources[i].name)));
        }
    }
    let home = files::home_dir();
    let cell = files::plan(sql, base, home.as_deref(), cap).map_err(CommandError::BadRequest)?;
    let mut sources = sources.to_vec();
    let mut reads = cell.clone();
    for &i in &locals {
        let r = files::plan(&sources[i].sql, base, home.as_deref(), cap).map_err(|e| CommandError::BadRequest(format!("la celda «{}»: {e}", sources[i].name)))?;
        if !r.is_empty() {
            sources[i].files_key = r.iter().map(|f| f.key.as_str()).collect::<Vec<_>>().join(",");
            sources[i].sql = files::rewrite(&sources[i].sql, &r);
            reads.extend(r);
        }
    }
    let mut wanted: Vec<DataFile> = Vec::new();
    for f in reads.iter().flat_map(|r| &r.files) {
        if !wanted.iter().any(|w| w.path == f.path) {
            wanted.push(f.clone());
        }
    }
    // Only what `allowed_files` lets through is ever granted.
    let allowed = match files::allowed_files(&wanted, access) {
        Ok(a) => a,
        Err(super::access::Gate::Blocked(e)) => return Err(CommandError::BadRequest(e)),
        Err(super::access::Gate::Ask(files)) => return Ok(Prepared::Ask(files)),
    };
    if let Some(f) = wanted.iter().find(|f| !allowed.iter().any(|a| a.path == f.path)) {
        return Err(CommandError::BadRequest(format!("no se permitió leer «{}»", f.display)));
    }
    let seen = super::access::changes(access, &allowed).await;
    let mut from_files = if reads.is_empty() { Vec::new() } else { load_files(s, tables, present, &reads, &allowed, cap, out, cancel).await? };
    super::access::remember(access, &seen);
    // The change notice, on each file whose content changed.
    for f in from_files.iter_mut().flat_map(|l| l.files.iter_mut()) {
        f.previous_mtime_ms = seen.iter().find(|x| x.path == f.path).and_then(|x| x.previous_mtime_ms);
    }
    let mut loaded = materialize(s, tables, present, sql, &sources, cap, out, rerun).await?;
    loaded.extend(from_files);
    Ok(Prepared::Ready(files::rewrite(sql, &cell), loaded))
}

/// Run `sql` (DuckDB SQL) over the tables of the sources it names and the
/// local files it reads, on the tab's DuckDB session. `notebook_path`: the
/// notebook's file, whose folder relative paths are read from (`None`
/// while it's unsaved). See the module docs.
pub async fn run_local(
    state: &AppState,
    tab: &str,
    sql: &str,
    max_rows: usize,
    cap: u64,
    sources: &[LocalSource],
    notebook_path: Option<&str>,
    approval: Option<&str>,
    policy: super::access::Policy,
) -> CommandResult<RunLocalResponse> {
    let sql = sql.trim();
    if sql.is_empty() {
        return Err(CommandError::BadRequest("la consulta está vacía".into()));
    }
    let cap = cap.clamp(ROW_CAP_RANGE.0, ROW_CAP_RANGE.1);
    if let Some(bad) = sources.iter().find(|s| !valid_name(&s.name)) {
        return Err(CommandError::BadRequest(format!("«{}» no es un nombre de tabla válido", bad.name)));
    }
    // What it may read: never the blocked files; the notebook's grants;
    // the files the user just approved for this run (the token is spent).
    let notebook = super::access::notebook_key(notebook_path);
    let approved = match approval {
        Some(t) => super::access::take(t, tab, notebook.as_deref())?,
        None => Vec::new(),
    };
    let grants = match &notebook {
        Some(nb) => state.store.list_file_grants(Some(nb))?,
        None => Vec::new(),
    };
    let access = super::access::Access { policy, notebook: notebook.clone(), grants, approved, store: Some(state.store.as_ref()) };
    let started = std::time::Instant::now();
    let key = session_key(tab);
    let entry = duck_entry(state, &key).await?;
    entry.cancelled.store(false, Ordering::SeqCst);

    // What's loaded, if it's still the same session.
    let mut tables: HashMap<String, Loaded> = {
        let mut all = CATALOGS.lock().map_err(|_| CommandError::Internal("tablas del notebook".into()))?;
        all.retain(|_, c| c.entry.strong_count() > 0);
        match all.get(&key) {
            Some(c) if c.entry.upgrade().is_some_and(|e| Arc::ptr_eq(&e, &entry)) => c.tables.clone(),
            _ => HashMap::new(),
        }
    };
    let mut out = QueryOutcome::default();
    let mut s = entry.session.lock().await;
    let present = if tables.is_empty() { HashSet::new() } else { existing_tables(&mut **s).await? };
    let duck = entry.clone();
    let rerun = |i: usize| {
        let (src, duck) = (sources[i].clone(), duck.clone());
        async move { rerun(state, tab, &src, cap, &duck).await }
    };
    let base = notebook_path.filter(|p| !p.trim().is_empty()).and_then(|p| std::path::Path::new(p).parent()).map(std::path::Path::to_path_buf);
    let prepared = prepare(&mut **s, &mut tables, &present, sql, sources, cap, base.as_deref(), &access, &mut out, Some(&entry.cancel), rerun).await;
    if let Ok(mut all) = CATALOGS.lock() {
        all.insert(key.clone(), Catalog { entry: Arc::downgrade(&entry), tables });
    }
    let (sql, loaded) = match prepared? {
        Prepared::Ready(sql, loaded) => (sql, loaded),
        Prepared::Ask(files) => {
            let approval = super::access::ask(tab, notebook.as_deref(), files)?;
            return Ok(RunLocalResponse { outcome: QueryOutcome::default(), loaded: Vec::new(), approval: Some(approval) });
        }
    };
    let sql = sql.as_str();

    // The cell's own SQL.
    let mut run = QueryOutcome::default();
    let finished = tokio::select! {
        r = s.execute(sql, max_rows, &mut run) => Some(r),
        _ = entry.cancel.notified() => None,
    };
    drop(s);
    match finished {
        None | Some(Err(dbine_driver::Error::Cancelled)) => return Err(CommandError::Cancelled),
        Some(Err(dbine_driver::Error::Statement(e))) => run.push_error(*e),
        Some(Err(e)) => run.push_error(ScriptError::new(e.to_string())),
        Some(Ok(())) => {}
    }
    let error = run.error.take().or_else(|| run.errors.first().map(|e| e.message.clone()));
    // The loading notes first, then what the run said.
    out.absorb(run);
    out.error = error;
    out.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(RunLocalResponse { outcome: out, loaded, approval: None })
}

#[cfg(test)]
#[path = "local_tests.rs"]
mod tests;
