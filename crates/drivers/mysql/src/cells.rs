//! MySQL values as JSON cells. Scripts go over the text protocol, which
//! returns every cell as bytes; column metadata decides whether they
//! become numbers, text or hex. Prepared statements (protected reads) go
//! over the binary protocol, whose typed values are written the way the
//! text protocol would send them.

use dbine_driver::{
    json_bytes, json_f64, json_i64, json_spatial_bytes, json_u64, ResultColumn, VectorElement,
    VectorMeta,
};
use mysql_async::consts::{ColumnFlags, ColumnType};
use mysql_async::{Column, Value};
use serde_json::Value as Json;

/// MySQL's charset number for binary strings (BLOB, BINARY, VARBINARY).
const BINARY_CHARSET: u16 = 63;

/// `MYSQL_TYPE_VAR_STRING` → `var_string`.
pub fn type_name(t: ColumnType) -> String {
    let s = format!("{t:?}");
    s.strip_prefix("MYSQL_TYPE_")
        .unwrap_or(&s)
        .to_ascii_lowercase()
}

/// A cell as JSON, using the column to read text-protocol bytes (and
/// binary-protocol values as the text protocol would have sent them).
pub fn cell(col: &Column, v: Value) -> Json {
    let Value::Bytes(b) = v else {
        return binary_json(v, col);
    };
    use ColumnType::*;
    let text = || String::from_utf8_lossy(&b).into_owned();
    match col.column_type() {
        MYSQL_TYPE_TINY | MYSQL_TYPE_SHORT | MYSQL_TYPE_INT24 | MYSQL_TYPE_LONG
        | MYSQL_TYPE_LONGLONG | MYSQL_TYPE_YEAR => {
            let s = text();
            if col.flags().contains(ColumnFlags::UNSIGNED_FLAG) {
                s.parse::<u64>().map_or_else(|_| s.into(), json_u64)
            } else {
                s.parse::<i64>().map_or_else(|_| s.into(), json_i64)
            }
        }
        MYSQL_TYPE_FLOAT | MYSQL_TYPE_DOUBLE => {
            let s = text();
            s.parse::<f64>().map_or_else(|_| s.into(), json_f64)
        }
        MYSQL_TYPE_BIT => json_bytes(&b),
        // MySQL 9's VECTOR: float32s, little-endian, whole.
        MYSQL_TYPE_VECTOR => {
            let m = VectorMeta::dense(None, VectorElement::F32);
            crate::vectors::cell(&m, &b, true).map_or_else(|| json_bytes(&b), Json::String)
        }
        // SRID + WKB, whole: the map view reads it.
        MYSQL_TYPE_GEOMETRY => json_spatial_bytes(&b),
        MYSQL_TYPE_TINY_BLOB
        | MYSQL_TYPE_MEDIUM_BLOB
        | MYSQL_TYPE_LONG_BLOB
        | MYSQL_TYPE_BLOB
        | MYSQL_TYPE_STRING
        | MYSQL_TYPE_VAR_STRING
        | MYSQL_TYPE_VARCHAR
            if col.character_set() == BINARY_CHARSET =>
        {
            json_bytes(&b)
        }
        _ => match String::from_utf8(b) {
            Ok(s) => s.into(),
            Err(e) => json_bytes(e.as_bytes()),
        },
    }
}

/// A result column's header. MySQL 9's `VECTOR` says so on the wire, with
/// its size (4 bytes a dimension) when the column comes from a table.
pub fn result_column(col: &Column) -> ResultColumn {
    let name = col.name_str().into_owned();
    if col.column_type() == ColumnType::MYSQL_TYPE_VECTOR {
        let len = col.column_length();
        let dims = (!col.org_table_str().is_empty() && len > 0 && len.is_multiple_of(4))
            .then_some(len / 4);
        let ty = dims.map_or_else(|| "vector".to_string(), |n| format!("vector({n})"));
        return ResultColumn::new(name, ty)
            .with_vector(VectorMeta::dense(dims, VectorElement::F32));
    }
    ResultColumn::new(name, type_name(col.column_type()))
}

/// A cell of a column the catalog says holds vectors: the canonical
/// JSON-array text from the stored bytes (binary charset) or the engine's
/// text; as any other cell when it doesn't read as a vector.
pub fn vector_cell(meta: &VectorMeta, col: &Column, v: Value) -> Json {
    match v {
        Value::Bytes(b) => {
            match crate::vectors::cell(meta, &b, col.character_set() == BINARY_CHARSET) {
                Some(t) => t.into(),
                None => cell(col, Value::Bytes(b)),
            }
        }
        other => cell(col, other),
    }
}

