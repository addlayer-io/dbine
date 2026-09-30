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
        self.args.get(i).map(|a| String::from_utf8_lossy(a).into_owned()).unwrap_or_default()
    }

    pub fn flag(&self, name: &str) -> bool {
        self.flags.iter().any(|(n, _)| n == name)
    }

    pub fn flag_value(&self, name: &str) -> Option<&str> {
        self.flags.iter().find(|(n, _)| n == name).and_then(|(_, v)| v.as_deref())
    }

    /// The verb plus the sub-command when there is one (`lease grant`).
    pub fn name(&self) -> String {
        let v = self.verb();
        match v.as_str() {
            "lease" | "member" | "endpoint" | "alarm" | "user" | "role" | "auth" | "snapshot" => format!("{v} {}", self.word(1).to_ascii_lowercase()),
            _ => v,
        }
    }
}

/// Commands that only read.
pub fn is_read(c: &Command) -> bool {
    matches!(
        c.name().as_str(),
        "get" | "lease timetolive" | "lease list" | "member list" | "endpoint status" | "alarm list" | "user list" | "role list"
            | "user get" | "role get" | "auth status" | "snapshot save"
    )
}

pub fn parse_script(text: &str) -> Result<Vec<Command>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let words = parse_line(line).map_err(|e| format!("línea {}: {e}", n + 1))?;
        if words.is_empty() {
            continue;
        }
        let mut c = Command::default();
        for (w, quoted) in words {
            match std::str::from_utf8(&w).ok().filter(|s| !quoted && s.starts_with("--") && s.len() > 2) {
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
        out.push(c);
    }
    Ok(out)
}

fn hex(b: u8) -> Option<u8> {
    (b as char).to_digit(16).map(|d| d as u8)
}

/// Words of a line and whether each was quoted.
fn parse_line(line: &str) -> Result<Vec<(Vec<u8>, bool)>, String> {
    let b = line.as_bytes();
    let mut out = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] == b'#' {
            return Ok(out);
        }
        let mut cur = Vec::new();
        let mut quoted = false;
        while i < b.len() && !b[i].is_ascii_whitespace() {
            match b[i] {
                b'"' => {
                    quoted = true;
                    i += 1;
                    loop {
                        let Some(&c) = b.get(i) else { return Err("comillas sin cerrar".into()) };
                        i += 1;
                        match c {
                            b'"' => break,
                            b'\\' => {
                                let Some(&n) = b.get(i) else { return Err("comillas sin cerrar".into()) };
                                i += 1;
                                if n == b'x' {
                                    if let (Some(h), Some(l)) = (b.get(i).and_then(|&x| hex(x)), b.get(i + 1).and_then(|&x| hex(x))) {
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
                        let Some(&c) = b.get(i) else { return Err("comillas sin cerrar".into()) };
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
    }
}

/// An argument as the editor takes it back: bare when safe, else `"…"`.
pub fn quote_arg(s: &str) -> String {
    let safe = !s.is_empty()
        && !s.starts_with("--")
        && s.bytes().all(|b| b.is_ascii_alphanumeric() || b"/_-.:@+=,".contains(&b));
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
    u64::from_str_radix(t, 16).map(|v| v as i64).map_err(|_| format!("id de lease inválido: {s} (va en hexadecimal, como lo muestra lease list)"))
}

pub fn lease_hex(id: i64) -> String {
    format!("{:x}", id as u64)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn lines_words_flags_and_quotes() {
        let s = parse_script("get /a --prefix --limit=5\n# nada\nput \"k 1\" 'v \\n' --lease=1a\nput k \"--x\"").unwrap();
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
        assert!(!is_read(&s[0]) && is_read(&s[1]) && is_read(&s[2]) && !is_read(&s[3]) && is_read(&s[4]));
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
