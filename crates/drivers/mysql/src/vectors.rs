//! Vectors (embeddings) in the engines of this crate: their types, how
//! their cells come over the wire, their literals and their vector indexes.
//!
//! - **MySQL 9+**: `VECTOR(n)` (float32), sent as binary
//!   (`MYSQL_TYPE_VECTOR`); `STRING_TO_VECTOR('[…]')` literals. No vector
//!   index in MySQL Community (HeatWave only).
//! - **MariaDB 11.7+**: `VECTOR(n)` (float32), sent as a binary string the
//!   wire doesn't tell from `VARBINARY` (the session looks the column up);
//!   `VEC_FromText('[…]')`; `VECTOR INDEX (c) M=… DISTANCE=euclidean|cosine`.
//! - **TiDB 8.4+**: `VECTOR[(n)]`, sent as JSON-array text; `'[…]'` casts;
//!   `VECTOR INDEX ((VEC_COSINE_DISTANCE(c))) USING HNSW` (needs TiFlash).
//! - **OceanBase 4.3.3+**: `VECTOR(n)`; `'[…]'`; `VECTOR INDEX (c) WITH
//!   (distance=…, type=hnsw…, lib=vsag|ob, m=…)`.
//! - **SingleStore 8.5+**: `VECTOR(n[, F32|F16|F64|I8|I16|I32|I64])`, sent
//!   as JSON text by default; `VECTOR INDEX (c) INDEX_OPTIONS '{…}'`.
//! - **Doris 4.0+**: `ARRAY<FLOAT>` with an `ANN` index (`index_type`,
//!   `metric_type`, `dim`…); `[…]` array literals.
//! - **StarRocks 3.4+**: `ARRAY<FLOAT>` with a `VECTOR` index.
//! - **GreptimeDB**: `VECTOR(n)`, binary over the wire.
//! - **Manticore**: `float_vector` attributes, searched through the column's
//!   own KNN settings (`knn_type='hnsw' knn_dims='n' hnsw_similarity=…`),
//!   kept here as a vector index named after the column.
//! - **Databend**: `VECTOR(n)` and `VECTOR INDEX … distance='…'`.

use crate::Variant;
use dbine_driver::sql::{quote_ident, Quote};
use dbine_driver::vector::{self, metric, param};
use dbine_driver::{
    Error, IndexDef, Result, VectorElement, VectorIndex, VectorIndexSpec, VectorMeta,
    VectorParamSpec,
};
use std::collections::BTreeMap;

fn q(name: &str) -> String {
    quote_ident(Quote::Backtick, name)
}

/// The engine stores vectors and the driver handles them: every engine of
/// the crate (MySQL since 9.0), but not the managed MySQL services, which
/// run MySQL 8 (Aurora MySQL has no vector type; Cloud SQL's embeddings are
/// `VARBINARY` columns with its own functions and SCANN indexes).
pub(crate) fn supported(v: Variant) -> bool {
    !matches!(v, Variant::AuroraMySql | Variant::CloudSqlMySql)
}

/// The kind of a vector index as each engine reads it back.
pub(crate) fn kind(v: Variant) -> &'static str {
    match v.base() {
        Variant::Doris => "ANN",
        Variant::Manticore => "KNN",
        _ => "VECTOR",
    }
}

/// Types the designer offers, with an example dimension.
pub(crate) fn data_types(v: Variant) -> &'static [&'static str] {
    match v.base() {
        Variant::MySql | Variant::MariaDb | Variant::OceanBase => &["vector(1536)"],
        Variant::TiDb => &["vector(1536)", "vector"],
        Variant::SingleStore => &["vector(1536)", "vector(1536, F16)", "vector(1536, I8)"],
        Variant::StarRocks | Variant::Doris => &["ARRAY<FLOAT>"],
        Variant::GreptimeDb | Variant::Databend => &["VECTOR(1536)"],
        // `float_vector` is already in Manticore's list.
        _ => &[],
    }
}

fn element(s: &str) -> VectorElement {
    match s.trim().to_ascii_uppercase().as_str() {
        "F32" | "FLOAT" | "FLOAT32" => VectorElement::F32,
        "F64" | "DOUBLE" | "FLOAT64" => VectorElement::F64,
        "F16" | "FLOAT16" => VectorElement::F16,
        "I8" | "INT8" => VectorElement::I8,
        _ => VectorElement::Other,
    }
}

/// What a column type says about its vectors (`None`: not a vector type).
pub(crate) fn meta(v: Variant, data_type: &str) -> Option<VectorMeta> {
    let t = data_type.trim().to_ascii_lowercase();
    let base = v.base();
    if matches!(base, Variant::StarRocks | Variant::Doris) {
        let t: String = t.chars().filter(|c| !c.is_whitespace()).collect();
        return (t == "array<float>" || t.starts_with("array<float>not"))
            .then(|| VectorMeta::dense(None, VectorElement::F32));
    }
    if base == Variant::Manticore {
        return t
            .starts_with("float_vector")
            .then(|| VectorMeta::dense(None, VectorElement::F32));
    }
    let rest = t.strip_prefix("vector")?;
    // `vector<float>` (TiDB's long form) and `vector<float>(3)`.
    let (elem, rest) = match rest.strip_prefix('<') {
        Some(r) => {
            let (e, r) = r.split_once('>')?;
            (Some(element(e)), r)
        }
        None => (None, rest),
    };
    let rest = rest.trim_start();
    if !(rest.is_empty() || rest.starts_with('(') || rest.starts_with(' ')) {
        return None;
    }
    let args: Vec<&str> = match rest.strip_prefix('(') {
        Some(r) => r.split(')').next()?.split(',').map(str::trim).collect(),
        None => Vec::new(),
    };
    let dims = args.first().and_then(|d| d.parse().ok());
    let element = elem
        .or_else(|| args.get(1).map(|e| element(e)))
        .unwrap_or(VectorElement::F32);
    Some(VectorMeta::dense(dims, element))
}

/// The engine's full spelling of a vector column for a result header,
/// from its catalog type (`vector(3)`), kept as the catalog wrote it.
pub(crate) fn type_name(data_type: &str) -> String {
    data_type.trim().to_string()
}

/// A vector's numbers as the canonical text, written with the precision
/// of the column's elements (a float32 as its shortest float32 text).
pub(crate) fn text_of(meta: &VectorMeta, values: &[f64]) -> String {
    match meta.element {
        VectorElement::F32 | VectorElement::F16 | VectorElement::Bf16 => {
            vector::text(values.iter().map(|&x| x as f32))
        }
        _ => vector::text(values.iter().copied()),
    }
}

/// Numbers of a vector the engine wrote as text: a JSON array, Manticore's
/// `0.1,0.2` list or `(0.1,0.2)`.
fn parse_text(s: &str) -> Option<Vec<f64>> {
    let s = s.trim();
    if let Some(v) = vector::parse(s) {
        return Some(v);
    }
    let inner = s
        .strip_prefix('(')
        .and_then(|r| r.strip_suffix(')'))
        .unwrap_or(s);
    if inner.trim().is_empty() {
        return Some(Vec::new());
    }
    vector::parse(&format!("[{inner}]"))
}

/// A vector cell as the canonical JSON-array text: `binary` when the wire
/// sent the stored bytes (MySQL 9, MariaDB, GreptimeDB, SingleStore in
/// binary projection), else the engine's text. `None` when the value
/// doesn't read as a vector (the caller shows it as it came).
pub(crate) fn cell(meta: &VectorMeta, b: &[u8], binary: bool) -> Option<String> {
    if !binary {
        let values = parse_text(std::str::from_utf8(b).ok()?)?;
        return Some(text_of(meta, &values));
    }
    Some(match meta.element {
        VectorElement::F32 => vector::text(vector::from_f32_le(b)?),
        VectorElement::F64 => vector::text(vector::from_f64_le(b)?),
        VectorElement::F16 => vector::text(vector::from_f16_le(b)?),
        VectorElement::Bf16 => vector::text(vector::from_bf16_le(b)?),
        VectorElement::I8 => vector::text(vector::from_i8(b)),
        VectorElement::U8 => vector::text(vector::from_u8(b)),
        _ => return None,
    })
}

