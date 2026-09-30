//! Splits a script the way SQL*Plus / SQL Developer do:
//!
//! - plain SQL ends at `;` (outside quotes and comments), which is dropped;
//! - PL/SQL (`BEGIN`, `DECLARE`, `CREATE [OR REPLACE] PROCEDURE | FUNCTION |
//!   PACKAGE | TRIGGER | TYPE…`) keeps its `;`s and ends at a line holding
//!   only `/`, or at the end of the script;
//! - `EXEC proc(…)` becomes `BEGIN proc(…); END;`;
//! - SQL*Plus-only settings (`SET SERVEROUTPUT`, `PROMPT`, `SPOOL`…) are
//!   skipped.

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Statement {
    pub text: String,
    pub plsql: bool,
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

/// SQL*Plus commands that mean nothing to the server.
const SQLPLUS_ONLY: &[&str] = &[
    "SET SERVEROUTPUT",
    "SET ECHO",
    "SET FEEDBACK",
    "SET DEFINE",
    "SET VERIFY",
    "SET LINESIZE",
    "SET PAGESIZE",
    "SET TERMOUT",
    "SET HEADING",
    "SET TIMING",
    "SET SQLBLANKLINES",
    "PROMPT",
    "SPOOL",
    "WHENEVER",
    "SHOW ERRORS",
    "SHO ERR",
];

pub fn split(sql: &str) -> Vec<Statement> {
    let mut sp = Splitter::default();
    for line in sql.split_inclusive('\n') {
        sp.line(line);
    }
    sp.flush();
    sp.out
}

#[derive(Default)]
struct Splitter {
    out: Vec<Statement>,
    cur: String,
    state: Option<State>,
    /// Decided at the statement's first `;`.
    plsql: Option<bool>,
}

impl Splitter {
    fn state(&self) -> State {
        self.state.unwrap_or(State::Code)
    }

    fn line(&mut self, line: &str) {
        let trimmed = line.trim();
        if self.state() == State::Code {
            if trimmed == "/" {
                self.flush();
                return;
            }
            if strip_leading_comments(&self.cur).is_empty() {
                let upper = trimmed.to_ascii_uppercase();
                if SQLPLUS_ONLY.iter().any(|c| upper == *c || upper.starts_with(&format!("{c} "))) {
                    return;
                }
                if let Some(call) = strip_word(trimmed, "EXEC").or_else(|| strip_word(trimmed, "EXECUTE")) {
                    let call = call.trim().trim_end_matches(';').trim();
                    if !call.is_empty() && !call.to_ascii_uppercase().starts_with("IMMEDIATE") {
                        self.cur.clear();
                        self.out.push(Statement { text: format!("BEGIN {call}; END;"), plsql: true });
                        return;
                    }
                }
            }
        }
        let chars: Vec<char> = line.chars().collect();
        let mut i = 0;
        while i < chars.len() {
            let c = chars[i];
            let next = chars.get(i + 1).copied();
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
                        self.cur.extend(&chars[i..]);
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

    fn flush(&mut self) {
        let cur = std::mem::take(&mut self.cur);
        let plsql = self.plsql.take().unwrap_or_else(|| is_plsql(&cur));
        self.state = None;
        let text = strip_leading_comments(&cur).trim_end();
        if !text.is_empty() {
            self.out.push(Statement { text: text.to_string(), plsql });
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
        assert_eq!(s.len(), 2);
        assert_eq!(s[0].text, "BEGIN dbms_output.put_line('hi'); END;");
        assert!(s[0].plsql);
        assert_eq!(s[1].text, "select 1 from dual");
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
