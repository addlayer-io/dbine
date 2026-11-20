//! The explorer's pages (`Session::list_objects_page`): a folder of the
//! tree, a name search or a lookup as one catalog query over the objects
//! `list_objects` reads (`LIST_OBJECTS`), only from the system tables of
//! the kinds asked for, narrowed by name, sorted as the tree shows them and
//! cut to the page with `OFFSET … FETCH` (Firebird 3+; the cursor is the
//! offset, as `objects::page_of`'s). Names and patterns are bound; the SQL
//! holds only catalog text and numbers. A database has no schemas here.

use crate::{text, PACKAGE};
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery};
use rsfbclient_core::Column;

/// The longest name the catalog holds (63 characters since Firebird 4,
/// 31 before): a longer name or search matches nothing.
const NAME_MAX: usize = 63;

/// One kind's system table, as `LIST_OBJECTS` reads it.
struct Source {
    kind: &'static str,
    from: &'static str,
    name: &'static str,
    /// The owner (a trigger's table), or `None`.
    parent: Option<&'static str>,
    filter: &'static str,
}

const USER: &str = "COALESCE(RDB$SYSTEM_FLAG, 0) = 0";

/// In `LIST_OBJECTS`' order of kinds.
const SOURCES: &[Source] = &[
    Source {
        kind: kinds::TABLE,
        from: "RDB$RELATIONS",
        name: "RDB$RELATION_NAME",
        parent: None,
        filter: "RDB$VIEW_BLR IS NULL",
    },
    Source {
        kind: kinds::VIEW,
        from: "RDB$RELATIONS",
        name: "RDB$RELATION_NAME",
        parent: None,
        filter: "RDB$VIEW_BLR IS NOT NULL",
    },
    Source {
        kind: kinds::PROCEDURE,
        from: "RDB$PROCEDURES",
        name: "RDB$PROCEDURE_NAME",
        parent: None,
        filter: "RDB$PACKAGE_NAME IS NULL",
    },
    Source {
        kind: kinds::FUNCTION,
        from: "RDB$FUNCTIONS",
        name: "RDB$FUNCTION_NAME",
        parent: None,
        filter: "RDB$PACKAGE_NAME IS NULL",
    },
    Source {
        kind: PACKAGE,
        from: "RDB$PACKAGES",
        name: "RDB$PACKAGE_NAME",
        parent: None,
        filter: "",
    },
    Source {
        kind: kinds::TRIGGER,
        from: "RDB$TRIGGERS",
        name: "RDB$TRIGGER_NAME",
        parent: Some("RDB$RELATION_NAME"),
        filter: "",
    },
    Source {
        kind: kinds::SEQUENCE,
        from: "RDB$GENERATORS",
        name: "RDB$GENERATOR_NAME",
        parent: None,
        filter: "",
    },
    Source {
        kind: kinds::TYPE,
        from: "RDB$FIELDS",
        name: "RDB$FIELD_NAME",
        parent: None,
        filter: "RDB$FIELD_NAME NOT STARTING WITH 'RDB$'",
    },
];

