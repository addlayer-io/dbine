//! A DAX tokenizer for the lint and the optimizer's DAX rewrites. The app's
//! release build doesn't compile the Power BI driver in (it is a
//! downloadable host), so the driver's lexer
//! (`crates/drivers/powerbi/src/dax/lexer.rs`) can't be used here; this
//! one reads the text the same way:
//!
//! - `"…"` strings, a quote doubled (`""`); no backslash escapes.
//! - `'…'` table names, `''` inside.
//! - `[…]` column and measure names, `]]` inside.
//! - `//` and `--` line comments, `/* */` block comments (not nested).
//! - Numbers (`1`, `1.5`, `.5`, `1e3`), words (dotted names whole:
//!   `INFO.TABLES`, `STDEVX.S`), punctuation (`&&`, `||`, `==`, `<>`, `<=`
//!   and `>=` are one token).
//! - An unclosed string, name or comment runs to the end.
//!
//! Spaces and comments are dropped: what's inside them is never read as
//! code.

use std::ops::Range;

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum K {
    /// An identifier, keyword or function name.
    Word,
    /// `'Table name'`.
    Table,
    /// `[Column or measure]`.
    Ref,
    Str,
    Num,
    Punct,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Tok<'a> {
    pub k: K,
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
    /// How many `(` and `{` are open around it (a bracket counts outside
    /// itself).
    pub depth: u32,
}

impl Tok<'_> {
    /// A word equal to `w`, ignoring case.
    pub fn is(&self, w: &str) -> bool {
        self.k == K::Word && self.text.eq_ignore_ascii_case(w)
    }

    pub fn p(&self, s: &str) -> bool {
        self.k == K::Punct && self.text == s
    }

    /// The name, unquoted and lowercased (DAX names ignore case): `'My
    /// Table'` → `my table`, `[Sales]` → `sales`, `Sales` → `sales`.
    pub fn name(&self) -> String {
        let t = self.text;
        let (close, inner) = match self.k {
            K::Table => ('\'', t.get(1..).unwrap_or("")),
            K::Ref => (']', t.get(1..).unwrap_or("")),
            K::Str => ('"', t.get(1..).unwrap_or("")),
            _ => return t.to_lowercase(),
        };
        let inner = inner.strip_suffix(close).unwrap_or(inner);
        let doubled: String = [close, close].iter().collect();
        inner.replace(&doubled, &close.to_string()).to_lowercase()
    }
}

fn word_start(c: char) -> bool {
    c.is_alphabetic() || c == '_'
}

fn word_char(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '.'
}

/// The byte offset after a quoted token that starts at `i` and closes
/// with `close` (doubled, it is an escaped one).
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

/// `1`, `1.5`, `.5`, `1e3`, `1.5E-3`.
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

pub(crate) fn tokens(s: &str) -> Vec<Tok<'_>> {
    let b = s.as_bytes();
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    let mut out = Vec::new();
    let mut depth = 0u32;
    let mut i = 0;
    while i < b.len() {
        // `i` is always at a char boundary: every branch ends at one.
        let c = s[i..].chars().next().unwrap_or(' ');
        if c.is_whitespace() || c == '\u{feff}' {
            i += c.len_utf8();
            continue;
        }
        if (c == '/' && at(i + 1) == b'/') || (c == '-' && at(i + 1) == b'-') {
            i = s[i..].find('\n').map_or(b.len(), |p| i + p);
            continue;
        }
        if c == '/' && at(i + 1) == b'*' {
            i = s[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            continue;
        }
        let start = i;
        let k = if c == '"' {
            i = quoted_end(b, i, b'"');
            K::Str
        } else if c == '\'' {
            i = quoted_end(b, i, b'\'');
            K::Table
        } else if c == '[' {
            i = quoted_end(b, i, b']');
            K::Ref
        } else if c.is_ascii_digit() || (c == '.' && at(i + 1).is_ascii_digit()) {
            i = number_end(b, i);
            K::Num
        } else if word_start(c) {
            for ch in s[i..].chars() {
                if !word_char(ch) {
                    break;
                }
                i += ch.len_utf8();
            }
            K::Word
        } else {
            let two = s.get(i..i + 2).unwrap_or("");
            i += if ["&&", "||", "==", "<>", "<=", ">="].contains(&two) {
                2
            } else {
                c.len_utf8()
            };
            K::Punct
        };
        let text = &s[start..i];
        if k == K::Punct && matches!(text, ")" | "}") {
            depth = depth.saturating_sub(1);
        }
        out.push(Tok {
            k,
            text,
            start,
            end: i,
            depth,
        });
        if k == K::Punct && matches!(text, "(" | "{") {
            depth += 1;
        }
    }
    out
}

