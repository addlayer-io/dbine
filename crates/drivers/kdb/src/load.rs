//! Bulk load: each batch goes to kdb+ as one IPC message, a typed
//! columnar table applied to [`LOAD`] (`(LOAD; `t; flip `c1`c2!(…))`):
//! no q text is built from the values, and kdb+ appends the whole batch in
//! one `insert` (`upsert` on a keyed table).
//!
//! Each column is built as a vector of the target column's own q type (its
//! `meta` letter), so the table matches the target's types. A column whose
//! cells don't all convert (a text that isn't a number, `0W`, a timespan in
//! q's own form…) goes as strings instead, and [`LOAD`] casts them on the
//! server (`"J"$…`), as the insert scripts do.

use crate::edit;
use crate::ipc::{Vector, K};
use chrono::{DateTime, Datelike, NaiveDate, NaiveDateTime, NaiveTime, Timelike};
use dbine_driver::transfer::Cell;
use serde_json::Value;

/// The lambda each batch is applied to: `n` the table's name, `b` the
/// batch. A column sent as strings (a general list) is cast to the
/// target's type; string columns, general ones and lists of vectors stay
/// as sent. A keyed table takes the batch by `upsert` (a key already there
/// is updated, as kdb+ does), any other by `insert`. Answers the rows
/// loaded.
pub const LOAD: &str = "{[n;b] m:exec c!t from 0!meta n; k:cols b; b:flip k!{$[(x=\" \")|x within \"AZ\";y;0h=type y;(upper x)$y;y]}'[m k;b k]; $[count keys n;n upsert b;n insert b]; count b}";

/// Most rows in one message…
pub const MESSAGE_ROWS: u64 = 50_000;
/// …and most bytes (as [`Cell::size`] estimates them): far under the 2 GB
/// a kdb+ message can take.
pub const MESSAGE_BYTES: u64 = 64 * 1024 * 1024;

const NANOS_PER_DAY: i64 = 86_400_000_000_000;

fn epoch() -> NaiveDateTime {
    NaiveDate::from_ymd_opt(2000, 1, 1)
        .expect("2000-01-01")
        .and_hms_opt(0, 0, 0)
        .expect("midnight")
}

/// A batch's rows as a table of `columns`, each typed by its `meta` letter
/// in `types` (a blank: unknown, typed by its cells).
pub fn table(columns: &[String], types: &[char], rows: &[Vec<Cell>]) -> K {
    let cols = (0..columns.len())
        .map(|j| {
            let cells: Vec<&Cell> = rows
                .iter()
                .map(|r| r.get(j).unwrap_or(&Cell::Null))
                .collect();
            column(types.get(j).copied().unwrap_or(' '), &cells)
        })
        .collect();
    K::Table(columns.to_vec(), cols)
}

/// One column of type `letter`: a typed vector, or strings when a cell
/// doesn't convert.
pub fn column(letter: char, cells: &[&Cell]) -> K {
    let letter = if letter == ' ' { infer(cells) } else { letter };
    typed(letter, cells).unwrap_or_else(|| strings(cells, edit::value_text))
}

/// The letter a column of unknown type takes from its first value.
fn infer(cells: &[&Cell]) -> char {
    match cells.iter().find(|c| !matches!(c, Cell::Null)) {
        Some(Cell::Bool(_)) => 'b',
        Some(Cell::Int(_) | Cell::UInt(_)) => {
            if cells.iter().any(|c| matches!(c, Cell::Float(_))) {
                'f'
            } else {
                'j'
            }
        }
        Some(Cell::Float(_) | Cell::Decimal(_)) => 'f',
        Some(Cell::Date(_)) => 'd',
        Some(Cell::Time(_)) => 'n',
        Some(Cell::DateTime(_) | Cell::DateTimeTz(_)) => 'p',
        Some(Cell::Uuid(_)) => 'g',
        Some(Cell::Bytes(_)) => 'X',
        _ => 'C',
    }
}

/// Every cell converted by `f`, nulls as `null`; `None` if one doesn't.
fn each<T: Clone>(cells: &[&Cell], null: T, f: impl Fn(&Cell) -> Option<T>) -> Option<Vec<T>> {
    cells
        .iter()
        .map(|c| match c {
            Cell::Null => Some(null.clone()),
            c => f(c),
        })
        .collect()
}

