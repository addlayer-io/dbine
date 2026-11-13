//! ksqlDB's scheduler managed through the designer
//! ([`dbine_driver::Session::scheduler_info`] and the rest): its persistent
//! queries, the one kind of object that runs on the server on its own
//! (`CREATE STREAM … AS SELECT`, `CREATE TABLE … AS SELECT`, `INSERT INTO …
//! SELECT`). They are the jobs of [`crate::jobs`], with the same ids.
//!
//! - The form takes the query's statement apart (`SHOW QUERIES EXTENDED`
//!   keeps it, rewritten by ksqlDB for `CREATE … AS SELECT`): the stream or
//!   table it writes and its `WITH` clause (topic, formats, partitions,
//!   retention…), the `SELECT`, the properties it was started with
//!   (`overriddenProperties`, sent as `streamsProperties`) and whether it is
//!   paused.
//! - Scripts are ksqlDB statements: the properties as `SET` / `UNSET` around
//!   the statement (as DBine's editor runs them), then the statement. A
//!   change to a `CREATE … AS SELECT` is `CREATE OR REPLACE`, in place: the
//!   query keeps its id, its state and its offsets, and ksqlDB refuses what
//!   it can't upgrade. A change to the destination (the `WITH` clause), or
//!   one asked to recreate, is `TERMINATE` + `DROP` + `CREATE`: the state
//!   and the offsets go, the topic stays. An `INSERT INTO` can't be replaced:
//!   it is terminated and started again with the same `QUERY_ID`. Enabling
//!   and disabling is `RESUME` / `PAUSE` (ksqlDB 0.26 on).
//! - Deleting is `TERMINATE`: the stream or table it fed stays.
//! - The query a `CREATE SOURCE TABLE` runs (`CST_…`) is listed read-only:
//!   ksqlDB refuses to terminate or pause it ("linked to a source table"),
//!   it ends with its table.
//! - There is no history to clear, no steps and no action besides the
//!   enabled switch (`PAUSE` / `RESUME`); the scheduler's log is the errors
//!   each query keeps (`queryErrors`).
//!
//! Saving runs one statement per request, each waiting for the previous
//! one's command (`commandSequenceNumber`) and for its own to finish
//! (`/status`); ksqlDB has no transactions, so the script checks first what
//! would make a later statement fail (other queries reading from a stream
//! to drop, an id in use). ksqlDB checks a statement only when it gets it:
//! when it refuses a recreate's new statement after the `TERMINATE` (and
//! the `DROP`) ran, the old query is made again as it was, with its
//! properties and paused if it was ([`Restore`]).
//!
//! Ids and every value come from the app as untrusted text: query ids have
//! the shape [`valid_id`] checks and must name a persistent query the server
//! lists; names are written as `` `quoted` `` identifiers (or plain when
//! typed plain, as ksqlDB reads them in upper case); `WITH` values as
//! `'quoted'` literals or checked numbers; the `SELECT` is refused when it
//! holds more than one statement.

use crate::jobs::{can_pause, descriptions, NO_PAUSE_NOTE};
use crate::processes::valid_id;
use crate::{check, http_error, schema_type, text, KsqlSession, KSQL_JSON, PROCESSING_LOG};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{
    Error, FormField, FormKind, FormList, FormSection, FormValues, MonitorTable, ObjectForm,
    Result, SchedulerInfo, SchedulerItem, SchedulerKind, SchedulerList,
};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;
use std::fmt::Write as _;
use std::time::{Duration, Instant};

// ---------------------------------------------------------------------------
// Messages
// ---------------------------------------------------------------------------

const KIND_ONLY: &str = "ksqlDB solo tiene consultas persistentes en su planificador";
const STALE: &str = "La consulta cambió en el servidor desde que se abrió el formulario: abrilo de nuevo para no pisar ese cambio.";
const REPLACE_NOTE: &str = "Un cambio en la consulta o en sus propiedades se aplica en el lugar (CREATE OR REPLACE): conserva su id, su estado y sus offsets, si ksqlDB acepta el cambio. Cambiar el destino (topic, formatos, particiones…) o marcar «Recrear la consulta» la termina, borra su stream o tabla (el topic queda) y la crea de nuevo: se pierden su estado y sus offsets, y vuelve a leer su entrada desde «Leer desde».";
const INSERT_NOTE: &str = "ksqlDB no cambia una INSERT INTO en el lugar: guardar un cambio la termina y la crea de nuevo con el mismo id. Se pierden su estado y sus offsets, y vuelve a leer su entrada desde «Leer desde» (desde el principio, el destino recibe otra vez lo que ya tenía).";
const PROPS_NOTE: &str = "Las propiedades valen solo para esta consulta: ksqlDB guarda las que se le pasaron al crearla o reemplazarla, y una consulta nueva o reemplazada usa las de este formulario.";
const LOG_NOTE: &str = "El registro son los últimos errores que ksqlDB guarda en memoria de cada consulta persistente (ksql.query.error.max.queue.size): se pierden al reiniciar el servidor.";

/// Longest the save waits for one command.
const COMMAND_WAIT: Duration = Duration::from_secs(120);
/// Longest a `SELECT` may be.
const MAX_SELECT: usize = 1 << 20;

fn bad(message: String) -> Error {
    Error::Query(message)
}

/// Text from the app, short and on one line, for a message.
fn shown(s: &str) -> String {
    s.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(60)
        .collect()
}

/// Text for a `--` comment line: a name from the server can't end the
/// comment and turn the rest of the line into a statement. Line breaks
/// (ksqlDB ends a comment at `\r` too, and U+2028/U+2029 for an editor)
/// and other control characters become spaces.
fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') {
                ' '
            } else {
                c
            }
        })
        .collect()
}

fn kind_check(kind: &str) -> Result<()> {
    if kind.trim() == "job" {
        Ok(())
    } else {
        Err(Error::Unsupported(KIND_ONLY.into()))
    }
}

// ---------------------------------------------------------------------------
// The statement, taken apart
// ---------------------------------------------------------------------------

/// What a persistent query's statement makes.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stmt {
    Stream,
    Table,
    Insert,
}

impl Stmt {
    fn key(self) -> &'static str {
        match self {
            Stmt::Stream => "create_stream",
            Stmt::Table => "create_table",
            Stmt::Insert => "insert",
        }
    }

    fn of_key(k: &str) -> Option<Stmt> {
        match k.trim() {
            "create_stream" => Some(Stmt::Stream),
            "create_table" => Some(Stmt::Table),
            "insert" => Some(Stmt::Insert),
            _ => None,
        }
    }

    /// `STREAM` / `TABLE` for `CREATE` and `DROP`.
    fn word(self) -> &'static str {
        match self {
            Stmt::Table => "TABLE",
            _ => "STREAM",
        }
    }
}

/// A persistent query's statement in parts.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Def {
    pub(crate) stmt: Stmt,
    /// The stream or table it writes, as written in the statement
    /// (unquoted names in upper case, as ksqlDB reads them).
    pub(crate) sink: String,
    /// The `WITH` clause: upper-case key and the value as written (`'x'`,
    /// `1`, `true`).
    pub(crate) with: Vec<(String, String)>,
    /// `SELECT …` to the end, without the closing `;`.
    pub(crate) select: String,
}

/// A cursor over a statement: words, identifiers, parentheses, skipping
/// blanks and comments.
struct Lexer<'a> {
    s: &'a str,
    i: usize,
}

impl<'a> Lexer<'a> {
    fn new(s: &'a str) -> Self {
        Lexer { s, i: 0 }
    }

    fn rest(&self) -> &'a str {
        &self.s[self.i..]
    }

    fn skip(&mut self) {
        loop {
            let r = self.rest();
            let t = r.trim_start();
            self.i += r.len() - t.len();
            if t.starts_with("--") {
                self.i += t.find('\n').map_or(t.len(), |n| n + 1);
            } else if t.starts_with("/*") {
                self.i += t.find("*/").map_or(t.len(), |n| n + 2);
            } else {
                return;
            }
        }
    }

    /// The next plain word (`[A-Za-z_][A-Za-z0-9_]*`), as written.
    fn word(&mut self) -> Option<&'a str> {
        self.skip();
        let r = self.rest();
        let first = r.chars().next()?;
        if !(first.is_ascii_alphabetic() || first == '_') {
            return None;
        }
        let n = r
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(r.len());
        self.i += n;
        Some(&r[..n])
    }

    /// The keyword `w` (any case) next, consumed when it is.
    fn eat(&mut self, w: &str) -> bool {
        let at = self.i;
        match self.word() {
            Some(x) if x.eq_ignore_ascii_case(w) => true,
            _ => {
                self.i = at;
                false
            }
        }
    }

    /// An identifier: a plain word (in upper case, as ksqlDB reads it) or a
    /// `` `quoted` `` one (as written).
    fn ident(&mut self) -> Option<String> {
        self.skip();
        if let Some(r) = self.rest().strip_prefix('`') {
            let mut out = String::new();
            let mut chars = r.char_indices().peekable();
            while let Some((n, c)) = chars.next() {
                if c == '`' {
                    if chars.peek().map(|(_, c)| *c) == Some('`') {
                        out.push('`');
                        chars.next();
                        continue;
                    }
                    self.i += 1 + n + 1;
                    return Some(out);
                }
                out.push(c);
            }
            return None;
        }
        self.word().map(str::to_ascii_uppercase)
    }

    /// `( … )`: what's inside, quotes and nested parentheses kept.
    fn paren(&mut self) -> Option<&'a str> {
        self.skip();
        let r = self.rest();
        if !r.starts_with('(') {
            return None;
        }
        let mut depth = 0usize;
        let mut quote: Option<char> = None;
        for (n, c) in r.char_indices() {
            match (quote, c) {
                (Some(q), c) if c == q => quote = None,
                (Some(_), _) => {}
                (None, '\'' | '`' | '"') => quote = Some(c),
                (None, '(') => depth += 1,
                (None, ')') => {
                    depth -= 1;
                    if depth == 0 {
                        self.i += n + 1;
                        return Some(&r[1..n]);
                    }
                }
                _ => {}
            }
        }
        None
    }
}

/// `'it''s'` → `it's`; anything else as it is.
fn unquote(v: &str) -> String {
    let v = v.trim();
    match v.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
        Some(inner) if v.len() >= 2 => inner.replace("''", "'"),
        _ => v.to_string(),
    }
}

/// `'text'` with its quotes doubled.
fn lit(v: &str) -> String {
    format!("'{}'", v.replace('\'', "''"))
}

/// The pieces of a list split at top-level commas (quotes kept whole).
fn split_commas(s: &str) -> Vec<&str> {
    let (mut out, mut start, mut depth) = (Vec::new(), 0usize, 0usize);
    let mut quote: Option<char> = None;
    for (n, c) in s.char_indices() {
        match (quote, c) {
            (Some(q), c) if c == q => quote = None,
            (Some(_), _) => {}
            (None, '\'' | '`' | '"') => quote = Some(c),
            (None, '(') => depth += 1,
            (None, ')') => depth = depth.saturating_sub(1),
            (None, ',') if depth == 0 => {
                out.push(&s[start..n]);
                start = n + 1;
            }
            _ => {}
        }
    }
    out.push(&s[start..]);
    out.into_iter().filter(|p| !p.trim().is_empty()).collect()
}

fn parse_with(inner: &str) -> Option<Vec<(String, String)>> {
    split_commas(inner)
        .into_iter()
        .map(|p| {
            let (k, v) = p.split_once('=')?;
            let k = k
                .trim()
                .trim_matches('`')
                .trim_matches('"')
                .to_ascii_uppercase();
            (!k.is_empty()).then(|| (k, v.trim().to_string()))
        })
        .collect()
}

/// A persistent query's statement, as `SHOW QUERIES EXTENDED` keeps it.
pub(crate) fn parse(statement: &str) -> Option<Def> {
    let mut l = Lexer::new(statement);
    let stmt = if l.eat("CREATE") {
        if l.eat("OR") && !l.eat("REPLACE") {
            return None;
        }
        let stmt = if l.eat("STREAM") {
            Stmt::Stream
        } else if l.eat("TABLE") {
            Stmt::Table
        } else {
            return None;
        };
        if l.eat("IF") && !(l.eat("NOT") && l.eat("EXISTS")) {
            return None;
        }
        stmt
    } else if l.eat("INSERT") && l.eat("INTO") {
        Stmt::Insert
    } else {
        return None;
    };
    let sink = l.ident()?;
    let mut with = Vec::new();
    if l.eat("WITH") {
        with = parse_with(l.paren()?)?;
    }
    if stmt != Stmt::Insert && !l.eat("AS") {
        return None;
    }
    l.skip();
    let select = l.rest().trim().trim_end_matches(';').trim_end().to_string();
    Lexer::new(&select).eat("SELECT").then_some(Def {
        stmt,
        sink,
        with,
        select,
    })
}

