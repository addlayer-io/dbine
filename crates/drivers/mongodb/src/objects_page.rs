//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): a page of the session database's collections and
//! views.
//!
//! One `listCollections` on the database with the query's filters as its
//! `filter` (MongoDB, FerretDB and DocumentDB take one on `name` and `type`
//! with `nameOnly`): the type for one kind, and a case-insensitive regular
//! expression for the search (`/text/i`) or the lookup (`/^text$/i`), the
//! text escaped. `listCollections` has no sort, limit or start, so what it
//! answers (this database's names that pass the filters, never another
//! database's) is put in the tree's order and paged here.
//!
//! - **Order:** name ignoring case, then as written, then the kind: what
//!   `objects::page_of` makes of `list_objects`.
//! - **Cursor:** the page's last object (`[schema, name, kind, parent]`);
//!   the next page starts after it (keyset), so pages neither skip nor
//!   repeat collections even when the database changes in between.

use crate::MongoSession;
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery, Result};
use mongodb::bson::{doc, Document};

/// The kinds `list_objects` gives, in its order.
const KINDS: &[&str] = &[kinds::COLLECTION, kinds::VIEW];

/// A `listCollections` entry as `list_objects` shows it (`None`: a
/// `system.*` collection).
pub(crate) fn object(d: &Document) -> Option<DbObject> {
    let name = d.get_str("name").ok()?;
    if name.starts_with("system.") {
        return None;
    }
    let kind = if d.get_str("type") == Ok("view") {
        kinds::VIEW
    } else {
        kinds::COLLECTION
    };
    Some(DbObject {
        kind: kind.into(),
        schema: None,
        name: name.into(),
        parent: None,
    })
}

/// `text` matched literally in a regular expression.
fn escape(text: &str) -> String {
    let mut out = String::with_capacity(text.len());
    for c in text.chars() {
        if "\\^$.|?*+()[]{}".contains(c) {
            out.push('\\');
        }
        out.push(c);
    }
    out
}

/// `listCollections`' filter for the query (the kinds it admits, of
/// `KINDS`): `None` when no collection can pass it.
fn filter(q: &ObjectQuery, wanted: &[&str]) -> Option<Document> {
    if wanted.is_empty() || (!q.no_schema && q.schema.is_some()) {
        return None;
    }
    let mut all = Vec::new();
    if wanted.len() < KINDS.len() {
        // Anything that isn't a view is a collection (time series too).
        all.push(if wanted == [kinds::VIEW] {
            doc! { "type": "view" }
        } else {
            doc! { "type": { "$ne": "view" } }
        });
    }
    if let Some(p) = q.pattern.as_deref().filter(|p| !p.is_empty()) {
        all.push(doc! { "name": { "$regex": escape(p), "$options": "i" } });
    }
    if let Some(n) = &q.name {
        all.push(doc! { "name": { "$regex": format!("^{}$", escape(n)), "$options": "i" } });
    }
    Some(match all.len() {
        0 => Document::new(),
        1 => all.remove(0),
        _ => doc! { "$and": all },
    })
}

impl MongoSession {
    /// A page of the database's collections and views: the same ones as
    /// `list_objects`, narrowed by the server (see the module's notes).
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        let Some(filter) = filter(q, &wanted(q)) else {
            return Ok(ObjectPage::default());
        };
        let cmd = doc! {
            "listCollections": 1,
            "filter": filter,
            "nameOnly": true,
            "authorizedCollections": true,
        };
        let docs = self.first_batch(cmd, usize::MAX).await?;
        Ok(page(q, docs.iter().filter_map(object).collect()))
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

    #[test]
    fn the_filter_asks_the_server_for_the_kind_and_the_name() {
        let with = |f: &dyn Fn(&mut ObjectQuery)| {
            let mut q = ObjectQuery::default();
            f(&mut q);
            filter(&q, &wanted(&q))
        };
        assert_eq!(with(&|_| {}), Some(Document::new()));
        assert_eq!(with(&|q| q.no_schema = true), Some(Document::new()));
        assert_eq!(
            with(&|q| q.kind = Some(kinds::VIEW.into())),
            Some(doc! { "type": "view" })
        );
        assert_eq!(
            with(&|q| q.not_kinds = vec![kinds::VIEW.into()]),
            Some(doc! { "type": { "$ne": "view" } })
        );
        assert_eq!(
            with(&|q| q.pattern = Some("a.b(1)*".into())),
            Some(doc! { "name": { "$regex": r"a\.b\(1\)\*", "$options": "i" } })
        );
        assert_eq!(
            with(&|q| q.pattern = Some(String::new())),
            Some(Document::new())
        );
        assert_eq!(
            with(&|q| {
                q.kind = Some(kinds::COLLECTION.into());
                q.name = Some("Pedidos$".into());
            }),
            Some(doc! { "$and": [
                { "type": { "$ne": "view" } },
                { "name": { "$regex": r"^Pedidos\$$", "$options": "i" } },
            ] })
        );
        // Nothing has a schema; nothing else is declared.
        assert_eq!(with(&|q| q.schema = Some("db".into())), None);
        assert_eq!(
            with(&|q| q.not_kinds = KINDS.iter().map(|k| k.to_string()).collect()),
            None
        );
        assert_eq!(with(&|q| q.kind = Some(kinds::TABLE.into())), None);
    }

    #[test]
    fn pages_follow_page_of() {
        let mut all: Vec<DbObject> = (0..25)
            .map(|i| {
                object(&doc! { "name": if i % 2 == 0 { format!("Pedidos_{i:02}") } else { format!("pedidos_{i:02}_x") }, "type": "collection" })
                    .unwrap()
            })
            .collect();
        for (name, ty) in [
            ("V_pedidos", "view"),
            ("a", "timeseries"),
            ("system.views", "collection"),
        ] {
            all.extend(object(&doc! { "name": name, "type": ty }));
        }
        assert_eq!(all.len(), 27, "system.* left out");
        assert_eq!(all[26].kind, kinds::COLLECTION, "time series");
        all.sort_by_key(|o| o.name.to_lowercase());
        for q in [
            ObjectQuery::default(),
            ObjectQuery {
                kind: Some(kinds::COLLECTION.into()),
                ..Default::default()
            },
            ObjectQuery {
                pattern: Some("PEDIDOS".into()),
                ..Default::default()
            },
            ObjectQuery {
                name: Some("v_PEDIDOS".into()),
                ..Default::default()
            },
        ] {
            let (mut mine, mut theirs) = (q.clone(), q.clone());
            mine.limit = 10;
            theirs.limit = 10;
            loop {
                let got = page(&mine, all.clone());
                let want = page_of(&all, &theirs);
                assert_eq!(got.objects, want.objects, "{q:?}");
                match (got.next, want.next) {
                    (Some(a), Some(b)) => {
                        mine.after = Some(a);
                        theirs.after = Some(b);
                    }
                    (None, None) => break,
                    other => panic!("{q:?}: {other:?}"),
                }
            }
        }
    }
}
