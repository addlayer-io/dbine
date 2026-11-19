//! Arrow values as the UI's JSON cells.

use arrow_array::cast::AsArray;
use arrow_array::types::*;
use arrow_array::{Array, RecordBatch, UnionArray};
use arrow_schema::DataType;
use dbine_driver::{json_bytes, json_f64, json_i64, json_u64, VectorElement, VectorMeta};
use serde_json::Value;

/// A vector column: a `FixedSizeList` of floats, the usual Arrow form of an
/// embedding (DuckDB's `FLOAT[3]`, Arrow-native vector stores), with its
/// length as the dimension.
pub fn vector_meta(t: &DataType) -> Option<VectorMeta> {
    let DataType::FixedSizeList(f, n) = t else {
        return None;
    };
    let element = match f.data_type() {
        DataType::Float32 => VectorElement::F32,
        DataType::Float64 => VectorElement::F64,
        DataType::Float16 => VectorElement::F16,
        _ => return None,
    };
    Some(VectorMeta::dense(u32::try_from(*n).ok(), element))
}

/// A list of floats (fixed or not) as the vector text (`[0.1,2]`), each
/// number at its own precision; `None` when it isn't one or holds a null.
pub fn float_list_text(a: &dyn Array, row: usize) -> Option<String> {
    let values = match a.data_type() {
        DataType::FixedSizeList(..) => a.as_fixed_size_list().value(row),
        DataType::List(_) => a.as_list::<i32>().value(row),
        DataType::LargeList(_) => a.as_list::<i64>().value(row),
        _ => return None,
    };
    if values.null_count() > 0 {
        return None;
    }
    Some(match values.data_type() {
        DataType::Float32 => dbine_driver::vector::text(
            values
                .as_primitive::<Float32Type>()
                .values()
                .iter()
                .copied(),
        ),
        DataType::Float64 => dbine_driver::vector::text(
            values
                .as_primitive::<Float64Type>()
                .values()
                .iter()
                .copied(),
        ),
        DataType::Float16 => dbine_driver::vector::text(
            values
                .as_primitive::<Float16Type>()
                .values()
                .iter()
                .map(|h| h.to_f32()),
        ),
        _ => return None,
    })
}

fn display(a: &dyn Array, row: usize) -> Value {
    arrow_cast::display::array_value_to_string(a, row)
        .map(Value::String)
        .unwrap_or(Value::Null)
}

/// One cell: integers checked for JS precision, binaries as `0x…`, dates
/// and timestamps as ISO text (`2024-01-31 13:45:00`), decimals as their
/// Arrow text, and structs, lists and maps as JSON text with each struct
/// field under its name (see `nested`).
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
        DataType::Binary
        | DataType::LargeBinary
        | DataType::BinaryView
        | DataType::FixedSizeBinary(_) => binary_at(a, row)
            .map(|b| json_bytes(&b))
            .unwrap_or(Value::Null),
        DataType::Timestamp(..) => match display(a, row) {
            Value::String(s) => Value::String(s.replacen('T', " ", 1)),
            v => v,
        },
        DataType::List(_) | DataType::LargeList(_) | DataType::FixedSizeList(..)
            if float_list_text(a, row).is_some() =>
        {
            Value::String(float_list_text(a, row).unwrap_or_default())
        }
        DataType::Struct(_)
        | DataType::List(_)
        | DataType::LargeList(_)
        | DataType::FixedSizeList(..)
        | DataType::Map(..) => Value::String(nested(a, row).to_string()),
        _ => display(a, row),
    }
}