/// A vector value of an edit or insert as the engine's literal.
pub(crate) fn literal(v: Variant, values: &[f64]) -> String {
    let text = vector::text(values.iter().copied());
    match v.base() {
        Variant::MySql => format!("STRING_TO_VECTOR('{text}')"),
        Variant::MariaDb => format!("VEC_FromText('{text}')"),
        Variant::StarRocks | Variant::Doris | Variant::Databend => text,
        Variant::Manticore => format!("({})", &text[1..text.len() - 1]),
        _ => format!("'{text}'"),
    }
}

/// The value of an edit as a vector, when it's one (a JSON array of
/// numbers; see [`dbine_driver::vector`]).
pub(crate) fn edit_value(v: &serde_json::Value) -> Option<Vec<f64>> {
    match v {
        serde_json::Value::Array(_) => vector::parse_value(v),
        _ => None,
    }
}

// ------------------------------------------------------------- metrics

/// (standard, native) metric names per engine.
fn metrics(v: Variant) -> &'static [(&'static str, &'static str)] {
    match v.base() {
        Variant::MariaDb => &[(metric::L2, "euclidean"), (metric::COSINE, "cosine")],
        Variant::TiDb => &[
            (metric::COSINE, "VEC_COSINE_DISTANCE"),
            (metric::L2, "VEC_L2_DISTANCE"),
        ],
        Variant::OceanBase => &[
            (metric::L2, "L2"),
            (metric::IP, "INNER_PRODUCT"),
            (metric::COSINE, "COSINE"),
        ],
        Variant::SingleStore => &[
            (metric::IP, "DOT_PRODUCT"),
            (metric::L2, "EUCLIDEAN_DISTANCE"),
        ],
        Variant::Doris => &[(metric::L2, "l2_distance"), (metric::IP, "inner_product")],
        Variant::StarRocks => &[
            (metric::L2, "l2_distance"),
            (metric::COSINE, "cosine_similarity"),
        ],
        Variant::Manticore => &[
            (metric::L2, "L2"),
            (metric::IP, "IP"),
            (metric::COSINE, "COSINE"),
        ],
        Variant::Databend => &[
            (metric::COSINE, "cosine"),
            (metric::L2, "l2"),
            (metric::L1, "l1"),
        ],
        // Squared L2 orders like L2.
        Variant::GreptimeDb => &[
            (metric::L2, "l2sq"),
            (metric::COSINE, "cosine"),
            (metric::IP, "ip"),
        ],
        _ => &[],
    }
}

/// A native metric name in standard terms (or as the engine spells it,
/// lower case, when no standard name fits).
fn standard_metric(v: Variant, native: &str) -> String {
    metrics(v)
        .iter()
        .find(|(_, n)| n.eq_ignore_ascii_case(native.trim()))
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| native.trim().to_ascii_lowercase())
}

/// A metric (standard or native) as the engine writes it, or why it can't.
fn native_metric(v: Variant, m: &str) -> Result<&'static str> {
    metrics(v)
        .iter()
        .find(|(s, n)| s.eq_ignore_ascii_case(m) || n.eq_ignore_ascii_case(m))
        .map(|(_, n)| *n)
        .ok_or_else(|| {
            let have: Vec<&str> = metrics(v).iter().map(|(s, _)| *s).collect();
            Error::Unsupported(format!(
                "{} no tiene la distancia «{m}» en sus índices vectoriales (tiene: {})",
                engine(v),
                have.join(", ")
            ))
        })
}

fn engine(v: Variant) -> &'static str {
    match v.base() {
        Variant::MySql => "MySQL",
        Variant::MariaDb => "MariaDB",
        Variant::TiDb => "TiDB",
        Variant::OceanBase => "OceanBase",
        Variant::SingleStore => "SingleStore",
        Variant::StarRocks => "StarRocks",
        Variant::Doris => "Doris",
        Variant::Databend => "Databend",
        Variant::Manticore => "Manticore",
        Variant::GreptimeDb => "GreptimeDB",
        _ => "el motor",
    }
}

// ------------------------------------------------------------ parameters

/// (standard or own key, native) build-parameter names per engine.
fn params(v: Variant) -> &'static [(&'static str, &'static str)] {
    match v.base() {
        Variant::MariaDb => &[(param::M, "M")],
        Variant::OceanBase => &[
            (param::M, "m"),
            (param::EF_CONSTRUCTION, "ef_construction"),
            ("ef_search", "ef_search"),
            (param::LISTS, "nlist"),
            ("sample_per_nlist", "sample_per_nlist"),
            ("nbits", "nbits"),
            ("lib", "lib"),
        ],
        Variant::SingleStore => &[
            (param::M, "M"),
            (param::EF_CONSTRUCTION, "efConstruction"),
            ("ef", "ef"),
            (param::LISTS, "nlist"),
            ("nprobe", "nprobe"),
            ("nbits", "nbits"),
            ("m_pq", "m"),
        ],
        Variant::Doris => &[
            ("dim", "dim"),
            (param::M, "max_degree"),
            (param::EF_CONSTRUCTION, "ef_construction"),
            (param::LISTS, "nlist"),
            (param::QUANTIZATION, "quantizer"),
            ("pq_m", "pq_m"),
            ("pq_nbits", "pq_nbits"),
        ],
        Variant::StarRocks => &[
            ("dim", "dim"),
            (param::M, "M"),
            (param::EF_CONSTRUCTION, "efconstruction"),
            (param::LISTS, "nlist"),
            ("nbits", "nbits"),
            ("m_ivfpq", "M_IVFPQ"),
            ("is_vector_normed", "is_vector_normed"),
        ],
        Variant::Manticore => &[
            ("knn_dims", "knn_dims"),
            (param::M, "hnsw_m"),
            (param::EF_CONSTRUCTION, "hnsw_ef_construction"),
            (param::QUANTIZATION, "quantization"),
        ],
        Variant::Databend => &[(param::M, "m"), (param::EF_CONSTRUCTION, "ef_construct")],
        Variant::GreptimeDb => &[
            (param::M, "connectivity"),
            (param::EF_CONSTRUCTION, "expansion_add"),
            ("ef_search", "expansion_search"),
            ("engine", "engine"),
        ],
        _ => &[],
    }
}

fn standard_param(v: Variant, native: &str) -> String {
    params(v)
        .iter()
        .find(|(_, n)| *n == native)
        .or_else(|| {
            params(v)
                .iter()
                .find(|(_, n)| n.eq_ignore_ascii_case(native))
        })
        .map(|(s, _)| s.to_string())
        .unwrap_or_else(|| native.to_string())
}

fn native_param(v: Variant, key: &str) -> String {
    params(v)
        .iter()
        .find(|(s, _)| *s == key)
        .map(|(_, n)| n.to_string())
        .unwrap_or_else(|| key.to_string())
}

