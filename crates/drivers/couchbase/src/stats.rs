//! What the server already knows, for documenting a bucket
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: the Data service's `kv_collection_item_count` gauge per
//!   collection, from the statistics REST API
//!   (`/pools/default/stats/range`, Couchbase Server 7.0+), added up over
//!   the nodes. Never a `SELECT COUNT(*)`. Older servers, or a login
//!   without the stats privilege, get nothing.
//! - Comments: Couchbase keeps none on scopes, collections, indexes or
//!   functions.

use crate::{encode, CbSession};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};
use serde_json::Value;
use std::collections::BTreeMap;

const METRIC: &str = "kv_collection_item_count";

/// `(scope, collection) → items` from a stats range reply: the latest
/// sample of each series (one per collection when aggregated over nodes).
/// A series split by vBucket `state` only counts the active one.
pub(crate) fn item_counts(reply: &Value) -> BTreeMap<(String, String), u64> {
    let mut out = BTreeMap::new();
    for series in reply.get("data").and_then(Value::as_array).into_iter().flatten() {
        let m = series.get("metric");
        let label = |k: &str| m.and_then(|m| m.get(k)).and_then(Value::as_str);
        if label("state").is_some_and(|s| s != "active") {
            continue;
        }
        let (Some(scope), Some(coll)) = (label("scope"), label("collection")) else { continue };
        let last = series.get("values").and_then(Value::as_array).and_then(|v| v.last());
        let n = last.and_then(|p| p.get(1)).and_then(|v| match v {
            Value::String(s) => s.parse::<f64>().ok(),
            v => v.as_f64(),
        });
        if let Some(n) = n.filter(|n| n.is_finite() && *n >= 0.0) {
            *out.entry((scope.to_string(), coll.to_string())).or_default() += n as u64;
        }
    }
    out
}

pub(crate) async fn row_estimates(s: &CbSession) -> Result<Vec<RowEstimate>> {
    let b = s.bucket()?;
    let path = format!("/pools/default/stats/range/{METRIC}?bucket={}&start=-60&step=10&nodesAggregation=sum", encode(&b));
    let Ok(reply) = s.conn.mgmt_get(&path).await else { return Ok(Vec::new()) };
    Ok(item_counts(&reply)
        .into_iter()
        .filter(|((scope, _), _)| !scope.starts_with("_system"))
        .map(|((scope, name), rows)| RowEstimate {
            object: ObjectRef { kind: kinds::COLLECTION.into(), schema: Some(format!("{b}.{scope}")), name },
            rows,
        })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn latest_sample_per_collection() {
        let reply = json!({ "data": [
            { "metric": { "bucket": "b", "scope": "_default", "collection": "_default", "name": METRIC },
              "values": [[1700000000, "10"], [1700000010, "12"]] },
            { "metric": { "bucket": "b", "scope": "inv", "collection": "items", "name": METRIC },
              "values": [[1700000010, "3"]] },
            { "metric": { "bucket": "b", "scope": "inv", "collection": "items", "state": "replica" },
              "values": [[1700000010, "3"]] },
            { "metric": { "bucket": "b", "scope": "inv", "collection": "gone" }, "values": [] }
        ]});
        let c = item_counts(&reply);
        assert_eq!(c[&("_default".to_string(), "_default".to_string())], 12);
        assert_eq!(c[&("inv".to_string(), "items".to_string())], 3);
        assert!(!c.contains_key(&("inv".to_string(), "gone".to_string())));
    }
}
