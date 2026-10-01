//! Splits a Firebird script into statements, the way isql does plus what
//! GUI tools add:
//!
//! - `SET TERM <t> <current>` changes the terminator (for PSQL scripts);
//! - without SET TERM, a PSQL unit (`CREATE [OR ALTER] PROCEDURE | FUNCTION |
//!   TRIGGER | PACKAGE … AS …`, `RECREATE …`, `EXECUTE BLOCK`) runs to the
//!   `;` that closes its outermost `BEGIN … END`;
//! - isql-only settings (`SET AUTODDL`, `SET NAMES`, `SHOW …`) are skipped.

/// isql commands that mean nothing to the server.
const ISQL_ONLY: &[&str] = &[
    "SET AUTODDL",
    "SET SQL DIALECT",
    "SET NAMES",
    "SET LIST",
    "SET STATS",
    "SET PLAN",
    "SET PLANONLY",
    "SET COUNT",
    "SET ECHO",
    "SET HEADING",
    "SET BAIL",
    "SET WARNINGS",
    "SET WNG",
    "SET BLOB",
    "SET BLOBDISPLAY",
    "SET WIDTH",
    "SET ROWCOUNT",
    "SET MAXROWS",
    "SET PER_TABLE_STATS",
    "SET EXPLAIN",
    "SET KEEP_TRAN_PARAMS",
    "SET SQLDA_DISPLAY",
    "SET TIME",
    "SHOW",
    "INPUT",
    "OUTPUT",
    "QUIT",
    "EXIT",
];

#[derive(Clone, Copy, PartialEq, Eq)]
enum State {
    Code,
    Single,
    Double,
    Line,
    Block,
}

pub fn split(sql: &str) -> Vec<String> {
    pieces(sql).into_iter().filter(|p| !p.skipped).map(|p| p.text).collect()
}

/// One statement of a script, or an isql command it skips.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Piece {
    pub text: String,
    /// Byte offset of `text` in the script.
    pub start: usize,
    /// An isql-only command (`SHOW`, `SET AUTODDL`…): not sent.
    pub skipped: bool,
}

/// The statements of a script with their positions, isql-only commands
/// included (marked `skipped`).
pub fn pieces(sql: &str) -> Vec<Piece> {
    let chars: Vec<char> = sql.chars().collect();
    // Byte offset of each char (and of the end).
    let bytes: Vec<usize> = sql.char_indices().map(|(b, _)| b).chain(std::iter::once(sql.len())).collect();
    let mut seg = 0usize;
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut term: Vec<char> = vec![';'];
    let mut state = State::Code;
    // PSQL tracking for the default terminator.
    let mut word = String::new();
    let mut psql = Psql::default();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match state {
            State::Line => {
                cur.push(c);
                if c == '\n' {
                    state = State::Code;
                }
            }
            State::Block => {
                cur.push(c);
                if c == '*' && next == Some('/') {
                    cur.push('/');
                    i += 1;
                    state = State::Code;
                }
            }
            State::Single | State::Double => {
                cur.push(c);
                let q = if state == State::Single { '\'' } else { '"' };
                if c == q {
                    if next == Some(q) {
                        cur.push(q);
                        i += 1;
                    } else {
                        state = State::Code;
                    }
                }
            }
            State::Code => {
                if c.is_alphanumeric() || c == '_' || c == '$' {
                    word.push(c);
                } else if !word.is_empty() {
                    psql.word(&std::mem::take(&mut word));
                }
                if chars[i..].starts_with(&term) && (term != [';'] || psql.may_end()) {
                    let stmt = std::mem::take(&mut cur);
                    let at = bytes[seg];
                    i += term.len();
                    seg = i;
                    psql = Psql::default();
                    word.clear();
                    if let Some(t) = set_term(&stmt) {
                        term = t.chars().collect();
                    } else {
                        push(&mut out, &stmt, at);
                    }
                    continue;
                }
                match c {
                    '-' if next == Some('-') => state = State::Line,
                    '/' if next == Some('*') => {
                        cur.push('/');
                        cur.push('*');
                        i += 2;
                        state = State::Block;
                        continue;
                    }
                    '\'' => state = State::Single,
                    '"' => state = State::Double,
                    _ => {}
                }
                cur.push(c);
            }
        }
        i += 1;
    }
    if !word.is_empty() {
        psql.word(&word);
    }
    if set_term(&cur).is_none() {
        push(&mut out, &cur, bytes[seg.min(chars.len())]);
    }
    out
}

