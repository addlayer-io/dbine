//! q values as DBine's cells and result sets.
//!
//! Nulls (`0N`, `0n`, the empty symbol, the null guid…) become JSON null;
//! infinities (`0W`, `-0W`, `0w`) stay as q writes them. Temporal values go
//! in ISO form (`2024-01-31 13:45:00.123456789`); a timespan as q writes
//! it (`0D12:00:00.000000000`).

use crate::ipc::{Vector, K};
use chrono::{NaiveDate, NaiveDateTime, TimeDelta};
use dbine_driver::{json_bytes, json_f64, json_i64, QueryOutcome, ResultColumn};
use serde_json::Value;

fn epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2000, 1, 1)
        .expect("2000-01-01")
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
}

/// q's type names, by the type number of a vector (or of an atom, negated).
pub fn type_name(t: i8) -> &'static str {
    match t.unsigned_abs() {
        1 => "boolean",
        2 => "guid",
        4 => "byte",
        5 => "short",
        6 => "int",
        7 => "long",
        8 => "real",
        9 => "float",
        10 => "char",
        11 => "symbol",
        12 => "timestamp",
        13 => "month",
        14 => "date",
        15 => "datetime",
        16 => "timespan",
        17 => "minute",
        18 => "second",
        19 => "time",
        _ => "",
    }
}

/// A `meta` type letter as a name: `j` long, `s` symbol, `C` string (a
/// list of chars), other uppercase letters a list of that type, a blank a
/// general list.
pub fn meta_type(c: char) -> String {
    const LETTERS: &str = "bg xhijefcspmdznuvt";
    if c == ' ' {
        return "list".into();
    }
    if c == 'C' {
        return "string".into();
    }
    let lower = c.to_ascii_lowercase();
    let name = LETTERS
        .find(lower)
        .map(|i| type_name(i as i8 + 1))
        .filter(|n| !n.is_empty())
        .unwrap_or("list");
    if c.is_ascii_uppercase() {
        format!("{name} list")
    } else {
        name.to_string()
    }
}

/// The `meta` letter of a type name (the inverse of [`meta_type`]): what a
/// cast (`"J"$…`) takes.
pub fn meta_letter(name: &str) -> Option<char> {
    let name = name.trim().to_ascii_lowercase();
    if name == "string" {
        return Some('C');
    }
    let (base, list) = match name.strip_suffix(" list") {
        Some(b) => (b, true),
        None => (name.as_str(), false),
    };
    const LETTERS: &str = "bg xhijefcspmdznuvt";
    let i = (1..=19).find(|&t| type_name(t) == base)?;
    let c = LETTERS.chars().nth(i as usize - 1)?;
    Some(if list { c.to_ascii_uppercase() } else { c })
}

fn int_cell(v: i64, null: i64, inf: i64) -> Value {
    if v == null {
        Value::Null
    } else if v == inf {
        Value::String("0W".into())
    } else if v == -inf {
        Value::String("-0W".into())
    } else {
        json_i64(v)
    }
}

fn float_cell(v: f64) -> Value {
    if v.is_nan() {
        Value::Null
    } else if v.is_infinite() {
        Value::String(if v > 0.0 { "0w" } else { "-0w" }.into())
    } else {
        json_f64(v)
    }
}

/// A 32-bit temporal: null and infinities as q, the rest by `f`.
fn temporal32(v: i32, f: impl Fn(i32) -> String) -> Value {
    match v {
        i32::MIN => Value::Null,
        i32::MAX => Value::String("0W".into()),
        n if n == -i32::MAX => Value::String("-0W".into()),
        n => Value::String(f(n)),
    }
}

fn temporal64(v: i64, f: impl Fn(i64) -> String) -> Value {
    match v {
        i64::MIN => Value::Null,
        i64::MAX => Value::String("0W".into()),
        n if n == -i64::MAX => Value::String("-0W".into()),
        n => Value::String(f(n)),
    }
}

