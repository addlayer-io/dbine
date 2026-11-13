//! The editor's command completion (#97): each Redis, Valkey or Dragonfly
//! command is a `Command` entry, and container commands (`CLIENT`,
//! `CONFIG`…) list their subcommands as `CLIENT KILL`.
//!
//! `COMMAND DOCS` (Redis 7+, Valkey) gives every command the server has,
//! modules included, with its arguments and summary, in one round trip.
//! Without it, `COMMAND` gives the names and the static catalog
//! (`functions/redis.txt`, from Redis' command reference) their arguments,
//! and the subcommands of a container it lists bare, filtered by the Redis
//! version the server reports. When the server refuses both, the static
//! catalog alone, filtered by that version (all of it inside an open
//! `MULTI`, where any command would be queued into the user's
//! transaction).

use crate::shape::text_of;
use crate::RedisSession;
use dbine_driver::functions::{parse_catalog, FunctionInfo, FunctionKind};
use redis::Value;

const CATALOG: &str = include_str!("functions/redis.txt");

/// Commands Redis' reference marks internal (`syscmd`), for the `COMMAND`
/// reply, which doesn't say so; and Redis 6's guards against cross-protocol
/// attacks (`POST`, `HOST:`), which `COMMAND` lists as commands.
const INTERNAL: &[&str] = &[
    "debug",
    "pfdebug",
    "pfselftest",
    "replconf",
    "restore-asking",
    "post",
    "host:",
];

pub async fn catalog(s: &mut RedisSession) -> Vec<FunctionInfo> {
    // Inside an open MULTI the server would queue these reads into the
    // user's transaction.
    if s.multi {
        return static_catalog(None);
    }
    if let Ok(reply) = s.run(&[b"COMMAND", b"DOCS"]).await {
        let docs = from_docs(reply);
        if !docs.is_empty() {
            return docs;
        }
    }
    let (command, info) = command_and_info(s).await;
    let version = info.and_then(|v| redis_version(&text_of(&v)));
    if let Some(reply) = command {
        let names = command_names(reply);
        if !names.is_empty() {
            return from_names(&names, version.as_deref());
        }
    }
    static_catalog(version.as_deref())
}

/// `COMMAND` and `INFO server` in one round trip, each `None` when the
/// server refuses it: an older server's `COMMAND` lists containers without
/// their subcommands, and the catalog's are filtered by its version.
async fn command_and_info(s: &mut RedisSession) -> (Option<Value>, Option<Value>) {
    let mut pipe = redis::pipe();
    pipe.cmd("COMMAND")
        .cmd("INFO")
        .arg("server")
        .ignore_errors();
    match pipe
        .query_async::<Vec<redis::RedisResult<Value>>>(&mut s.conn)
        .await
    {
        Ok(replies) => {
            let mut it = replies.into_iter().map(Result::ok);
            (it.next().flatten(), it.next().flatten())
        }
        Err(_) => (None, None),
    }
}

/// The static catalog for a Redis `version` (all of it when unknown).
pub fn static_catalog(version: Option<&str>) -> Vec<FunctionInfo> {
    let mut v: Vec<FunctionInfo> = parse_catalog(CATALOG, version)
        .into_iter()
        .map(|mut f| {
            f.name = display_name(&f.name);
            f
        })
        .collect();
    v.sort_by(|a, b| a.name.cmp(&b.name));
    v
}

/// `redis_version` of `INFO server`: Valkey and Dragonfly report the
/// Redis version they match there too.
fn redis_version(info: &str) -> Option<String> {
    info.lines()
        .find_map(|l| l.trim().strip_prefix("redis_version:"))
        .map(|v| v.trim().to_string())
        .filter(|v| !v.is_empty())
}

/// `client|kill` → `CLIENT KILL`.
fn display_name(name: &str) -> String {
    name.replace('|', " ").to_ascii_uppercase()
}

/// A reply's field → value pairs: a RESP3 map or RESP2's flat array.
fn pairs(v: Value) -> Vec<(String, Value)> {
    match v {
        Value::Map(m) => m.into_iter().map(|(k, v)| (text_of(&k), v)).collect(),
        Value::Array(a) => {
            let mut it = a.into_iter();
            let mut out = Vec::new();
            while let (Some(k), Some(v)) = (it.next(), it.next()) {
                out.push((text_of(&k), v));
            }
            out
        }
        _ => Vec::new(),
    }
}

fn items(v: Value) -> Vec<Value> {
    match v {
        Value::Array(a) | Value::Set(a) => a,
        _ => Vec::new(),
    }
}

