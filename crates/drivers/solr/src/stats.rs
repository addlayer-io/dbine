//! What the server already knows, for documenting it
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `index.numDocs` of `admin/cores?action=STATUS`, the live
//!   document count each core's Lucene index keeps (never a `*:*` query).
//!   Standalone: one figure per core. SolrCloud: the cores answer for the
//!   node DBine is connected to, so a collection gets its count (the sum of
//!   one replica per shard) only when every one of its shards has a replica
//!   on that node; otherwise it gets none.
//! - Comments: Solr keeps none on cores, collections or configsets.

use crate::SolrSession;
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};
use dbine_driver_elasticsearch::json::J;
use std::collections::{BTreeMap, HashMap};

/// Standalone: `numDocs` per core of a cores `STATUS` reply.
pub(crate) fn core_docs(st: &J) -> BTreeMap<String, u64> {
    st.get("status")
        .and_then(J::as_obj)
        .into_iter()
        .flatten()
        .filter_map(|(name, c)| Some((name.clone(), c.at(&["index", "numDocs"]).and_then(J::as_u64)?)))
        .collect()
}

/// SolrCloud: per collection, the document count when the local cores of
/// `st` cover all the shards `CLUSTERSTATUS` lists for it (one replica per
/// shard: the largest, as a replica still recovering may lag behind).
pub(crate) fn collection_docs(st: &J, cs: &J) -> BTreeMap<String, u64> {
    let mut local: HashMap<(String, String), u64> = HashMap::new();
    for (_, c) in st.get("status").and_then(J::as_obj).into_iter().flatten() {
        let (Some(col), Some(shard), Some(n)) = (
            c.at(&["cloud", "collection"]).and_then(J::as_str),
            c.at(&["cloud", "shard"]).and_then(J::as_str),
            c.at(&["index", "numDocs"]).and_then(J::as_u64),
        ) else {
            continue;
        };
        let e = local.entry((col.to_string(), shard.to_string())).or_default();
        *e = (*e).max(n);
    }
    let mut out = BTreeMap::new();
    for (name, c) in cs.at(&["cluster", "collections"]).and_then(J::as_obj).into_iter().flatten() {
        let shards = c.get("shards").and_then(J::as_obj).map(Vec::as_slice).unwrap_or(&[]);
        let counts: Vec<u64> = shards.iter().filter_map(|(s, _)| local.get(&(name.clone(), s.clone())).copied()).collect();
        if !shards.is_empty() && counts.len() == shards.len() {
            out.insert(name.clone(), counts.iter().sum());
        }
    }
    out
}

pub(crate) async fn row_estimates(s: &SolrSession) -> Result<Vec<RowEstimate>> {
    let st = s.get_json("/solr/admin/cores?action=STATUS&wt=json").await?;
    let docs = if s.cloud {
        match s.get_json("/solr/admin/collections?action=CLUSTERSTATUS&wt=json").await {
            Ok(cs) => collection_docs(&st, &cs),
            Err(_) => BTreeMap::new(),
        }
    } else {
        core_docs(&st)
    };
    Ok(docs
        .into_iter()
        .filter(|(n, _)| !n.starts_with('.'))
        .map(|(name, rows)| RowEstimate { object: ObjectRef { kind: kinds::COLLECTION.into(), schema: None, name }, rows })
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn standalone_cores() {
        let st = J::parse(r#"{"status":{"books":{"index":{"numDocs":5}},"empty":{"index":{"numDocs":0}},"broken":{}}}"#).unwrap();
        let d = core_docs(&st);
        assert_eq!(d.get("books"), Some(&5));
        assert_eq!(d.get("empty"), Some(&0));
        assert!(!d.contains_key("broken"));
    }

    #[test]
    fn cloud_needs_every_shard_on_the_node() {
        let st = J::parse(
            r#"{"status":{
                "a_s1_r1":{"cloud":{"collection":"a","shard":"shard1"},"index":{"numDocs":3}},
                "a_s1_r2":{"cloud":{"collection":"a","shard":"shard1"},"index":{"numDocs":2}},
                "a_s2_r1":{"cloud":{"collection":"a","shard":"shard2"},"index":{"numDocs":4}},
                "b_s1_r1":{"cloud":{"collection":"b","shard":"shard1"},"index":{"numDocs":9}}}}"#,
        )
        .unwrap();
        let cs = J::parse(
            r#"{"cluster":{"collections":{
                "a":{"shards":{"shard1":{},"shard2":{}}},
                "b":{"shards":{"shard1":{},"shard2":{}}}}}}"#,
        )
        .unwrap();
        let d = collection_docs(&st, &cs);
        assert_eq!(d.get("a"), Some(&7));
        assert!(!d.contains_key("b"));
    }
}
