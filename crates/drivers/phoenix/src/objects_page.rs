//! The explorer's pages of objects ([`dbine_driver::Session::list_objects_page`],
//! docs/drivers.md, "Objects by page"): what `list_objects` reads, narrowed
//! to the page on the server.
//!
//! - **Sources:** `SYSTEM.CATALOG`'s table and view header rows (rank 0)
//!   and `SYSTEM."SEQUENCE"` (rank 1), without tenants' objects, as
//!   `list_objects`. Each one is asked only when the query admits one of
//!   its kinds.
//! - **Order:** schema (the default one, `NULL`, first), then name, both
//!   ignoring case, then as written, then the rank (`list_objects`' order
//!   for a table and a sequence of the same name).
//! - **Cursor:** the page's last object, `[schema, name, rank]` (keyset):
//!   the next page asks each source for what sorts after it, so pages
//!   neither skip nor repeat objects even when the catalog changes in
//!   between.
//! - **Page:** each source gives at most one object more than the page; the
//!   merge keeps the page and knows whether more remain.
//!
//! A generic Avatica server (another database behind the same protocol)
//! has no catalog tables to narrow: its metadata calls list everything, so
//! it keeps the contract's default and the app pages `list_objects`.
//!
//! Values from the query or the cursor go in as string literals (quotes
//! doubled: Phoenix's strings have no other escape).

use crate::{lit, text, PhoenixSession};
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
const MAX_PAGE: usize = 10_000;

/// The kinds of `SYSTEM.CATALOG`'s rows.
const TABLE_KINDS: &[&str] = &[kinds::TABLE, kinds::VIEW];

/// The objects a page holds.
fn size(q: &ObjectQuery) -> usize {
    q.page_size().min(MAX_PAGE)
}

/// The previous page's last object: its schema, name and source (the sort
/// key).
#[derive(Debug, Clone, PartialEq, Eq)]
struct Cursor {
    schema: Option<String>,
    name: String,
    rank: u8,
}

impl Cursor {
    /// `None` for a cursor this driver didn't write.
    fn parse(s: &str) -> Option<Cursor> {
        let (schema, name, rank) = serde_json::from_str(s).ok()?;
        Some(Cursor { schema, name, rank })
    }

    fn write(&self) -> String {
        serde_json::json!([self.schema, self.name, self.rank]).to_string()
    }
}

/// The tree's order ([`dbine_driver::objects::page_of`]'s), the rank last.
type Key = (bool, String, String, String, String, u8);

fn key(schema: Option<&str>, name: &str, rank: u8) -> Key {
    let s = schema.unwrap_or("");
    (
        schema.is_some(),
        s.to_lowercase(),
        name.to_lowercase(),
        s.to_string(),
        name.to_string(),
        rank,
    )
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

/// `(c1 > v1 OR (c1 = v1 AND (… AND cN op vN)))`.
fn after_sql(keys: &[(String, String)], op: &str) -> String {
    match keys {
        [] => "TRUE".into(),
        [(c, v)] => format!("{c} {op} {v}"),
        [(c, v), rest @ ..] => format!("({c} > {v} OR ({c} = {v} AND {}))", after_sql(rest, op)),
    }
}

/// The conditions on a source's schema column `sch` and name column `nm`,
/// for the source of rank `rank`: the schema, the search, the lookup and
/// the cursor.
fn filters(q: &ObjectQuery, after: Option<&Cursor>, sch: &str, nm: &str, rank: u8) -> String {
    let mut sql = String::new();
    if q.no_schema {
        sql += &format!(" AND {sch} IS NULL");
    } else if let Some(s) = &q.schema {
        sql += &format!(" AND {sch} = {}", lit(s));
    }
    if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
        sql += &format!(" AND INSTR(LOWER({nm}), LOWER({})) > 0", lit(p));
    }
    if let Some(n) = &q.name {
        sql += &format!(" AND LOWER({nm}) = LOWER({})", lit(n));
    }
    if let Some(a) = after {
        // With a later rank, the cursor's own schema and name too.
        let op = if rank > a.rank { ">=" } else { ">" };
        let n = lit(&a.name);
        let names = [
            (format!("LOWER({nm})"), format!("LOWER({n})")),
            (nm.to_string(), n),
        ];
        sql += &match &a.schema {
            // The default schema sorts first: past it, any other one.
            None => format!(
                " AND (({sch} IS NULL AND {}) OR {sch} IS NOT NULL)",
                after_sql(&names, op)
            ),
            Some(s) => {
                let s = lit(s);
                let [lower_name, name] = names;
                let keys = [
                    (format!("LOWER({sch})"), format!("LOWER({s})")),
                    lower_name,
                    (sch.to_string(), s),
                    name,
                ];
                format!(" AND {sch} IS NOT NULL AND {}", after_sql(&keys, op))
            }
        };
    }
    sql
}

