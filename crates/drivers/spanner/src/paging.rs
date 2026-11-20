//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): the tree's order and cursor, and one catalog query
//! for a page of the sources a query needs.
//!
//! - **Order:** schema (none first), then name, both ignoring case, then as
//!   written, then the kind: `ORDER BY LOWER(s), LOWER(n), s, n, k` on the
//!   server, the same key here. The default schema is `''`, which sorts
//!   first, as objects without a schema do in the tree.
//! - **Cursor:** the page's last object (`[schema, name, kind]`); the next
//!   page asks for what sorts after it (keyset), so pages neither skip nor
//!   repeat objects even when the catalog changes in between.
//! - **Page:** the query asks for one object more than the page, which
//!   tells whether more remain.

use crate::SpannerSession;
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery, Result};

/// The largest page asked of the server: a bigger `limit` gets pages of
/// this size (a page holds at most the limit).
pub(crate) const MAX_PAGE: usize = 10_000;

/// A bound value's placeholder (the `n`th, from 0; `text_rows` names them).
fn placeholder(n: usize) -> String {
    format!("@p{n}")
}

/// `needle` inside `hay` (both already lowercased).
fn contains(hay: &str, needle: &str) -> String {
    format!("STRPOS({hay}, {needle}) > 0")
}

/// The objects a page holds.
pub(crate) fn size(q: &ObjectQuery) -> usize {
    q.page_size().min(MAX_PAGE)
}

type Key = (bool, String, String, String, String, String);

/// The tree's order (`dbine_driver::objects::page_of`'s), with the kind last.
fn key(o: &DbObject) -> Key {
    let s = o.schema.clone().unwrap_or_default();
    (
        o.schema.is_some(),
        s.to_lowercase(),
        o.name.to_lowercase(),
        s,
        o.name.clone(),
        o.kind.clone(),
    )
}

/// The object the query's cursor points at (the previous page's last). A
/// cursor this driver didn't write starts from the top, as `page_of` does.
pub(crate) fn after(q: &ObjectQuery) -> Option<DbObject> {
    let (schema, name, kind): (Option<String>, String, String) =
        serde_json::from_str(q.after.as_deref()?).ok()?;
    Some(DbObject {
        kind,
        schema,
        name,
        parent: None,
    })
}

fn cursor(o: &DbObject) -> String {
    serde_json::json!([o.schema, o.name, o.kind]).to_string()
}

