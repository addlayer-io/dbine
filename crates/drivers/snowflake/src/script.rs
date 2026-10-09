//! Editor scripts on the SQL API: how a script is split, what a run leaves
//! in the session for the next one, and errors with their position.
//!
//! The SQL API opens a server session per request, so `USE`, `ALTER
//! SESSION` and `SET` variables would be lost between runs. Each run ends
//! with a query of the session's context (and `SHOW VARIABLES` when the
//! script sets any), which becomes the context of the next request; the
//! `ALTER SESSION` statements that ran and the variables are replayed at the
//! start of it.

use crate::blocks;
use dbine_driver::sql::{split_script, ScriptDialect, ScriptStatement};
use dbine_driver::ScriptError;
use serde_json::Value as Json;

/// What the session holds after each run, read in the same request.
pub const CONTEXT_QUERY: &str = "SELECT CURRENT_DATABASE(), CURRENT_SCHEMA(), CURRENT_WAREHOUSE(), CURRENT_ROLE()";
pub const VARIABLES_QUERY: &str = "SHOW VARIABLES";
/// `ALTER SESSION` statements kept for replay, at most.
const MAX_ALTERS: usize = 64;

/// snowsql's reading of a script: backslash escapes in '…', `$$ … $$`
/// bodies, `"ident"` (escaped only by doubling `""`).
pub fn dialect() -> ScriptDialect {
    ScriptDialect { backslash_escapes: true, dquote_idents: true, dollar_quotes: true, backtick_idents: false, ..ScriptDialect::generic() }
}

/// The statements as Snowflake runs them: anonymous blocks (`BEGIN … END`,
/// `DECLARE … BEGIN … END`) whole.
pub fn units(text: &str) -> Vec<ScriptStatement> {
    let rules = blocks::Rules { declare_opens: true, ..Default::default() };
    blocks::merge(text, split_script(text, &dialect()), rules)
}

/// The first two words, upper-cased.
pub fn head(stmt: &str) -> (String, String) {
    let mut w = stmt.split(|c: char| c.is_whitespace() || c == ';' || c == '(').filter(|w| !w.is_empty());
    let up = |s: Option<&str>| s.unwrap_or_default().to_ascii_uppercase();
    (up(w.next()), up(w.next()))
}

/// The script can change the session's database, schema, warehouse or
/// role: `USE …`, or `EXECUTE IMMEDIATE` (which may run one). Only then is
/// the context read back after the run.
pub fn changes_context(units: &[ScriptStatement]) -> bool {
    units.iter().any(|u| {
        let (a, b) = head(&u.text);
        a == "USE" || (a == "EXECUTE" && b == "IMMEDIATE")
    })
}

/// The script ends with a transaction open (`BEGIN` / `START TRANSACTION`
/// after its last `COMMIT` / `ROLLBACK`): it doesn't reach the next run,
/// which is another request (another server session).
pub fn leaves_transaction_open(units: &[ScriptStatement]) -> bool {
    let mut open = false;
    for u in units.iter().filter(|u| u.kind != dbine_driver::StatementKind::Block) {
        match head(&u.text) {
            (a, b) if a == "BEGIN" && matches!(b.as_str(), "" | "TRANSACTION" | "WORK" | "NAME") => open = true,
            (a, b) if a == "START" && b == "TRANSACTION" => open = true,
            (a, _) if a == "COMMIT" || a == "ROLLBACK" => open = false,
            _ => {}
        }
    }
    open
}

/// Session state carried from one request to the next.
#[derive(Default, Clone, Debug, PartialEq)]
pub struct Carry {
    /// `ALTER SESSION …` statements that ran, in order.
    pub alters: Vec<String>,
    /// Session variables as `SET name = literal`.
    pub vars: Vec<String>,
}

impl Carry {
    /// What goes before the script in the next request.
    pub fn preamble(&self) -> Vec<String> {
        self.alters.iter().chain(&self.vars).cloned().collect()
    }

