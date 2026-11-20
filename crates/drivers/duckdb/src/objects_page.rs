//! The explorer's pages (`Session::list_objects_page`): a folder of the
//! tree, a name search or a lookup as one query on DuckDB's catalog
//! functions (`duckdb_tables()`, `duckdb_views()`…), narrowed to the schema,
//! kinds and names asked for, sorted as the tree shows them (schema, then
//! name, ignoring case, then as written) and cut to the page with
//! `LIMIT … OFFSET` (the cursor is the offset, as `objects::page_of`'s). The
//! database, schema, names and patterns go as bound values; the SQL holds
//! only catalog text and numbers.

use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery};

/// A catalog function of objects of one kind, as `list_objects` reads it.
struct Source {
    kind: &'static str,
    /// `SELECT DISTINCT` (a macro's overloads are one object).
    distinct: bool,
    from: &'static str,
    name: &'static str,
    /// What always holds, beside the database (`""`: nothing).
    base: &'static str,
}

/// In `list_objects`' order (which only breaks ties of a schema and name).
const SOURCES: &[Source] = &[
    Source {
        kind: kinds::TABLE,
        distinct: false,
        from: "duckdb_tables()",
        name: "table_name",
        base: "NOT internal",
    },
    Source {
        kind: kinds::VIEW,
        distinct: false,
        from: "duckdb_views()",
        name: "view_name",
        base: "NOT internal",
    },
    Source {
        kind: kinds::FUNCTION,
        distinct: true,
        from: "duckdb_functions()",
        name: "function_name",
        base: "NOT internal AND function_type IN ('macro', 'table_macro')",
    },
    Source {
        kind: kinds::SEQUENCE,
        distinct: false,
        from: "duckdb_sequences()",
        name: "sequence_name",
        base: "",
    },
    Source {
        kind: kinds::TYPE,
        distinct: false,
        from: "duckdb_types()",
        name: "type_name",
        base: "NOT internal",
    },
];

/// The whole list, as `list_objects` reads it (`?1`: the database), by
/// schema, name and kind.
pub fn list_sql() -> String {
    let branches: Vec<String> = SOURCES
        .iter()
        .map(|s| branch(s, &["database_name = ?1".to_string()]))
        .collect();
    format!("{} ORDER BY 2, 3, 1", branches.join(" UNION ALL "))
}

/// `s`'s `SELECT` of (kind, schema, name) where `conds` hold.
fn branch(s: &Source, conds: &[String]) -> String {
    let mut conds = conds.to_vec();
    if !s.base.is_empty() {
        conds.push(s.base.to_string());
    }
    format!(
        "SELECT {}'{}' AS k, schema_name AS s, {} AS n FROM {} WHERE {}",
        if s.distinct { "DISTINCT " } else { "" },
        s.kind,
        s.name,
        s.from,
        conds.join(" AND ")
    )
}

/// Whether `q` keeps objects of `kind`.
fn wants(q: &ObjectQuery, kind: &str) -> bool {
    q.kind.as_deref().is_none_or(|k| k == kind) && !q.not_kinds.iter().any(|k| k == kind)
}

/// A page's query: the SQL, its bound values in order (`?1` the database),
/// and the slice of the sorted list it asks for.
#[derive(Debug)]
pub struct PageQuery {
    pub sql: String,
    pub params: Vec<String>,
    offset: usize,
    size: usize,
}

