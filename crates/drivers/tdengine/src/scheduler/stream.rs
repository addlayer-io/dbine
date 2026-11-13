//! A stream's statement, both ways: read from the `sql` that
//! `information_schema.ins_streams` keeps (lowercased by the server outside
//! quotes and backticks) into the designer's values, and written from them.
//!
//! - The old engine (up to 3.3.6): `CREATE STREAM [IF NOT EXISTS] name
//!   [TRIGGER AT_ONCE | WINDOW_CLOSE | MAX_DELAY t | FORCE_WINDOW_CLOSE |
//!   CONTINUOUS_WINDOW_CLOSE [RECALCULATE t]] [WATERMARK t] [IGNORE EXPIRED
//!   0|1] [DELETE_MARK t] [FILL_HISTORY 0|1 [ASYNC]] [IGNORE UPDATE 0|1]
//!   INTO [db.]stb [(cols)] [TAGS (…)] [SUBTABLE(expr)] AS subquery
//!   [NOTIFY(urls) ON (events) [NOTIFY_HISTORY 0|1] [ON_FAILURE DROP|PAUSE]]`
//!   (the notification from 3.3.6).
//! - The new engine (3.3.7 on): `CREATE STREAM [IF NOT EXISTS] db.name
//!   trigger [FROM [db.]table] [PARTITION BY …] [STREAM_OPTIONS(option |
//!   …)] [NOTIFY(urls) [ON (events)] [WHERE cond] [NOTIFY_OPTIONS(…)]]
//!   [INTO [db.]table] [OUTPUT_SUBTABLE(expr)] [(cols)] [TAGS (…)] [AS
//!   subquery]`, the trigger one of `PERIOD`, `[INTERVAL] SLIDING`,
//!   `SESSION`, `STATE_WINDOW`, `EVENT_WINDOW` (with `TRUE_FOR`) and
//!   `COUNT_WINDOW`. The server adds `CONFIG(timezone = …, firstDayOfWeek =
//!   …)` to the options: it is kept as two fields.
//!
//! Names (the stream, its database) are written backquoted as the server
//! lists them; table names keep the quoting the user gave them (a bare one
//! is lowercased by TDengine, a backquoted one isn't). Expressions, column
//! and tag lists and the query are the user's SQL: checked to be closed
//! (quotes, parentheses) and free of `;` and comments, so each stays inside
//! its clause.

use super::{bad, shown, Vals};
use crate::ddl::{lit, q};
use dbine_driver::{FormValues, Result};
use serde_json::{json, Value};
use std::collections::BTreeMap;

