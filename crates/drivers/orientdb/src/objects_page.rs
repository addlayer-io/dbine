//! The explorer's pages (`Session::list_objects_page`) out of the sources
//! `list_objects` reads, only the kinds asked for, put together with
//! [`crate::object_pages`]. Nothing has a schema.
//!
//! - **Classes** (vertices, edges, documents): a class's kind comes from
//!   its ancestors, so the classes are read whole, but only their names
//!   and superclasses (`metadata:schema`, not the whole schema
//!   `list_objects` reads).
//! - **Indexes** (`metadata:indexmanager`), **functions** (`OFunction`)
//!   and **sequences** (`OSequence`): only the kind asked for, narrowed by
//!   the server to the search or the lookup
//!   (`name.toLowerCase().indexOf(…)`, `name.toLowerCase() = …`; Java's
//!   lowercase is Rust's).
//!
//! OrientDB sorts strings with the database's collator (by language:
//! `ñ` next to `n`, `_` before digits, `a` before `A`), not as the tree
//! does, so it can't page in the tree's order: each kind's narrowed list
//! is sorted and paged here. With more than [`object_pages::WHOLE_MAX`]
//! indexes, functions or sequences in one, the pages are left to the app.
//!
//! A read-only connection reads by GET, without bound values: names and
//! patterns go as string literals ([`string`]).

use crate::ddl::string;
use crate::{object_pages, EDGE, VERTEX};
use dbine_driver::{kinds, DbObject, ObjectQuery};
use serde_json::Value;

/// The kinds of the classes, in `list_objects`' order (all from one list).
pub(crate) const CLASS_KINDS: &[&str] = &[VERTEX, EDGE, kinds::TABLE];

/// A kind the server narrows by name.
#[derive(Debug, Clone, Copy)]
pub(crate) struct Source {
    pub kind: &'static str,
    /// `FROM` and the conditions every row needs (`list_objects`').
    from: &'static str,
    /// The owner's column (an index's class), if any.
    parent: Option<&'static str>,
    /// Missing or refused: `list_objects` goes without it.
    pub optional: bool,
}

/// After the classes, in `list_objects`' order.
pub(crate) const SOURCES: &[Source] = &[
    Source {
        kind: kinds::INDEX,
        from: "(SELECT expand(indexes) FROM metadata:indexmanager) WHERE \
               (indexDefinition.className IS NULL OR indexDefinition.className NOT IN {system})",
        parent: Some("indexDefinition.className"),
        optional: false,
    },
    Source {
        kind: kinds::FUNCTION,
        from: "OFunction WHERE true",
        parent: None,
        optional: true,
    },
    Source {
        kind: kinds::SEQUENCE,
        from: "OSequence WHERE true",
        parent: None,
        optional: true,
    },
];

/// `s`'s names that `q`'s search or lookup keeps, at most
/// [`object_pages::WHOLE_MAX`] + 1.
pub(crate) fn narrowed(s: &Source, q: &ObjectQuery) -> String {
    let system = format!(
        "[{}]",
        crate::system_classes()
            .map(string)
            .collect::<Vec<_>>()
            .join(", ")
    );
    let parent = s.parent.map(|p| format!(", {p} AS p")).unwrap_or_default();
    let mut sql = format!(
        "SELECT name{parent} FROM {}",
        s.from.replace("{system}", &system)
    );
    if let Some(n) = &q.name {
        sql.push_str(&format!(
            " AND name.toLowerCase() = {}",
            string(&n.to_lowercase())
        ));
    }
    if let Some(p) = &q.pattern {
        sql.push_str(&format!(
            " AND name.toLowerCase().indexOf({}) > -1",
            string(&p.to_lowercase())
        ));
    }
    sql.push_str(&format!(" LIMIT {}", object_pages::WHOLE_MAX + 1));
    sql
}

/// The light class list: names and superclasses, which give the kinds.
pub(crate) const CLASSES: &str =
    "SELECT name, superClass, superClasses FROM (SELECT expand(classes) FROM metadata:schema)";

/// The objects of `kind` in a query's rows.
pub(crate) fn objects(kind: &'static str, rows: &[Vec<(String, Value)>]) -> Vec<DbObject> {
    rows.iter()
        .filter_map(|r| {
            let get = |k: &str| r.iter().find(|(c, _)| c == k).map(|(_, v)| v);
            Some(DbObject {
                kind: kind.into(),
                schema: None,
                name: get("name")?.as_str()?.to_string(),
                parent: get("p").and_then(Value::as_str).map(str::to_string),
            })
        })
        .collect()
}

/// The class rows as the class JSON `classify` takes.
pub(crate) fn class_values(rows: &[Vec<(String, Value)>]) -> Vec<Value> {
    rows.iter()
        .map(|r| Value::Object(r.iter().cloned().collect()))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_reads_its_kind_only() {
        let q = ObjectQuery {
            kind: Some(kinds::SEQUENCE.into()),
            ..Default::default()
        };
        assert_eq!(
            narrowed(&SOURCES[2], &q),
            "SELECT name FROM OSequence WHERE true LIMIT 10001"
        );
        // Indexes of user classes (and manual ones), with their class.
        let sql = narrowed(&SOURCES[0], &q);
        assert!(
            sql.starts_with(
                "SELECT name, indexDefinition.className AS p \
                 FROM (SELECT expand(indexes) FROM metadata:indexmanager) WHERE \
                 (indexDefinition.className IS NULL OR indexDefinition.className NOT IN ['OUser', "
            ),
            "{sql}"
        );
        assert!(sql.contains("'OMultiPolygon'"), "{sql}");
    }

    #[test]
    fn user_text_is_quoted_lowercased() {
        let q = ObjectQuery {
            pattern: Some("A' OR 1=1 \\".into()),
            name: Some("Ñu".into()),
            ..Default::default()
        };
        let sql = narrowed(&SOURCES[1], &q);
        assert_eq!(
            sql,
            r"SELECT name FROM OFunction WHERE true AND name.toLowerCase() = 'ñu' AND name.toLowerCase().indexOf('a\' or 1=1 \\') > -1 LIMIT 10001"
        );
    }

    #[test]
    fn rows_become_objects() {
        let rows = vec![
            vec![
                ("name".to_string(), Value::from("Doc.x")),
                ("p".to_string(), Value::from("Doc")),
            ],
            vec![
                ("name".to_string(), Value::from("manual")),
                ("p".to_string(), Value::Null),
            ],
        ];
        let o = objects(kinds::INDEX, &rows);
        assert_eq!(o[0].parent.as_deref(), Some("Doc"));
        assert_eq!(o[1].parent, None);
        let v = class_values(&[vec![
            ("name".to_string(), Value::from("Person")),
            ("superClass".to_string(), Value::from("V")),
        ]]);
        assert_eq!(crate::classify(&v)[0].1, VERTEX);
    }
}
