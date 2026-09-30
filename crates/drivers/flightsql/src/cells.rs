//! Arrow values as the UI's JSON cells.

use arrow_array::cast::AsArray;
use arrow_array::types::*;
use arrow_array::{Array, RecordBatch, UnionArray};
use arrow_schema::DataType;
use dbine_driver::{json_bytes, json_f64, json_i64, json_u64};
use serde_json::Value;

fn display(a: &dyn Array, row: usize) -> Value {
    arrow_cast::display::array_value_to_string(a, row).map(Value::String).unwrap_or(Value::Null)
}

/// One cell: integers checked for JS precision, binaries as `0x…`, dates
/// and timestamps as ISO text (`2024-01-31 13:45:00`), decimals and nested
/// values as their Arrow text.
pub fn cell(a: &dyn Array, row: usize) -> Value {
    if a.is_null(row) {
        return Value::Null;
    }
    match a.data_type() {
        DataType::Null => Value::Null,
        DataType::Boolean => Value::Bool(a.as_boolean().value(row)),
        DataType::Int8 => json_i64(a.as_primitive::<Int8Type>().value(row) as i64),
        DataType::Int16 => json_i64(a.as_primitive::<Int16Type>().value(row) as i64),
        DataType::Int32 => json_i64(a.as_primitive::<Int32Type>().value(row) as i64),
        DataType::Int64 => json_i64(a.as_primitive::<Int64Type>().value(row)),
        DataType::UInt8 => json_u64(a.as_primitive::<UInt8Type>().value(row) as u64),
        DataType::UInt16 => json_u64(a.as_primitive::<UInt16Type>().value(row) as u64),
        DataType::UInt32 => json_u64(a.as_primitive::<UInt32Type>().value(row) as u64),
        DataType::UInt64 => json_u64(a.as_primitive::<UInt64Type>().value(row)),
        DataType::Float32 => json_f64(a.as_primitive::<Float32Type>().value(row) as f64),
        DataType::Float64 => json_f64(a.as_primitive::<Float64Type>().value(row)),
        DataType::Utf8 => Value::String(a.as_string::<i32>().value(row).to_string()),
        DataType::LargeUtf8 => Value::String(a.as_string::<i64>().value(row).to_string()),
        DataType::Utf8View => Value::String(a.as_string_view().value(row).to_string()),
        DataType::Binary | DataType::LargeBinary | DataType::BinaryView | DataType::FixedSizeBinary(_) => {
            binary_at(a, row).map(|b| json_bytes(&b)).unwrap_or(Value::Null)
        }
        DataType::Timestamp(..) => match display(a, row) {
            Value::String(s) => Value::String(s.replacen('T', " ", 1)),
            v => v,
        },
        _ => display(a, row),
    }
}

pub fn binary_at(a: &dyn Array, row: usize) -> Option<Vec<u8>> {
    if a.is_null(row) {
        return None;
    }
    Some(match a.data_type() {
        DataType::Binary => a.as_binary::<i32>().value(row).to_vec(),
        DataType::LargeBinary => a.as_binary::<i64>().value(row).to_vec(),
        DataType::BinaryView => a.as_binary_view().value(row).to_vec(),
        DataType::FixedSizeBinary(_) => a.as_fixed_size_binary().value(row).to_vec(),
        _ => return None,
    })
}

/// `GetSqlInfo` rows: (info id, value of the dense union).
pub fn sql_info_rows(b: &RecordBatch) -> Vec<(u32, Value)> {
    let (Some(ids), Some(values)) = (b.column_by_name("info_name"), b.column_by_name("value")) else { return Vec::new() };
    let Some(u) = values.as_any().downcast_ref::<UnionArray>() else { return Vec::new() };
    let ids = ids.as_primitive::<UInt32Type>();
    (0..b.num_rows())
        .map(|r| {
            let child = u.child(u.type_id(r));
            (ids.value(r), cell(child.as_ref(), u.value_offset(r)))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use arrow_array::{ArrayRef, BinaryArray, Int64Array, StringArray, TimestampMillisecondArray, UInt64Array};
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn cells() {
        let big: ArrayRef = Arc::new(Int64Array::from(vec![Some(9007199254740993), None, Some(1)]));
        assert_eq!(cell(big.as_ref(), 0), json!("9007199254740993"));
        assert_eq!(cell(big.as_ref(), 1), Value::Null);
        assert_eq!(cell(big.as_ref(), 2), json!(1));
        let u: ArrayRef = Arc::new(UInt64Array::from(vec![u64::MAX]));
        assert_eq!(cell(u.as_ref(), 0), json!("18446744073709551615"));
        let s: ArrayRef = Arc::new(StringArray::from(vec!["ñ"]));
        assert_eq!(cell(s.as_ref(), 0), json!("ñ"));
        let b: ArrayRef = Arc::new(BinaryArray::from(vec![&b"\xca\xfe"[..]]));
        assert_eq!(cell(b.as_ref(), 0), json!("0xCAFE"));
        let t: ArrayRef = Arc::new(TimestampMillisecondArray::from(vec![1706708700123]));
        assert_eq!(cell(t.as_ref(), 0), json!("2024-01-31 13:45:00.123"));
    }
}
