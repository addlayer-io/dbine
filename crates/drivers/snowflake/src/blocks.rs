//! Anonymous procedural blocks kept whole when a script is split: `BEGIN …
//! END`, Snowflake's `DECLARE … BEGIN … END`, and control flow written at
//! the top level (`IF … END IF`, `LOOP … END LOOP`, `WHILE`, `FOR`,
//! `REPEAT`). The shared dialect only keeps routine bodies (`CREATE
//! PROCEDURE … BEGIN … END`) whole and cuts these at each `;`, so the
//! units it gives are joined back until the block closes.

use dbine_driver::sql::{ScriptStatement, StatementKind};

/// What opens a block on this engine.
#[derive(Clone, Copy, Default)]
pub(crate) struct Rules {
    /// A statement starting with `DECLARE` opens a block that runs to the
    /// end of its `BEGIN … END` (Snowflake Scripting). Off where `DECLARE`
    /// is a statement of its own (BigQuery, Databricks).
    pub declare_opens: bool,
    /// `#` starts a line comment (BigQuery).
    pub hash_comments: bool,
    /// `//` starts a line comment (Snowflake).
    pub slash_comments: bool,
}

/// `units` (from the dialect's split of `script`) with each anonymous
/// block joined into one [`StatementKind::Block`] unit.
pub(crate) fn merge(script: &str, units: Vec<ScriptStatement>, rules: Rules) -> Vec<ScriptStatement> {
    let mut out = Vec::with_capacity(units.len());
    let mut i = 0;
    while i < units.len() {
        let u = &units[i];
        if u.kind == StatementKind::ClientCommand {
            out.push(u.clone());
            i += 1;
            continue;
        }
        let (d, head) = balance(&u.text, rules);
        let declare = rules.declare_opens && head.as_deref() == Some("DECLARE");
        if d <= 0 && !declare {
            out.push(u.clone());
            i += 1;
            continue;
        }
        let (mut depth, mut opened, mut j) = (d, d > 0, i);
        while (depth > 0 || !opened) && j + 1 < units.len() {
            j += 1;
            let (dj, _) = balance(&units[j].text, rules);
            depth += dj;
            opened |= depth > 0;
        }
        let (start, end) = (u.start, units[j].end);
        out.push(ScriptStatement {
            text: script[start..end].to_string(),
            start,
            end,
            line: u.line,
            kind: if j > i { StatementKind::Block } else { u.kind },
            repeat: 1,
            error: None,
        });
        i = j + 1;
    }
    out
}

/// A word of a statement, uppercased, and whether it can be a keyword
/// (`t.end`, `:end`, `x::end` can't).
struct Word {
    text: String,
    keyword: bool,
    /// A `(` right after it: `IF(a, b, c)` is a function.
    call: bool,
}

/// Blocks a statement opens minus those it closes, and its first word
/// (after a `label:`).
fn balance(text: &str, rules: Rules) -> (i32, Option<String>) {
    let words = words(text, rules);
    let mut k = 0;
    // `label: BEGIN`, `label: LOOP`
    if words.len() > 1 && words[1].text == ":" {
        k = 2;
    }
    let words: Vec<&Word> = words[k..].iter().filter(|w| w.text != ":").collect();
    let head = words.first().map(|w| w.text.clone());
    let mut d = 0;
    let mut i = 0;
    while i < words.len() {
        let w = words[i];
        let next = words.get(i + 1).filter(|n| n.keyword).map(|n| n.text.as_str());
        // Control flow opens where a statement starts: first, or right
        // after BEGIN / THEN / ELSE / DO / LOOP / REPEAT.
        let starts = i == 0 || matches!(words[i - 1].text.as_str(), "BEGIN" | "THEN" | "ELSE" | "DO" | "LOOP" | "REPEAT");
        if w.keyword && starts && !w.call && matches!(w.text.as_str(), "IF" | "LOOP" | "WHILE" | "FOR" | "REPEAT") {
            d += 1;
        }
        if w.keyword {
            match w.text.as_str() {
                // `BEGIN;`, `BEGIN TRANSACTION`, `BEGIN WORK`, `BEGIN NAME t`:
                // a transaction, not a block.
                "BEGIN" if next.is_some_and(|n| !matches!(n, "TRANSACTION" | "TRAN" | "WORK" | "NAME")) => d += 1,
                "BEGIN" if next.is_none() && i + 1 < words.len() => d += 1,
                "CASE" => d += 1,
                "END" => {
                    d -= 1;
                    if matches!(next, Some("IF" | "LOOP" | "WHILE" | "FOR" | "REPEAT" | "CASE")) {
                        i += 1;
                    }
                }
                _ => {}
            }
        }
        i += 1;
    }
    (d, head)
}

