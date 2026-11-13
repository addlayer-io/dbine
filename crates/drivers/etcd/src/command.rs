//! The editor's language: `etcdctl` commands, one per line.
//!
//! ```text
//! get <key> [<range_end>] [--prefix] [--from-key] [--limit=N] [--rev=N]
//!     [--keys-only] [--count-only] [--order=ASCEND|DESCEND]
//!     [--sort-by=KEY|VERSION|CREATE|MODIFY|VALUE]
//! put <key> <value> [--lease=<hex id>] [--ttl=<seconds>] [--prev-kv]
//! del <key> [<range_end>] [--prefix] [--from-key] [--prev-kv]
//! lease grant <ttl> | lease revoke <id> | lease timetolive <id> [--keys] | lease list
//! member list | endpoint status | alarm list | compaction <rev> [--physical]
//! user list | user add <name> <password> [--no-password] | user passwd <name> <password>
//! user delete|get <name> | user grant-role|revoke-role <user> <role>
//! role list | role add|delete|get <role>
//! role grant-permission <role> read|write|readwrite <key> [<range_end>] [--prefix] [--from-key]
//! role revoke-permission <role> <key> [<range_end>] [--prefix] [--from-key]
//! auth status | auth enable | auth disable
//! snapshot save <file>
//! ```
//!
//! Arguments are quoted like a shell: `"…"` with backslash escapes (`\n`,
//! `\t`, `\"`, `\\`, `\xNN`) or `'…'` verbatim. `#` starts a comment.
//! `--ttl` on `put` is DBine's own: it grants a lease and attaches it; so
//! is the password as an argument of `user add` / `user passwd` (etcdctl
//! prompts for it or takes `--new-user-password`).
//!
//! A quote never spans lines, so a command is always one line. `execute`
//! and the editor's units ([`split`]) come from the same lines, so they
//! number the same commands at the same places.

use dbine_driver::{ScriptStatement, StatementKind};

/// One parsed command: words (lower-cased verb kept as typed) and flags.
#[derive(Debug, Clone, PartialEq, Default)]
pub struct Command {
    pub args: Vec<Vec<u8>>,
    /// `--name` or `--name=value`, lower-cased names.
    pub flags: Vec<(String, Option<String>)>,
}

impl Command {
    pub fn verb(&self) -> String {
        self.word(0).to_ascii_lowercase()
    }

    pub fn word(&self, i: usize) -> String {
        self.args
            .get(i)
            .map(|a| String::from_utf8_lossy(a).into_owned())
            .unwrap_or_default()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }

    pub fn flag_value(&self, name: &str) -> Option<&str> {
        self.flags
            .iter()
            .find(|(n, _)| n == name)
            .and_then(|(_, v)| v.as_deref())
    }

    /// The verb plus the sub-command when there is one (`lease grant`).
    pub fn name(&self) -> String {
        let v = self.verb();
        match v.as_str() {
            "lease" | "member" | "endpoint" | "alarm" | "user" | "role" | "auth" | "snapshot" => {
                format!("{v} {}", self.word(1).to_ascii_lowercase())
            }
            _ => v,
        }
    }
}

/// Commands that only read.
pub fn is_read(c: &Command) -> bool {
    matches!(
        c.name().as_str(),
        "get"
            | "lease timetolive"
            | "lease list"
            | "member list"
            | "endpoint status"
            | "alarm list"
            | "user list"
            | "role list"
            | "user get"
            | "role get"
            | "auth status"
            | "snapshot save"
    )
}

#[cfg(test)]
pub fn parse_script(text: &str) -> Result<Vec<Command>, String> {
    parse_placed(text)
        .map(|v| v.into_iter().map(|p| p.command).collect())
        .map_err(|(e, line, _)| format!("línea {line}: {e}"))
}

/// A command of an editor script with its place: byte range from its
/// first word to the end of its last (a trailing comment left out) and
/// 1-based line.
pub struct Placed {
    pub command: Command,
    pub start: usize,
    pub end: usize,
    pub line: u32,
}

/// A line that doesn't parse: its error, its 1-based number and where it
/// starts, and the range of its text (blanks around it left out).
struct Unparsed {
    error: String,
    line: u32,
    line_start: usize,
    start: usize,
    end: usize,
}