/// The `SELECT` as one statement: it starts with `SELECT` and has no `;`
/// but a closing one (outside quotes and comments). Returns it without
/// that `;`.
pub(crate) fn one_select(sql: &str) -> Result<String> {
    let t = sql.trim();
    if t.is_empty() {
        return Err(bad("La consulta necesita su SELECT.".into()));
    }
    if t.len() > MAX_SELECT {
        return Err(bad("El SELECT es demasiado largo.".into()));
    }
    if !Lexer::new(t).eat("SELECT") {
        return Err(bad(
            "La consulta tiene que empezar con SELECT (el CREATE o el INSERT INTO los escribe DBine).".into(),
        ));
    }
    let b = t.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            q @ (b'\'' | b'`' | b'"') => {
                i += 1;
                while i < b.len() {
                    if b[i] == q {
                        if b.get(i + 1) == Some(&q) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                if i >= b.len() {
                    return Err(bad(
                        "El SELECT tiene un texto o un nombre sin cerrar.".into()
                    ));
                }
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
                continue;
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                match t[i + 2..].find("*/") {
                    Some(n) => i += 2 + n + 2,
                    None => return Err(bad("El SELECT tiene un comentario sin cerrar.".into())),
                }
                continue;
            }
            b';' => {
                if !t[i + 1..].trim().is_empty() && !only_comments(&t[i + 1..]) {
                    return Err(bad(
                        "El SELECT tiene más de una sentencia: escribí solo la consulta.".into(),
                    ));
                }
                return Ok(t[..i].trim_end().to_string());
            }
            _ => {}
        }
        i += 1;
    }
    Ok(t.to_string())
}

fn only_comments(s: &str) -> bool {
    let mut l = Lexer::new(s);
    l.skip();
    l.rest().is_empty()
}

// ---------------------------------------------------------------------------
// The destination's WITH clause
// ---------------------------------------------------------------------------

/// How a `WITH` property is edited.
#[derive(Clone, Copy, PartialEq)]
enum Prop {
    Text,
    /// A whole number in the range.
    Int(i64, i64),
    Format,
    Delimiter,
    /// `true`, `false` or unset.
    Flag,
}

/// The `WITH` properties of a `CREATE … AS SELECT` the form has fields
/// for, in the order the script writes them: key, field, kind.
const WITH_PROPS: [(&str, &str, Prop); 15] = [
    ("KAFKA_TOPIC", "kafka_topic", Prop::Text),
    ("PARTITIONS", "partitions", Prop::Int(1, 1_000_000)),
    ("REPLICAS", "replicas", Prop::Int(-1, 32_767)),
    ("RETENTION_MS", "retention_ms", Prop::Int(-1, i64::MAX)),
    ("KEY_FORMAT", "key_format", Prop::Format),
    ("VALUE_FORMAT", "value_format", Prop::Format),
    ("KEY_DELIMITER", "key_delimiter", Prop::Delimiter),
    ("VALUE_DELIMITER", "value_delimiter", Prop::Delimiter),
    ("TIMESTAMP", "timestamp", Prop::Text),
    ("TIMESTAMP_FORMAT", "timestamp_format", Prop::Text),
    ("WRAP_SINGLE_VALUE", "wrap_single_value", Prop::Flag),
    (
        "KEY_SCHEMA_ID",
        "key_schema_id",
        Prop::Int(0, 2_147_483_647),
    ),
    (
        "VALUE_SCHEMA_ID",
        "value_schema_id",
        Prop::Int(0, 2_147_483_647),
    ),
    ("KEY_SCHEMA_FULL_NAME", "key_schema_full_name", Prop::Text),
    (
        "VALUE_SCHEMA_FULL_NAME",
        "value_schema_full_name",
        Prop::Text,
    ),
];

/// Kept by ksqlDB from the source's kind; it refuses it in a statement.
const CLEANUP_POLICY: &str = "CLEANUP_POLICY";
/// The id of an `INSERT INTO` (its `WITH` clause has nothing else).
const QUERY_ID: &str = "QUERY_ID";

const FORMATS: [&str; 8] = [
    "JSON",
    "JSON_SR",
    "AVRO",
    "PROTOBUF",
    "PROTOBUF_NOSR",
    "DELIMITED",
    "KAFKA",
    "NONE",
];

fn format_select() -> FormKind {
    FormKind::select(&[
        ("", "El valor por defecto del servidor"),
        ("JSON", "JSON"),
        ("JSON_SR", "JSON con Schema Registry (JSON_SR)"),
        ("AVRO", "Avro"),
        ("PROTOBUF", "Protobuf"),
        ("PROTOBUF_NOSR", "Protobuf sin Schema Registry"),
        ("DELIMITED", "Delimitado"),
        ("KAFKA", "KAFKA (tipos primitivos)"),
        ("NONE", "Ninguno (sin clave)"),
    ])
}

/// A property name for `WITH` or `SET`: letters, digits, `_`, `.`, `-`.
fn prop_name(k: &str) -> bool {
    !k.is_empty()
        && k.len() <= 200
        && k.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '_' | '.' | '-'))
}

/// A value as `WITH` writes it: numbers and booleans bare, the rest quoted.
fn with_value(v: &str) -> String {
    let t = v.trim();
    if t.parse::<i64>().is_ok() || t.eq_ignore_ascii_case("true") || t.eq_ignore_ascii_case("false")
    {
        t.to_string()
    } else {
        lit(v)
    }
}

/// The `WITH` clause as key → value, to tell whether it changed.
fn with_map(with: &[(String, String)]) -> BTreeMap<String, String> {
    with.iter()
        .filter(|(k, _)| k != CLEANUP_POLICY && k != QUERY_ID)
        .map(|(k, v)| (k.clone(), unquote(v)))
        .collect()
}

// ---------------------------------------------------------------------------
// What the server has
// ---------------------------------------------------------------------------

/// A persistent query as the designer reads it.
#[derive(Debug, Clone)]
pub(crate) struct Query {
    pub(crate) id: String,
    pub(crate) def: Def,
    /// The statement as the server keeps it: the form carries it back, and
    /// a save over a changed query is refused.
    pub(crate) statement: String,
    pub(crate) props: BTreeMap<String, String>,
    pub(crate) paused: bool,
    pub(crate) sources: Vec<String>,
    pub(crate) description: Value,
}

fn strings(q: &Value, key: &str) -> Vec<String> {
    q.get(key)
        .and_then(Value::as_array)
        .map(|a| a.iter().map(text).filter(|s| !s.is_empty()).collect())
        .unwrap_or_default()
}

fn query_id(q: &Value) -> Option<String> {
    q.get("id")
        .map(|i| i.get("id").map(text).unwrap_or_else(|| text(i)))
        .filter(|s| valid_id(s))
}

/// Paused on every host that says.
fn paused(q: &Value) -> bool {
    let hosts: Vec<String> = q
        .get("ksqlHostQueryStatus")
        .and_then(Value::as_object)
        .map(|m| m.values().map(text).collect())
        .unwrap_or_default();
    if hosts.is_empty() {
        return q.get("state").map(text).as_deref() == Some("PAUSED");
    }
    hosts.iter().all(|s| s == "PAUSED")
}

/// A persistent query as the tree lists it. One whose statement DBine
/// doesn't take apart — the query a `CREATE SOURCE TABLE` keeps running
/// (`CST_…`, which ksqlDB ends only with its table), or a statement DBine
/// doesn't read — opens read-only and isn't deleted from the designer: its
/// form and its `TERMINATE` both start from the statement.
fn item_of(q: &Value, pause: bool) -> Option<SchedulerItem> {
    let id = query_id(q)?;
    let sinks = strings(q, "sinks");
    let sources = strings(q, "sources");
    let readable = query_of(q).is_some();
    Some(SchedulerItem {
        name: id.clone(),
        id,
        description: Some(format!("{} ← {}", sinks.join(", "), sources.join(", "))),
        enabled: pause.then(|| !paused(q)),
        system: false,
        can_edit: (!readable).then_some(false),
        can_delete: (!readable).then_some(false),
        actions: None,
        run_from_step: None,
        clear_history: None,
    })
}

pub(crate) fn query_of(q: &Value) -> Option<Query> {
    let id = query_id(q)?;
    let statement = q.get("statementText").map(text).unwrap_or_default();
    let mut def = parse(&statement)?;
    // The server's name for the sink, when it says.
    if let Some(s) = strings(q, "sinks").into_iter().next() {
        if s.eq_ignore_ascii_case(&def.sink) {
            def.sink = s;
        }
    }
    let props = q
        .get("overriddenProperties")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(k, v)| (k.clone(), text(v))).collect())
        .unwrap_or_default();
    Some(Query {
        paused: paused(q),
        sources: strings(q, "sources"),
        description: q.clone(),
        id,
        def,
        statement,
        props,
    })
}

/// The newest errors each query kept: (when, query, kind, message).
pub(crate) fn errors_of(queries: &[&Value]) -> Vec<(i64, String, String, String)> {
    let mut out = Vec::new();
    for q in queries {
        let Some(id) = query_id(q) else { continue };
        for e in q
            .get("queryErrors")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            out.push((
                e.get("timestamp").and_then(Value::as_i64).unwrap_or(0),
                id.clone(),
                e.get("type").map(text).unwrap_or_default(),
                e.get("errorMessage").map(text).unwrap_or_default(),
            ));
        }
    }
    out.sort_by_key(|a| std::cmp::Reverse(a.0));
    out
}

// ---------------------------------------------------------------------------
// Values from the designer
// ---------------------------------------------------------------------------

/// A form's values (or a list row's) as the app sent them: untrusted, any
/// JSON type where a field expects another.
#[derive(Clone, Copy)]
struct Vals<'a>(&'a BTreeMap<String, Value>);

impl<'a> Vals<'a> {
    fn get(&self, k: &str) -> Option<&'a Value> {
        self.0.get(k).filter(|v| !v.is_null())
    }

    fn raw(&self, k: &str) -> String {
        match self.get(k) {
            Some(Value::String(s)) => s.clone(),
            Some(Value::Array(_) | Value::Object(_)) | None => String::new(),
            Some(v) => v.to_string(),
        }
    }

    fn text(&self, k: &str) -> String {
        self.raw(k).trim().to_string()
    }

    fn opt(&self, k: &str) -> Option<String> {
        Some(self.text(k)).filter(|t| !t.is_empty())
    }

    fn bool_or(&self, k: &str, default: bool) -> bool {
        match self.get(k) {
            Some(Value::Bool(b)) => *b,
            Some(Value::Number(n)) => n.as_f64().is_some_and(|f| f != 0.0),
            Some(Value::String(s)) => match s.trim().to_ascii_lowercase().as_str() {
                "true" | "1" | "yes" | "on" => true,
                "" => default,
                _ => false,
            },
            _ => default,
        }
    }

    /// A whole number in `min..=max`; `None` when empty.
    fn int(&self, k: &str, label: &str, min: i64, max: i64) -> Result<Option<i64>> {
        let n = match self.get(k) {
            None => return Ok(None),
            Some(Value::String(s)) if s.trim().is_empty() => return Ok(None),
            Some(Value::Number(n)) => n
                .as_i64()
                .or_else(|| n.as_f64().filter(|f| f.fract() == 0.0).map(|f| f as i64)),
            Some(Value::String(s)) => s.trim().parse::<i64>().ok(),
            Some(_) => None,
        };
        match n {
            Some(n) if (min..=max).contains(&n) => Ok(Some(n)),
            Some(n) => Err(bad(format!(
                "«{label}» tiene que estar entre {min} y {max} (es {n})."
            ))),
            None => Err(bad(format!("«{label}» tiene que ser un número entero."))),
        }
    }
}

/// Text that goes into a quoted literal: one line, no control characters.
fn one_line(v: &str, label: &str) -> Result<String> {
    if v.chars().any(char::is_control) {
        return Err(bad(format!(
            "«{label}» no puede tener saltos de línea ni caracteres de control."
        )));
    }
    if v.len() > 4000 {
        return Err(bad(format!("«{label}» es demasiado largo.")));
    }
    Ok(v.to_string())
}

