//! The explorer's pages (`Session::list_objects_page`) out of the sources
//! `list_objects` reads, only the kinds asked for, put together with
//! [`crate::object_pages`]. Nothing has a schema.
//!
//! - **Neo4j:** every kind is a Cypher query the server filters, sorts and
//!   pages (`db.labels()`, `db.relationshipTypes()`, `SHOW INDEXES`,
//!   `SHOW CONSTRAINTS`, `SHOW PROCEDURES` with `WHERE`,
//!   `ORDER BY toLower(n), n`, `SKIP`/`LIMIT`), the user's text bound.
//!   Its `toLower` is Java's, the same mapping as Rust's `to_lowercase`,
//!   but it compares strings by UTF-16 units, which order characters from
//!   U+E000 up unlike Rust: names with them are read apart, whole, and
//!   sorted here.
//! - **Memgraph:** `SHOW NODE_LABELS INFO`, `SHOW EDGE_TYPES INFO`,
//!   `SHOW INDEX INFO`, `SHOW CONSTRAINT INFO` and `SHOW TRIGGERS` take no
//!   `WHERE` or `LIMIT`: a folder of those reads its own list only, sorted
//!   here. Procedures (`mg.procedures()`) are filtered, sorted and paged
//!   by the server; its `toLower` folds ASCII only, so names outside
//!   printable ASCII are read apart, whole.
//! - **Neptune:** its labels come from the graph summary, one document
//!   with every label and type and no filter: the app pages
//!   `list_objects`.

use crate::object_pages;
use crate::packstream::Value as Bolt;
use crate::{Flavor, CONSTRAINT, LABEL, RELATIONSHIP};
use dbine_driver::{kinds, DbObject, ObjectQuery};
use serde_json::Value;

/// Where a kind's names come from.
#[derive(Debug, Clone, Copy, PartialEq)]
pub(crate) enum Origin {
    /// A query the server filters, sorts and pages: `{w}` takes the
    /// conditions on `n` (the name), `{tail}` the order and the page; rows
    /// bring `n` and, for indexes and constraints, `t` (their labels or
    /// types).
    Cypher(&'static str),
    /// A list read whole (with `list_objects`' own calls) and sorted here.
    Whole,
}

#[derive(Debug, Clone, Copy)]
pub(crate) struct Source {
    pub kind: &'static str,
    pub origin: Origin,
}

/// The flavor's sources, in `list_objects`' order of kinds (`None`:
/// Neptune, left to the app).
pub(crate) fn sources(f: Flavor) -> Option<Vec<Source>> {
    let s = |kind, origin| Source { kind, origin };
    match f {
        Flavor::Neo4j => Some(vec![
            s(
                LABEL,
                Origin::Cypher("CALL db.labels() YIELD label AS n WHERE {w} RETURN n{tail}"),
            ),
            s(
                RELATIONSHIP,
                Origin::Cypher(
                    "CALL db.relationshipTypes() YIELD relationshipType AS n WHERE {w} RETURN n{tail}",
                ),
            ),
            s(
                kinds::INDEX,
                Origin::Cypher(
                    "SHOW INDEXES YIELD name AS n, labelsOrTypes AS t, owningConstraint \
                     WHERE owningConstraint IS NULL AND {w} RETURN n, t{tail}",
                ),
            ),
            s(
                CONSTRAINT,
                Origin::Cypher(
                    "SHOW CONSTRAINTS YIELD name AS n, labelsOrTypes AS t WHERE {w} RETURN n, t{tail}",
                ),
            ),
            s(
                kinds::PROCEDURE,
                Origin::Cypher("SHOW PROCEDURES YIELD name AS n WHERE {w} RETURN n{tail}"),
            ),
        ]),
        Flavor::Memgraph => Some(vec![
            s(LABEL, Origin::Whole),
            s(RELATIONSHIP, Origin::Whole),
            s(kinds::INDEX, Origin::Whole),
            s(CONSTRAINT, Origin::Whole),
            s(
                kinds::PROCEDURE,
                Origin::Cypher("CALL mg.procedures() YIELD name AS n WHERE {w} RETURN n{tail}"),
            ),
            s(kinds::TRIGGER, Origin::Whole),
        ]),
        Flavor::Neptune => None,
    }
}

/// The names the server folds and sorts as the tree does, as a regex
/// over the whole name: below U+E000 for Neo4j, printable ASCII for
/// Memgraph.
fn plain_regex(f: Flavor) -> &'static str {
    match f {
        Flavor::Memgraph => "[ -~]*",
        _ => r"[^\x{E000}-\x{10FFFF}]*",
    }
}

