//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`],
//! docs/drivers.md, "Objects by page").
//!
//! Flight SQL's own catalog command, `GetTables`, can't page, filter by kind
//! or match names ignoring case: a page out of it would mean reading the
//! whole list each time. What can is the SQL catalog of the engine behind,
//! which the protocol doesn't define. Only DuckDB's (GizmoSQL) is read here,
//! the `information_schema.tables` its `GetTables` answers from, so the
//! pages hold what `list_objects` lists. Another engine, or a DuckDB-named
//! server whose query fails (SQLite behind GizmoSQL), keeps the contract's
//! default: the app pages `list_objects`.
//!
//! - **Source:** the session catalog's tables and views (when none was
//!   chosen, every catalog's but GizmoSQL's own `_gizmosql_*`, as
//!   `GetTables` answers), without `information_schema`, `pg_catalog` and
//!   `sys`, as `list_objects`.
//! - **Order:** schema (none, `''`, first), then name, both ignoring case,
//!   then as written, then the catalog (two catalogs can share a schema and
//!   a name): `ORDER BY lower(s), lower(n), s, n, c`.
//! - **Cursor:** the page's last object, `[schema, name, catalog]`
//!   (keyset): the next page asks for what sorts after it, so pages neither
//!   skip nor repeat objects even when the catalog changes in between.
//! - **Page:** the query asks for one object more than the page, which
//!   tells whether more remain.
//!
//! Values from the query or the cursor go in as string literals (quotes
//! doubled: DuckDB's plain strings have no other escape).

use crate::{text, Engine, FlightSession};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
const MAX_PAGE: usize = 10_000;

/// The kinds `list_objects` gives.
const KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW];

/// The objects a page holds.
fn size(q: &ObjectQuery) -> usize {
    q.page_size().min(MAX_PAGE)
}

fn lit(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The previous page's last object: its schema, name and catalog.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    schema: Option<String>,
    name: String,
    catalog: String,
}

impl Cursor {
    /// `None` for a cursor this driver didn't write.
    fn parse(s: &str) -> Option<Cursor> {
        let (schema, name, catalog) = serde_json::from_str(s).ok()?;
        Some(Cursor {
            schema,
            name,
            catalog,
        })
    }

    fn write(&self) -> String {
        serde_json::json!([self.schema, self.name, self.catalog]).to_string()
    }
}

/// The tree's order ([`dbine_driver::objects::page_of`]'s), the catalog
/// last.
type Key = (bool, String, String, String, String, String);

