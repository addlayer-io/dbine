//! What the catalog already knows, for documenting a keyspace
//! ([`dbine_driver::Session::row_estimates`],
//! [`dbine_driver::Session::object_comments`]).
//!
//! - Rows: the partition estimates the node keeps for its primary token
//!   ranges (`system.table_estimates` with `range_type = 'primary'` on
//!   Cassandra 4.0+, `system.size_estimates` before it and on ScyllaDB),
//!   the same figures `nodetool tablestats` shows. They're local, so the
//!   node's share is extrapolated to the whole ring by the fraction of the
//!   ring those ranges cover (Murmur3 spreads partitions evenly). They
//!   count partitions, not CQL rows: a table with clustering columns has
//!   more rows than that. Amazon Keyspaces has neither table: nothing.
//! - Comments: a table's `comment` arrives with the schema; a materialized
//!   view keeps its own in `system_schema.views`. Types and functions have
//!   none.

use crate::{int, text, CassandraSession};
use dbine_driver::stats::{ObjectComment, RowEstimate};
use dbine_driver::{kinds, ObjectRef, Result};
use std::collections::BTreeMap;

/// The Murmur3 ring: 2^64 tokens.
const RING: f64 = 18_446_744_073_709_551_616.0;

/// One token range's estimate.
pub(crate) struct RangeEstimate {
    pub table: String,
    pub start: String,
    pub end: String,
    pub partitions: i64,
}

/// Size of a token range `(start, end]` of the Murmur3 ring (wrapping past
/// the end); `None` for tokens of other partitioners.
fn range_len(start: &str, end: &str) -> Option<f64> {
    let (s, e) = (start.parse::<i64>().ok()? as i128, end.parse::<i64>().ok()? as i128);
    let len = if e > s { e - s } else { e - s + (1i128 << 64) };
    Some(len as f64)
}

/// Partitions per table: the ranges' sum, scaled to the whole ring when
/// the tokens are Murmur3's (a node owning a quarter of the ring holds
/// about a quarter of the partitions).
pub(crate) fn per_table(ranges: &[RangeEstimate]) -> BTreeMap<String, u64> {
    let mut acc: BTreeMap<&str, (f64, Option<f64>)> = BTreeMap::new();
    for r in ranges {
        let e = acc.entry(&r.table).or_insert((0.0, Some(0.0)));
        e.0 += r.partitions.max(0) as f64;
        e.1 = match (e.1, range_len(&r.start, &r.end)) {
            (Some(total), Some(len)) => Some(total + len),
            _ => None,
        };
    }
    acc.into_iter()
        .map(|(t, (n, covered))| {
            let share = covered.map(|c| c / RING).filter(|s| *s > 0.0 && *s <= 1.0).unwrap_or(1.0);
            (t.to_string(), (n / share).round() as u64)
        })
        .collect()
}

impl CassandraSession {
    async fn ranges(&self, ks: &str) -> Vec<RangeEstimate> {
        let read = |r: &scylla::value::Row| RangeEstimate { table: text(r, 0), start: text(r, 1), end: text(r, 2), partitions: int(r, 3) };
        // Cassandra 4.0+: `primary` (the node's own ranges) beside `local_primary`.
        let cql = "SELECT table_name, range_start, range_end, partitions_count, range_type FROM system.table_estimates WHERE keyspace_name = ?";
        if let Ok(rows) = self.rows(cql, (ks,)).await {
            return rows.iter().filter(|r| text(r, 4) == "primary").map(read).collect();
        }
        let cql = "SELECT table_name, range_start, range_end, partitions_count FROM system.size_estimates WHERE keyspace_name = ?";
        self.rows(cql, (ks,)).await.map(|rows| rows.iter().map(read).collect()).unwrap_or_default()
    }

    /// The keyspace's materialized views (without ScyllaDB's index views).
    async fn views(&self, ks: &str) -> Vec<(String, String)> {
        let indexes: Vec<String> = self
            .rows("SELECT index_name FROM system_schema.indexes WHERE keyspace_name = ?", (ks,))
            .await
            .map(|rows| rows.iter().map(|r| format!("{}_index", text(r, 0))).collect())
            .unwrap_or_default();
        self.rows("SELECT view_name, comment FROM system_schema.views WHERE keyspace_name = ?", (ks,))
            .await
            .map(|rows| rows.iter().map(|r| (text(r, 0), text(r, 1))).filter(|(v, _)| !indexes.contains(v)).collect())
            .unwrap_or_default()
    }

    pub(crate) async fn stats_rows(&self) -> Result<Vec<RowEstimate>> {
        let Some(ks) = self.keyspace.clone() else { return Ok(Vec::new()) };
        let ranges = self.ranges(&ks).await;
        if ranges.is_empty() {
            return Ok(Vec::new());
        }
        let views: Vec<String> = self.views(&ks).await.into_iter().map(|(v, _)| v).collect();
        Ok(per_table(&ranges)
            .into_iter()
            .map(|(name, rows)| {
                let kind = if views.contains(&name) { kinds::MATERIALIZED_VIEW } else { kinds::TABLE };
                RowEstimate { object: ObjectRef { kind: kind.into(), schema: Some(ks.clone()), name }, rows }
            })
            .collect())
    }

    pub(crate) async fn stats_comments(&self) -> Result<Vec<ObjectComment>> {
        let Some(ks) = self.keyspace.clone() else { return Ok(Vec::new()) };
        Ok(self
            .views(&ks)
            .await
            .into_iter()
            .filter(|(_, c)| !c.trim().is_empty())
            .map(|(name, comment)| ObjectComment {
                object: ObjectRef { kind: kinds::MATERIALIZED_VIEW.into(), schema: None, name },
                comment,
            })
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn r(table: &str, start: i64, end: i64, partitions: i64) -> RangeEstimate {
        RangeEstimate { table: table.into(), start: start.to_string(), end: end.to_string(), partitions }
    }

    #[test]
    fn whole_ring_counts_as_is() {
        // One node owning everything: one range that wraps around.
        let m = per_table(&[r("t", i64::MIN, i64::MIN, 42)]);
        assert_eq!(m["t"], 42);
    }

    #[test]
    fn a_quarter_of_the_ring_is_scaled_up() {
        let q = 1i64 << 62;
        let m = per_table(&[r("t", 0, q / 2, 10), r("t", q / 2, q, 15), r("u", 0, q, 0)]);
        assert_eq!(m["t"], 100);
        assert_eq!(m["u"], 0);
    }

    #[test]
    fn other_partitioners_keep_the_sum() {
        let m = per_table(&[RangeEstimate { table: "t".into(), start: "abc".into(), end: "def".into(), partitions: 7 }]);
        assert_eq!(m["t"], 7);
    }

    #[test]
    fn wrapping_range_length() {
        assert_eq!(range_len("10", "-10"), Some(RING - 20.0));
        assert_eq!(range_len("-10", "10"), Some(20.0));
    }
}