/// Whether `s` holds only what [`plain_regex`] admits.
fn plain(f: Flavor, s: &str) -> bool {
    match f {
        Flavor::Memgraph => s.bytes().all(|b| (b' '..=b'~').contains(&b)),
        _ => s.chars().all(|c| (c as u32) < 0xE000),
    }
}

fn render(template: &str, w: &str, tail: &str) -> String {
    template.replace("{w}", w).replace("{tail}", tail)
}

/// The query of the names the server pages itself (`template`'s, see
/// [`Origin::Cypher`]) from `offset` (one more than `size`), with its
/// parameters; `None` when none of them can match.
pub(crate) fn plain_page(
    f: Flavor,
    template: &str,
    q: &ObjectQuery,
    offset: usize,
    size: usize,
) -> Option<(String, Bolt)> {
    let mut w = vec!["n =~ $plain"];
    let mut params = vec![("plain".to_string(), Bolt::from(plain_regex(f)))];
    if let Some(n) = &q.name {
        let n = n.to_lowercase();
        if !plain(f, &n) {
            return None;
        }
        w.push("toLower(n) = $name");
        params.push(("name".into(), Bolt::from(n)));
    }
    if let Some(p) = &q.pattern {
        let p = p.to_lowercase();
        if !plain(f, &p) {
            return None;
        }
        w.push("toLower(n) CONTAINS $pattern");
        params.push(("pattern".into(), Bolt::from(p)));
    }
    let as_int = |n: usize| Bolt::Int(i64::try_from(n).unwrap_or(i64::MAX));
    params.push(("skip".into(), as_int(offset)));
    params.push(("limit".into(), as_int(size.saturating_add(1))));
    Some((
        render(
            template,
            &w.join(" AND "),
            " ORDER BY toLower(n), n SKIP $skip LIMIT $limit",
        ),
        Bolt::Map(params),
    ))
}

/// The query of the other names (read whole, filtered and sorted here).
pub(crate) fn rest(f: Flavor, template: &str) -> (String, Bolt) {
    (
        render(template, "NOT n =~ $plain", ""),
        Bolt::Map(vec![("plain".into(), Bolt::from(plain_regex(f)))]),
    )
}

/// The objects of `kind` in a query's rows (`n`, and `t` for indexes and
/// constraints, which `list_objects` gives their labels or types as
/// parent).
pub(crate) fn objects(
    kind: &'static str,
    rows: Vec<serde_json::Map<String, Value>>,
) -> Vec<DbObject> {
    let owned = kind == kinds::INDEX || kind == CONSTRAINT;
    rows.into_iter()
        .filter_map(|r| {
            let name = r.get("n")?.as_str()?.to_string();
            let parent = owned.then(|| {
                r.get("t")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .collect::<Vec<_>>()
                            .join(",")
                    })
                    .unwrap_or_default()
            });
            Some(DbObject {
                kind: kind.into(),
                schema: None,
                name,
                parent,
            })
        })
        .filter(|o| !o.name.is_empty() || owned)
        .collect()
}

