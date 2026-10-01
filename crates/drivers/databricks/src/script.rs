//! Editor scripts on the Statement Execution API: how a script is split,
//! the `USE` that the session keeps (each request is stateless: catalog and
//! schema travel with it), and errors with their class, SQLSTATE and
//! position.

use crate::blocks;
use dbine_driver::sql::{split_script, ScriptDialect, ScriptStatement};
use dbine_driver::ScriptError;

/// The Databricks SQL editor's reading: backslash escapes, `` `ident` ``.
pub fn dialect() -> ScriptDialect {
    ScriptDialect { backslash_escapes: true, ..ScriptDialect::generic() }
}

/// The statements as the SQL editor runs them: compound `BEGIN … END`
/// blocks (SQL scripting) whole.
pub fn units(text: &str) -> Vec<ScriptStatement> {
    blocks::merge(text, split_script(text, &dialect()), blocks::Rules::default())
}

/// What a `USE` / `SET CATALOG` changes.
#[derive(Debug, PartialEq)]
pub enum Use {
    Catalog(String),
    Schema(String),
    Both(String, String),
}

/// `USE CATALOG c`, `SET CATALOG c`, `USE [SCHEMA|DATABASE] [c.]s`,
/// `SET SCHEMA s`.
pub fn use_target(stmt: &str) -> Option<Use> {
    let words = idents(stmt);
    let up: Vec<String> = words.iter().take(2).map(|w| w.to_ascii_uppercase()).collect();
    let kw = |i: usize, w: &str| up.get(i).is_some_and(|u| u == w);
    let (kind, skip) = match () {
        _ if (kw(0, "USE") || kw(0, "SET")) && kw(1, "CATALOG") => ("catalog", 2),
        _ if (kw(0, "USE") || kw(0, "SET")) && (kw(1, "SCHEMA") || kw(1, "DATABASE")) => ("schema", 2),
        _ if kw(0, "USE") => ("schema", 1),
        _ => return None,
    };
    let parts = name_parts(&stmt_rest(stmt, skip))?;
    match (kind, parts.as_slice()) {
        ("catalog", [c]) => Some(Use::Catalog(c.clone())),
        ("schema", [s]) => Some(Use::Schema(s.clone())),
        ("schema", [c, s]) => Some(Use::Both(c.clone(), s.clone())),
        _ => None,
    }
}

/// The plain words of a statement (enough to read its first keywords).
fn idents(stmt: &str) -> Vec<&str> {
    stmt.split(|c: char| c.is_whitespace()).filter(|w| !w.is_empty()).collect()
}

/// The text after the first `n` words.
fn stmt_rest(stmt: &str, n: usize) -> String {
    let mut s = stmt.trim_start();
    for _ in 0..n {
        let end = s.find(char::is_whitespace).unwrap_or(s.len());
        s = s[end..].trim_start();
    }
    s.trim().trim_end_matches(';').trim().to_string()
}

/// `a.b`, `` `a`.`b` ``, `'a'`: the dotted name's parts, unquoted.
fn name_parts(s: &str) -> Option<Vec<String>> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '`' | '\'' | '"' => {
                while let Some(n) = chars.next() {
                    if n == c {
                        if chars.peek() == Some(&c) {
                            cur.push(c);
                            chars.next();
                            continue;
                        }
                        break;
                    }
                    cur.push(n);
                }
            }
            '.' => parts.push(std::mem::take(&mut cur)),
            c if c.is_whitespace() => return None,
            c => cur.push(c),
        }
    }
    parts.push(cur);
    parts.iter().all(|p| !p.is_empty()).then_some(parts)
}

/// A failed statement: `[ERROR_CLASS] message SQLSTATE: 42P01; line 1 pos
/// 14` (or `(line 1, pos 14)`, `== SQL (line 1, position 15) ==`).
pub fn error(message: &str, stmt: &str) -> ScriptError {
    let mut e = ScriptError::new(message);
    if let Some(class) = message.strip_prefix('[').and_then(|r| r.split_once(']')).map(|(c, _)| c) {
        if !class.is_empty() && class.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '.') {
            e = e.with_code(class);
        }
    }
    if let Some(at) = message.find("SQLSTATE: ") {
        let s: String = message[at + 10..].chars().take_while(char::is_ascii_alphanumeric).collect();
        if s.len() == 5 {
            e = e.with_sqlstate(s);
        }
    }
    if let Some((line, col)) = position(message) {
        if let Some(off) = offset_of(stmt, line, col) {
            e.line = Some(line);
            e.offset = Some(off);
        }
    }
    e
}

