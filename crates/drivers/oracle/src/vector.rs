//! Oracle's `VECTOR(dims|*, FLOAT32|FLOAT64|INT8|BINARY|*, DENSE|SPARSE)`
//! (Oracle Database 23ai and later) and its vector indexes.
//!
//! - **Cells:** a dense vector is its JSON-array text (`[1,2.5,-3]`),
//!   numbers written the shortest way. A BINARY vector keeps Oracle's own
//!   text: its packed bytes (`[255,0]` for 16 dimensions), which is what a
//!   literal of that column takes back. A SPARSE vector keeps Oracle's text
//!   `[dims,[indices],[values]]`, indices from 0.
//! - **Types:** the dictionary's `VECTOR_INFO` (`VECTOR(3,FLOAT32,DENSE)`)
//!   gives the declared type; a result column's metadata gives the same
//!   from the client.
//! - **Literals:** the text in a string, which the server converts to the
//!   column's type (a `TO_VECTOR` would make every one FLOAT32 and refuse
//!   BINARY and SPARSE); past 4000 bytes, as a `TO_CLOB(…) || …` chain.
//! - **Indexes:** `CREATE VECTOR INDEX … ORGANIZATION INMEMORY NEIGHBOR
//!   GRAPH` (HNSW) or `NEIGHBOR PARTITIONS` (IVF), `DISTANCE …`, `WITH
//!   TARGET ACCURACY n`, `PARAMETERS (TYPE HNSW, NEIGHBORS m,
//!   EFCONSTRUCTION ef | TYPE IVF, NEIGHBOR PARTITIONS n)`, `INCLUDE (…)`.

use dbine_driver::vector::{self, metric, param, VectorElement, VectorIndex, VectorMeta};
use std::collections::BTreeMap;

/// `IndexDef::kind` of a vector index.
pub const KIND: &str = "VECTOR";

/// Whether `ALL_TAB_COLS` / `ALL_TAB_COLUMNS` have `VECTOR_INFO` (each
/// vector column's declared type); asked once per connection, kept in
/// `cache` (0 not asked, 1 yes, 2 no).
pub(crate) fn info_column(c: &oracledb::Connection, cache: &std::sync::atomic::AtomicU8) -> bool {
    use std::sync::atomic::Ordering;
    match cache.load(Ordering::Relaxed) {
        1 => return true,
        2 => return false,
        _ => {}
    }
    let has = c.version().is_ok_and(|v| v.0 >= 23)
        && c.query_row(
            "SELECT COUNT(*) FROM all_tab_columns
              WHERE owner = 'SYS' AND table_name = 'ALL_TAB_COLS' AND column_name = 'VECTOR_INFO'",
            &[],
        )
        .and_then(|r| r.get::<i64>(0))
        .is_ok_and(|n| n > 0);
    cache.store(if has { 1 } else { 2 }, Ordering::Relaxed);
    has
}

/// The select-list item with a column's `VECTOR_INFO` (`c` its alias), or
/// a NULL where the dictionary doesn't have it.
pub(crate) fn info_expr(has: bool) -> &'static str {
    if has {
        "c.vector_info"
    } else {
        "CAST(NULL AS VARCHAR2(1))"
    }
}

/// The declared type of a column whose `DATA_TYPE` is `VECTOR`, from its
/// `VECTOR_INFO`; a bare `VECTOR` without it.
pub(crate) fn declared(info: Option<&str>) -> String {
    info.and_then(Shape::parse)
        .map_or_else(|| "VECTOR".to_string(), |s| s.ddl())
}

/// The engine's name for a vector index's target accuracy, in
/// `VectorIndex::params`.
pub const TARGET_ACCURACY: &str = "target_accuracy";

/// Longest string literal SQL takes (bytes).
const MAX_LITERAL: usize = 4000;

