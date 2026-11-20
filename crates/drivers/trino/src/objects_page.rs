//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`],
//! docs/drivers.md, "Objects by page"): what `list_objects` reads, narrowed
//! to the page on the server, in one query.
//!
//! - **Source:** the catalog's `information_schema.tables` without
//!   `information_schema`, with `system.metadata.materialized_views` joined
//!   for the materialized views (which show there as tables). A server
//!   without that table (Presto) has none, as in `list_objects`.
//! - **Order:** schema, then name, both ignoring case, then as written:
//!   `ORDER BY lower(s), lower(n), s, n` (a schema and name are unique in
//!   the catalog).
//! - **Cursor:** the page's last object, `[schema, name]` (keyset): the next
//!   page asks for what sorts after it, so pages neither skip nor repeat
//!   objects even when the catalog changes in between.
//! - **Page:** the query asks for one object more than the page, which
//!   tells whether more remain.
//!
//! Values from the query or the cursor go in as string literals (quotes
//! doubled: Trino's strings have no other escape), the catalog as a quoted
//! identifier.

use crate::{lit, TrinoSession};
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
const MAX_PAGE: usize = 10_000;

/// The kinds `list_objects` gives.
const KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW, kinds::MATERIALIZED_VIEW];

/// The objects a page holds.
fn size(q: &ObjectQuery) -> usize {
    q.page_size().min(MAX_PAGE)
}

/// The previous page's last object: its schema and name.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    schema: String,
    name: String,
}

impl Cursor {
    /// `None` for a cursor this driver didn't write.
    fn parse(s: &str) -> Option<Cursor> {
        let (schema, name) = serde_json::from_str(s).ok()?;
        Some(Cursor { schema, name })
    }

    fn write(&self) -> String {
        serde_json::json!([self.schema, self.name]).to_string()
    }
}

/// The tree's order ([`dbine_driver::objects::page_of`]'s).
type Key = (String, String, String, String);

fn key(schema: &str, name: &str) -> Key {
    (
        schema.to_lowercase(),
        name.to_lowercase(),
        schema.to_string(),
        name.to_string(),
    )
}

/// What the app takes as "page the whole list yourself" (the contract's
/// default).
fn unsupported() -> Error {
    Error::Unsupported("este motor todavía no lista sus objetos por páginas".into())
}

/// The kinds among [`KINDS`] the query admits.
fn wanted(q: &ObjectQuery) -> Vec<&'static str> {
    KINDS
        .iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// `(c1 > v1 OR (c1 = v1 AND (c2 > v2 OR (… AND cN > vN))))`.
fn after_sql(keys: &[(String, String)]) -> String {
    match keys {
        [] => "TRUE".into(),
        [(c, v)] => format!("{c} > {v}"),
        [(c, v), rest @ ..] => format!("({c} > {v} OR ({c} = {v} AND {}))", after_sql(rest)),
    }
}

