//! A collection's GSI indexes and how they're used (`Session::index_usage`).
//!
//! - The indexes: `system:indexes` (name, keys, `WHERE` condition, primary
//!   or not, last scan time). The primary index (`#primary`, over the
//!   document keys) is the collection's primary key. GSI has no unique
//!   indexes. Full-text (FTS) indexes are another service: not listed.
//! - The counters: the cluster manager's statistics
//!   (`/pools/default/stats/range/<metric>`, labels bucket / scope /
//!   collection / index, nodes added up), which every index node feeds:
//!   - `index_num_requests` (scan requests) → `seeks`. GSI answers every
//!     request with a scan of spans, a point lookup being a span of one
//!     key: there's no seek / scan split (`seek_scan_split` false).
//!   - `index_disk_size` → `size_kb`.
//!   - The last read: `metadata.stats.last_known_scan_time` (or
//!     `metadata.last_scan_time`) of `system:indexes`.
//!
//!   The indexer keeps them in memory: they start again when it does, so
//!   `since` is the start of the index node that started last (from its
//!   `uptime` in `/pools/default`).
//! - No writes: the indexer's only write counter, `num_docs_indexed`
//!   (`index_num_docs_indexed`), includes the documents indexed by the
//!   initial build, and no counter tells those apart (`num_items_flushed`
//!   and `num_flush_queued` move with it; checked on Couchbase 8). A freshly
//!   built index nobody has read yet would show as "sin uso" right away, so
//!   `writes_counted` is false (the UI shows a dash, nothing is "sin uso")
//!   and the note says why.
//! - The statistics need the "External Stats Reader" role (or an admin
//!   one). Refused: the indexes are listed without counters, with a note.
//! - No foreign keys: documents reference each other by key, by convention.

use crate::{encode, text, CbSession};
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result};
use serde_json::Value;
use std::collections::HashMap;

/// `(scan requests, disk bytes)` per index name.
type Stats = HashMap<String, (u64, Option<u64>)>;

/// The metrics read, in the order of the tuple in [`Stats`].
pub(crate) const METRICS: [&str; 2] = ["index_num_requests", "index_disk_size"];

/// What the note says when the counters were read: why there are no writes.
pub(crate) const NO_WRITES_NOTE: &str = "Couchbase cuenta las escrituras de un índice junto con los documentos de su construcción inicial, sin distinguirlas: no se muestran escrituras y ningún índice se marca sin uso.";

/// `yyyy-mm-dd hh:mm:ss` (UTC) of a Unix time in seconds.
pub(crate) fn utc(secs: i64) -> String {
    let days = secs.div_euclid(86_400);
    let rem = secs.rem_euclid(86_400);
    // Howard Hinnant's civil_from_days.
    let z = days + 719_468;
    let era = z.div_euclid(146_097);
    let doe = z - era * 146_097;
    let yoe = (doe - doe / 1460 + doe / 36_524 - doe / 146_096) / 365;
    let doy = doe - (365 * yoe + yoe / 4 - yoe / 100);
    let mp = (5 * doy + 2) / 153;
    let d = doy - (153 * mp + 2) / 5 + 1;
    let m = if mp < 10 { mp + 3 } else { mp - 9 };
    let y = yoe + era * 400 + i64::from(m <= 2);
    format!("{y:04}-{m:02}-{d:02} {:02}:{:02}:{:02}", rem / 3600, rem % 3600 / 60, rem % 60)
}

/// A scan time: a Unix time in s, ms, µs or ns (0 = never), or a date text.
pub(crate) fn scan_time(v: Option<&Value>) -> Option<String> {
    match v? {
        Value::Number(n) => {
            let n = n.as_f64()?;
            if n <= 0.0 {
                return None;
            }
            let secs = if n > 1e17 { n / 1e9 } else if n > 1e14 { n / 1e6 } else if n > 1e11 { n / 1e3 } else { n };
            Some(utc(secs as i64))
        }
        Value::String(s) if s.len() >= 19 => Some(s[..19].replace('T', " ")),
        _ => None,
    }
}

