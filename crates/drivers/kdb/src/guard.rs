//! Read-only connections on kdb+.
//!
//! q is a whole programming language, so no word list can tell every read
//! from every write. Two layers:
//!
//! - **On the server**, each statement runs inside `reval`, which evaluates
//!   it as if the server had been started with `-b` (no writes to global
//!   variables, no file or system changes). Servers before kdb+ 3.6 don't
//!   have `reval`: there, only the check below applies.
//! - **Here**, before anything is sent, a statement is refused unless it is
//!   plainly a read: no assignment outside a function body or a q-sql
//!   column list, no word that writes or runs other code (`set`, `upsert`,
//!   `insert`, `system`, `value`, `hopen`…), no update or delete of a table
//!   by name, no file verbs (`0:` `1:` `2:`), no internal functions
//!   (`-N!`), and only the system commands that show something.
//!
//! What it can't prove read-only, it refuses.

/// Words that write, open connections, or evaluate other code.
const WRITE_WORDS: &[&str] = &[
    "set", "upsert", "insert", "system", "value", "eval", "reval", "hopen", "hclose", "hdel",
    "exit", "save", "rsave", "dsave", "load", "rload", "setenv", ".Q.dpft", ".Q.dpfts", ".Q.dpt",
    ".Q.dpfs", ".Q.hdpf", ".Q.en", ".Q.ens", ".Q.gc", ".Q.chk", ".Q.l", ".Q.lo", ".Q.ld", ".Q.trp",
    ".Q.trap", ".z.exit",
];

/// System commands that only show something; arguments are allowed only
/// on the first four (a namespace to list).
const SHOW_COMMANDS: &[&str] = &[
    "v", "f", "a", "b", "B", "w", "d", "c", "e", "o", "p", "P", "s", "T", "z",
];

/// Why `stmt` isn't allowed on a read-only connection, or `None`.
pub fn refusal(stmt: &str) -> Option<String> {
    let s = stmt.trim();
    if let Some(cmd) = s.strip_prefix('\\') {
        let mut parts = cmd.split_whitespace();
        let name = parts.next().unwrap_or("");
        let has_args = parts.next().is_some();
        let ok =
            SHOW_COMMANDS.contains(&name) && (!has_args || matches!(name, "v" | "f" | "a" | "b"));
        return (!ok).then(|| {
            format!("Conexión de solo lectura: se bloqueó el comando de sistema \\{name}. Solo se permiten los que muestran algo (\\v, \\f, \\a, \\b, \\w…).")
        });
    }
    let code = mask(s);
    if let Some(w) = write_word(&code) {
        return Some(format!(
            "Conexión de solo lectura: se bloqueó {w}. Solo se permiten lecturas."
        ));
    }
    if let Some(why) = assignment(&code) {
        return Some(format!(
            "Conexión de solo lectura: se bloqueó {why}. Solo se permiten lecturas."
        ));
    }
    None
}

/// `s` with string contents and comments blanked out (same length), so
/// what's inside them can't match.
fn mask(s: &str) -> String {
    let b = s.as_bytes();
    let mut out = Vec::with_capacity(b.len());
    let mut i = 0;
    let mut in_str = false;
    while i < b.len() {
        let c = b[i];
        if in_str {
            if c == b'\\' && i + 1 < b.len() {
                out.extend_from_slice(b"  ");
                i += 2;
                continue;
            }
            if c == b'"' {
                in_str = false;
                out.push(b'"');
            } else {
                out.push(if c == b'\n' { b'\n' } else { b' ' });
            }
            i += 1;
            continue;
        }
        if c == b'"' {
            in_str = true;
            out.push(b'"');
            i += 1;
            continue;
        }
        // ` /…` to the end of the line is a comment (a `/` right after
        // something is an iterator: `+/`).
        if c == b'/' && (i == 0 || b[i - 1] == b' ' || b[i - 1] == b'\t' || b[i - 1] == b'\n') {
            while i < b.len() && b[i] != b'\n' {
                out.push(b' ');
                i += 1;
            }
            continue;
        }
        out.push(c);
        i += 1;
    }
    String::from_utf8(out).unwrap_or_default()
}

