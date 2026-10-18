//! Notebook parameters (docs/notebooks.md#parameters): values asked for
//! when the notebook runs, which every cell uses as `{{name}}`.
//!
//! - **Defined** in `metadata.dbine.parameters`: name, type, default,
//!   description, required, and the values of a list. Never connection
//!   details: a default that looks like a credential or a connection string
//!   is dropped on read and on write.
//! - **Used** as `{{name}}` (spaces allowed inside the braces) in code: in a
//!   string, a quoted name or where a name goes (`FROM {{t}}`, `t.{{c}}`,
//!   `{{f}}(…)`, `tabla_{{mes}}`) it is refused; in a comment it is left as
//!   it is. No engine reads `{{` as its own syntax outside strings: not SQL,
//!   CQL, mongosh, Redis, Flux, Cypher nor N1QL (whose `{` objects never hold
//!   a bare `{`); `@x` is T-SQL's variable and `:x` PostgreSQL's cast and
//!   Oracle's bind, so neither could be ours.
//! - **Run** with each value bound by the driver where it binds
//!   ([`dbine_driver::Session::execute_bound`]), else written as a literal by
//!   the driver's quoting ([`dbine_driver::Driver::param_literal`]). Never
//!   raw text.
//!
//! [`prepare_notebook`] is the whole notebook's executable form for a run
//! without the UI (scheduled runs); [`prepare_text`] one cell's, for the
//! editor's run.

use super::{CellKind, Notebook, NotebookCell};
use dbine_driver::sql::{name_tokens, NameToken, ScriptDialect, ScriptStatement, TokenKind};
use dbine_driver::{BoundPart, BoundStatement, Driver, Language, ParamValue};
use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};
use std::collections::BTreeMap;
use std::fmt;

/// The longest parameter name.
const MAX_NAME: usize = 63;
/// Values of a list, and characters of a text value, kept at most.
const MAX_OPTIONS: usize = 500;
const MAX_TEXT: usize = 100_000;

#[derive(Serialize, Deserialize, Clone, Copy, Debug, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
pub enum ParamType {
    Text,
    Number,
    Date,
    Datetime,
    Boolean,
    /// One of `options` (text).
    List,
}

#[derive(Serialize, Deserialize, Clone, Debug, PartialEq)]
pub struct NotebookParameter {
    pub name: String,
    #[serde(rename = "type")]
    pub kind: ParamType,
    /// Used when the run gives no value. JSON: a string, number or bool.
    #[serde(default)]
    pub default: Option<Value>,
    #[serde(default)]
    pub description: Option<String>,
    /// The run needs a value (given or the default).
    #[serde(default)]
    pub required: bool,
    /// List: the values offered.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub options: Vec<String>,
}

/// Values given for a run, by name (JSON: strings, numbers, bools, null).
pub type Values = BTreeMap<String, Value>;

/// Why a run can't use its parameters (Spanish, as every backend message).
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ParamError {
    /// Required parameters without a value or a default.
    Missing { names: Vec<String> },
    /// A value that isn't of its parameter's type.
    Invalid { name: String, message: String },
    /// `{{name}}` in SQL code with no parameter of that name.
    Undefined { cell: Option<String>, name: String },
    /// `{{name}}` inside a string literal.
    InString { cell: Option<String>, name: String },
    /// `{{name}}` where a table, column or function name goes.
    Identifier { cell: Option<String>, name: String },
    /// The engine can't write the value (NULL in Redis…).
    Literal { cell: Option<String>, name: String, message: String },
}

fn in_cell(cell: &Option<String>) -> String {
    cell.as_deref().map(|c| format!(" (celda {c})")).unwrap_or_default()
}