/// Each line of `text` that holds a command, placed, in order; a line that
/// doesn't parse comes as [`Unparsed`]. Comment and blank lines give
/// nothing. Both [`parse_placed`] (what `execute` runs and numbers) and
/// [`split`] (the editor's units) read the script through it.
fn placed(text: &str) -> impl Iterator<Item = Result<Placed, Unparsed>> + '_ {
    let mut at = 0;
    text.split('\n').enumerate().filter_map(move |(n, raw)| {
        let line_start = at;
        at += raw.len() + 1;
        let line = raw.strip_suffix('\r').unwrap_or(raw);
        let n = n as u32 + 1;
        match parse_line(line) {
            Ok(w) if w.words.is_empty() => None,
            Ok(w) => Some(Ok(Placed {
                command: command(w.words),
                start: line_start + w.from,
                end: line_start + w.to,
                line: n,
            })),
            Err(error) => {
                let t = line.trim_end();
                Some(Err(Unparsed {
                    error,
                    line: n,
                    line_start,
                    start: line_start + t.len() - t.trim_start().len(),
                    end: line_start + t.len(),
                }))
            }
        }
    })
}

/// A line's words as a command: an unquoted `--name[=value]` is a flag.
fn command(words: Vec<(Vec<u8>, bool)>) -> Command {
    let mut c = Command::default();
    for (w, quoted) in words {
        match std::str::from_utf8(&w)
            .ok()
            .filter(|s| !quoted && s.starts_with("--") && s.len() > 2)
        {
            Some(f) => {
                let f = &f[2..];
                let (k, v) = match f.split_once('=') {
                    Some((k, v)) => (k, Some(v.to_string())),
                    None => (f, None),
                };
                c.flags.push((k.to_ascii_lowercase(), v));
            }
            None => c.args.push(w),
        }
    }
    c
}

/// The script's commands as the editor's units ([`Driver::split_script`]):
/// the ones [`parse_placed`] gives `execute`, in the same order and at the
/// same places, so a result's statement number points at its line. It
/// never fails: a line that doesn't parse is still a unit, without an
/// `error`, so a command being typed (an open quote) keeps the lenses of
/// the others; running it reports the error at its line (`execute` runs
/// nothing of such a script).
///
/// [`Driver::split_script`]: dbine_driver::Driver::split_script
pub fn split(text: &str) -> Vec<ScriptStatement> {
    placed(text)
        .map(|r| {
            let (start, end, line) = match r {
                Ok(p) => (p.start, p.end, p.line),
                Err(u) => (u.start, u.end, u.line),
            };
            ScriptStatement {
                text: text[start..end].to_string(),
                start,
                end,
                line,
                kind: StatementKind::Sql,
                repeat: 1,
                error: None,
            }
        })
        .collect()
}

/// [`parse_script`] with each command's place; an error carries the line
/// and where it starts.
pub fn parse_placed(text: &str) -> Result<Vec<Placed>, (String, u32, usize)> {
    placed(text)
        .map(|r| r.map_err(|u| (u.error, u.line, u.line_start)))
        .collect()
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// A line's words, whether each was quoted, and where they are in it:
/// `from` is where the first one starts and `to` where the last one ends
/// (both 0 without words).
struct Words {
    words: Vec<(Vec<u8>, bool)>,
    from: usize,
    to: usize,
}

fn parse_line(line: &str) -> Result<Words, String> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let (mut from, mut to) = (0, 0);
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] == b'#' {
            return Ok(Words {
                words: out,
                from,
                to,
            });
        }
        if out.is_empty() {
            from = i;
        }
        let mut cur = Vec::new();
        let mut quoted = false;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            match b[i] {
                b'"' => {
                    quoted = true;
                    i += 1;
                    loop {
                        let Some(&c) = b.get(i) else {
                            return Err("comillas sin cerrar".into());
                        };
                        i += 1;
                        match c {
                            b'"' => break,
                            b'\\' => {
                                let Some(&n) = b.get(i) else {
                                    return Err("comillas sin cerrar".into());
                                };
                                i += 1;
                                if n == b'x' {
                                    if let (Some(h), Some(l)) = (
                                        b.get(i).and_then(|&x| hex(x)),
                                        b.get(i + 1).and_then(|&x| hex(x)),
                                    ) {
                                        cur.push(h * 16 + l);
                                        i += 2;
                                        continue;
                                    }
                                }
                                cur.push(match n {
                                    b'n' => b'\n',
                                    b'r' => b'\r',
                                    b't' => b'\t',
                                    b'0' => 0,
                                    other => other,
                                });
                            }
                            c => cur.push(c),
                        }
                    }
                }
                b'\'' => {
                    quoted = true;
                    i += 1;
                    loop {
                        let Some(&c) = b.get(i) else {
                            return Err("comillas sin cerrar".into());
                        };
                        i += 1;
                        if c == b'\'' {
                            break;
                        }
                        cur.push(c);
                    }
                }
                c => {
                    cur.push(c);
                    i += 1;
                }
            }
        }
        out.push((cur, quoted));
        to = i;
    }
}

