//! The explorer's pages (`Session::list_objects_page`): a folder of the
//! tree, a name search or a lookup as one catalog query over the objects
//! `list_objects` reads (`LIST_OBJECTS` and `structure::OBJECTS`: the
//! schema's own, then public synonyms of them under `PUBLIC`), only from
//! the system views of the kinds asked for, narrowed by name, sorted as the
//! tree shows them and cut to the page with `LIMIT … OFFSET` (the cursor is
//! the offset, as `objects::page_of`'s). The schema, names and patterns are
//! bound (lower-cased here, as `page_of` compares them); the SQL holds only
//! catalog text and numbers.

use crate::text;
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery};
use hdbconnect_async::HdbValue;

/// One kind's system view, as `list_objects` reads it.
struct Source {
    kind: &'static str,
    /// Listed under `PUBLIC` (a public synonym), not at the database level.
    public: bool,
    from: &'static str,
    name: &'static str,
    /// The owner (a trigger's table), or `None`.
    parent: Option<&'static str>,
    /// Each `?` takes the session's schema.
    filter: &'static str,
}

/// In `list_objects`' order.
const SOURCES: &[Source] = &[
    Source {
        kind: kinds::TABLE,
        public: false,
        from: "SYS.TABLES",
        name: "TABLE_NAME",
        parent: None,
        filter: "SCHEMA_NAME = ? AND IS_SYSTEM_TABLE = 'FALSE' AND IS_USER_DEFINED_TYPE = 'FALSE'",
    },
    Source {
        kind: kinds::VIEW,
        public: false,
        from: "SYS.VIEWS",
        name: "VIEW_NAME",
        parent: None,
        filter: "SCHEMA_NAME = ?",
    },
    Source {
        kind: kinds::PROCEDURE,
        public: false,
        from: "SYS.PROCEDURES",
        name: "PROCEDURE_NAME",
        parent: None,
        filter: "SCHEMA_NAME = ?",
    },
    Source {
        kind: kinds::FUNCTION,
        public: false,
        from: "SYS.FUNCTIONS",
        name: "FUNCTION_NAME",
        parent: None,
        filter: "SCHEMA_NAME = ?",
    },
    Source {
        kind: kinds::TRIGGER,
        public: false,
        from: "SYS.TRIGGERS",
        name: "TRIGGER_NAME",
        parent: Some("SUBJECT_TABLE_NAME"),
        filter: "SCHEMA_NAME = ?",
    },
    Source {
        kind: kinds::SEQUENCE,
        public: false,
        from: "SYS.SEQUENCES",
        name: "SEQUENCE_NAME",
        parent: None,
        filter: "SCHEMA_NAME = ?",
    },
    Source {
        kind: kinds::SYNONYM,
        public: false,
        from: "SYS.SYNONYMS",
        name: "SYNONYM_NAME",
        parent: None,
        filter: "SCHEMA_NAME = ?",
    },
    Source {
        kind: kinds::SYNONYM,
        public: true,
        from: "SYS.SYNONYMS",
        name: "SYNONYM_NAME",
        parent: None,
        filter: "SCHEMA_NAME = 'PUBLIC' AND OBJECT_SCHEMA = ?",
    },
    Source {
        kind: kinds::TYPE,
        public: false,
        from: "SYS.TABLES",
        name: "TABLE_NAME",
        parent: None,
        filter: "SCHEMA_NAME = ? AND IS_USER_DEFINED_TYPE = 'TRUE' AND TABLE_NAME NOT LIKE '\\_SYS%' ESCAPE '\\'",
    },
];

/// Whether `q` keeps objects of `kind`.
fn wants(q: &ObjectQuery, kind: &str) -> bool {
    q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
}

/// Whether `q` keeps objects listed under `PUBLIC` (`public`) or without
/// a schema.
fn in_schema(q: &ObjectQuery, public: bool) -> bool {
    if q.no_schema {
        !public
    } else {
        match q.schema.as_deref() {
            None => true,
            Some(s) => public && s == "PUBLIC",
        }
    }
}

