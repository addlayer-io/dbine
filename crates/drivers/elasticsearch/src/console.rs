//! Kibana Dev Tools console syntax, shared with the Solr driver:
//!
//! ```text
//! # a comment
//! GET /my-index/_search
//! {
//!   "query": { "match_all": {} }
//! }
//!
//! POST /_bulk
//! {"index": {"_index": "x"}}
//! {"a": 1}
//!
//! SELECT * FROM "my-index" LIMIT 10
//! ```
//!
//! A request is a `METHOD /path?query` line plus an optional body (JSON,
//! possibly multi-line, or NDJSON). It ends at a blank line (once the body's
//! braces are balanced) or at the next request / SQL line. Lines starting
//! with `SELECT`, `SHOW`, `DESCRIBE` or `DESC` start a SQL statement, which
//! ends at a blank line, a trailing `;` or the next request line. Whole
//! lines starting with `#` or `//` are comments.

use dbine_driver::{ScriptStatement, StatementKind};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Command {
    Http(Request),
    Sql(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Request {
    /// Upper case.
    pub method: String,
    /// Always starts with `/`; includes the query string.
    pub path: String,
    /// Raw body text, trimmed; `None` when there's none.
    pub body: Option<String>,
}

impl Request {
    /// Path without the query string.
    pub fn path_only(&self) -> &str {
        self.path.split('?').next().unwrap_or("")
    }

    /// Value of a query-string parameter (not percent-decoded).
    pub fn query_param(&self, key: &str) -> Option<&str> {
        let q = self.path.split_once('?')?.1;
        q.split('&').find_map(|kv| {
            let (k, v) = kv.split_once('=').unwrap_or((kv, ""));
            (k == key).then_some(v)
        })
    }

    /// Path segments, without the query string.
    pub fn segments(&self) -> Vec<&str> {
        self.path_only()
            .split('/')
            .filter(|s| !s.is_empty())
            .collect()
    }

    /// The body holds more than one JSON value (one per line).
    pub fn is_ndjson(&self) -> bool {
        self.body.as_deref().is_some_and(|b| {
            let mut n = 0;
            for v in serde_json::Deserializer::from_str(b).into_iter::<serde_json::Value>() {
                if v.is_err() {
                    return false;
                }
                n += 1;
            }
            n > 1
        })
    }
}

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];
const SQL_WORDS: &[&str] = &["SELECT", "SHOW", "DESCRIBE", "DESC"];

fn first_word(line: &str) -> String {
    line.trim_start()
        .chars()
        .take_while(|c| c.is_ascii_alphabetic())
        .collect::<String>()
        .to_ascii_uppercase()
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//")
}

fn method_of(line: &str) -> Option<String> {
    let w = first_word(line);
    let rest = &line.trim_start()[w.len()..];
    (METHODS.contains(&w.as_str()) && (rest.is_empty() || rest.starts_with(char::is_whitespace)))
        .then_some(w)
}

fn is_sql_start(line: &str) -> bool {
    let w = first_word(line);
    let rest = &line.trim_start()[w.len()..];
    SQL_WORDS.contains(&w.as_str()) && (rest.is_empty() || rest.starts_with(char::is_whitespace))
}

/// Net `{`/`[` depth of `text`, ignoring those inside JSON strings.
fn depth(text: &str) -> i64 {
    let (mut d, mut in_str, mut esc) = (0i64, false, false);
    for c in text.chars() {
        if in_str {
            match (esc, c) {
                (true, _) => esc = false,
                (false, '\\') => esc = true,
                (false, '"') => in_str = false,
                _ => {}
            }
            continue;
        }
        match c {
            '"' => in_str = true,
            '{' | '[' => d += 1,
            '}' | ']' => d -= 1,
            _ => {}
        }
    }
    d
}

enum Cur {
    None,
    Req(String, String, Vec<String>),
    Sql(Vec<String>),
    /// A line that's neither a request nor SQL (its message), with the
    /// lines after it up to a blank line or the next request / SQL line.
    Bad(String),
}

/// Where the current command is: its first character and line, and the
/// end of its last non-blank line.
#[derive(Clone, Copy)]
struct Span {
    start: usize,
    line: u32,
    end: usize,
}

/// A command with where it is in the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub command: Command,
    /// Byte offset of its first line's first non-blank character.
    pub offset: usize,
    /// 1-based line of that character.
    pub line: u32,
    /// Byte offset where it ends: its last non-blank line's end, without an
    /// SQL statement's `;`.
    pub end: usize,
}

/// A line that's neither a request nor SQL, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
    pub line: u32,
    /// The end of the lines that go with it (up to a blank line or the
    /// next request / SQL line).
    pub end: usize,
}

