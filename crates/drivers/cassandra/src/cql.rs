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
    for p in pieces
        .into_iter()
        .map(|p| p.trim().to_string())
        .filter(|p| !p.is_empty())
    {
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
    /// The statement (without its `;`; a batch whole), or the command line:
    /// `script[start..end]`.
    pub text: String,
    /// Byte offsets and 1-based line of `start` in the script.
    pub start: usize,
    pub end: usize,
    pub line: u32,
    /// A cqlsh command (`CONSISTENCY`, `PAGING`…): the client handles it.
    /// It takes the rest of its line; the `;` is optional.
    pub command: bool,
}

/// cqlsh's own commands (the server doesn't know them).
const SHELL_COMMANDS: &[&str] = &[
    "consistency",
    "serial",
    "paging",
    "tracing",
    "expand",
    "show",
    "source",
    "capture",
    "copy",
    "login",
    "exit",
    "quit",
    "clear",
    "cls",
    "help",
];

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
                at = s[at + 1..]
                    .find(q as char)
                    .map_or(b.len(), |n| at + 1 + n + 1);
            }
            b'$' if b.get(at + 1) == Some(&b'$') => {
                at = s[at + 2..].find("$$").map_or(b.len(), |n| at + 2 + n + 2)
            }
            b'-' | b'/' if b[at..].starts_with(b"--") || b[at..].starts_with(b"//") => {
                at = s[at..].find('\n').map_or(b.len(), |n| at + n + 1)
            }
            b'/' if b.get(at + 1) == Some(&b'*') => {
                at = s[at + 2..].find("*/").map_or(b.len(), |n| at + 2 + n + 2)
            }
            b';' => return at,
            _ => at += 1,
        }
    }
    b.len()
}

/// The script's statements and cqlsh commands, in order. As [`split`], a
/// `BEGIN … BATCH` up to `APPLY BATCH` is one statement.
pub fn script(text: &str) -> Vec<Unit> {
    let line = |at: usize| {
        text.as_bytes()[..at]
            .iter()
            .filter(|&&c| c == b'\n')
            .count() as u32
            + 1
    };
    // `text[from..to]` without its trailing blanks.
    let unit = |from: usize, to: usize, command: bool| {
        let t = text[from..to].trim_end();
        Unit {
            text: t.to_string(),
            start: from,
            end: from + t.len(),
            line: line(from),
            command,
        }
    };
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
        // The word the unit starts with (none when it starts with a sign).
        let lead = rest
            .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .unwrap_or(rest.len());
        let first = rest[..lead].to_ascii_lowercase();
        let alone = rest[lead..]
            .chars()
            .next()
            .is_none_or(|c| c.is_whitespace() || c == ';');
        if batch.is_none() && SHELL_COMMANDS.contains(&first.as_str()) && alone {
            let end = rest.find('\n').unwrap_or(rest.len());
            let cmd = rest[..end].trim_end().trim_end_matches(';').trim_end();
            out.push(unit(at, at + cmd.len(), true));
            at += end;
            continue;
        }
        let end = statement_end(text, at);
        let apply = first == "apply"
            && lead > 0
            && first_words(rest, 2).get(1).is_some_and(|w| w == "batch");
        match batch {
            Some(from) if apply => {
                out.push(unit(from, end, false));
                batch = None;
            }
            Some(_) => {}
            None if first == "begin" => batch = Some(at),
            None => out.push(unit(at, end, false)),
        }
        at = (end + 1).min(text.len());
    }
    if let Some(from) = batch {
        // No APPLY BATCH: the server says what's missing.
        out.push(unit(from, text.len(), false));
    }
    out
}

/// [`script`] as the app's units: what `execute` numbers, at the same
/// places, so "run the statement at the cursor", the run lens and named
/// results see the statements cqlsh runs. A cqlsh command is a unit the
/// driver runs (not a client command the app would skip).
pub fn statements(text: &str) -> Vec<dbine_driver::ScriptStatement> {
    script(text)
        .into_iter()
        .map(|u| dbine_driver::ScriptStatement {
            text: u.text,
            start: u.start,
            end: u.end,
            line: u.line,
            kind: dbine_driver::StatementKind::Sql,
            repeat: 1,
            error: None,
        })
        .collect()
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
    "add",
    "allow",
    "alter",
    "and",
    "apply",
    "asc",
    "authorize",
    "batch",
    "begin",
    "by",
    "columnfamily",
    "create",
    "delete",
    "desc",
    "describe",
    "drop",
    "entries",
    "execute",
    "from",
    "full",
    "grant",
    "if",
    "in",
    "index",
    "infinity",
    "insert",
    "into",
    "is",
    "keyspace",
    "limit",
    "materialized",
    "mbean",
    "mbeans",
    "modify",
    "nan",
    "norecursive",
    "not",
    "null",
    "of",
    "on",
    "or",
    "order",
    "primary",
    "rename",
    "replace",
    "revoke",
    "schema",
    "select",
    "set",
    "table",
    "to",
    "token",
    "truncate",
    "unlogged",
    "unset",
    "update",
    "use",
    "using",
    "view",
    "where",
    "with",
];