/// A stats range reply: the last value of each series, by its `index` label.
pub(crate) fn series(reply: &Value) -> HashMap<String, u64> {
    let mut out = HashMap::new();
    for d in reply.get("data").and_then(Value::as_array).into_iter().flatten() {
        let Some(name) = d.pointer("/metric/index").and_then(Value::as_str) else { continue };
        let last = d.get("values").and_then(Value::as_array).and_then(|v| v.last()).and_then(|p| p.get(1));
        let n = match last {
            Some(Value::String(s)) => s.parse::<f64>().ok(),
            Some(Value::Number(n)) => n.as_f64(),
            _ => None,
        };
        if let Some(n) = n {
            *out.entry(name.to_string()).or_insert(0) += n.max(0.0) as u64;
        }
    }
    out
}

/// `system:indexes` rows (see [`CbSession::index_usage_report`]) with
/// their counters (`None`: not readable).
pub(crate) fn assemble(rows: &[Value], stats: Option<&Stats>) -> Vec<IndexUsage> {
    rows.iter()
        .map(|r| {
            let name = r.get("name").map(text).unwrap_or_default();
            let primary = r.get("is_primary").and_then(Value::as_bool).unwrap_or(false);
            let mut keys: Vec<String> =
                r.get("index_key").and_then(Value::as_array).into_iter().flatten().filter_map(Value::as_str).map(str::to_string).collect();
            if primary && keys.is_empty() {
                keys.push("meta().id".into());
            }
            let (seeks, disk) = stats.and_then(|m| m.get(&name)).copied().unwrap_or_default();
            let partitioned = r.get("partition").and_then(Value::as_str).is_some_and(|p| !p.is_empty());
            let kind = match (primary, partitioned) {
                (true, _) => "PRIMARY",
                (false, true) => "GSI PARTITIONED",
                (false, false) => "GSI",
            };
            IndexUsage {
                kind: kind.into(),
                unique: false,
                primary_key: primary,
                key_columns: keys,
                filter: r.get("condition").and_then(Value::as_str).filter(|c| !c.is_empty()).map(str::to_string),
                size_kb: disk.map(|b| b.div_ceil(1024)),
                seeks,
                last_read: scan_time(r.get("last_scan")).or_else(|| scan_time(r.get("last_scan_time"))),
                name,
                ..Default::default()
            }
        })
        .collect()
}

impl CbSession {
    pub(crate) async fn index_usage_report(&self, obj: &ObjectRef) -> Result<Option<IndexUsageReport>> {
        let b = self.bucket()?;
        let (bucket, scope) = obj.schema.as_deref().and_then(crate::ddl::split_schema).map(|(b, s)| (b.to_string(), s.to_string())).unwrap_or((b, "_default".into()));
        let lit = |s: &str| serde_json::to_string(s).unwrap_or_default();
        // Indexes on a bucket's `_default` collection made before scopes
        // existed carry no bucket_id: their keyspace is the bucket.
        let legacy = if scope == "_default" && obj.name == "_default" { format!(" OR (i.bucket_id IS MISSING AND i.keyspace_id = {})", lit(&bucket)) } else { String::new() };
        let rows = self
            .results(&format!(
                "SELECT i.name, i.is_primary, i.index_key, i.`condition`, i.`partition`,
                        i.metadata.stats.last_known_scan_time AS last_scan, i.metadata.last_scan_time
                 FROM system:indexes AS i
                 WHERE i.`using` = 'gsi' AND ((i.bucket_id = {} AND i.scope_id = {} AND i.keyspace_id = {}){legacy})
                 ORDER BY i.is_primary DESC, i.name",
                lit(&bucket),
                lit(&scope),
                lit(&obj.name)
            ))
            .await?;
        let mut stats: Stats = HashMap::new();
        let mut note = None;
        for (i, m) in METRICS.iter().enumerate() {
            let path = format!(
                "/pools/default/stats/range/{m}?bucket={}&scope={}&collection={}&start=-1&step=1&nodesAggregation=sum",
                encode(&bucket),
                encode(&scope),
                encode(&obj.name)
            );
            match self.conn.mgmt_get(&path).await {
                Ok(v) => {
                    for (name, n) in series(&v) {
                        let e = stats.entry(name).or_insert((0, None));
                        match i {
                            0 => e.0 = n,
                            _ => e.1 = Some(n),
                        }
                    }
                }
                Err(e) => {
                    note = Some(if e.to_string().contains("permiso") {
                        "El usuario no puede leer las estadísticas del servicio de índices (rol «External Stats Reader» o uno de administración): se listan los índices sin contadores.".to_string()
                    } else {
                        format!("No se pudieron leer las estadísticas del servicio de índices: {e}")
                    });
                    break;
                }
            }
        }
        let available = note.is_none();
        let since = if available { self.index_node_start().await } else { None };
        Ok(Some(IndexUsageReport {
            since,
            stats_available: available,
            note: note.or_else(|| Some(NO_WRITES_NOTE.to_string())),
            indexes: assemble(&rows, available.then_some(&stats)),
            foreign_keys: Vec::new(),
            seek_scan_split: false,
            writes_counted: false,
        }))
    }

