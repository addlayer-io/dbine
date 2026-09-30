//! Server-side key search, for engines whose databases hold keys instead of
//! a handful of tables (Redis, etcd). A database can have millions of keys:
//! the explorer never lists them all, it asks for them a page at a time, by
//! pattern, and the driver walks the keyspace on the server ([`Session::scan_keys`]).
//!
//! [`Session::scan_keys`]: crate::Session::scan_keys

use serde::{Deserialize, Serialize};

/// How a search pattern is read.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum KeySyntax {
    /// `*`, `?` and `[…]` wildcards (Redis `MATCH`). Text without wildcards
    /// matches keys that contain it.
    Glob,
    /// The keys that start with the text (etcd ranges).
    Prefix,
}

/// What an engine's key search offers ([`crate::Driver::key_search`]).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeySearch {
    pub syntax: KeySyntax,
    /// Separator the explorer nests keys by (`user:1:cart` → `user` › `user:1`).
    #[serde(deserialize_with = "crate::serde_static::str")]
    pub separator: &'static str,
    /// Types it filters by on the server, as [`KeyEntry::key_type`] names
    /// them; empty when it has no types.
    #[serde(deserialize_with = "crate::serde_static::strs")]
    pub types: Vec<&'static str>,
    /// Whether matching tells upper from lower case (shown in the search box).
    pub case_sensitive: bool,
}

/// One page of a search.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyScan {
    /// Empty = every key.
    #[serde(default)]
    pub pattern: String,
    /// Only keys of this type (one of [`KeySearch::types`]).
    #[serde(default)]
    pub key_type: Option<String>,
    /// Where the previous page stopped; `None` starts the search.
    #[serde(default)]
    pub cursor: Option<String>,
    /// Keys wanted in this page. The driver may return fewer (a selective
    /// pattern over a big keyspace) and hand back a cursor to go on.
    pub count: u32,
}

/// A page of keys.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct KeyPage {
    pub keys: Vec<KeyEntry>,
    /// Where to go on from; `None` when the search is over.
    pub cursor: Option<String>,
    /// Keys in the whole database (or in the searched range), when the
    /// engine tells; only in the first page.
    pub total: Option<u64>,
    /// Keys the server looked at for this page, matching or not (to show
    /// how far a selective search got).
    pub scanned: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct KeyEntry {
    pub name: String,
    pub key_type: Option<String>,
    /// Milliseconds to expiry; `None` when the key doesn't expire (or the
    /// engine doesn't say).
    pub ttl_ms: Option<i64>,
}

/// Whether a glob pattern has wildcards (`*`, `?`, `[`), unescaped.
pub fn has_wildcards(pattern: &str) -> bool {
    let mut escaped = false;
    for c in pattern.chars() {
        match c {
            _ if escaped => escaped = false,
            '\\' => escaped = true,
            '*' | '?' | '[' => return true,
            _ => {}
        }
    }
    false
}

/// Text as a literal inside a glob pattern.
pub fn glob_escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if matches!(c, '*' | '?' | '[' | ']' | '\\') {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn wildcards_and_escaping() {
        assert!(has_wildcards("user:*"));
        assert!(has_wildcards("a?c"));
        assert!(has_wildcards("[ab]x"));
        assert!(!has_wildcards("user:1"));
        assert!(!has_wildcards(r"price\*2"));
        assert_eq!(glob_escape("a*b[1]?"), r"a\*b\[1\]\?");
        assert!(!has_wildcards(&glob_escape("x*y")));
    }
}
