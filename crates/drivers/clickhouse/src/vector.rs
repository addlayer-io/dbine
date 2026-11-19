//! Vectors in ClickHouse. There is no vector type: an embedding is an
//! `Array(Float32)` (or `Array(BFloat16)`, `Array(Float64)`), searched with
//! the `vector_similarity` skip index and the distance functions
//! (`cosineDistance`, `L2Distance`, `dotProduct`…).
//!
//! Which arrays are vectors: `Array(Float32)` and `Array(BFloat16)`, the
//! types ClickHouse's vector search documents for embeddings (BFloat16
//! exists for machine learning), in results, describes and conversions.
//! `Array(Float64)` is ClickHouse's general array of numbers, so it's a
//! vector only where a `vector_similarity` index says so (the index, not
//! the result, knows it). A ClickHouse array has no fixed length: the
//! dimension is the index's.
//!
//! Timeplus Proton (3.0) has `array(float32)` and the distances
//! (`cosine_distance`, `l2_distance`…) but neither `bfloat16` nor a vector
//! index.

use dbine_driver::vector::{metric, param};
use dbine_driver::{VectorElement, VectorIndex, VectorMeta};

/// The index type of a vector index.
pub const KIND: &str = "vector_similarity";
/// The `vector_similarity` granularity ClickHouse uses when none is given
/// (and recommends): an index covers a whole part.
pub const DEFAULT_GRANULARITY: &str = "100000000";
/// The index's dimension, which ClickHouse requires, as a parameter.
pub const DIMENSIONS: &str = "dimensions";

/// ClickHouse's distance functions for the index and the standard names.
const METRICS: [(&str, &str); 3] = [
    ("cosineDistance", metric::COSINE),
    ("L2Distance", metric::L2),
    ("dotProduct", metric::IP),
];

/// `Nullable(LowCardinality(T))` → `t` (an array can't be either, but a
/// result type may be spelled so).
fn base(t: &str) -> String {
    crate::base_type(t)
}

/// A column of type `t` holds vectors ([`self`]): `Array(Float32)` or
/// `Array(BFloat16)` (`array(float32)` in Timeplus).
pub fn meta(t: &str) -> Option<VectorMeta> {
    let element = match base(t).replace(' ', "").as_str() {
        "array(float32)" => VectorElement::F32,
        "array(bfloat16)" => VectorElement::Bf16,
        _ => return None,
    };
    Some(VectorMeta::dense(None, element))
}

/// A vector cell as the server wrote it (`[0.1,2,1e-7]`) as DBine's vector
/// text (`[0.1,2,0.0000001]`), each number the shortest way a Float32 (or
/// BFloat16, which a Float32 holds) reads back; `None` when it isn't a
/// whole array of finite numbers.
pub fn cell_text(raw: &str) -> Option<String> {
    let n = dbine_driver::vector::parse(raw)?;
    Some(dbine_driver::vector::text(n.into_iter().map(|x| x as f32)))
}

/// A vector value of an edit or insert script (a JSON array of numbers) as
/// ClickHouse's array literal `[1,2.5]`.
pub fn literal(v: &serde_json::Value) -> Option<String> {
    match v {
        serde_json::Value::Array(_) => dbine_driver::vector::value_text(v),
        _ => None,
    }
}

/// The quoted or bare arguments of `vector_similarity(…)`.
fn args(type_full: &str) -> Option<Vec<String>> {
    let t = type_full.trim();
    let head = t.get(..KIND.len())?;
    if !head.eq_ignore_ascii_case(KIND) {
        return None;
    }
    let inner = t[KIND.len()..]
        .trim()
        .strip_prefix('(')?
        .strip_suffix(')')?;
    Some(
        crate::schema::split_top(inner)
            .into_iter()
            .map(|a| {
                let a = a.trim();
                match a.strip_prefix('\'').and_then(|x| x.strip_suffix('\'')) {
                    Some(q) => q.replace("\\'", "'").replace("''", "'"),
                    None => a.to_string(),
                }
            })
            .collect(),
    )
}

/// A `vector_similarity('hnsw', 'cosineDistance', 3[, 'bf16', 32, 128])`
/// index type as its [`VectorIndex`]: the dimension as `dimensions`, the
/// quantization, `m` (`hnsw_max_connections_per_layer`) and
/// `ef_construction` (`hnsw_candidate_list_size_for_construction`); a 0
/// (ClickHouse's default) is left out.
pub fn parse_index(type_full: &str) -> Option<VectorIndex> {
    let a = args(type_full)?;
    let mut v = VectorIndex {
        algorithm: a.first().map(|x| x.to_ascii_lowercase()),
        metric: a.get(1).map(|m| {
            METRICS
                .iter()
                .find(|(n, _)| n.eq_ignore_ascii_case(m))
                .map_or_else(|| m.clone(), |(_, std)| std.to_string())
        }),
        ..Default::default()
    };
    for (i, key) in [
        (2, DIMENSIONS),
        (3, param::QUANTIZATION),
        (4, param::M),
        (5, param::EF_CONSTRUCTION),
    ] {
        if let Some(x) = a.get(i).filter(|x| !x.is_empty() && x.as_str() != "0") {
            v.params.insert(key.to_string(), x.clone());
        }
    }
    Some(v)
}