/// A stream or table name as typed for a new one: plain names go as they
/// are (ksqlDB reads them in upper case), any other is quoted.
fn new_name(name: &str) -> Result<String> {
    let n = one_line(name.trim(), "Nombre")?;
    if n.is_empty() {
        return Err(bad(
            "La consulta necesita el nombre de su stream o tabla.".into()
        ));
    }
    if n.len() > 255 {
        return Err(bad("El nombre es demasiado largo.".into()));
    }
    let plain = n
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    Ok(if plain && !reserved(&n) {
        n
    } else {
        quote_ident(Quote::Backtick, &n)
    })
}

/// Words a plain name can't be (ksqlDB's reserved ones a name could clash
/// with).
fn reserved(n: &str) -> bool {
    const WORDS: [&str; 36] = [
        "SELECT",
        "FROM",
        "WHERE",
        "AS",
        "AND",
        "OR",
        "NOT",
        "NULL",
        "TRUE",
        "FALSE",
        "CREATE",
        "STREAM",
        "TABLE",
        "INSERT",
        "INTO",
        "WITH",
        "EMIT",
        "CHANGES",
        "FINAL",
        "GROUP",
        "BY",
        "HAVING",
        "WINDOW",
        "JOIN",
        "ON",
        "LIMIT",
        "PARTITION",
        "DROP",
        "TERMINATE",
        "VALUES",
        "SHOW",
        "LIST",
        "DESCRIBE",
        "EXPLAIN",
        "PAUSE",
        "RESUME",
    ];
    WORDS.iter().any(|w| w.eq_ignore_ascii_case(n))
}

/// A server's stream or table name, quoted (its case kept).
fn server_name(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

// ---------------------------------------------------------------------------
// The wanted query
// ---------------------------------------------------------------------------

/// What the form asks for.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Wanted {
    pub(crate) stmt: Stmt,
    /// The name as written in the statement.
    pub(crate) name_sql: String,
    /// The `WITH` clause (`CREATE`), as written.
    pub(crate) with: Vec<(String, String)>,
    /// `INSERT INTO`'s own id (`QUERY_ID`), upper case.
    pub(crate) query_id: Option<String>,
    pub(crate) select: String,
    pub(crate) props: BTreeMap<String, String>,
    pub(crate) enabled: bool,
    pub(crate) recreate: bool,
}

/// The form's values as a query, checked. `streams`: the streams an
/// `INSERT INTO` may write to (`None`: not checked).
pub(crate) fn wanted(
    values: &FormValues,
    current: Option<&Query>,
    streams: Option<&[String]>,
) -> Result<Wanted> {
    let v = Vals(&values.values);
    let stmt = match current {
        Some(q) => q.def.stmt,
        None => {
            let k = v.text("statement_kind");
            Stmt::of_key(if k.is_empty() { "create_stream" } else { &k }).ok_or_else(|| {
                bad(format!(
                    "«{}» no es un tipo de consulta persistente (CREATE STREAM, CREATE TABLE o INSERT INTO).",
                    shown(&k)
                ))
            })?
        }
    };
    let name_sql = match (current, stmt) {
        (Some(q), _) => server_name(&q.def.sink),
        (None, Stmt::Insert) => {
            let target = v
                .opt("target")
                .ok_or_else(|| bad("Elegí el stream en el que escribe la INSERT INTO.".into()))?;
            if let Some(list) = streams {
                let Some(found) = list.iter().find(|s| **s == target) else {
                    return Err(bad(format!(
                        "No existe el stream «{}» (una INSERT INTO escribe en un stream que ya existe).",
                        shown(&target)
                    )));
                };
                server_name(found)
            } else {
                server_name(&target)
            }
        }
        (None, _) => new_name(&v.raw("name"))?,
    };
    let select = one_select(&v.raw("select"))?;
    let mut with = Vec::new();
    let mut query_id = None;
    if stmt == Stmt::Insert {
        query_id = match current {
            Some(q) => Some(q.id.clone()),
            None => match v.opt("query_id") {
                None => None,
                Some(id) => {
                    let ok = id.len() <= 200
                        && id.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
                    if !ok {
                        return Err(bad(format!(
                            "«{}» no es un id de consulta válido (letras, dígitos y _).",
                            shown(&id)
                        )));
                    }
                    Some(id.to_ascii_uppercase())
                }
            },
        };
    } else {
        for (key, field, kind) in WITH_PROPS {
            let label = key;
            let value = match kind {
                Prop::Text => v
                    .opt(field)
                    .map(|t| one_line(&t, label).map(|t| lit(&t)))
                    .transpose()?,
                Prop::Int(min, max) => v.int(field, label, min, max)?.map(|n| n.to_string()),
                Prop::Format => match v.opt(field) {
                    None => None,
                    Some(f) => {
                        let up = f.to_ascii_uppercase();
                        if !FORMATS.contains(&up.as_str()) {
                            return Err(bad(format!(
                                "«{}» no es un formato de ksqlDB.",
                                shown(&f)
                            )));
                        }
                        Some(lit(&up))
                    }
                },
                Prop::Delimiter => match v.opt(field) {
                    None => None,
                    Some(d) => {
                        let up = d.to_ascii_uppercase();
                        if d.chars().count() != 1 && up != "TAB" && up != "SPACE" {
                            return Err(bad(format!(
                                "«{label}» es un solo carácter, TAB o SPACE."
                            )));
                        }
                        Some(lit(&one_line(&d, label)?))
                    }
                },
                Prop::Flag => match v.text(field).to_ascii_lowercase().as_str() {
                    "" => None,
                    "true" => Some("true".into()),
                    "false" => Some("false".into()),
                    other => {
                        return Err(bad(format!(
                            "«{label}»: «{}» no es true ni false.",
                            shown(other)
                        )))
                    }
                },
            };
            if let Some(value) = value {
                with.push((key.to_string(), value));
            }
        }
        for row in values.lists.get("with").into_iter().flatten() {
            let r = Vals(row);
            let key = r.text("key").to_ascii_uppercase();
            if key.is_empty() && r.text("value").is_empty() {
                continue;
            }
            if !prop_name(&key) || key.contains(['.', '-']) {
                return Err(bad(format!(
                    "«{}» no es el nombre de una propiedad de WITH.",
                    shown(&key)
                )));
            }
            if key == CLEANUP_POLICY || key == QUERY_ID {
                return Err(bad(format!(
                    "ksqlDB no deja poner {key} en un CREATE … AS SELECT."
                )));
            }
            if with.iter().any(|(k, _)| *k == key) {
                return Err(bad(format!("La propiedad {key} está dos veces.")));
            }
            let value = one_line(&r.raw("value"), &key)?;
            with.push((key, with_value(&value)));
        }
    }
    let mut props = BTreeMap::new();
    for row in values.lists.get("properties").into_iter().flatten() {
        let r = Vals(row);
        let key = r.text("name");
        let value = r.raw("value");
        if key.is_empty() && value.trim().is_empty() {
            continue;
        }
        if !prop_name(&key) {
            return Err(bad(format!(
                "«{}» no es el nombre de una propiedad de ksqlDB.",
                shown(&key)
            )));
        }
        if key == "auto.offset.reset" {
            return Err(bad(
                "auto.offset.reset se elige en «Leer desde», no en la lista de propiedades.".into(),
            ));
        }
        if props.insert(key.clone(), one_line(&value, &key)?).is_some() {
            return Err(bad(format!("La propiedad {key} está dos veces.")));
        }
    }
    match v.text("offset_reset").as_str() {
        "" => {}
        o @ ("earliest" | "latest") => {
            props.insert("auto.offset.reset".into(), o.into());
        }
        other => {
            return Err(bad(format!(
                "«{}» no es un valor de «Leer desde» (earliest o latest).",
                shown(other)
            )))
        }
    }
    Ok(Wanted {
        stmt,
        name_sql,
        with,
        query_id,
        select,
        props,
        enabled: v.bool_or("enabled", true),
        recreate: current.is_some() && v.bool_or("recreate", false),
    })
}

// ---------------------------------------------------------------------------
// Scripts
// ---------------------------------------------------------------------------

/// After the statement: pause or resume the query.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Toggle {
    Pause,
    Resume,
}

/// One statement the save runs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Step {
    pub(crate) sql: String,
    /// Sent with the query's properties (the `CREATE` / `INSERT`).
    pub(crate) with_props: bool,
}

/// What a change writes and runs.
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Plan {
    pub(crate) script: String,
    pub(crate) steps: Vec<Step>,
    pub(crate) props: BTreeMap<String, String>,
    /// The query's id when it keeps it.
    pub(crate) id: Option<String>,
    /// Pause or resume once the statements ran (with the query's id).
    pub(crate) toggle: Option<Toggle>,
    /// Nothing differs from what the server has.
    pub(crate) empty: bool,
    /// A recreate: how to put the old query back when ksqlDB refuses a
    /// step after its `TERMINATE` (the first step).
    pub(crate) restore: Option<Restore>,
}

/// The old query of a recreate, to make again when ksqlDB refuses the new
/// statement (it checks a statement only when it gets it, so a bad
/// `SELECT`, a format that needs Schema Registry or a topic that doesn't
/// fit is refused after the `TERMINATE` and the `DROP` ran).
#[derive(Debug, Clone, PartialEq)]
pub(crate) struct Restore {
    /// The old query as DBine writes it again: its `WITH` clause without
    /// what ksqlDB keeps but refuses in a statement, an `INSERT INTO` with
    /// its own id, its properties.
    pub(crate) old: Wanted,
    /// The old query's id.
    pub(crate) id: String,
    /// The step that drops its stream or table (a `CREATE … AS SELECT`).
    pub(crate) drop: Option<usize>,
    /// It was paused: it is paused again.
    pub(crate) paused: bool,
}

impl Restore {
    fn of(q: &Query, drop: Option<usize>) -> Restore {
        let insert = q.def.stmt == Stmt::Insert;
        Restore {
            old: Wanted {
                stmt: q.def.stmt,
                name_sql: server_name(&q.def.sink),
                with: if insert {
                    Vec::new()
                } else {
                    q.def
                        .with
                        .iter()
                        .filter(|(k, _)| k != CLEANUP_POLICY && k != QUERY_ID)
                        .cloned()
                        .collect()
                },
                query_id: insert.then(|| q.id.clone()),
                select: q.def.select.clone(),
                props: q.props.clone(),
                enabled: !q.paused,
                recreate: false,
            },
            id: q.id.clone(),
            drop,
            paused: q.paused,
        }
    }

    /// The statement that makes the old query again after step `failed`
    /// was refused: `CREATE OR REPLACE` when the `DROP` itself was refused
    /// (its stream or table is still there, without a query).
    pub(crate) fn statement(&self, failed: usize) -> String {
        statement(&self.old, self.drop == Some(failed))
    }

    /// The same as a script, with its properties and the pause: what the
    /// user runs when DBine couldn't.
    pub(crate) fn script(&self, failed: usize) -> String {
        let step = Step {
            sql: self.statement(failed),
            with_props: true,
        };
        let pause = self
            .paused
            .then(|| (Toggle::Pause, self.old.query_id.as_deref().unwrap_or("")));
        render(&[], &[step], &self.old.props, pause)
    }

    /// What the save says when step `failed` was refused with `cause`, and
    /// how the restore went.
    pub(crate) fn message(&self, failed: usize, cause: &str, restored: &Restored) -> String {
        let mut out = vec![format!("ksqlDB rechazó el cambio: {cause}")];
        out.push(match self.drop {
            Some(d) if failed > d => format!(
                "DBine ya había terminado la consulta {} y borrado su stream o tabla {} (el topic queda).",
                self.id,
                comment_text(&self.old.name_sql)
            ),
            _ => format!("DBine ya había terminado la consulta {}.", self.id),
        });
        match restored {
            Restored::Made { id, paused } => {
                out.push(match id {
                    Some(n) if *n != self.id => {
                        format!("La volvió a crear como estaba, con el id {n}.")
                    }
                    _ => "La volvió a crear como estaba.".into(),
                });
                out.push(
                    "Perdió su estado y sus offsets: lee su entrada otra vez desde auto.offset.reset."
                        .into(),
                );
                if let Err(m) = paused {
                    out.push(format!("Estaba pausada y no se pudo pausar otra vez: {m}"));
                }
            }
            Restored::Failed(m) => {
                out.push(format!(
                    "Intentó volver a crearla como estaba y ksqlDB también lo rechazó: {m}"
                ));
                out.push("La sentencia que tenía era:".into());
                out.push(self.script(failed).trim_end().to_string());
            }
        }
        out.join("\n")
    }
}

