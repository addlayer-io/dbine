//! Sync by rows (see `dbine_driver::transfer::DeltaSpec`) between two kdb+
//! processes: bucket summaries on each side, the source's filter of the
//! changed buckets, and the apply on the target.
//!
//! - **Canonical values.** What the hashes see of a column ([`CANON`]):
//!   integers (`h`, `i`, `j`) as longs, reals as floats, symbols as
//!   strings and an enumeration (a database's `sym`, a linked column) by
//!   its values, so a key or a row hashes the same whether the other side
//!   stores it as `int` or `long`, symbol or string. `Sizes` takes the
//!   columns that are lists that way (strings and symbols, byte lists,
//!   general lists) by their lengths only, as PostgreSQL's takes its text.
//! - **Hashes.** The `md5` of each row's serialization (`-8!`, without its
//!   8-byte header): the same bytes for the same values on any kdb+.
//! - **Buckets.** `Range`: `-1` below `lo`, `n` above `hi`, else
//!   `(k-lo) div width`, computed as `(k div w)-(lo div w)-(k mod w)<(lo
//!   mod w)` so it never overflows a long. `Hash`: the md5's first 4 bytes
//!   (an unsigned 32-bit number) modulo `n`.
//! - **Summary.** Per bucket its rows and the sum of the rows' 64-bit hashes
//!   (the md5's first 8 bytes), summed on the server as two 32-bit halves
//!   that never overflow a long and joined here into the exact decimal sum.
//!   On a read-only connection it runs inside `reval`.
//! - **Filter.** Adjacent `Range` buckets merge into one `(k>=a)&k<b`;
//!   `Hash` buckets are the bucket's expression `in` their list.
//! - **Apply.** The source's rows go to a staging table ([`stage`]); one
//!   request then computes, on the target's rows of those buckets (all of
//!   them for an empty list), which are deleted (their key isn't staged),
//!   updated (staged with other values, compared with `~`) and inserted
//!   (staged keys the table doesn't have), builds the new table and
//!   assigns it in one step (`set`), keyed again when it was (with its
//!   key checked unique). kdb+ has no transactions: a failure before that
//!   assignment leaves the table as it was, and kdb+ runs one request at a
//!   time, so no one sees it half changed. Partitioned and splayed tables
//!   are refused, as the grid does.

use crate::edit::qsym;
use crate::ipc::{self, Vector, K};
use crate::read::{fresh, read, symbol_list, symbols};
use crate::stage::{self, ONE_STEP};
use crate::sync::column_name;
use crate::KdbSession;
use dbine_driver::transfer::{
    BatchSource, BucketSum, Buckets, DeltaDepth, DeltaResult, DeltaSpec, Progress, TransferColumn,
};
use dbine_driver::{Error, ObjectRef, Result};

/// A column's values as the hashes see them: `{[z;x]…}`, `z` taking a
/// list column by its lengths.
macro_rules! canon {
    () => {
        "{[z;x] y:type x; x:$[y within 20 76h; .z.s[0b;value x]; y=11h; string x; y within 5 7h; `long$x; y=8h; `float$x; x]; y:type x; $[z&(y=0h)|y>76h; count each x; x]}"
    };
}

/// The md5 of each row of a list of columns, as 16 byte vectors (byte `i`
/// of every row).
macro_rules! md5 {
    () => {
        "{flip {md5 \"c\"$8_-8!x} each flip x}"
    };
}

/// The bucket of each row: `{[m5;b;x]…}` with [`MD5`] as `m5`, the
/// buckets `b` (`` (`hash;n) `` or `` (`range;lo;hi;width;n;lo div width;lo
/// mod width) ``) and the canonical key columns `x`.
macro_rules! bucket {
    () => {
        "{[m5;b;x] $[`hash~first b; $[count first x; (sum 16777216 65536 256 1*`long$(m5 x) 0 1 2 3) mod b 1; `long$()]; {[k;lo;hi;w;n;ql;rl] ?[k<lo;-1;?[k>hi;n;((k div w)-ql)-rl>k mod w]]} . (enlist first x),1_b]}"
    };
}

pub const CANON: &str = canon!();
pub const MD5: &str = md5!();
pub const BUCKET: &str = bucket!();