fn ident_char(c: char) -> bool {
    c.is_ascii_alphanumeric() || c == '_' || c == '.'
}

/// The identifiers of `code` (dotted names whole: `.Q.dpft`), with where
/// they start. A symbol literal (`` `set ``) is data, not a call.
fn idents(code: &str) -> Vec<(usize, &str)> {
    let mut out = Vec::new();
    let b = code.as_bytes();
    let mut i = 0;
    while i < b.len() {
        let c = b[i] as char;
        if ident_char(c) && !(c == '.' && b.get(i + 1).is_some_and(|n| n.is_ascii_digit())) {
            let start = i;
            while i < b.len() && ident_char(b[i] as char) {
                i += 1;
            }
            let after_backtick = start > 0 && b[start - 1] == b'`';
            let word = &code[start..i];
            if !after_backtick && !word.chars().next().is_some_and(|c| c.is_ascii_digit()) {
                out.push((start, word));
            }
            continue;
        }
        i += 1;
    }
    out
}

fn write_word(code: &str) -> Option<String> {
    let words = idents(code);
    for (_, w) in &words {
        let w = w.trim_end_matches('.');
        if WRITE_WORDS.contains(&w) {
            return Some(format!("«{w}»"));
        }
    }
    // update/delete of a table by name: `update … from `t`.
    let lower = code.to_ascii_lowercase();
    for (at, w) in &words {
        if matches!(*w, "update" | "delete") {
            if let Some(f) = lower[*at..].find("from") {
                let rest = lower[at + f + 4..].trim_start();
                if rest.starts_with('`') {
                    return Some(format!("«{w} … from `tabla»"));
                }
            }
        }
    }
    let b = code.as_bytes();
    for i in 0..b.len() {
        // Functional amend / update / delete of a name: `@[`t;…]`,
        // `.[`t;…]`, `![`t;…]`.
        if matches!(b[i], b'@' | b'.' | b'!') && b.get(i + 1) == Some(&b'[') {
            let rest = code[i + 2..].trim_start();
            if rest.starts_with('`') {
                return Some(format!("«{}[`…]»", b[i] as char));
            }
        }
        // File verbs: `0:` (text), `1:` (binary), `2:` (load a library).
        if matches!(b[i], b'0' | b'1' | b'2')
            && b.get(i + 1) == Some(&b':')
            && b.get(i + 2) != Some(&b':')
            && (i == 0 || !(b[i - 1] as char).is_ascii_alphanumeric())
        {
            return Some(format!("«{}:»", b[i] as char));
        }
        // Internal functions: `-11!` (replay a log), `-15!`…
        if b[i] == b'-' && b.get(i + 1).is_some_and(u8::is_ascii_digit) {
            let mut j = i + 1;
            while j < b.len() && b[j].is_ascii_digit() {
                j += 1;
            }
            if b.get(j) == Some(&b'!') {
                return Some(format!("«{}»", &code[i..=j]));
            }
        }
    }
    None
}

/// The name an assignment at byte `i` (a `:`) would write: the
/// identifier before it, past an index (`x[0]:`) and an operator
/// (`x+:`). `None` when what comes before isn't a name (a time literal
/// `12:30`, a `(`, an iterator…).
fn target(code: &str, i: usize) -> Option<&str> {
    let mut before = code[..i].trim_end();
    if let Some(op) = before
        .chars()
        .last()
        .filter(|c| "+-*%&|^=<>$,#_~!?@".contains(*c))
    {
        before = before[..before.len() - op.len_utf8()].trim_end();
    }
    if before.ends_with(']') {
        let mut depth = 0i32;
        let mut open = None;
        for (j, c) in before.char_indices().rev() {
            match c {
                ']' => depth += 1,
                '[' => {
                    depth -= 1;
                    if depth == 0 {
                        open = Some(j);
                        break;
                    }
                }
                _ => {}
            }
        }
        before = before[..open?].trim_end();
    }
    let start = before
        .char_indices()
        .rev()
        .take_while(|(_, c)| ident_char(*c))
        .last()
        .map(|(j, _)| j)?;
    let name = &before[start..];
    let first = name.trim_start_matches('.').chars().next()?;
    let quoted = start > 0 && before.as_bytes()[start - 1] == b'`';
    (first.is_ascii_alphabetic() && !quoted).then_some(name)
}

