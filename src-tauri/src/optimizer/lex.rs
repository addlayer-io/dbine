//! The token stream the rules work on: words, quoted names, strings,
//! numbers and punctuation with their byte ranges in the text. Comments and
//! spaces are left out; a rewrite splices the original text, so they stay
//! where the user wrote them.

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Kind {
    /// A keyword or an unquoted name.
    Word,
    /// A quoted name: "x", `x`, [x].
    Ident,
    /// A string, with its prefix (N'…', E'…', $$…$$, q'[…]').
    Str,
    Num,
    /// `?`, `$1`, `:name`, `@name` (T-SQL variables too).
    Param,
    Op,
    LParen,
    RParen,
    Comma,
    Dot,
    Semi,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Tok {
    pub kind: Kind,
    pub start: usize,
    pub end: usize,
}

/// How the dialect quotes and comments.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub struct Flavor {
    pub brackets: bool,
    pub backticks: bool,
    pub dollar_quotes: bool,
    pub backslash_escapes: bool,
    pub hash_comments: bool,
    /// "…" is a string, not a name (MySQL without ANSI_QUOTES, BigQuery).
    pub double_quoted_strings: bool,
    pub q_quotes: bool,
}

impl Flavor {
    pub fn for_dialect(dialect: &str) -> Self {
        match dialect {
            "mssql" | "sybase" | "access" => Self { brackets: true, ..Self::default() },
            "mysql" => Self { backticks: true, backslash_escapes: true, hash_comments: true, double_quoted_strings: true, ..Self::default() },
            "bigquery" => Self { backticks: true, backslash_escapes: true, double_quoted_strings: true, ..Self::default() },
            "clickhouse" => Self { backticks: true, backslash_escapes: true, ..Self::default() },
            "hive" | "sparksql" | "databricks" => Self { backticks: true, backslash_escapes: true, ..Self::default() },
            "postgres" | "standard" => Self { dollar_quotes: true, ..Self::default() },
            "oracle" => Self { q_quotes: true, ..Self::default() },
            "sqlite" => Self { brackets: true, backticks: true, ..Self::default() },
            _ => Self { backticks: true, ..Self::default() },
        }
    }
}

fn word_byte(c: u8) -> bool {
    c.is_ascii_alphanumeric() || c == b'_' || c == b'$' || c == b'#' || c >= 0x80
}

