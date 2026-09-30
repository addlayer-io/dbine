//! Sync by rows ("sincronizar solo lo que cambió"), through the driver
//! contract ([`dbine_driver::DeltaSpec`]).
//!
//! Per table:
//! 1. **Buckets.** When the first key column is an integer (the source's
//!    [`Session::key_range`] answers), [`Buckets::Range`]: about
//!    [`ROWS_PER_BUCKET`] rows each between the source's smallest and
//!    largest value; rows only on the target fall in the edge buckets `-1`
//!    and `n`. Otherwise [`Buckets::Hash`] with a **prime** `n` (17 to
//!    65 537), so a hash that rotates by column doesn't ignore the leading
//!    ones.
//! 2. **Summary.** Both sides count and sum their buckets at once
//!    ([`Session::delta_summary`]; the source only reads).
//! 3. **Compare.** [`dbine_driver::transfer::changed_buckets`].
//! 4. **Apply.** With at most [`MAX_FILTERED_BUCKETS`] changed buckets and
//!    no more than half of them, only their source rows are read
//!    ([`Driver::delta_filter`]) and merged
//!    ([`Session::delta_apply`] with those buckets). Otherwise the whole
//!    table is read and merged: `delta_apply` gets an **empty bucket list,
//!    which means every bucket** (the whole table, no bucket filter on the
//!    target), and the source is read without a filter.
//!
//! The merge is one target transaction: a failed sync leaves the target as
//! it was, so it simply runs again. The table is never emptied.

use crate::copy::{self, CopyInput, DeltaWrite};
use crate::event::{CopyPath, CopyStats, LogLevel, Phase};
use crate::job::TransferJob;
use crate::retry::is_transient;
use dbine_driver::transfer::{changed_buckets, Progress};
use dbine_driver::{BucketSum, Buckets, DeltaDepth, DeltaResult, DeltaSpec, Driver, Error, Result, Session};
use std::time::Instant;

/// Rows per bucket, roughly.
pub const ROWS_PER_BUCKET: u64 = 1_000;
/// Hash buckets: the smallest and largest prime used.
pub const MIN_HASH_BUCKETS: u64 = 17;
pub const MAX_HASH_BUCKETS: u64 = 65_537;
/// Range buckets: at most this many (plus the two edges).
pub const MAX_RANGE_BUCKETS: u64 = 65_536;
/// More changed buckets than this (or than half of them) and the whole
/// table is merged instead of a filtered read.
pub const MAX_FILTERED_BUCKETS: usize = 5_000;

/// Range buckets over `lo..=hi` for about `rows` rows.
pub fn range_buckets(column: &str, lo: i64, hi: i64, rows: u64) -> Buckets {
    let (lo, hi) = (lo.min(hi), lo.max(hi));
    let span = hi as i128 - lo as i128 + 1;
    let want = (rows / ROWS_PER_BUCKET).clamp(1, MAX_RANGE_BUCKETS) as i128;
    let width = ((span + want - 1) / want).clamp(1, i64::MAX as i128);
    let n = (span + width - 1) / width;
    Buckets::Range { column: column.to_string(), lo, hi, width: width as i64, n: n as u64 }
}

/// Hash buckets for about `rows` rows: a prime in 17..=65 537.
pub fn hash_buckets(rows: u64) -> Buckets {
    let want = (rows / ROWS_PER_BUCKET).clamp(MIN_HASH_BUCKETS, MAX_HASH_BUCKETS);
    let n = (want..=MAX_HASH_BUCKETS).find(|&k| is_prime(k)).unwrap_or(MAX_HASH_BUCKETS);
    Buckets::Hash { n }
}

fn is_prime(n: u64) -> bool {
    n >= 2 && (2..).take_while(|d| d * d <= n).all(|d| !n.is_multiple_of(d))
}

/// Buckets a table has, edges included.
pub fn bucket_count(b: &Buckets) -> u64 {
    match b {
        Buckets::Range { n, .. } => n + 2,
        Buckets::Hash { n } => *n,
    }
}

/// Whether `changed` buckets out of `buckets` are applied filtered (only
/// their rows) rather than as the whole table.
pub fn apply_filtered(changed: usize, buckets: &Buckets) -> bool {
    changed <= MAX_FILTERED_BUCKETS && changed as u64 <= bucket_count(buckets) / 2
}

/// What a sync needs besides its sessions.
pub(crate) struct DeltaInput<'a> {
    pub job: &'a TransferJob,
    pub key: &'a [String],
    pub depth: DeltaDepth,
    pub max_cores: u32,
    pub source_driver: &'a dyn Driver,
    pub target_driver: &'a dyn Driver,
    pub phase: &'a (dyn Fn(Phase) + Send + Sync),
    /// The rows to review, once known.
    pub total: &'a (dyn Fn(u64) + Send + Sync),
    pub progress: Progress<'a>,
    pub log: &'a (dyn Fn(LogLevel, String) + Send + Sync),
}

/// Both ends can sync by rows with each other, or why not (Spanish).
pub(crate) fn check_support(source: &dyn Driver, target: &dyn Driver) -> Result<()> {
    if source.info().id != target.info().id {
        return Err(Error::Unsupported(format!(
            "sincronizar por filas necesita el mismo motor en origen y destino ({} → {})",
            source.info().name,
            target.info().name
        )));
    }
    if !source.supports_delta() || !target.supports_delta() {
        return Err(Error::Unsupported(format!("{} no sincroniza por filas", source.info().name)));
    }
    Ok(())
}

