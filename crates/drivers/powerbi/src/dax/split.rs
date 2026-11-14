//! A DAX script cut into the units the app runs (spec §7.4). Owned by part
//! B.
//!
//! - Top-level `DEFINE` and `EVALUATE` (case-insensitive, outside strings,
//!   names, refs and comments, at a token boundary) cut the script:
//!   `DEFINE` starts a unit; `EVALUATE` starts one unless the current unit
//!   started with `DEFINE` (its definitions serve every `EVALUATE` after
//!   it). `ORDER BY` / `START AT` stay with their `EVALUATE`.
//! - A unit's `kind` is `StatementKind::Block` when it holds more than one
//!   `EVALUATE`, else `StatementKind::Sql`: MCP's approval-free check reads
//!   it (`mcp/reads/allowlist.rs::dax_approval_free`), so it must be exact.
//! - A unit with `DEFINE` and no `EVALUATE` gets `error: "Falta EVALUATE
//!   después de DEFINE"`.
//! - The driver's own commands ([`crate::admin`]): outside a `DEFINE` /
//!   `EVALUATE` unit, a line whose first word is one of
//!   [`crate::admin::ADMIN_VERBS`] is a unit by itself, up to the end of
//!   that line. Inside a DAX unit such a line stays in the unit.
//! - Anything else outside a DAX unit (text that isn't a query) is a unit
//!   of its own up to the next unit, so `execute` refuses it and the app's
//!   guards count it as a write.
//! - `text` is `script[start..end]`, leading and trailing comments and
//!   blank lines left out (the comment above an `EVALUATE` names its
//!   result); comment-only parts are dropped; `line` is 1-based. Never
//!   panics (fuzzed, S7).

use super::lexer::{tokenize, Token, TokenKind};
use dbine_driver::sql::{ScriptStatement, StatementKind};

/// The error of a `DEFINE` without an `EVALUATE`.
pub const DEFINE_WITHOUT_EVALUATE: &str = "Falta EVALUATE después de DEFINE";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// `DEFINE` / `EVALUATE`: whether it started with `DEFINE`, and how
    /// many `EVALUATE`s it has.
    Dax { define: bool, evaluates: usize },
    /// One of the driver's commands (one line).
    Admin,
    /// Text that isn't a query.
    Other,
}

#[derive(Debug, Clone, Copy)]
struct Open {
    start: usize,
    end: usize,
    kind: Kind,
}

fn is_admin_verb(t: &Token<'_>) -> bool {
    t.kind == TokenKind::Word
        && crate::admin::ADMIN_VERBS
            .iter()
            .any(|v| t.text.eq_ignore_ascii_case(v))
}

/// The units of `text`.
pub fn split(text: &str) -> Vec<ScriptStatement> {
    let mut units: Vec<Open> = Vec::new();
    let mut current: Option<Open> = None;
    // Only spaces and comments since the last line break (or the start).
    let mut line_start = true;
    for t in tokenize(text) {
        if t.is_trivia() {
            if t.text.contains('\n') {
                line_start = true;
                if let Some(o) = current.filter(|o| o.kind == Kind::Admin) {
                    units.push(o);
                    current = None;
                }
            }
            continue;
        }
        let starts = |kind| Open {
            start: t.start,
            end: t.end(),
            kind,
        };
        if t.is_word("define") {
            units.extend(current.take());
            current = Some(starts(Kind::Dax {
                define: true,
                evaluates: 0,
            }));
        } else if t.is_word("evaluate") {
            match &mut current {
                Some(Open {
                    kind:
                        Kind::Dax {
                            define: true,
                            evaluates,
                        },
                    end,
                    ..
                }) => {
                    *evaluates += 1;
                    *end = t.end();
                }
                _ => {
                    units.extend(current.take());
                    current = Some(starts(Kind::Dax {
                        define: false,
                        evaluates: 1,
                    }));
                }
            }
        } else {
            match &mut current {
                Some(
                    o @ Open {
                        kind: Kind::Dax { .. } | Kind::Admin,
                        ..
                    },
                ) => o.end = t.end(),
                _ if line_start && is_admin_verb(&t) => {
                    units.extend(current.take());
                    current = Some(starts(Kind::Admin));
                }
                Some(o) => o.end = t.end(),
                None => current = Some(starts(Kind::Other)),
            }
        }
        line_start = false;
    }
    units.extend(current);
    units
        .into_iter()
        .map(|u| {
            let (kind, error) = match u.kind {
                Kind::Dax {
                    define: true,
                    evaluates: 0,
                } => (
                    StatementKind::Sql,
                    Some(DEFINE_WITHOUT_EVALUATE.to_string()),
                ),
                Kind::Dax { evaluates, .. } if evaluates > 1 => (StatementKind::Block, None),
                _ => (StatementKind::Sql, None),
            };
            ScriptStatement {
                text: text[u.start..u.end].to_string(),
                start: u.start,
                end: u.end,
                line: 1 + text[..u.start].bytes().filter(|&b| b == b'\n').count() as u32,
                kind,
                repeat: 1,
                error,
            }
        })
        .collect()
}