fn finish(text: &str, cur: Cur, at: Span, out: &mut Vec<Result<Located, ParseError>>) {
    let mut end = at.end;
    let command = match cur {
        Cur::None => return,
        Cur::Bad(message) => {
            out.push(Err(ParseError {
                message,
                offset: at.start,
                end,
                line: at.line,
            }));
            return;
        }
        Cur::Req(method, path, body) => {
            let body = body.join("\n");
            let body = body.trim();
            let mut path = path;
            if !path.starts_with('/') {
                path.insert(0, '/');
            }
            Command::Http(Request {
                method,
                path,
                body: (!body.is_empty()).then(|| body.to_string()),
            })
        }
        Cur::Sql(lines) => {
            let s = lines.join("\n");
            let s = s.trim().trim_end_matches(';').trim();
            if s.is_empty() {
                return;
            }
            // Without its `;`, as the SQL splitter leaves a statement.
            end = at.start
                + text[at.start..end]
                    .trim_end_matches(|c: char| c == ';' || c.is_whitespace())
                    .len();
            Command::Sql(s.to_string())
        }
    };
    out.push(Ok(Located {
        command,
        offset: at.start,
        end,
        line: at.line,
    }));
}

/// Split a console script into requests and SQL statements.
/// `Err` names the first line that's neither.
pub fn parse(text: &str) -> Result<Vec<Command>, String> {
    parse_located(text)
        .map(|v| v.into_iter().map(|l| l.command).collect())
        .map_err(|e| e.message)
}

/// [`parse`], keeping where each command is in `text`.
pub fn parse_located(text: &str) -> Result<Vec<Located>, ParseError> {
    scan(text).into_iter().collect()
}