impl fmt::Display for ParamError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ParamError::Missing { names } => write!(f, "Faltan valores para los parámetros obligatorios: {}", names.join(", ")),
            ParamError::Invalid { name, message } => write!(f, "El valor del parámetro «{name}» no es válido: {message}"),
            ParamError::Undefined { cell, name } => {
                write!(f, "El notebook no tiene un parámetro «{name}»{}: definilo en Parámetros o quitá las llaves", in_cell(cell))
            }
            ParamError::InString { cell, name } => write!(
                f,
                "El parámetro «{name}» está dentro de un texto entre comillas{}: escribilo sin comillas, el valor va como texto (por ejemplo, '%' || {{{{{name}}}}} || '%')",
                in_cell(cell)
            ),
            ParamError::Identifier { cell, name } => write!(
                f,
                "El parámetro «{name}» está donde va un nombre (tabla, columna o función){}: los parámetros solo reemplazan valores, no nombres",
                in_cell(cell)
            ),
            ParamError::Literal { cell, name, message } => write!(f, "El parámetro «{name}»{}: {message}", in_cell(cell)),
        }
    }
}

impl std::error::Error for ParamError {}

impl From<ParamError> for crate::error::CommandError {
    fn from(e: ParamError) -> Self {
        crate::error::CommandError::BadRequest(e.to_string())
    }
}

// ---------------------------------------------------------------------------
// Definitions (metadata.dbine.parameters)

/// A parameter name: a letter or `_`, then letters, digits or `_` (ASCII).
pub fn valid_name(name: &str) -> bool {
    name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.len() <= MAX_NAME
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
}

/// A name a default must not be kept under (a password, a token…).
fn secret_name(name: &str) -> bool {
    let n: String = name.chars().filter(|c| c.is_ascii_alphanumeric()).map(|c| c.to_ascii_lowercase()).collect();
    ["password", "passwd", "pwd", "secret", "token", "apikey", "credential", "connectionstring", "connstr", "accesskey", "privatekey"]
        .iter()
        .any(|w| n.contains(w))
}

/// A default that is a credential or a connection's details.
fn sensitive_default(p: &NotebookParameter) -> bool {
    p.default.as_ref().is_some_and(|d| !d.is_null() && (secret_name(&p.name) || d.as_str().is_some_and(super::looks_like_connection_string)))
}

/// The parameters of `metadata.dbine.parameters`: entries with a valid,
/// unused name (case-insensitive) and a known type; defaults that are
/// scalars of that type. Returns whether a default was dropped as
/// sensitive.
pub fn read_parameters(v: Option<&Value>) -> (Vec<NotebookParameter>, bool) {
    let mut out: Vec<NotebookParameter> = Vec::new();
    let mut dropped = false;
    for item in v.and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]) {
        let Ok(mut p) = serde_json::from_value::<NotebookParameter>(item.clone()) else { continue };
        p.name = p.name.trim().to_string();
        if !valid_name(&p.name) || out.iter().any(|o| o.name.eq_ignore_ascii_case(&p.name)) {
            continue;
        }
        if p.default.as_ref().is_some_and(|d| !(d.is_string() || d.is_number() || d.is_boolean())) {
            p.default = None;
        }
        if sensitive_default(&p) {
            p.default = None;
            dropped = true;
        }
        p.description = p.description.map(|d| d.trim().to_string()).filter(|d| !d.is_empty());
        p.options.retain(|o| !o.trim().is_empty());
        p.options.truncate(MAX_OPTIONS);
        out.push(p);
    }
    (out, dropped)
}

/// `metadata.dbine.parameters` for the file (no sensitive defaults).
pub fn parameters_json(ps: &[NotebookParameter]) -> Value {
    Value::Array(
        ps.iter()
            .filter(|p| valid_name(&p.name))
            .map(|p| {
                let mut p = p.clone();
                if sensitive_default(&p) {
                    p.default = None;
                }
                let mut v = serde_json::to_value(&p).unwrap_or(Value::Null);
                if let Some(m) = v.as_object_mut() {
                    // Only what is set.
                    m.retain(|k, v| !(v.is_null() || (k == "required" && v == &Value::Bool(false))));
                }
                v
            })
            .collect(),
    )
}

// ---------------------------------------------------------------------------
// Values

fn empty(v: &Value) -> bool {
    v.is_null() || v.as_str().is_some_and(|s| s.trim().is_empty())
}

