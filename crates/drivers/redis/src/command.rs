//! The editor's language: one Redis command per line, quoted like
//! `redis-cli` (`"…"` with backslash escapes, `'…'` with only `\'`), and
//! `#` starting a comment. Also the read-only allow-list.

/// Commands of a script, each as its raw arguments. A `#` at the start of
/// an argument (outside quotes) comments out the rest of the line.
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

pub(crate) fn parse_line(line: &str) -> Result<Vec<Vec<u8>>, String> {
    let b = line.as_bytes();
    let mut args = Vec::new();
    let mut i = 0;
    loop {
        while i < b.len() && b[i].is_ascii_whitespace() {
            i += 1;
        }
        if i >= b.len() || b[i] == b'#' {
            return Ok(args);
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
        && s.chars().all(|c| !c.is_whitespace() && !c.is_control() && c != '"' && c != '\'' && c != '\\');
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
    "GET", "MGET", "GETRANGE", "SUBSTR", "STRLEN", "LCS", "EXISTS", "TYPE", "TTL", "PTTL", "EXPIRETIME",
    "PEXPIRETIME", "KEYS", "SCAN", "RANDOMKEY", "DBSIZE", "DUMP", "TOUCH", "GETBIT", "BITCOUNT", "BITPOS",
    "BITFIELD_RO", "SORT_RO",
    // hashes
    "HGET", "HMGET", "HGETALL", "HKEYS", "HVALS", "HLEN", "HEXISTS", "HSTRLEN", "HSCAN", "HRANDFIELD", "HTTL",
    "HPTTL", "HEXPIRETIME", "HPEXPIRETIME",
    // lists
    "LRANGE", "LINDEX", "LLEN", "LPOS",
    // sets
    "SMEMBERS", "SISMEMBER", "SMISMEMBER", "SCARD", "SSCAN", "SRANDMEMBER", "SINTER", "SINTERCARD", "SUNION",
    "SDIFF",
    // sorted sets
    "ZRANGE", "ZRANGEBYSCORE", "ZRANGEBYLEX", "ZREVRANGE", "ZREVRANGEBYSCORE", "ZREVRANGEBYLEX", "ZSCORE",
    "ZMSCORE", "ZRANK", "ZREVRANK", "ZCARD", "ZCOUNT", "ZLEXCOUNT", "ZSCAN", "ZRANDMEMBER", "ZINTER", "ZUNION",
    "ZDIFF", "ZINTERCARD",
    // streams
    "XRANGE", "XREVRANGE", "XLEN", "XREAD", "XPENDING",
    // geo, hyperloglog
    "GEOPOS", "GEODIST", "GEOHASH", "GEORADIUS_RO", "GEORADIUSBYMEMBER_RO", "GEOSEARCH", "PFCOUNT",
    // server and connection
    "INFO", "PING", "ECHO", "TIME", "LASTSAVE", "ROLE", "LOLWUT", "SELECT", "COMMAND", "EVAL_RO", "EVALSHA_RO",
    "FCALL_RO",
    // modules: RedisJSON, RediSearch, RedisTimeSeries, RedisBloom
    "JSON.GET", "JSON.MGET", "JSON.TYPE", "JSON.STRLEN", "JSON.ARRLEN", "JSON.ARRINDEX", "JSON.OBJKEYS",
    "JSON.OBJLEN", "JSON.RESP", "FT.SEARCH", "FT.AGGREGATE", "FT.INFO", "FT._LIST", "FT.EXPLAIN", "FT.PROFILE",
    "TS.GET", "TS.MGET", "TS.RANGE", "TS.REVRANGE", "TS.MRANGE", "TS.MREVRANGE", "TS.INFO", "TS.QUERYINDEX",
    "BF.EXISTS", "BF.MEXISTS", "BF.INFO", "CF.EXISTS", "CF.MEXISTS", "CF.COUNT", "CF.INFO", "CMS.QUERY",
    "CMS.INFO", "TOPK.QUERY", "TOPK.LIST", "TOPK.INFO",
];

const READ_SUBCOMMANDS: &[(&str, &[&str])] = &[
    ("CONFIG", &["GET", "HELP"]),
    ("OBJECT", &["ENCODING", "FREQ", "IDLETIME", "REFCOUNT", "HELP"]),
    ("MEMORY", &["USAGE", "STATS", "DOCTOR", "MALLOC-STATS", "HELP"]),
    ("CLIENT", &["LIST", "INFO", "GETNAME", "ID", "HELP"]),
    ("SLOWLOG", &["GET", "LEN", "HELP"]),
    ("ACL", &["WHOAMI", "LIST", "USERS", "CAT", "GETUSER", "HELP"]),
    ("CLUSTER", &["INFO", "NODES", "SLOTS", "SHARDS", "MYID", "KEYSLOT", "COUNTKEYSINSLOT", "HELP"]),
    ("XINFO", &["STREAM", "GROUPS", "CONSUMERS", "HELP"]),
    ("MODULE", &["LIST", "HELP"]),
    ("FUNCTION", &["LIST", "DUMP", "STATS", "HELP"]),
    ("SCRIPT", &["EXISTS", "HELP"]),
    ("LATENCY", &["LATEST", "HISTORY", "DOCTOR", "GRAPH", "HISTOGRAM", "HELP"]),
    ("PUBSUB", &["CHANNELS", "NUMSUB", "NUMPAT", "SHARDCHANNELS", "SHARDNUMSUB", "HELP"]),
];

/// Whether a command (its first two arguments, any case) only reads.
pub fn is_read(args: &[Vec<u8>]) -> bool {
    let Some(name) = args.first() else { return true };
    let name = String::from_utf8_lossy(name).to_ascii_uppercase();
    if READS.contains(&name.as_str()) {
        return true;
    }
    if let Some((_, subs)) = READ_SUBCOMMANDS.iter().find(|(c, _)| *c == name) {
        let sub = args.get(1).map(|s| String::from_utf8_lossy(s).to_ascii_uppercase()).unwrap_or_default();
        return subs.contains(&sub.as_str());
    }
    false
}

/// Commands that turn the connection into a push channel or block it for
/// good: the editor can't show them.
pub fn is_streaming(name: &str) -> bool {
    matches!(
        name,
        "SUBSCRIBE" | "PSUBSCRIBE" | "SSUBSCRIBE" | "MONITOR" | "SYNC" | "PSYNC" | "RESET" | "QUIT" | "HELLO"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn words(line: &str) -> Vec<String> {
        parse_line(line).unwrap().into_iter().map(|a| String::from_utf8(a).unwrap()).collect()
    }

    #[test]
    fn splits_on_whitespace_and_quotes() {
        assert_eq!(words("SET  key   value"), ["SET", "key", "value"]);
        assert_eq!(words(r#"SET "a key" 'it\'s'"#), ["SET", "a key", "it's"]);
        assert_eq!(words(r#"SET k "line\nbreak \"q\" \x41""#), ["SET", "k", "line\nbreak \"q\" A"]);
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
        assert_eq!(parse_line(r#"GET "\xff\x00""#).unwrap()[1], vec![0xff, 0x00]);
    }

    #[test]
    fn quoting_round_trips() {
        for k in ["plain:key", "with space", "q\"uote", "back\\slash", "#hash", "", "nl\nx", "it's"] {
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
