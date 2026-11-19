//! SAP HANA Cloud's `REAL_VECTOR(n)` and its HNSW vector indexes.
//!
//! The client (`hdbconnect_async` 0.32) has no `REAL_VECTOR` type: a
//! result with one fails to decode ("Illegal type code 96"), which
//! [`explain`] turns into what to do. DBine reads vectors as text itself
//! (`TO_NVARCHAR`, in transfers) and writes them with `TO_REAL_VECTOR`.
//! The catalog (`SYS.TABLE_COLUMNS.LENGTH`) gives the dimension, and
//! `SYS.VECTOR_INDEXES` the indexes' settings. Without a HANA Cloud
//! server at hand these follow SAP's SQL reference and are covered by unit
//! tests only.

use dbine_driver::vector::{self, metric, param, VectorIndex};
use std::collections::BTreeMap;

/// The type's name in the catalog.
pub const TYPE: &str = "REAL_VECTOR";

/// `IndexDef::kind` of a vector index (`CREATE HNSW VECTOR INDEX`).
pub const KIND: &str = "HNSW VECTOR";

/// The engine's name for the search-time candidate list (`efSearch`), in
/// `VectorIndex::params`.
pub const EF_SEARCH: &str = "ef_search";

/// `REAL_VECTOR(n)`, or `REAL_VECTOR` (any dimension).
pub(crate) fn type_name(dims: Option<i64>) -> String {
    match dims {
        Some(n) if n > 0 => format!("{TYPE}({n})"),
        _ => TYPE.to_string(),
    }
}

/// Whether a catalog type is `REAL_VECTOR`.
pub(crate) fn is_vector(data_type: &str) -> bool {
    data_type
        .trim()
        .get(..TYPE.len())
        .is_some_and(|h| h.eq_ignore_ascii_case(TYPE))
}

/// The select-list item that reads a vector column as its text.
pub(crate) fn read_expr(quoted: &str) -> String {
    format!("TO_NVARCHAR({quoted}) AS {quoted}")
}

/// HANA's text of a vector (`[1,2.5]`, `[1.0e0,…]`) in the canonical form.
pub(crate) fn canonical(text: &str) -> String {
    vector::parse(text)
        .map(|v| vector::text(v.into_iter().map(|x| x as f32)))
        .unwrap_or_else(|| text.to_string())
}

/// A vector edit's value as a literal: `TO_REAL_VECTOR('[1,2]')`.
pub(crate) fn literal(v: &serde_json::Value) -> Option<String> {
    vector::value_text(v).map(|t| format!("TO_REAL_VECTOR('{t}')"))
}

/// What to do when the client can't decode a `REAL_VECTOR` result.
pub(crate) fn explain(message: &str) -> Option<&'static str> {
    message.contains("Illegal type code 96").then_some(
        "El resultado tiene una columna REAL_VECTOR y el cliente de HANA que usa DBine no lee ese tipo: \
         pedila como texto con TO_NVARCHAR(columna).",
    )
}

/// HANA's similarity functions and the standard distances.
pub(crate) fn metric_of(native: &str) -> String {
    match native.trim().to_ascii_uppercase().as_str() {
        "COSINE_SIMILARITY" | "COSINE" => metric::COSINE.into(),
        "L2DISTANCE" | "L2" | "EUCLIDEAN" => metric::L2.into(),
        other => other.to_ascii_lowercase(),
    }
}

/// The `SIMILARITY FUNCTION …` of a standard (or HANA) distance.
pub(crate) fn native_metric(m: &str) -> Option<&'static str> {
    match m.trim().to_ascii_lowercase().as_str() {
        metric::COSINE | "cosine_similarity" => Some("COSINE_SIMILARITY"),
        metric::L2 | "l2distance" | "euclidean" => Some("L2DISTANCE"),
        _ => None,
    }
}

/// A `SYS.VECTOR_INDEXES` row (column name → value) as the index's
/// [`VectorIndex`]: its similarity function and the build / search
/// configurations' `M`, `efConstruction` and `efSearch`.
pub(crate) fn from_catalog(row: &BTreeMap<String, String>) -> VectorIndex {
    let find = |needle: &str| {
        row.iter()
            .find(|(k, _)| k.to_ascii_uppercase().contains(needle))
            .map(|(_, v)| v.as_str())
    };
    let mut params = BTreeMap::new();
    for (config, keys) in [
        (
            "BUILD",
            &[("M", param::M), ("efConstruction", param::EF_CONSTRUCTION)][..],
        ),
        ("SEARCH", &[("efSearch", EF_SEARCH)][..]),
    ] {
        let Some(json) = find(config) else { continue };
        let Ok(v) = serde_json::from_str::<serde_json::Value>(json) else {
            continue;
        };
        for (native, standard) in keys {
            if let Some(n) = v.get(native).and_then(|n| n.as_i64()) {
                params.insert(standard.to_string(), n.to_string());
            }
        }
    }
    VectorIndex {
        algorithm: Some("hnsw".into()),
        metric: find("SIMILARITY").map(metric_of),
        params,
    }
}

