//! Editor scripts: errors placed in the text the driver got.

use dbine_driver::sql::ScriptStatement;
use dbine_driver::Error;
#[cfg(test)]
use dbine_driver::ScriptError;

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

/// A refused statement, placed at the text its `near "…"` quotes.
pub fn place(e: Error, stmt: &str) -> Error {
    let Error::Statement(mut se) = e else { return e };
    if let Some(i) = se.message.find("near \"") {
        let frag = &se.message[i + 6..];
        let frag = frag.rfind('"').map_or(frag, |j| &frag[..j]);
        if let Some(off) = (!frag.is_empty()).then(|| stmt.find(frag)).flatten() {
            let line = stmt[..off].matches('\n').count() as u32 + 1;
            se.line = Some(line);
            se.offset = Some(off);
        }
    }
    Error::Statement(se)
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::sql::{split_script, ScriptDialect};

    #[test]
    fn errors_are_placed() {
        let s = "show databases;\nselect *\nfron x";
        let u = &split_script(s, &ScriptDialect::generic())[1];
        let e = ScriptError::new("syntax error near \"fron x\"").with_code("0x2600");
        let Error::Statement(e) = shift(place(e.into(), &u.text), u) else { panic!() };
        assert_eq!((e.line, e.offset, e.code.as_deref()), (Some(3), Some(s.find("fron").unwrap()), Some("0x2600")));
        let Error::Statement(e) = place(ScriptError::new("Database not exist").into(), "x") else { panic!() };
        assert_eq!((e.line, e.offset), (None, None));
    }
}
