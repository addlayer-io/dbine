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

/// A refused statement, placed by the parser's `line L:C` (C from 0).
pub fn error(message: &str, stmt: &str) -> Error {
    let at = message.find("line ").and_then(|i| {
        let (l, c) = message[i + 5..].split_once(':')?;
        let c: String = c.chars().take_while(char::is_ascii_digit).collect();
        Some((l.parse().ok()?, c.parse::<u32>().ok()? + 1))
    });
    placed(ScriptError::new(message), stmt, at).into()
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::sql::{split_script, ScriptDialect};

    #[test]
    fn errors_are_placed() {
        let s = "show databases;\nselect * fron root.x";
        let u = &split_script(s, &ScriptDialect::generic())[1];
        let Error::Statement(e) = shift(error("line 1:14 no viable alternative at input 'select * fron root'", &u.text), u) else { panic!() };
        assert_eq!((e.line, e.offset), (Some(2), Some(s.find("root.x").unwrap())));
        let Error::Statement(e) = error("Unsupported datatype: NOPE", "x") else { panic!() };
        assert_eq!((e.line, e.offset), (None, None));
    }
}
