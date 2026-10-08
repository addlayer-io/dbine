//! "Buscar en la base" ([`crate::Session::search_code`]): the text of views,
//! routines, triggers and the like.
//!
//! The app scans by itself (each object's [`crate::Session::definition`],
//! with progress and cancel) unless the driver answers in one catalog
//! query. Matching is plain text, line by line, so both give the same hits.

use serde::{Deserialize, Serialize};

/// What to look for.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeSearch {
    pub text: String,
    #[serde(default)]
    pub case_sensitive: bool,
    /// Only where it's a whole word (not inside a longer identifier).
    #[serde(default)]
    pub whole_word: bool,
    /// Object kinds to search; empty: every kind with a definition.
    #[serde(default)]
    pub kinds: Vec<String>,
    /// Stop after this many hits (0: no cap).
    #[serde(default)]
    pub max_hits: usize,
}

/// One line of an object's source where the text shows up.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeHit {
    pub kind: String,
    pub schema: Option<String>,
    pub name: String,
    /// The table a trigger or index belongs to.
    #[serde(default)]
    pub parent: Option<String>,
    /// 1-based.
    pub line: u32,
    /// That line, trimmed to [`MAX_LINE`] characters.
    pub text: String,
}

/// A driver's answer from its catalog.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CodeSearchReport {
    pub hits: Vec<CodeHit>,
    /// Objects whose source was read.
    pub scanned: usize,
    /// Objects whose source couldn't be read (no permission…).
    #[serde(default)]
    pub unreadable: Vec<String>,
    /// The cap cut it short.
    #[serde(default)]
    pub truncated: bool,
}

/// Characters kept of a matching line.
pub const MAX_LINE: usize = 300;

fn is_word(c: char) -> bool {
    c.is_alphanumeric() || c == '_' || c == '$' || c == '#'
}

/// Whether `line` holds `q`'s text (as `q` asks: case, whole word).
pub fn line_matches(line: &str, q: &CodeSearch) -> bool {
    if q.text.is_empty() {
        return false;
    }
    let (hay, needle) = if q.case_sensitive { (line.to_string(), q.text.clone()) } else { (line.to_lowercase(), q.text.to_lowercase()) };
    let mut from = 0;
    while let Some(i) = hay[from..].find(&needle) {
        let at = from + i;
        let end = at + needle.len();
        if !q.whole_word {
            return true;
        }
        let before = hay[..at].chars().next_back();
        let after = hay[end..].chars().next();
        if !before.is_some_and(is_word) && !after.is_some_and(is_word) {
            return true;
        }
        from = at + needle.chars().next().map_or(1, char::len_utf8);
    }
    false
}

/// The hits in one object's `source`.
pub fn hits_in(kind: &str, schema: Option<&str>, name: &str, parent: Option<&str>, source: &str, q: &CodeSearch) -> Vec<CodeHit> {
    source
        .lines()
        .enumerate()
        .filter(|(_, l)| line_matches(l, q))
        .map(|(i, l)| CodeHit {
            kind: kind.to_string(),
            schema: schema.map(str::to_string),
            name: name.to_string(),
            parent: parent.map(str::to_string),
            line: i as u32 + 1,
            text: l.trim().chars().take(MAX_LINE).collect(),
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn q(text: &str, case: bool, word: bool) -> CodeSearch {
        CodeSearch { text: text.into(), case_sensitive: case, whole_word: word, ..Default::default() }
    }

    #[test]
    fn plain_case_and_whole_word() {
        assert!(line_matches("SELECT * FROM Clientes c", &q("clientes", false, false)));
        assert!(!line_matches("SELECT * FROM Clientes c", &q("clientes", true, false)));
        assert!(!line_matches("FROM ClientesHistorico", &q("clientes", false, true)));
        assert!(line_matches("FROM dbo.Clientes;", &q("clientes", false, true)));
        assert!(line_matches("x ClientesH, Clientes", &q("clientes", false, true)), "a later whole-word match counts");
        assert!(!line_matches("anything", &q("", false, false)));
    }

    #[test]
    fn hits_carry_their_line() {
        let h = hits_in("view", Some("dbo"), "v", None, "CREATE VIEW v AS\n  SELECT id\n  FROM ventas", &q("ventas", false, true));
        assert_eq!(h.len(), 1);
        assert_eq!((h[0].line, h[0].text.as_str()), (3, "FROM ventas"));
    }
}
