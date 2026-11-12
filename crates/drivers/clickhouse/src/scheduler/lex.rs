//! A small lexer for ClickHouse's SQL: enough to read what the server
//! prints (`SHOW CREATE TABLE`, `SHOW CREATE TASK`) and to check that a
//! fragment from a form (an engine clause, a column list, a query) is
//! closed: its strings, quoted names and comments end, its parentheses
//! balance, and no `;` ends the statement early.

/// What a token is.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum T {
    /// A keyword, a bare name or a number.
    Word,
    /// `` `name` `` or `"name"`.
    Ident,
    /// `'text'` or a heredoc (`$tag$ … $tag$`).
    Str,
    /// One other character: `(`, `)`, `,`, `.`, `=`, `;`, `-`…
    Punct,
    /// `-- …`, `# …` or `/* … */`.
    Comment,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Tok {
    pub(crate) kind: T,
    pub(crate) start: usize,
    pub(crate) end: usize,
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c >= 0x80
}

/// The end of a quoted run that starts at `i` (backslash escapes and the
/// quote doubled); `None` when it doesn't end.
fn quoted(b: &[u8], i: usize, q: u8) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() {
        match b[j] {
            b'\\' => j += 2,
            c if c == q => {
                if b.get(j + 1) == Some(&q) {
                    j += 2;
                } else {
                    return Some(j + 1);
                }
            }
            _ => j += 1,
        }
    }
    None
}

/// The tokens of `s`; `None` when a string, a quoted name, a heredoc or a
/// block comment doesn't end.
pub(crate) fn lex(s: &str) -> Option<Vec<Tok>> {
    let b = s.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    while i < b.len() {
        let c = b[i];
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        let start = i;
        let kind = match c {
            b'-' if b.get(i + 1) == Some(&b'-') => {
                i = b[i..]
                    .iter()
                    .position(|c| *c == b'\n')
                    .map_or(b.len(), |p| i + p);
                T::Comment
            }
            b'#' if matches!(b.get(i + 1), Some(b' ' | b'!')) => {
                i = b[i..]
                    .iter()
                    .position(|c| *c == b'\n')
                    .map_or(b.len(), |p| i + p);
                T::Comment
            }
            b'/' if b.get(i + 1) == Some(&b'*') => {
                let mut depth = 0usize;
                loop {
                    if i + 1 >= b.len() {
                        return None;
                    }
                    if b[i] == b'/' && b[i + 1] == b'*' {
                        depth += 1;
                        i += 2;
                    } else if b[i] == b'*' && b[i + 1] == b'/' {
                        depth -= 1;
                        i += 2;
                        if depth == 0 {
                            break;
                        }
                    } else {
                        i += 1;
                    }
                }
                T::Comment
            }
            b'\'' => {
                i = quoted(b, i, b'\'')?;
                T::Str
            }
            b'`' | b'"' => {
                i = quoted(b, i, c)?;
                T::Ident
            }
            b'$' => {
                let tag_end = b[i + 1..]
                    .iter()
                    .position(|c| !(c.is_ascii_alphanumeric() || *c == b'_'))
                    .map(|p| i + 1 + p);
                match tag_end.filter(|e| b[*e] == b'$') {
                    Some(e) => {
                        let tag = &s[i..=e];
                        let close = s[e + 1..].find(tag)?;
                        i = e + 1 + close + tag.len();
                        T::Str
                    }
                    None => {
                        i += 1;
                        T::Punct
                    }
                }
            }
            c if word_byte(c) => {
                while i < b.len() && word_byte(b[i]) {
                    i += 1;
                }
                T::Word
            }
            _ => {
                i += 1;
                T::Punct
            }
        };
        out.push(Tok {
            kind,
            start,
            end: i,
        });
    }
    Some(out)
}

/// A quoted name or string without its quotes, its escapes read.
pub(crate) fn unquote(text: &str) -> String {
    let mut chars = text.chars();
    let Some(q) = chars.next() else {
        return String::new();
    };
    if q == '$' {
        // A heredoc: `$tag$text$tag$`, no escapes.
        let Some(open) = text[1..].find('$').map(|p| p + 2) else {
            return String::new();
        };
        let close = text.len().saturating_sub(open).max(open);
        return text.get(open..close).unwrap_or_default().to_string();
    }
    let inner: Vec<char> = chars.collect();
    let inner = &inner[..inner.len().saturating_sub(1)];
    let mut out = String::new();
    let mut i = 0;
    while i < inner.len() {
        let c = inner[i];
        if c == '\\' && i + 1 < inner.len() {
            i += 1;
            match inner[i] {
                'n' => out.push('\n'),
                't' => out.push('\t'),
                'r' => out.push('\r'),
                '0' => out.push('\0'),
                'b' => out.push('\u{8}'),
                'f' => out.push('\u{c}'),
                'a' => out.push('\u{7}'),
                'v' => out.push('\u{b}'),
                'x' if i + 2 < inner.len() => {
                    let hex: String = inner[i + 1..i + 3].iter().collect();
                    match u8::from_str_radix(&hex, 16) {
                        Ok(n) => {
                            out.push(char::from(n));
                            i += 2;
                        }
                        Err(_) => out.push('x'),
                    }
                }
                other => out.push(other),
            }
        } else if c == q && inner.get(i + 1) == Some(&q) {
            out.push(q);
            i += 1;
        } else {
            out.push(c);
        }
        i += 1;
    }
    out
}

