//! DAX tokens (spec §7.4, §8.1). Owned by part B; part C reads them for
//! the dependency scan and the code search (F12: the generic SQL lexer
//! reads `'Sales'` as a string and doesn't see `'T'[c]` as a column).
//!
//! - `"…"` strings, a quote doubled (`""`); no backslash escapes.
//! - `'…'` table names, `''` inside.
//! - `[…]` column and measure names, `]]` inside.
//! - `//` and `--` line comments (up to the `\n`, which isn't part of
//!   them), `/* */` block comments (not nested: the first `*/` closes).
//! - Numbers (`1`, `1.5`, `.5`, `1e3`), words (identifiers, keywords,
//!   function names, dotted ones such as `INFO.VIEW.TABLES` and
//!   `STDEVX.S`), punctuation and operators.
//! - An unclosed string, name, ref or block comment runs to the end.
//!
//! Every byte of the text belongs to exactly one token (spaces too), so
//! `start` offsets index the original text and the tokens concatenate back
//! to it. Whitespace is what `char::is_whitespace` says, plus the BOM
//! (`U+FEFF`), so a unit never starts with one. Never panics on any input
//! (fuzzed, S7).

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TokenKind {
    /// An identifier, keyword or function name (dotted names whole).
    Word,
    /// `'Table name'`.
    Table,
    /// `[Column or measure]`.
    Ref,
    /// `"text"`.
    String,
    Number,
    Comment,
    /// Operators and punctuation (`(`, `,`, `&&`, `<>`…).
    Punct,
    /// Whitespace.
    Space,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Token<'a> {
    pub kind: TokenKind,
    /// The token as written (quotes included).
    pub text: &'a str,
    /// Byte offset in the text.
    pub start: usize,
}

impl Token<'_> {
    /// Byte offset just past the token.
    pub fn end(&self) -> usize {
        self.start + self.text.len()
    }

    /// A word equal to `w`, ignoring ASCII case.
    pub fn is_word(&self, w: &str) -> bool {
        self.kind == TokenKind::Word && self.text.eq_ignore_ascii_case(w)
    }

    /// Punctuation equal to `p`.
    pub fn is_punct(&self, p: &str) -> bool {
        self.kind == TokenKind::Punct && self.text == p
    }

    /// Spaces and comments: what a reader skips.
    pub fn is_trivia(&self) -> bool {
        matches!(self.kind, TokenKind::Space | TokenKind::Comment)
    }
}

/// Whitespace as the splitter reads it.
pub fn is_space(c: char) -> bool {
    c.is_whitespace() || c == '\u{feff}'
}

fn word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// The byte offset after a quoted token that starts at `i` with `open`
/// and closes with `close` (a doubled `close` is an escaped one).
fn quoted_end(b: &[u8], i: usize, close: u8) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if b[j] == close {
            if b.get(j + 1) == Some(&close) {
                j += 2;
                continue;
            }
            return j + 1;
        }
        j += 1;
    }
    b.len()
}

/// The tokens of `text`, in order.
pub fn tokenize(text: &str) -> Vec<Token<'_>> {
    let b = text.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        // `i` is always at a char boundary: every branch ends at one.
        let c = text[i..].chars().next().unwrap_or(' ');
        let (kind, end) = if is_space(c) {
            let mut j = i;
            for ch in text[i..].chars() {
                if !is_space(ch) {
                    break;
                }
                j += ch.len_utf8();
            }
            (TokenKind::Space, j)
        } else if text[i..].starts_with("//") || text[i..].starts_with("--") {
            let end = text[i..].find('\n').map_or(b.len(), |n| i + n);
            (TokenKind::Comment, end)
        } else if text[i..].starts_with("/*") {
            let end = text[i + 2..].find("*/").map_or(b.len(), |n| i + 2 + n + 2);
            (TokenKind::Comment, end)
        } else if c == '"' {
            (TokenKind::String, quoted_end(b, i, b'"'))
        } else if c == '\'' {
            (TokenKind::Table, quoted_end(b, i, b'\''))
        } else if c == '[' {
            (TokenKind::Ref, quoted_end(b, i, b']'))
        } else if c.is_ascii_digit() || (c == '.' && b.get(i + 1).is_some_and(u8::is_ascii_digit)) {
            (TokenKind::Number, number_end(b, i))
        } else if word_start(c) {
            let mut j = i;
            for ch in text[i..].chars() {
                if !word_char(ch) {
                    break;
                }
                j += ch.len_utf8();
            }
            (TokenKind::Word, j)
        } else {
            let two = text.get(i..i + 2).unwrap_or("");
            let len = if ["&&", "||", "==", "<>", "<=", ">="].contains(&two) {
                2
            } else {
                c.len_utf8()
            };
            (TokenKind::Punct, i + len)
        };
        out.push(Token {
            kind,
            text: &text[i..end],
            start: i,
        });
        i = end;
    }
    out
}

/// `1`, `1.5`, `.5`, `1e3`, `1.5E-3` (ASCII only, so the end is a char
/// boundary).
fn number_end(b: &[u8], i: usize) -> usize {
    let digits = |mut j: usize| {
        while j < b.len() && b[j].is_ascii_digit() {
            j += 1;
        }
        j
    };
    let mut j = digits(i);
    if b.get(j) == Some(&b'.') {
        j = digits(j + 1);
    }
    if matches!(b.get(j), Some(b'e' | b'E')) {
        let mut k = j + 1;
        if matches!(b.get(k), Some(b'+' | b'-')) {
            k += 1;
        }
        if b.get(k).is_some_and(u8::is_ascii_digit) {
            j = digits(k);
        }
    }
    j
}

