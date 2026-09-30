//! The MongoDB editor language: mongo-shell-like calls and raw command
//! documents, parsed into database commands.
//!
//! Values are "relaxed JSON" (JSON5-ish): unquoted keys, single quotes,
//! trailing commas, comments, and the shell constructors (`ObjectId("…")`,
//! `ISODate("…")`, `NumberLong(…)`, `/regex/i`…), which become MongoDB
//! extended JSON before turning into BSON.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use mongodb::bson::{doc, oid::ObjectId, Bson, Document};
use serde_json::{json, Map, Value};

/// How a command's reply becomes a result.
#[derive(Debug, Clone, PartialEq)]
pub enum Shape {
    /// A cursor command (find, aggregate, listCollections, listIndexes):
    /// one row per document.
    Cursor,
    /// A cursor whose first document's `count` field is the answer
    /// (countDocuments); no document means 0.
    CountAgg,
    /// A `count` command: the reply's `n`.
    Count,
    /// A `distinct` command: one row per value, in a column named after the key.
    Distinct(String),
    /// insert / update / delete: the affected count.
    Write,
    /// Any other command: the reply as a single row.
    Reply,
    /// `create` from `db.createCollection(name, options, { ifNotExists: true })`
    /// (a DBine extension): an existing namespace is not an error.
    CreateIfMissing,    /// A user/role command from `db.runCommand(cmd, options, { ifExists: true })`
    /// (a DBine extension): skipped when its user or role doesn't exist.
    IfExists,
}

#[derive(Debug, Clone, PartialEq)]
pub struct Stmt {
    pub cmd: Document,
    pub shape: Shape,
    /// Run against `admin` (`db.adminCommand`).
    pub admin: bool,
}

type R<T> = std::result::Result<T, String>;

/// Parse a script: statements are `db.…` calls or `{…}` command documents,
/// separated by `;` or simply following each other (a newline is enough;
/// a line starting with `.` continues the previous call chain).
pub fn parse_script(text: &str) -> R<Vec<Stmt>> {
    Ok(parse_script_text(text)?.into_iter().map(|(_, s)| s).collect())
}

/// [`parse_script`], with each statement's source text.
pub fn parse_script_text(text: &str) -> R<Vec<(String, Stmt)>> {
    let mut p = P { s: text.chars().collect(), i: 0 };
    let mut out = Vec::new();
    loop {
        p.ws();
        while p.peek() == Some(';') {
            p.i += 1;
            p.ws();
        }
        let Some(c) = p.peek() else { break };
        let start = p.i;
        let stmt = if c == '{' { command_stmt(to_doc(p.value()?)?, false) } else { p.db_call()? };
        let src: String = p.s[start..p.i].iter().collect();
        out.push((src.trim().to_string(), stmt));
    }
    Ok(out)
}

/// How a raw command document runs, from its first key.
pub fn command_stmt(cmd: Document, admin: bool) -> Stmt {
    let name = cmd.keys().next().map(|k| k.to_ascii_lowercase()).unwrap_or_default();
    let explain = cmd.get_bool("explain").unwrap_or(false);
    let shape = match name.as_str() {
        "find" | "listcollections" | "listindexes" => Shape::Cursor,
        "aggregate" if !explain => Shape::Cursor,
        "insert" | "update" | "delete" => Shape::Write,
        _ => Shape::Reply,
    };
    Stmt { cmd, shape, admin }
}

/// Commands allowed on a read-only connection (lowercase).
const READ_COMMANDS: &[&str] = &[
    "find",
    "aggregate",
    "count",
    "distinct",
    "listcollections",
    "listindexes",
    "listdatabases",
    "dbstats",
    "collstats",
    "datasize",
    "serverstatus",
    "buildinfo",
    "hostinfo",
    "ping",
    "hello",
    "ismaster",
    "connectionstatus",
    "explain",
    "currentop",
    "top",
    "getlog",
    "getparameter",
    "getcmdlineopts",
    "listcommands",
    "usersinfo",
    "rolesinfo",
    "replsetgetstatus",
    "replsetgetconfig",
    "whatsmyuri",
    "features",
];

/// `Some(reason)` when the command writes (read-only connections refuse it).
pub fn write_reason(cmd: &Document) -> Option<String> {
    let Some((name, val)) = cmd.iter().next() else { return Some("comando vacío".into()) };
    let lname = name.to_ascii_lowercase();
    if !READ_COMMANDS.contains(&lname.as_str()) {
        return Some(name.clone());
    }
    match lname.as_str() {
        "aggregate" => {
            let stages = cmd.get_array("pipeline").map(|a| a.as_slice()).unwrap_or(&[]);
            stages.iter().find_map(|s| {
                let d = s.as_document()?;
                d.keys().find(|k| *k == "$out" || *k == "$merge").cloned()
            })
        }
        "explain" => val.as_document().and_then(write_reason),
        _ => None,
    }
}

