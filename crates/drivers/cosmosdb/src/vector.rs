//! Vectors (embeddings) in Cosmos DB for NoSQL.
//!
//! A container declares its vector fields in its **vector embedding
//! policy** (`vectorEmbeddingPolicy.vectorEmbeddings`: `path`, `dataType`
//! `float32` | `int8` | `uint8`, `dimensions`, `distanceFunction`
//! `cosine` | `euclidean` | `dotproduct`), set when the container is
//! created, and indexes them with the indexing policy's `vectorIndexes`
//! (`path`, `type` `flat` | `quantizedFlat` | `diskANN`,
//! `quantizationByteSize`, `indexingSearchListSize`, `vectorIndexShardKey`).
//! The server refuses a vector index whose path the embedding policy
//! doesn't declare.
//!
//! In the schema model:
//! - each embedding is a field of type `vector(N)` (float32),
//!   `vector(N, int8)` or `vector(N, uint8)`, named by its path as the
//!   other fields are (`/emb` → `emb`); a field no sampled item has is
//!   added;
//! - its vector index is an [`IndexDef`] of kind `vector` with
//!   [`IndexDef::vector`]: the `type` as the algorithm, the embedding's
//!   distance as the metric (`euclidean` = `l2`, `dotproduct` = `ip`) and
//!   the other keys as parameters, by their names;
//! - the distance of an embedding without a vector index goes in the
//!   table option `vector_distances` (`emb=euclidean, …`), when it isn't
//!   `cosine`.
//!
//! The vector paths are left out of the default indexing policy's range
//! index (`excludedPaths`), as Cosmos DB recommends.

use dbine_driver::vector::metric;
use dbine_driver::{
    ColumnDef, Error, IndexDef, Result, TableSchema, VectorElement, VectorIndex, VectorIndexSpec,
    VectorMeta, VectorParamSpec,
};
use serde_json::{json, Map, Value};
use std::collections::HashMap;

/// The kind of a vector index (an entry of `vectorIndexes`).
pub const KIND: &str = "vector";
/// Table option: distances of the embeddings without a vector index.
pub const DISTANCES: &str = "vector_distances";

/// `vector(N)`, `vector(N, int8)`, `vector(N, uint8)`.
pub fn type_name(dims: Option<u32>, element: VectorElement) -> String {
    let el = match element {
        VectorElement::I8 => ", int8",
        VectorElement::U8 => ", uint8",
        _ => "",
    };
    match dims {
        Some(n) => format!("vector({n}{el})"),
        None if el.is_empty() => "vector".into(),
        None => format!("vector({})", &el[2..]),
    }
}

/// A `vector(…)` field type's dimension and element (`None`: not one).
pub fn parse_type(t: &str) -> Option<(Option<u32>, VectorElement)> {
    t.split('|').find_map(|t| {
        let lower = t.trim().to_ascii_lowercase();
        let rest = lower.strip_prefix("vector")?.trim();
        if rest.is_empty() {
            return Some((None, VectorElement::F32));
        }
        let inner = rest.strip_prefix('(')?.strip_suffix(')')?;
        let mut dims = None;
        let mut el = VectorElement::F32;
        for part in inner.split(',').map(str::trim).filter(|p| !p.is_empty()) {
            if let Ok(n) = part.parse::<u32>() {
                dims = Some(n);
            } else {
                el = element(part)?;
            }
        }
        Some((dims, el))
    })
}

fn element(data_type: &str) -> Option<VectorElement> {
    Some(match data_type.to_ascii_lowercase().as_str() {
        "float32" | "float" => VectorElement::F32,
        "int8" => VectorElement::I8,
        "uint8" => VectorElement::U8,
        _ => return None,
    })
}

fn data_type(el: VectorElement) -> &'static str {
    match el {
        VectorElement::I8 => "int8",
        VectorElement::U8 => "uint8",
        _ => "float32",
    }
}

fn metric_of(distance: &str) -> String {
    match distance.to_ascii_lowercase().as_str() {
        "cosine" => metric::COSINE.into(),
        "euclidean" => metric::L2.into(),
        "dotproduct" => metric::IP.into(),
        other => other.into(),
    }
}