/// The tree's order on the columns `sch` and `nm`, and one object more than
/// the page.
fn order(q: &ObjectQuery, sch: &str, nm: &str) -> String {
    format!(
        " ORDER BY LOWER({sch}) NULLS FIRST, LOWER({nm}), {sch} NULLS FIRST, {nm} LIMIT {}",
        size(q) + 1
    )
}

/// `SYSTEM.CATALOG`'s query for the page, `None` when the query admits
/// neither tables nor views. Rows `TABLE_SCHEM, TABLE_NAME, TABLE_TYPE`.
fn tables_sql(q: &ObjectQuery, after: Option<&Cursor>) -> Option<String> {
    let kinds = wanted(q, TABLE_KINDS);
    if kinds.is_empty() {
        return None;
    }
    let types = match kinds.as_slice() {
        [k] if *k == kinds::VIEW => "('v')",
        [_] => "('u')",
        _ => "('u', 'v')",
    };
    Some(format!(
        "SELECT TABLE_SCHEM, TABLE_NAME, TABLE_TYPE FROM SYSTEM.CATALOG \
         WHERE TENANT_ID IS NULL AND COLUMN_NAME IS NULL AND COLUMN_FAMILY IS NULL \
         AND TABLE_TYPE IN {types}{}{}",
        filters(q, after, "TABLE_SCHEM", "TABLE_NAME", 0),
        order(q, "TABLE_SCHEM", "TABLE_NAME")
    ))
}

/// `SYSTEM."SEQUENCE"`'s query for the page, `None` when the query doesn't
/// admit sequences. Rows `SEQUENCE_SCHEMA, SEQUENCE_NAME`.
fn sequences_sql(q: &ObjectQuery, after: Option<&Cursor>) -> Option<String> {
    if wanted(q, &[kinds::SEQUENCE]).is_empty() {
        return None;
    }
    Some(format!(
        "SELECT SEQUENCE_SCHEMA, SEQUENCE_NAME FROM SYSTEM.\"SEQUENCE\" WHERE TENANT_ID IS NULL{}{}",
        filters(q, after, "SEQUENCE_SCHEMA", "SEQUENCE_NAME", 1),
        order(q, "SEQUENCE_SCHEMA", "SEQUENCE_NAME")
    ))
}