/// A date, or the day count as is when it falls outside the dates chrono
/// writes (about ±262,000 years).
pub fn date(days: i32) -> String {
    TimeDelta::try_days(days as i64)
        .and_then(|d| epoch().checked_add_signed(d))
        .map_or_else(
            || days.to_string(),
            |t| t.date().format("%Y-%m-%d").to_string(),
        )
}

/// Every i64 of nanoseconds fits (±292 years from 2000).
pub fn timestamp(ns: i64) -> String {
    let Some(t) = epoch().checked_add_signed(TimeDelta::nanoseconds(ns)) else {
        return ns.to_string();
    };
    if ns.rem_euclid(1_000_000_000) == 0 {
        t.format("%Y-%m-%d %H:%M:%S").to_string()
    } else {
        t.format("%Y-%m-%d %H:%M:%S%.9f").to_string()
    }
}

fn month(m: i32) -> String {
    let y = 2000 + m.div_euclid(12);
    format!("{y:04}-{:02}", m.rem_euclid(12) + 1)
}

/// A datetime, or the day count as is when it falls outside the dates
/// chrono writes.
fn datetime(days: f64) -> String {
    let ms = days * 86_400_000.0;
    // `as` saturates; anything near i64's range is far past chrono's dates.
    let t = (ms.abs() < 9.0e18)
        .then(|| TimeDelta::try_milliseconds(ms.round() as i64))
        .flatten()
        .and_then(|d| epoch().checked_add_signed(d));
    match t {
        Some(t) => t.format("%Y-%m-%d %H:%M:%S%.3f").to_string(),
        None => days.to_string(),
    }
}

/// q's form: `[-]{d}D{hh}:{mm}:{ss}.{nnnnnnnnn}`.
fn timespan(ns: i64) -> String {
    let sign = if ns < 0 { "-" } else { "" };
    let n = ns.unsigned_abs();
    let (d, rest) = (n / 86_400_000_000_000, n % 86_400_000_000_000);
    let (h, rest) = (rest / 3_600_000_000_000, rest % 3_600_000_000_000);
    let (m, rest) = (rest / 60_000_000_000, rest % 60_000_000_000);
    let (s, f) = (rest / 1_000_000_000, rest % 1_000_000_000);
    format!("{sign}{d}D{h:02}:{m:02}:{s:02}.{f:09}")
}

fn clock(sign: bool, h: u32, m: u32, s: Option<u32>, ms: Option<u32>) -> String {
    let mut out = format!("{}{h:02}:{m:02}", if sign { "-" } else { "" });
    if let Some(s) = s {
        out.push_str(&format!(":{s:02}"));
    }
    if let Some(ms) = ms {
        out.push_str(&format!(".{ms:03}"));
    }
    out
}

fn minute(v: i32) -> String {
    let n = v.unsigned_abs();
    clock(v < 0, n / 60, n % 60, None, None)
}

fn second(v: i32) -> String {
    let n = v.unsigned_abs();
    clock(v < 0, n / 3600, n / 60 % 60, Some(n % 60), None)
}

fn time(v: i32) -> String {
    let n = v.unsigned_abs();
    clock(
        v < 0,
        n / 3_600_000,
        n / 60_000 % 60,
        Some(n / 1000 % 60),
        Some(n % 1000),
    )
}

pub fn guid(g: &[u8; 16]) -> String {
    let h: String = g.iter().map(|b| format!("{b:02x}")).collect();
    format!(
        "{}-{}-{}-{}-{}",
        &h[0..8],
        &h[8..12],
        &h[12..16],
        &h[16..20],
        &h[20..32]
    )
}