/// A single relaxed-JSON value (shell helpers allowed), as extended JSON.
pub fn parse_value(text: &str) -> R<Value> {
    let mut p = P { s: text.chars().collect(), i: 0 };
    let v = p.value()?;
    p.ws();
    if p.peek().is_some() {
        return p.err("sobra texto después del valor");
    }
    Ok(v)
}

/// `db.<method>(…)`: methods on the database itself.
fn db_method(method: &str, args: Vec<Value>) -> R<Stmt> {
    let mut a = args.into_iter();
    let first = a.next();
    let name_arg = |v: Option<Value>| match v {
        Some(Value::String(s)) if !s.is_empty() => Ok(s),
        _ => Err(format!("{method} necesita el nombre como texto")),
    };
    let stmt = match method {
        "getCollectionNames" => Stmt {
            cmd: doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true },
            shape: Shape::Cursor,
            admin: false,
        },
        "getCollectionInfos" => {
            let mut cmd = doc! { "listCollections": 1 };
            if let Some(f) = first {
                cmd.insert("filter", to_doc(f)?);
            }
            Stmt { cmd, shape: Shape::Cursor, admin: false }
        }
        "runCommand" | "adminCommand" => {
            let cmd = match first {
                Some(Value::String(name)) => doc! { name: 1 },
                Some(v) => to_doc(v)?,
                None => return Err(format!("{method} necesita un documento")),
            };
            let _options = a.next();
            let guard = opt_doc(&mut a)?.is_some_and(|g| g.get_bool("ifExists").unwrap_or(false));
            let mut st = command_stmt(cmd, method == "adminCommand");
            if guard {
                st.shape = Shape::IfExists;
            }
            st
        }
        // db.createCollection(name, options[, { ifNotExists: true }])
        "createCollection" => {
            let mut cmd = doc! { "create": name_arg(first)? };
            if let Some(o) = opt_doc(&mut a)? {
                cmd.extend(o);
            }
            let guard = opt_doc(&mut a)?.is_some_and(|g| g.get_bool("ifNotExists").unwrap_or(false));
            Stmt { cmd, shape: if guard { Shape::CreateIfMissing } else { Shape::Reply }, admin: false }
        }
        // db.createView(name, source, pipeline[, options])
        "createView" => {
            let name = name_arg(first)?;
            let source = name_arg(a.next())?;
            let pipeline: Vec<Bson> = match a.next() {
                Some(Value::Array(st)) => st.into_iter().map(to_bson).collect::<R<_>>()?,
                None => Vec::new(),
                Some(other) => return Err(format!("createView espera un array de etapas, no {other}")),
            };
            let mut cmd = doc! { "create": name, "viewOn": source, "pipeline": pipeline };
            if let Some(o) = opt_doc(&mut a)? {
                cmd.extend(o);
            }
            command_stmt(cmd, false)
        }
        "dropDatabase" => command_stmt(doc! { "dropDatabase": 1 }, false),
        "stats" => command_stmt(doc! { "dbStats": 1 }, false),
        "serverStatus" => command_stmt(doc! { "serverStatus": 1 }, false),
        "hostInfo" => command_stmt(doc! { "hostInfo": 1 }, false),
        "version" | "serverBuildInfo" => command_stmt(doc! { "buildInfo": 1 }, false),
        "currentOp" => command_stmt(doc! { "currentOp": 1 }, true),
        other => return Err(format!("método de base no soportado: db.{other}()")),
    };
    Ok(stmt)
}

/// The shell's default index name: `a_1_b_-1`, `body_text`.
pub fn index_name(keys: &Document) -> String {
    keys.iter()
        .map(|(k, v)| {
            let v = match v {
                Bson::String(s) => s.clone(),
                Bson::Int32(n) => n.to_string(),
                Bson::Int64(n) => n.to_string(),
                Bson::Double(n) if n.fract() == 0.0 => (*n as i64).to_string(),
                other => other.to_string(),
            };
            format!("{k}_{v}")
        })
        .collect::<Vec<_>>()
        .join("_")
}

/// One `createIndexes` entry: key, name (the shell's default when absent)
/// and the options.
fn index_spec(keys: Value, opts: Option<&Document>) -> R<Document> {
    let key = to_doc(keys)?;
    if key.is_empty() {
        return Err("el índice necesita al menos un campo".into());
    }
    let mut spec = doc! { "key": key.clone() };
    if let Some(o) = opts {
        spec.extend(o.clone());
    }
    if !spec.contains_key("name") {
        spec.insert("name", index_name(&key));
    }
    Ok(spec)
}

fn to_doc(v: Value) -> R<Document> {
    match v {
        Value::Object(m) => Document::try_from(m).map_err(|e| format!("documento inválido: {e}")),
        other => Err(format!("se esperaba un documento {{…}} y llegó {other}")),
    }
}

