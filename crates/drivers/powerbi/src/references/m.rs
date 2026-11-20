//! What a Power Query (M) expression reads: a partition's or a shared
//! expression's sources outside the model ([`sources`]) and the model's
//! shared expressions, parameters and tables it names ([`names`]).
//!
//! M is read, not run. The source functions Power BI writes are known
//! (`Sql.Database`, `Databricks.Catalogs`, `Excel.Workbook(File.Contents(…))`,
//! `Web.Contents`…; any other `X.Database(server, database)` is read the
//! same way) and the steps after them are followed: a navigation
//! `Source{[Schema="dbo",Item="medidas"]}[Data]`, `{[Name="…",Kind="…"]}`,
//! a native query (`Value.NativeQuery(Source, "select …")`, the `Query`
//! option, `Odbc.Query`), and a step or a shared expression that holds a
//! source (`Source = DW`). A server or a database written as a parameter
//! (`Sql.Database(Servidor, "dw")`) takes the parameter's value. What
//! reads the source last is reported: a navigated source is reported once,
//! at the table it reaches.

use dbine_driver::dependencies::used_names;
use dbine_driver::dependencies::ExternalRef;
use dbine_driver::sql::ScriptDialect;
use std::collections::{BTreeMap, BTreeSet};

/// The model's shared M expressions and parameters, by name.
pub struct Env<'a> {
    pub shared: &'a [(String, String)],
}

impl Env<'_> {
    fn get(&self, name: &str) -> Option<&str> {
        self.shared
            .iter()
            .find(|(n, _)| n == name)
            .map(|(_, m)| m.as_str())
    }
}

/// A source the expression reads, and where it's written (byte offsets in
/// the expression).
#[derive(Debug, Clone, PartialEq)]
pub struct Source {
    pub external: ExternalRef,
    pub at: Vec<usize>,
}

/// The sources outside the model that `m` reads.
pub fn sources(m: &str, env: &Env<'_>) -> Vec<Source> {
    let parsed = parse(m);
    let mut ev = Eval::new(&parsed, env, 0);
    // Each candidate, and the step it is the whole value of.
    let mut found: Vec<(Src, Option<usize>)> = Vec::new();
    let mut consumed: BTreeSet<String> = BTreeSet::new();
    for i in 0..parsed.toks.len() {
        if ev.skip.contains(&i) {
            continue;
        }
        let Some(a) = ev.atom(i) else { continue };
        if !a.produced {
            continue;
        }
        if let Some(b) = &a.base {
            consumed.insert(b.clone());
        }
        let whole = parsed
            .binds
            .iter()
            .position(|b| b.start == i && b.end == a.end);
        found.push((a.src, whole));
    }
    let mut out: Vec<Source> = Vec::new();
    for (src, whole) in found {
        if whole.is_some_and(|k| consumed.contains(&parsed.binds[k].name)) {
            continue;
        }
        let mut at = src.at.clone();
        let external = src.external();
        match out.iter_mut().find(|s| s.external == external) {
            Some(s) => s.at.append(&mut at),
            None => out.push(Source { external, at }),
        }
    }
    for s in &mut out {
        s.at.sort_unstable();
        s.at.dedup();
    }
    out
}

/// The names in `m` that are one of `wanted` (shared expressions,
/// parameters, tables): an identifier, plain or `#"…"`, that isn't one of
/// the expression's own steps, a record's field (`[Name=…]`) or a field
/// access (`[Data]`). Each with the offsets where it's written.
pub fn names(m: &str, wanted: &[String]) -> Vec<(String, Vec<usize>)> {
    let p = parse(m);
    let own: BTreeSet<&str> = p.binds.iter().map(|b| b.name.as_str()).collect();
    let mut out: Vec<(String, Vec<usize>)> = Vec::new();
    for (i, t) in p.toks.iter().enumerate() {
        if t.kind != Kind::Ident || (!t.quoted && KEYWORDS.contains(&t.text.as_str())) {
            continue;
        }
        if own.contains(t.text.as_str()) || p.is(i + 1, "=") {
            continue;
        }
        if i > 0 && p.is(i - 1, "[") && p.is(i + 1, "]") {
            continue;
        }
        let Some(w) = wanted.iter().find(|w| **w == t.text) else {
            continue;
        };
        match out.iter_mut().find(|(n, _)| n == w) {
            Some((_, at)) => at.push(t.start),
            None => out.push((w.clone(), vec![t.start])),
        }
    }
    out
}

/// What a query partition's SQL reads (a legacy data source's query, no
/// M): its tables, on a server the partition doesn't name.
pub fn native_source(sql: &str) -> ExternalRef {
    Src {
        native: Some(sql.to_string()),
        ..Default::default()
    }
    .external()
}

/// The tables a native query reads (`FROM`, `JOIN`…): qualifiers nearest
/// first, then the name.
pub fn native_tables(sql: &str) -> Vec<(Vec<String>, String)> {
    let dialect = ScriptDialect {
        dquote_idents: true,
        bracket_idents: true,
        ..ScriptDialect::generic()
    };
    let mut out: Vec<(Vec<String>, String)> = Vec::new();
    for u in used_names(sql, &dialect) {
        if u.call {
            continue;
        }
        let same = |(q, n): &(Vec<String>, String)| {
            n.eq_ignore_ascii_case(&u.name)
                && q.len() == u.quals.len()
                && q.iter()
                    .zip(&u.quals)
                    .all(|(a, b)| a.eq_ignore_ascii_case(b))
        };
        if !out.iter().any(same) {
            out.push((u.quals, u.name));
        }
    }
    out
}