fn spec_param(
    key: &'static str,
    algorithms: &[&'static str],
    default: &'static str,
) -> VectorParamSpec {
    VectorParamSpec {
        key,
        algorithms: algorithms.to_vec(),
        default,
    }
}

/// What the designer offers for a vector index (`None`: the engine has
/// none; MySQL's are HeatWave's only).
pub(crate) fn designer(v: Variant) -> Option<VectorIndexSpec> {
    let metric_names = |v| metrics(v).iter().map(|(s, _)| *s).collect::<Vec<_>>();
    Some(match v.base() {
        Variant::MariaDb => VectorIndexSpec {
            algorithms: Vec::new(),
            metrics: metric_names(v),
            params: vec![spec_param(param::M, &[], "6")],
            filter_columns: false,
        },
        Variant::TiDb => VectorIndexSpec {
            algorithms: vec!["hnsw"],
            metrics: metric_names(v),
            params: Vec::new(),
            filter_columns: false,
        },
        Variant::OceanBase => {
            let hnsw = &["hnsw", "hnsw_sq", "hnsw_bq"];
            let ivf = &["ivf_flat", "ivf_sq8", "ivf_pq"];
            VectorIndexSpec {
                algorithms: vec![
                    "hnsw", "hnsw_sq", "hnsw_bq", "ivf_flat", "ivf_sq8", "ivf_pq",
                ],
                metrics: metric_names(v),
                params: vec![
                    spec_param(param::M, hnsw, "16"),
                    spec_param(param::EF_CONSTRUCTION, hnsw, "200"),
                    spec_param("ef_search", hnsw, "64"),
                    spec_param(param::LISTS, ivf, "128"),
                    spec_param("sample_per_nlist", ivf, "256"),
                ],
                filter_columns: false,
            }
        }
        Variant::SingleStore => {
            let hnsw = &["hnsw_flat", "hnsw_pq"];
            let ivf = &["ivf_flat", "ivf_pq", "ivf_pqfs"];
            VectorIndexSpec {
                algorithms: vec![
                    "auto",
                    "hnsw_flat",
                    "hnsw_pq",
                    "ivf_flat",
                    "ivf_pq",
                    "ivf_pqfs",
                    "flat",
                ],
                metrics: metric_names(v),
                params: vec![
                    spec_param(param::M, hnsw, "30"),
                    spec_param(param::EF_CONSTRUCTION, hnsw, "40"),
                    spec_param(param::LISTS, ivf, "128"),
                    spec_param("nprobe", ivf, "8"),
                ],
                filter_columns: false,
            }
        }
        Variant::Doris => VectorIndexSpec {
            algorithms: vec!["hnsw", "ivf"],
            metrics: metric_names(v),
            params: vec![
                spec_param("dim", &[], ""),
                spec_param(param::M, &["hnsw"], "32"),
                spec_param(param::EF_CONSTRUCTION, &["hnsw"], "40"),
                spec_param(param::LISTS, &["ivf"], "1024"),
                spec_param(param::QUANTIZATION, &[], "flat"),
            ],
            filter_columns: false,
        },
        Variant::StarRocks => VectorIndexSpec {
            algorithms: vec!["hnsw", "ivfpq"],
            metrics: metric_names(v),
            params: vec![
                spec_param("dim", &[], ""),
                spec_param(param::M, &["hnsw"], "16"),
                spec_param(param::EF_CONSTRUCTION, &["hnsw"], "40"),
                spec_param(param::LISTS, &["ivfpq"], "16"),
                spec_param("nbits", &["ivfpq"], "8"),
                spec_param("m_ivfpq", &["ivfpq"], ""),
                spec_param("is_vector_normed", &[], "false"),
            ],
            filter_columns: false,
        },
        Variant::Manticore => VectorIndexSpec {
            algorithms: vec!["hnsw"],
            metrics: metric_names(v),
            params: vec![
                spec_param("knn_dims", &[], ""),
                spec_param(param::M, &[], "16"),
                spec_param(param::EF_CONSTRUCTION, &[], "200"),
            ],
            filter_columns: false,
        },
        Variant::GreptimeDb => VectorIndexSpec {
            algorithms: Vec::new(),
            metrics: metric_names(v),
            params: vec![
                spec_param(param::M, &[], "16"),
                spec_param(param::EF_CONSTRUCTION, &[], "128"),
                spec_param("ef_search", &[], "64"),
            ],
            filter_columns: false,
        },
        Variant::Databend => VectorIndexSpec {
            algorithms: vec!["hnsw"],
            metrics: metric_names(v),
            params: vec![
                spec_param(param::M, &[], "16"),
                spec_param(param::EF_CONSTRUCTION, &[], "100"),
            ],
            filter_columns: false,
        },
        _ => return None,
    })
}

/// The index's vector settings, or the reason it can't be one: a vector
/// index has exactly one column (the vector) here.
fn vector_of(v: Variant, ix: &IndexDef) -> Result<(&VectorIndex, &str)> {
    let vi = ix
        .vector
        .as_ref()
        .ok_or_else(|| Error::Query(format!("«{}» no es un índice vectorial", ix.name)))?;
    match ix.columns.as_slice() {
        [c] => Ok((vi, c.as_str())),
        _ => Err(Error::Unsupported(format!(
            "{}: un índice vectorial lleva una sola columna, la del vector (índice «{}»)",
            engine(v),
            ix.name
        ))),
    }
}

/// The index's build parameters the engine has, in its names (another
/// engine's tuning, from a conversion, is left out).
fn params_text(v: Variant, vi: &VectorIndex, skip: &[&str]) -> Vec<(String, String)> {
    vi.params
        .iter()
        .filter(|(k, _)| !skip.contains(&k.as_str()))
        .filter(|(k, _)| params(v).iter().any(|(s, n)| s == k || n == k))
        .map(|(k, val)| (native_param(v, k), val.clone()))
        .collect()
}

/// A vector index as the statement that creates it on the MySQL servers
/// and the engines with their own CREATE (MariaDB, TiDB, OceanBase,
/// SingleStore, Databend). `table` is quoted.
pub(crate) fn create_index(
    v: Variant,
    table: &str,
    ix: &IndexDef,
    if_exists: bool,
) -> Result<String> {
    let (vi, col) = vector_of(v, ix)?;
    let name = q(&ix.name);
    let col_q = q(col);
    let metric = vi.metric.as_deref().filter(|m| !m.is_empty());
    match v.base() {
        Variant::MariaDb => {
            let guard = if if_exists { "IF NOT EXISTS " } else { "" };
            let mut s = format!("CREATE VECTOR INDEX {guard}{name} ON {table} ({col_q})");
            for (_, val) in params_text(v, vi, &[]) {
                s.push_str(&format!(" M={}", number(&val)?));
            }
            if let Some(m) = metric {
                s.push_str(&format!(" DISTANCE={}", native_metric(v, m)?));
            }
            Ok(format!("{s};"))
        }
        Variant::TiDb => {
            if let Some(a) = vi
                .algorithm
                .as_deref()
                .filter(|a| !a.eq_ignore_ascii_case("hnsw"))
            {
                return Err(Error::Unsupported(format!(
                    "TiDB solo tiene índices vectoriales HNSW (no «{a}»)"
                )));
            }
            let f = native_metric(v, metric.unwrap_or(metric::COSINE))?;
            Ok(format!(
                "CREATE VECTOR INDEX {name} ON {table} (({f}({col_q}))) USING HNSW;"
            ))
        }
        Variant::OceanBase => {
            let algorithm = vi
                .algorithm
                .as_deref()
                .unwrap_or("hnsw")
                .to_ascii_lowercase();
            let lib = if algorithm.starts_with("ivf") {
                "ob"
            } else {
                "vsag"
            };
            let mut with = vec![
                format!(
                    "distance={}",
                    native_metric(v, metric.unwrap_or(metric::L2))?
                ),
                format!("type={algorithm}"),
            ];
            if !vi.params.contains_key("lib") {
                with.push(format!("lib={lib}"));
            }
            for (k, val) in params_text(v, vi, &[]) {
                with.push(format!("{k}={}", word(&val)?));
            }
            Ok(format!(
                "CREATE VECTOR INDEX {name} ON {table} ({col_q}) WITH ({});",
                with.join(", ")
            ))
        }
        Variant::SingleStore => {
            // The JSON written in this order: type, metric, parameters.
            let mut o: Vec<(String, serde_json::Value)> = Vec::new();
            if let Some(a) = vi.algorithm.as_deref() {
                o.push(("index_type".into(), a.to_ascii_uppercase().into()));
            }
            if let Some(m) = metric {
                o.push(("metric_type".into(), native_metric(v, m)?.into()));
            }
            for (k, val) in params_text(v, vi, &[]) {
                let j = val
                    .parse::<i64>()
                    .map(serde_json::Value::from)
                    .unwrap_or_else(|_| val.clone().into());
                o.push((k, j));
            }
            let opts = if o.is_empty() {
                String::new()
            } else {
                let json = o
                    .iter()
                    .map(|(k, j)| format!("{}:{j}", serde_json::Value::from(k.as_str())))
                    .collect::<Vec<_>>()
                    .join(",");
                format!(
                    " INDEX_OPTIONS '{{{}}}'",
                    json.replace('\\', "\\\\").replace('\'', "''")
                )
            };
            Ok(format!(
                "ALTER TABLE {table} ADD VECTOR INDEX {name} ({col_q}){opts};"
            ))
        }
        Variant::Databend => {
            let guard = if if_exists { "IF NOT EXISTS " } else { "" };
            let mut s = format!("CREATE VECTOR INDEX {guard}{name} ON {table}({col_q})");
            for (k, val) in params_text(v, vi, &[]) {
                s.push_str(&format!(" {k}={}", number(&val)?));
            }
            let m = native_metric(v, metric.unwrap_or(metric::COSINE))?;
            s.push_str(&format!(" distance='{m}'"));
            Ok(format!("{s};"))
        }
        Variant::MySql => Err(Error::Unsupported(format!(
            "MySQL no tiene índices vectoriales (solo HeatWave): el índice «{}» no se puede crear",
            ix.name
        ))),
        _ => Err(Error::Unsupported(format!(
            "{} no tiene índices vectoriales (índice «{}»)",
            engine(v),
            ix.name
        ))),
    }
}

/// A number for an unquoted option, refused when it isn't one (it's
/// written into the statement as is).
fn number(s: &str) -> Result<String> {
    let s = s.trim();
    if !s.is_empty() && s.bytes().all(|b| b.is_ascii_digit() || b == b'.') {
        Ok(s.to_string())
    } else {
        Err(Error::Query(format!(
            "«{s}» no es un número válido para un parámetro del índice vectorial"
        )))
    }
}

/// A bare word or number for an unquoted option.
fn word(s: &str) -> Result<String> {
    let s = s.trim();
    if !s.is_empty()
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || c == '_' || c == '.')
    {
        Ok(s.to_string())
    } else {
        Err(Error::Query(format!(
            "«{s}» no es un valor válido para un parámetro del índice vectorial"
        )))
    }
}

