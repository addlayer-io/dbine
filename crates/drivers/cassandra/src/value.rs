//! CQL values as result cells: text as text, integers as numbers (strings
//! past 2^53), decimals and varints as exact strings, dates and times in
//! ISO form, UUIDs as strings, blobs as `0x…`, collections, tuples and
//! UDTs as compact JSON.

use chrono::{DateTime, NaiveDate, NaiveTime, Timelike};
use dbine_driver::{json_bytes, json_f64, json_i64};
use scylla::value::CqlValue;
use serde_json::Value as J;

pub fn cell(v: Option<&CqlValue>) -> J {
    let Some(v) = v else { return J::Null };
    match v {
        CqlValue::List(_)
        | CqlValue::Set(_)
        | CqlValue::Map(_)
        | CqlValue::Tuple(_)
        | CqlValue::Vector(_)
        | CqlValue::UserDefinedType { .. } => J::String(to_json(v).to_string()),
        other => to_json(other),
    }
}

/// A value as JSON, keeping collections as arrays and objects.
pub fn to_json(v: &CqlValue) -> J {
    match v {
        CqlValue::Ascii(s) | CqlValue::Text(s) => J::String(s.clone()),
        CqlValue::Boolean(b) => J::Bool(*b),
        CqlValue::Blob(b) => json_bytes(b),
        CqlValue::Counter(c) => json_i64(c.0),
        CqlValue::Decimal(d) => {
            let (bytes, scale) = d.as_signed_be_bytes_slice_and_exponent();
            J::String(decimal(bytes, scale))
        }
        CqlValue::Varint(v) => J::String(decimal(v.as_signed_bytes_be_slice(), 0)),
        CqlValue::Date(d) => J::String(date(d.0)),
        CqlValue::Double(f) => json_f64(*f),
        // Through its shortest text so 1.1f doesn't become 1.100000023841858.
        CqlValue::Float(f) => json_f64(f.to_string().parse().unwrap_or(f64::from(*f))),
        CqlValue::Duration(d) => J::String(duration(d.months, d.days, d.nanoseconds)),
        CqlValue::Empty => J::Null,
        CqlValue::Int(i) => J::from(*i),
        CqlValue::BigInt(i) => json_i64(*i),
        CqlValue::SmallInt(i) => J::from(*i),
        CqlValue::TinyInt(i) => J::from(*i),
        CqlValue::Timestamp(t) => J::String(timestamp(t.0)),
        CqlValue::Time(t) => J::String(time(t.0)),
        CqlValue::Inet(ip) => J::String(ip.to_string()),
        CqlValue::Uuid(u) => J::String(u.to_string()),
        CqlValue::Timeuuid(u) => J::String(u.to_string()),
        CqlValue::List(items) | CqlValue::Set(items) | CqlValue::Vector(items) => {
            J::Array(items.iter().map(to_json).collect())
        }
        CqlValue::Tuple(items) => J::Array(items.iter().map(|i| i.as_ref().map_or(J::Null, to_json)).collect()),
        CqlValue::Map(pairs) => {
            if pairs.iter().all(|(k, _)| matches!(k, CqlValue::Text(_) | CqlValue::Ascii(_))) {
                let mut o = serde_json::Map::new();
                for (k, v) in pairs {
                    if let CqlValue::Text(k) | CqlValue::Ascii(k) = k {
                        o.insert(k.clone(), to_json(v));
                    }
                }
                J::Object(o)
            } else {
                J::Array(pairs.iter().map(|(k, v)| J::Array(vec![to_json(k), to_json(v)])).collect())
            }
        }
        CqlValue::UserDefinedType { fields, .. } => {
            let mut o = serde_json::Map::new();
            for (name, v) in fields {
                o.insert(name.clone(), v.as_ref().map_or(J::Null, to_json));
            }
            J::Object(o)
        }
        other => J::String(format!("{other:?}")),
    }
}