    /// When the index node that started last did (its counters started then).
    async fn index_node_start(&self) -> Option<String> {
        let v = self.conn.mgmt_get("/pools/default").await.ok()?;
        let uptime = v
            .get("nodes")?
            .as_array()?
            .iter()
            .filter(|n| n.get("services").and_then(Value::as_array).is_some_and(|s| s.iter().any(|x| x == "index")))
            .filter_map(|n| n.get("uptime").and_then(|u| u.as_str().and_then(|s| s.parse::<i64>().ok()).or_else(|| u.as_i64())))
            .min()?;
        let now = std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).ok()?.as_secs() as i64;
        Some(utc(now - uptime))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn dates() {
        assert_eq!(utc(0), "1970-01-01 00:00:00");
        assert_eq!(utc(1_704_067_200), "2024-01-01 00:00:00");
        assert_eq!(utc(951_825_600 + 3_723), "2000-02-29 13:02:03");
        assert_eq!(scan_time(Some(&json!(0))), None);
        assert_eq!(scan_time(Some(&json!(1_704_067_200_000_000_000_i64))), Some("2024-01-01 00:00:00".into()));
        assert_eq!(scan_time(Some(&json!(1_704_067_200_000_i64))), Some("2024-01-01 00:00:00".into()));
        assert_eq!(scan_time(Some(&json!("2024-01-01T00:00:00.123Z"))), Some("2024-01-01 00:00:00".into()));
        assert_eq!(scan_time(None), None);
    }

    #[test]
    fn stats_series() {
        let v = json!({"data": [
            {"metric": {"index": "#primary", "nodes": ["a"]}, "values": [[1, "4"], [2, "7"]]},
            {"metric": {"index": "ix_a"}, "values": [[1, "0"], [2, "5"]]},
            {"metric": {"nodes": ["a"]}, "values": [[1, "9"]]}
        ]});
        let m = series(&v);
        assert_eq!(m.get("#primary"), Some(&7));
        assert_eq!(m.get("ix_a"), Some(&5));
        assert_eq!(m.len(), 2);
    }

    #[test]
    fn rows_become_indexes() {
        let rows = vec![
            json!({"name": "#primary", "is_primary": true, "last_scan": 0}),
            json!({"name": "ix_cliente", "is_primary": false, "index_key": ["`cliente`"], "last_scan": 1_704_067_200_000_000_000_i64}),
            json!({"name": "ix_fecha", "is_primary": false, "index_key": ["`fecha` DESC"], "condition": "(`fecha` > 0)", "partition": "HASH(`fecha`)"}),
        ];
        let mut stats = Stats::new();
        stats.insert("ix_cliente".into(), (5, Some(2048)));
        stats.insert("ix_fecha".into(), (0, Some(100)));
        let r = IndexUsageReport { stats_available: true, indexes: assemble(&rows, Some(&stats)), seek_scan_split: false, writes_counted: false, ..Default::default() }.derived();
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap();
        assert!(get("#primary").primary_key);
        assert_eq!(get("#primary").kind, "PRIMARY");
        assert_eq!(get("#primary").key_columns, ["meta().id"]);
        assert_eq!((get("ix_cliente").seeks, get("ix_cliente").size_kb), (5, Some(2)));
        assert_eq!(get("ix_cliente").last_read.as_deref(), Some("2024-01-01 00:00:00"));
        assert_eq!(get("ix_fecha").filter.as_deref(), Some("(`fecha` > 0)"));
        assert_eq!(get("ix_fecha").kind, "GSI PARTITIONED");
        // Writes aren't counted (the initial build is mixed in): an unread
        // index is not "sin uso".
        assert!(r.indexes.iter().all(|i| i.updates == 0 && !i.unused && i.writes_per_read.is_none()));
        assert_eq!(get("ix_cliente").read_share, Some(1.0));
        assert!(r.indexes.iter().all(|i| i.seek_health.is_none()));
        // Without statistics: listed, no counters.
        assert!(assemble(&rows, None).iter().all(|i| i.seeks == 0 && i.updates == 0 && i.size_kb.is_none()));
    }
}