/// A tokenized script with its brackets matched.
pub(crate) struct Dax<'a> {
    pub src: &'a str,
    pub t: Vec<Tok<'a>>,
    /// For a `(` or `{`, the index of the bracket that closes it
    /// (`t.len()` when none does).
    close: Vec<usize>,
}

impl<'a> Dax<'a> {
    pub fn new(src: &'a str) -> Self {
        let t = tokens(src);
        let mut close = vec![t.len(); t.len()];
        let mut open = Vec::new();
        for (i, x) in t.iter().enumerate() {
            if x.p("(") || x.p("{") {
                open.push(i);
            } else if x.p(")") || x.p("}") {
                if let Some(o) = open.pop() {
                    close[o] = i;
                }
            }
        }
        Self { src, t, close }
    }

    pub fn is(&self, i: usize, w: &str) -> bool {
        self.t.get(i).is_some_and(|x| x.is(w))
    }

    pub fn p(&self, i: usize, s: &str) -> bool {
        self.t.get(i).is_some_and(|x| x.p(s))
    }

    /// `NAME(` at `i`: the index of its `(`.
    pub fn call(&self, i: usize) -> Option<usize> {
        (self.t.get(i)?.k == K::Word && self.p(i + 1, "(")).then_some(i + 1)
    }

    /// The bracket closing the one at `open` (`t.len()` when unclosed).
    pub fn closing(&self, open: usize) -> usize {
        self.close[open]
    }

    /// The arguments of the call whose `(` is at `open`, as token ranges
    /// (an empty argument is an empty range).
    pub fn args(&self, open: usize) -> Vec<Range<usize>> {
        let end = self.closing(open);
        let d = self.t[open].depth + 1;
        let mut out = Vec::new();
        let mut a = open + 1;
        for j in open + 1..end {
            if self.t[j].depth == d && self.t[j].p(",") {
                out.push(a..j);
                a = j + 1;
            }
        }
        if a < end || !out.is_empty() {
            out.push(a..end);
        }
        out
    }

    /// The tokens `r` are exactly one closed call: the index of its name.
    pub fn whole_call(&self, r: &Range<usize>) -> Option<usize> {
        let open = self.call(r.start)?;
        (r.end >= 3 && self.closing(open) == r.end - 1).then_some(r.start)
    }

    /// The name of the call at `i`, uppercased.
    pub fn fname(&self, i: usize) -> String {
        self.t[i].text.to_ascii_uppercase()
    }

