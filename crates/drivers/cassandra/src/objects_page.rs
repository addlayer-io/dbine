//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): a page of the session keyspace's objects.
//!
//! The objects live in `system_schema`, whose tables are partitioned by
//! `keyspace_name`: each page reads one partition (the keyspace) of only
//! the tables the query's kinds need (`tables` for the tables folder,
//! `types` for the types…), never another keyspace. CQL can't compare
//! ignoring case or search inside a name, and the partition comes in
//! `table_name` order, case included, so the names are filtered and put
//! in the tree's order here.
//!
//! - **Order:** name ignoring case, then as written, then the kind in the
//!   order `list_objects` gives them (tables, materialized views, types,
//!   functions): what `objects::page_of` makes of `list_objects`.
//! - **Cursor:** the page's last object (`[schema, name, kind, parent]`);
//!   the next page starts after it (keyset), so pages neither skip nor
//!   repeat objects even when the keyspace changes in between.

use crate::{text, CassandraSession, TYPE};
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery, Result};

/// The kinds `list_objects` gives, in its order.
const KINDS: &[&str] = &[
    kinds::TABLE,
    kinds::MATERIALIZED_VIEW,
    TYPE,
    kinds::FUNCTION,
];

impl CassandraSession {
    /// A page of the keyspace's objects: the same tables, materialized
    /// views (not Scylla's index views), types and functions as
    /// `list_objects`, reading only the kinds the query admits.
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        let Some(ks) = self.keyspace.clone() else {
            return Ok(ObjectPage::default());
        };
        // No object has a schema.
        if !q.no_schema && q.schema.is_some() {
            return Ok(ObjectPage::default());
        }
        let obj = |kind: &str, name: String, parent: Option<String>| DbObject {
            kind: kind.into(),
            schema: None,
            name,
            parent,
        };
        let wanted = wanted(q);
        let mut found = Vec::new();
        if wanted.contains(&kinds::TABLE) {
            for r in self
                .rows(
                    "SELECT table_name FROM system_schema.tables WHERE keyspace_name = ?",
                    (&ks,),
                )
                .await?
            {
                found.push(obj(kinds::TABLE, text(&r, 0), None));
            }
        }
        if wanted.contains(&kinds::MATERIALIZED_VIEW) {
            // Scylla keeps secondary indexes as views too; those aren't the user's.
            let indexes: Vec<String> = self
                .rows(
                    "SELECT index_name FROM system_schema.indexes WHERE keyspace_name = ?",
                    (&ks,),
                )
                .await
                .map(|rows| {
                    rows.iter()
                        .map(|r| format!("{}_index", text(r, 0)))
                        .collect()
                })
                .unwrap_or_default();
            for r in self
                .rows(
                    "SELECT view_name, base_table_name FROM system_schema.views WHERE keyspace_name = ?",
                    (&ks,),
                )
                .await?
            {
                let name = text(&r, 0);
                if !indexes.contains(&name) {
                    found.push(obj(kinds::MATERIALIZED_VIEW, name, Some(text(&r, 1))));
                }
            }
        }
        if wanted.contains(&TYPE) {
            for r in self
                .rows(
                    "SELECT type_name FROM system_schema.types WHERE keyspace_name = ?",
                    (&ks,),
                )
                .await?
            {
                found.push(obj(TYPE, text(&r, 0), None));
            }
        }
        if wanted.contains(&kinds::FUNCTION) {
            // One per name (the overloads come together, by name).
            for r in self
                .rows(
                    "SELECT function_name FROM system_schema.functions WHERE keyspace_name = ?",
                    (&ks,),
                )
                .await?
            {
                found.push(obj(kinds::FUNCTION, text(&r, 0), None));
            }
        }
        Ok(page(q, found))
    }
}

// -- the tree's order and cursor ------------------------------------------------------

/// `objects::page_of`'s order (schema, none first, then name, both
/// ignoring case, then as written), then the kind in `list_objects`' order
/// and the parent: what `page_of`'s stable sort keeps of `list_objects`
/// among equal names.
type Key = (bool, String, String, String, String, usize, String);

