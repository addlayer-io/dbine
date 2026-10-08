//! "Optimizar consulta" (docs/optimizar-consulta.md): candidates that may
//! run faster than a query, from the rules (rewrites known to be
//! equivalent), from the AI and from the user, all verified the same way by
//! "Comparar"; and index suggestions from the execution plan. Nothing here
//! changes the database: scripts and rewrites only reach the editor.

pub mod ai;
pub mod catalog;
pub mod compare;
pub mod hints;
pub mod lex;
#[cfg(test)]
mod live_tests;
pub mod mongo;
pub mod parse;
pub mod rules;

use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    Rule,
    Ai,
    User,
}

/// A version of the query to compare with the original.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Candidate {
    pub id: String,
    pub source: Source,
    /// The rule that wrote it (`optimizer:rules.<rule>` in the UI).
    #[serde(default)]
    pub rule: Option<String>,
    /// Values for the rule's text (the column, the tables…).
    #[serde(default)]
    pub params: BTreeMap<String, String>,
    /// The AI's own title and explanation.
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub explanation: Option<String>,
    pub sql: String,
    /// Not proven equivalent in every case (the AI's, MongoDB's `$where`):
    /// the UI asks to compare it before using it.
    #[serde(default)]
    pub verify: bool,
}

/// Something worth knowing that isn't a rewrite (`optimizer:notes.<rule>`).
#[derive(Debug, Clone, Serialize, PartialEq)]
pub struct Note {
    pub rule: String,
    pub params: BTreeMap<String, String>,
    /// A version to open and adjust by hand (the columns instead of `*`).
    pub sql: Option<String>,
}

/// The rule candidates and notes of a query in the driver's language.
pub fn rewrites(src: &str, language: dbine_driver::Language, dialect: &str, tables: Option<&[dbine_driver::TableSchema]>) -> (Vec<Candidate>, Vec<Note>) {
    match language {
        dbine_driver::Language::Sql => rules::analyze(src, dialect, tables),
        dbine_driver::Language::Json => (mongo::where_to_operators(src), Vec::new()),
        _ => (Vec::new(), Vec::new()),
    }
}

/// The original orders its rows (a top-level ORDER BY, a MongoDB sort):
/// the comparison then counts their order.
pub fn ordered(src: &str, language: dbine_driver::Language, dialect: &str) -> bool {
    match language {
        dbine_driver::Language::Sql => {
            let sql = parse::Sql::parse(src, lex::Flavor::for_dialect(dialect));
            sql.blocks.iter().any(|b| b.depth == 0 && b.order_by.is_some())
        }
        dbine_driver::Language::Cypher => src.to_lowercase().contains("order by"),
        _ => src.contains(".sort(") || src.contains("$sort") || src.contains("\"sort\""),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::Language;

    #[test]
    fn order_counts_only_at_the_top() {
        assert!(ordered("SELECT a FROM t ORDER BY a", Language::Sql, "postgres"));
        assert!(!ordered("SELECT * FROM (SELECT a FROM t ORDER BY a LIMIT 3) x", Language::Sql, "postgres"));
        assert!(ordered("db.c.find({}).sort({a: 1})", Language::Json, ""));
        assert!(!ordered("db.c.find({})", Language::Json, ""));
    }

    #[test]
    fn rewrites_per_language() {
        assert!(!rewrites("SELECT * FROM c WHERE (SELECT COUNT(*) FROM o) > 0", Language::Sql, "postgres", None).0.is_empty());
        assert!(!rewrites("db.c.find({$where: 'this.a > 1'})", Language::Json, "", None).0.is_empty());
        assert!(rewrites("GET k", Language::Redis, "", None).0.is_empty());
    }
}