/// M's keywords: never a name.
const KEYWORDS: &[&str] = &[
    "let",
    "in",
    "each",
    "if",
    "then",
    "else",
    "true",
    "false",
    "null",
    "and",
    "or",
    "not",
    "type",
    "meta",
    "as",
    "is",
    "try",
    "otherwise",
    "error",
    "section",
    "shared",
    "optional",
    "nullable",
];

// -- tokens -------------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// A name, dots inside (`Sql.Database`), or `#"…"` (`quoted`).
    Ident,
    /// `"…"`, its value unescaped.
    Str,
    Num,
    Punct,
}

#[derive(Debug, Clone)]
struct Tok {
    kind: Kind,
    /// A name or a string's value; punctuation as written.
    text: String,
    start: usize,
    quoted: bool,
}

fn tokenize(m: &str) -> Vec<Tok> {
    let mut out = Vec::new();
    let mut i = 0;
    while let Some(c) = m[i..].chars().next() {
        let start = i;
        let rest = &m[i..];
        if c.is_whitespace() || c == '\u{feff}' {
            i += c.len_utf8();
        } else if rest.starts_with("//") {
            i = rest.find('\n').map_or(m.len(), |n| i + n);
        } else if let Some(body) = rest.strip_prefix("/*") {
            i = body.find("*/").map_or(m.len(), |n| i + 2 + n + 2);
        } else if c == '"' || rest.starts_with("#\"") {
            let quote = if c == '#' { i + 1 } else { i };
            let (text, end) = string_at(m, quote);
            out.push(Tok {
                kind: if c == '#' { Kind::Ident } else { Kind::Str },
                text,
                start,
                quoted: c == '#',
            });
            i = end;
        } else if c.is_alphabetic()
            || c == '_'
            || (c == '#' && rest[1..].chars().next().is_some_and(char::is_alphabetic))
        {
            let mut j = i + c.len_utf8();
            for ch in m[j..].chars() {
                if ch.is_alphanumeric() || ch == '_' || ch == '.' {
                    j += ch.len_utf8();
                } else {
                    break;
                }
            }
            let text = m[i..j].trim_end_matches('.');
            i += text.len();
            out.push(Tok {
                kind: Kind::Ident,
                text: text.to_string(),
                start,
                quoted: false,
            });
        } else if c.is_ascii_digit() {
            let mut j = i + 1;
            for ch in m[j..].chars() {
                if ch.is_ascii_alphanumeric() || ch == '.' {
                    j += 1;
                } else {
                    break;
                }
            }
            out.push(Tok {
                kind: Kind::Num,
                text: m[i..j].to_string(),
                start,
                quoted: false,
            });
            i = j;
        } else {
            let op = ["=>", "<>", "<=", ">=", "...", "..", "??"]
                .into_iter()
                .find(|op| rest.starts_with(op))
                .unwrap_or(&rest[..c.len_utf8()]);
            out.push(Tok {
                kind: Kind::Punct,
                text: op.to_string(),
                start,
                quoted: false,
            });
            i += op.len();
        }
    }
    out
}

/// The string literal at `q` (its `"`): the value, `""` and `#(lf)`-style
/// escapes undone, and the byte past it. One that never ends runs to the
/// end.
fn string_at(m: &str, q: usize) -> (String, usize) {
    let b = m.as_bytes();
    let mut i = q + 1;
    let mut raw = String::new();
    let mut from = i;
    while i < b.len() {
        if b[i] == b'"' {
            raw.push_str(&m[from..i]);
            if b.get(i + 1) == Some(&b'"') {
                raw.push('"');
                i += 2;
                from = i;
                continue;
            }
            return (unescape(&raw), i + 1);
        }
        i += 1;
    }
    raw.push_str(&m[from..]);
    (unescape(&raw), m.len())
}

/// M's escapes inside a string: `#(lf)`, `#(cr)`, `#(tab)`, `#(cr,lf)`,
/// `#(#)`.
fn unescape(s: &str) -> String {
    if !s.contains("#(") {
        return s.to_string();
    }
    s.replace("#(cr,lf)", "\r\n")
        .replace("#(lf)", "\n")
        .replace("#(cr)", "\r")
        .replace("#(tab)", "\t")
        .replace("#(#)", "#")
}

// -- steps --------------------------------------------------------------------------------

/// A `name = …` of a `let`: its value's tokens are `start..end`.
#[derive(Debug, Clone)]
struct Bind {
    name: String,
    start: usize,
    end: usize,
}

struct Parsed {
    toks: Vec<Tok>,
    /// The bracket that closes each one that opens.
    pair: Vec<Option<usize>>,
    binds: Vec<Bind>,
    /// The value of the whole expression: after the outer `let`'s `in`.
    result: (usize, usize),
}

impl Parsed {
    fn is(&self, i: usize, p: &str) -> bool {
        self.toks
            .get(i)
            .is_some_and(|t| t.kind == Kind::Punct && t.text == p)
    }

    fn word(&self, i: usize, w: &str) -> bool {
        self.toks
            .get(i)
            .is_some_and(|t| t.kind == Kind::Ident && !t.quoted && t.text == w)
    }

    /// Where the value that starts at `s` ends: a `,` or an `in` outside
    /// any bracket or inner `let`, or a bracket that closes one opened
    /// before it.
    fn value_end(&self, s: usize) -> usize {
        let mut depth = 0i32;
        let mut k = s;
        while k < self.toks.len() {
            let t = &self.toks[k];
            match t.kind {
                Kind::Punct if matches!(t.text.as_str(), "(" | "[" | "{") => depth += 1,
                Kind::Punct if matches!(t.text.as_str(), ")" | "]" | "}") => {
                    if depth == 0 {
                        return k;
                    }
                    depth -= 1;
                }
                Kind::Punct if t.text == "," && depth == 0 => return k,
                Kind::Ident if self.word(k, "let") => depth += 1,
                Kind::Ident if self.word(k, "in") => {
                    if depth == 0 {
                        return k;
                    }
                    depth -= 1;
                }
                _ => {}
            }
            k += 1;
        }
        k
    }
}

