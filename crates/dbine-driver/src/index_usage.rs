//! How a table's indexes are used (`Session::index_usage`): the engine's
//! counters since they started (seeks, scans, lookups, updates), each
//! index's definition and size, and the table's foreign keys (the explorer
//! marks those columns from the same read).
//!
//! The numbers the UI shows are derived here, in one place
//! ([`IndexUsageReport::derive`]): reads = seeks + scans + lookups; the read
//! share = this index's reads over the sum of the table's indexes' reads
//! (`None` when that sum is 0); unused = no reads but updates (an index that
//! costs on every write and helps no query).

use crate::schema::ForeignKeyDef;
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexUsageReport {
    /// When the counters started (`yyyy-mm-dd hh:mm:ss`, server time);
    /// `None` when the engine doesn't say.
    #[serde(default)]
    pub since: Option<String>,
    /// The counters were read. `false`: the indexes are listed but the
    /// login can't see their usage (see `note`), and every counter is 0.
    #[serde(default)]
    pub stats_available: bool,
    /// What the UI tells the user about the numbers (a missing permission…).
    #[serde(default)]
    pub note: Option<String>,
    #[serde(default)]
    pub indexes: Vec<IndexUsage>,
    /// The table's foreign keys (the explorer's link icons).
    #[serde(default)]
    pub foreign_keys: Vec<ForeignKeyDef>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
pub struct IndexUsage {
    pub name: String,
    /// The engine's kind (`CLUSTERED`, `NONCLUSTERED`, `CLUSTERED COLUMNSTORE`…).
    pub kind: String,
    #[serde(default)]
    pub unique: bool,
    #[serde(default)]
    pub primary_key: bool,
    /// In key order; descending ones end in ` DESC`.
    #[serde(default)]
    pub key_columns: Vec<String>,
    #[serde(default)]
    pub included_columns: Vec<String>,
    #[serde(default)]
    pub filter: Option<String>,
    #[serde(default)]
    pub size_kb: Option<u64>,
    #[serde(default)]
    pub seeks: u64,
    #[serde(default)]
    pub scans: u64,
    #[serde(default)]
    pub lookups: u64,
    #[serde(default)]
    pub updates: u64,
    #[serde(default)]
    pub last_read: Option<String>,
    #[serde(default)]
    pub last_write: Option<String>,
    /// Derived ([`IndexUsageReport::derive`]): seeks + scans + lookups.
    #[serde(default)]
    pub reads: u64,
    /// Derived: share of the table's reads, 0–1.
    #[serde(default)]
    pub read_share: Option<f64>,
    /// Derived: written but never read.
    #[serde(default)]
    pub unused: bool,
    /// Derived: updates / reads (`None` without reads).
    #[serde(default)]
    pub writes_per_read: Option<f64>,
}

impl IndexUsageReport {
    /// Fills the derived numbers of every index. Without counters
    /// (`stats_available` false) there is no share and nothing is unused.
    pub fn derive(&mut self) {
        let stats = self.stats_available;
        for i in &mut self.indexes {
            i.reads = if stats { i.seeks + i.scans + i.lookups } else { 0 };
        }
        let total: u64 = self.indexes.iter().map(|i| i.reads).sum();
        for i in &mut self.indexes {
            i.read_share = (stats && total > 0).then(|| i.reads as f64 / total as f64);
            i.unused = stats && i.reads == 0 && i.updates > 0;
            i.writes_per_read = (stats && i.reads > 0).then(|| i.updates as f64 / i.reads as f64);
        }
    }

    /// [`Self::derive`], by value.
    pub fn derived(mut self) -> Self {
        self.derive();
        self
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ix(name: &str, seeks: u64, scans: u64, lookups: u64, updates: u64) -> IndexUsage {
        IndexUsage { name: name.into(), kind: "NONCLUSTERED".into(), seeks, scans, lookups, updates, ..Default::default() }
    }

    #[test]
    fn reads_share_and_unused() {
        let r = IndexUsageReport {
            stats_available: true,
            indexes: vec![ix("pk", 6, 1, 0, 10), ix("a", 2, 0, 1, 4), ix("b", 0, 0, 0, 7), ix("c", 0, 0, 0, 0)],
            ..Default::default()
        }
        .derived();
        let get = |n: &str| r.indexes.iter().find(|i| i.name == n).unwrap();
        assert_eq!(get("pk").reads, 7);
        assert_eq!(get("a").reads, 3);
        assert_eq!(get("pk").read_share, Some(0.7));
        assert_eq!(get("a").read_share, Some(0.3));
        assert_eq!(get("b").read_share, Some(0.0));
        assert!(get("b").unused, "written, never read");
        assert!(!get("c").unused, "never written either");
        assert!(!get("a").unused);
        assert_eq!(get("a").writes_per_read.map(|v| (v * 1000.0).round() / 1000.0), Some(1.333));
        assert_eq!(get("b").writes_per_read, None);
    }

    #[test]
    fn no_reads_means_no_share() {
        let r = IndexUsageReport { stats_available: true, indexes: vec![ix("a", 0, 0, 0, 3), ix("b", 0, 0, 0, 0)], ..Default::default() }.derived();
        assert!(r.indexes.iter().all(|i| i.read_share.is_none()));
        assert!(r.indexes[0].unused);
    }

    #[test]
    fn without_stats_nothing_is_derived() {
        let r = IndexUsageReport { stats_available: false, indexes: vec![ix("a", 5, 0, 0, 3), ix("b", 0, 0, 0, 9)], ..Default::default() }.derived();
        assert!(r.indexes.iter().all(|i| i.reads == 0 && i.read_share.is_none() && !i.unused && i.writes_per_read.is_none()));
    }

    #[test]
    fn reads_without_derived_fields() {
        // A host that didn't fill the derived fields (they're recomputed by the app).
        let r: IndexUsageReport = serde_json::from_str(r#"{"indexes":[{"name":"a","kind":"CLUSTERED","seeks":1}]}"#).unwrap();
        assert_eq!((r.indexes[0].reads, r.indexes[0].read_share, r.stats_available), (0, None, false));
    }
}
