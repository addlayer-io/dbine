//! What the engine tells the UI while a run goes.

use dbine_driver::DeltaResult;
use serde::{Deserialize, Serialize};
use std::time::Duration;

/// A run's news, in order. `table` is the job's name.
#[derive(Debug, Clone, Serialize)]
#[serde(tag = "event", rename_all = "snake_case")]
pub enum Event {
    RunStarted { run_id: String, tables: usize, parallel: usize },
    /// An attempt at a table starts (`attempt` counts across resumes).
    TableStarted { table: String, attempt: u32 },
    TablePhase { table: String, phase: Phase },
    /// Committed rows; at most one every [`crate::PROGRESS_EVERY`] per
    /// table, plus the final one.
    TableProgress { table: String, rows_done: u64, rows_total: Option<u64>, rows_per_s: f64 },
    TableDone { table: String, rows: u64, stats: CopyStats },
    TableFailed { table: String, error: String },
    TableCancelled { table: String },
    RunFinished { summary: RunSummary },
    Log { level: LogLevel, text: String },
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Phase {
    /// Comparing the target's columns with the expected ones.
    Check,
    /// Emptying the target table.
    Truncate,
    Copy,
    /// The `post` statements (indexes).
    Indexes,
    /// Sync by rows: both sides sum their buckets.
    Summary,
    /// Sync by rows: the buckets that differ are picked.
    Compare,
    /// Sync by rows: the source rows of those buckets are merged into the
    /// target (one transaction).
    Apply,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum LogLevel {
    Info,
    Warn,
    Error,
}

/// How a table's rows traveled.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyPath {
    /// Inside the driver ([`dbine_driver::Driver::copy_native`]).
    Native,
    /// The target's native bulk load.
    BulkLoad,
    /// The driver's `insert_script`, batch by batch.
    InsertScript,
    /// Sync by rows ([`dbine_driver::Session::delta_apply`]).
    Delta,
}

/// The side that held the copy back.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Bottleneck {
    Source,
    Destination,
}

/// A table copy's figures.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct CopyStats {
    /// `None`: nothing was copied in this run (the table was already copied).
    pub path: Option<CopyPath>,
    pub rows: u64,
    pub elapsed_ms: u64,
    pub rows_per_s: f64,
    /// Time the writer waited for batches.
    pub waited_on_source_ms: u64,
    /// Time the reader waited for room (the writer was behind).
    pub waited_on_destination_ms: u64,
    /// A side waited ≥ 15 % of the time and ≥ 2× the other: the other one
    /// is the bottleneck.
    pub bottleneck: Option<Bottleneck>,
    /// Sync by rows: what changed on the target. `rows` are then the rows
    /// reviewed (the source rows of the buckets that differed).
    #[serde(default)]
    pub delta: Option<DeltaResult>,
}

impl CopyStats {
    pub(crate) fn measure(path: CopyPath, rows: u64, elapsed: Duration, waited_on_source: Duration, waited_on_destination: Duration) -> Self {
        let wall = elapsed.as_secs_f64();
        let (src, dst) = (waited_on_source.as_secs_f64(), waited_on_destination.as_secs_f64());
        let bottleneck = if wall <= 0.0 {
            None
        } else if dst >= 0.15 * wall && dst >= 2.0 * src {
            Some(Bottleneck::Destination)
        } else if src >= 0.15 * wall && src >= 2.0 * dst {
            Some(Bottleneck::Source)
        } else {
            None
        };
        CopyStats {
            path: Some(path),
            rows,
            elapsed_ms: elapsed.as_millis() as u64,
            rows_per_s: rate(rows, elapsed),
            waited_on_source_ms: waited_on_source.as_millis() as u64,
            waited_on_destination_ms: waited_on_destination.as_millis() as u64,
            bottleneck,
            delta: None,
        }
    }

    /// A table copied before (a resume only ran its `post`).
    pub(crate) fn already(rows: u64) -> Self {
        CopyStats { rows, ..Default::default() }
    }
}

/// Rows per second.
pub(crate) fn rate(rows: u64, elapsed: Duration) -> f64 {
    let s = elapsed.as_secs_f64();
    if s > 0.0 {
        rows as f64 / s
    } else {
        0.0
    }
}

/// How a run ended.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct RunSummary {
    pub run_id: String,
    pub status: crate::RunStatus,
    pub done: usize,
    pub failed: usize,
    pub cancelled: usize,
    /// Not finished: pending, or copied without their `post`.
    pub pending: usize,
    /// Committed rows of every table.
    pub rows: u64,
    pub elapsed_ms: u64,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bottleneck_verdict() {
        let s = CopyStats::measure(CopyPath::BulkLoad, 1000, Duration::from_secs(10), Duration::from_secs(1), Duration::from_secs(5));
        assert_eq!(s.bottleneck, Some(Bottleneck::Destination));
        assert_eq!(s.rows_per_s, 100.0);
        let s = CopyStats::measure(CopyPath::BulkLoad, 1, Duration::from_secs(10), Duration::from_secs(3), Duration::from_secs(2));
        assert_eq!(s.bottleneck, None);
        let s = CopyStats::measure(CopyPath::BulkLoad, 1, Duration::from_secs(10), Duration::from_secs(2), Duration::ZERO);
        assert_eq!(s.bottleneck, Some(Bottleneck::Source));
    }
}