/// `v` as `p`'s type.
pub fn value_of(p: &NotebookParameter, v: &Value) -> Result<ParamValue, ParamError> {
    let bad = |message: &str| ParamError::Invalid { name: p.name.clone(), message: message.into() };
    let text = || match v {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    };
    if v.is_null() {
        return Ok(ParamValue::Null);
    }
    match p.kind {
        ParamType::Text => {
            let s = text().ok_or_else(|| bad("tiene que ser un texto"))?;
            if s.chars().count() > MAX_TEXT {
                return Err(bad("el texto es demasiado largo"));
            }
            // A NUL ends a string in some engines and protocols.
            if s.contains('\0') {
                return Err(bad("el texto tiene un carácter nulo (\\0)"));
            }
            Ok(ParamValue::Text(s))
        }
        ParamType::Number => text().and_then(|s| ParamValue::number(&s)).ok_or_else(|| bad("tiene que ser un número (por ejemplo 12 o -3.5)")),
        ParamType::Date => text().and_then(|s| ParamValue::date(&s)).ok_or_else(|| bad("tiene que ser una fecha AAAA-MM-DD")),
        ParamType::Datetime => {
            text().and_then(|s| ParamValue::datetime(&s)).ok_or_else(|| bad("tiene que ser una fecha y hora AAAA-MM-DD HH:MM[:SS]"))
        }
        ParamType::Boolean => match v {
            Value::Bool(b) => Ok(ParamValue::Bool(*b)),
            Value::String(s) if matches!(s.trim().to_ascii_lowercase().as_str(), "true" | "1" | "sí" | "si" | "yes") => Ok(ParamValue::Bool(true)),
            Value::String(s) if matches!(s.trim().to_ascii_lowercase().as_str(), "false" | "0" | "no") => Ok(ParamValue::Bool(false)),
            Value::Number(n) if n.as_i64() == Some(1) => Ok(ParamValue::Bool(true)),
            Value::Number(n) if n.as_i64() == Some(0) => Ok(ParamValue::Bool(false)),
            _ => Err(bad("tiene que ser verdadero o falso")),
        },
        ParamType::List => {
            let s = text().ok_or_else(|| bad("tiene que ser uno de los valores de la lista"))?;
            if !p.options.iter().any(|o| *o == s) {
                return Err(bad("tiene que ser uno de los valores de la lista"));
            }
            Ok(ParamValue::Text(s))
        }
    }
}

/// The values a run uses: each parameter's given value, else its default,
/// else NULL; a required one with neither is an error (all of them listed).
/// Names not defined are ignored.
pub fn resolve(defs: &[NotebookParameter], given: &Values) -> Result<BTreeMap<String, ParamValue>, ParamError> {
    let mut out = BTreeMap::new();
    let mut missing = Vec::new();
    for p in defs {
        let given = given.get(&p.name).or_else(|| given.iter().find(|(k, _)| k.eq_ignore_ascii_case(&p.name)).map(|(_, v)| v));
        let v = given.filter(|v| !empty(v)).or_else(|| p.default.as_ref().filter(|v| !empty(v)));
        match v {
            Some(v) => {
                out.insert(p.name.clone(), value_of(p, v)?);
            }
            None if p.required => missing.push(p.name.clone()),
            None => {
                out.insert(p.name.clone(), ParamValue::Null);
            }
        }
    }
    if !missing.is_empty() {
        return Err(ParamError::Missing { names: missing });
    }
    Ok(out)
}

/// A value as the file records it (`metadata.dbine.params` of a result).
#[allow(dead_code)] // Used by `prepare_notebook`.
pub fn value_json(v: &ParamValue) -> Value {
    match v {
        ParamValue::Null => Value::Null,
        ParamValue::Text(s) | ParamValue::Date(s) | ParamValue::DateTime(s) => Value::String(s.clone()),
        ParamValue::Number(n) => serde_json::from_str::<serde_json::Number>(n).map(Value::Number).unwrap_or_else(|_| Value::String(n.clone())),
        ParamValue::Bool(b) => Value::Bool(*b),
    }
}

/// The values a result records: the parameters its cell used.
#[allow(dead_code)] // Used by `prepare_notebook`.
pub fn values_json(values: &BTreeMap<String, ParamValue>, used: &[String]) -> Map<String, Value> {
    used.iter().filter_map(|n| values.get(n).map(|v| (n.clone(), value_json(v)))).collect()
}

// ---------------------------------------------------------------------------
// Where a cell's text uses them