fn key(o: &DbObject) -> Key {
    let s = o.schema.clone().unwrap_or_default();
    (
        o.schema.is_some(),
        s.to_lowercase(),
        o.name.to_lowercase(),
        s,
        o.name.clone(),
        KINDS
            .iter()
            .position(|k| *k == o.kind)
            .unwrap_or(KINDS.len()),
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
    use dbine_driver::objects::page_of;

    fn obj(kind: &str, name: &str) -> DbObject {
        DbObject {
            kind: kind.into(),
            schema: None,
            name: name.into(),
            parent: None,
        }
    }

    /// `list_objects`' shape: sorted by name as written, functions once.
    fn whole() -> Vec<DbObject> {
        let mut v: Vec<DbObject> = (0..23)
            .map(|i| {
                obj(
                    kinds::TABLE,
                    &if i % 2 == 0 {
                        format!("Events_{i:02}")
                    } else {
                        format!("events_{i:02}_x")
                    },
                )
            })
            .collect();
        v.extend([
            obj(kinds::TABLE, "dup"),
            obj(TYPE, "dup"),
            obj(kinds::FUNCTION, "dup"),
            obj(kinds::MATERIALIZED_VIEW, "Dup"),
            obj(TYPE, "addr"),
            obj(kinds::FUNCTION, "f_%_x"),
        ]);
        v.sort_by(|a, b| a.name.cmp(&b.name));
        v
    }

    /// Every page of `q`, paged here and by `page_of`: the same objects in
    /// the same pages.
    fn same_pages(all: &[DbObject], q: ObjectQuery) -> usize {
        let (mut mine, mut theirs) = (q.clone(), q.clone());
        let mut pages = 0;
        loop {
            let got = page(&mine, all.to_vec());
            let want = page_of(all, &theirs);
            assert_eq!(got.objects, want.objects, "page {pages} of {q:?}");
            assert_eq!(got.next.is_some(), want.next.is_some(), "{q:?}");
            pages += 1;
            match (got.next, want.next) {
                (Some(a), Some(b)) => {
                    mine.after = Some(a);
                    theirs.after = Some(b);
                }
                _ => return pages,
            }
        }
    }

    #[test]
    fn pages_follow_page_of_including_ties_between_kinds() {
        let all = whole();
        let with = |f: &dyn Fn(&mut ObjectQuery)| {
            let mut q = ObjectQuery {
                limit: 4,
                ..Default::default()
            };
            f(&mut q);
            q
        };
        let cases = [
            with(&|_| {}),
            with(&|q| q.kind = Some(kinds::TABLE.into())),
            with(&|q| q.no_schema = true),
            with(&|q| q.schema = Some("ks".into())),
            with(&|q| q.pattern = Some("EVENTS".into())),
            with(&|q| q.pattern = Some("%".into())),
            with(&|q| q.name = Some("DUP".into())),
            with(&|q| {
                q.name = Some("dup".into());
                q.limit = 1;
            }),
            with(&|q| q.not_kinds = KINDS.iter().map(|k| k.to_string()).collect()),
            with(&|q| q.not_kinds = vec![kinds::TABLE.into()]),
            with(&|q| q.limit = 0),
        ];
        let mut longest = 0;
        for q in cases {
            longest = longest.max(same_pages(&all, q));
        }
        assert_eq!(longest, 8, "29 objects by 4");
    }

    #[test]
    fn the_kinds_asked_for_and_a_foreign_cursor() {
        let q = ObjectQuery {
            kind: Some(TYPE.into()),
            not_kinds: vec![kinds::FUNCTION.into()],
            ..Default::default()
        };
        assert_eq!(wanted(&q), [TYPE]);
        let q = ObjectQuery {
            not_kinds: vec![kinds::TABLE.into(), TYPE.into()],
            ..Default::default()
        };
        assert_eq!(wanted(&q), [kinds::MATERIALIZED_VIEW, kinds::FUNCTION]);
        // An offset (the app's own cursor) starts at the top.
        let q = ObjectQuery {
            after: Some("3".into()),
            limit: 2,
            ..Default::default()
        };
        let p = page(&q, whole());
        assert_eq!(p.objects[0].name, "addr");
        assert_eq!(
            p.next.as_deref(),
            Some(r#"[null,"Dup","materialized_view",null]"#),
            "{p:?}"
        );
    }
}