/// Doris / StarRocks: the index type and its properties (`USING ANN
/// PROPERTIES(…)`, `USING VECTOR (…)`), native names, in the order the
/// engine prints them.
pub(crate) fn olap_index(v: Variant, ix: &IndexDef) -> Result<(String, Vec<(String, String)>)> {
    let (vi, _) = vector_of(v, ix)?;
    let mut props = Vec::new();
    let algorithm = vi
        .algorithm
        .as_deref()
        .unwrap_or("hnsw")
        .to_ascii_lowercase();
    props.push(("index_type".to_string(), algorithm));
    let m = vi.metric.as_deref().unwrap_or(metric::L2);
    props.push(("metric_type".to_string(), native_metric(v, m)?.to_string()));
    props.extend(params_text(v, vi, &[]));
    if !props.iter().any(|(k, _)| k == "dim") {
        return Err(Error::Unsupported(format!(
            "{}: el índice vectorial «{}» necesita la dimensión de los vectores (parámetro dim)",
            engine(v),
            ix.name
        )));
    }
    if v.base() == Variant::StarRocks && !props.iter().any(|(k, _)| k == "is_vector_normed") {
        props.push(("is_vector_normed".into(), "false".into()));
    }
    Ok((kind(v).to_string(), props))
}

/// Manticore: the column's KNN settings (`knn_type='hnsw' knn_dims='3'
/// hnsw_similarity='L2' …`), from its vector index.
pub(crate) fn manticore_knn(ix: &IndexDef) -> Result<String> {
    let v = Variant::Manticore;
    let (vi, _) = vector_of(v, ix)?;
    let algorithm = vi.algorithm.as_deref().unwrap_or("hnsw");
    let dims = vi.params.get("knn_dims").ok_or_else(|| {
        Error::Unsupported(format!(
            "Manticore: el índice KNN «{}» necesita la dimensión de los vectores (knn_dims)",
            ix.name
        ))
    })?;
    let mut s = format!(
        "knn_type='{}' knn_dims='{}' hnsw_similarity='{}'",
        word(algorithm)?,
        number(dims)?,
        native_metric(v, vi.metric.as_deref().unwrap_or(metric::L2))?
    );
    for (k, val) in params_text(v, vi, &["knn_dims"]) {
        s.push_str(&format!(" {k}='{}'", word(&val)?));
    }
    Ok(s)
}

// ------------------------------------------------------------- reading

fn unquote(s: &str) -> String {
    let s = s.trim();
    for q in ['\'', '"', '`'] {
        if s.len() >= 2 && s.starts_with(q) && s.ends_with(q) {
            return s[1..s.len() - 1].replace(&format!("{q}{q}"), &q.to_string());
        }
    }
    s.to_string()
}

/// A leading backtick-quoted identifier and what follows it.
fn ident(s: &str) -> Option<(String, &str)> {
    let s = s.trim_start();
    if let Some(r) = s.strip_prefix('`') {
        let mut out = String::new();
        let mut chars = r.char_indices().peekable();
        while let Some((i, c)) = chars.next() {
            if c == '`' {
                if chars.peek().map(|(_, n)| *n) == Some('`') {
                    chars.next();
                    out.push('`');
                    continue;
                }
                return Some((out, &r[i + 1..]));
            }
            out.push(c);
        }
        return None;
    }
    let end = s
        .find(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
        .unwrap_or(s.len());
    (end > 0).then(|| (s[..end].to_string(), &s[end..]))
}

/// The vector indexes of a `SHOW CREATE TABLE`, as `(name, column, vector,
/// native options)`: MariaDB's `VECTOR KEY `v` (`v`) `M`='8'
/// `DISTANCE`='cosine'`, TiDB's `VECTOR INDEX `i`((VEC_COSINE_DISTANCE(`v`)))`,
/// OceanBase's `VECTOR KEY `i` (`v`) WITH (DISTANCE=L2, TYPE=HNSW, LIB=VSAG)`,
/// SingleStore's `VECTOR INDEX `i` (`v`) INDEX_OPTIONS='{…}'`.
pub(crate) fn from_create(v: Variant, create: &str) -> Vec<IndexDef> {
    let mut out = Vec::new();
    for l in create.lines() {
        let l = l.trim().trim_end_matches(',');
        let upper = l.to_ascii_uppercase();
        let Some(rest) = ["VECTOR KEY ", "VECTOR INDEX "]
            .iter()
            .find(|p| upper.starts_with(**p))
            .map(|p| &l[p.len()..])
        else {
            continue;
        };
        let Some((name, rest)) = ident(rest) else {
            continue;
        };
        let rest = rest.trim_start();
        let Some(body) = rest
            .starts_with('(')
            .then(|| crate::structure::inside(rest, 0))
            .flatten()
        else {
            continue;
        };
        let after = rest[body.len() + 2..].trim();
        let mut vi = VectorIndex::default();
        let mut options = BTreeMap::new();
        let column;
        if v.base() == Variant::TiDb {
            // ((VEC_COSINE_DISTANCE(`v`))): the function names the metric.
            let inner = body.trim().trim_start_matches('(').trim_end_matches(')');
            let Some((f, arg)) = inner.split_once('(') else {
                continue;
            };
            let Some((c, _)) = ident(arg) else { continue };
            column = c;
            vi.algorithm = Some("hnsw".into());
            vi.metric = Some(standard_metric(v, f));
            options.insert("DISTANCE".into(), f.trim().to_ascii_uppercase());
        } else {
            let Some((c, _)) = ident(body) else { continue };
            column = c;
            let after_u = after.to_ascii_uppercase();
            if let Some(at) = after_u.find("INDEX_OPTIONS") {
                // SingleStore: a JSON object, quoted.
                let json = after[at + "INDEX_OPTIONS".len()..]
                    .trim_start()
                    .trim_start_matches('=')
                    .trim();
                let json = unquote(json.split_once(" COMMENT ").map_or(json, |(j, _)| j));
                options.insert("INDEX_OPTIONS".into(), json.clone());
                if let Ok(serde_json::Value::Object(o)) = serde_json::from_str(&json) {
                    for (k, val) in o {
                        let val = match val {
                            serde_json::Value::String(s) => s,
                            other => other.to_string(),
                        };
                        match k.as_str() {
                            "index_type" => vi.algorithm = Some(val.to_ascii_lowercase()),
                            "metric_type" => vi.metric = Some(standard_metric(v, &val)),
                            _ => {
                                vi.params.insert(standard_param(v, &k), val);
                            }
                        }
                    }
                }
            } else if let Some(at) = after_u.find("WITH") {
                // OceanBase: WITH (DISTANCE=L2, TYPE=HNSW, LIB=VSAG, …).
                let w = after[at + 4..].trim_start();
                let kv = if w.starts_with('(') {
                    crate::structure::inside(w, 0).unwrap_or("")
                } else {
                    w
                };
                for part in crate::structure::split_top(kv) {
                    let Some((k, val)) = part.split_once('=') else {
                        continue;
                    };
                    let (k, val) = (unquote(k).to_ascii_lowercase(), unquote(val));
                    options.insert(k.clone(), val.clone());
                    match k.as_str() {
                        "distance" => vi.metric = Some(standard_metric(v, &val)),
                        "type" => vi.algorithm = Some(val.to_ascii_lowercase()),
                        "lib" => {}
                        _ => {
                            vi.params
                                .insert(standard_param(v, &k), val.to_ascii_lowercase());
                        }
                    }
                }
            } else {
                // MariaDB: `M`='8' `DISTANCE`='cosine', space separated.
                let mut rest = after;
                while let Some((k, r)) = ident(rest) {
                    let r = r.trim_start();
                    let Some(r) = r.strip_prefix('=') else { break };
                    let r = r.trim_start();
                    let (val, r) = if let Some(q) = r.strip_prefix('\'') {
                        match q.find('\'') {
                            Some(e) => (q[..e].to_string(), &q[e + 1..]),
                            None => break,
                        }
                    } else {
                        let e = r.find(' ').unwrap_or(r.len());
                        (r[..e].to_string(), &r[e..])
                    };
                    let ku = k.to_ascii_uppercase();
                    options.insert(ku.clone(), val.clone());
                    match ku.as_str() {
                        "DISTANCE" => vi.metric = Some(standard_metric(v, &val)),
                        "M" => {
                            vi.params.insert(param::M.into(), val);
                        }
                        // COMMENT and the like aren't vector settings.
                        _ => {
                            options.remove(&ku);
                        }
                    }
                    rest = r;
                }
            }
        }
        out.push(IndexDef {
            name,
            columns: vec![column],
            kind: Some(kind(v).into()),
            options,
            vector: Some(vi),
            ..Default::default()
        });
    }
    out
}

/// A Doris / StarRocks index read from SHOW INDEX (kind and properties,
/// native names) as a vector index, when it is one: the properties stay
/// in `options` (they're what compare tells apart).
pub(crate) fn from_olap(v: Variant, ix: &mut IndexDef) {
    let is_vector = ix.kind.as_deref().is_some_and(|k| {
        let k = k.trim();
        (v.base() == Variant::Doris && k.eq_ignore_ascii_case("ANN"))
            || (v.base() == Variant::StarRocks && k.eq_ignore_ascii_case("VECTOR"))
    });
    if !is_vector {
        return;
    }
    let mut vi = VectorIndex::default();
    for (k, val) in &ix.options {
        match k.to_ascii_lowercase().as_str() {
            "index_type" => vi.algorithm = Some(val.to_ascii_lowercase()),
            "metric_type" => vi.metric = Some(standard_metric(v, val)),
            "comment" => {}
            _ => {
                vi.params.insert(standard_param(v, k), val.clone());
            }
        }
    }
    ix.kind = Some(kind(v).into());
    ix.vector = Some(vi);
}

/// A Databend index from `system.indexes` as a vector index, when its type
/// is `VECTOR` (`distance='cosine,l2' m='16' ef_construct='100'`): the first
/// distance is its metric, the options stay as read.
pub(crate) fn from_databend(ix: &mut IndexDef) {
    let v = Variant::Databend;
    if !ix
        .kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case("VECTOR"))
    {
        return;
    }
    let mut vi = VectorIndex {
        algorithm: Some("hnsw".into()),
        ..Default::default()
    };
    for (k, val) in &ix.options {
        match k.to_ascii_lowercase().as_str() {
            "distance" => {
                vi.metric = val
                    .split(',')
                    .map(str::trim)
                    .find(|m| !m.is_empty())
                    .map(|m| standard_metric(v, m));
            }
            _ => {
                vi.params.insert(standard_param(v, k), val.clone());
            }
        }
    }
    ix.kind = Some(kind(v).into());
    ix.vector = Some(vi);
}