/// `{{name}}` in a text: where it is and the name as written.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Marker {
    /// The parameter's name as defined (case as in the definition).
    pub name: String,
    pub start: usize,
    pub end: usize,
}

/// The `{{ name }}` at `i`, if any: (name, end).
fn marker_at(b: &[u8], i: usize) -> Option<(&str, usize)> {
    if b.get(i) != Some(&b'{') || b.get(i + 1) != Some(&b'{') {
        return None;
    }
    let mut j = i + 2;
    while b.get(j).is_some_and(|c| *c == b' ' || *c == b'\t') {
        j += 1;
    }
    let start = j;
    if !b.get(j).is_some_and(|c| c.is_ascii_alphabetic() || *c == b'_') {
        return None;
    }
    while b.get(j).is_some_and(|c| c.is_ascii_alphanumeric() || *c == b'_') {
        j += 1;
    }
    let end_name = j;
    while b.get(j).is_some_and(|c| *c == b' ' || *c == b'\t') {
        j += 1;
    }
    if b.get(j) != Some(&b'}') || b.get(j + 1) != Some(&b'}') {
        return None;
    }
    // `{{{x}}}` is someone else's (a template's triple braces).
    if i > 0 && b[i - 1] == b'{' {
        return None;
    }
    std::str::from_utf8(&b[start..end_name]).ok().map(|n| (n, j + 2))
}

/// Every `{{name}}` of `text`, wherever it is (strings and comments too).
fn all_markers(text: &str) -> Vec<(String, usize, usize)> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i + 1 < b.len() {
        if let Some((name, end)) = marker_at(b, i) {
            out.push((name.to_string(), i, end));
            i = end;
        } else {
            i += 1;
        }
    }
    out
}

/// The parameters `text` mentions (defined ones, in order, each once),
/// wherever: what marks a cell's result stale when a value changes (the UI
/// has its own copy, `notebookParams.ts`).
#[allow(dead_code)]
pub fn mentioned(text: &str, defs: &[NotebookParameter]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (n, _, _) in all_markers(text) {
        if let Some(p) = defs.iter().find(|p| p.name.eq_ignore_ascii_case(&n)) {
            if !out.contains(&p.name) {
                out.push(p.name.clone());
            }
        }
    }
    out
}

/// The names among `names` that `text` mentions as `{{name}}` (each once,
/// as given), wherever they are.
pub fn mentioned_names(text: &str, names: &[String]) -> Vec<String> {
    let mut out: Vec<String> = Vec::new();
    for (n, _, _) in all_markers(text) {
        if let Some(d) = names.iter().find(|d| d.eq_ignore_ascii_case(&n)) {
            if !out.contains(d) {
                out.push(d.clone());
            }
        }
    }
    out
}

/// `text` with each `{{name}}` of `names` as a `:name` bind (the Python
/// export's SQLAlchemy cells, which bind them).
pub fn to_named_binds(text: &str, names: &[String]) -> String {
    let mut out = String::with_capacity(text.len());
    let mut at = 0;
    for (n, start, end) in all_markers(text) {
        if let Some(d) = names.iter().find(|d| d.eq_ignore_ascii_case(&n)) {
            out.push_str(&text[at..start]);
            out.push(':');
            out.push_str(d);
            at = end;
        }
    }
    out.push_str(&text[at..]);
    out
}

/// Words after which what follows is a name, not a value.
const NAME_AFTER: &[&str] = &[
    "from", "join", "into", "update", "table", "view", "index", "database", "schema", "use", "exec", "execute", "call", "as", "by", "set",
    "truncate", "describe", "procedure", "function", "trigger", "sequence", "keyspace", "collection", "type", "label",
];

/// How a cell's language writes strings and comments, for the languages
/// the SQL scanner doesn't read (mongosh / JSON, Redis, Flux, Cypher).
#[derive(Clone, Copy)]
struct CLike {
    /// `'…'` is a string too.
    single: bool,
    /// `//` and `/* */` comments.
    slash: bool,
    /// `#` at an argument's start comments out the line (Redis).
    hash: bool,
}

