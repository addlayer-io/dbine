//! The explorer's pages (`Session::list_objects_page`): a folder of the
//! tree, a name search or a lookup as one dictionary query over the
//! objects `list_objects` reads (the schema's own, then the public synonyms
//! of them under `PUBLIC`), narrowed to the kinds and names asked for,
//! sorted as the tree shows them and cut to the page with `ROWNUM` (every
//! version since 11g; the cursor is the offset, as `objects::page_of`'s).
//! The owner, names and patterns are bound; the SQL holds only dictionary
//! text and numbers. Names sort by their bytes (`NLSSORT … BINARY`),
//! whatever the session's NLS_SORT.

use crate::{object_kind, PACKAGE};
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery};

/// Each kind `list_objects` returns and its `OBJECT_TYPE`.
const TYPES: &[(&str, &str)] = &[
    (kinds::TABLE, "TABLE"),
    (kinds::VIEW, "VIEW"),
    (kinds::MATERIALIZED_VIEW, "MATERIALIZED VIEW"),
    (kinds::PROCEDURE, "PROCEDURE"),
    (kinds::FUNCTION, "FUNCTION"),
    (PACKAGE, "PACKAGE"),
    (kinds::TRIGGER, "TRIGGER"),
    (kinds::SEQUENCE, "SEQUENCE"),
    (kinds::SYNONYM, "SYNONYM"),
    (kinds::TYPE, "TYPE"),
];

/// What `LIST_OBJECTS` leaves out of `all_objects`: generated and
/// secondary objects, the recycle bin and vector indexes' internal tables.
const OWN_OBJECTS: &str =
    "o.generated = 'N' AND o.secondary = 'N' AND o.object_name NOT LIKE 'BIN$%' \
     AND NOT REGEXP_LIKE(o.object_name, '^VECTOR\\$.+\\$[0-9]+_[0-9]+_[0-9]+\\$')";

/// A materialized view's container table is listed as the view only.
const NOT_MVIEW_TABLES: &str = "NOT (o.object_type = 'TABLE' AND EXISTS ( \
     SELECT 1 FROM all_mviews m WHERE m.owner = o.owner AND m.mview_name = o.object_name))";

/// A trigger's table, its parent.
const TRIGGER_TABLE: &str = " LEFT JOIN all_triggers t \
     ON o.object_type = 'TRIGGER' AND t.owner = o.owner AND t.trigger_name = o.object_name";

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

/// A page's query: the SQL, its bound values (`:1`, `:2`… in order), and
/// the slice of the sorted list it asks for.
#[derive(Debug)]
pub(crate) struct PageQuery {
    pub sql: String,
    pub params: Vec<String>,
    offset: usize,
    size: usize,
}

/// Binds in order of appearance.
struct Binds(Vec<String>);

impl Binds {
    fn bind(&mut self, v: &str) -> String {
        self.0.push(v.to_string());
        format!(":{}", self.0.len())
    }

    /// The name and pattern conditions on the name column `col`.
    fn names(&mut self, q: &ObjectQuery, col: &str, conds: &mut Vec<String>) {
        if let Some(n) = &q.name {
            let b = self.bind(n);
            conds.push(format!("LOWER({col}) = LOWER({b})"));
        }
        if let Some(p) = &q.pattern {
            let b = self.bind(&contains_pattern(p));
            conds.push(format!("LOWER({col}) LIKE LOWER({b}) ESCAPE '!'"));
        }
    }
}