/// The squared euclidean distance (Couchbase's `l2_squared`), and
/// hamming over bits (a bit vector from another engine, stored here as 0
/// and 1; the schema conversion lets it through only over bits): Cosmos
/// DB has the euclidean one, with the same nearest neighbors (see
/// [`notes`]).
fn squared(m: &str) -> bool {
    matches!(
        m.to_ascii_lowercase().as_str(),
        "l2_squared" | "euclidean_squared" | metric::HAMMING
    )
}

fn distance_of(m: &str) -> Result<&'static str> {
    Ok(match m.to_ascii_lowercase().as_str() {
        metric::COSINE => "cosine",
        metric::L2 | "euclidean" | "l2_squared" | "euclidean_squared" | metric::HAMMING => {
            "euclidean"
        }
        metric::IP | "dotproduct" => "dotproduct",
        other => {
            return Err(Error::Query(format!(
                "Cosmos DB no tiene la distancia «{other}» (cosine, euclidean o dotproduct)."
            )))
        }
    })
}

/// A field name as its path, and back (as `ddl::path` / `ddl::field_of`).
fn path(field: &str) -> String {
    crate::ddl::path(field)
}

/// One embedding of a container's policy.
#[derive(Debug, Clone, PartialEq)]
pub struct Embedding {
    pub field: String,
    pub element: VectorElement,
    pub dims: Option<u32>,
    pub distance: String,
}

/// The embeddings a container (its REST body) declares.
pub fn embeddings(coll: &Value) -> Vec<Embedding> {
    coll["vectorEmbeddingPolicy"]["vectorEmbeddings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| {
            Some(Embedding {
                field: crate::ddl::field_of(e["path"].as_str()?),
                element: e["dataType"]
                    .as_str()
                    .and_then(element)
                    .unwrap_or(VectorElement::Other),
                dims: e["dimensions"].as_u64().map(|n| n as u32),
                distance: e["distanceFunction"]
                    .as_str()
                    .unwrap_or("cosine")
                    .to_ascii_lowercase(),
            })
        })
        .collect()
}

/// The vector fields of a container, for its results: field → vector.
/// A vector index's path without an embedding (a server that doesn't
/// report the embedding policy, like the emulator) is a vector field of
/// unknown dimension.
pub fn fields(coll: &Value) -> HashMap<String, VectorMeta> {
    let mut out: HashMap<String, VectorMeta> = indexed_fields(coll)
        .into_iter()
        .map(|f| (f, VectorMeta::dense(None, VectorElement::F32)))
        .collect();
    out.extend(
        embeddings(coll)
            .into_iter()
            .map(|e| (e.field, VectorMeta::dense(e.dims, e.element))),
    );
    out
}

/// The fields of the indexing policy's vector indexes.
fn indexed_fields(coll: &Value) -> Vec<String> {
    coll["indexingPolicy"]["vectorIndexes"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["path"].as_str().map(crate::ddl::field_of))
        .collect()
}

/// The common length of the arrays of numbers `field` holds in `items`
/// (`None`: none, or not all the same).
pub fn sampled_dims(items: &[Value], field: &str) -> Option<u32> {
    let mut lens = items
        .iter()
        .filter_map(|d| d.get(field))
        .filter(|v| !v.is_null())
        .map(|v| match v {
            Value::Array(a) if a.iter().all(Value::is_number) => Some(a.len()),
            _ => None,
        });
    let first = lens.next()??;
    lens.all(|l| l == Some(first)).then_some(first as u32)
}