/// Where a unit's `DEFINE` and each of its `EVALUATE`s start (byte offsets
/// in the unit), as the splitter reads it.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Parts {
    pub define: Option<usize>,
    pub evaluates: Vec<usize>,
}

/// The `DEFINE` and `EVALUATE` keywords of one unit.
pub fn parts(unit: &str) -> Parts {
    let mut p = Parts::default();
    for t in tokenize(unit) {
        if t.is_word("define") && p.define.is_none() && p.evaluates.is_empty() {
            p.define = Some(t.start);
        } else if t.is_word("evaluate") {
            p.evaluates.push(t.start);
        }
    }
    p
}

/// The 1-based line of byte offset `at` in `text`.
pub fn line_of(text: &str, at: usize) -> u32 {
    let at = at.min(text.len());
    1 + text.as_bytes()[..at]
        .iter()
        .filter(|&&b| b == b'\n')
        .count() as u32
}

#[cfg(test)]
mod tests {
    use super::*;

    fn texts(s: &str) -> Vec<String> {
        split(s).into_iter().map(|u| u.text).collect()
    }

    /// What `mcp/reads/allowlist.rs::dax_approval_free` checks, on this
    /// splitter (the app's test of the same name runs the real driver).
    fn approval_free(q: &str) -> bool {
        let units = split(q);
        let [unit] = units.as_slice() else {
            return false;
        };
        unit.error.is_none()
            && unit.kind == StatementKind::Sql
            && dbine_driver::read_only::dax_is_query(&unit.text)
    }

    #[test]
    fn the_app_check_through_this_splitter() {
        assert!(approval_free("// n\nEVALUATE 'T'"));
        assert!(approval_free(
            "DEFINE MEASURE 'T'[m] = 1\nEVALUATE {[m]}\nORDER BY [m]"
        ));
        assert!(!approval_free("EVALUATE {1}\nEVALUATE {2}"));
        assert!(!approval_free("EVALUATE {1} EVALUATE {2}"));
        assert!(!approval_free("DEFINE VAR x = 1 EVALUATE {x} EVALUATE {x}"));
        assert!(!approval_free(
            "DEFINE VAR x = 1\nEVALUATE {x}\n// b\nEVALUATE {x}"
        ));
        assert!(!approval_free("GRANT Read TO \"ana@contoso.com\""));
        assert!(!approval_free("DEFINE VAR x = 1"));
        assert!(approval_free("EVALUATE { \"EVALUATE\" } // EVALUATE {2}"));
        assert!(approval_free("EVALUATE 'EVALUATE'"));
        assert!(approval_free("EVALUATE {[EVALUATE]}"));
        assert!(approval_free("EVALUATE {1} /* EVALUATE {2} */"));
    }

