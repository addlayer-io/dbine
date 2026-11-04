//! Row counts of the explorer's tables ([`dbine_driver::Session::row_estimates`]),
//! from what kdb+ already knows: a table in memory or splayed is a set of
//! vectors (mapped, for a splayed one) whose length is the count, so
//! `count` reads nothing. A partitioned table's count is the sum of its
//! partitions', which `count` would read from every partition: only what
//! kdb+ already counted (`.Q.pn`, filled by an earlier count) is used,
//! and a table it hasn't counted yet is left out.

use crate::ipc::K;
use crate::{EXPLORER, PARTITIONED, SPLAYED};
use dbine_driver::{kinds, stats::RowEstimate, ObjectRef};

/// Applied to [`EXPLORER`]'s table: per table its qualified name (`n`),
/// kind (`k`) and rows (`r`, `0N` when unknown), each one under
/// protection.
pub const ROWS: &str = concat!(
    "{[e] e:select from e where k in `table`splayed`partitioned;",
    " q:{$[x=`.;y;`$(string x),\".\",string y]}'[e`ns;e`n];",
    " c:{[k;n] @[{[k;n] $[k=`partitioned;$[count p:.Q.pn n;$[any null p;0N;sum p];0N];count get n]}k;n;0N]}'[e`k;q];",
    " ([] n:q; k:e`k; r:c)}"
);

/// The explorer's tables and their counts in one round trip.
pub fn query() -> String {
    format!("{ROWS}[{EXPLORER}]")
}

/// A count as q gives it (`0N` and negatives: none).
fn rows(k: &K) -> Option<u64> {
    match k {
        K::Long(n) if *n >= 0 => Some(*n as u64),
        K::Int(n) if *n >= 0 => Some(*n as u64),
        _ => None,
    }
}

/// The rows of [`ROWS`]'s table, as the explorer names the objects.
pub fn estimates(k: K) -> Vec<RowEstimate> {
    let K::Table(names, cols) = k else {
        return Vec::new();
    };
    let col = |n: &str| names.iter().position(|c| c == n).map(|i| &cols[i]);
    let (Some(n), Some(kind), Some(r)) = (col("n"), col("k"), col("r")) else {
        return Vec::new();
    };
    let (n, kind) = (
        n.symbols().unwrap_or_default(),
        kind.symbols().unwrap_or_default(),
    );
    n.into_iter()
        .zip(kind)
        .enumerate()
        .filter_map(|(i, (name, kind))| {
            let rows = rows(&r.item(i)?)?;
            let kind = match kind.as_str() {
                "partitioned" => PARTITIONED,
                "splayed" => SPLAYED,
                _ => kinds::TABLE,
            };
            Some(RowEstimate {
                object: ObjectRef {
                    kind: kind.into(),
                    schema: None,
                    name,
                },
                rows,
            })
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::Vector;

    fn syms(v: &[&str]) -> K {
        K::Vector(0, Vector::Symbol(v.iter().map(|s| s.to_string()).collect()))
    }

    #[test]
    fn counts_only_what_kdb_already_knows() {
        let q = query();
        assert!(q.starts_with("{[e] e:select from e where k in `table`splayed`partitioned;"));
        assert!(q.ends_with(&format!("[{EXPLORER}]")), "{q}");
        // A partitioned table: the sum of `.Q.pn` when every partition is
        // counted, else nothing; any other: its `count`.
        assert!(ROWS.contains(
            "$[k=`partitioned;$[count p:.Q.pn n;$[any null p;0N;sum p];0N];count get n]"
        ));
        // Each one under protection.
        assert!(ROWS.contains("@[{[k;n] "));
        assert!(ROWS.contains("}k;n;0N]"));
    }

    #[test]
    fn rows_become_estimates() {
        let t = K::Table(
            vec!["n".into(), "k".into(), "r".into()],
            vec![
                syms(&["trade", "quotes", "daily", ".ns.t", "hdb"]),
                syms(&["table", "partitioned", "splayed", "table", "partitioned"]),
                K::Vector(0, Vector::Long(vec![5, 600, 3, 0, i64::MIN])),
            ],
        );
        let got: Vec<(String, String, u64)> = estimates(t)
            .into_iter()
            .map(|e| (e.object.kind, e.object.name, e.rows))
            .collect();
        assert_eq!(
            got,
            [
                ("table".to_string(), "trade".to_string(), 5),
                (PARTITIONED.into(), "quotes".into(), 600),
                (SPLAYED.into(), "daily".into(), 3),
                ("table".into(), ".ns.t".into(), 0),
            ]
        );
        assert!(estimates(K::Nil).is_empty());
    }
}