fn key(schema: Option<&str>, name: &str, catalog: &str) -> Key {
    let s = schema.unwrap_or("");
    (
        schema.is_some(),
        s.to_lowercase(),
        name.to_lowercase(),
        s.to_string(),
        name.to_string(),
        catalog.to_string(),
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

/// DuckDB's query for the page in `catalog` (every one when `None`),
/// `None` when nothing can match it. Rows `s, n, t, c`: the schema (`''`
/// for none), the name, the table type and the catalog.
fn page_sql(catalog: Option<&str>, q: &ObjectQuery, after: Option<&Cursor>) -> Option<String> {
    let kinds = wanted(q);
    if kinds.is_empty() {
        return None;
    }
    let mut filters = vec![
        "lower(s) <> 'information_schema'".to_string(),
        "s NOT IN ('pg_catalog', 'sys')".to_string(),
    ];
    filters.insert(
        0,
        match catalog {
            Some(c) => format!("c = {}", lit(c)),
            None => "NOT starts_with(c, '_gizmosql')".into(),
        },
    );
    if q.no_schema {
        filters.push("s = ''".into());
    } else if let Some(s) = &q.schema {
        filters.push(format!("s = {}", lit(s)));
    }
    if kinds.len() < KINDS.len() {
        filters.push(if kinds == [kinds::VIEW] {
            "upper(t) LIKE '%VIEW%'".into()
        } else {
            "upper(t) NOT LIKE '%VIEW%'".into()
        });
    }
    if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
        filters.push(format!("strpos(lower(n), lower({})) > 0", lit(p)));
    }
    if let Some(n) = &q.name {
        filters.push(format!("lower(n) = lower({})", lit(n)));
    }
    if let Some(a) = after {
        let (s, n) = (lit(a.schema.as_deref().unwrap_or("")), lit(&a.name));
        filters.push(after_sql(&[
            ("lower(s)".into(), format!("lower({s})")),
            ("lower(n)".into(), format!("lower({n})")),
            ("s".into(), s),
            ("n".into(), n),
            ("c".into(), lit(&a.catalog)),
        ]));
    }
    Some(format!(
        "SELECT s, n, t, c FROM (SELECT coalesce(table_catalog, '') AS c, \
         coalesce(table_schema, '') AS s, table_name AS n, coalesce(table_type, '') AS t \
         FROM information_schema.tables) x \
         WHERE {} ORDER BY lower(s), lower(n), s, n, c LIMIT {}",
        filters.join(" AND "),
        size(q) + 1
    ))
}

/// The page out of the rows (at most one more than the page): sorted, past
/// the cursor, cut at the page; the cursor is the last object kept. What's
/// shown is checked against the query once more (the server's lowercase
/// may differ from Rust's in rare letters).
fn merge(q: &ObjectQuery, after: Option<&Cursor>, found: Vec<(DbObject, String)>) -> ObjectPage {
    let from = after.map(|a| key(a.schema.as_deref(), &a.name, &a.catalog));
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .map(|(o, c)| (key(o.schema.as_deref(), &o.name, &c), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = size(q);
    let next = (found.len() > size).then(|| {
        let (some, _, _, schema, name, catalog) = found[size - 1].0.clone();
        Cursor {
            schema: some.then_some(schema),
            name,
            catalog,
        }
        .write()
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

impl FlightSession {
    /// A page of the catalog's objects (see the module's doc).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        if self.server.engine() != Engine::DuckDb || self.pages_refused {
            return Err(unsupported());
        }
        let after = match q.after.as_deref().map(Cursor::parse) {
            None => None,
            Some(Some(c)) => Some(c),
            // Not ours: the app's own position in the whole list, after an
            // earlier page fell back to it. It goes on paging that list.
            Some(None) => return Err(unsupported()),
        };
        let catalog = self.catalog.clone().filter(|c| !c.is_empty());
        let Some(sql) = page_sql(catalog.as_deref(), q, after.as_ref()) else {
            return Ok(ObjectPage::default());
        };
        let rows = match self.rows(&sql).await {
            Ok((_, rows)) => rows,
            // A first page the server refuses (no `information_schema`, as
            // SQLite behind GizmoSQL): the app pages the whole list, from
            // now on.
            Err(e) if e.is_query() && after.is_none() => {
                tracing::debug!(
                    "flightsql: object page refused, the app reads the whole list: {e}"
                );
                self.pages_refused = true;
                return Err(unsupported());
            }
            Err(e) => return Err(e),
        };
        let found = rows
            .iter()
            .filter(|r| r.len() == 4)
            .map(|r| {
                let o = DbObject {
                    kind: if text(&r[2]).to_ascii_uppercase().contains("VIEW") {
                        kinds::VIEW
                    } else {
                        kinds::TABLE
                    }
                    .into(),
                    schema: Some(text(&r[0])).filter(|s| !s.is_empty()),
                    name: text(&r[1]),
                    parent: None,
                };
                (o, text(&r[3]))
            })
            .collect();
        Ok(merge(q, after.as_ref(), found))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str, catalog: &str) -> (DbObject, String) {
        (
            DbObject {
                kind: kind.into(),
                schema: schema.map(Into::into),
                name: name.into(),
                parent: None,
            },
            catalog.into(),
        )
    }

    /// A schema's views folder: the catalog, the exclusions, the schema,
    /// the kind, the search and the cursor, quotes doubled; one more than
    /// the page.
    #[test]
    fn a_views_folder() {
        let q = ObjectQuery {
            schema: Some("ma'in".into()),
            kind: Some(kinds::VIEW.into()),
            pattern: Some("o'r".into()),
            limit: 7,
            ..Default::default()
        };
        let after = Cursor {
            schema: Some("ma'in".into()),
            name: "b".into(),
            catalog: "my'db".into(),
        };
        let sql = page_sql(Some("my'db"), &q, Some(&after)).unwrap();
        assert!(sql.contains("WHERE c = 'my''db' AND lower(s) <> 'information_schema' AND s NOT IN ('pg_catalog', 'sys') AND s = 'ma''in' AND upper(t) LIKE '%VIEW%' AND strpos(lower(n), lower('o''r')) > 0 AND (lower(s) > lower('ma''in') OR"), "{sql}");
        assert!(
            sql.contains(
                "(s > 'ma''in' OR (s = 'ma''in' AND (n > 'b' OR (n = 'b' AND c > 'my''db')))))))) ORDER BY"
            ),
            "{sql}"
        );
        assert!(
            sql.ends_with("ORDER BY lower(s), lower(n), s, n, c LIMIT 8"),
            "{sql}"
        );
        // No catalog chosen: every one, as `GetTables`; tables only.
        let q = ObjectQuery {
            kind: Some(kinds::TABLE.into()),
            no_schema: true,
            ..Default::default()
        };
        let sql = page_sql(None, &q, None).unwrap();
        assert!(
            sql.contains(
                "x WHERE NOT starts_with(c, '_gizmosql') AND lower(s) <> 'information_schema' AND"
            ),
            "{sql}"
        );
        assert!(
            sql.contains("AND s = '' AND upper(t) NOT LIKE '%VIEW%' ORDER BY"),
            "{sql}"
        );
    }

    #[test]
    fn other_kinds_and_cursors() {
        let q = ObjectQuery {
            not_kinds: KINDS.iter().map(|k| k.to_string()).collect(),
            ..Default::default()
        };
        assert!(page_sql(None, &q, None).is_none());
        let sql = page_sql(None, &ObjectQuery::default(), None).unwrap();
        assert!(!sql.contains("VIEW"), "{sql}");
        let c = Cursor {
            schema: None,
            name: "x".into(),
            catalog: "memory".into(),
        };
        assert_eq!(Cursor::parse(&c.write()), Some(c));
        assert_eq!(Cursor::parse("1000"), None);
    }

    /// Two catalogs with the same schema and name: both, in catalog order.
    #[test]
    fn pages_merge_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            obj(kinds::TABLE, Some("z"), "x", "memory"),
            obj(kinds::VIEW, Some("main"), "s", "memory"),
            obj(kinds::TABLE, Some("main"), "s", "att"),
            obj(kinds::TABLE, None, "Z", "memory"),
        ];
        let p = merge(&q, None, found.clone());
        let got: Vec<_> = p.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got, ["Z", "s"]);
        assert_eq!(p.objects[1].kind, kinds::TABLE);
        let after = Cursor::parse(p.next.as_deref().unwrap()).unwrap();
        assert_eq!(after.catalog, "att");
        let p2 = merge(&q, Some(&after), found);
        let got: Vec<_> = p2.objects.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(got, [kinds::VIEW, kinds::TABLE]);
        assert_eq!(p2.objects[1].name, "x");
        assert_eq!(p2.next, None);
    }
}