fn typed(letter: char, cells: &[&Cell]) -> Option<K> {
    let v = match letter {
        'b' => Vector::Bool(each(cells, false, bool_of)?),
        'g' => Vector::Guid(each(cells, [0; 16], guid_of)?),
        'x' => Vector::Byte(each(cells, 0, |c| u8::try_from(int_of(c)?).ok())?),
        'h' => Vector::Short(each(cells, i16::MIN, |c| {
            i16::try_from(int_of(c)?).ok().filter(|&n| n != i16::MIN)
        })?),
        'i' => Vector::Int(each(cells, i32::MIN, |c| {
            i32::try_from(int_of(c)?).ok().filter(|&n| n != i32::MIN)
        })?),
        'j' => Vector::Long(each(cells, i64::MIN, |c| {
            int_of(c).filter(|&n| n != i64::MIN)
        })?),
        'e' => Vector::Real(each(cells, f32::NAN, |c| float_of(c).map(|f| f as f32))?),
        'f' => Vector::Float(each(cells, f64::NAN, float_of)?),
        'c' => Vector::Char(each(cells, b' ', |c| {
            Some(text(c).bytes().next().unwrap_or(b' '))
        })?),
        's' => Vector::Symbol(each(cells, String::new(), |c| Some(text(c)))?),
        'p' => Vector::Timestamp(each(cells, i64::MIN, timestamp_of)?),
        'm' => Vector::Month(each(cells, i32::MIN, month_of)?),
        'd' => Vector::Date(each(cells, i32::MIN, |c| {
            i32::try_from(timestamp_of(c)?.div_euclid(NANOS_PER_DAY)).ok()
        })?),
        'z' => Vector::Datetime(each(cells, f64::NAN, |c| {
            Some(timestamp_of(c)? as f64 / NANOS_PER_DAY as f64)
        })?),
        'n' => Vector::Timespan(each(cells, i64::MIN, time_of)?),
        'u' => Vector::Minute(each(cells, i32::MIN, |c| {
            i32::try_from(time_of(c)? / 60_000_000_000).ok()
        })?),
        'v' => Vector::Second(each(cells, i32::MIN, |c| {
            i32::try_from(time_of(c)? / 1_000_000_000).ok()
        })?),
        't' => Vector::Time(each(cells, i32::MIN, |c| {
            i32::try_from(time_of(c)? / 1_000_000).ok()
        })?),
        // A string per row.
        'C' => return Some(strings(cells, text_of)),
        // A list of vectors per row: bytes, or a JSON array of values.
        c if c.is_ascii_uppercase() => {
            let lower = c.to_ascii_lowercase();
            let items = cells
                .iter()
                .map(|cell| match cell {
                    Cell::Null => typed(lower, &[]),
                    Cell::Bytes(b) if lower == 'x' => Some(K::Vector(0, Vector::Byte(b.clone()))),
                    Cell::Json(s) => match serde_json::from_str::<Value>(s).ok()? {
                        Value::Array(a) => {
                            let a: Vec<Cell> = a.iter().map(Cell::from_json).collect();
                            typed(lower, &a.iter().collect::<Vec<_>>())
                        }
                        _ => None,
                    },
                    _ => None,
                })
                .collect::<Option<Vec<K>>>()?;
            return Some(K::List(items));
        }
        _ => return None,
    };
    Some(K::Vector(0, v))
}

/// The cells as a general list of strings, each written by `f`.
fn strings(cells: &[&Cell], f: impl Fn(&Value) -> String) -> K {
    K::List(
        cells
            .iter()
            .map(|c| K::Vector(0, Vector::Char(f(&c.to_json()).into_bytes())))
            .collect(),
    )
}

/// A grid value's text as is (null: empty).
fn text_of(v: &Value) -> String {
    match v {
        Value::Null => String::new(),
        Value::String(s) => s.clone(),
        other => other.to_string(),
    }
}

fn text(c: &Cell) -> String {
    text_of(&c.to_json())
}

