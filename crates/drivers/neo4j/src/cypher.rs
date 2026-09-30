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
    use serde_json::json;

    #[test]
    fn splits_outside_strings_and_comments() {
        let s = split("MATCH (n) RETURN n; // a; comment\nRETURN 'a;b', \"c\\\";\" /* ; */ ;\n;  \n// only a comment\nRETURN `x;y`");
        assert_eq!(s, ["MATCH (n) RETURN n", "// a; comment\nRETURN 'a;b', \"c\\\";\" /* ; */", "// only a comment\nRETURN `x;y`"]);
        assert!(split("  ; // nothing\n").is_empty());
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