    /// The `ALTER SESSION` statements of a script that ran.
    pub fn absorb_alters(&mut self, units: &[ScriptStatement]) {
        for u in units {
            let (a, b) = head(&u.text);
            if a == "ALTER" && b == "SESSION" && !self.alters.contains(&u.text) {
                self.alters.push(u.text.clone());
            }
        }
        let extra = self.alters.len().saturating_sub(MAX_ALTERS);
        self.alters.drain(..extra);
    }

    /// The variables from `SHOW VARIABLES` (column names lower-cased).
    pub fn set_vars(&mut self, names: &[String], rows: &[Json]) {
        let col = |n: &str| names.iter().position(|c| c == n);
        let (Some(n), Some(v), Some(t)) = (col("name"), col("value"), col("type")) else { return };
        self.vars = rows
            .iter()
            .filter_map(|r| {
                let r = r.as_array()?;
                let name = r.get(n)?.as_str()?;
                let value = r.get(v).and_then(Json::as_str);
                let ty = r.get(t).and_then(Json::as_str).unwrap_or("text");
                Some(format!("SET {} = {}", var_name(name), literal(value, ty)))
            })
            .collect();
    }
}

fn var_name(name: &str) -> String {
    let plain = name.chars().next().is_some_and(|c| c.is_ascii_alphabetic() || c == '_')
        && name.chars().all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_' || c == '$');
    if plain {
        name.to_string()
    } else {
        format!("\"{}\"", name.replace('"', "\"\""))
    }
}

/// A `SHOW VARIABLES` value as a literal of its type.
fn literal(value: Option<&str>, ty: &str) -> String {
    let Some(v) = value else { return "NULL".into() };
    let quoted = format!("'{}'", v.replace('\\', "\\\\").replace('\'', "''"));
    match ty.to_ascii_lowercase().as_str() {
        "fixed" | "real" | "number" | "float" | "boolean" if !v.is_empty() => v.to_string(),
        t @ ("date" | "time" | "timestamp_ltz" | "timestamp_ntz" | "timestamp_tz" | "binary") => format!("{quoted}::{t}"),
        _ => quoted,
    }
}

/// A failed request as a statement error: the body's code, SQLSTATE and
/// message. Its position (`line L at position P`, P from 0) counts in the
/// failing statement, so it's placed only when the script had one.
pub fn error(body: &Json, units: &[ScriptStatement]) -> Option<ScriptError> {
    let message = body.get("message").and_then(Json::as_str).filter(|m| !m.is_empty())?;
    let mut e = ScriptError::new(message);
    if let Some(c) = body.get("code").and_then(Json::as_str).filter(|c| !c.is_empty()) {
        e = e.with_code(c);
    }
    if let Some(s) = body.get("sqlState").and_then(Json::as_str).filter(|s| !s.is_empty()) {
        e = e.with_sqlstate(s);
    }
    if let ([u], Some((line, pos))) = (units, position(message)) {
        if let Some(off) = offset_of(&u.text, line, pos + 1) {
            e.offset = Some(u.start + off);
            e.line = Some(u.line + line - 1);
        }
    }
    Some(e)
}

