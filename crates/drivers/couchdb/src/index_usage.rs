//! The database's indexes (`Session::index_usage` on `_all_docs`, the
//! database's documents):
//!
//! - `_all_docs`, the `_id` index CouchDB keeps itself (the "special"
//!   one of `GET /{db}/_index`), is the primary key.
//! - The Mango indexes of `GET /{db}/_index` (json or text; fields, the
//!   descending ones with ` DESC`, and `partial_filter_selector` as the
//!   filter), named as the schema compare names them.
//! - The size: `GET /{db}/_design/{ddoc}/_info` (`sizes.active`), when the
//!   design document holds that index alone.
//!
//! CouchDB counts no use per index (`_stats` has only server-wide Mango
//! figures): the report has no counters and says so. No foreign keys.

use crate::{ddl, seg, CouchSession};
use dbine_driver::{IndexUsage, IndexUsageReport, Result};
use reqwest::Method;
use serde_json::Value;
use std::collections::HashMap;

pub(crate) const NOTE: &str = "CouchDB no lleva la cuenta del uso de cada índice: se listan sin contadores.";

fn ddoc_of(ix: &Value) -> Option<&str> {
    ix.get("ddoc").and_then(Value::as_str)
}

/// `GET /{db}/_index` as the report; `sizes`: active bytes by design doc.
pub(crate) fn report(indexes: &Value, sizes: &HashMap<String, u64>) -> IndexUsageReport {
    let list: Vec<&Value> = indexes.get("indexes").and_then(Value::as_array).into_iter().flatten().collect();
    let mut per_ddoc: HashMap<&str, usize> = HashMap::new();
    for ix in &list {
        if let Some(d) = ddoc_of(ix) {
            *per_ddoc.entry(d).or_default() += 1;
        }
    }
    let mut out = vec![IndexUsage { name: crate::ALL_DOCS.into(), kind: "PRIMARY".into(), unique: true, primary_key: true, key_columns: vec!["_id".into()], ..Default::default() }];
    for ix in list {
        let Some(def) = ddl::index_def(ix) else { continue };
        let size_kb = ddoc_of(ix).filter(|d| per_ddoc.get(d) == Some(&1)).and_then(|d| sizes.get(d)).map(|b| b.div_ceil(1024));
        out.push(IndexUsage {
            name: def.name,
            kind: def.kind.unwrap_or_else(|| "json".into()).to_ascii_uppercase(),
            key_columns: def.columns.into_iter().map(|c| c.strip_suffix(":desc").map(|f| format!("{f} DESC")).unwrap_or(c)).collect(),
            filter: def.filter,
            size_kb,
            ..Default::default()
        });
    }
    IndexUsageReport { stats_available: false, note: Some(NOTE.into()), indexes: out, seek_scan_split: false, ..Default::default() }
}

impl CouchSession {
    pub(crate) async fn index_usage_report(&self) -> Result<Option<IndexUsageReport>> {
        let path = self.db_path()?;
        let indexes = self.call(Method::GET, &format!("{path}/_index"), None).await?;
        let mut sizes = HashMap::new();
        let ddocs: Vec<String> = indexes.get("indexes").and_then(Value::as_array).into_iter().flatten().filter_map(ddoc_of).map(str::to_string).collect();
        for d in ddocs {
            let name = d.trim_start_matches("_design/");
            if let Ok(info) = self.call(Method::GET, &format!("{path}/_design/{}/_info", seg(name)), None).await {
                if let Some(n) = info.pointer("/view_index/sizes/active").and_then(Value::as_u64) {
                    sizes.insert(d, n);
                }
            }
        }
        Ok(Some(report(&indexes, &sizes)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn mango_indexes() {
        let v = json!({"total_rows": 4, "indexes": [
            {"ddoc": null, "name": "_all_docs", "type": "special", "def": {"fields": [{"_id": "asc"}]}},
            {"ddoc": "_design/a", "name": "ix_cliente", "type": "json", "def": {"fields": [{"cliente": "asc"}, {"fecha": "desc"}], "partial_filter_selector": {"fecha": {"$gt": 0}}}},
            {"ddoc": "_design/b", "name": "ix_x", "type": "json", "def": {"fields": [{"x": "asc"}]}},
            {"ddoc": "_design/b", "name": "ix_y", "type": "json", "def": {"fields": [{"y": "asc"}]}}
        ]});
        let mut sizes = HashMap::new();
        sizes.insert("_design/a".to_string(), 3000_u64);
        sizes.insert("_design/b".to_string(), 9000_u64);
        let r = report(&v, &sizes).derived();
        let names: Vec<&str> = r.indexes.iter().map(|i| i.name.as_str()).collect();
        assert_eq!(names, ["_all_docs", "ix_cliente", "ix_x", "ix_y"]);
        assert!(r.indexes[0].primary_key);
        assert_eq!(r.indexes[1].key_columns, ["cliente", "fecha DESC"]);
        assert_eq!(r.indexes[1].kind, "JSON");
        assert!(r.indexes[1].filter.is_some());
        assert_eq!(r.indexes[1].size_kb, Some(3));
        // Two indexes in one design doc: its size isn't either's.
        assert_eq!(r.indexes[2].size_kb, None);
        assert!(!r.stats_available);
    }
}
