//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`],
//! docs/drivers.md, "Objects by page"): what `list_objects` reads, narrowed
//! to the page on the server.
//!
//! - **Sources:** the schema's `INFORMATION_SCHEMA.TABLES` (rank 0) and, in
//!   file-system workspaces, its files and directories (rank 1) from
//!   `INFORMATION_SCHEMA.FILES`, the table `SHOW FILES` reads: the
//!   workspace's own entries (not those inside its directories, when the
//!   listing is recursive), without hidden files, view and checksum files,
//!   or a file named as a table. Each one is asked only when the query
//!   admits one of its kinds.
//! - **Order:** every object is in the session's schema, so by name
//!   ignoring case, then as written, then the rank:
//!   `ORDER BY lower(name), name`.
//! - **Cursor:** the page's last object, `[name, rank]` (keyset): the next
//!   page asks each source for what sorts after it, so pages neither skip
//!   nor repeat objects even when the workspace changes in between.
//! - **Page:** each source gives at most one object more than the page; the
//!   merge keeps the page and knows whether more remain.
//!
//! Values from the query or the cursor go in as string literals (quotes
//! doubled: Drill's strings have no other escape).

use crate::{lit, DrillSession, FILE};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
const MAX_PAGE: usize = 10_000;

/// The kinds of `INFORMATION_SCHEMA.TABLES`' rows.
const TABLE_KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW];

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