fn to_bson(v: Value) -> R<Bson> {
    Bson::try_from(v).map_err(|e| format!("valor inválido: {e}"))
}

struct P {
    s: Vec<char>,
    i: usize,
}

impl P {
    fn peek(&self) -> Option<char> {
        self.s.get(self.i).copied()
    }

    fn at(&self, off: usize) -> Option<char> {
        self.s.get(self.i + off).copied()
    }

    fn err<T>(&self, msg: impl std::fmt::Display) -> R<T> {
        let line = self.s[..self.i.min(self.s.len())].iter().filter(|c| **c == '\n').count() + 1;
        Err(format!("línea {line}: {msg}"))
    }

    /// Skip whitespace and `//` / `/* */` comments.
    fn ws(&mut self) {
        loop {
            match self.peek() {
                Some(c) if c.is_whitespace() => self.i += 1,
                Some('/') if self.at(1) == Some('/') => {
                    while let Some(c) = self.peek() {
                        self.i += 1;
                        if c == '\n' {
                            break;
                        }
                    }
                }
                Some('/') if self.at(1) == Some('*') => {
                    self.i += 2;
                    while self.peek().is_some() && !(self.peek() == Some('*') && self.at(1) == Some('/')) {
                        self.i += 1;
                    }
                    self.i = (self.i + 2).min(self.s.len());
                }
                _ => return,
            }
        }
    }

    fn eat(&mut self, c: char) -> bool {
        self.ws();
        if self.peek() == Some(c) {
            self.i += 1;
            true
        } else {
            false
        }
    }

    fn expect(&mut self, c: char) -> R<()> {
        if self.eat(c) {
            Ok(())
        } else {
            match self.peek() {
                Some(got) => self.err(format!("se esperaba '{c}' y se encontró '{got}'")),
                None => self.err(format!("se esperaba '{c}' y terminó el texto")),
            }
        }
    }

    fn ident(&mut self) -> Option<String> {
        self.ws();
        let start = self.i;
        while let Some(c) = self.peek() {
            let ok = c.is_alphanumeric() || c == '_' || c == '$';
            if !ok || (self.i == start && c.is_ascii_digit()) {
                break;
            }
            self.i += 1;
        }
        (self.i > start).then(|| self.s[start..self.i].iter().collect())
    }

    /// `(a, b, …)` after the opening parenthesis was seen (not consumed).
    fn args(&mut self) -> R<Vec<Value>> {
        self.expect('(')?;
        let mut out = Vec::new();
        loop {
            if self.eat(')') {
                return Ok(out);
            }
            out.push(self.value()?);
            if !self.eat(',') {
                self.expect(')')?;
                return Ok(out);
            }
        }
    }

    /// `db.<coll>.<method>(…)…` or `db.<method>(…)`.
    fn db_call(&mut self) -> R<Stmt> {
        match self.ident() {
            Some(w) if w == "db" => {}
            Some(w) => return self.err(format!("se esperaba `db.` o un documento {{…}}, no `{w}`")),
            None => return self.err(format!("se esperaba `db.` o un documento {{…}}, no '{}'", self.peek().unwrap_or(' '))),
        }
        // Collection name segments until one is followed by `(`.
        let mut parts: Vec<String> = Vec::new();
        let (method, args) = loop {
            self.ws();
            match self.peek() {
                Some('.') => {
                    self.i += 1;
                    let Some(name) = self.ident() else { return self.err("se esperaba un nombre después de '.'") };
                    self.ws();
                    if self.peek() == Some('(') {
                        break (name, self.args()?);
                    }
                    parts.push(name);
                }
                Some('[') => {
                    self.i += 1;
                    self.ws();
                    let Value::String(name) = self.value()? else { return self.err("se esperaba un nombre entre comillas") };
                    self.expect(']')?;
                    parts.push(name);
                }
                _ => return self.err("se esperaba una llamada, p. ej. db.coleccion.find({})"),
            }
        };
        let (coll, method, args) = if parts.is_empty() {
            if method == "getCollection" {
                let Some(Value::String(name)) = args.into_iter().next() else {
                    return self.err("getCollection necesita el nombre de la colección");
                };
                self.expect('.')?;
                let Some(m) = self.ident() else { return self.err("se esperaba un método de colección") };
                let a = self.args()?;
                (name, m, a)
            } else {
                return db_method(&method, args).or_else(|e| self.err(e));
            }
        } else {
            (parts.join("."), method, args)
        };
        let mut stmt = coll_method(&coll, &method, args).or_else(|e| self.err(e))?;
        // Chained modifiers: .sort(…).limit(…)…
        loop {
            self.ws();
            if self.peek() != Some('.') {
                break;
            }
            self.i += 1;
            let Some(m) = self.ident() else { return self.err("se esperaba un método después de '.'") };
            let a = if self.eat_peek('(') { self.args()? } else { Vec::new() };
            modifier(&mut stmt, &m, a).or_else(|e| self.err(e))?;
        }
        Ok(stmt)
    }

