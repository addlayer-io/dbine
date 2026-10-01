//! Splits a script the way SQL*Plus / SQL Developer do:
//!
//! - plain SQL ends at `;` (outside quotes and comments), which is dropped;
//! - PL/SQL (`BEGIN`, `DECLARE`, `CREATE [OR REPLACE] PROCEDURE | FUNCTION |
//!   PACKAGE | TRIGGER | TYPE…`) keeps its `;`s and ends at a line holding
//!   only `/`, or at the end of the script;
//! - `EXEC proc(…)` becomes `BEGIN proc(…); END;`;
//! - SQL*Plus commands at the start of a statement take their line
//!   ([`Command`]): `PROMPT`, `SET SERVEROUTPUT`, `SHOW ERRORS` run in the
//!   driver, cosmetic settings are skipped, the rest are reported as not
//!   run. A `-` at the end of a command's line (or an `EXEC`'s) continues
//!   it on the next, as in SQL*Plus.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    pub plsql: bool,
    /// Byte offset of `text` in the script (of the line, for a command or
    /// an `EXEC`).
    pub start: usize,
    /// `text` is the script's own (an `EXEC` is rewritten as a block):
    /// positions the server reports in it are positions in the script.
    pub verbatim: bool,
    /// A SQL*Plus command: the driver handles it, nothing is sent as is.
    pub command: Option<Command>,
}

/// A SQL*Plus command line.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    /// `PRO[MPT] [text]`: the text as a message.
    Prompt(String),
    /// `SET SERVEROUT[PUT] ON|OFF …`: show DBMS_OUTPUT or not.
    ServerOutput(bool),
    /// `SHO[W] ERR[ORS] [type [schema.]name]`: the compile errors of that
    /// object, or of the last one compiled in the session.
    ShowErrors(Option<Object>),
    /// Display settings with nothing to do here (`SET LINESIZE`, `REM`,
    /// `COLUMN`…).
    Ignored,
    /// A command DBine doesn't run (`SPOOL`, `@file`, `CONNECT`,
    /// `WHENEVER`…): reported as skipped.
    Unsupported,
}

/// A schema object that compiles (`SHOW ERRORS`, `ALL_ERRORS`).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Object {
    /// As `ALL_ERRORS.TYPE` spells it: `PROCEDURE`, `PACKAGE BODY`…
    pub kind: String,
    /// `None`: the session's current schema.
    pub owner: Option<String>,
    pub name: String,
    /// 1-based line of the statement text where the object's source starts
    /// (its kind keyword), for the units whose error lines are counted from
    /// there.
    pub source_line: Option<u32>,
}

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    Single,
    Double,
    /// `q'[…]'`: the closing delimiter.
    QQuote(char),
    Block,
}

/// SQL*Plus commands with nothing to do here, and the shortest abbreviation
/// SQL*Plus takes for each.
const IGNORED: &[(&str, usize)] = &[
    ("BREAK", 3),
    ("BTITLE", 3),
    ("CLEAR", 2),
    ("COLUMN", 3),
    ("COMPUTE", 4),
    ("REMARK", 3),
    ("REPFOOTER", 4),
    ("REPHEADER", 4),
    ("TIMING", 4),
    ("TTITLE", 3),
];

/// SQL*Plus commands DBine doesn't run.
const UNSUPPORTED: &[(&str, usize)] = &[
    ("ACCEPT", 3),
    ("ARCHIVE", 7),
    ("CONNECT", 4),
    ("DEFINE", 3),
    ("DESCRIBE", 4),
    ("DISCONNECT", 4),
    ("EXIT", 4),
    ("HOST", 3),
    ("PASSWORD", 6),
    ("PAUSE", 3),
    ("PRINT", 5),
    ("QUIT", 4),
    ("RECOVER", 7),
    ("SHOW", 3),
    ("SHUTDOWN", 8),
    ("SPOOL", 3),
    ("START", 3),
    ("STARTUP", 7),
    ("UNDEFINE", 5),
    ("VARIABLE", 3),
    ("WHENEVER", 8),
];

