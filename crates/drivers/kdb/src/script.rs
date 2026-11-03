//! A q script cut into statements as kdb+ loads a script file (`\l`):
//!
//! - a line that starts with a blank continues the statement above it;
//! - a line that starts with `/` is a comment; a line that is only `/`
//!   opens a comment block, closed by a line that is only `\`;
//! - a line that is only `\` outside a block ends the script: what follows
//!   is ignored;
//! - an empty line ends the statement above it;
//! - but while a `{`, `[` or `(` is still open (a function, a table
//!   literal… spread over lines), no line ends the statement: blank lines
//!   and comments inside it stay in it. Brackets inside strings and
//!   comments don't count.
//!
//! Each statement goes to the server as one sync query.

use dbine_driver::{ScriptStatement, StatementKind};

pub fn split(text: &str) -> Vec<ScriptStatement> {
    let mut out: Vec<ScriptStatement> = Vec::new();
    let mut open: Option<(usize, usize, u32)> = None; // start, end, line
    let mut block = false;
    let mut at = 0usize;
    let close = |open: &mut Option<(usize, usize, u32)>, out: &mut Vec<ScriptStatement>| {
        if let Some((start, end, line)) = open.take() {
            out.push(ScriptStatement {
                text: text[start..end].to_string(),
                start,
                end,
                line,
                kind: StatementKind::Sql,
                repeat: 1,
                error: None,
            });
        }
    };
    // Brackets still open in the current statement.
    let mut depth = 0usize;
    for (i, raw) in text.split_inclusive('\n').enumerate() {
        let line_no = i as u32 + 1;
        let start = at;
        at += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        let end = start + line.len();
        let bare = line.trim_end();
        if depth > 0 {
            if let Some((_, e, _)) = &mut open {
                if !bare.trim_start().is_empty() {
                    *e = start + bare.len();
                }
                depth = brackets(line, depth);
                continue;
            }
            depth = 0;
        }
        if block {
            if bare == "\\" {
                block = false;
            }
            continue;
        }
        if bare == "/" {
            close(&mut open, &mut out);
            block = true;
            continue;
        }
        if bare == "\\" {
            close(&mut open, &mut out);
            break;
        }
        if bare.trim_start().is_empty() {
            close(&mut open, &mut out);
            continue;
        }
        if line.starts_with('/') {
            close(&mut open, &mut out);
            continue;
        }
        let indented = line.starts_with([' ', '\t']);
        match &mut open {
            Some((_, e, _)) if indented => *e = start + bare.len(),
            _ => {
                close(&mut open, &mut out);
                depth = 0;
                let lead = line.len() - line.trim_start().len();
                open = Some((start + lead, end.min(start + bare.len()), line_no));
            }
        }
        // A system command (`\l`, `\ts f[]`) is one line.
        if !line.trim_start().starts_with('\\') {
            depth = brackets(line, depth);
        }
    }
    close(&mut open, &mut out);
    out
}

/// `depth` after the brackets of `line`: strings (`"a{"`, with `\"`
/// escapes) and comments (a `/` at the start or after a blank) don't
/// count, and a stray closing bracket doesn't go below zero.
fn brackets(line: &str, mut depth: usize) -> usize {
    let mut string = false;
    let mut prev = ' ';
    let mut cs = line.chars();
    while let Some(c) = cs.next() {
        if string {
            match c {
                '\\' => {
                    cs.next();
                }
                '"' => string = false,
                _ => {}
            }
        } else {
            match c {
                '"' => string = true,
                '/' if prev == ' ' || prev == '\t' => break,
                '{' | '[' | '(' => depth += 1,
                '}' | ']' | ')' => depth = depth.saturating_sub(1),
                _ => {}
            }
        }
        prev = c;
    }
    depth
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<(String, usize, u32)> {
        split(s)
            .into_iter()
            .map(|x| (x.text, x.start, x.line))
            .collect()
    }

    #[test]
    fn statements_comments_and_continuations() {
        let s = "a:1\n/ a comment\nf:{[x]\n  x+1}\n\nselect from t\n";
        assert_eq!(
            texts(s),
            vec![
                ("a:1".into(), 0, 1),
                ("f:{[x]\n  x+1}".into(), 16, 3),
                ("select from t".into(), 31, 6),
            ]
        );
    }

    #[test]
    fn an_open_bracket_keeps_blank_lines_and_comments_in_the_statement() {
        let f = "f:{[x]\n  a:x+1;\n\n/ a comment at column 0\n  b:\"}\";  / a } in a comment\n  a}";
        let s = format!("{f}\n\nf 1\n");
        assert_eq!(
            texts(&s),
            vec![(f.to_string(), 0, 1), ("f 1".into(), f.len() + 2, 8)]
        );
        // A table literal over lines, with a blank line inside.
        assert_eq!(
            texts("t:([] a:1 2;\n\n  b:`x`y)\nt")
                .into_iter()
                .map(|x| x.0)
                .collect::<Vec<_>>(),
            ["t:([] a:1 2;\n\n  b:`x`y)", "t"]
        );
        // Brackets in strings, chars and comments don't count.
        assert_eq!(
            texts("s:\"(\"\n\nc:\"\\\"[\"\n\nx:1 / (\n\ny:2")
                .into_iter()
                .map(|x| x.0)
                .collect::<Vec<_>>(),
            ["s:\"(\"", "c:\"\\\"[\"", "x:1 / (", "y:2"]
        );
        // A stray closing bracket doesn't hold the next statements.
        assert_eq!(texts("a)\n\nb")[1].0, "b");
        // Offsets still point into the text.
        for st in split(&s) {
            assert_eq!(&s[st.start..st.end], st.text);
        }
    }

    #[test]
    fn blocks_and_the_end_of_the_script() {
        let s = "1+1\n/\nignored\n\\\n2+2\n\\\n3+3\n";
        assert_eq!(
            texts(s).into_iter().map(|x| x.0).collect::<Vec<_>>(),
            ["1+1", "2+2"]
        );
        // System commands are statements of their own.
        assert_eq!(
            texts("\\v\ntables[]")
                .into_iter()
                .map(|x| x.0)
                .collect::<Vec<_>>(),
            ["\\v", "tables[]"]
        );
        // CRLF and trailing blanks.
        assert_eq!(
            texts("x:1  \r\ny:2\r\n")
                .into_iter()
                .map(|x| x.0)
                .collect::<Vec<_>>(),
            ["x:1", "y:2"]
        );
        // An indented first line still starts a statement.
        assert_eq!(texts("  til 3")[0].0, "til 3");
        assert!(split("/ only a comment\n").is_empty());
        // Offsets point into the original text.
        let s = "a\n  b\nc";
        for st in split(s) {
            assert_eq!(&s[st.start..st.end], st.text);
        }
    }
}