/// `s` as a `LIKE` pattern matching it anywhere, `!` escaping the
/// wildcards.
fn contains_pattern(s: &str) -> String {
    let mut out = String::with_capacity(s.len() + 2);
    out.push('%');
    for c in s.chars() {
        if matches!(c, '!' | '%' | '_') {
            out.push('!');
        }
        out.push(c);
    }
    out.push('%');
    out
}

/// A page's query: the SQL, its bound values in order, and the slice of
/// the sorted list it asks for.
#[derive(Debug)]
pub(crate) struct PageQuery {
    pub sql: String,
    pub params: Vec<String>,
    offset: usize,
    size: usize,
}

/// The query for page `q` of schema `schema`, or `None` when no object can
/// match.
pub(crate) fn page_query(schema: &str, q: &ObjectQuery) -> Option<PageQuery> {
    let mut params = Vec::new();
    let mut branches = Vec::new();
    for (rank, s) in SOURCES.iter().enumerate() {
        if !wants(q, s.kind) || !in_schema(q, s.public) {
            continue;
        }
        let mut conds = vec![s.filter.to_string()];
        params.extend(s.filter.matches('?').map(|_| schema.to_string()));
        if let Some(n) = &q.name {
            conds.push(format!("LOWER({}) = ?", s.name));
            params.push(n.to_lowercase());
        }
        if let Some(p) = &q.pattern {
            conds.push(format!("LOWER({}) LIKE ? ESCAPE '!'", s.name));
            params.push(contains_pattern(&p.to_lowercase()));
        }
        let parent = s.parent.unwrap_or("CAST(NULL AS NVARCHAR(256))");
        let (in_tree, sr) = if s.public {
            ("CAST('PUBLIC' AS NVARCHAR(256))", 1)
        } else {
            ("CAST(NULL AS NVARCHAR(256))", 0)
        };
        branches.push(format!(
            "SELECT CAST('{}' AS NVARCHAR(20)) AS k, {in_tree} AS s, {} AS n, {parent} AS p, \
             {sr} AS sr, {rank} AS r FROM {} WHERE {}",
            s.kind,
            s.name,
            s.from,
            conds.join(" AND ")
        ));
    }
    if branches.is_empty() {
        return None;
    }
    let offset = q
        .after
        .as_deref()
        .and_then(|a| a.parse::<usize>().ok())
        .unwrap_or(0);
    let size = q.page_size();
    let sql = format!(
        "SELECT o.k, o.s, o.n, o.p FROM ({}) o ORDER BY o.sr, LOWER(o.n), o.n, o.r \
         LIMIT {} OFFSET {offset}",
        branches.join(" UNION ALL "),
        size.saturating_add(1)
    );
    Some(PageQuery {
        sql,
        params,
        offset,
        size,
    })
}