/// `word` (upper case) abbreviates one of `list`'s commands.
fn is_one_of(word: &str, list: &[(&str, usize)]) -> bool {
    list.iter().any(|(c, min)| word.len() >= *min && c.starts_with(word))
}

/// The SQL*Plus command on this (trimmed) line, if it is one. `EXEC` is
/// handled apart (it becomes a block).
pub fn command(line: &str) -> Option<Command> {
    if line.starts_with('@') {
        return Some(Command::Unsupported);
    }
    // SQL*Plus takes an optional `;` after its commands (not after PROMPT,
    // whose text runs to the end of the line).
    let words: Vec<String> =
        line.trim_end_matches(';').split_whitespace().take(5).map(str::to_ascii_uppercase).collect();
    let first = words.first()?.as_str();
    if first.len() >= 3 && "PROMPT".starts_with(first) {
        let rest = line[first.len()..].strip_prefix(|c: char| c.is_whitespace()).unwrap_or("");
        return Some(Command::Prompt(rest.trim_end().to_string()));
    }
    if first == "SET" {
        let var = words.get(1).map_or("", String::as_str);
        return match var {
            // SQL, not SQL*Plus.
            "TRANSACTION" | "ROLE" | "CONSTRAINT" | "CONSTRAINTS" => None,
            v if v.len() >= 9 && "SERVEROUTPUT".starts_with(v) => match words.get(2).map(String::as_str) {
                Some("ON") => Some(Command::ServerOutput(true)),
                Some("OFF") => Some(Command::ServerOutput(false)),
                _ => Some(Command::Ignored),
            },
            _ => Some(Command::Ignored),
        };
    }
    if first.len() >= 3 && "SHOW".starts_with(first) {
        if words.get(1).is_some_and(|w| w.len() >= 3 && "ERRORS".starts_with(w.as_str())) {
            let after = line.trim_end_matches(';').split_whitespace().skip(2).collect::<Vec<_>>().join(" ");
            return Some(Command::ShowErrors(show_errors_target(&after)));
        }
        return Some(Command::Unsupported);
    }
    if is_one_of(first, IGNORED) {
        return Some(Command::Ignored);
    }
    if is_one_of(first, UNSUPPORTED) {
        return Some(Command::Unsupported);
    }
    None
}

/// `[type [schema.]name]` after SHOW ERRORS.
fn show_errors_target(rest: &str) -> Option<Object> {
    let mut words = rest.split_whitespace();
    let mut kind = words.next()?.to_ascii_uppercase();
    let mut name = words.next()?;
    if (matches!(kind.as_str(), "PACKAGE" | "TYPE") && name.eq_ignore_ascii_case("BODY"))
        || (kind == "JAVA" && (name.eq_ignore_ascii_case("SOURCE") || name.eq_ignore_ascii_case("CLASS")))
    {
        kind = format!("{kind} {}", name.to_ascii_uppercase());
        name = words.next()?;
    }
    let (owner, name) = object_name(name)?;
    Some(Object { kind, owner, name, source_line: None })
}

/// `[schema.]name` as the dictionary stores it: unquoted parts upper-cased,
/// quoted ones as written.
fn object_name(s: &str) -> Option<(Option<String>, String)> {
    let mut parts = Vec::new();
    let mut rest = s;
    loop {
        let (part, after) = if let Some(q) = rest.strip_prefix('"') {
            let end = q.find('"')?;
            (q[..end].to_string(), &q[end + 1..])
        } else {
            let end = rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len());
            (rest[..end].to_ascii_uppercase(), &rest[end..])
        };
        if part.is_empty() {
            return None;
        }
        parts.push(part);
        match after.strip_prefix('.') {
            Some(a) if parts.len() < 2 => rest = a,
            _ => break,
        }
    }
    let name = parts.pop()?;
    Some((parts.pop(), name))
}

