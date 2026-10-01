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
        self.path_only().split('/').filter(|s| !s.is_empty()).collect()
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
    line.trim_start().chars().take_while(|c| c.is_ascii_alphabetic()).collect::<String>().to_ascii_uppercase()
}

fn is_comment(line: &str) -> bool {
    let t = line.trim_start();
    t.starts_with('#') || t.starts_with("//")
}

fn method_of(line: &str) -> Option<String> {
    let w = first_word(line);
    let rest = &line.trim_start()[w.len()..];
    (METHODS.contains(&w.as_str()) && (rest.is_empty() || rest.starts_with(char::is_whitespace))).then_some(w)
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
}

/// A command with where it starts in the script.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Located {
    pub command: Command,
    /// Byte offset of its first line's first non-blank character.
    pub offset: usize,
    /// 1-based line of that character.
    pub line: u32,
}

/// A line that's neither a request nor SQL, and where it is.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ParseError {
    pub message: String,
    pub offset: usize,
    pub line: u32,
}

fn finish(cur: Cur, at: (usize, u32), out: &mut Vec<Located>) {
    let command = match cur {
        Cur::None => return,
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
            Command::Sql(s.to_string())
        }
    };
    out.push(Located { command, offset: at.0, line: at.1 });
}

/// Split a console script into requests and SQL statements.
/// `Err` names the first line that's neither.
pub fn parse(text: &str) -> Result<Vec<Command>, String> {
    parse_located(text).map(|v| v.into_iter().map(|l| l.command).collect()).map_err(|e| e.message)
}

/// [`parse`], keeping where each command starts in `text`.
pub fn parse_located(text: &str) -> Result<Vec<Located>, ParseError> {
    let mut out = Vec::new();
    let mut cur = Cur::None;
    let mut at = (0usize, 1u32);
    let mut pos = 0usize;
    for (n, raw) in text.split_inclusive('\n').enumerate() {
        let start = pos;
        pos += raw.len();
        let line = raw.strip_suffix('\n').unwrap_or(raw);
        let line = line.strip_suffix('\r').unwrap_or(line);
        let here = (start + (line.len() - line.trim_start().len()), n as u32 + 1);
        if is_comment(line) {
            continue;
        }
        let blank = line.trim().is_empty();
        // Inside an unbalanced JSON body anything but a request line is content.
        if let Cur::Req(_, _, body) = &mut cur {
            if depth(&body.join("\n")) > 0 && method_of(line).is_none() {
                body.push(line.to_string());
                continue;
            }
        }
        if blank {
            finish(std::mem::replace(&mut cur, Cur::None), at, &mut out);
            continue;
        }
        if let Some(m) = method_of(line) {
            finish(std::mem::replace(&mut cur, Cur::None), at, &mut out);
            let path = line.trim_start()[m.len()..].trim().to_string();
            if path.is_empty() {
                return Err(ParseError { message: format!("Línea {}: falta la ruta después de {m}.", n + 1), offset: here.0, line: here.1 });
            }
            cur = Cur::Req(m, path, Vec::new());
            at = here;
            continue;
        }
        if is_sql_start(line) && !matches!(cur, Cur::Sql(_)) {
            finish(std::mem::replace(&mut cur, Cur::None), at, &mut out);
            cur = Cur::Sql(Vec::new());
            at = here;
        }
        match &mut cur {
            Cur::Req(_, _, body) => body.push(line.to_string()),
            Cur::Sql(lines) => {
                lines.push(line.to_string());
                if line.trim_end().ends_with(';') {
                    finish(std::mem::replace(&mut cur, Cur::None), at, &mut out);
                }
            }
            Cur::None => {
                return Err(ParseError {
                    message: format!(
                        "Línea {}: se esperaba una petición (GET /ruta, POST /ruta…) o una sentencia SQL: {}",
                        n + 1,
                        line.trim()
                    ),
                    offset: here.0,
                    line: here.1,
                })
            }
        }
    }
    finish(cur, at, &mut out);
    Ok(out)
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
        let s = "GET /x/_search\n{\n  \"size\": 1,\n\n  \"query\": {\"match_all\": {}}\n}\n\nGET /y";
        let cmds = parse(s).unwrap();
        assert_eq!(cmds.len(), 2);
        let body: serde_json::Value = serde_json::from_str(req(&cmds[0]).body.as_deref().unwrap()).unwrap();
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
        let body: serde_json::Value = serde_json::from_str(req(&cmds[0]).body.as_deref().unwrap()).unwrap();
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
                Command::Http(Request { method: "GET".into(), path: "/".into(), body: None }),
                Command::Sql("describe t".into()),
            ]
        );
    }

    #[test]
    fn query_helpers() {
        let r = Request { method: "GET".into(), path: "/a/_cat/indices?v&format=json".into(), body: None };
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
}