// ---------------------------------------------------------------------------
// Tokens
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum T {
    /// A word, a number or a (qualified, possibly backquoted) name.
    Word,
    /// A '…' or "…" literal.
    Str,
    /// `( … )`, whole.
    Group,
    /// Anything else, one character.
    Punct,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Tok {
    pub(crate) t: T,
    pub(crate) a: usize,
    pub(crate) b: usize,
}

fn is_word(c: u8) -> bool {
    c.is_ascii_alphanumeric() || matches!(c, b'_' | b'.' | b'%' | b'$') || c >= 0x80
}

fn skip_quote(s: &[u8], mut i: usize) -> Option<usize> {
    let q = s[i];
    i += 1;
    while i < s.len() {
        match s[i] {
            b'\\' => i += 2,
            c if c == q && s.get(i + 1) == Some(&q) => i += 2,
            c if c == q => return Some(i + 1),
            _ => i += 1,
        }
    }
    None
}

fn skip_backtick(s: &[u8], i: usize) -> Option<usize> {
    s[i + 1..]
        .iter()
        .position(|&c| c == b'`')
        .map(|p| i + 1 + p + 1)
}

fn skip_group(s: &[u8], mut i: usize) -> Option<usize> {
    let mut depth = 0;
    while i < s.len() {
        match s[i] {
            b'\'' | b'"' => {
                i = skip_quote(s, i)?;
                continue;
            }
            b'`' => {
                i = skip_backtick(s, i)?;
                continue;
            }
            b'(' => depth += 1,
            b')' => {
                depth -= 1;
                if depth == 0 {
                    return Some(i + 1);
                }
            }
            b';' | b'#' => return None,
            b'-' if s.get(i + 1) == Some(&b'-') => return None,
            b'/' if s.get(i + 1) == Some(&b'*') => return None,
            _ => {}
        }
        i += 1;
    }
    None
}

/// The statement's top-level tokens; `None` when a quote or parenthesis
/// isn't closed, or there is a comment or a `;` (outside quotes).
pub(crate) fn lex(sql: &str) -> Option<Vec<Tok>> {
    let s = sql.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < s.len() {
        let c = s[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let a = i;
        let t = match c {
            b'\'' | b'"' => {
                i = skip_quote(s, i)?;
                T::Str
            }
            b'(' => {
                i = skip_group(s, i)?;
                T::Group
            }
            b')' | b';' | b'#' => return None,
            b'-' if s.get(i + 1) == Some(&b'-') => return None,
            b'/' if s.get(i + 1) == Some(&b'*') => return None,
            b'`' => {
                while i < s.len() && (is_word(s[i]) || s[i] == b'`') {
                    i = if s[i] == b'`' {
                        skip_backtick(s, i)?
                    } else {
                        i + 1
                    };
                }
                T::Word
            }
            _ if is_word(c) => {
                while i < s.len() && (is_word(s[i]) || s[i] == b'`') {
                    i = if s[i] == b'`' {
                        skip_backtick(s, i)?
                    } else {
                        i + 1
                    };
                }
                T::Word
            }
            _ => {
                i += 1;
                T::Punct
            }
        };
        out.push(Tok { t, a, b: i });
    }
    Some(out)
}

/// `inner` split at `sep` outside quotes, backticks and parentheses.
pub(crate) fn split_top(inner: &str, sep: u8) -> Vec<String> {
    let s = inner.as_bytes();
    let mut out = Vec::new();
    let (mut depth, mut start, mut i) = (0i32, 0, 0);
    while i < s.len() {
        match s[i] {
            b'\'' | b'"' => {
                i = skip_quote(s, i).unwrap_or(s.len());
                continue;
            }
            b'`' => {
                i = skip_backtick(s, i).unwrap_or(s.len());
                continue;
            }
            b'(' => depth += 1,
            b')' => depth -= 1,
            c if c == sep && depth == 0 => {
                out.push(inner[start..i].trim().to_string());
                start = i + 1;
            }
            _ => {}
        }
        i += 1;
    }
    let last = inner[start..].trim();
    if !last.is_empty() || !out.is_empty() {
        out.push(last.to_string());
    }
    out
}

/// A '…' literal's text.
fn unquote(v: &str) -> String {
    let v = v.trim();
    let b = v.as_bytes();
    if b.len() >= 2 && matches!(b[0], b'\'' | b'"') && b[b.len() - 1] == b[0] {
        let q = b[0] as char;
        let inner = &v[1..v.len() - 1];
        let mut out = String::new();
        let mut chars = inner.chars().peekable();
        while let Some(c) = chars.next() {
            if c == '\\' {
                if let Some(n) = chars.next() {
                    out.push(n);
                }
            } else if c == q && chars.peek() == Some(&q) {
                chars.next();
                out.push(q);
            } else {
                out.push(c);
            }
        }
        return out;
    }
    v.to_string()
}

/// `` `db`.`name` `` / `db.name` / `name` as its parts, without backquotes.
pub(crate) fn name_parts(word: &str) -> Vec<String> {
    let s = word.as_bytes();
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut i = 0;
    while i < s.len() {
        match s[i] {
            b'`' => {
                let end = skip_backtick(s, i).unwrap_or(s.len());
                cur.push_str(&word[i + 1..end.saturating_sub(1).max(i + 1)]);
                i = end;
            }
            b'.' => {
                parts.push(std::mem::take(&mut cur));
                i += 1;
            }
            _ => {
                let start = i;
                while i < s.len() && !matches!(s[i], b'`' | b'.') {
                    i += 1;
                }
                cur.push_str(&word[start..i]);
            }
        }
    }
    parts.push(cur);
    parts
}

struct P<'a> {
    sql: &'a str,
    toks: Vec<Tok>,
    i: usize,
}

impl<'a> P<'a> {
    fn new(sql: &'a str) -> Option<P<'a>> {
        Some(P {
            sql,
            toks: lex(sql)?,
            i: 0,
        })
    }

    fn text(&self, t: Tok) -> &'a str {
        &self.sql[t.a..t.b]
    }

    fn kw(&self, w: &str) -> bool {
        self.toks
            .get(self.i)
            .is_some_and(|t| t.t == T::Word && self.text(*t).eq_ignore_ascii_case(w))
    }

    fn eat(&mut self, w: &str) -> bool {
        let yes = self.kw(w);
        if yes {
            self.i += 1;
        }
        yes
    }

    fn expect(&mut self, w: &str) -> Option<()> {
        self.eat(w).then_some(())
    }

    fn word(&mut self) -> Option<&'a str> {
        let t = *self.toks.get(self.i).filter(|t| t.t == T::Word)?;
        self.i += 1;
        Some(self.text(t))
    }

    fn at_group(&self) -> bool {
        self.toks.get(self.i).is_some_and(|t| t.t == T::Group)
    }

    /// A group's inside.
    fn group(&mut self) -> Option<&'a str> {
        let t = *self.toks.get(self.i).filter(|t| t.t == T::Group)?;
        self.i += 1;
        Some(self.sql[t.a + 1..t.b - 1].trim())
    }

    /// The text from here up to the first of `stops` (a word), or the end.
    fn until(&mut self, stops: &[&str]) -> Option<&'a str> {
        let from = self.i;
        while self.i < self.toks.len() && !stops.iter().any(|s| self.kw(s)) {
            self.i += 1;
        }
        (self.i > from).then(|| &self.sql[self.toks[from].a..self.toks[self.i - 1].b])
    }

    fn done(&self) -> bool {
        self.i == self.toks.len()
    }
}

/// The values a form takes, as the parser fills them.
#[derive(Default)]
struct Out {
    values: BTreeMap<String, Value>,
    urls: Vec<String>,
}

impl Out {
    fn set(&mut self, k: &str, v: impl Into<Value>) {
        self.values.insert(k.into(), v.into());
    }

    fn done(self) -> FormValues {
        FormValues {
            values: self.values,
            lists: BTreeMap::from([(
                "notify_urls".to_string(),
                self.urls
                    .into_iter()
                    .map(|u| BTreeMap::from([("url".to_string(), json!(u))]))
                    .collect(),
            )]),
        }
    }
}

fn header(p: &mut P) -> Option<Vec<String>> {
    p.expect("create")?;
    p.expect("stream")?;
    if p.eat("if") {
        p.expect("not")?;
        p.expect("exists")?;
    }
    Some(name_parts(p.word()?))
}