/// The query for page `q` of database `db`, or `None` when no object can
/// match (objects without a schema: every one has one here, or no kind
/// left).
pub fn page_query(db: &str, q: &ObjectQuery) -> Option<PageQuery> {
    if q.no_schema {
        return None;
    }
    let mut params = vec![db.to_string()];
    let mut bind = |v: &str| {
        params.push(v.to_string());
        format!("?{}", params.len())
    };
    let mut conds = vec!["database_name = ?1".to_string()];
    if let Some(s) = &q.schema {
        conds.push(format!("schema_name = {}", bind(s)));
    }
    // `__N__` stands for each source's name column.
    if let Some(n) = &q.name {
        conds.push(format!("lower(__N__) = lower({})", bind(n)));
    }
    if let Some(p) = &q.pattern {
        conds.push(format!("contains(lower(__N__), lower({}))", bind(p)));
    }
    let branches: Vec<String> = SOURCES
        .iter()
        .filter(|s| wants(q, s.kind))
        .map(|s| {
            let conds: Vec<String> = conds.iter().map(|c| c.replace("__N__", s.name)).collect();
            branch(s, &conds)
        })
        .collect();
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
        "SELECT k, s, n FROM ({}) o ORDER BY lower(s), lower(n), s, n, k LIMIT {} OFFSET {offset}",
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

/// A row of (kind, schema, name) as the explorer's object.
pub fn object(r: Vec<Option<String>>) -> DbObject {
    let mut r = r.into_iter();
    DbObject {
        kind: r.next().flatten().unwrap_or_default(),
        schema: r.next().flatten(),
        name: r.next().flatten().unwrap_or_default(),
        parent: None,
    }
}

impl PageQuery {
    /// The page out of the query's rows (one more than the page when more
    /// remain).
    pub fn page(&self, rows: Vec<Vec<Option<String>>>) -> ObjectPage {
        let more = rows.len() > self.size;
        ObjectPage {
            objects: rows.into_iter().take(self.size).map(object).collect(),
            next: more.then(|| (self.offset + self.size).to_string()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_folder_reads_only_its_catalog_function() {
        let q = ObjectQuery {
            schema: Some("main".into()),
            kind: Some("view".into()),
            limit: 10,
            ..Default::default()
        };
        let p = page_query("shop", &q).unwrap();
        assert_eq!(p.params, ["shop", "main"]);
        assert_eq!(
            p.sql,
            "SELECT k, s, n FROM (SELECT 'view' AS k, schema_name AS s, view_name AS n FROM duckdb_views() \
             WHERE database_name = ?1 AND schema_name = ?2 AND NOT internal) o \
             ORDER BY lower(s), lower(n), s, n, k LIMIT 11 OFFSET 0"
        );
        // Macros: one per name, whatever its overloads.
        let q = ObjectQuery {
            kind: Some("function".into()),
            after: Some("20".into()),
            limit: 10,
            ..Default::default()
        };
        let p = page_query("shop", &q).unwrap();
        assert!(
            p.sql.contains("SELECT DISTINCT 'function' AS k"),
            "{}",
            p.sql
        );
        assert!(p.sql.contains("function_type IN ('macro', 'table_macro')"));
        assert!(!p.sql.contains("UNION ALL"));
        assert!(p.sql.ends_with("LIMIT 11 OFFSET 20"));
    }

    #[test]
    fn user_text_is_bound() {
        let q = ObjectQuery {
            schema: Some("s' OR 1=1 --".into()),
            pattern: Some("50%_off'".into()),
            name: Some("x'y".into()),
            ..Default::default()
        };
        let p = page_query("db'x", &q).unwrap();
        assert!(
            !p.sql.contains("50%") && !p.sql.contains("OR 1=1") && !p.sql.contains("x'y"),
            "{}",
            p.sql
        );
        assert_eq!(p.params, ["db'x", "s' OR 1=1 --", "x'y", "50%_off'"]);
        // Five sources, each with the same bound values.
        assert_eq!(p.sql.matches(" UNION ALL ").count(), 4);
        assert_eq!(p.sql.matches("schema_name = ?2").count(), 5);
        assert!(p.sql.contains("lower(table_name) = lower(?3)"));
        assert!(p.sql.contains("contains(lower(sequence_name), lower(?4))"));
        assert!(p.sql.ends_with("LIMIT 1001 OFFSET 0"), "the default page");
    }

    #[test]
    fn kinds_left_out_and_objects_without_a_schema() {
        let q = ObjectQuery {
            not_kinds: ["table", "view", "function", "sequence", "type"]
                .map(String::from)
                .to_vec(),
            ..Default::default()
        };
        assert!(page_query("db", &q).is_none());
        let q = ObjectQuery {
            not_kinds: vec!["table".into(), "type".into()],
            ..Default::default()
        };
        let p = page_query("db", &q).unwrap();
        assert_eq!(p.sql.matches(" UNION ALL ").count(), 2, "{}", p.sql);
        assert!(!p.sql.contains("duckdb_tables") && !p.sql.contains("duckdb_types"));
        let q = ObjectQuery {
            no_schema: true,
            schema: Some("main".into()),
            ..Default::default()
        };
        assert!(page_query("db", &q).is_none());
    }

    #[test]
    fn the_whole_list_breaks_ties_by_kind() {
        let sql = list_sql();
        assert!(sql.ends_with("ORDER BY 2, 3, 1"));
        assert_eq!(sql.matches("database_name = ?1").count(), 5);
        assert!(sql.contains("SELECT 'sequence' AS k, schema_name AS s, sequence_name AS n FROM duckdb_sequences() WHERE database_name = ?1 UNION ALL"));
    }

    #[test]
    fn a_bad_cursor_starts_over_and_more_rows_give_the_next() {
        let q = ObjectQuery {
            after: Some("x".into()),
            limit: 2,
            ..Default::default()
        };
        let p = page_query("db", &q).unwrap();
        assert!(p.sql.ends_with("LIMIT 3 OFFSET 0"));
        assert_eq!(p.page(Vec::new()), ObjectPage::default());
        let row = |n: &str| vec![Some("table".into()), Some("main".into()), Some(n.into())];
        let page = p.page(vec![row("a"), row("b"), row("c")]);
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[1].schema.as_deref(), Some("main"));
        assert_eq!(page.next.as_deref(), Some("2"));
    }
}