/// A declared vector type, as the dictionary or the client says it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Shape {
    /// `None`: `*` (any).
    pub dims: Option<u32>,
    /// `FLOAT32`, `FLOAT64`, `INT8`, `BINARY`; `None`: `*`.
    pub format: Option<String>,
    pub sparse: bool,
}

impl Shape {
    /// `VECTOR(3,FLOAT32,DENSE)` (`ALL_TAB_COLS.VECTOR_INFO`), `VECTOR(3,
    /// float32)` / `VECTOR(*,float32)` (the client), `VECTOR(3)`, `VECTOR`.
    pub(crate) fn parse(t: &str) -> Option<Shape> {
        let t = t.trim();
        if !t.get(..6)?.eq_ignore_ascii_case("VECTOR") {
            return None;
        }
        let inner = t[6..].trim();
        if inner.is_empty() {
            return Some(Shape {
                dims: None,
                format: None,
                sparse: false,
            });
        }
        let inner = inner.strip_prefix('(')?.strip_suffix(')')?;
        let mut parts = inner.split(',').map(|p| p.trim().to_ascii_uppercase());
        let dims = match parts.next()?.as_str() {
            "*" | "" => None,
            n => Some(n.parse().ok()?),
        };
        let format = match parts.next().as_deref() {
            None | Some("*") => None,
            Some(f @ ("FLOAT32" | "FLOAT64" | "INT8" | "BINARY")) => Some(f.to_string()),
            Some(_) => return None,
        };
        let sparse = match parts.next().as_deref() {
            None | Some("DENSE") => false,
            Some("SPARSE") => true,
            Some(_) => return None,
        };
        parts.next().is_none().then_some(Shape {
            dims,
            format,
            sparse,
        })
    }

    /// As DDL declares it: `VECTOR(3, FLOAT32)`, `VECTOR(*, *)`,
    /// `VECTOR(5, FLOAT32, SPARSE)`.
    pub(crate) fn ddl(&self) -> String {
        let dims = self.dims.map_or("*".to_string(), |n| n.to_string());
        let format = self.format.as_deref().unwrap_or("*");
        if self.sparse {
            format!("VECTOR({dims}, {format}, SPARSE)")
        } else {
            format!("VECTOR({dims}, {format})")
        }
    }

    pub(crate) fn element(&self) -> VectorElement {
        match self.format.as_deref() {
            Some("FLOAT64") => VectorElement::F64,
            Some("INT8") => VectorElement::I8,
            Some("BINARY") => VectorElement::Bit,
            // FLOAT32, and `*` (stored as given, usually float32).
            _ => VectorElement::F32,
        }
    }

    pub(crate) fn meta(&self) -> VectorMeta {
        VectorMeta {
            dims: self.dims,
            element: self.element(),
            sparse: self.sparse,
        }
    }
}

/// A result column's shape from the client's metadata
/// (`Metadata::data_type`: `VECTOR(3,float32)`, `VECTOR(*,float32)`, `VECTOR`).
pub(crate) fn column_shape(data_type: &str, sparse: bool) -> Shape {
    let mut s = Shape::parse(data_type).unwrap_or(Shape {
        dims: None,
        format: None,
        sparse: false,
    });
    s.sparse = sparse;
    s
}

/// A vector value as its cell / transfer text.
pub(crate) fn text(v: &oracledb::Vector) -> String {
    match v {
        oracledb::Vector::Dense(d) => dense(d),
        oracledb::Vector::Sparse(s) => format!(
            "[{},{},{}]",
            s.num_dimensions(),
            vector::text(s.indices().iter().map(|&i| i as f64)),
            dense(s.values())
        ),
    }
}

fn dense(d: &oracledb::VectorData) -> String {
    use oracledb::VectorData as D;
    match d {
        D::Float32(x) => vector::text(x.iter().copied()),
        D::Float64(x) => vector::text(x.iter().copied()),
        D::Int8(x) => vector::text(x.iter().copied()),
        // Oracle's own text for BINARY: the packed bytes.
        D::Binary(x) => vector::text(x.iter().copied()),
    }
}