/// How putting the old query back went.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Restored {
    /// Made again with this id; `paused`: whether pausing it again (when it
    /// was paused) failed.
    Made {
        id: Option<String>,
        paused: std::result::Result<(), String>,
    },
    /// Refused too, with this message.
    Failed(String),
}

/// The `CREATE` / `INSERT` statement.
fn statement(w: &Wanted, replace: bool) -> String {
    let mut s = match w.stmt {
        Stmt::Insert => format!("INSERT INTO {}", w.name_sql),
        st => format!(
            "CREATE {}{} {}",
            if replace { "OR REPLACE " } else { "" },
            st.word(),
            w.name_sql
        ),
    };
    let with: Vec<String> = match (&w.query_id, w.stmt) {
        (Some(id), Stmt::Insert) => vec![format!("{QUERY_ID}={}", lit(id))],
        (_, Stmt::Insert) => Vec::new(),
        _ => w.with.iter().map(|(k, v)| format!("{k}={v}")).collect(),
    };
    if !with.is_empty() {
        let _ = write!(s, " WITH ({})", with.join(", "));
    }
    if w.stmt != Stmt::Insert {
        s.push_str(" AS");
    }
    s.push('\n');
    s.push_str(&w.select);
    // A closing line comment would swallow the `;`.
    if w.select
        .rsplit('\n')
        .next()
        .is_some_and(|l| l.contains("--"))
    {
        s.push('\n');
    }
    s.push(';');
    s
}

/// The script around the steps: a comment, the properties set before the
/// statement that takes them and unset after it, and the pause or resume.
fn render(
    head: &[String],
    steps: &[Step],
    props: &BTreeMap<String, String>,
    toggle: Option<(Toggle, &str)>,
) -> String {
    let mut out = String::new();
    for h in head {
        for line in h.lines() {
            let _ = writeln!(out, "-- {}", comment_text(line));
        }
    }
    for st in steps {
        if st.with_props {
            for (k, v) in props {
                let _ = writeln!(out, "SET {}={};", lit(k), lit(v));
            }
        }
        out.push_str(&st.sql);
        if !st.sql.ends_with(';') {
            out.push(';');
        }
        out.push('\n');
        if st.with_props {
            for k in props.keys() {
                let _ = writeln!(out, "UNSET {};", lit(k));
            }
        }
    }
    match toggle {
        Some((t, id)) if valid_id(id) => {
            let _ = writeln!(
                out,
                "{} {id};",
                if t == Toggle::Pause {
                    "PAUSE"
                } else {
                    "RESUME"
                }
            );
        }
        Some((Toggle::Pause, _)) => {
            out.push_str("-- Al terminar, DBine la pausa: PAUSE <id que le dé ksqlDB>;\n");
        }
        _ => {}
    }
    out
}

/// A new query.
pub(crate) fn create_plan(w: &Wanted) -> Plan {
    let steps = vec![Step {
        sql: statement(w, false),
        with_props: true,
    }];
    let toggle = (!w.enabled).then_some(Toggle::Pause);
    let head = vec![format!(
        "Consulta persistente nueva: {}",
        match w.stmt {
            Stmt::Stream => "CREATE STREAM … AS SELECT",
            Stmt::Table => "CREATE TABLE … AS SELECT",
            Stmt::Insert => "INSERT INTO … SELECT",
        }
    )];
    let id = w.query_id.clone();
    Plan {
        script: render(
            &head,
            &steps,
            &w.props,
            toggle.map(|t| (t, id.as_deref().unwrap_or(""))),
        ),
        steps,
        props: w.props.clone(),
        id,
        toggle,
        empty: false,
        restore: None,
    }
}

/// The SELECT compared without its blanks' shape.
fn same_text(a: &str, b: &str) -> bool {
    a.split_whitespace().eq(b.split_whitespace())
}

/// A change to the query `q` (its readers: the other queries that read
/// from its stream or table).
pub(crate) fn alter_plan(q: &Query, w: &Wanted, readers: &[String], pause: bool) -> Result<Plan> {
    let wanted_with = with_map(&w.with);
    let current_with = with_map(&q.def.with);
    let with_changed = q.def.stmt != Stmt::Insert && wanted_with != current_with;
    let select_changed = !same_text(&q.def.select, &w.select);
    let props_changed = q.props != w.props;
    let definition = with_changed || select_changed || props_changed || w.recreate;
    let toggle_wanted = match (q.paused, w.enabled) {
        (false, false) => Some(Toggle::Pause),
        (true, true) => Some(Toggle::Resume),
        _ => None,
    };
    if toggle_wanted.is_some() && !pause {
        return Err(Error::Unsupported(NO_PAUSE_NOTE.into()));
    }
    if !definition {
        let Some(t) = toggle_wanted else {
            return Ok(Plan {
                script: format!("-- {}: no hay cambios que guardar.\n", q.id),
                steps: Vec::new(),
                props: BTreeMap::new(),
                id: Some(q.id.clone()),
                toggle: None,
                empty: true,
                restore: None,
            });
        };
        let head = vec![format!(
            "Consulta persistente {}: {}",
            q.id,
            if t == Toggle::Pause {
                "se pausa (PAUSE): conserva su estado y su lugar en la entrada"
            } else {
                "se reanuda (RESUME) desde donde quedó"
            }
        )];
        return Ok(Plan {
            script: render(&head, &[], &BTreeMap::new(), Some((t, &q.id))),
            steps: Vec::new(),
            props: BTreeMap::new(),
            id: Some(q.id.clone()),
            toggle: Some(t),
            empty: false,
            restore: None,
        });
    }
    // A replaced or recreated query runs: paused only when asked.
    let toggle = (!w.enabled && pause).then_some(Toggle::Pause);
    let recreate = q.def.stmt == Stmt::Insert || with_changed || w.recreate;
    if !recreate {
        let steps = vec![Step {
            sql: statement(w, true),
            with_props: true,
        }];
        let head = vec![format!(
            "Consulta persistente {}: se reemplaza en el lugar (CREATE OR REPLACE); conserva su id, su estado y sus offsets.",
            q.id
        )];
        return Ok(Plan {
            script: render(&head, &steps, &w.props, toggle.map(|t| (t, q.id.as_str()))),
            steps,
            props: w.props.clone(),
            id: Some(q.id.clone()),
            toggle,
            empty: false,
            restore: None,
        });
    }
    if q.def.stmt != Stmt::Insert && !readers.is_empty() {
        return Err(bad(format!(
            "Otras consultas leen de {} ({}): para recrear esta, terminalas antes.",
            q.def.sink,
            readers.join(", ")
        )));
    }
    let mut steps = vec![Step {
        sql: format!("TERMINATE {};", q.id),
        with_props: false,
    }];
    let mut head = Vec::new();
    if q.def.stmt == Stmt::Insert {
        head.push(format!(
            "ksqlDB no cambia una INSERT INTO en el lugar: se termina {} y se crea de nuevo con el mismo id.",
            q.id
        ));
    } else {
        head.push(format!(
            "Se recrea la consulta persistente {}: TERMINATE, DROP {} (el topic queda) y CREATE.",
            q.id,
            q.def.stmt.word()
        ));
        steps.push(Step {
            sql: format!("DROP {} {};", q.def.stmt.word(), server_name(&q.def.sink)),
            with_props: false,
        });
        if let (Some(old), Some(new)) = (
            current_with.get("KAFKA_TOPIC"),
            wanted_with.get("KAFKA_TOPIC"),
        ) {
            if old != new {
                head.push(format!(
                    "El topic {old} queda con lo que tenía; la consulta nueva escribe en {new}."
                ));
            }
        }
    }
    head.push(
        "Se pierden su estado y sus offsets: la consulta nueva lee su entrada otra vez desde auto.offset.reset.".into(),
    );
    head.push(
        "Si ksqlDB rechaza la sentencia nueva, DBine vuelve a crear la consulta anterior como estaba (también sin su estado ni sus offsets).".into(),
    );
    let drop = (steps.len() > 1).then_some(1);
    steps.push(Step {
        sql: statement(w, false),
        with_props: true,
    });
    let id = (q.def.stmt == Stmt::Insert).then(|| q.id.clone());
    Ok(Plan {
        script: render(
            &head,
            &steps,
            &w.props,
            toggle.map(|t| (t, id.as_deref().unwrap_or(""))),
        ),
        steps,
        props: w.props.clone(),
        id,
        toggle,
        empty: false,
        restore: Some(Restore::of(q, drop)),
    })
}

pub(crate) fn delete_script(q: &Query) -> String {
    // The sink's name comes from the server: on one line in the comments.
    format!(
        "-- Termina la consulta persistente {id}: se borra para siempre. {kind} {sink} y su topic quedan; para borrarlos también:\n--   DROP {kind} {quoted} DELETE TOPIC;\nTERMINATE {id};\n",
        id = q.id,
        kind = q.def.stmt.word(),
        sink = comment_text(&q.def.sink),
        quoted = comment_text(&server_name(&q.def.sink)),
    )
}

// ---------------------------------------------------------------------------
// The form
// ---------------------------------------------------------------------------

/// Each property as a row of the properties list.
fn prop_rows(props: &BTreeMap<String, String>) -> Vec<BTreeMap<String, Value>> {
    props
        .iter()
        .filter(|(k, _)| *k != "auto.offset.reset")
        .map(|(k, v)| {
            BTreeMap::from([
                ("name".to_string(), json!(k)),
                ("value".to_string(), json!(v)),
            ])
        })
        .collect()
}

