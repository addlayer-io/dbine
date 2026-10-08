//! Line-command languages: Redis (Redis, Valkey, Dragonfly) and etcd. One
//! command per line, arguments quoted like a shell (`"…"` with backslash
//! escapes, `'…'`), `#` starting a comment.

use super::Finding;

/// An argument: its value (unquoted) and its byte range.
struct Arg<'a> {
    raw: &'a str,
    value: String,
    start: usize,
    end: usize,
}

/// The arguments of each line.
fn lines(script: &str) -> Vec<Vec<Arg<'_>>> {
    let mut out = Vec::new();
    let mut base = 0;
    for line in script.split_inclusive('\n') {
        let b = line.as_bytes();
        let mut args = Vec::new();
        let mut i = 0;
        while i < b.len() {
            let c = b[i];
            if c.is_ascii_whitespace() {
                i += 1;
                continue;
            }
            if c == b'#' {
                break;
            }
            let start = i;
            let mut value = String::new();
            if c == b'"' || c == b'\'' {
                i += 1;
                while i < b.len() && b[i] != c && b[i] != b'\n' {
                    if b[i] == b'\\' && i + 1 < b.len() {
                        i += 1;
                    }
                    let ch = line[i..].chars().next().unwrap_or(' ');
                    value.push(ch);
                    i += ch.len_utf8();
                }
                i = (i + 1).min(b.len());
            } else {
                while i < b.len() && !b[i].is_ascii_whitespace() {
                    i += 1;
                }
                value.push_str(&line[start..i]);
            }
            args.push(Arg { raw: &line[start..i], value, start: base + start, end: base + i });
        }
        if !args.is_empty() {
            out.push(args);
        }
        base += line.len();
    }
    out
}

pub(super) fn lint_redis(script: &str, out: &mut Vec<Finding>) {
    for args in lines(script) {
        let cmd = &args[0];
        let name = cmd.value.to_ascii_uppercase();
        let last = args.last().map_or(cmd.end, |a| a.end);
        match name.as_str() {
            "KEYS" => out.push(Finding::new("keys-command", cmd.start, last).param("pattern", args.get(1).map_or("", |a| a.raw))),
            "FLUSHALL" | "FLUSHDB" => out.push(Finding::new("flush", cmd.start, last).param("command", name.clone())),
            "HGETALL" | "SMEMBERS" => out.push(Finding::new("big-read", cmd.start, last).param("command", name.clone())),
            // LRANGE / ZRANGE k 0 -1: the whole list or set.
            "LRANGE" | "ZRANGE" if args.len() == 4 && args[2].value == "0" && args[3].value == "-1" => {
                out.push(Finding::new("big-read", cmd.start, last).param("command", format!("{name} … 0 -1")))
            }
            _ => {}
        }
    }
}

pub(super) fn lint_etcd(script: &str, out: &mut Vec<Finding>) {
    for args in lines(script) {
        let cmd = args[0].value.to_ascii_lowercase();
        if cmd != "del" && cmd != "get" {
            continue;
        }
        let flag = |f: &str| args.iter().any(|a| a.value == f || a.value.starts_with(&format!("{f}=")));
        let keys: Vec<&Arg> = args[1..].iter().filter(|a| !a.value.starts_with("--")).collect();
        let Some(key) = keys.first() else { continue };
        // "" with --prefix, or "" / "\0" with --from-key: every key.
        let all = (flag("--prefix") && key.value.is_empty()) || (flag("--from-key") && (key.value.is_empty() || key.raw.trim_matches(['"', '\'']) == "\\0"));
        if !all {
            continue;
        }
        let last = args.last().map_or(args[0].end, |a| a.end);
        let call: String = args.iter().map(|a| a.raw).collect::<Vec<_>>().join(" ");
        if cmd == "del" {
            out.push(Finding::new("write-all", args[0].start, last).param("call", call));
        } else if !flag("--limit") && !flag("--count-only") {
            out.push(Finding::new("read-all", args[0].start, last).param("call", call));
        }
    }
}