/// The old engine's statement as the form's values.
pub(crate) fn parse_old(sql: &str) -> Option<FormValues> {
    let mut p = P::new(sql.trim().trim_end_matches(';'))?;
    let name = header(&mut p)?;
    let [name] = name.as_slice() else {
        return None;
    };
    let mut o = Out::default();
    o.set("name", name.as_str());
    while !p.kw("into") {
        match p.word()?.to_ascii_lowercase().as_str() {
            "trigger" => {
                let t = p.word()?.to_ascii_lowercase();
                match t.as_str() {
                    "at_once" | "window_close" | "force_window_close" => {}
                    "max_delay" => o.set("max_delay", p.word()?),
                    "continuous_window_close" => {
                        if p.eat("recalculate") {
                            o.set("recalculate", p.word()?);
                        }
                    }
                    _ => return None,
                }
                o.set("trigger", t);
            }
            "watermark" => o.set("watermark", p.word()?),
            "delete_mark" => o.set("delete_mark", p.word()?),
            "ignore" => match p.word()?.to_ascii_lowercase().as_str() {
                "expired" => o.set("ignore_expired", p.word()?),
                "update" => o.set("ignore_update", p.word()?),
                _ => return None,
            },
            "fill_history" => {
                o.set("fill_history", p.word()?);
                o.set("fill_history_async", p.eat("async"));
            }
            _ => return None,
        }
    }
    p.expect("into")?;
    o.set("into", p.word()?);
    if p.at_group() {
        o.set("columns", p.group()?);
    }
    if p.eat("tags") {
        o.set("tags", p.group()?);
    }
    if p.eat("subtable") {
        o.set("subtable", p.group()?);
    }
    p.expect("as")?;
    o.set("query", p.until(&["notify"])?);
    if p.eat("notify") {
        o.urls = split_top(p.group()?, b',')
            .iter()
            .map(|u| unquote(u))
            .collect();
        p.expect("on")?;
        let events: Vec<String> = split_top(p.group()?, b',')
            .iter()
            .map(|e| unquote(e).to_ascii_lowercase())
            .collect();
        o.set("notify_events", events);
        loop {
            if p.eat("notify_history") {
                o.set("notify_history", p.word()?);
            } else if p.eat("on_failure") {
                o.set("on_failure", p.word()?.to_ascii_lowercase());
            } else {
                break;
            }
        }
    }
    p.done().then(|| o.done())
}

/// The arguments of a group, split at top-level commas.
fn args(inner: &str) -> Vec<String> {
    split_top(inner, b',')
}

fn stream_options(o: &mut Out, inner: &str) -> Option<()> {
    let mut extra = Vec::new();
    for item in split_top(inner, b'|') {
        let mut p = P::new(&item)?;
        let name = p.word()?.to_ascii_lowercase();
        let arg = if p.at_group() { p.group() } else { None };
        if !p.done() {
            return None;
        }
        match (name.as_str(), arg) {
            ("config", Some(a)) => {
                for kv in args(a) {
                    let (k, v) = kv.split_once('=')?;
                    match k.trim().to_ascii_lowercase().as_str() {
                        "timezone" => o.set("timezone", unquote(v)),
                        "firstdayofweek" => o.set("first_day_of_week", unquote(v)),
                        _ => return None,
                    }
                }
            }
            ("watermark" | "expired_time" | "max_delay" | "pre_filter", Some(a)) => {
                o.set(&name, a);
            }
            (
                "ignore_disorder"
                | "delete_recalc"
                | "delete_output_table"
                | "calc_notify_only"
                | "low_latency_calc"
                | "force_output"
                | "ignore_nodata_trigger",
                None,
            ) => o.set(&name, true),
            ("fill_history" | "fill_history_first", a) => {
                o.set("fill", name.as_str());
                if let Some(a) = a {
                    o.set("fill_start", unquote(a));
                }
            }
            ("event_type", Some(a)) => {
                let e: Vec<String> = split_top(a, b'|')
                    .iter()
                    .map(|x| x.to_ascii_lowercase())
                    .collect();
                o.set("event_type", e);
            }
            _ => extra.push(item.clone()),
        }
    }
    if !extra.is_empty() {
        o.set("extra_options", extra.join(" | "));
    }
    Some(())
}