/// Whether `q` keeps objects of `kind`.
fn wants(q: &ObjectQuery, kind: &str) -> bool {
    q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
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

/// The query for page `q`, or `None` when no object can match (a schema
/// asked for, no kind left, a name longer than any).
pub(crate) fn page_query(q: &ObjectQuery) -> Option<PageQuery> {
    if !q.no_schema && q.schema.is_some() {
        return None;
    }
    let too_long = |s: &Option<String>| s.as_deref().is_some_and(|s| s.chars().count() > NAME_MAX);
    if too_long(&q.name) || too_long(&q.pattern) {
        return None;
    }
    let mut params = Vec::new();
    let mut branches = Vec::new();
    for (rank, s) in SOURCES.iter().enumerate() {
        if !wants(q, s.kind) {
            continue;
        }
        let name = format!("TRIM({})", s.name);
        let mut conds = vec![USER.to_string()];
        if !s.filter.is_empty() {
            conds.push(s.filter.to_string());
        }
        // Firebird can't type a bare `?` inside LOWER: it's cast.
        if let Some(n) = &q.name {
            conds.push(format!(
                "LOWER({name}) = LOWER(CAST(? AS VARCHAR({NAME_MAX})))"
            ));
            params.push(n.clone());
        }
        if let Some(p) = &q.pattern {
            conds.push(format!(
                "LOWER({name}) LIKE LOWER(CAST(? AS VARCHAR({}))) ESCAPE '!'",
                2 * NAME_MAX + 2
            ));
            params.push(contains_pattern(p));
        }
        let parent = match s.parent {
            Some(p) => format!("TRIM({p})"),
            None => "CAST(NULL AS VARCHAR(63))".into(),
        };
        branches.push(format!(
            "SELECT CAST('{}' AS VARCHAR(20)) AS k, {name} AS n, {parent} AS p, {rank} AS r \
             FROM {} WHERE {}",
            s.kind,
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
        "SELECT x.k, x.n, x.p FROM ({}) x ORDER BY LOWER(x.n), x.n, x.r \
         OFFSET {offset} ROWS FETCH NEXT {} ROWS ONLY",
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
    pub(crate) fn page(&self, rows: &[Vec<Column>]) -> ObjectPage {
        let more = rows.len() > self.size;
        let objects = rows
            .iter()
            .take(self.size)
            .filter_map(|r| {
                Some(DbObject {
                    kind: text(r.first()?)?,
                    schema: None,
                    name: text(r.get(1)?)?,
                    parent: r.get(2).and_then(text).filter(|p| !p.is_empty()),
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

    /// The same objects as `list_objects`: its query reads each source so.
    #[test]
    fn sources_are_list_objects_own() {
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let list = flat(crate::LIST_OBJECTS);
        for (i, s) in SOURCES.iter().enumerate() {
            let filter = if s.filter.is_empty() {
                String::new()
            } else {
                format!(" AND {}", s.filter)
            };
            // The first SELECT types the columns.
            let (kind, parent) = match (i, s.parent) {
                (0, _) => (
                    format!("CAST('{}' AS VARCHAR(20))", s.kind),
                    "CAST(NULL AS VARCHAR(63))".to_string(),
                ),
                (_, Some(p)) => (format!("'{}'", s.kind), format!("TRIM({p})")),
                (_, None) => (format!("'{}'", s.kind), "NULL".to_string()),
            };
            let line = format!(
                "SELECT {kind}, TRIM({}), {parent} FROM {} WHERE {USER}{filter}",
                s.name, s.from
            );
            assert!(list.contains(&flat(&line)), "{line}\n{list}");
        }
    }

    #[test]
    fn a_folder_reads_its_table_only() {
        let q = ObjectQuery {
            no_schema: true,
            kind: Some("sequence".into()),
            after: Some("20".into()),
            limit: 10,
            ..Default::default()
        };
        let p = page_query(&q).unwrap();
        assert!(p.params.is_empty());
        assert_eq!(p.sql.matches("FROM RDB$").count(), 1, "{}", p.sql);
        assert!(p
            .sql
            .contains("FROM RDB$GENERATORS WHERE COALESCE(RDB$SYSTEM_FLAG, 0) = 0"));
        assert!(p.sql.ends_with("OFFSET 20 ROWS FETCH NEXT 11 ROWS ONLY"));
        // Triggers come with their table.
        let q = ObjectQuery {
            kind: Some("trigger".into()),
            ..Default::default()
        };
        let p = page_query(&q).unwrap();
        assert!(p.sql.contains("TRIM(RDB$RELATION_NAME) AS p"), "{}", p.sql);
        assert!(p.sql.ends_with("OFFSET 0 ROWS FETCH NEXT 1001 ROWS ONLY"));
    }

    #[test]
    fn nothing_to_ask_for() {
        for q in [
            ObjectQuery {
                schema: Some("PUBLIC".into()),
                ..Default::default()
            },
            ObjectQuery {
                not_kinds: SOURCES.iter().map(|s| s.kind.to_string()).collect(),
                ..Default::default()
            },
            ObjectQuery {
                name: Some("x".repeat(NAME_MAX + 1)),
                ..Default::default()
            },
        ] {
            assert!(page_query(&q).is_none(), "{q:?}");
        }
        // A schema with "no schema" asked for is ignored.
        let q = ObjectQuery {
            schema: Some("PUBLIC".into()),
            no_schema: true,
            ..Default::default()
        };
        assert_eq!(
            page_query(&q).unwrap().sql.matches(" UNION ALL ").count(),
            7
        );
    }

    #[test]
    fn user_text_is_bound_and_wildcards_escaped() {
        let q = ObjectQuery {
            kind: Some("view".into()),
            pattern: Some("a_1%!'".into()),
            name: Some("x' OR 1=1 --".into()),
            ..Default::default()
        };
        let p = page_query(&q).unwrap();
        assert!(
            !p.sql.contains("a_1") && !p.sql.contains("1=1"),
            "{}",
            p.sql
        );
        assert_eq!(p.params, ["x' OR 1=1 --", "%a!_1!%!!'%"]);
        assert!(p
            .sql
            .contains("LOWER(TRIM(RDB$RELATION_NAME)) = LOWER(CAST(? AS VARCHAR(63)))"));
        assert!(p.sql.contains(
            "LOWER(TRIM(RDB$RELATION_NAME)) LIKE LOWER(CAST(? AS VARCHAR(128))) ESCAPE '!'"
        ));
    }
}
