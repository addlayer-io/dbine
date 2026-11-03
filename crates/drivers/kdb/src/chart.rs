//! A chart over a query's whole result, aggregated on the server: the
//! query (one q statement that gives a table) inside a q-sql `select … by`,
//! with the columns [`aggregate::grouped_sql`] gives SQL engines
//! (`dbine_g0`, `dbine_g1`, `dbine_n`, `dbine_m0`…), sorted by the groups
//! and cut to `max_groups + 1` rows. The measures follow SQL's semantics:
//! `COUNT(c)` counts non-null cells, and `SUM`, `AVG`, `MIN`, `MAX` skip
//! nulls and give none over a group without numbers.

use crate::{edit, script};
use dbine_driver::aggregate::{self, AggFn, AggregateSpec};
use dbine_driver::{Error, Result};

fn unsupported() -> Error {
    Error::Unsupported("este motor no agrupa gráficos en el servidor".into())
}

/// A result column as q-sql names it, or `None` when it isn't a plain name.
fn name(c: &str) -> Option<&str> {
    (edit::plain(c) && !c.contains('.')).then_some(c)
}

/// `query` aggregated by `spec`, as one q statement.
pub fn grouped(query: &str, spec: &AggregateSpec) -> Result<String> {
    let spec = aggregate::validate(spec)?;
    let stmts = script::split(query);
    let [one] = stmts.as_slice() else {
        return Err(unsupported());
    };
    let inner = one.text.trim();
    if inner.starts_with('\\') {
        return Err(unsupported());
    }
    let mut cols = vec!["dbine_n:count i".to_string()];
    for (i, m) in spec.measures.iter().enumerate() {
        let e = match (&m.column, m.func) {
            (None, _) => "count i".to_string(),
            (Some(c), f) => {
                let c = name(c).ok_or_else(unsupported)?;
                match f {
                    // A list of strings has no nulls; other columns count
                    // their non-null cells.
                    AggFn::Count => format!("{{$[0h=type x;count x;sum not null x]}} {c}"),
                    AggFn::Sum => format!("{{$[all null x;0n;\"f\"$sum x]}} {c}"),
                    AggFn::Avg => format!("avg {c}"),
                    AggFn::Min => format!("min {c}"),
                    AggFn::Max => format!("max {c}"),
                }
            }
        };
        cols.push(format!("dbine_m{i}:{e}"));
    }
    let mut by = Vec::new();
    if let Some(x) = &spec.x {
        by.push(format!("dbine_g0:{}", name(x).ok_or_else(unsupported)?));
    }
    if let Some(s) = &spec.split {
        by.push(format!("dbine_g1:{}", name(s).ok_or_else(unsupported)?));
    }
    // The closing parenthesis on a line of its own: a trailing `/ comment`
    // in the query doesn't swallow it.
    let from = format!("from 0!({inner}\n )");
    if by.is_empty() {
        return Ok(format!("select {} {from}", cols.join(", ")));
    }
    let keys: String = (0..by.len()).map(|i| format!("`dbine_g{i}")).collect();
    Ok(format!(
        "{} sublist {keys} xasc 0!select {} by {} {from}",
        u64::from(spec.max_groups) + 1,
        cols.join(", "),
        by.join(", ")
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::aggregate::Measure;

    fn spec(
        x: Option<&str>,
        split: Option<&str>,
        measures: &[(Option<&str>, AggFn)],
    ) -> AggregateSpec {
        AggregateSpec {
            x: x.map(str::to_string),
            split: split.map(str::to_string),
            measures: measures
                .iter()
                .map(|(c, f)| Measure {
                    column: c.map(str::to_string),
                    func: *f,
                })
                .collect(),
            max_groups: 100,
        }
    }

    #[test]
    fn groups_become_a_select_by() {
        let q = grouped(
            "select from trade where px>1",
            &spec(
                Some("sym"),
                Some("side"),
                &[
                    (None, AggFn::Count),
                    (Some("px"), AggFn::Sum),
                    (Some("px"), AggFn::Avg),
                ],
            ),
        )
        .unwrap();
        assert_eq!(
            q,
            "101 sublist `dbine_g0`dbine_g1 xasc 0!select dbine_n:count i, dbine_m0:count i, \
             dbine_m1:{$[all null x;0n;\"f\"$sum x]} px, dbine_m2:avg px \
             by dbine_g0:sym, dbine_g1:side from 0!(select from trade where px>1\n )"
        );
        // One statement for the server, as the read-only path takes it.
        assert_eq!(script::split(&q).len(), 1);
    }

    #[test]
    fn a_total_has_no_by() {
        let q = grouped(
            "trade",
            &spec(
                None,
                None,
                &[(Some("sz"), AggFn::Count), (Some("px"), AggFn::Max)],
            ),
        )
        .unwrap();
        assert_eq!(
            q,
            "select dbine_n:count i, dbine_m0:{$[0h=type x;count x;sum not null x]} sz, \
             dbine_m1:max px from 0!(trade\n )"
        );
    }

    #[test]
    fn what_q_sql_cant_name_aggregates_in_dbine() {
        let m = [(None, AggFn::Count)];
        for (query, x) in [
            ("a:1\nb:2", "sym"),
            ("\\v", "sym"),
            ("select from t", "my col"),
            ("select from t", ".ns.c"),
        ] {
            let e = grouped(query, &spec(Some(x), None, &m)).unwrap_err();
            assert!(matches!(e, Error::Unsupported(_)), "{query} {x}: {e}");
        }
        // A query continued on the next line is one statement.
        assert!(grouped("select from t\n where px>1", &spec(Some("sym"), None, &m)).is_ok());
    }
}
