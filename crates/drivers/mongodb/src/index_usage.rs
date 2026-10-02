//! A collection's indexes and how they're used (`Session::index_usage`).
//!
//! - The indexes: `listIndexes` (key, unique, sparse, TTL, hidden,
//!   `partialFilterExpression` as the filter). `_id_` (or a clustered
//!   collection's clustered index) is the primary key.
//! - The counters: the `$indexStats` stage. MongoDB keeps one number per
//!   index, `accesses.ops` (operations that used it, since `accesses.since`:
//!   the server's start or the index's creation, whichever is later), with
//!   no difference between a point lookup and a range or full scan. It goes
//!   in `seeks`, and `seek_scan_split` is false so the UI shows no seek
//!   health. On a sharded collection there is one row per shard: they're
//!   added up and `since` is the earliest.
//! - The writes: MongoDB has no per-index write counter. The only one is
//!   the collection's (`$collStats` `latencyStats.writes.ops`): operations,
//!   not index entries (an `updateMany` is one), counted since the server
//!   started or the collection was created, so an index created later
//!   would carry writes made before it existed, and a partial or sparse
//!   one writes made to documents it doesn't cover. Shown per index it
//!   would mislead: `writes_counted` is false (the UI shows a dash and no
//!   index is "sin uso"), and the note says why.
//! - The size: `$collStats` `storageStats.indexSizes` (bytes, rounded up
//!   to KB).
//! - `$indexStats` needs the `indexStats` action (`clusterMonitor`, or
//!   `dbAdmin` on the database); `$collStats` needs `collStats`. Refused or
//!   missing (FerretDB has no `$indexStats`): the indexes are listed without
//!   counters and the note says why.
//! - No foreign keys: documents reference each other only by convention.

use crate::{Flavor, MongoSession};
use dbine_driver::{IndexUsage, IndexUsageReport, ObjectRef, Result};
use mongodb::bson::{doc, Bson, Document};
use std::collections::HashMap;

/// One index's `$indexStats` counters, shards added up.
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Access {
    pub ops: u64,
    /// `yyyy-mm-dd hh:mm:ss` (UTC).
    pub since: Option<String>,
    /// `since` in milliseconds since the epoch.
    pub since_ms: Option<i64>,
}

fn as_u64(b: &Bson) -> u64 {
    match b {
        Bson::Int32(n) => (*n).max(0) as u64,
        Bson::Int64(n) => (*n).max(0) as u64,
        Bson::Double(n) if *n > 0.0 => *n as u64,
        _ => 0,
    }
}

fn date_text(b: &Bson) -> Option<String> {
    match b {
        Bson::DateTime(d) => d.try_to_rfc3339_string().ok().map(|s| s.replace('T', " ").chars().take(19).collect()),
        _ => None,
    }
}

/// `$indexStats` rows by index name (one per shard: added up, the earliest `since`).
pub(crate) fn accesses(rows: &[Document]) -> HashMap<String, Access> {
    let mut out: HashMap<String, Access> = HashMap::new();
    for r in rows {
        let Ok(name) = r.get_str("name") else { continue };
        let a = out.entry(name.to_string()).or_default();
        if let Ok(acc) = r.get_document("accesses") {
            a.ops += acc.get("ops").map(as_u64).unwrap_or(0);
            if let Some(Bson::DateTime(d)) = acc.get("since") {
                if a.since_ms.is_none_or(|x| d.timestamp_millis() < x) {
                    a.since_ms = Some(d.timestamp_millis());
                    a.since = date_text(&Bson::DateTime(*d));
                }
            }
        }
    }
    out
}

/// A `listIndexes` spec as an index (no counters).
pub(crate) fn index_of(spec: &Document) -> Option<IndexUsage> {
    let name = spec.get_str("name").ok()?.to_string();
    let key = spec.get_document("key").ok()?;
    let mut columns = Vec::new();
    let mut kinds: Vec<String> = Vec::new();
    let mut add_kind = |k: &str| {
        if !kinds.iter().any(|x| x == k) {
            kinds.push(k.to_string());
        }
    };
    for (k, v) in key {
        match (k.as_str(), v) {
            ("_ftsx", _) => {}
            ("_fts", _) => {
                add_kind("TEXT");
                match spec.get_document("weights") {
                    Ok(w) => columns.extend(w.keys().map(|f| format!("{f} (text)"))),
                    Err(_) => columns.push("(text)".into()),
                }
            }
            (_, Bson::String(s)) => {
                add_kind(&s.to_ascii_uppercase());
                columns.push(format!("{k} ({s})"));
            }
            _ => {
                if k.contains("$**") {
                    add_kind("WILDCARD");
                }
                let desc = match v {
                    Bson::Int32(n) => *n < 0,
                    Bson::Int64(n) => *n < 0,
                    Bson::Double(n) => *n < 0.0,
                    _ => false,
                };
                columns.push(if desc { format!("{k} DESC") } else { k.clone() });
            }
        }
    }
    let clustered = spec.get_bool("clustered").unwrap_or(false);
    let primary_key = name == "_id_" || clustered;
    let mut kind = if kinds.is_empty() { if clustered { "CLUSTERED".to_string() } else { "BTREE".to_string() } } else { kinds.join(" ") };
    if spec.get("expireAfterSeconds").is_some() {
        kind.push_str(" TTL");
    }
    for (k, label) in [("sparse", " SPARSE"), ("hidden", " HIDDEN")] {
        if spec.get_bool(k).unwrap_or(false) {
            kind.push_str(label);
        }
    }
    let filter = spec
        .get_document("partialFilterExpression")
        .ok()
        .map(|f| Bson::Document(f.clone()).into_relaxed_extjson().to_string());
    Some(IndexUsage {
        name,
        kind,
        unique: primary_key || spec.get_bool("unique").unwrap_or(false),
        primary_key,
        key_columns: columns,
        filter,
        ..Default::default()
    })
}

