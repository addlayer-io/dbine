//! The MongoDB editor language: mongo-shell-like calls and raw command
//! documents, parsed into database commands.
//!
//! Values are "relaxed JSON" (JSON5-ish): unquoted keys, single quotes,
//! trailing commas, comments, and the shell constructors (`ObjectId("…")`,
//! `ISODate("…")`, `NumberLong(…)`, `/regex/i`…), which become MongoDB
//! extended JSON before turning into BSON.

use chrono::{DateTime, NaiveDate, NaiveDateTime, Utc};
use dbine_driver::{ScriptStatement, StatementKind};
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
    CreateIfMissing,
    /// A user/role command from `db.runCommand(cmd, options, { ifExists: true })`
    /// (a DBine extension): skipped when its user or role doesn't exist.
    IfExists,
    /// `db.c.renameCollection(new)`: `renameCollection` runs on `admin`
    /// with both names qualified with the session's database.
    Rename,
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
#[cfg(test)]
pub fn parse_script(text: &str) -> R<Vec<Stmt>> {
    Ok(parse_script_text(text)?
        .into_iter()
        .map(|(_, s)| s)
        .collect())
}

/// [`parse_script`], with each statement's source text.
pub fn parse_script_text(text: &str) -> R<Vec<(String, Stmt)>> {
    let mut p = P {
        s: text.chars().collect(),
        i: 0,
    };
    let mut out = Vec::new();
    loop {
        p.ws();
        while p.peek() == Some(';') {
            p.i += 1;
            p.ws();
        }
        let Some(c) = p.peek() else { break };
        let start = p.i;
        let stmt = if c == '{' {
            p.value().and_then(to_doc).map(|d| command_stmt(d, false))
        } else {
            p.db_call()
        };
        let stmt = stmt.map_err(|m| p.at_line(m))?;
        let src: String = p.s[start..p.i].iter().collect();
        out.push((src.trim().to_string(), stmt));
    }
    Ok(out)
}

/// What a unit of an editor script does.
#[derive(Debug, Clone, PartialEq)]
pub enum Item {
    Run(Stmt),
    /// `use <db>`: the session's database from here on (as mongosh).
    Use(String),
    /// `show dbs|databases|collections|tables|users|roles` (lowercase).
    Show(String),
}

/// A unit of an editor script, with its place: the byte offsets of its
/// first character and past its last one (the comments and blank lines
/// around it and its `;` left out), and the 1-based line of `start`.
#[derive(Debug, Clone, PartialEq)]
pub struct Unit {
    pub item: Item,
    pub start: usize,
    pub end: usize,
    pub line: u32,
}

/// Where a script doesn't parse (byte offset, 1-based line).
#[derive(Debug, Clone, PartialEq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
    pub line: u32,
}

/// The script's units, as mongosh reads a file: `db.…` calls, `{…}`
/// command documents and the `use` / `show` shell helpers (each on its
/// line). A syntax error anywhere runs nothing, as a script file that
/// doesn't parse.
pub fn parse_units(text: &str) -> std::result::Result<Vec<Unit>, ParseError> {
    pieces(text)
        .into_iter()
        .map(|p| {
            p.parsed.map(|item| Unit {
                item,
                start: p.start,
                end: p.end,
                line: p.line,
            })
        })
        .collect()
}

/// The script's statements for the editor (the run lens, "run the
/// statement at the cursor", named results): the units [`parse_units`]
/// gives `execute`, which numbers its results by them, so both agree on
/// where each statement starts and ends and on its number. A unit that
/// doesn't parse is still one, bounded by its brackets, strings and
/// comments ([`unit_end`]): the statements around one being written keep
/// theirs, and running it reports the syntax error at its place.
pub(crate) fn statements(text: &str) -> Vec<ScriptStatement> {
    pieces(text)
        .into_iter()
        .map(|p| ScriptStatement {
            text: text[p.start..p.end].to_string(),
            start: p.start,
            end: p.end,
            line: p.line,
            kind: StatementKind::Sql,
            repeat: 1,
            error: None,
        })
        .collect()
}

/// A unit of the script: what it does, or why it doesn't parse.
struct Piece {
    parsed: std::result::Result<Item, ParseError>,
    start: usize,
    end: usize,
    line: u32,
}