/// A two's-complement big-endian integer scaled by 10^-scale, exactly.
pub(crate) fn decimal(bytes: &[u8], scale: i32) -> String {
    let digits = match big_to_string(bytes) {
        Some(d) => d,
        None => return json_bytes(bytes).as_str().unwrap_or_default().to_string(),
    };
    let (neg, digits) = match digits.strip_prefix('-') {
        Some(d) => (true, d.to_string()),
        None => (false, digits),
    };
    let body = if scale <= 0 {
        if digits == "0" {
            digits
        } else {
            digits + &"0".repeat(scale.unsigned_abs() as usize)
        }
    } else {
        let scale = scale as usize;
        let padded = format!("{digits:0>width$}", width = scale + 1);
        let (int, frac) = padded.split_at(padded.len() - scale);
        format!("{int}.{frac}")
    };
    if neg {
        format!("-{body}")
    } else {
        body
    }
}

/// Decimal digits of a signed big-endian integer of any length.
fn big_to_string(bytes: &[u8]) -> Option<String> {
    if bytes.is_empty() {
        return Some("0".into());
    }
    let neg = bytes[0] & 0x80 != 0;
    // Magnitude, as big-endian bytes.
    let mut mag: Vec<u8> = if neg {
        let mut v: Vec<u8> = bytes.iter().map(|b| !b).collect();
        for b in v.iter_mut().rev() {
            let (r, carry) = b.overflowing_add(1);
            *b = r;
            if !carry {
                break;
            }
        }
        v
    } else {
        bytes.to_vec()
    };
    if bytes.len() > 4096 {
        return None;
    }
    // Repeated division by 10 of a base-256 number.
    let mut digits = Vec::new();
    while mag.iter().any(|b| *b != 0) {
        let mut rem: u32 = 0;
        for b in mag.iter_mut() {
            let cur = (rem << 8) | u32::from(*b);
            *b = (cur / 10) as u8;
            rem = cur % 10;
        }
        digits.push(b'0' + rem as u8);
    }
    if digits.is_empty() {
        digits.push(b'0');
    }
    if neg {
        digits.push(b'-');
    }
    digits.reverse();
    String::from_utf8(digits).ok()
}

/// CQL `date`: days since the epoch, offset by 2^31.
pub(crate) fn date(raw: u32) -> String {
    let days = i64::from(raw) - (1i64 << 31);
    NaiveDate::from_ymd_opt(1970, 1, 1)
        .and_then(|e| e.checked_add_signed(chrono::Duration::days(days)))
        .map_or_else(|| days.to_string(), |d| d.format("%Y-%m-%d").to_string())
}

/// CQL `timestamp`: milliseconds since the epoch, shown in UTC.
pub(crate) fn timestamp(ms: i64) -> String {
    DateTime::from_timestamp_millis(ms).map_or_else(
        || ms.to_string(),
        |t| {
            if ms % 1000 == 0 {
                t.format("%Y-%m-%d %H:%M:%S").to_string()
            } else {
                t.format("%Y-%m-%d %H:%M:%S%.3f").to_string()
            }
        },
    )
}

/// CQL `time`: nanoseconds since midnight.
pub(crate) fn time(ns: i64) -> String {
    let secs = (ns / 1_000_000_000) as u32;
    let nanos = (ns % 1_000_000_000) as u32;
    NaiveTime::from_num_seconds_from_midnight_opt(secs, nanos).map_or_else(
        || ns.to_string(),
        |t| {
            if t.nanosecond() == 0 {
                t.format("%H:%M:%S").to_string()
            } else {
                t.format("%H:%M:%S%.9f").to_string()
            }
        },
    )
}

/// CQL's own duration notation (`1mo2d3h4m5s6ms7us8ns`).
pub(crate) fn duration(months: i32, days: i32, nanos: i64) -> String {
    let neg = months < 0 || days < 0 || nanos < 0;
    let (months, days, mut n) = (months.unsigned_abs(), days.unsigned_abs(), nanos.unsigned_abs());
    let mut s = String::new();
    if neg {
        s.push('-');
    }
    let (y, mo) = (months / 12, months % 12);
    for (v, unit) in [(u64::from(y), "y"), (u64::from(mo), "mo"), (u64::from(days), "d")] {
        if v > 0 {
            s.push_str(&format!("{v}{unit}"));
        }
    }
    for (size, unit) in [
        (3_600_000_000_000u64, "h"),
        (60_000_000_000, "m"),
        (1_000_000_000, "s"),
        (1_000_000, "ms"),
        (1_000, "us"),
        (1, "ns"),
    ] {
        if n >= size {
            s.push_str(&format!("{}{unit}", n / size));
            n %= size;
        }
    }
    if s.is_empty() || s == "-" {
        s = "0s".into();
    }
    s
}

