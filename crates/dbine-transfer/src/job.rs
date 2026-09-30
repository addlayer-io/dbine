//! What a run copies ([`TransferJob`]) and how ([`RunOptions`]).

use crate::slots::MAX_PARALLEL;
use dbine_driver::{transfer::CHUNK_ROWS, DeltaDepth, LoadSpec, ReadSpec, TransferColumn};
use serde::{Deserialize, Serialize};

/// One table to copy.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct TransferJob {
    /// Shown in the UI and the table's key in the run's state: unique per run.
    pub name: String,
    /// What to read on the source.
    pub source: ReadSpec,
    /// Where to load it, `columns` in the order of the read's cells. The
    /// run's [`RunOptions`] overwrite its lock, identity and commit settings.
    pub target: LoadSpec,
    /// Rows the catalog estimates: orders the run and shows progress.
    #[serde(default)]
    pub row_estimate: Option<u64>,
    /// Target script that empties the table. Without it, a table left half
    /// copied can't be copied again.
    #[serde(default)]
    pub truncate: Option<String>,
    /// "Vaciar y copiar": empty the target table (with `truncate`) before
    /// the first copy.
    #[serde(default)]
    pub empty_first: bool,
    /// Statements before the `insert_script` batches
    /// ([`dbine_driver::Driver::data_load_wrap`]).
    #[serde(default)]
    pub before: String,
    /// Statements after the rows are loaded, whatever the path (native
    /// copy, bulk load or `insert_script`): sequence resync, identity
    /// restart…
    #[serde(default)]
    pub after: String,
    /// Statements run right after the copy (the table's indexes), while
    /// other tables go on copying. They run again after a cut, so they
    /// should be idempotent.
    #[serde(default)]
    pub post: Vec<String>,
    /// The target table existed before the run: it's only copied into when
    /// it has no rows, unless `empty_first`.
    #[serde(default)]
    pub preexisting: bool,
    /// Columns the target must have, checked before copying (names ignoring
    /// case; types only between the same engine). Empty: no check.
    #[serde(default)]
    pub expected_columns: Vec<TransferColumn>,
    /// Copy every row (the default) or sync only what changed.
    #[serde(default)]
    pub mode: TransferMode,
}

/// How a table's rows reach the target.
#[derive(Debug, Clone, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum TransferMode {
    /// Every row, into an empty table (see [`TransferJob::truncate`]).
    #[default]
    Copy,
    /// Sync by rows ("sincronizar solo lo que cambió"): only the rows that
    /// differ are inserted, updated or deleted, in one target transaction.
    /// Both ends must be the same driver, with
    /// [`dbine_driver::Driver::supports_delta`]. The table is never
    /// emptied. See [`crate::delta`].
    Delta {
        /// The key (primary key or a unique, not-null one), target names.
        key: Vec<String>,
        depth: DeltaDepth,
        /// Cores the engine may use for the summary (0: the server decides).
        #[serde(default)]
        max_cores: u32,
    },
}

/// Which tables start first.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyOrder {
    /// Longest job first: the shortest wall time. Unknown sizes last.
    #[default]
    LargestFirst,
    SmallestFirst,
    Alphabetical,
}

/// How a run goes.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(default)]
pub struct RunOptions {
    /// Tables copying at once (1 to [`MAX_PARALLEL`]); can change live.
    pub parallel: usize,
    pub order: CopyOrder,
    /// Commit every this many rows…
    pub commit_rows: u64,
    /// …or bytes, whichever comes first.
    pub commit_bytes: u64,
    /// Retries of a table after a transient error.
    pub max_retries: u32,
    /// First wait before retrying; doubles each time, up to 30 s.
    pub backoff_ms: u64,
    /// Stop the run at the first table that fails.
    pub fail_fast: bool,
    /// Lock the target table while loading.
    pub table_lock: bool,
    /// Keep the source's identity / auto-increment values.
    pub keep_identity: bool,
}

impl Default for RunOptions {
    fn default() -> Self {
        RunOptions {
            parallel: 8,
            order: CopyOrder::LargestFirst,
            commit_rows: LoadSpec::DEFAULT_COMMIT_ROWS,
            commit_bytes: LoadSpec::DEFAULT_COMMIT_BYTES,
            max_retries: 3,
            backoff_ms: 1_000,
            fail_fast: false,
            table_lock: true,
            keep_identity: true,
        }
    }
}

impl RunOptions {
    /// Values in range: parallel 1..=32; commit windows end on a batch.
    pub(crate) fn normalized(mut self) -> Self {
        self.parallel = self.parallel.clamp(1, MAX_PARALLEL);
        self.commit_rows = self.commit_rows.max(CHUNK_ROWS as u64);
        self.commit_bytes = self.commit_bytes.max(1);
        self
    }

    /// The run's load settings on a table's [`LoadSpec`].
    pub(crate) fn apply(&self, load: &mut LoadSpec) {
        load.table_lock = self.table_lock;
        load.keep_identity = self.keep_identity;
        load.commit_rows = self.commit_rows;
        load.commit_bytes = self.commit_bytes;
    }
}

/// Order the tables (stable: ties keep the app's order).
pub(crate) fn sort_jobs(jobs: &mut [TransferJob], order: CopyOrder) {
    match order {
        // `None` sorts below any `Some`: reversed, unknown sizes go last.
        CopyOrder::LargestFirst => jobs.sort_by_key(|j| std::cmp::Reverse(j.row_estimate)),
        CopyOrder::SmallestFirst => jobs.sort_by_key(|j| (j.row_estimate.is_none(), j.row_estimate)),
        CopyOrder::Alphabetical => jobs.sort_by_key(|j| j.name.to_lowercase()),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ObjectRef;

    fn job(name: &str, rows: Option<u64>) -> TransferJob {
        let table = ObjectRef { kind: "table".into(), schema: None, name: name.into() };
        TransferJob {
            name: name.into(),
            source: ReadSpec { table: table.clone(), columns: None, filter: None },
            target: LoadSpec { table, columns: vec![], table_lock: false, keep_identity: false, commit_rows: 1, commit_bytes: 1 },
            row_estimate: rows,
            truncate: None,
            empty_first: false,
            before: String::new(),
            after: String::new(),
            post: vec![],
            preexisting: false,
            expected_columns: vec![],
            mode: TransferMode::Copy,
        }
    }

    #[test]
    fn orders() {
        let mut jobs = vec![job("b", Some(10)), job("a", None), job("c", Some(300)), job("d", Some(10))];
        sort_jobs(&mut jobs, CopyOrder::LargestFirst);
        assert_eq!(jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(), ["c", "b", "d", "a"]);
        sort_jobs(&mut jobs, CopyOrder::SmallestFirst);
        assert_eq!(jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(), ["b", "d", "c", "a"]);
        sort_jobs(&mut jobs, CopyOrder::Alphabetical);
        assert_eq!(jobs.iter().map(|j| j.name.as_str()).collect::<Vec<_>>(), ["a", "b", "c", "d"]);
    }

    #[test]
    fn options_in_range() {
        let o = RunOptions { parallel: 99, commit_rows: 10, ..Default::default() }.normalized();
        assert_eq!(o.parallel, MAX_PARALLEL);
        assert_eq!(o.commit_rows, CHUNK_ROWS as u64);
        assert_eq!(RunOptions { parallel: 0, ..Default::default() }.normalized().parallel, 1);
    }
}
