//! What the catalog already knows, for documenting a database
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `estimatedDocumentCount` per collection, which answers from the
//!   collection's metadata (the `count` command without a filter), never a
//!   scan. Views have no metadata count, so they're left out; a collection
//!   the user can't count is left out too.
//! - Comments: MongoDB keeps none on collections, views or indexes (the
//!   `comment` option belongs to operations), so there are none.

use crate::MongoSession;
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};
use mongodb::bson::{doc, Document};

/// The collections of a `listCollections` reply that keep a document
/// count: real collections (not views) outside the `system.` namespace.
pub(crate) fn countable(specs: &[Document]) -> Vec<String> {
    specs
        .iter()
        .filter_map(|d| {
            let name = d.get_str("name").ok()?;
            let kind = d.get_str("type").unwrap_or("collection");
            (kind == "collection" && !name.starts_with("system.")).then(|| name.to_string())
        })
        .collect()
}

pub(crate) async fn row_estimates(s: &MongoSession) -> Result<Vec<RowEstimate>> {
    let cmd = doc! { "listCollections": 1, "nameOnly": true, "authorizedCollections": true };
    let specs = s.first_batch(cmd, usize::MAX).await?;
    let mut out = Vec::new();
    for name in countable(&specs) {
        let coll = s.db.collection::<Document>(&name);
        if let Ok(n) = coll.estimated_document_count().await {
            out.push(RowEstimate { object: ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name }, rows: n });
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn countable_skips_views_and_system_collections() {
        let specs = vec![
            doc! { "name": "orders", "type": "collection" },
            doc! { "name": "orders_by_city", "type": "view" },
            doc! { "name": "system.views", "type": "collection" },
            doc! { "name": "legacy" },
            doc! { "name": "metrics", "type": "timeseries" },
        ];
        assert_eq!(countable(&specs), vec!["orders".to_string(), "legacy".to_string()]);
    }
}
