//! The explorer's pages (`Session::list_objects_page`) as catalog queries
//! over the objects `list_objects` reads ([`SOURCES`]), only the kinds
//! asked for, in one `UNION ALL` (put together with
//! [`crate::object_pages`]). Every object lives in the session's database
//! (its schema).
//!
//! TDengine's `LOWER` folds ASCII only, so the tree's order (Rust's
//! `to_lowercase`) comes in two parts: names in ASCII
//! (`LENGTH = CHAR_LENGTH`), which the server filters, sorts
//! (`ORDER BY LOWER(n), n`) and pages (`LIMIT … OFFSET …`); and names with
//! other characters, read whole and sorted here (with more than
//! [`object_pages::WHOLE_MAX`] of them the pages are left to the app).
//! The REST API takes no bound values: names and patterns go as string
//! literals ([`lit`]).
//!
//! Unlike `list_objects`, which reads at most `MAX_SUBTABLES` subtables,
//! the pages reach every one of them.

use crate::ddl::{lit, SUBTABLE, SUPERTABLE};
use crate::object_pages;
use dbine_driver::{kinds, DbObject, ObjectQuery};
use serde_json::Value;

/// One kind's catalog table, as `list_objects` reads it.
pub(crate) struct Source {
    pub kind: &'static str,
    pub from: &'static str,
    name: &'static str,
    /// The owner (a subtable's supertable), or `None`.
    parent: Option<&'static str>,
    /// The column with the database.
    pub db_col: &'static str,
    filter: &'static str,
    /// Missing on some editions and versions (`list_objects` skips them).
    pub optional: bool,
}

/// In `list_objects`' order of kinds.
pub(crate) const SOURCES: &[Source] = &[
    Source {
        kind: SUPERTABLE,
        from: "ins_stables",
        name: "stable_name",
        parent: None,
        db_col: "db_name",
        filter: "",
        optional: false,
    },
    Source {
        kind: kinds::TABLE,
        from: "ins_tables",
        name: "table_name",
        parent: None,
        db_col: "db_name",
        filter: "type = 'NORMAL_TABLE'",
        optional: false,
    },
    Source {
        kind: SUBTABLE,
        from: "ins_tables",
        name: "table_name",
        parent: Some("stable_name"),
        db_col: "db_name",
        filter: "type = 'CHILD_TABLE'",
        optional: false,
    },
    Source {
        kind: kinds::VIEW,
        from: "ins_views",
        name: "view_name",
        parent: None,
        db_col: "db_name",
        filter: "",
        optional: true,
    },
    Source {
        kind: kinds::STREAM,
        from: "ins_streams",
        name: "stream_name",
        parent: None,
        db_col: "source_db",
        filter: "",
        optional: true,
    },
    Source {
        kind: kinds::TOPIC,
        from: "ins_topics",
        name: "topic_name",
        parent: None,
        db_col: "db_name",
        filter: "",
        optional: true,
    },
];

/// The query that tells whether an optional catalog exists here (as
/// `list_objects` reads it).
pub(crate) fn probe(s: &Source) -> String {
    format!(
        "SELECT COUNT(*) FROM information_schema.{} WHERE {} = ''",
        s.from, s.db_col
    )
}

/// The sources `q` reads (indexes into [`SOURCES`]), leaving out the
/// catalogs this server lacks.
pub(crate) fn sources(q: &ObjectQuery, db: &str, missing: &[&str]) -> Vec<usize> {
    if !object_pages::schema_ok(q, Some(db)) {
        return Vec::new();
    }
    SOURCES
        .iter()
        .enumerate()
        .filter(|(_, s)| object_pages::wants(q, s.kind) && !missing.contains(&s.from))
        .map(|(i, _)| i)
        .collect()
}

/// `s` as a `LIKE` pattern matching it anywhere, `\` escaping the
/// wildcards (then quoted with [`lit`]).
fn contains_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '\\' | '%' | '_') {
            out.push('\\');
        }
        out.push(c);
    }
    out.push('%');
    out
}

const NAME: &str = "VARCHAR(192)";

/// `UNION ALL` of `srcs`, each row `(n, p, r)`: name, parent, source.
fn union(db: &str, srcs: &[usize], conds: &[String]) -> String {
    srcs.iter()
        .map(|&i| {
            let s = &SOURCES[i];
            let mut w = vec![format!("{} = {}", s.db_col, lit(db))];
            if !s.filter.is_empty() {
                w.push(s.filter.to_string());
            }
            w.extend(conds.iter().map(|c| c.replace("{n}", s.name)));
            let parent = match s.parent {
                Some(p) => format!("CAST({p} AS {NAME})"),
                None => format!("CAST(NULL AS {NAME})"),
            };
            format!(
                "SELECT CAST({} AS {NAME}) AS n, {parent} AS p, {i} AS r \
                 FROM information_schema.{} WHERE {}",
                s.name,
                s.from,
                w.join(" AND ")
            )
        })
        .collect::<Vec<_>>()
        .join(" UNION ALL ")
}

/// The page of the names in ASCII from `offset` (one more than `size`);
/// `None` when no such name can match.
pub(crate) fn ascii_page(
    q: &ObjectQuery,
    db: &str,
    srcs: &[usize],
    offset: usize,
    size: usize,
) -> Option<String> {
    if srcs.is_empty() {
        return None;
    }
    let mut conds = vec!["LENGTH({n}) = CHAR_LENGTH({n})".to_string()];
    if let Some(n) = &q.name {
        let n = n.to_lowercase();
        if !n.is_ascii() {
            return None;
        }
        conds.push(format!("LOWER({{n}}) = {}", lit(&n)));
    }
    if let Some(p) = &q.pattern {
        let p = p.to_lowercase();
        if !p.is_ascii() {
            return None;
        }
        conds.push(format!("LOWER({{n}}) LIKE {}", lit(&contains_pattern(&p))));
    }
    Some(format!(
        "SELECT n, p, r FROM ({}) x ORDER BY LOWER(n), n, r LIMIT {} OFFSET {offset}",
        union(db, srcs, &conds),
        size.saturating_add(1)
    ))
}