/// `[t;k;c;z;b]`: table `t`'s buckets by key `k`, rows compared on
/// columns `c` (`z`: by length), as a table of bucket, rows and the two
/// halves of the hash sum.
pub const SUMMARY: &str = concat!(
    "{[t;k;c;z;b] cn:",
    canon!(),
    "; m5:",
    md5!(),
    "; bk:",
    bucket!(),
    "; d:?[t;();0b;u!u:distinct k,c]; if[0=count d; :([] b:`long$(); n:`long$(); h:`long$(); l:`long$())]; g:bk[m5;b;cn[0b] each d k]; r:m5 cn'[z;d c]; w:16777216 65536 256 1; 0!select n:count i, h:sum h, l:sum l by b from ([] b:g; h:sum w*`long$r 0 1 2 3; l:sum w*`long$r 4 5 6 7)}"
);

/// A key of a keyed table that repeats: refused before the assignment.
macro_rules! dup_key {
    () => {
        "'\"la tabla tiene clave y las filas repiten un valor de esa clave\""
    };
}

/// `[n;s;k;c;b;B]`: table `n` made equal, in buckets `B` (empty: all), to
/// the staged rows of `s` (columns `c`, key `k`); answers the rows
/// inserted, updated and deleted.
pub const APPLY: &str = concat!(
    "{[n;s;k;c;b;B] cn:",
    canon!(),
    "; m5:",
    md5!(),
    "; bk:",
    bucket!(),
    "; ky:keys n; t:0!get n; st:get s; nt:count t; ns:count st; cc:c except k;",
    " sc:$[count B; where bk[m5;b;cn[0b] each t k] in B; til nt];",
    " tk:?[t;();0b;k!k]; sk:?[st;();0b;k!k]; m:sk?tk; d:sc where ns=m sc; mw:where m<ns;",
    " u:$[count[cc]&count mw; mw where (|/){not x~'y}'[t[cc]@\\:mw; st[cc]@\\:m mw]; `long$()];",
    " a:where nt=tk?sk;",
    " if[count u; t:@[t;cc;:;{[x;y;i;j] @[x;i;:;y j]}[;;u;m u]'[t cc;st cc]]];",
    " if[count d; t:t where not (til nt) in d];",
    " if[count a; t:t upsert st a];",
    " if[count ky; t:ky xkey t; if[count[t]>count distinct key t; ",
    dup_key!(),
    "]];",
    " if[count[u]+count[d]+count a; n set t];",
    " (count a; count u; count d)}"
);

/// `[t;c]`: the `meta` letter of column `c`, and when it's an integer a
/// one-row table of its smallest (`a`) and largest (`b`) value and the
/// rows (`n`).
pub const KEY_RANGE: &str = "{[t;c] m:0!meta t; y:m[`t] m[`c]?c; (y; $[y in \"hij\"; ?[t;();0b;`a`b`n!((min;c);(max;c);(count;c))]; ()])}";

/// A long in q text (the null long can't be written as digits).
fn qlong(n: i64) -> String {
    if n == i64::MIN {
        "0N".into()
    } else {
        n.to_string()
    }
}

/// The buckets as q (the argument `b` of [`SUMMARY`] and [`APPLY`]).
fn buckets_q(spec: &DeltaSpec) -> Result<String> {
    match &spec.buckets {
        Buckets::Range {
            lo, hi, width, n, ..
        } => {
            check_range(*lo, *hi, *width, *n)?;
            Ok(format!(
                "(`range;{};{};{};{};{};{})",
                qlong(*lo),
                qlong(*hi),
                width,
                n,
                qlong(lo.div_euclid(*width)),
                lo.rem_euclid(*width)
            ))
        }
        Buckets::Hash { n } => {
            check_hash(spec, *n)?;
            Ok(format!("(`hash;{n})"))
        }
    }
}

/// The buckets as a q object (for [`APPLY`]'s request).
fn buckets_k(spec: &DeltaSpec) -> Result<K> {
    Ok(match &spec.buckets {
        Buckets::Range {
            lo, hi, width, n, ..
        } => {
            check_range(*lo, *hi, *width, *n)?;
            K::List(vec![
                K::Symbol("range".into()),
                K::Long(*lo),
                K::Long(*hi),
                K::Long(*width),
                K::Long(*n as i64),
                K::Long(lo.div_euclid(*width)),
                K::Long(lo.rem_euclid(*width)),
            ])
        }
        Buckets::Hash { n } => {
            check_hash(spec, *n)?;
            K::List(vec![K::Symbol("hash".into()), K::Long(*n as i64)])
        }
    })
}