/// Whether the wire's type of `col` could carry a vector the catalog
/// knows (a string or a blob).
pub fn may_be_vector(col: &Column) -> bool {
    use ColumnType::*;
    matches!(
        col.column_type(),
        MYSQL_TYPE_STRING
            | MYSQL_TYPE_VAR_STRING
            | MYSQL_TYPE_VARCHAR
            | MYSQL_TYPE_BLOB
            | MYSQL_TYPE_TINY_BLOB
            | MYSQL_TYPE_MEDIUM_BLOB
            | MYSQL_TYPE_LONG_BLOB
    )
}

/// Whether `col` holds binary strings (its bytes, not text).
pub fn is_binary(col: &Column) -> bool {
    col.character_set() == BINARY_CHARSET
}

/// A binary-protocol value as the text protocol writes it: a FLOAT in its
/// shortest form (`1.1`, not the f64 widening `1.100000023841858`), and
/// dates and times with as many fractional digits as the column has
/// (`DATETIME(3)` → `.120`, `DATETIME(6)` → `.000000`).
fn binary_json(v: Value, col: &Column) -> Json {
    let digits = usize::from(col.decimals()).min(6);
    let fraction = |us: u32| {
        if digits == 0 {
            String::new()
        } else {
            format!(".{:06}", us)[..=digits].to_string()
        }
    };
    match v {
        Value::Float(f) => json_f64(f.to_string().parse().unwrap_or(f64::from(f))),
        Value::Date(y, mo, d, h, mi, s, us) if col.column_type() != ColumnType::MYSQL_TYPE_DATE => {
            format!(
                "{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}{}",
                fraction(us)
            )
            .into()
        }
        Value::Time(neg, days, h, mi, s, us) => {
            let hours = u64::from(days) * 24 + u64::from(h);
            format!(
                "{}{hours:02}:{mi:02}:{s:02}{}",
                if neg { "-" } else { "" },
                fraction(us)
            )
            .into()
        }
        v => value_json(v, col.column_type()),
    }
}

/// A typed value (binary protocol) as JSON.
pub fn value_json(v: Value, t: ColumnType) -> Json {
    match v {
        Value::NULL => Json::Null,
        Value::Int(i) => json_i64(i),
        Value::UInt(u) => json_u64(u),
        Value::Float(f) => json_f64(f64::from(f)),
        Value::Double(f) => json_f64(f),
        Value::Bytes(b) => match String::from_utf8(b) {
            Ok(s) => s.into(),
            Err(e) => json_bytes(e.as_bytes()),
        },
        Value::Date(y, mo, d, h, mi, s, us) => {
            if t == ColumnType::MYSQL_TYPE_DATE && (h, mi, s, us) == (0, 0, 0, 0) {
                format!("{y:04}-{mo:02}-{d:02}").into()
            } else {
                format!("{y:04}-{mo:02}-{d:02} {h:02}:{mi:02}:{s:02}{}", micros(us)).into()
            }
        }
        Value::Time(neg, days, h, mi, s, us) => {
            let hours = u64::from(days) * 24 + u64::from(h);
            format!(
                "{}{hours:02}:{mi:02}:{s:02}{}",
                if neg { "-" } else { "" },
                micros(us)
            )
            .into()
        }
    }
}

/// Any value as text, for catalog queries (`None` for NULL).
pub fn value_text(v: &Value) -> Option<String> {
    match v {
        Value::NULL => None,
        Value::Bytes(b) => Some(String::from_utf8_lossy(b).into_owned()),
        other => match value_json(other.clone(), ColumnType::MYSQL_TYPE_DATETIME) {
            Json::String(s) => Some(s),
            j => Some(j.to_string()),
        },
    }
}