/// A vector edit's value as a literal: its text in a string the server
/// converts to the column's type, chained through `TO_CLOB` past the
/// 4000-byte limit of a literal. `None` when `v` isn't a vector.
pub(crate) fn literal(v: &serde_json::Value) -> Option<String> {
    vector::value_text(v).map(|t| text_literal(&t))
}

/// `'text'`, or `TO_CLOB('…') || '…'` in chunks for a long one. The text is
/// ASCII (numbers, brackets, commas).
pub(crate) fn text_literal(t: &str) -> String {
    if t.len() <= MAX_LITERAL {
        return format!("'{}'", t.replace('\'', "''"));
    }
    let chunks: Vec<&str> = t
        .as_bytes()
        .chunks(MAX_LITERAL)
        .map(|c| std::str::from_utf8(c).unwrap_or(""))
        .collect();
    let mut s = format!("TO_CLOB('{}')", chunks[0]);
    for c in &chunks[1..] {
        s.push_str(&format!("\n  || '{c}'"));
    }
    s
}

/// Text for a `--` comment line: line breaks can't end it early.
pub(crate) fn comment_text(s: &str) -> String {
    s.chars()
        .map(|c| {
            if matches!(c, '\r' | '\n' | '\u{85}' | '\u{2028}' | '\u{2029}') {
                '?'
            } else {
                c
            }
        })
        .collect()
}

/// Oracle's distance names and the standard ones.
pub(crate) fn metric_of(native: &str) -> String {
    match native.trim().to_ascii_uppercase().as_str() {
        "COSINE" => metric::COSINE.into(),
        "EUCLIDEAN" | "L2_DISTANCE" => metric::L2.into(),
        "DOT" | "INNER_PRODUCT" => metric::IP.into(),
        "MANHATTAN" | "L1_DISTANCE" => metric::L1.into(),
        "HAMMING" => metric::HAMMING.into(),
        "JACCARD" => metric::JACCARD.into(),
        other => other.to_ascii_lowercase(),
    }
}

/// The `DISTANCE …` of a standard (or Oracle) distance name.
pub(crate) fn native_metric(m: &str) -> Option<&'static str> {
    Some(match m.trim().to_ascii_lowercase().as_str() {
        metric::COSINE => "COSINE",
        metric::L2 | "euclidean" => "EUCLIDEAN",
        "euclidean_squared" | "l2_squared" => "EUCLIDEAN_SQUARED",
        metric::IP | "dot" => "DOT",
        metric::L1 | "manhattan" => "MANHATTAN",
        metric::HAMMING => "HAMMING",
        metric::JACCARD => "JACCARD",
        _ => return None,
    })
}

/// The algorithm of `ALL_INDEXES.INDEX_SUBTYPE` / `V$VECTOR_INDEX`'s
/// organization.
pub(crate) fn algorithm_of(subtype: &str) -> Option<&'static str> {
    let s = subtype.to_ascii_uppercase();
    if s.contains("HNSW") || s.contains("NEIGHBOR GRAPH") || s.contains("NEIGHBOR_GRAPH") {
        Some("hnsw")
    } else if s.contains("IVF") || s.contains("PARTITIONS") {
        Some("ivf")
    } else {
        None
    }
}

/// What the dictionary says about one vector index (`VECSYS.VECTOR$INDEX`'s
/// parameters, or `V$VECTOR_INDEX`'s distance and accuracy).
#[derive(Debug, Clone, Default, PartialEq)]
pub(crate) struct Details {
    pub distance: Option<String>,
    pub accuracy: Option<i64>,
    pub neighbors: Option<i64>,
    pub ef_construction: Option<i64>,
    /// IVF: the partitions asked for (`target_centroids`).
    pub partitions: Option<i64>,
}

