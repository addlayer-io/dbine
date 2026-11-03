//! A q script cut into statements as kdb+ loads a script file (`\l`):
//!
//! - a line that starts with a blank continues the statement above it;
//! - a line that starts with `/` is a comment; a line that is only `/`
//!   opens a comment block, closed by a line that is only `\`;
//! - a line that is only `\` outside a block ends the script: what follows
//!   is ignored;
//! - an empty line ends the statement above it.
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
    for (i, raw) in text.split_inclusive('\n').enumerate() {
        let line_no = i as u32 + 1;
        let start = at;
        at += raw.len();
        let line = raw.trim_end_matches(['\n', '\r']);
        let end = start + line.len();
        let bare = line.trim_end();
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
                let lead = line.len() - line.trim_start().len();
                open = Some((start + lead, end.min(start + bare.len()), line_no));
            }
        }
    }
    close(&mut open, &mut out);
    out
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
