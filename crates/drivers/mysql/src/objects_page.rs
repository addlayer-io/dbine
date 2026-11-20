//! The explorer's pages (`Session::list_objects_page`): a folder of the
//! tree, a name search or a lookup as one catalog query over the objects
//! `list_objects` reads, narrowed to the kinds and names asked for, sorted
//! as the tree shows them (name ignoring case, then as written) and cut to
//! the page with `LIMIT … OFFSET` (the cursor is the offset, as
//! `objects::page_of`'s). The database, names and patterns go as bound
//! values of a prepared statement, even on the engines whose user
//! statements get them as literals (`Variant::binds`): a catalog query with
//! text values is what their prepared statements take. The SQL holds only
//! catalog text and numbers.
//!
//! Every object here is at the database level (no schema).

use crate::session::at;
use crate::Variant;
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery};
use mysql_async::Row;

/// A catalog source of objects; `rank` is its place in `list_objects`
/// (two objects of the same name keep that order).
#[derive(Debug, Clone, Copy)]
pub(crate) struct Source {
    rank: u8,
    from: &'static str,
    /// The column naming the database.
    db: &'static str,
    name: &'static str,
    /// The owner (a trigger's table), or `NULL`.
    parent: &'static str,
    /// What always holds; each `?` takes the database.
    base: &'static str,
    /// The kinds it holds, each with the condition that picks it out
    /// (`""`: every row).
    kinds: &'static [(&'static str, &'static str)],
}

/// Tables and views of `information_schema.TABLES`, as `table_kind` reads
/// `TABLE_TYPE` (system views and sequences aren't relations).
const RELATION_KINDS: &[(&str, &str)] = &[
    (kinds::VIEW, "UPPER(COALESCE(TABLE_TYPE, '')) = 'VIEW'"),
    (
        kinds::TABLE,
        "UPPER(COALESCE(TABLE_TYPE, '')) NOT LIKE '%VIEW%' AND UPPER(COALESCE(TABLE_TYPE, '')) <> 'SEQUENCE'",
    ),
];

const RELATIONS: Source = Source {
    rank: 0,
    from: "information_schema.TABLES",
    db: "TABLE_SCHEMA",
    name: "TABLE_NAME",
    parent: "NULL",
    base: "",
    kinds: RELATION_KINDS,
};

const ROUTINES: Source = Source {
    rank: 2,
    from: "information_schema.ROUTINES",
    db: "ROUTINE_SCHEMA",
    name: "ROUTINE_NAME",
    parent: "NULL",
    base: "ROUTINE_TYPE IS NOT NULL",
    kinds: &[
        (kinds::PROCEDURE, "UPPER(ROUTINE_TYPE) = 'PROCEDURE'"),
        (kinds::FUNCTION, "UPPER(ROUTINE_TYPE) <> 'PROCEDURE'"),
    ],
};

const TRIGGERS: Source = Source {
    rank: 3,
    from: "information_schema.TRIGGERS",
    db: "TRIGGER_SCHEMA",
    name: "TRIGGER_NAME",
    parent: "EVENT_OBJECT_TABLE",
    base: "",
    kinds: &[(kinds::TRIGGER, "")],
};

/// MariaDB's and TiDB's sequences, rows of `information_schema.TABLES`.
const SEQUENCES: Source = Source {
    rank: 4,
    from: "information_schema.TABLES",
    db: "TABLE_SCHEMA",
    name: "TABLE_NAME",
    parent: "NULL",
    base: "TABLE_TYPE = 'SEQUENCE'",
    kinds: &[(kinds::SEQUENCE, "")],
};

const OCEANBASE_SEQUENCES: Source = Source {
    rank: 4,
    from: "oceanbase.DBA_OB_SEQUENCE_OBJECTS",
    db: "DATABASE_NAME",
    name: "SEQUENCE_NAME",
    parent: "NULL",
    base: "",
    kinds: &[(kinds::SEQUENCE, "")],
};

/// StarRocks' materialized views, which `information_schema.TABLES` also
/// lists as tables or views.
const STARROCKS_MVS: &str =
    "TABLE_NAME NOT IN (SELECT TABLE_NAME FROM information_schema.materialized_views \
     WHERE TABLE_SCHEMA = ? AND TABLE_NAME IS NOT NULL)";