impl Details {
    /// From `VECSYS.VECTOR$INDEX.IDX_PARAMS` (JSON).
    pub(crate) fn from_params(json: &str) -> Details {
        let v: serde_json::Value = serde_json::from_str(json).unwrap_or_default();
        let n = |k: &str| v.get(k).and_then(serde_json::Value::as_i64);
        Details {
            distance: v
                .get("distance")
                .and_then(serde_json::Value::as_str)
                .map(str::to_string),
            accuracy: n("accuracy"),
            neighbors: n("num_neighbors"),
            ef_construction: n("efConstruction"),
            partitions: n("target_centroids"),
        }
    }
}

/// The index's [`VectorIndex`]: standard metric and parameter names.
pub(crate) fn index(algorithm: Option<&str>, d: &Details) -> VectorIndex {
    let mut params = BTreeMap::new();
    if let Some(a) = d.accuracy {
        params.insert(TARGET_ACCURACY.to_string(), a.to_string());
    }
    match algorithm {
        Some("hnsw") => {
            if let Some(m) = d.neighbors {
                params.insert(param::M.to_string(), m.to_string());
            }
            if let Some(ef) = d.ef_construction {
                params.insert(param::EF_CONSTRUCTION.to_string(), ef.to_string());
            }
        }
        Some("ivf") => {
            if let Some(p) = d.partitions {
                params.insert(param::LISTS.to_string(), p.to_string());
            }
        }
        _ => {}
    }
    VectorIndex {
        algorithm: algorithm.map(str::to_string),
        metric: d.distance.as_deref().map(metric_of),
        params,
    }
}

/// `CREATE VECTOR INDEX`. `cols`: the vector column, then the INCLUDE
/// ones, quoted. `Err` with why it can't be written.
pub(crate) fn index_sql(
    table: &str,
    name: &str,
    cols: &[String],
    v: &VectorIndex,
) -> Result<String, String> {
    let (first, include) = cols
        .split_first()
        .ok_or_else(|| "el índice vectorial no tiene columna".to_string())?;
    let algorithm = v
        .algorithm
        .as_deref()
        .map(str::to_ascii_lowercase)
        .unwrap_or_else(|| "hnsw".into());
    let organization = match algorithm.as_str() {
        "hnsw" => "INMEMORY NEIGHBOR GRAPH",
        "ivf" | "ivfflat" | "ivf_flat" => "NEIGHBOR PARTITIONS",
        other => {
            return Err(format!(
                "Oracle no tiene el algoritmo «{other}» (HNSW o IVF)"
            ))
        }
    };
    let mut s = format!("CREATE VECTOR INDEX {name} ON {table} ({first})");
    if !include.is_empty() {
        s.push_str(&format!(" INCLUDE ({})", include.join(", ")));
    }
    s.push_str(&format!(" ORGANIZATION {organization}"));
    if let Some(m) = v.metric.as_deref() {
        let d = native_metric(m).ok_or_else(|| format!("Oracle no tiene la distancia «{m}»"))?;
        s.push_str(&format!(" DISTANCE {d}"));
    }
    let p = |k: &str| {
        v.params
            .get(k)
            .map(|x| x.trim())
            .filter(|x| !x.is_empty() && x.chars().all(|c| c.is_ascii_digit()))
    };
    if let Some(a) = p(TARGET_ACCURACY) {
        s.push_str(&format!(" WITH TARGET ACCURACY {a}"));
    }
    let mut params = Vec::new();
    if organization.starts_with("INMEMORY") {
        if let Some(m) = p(param::M) {
            params.push(format!("NEIGHBORS {m}"));
        }
        if let Some(ef) = p(param::EF_CONSTRUCTION) {
            params.push(format!("EFCONSTRUCTION {ef}"));
        }
        if !params.is_empty() {
            params.insert(0, "TYPE HNSW".into());
        }
    } else if let Some(n) = p(param::LISTS) {
        params.push("TYPE IVF".into());
        params.push(format!("NEIGHBOR PARTITIONS {n}"));
    }
    if !params.is_empty() {
        s.push_str(&format!(" PARAMETERS ({})", params.join(", ")));
    }
    Ok(s)
}

