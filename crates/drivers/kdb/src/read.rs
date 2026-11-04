//! Reading a table in typed batches ([`Session::read_batches`]): a
//! migration or a clone from kdb+, and the source side of a sync by rows.
//!
//! Each page is a window of the table's rows by index (`sublist`, or
//! `.Q.ind` on a partitioned table, which reads only the partitions those
//! rows are in), then filtered by the read's condition (q, through
//! `parse`): memory stays bounded whatever the table's size, and a filter
//! looks at each row once. Values come as q objects and become cells
//! without loss: integers and floats as themselves, temporals in ISO form
//! with all their digits, symbols and strings as text, guids, byte lists
//! whole, other lists as JSON. Nulls are nulls; an infinity a cell can't
//! hold as a number stays as q writes it (`0W`), which a load into kdb+
//! casts back on the server.
//!
//! These reads, like every other one DBine writes here (a sync's
//! summaries, a clone's catalog), run inside `reval` on a read-only
//! connection ([`read`]). On a server without `reval` only an unfiltered
//! read goes there, as the grid's browse.
//!
//! [`Session::read_batches`]: dbine_driver::Session::read_batches

use crate::cells;
use crate::edit::{qstr, qsym};
use crate::ipc::{Vector, K};
use crate::sync::column_name;
use crate::{KdbSession, REVAL};
use dbine_driver::transfer::{BatchBuilder, BatchSinkRef, Cell, ReadSpec, TransferColumn};
use dbine_driver::{Error, Result};
use serde_json::Value;
use std::sync::atomic::Ordering;

/// Rows per page (an index window of the table, before the filter).
pub const PAGE_ROWS: u64 = 50_000;

/// The table's row count, and its columns' names and `meta` letters.
pub const BEGIN: &str = "{[t] v:get t; m:0!meta v; (count v; m`c; m`t)}";

/// Rows `a` to `a+n-1` of table `t` (keyed or not, in memory or on disk),
/// filtered by the q condition `f` (`""`: none), with only the columns `c`
/// (none: all of them).
pub const PAGE: &str = "{[t;c;f;a;n] v:get t; x:$[1b~@[.Q.qp;v;0];.Q.ind[v;a+til 0|n&(count v)-a];(a;n) sublist 0!v]; ?[x;$[count f;enlist parse f;()];0b;$[count c;c!c;()]]}";

pub(crate) fn no_reval() -> Error {
    Error::Unsupported(
        "este servidor kdb+ no tiene reval (kdb+ 3.6 o posterior): DBine no puede asegurar en el servidor que una consulta solo lea".into(),
    )
}

/// A new request of the session's own (a cancel earlier is over).
pub(crate) fn fresh(s: &KdbSession) {
    s.cancel.flag.store(false, Ordering::SeqCst);
}

/// Evaluate `text`, q DBine writes to read (names checked or written as
/// symbols, numbers it computed): inside `reval` on a read-only
/// connection, and refused there when the server doesn't have it.
pub(crate) async fn read(s: &mut KdbSession, text: &str) -> Result<K> {
    if !s.read_only {
        return s.q(text).await;
    }
    if !s.reval {
        return Err(no_reval());
    }
    s.q(&format!("{{{REVAL}}} {}", qstr(text))).await
}

/// `names` as a q symbol list (`` (),`a`b ``, `` `symbol$() `` when
/// empty), each one a plain name or written as a symbol.
pub(crate) fn symbols(names: &[String]) -> String {
    if names.is_empty() {
        return "`symbol$()".into();
    }
    if names.iter().all(|n| crate::edit::plain(n)) {
        return format!(
            "(),{}",
            names.iter().map(|n| format!("`{n}")).collect::<String>()
        );
    }
    let items: Vec<String> = names.iter().map(|n| qsym(n)).collect();
    if items.len() == 1 {
        format!("enlist {}", items[0])
    } else {
        format!("({})", items.join(";"))
    }
}