/// The object a `CREATE` / `ALTER … COMPILE` of PL/SQL (or a view) compiles.
pub fn compiled_object(stmt: &str) -> Option<Object> {
    let text = strip_leading_comments(stmt);
    let base = stmt.len() - text.len();
    let mut tokens = words_at(text);
    let (verb, _) = tokens.next()?;
    if !verb.eq_ignore_ascii_case("CREATE") && !verb.eq_ignore_ascii_case("ALTER") {
        return None;
    }
    let mut kind_at = None;
    let mut kind = String::new();
    for (w, at) in tokens.by_ref() {
        let u = w.to_ascii_uppercase();
        if kind.is_empty() {
            match u.as_str() {
                "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE" | "EDITIONING" | "FORCE" | "NO" | "AND"
                | "RESOLVE" | "COMPILE" | "NOFORCE" | "NAMED" => continue,
                "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "VIEW" | "LIBRARY" | "JAVA" => {
                    kind = u;
                    kind_at = Some(at);
                    continue;
                }
                _ => return None,
            }
        }
        if (matches!(kind.as_str(), "PACKAGE" | "TYPE") && u == "BODY") || (kind == "JAVA" && matches!(u.as_str(), "SOURCE" | "CLASS")) {
            kind = format!("{kind} {u}");
            continue;
        }
        if u == "IF" || u == "NOT" || u == "EXISTS" {
            continue;
        }
        let rest = &text[at..];
        let (owner, name) = object_name(rest)?;
        let source_line = if kind == "TRIGGER" {
            // A trigger's error lines count from its PL/SQL block.
            trigger_block(stmt).map(|b| (stmt[..b].matches('\n').count() + 1) as u32)
        } else {
            kind_at.map(|k| (stmt[..base + k].matches('\n').count() + 1) as u32)
        };
        return Some(Object { kind, owner, name, source_line });
    }
    None
}

/// Where a trigger's PL/SQL block starts: its first DECLARE or BEGIN
/// outside comments and quotes. `None` for a compound trigger (its lines
/// don't count from one block).
fn trigger_block(stmt: &str) -> Option<usize> {
    let b = stmt.as_bytes();
    let mut i = 0;
    while i < b.len() {
        match b[i] {
            b'-' if b.get(i + 1) == Some(&b'-') => i = stmt[i..].find('\n').map_or(b.len(), |n| i + n),
            b'/' if b.get(i + 1) == Some(&b'*') => i = stmt[i + 2..].find("*/").map_or(b.len(), |n| i + n + 4),
            q @ (b'\'' | b'"') => i = stmt[i + 1..].find(q as char).map_or(b.len(), |n| i + n + 2),
            c if c.is_ascii_alphabetic() => {
                let end = stmt[i..].find(|c: char| !is_ident(c)).map_or(b.len(), |n| i + n);
                let w = &stmt[i..end];
                if w.eq_ignore_ascii_case("COMPOUND") {
                    return None;
                }
                if w.eq_ignore_ascii_case("BEGIN") || w.eq_ignore_ascii_case("DECLARE") {
                    return Some(i);
                }
                i = end;
            }
            _ => i += 1,
        }
    }
    None
}

/// The words of `s` (identifier runs and quoted names) with their offsets,
/// stopping at the first other character that isn't a space.
fn words_at(s: &str) -> impl Iterator<Item = (&str, usize)> {
    let mut i = 0;
    std::iter::from_fn(move || {
        let rest = &s[i..];
        let skip = rest.len() - rest.trim_start().len();
        i += skip;
        let rest = &s[i..];
        let len = if let Some(quoted) = rest.strip_prefix('"') {
            quoted.find('"').map(|e| e + 2)?
        } else {
            rest.find(|c: char| !is_ident(c)).unwrap_or(rest.len())
        };
        if len == 0 {
            return None;
        }
        let at = i;
        i += len;
        Some((&s[at..at + len], at))
    })
}