/// A `'…'` or `[…]` token's name, its doubled quote undone. Anything else
/// comes back as it is.
pub fn unquote(token: &str) -> String {
    let (open, close) = match token.chars().next() {
        Some('\'') => ('\'', '\''),
        Some('[') => ('[', ']'),
        Some('"') => ('"', '"'),
        _ => return token.to_string(),
    };
    let inner = &token[open.len_utf8()..];
    let inner = if inner.len() >= close.len_utf8() && inner.ends_with(close) {
        // A closed token ends with its quote; an unclosed one doesn't.
        let mut doubled = 0;
        for ch in inner.chars().rev() {
            if ch == close {
                doubled += 1;
            } else {
                break;
            }
        }
        // An odd run of quotes at the end: the last one closes.
        if doubled % 2 == 1 {
            &inner[..inner.len() - close.len_utf8()]
        } else {
            inner
        }
    } else {
        inner
    };
    let pair: String = [close, close].iter().collect();
    inner.replace(&pair, &close.to_string())
}

/// The tokens that aren't spaces or comments.
pub fn code(text: &str) -> Vec<Token<'_>> {
    tokenize(text)
        .into_iter()
        .filter(|t| !t.is_trivia())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(text: &str) -> Vec<(TokenKind, &str)> {
        tokenize(text)
            .into_iter()
            .map(|t| (t.kind, t.text))
            .collect()
    }

    #[test]
    fn every_construct() {
        use TokenKind::*;
        assert_eq!(
            kinds("EVALUATE 'Sales d''Or'[Total [neto]]] \"a\"\"b\" 1.5e3 -- c\n// d\n/* e */INFO.VIEW.TABLES() && x<>y"),
            vec![
                (Word, "EVALUATE"),
                (Space, " "),
                (Table, "'Sales d''Or'"),
                (Ref, "[Total [neto]]]"),
                (Space, " "),
                (String, "\"a\"\"b\""),
                (Space, " "),
                (Number, "1.5e3"),
                (Space, " "),
                (Comment, "-- c"),
                (Space, "\n"),
                (Comment, "// d"),
                (Space, "\n"),
                (Comment, "/* e */"),
                (Word, "INFO.VIEW.TABLES"),
                (Punct, "("),
                (Punct, ")"),
                (Space, " "),
                (Punct, "&&"),
                (Space, " "),
                (Word, "x"),
                (Punct, "<>"),
                (Word, "y"),
            ]
        );
        assert_eq!(unquote("'Sales d''Or'"), "Sales d'Or");
        assert_eq!(unquote("[Total [neto]]]"), "Total [neto]");
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote("'unclosed"), "unclosed");
        assert_eq!(unquote("Word"), "Word");
    }

    #[test]
    fn keywords_inside_strings_names_refs_and_comments_are_not_words() {
        for text in [
            "\"EVALUATE\"",
            "'EVALUATE'",
            "[EVALUATE]",
            "// EVALUATE",
            "-- EVALUATE",
            "/* EVALUATE */",
            "/* unclosed EVALUATE",
            "\"unclosed EVALUATE",
        ] {
            assert!(
                !tokenize(text).iter().any(|t| t.is_word("evaluate")),
                "{text}"
            );
        }
        // A bare CR doesn't end a line comment (as the app's guards read it).
        assert!(!tokenize("-- x\rEVALUATE")
            .iter()
            .any(|t| t.is_word("evaluate")));
        assert!(tokenize("-- x\r\nEVALUATE")
            .iter()
            .any(|t| t.is_word("evaluate")));
        // Words end at their boundary.
        assert!(!tokenize("EVALUATEX").iter().any(|t| t.is_word("evaluate")));
    }

    #[test]
    fn tokens_cover_every_byte_and_never_panic() {
        let samples = [
            "",
            "ñ'á'[é]\"ü\"",
            "\u{feff}\u{a0}EVALUATE",
            "'",
            "[",
            "\"",
            "/*",
            "/",
            "-",
            "1.",
            ".5",
            "1e",
            "1e+",
            "日本語 [列] '表'",
            "a]]b''c\"\"d",
        ];
        for s in samples {
            let toks = tokenize(s);
            let joined: String = toks.iter().map(|t| t.text).collect();
            assert_eq!(joined, s);
            let mut at = 0;
            for t in &toks {
                assert_eq!(t.start, at);
                at = t.end();
            }
        }
        // Every prefix of a tricky text, cut at each char boundary.
        let tricky = "DEFINE VAR x = \"a\"\"b\" // c\nEVALUATE { 'T''x'[c]]d], 1.5e-3 } /* é */";
        for (i, _) in tricky.char_indices() {
            let toks = tokenize(&tricky[..i]);
            let joined: String = toks.iter().map(|t| t.text).collect();
            assert_eq!(joined, &tricky[..i]);
        }
        assert_eq!(tokenize("\u{feff}x")[0].kind, TokenKind::Space);
    }
}