fn parse(m: &str) -> Parsed {
    let toks = tokenize(m);
    let mut pair = vec![None; toks.len()];
    let mut open: Vec<usize> = Vec::new();
    for (i, t) in toks.iter().enumerate() {
        if t.kind != Kind::Punct {
            continue;
        }
        match t.text.as_str() {
            "(" | "[" | "{" => open.push(i),
            ")" | "]" | "}" => {
                if let Some(o) = open.pop() {
                    pair[o] = Some(i);
                }
            }
            _ => {}
        }
    }
    let mut p = Parsed {
        toks,
        pair,
        binds: Vec::new(),
        result: (0, 0),
    };
    // A `let` frame is open while its `in` hasn't come: its bindings are
    // `name =` right after the `let` or a `,` at its own level.
    let mut frames: Vec<bool> = Vec::new();
    for i in 0..p.toks.len() {
        let t = &p.toks[i];
        match t.kind {
            Kind::Punct if matches!(t.text.as_str(), "(" | "[" | "{") => frames.push(false),
            Kind::Punct if matches!(t.text.as_str(), ")" | "]" | "}") => {
                frames.pop();
            }
            Kind::Ident if p.word(i, "let") => frames.push(true),
            Kind::Ident if p.word(i, "in") => {
                if frames.last() == Some(&true) {
                    frames.pop();
                }
            }
            Kind::Ident
                if frames.last() == Some(&true)
                    && p.is(i + 1, "=")
                    && i > 0
                    && (p.word(i - 1, "let") || p.is(i - 1, ",")) =>
            {
                let end = p.value_end(i + 2);
                let name = t.text.clone();
                p.binds.push(Bind {
                    name,
                    start: i + 2,
                    end,
                });
            }
            _ => {}
        }
    }
    let n = p.toks.len();
    p.result = if p.word(0, "let") {
        let mut depth = 0i32;
        let mut at = n;
        for k in 1..n {
            if p.word(k, "let") {
                depth += 1;
            } else if p.word(k, "in") {
                if depth == 0 {
                    at = k;
                    break;
                }
                depth -= 1;
            }
        }
        ((at + 1).min(n), n)
    } else {
        (0, n)
    };
    p
}

// -- values -------------------------------------------------------------------------------

/// A source being read, as far as the steps tell.
#[derive(Debug, Clone, Default)]
struct Src {
    system: Option<String>,
    server: Option<String>,
    database: Option<String>,
    schema: Option<String>,
    name: Option<String>,
    /// SharePoint folders before the file.
    path: Vec<String>,
    /// What the UI shows besides (an HTTP path, a warehouse, a folder).
    extra: Vec<String>,
    /// `Sql.Databases`: a navigation by name picks a database.
    lists_databases: bool,
    /// `Databricks.Query`: a function of the SQL it runs.
    function: bool,
    native: Option<String>,
    at: Vec<usize>,
}

impl Src {
    fn of(system: &str) -> Src {
        Src {
            system: Some(system.to_string()),
            ..Default::default()
        }
    }

    fn external(&self) -> ExternalRef {
        let mut x = ExternalRef {
            system: self.system.clone(),
            server: self.server.clone(),
            database: self.database.clone(),
            schema: self.schema.clone(),
            name: self.name.clone(),
            ..Default::default()
        };
        let mut detail = self.extra.clone();
        if !self.path.is_empty() {
            detail.push(self.path.join("/"));
        }
        if let Some(sql) = &self.native {
            let tables = native_tables(sql);
            match tables.as_slice() {
                [] => detail.push(one_line(sql)),
                [(quals, name)] => {
                    x.name = Some(name.clone());
                    x.schema = quals.first().cloned().or(x.schema);
                    if let Some(db) = quals.get(1) {
                        x.database = Some(db.clone());
                    }
                }
                many => detail.push(
                    many.iter()
                        .map(|(q, n)| {
                            let mut parts: Vec<&str> = q.iter().rev().map(String::as_str).collect();
                            parts.push(n);
                            parts.join(".")
                        })
                        .collect::<Vec<_>>()
                        .join(", "),
                ),
            }
        }
        if !detail.is_empty() {
            x.detail = Some(detail.join(" · "));
        }
        x
    }
}

/// A SQL text on one line, cut at [`dbine_driver::dependencies::LINE_CHARS`].
fn one_line(sql: &str) -> String {
    sql.split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(dbine_driver::dependencies::LINE_CHARS)
        .collect()
}

/// The value written at a token and where it ends.
struct Atom {
    src: Src,
    end: usize,
    /// The step or shared expression it starts from (`Source{…}`).
    base: Option<String>,
    /// It reads more than its base: a source call, a navigation, a query.
    produced: bool,
}

struct Eval<'a> {
    p: &'a Parsed,
    env: &'a Env<'a>,
    depth: u8,
    memo: BTreeMap<String, Option<Src>>,
    /// Calls read as part of another source (`File.Contents` inside
    /// `Excel.Workbook`): not reported on their own.
    skip: BTreeSet<usize>,
}

/// How deep shared expressions are followed into each other.
const MAX_DEPTH: u8 = 4;