    fn eat_peek(&mut self, c: char) -> bool {
        self.ws();
        self.peek() == Some(c)
    }

    fn value(&mut self) -> R<Value> {
        self.ws();
        let Some(c) = self.peek() else { return self.err("se esperaba un valor y terminó el texto") };
        match c {
            '{' => {
                self.i += 1;
                let mut m = Map::new();
                loop {
                    if self.eat('}') {
                        return Ok(Value::Object(m));
                    }
                    let key = self.key()?;
                    self.expect(':')?;
                    let v = self.value()?;
                    m.insert(key, v);
                    if !self.eat(',') {
                        self.expect('}')?;
                        return Ok(Value::Object(m));
                    }
                }
            }
            '[' => {
                self.i += 1;
                let mut a = Vec::new();
                loop {
                    if self.eat(']') {
                        return Ok(Value::Array(a));
                    }
                    a.push(self.value()?);
                    if !self.eat(',') {
                        self.expect(']')?;
                        return Ok(Value::Array(a));
                    }
                }
            }
            '"' | '\'' => self.string().map(Value::String),
            '/' => self.regex(),
            c if c.is_ascii_digit() || c == '-' || c == '+' || c == '.' => self.number(),
            _ => {
                let Some(mut w) = self.ident() else { return self.err(format!("valor inesperado '{c}'")) };
                if w == "new" {
                    let Some(n) = self.ident() else { return self.err("se esperaba un constructor después de `new`") };
                    w = n;
                }
                match w.as_str() {
                    "true" => Ok(Value::Bool(true)),
                    "false" => Ok(Value::Bool(false)),
                    "null" | "undefined" => Ok(Value::Null),
                    "NaN" => Ok(json!({ "$numberDouble": "NaN" })),
                    "Infinity" => Ok(json!({ "$numberDouble": "Infinity" })),
                    "MinKey" | "MaxKey" => {
                        if self.eat_peek('(') {
                            self.args()?;
                        }
                        Ok(if w == "MinKey" { json!({ "$minKey": 1 }) } else { json!({ "$maxKey": 1 }) })
                    }
                    _ if self.eat_peek('(') => {
                        let args = self.args()?;
                        constructor(&w, args).or_else(|e| self.err(e))
                    }
                    _ => self.err(format!("identificador desconocido `{w}`")),
                }
            }
        }
    }

    fn key(&mut self) -> R<String> {
        self.ws();
        match self.peek() {
            Some('"') | Some('\'') => self.string(),
            _ => {
                let start = self.i;
                while let Some(c) = self.peek() {
                    if c.is_alphanumeric() || c == '_' || c == '$' || c == '.' || c == '-' {
                        self.i += 1;
                    } else {
                        break;
                    }
                }
                if self.i == start {
                    return self.err("se esperaba una clave");
                }
                Ok(self.s[start..self.i].iter().collect())
            }
        }
    }

    fn string(&mut self) -> R<String> {
        let q = self.peek().expect("a quote");
        self.i += 1;
        let mut out = String::new();
        loop {
            let Some(c) = self.peek() else { return self.err("texto sin cerrar") };
            self.i += 1;
            match c {
                c if c == q => return Ok(out),
                '\\' => {
                    let Some(e) = self.peek() else { return self.err("texto sin cerrar") };
                    self.i += 1;
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex: String = self.s.get(self.i..self.i + 4).map(|h| h.iter().collect()).unwrap_or_default();
                            let Some(ch) = u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32) else {
                                return self.err("escape \\u inválido");
                            };
                            self.i += 4;
                            out.push(ch);
                        }
                        other => out.push(other),
                    }
                }
                c => out.push(c),
            }
        }
    }

    fn number(&mut self) -> R<Value> {
        let start = self.i;
        while let Some(c) = self.peek() {
            if c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '+') {
                self.i += 1;
            } else {
                break;
            }
        }
        let raw: String = self.s[start..self.i].iter().collect();
        let t = raw.trim_start_matches('+');
        if let Ok(i) = t.parse::<i64>() {
            return Ok(Value::from(i));
        }
        if let Some(h) = t.strip_prefix("0x").or_else(|| t.strip_prefix("0X")) {
            if let Ok(i) = i64::from_str_radix(h, 16) {
                return Ok(Value::from(i));
            }
        }
        match t {
            "-Infinity" => return Ok(json!({ "$numberDouble": "-Infinity" })),
            "Infinity" => return Ok(json!({ "$numberDouble": "Infinity" })),
            _ => {}
        }
        match t.parse::<f64>() {
            Ok(f) if f.is_finite() => Ok(json!(f)),
            _ => self.err(format!("número inválido `{raw}`")),
        }
    }

    fn regex(&mut self) -> R<Value> {
        self.i += 1;
        let mut pat = String::new();
        let mut in_class = false;
        loop {
            let Some(c) = self.peek() else { return self.err("expresión regular sin cerrar") };
            self.i += 1;
            match c {
                '\\' => {
                    pat.push(c);
                    if let Some(n) = self.peek() {
                        pat.push(n);
                        self.i += 1;
                    }
                }
                '[' => {
                    in_class = true;
                    pat.push(c);
                }
                ']' => {
                    in_class = false;
                    pat.push(c);
                }
                '/' if !in_class => break,
                c => pat.push(c),
            }
        }
        let mut flags = String::new();
        while let Some(c) = self.peek().filter(|c| c.is_ascii_alphabetic()) {
            flags.push(c);
            self.i += 1;
        }
        Ok(regex_value(&pat, &flags))
    }
}