/// GreptimeDB: a column's `VECTOR INDEX WITH(metric = '…', connectivity =
/// '…', …)`, written in its CREATE TABLE line (ALTER doesn't add one).
pub(crate) fn greptime_column_index(ix: &IndexDef) -> Result<String> {
    let v = Variant::GreptimeDb;
    let (vi, _) = vector_of(v, ix)?;
    let mut with = Vec::new();
    if let Some(m) = vi.metric.as_deref() {
        with.push(format!("metric = '{}'", native_metric(v, m)?));
    }
    for (k, val) in params_text(v, vi, &[]) {
        with.push(format!("{k} = '{}'", word(&val)?));
    }
    Ok(if with.is_empty() {
        " VECTOR INDEX".into()
    } else {
        format!(" VECTOR INDEX WITH({})", with.join(", "))
    })
}

/// A GreptimeDB vector index read from its column's `WITH(…)` options.
pub(crate) fn greptime_index(column: &str, options: BTreeMap<String, String>) -> IndexDef {
    let v = Variant::GreptimeDb;
    let mut vi = VectorIndex::default();
    for (k, val) in &options {
        match k.as_str() {
            "metric" => vi.metric = Some(standard_metric(v, val)),
            // usearch's HNSW, the one engine: its defaults aren't settings.
            "engine" => {}
            _ => {
                vi.params.insert(standard_param(v, k), val.clone());
            }
        }
    }
    IndexDef {
        name: format!("VECTOR_INDEX_{column}"),
        columns: vec![column.to_string()],
        kind: Some(kind(v).into()),
        options,
        vector: Some(vi),
        ..Default::default()
    }
}

/// Manticore's KNN settings of each `float_vector` column in its `SHOW
/// CREATE TABLE` (`v float_vector knn_type='hnsw' knn_dims='3'
/// hnsw_similarity='L2'`), as vector indexes named after the column.
pub(crate) fn manticore_indexes(create: &str) -> Vec<IndexDef> {
    let v = Variant::Manticore;
    let mut out = Vec::new();
    let Some(open) = create.find('(') else {
        return out;
    };
    let Some(body) = crate::structure::inside(create, open) else {
        return out;
    };
    for col in crate::structure::split_top(body) {
        let Some((name, rest)) = ident(&col) else {
            continue;
        };
        let rest = rest.trim_start();
        if !rest.to_ascii_lowercase().starts_with("float_vector") {
            continue;
        }
        let settings =
            crate::structure::key_values(&rest["float_vector".len()..].replace("' ", "', "));
        if !settings.keys().any(|k| k.eq_ignore_ascii_case("knn_type")) {
            continue;
        }
        let mut vi = VectorIndex::default();
        let mut options = BTreeMap::new();
        for (k, val) in settings {
            let k = k.to_ascii_lowercase();
            options.insert(k.clone(), val.clone());
            match k.as_str() {
                "knn_type" => vi.algorithm = Some(val.to_ascii_lowercase()),
                "hnsw_similarity" => vi.metric = Some(standard_metric(v, &val)),
                _ => {
                    vi.params.insert(standard_param(v, &k), val);
                }
            }
        }
        out.push(IndexDef {
            name: name.clone(),
            columns: vec![name],
            kind: Some(kind(v).into()),
            options,
            vector: Some(vi),
            ..Default::default()
        });
    }
    out
}

/// `float_vector knn_type=…` as the bare type (the KNN settings are the
/// column's vector index).
pub(crate) fn manticore_base_type(ty: &str) -> &str {
    let t = ty.trim();
    if t.len() >= 12 && t[..12].eq_ignore_ascii_case("float_vector") {
        &t[..12]
    } else {
        t
    }
}

// ------------------------------------------------------------- results

/// How long a schema's vector columns are trusted before they're looked
/// up again (another session may have changed the table).
const SEEN_FOR: std::time::Duration = std::time::Duration::from_secs(60);

/// The vector columns of the schemas a session looked up, for the engines
/// whose wire doesn't tell a vector column apart (MariaDB sends a binary
/// string like `VARBINARY`'s, TiDB and SingleStore text, the OLAP engines
/// their arrays as text): (table, column in lower case) → catalog type.
#[derive(Debug, Default)]
pub(crate) struct SeenColumns {
    schemas: std::collections::HashMap<String, (std::time::Instant, SchemaColumns)>,
}