fn check_range(lo: i64, hi: i64, width: i64, n: u64) -> Result<()> {
    if width <= 0 || n == 0 || hi < lo || n > i64::MAX as u64 {
        return Err(Error::Query(format!(
            "rangos inválidos: {lo}..{hi}, ancho {width}, {n} grupos"
        )));
    }
    Ok(())
}

fn check_hash(spec: &DeltaSpec, n: u64) -> Result<()> {
    if n == 0 || n > i64::MAX as u64 || spec.key.is_empty() {
        return Err(Error::Query(
            "grupos por hash sin clave o sin cantidad".into(),
        ));
    }
    Ok(())
}

/// The key and the compared columns, checked as q names.
fn names(spec: &DeltaSpec) -> Result<()> {
    if spec.key.is_empty() {
        return Err(Error::State(
            "para sincronizar por filas hace falta una clave".into(),
        ));
    }
    for c in spec.key.iter().chain(&spec.columns) {
        column_name(c)?;
    }
    if let Buckets::Range { column, .. } = &spec.buckets {
        column_name(column)?;
    }
    Ok(())
}

/// The summary's request (see [`SUMMARY`]).
pub(crate) fn summary_q(spec: &DeltaSpec) -> Result<String> {
    names(spec)?;
    let compared: &[String] = if spec.depth == DeltaDepth::Keys {
        &spec.key
    } else {
        &spec.columns
    };
    if compared.is_empty() {
        return Err(Error::Query("no hay columnas para comparar".into()));
    }
    let sizes: String = compared
        .iter()
        .map(|c| {
            if spec.depth == DeltaDepth::Sizes && !spec.key.contains(c) {
                '1'
            } else {
                '0'
            }
        })
        .collect();
    Ok(format!(
        "{SUMMARY}[{};{};{};(),{sizes}b;{}]",
        qsym(&spec.table.name),
        symbols(&spec.key),
        symbols(compared),
        buckets_q(spec)?
    ))
}

/// The condition that selects the rows of `buckets` (see the module's
/// docs).
pub fn filter(spec: &DeltaSpec, buckets: &[i64]) -> Result<String> {
    names(spec)?;
    if buckets.is_empty() {
        return Ok("0b".into());
    }
    let mut b = buckets.to_vec();
    b.sort_unstable();
    b.dedup();
    match &spec.buckets {
        Buckets::Hash { n } => {
            check_hash(spec, *n)?;
            let keys = match spec.key.as_slice() {
                [one] => format!("enlist {one}"),
                many => format!("({})", many.join(";")),
            };
            let list = b.iter().map(|k| qlong(*k)).collect::<Vec<_>>().join(" ");
            Ok(format!(
                "({BUCKET}[{MD5};(`hash;{n});{CANON}[0b] each {keys}]) in (),{list}"
            ))
        }
        Buckets::Range {
            column,
            lo,
            hi,
            width,
            n,
        } => {
            check_range(*lo, *hi, *width, *n)?;
            let c = column.as_str();
            let (lo, hi, w, n) = (*lo as i128, *hi as i128, *width as i128, *n as i128);
            // A bucket's first value and the first one past it (`None`:
            // unbounded). Buckets `-1` and `n` hold the rows outside
            // lo..=hi (the null key, the smallest long, below lo).
            let start = |k: i128| -> Option<i128> {
                match k {
                    k if k < 0 => None,
                    k if k >= n => Some((lo + n * w).min(hi + 1)),
                    k => Some(lo + k * w),
                }
            };
            let end = |k: i128| -> Option<i128> {
                match k {
                    k if k < 0 => Some(lo),
                    k if k >= n => None,
                    k => Some((lo + (k + 1) * w).min(hi + 1)),
                }
            };
            let mut runs: Vec<(i128, i128)> = Vec::new();
            for k in b.iter().map(|&k| k as i128) {
                match runs.last_mut() {
                    Some(r) if r.1 + 1 == k => r.1 = k,
                    _ => runs.push((k, k)),
                }
            }
            let (min, max) = (i64::MIN as i128, i64::MAX as i128);
            let parts: Vec<String> = runs
                .iter()
                .map(|&(a, z)| {
                    let mut conds: Vec<String> = Vec::new();
                    if let Some(s) = start(a).filter(|s| *s > min) {
                        conds.push(format!("{c}>={s}"));
                    }
                    if let Some(e) = end(z).filter(|e| *e <= max) {
                        conds.push(format!("{c}<{e}"));
                    }
                    match conds.as_slice() {
                        [] => "1b".to_string(),
                        [one] => format!("({one})"),
                        [a, b] => format!("(({a})&{b})"),
                        _ => unreachable!("two bounds at most"),
                    }
                })
                .collect();
            if parts.iter().any(|p| p == "1b") {
                return Ok("1b".into());
            }
            Ok(parts.join("|"))
        }
    }
}