fn regex_value(pattern: &str, flags: &str) -> Value {
    let mut f: Vec<char> = flags.chars().collect();
    f.sort_unstable();
    f.dedup();
    json!({ "$regularExpression": { "pattern": pattern, "options": f.into_iter().collect::<String>() } })
}

fn arg_str(args: &[Value], i: usize) -> Option<String> {
    match args.get(i)? {
        Value::String(s) => Some(s.clone()),
        Value::Number(n) => Some(n.to_string()),
        _ => None,
    }
}

/// Shell constructors → extended JSON.
fn constructor(name: &str, args: Vec<Value>) -> R<Value> {
    Ok(match name {
        "ObjectId" => match arg_str(&args, 0) {
            Some(hex) => {
                ObjectId::parse_str(&hex).map_err(|_| format!("ObjectId inválido: \"{hex}\""))?;
                json!({ "$oid": hex })
            }
            None => json!({ "$oid": ObjectId::new().to_hex() }),
        },
        "ISODate" | "Date" => {
            let ms = match args.first() {
                None => Utc::now().timestamp_millis(),
                Some(Value::Number(n)) => n.as_f64().unwrap_or(0.0) as i64,
                Some(Value::String(s)) => parse_date(s).ok_or_else(|| format!("fecha inválida: \"{s}\""))?,
                Some(other) => return Err(format!("fecha inválida: {other}")),
            };
            json!({ "$date": { "$numberLong": ms.to_string() } })
        }
        "NumberLong" | "Long" => {
            let s = arg_str(&args, 0).unwrap_or_else(|| "0".into());
            s.parse::<i64>().map_err(|_| format!("NumberLong inválido: {s}"))?;
            json!({ "$numberLong": s })
        }
        "NumberInt" | "Int32" => {
            let s = arg_str(&args, 0).unwrap_or_else(|| "0".into());
            let n: i32 = s.parse().map_err(|_| format!("NumberInt inválido: {s}"))?;
            json!({ "$numberInt": n.to_string() })
        }
        "NumberDecimal" | "Decimal128" => json!({ "$numberDecimal": arg_str(&args, 0).unwrap_or_else(|| "0".into()) }),
        "Double" => json!(args.first().and_then(Value::as_f64).unwrap_or(0.0)),
        "Timestamp" => {
            let t = args.first().and_then(Value::as_u64).unwrap_or(0);
            let i = args.get(1).and_then(Value::as_u64).unwrap_or(0);
            json!({ "$timestamp": { "t": t, "i": i } })
        }
        "UUID" => {
            let hex: String = arg_str(&args, 0).unwrap_or_default().chars().filter(|c| *c != '-').collect();
            let bytes = decode_hex(&hex).filter(|b| b.len() == 16).ok_or_else(|| format!("UUID inválido: {hex}"))?;
            json!({ "$binary": { "base64": b64(&bytes), "subType": "04" } })
        }
        "BinData" => {
            let sub = args.first().and_then(Value::as_u64).unwrap_or(0);
            let data = arg_str(&args, 1).unwrap_or_default();
            json!({ "$binary": { "base64": data, "subType": format!("{sub:02x}") } })
        }
        "RegExp" => regex_value(&arg_str(&args, 0).unwrap_or_default(), &arg_str(&args, 1).unwrap_or_default()),
        other => return Err(format!("constructor desconocido `{other}(…)`")),
    })
}

/// Milliseconds since the epoch; dates without a zone are UTC.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    for f in ["%Y-%m-%dT%H:%M:%S%.f", "%Y-%m-%d %H:%M:%S%.f", "%Y-%m-%dT%H:%M"] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), f) {
            return Some(d.and_utc().timestamp_millis());
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| d.and_hms_opt(0, 0, 0).expect("midnight").and_utc().timestamp_millis())
}

