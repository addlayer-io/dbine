//! What the server already knows, for documenting a database
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `records` of each class in the database's metadata
//!   (`GET /database/{db}`), which the server takes from its clusters'
//!   record counts, kept by the storage: no class scan. The figure is
//!   polymorphic, as OrientDB counts a class: it includes the records of
//!   its subclasses.
//! - Comments: classes and their properties keep a description, and
//!   classes come with the schema; functions, sequences and indexes have
//!   none.

use crate::{classify, OrientSession};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{ObjectRef, Result};
use serde_json::Value;

/// `(name, kind, records)` of the user's classes in the metadata.
pub(crate) fn class_records(meta: &Value) -> Vec<(String, String, u64)> {
    classify(meta.get("classes").and_then(Value::as_array).map(Vec::as_slice).unwrap_or(&[]))
        .into_iter()
        .filter_map(|(name, kind, c)| Some((name, kind.to_string(), c.get("records").and_then(Value::as_u64)?)))
        .collect()
}

pub(crate) async fn row_estimates(s: &OrientSession) -> Result<Vec<RowEstimate>> {
    let meta = s.metadata().await?;
    Ok(class_records(&meta)
        .into_iter()
        .map(|(name, kind, rows)| RowEstimate { object: ObjectRef { kind, schema: None, name }, rows })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn records_per_class() {
        let meta = json!({ "classes": [
            { "name": "V", "records": 5 },
            { "name": "Person", "superClass": "V", "records": 3 },
            { "name": "Knows", "superClass": "E", "records": 2 },
            { "name": "Note", "records": 1 },
            { "name": "NoCount" },
            { "name": "OUser", "records": 4 }
        ]});
        let r = class_records(&meta);
        assert!(r.contains(&("Person".to_string(), crate::VERTEX.to_string(), 3)), "{r:?}");
        assert!(r.contains(&("Knows".to_string(), crate::EDGE.to_string(), 2)), "{r:?}");
        assert!(r.contains(&("Note".to_string(), dbine_driver::kinds::TABLE.to_string(), 1)), "{r:?}");
        assert!(!r.iter().any(|(n, ..)| n == "NoCount" || n == "OUser"), "{r:?}");
    }
}
