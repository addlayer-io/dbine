//! Db2 LUW's `VECTOR(n, FLOAT32 | INT8)` (Db2 12.1.2 and later) through
//! ODBC / CLI.
//!
//! - **Values:** Db2 hands vectors to applications in their string form, a
//!   JSON-like array; cells and transfer reads are that text in the
//!   canonical form ([`dbine_driver::vector`]).
//! - **Types:** a column whose type name starts with `VECTOR` (`VECTOR`,
//!   `VECTOR(FLOAT32)` as `DESCRIBE TABLE` spells it, `VECTOR(3, INT8)`)
//!   is a vector; the catalog's length is its dimension.
//! - **Literals and loads:** Db2 builds a vector from a string with
//!   `VECTOR(string, dimension, coordinate type)` (no implicit cast is
//!   documented): edits write `VECTOR('[…]', n, FLOAT32)` with the array's
//!   length, and loads bind each vector column as `VECTOR(?, n, type)` from
//!   the target column's type.
//!
//! An untyped edit can't tell a FLOAT32 column from an INT8 one, so it
//! writes FLOAT32, the common one (Db2 refuses it for an INT8 column);
//! scripts that know the columns' types ([`typed_literal`]) and transfers
//! (which read the target's types) write either.

use dbine_driver::vector::{self, VectorElement, VectorMeta};

/// Whether a type name is a vector's.
pub fn is_vector(type_name: &str) -> bool {
    type_name
        .trim()
        .get(..6)
        .is_some_and(|t| t.eq_ignore_ascii_case("VECTOR"))
}

/// A vector type's dimension and coordinate type, from its name and (when
/// the name doesn't say) the catalog's size. `None` when it isn't one.
fn shape(type_name: &str, size: Option<u64>) -> Option<(Option<u64>, &'static str)> {
    if !is_vector(type_name) {
        return None;
    }
    let inner = type_name.trim()[6..]
        .trim()
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or("");
    let mut dims = None;
    let mut coord = "FLOAT32";
    for part in inner.split(',').map(|p| p.trim().to_ascii_uppercase()) {
        match part.as_str() {
            "" => {}
            "INT8" => coord = "INT8",
            "FLOAT32" | "REAL" => coord = "FLOAT32",
            n => dims = n.parse::<u64>().ok().or(dims),
        }
    }
    Some((dims.or(size.filter(|&n| n > 0)), coord))
}

/// The declared type, `VECTOR(n, FLOAT32)`; `None` when it isn't a vector.
/// Without the dimension, the name as it came.
pub fn declared(type_name: &str, size: Option<u64>) -> Option<String> {
    let (dims, coord) = shape(type_name, size)?;
    Some(match dims {
        Some(n) => format!("VECTOR({n}, {coord})"),
        None => type_name.trim().to_string(),
    })
}

/// What a result column of this type says; `None` when it isn't a vector.
pub fn meta(type_name: &str) -> Option<VectorMeta> {
    let (dims, coord) = shape(type_name, None)?;
    let element = if coord == "INT8" {
        VectorElement::I8
    } else {
        VectorElement::F32
    };
    Some(VectorMeta::dense(
        dims.and_then(|n| u32::try_from(n).ok()),
        element,
    ))
}

/// Db2's text of a vector in the canonical form; as it came when it
/// doesn't read as one.
pub fn canonical(text: String) -> String {
    match vector::parse(&text) {
        // FLOAT32 or INT8 coordinates: the shortest float32 of each.
        Some(v) => vector::text(v.into_iter().map(|x| x as f32)),
        None => text,
    }
}

/// A vector edit's value as Db2's literal, `VECTOR('[1,2]', 2, FLOAT32)`;
/// `None` when `v` isn't a vector.
pub fn literal(v: &serde_json::Value) -> Option<String> {
    let values = vector::parse_value(v)?;
    if values.is_empty() {
        return None;
    }
    Some(format!(
        "VECTOR('{}', {}, FLOAT32)",
        vector::text(values.iter().copied()),
        values.len()
    ))
}