impl<'a> Eval<'a> {
    fn new(p: &'a Parsed, env: &'a Env<'a>, depth: u8) -> Eval<'a> {
        Eval {
            p,
            env,
            depth,
            memo: BTreeMap::new(),
            skip: BTreeSet::new(),
        }
    }

    fn tok(&self, i: usize) -> Option<&'a Tok> {
        self.p.toks.get(i)
    }

    /// The comma-separated parts between the bracket at `open` and its
    /// pair.
    fn parts(&self, open: usize) -> Vec<(usize, usize)> {
        let Some(close) = self.p.pair.get(open).copied().flatten() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        let mut from = open + 1;
        let mut k = open + 1;
        while k < close {
            if let Some(Some(c)) = self.p.pair.get(k) {
                k = c + 1;
                continue;
            }
            if self.p.is(k, ",") {
                out.push((from, k));
                from = k + 1;
            }
            k += 1;
        }
        if from < close {
            out.push((from, close));
        }
        out
    }

    /// A record `[a = …, b = …]` at `open`: each field with its value when
    /// it's text.
    fn record(&mut self, open: usize) -> Vec<(String, Option<String>)> {
        if !self.p.is(open, "[") {
            return Vec::new();
        }
        let mut out = Vec::new();
        for (s, e) in self.parts(open) {
            let Some(t) = self.tok(s) else { continue };
            if t.kind == Kind::Ident && self.p.is(s + 1, "=") {
                out.push((t.text.clone(), self.text(s + 2, e)));
            }
        }
        out
    }

    /// The text `s..e` evaluates to: literals, steps and parameters that
    /// hold one, joined with `&`.
    fn text(&mut self, s: usize, e: usize) -> Option<String> {
        if s >= e {
            return None;
        }
        // `"x" meta [IsParameterQuery=true…]`: the value before `meta`.
        let e = (s..e).find(|&k| self.p.word(k, "meta")).unwrap_or(e);
        let mut out = String::new();
        let mut from = s;
        for k in s..=e {
            if k < e && !self.p.is(k, "&") {
                continue;
            }
            if k != from + 1 {
                return None;
            }
            let t = self.tok(from)?;
            match t.kind {
                Kind::Str => out.push_str(&t.text),
                Kind::Ident => out.push_str(&self.literal(&t.text)?),
                _ => return None,
            }
            from = k + 1;
        }
        Some(out)
    }

    /// The text a step or a parameter holds.
    fn literal(&mut self, name: &str) -> Option<String> {
        if let Some(b) = self.p.binds.iter().find(|b| b.name == name).cloned() {
            if self.depth >= MAX_DEPTH {
                return None;
            }
            self.depth += 1;
            let v = self.text(b.start, b.end);
            self.depth -= 1;
            return v;
        }
        if self.depth >= MAX_DEPTH {
            return None;
        }
        let m = self.env.get(name)?;
        let p = parse(m);
        let mut ev = Eval::new(&p, self.env, self.depth + 1);
        ev.text(p.result.0, p.result.1)
    }

    /// The source a step or a shared expression holds.
    fn resolve(&mut self, name: &str) -> Option<Src> {
        if let Some(v) = self.memo.get(name) {
            return v.clone();
        }
        self.memo.insert(name.to_string(), None);
        let v = if let Some(b) = self.p.binds.iter().find(|b| b.name == name).cloned() {
            self.atom(b.start).filter(|a| a.end == b.end).map(|a| a.src)
        } else if self.depth < MAX_DEPTH {
            self.env.get(name).and_then(|m| {
                let p = parse(m);
                let mut ev = Eval::new(&p, self.env, self.depth + 1);
                let (s, e) = p.result;
                ev.atom(s).filter(|a| a.end == e).map(|a| Src {
                    // Offsets in another expression mean nothing here.
                    at: Vec::new(),
                    ..a.src
                })
            })
        } else {
            None
        };
        self.memo.insert(name.to_string(), v.clone());
        v
    }

    /// The value written at `i`: a source call, `Value.NativeQuery`, or a
    /// step or shared expression holding a source, with the navigation
    /// after it.
    fn atom(&mut self, i: usize) -> Option<Atom> {
        let t = self.tok(i)?;
        if t.kind != Kind::Ident || (!t.quoted && KEYWORDS.contains(&t.text.as_str())) {
            return None;
        }
        let call = self.p.is(i + 1, "(");
        let (mut src, mut j, base, mut produced) =
            if call && !t.quoted && t.text == "Value.NativeQuery" {
                let close = self.p.pair[i + 1]?;
                let parts = self.parts(i + 1);
                let &(s0, e0) = parts.first()?;
                let inner = self.atom(s0).filter(|a| a.end == e0)?;
                self.skip.insert(s0);
                let mut src = inner.src;
                src.native = parts.get(1).and_then(|&(s, e)| self.text(s, e));
                (src, close + 1, inner.base, true)
            } else if call && !t.quoted && source_function(&t.text) {
                let close = self.p.pair[i + 1]?;
                (self.source(&t.text, i + 1)?, close + 1, None, true)
            } else {
                let src = self.resolve(&t.text)?;
                (src, i + 1, Some(t.text.clone()), false)
            };
        src.at.push(t.start);
        // `Databricks.Query(…)("select …")`, or a step holding it, called.
        if src.function && self.p.is(j, "(") {
            if let Some(close) = self.p.pair[j] {
                let parts = self.parts(j);
                if let Some(sql) = parts.first().and_then(|&(s, e)| self.text(s, e)) {
                    src.native = Some(sql);
                    src.function = false;
                    produced = true;
                }
                j = close + 1;
            }
        }
        loop {
            if self.p.is(j, "{") {
                let close = self.p.pair[j]?;
                if self.p.is(j + 1, "[") && self.p.pair[j + 1] == Some(close - 1) {
                    let fields = self.record(j + 1);
                    navigate(&mut src, &fields);
                    if let Some(t) = self.tok(j) {
                        src.at.push(t.start);
                    }
                    produced = true;
                }
                j = close + 1;
            } else if self.p.is(j, "[") && self.p.is(j + 2, "]") {
                // `[Data]`, `[Content]`: the navigation's value.
                j += 3;
            } else {
                break;
            }
            if self.p.is(j, "?") {
                j += 1;
            }
        }
        Some(Atom {
            src,
            end: j,
            base,
            produced,
        })
    }

