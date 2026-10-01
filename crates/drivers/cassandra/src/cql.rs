//! CQL text helpers: splitting a script into statements, the read-only
//! check and identifier quoting.

/// Statements of a script, split on `;` outside '…', "…", `$$…$$` and
/// comments (`--`, `//`, `/* */`). A `BEGIN … BATCH` up to `APPLY BATCH`
/// stays one statement.
pub fn split(script: &str) -> Vec<String> {
    let mut pieces = Vec::new();
    let mut cur = String::new();
    let chars: Vec<char> = script.chars().collect();
    let mut i = 0;
    while i < chars.len() {
        let c = chars[i];
        let next = chars.get(i + 1).copied();
        match c {
            '\'' | '"' => {
                // A doubled quote inside is just two toggles: same result.
                cur.push(c);
                i += 1;
                while i < chars.len() {
                    cur.push(chars[i]);
                    if chars[i] == c {
                        break;
                    }
                    i += 1;
                }
            }
            '$' if next == Some('$') => {
                cur.push_str("$$");
                i += 2;
                while i < chars.len() && !(chars[i] == '$' && chars.get(i + 1) == Some(&'$')) {
                    cur.push(chars[i]);
                    i += 1;
                }
                if i < chars.len() {
                    cur.push_str("$$");
                    i += 1;
                }
            }
            '-' if next == Some('-') => i = skip_line(&chars, i, &mut cur),
            '/' if next == Some('/') => i = skip_line(&chars, i, &mut cur),
            '/' if next == Some('*') => {
                i += 2;
                while i < chars.len() && !(chars[i] == '*' && chars.get(i + 1) == Some(&'/')) {
                    i += 1;
                }
                i += 1;
                cur.push(' ');
            }
            ';' => pieces.push(std::mem::take(&mut cur)),
            _ => cur.push(c),
        }
        i += 1;
    }
    pieces.push(cur);

    let mut out: Vec<String> = Vec::new();
    let mut batch: Option<Vec<String>> = None;
    for p in pieces.into_iter().map(|p| p.trim().to_string()).filter(|p| !p.is_empty()) {
        let words = first_words(&p, 3);
        match &mut batch {
            Some(parts) => {
                let done = words.len() >= 2 && words[0] == "apply" && words[1] == "batch";
                parts.push(p);
                if done {
                    out.push(batch.take().expect("batch").join(";\n"));
                }
            }
            None if words.first().is_some_and(|w| w == "begin") => batch = Some(vec![p]),
            None => out.push(p),
        }
    }
    if let Some(parts) = batch {
        out.push(parts.join(";\n"));
    }
    out
}

/// A unit of an editor script, as cqlsh reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The statement (without its `;`; a batch whole), or the command line.
    pub text: String,
    /// Byte offset and 1-based line in the script.
    pub start: usize,
    pub line: u32,
    /// A cqlsh command (`CONSISTENCY`, `PAGING`…): the client handles it.
    /// It takes the rest of its line; the `;` is optional.
    pub command: bool,
}

/// cqlsh's own commands (the server doesn't know them).
const SHELL_COMMANDS: &[&str] =
    &["consistency", "serial", "paging", "tracing", "expand", "show", "source", "capture", "copy", "login", "exit", "quit", "clear", "cls", "help"];

/// Bytes of whitespace and comments (`--`, `//`, `/* */`) from `at`.
fn trivia(s: &str, mut at: usize) -> usize {
    let b = s.as_bytes();
    loop {
        if at < b.len() && b[at].is_ascii_whitespace() {
            at += 1;
        } else if b[at..].starts_with(b"--") || b[at..].starts_with(b"//") {
            at = s[at..].find('\n').map_or(b.len(), |n| at + n + 1);
        } else if b[at..].starts_with(b"/*") {
            at = s[at + 2..].find("*/").map_or(b.len(), |n| at + 2 + n + 2);
        } else {
            return at;
        }
    }
}

/// Where the statement starting at `at` ends: its `;` (outside '…', "…",
/// `$$…$$` and comments) or the end of the script.
fn statement_end(s: &str, mut at: usize) -> usize {
    let b = s.as_bytes();
    while at < b.len() {
        match b[at] {
            q @ (b'\'' | b'"') => {
                // A doubled quote is just two toggles: same result.
                at = s[at + 1..].find(q as char).map_or(b.len(), |n| at + 1 + n + 1);
            }
            b'$' if b.get(at + 1) == Some(&b'$') => at = s[at + 2..].find("$$").map_or(b.len(), |n| at + 2 + n + 2),
            b'-' | b'/' if b[at..].starts_with(b"--") || b[at..].starts_with(b"//") => {
                at = s[at..].find('\n').map_or(b.len(), |n| at + n + 1)
            }
            b'/' if b.get(at + 1) == Some(&b'*') => at = s[at + 2..].find("*/").map_or(b.len(), |n| at + 2 + n + 2),
            b';' => return at,
            _ => at += 1,
        }
    }
    b.len()
}