    #[test]
    fn units_offsets_and_lines() {
        let s = "// first\nEVALUATE 'A'\n\n-- second\nevaluate TOPN(5, 'B')\nORDER BY 'B'[x] DESC\nSTART AT 3\n";
        let u = split(s);
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].text, "EVALUATE 'A'");
        assert_eq!((u[0].start, u[0].line), (9, 2));
        assert_eq!(
            u[1].text,
            "evaluate TOPN(5, 'B')\nORDER BY 'B'[x] DESC\nSTART AT 3"
        );
        assert_eq!(u[1].line, 5);
        for x in &u {
            assert_eq!(&s[x.start..x.end], x.text);
            assert_eq!(x.kind, StatementKind::Sql);
            assert!(x.error.is_none());
        }
        // DEFINE groups its EVALUATEs; another DEFINE starts a unit.
        let s = "DEFINE VAR x = 1\nEVALUATE {x}\n// b\nEVALUATE {x + 1}\nDEFINE MEASURE 'T'[m] = 2 EVALUATE {[m]}\nEVALUATE {3}";
        let u = split(s);
        assert_eq!(
            u.iter().map(|x| x.text.as_str()).collect::<Vec<_>>(),
            [
                "DEFINE VAR x = 1\nEVALUATE {x}\n// b\nEVALUATE {x + 1}\nDEFINE MEASURE 'T'[m] = 2 EVALUATE {[m]}\nEVALUATE {3}"
                    .split("\nDEFINE MEASURE")
                    .next()
                    .unwrap(),
                "DEFINE MEASURE 'T'[m] = 2 EVALUATE {[m]}\nEVALUATE {3}",
            ]
        );
        assert_eq!(u[0].kind, StatementKind::Block);
        assert_eq!(u[1].kind, StatementKind::Block);
        // A DEFINE without EVALUATE.
        let u = split("DEFINE VAR x = 1 // nothing else");
        assert_eq!(u.len(), 1);
        assert_eq!(u[0].text, "DEFINE VAR x = 1");
        assert_eq!(u[0].error.as_deref(), Some(DEFINE_WITHOUT_EVALUATE));
        // Comment-only scripts have no units.
        assert!(split("// a\n/* b */\n-- c").is_empty());
        assert!(split("").is_empty());
    }

    #[test]
    fn the_drivers_commands_are_units_of_one_line() {
        let s = "GRANT Read, Build TO \"ana@contoso.com\" -- why\nrevoke ALL FROM \"x\"\n/* c */ RENAME MODEL \"a\" TO \"b\"\nEVALUATE {1}\nGRANT Read TO \"in the unit\"";
        assert_eq!(
            texts(s),
            [
                "GRANT Read, Build TO \"ana@contoso.com\"",
                "revoke ALL FROM \"x\"",
                "RENAME MODEL \"a\" TO \"b\"",
                // Inside a DAX unit the line stays in the unit.
                "EVALUATE {1}\nGRANT Read TO \"in the unit\"",
            ]
        );
        for u in split(s).iter().take(3) {
            assert!(crate::admin::is_command(&u.text), "{}", u.text);
        }
        // Not at the start of a line: part of the text before it.
        assert_eq!(texts("foo GRANT x"), ["foo GRANT x"]);
        assert_eq!(texts("foo\nGRANT x"), ["foo", "GRANT x"]);
        // EVALUATE cuts a command line too.
        assert_eq!(
            texts("ALTER MODEL \"m\" SET DESCRIPTION \"EVALUATE\" EVALUATE {1}"),
            [
                "ALTER MODEL \"m\" SET DESCRIPTION \"EVALUATE\"",
                "EVALUATE {1}"
            ]
        );
    }

    #[test]
    fn text_that_isnt_a_query_is_a_unit_the_guards_see() {
        let u = split("SELECT * FROM t\nEVALUATE {1}");
        assert_eq!(u.len(), 2);
        assert_eq!(u[0].text, "SELECT * FROM t");
        assert_eq!(
            dbine_driver::read_only::dax_first_write(&u).as_deref(),
            Some("SELECT")
        );
        assert!(dbine_driver::read_only::dax_first_write(&split("EVALUATE {1}")).is_none());
    }

    #[test]
    fn parts_of_a_unit() {
        let unit = "DEFINE VAR x = \"EVALUATE\"\nEVALUATE {x}\nevaluate {1}";
        let p = parts(unit);
        assert_eq!(p.define, Some(0));
        assert_eq!(p.evaluates, vec![26, 39]);
        assert_eq!(&unit[26..34], "EVALUATE");
        assert_eq!(parts("EVALUATE 'T'").define, None);
        assert_eq!(line_of(unit, 39), 3);
    }

    #[test]
    fn never_panics_and_text_is_the_slice() {
        let corpus = [
            "DEFINE",
            "EVALUATE",
            "\u{feff}EVALUATE {1}",
            "'\nEVALUATE",
            "[EVALUATE\n]",
            "/* EVALUATE",
            "GRANT\nGRANT\n",
            "é EVALUATE é",
            "DEFINE VAR x = \"a\nEVALUATE {x}",
        ];
        for s in corpus {
            for (i, _) in s.char_indices().chain([(s.len(), ' ')]) {
                for u in split(&s[..i]) {
                    assert_eq!(&s[..i][u.start..u.end], u.text);
                    assert!(!u.text.is_empty());
                }
            }
        }
        assert_eq!(texts("\u{feff}EVALUATE {1}"), ["EVALUATE {1}"]);
    }
}
