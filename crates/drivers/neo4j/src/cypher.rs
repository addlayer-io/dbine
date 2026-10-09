//! Cypher text helpers: splitting a script into statements, spotting
//! writes (read-only connections), quoting identifiers and writing literals.

use serde_json::Value;

/// Walks the tokens of a statement, skipping comments and whitespace:
/// `on(start, end, kind)` in char offsets, kind `w` (word), `s` (string),
/// `q` (quoted name) or `p` (punctuation). Returning `false` stops.
fn scan(text: &str, mut on: impl FnMut(usize, usize, char) -> bool) {
    let b: Vec<char> = text.chars().collect();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        let start = i;
        if c == '/' && b.get(i + 1) == Some(&'/') {
            while i < b.len() && b[i] != '\n' {
                i += 1;
            }
            continue;
        }
        if c == '/' && b.get(i + 1) == Some(&'*') {
            i += 2;
            while i < b.len() && !(b[i] == '*' && b.get(i + 1) == Some(&'/')) {
                i += 1;
            }
            i = (i + 2).min(b.len());
            continue;
        }
        let kind = if c == '\'' || c == '"' {
            i += 1;
            while i < b.len() && b[i] != c {
                if b[i] == '\\' {
                    i += 1;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            's'
        } else if c == '`' {
            i += 1;
            while i < b.len() {
                if b[i] == '`' {
                    if b.get(i + 1) == Some(&'`') {
                        i += 2;
                        continue;
                    }
                    break;
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            'q'
        } else if c.is_alphanumeric() || c == '_' || c == '$' {
            while i < b.len() && (b[i].is_alphanumeric() || b[i] == '_' || b[i] == '$' || b[i] == '.') {
                i += 1;
            }
            'w'
        } else {
            i += 1;
            if c.is_whitespace() {
                continue;
            }
            'p'
        };
        if !on(start, i, kind) {
            return;
        }
    }
}

/// Statements of a script, split on `;` outside strings and comments.
/// Blank statements (only comments) are dropped.
pub fn split(text: &str) -> Vec<String> {
    let chars: Vec<char> = text.chars().collect();
    let mut out = Vec::new();
    let mut from = 0;
    let mut has_code = false;
    scan(text, |s, e, kind| {
        if kind == 'p' && chars[s] == ';' {
            if has_code {
                out.push(chars[from..s].iter().collect::<String>().trim().to_string());
            }
            from = e;
            has_code = false;
        } else {
            has_code = true;
        }
        true
    });
    if has_code {
        out.push(chars[from..].iter().collect::<String>().trim().to_string());
    }
    out
}

/// A unit of an editor script, as cypher-shell reads it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Unit {
    /// The statement (without its `;`), or the whole command line.
    pub text: String,
    /// Byte offset and 1-based line in the script.
    pub start: usize,
    pub line: u32,
    /// A client command (`:use`, `:begin`, `:param`…): a line starting
    /// with `:` where a statement would start. It takes its line and needs
    /// no `;`.
    pub command: bool,
}

/// Bytes of whitespace and comments at the start of `s`.
fn trivia(s: &str) -> usize {
    let b = s.as_bytes();
    let mut i = 0;
    loop {
        if i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        } else if b[i..].starts_with(b"//") {
            i = s[i..].find('\n').map_or(b.len(), |n| i + n + 1);
        } else if b[i..].starts_with(b"/*") {
            i = s[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
        } else {
            return i;
        }
    }
}

/// The script's statements and client commands, in order.
pub fn script(text: &str) -> Vec<Unit> {
    let mut out = Vec::new();
    let mut at = 0;
    loop {
        at += trivia(&text[at..]);
        if at >= text.len() {
            return out;
        }
        let line = text.as_bytes()[..at].iter().filter(|&&c| c == b'\n').count() as u32 + 1;
        let rest = &text[at..];
        if rest.starts_with(':') {
            let end = rest.find('\n').unwrap_or(rest.len());
            let cmd = rest[..end].trim_end().trim_end_matches(';').trim_end();
            out.push(Unit { text: cmd.to_string(), start: at, line, command: true });
            at += end;
            continue;
        }
        let bytes: Vec<usize> = rest.char_indices().map(|(i, _)| i).collect();
        let mut semi = None;
        scan(rest, |s, _, kind| {
            if kind == 'p' && rest[bytes[s]..].starts_with(';') {
                semi = Some(bytes[s]);
                return false;
            }
            true
        });
        let end = semi.unwrap_or(rest.len());
        let stmt = rest[..end].trim_end();
        if !stmt.is_empty() {
            out.push(Unit { text: stmt.to_string(), start: at, line, command: false });
        }
        at += semi.map_or(rest.len(), |s| s + 1);
    }
}

/// A statement Neo4j only runs in an implicit (auto-commit) transaction:
/// `CALL { … } IN TRANSACTIONS`, `LOAD CSV … PERIODIC COMMIT`, and the
/// administration commands (databases, users, roles, privileges, `SHOW`).
pub fn implicit_only(stmt: &str) -> bool {
    let w = words(stmt);
    let has = |a: &str, b: &str| w.windows(2).any(|p| p[0] == a && p[1] == b);
    if has("IN", "TRANSACTIONS") || has("PERIODIC", "COMMIT") {
        return true;
    }
    let at = |i: usize| w.get(i).map(String::as_str).unwrap_or("");
    // `CREATE OR REPLACE DATABASE x`: the object is the fourth word.
    let object = if at(1) == "OR" { at(3) } else { at(1) };
    matches!(at(0), "SHOW" | "GRANT" | "DENY" | "REVOKE" | "TERMINATE" | "ENABLE" | "DEALLOCATE" | "REALLOCATE" | "DRYRUN")
        || (matches!(at(0), "CREATE" | "DROP" | "ALTER" | "START" | "STOP" | "RENAME")
            && matches!(object, "DATABASE" | "COMPOSITE" | "ALIAS" | "USER" | "ROLE" | "SERVER"))
}

/// Upper-cased words of a statement (outside strings/comments/quoted names).
pub fn words(stmt: &str) -> Vec<String> {
    let chars: Vec<char> = stmt.chars().collect();
    let mut w = Vec::new();
    scan(stmt, |s, e, kind| {
        if kind == 'w' {
            w.push(chars[s..e].iter().collect::<String>().to_ascii_uppercase());
        }
        true
    });
    w
}

/// Clauses and commands that change data, schema or the server.
const WRITE_WORDS: &[&str] = &[
    "CREATE", "MERGE", "DELETE", "DETACH", "SET", "REMOVE", "DROP", "FOREACH", "LOAD", "GRANT", "REVOKE", "DENY", "ALTER",
    "RENAME", "START", "STOP", "TERMINATE", "ENABLE", "DEALLOCATE", "REALLOCATE", "IMPORT", "INSERT", "FREE", "ANALYZE",
    "CLEAR", "RECOVER", "REGISTER", "UNREGISTER", "DEMOTE", "PROMOTE",
];

/// Procedures a read-only connection may call (prefixes).
const READ_PROCEDURES: &[&str] = &[
    "DB.LABELS", "DB.RELATIONSHIPTYPES", "DB.PROPERTYKEYS", "DB.SCHEMA.", "DB.INDEXES", "DB.CONSTRAINTS", "DB.INFO",
    "DB.PING", "DB.STATS.RETRIEVE", "DB.STATS.STATUS", "DBMS.COMPONENTS", "DBMS.INFO", "DBMS.LISTCONFIG",
    "DBMS.LISTCONNECTIONS", "DBMS.QUERYJMX", "DBMS.SHOWCURRENTUSER", "DBMS.LISTCAPABILITIES", "DBMS.PROCEDURES",
    "DBMS.FUNCTIONS", "DBMS.ROUTING.GETROUTINGTABLE", "DBMS.CLUSTER.ROUTING.GETROUTINGTABLE", "MG.PROCEDURES", "MG.FUNCTIONS",
    "MG.TRANSFORMATIONS", "SCHEMA.NODE_TYPE_PROPERTIES", "SCHEMA.REL_TYPE_PROPERTIES", "APOC.META.", "GDS.LIST",
    "GDS.VERSION", "NEPTUNE.READ",
];

/// Why a statement isn't a read, or `None` when it only reads.
pub fn write_reason(stmt: &str) -> Option<String> {
    let w = words(stmt);
    for (i, word) in w.iter().enumerate() {
        // `SHOW …` and `EXPLAIN` are reads; `CALL {…} IN TRANSACTIONS` only
        // matters when its body writes (caught by the words inside).
        if WRITE_WORDS.contains(&word.as_str()) {
            // `ON CREATE SET` is still a MERGE; any match is a write anyway.
            return Some(word.clone());
        }
        if word == "CALL" {
            // A subquery `CALL {` is followed by a clause, not a dotted name.
            if let Some(p) = w.get(i + 1).filter(|p| p.contains('.')) {
                if !READ_PROCEDURES.iter().any(|r| p.starts_with(r)) {
                    return Some(format!("CALL {}", p.to_ascii_lowercase()));
                }
            }
        }
    }
    None
}

/// Text for a `//` comment line: a server-controlled name can't end the
/// comment and turn the rest of the line into a statement. Line breaks
/// (CR, LF, NEL, U+2028/U+2029) and other control characters become `?`.
pub fn comment_text(s: &str) -> String {
    s.chars().map(|c| if c.is_control() || matches!(c, '\u{2028}' | '\u{2029}') { '?' } else { c }).collect()
}

/// A name between backticks when it isn't a plain identifier.
pub fn ident(name: &str) -> String {
    let plain = !name.is_empty()
        && name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_alphanumeric() || c == '_');
    if plain {
        name.to_string()
    } else {
        format!("`{}`", name.replace('`', "``"))
    }
}

/// A string literal.
pub fn string(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('\'');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '\'' => o.push_str("\\'"),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('\'');
    o
}

/// A JSON value as a Cypher literal. Maps become map literals (valid as
/// parameters of CREATE only when nested in lists they're not: properties
/// can't hold maps, so [`property`] writes those as JSON text).
pub fn literal(v: &Value) -> String {
    match v {
        Value::Null => "null".into(),
        Value::Bool(b) => b.to_string(),
        Value::Number(n) => n.to_string(),
        Value::String(s) => string(s),
        Value::Array(a) => format!("[{}]", a.iter().map(literal).collect::<Vec<_>>().join(", ")),
        Value::Object(o) => {
            format!("{{{}}}", o.iter().map(|(k, v)| format!("{}: {}", ident(k), literal(v))).collect::<Vec<_>>().join(", "))
        }
    }
}

/// A property value: maps (and lists holding maps) aren't valid property
/// values, so they go as JSON text.
pub fn property(v: &Value) -> String {
    match v {
        Value::Object(_) => string(&v.to_string()),
        Value::Array(a) if a.iter().any(|x| x.is_object() || x.is_array()) => string(&v.to_string()),
        other => literal(other),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_name_cannot_end_a_line_comment() {
        let c = format!("// {}", comment_text("a\nMATCH (n) DETACH DELETE n\r\u{85}\u{2028}\u{2029}`x"));
        assert_eq!(c, "// a?MATCH (n) DETACH DELETE n????`x");
        assert_eq!(c.lines().count(), 1);
    }
    use serde_json::json;

    #[test]
    fn splits_outside_strings_and_comments() {
        let s = split("MATCH (n) RETURN n; // a; comment\nRETURN 'a;b', \"c\\\";\" /* ; */ ;\n;  \n// only a comment\nRETURN `x;y`");
        assert_eq!(s, ["MATCH (n) RETURN n", "// a; comment\nRETURN 'a;b', \"c\\\";\" /* ; */", "// only a comment\nRETURN `x;y`"]);
        assert!(split("  ; // nothing\n").is_empty());
    }

    #[test]
    fn script_units_and_commands() {
        let s = ":use movies\nMATCH (n)\nRETURN n;\n  :begin\nCREATE (:A {t: 'x;y'}); // c; d\n:param n => 1 + 2;\n:commit\nRETURN 'é' AS a; RETURN 2";
        let u = script(s);
        let t: Vec<(&str, u32, bool)> = u.iter().map(|u| (u.text.as_str(), u.line, u.command)).collect();
        assert_eq!(
            t,
            [
                (":use movies", 1, true),
                ("MATCH (n)\nRETURN n", 2, false),
                (":begin", 4, true),
                ("CREATE (:A {t: 'x;y'})", 5, false),
                (":param n => 1 + 2", 6, true),
                (":commit", 7, true),
                ("RETURN 'é' AS a", 8, false),
                ("RETURN 2", 8, false),
            ]
        );
        for x in &u {
            assert!(s[x.start..].starts_with(&x.text), "{x:?}");
        }
        // A colon inside a statement is Cypher.
        assert_eq!(script("MATCH (n\n:Person) RETURN n").len(), 1);
        assert!(script("  // only\n/* comments */ ;").is_empty());
        assert!(script("// x\n").is_empty());
    }

    #[test]
    fn implicit_only_statements() {
        assert!(implicit_only("CALL { MATCH (n) DETACH DELETE n } IN TRANSACTIONS OF 100 ROWS"));
        assert!(implicit_only("CREATE DATABASE x IF NOT EXISTS"));
        assert!(implicit_only("create or replace database x"));
        assert!(implicit_only("show databases"));
        assert!(implicit_only("GRANT ROLE r TO u"));
        assert!(!implicit_only("CREATE (n:User {name: 'x'})"));
        assert!(!implicit_only("MATCH (n) RETURN n"));
    }

    #[test]
    fn writes() {
        assert_eq!(write_reason("MATCH (n) RETURN n.set, 'CREATE'"), None);
        assert_eq!(write_reason("MATCH (n) WHERE n.name = 'DELETE' RETURN `CREATE`"), None);
        assert_eq!(write_reason("SHOW INDEXES"), None);
        assert_eq!(write_reason("CALL db.labels() YIELD label RETURN label"), None);
        assert_eq!(write_reason("CALL dbms.queryJmx('*:*')"), None);
        assert_eq!(write_reason("CALL { MATCH (n) RETURN n } RETURN n"), None);
        assert_eq!(write_reason("match (n) detach delete n").as_deref(), Some("DETACH"));
        assert_eq!(write_reason("MERGE (a:A)").as_deref(), Some("MERGE"));
        assert_eq!(write_reason("CALL apoc.create.node(['A'], {})").as_deref(), Some("CALL apoc.create.node"));
        assert_eq!(write_reason("CALL db.createLabel('X')").as_deref(), Some("CALL db.createlabel"));
        assert_eq!(write_reason("CALL { CREATE (n) } IN TRANSACTIONS").as_deref(), Some("CREATE"));
    }

    #[test]
    fn quoting() {
        assert_eq!(ident("Person"), "Person");
        assert_eq!(ident("Mi Etiqueta"), "`Mi Etiqueta`");
        assert_eq!(ident("a`b"), "`a``b`");
        assert_eq!(string("it's \\ ok\n"), "'it\\'s \\\\ ok\\n'");
        assert_eq!(literal(&json!({ "a b": [1, "x"] })), "{`a b`: [1, 'x']}");
        assert_eq!(property(&json!({ "a": 1 })), "'{\"a\":1}'");
        assert_eq!(property(&json!([1, 2])), "[1, 2]");
    }
}