pub(crate) type SchemaColumns = std::collections::HashMap<(String, String), String>;

/// What a result column is, as far as the session knows.
#[derive(Debug, Clone, PartialEq)]
pub(crate) enum Known {
    /// A vector column, with its catalog type.
    Vector(String),
    NotVector,
    /// Its schema hasn't been looked up (or not lately).
    Unknown,
}

impl SeenColumns {
    pub(crate) fn fresh(&self, schema: &str) -> bool {
        self.schemas
            .get(schema)
            .is_some_and(|(at, _)| at.elapsed() < SEEN_FOR)
    }

    pub(crate) fn known(&self, schema: &str, table: &str, column: &str) -> Known {
        match self.schemas.get(schema) {
            Some((at, cols)) if at.elapsed() < SEEN_FOR => {
                match cols.get(&(table.to_string(), column.to_ascii_lowercase())) {
                    Some(t) => Known::Vector(t.clone()),
                    None => Known::NotVector,
                }
            }
            _ => Known::Unknown,
        }
    }

    pub(crate) fn insert(&mut self, schema: &str, cols: SchemaColumns) {
        self.schemas
            .insert(schema.to_string(), (std::time::Instant::now(), cols));
    }

    /// After a DDL: the tables may have changed.
    pub(crate) fn clear(&mut self) {
        self.schemas.clear();
    }
}

/// Engines whose result columns are found to be vectors through the
/// catalog: their wire type is a string or a blob (MySQL 9's has a type of
/// its own).
pub(crate) fn looked_up(v: Variant) -> bool {
    v.base() != Variant::MySql
}

/// The table a single-table `SELECT` reads (`[schema.]table`), for the
/// engines whose result columns don't say which table they come from
/// (GreptimeDB, Manticore). `None` for anything else: joins, several
/// tables, subqueries, other statements.
pub(crate) fn single_table(sql: &str) -> Option<(Option<String>, String)> {
    let s = sql.replace(['\n', '\r', '\t'], " ");
    let s = s.trim().trim_end_matches(';').trim();
    let lower = s.to_ascii_lowercase();
    if !lower.starts_with("select") || s.contains(';') || lower.contains(" join ") {
        return None;
    }
    let at = lower.find(" from ")? + " from ".len();
    if lower[at..].contains(" from ") {
        return None;
    }
    let (first, rest) = ident(&s[at..])?;
    let (schema, table, rest) = match rest.strip_prefix('.') {
        Some(r) => {
            let (t, r) = ident(r)?;
            (Some(first), t, r)
        }
        None => (None, first, rest),
    };
    let rest = rest.trim_start();
    // `FROM t, u`, `FROM t AS x, u`: more than one table.
    if rest.starts_with(',')
        || rest.starts_with('(')
        || rest.contains(',') && {
            let r = rest.to_ascii_lowercase();
            let end = ["where", "group", "order", "limit", "option", "having"]
                .iter()
                .filter_map(|k| r.find(&format!("{k} ")).or_else(|| r.find(k)))
                .min()
                .unwrap_or(r.len());
            r[..end].contains(',')
        }
    {
        return None;
    }
    Some((schema, table))
}

/// The catalog query for a schema's vector columns (table, column, type):
/// filtered further with [`meta`].
pub(crate) fn columns_query(schema_literal: &str) -> String {
    format!(
        "SELECT TABLE_NAME, COLUMN_NAME, COLUMN_TYPE FROM information_schema.COLUMNS \
         WHERE TABLE_SCHEMA = {schema_literal} AND (LOWER(COLUMN_TYPE) LIKE 'vector%' \
         OR LOWER(COLUMN_TYPE) LIKE 'array<float>%' OR LOWER(DATA_TYPE) LIKE 'vector%')"
    )
}