    /// The source a known function's call at `open` (its `(`) names.
    fn source(&mut self, f: &str, open: usize) -> Option<Src> {
        let parts = self.parts(open);
        let arg = |ev: &mut Self, k: usize| {
            parts
                .get(k)
                .and_then(|&(s, e)| ev.text(s, e))
                .filter(|v| !v.is_empty())
        };
        let option = |ev: &mut Self, k: usize, field: &str| {
            parts.get(k).and_then(|&(s, _)| {
                ev.record(s)
                    .into_iter()
                    .find(|(n, _)| n.eq_ignore_ascii_case(field))
                    .and_then(|(_, v)| v)
            })
        };
        let src = match f {
            "Sql.Database" | "Sql.Databases" => {
                let mut src = Src::of("SQL Server");
                src.server = arg(self, 0);
                if f == "Sql.Database" {
                    src.database = arg(self, 1);
                    src.native = option(self, 2, "Query");
                } else {
                    src.lists_databases = true;
                }
                src
            }
            "Databricks.Catalogs"
            | "Databricks.Contents"
            | "Databricks.Query"
            | "DatabricksMultiCloud.Catalogs"
            | "DatabricksMultiCloud.Query" => {
                let mut src = Src::of("Databricks");
                src.server = arg(self, 0);
                src.extra.extend(arg(self, 1));
                src.database = option(self, 2, "Catalog");
                src.schema = option(self, 2, "Database");
                src.function = f.ends_with(".Query");
                src
            }
            "Snowflake.Databases" => {
                let mut src = Src::of("Snowflake");
                src.server = arg(self, 0);
                src.extra.extend(arg(self, 1));
                src
            }
            "PostgreSQL.Database" | "MySQL.Database" => {
                let mut src = Src::of(if f.starts_with("MySQL") {
                    "MySQL"
                } else {
                    "PostgreSQL"
                });
                src.server = arg(self, 0);
                src.database = arg(self, 1);
                src.native = option(self, 2, "Query");
                src
            }
            "Oracle.Database" => {
                let mut src = Src::of("Oracle");
                src.server = arg(self, 0);
                src.native = option(self, 1, "Query");
                src
            }
            "Odbc.DataSource" | "Odbc.Query" => {
                let mut src = Src::of("ODBC");
                src.server = match parts.first() {
                    Some(&(s, _)) if self.p.is(s, "[") => {
                        let pairs: Vec<String> = self
                            .record(s)
                            .into_iter()
                            .filter_map(|(k, v)| Some(format!("{k}={}", v?)))
                            .collect();
                        Some(connection_string(&pairs.join(";")))
                    }
                    _ => arg(self, 0).map(|c| connection_string(&c)),
                }
                .filter(|c| !c.is_empty());
                if f == "Odbc.Query" {
                    src.native = arg(self, 1);
                }
                src
            }
            "Excel.Workbook" | "Csv.Document" => {
                // Its file: `File.Contents(path)` or `Web.Contents(url)`.
                // Anything else (a SharePoint file's content) is another
                // source's, reported there.
                let &(s, e) = parts.first()?;
                let inner = self.tok(s)?;
                let file = matches!(inner.text.as_str(), "File.Contents" | "Web.Contents")
                    && self.p.is(s + 1, "(")
                    && self.p.pair[s + 1] == Some(e - 1);
                if !file {
                    return None;
                }
                self.skip.insert(s);
                let path = self
                    .parts(s + 1)
                    .first()
                    .and_then(|&(a, b)| self.text(a, b));
                let mut src = Src::of(if f == "Excel.Workbook" {
                    "Excel"
                } else {
                    "CSV"
                });
                src.name = path.as_deref().map(file_name);
                src.server = path;
                src
            }
            "File.Contents" | "Folder.Files" | "Folder.Contents" => {
                let mut src = Src::of(if f == "File.Contents" {
                    "File"
                } else {
                    "Folder"
                });
                src.server = arg(self, 0);
                if f == "File.Contents" {
                    src.name = src.server.as_deref().map(file_name);
                }
                src
            }
            "SharePoint.Files" | "SharePoint.Contents" | "SharePoint.Tables" => {
                let mut src = Src::of("SharePoint");
                src.server = arg(self, 0);
                src
            }
            "Web.Contents" => {
                let mut src = Src::of("Web");
                src.server = arg(self, 0);
                src.name = option(self, 1, "RelativePath");
                src
            }
            other => {
                let (ns, f) = other.rsplit_once('.')?;
                let mut src = Src::of(ns);
                src.server = arg(self, 0);
                match f {
                    "Database" => src.database = arg(self, 1),
                    _ => src.lists_databases = true,
                }
                src
            }
        };
        Some(src)
    }
}