/// The script's statements and cqlsh commands, in order. As [`split`], a
/// `BEGIN … BATCH` up to `APPLY BATCH` is one statement.
pub fn script(text: &str) -> Vec<Unit> {
    let line = |at: usize| text.as_bytes()[..at].iter().filter(|&&c| c == b'\n').count() as u32 + 1;
    let mut out: Vec<Unit> = Vec::new();
    // The open batch: its start in `text`.
    let mut batch: Option<usize> = None;
    let mut at = 0;
    loop {
        at = trivia(text, at);
        if at >= text.len() {
            break;
        }
        let rest = &text[at..];
        let words = first_words(rest, 2);
        let first = words.first().map(String::as_str).unwrap_or("");
        let alone = rest.len() == first.len() || rest[first.len()..].starts_with(|c: char| c.is_whitespace() || c == ';');
        if batch.is_none() && SHELL_COMMANDS.contains(&first) && alone {
            let end = rest.find('\n').unwrap_or(rest.len());
            let cmd = rest[..end].trim_end().trim_end_matches(';').trim_end();
            out.push(Unit { text: cmd.to_string(), start: at, line: line(at), command: true });
            at += end;
            continue;
        }
        let end = statement_end(text, at);
        match batch {
            Some(from) if words.len() >= 2 && first == "apply" && words[1] == "batch" => {
                out.push(Unit { text: text[from..end].trim_end().to_string(), start: from, line: line(from), command: false });
                batch = None;
            }
            Some(_) => {}
            None if first == "begin" => batch = Some(at),
            None => out.push(Unit { text: text[at..end].trim_end().to_string(), start: at, line: line(at), command: false }),
        }
        at = (end + 1).min(text.len());
    }
    if let Some(from) = batch {
        // No APPLY BATCH: the server says what's missing.
        out.push(Unit { text: text[from..].trim_end().to_string(), start: from, line: line(from), command: false });
    }
    out
}

/// Up to the end of the line (kept, so line numbers in errors still match).
fn skip_line(chars: &[char], mut i: usize, cur: &mut String) -> usize {
    while i < chars.len() && chars[i] != '\n' {
        i += 1;
    }
    if i < chars.len() {
        cur.push('\n');
    }
    i
}

fn first_words(stmt: &str, n: usize) -> Vec<String> {
    stmt.split(|c: char| !c.is_ascii_alphanumeric() && c != '_')
        .filter(|w| !w.is_empty())
        .take(n)
        .map(str::to_ascii_lowercase)
        .collect()
}

/// Statement kinds a read-only connection lets through.
const READS: &[&str] = &["select", "use", "describe", "desc", "list"];

/// The first keyword of the first statement that isn't a read.
pub fn first_write(statements: &[String]) -> Option<String> {
    statements.iter().find_map(|s| {
        let kw = first_words(s, 1).pop()?;
        (!READS.contains(&kw.as_str())).then(|| kw.to_uppercase())
    })
}

/// Words CQL won't take as a bare identifier.
const RESERVED: &[&str] = &[
    "add", "allow", "alter", "and", "apply", "asc", "authorize", "batch", "begin", "by", "columnfamily", "create",
    "delete", "desc", "describe", "drop", "entries", "execute", "from", "full", "grant", "if", "in", "index",
    "infinity", "insert", "into", "is", "keyspace", "limit", "materialized", "mbean", "mbeans", "modify", "nan",
    "norecursive", "not", "null", "of", "on", "or", "order", "primary", "rename", "replace", "revoke", "schema",
    "select", "set", "table", "to", "token", "truncate", "unlogged", "unset", "update", "use", "using", "view",
    "where", "with",
];