/// The page out of what the sources found (each at most one more than the
/// page): sorted, past the cursor, cut at the page; the cursor is the last
/// object kept. What's shown is checked against the query once more (the
/// server's lowercase may differ from Rust's in rare letters).
fn merge(q: &ObjectQuery, after: Option<&Cursor>, found: Vec<(DbObject, u8)>) -> ObjectPage {
    let from = after.map(|a| key(a.schema.as_deref(), &a.name, a.rank));
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .map(|(o, rank)| (key(o.schema.as_deref(), &o.name, rank), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = size(q);
    let next = (found.len() > size).then(|| {
        let (some, _, _, schema, name, rank) = found[size - 1].0.clone();
        Cursor {
            schema: some.then_some(schema),
            name,
            rank,
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

impl PhoenixSession {
    /// A page of Phoenix's objects (see the module's doc).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        if self.generic {
            return Err(unsupported());
        }
        let after = match q.after.as_deref().map(Cursor::parse) {
            None => None,
            Some(Some(c)) => Some(c),
            // Not ours: the app's own position in the whole list, after an
            // earlier page fell back to it. It goes on paging that list.
            Some(None) => return Err(unsupported()),
        };
        let schema = |v: &serde_json::Value| Some(text(v)).filter(|s| !s.is_empty());
        let mut found: Vec<(DbObject, u8)> = Vec::new();
        if let Some(sql) = tables_sql(q, after.as_ref()) {
            for r in self.rows(&sql).await? {
                if r.len() < 3 {
                    continue;
                }
                found.push((
                    DbObject {
                        kind: if text(&r[2]) == "v" {
                            kinds::VIEW
                        } else {
                            kinds::TABLE
                        }
                        .into(),
                        schema: schema(&r[0]),
                        name: text(&r[1]),
                        parent: None,
                    },
                    0,
                ));
            }
        }
        if let Some(sql) = sequences_sql(q, after.as_ref()) {
            for r in self.rows(&sql).await? {
                if r.len() < 2 {
                    continue;
                }
                found.push((
                    DbObject {
                        kind: kinds::SEQUENCE.into(),
                        schema: schema(&r[0]),
                        name: text(&r[1]),
                        parent: None,
                    },
                    1,
                ));
            }
        }
        Ok(merge(q, after.as_ref(), found))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn obj(kind: &str, schema: Option<&str>, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: schema.map(Into::into),
            name: name.into(),
            parent: None,
        }
    }

    /// A schema's views folder: the type, the schema, the search and the
    /// cursor, quotes doubled; no sequences asked.
    #[test]
    fn a_views_folder() {
        let q = ObjectQuery {
            schema: Some("S'1".into()),
            kind: Some(kinds::VIEW.into()),
            pattern: Some("o'r".into()),
            limit: 7,
            ..Default::default()
        };
        let after = Cursor {
            schema: Some("S'1".into()),
            name: "B".into(),
            rank: 0,
        };
        let sql = tables_sql(&q, Some(&after)).unwrap();
        assert_eq!(
            sql,
            "SELECT TABLE_SCHEM, TABLE_NAME, TABLE_TYPE FROM SYSTEM.CATALOG \
             WHERE TENANT_ID IS NULL AND COLUMN_NAME IS NULL AND COLUMN_FAMILY IS NULL \
             AND TABLE_TYPE IN ('v') AND TABLE_SCHEM = 'S''1' \
             AND INSTR(LOWER(TABLE_NAME), LOWER('o''r')) > 0 \
             AND TABLE_SCHEM IS NOT NULL AND (LOWER(TABLE_SCHEM) > LOWER('S''1') OR (LOWER(TABLE_SCHEM) = LOWER('S''1') \
             AND (LOWER(TABLE_NAME) > LOWER('B') OR (LOWER(TABLE_NAME) = LOWER('B') \
             AND (TABLE_SCHEM > 'S''1' OR (TABLE_SCHEM = 'S''1' AND TABLE_NAME > 'B')))))) \
             ORDER BY LOWER(TABLE_SCHEM) NULLS FIRST, LOWER(TABLE_NAME), TABLE_SCHEM NULLS FIRST, TABLE_NAME LIMIT 8"
        );
        assert!(sequences_sql(&q, None).is_none());
    }

    /// The default schema: `IS NULL`, and past a cursor in it, its later
    /// names and every named schema; after a table, the sequences start at
    /// its name.
    #[test]
    fn the_default_schema_and_sequences() {
        let q = ObjectQuery {
            no_schema: true,
            schema: Some("ignored".into()),
            ..Default::default()
        };
        let after = Cursor {
            schema: None,
            name: "X".into(),
            rank: 0,
        };
        let sql = sequences_sql(&q, Some(&after)).unwrap();
        assert!(sql.contains("WHERE TENANT_ID IS NULL AND SEQUENCE_SCHEMA IS NULL AND ((SEQUENCE_SCHEMA IS NULL AND (LOWER(SEQUENCE_NAME) > LOWER('X') OR (LOWER(SEQUENCE_NAME) = LOWER('X') AND SEQUENCE_NAME >= 'X'))) OR SEQUENCE_SCHEMA IS NOT NULL) ORDER BY"), "{sql}");
        assert!(!sql.contains("ignored"), "{sql}");
        let sql = tables_sql(&q, None).unwrap();
        assert!(
            sql.contains("TABLE_TYPE IN ('u', 'v') AND TABLE_SCHEM IS NULL ORDER BY"),
            "{sql}"
        );
        assert_eq!(Cursor::parse(&after.write()), Some(after));
        assert_eq!(Cursor::parse("1000"), None);
        // "Otros": nothing.
        let q = ObjectQuery {
            not_kinds: vec![
                kinds::TABLE.into(),
                kinds::VIEW.into(),
                kinds::SEQUENCE.into(),
            ],
            ..Default::default()
        };
        assert!(tables_sql(&q, None).is_none());
        assert!(sequences_sql(&q, None).is_none());
    }

    #[test]
    fn pages_merge_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            (obj(kinds::SEQUENCE, Some("A"), "S"), 1),
            (obj(kinds::TABLE, Some("A"), "S"), 0),
            (obj(kinds::TABLE, None, "Z"), 0),
        ];
        let p = merge(&q, None, found.clone());
        let got: Vec<_> = p.objects.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(got, [kinds::TABLE, kinds::TABLE]);
        assert_eq!(p.objects[0].schema, None);
        let after = Cursor::parse(p.next.as_deref().unwrap()).unwrap();
        assert_eq!(after.rank, 0);
        let p2 = merge(&q, Some(&after), found);
        let got: Vec<_> = p2.objects.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(got, [kinds::SEQUENCE]);
        assert_eq!(p2.next, None);
    }
}
