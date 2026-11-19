//! Vectors in DuckDB: a fixed-size `FLOAT[n]` or `DOUBLE[n]` array is the
//! vector type (`array_cosine_similarity`, `array_distance` and the other
//! `array_*` distances take only arrays, and the vss extension's HNSW
//! indexes only `FLOAT[n]`). Lists (`FLOAT[]`) and arrays of other types
//! aren't vectors.

use dbine_driver::{VectorElement, VectorMeta};
use duckdb::types::Value;

/// Whether `t` (a type as DuckDB writes it: `FLOAT[3]`, `DOUBLE[768]`) is a
/// vector, and its numbers and dimensions.
pub fn type_meta(t: &str) -> Option<(VectorElement, u32)> {
    let t = t.trim();
    let open = t.strip_suffix(']')?.rfind('[')?;
    let dims: u32 = t[open + 1..t.len() - 1].trim().parse().ok()?;
    let element = match t[..open].trim().to_ascii_uppercase().as_str() {
        "FLOAT" | "REAL" | "FLOAT4" => VectorElement::F32,
        "DOUBLE" | "FLOAT8" => VectorElement::F64,
        _ => return None,
    };
    Some((element, dims))
}

/// The column metadata for a vector type ([`type_meta`]).
pub fn meta(t: &str) -> Option<VectorMeta> {
    type_meta(t).map(|(e, d)| VectorMeta::dense(Some(d), e))
}

/// A result column's type, from the result's Arrow schema: `FLOAT[3]` and
/// `DOUBLE[3]` (a fixed-size list of floats), else `None`.
pub fn result_type(dt: &duckdb::arrow::datatypes::DataType) -> Option<String> {
    use duckdb::arrow::datatypes::DataType as D;
    match dt {
        D::FixedSizeList(f, n) => match f.data_type() {
            D::Float32 => Some(format!("FLOAT[{n}]")),
            D::Float64 => Some(format!("DOUBLE[{n}]")),
            _ => None,
        },
        _ => None,
    }
}

/// A vector cell as DBine's vector text (`[0.1,2]`: each number the
/// shortest way its own type reads back, not widened to f64); `None` when
/// it isn't a whole array of floats (a NULL element, another value).
pub fn cell(v: &Value) -> Option<serde_json::Value> {
    let Value::Array(items) = v else {
        return None;
    };
    if items.iter().all(|x| matches!(x, Value::Float(_))) {
        return Some(dbine_driver::vector::json(items.iter().map(|x| match x {
            Value::Float(f) => *f,
            _ => 0.0,
        })));
    }
    if items.iter().all(|x| matches!(x, Value::Double(_))) {
        return Some(dbine_driver::vector::json(items.iter().map(|x| match x {
            Value::Double(f) => *f,
            _ => 0.0,
        })));
    }
    None
}

/// A vector value of an edit or insert script (a JSON array of numbers):
/// `'[1,2.5]'`, which DuckDB casts to the column's `FLOAT[n]` on INSERT
/// and UPDATE.
pub fn literal(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Array(_) => {
            dbine_driver::vector::value_text(v).map(|t| format!("'{t}'"))
        }
        _ => None,
    }
}

/// The text DuckDB writes for a `FLOAT[n]` / `DOUBLE[n]` (`[0.1, 2.0]`) as
/// DBine's vector text, in the column's precision; `None` when it isn't
/// one (`nan` and `inf` aren't JSON).
pub fn text(raw: &str, element: VectorElement) -> Option<String> {
    let n = dbine_driver::vector::parse(raw)?;
    Some(match element {
        VectorElement::F32 => dbine_driver::vector::text(n.into_iter().map(|x| x as f32)),
        _ => dbine_driver::vector::text(n),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn vector_types() {
        assert_eq!(type_meta("FLOAT[3]"), Some((VectorElement::F32, 3)));
        assert_eq!(type_meta("double[768]"), Some((VectorElement::F64, 768)));
        assert_eq!(type_meta("REAL[ 2 ]"), Some((VectorElement::F32, 2)));
        for no in [
            "FLOAT[]",
            "INTEGER[3]",
            "FLOAT[3][2]x",
            "FLOAT",
            "VARCHAR[2]",
        ] {
            assert_eq!(type_meta(no), None, "{no}");
        }
        assert_eq!(
            meta("FLOAT[2]"),
            Some(VectorMeta::dense(Some(2), VectorElement::F32))
        );
    }

    #[test]
    fn cells_and_literals() {
        let v = Value::Array(vec![
            Value::Float(0.1),
            Value::Float(2.0),
            Value::Float(-3.5),
        ]);
        assert_eq!(cell(&v), Some(json!("[0.1,2,-3.5]")));
        let d = Value::Array(vec![Value::Double(1e-7), Value::Double(2.0)]);
        assert_eq!(cell(&d), Some(json!("[0.0000001,2]")));
        assert_eq!(
            cell(&Value::Array(vec![Value::Float(1.0), Value::Null])),
            None
        );
        assert_eq!(cell(&Value::Int(1)), None);
        assert_eq!(literal(&json!([1.0, 2.5])).as_deref(), Some("'[1,2.5]'"));
        assert_eq!(literal(&json!("[1]")), None);
        assert_eq!(
            text("[0.1, 2.0, 3.5]", VectorElement::F32).as_deref(),
            Some("[0.1,2,3.5]")
        );
        assert_eq!(text("[nan, 1.0]", VectorElement::F32), None);
    }
}