/// The tokens of a C-like text: names, strings and punctuation, comments
/// and spaces left out (the SQL scanner's shape).
fn clike_tokens(text: &str, l: CLike) -> Vec<NameToken<'_>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if l.slash && c == b'/' && b.get(i + 1) == Some(&b'/') {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if l.slash && c == b'/' && b.get(i + 1) == Some(&b'*') {
            i = text[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            continue;
        }
        if l.hash && c == b'#' && (i == 0 || b[i - 1].is_ascii_whitespace()) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'"' || (l.single && c == b'\'') || (c == b'`') {
            let mut j = i + 1;
            while j < b.len() && b[j] != c {
                j += if b[j] == b'\\' { 2 } else { 1 };
            }
            let end = (j + 1).min(b.len());
            let kind = if c == b'`' { TokenKind::Name } else { TokenKind::String };
            out.push(NameToken { kind, text: &text[i..end], start: i, end });
            i = end;
            continue;
        }
        if c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80 {
            let mut j = i;
            while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_' || b[j] == b'$' || b[j] >= 0x80) {
                j += 1;
            }
            out.push(NameToken { kind: TokenKind::Name, text: &text[i..j], start: i, end: j });
            i = j;
            continue;
        }
        let mut j = i + 1;
        while j < b.len() && !text.is_char_boundary(j) {
            j += 1;
        }
        out.push(NameToken { kind: TokenKind::Punct, text: &text[i..j], start: i, end: j });
        i = j;
    }
    out
}

/// What a cell's text is written in, for finding its parameters and
/// writing their values.
pub enum Engine<'a> {
    /// A cell on a connection: its driver.
    Driver(&'a dyn Driver),
    /// A DataFrame SQL cell: DuckDB, on no connection.
    DataFrames,
}

impl Engine<'_> {
    fn language(&self) -> Language {
        match self {
            Engine::Driver(d) => d.info().language,
            Engine::DataFrames => Language::Sql,
        }
    }

    fn dialect(&self) -> ScriptDialect {
        match self {
            Engine::Driver(d) => d.script_dialect(),
            Engine::DataFrames => ScriptDialect::postgres(),
        }
    }

    fn literal(&self, v: &ParamValue) -> dbine_driver::Result<String> {
        match self {
            Engine::Driver(d) => d.param_literal(v),
            Engine::DataFrames => dbine_driver::params::literal(Language::Sql, "duckdb", "duckdb", v),
        }
    }

    /// The units a run goes through (as the editor cuts them).
    fn split(&self, text: &str) -> Vec<ScriptStatement> {
        match self {
            Engine::Driver(d) => d.split_script(text),
            Engine::DataFrames => dbine_driver::sql::split_script(text, &ScriptDialect::postgres()),
        }
    }

    /// Whether the driver may bind values (the session says for sure).
    fn binds(&self) -> bool {
        matches!(self, Engine::Driver(_))
    }

    fn tokens<'t>(&self, text: &'t str) -> Vec<NameToken<'t>> {
        match self.language() {
            Language::Sql | Language::Cql => name_tokens(text, &self.dialect()),
            Language::Json => clike_tokens(text, CLike { single: true, slash: true, hash: false }),
            Language::Redis => clike_tokens(text, CLike { single: true, slash: false, hash: true }),
            Language::Flux => clike_tokens(text, CLike { single: false, slash: true, hash: false }),
            Language::Cypher => clike_tokens(text, CLike { single: true, slash: true, hash: false }),
        }
    }
}