/// An assignment that writes outside the statement: any at the top level
/// (not in a function body nor in a q-sql column list), a global one
/// (`x::`), and, inside a function body, one to a dotted name (always
/// global in q: `.z.pg:`).
fn assignment(code: &str) -> Option<String> {
    let b = code.as_bytes();
    let mut braces = 0i32;
    // q-sql templates: from `select`/`exec`/`update`/`delete` to `from`,
    // `:` names a column.
    let mut template = false;
    let words = idents(code);
    let mut next_word = words.iter().peekable();
    let mut i = 0;
    while i < b.len() {
        while let Some((at, w)) = next_word.peek() {
            if *at > i {
                break;
            }
            if *at == i {
                match *w {
                    "select" | "exec" | "update" | "delete" => template = true,
                    "from" => template = false,
                    _ => {}
                }
            }
            next_word.next();
        }
        match b[i] {
            b'{' => braces += 1,
            b'}' => braces -= 1,
            b':' => {
                let global = b.get(i + 1) == Some(&b':');
                let name = target(code, i);
                if global {
                    // `::` alone is the identity, the generic null.
                    if name.is_some() {
                        return Some("una asignación global (::)".into());
                    }
                    i += 2;
                    continue;
                }
                if let Some(n) = name {
                    if (braces <= 0 && !template) || (braces > 0 && n.starts_with('.')) {
                        return Some(format!("una asignación a {n}"));
                    }
                }
            }
            _ => {}
        }
        i += 1;
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_pass() {
        for s in [
            "select from trade",
            "select[100] from trade where sym=`AAPL, price>10",
            "select avg price by sym from trade",
            "select vwap:size wavg price by sym from trade",
            "exec sum size from trade",
            "update mid:0.5*bid+ask from quote",
            "delete from trade where sym=`X",
            "tables[]",
            "meta trade",
            "count trade",
            "0!kt",
            "{x+1} each til 10",
            "{a:x*2; a+1}[3]",
            "`set`upsert",
            "\"a:b set c\"",
            "1+1 / a: comment set",
            "\\v",
            "\\f .myns",
            "\\w",
            "+/[1 2 3]",
            "(::)",
            ".Q.s1 trade",
            "aj[`sym`time;trade;quote]",
            "get `trade",
            "-8 sublist trade",
            "select from t where time>12:30",
            "select from t where ts within 2024.01.31D12:30:00 2024.02.01D00:00:00",
            "{[t] t:t+1; t}[1]",
        ] {
            assert_eq!(refusal(s), None, "{s}");
        }
    }

    #[test]
    fn writes_are_refused() {
        for s in [
            "a:1",
            "t2:select from t",
            "x::1",
            "f:{x::1}",
            "x[0]:5",
            "x+:1",
            "x ,: 1",
            "`trade insert (`A;1.0)",
            "`t upsert ([] a:1 2)",
            "`x set 1",
            "update price:0 from `trade",
            "delete from `trade where sym=`X",
            "@[`t;0;:;1]",
            ".[`t;(0;`a);:;1]",
            "![`t;();0b;enlist `a]",
            "`:file 0: (\"a\";\"b\")",
            "`:f 1: 0x00",
            "`lib 2: (`f;1)",
            "-11!`:log",
            "system \"rm -rf x\"",
            "value \"a:1\"",
            "hopen `:host:5000",
            "exit 0",
            ".Q.dpft[`:db;2024.01.01;`sym;`t]",
            "\\l script.q",
            "\\p 6000",
            "\\\\",
            "\\t select from t",
            "{x} {.z.pg:{}}",
        ] {
            assert!(refusal(s).is_some(), "{s}");
        }
    }
}
