//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`],
//! docs/drivers.md, "Objects by page"): what `list_objects` reads, narrowed
//! to the page on the server.
//!
//! - **Sources:** the session database's `system.tables` (rank 0) and the
//!   SQL user-defined functions of `system.functions` (rank 1), with the
//!   same exclusions as `list_objects`. Each one is asked only when the
//!   query admits one of its kinds; the functions are skipped when the
//!   server can't list them, as `list_objects` does.
//! - **Order:** objects have no schema here, so by name ignoring case, then
//!   as written, then the rank (`list_objects`' order for a table and a
//!   function of the same name): `ORDER BY lowerUTF8(name), name`.
//! - **Cursor:** the page's last object, `[name, rank]` (keyset): the next
//!   page asks each source for what sorts after it, so pages neither skip
//!   nor repeat objects even when the catalog changes in between.
//! - **Page:** each source gives at most one object more than the page; the
//!   merge keeps the page and knows whether more remain.
//!
//! Every value from the query or the cursor is a query parameter
//! (`{name:String}`), never written into the SQL.

use crate::{kind_of, text, ClickHouseSession, Flavor, DICTIONARY};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
const MAX_PAGE: usize = 10_000;

/// The objects a page holds.
fn size(q: &ObjectQuery) -> usize {
    q.page_size().min(MAX_PAGE)
}

/// The previous page's last object: its name and source (the sort key).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    name: String,
    rank: u8,
}

impl Cursor {
    /// `None` for a cursor this driver didn't write.
    fn parse(s: &str) -> Option<Cursor> {
        let (name, rank) = serde_json::from_str(s).ok()?;
        Some(Cursor { name, rank })
    }

    fn write(&self) -> String {
        serde_json::json!([self.name, self.rank]).to_string()
    }
}

/// The tree's order ([`dbine_driver::objects::page_of`]'s), the rank last.
type Key = (String, String, u8);

fn key(name: &str, rank: u8) -> Key {
    (name.to_lowercase(), name.to_string(), rank)
}

/// What the app takes as "page the whole list yourself" (the contract's
/// default).
fn unsupported() -> Error {
    Error::Unsupported("este motor todavía no lista sus objetos por páginas".into())
}

/// The kinds a table of `system.tables` can have, by flavor.
fn table_kinds(flavor: Flavor) -> &'static [&'static str] {
    match flavor {
        Flavor::ClickHouse => &[
            kinds::TABLE,
            kinds::VIEW,
            kinds::MATERIALIZED_VIEW,
            DICTIONARY,
        ],
        Flavor::Timeplus => &[
            kinds::TABLE,
            kinds::VIEW,
            kinds::MATERIALIZED_VIEW,
            DICTIONARY,
            kinds::STREAM,
        ],
    }
}

