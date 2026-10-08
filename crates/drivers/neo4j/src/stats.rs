//! What the engine already knows, for documenting a graph
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: nodes per label and relationships per type.
//!   - Neo4j: its count store, which keeps those totals up to date. The
//!     planner answers `MATCH (n:L) RETURN count(n)` and
//!     `MATCH ()-[r:T]->() RETURN count(r)` from it
//!     (`NodeCountFromCountStore` / `RelationshipCountFromCountStore`)
//!     without touching a node or a relationship, so only those two exact
//!     shapes are sent.
//!   - Memgraph: the `count` of `SHOW INDEX INFO` for label-only and
//!     edge-type-only indexes; a label without such an index has no figure
//!     (counting it would scan).
//!   - Neptune: nothing. Its statistics summary gives graph totals and the
//!     label names, not a count per label.
//! - Comments: none of the three keeps comments on labels, relationship
//!   types, indexes, constraints or procedures.

use crate::{as_text, cypher, Flavor, GraphSession, LABEL, RELATIONSHIP};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{ObjectRef, Result};
use serde_json::Value;

/// A count-store query for a label or relationship type.
pub(crate) fn count_store_query(rel: bool, name: &str) -> String {
    let n = cypher::ident(name);
    if rel {
        format!("MATCH ()-[r:{n}]->() RETURN count(r)")
    } else {
        format!("MATCH (n:{n}) RETURN count(n)")
    }
}

/// Memgraph's `SHOW INDEX INFO` rows → `(relationship?, name, count)` for
/// the indexes on a whole label or edge type (no property).
pub(crate) fn memgraph_counts(rows: &[serde_json::Map<String, Value>]) -> Vec<(bool, String, u64)> {
    rows.iter()
        .filter_map(|r| {
            let t = r.get("index type").map(as_text).unwrap_or_default();
            let rel = match t.as_str() {
                "label" => false,
                "edge-type" => true,
                _ => return None,
            };
            if r.get("property").is_some_and(|p| !p.is_null() && !as_text(p).is_empty() && p != &Value::Array(Vec::new())) {
                return None;
            }
            let name = r.get("label").map(as_text).filter(|n| !n.is_empty())?;
            let n = r.get("count").and_then(Value::as_u64)?;
            Some((rel, name, n))
        })
        .collect()
}

fn estimate(rel: bool, name: String, rows: u64) -> RowEstimate {
    RowEstimate { object: ObjectRef { kind: if rel { RELATIONSHIP } else { LABEL }.into(), schema: None, name }, rows }
}

impl GraphSession {
    pub(crate) async fn stats_rows(&mut self) -> Result<Vec<RowEstimate>> {
        match self.flavor {
            Flavor::Neo4j => {
                let mut out = Vec::new();
                for rel in [false, true] {
                    for name in self.labels(rel).await.unwrap_or_default() {
                        let n = self.strings(&count_store_query(rel, &name)).await.ok();
                        if let Some(n) = n.and_then(|v| v.into_iter().next()).and_then(|v| v.parse().ok()) {
                            out.push(estimate(rel, name, n));
                        }
                    }
                }
                Ok(out)
            }
            Flavor::Memgraph => {
                let rows = self.records("SHOW INDEX INFO").await.unwrap_or_default();
                Ok(memgraph_counts(&rows).into_iter().map(|(rel, name, n)| estimate(rel, name, n)).collect())
            }
            Flavor::Neptune => Ok(Vec::new()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn count_store_shapes() {
        assert_eq!(count_store_query(false, "Person"), "MATCH (n:Person) RETURN count(n)");
        assert_eq!(count_store_query(true, "ACTED IN"), "MATCH ()-[r:`ACTED IN`]->() RETURN count(r)");
    }

    #[test]
    fn memgraph_whole_label_indexes_only() {
        let rows: Vec<serde_json::Map<String, Value>> = [
            json!({ "index type": "label", "label": "Person", "property": null, "count": 7 }),
            json!({ "index type": "label+property", "label": "Person", "property": "name", "count": 5 }),
            json!({ "index type": "edge-type", "label": "KNOWS", "property": null, "count": 3 }),
            json!({ "index type": "text", "label": "Doc", "property": null, "count": 2 }),
        ]
        .into_iter()
        .map(|v| v.as_object().unwrap().clone())
        .collect();
        assert_eq!(memgraph_counts(&rows), vec![(false, "Person".to_string(), 7), (true, "KNOWS".to_string(), 3)]);
    }
}