/// One value as a cell.
pub fn cell(k: &K) -> Value {
    match k {
        K::Bool(b) => Value::Bool(*b),
        K::Guid(g) if g.iter().all(|&b| b == 0) => Value::Null,
        K::Guid(g) => Value::String(guid(g)),
        K::Byte(b) => json_i64(*b as i64),
        K::Short(n) => int_cell(*n as i64, i16::MIN as i64, i16::MAX as i64),
        K::Int(n) => int_cell(*n as i64, i32::MIN as i64, i32::MAX as i64),
        K::Long(n) => int_cell(*n, i64::MIN, i64::MAX),
        K::Real(f) => float_cell(*f as f64),
        K::Float(f) => float_cell(*f),
        K::Char(b' ') => Value::Null,
        K::Char(c) => Value::String((*c as char).to_string()),
        K::Symbol(s) if s.is_empty() => Value::Null,
        K::Symbol(s) => Value::String(s.clone()),
        K::Timestamp(n) => temporal64(*n, timestamp),
        K::Month(n) => temporal32(*n, month),
        K::Date(n) => temporal32(*n, date),
        K::Datetime(f) if f.is_nan() => Value::Null,
        K::Datetime(f) if f.is_infinite() => float_cell(*f),
        K::Datetime(f) => Value::String(datetime(*f)),
        K::Timespan(n) => temporal64(*n, timespan),
        K::Minute(n) => temporal32(*n, minute),
        K::Second(n) => temporal32(*n, second),
        K::Time(n) => temporal32(*n, time),
        K::Vector(_, Vector::Char(c)) => Value::String(String::from_utf8_lossy(c).into_owned()),
        K::Vector(_, Vector::Byte(b)) => json_bytes(b),
        K::Nil => Value::Null,
        K::Lambda(_, src) => Value::String(src.clone()),
        K::Function(f) => Value::String(f.clone()),
        K::Error(e) => Value::String(format!("'{e}")),
        nested => Value::String(nested_json(nested).to_string()),
    }
}

/// A nested value (a list in a cell, a dictionary…) as JSON.
fn nested_json(k: &K) -> Value {
    nested_at(k, 0)
}

/// [`nested_json`] `depth` levels down: past [`crate::ipc::MAX_DEPTH`] (more
/// than a decoded message can hold) the rest shows as `…`.
fn nested_at(k: &K, depth: usize) -> Value {
    if depth >= crate::ipc::MAX_DEPTH {
        return Value::String("…".into());
    }
    let nested_json = |x: &K| nested_at(x, depth + 1);
    match k {
        K::Vector(_, Vector::Char(_)) => cell(k),
        K::Vector(_, v) => Value::Array(
            (0..v.len())
                .filter_map(|i| v.atom(i))
                .map(|a| cell(&a))
                .collect(),
        ),
        K::List(l) => Value::Array(l.iter().map(nested_json).collect()),
        K::Dict(keys, values) => match (keys.symbols(), values.as_ref()) {
            (Some(names), v) if names.len() == v.count() => Value::Object(
                names
                    .into_iter()
                    .enumerate()
                    .map(|(i, n)| (n, v.item(i).map(|x| nested_json(&x)).unwrap_or(Value::Null)))
                    .collect(),
            ),
            _ => Value::Array(vec![nested_json(keys), nested_json(values)]),
        },
        K::Table(names, cols) => {
            let n = cols.first().map_or(0, K::count);
            Value::Array(
                (0..n)
                    .map(|i| {
                        Value::Object(
                            names
                                .iter()
                                .zip(cols)
                                .map(|(c, col)| {
                                    (
                                        c.clone(),
                                        col.item(i).map(|x| nested_json(&x)).unwrap_or(Value::Null),
                                    )
                                })
                                .collect(),
                        )
                    })
                    .collect(),
            )
        }
        atom => cell(atom),
    }
}

/// The type name of a column (a vector, or a list of strings).
fn column_type(col: &K) -> String {
    match col {
        K::Vector(_, v) => type_name(v.type_code()).to_string(),
        K::List(l)
            if !l.is_empty() && l.iter().all(|x| matches!(x, K::Vector(_, Vector::Char(_)))) =>
        {
            "string".into()
        }
        _ => String::new(),
    }
}

fn columns(names: &[String], cols: &[K]) -> Vec<ResultColumn> {
    names
        .iter()
        .zip(cols)
        .map(|(n, c)| ResultColumn {
            name: n.clone(),
            type_name: column_type(c),
        })
        .collect()
}