const MATERIALIZED_VIEWS: Source = Source {
    rank: 1,
    from: "information_schema.materialized_views",
    db: "TABLE_SCHEMA",
    name: "TABLE_NAME",
    parent: "NULL",
    base: "",
    kinds: &[(kinds::MATERIALIZED_VIEW, "")],
};

/// How a catalog compares names.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Dialect {
    /// Collations that ignore case and accents (MySQL, MariaDB, TiDB,
    /// OceanBase, SingleStore): they'd tie `Orders` with `orders` and leave
    /// the order between them to chance, so names are compared and sorted
    /// as bytes (`CAST … AS BINARY`); a search is a `LIKE` with its
    /// wildcards escaped.
    Collated,
    /// Strings compared as bytes already, and no `LIKE … ESCAPE`
    /// (StarRocks, Doris, GreptimeDB): a search is `LOCATE`, which has no
    /// wildcards.
    Bytes,
}

/// What a variant's catalog holds and how to ask it.
pub(crate) struct Catalog {
    dialect: Dialect,
    sources: Vec<Source>,
}

/// The sources of the objects `list_objects` returns on `v`, or `None`
/// where they can't be read by pages: Manticore lists its tables only with
/// `SHOW TABLES`, Databend its sequences only with `SHOW SEQUENCES`.
pub(crate) fn catalog(v: Variant) -> Option<Catalog> {
    let dialect = match v {
        Variant::MySql
        | Variant::MariaDb
        | Variant::TiDb
        | Variant::OceanBase
        | Variant::SingleStore => Dialect::Collated,
        Variant::StarRocks | Variant::Doris | Variant::GreptimeDb => Dialect::Bytes,
        _ => return None,
    };
    let mut sources = vec![RELATIONS];
    if v == Variant::StarRocks {
        sources = vec![
            Source {
                base: STARROCKS_MVS,
                ..RELATIONS
            },
            MATERIALIZED_VIEWS,
        ];
    }
    if v.has_routines() {
        sources.push(ROUTINES);
    }
    if v.has_triggers() {
        sources.push(TRIGGERS);
    }
    if v.has_sequences() {
        sources.push(if v == Variant::OceanBase {
            OCEANBASE_SEQUENCES
        } else {
            SEQUENCES
        });
    }
    Some(Catalog { dialect, sources })
}

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