/// Every unit of the script, in order: after one that doesn't parse, the
/// parser goes on where [`unit_end`] says it ends.
fn pieces(text: &str) -> Vec<Piece> {
    let mut p = P {
        s: text.chars().collect(),
        i: 0,
    };
    let bytes: Vec<usize> = text
        .char_indices()
        .map(|(b, _)| b)
        .chain(std::iter::once(text.len()))
        .collect();
    let breaks: Vec<usize> = text.match_indices('\n').map(|(b, _)| b).collect();
    let line = |b: usize| breaks.partition_point(|&n| n < b) as u32 + 1;
    let mut out = Vec::new();
    loop {
        p.ws();
        while p.peek() == Some(';') {
            p.i += 1;
            p.ws();
        }
        let Some(c) = p.peek() else { return out };
        let first = p.i;
        let item = if c == '{' {
            p.value()
                .and_then(to_doc)
                .map(|d| Item::Run(command_stmt(d, false)))
        } else if let Some(h) = p.helper() {
            h
        } else {
            p.db_call().map(Item::Run)
        };
        let (parsed, last) = match item {
            // A helper takes the rest of its line: its `;` and the blank
            // space before the line break aren't part of it.
            Ok(item) => {
                let mut last = p.i;
                while last > first && (p.s[last - 1].is_whitespace() || p.s[last - 1] == ';') {
                    last -= 1;
                }
                (Ok(item), last)
            }
            Err(m) => {
                let at = bytes[p.i.min(p.s.len())];
                let last = unit_end(&p.s, first).max(first + 1);
                p.i = last;
                // The error carries the line apart (no "línea N: " prefix).
                let e = ParseError {
                    message: m,
                    offset: at,
                    line: line(at),
                };
                (Err(e), last)
            }
        };
        out.push(Piece {
            parsed,
            start: bytes[first],
            end: bytes[last],
            line: line(bytes[first]),
        });
    }
}

/// Where a unit that doesn't parse ends (the char index past its last
/// character), from `start`, its first one. Brackets, strings, regexes and
/// comments are counted as the parser reads them: it ends at a `;` or a
/// line break outside brackets, unless the next line goes on with the call
/// chain (`.limit(…)`), and, with brackets left open (one being written),
/// before a line that starts a `db.` call, which is never a value.
fn unit_end(s: &[char], start: usize) -> usize {
    let mut depth = 0usize;
    let mut i = start;
    let mut end = start;
    while let Some(&c) = s.get(i) {
        match c {
            '/' if matches!(s.get(i + 1), Some('/' | '*')) => {
                i = skip_comment(s, i);
                continue;
            }
            '"' | '\'' | '/' => {
                i = skip_quoted(s, i);
                end = i;
                continue;
            }
            '\n' => {
                let next = skip_blank(s, i);
                let ends = if depth == 0 {
                    s.get(next) != Some(&'.')
                } else {
                    starts_db_call(s, next)
                };
                if ends {
                    return end;
                }
                i = next;
                continue;
            }
            ';' if depth == 0 => return end,
            '(' | '[' | '{' => depth += 1,
            ')' | ']' | '}' => depth = depth.saturating_sub(1),
            c if c.is_whitespace() => {
                i += 1;
                continue;
            }
            _ => {}
        }
        i += 1;
        end = i;
    }
    end
}

/// Past the `//` (up to its line break, not included) or `/* */` comment
/// at `i`.
fn skip_comment(s: &[char], mut i: usize) -> usize {
    if s.get(i + 1) == Some(&'/') {
        while s.get(i).is_some_and(|&c| c != '\n') {
            i += 1;
        }
        return i;
    }
    i += 2;
    while i < s.len() && !(s[i] == '*' && s.get(i + 1) == Some(&'/')) {
        i += 1;
    }
    (i + 2).min(s.len())
}

/// Past the string (`"…"`, `'…'`) or regex (`/…/flags`) at `i`. One left
/// open ends at its line break, so it doesn't take the lines after it.
fn skip_quoted(s: &[char], mut i: usize) -> usize {
    let q = s[i];
    let mut class = false;
    i += 1;
    while let Some(&c) = s.get(i) {
        match c {
            '\n' => return i,
            '\\' => i += 1,
            '[' if q == '/' => class = true,
            ']' if q == '/' => class = false,
            c if c == q && !class => {
                i += 1;
                if q == '/' {
                    while s.get(i).is_some_and(|c| c.is_ascii_alphabetic()) {
                        i += 1;
                    }
                }
                return i;
            }
            _ => {}
        }
        i += 1;
    }
    s.len()
}

/// The first char past the blank space and comments from `i`.
fn skip_blank(s: &[char], mut i: usize) -> usize {
    loop {
        match s.get(i) {
            Some(c) if c.is_whitespace() => i += 1,
            Some('/') if matches!(s.get(i + 1), Some('/' | '*')) => i = skip_comment(s, i),
            _ => return i,
        }
    }
}

/// `db.` or `db[` (blank space allowed before the `.` or `[`) at `i`.
fn starts_db_call(s: &[char], i: usize) -> bool {
    if s.get(i..i + 2) != Some(&['d', 'b'][..]) {
        return false;
    }
    let next = s[i + 2..]
        .iter()
        .find(|c| !c.is_whitespace() || **c == '\n');
    matches!(next, Some('.' | '['))
}