/// `table`'s smallest and largest value of `column` and its rows, when
/// the column is an integer (`h`, `i` or `j`); `None` when it's empty.
pub(crate) async fn key_range(
    s: &mut KdbSession,
    table: &ObjectRef,
    column: &str,
) -> Result<Option<(i64, i64, u64)>> {
    fresh(s);
    let c = column_name(column)?;
    let k = read(s, &format!("{KEY_RANGE}[{};`{c}]", qsym(&table.name))).await?;
    range_of(&k, column)
}

fn range_of(k: &K, column: &str) -> Result<Option<(i64, i64, u64)>> {
    let letter = match k.item(0) {
        Some(K::Char(c)) => c as char,
        _ => ' ',
    };
    if !"hij".contains(letter) {
        return Err(Error::Unsupported(format!(
            "la columna «{column}» no es entera ({})",
            crate::cells::meta_type(letter)
        )));
    }
    let Some(K::Table(names, cols)) = k.item(1) else {
        return Err(Error::Query(
            "kdb+ no respondió lo que DBine esperaba".into(),
        ));
    };
    let at = |n: &str| -> Option<i64> {
        let i = names.iter().position(|c| c == n)?;
        match cols[i].item(0)? {
            K::Long(v) => Some(v),
            K::Int(v) => Some(i64::from(v)),
            K::Short(v) => Some(i64::from(v)),
            _ => None,
        }
    };
    let rows = at("n").unwrap_or(0).max(0) as u64;
    Ok(match (at("a"), at("b")) {
        (Some(lo), Some(hi)) if rows > 0 && lo <= hi => Some((lo, hi, rows)),
        _ => None,
    })
}

pub(crate) async fn summary(s: &mut KdbSession, spec: &DeltaSpec) -> Result<Vec<BucketSum>> {
    fresh(s);
    let text = summary_q(spec)?;
    sums(read(s, &text).await?)
}

/// [`SUMMARY`]'s table as bucket sums (the halves joined: `h·2³² + l`).
fn sums(k: K) -> Result<Vec<BucketSum>> {
    let K::Table(names, cols) = k else {
        return Err(Error::Query(
            "kdb+ no respondió lo que DBine esperaba".into(),
        ));
    };
    let col = |n: &str| -> Result<Vec<i64>> {
        match names.iter().position(|c| c == n).map(|i| &cols[i]) {
            Some(K::Vector(_, Vector::Long(v))) => Ok(v.clone()),
            _ => Err(Error::Query("grupo inválido".into())),
        }
    };
    let (b, n, h, l) = (col("b")?, col("n")?, col("h")?, col("l")?);
    let mut out: Vec<BucketSum> = (0..b.len())
        .map(|i| BucketSum {
            bucket: b[i],
            rows: n[i].max(0) as u64,
            sum: ((h[i].max(0) as u128) * (1u128 << 32) + l[i].max(0) as u128).to_string(),
        })
        .collect();
    out.sort_by_key(|x| x.bucket);
    Ok(out)
}