/// An identifier as CQL needs it: bare when it's a plain lowercase name,
/// double-quoted otherwise (case and special characters survive).
pub fn ident(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name.chars().all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
        && !RESERVED.contains(&name);
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

pub fn qualified(keyspace: Option<&str>, name: &str) -> String {
    match keyspace {
        Some(k) if !k.is_empty() => format!("{}.{}", ident(k), ident(name)),
        _ => ident(name),
    }
}

/// `text` with the `keyspace.` qualifier taken off the names (outside
/// '…' literals and `$$…$$` bodies), so it runs in whatever keyspace the
/// session uses.
pub fn unqualify(text: &str, keyspace: &str) -> String {
    let quoted = format!("\"{}\".", keyspace.replace('"', "\"\""));
    let plain = format!("{keyspace}.");
    let prefixes = [quoted.as_str(), plain.as_str()];
    let word = |c: char| c.is_alphanumeric() || c == '_' || c == '"';
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    let mut prev: Option<char> = None;
    while let Some(c) = rest.chars().next() {
        if c == '\'' || rest.starts_with("$$") {
            let close = if c == '\'' { "'" } else { "$$" };
            let open = close.len();
            let end = rest[open..].find(close).map_or(rest.len(), |i| open + i + close.len());
            out.push_str(&rest[..end]);
            prev = rest[..end].chars().last();
            rest = &rest[end..];
            continue;
        }
        if !prev.is_some_and(word) {
            if let Some(p) = prefixes.iter().find(|p| rest.starts_with(**p)) {
                rest = &rest[p.len()..];
                prev = Some('.');
                continue;
            }
        }
        out.push(c);
        prev = Some(c);
        rest = &rest[c.len_utf8()..];
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn script_units_batches_and_shell_commands() {
        let s = "CONSISTENCY QUORUM\nSELECT * FROM t WHERE a = 'x;y'; -- c;\nBEGIN BATCH\n  INSERT INTO t (a) VALUES ('é');\n  INSERT INTO t (a) VALUES ($$b;c$$);\nAPPLY BATCH;\npaging off;\n// d; e\nSELECT 1 FROM t";
        let u = script(s);
        let t: Vec<(&str, u32, bool)> = u.iter().map(|u| (u.text.as_str(), u.line, u.command)).collect();
        assert_eq!(
            t,
            [
                ("CONSISTENCY QUORUM", 1, true),
                ("SELECT * FROM t WHERE a = 'x;y'", 2, false),
                ("BEGIN BATCH\n  INSERT INTO t (a) VALUES ('é');\n  INSERT INTO t (a) VALUES ($$b;c$$);\nAPPLY BATCH", 3, false),
                ("paging off", 7, true),
                ("SELECT 1 FROM t", 9, false),
            ]
        );
        for x in &u {
            assert!(s[x.start..].starts_with(&x.text), "{x:?}");
        }
        // A column called `paging` in a statement isn't a command.
        assert_eq!(script("SELECT paging FROM t").len(), 1);
        assert!(script("SELECT paging FROM t")[0].command == false);
        assert!(script("PAGING")[0].command);
        assert!(script("-- only\n/* comments */").is_empty());
    }

    #[test]
    fn keyspace_qualifiers_come_off() {
        let d = "CREATE MATERIALIZED VIEW ks.v AS\n    SELECT * FROM ks.t WHERE a = 'ks.x' AND b IS NOT NULL\n    PRIMARY KEY (b, a);";
        assert_eq!(unqualify(d, "ks"), "CREATE MATERIALIZED VIEW v AS\n    SELECT * FROM t WHERE a = 'ks.x' AND b IS NOT NULL\n    PRIMARY KEY (b, a);");
        assert_eq!(unqualify("CREATE TYPE \"Ks\".addr (x frozen<\"Ks\".p>, y myks.t)", "Ks"), "CREATE TYPE addr (x frozen<p>, y myks.t)");
        assert_eq!(unqualify("CREATE FUNCTION ks.f() AS $$ return ks.x; $$;", "ks"), "CREATE FUNCTION f() AS $$ return ks.x; $$;");
    }

    #[test]
    fn splits_outside_quotes_and_comments() {
        let s = split("select ';' from t; -- a;b\n// c;d\nselect \"x;y\" from t /* ; */ ;;");
        assert_eq!(s, vec!["select ';' from t", "select \"x;y\" from t"]);
    }

    #[test]
    fn doubled_quotes_and_dollar_bodies() {
        let s = split("insert into t (a) values ('it''s; ok'); create function f() returns int language java as $$ return 1; $$;");
        assert_eq!(s.len(), 2);
        assert!(s[0].ends_with("'it''s; ok')"));
        assert!(s[1].contains("$$ return 1; $$"));
    }

    #[test]
    fn a_batch_stays_whole() {
        let s = split(
            "BEGIN BATCH\n INSERT INTO t (a) VALUES (1);\n INSERT INTO t (a) VALUES (2);\nAPPLY BATCH;\nSELECT * FROM t;",
        );
        assert_eq!(s.len(), 2);
        assert!(s[0].starts_with("BEGIN BATCH") && s[0].ends_with("APPLY BATCH"));
        assert_eq!(s[1], "SELECT * FROM t");
    }

    #[test]
    fn read_only_check() {
        assert_eq!(first_write(&split("select * from t; use ks; describe tables; list roles")), None);
        assert_eq!(first_write(&split("select 1 from t; insert into t (a) values (1)")).as_deref(), Some("INSERT"));
        assert_eq!(first_write(&split("/* x */ TRUNCATE t")).as_deref(), Some("TRUNCATE"));
        assert_eq!(first_write(&split("begin batch insert into t (a) values (1); apply batch")).as_deref(), Some("BEGIN"));
    }

    #[test]
    fn identifiers() {
        assert_eq!(ident("users"), "users");
        assert_eq!(ident("Users"), "\"Users\"");
        assert_eq!(ident("select"), "\"select\"");
        assert_eq!(ident("a\"b"), "\"a\"\"b\"");
        assert_eq!(qualified(Some("ks"), "t 1"), "ks.\"t 1\"");
    }
}
