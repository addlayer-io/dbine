//! Script helpers: error positions and statement units. Athena (Trino
//! underneath: same `line L:C:` messages) includes this file via `#[path]`.

use dbine_driver::sql::{split_script, ScriptDialect, ScriptStatement};
use dbine_driver::{Error, ScriptError};

/// `line 1:8: Column 'x' cannot be resolved` → (1, 8). Also finds it after
/// a prefix (`SYNTAX_ERROR: line 2:3: …`).
pub fn line_col(msg: &str) -> Option<(u32, u32)> {
    let at = msg.find("line ")?;
    let rest = &msg[at + 5..];
    let (l, rest) = rest.split_once(':')?;
    let c: String = rest.chars().take_while(char::is_ascii_digit).collect();
    Some((l.trim().parse().ok()?, c.parse().ok()?))
}

/// Byte offset of a 1-based line and column (columns count characters).
pub fn offset_of(text: &str, line: u32, col: u32) -> Option<usize> {
    if line == 0 {
        return None;
    }
    let mut start = 0;
    for _ in 1..line {
        start += text[start..].find('\n')? + 1;
    }
    let line_text = &text[start..text[start..].find('\n').map_or(text.len(), |e| start + e)];
    let skip = col.saturating_sub(1) as usize;
    Some(start + line_text.char_indices().nth(skip).map_or(line_text.len(), |(i, _)| i))
}

/// A statement error placed in `text` by its line and column.
pub fn placed(mut e: ScriptError, text: &str, line: Option<u32>, col: Option<u32>) -> ScriptError {
    if let Some(l) = line.filter(|l| *l > 0) {
        e.line = Some(l);
        e.offset = offset_of(text, l, col.unwrap_or(1));
    }
    e
}

/// The statements of `text` as the app splits it.
pub fn units(text: &str, dialect: &ScriptDialect) -> Vec<ScriptStatement> {
    split_script(text, dialect)
}

/// An error of one unit moved to the whole text it came from (the driver
/// got several statements at once): offset and line shifted.
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

/// The first two words of a statement, upper-cased.
pub fn head(stmt: &str) -> (String, String) {
    let mut w = stmt.split(|c: char| c.is_whitespace() || c == ';' || c == '(').filter(|w| !w.is_empty());
    let up = |s: Option<&str>| s.unwrap_or_default().to_ascii_uppercase();
    (up(w.next()), up(w.next()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn positions_from_messages() {
        assert_eq!(line_col("line 1:8: Column 'x' cannot be resolved"), Some((1, 8)));
        assert_eq!(line_col("SYNTAX_ERROR: line 2:13: mismatched input"), Some((2, 13)));
        assert_eq!(line_col("Table not found"), None);
        let t = "select 1;\nselect ñx from t";
        assert_eq!(offset_of(t, 2, 8), Some(17));
        assert_eq!(&t[offset_of(t, 2, 9).unwrap()..offset_of(t, 2, 9).unwrap() + 1], "x");
        assert_eq!(offset_of(t, 3, 1), None);
    }

    #[test]
    fn errors_shift_to_the_script() {
        let u = &units("select 1;\n\n  select x", &ScriptDialect::generic())[1];
        let e = shift(placed(ScriptError::new("x"), &u.text, Some(1), Some(8)).into(), u);
        let Error::Statement(se) = e else { panic!() };
        assert_eq!((se.line, se.offset), (Some(3), Some(20)));
    }
}
