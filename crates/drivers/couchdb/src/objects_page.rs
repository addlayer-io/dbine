//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): a page of the session database's objects, its
//! documents (`_all_docs`) and the views of its design documents
//! (`ddoc/view`).
//!
//! The documents need no request; the views, the design documents'
//! bodies, and only those the query can reach: a lookup reads the ids
//! (`_design_docs`) and then the bodies of the design documents its name
//! can be in (`POST _design_docs?include_docs=true` with their `keys`); a
//! search or the views folder reads the database's design documents with
//! their bodies, as `list_objects` does. Never another database's.
//! `_design_docs` comes by id, case included, and can't search inside a
//! name, so the views are filtered and put in the tree's order here.
//!
//! - **Order:** name ignoring case, then as written, then the kind: what
//!   `objects::page_of` makes of `list_objects`.
//! - **Cursor:** the page's last object (`[schema, name, kind, parent]`);
//!   the next page starts after it (keyset), so pages neither skip nor
//!   repeat views even when the database changes in between.

use crate::{CouchSession, ALL_DOCS};
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery, Result};
use reqwest::Method;
use serde_json::{json, Value};

/// The kinds `list_objects` gives, in its order.
const KINDS: &[&str] = &[kinds::COLLECTION, kinds::VIEW];

/// The pseudo-collection of every document.
pub(crate) fn all_docs() -> DbObject {
    DbObject {
        kind: kinds::COLLECTION.into(),
        schema: None,
        name: ALL_DOCS.into(),
        parent: None,
    }
}

/// The views of `_design_docs?include_docs=true`'s rows, as
/// `ddoc/view` with the design document as the parent.
pub(crate) fn views(r: &Value) -> Vec<DbObject> {
    let mut views = Vec::new();
    for row in r
        .get("rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
    {
        let Some(doc) = row.get("doc").filter(|d| d.is_object()) else {
            continue;
        };
        let ddoc = doc
            .get("_id")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim_start_matches("_design/");
        for v in doc
            .get("views")
            .and_then(Value::as_object)
            .map(|m| m.keys().collect::<Vec<_>>())
            .unwrap_or_default()
        {
            views.push(DbObject {
                kind: kinds::VIEW.into(),
                schema: None,
                name: format!("{ddoc}/{v}"),
                parent: Some(ddoc.to_string()),
            });
        }
    }
    views
}

/// The design documents (their ids, `_design/…`) whose views a lookup of
/// `name` can find: those whose name, ignoring case, begins it with a `/`.
fn lookup_ids(ids: &Value, name: &str) -> Vec<String> {
    let name = name.to_lowercase();
    ids.get("rows")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or(&[])
        .iter()
        .filter_map(|r| r.get("id").and_then(Value::as_str))
        .filter(|id| {
            let ddoc = id.trim_start_matches("_design/").to_lowercase();
            name.strip_prefix(&ddoc)
                .is_some_and(|rest| rest.starts_with('/'))
        })
        .map(str::to_string)
        .collect()
}

impl CouchSession {
    /// A page of the database's objects: the same documents and views as
    /// `list_objects`, reading only the design documents the query needs
    /// (see the module's notes).
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        let db = self.db_path()?;
        // No object has a schema.
        if !q.no_schema && q.schema.is_some() {
            return Ok(ObjectPage::default());
        }
        let wanted = wanted(q);
        let mut found = Vec::new();
        if wanted.contains(&kinds::COLLECTION) {
            found.push(all_docs());
        }
        if wanted.contains(&kinds::VIEW) {
            let docs = match &q.name {
                Some(name) => {
                    let ids = self
                        .call(Method::GET, &format!("{db}/_design_docs"), None)
                        .await?;
                    let keys = lookup_ids(&ids, name);
                    if keys.is_empty() {
                        Value::Null
                    } else {
                        self.call(
                            Method::POST,
                            &format!("{db}/_design_docs?include_docs=true"),
                            Some(&json!({ "keys": keys })),
                        )
                        .await?
                    }
                }
                None => {
                    self.call(
                        Method::GET,
                        &format!("{db}/_design_docs?include_docs=true"),
                        None,
                    )
                    .await?
                }
            };
            found.extend(views(&docs));
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

    fn ddocs() -> Value {
        let mut rows: Vec<Value> = (0..25)
            .map(|i| {
                let id = if i % 2 == 0 {
                    format!("_design/Pedidos_{i:02}")
                } else {
                    format!("_design/pedidos_{i:02}")
                };
                json!({ "id": id, "doc": { "_id": id, "views": { "by_a": {}, "By_B": {} } } })
            })
            .collect();
        rows.push(json!({ "id": "_design/empty", "doc": { "_id": "_design/empty" } }));
        rows.push(json!({ "key": "_design/gone", "error": "not_found" }));
        json!({ "rows": rows })
    }

    #[test]
    fn views_and_the_design_documents_a_lookup_reads() {
        let v = views(&ddocs());
        assert_eq!(v.len(), 50);
        assert!(v
            .iter()
            .any(|o| o.name == "Pedidos_00/By_B" && o.parent.as_deref() == Some("Pedidos_00")));
        let ids = ddocs();
        assert_eq!(lookup_ids(&ids, "PEDIDOS_01/BY_A"), ["_design/pedidos_01"]);
        assert_eq!(lookup_ids(&ids, "pedidos_00/x"), ["_design/Pedidos_00"]);
        assert!(lookup_ids(&ids, "pedidos_0").is_empty());
        assert!(lookup_ids(&ids, "empty").is_empty());
        assert_eq!(lookup_ids(&ids, "Empty/"), ["_design/empty"]);
    }

    #[test]
    fn pages_follow_page_of() {
        let mut all = vec![all_docs()];
        let mut v = views(&ddocs());
        v.sort_by(|a, b| a.name.cmp(&b.name));
        all.extend(v);
        for q in [
            ObjectQuery::default(),
            ObjectQuery {
                kind: Some(kinds::VIEW.into()),
                ..Default::default()
            },
            ObjectQuery {
                pattern: Some("_b".into()),
                ..Default::default()
            },
            ObjectQuery {
                name: Some("PEDIDOS_03/by_a".into()),
                ..Default::default()
            },
            ObjectQuery {
                kind: Some(kinds::COLLECTION.into()),
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