/// The query for page `q` of database `db`, or `None` when no object can
/// match (a schema asked for: these engines have none here, or no kind
/// left).
pub(crate) fn page_query(catalog: &Catalog, db: &str, q: &ObjectQuery) -> Option<PageQuery> {
    if !q.no_schema && q.schema.is_some() {
        return None;
    }
    let collated = catalog.dialect == Dialect::Collated;
    let lower = |x: &str| {
        if collated {
            format!("CAST(LOWER({x}) AS BINARY)")
        } else {
            format!("LOWER({x})")
        }
    };
    let mut params = Vec::new();
    let mut branches = Vec::new();
    for s in &catalog.sources {
        let picked: Vec<&str> = s
            .kinds
            .iter()
            .filter(|(k, _)| wants(q, k))
            .map(|(_, c)| *c)
            .collect();
        if picked.is_empty() {
            continue;
        }
        let kind = match s.kinds {
            [(k, _)] => format!("'{k}'"),
            many => format!(
                "CASE {} END",
                many.iter()
                    .map(|(k, c)| format!("WHEN {c} THEN '{k}'"))
                    .collect::<Vec<_>>()
                    .join(" ")
            ),
        };
        let mut conds = vec![format!("{} = ?", s.db)];
        params.push(db.to_string());
        if !s.base.is_empty() {
            conds.push(s.base.to_string());
            params.extend(s.base.matches('?').map(|_| db.to_string()));
        }
        if !picked.contains(&"") {
            conds.push(format!(
                "({})",
                picked
                    .iter()
                    .map(|c| format!("({c})"))
                    .collect::<Vec<_>>()
                    .join(" OR ")
            ));
        }
        if let Some(n) = &q.name {
            conds.push(format!("{} = {}", lower(s.name), lower("?")));
            params.push(n.clone());
        }
        if let Some(p) = &q.pattern {
            if collated {
                conds.push(format!("{} LIKE {} ESCAPE '!'", lower(s.name), lower("?")));
                params.push(contains_pattern(p));
            } else {
                conds.push(format!("LOCATE({}, {}) > 0", lower("?"), lower(s.name)));
                params.push(p.clone());
            }
        }
        let name = if collated {
            format!("CAST({} AS BINARY)", s.name)
        } else {
            s.name.to_string()
        };
        branches.push(format!(
            "SELECT {kind} AS k, {} AS l, {name} AS n, {} AS p, {} AS r FROM {} WHERE {}",
            lower(s.name),
            s.parent,
            s.rank,
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
        "SELECT k, n, p FROM ({}) o ORDER BY l, n, r, k LIMIT {} OFFSET {offset}",
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
    pub(crate) fn page(&self, rows: &[Row]) -> ObjectPage {
        let mut objects: Vec<DbObject> = rows
            .iter()
            .filter_map(|r| {
                Some(DbObject {
                    kind: at(r, 0)?,
                    schema: None,
                    name: at(r, 1)?,
                    parent: at(r, 2),
                })
            })
            .collect();
        let more = rows.len() > self.size;
        objects.truncate(self.size);
        ObjectPage {
            objects,
            next: more.then(|| (self.offset + self.size).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn query(v: Variant, q: &ObjectQuery) -> Option<PageQuery> {
        page_query(&catalog(v).unwrap(), "shop", q)
    }

    #[test]
    fn engines_without_a_pageable_catalog_keep_the_default() {
        for v in [Variant::Manticore, Variant::Databend] {
            assert!(catalog(v).is_none(), "{v:?}");
        }
        for v in [
            Variant::MySql,
            Variant::MariaDb,
            Variant::AuroraMySql,
            Variant::CloudSqlMySql,
            Variant::TiDb,
            Variant::OceanBase,
            Variant::SingleStore,
            Variant::StarRocks,
            Variant::Doris,
            Variant::VeloDb,
            Variant::GreptimeDb,
        ] {
            assert!(catalog(v.base()).is_some(), "{v:?}");
        }
    }

    #[test]
    fn byte_strings_search_without_wildcards() {
        let q = ObjectQuery {
            pattern: Some("50%_off".into()),
            name: Some("Orders".into()),
            ..Default::default()
        };
        let p = query(Variant::GreptimeDb, &q).unwrap();
        assert!(
            !p.sql.contains("BINARY") && !p.sql.contains("LIKE ?"),
            "{}",
            p.sql
        );
        assert!(p.sql.contains("LOCATE(LOWER(?), LOWER(TABLE_NAME)) > 0"));
        assert!(p.sql.contains("LOWER(TABLE_NAME) = LOWER(?)"));
        assert_eq!(p.params, ["shop", "Orders", "50%_off"]);
        // StarRocks: its materialized views out of the relations, in their
        // own source.
        let p = query(Variant::StarRocks, &ObjectQuery::default()).unwrap();
        assert!(p.sql.contains("TABLE_NAME NOT IN (SELECT TABLE_NAME FROM information_schema.materialized_views WHERE TABLE_SCHEMA = ?"));
        assert!(p.sql.contains("SELECT 'materialized_view' AS k"));
        assert_eq!(p.params, ["shop", "shop", "shop"]);
        let q = ObjectQuery {
            kind: Some("materialized_view".into()),
            ..Default::default()
        };
        let p = query(Variant::StarRocks, &q).unwrap();
        assert_eq!(p.params, ["shop"]);
        assert!(!p.sql.contains("UNION ALL"));
    }

    #[test]
    fn a_folder_reads_only_its_catalog() {
        let q = ObjectQuery {
            no_schema: true,
            kind: Some("view".into()),
            limit: 10,
            ..Default::default()
        };
        let p = query(Variant::MySql, &q).unwrap();
        assert_eq!(p.params, ["shop"]);
        assert!(p.sql.contains("information_schema.TABLES"), "{}", p.sql);
        assert!(!p.sql.contains("ROUTINES") && !p.sql.contains("TRIGGERS"));
        assert!(
            p.sql
                .contains("((UPPER(COALESCE(TABLE_TYPE, '')) = 'VIEW'))"),
            "{}",
            p.sql
        );
        assert!(p.sql.ends_with("ORDER BY l, n, r, k LIMIT 11 OFFSET 0"));

        // Triggers: their own catalog, with the table they belong to.
        let q = ObjectQuery {
            kind: Some("trigger".into()),
            after: Some("20".into()),
            limit: 10,
            ..Default::default()
        };
        let p = query(Variant::MySql, &q).unwrap();
        assert!(p
            .sql
            .contains("FROM information_schema.TRIGGERS WHERE TRIGGER_SCHEMA = ?"));
        assert!(p.sql.contains("EVENT_OBJECT_TABLE AS p"));
        assert!(!p.sql.contains("information_schema.TABLES"));
        assert!(p.sql.ends_with("LIMIT 11 OFFSET 20"));
    }

    #[test]
    fn user_text_is_bound_and_wildcards_escaped() {
        let q = ObjectQuery {
            pattern: Some("50%_off!'".into()),
            name: Some("x' OR 1=1 --".into()),
            ..Default::default()
        };
        let p = query(Variant::MariaDb, &q).unwrap();
        assert!(
            !p.sql.contains("50%") && !p.sql.contains("OR 1=1"),
            "{}",
            p.sql
        );
        // Four sources (relations, routines, triggers, sequences), each
        // with the database, the name and the pattern.
        assert_eq!(p.sql.matches(" UNION ALL ").count(), 3);
        assert_eq!(p.params.len(), 12);
        assert_eq!(p.params[1], "x' OR 1=1 --");
        assert_eq!(p.params[2], "%50!%!_off!!'%");
        assert!(p.sql.contains(
            "CAST(LOWER(TABLE_NAME) AS BINARY) LIKE CAST(LOWER(?) AS BINARY) ESCAPE '!'"
        ));
        assert!(p
            .sql
            .contains("CAST(LOWER(ROUTINE_NAME) AS BINARY) = CAST(LOWER(?) AS BINARY)"));
        assert!(p.sql.ends_with("LIMIT 1001 OFFSET 0"), "the default page");
    }

    #[test]
    fn kinds_left_out_and_schemas_asked_for() {
        // "Otros" of a driver that declares every kind it returns: nothing.
        let q = ObjectQuery {
            not_kinds: ["table", "view", "procedure", "function", "trigger"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        };
        assert!(query(Variant::MySql, &q).is_none());
        // MariaDB's sequences still count as "Otros" there.
        let p = query(Variant::MariaDb, &q).unwrap();
        assert!(p.sql.contains("TABLE_TYPE = 'SEQUENCE'"));
        assert_eq!(p.sql.matches(" UNION ALL ").count(), 0);
        // Functions without procedures: the routine's type picks them.
        let q = ObjectQuery {
            not_kinds: vec!["procedure".into()],
            kind: Some("function".into()),
            ..Default::default()
        };
        let p = query(Variant::SingleStore, &q).unwrap();
        assert!(
            p.sql.contains("((UPPER(ROUTINE_TYPE) <> 'PROCEDURE'))"),
            "{}",
            p.sql
        );
        // No object here has a schema.
        let q = ObjectQuery {
            schema: Some("shop".into()),
            ..Default::default()
        };
        assert!(query(Variant::MySql, &q).is_none());
        let q = ObjectQuery {
            schema: Some("shop".into()),
            no_schema: true,
            kind: Some("sequence".into()),
            ..Default::default()
        };
        let p = query(Variant::OceanBase, &q).unwrap();
        assert!(p
            .sql
            .contains("oceanbase.DBA_OB_SEQUENCE_OBJECTS WHERE DATABASE_NAME = ?"));
    }

    #[test]
    fn a_bad_cursor_starts_over() {
        let q = ObjectQuery {
            after: Some("x".into()),
            limit: 7,
            ..Default::default()
        };
        let p = query(Variant::TiDb, &q).unwrap();
        assert!(p.sql.ends_with("LIMIT 8 OFFSET 0"));
        assert_eq!(p.page(&[]), ObjectPage::default());
    }
}