fn decode_hex(h: &str) -> Option<Vec<u8>> {
    if h.len() % 2 != 0 {
        return None;
    }
    (0..h.len()).step_by(2).map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok()).collect()
}

fn b64(bytes: &[u8]) -> String {
    use base64::Engine;
    base64::engine::general_purpose::STANDARD.encode(bytes)
}

fn opt_doc(args: &mut std::vec::IntoIter<Value>) -> R<Option<Document>> {
    match args.next() {
        None | Some(Value::Null) => Ok(None),
        Some(v) => to_doc(v).map(Some),
    }
}

fn filter_arg(args: &mut std::vec::IntoIter<Value>) -> R<Document> {
    Ok(opt_doc(args)?.unwrap_or_default())
}

/// Update/delete options the server takes per statement.
const UPDATE_OPTS: &[&str] = &["upsert", "arrayFilters", "hint", "collation"];

fn coll_method(coll: &str, method: &str, args: Vec<Value>) -> R<Stmt> {
    let mut a = args.into_iter();
    let stmt = |cmd, shape| Stmt { cmd, shape, admin: false };
    Ok(match method {
        "find" | "findOne" => {
            let mut cmd = doc! { "find": coll, "filter": filter_arg(&mut a)? };
            if let Some(p) = opt_doc(&mut a)? {
                cmd.insert("projection", p);
            }
            if method == "findOne" {
                cmd.insert("limit", 1);
                cmd.insert("singleBatch", true);
            }
            stmt(cmd, Shape::Cursor)
        }
        "aggregate" => {
            let first = a.next().unwrap_or(Value::Array(Vec::new()));
            let (pipeline, opts) = match first {
                Value::Array(stages) => (stages, opt_doc(&mut a)?),
                stage @ Value::Object(_) => (std::iter::once(stage).chain(a.by_ref()).collect(), None),
                other => return Err(format!("aggregate espera un array de etapas, no {other}")),
            };
            let pipeline: Vec<Bson> = pipeline.into_iter().map(to_bson).collect::<R<_>>()?;
            let mut cmd = doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} };
            if let Some(o) = opts {
                for (k, v) in o {
                    if k == "batchSize" {
                        cmd.insert("cursor", doc! { "batchSize": v });
                    } else {
                        cmd.insert(k, v);
                    }
                }
            }
            let shape = if cmd.get_bool("explain").unwrap_or(false) { Shape::Reply } else { Shape::Cursor };
            stmt(cmd, shape)
        }
        "countDocuments" => {
            let mut pipeline = vec![Bson::Document(doc! { "$match": filter_arg(&mut a)? })];
            if let Some(o) = opt_doc(&mut a)? {
                if let Some(s) = o.get("skip") {
                    pipeline.push(Bson::Document(doc! { "$skip": s.clone() }));
                }
                if let Some(l) = o.get("limit") {
                    pipeline.push(Bson::Document(doc! { "$limit": l.clone() }));
                }
            }
            pipeline.push(Bson::Document(doc! { "$group": { "_id": 1, "count": { "$sum": 1 } } }));
            stmt(doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} }, Shape::CountAgg)
        }
        "estimatedDocumentCount" => stmt(doc! { "count": coll }, Shape::Count),
        "count" => stmt(doc! { "count": coll, "query": filter_arg(&mut a)? }, Shape::Count),
        "distinct" => {
            let Some(Value::String(key)) = a.next() else { return Err("distinct necesita el nombre del campo".into()) };
            let cmd = doc! { "distinct": coll, "key": key.as_str(), "query": filter_arg(&mut a)? };
            stmt(cmd, Shape::Distinct(key))
        }
        "getIndexes" => stmt(doc! { "listIndexes": coll }, Shape::Cursor),
        "stats" => stmt(doc! { "collStats": coll }, Shape::Reply),
        "insertOne" | "insert" | "insertMany" => {
            let docs: Vec<Bson> = match a.next() {
                Some(Value::Array(v)) => v.into_iter().map(|d| to_doc(d).map(Bson::Document)).collect::<R<_>>()?,
                Some(v) => vec![Bson::Document(to_doc(v)?)],
                None => return Err(format!("{method} necesita un documento")),
            };
            let mut cmd = doc! { "insert": coll, "documents": docs };
            if let Some(o) = opt_doc(&mut a)? {
                if let Some(ordered) = o.get("ordered") {
                    cmd.insert("ordered", ordered.clone());
                }
            }
            stmt(cmd, Shape::Write)
        }
        "updateOne" | "updateMany" | "replaceOne" => {
            let q = filter_arg(&mut a)?;
            let u = match a.next() {
                Some(Value::Array(p)) if method != "replaceOne" => Bson::Array(p.into_iter().map(to_bson).collect::<R<_>>()?),
                Some(v) => Bson::Document(to_doc(v)?),
                None => return Err(format!("{method} necesita el documento de cambios")),
            };
            let mut one = doc! { "q": q, "u": u, "multi": method == "updateMany" };
            if let Some(o) = opt_doc(&mut a)? {
                for k in UPDATE_OPTS {
                    if let Some(v) = o.get(*k) {
                        one.insert(*k, v.clone());
                    }
                }
            }
            stmt(doc! { "update": coll, "updates": [one] }, Shape::Write)
        }
        "deleteOne" | "deleteMany" | "remove" => {
            let q = filter_arg(&mut a)?;
            let limit = if method == "deleteOne" { 1 } else { 0 };
            stmt(doc! { "delete": coll, "deletes": [{ "q": q, "limit": limit }] }, Shape::Write)
        }
        "drop" => stmt(doc! { "drop": coll }, Shape::Reply),
        // db.c.createIndex(keys[, options]) / createIndexes([keys…][, options])
        "createIndex" | "ensureIndex" => {
            let keys = a.next().ok_or_else(|| format!("{method} necesita las claves del índice"))?;
            let opts = opt_doc(&mut a)?;
            stmt(doc! { "createIndexes": coll, "indexes": [index_spec(keys, opts.as_ref())?] }, Shape::Reply)
        }
        "createIndexes" => {
            let Some(Value::Array(all)) = a.next() else { return Err("createIndexes espera un array de claves".into()) };
            let opts = opt_doc(&mut a)?;
            let specs: Vec<Bson> = all.into_iter().map(|k| index_spec(k, opts.as_ref()).map(Bson::Document)).collect::<R<_>>()?;
            stmt(doc! { "createIndexes": coll, "indexes": specs }, Shape::Reply)
        }
        "dropIndex" | "dropIndexes" => {
            let index = match a.next() {
                None if method == "dropIndexes" => Bson::String("*".into()),
                None => return Err("dropIndex necesita el nombre o las claves del índice".into()),
                Some(v) => to_bson(v)?,
            };
            stmt(doc! { "dropIndexes": coll, "index": index }, Shape::Reply)
        }
        other => return Err(format!("método no soportado: db.{coll}.{other}()")),
    })
}

