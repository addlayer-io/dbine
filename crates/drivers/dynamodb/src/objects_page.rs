//! `list_objects_page` (see `dbine_driver::objects` and docs/drivers.md,
//! "Objects by page"): a page of the region's tables and secondary
//! indexes.
//!
//! `list_objects` reads every table's `DescribeTable` for its indexes; a
//! page reads only what it shows:
//!
//! - **Tables:** the names from `ListTables` (100 a call, following
//!   `LastEvaluatedTableName` as `ExclusiveStartTableName`), and no
//!   `DescribeTable`. `ListTables` filters nothing and orders by the bytes
//!   of the name (`B` before `a`), not the tree's case-insensitive order, so
//!   its start can't be the tree's cursor: the names are filtered and
//!   ordered here, a page cut after the cursor.
//! - **One table's indexes** (`schema`: the table): its `DescribeTable`.
//! - **Every table's indexes** (the indexes folder): the tables in the
//!   tree's order from the cursor's, described a few at a time until the
//!   page is full (a page of tables needs none).
//! - **A search or a lookup among every table's indexes** would describe
//!   every table each time: it answers `Unsupported`, and the app pages
//!   its whole list (read once and kept a while) as before.
//!
//! - **Order:** tables (no schema) by name ignoring case, then as written;
//!   then indexes by table and by name, the same way: what
//!   `objects::page_of` makes of `list_objects`.
//! - **Cursor:** the page's last object (`[schema, name, kind, parent]`);
//!   the next page starts after it (keyset), so pages neither skip nor
//!   repeat objects even when tables come and go in between.

use crate::{key_order, DynamoSession};
use aws_sdk_dynamodb::types::TableDescription;
use dbine_driver::{kinds, DbObject, Error, ObjectPage, ObjectQuery, Result};
use futures::stream::{self, StreamExt};

/// The kinds `list_objects` gives, in its order.
const KINDS: &[&str] = &[kinds::TABLE, kinds::INDEX];

/// A table as `list_objects` shows it.
pub(crate) fn table(name: &str) -> DbObject {
    DbObject {
        kind: kinds::TABLE.into(),
        schema: None,
        name: name.into(),
        parent: None,
    }
}

/// A table's secondary indexes (global, then local) as `list_objects`
/// shows them: the table rides in `schema` so `ObjectRef` finds it back.
pub(crate) fn indexes(table: &str, d: &TableDescription) -> Vec<DbObject> {
    d.global_secondary_indexes()
        .iter()
        .filter_map(|i| i.index_name())
        .chain(
            d.local_secondary_indexes()
                .iter()
                .filter_map(|i| i.index_name()),
        )
        .map(|name| DbObject {
            kind: kinds::INDEX.into(),
            schema: Some(table.to_string()),
            name: name.to_string(),
            parent: Some(table.to_string()),
        })
        .collect()
}

impl DynamoSession {
    /// A page of the tables and indexes, describing only the tables it
    /// needs (see the module's notes).
    pub(crate) async fn objects_page(&mut self, q: &ObjectQuery) -> Result<ObjectPage> {
        let wanted = wanted(q);
        let tables = wanted.contains(&kinds::TABLE) && (q.no_schema || q.schema.is_none());
        let with_indexes = wanted.contains(&kinds::INDEX) && !q.no_schema;
        if !tables && !with_indexes {
            return Ok(ObjectPage::default());
        }
        let search = q.pattern.as_deref().is_some_and(|p| !p.is_empty()) || q.name.is_some();
        if with_indexes && q.schema.is_none() && search {
            return Err(Error::Unsupported(
                "este motor todavía no lista sus objetos por páginas".into(),
            ));
        }
        // One table's indexes.
        if let Some(t) = q.schema.as_deref().filter(|_| !q.no_schema) {
            let found = match self.describe_quietly(t).await {
                Some(d) => {
                    self.keys.insert(t.to_string(), key_order(d.key_schema()));
                    indexes(t, &d)
                }
                None => Vec::new(),
            };
            return Ok(page(q, found));
        }
        let names = self.table_names().await?;
        let from = after(q).as_ref().map(key);
        let past = |o: &DbObject| q.matches(o) && from.as_ref().is_none_or(|f| key(o) > *f);
        let mut found: Vec<DbObject> = Vec::new();
        if tables {
            found.extend(names.iter().map(|n| table(n)));
        }
        let size = q.page_size();
        let shown = found.iter().filter(|o| past(o)).count();
        if with_indexes && shown <= size {
            // The tables in the tree's order, from the cursor's on (an
            // index's table; a table cursor is before every index).
            let start = from
                .as_ref()
                .filter(|f| f.0)
                .map(|f| (f.1.clone(), f.3.clone()));
            let mut order: Vec<(String, String)> = names
                .iter()
                .map(|n| (n.to_lowercase(), n.clone()))
                .filter(|t| start.as_ref().is_none_or(|s| t >= s))
                .collect();
            order.sort();
            let order: Vec<String> = order.into_iter().map(|(_, n)| n).collect();
            let mut need = size + 1 - shown;
            let mut keys = Vec::new();
            {
                let me = &*self;
                let mut described = stream::iter(order)
                    .map(|t| async move {
                        let d = me.describe_quietly(&t).await;
                        (t, d)
                    })
                    .buffered(8);
                while let Some((t, d)) = described.next().await {
                    let Some(d) = d else { continue };
                    keys.push((t.clone(), key_order(d.key_schema())));
                    for ix in indexes(&t, &d) {
                        if past(&ix) {
                            need = need.saturating_sub(1);
                        }
                        found.push(ix);
                    }
                    // Whole tables only: the page's last index and the next.
                    if need == 0 {
                        break;
                    }
                }
            }
            self.keys.extend(keys);
        }
        Ok(page(q, found))
    }

    /// `DescribeTable`, `None` where it fails (as `list_objects`, which
    /// shows such a table without indexes).
    async fn describe_quietly(&self, table: &str) -> Option<TableDescription> {
        self.client
            .describe_table()
            .table_name(table)
            .send()
            .await
            .ok()
            .and_then(|o| o.table)
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

    #[test]
    fn the_kinds_and_the_cursor() {
        let q = ObjectQuery {
            not_kinds: vec![kinds::TABLE.into()],
            ..Default::default()
        };
        assert_eq!(wanted(&q), [kinds::INDEX]);
        let ix = DbObject {
            kind: kinds::INDEX.into(),
            schema: Some("Pedidos".into()),
            name: "porFecha".into(),
            parent: Some("Pedidos".into()),
        };
        let q = ObjectQuery {
            after: Some(cursor(&ix)),
            ..Default::default()
        };
        assert_eq!(after(&q), Some(ix.clone()));
        // Tables (no schema) before every index, ignoring case.
        assert!(key(&table("zeta")) < key(&ix));
        assert!(key(&table("B")) > key(&table("a")));
    }
}