fn micros(us: u32) -> String {
    if us == 0 {
        String::new()
    } else {
        format!(".{us:06}")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dates_drop_zero_time_only_for_date_columns() {
        let d = Value::Date(2024, 3, 9, 0, 0, 0, 0);
        assert_eq!(
            value_json(d.clone(), ColumnType::MYSQL_TYPE_DATE),
            "2024-03-09"
        );
        assert_eq!(
            value_json(d, ColumnType::MYSQL_TYPE_DATETIME),
            "2024-03-09 00:00:00"
        );
        assert_eq!(
            value_json(
                Value::Date(2024, 3, 9, 13, 5, 7, 120),
                ColumnType::MYSQL_TYPE_DATETIME
            ),
            "2024-03-09 13:05:07.000120"
        );
    }

    #[test]
    fn times_count_total_hours() {
        assert_eq!(
            value_json(
                Value::Time(false, 1, 2, 3, 4, 0),
                ColumnType::MYSQL_TYPE_TIME
            ),
            "26:03:04"
        );
        assert_eq!(
            value_json(
                Value::Time(true, 0, 5, 0, 0, 500),
                ColumnType::MYSQL_TYPE_TIME
            ),
            "-05:00:00.000500"
        );
    }

    #[test]
    fn numbers_and_bytes() {
        let t = ColumnType::MYSQL_TYPE_LONGLONG;
        assert_eq!(value_json(Value::Int(-3), t), serde_json::json!(-3));
        assert_eq!(value_json(Value::UInt(u64::MAX), t), "18446744073709551615");
        assert_eq!(value_json(Value::NULL, t), Json::Null);
        assert_eq!(
            value_json(Value::Bytes(vec![0xff, 0x00]), ColumnType::MYSQL_TYPE_BLOB),
            "0xFF00"
        );
        assert_eq!(type_name(ColumnType::MYSQL_TYPE_VAR_STRING), "var_string");
    }

    #[test]
    fn binary_values_read_as_the_text_protocol_writes_them() {
        let col = |t, d| Column::new(t).with_decimals(d);
        let dt = ColumnType::MYSQL_TYPE_DATETIME;
        assert_eq!(
            cell(&col(dt, 0), Value::Date(2024, 3, 9, 13, 5, 7, 0)),
            "2024-03-09 13:05:07"
        );
        assert_eq!(
            cell(&col(dt, 3), Value::Date(2024, 3, 9, 13, 5, 7, 120_000)),
            "2024-03-09 13:05:07.120"
        );
        assert_eq!(
            cell(&col(dt, 6), Value::Date(2024, 3, 9, 0, 0, 0, 0)),
            "2024-03-09 00:00:00.000000"
        );
        assert_eq!(
            cell(
                &col(ColumnType::MYSQL_TYPE_DATE, 0),
                Value::Date(2024, 3, 9, 0, 0, 0, 0)
            ),
            "2024-03-09"
        );
        assert_eq!(
            cell(
                &col(ColumnType::MYSQL_TYPE_TIME, 2),
                Value::Time(true, 1, 2, 3, 4, 500_000)
            ),
            "-26:03:04.50"
        );
        assert_eq!(
            cell(&col(ColumnType::MYSQL_TYPE_FLOAT, 31), Value::Float(1.1)),
            serde_json::json!(1.1)
        );
        assert_eq!(
            cell(&col(ColumnType::MYSQL_TYPE_LONGLONG, 0), Value::Int(-3)),
            serde_json::json!(-3)
        );
    }

    #[test]
    fn mysql_vectors_read_whole() {
        let col = Column::new(ColumnType::MYSQL_TYPE_VECTOR)
            .with_column_length(12)
            .with_org_table(b"t")
            .with_character_set(BINARY_CHARSET);
        let b = dbine_driver::vector::to_f32_le(&[1.0, 2.5, -3.0]);
        assert_eq!(cell(&col, Value::Bytes(b)), "[1,2.5,-3]");
        let rc = result_column(&col);
        assert_eq!(rc.type_name, "vector(3)");
        assert_eq!(
            rc.vector,
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        // An expression's vector: no table, no known dimension.
        let expr = Column::new(ColumnType::MYSQL_TYPE_VECTOR).with_column_length(65532);
        assert_eq!(result_column(&expr).type_name, "vector");
        // Not a multiple of 4: shown as bytes.
        assert_eq!(cell(&col, Value::Bytes(vec![1, 2, 3])), "0x010203");
        // A catalog-known vector column in text (TiDB) or bytes (MariaDB).
        let m = VectorMeta::dense(Some(2), VectorElement::F32);
        let text = Column::new(ColumnType::MYSQL_TYPE_LONG_BLOB).with_character_set(46);
        assert_eq!(
            vector_cell(&m, &text, Value::Bytes(b"[1, 2]".to_vec())),
            "[1,2]"
        );
        let bin = Column::new(ColumnType::MYSQL_TYPE_VAR_STRING).with_character_set(BINARY_CHARSET);
        let b = dbine_driver::vector::to_f32_le(&[0.5, 4.0]);
        assert_eq!(vector_cell(&m, &bin, Value::Bytes(b)), "[0.5,4]");
        assert_eq!(vector_cell(&m, &bin, Value::NULL), Json::Null);
    }

    #[test]
    fn catalog_text() {
        assert_eq!(value_text(&Value::NULL), None);
        assert_eq!(
            value_text(&Value::Bytes(b"abc".to_vec())).as_deref(),
            Some("abc")
        );
        assert_eq!(value_text(&Value::Int(7)).as_deref(), Some("7"));
    }
}
