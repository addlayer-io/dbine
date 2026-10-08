//! What the cluster already knows, for documenting it
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: `docs.count` of `_cat/indices`, the primaries' document count
//!   that each shard keeps (Lucene's `numDocs`, so nested documents count
//!   too); a data stream adds up its backing indices. A closed index has no
//!   count and is left out. Aliases are left out: they point at indices
//!   already listed.
//! - Comments: indices, aliases and data streams have none (`_meta` in a
//!   mapping is free-form application data, not a comment).

use crate::json::J;
use crate::EsSession;
use dbine_driver::stats::RowEstimate;
use dbine_driver::{kinds, ObjectRef, Result};
use std::collections::HashMap;

/// `docs.count` per index from `_cat/indices?format=json&h=index,docs.count`
/// (the count comes as a string; a closed index has `null`).
pub(crate) fn doc_counts(cat: &J) -> HashMap<String, u64> {
    cat.as_arr()
        .unwrap_or(&[])
        .iter()
        .filter_map(|i| {
            let name = i.get("index").and_then(J::as_str)?;
            let n = match i.get("docs.count")? {
                J::Str(s) => s.parse().ok()?,
                J::Num(n) => n.as_u64()?,
                _ => return None,
            };
            Some((name.to_string(), n))
        })
        .collect()
}

/// Data stream name → its backing indices, from `GET /_data_stream`.
pub(crate) fn stream_indices(ds: &J) -> Vec<(String, Vec<String>)> {
    ds.get("data_streams")
        .and_then(J::as_arr)
        .unwrap_or(&[])
        .iter()
        .filter_map(|d| {
            let name = d.get("name").and_then(J::as_str)?;
            let indices = d
                .get("indices")
                .and_then(J::as_arr)
                .unwrap_or(&[])
                .iter()
                .filter_map(|i| i.get("index_name").and_then(J::as_str).map(str::to_string))
                .collect();
            Some((name.to_string(), indices))
        })
        .collect()
}

pub(crate) async fn row_estimates(s: &EsSession) -> Result<Vec<RowEstimate>> {
    let cat = s.get_json("/_cat/indices?format=json&expand_wildcards=all&h=index,docs.count&s=index").await?;
    let counts = doc_counts(&cat);
    let obj = |kind: &str, name: &str| ObjectRef { kind: kind.into(), schema: None, name: name.into() };
    let mut names: Vec<&String> = counts.keys().filter(|n| s.visible(n)).collect();
    names.sort();
    let mut out: Vec<RowEstimate> =
        names.into_iter().map(|n| RowEstimate { object: obj(kinds::INDEX, n), rows: counts[n] }).collect();
    if let Ok(ds) = s.get_json("/_data_stream").await {
        for (name, indices) in stream_indices(&ds).into_iter().filter(|(n, _)| s.visible(n)) {
            let known: Vec<u64> = indices.iter().filter_map(|i| counts.get(i).copied()).collect();
            if !known.is_empty() {
                out.push(RowEstimate { object: obj(kinds::STREAM, &name), rows: known.iter().sum() });
            }
        }
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn counts_skip_closed_indices() {
        let cat = J::parse(r#"[{"index":"books","docs.count":"5"},{"index":"old","docs.count":null},{"index":"n","docs.count":7}]"#).unwrap();
        let c = doc_counts(&cat);
        assert_eq!(c.get("books"), Some(&5));
        assert_eq!(c.get("n"), Some(&7));
        assert!(!c.contains_key("old"));
    }

    #[test]
    fn data_streams_list_backing_indices() {
        let ds = J::parse(
            r#"{"data_streams":[{"name":"logs-app","indices":[{"index_name":".ds-logs-app-1"},{"index_name":".ds-logs-app-2"}]}]}"#,
        )
        .unwrap();
        assert_eq!(stream_indices(&ds), vec![("logs-app".to_string(), vec![".ds-logs-app-1".to_string(), ".ds-logs-app-2".to_string()])]);
    }
}
