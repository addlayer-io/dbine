//! A small tokenizer for the languages that aren't SQL: the MongoDB shell
//! (JavaScript), Cypher and the JSON bodies of the HTTP consoles. `//` and
//! `/* */` comments; '…' and "…" strings with backslash escapes; `` `…` ``
//! (a string in JS, a name in Cypher); JS regex literals; `#` line comments
//! where the console takes them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum C {
    Word,
    Num,
    Str,
    /// `` `name` `` (Cypher).
    Name,
    /// `/body/flags` (JS).
    Regex,
    Punct,
}

#[derive(Debug, Clone)]
pub(super) struct Tok<'a> {
    pub k: C,
    pub text: &'a str,
    pub start: usize,
    pub end: usize,
    /// Depth of (), [] and {} together.
    pub depth: u32,
}

impl Tok<'_> {
    pub fn is(&self, w: &str) -> bool {
        self.k == C::Word && self.text.eq_ignore_ascii_case(w)
    }

    pub fn p(&self, c: char) -> bool {
        self.k == C::Punct && self.text.len() == c.len_utf8() && self.text.starts_with(c)
    }

    /// A string's content, without its quotes (escapes left as written).
    pub fn body(&self) -> &str {
        let t = self.text;
        if !matches!(self.k, C::Str | C::Name) {
            return t;
        }
        // An unterminated string can end in a multi-byte character: only cut
        // the closing quote when it is there.
        let inner = t.get(1..).unwrap_or("");
        match t.as_bytes()[0] {
            q if t.len() >= 2 && t.as_bytes()[t.len() - 1] == q => &inner[..inner.len() - 1],
            _ => inner,
        }
    }
}

#[derive(Clone, Copy, Default)]
pub(super) struct Options {
    /// `/…/` is a regex where a value goes.
    pub regex: bool,
    /// `` `…` `` is a name, not a string.
    pub backtick_names: bool,
    /// `#` and `//` comment out a line only at its start (consoles).
    pub line_start_comments: bool,
}

fn is_ident(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c >= 0x80
}

pub(super) fn tokens(s: &str, o: Options) -> Vec<Tok<'_>> {
    let b = s.as_bytes();
    let mut out: Vec<Tok> = Vec::new();
    let mut depth = 0u32;
    let mut i = 0;
    let mut at_line_start = true;
    let at = |i: usize| b.get(i).copied().unwrap_or(0);
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            at_line_start |= c == b'\n';
            i += 1;
            continue;
        }
        if o.line_start_comments && at_line_start && (c == b'#' || (c == b'/' && at(i + 1) == b'/')) {
            i = s[i..].find('\n').map_or(b.len(), |p| i + p);
            continue;
        }
        if !o.line_start_comments && c == b'/' && at(i + 1) == b'/' {
            i = s[i..].find('\n').map_or(b.len(), |p| i + p);
            continue;
        }
        if c == b'/' && at(i + 1) == b'*' {
            i = s[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            continue;
        }
        let start = i;
        at_line_start = false;
        let k = if c == b'\'' || c == b'"' || c == b'`' {
            i += 1;
            while i < b.len() && b[i] != c {
                i += if b[i] == b'\\' { 2 } else { 1 };
            }
            i = (i + 1).min(b.len());
            if c == b'`' && o.backtick_names { C::Name } else { C::Str }
        } else if c == b'/' && o.regex && regex_allowed(out.last()) {
            i += 1;
            let mut class = false;
            while i < b.len() && b[i] != b'\n' && (class || b[i] != b'/') {
                match b[i] {
                    b'\\' => i += 1,
                    b'[' => class = true,
                    b']' => class = false,
                    _ => {}
                }
                i += 1;
            }
            i = (i + 1).min(b.len());
            while i < b.len() && b[i].is_ascii_alphabetic() {
                i += 1;
            }
            C::Regex
        } else if is_ident(c) {
            while i < b.len() && is_ident(b[i]) {
                i += 1;
            }
            if c.is_ascii_digit() { C::Num } else { C::Word }
        } else {
            i += 1;
            while i < b.len() && !s.is_char_boundary(i) {
                i += 1;
            }
            C::Punct
        };
        let text = &s[start..i.min(b.len())];
        if k == C::Punct && matches!(text, ")" | "]" | "}") {
            depth = depth.saturating_sub(1);
        }
        out.push(Tok { k, text, start, end: start + text.len(), depth });
        if k == C::Punct && matches!(text, "(" | "[" | "{") {
            depth += 1;
        }
    }
    out
}

/// A `/` starts a regex where a value is expected, not after one.
fn regex_allowed(prev: Option<&Tok>) -> bool {
    match prev {
        None => true,
        Some(t) => t.k == C::Punct && matches!(t.text, "(" | "," | "=" | ":" | "[" | "!" | "&" | "|" | "?" | "{" | "}" | ";"),
    }
}

/// The index of the bracket closing the one at `open`.
pub(super) fn closing(t: &[Tok], open: usize) -> usize {
    let d = t[open].depth;
    (open + 1..t.len()).find(|&j| t[j].depth == d && t[j].k == C::Punct && matches!(t[j].text, ")" | "]" | "}")).unwrap_or(t.len())
}
