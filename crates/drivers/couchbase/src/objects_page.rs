//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): a page of the bucket's collections, indexes and
//! functions.
//!
//! One N1QL query per catalog the query's kinds need, each narrowed and
//! cut on the server: `system:keyspaces` for the collections,
//! `system:indexes` for the indexes and `system:functions` for the
//! functions, with `list_objects`' conditions (the bucket, GSI indexes,
//! no `_system` scope), the query's (`bucket.scope`, `CONTAINS(LOWER(…))`,
//! `LOWER(…) =`), the rows after the cursor in the tree's order and one
//! more than the page (`ORDER BY LOWER(s), LOWER(n), s, n, p LIMIT`). The
//! catalogs' first rows are merged here. `system:keyspaces` has no row for
//! a bucket's default collection: the bucket's own row stands for it
//! (`_default._default`). Where `system:keyspaces` can't be
//! read, the collections come from the cluster manager's list of the
//! bucket's scopes, as in `list_objects`.
//!
//! - **Order:** schema (`bucket.scope`; functions have none and come
//!   first), then name, both ignoring case, then as written, then the kind
//!   (collections, indexes, functions, `list_objects`' order) and the
//!   collection of an index: what `objects::page_of` makes of
//!   `list_objects`.
//! - **Cursor:** the page's last object (`[schema, name, kind, parent]`);
//!   each catalog's next query asks for what sorts after it (keyset), so
//!   pages neither skip nor repeat objects even when the bucket changes in
//!   between.

use crate::{encode, text, CbSession};
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery, Result};
use serde_json::Value;

/// The kinds `list_objects` gives, in its order.
const KINDS: &[&str] = &[kinds::COLLECTION, kinds::INDEX, kinds::FUNCTION];

/// `s` as a N1QL string.
fn lit(s: &str) -> String {
    serde_json::to_string(s).unwrap_or_default()
}

/// A catalog's rows as objects of one kind: N1QL expressions for the
/// schema, the name and the parent (`""` for none).
struct Source {
    kind: &'static str,
    schema: String,
    name: String,
    parent: String,
    /// From `FROM` to a `WHERE` with `list_objects`' conditions.
    from: String,
}

impl Source {
    fn has_schema(&self) -> bool {
        self.kind != kinds::FUNCTION
    }

    /// The bucket's catalogs, those of the kinds the query admits.
    fn of(bucket: &str, wanted: &[&str]) -> Vec<Source> {
        let b = lit(bucket);
        let mut v = Vec::new();
        if wanted.contains(&kinds::COLLECTION) {
            // The bucket's own row (no `bucket`, no `scope`) stands for its
            // default collection, `_default._default`.
            let scope = "IFMISSING(k.`scope`, \"_default\")";
            v.push(Source {
                kind: kinds::COLLECTION,
                schema: format!("({b} || \".\" || {scope})"),
                name: "(CASE WHEN k.`bucket` IS MISSING THEN \"_default\" ELSE k.name END)".into(),
                parent: "\"\"".into(),
                from: format!(
                    "system:keyspaces AS k WHERE (k.`bucket` = {b} OR (k.`bucket` IS MISSING \
                     AND k.name = {b} AND k.namespace_id = \"default\")) AND POSITION({scope}, \"_system\") != 0"
                ),
            });
        }
        if wanted.contains(&kinds::INDEX) {
            // An index of the bucket's default collection from before
            // collections has no bucket_id: `_default._default`.
            let scope = "(CASE WHEN i.bucket_id IS MISSING THEN \"_default\" ELSE i.scope_id END)";
            v.push(Source {
                kind: kinds::INDEX,
                schema: format!("({b} || \".\" || {scope})"),
                name: "i.name".into(),
                parent: "(CASE WHEN i.bucket_id IS MISSING THEN \"_default\" ELSE i.keyspace_id END)"
                    .into(),
                from: format!(
                    "system:indexes AS i WHERE (i.bucket_id = {b} OR (i.bucket_id IS MISSING AND i.keyspace_id = {b})) \
                     AND i.`using` = \"gsi\" AND POSITION({scope}, \"_system\") != 0"
                ),
            });
        }
        if wanted.contains(&kinds::FUNCTION) {
            v.push(Source {
                kind: kinds::FUNCTION,
                schema: "\"\"".into(),
                name: "f.identity.name".into(),
                parent: "\"\"".into(),
                from: format!(
                    "system:functions AS f WHERE (f.identity.`bucket` = {b} OR f.identity.type = \"global\")"
                ),
            });
        }
        v
    }