/// The kinds among `of` the query admits.
fn wanted(q: &ObjectQuery, of: &[&'static str]) -> Vec<&'static str> {
    of.iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// [`kind_of`] in SQL: the kind of a `system.tables` row by its engine.
fn kind_sql(flavor: Flavor) -> String {
    let stream = match flavor {
        Flavor::Timeplus => format!(
            " WHEN engine IN ('Stream', 'ExternalStream', 'Random') THEN '{}'",
            kinds::STREAM
        ),
        Flavor::ClickHouse => String::new(),
    };
    format!(
        "CASE WHEN engine IN ('View', 'LiveView', 'WindowView') THEN '{}' \
         WHEN engine = 'MaterializedView' THEN '{}' \
         WHEN engine = 'Dictionary' THEN '{DICTIONARY}'{stream} ELSE '{}' END",
        kinds::VIEW,
        kinds::MATERIALIZED_VIEW,
        kinds::TABLE
    )
}

/// The query's parameters, named `p0`, `p1`… in order.
#[derive(Default)]
struct Params(Vec<(String, String)>);

impl Params {
    fn bind(&mut self, v: &str) -> String {
        let name = format!("p{}", self.0.len());
        self.0.push((name.clone(), v.to_string()));
        format!("{{{name}:String}}")
    }
}

/// The name, search, lookup and cursor conditions on the column `name`, for
/// the source of rank `rank`.
fn name_filters(q: &ObjectQuery, after: Option<&Cursor>, rank: u8, p: &mut Params) -> String {
    let mut sql = String::new();
    if let Some(pat) = q.pattern.as_deref().filter(|s| !s.is_empty()) {
        sql += &format!(
            " AND positionUTF8(lowerUTF8(name), lowerUTF8({})) > 0",
            p.bind(pat)
        );
    }
    if let Some(n) = &q.name {
        sql += &format!(" AND lowerUTF8(name) = lowerUTF8({})", p.bind(n));
    }
    if let Some(a) = after {
        // (lower(name), name, rank) after the cursor's: with a later rank,
        // the cursor's own name too.
        let op = if rank > a.rank { ">=" } else { ">" };
        let (gt, eq, ge) = (p.bind(&a.name), p.bind(&a.name), p.bind(&a.name));
        sql += &format!(
            " AND (lowerUTF8(name) > lowerUTF8({gt}) \
             OR (lowerUTF8(name) = lowerUTF8({eq}) AND name {op} {ge}))"
        );
    }
    sql
}

/// `system.tables`' query for the page, `None` when the query admits none
/// of its kinds. Rows `name, engine`.
fn tables_sql(
    flavor: Flavor,
    db: &str,
    q: &ObjectQuery,
    after: Option<&Cursor>,
    p: &mut Params,
) -> Option<String> {
    let all = table_kinds(flavor);
    let kinds = wanted(q, all);
    if kinds.is_empty() {
        return None;
    }
    let mut sql = format!(
        "SELECT name, engine FROM system.tables \
         WHERE database = {} AND NOT is_temporary AND name NOT LIKE '.inner%'",
        p.bind(db)
    );
    if kinds.len() < all.len() {
        let list: Vec<String> = kinds.iter().map(|k| format!("'{k}'")).collect();
        sql += &format!(" AND ({}) IN ({})", kind_sql(flavor), list.join(", "));
    }
    sql += &name_filters(q, after, 0, p);
    sql += &format!(" ORDER BY lowerUTF8(name), name LIMIT {}", size(q) + 1);
    Some(sql)
}

/// `system.functions`' query for the page (SQL user-defined functions),
/// `None` when the query doesn't admit functions. Rows `name`.
fn functions_sql(q: &ObjectQuery, after: Option<&Cursor>, p: &mut Params) -> Option<String> {
    if wanted(q, &[kinds::FUNCTION]).is_empty() {
        return None;
    }
    let mut sql = "SELECT name FROM system.functions WHERE origin = 'SQLUserDefined'".to_string();
    sql += &name_filters(q, after, 1, p);
    sql += &format!(" ORDER BY lowerUTF8(name), name LIMIT {}", size(q) + 1);
    Some(sql)
}

/// The page out of what the sources found (each at most one more than the
/// page): sorted, past the cursor, cut at the page; the cursor is the last
/// object kept. What's shown is checked against the query once more (the
/// server's lowercase may differ from Rust's in rare letters).
fn merge(q: &ObjectQuery, after: Option<&Cursor>, found: Vec<(DbObject, u8)>) -> ObjectPage {
    let from = after.map(|a| key(&a.name, a.rank));
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .map(|(o, rank)| (key(&o.name, rank), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = size(q);
    let next = (found.len() > size).then(|| {
        let (_, name, rank) = found[size - 1].0.clone();
        Cursor { name, rank }.write()
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

impl ClickHouseSession {
    /// A page of the session database's objects (see the module's doc).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let after = match q.after.as_deref().map(Cursor::parse) {
            None => None,
            Some(Some(c)) => Some(c),
            // Not ours: the app's own position in the whole list, after an
            // earlier page fell back to it. It goes on paging that list.
            Some(None) => return Err(unsupported()),
        };
        // Nothing here has a schema.
        if !q.no_schema && q.schema.is_some() {
            return Ok(ObjectPage::default());
        }
        let mut found: Vec<(DbObject, u8)> = Vec::new();
        let mut p = Params::default();
        if let Some(sql) = tables_sql(self.flavor, &self.database, q, after.as_ref(), &mut p) {
            let params: Vec<(&str, &str)> =
                p.0.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            for r in self.rows(&sql, &params).await? {
                if r.len() < 2 {
                    continue;
                }
                found.push((
                    DbObject {
                        kind: kind_of(&text(&r[1]), self.flavor).to_string(),
                        schema: None,
                        name: text(&r[0]),
                        parent: None,
                    },
                    0,
                ));
            }
        }
        let mut p = Params::default();
        if let Some(sql) = functions_sql(q, after.as_ref(), &mut p) {
            let params: Vec<(&str, &str)> =
                p.0.iter().map(|(k, v)| (k.as_str(), v.as_str())).collect();
            // As `list_objects`: a server that can't list them has none.
            if let Ok(rows) = self.rows(&sql, &params).await {
                found.extend(rows.iter().filter_map(|r| r.first()).map(|n| {
                    (
                        DbObject {
                            kind: kinds::FUNCTION.into(),
                            schema: None,
                            name: text(n),
                            parent: None,
                        },
                        1,
                    )
                }));
            }
        }
        Ok(merge(q, after.as_ref(), found))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: None,
            name: name.into(),
            parent: None,
        }
    }

    /// A folder asks only for its kind, every value is a parameter and the
    /// page asks one more than its size.
    #[test]
    fn a_folder_binds_everything_and_asks_one_more() {
        let q = ObjectQuery {
            kind: Some(kinds::VIEW.into()),
            pattern: Some("o'r%".into()),
            limit: 7,
            ..Default::default()
        };
        let after = Cursor {
            name: "b'x".into(),
            rank: 0,
        };
        let mut p = Params::default();
        let sql = tables_sql(Flavor::ClickHouse, "db", &q, Some(&after), &mut p).unwrap();
        assert!(sql.starts_with("SELECT name, engine FROM system.tables WHERE database = {p0:String} AND NOT is_temporary AND name NOT LIKE '.inner%' AND (CASE"), "{sql}");
        assert!(sql.contains(") IN ('view')"), "{sql}");
        assert!(
            sql.contains("AND positionUTF8(lowerUTF8(name), lowerUTF8({p1:String})) > 0"),
            "{sql}"
        );
        assert!(
            sql.contains(
                "AND (lowerUTF8(name) > lowerUTF8({p2:String}) OR (lowerUTF8(name) = lowerUTF8({p3:String}) AND name > {p4:String}))"
            ),
            "{sql}"
        );
        assert!(
            sql.ends_with("ORDER BY lowerUTF8(name), name LIMIT 8"),
            "{sql}"
        );
        assert!(!sql.contains("o'r"), "{sql}");
        let values: Vec<&str> = p.0.iter().map(|(_, v)| v.as_str()).collect();
        assert_eq!(values, ["db", "o'r%", "b'x", "b'x", "b'x"]);
        // A view folder never asks for functions.
        assert!(functions_sql(&q, None, &mut Params::default()).is_none());
    }

    /// After a table, the functions start at its name; after a function,
    /// the tables start past it.
    #[test]
    fn the_cursor_carries_the_rank() {
        let q = ObjectQuery::default();
        let at_table = Cursor {
            name: "x".into(),
            rank: 0,
        };
        let sql = functions_sql(&q, Some(&at_table), &mut Params::default()).unwrap();
        assert!(sql.contains("AND name >= {p2:String}"), "{sql}");
        let at_function = Cursor {
            name: "x".into(),
            rank: 1,
        };
        let sql = tables_sql(
            Flavor::Timeplus,
            "db",
            &q,
            Some(&at_function),
            &mut Params::default(),
        )
        .unwrap();
        assert!(sql.contains("AND name > {p3:String}"), "{sql}");
        // Every kind: no kind condition.
        assert!(!sql.contains("CASE"), "{sql}");
        assert_eq!(Cursor::parse(&at_function.write()), Some(at_function));
        // The app's own offsets aren't ours.
        assert_eq!(Cursor::parse("1000"), None);
    }

    /// "Otros" (every declared kind excluded) asks nothing; Timeplus' streams
    /// are a kind of their own there.
    #[test]
    fn other_kinds_and_streams() {
        let declared = vec![
            kinds::TABLE.to_string(),
            kinds::VIEW.into(),
            kinds::MATERIALIZED_VIEW.into(),
            DICTIONARY.into(),
            kinds::FUNCTION.into(),
        ];
        let q = ObjectQuery {
            not_kinds: declared,
            ..Default::default()
        };
        let mut p = Params::default();
        assert!(tables_sql(Flavor::ClickHouse, "db", &q, None, &mut p).is_none());
        assert!(functions_sql(&q, None, &mut p).is_none());
        let sql = tables_sql(Flavor::Timeplus, "db", &q, None, &mut p).unwrap();
        assert!(sql.contains(") IN ('stream')"), "{sql}");
        assert!(kind_sql(Flavor::Timeplus).contains("'Random') THEN 'stream'"));
    }

    #[test]
    fn pages_merge_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            (obj(kinds::TABLE, "b"), 0),
            (obj(kinds::FUNCTION, "B"), 1),
            (obj(kinds::TABLE, "B"), 0),
            (obj(kinds::VIEW, "a"), 0),
        ];
        let p = merge(&q, None, found.clone());
        let got: Vec<_> = p.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got, ["a", "B"]);
        assert_eq!(p.objects[1].kind, kinds::TABLE);
        let after = Cursor::parse(p.next.as_deref().unwrap()).unwrap();
        let p2 = merge(&q, Some(&after), found);
        let got: Vec<_> = p2.objects.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(got, [kinds::FUNCTION, kinds::TABLE]);
        assert_eq!(p2.next, None);
    }
}