/// The new engine's statement as the form's values.
pub(crate) fn parse_new(sql: &str) -> Option<FormValues> {
    let mut p = P::new(sql.trim().trim_end_matches(';'))?;
    let name = header(&mut p)?;
    let mut o = Out::default();
    match name.as_slice() {
        [db, n] => {
            o.set("database", db.as_str());
            o.set("name", n.as_str());
        }
        [n] => o.set("name", n.as_str()),
        _ => return None,
    }
    let trigger = p.word()?.to_ascii_lowercase();
    match trigger.as_str() {
        "period" => {
            let a = args(p.group()?);
            o.set("trigger", "period");
            o.set("period", a.first()?.as_str());
            if let Some(x) = a.get(1) {
                o.set("period_offset", x.as_str());
            }
        }
        "interval" | "sliding" => {
            o.set("trigger", "sliding");
            if trigger == "interval" {
                let a = args(p.group()?);
                o.set("interval", a.first()?.as_str());
                if let Some(x) = a.get(1) {
                    o.set("interval_offset", x.as_str());
                }
                p.expect("sliding")?;
            }
            let a = args(p.group()?);
            o.set("sliding", a.first()?.as_str());
            if let Some(x) = a.get(1) {
                o.set("sliding_offset", x.as_str());
            }
        }
        "session" => {
            let a = args(p.group()?);
            o.set("trigger", "session");
            o.set("session_col", a.first()?.as_str());
            o.set("session_gap", a.get(1)?.as_str());
        }
        "state_window" => {
            o.set("trigger", "state");
            o.set("state_expr", p.group()?);
        }
        "event_window" => {
            o.set("trigger", "event");
            let inner = p.group()?;
            let mut e = P::new(inner)?;
            e.expect("start")?;
            e.expect("with")?;
            let from = e.i;
            while !(e.kw("end")
                && e.toks
                    .get(e.i + 1)
                    .is_some_and(|t| t.t == T::Word && e.text(*t).eq_ignore_ascii_case("with")))
            {
                e.i += 1;
                if e.i >= e.toks.len() {
                    return None;
                }
            }
            let start = &inner[e.toks.get(from)?.a..e.toks[e.i - 1].b];
            e.i += 2;
            let end = &inner[e.toks.get(e.i)?.a..];
            o.set("start_with", start.trim());
            o.set("end_with", end.trim());
        }
        "count_window" => {
            let a = args(p.group()?);
            o.set("trigger", "count");
            o.set("count", a.first()?.as_str());
            let mut rest = &a[1..];
            if let Some(s) = rest
                .first()
                .filter(|s| s.bytes().all(|b| b.is_ascii_digit()))
            {
                o.set("count_sliding", s.as_str());
                rest = &rest[1..];
            }
            if !rest.is_empty() {
                o.set("count_cols", rest.join(", "));
            }
        }
        _ => return None,
    }
    if matches!(trigger.as_str(), "state_window" | "event_window") && p.eat("true_for") {
        o.set("true_for", p.group()?);
    }
    if p.eat("from") {
        o.set("from_table", p.word()?);
    }
    if p.eat("partition") {
        p.expect("by")?;
        o.set(
            "partition_by",
            p.until(&["stream_options", "notify", "into", "output_subtable", "as"])?,
        );
    }
    if p.eat("stream_options") {
        stream_options(&mut o, p.group()?)?;
    }
    if p.eat("notify") {
        o.urls = args(p.group()?).iter().map(|u| unquote(u)).collect();
        if p.eat("on") {
            let e: Vec<String> = split_top(p.group()?, b'|')
                .iter()
                .map(|x| x.to_ascii_lowercase())
                .collect();
            o.set("notify_events", e);
        }
        if p.eat("where") {
            o.set(
                "notify_where",
                p.until(&["notify_options", "into", "output_subtable", "as"])?,
            );
        }
        if p.eat("notify_options") {
            for x in split_top(p.group()?, b'|') {
                match x.to_ascii_lowercase().as_str() {
                    "notify_history" => o.set("notify_history", true),
                    "on_failure_pause" => o.set("on_failure_pause", true),
                    _ => return None,
                }
            }
        }
    }
    if p.eat("into") {
        o.set("into", p.word()?);
    }
    if p.eat("output_subtable") {
        o.set("subtable", p.group()?);
    }
    if p.at_group() {
        o.set("columns", p.group()?);
    }
    if p.eat("tags") {
        o.set("tags", p.group()?);
    }
    if p.eat("as") {
        o.set("query", p.until(&[])?);
    }
    p.done().then(|| o.done())
}

// ---------------------------------------------------------------------------
// Checks
// ---------------------------------------------------------------------------

/// A stream's or a database's name, as the server lists it.
pub(crate) fn plain(v: &str, label: &str) -> Result<String> {
    if crate::jobs::plain_name(v) && v.trim() == v {
        return Ok(v.to_string());
    }
    Err(bad(format!(
        "«{label}»: «{}» no es un nombre válido (hasta 192 caracteres, sin acentos graves ni saltos de línea).",
        shown(v)
    )))
}

/// A duration: a whole number and one of TDengine's units (b, u, a, s, m,
/// h, d, w, n, y).
pub(crate) fn dur(v: &str, label: &str) -> Result<String> {
    let t = v.trim().to_ascii_lowercase();
    let (n, u) = t.split_at(t.len().saturating_sub(1));
    let ok = !n.is_empty()
        && n.len() <= 18
        && n.bytes().all(|b| b.is_ascii_digit())
        && matches!(u, "b" | "u" | "a" | "s" | "m" | "h" | "d" | "w" | "n" | "y");
    if !ok {
        return Err(bad(format!(
            "«{label}»: «{}» no es una duración de TDengine (por ejemplo 10s, 5m, 1h, 1d).",
            shown(v)
        )));
    }
    Ok(t)
}

/// A table as the user wrote it: `[db.]table`, each part bare or
/// backquoted.
pub(crate) fn table(v: &str, label: &str) -> Result<String> {
    let t = v.trim();
    let one_word = lex(t).is_some_and(|toks| toks.len() == 1 && toks[0].t == T::Word);
    let parts = name_parts(t);
    let raw_parts: Vec<&str> = if one_word { split_raw(t) } else { Vec::new() };
    let ok = one_word
        && (1..=2).contains(&parts.len())
        && raw_parts.len() == parts.len()
        && raw_parts.iter().all(|p| {
            if let Some(inner) = p.strip_prefix('`').and_then(|x| x.strip_suffix('`')) {
                !inner.is_empty() && !inner.contains('`') && !inner.chars().any(char::is_control)
            } else {
                let mut b = p.bytes();
                b.next()
                    .is_some_and(|c| c.is_ascii_alphabetic() || c == b'_')
                    && b.all(|c| c.is_ascii_alphanumeric() || c == b'_')
            }
        });
    if !ok {
        return Err(bad(format!(
            "«{label}»: «{}» no es un nombre de tabla ([base.]tabla).",
            shown(v)
        )));
    }
    Ok(t.to_string())
}