/// Where a quoted run that starts at `i` (on `quote`) ends: after the closing
/// quote, a doubled quote being an escaped one.
fn quoted_end(b: &[u8], i: usize, close: u8, backslash: bool) -> usize {
    let mut j = i + 1;
    while j < b.len() {
        if backslash && b[j] == b'\\' {
            j += 2;
            continue;
        }
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

pub fn lex(src: &str, f: Flavor) -> Vec<Tok> {
    let b = src.as_bytes();
    let mut out: Vec<Tok> = Vec::new();
    let mut i = 0;
    let push = |out: &mut Vec<Tok>, kind, start, end| out.push(Tok { kind, start, end });
    while i < b.len() {
        let c = b[i];
        let next = b.get(i + 1).copied();
        if c.is_ascii_whitespace() {
            i += 1;
            continue;
        }
        if (c == b'-' && next == Some(b'-')) || (c == b'#' && f.hash_comments) {
            while i < b.len() && b[i] != b'\n' {
                i += 1;
            }
            continue;
        }
        if c == b'/' && next == Some(b'*') {
            i = src[i + 2..].find("*/").map_or(b.len(), |p| i + 2 + p + 2);
            continue;
        }
        let start = i;
        match c {
            b'\'' => {
                i = quoted_end(b, i, b'\'', f.backslash_escapes);
                push(&mut out, Kind::Str, start, i);
            }
            b'"' => {
                i = quoted_end(b, i, b'"', f.double_quoted_strings && f.backslash_escapes);
                push(&mut out, if f.double_quoted_strings { Kind::Str } else { Kind::Ident }, start, i);
            }
            b'`' if f.backticks => {
                i = quoted_end(b, i, b'`', false);
                push(&mut out, Kind::Ident, start, i);
            }
            b'[' if f.brackets => {
                i = quoted_end(b, i, b']', false);
                push(&mut out, Kind::Ident, start, i);
            }
            b'$' if f.dollar_quotes && dollar_tag(b, i).is_some() => {
                let tag_end = dollar_tag(b, i).unwrap_or(i + 1);
                let tag = &src[i..tag_end];
                i = src[tag_end..].find(tag).map_or(b.len(), |p| tag_end + p + tag.len());
                push(&mut out, Kind::Str, start, i);
            }
            b'$' if next.is_some_and(|n| n.is_ascii_digit()) => {
                i += 1;
                while i < b.len() && b[i].is_ascii_digit() {
                    i += 1;
                }
                push(&mut out, Kind::Param, start, i);
            }
            b'(' => {
                i += 1;
                push(&mut out, Kind::LParen, start, i);
            }
            b')' => {
                i += 1;
                push(&mut out, Kind::RParen, start, i);
            }
            b',' => {
                i += 1;
                push(&mut out, Kind::Comma, start, i);
            }
            b';' => {
                i += 1;
                push(&mut out, Kind::Semi, start, i);
            }
            b'.' if next.is_some_and(|n| n.is_ascii_digit())
                && !out.last().is_some_and(|t| matches!(t.kind, Kind::Word | Kind::Ident | Kind::RParen) && t.end == i) =>
            {
                i = number_end(b, i);
                push(&mut out, Kind::Num, start, i);
            }
            b'.' => {
                i += 1;
                push(&mut out, Kind::Dot, start, i);
            }
            b'0'..=b'9' => {
                i = number_end(b, i);
                push(&mut out, Kind::Num, start, i);
            }
            b'?' => {
                i += 1;
                push(&mut out, Kind::Param, start, i);
            }
            b':' if next.is_some_and(|n| n.is_ascii_alphabetic() || n == b'_') && !out.last().is_some_and(|t| t.end == i && t.kind == Kind::Op) => {
                i += 1;
                while i < b.len() && word_byte(b[i]) {
                    i += 1;
                }
                push(&mut out, Kind::Param, start, i);
            }
            b'@' => {
                i += 1;
                while i < b.len() && (word_byte(b[i]) || b[i] == b'@') {
                    i += 1;
                }
                push(&mut out, Kind::Param, start, i);
            }
            _ if word_byte(c) && c != b'$' => {
                while i < b.len() && word_byte(b[i]) {
                    i += 1;
                }
                let w = &src[start..i];
                // N'…', E'…', X'…', B'…', U&'…', Oracle's q'[…]'.
                if b.get(i) == Some(&b'\'') {
                    let lw = w.to_ascii_lowercase();
                    if f.q_quotes && (lw == "q" || lw == "nq") {
                        i = q_quote_end(b, i);
                        push(&mut out, Kind::Str, start, i);
                        continue;
                    }
                    if matches!(lw.as_str(), "n" | "e" | "x" | "b" | "r" | "_utf8mb4" | "_utf8") {
                        i = quoted_end(b, i, b'\'', f.backslash_escapes || lw == "e");
                        push(&mut out, Kind::Str, start, i);
                        continue;
                    }
                }
                if w.eq_ignore_ascii_case("u") && b.get(i) == Some(&b'&') && b.get(i + 1) == Some(&b'\'') {
                    i = quoted_end(b, i + 1, b'\'', false);
                    push(&mut out, Kind::Str, start, i);
                    continue;
                }
                push(&mut out, Kind::Word, start, i);
            }
            _ => {
                const OPS: &[&str] = &["<=>", "->>", "<=", ">=", "<>", "!=", "||", "::", "->", "=>", "**", "!<", "!>"];
                let op = OPS.iter().find(|o| src[i..].starts_with(*o)).map_or(1, |o| o.len());
                // A multi-byte character that isn't a word (a stray symbol).
                let mut end = i + op;
                while !src.is_char_boundary(end) {
                    end += 1;
                }
                i = end;
                push(&mut out, Kind::Op, start, i);
            }
        }
    }
    out
}

/// `$tag$` at `i`: where the tag ends (after its second `$`).
fn dollar_tag(b: &[u8], i: usize) -> Option<usize> {
    let mut j = i + 1;
    while j < b.len() && (b[j].is_ascii_alphanumeric() || b[j] == b'_') {
        if j == i + 1 && b[j].is_ascii_digit() {
            return None;
        }
        j += 1;
    }
    (b.get(j) == Some(&b'$')).then_some(j + 1)
}

fn number_end(b: &[u8], mut i: usize) -> usize {
    if b[i] == b'0' && matches!(b.get(i + 1), Some(b'x' | b'X')) {
        i += 2;
        while i < b.len() && b[i].is_ascii_hexdigit() {
            i += 1;
        }
        return i;
    }
    while i < b.len() && b[i].is_ascii_digit() {
        i += 1;
    }
    if b.get(i) == Some(&b'.') && b.get(i + 1) != Some(&b'.') {
        i += 1;
        while i < b.len() && b[i].is_ascii_digit() {
            i += 1;
        }
    }
    if matches!(b.get(i), Some(b'e' | b'E')) {
        let mut j = i + 1;
        if matches!(b.get(j), Some(b'+' | b'-')) {
            j += 1;
        }
        if b.get(j).is_some_and(|c| c.is_ascii_digit()) {
            i = j;
            while i < b.len() && b[i].is_ascii_digit() {
                i += 1;
            }
        }
    }
    i
}

/// Oracle's `q'[ … ]'` starting at the quote after `q`.
fn q_quote_end(b: &[u8], i: usize) -> usize {
    let Some(&open) = b.get(i + 1) else { return b.len() };
    let close = match open {
        b'[' => b']',
        b'(' => b')',
        b'{' => b'}',
        b'<' => b'>',
        c => c,
    };
    let mut j = i + 2;
    while j + 1 < b.len() {
        if b[j] == close && b[j + 1] == b'\'' {
            return j + 2;
        }
        j += 1;
    }
    b.len()
}

/// A quoted name without its quotes (doubled quotes undone); other tokens as written.
pub fn unquote(text: &str) -> String {
    let b = text.as_bytes();
    if b.len() >= 2 {
        let (o, c) = (b[0], b[b.len() - 1]);
        let pair = match o {
            b'"' => Some(b'"'),
            b'`' => Some(b'`'),
            b'[' => Some(b']'),
            _ => None,
        };
        if pair == Some(c) {
            let inner = &text[1..text.len() - 1];
            let q = c as char;
            return inner.replace(&format!("{q}{q}"), &q.to_string());
        }
    }
    text.to_string()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn kinds(src: &str, f: Flavor) -> Vec<(Kind, &str)> {
        lex(src, f).into_iter().map(|t| (t.kind, &src[t.start..t.end])).collect()
    }

    #[test]
    fn words_strings_and_punctuation() {
        let k = kinds("SELECT a.b, 'it''s' FROM t -- x\nWHERE c >= 1.5e3;", Flavor::default());
        assert_eq!(
            k,
            vec![
                (Kind::Word, "SELECT"),
                (Kind::Word, "a"),
                (Kind::Dot, "."),
                (Kind::Word, "b"),
                (Kind::Comma, ","),
                (Kind::Str, "'it''s'"),
                (Kind::Word, "FROM"),
                (Kind::Word, "t"),
                (Kind::Word, "WHERE"),
                (Kind::Word, "c"),
                (Kind::Op, ">="),
                (Kind::Num, "1.5e3"),
                (Kind::Semi, ";"),
            ]
        );
    }

    #[test]
    fn quoting_per_dialect() {
        let ms = kinds("[dbo].[my t] = N'x'", Flavor::for_dialect("mssql"));
        assert_eq!(ms, vec![(Kind::Ident, "[dbo]"), (Kind::Dot, "."), (Kind::Ident, "[my t]"), (Kind::Op, "="), (Kind::Str, "N'x'")]);
        let my = kinds("`a` = \"b\\\"c\" # c\n", Flavor::for_dialect("mysql"));
        assert_eq!(my, vec![(Kind::Ident, "`a`"), (Kind::Op, "="), (Kind::Str, "\"b\\\"c\"")]);
        let pg = kinds("x = $$a;b$$ AND y = $1::int", Flavor::for_dialect("postgres"));
        assert_eq!(
            pg,
            vec![
                (Kind::Word, "x"),
                (Kind::Op, "="),
                (Kind::Str, "$$a;b$$"),
                (Kind::Word, "AND"),
                (Kind::Word, "y"),
                (Kind::Op, "="),
                (Kind::Param, "$1"),
                (Kind::Op, "::"),
                (Kind::Word, "int"),
            ]
        );
        let ora = kinds("q'[it's]' || :p", Flavor::for_dialect("oracle"));
        assert_eq!(ora, vec![(Kind::Str, "q'[it's]'"), (Kind::Op, "||"), (Kind::Param, ":p")]);
    }

    #[test]
    fn comments_are_skipped_and_numbers_after_names_are_dots() {
        let k = kinds("/* a */ t.1 , .5 @v @@rowcount", Flavor::for_dialect("mssql"));
        assert_eq!(k, vec![(Kind::Word, "t"), (Kind::Dot, "."), (Kind::Num, "1"), (Kind::Comma, ","), (Kind::Num, ".5"), (Kind::Param, "@v"), (Kind::Param, "@@rowcount")]);
    }

    #[test]
    fn unquoting() {
        assert_eq!(unquote("\"a\"\"b\""), "a\"b");
        assert_eq!(unquote("[a]]b]"), "a]b");
        assert_eq!(unquote("plain"), "plain");
    }
}