/// The statement's kind as a tag (`CREATE TABLE`, `INSERT`, `PL/SQL`…).
pub fn tag(stmt: &str) -> String {
    let words: Vec<String> = strip_leading_comments(stmt)
        .split(|c: char| !is_ident(c))
        .filter(|w| !w.is_empty())
        .take(10)
        .map(str::to_ascii_uppercase)
        .collect();
    let w = |i: usize| words.get(i).map_or("", String::as_str);
    match w(0) {
        "BEGIN" | "DECLARE" => "PL/SQL".into(),
        "WITH" => "SELECT".into(),
        "CREATE" | "ALTER" | "DROP" | "TRUNCATE" => {
            const MODIFIERS: &[&str] = &[
                "OR", "REPLACE", "EDITIONABLE", "NONEDITIONABLE", "EDITIONING", "FORCE", "NO", "GLOBAL", "PRIVATE",
                "TEMPORARY", "SHARDED", "DUPLICATED", "IMMUTABLE", "BLOCKCHAIN", "UNIQUE", "BITMAP", "MULTIVALUE",
                "PUBLIC", "SHARED", "IF", "NOT", "EXISTS",
            ];
            let Some(k) = (1..words.len()).find(|&i| !MODIFIERS.contains(&w(i))) else { return w(0).into() };
            let two = matches!((w(k), w(k + 1)), ("PACKAGE" | "TYPE", "BODY") | ("MATERIALIZED", "VIEW" | "ZONEMAP") | ("DATABASE", "LINK") | ("JAVA", _));
            if two {
                format!("{} {} {}", w(0), w(k), w(k + 1))
            } else {
                format!("{} {}", w(0), w(k))
            }
        }
        "SET" | "LOCK" if !w(1).is_empty() => format!("{} {}", w(0), w(1)),
        other => other.into(),
    }
}

pub fn split(sql: &str) -> Vec<Statement> {
    let mut sp = Splitter::default();
    let mut base = 0;
    for line in sql.split_inclusive('\n') {
        sp.line(line, base);
        base += line.len();
    }
    sp.flush();
    sp.out
}

#[derive(Default)]
struct Splitter {
    out: Vec<Statement>,
    cur: String,
    /// Byte offset in the script of `cur`'s first character (`cur` is always
    /// a verbatim slice of the script).
    cur_start: usize,
    state: Option<State>,
    /// Decided at the statement's first `;`.
    plsql: Option<bool>,
    /// A command line ended by `-` (SQL*Plus continuation): its text so
    /// far, without the `-`, and where it starts.
    cont: Option<(String, usize)>,
}

impl Splitter {
    fn state(&self) -> State {
        self.state.unwrap_or(State::Code)
    }

