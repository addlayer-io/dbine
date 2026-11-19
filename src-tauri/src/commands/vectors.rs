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

/// How a compared column's vectors compare: as float32 numbers, or (both
/// sides keep wider numbers) as float64 ones.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Precision {
    F32,
    F64,
}

/// Whether `e`'s numbers are wider than a float32 holds: 64-bit floats,
/// and the elements this build doesn't know (32-bit integers among them),
/// which compare as float64 so none of their digits is lost.
fn wide(e: VectorElement) -> bool {
    matches!(e, VectorElement::F64 | VectorElement::Other)
}

/// How a compared column pair's values compare when either side holds
/// dense vectors (`l` and `r`: each side's element type, `None` where that
/// side's column isn't a vector column); `None`: neither is. At the lower
/// precision of the two: float32 unless both keep wider numbers, so a
/// float64 vector equals the float32 one a sync writes from it. A side
/// that isn't a vector column (text, JSON, a schemaless engine's arrays)
/// compares at the other's.
pub(crate) fn pair_precision(
    l: Option<VectorElement>,
    r: Option<VectorElement>,
) -> Option<Precision> {
    let p = |wide: bool| {
        if wide {
            Precision::F64
        } else {
            Precision::F32
        }
    };
    match (l, r) {
        (None, None) => None,
        (Some(a), Some(b)) => Some(p(wide(a) && wide(b))),
        (Some(a), None) | (None, Some(a)) => Some(p(wide(a))),
    }
}

/// A vector cell's text as compared: its numbers as the shortest text that
/// reads back as the same number at `precision`, so `[1e-07, 2]` and
/// `[0.0000001,2.0]` are the same vector, and so are a float64's
/// `0.10000000149011612` and a float32's `0.1`. `None` when the value
/// isn't a vector.
pub(crate) fn compared_text(v: &Value, precision: Precision) -> Option<String> {
    let nums = vector::parse_value(v)?;
    Some(match precision {
        Precision::F64 => vector::text(nums),
        Precision::F32 => vector::text(nums.into_iter().map(|x| x as f32)),
    })
}

/// A cell of a compared column that holds vectors on either side, as
/// compared ([`compared_text`]), as text: a value that reads as a dense
/// vector (text or a JSON array of numbers); anything else as it came.
pub(crate) fn compared(v: Value, precision: Option<Precision>) -> Value {
    match precision {
        Some(p) if matches!(v, Value::String(_) | Value::Array(_)) => {
            compared_text(&v, p).map_or(v, Value::String)
        }
        _ => v,
    }
}

/// Whether a native type reads as a vector type on some engine by its
/// spelling alone, for code with no engine at hand (a notebook's warnings,
/// a subset's masks): `vector(3)`, `VECTOR(768, FLOAT32)`,
/// `VECTOR<FLOAT>(3)`, `vector<float, 3>`, `halfvec(3)`, `sparsevec(5)`,
/// `float_vector(3)`, `F32_BLOB(3)`, `REAL_VECTOR(3)`, `dense_vector`,
/// `knn_vector`, `vecf32(3)`, DuckDB's `FLOAT[3]`.
pub(crate) fn looks_like_vector_type(native: &str) -> bool {
    let t = native.trim().to_ascii_lowercase();
    let base: String = t
        .chars()
        .take_while(|c| c.is_ascii_alphanumeric() || *c == '_')
        .collect();
    let rest = t[base.len()..].trim_start();
    if matches!(
        base.as_str(),
        "vector"
            | "halfvec"
            | "sparsevec"
            | "float_vector"
            | "real_vector"
            | "half_vector"
            | "dense_vector"
            | "sparse_vector"
            | "knn_vector"
            | "vecf32"
            | "vecf64"
            | "f32_blob"
            | "f64_blob"
            | "f16_blob"
            | "fb16_blob"
            | "f8_blob"
            | "f1bit_blob"
    ) {
        return true;
    }
    // A fixed-size array of floats (DuckDB's ARRAY): `FLOAT[3]`.
    matches!(
        base.as_str(),
        "float" | "real" | "double" | "float4" | "float8"
    ) && rest
        .strip_prefix('[')
        .and_then(|r| r.strip_suffix(']'))
        .is_some_and(|n| !n.is_empty() && n.chars().all(|c| c.is_ascii_digit()))
}

/// The text a sparse vector column takes, by its type: pgvector's
/// `sparsevec` (`{1:0.5,3:2}/5`, positions from 1) or Oracle's
/// `VECTOR(n, FLOAT32, SPARSE)` (`[5,[1,3],[0.5,2]]`).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SparseText {
    Pg,
    Oracle,
}