/// The `{{name}}` of `text` that run with values: defined parameters in
/// code. A defined one in a string, a quoted name or where a name goes is
/// an error; in a comment it stays text. In SQL code an undefined
/// `{{name}}` is an error too (`{{` means nothing there); in other
/// languages it stays text (a search template's own `{{x}}`).
pub fn markers(text: &str, engine: &Engine<'_>, defs: &[NotebookParameter], cell: Option<&str>) -> Result<Vec<Marker>, ParamError> {
    let found = all_markers(text);
    if found.is_empty() {
        return Ok(Vec::new());
    }
    let language = engine.language();
    let sql = matches!(language, Language::Sql | Language::Cql);
    let toks = engine.tokens(text);
    let cell = cell.map(str::to_string);
    let b = text.as_bytes();
    let mut out = Vec::new();
    for (written, start, end) in found {
        let def = defs.iter().find(|p| p.name.eq_ignore_ascii_case(&written));
        // The token that holds the marker's first brace: none is a comment.
        let at = toks.partition_point(|t| t.end <= start);
        let holder = toks.get(at).filter(|t| t.start <= start);
        let Some(holder) = holder else { continue };
        let quoted_name = holder.kind == TokenKind::Name && holder.start < start;
        if holder.kind == TokenKind::String || quoted_name {
            let Some(p) = def else { continue };
            return Err(if quoted_name {
                ParamError::Identifier { cell, name: p.name.clone() }
            } else {
                ParamError::InString { cell, name: p.name.clone() }
            });
        }
        let Some(p) = def else {
            if sql {
                return Err(ParamError::Undefined { cell, name: written });
            }
            continue;
        };
        let ident = || ParamError::Identifier { cell: cell.clone(), name: p.name.clone() };
        let prev = toks[..at].last();
        let next = toks.iter().find(|t| t.start >= end);
        // Glued to a word: part of a name (`tabla_{{mes}}`, `{{x}}_2`).
        let glued = |t: Option<&NameToken<'_>>, touches: bool| t.is_some_and(|t| touches && t.kind == TokenKind::Name);
        if glued(prev, prev.is_some_and(|t| t.end == start)) || glued(next, next.is_some_and(|t| t.start == end)) {
            return Err(ident());
        }
        let punct = |t: Option<&NameToken<'_>>, c: &str| t.is_some_and(|t| t.kind == TokenKind::Punct && t.text == c);
        // `t.{{c}}`, `{{s}}.t`, `{{f}}(…)`.
        if punct(prev, ".") || punct(next, ".") || (sql && punct(next, "(")) {
            return Err(ident());
        }
        let word = |t: Option<&NameToken<'_>>| t.filter(|t| t.kind == TokenKind::Name && !b[t.start..].starts_with(b"\"") && !b[t.start..].starts_with(b"`") && !b[t.start..].starts_with(b"[")).map(|t| t.text.to_ascii_lowercase());
        match language {
            Language::Sql | Language::Cql => {
                if word(prev).is_some_and(|w| NAME_AFTER.contains(&w.as_str())) {
                    return Err(ident());
                }
            }
            // A key (`{ {{k}}: 1 }`) or a collection (`db.{{c}}`, above).
            Language::Json => {
                if punct(next, ":") {
                    return Err(ident());
                }
            }
            // The command itself: the first word of its line.
            Language::Redis => {
                let line_start = text[..start].rfind('\n').map_or(0, |p| p + 1);
                if text[line_start..start].trim().is_empty() {
                    return Err(ident());
                }
            }
            // A label or a relationship type (`(n:{{l}})`, `[:{{t}}]`); a
            // map's value (`{k: {{v}}}`) is a value.
            Language::Cypher => {
                if punct(prev, ":") && (punct(next, ")") || punct(next, "]") || punct(next, "{") || punct(next, "|") || punct(next, ":")) {
                    return Err(ident());
                }
                if word(prev).is_some_and(|w| w == "as") {
                    return Err(ident());
                }
            }
            Language::Flux => {}
        }
        out.push(Marker { name: p.name.clone(), start, end });
    }
    Ok(out)
}

/// A text with its markers as the parts of a [`BoundStatement`] (values in
/// order of first use).
fn bound(text: &str, markers: &[&Marker], values: &BTreeMap<String, ParamValue>, base: usize) -> BoundStatement {
    let mut parts = Vec::new();
    let mut names: Vec<&str> = Vec::new();
    let mut vals = Vec::new();
    let mut at = 0;
    for m in markers {
        let (s, e) = (m.start - base, m.end - base);
        if s > at {
            parts.push(BoundPart::Text(text[at..s].to_string()));
        }
        let i = match names.iter().position(|n| *n == m.name) {
            Some(i) => i,
            None => {
                names.push(&m.name);
                vals.push(values.get(&m.name).cloned().unwrap_or(ParamValue::Null));
                names.len() - 1
            }
        };
        parts.push(BoundPart::Param(i));
        at = e;
    }
    if at < text.len() {
        parts.push(BoundPart::Text(text[at..].to_string()));
    }
    BoundStatement { parts, values: vals }
}

