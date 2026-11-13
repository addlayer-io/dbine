//! The editor's language: one Redis command per line, quoted like
//! `redis-cli` (`"…"` with backslash escapes, `'…'` with only `\'`), and
//! `#` starting a comment. Also the read-only allow-list.
//!
//! A quote never spans lines, so a command is always one line: MULTI, the
//! commands it queues and EXEC are separate commands, and an EVAL body
//! sits inside its line. `execute` and the editor's units
//! ([`split`]) come from the same lines, so they number the same commands
//! at the same places.

use dbine_driver::{ScriptStatement, StatementKind};

/// Commands of a script, each as its raw arguments. A `#` at the start of
/// an argument (outside quotes) comments out the rest of the line.
#[cfg(test)]
pub fn parse_script(text: &str) -> Result<Vec<Vec<Vec<u8>>>, String> {
    let mut out = Vec::new();
    for (n, line) in text.lines().enumerate() {
        let args = parse_line(line).map_err(|e| format!("línea {}: {e}", n + 1))?;
        if !args.is_empty() {
            out.push(args);
        }
    }
    Ok(out)
}

/// A command of an editor script with its place: byte range from its first
/// argument to the end of its last (a trailing comment left out) and
/// 1-based line.
pub struct Placed {
    pub args: Vec<Vec<u8>>,
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
        match parse_args(line) {
            Ok(p) if p.args.is_empty() => None,
            Ok(p) => Some(Ok(Placed {
                args: p.args,
                start: line_start + p.from,
                end: line_start + p.to,
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

/// [`parse_script`] with each command's place; an error carries the line
/// and its start.
pub fn parse_placed(text: &str) -> Result<Vec<Placed>, (String, u32, usize)> {
    placed(text)
        .map(|r| r.map_err(|u| (u.error, u.line, u.line_start)))
        .collect()
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

pub(crate) fn parse_line(line: &str) -> Result<Vec<Vec<u8>>, String> {
    parse_args(line).map(|p| p.args)
}

/// A line's arguments and where they are in it: `from` is where the first
/// one starts and `to` where the last one ends (both 0 without arguments).
struct Args {
    args: Vec<Vec<u8>>,
    from: usize,
    to: usize,
}

fn parse_args(line: &str) -> Result<Args, String> {
    let b = line.as_bytes();
    let mut args = Vec::new();
    let (mut from, mut to) = (0, 0);
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] == b'#' {
            return Ok(Args { args, from, to });
        }
        if args.is_empty() {
            from = i;
        }
        let mut cur = Vec::new();
        let mut quote: Option<u8> = None;
        loop {
            if i >= b.len() {
                if quote.is_some() {
                    return Err("comillas sin cerrar".into());
                }
                break;
            }
            let c = b[i];
            match quote {
                Some(b'"') => match c {
                    b'\\' if i + 1 < b.len() => {
                        let n = b[i + 1];
                        if n == b'x' && i + 3 < b.len() && is_hex(b[i + 2]) && is_hex(b[i + 3]) {
                            cur.push(hex(b[i + 2]) * 16 + hex(b[i + 3]));
                            i += 4;
                            continue;
                        }
                        cur.push(match n {
                            b'n' => b'\n',
                            b'r' => b'\r',
                            b't' => b'\t',
                            b'b' => 8,
                            b'a' => 7,
                            other => other,
                        });
                        i += 2;
                        continue;
                    }
                    b'"' => {
                        quote = None;
                        if i + 1 < b.len() && !b[i + 1].is_ascii_whitespace() {
                            return Err("falta un espacio después de las comillas".into());
                        }
                    }
                    _ => cur.push(c),
                },
                Some(_) => match c {
                    b'\\' if i + 1 < b.len() && b[i + 1] == b'\'' => {
                        cur.push(b'\'');
                        i += 2;
                        continue;
                    }
                    b'\'' => {
                        quote = None;
                        if i + 1 < b.len() && !b[i + 1].is_ascii_whitespace() {
                            return Err("falta un espacio después de las comillas".into());
                        }
                    }
                    _ => cur.push(c),
                },
                None => match c {
                    b'"' | b'\'' => quote = Some(c),
                    c if c.is_ascii_whitespace() => break,
                    _ => cur.push(c),
                },
            }
            i += 1;
        }
        args.push(cur);
        to = i;
    }
}

fn is_hex(c: u8) -> bool {
    c.is_ascii_hexdigit()
}

fn hex(c: u8) -> u8 {
    (c as char).to_digit(16).unwrap_or(0) as u8
}

/// A key as the editor writes it: bare when it's plain, double-quoted and
/// escaped otherwise.
pub fn quote_arg(s: &str) -> String {
    let plain = !s.is_empty()
        && !s.starts_with('#')
        && s.chars()
            .all(|c| !c.is_whitespace() && !c.is_control() && c != '"' && c != '\'' && c != '\\');
    if plain {
        return s.to_string();
    }
    let mut out = String::with_capacity(s.len() + 2);
    out.push('"');
    for c in s.chars() {
        match c {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            c if c.is_control() => out.push_str(&format!("\\x{:02x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// Commands that only read. Container commands (CONFIG, OBJECT…) list the
/// subcommands that read.
const READS: &[&str] = &[
    // keys and strings
    "GET",
    "MGET",
    "GETRANGE",
    "SUBSTR",
    "STRLEN",
    "LCS",
    "EXISTS",
    "TYPE",
    "TTL",
    "PTTL",
    "EXPIRETIME",
    "PEXPIRETIME",
    "KEYS",
    "SCAN",
    "RANDOMKEY",
    "DBSIZE",
    "DUMP",
    "TOUCH",
    "GETBIT",
    "BITCOUNT",
    "BITPOS",
    "BITFIELD_RO",
    "SORT_RO",
    // hashes
    "HGET",
    "HMGET",
    "HGETALL",
    "HKEYS",
    "HVALS",
    "HLEN",
    "HEXISTS",
    "HSTRLEN",
    "HSCAN",
    "HRANDFIELD",
    "HTTL",
    "HPTTL",
    "HEXPIRETIME",
    "HPEXPIRETIME",
    // lists
    "LRANGE",
    "LINDEX",
    "LLEN",
    "LPOS",
    // sets
    "SMEMBERS",
    "SISMEMBER",
    "SMISMEMBER",
    "SCARD",
    "SSCAN",
    "SRANDMEMBER",
    "SINTER",
    "SINTERCARD",
    "SUNION",
    "SDIFF",
    // sorted sets
    "ZRANGE",
    "ZRANGEBYSCORE",
    "ZRANGEBYLEX",
    "ZREVRANGE",
    "ZREVRANGEBYSCORE",
    "ZREVRANGEBYLEX",
    "ZSCORE",
    "ZMSCORE",
    "ZRANK",
    "ZREVRANK",
    "ZCARD",
    "ZCOUNT",
    "ZLEXCOUNT",
    "ZSCAN",
    "ZRANDMEMBER",
    "ZINTER",
    "ZUNION",
    "ZDIFF",
    "ZINTERCARD",
    // streams
    "XRANGE",
    "XREVRANGE",
    "XLEN",
    "XREAD",
    "XPENDING",
    // geo, hyperloglog
    "GEOPOS",
    "GEODIST",
    "GEOHASH",
    "GEORADIUS_RO",
    "GEORADIUSBYMEMBER_RO",
    "GEOSEARCH",
    "PFCOUNT",
    // server and connection
    "INFO",
    "PING",
    "ECHO",
    "TIME",
    "LASTSAVE",
    "ROLE",
    "LOLWUT",
    "SELECT",
    "COMMAND",
    "EVAL_RO",
    "EVALSHA_RO",
    "FCALL_RO",
    // modules: RedisJSON, RediSearch, RedisTimeSeries, RedisBloom
    "JSON.GET",
    "JSON.MGET",
    "JSON.TYPE",
    "JSON.STRLEN",
    "JSON.ARRLEN",
    "JSON.ARRINDEX",
    "JSON.OBJKEYS",
    "JSON.OBJLEN",
    "JSON.RESP",
    "FT.SEARCH",
    "FT.AGGREGATE",
    "FT.INFO",
    "FT._LIST",
    "FT.EXPLAIN",
    "FT.PROFILE",
    "TS.GET",
    "TS.MGET",
    "TS.RANGE",
    "TS.REVRANGE",
    "TS.MRANGE",
    "TS.MREVRANGE",
    "TS.INFO",
    "TS.QUERYINDEX",
    "BF.EXISTS",
    "BF.MEXISTS",
    "BF.INFO",
    "CF.EXISTS",
    "CF.MEXISTS",
    "CF.COUNT",
    "CF.INFO",
    "CMS.QUERY",
    "CMS.INFO",
    "TOPK.QUERY",
    "TOPK.LIST",
    "TOPK.INFO",
];

const READ_SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("CONFIG", &["GET", "HELP"]),
    (
        "OBJECT",
        &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT", "HELP"],
    ),
    (
        "MEMORY",
        &["USAGE", "STATS", "DOCTOR", "MALLOC-STATS", "HELP"],
    ),
    ("CLIENT", &["LIST", "INFO", "GETNAME", "ID", "HELP"]),
    ("SLOWLOG", &["GET", "LEN", "HELP"]),
    (
        "ACL",
        &["WHOAMI", "LIST", "USERS", "CAT", "GETUSER", "HELP"],
    ),
    (
        "CLUSTER",
        &[
            "INFO",
            "NODES",
            "SLOTS",
            "SHARDS",
            "MYID",
            "KEYSLOT",
            "COUNTKEYSINSLOT",
            "HELP",
        ],
    ),
    ("XINFO", &["STREAM", "GROUPS", "CONSUMERS", "HELP"]),
    ("MODULE", &["LIST", "HELP"]),
    ("FUNCTION", &["LIST", "DUMP", "STATS", "HELP"]),
    ("SCRIPT", &["EXISTS", "HELP"]),
    (
        "LATENCY",
        &["LATEST", "HISTORY", "DOCTOR", "GRAPH", "HISTOGRAM", "HELP"],
    ),
    (
        "PUBSUB",
        &[
            "CHANNELS",
            "NUMSUB",
            "NUMPAT",
            "SHARDCHANNELS",
            "SHARDNUMSUB",
            "HELP",
        ],
    ),
];

/// Whether a command (its first two arguments, any case) only reads.
pub fn is_read(args: &[Vec<u8>]) -> bool {
    let Some(name) = args.first() else {
        return true;
    };
    let name = String::from_utf8_lossy(name).to_ascii_uppercase();
    if READS.contains(&name.as_str()) {
        return true;
    }
    if let Some((_, subs)) = READ_SUBCOMMANDS.iter().find(|(c, _)| *c == name) {
        let sub = args
            .get(1)
            .map(|s| String::from_utf8_lossy(s).to_ascii_uppercase())
            .unwrap_or_default();
        return subs.contains(&sub.as_str());
    }
    false
}

/// Commands that turn the connection into a push channel or block it for
/// good: the editor can't show them.
pub fn is_streaming(name: &str) -> bool {
    matches!(
        name,
        "SUBSCRIBE"
            | "PSUBSCRIBE"
            | "SSUBSCRIBE"
            | "MONITOR"
            | "SYNC"
            | "PSYNC"
            | "RESET"
            | "QUIT"
            | "HELLO"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn placed_commands() {
        let p = parse_placed("SET a 1\r\n# c\n\n  GET \"é\"\nDEL a").unwrap();
        let v: Vec<(usize, usize, u32)> = p.iter().map(|c| (c.start, c.end, c.line)).collect();
        assert_eq!(v, [(0, 7, 1), (16, 24, 4), (25, 30, 5)]);
        assert_eq!(
            parse_placed("GET a\nGET \"open").err(),
            Some(("comillas sin cerrar".to_string(), 2, 6))
        );
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
    /// order, starts and lines.
    fn agrees(s: &str) {
        let placed = parse_placed(s).unwrap();
        let units = split(s);
        assert_eq!(units.len(), placed.len(), "{s:?}");
        for (u, p) in units.iter().zip(&placed) {
            assert_eq!((u.start, u.end, u.line), (p.start, p.end, p.line), "{s:?}");
            assert!(u.error.is_none());
            assert_eq!(parse_line(&u.text).unwrap(), p.args);
        }
    }

    #[test]
    fn one_unit_per_command_line() {
        // The audit's scripts: one unit per line.
        assert_eq!(texts("SET a 1\nGET a"), ["SET a 1", "GET a"]);
        assert_eq!(texts("SET k \"a;b\"\nGET k"), ["SET k \"a;b\"", "GET k"]);
        assert_eq!(texts("MULTI\nINCR a\nEXEC"), ["MULTI", "INCR a", "EXEC"]);
        assert_eq!(
            units("SET a 1\nGET a\nDEL a"),
            [
                ("SET a 1".into(), 0, 7, 1),
                ("GET a".into(), 8, 13, 2),
                ("DEL a".into(), 14, 19, 3)
            ]
        );
        for s in [
            "SET a 1\nGET a",
            "SET k \"a;b\"\nGET k",
            "MULTI\nINCR a\nEXEC",
            "SET a 1\nGET a\nDEL a",
        ] {
            agrees(s);
        }
    }

    #[test]
    fn quotes_comments_blank_lines_and_crlf() {
        // Separators of other languages inside quotes, `\'` and escapes.
        let s = "SET k 'it\\'s; GO' \nHSET h f \"a\\\"b # c\" # trailing\n";
        assert_eq!(texts(s), ["SET k 'it\\'s; GO'", "HSET h f \"a\\\"b # c\""]);
        agrees(s);
        // Comment lines and blank lines aren't units, nor part of one; `//`
        // isn't a comment in redis-cli, so it is a command.
        let s = "# first\n\n   # indented\nGET a\n\n// not a comment\n";
        assert_eq!(
            units(s),
            [
                ("GET a".into(), 23, 28, 4),
                ("// not a comment".into(), 30, 46, 6)
            ]
        );
        agrees(s);
        // A `#` inside an argument doesn't comment.
        assert_eq!(texts("GET a#b # c"), ["GET a#b"]);
        // CRLF and indentation: the unit starts at its first argument and
        // ends at its last.
        let s = "SET a 1\r\n\r\n\t GET  \"é\"  \r\nEVAL \"return 1\" 0\r\n";
        assert_eq!(
            units(s),
            [
                ("SET a 1".into(), 0, 7, 1),
                ("GET  \"é\"".into(), 13, 22, 3),
                ("EVAL \"return 1\" 0".into(), 26, 43, 4)
            ]
        );
        agrees(s);
        assert!(split("").is_empty());
        assert!(split("# only\n\n").is_empty());
    }

    #[test]
    fn a_line_that_does_not_parse_is_still_a_unit() {
        // A quote doesn't span lines: the next line is its own unit.
        let s = "GET a\n  SET k \"open  \nGET b";
        let u = split(s);
        assert_eq!(u.len(), 3);
        assert_eq!(
            (u[1].text.as_str(), u[1].start, u[1].line),
            ("SET k \"open", 8, 2)
        );
        assert_eq!(u[2].text, "GET b");
        // No unit carries an error (the editor keeps every lens while a
        // command is being typed): `execute` refuses that script at the
        // same line, and that unit alone with the same message.
        assert!(u.iter().all(|u| u.error.is_none()));
        assert_eq!(parse_placed(s).err().map(|e| e.1), Some(2));
        assert_eq!(
            parse_placed(&u[1].text).err().map(|e| e.0).as_deref(),
            Some("comillas sin cerrar")
        );
        let bad = split("GET \"a\"b");
        assert!(bad.len() == 1 && bad[0].error.is_none());
        assert!(parse_placed("GET \"a\"b").is_err());
    }

    fn words(line: &str) -> Vec<String> {
        parse_line(line)
            .unwrap()
            .into_iter()
            .map(|a| String::from_utf8(a).unwrap())
            .collect()
    }

    #[test]
    fn splits_on_whitespace_and_quotes() {
        assert_eq!(words("SET  key   value"), ["SET", "key", "value"]);
        assert_eq!(words(r#"SET "a key" 'it\'s'"#), ["SET", "a key", "it's"]);
        assert_eq!(
            words(r#"SET k "line\nbreak \"q\" \x41""#),
            ["SET", "k", "line\nbreak \"q\" A"]
        );
        assert_eq!(words(r#"GET ab"c d""#), ["GET", "abc d"]);
        assert_eq!(words(r#"SET k """#), ["SET", "k", ""]);
    }

    #[test]
    fn comments_and_blank_lines_are_skipped() {
        let s = parse_script("# header\n\nGET a # trailing\n  # indented\nHGETALL \"#h\"").unwrap();
        assert_eq!(s.len(), 2);
        assert_eq!(s[0], vec![b"GET".to_vec(), b"a".to_vec()]);
        assert_eq!(s[1][1], b"#h".to_vec());
    }

    #[test]
    fn bad_quoting_is_an_error() {
        assert!(parse_script("GET \"open").unwrap_err().contains("línea 1"));
        assert!(parse_line("GET \"a\"b").is_err());
    }

    #[test]
    fn binary_escapes() {
        assert_eq!(
            parse_line(r#"GET "\xff\x00""#).unwrap()[1],
            vec![0xff, 0x00]
        );
    }

    #[test]
    fn quoting_round_trips() {
        for k in [
            "plain:key",
            "with space",
            "q\"uote",
            "back\\slash",
            "#hash",
            "",
            "nl\nx",
            "it's",
        ] {
            let line = format!("GET {}", quote_arg(k));
            assert_eq!(parse_line(&line).unwrap()[1], k.as_bytes(), "{line}");
        }
        assert_eq!(quote_arg("user:1"), "user:1");
    }

    #[test]
    fn read_only_allow_list() {
        let read = |l: &str| is_read(&parse_line(l).unwrap());
        assert!(read("get a"));
        assert!(read("HGETALL h"));
        assert!(read("config get databases"));
        assert!(read("json.get doc $"));
        assert!(!read("SET a 1"));
        assert!(!read("config set maxmemory 1"));
        assert!(!read("FLUSHALL"));
        assert!(!read("EVAL \"return 1\" 0"));
        assert!(!read("DEL a"));
    }
}
