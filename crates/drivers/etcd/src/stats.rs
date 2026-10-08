//! What the server already knows, for documenting it
//! ([`dbine_driver::Session::row_estimates`]).
//!
//! - Rows: the member's key count, the `etcd_debugging_mvcc_keys_total`
//!   gauge of `/metrics` (the MVCC store keeps it), never a `count_only`
//!   range, which walks the key index. It covers the whole keyspace, so a
//!   connection limited to a prefix gets nothing: etcd keeps no count per
//!   prefix. Without access to `/metrics`, nothing either.
//! - Comments: etcd keeps none.

use crate::{prom, EtcdSession};
use dbine_driver::stats::RowEstimate;
use dbine_driver::{ObjectRef, Result};

/// The object kind of the keyspace's estimate.
pub(crate) const KIND_DATABASE: &str = "database";

/// The key count among the member's samples.
pub(crate) fn keys_total(m: &prom::Samples) -> Option<u64> {
    ["etcd_debugging_mvcc_keys_total", "etcd_mvcc_keys_total"].iter().find_map(|n| m.sum(n)).filter(|n| *n >= 0.0).map(|n| n as u64)
}

pub(crate) async fn row_estimates(s: &EtcdSession) -> Result<Vec<RowEstimate>> {
    if s.prefix.is_some() {
        return Ok(Vec::new());
    }
    let Ok(m) = s.metrics().await else { return Ok(Vec::new()) };
    Ok(keys_total(&m)
        .map(|rows| RowEstimate { object: ObjectRef { kind: KIND_DATABASE.into(), schema: None, name: "default".into() }, rows })
        .into_iter()
        .collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_from_metrics() {
        let m = prom::Samples::parse("# TYPE etcd_debugging_mvcc_keys_total gauge\netcd_debugging_mvcc_keys_total 42\n");
        assert_eq!(keys_total(&m), Some(42));
        assert_eq!(keys_total(&prom::Samples::parse("etcd_server_has_leader 1\n")), None);
    }
}