fn rows(cols: &[K], max_rows: usize, out: &mut QueryOutcome) {
    let n = cols.first().map_or(0, K::count);
    for i in 0..n {
        let row = cols
            .iter()
            .map(|c| c.item(i).map_or(Value::Null, |x| cell(&x)))
            .collect();
        out.push_row(row, max_rows);
        // Past max_rows nothing is kept: count the rest without building
        // rows nobody will see.
        if let Some(r) = out.results.last_mut().filter(|r| r.truncated) {
            r.total_rows += (n - i - 1) as u64;
            break;
        }
    }
}

/// What a statement answered, as results in `out`.
pub fn emit(k: K, max_rows: usize, out: &mut QueryOutcome) {
    match k {
        K::Nil => out.push_affected(0),
        K::Table(names, cols) => {
            out.begin_result(columns(&names, &cols));
            rows(&cols, max_rows, out);
        }
        // A keyed table: its key columns, then the others.
        K::Dict(keys, values) if matches!((&*keys, &*values), (K::Table(..), K::Table(..))) => {
            let (K::Table(kn, kc), K::Table(vn, vc)) = (*keys, *values) else {
                unreachable!()
            };
            let names: Vec<String> = kn.into_iter().chain(vn).collect();
            let cols: Vec<K> = kc.into_iter().chain(vc).collect();
            out.begin_result(columns(&names, &cols));
            rows(&cols, max_rows, out);
        }
        K::Dict(keys, values) => {
            let (kc, vc) = (*keys, *values);
            if kc.count() == vc.count() && matches!(kc, K::Vector(..) | K::List(_)) {
                out.begin_result(vec![
                    ResultColumn {
                        name: "key".into(),
                        type_name: column_type(&kc),
                    },
                    ResultColumn {
                        name: "value".into(),
                        type_name: column_type(&vc),
                    },
                ]);
                rows(&[kc, vc], max_rows, out);
            } else {
                one(
                    Value::String(nested_json(&K::Dict(Box::new(kc), Box::new(vc))).to_string()),
                    "",
                    max_rows,
                    out,
                );
            }
        }
        K::Vector(_, Vector::Char(c)) => one(
            Value::String(String::from_utf8_lossy(&c).into_owned()),
            "string",
            max_rows,
            out,
        ),
        v @ (K::Vector(..) | K::List(_)) => {
            let t = column_type(&v);
            out.begin_result(vec![ResultColumn {
                name: "value".into(),
                type_name: t,
            }]);
            rows(&[v], max_rows, out);
        }
        atom => {
            let t = atom_type(&atom);
            one(cell(&atom), t, max_rows, out)
        }
    }
}

fn atom_type(k: &K) -> &'static str {
    match k {
        K::Bool(_) => "boolean",
        K::Guid(_) => "guid",
        K::Byte(_) => "byte",
        K::Short(_) => "short",
        K::Int(_) => "int",
        K::Long(_) => "long",
        K::Real(_) => "real",
        K::Float(_) => "float",
        K::Char(_) => "char",
        K::Symbol(_) => "symbol",
        K::Timestamp(_) => "timestamp",
        K::Month(_) => "month",
        K::Date(_) => "date",
        K::Datetime(_) => "datetime",
        K::Timespan(_) => "timespan",
        K::Minute(_) => "minute",
        K::Second(_) => "second",
        K::Time(_) => "time",
        _ => "",
    }
}