/// `a.`b c`` → [`a`, `` `b c` ``].
fn split_raw(t: &str) -> Vec<&str> {
    let s = t.as_bytes();
    let mut out = Vec::new();
    let (mut start, mut i) = (0, 0);
    while i < s.len() {
        match s[i] {
            b'`' => i = skip_backtick(s, i).unwrap_or(s.len()),
            b'.' => {
                out.push(&t[start..i]);
                start = i + 1;
                i += 1;
            }
            _ => i += 1,
        }
    }
    out.push(&t[start..]);
    out
}

/// SQL the user wrote for a clause: quotes and parentheses closed, no `;`
/// and no comments, so it stays inside its clause.
pub(crate) fn fragment(v: &str, label: &str) -> Result<String> {
    let t = v.trim();
    let ok = t.len() <= 1 << 20
        && lex(t).is_some_and(|toks| {
            !toks.is_empty()
                && toks
                    .iter()
                    .all(|x| !(x.t == T::Punct && &t[x.a..x.b] == ";"))
        });
    if !ok {
        return Err(bad(format!(
            "«{label}» tiene comillas o paréntesis sin cerrar, un «;» o un comentario: tiene que ser una sola expresión."
        )));
    }
    Ok(t.to_string())
}

fn int(v: Vals, k: &str, label: &str, min: i64) -> Result<Option<i64>> {
    match v.opt(k) {
        None => Ok(None),
        Some(t) => match t.parse::<i64>() {
            Ok(n) if n >= min => Ok(Some(n)),
            _ => Err(bad(format!(
                "«{label}» tiene que ser un número entero desde {min}."
            ))),
        },
    }
}

/// `0` / `1`, or nothing.
fn flag01(v: Vals, k: &str, label: &str) -> Result<Option<&'static str>> {
    match v.opt(k).as_deref() {
        None => Ok(None),
        Some("1") | Some("true") => Ok(Some("1")),
        Some("0") | Some("false") => Ok(Some("0")),
        Some(o) => Err(bad(format!("«{label}»: «{}» no es 0 ni 1.", shown(o)))),
    }
}

/// A list of events (`window_open`, `window_close`).
fn events(v: Vals, k: &str) -> Result<Vec<&'static str>> {
    let mut out = Vec::new();
    for e in v.strings(k) {
        out.push(match e.to_ascii_lowercase().as_str() {
            "window_open" => "WINDOW_OPEN",
            "window_close" => "WINDOW_CLOSE",
            _ => {
                return Err(bad(format!(
                    "«{}» no es un evento de ventana (WINDOW_OPEN o WINDOW_CLOSE).",
                    shown(&e)
                )))
            }
        });
    }
    Ok(out)
}

fn urls(values: &FormValues) -> Result<Vec<String>> {
    let mut out = Vec::new();
    for r in super::rows(values, "notify_urls") {
        let Some(u) = r.opt("url") else { continue };
        if u.chars().any(char::is_control) || u.len() > 2000 {
            return Err(bad(format!("«{}» no es una URL válida.", shown(&u))));
        }
        out.push(lit(&u));
    }
    Ok(out)
}

/// The old engine's `TAGS` clause: as written, or, when the target table
/// already exists, only the tags' names (it refuses to define them again:
/// "Table already exist"; the new engine takes the definitions, and refuses
/// the names alone).
fn tags(v: &str, target_exists: bool) -> Result<String> {
    let t = fragment(v, "Tags")?;
    if !target_exists {
        return Ok(t);
    }
    let names: Option<Vec<String>> = split_top(&t, b',')
        .iter()
        .map(|item| {
            let toks = lex(item)?;
            let first = toks.first().filter(|x| x.t == T::Word)?;
            Some(item[first.a..first.b].to_string())
        })
        .collect();
    names
        .filter(|n| !n.is_empty())
        .map(|n| n.join(", "))
        .ok_or_else(|| bad("«Tags» tiene que ser una lista de tags separados por comas.".into()))
}

/// The table `into` names, `[db.]table` with `db` defaulting to the
/// stream's database, as TDengine stores the names (a bare one in
/// lowercase).
pub(crate) fn target_table(values: &FormValues) -> Option<(String, String)> {
    let v = Vals(&values.values);
    let into = table(&v.opt("into")?, "INTO").ok()?;
    let parts: Vec<String> = split_raw(&into)
        .iter()
        .map(
            |p| match p.strip_prefix('`').and_then(|x| x.strip_suffix('`')) {
                Some(inner) => inner.to_string(),
                None => p.to_ascii_lowercase(),
            },
        )
        .collect();
    match parts.as_slice() {
        [t] => Some((v.opt("database")?, t.clone())),
        [d, t] => Some((d.clone(), t.clone())),
        _ => None,
    }
}

/// The query after `AS`: one statement.
fn query(v: Vals) -> Result<Option<String>> {
    v.opt("query")
        .map(|q| fragment(q.trim_end().trim_end_matches(';'), "Consulta"))
        .transpose()
}

