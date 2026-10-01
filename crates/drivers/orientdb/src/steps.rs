//! Editor bookkeeping for the statements this driver splits itself: each
//! one's results carry its place in the script and its time, and its
//! error the script offset and line, as when the app splits the script.

use dbine_driver::{Error, QueryOutcome, ScriptError};
use std::time::Instant;

/// A statement of the text `execute` got, while it runs.
pub struct Step {
    /// The app handed over the text without numbering its statements
    /// (`Whole`): this driver numbers them.
    own: bool,
    r0: usize,
    /// Byte offset and 1-based line of the statement in that text.
    offset: usize,
    line: u32,
    started: Instant,
}

impl Step {
    /// Statement `index` starts. `own`: `out.current_statement` was `None`
    /// when `execute` began.
    pub fn start(out: &mut QueryOutcome, own: bool, index: usize, offset: usize, line: u32) -> Self {
        if own {
            out.current_statement = Some(index);
        }
        Step { own, r0: out.results.len(), offset, line, started: Instant::now() }
    }

    /// It ended: its results get its number, place and time.
    pub fn finish(&self, out: &mut QueryOutcome) {
        if !self.own {
            return;
        }
        let ms = self.started.elapsed().as_millis() as u64;
        let index = out.current_statement;
        let from = self.r0.min(out.results.len());
        for r in &mut out.results[from..] {
            r.statement = index;
            r.offset = Some(self.offset);
            r.line = Some(self.line);
            r.elapsed_ms.get_or_insert(ms);
        }
    }

    /// Its failure, placed in the text: an offset or line the driver gave
    /// relative to the statement becomes one of the text; without them,
    /// the statement's start. Connection, cancel and other errors that end
    /// the script stay as they are.
    pub fn place(&self, e: Error) -> Error {
        let mut se = match e {
            Error::Query(m) | Error::Unsupported(m) => ScriptError::new(m),
            Error::Statement(se) => *se,
            other => return other,
        };
        se.offset = Some(self.offset + se.offset.unwrap_or(0));
        se.line = Some(se.line.map_or(self.line, |l| self.line + l.saturating_sub(1)));
        Error::Statement(Box::new(se))
    }
}

/// Byte offset of 1-based `line` / char `column` in `text` (a server's
/// position inside the statement it got).
#[allow(dead_code)]
pub fn offset_of(text: &str, line: u32, column: u32) -> usize {
    let mut at = 0;
    for _ in 1..line.max(1) {
        match text[at..].find('\n') {
            Some(n) => at += n + 1,
            None => return text.len(),
        }
    }
    let rest = &text[at..];
    at + rest.char_indices().nth(column.saturating_sub(1) as usize).map_or(rest.len(), |(i, _)| i)
}

/// 1-based line of byte `offset` in `text`.
#[allow(dead_code)]
pub fn line_at(text: &str, offset: usize) -> u32 {
    text.as_bytes()[..offset.min(text.len())].iter().filter(|&&c| c == b'\n').count() as u32 + 1
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn steps_place_results_and_errors() {
        let mut out = QueryOutcome::default();
        let s = Step::start(&mut out, true, 2, 10, 3);
        out.push_affected(1);
        s.finish(&mut out);
        let r = &out.results[0];
        assert_eq!((r.statement, r.offset, r.line), (Some(2), Some(10), Some(3)));
        assert!(r.elapsed_ms.is_some());
        let e = s.place(Error::Query("x".into())).to_script_error();
        assert_eq!((e.offset, e.line, e.message.as_str()), (Some(10), Some(3), "x"));
        let e = s.place(ScriptError::new("y").at_offset(4).at_line(2).into()).to_script_error();
        assert_eq!((e.offset, e.line), (Some(14), Some(4)));
        assert!(matches!(s.place(Error::Cancelled), Error::Cancelled));
        // Inside an app-numbered run nothing is renumbered.
        let mut out = QueryOutcome::default();
        let s = Step::start(&mut out, false, 5, 0, 1);
        out.push_affected(1);
        s.finish(&mut out);
        assert_eq!((out.current_statement, out.results[0].statement), (None, None));
        assert_eq!(offset_of("ab\ncdé f", 2, 4), 7);
        assert_eq!(offset_of("abc", 1, 1), 0);
        assert_eq!(line_at("a\nb\nc", 4), 3);
    }
}