/// What the note says when the counters were read: why there are no writes.
pub(crate) const NO_WRITES_NOTE: &str = "MongoDB no cuenta escrituras por índice, solo operaciones de la colección (desde que arrancó el servidor o se creó la colección, sin distinguir índices creados después): no se muestran escrituras y ningún índice se marca sin uso.";

/// The report from the reads. `stats` `None`: `$indexStats` was refused
/// (`note` says why).
pub(crate) fn assemble(specs: &[Document], stats: Option<&HashMap<String, Access>>, sizes: Option<&Document>, note: Option<String>) -> IndexUsageReport {
    let mut since: Option<String> = None;
    let indexes = specs
        .iter()
        .filter_map(index_of)
        .map(|mut ix| {
            if let Some(a) = stats.and_then(|m| m.get(&ix.name)) {
                ix.seeks = a.ops;
                if let Some(s) = &a.since {
                    if since.as_ref().is_none_or(|x| s < x) {
                        since = Some(s.clone());
                    }
                }
            }
            ix.size_kb = sizes.and_then(|d| d.get(&ix.name)).map(as_u64).map(|b| b.div_ceil(1024));
            ix
        })
        .collect();
    let note = note.or_else(|| stats.is_some().then(|| NO_WRITES_NOTE.to_string()));
    IndexUsageReport { since, stats_available: stats.is_some(), note, indexes, foreign_keys: Vec::new(), seek_scan_split: false, writes_counted: false }
}

/// Why `$indexStats` failed, for the note.
pub(crate) fn stats_note(e: &mongodb::error::Error) -> String {
    let code = crate::command_code(e);
    match code {
        // Unauthorized
        Some(13) => "El usuario no tiene la acción indexStats (rol clusterMonitor, o dbAdmin en la base): se listan los índices sin contadores.".into(),
        // Unrecognized pipeline stage (FerretDB and other compatibles).
        Some(40324) | Some(16436) => "Este servidor no tiene $indexStats: se listan los índices sin contadores.".into(),
        _ => format!("No se pudieron leer los contadores ($indexStats): {e}"),
    }
}

impl MongoSession {
    async fn aggregate_all(&self, coll: &str, pipeline: Vec<Document>) -> std::result::Result<Vec<Document>, mongodb::error::Error> {
        use futures::TryStreamExt;
        let mut cur = self.db.run_cursor_command(doc! { "aggregate": coll, "pipeline": pipeline, "cursor": {} }).await?;
        let mut out = Vec::new();
        while let Some(d) = cur.try_next().await? {
            out.push(d);
        }
        Ok(out)
    }