    fn line(&mut self, line: &str, base: usize) {
        let trimmed = line.trim();
        if let Some((mut text, start)) = self.cont.take() {
            text.push_str(trimmed);
            match text.strip_suffix('-') {
                Some(head) => self.cont = Some((head.to_string(), start)),
                None => {
                    self.command_line(&text, start, false);
                }
            }
            return;
        }
        if self.state() == State::Code {
            if trimmed == "/" {
                self.flush();
                return;
            }
            if strip_leading_comments(&self.cur).is_empty() {
                let start = base + (line.len() - line.trim_start().len());
                if let Some(head) = trimmed.strip_suffix('-').filter(|_| is_command_line(trimmed)) {
                    // Comments before it are dropped with it.
                    self.cur.clear();
                    self.cont = Some((head.to_string(), start));
                    return;
                }
                if self.command_line(trimmed, start, true) {
                    return;
                }
            }
        }
        let chars: Vec<(usize, char)> = line.char_indices().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i].1;
            let next = chars.get(i + 1).map(|x| x.1);
            if self.cur.is_empty() {
                self.cur_start = base + chars[i].0;
            }
            match self.state() {
                State::Block => {
                    self.cur.push(c);
                    if c == '*' && next == Some('/') {
                        self.cur.push('/');
                        i += 1;
                        self.state = None;
                    }
                }
                State::Single => {
                    self.cur.push(c);
                    if c == '\'' {
                        if next == Some('\'') {
                            self.cur.push('\'');
                            i += 1;
                        } else {
                            self.state = None;
                        }
                    }
                }
                State::QQuote(close) => {
                    self.cur.push(c);
                    if c == close && next == Some('\'') {
                        self.cur.push('\'');
                        i += 1;
                        self.state = None;
                    }
                }
                State::Double => {
                    self.cur.push(c);
                    if c == '"' {
                        self.state = None;
                    }
                }
                State::Code => match c {
                    '-' if next == Some('-') => {
                        // Rest of the line is a comment.
                        self.cur.push_str(&line[chars[i].0..]);
                        return;
                    }
                    '/' if next == Some('*') => {
                        self.cur.push_str("/*");
                        i += 1;
                        self.state = Some(State::Block);
                    }
                    '\'' => {
                        let q_prefix = {
                            let before: Vec<char> = self.cur.chars().rev().take(3).collect();
                            matches!(before.first(), Some('q' | 'Q'))
                                && match before.get(1) {
                                    None => true,
                                    Some('n' | 'N') => !before.get(2).is_some_and(|c| is_ident(*c)),
                                    Some(c) => !is_ident(*c),
                                }
                        };
                        self.cur.push('\'');
                        match (q_prefix, next) {
                            (true, Some(open)) => {
                                self.cur.push(open);
                                i += 1;
                                self.state = Some(State::QQuote(closing(open)));
                            }
                            _ => self.state = Some(State::Single),
                        }
                    }
                    '"' => {
                        self.cur.push('"');
                        self.state = Some(State::Double);
                    }
                    ';' => {
                        let plsql = *self.plsql.get_or_insert_with(|| is_plsql(&self.cur));
                        if plsql {
                            self.cur.push(';');
                        } else {
                            self.flush();
                        }
                    }
                    _ => self.cur.push(c),
                },
            }
            i += 1;
        }
    }

    /// A SQL*Plus command or an `EXEC` that starts at `start`: pushed, and
    /// `true`; `false` when `text` is neither. `verbatim`: `text` is the
    /// script's own (not joined from continued lines).
    fn command_line(&mut self, text: &str, start: usize, verbatim: bool) -> bool {
        if let Some(cmd) = command(text) {
            // Comments before it are dropped with it.
            self.cur.clear();
            self.out.push(Statement { text: text.to_string(), plsql: false, start, verbatim, command: Some(cmd) });
            return true;
        }
        if let Some(call) = exec_call(text) {
            self.cur.clear();
            self.out.push(Statement { text: format!("BEGIN {call}; END;"), plsql: true, start, verbatim: false, command: None });
            return true;
        }
        false
    }

    fn flush(&mut self) {
        // A continued command at the end of the script ends there.
        if let Some((text, start)) = self.cont.take() {
            self.command_line(&text, start, false);
        }
        let cur = std::mem::take(&mut self.cur);
        let plsql = self.plsql.take().unwrap_or_else(|| is_plsql(&cur));
        self.state = None;
        let stripped = strip_leading_comments(&cur);
        let start = self.cur_start + (cur.len() - stripped.len());
        let text = stripped.trim_end();
        if !text.is_empty() {
            self.out.push(Statement { text: text.to_string(), plsql, start, verbatim: true, command: None });
        }
    }
}

fn is_ident(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '#'
}

fn closing(open: char) -> char {
    match open {
        '[' => ']',
        '{' => '}',
        '(' => ')',
        '<' => '>',
        c => c,
    }
}

/// The call of an `EXEC[UTE] call` line (`EXECUTE IMMEDIATE` is SQL's,
/// not SQL*Plus's), without its `;`.
fn exec_call(line: &str) -> Option<&str> {
    let call = strip_word(line, "EXEC").or_else(|| strip_word(line, "EXECUTE"))?.trim().trim_end_matches(';').trim();
    (!call.is_empty() && !call.to_ascii_uppercase().starts_with("IMMEDIATE")).then_some(call)
}

/// A line SQL*Plus reads as its own command (or an `EXEC`), where a `-`
/// at the end continues it.
fn is_command_line(line: &str) -> bool {
    command(line).is_some() || exec_call(line).is_some()
}

/// `rest` when `line` starts with the word `w` (case-insensitive).
fn strip_word<'a>(line: &'a str, w: &str) -> Option<&'a str> {
    let head = line.get(..w.len())?;
    let rest = &line[w.len()..];
    (head.eq_ignore_ascii_case(w) && rest.starts_with(char::is_whitespace)).then_some(rest)
}