/// The page's query, `None` when nothing can match it. Rows `s, n, k`.
/// `mvs`: join the materialized views.
fn page_sql(catalog: &str, mvs: bool, q: &ObjectQuery, after: Option<&Cursor>) -> Option<String> {
    // Every object has a schema.
    if q.no_schema {
        return None;
    }
    let kinds = wanted(q);
    if kinds.is_empty() {
        return None;
    }
    let mut filters = vec!["t.table_schema <> 'information_schema'".to_string()];
    let mut mv_filters = vec![format!("catalog_name = {}", lit(catalog))];
    if let Some(s) = &q.schema {
        filters.push(format!("t.table_schema = {}", lit(s)));
        mv_filters.push(format!("schema_name = {}", lit(s)));
    }
    if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
        filters.push(format!(
            "strpos(lower(t.table_name), lower({})) > 0",
            lit(p)
        ));
    }
    if let Some(n) = &q.name {
        filters.push(format!("lower(t.table_name) = lower({})", lit(n)));
    }
    let kind = if mvs {
        format!(
            "CASE WHEN m.name IS NOT NULL THEN '{}' WHEN t.table_type = 'VIEW' THEN '{}' ELSE '{}' END",
            kinds::MATERIALIZED_VIEW,
            kinds::VIEW,
            kinds::TABLE
        )
    } else {
        format!(
            "CASE WHEN t.table_type = 'VIEW' THEN '{}' ELSE '{}' END",
            kinds::VIEW,
            kinds::TABLE
        )
    };
    let join = if mvs {
        format!(
            " LEFT JOIN (SELECT schema_name, name FROM system.metadata.materialized_views WHERE {}) m \
             ON m.schema_name = t.table_schema AND m.name = t.table_name",
            mv_filters.join(" AND ")
        )
    } else {
        String::new()
    };
    let mut outer = Vec::new();
    if kinds.len() < KINDS.len() {
        let list: Vec<String> = kinds.iter().map(|k| format!("'{k}'")).collect();
        outer.push(format!("k IN ({})", list.join(", ")));
    }
    if let Some(a) = after {
        let (s, n) = (lit(&a.schema), lit(&a.name));
        outer.push(after_sql(&[
            ("lower(s)".into(), format!("lower({s})")),
            ("lower(n)".into(), format!("lower({n})")),
            ("s".into(), s),
            ("n".into(), n),
        ]));
    }
    Some(format!(
        "SELECT s, n, k FROM (SELECT t.table_schema AS s, t.table_name AS n, {kind} AS k \
         FROM {}.information_schema.tables t{join} WHERE {}) x{} \
         ORDER BY lower(s), lower(n), s, n LIMIT {}",
        quote_ident(Quote::Double, catalog),
        filters.join(" AND "),
        if outer.is_empty() {
            String::new()
        } else {
            format!(" WHERE {}", outer.join(" AND "))
        },
        size(q) + 1
    ))
}