/// The script cut into the units `execute` runs and numbers, in the same
/// order and at the same places ([`parse_located`]): a request (its method
/// line and body) or an SQL statement (without its `;`). Comment and blank
/// lines before one aren't part of it. A line that's neither is still a
/// unit (with the lines that go with it), without an `error`, and the rest
/// of the script is still cut: a request being typed (`GE`, `GET` without
/// its path yet) keeps the lenses of the others, and running it reports
/// the parse error at its place (`execute` refuses the script).
pub fn split(text: &str) -> Vec<ScriptStatement> {
    scan(text)
        .into_iter()
        .map(|r| {
            let (start, end, line) = match r {
                Ok(l) => (l.offset, l.end, l.line),
                Err(e) => (e.offset, e.end, e.line),
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

/// Every command of the script in order, and an `Err` for each line that's
/// neither a request nor SQL (the scan goes on after it). Before the first
/// `Err` it is what [`parse_located`] returns.
fn scan(text: &str) -> Vec<Result<Located, ParseError>> {
    let mut out = Vec::new();
    let mut cur = Cur::None;
    let mut at = Span {
        start: 0,
        line: 1,
        end: 0,
    };
    let mut pos = 0usize;
    for (n, raw) in text.split_inclusive('\n').enumerate() {
        let start = pos;
        pos += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let here = Span {
            start: start + (line.len() - line.trim_start().len()),
            line: n as u32 + 1,
            end: start + line.trim_end().len(),
        };
        if is_comment(line) {
            continue;
        }
        let blank = line.trim().is_empty();
        // Inside an unbalanced JSON body anything but a request line is content.
        if let Cur::Req(_, _, body) = &mut cur {
            if depth(&body.join("\n")) > 0 && method_of(line).is_none() {
                body.push(line.to_string());
                if !blank {
                    at.end = here.end;
                }
                continue;
            }
        }
        if blank {
            finish(text, std::mem::replace(&mut cur, Cur::None), at, &mut out);
            continue;
        }
        if let Some(m) = method_of(line) {
            finish(text, std::mem::replace(&mut cur, Cur::None), at, &mut out);
            let path = line.trim_start()[m.len()..].trim().to_string();
            cur = if path.is_empty() {
                Cur::Bad(format!("Línea {}: falta la ruta después de {m}.", n + 1))
            } else {
                Cur::Req(m, path, Vec::new())
            };
            at = here;
            continue;
        }
        if is_sql_start(line) && !matches!(cur, Cur::Sql(_)) {
            finish(text, std::mem::replace(&mut cur, Cur::None), at, &mut out);
            cur = Cur::Sql(Vec::new());
            at = here;
        }
        at.end = here.end;
        match &mut cur {
            Cur::Req(_, _, body) => body.push(line.to_string()),
            Cur::Sql(lines) => {
                lines.push(line.to_string());
                if line.trim_end().ends_with(';') {
                    finish(text, std::mem::replace(&mut cur, Cur::None), at, &mut out);
                }
            }
            Cur::Bad(_) => {}
            Cur::None => {
                cur = Cur::Bad(format!(
                    "Línea {}: se esperaba una petición (GET /ruta, POST /ruta…) o una sentencia SQL: {}",
                    n + 1,
                    line.trim()
                ));
                at = here;
            }
        }
    }
    finish(text, cur, at, &mut out);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    fn req(c: &Command) -> &Request {
        match c {
            Command::Http(r) => r,
            Command::Sql(s) => panic!("expected a request, got SQL {s}"),
        }
    }

    #[test]
    fn multi_request_script() {
        let s = "# listado\nGET /_cat/indices\n\nGET /idx/_search\n{\n  \"query\": {\"match_all\": {}}\n}\nPOST idx/_count\n{\"query\":{\"term\":{\"a\":1}}}\n";
        let cmds = parse(s).unwrap();
        assert_eq!(cmds.len(), 3);
        assert_eq!(req(&cmds[0]).path, "/_cat/indices");
        assert_eq!(req(&cmds[0]).body, None);
        let r = req(&cmds[1]);
        assert_eq!(r.method, "GET");
        assert!(r.body.as_deref().unwrap().contains("match_all"));
        assert_eq!(req(&cmds[2]).path, "/idx/_count");
    }

    #[test]
    fn blank_line_inside_json_body_does_not_split() {
        let s =
            "GET /x/_search\n{\n  \"size\": 1,\n\n  \"query\": {\"match_all\": {}}\n}\n\nGET /y";
        let cmds = parse(s).unwrap();
        assert_eq!(cmds.len(), 2);
        let body: serde_json::Value =
            serde_json::from_str(req(&cmds[0]).body.as_deref().unwrap()).unwrap();
        assert_eq!(body["size"], 1);
    }

    #[test]
    fn ndjson_body() {
        let s = "POST /_bulk\n{\"index\":{\"_index\":\"t\"}}\n{\"a\":1}\n{\"index\":{\"_index\":\"t\"}}\n{\"a\":2}\n";
        let cmds = parse(s).unwrap();
        assert_eq!(cmds.len(), 1);
        let r = req(&cmds[0]);
        assert!(r.is_ndjson());
        assert_eq!(r.body.as_deref().unwrap().lines().count(), 4);
        let one = parse("GET /x/_search\n{\"size\":1}").unwrap();
        assert!(!req(&one[0]).is_ndjson());
    }

    #[test]
    fn comments_and_braces_in_strings() {
        let s = "// top\nGET /x/_search\n# inside\n{\"query\": {\"match\": {\"t\": \"a { b\"}}}\n";
        let cmds = parse(s).unwrap();
        assert_eq!(cmds.len(), 1);
        let body: serde_json::Value =
            serde_json::from_str(req(&cmds[0]).body.as_deref().unwrap()).unwrap();
        assert_eq!(body["query"]["match"]["t"], "a { b");
    }

    #[test]
    fn sql_statements() {
        let s = "SELECT a\nFROM t\nWHERE b = 1;\nshow tables\n\nGET /\ndescribe t";
        let cmds = parse(s).unwrap();
        assert_eq!(
            cmds,
            vec![
                Command::Sql("SELECT a\nFROM t\nWHERE b = 1".into()),
                Command::Sql("show tables".into()),
                Command::Http(Request {
                    method: "GET".into(),
                    path: "/".into(),
                    body: None
                }),
                Command::Sql("describe t".into()),
            ]
        );
    }

    #[test]
    fn query_helpers() {
        let r = Request {
            method: "GET".into(),
            path: "/a/_cat/indices?v&format=json".into(),
            body: None,
        };
        assert_eq!(r.path_only(), "/a/_cat/indices");
        assert_eq!(r.query_param("format"), Some("json"));
        assert_eq!(r.query_param("v"), Some(""));
        assert_eq!(r.segments(), vec!["a", "_cat", "indices"]);
    }

    #[test]
    fn located_offsets_and_lines() {
        let s = "# c\r\nGET /a\r\n\n  PUT /b\n{\n  \"x\": 1\n}\nSELECT 1;\nDELETE /c";
        let v = parse_located(s).unwrap();
        let at: Vec<(usize, u32)> = v.iter().map(|l| (l.offset, l.line)).collect();
        assert_eq!(at.len(), 4);
        assert_eq!(&s[at[0].0..at[0].0 + 6], "GET /a");
        assert_eq!(at[0].1, 2);
        assert_eq!(&s[at[1].0..at[1].0 + 6], "PUT /b");
        assert_eq!(at[1].1, 4);
        assert_eq!(&s[at[2].0..at[2].0 + 6], "SELECT");
        assert_eq!(at[2].1, 8);
        assert_eq!(at[3].1, 9);
        let e = parse_located("GET /a\n\nhola").unwrap_err();
        assert_eq!((e.offset, e.line), (8, 3));
    }

    #[test]
    fn garbage_is_an_error() {
        assert!(parse("hello world").unwrap_err().contains("Línea 1"));
        assert!(parse("GET").is_err());
    }

    /// `(start, end, line)` and text of each unit, checking that the units
    /// are where `execute` numbers its commands and that each unit's text
    /// alone parses to that one command (what "run the statement at the
    /// cursor" sends).
    fn units(s: &str) -> Vec<(usize, usize, u32, String)> {
        let units = split(s);
        let located = parse_located(s).unwrap();
        assert_eq!(units.len(), located.len(), "{units:?}");
        for (u, l) in units.iter().zip(&located) {
            assert_eq!((u.start, u.end, u.line), (l.offset, l.end, l.line));
            assert_eq!(u.error, None);
            assert_eq!(&s[u.start..u.end], u.text);
            assert_eq!(parse(&u.text).unwrap(), vec![l.command.clone()]);
        }
        units
            .into_iter()
            .map(|u| (u.start, u.end, u.line, u.text))
            .collect()
    }

    #[test]
    fn split_two_requests() {
        let u =
            units("GET /_cat/indices\nGET /idx/_search\n{\n  \"query\": {\"match_all\": {}}\n}");
        assert_eq!(u.len(), 2);
        assert_eq!(u[0], (0, 17, 1, "GET /_cat/indices".into()));
        assert_eq!(u[1].2, 2);
        assert_eq!(
            u[1].3,
            "GET /idx/_search\n{\n  \"query\": {\"match_all\": {}}\n}"
        );
    }

    #[test]
    fn split_comments_blank_lines_and_separators_in_strings() {
        let s = "# c\nGET _cluster/health\n\n// next\nPOST idx/_count\n{\"query\": {\"term\": {\"x\": \"x;y\\n}\\\" GET /z\"}}}\n# after\n\n";
        let u = units(s);
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].2, u[0].3.as_str()), (2, "GET _cluster/health"));
        assert_eq!(u[1].2, 5);
        assert!(u[1].3.starts_with("POST idx/_count\n{"));
        assert!(u[1].3.ends_with("}}}"), "{:?}", u[1].3);
    }

    #[test]
    fn split_three_with_sql_ndjson_and_crlf() {
        let s = "SELECT a\r\nFROM t;\r\nPOST /_bulk\r\n{\"index\":{}}\r\n{\"a\":1}\r\n\r\nshow tables\r\n";
        let u = units(s);
        assert_eq!(u.len(), 3);
        assert_eq!((u[0].2, u[0].3.as_str()), (1, "SELECT a\r\nFROM t"));
        assert_eq!(
            (u[1].2, u[1].3.as_str()),
            (3, "POST /_bulk\r\n{\"index\":{}}\r\n{\"a\":1}")
        );
        assert_eq!((u[2].2, u[2].3.as_str()), (7, "show tables"));
        // `;` statements one after another, and a request right after SQL.
        let u = units("SELECT 1;\nSELECT 2;\nGET /a");
        assert_eq!(
            u.iter().map(|u| u.3.as_str()).collect::<Vec<_>>(),
            ["SELECT 1", "SELECT 2", "GET /a"]
        );
        // A blank line inside an open body stays in it.
        let u = units("  PUT /b\n{\n\n  \"x\": 1\n}\n\nDELETE /b");
        assert_eq!(u.len(), 2);
        assert_eq!((u[0].0, u[0].2), (2, 1));
        assert_eq!(u[0].3, "PUT /b\n{\n\n  \"x\": 1\n}");
    }

    #[test]
    fn split_cuts_around_lines_that_are_neither() {
        let s = "GET /a\n\nhola\n  mundo\n\nGET\n{}\nPOST /b";
        let u = split(s);
        let got: Vec<_> = u.iter().map(|u| (u.text.as_str(), u.line)).collect();
        assert_eq!(
            got,
            [
                ("GET /a", 1),
                ("hola\n  mundo", 3),
                ("GET\n{}", 6),
                ("POST /b", 8),
            ]
        );
        // No unit carries an error (the editor keeps every lens while a
        // request is being typed): running one reports it at its place.
        assert!(u.iter().all(|u| u.error.is_none()));
        let e = parse_located(s).unwrap_err();
        assert_eq!((e.offset, e.line), (u[1].start, 3));
        assert_eq!(parse_located(&u[1].text).unwrap_err().offset, 0);
        assert!(parse(&u[2].text).is_err());
        for typing in ["G", "GE", "GET"] {
            let s = format!("GET /a\n\n{typing}\n\nPOST /b");
            let u = split(&s);
            assert_eq!(u.len(), 3, "{typing}");
            assert!(u.iter().all(|u| u.error.is_none()), "{typing}");
            assert!(parse(&s).is_err(), "{typing}");
        }
        assert!(split("").is_empty());
        assert!(split("# solo\n\n// comentarios\n").is_empty());
    }
}