/// A cell's text ready to run.
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct Prepared {
    /// The text with every value as the engine's literal: what runs when
    /// the driver doesn't bind, what the history keeps.
    pub text: String,
    /// One per unit of `text` as the driver cuts it ([`Driver::split_script`]):
    /// the unit with its values apart, `None` for a unit with none. Empty
    /// when the cell has no parameters, or when the units can't be matched
    /// with the source's (then the literals run).
    pub bound: Vec<Option<BoundStatement>>,
    /// The parameters the text uses, in order of first use.
    pub used: Vec<String>,
}

impl Prepared {
    /// A unit's bound form, by index, when it has values.
    pub fn bound_unit(&self, i: usize) -> Option<&BoundStatement> {
        self.bound.get(i).and_then(Option::as_ref).filter(|b| b.has_params())
    }
}

/// `text` (a cell's source) ready to run on `engine` with `values`: the
/// literal form, and the bound form per unit.
pub fn prepare_text(
    text: &str,
    engine: &Engine<'_>,
    defs: &[NotebookParameter],
    values: &BTreeMap<String, ParamValue>,
    cell: Option<&str>,
) -> Result<Prepared, ParamError> {
    let ms = markers(text, engine, defs, cell)?;
    if ms.is_empty() {
        return Ok(Prepared { text: text.to_string(), bound: Vec::new(), used: Vec::new() });
    }
    let mut used: Vec<String> = Vec::new();
    for m in &ms {
        if !used.contains(&m.name) {
            used.push(m.name.clone());
        }
    }
    let mut lits: BTreeMap<&str, String> = BTreeMap::new();
    for n in &used {
        let v = values.get(n).cloned().unwrap_or(ParamValue::Null);
        let lit = engine.literal(&v).map_err(|e| ParamError::Literal { cell: cell.map(str::to_string), name: n.clone(), message: e.to_string() })?;
        lits.insert(n.as_str(), lit);
    }
    let all: Vec<&Marker> = ms.iter().collect();
    let whole = bound(text, &all, values, 0);
    let lit_names: Vec<&String> = used.iter().collect();
    let literal = whole.render(|i| {
        // `whole.values` are in order of first use, as `used`.
        lit_names.get(i).and_then(|n| lits.get(n.as_str())).cloned().unwrap_or_default()
    });
    let mut out = Prepared { text: literal, bound: Vec::new(), used };
    if engine.binds() {
        let src_units = engine.split(text);
        let lit_units = engine.split(&out.text);
        let verbatim = src_units.iter().all(|u| text.get(u.start..u.end).is_some_and(|s| s == u.text));
        // Each marker inside one unit, and the units the same in both texts.
        let inside = ms.iter().all(|m| src_units.iter().any(|u| u.start <= m.start && m.end <= u.end));
        let same = src_units.len() == lit_units.len() && src_units.iter().zip(&lit_units).all(|(a, b)| a.kind == b.kind && a.repeat == b.repeat);
        if verbatim && inside && same {
            out.bound = src_units
                .iter()
                .map(|u| {
                    let mine: Vec<&Marker> = ms.iter().filter(|m| u.start <= m.start && m.end <= u.end).collect();
                    (!mine.is_empty()).then(|| bound(&u.text, &mine, values, u.start))
                })
                .collect();
        }
    }
    Ok(out)
}