    /// The query for this catalog's rows (`s`, `n`, `p`) of the page:
    /// those the query admits, after the cursor, in the tree's order, one
    /// more than the page. `None` when it can have none.
    fn query(&self, q: &ObjectQuery, after: Option<&Key>) -> Option<String> {
        let mut sql = format!(
            "SELECT {} AS s, {} AS n, {} AS p FROM {}",
            self.schema, self.name, self.parent, self.from
        );
        if self.has_schema() {
            if q.no_schema {
                return None;
            }
            if let Some(s) = &q.schema {
                sql += &format!(" AND {} = {}", self.schema, lit(s));
            }
        } else if !q.no_schema && q.schema.is_some() {
            return None;
        }
        if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
            sql += &format!(
                " AND CONTAINS(LOWER({}), {})",
                self.name,
                lit(&p.to_lowercase())
            );
        }
        if let Some(n) = &q.name {
            sql += &format!(" AND LOWER({}) = {}", self.name, lit(&n.to_lowercase()));
        }
        if let Some(k) = after {
            if self.has_schema() != k.0 {
                // Objects without a schema come first.
                if !self.has_schema() {
                    return None;
                }
            } else {
                let mut cols = vec![
                    (format!("LOWER({})", self.schema), lit(&k.1)),
                    (format!("LOWER({})", self.name), lit(&k.2)),
                    (self.schema.clone(), lit(&k.3)),
                    (self.name.clone(), lit(&k.4)),
                ];
                let rank = rank(self.kind);
                if rank == k.5 {
                    cols.push((self.parent.clone(), lit(&k.6)));
                }
                // Past the cursor; a kind after the cursor's from its name on.
                sql += &format!(" AND {}", past(&cols, rank <= k.5));
            }
        }
        sql += &format!(
            " ORDER BY LOWER({s}), LOWER({n}), {s}, {n}, {p} LIMIT {}",
            q.page_size() + 1,
            s = self.schema,
            n = self.name,
            p = self.parent,
        );
        Some(sql)
    }

    /// A row of its query as an object.
    fn object(&self, row: &Value) -> Option<DbObject> {
        let at = |k: &str| row.get(k).map(text);
        let name = at("n")?;
        Some(DbObject {
            kind: self.kind.into(),
            schema: self.has_schema().then(|| at("s")).flatten(),
            name,
            parent: (self.kind == kinds::INDEX).then(|| at("p")).flatten(),
        })
    }
}

/// `(c1, …, cN) > (v1, …, vN)` (`>=` when not `strict`), column by column.
fn past(cols: &[(String, String)], strict: bool) -> String {
    let (c, v) = &cols[0];
    if cols.len() == 1 {
        return format!("{c} {} {v}", if strict { ">" } else { ">=" });
    }
    format!(
        "({c} > {v} OR ({c} = {v} AND {}))",
        past(&cols[1..], strict)
    )
}

impl CbSession {
    /// A page of the bucket's objects: the same collections, indexes and
    /// functions as `list_objects`, a few rows of each catalog (see the
    /// module's notes).
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        let b = self.bucket()?;
        let from = after(q).as_ref().map(key);
        let mut found = Vec::new();
        for src in Source::of(&b, &wanted(q)) {
            let Some(sql) = src.query(q, from.as_ref()) else {
                continue;
            };
            match self.results(&sql).await {
                Ok(rows) => found.extend(rows.iter().filter_map(|r| src.object(r))),
                // Without system:keyspaces, the cluster manager's list.
                Err(e) if src.kind == kinds::COLLECTION => match self.rest_collections(&b).await {
                    Ok(all) => found.extend(all),
                    Err(_) => return Err(e),
                },
                // Indexes and functions that can't be read aren't listed,
                // as in `list_objects`.
                Err(_) => {}
            }
        }
        Ok(page(q, found))
    }

    /// The bucket's collections from the cluster manager (`_system`'s
    /// left out).
    pub(crate) async fn rest_collections(&self, b: &str) -> Result<Vec<DbObject>> {
        let v = self
            .conn
            .mgmt_get(&format!("/pools/default/buckets/{}/scopes", encode(b)))
            .await?;
        let mut out = Vec::new();
        for s in v
            .get("scopes")
            .and_then(Value::as_array)
            .into_iter()
            .flatten()
        {
            let scope = s.get("name").map(text).unwrap_or_default();
            if scope.starts_with("_system") {
                continue;
            }
            for c in s
                .get("collections")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                out.push(DbObject {
                    kind: kinds::COLLECTION.into(),
                    schema: Some(format!("{b}.{scope}")),
                    name: c.get("name").map(text).unwrap_or_default(),
                    parent: None,
                });
            }
        }
        Ok(out)
    }
}

// -- the tree's order and cursor ------------------------------------------------------

/// `objects::page_of`'s order (schema, none first, then name, both
/// ignoring case, then as written), then the kind in `list_objects`' order
/// and the parent: what `page_of`'s stable sort keeps of `list_objects`
/// among equal names.
type Key = (bool, String, String, String, String, usize, String);

fn rank(kind: &str) -> usize {
    KINDS.iter().position(|k| *k == kind).unwrap_or(KINDS.len())
}

fn key(o: &DbObject) -> Key {
    let s = o.schema.clone().unwrap_or_default();
    (
        o.schema.is_some(),
        s.to_lowercase(),
        o.name.to_lowercase(),
        s,
        o.name.clone(),
        rank(&o.kind),
        o.parent.clone().unwrap_or_default(),
    )
}

