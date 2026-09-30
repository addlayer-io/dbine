//! Splits a native type spelling into its parts, so dialects classify by
//! name and read arguments without each writing its own tokenizer:
//!
//! - `varchar(50)` → name `varchar`, args `["50"]`
//! - `numeric(10, 2)` → name `numeric`, args `["10", "2"]`
//! - `int unsigned zerofill` → name `int`, flags `unsigned`, `zerofill`
//! - `timestamp(3) with time zone` → name `timestamp`, args `["3"]`, `with_tz`
//! - `integer[]` / `_int4` → array of `integer` / `int4`
//! - `Nullable(LowCardinality(String))` → name `string`, wrappers noted
//! - `enum('a','b')` → args `["a", "b"]` (quotes removed)
//! - `character varying(20)`, `double precision` → multi-word names kept

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct TypeSpec {
    /// Base name, lower case, single spaces (`character varying`).
    pub name: String,
    /// Arguments inside the first parentheses, unquoted.
    pub args: Vec<String>,
    /// Arguments that were quoted strings (enum/set values).
    pub quoted_args: bool,
    pub unsigned: bool,
    pub with_tz: bool,
    /// `WITH LOCAL TIME ZONE` (Oracle): normalized to the session zone.
    pub local_tz: bool,
    /// Dimensions of an array suffix (`[]`, `[][]`) or `_` prefix.
    pub array_dims: u8,
    /// ClickHouse-style wrappers that were peeled off, outermost first
    /// (`nullable`, `lowcardinality`).
    pub wrappers: Vec<String>,
    /// Anything after the name/args that wasn't recognized
    /// (`character set utf8mb4`, `collate …`, `zerofill`, `identity`).
    pub rest: Vec<String>,
    /// The spelling as given.
    pub raw: String,
}

impl TypeSpec {
    /// Numeric argument `i`, if present and numeric.
    pub fn arg_u32(&self, i: usize) -> Option<u32> {
        self.args.get(i).and_then(|a| a.trim().parse().ok())
    }

    /// Whether `word` appears among the trailing modifiers.
    pub fn has(&self, word: &str) -> bool {
        self.rest.iter().any(|r| r == word)
    }

    /// The `max` argument (`varchar(max)`, `varbinary(max)`).
    pub fn is_max(&self) -> bool {
        self.args.first().is_some_and(|a| a.eq_ignore_ascii_case("max"))
    }
}

/// Wrappers that only change nullability or storage, never the value.
const WRAPPERS: &[&str] = &["nullable", "lowcardinality", "low_cardinality", "simpleaggregatefunction"];

pub fn parse(native: &str) -> TypeSpec {
    let raw = native.trim().to_string();
    let mut spec = TypeSpec { raw: raw.clone(), ..Default::default() };
    let mut s = raw.clone();

    // Peel `Wrapper(inner)`.
    loop {
        let lower = s.to_ascii_lowercase();
        let Some(open) = lower.find('(') else { break };
        let head = lower[..open].trim();
        if WRAPPERS.contains(&head) && s.trim_end().ends_with(')') {
            spec.wrappers.push(head.to_string());
            let inner = s[open + 1..s.trim_end().len() - 1].trim().to_string();
            // SimpleAggregateFunction(fn, Type): keep the type.
            s = if head == "simpleaggregatefunction" {
                inner.split_once(',').map_or(inner.clone(), |(_, t)| t.trim().to_string())
            } else {
                inner
            };
            continue;
        }
        break;
    }

    // Array suffixes.
    let mut body = s.trim().to_string();
    while body.ends_with("[]") {
        spec.array_dims += 1;
        body.truncate(body.len() - 2);
        body = body.trim_end().to_string();
    }
    // `ARRAY` keyword suffix (PostgreSQL `integer ARRAY`).
    if body.to_ascii_lowercase().ends_with(" array") {
        spec.array_dims += 1;
        body.truncate(body.len() - 6);
    }

    // name (args) rest…
    let (head, args, tail) = match body.find('(') {
        Some(open) => {
            let close = matching_paren(&body, open).unwrap_or(body.len());
            let inside = &body[open + 1..close.min(body.len())];
            let tail = if close < body.len() { body[close + 1..].to_string() } else { String::new() };
            (body[..open].to_string(), Some(inside.to_string()), tail)
        }
        None => (body.clone(), None, String::new()),
    };

    // The name may be multi-word; modifiers follow it. A modifier word
    // after the args (`timestamp(3) with time zone`) goes to the tail.
    let head_words: Vec<String> = head.split_whitespace().map(|w| w.to_ascii_lowercase()).collect();
    let mut name_words = Vec::new();
    let mut mods: Vec<String> = Vec::new();
    for w in head_words {
        if mods.is_empty() && !is_modifier(&w) {
            name_words.push(w);
        } else {
            mods.push(w);
        }
    }
    mods.extend(tail.split_whitespace().map(|w| w.to_ascii_lowercase()));

    let mut name = name_words.join(" ");
    if let Some(stripped) = name.strip_prefix('_') {
        // PostgreSQL internal array names: `_int4`, `_text`.
        if !stripped.is_empty() && spec.array_dims == 0 {
            spec.array_dims = 1;
            name = stripped.to_string();
        }
    }
    spec.name = name;

    if let Some(a) = args {
        let parts = split_args(&a);
        spec.quoted_args = parts.iter().any(|p| p.starts_with('\''));
        spec.args = parts.into_iter().map(|p| unquote(&p)).collect();
    }

    // Modifiers.
    let joined = mods.join(" ");
    if joined.contains("with local time zone") {
        spec.local_tz = true;
        spec.with_tz = true;
    } else if joined.contains("with time zone") {
        spec.with_tz = true;
    }
    let mut i = 0;
    while i < mods.len() {
        let w = mods[i].as_str();
        match w {
            "unsigned" => spec.unsigned = true,
            "signed" => {}
            "with" | "without" | "time" | "zone" | "local" => {}
            _ => spec.rest.push(w.to_string()),
        }
        i += 1;
    }
    spec
}