/// A `vectorIndexes` entry as a vector index (its `type` as the algorithm,
/// the other keys as parameters).
pub fn index(n: usize, entry: &Value, embeddings: &[Embedding]) -> IndexDef {
    let field = crate::ddl::field_of(entry["path"].as_str().unwrap_or_default());
    let mut v = VectorIndex {
        algorithm: entry["type"].as_str().map(str::to_string),
        metric: embeddings
            .iter()
            .find(|e| e.field == field)
            .map(|e| metric_of(&e.distance)),
        ..Default::default()
    };
    for (k, x) in entry.as_object().into_iter().flatten() {
        if k != "path" && k != "type" {
            v.params.insert(
                k.clone(),
                x.as_str()
                    .map(str::to_string)
                    .unwrap_or_else(|| x.to_string()),
            );
        }
    }
    IndexDef {
        name: format!("vector_{n}"),
        columns: vec![field],
        kind: Some(KIND.into()),
        vector: Some(v),
        ..Default::default()
    }
}

/// The read-back container: its embeddings as `vector(N)` fields (added
/// when no sampled item has them), and the distances of those without a
/// vector index in [`DISTANCES`].
pub fn describe(t: &mut TableSchema, embeddings: &[Embedding], sample: &[Value]) {
    let mut lone = Vec::new();
    // A vector index without its embedding (the emulator doesn't report
    // the policy): a float32 field of the sampled length.
    let unreported: Vec<Embedding> = t
        .indexes
        .iter()
        .filter(|i| i.vector.is_some())
        .filter_map(|i| i.columns.first())
        .filter(|f| !embeddings.iter().any(|e| &e.field == *f))
        .map(|f| Embedding {
            field: f.clone(),
            element: VectorElement::F32,
            dims: sampled_dims(sample, f),
            distance: "cosine".into(),
        })
        .collect();
    // Said, so a copy or a sync doesn't take the guess for the policy.
    if !unreported.is_empty() {
        let fields: Vec<String> = unreported
            .iter()
            .map(|e| match e.dims {
                Some(n) => format!("«{}» ({n} dimensiones, de los ítems leídos)", e.field),
                None => format!("«{}» (sin dimensión: ningún ítem leído la tiene)", e.field),
            })
            .collect();
        t.warnings.push(format!(
            "El servidor no devolvió la política de vectores de {}: de {} se toma la distancia cosine y elementos float32. Revisalos antes de copiar o sincronizar el contenedor.",
            t.name,
            fields.join(", ")
        ));
    }
    for e in embeddings.iter().chain(&unreported) {
        let ty = type_name(e.dims, e.element);
        match t.columns.iter_mut().find(|c| c.name == e.field) {
            Some(c) => c.data_type = ty,
            None => t.columns.push(ColumnDef {
                name: e.field.clone(),
                data_type: ty,
                nullable: true,
                ..Default::default()
            }),
        }
        let indexed = t
            .indexes
            .iter()
            .any(|i| i.vector.is_some() && i.columns.first() == Some(&e.field));
        if !indexed && e.distance != "cosine" {
            lone.push(format!("{}={}", e.field, e.distance));
        }
    }
    if !lone.is_empty() {
        t.options.insert(DISTANCES.into(), lone.join(", "));
    }
}

/// The `vectorEmbeddingPolicy` of the designed container: one embedding
/// per `vector(N)` field, with the distance of its vector index, else of
/// [`DISTANCES`], else `cosine`. `None` without vector fields.
pub fn embedding_policy(t: &TableSchema) -> Result<Option<Value>> {
    let lone: HashMap<String, String> = t
        .options
        .get(DISTANCES)
        .map(|s| {
            s.split(',')
                .filter_map(|p| p.split_once('='))
                .map(|(f, d)| (f.trim().to_string(), d.trim().to_string()))
                .collect()
        })
        .unwrap_or_default();
    let mut out = Vec::new();
    for c in &t.columns {
        let Some((dims, el)) = parse_type(&c.data_type) else {
            continue;
        };
        let dims = dims.ok_or_else(|| {
            Error::Query(format!(
                "El campo vectorial {} necesita la dimensión: vector(N).",
                c.name
            ))
        })?;
        let indexed = t
            .indexes
            .iter()
            .find(|i| is_vector(i) && i.columns.first() == Some(&c.name))
            .and_then(|i| i.vector.as_ref()?.metric.clone());
        let distance = match indexed.or_else(|| lone.get(&c.name).cloned()) {
            Some(m) => distance_of(&m)?,
            None => "cosine",
        };
        out.push(json!({
            "path": path(&c.name),
            "dataType": data_type(el),
            "distanceFunction": distance,
            "dimensions": dims,
        }));
    }
    for i in t.indexes.iter().filter(|i| is_vector(i)) {
        let f = i.columns.first().map(String::as_str).unwrap_or_default();
        if !t
            .columns
            .iter()
            .any(|c| c.name == f && parse_type(&c.data_type).is_some())
        {
            return Err(Error::Query(format!(
                "El índice vectorial {} necesita que el campo «{f}» sea de tipo vector(N) (la política de vectores del contenedor).",
                i.name
            )));
        }
    }
    Ok((!out.is_empty()).then(|| json!({ "vectorEmbeddings": out })))
}

