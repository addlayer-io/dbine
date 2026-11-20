//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): a page of the session database's containers.
//!
//! A search or a lookup queries the database's containers
//! (`POST /dbs/{db}/colls`, `x-ms-documentdb-isquery`) with the name as a
//! bound parameter: `SELECT * FROM root r WHERE CONTAINS(LOWER(r.id), @v)`
//! or `LOWER(r.id) = @v`. The folder reads the database's container feed,
//! as `list_objects`. Never another database's. Neither the feed nor the
//! query orders by name ignoring case (an `ORDER BY` is by the bytes, and
//! the metadata query may not take one), so what they answer is filtered
//! again, put in the tree's order and cut here. A gateway that refuses the
//! query gets the feed; the vNext emulator answers it with every container.
//!
//! - **Order:** name ignoring case, then as written: what
//!   `objects::page_of` makes of `list_objects`.
//! - **Cursor:** the page's last container (`[schema, name, kind,
//!   parent]`); the next page starts after it (keyset), so pages neither
//!   skip nor repeat containers even when the database changes in between.

use crate::CosmosSession;
use dbine_driver::{kinds, DbObject, ObjectPage, ObjectQuery, Result};
use reqwest::Method;
use serde_json::{json, Value};

/// The kinds `list_objects` gives, in its order.
const KINDS: &[&str] = &[kinds::COLLECTION];

/// A container as `list_objects` shows it.
pub(crate) fn container(name: String) -> DbObject {
    DbObject {
        kind: kinds::COLLECTION.into(),
        schema: None,
        name,
        parent: None,
    }
}

/// The containers query for a search or a lookup (`None`: the folder,
/// the whole feed).
fn filter(q: &ObjectQuery) -> Option<Value> {
    let (cond, value) = match (&q.name, q.pattern.as_deref().filter(|p| !p.is_empty())) {
        (Some(n), _) => ("LOWER(r.id) = @v", n),
        (None, Some(p)) => ("CONTAINS(LOWER(r.id), @v)", &p.to_string()),
        (None, None) => return None,
    };
    Some(json!({
        "query": format!("SELECT * FROM root r WHERE {cond}"),
        "parameters": [{ "name": "@v", "value": value.to_lowercase() }],
    }))
}

impl CosmosSession {
    /// A page of the database's containers: the same ones as
    /// `list_objects`, a search or a lookup narrowed by the server (see the
    /// module's notes).
    pub(crate) async fn objects_page(&self, q: &ObjectQuery) -> Result<ObjectPage> {
        let link = self.db_link()?;
        // No container has a schema; nothing else is listed.
        if (!q.no_schema && q.schema.is_some()) || wanted(q).is_empty() {
            return Ok(ObjectPage::default());
        }
        let names = match filter(q) {
            None => self.containers().await?,
            Some(body) => match self.query_containers(&link, &body).await {
                Ok(names) => names,
                Err(e) => {
                    tracing::info!("cosmosdb: containers query refused, listing them: {e}");
                    self.containers().await?
                }
            },
        };
        Ok(page(q, names.into_iter().map(container).collect()))
    }

    /// The ids of the containers `body` (a query) finds, following its
    /// continuations.
    async fn query_containers(&self, link: &str, body: &Value) -> Result<Vec<String>> {
        let path = format!("{}/colls", self.db_path()?);
        let mut out = Vec::new();
        let mut cont: Option<String> = None;
        loop {
            let mut h = vec![
                ("x-ms-documentdb-isquery", "True".to_string()),
                ("Content-Type", "application/query+json".to_string()),
            ];
            if let Some(c) = &cont {
                h.push(("x-ms-continuation", c.clone()));
            }
            let r = self
                .call(Method::POST, "colls", link, &path, Some(body), &h)
                .await?;
            out.extend(
                r.body
                    .get("DocumentCollections")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                    .filter_map(|c| c.get("id")?.as_str().map(str::to_string)),
            );
            match r.continuation {
                Some(c) if !c.is_empty() => cont = Some(c),
                _ => return Ok(out),
            }
        }
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
    fn searches_and_lookups_query_the_containers() {
        let with = |f: &dyn Fn(&mut ObjectQuery)| {
            let mut q = ObjectQuery::default();
            f(&mut q);
            filter(&q)
        };
        assert_eq!(with(&|_| {}), None);
        assert_eq!(with(&|q| q.pattern = Some(String::new())), None);
        assert_eq!(
            with(&|q| q.pattern = Some("Pedidos'".into())),
            Some(json!({
                "query": "SELECT * FROM root r WHERE CONTAINS(LOWER(r.id), @v)",
                "parameters": [{ "name": "@v", "value": "pedidos'" }],
            }))
        );
        assert_eq!(
            with(&|q| {
                q.pattern = Some("x".into());
                q.name = Some("Notas".into());
            }),
            Some(json!({
                "query": "SELECT * FROM root r WHERE LOWER(r.id) = @v",
                "parameters": [{ "name": "@v", "value": "notas" }],
            }))
        );
    }

    #[test]
    fn pages_follow_page_of() {
        let all: Vec<DbObject> = (0..25)
            .map(|i| {
                container(if i % 2 == 0 {
                    format!("Pedidos_{i:02}")
                } else {
                    format!("pedidos_{i:02}_x")
                })
            })
            .collect();
        for q in [
            ObjectQuery::default(),
            ObjectQuery {
                pattern: Some("_X".into()),
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