/// The text without leading whitespace and comments.
pub fn strip_leading_comments(s: &str) -> &str {
    let mut s = s.trim_start();
    loop {
        if let Some(rest) = s.strip_prefix("--") {
            s = rest.find('\n').map_or("", |i| &rest[i + 1..]).trim_start();
        } else if let Some(rest) = s.strip_prefix("/*") {
            s = rest.find("*/").map_or("", |i| &rest[i + 2..]).trim_start();
        } else {
            return s;
        }
    }
}

/// Whether `sql` has a `:name` outside quotes and comments, i.e. something
/// the client would take for a bind variable (`:new`, `:old` in a trigger).
pub fn has_bind_like(sql: &str) -> bool {
    let chars: Vec<char> = sql.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '-' if next == Some('-') => {
                while i < chars.len() && chars[i] != '\n' {
                    i += 1;
                }
            }
            '/' if next == Some('*') => {
                i += 2;
                while i + 1 < chars.len() && !(chars[i] == '*' && chars[i + 1] == '/') {
                    i += 1;
                }
                i += 1;
            }
            '\'' => {
                let q = i > 0 && matches!(chars[i - 1], 'q' | 'Q');
                if let (true, Some(open)) = (q, next) {
                    let close = closing(open);
                    i += 2;
                    while i + 1 < chars.len() && !(chars[i] == close && chars[i + 1] == '\'') {
                        i += 1;
                    }
                    i += 1;
                } else {
                    i += 1;
                    while i < chars.len() && chars[i] != '\'' {
                        i += 1;
                    }
                }
            }
            '"' => {
                i += 1;
                while i < chars.len() && chars[i] != '"' {
                    i += 1;
                }
            }
            ':' if next.is_some_and(|n| n.is_alphanumeric() || n == '"') => return true,
            _ => {}
        }
        i += 1;
    }
    false
}

/// First word, upper-cased, ignoring leading comments.
pub fn first_word(sql: &str) -> String {
    strip_leading_comments(sql).chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_uppercase()
}