pub(crate) async fn apply(
    s: &mut KdbSession,
    spec: &DeltaSpec,
    buckets: &[i64],
    columns: &[TransferColumn],
    source: &mut dyn BatchSource,
    progress: Progress<'_>,
) -> Result<DeltaResult> {
    stage::writes(s)?;
    crate::writable(&spec.table)?;
    names(spec)?;
    // The source's cells come in the read's order.
    let cols: Vec<String> = if columns.is_empty() {
        spec.columns.clone()
    } else {
        columns.iter().map(|c| c.name.clone()).collect()
    };
    for c in &cols {
        column_name(c)?;
    }
    for k in &spec.key {
        if !cols.contains(k) {
            return Err(Error::Query(format!(
                "la lectura no trae la columna de clave «{k}»"
            )));
        }
    }
    fresh(s);
    let table = spec.table.name.clone();
    let st = stage::create(s, &table, &cols).await?;
    let r = async {
        stage::load(s, &st, &table, &cols, source, progress).await?;
        let msg = ipc::apply(
            APPLY,
            vec![
                K::Symbol(table.clone()),
                K::Symbol(st.name()),
                symbol_list(&spec.key),
                symbol_list(&cols),
                buckets_k(spec)?,
                K::Vector(0, Vector::Long(buckets.to_vec())),
            ],
        );
        s.request(&msg).await
    }
    .await;
    stage::drop(s, &st).await;
    let (inserted, updated, deleted) = counts(&r?)?;
    Ok(DeltaResult {
        inserted,
        updated,
        deleted,
        notes: vec![ONE_STEP.into()],
    })
}