/// How a raw command document runs, from its first key.
pub fn command_stmt(cmd: Document, admin: bool) -> Stmt {
    let name = cmd
        .keys()
        .next()
        .map(|k| k.to_ascii_lowercase())
        .unwrap_or_default();
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
    let Some((name, val)) = cmd.iter().next() else {
        return Some("comando vacío".into());
    };
    let lname = name.to_ascii_lowercase();
    if !READ_COMMANDS.contains(&lname.as_str()) {
        return Some(name.clone());
    }
    match lname.as_str() {
        "aggregate" => {
            let stages = cmd
                .get_array("pipeline")
                .map(|a| a.as_slice())
                .unwrap_or(&[]);
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
    let mut p = P {
        s: text.chars().collect(),
        i: 0,
    };
    let v = p.value().map_err(|m| p.at_line(m))?;
    p.ws();
    if p.peek().is_some() {
        return Err(p.at_line("sobra texto después del valor".into()));
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
            Stmt {
                cmd,
                shape: Shape::Cursor,
                admin: false,
            }
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
            let guard =
                opt_doc(&mut a)?.is_some_and(|g| g.get_bool("ifNotExists").unwrap_or(false));
            Stmt {
                cmd,
                shape: if guard {
                    Shape::CreateIfMissing
                } else {
                    Shape::Reply
                },
                admin: false,
            }
        }
        // db.createView(name, source, pipeline[, options])
        "createView" => {
            let name = name_arg(first)?;
            let source = name_arg(a.next())?;
            let pipeline: Vec<Bson> = match a.next() {
                Some(Value::Array(st)) => st.into_iter().map(to_bson).collect::<R<_>>()?,
                None => Vec::new(),
                Some(other) => {
                    return Err(format!("createView espera un array de etapas, no {other}"))
                }
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

    /// The error, without its line: the callers that return a plain text
    /// add it ([`P::at_line`]); [`parse_units`] carries it apart.
    fn err<T>(&self, msg: impl std::fmt::Display) -> R<T> {
        Err(msg.to_string())
    }

    /// `msg` with the line the parser stopped at, as one message.
    fn at_line(&self, msg: String) -> String {
        let line = self.s[..self.i.min(self.s.len())]
            .iter()
            .filter(|c| **c == '\n')
            .count()
            + 1;
        format!("línea {line}: {msg}")
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
                    while self.peek().is_some()
                        && !(self.peek() == Some('*') && self.at(1) == Some('/'))
                    {
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
            Some(w) => {
                return self.err(format!("se esperaba `db.` o un documento {{…}}, no `{w}`"))
            }
            None => {
                return self.err(format!(
                    "se esperaba `db.` o un documento {{…}}, no '{}'",
                    self.peek().unwrap_or(' ')
                ))
            }
        }
        // Collection name segments until one is followed by `(`.
        let mut parts: Vec<String> = Vec::new();
        let (method, args) = loop {
            self.ws();
            match self.peek() {
                Some('.') => {
                    self.i += 1;
                    let Some(name) = self.ident() else {
                        return self.err("se esperaba un nombre después de '.'");
                    };
                    self.ws();
                    if self.peek() == Some('(') {
                        break (name, self.args()?);
                    }
                    parts.push(name);
                }
                Some('[') => {
                    self.i += 1;
                    self.ws();
                    let Value::String(name) = self.value()? else {
                        return self.err("se esperaba un nombre entre comillas");
                    };
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
                let Some(m) = self.ident() else {
                    return self.err("se esperaba un método de colección");
                };
                let a = self.args()?;
                (name, m, a)
            } else {
                return db_method(&method, args).or_else(|e| self.err(e));
            }
        } else {
            (parts.join("."), method, args)
        };
        let mut stmt = coll_method(&coll, &method, args).or_else(|e| self.err(e))?;
        // Chained modifiers: .sort(…).limit(…)… The call ends at its last
        // character, without the blank space or comments after it.
        loop {
            let end = self.i;
            self.ws();
            if self.peek() != Some('.') {
                self.i = end;
                break;
            }
            self.i += 1;
            let Some(m) = self.ident() else {
                return self.err("se esperaba un método después de '.'");
            };
            let end = self.i;
            let a = if self.eat_peek('(') {
                self.args()?
            } else {
                self.i = end;
                Vec::new()
            };
            modifier(&mut stmt, &m, a).or_else(|e| self.err(e))?;
        }
        Ok(stmt)
    }

    /// `use <db>` / `show <what>`: the shell helper and the rest of its
    /// line, or `None` (nothing consumed) when the line isn't one.
    fn helper(&mut self) -> Option<R<Item>> {
        let save = self.i;
        let word = self.ident();
        let Some(word @ ("use" | "show")) = word.as_deref() else {
            self.i = save;
            return None;
        };
        let mut end = self.i;
        while end < self.s.len() && self.s[end] != '\n' {
            end += 1;
        }
        let rest: String = self.s[self.i..end].iter().collect();
        let arg = rest.trim().trim_end_matches(';').trim();
        if self.s.get(self.i).is_some_and(|c| !c.is_whitespace()) || arg.is_empty() {
            self.i = save;
            return None;
        }
        let item = match word {
            "use" if arg.contains(char::is_whitespace) => {
                return Some(self.err(format!("`use` lleva solo el nombre de la base, no `{arg}`")))
            }
            "use" => Item::Use(arg.trim_matches(|c| c == '"' || c == '\'').to_string()),
            _ => Item::Show(arg.to_ascii_lowercase()),
        };
        self.i = end;
        Some(Ok(item))
    }

    fn eat_peek(&mut self, c: char) -> bool {
        self.ws();
        self.peek() == Some(c)
    }

    fn value(&mut self) -> R<Value> {
        self.ws();
        let Some(c) = self.peek() else {
            return self.err("se esperaba un valor y terminó el texto");
        };
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
                let Some(mut w) = self.ident() else {
                    return self.err(format!("valor inesperado '{c}'"));
                };
                if w == "new" {
                    let Some(n) = self.ident() else {
                        return self.err("se esperaba un constructor después de `new`");
                    };
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
                        Ok(if w == "MinKey" {
                            json!({ "$minKey": 1 })
                        } else {
                            json!({ "$maxKey": 1 })
                        })
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
            let Some(c) = self.peek() else {
                return self.err("texto sin cerrar");
            };
            self.i += 1;
            match c {
                c if c == q => return Ok(out),
                '\\' => {
                    let Some(e) = self.peek() else {
                        return self.err("texto sin cerrar");
                    };
                    self.i += 1;
                    match e {
                        'n' => out.push('\n'),
                        't' => out.push('\t'),
                        'r' => out.push('\r'),
                        'b' => out.push('\u{8}'),
                        'f' => out.push('\u{c}'),
                        '0' => out.push('\0'),
                        'u' => {
                            let hex: String = self
                                .s
                                .get(self.i..self.i + 4)
                                .map(|h| h.iter().collect())
                                .unwrap_or_default();
                            let Some(ch) =
                                u32::from_str_radix(&hex, 16).ok().and_then(char::from_u32)
                            else {
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
            let Some(c) = self.peek() else {
                return self.err("expresión regular sin cerrar");
            };
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
                Some(Value::String(s)) => {
                    parse_date(s).ok_or_else(|| format!("fecha inválida: \"{s}\""))?
                }
                Some(other) => return Err(format!("fecha inválida: {other}")),
            };
            json!({ "$date": { "$numberLong": ms.to_string() } })
        }
        "NumberLong" | "Long" => {
            let s = arg_str(&args, 0).unwrap_or_else(|| "0".into());
            s.parse::<i64>()
                .map_err(|_| format!("NumberLong inválido: {s}"))?;
            json!({ "$numberLong": s })
        }
        "NumberInt" | "Int32" => {
            let s = arg_str(&args, 0).unwrap_or_else(|| "0".into());
            let n: i32 = s.parse().map_err(|_| format!("NumberInt inválido: {s}"))?;
            json!({ "$numberInt": n.to_string() })
        }
        "NumberDecimal" | "Decimal128" => {
            json!({ "$numberDecimal": arg_str(&args, 0).unwrap_or_else(|| "0".into()) })
        }
        "Double" => json!(args.first().and_then(Value::as_f64).unwrap_or(0.0)),
        "Timestamp" => {
            let t = args.first().and_then(Value::as_u64).unwrap_or(0);
            let i = args.get(1).and_then(Value::as_u64).unwrap_or(0);
            json!({ "$timestamp": { "t": t, "i": i } })
        }
        "UUID" => {
            let hex: String = arg_str(&args, 0)
                .unwrap_or_default()
                .chars()
                .filter(|c| *c != '-')
                .collect();
            let bytes = decode_hex(&hex)
                .filter(|b| b.len() == 16)
                .ok_or_else(|| format!("UUID inválido: {hex}"))?;
            json!({ "$binary": { "base64": b64(&bytes), "subType": "04" } })
        }
        "BinData" => {
            let sub = args.first().and_then(Value::as_u64).unwrap_or(0);
            let data = arg_str(&args, 1).unwrap_or_default();
            json!({ "$binary": { "base64": data, "subType": format!("{sub:02x}") } })
        }
        "RegExp" => regex_value(
            &arg_str(&args, 0).unwrap_or_default(),
            &arg_str(&args, 1).unwrap_or_default(),
        ),
        other => return Err(format!("constructor desconocido `{other}(…)`")),
    })
}

/// Milliseconds since the epoch; dates without a zone are UTC.
pub fn parse_date(s: &str) -> Option<i64> {
    let s = s.trim();
    if let Ok(d) = DateTime::parse_from_rfc3339(s) {
        return Some(d.timestamp_millis());
    }
    for f in [
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M",
    ] {
        if let Ok(d) = NaiveDateTime::parse_from_str(s.trim_end_matches('Z'), f) {
            return Some(d.and_utc().timestamp_millis());
        }
    }
    NaiveDate::parse_from_str(s, "%Y-%m-%d").ok().map(|d| {
        d.and_hms_opt(0, 0, 0)
            .expect("midnight")
            .and_utc()
            .timestamp_millis()
    })
}

fn decode_hex(h: &str) -> Option<Vec<u8>> {
    if !h.len().is_multiple_of(2) {
        return None;
    }
    (0..h.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(h.get(i..i + 2)?, 16).ok())
        .collect()
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
    let stmt = |cmd, shape| Stmt {
        cmd,
        shape,
        admin: false,
    };
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
                stage @ Value::Object(_) => {
                    (std::iter::once(stage).chain(a.by_ref()).collect(), None)
                }
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
            let shape = if cmd.get_bool("explain").unwrap_or(false) {
                Shape::Reply
            } else {
                Shape::Cursor
            };
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
            pipeline.push(Bson::Document(
                doc! { "$group": { "_id": 1, "count": { "$sum": 1 } } },
            ));
            stmt(
                doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} },
                Shape::CountAgg,
            )
        }
        "estimatedDocumentCount" => stmt(doc! { "count": coll }, Shape::Count),
        "count" => stmt(
            doc! { "count": coll, "query": filter_arg(&mut a)? },
            Shape::Count,
        ),
        "distinct" => {
            let Some(Value::String(key)) = a.next() else {
                return Err("distinct necesita el nombre del campo".into());
            };
            let cmd = doc! { "distinct": coll, "key": key.as_str(), "query": filter_arg(&mut a)? };
            stmt(cmd, Shape::Distinct(key))
        }
        "getIndexes" => stmt(doc! { "listIndexes": coll }, Shape::Cursor),
        "stats" => stmt(doc! { "collStats": coll }, Shape::Reply),
        "insertOne" | "insert" | "insertMany" => {
            let docs: Vec<Bson> = match a.next() {
                Some(Value::Array(v)) => v
                    .into_iter()
                    .map(|d| to_doc(d).map(Bson::Document))
                    .collect::<R<_>>()?,
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
                Some(Value::Array(p)) if method != "replaceOne" => {
                    Bson::Array(p.into_iter().map(to_bson).collect::<R<_>>()?)
                }
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
            stmt(
                doc! { "delete": coll, "deletes": [{ "q": q, "limit": limit }] },
                Shape::Write,
            )
        }
        "drop" => stmt(doc! { "drop": coll }, Shape::Reply),
        // db.c.renameCollection(new[, dropTarget]), in the same database.
        "renameCollection" => {
            let to = match a.next() {
                Some(Value::String(s)) if !s.is_empty() => s,
                _ => return Err("renameCollection necesita el nombre nuevo como texto".into()),
            };
            let drop_target = matches!(a.next(), Some(Value::Bool(true)));
            Stmt {
                cmd: doc! { "renameCollection": coll, "to": to, "dropTarget": drop_target },
                shape: Shape::Rename,
                admin: true,
            }
        }
        // db.c.createIndex(keys[, options]) / createIndexes([keys…][, options])
        "createIndex" | "ensureIndex" => {
            let keys = a
                .next()
                .ok_or_else(|| format!("{method} necesita las claves del índice"))?;
            let opts = opt_doc(&mut a)?;
            stmt(
                doc! { "createIndexes": coll, "indexes": [index_spec(keys, opts.as_ref())?] },
                Shape::Reply,
            )
        }
        "createIndexes" => {
            let Some(Value::Array(all)) = a.next() else {
                return Err("createIndexes espera un array de claves".into());
            };
            let opts = opt_doc(&mut a)?;
            let specs: Vec<Bson> = all
                .into_iter()
                .map(|k| index_spec(k, opts.as_ref()).map(Bson::Document))
                .collect::<R<_>>()?;
            stmt(
                doc! { "createIndexes": coll, "indexes": specs },
                Shape::Reply,
            )
        }
        "dropIndex" | "dropIndexes" => {
            let index = match a.next() {
                None if method == "dropIndexes" => Bson::String("*".into()),
                None => return Err("dropIndex necesita el nombre o las claves del índice".into()),
                Some(v) => to_bson(v)?,
            };
            stmt(doc! { "dropIndexes": coll, "index": index }, Shape::Reply)
        }
        // db.c.hideIndex(name | keys) / unhideIndex(…): `collMod` (MongoDB 4.4+).
        "hideIndex" | "unhideIndex" => {
            let mut index = match a.next() {
                Some(Value::String(s)) => doc! { "name": s },
                Some(v @ Value::Object(_)) => doc! { "keyPattern": to_doc(v)? },
                _ => {
                    return Err(format!(
                        "{method} necesita el nombre o las claves del índice"
                    ))
                }
            };
            index.insert("hidden", method == "hideIndex");
            stmt(doc! { "collMod": coll, "index": index }, Shape::Reply)
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
            let n = first
                .as_ref()
                .and_then(Value::as_i64)
                .ok_or_else(|| format!(".{name}() necesita un número"))?;
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
    fn hide_and_unhide_index() {
        let h = one(r#"db.getCollection("pedidos").hideIndex("ix_a")"#);
        assert_eq!(
            h.cmd,
            doc! { "collMod": "pedidos", "index": { "name": "ix_a", "hidden": true } }
        );
        assert_eq!(write_reason(&h.cmd).as_deref(), Some("collMod"));
        let u = one("db.pedidos.unhideIndex({ a: 1, b: -1 })");
        assert_eq!(
            u.cmd,
            doc! { "collMod": "pedidos", "index": { "keyPattern": { "a": 1, "b": -1 }, "hidden": false } }
        );
        assert!(parse_script("db.pedidos.hideIndex()").is_err());
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
        let v = parse_script(
            "db.a.find()\ndb.b.countDocuments({})  ; db.getCollectionNames()\n{ ping: 1 }",
        )
        .unwrap();
        assert_eq!(v.len(), 4);
        assert_eq!(v[1].shape, Shape::CountAgg);
        assert_eq!(v[2].cmd.get_bool("nameOnly"), Ok(true));
        assert_eq!(v[3].cmd, doc! { "ping": 1 });
        assert_eq!(v[3].shape, Shape::Reply);
    }

    #[test]
    fn shell_constructors_become_bson() {
        let s = one(
            r#"db.c.find({_id: ObjectId("65a1b2c3d4e5f60718293a4b"), at: {$gte: ISODate("2024-01-31")}, n: NumberLong("9007199254740993"), r: /ab\/c/i, trailing: [1,2,],})"#,
        );
        let f = s.cmd.get_document("filter").unwrap();
        assert_eq!(
            f.get_object_id("_id").unwrap().to_hex(),
            "65a1b2c3d4e5f60718293a4b"
        );
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
        assert_eq!(
            one("db.system.views.find()").cmd.get_str("find"),
            Ok("system.views")
        );
        assert_eq!(
            one("db['my-coll'].find()").cmd.get_str("find"),
            Ok("my-coll")
        );
        assert_eq!(
            one("db.getCollection(\"a b\").find({}).limit(1)")
                .cmd
                .get_str("find"),
            Ok("a b")
        );
    }

    #[test]
    fn writes_and_distinct() {
        let s = one("db.c.updateMany({a: 1}, {$set: {b: 2}}, {upsert: true})");
        assert_eq!(s.shape, Shape::Write);
        let u = s.cmd.get_array("updates").unwrap()[0]
            .as_document()
            .unwrap()
            .clone();
        assert_eq!(u.get_bool("multi"), Ok(true));
        assert_eq!(u.get_bool("upsert"), Ok(true));
        let d = one("db.c.distinct('city', {active: true})");
        assert_eq!(d.shape, Shape::Distinct("city".into()));
        let del = one("db.c.deleteOne({x: 1})");
        assert_eq!(
            del.cmd.get_array("deletes").unwrap()[0]
                .as_document()
                .unwrap()
                .get_i32("limit"),
            Ok(1)
        );
    }

    #[test]
    fn read_only_whitelist() {
        assert_eq!(write_reason(&one("db.c.find({})").cmd), None);
        assert_eq!(
            write_reason(&one("db.c.aggregate([{$match: {}}])").cmd),
            None
        );
        assert_eq!(
            write_reason(&one("db.c.aggregate([{$match: {}}, {$out: 'x'}])").cmd).as_deref(),
            Some("$out")
        );
        assert_eq!(
            write_reason(&one("db.c.insertOne({a: 1})").cmd).as_deref(),
            Some("insert")
        );
        assert_eq!(
            write_reason(&one("db.runCommand({dropDatabase: 1})").cmd).as_deref(),
            Some("dropDatabase")
        );
        assert_eq!(write_reason(&one("db.c.find().explain()").cmd), None);
        assert_eq!(write_reason(&one("db.c.find().count()").cmd), None);
    }

    #[test]
    fn units_with_use_and_show() {
        let u = parse_units(
            "use ventas\nshow collections;\ndb.c.find({ a: 1 })\n  .limit(2); { ping: 1 }\nuse = 3",
        )
        .unwrap_err();
        // `use = 3` isn't the helper: it's an unparsable call.
        assert_eq!(u.line, 5);
        let u = parse_units(
            "use ventas\nshow collections;\ndb.c.find({ a: 'é' })\n  .limit(2); { ping: 1 }",
        )
        .unwrap();
        let items: Vec<(&Item, u32, usize)> =
            u.iter().map(|u| (&u.item, u.line, u.start)).collect();
        assert_eq!(items[0], (&Item::Use("ventas".into()), 1, 0));
        assert_eq!(items[1], (&Item::Show("collections".into()), 2, 11));
        assert!(matches!(items[2], (Item::Run(_), 3, 29)));
        assert!(matches!(items[3], (Item::Run(_), 4, 65)));
        assert_eq!(u.len(), 4);
        // A collection called `use` is still a call.
        assert!(matches!(
            parse_units("db.use.find()").unwrap()[0].item,
            Item::Run(_)
        ));
        let e = parse_units("db.c.find({})\n  db.c.find({a: })").unwrap_err();
        assert_eq!((e.line, e.offset), (2, 30));
        assert!(
            e.message.starts_with("se esperaba un valor") || !e.message.starts_with("línea"),
            "{e:?}"
        );
        assert!(parse_units("use a b").is_err());
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

    /// `(text, start, end, line)` of each statement the editor gets.
    fn cut(text: &str) -> Vec<(&str, usize, usize, u32)> {
        statements(text)
            .iter()
            .map(|s| {
                assert_eq!(s.text, text[s.start..s.end]);
                assert_eq!((s.kind, s.repeat, &s.error), (StatementKind::Sql, 1, &None));
                (&text[s.start..s.end], s.start, s.end, s.line)
            })
            .collect()
    }

    /// The editor's statements are `execute`'s units: the same places, in
    /// the same order (its results are numbered by them), and each one runs
    /// alone as it runs in the whole script.
    fn same_as_execute(text: &str) {
        let units = parse_units(text).unwrap();
        let st = statements(text);
        assert_eq!(st.len(), units.len(), "{text:?}");
        for (s, u) in st.iter().zip(&units) {
            assert_eq!(
                (s.start, s.end, s.line),
                (u.start, u.end, u.line),
                "{text:?}"
            );
            let alone = parse_units(&s.text).unwrap();
            assert_eq!(alone.len(), 1, "{:?}", s.text);
            assert_eq!(alone[0].item, u.item, "{:?}", s.text);
        }
    }

    #[test]
    fn one_statement_per_unit_without_semicolons() {
        let t = "db.a.find({})\ndb.b.find({})";
        assert_eq!(
            cut(t),
            [("db.a.find({})", 0, 13, 1), ("db.b.find({})", 14, 27, 2)]
        );
        same_as_execute(t);
        let t = "use test\nshow collections";
        assert_eq!(
            cut(t),
            [("use test", 0, 8, 1), ("show collections", 9, 25, 2)]
        );
        same_as_execute(t);
        // A pipeline over several lines stays whole.
        let t = "db.a.aggregate([\n  { $match: { x: 1 } },\n  { $group: { _id: \"$k\", n: { $sum: 1 } } }\n])\ndb.b.countDocuments({})";
        let c = cut(t);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c[0].0.starts_with("db.a.aggregate([") && c[0].0.ends_with("])"));
        assert_eq!(c[0].3, 1);
        assert_eq!(c[1], ("db.b.countDocuments({})", t.len() - 23, t.len(), 5));
        same_as_execute(t);
    }

    #[test]
    fn three_statements_with_and_without_semicolons() {
        let t = "use ventas;\nshow collections ;  \n{ ping: 1 }; db.c.find({ a: 1 })\n  .limit(2);";
        assert_eq!(
            cut(t),
            [
                ("use ventas", 0, 10, 1),
                ("show collections", 12, 28, 2),
                ("{ ping: 1 }", 33, 44, 3),
                ("db.c.find({ a: 1 })\n  .limit(2)", 46, 77, 3),
            ]
        );
        same_as_execute(t);
        let t = "db.a.find()\n\n\ndb.b.find()\n;;\ndb.c.find()";
        let lines: Vec<u32> = cut(t).iter().map(|c| c.3).collect();
        assert_eq!(lines, [1, 4, 6]);
        same_as_execute(t);
    }

    #[test]
    fn strings_regexes_and_comments_hold_separators() {
        let t = "db.a.find({ s: \"x;y\\\" })\\ndb.b\", t: '// no', u: '/* no */' })\ndb.b.find({ r: /[/;)]+\\/x/i, k: 'a)]}' })";
        let c = cut(t);
        assert_eq!(c.len(), 2, "{c:?}");
        assert!(c[1].0.starts_with("db.b.find({ r: /"), "{c:?}");
        same_as_execute(t);
        // Comments before a statement aren't part of it (the lens sits on
        // its first code line); comments after it either.
        let t = "// clientes: activos\n/* varias\n   líneas */\ndb.clientes.find({ /* dentro */ a: 1 }) // al final\n\n// otra\n  .limit(5) // límite\n// siguiente\ndb.b.find()";
        let c = cut(t);
        assert_eq!(c.len(), 2, "{c:?}");
        assert_eq!(c[0].3, 4);
        assert!(c[0].0.starts_with("db.clientes.find(") && c[0].0.ends_with(".limit(5)"));
        assert_eq!(c[1].0, "db.b.find()");
        assert_eq!(c[1].3, 9);
        same_as_execute(t);
    }

    #[test]
    fn crlf_and_byte_offsets() {
        let t = "use test\r\nshow collections\r\ndb.a.find({ n: 'ñandú' })\r\n  .sort({ n: 1 })\r\ndb.b.find()\r\n";
        assert_eq!(
            cut(t),
            [
                ("use test", 0, 8, 1),
                ("show collections", 10, 26, 2),
                ("db.a.find({ n: 'ñandú' })\r\n  .sort({ n: 1 })", 28, 74, 3),
                ("db.b.find()", 76, 87, 5),
            ]
        );
        same_as_execute(t);
    }

    #[test]
    fn a_unit_that_doesnt_parse_keeps_the_others_in_place() {
        // A bad value: execute runs nothing; the editor still sees three.
        let t = "db.a.find({})\ndb.b.find({ a: })\ndb.c.find()";
        assert!(parse_units(t).is_err());
        assert_eq!(
            cut(t),
            [
                ("db.a.find({})", 0, 13, 1),
                ("db.b.find({ a: })", 14, 31, 2),
                ("db.c.find()", 32, 43, 3),
            ]
        );
        // Brackets left open (being written): the next `db.` line is
        // another statement.
        let t = "db.a.aggregate([\n  { $match: { a: 1 }\ndb.c.find()";
        let c = cut(t);
        assert_eq!(c.len(), 2, "{c:?}");
        assert_eq!(c[1], ("db.c.find()", 38, 49, 3));
        // A quote left open ends at its line.
        let t = "db.a.find({ a: 'x })\ndb.b.find()";
        assert_eq!(cut(t)[1], ("db.b.find()", 21, 32, 2));
        // A chain being written, its `;`, an unknown method.
        let t = "db.a.find().\ndb.b.find();db.c.frob({ x: ';' }); { ping: 1 }\nselect 1";
        assert_eq!(
            cut(t).iter().map(|c| c.0).collect::<Vec<_>>(),
            [
                "db.a.find().",
                "db.b.find()",
                "db.c.frob({ x: ';' })",
                "{ ping: 1 }",
                "select 1"
            ]
        );
        assert!(statements("").is_empty() && statements(" ;\n// nada\n").is_empty());
    }

    /// As the text is typed, every prefix cuts into ordered statements that
    /// start and end on code, and the complete ones agree with `execute`.
    #[test]
    fn every_prefix_while_typing() {
        let t = "// nombre: año\nuse ventas\r\ndb.a.find({ s: 'ñ;\"' /* c */, r: /[/]x/i })\n  .sort({ a: -1 }); db['b c'].aggregate([\n  { $match: {} }\n])\n{ ping: 1 }\nshow dbs;";
        same_as_execute(t);
        for (cut_at, _) in t.char_indices().chain(std::iter::once((t.len(), ' '))) {
            let text = &t[..cut_at];
            let parses = parse_units(text).is_ok();
            if parses {
                same_as_execute(text);
            }
            let mut prev = 0;
            for s in statements(text) {
                assert!(
                    prev <= s.start && s.start < s.end && s.end <= text.len(),
                    "{text:?}"
                );
                assert!(text.is_char_boundary(s.start) && text.is_char_boundary(s.end));
                assert_eq!(s.text, s.text.trim_start(), "{text:?}");
                assert!(
                    !s.text.starts_with("//") && !s.text.starts_with("/*"),
                    "{text:?}"
                );
                // (Blank space or a `;` may end one inside a quote left open.)
                if parses {
                    assert!(
                        s.text == s.text.trim() && !s.text.ends_with(';'),
                        "{text:?}"
                    );
                }
                prev = s.end;
            }
        }
    }
}