/// The kinds among `of` the query admits.
fn wanted(q: &ObjectQuery, of: &[&'static str]) -> Vec<&'static str> {
    of.iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// The search, lookup and cursor conditions on the name column `col`, for
/// the source of rank `rank`.
fn name_filters(q: &ObjectQuery, after: Option<&Cursor>, col: &str, rank: u8) -> String {
    let mut sql = String::new();
    if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
        sql += &format!(" AND strpos(lower({col}), lower({})) > 0", lit(p));
    }
    if let Some(n) = &q.name {
        sql += &format!(" AND lower({col}) = lower({})", lit(n));
    }
    if let Some(a) = after {
        // (lower(name), name, rank) after the cursor's: with a later rank,
        // the cursor's own name too.
        let op = if rank > a.rank { ">=" } else { ">" };
        let v = lit(&a.name);
        sql += &format!(
            " AND (lower({col}) > lower({v}) OR (lower({col}) = lower({v}) AND {col} {op} {v}))"
        );
    }
    sql
}

/// `INFORMATION_SCHEMA.TABLES`' query for the page, `None` when the query
/// admits neither tables nor views. Rows `TABLE_NAME, TABLE_TYPE`.
fn tables_sql(schema: &str, q: &ObjectQuery, after: Option<&Cursor>) -> Option<String> {
    let kinds = wanted(q, TABLE_KINDS);
    if kinds.is_empty() {
        return None;
    }
    let mut sql = format!(
        "SELECT TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.`TABLES` WHERE TABLE_SCHEMA = {}",
        lit(schema)
    );
    if kinds.len() < TABLE_KINDS.len() {
        sql += if kinds == [kinds::VIEW] {
            " AND TABLE_TYPE = 'VIEW'"
        } else {
            " AND TABLE_TYPE <> 'VIEW'"
        };
    }
    sql += &name_filters(q, after, "TABLE_NAME", 0);
    sql += &format!(
        " ORDER BY lower(TABLE_NAME), TABLE_NAME LIMIT {}",
        size(q) + 1
    );
    Some(sql)
}

/// `INFORMATION_SCHEMA.FILES`' query for the page (what `list_objects`
/// keeps of `SHOW FILES`), `None` when the query doesn't admit files. Each
/// side of the join is narrowed to the schema itself, so Drill lists only
/// that workspace. Rows `RELATIVE_PATH`.
fn files_sql(schema: &str, q: &ObjectQuery, after: Option<&Cursor>) -> Option<String> {
    if wanted(q, &[FILE]).is_empty() {
        return None;
    }
    let s = lit(schema);
    Some(format!(
        "SELECT f.RELATIVE_PATH FROM (SELECT RELATIVE_PATH FROM INFORMATION_SCHEMA.`FILES` \
         WHERE SCHEMA_NAME = {s} AND RELATIVE_PATH = FILE_NAME AND RELATIVE_PATH <> '' \
         AND substr(RELATIVE_PATH, 1, 1) NOT IN ('.', '_') \
         AND RELATIVE_PATH NOT LIKE '%.view.drill' AND RELATIVE_PATH NOT LIKE '%.crc'{}) f \
         LEFT JOIN (SELECT TABLE_NAME FROM INFORMATION_SCHEMA.`TABLES` WHERE TABLE_SCHEMA = {s}) t \
         ON t.TABLE_NAME = f.RELATIVE_PATH WHERE t.TABLE_NAME IS NULL \
         ORDER BY lower(f.RELATIVE_PATH), f.RELATIVE_PATH LIMIT {}",
        name_filters(q, after, "RELATIVE_PATH", 1),
        size(q) + 1
    ))
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

impl DrillSession {
    /// A page of the session schema's objects (see the module's doc).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let after = match q.after.as_deref().map(Cursor::parse) {
            None => None,
            Some(Some(c)) => Some(c),
            // Not ours: the app's own position in the whole list, after an
            // earlier page fell back to it. It goes on paging that list.
            Some(None) => return Err(unsupported()),
        };
        let schema = self.schema()?;
        // Every object is in the session's schema.
        if q.no_schema || q.schema.as_ref().is_some_and(|s| *s != schema) {
            return Ok(ObjectPage::default());
        }
        let mut found: Vec<(DbObject, u8)> = Vec::new();
        if let Some(sql) = tables_sql(&schema, q, after.as_ref()) {
            for r in self.strings(&sql).await? {
                if r.len() < 2 {
                    continue;
                }
                found.push((
                    DbObject {
                        kind: if r[1] == "VIEW" {
                            kinds::VIEW
                        } else {
                            kinds::TABLE
                        }
                        .into(),
                        schema: Some(schema.clone()),
                        name: r[0].clone(),
                        parent: None,
                    },
                    0,
                ));
            }
        }
        if let Some(sql) = files_sql(&schema, q, after.as_ref()) {
            // As `list_objects`: a schema whose files can't be listed has
            // none.
            if let Ok(rows) = self.strings(&sql).await {
                found.extend(
                    rows.into_iter()
                        .filter_map(|r| r.into_iter().next())
                        .map(|name| {
                            (
                                DbObject {
                                    kind: FILE.into(),
                                    schema: Some(schema.clone()),
                                    name,
                                    parent: None,
                                },
                                1,
                            )
                        }),
                );
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
            schema: Some("dfs.tmp".into()),
            name: name.into(),
            parent: None,
        }
    }

    /// A views folder: one kind, the search and the cursor, quotes doubled,
    /// one more than the page; no files asked.
    #[test]
    fn a_views_folder() {
        let q = ObjectQuery {
            kind: Some(kinds::VIEW.into()),
            pattern: Some("o'r".into()),
            limit: 7,
            ..Default::default()
        };
        let after = Cursor {
            name: "b'x".into(),
            rank: 0,
        };
        let sql = tables_sql("dfs.t'mp", &q, Some(&after)).unwrap();
        assert_eq!(
            sql,
            "SELECT TABLE_NAME, TABLE_TYPE FROM INFORMATION_SCHEMA.`TABLES` WHERE TABLE_SCHEMA = 'dfs.t''mp' \
             AND TABLE_TYPE = 'VIEW' AND strpos(lower(TABLE_NAME), lower('o''r')) > 0 \
             AND (lower(TABLE_NAME) > lower('b''x') OR (lower(TABLE_NAME) = lower('b''x') AND TABLE_NAME > 'b''x')) \
             ORDER BY lower(TABLE_NAME), TABLE_NAME LIMIT 8"
        );
        assert!(files_sql("dfs.tmp", &q, None).is_none());
    }

    /// The files: the workspace's own entries without what `list_objects`
    /// skips; after a table, from its name on.
    #[test]
    fn the_files_folder() {
        let q = ObjectQuery {
            kind: Some(FILE.into()),
            name: Some("X".into()),
            ..Default::default()
        };
        assert!(tables_sql("dfs.tmp", &q, None).is_none());
        let after = Cursor {
            name: "x".into(),
            rank: 0,
        };
        let sql = files_sql("dfs.tmp", &q, Some(&after)).unwrap();
        for part in [
            "WHERE SCHEMA_NAME = 'dfs.tmp' AND RELATIVE_PATH = FILE_NAME",
            "NOT IN ('.', '_')",
            "NOT LIKE '%.view.drill'",
            "AND lower(RELATIVE_PATH) = lower('X')",
            "AND RELATIVE_PATH >= 'x'))) f LEFT JOIN",
            "WHERE TABLE_SCHEMA = 'dfs.tmp') t ON t.TABLE_NAME = f.RELATIVE_PATH WHERE t.TABLE_NAME IS NULL",
            "LIMIT 1001",
        ] {
            assert!(sql.contains(part), "{part}: {sql}");
        }
        assert_eq!(Cursor::parse(&after.write()), Some(after));
        assert_eq!(Cursor::parse("1000"), None);
        // "Otros": nothing.
        let q = ObjectQuery {
            not_kinds: vec![kinds::TABLE.into(), kinds::VIEW.into(), FILE.into()],
            ..Default::default()
        };
        assert!(tables_sql("s", &q, None).is_none());
        assert!(files_sql("s", &q, None).is_none());
    }

    #[test]
    fn pages_merge_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            (obj(FILE, "b.csv"), 1),
            (obj(kinds::TABLE, "B.csv"), 0),
            (obj(kinds::VIEW, "a"), 0),
        ];
        let p = merge(&q, None, found.clone());
        let got: Vec<_> = p.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got, ["a", "B.csv"]);
        let after = Cursor::parse(p.next.as_deref().unwrap()).unwrap();
        let p2 = merge(&q, Some(&after), found);
        let got: Vec<_> = p2.objects.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(got, [FILE]);
        assert_eq!(p2.next, None);
    }
}