/// The query for page `q` of schema `owner`, or `None` when no object can
/// match.
pub(crate) fn page_query(owner: &str, q: &ObjectQuery) -> Option<PageQuery> {
    let mut binds = Binds(Vec::new());
    let mut branches = Vec::new();
    // The schema's own objects: no schema in the tree.
    let types: Vec<&str> = TYPES
        .iter()
        .filter(|(k, _)| wants(q, k))
        .map(|(_, t)| *t)
        .collect();
    if (q.no_schema || q.schema.is_none()) && !types.is_empty() {
        let owner_bind = binds.bind(owner);
        let mut conds = vec![
            format!("o.owner = {owner_bind}"),
            format!(
                "o.object_type IN ({})",
                types
                    .iter()
                    .map(|t| format!("'{t}'"))
                    .collect::<Vec<_>>()
                    .join(", ")
            ),
            OWN_OBJECTS.to_string(),
        ];
        if types.contains(&"TABLE") {
            conds.push(NOT_MVIEW_TABLES.to_string());
        }
        binds.names(q, "o.object_name", &mut conds);
        let (parent, join) = if types.contains(&"TRIGGER") {
            ("t.table_name", TRIGGER_TABLE)
        } else {
            ("CAST(NULL AS VARCHAR2(128))", "")
        };
        branches.push(format!(
            "SELECT o.object_type AS k, CAST(NULL AS VARCHAR2(128)) AS s, o.object_name AS n, \
             {parent} AS p, 0 AS sr FROM all_objects o{join} WHERE {}",
            conds.join(" AND ")
        ));
    }
    // Public synonyms of them, under PUBLIC.
    let public = !q.no_schema && q.schema.as_deref().is_none_or(|s| s == "PUBLIC");
    if public && wants(q, kinds::SYNONYM) {
        let owner_bind = binds.bind(owner);
        let mut conds = vec![
            "owner = 'PUBLIC'".to_string(),
            format!("table_owner = {owner_bind}"),
        ];
        binds.names(q, "synonym_name", &mut conds);
        branches.push(format!(
            "SELECT 'SYNONYM' AS k, 'PUBLIC' AS s, synonym_name AS n, \
             CAST(NULL AS VARCHAR2(128)) AS p, 1 AS sr FROM all_synonyms WHERE {}",
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
        "SELECT k, s, n, p FROM (\
         SELECT k, s, n, p, ROWNUM AS rn FROM (\
         SELECT k, s, n, p FROM ({}) \
         ORDER BY sr, NLSSORT(LOWER(n), 'NLS_SORT=BINARY'), NLSSORT(n, 'NLS_SORT=BINARY'), k\
         ) WHERE ROWNUM <= {}) WHERE rn > {offset} ORDER BY rn",
        branches.join(" UNION ALL "),
        offset.saturating_add(size).saturating_add(1)
    );
    Some(PageQuery {
        sql,
        params: binds.0,
        offset,
        size,
    })
}

impl PageQuery {
    /// The page out of the query's rows (`OBJECT_TYPE`, schema, name,
    /// parent), one more than the page when more remain.
    pub(crate) fn page(&self, rows: Vec<[Option<String>; 4]>) -> ObjectPage {
        let more = rows.len() > self.size;
        let objects = rows
            .into_iter()
            .take(self.size)
            .filter_map(|[k, s, n, p]| {
                Some(DbObject {
                    kind: object_kind(&k?)?.to_string(),
                    schema: s,
                    name: n?,
                    parent: p,
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

    /// The same objects as `list_objects`: its query holds each filter.
    #[test]
    fn filters_are_list_objects_own() {
        let flat = |s: &str| s.split_whitespace().collect::<Vec<_>>().join(" ");
        let list = flat(crate::LIST_OBJECTS);
        for f in [OWN_OBJECTS, NOT_MVIEW_TABLES, TRIGGER_TABLE] {
            assert!(list.contains(&flat(f)), "{f}\n{list}");
        }
        for (k, t) in TYPES {
            assert!(list.contains(&format!("'{t}'")), "{t}");
            assert_eq!(object_kind(t), Some(*k));
        }
    }

    #[test]
    fn a_folder_reads_its_kind_only() {
        let q = ObjectQuery {
            no_schema: true,
            kind: Some("view".into()),
            limit: 10,
            after: Some("30".into()),
            ..Default::default()
        };
        let p = page_query("HR", &q).unwrap();
        assert_eq!(p.params, ["HR"]);
        assert!(p.sql.contains("o.object_type IN ('VIEW')"), "{}", p.sql);
        assert!(!p.sql.contains("all_synonyms") && !p.sql.contains("all_triggers"));
        assert!(!p.sql.contains("all_mviews"), "only tables need it");
        assert!(p
            .sql
            .contains("WHERE ROWNUM <= 41) WHERE rn > 30 ORDER BY rn"));
        // Triggers come with their table.
        let q = ObjectQuery {
            kind: Some("trigger".into()),
            ..Default::default()
        };
        let p = page_query("HR", &q).unwrap();
        assert!(p.sql.contains("LEFT JOIN all_triggers t"), "{}", p.sql);
        assert!(p.sql.contains("ROWNUM <= 1001"), "the default page");
    }

    #[test]
    fn public_synonyms_sit_under_public() {
        let q = ObjectQuery {
            schema: Some("PUBLIC".into()),
            ..Default::default()
        };
        let p = page_query("HR", &q).unwrap();
        assert!(!p.sql.contains("all_objects"), "{}", p.sql);
        assert!(p
            .sql
            .contains("FROM all_synonyms WHERE owner = 'PUBLIC' AND table_owner = :1"));
        // Another schema, or a kind that isn't a synonym there: nothing.
        let other = ObjectQuery {
            schema: Some("SYS".into()),
            ..Default::default()
        };
        assert!(page_query("HR", &other).is_none());
        let tables = ObjectQuery {
            kind: Some("table".into()),
            ..q
        };
        assert!(page_query("HR", &tables).is_none());
        // Objects without a schema leave them out.
        let q = ObjectQuery {
            no_schema: true,
            kind: Some("synonym".into()),
            ..Default::default()
        };
        let p = page_query("HR", &q).unwrap();
        assert!(!p.sql.contains("all_synonyms"));
        assert!(p.sql.contains("o.object_type IN ('SYNONYM')"));
        // "Otros": none left.
        let q = ObjectQuery {
            not_kinds: TYPES.iter().map(|(k, _)| k.to_string()).collect(),
            ..Default::default()
        };
        assert!(page_query("HR", &q).is_none());
    }

    #[test]
    fn user_text_is_bound_and_wildcards_escaped() {
        let q = ObjectQuery {
            pattern: Some("A_1%!'".into()),
            name: Some("x' OR 1=1 --".into()),
            ..Default::default()
        };
        let p = page_query("HR", &q).unwrap();
        assert!(
            !p.sql.contains("A_1") && !p.sql.contains("1=1"),
            "{}",
            p.sql
        );
        assert_eq!(
            p.params,
            [
                "HR",
                "x' OR 1=1 --",
                "%A!_1!%!!'%",
                "HR",
                "x' OR 1=1 --",
                "%A!_1!%!!'%"
            ]
        );
        assert!(p.sql.contains("LOWER(o.object_name) = LOWER(:2)"));
        assert!(p
            .sql
            .contains("LOWER(o.object_name) LIKE LOWER(:3) ESCAPE '!'"));
        assert!(p
            .sql
            .contains("LOWER(synonym_name) LIKE LOWER(:6) ESCAPE '!'"));
    }

    #[test]
    fn rows_become_the_page() {
        let q = ObjectQuery {
            limit: 2,
            after: Some("4".into()),
            ..Default::default()
        };
        let p = page_query("HR", &q).unwrap();
        let row = |k: &str, s: Option<&str>, n: &str| {
            [Some(k.into()), s.map(Into::into), Some(n.into()), None]
        };
        let page = p.page(vec![
            row("MATERIALIZED VIEW", None, "MV"),
            row("PACKAGE", None, "PKG"),
            row("SYNONYM", Some("PUBLIC"), "S"),
        ]);
        assert_eq!(page.next.as_deref(), Some("6"));
        assert_eq!(page.objects.len(), 2);
        assert_eq!(page.objects[0].kind, "materialized_view");
        assert_eq!(page.objects[1].kind, PACKAGE);
        let last = p.page(vec![row("SYNONYM", Some("PUBLIC"), "S")]);
        assert_eq!(last.next, None);
        assert_eq!(last.objects[0].schema.as_deref(), Some("PUBLIC"));
    }
}