/// The words of `text` outside quotes and comments; `:` is kept as a word
/// so a leading label can be told.
fn words(text: &str, rules: Rules) -> Vec<Word> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        match c {
            b'\'' | b'"' | b'`' => {
                i += 1;
                while i < b.len() {
                    if b[i] == b'\\' && c != b'`' {
                        i += 2;
                        continue;
                    }
                    if b[i] == c {
                        if b.get(i + 1) == Some(&c) {
                            i += 2;
                            continue;
                        }
                        break;
                    }
                    i += 1;
                }
                i += 1;
            }
            b'$' if b.get(i + 1) == Some(&b'$') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'$' && b[i + 1] == b'$') {
                    i += 1;
                }
                i += 2;
            }
            b'-' if b.get(i + 1) == Some(&b'-') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'#' if rules.hash_comments => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if rules.slash_comments && b.get(i + 1) == Some(&b'/') => {
                while i < b.len() && b[i] != b'\n' {
                    i += 1;
                }
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                i += 2;
                while i + 1 < b.len() && !(b[i] == b'*' && b[i + 1] == b'/') {
                    i += 1;
                }
                i += 2;
            }
            b':' => {
                // A label's colon (`lbl:`, right after a word), kept so the
                // head can skip it; `::` casts and `:var` binds aren't.
                if b.get(i + 1) != Some(&b':') && i > 0 && ident(b[i - 1]) {
                    out.push(Word { text: ":".into(), keyword: false, call: false });
                }
                i += 1;
            }
            c if c.is_ascii_alphabetic() || c == b'_' => {
                let s = i;
                while i < b.len() && (b[i].is_ascii_alphanumeric() || b[i] == b'_' || b[i] == b'$') {
                    i += 1;
                }
                let prev = s.checked_sub(1).map(|p| b[p]);
                // `t.end`, `@end`, `$end`, a `:end` bind and an `x::end`
                // cast; `lbl:BEGIN` is a label and its keyword.
                let bound = match prev {
                    Some(b'.' | b'@' | b'$') => true,
                    Some(b':') => !(s >= 2 && ident(b[s - 2])),
                    _ => false,
                };
                out.push(Word { text: text[s..i].to_ascii_uppercase(), keyword: !bound, call: b.get(i) == Some(&b'(') });
            }
            _ => i += 1,
        }
    }
    out
}