/// The page out of the rows (at most one more than the page): sorted, past
/// the cursor, cut at the page; the cursor is the last object kept. What's
/// shown is checked against the query once more (the server's lowercase
/// may differ from Rust's in rare letters).
fn merge(q: &ObjectQuery, after: Option<&Cursor>, found: Vec<DbObject>) -> ObjectPage {
    let from = after.map(|a| key(&a.schema, &a.name));
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .map(|o| (key(o.schema.as_deref().unwrap_or(""), &o.name), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = size(q);
    let next = (found.len() > size).then(|| {
        let (_, _, schema, name) = found[size - 1].0.clone();
        Cursor { schema, name }.write()
    });
    found.truncate(size);
    ObjectPage {
        objects: found
            .into_iter()
            .map(|(_, o)| o)
            .filter(|o| q.matches(o))
            .collect(),
        next,
    }
}

impl TrinoSession {
    /// A page of the catalog's objects (see the module's doc).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let after = match q.after.as_deref().map(Cursor::parse) {
            None => None,
            Some(Some(c)) => Some(c),
            // Not ours: the app's own position in the whole list, after an
            // earlier page fell back to it. It goes on paging that list.
            Some(None) => return Err(unsupported()),
        };
        let cat = self.catalog()?;
        let mvs = self.mv_catalog != Some(false);
        let Some(sql) = page_sql(&cat, mvs, q, after.as_ref()) else {
            return Ok(ObjectPage::default());
        };
        let rows = match self.strings(&sql).await {
            Ok(rows) => {
                if mvs {
                    self.mv_catalog = Some(true);
                }
                rows
            }
            // As `list_objects`: a server that can't list the materialized
            // views (Presto) has none.
            Err(Error::Query(e)) if mvs && self.mv_catalog.is_none() => {
                tracing::debug!("{:?}: no materialized views to join: {e}", self.flavor);
                self.mv_catalog = Some(false);
                let sql = page_sql(&cat, false, q, after.as_ref()).unwrap_or_default();
                self.strings(&sql).await?
            }
            Err(e) => return Err(e),
        };
        let found = rows
            .into_iter()
            .filter(|r| r.len() == 3)
            .map(|mut r| DbObject {
                kind: r.pop().unwrap_or_default(),
                name: r.pop().unwrap_or_default(),
                schema: r.pop(),
                parent: None,
            })
            .collect();
        Ok(merge(q, after.as_ref(), found))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: Some(schema.into()),
            name: name.into(),
            parent: None,
        }
    }

    /// A schema's folder of one kind: the schema narrows both sides of the
    /// join, the kind is filtered on the joined value, quotes are doubled
    /// and the page asks one more than its size.
    #[test]
    fn a_folder_query() {
        let q = ObjectQuery {
            schema: Some("ven'tas".into()),
            kind: Some(kinds::TABLE.into()),
            pattern: Some("o'r".into()),
            limit: 7,
            ..Default::default()
        };
        let after = Cursor {
            schema: "ven'tas".into(),
            name: "b".into(),
        };
        let sql = page_sql("my\"cat", true, &q, Some(&after)).unwrap();
        assert!(
            sql.contains("FROM \"my\"\"cat\".information_schema.tables t LEFT JOIN"),
            "{sql}"
        );
        assert!(
            sql.contains("WHERE catalog_name = 'my\"cat' AND schema_name = 'ven''tas') m"),
            "{sql}"
        );
        assert!(
            sql.contains("AND t.table_schema = 'ven''tas' AND strpos(lower(t.table_name), lower('o''r')) > 0"),
            "{sql}"
        );
        assert!(sql.contains(") x WHERE k IN ('table') AND (lower(s) > lower('ven''tas') OR (lower(s) = lower('ven''tas') AND (lower(n) > lower('b') OR (lower(n) = lower('b') AND (s > 'ven''tas' OR (s = 'ven''tas' AND n > 'b'))))))"), "{sql}");
        assert!(
            sql.ends_with("ORDER BY lower(s), lower(n), s, n LIMIT 8"),
            "{sql}"
        );
        // Without the materialized views' table.
        let sql = page_sql("c", false, &q, None).unwrap();
        assert!(!sql.contains("materialized_views"), "{sql}");
        assert!(
            sql.contains("CASE WHEN t.table_type = 'VIEW' THEN 'view' ELSE 'table' END"),
            "{sql}"
        );
    }

    #[test]
    fn nothing_without_a_schema_or_a_kind() {
        let q = ObjectQuery {
            no_schema: true,
            ..Default::default()
        };
        assert!(page_sql("c", true, &q, None).is_none());
        let q = ObjectQuery {
            not_kinds: KINDS.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        };
        assert!(page_sql("c", true, &q, None).is_none());
        let q = ObjectQuery {
            kind: Some(kinds::FUNCTION.into()),
            ..Default::default()
        };
        assert!(page_sql("c", true, &q, None).is_none());
        // Every kind: no kind condition.
        let sql = page_sql("c", true, &ObjectQuery::default(), None).unwrap();
        assert!(!sql.contains("k IN"), "{sql}");
        assert!(sql.contains(") x ORDER BY"), "{sql}");
    }

    #[test]
    fn pages_merge_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            obj(kinds::TABLE, "b", "x"),
            obj(kinds::VIEW, "A", "s"),
            obj(kinds::TABLE, "a", "S"),
        ];
        let p = merge(&q, None, found.clone());
        let got: Vec<_> = p.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got, ["s", "S"]);
        let after = Cursor::parse(p.next.as_deref().unwrap()).unwrap();
        assert_eq!(after.schema, "a");
        let p2 = merge(&q, Some(&after), found);
        assert_eq!(p2.objects.len(), 1);
        assert_eq!(p2.next, None);
        assert_eq!(Cursor::parse("1000"), None);
    }
}