    /// The text of the tokens `r` as written (empty for an empty range).
    pub fn text(&self, r: &Range<usize>) -> &'a str {
        if r.is_empty() {
            return "";
        }
        &self.src[self.t[r.start].start..self.t[r.end - 1].end]
    }

    /// The table naming the reference at `i` (`'T'[c]`, `T[c]`): its index.
    pub fn table_of(&self, i: usize) -> Option<usize> {
        let x = self.t.get(i).filter(|x| x.k == K::Ref)?;
        let prev = self.t.get(i.checked_sub(1)?)?;
        let qualifies = match prev.k {
            K::Table => true,
            // `Sales[Amount]`: written together (`RETURN [M]` isn't one).
            K::Word => prev.end == x.start,
            _ => false,
        };
        qualifies.then_some(i - 1)
    }

    /// The tokens `r` are one column or measure reference (`[c]`, `T[c]`,
    /// `'T'[c]`): the index of its `[…]`.
    pub fn reference(&self, r: &Range<usize>) -> Option<usize> {
        match r.len() {
            1 if self.t[r.start].k == K::Ref => Some(r.start),
            2 if self.table_of(r.start + 1) == Some(r.start) => Some(r.start + 1),
            _ => None,
        }
    }

    /// The same code, ignoring case, spacing, comments and table quotes.
    pub fn same(&self, a: &Range<usize>, b: &Range<usize>) -> bool {
        let named = |k: K| matches!(k, K::Word | K::Table);
        a.len() == b.len()
            && a.clone().zip(b.clone()).all(|(i, j)| {
                let (x, y) = (&self.t[i], &self.t[j]);
                if (named(x.k) && named(y.k)) || (x.k == K::Ref && y.k == K::Ref) {
                    x.name() == y.name()
                } else {
                    x.k == y.k && x.text == y.text
                }
            })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(s: &str) -> Vec<(K, &str)> {
        tokens(s).into_iter().map(|t| (t.k, t.text)).collect()
    }

    #[test]
    fn reads_names_strings_and_comments() {
        assert_eq!(
            kinds("EVALUATE 'My ''T'''[A]]b] // x\n\"s\"\"t\" -- y\n/* z */ 1.5e3"),
            vec![
                (K::Word, "EVALUATE"),
                (K::Table, "'My ''T'''"),
                (K::Ref, "[A]]b]"),
                (K::Str, "\"s\"\"t\""),
                (K::Num, "1.5e3"),
            ]
        );
        assert_eq!(
            kinds("INFO.TABLES() <= .5 && x <> y"),
            vec![
                (K::Word, "INFO.TABLES"),
                (K::Punct, "("),
                (K::Punct, ")"),
                (K::Punct, "<="),
                (K::Num, ".5"),
                (K::Punct, "&&"),
                (K::Word, "x"),
                (K::Punct, "<>"),
                (K::Word, "y"),
            ]
        );
        let t = tokens("'My ''T'''[A]]b]");
        assert_eq!(t[0].name(), "my 't'");
        assert_eq!(t[1].name(), "a]b");
    }

    #[test]
    fn unclosed_runs_to_the_end_and_never_panics() {
        for s in [
            "\"abc",
            "'T",
            "[c",
            "/* x",
            "ñ'",
            "[ñ",
            "\u{feff}EVALUATE",
            "x /",
            "é1",
        ] {
            let d = Dax::new(s);
            for i in 0..d.t.len() {
                let _ = d.t[i].name();
                let _ = d.table_of(i);
            }
        }
        assert_eq!(kinds("[ñandú"), vec![(K::Ref, "[ñandú")]);
    }

    #[test]
    fn calls_and_arguments() {
        let d = Dax::new("CALCULATE([M], 'T'[c] = 1, FILTER(ALL(T[d]), T[d] > 2), )");
        let args = d.args(1);
        assert_eq!(args.len(), 4);
        assert_eq!(d.text(&args[0]), "[M]");
        assert_eq!(d.text(&args[1]), "'T'[c] = 1");
        assert_eq!(d.text(&args[2]), "FILTER(ALL(T[d]), T[d] > 2)");
        assert!(args[3].is_empty());
        assert!(d.whole_call(&args[2]).is_some());
        assert!(d.whole_call(&args[1]).is_none());
        assert!(d.reference(&args[0]).is_some());
        let empty = Dax::new("f()");
        assert_eq!(empty.args(empty.call(0).unwrap()).len(), 0);
        let d = Dax::new("x RETURN [M] + T[c]");
        assert_eq!(d.table_of(2), None);
        assert_eq!(d.table_of(5), Some(4));
        let d = Dax::new("'Sales'[Amount] sales[AMOUNT]");
        assert!(d.same(&(0..2), &(2..4)));
    }
}