/// An argument as the editor takes it back: bare when safe, else `"…"`.
pub fn quote_arg(s: &str) -> String {
    let safe = !s.is_empty()
        && !s.starts_with("--")
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b"/_-.:@+=,".contains(&b));
    if safe {
        return s.to_string();
    }
    let mut out = String::from("\"");
    for ch in s.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// The end of the range covering every key that starts with `prefix`
/// (etcdctl's `--prefix`): the prefix with its last byte below 0xff
/// incremented; `\0` (to the end) when there is none.
pub fn prefix_end(prefix: &[u8]) -> Vec<u8> {
    let mut end = prefix.to_vec();
    while let Some(last) = end.pop() {
        if last < 0xff {
            end.push(last + 1);
            return end;
        }
    }
    vec![0]
}

/// Lease ids are shown and typed in hex, like etcdctl.
pub fn parse_lease(s: &str) -> Result<i64, String> {
    let t = s.trim().trim_start_matches("0x");
    u64::from_str_radix(t, 16).map(|v| v as i64).map_err(|_| {
        format!("id de lease inválido: {s} (va en hexadecimal, como lo muestra lease list)")
    })
}

pub fn lease_hex(id: i64) -> String {
    format!("{:x}", id as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placed_commands() {
        let p = parse_placed("get a\r\n# c\n\n  put \"é\" v\nlease list").unwrap();
        let v: Vec<(usize, usize, u32, String)> = p
            .iter()
            .map(|c| (c.start, c.end, c.line, c.command.verb()))
            .collect();
        assert_eq!(
            v,
            [
                (0, 5, 1, "get".into()),
                (14, 24, 4, "put".into()),
                (25, 35, 5, "lease".into())
            ]
        );
        assert_eq!(
            parse_placed("get a\nget \"open").err().map(|e| (e.1, e.2)),
            Some((2, 6))
        );
        assert!(parse_script("get a\nget \"open")
            .unwrap_err()
            .starts_with("línea 2: "));
    }

    /// `(text, start, end, line)` of each unit; every unit's text is its
    /// range of the script.
    fn units(s: &str) -> Vec<(String, usize, usize, u32)> {
        split(s)
            .into_iter()
            .map(|u| {
                assert_eq!(&s[u.start..u.end], u.text);
                assert_eq!((u.kind, u.repeat), (StatementKind::Sql, 1));
                (u.text, u.start, u.end, u.line)
            })
            .collect()
    }

    fn texts(s: &str) -> Vec<String> {
        units(s).into_iter().map(|u| u.0).collect()
    }

    /// The editor's units are the commands `execute` numbers: same count,
    /// order, places, and each unit parses to its command.
    fn agrees(s: &str) {
        let placed = parse_placed(s).unwrap();
        let units = split(s);
        assert_eq!(units.len(), placed.len(), "{s:?}");
        for (u, p) in units.iter().zip(&placed) {
            assert_eq!((u.start, u.end, u.line), (p.start, p.end, p.line), "{s:?}");
            assert!(u.error.is_none());
            assert_eq!(
                parse_script(&u.text).unwrap(),
                std::slice::from_ref(&p.command)
            );
        }
    }

    #[test]
    fn one_unit_per_command_line() {
        // The audit's scripts: one unit per line.
        assert_eq!(texts("get a\nput a 1"), ["get a", "put a 1"]);
        assert_eq!(texts("put k \"a;b\"\nget k"), ["put k \"a;b\"", "get k"]);
        assert_eq!(
            units("get /a --prefix --limit=5\nlease grant 60\nmember list"),
            [
                ("get /a --prefix --limit=5".into(), 0, 25, 1),
                ("lease grant 60".into(), 26, 40, 2),
                ("member list".into(), 41, 52, 3)
            ]
        );
        for s in [
            "get a\nput a 1",
            "put k \"a;b\"\nget k",
            "get /a --prefix --limit=5\nlease grant 60\nmember list",
        ] {
            agrees(s);
        }
    }

    #[test]
    fn quotes_flags_comments_blank_lines_and_crlf() {
        // Separators of other languages inside quotes; `'…'` is verbatim;
        // a quoted `--x` is a value; a trailing comment isn't in the unit.
        let s = "put k 'a; \\ GO' --prev-kv  # c\nput \"k 2\" \"x\\\"y # z\" \"--x\"\n";
        assert_eq!(
            texts(s),
            [
                "put k 'a; \\ GO' --prev-kv",
                "put \"k 2\" \"x\\\"y # z\" \"--x\""
            ]
        );
        agrees(s);
        // Comment lines and blank lines aren't units, nor part of one.
        let s = "# first\n\n   # indented\nget a\n\nauth status\n";
        assert_eq!(
            units(s),
            [
                ("get a".into(), 23, 28, 4),
                ("auth status".into(), 30, 41, 6)
            ]
        );
        agrees(s);
        // CRLF and indentation: a unit starts at its first word and ends
        // at its last.
        let s = "get a\r\n\r\n\t del  \"é\" --prefix \r\nuser list\r\n";
        assert_eq!(
            units(s),
            [
                ("get a".into(), 0, 5, 1),
                ("del  \"é\" --prefix".into(), 11, 29, 3),
                ("user list".into(), 32, 41, 4)
            ]
        );
        agrees(s);
        assert!(split("").is_empty());
        assert!(split("# only\n\n").is_empty());
    }

    #[test]
    fn a_line_that_does_not_parse_is_still_a_unit() {
        // A quote doesn't span lines: the next line is its own unit.
        let s = "get a\n  put k \"open  \nget b";
        let u = split(s);
        assert_eq!(u.len(), 3);
        assert_eq!(
            (u[1].text.as_str(), u[1].start, u[1].line),
            ("put k \"open", 8, 2)
        );
        assert_eq!(u[2].text, "get b");
        // No unit carries an error (the editor keeps every lens while a
        // command is being typed): `execute` refuses that script at the
        // same line, and that unit alone with the same message.
        assert!(u.iter().all(|u| u.error.is_none()));
        assert_eq!(parse_placed(s).err().map(|e| e.1), Some(2));
        assert_eq!(
            parse_placed(&u[1].text).err().map(|e| e.0).as_deref(),
            Some("comillas sin cerrar")
        );
    }

    #[test]
    fn lines_words_flags_and_quotes() {
        let s = parse_script(
            "get /a --prefix --limit=5\n# nada\nput \"k 1\" 'v \\n' --lease=1a\nput k \"--x\"",
        )
        .unwrap();
        assert_eq!(s.len(), 3);
        assert_eq!(s[0].name(), "get");
        assert!(s[0].flag("prefix"));
        assert_eq!(s[0].flag_value("limit"), Some("5"));
        assert_eq!(s[1].args[1], b"k 1".to_vec());
        assert_eq!(s[1].args[2], b"v \\n".to_vec());
        assert_eq!(s[1].flag_value("lease"), Some("1a"));
        // A quoted "--x" is a value, not a flag.
        assert_eq!(s[2].args[2], b"--x".to_vec());
        assert!(parse_script("get \"x").is_err());
        let e = parse_script("put k \"a\\x41\\\"\"").unwrap();
        assert_eq!(e[0].args[2], b"aA\"".to_vec());
    }

    #[test]
    fn names_and_read_only() {
        let s = parse_script("lease grant 60\nLEASE list\nmember list\ndel x\nget x").unwrap();
        assert_eq!(s[0].name(), "lease grant");
        assert!(
            !is_read(&s[0])
                && is_read(&s[1])
                && is_read(&s[2])
                && !is_read(&s[3])
                && is_read(&s[4])
        );
    }

    #[test]
    fn prefixes_and_leases() {
        assert_eq!(prefix_end(b"/a"), b"/b".to_vec());
        assert_eq!(prefix_end(b"a\xff"), b"b".to_vec());
        assert_eq!(prefix_end(b""), vec![0]);
        assert_eq!(parse_lease("694d77aa9e38260f").unwrap(), 0x694d77aa9e38260f);
        assert_eq!(lease_hex(0x1a), "1a");
        assert!(parse_lease("zz").is_err());
        assert_eq!(quote_arg("/config/app"), "/config/app");
        assert_eq!(quote_arg("a b\"c"), "\"a b\\\"c\"");
        assert_eq!(quote_arg(""), "\"\"");
    }
}