/// Cursor modifiers chained after `find`/`aggregate`.
fn modifier(stmt: &mut Stmt, name: &str, args: Vec<Value>) -> R<()> {
    let is_find = stmt.cmd.contains_key("find");
    let mut a = args.into_iter();
    let first = a.next();
    let need_find = |what: &str| -> R<()> {
        if is_find {
            Ok(())
        } else {
            Err(format!(".{what}() solo se puede encadenar a find()"))
        }
    };
    match name {
        "pretty" | "toArray" => {}
        "explain" => {
            let verbosity = match first {
                Some(Value::String(v)) => v,
                Some(Value::Bool(true)) => "allPlansExecution".into(),
                _ => "queryPlanner".into(),
            };
            let inner = std::mem::take(&mut stmt.cmd);
            stmt.cmd = doc! { "explain": inner, "verbosity": verbosity };
            stmt.shape = Shape::Reply;
        }
        "sort" | "projection" | "hint" | "collation" | "max" | "min" => {
            need_find(name)?;
            let v = first.ok_or_else(|| format!(".{name}() necesita un argumento"))?;
            stmt.cmd.insert(name, to_bson(v)?);
        }
        "limit" | "skip" | "batchSize" | "maxTimeMS" => {
            let n = first.as_ref().and_then(Value::as_i64).ok_or_else(|| format!(".{name}() necesita un número"))?;
            if name == "maxTimeMS" {
                stmt.cmd.insert(name, n);
                return Ok(());
            }
            need_find(name)?;
            if name == "limit" && n < 0 {
                stmt.cmd.insert("limit", -n);
                stmt.cmd.insert("singleBatch", true);
            } else {
                stmt.cmd.insert(name, n);
            }
        }
        "count" | "itcount" | "size" => {
            need_find(name)?;
            let mut c = doc! { "count": stmt.cmd.get_str("find").unwrap_or_default() };
            if let Ok(f) = stmt.cmd.get_document("filter") {
                c.insert("query", f.clone());
            }
            if name != "count" {
                for k in ["limit", "skip"] {
                    if let Some(v) = stmt.cmd.get(k) {
                        c.insert(k, v.clone());
                    }
                }
            }
            stmt.cmd = c;
            stmt.shape = Shape::Count;
        }
        other => return Err(format!("modificador no soportado: .{other}()")),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn one(s: &str) -> Stmt {
        let mut v = parse_script(s).unwrap();
        assert_eq!(v.len(), 1, "{v:?}");
        v.remove(0)
    }

    #[test]
    fn find_with_modifiers_over_lines() {
        let s = one("db.users.find({ age: { $gt: 30 }, name: 'x' }, {name: 1})\n  .sort({age: -1})\n  .limit(5).skip(2)");
        assert_eq!(s.shape, Shape::Cursor);
        assert_eq!(
            s.cmd,
            doc! { "find": "users", "filter": { "age": { "$gt": 30 }, "name": "x" }, "projection": { "name": 1 },
            "sort": { "age": -1 }, "limit": 5_i64, "skip": 2_i64 }
        );
    }

    #[test]
    fn several_statements_by_newline_and_semicolon() {
        let v = parse_script("db.a.find()\ndb.b.countDocuments({})  ; db.getCollectionNames()\n{ ping: 1 }").unwrap();
        assert_eq!(v.len(), 4);
        assert_eq!(v[1].shape, Shape::CountAgg);
        assert_eq!(v[2].cmd.get_bool("nameOnly"), Ok(true));
        assert_eq!(v[3].cmd, doc! { "ping": 1 });
        assert_eq!(v[3].shape, Shape::Reply);
    }

    #[test]
    fn shell_constructors_become_bson() {
        let s = one(r#"db.c.find({_id: ObjectId("65a1b2c3d4e5f60718293a4b"), at: {$gte: ISODate("2024-01-31")}, n: NumberLong("9007199254740993"), r: /ab\/c/i, trailing: [1,2,],})"#);
        let f = s.cmd.get_document("filter").unwrap();
        assert_eq!(f.get_object_id("_id").unwrap().to_hex(), "65a1b2c3d4e5f60718293a4b");
        let at = f.get_document("at").unwrap().get_datetime("$gte").unwrap();
        assert_eq!(at.timestamp_millis(), 1_706_659_200_000);
        assert_eq!(f.get_i64("n"), Ok(9_007_199_254_740_993));
        match f.get("r") {
            Some(Bson::RegularExpression(r)) => {
                assert_eq!(r.pattern, "ab\\/c");
                assert_eq!(r.options, "i");
            }
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn raw_command_documents() {
        let v = parse_script(r#"{ "find": "coll", "filter": {"a": 1} } { aggregate: 'c', pipeline: [], explain: true }"#).unwrap();
        assert_eq!(v[0].shape, Shape::Cursor);
        assert_eq!(v[1].shape, Shape::Reply);
    }

    #[test]
    fn collection_names_with_dots_and_brackets() {
        assert_eq!(one("db.system.views.find()").cmd.get_str("find"), Ok("system.views"));
        assert_eq!(one("db['my-coll'].find()").cmd.get_str("find"), Ok("my-coll"));
        assert_eq!(one("db.getCollection(\"a b\").find({}).limit(1)").cmd.get_str("find"), Ok("a b"));
    }

    #[test]
    fn writes_and_distinct() {
        let s = one("db.c.updateMany({a: 1}, {$set: {b: 2}}, {upsert: true})");
        assert_eq!(s.shape, Shape::Write);
        let u = s.cmd.get_array("updates").unwrap()[0].as_document().unwrap().clone();
        assert_eq!(u.get_bool("multi"), Ok(true));
        assert_eq!(u.get_bool("upsert"), Ok(true));
        let d = one("db.c.distinct('city', {active: true})");
        assert_eq!(d.shape, Shape::Distinct("city".into()));
        let del = one("db.c.deleteOne({x: 1})");
        assert_eq!(del.cmd.get_array("deletes").unwrap()[0].as_document().unwrap().get_i32("limit"), Ok(1));
    }

    #[test]
    fn read_only_whitelist() {
        assert_eq!(write_reason(&one("db.c.find({})").cmd), None);
        assert_eq!(write_reason(&one("db.c.aggregate([{$match: {}}])").cmd), None);
        assert_eq!(write_reason(&one("db.c.aggregate([{$match: {}}, {$out: 'x'}])").cmd).as_deref(), Some("$out"));
        assert_eq!(write_reason(&one("db.c.insertOne({a: 1})").cmd).as_deref(), Some("insert"));
        assert_eq!(write_reason(&one("db.runCommand({dropDatabase: 1})").cmd).as_deref(), Some("dropDatabase"));
        assert_eq!(write_reason(&one("db.c.find().explain()").cmd), None);
        assert_eq!(write_reason(&one("db.c.find().count()").cmd), None);
    }

    #[test]
    fn errors_point_at_the_line() {
        let e = parse_script("db.c.find({})\ndb.c.find({a: })").unwrap_err();
        assert!(e.starts_with("línea 2"), "{e}");
        assert!(parse_script("select * from t").is_err());
        assert!(parse_script("db.c.frobnicate()").is_err());
    }

    #[test]
    fn comments_are_skipped() {
        let v = parse_script("// all\ndb.c.find({ /* none */ })\n/* end */").unwrap();
        assert_eq!(v.len(), 1);
    }
}
