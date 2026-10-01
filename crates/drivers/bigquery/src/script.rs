//! Editor scripts on BigQuery: how a script is split, when a run needs a
//! BigQuery session (temp tables, variables and transactions that last
//! between runs), and errors placed by their `[line:column]`.

use crate::blocks;
use dbine_driver::sql::{split_script, ScriptDialect, ScriptStatement};
use dbine_driver::ScriptError;

/// The console's reading: backslash escapes, `#` comments, `` `ident` ``.
pub fn dialect() -> ScriptDialect {
    ScriptDialect { backslash_escapes: true, hash_comments: true, ..ScriptDialect::generic() }
}

fn rules() -> blocks::Rules {
    blocks::Rules { hash_comments: true, ..Default::default() }
}

/// The statements as BigQuery scripting runs them: `BEGIN … [EXCEPTION
/// …] END`, `IF`, `LOOP`, `WHILE`, `FOR`, `REPEAT` whole.
pub fn units(text: &str) -> Vec<ScriptStatement> {
    blocks::merge(text, split_script(text, &dialect()), rules())
}

/// The first two words, upper-cased.
fn head(stmt: &str) -> (String, String) {
    let mut w = stmt.split(|c: char| c.is_whitespace() || c == ';' || c == '(').filter(|w| !w.is_empty());
    let up = |s: Option<&str>| s.unwrap_or_default().to_ascii_uppercase();
    (up(w.next()), up(w.next()))
}

/// The script leaves something for the next run: a temp table or
/// function, a variable or system variable, a transaction. Those need a
/// BigQuery session; other scripts run without one.
pub fn needs_session(units: &[ScriptStatement]) -> bool {
    units.iter().any(|u| {
        let (a, b) = head(&u.text);
        match a.as_str() {
            "DECLARE" | "SET" | "COMMIT" | "ROLLBACK" => true,
            "BEGIN" => b.is_empty() || b == "TRANSACTION",
            "CREATE" => {
                let words: Vec<String> = u.text.split_whitespace().take(5).map(str::to_ascii_uppercase).collect();
                words.iter().any(|w| w == "TEMP" || w == "TEMPORARY")
            }
            _ => false,
        }
    })
}

/// More than one statement (or a block): BigQuery runs it as a script,
/// with a child job per statement.
pub fn is_script(units: &[ScriptStatement]) -> bool {
    units.len() > 1 || units.iter().any(|u| u.kind == dbine_driver::StatementKind::Block)
}

/// The session ended on the server (it expired after its idle time, or
/// was terminated): a new one is needed.
pub fn session_gone(msg: &str) -> bool {
    let m = msg.to_ascii_lowercase();
    m.contains("session") && (m.contains("expired") || m.contains("not found") || m.contains("terminated") || m.contains("is not active"))
}

/// A refused query, placed by its `[line:column]` (BigQuery counts both in
/// the whole text it got: `Syntax error: … at [3:5]`, `… [at 3:5]`).
pub fn error(message: &str, text: &str) -> ScriptError {
    let mut e = ScriptError::new(message);
    if let Some((line, col)) = position(message) {
        if let Some(off) = offset_of(text, line, col) {
            e.line = Some(line);
            e.offset = Some(off);
        }
    }
    e
}

fn position(msg: &str) -> Option<(u32, u32)> {
    let mut rest = msg;
    // The last bracketed `[L:C]` / `[at L:C]` is the position.
    let mut found = None;
    while let Some(i) = rest.find('[') {
        let inner = &rest[i + 1..];
        let Some(j) = inner.find(']') else { break };
        let body = inner[..j].trim_start_matches("at ").trim();
        if let Some((l, c)) = body.split_once(':') {
            if let (Ok(l), Ok(c)) = (l.parse(), c.parse()) {
                found = Some((l, c));
            }
        }
        rest = &inner[j..];
    }
    found
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn scripting_blocks_stay_whole() {
        let t = |s: &str| units(s).into_iter().map(|u| u.text).collect::<Vec<_>>();
        let s = "DECLARE x INT64 DEFAULT 0;\nLOOP\n  SET x = x + 1;\n  IF x >= 3 THEN LEAVE; END IF;\nEND LOOP;\nBEGIN\n  SELECT 1/0;\nEXCEPTION WHEN ERROR THEN\n  SELECT @@error.message;\nEND;\nSELECT x; # done; really";
        assert_eq!(t(s).len(), 4);
        assert_eq!(t(s)[3], "SELECT x");
        assert_eq!(t("SELECT 'a\\';b'; SELECT \"#\"; SELECT 3").len(), 3);
        assert!(is_script(&units(s)));
        assert!(!is_script(&units("SELECT 1;")));
        assert!(is_script(&units("BEGIN SELECT 1; SELECT 2; END")));
    }

    #[test]
    fn what_needs_a_session() {
        let n = |s: &str| needs_session(&units(s));
        assert!(n("CREATE TEMP TABLE t AS SELECT 1"));
        assert!(n("create or replace temporary function f() as (1)"));
        assert!(n("DECLARE x INT64 DEFAULT 1"));
        assert!(n("SET @@dataset_id = 'ventas'"));
        assert!(n("BEGIN TRANSACTION; INSERT INTO d.t VALUES (1)"));
        assert!(n("BEGIN; INSERT INTO d.t VALUES (1)"));
        assert!(n("COMMIT"));
        assert!(!n("SELECT 1; CREATE TABLE d.t (a INT64)"));
        assert!(!n("BEGIN SELECT 1; END"));
        assert!(!n("GRANT `roles/bigquery.dataViewer` ON SCHEMA d TO 'user:a@b.c'"));
    }

    #[test]
    fn errors_are_placed_in_the_script() {
        let s = "SELECT 1;\nSELECT x FROM\n  nope";
        let e = error("Unrecognized name: x at [2:8]", s);
        assert_eq!((e.line, e.offset), (Some(2), Some(s.find("x FROM").unwrap())));
        let e = error("Table not found: nope [at 3:3]; Did you mean …", s);
        assert_eq!((e.line, e.offset), (Some(3), Some(s.find("nope").unwrap())));
        let e = error("Access Denied: [project] no", s);
        assert_eq!((e.line, e.offset), (None, None));
        assert!(session_gone("Session abc has expired"));
        assert!(session_gone("Session not found: xyz"));
        assert!(!session_gone("Syntax error"));
    }
}