/// The text with its tokens joined by single spaces (comments kept): two
/// texts that differ only in their spacing read the same.
pub(crate) fn normalized(s: &str) -> Option<String> {
    let toks = lex(s)?;
    Some(
        toks.iter()
            .map(|t| &s[t.start..t.end])
            .collect::<Vec<_>>()
            .join(" "),
    )
}

/// Whether two texts are the same SQL but for their spacing.
pub(crate) fn same_sql(a: &str, b: &str) -> bool {
    match (normalized(a), normalized(b)) {
        (Some(a), Some(b)) => a == b,
        _ => a.trim() == b.trim(),
    }
}

/// A fragment of a statement from a form is closed: it lexes whole, its
/// parentheses balance and no `;` ends the statement inside it.
pub(crate) fn closed(s: &str) -> bool {
    let Some(toks) = lex(s) else { return false };
    let mut depth = 0i64;
    for t in &toks {
        if t.kind != T::Punct {
            continue;
        }
        match &s[t.start..t.end] {
            "(" | "[" => depth += 1,
            ")" | "]" => {
                depth -= 1;
                if depth < 0 {
                    return false;
                }
            }
            ";" => return false,
            _ => {}
        }
    }
    depth == 0
}

/// A query from a form without the `;` (and the spaces) at its end.
pub(crate) fn trim_query(s: &str) -> String {
    let mut q = s.trim();
    while let Some(rest) = q.strip_suffix(';') {
        q = rest.trim_end();
    }
    q.to_string()
}

/// Each line trimmed, the empty ones at the start and the end dropped (a
/// clause the server printed indented on several lines).
pub(crate) fn dedent(s: &str) -> String {
    let lines: Vec<&str> = s.lines().map(str::trim).collect();
    let first = lines
        .iter()
        .position(|l| !l.is_empty())
        .unwrap_or(lines.len());
    let last = lines
        .iter()
        .rposition(|l| !l.is_empty())
        .map_or(0, |p| p + 1);
    if first >= last {
        return String::new();
    }
    lines[first..last].join("\n")
}

/// Reads tokens (comments skipped) of a statement the server printed.
pub(crate) struct Parser<'a> {
    pub(crate) s: &'a str,
    pub(crate) t: Vec<Tok>,
    pub(crate) i: usize,
}

impl<'a> Parser<'a> {
    pub(crate) fn new(s: &'a str) -> Option<Self> {
        let t = lex(s)?
            .into_iter()
            .filter(|t| t.kind != T::Comment)
            .collect();
        Some(Parser { s, t, i: 0 })
    }

    pub(crate) fn text_at(&self, i: usize) -> &'a str {
        self.t.get(i).map_or("", |t| &self.s[t.start..t.end])
    }

    pub(crate) fn kind_at(&self, i: usize) -> Option<T> {
        self.t.get(i).map(|t| t.kind)
    }

    pub(crate) fn done(&self) -> bool {
        self.i >= self.t.len()
    }

    /// The keyword `kw` at `i` (a bare word, any case).
    pub(crate) fn is_kw_at(&self, i: usize, kw: &str) -> bool {
        self.kind_at(i) == Some(T::Word) && self.text_at(i).eq_ignore_ascii_case(kw)
    }

    pub(crate) fn peek_kw(&self, kw: &str) -> bool {
        self.is_kw_at(self.i, kw)
    }

    pub(crate) fn eat_kw(&mut self, kw: &str) -> bool {
        let ok = self.peek_kw(kw);
        if ok {
            self.i += 1;
        }
        ok
    }

    pub(crate) fn kw(&mut self, kw: &str) -> Option<()> {
        self.eat_kw(kw).then_some(())
    }

    pub(crate) fn peek_punct(&self, p: &str) -> bool {
        self.kind_at(self.i) == Some(T::Punct) && self.text_at(self.i) == p
    }

    pub(crate) fn eat_punct(&mut self, p: &str) -> bool {
        let ok = self.peek_punct(p);
        if ok {
            self.i += 1;
        }
        ok
    }

    /// A name: a bare word or a quoted name.
    pub(crate) fn part(&mut self) -> Option<String> {
        let out = match self.kind_at(self.i)? {
            T::Word => self.text_at(self.i).to_string(),
            T::Ident => unquote(self.text_at(self.i)),
            _ => return None,
        };
        self.i += 1;
        Some(out)
    }

    /// `[db.]name`.
    pub(crate) fn qname(&mut self) -> Option<(Option<String>, String)> {
        let first = self.part()?;
        if self.peek_punct(".") {
            self.i += 1;
            let second = self.part()?;
            Some((Some(first), second))
        } else {
            Some((None, first))
        }
    }

    /// A string literal's text.
    pub(crate) fn string(&mut self) -> Option<String> {
        if self.kind_at(self.i)? != T::Str {
            return None;
        }
        let out = unquote(self.text_at(self.i));
        self.i += 1;
        Some(out)
    }

    /// The source text from token `from` to the one before `to`.
    pub(crate) fn span(&self, from: usize, to: usize) -> &'a str {
        if from >= to || from >= self.t.len() {
            return "";
        }
        let end = self.t[to.min(self.t.len()) - 1].end;
        &self.s[self.t[from].start..end]
    }

    /// The source text from token `from` to the end.
    pub(crate) fn rest(&self, from: usize) -> &'a str {
        self.t.get(from).map_or("", |t| &self.s[t.start..])
    }
}