/// The functions read as a source: the known ones, and any other
/// `X.Database`, `X.Databases` or `X.Catalogs` (`AmazonRedshift.Database`,
/// `GoogleBigQuery.Database`…).
fn source_function(f: &str) -> bool {
    const KNOWN: &[&str] = &[
        "Sql.Database",
        "Sql.Databases",
        "Databricks.Catalogs",
        "Databricks.Contents",
        "Databricks.Query",
        "DatabricksMultiCloud.Catalogs",
        "DatabricksMultiCloud.Query",
        "Snowflake.Databases",
        "PostgreSQL.Database",
        "Oracle.Database",
        "MySQL.Database",
        "Odbc.DataSource",
        "Odbc.Query",
        "Excel.Workbook",
        "Csv.Document",
        "File.Contents",
        "Folder.Files",
        "Folder.Contents",
        "SharePoint.Files",
        "SharePoint.Contents",
        "SharePoint.Tables",
        "Web.Contents",
    ];
    KNOWN.contains(&f)
        || f.split_once('.').is_some_and(|(ns, rest)| {
            !ns.is_empty() && matches!(rest, "Database" | "Databases" | "Catalogs")
        })
}

/// A navigation step's record (`[Schema="dbo",Item="medidas"]`,
/// `[Name="ventas",Kind="Schema"]`) applied to the source.
fn navigate(src: &mut Src, fields: &[(String, Option<String>)]) {
    let get = |k: &str| {
        fields
            .iter()
            .find(|(n, _)| n.eq_ignore_ascii_case(k))
            .and_then(|(_, v)| v.clone())
    };
    let kind = get("Kind").map(|k| k.to_ascii_lowercase());
    if let Some(folder) = get("Folder Path") {
        src.extra.push(folder);
    }
    match (get("Schema"), get("Item"), get("Name")) {
        (Some(schema), Some(item), _) => {
            src.schema = Some(schema);
            src.name = Some(item);
        }
        (Some(schema), None, _) => src.schema = Some(schema),
        (None, Some(item), _) => src.name = Some(item),
        (None, None, Some(name)) => match kind.as_deref() {
            Some("database" | "catalog") => src.database = Some(name),
            Some("schema") => src.schema = Some(name),
            Some(_) => src.name = Some(name),
            None if src.lists_databases && src.database.is_none() => src.database = Some(name),
            None if src.system.as_deref() == Some("SharePoint") => {
                if let Some(prev) = src.name.replace(name) {
                    src.path.push(prev);
                }
            }
            None => src.name = Some(name),
        },
        (None, None, None) => {
            // One text field of another name (`[Id="…"]`): the item.
            if let [(_, Some(v))] = fields {
                src.name = Some(v.clone());
            }
        }
    }
}

/// A file's name, from a path or a URL.
fn file_name(path: &str) -> String {
    let p = path.split(['?', '#']).next().unwrap_or(path);
    let p = p.trim_end_matches(['/', '\\']);
    p.rsplit(['/', '\\']).next().unwrap_or(p).to_string()
}