/// `names` as a typed symbol list, for a request's arguments.
pub(crate) fn symbol_list(names: &[String]) -> K {
    K::Vector(0, Vector::Symbol(names.to_vec()))
}

pub(crate) async fn read_batches(
    s: &mut KdbSession,
    spec: &ReadSpec,
    sink: BatchSinkRef,
) -> Result<u64> {
    if s.read_only && !s.reval && spec.filter.is_none() {
        // The grid's browse: what a server without reval runs read-only.
        return dbine_driver::transfer::read_via_execute(s, spec, sink).await;
    }
    fresh(s);
    let wanted = spec.columns.clone().unwrap_or_default();
    for c in &wanted {
        column_name(c)?;
    }
    let t = qsym(&spec.table.name);
    let begin = read(s, &format!("{BEGIN}[{t}]")).await?;
    let (total, columns) = begin_of(&begin, &wanted)?;
    sink.lock()
        .map_err(|_| Error::State("destino de lotes".into()))?
        .begin(&columns)?;
    let c = symbols(&wanted);
    let f = qstr(spec.filter.as_deref().unwrap_or(""));
    let mut builder = BatchBuilder::new();
    let mut at = 0u64;
    while at < total {
        let page = read(s, &format!("{PAGE}[{t};{c};{f};{at};{PAGE_ROWS}]")).await?;
        let rows = rows(page)?;
        let mut sink = sink
            .lock()
            .map_err(|_| Error::State("destino de lotes".into()))?;
        for r in rows {
            builder.push(r, &mut *sink)?;
        }
        at += PAGE_ROWS;
    }
    let mut sink = sink
        .lock()
        .map_err(|_| Error::State("destino de lotes".into()))?;
    builder.flush(&mut *sink)?;
    Ok(builder.rows)
}

/// [`BEGIN`]'s answer: the row count and the columns read (`wanted`, in
/// that order, or all of them).
fn begin_of(k: &K, wanted: &[String]) -> Result<(u64, Vec<TransferColumn>)> {
    let bad = || Error::Query("kdb+ no respondió lo que DBine esperaba".into());
    let total = match k.item(0).ok_or_else(bad)? {
        K::Long(n) => n.max(0) as u64,
        K::Int(n) => n.max(0) as u64,
        _ => return Err(bad()),
    };
    let names = k.item(1).and_then(|n| n.symbols()).ok_or_else(bad)?;
    let letters = k.item(2).ok_or_else(bad)?;
    let letter = |i: usize| match letters.item(i) {
        Some(K::Char(c)) => c as char,
        _ => ' ',
    };
    let column = |i: usize| TransferColumn {
        name: names[i].clone(),
        type_name: cells::meta_type(letter(i)),
        nullable: true,
    };
    let columns = if wanted.is_empty() {
        (0..names.len()).map(column).collect()
    } else {
        wanted
            .iter()
            .map(|w| {
                names
                    .iter()
                    .position(|n| n == w)
                    .map(column)
                    .ok_or_else(|| Error::Query(format!("la tabla no tiene la columna «{w}»")))
            })
            .collect::<Result<_>>()?
    };
    Ok((total, columns))
}

/// A page (a table) as rows of cells.
pub(crate) fn rows(page: K) -> Result<Vec<Vec<Cell>>> {
    let K::Table(_, cols) = page else {
        return Err(Error::Query(
            "kdb+ no respondió lo que DBine esperaba".into(),
        ));
    };
    let n = cols.first().map_or(0, K::count);
    let mut rows: Vec<Vec<Cell>> = (0..n).map(|_| Vec::with_capacity(cols.len())).collect();
    for col in &cols {
        match col {
            K::Vector(_, v) => {
                for (i, row) in rows.iter_mut().enumerate() {
                    row.push(v.atom(i).map_or(Cell::Null, |a| cell(&a)));
                }
            }
            K::List(items) => {
                for (row, item) in rows.iter_mut().zip(items) {
                    row.push(cell(item));
                }
            }
            _ => rows.iter_mut().for_each(|r| r.push(Cell::Null)),
        }
    }
    Ok(rows)
}