/// The kinds among `kinds` the query admits.
pub(crate) fn wanted(q: &ObjectQuery, kinds: &[&'static str]) -> Vec<&'static str> {
    kinds
        .iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// The page out of what the query found (at most one more than the page):
/// sorted, past the cursor, without repeats, cut at the page; the cursor
/// is the last object kept. What's shown is checked against the query once
/// more (a server's lowercase may differ in rare letters).
pub(crate) fn merge(q: &ObjectQuery, after: Option<&DbObject>, found: Vec<DbObject>) -> ObjectPage {
    let from = after.map(key);
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .map(|o| (key(&o), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = size(q);
    let next = (found.len() > size).then(|| cursor(&found[size - 1].1));
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

/// A catalog's list of objects: its rows as the schema (`''` for none), the
/// name and the kind's id.
pub(crate) struct Source<'a> {
    pub distinct: bool,
    pub schema: &'a str,
    pub name: &'a str,
    /// The row's kind id (an SQL expression).
    pub kind: &'a str,
    /// What follows `FROM`, up to a `WHERE` with `list_objects`'
    /// exclusions (`WHERE TRUE` when there are none).
    pub from: &'a str,
    /// The kinds it gives.
    pub kinds: &'static [&'static str],
}

/// The values bound to a query, in the order of their placeholders.
#[derive(Default)]
pub(crate) struct Binds(pub Vec<String>);

impl Binds {
    fn bind(&mut self, v: &str) -> String {
        self.0.push(v.to_string());
        placeholder(self.0.len() - 1)
    }
}

/// One query for the page out of `sources` (those the query admits, joined
/// with `UNION ALL`): rows `s, n, k` past the cursor, in the tree's order,
/// one more than the page. `None` when no source has a kind the query
/// admits. Every value is bound.
pub(crate) fn query(
    sources: &[&Source],
    q: &ObjectQuery,
    after: Option<&DbObject>,
    b: &mut Binds,
) -> Option<String> {
    if !q.no_schema && q.schema.as_deref() == Some("") {
        return None;
    }
    let mut parts = Vec::new();
    for src in sources {
        let kinds = wanted(q, src.kinds);
        if kinds.is_empty() {
            continue;
        }
        let mut sql = format!(
            "SELECT {}{} AS s, {} AS n, {} AS k FROM {}",
            if src.distinct { "DISTINCT " } else { "" },
            src.schema,
            src.name,
            src.kind,
            src.from
        );
        if q.no_schema {
            sql += &format!(" AND {} = ''", src.schema);
        } else if let Some(s) = &q.schema {
            sql += &format!(" AND {} = {}", src.schema, b.bind(s));
        }
        if kinds.len() < src.kinds.len() {
            let list: Vec<String> = kinds.iter().map(|k| format!("'{k}'")).collect();
            sql += &format!(" AND {} IN ({})", src.kind, list.join(", "));
        }
        if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
            let needle = format!("LOWER({})", b.bind(p));
            sql += &format!(
                " AND {}",
                contains(&format!("LOWER({})", src.name), &needle)
            );
        }
        if let Some(n) = &q.name {
            sql += &format!(" AND LOWER({}) = LOWER({})", src.name, b.bind(n));
        }
        parts.push(sql);
    }
    if parts.is_empty() {
        return None;
    }
    let mut sql = format!("SELECT s, n, k FROM ({}) t", parts.join(" UNION ALL "));
    if let Some(a) = after {
        let s = a.schema.as_deref().unwrap_or("");
        let keys = [
            ("LOWER(s)", s, true),
            ("LOWER(n)", a.name.as_str(), true),
            ("s", s, false),
            ("n", a.name.as_str(), false),
            ("k", a.kind.as_str(), false),
        ];
        // (c1 > v1 OR (c1 = v1 AND (c2 > v2 OR (… AND cN > vN)))).
        let mut cond = String::new();
        let mut close = String::new();
        for (i, (col, v, lower)) in keys.iter().enumerate() {
            let mut val = || {
                let p = b.bind(v);
                if *lower {
                    format!("LOWER({p})")
                } else {
                    p
                }
            };
            if i + 1 == keys.len() {
                cond += &format!("{col} > {}", val());
            } else {
                let gt = val();
                let eq = val();
                cond += &format!("({col} > {gt} OR ({col} = {eq} AND ");
                close += "))";
            }
        }
        sql += &format!(" WHERE {cond}{close}");
    }
    sql += &format!(
        " ORDER BY LOWER(s), LOWER(n), s, n, k LIMIT {}",
        size(q) + 1
    );
    Some(sql)
}

/// A row `s, n, k` as an object (`''`: no schema).
pub(crate) fn object(r: &[Option<String>]) -> Option<DbObject> {
    let at = |i: usize| r.get(i).cloned().flatten();
    Some(DbObject {
        schema: at(0).filter(|s| !s.is_empty()),
        name: at(1)?,
        kind: at(2)?,
        parent: None,
    })
}

// -- Spanner's sources ---------------------------------------------------------------

impl SpannerSession {
    /// A page of the database's objects: the same tables, views and
    /// sequences as `list_objects`, in one `INFORMATION_SCHEMA` query
    /// narrowed to the page.
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let tables = Source {
            distinct: false,
            schema: "table_schema",
            name: "table_name",
            kind: "CASE table_type WHEN 'VIEW' THEN 'view' ELSE 'table' END",
            from: "INFORMATION_SCHEMA.TABLES \
                   WHERE table_schema NOT IN ('INFORMATION_SCHEMA', 'SPANNER_SYS')",
            kinds: &[kinds::TABLE, kinds::VIEW],
        };
        let sequences = Source {
            distinct: false,
            schema: "SCHEMA",
            name: "NAME",
            kind: "'sequence'",
            from: "INFORMATION_SCHEMA.SEQUENCES WHERE TRUE",
            kinds: &[kinds::SEQUENCE],
        };
        let after = after(q);
        let mut b = Binds::default();
        let Some(sql) = query(&[&tables, &sequences], q, after.as_ref(), &mut b) else {
            return Ok(ObjectPage::default());
        };
        let args: Vec<&str> = b.0.iter().map(String::as_str).collect();
        let rows = self.text_rows(&sql, &args).await?;
        let found = rows.iter().filter_map(|r| object(r)).collect();
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

    /// The default schema's objects (`''`) are those without a schema, the
    /// cursor binds as `@pN` and the page asks one more than its size.
    #[test]
    fn no_schema_is_the_default_one_and_values_are_bound() {
        let src = Source {
            distinct: false,
            schema: "table_schema",
            name: "table_name",
            kind: "'table'",
            from: "T WHERE TRUE",
            kinds: &[kinds::TABLE],
        };
        let q = ObjectQuery {
            no_schema: true,
            schema: Some("ignored".into()),
            pattern: Some("x".into()),
            limit: 7,
            ..Default::default()
        };
        let a = obj(kinds::TABLE, None, "b");
        let mut b = Binds::default();
        let sql = query(&[&src], &q, Some(&a), &mut b).unwrap();
        assert!(
            sql.contains(
                "WHERE TRUE AND table_schema = '' AND STRPOS(LOWER(table_name), LOWER(@p0)) > 0"
            ),
            "{sql}"
        );
        assert!(sql.contains("(LOWER(s) > LOWER(@p1) OR"), "{sql}");
        assert!(
            sql.ends_with("AND k > @p9)))))))) ORDER BY LOWER(s), LOWER(n), s, n, k LIMIT 8"),
            "{sql}"
        );
        assert_eq!(b.0, ["x", "", "", "b", "b", "", "", "b", "b", "table"]);
    }

    #[test]
    fn pages_merge_and_follow_the_cursor() {
        let q = ObjectQuery {
            limit: 2,
            ..Default::default()
        };
        let found = vec![
            obj(kinds::TABLE, Some("b"), "x"),
            obj(kinds::SEQUENCE, Some("A"), "s"),
            obj(kinds::TABLE, Some("A"), "s"),
            obj(kinds::TABLE, None, "Z"),
            obj(kinds::VIEW, None, "a"),
        ];
        let p = merge(&q, None, found.clone());
        let got: Vec<_> = p.objects.iter().map(|o| o.name.as_str()).collect();
        assert_eq!(got, ["a", "Z"]);
        let mut q2 = q.clone();
        q2.after = p.next;
        let p2 = merge(&q2, after(&q2).as_ref(), found.clone());
        let got: Vec<_> = p2.objects.iter().map(|o| o.kind.as_str()).collect();
        assert_eq!(got, [kinds::SEQUENCE, kinds::TABLE]);
        q2.after = p2.next;
        let p3 = merge(&q2, after(&q2).as_ref(), found);
        assert_eq!(p3.objects.len(), 1);
        assert_eq!(p3.next, None);
    }
}