/// The index type for `v`: `vector_similarity('hnsw', 'cosineDistance', 3)`,
/// with the six arguments when a quantization, `m` or `ef_construction` is
/// set (0 for the ones that aren't: ClickHouse's default). `Err`: it has no
/// dimension, which ClickHouse requires.
pub fn index_type(v: &VectorIndex) -> Result<String, String> {
    let p = |k: &str| v.params.get(k).map(|x| x.trim()).filter(|x| !x.is_empty());
    let dims = p(DIMENSIONS).ok_or(
        "ClickHouse necesita la dimensión del vector (parámetro «dimensions») para crear el índice",
    )?;
    if dims.parse::<u32>().is_err() {
        return Err(format!(
            "la dimensión del índice vectorial no es un número: «{dims}»"
        ));
    }
    let lit = |s: &str| format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"));
    let algorithm = v
        .algorithm
        .as_deref()
        .filter(|a| !a.is_empty())
        .unwrap_or("hnsw");
    let m = v
        .metric
        .as_deref()
        .filter(|m| !m.is_empty())
        .unwrap_or(metric::L2);
    let native = METRICS
        .iter()
        .find(|(_, std)| *std == m)
        .map_or(m, |(n, _)| n);
    let mut out = format!("{KIND}({}, {}, {dims}", lit(algorithm), lit(native));
    let (q, mm, ef) = (
        p(param::QUANTIZATION),
        p(param::M),
        p(param::EF_CONSTRUCTION),
    );
    if q.is_some() || mm.is_some() || ef.is_some() {
        out.push_str(&format!(
            ", {}, {}, {}",
            lit(q.unwrap_or("bf16")),
            mm.unwrap_or("0"),
            ef.unwrap_or("0")
        ));
    }
    out.push(')');
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn which_arrays_are_vectors() {
        assert_eq!(
            meta("Array(Float32)"),
            Some(VectorMeta::dense(None, VectorElement::F32))
        );
        assert_eq!(
            meta("array(bfloat16)").unwrap().element,
            VectorElement::Bf16
        );
        for no in [
            "Array(Float64)",
            "Array(Int8)",
            "Float32",
            "Array(Array(Float32))",
        ] {
            assert_eq!(meta(no), None, "{no}");
        }
        assert_eq!(
            cell_text("[0.1,2,1e-7]").as_deref(),
            Some("[0.1,2,0.0000001]")
        );
        assert_eq!(cell_text("[nan,1]"), None);
        assert_eq!(literal(&json!([1.0, 2.5])).as_deref(), Some("[1,2.5]"));
        assert_eq!(literal(&json!("[1]")), None);
    }

    #[test]
    fn index_types_round_trip() {
        let v = parse_index("vector_similarity('hnsw', 'cosineDistance', 3)").unwrap();
        assert_eq!(v.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(v.metric.as_deref(), Some("cosine"));
        assert_eq!(v.params.get(DIMENSIONS).map(String::as_str), Some("3"));
        assert_eq!(
            index_type(&v).unwrap(),
            "vector_similarity('hnsw', 'cosineDistance', 3)"
        );
        let six = "vector_similarity('hnsw', 'L2Distance', 768, 'bf16', 32, 128)";
        let v = parse_index(six).unwrap();
        assert_eq!(
            v.params
                .iter()
                .map(|(k, v)| format!("{k}={v}"))
                .collect::<Vec<_>>(),
            [
                "dimensions=768",
                "ef_construction=128",
                "m=32",
                "quantization=bf16"
            ]
        );
        assert_eq!(index_type(&v).unwrap(), six);
        // 0 is ClickHouse's default: left out, and written back as 0.
        let v = parse_index("vector_similarity('hnsw', 'dotProduct', 3, 'i8', 0, 0)").unwrap();
        assert_eq!(v.metric.as_deref(), Some("ip"));
        assert_eq!(v.params.len(), 2);
        assert_eq!(
            index_type(&v).unwrap(),
            "vector_similarity('hnsw', 'dotProduct', 3, 'i8', 0, 0)"
        );
        assert!(parse_index("bloom_filter(0.01)").is_none());
        assert!(index_type(&VectorIndex::default())
            .unwrap_err()
            .contains("dimensions"));
    }
}
