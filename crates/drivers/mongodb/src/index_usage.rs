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
//! - The writes: MongoDB has no per-index write counter. Every write to the
//!   collection maintains its indexes (a partial or sparse one only for the
//!   documents it covers), so `updates` is the collection's write
//!   operations since the server started or the collection was created
//!   (`$collStats` `latencyStats`). That count starts with the primary key
//!   index (`_id_`), whose `accesses.since` marks it. An index created
//!   later (its `accesses.since` after the `_id_` one, beyond the few
//!   seconds a restart takes to load the indexes) would inherit the writes
//!   made before it existed: it gets no writes, so it's never "sin uso",
//!   and the note says so. Any other index with no reads on a collection
//!   that's written is "sin uso".
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

/// How long after the server's start an index's counters may start and
/// still count as loaded at startup (a restart loads the indexes one by one).
const STARTUP_SLACK_MS: i64 = 10_000;

/// The report from the reads. `stats` `None`: `$indexStats` was refused
/// (`note` says why). `writes`: the collection's write operations, counted
/// since the `_id_` index's `accesses.since`. `server_start_ms`: when the
/// server started (`serverStatus`), to tell the indexes loaded at startup
/// from the ones created afterwards.
pub(crate) fn assemble(
    specs: &[Document],
    stats: Option<&HashMap<String, Access>>,
    writes: Option<u64>,
    sizes: Option<&Document>,
    note: Option<String>,
    server_start_ms: Option<i64>,
) -> IndexUsageReport {
    // When the collection's write count started: the primary key index's
    // counters start with it (server start or collection creation).
    let writes_from = stats.and_then(|m| {
        specs
            .iter()
            .filter_map(index_of)
            .filter(|ix| ix.primary_key)
            .filter_map(|ix| m.get(&ix.name).and_then(|a| a.since_ms))
            .min()
    });
    // Indexes loaded at the server's start get a few seconds of slack.
    let slack = match (writes_from, server_start_ms) {
        (Some(w), Some(st)) if w - st <= STARTUP_SLACK_MS => STARTUP_SLACK_MS,
        _ => 0,
    };
    let mut late = 0usize;
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
            let created_later = match (writes_from, stats.and_then(|m| m.get(&ix.name)).and_then(|a| a.since_ms)) {
                (Some(w), Some(s)) => s > w + slack,
                _ => false,
            };
            if created_later {
                late += 1;
            } else if stats.is_some() {
                ix.updates = writes.unwrap_or(0);
            }
            ix.size_kb = sizes.and_then(|d| d.get(&ix.name)).map(as_u64).map(|b| b.div_ceil(1024));
            ix
        })
        .collect();
    let mut note = note;
    if stats.is_some() && writes.is_none() && note.is_none() {
        note = Some("MongoDB no cuenta escrituras por índice y no se pudieron leer las de la colección ($collStats): ningún índice se marca sin uso.".into());
    } else if late > 0 && writes.is_some() && note.is_none() {
        note = Some(format!(
            "MongoDB no cuenta escrituras por índice: se usan las de la colección, que cuentan desde que arrancó el servidor o se creó la colección. {} se {} después: no se les asignan escrituras ni se {} sin uso.",
            if late == 1 { "Un índice".to_string() } else { format!("{late} índices") },
            if late == 1 { "creó" } else { "crearon" },
            if late == 1 { "marca" } else { "marcan" },
        ));
    }
    IndexUsageReport { since, stats_available: stats.is_some(), note, indexes, foreign_keys: Vec::new(), seek_scan_split: false, writes_counted: writes.is_some() }
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
        let coll = self
            .aggregate_all(&obj.name, vec![doc! { "$collStats": { "latencyStats": {}, "storageStats": {} } }])
            .await
            .unwrap_or_default();
        // One row per shard: writes added up, sizes merged.
        let mut writes: Option<u64> = None;
        let mut sizes = Document::new();
        for r in &coll {
            if let Ok(ops) = r.get_document("latencyStats").and_then(|l| l.get_document("writes")).map(|w| w.get("ops").map(as_u64).unwrap_or(0)) {
                *writes.get_or_insert(0) += ops;
            }
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
        // The server's start: now minus its uptime (`serverStatus`, which
        // needs the `serverStatus` action; without it, no startup slack).
        let server_start_ms = match stats {
            Some(_) => self.db.run_command(doc! { "serverStatus": 1, "repl": 0, "metrics": 0, "locks": 0 }).await.ok().and_then(|r| {
                let now = r.get_datetime("localTime").ok()?.timestamp_millis();
                let up = r.get("uptimeMillis").map(as_u64)? as i64;
                Some(now - up)
            }),
            None => None,
        };
        Ok(Some(assemble(&specs, stats.as_ref(), writes, sizes, note, server_start_ms)))
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
    fn report_counters_and_unused() {
        let t = DateTime::from_millis(1_700_000_000_000);
        let stats = accesses(&[
            doc! { "name": "_id_", "accesses": { "ops": 2_i64, "since": t } },
            doc! { "name": "email_1", "accesses": { "ops": 5_i64, "since": t } },
            doc! { "name": "cat_at", "accesses": { "ops": 0_i64, "since": t } },
        ]);
        let sizes = doc! { "_id_": 4096_i32, "email_1": 1000_i64 };
        let r = assemble(&specs(), Some(&stats), Some(9), Some(&sizes), None, Some(1_700_000_000_000)).derived();
        assert!(r.stats_available && !r.seek_scan_split);
        assert_eq!(r.since.as_deref(), Some("2023-11-14 22:13:20"));
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap();
        assert_eq!((get("email_1").seeks, get("email_1").scans, get("email_1").updates), (5, 0, 9));
        assert_eq!(get("_id_").size_kb, Some(4));
        assert_eq!(get("email_1").size_kb, Some(1));
        assert!(get("cat_at").unused);
        assert!(!get("email_1").unused);
        // No seek / scan split: no health.
        assert!(r.indexes.iter().all(|i| i.seek_health.is_none()));
    }

    #[test]
    fn index_created_after_the_write_count_started() {
        let start = 1_700_000_000_000_i64;
        let at = |ms: i64| DateTime::from_millis(ms);
        let rows = |late: i64| {
            accesses(&[
                doc! { "name": "_id_", "accesses": { "ops": 0_i64, "since": at(start + 100) } },
                doc! { "name": "email_1", "accesses": { "ops": 0_i64, "since": at(start + 900) } },
                doc! { "name": "cat_at", "accesses": { "ops": 0_i64, "since": at(late) } },
            ])
        };
        // Loaded at the server's start (ms apart), cat_at created an hour later.
        let r = assemble(&specs(), Some(&rows(start + 3_600_000)), Some(4), None, None, Some(start)).derived();
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap().clone();
        assert!(get("email_1").unused && get("email_1").updates == 4);
        assert!(!get("cat_at").unused && get("cat_at").updates == 0);
        assert!(r.note.as_deref().unwrap().contains("Un índice se creó después"));
        // A collection created after the server's start: no slack, an index
        // created a few ms after `_id_` doesn't inherit its writes.
        let r = assemble(&specs(), Some(&rows(start + 3_600_000)), Some(4), None, None, Some(start - 86_400_000)).derived();
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap().clone();
        assert!(!get("email_1").unused && get("email_1").updates == 0);
        assert!(r.note.as_deref().unwrap().contains("2 índices se crearon"));
        // All loaded together: no note.
        let r = assemble(&specs(), Some(&rows(start + 100)), Some(4), None, None, Some(start)).derived();
        assert!(r.note.is_none() && r.indexes.iter().find(|i| i.name == "cat_at").unwrap().unused);
    }

    #[test]
    fn without_index_stats() {
        let r = assemble(&specs(), None, Some(9), None, Some("x".into()), None).derived();
        assert!(!r.stats_available);
        assert_eq!(r.note.as_deref(), Some("x"));
        assert!(r.indexes.iter().all(|i| i.updates == 0 && !i.unused));
        assert_eq!(r.indexes.len(), 6);
    }
}