/// A value as JSON, nested ones included: a struct as an object with its
/// field names, a list as an array, a map as an object keyed by the keys'
/// text; the scalars inside as `cell` shows them.
fn nested(a: &dyn Array, row: usize) -> Value {
    if a.is_null(row) {
        return Value::Null;
    }
    let items = |v: &dyn Array| Value::Array((0..v.len()).map(|i| nested(v, i)).collect());
    match a.data_type() {
        DataType::Struct(fields) => Value::Object(
            fields
                .iter()
                .zip(a.as_struct().columns())
                .map(|(f, c)| (f.name().clone(), nested(c.as_ref(), row)))
                .collect(),
        ),
        DataType::List(_) => items(a.as_list::<i32>().value(row).as_ref()),
        DataType::LargeList(_) => items(a.as_list::<i64>().value(row).as_ref()),
        DataType::FixedSizeList(..) => items(a.as_fixed_size_list().value(row).as_ref()),
        DataType::Map(..) => {
            let e = a.as_map().value(row);
            let (k, v) = (e.column(0), e.column(1));
            Value::Object(
                (0..e.len())
                    .map(|i| {
                        let key = match nested(k.as_ref(), i) {
                            Value::String(s) => s,
                            other => other.to_string(),
                        };
                        (key, nested(v.as_ref(), i))
                    })
                    .collect(),
            )
        }
        _ => cell(a, row),
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
    let (Some(ids), Some(values)) = (b.column_by_name("info_name"), b.column_by_name("value"))
    else {
        return Vec::new();
    };
    let Some(u) = values.as_any().downcast_ref::<UnionArray>() else {
        return Vec::new();
    };
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
    use arrow_array::{
        ArrayRef, BinaryArray, Int64Array, StringArray, TimestampMillisecondArray, UInt64Array,
    };
    use serde_json::json;
    use std::sync::Arc;

    #[test]
    fn cells() {
        let big: ArrayRef = Arc::new(Int64Array::from(vec![
            Some(9007199254740993),
            None,
            Some(1),
        ]));
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

    #[test]
    fn float_fixed_lists_are_vectors() {
        use arrow_array::types::Float32Type;
        use arrow_array::{FixedSizeListArray, Float32Array, ListArray};
        use arrow_schema::Field;
        let item = Arc::new(Field::new("item", DataType::Float32, true));
        let v = FixedSizeListArray::try_new(
            item.clone(),
            3,
            Arc::new(Float32Array::from(vec![0.1, 2.0, -3.5, 1.0, 0.0, 0.0])),
            None,
        )
        .unwrap();
        assert_eq!(
            vector_meta(v.data_type()),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        // FLOAT at its own precision (not 0.10000000149011612).
        assert_eq!(cell(&v, 0), json!("[0.1,2,-3.5]"));
        assert_eq!(cell(&v, 1), json!("[1,0,0]"));
        let ints = FixedSizeListArray::try_new(
            Arc::new(Field::new("item", DataType::Int32, true)),
            2,
            Arc::new(arrow_array::Int32Array::from(vec![1, 2])),
            None,
        )
        .unwrap();
        assert_eq!(vector_meta(ints.data_type()), None);
        assert_eq!(cell(&ints, 0), json!("[1,2]"));
        // A variable list of floats: the same text, not a vector column.
        let l = ListArray::from_iter_primitive::<Float32Type, _, _>(vec![
            Some(vec![Some(0.5), Some(1.5)]),
            Some(vec![Some(1.0), None]),
        ]);
        assert_eq!(vector_meta(l.data_type()), None);
        assert_eq!(cell(&l, 0), json!("[0.5,1.5]"));
        assert_eq!(cell(&l, 1), json!("[1.0,null]"));
    }

    #[test]
    fn structs_show_their_field_names() {
        use arrow_array::builder::{Int32Builder, ListBuilder, StringBuilder, StructBuilder};
        use arrow_array::{ListArray, StructArray};
        use arrow_schema::{Field, Fields};

        // ARRAY(STRUCT(product_name VARCHAR, quantity INTEGER))
        let item_fields = Fields::from(vec![
            Field::new("product_name", DataType::Utf8, true),
            Field::new("quantity", DataType::Int32, true),
        ]);
        let mut items = ListBuilder::new(StructBuilder::from_fields(item_fields.clone(), 1));
        let sb = items.values();
        sb.field_builder::<StringBuilder>(0)
            .unwrap()
            .append_value("demo-product");
        sb.field_builder::<Int32Builder>(1).unwrap().append_value(2);
        sb.append(true);
        items.append(true);
        items.append(true); // empty list
        let items: ListArray = items.finish();
        let big: ArrayRef = Arc::new(Int64Array::from(vec![9007199254740993, 1]));
        let note: ArrayRef = Arc::new(StringArray::from(vec![None, Some("")]));
        let row = StructArray::from(vec![
            (
                Arc::new(Field::new("customer_id", DataType::Int64, true)),
                big,
            ),
            (
                Arc::new(Field::new("items", items.data_type().clone(), true)),
                Arc::new(items) as ArrayRef,
            ),
            (Arc::new(Field::new("note", DataType::Utf8, true)), note),
        ]);
        let v: Value = serde_json::from_str(cell(&row, 0).as_str().unwrap()).unwrap();
        assert_eq!(
            v,
            json!({"customer_id": "9007199254740993",
                   "items": [{"product_name": "demo-product", "quantity": 2}], "note": null})
        );
        let v: Value = serde_json::from_str(cell(&row, 1).as_str().unwrap()).unwrap();
        assert_eq!(v, json!({"customer_id": 1, "items": [], "note": ""}));
    }
}