/// (line, 1-based column) of the message's position.
fn position(msg: &str) -> Option<(u32, u32)> {
    let num = |s: &str| -> Option<u32> { s.chars().take_while(char::is_ascii_digit).collect::<String>().parse().ok() };
    let at = msg.find("line ")?;
    let rest = &msg[at + 5..];
    let line = num(rest)?;
    let rest = rest.trim_start_matches(|c: char| c.is_ascii_digit()).trim_start_matches([',', ' ']);
    if let Some(p) = rest.strip_prefix("position ") {
        return Some((line, num(p)?));
    }
    let p = rest.strip_prefix("pos ")?;
    Some((line, num(p)? + 1))
}

/// Byte offset of a 1-based line and column (columns count characters).
fn offset_of(text: &str, line: u32, col: u32) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line.max(1) {
        start += text[start..].find('\n')? + 1;
    }
    let end = text[start..].find('\n').map_or(text.len(), |e| start + e);
    let skip = col.saturating_sub(1) as usize;
    Some(start + text[start..end].char_indices().nth(skip).map_or(end - start, |(i, _)| i))
}

/// An error of one unit moved to the text the driver got.
pub fn shift(e: dbine_driver::Error, unit: &ScriptStatement) -> dbine_driver::Error {
    match e {
        dbine_driver::Error::Statement(mut se) => {
            se.offset = Some(unit.start + se.offset.unwrap_or(0).min(unit.text.len()));
            se.line = Some(unit.line + se.line.unwrap_or(1) - 1);
            dbine_driver::Error::Statement(se)
        }
        other => other,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn use_statements() {
        assert_eq!(use_target("USE CATALOG main"), Some(Use::Catalog("main".into())));
        assert_eq!(use_target("set catalog `my cat`;"), Some(Use::Catalog("my cat".into())));
        assert_eq!(use_target("USE SCHEMA ventas"), Some(Use::Schema("ventas".into())));
        assert_eq!(use_target("use database ventas"), Some(Use::Schema("ventas".into())));
        assert_eq!(use_target("USE main.ventas"), Some(Use::Both("main".into(), "ventas".into())));
        assert_eq!(use_target("USE `a.b`.`c`"), Some(Use::Both("a.b".into(), "c".into())));
        assert_eq!(use_target("USE SCHEMA 'ventas'"), Some(Use::Schema("ventas".into())));
        assert_eq!(use_target("SET spark.sql.ansi.enabled = true"), None);
        assert_eq!(use_target("SELECT 1"), None);
        assert_eq!(use_target("USE"), None);
    }

    #[test]
    fn errors_carry_class_sqlstate_and_position() {
        let m = "[TABLE_OR_VIEW_NOT_FOUND] The table or view `nope` cannot be found. SQLSTATE: 42P01; line 2 pos 16";
        let stmt = "select 1,\n  2 from x join nope";
        let e = error(m, stmt);
        assert_eq!((e.code.as_deref(), e.sqlstate.as_deref(), e.line), (Some("TABLE_OR_VIEW_NOT_FOUND"), Some("42P01"), Some(2)));
        assert_eq!(&stmt[e.offset.unwrap()..], "nope");
        let e = error("[PARSE_SYNTAX_ERROR] Syntax error at or near 'fron'. SQLSTATE: 42601 (line 1, pos 9)", "select 1 fron t");
        assert_eq!((e.line, e.offset), (Some(1), Some(9)));
        let e = error("[X] boom\n== SQL (line 1, position 10) ==", "select 1 fron t");
        assert_eq!(e.offset, Some(9));
        let e = error("la sentencia falló", "x");
        assert_eq!((e.code, e.sqlstate, e.line), (None, None, None));
    }

    #[test]
    fn compound_blocks_stay_whole() {
        let t = |s: &str| units(s).into_iter().map(|u| u.text).collect::<Vec<_>>();
        assert_eq!(t("BEGIN\n  DECLARE x INT DEFAULT 1;\n  SELECT x;\nEND;\nSELECT 2"), vec!["BEGIN\n  DECLARE x INT DEFAULT 1;\n  SELECT x;\nEND", "SELECT 2"]);
        assert_eq!(t("SELECT 'a\\';b'; SELECT 2").len(), 2);
        assert_eq!(t("BEGIN TRANSACTION; SELECT 1; COMMIT").len(), 3);
    }
}
