//! What the server already knows, for documenting a database
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `doc_count` of `GET /{db}` (the database's info, kept by the
//!   storage engine) for the `_all_docs` collection. Views keep no row
//!   count (`_info` only reports their index size), so they get none.
//! - Comments: CouchDB keeps none on databases, design documents or views.

use crate::{CouchSession, ALL_DOCS};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};
use reqwest::Method;
use serde_json::Value;

/// `doc_count` of a database's info (live documents, design documents
/// included, as `_all_docs` lists them).
pub(crate) fn doc_count(info: &Value) -> Option<u64> {
    info.get("doc_count").and_then(Value::as_u64)
}

pub(crate) async fn row_estimates(s: &CouchSession) -> Result<Vec<RowEstimate>> {
    let info = s.call(Method::GET, &s.db_path()?, None).await?;
    Ok(doc_count(&info)
        .map(|rows| RowEstimate { object: ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name: ALL_DOCS.into() }, rows })
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn doc_count_from_info() {
        assert_eq!(doc_count(&json!({ "db_name": "a", "doc_count": 3, "doc_del_count": 1 })), Some(3));
        assert_eq!(doc_count(&json!({ "error": "not_found" })), None);
    }
}