/// `stmt` (found at byte `at`) without its leading comments and spaces.
fn push(out: &mut Vec<Piece>, stmt: &str, at: usize) {
    let lead = strip_leading_comments(stmt);
    let text = lead.trim_end();
    if text.is_empty() {
        return;
    }
    let upper = text.to_ascii_uppercase();
    let skipped = ISQL_ONLY.iter().any(|c| upper == *c || upper.starts_with(&format!("{c} ")));
    out.push(Piece { text: text.to_string(), start: at + (stmt.len() - lead.len()), skipped });
}

/// The new terminator when `stmt` is `SET TERM <t>`.
fn set_term(stmt: &str) -> Option<String> {
    let s = strip_leading_comments(stmt);
    let mut parts = s.split_whitespace();
    let (a, b, t) = (parts.next()?, parts.next()?, parts.next()?);
    (a.eq_ignore_ascii_case("SET") && b.eq_ignore_ascii_case("TERM") && parts.next().is_none()).then(|| t.to_string())
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

/// Where a statement is in a PSQL unit, from its words.
#[derive(Default)]
struct Psql {
    /// First words, to recognise the unit.
    head: Vec<String>,
    /// Recognised as PSQL (`CREATE PROCEDURE … AS`, `EXECUTE BLOCK`…).
    unit: bool,
    /// Saw `AS` after a PSQL head: the body (declarations, BEGIN) follows.
    body: bool,
    seen_begin: bool,
    depth: i32,
}

impl Psql {
    fn word(&mut self, w: &str) {
        let w = w.to_ascii_uppercase();
        if self.head.len() < 5 {
            self.head.push(w.clone());
            if !self.unit {
                self.unit = is_psql_head(&self.head);
            }
        }
        if !self.unit {
            return;
        }
        match w.as_str() {
            "AS" if !self.body => self.body = true,
            "BEGIN" if self.body || self.head.first().is_some_and(|h| h == "EXECUTE") => {
                self.body = true;
                self.seen_begin = true;
                self.depth += 1;
            }
            "CASE" if self.seen_begin => self.depth += 1,
            "END" if self.seen_begin => self.depth -= 1,
            _ => {}
        }
    }

    /// A `;` here ends the statement.
    fn may_end(&self) -> bool {
        if !self.unit || !self.body {
            return true;
        }
        self.seen_begin && self.depth <= 0
    }
}

fn is_psql_head(head: &[String]) -> bool {
    let h: Vec<&str> = head.iter().map(String::as_str).collect();
    let object = |w: &str| matches!(w, "PROCEDURE" | "FUNCTION" | "TRIGGER" | "PACKAGE");
    match h.as_slice() {
        ["EXECUTE", "BLOCK", ..] => true,
        ["CREATE", "OR", "ALTER", o, ..] | ["CREATE", o, ..] | ["RECREATE", o, ..] | ["ALTER", o, ..] => object(o),
        _ => false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plain_statements() {
        assert_eq!(split("select 1 from rdb$database; select ';' from rdb$database;"), vec![
            "select 1 from rdb$database",
            "select ';' from rdb$database"
        ]);
        assert_eq!(split("-- c;\nselect 1 from rdb$database /* ; */"), vec!["select 1 from rdb$database /* ; */"]);
    }

    #[test]
    fn set_term_blocks() {
        let sql = "SET TERM ^ ;\nCREATE PROCEDURE p AS\nBEGIN\n  x = 1;\nEND^\nSET TERM ; ^\nSELECT 1 FROM rdb$database;";
        assert_eq!(split(sql), vec!["CREATE PROCEDURE p AS\nBEGIN\n  x = 1;\nEND", "SELECT 1 FROM rdb$database"]);
    }

    #[test]
    fn psql_without_set_term() {
        let sql = "create or alter procedure p (a int) returns (b int) as\n\
                   declare variable v int;\n\
                   begin\n  v = case when a > 0 then 1 else 0 end;\n  if (v = 1) then begin b = 1; end\n  suspend;\nend;\n\
                   select 1 from rdb$database;\n\
                   execute block as begin end;\n\
                   alter trigger t inactive;\n\
                   create function f (x int) returns int external name 'm!f' engine udr;\n\
                   commit;";
        let s = split(sql);
        assert_eq!(s.len(), 6, "{s:#?}");
        assert!(s[0].ends_with("suspend;\nend"));
        assert_eq!(s[1], "select 1 from rdb$database");
        assert_eq!(s[2], "execute block as begin end");
        assert_eq!(s[3], "alter trigger t inactive");
        assert!(s[4].starts_with("create function f"));
        assert_eq!(s[5], "commit");
    }

    #[test]
    fn packages_nest() {
        let sql = "create package body pk as begin\n procedure a as begin end\n function b returns int as begin return 1; end\nend;\nselect 2 from rdb$database";
        let s = split(sql);
        assert_eq!(s.len(), 2, "{s:#?}");
        assert!(s[0].ends_with("end\nend"));
    }

    #[test]
    fn pieces_know_where_they_are() {
        let sql = "-- head\nselect 1 from rdb$database;\n  SHOW TABLES;\nSET TERM ^ ;\nexecute block as begin end^";
        let p = pieces(sql);
        assert_eq!(p.len(), 3, "{p:#?}");
        for x in &p {
            assert_eq!(&sql[x.start..x.start + x.text.len()], x.text);
        }
        assert!(!p[0].skipped && p[1].skipped && !p[2].skipped);
        let s = "select 'ñ;' from rdb$database; select 2 from rdb$database";
        let p = pieces(s);
        assert_eq!(&s[p[1].start..], "select 2 from rdb$database");
    }

    #[test]
    fn isql_commands_are_skipped() {
        assert_eq!(split("SET AUTODDL ON;\nSET NAMES UTF8;\nSET GENERATOR g TO 5;\nshow tables;"), vec![
            "SET GENERATOR g TO 5"
        ]);
    }
}

/// `(name, args)` of `EXECUTE PROCEDURE name args`, name as written.
pub fn execute_procedure(sql: &str) -> Option<(String, String)> {
    let s = strip_leading_comments(sql);
    let mut words = s.splitn(3, char::is_whitespace);
    let (a, b) = (words.next()?, words.next()?);
    if !a.eq_ignore_ascii_case("EXECUTE") || !b.eq_ignore_ascii_case("PROCEDURE") {
        return None;
    }
    let rest = words.next()?.trim_start();
    let mut end = 0;
    let chars: Vec<(usize, char)> = rest.char_indices().collect();
    let mut i = 0;
    while i < chars.len() {
        let (pos, c) = chars[i];
        if c == '"' {
            i += 1;
            while i < chars.len() {
                if chars[i].1 == '"' {
                    if chars.get(i + 1).is_some_and(|n| n.1 == '"') {
                        i += 1;
                    } else {
                        break;
                    }
                }
                i += 1;
            }
            end = chars.get(i).map_or(rest.len(), |(p, _)| p + 1);
        } else if c.is_alphanumeric() || c == '_' || c == '$' || c == '.' {
            end = pos + c.len_utf8();
        } else {
            break;
        }
        i += 1;
    }
    (end > 0).then(|| (rest[..end].to_string(), rest[end..].trim().to_string()))
}

/// Catalog spelling of each part of a (possibly package-qualified) name:
/// unquoted parts upper-cased, quoted ones as is.
pub fn split_name(name: &str) -> Vec<String> {
    let mut parts = Vec::new();
    let mut cur = String::new();
    let mut quoted = false;
    let mut was_quoted = false;
    let mut chars = name.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '"' if quoted && chars.peek() == Some(&'"') => {
                cur.push('"');
                chars.next();
            }
            '"' => {
                quoted = !quoted;
                was_quoted = true;
            }
            '.' if !quoted => {
                parts.push(if was_quoted { std::mem::take(&mut cur) } else { std::mem::take(&mut cur).to_uppercase() });
                was_quoted = false;
            }
            c => cur.push(c),
        }
    }
    parts.push(if was_quoted { cur } else { cur.to_uppercase() });
    parts
}

#[cfg(test)]
mod name_tests {
    use super::*;

    #[test]
    fn execute_procedure_parts() {
        assert_eq!(execute_procedure("execute procedure p(1, 2)"), Some(("p".into(), "(1, 2)".into())));
        assert_eq!(execute_procedure("EXECUTE PROCEDURE \"My P\" 1"), Some(("\"My P\"".into(), "1".into())));
        assert_eq!(execute_procedure("EXECUTE PROCEDURE pkg.p"), Some(("pkg.p".into(), "".into())));
        assert_eq!(execute_procedure("select 1"), None);
        assert_eq!(split_name("pkg.p"), vec!["PKG", "P"]);
        assert_eq!(split_name("\"My\"\"P\""), vec!["My\"P"]);
    }
}