fn bool_of(c: &Cell) -> Option<bool> {
    match c {
        Cell::Bool(b) => Some(*b),
        Cell::Int(n) => Some(*n != 0),
        Cell::UInt(n) => Some(*n != 0),
        Cell::Text(s) => match s.trim().to_ascii_lowercase().as_str() {
            "1" | "1b" | "true" => Some(true),
            "0" | "0b" | "false" => Some(false),
            _ => None,
        },
        _ => None,
    }
}

fn int_of(c: &Cell) -> Option<i64> {
    match c {
        Cell::Int(n) => Some(*n),
        Cell::UInt(n) => i64::try_from(*n).ok(),
        Cell::Bool(b) => Some(i64::from(*b)),
        Cell::Float(f) if f.fract() == 0.0 && f.abs() < 9.2e18 => Some(*f as i64),
        Cell::Decimal(s) | Cell::Text(s) => {
            let s = s.trim();
            s.parse::<i64>().ok().or_else(|| {
                let f: f64 = s.parse().ok()?;
                (f.is_finite() && f.fract() == 0.0 && f.abs() < 9.2e18).then_some(f as i64)
            })
        }
        _ => None,
    }
}

fn float_of(c: &Cell) -> Option<f64> {
    match c {
        Cell::Int(n) => Some(*n as f64),
        Cell::UInt(n) => Some(*n as f64),
        Cell::Bool(b) => Some(f64::from(u8::from(*b))),
        Cell::Float(f) => Some(*f),
        Cell::Decimal(s) | Cell::Text(s) => s.trim().parse().ok().filter(|f: &f64| !f.is_nan()),
        _ => None,
    }
}

fn guid_of(c: &Cell) -> Option<[u8; 16]> {
    let (Cell::Uuid(s) | Cell::Text(s)) = c else {
        return None;
    };
    let hex: Vec<u8> = s.bytes().filter(|&b| b != b'-').collect();
    if hex.len() != 32 {
        return None;
    }
    let mut g = [0u8; 16];
    for (i, pair) in hex.chunks(2).enumerate() {
        g[i] = u8::from_str_radix(std::str::from_utf8(pair).ok()?, 16).ok()?;
    }
    Some(g)
}

/// Nanoseconds since 2000-01-01 of a date or a date and time (one with an
/// offset, in UTC).
fn timestamp_of(c: &Cell) -> Option<i64> {
    let (Cell::Date(s) | Cell::DateTime(s) | Cell::DateTimeTz(s) | Cell::Text(s)) = c else {
        return None;
    };
    let s = s.trim();
    let t = parse_datetime(s)?;
    (t - epoch()).num_nanoseconds()
}

fn parse_datetime(s: &str) -> Option<NaiveDateTime> {
    const NAIVE: [&str; 4] = [
        "%Y-%m-%d %H:%M:%S%.f",
        "%Y-%m-%dT%H:%M:%S%.f",
        "%Y-%m-%d %H:%M",
        "%Y.%m.%dD%H:%M:%S%.f",
    ];
    const ZONED: [&str; 4] = [
        "%Y-%m-%d %H:%M:%S%.f%:z",
        "%Y-%m-%dT%H:%M:%S%.f%:z",
        "%Y-%m-%d %H:%M:%S%.f%#z",
        "%Y-%m-%dT%H:%M:%S%.fZ",
    ];
    // Only the formats the text's shape can be (a load parses every cell).
    let Some(time) = s.get(10..).filter(|t| !t.is_empty()) else {
        return ["%Y-%m-%d", "%Y.%m.%d"]
            .iter()
            .find_map(|f| NaiveDate::parse_from_str(s, f).ok())
            .and_then(|d| d.and_hms_opt(0, 0, 0));
    };
    let zoned = time.contains(['+', 'Z']) || time.get(1..).is_some_and(|t| t.contains('-'));
    if !zoned {
        return NAIVE
            .iter()
            .find_map(|f| NaiveDateTime::parse_from_str(s, f).ok());
    }
    ZONED
        .iter()
        .find_map(|f| DateTime::parse_from_str(s, f).ok())
        .map(|t| t.naive_utc())
        .or_else(|| {
            let s = s.strip_suffix('Z')?;
            NaiveDateTime::parse_from_str(s, "%Y-%m-%dT%H:%M:%S%.f").ok()
        })
}