/// Run `p` on `session`, as the editor runs a cell: unit by unit (the
/// driver's cut) unless the driver takes scripts whole, each unit with its
/// values bound where the driver binds them and as literals where it
/// doesn't (it says so before running anything). Stops at the first error.
/// Returns how many units ran with bound values. What a run without the UI
/// does with each [`PreparedCell`] (the editor's run goes through
/// `execute_query`, which does the same with its progress and history).
#[allow(dead_code)] // The API for runs without the UI (scheduled runs).
pub async fn run_prepared(
    session: &mut dyn dbine_driver::Session,
    driver: &dyn Driver,
    p: &Prepared,
    max_rows: usize,
    out: &mut dbine_driver::QueryOutcome,
) -> dbine_driver::Result<usize> {
    async fn one(
        session: &mut dyn dbine_driver::Session,
        b: Option<&BoundStatement>,
        literal: &str,
        max_rows: usize,
        out: &mut dbine_driver::QueryOutcome,
    ) -> dbine_driver::Result<bool> {
        if let Some(b) = b {
            match session.execute_bound(b, max_rows, out).await {
                Err(dbine_driver::Error::Unsupported(_)) => {}
                r => return r.map(|()| true),
            }
        }
        session.execute(literal, max_rows, out).await.map(|()| false)
    }
    if driver.script_mode() == dbine_driver::ScriptMode::Whole {
        let b = if p.bound.len() == 1 { p.bound_unit(0) } else { None };
        return one(session, b, &p.text, max_rows, out).await.map(usize::from);
    }
    let units = driver.split_script(&p.text);
    let aligned = p.bound.len() == units.len();
    let mut bound = 0;
    for (i, u) in units.iter().enumerate() {
        if u.kind == dbine_driver::StatementKind::ClientCommand {
            continue;
        }
        if let Some(e) = &u.error {
            return Err(dbine_driver::Error::Query(e.clone()));
        }
        let b = if aligned { p.bound_unit(i) } else { None };
        for _ in 0..u.repeat.max(1) {
            bound += usize::from(one(session, b, &u.text, max_rows, out).await?);
        }
    }
    Ok(bound)
}

// ---------------------------------------------------------------------------
// The whole notebook (runs without the UI)

/// A cell of a notebook run, ready to execute.
#[allow(dead_code)] // The API for runs without the UI (scheduled runs).
#[derive(Debug, Clone, PartialEq, Serialize)]
pub struct PreparedCell {
    /// The cell's id and position.
    pub id: String,
    pub index: usize,
    /// Its name as a DataFrame (`metadata.dbine.name`).
    pub name: Option<String>,
    /// A DataFrame SQL cell (runs on DuckDB over the cells above).
    pub dataframes: bool,
    /// See [`Prepared`].
    pub prepared: Prepared,
    /// The values this cell used, for its result (`metadata.dbine.params`).
    pub values: Map<String, Value>,
}

/// Every code cell of `nb` (SQL on a connection and DataFrame SQL; write
/// cells too once they exist, they are code cells with a source) ready to
/// run with `given`: missing values take their defaults, a required one
/// without either fails the whole run ([`ParamError::Missing`]), and so does
/// any cell that uses a parameter where it can't go.
///
/// `driver_of` resolves a cell on a connection to its driver (the caller
/// maps the cell's target, or the notebook's, to a local connection); a
/// cell it can't resolve is left out of the result, and the caller reports
/// it as it does any unresolved target. Empty cells are left out too.
#[allow(dead_code)] // The API for runs without the UI (scheduled runs).
pub fn prepare_notebook(
    nb: &Notebook,
    given: &Values,
    driver_of: &dyn Fn(&NotebookCell) -> Option<std::sync::Arc<dyn Driver>>,
) -> Result<Vec<PreparedCell>, ParamError> {
    let values = resolve(&nb.parameters, given)?;
    let mut out = Vec::new();
    for (index, c) in nb.cells.iter().enumerate() {
        if c.kind != CellKind::Sql || c.source.trim().is_empty() {
            continue;
        }
        let dataframes = c.target.as_ref().is_some_and(|t| t.results);
        let label = c.name.clone().unwrap_or_else(|| (index + 1).to_string());
        let prepared = if dataframes {
            prepare_text(&c.source, &Engine::DataFrames, &nb.parameters, &values, Some(&label))?
        } else {
            let Some(d) = driver_of(c) else { continue };
            prepare_text(&c.source, &Engine::Driver(d.as_ref()), &nb.parameters, &values, Some(&label))?
        };
        let vals = values_json(&values, &prepared.used);
        out.push(PreparedCell { id: c.id.clone(), index, name: c.name.clone(), dataframes, prepared, values: vals });
    }
    Ok(out)
}

#[cfg(test)]
#[path = "params_tests.rs"]
mod tests;

#[cfg(test)]
#[path = "params_live_tests.rs"]
mod live;