/// The form for a query (`None`: a new one). `streams`: the streams an
/// `INSERT INTO` may write to; `pause`: the server has PAUSE.
pub(crate) fn form_of(q: Option<&Query>, streams: &[String], pause: bool) -> ObjectForm {
    let existing = q.is_some();
    let mut f = ObjectForm::default();
    let mut values: BTreeMap<String, Value> = BTreeMap::new();
    let stmt = q.map_or(Stmt::Stream, |q| q.def.stmt);
    values.insert("statement_kind".into(), json!(stmt.key()));
    values.insert("enabled".into(), json!(q.is_none_or(|q| !q.paused)));
    values.insert("recreate".into(), json!(false));
    let mut general = FormSection::new("general", "General").fields(vec![
        FormField::new(
            "statement_kind",
            "Tipo",
            FormKind::select(&[
                ("create_stream", "CREATE STREAM … AS SELECT"),
                ("create_table", "CREATE TABLE … AS SELECT"),
                ("insert", "INSERT INTO … SELECT"),
            ]),
        )
        .required()
        .read_only(existing)
        .default(json!("create_stream"))
        .help("Un CREATE … AS SELECT crea su stream o tabla; una INSERT INTO escribe en un stream que ya existe."),
        FormField::new("name", "Stream o tabla que crea", FormKind::Text)
            .required()
            .read_only(existing)
            .when("statement_kind", &["create_stream", "create_table"])
            .placeholder("pedidos_por_cliente")
            .help("Un nombre sin comillas queda en mayúsculas, como lo lee ksqlDB."),
        FormField::new("target", "Stream en el que escribe", FormKind::reference("stream", false))
            .required()
            .read_only(existing)
            .when("statement_kind", &["insert"]),
        FormField::new("query_id", "Id de la consulta", FormKind::Text)
            .read_only(existing)
            .when("statement_kind", &["insert"])
            .placeholder("INSERT_PEDIDOS")
            .help("Opcional: el id con el que ksqlDB la lista (QUERY_ID). Sin él, ksqlDB le pone uno."),
        FormField::new("enabled", "Habilitada", FormKind::Bool)
            .read_only(!pause)
            .default(json!(true))
            .help("Deshabilitarla la pausa (PAUSE): conserva su estado y su lugar en la entrada, y sigue desde ahí al reanudarla."),
    ]);
    if existing {
        general = general.field(
            FormField::new("recreate", "Recrear la consulta", FormKind::Bool)
                .when("statement_kind", &["create_stream", "create_table"])
                .default(json!(false))
                .help("Para un cambio que ksqlDB no acepta en el lugar: termina la consulta, borra su stream o tabla (el topic queda) y la crea de nuevo. Se pierden su estado y sus offsets."),
        );
    }
    let query = FormSection::new("query", "Consulta").fields(vec![FormField::new(
        "select",
        "SELECT",
        FormKind::code("sql"),
    )
    .required()
    .placeholder("SELECT cliente, COUNT(*) AS pedidos FROM pedidos GROUP BY cliente EMIT CHANGES")
    .help("Solo la consulta: DBine escribe el CREATE o el INSERT INTO con el destino de este formulario.")]);
    let mut dest_fields = Vec::new();
    for (key, field, kind) in WITH_PROPS {
        let label_kind = match kind {
            Prop::Text => FormKind::Text,
            Prop::Int(min, _) => FormKind::number(Some(min as f64), None),
            Prop::Format => format_select(),
            Prop::Delimiter => FormKind::Text,
            Prop::Flag => FormKind::select(&[
                ("", "El valor por defecto del servidor"),
                ("true", "Sí"),
                ("false", "No"),
            ]),
        };
        let f = match key {
            "KAFKA_TOPIC" => FormField::new(field, "Topic de Kafka", label_kind)
                .help("Sin él, ksqlDB usa el nombre del stream o la tabla."),
            "PARTITIONS" => FormField::new(field, "Particiones", label_kind),
            "REPLICAS" => FormField::new(field, "Réplicas", label_kind),
            "RETENTION_MS" => FormField::new(field, "Retención del topic (ms)", label_kind)
                .help("-1: para siempre."),
            "KEY_FORMAT" => FormField::new(field, "Formato de la clave", label_kind),
            "VALUE_FORMAT" => FormField::new(field, "Formato del valor", label_kind),
            "KEY_DELIMITER" => FormField::new(field, "Separador de la clave", label_kind)
                .when("key_format", &["DELIMITED"])
                .help("Un carácter, TAB o SPACE."),
            "VALUE_DELIMITER" => FormField::new(field, "Separador del valor", label_kind)
                .when("value_format", &["DELIMITED"])
                .help("Un carácter, TAB o SPACE."),
            "TIMESTAMP" => FormField::new(field, "Columna de tiempo", label_kind)
                .help("La columna que ksqlDB usa como tiempo del evento (ROWTIME)."),
            "TIMESTAMP_FORMAT" => {
                FormField::new(field, "Formato de la columna de tiempo", label_kind)
                    .placeholder("yyyy-MM-dd HH:mm:ss")
            }
            "WRAP_SINGLE_VALUE" => FormField::new(field, "Envolver un valor único", label_kind),
            "KEY_SCHEMA_ID" => FormField::new(field, "Id del esquema de la clave", label_kind),
            "VALUE_SCHEMA_ID" => FormField::new(field, "Id del esquema del valor", label_kind),
            "KEY_SCHEMA_FULL_NAME" => {
                FormField::new(field, "Nombre completo del esquema de la clave", label_kind)
            }
            _ => FormField::new(field, "Nombre completo del esquema del valor", label_kind),
        };
        dest_fields.push(f.when("statement_kind", &["create_stream", "create_table"]));
    }
    let mut extra_rows = Vec::new();
    let mut offset = "earliest".to_string();
    let mut prop_list_rows = Vec::new();
    if let Some(q) = q {
        values.insert("name".into(), json!(q.def.sink));
        values.insert("target".into(), json!(q.def.sink));
        values.insert("select".into(), json!(q.def.select));
        values.insert("statement".into(), json!(q.statement));
        if q.def.stmt == Stmt::Insert {
            values.insert("query_id".into(), json!(q.id));
        }
        for (k, v) in &q.def.with {
            if let Some((_, field, _)) = WITH_PROPS.iter().find(|(key, _, _)| key == k) {
                let value = unquote(v);
                let n = value.parse::<i64>().ok();
                values.insert(
                    field.to_string(),
                    match n {
                        Some(n) if v.trim().parse::<i64>().is_ok() => json!(n),
                        _ => json!(value),
                    },
                );
            } else if k != CLEANUP_POLICY && k != QUERY_ID {
                extra_rows.push(BTreeMap::from([
                    ("key".to_string(), json!(k)),
                    ("value".to_string(), json!(unquote(v))),
                ]));
            } else if k == CLEANUP_POLICY {
                values.insert("cleanup_policy".into(), json!(unquote(v)));
            }
        }
        offset = q
            .props
            .get("auto.offset.reset")
            .cloned()
            .unwrap_or_default();
        prop_list_rows = prop_rows(&q.props);
    } else {
        values.insert("value_format".into(), json!("JSON"));
    }
    values.insert("offset_reset".into(), json!(offset));
    if existing {
        dest_fields.push(
            FormField::new(
                "cleanup_policy",
                "Política de limpieza del topic",
                FormKind::Text,
            )
            .read_only(true)
            .when("statement_kind", &["create_stream", "create_table"])
            .help("ksqlDB la elige por el tipo: delete para un stream, compact para una tabla."),
        );
    }
    // An INSERT INTO writes into a stream that exists: it has no WITH of
    // its own, so the tab shows only for a CREATE … AS SELECT.
    let dest = FormSection::new("destination", "Destino")
        .when("statement_kind", &["create_stream", "create_table"])
        .fields(dest_fields)
        .list(
            FormList::new(
                "with",
                "Otras propiedades de WITH",
                "key",
                vec![
                    FormField::new("key", "Propiedad", FormKind::Text)
                        .required()
                        .placeholder("VALUE_PROTOBUF_NULLABLE_REPRESENTATION"),
                    FormField::new("value", "Valor", FormKind::Text),
                ],
            )
            .when("statement_kind", &["create_stream", "create_table"])
            .rows(extra_rows),
        );
    let props = FormSection::new("properties", "Propiedades")
        .field(
            FormField::new(
                "offset_reset",
                "Leer desde",
                FormKind::select(&[
                    ("", "El valor por defecto del servidor"),
                    ("earliest", "El principio de la entrada (earliest)"),
                    ("latest", "Solo lo nuevo (latest)"),
                ]),
            )
            .default(json!("earliest"))
            .help("auto.offset.reset: desde dónde lee su entrada una consulta nueva o recreada. Un reemplazo en el lugar sigue desde donde estaba."),
        )
        .list(
            FormList::new(
                "properties",
                "Propiedades de la consulta",
                "name",
                vec![
                    FormField::new("name", "Propiedad", FormKind::Text)
                        .required()
                        .placeholder("processing.guarantee"),
                    FormField::new("value", "Valor", FormKind::Text).placeholder("exactly_once_v2"),
                ],
            )
            .rows(prop_list_rows),
        );
    let mut sections = vec![general, query, dest, props];
    if let Some(q) = q {
        sections.push(state_section(q, &mut values));
    }
    f.sections = sections;
    f.values = values;
    f.choices.insert(
        "stream".into(),
        streams.iter().map(|s| (s.clone(), s.clone())).collect(),
    );
    if let Some(q) = q {
        f.notes.push(
            if q.def.stmt == Stmt::Insert {
                INSERT_NOTE
            } else {
                REPLACE_NOTE
            }
            .into(),
        );
    }
    f.notes.push(PROPS_NOTE.into());
    if !pause {
        f.notes.push(NO_PAUSE_NOTE.into());
    }
    f
}

/// What the server says of the query now, shown only.
fn state_section(q: &Query, values: &mut BTreeMap<String, Value>) -> FormSection {
    let d = &q.description;
    let mut put = |k: &str, v: String| {
        values.insert(k.into(), json!(v));
    };
    put("state_id", q.id.clone());
    let hosts: Vec<String> = d
        .get("ksqlHostQueryStatus")
        .and_then(Value::as_object)
        .map(|m| m.iter().map(|(h, s)| format!("{h}: {}", text(s))).collect())
        .unwrap_or_default();
    put(
        "state_hosts",
        if hosts.is_empty() {
            d.get("state").map(text).unwrap_or_default()
        } else {
            hosts.join(", ")
        },
    );
    put("state_sources", q.sources.join(", "));
    put("state_sinks", strings(d, "sinks").join(", "));
    put(
        "state_group",
        d.get("consumerGroupId").map(text).unwrap_or_default(),
    );
    let columns: Vec<String> = d
        .get("fields")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .map(|f| {
            let key = matches!(
                f.get("type").and_then(Value::as_str),
                Some("KEY" | "PRIMARY_KEY")
            );
            format!(
                "{} {}{}",
                text(&f["name"]),
                f.get("schema").map(schema_type).unwrap_or_default(),
                if key { " KEY" } else { "" }
            )
        })
        .collect();
    put("state_columns", columns.join(", "));
    put(
        "state_window",
        d.get("windowType").map(text).unwrap_or_default(),
    );
    let last = errors_of(&[d]).into_iter().next();
    put(
        "state_error",
        last.map(|(_, _, _, m)| m.chars().take(4000).collect())
            .unwrap_or_default(),
    );
    FormSection::new("state", "Estado").fields(vec![
        FormField::new("state_id", "Id", FormKind::Text).read_only(true),
        FormField::new("state_hosts", "Estado en cada servidor", FormKind::Text).read_only(true),
        FormField::new("state_sources", "Lee de", FormKind::Text).read_only(true),
        FormField::new("state_sinks", "Escribe en", FormKind::Text).read_only(true),
        FormField::new("state_group", "Grupo de consumidores", FormKind::Text).read_only(true),
        FormField::new("state_columns", "Columnas que escribe", FormKind::Textarea).read_only(true),
        FormField::new("state_window", "Ventana", FormKind::Text).read_only(true),
        FormField::new("state_error", "Último error", FormKind::Textarea).read_only(true),
    ])
}

// ---------------------------------------------------------------------------
// The server
// ---------------------------------------------------------------------------

/// A command's result: its sequence number (for the next statement) and
/// the query it created.
#[derive(Debug, Default)]
struct Ran {
    seq: Option<i64>,
    query_id: Option<String>,
}

/// `stream/`X`/create` as a `/status/…` path: each segment encoded.
fn status_path(command_id: &str) -> String {
    let segs: Vec<String> = command_id
        .split('/')
        .map(|seg| {
            seg.bytes()
                .map(|b| {
                    if b.is_ascii_alphanumeric() || b"-_.~".contains(&b) {
                        (b as char).to_string()
                    } else {
                        format!("%{b:02X}")
                    }
                })
                .collect()
        })
        .collect();
    format!("/status/{}", segs.join("/"))
}

/// "Created query with ID CSAS_X_3" → `CSAS_X_3`.
fn created_id(message: &str) -> Option<String> {
    message
        .rsplit_once("with ID ")
        .map(|(_, id)| id.trim().trim_end_matches('.').to_string())
        .filter(|id| valid_id(id))
}

impl KsqlSession {
    /// One statement with exactly these properties (not the session's),
    /// after the command `seq`; waits for its command to finish.
    async fn command(
        &mut self,
        stmt: &str,
        props: &BTreeMap<String, String>,
        seq: Option<i64>,
    ) -> Result<Ran> {
        let mut body = json!({
            "ksql": stmt,
            "streamsProperties": Value::Object(props.iter().map(|(k, v)| (k.clone(), Value::String(v.clone()))).collect::<Map<_, _>>()),
        });
        if let Some(n) = seq {
            body["commandSequenceNumber"] = json!(n);
        }
        let resp = self
            .conn
            .post("/ksql")
            .header("Content-Type", KSQL_JSON)
            .header("Accept", KSQL_JSON)
            .body(body.to_string())
            .send()
            .await
            .map_err(http_error)?;
        let v: Value = check(resp).await?.json().await.map_err(Error::query)?;
        let ents = match v {
            Value::Array(a) => a,
            v => vec![v],
        };
        let mut ran = Ran::default();
        for e in &ents {
            if e.get("@type").and_then(Value::as_str) == Some("warning_entity") {
                return Err(bad(e.get("message").map(text).unwrap_or_default()));
            }
            ran.seq = e
                .get("commandSequenceNumber")
                .and_then(Value::as_i64)
                .or(ran.seq);
            let status = e.get("commandStatus");
            let state = status
                .and_then(|s| s.get("status"))
                .map(text)
                .unwrap_or_default();
            let mut message = status
                .and_then(|s| s.get("message"))
                .map(text)
                .unwrap_or_default();
            ran.query_id = status
                .and_then(|s| s.get("queryId"))
                .map(text)
                .filter(|q| valid_id(q))
                .or_else(|| created_id(&message))
                .or(ran.query_id);
            let mut state = state;
            if matches!(state.as_str(), "QUEUED" | "PARSING" | "EXECUTING") {
                if let Some(cid) = e.get("commandId").map(text).filter(|c| !c.is_empty()) {
                    let start = Instant::now();
                    while matches!(state.as_str(), "QUEUED" | "PARSING" | "EXECUTING")
                        && start.elapsed() < COMMAND_WAIT
                    {
                        tokio::time::sleep(Duration::from_millis(500)).await;
                        match self.get_json(&status_path(&cid)).await {
                            Ok(st) => {
                                state = st.get("status").map(text).unwrap_or_default();
                                message = st.get("message").map(text).unwrap_or(message);
                                ran.query_id = st
                                    .get("queryId")
                                    .map(text)
                                    .filter(|q| valid_id(q))
                                    .or_else(|| created_id(&message))
                                    .or(ran.query_id);
                            }
                            Err(e) => {
                                tracing::debug!("ksqlDB command status: {e}");
                            }
                        }
                    }
                }
            }
            if matches!(state.as_str(), "ERROR" | "TERMINATED") {
                return Err(bad(message));
            }
        }
        Ok(ran)
    }