/// A word that ends the type name: what follows modifies it.
fn is_modifier(w: &str) -> bool {
    matches!(w, "unsigned" | "signed" | "zerofill" | "with" | "without" | "collate" | "not" | "null" | "identity" | "generated" | "default")
}

fn matching_paren(s: &str, open: usize) -> Option<usize> {
    let mut depth = 0i32;
    let mut in_quote = false;
    for (i, c) in s.char_indices().skip_while(|(i, _)| *i < open) {
        match c {
            '\'' => in_quote = !in_quote,
            '(' if !in_quote => depth += 1,
            ')' if !in_quote => {
                depth -= 1;
                if depth == 0 {
                    return Some(i);
                }
            }
            _ => {}
        }
    }
    None
}

/// Split on top-level commas, keeping quoted strings and nested parens.
pub fn split_args(s: &str) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = String::new();
    let mut depth = 0i32;
    let mut in_quote = false;
    let mut chars = s.chars().peekable();
    while let Some(c) = chars.next() {
        match c {
            '\'' => {
                // '' inside a quoted string is an escaped quote.
                if in_quote && chars.peek() == Some(&'\'') {
                    cur.push('\'');
                    cur.push(chars.next().unwrap());
                    continue;
                }
                in_quote = !in_quote;
                cur.push(c);
            }
            '(' if !in_quote => {
                depth += 1;
                cur.push(c);
            }
            ')' if !in_quote => {
                depth -= 1;
                cur.push(c);
            }
            ',' if !in_quote && depth == 0 => {
                out.push(cur.trim().to_string());
                cur.clear();
            }
            _ => cur.push(c),
        }
    }
    if !cur.trim().is_empty() {
        out.push(cur.trim().to_string());
    }
    out
}

fn unquote(p: &str) -> String {
    let p = p.trim();
    // ClickHouse enum values: `'a' = 1`.
    let p = if p.starts_with('\'') { p.rsplit_once('=').filter(|(l, _)| l.trim_end().ends_with('\'')).map_or(p, |(l, _)| l.trim()) } else { p };
    if p.len() >= 2 && p.starts_with('\'') && p.ends_with('\'') {
        p[1..p.len() - 1].replace("''", "'")
    } else {
        p.to_string()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn simple() {
        let t = parse("VARCHAR(50)");
        assert_eq!(t.name, "varchar");
        assert_eq!(t.arg_u32(0), Some(50));
        let t = parse("numeric(10, 2)");
        assert_eq!((t.arg_u32(0), t.arg_u32(1)), (Some(10), Some(2)));
    }

    #[test]
    fn multi_word_and_modifiers() {
        let t = parse("character varying(20)");
        assert_eq!(t.name, "character varying");
        assert_eq!(t.arg_u32(0), Some(20));
        let t = parse("double precision");
        assert_eq!(t.name, "double precision");
        let t = parse("int(11) unsigned zerofill");
        assert_eq!(t.name, "int");
        assert!(t.unsigned);
        assert!(t.has("zerofill"));
        let t = parse("bigint unsigned");
        assert_eq!(t.name, "bigint");
        assert!(t.unsigned);
    }

    #[test]
    fn time_zones() {
        let t = parse("timestamp(3) with time zone");
        assert_eq!(t.name, "timestamp");
        assert_eq!(t.arg_u32(0), Some(3));
        assert!(t.with_tz);
        let t = parse("TIMESTAMP(6) WITH LOCAL TIME ZONE");
        assert!(t.with_tz && t.local_tz);
        let t = parse("timestamp without time zone");
        assert_eq!(t.name, "timestamp");
        assert!(!t.with_tz);
        let t = parse("time with time zone");
        assert_eq!(t.name, "time");
        assert!(t.with_tz);
    }

    #[test]
    fn arrays() {
        let t = parse("integer[]");
        assert_eq!((t.name.as_str(), t.array_dims), ("integer", 1));
        let t = parse("text[][]");
        assert_eq!(t.array_dims, 2);
        let t = parse("_int4");
        assert_eq!((t.name.as_str(), t.array_dims), ("int4", 1));
        let t = parse("varchar(10) ARRAY");
        assert_eq!((t.name.as_str(), t.array_dims, t.arg_u32(0)), ("varchar", 1, Some(10)));
    }

    #[test]
    fn wrappers_and_enums() {
        let t = parse("Nullable(LowCardinality(String))");
        assert_eq!(t.name, "string");
        assert_eq!(t.wrappers, vec!["nullable", "lowcardinality"]);
        let t = parse("enum('a','it''s','c')");
        assert_eq!(t.args, vec!["a", "it's", "c"]);
        assert!(t.quoted_args);
        let t = parse("Enum8('x' = 1, 'y' = 2)");
        assert_eq!(t.name, "enum8");
        assert_eq!(t.args, vec!["x", "y"]);
        let t = parse("Map(String, Array(UInt8))");
        assert_eq!(t.args, vec!["String", "Array(UInt8)"]);
    }

    #[test]
    fn max_and_charset() {
        let t = parse("nvarchar(max)");
        assert!(t.is_max());
        let t = parse("varchar(255) character set utf8mb4 collate utf8mb4_bin");
        assert_eq!(t.name, "varchar");
        assert!(t.has("utf8mb4"));
    }
}