/// Whether `q` can keep anything at all (nothing here has a schema).
pub(crate) fn any(q: &ObjectQuery) -> bool {
    object_pages::schema_ok(q, None)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn param<'a>(p: &'a Bolt, k: &str) -> Option<&'a Bolt> {
        p.get(k)
    }

    #[test]
    fn a_folder_is_one_paged_query() {
        let src = sources(Flavor::Neo4j).unwrap();
        let Origin::Cypher(t) = src[2].origin else {
            panic!()
        };
        let q = ObjectQuery {
            kind: Some(kinds::INDEX.into()),
            ..Default::default()
        };
        let (cypher, params) = plain_page(Flavor::Neo4j, t, &q, 30, 10).unwrap();
        assert_eq!(
            cypher,
            "SHOW INDEXES YIELD name AS n, labelsOrTypes AS t, owningConstraint \
             WHERE owningConstraint IS NULL AND n =~ $plain RETURN n, t \
             ORDER BY toLower(n), n SKIP $skip LIMIT $limit"
        );
        assert_eq!(param(&params, "skip"), Some(&Bolt::Int(30)));
        assert_eq!(param(&params, "limit"), Some(&Bolt::Int(11)));
        let (cypher, _) = rest(Flavor::Neo4j, t);
        assert!(
            cypher.ends_with("AND NOT n =~ $plain RETURN n, t"),
            "{cypher}"
        );
    }

    #[test]
    fn user_text_is_bound_lowercased() {
        let t = "CALL db.labels() YIELD label AS n WHERE {w} RETURN n{tail}";
        let q = ObjectQuery {
            pattern: Some("A' OR 1=1".into()),
            name: Some("Ñu".into()),
            ..Default::default()
        };
        let (cypher, params) = plain_page(Flavor::Neo4j, t, &q, 0, 5).unwrap();
        assert!(!cypher.contains("1=1"), "{cypher}");
        assert!(cypher.contains("toLower(n) = $name AND toLower(n) CONTAINS $pattern"));
        assert_eq!(param(&params, "pattern"), Some(&Bolt::from("a' or 1=1")));
        assert_eq!(param(&params, "name"), Some(&Bolt::from("ñu")));
        // Memgraph folds ASCII only: such a name is only among the rest.
        assert!(plain_page(Flavor::Memgraph, t, &q, 0, 5).is_none());
        let q = ObjectQuery {
            pattern: Some("\u{FF21}".into()),
            ..Default::default()
        };
        assert!(plain_page(Flavor::Neo4j, t, &q, 0, 5).is_none());
    }

    #[test]
    fn plain_names() {
        assert!(plain(Flavor::Neo4j, "Ñandú ΑΣ"));
        assert!(!plain(Flavor::Neo4j, "\u{E000}") && !plain(Flavor::Neo4j, "😀"));
        assert!(plain(Flavor::Memgraph, "a.b ~"));
        assert!(!plain(Flavor::Memgraph, "ñ") && !plain(Flavor::Memgraph, "a\tb"));
    }

    #[test]
    fn rows_become_objects() {
        let rows = |v: Value| {
            v.as_array()
                .unwrap()
                .iter()
                .map(|r| r.as_object().unwrap().clone())
                .collect::<Vec<_>>()
        };
        let o = objects(
            kinds::INDEX,
            rows(json!([{"n": "i1", "t": ["A", "B"]}, {"n": "lookup", "t": null}])),
        );
        assert_eq!(o[0].parent.as_deref(), Some("A,B"));
        assert_eq!(o[1].parent.as_deref(), Some(""));
        let o = objects(LABEL, rows(json!([{"n": "Person"}])));
        assert_eq!(o[0].parent, None);
        assert_eq!(o[0].kind, LABEL);
    }

    #[test]
    fn neptune_is_left_to_the_app() {
        assert!(sources(Flavor::Neptune).is_none());
        let kinds = |f| -> Vec<&str> { sources(f).unwrap().iter().map(|s| s.kind).collect() };
        // `list_objects`' order of kinds, the ones the driver declares.
        for f in [Flavor::Neo4j, Flavor::Memgraph] {
            let declared: Vec<&str> = crate::kinds_for(f).iter().map(|k| k.id).collect();
            assert_eq!(kinds(f), declared);
        }
    }
}