/// A metric as the policy stores it, for comparing (`l2`, `euclidean`
/// and `l2_squared` are the same distance there; none is cosine).
pub fn compared_metric(m: Option<&str>) -> String {
    match m {
        Some(m) => distance_of(m)
            .map(str::to_string)
            .unwrap_or_else(|_| m.to_ascii_lowercase()),
        None => "cosine".into(),
    }
}

/// Most dimensions a `flat` vector index takes (`quantizedFlat` and
/// `diskANN` take more).
pub const FLAT_MAX_DIMS: u32 = 505;

/// The dimension of the vector field `i` indexes, from its type.
pub fn field_dims(t: &TableSchema, i: &IndexDef) -> Option<u32> {
    let f = i.columns.first()?;
    t.columns
        .iter()
        .find(|c| c.name == *f)
        .and_then(|c| parse_type(&c.data_type))
        .and_then(|(d, _)| d)
}

/// The vector index type `i` is written with: its algorithm, else the
/// option `type`, else `diskANN`. A `flat` index over more than
/// [`FLAT_MAX_DIMS`] dimensions (`dims`) is `quantizedFlat`, which also
/// compares every vector, over their quantized form.
fn index_type(i: &IndexDef, dims: Option<u32>) -> String {
    let t = i
        .vector
        .as_ref()
        .and_then(|v| v.algorithm.clone())
        .or_else(|| i.options.get("type").cloned())
        .unwrap_or_else(|| "diskANN".into());
    if t.eq_ignore_ascii_case("flat") && dims.is_some_and(|d| d > FLAT_MAX_DIMS) {
        "quantizedFlat".into()
    } else {
        t
    }
}

/// What the container's vector policy writes otherwise than the design
/// asks (a squared euclidean distance as euclidean, a `flat` index too
/// wide for it as `quantizedFlat`), for the script.
pub fn notes(t: &TableSchema) -> Vec<String> {
    let mut out = Vec::new();
    for i in t.indexes.iter().filter(|i| is_vector(i)) {
        if let Some(m) = i
            .vector
            .as_ref()
            .and_then(|v| v.metric.as_deref())
            .filter(|m| squared(m))
        {
            out.push(if m.eq_ignore_ascii_case(metric::HAMMING) {
                format!(
                    "El índice vectorial {} de {}: la distancia hamming queda euclidean (Cosmos DB no tiene hamming; sobre vectores de 0 y 1 los vecinos más cercanos son los mismos).",
                    i.name, t.name
                )
            } else {
                format!(
                    "El índice vectorial {} de {}: la distancia {m} queda euclidean (Cosmos DB no tiene la euclídea al cuadrado; los vecinos más cercanos son los mismos).",
                    i.name, t.name
                )
            });
        }
        let dims = field_dims(t, i);
        if index_type(i, None).eq_ignore_ascii_case("flat")
            && index_type(i, dims) != index_type(i, None)
        {
            out.push(format!(
                "El índice vectorial {} de {} queda quantizedFlat: flat indexa vectores de hasta 505 dimensiones, y este tiene {}.",
                i.name,
                t.name,
                dims.unwrap_or_default()
            ));
        }
    }
    out
}