fn ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_'
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::sql::{split_script, ScriptDialect};

    fn split(s: &str, rules: Rules) -> Vec<String> {
        merge(s, split_script(s, &ScriptDialect::generic()), rules).into_iter().map(|u| u.text).collect()
    }

    #[test]
    fn anonymous_blocks_stay_whole() {
        let r = Rules::default();
        assert_eq!(
            split("BEGIN\n  CREATE TABLE t (a INT);\n  INSERT INTO t VALUES (1);\nEND;\nSELECT 1;", r),
            vec!["BEGIN\n  CREATE TABLE t (a INT);\n  INSERT INTO t VALUES (1);\nEND", "SELECT 1"]
        );
        // Nested blocks, IF … END IF, CASE expressions and statements.
        let s = "BEGIN\n DECLARE x INT DEFAULT 0;\n IF x = 0 THEN\n  BEGIN SELECT CASE WHEN x > 1 THEN 1 ELSE 2 END; END;\n ELSE SELECT 3;\n END IF;\n CASE x WHEN 1 THEN SELECT 1; ELSE SELECT 2; END CASE;\nEND;\nselect 2";
        assert_eq!(split(s, r).len(), 2);
        // Top-level control flow (BigQuery scripting).
        assert_eq!(split("LOOP\n SET x = x + 1;\n IF x > 3 THEN LEAVE; END IF;\nEND LOOP;\nSELECT x;", r).len(), 2);
        assert_eq!(split("WHILE x < 3 DO SET x = x + 1; END WHILE; SELECT x", r).len(), 2);
        // Control flow right after BEGIN / THEN, on the same line.
        assert_eq!(split("BEGIN FOR i IN 1 TO 3 DO SELECT i; END FOR; RETURN 1; END; SELECT 2", r).len(), 2);
        assert_eq!(split("BEGIN IF a THEN IF b THEN SELECT 1; END IF; END IF; END; SELECT 2", r).len(), 2);
        assert_eq!(split("FOR r IN (SELECT 1 AS a) DO SELECT r.a; END FOR; SELECT 2", r).len(), 2);
        assert_eq!(split("REPEAT SET x = x + 1; UNTIL x > 3 END REPEAT; SELECT 2", r).len(), 2);
        // Labels.
        assert_eq!(split("lbl: BEGIN SELECT 1; SELECT 2; END lbl; SELECT 3", r).len(), 2);
        assert_eq!(split("lbl:BEGIN SELECT 1; SELECT 2; END lbl; SELECT 3", r).len(), 2);
        assert_eq!(split("outer: LOOP SELECT 1; LEAVE outer; END LOOP outer; SELECT 3", r).len(), 2);
        // BigQuery's BEGIN … EXCEPTION … END.
        assert_eq!(split("BEGIN SELECT 1/0; EXCEPTION WHEN ERROR THEN SELECT @@error.message; END; SELECT 2", r).len(), 2);
    }

    #[test]
    fn transactions_and_plain_statements_are_not_blocks() {
        let r = Rules::default();
        assert_eq!(split("BEGIN; INSERT INTO t VALUES (1); COMMIT;", r).len(), 3);
        assert_eq!(split("BEGIN TRANSACTION; INSERT INTO t VALUES (1); COMMIT", r).len(), 3);
        assert_eq!(split("BEGIN WORK; SELECT 1; COMMIT", r).len(), 3);
        assert_eq!(split("BEGIN NAME t1; SELECT 1; COMMIT", r).len(), 3);
        // Keywords as names, binds and casts, quoted text, comments.
        assert_eq!(split("SELECT t.end, t.begin FROM t; SELECT :end, x::end; SELECT 'BEGIN', \"END\"; SELECT 1 -- BEGIN\n; SELECT 2", r).len(), 5);
        assert_eq!(split("SELECT IF(a, 1, 2) FROM t; DROP TABLE IF EXISTS t; SELECT 3", r).len(), 3);
        assert_eq!(split("SELECT CASE WHEN a THEN IF(b, 1, 2) ELSE 0 END FROM t; SELECT 3", r).len(), 2);
        assert_eq!(split("SELECT $$ BEGIN $$; SELECT /* BEGIN */ 2", r).len(), 2);
        assert_eq!(split("SELECT 1 # BEGIN\n; SELECT 2", Rules { hash_comments: true, ..r }).len(), 2);
        // Routine bodies were already whole: balanced, left as they are.
        let s = "CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END; SELECT 3";
        assert_eq!(split(s, r), vec!["CREATE PROCEDURE p() BEGIN SELECT 1; SELECT 2; END", "SELECT 3"]);
    }

    #[test]
    fn declare_opens_a_block_where_the_engine_says_so() {
        let s = "DECLARE\n  x INT DEFAULT 1;\n  y STRING;\nBEGIN\n  y := 'a';\n  RETURN x;\nEND;\nSELECT 1;";
        let snowflake = Rules { declare_opens: true, ..Rules::default() };
        assert_eq!(split(s, snowflake).len(), 2);
        assert_eq!(split(s, snowflake)[0], s[..s.find("END;").unwrap() + 3]);
        // BigQuery: DECLARE is a statement of its own.
        assert_eq!(split("DECLARE x INT64 DEFAULT 1; SELECT x;", Rules::default()).len(), 2);
    }

    #[test]
    fn positions_are_those_of_the_script() {
        let s = "select 0;\n\nBEGIN\n SELECT 1;\nEND;";
        let units = merge(s, split_script(s, &ScriptDialect::generic()), Rules::default());
        assert_eq!(units.len(), 2);
        assert_eq!((units[1].start, units[1].line, units[1].kind), (11, 3, StatementKind::Block));
        assert_eq!(&s[units[1].start..units[1].end], "BEGIN\n SELECT 1;\nEND");
    }
}