    async fn all_descriptions(&mut self) -> Result<Vec<Value>> {
        let ents = self.ksql("SHOW QUERIES EXTENDED").await?;
        Ok(descriptions(&ents).into_iter().cloned().collect())
    }

    async fn persistent(&mut self, id: &str) -> Result<(Query, Vec<Value>)> {
        if !valid_id(id) {
            return Err(bad(format!(
                "«{}» no es un id de consulta persistente de ksqlDB",
                shown(id)
            )));
        }
        let all = self.all_descriptions().await?;
        let q = all
            .iter()
            .find(|q| query_id(q).as_deref() == Some(id))
            .ok_or_else(|| bad(format!("no existe la consulta persistente {id}")))?;
        let parsed = query_of(q).ok_or_else(|| {
            bad(format!(
                "DBine no pudo leer la sentencia de la consulta {id}: se ve en su detalle, pero no se edita desde el diseñador."
            ))
        })?;
        Ok((parsed, all))
    }

    async fn stream_names(&mut self) -> Result<Vec<String>> {
        let mut names = self.names("SHOW STREAMS", "streams").await?;
        names.retain(|n| n != PROCESSING_LOG && !n.starts_with('_'));
        names.sort();
        Ok(names)
    }

    async fn pause_allowed(&self) -> bool {
        can_pause(&self.version().await)
    }

    pub(crate) async fn scheduler_info_impl(&mut self) -> Result<SchedulerInfo> {
        let pause = self.pause_allowed().await;
        let mut notes = Vec::new();
        if !pause {
            notes.push(NO_PAUSE_NOTE.to_string());
        }
        notes.push(LOG_NOTE.to_string());
        Ok(SchedulerInfo {
            kinds: vec![SchedulerKind::new(
                "job",
                "Consultas persistentes",
                "consulta persistente",
            )
            .abilities(true, true, true)],
            logs: true,
            notes,
        })
    }

    pub(crate) async fn scheduler_items_impl(&mut self, kind: &str) -> Result<SchedulerList> {
        kind_check(kind)?;
        let pause = self.pause_allowed().await;
        let items = self
            .all_descriptions()
            .await?
            .iter()
            .filter_map(|q| item_of(q, pause))
            .collect();
        Ok(SchedulerList {
            items,
            notes: Vec::new(),
        })
    }

    pub(crate) async fn scheduler_form_impl(
        &mut self,
        kind: &str,
        id: Option<&str>,
    ) -> Result<ObjectForm> {
        kind_check(kind)?;
        let pause = self.pause_allowed().await;
        let streams = self.stream_names().await?;
        match id {
            None => Ok(form_of(None, &streams, pause)),
            Some(id) => {
                let (q, _) = self.persistent(id.trim()).await?;
                Ok(form_of(Some(&q), &streams, pause))
            }
        }
    }

    async fn plan(&mut self, id: Option<&str>, values: &FormValues) -> Result<Plan> {
        let pause = self.pause_allowed().await;
        match id {
            None => {
                let streams = self.stream_names().await?;
                let w = wanted(values, None, Some(&streams))?;
                if let Some(qid) = &w.query_id {
                    let taken = self
                        .all_descriptions()
                        .await?
                        .iter()
                        .any(|q| query_id(q).is_some_and(|x| x.eq_ignore_ascii_case(qid)));
                    if taken {
                        return Err(bad(format!("Ya hay una consulta con el id {qid}.")));
                    }
                }
                if !w.enabled && !pause {
                    return Err(Error::Unsupported(NO_PAUSE_NOTE.into()));
                }
                Ok(create_plan(&w))
            }
            Some(id) => {
                let (q, all) = self.persistent(id.trim()).await?;
                let seen = Vals(&values.values).opt("statement");
                if seen.is_some_and(|s| s != q.statement) {
                    return Err(bad(STALE.into()));
                }
                let w = wanted(values, Some(&q), None)?;
                let readers: Vec<String> = all
                    .iter()
                    .filter_map(query_of)
                    .filter(|o| o.id != q.id)
                    .filter(|o| {
                        o.sources
                            .iter()
                            .any(|s| s.eq_ignore_ascii_case(&q.def.sink))
                    })
                    .map(|o| o.id)
                    .collect();
                alter_plan(&q, &w, &readers, pause)
            }
        }
    }