const NANOS_PER_DAY: i64 = 86_400_000_000_000;

/// A grid text of `k` (q's own form: `0W`, `1D02:00:00.000000000`…).
fn text_of(k: &K) -> Cell {
    match cells::cell(k) {
        Value::String(s) => Cell::Text(s),
        Value::Null => Cell::Null,
        v => Cell::Text(v.to_string()),
    }
}

/// A 32-bit temporal: null as null, the infinities as q writes them, the
/// rest by `f`.
fn temporal32(v: i32, f: impl Fn(i32) -> Cell) -> Cell {
    match v {
        i32::MIN => Cell::Null,
        i32::MAX => Cell::Text("0W".into()),
        n if n == -i32::MAX => Cell::Text("-0W".into()),
        n => f(n),
    }
}

/// A time of day `HH:MM:SS.fffffffff` (`ns` within one day).
fn clock(ns: i64) -> String {
    let (h, rest) = (ns / 3_600_000_000_000, ns % 3_600_000_000_000);
    let (m, rest) = (rest / 60_000_000_000, rest % 60_000_000_000);
    let (s, f) = (rest / 1_000_000_000, rest % 1_000_000_000);
    format!("{h:02}:{m:02}:{s:02}.{f:09}")
}

/// A time of day as a cell when it is one (from midnight, under a day);
/// otherwise (negative, a day or longer) q's own text, which a load into
/// kdb+ casts on the server.
fn time_cell(ns: i64, k: &K) -> Cell {
    if (0..NANOS_PER_DAY).contains(&ns) {
        Cell::Time(clock(ns))
    } else {
        text_of(k)
    }
}