/// `… line 2 at position 7 …`.
fn position(msg: &str) -> Option<(u32, u32)> {
    let at = msg.find("line ")?;
    let mut w = msg[at + 5..].split_whitespace();
    let line = w.next()?.parse().ok()?;
    (w.next()? == "at" && w.next()? == "position").then_some(())?;
    let pos: String = w.next()?.chars().take_while(char::is_ascii_digit).collect();
    Some((line, pos.parse().ok()?))
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
    use serde_json::json;

    #[test]
    fn units_keep_blocks_and_dollar_bodies() {
        let t = |s: &str| units(s).into_iter().map(|u| u.text).collect::<Vec<_>>();
        assert_eq!(t("SELECT 1; SELECT ';'"), vec!["SELECT 1", "SELECT ';'"]);
        assert_eq!(t("SELECT 'a\\';b'; SELECT 2").len(), 2);
        let block = "EXECUTE IMMEDIATE $$\nBEGIN\n  SELECT 1;\n  SELECT 2;\nEND;\n$$";
        assert_eq!(t(&format!("{block};\nSELECT 3;")), vec![block, "SELECT 3"]);
        // Anonymous blocks, as Snowsight runs them.
        let anon = "DECLARE\n  n INT DEFAULT 0;\nBEGIN\n  FOR i IN 1 TO 3 DO\n    n := n + i;\n  END FOR;\n  RETURN n;\nEND";
        assert_eq!(t(&format!("{anon};\nSELECT 3;")), vec![anon, "SELECT 3"]);
        assert_eq!(t("BEGIN\n  CREATE TABLE t (a INT);\n  INSERT INTO t VALUES (1);\nEND;\nSELECT 1").len(), 2);
        // Transactions aren't blocks.
        assert_eq!(t("BEGIN; INSERT INTO t VALUES (1); COMMIT;").len(), 3);
        assert_eq!(t("BEGIN TRANSACTION; INSERT INTO t VALUES (1); COMMIT;").len(), 3);
    }

    #[test]
    fn alter_session_and_variables_carry_over() {
        let mut c = Carry::default();
        c.absorb_alters(&units("alter session set TIMEZONE = 'UTC'; select 1; ALTER SESSION UNSET QUERY_TAG; alter session set TIMEZONE = 'UTC'"));
        assert_eq!(c.alters, vec!["alter session set TIMEZONE = 'UTC'", "ALTER SESSION UNSET QUERY_TAG"]);
        let names: Vec<String> = ["session_id", "created_on", "updated_on", "name", "value", "type", "comment"].map(String::from).to_vec();
        let rows = vec![
            json!(["1", "t", "t", "N", "42", "fixed", null]),
            json!(["1", "t", "t", "S", "it's \\ x", "text", null]),
            json!(["1", "t", "t", "B", "true", "boolean", null]),
            json!(["1", "t", "t", "D", "2024-01-31", "date", null]),
            json!(["1", "t", "t", "mixed case", null, "text", null]),
        ];
        c.set_vars(&names, &rows);
        assert_eq!(
            c.vars,
            vec![
                "SET N = 42",
                "SET S = 'it''s \\\\ x'",
                "SET B = true",
                "SET D = '2024-01-31'::date",
                "SET \"mixed case\" = NULL",
            ]
        );
        assert_eq!(c.preamble().len(), 7);
    }

    #[test]
    fn only_use_and_execute_immediate_change_the_context() {
        assert!(changes_context(&units("select 1; use schema s")));
        assert!(changes_context(&units("EXECUTE IMMEDIATE 'USE DATABASE d'")));
        assert!(!changes_context(&units("CREATE ROLE r; GRANT ROLE r TO USER u; -- use x")));
        assert!(!changes_context(&units("CREATE DATABASE \"V_BKP_1\" CLONE \"V\";")));
    }

    #[test]
    fn open_transactions_at_the_end_are_found() {
        assert!(leaves_transaction_open(&units("BEGIN; INSERT INTO t VALUES (1)")));
        assert!(leaves_transaction_open(&units("commit; begin transaction;\nselect 1")));
        assert!(!leaves_transaction_open(&units("BEGIN; INSERT INTO t VALUES (1); COMMIT;")));
        assert!(!leaves_transaction_open(&units("START TRANSACTION; ROLLBACK")));
        assert!(!leaves_transaction_open(&units("BEGIN\n  INSERT INTO t VALUES (1);\nEND;")));
    }

    #[test]
    fn errors_carry_code_sqlstate_and_position() {
        let body = json!({ "code": "001003", "sqlState": "42000", "message": "SQL compilation error:\nsyntax error line 2 at position 0 unexpected 'fron'." });
        let s = "-- head\nselect 1\nfron t";
        let u = units(s);
        let e = error(&body, &u).unwrap();
        assert_eq!((e.code.as_deref(), e.sqlstate.as_deref()), (Some("001003"), Some("42000")));
        assert_eq!((e.line, e.offset), (Some(3), Some(s.find("fron").unwrap())));
        // Several statements: which one failed isn't known.
        let e = error(&body, &units("select 1; select 2")).unwrap();
        assert_eq!((e.line, e.offset), (None, None));
        assert!(error(&json!({}), &u).is_none());
    }
}