/// `(inserted; updated; deleted)` as a staged change answers them.
pub(crate) fn counts(k: &K) -> Result<(u64, u64, u64)> {
    match k {
        K::Vector(_, Vector::Long(v)) if v.len() == 3 => {
            Ok((v[0].max(0) as u64, v[1].max(0) as u64, v[2].max(0) as u64))
        }
        _ => Err(Error::Query(
            "kdb+ no respondió lo que DBine esperaba".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(buckets: Buckets, key: &[&str]) -> DeltaSpec {
        DeltaSpec {
            table: ObjectRef {
                kind: "table".into(),
                schema: None,
                name: "trade".into(),
            },
            key: key.iter().map(|s| s.to_string()).collect(),
            columns: vec!["id".into(), "sym".into(), "note".into()],
            buckets,
            depth: DeltaDepth::Full,
            max_cores: 0,
        }
    }

    fn range() -> Buckets {
        Buckets::Range {
            column: "id".into(),
            lo: 1,
            hi: 1000,
            width: 100,
            n: 10,
        }
    }

    #[test]
    fn range_filters_merge_adjacent_buckets() {
        let s = spec(range(), &["id"]);
        // 2,3,4 merge; -1 is unbounded below; 9 stops at hi; 10 is unbounded above.
        assert_eq!(filter(&s, &[3, 2, 4]).unwrap(), "((id>=201)&id<501)");
        assert_eq!(filter(&s, &[-1, 0]).unwrap(), "(id<101)");
        assert_eq!(filter(&s, &[9, 10]).unwrap(), "(id>=901)");
        assert_eq!(filter(&s, &[9]).unwrap(), "((id>=901)&id<1001)");
        assert_eq!(
            filter(&s, &[-1, 0, 5, 10]).unwrap(),
            "(id<101)|((id>=501)&id<601)|(id>=1001)"
        );
        assert_eq!(filter(&s, &(-1..=10).collect::<Vec<_>>()).unwrap(), "1b");
        assert_eq!(filter(&s, &[]).unwrap(), "0b");
        let bad = spec(
            Buckets::Range {
                column: "id".into(),
                lo: 5,
                hi: 1,
                width: 1,
                n: 1,
            },
            &["id"],
        );
        assert!(filter(&bad, &[0]).is_err());
        assert!(summary_q(&bad).is_err());
    }

    #[test]
    fn hash_filters_compute_the_summary_s_bucket() {
        let s = spec(Buckets::Hash { n: 17 }, &["id", "sym"]);
        let f = filter(&s, &[5, 1, 5]).unwrap();
        assert!(
            f.starts_with(&format!(
                "({BUCKET}[{MD5};(`hash;17);{CANON}[0b] each (id;sym)])"
            )),
            "{f}"
        );
        assert!(f.ends_with(") in (),1 5"), "{f}");
        let one = filter(&spec(Buckets::Hash { n: 7 }, &["id"]), &[3]).unwrap();
        assert!(one.contains("[0b] each enlist id]"), "{one}");
    }

    #[test]
    fn the_summary_takes_names_as_symbols_and_the_depth_s_columns() {
        let mut s = spec(Buckets::Hash { n: 17 }, &["id"]);
        let q = summary_q(&s).unwrap();
        assert!(q.starts_with(SUMMARY), "{q}");
        assert!(
            q.ends_with("[`trade;(),`id;(),`id`sym`note;(),000b;(`hash;17)]"),
            "{q}"
        );
        s.depth = DeltaDepth::Sizes;
        assert!(summary_q(&s).unwrap().ends_with(";(),011b;(`hash;17)]"));
        s.depth = DeltaDepth::Keys;
        assert!(summary_q(&s)
            .unwrap()
            .ends_with("[`trade;(),`id;(),`id;(),0b;(`hash;17)]"));
        // Range: lo div width and lo mod width go along, floored.
        let mut r = spec(
            Buckets::Range {
                column: "id".into(),
                lo: -7,
                hi: 100,
                width: 2,
                n: 54,
            },
            &["id"],
        );
        r.table.name = "my t".into();
        let q = summary_q(&r).unwrap();
        assert!(
            q.ends_with("[(`$ \"my t\");(),`id;(),`id`sym`note;(),000b;(`range;-7;100;2;54;-4;1)]"),
            "{q}"
        );
    }

    #[test]
    fn names_that_arent_q_names_never_reach_the_q() {
        let evil = "id;system\"rm -rf /\";`id";
        let mut s = spec(Buckets::Hash { n: 17 }, &[evil]);
        assert!(summary_q(&s).is_err());
        assert!(filter(&s, &[1]).is_err());
        s.key = vec!["id".into()];
        s.columns.push("a b".into());
        assert!(summary_q(&s).is_err());
        let mut r = spec(range(), &["id"]);
        r.buckets = Buckets::Range {
            column: "x:1".into(),
            lo: 1,
            hi: 2,
            width: 1,
            n: 2,
        };
        assert!(filter(&r, &[0]).is_err());
        s.key.clear();
        assert!(summary_q(&s).is_err());
    }

    #[test]
    fn the_apply_builds_the_table_then_assigns_it_once() {
        // Nothing is assigned before the end, and only when something changed.
        assert_eq!(APPLY.matches(" set ").count(), 1);
        assert!(APPLY.contains(" if[count[u]+count[d]+count a; n set t];"));
        assert!(APPLY.contains("t:ky xkey t"));
        assert!(APPLY.contains("@\\:mw"));
        assert!(SUMMARY.contains("8_-8!x"));
    }

    #[test]
    fn sums_join_their_halves_exactly() {
        let t = K::Table(
            vec!["b".into(), "n".into(), "h".into(), "l".into()],
            vec![
                K::Vector(0, Vector::Long(vec![3, -1])),
                K::Vector(0, Vector::Long(vec![2, 1])),
                K::Vector(0, Vector::Long(vec![8_589_934_590, 1])),
                K::Vector(0, Vector::Long(vec![5, 0])),
            ],
        );
        let s = sums(t).unwrap();
        assert_eq!(s[0].bucket, -1);
        assert_eq!(s[0].sum, (1u128 << 32).to_string());
        // Two 64-bit hashes of all ones: past a u64.
        assert_eq!(
            s[1].sum,
            (8_589_934_590u128 * (1u128 << 32) + 5).to_string()
        );
        assert_eq!(s[1].rows, 2);
        assert!(sums(K::Long(1)).is_err());
    }

    #[test]
    fn key_ranges_only_for_integers() {
        let table = |a: i64, b: i64, n: i64| {
            K::Table(
                vec!["a".into(), "b".into(), "n".into()],
                vec![
                    K::Vector(0, Vector::Long(vec![a])),
                    K::Vector(0, Vector::Long(vec![b])),
                    K::Vector(0, Vector::Long(vec![n])),
                ],
            )
        };
        let k = K::List(vec![K::Char(b'j'), table(1, 9, 5)]);
        assert_eq!(range_of(&k, "id").unwrap(), Some((1, 9, 5)));
        // Empty: min is 0W, max -0W.
        let k = K::List(vec![K::Char(b'j'), table(i64::MAX, -i64::MAX, 0)]);
        assert_eq!(range_of(&k, "id").unwrap(), None);
        let k = K::List(vec![K::Char(b's'), K::List(vec![])]);
        let e = range_of(&k, "sym").unwrap_err().to_string();
        assert!(e.contains("no es entera (symbol)"), "{e}");
        assert!(range_of(&K::List(vec![K::Char(b' ')]), "x").is_err());
    }

    #[test]
    fn counts_of_a_change() {
        assert_eq!(
            counts(&K::Vector(0, Vector::Long(vec![1, 2, 3]))).unwrap(),
            (1, 2, 3)
        );
        assert!(counts(&K::Long(1)).is_err());
    }
}