/// An identifier as CQL needs it: bare when it's a plain lowercase name,
/// double-quoted otherwise (case and special characters survive).
pub fn ident(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_lowercase())
        && name
            .chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '_')
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
            let end = rest[open..]
                .find(close)
                .map_or(rest.len(), |i| open + i + close.len());
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
        let t: Vec<(&str, u32, bool)> = u
            .iter()
            .map(|u| (u.text.as_str(), u.line, u.command))
            .collect();
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
            assert_eq!(s[x.start..x.end], x.text, "{x:?}");
        }
        // A column called `paging` in a statement isn't a command.
        assert_eq!(script("SELECT paging FROM t").len(), 1);
        assert!(!script("SELECT paging FROM t")[0].command);
        assert!(script("PAGING")[0].command);
        assert!(script("-- only\n/* comments */").is_empty());
        // A sign before a word isn't a word: no panic, one statement.
        assert_eq!(script("€x;").len(), 1);
        assert_eq!(script("(paging);")[0].text, "(paging)");
    }

    /// (text, start, end, line) of each unit the app gets.
    fn placed(s: &str) -> Vec<(String, usize, usize, u32)> {
        let units = statements(s);
        for u in &units {
            assert_eq!(s[u.start..u.end], u.text, "{u:?}");
            assert_eq!(u.kind, dbine_driver::StatementKind::Sql);
        }
        units
            .into_iter()
            .map(|u| (u.text, u.start, u.end, u.line))
            .collect()
    }

    #[test]
    fn app_units_match_what_execute_numbers() {
        // The audit's scripts.
        assert_eq!(placed("SELECT * FROM a;\nSELECT * FROM b;").len(), 2);
        assert_eq!(
            placed("PAGING OFF\nSELECT * FROM t;"),
            [
                ("PAGING OFF".into(), 0, 10, 1),
                ("SELECT * FROM t".into(), 11, 26, 2)
            ]
        );
        assert_eq!(
            placed("CONSISTENCY ONE\nTRACING ON"),
            [
                ("CONSISTENCY ONE".into(), 0, 15, 1),
                ("TRACING ON".into(), 16, 26, 2)
            ]
        );
        let b = "BEGIN BATCH\nINSERT INTO t (a) VALUES (1);\nINSERT INTO t (a) VALUES (2);\nAPPLY BATCH;\nSELECT * FROM t;";
        let u = placed(b);
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].1, u[0].3), (0, 1));
        assert!(u[0].0.ends_with("APPLY BATCH"));
        assert_eq!((u[1].0.as_str(), u[1].3), ("SELECT * FROM t", 5));
        // Three statements, a multi-line one, separators in quotes and
        // bodies, comments and blank lines before a statement (not part
        // of it), every cqlsh command word.
        let s = "-- head\n\nSELECT a,\n  b FROM t WHERE a = 'x;y';\n\n/* c; */ INSERT INTO t (a) VALUES ($$p;q$$);\n// e;\nSELECT \"c;d\" FROM t;";
        let u = placed(s);
        assert_eq!(
            u.iter().map(|x| x.3).collect::<Vec<_>>(),
            [3, 6, 8],
            "{u:?}"
        );
        assert_eq!(u[0].0, "SELECT a,\n  b FROM t WHERE a = 'x;y'");
        assert_eq!(u[1].0, "INSERT INTO t (a) VALUES ($$p;q$$)");
        for cmd in SHELL_COMMANDS {
            let u = placed(&format!("{cmd} x\nSELECT 1 FROM t;"));
            assert_eq!(u.len(), 2, "{cmd}");
            assert_eq!(u[0].0, format!("{cmd} x"));
        }
        // CRLF: the line ends before its `\r`.
        let u = placed("SERIAL CONSISTENCY SERIAL;\r\nSELECT 1 FROM t;\r\nEXPAND ON\r\n");
        assert_eq!(
            u,
            [
                ("SERIAL CONSISTENCY SERIAL".into(), 0, 25, 1),
                ("SELECT 1 FROM t".into(), 28, 43, 2),
                ("EXPAND ON".into(), 46, 55, 3)
            ]
        );
        // Without `;`, two statements are one, as in cqlsh.
        assert_eq!(placed("SELECT 1 FROM t\nSELECT 2 FROM t").len(), 1);
        // Inside an open batch a command word is CQL: without APPLY BATCH
        // the batch runs to the end (the server says what's missing).
        assert_eq!(
            placed("BEGIN UNLOGGED BATCH\nUPDATE t SET v = 1 WHERE a = 1;\nPAGING OFF\nSELECT 1 FROM t;").len(),
            1
        );
    }

    #[test]
    fn keyspace_qualifiers_come_off() {
        let d = "CREATE MATERIALIZED VIEW ks.v AS\n    SELECT * FROM ks.t WHERE a = 'ks.x' AND b IS NOT NULL\n    PRIMARY KEY (b, a);";
        assert_eq!(unqualify(d, "ks"), "CREATE MATERIALIZED VIEW v AS\n    SELECT * FROM t WHERE a = 'ks.x' AND b IS NOT NULL\n    PRIMARY KEY (b, a);");
        assert_eq!(
            unqualify(
                "CREATE TYPE \"Ks\".addr (x frozen<\"Ks\".p>, y myks.t)",
                "Ks"
            ),
            "CREATE TYPE addr (x frozen<p>, y myks.t)"
        );
        assert_eq!(
            unqualify("CREATE FUNCTION ks.f() AS $$ return ks.x; $$;", "ks"),
            "CREATE FUNCTION f() AS $$ return ks.x; $$;"
        );
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
        assert_eq!(
            first_write(&split(
                "select * from t; use ks; describe tables; list roles"
            )),
            None
        );
        assert_eq!(
            first_write(&split("select 1 from t; insert into t (a) values (1)")).as_deref(),
            Some("INSERT")
        );
        assert_eq!(
            first_write(&split("/* x */ TRUNCATE t")).as_deref(),
            Some("TRUNCATE")
        );
        assert_eq!(
            first_write(&split(
                "begin batch insert into t (a) values (1); apply batch"
            ))
            .as_deref(),
            Some("BEGIN")
        );
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