    pub(crate) async fn scheduler_script_impl(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<String> {
        kind_check(kind)?;
        Ok(self.plan(id, values).await?.script)
    }

    pub(crate) async fn scheduler_save_impl(
        &mut self,
        kind: &str,
        id: Option<&str>,
        values: &FormValues,
    ) -> Result<Option<String>> {
        kind_check(kind)?;
        let plan = self.plan(id, values).await?;
        if plan.empty {
            return Ok(plan.id);
        }
        let none = BTreeMap::new();
        let mut seq = None;
        let mut made = None;
        for (i, st) in plan.steps.iter().enumerate() {
            let ran = match self
                .command(
                    &st.sql,
                    if st.with_props { &plan.props } else { &none },
                    seq,
                )
                .await
            {
                Ok(ran) => ran,
                // The old query is gone (its TERMINATE ran): put it back.
                Err(e) => match plan.restore.as_ref().filter(|_| i > 0) {
                    Some(r) => {
                        let restored = self.restore(r, i, seq).await;
                        return Err(bad(r.message(i, &e.to_string(), &restored)));
                    }
                    None => return Err(e),
                },
            };
            seq = ran.seq.or(seq);
            if st.with_props {
                made = ran.query_id;
            }
        }
        let id = plan.id.clone().or(made);
        if let (Some(t), Some(qid)) = (plan.toggle, id.as_deref()) {
            if valid_id(qid) {
                let stmt = if t == Toggle::Pause {
                    "PAUSE"
                } else {
                    "RESUME"
                };
                self.command(&format!("{stmt} {qid};"), &none, seq).await?;
            }
        }
        Ok(id)
    }

    /// Makes the old query of a recreate again, after step `failed` was
    /// refused, with the properties it had, and pauses it when it was.
    async fn restore(&mut self, r: &Restore, failed: usize, seq: Option<i64>) -> Restored {
        let ran = match self.command(&r.statement(failed), &r.old.props, seq).await {
            Ok(ran) => ran,
            Err(e) => return Restored::Failed(e.to_string()),
        };
        let id = r.old.query_id.clone().or(ran.query_id);
        let mut paused = Ok(());
        if r.paused {
            paused = match id.as_deref().filter(|q| valid_id(q)) {
                Some(qid) => self
                    .command(&format!("PAUSE {qid};"), &BTreeMap::new(), ran.seq.or(seq))
                    .await
                    .map(|_| ())
                    .map_err(|e| e.to_string()),
                None => Err("ksqlDB no dijo el id de la consulta.".into()),
            };
        }
        Restored::Made { id, paused }
    }

    pub(crate) async fn scheduler_delete_script_impl(
        &mut self,
        kind: &str,
        id: &str,
    ) -> Result<String> {
        kind_check(kind)?;
        let (q, _) = self.persistent(id.trim()).await?;
        Ok(delete_script(&q))
    }

    pub(crate) async fn scheduler_delete_impl(&mut self, kind: &str, id: &str) -> Result<()> {
        kind_check(kind)?;
        let (q, _) = self.persistent(id.trim()).await?;
        // The id has the shape `valid_id` checked.
        self.command(&format!("TERMINATE {};", q.id), &BTreeMap::new(), None)
            .await
            .map(|_| ())
    }

    /// The errors each persistent query kept, newest first.
    pub(crate) async fn scheduler_logs_impl(&mut self) -> Result<Vec<MonitorTable>> {
        let all = self.all_descriptions().await?;
        let refs: Vec<&Value> = all.iter().collect();
        let mut t = MonitorTable::new(
            "query_errors",
            "Errores recientes de las consultas persistentes (en la memoria del servidor)",
            &["Fecha", "Consulta", "Tipo", "Mensaje"],
        );
        t.rows = errors_of(&refs)
            .into_iter()
            .map(|(ms, id, kind, message)| {
                vec![
                    json!(if ms > 0 {
                        crate::jobs::utc_ms(ms)
                    } else {
                        String::new()
                    }),
                    json!(id),
                    json!(kind),
                    json!(message.chars().take(4000).collect::<String>()),
                ]
            })
            .collect();
        Ok(vec![t])
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fv(values: Value, lists: Value) -> FormValues {
        serde_json::from_value(json!({"values": values, "lists": lists})).unwrap()
    }

    const CSAS: &str = "CREATE OR REPLACE STREAM DBINE_TEST_SCHED_OUT WITH (CLEANUP_POLICY='delete', KAFKA_TOPIC='dbine_test_sched_out', PARTITIONS=1, REPLICAS=1, RETENTION_MS=604800000, VALUE_FORMAT='JSON') AS SELECT\n  S.ID ID,\n  S.V V\nFROM S S\nWHERE (S.N > 0)\nEMIT CHANGES;";

    fn desc(id: &str, statement: &str, sinks: &[&str], props: Value, state: &str) -> Value {
        json!({"id": id, "queryType": "PERSISTENT", "statementText": statement, "sinks": sinks,
               "sources": ["S"], "overriddenProperties": props, "state": state,
               "ksqlHostQueryStatus": {"h:8088": state}, "consumerGroupId": "g",
               "fields": [{"name": "ID", "schema": {"type": "INTEGER"}, "type": "KEY"}, {"name": "V", "schema": {"type": "STRING"}}],
               "queryErrors": [{"errorMessage": "boom", "timestamp": 1794499800000_i64, "type": "USER"}]})
    }

    fn csas() -> Query {
        query_of(&desc(
            "CSAS_DBINE_TEST_SCHED_OUT_855",
            CSAS,
            &["DBINE_TEST_SCHED_OUT"],
            json!({"auto.offset.reset": "earliest", "processing.guarantee": "at_least_once"}),
            "RUNNING",
        ))
        .unwrap()
    }

    fn insert() -> Query {
        query_of(&desc(
            "DBINE_TEST_SCHED_Q1",
            "INSERT INTO dbine_test_sched_ins WITH (QUERY_ID='DBINE_TEST_SCHED_Q1') SELECT id, v FROM s EMIT CHANGES;",
            &["DBINE_TEST_SCHED_INS"],
            json!({}),
            "PAUSED",
        ))
        .unwrap()
    }

    /// The form's values for a query, as the app sends them back.
    fn values_of(q: &Query) -> FormValues {
        let f = form_of(Some(q), &[], true);
        let mut lists = BTreeMap::new();
        for s in &f.sections {
            for l in &s.lists {
                lists.insert(l.key.clone(), l.rows.clone());
            }
        }
        FormValues {
            values: f.values,
            lists,
        }
    }

    #[test]
    fn statements_are_taken_apart() {
        let d = parse(CSAS).unwrap();
        assert_eq!(d.stmt, Stmt::Stream);
        assert_eq!(d.sink, "DBINE_TEST_SCHED_OUT");
        assert_eq!(d.with[0], ("CLEANUP_POLICY".into(), "'delete'".into()));
        assert_eq!(d.with[2], ("PARTITIONS".into(), "1".into()));
        assert!(d.select.starts_with("SELECT\n  S.ID ID") && d.select.ends_with("EMIT CHANGES"));
        let d = parse("insert into `my ``odd`` s` with (QUERY_ID='Q') select * from s;").unwrap();
        assert_eq!((d.stmt, d.sink.as_str()), (Stmt::Insert, "my `odd` s"));
        assert_eq!(d.with, vec![("QUERY_ID".into(), "'Q'".into())]);
        let d = parse(
            "CREATE TABLE IF NOT EXISTS t AS SELECT v, COUNT(*) c FROM s GROUP BY v EMIT CHANGES",
        )
        .unwrap();
        assert_eq!((d.stmt, d.sink.as_str()), (Stmt::Table, "T"));
        let d = parse(
            "CREATE STREAM x WITH (KAFKA_TOPIC='a,b(', VALUE_DELIMITER=',') AS SELECT 1 FROM s",
        )
        .unwrap();
        assert_eq!(d.with[0].1, "'a,b('");
        assert_eq!(d.with[1].1, "','");
        for bad in [
            "SELECT 1",
            "CREATE STREAM x (id INT) WITH (KAFKA_TOPIC='t')",
            "DROP STREAM x",
            "CREATE OR STREAM x AS SELECT 1",
            "",
        ] {
            assert!(parse(bad).is_none(), "{bad:?}");
        }
    }

    #[test]
    fn the_select_is_one_statement() {
        assert_eq!(
            one_select("SELECT * FROM s EMIT CHANGES;").unwrap(),
            "SELECT * FROM s EMIT CHANGES"
        );
        assert_eq!(
            one_select("  select ';' AS x FROM s -- a; b\n EMIT CHANGES ;  -- end").unwrap(),
            "select ';' AS x FROM s -- a; b\n EMIT CHANGES"
        );
        assert_eq!(
            one_select("SELECT `a;b` FROM s /* ; */").unwrap(),
            "SELECT `a;b` FROM s /* ; */"
        );
        for bad in [
            "",
            "DROP STREAM s",
            "SELECT 1 FROM s; DROP STREAM s",
            "SELECT 1 FROM s;\nTERMINATE CSAS_X_1;",
            "SELECT 'x FROM s",
            "SELECT 1 /* x",
            "CREATE STREAM a AS SELECT 1 FROM s",
        ] {
            assert!(one_select(bad).is_err(), "{bad:?}");
        }
    }

    #[test]
    fn a_new_stream_query() {
        let v = fv(
            json!({"statement_kind": "create_stream", "name": "pedidos_x", "select": "SELECT id, v FROM s EMIT CHANGES",
                   "kafka_topic": "ped'idos", "partitions": 3, "replicas": "1", "value_format": "json", "key_format": "KAFKA",
                   "wrap_single_value": "false", "offset_reset": "earliest", "enabled": false}),
            json!({"properties": [{"name": "processing.guarantee", "value": "exactly_once_v2"}],
                   "with": [{"key": "value_protobuf_nullable_representation", "value": "OPTIONAL"}]}),
        );
        let w = wanted(&v, None, Some(&[])).unwrap();
        let p = create_plan(&w);
        assert_eq!(
            p.steps[0].sql,
            "CREATE STREAM pedidos_x WITH (KAFKA_TOPIC='ped''idos', PARTITIONS=3, REPLICAS=1, KEY_FORMAT='KAFKA', VALUE_FORMAT='JSON', WRAP_SINGLE_VALUE=false, VALUE_PROTOBUF_NULLABLE_REPRESENTATION='OPTIONAL') AS\nSELECT id, v FROM s EMIT CHANGES;"
        );
        assert_eq!(p.toggle, Some(Toggle::Pause));
        assert!(p
            .script
            .starts_with("-- Consulta persistente nueva: CREATE STREAM"));
        assert!(p.script.contains("SET 'auto.offset.reset'='earliest';\nSET 'processing.guarantee'='exactly_once_v2';\nCREATE STREAM"));
        assert!(p.script.contains(
            "EMIT CHANGES;\nUNSET 'auto.offset.reset';\nUNSET 'processing.guarantee';\n"
        ));
        assert!(p.script.contains("PAUSE <id"));
        // An odd name is quoted; a plain one is read in upper case by ksqlDB.
        let mut v2 = v.clone();
        v2.values.insert("name".into(), json!("my stream"));
        assert!(create_plan(&wanted(&v2, None, Some(&[])).unwrap()).steps[0]
            .sql
            .starts_with("CREATE STREAM `my stream` WITH"));
        v2.values.insert("name".into(), json!("select"));
        assert!(create_plan(&wanted(&v2, None, Some(&[])).unwrap()).steps[0]
            .sql
            .starts_with("CREATE STREAM `select`"));
    }

    #[test]
    fn a_new_insert_into() {
        let v = fv(
            json!({"statement_kind": "insert", "target": "DBINE_INS", "query_id": "my_q", "select": "SELECT * FROM s EMIT CHANGES", "offset_reset": ""}),
            json!({}),
        );
        let streams = vec!["DBINE_INS".to_string()];
        let w = wanted(&v, None, Some(&streams)).unwrap();
        let p = create_plan(&w);
        assert_eq!(
            p.steps[0].sql,
            "INSERT INTO `DBINE_INS` WITH (QUERY_ID='MY_Q')\nSELECT * FROM s EMIT CHANGES;"
        );
        assert_eq!(p.id.as_deref(), Some("MY_Q"));
        assert!(p.toggle.is_none() && p.props.is_empty());
        // Only into a stream the server has, with a clean id.
        let mut bad = v.clone();
        bad.values.insert("target".into(), json!("NOPE"));
        assert!(wanted(&bad, None, Some(&streams)).is_err());
        let mut bad = v.clone();
        bad.values
            .insert("query_id".into(), json!("x; DROP STREAM s"));
        assert!(wanted(&bad, None, Some(&streams)).is_err());
    }

    #[test]
    fn values_are_checked() {
        let base = json!({"statement_kind": "create_table", "name": "t", "select": "SELECT v, COUNT(*) c FROM s GROUP BY v EMIT CHANGES"});
        let try_with = |k: &str, v: Value| {
            let mut vals = base.clone();
            vals[k] = v;
            wanted(&fv(vals, json!({})), None, Some(&[]))
        };
        assert!(try_with("partitions", json!(0)).is_err());
        assert!(try_with("partitions", json!("x")).is_err());
        assert!(try_with("value_format", json!("XML")).is_err());
        assert!(try_with("value_delimiter", json!(",,")).is_err());
        assert!(try_with("value_delimiter", json!("TAB")).is_ok());
        assert!(try_with("kafka_topic", json!("a\nb")).is_err());
        assert!(try_with("wrap_single_value", json!("maybe")).is_err());
        assert!(try_with("offset_reset", json!("middle")).is_err());
        assert!(try_with("statement_kind", json!("drop")).is_err());
        assert!(try_with("name", json!("")).is_err());
        let with_list =
            |rows: Value| wanted(&fv(base.clone(), json!({"with": rows})), None, Some(&[]));
        assert!(with_list(json!([{"key": "CLEANUP_POLICY", "value": "delete"}])).is_err());
        assert!(with_list(json!([{"key": "A=1) AS SELECT", "value": "x"}])).is_err());
        assert!(with_list(
            json!([{"key": "PARTITIONS", "value": "2"}, {"key": "PARTITIONS", "value": "2"}])
        )
        .is_err());
        let props = |rows: Value| {
            wanted(
                &fv(base.clone(), json!({"properties": rows})),
                None,
                Some(&[]),
            )
        };
        assert!(props(json!([{"name": "a b", "value": "x"}])).is_err());
        assert!(props(json!([{"name": "auto.offset.reset", "value": "latest"}])).is_err());
        assert!(
            props(json!([{"name": "x.y", "value": "1"}, {"name": "x.y", "value": "2"}])).is_err()
        );
        assert!(props(json!([{"name": "", "value": ""}])).is_ok());
    }

    #[test]
    fn an_untouched_form_changes_nothing() {
        for q in [csas(), insert()] {
            let v = values_of(&q);
            let w = wanted(&v, Some(&q), None).unwrap();
            let p = alter_plan(&q, &w, &[], true).unwrap();
            assert!(p.empty, "{}: {}", q.id, p.script);
            assert!(p.script.contains("no hay cambios"));
        }
    }

    #[test]
    fn a_new_select_replaces_in_place() {
        let q = csas();
        let mut v = values_of(&q);
        v.values.insert(
            "select".into(),
            json!("SELECT ID, V, N FROM S EMIT CHANGES"),
        );
        let w = wanted(&v, Some(&q), None).unwrap();
        let p = alter_plan(&q, &w, &["CSAS_OTHER_1".into()], true).unwrap();
        assert_eq!(p.steps.len(), 1);
        assert_eq!(
            p.steps[0].sql,
            "CREATE OR REPLACE STREAM `DBINE_TEST_SCHED_OUT` WITH (KAFKA_TOPIC='dbine_test_sched_out', PARTITIONS=1, REPLICAS=1, RETENTION_MS=604800000, VALUE_FORMAT='JSON') AS\nSELECT ID, V, N FROM S EMIT CHANGES;"
        );
        assert!(!p.steps[0].sql.contains("CLEANUP_POLICY"));
        assert_eq!(p.id.as_deref(), Some("CSAS_DBINE_TEST_SCHED_OUT_855"));
        assert!(p
            .script
            .contains("SET 'processing.guarantee'='at_least_once';"));
        assert!(p.toggle.is_none());
        // Properties alone also replace it (ksqlDB keeps the ones it was given).
        let mut v = values_of(&q);
        v.lists.insert("properties".into(), vec![]);
        let w = wanted(&v, Some(&q), None).unwrap();
        let p = alter_plan(&q, &w, &[], true).unwrap();
        assert!(p.steps[0].sql.starts_with("CREATE OR REPLACE STREAM"));
        assert!(!p.script.contains("processing.guarantee"));
    }

    #[test]
    fn a_new_destination_recreates() {
        let q = csas();
        let mut v = values_of(&q);
        v.values.insert("kafka_topic".into(), json!("other_topic"));
        let w = wanted(&v, Some(&q), None).unwrap();
        let p = alter_plan(&q, &w, &[], true).unwrap();
        let sqls: Vec<&str> = p.steps.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(sqls[0], "TERMINATE CSAS_DBINE_TEST_SCHED_OUT_855;");
        assert_eq!(sqls[1], "DROP STREAM `DBINE_TEST_SCHED_OUT`;");
        assert!(sqls[2]
            .starts_with("CREATE STREAM `DBINE_TEST_SCHED_OUT` WITH (KAFKA_TOPIC='other_topic'"));
        assert!(
            p.id.is_none(),
            "a recreated CREATE … AS SELECT gets a new id"
        );
        assert!(p.script.starts_with("-- Se recrea la consulta persistente"));
        assert!(p.script.contains("El topic dbine_test_sched_out queda"));
        assert!(p.script.contains("Se pierden su estado y sus offsets"));
        // Not while another query reads from it.
        let e = alter_plan(&q, &w, &["CSAS_READER_2".into()], true).unwrap_err();
        assert!(e.to_string().contains("CSAS_READER_2"));
        // Asked to recreate with the same destination.
        let mut v = values_of(&q);
        v.values.insert("recreate".into(), json!(true));
        let w = wanted(&v, Some(&q), None).unwrap();
        assert_eq!(alter_plan(&q, &w, &[], true).unwrap().steps.len(), 3);
    }

    #[test]
    fn an_insert_into_is_terminated_and_made_again_with_its_id() {
        let q = insert();
        assert!(q.paused);
        let mut v = values_of(&q);
        v.values.insert(
            "select".into(),
            json!("SELECT id, UCASE(v) AS v FROM s EMIT CHANGES"),
        );
        let w = wanted(&v, Some(&q), None).unwrap();
        assert!(!w.enabled, "it stays paused");
        let p = alter_plan(&q, &w, &["CSAS_READER_2".into()], true).unwrap();
        let sqls: Vec<&str> = p.steps.iter().map(|s| s.sql.as_str()).collect();
        assert_eq!(sqls, vec![
            "TERMINATE DBINE_TEST_SCHED_Q1;",
            "INSERT INTO `DBINE_TEST_SCHED_INS` WITH (QUERY_ID='DBINE_TEST_SCHED_Q1')\nSELECT id, UCASE(v) AS v FROM s EMIT CHANGES;",
        ]);
        assert_eq!(p.id.as_deref(), Some("DBINE_TEST_SCHED_Q1"));
        assert_eq!(p.toggle, Some(Toggle::Pause));
        assert!(p.script.contains("PAUSE DBINE_TEST_SCHED_Q1;"));
        assert!(p
            .script
            .starts_with("-- ksqlDB no cambia una INSERT INTO en el lugar"));
    }

    #[test]
    fn enabling_alone_is_pause_or_resume() {
        let q = csas();
        let mut v = values_of(&q);
        v.values.insert("enabled".into(), json!(false));
        let w = wanted(&v, Some(&q), None).unwrap();
        let p = alter_plan(&q, &w, &[], true).unwrap();
        assert!(p.steps.is_empty());
        assert_eq!(p.toggle, Some(Toggle::Pause));
        assert!(p.script.ends_with("PAUSE CSAS_DBINE_TEST_SCHED_OUT_855;\n"));
        assert!(
            alter_plan(&q, &w, &[], false).is_err(),
            "no PAUSE before 0.26"
        );
        let q = insert();
        let mut v = values_of(&q);
        v.values.insert("enabled".into(), json!(true));
        let p = alter_plan(&q, &wanted(&v, Some(&q), None).unwrap(), &[], true).unwrap();
        assert_eq!(p.toggle, Some(Toggle::Resume));
        assert!(p.script.contains("RESUME DBINE_TEST_SCHED_Q1;"));
    }

    #[test]
    fn the_form_reads_the_query() {
        let q = csas();
        let f = form_of(Some(&q), &["S".into()], true);
        assert_eq!(f.values["statement_kind"], json!("create_stream"));
        assert_eq!(f.values["kafka_topic"], json!("dbine_test_sched_out"));
        assert_eq!(f.values["partitions"], json!(1));
        assert_eq!(f.values["retention_ms"], json!(604800000));
        assert_eq!(f.values["cleanup_policy"], json!("delete"));
        assert_eq!(f.values["offset_reset"], json!("earliest"));
        assert_eq!(f.values["state_error"], json!("boom"));
        assert_eq!(f.values["state_columns"], json!("ID INTEGER KEY, V STRING"));
        let props = f
            .sections
            .iter()
            .flat_map(|s| &s.lists)
            .find(|l| l.key == "properties")
            .unwrap();
        assert_eq!(props.rows.len(), 1, "auto.offset.reset has its own field");
        assert!(f.notes.iter().any(|n| n.contains("CREATE OR REPLACE")));
        assert_eq!(
            f.choices["stream"],
            vec![("S".to_string(), "S".to_string())]
        );
        let general = &f.sections[0];
        assert!(
            general
                .fields
                .iter()
                .find(|x| x.key == "name")
                .unwrap()
                .read_only
        );
        let f = form_of(None, &[], false);
        assert_eq!(f.values["value_format"], json!("JSON"));
        assert!(
            f.sections[0]
                .fields
                .iter()
                .find(|x| x.key == "enabled")
                .unwrap()
                .read_only
        );
        assert!(f.notes.iter().any(|n| n.contains("PAUSE")));
    }

    #[test]
    fn the_destination_tab_is_for_create_as_select_only() {
        let f = form_of(None, &[], true);
        let dest = f.sections.iter().find(|s| s.key == "destination").unwrap();
        let only_create = Some((
            "statement_kind".to_string(),
            vec!["create_stream".to_string(), "create_table".to_string()],
        ));
        assert_eq!(dest.when, only_create);
        assert_eq!(dest.lists[0].when, only_create);
        // The INSERT's own fields stay in General.
        assert!(f.sections[0].fields.iter().any(|x| x.key == "target"));
    }

    #[test]
    fn queries_dbine_cant_read_are_listed_read_only() {
        let q = desc(
            "CSAS_DBINE_TEST_SCHED_OUT_855",
            CSAS,
            &["DBINE_TEST_SCHED_OUT"],
            json!({}),
            "PAUSED",
        );
        let it = item_of(&q, true).unwrap();
        assert_eq!(it.id, "CSAS_DBINE_TEST_SCHED_OUT_855");
        assert_eq!(
            (it.can_edit, it.can_delete, it.enabled),
            (None, None, Some(false))
        );
        assert_eq!(item_of(&q, false).unwrap().enabled, None);
        // The query a CREATE SOURCE TABLE keeps running.
        let cst = desc(
            "CST_DBINE_TEST_SCHED_SRC_3",
            "CREATE SOURCE TABLE DBINE_TEST_SCHED_SRC (ID INTEGER PRIMARY KEY, V STRING) WITH (KAFKA_TOPIC='t', VALUE_FORMAT='JSON');",
            &["DBINE_TEST_SCHED_SRC"],
            json!({}),
            "RUNNING",
        );
        let it = item_of(&cst, true).unwrap();
        assert_eq!((it.can_edit, it.can_delete), (Some(false), Some(false)));
        // Its job row offers neither Detener nor the enabled switch.
        let job = crate::jobs::job_of(&cst, true).unwrap();
        assert!(!job.can_stop && !job.can_enable);
        assert!(item_of(&json!({"id": "x; DROP STREAM S"}), true).is_none());
    }

    #[test]
    fn a_stale_form_and_odd_ids_are_refused() {
        assert!(kind_check("schedule").is_err());
        assert!(kind_check("job").is_ok());
        assert_eq!(
            created_id("Created query with ID CSAS_X_3"),
            Some("CSAS_X_3".into())
        );
        assert_eq!(created_id("Created query with ID X; DROP"), None);
        assert_eq!(
            status_path("stream/`DBINE_TEST_SCHED_INS`/create"),
            "/status/stream/%60DBINE_TEST_SCHED_INS%60/create"
        );
    }

    #[test]
    fn a_refused_recreate_puts_the_old_query_back() {
        // A CREATE … AS SELECT recreated for a new topic.
        let q = csas();
        let mut v = values_of(&q);
        v.values.insert("kafka_topic".into(), json!("other_topic"));
        let w = wanted(&v, Some(&q), None).unwrap();
        let p = alter_plan(&q, &w, &[], true).unwrap();
        assert!(p
            .script
            .contains("-- Si ksqlDB rechaza la sentencia nueva, DBine vuelve a crear"));
        let r = p.restore.as_ref().expect("a recreate restores");
        assert_eq!((r.drop, r.paused), (Some(1), false));
        assert_eq!(r.old.props, q.props);
        // Refused at the CREATE: the old one, as it was (without what
        // ksqlDB keeps but refuses in a statement).
        let old = "CREATE STREAM `DBINE_TEST_SCHED_OUT` WITH (KAFKA_TOPIC='dbine_test_sched_out', PARTITIONS=1, REPLICAS=1, RETENTION_MS=604800000, VALUE_FORMAT='JSON') AS\nSELECT\n  S.ID ID,\n  S.V V\nFROM S S\nWHERE (S.N > 0)\nEMIT CHANGES;";
        assert_eq!(r.statement(2), old);
        // Refused at the DROP: the stream is still there, without a query.
        assert_eq!(
            r.statement(1),
            old.replacen("CREATE STREAM", "CREATE OR REPLACE STREAM", 1)
        );
        let script = r.script(2);
        assert!(script.contains("SET 'auto.offset.reset'='earliest';\nSET 'processing.guarantee'='at_least_once';\nCREATE STREAM"));
        assert!(!script.contains("PAUSE"));
        let made = Restored::Made {
            id: Some("CSAS_DBINE_TEST_SCHED_OUT_900".into()),
            paused: Ok(()),
        };
        let m = r.message(
            2,
            "Line: 2, Col: 8: SELECT column 'IDD' cannot be resolved.",
            &made,
        );
        assert!(m.starts_with("ksqlDB rechazó el cambio: Line: 2"), "{m}");
        assert!(
            m.contains("y borrado su stream o tabla `DBINE_TEST_SCHED_OUT`"),
            "{m}"
        );
        assert!(
            m.contains("La volvió a crear como estaba, con el id CSAS_DBINE_TEST_SCHED_OUT_900."),
            "{m}"
        );
        let m = r.message(1, "x", &made);
        assert!(!m.contains("borrado"), "{m}");
        let m = r.message(2, "x", &Restored::Failed("y".into()));
        assert!(
            m.contains("ksqlDB también lo rechazó: y\nLa sentencia que tenía era:\nSET 'auto.offset.reset'"),
            "{m}"
        );
        assert!(m.contains(old), "{m}");
        // Asked to recreate it: the same restore.
        let mut v = values_of(&q);
        v.values.insert("recreate".into(), json!(true));
        let w = wanted(&v, Some(&q), None).unwrap();
        assert_eq!(
            alter_plan(&q, &w, &[], true)
                .unwrap()
                .restore
                .unwrap()
                .statement(2),
            old
        );

        // An INSERT INTO: made again with its id, and paused as it was.
        let q = insert();
        let mut v = values_of(&q);
        v.values
            .insert("select".into(), json!("SELECT idd FROM s EMIT CHANGES"));
        let w = wanted(&v, Some(&q), None).unwrap();
        let r = alter_plan(&q, &w, &[], true).unwrap().restore.unwrap();
        assert_eq!((r.drop, r.paused), (None, true));
        assert_eq!(
            r.statement(1),
            "INSERT INTO `DBINE_TEST_SCHED_INS` WITH (QUERY_ID='DBINE_TEST_SCHED_Q1')\nSELECT id, v FROM s EMIT CHANGES;"
        );
        assert!(r
            .script(1)
            .ends_with("EMIT CHANGES;\nPAUSE DBINE_TEST_SCHED_Q1;\n"));
        let m = r.message(
            1,
            "x",
            &Restored::Made {
                id: Some("DBINE_TEST_SCHED_Q1".into()),
                paused: Err("z".into()),
            },
        );
        assert!(m.contains("La volvió a crear como estaba.\n"), "{m}");
        assert!(m.ends_with("no se pudo pausar otra vez: z"), "{m}");

        // What changes in place or only pauses has nothing to restore.
        let q = csas();
        let mut v = values_of(&q);
        v.values
            .insert("select".into(), json!("SELECT ID FROM S EMIT CHANGES"));
        let w = wanted(&v, Some(&q), None).unwrap();
        assert!(alter_plan(&q, &w, &[], true).unwrap().restore.is_none());
        let mut v = values_of(&q);
        v.values.insert("enabled".into(), json!(false));
        let w = wanted(&v, Some(&q), None).unwrap();
        assert!(alter_plan(&q, &w, &[], true).unwrap().restore.is_none());
    }

    #[test]
    fn the_delete_is_terminate() {
        let s = delete_script(&csas());
        assert!(s.ends_with("TERMINATE CSAS_DBINE_TEST_SCHED_OUT_855;\n"));
        assert!(s.contains("--   DROP STREAM `DBINE_TEST_SCHED_OUT` DELETE TOPIC;"));
        // A name with line breaks stays inside its comments.
        let mut q = csas();
        q.def.sink = "x\nDROP STREAM y DELETE TOPIC;\r--\u{2028}z".into();
        let s = delete_script(&q);
        let lines: Vec<&str> = s.split(['\n', '\r', '\u{2028}']).collect();
        assert_eq!(lines.len(), 4, "{s:?}");
        assert!(lines[..2].iter().all(|l| l.starts_with("-- ")), "{s:?}");
        assert_eq!(lines[2], "TERMINATE CSAS_DBINE_TEST_SCHED_OUT_855;");
        assert!(
            s.contains("STREAM x DROP STREAM y DELETE TOPIC; -- z y su topic"),
            "{s:?}"
        );
        // The same in a script's heading.
        let r = render(&["a\rDROP STREAM s;".into()], &[], &BTreeMap::new(), None);
        assert_eq!(r, "-- a DROP STREAM s;\n");
    }

    #[test]
    fn errors_newest_first() {
        let a = desc("A_1", CSAS, &["X"], json!({}), "RUNNING");
        let mut b = desc("B_2", CSAS, &["X"], json!({}), "RUNNING");
        b["queryErrors"] = json!([{"errorMessage": "older", "timestamp": 5, "type": "SYSTEM"}]);
        let e = errors_of(&[&b, &a]);
        assert_eq!(e[0].1, "A_1");
        assert_eq!(e[1].3, "older");
    }
}
