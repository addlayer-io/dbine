//! Vector (embedding) columns as the app's paths see them (`dbine_driver::vector`):
//! which columns of a result or a table hold vectors, their cells as the
//! JSON arrays `insert_script` and `update_script` take, and their text as
//! compared.
//!
//! A result says it itself ([`dbine_driver::ResultColumn::vector`], set by
//! the driver). A table's describe says it through the column's native type
//! (`vector(3)`, `VECTOR(3, FLOAT32)`, `F32_BLOB(3)`…), classified by the
//! engine's `dbine-schema` dialect.

use dbine_driver::vector::{self, VectorElement};
use dbine_driver::{ResultColumn, VectorMeta};
use dbine_schema::dialect::Dialect;
use dbine_schema::LogicalType;
use serde_json::Value;

/// The vector a native type is on `driver_id`'s engine; `None` for any
/// other type, or an engine `dbine-schema` doesn't know.
pub(crate) fn of_type(driver_id: &str, native: &str) -> Option<VectorMeta> {
    of_type_in(dbine_schema::dialect::for_driver(driver_id)?, native)
}

/// [`of_type`] with the dialect given.
pub(crate) fn of_type_in(d: &dyn Dialect, native: &str) -> Option<VectorMeta> {
    if native.trim().is_empty() {
        return None;
    }
    let spec = dbine_schema::parse::parse(native);
    match dbine_schema::convert::logical_of(d, &spec) {
        LogicalType::Vector {
            dims,
            element,
            sparse,
        } => Some(VectorMeta {
            dims,
            element,
            sparse,
        }),
        _ => None,
    }
}

/// A result column's vector: the driver's mark, else what its type says on
/// `driver_id`'s engine.
pub(crate) fn of_column(driver_id: &str, c: &ResultColumn) -> Option<VectorMeta> {
    c.vector
        .clone()
        .or_else(|| of_type(driver_id, &c.type_name))
}

/// A vector value as `insert_script` and `update_script` take it: a JSON
/// array of numbers. Anything that isn't one (NULL, text that doesn't read
/// as a vector) stays as it is.
pub(crate) fn script_value(v: Value) -> Value {
    if matches!(v, Value::Array(_)) && vector::parse_value(&v).is_some() {
        return v;
    }
    match vector::parse_value(&v) {
        Some(nums) => Value::Array(
            nums.into_iter()
                .map(dbine_driver::model::json_f64)
                .collect(),
        ),
        None => v,
    }
}

/// `rows` with the cells of the dense vector columns (`vectors`, by
/// position) as [`script_value`]s.
pub(crate) fn script_rows(vectors: &[bool], rows: Vec<Vec<Value>>) -> Vec<Vec<Value>> {
    if !vectors.iter().any(|v| *v) {
        return rows;
    }
    rows.into_iter()
        .map(|r| {
            r.into_iter()
                .enumerate()
                .map(|(i, v)| {
                    if vectors.get(i).copied().unwrap_or(false) {
                        script_value(v)
                    } else {
                        v
                    }
                })
                .collect()
        })
        .collect()
}

/// A vector cell's text as compared: its numbers as the shortest text that
/// reads back as the same number of the column's precision (32-bit floats
/// unless the column says 64), so `[1e-07, 2]` and `[0.0000001,2.0]` are
/// the same vector. Exact: the stored value reads back from it. `None`
/// when the value isn't a vector.
pub(crate) fn compared_text(v: &Value, element: VectorElement) -> Option<String> {
    let nums = vector::parse_value(v)?;
    Some(match element {
        VectorElement::F64 | VectorElement::Other => vector::text(nums),
        _ => vector::text(nums.into_iter().map(|x| x as f32)),
    })
}