/// A vector index: kind `vector`, or a designer row with "Vectorial" on.
pub fn is_vector(i: &IndexDef) -> bool {
    i.vector.is_some()
        || i.kind
            .as_deref()
            .is_some_and(|k| k.trim().eq_ignore_ascii_case(KIND))
}

/// A vector index's `vectorIndexes` entry; `dims`: its field's dimension
/// (see [`index_type`]).
pub fn entry(i: &IndexDef, dims: Option<u32>) -> Value {
    let mut e = Map::new();
    e.insert(
        "path".into(),
        path(i.columns.first().map(String::as_str).unwrap_or_default()).into(),
    );
    let v = i.vector.clone().unwrap_or_default();
    e.insert("type".into(), index_type(i, dims).into());
    // Options of an index from before (`type`, `quantizationByteSize`…),
    // then the parameters.
    let params = i
        .options
        .iter()
        .chain(v.params.iter())
        .filter(|(k, _)| k.as_str() != "type");
    for (k, x) in params {
        if !x.trim().is_empty() {
            e.insert(
                k.clone(),
                serde_json::from_str(x).unwrap_or_else(|_| Value::String(x.clone())),
            );
        }
    }
    Value::Object(e)
}

/// The paths of the embeddings, as the range index's excluded paths.
pub fn excluded_paths(policy: &Value) -> Vec<Value> {
    policy["vectorEmbeddings"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|e| e["path"].as_str())
        .map(|p| json!({ "path": format!("{p}/*") }))
        .collect()
}

/// The designer's vector index choices.
pub fn designer_spec() -> VectorIndexSpec {
    let p = |key, algorithms: &[&'static str], default| VectorParamSpec {
        key,
        algorithms: algorithms.to_vec(),
        default,
    };
    VectorIndexSpec {
        algorithms: vec!["diskANN", "quantizedFlat", "flat"],
        metrics: vec![metric::COSINE, metric::L2, metric::IP],
        params: vec![
            p("quantizationByteSize", &["diskANN", "quantizedFlat"], ""),
            p("indexingSearchListSize", &["diskANN"], "100"),
        ],
        filter_columns: false,
    }
}

/// Whether a source column's type holds dense vectors, whichever engine it
/// comes from: `vector(3)`, `VECTOR(3, FLOAT32)`, `vector<float, 3>`,
/// `frozen<vector<float, 3>>`, `halfvec(3)`, `dense_vector(3)`,
/// `knn_vector(3)`, `float_vector(3)`, `REAL_VECTOR(3)`, `F32_BLOB(3)`,
/// `vecf32(3)`, `ARRAY<FLOAT32>(vector_length=>3)`…, also with a crate's
/// mark (`bson:vector(3, int8)`) or among `|` alternatives
/// (`bson:vector(3)|bson:null`). Sparse vectors (`sparsevec`,
/// `sparse_vector`, `VECTOR(…, SPARSE)`) aren't: their text isn't an
/// array. The same list in every crate of the document family (the
/// contract has no helper for it).
pub fn is_vector_type(t: &str) -> bool {
    t.split('|').any(|part| {
        let p = part.trim().to_ascii_lowercase();
        if p.contains("sparse") {
            return false;
        }
        p.split(|c: char| !(c.is_ascii_alphanumeric() || c == '_'))
            .any(|tok| {
                tok.ends_with("vector")
                    || tok.ends_with("vec")
                    || matches!(
                        tok,
                        "vecf32"
                            | "vecf64"
                            | "vector_length"
                            | "f32_blob"
                            | "f64_blob"
                            | "f16_blob"
                            | "fb16_blob"
                            | "bf16_blob"
                            | "f8_blob"
                            | "f1bit_blob"
                    )
            })
    })
}