/// `COMMAND DOCS`: name → its documentation.
pub fn from_docs(reply: Value) -> Vec<FunctionInfo> {
    let mut out = Vec::new();
    for (name, doc) in pairs(reply) {
        doc_entries(&name, doc, &mut out);
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out
}

fn doc_entries(name: &str, doc: Value, out: &mut Vec<FunctionInfo>) {
    let mut summary = String::new();
    let mut args = Vec::new();
    let mut subcommands = Vec::new();
    for (k, v) in pairs(doc) {
        match k.as_str() {
            "summary" => summary = text_of(&v),
            "arguments" => args = items(v).into_iter().map(Arg::from).collect(),
            "subcommands" => subcommands = pairs(v),
            "doc_flags" if items(v).iter().any(|f| text_of(f) == "syscmd") => return,
            _ => {}
        }
    }
    if !subcommands.is_empty() {
        for (sub, d) in subcommands {
            doc_entries(&sub, d, out);
        }
        return;
    }
    let args = args.iter().map(Arg::syntax).collect::<Vec<_>>().join(" ");
    out.push(
        FunctionInfo::new(display_name(name), FunctionKind::Command)
            .args(args)
            .description(summary),
    );
}

/// One argument of `COMMAND DOCS`.
#[derive(Default)]
struct Arg {
    name: String,
    kind: String,
    token: Option<String>,
    optional: bool,
    multiple: bool,
    multiple_token: bool,
    args: Vec<Arg>,
}

impl From<Value> for Arg {
    fn from(v: Value) -> Arg {
        let mut a = Arg::default();
        let mut display = None;
        for (k, v) in pairs(v) {
            match k.as_str() {
                "name" => a.name = text_of(&v),
                "display_text" => display = Some(text_of(&v)),
                "type" => a.kind = text_of(&v),
                "token" => a.token = Some(text_of(&v)),
                "flags" => {
                    for f in items(v) {
                        match text_of(&f).as_str() {
                            "optional" => a.optional = true,
                            "multiple" => a.multiple = true,
                            "multiple_token" => a.multiple_token = true,
                            _ => {}
                        }
                    }
                }
                "arguments" => a.args = items(v).into_iter().map(Arg::from).collect(),
                _ => {}
            }
        }
        if let Some(d) = display.filter(|d| !d.is_empty()) {
            a.name = d;
        }
        a
    }
}

impl Arg {
    /// The argument as Redis' reference writes it: `[NX | XX]`,
    /// `field value [field value ...]`, `<SET encoding offset value | …>`.
    fn syntax(&self) -> String {
        let args = match self.kind.as_str() {
            "block" => self
                .args
                .iter()
                .map(Arg::syntax)
                .collect::<Vec<_>>()
                .join(" "),
            "oneof" => self
                .args
                .iter()
                .map(Arg::syntax)
                .collect::<Vec<_>>()
                .join(" | "),
            "pure-token" => String::new(),
            _ => self.name.clone(),
        };
        let oneof = self.kind == "oneof" && !self.optional;
        let mut s = String::new();
        if self.optional {
            s.push('[');
        }
        if let Some(t) = &self.token {
            s.push_str(t);
            if self.kind != "pure-token" {
                s.push(' ');
            }
        }
        if oneof {
            s.push('<');
        }
        if self.multiple {
            let token = match (&self.token, self.multiple_token) {
                (Some(t), true) => format!("{t} "),
                _ => String::new(),
            };
            s.push_str(&format!("{args} [{token}{args} ...]"));
        } else {
            s.push_str(&args);
        }
        if oneof {
            s.truncate(s.trim_end().len());
            s.push('>');
        }
        if self.optional {
            s.truncate(s.trim_end().len());
            s.push(']');
        }
        s
    }
}

/// `COMMAND`'s names, each container's subcommands instead of the
/// container when the server lists them (Redis 7+: the 10th field).
pub fn command_names(reply: Value) -> Vec<String> {
    let mut out = Vec::new();
    for entry in items(reply) {
        let mut fields = items(entry).into_iter();
        let Some(name) = fields.next().map(|n| text_of(&n)) else {
            continue;
        };
        let subs: Vec<String> = fields
            .nth(8)
            .map(items)
            .unwrap_or_default()
            .into_iter()
            .filter_map(|s| items(s).into_iter().next().map(|n| text_of(&n)))
            .collect();
        if subs.is_empty() {
            out.push(name);
        } else {
            out.extend(subs);
        }
    }
    out
}

/// The server's command names, with the static catalog's arguments where
/// it has them. A container the server lists without its subcommands
/// (older servers, Dragonfly) gets the catalog's that the server's Redis
/// `version` has (all of them when unknown).
pub fn from_names(names: &[String], version: Option<&str>) -> Vec<FunctionInfo> {
    // A name the server lists exists whatever its version says.
    let all = static_catalog(None);
    let have = static_catalog(version);
    let mut out = Vec::new();
    for n in names {
        if INTERNAL.contains(&n.to_ascii_lowercase().as_str()) {
            continue;
        }
        let name = display_name(n);
        if let Some(f) = all.iter().find(|f| f.name == name) {
            out.push(f.clone());
            continue;
        }
        let prefix = format!("{name} ");
        let subs: Vec<FunctionInfo> = have
            .iter()
            .filter(|f| f.name.starts_with(&prefix))
            .cloned()
            .collect();
        if subs.is_empty() || name.contains(' ') {
            out.push(FunctionInfo::new(name, FunctionKind::Command));
        } else {
            out.extend(subs);
        }
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    out.dedup_by(|a, b| a.name == b.name);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn bulk(s: &str) -> Value {
        Value::BulkString(s.as_bytes().to_vec())
    }

    fn arr(v: Vec<Value>) -> Value {
        Value::Array(v)
    }

    /// A RESP2 flat array of field / value.
    fn flat(pairs: Vec<(&str, Value)>) -> Value {
        arr(pairs.into_iter().flat_map(|(k, v)| [bulk(k), v]).collect())
    }

    /// The same as a RESP3 map.
    fn map(pairs: Vec<(&str, Value)>) -> Value {
        Value::Map(pairs.into_iter().map(|(k, v)| (bulk(k), v)).collect())
    }

    fn flags(f: &[&str]) -> Value {
        arr(f
            .iter()
            .map(|s| Value::SimpleString(s.to_string()))
            .collect())
    }

    #[test]
    fn the_catalog_parses_line_by_line() {
        for line in CATALOG.lines() {
            let line = line.trim();
            if line.is_empty() || line.starts_with('#') {
                continue;
            }
            assert_eq!(parse_catalog(line, None).len(), 1, "unreadable: {line}");
        }
        let all = static_catalog(None);
        assert!(all.len() > 300);
        assert!(all.iter().all(|f| f.kind == FunctionKind::Command));
        assert!(all.iter().all(|f| f.description.is_empty()));
        let hset = all.iter().find(|f| f.name == "HSET").unwrap();
        assert_eq!(hset.args, "key field value [field value ...]");
        assert!(all.iter().any(|f| f.name == "CLIENT KILL"));
        assert!(!all
            .iter()
            .any(|f| f.name == "CLIENT" || f.name == "REPLCONF"));
    }

    #[test]
    fn the_static_catalog_follows_the_redis_version() {
        let has = |v: Option<&str>, name: &str| static_catalog(v).iter().any(|f| f.name == name);
        assert!(has(Some("7.4.2"), "HEXPIRE"));
        assert!(!has(Some("7.2.4"), "HEXPIRE"));
        assert!(has(Some("7.2.4"), "FUNCTION LOAD"));
        assert!(!has(Some("6.2.14"), "FUNCTION LOAD"));
        assert!(!has(Some("6.0.0"), "GETEX"));
        assert!(has(Some("8.0.2"), "HGETEX"));
        assert!(!has(Some("7.4.2"), "HGETEX"));
        assert_eq!(
            redis_version("# Server\r\nredis_version:7.2.4\r\nvalkey_version:8.0.1\r\n"),
            Some("7.2.4".into())
        );
    }

    #[test]
    fn command_docs_map_to_commands() {
        let hset_args = |wrap: fn(Vec<(&str, Value)>) -> Value| {
            arr(vec![
                wrap(vec![
                    ("name", bulk("key")),
                    ("type", bulk("key")),
                    ("display_text", bulk("key")),
                ]),
                wrap(vec![
                    ("name", bulk("data")),
                    ("type", bulk("block")),
                    ("flags", flags(&["multiple"])),
                    (
                        "arguments",
                        arr(vec![
                            wrap(vec![("name", bulk("field")), ("type", bulk("string"))]),
                            wrap(vec![("name", bulk("value")), ("type", bulk("string"))]),
                        ]),
                    ),
                ]),
            ])
        };
        for wrap in [flat as fn(Vec<(&str, Value)>) -> Value, map] {
            let reply = wrap(vec![
                (
                    "hset",
                    wrap(vec![
                        (
                            "summary",
                            bulk("Creates or modifies the value of a field in a hash."),
                        ),
                        ("since", bulk("2.0.0")),
                        ("arguments", hset_args(wrap)),
                    ]),
                ),
                (
                    "set",
                    wrap(vec![(
                        "arguments",
                        arr(vec![
                            wrap(vec![("name", bulk("key")), ("type", bulk("key"))]),
                            wrap(vec![
                                ("name", bulk("condition")),
                                ("type", bulk("oneof")),
                                ("flags", flags(&["optional"])),
                                (
                                    "arguments",
                                    arr(vec![
                                        wrap(vec![
                                            ("name", bulk("nx")),
                                            ("type", bulk("pure-token")),
                                            ("token", bulk("NX")),
                                        ]),
                                        wrap(vec![
                                            ("name", bulk("xx")),
                                            ("type", bulk("pure-token")),
                                            ("token", bulk("XX")),
                                        ]),
                                    ]),
                                ),
                            ]),
                            wrap(vec![
                                ("name", bulk("numfields")),
                                ("type", bulk("integer")),
                                ("token", bulk("FIELDS")),
                            ]),
                        ]),
                    )]),
                ),
                (
                    "client",
                    wrap(vec![(
                        "subcommands",
                        wrap(vec![
                            (
                                "client|kill",
                                wrap(vec![("summary", bulk("Terminates open connections."))]),
                            ),
                            ("client|list", wrap(vec![])),
                        ]),
                    )]),
                ),
                ("replconf", wrap(vec![("doc_flags", flags(&["syscmd"]))])),
            ]);
            let got = from_docs(reply);
            let names: Vec<&str> = got.iter().map(|f| f.name.as_str()).collect();
            assert_eq!(names, ["CLIENT KILL", "CLIENT LIST", "HSET", "SET"]);
            assert_eq!(got[0].description, "Terminates open connections.");
            assert_eq!(got[2].args, "key field value [field value ...]");
            assert_eq!(
                got[2].description,
                "Creates or modifies the value of a field in a hash."
            );
            assert_eq!(got[3].args, "key [NX | XX] FIELDS numfields");
            assert!(got.iter().all(|f| f.kind == FunctionKind::Command));
        }
    }

    #[test]
    fn required_choices_and_repeated_tokens_read_as_the_reference() {
        let a = Arg {
            kind: "oneof".into(),
            args: vec![
                Arg {
                    name: "id".into(),
                    kind: "string".into(),
                    ..Arg::default()
                },
                Arg {
                    kind: "pure-token".into(),
                    token: Some("*".into()),
                    ..Arg::default()
                },
            ],
            ..Arg::default()
        };
        assert_eq!(a.syntax(), "<id | *>");
        let b = Arg {
            name: "pattern".into(),
            kind: "pattern".into(),
            token: Some("GET".into()),
            optional: true,
            multiple: true,
            multiple_token: true,
            ..Arg::default()
        };
        assert_eq!(b.syntax(), "[GET pattern [GET pattern ...]]");
    }

    #[test]
    fn command_names_use_the_catalogs_arguments() {
        let entry = |name: &str, subs: Vec<Value>| {
            let mut e = vec![bulk(name), Value::Int(-1), arr(vec![])];
            e.extend((0..6).map(|_| Value::Int(0)));
            e.push(arr(subs));
            arr(e)
        };
        // Redis 6: no subcommand field at all.
        let old = |name: &str| arr(vec![bulk(name), Value::Int(2), arr(vec![])]);
        let reply = arr(vec![
            old("get"),
            entry("hset", vec![]),
            entry("object", vec![entry("object|encoding", vec![])]),
            entry("config", vec![]),
            entry("my.module", vec![]),
            entry("replconf", vec![]),
        ]);
        let names = command_names(reply);
        assert_eq!(
            names,
            [
                "get",
                "hset",
                "object|encoding",
                "config",
                "my.module",
                "replconf"
            ]
        );
        let got = from_names(&names, None);
        let find = |n: &str| got.iter().find(|f| f.name == n);
        assert_eq!(
            find("HSET").unwrap().args,
            "key field value [field value ...]"
        );
        assert_eq!(find("OBJECT ENCODING").unwrap().args, "key");
        assert!(find("OBJECT FREQ").is_none());
        // CONFIG without its subcommands: the catalog's.
        assert!(find("CONFIG GET").is_some() && find("CONFIG").is_none());
        assert_eq!(find("MY.MODULE").unwrap().args, "");
        assert!(find("REPLCONF").is_none());

        // Redis 6.2: CLIENT without its subcommands gets the catalog's of
        // 6.2 only, and its cross-protocol guards aren't commands.
        let names = command_names(arr(vec![
            old("client"),
            old("getex"),
            old("hgetex"),
            old("post"),
            old("host:"),
        ]));
        let got = from_names(&names, Some("6.2.14"));
        let find = |n: &str| got.iter().find(|f| f.name == n);
        assert!(find("CLIENT INFO").is_some() && find("CLIENT KILL").is_some());
        assert!(find("CLIENT NO-EVICT").is_none() && find("CLIENT SETINFO").is_none());
        assert!(find("GETEX").unwrap().args.starts_with("key [EX seconds"));
        // A command the server lists keeps its arguments even past the
        // version it reports (Dragonfly reports an older Redis).
        assert!(find("HGETEX").unwrap().args.starts_with("key [EX seconds"));
        assert!(find("POST").is_none() && find("HOST:").is_none());
        // With no version known, all of them.
        let got = from_names(&names, None);
        assert!(got.iter().any(|f| f.name == "CLIENT NO-EVICT"));
    }
}