fn one(v: Value, type_name: &str, max_rows: usize, out: &mut QueryOutcome) {
    out.begin_result(vec![ResultColumn {
        name: "result".into(),
        type_name: type_name.into(),
    }]);
    out.push_row(vec![v], max_rows);
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn temporals_nulls_and_infinities() {
        assert_eq!(cell(&K::Date(0)), json!("2000-01-01"));
        assert_eq!(cell(&K::Date(-1)), json!("1999-12-31"));
        assert_eq!(cell(&K::Date(i32::MIN)), Value::Null);
        assert_eq!(cell(&K::Date(i32::MAX)), json!("0W"));
        assert_eq!(
            cell(&K::Timestamp(1_500_000_000)),
            json!("2000-01-01 00:00:01.500000000")
        );
        assert_eq!(
            cell(&K::Timestamp(86_400_000_000_000)),
            json!("2000-01-02 00:00:00")
        );
        assert_eq!(cell(&K::Timestamp(i64::MIN)), Value::Null);
        assert_eq!(cell(&K::Month(13)), json!("2001-02"));
        assert_eq!(cell(&K::Month(-1)), json!("1999-12"));
        assert_eq!(cell(&K::Datetime(1.5)), json!("2000-01-02 12:00:00.000"));
        assert_eq!(cell(&K::Datetime(f64::NAN)), Value::Null);
        assert_eq!(
            cell(&K::Timespan(90_061_000_000_001)),
            json!("1D01:01:01.000000001")
        );
        assert_eq!(cell(&K::Timespan(-1)), json!("-0D00:00:00.000000001"));
        assert_eq!(cell(&K::Minute(61)), json!("01:01"));
        assert_eq!(cell(&K::Second(3_661)), json!("01:01:01"));
        assert_eq!(cell(&K::Time(45_296_789)), json!("12:34:56.789"));
        assert_eq!(cell(&K::Time(i32::MIN)), Value::Null);
        assert_eq!(cell(&K::Long(i64::MIN)), Value::Null);
        assert_eq!(cell(&K::Long(i64::MAX)), json!("0W"));
        assert_eq!(cell(&K::Long(-i64::MAX)), json!("-0W"));
        assert_eq!(cell(&K::Int(7)), json!(7));
        assert_eq!(cell(&K::Short(i16::MIN)), Value::Null);
        assert_eq!(cell(&K::Float(f64::NAN)), Value::Null);
        assert_eq!(cell(&K::Float(f64::NEG_INFINITY)), json!("-0w"));
        assert_eq!(cell(&K::Symbol(String::new())), Value::Null);
        assert_eq!(cell(&K::Char(b' ')), Value::Null);
        assert_eq!(cell(&K::Guid([0; 16])), Value::Null);
        assert_eq!(
            cell(&K::Guid([0x11; 16])),
            json!("11111111-1111-1111-1111-111111111111")
        );
        assert_eq!(cell(&K::Bool(true)), json!(true));
    }

    #[test]
    fn temporals_past_chrono_s_range_do_not_panic() {
        // Past ±262,000 years: the raw count, not a panic.
        assert_eq!(cell(&K::Date(2_147_483_646)), json!("2147483646"));
        assert_eq!(cell(&K::Date(-2_147_483_646)), json!("-2147483646"));
        assert_eq!(cell(&K::Datetime(1.0e300)), json!(1.0e300.to_string()));
        assert_eq!(cell(&K::Datetime(-2.0e12)), json!((-2.0e12).to_string()));
        assert_eq!(cell(&K::Datetime(2_147_483_646.0)), json!("2147483646"));
        // Every timestamp fits, the extremes included.
        assert!(
            matches!(cell(&K::Timestamp(i64::MAX - 1)), Value::String(s) if s.starts_with("2292-"))
        );
        assert!(
            matches!(cell(&K::Timestamp(-i64::MAX + 1)), Value::String(s) if s.starts_with("1707-"))
        );
        // Dates near the edge of what chrono writes still show as dates.
        assert_eq!(cell(&K::Date(2_932_896)), json!("+10029-12-30"));
    }

    #[test]
    fn meta_letters_and_names() {
        assert_eq!(meta_type('j'), "long");
        assert_eq!(meta_type('s'), "symbol");
        assert_eq!(meta_type('C'), "string");
        assert_eq!(meta_type('J'), "long list");
        assert_eq!(meta_type(' '), "list");
        assert_eq!(meta_type('p'), "timestamp");
        for c in "bgxhijefcspmdznuvtC".chars() {
            assert_eq!(meta_letter(&meta_type(c)), Some(c), "{c}");
        }
        assert_eq!(meta_letter("nope"), None);
    }

    #[test]
    fn tables_keyed_tables_dicts_lists_and_atoms() {
        let t = K::Table(
            vec!["sym".into(), "px".into(), "note".into()],
            vec![
                K::Vector(0, Vector::Symbol(vec!["a".into(), "b".into()])),
                K::Vector(0, Vector::Float(vec![1.5, f64::NAN])),
                K::List(vec![
                    K::Vector(0, Vector::Char(b"hi".to_vec())),
                    K::Vector(0, Vector::Char(b"".to_vec())),
                ]),
            ],
        );
        let mut out = QueryOutcome::default();
        emit(t, 100, &mut out);
        let r = &out.results[0];
        assert_eq!(
            r.columns
                .iter()
                .map(|c| (c.name.as_str(), c.type_name.as_str()))
                .collect::<Vec<_>>(),
            [("sym", "symbol"), ("px", "float"), ("note", "string")]
        );
        assert_eq!(
            r.rows,
            vec![
                vec![json!("a"), json!(1.5), json!("hi")],
                vec![json!("b"), Value::Null, json!("")]
            ]
        );

        let keyed = K::Dict(
            Box::new(K::Table(
                vec!["k".into()],
                vec![K::Vector(0, Vector::Long(vec![1]))],
            )),
            Box::new(K::Table(
                vec!["v".into()],
                vec![K::Vector(0, Vector::Symbol(vec!["x".into()]))],
            )),
        );
        let mut out = QueryOutcome::default();
        emit(keyed, 100, &mut out);
        assert_eq!(out.results[0].columns.len(), 2);
        assert_eq!(out.results[0].rows, vec![vec![json!(1), json!("x")]]);

        let dict = K::Dict(
            Box::new(K::Vector(0, Vector::Symbol(vec!["a".into(), "b".into()]))),
            Box::new(K::Vector(0, Vector::Long(vec![1, 2]))),
        );
        let mut out = QueryOutcome::default();
        emit(dict, 100, &mut out);
        assert_eq!(
            out.results[0].rows,
            vec![vec![json!("a"), json!(1)], vec![json!("b"), json!(2)]]
        );

        let mut out = QueryOutcome::default();
        emit(K::Vector(0, Vector::Long((0..10).collect())), 3, &mut out);
        assert_eq!(out.results[0].rows.len(), 3);
        assert!(out.results[0].truncated);

        let mut out = QueryOutcome::default();
        emit(K::Long(42), 100, &mut out);
        assert_eq!(out.results[0].columns[0].name, "result");
        assert_eq!(out.results[0].rows, vec![vec![json!(42)]]);

        let mut out = QueryOutcome::default();
        emit(K::Vector(0, Vector::Char(b"text".to_vec())), 100, &mut out);
        assert_eq!(out.results[0].rows, vec![vec![json!("text")]]);

        // A nested list in a cell: compact JSON.
        let mut out = QueryOutcome::default();
        emit(
            K::List(vec![
                K::Vector(0, Vector::Long(vec![1, 2])),
                K::Symbol("s".into()),
            ]),
            100,
            &mut out,
        );
        assert_eq!(
            out.results[0].rows,
            vec![vec![json!("[1,2]")], vec![json!("s")]]
        );

        let mut out = QueryOutcome::default();
        emit(K::Nil, 100, &mut out);
        assert!(out.results[0].columns.is_empty());
    }

    #[test]
    fn a_nesting_past_the_decode_limit_shows_cut() {
        // Built here (a decoded message can't be this deep): the cell shows
        // `…` where the limit is, instead of recursing on.
        let mut k = K::Long(1);
        for _ in 0..crate::ipc::MAX_DEPTH + 10 {
            k = K::List(vec![k]);
        }
        let v = nested_json(&k).to_string();
        assert!(v.contains("[\"…\"]"), "{v}");
        assert_eq!(v.matches('[').count(), crate::ipc::MAX_DEPTH);
    }
}