/// A vector value of an edit (a JSON array of numbers) as stored: the
/// array, its numbers as the engine reads them back.
pub fn cell_text(v: &Value) -> Option<String> {
    match v {
        Value::Array(_) => dbine_driver::vector::value_text(v),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn vector_types_of_every_engine() {
        for t in [
            "vector(3)",
            "bson:vector(3)",
            "bson:vector(3, int8)|bson:null",
            "VECTOR(3, FLOAT32)",
            "vector<float, 3>",
            "frozen<vector<float, 3>>",
            "halfvec(3)",
            "dense_vector(3)",
            "knn_vector(3)",
            "float_vector(3)",
            "REAL_VECTOR(3)",
            "F32_BLOB(3)",
            "vecf32(3)",
            "ARRAY<FLOAT32>(vector_length=>3)",
            "vector",
        ] {
            assert!(is_vector_type(t), "{t}");
        }
        for t in [
            "sparsevec(5)",
            "sparse_vector",
            "VECTOR(3, FLOAT32, SPARSE)",
            "varchar(20)",
            "text",
            "",
            "bson:binData",
            "bson:array",
            "json",
            "blob",
        ] {
            assert!(!is_vector_type(t), "{t}");
        }
    }

    fn coll() -> Value {
        json!({
            "id": "docs",
            "partitionKey": { "paths": ["/cat"], "kind": "Hash" },
            "vectorEmbeddingPolicy": { "vectorEmbeddings": [
                { "path": "/emb", "dataType": "float32", "dimensions": 3, "distanceFunction": "dotproduct" },
                { "path": "/raw", "dataType": "uint8", "dimensions": 4, "distanceFunction": "euclidean" },
            ] },
            "indexingPolicy": {
                "indexingMode": "consistent",
                "includedPaths": [{ "path": "/*" }],
                "excludedPaths": [{ "path": "/\"_etag\"/?" }, { "path": "/emb/*" }],
                "vectorIndexes": [{ "path": "/emb", "type": "quantizedFlat", "quantizationByteSize": 2 }],
            },
        })
    }

    #[test]
    fn types_spell_dims_and_element() {
        assert_eq!(type_name(Some(3), VectorElement::F32), "vector(3)");
        assert_eq!(type_name(Some(3), VectorElement::U8), "vector(3, uint8)");
        assert_eq!(
            parse_type("vector(1536)"),
            Some((Some(1536), VectorElement::F32))
        );
        assert_eq!(
            parse_type("array|vector(4, int8)"),
            Some((Some(4), VectorElement::I8))
        );
        assert_eq!(parse_type("array"), None);
        assert_eq!(parse_type("vector(3, bit)"), None);
        for t in [
            "vector(3)",
            "VECTOR(3, FLOAT32)",
            "halfvec(2)",
            "vector<float, 3>",
            "F32_BLOB(3)",
        ] {
            assert!(is_vector_type(t), "{t}");
        }
        for t in ["sparsevec(5)", "array", "vectorize", ""] {
            assert!(!is_vector_type(t), "{t}");
        }
    }

    #[test]
    fn policies_read_back_and_write_the_same() {
        let c = coll();
        let emb = embeddings(&c);
        assert_eq!(emb.len(), 2);
        assert_eq!(emb[0].dims, Some(3));
        let ix = index(1, &c["indexingPolicy"]["vectorIndexes"][0], &emb);
        assert_eq!(ix.columns, ["emb"]);
        let v = ix.vector.clone().unwrap();
        assert_eq!(v.algorithm.as_deref(), Some("quantizedFlat"));
        assert_eq!(v.metric.as_deref(), Some(metric::IP));
        assert_eq!(
            v.params.get("quantizationByteSize").map(String::as_str),
            Some("2")
        );
        let mut t = TableSchema {
            name: "docs".into(),
            indexes: vec![ix.clone()],
            ..Default::default()
        };
        describe(&mut t, &emb, &[]);
        assert_eq!(t.columns[0].data_type, "vector(3)");
        assert_eq!(t.columns[1].data_type, "vector(4, uint8)");
        assert_eq!(
            t.options.get(DISTANCES).map(String::as_str),
            Some("raw=euclidean")
        );
        // Written back: the same policy and entry.
        assert_eq!(
            embedding_policy(&t).unwrap().unwrap(),
            c["vectorEmbeddingPolicy"]
        );
        assert_eq!(entry(&ix, None), c["indexingPolicy"]["vectorIndexes"][0]);
        assert_eq!(
            fields(&c).get("raw"),
            Some(&VectorMeta::dense(Some(4), VectorElement::U8))
        );
        // Without the embedding policy (the emulator): the index's field,
        // with the sampled length.
        let mut bare = c.clone();
        bare.as_object_mut()
            .unwrap()
            .remove("vectorEmbeddingPolicy");
        assert_eq!(
            fields(&bare).get("emb"),
            Some(&VectorMeta::dense(None, VectorElement::F32))
        );
        let mut t = TableSchema {
            name: "docs".into(),
            indexes: vec![index(1, &bare["indexingPolicy"]["vectorIndexes"][0], &[])],
            ..Default::default()
        };
        let sample = [
            json!({ "emb": [1, 0, 0] }),
            json!({ "emb": [0.5, 0.5, 0] }),
            json!({ "x": 1 }),
        ];
        describe(&mut t, &[], &sample);
        assert_eq!(t.columns[0].data_type, "vector(3)");
        // The guess is said, so a copy doesn't take it for the policy.
        assert_eq!(t.warnings.len(), 1);
        assert!(
            t.warnings[0].contains("«emb» (3 dimensiones, de los ítems leídos)")
                && t.warnings[0].contains("cosine"),
            "{:?}",
            t.warnings
        );
        let mut empty = TableSchema {
            name: "docs".into(),
            indexes: vec![index(1, &bare["indexingPolicy"]["vectorIndexes"][0], &[])],
            ..Default::default()
        };
        describe(&mut empty, &[], &[]);
        assert!(
            empty.warnings[0].contains("sin dimensión"),
            "{:?}",
            empty.warnings
        );
        // Reported: nothing to say.
        let mut t = TableSchema {
            name: "docs".into(),
            indexes: vec![ix.clone()],
            ..Default::default()
        };
        describe(&mut t, &emb, &[]);
        assert!(t.warnings.is_empty());
        assert_eq!(
            sampled_dims(&[json!({ "e": [1] }), json!({ "e": [1, 2] })], "e"),
            None
        );
    }

    #[test]
    fn designer_indexes_need_a_vector_field() {
        let mut t = TableSchema {
            name: "d".into(),
            columns: vec![ColumnDef {
                name: "emb".into(),
                data_type: "vector".into(),
                ..Default::default()
            }],
            indexes: vec![IndexDef {
                name: "v".into(),
                columns: vec!["emb".into()],
                vector: Some(VectorIndex {
                    metric: Some("l2".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        assert!(embedding_policy(&t)
            .unwrap_err()
            .to_string()
            .contains("vector(N)"));
        t.columns[0].data_type = "vector(8)".into();
        let p = embedding_policy(&t).unwrap().unwrap();
        assert_eq!(p["vectorEmbeddings"][0]["distanceFunction"], "euclidean");
        assert_eq!(entry(&t.indexes[0], None)["type"], "diskANN");
        t.columns[0].data_type = "string".into();
        assert!(embedding_policy(&t).is_err());
        t.indexes[0].vector.as_mut().unwrap().metric = Some("jaccard".into());
        t.columns[0].data_type = "vector(8)".into();
        assert!(embedding_policy(&t).is_err());
        assert_eq!(excluded_paths(&p), vec![json!({ "path": "/emb/*" })]);
    }

    #[test]
    fn a_flat_index_too_wide_is_quantized_flat_and_said() {
        let t = |dims: u32, algorithm: &str| TableSchema {
            name: "d".into(),
            columns: vec![ColumnDef {
                name: "emb".into(),
                data_type: format!("vector({dims})"),
                ..Default::default()
            }],
            indexes: vec![IndexDef {
                name: "v".into(),
                columns: vec!["emb".into()],
                vector: Some(VectorIndex {
                    algorithm: Some(algorithm.into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            options: [(crate::ddl::PARTITION_KEY.to_string(), "/id".to_string())].into(),
            ..Default::default()
        };
        let wide = t(1536, "flat");
        assert_eq!(field_dims(&wide, &wide.indexes[0]), Some(1536));
        let body = crate::ddl::container_body(&wide, true).unwrap();
        assert_eq!(
            body["indexingPolicy"]["vectorIndexes"][0]["type"],
            "quantizedFlat"
        );
        let n = notes(&wide);
        assert_eq!(n.len(), 1, "{n:?}");
        assert!(
            n[0].contains("queda quantizedFlat") && n[0].contains("1536"),
            "{n:?}"
        );
        // Up to 505 dimensions, and the other types: as asked, nothing said.
        for (dims, alg) in [(505, "flat"), (1536, "diskANN"), (1536, "quantizedFlat")] {
            let ok = t(dims, alg);
            let body = crate::ddl::container_body(&ok, true).unwrap();
            assert_eq!(body["indexingPolicy"]["vectorIndexes"][0]["type"], alg);
            assert!(notes(&ok).is_empty());
        }
        // The table's DDL says it in a comment.
        let ddl = crate::ddl::table_ddl(
            &wide,
            dbine_driver::DdlParts {
                create: true,
                indexes: true,
                ..Default::default()
            },
        );
        let ddl = ddl.unwrap();
        assert!(
            ddl.contains("-- El índice vectorial v de d queda quantizedFlat"),
            "{ddl}"
        );
    }

    #[test]
    fn squared_euclidean_is_written_euclidean_and_said() {
        let t = TableSchema {
            name: "d".into(),
            columns: vec![ColumnDef {
                name: "emb".into(),
                data_type: "vector(2)".into(),
                ..Default::default()
            }],
            indexes: vec![IndexDef {
                name: "v".into(),
                columns: vec!["emb".into()],
                vector: Some(VectorIndex {
                    metric: Some("l2_squared".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = embedding_policy(&t).unwrap().unwrap();
        assert_eq!(p["vectorEmbeddings"][0]["distanceFunction"], "euclidean");
        let n = notes(&t);
        assert_eq!(n.len(), 1);
        assert!(n[0].contains("l2_squared queda euclidean"), "{n:?}");
        let ddl = crate::ddl::table_ddl(
            &TableSchema {
                options: [("partition_key".to_string(), "/id".to_string())].into(),
                ..t.clone()
            },
            dbine_driver::DdlParts {
                create: true,
                indexes: true,
                ..Default::default()
            },
        )
        .unwrap();
        assert!(ddl.starts_with("-- El índice vectorial v de d"), "{ddl}");
        // Compared as the policy stores it.
        assert_eq!(
            compared_metric(Some("l2_squared")),
            compared_metric(Some("l2"))
        );
        assert_eq!(compared_metric(None), "cosine");
        assert_ne!(
            compared_metric(Some("cosine")),
            compared_metric(Some("jaccard"))
        );
    }

    #[test]
    fn hamming_over_bits_is_written_euclidean_and_said() {
        // A bit vector from another engine (0 and 1 here, `vector(8)`) with
        // its hamming index, as the schema conversion lets it through.
        let t = TableSchema {
            name: "d".into(),
            columns: vec![ColumnDef {
                name: "b".into(),
                data_type: "vector(8)".into(),
                ..Default::default()
            }],
            indexes: vec![IndexDef {
                name: "bix".into(),
                columns: vec!["b".into()],
                vector: Some(VectorIndex {
                    algorithm: Some("diskANN".into()),
                    metric: Some("hamming".into()),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        let p = embedding_policy(&t).unwrap().unwrap();
        assert_eq!(p["vectorEmbeddings"][0]["distanceFunction"], "euclidean");
        let n = notes(&t);
        assert_eq!(n.len(), 1);
        assert!(
            n[0].contains("la distancia hamming queda euclidean"),
            "{n:?}"
        );
        // Compared as the policy stores it: no difference after creating.
        assert_eq!(
            compared_metric(Some("hamming")),
            compared_metric(Some("l2"))
        );
    }
}