impl SparseText {
    /// The sparse text of a column of type `native`; `None` for a type
    /// that isn't one of these.
    pub(crate) fn of_type(native: &str) -> Option<SparseText> {
        let t = native.trim().to_ascii_lowercase();
        if t.starts_with("sparsevec") {
            Some(SparseText::Pg)
        } else if t.starts_with("vector") && t.contains("sparse") {
            Some(SparseText::Oracle)
        } else {
            None
        }
    }
}

/// A sparse vector: its dimensions and its non-zero positions (as the
/// engine's text numbers them) with their values.
#[derive(Clone, Debug, PartialEq)]
pub(crate) struct Sparse {
    pub dims: u64,
    pub entries: Vec<(u64, f64)>,
}

/// One number as a vector's text writes it (shortest, `null` if not finite).
fn number(x: f64) -> String {
    vector::text([x]).trim_matches(['[', ']']).to_string()
}

impl Sparse {
    /// `text` in the form `f`; `None` when it isn't one.
    pub(crate) fn parse(text: &str, f: SparseText) -> Option<Sparse> {
        let t = text.trim();
        match f {
            SparseText::Pg => {
                let (body, dims) = t.rsplit_once('/')?;
                let dims = dims.trim().parse().ok()?;
                let inner = body.trim().strip_prefix('{')?.strip_suffix('}')?.trim();
                let entries = if inner.is_empty() {
                    Vec::new()
                } else {
                    inner
                        .split(',')
                        .map(|e| {
                            let (i, x) = e.split_once(':')?;
                            let x: f64 = x.trim().parse().ok()?;
                            x.is_finite().then_some(())?;
                            Some((i.trim().parse().ok()?, x))
                        })
                        .collect::<Option<Vec<_>>>()?
                };
                Some(Sparse { dims, entries })
            }
            SparseText::Oracle => {
                let v: Value = serde_json::from_str(t).ok()?;
                let [dims, at, values] = v.as_array()?.as_slice() else {
                    return None;
                };
                let at = at.as_array()?;
                let values = values.as_array()?;
                if at.len() != values.len() {
                    return None;
                }
                let entries = at
                    .iter()
                    .zip(values)
                    .map(|(i, x)| Some((i.as_u64()?, x.as_f64().filter(|x| x.is_finite())?)))
                    .collect::<Option<Vec<_>>>()?;
                Some(Sparse {
                    dims: dims.as_u64()?,
                    entries,
                })
            }
        }
    }

    /// Its text in the form `f`.
    pub(crate) fn text(&self, f: SparseText) -> String {
        match f {
            SparseText::Pg => {
                let entries: Vec<String> = self
                    .entries
                    .iter()
                    .map(|(i, x)| format!("{i}:{}", number(*x)))
                    .collect();
                format!("{{{}}}/{}", entries.join(","), self.dims)
            }
            SparseText::Oracle => {
                let at: Vec<String> = self.entries.iter().map(|(i, _)| i.to_string()).collect();
                let values: Vec<String> = self.entries.iter().map(|(_, x)| number(*x)).collect();
                format!("[{},[{}],[{}]]", self.dims, at.join(","), values.join(","))
            }
        }
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
        let f32 = |s: &str| compared_text(&json!(s), Precision::F32);
        assert_eq!(f32("[1e-07, 2]"), f32("[0.0000001,2.0]"));
        assert_eq!(f32("[1e-07, 2]").as_deref(), Some("[0.0000001,2]"));
        // The digits a float32 can't tell apart.
        assert_eq!(f32("[0.1]"), f32("[0.100000001]"));
        assert_ne!(f32("[0.1]"), f32("[0.1001]"));
        assert_ne!(f32("[1,2]"), f32("[1,2,0]"));
        assert_eq!(
            compared_text(&json!([0.5, 1]), Precision::F32).as_deref(),
            Some("[0.5,1]")
        );
        // Two 64-bit columns keep their precision.
        let f64 = |s: &str| compared_text(&json!(s), Precision::F64);
        assert_ne!(f64("[0.1]"), f64("[0.100000001]"));
        assert_eq!(f64("[1e-07]"), f64("[0.0000001]"));
        // Not a vector, or not a vector column: as it came.
        let p = Some(Precision::F32);
        assert_eq!(compared(json!("x"), p), json!("x"));
        assert_eq!(compared(json!("[1.0]"), None), json!("[1.0]"));
        assert_eq!(compared(json!(null), p), json!(null));
        assert_eq!(compared(json!(7), p), json!(7));
        assert_eq!(compared(json!("[1.0, 2]"), p), json!("[1,2]"));
        // A schemaless engine's array of numbers, as the vector's text.
        assert_eq!(compared(json!([1.0, 2.0]), p), json!("[1,2]"));
        assert_eq!(compared(json!(["a"]), p), json!(["a"]));
    }