/// Whether a statement is a PL/SQL unit (its `;`s don't end it).
fn is_plsql(stmt: &str) -> bool {
    let words: Vec<String> = strip_leading_comments(stmt)
        .split(|c: char| !is_ident(c))
        .filter(|w| !w.is_empty())
        .take(6)
        .map(str::to_ascii_uppercase)
        .collect();
    let w: Vec<&str> = words.iter().map(String::as_str).collect();
    match w.first() {
        Some(&"BEGIN" | &"DECLARE") => true,
        Some(&"CREATE") => w[1..]
            .iter()
            .find(|x| !matches!(**x, "OR" | "REPLACE" | "EDITIONABLE" | "NONEDITIONABLE" | "EDITIONING"))
            .is_some_and(|x| matches!(*x, "PROCEDURE" | "FUNCTION" | "PACKAGE" | "TRIGGER" | "TYPE" | "LIBRARY")),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(sql: &str) -> Vec<String> {
        split(sql).into_iter().map(|s| s.text).collect()
    }

    #[test]
    fn plain_sql_splits_on_semicolons() {
        assert_eq!(texts("select 1 from dual; select ';' from dual;\n"), vec![
            "select 1 from dual",
            "select ';' from dual"
        ]);
        assert_eq!(texts("-- lead\nselect 1 from dual -- x;\n;"), vec!["select 1 from dual -- x;"]);
        assert_eq!(texts("select q'[a;b]' from dual; select 'it''s;' from dual"), vec![
            "select q'[a;b]' from dual",
            "select 'it''s;' from dual"
        ]);
        assert_eq!(texts("select /* ; */ 1 from dual"), vec!["select /* ; */ 1 from dual"]);
    }

    #[test]
    fn plsql_blocks_end_at_slash() {
        let sql = "create or replace procedure p as\nbegin\n  null;\nend;\n/\nselect 1 from dual;\nbegin\n  p;\nend;\n/\n";
        let s = split(sql);
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].text, "create or replace procedure p as\nbegin\n  null;\nend;");
        assert!(s[0].plsql);
        assert_eq!(s[1].text, "select 1 from dual");
        assert!(!s[1].plsql);
        assert_eq!(s[2].text, "begin\n  p;\nend;");
    }

    #[test]
    fn plsql_without_slash_runs_to_the_end() {
        let s = split("declare x number; begin x := 1; end;");
        assert_eq!(s.len(), 1);
        assert!(s[0].plsql);
        assert_eq!(s[0].text, "declare x number; begin x := 1; end;");
    }

    #[test]
    fn slash_inside_a_string_is_kept() {
        let s = texts("begin\n  x := '\n/\n';\nend;\n/");
        assert_eq!(s, vec!["begin\n  x := '\n/\n';\nend;"]);
    }

    #[test]
    fn sqlplus_commands() {
        let s = split("SET SERVEROUTPUT ON\nexec dbms_output.put_line('hi');\nselect 1 from dual;\n/\n");
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].command, Some(Command::ServerOutput(true)));
        assert_eq!(s[1].text, "BEGIN dbms_output.put_line('hi'); END;");
        assert!(s[1].plsql && !s[1].verbatim);
        assert_eq!(s[1].start, 20);
        assert_eq!(s[2].text, "select 1 from dual");
    }

    #[test]
    fn a_dash_continues_a_command_line() {
        let sql = "EXEC p(1, -\n  2)\nPROMPT one -\ntwo\nselect 1 -\n1 from dual;\nexec -\n";
        let s = split(sql);
        assert_eq!(s.len(), 3, "{s:?}");
        assert_eq!(s[0].text, "BEGIN p(1, 2); END;");
        assert_eq!(s[0].start, 0);
        assert_eq!(s[1].command, Some(Command::Prompt("one two".into())));
        assert_eq!(s[1].start, sql.find("PROMPT").unwrap());
        // In SQL a trailing `-` is a minus.
        assert_eq!(s[2].text, "select 1 -\n1 from dual");
        // Continued at the end of the script.
        assert_eq!(texts("exec p(-"), vec!["BEGIN p(; END;"]);
        assert_eq!(texts("EXECUTE IMMEDIATE -"), vec!["EXECUTE IMMEDIATE -"]);
    }

    #[test]
    fn command_lines() {
        assert_eq!(command("PROMPT it's done; ok"), Some(Command::Prompt("it's done; ok".into())));
        assert_eq!(command("pro"), Some(Command::Prompt(String::new())));
        assert_eq!(command("set serverout off;"), Some(Command::ServerOutput(false)));
        assert_eq!(command("SET SERVEROUTPUT ON SIZE UNLIMITED FORMAT WRAPPED"), Some(Command::ServerOutput(true)));
        assert_eq!(command("SET DEFINE OFF"), Some(Command::Ignored));
        assert_eq!(command("REM a note"), Some(Command::Ignored));
        assert_eq!(command("col name format a20"), Some(Command::Ignored));
        assert_eq!(command("SET TRANSACTION READ ONLY"), None);
        assert_eq!(command("set role all"), None);
        assert_eq!(command("COMMIT"), None);
        assert_eq!(command("COMMENT ON TABLE t IS 'x'"), None);
        assert_eq!(command("CALL p()"), None);
        assert_eq!(command("spool out.log"), Some(Command::Unsupported));
        assert_eq!(command("@install.sql"), Some(Command::Unsupported));
        assert_eq!(command("WHENEVER SQLERROR EXIT"), Some(Command::Unsupported));
        assert_eq!(command("SHOW USER"), Some(Command::Unsupported));
        assert_eq!(command("SHO ERR"), Some(Command::ShowErrors(None)));
        assert_eq!(
            command("show errors package body scott.\"Pkg\";"),
            Some(Command::ShowErrors(Some(Object {
                kind: "PACKAGE BODY".into(),
                owner: Some("SCOTT".into()),
                name: "Pkg".into(),
                source_line: None
            })))
        );
        assert_eq!(
            command("SHOW ERRORS PROCEDURE p"),
            Some(Command::ShowErrors(Some(Object { kind: "PROCEDURE".into(), owner: None, name: "P".into(), source_line: None })))
        );
    }

    #[test]
    fn statements_keep_their_offsets() {
        let sql = "-- head\nselect 'é' from dual;\n  /* c */ insert into t values (1);\nPROMPT x\nbegin\n  null;\nend;\n/\nselect 2 from dual";
        for s in split(sql) {
            assert_eq!(&sql[s.start..s.start + s.text.len()], s.text, "{s:?}");
        }
        let starts: Vec<usize> = split(sql).iter().map(|s| s.start).collect();
        assert_eq!(starts.len(), 5);
        assert_eq!(&sql[starts[1]..starts[1] + 6], "insert");
    }

    #[test]
    fn compiled_objects() {
        let o = compiled_object("CREATE OR REPLACE\nPROCEDURE p AS BEGIN NULL; END;").unwrap();
        assert_eq!((o.kind.as_str(), o.owner, o.name.as_str(), o.source_line), ("PROCEDURE", None, "P", Some(2)));
        let o = compiled_object("-- x\ncreate or replace editionable package body Hr.\"Pkg\" as end;").unwrap();
        assert_eq!((o.kind.as_str(), o.owner.as_deref(), o.name.as_str(), o.source_line), ("PACKAGE BODY", Some("HR"), "Pkg", Some(2)));
        let o = compiled_object("create or replace force view v as select 1 x from dual").unwrap();
        assert_eq!((o.kind.as_str(), o.name.as_str()), ("VIEW", "V"));
        let o = compiled_object("create trigger t\nbefore insert on x -- begin\nfor each row when (new.a = 'begin')\nDECLARE n number;\nbegin null; end;").unwrap();
        assert_eq!((o.kind.as_str(), o.source_line), ("TRIGGER", Some(4)));
        let o = compiled_object("create trigger t for insert on x\ncompound trigger\nbefore statement is begin null; end before statement;\nend;").unwrap();
        assert_eq!(o.source_line, None);
        let o = compiled_object("ALTER TRIGGER trg COMPILE").unwrap();
        assert_eq!((o.kind.as_str(), o.name.as_str()), ("TRIGGER", "TRG"));
        let o = compiled_object("create type body t_point as member function x return number is begin return 1; end; end;").unwrap();
        assert_eq!((o.kind.as_str(), o.name.as_str()), ("TYPE BODY", "T_POINT"));
        assert_eq!(compiled_object("create table t (a number)"), None);
        assert_eq!(compiled_object("alter session set nls_date_format = 'YYYY'"), None);
        assert_eq!(compiled_object("begin null; end;"), None);
    }

    #[test]
    fn tags() {
        assert_eq!(tag("create global temporary table t (a number)"), "CREATE TABLE");
        assert_eq!(tag("CREATE OR REPLACE PACKAGE BODY p AS END;"), "CREATE PACKAGE BODY");
        assert_eq!(tag("create materialized view mv as select 1 from dual"), "CREATE MATERIALIZED VIEW");
        assert_eq!(tag("create unique index i on t (a)"), "CREATE INDEX");
        assert_eq!(tag("drop table t purge"), "DROP TABLE");
        assert_eq!(tag("truncate table t"), "TRUNCATE TABLE");
        assert_eq!(tag("/* x */ insert into t values (1)"), "INSERT");
        assert_eq!(tag("with a as (select 1 from dual) select * from a"), "SELECT");
        assert_eq!(tag("declare x number; begin null; end;"), "PL/SQL");
        assert_eq!(tag("set transaction read only"), "SET TRANSACTION");
        assert_eq!(tag("alter session set x = 1"), "ALTER SESSION");
        assert_eq!(tag("commit"), "COMMIT");
    }

    #[test]
    fn bind_like_colons() {
        assert!(has_bind_like("create trigger t before insert on x for each row begin :new.a := 1; end;"));
        assert!(!has_bind_like("begin x := 1; end;"));
        assert!(!has_bind_like("select ':a', q'[:b]', \":c\" from dual -- :d\n/* :e */"));
    }

    #[test]
    fn create_table_is_plain_sql() {
        assert!(!is_plsql("CREATE TABLE t (a NUMBER)"));
        assert!(is_plsql("CREATE OR REPLACE EDITIONABLE PACKAGE BODY x AS"));
        assert!(is_plsql("/* c */ create trigger t before insert on x"));
        assert!(!is_plsql("create or replace view v as select 1 from dual"));
    }
}