/// Sync the table's rows from `src` into `tgt`.
pub(crate) async fn sync_table(input: DeltaInput<'_>, mut src: Box<dyn Session>, tgt: &mut dyn Session) -> Result<CopyStats> {
    let job = input.job;
    let name = &job.name;
    if input.key.is_empty() {
        return Err(Error::State("para sincronizar por filas hace falta una clave".into()));
    }
    if job.source.filter.is_some() {
        return Err(Error::Unsupported("una tabla con filtro no se sincroniza por filas".into()));
    }
    let start = Instant::now();
    (input.phase)(Phase::Summary);

    let first = &input.key[0];
    let buckets = match src.key_range(&job.source.table, first).await {
        Ok(Some((lo, hi, rows))) => range_buckets(first, lo, hi, rows),
        // An empty source: one bucket; every target row is on an edge.
        Ok(None) => range_buckets(first, 0, 0, 0),
        Err(e) if is_transient(&e) || matches!(e, Error::Cancelled) => return Err(e),
        Err(e) => {
            (input.log)(LogLevel::Info, format!("{name}: la clave no es entera ({e}); se agrupa por hash"));
            hash_buckets(job.row_estimate.unwrap_or(0))
        }
    };
    let source_columns = job.source.columns.clone().unwrap_or_else(|| job.target.columns.clone());
    let spec_for = |table: &dbine_driver::ObjectRef, columns: Vec<String>| DeltaSpec {
        table: table.clone(),
        key: input.key.to_vec(),
        columns,
        buckets: buckets.clone(),
        depth: input.depth,
        max_cores: input.max_cores,
    };
    let source_spec = spec_for(&job.source.table, source_columns);
    let target_spec = spec_for(&job.target.table, job.target.columns.clone());

    let (a, b) = tokio::join!(src.delta_summary(&source_spec), tgt.delta_summary(&target_spec));
    let (a, b) = (a?, b?);

    (input.phase)(Phase::Compare);
    let changed = changed_buckets(&a, &b);
    let total = bucket_count(&buckets);
    (input.log)(LogLevel::Info, format!("{name}: {} de {total} grupos distintos", changed.len()));
    if changed.is_empty() {
        (input.total)(0);
        let mut stats = CopyStats::measure(CopyPath::Delta, 0, start.elapsed(), Default::default(), Default::default());
        stats.delta = Some(DeltaResult::default());
        return Ok(stats);
    }

    let filtered = apply_filtered(changed.len(), &buckets);
    let (apply, filter, to_review) = if filtered {
        let filter = input.source_driver.delta_filter(&source_spec, &changed)?;
        (changed.clone(), Some(filter), rows_in(&a, &changed))
    } else {
        (Vec::new(), None, a.iter().map(|s| s.rows).sum())
    };
    (input.total)(to_review);

    (input.phase)(Phase::Apply);
    let copy = CopyInput {
        job,
        source_driver: input.source_driver,
        target_driver: input.target_driver,
        native: false,
        progress: input.progress,
        log: input.log,
        delta: Some(DeltaWrite { spec: &target_spec, buckets: &apply, filter }),
    };
    let mut stats = copy::copy_table(copy, src, tgt).await?;
    stats.elapsed_ms = start.elapsed().as_millis() as u64;
    stats.rows_per_s = crate::event::rate(stats.rows, start.elapsed());
    if let Some(r) = &stats.delta {
        (input.log)(
            LogLevel::Info,
            format!("{name}: {} insertadas, {} actualizadas, {} borradas", r.inserted, r.updated, r.deleted),
        );
    }
    Ok(stats)
}

/// Source rows in these buckets.
fn rows_in(sums: &[BucketSum], buckets: &[i64]) -> u64 {
    sums.iter().filter(|s| buckets.binary_search(&s.bucket).is_ok()).map(|s| s.rows).sum()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn range_sizes() {
        match range_buckets("id", 1, 10_000, 10_000) {
            Buckets::Range { lo, hi, width, n, .. } => assert_eq!((lo, hi, width, n), (1, 10_000, 1_000, 10)),
            b => panic!("{b:?}"),
        }
        // Fewer values than buckets wanted.
        match range_buckets("id", 5, 7, 5_000_000) {
            Buckets::Range { width, n, .. } => assert_eq!((width, n), (1, 3)),
            b => panic!("{b:?}"),
        }
        // The whole i64 range doesn't overflow.
        match range_buckets("id", i64::MIN, i64::MAX, 10) {
            Buckets::Range { width, n, .. } => assert!(width > 0 && n >= 1),
            b => panic!("{b:?}"),
        }
    }

    #[test]
    fn hash_is_prime_in_range() {
        for rows in [0, 10_000, 17_000, 100_000, 1_000_000, 10_000_000_000] {
            let Buckets::Hash { n } = hash_buckets(rows) else { unreachable!() };
            assert!(is_prime(n) && (MIN_HASH_BUCKETS..=MAX_HASH_BUCKETS).contains(&n), "{rows}: {n}");
        }
        assert!(matches!(hash_buckets(100_000), Buckets::Hash { n: 101 }));
    }

    #[test]
    fn filtered_threshold() {
        let b = Buckets::Hash { n: 17 };
        assert!(apply_filtered(8, &b));
        assert!(!apply_filtered(9, &b));
        let big = Buckets::Hash { n: 65_537 };
        assert!(apply_filtered(5_000, &big));
        assert!(!apply_filtered(5_001, &big));
    }
}