    /// A pair compares at the lower precision of its sides: a float64
    /// vector (DuckDB `DOUBLE[n]`, ClickHouse `Array(Float64)`, Oracle
    /// `FLOAT64`) equals the float32 one a sync writes from it.
    #[test]
    fn a_pair_compares_at_the_lower_precision() {
        use VectorElement as E;
        assert_eq!(pair_precision(None, None), None);
        assert_eq!(
            pair_precision(Some(E::F32), Some(E::F32)),
            Some(Precision::F32)
        );
        assert_eq!(
            pair_precision(Some(E::F64), Some(E::F32)),
            Some(Precision::F32)
        );
        assert_eq!(
            pair_precision(Some(E::F16), Some(E::F64)),
            Some(Precision::F32)
        );
        assert_eq!(
            pair_precision(Some(E::F64), Some(E::F64)),
            Some(Precision::F64)
        );
        assert_eq!(
            pair_precision(Some(E::Other), Some(E::F64)),
            Some(Precision::F64)
        );
        // Against a side that isn't a vector column: the vector's own.
        assert_eq!(pair_precision(None, Some(E::F32)), Some(Precision::F32));
        assert_eq!(pair_precision(Some(E::F64), None), Some(Precision::F64));
        // pgvector's float32 against a float64's widened value or more
        // digits: the same vector at float32.
        let p = pair_precision(Some(E::F32), Some(E::F64)).unwrap();
        let widened = 0.1f32 as f64;
        assert_eq!(
            compared_text(&json!("[0.1, 0.12345679]"), p),
            compared_text(&json!([widened, 0.123456789]), p)
        );
    }

    #[test]
    fn vector_types_are_told_by_their_spelling() {
        for t in [
            "vector(3)",
            "VECTOR(768, FLOAT32)",
            "VECTOR(5, FLOAT32, SPARSE)",
            "VECTOR<FLOAT>(3)",
            "vector<float, 3>",
            "halfvec(4)",
            "sparsevec(5)",
            "float_vector(3)",
            "F32_BLOB(3)",
            "REAL_VECTOR(3)",
            "dense_vector",
            "FLOAT[3]",
            "DOUBLE[1536]",
            " vector ",
        ] {
            assert!(looks_like_vector_type(t), "{t}");
        }
        for t in [
            "",
            "varchar(3)",
            "real[]",
            "FLOAT[]",
            "float",
            "double precision",
            "INTEGER[3]",
            "vectors",
            "Array(Float32)",
            "json",
        ] {
            assert!(!looks_like_vector_type(t), "{t}");
        }
    }

    #[test]
    fn sparse_vectors_read_and_write_their_engines_text() {
        assert_eq!(SparseText::of_type("sparsevec(5)"), Some(SparseText::Pg));
        assert_eq!(
            SparseText::of_type("VECTOR(5, FLOAT32, SPARSE)"),
            Some(SparseText::Oracle)
        );
        assert_eq!(SparseText::of_type("vector(5)"), None);
        let s = Sparse {
            dims: 5,
            entries: vec![(1, 0.5), (3, -2.0)],
        };
        assert_eq!(s.text(SparseText::Pg), "{1:0.5,3:-2}/5");
        assert_eq!(s.text(SparseText::Oracle), "[5,[1,3],[0.5,-2]]");
        for f in [SparseText::Pg, SparseText::Oracle] {
            assert_eq!(Sparse::parse(&s.text(f), f), Some(s.clone()));
        }
        assert_eq!(
            Sparse::parse(" {1: 0.5 , 3:-2e0} / 5", SparseText::Pg),
            Some(s.clone())
        );
        assert_eq!(
            Sparse::parse("{}/3", SparseText::Pg).map(|s| s.entries.len()),
            Some(0)
        );
        for bad in ["[1,2]", "{1:a}/3", "{1:2}", "5"] {
            assert_eq!(Sparse::parse(bad, SparseText::Pg), None, "{bad}");
        }
        for bad in ["[5,[1],[1,2]]", "[1,2]", "{1:2}/3"] {
            assert_eq!(Sparse::parse(bad, SparseText::Oracle), None, "{bad}");
        }
    }
}