/// The previous page's last object, from the cursor. One this driver
/// didn't write starts at the top, as `page_of` does.
fn after(q: &ObjectQuery) -> Option<DbObject> {
    let (schema, name, kind, parent) = serde_json::from_str(q.after.as_deref()?).ok()?;
    Some(DbObject {
        kind,
        schema,
        name,
        parent,
    })
}

fn cursor(o: &DbObject) -> String {
    serde_json::json!([o.schema, o.name, o.kind, o.parent]).to_string()
}

/// The kinds the query admits.
fn wanted(q: &ObjectQuery) -> Vec<&'static str> {
    KINDS
        .iter()
        .copied()
        .filter(|k| {
            q.kind.as_deref().is_none_or(|w| w == *k) && !q.not_kinds.iter().any(|n| n == k)
        })
        .collect()
}

/// The page out of the objects found: those the query admits, after the
/// cursor, in the tree's order, once each, cut at the page size.
fn page(q: &ObjectQuery, found: Vec<DbObject>) -> ObjectPage {
    let from = after(q).as_ref().map(key);
    let mut found: Vec<(Key, DbObject)> = found
        .into_iter()
        .filter(|o| q.matches(o))
        .map(|o| (key(&o), o))
        .filter(|(k, _)| from.as_ref().is_none_or(|f| k > f))
        .collect();
    found.sort_by(|a, b| a.0.cmp(&b.0));
    found.dedup_by(|a, b| a.0 == b.0);
    let size = q.page_size();
    let next = (found.len() > size).then(|| cursor(&found[size - 1].1));
    found.truncate(size);
    ObjectPage {
        objects: found.into_iter().map(|(_, o)| o).collect(),
        next,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn each_catalog_asks_for_its_rows_after_the_cursor() {
        let srcs = Source::of("b\"x", &[kinds::COLLECTION, kinds::INDEX, kinds::FUNCTION]);
        assert_eq!(srcs.len(), 3);
        let ix = DbObject {
            kind: kinds::INDEX.into(),
            schema: Some("b\"x.Ventas".into()),
            name: "Ix_1".into(),
            parent: Some("pedidos".into()),
        };
        let q = ObjectQuery {
            pattern: Some("IX".into()),
            limit: 10,
            ..Default::default()
        };
        let k = key(&ix);
        // Collections sort before an index of the same name: theirs after
        // its name; indexes after it and its collection.
        let sql = srcs[0].query(&q, Some(&k)).unwrap();
        let (scope, name) = (
            "IFMISSING(k.`scope`, \"_default\")",
            "(CASE WHEN k.`bucket` IS MISSING THEN \"_default\" ELSE k.name END)",
        );
        assert!(sql.starts_with(&format!("SELECT (\"b\\\"x\" || \".\" || {scope}) AS s, {name} AS n, \"\" AS p FROM system:keyspaces AS k WHERE (k.`bucket` = \"b\\\"x\" OR (k.`bucket` IS MISSING AND k.name = \"b\\\"x\" AND")), "{sql}");
        assert!(
            sql.contains(&format!(" AND CONTAINS(LOWER({name}), \"ix\")")),
            "{sql}"
        );
        assert!(sql.contains(&format!(" {name} > \"Ix_1\")")), "{sql}");
        assert!(sql.ends_with("LIMIT 11"), "{sql}");
        let sql = srcs[1].query(&q, Some(&k)).unwrap();
        assert!(sql.contains("i.keyspace_id END) > \"pedidos\")"), "{sql}");
        let fns = |q: &ObjectQuery, k: Option<&Key>| srcs[2].query(q, k);
        // Functions (no schema) come before every collection and index.
        assert_eq!(fns(&q, Some(&k)), None);
        assert!(fns(&q, None)
            .unwrap()
            .contains("CONTAINS(LOWER(f.identity.name), \"ix\")"));
        // A schema has no functions; objects without one, no collections.
        let schema = ObjectQuery {
            schema: Some("b.s".into()),
            ..Default::default()
        };
        assert_eq!(fns(&schema, None), None);
        assert!(srcs[0]
            .query(&schema, None)
            .unwrap()
            .contains(&format!(" AND (\"b\\\"x\" || \".\" || {scope}) = \"b.s\"")));
        let loose = ObjectQuery {
            no_schema: true,
            name: Some("F".into()),
            ..Default::default()
        };
        assert_eq!(srcs[0].query(&loose, None), None);
        assert!(fns(&loose, None)
            .unwrap()
            .contains(" AND LOWER(f.identity.name) = \"f\""));
    }

    #[test]
    fn the_cursor_comparison() {
        let cols = vec![("a".to_string(), "1".to_string()), ("b".into(), "2".into())];
        assert_eq!(past(&cols, true), "(a > 1 OR (a = 1 AND b > 2))");
        assert_eq!(past(&cols, false), "(a > 1 OR (a = 1 AND b >= 2))");
        assert_eq!(
            wanted(&ObjectQuery {
                not_kinds: vec![kinds::INDEX.into()],
                ..Default::default()
            }),
            [kinds::COLLECTION, kinds::FUNCTION]
        );
    }
}
