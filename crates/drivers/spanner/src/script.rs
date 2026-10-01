//! Editor scripts: errors placed in the text the driver got.

use dbine_driver::sql::ScriptStatement;
use dbine_driver::{Error, ScriptError};

/// Byte offset of a 1-based line and column (columns count characters).
pub fn offset_of(text: &str, line: u32, col: u32) -> Option<usize> {
    let mut start = 0;
    for _ in 1..line.max(1) {
        start += text[start..].find('\n')? + 1;
    }
    let end = text[start..].find('\n').map_or(text.len(), |e| start + e);
    let skip = col.saturating_sub(1) as usize;
    Some(start + text[start..end].char_indices().nth(skip).map_or(end - start, |(i, _)| i))
}

/// A statement error with its position in `text` (1-based line and column).
pub fn placed(mut e: ScriptError, text: &str, at: Option<(u32, u32)>) -> ScriptError {
    if let Some((line, col)) = at {
        if let Some(off) = offset_of(text, line, col) {
            e.line = Some(line);
            e.offset = Some(off);
        }
    }
    e
}

/// An error of one unit moved to the whole text the driver got.
pub fn shift(e: Error, unit: &ScriptStatement) -> Error {
    match e {
        Error::Statement(mut se) => {
            se.offset = Some(unit.start + se.offset.unwrap_or(0).min(unit.text.len()));
            se.line = Some(unit.line + se.line.unwrap_or(1) - 1);
            Error::Statement(se)
        }
        other => other,
    }
}

/// spanner-cli's reading of GoogleSQL: backslash escapes, `#` comments,
/// `` `ident` ``.
pub fn dialect() -> dbine_driver::ScriptDialect {
    dbine_driver::ScriptDialect { backslash_escapes: true, hash_comments: true, ..dbine_driver::ScriptDialect::generic() }
}

/// A refused statement, placed by GoogleSQL's `[at L:C]`; without one it
/// stays the plain error it was.
pub fn error(message: &str, stmt: &str) -> Error {
    let at = message.rfind("[at ").and_then(|i| {
        let (l, c) = message[i + 4..].split_once(':')?;
        let c: String = c.chars().take_while(char::is_ascii_digit).collect();
        Some((l.parse().ok()?, c.parse().ok()?))
    });
    match placed(ScriptError::new(message), stmt, at) {
        e if e.line.is_some() => e.into(),
        _ => Error::Query(message.to_string()),
    }
}

/// The transaction statements spanner-cli takes: `BEGIN [RW|TRANSACTION]`,
/// `COMMIT [TRANSACTION]`, `ROLLBACK [TRANSACTION]`.
#[derive(Debug, PartialEq)]
pub enum TxCommand {
    Begin,
    Commit,
    Rollback,
}

pub fn tx_command(stmt: &str) -> Option<TxCommand> {
    let words: Vec<String> = stmt.split_whitespace().map(str::to_ascii_uppercase).collect();
    let rest_ok = |extra: &[&str]| words.len() == 1 || (words.len() == 2 && extra.contains(&words[1].as_str()));
    match words.first()?.as_str() {
        "BEGIN" if rest_ok(&["TRANSACTION", "RW", "WORK"]) => Some(TxCommand::Begin),
        "COMMIT" if rest_ok(&["TRANSACTION", "WORK"]) => Some(TxCommand::Commit),
        "ROLLBACK" if rest_ok(&["TRANSACTION", "WORK"]) => Some(TxCommand::Rollback),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::sql::split_script;

    #[test]
    fn errors_are_placed() {
        let s = "SELECT 1;\n# note\nSELECT x\nFROM nope";
        let u = &split_script(s, &dialect())[1];
        let Error::Statement(e) = shift(error("Table not found: nope [at 2:6]\nFROM nope\n     ^", &u.text), u) else { panic!() };
        assert_eq!((e.line, e.offset), (Some(4), Some(s.find("nope").unwrap())));
        assert!(matches!(error("Row [1] already exists", "x"), Error::Query(m) if m == "Row [1] already exists"));
    }

    #[test]
    fn transaction_statements() {
        assert_eq!(tx_command("BEGIN"), Some(TxCommand::Begin));
        assert_eq!(tx_command("begin transaction"), Some(TxCommand::Begin));
        assert_eq!(tx_command("BEGIN RW"), Some(TxCommand::Begin));
        assert_eq!(tx_command("commit"), Some(TxCommand::Commit));
        assert_eq!(tx_command("ROLLBACK TRANSACTION"), Some(TxCommand::Rollback));
        assert_eq!(tx_command("BEGIN RO STALENESS 10s"), None);
        assert_eq!(tx_command("SELECT 1"), None);
        assert_eq!(split_script("SELECT 'a\\';b'; SELECT 2 # x;\n; SELECT 3", &dialect()).len(), 3);
    }
}
