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

fn distance_of(m: &str) -> Result<&'static str> {
    Ok(match m.to_ascii_lowercase().as_str() {
        metric::COSINE => "cosine",
        metric::L2 | "euclidean" => "euclidean",
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

/// A vector index: kind `vector`, or a designer row with "Vectorial" on.
pub fn is_vector(i: &IndexDef) -> bool {
    i.vector.is_some()
        || i.kind
            .as_deref()
            .is_some_and(|k| k.trim().eq_ignore_ascii_case(KIND))
}

/// A vector index's `vectorIndexes` entry.
pub fn entry(i: &IndexDef) -> Value {
    let mut e = Map::new();
    e.insert(
        "path".into(),
        path(i.columns.first().map(String::as_str).unwrap_or_default()).into(),
    );
    let v = i.vector.clone().unwrap_or_default();
    let algorithm = v
        .algorithm
        .clone()
        .or_else(|| i.options.get("type").cloned())
        .unwrap_or_else(|| "diskANN".into());
    e.insert("type".into(), algorithm.into());
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

/// A source column type that holds vectors: this driver's `vector(N)` and
/// other engines' (pgvector's `vector` / `halfvec`, `VECTOR(…)`,
/// Cassandra's `vector<…>`, libSQL's `F32_BLOB`, HANA's `REAL_VECTOR`…).
/// Sparse vectors aren't.
pub fn is_vector_type(t: &str) -> bool {
    let p = t.trim().to_ascii_lowercase();
    [
        "vector",
        "halfvec",
        "real_vector",
        "half_vector",
        "f32_blob",
        "f64_blob",
        "f16_blob",
        "fb16_blob",
        "f8_blob",
        "f1bit_blob",
    ]
    .iter()
    .any(|v| {
        p.strip_prefix(v)
            .is_some_and(|r| r.is_empty() || r.starts_with(['(', '<', ' ']))
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
        assert_eq!(entry(&ix), c["indexingPolicy"]["vectorIndexes"][0]);
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
        assert_eq!(entry(&t.indexes[0])["type"], "diskANN");
        t.columns[0].data_type = "string".into();
        assert!(embedding_policy(&t).is_err());
        t.indexes[0].vector.as_mut().unwrap().metric = Some("hamming".into());
        t.columns[0].data_type = "vector(8)".into();
        assert!(embedding_policy(&t).is_err());
        assert_eq!(excluded_paths(&p), vec![json!({ "path": "/emb/*" })]);
    }
}