/// An ODBC connection string without its secrets (`PWD`, `Password`,
/// tokens and keys).
fn connection_string(c: &str) -> String {
    c.split(';')
        .map(str::trim)
        .filter(|p| !p.is_empty())
        .filter(|p| {
            let k: String = p
                .split('=')
                .next()
                .unwrap_or("")
                .chars()
                .filter(char::is_ascii_alphanumeric)
                .map(|c| c.to_ascii_lowercase())
                .collect();
            !(k == "pwd"
                || k.contains("pass")
                || k.contains("secret")
                || k.contains("token")
                || k.ends_with("key"))
        })
        .collect::<Vec<_>>()
        .join(";")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn env(shared: &[(&str, &str)]) -> Vec<(String, String)> {
        shared
            .iter()
            .map(|(n, m)| (n.to_string(), m.to_string()))
            .collect()
    }

    fn one(m: &str, shared: &[(String, String)]) -> ExternalRef {
        let s = sources(m, &Env { shared });
        assert_eq!(s.len(), 1, "{s:#?}");
        s.into_iter().next().unwrap().external
    }

    fn x(
        system: &str,
        server: Option<&str>,
        database: Option<&str>,
        schema: Option<&str>,
        name: Option<&str>,
        detail: Option<&str>,
    ) -> ExternalRef {
        ExternalRef {
            system: Some(system.into()),
            server: server.map(Into::into),
            database: database.map(Into::into),
            schema: schema.map(Into::into),
            name: name.map(Into::into),
            detail: detail.map(Into::into),
            mentions: Vec::new(),
        }
    }

    #[test]
    fn sql_server_navigation_reaches_the_table() {
        let m = "let\n    Source = Sql.Database(\"sql.contoso.com\", \"dw\"),\n    t = Source{[Schema=\"indicadores_directorio\",Item=\"medidas\"]}[Data],\n    #\"Filas filtradas\" = Table.SelectRows(t, each [valor] > 0)\nin\n    #\"Filas filtradas\"";
        let s = sources(m, &Env { shared: &[] });
        assert_eq!(s.len(), 1, "{s:#?}");
        assert_eq!(
            s[0].external,
            x(
                "SQL Server",
                Some("sql.contoso.com"),
                Some("dw"),
                Some("indicadores_directorio"),
                Some("medidas"),
                None
            )
        );
        // Written on lines 2 and 3.
        let mut lines: Vec<usize> = s[0]
            .at
            .iter()
            .map(|&o| m[..o].matches('\n').count() + 1)
            .collect();
        lines.dedup();
        assert_eq!(lines, [2, 3]);
        // Inline, and two tables of one source.
        assert_eq!(
            one(
                "Sql.Database(\"s\", \"db\"){[Schema=\"dbo\",Item=\"t\"]}[Data]",
                &[]
            ),
            x(
                "SQL Server",
                Some("s"),
                Some("db"),
                Some("dbo"),
                Some("t"),
                None
            )
        );
        let two = "let S = Sql.Database(\"s\", \"db\"), a = S{[Schema=\"dbo\",Item=\"a\"]}[Data], b = S{[Schema=\"dbo\",Item=\"b\"]}[Data], j = Table.NestedJoin(a, {\"id\"}, b, {\"id\"}, \"b\") in j";
        let names: Vec<Option<String>> = sources(two, &Env { shared: &[] })
            .into_iter()
            .map(|s| s.external.name)
            .collect();
        assert_eq!(names, [Some("a".to_string()), Some("b".to_string())]);
    }

    #[test]
    fn databricks_catalog_schema_and_table() {
        let m = r#"let
    Source = Databricks.Catalogs("adb-1234567890123456.7.azuredatabricks.net", "/sql/1.0/warehouses/abc123", [Catalog=null, Database=null, EnableAutomaticProxyDiscovery=null]),
    hive_metastore_Database = Source{[Name="hive_metastore",Kind="Database"]}[Data],
    indicadores_directorio_Schema = hive_metastore_Database{[Name="indicadores_directorio",Kind="Schema"]}[Data],
    medidas_Table = indicadores_directorio_Schema{[Name="medidas",Kind="Table"]}[Data]
in
    medidas_Table"#;
        assert_eq!(
            one(m, &[]),
            x(
                "Databricks",
                Some("adb-1234567890123456.7.azuredatabricks.net"),
                Some("hive_metastore"),
                Some("indicadores_directorio"),
                Some("medidas"),
                Some("/sql/1.0/warehouses/abc123")
            )
        );
        // A native query: as a function, or through Value.NativeQuery.
        let q = "let Source = Databricks.Query(\"h\", \"/sql/p\"), r = Source(\"SELECT * FROM indicadores_directorio.medidas WHERE anio = 2026\") in r";
        assert_eq!(
            one(q, &[]),
            x(
                "Databricks",
                Some("h"),
                None,
                Some("indicadores_directorio"),
                Some("medidas"),
                Some("/sql/p")
            )
        );
        let q = "let Source = Databricks.Catalogs(\"h\", \"/sql/p\"), r = Value.NativeQuery(Source{[Name=\"main\",Kind=\"Database\"]}[Data], \"select a.x from ventas.a a join ventas.b b on a.id = b.id\", null, [EnableFolding=true]) in r";
        assert_eq!(
            one(q, &[]),
            x(
                "Databricks",
                Some("h"),
                Some("main"),
                None,
                None,
                Some("/sql/p · ventas.a, ventas.b")
            )
        );
    }

    #[test]
    fn native_queries_and_options() {
        // `[Query=…]`, with M's escapes.
        assert_eq!(
            one(
                "Sql.Database(\"s\", \"dw\", [Query=\"select *#(lf)from [indicadores].[medidas]\"])",
                &[]
            ),
            x(
                "SQL Server",
                Some("s"),
                Some("dw"),
                Some("indicadores"),
                Some("medidas"),
                None
            )
        );
        // Nothing to name: the query itself, on one line.
        assert_eq!(
            one(
                "Value.NativeQuery(PostgreSQL.Database(\"pg:5432\", \"app\"), \"select 1\")",
                &[]
            ),
            x(
                "PostgreSQL",
                Some("pg:5432"),
                Some("app"),
                None,
                None,
                Some("select 1")
            )
        );
        assert_eq!(
            one(
                "Odbc.Query(\"dsn=Ventas;uid=ana;PWD=secreto\", \"select * from dbo.pedidos\")",
                &[]
            ),
            x(
                "ODBC",
                Some("dsn=Ventas;uid=ana"),
                None,
                Some("dbo"),
                Some("pedidos"),
                None
            )
        );
        let odbc = "let Source = Odbc.DataSource([dsn=\"Hive\", Password=\"x\"], [HierarchicalNavigation=true]), d = Source{[Name=\"default\",Kind=\"Database\"]}[Data], t = d{[Name=\"ventas\",Kind=\"Table\"]}[Data] in t";
        assert_eq!(
            one(odbc, &[]),
            x(
                "ODBC",
                Some("dsn=Hive"),
                Some("default"),
                None,
                Some("ventas"),
                None
            )
        );
    }

    #[test]
    fn other_engines_and_files() {
        assert_eq!(
            one("let S = Sql.Databases(\"srv\"), d = S{[Name=\"dw\"]}[Data], t = d{[Schema=\"dbo\",Item=\"x\"]}[Data] in t", &[]),
            x("SQL Server", Some("srv"), Some("dw"), Some("dbo"), Some("x"), None)
        );
        assert_eq!(
            one("let S = Snowflake.Databases(\"acme.snowflakecomputing.com\", \"WH\"), d = S{[Name=\"DB\",Kind=\"Database\"]}[Data], s = d{[Name=\"PUBLIC\",Kind=\"Schema\"]}[Data], v = s{[Name=\"V\",Kind=\"View\"]}[Data] in v", &[]),
            x("Snowflake", Some("acme.snowflakecomputing.com"), Some("DB"), Some("PUBLIC"), Some("V"), Some("WH"))
        );
        assert_eq!(
            one("let S = Oracle.Database(\"ora:1521/XE\", [HierarchicalNavigation=true]), h = S{[Schema=\"HR\"]}[Data], e = h{[Name=\"EMP\"]}[Data] in e", &[]),
            x("Oracle", Some("ora:1521/XE"), None, Some("HR"), Some("EMP"), None)
        );
        assert_eq!(
            one("let S = MySQL.Database(\"my:3306\", \"shop\", [ReturnSingleDatabase=true]), t = S{[Schema=\"shop\",Item=\"orders\"]}[Data] in t", &[]),
            x("MySQL", Some("my:3306"), Some("shop"), Some("shop"), Some("orders"), None)
        );
        assert_eq!(
            one("let S = AmazonRedshift.Database(\"rs.amazonaws.com:5439\", \"dev\"), t = S{[Name=\"public\"]}[Data] in t", &[]),
            x("AmazonRedshift", Some("rs.amazonaws.com:5439"), Some("dev"), None, Some("public"), None)
        );
        // A workbook's sheet; the File.Contents inside isn't a source of its own.
        assert_eq!(
            one("let S = Excel.Workbook(File.Contents(\"C:\\datos\\ventas.xlsx\"), null, true), h = S{[Item=\"Hoja1\",Kind=\"Sheet\"]}[Data] in h", &[]),
            x("Excel", Some("C:\\datos\\ventas.xlsx"), None, None, Some("Hoja1"), None)
        );
        assert_eq!(
            one("let S = Csv.Document(File.Contents(\"/data/a.csv\"),[Delimiter=\",\"]), p = Table.PromoteHeaders(S) in p", &[]),
            x("CSV", Some("/data/a.csv"), None, None, Some("a.csv"), None)
        );
        assert_eq!(
            one("let S = SharePoint.Files(\"https://contoso.sharepoint.com/sites/bi\", [ApiVersion = 15]), f = S{[Name=\"metas.xlsx\",#\"Folder Path\"=\"https://contoso.sharepoint.com/sites/bi/Docs/\"]}[Content], w = Excel.Workbook(f) in w", &[]),
            x("SharePoint", Some("https://contoso.sharepoint.com/sites/bi"), None, None, Some("metas.xlsx"), Some("https://contoso.sharepoint.com/sites/bi/Docs/"))
        );
        assert_eq!(
            one("let S = SharePoint.Contents(\"https://c.sharepoint.com/sites/bi\"), d = S{[Name=\"Docs\"]}[Content], f = d{[Name=\"a.csv\"]}[Content] in f", &[]),
            x("SharePoint", Some("https://c.sharepoint.com/sites/bi"), None, None, Some("a.csv"), Some("Docs"))
        );
        assert_eq!(
            one("let Source = Json.Document(Web.Contents(\"https://api.contoso.com/v1/ventas?api_key=***\", [RelativePath=\"oeste\"])) in Source", &[]),
            x("Web", Some("https://api.contoso.com/v1/ventas?api_key=***"), None, None, Some("oeste"), None)
        );
    }

    #[test]
    fn parameters_and_shared_expressions() {
        let shared = env(&[
            ("Servidor", "\"sql.contoso.com\" meta [IsParameterQuery=true, Type=\"Text\", IsParameterQueryRequired=true]"),
            ("DW", "let Source = Sql.Database(Servidor, \"dw\") in Source"),
        ]);
        // The parameter's value is the server.
        assert_eq!(
            one("let S = Sql.Database(Servidor, \"dw\"), t = S{[Schema=\"dbo\",Item=\"x\"]}[Data] in t", &shared),
            x("SQL Server", Some("sql.contoso.com"), Some("dw"), Some("dbo"), Some("x"), None)
        );
        // A step that holds a shared source, navigated.
        let m = "let\n    Source = DW,\n    t = Source{[Schema=\"indicadores_directorio\",Item=\"medidas\"]}[Data]\nin\n    t";
        let s = sources(m, &Env { shared: &shared });
        assert_eq!(s.len(), 1, "{s:#?}");
        assert_eq!(
            s[0].external,
            x(
                "SQL Server",
                Some("sql.contoso.com"),
                Some("dw"),
                Some("indicadores_directorio"),
                Some("medidas"),
                None
            )
        );
        // Offsets in this expression only (lines 2 and 3).
        assert!(s[0].at.iter().all(|&o| o < m.len()));
        // Shared expressions and tables it names; not its own steps nor fields.
        let wanted = vec![
            "DW".to_string(),
            "Servidor".to_string(),
            "Ventas".to_string(),
            "Data".to_string(),
        ];
        let got = names(
            "let Source = DW, t = Source{[Schema=\"dbo\",Item=\"x\"]}[Data], j = Table.Combine({t, #\"Ventas\"}), f = Table.SelectRows(j, each [Data] = Servidor) in f",
            &wanted,
        );
        let got: Vec<&str> = got.iter().map(|(n, _)| n.as_str()).collect();
        assert_eq!(got, ["DW", "Ventas", "Servidor"]);
        // A cycle ends.
        let cyc = env(&[("A", "B"), ("B", "A{[Name=\"x\"]}[Data]")]);
        assert!(sources("A{[Name=\"y\"]}[Data]", &Env { shared: &cyc }).is_empty());
    }

    #[test]
    fn nothing_is_read_from_strings_comments_or_garbage() {
        assert!(sources(
            "// Sql.Database(\"s\", \"d\")\n\"Sql.Database(x)\"",
            &Env { shared: &[] }
        )
        .is_empty());
        for m in [
            "",
            "let",
            "let in",
            "Sql.Database(",
            "{[",
            "let a = (b, c = Sql.Database(\"s\"",
            "#\"",
            "Value.NativeQuery()",
            "Excel.Workbook()",
        ] {
            let _ = sources(m, &Env { shared: &[] });
            let _ = names(m, &["a".into()]);
        }
        assert_eq!(file_name("https://x/y/a.xlsx?raw=1"), "a.xlsx");
        assert_eq!(
            connection_string("Driver={X};Server=h;Password=p;ApiKey=k;"),
            "Driver={X};Server=h"
        );
    }
}