// ---------------------------------------------------------------------------
// Writing the statement
// ---------------------------------------------------------------------------

/// What a statement creates: the stream's database (the new engine's, or
/// the old one's database for the names without one), its name and the
/// `CREATE STREAM`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Built {
    pub(crate) db: String,
    pub(crate) name: String,
    pub(crate) sql: String,
}

fn db_and_name(v: Vals) -> Result<(String, String)> {
    let db = v
        .opt("database")
        .ok_or_else(|| bad("El stream necesita una base de datos.".into()))?;
    let name = v
        .opt("name")
        .ok_or_else(|| bad("El stream necesita un nombre.".into()))?;
    Ok((plain(&db, "Base de datos")?, plain(&name, "Nombre")?))
}

/// The old engine's `CREATE STREAM`.
pub(crate) fn build_old(values: &FormValues, notify: bool, target_exists: bool) -> Result<Built> {
    let v = Vals(&values.values);
    let (db, name) = db_and_name(v)?;
    let mut opts = Vec::new();
    match v.opt("trigger").as_deref() {
        None => {}
        Some(t @ ("at_once" | "window_close" | "force_window_close")) => {
            opts.push(format!("TRIGGER {}", t.to_ascii_uppercase()))
        }
        Some("max_delay") => {
            let d = v
                .opt("max_delay")
                .ok_or_else(|| bad("MAX_DELAY necesita su demora máxima.".into()))?;
            opts.push(format!("TRIGGER MAX_DELAY {}", dur(&d, "Demora máxima")?));
        }
        Some("continuous_window_close") => {
            let mut t = "TRIGGER CONTINUOUS_WINDOW_CLOSE".to_string();
            if let Some(r) = v.opt("recalculate") {
                t.push_str(&format!(" RECALCULATE {}", dur(&r, "Recalcular cada")?));
            }
            opts.push(t);
        }
        Some(o) => {
            return Err(bad(format!(
                "«{}» no es un modo de disparo de TDengine.",
                shown(o)
            )))
        }
    }
    if let Some(w) = v.opt("watermark") {
        opts.push(format!("WATERMARK {}", dur(&w, "Watermark")?));
    }
    if let Some(n) = flag01(v, "ignore_expired", "Ignorar datos vencidos")? {
        opts.push(format!("IGNORE EXPIRED {n}"));
    }
    if let Some(d) = v.opt("delete_mark") {
        opts.push(format!(
            "DELETE_MARK {}",
            dur(&d, "Conservar el estado (DELETE_MARK)")?
        ));
    }
    if let Some(n) = flag01(v, "fill_history", "Procesar los datos históricos")? {
        let asynch = n == "1" && v.bool("fill_history_async");
        opts.push(format!(
            "FILL_HISTORY {n}{}",
            if asynch { " ASYNC" } else { "" }
        ));
    }
    if let Some(n) = flag01(v, "ignore_update", "Ignorar actualizaciones")? {
        opts.push(format!("IGNORE UPDATE {n}"));
    }
    let mut sql = format!("CREATE STREAM {}", q(&name));
    if !opts.is_empty() {
        sql.push_str("\n  ");
        sql.push_str(&opts.join(" "));
    }
    let into = v
        .opt("into")
        .ok_or_else(|| bad("El stream necesita su supertabla de destino (INTO).".into()))?;
    sql.push_str(&format!("\n  INTO {}", table(&into, "Destino (INTO)")?));
    if let Some(c) = v.opt("columns") {
        sql.push_str(&format!(" ({})", fragment(&c, "Columnas")?));
    }
    if let Some(t) = v.opt("tags") {
        sql.push_str(&format!("\n  TAGS ({})", tags(&t, target_exists)?));
    }
    if let Some(s) = v.opt("subtable") {
        sql.push_str(&format!(
            "\n  SUBTABLE({})",
            fragment(&s, "Subtablas (SUBTABLE)")?
        ));
    }
    let query =
        query(v)?.ok_or_else(|| bad("El stream necesita su consulta (AS SELECT …).".into()))?;
    sql.push_str("\n  AS\n");
    sql.push_str(&query);
    let urls = urls(values)?;
    let ev = events(v, "notify_events")?;
    if !urls.is_empty() {
        if !notify {
            return Err(bad(
                "Esta versión de TDengine no tiene notificaciones de streams (llegaron en la 3.3.6).".into(),
            ));
        }
        if ev.is_empty() {
            return Err(bad(
                "La notificación necesita al menos un evento (WINDOW_OPEN o WINDOW_CLOSE).".into(),
            ));
        }
        let ev: Vec<String> = ev.iter().map(|e| lit(e)).collect();
        sql.push_str(&format!(
            "\n  NOTIFY({}) ON ({})",
            urls.join(", "),
            ev.join(", ")
        ));
        if let Some(n) = flag01(v, "notify_history", "Notificar también lo histórico")? {
            sql.push_str(&format!(" NOTIFY_HISTORY {n}"));
        }
        match v.opt("on_failure").as_deref() {
            None => {}
            Some("drop") => sql.push_str(" ON_FAILURE DROP"),
            Some("pause") => sql.push_str(" ON_FAILURE PAUSE"),
            Some(o) => {
                return Err(bad(format!(
                    "«{}» no es una opción de ON_FAILURE (drop o pause).",
                    shown(o)
                )))
            }
        }
    } else if !ev.is_empty() || v.opt("notify_history").is_some() || v.opt("on_failure").is_some() {
        return Err(bad(
            "Las opciones de notificación necesitan al menos una URL.".into(),
        ));
    }
    Ok(Built { db, name, sql })
}