/// `CREATE HNSW VECTOR INDEX … ON t (c) SIMILARITY FUNCTION … BUILD
/// CONFIGURATION '{…}' SEARCH CONFIGURATION '{…}'`, or why it can't be.
pub(crate) fn index_sql(
    table: &str,
    name: &str,
    column: Option<&str>,
    v: &VectorIndex,
) -> Result<String, String> {
    let column = column.ok_or_else(|| "el índice vectorial no tiene columna".to_string())?;
    if let Some(a) = v
        .algorithm
        .as_deref()
        .filter(|a| !a.eq_ignore_ascii_case("hnsw"))
    {
        return Err(format!("HANA no tiene el algoritmo «{a}» (solo HNSW)"));
    }
    let mut s = format!("CREATE HNSW VECTOR INDEX {name} ON {table} ({column})");
    if let Some(m) = v.metric.as_deref() {
        let f = native_metric(m).ok_or_else(|| {
            format!("HANA no tiene la distancia «{m}» (solo coseno y euclidiana)")
        })?;
        s.push_str(&format!(" SIMILARITY FUNCTION {f}"));
    }
    let number = |k: &str| v.params.get(k).and_then(|x| x.trim().parse::<u64>().ok());
    let mut build = serde_json::Map::new();
    if let Some(m) = number(param::M) {
        build.insert("M".into(), m.into());
    }
    if let Some(ef) = number(param::EF_CONSTRUCTION) {
        build.insert("efConstruction".into(), ef.into());
    }
    if !build.is_empty() {
        s.push_str(&format!(
            " BUILD CONFIGURATION '{}'",
            serde_json::Value::Object(build)
        ));
    }
    if let Some(ef) = number(EF_SEARCH) {
        s.push_str(&format!(" SEARCH CONFIGURATION '{{\"efSearch\":{ef}}}'"));
    }
    Ok(s)
}

/// What the designer offers: HNSW, cosine or euclidean, `M`,
/// `efConstruction` and `efSearch`.
pub(crate) fn spec() -> dbine_driver::VectorIndexSpec {
    use dbine_driver::VectorParamSpec as P;
    dbine_driver::VectorIndexSpec {
        algorithms: vec!["hnsw"],
        metrics: vec![metric::COSINE, metric::L2],
        params: vec![
            P {
                key: param::M,
                algorithms: vec![],
                default: "64",
            },
            P {
                key: param::EF_CONSTRUCTION,
                algorithms: vec![],
                default: "128",
            },
            P {
                key: EF_SEARCH,
                algorithms: vec![],
                default: "",
            },
        ],
        filter_columns: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn types_and_text() {
        assert_eq!(type_name(Some(1536)), "REAL_VECTOR(1536)");
        assert_eq!(type_name(Some(0)), "REAL_VECTOR");
        assert_eq!(type_name(None), "REAL_VECTOR");
        assert!(is_vector("REAL_VECTOR(3)") && !is_vector("REAL"));
        assert_eq!(read_expr("\"V\""), "TO_NVARCHAR(\"V\") AS \"V\"");
        assert_eq!(canonical("[1.0e0,2.5,3.0000001e-01]"), "[1,2.5,0.3]");
        assert_eq!(canonical("not a vector"), "not a vector");
        assert_eq!(
            literal(&json!([1.0, 2.5])).as_deref(),
            Some("TO_REAL_VECTOR('[1,2.5]')")
        );
        assert!(explain("impl error: Illegal type code 96").is_some());
        assert!(explain("something else").is_none());
    }

    #[test]
    fn indexes_both_ways() {
        let row: BTreeMap<String, String> = [
            ("INDEX_NAME", "IX"),
            ("SIMILARITY_FUNCTION", "L2DISTANCE"),
            ("BUILD_CONFIGURATION", r#"{"M":32,"efConstruction":200}"#),
            ("SEARCH_CONFIGURATION", r#"{"efSearch":50}"#),
        ]
        .into_iter()
        .map(|(k, v)| (k.to_string(), v.to_string()))
        .collect();
        let v = from_catalog(&row);
        assert_eq!(v.metric.as_deref(), Some("l2"));
        assert_eq!(v.params.get("m").map(String::as_str), Some("32"));
        assert_eq!(v.params.get("ef_search").map(String::as_str), Some("50"));
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", Some("\"V\""), &v).unwrap(),
            "CREATE HNSW VECTOR INDEX \"IX\" ON \"T\" (\"V\") SIMILARITY FUNCTION L2DISTANCE \
             BUILD CONFIGURATION '{\"M\":32,\"efConstruction\":200}' SEARCH CONFIGURATION '{\"efSearch\":50}'"
        );
        let plain = VectorIndex {
            metric: Some("cosine".into()),
            ..Default::default()
        };
        assert_eq!(
            index_sql("t", "ix", Some("v"), &plain).unwrap(),
            "CREATE HNSW VECTOR INDEX ix ON t (v) SIMILARITY FUNCTION COSINE_SIMILARITY"
        );
        let ip = VectorIndex {
            metric: Some("ip".into()),
            ..Default::default()
        };
        assert!(index_sql("t", "ix", Some("v"), &ip).is_err());
        let ivf = VectorIndex {
            algorithm: Some("ivf".into()),
            ..Default::default()
        };
        assert!(index_sql("t", "ix", Some("v"), &ivf).is_err());
        assert!(index_sql("t", "ix", None, &plain).is_err());
    }
}