/// The names with characters outside ASCII (filtered and sorted here), in
/// `list_objects`' order of kinds, at most [`object_pages::WHOLE_MAX`] + 1.
pub(crate) fn others(db: &str, srcs: &[usize]) -> String {
    format!(
        "SELECT n, p, r FROM ({}) x ORDER BY r LIMIT {}",
        union(db, srcs, &["LENGTH({n}) <> CHAR_LENGTH({n})".to_string()]),
        object_pages::WHOLE_MAX + 1
    )
}

/// The objects of `(n, p, r)` rows.
pub(crate) fn objects(rows: Vec<Vec<Value>>, db: &str) -> Vec<DbObject> {
    rows.into_iter()
        .filter_map(|r| {
            let source = SOURCES.get(usize::try_from(r.get(2)?.as_u64()?).ok()?)?;
            Some(DbObject {
                kind: source.kind.into(),
                schema: Some(db.into()),
                name: r.first()?.as_str()?.to_string(),
                parent: r
                    .get(1)
                    .and_then(Value::as_str)
                    .filter(|p| !p.is_empty())
                    .map(str::to_string),
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// The same objects as `list_objects`: its queries read each source so.
    #[test]
    fn sources_are_list_objects_own() {
        let lib = include_str!("lib.rs");
        let start = lib.find("async fn list_objects(").unwrap();
        let end = start + lib[start..].find("async fn columns(").unwrap();
        let list = &lib[start..end];
        for s in SOURCES {
            assert!(
                list.contains(&format!("{} FROM information_schema.{}", s.name, s.from))
                    || list.contains(&format!(
                        "{}, {} FROM information_schema.{}",
                        s.name,
                        s.parent.unwrap_or("-"),
                        s.from
                    )),
                "{}",
                s.from
            );
            assert!(
                list.contains(&format!("WHERE {} = {{}}", s.db_col)),
                "{}",
                s.db_col
            );
            if !s.filter.is_empty() {
                assert!(list.contains(s.filter), "{}", s.filter);
            }
        }
    }

    #[test]
    fn a_folder_reads_its_kind_only() {
        let q = ObjectQuery {
            kind: Some(SUBTABLE.into()),
            ..Default::default()
        };
        let srcs = sources(&q, "db", &[]);
        assert_eq!(srcs, [2]);
        let sql = ascii_page(&q, "db", &srcs, 20, 10).unwrap();
        assert_eq!(
            sql,
            "SELECT n, p, r FROM (SELECT CAST(table_name AS VARCHAR(192)) AS n, \
             CAST(stable_name AS VARCHAR(192)) AS p, 2 AS r FROM information_schema.ins_tables \
             WHERE db_name = 'db' AND type = 'CHILD_TABLE' \
             AND LENGTH(table_name) = CHAR_LENGTH(table_name)) x \
             ORDER BY LOWER(n), n, r LIMIT 11 OFFSET 20"
        );
        assert!(others("db", &srcs)
            .ends_with("LENGTH(table_name) <> CHAR_LENGTH(table_name)) x ORDER BY r LIMIT 10001"));
        // "Otros", another database, catalogs this server lacks.
        let all: Vec<String> = SOURCES.iter().map(|s| s.kind.to_string()).collect();
        let none = ObjectQuery {
            not_kinds: all,
            ..Default::default()
        };
        assert!(sources(&none, "db", &[]).is_empty());
        let other = ObjectQuery {
            schema: Some("other".into()),
            ..Default::default()
        };
        assert!(sources(&other, "db", &[]).is_empty());
        let loose = ObjectQuery {
            no_schema: true,
            ..Default::default()
        };
        assert!(sources(&loose, "db", &[]).is_empty());
        assert_eq!(
            sources(&ObjectQuery::default(), "db", &["ins_views", "ins_streams"]),
            [0, 1, 2, 5]
        );
    }

    #[test]
    fn user_text_is_quoted_and_wildcards_escaped() {
        let q = ObjectQuery {
            kind: Some(kinds::TABLE.into()),
            pattern: Some("A_1%\\'".into()),
            name: Some("x' OR 1=1 --".into()),
            ..Default::default()
        };
        let sql = ascii_page(&q, "d'b", &sources(&q, "d'b", &[]), 0, 5).unwrap();
        assert!(sql.contains("WHERE db_name = 'd''b'"), "{sql}");
        assert!(sql.contains("LOWER(table_name) = 'x'' or 1=1 --'"), "{sql}");
        assert!(
            sql.contains(r"LOWER(table_name) LIKE '%a\\_1\\%\\\\''%'"),
            "{sql}"
        );
        // Text outside ASCII: only the names read whole can match.
        let q = ObjectQuery {
            pattern: Some("ñ".into()),
            ..Default::default()
        };
        assert!(ascii_page(&q, "db", &[0], 0, 5).is_none());
    }

    #[test]
    fn rows_become_objects() {
        let rows = vec![
            vec![json!("d1"), json!("st"), json!(2)],
            vec![json!("t"), Value::Null, json!(1)],
        ];
        let o = objects(rows, "db");
        assert_eq!(o[0].kind, SUBTABLE);
        assert_eq!(o[0].parent.as_deref(), Some("st"));
        assert_eq!(o[1].kind, kinds::TABLE);
        assert_eq!(o[1].schema.as_deref(), Some("db"));
        assert_eq!(o[1].parent, None);
    }
}