/// A dense vector column's cell as compared ([`compared_text`]), as text;
/// anything else as it came.
pub(crate) fn compared(v: Value, element: Option<VectorElement>) -> Value {
    match element {
        Some(e) if matches!(v, Value::String(_) | Value::Array(_)) => {
            compared_text(&v, e).map_or(v, Value::String)
        }
        _ => v,
    }
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use dbine_schema::dialect::{Caps, Rendered};
    use dbine_schema::parse::TypeSpec;
    use serde_json::json;

    /// PostgreSQL's dialect, with pgvector's `vector(n)` and `halfvec(n)`
    /// classified (what the PostgreSQL family's dialect does once it knows
    /// vectors): the app's paths tested without depending on it.
    pub(crate) struct WithVectors;

    fn pg() -> &'static dyn Dialect {
        dbine_schema::dialect::for_driver("postgres").expect("postgres dialect")
    }

    impl Dialect for WithVectors {
        fn id(&self) -> &'static str {
            "postgres"
        }
        fn parse_type(&self, t: &TypeSpec) -> LogicalType {
            let element = match t.name.as_str() {
                "vector" => VectorElement::F32,
                "halfvec" => VectorElement::F16,
                _ => return pg().parse_type(t),
            };
            LogicalType::Vector {
                dims: t.arg_u32(0),
                element,
                sparse: false,
            }
        }
        fn render_type(&self, t: &LogicalType) -> Rendered {
            pg().render_type(t)
        }
        fn render_default(
            &self,
            d: &dbine_schema::default::DefaultValue,
            ty: &LogicalType,
        ) -> Option<String> {
            pg().render_default(d, ty)
        }
        fn caps(&self) -> Caps {
            pg().caps()
        }
    }

    #[test]
    fn vector_types_are_classified_by_the_dialect() {
        assert_eq!(
            of_type_in(&WithVectors, "vector(3)"),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(
            of_type_in(&WithVectors, "halfvec"),
            Some(VectorMeta::dense(None, VectorElement::F16))
        );
        for t in ["", "text", "integer[]", "jsonb", "real[]"] {
            assert_eq!(of_type_in(&WithVectors, t), None, "{t}");
        }
        // The driver's mark comes first.
        let c = ResultColumn::new("v", "text")
            .with_vector(VectorMeta::dense(Some(2), VectorElement::F32));
        assert_eq!(of_column("postgres", &c).unwrap().dims, Some(2));
        assert_eq!(of_column("postgres", &ResultColumn::new("t", "text")), None);
        assert_eq!(of_type("no-such-driver", "vector(3)"), None);
    }

    #[test]
    fn script_values_are_json_arrays_of_numbers() {
        assert_eq!(
            script_value(json!("[1, 2.5,-3e-2]")),
            json!([1.0, 2.5, -0.03])
        );
        assert_eq!(script_value(json!([1, 2])), json!([1, 2]));
        // Not a vector: as it came.
        for v in [
            json!(null),
            json!(""),
            json!("{1:2}/3"),
            json!("[1,a]"),
            json!(4),
        ] {
            assert_eq!(script_value(v.clone()), v);
        }
        let rows = vec![vec![json!(1), json!("[1,2]"), json!("[3]")]];
        assert_eq!(
            script_rows(&[false, true], rows.clone()),
            vec![vec![json!(1), json!([1.0, 2.0]), json!("[3]")]]
        );
        assert_eq!(script_rows(&[], rows.clone()), rows);
    }

    #[test]
    fn vectors_compare_by_value() {
        let f32 = |s: &str| compared_text(&json!(s), VectorElement::F32);
        assert_eq!(f32("[1e-07, 2]"), f32("[0.0000001,2.0]"));
        assert_eq!(f32("[1e-07, 2]").as_deref(), Some("[0.0000001,2]"));
        // The digits a float32 can't tell apart.
        assert_eq!(f32("[0.1]"), f32("[0.100000001]"));
        assert_ne!(f32("[0.1]"), f32("[0.1001]"));
        assert_ne!(f32("[1,2]"), f32("[1,2,0]"));
        assert_eq!(
            compared_text(&json!([0.5, 1]), VectorElement::F32).as_deref(),
            Some("[0.5,1]")
        );
        // A 64-bit column keeps its precision.
        let f64 = |s: &str| compared_text(&json!(s), VectorElement::F64);
        assert_ne!(f64("[0.1]"), f64("[0.100000001]"));
        assert_eq!(f64("[1e-07]"), f64("[0.0000001]"));
        // Not a vector, or not a vector column: as it came.
        assert_eq!(compared(json!("x"), Some(VectorElement::F32)), json!("x"));
        assert_eq!(compared(json!("[1.0]"), None), json!("[1.0]"));
        assert_eq!(compared(json!(null), Some(VectorElement::F32)), json!(null));
        assert_eq!(
            compared(json!("[1.0, 2]"), Some(VectorElement::F32)),
            json!("[1,2]")
        );
    }
}