    pub(crate) async fn index_usage_report(&self, obj: &ObjectRef) -> Result<Option<IndexUsageReport>> {
        let specs = match self.first_batch(doc! { "listIndexes": &obj.name }, usize::MAX).await {
            Ok(s) => s,
            // A view or a missing collection: nothing to report.
            Err(_) if self.first_batch(doc! { "listCollections": 1, "filter": { "name": &obj.name, "type": "collection" } }, 1).await?.is_empty() => {
                return Ok(None)
            }
            Err(e) => return Err(e),
        };
        let (stats, note) = if self.flavor == Flavor::Ferret {
            // FerretDB answers `$indexStats` with every counter at zero.
            (None, Some("FerretDB no cuenta el uso de los índices ($indexStats devuelve ceros): se listan sin contadores.".to_string()))
        } else {
            match self.aggregate_all(&obj.name, vec![doc! { "$indexStats": {} }]).await {
                Ok(rows) => (Some(accesses(&rows)), None),
                Err(e) => (None, Some(stats_note(&e))),
            }
        };
        let coll = self.aggregate_all(&obj.name, vec![doc! { "$collStats": { "storageStats": {} } }]).await.unwrap_or_default();
        // One row per shard: sizes merged.
        let mut sizes = Document::new();
        for r in &coll {
            if let Ok(s) = r.get_document("storageStats").and_then(|s| s.get_document("indexSizes")) {
                for (k, v) in s {
                    let prev = sizes.get(k).map(as_u64).unwrap_or(0);
                    sizes.insert(k.clone(), Bson::Int64((prev + as_u64(v)) as i64));
                }
            }
        }
        // Servers without `$collStats` (FerretDB): the sizes from `collStats`.
        if coll.is_empty() {
            if let Ok(r) = self.db.run_command(doc! { "collStats": &obj.name }).await {
                if let Ok(s) = r.get_document("indexSizes") {
                    sizes = s.clone();
                }
            }
        }
        let sizes = (!sizes.is_empty()).then_some(&sizes);
        Ok(Some(assemble(&specs, stats.as_ref(), sizes, note)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use mongodb::bson::DateTime;

    fn specs() -> Vec<Document> {
        vec![
            doc! { "v": 2, "key": { "_id": 1 }, "name": "_id_" },
            doc! { "v": 2, "key": { "email": 1 }, "name": "email_1", "unique": true, "sparse": true },
            doc! { "v": 2, "key": { "cat": 1, "at": -1 }, "name": "cat_at", "partialFilterExpression": { "h": { "$gt": 0 } } },
            doc! { "v": 2, "key": { "_fts": "text", "_ftsx": 1 }, "name": "ft", "weights": { "titulo": 10, "cuerpo": 1 } },
            doc! { "v": 2, "key": { "loc": "2dsphere" }, "name": "geo" },
            doc! { "v": 2, "key": { "at": 1 }, "name": "exp", "expireAfterSeconds": 60, "hidden": true },
        ]
    }

    #[test]
    fn specs_become_indexes() {
        let ix: Vec<IndexUsage> = specs().iter().filter_map(index_of).collect();
        let get = |n: &str| ix.iter().find(|i| i.name == n).unwrap();
        assert!(get("_id_").primary_key && get("_id_").unique);
        assert_eq!(get("_id_").key_columns, ["_id"]);
        assert_eq!(get("email_1").kind, "BTREE SPARSE");
        assert!(get("email_1").unique && !get("email_1").primary_key);
        assert_eq!(get("cat_at").key_columns, ["cat", "at DESC"]);
        assert_eq!(get("cat_at").filter.as_deref(), Some(r#"{"h":{"$gt":0}}"#));
        assert_eq!(get("ft").kind, "TEXT");
        assert_eq!(get("ft").key_columns, ["titulo (text)", "cuerpo (text)"]);
        assert_eq!(get("geo").kind, "2DSPHERE");
        assert_eq!(get("geo").key_columns, ["loc (2dsphere)"]);
        assert_eq!(get("exp").kind, "BTREE TTL HIDDEN");
    }

    #[test]
    fn shards_add_up_and_earliest_since() {
        let t1 = DateTime::from_millis(1_700_000_000_000);
        let t2 = DateTime::from_millis(1_600_000_000_000);
        let rows = vec![
            doc! { "name": "a", "accesses": { "ops": 3_i64, "since": t1 } },
            doc! { "name": "a", "accesses": { "ops": 4_i32, "since": t2 } },
            doc! { "name": "b", "accesses": { "ops": 0_i64, "since": t1 } },
        ];
        let m = accesses(&rows);
        assert_eq!(m["a"].ops, 7);
        assert_eq!(m["a"].since.as_deref(), Some("2020-09-13 12:26:40"));
        assert_eq!(m["b"].ops, 0);
    }

    #[test]
    fn report_counters_without_writes() {
        let t = DateTime::from_millis(1_700_000_000_000);
        let stats = accesses(&[
            doc! { "name": "_id_", "accesses": { "ops": 2_i64, "since": t } },
            doc! { "name": "email_1", "accesses": { "ops": 5_i64, "since": t } },
            doc! { "name": "cat_at", "accesses": { "ops": 0_i64, "since": DateTime::from_millis(1_700_003_600_000) } },
        ]);
        let sizes = doc! { "_id_": 4096_i32, "email_1": 1000_i64 };
        let r = assemble(&specs(), Some(&stats), Some(&sizes), None).derived();
        assert!(r.stats_available && !r.seek_scan_split && !r.writes_counted);
        assert_eq!(r.since.as_deref(), Some("2023-11-14 22:13:20"));
        assert_eq!(r.note.as_deref(), Some(NO_WRITES_NOTE));
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap();
        assert_eq!((get("email_1").seeks, get("email_1").scans), (5, 0));
        assert_eq!(get("_id_").size_kb, Some(4));
        assert_eq!(get("email_1").size_kb, Some(1));
        // Reads and shares stay; writes are unknown: none, nothing unused.
        assert_eq!(get("email_1").read_share.map(|v| (v * 100.0).round()), Some(71.0));
        assert!(r.indexes.iter().all(|i| i.updates == 0 && !i.unused && i.writes_per_read.is_none()));
        // No seek / scan split: no health.
        assert!(r.indexes.iter().all(|i| i.seek_health.is_none()));
    }

    #[test]
    fn without_index_stats() {
        let r = assemble(&specs(), None, None, Some("x".into())).derived();
        assert!(!r.stats_available);
        assert_eq!(r.note.as_deref(), Some("x"));
        assert!(r.indexes.iter().all(|i| i.updates == 0 && !i.unused));
        assert_eq!(r.indexes.len(), 6);
    }
}