impl PageQuery {
    /// The page out of the query's rows (one more than the page when more
    /// remain).
    pub(crate) fn page(&self, rows: &[Vec<HdbValue<'static>>]) -> ObjectPage {
        let more = rows.len() > self.size;
        let objects = rows
            .iter()
            .take(self.size)
            .filter_map(|r| {
                Some(DbObject {
                    kind: text(r.first()?)?,
                    schema: r.get(1).and_then(text),
                    name: text(r.get(2)?)?,
                    parent: r.get(3).and_then(text),
                })
            })
            .collect();
        ObjectPage {
            objects,
            next: more.then(|| (self.offset + self.size).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// The same objects as `list_objects`: its queries read each source so.
    #[test]
    fn sources_are_list_objects_own() {
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let list = flat(&format!(
            "{} {}",
            crate::LIST_OBJECTS,
            crate::structure::OBJECTS
        ));
        for s in SOURCES {
            let line = format!(
                "{} FROM {} WHERE {}",
                s.parent.unwrap_or(s.name),
                s.from,
                s.filter
            );
            // `list_objects` puts a NULL parent last; synonyms and types
            // their name.
            let alt = format!("NULL FROM {} WHERE {}", s.from, s.filter);
            assert!(
                list.contains(&flat(&line)) || list.contains(&flat(&alt)),
                "{line}\n{list}"
            );
            assert!(list.contains(&format!("'{}'", s.kind)), "{}", s.kind);
        }
    }

    #[test]
    fn a_folder_reads_its_view_only() {
        let q = ObjectQuery {
            no_schema: true,
            kind: Some("procedure".into()),
            after: Some("20".into()),
            limit: 10,
            ..Default::default()
        };
        let p = page_query("SALES", &q).unwrap();
        assert_eq!(p.params, ["SALES"]);
        assert_eq!(p.sql.matches(" FROM SYS.").count(), 1, "{}", p.sql);
        assert!(p.sql.contains("FROM SYS.PROCEDURES WHERE SCHEMA_NAME = ?"));
        assert!(p.sql.ends_with("LIMIT 11 OFFSET 20"));
        // Triggers come with their table.
        let q = ObjectQuery {
            kind: Some("trigger".into()),
            ..Default::default()
        };
        let p = page_query("SALES", &q).unwrap();
        assert!(p.sql.contains("SUBJECT_TABLE_NAME AS p"), "{}", p.sql);
        assert!(p.sql.ends_with("LIMIT 1001 OFFSET 0"));
    }

    #[test]
    fn public_synonyms_sit_under_public() {
        let q = ObjectQuery {
            schema: Some("PUBLIC".into()),
            ..Default::default()
        };
        let p = page_query("SALES", &q).unwrap();
        assert_eq!(p.sql.matches(" FROM SYS.").count(), 1, "{}", p.sql);
        assert!(p
            .sql
            .contains("FROM SYS.SYNONYMS WHERE SCHEMA_NAME = 'PUBLIC' AND OBJECT_SCHEMA = ?"));
        assert!(p.sql.contains("CAST('PUBLIC' AS NVARCHAR(256)) AS s"));
        // The schema's own synonyms, at the database level.
        let q = ObjectQuery {
            no_schema: true,
            kind: Some("synonym".into()),
            ..Default::default()
        };
        let p = page_query("SALES", &q).unwrap();
        assert!(!p.sql.contains("'PUBLIC'"), "{}", p.sql);
        // Both, when no schema is asked for.
        let q = ObjectQuery {
            kind: Some("synonym".into()),
            ..Default::default()
        };
        assert_eq!(page_query("SALES", &q).unwrap().params.len(), 2);
        // Another schema, or "Otros": nothing.
        for q in [
            ObjectQuery {
                schema: Some("SYS".into()),
                ..Default::default()
            },
            ObjectQuery {
                not_kinds: SOURCES.iter().map(|s| s.kind.to_string()).collect(),
                ..Default::default()
            },
        ] {
            assert!(page_query("SALES", &q).is_none(), "{q:?}");
        }
    }

    #[test]
    fn user_text_is_bound_and_wildcards_escaped() {
        let q = ObjectQuery {
            kind: Some("view".into()),
            pattern: Some("A_1%!'".into()),
            name: Some("X' OR 1=1 --".into()),
            ..Default::default()
        };
        let p = page_query("SALES", &q).unwrap();
        assert!(
            !p.sql.contains("A_1") && !p.sql.contains("1=1"),
            "{}",
            p.sql
        );
        assert_eq!(p.params, ["SALES", "x' or 1=1 --", "%a!_1!%!!'%"]);
        assert!(p
            .sql
            .contains("LOWER(VIEW_NAME) = ? AND LOWER(VIEW_NAME) LIKE ? ESCAPE '!'"));
    }

    #[test]
    fn rows_become_the_page() {
        let q = ObjectQuery {
            limit: 1,
            ..Default::default()
        };
        let p = page_query("SALES", &q).unwrap();
        let row = |k: &'static str, s: Option<&'static str>, n: &'static str| {
            vec![
                HdbValue::STR(k),
                s.map_or(HdbValue::NULL, HdbValue::STR),
                HdbValue::STR(n),
                HdbValue::NULL,
            ]
        };
        let page = p.page(&[row("synonym", Some("PUBLIC"), "S"), row("table", None, "T")]);
        assert_eq!(page.next.as_deref(), Some("1"));
        assert_eq!(page.objects.len(), 1);
        assert_eq!(page.objects[0].schema.as_deref(), Some("PUBLIC"));
        assert_eq!(page.objects[0].parent, None);
    }
}