/// What the designer offers: HNSW (in memory) and IVF, every distance,
/// HNSW's neighbors and efConstruction, IVF's partitions, the target
/// accuracy, and INCLUDE columns.
pub(crate) fn spec() -> dbine_driver::VectorIndexSpec {
    use dbine_driver::VectorParamSpec as P;
    dbine_driver::VectorIndexSpec {
        algorithms: vec!["hnsw", "ivf"],
        metrics: vec![
            metric::COSINE,
            metric::L2,
            metric::IP,
            metric::L1,
            metric::HAMMING,
            metric::JACCARD,
        ],
        params: vec![
            P {
                key: param::M,
                algorithms: vec!["hnsw"],
                default: "32",
            },
            P {
                key: param::EF_CONSTRUCTION,
                algorithms: vec!["hnsw"],
                default: "300",
            },
            P {
                key: param::LISTS,
                algorithms: vec!["ivf"],
                default: "",
            },
            P {
                key: TARGET_ACCURACY,
                algorithms: vec![],
                default: "90",
            },
        ],
        filter_columns: true,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn shapes_from_the_dictionary_and_the_client() {
        let s = Shape::parse("VECTOR(3,FLOAT32,DENSE)").unwrap();
        assert_eq!(s.ddl(), "VECTOR(3, FLOAT32)");
        assert_eq!(s.meta(), VectorMeta::dense(Some(3), VectorElement::F32));
        let s = Shape::parse("VECTOR(5,FLOAT32,SPARSE)").unwrap();
        assert_eq!(s.ddl(), "VECTOR(5, FLOAT32, SPARSE)");
        assert!(s.meta().sparse);
        assert_eq!(
            Shape::parse("VECTOR(*,*,DENSE)").unwrap().ddl(),
            "VECTOR(*, *)"
        );
        assert_eq!(
            Shape::parse("VECTOR(16,BINARY,DENSE)").unwrap().element(),
            VectorElement::Bit
        );
        assert_eq!(
            Shape::parse("VECTOR(4, int8)").unwrap().ddl(),
            "VECTOR(4, INT8)"
        );
        assert_eq!(
            Shape::parse("VECTOR(2,float64)").unwrap().element(),
            VectorElement::F64
        );
        assert_eq!(Shape::parse("VECTOR").unwrap().ddl(), "VECTOR(*, *)");
        assert_eq!(
            Shape::parse("VECTOR(*,float32)").unwrap().ddl(),
            "VECTOR(*, FLOAT32)"
        );
        assert_eq!(Shape::parse("VARCHAR2(3)"), None);
        assert_eq!(Shape::parse("VECTOR(3,FLOAT16)"), None);
        let c = column_shape("VECTOR(5,float32)", true);
        assert_eq!(c.ddl(), "VECTOR(5, FLOAT32, SPARSE)");
    }

    #[test]
    fn cells_are_short_json_arrays() {
        use oracledb::VectorData as D;
        let v = oracledb::Vector::Dense(D::Float32(vec![1.0, 2.5, 0.1]));
        assert_eq!(text(&v), "[1,2.5,0.1]");
        let v = oracledb::Vector::Dense(D::Float64(vec![1.5, 2.25]));
        assert_eq!(text(&v), "[1.5,2.25]");
        let v = oracledb::Vector::Dense(D::Int8(vec![1, -2, 3]));
        assert_eq!(text(&v), "[1,-2,3]");
        let v = oracledb::Vector::Dense(D::Binary(vec![255, 0]));
        assert_eq!(text(&v), "[255,0]");
        let s = oracledb::Vector::Sparse(oracledb::SparseVector::new(
            5,
            vec![1, 3],
            D::Float32(vec![0.5, 2.5]),
        ));
        assert_eq!(text(&s), "[5,[1,3],[0.5,2.5]]");
    }

    #[test]
    fn literals_take_any_length() {
        assert_eq!(literal(&json!([1.0, 2.5])).as_deref(), Some("'[1,2.5]'"));
        assert_eq!(literal(&json!("x")), None);
        let long: Vec<f64> = (0..1536).map(|i| i as f64 * 0.001234567).collect();
        let lit = literal(&json!(long)).unwrap();
        assert!(lit.starts_with("TO_CLOB('[0,"), "{}", &lit[..40]);
        assert!(lit.contains("\n  || '"));
        // Every chunk fits a literal and the chunks put back the text.
        let text = vector::value_text(&json!(long)).unwrap();
        let mut back = String::new();
        for part in lit.split("\n  || ") {
            let inner = part
                .trim_start_matches("TO_CLOB(")
                .trim_end_matches(')')
                .trim_matches('\'');
            assert!(inner.len() <= MAX_LITERAL);
            back.push_str(inner);
        }
        assert_eq!(back, text);
    }

    #[test]
    fn indexes_both_ways() {
        let hnsw = Details::from_params(
            r#"{"type":"HNSW","distance":"EUCLIDEAN","accuracy":95,"efConstruction":100,"num_neighbors":16}"#,
        );
        let v = index(Some("hnsw"), &hnsw);
        assert_eq!(v.metric.as_deref(), Some("l2"));
        assert_eq!(v.params.get("m").map(String::as_str), Some("16"));
        assert_eq!(
            v.params.get("ef_construction").map(String::as_str),
            Some("100")
        );
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", &["\"V\"".into()], &v).unwrap(),
            "CREATE VECTOR INDEX \"IX\" ON \"T\" (\"V\") ORGANIZATION INMEMORY NEIGHBOR GRAPH \
             DISTANCE EUCLIDEAN WITH TARGET ACCURACY 95 PARAMETERS (TYPE HNSW, NEIGHBORS 16, EFCONSTRUCTION 100)"
        );
        let ivf = Details::from_params(
            r#"{"type":"IVF_FLAT","distance":"COSINE","accuracy":90,"target_centroids":2}"#,
        );
        let v = index(Some("ivf"), &ivf);
        assert_eq!(v.params.get("lists").map(String::as_str), Some("2"));
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", &["\"V\"".into(), "\"C\"".into()], &v).unwrap(),
            "CREATE VECTOR INDEX \"IX\" ON \"T\" (\"V\") INCLUDE (\"C\") ORGANIZATION NEIGHBOR PARTITIONS \
             DISTANCE COSINE WITH TARGET ACCURACY 90 PARAMETERS (TYPE IVF, NEIGHBOR PARTITIONS 2)"
        );
        // From the designer: standard names only, the rest the engine's.
        let d = VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some("ip".into()),
            ..Default::default()
        };
        assert_eq!(
            index_sql("\"T\"", "\"IX\"", &["\"V\"".into()], &d).unwrap(),
            "CREATE VECTOR INDEX \"IX\" ON \"T\" (\"V\") ORGANIZATION INMEMORY NEIGHBOR GRAPH DISTANCE DOT"
        );
        let bad = VectorIndex {
            algorithm: Some("diskann".into()),
            ..Default::default()
        };
        assert!(index_sql("t", "ix", &["v".into()], &bad).is_err());
        assert!(index_sql("t", "ix", &[], &d).is_err());
        assert_eq!(algorithm_of("INMEMORY_NEIGHBOR_GRAPH_HNSW"), Some("hnsw"));
        assert_eq!(algorithm_of("NEIGHBOR PARTITIONS"), Some("ivf"));
        assert_eq!(metric_of("MANHATTAN"), "l1");
        assert_eq!(native_metric("jaccard"), Some("JACCARD"));
    }
}