/// A statement that may change a table's columns: what the session saw
/// is looked up again.
pub(crate) fn may_change_columns(sql: &str) -> bool {
    sql.split(';').any(|st| {
        st.split_whitespace().next().is_some_and(|w| {
            ["create", "alter", "drop", "rename"]
                .iter()
                .any(|k| w.eq_ignore_ascii_case(k))
        })
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn vix(
        name: &str,
        col: &str,
        algorithm: Option<&str>,
        m: Option<&str>,
        p: &[(&str, &str)],
    ) -> IndexDef {
        IndexDef {
            name: name.into(),
            columns: vec![col.into()],
            vector: Some(VectorIndex {
                algorithm: algorithm.map(Into::into),
                metric: m.map(Into::into),
                params: p
                    .iter()
                    .map(|(k, v)| (k.to_string(), v.to_string()))
                    .collect(),
            }),
            ..Default::default()
        }
    }

    #[test]
    fn types_tell_their_dimensions_and_elements() {
        let f32 = VectorElement::F32;
        assert_eq!(
            meta(Variant::MySql, "vector(3)"),
            Some(VectorMeta::dense(Some(3), f32))
        );
        assert_eq!(
            meta(Variant::MariaDb, "VECTOR(1536)"),
            Some(VectorMeta::dense(Some(1536), f32))
        );
        assert_eq!(
            meta(Variant::TiDb, "vector"),
            Some(VectorMeta::dense(None, f32))
        );
        assert_eq!(
            meta(Variant::TiDb, "vector<float>(4)"),
            Some(VectorMeta::dense(Some(4), f32))
        );
        assert_eq!(
            meta(Variant::SingleStore, "vector(8, F16)"),
            Some(VectorMeta::dense(Some(8), VectorElement::F16))
        );
        assert_eq!(
            meta(Variant::SingleStore, "vector(8,I8)"),
            Some(VectorMeta::dense(Some(8), VectorElement::I8))
        );
        assert_eq!(
            meta(Variant::SingleStore, "vector(8, I64)"),
            Some(VectorMeta::dense(Some(8), VectorElement::Other))
        );
        assert_eq!(
            meta(Variant::GreptimeDb, "Vector(3)"),
            Some(VectorMeta::dense(Some(3), f32))
        );
        assert_eq!(
            meta(Variant::Doris, "array<float>"),
            Some(VectorMeta::dense(None, f32))
        );
        assert_eq!(
            meta(Variant::StarRocks, "ARRAY<FLOAT>"),
            Some(VectorMeta::dense(None, f32))
        );
        assert_eq!(meta(Variant::Doris, "array<int>"), None);
        assert_eq!(meta(Variant::MySql, "array<float>"), None);
        assert_eq!(
            meta(Variant::Manticore, "float_vector"),
            Some(VectorMeta::dense(None, f32))
        );
        for not in ["varchar(10)", "vectors", "vectorx(3)", "blob"] {
            assert_eq!(meta(Variant::MySql, not), None, "{not}");
        }
        // Aurora / Cloud SQL go as MySQL.
        assert!(meta(Variant::CloudSqlMySql, "vector(2)").is_some());
    }

    #[test]
    fn cells_decode_binary_and_text_whole() {
        let m = VectorMeta::dense(Some(3), VectorElement::F32);
        let b = vector::to_f32_le(&[1.0, 2.5, -3.0]);
        assert_eq!(cell(&m, &b, true).as_deref(), Some("[1,2.5,-3]"));
        // float32 values written as their shortest float32 text.
        let b = vector::to_f32_le(&[0.1, 1e-7]);
        assert_eq!(cell(&m, &b, true).as_deref(), Some("[0.1,0.0000001]"));
        assert_eq!(cell(&m, &[0; 5], true), None);
        // A big one comes whole (never cut like binary).
        let big: Vec<f64> = (0..1536).map(|i| i as f64 / 8.0).collect();
        let t = cell(&m, &vector::to_f32_le(&big), true).unwrap();
        assert_eq!(vector::parse(&t).unwrap().len(), 1536);
        // Text from the engine, normalised.
        assert_eq!(
            cell(&m, b"[1, 2.50, -3]", false).as_deref(),
            Some("[1,2.5,-3]")
        );
        assert_eq!(cell(&m, b"[0.1,0.2]", false).as_deref(), Some("[0.1,0.2]"));
        assert_eq!(
            cell(&m, b"0.100000,0.200000", false).as_deref(),
            Some("[0.1,0.2]")
        );
        assert_eq!(cell(&m, b"(1,2)", false).as_deref(), Some("[1,2]"));
        assert_eq!(cell(&m, b"[]", false).as_deref(), Some("[]"));
        assert_eq!(cell(&m, b"hello", false), None);
        let h = VectorMeta::dense(Some(2), VectorElement::F16);
        assert_eq!(
            cell(&h, &vector::to_f16_le(&[1.0, -0.5]), true).as_deref(),
            Some("[1,-0.5]")
        );
        let i = VectorMeta::dense(Some(2), VectorElement::I8);
        assert_eq!(
            cell(&i, &vector::to_i8(&[-3.0, 7.0]), true).as_deref(),
            Some("[-3,7]")
        );
    }

    #[test]
    fn literals_per_engine() {
        let v = [1.0, 2.5, -3.0];
        assert_eq!(
            literal(Variant::MySql, &v),
            "STRING_TO_VECTOR('[1,2.5,-3]')"
        );
        assert_eq!(
            literal(Variant::AuroraMySql, &v),
            "STRING_TO_VECTOR('[1,2.5,-3]')"
        );
        assert_eq!(literal(Variant::MariaDb, &v), "VEC_FromText('[1,2.5,-3]')");
        assert_eq!(literal(Variant::TiDb, &v), "'[1,2.5,-3]'");
        assert_eq!(literal(Variant::OceanBase, &v), "'[1,2.5,-3]'");
        assert_eq!(literal(Variant::SingleStore, &v), "'[1,2.5,-3]'");
        assert_eq!(literal(Variant::GreptimeDb, &v), "'[1,2.5,-3]'");
        assert_eq!(literal(Variant::Doris, &v), "[1,2.5,-3]");
        assert_eq!(literal(Variant::StarRocks, &v), "[1,2.5,-3]");
        assert_eq!(literal(Variant::Databend, &v), "[1,2.5,-3]");
        assert_eq!(literal(Variant::Manticore, &v), "(1,2.5,-3)");
        assert_eq!(edit_value(&json!([1, 2.5])), Some(vec![1.0, 2.5]));
        assert_eq!(edit_value(&json!("[1,2]")), None);
        assert_eq!(edit_value(&json!(["a"])), None);
    }

    #[test]
    fn mariadb_indexes_round_trip() {
        let v = Variant::MariaDb;
        let ix = vix("iv", "v", None, Some("cosine"), &[("m", "8")]);
        assert_eq!(
            create_index(v, "`t`", &ix, false).unwrap(),
            "CREATE VECTOR INDEX `iv` ON `t` (`v`) M=8 DISTANCE=cosine;"
        );
        assert_eq!(
            create_index(v, "`t`", &vix("iv", "v", None, None, &[]), true).unwrap(),
            "CREATE VECTOR INDEX IF NOT EXISTS `iv` ON `t` (`v`);"
        );
        assert!(create_index(v, "`t`", &vix("iv", "v", None, Some("ip"), &[]), false).is_err());
        assert!(create_index(
            v,
            "`t`",
            &vix("iv", "v", None, None, &[("m", "8; DROP")]),
            false
        )
        .is_err());
        let create = "CREATE TABLE `t` (\n  `id` int(11) NOT NULL,\n  `v` vector(3) NOT NULL,\n  PRIMARY KEY (`id`),\n  VECTOR KEY `iv` (`v`) `M`='8' `DISTANCE`='cosine'\n) ENGINE=InnoDB";
        let read = from_create(v, create);
        assert_eq!(read.len(), 1);
        let r = &read[0];
        assert_eq!(
            (r.name.as_str(), r.columns.as_slice()),
            ("iv", &["v".to_string()][..])
        );
        assert_eq!(r.kind.as_deref(), Some("VECTOR"));
        assert_eq!(r.vector, ix.vector);
        assert_eq!(
            r.options.get("DISTANCE").map(String::as_str),
            Some("cosine")
        );
        let plain = from_create(v, "CREATE TABLE `t` (\n  VECTOR KEY `v` (`v`)\n)");
        assert_eq!(plain[0].vector, Some(VectorIndex::default()));
    }

    #[test]
    fn tidb_indexes_name_their_distance_function() {
        let v = Variant::TiDb;
        assert_eq!(
            create_index(
                v,
                "`t`",
                &vix("iv", "v", Some("hnsw"), Some("l2"), &[]),
                false
            )
            .unwrap(),
            "CREATE VECTOR INDEX `iv` ON `t` ((VEC_L2_DISTANCE(`v`))) USING HNSW;"
        );
        assert_eq!(
            create_index(v, "`t`", &vix("iv", "v", None, None, &[]), false).unwrap(),
            "CREATE VECTOR INDEX `iv` ON `t` ((VEC_COSINE_DISTANCE(`v`))) USING HNSW;"
        );
        assert!(
            create_index(v, "`t`", &vix("iv", "v", Some("ivfflat"), None, &[]), false).is_err()
        );
        let create = "CREATE TABLE `t` (\n  `id` int NOT NULL,\n  `v` vector(3) DEFAULT NULL,\n  PRIMARY KEY (`id`) /*T![clustered_index] CLUSTERED */,\n  VECTOR INDEX `iv`((VEC_COSINE_DISTANCE(`v`)))\n) ENGINE=InnoDB";
        let r = &from_create(v, create)[0];
        assert_eq!(r.columns, ["v"]);
        let vi = r.vector.as_ref().unwrap();
        assert_eq!(vi.metric.as_deref(), Some("cosine"));
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
    }

    #[test]
    fn oceanbase_indexes_take_type_lib_and_params() {
        let v = Variant::OceanBase;
        let ix = vix(
            "iv",
            "v",
            Some("hnsw"),
            Some("l2"),
            &[("m", "16"), ("ef_construction", "200")],
        );
        assert_eq!(
            create_index(v, "`t`", &ix, false).unwrap(),
            "CREATE VECTOR INDEX `iv` ON `t` (`v`) WITH (distance=L2, type=hnsw, lib=vsag, ef_construction=200, m=16);"
        );
        assert_eq!(
            create_index(v, "`t`", &vix("iv", "v", Some("ivf_flat"), Some("ip"), &[("lists", "64")]), false).unwrap(),
            "CREATE VECTOR INDEX `iv` ON `t` (`v`) WITH (distance=INNER_PRODUCT, type=ivf_flat, lib=ob, nlist=64);"
        );
        let create = "CREATE TABLE `t` (\n  `v` VECTOR(3) DEFAULT NULL,\n  VECTOR KEY `iv` (`v`) WITH (DISTANCE=L2, TYPE=HNSW, LIB=VSAG, M=16, EF_CONSTRUCTION=200) BLOCK_SIZE 16384 LOCAL\n)";
        let r = &from_create(v, create)[0];
        let vi = r.vector.as_ref().unwrap();
        assert_eq!(vi.metric.as_deref(), Some("l2"));
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(vi.params.get("m").map(String::as_str), Some("16"));
        assert_eq!(
            vi.params.get("ef_construction").map(String::as_str),
            Some("200")
        );
    }

    #[test]
    fn singlestore_indexes_carry_json_options() {
        let v = Variant::SingleStore;
        let ix = vix(
            "iv",
            "v",
            Some("hnsw_flat"),
            Some("l2"),
            &[("m", "30"), ("ef_construction", "40")],
        );
        assert_eq!(
            create_index(v, "`t`", &ix, false).unwrap(),
            r#"ALTER TABLE `t` ADD VECTOR INDEX `iv` (`v`) INDEX_OPTIONS '{"index_type":"HNSW_FLAT","metric_type":"EUCLIDEAN_DISTANCE","efConstruction":40,"M":30}';"#
        );
        assert_eq!(
            create_index(v, "`t`", &vix("iv", "v", None, None, &[]), false).unwrap(),
            "ALTER TABLE `t` ADD VECTOR INDEX `iv` (`v`);"
        );
        let create = "CREATE TABLE `t` (\n  `v` vector(4, F32) DEFAULT NULL,\n  VECTOR INDEX `iv` (`v`) INDEX_OPTIONS='{\"index_type\":\"HNSW_FLAT\",\"metric_type\":\"EUCLIDEAN_DISTANCE\",\"M\":30}'\n)";
        let r = &from_create(v, create)[0];
        let vi = r.vector.as_ref().unwrap();
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw_flat"));
        assert_eq!(vi.metric.as_deref(), Some("l2"));
        assert_eq!(vi.params.get("m").map(String::as_str), Some("30"));
    }

    #[test]
    fn olap_indexes_need_the_dimension() {
        let ix = vix(
            "iv",
            "v",
            Some("hnsw"),
            Some("l2"),
            &[("dim", "3"), ("m", "32")],
        );
        let (k, p) = olap_index(Variant::Doris, &ix).unwrap();
        assert_eq!(k, "ANN");
        assert_eq!(
            p,
            [
                ("index_type", "hnsw"),
                ("metric_type", "l2_distance"),
                ("dim", "3"),
                ("max_degree", "32")
            ]
            .map(|(a, b)| (a.to_string(), b.to_string()))
        );
        let (k, p) = olap_index(
            Variant::StarRocks,
            &vix(
                "iv",
                "v",
                None,
                Some("cosine"),
                &[("dim", "3"), ("m", "16")],
            ),
        )
        .unwrap();
        assert_eq!(k, "VECTOR");
        assert!(p.contains(&("metric_type".into(), "cosine_similarity".into())));
        assert!(p.contains(&("M".into(), "16".into())));
        assert!(p.contains(&("is_vector_normed".into(), "false".into())));
        assert!(olap_index(Variant::Doris, &vix("iv", "v", None, None, &[])).is_err());
        assert!(olap_index(
            Variant::Doris,
            &vix("iv", "v", None, Some("cosine"), &[("dim", "3")])
        )
        .is_err());
        // Read back from SHOW INDEX.
        let mut read = IndexDef {
            name: "iv".into(),
            columns: vec!["v".into()],
            kind: Some("ANN".into()),
            options: [
                ("index_type", "hnsw"),
                ("metric_type", "l2_distance"),
                ("dim", "3"),
                ("max_degree", "32"),
            ]
            .iter()
            .map(|(a, b)| (a.to_string(), b.to_string()))
            .collect(),
            ..Default::default()
        };
        from_olap(Variant::Doris, &mut read);
        assert_eq!(read.vector, ix.vector);
    }

    #[test]
    fn manticore_knn_lives_in_the_column() {
        let ix = vix(
            "v",
            "v",
            Some("hnsw"),
            Some("cosine"),
            &[("knn_dims", "4"), ("m", "16")],
        );
        assert_eq!(
            manticore_knn(&ix).unwrap(),
            "knn_type='hnsw' knn_dims='4' hnsw_similarity='COSINE' hnsw_m='16'"
        );
        assert!(manticore_knn(&vix("v", "v", None, None, &[])).is_err());
        let create = "CREATE TABLE t (\nv float_vector knn_type='hnsw' knn_dims='4' hnsw_similarity='COSINE' hnsw_m='16',\ntitle text\n)";
        let r = manticore_indexes(create);
        assert_eq!(r.len(), 1);
        assert_eq!(r[0].vector, ix.vector);
        assert_eq!(
            manticore_base_type("float_vector knn_type='hnsw'"),
            "float_vector"
        );
    }

    #[test]
    fn mysql_has_no_vector_index_and_says_so() {
        let e = create_index(
            Variant::MySql,
            "`t`",
            &vix("iv", "v", None, None, &[]),
            false,
        )
        .unwrap_err();
        assert!(e.to_string().contains("HeatWave"), "{e}");
        assert!(designer(Variant::MySql).is_none());
        for v in [
            Variant::MariaDb,
            Variant::TiDb,
            Variant::OceanBase,
            Variant::SingleStore,
            Variant::Doris,
            Variant::StarRocks,
            Variant::Manticore,
            Variant::Databend,
        ] {
            let d = designer(v).unwrap();
            assert!(!d.metrics.is_empty(), "{v:?}");
            for m in &d.metrics {
                assert!(native_metric(v, m).is_ok(), "{v:?} {m}");
            }
        }
    }

    #[test]
    fn single_tables_are_found_in_selects() {
        let one = |s: &str| single_table(s);
        assert_eq!(one("SELECT * FROM `t` LIMIT 10"), Some((None, "t".into())));
        assert_eq!(
            one("SELECT *\nFROM `t`\nLIMIT 100"),
            Some((None, "t".into()))
        );
        assert_eq!(
            one("select id, v from db.items where id = 1;"),
            Some((Some("db".into()), "items".into()))
        );
        assert_eq!(
            one("SELECT v FROM t ORDER BY knn_dist() LIMIT 3"),
            Some((None, "t".into()))
        );
        assert_eq!(
            one("SELECT a, b FROM t WHERE a IN (1, 2)"),
            Some((None, "t".into()))
        );
        for none in [
            "SELECT * FROM a, b",
            "SELECT * FROM a JOIN b ON a.x = b.x",
            "SELECT * FROM (SELECT 1) x",
            "INSERT INTO t VALUES (1)",
            "SELECT 1; SELECT * FROM t",
            "SELECT * FROM t WHERE x IN (SELECT y FROM u)",
        ] {
            assert_eq!(one(none), None, "{none}");
        }
    }

    #[test]
    fn seen_columns_expire_and_clear() {
        let mut s = SeenColumns::default();
        assert_eq!(s.known("db", "t", "v"), Known::Unknown);
        let mut cols = SchemaColumns::new();
        cols.insert(("t".into(), "v".into()), "vector(3)".into());
        s.insert("db", cols);
        assert!(s.fresh("db"));
        assert_eq!(s.known("db", "t", "V"), Known::Vector("vector(3)".into()));
        assert_eq!(s.known("db", "t", "id"), Known::NotVector);
        assert_eq!(s.known("db", "T", "v"), Known::NotVector);
        s.clear();
        assert_eq!(s.known("db", "t", "v"), Known::Unknown);
        assert!(may_change_columns("ALTER TABLE t ADD v vector(3)"));
        assert!(!may_change_columns("SELECT * FROM t"));
        assert!(!may_change_columns("SELECT created_at, dropped FROM t"));
        assert!(may_change_columns("SELECT 1; drop table t"));
        assert!(
            looked_up(Variant::MariaDb)
                && looked_up(Variant::TiDb)
                && looked_up(Variant::Manticore)
        );
        assert!(!looked_up(Variant::MySql) && !looked_up(Variant::AuroraMySql));
    }

    #[test]
    fn greptime_indexes_go_in_the_column() {
        let ix = vix(
            "iv",
            "v",
            None,
            Some("cosine"),
            &[("m", "16"), ("ef_construction", "128")],
        );
        assert_eq!(
            greptime_column_index(&ix).unwrap(),
            " VECTOR INDEX WITH(metric = 'cosine', expansion_add = '128', connectivity = '16')"
        );
        let opts: BTreeMap<String, String> = [
            ("connectivity", "16"),
            ("engine", "usearch"),
            ("expansion_add", "128"),
            ("metric", "cosine"),
        ]
        .iter()
        .map(|(a, b)| (a.to_string(), b.to_string()))
        .collect();
        let back = greptime_index("v", opts);
        assert_eq!(back.name, "VECTOR_INDEX_v");
        assert_eq!(back.vector, ix.vector);
        assert_eq!(standard_metric(Variant::GreptimeDb, "l2sq"), "l2");
    }

    #[test]
    fn other_engines_tuning_is_left_out() {
        // pgvector's ef_search or Oracle's neighbors mean nothing to MariaDB.
        let ix = vix(
            "iv",
            "v",
            None,
            Some("l2"),
            &[("m", "8"), ("ef_search", "40"), ("neighbors", "32")],
        );
        assert_eq!(
            create_index(Variant::MariaDb, "`t`", &ix, false).unwrap(),
            "CREATE VECTOR INDEX `iv` ON `t` (`v`) M=8 DISTANCE=euclidean;"
        );
    }

    #[test]
    fn databend_indexes() {
        assert_eq!(
            create_index(
                Variant::Databend,
                "`t`",
                &vix(
                    "iv",
                    "v",
                    None,
                    Some("l2"),
                    &[("m", "16"), ("ef_construction", "100")]
                ),
                false
            )
            .unwrap(),
            "CREATE VECTOR INDEX `iv` ON `t`(`v`) ef_construct=100 m=16 distance='l2';"
        );
    }
}