#[cfg(test)]
mod tests {
    use super::*;
    use scylla::value::{Counter, CqlDate, CqlTime, CqlTimestamp};

    #[test]
    fn scalars() {
        assert_eq!(cell(None), J::Null);
        assert_eq!(cell(Some(&CqlValue::Int(5))), J::from(5));
        assert_eq!(cell(Some(&CqlValue::BigInt(i64::MAX))), J::from("9223372036854775807"));
        assert_eq!(cell(Some(&CqlValue::Counter(Counter(3)))), J::from(3));
        assert_eq!(cell(Some(&CqlValue::Float(1.1))), J::from(1.1));
        assert_eq!(cell(Some(&CqlValue::Blob(vec![0xca, 0xfe]))), J::from("0xCAFE"));
        assert_eq!(cell(Some(&CqlValue::Text("x".into()))), J::from("x"));
    }

    #[test]
    fn dates_and_times() {
        assert_eq!(cell(Some(&CqlValue::Date(CqlDate(1 << 31)))), J::from("1970-01-01"));
        assert_eq!(cell(Some(&CqlValue::Date(CqlDate((1 << 31) + 19_753)))), J::from("2024-01-31"));
        assert_eq!(cell(Some(&CqlValue::Timestamp(CqlTimestamp(1_706_708_700_000)))), J::from("2024-01-31 13:45:00"));
        assert_eq!(cell(Some(&CqlValue::Timestamp(CqlTimestamp(1_706_708_700_123)))), J::from("2024-01-31 13:45:00.123"));
        assert_eq!(cell(Some(&CqlValue::Time(CqlTime(3_723_000_000_000)))), J::from("01:02:03"));
        assert_eq!(duration(14, 2, 3_600_000_000_000 + 5_000_000), "1y2mo2d1h5ms");
        assert_eq!(duration(0, 0, 0), "0s");
    }

    #[test]
    fn exact_numbers() {
        assert_eq!(big_to_string(&[0x01, 0x00]).unwrap(), "256");
        assert_eq!(big_to_string(&[0xff]).unwrap(), "-1");
        assert_eq!(big_to_string(&[0xff, 0x00]).unwrap(), "-256");
        assert_eq!(big_to_string(&[0x00]).unwrap(), "0");
        // 2^64 needs 9 bytes.
        assert_eq!(big_to_string(&[0x01, 0, 0, 0, 0, 0, 0, 0, 0]).unwrap(), "18446744073709551616");
        assert_eq!(decimal(&[0x04, 0xd2], 2), "12.34");
        assert_eq!(decimal(&[0xfb, 0x2e], 2), "-12.34");
        assert_eq!(decimal(&[0x05], 3), "0.005");
        assert_eq!(decimal(&[0x05], -2), "500");
    }

    #[test]
    fn collections_are_json() {
        let list = CqlValue::List(vec![CqlValue::Int(1), CqlValue::Text("a".into())]);
        assert_eq!(cell(Some(&list)), J::from(r#"[1,"a"]"#));
        let map = CqlValue::Map(vec![(CqlValue::Text("k".into()), CqlValue::Int(1))]);
        assert_eq!(cell(Some(&map)), J::from(r#"{"k":1}"#));
        let map = CqlValue::Map(vec![(CqlValue::Int(1), CqlValue::Boolean(true))]);
        assert_eq!(cell(Some(&map)), J::from("[[1,true]]"));
        let udt = CqlValue::UserDefinedType {
            keyspace: "ks".into(),
            name: "addr".into(),
            fields: vec![("city".into(), Some(CqlValue::Text("x".into()))), ("zip".into(), None)],
        };
        assert_eq!(cell(Some(&udt)), J::from(r#"{"city":"x","zip":null}"#));
    }
}