/// Months since 2000.01 of a date or of `YYYY-MM`.
fn month_of(c: &Cell) -> Option<i32> {
    let (y, m) = match c {
        Cell::Date(_) | Cell::DateTime(_) | Cell::DateTimeTz(_) | Cell::Text(_) => {
            let text = text(c);
            let s = text.trim();
            match parse_datetime(s) {
                Some(t) => (t.year(), t.month() as i32),
                None => {
                    let (y, m) = s.split_once(['-', '.'])?;
                    let m: i32 = m.parse().ok().filter(|m| (1..=12).contains(m))?;
                    (y.parse().ok()?, m)
                }
            }
        }
        _ => return None,
    };
    Some((y - 2000) * 12 + m - 1)
}

/// Nanoseconds since midnight of a time of day.
fn time_of(c: &Cell) -> Option<i64> {
    let (Cell::Time(s) | Cell::Text(s)) = c else {
        return None;
    };
    let t = ["%H:%M:%S%.f", "%H:%M"]
        .iter()
        .find_map(|f| NaiveTime::parse_from_str(s.trim(), f).ok())?;
    Some(i64::from(t.num_seconds_from_midnight()) * 1_000_000_000 + i64::from(t.nanosecond()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ipc::{decode, message, RESPONSE};

    fn col(letter: char, cells: &[Cell]) -> K {
        column(letter, &cells.iter().collect::<Vec<_>>())
    }

    fn v(k: K) -> Vector {
        match k {
            K::Vector(_, v) => v,
            other => panic!("not a vector: {other:?}"),
        }
    }

    #[test]
    fn numbers_take_the_target_type_and_its_nulls() {
        let cells = [Cell::Int(1), Cell::Null, Cell::Text("3".into())];
        assert_eq!(v(col('j', &cells)), Vector::Long(vec![1, i64::MIN, 3]));
        assert_eq!(v(col('i', &cells)), Vector::Int(vec![1, i32::MIN, 3]));
        assert_eq!(v(col('h', &cells)), Vector::Short(vec![1, i16::MIN, 3]));
        let f = v(col(
            'f',
            &[Cell::Float(1.5), Cell::Null, Cell::Decimal("2".into())],
        ));
        let Vector::Float(f) = f else { panic!() };
        assert!(f[0] == 1.5 && f[1].is_nan() && f[2] == 2.0);
        assert_eq!(
            v(col('b', &[Cell::Bool(true), Cell::Int(0), Cell::Null])),
            Vector::Bool(vec![true, false, false])
        );
        assert_eq!(v(col('x', &[Cell::Int(255)])), Vector::Byte(vec![255]));
    }

    #[test]
    fn text_and_symbols() {
        assert_eq!(
            v(col('s', &[Cell::Text("AAPL".into()), Cell::Null])),
            Vector::Symbol(vec!["AAPL".into(), String::new()])
        );
        assert_eq!(
            v(col('c', &[Cell::Text("xy".into())])),
            Vector::Char(vec![b'x'])
        );
        assert_eq!(
            col('C', &[Cell::Text("2024-01-31".into()), Cell::Null]),
            K::List(vec![
                K::Vector(0, Vector::Char(b"2024-01-31".to_vec())),
                K::Vector(0, Vector::Char(Vec::new())),
            ])
        );
    }

    #[test]
    fn temporals_count_from_2000() {
        assert_eq!(
            v(col('d', &[Cell::Date("2000-01-02".into()), Cell::Null])),
            Vector::Date(vec![1, i32::MIN])
        );
        assert_eq!(
            v(col('p', &[Cell::DateTime("2000-01-01 00:00:01.5".into())])),
            Vector::Timestamp(vec![1_500_000_000])
        );
        // An offset is taken to UTC.
        assert_eq!(
            v(col(
                'p',
                &[Cell::DateTimeTz("2000-01-01 01:00:00+01:00".into())]
            )),
            Vector::Timestamp(vec![0])
        );
        assert_eq!(
            v(col('m', &[Cell::Text("2001-02".into())])),
            Vector::Month(vec![13])
        );
        assert_eq!(
            v(col('t', &[Cell::Time("01:00:00.250".into())])),
            Vector::Time(vec![3_600_250])
        );
        assert_eq!(
            v(col('n', &[Cell::Time("00:00:01".into())])),
            Vector::Timespan(vec![1_000_000_000])
        );
        assert_eq!(
            v(col('u', &[Cell::Time("02:03".into())])),
            Vector::Minute(vec![123])
        );
        assert_eq!(
            v(col('v', &[Cell::Time("00:01:05".into())])),
            Vector::Second(vec![65])
        );
        let Vector::Datetime(z) = v(col('z', &[Cell::Date("2000-01-03".into())])) else {
            panic!()
        };
        assert_eq!(z, vec![2.0]);
    }

    #[test]
    fn guids_and_vectors() {
        let g = "00112233-4455-6677-8899-aabbccddeeff";
        let Vector::Guid(got) = v(col('g', &[Cell::Uuid(g.into()), Cell::Null])) else {
            panic!()
        };
        assert_eq!(got[0][0], 0x00);
        assert_eq!(got[0][15], 0xff);
        assert_eq!(got[1], [0; 16]);
        assert_eq!(
            col('J', &[Cell::Json("[1,2]".into()), Cell::Null]),
            K::List(vec![
                K::Vector(0, Vector::Long(vec![1, 2])),
                K::Vector(0, Vector::Long(Vec::new())),
            ])
        );
        assert_eq!(
            col('X', &[Cell::Bytes(vec![1, 2])]),
            K::List(vec![K::Vector(0, Vector::Byte(vec![1, 2]))])
        );
    }

    #[test]
    fn what_doesnt_convert_goes_as_strings_q_casts() {
        // `0W` (q's infinity) and a date in an int column: cast on the server.
        assert_eq!(
            col('j', &[Cell::Int(1), Cell::Text("0W".into()), Cell::Null]),
            K::List(vec![
                K::Vector(0, Vector::Char(b"1".to_vec())),
                K::Vector(0, Vector::Char(b"0W".to_vec())),
                K::Vector(0, Vector::Char(Vec::new())),
            ])
        );
        // ISO dates in the form q's cast reads.
        assert_eq!(
            col(
                'd',
                &[
                    Cell::Text("someday".into()),
                    Cell::Date("2024-01-31".into())
                ]
            ),
            K::List(vec![
                K::Vector(0, Vector::Char(b"someday".to_vec())),
                K::Vector(0, Vector::Char(b"2024.01.31".to_vec())),
            ])
        );
    }

    #[test]
    fn untyped_columns_are_typed_by_their_cells() {
        assert_eq!(
            v(col(' ', &[Cell::Null, Cell::Int(2)])),
            Vector::Long(vec![i64::MIN, 2])
        );
        let Vector::Float(f) = v(col(' ', &[Cell::Int(1), Cell::Float(2.5)])) else {
            panic!()
        };
        assert_eq!(f, vec![1.0, 2.5]);
        assert!(matches!(col(' ', &[Cell::Text("a".into())]), K::List(_)));
    }

    #[test]
    fn a_batch_is_one_typed_table_on_the_wire() {
        let rows = vec![
            vec![Cell::Int(1), Cell::Text("a".into()), Cell::Float(1.5)],
            vec![Cell::Int(2), Cell::Null],
        ];
        let t = table(
            &["id".into(), "sym".into(), "px".into()],
            &['j', 's', 'f'],
            &rows,
        );
        let K::Table(names, cols) = &t else { panic!() };
        assert_eq!(names, &["id", "sym", "px"]);
        assert_eq!(cols[0], K::Vector(0, Vector::Long(vec![1, 2])));
        assert_eq!(
            cols[1],
            K::Vector(0, Vector::Symbol(vec!["a".into(), String::new()]))
        );
        // The encoder writes it as kdb+ does (`98 0 99`, symbols, columns)
        // and reads it back the same.
        let m = message(RESPONSE, &t);
        assert_eq!(&m[8..13], &[98, 0, 99, 11, 0]);
        let K::Table(n2, c2) = decode(&m).unwrap() else {
            panic!()
        };
        assert_eq!(n2, *names);
        assert_eq!(c2[0], cols[0]);
        assert_eq!(c2[1], cols[1]);
    }
}