/// A vector value for a column of a known vector type (`type_name` as a
/// result or a describe gives it, `meta` its vector meta), from its numbers
/// or the grid's text: `VECTOR('[…]', n, FLOAT32 | INT8)` with the
/// column's coordinate type and dimension (the value's length when the
/// type doesn't say). `None` when the value isn't a vector.
pub fn typed_literal(
    type_name: &str,
    meta: Option<&VectorMeta>,
    v: &serde_json::Value,
) -> Option<String> {
    let values = vector::parse_value(v)?;
    if values.is_empty() {
        return None;
    }
    let up = type_name.to_ascii_uppercase();
    let coord = if up.contains("INT8") {
        "INT8"
    } else if up.contains("FLOAT32") || up.contains("REAL") {
        "FLOAT32"
    } else if meta.is_some_and(|m| m.element == VectorElement::I8) {
        "INT8"
    } else {
        "FLOAT32"
    };
    let dims = shape(type_name, None)
        .and_then(|(d, _)| d)
        .or_else(|| meta.and_then(|m| m.dims).map(u64::from))
        .unwrap_or(values.len() as u64);
    Some(format!(
        "VECTOR('{}', {dims}, {coord})",
        vector::text(values.iter().copied())
    ))
}

/// The VALUES item that loads a vector column of this type from a text
/// parameter, `VECTOR(?, n, FLOAT32)`; `None` when it isn't a vector or
/// its dimension isn't known (the parameter then goes as it is).
pub fn param(type_name: &str, size: Option<u64>) -> Option<String> {
    let (dims, coord) = shape(type_name, size)?;
    Some(format!("VECTOR(?, {}, {coord})", dims?))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn types_as_db2_spells_them() {
        assert!(is_vector("VECTOR"));
        assert!(is_vector("vector(3, int8)"));
        assert!(!is_vector("VARCHAR"));
        assert!(!is_vector("VECT"));
        assert_eq!(
            declared("VECTOR", Some(1536)).as_deref(),
            Some("VECTOR(1536, FLOAT32)")
        );
        assert_eq!(
            declared("VECTOR(INT8)", Some(300)).as_deref(),
            Some("VECTOR(300, INT8)")
        );
        assert_eq!(
            declared("VECTOR(REAL)", Some(3)).as_deref(),
            Some("VECTOR(3, FLOAT32)")
        );
        assert_eq!(
            declared("VECTOR(3, INT8)", None).as_deref(),
            Some("VECTOR(3, INT8)")
        );
        assert_eq!(declared("VECTOR", None).as_deref(), Some("VECTOR"));
        assert_eq!(declared("INTEGER", Some(4)), None);
        assert_eq!(
            meta("VECTOR(3, FLOAT32)"),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(
            meta("VECTOR(INT8)"),
            Some(VectorMeta::dense(None, VectorElement::I8))
        );
        assert_eq!(meta("CLOB"), None);
    }

    #[test]
    fn values_literals_and_loads() {
        assert_eq!(canonical("[1.0E0, 2.5E0, -3]".into()), "[1,2.5,-3]");
        assert_eq!(canonical("[1.0000000149011612E-1]".into()), "[0.1]");
        assert_eq!(canonical("no".into()), "no");
        assert_eq!(
            literal(&json!([0.5, 2.0, -1.0])).as_deref(),
            Some("VECTOR('[0.5,2,-1]', 3, FLOAT32)")
        );
        assert_eq!(literal(&json!([])), None);
        assert_eq!(literal(&json!("x")), None);
        assert_eq!(
            param("VECTOR", Some(3)).as_deref(),
            Some("VECTOR(?, 3, FLOAT32)")
        );
        assert_eq!(
            param("VECTOR(INT8)", Some(4)).as_deref(),
            Some("VECTOR(?, 4, INT8)")
        );
        assert_eq!(param("VECTOR", None), None);
        assert_eq!(param("INTEGER", Some(4)), None);
    }

    #[test]
    fn typed_literals_take_the_columns_coordinate_type() {
        // From a describe's type; from a result's meta; from the grid's text.
        assert_eq!(
            typed_literal("VECTOR(3, INT8)", None, &json!([1, -2, 3])).as_deref(),
            Some("VECTOR('[1,-2,3]', 3, INT8)")
        );
        let int8 = VectorMeta::dense(Some(2), VectorElement::I8);
        assert_eq!(
            typed_literal("VECTOR", Some(&int8), &json!("[4,5]")).as_deref(),
            Some("VECTOR('[4,5]', 2, INT8)")
        );
        assert_eq!(
            typed_literal("VECTOR(FLOAT32)", None, &json!([0.5, 1])).as_deref(),
            Some("VECTOR('[0.5,1]', 2, FLOAT32)")
        );
        assert_eq!(typed_literal("VECTOR(3, INT8)", None, &json!("x")), None);
        assert_eq!(typed_literal("VECTOR(3, INT8)", None, &json!([])), None);
    }
}