/// One q value as a cell, without loss (see the module's docs).
pub fn cell(k: &K) -> Cell {
    match k {
        K::Bool(b) => Cell::Bool(*b),
        K::Guid(g) if g.iter().all(|&b| b == 0) => Cell::Null,
        K::Guid(g) => Cell::Uuid(cells::guid(g)),
        K::Byte(b) => Cell::Int(i64::from(*b)),
        K::Short(n) if *n == i16::MIN => Cell::Null,
        K::Short(n) => Cell::Int(i64::from(*n)),
        K::Int(n) if *n == i32::MIN => Cell::Null,
        K::Int(n) => Cell::Int(i64::from(*n)),
        K::Long(n) if *n == i64::MIN => Cell::Null,
        K::Long(n) => Cell::Int(*n),
        K::Real(f) if f.is_nan() => Cell::Null,
        K::Real(f) => Cell::Float(f64::from(*f)),
        K::Float(f) if f.is_nan() => Cell::Null,
        K::Float(f) => Cell::Float(*f),
        K::Char(b' ') => Cell::Null,
        K::Char(c) => Cell::Text((*c as char).to_string()),
        K::Symbol(s) if s.is_empty() => Cell::Null,
        K::Symbol(s) => Cell::Text(s.clone()),
        K::Timestamp(n) => match *n {
            i64::MIN => Cell::Null,
            n if n == i64::MAX || n == -i64::MAX => text_of(k),
            n => Cell::DateTime(cells::timestamp(n)),
        },
        K::Month(m) => temporal32(*m, |_| text_of(k)),
        K::Date(d) => temporal32(*d, |d| Cell::Date(cells::date(d))),
        K::Datetime(f) if f.is_nan() => Cell::Null,
        K::Datetime(f) if f.is_infinite() => text_of(k),
        K::Datetime(f) => {
            // Days as nanoseconds: as exact as the float itself (it has
            // about 150 ns of resolution today).
            let ns = (f * NANOS_PER_DAY as f64).round();
            if ns.abs() < 9.2e18 {
                Cell::DateTime(cells::timestamp(ns as i64))
            } else {
                text_of(k)
            }
        }
        K::Timespan(n) => match *n {
            i64::MIN => Cell::Null,
            n if n == i64::MAX || n == -i64::MAX => text_of(k),
            n => time_cell(n, k),
        },
        K::Minute(n) => temporal32(*n, |n| time_cell(i64::from(n) * 60_000_000_000, k)),
        K::Second(n) => temporal32(*n, |n| time_cell(i64::from(n) * 1_000_000_000, k)),
        K::Time(n) => temporal32(*n, |n| time_cell(i64::from(n) * 1_000_000, k)),
        K::Vector(_, Vector::Char(c)) => Cell::Text(String::from_utf8_lossy(c).into_owned()),
        K::Vector(_, Vector::Byte(b)) => Cell::Bytes(b.clone()),
        K::Nil => Cell::Null,
        K::Lambda(_, src) => Cell::Text(src.clone()),
        K::Function(f) => Cell::Text(f.clone()),
        K::Error(e) => Cell::Text(format!("'{e}")),
        nested => match cells::cell(nested) {
            Value::String(s) => Cell::Json(s),
            v => Cell::from_json(&v),
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn numbers_keep_their_value_and_nulls_are_null() {
        assert_eq!(cell(&K::Long(i64::MIN)), Cell::Null);
        // An infinity is a number: a load into kdb+ gives it back as is.
        assert_eq!(cell(&K::Long(i64::MAX)), Cell::Int(i64::MAX));
        assert_eq!(cell(&K::Int(i32::MAX)), Cell::Int(i32::MAX as i64));
        assert_eq!(cell(&K::Int(i32::MIN)), Cell::Null);
        assert_eq!(cell(&K::Short(-3)), Cell::Int(-3));
        assert_eq!(cell(&K::Byte(255)), Cell::Int(255));
        assert_eq!(cell(&K::Real(1.5)), Cell::Float(1.5));
        assert_eq!(cell(&K::Float(f64::NAN)), Cell::Null);
        assert_eq!(cell(&K::Float(f64::INFINITY)), Cell::Float(f64::INFINITY));
        assert_eq!(cell(&K::Bool(true)), Cell::Bool(true));
        assert_eq!(cell(&K::Symbol(String::new())), Cell::Null);
        assert_eq!(cell(&K::Symbol("AAPL".into())), Cell::Text("AAPL".into()));
        assert_eq!(cell(&K::Char(b'x')), Cell::Text("x".into()));
        assert_eq!(cell(&K::Guid([0; 16])), Cell::Null);
        assert_eq!(
            cell(&K::Guid([0x11; 16])),
            Cell::Uuid("11111111-1111-1111-1111-111111111111".into())
        );
    }

    #[test]
    fn temporals_keep_every_digit() {
        assert_eq!(
            cell(&K::Timestamp(1_500_000_001)),
            Cell::DateTime("2000-01-01 00:00:01.500000001".into())
        );
        assert_eq!(cell(&K::Timestamp(i64::MAX)), Cell::Text("0W".into()));
        assert_eq!(cell(&K::Timestamp(i64::MIN)), Cell::Null);
        assert_eq!(cell(&K::Date(1)), Cell::Date("2000-01-02".into()));
        assert_eq!(cell(&K::Date(i32::MAX)), Cell::Text("0W".into()));
        assert_eq!(cell(&K::Date(-i32::MAX)), Cell::Text("-0W".into()));
        assert_eq!(cell(&K::Month(13)), Cell::Text("2001-02".into()));
        assert_eq!(cell(&K::Month(i32::MIN)), Cell::Null);
        assert_eq!(
            cell(&K::Timespan(3_600_000_000_001)),
            Cell::Time("01:00:00.000000001".into())
        );
        // Past a day or negative: q's own text, cast on the server.
        assert_eq!(
            cell(&K::Timespan(90_061_000_000_001)),
            Cell::Text("1D01:01:01.000000001".into())
        );
        assert_eq!(
            cell(&K::Timespan(-1)),
            Cell::Text("-0D00:00:00.000000001".into())
        );
        assert_eq!(
            cell(&K::Minute(61)),
            Cell::Time("01:01:00.000000000".into())
        );
        assert_eq!(cell(&K::Minute(1500)), Cell::Text("25:00".into()));
        assert_eq!(
            cell(&K::Second(65)),
            Cell::Time("00:01:05.000000000".into())
        );
        assert_eq!(
            cell(&K::Time(45_296_789)),
            Cell::Time("12:34:56.789000000".into())
        );
        assert_eq!(cell(&K::Time(i32::MIN)), Cell::Null);
        assert_eq!(
            cell(&K::Datetime(1.5)),
            Cell::DateTime("2000-01-02 12:00:00".into())
        );
        assert_eq!(cell(&K::Datetime(f64::NAN)), Cell::Null);
    }

    #[test]
    fn strings_bytes_and_lists() {
        assert_eq!(
            cell(&K::Vector(0, Vector::Char(b"hi".to_vec()))),
            Cell::Text("hi".into())
        );
        let big = vec![7u8; 4096];
        assert_eq!(
            cell(&K::Vector(0, Vector::Byte(big.clone()))),
            Cell::Bytes(big)
        );
        assert_eq!(
            cell(&K::Vector(0, Vector::Long(vec![1, i64::MIN]))),
            Cell::Json("[1,null]".into())
        );
    }

    #[test]
    fn a_page_becomes_rows_and_the_columns_their_types() {
        let page = K::Table(
            vec!["id".into(), "note".into()],
            vec![
                K::Vector(0, Vector::Long(vec![1, 2])),
                K::List(vec![
                    K::Vector(0, Vector::Char(b"a".to_vec())),
                    K::Vector(0, Vector::Char(Vec::new())),
                ]),
            ],
        );
        assert_eq!(
            rows(page).unwrap(),
            vec![
                vec![Cell::Int(1), Cell::Text("a".into())],
                vec![Cell::Int(2), Cell::Text(String::new())],
            ]
        );
        assert!(rows(K::Long(1)).is_err());
        let begin = K::List(vec![
            K::Long(7),
            K::Vector(0, Vector::Symbol(vec!["id".into(), "px".into()])),
            K::Vector(0, Vector::Char(b"jf".to_vec())),
        ]);
        let (n, cols) = begin_of(&begin, &[]).unwrap();
        assert_eq!(n, 7);
        assert_eq!(
            cols.iter()
                .map(|c| (c.name.as_str(), c.type_name.as_str()))
                .collect::<Vec<_>>(),
            [("id", "long"), ("px", "float")]
        );
        let (_, cols) = begin_of(&begin, &["px".into()]).unwrap();
        assert_eq!(cols[0].name, "px");
        let e = begin_of(&begin, &["nope".into()]).unwrap_err().to_string();
        assert!(e.contains("«nope»"), "{e}");
    }

    #[test]
    fn symbol_lists_in_q() {
        assert_eq!(symbols(&[]), "`symbol$()");
        assert_eq!(symbols(&["a".into()]), "(),`a");
        assert_eq!(symbols(&["a".into(), ".ns.b".into()]), "(),`a`.ns.b");
        assert_eq!(symbols(&["my t".into()]), "enlist (`$ \"my t\")");
        assert_eq!(
            symbols(&["a".into(), "x\"y".into()]),
            "(`a;(`$ \"x\\\"y\"))"
        );
    }

    #[test]
    fn the_page_reads_a_window_then_filters() {
        assert!(PAGE.contains(".Q.ind[v;a+til 0|n&(count v)-a]"));
        assert!(PAGE.contains("(a;n) sublist 0!v"));
        assert!(PAGE.contains("enlist parse f"));
    }
}