/// `FILL_HISTORY`'s start: a number as it is, a date and time as a literal.
fn start_time(v: &str) -> Result<String> {
    let t = v.trim();
    if !t.is_empty() && t.bytes().all(|b| b.is_ascii_digit()) {
        return Ok(t.to_string());
    }
    if t.is_empty() || t.chars().any(char::is_control) || t.len() > 64 {
        return Err(bad(format!(
            "«Procesar desde»: «{}» no es una fecha y hora.",
            shown(v)
        )));
    }
    Ok(lit(t))
}

/// The new engine's `CREATE STREAM`.
pub(crate) fn build_new(values: &FormValues) -> Result<Built> {
    let v = Vals(&values.values);
    let (db, name) = db_and_name(v)?;
    let req = |k: &str, label: &str| v.opt(k).ok_or_else(|| bad(format!("Falta «{label}».")));
    let mut sql = format!("CREATE STREAM {}.{}\n  ", q(&db), q(&name));
    let trigger = v.opt("trigger").unwrap_or_else(|| "sliding".into());
    match trigger.as_str() {
        "period" => {
            let p = dur(&req("period", "Cada (PERIOD)")?, "Cada (PERIOD)")?;
            match v.opt("period_offset") {
                Some(o) => sql.push_str(&format!("PERIOD({p}, {})", dur(&o, "Desfase")?)),
                None => sql.push_str(&format!("PERIOD({p})")),
            }
        }
        "sliding" => {
            if let Some(i) = v.opt("interval") {
                let i = dur(&i, "Ventana (INTERVAL)")?;
                match v.opt("interval_offset") {
                    Some(o) => sql.push_str(&format!(
                        "INTERVAL({i}, {}) ",
                        dur(&o, "Desfase de la ventana")?
                    )),
                    None => sql.push_str(&format!("INTERVAL({i}) ")),
                }
            } else if v.opt("interval_offset").is_some() {
                return Err(bad(
                    "El desfase de la ventana necesita la ventana (INTERVAL).".into(),
                ));
            }
            let s = dur(&req("sliding", "Paso (SLIDING)")?, "Paso (SLIDING)")?;
            match v.opt("sliding_offset") {
                Some(o) => sql.push_str(&format!("SLIDING({s}, {})", dur(&o, "Desfase del paso")?)),
                None => sql.push_str(&format!("SLIDING({s})")),
            }
        }
        "session" => {
            let c = fragment(
                &req("session_col", "Columna de tiempo")?,
                "Columna de tiempo",
            )?;
            let g = dur(&req("session_gap", "Pausa máxima")?, "Pausa máxima")?;
            sql.push_str(&format!("SESSION({c}, {g})"));
        }
        "state" => {
            let e = fragment(&req("state_expr", "Estado")?, "Estado")?;
            sql.push_str(&format!("STATE_WINDOW({e})"));
        }
        "event" => {
            let s = fragment(&req("start_with", "Empieza cuando")?, "Empieza cuando")?;
            let e = fragment(&req("end_with", "Termina cuando")?, "Termina cuando")?;
            sql.push_str(&format!("EVENT_WINDOW(START WITH {s} END WITH {e})"));
        }
        "count" => {
            let n = int(v, "count", "Filas por ventana", 1)?
                .ok_or_else(|| bad("Falta «Filas por ventana».".into()))?;
            let mut a = vec![n.to_string()];
            if let Some(s) = int(v, "count_sliding", "Paso (filas)", 1)? {
                a.push(s.to_string());
            }
            if let Some(c) = v.opt("count_cols") {
                a.push(fragment(&c, "Columnas")?);
            }
            sql.push_str(&format!("COUNT_WINDOW({})", a.join(", ")));
        }
        o => {
            return Err(bad(format!(
                "«{}» no es un disparador de streams de TDengine.",
                shown(o)
            )))
        }
    }
    if matches!(trigger.as_str(), "state" | "event") {
        if let Some(t) = v.opt("true_for") {
            sql.push_str(&format!(
                " TRUE_FOR({})",
                fragment(&t, "Duración mínima (TRUE_FOR)")?
            ));
        }
    }
    if let Some(f) = v.opt("from_table") {
        sql.push_str(&format!(
            "\n  FROM {}",
            table(&f, "Tabla que lo dispara (FROM)")?
        ));
    }
    if let Some(pb) = v.opt("partition_by") {
        sql.push_str(&format!(
            "\n  PARTITION BY {}",
            fragment(&pb, "Agrupar por (PARTITION BY)")?
        ));
    }
    let mut opts = Vec::new();
    let tz = v.opt("timezone");
    let fdw = int(v, "first_day_of_week", "Primer día de la semana", 0)?;
    if fdw.is_some_and(|d| d > 6) {
        return Err(bad(
            "«Primer día de la semana» va de 0 (domingo) a 6 (sábado).".into(),
        ));
    }
    if tz.is_some() || fdw.is_some() {
        let mut c = Vec::new();
        if let Some(t) = &tz {
            if t.chars().any(char::is_control) || t.len() > 100 {
                return Err(bad(format!("«{}» no es una zona horaria.", shown(t))));
            }
            c.push(format!("timezone = {}", lit(t)));
        }
        if let Some(d) = fdw {
            c.push(format!("firstDayOfWeek = {d}"));
        }
        opts.push(format!("CONFIG({})", c.join(", ")));
    }
    for (k, word, label) in [
        ("watermark", "WATERMARK", "Watermark"),
        ("expired_time", "EXPIRED_TIME", "Vencimiento (EXPIRED_TIME)"),
    ] {
        if let Some(x) = v.opt(k) {
            opts.push(format!("{word}({})", dur(&x, label)?));
        }
    }
    for (k, word) in [
        ("ignore_disorder", "IGNORE_DISORDER"),
        ("delete_recalc", "DELETE_RECALC"),
        ("delete_output_table", "DELETE_OUTPUT_TABLE"),
    ] {
        if v.bool(k) {
            opts.push(word.into());
        }
    }
    match v.opt("fill").as_deref() {
        None => {
            if v.opt("fill_start").is_some() {
                return Err(bad(
                    "«Procesar desde» necesita que se procesen los datos históricos.".into(),
                ));
            }
        }
        Some(f @ ("fill_history" | "fill_history_first")) => {
            let word = f.to_ascii_uppercase();
            match v.opt("fill_start") {
                Some(s) => opts.push(format!("{word}({})", start_time(&s)?)),
                None => opts.push(word),
            }
        }
        Some(o) => {
            return Err(bad(format!(
                "«{}» no es una opción de históricos.",
                shown(o)
            )))
        }
    }
    for (k, word) in [
        ("calc_notify_only", "CALC_NOTIFY_ONLY"),
        ("low_latency_calc", "LOW_LATENCY_CALC"),
    ] {
        if v.bool(k) {
            opts.push(word.into());
        }
    }
    if let Some(pf) = v.opt("pre_filter") {
        opts.push(format!(
            "PRE_FILTER({})",
            fragment(&pf, "Filtro previo (PRE_FILTER)")?
        ));
    }
    if v.bool("force_output") {
        opts.push("FORCE_OUTPUT".into());
    }
    if let Some(m) = v.opt("max_delay") {
        opts.push(format!(
            "MAX_DELAY({})",
            dur(&m, "Demora máxima (MAX_DELAY)")?
        ));
    }
    let et = events(v, "event_type")?;
    if !et.is_empty() {
        opts.push(format!("EVENT_TYPE({})", et.join("|")));
    }
    if v.bool("ignore_nodata_trigger") {
        opts.push("IGNORE_NODATA_TRIGGER".into());
    }
    if let Some(x) = v.opt("extra_options") {
        let x = fragment(&x, "Otras opciones")?;
        if split_top(&x, b'|').iter().any(|o| o.is_empty()) {
            return Err(bad("«Otras opciones» van separadas por «|».".into()));
        }
        opts.push(x);
    }
    if !opts.is_empty() {
        sql.push_str(&format!("\n  STREAM_OPTIONS({})", opts.join(" | ")));
    }
    let urls = urls(values)?;
    let ev = events(v, "notify_events")?;
    let notify_opts: Vec<&str> = [
        ("notify_history", "NOTIFY_HISTORY"),
        ("on_failure_pause", "ON_FAILURE_PAUSE"),
    ]
    .iter()
    .filter(|(k, _)| v.bool(k))
    .map(|(_, w)| *w)
    .collect();
    let cond = v
        .opt("notify_where")
        .map(|w| fragment(&w, "Notificar si (WHERE)"))
        .transpose()?;
    if !urls.is_empty() {
        sql.push_str(&format!("\n  NOTIFY({})", urls.join(", ")));
        if !ev.is_empty() {
            sql.push_str(&format!(" ON ({})", ev.join("|")));
        }
        if let Some(c) = cond {
            sql.push_str(&format!(" WHERE {c}"));
        }
        if !notify_opts.is_empty() {
            sql.push_str(&format!(" NOTIFY_OPTIONS({})", notify_opts.join("|")));
        }
    } else if !ev.is_empty() || cond.is_some() || !notify_opts.is_empty() {
        return Err(bad(
            "Las opciones de notificación necesitan al menos una URL.".into(),
        ));
    }
    let into = v.opt("into");
    if let Some(i) = &into {
        sql.push_str(&format!("\n  INTO {}", table(i, "Destino (INTO)")?));
    }
    let mut out_parts = false;
    if let Some(s) = v.opt("subtable") {
        sql.push_str(&format!(
            "\n  OUTPUT_SUBTABLE({})",
            fragment(&s, "Subtablas (OUTPUT_SUBTABLE)")?
        ));
        out_parts = true;
    }
    if let Some(c) = v.opt("columns") {
        sql.push_str(&format!("\n  ({})", fragment(&c, "Columnas")?));
        out_parts = true;
    }
    if let Some(t) = v.opt("tags") {
        sql.push_str(&format!("\n  TAGS ({})", fragment(&t, "Tags")?));
        out_parts = true;
    }
    if out_parts && into.is_none() {
        return Err(bad(
            "Las subtablas, columnas y tags del resultado necesitan su tabla de destino (INTO)."
                .into(),
        ));
    }
    match query(v)? {
        Some(q) => {
            sql.push_str("\n  AS\n");
            sql.push_str(&q);
        }
        None if into.is_some() => {
            return Err(bad(
                "El stream escribe en una tabla: necesita su consulta (AS SELECT …).".into(),
            ))
        }
        None => {}
    }
    Ok(Built { db, name, sql })
}
