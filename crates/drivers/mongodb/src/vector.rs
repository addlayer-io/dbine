//! Vectors (embeddings) in MongoDB, FerretDB and Amazon DocumentDB.
//!
//! # Values
//!
//! - A vector is an array of numbers or a BinData of subtype 9
//!   (`Binary.fromFloat32Array`, `fromInt8Array`, `fromPackedBits`): a
//!   dtype byte (`0x27` float32, `0x03` int8, `0x10` packed bits), a
//!   padding byte (the unused bits of the last byte, packed bits only) and
//!   the data. Results and transfer reads show both as the canonical JSON
//!   array ([`dbine_driver::vector`]).
//! - A vector field's type in the designer and the schema model is
//!   `vector(N)` (numbers: an array, or float32 BinData), `vector(N, int8)`
//!   or `vector(N, bit)`. The validator the designer makes admits an array
//!   or a BinData for it.
//! - Edits keep the stored form ([`keep_form_expr`]); a new document from
//!   the grid takes the form recent results showed ([`binary_fields`]).
//!
//! # Indexes ([`IndexDef::kind`])
//!
//! - `vectorSearch`: Atlas Vector Search, and MongoDB Community /
//!   Enterprise 8.2+ with `mongot`. A search index, listed by
//!   `$listSearchIndexes` and made with `createSearchIndex`,
//!   `updateSearchIndex` and `dropSearchIndex`; its definition is
//!   `{ fields: [{ type: "vector", path, numDimensions, similarity,
//!   quantization, hnswOptions }, { type: "filter", path }…] }`. `columns`
//!   are the vector path and then the filter paths. Algorithm `hnsw`;
//!   metrics `cosine`, `euclidean` (`l2`), `dotProduct` (`ip`); parameters
//!   `quantization` (`scalar`, `binary`), `m` (`hnswOptions.maxEdges`) and
//!   `ef_construction` (`hnswOptions.numEdgeCandidates`). A definition the
//!   columns and the [`VectorIndex`] can't say whole (several vector
//!   fields, auto-embedding fields, other keys) is kept in the option
//!   `definition`, and writes put the index's settings over it.
//! - `search`: an Atlas Search index whose mapping has `knnVector` fields
//!   (Atlas's older vector search): `columns[0]` is the first of them; its
//!   `dimensions` and `similarity` are the index's, the rest of the
//!   definition is kept whole.
//! - `vector`: Amazon DocumentDB, `createIndex({ f: "vector" }, {
//!   vectorOptions: { type: hnsw | ivfflat, dimensions, similarity:
//!   euclidean | cosine | dotProduct, m, efConstruction, lists } })`. One
//!   field, no filters.
//! - `cosmosSearch`: FerretDB 2 (the DocumentDB extension) and Azure
//!   Cosmos DB for MongoDB vCore, `createIndex({ f: "cosmosSearch" }, {
//!   cosmosSearchOptions: { kind: vector-hnsw | vector-ivf |
//!   vector-diskann, similarity: COS | L2 | IP, dimensions, m,
//!   efConstruction, numLists, maxDegree, lBuild, compression, filter:
//!   [paths] } })`. Read back, the algorithm is `hnsw`, `ivf` or
//!   `diskann`; the MongoDB designer names vCore's by its own names
//!   (`vector-hnsw`…), which make a `cosmosSearch` index there.
//!
//! The dimension is the vector field's type (`vector(N)`) when the field
//! is a column of the table, otherwise the native option (`numDimensions`,
//! `dimensions`). A vector index without a kind gets the server's
//! ([`for_flavor`]), and one of another server's kind is translated to it,
//! with a warning for what it can't take.

use crate::Flavor;
use dbine_driver::vector::{self as v, metric, param};
use dbine_driver::{
    Error, IndexDef, IndexUsage, Result, TableSchema, VectorElement, VectorIndex, VectorIndexSpec,
    VectorMeta, VectorParamSpec,
};
use mongodb::bson::{spec::BinarySubtype, Binary, Bson, Document};
use serde_json::{json, Map, Value};
use std::collections::BTreeMap;

/// Atlas Vector Search / mongot search index.
pub(crate) const SEARCH: &str = "vectorSearch";
/// Amazon DocumentDB vector index.
pub(crate) const DOCDB: &str = "vector";
/// FerretDB 2 / Cosmos DB for MongoDB vCore vector index.
pub(crate) const COSMOS: &str = "cosmosSearch";
/// Atlas Search (full-text) index: a vector one when it maps `knnVector`
/// fields (Atlas's older vector search).
pub(crate) const FTS: &str = "search";

const DTYPE_F32: u8 = 0x27;
const DTYPE_I8: u8 = 0x03;
const DTYPE_BIT: u8 = 0x10;

// ---------------------------------------------------------------- values

/// A BinData vector (subtype 9) as its canonical text, element type and
/// dimensions; `None` when the bytes aren't one.
pub(crate) fn decode(bytes: &[u8]) -> Option<(String, VectorElement, usize)> {
    let (&dtype, rest) = bytes.split_first()?;
    let (&pad, data) = rest.split_first()?;
    match dtype {
        DTYPE_F32 if pad == 0 => {
            let f = v::from_f32_le(data)?;
            Some((v::text(f.iter().copied()), VectorElement::F32, f.len()))
        }
        DTYPE_I8 if pad == 0 => {
            let f = v::from_i8(data);
            Some((v::text(f.iter().copied()), VectorElement::I8, f.len()))
        }
        DTYPE_BIT if pad < 8 && (pad == 0 || !data.is_empty()) => {
            let n = data.len() * 8 - pad as usize;
            let bits = v::from_bits(data, Some(n));
            Some((v::text(bits.iter().copied()), VectorElement::Bit, n))
        }
        _ => None,
    }
}

/// A BinData vector of `values` as `element` (float32 unless int8 or bits).
pub(crate) fn encode(values: &[f64], element: VectorElement) -> Binary {
    let mut bytes = match element {
        VectorElement::I8 => vec![DTYPE_I8, 0],
        VectorElement::Bit => vec![DTYPE_BIT, ((8 - values.len() % 8) % 8) as u8],
        _ => vec![DTYPE_F32, 0],
    };
    bytes.extend(match element {
        VectorElement::I8 => v::to_i8(values),
        VectorElement::Bit => v::to_bits(values),
        _ => v::to_f32_le(values),
    });
    Binary {
        subtype: BinarySubtype::Vector,
        bytes,
    }
}

/// A BinData vector as its text, or `None` when `b` isn't one.
pub(crate) fn binary_text(b: &Binary) -> Option<String> {
    (b.subtype == BinarySubtype::Vector)
        .then(|| decode(&b.bytes))
        .flatten()
        .map(|d| d.0)
}

/// What a value says about the vector it is: a BinData vector's element
/// and length, an array of numbers' length (float), `None` otherwise.
pub(crate) fn value_meta(b: &Bson) -> Option<VectorMeta> {
    match b {
        Bson::Binary(bin) if bin.subtype == BinarySubtype::Vector => {
            let (_, el, n) = decode(&bin.bytes)?;
            Some(VectorMeta::dense(Some(n as u32), el))
        }
        Bson::Array(a)
            if !a.is_empty()
                && a.iter()
                    .all(|x| matches!(x, Bson::Double(_) | Bson::Int32(_) | Bson::Int64(_))) =>
        {
            Some(VectorMeta::dense(Some(a.len() as u32), VectorElement::F32))
        }
        _ => None,
    }
}

/// A vector value (BinData or array of numbers) as its canonical text.
pub(crate) fn text_of(b: &Bson) -> Option<String> {
    match b {
        Bson::Binary(bin) => binary_text(bin),
        Bson::Array(a) => {
            let n: Option<Vec<f64>> = a
                .iter()
                .map(|x| match x {
                    Bson::Double(f) => Some(*f),
                    Bson::Int32(i) => Some(*i as f64),
                    Bson::Int64(i) => Some(*i as f64),
                    _ => None,
                })
                .collect();
            n.map(v::text)
        }
        _ => None,
    }
}

// ---------------------------------------------------------------- field types

/// `vector(N)`, `vector(N, int8)`, `vector(N, bit)` (`vector` without a
/// known dimension).
pub(crate) fn type_name(dims: Option<u32>, element: VectorElement) -> String {
    let el = match element {
        VectorElement::I8 => Some("int8"),
        VectorElement::Bit => Some("bit"),
        _ => None,
    };
    match (dims, el) {
        (Some(n), Some(e)) => format!("vector({n}, {e})"),
        (Some(n), None) => format!("vector({n})"),
        (None, Some(e)) => format!("vector({e})"),
        (None, None) => "vector".into(),
    }
}

/// A `vector(…)` field type's dimension and element; `None` when `t`
/// isn't one.
pub(crate) fn parse_type(t: &str) -> Option<(Option<u32>, VectorElement)> {
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
            el = match part {
                "float32" | "float" | "f32" | "double" | "number" => VectorElement::F32,
                "int8" | "i8" => VectorElement::I8,
                "bit" | "int1" | "packed_bit" | "binary" => VectorElement::Bit,
                _ => return None,
            };
        }
    }
    Some((dims, el))
}

/// The vector type among a field's `|` types (`array|vector(3)`).
pub(crate) fn type_in_list(t: &str) -> Option<(Option<u32>, VectorElement)> {
    t.split('|').find_map(parse_type)
}

// ---------------------------------------------------------------- metrics

fn atlas_metric(m: &str) -> Option<&'static str> {
    Some(match m {
        "cosine" => metric::COSINE,
        "euclidean" => metric::L2,
        "dotProduct" | "dotproduct" => metric::IP,
        _ => return None,
    })
}

fn metric_atlas(m: &str) -> Result<&'static str> {
    Ok(match m {
        metric::COSINE => "cosine",
        metric::L2 | "euclidean" => "euclidean",
        metric::IP | "dotProduct" | "dotproduct" => "dotProduct",
        other => {
            return Err(Error::Query(format!(
                "la distancia «{other}» no está en este motor (cosine, euclidean o dotProduct)"
            )))
        }
    })
}

fn cosmos_metric(m: &str) -> Option<&'static str> {
    Some(match m.to_ascii_uppercase().as_str() {
        "COS" => metric::COSINE,
        "L2" => metric::L2,
        "IP" => metric::IP,
        _ => return None,
    })
}

fn metric_cosmos(m: &str) -> Result<&'static str> {
    Ok(match m {
        metric::COSINE | "cos" | "COS" => "COS",
        metric::L2 | "L2" | "euclidean" => "L2",
        metric::IP | "IP" | "dotProduct" => "IP",
        other => {
            return Err(Error::Query(format!(
                "la distancia «{other}» no está en este motor (COS, L2 o IP)"
            )))
        }
    })
}

/// Standard metrics this family has.
pub(crate) fn known_metric(m: &str) -> bool {
    matches!(m, metric::COSINE | metric::L2 | metric::IP)
}

// ---------------------------------------------------------------- reading

fn num_text(b: &Bson) -> Option<String> {
    match b {
        Bson::Int32(n) => Some(n.to_string()),
        Bson::Int64(n) => Some(n.to_string()),
        Bson::Double(n) if n.fract() == 0.0 => Some((*n as i64).to_string()),
        Bson::Double(n) => Some(n.to_string()),
        Bson::String(s) => Some(s.clone()),
        Bson::Boolean(b) => Some(b.to_string()),
        _ => None,
    }
}

/// A scalar of a search index definition as a parameter's text (`16.0` →
/// `16`); `None` for objects and arrays.
fn json_num_text(x: &Value) -> Option<String> {
    match x {
        Value::Number(n) => Some(match n.as_f64() {
            Some(f) if n.is_f64() && f.fract() == 0.0 && f.abs() < 9e15 => (f as i64).to_string(),
            _ => n.to_string(),
        }),
        Value::String(s) => Some(s.clone()),
        Value::Bool(b) => Some(b.to_string()),
        _ => None,
    }
}

fn std_metric(m: &str) -> String {
    atlas_metric(m)
        .map(str::to_string)
        .unwrap_or_else(|| m.to_string())
}

/// A search index's definition (`latestDefinition`, the one being built,
/// else `definition`) as relaxed Extended JSON.
fn definition_of(spec: &Document) -> Option<Value> {
    spec.get_document("latestDefinition")
        .or_else(|_| spec.get_document("definition"))
        .ok()
        .map(|d| Bson::Document(d.clone()).into_relaxed_extjson())
}

/// The type of a `$listSearchIndexes` entry (`search` when it doesn't say).
fn search_type(spec: &Document) -> &str {
    spec.get_str("type").unwrap_or(FTS)
}

/// The `knnVector` fields of an Atlas Search mapping (`mappings.fields`,
/// nested documents included): path, dimensions and similarity.
fn knn_fields(def: &Value) -> Vec<(String, Option<u64>, Option<String>)> {
    fn walk(fields: &Value, prefix: &str, out: &mut Vec<(String, Option<u64>, Option<String>)>) {
        for (k, spec) in fields.as_object().into_iter().flatten() {
            let path = if prefix.is_empty() {
                k.clone()
            } else {
                format!("{prefix}.{k}")
            };
            // A field maps to one type or a list of them.
            let specs: Vec<&Value> = match spec {
                Value::Array(a) => a.iter().collect(),
                other => vec![other],
            };
            for s in specs {
                match s["type"].as_str() {
                    Some("knnVector") => out.push((
                        path.clone(),
                        s["dimensions"].as_u64(),
                        s["similarity"].as_str().map(str::to_string),
                    )),
                    Some("document" | "embeddedDocuments") => walk(&s["fields"], &path, out),
                    _ => {}
                }
            }
        }
    }
    let mut out = Vec::new();
    walk(&def["mappings"]["fields"], "", &mut out);
    out
}

/// A search index definition as compared: numbers by value (`16` =
/// `16.0`), a `vectorSearch` one's fields in any order, and
/// `quantization: "none"` as no quantization.
pub(crate) fn normalized(def: &Value) -> Value {
    fn nums(v: &Value) -> Value {
        match v {
            Value::Number(n) => match n.as_f64() {
                Some(f) if f.fract() == 0.0 && f.abs() < 9e15 => Value::from(f as i64),
                _ => v.clone(),
            },
            Value::Array(a) => Value::Array(a.iter().map(nums).collect()),
            Value::Object(o) => {
                Value::Object(o.iter().map(|(k, x)| (k.clone(), nums(x))).collect())
            }
            other => other.clone(),
        }
    }
    let mut d = nums(def);
    if let Some(fields) = d.get_mut("fields").and_then(Value::as_array_mut) {
        for f in fields.iter_mut() {
            if let Some(o) = f.as_object_mut() {
                if o.get("quantization").and_then(Value::as_str) == Some("none") {
                    o.remove("quantization");
                }
            }
        }
        fields.sort_by(|a, b| {
            let key = |f: &Value| {
                (
                    f["type"].as_str().unwrap_or("").to_string(),
                    f["path"].as_str().unwrap_or("").to_string(),
                )
            };
            key(a).cmp(&key(b))
        });
    }
    d
}

/// A `$listSearchIndexes` entry that is a vector index: a `vectorSearch`
/// one, or an Atlas Search (`search`) one that maps `knnVector` fields.
/// `None` for any other.
pub(crate) fn search_index_def(spec: &Document) -> Option<IndexDef> {
    let name = spec.get_str("name").ok()?.to_string();
    let def = definition_of(spec)?;
    match search_type(spec) {
        SEARCH => vector_search_index(name, &def),
        FTS => knn_index(name, &def),
        _ => None,
    }
}

/// A `vectorSearch` definition as an index: `columns` are its vector field
/// (else its first field that isn't a filter, like an auto-embedding one)
/// and its filter fields; the vector field's settings are the
/// [`VectorIndex`]. What those can't say (several vector fields,
/// auto-embedding, keys of objects) keeps the definition whole in the
/// option `definition`, which writes start from (see
/// [`search_definition`]).
fn vector_search_index(name: String, def: &Value) -> Option<IndexDef> {
    let fields = def["fields"].as_array()?;
    let primary = fields
        .iter()
        .find(|f| f["type"] == "vector")
        .or_else(|| fields.iter().find(|f| f["type"] != "filter"))?;
    let mut columns = vec![primary["path"].as_str()?.to_string()];
    columns.extend(
        fields
            .iter()
            .filter(|f| f["type"] == "filter")
            .filter_map(|f| f["path"].as_str().map(str::to_string)),
    );
    let mut vi = VectorIndex {
        metric: primary["similarity"].as_str().map(std_metric),
        ..Default::default()
    };
    let mut options = BTreeMap::new();
    let vector = primary["type"] == "vector";
    if vector {
        // Atlas Vector Search is HNSW (its `hnswOptions`).
        vi.algorithm = Some("hnsw".into());
    }
    for (k, x) in primary.as_object()? {
        match k.as_str() {
            "type" | "path" | "similarity" => {}
            "numDimensions" => {
                if let Some(n) = json_num_text(x) {
                    options.insert(k.clone(), n);
                }
            }
            "quantization" => {
                if let Some(q) = x.as_str().filter(|q| *q != "none") {
                    vi.params.insert(param::QUANTIZATION.into(), q.into());
                }
            }
            "hnswOptions" => {
                for (hk, hv) in x.as_object().into_iter().flatten() {
                    let key = match hk.as_str() {
                        "maxEdges" => param::M,
                        "numEdgeCandidates" => param::EF_CONSTRUCTION,
                        // Kept in the definition.
                        _ => continue,
                    };
                    if let Some(n) = json_num_text(hv) {
                        vi.params.insert(key.into(), n);
                    }
                }
            }
            other => {
                if let Some(t) = json_num_text(x) {
                    vi.params.insert(other.to_string(), t);
                }
            }
        }
    }
    let mut ix = IndexDef {
        name,
        columns,
        kind: Some(SEARCH.into()),
        options,
        vector: Some(vi),
        ..Default::default()
    };
    let rebuilt = search_definition(&ix, None).ok();
    if !vector || rebuilt.map(|d| normalized(&d)) != Some(normalized(def)) {
        ix.options.insert("definition".into(), def.to_string());
    }
    Some(ix)
}

/// An Atlas Search definition with `knnVector` fields as a vector index
/// (kind `search`): its first `knnVector` field, its dimensions and
/// similarity. The definition stays whole in the option `definition`.
fn knn_index(name: String, def: &Value) -> Option<IndexDef> {
    let (path, dims, similarity) = knn_fields(def).into_iter().next()?;
    let mut options = BTreeMap::new();
    if let Some(n) = dims {
        options.insert("dimensions".into(), n.to_string());
    }
    options.insert("definition".into(), def.to_string());
    Some(IndexDef {
        name,
        columns: vec![path],
        kind: Some(FTS.into()),
        options,
        vector: Some(VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: similarity.as_deref().map(std_metric),
            ..Default::default()
        }),
        ..Default::default()
    })
}

/// A `listIndexes` entry that is a DocumentDB (`vector`) or FerretDB /
/// vCore (`cosmosSearch`) vector index; `None` for any other index.
pub(crate) fn plain_index_def(spec: &Document) -> Option<IndexDef> {
    let name = spec.get_str("name").ok()?.to_string();
    let key = spec.get_document("key").ok()?;
    let (path, kind) = key
        .iter()
        .find(|(_, x)| matches!(x.as_str(), Some(DOCDB | COSMOS)))?;
    let kind = kind.as_str()?.to_string();
    let opts = spec
        .get_document(if kind == DOCDB {
            "vectorOptions"
        } else {
            "cosmosSearchOptions"
        })
        .ok()?;
    let mut columns = vec![path.clone()];
    let mut vi = VectorIndex::default();
    let mut options = BTreeMap::new();
    for (k, x) in opts {
        match (kind.as_str(), k.as_str()) {
            (_, "dimensions") => {
                if let Some(n) = num_text(x) {
                    options.insert("dimensions".into(), n);
                }
            }
            (DOCDB, "type") => vi.algorithm = x.as_str().map(str::to_string),
            (COSMOS, "kind") => {
                vi.algorithm = x
                    .as_str()
                    .map(|k| k.strip_prefix("vector-").unwrap_or(k).to_string())
            }
            (DOCDB, "similarity") => vi.metric = x.as_str().map(std_metric),
            (COSMOS, "similarity") => {
                vi.metric = x.as_str().map(|m| {
                    cosmos_metric(m)
                        .map(str::to_string)
                        .unwrap_or_else(|| m.to_string())
                })
            }
            (COSMOS, "filter") => {
                if let Some(a) = x.as_array() {
                    columns.extend(a.iter().filter_map(Bson::as_str).map(str::to_string));
                }
            }
            (_, other) => {
                let key = match other {
                    "efConstruction" => param::EF_CONSTRUCTION,
                    "lists" | "numLists" => param::LISTS,
                    "compression" => param::QUANTIZATION,
                    "m" => param::M,
                    o => o,
                };
                if let Some(t) = num_text(x) {
                    vi.params.insert(key.to_string(), t);
                }
            }
        }
    }
    Some(IndexDef {
        name,
        columns,
        kind: Some(kind),
        options,
        vector: Some(vi),
        ..Default::default()
    })
}

/// A vector search index for the explorer, with its status (`READY`,
/// `BUILDING`, `PENDING`, `FAILED`…): kind `VECTOR SEARCH` (each field
/// with its type, a vector one with its dimension and similarity) or, for
/// an Atlas Search index with `knnVector` fields, `SEARCH` (those fields).
pub(crate) fn search_usage(spec: &Document) -> Option<IndexUsage> {
    let name = spec.get_str("name").ok()?.to_string();
    let def = definition_of(spec).unwrap_or(Value::Null);
    let (kind, key_columns) = match search_type(spec) {
        SEARCH => (
            "VECTOR SEARCH",
            def["fields"]
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|f| {
                    let (t, p) = (f["type"].as_str()?, f["path"].as_str()?);
                    Some(match (t, f.get("numDimensions").and_then(json_num_text)) {
                        ("vector", Some(n)) => format!(
                            "{p} (vector {n}, {})",
                            f["similarity"].as_str().unwrap_or("?")
                        ),
                        _ => format!("{p} ({t})"),
                    })
                })
                .collect::<Vec<_>>(),
        ),
        FTS => {
            let knn = knn_fields(&def);
            if knn.is_empty() {
                return None;
            }
            (
                "SEARCH",
                knn.into_iter()
                    .map(|(p, n, s)| {
                        format!(
                            "{p} (knnVector {}, {})",
                            n.map(|n| n.to_string()).unwrap_or("?".into()),
                            s.as_deref().unwrap_or("?")
                        )
                    })
                    .collect(),
            )
        }
        _ => return None,
    };
    let status = spec.get_str("status").unwrap_or("");
    Some(IndexUsage {
        name,
        kind: format!("{kind} {status}").trim().to_string(),
        key_columns,
        ..Default::default()
    })
}

/// Index kinds that are vector indexes.
pub(crate) fn is_vector_kind(kind: Option<&str>) -> bool {
    kind.is_some_and(|k| {
        [SEARCH, DOCDB, COSMOS, FTS]
            .iter()
            .any(|x| k.trim().eq_ignore_ascii_case(x))
    })
}

/// The index is a search index (`vectorSearch`, or Atlas Search's `search`
/// with `knnVector` fields): it has its own commands.
pub(crate) fn is_search(ix: &IndexDef) -> bool {
    ix.kind.as_deref().is_some_and(|k| {
        k.trim().eq_ignore_ascii_case(SEARCH) || k.trim().eq_ignore_ascii_case(FTS)
    })
}

/// An Atlas Search index with `knnVector` fields.
pub(crate) fn is_knn(ix: &IndexDef) -> bool {
    ix.kind
        .as_deref()
        .is_some_and(|k| k.trim().eq_ignore_ascii_case(FTS))
}

fn dims_key(ix: &IndexDef) -> &'static str {
    if ix
        .kind
        .as_deref()
        .is_some_and(|k| k.trim().eq_ignore_ascii_case(SEARCH))
    {
        "numDimensions"
    } else {
        "dimensions"
    }
}

/// `ix` with its dimension in its native option, from the vector field's
/// type in `t` when the option isn't set.
pub(crate) fn with_dims(t: &TableSchema, ix: &IndexDef) -> IndexDef {
    let mut out = ix.clone();
    if ix.vector.is_none() && !is_vector_kind(ix.kind.as_deref()) {
        return out;
    }
    let key = dims_key(ix);
    let Some(path) = ix.columns.first() else {
        return out;
    };
    let from_column = t
        .columns
        .iter()
        .find(|c| c.name == *path)
        .and_then(|c| type_in_list(&c.data_type))
        .and_then(|(d, _)| d);
    if let Some(n) = from_column {
        out.options.insert(key.into(), n.to_string());
    }
    out
}

/// The vector fields an index declares, with their dimension: the vector
/// fields of its whole definition when it keeps one, else its first
/// column. The index's own field (`columns[0]`) takes the dimension of
/// its native option when that is set.
fn vector_fields_of(ix: &IndexDef) -> Vec<(String, Option<u32>)> {
    let dims = ix
        .options
        .get(dims_key(ix))
        .and_then(|n| n.trim().parse::<u32>().ok());
    let def = ix
        .options
        .get("definition")
        .and_then(|d| serde_json::from_str::<Value>(d).ok());
    let mut out: Vec<(String, Option<u32>)> = match &def {
        Some(d) if is_knn(ix) => knn_fields(d)
            .into_iter()
            .map(|(p, n, _)| (p, n.map(|n| n as u32)))
            .collect(),
        Some(d) => d["fields"]
            .as_array()
            .into_iter()
            .flatten()
            .filter(|f| f["type"] == "vector")
            .filter_map(|f| {
                Some((
                    f["path"].as_str()?.to_string(),
                    f["numDimensions"].as_u64().map(|n| n as u32),
                ))
            })
            .collect(),
        None => ix
            .columns
            .first()
            .map(|p| (p.clone(), dims))
            .into_iter()
            .collect(),
    };
    if let (Some(p), Some(n)) = (ix.columns.first(), dims) {
        if let Some(e) = out.iter_mut().find(|(q, _)| q == p) {
            e.1 = Some(n);
        }
    }
    out
}

/// The read-back schema: each vector field of the vector indexes gets its
/// `vector(N)` type (a column is added when no sampled document has it),
/// and the index's dimension leaves its options when its field's type
/// carries it.
pub(crate) fn describe_fields(t: &mut TableSchema) {
    for i in 0..t.indexes.len() {
        if t.indexes[i].vector.is_none() {
            continue;
        }
        let own = t.indexes[i].columns.first().cloned();
        let mut typed = false;
        for (path, dims) in vector_fields_of(&t.indexes[i]) {
            if path.contains('.') {
                continue;
            }
            match t.columns.iter_mut().find(|c| c.name == path) {
                Some(c) => {
                    let el = type_in_list(&c.data_type)
                        .map(|(_, e)| e)
                        .unwrap_or_default();
                    c.data_type = type_name(dims, el);
                }
                None => t.columns.push(dbine_driver::ColumnDef {
                    name: path.clone(),
                    data_type: type_name(dims, VectorElement::F32),
                    nullable: true,
                    ..Default::default()
                }),
            }
            typed |= dims.is_some() && own.as_deref() == Some(path.as_str());
        }
        if typed {
            let key = dims_key(&t.indexes[i]);
            t.indexes[i].options.remove(key);
        }
    }
}

// ---------------------------------------------------------------- writing

fn number(s: &str) -> Value {
    serde_json::from_str::<Value>(s.trim())
        .ok()
        .filter(|v| v.is_number() || v.is_boolean())
        .unwrap_or_else(|| Value::String(s.trim().to_string()))
}

fn dims_of(ix: &IndexDef, given: Option<u32>) -> Result<u32> {
    let key = dims_key(ix);
    given
        .or_else(|| ix.options.get(key).and_then(|n| n.trim().parse().ok()))
        .ok_or_else(|| {
            Error::Query(format!(
                "el índice vectorial {} necesita la dimensión: el campo «{}» tiene que ser de tipo vector(N)",
                ix.name,
                ix.columns.first().map(String::as_str).unwrap_or("")
            ))
        })
}

/// The vector field of a `vectorSearch` definition: `ix`'s settings over
/// `base` (the server's field: its other keys stay).
fn vector_field(
    ix: &IndexDef,
    base: &Map<String, Value>,
    path: &str,
    dims: Option<u32>,
) -> Result<Map<String, Value>> {
    let vi = ix.vector.clone().unwrap_or_default();
    let mut f = base.clone();
    f.insert("type".into(), "vector".into());
    f.insert("path".into(), path.into());
    let n = match dims_of(ix, dims) {
        Ok(n) => Value::from(n),
        Err(e) => f
            .get("numDimensions")
            .filter(|n| n.is_number())
            .cloned()
            .ok_or(e)?,
    };
    f.insert("numDimensions".into(), n);
    match vi.metric.as_deref() {
        Some(m) => {
            f.insert("similarity".into(), metric_atlas(m)?.into());
        }
        None => {
            f.entry("similarity").or_insert_with(|| "cosine".into());
        }
    }
    match vi
        .params
        .get(param::QUANTIZATION)
        .map(|q| q.trim())
        .filter(|q| !q.is_empty())
    {
        Some(q) => {
            f.insert("quantization".into(), q.into());
        }
        // An explicit `none` is no quantization: it stays as written.
        None if f.get("quantization").and_then(Value::as_str) == Some("none") => {}
        None => {
            f.remove("quantization");
        }
    }
    let mut hnsw = f
        .get("hnswOptions")
        .and_then(Value::as_object)
        .cloned()
        .unwrap_or_default();
    hnsw.remove("maxEdges");
    hnsw.remove("numEdgeCandidates");
    for (k, x) in vi.params.iter().filter(|(_, x)| !x.trim().is_empty()) {
        match k.as_str() {
            param::M | "maxEdges" => {
                hnsw.insert("maxEdges".into(), number(x));
            }
            param::EF_CONSTRUCTION | "numEdgeCandidates" => {
                hnsw.insert("numEdgeCandidates".into(), number(x));
            }
            _ => {}
        }
    }
    if hnsw.is_empty() {
        f.remove("hnswOptions");
    } else {
        f.insert("hnswOptions".into(), Value::Object(hnsw));
    }
    // Other keys: a scalar one is a parameter (as read), so the parameters
    // say which stay; objects and lists stay as they are.
    const OWN: [&str; 6] = [
        "type",
        "path",
        "numDimensions",
        "similarity",
        "quantization",
        "hnswOptions",
    ];
    f.retain(|k, x| {
        OWN.contains(&k.as_str()) || json_num_text(x).is_none() || vi.params.contains_key(k)
    });
    for (k, x) in &vi.params {
        let own = matches!(
            k.as_str(),
            param::QUANTIZATION
                | param::M
                | param::EF_CONSTRUCTION
                | "maxEdges"
                | "numEdgeCandidates"
        );
        if !own && !x.trim().is_empty() {
            f.insert(k.clone(), number(x));
        }
    }
    Ok(f)
}

/// A search index's `definition`: the one it keeps (the option
/// `definition`, from the server) with the index's own settings over it,
/// else a new one. For `vectorSearch`, the vector field (`columns[0]`)
/// takes the dimension, metric and parameters, and the filter fields are
/// `columns[1..]` (those the definition had keep their place, new ones go
/// last); its other fields stay. For Atlas Search (`search`), its
/// `knnVector` field takes the dimension and metric.
pub(crate) fn search_definition(ix: &IndexDef, dims: Option<u32>) -> Result<Value> {
    let base = match ix.options.get("definition") {
        Some(d) => Some(crate::ddl::relaxed(d, |e| {
            format!("la definición del índice {} no es válida: {e}", ix.name)
        })?),
        None => None,
    };
    let path = ix
        .columns
        .first()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error::Query(format!("el índice {} no tiene campos", ix.name)))?;
    if is_knn(ix) {
        return knn_definition(ix, base, path, dims);
    }
    let filters: Vec<&str> = ix.columns[1..]
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .collect();
    let mut out = base.filter(Value::is_object).unwrap_or_else(|| json!({}));
    let had: Vec<Value> = out["fields"].as_array().cloned().unwrap_or_default();
    let primary = had
        .iter()
        .position(|f| f["type"] == "vector")
        .or_else(|| had.iter().position(|f| f["type"] != "filter"));
    let mut fields = Vec::new();
    let mut placed: Vec<String> = Vec::new();
    for (i, f) in had.iter().enumerate() {
        if Some(i) == primary {
            if f["type"] == "vector" {
                let base = f.as_object().cloned().unwrap_or_default();
                fields.push(Value::Object(vector_field(ix, &base, path, dims)?));
            } else {
                let mut f = f.clone();
                f["path"] = path.into();
                fields.push(f);
            }
        } else if f["type"] == "filter" {
            if let Some(p) = f["path"]
                .as_str()
                .filter(|p| filters.contains(p) && !placed.iter().any(|x| x == p))
            {
                placed.push(p.to_string());
                fields.push(f.clone());
            }
        } else {
            fields.push(f.clone());
        }
    }
    if primary.is_none() {
        fields.insert(0, Value::Object(vector_field(ix, &Map::new(), path, dims)?));
    }
    for p in filters {
        if !placed.iter().any(|x| x == p) {
            placed.push(p.to_string());
            fields.push(json!({ "type": "filter", "path": p }));
        }
    }
    out["fields"] = Value::Array(fields);
    Ok(out)
}

/// The mapping of the `knnVector` field at `path` (`a.b`: `b` inside the
/// `document` mapping of `a`).
fn knn_spec_mut<'a>(def: &'a mut Value, path: &str) -> Option<&'a mut Value> {
    let parts: Vec<&str> = path.split('.').collect();
    let mut fields = def.pointer_mut("/mappings/fields")?;
    for (i, p) in parts.iter().enumerate() {
        let last = i + 1 == parts.len();
        let wanted = |s: &Value| {
            if last {
                s["type"] == "knnVector"
            } else {
                matches!(s["type"].as_str(), Some("document" | "embeddedDocuments"))
            }
        };
        let entry = fields.get_mut(*p)?;
        let spec = match entry {
            Value::Array(a) => a.iter_mut().find(|s| wanted(s))?,
            other if wanted(other) => other,
            _ => return None,
        };
        if last {
            return Some(spec);
        }
        fields = spec.get_mut("fields")?;
    }
    None
}

/// An Atlas Search definition with the index's `knnVector` field (its
/// path, dimension and metric) over the one it keeps.
fn knn_definition(
    ix: &IndexDef,
    base: Option<Value>,
    path: &str,
    dims: Option<u32>,
) -> Result<Value> {
    let vi = ix.vector.clone().unwrap_or_default();
    let mut d = base
        .filter(Value::is_object)
        .unwrap_or_else(|| json!({ "mappings": { "dynamic": false, "fields": {} } }));
    // A top-level field renamed: its mapping moves with it.
    if let Some(old) = knn_fields(&d).into_iter().next().map(|f| f.0) {
        if old != path && !old.contains('.') && !path.contains('.') {
            if let Some(fields) = d
                .pointer_mut("/mappings/fields")
                .and_then(Value::as_object_mut)
            {
                if let Some(m) = fields.remove(&old) {
                    fields.insert(path.to_string(), m);
                }
            }
        }
    }
    if knn_spec_mut(&mut d, path).is_none() {
        if path.contains('.') {
            return Err(Error::Query(format!(
                "el índice {} no tiene un campo knnVector en «{path}»",
                ix.name
            )));
        }
        if d.pointer("/mappings/fields")
            .and_then(Value::as_object)
            .is_none()
        {
            d["mappings"]["fields"] = json!({});
        }
        d["mappings"]["fields"][path] = json!({ "type": "knnVector" });
    }
    let dims = dims_of(ix, dims);
    let similarity = vi.metric.as_deref().map(metric_atlas).transpose()?;
    let spec = knn_spec_mut(&mut d, path).expect("the knnVector mapping was just made");
    match dims {
        Ok(n) => spec["dimensions"] = n.into(),
        Err(e) if !spec["dimensions"].is_number() => return Err(e),
        Err(_) => {}
    }
    match similarity {
        Some(s) => spec["similarity"] = s.into(),
        None if spec["similarity"].is_null() => spec["similarity"] = "cosine".into(),
        None => {}
    }
    Ok(d)
}

/// `createSearchIndex` for a search index.
pub(crate) fn create_search(coll: &str, ix: &IndexDef) -> Result<String> {
    let def = search_definition(ix, None)?;
    Ok(format!(
        "{coll}.createSearchIndex({})",
        json!({
            "name": ix.name.trim(),
            "type": if is_knn(ix) { FTS } else { SEARCH },
            "definition": def
        })
    ))
}

/// `updateSearchIndex` with `ix`'s definition.
pub(crate) fn update_search(coll: &str, ix: &IndexDef) -> Result<String> {
    Ok(format!(
        "{coll}.updateSearchIndex({}, {})",
        Value::String(ix.name.trim().into()),
        search_definition(ix, None)?
    ))
}

pub(crate) fn drop_search(coll: &str, name: &str) -> String {
    format!(
        "{coll}.dropSearchIndex({})",
        Value::String(name.trim().into())
    )
}

/// Two search indexes build the same: same kind and the same definition
/// (as [`normalized`]), each with its table's dimension.
pub(crate) fn same_search(a: &IndexDef, b: &IndexDef) -> bool {
    is_knn(a) == is_knn(b)
        && matches!(
            (search_definition(a, None), search_definition(b, None)),
            (Ok(x), Ok(y)) if normalized(&x) == normalized(&y)
        )
}

/// Key and options (`vectorOptions` / `cosmosSearchOptions`) of a
/// DocumentDB or FerretDB vector index, `name` and `unique` aside.
pub(crate) fn plain_index_parts(ix: &IndexDef) -> Result<(Map<String, Value>, Map<String, Value>)> {
    let kind = ix.kind.as_deref().unwrap_or(DOCDB);
    let cosmos = kind == COSMOS;
    let path = ix
        .columns
        .first()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error::Query(format!("el índice {} no tiene campos", ix.name)))?;
    if !cosmos && ix.columns.len() > 1 {
        return Err(Error::Query(format!(
            "el índice vectorial {} de DocumentDB toma un solo campo, sin filtros",
            ix.name
        )));
    }
    let vi = ix.vector.clone().unwrap_or_default();
    let mut o = Map::new();
    let algorithm = vi
        .algorithm
        .as_deref()
        .unwrap_or("hnsw")
        .to_ascii_lowercase();
    // vCore's own names (`vector-ivf`) as the algorithm's.
    let algorithm = algorithm
        .strip_prefix("vector-")
        .unwrap_or(&algorithm)
        .to_string();
    if cosmos {
        let a = match algorithm.as_str() {
            "ivf" | "ivfflat" => "ivf",
            other => other,
        };
        o.insert("kind".into(), format!("vector-{a}").into());
        o.insert(
            "similarity".into(),
            metric_cosmos(vi.metric.as_deref().unwrap_or(metric::COSINE))?.into(),
        );
    } else {
        o.insert(
            "type".into(),
            (if algorithm == "ivf" {
                "ivfflat"
            } else {
                algorithm.as_str()
            })
            .into(),
        );
        o.insert(
            "similarity".into(),
            metric_atlas(vi.metric.as_deref().unwrap_or(metric::COSINE))?.into(),
        );
    }
    o.insert("dimensions".into(), dims_of(ix, None)?.into());
    for (k, x) in &vi.params {
        let x = x.trim();
        // Empty, or no compression: the server's default.
        if x.is_empty() || (k == param::QUANTIZATION && x.eq_ignore_ascii_case("none")) {
            continue;
        }
        let key = match (k.as_str(), cosmos) {
            (param::EF_CONSTRUCTION, _) => "efConstruction",
            (param::LISTS, true) => "numLists",
            (param::LISTS, false) => "lists",
            (param::QUANTIZATION, true) => "compression",
            (param::QUANTIZATION, false) => {
                return Err(Error::Query(format!(
                    "el índice vectorial {} de DocumentDB no comprime los vectores",
                    ix.name
                )))
            }
            (other, _) => other,
        };
        o.insert(key.into(), number(x));
    }
    if cosmos && ix.columns.len() > 1 {
        o.insert(
            "filter".into(),
            ix.columns[1..]
                .iter()
                .map(|c| Value::String(c.trim().into()))
                .collect(),
        );
    }
    let mut key = Map::new();
    key.insert(path.into(), kind.into());
    let mut opts = Map::new();
    opts.insert(
        if cosmos {
            "cosmosSearchOptions"
        } else {
            "vectorOptions"
        }
        .into(),
        Value::Object(o),
    );
    Ok((key, opts))
}

// ---------------------------------------------------------------- flavors

/// The kind of vector index `flavor` makes.
pub(crate) fn kind_of(flavor: Flavor) -> &'static str {
    match flavor {
        Flavor::Mongo => SEARCH,
        Flavor::DocumentDb => DOCDB,
        Flavor::Ferret => COSMOS,
    }
}

/// The algorithms of Azure Cosmos DB for MongoDB vCore, by its own names
/// (`cosmosSearchOptions.kind`): on a MongoDB connection they make a
/// `cosmosSearch` index; any other algorithm, an Atlas `vectorSearch` one.
const VCORE: [&str; 3] = ["vector-hnsw", "vector-ivf", "vector-diskann"];

/// `t`'s vector indexes in `flavor`'s terms: a missing kind gets the
/// server's (on MongoDB, `cosmosSearch` for a vCore algorithm, see
/// [`VCORE`]), another server's kind is translated, and what the kind
/// can't take goes with a warning (also for an index from another engine,
/// which comes without a kind). MongoDB keeps `cosmosSearch` (Cosmos DB
/// vCore speaks the MongoDB protocol) and Atlas Search's `search`.
pub(crate) fn for_flavor(t: &TableSchema, flavor: Flavor) -> (TableSchema, Vec<String>) {
    let mut out = t.clone();
    let mut warnings = Vec::new();
    for ix in out.indexes.iter_mut() {
        if ix.vector.is_none() && !is_vector_kind(ix.kind.as_deref()) {
            continue;
        }
        let from = ix
            .kind
            .clone()
            .filter(|k| is_vector_kind(Some(k)))
            .unwrap_or_default();
        let vcore = ix
            .vector
            .as_ref()
            .and_then(|v| v.algorithm.as_deref())
            .is_some_and(|a| VCORE.iter().any(|x| a.trim().eq_ignore_ascii_case(x)));
        let to = match flavor {
            Flavor::Mongo if from.eq_ignore_ascii_case(COSMOS) || (from.is_empty() && vcore) => {
                COSMOS
            }
            Flavor::Mongo if from.eq_ignore_ascii_case(FTS) => FTS,
            f => kind_of(f),
        };
        if from.eq_ignore_ascii_case(to) {
            ix.kind = Some(to.into());
            continue;
        }
        let mut vi = ix.vector.clone().unwrap_or_default();
        let mut lost: Vec<String> = Vec::new();
        let mut changed: Vec<String> = Vec::new();
        // The dimension's option has another name.
        if let Some(d) = ["numDimensions", "dimensions"]
            .iter()
            .find_map(|k| ix.options.remove(*k))
        {
            ix.options.insert(
                if to == SEARCH {
                    "numDimensions"
                } else {
                    "dimensions"
                }
                .into(),
                d,
            );
        }
        if ix.options.remove("definition").is_some() {
            lost.push(format!("la definición completa de {from}"));
        }
        let keep: &[&str] = match to {
            SEARCH => &[param::M, param::EF_CONSTRUCTION, param::QUANTIZATION],
            DOCDB => &[param::M, param::EF_CONSTRUCTION, param::LISTS],
            _ => &[
                param::M,
                param::EF_CONSTRUCTION,
                param::LISTS,
                param::QUANTIZATION,
                "maxDegree",
                "lBuild",
            ],
        };
        let quant_ok = |q: &str| match to {
            SEARCH => matches!(q, "scalar" | "binary" | "none"),
            _ => matches!(q, "half" | "pq"),
        };
        vi.params.retain(|k, x| {
            if x.trim().is_empty() {
                return false;
            }
            let ok = keep.contains(&k.as_str()) && (k != param::QUANTIZATION || quant_ok(x.trim()));
            if !ok {
                lost.push(format!("{k}={x}"));
            }
            ok
        });
        let algorithm = vi.algorithm.as_deref().map(|a| {
            let a = a.trim().to_ascii_lowercase();
            a.strip_prefix("vector-").map(str::to_string).unwrap_or(a)
        });
        vi.algorithm = match (to, algorithm.as_deref()) {
            (SEARCH, Some(a)) if a != "hnsw" => {
                lost.push(format!("el algoritmo {a}"));
                Some("hnsw".into())
            }
            (SEARCH, _) => Some("hnsw".into()),
            (DOCDB, Some("ivf" | "ivfflat")) => Some("ivfflat".into()),
            (DOCDB, Some(a)) if a != "hnsw" => {
                lost.push(format!("el algoritmo {a}"));
                Some("hnsw".into())
            }
            (DOCDB, _) => Some("hnsw".into()),
            (_, Some("ivf" | "ivfflat")) => Some("ivf".into()),
            (_, Some(a)) if a == "diskann" || a == "hnsw" => Some(a.into()),
            (_, Some(a)) => {
                lost.push(format!("el algoritmo {a}"));
                Some("hnsw".into())
            }
            (_, None) => Some("hnsw".into()),
        };
        match vi.metric.clone().as_deref() {
            Some(m @ ("l2_squared" | "euclidean_squared")) => {
                changed.push(format!(
                    "la distancia {m} queda l2 (euclidean: los vecinos más cercanos son los mismos, la distancia que devuelve es su raíz)"
                ));
                vi.metric = Some(metric::L2.into());
            }
            Some(m) if !known_metric(m) => {
                lost.push(format!("la distancia {m} (queda cosine)"));
                vi.metric = Some(metric::COSINE.into());
            }
            _ => {}
        }
        if to == DOCDB && ix.columns.len() > 1 {
            lost.push(format!("los filtros ({})", ix.columns[1..].join(", ")));
            ix.columns.truncate(1);
        }
        if !lost.is_empty() {
            changed.insert(0, format!("pierde {}", lost.join(", ")));
        }
        if !changed.is_empty() {
            let how = if from.is_empty() {
                format!("se crea como {to}")
            } else {
                format!("pasa de {from} a {to}")
            };
            warnings.push(format!(
                "El índice vectorial {} {how}: {}.",
                ix.name,
                changed.join("; ")
            ));
        }
        ix.kind = Some(to.into());
        ix.vector = Some(vi);
    }
    (out, warnings)
}

/// The designer's vector index choices for `flavor`. On MongoDB, the
/// algorithm also picks the server: `hnsw` is Atlas Vector Search (and
/// MongoDB with mongot), the [`VCORE`] ones Azure Cosmos DB for MongoDB
/// vCore; each offers its own parameters.
pub(crate) fn designer_spec(flavor: Flavor) -> VectorIndexSpec {
    let p = |key, algorithms: &[&'static str], default| VectorParamSpec {
        key,
        algorithms: algorithms.to_vec(),
        default,
    };
    match flavor {
        Flavor::Mongo => VectorIndexSpec {
            algorithms: vec!["hnsw", VCORE[0], VCORE[1], VCORE[2]],
            metrics: vec![metric::COSINE, metric::L2, metric::IP],
            params: vec![
                p(param::QUANTIZATION, &["hnsw"], "none"),
                p(param::M, &["hnsw"], "16"),
                p(param::EF_CONSTRUCTION, &["hnsw"], "100"),
                // vCore (a vCore index read back says `ivf`, `diskann`).
                p(param::M, &[VCORE[0]], "16"),
                p(param::EF_CONSTRUCTION, &[VCORE[0]], "64"),
                p(param::LISTS, &[VCORE[1], "ivf"], "1"),
                p("maxDegree", &[VCORE[2], "diskann"], "32"),
                p("lBuild", &[VCORE[2], "diskann"], "50"),
                p(
                    param::QUANTIZATION,
                    &[VCORE[0], VCORE[1], VCORE[2], "ivf", "diskann"],
                    "",
                ),
            ],
            filter_columns: true,
        },
        Flavor::DocumentDb => VectorIndexSpec {
            algorithms: vec!["hnsw", "ivfflat"],
            metrics: vec![metric::L2, metric::COSINE, metric::IP],
            params: vec![
                p(param::M, &["hnsw"], "16"),
                p(param::EF_CONSTRUCTION, &["hnsw"], "64"),
                p(param::LISTS, &["ivfflat"], ""),
            ],
            filter_columns: false,
        },
        Flavor::Ferret => VectorIndexSpec {
            algorithms: vec!["hnsw", "ivf"],
            metrics: vec![metric::COSINE, metric::L2, metric::IP],
            params: vec![
                p(param::M, &["hnsw"], "16"),
                p(param::EF_CONSTRUCTION, &["hnsw"], "64"),
                p(param::LISTS, &["ivf"], "1"),
                p(param::QUANTIZATION, &["hnsw"], ""),
            ],
            filter_columns: true,
        },
    }
}

// ---------------------------------------------------------------- edits

/// `element` when every value fits it as it is (int8: whole numbers in
/// -128..=127; bit: 0 and 1), else float32: a vector is never rounded to
/// fit a form.
pub(crate) fn fitting(element: VectorElement, values: &[f64]) -> VectorElement {
    let fits = match element {
        VectorElement::I8 => values
            .iter()
            .all(|x| x.fract() == 0.0 && (-128.0..=127.0).contains(x)),
        VectorElement::Bit => values.iter().all(|x| *x == 0.0 || *x == 1.0),
        _ => true,
    };
    if fits {
        element
    } else {
        VectorElement::F32
    }
}

/// `$set` value of an edit that writes vector `values` over `field`,
/// keeping the stored form: where the document holds a BinData vector, a
/// BinData of the same element (float32 when the new numbers don't fit
/// it), otherwise the array. An aggregation expression for a pipeline
/// update.
///
/// The stored element is told exactly, never by size alone: with the value
/// the field had (`old`, the grid's row), the field is compared with that
/// value's BinData in each element (only the element it was stored in is
/// equal); without it, with the byte range of each element at the new
/// length (a BinData compares by length, then subtype, then bytes, so a
/// range holds only that length and that element's leading byte). A
/// BinData none of them matches (changed meanwhile, or a new length
/// without `old`) gets float32.
pub(crate) fn keep_form_expr(field: &str, values: &[f64], old: Option<&[f64]>) -> String {
    use base64::Engine;
    let lit = |bytes: &[u8]| {
        format!(
            "BinData(9, \"{}\")",
            base64::engine::general_purpose::STANDARD.encode(bytes)
        )
    };
    let new_as = |el| lit(&encode(values, fitting(el, values)).bytes);
    let path = if field.contains('.') || field.starts_with('$') {
        format!(
            "{{ $getField: {{ $literal: {} }} }}",
            Value::String(field.into())
        )
    } else {
        Value::String(format!("${field}")).to_string()
    };
    const ELEMENTS: [VectorElement; 3] =
        [VectorElement::F32, VectorElement::I8, VectorElement::Bit];
    let mut branches = Vec::new();
    match old.filter(|o| !o.is_empty()) {
        Some(old) => {
            let mut seen: Vec<Vec<u8>> = Vec::new();
            for el in ELEMENTS.into_iter().filter(|el| fitting(*el, old) == *el) {
                let stored = encode(old, el).bytes;
                if seen.contains(&stored) {
                    continue;
                }
                branches.push(format!(
                    "{{ case: {{ $eq: [{path}, {}] }}, then: {} }}",
                    lit(&stored),
                    new_as(el)
                ));
                seen.push(stored);
            }
        }
        None => {
            let n = values.len();
            for el in ELEMENTS {
                let (dtype, pad, len) = match el {
                    VectorElement::I8 => (DTYPE_I8, 0, n),
                    VectorElement::Bit => (DTYPE_BIT, ((8 - n % 8) % 8) as u8, n.div_ceil(8)),
                    _ => (DTYPE_F32, 0, 4 * n),
                };
                let bound = |fill: u8| {
                    let mut b = vec![dtype, pad];
                    b.resize(2 + len, fill);
                    lit(&b)
                };
                branches.push(format!(
                    "{{ case: {{ $and: [{{ $gte: [{path}, {}] }}, {{ $lte: [{path}, {}] }}] }}, then: {} }}",
                    bound(0x00),
                    bound(0xFF),
                    new_as(el)
                ));
            }
        }
    }
    let binary = format!(
        "{{ $switch: {{ branches: [{}], default: {} }} }}",
        branches.join(", "),
        new_as(VectorElement::F32)
    );
    // The array as doubles, as an array vector is stored.
    let array = Value::Array(values.iter().map(|x| dbine_driver::json_f64(*x)).collect());
    format!("{{ $cond: [{{ $eq: [{{ $type: {path} }}, \"binData\"] }}, {binary}, {array}] }}")
}

// ---------------------------------------------------------------- inserts

/// BinData vector fields that recent results showed, by collection name:
/// field → element. An insert from the grid writes its vectors in that
/// form (see `ddl::insert_script`), so a new document doesn't mix arrays
/// into a collection of BinData vectors. Edit scripts know only the
/// collection's name, so this is by name, for [`FORM_TTL`].
type Forms = std::collections::HashMap<
    String,
    (
        std::time::Instant,
        std::collections::HashMap<String, VectorElement>,
    ),
>;

fn forms() -> &'static std::sync::Mutex<Forms> {
    static FORMS: std::sync::OnceLock<std::sync::Mutex<Forms>> = std::sync::OnceLock::new();
    FORMS.get_or_init(Default::default)
}

const FORM_TTL: std::time::Duration = std::time::Duration::from_secs(30 * 60);

/// Notes that a result from `coll` showed BinData vectors in `fields`.
pub(crate) fn note_binary_fields(coll: &str, fields: &[(String, VectorElement)]) {
    if fields.is_empty() {
        return;
    }
    if let Ok(mut m) = forms().lock() {
        m.retain(|_, (at, _)| at.elapsed() < FORM_TTL);
        let e = m
            .entry(coll.to_string())
            .or_insert_with(|| (std::time::Instant::now(), Default::default()));
        e.0 = std::time::Instant::now();
        e.1.extend(fields.iter().cloned());
    }
}

/// The BinData vector fields recent results of `coll` showed.
pub(crate) fn binary_fields(coll: &str) -> std::collections::HashMap<String, VectorElement> {
    forms()
        .lock()
        .ok()
        .and_then(|m| {
            m.get(coll)
                .filter(|(at, _)| at.elapsed() < FORM_TTL)
                .map(|(_, f)| f.clone())
        })
        .unwrap_or_default()
}

/// A vector value of an insert in a BinData field: `BinData(9, "…")` of
/// its element (float32 when the numbers don't fit it).
pub(crate) fn binary_literal(values: &[f64], element: VectorElement) -> String {
    use base64::Engine;
    format!(
        "BinData(9, \"{}\")",
        base64::engine::general_purpose::STANDARD
            .encode(encode(values, fitting(element, values)).bytes)
    )
}

// ---------------------------------------------------------------- session

/// Vector fields of the collections results came from (`db.coll` →
/// field → what it holds), for [`CACHE_TTL`]: a result over a collection
/// with numeric arrays needs its vector indexes to know which ones are
/// vectors. Cleared when the session runs anything but a read.
/// A collection's vector fields: path → what it holds.
type Paths = std::collections::HashMap<String, VectorMeta>;

#[derive(Default)]
pub(crate) struct Cache(
    std::sync::Mutex<std::collections::HashMap<String, (std::time::Instant, Paths)>>,
);

const CACHE_TTL: std::time::Duration = std::time::Duration::from_secs(30);

impl Cache {
    pub(crate) fn clear(&self) {
        if let Ok(mut m) = self.0.lock() {
            m.clear();
        }
    }
}

impl crate::MongoSession {
    /// `coll`'s search indexes (`$listSearchIndexes`). None where the
    /// server has no search: FerretDB and DocumentDB, and a MongoDB without
    /// mongot (its refusal says so for the rest of the session, see
    /// [`no_search`]), and a collection that doesn't exist yet. Any other
    /// failure (no permission, mongot not up yet, the network) is the
    /// server's message, and the next call asks again.
    pub(crate) async fn search_indexes(
        &self,
        db: &mongodb::Database,
        coll: &str,
    ) -> std::result::Result<Vec<Document>, String> {
        use futures::TryStreamExt;
        use std::sync::atomic::Ordering;
        if self.no_search.load(Ordering::Relaxed) {
            return Ok(Vec::new());
        }
        let cmd = mongodb::bson::doc! { "aggregate": coll, "pipeline": [{ "$listSearchIndexes": {} }], "cursor": {} };
        let mut cur = match db.run_cursor_command(cmd).await {
            Ok(c) => c,
            Err(e) => {
                let code = crate::command_code(&e);
                if code == Some(26) {
                    return Ok(Vec::new());
                }
                let text = crate::err(e).to_string();
                if no_search(code, &text) {
                    self.no_search.store(true, Ordering::Relaxed);
                    return Ok(Vec::new());
                }
                return Err(text);
            }
        };
        let mut out = Vec::new();
        loop {
            match cur.try_next().await {
                Ok(Some(d)) => out.push(d),
                Ok(None) => break,
                Err(e) => return Err(crate::err(e).to_string()),
            }
        }
        Ok(out)
    }

    /// The vector fields `coll`'s indexes declare: path → dimension
    /// (`listIndexes` for DocumentDB / FerretDB, the search indexes).
    pub(crate) async fn vector_paths(
        &self,
        db: &mongodb::Database,
        coll: &str,
    ) -> std::collections::HashMap<String, VectorMeta> {
        use futures::TryStreamExt;
        let key = format!("{}.{coll}", db.name());
        if let Ok(m) = self.vectors.0.lock() {
            if let Some((at, paths)) = m.get(&key) {
                if at.elapsed() < CACHE_TTL {
                    return paths.clone();
                }
            }
        }
        let mut paths = std::collections::HashMap::new();
        let mut defs: Vec<IndexDef> = Vec::new();
        if let Ok(mut cur) = db
            .run_cursor_command(mongodb::bson::doc! { "listIndexes": coll })
            .await
        {
            while let Ok(Some(d)) = cur.try_next().await {
                defs.extend(plain_index_def(&d));
            }
        }
        defs.extend(
            self.search_indexes(db, coll)
                .await
                .unwrap_or_default()
                .iter()
                .filter_map(search_index_def),
        );
        for ix in &defs {
            for (p, dims) in vector_fields_of(ix) {
                paths.insert(p, VectorMeta::dense(dims, VectorElement::F32));
            }
        }
        if let Ok(mut m) = self.vectors.0.lock() {
            m.insert(key, (std::time::Instant::now(), paths.clone()));
        }
        paths
    }

    /// The vector columns of a result read from `coll`: fields whose
    /// values are all vectors, with a BinData vector among them, or a
    /// vector index on them.
    pub(crate) async fn result_vectors(
        &self,
        db: &mongodb::Database,
        coll: Option<&str>,
        docs: &[Document],
    ) -> std::collections::HashMap<String, VectorMeta> {
        let mut out = std::collections::HashMap::new();
        let mut arrays = Vec::new();
        let mut noted = Vec::new();
        for k in crate::convert::union_keys(docs) {
            let vals: Vec<&Bson> = docs
                .iter()
                .filter_map(|d| d.get(&k))
                .filter(|x| !matches!(x, Bson::Null | Bson::Undefined))
                .collect();
            let metas: Option<Vec<VectorMeta>> = vals.iter().map(|x| value_meta(x)).collect();
            let Some(metas) = metas.filter(|m| !m.is_empty()) else {
                continue;
            };
            let dims = metas[0]
                .dims
                .filter(|d| metas.iter().all(|m| m.dims == Some(*d)));
            let binary: Vec<VectorElement> = vals
                .iter()
                .zip(&metas)
                .filter(|(x, _)| matches!(x, Bson::Binary(_)))
                .map(|(_, m)| m.element)
                .collect();
            if binary.is_empty() {
                arrays.push((k, dims));
            } else {
                let el = if binary.iter().all(|e| *e == binary[0]) {
                    binary[0]
                } else {
                    VectorElement::Other
                };
                // Inserts follow a field whose vectors are all BinData of
                // one element (a mixed field keeps arrays).
                if el != VectorElement::Other && binary.len() == vals.len() {
                    noted.push((k.clone(), el));
                }
                out.insert(k, VectorMeta::dense(dims, el));
            }
        }
        if let Some(c) = coll {
            note_binary_fields(c, &noted);
        }
        if let (false, Some(c)) = (arrays.is_empty(), coll) {
            let paths = self.vector_paths(db, c).await;
            for (k, dims) in arrays {
                if let Some(m) = paths.get(&k) {
                    out.insert(k, VectorMeta::dense(m.dims.or(dims), VectorElement::F32));
                }
            }
        }
        out
    }
}

/// A `$listSearchIndexes` refusal that means the server has no search
/// at all: MongoDB without mongot (7.0: "only allowed on MongoDB Atlas",
/// 6047401; later, `SearchNotEnabled`, 31082), a server that doesn't know
/// the stage (40324) or the command (59, 115, 238), or says so in words.
/// Not a missing permission, mongot still starting or the network: those
/// are asked again.
pub(crate) fn no_search(code: Option<i32>, text: &str) -> bool {
    let t = text.to_ascii_lowercase();
    matches!(code, Some(6047401 | 31082 | 40324 | 59 | 115 | 238))
        || t.contains("only allowed on mongodb atlas")
        || t.contains("requires additional configuration")
        || t.contains("unrecognized pipeline stage")
        || t.contains("search index commands are only supported")
}

#[cfg(test)]
mod tests {
    use super::*;
    use dbine_driver::ColumnDef;
    use mongodb::bson::doc;

    #[test]
    fn bindata_vectors_decode_and_encode() {
        // Binary.fromFloat32Array(new Float32Array([1, 0, 0])) in mongosh.
        use base64::Engine;
        let b = base64::engine::general_purpose::STANDARD
            .decode("JwAAAIA/AAAAAAAAAAA=")
            .unwrap();
        assert_eq!(
            decode(&b),
            Some(("[1,0,0]".to_string(), VectorElement::F32, 3))
        );
        assert_eq!(encode(&[1.0, 0.0, 0.0], VectorElement::F32).bytes, b);
        let f = encode(&[0.1, -2.5, 3e-7], VectorElement::F32);
        assert_eq!(binary_text(&f).as_deref(), Some("[0.1,-2.5,0.0000003]"));
        let i = encode(&[-128.0, 5.0, 127.0], VectorElement::I8);
        assert_eq!(i.bytes, vec![0x03, 0, 0x80, 5, 127]);
        assert_eq!(binary_text(&i).as_deref(), Some("[-128,5,127]"));
        // Packed bits: 9 dimensions, 7 padding bits.
        let p = encode(&[1., 0., 1., 1., 0., 0., 0., 0., 1.], VectorElement::Bit);
        assert_eq!(p.bytes, vec![0x10, 7, 0b1011_0000, 0b1000_0000]);
        assert_eq!(
            decode(&p.bytes),
            Some(("[1,0,1,1,0,0,0,0,1]".into(), VectorElement::Bit, 9))
        );
        // Not vectors: unknown dtype, padding on floats, short.
        assert_eq!(decode(&[0x27, 1, 0, 0, 0, 0]), None);
        assert_eq!(decode(&[0x99, 0]), None);
        assert_eq!(decode(&[0x27]), None);
        assert_eq!(decode(&[0x27, 0, 1, 2]), None);
        let generic = Binary {
            subtype: BinarySubtype::Generic,
            bytes: b,
        };
        assert_eq!(binary_text(&generic), None);
    }

    #[test]
    fn values_say_what_vector_they_are() {
        let bin = Bson::Binary(encode(&[1.0, 2.0], VectorElement::I8));
        assert_eq!(
            value_meta(&bin),
            Some(VectorMeta::dense(Some(2), VectorElement::I8))
        );
        let arr = Bson::Array(vec![Bson::Double(0.5), Bson::Int32(2), Bson::Int64(-3)]);
        assert_eq!(
            value_meta(&arr),
            Some(VectorMeta::dense(Some(3), VectorElement::F32))
        );
        assert_eq!(text_of(&arr).as_deref(), Some("[0.5,2,-3]"));
        assert_eq!(value_meta(&Bson::Array(vec![])), None);
        assert_eq!(
            value_meta(&Bson::Array(vec![Bson::String("a".into())])),
            None
        );
        assert_eq!(text_of(&Bson::Array(vec![Bson::Null])), None);
    }

    #[test]
    fn field_types_spell_dims_and_element() {
        assert_eq!(type_name(Some(3), VectorElement::F32), "vector(3)");
        assert_eq!(type_name(Some(3), VectorElement::I8), "vector(3, int8)");
        assert_eq!(type_name(Some(9), VectorElement::Bit), "vector(9, bit)");
        assert_eq!(type_name(None, VectorElement::F32), "vector");
        for (t, want) in [
            ("vector(1536)", (Some(1536), VectorElement::F32)),
            ("VECTOR(3, int8)", (Some(3), VectorElement::I8)),
            ("vector(9,bit)", (Some(9), VectorElement::Bit)),
            ("vector(int1)", (None, VectorElement::Bit)),
            ("vector", (None, VectorElement::F32)),
        ] {
            assert_eq!(parse_type(t), Some(want), "{t}");
        }
        for bad in ["vectors", "vector(3, int16)", "array", "vector[3]"] {
            assert_eq!(parse_type(bad), None, "{bad}");
        }
        assert_eq!(
            type_in_list("array|vector(3)|null"),
            Some((Some(3), VectorElement::F32))
        );
    }

    fn ext(d: &Document) -> Value {
        Bson::Document(d.clone()).into_relaxed_extjson()
    }

    fn atlas_spec() -> Document {
        doc! {
            "id": "6aff", "name": "vix", "type": "vectorSearch", "status": "READY", "queryable": true,
            "latestDefinition": { "fields": [
                { "type": "vector", "path": "v", "numDimensions": 3, "similarity": "euclidean", "quantization": "scalar",
                  "hnswOptions": { "maxEdges": 32, "numEdgeCandidates": 200 } },
                { "type": "filter", "path": "c" },
            ] },
        }
    }

    #[test]
    fn atlas_search_indexes_read_and_write_back() {
        let ix = search_index_def(&atlas_spec()).unwrap();
        assert_eq!(ix.name, "vix");
        assert_eq!(ix.kind.as_deref(), Some(SEARCH));
        assert_eq!(ix.columns, ["v", "c"]);
        let vi = ix.vector.clone().unwrap();
        assert_eq!(vi.metric.as_deref(), Some(metric::L2));
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(
            vi.params,
            [
                ("ef_construction", "200"),
                ("m", "32"),
                ("quantization", "scalar")
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
        );
        assert_eq!(
            ix.options.get("numDimensions").map(String::as_str),
            Some("3")
        );
        assert!(!ix.options.contains_key("definition"));
        // Written back, the same definition.
        let def = search_definition(&ix, None).unwrap();
        assert_eq!(
            def,
            ext(atlas_spec().get_document("latestDefinition").unwrap())
        );
        let text = create_search("db.getCollection(\"c\")", &ix).unwrap();
        let st = crate::shell::parse_script(&text).unwrap();
        assert_eq!(
            st[0].cmd.get_array("indexes").unwrap()[0]
                .as_document()
                .unwrap()
                .get_str("type"),
            Ok(SEARCH)
        );
        // Two vector fields: the definition goes whole.
        let two = doc! { "name": "two", "type": "vectorSearch", "latestDefinition": { "fields": [
            { "type": "vector", "path": "a", "numDimensions": 2, "similarity": "cosine" },
            { "type": "vector", "path": "b", "numDimensions": 2, "similarity": "cosine" },
        ] } };
        let ix2 = search_index_def(&two).unwrap();
        assert!(ix2.options.contains_key("definition"));
        assert_eq!(
            search_definition(&ix2, None).unwrap(),
            ext(two.get_document("latestDefinition").unwrap())
        );
        // Full-text search indexes are not vector indexes.
        assert!(
            search_index_def(&doc! { "name": "s", "type": "search", "latestDefinition": {} })
                .is_none()
        );
        let u = search_usage(&atlas_spec()).unwrap();
        assert_eq!(u.kind, "VECTOR SEARCH READY");
        assert_eq!(u.key_columns, ["v (vector 3, euclidean)", "c (filter)"]);
    }

    #[test]
    fn documentdb_and_ferretdb_vector_indexes_round_trip() {
        let docdb = doc! { "v": 4, "name": "vd", "key": { "emb": "vector" },
        "vectorOptions": { "type": "hnsw", "dimensions": 3, "similarity": "dotProduct", "m": 16, "efConstruction": 64 } };
        let ix = plain_index_def(&docdb).unwrap();
        assert_eq!(ix.kind.as_deref(), Some(DOCDB));
        let vi = ix.vector.clone().unwrap();
        assert_eq!(vi.algorithm.as_deref(), Some("hnsw"));
        assert_eq!(vi.metric.as_deref(), Some(metric::IP));
        assert_eq!(
            vi.params.get("ef_construction").map(String::as_str),
            Some("64")
        );
        let (key, o) = plain_index_parts(&ix).unwrap();
        assert_eq!(Value::Object(key), json!({ "emb": "vector" }));
        assert_eq!(
            Value::Object(o),
            json!({ "vectorOptions": { "type": "hnsw", "similarity": "dotProduct", "dimensions": 3, "m": 16, "efConstruction": 64 } })
        );
        let cosmos = doc! { "v": 2, "name": "vc", "key": { "emb": "cosmosSearch" },
        "cosmosSearchOptions": { "kind": "vector-ivf", "similarity": "L2", "dimensions": 3, "numLists": 2, "filter": ["c"] } };
        let ix = plain_index_def(&cosmos).unwrap();
        assert_eq!(ix.columns, ["emb", "c"]);
        let vi = ix.vector.clone().unwrap();
        assert_eq!(vi.algorithm.as_deref(), Some("ivf"));
        assert_eq!(vi.metric.as_deref(), Some(metric::L2));
        assert_eq!(vi.params.get("lists").map(String::as_str), Some("2"));
        let (key, o) = plain_index_parts(&ix).unwrap();
        assert_eq!(Value::Object(key), json!({ "emb": "cosmosSearch" }));
        assert_eq!(
            Value::Object(o),
            json!({ "cosmosSearchOptions": { "kind": "vector-ivf", "similarity": "L2", "dimensions": 3, "numLists": 2, "filter": ["c"] } })
        );
        assert!(plain_index_def(&doc! { "name": "a_1", "key": { "a": 1 } }).is_none());
        // DocumentDB takes one field and no compression.
        let mut bad = plain_index_def(&docdb).unwrap();
        bad.columns.push("c".into());
        assert!(plain_index_parts(&bad).is_err());
    }

    #[test]
    fn dims_come_from_the_field_type() {
        let mut t = TableSchema {
            name: "c".into(),
            columns: vec![ColumnDef {
                name: "v".into(),
                data_type: "array".into(),
                ..Default::default()
            }],
            indexes: vec![
                search_index_def(&atlas_spec()).unwrap(),
                plain_index_def(&doc! { "name": "n", "key": { "a.b": "cosmosSearch" },
                    "cosmosSearchOptions": { "kind": "vector-hnsw", "similarity": "COS", "dimensions": 4 } })
                .unwrap(),
            ],
            ..Default::default()
        };
        describe_fields(&mut t);
        assert_eq!(t.columns[0].data_type, "vector(3)");
        assert!(!t.indexes[0].options.contains_key("numDimensions"));
        // A nested path keeps its dimension in the index.
        assert_eq!(
            t.indexes[1].options.get("dimensions").map(String::as_str),
            Some("4")
        );
        assert_eq!(
            with_dims(&t, &t.indexes[0])
                .options
                .get("numDimensions")
                .map(String::as_str),
            Some("3")
        );
        assert!(search_definition(&t.indexes[0], None).is_err());
        // A vector index on a field no document has yet adds the field.
        let mut e = TableSchema {
            name: "e".into(),
            indexes: vec![search_index_def(&atlas_spec()).unwrap()],
            ..Default::default()
        };
        describe_fields(&mut e);
        assert_eq!(e.columns[0].name, "v");
        assert_eq!(e.columns[0].data_type, "vector(3)");
    }

    #[test]
    fn kinds_follow_the_server() {
        let mut t = TableSchema {
            name: "c".into(),
            indexes: vec![IndexDef {
                name: "vx".into(),
                columns: vec!["v".into(), "c".into()],
                vector: Some(VectorIndex {
                    algorithm: Some("ivfflat".into()),
                    metric: Some(metric::IP.into()),
                    params: [("lists", "10"), ("quantization", "scalar")]
                        .into_iter()
                        .map(|(k, v)| (k.to_string(), v.to_string()))
                        .collect(),
                }),
                ..Default::default()
            }],
            ..Default::default()
        };
        // No kind (an index from another engine): the server's, and what
        // it can't take is said.
        let (m, w) = for_flavor(&t, Flavor::Mongo);
        assert_eq!(m.indexes[0].kind.as_deref(), Some(SEARCH));
        assert_eq!(
            w,
            ["El índice vectorial vx se crea como vectorSearch: pierde lists=10, el algoritmo ivfflat."]
        );
        // From the designer (Atlas's own choices): nothing to say.
        let mut atlas = t.clone();
        atlas.indexes[0].vector = Some(VectorIndex {
            algorithm: Some("hnsw".into()),
            metric: Some(metric::COSINE.into()),
            params: [
                ("quantization", "none"),
                ("m", "16"),
                ("ef_construction", ""),
            ]
            .into_iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect(),
        });
        let (a, w) = for_flavor(&atlas, Flavor::Mongo);
        assert!(w.is_empty(), "{w:?}");
        assert_eq!(a.indexes[0].kind.as_deref(), Some(SEARCH));
        // A vCore algorithm on a MongoDB connection: cosmosSearch, by its
        // own name, with its parameters.
        let mut vcore = t.clone();
        vcore.indexes[0].vector = Some(VectorIndex {
            algorithm: Some("vector-ivf".into()),
            metric: Some(metric::L2.into()),
            params: [("lists", "4"), ("quantization", "none")]
                .into_iter()
                .map(|(k, v)| (k.to_string(), v.to_string()))
                .collect(),
        });
        let (v, w) = for_flavor(&vcore, Flavor::Mongo);
        assert_eq!(v.indexes[0].kind.as_deref(), Some(COSMOS));
        assert_eq!(
            v.indexes[0].vector.as_ref().unwrap().algorithm.as_deref(),
            Some("ivf")
        );
        assert_eq!(w.len(), 1, "{w:?}");
        assert!(w[0].contains("quantization=none"), "{w:?}");
        let mut vi = v.indexes[0].clone();
        vi.options.insert("dimensions".into(), "3".into());
        let (_, o) = plain_index_parts(&vi).unwrap();
        assert_eq!(
            Value::Object(o),
            json!({ "cosmosSearchOptions": { "kind": "vector-ivf", "similarity": "L2", "dimensions": 3, "numLists": 4, "filter": ["c"] } })
        );
        // Couchbase's squared euclidean: l2, said.
        let mut sq = t.clone();
        sq.indexes[0].vector = Some(VectorIndex {
            metric: Some("l2_squared".into()),
            ..Default::default()
        });
        let (q, w) = for_flavor(&sq, Flavor::Mongo);
        assert_eq!(
            q.indexes[0].vector.as_ref().unwrap().metric.as_deref(),
            Some(metric::L2)
        );
        assert!(w[0].contains("l2_squared queda l2"), "{w:?}");
        // Another server's kind: translated, with what's lost.
        t.indexes[0].kind = Some(SEARCH.into());
        let (d, w) = for_flavor(&t, Flavor::DocumentDb);
        assert_eq!(d.indexes[0].kind.as_deref(), Some(DOCDB));
        assert_eq!(d.indexes[0].columns, ["v"]);
        let vi = d.indexes[0].vector.as_ref().unwrap();
        assert_eq!(vi.algorithm.as_deref(), Some("ivfflat"));
        assert_eq!(vi.params.get("lists").map(String::as_str), Some("10"));
        assert!(
            w[0].contains("quantization=scalar") && w[0].contains("los filtros (c)"),
            "{w:?}"
        );
        let (f, _) = for_flavor(&t, Flavor::Ferret);
        assert_eq!(f.indexes[0].kind.as_deref(), Some(COSMOS));
        assert_eq!(
            f.indexes[0].vector.as_ref().unwrap().algorithm.as_deref(),
            Some("ivf")
        );
        // Cosmos DB vCore's kind stays on a MongoDB connection.
        t.indexes[0].kind = Some(COSMOS.into());
        assert_eq!(
            for_flavor(&t, Flavor::Mongo).0.indexes[0].kind.as_deref(),
            Some(COSMOS)
        );
    }

    #[test]
    fn edits_keep_the_stored_form() {
        use base64::Engine;
        let b64 = |b: &[u8]| base64::engine::general_purpose::STANDARD.encode(b);
        // With the value it had: equal to it in one element, nothing else.
        let e = keep_form_expr("v", &[1.0, 0.0, 0.0], Some(&[2.0, 1.0, 0.0]));
        assert!(
            e.starts_with("{ $cond: [{ $eq: [{ $type: \"$v\" }, \"binData\"] }"),
            "{e}"
        );
        assert!(e.ends_with(", [1.0,0.0,0.0]] }"), "{e}");
        let stored_f32 = b64(&encode(&[2.0, 1.0, 0.0], VectorElement::F32).bytes);
        let stored_i8 = b64(&encode(&[2.0, 1.0, 0.0], VectorElement::I8).bytes);
        assert!(
            e.contains(&format!("$eq: [\"$v\", BinData(9, \"{stored_f32}\")]")),
            "{e}"
        );
        assert!(
            e.contains(&format!("$eq: [\"$v\", BinData(9, \"{stored_i8}\")]")),
            "{e}"
        );
        // [2, 1, 0] isn't bits: no bit branch.
        assert_eq!(e.matches("case:").count(), 2, "{e}");
        assert!(!e.contains("$binarySize"), "{e}");
        // It parses as an update pipeline value.
        let st = crate::shell::parse_script(&format!(
            "db.c.updateOne({{ _id: 1 }}, [{{ $set: {{ \"v\": {e} }} }}])"
        ))
        .unwrap();
        assert!(st[0].cmd.get_array("updates").is_ok());
        // The reviewer's collision: a stored 2-dimension float32 vector is
        // as long as 8 int8 values. The edit to 8 small integers matches
        // only the float32 BinData of the old value, and stays float32.
        let new: Vec<f64> = (1..=8).map(f64::from).collect();
        let e = keep_form_expr("v", &new, Some(&[0.5, 1.5]));
        assert_eq!(e.matches("case:").count(), 1, "{e}");
        let as_f32 = b64(&encode(&new, VectorElement::F32).bytes);
        assert!(
            e.contains(&format!(
                "BinData(9, \"{}\")] }}, then: BinData(9, \"{as_f32}\")",
                b64(&encode(&[0.5, 1.5], VectorElement::F32).bytes)
            )),
            "{e}"
        );
        // An int8 vector edited to numbers int8 can't hold: float32, never
        // rounded.
        let e = keep_form_expr("v", &[0.25, 3.0], Some(&[1.0, 2.0]));
        let i8_old = b64(&encode(&[1.0, 2.0], VectorElement::I8).bytes);
        let f32_new = b64(&encode(&[0.25, 3.0], VectorElement::F32).bytes);
        assert!(
            e.contains(&format!(
                "BinData(9, \"{i8_old}\")] }}, then: BinData(9, \"{f32_new}\")"
            )),
            "{e}"
        );
        // Without the old value: by each element's byte range at the new
        // length (length, subtype and leading byte decide).
        let f = keep_form_expr("a.b", &[0.5, 2.0], None);
        assert!(
            f.contains("$getField") && f.contains("$gte") && f.contains("$lte"),
            "{f}"
        );
        assert_eq!(f.matches("case:").count(), 3, "{f}");
        let lo = b64(&[DTYPE_I8, 0, 0, 0]);
        let hi = b64(&[DTYPE_I8, 0, 0xFF, 0xFF]);
        assert!(f.contains(&lo) && f.contains(&hi), "{f}");
        assert_eq!(
            fitting(VectorElement::I8, &[1.0, -128.0]),
            VectorElement::I8
        );
        assert_eq!(fitting(VectorElement::I8, &[1.5]), VectorElement::F32);
        assert_eq!(fitting(VectorElement::Bit, &[1.0, 2.0]), VectorElement::F32);
        assert_eq!(fitting(VectorElement::Bit, &[1.0, 0.0]), VectorElement::Bit);
    }

    #[test]
    fn inserts_take_the_form_results_showed() {
        note_binary_fields("vec_forms_test", &[("emb".into(), VectorElement::I8)]);
        let f = binary_fields("vec_forms_test");
        assert_eq!(f.get("emb"), Some(&VectorElement::I8));
        assert!(binary_fields("vec_forms_other").is_empty());
        let target = dbine_driver::ObjectRef {
            kind: "collection".into(),
            schema: None,
            name: "vec_forms_test".into(),
        };
        let text = crate::ddl::insert_script(
            &target,
            &["_id".into(), "emb".into(), "arr".into()],
            &[vec![json!(1), json!([1, -2, 3]), json!([1, 2])]],
            &f,
        )
        .unwrap();
        assert!(
            text.contains(&binary_literal(&[1.0, -2.0, 3.0], VectorElement::I8))
                && text.contains("\"arr\": [1,2]"),
            "{text}"
        );
        let st = crate::shell::parse_script(&text).unwrap();
        let doc = st[0].cmd.get_array("documents").unwrap()[0]
            .as_document()
            .unwrap();
        assert!(
            matches!(doc.get("emb"), Some(Bson::Binary(b)) if b.subtype == BinarySubtype::Vector)
        );
        // Numbers int8 can't hold: float32.
        assert_eq!(
            binary_literal(&[0.5], VectorElement::I8),
            binary_literal(&[0.5], VectorElement::F32)
        );
    }

    #[test]
    fn designer_choices_per_server() {
        let m = designer_spec(Flavor::Mongo);
        assert!(m.filter_columns);
        // Atlas (hnsw) first, then vCore by its own names.
        assert_eq!(
            m.algorithms,
            ["hnsw", "vector-hnsw", "vector-ivf", "vector-diskann"]
        );
        let of = |a: &str| -> Vec<&str> {
            m.params
                .iter()
                .filter(|p| p.algorithms.is_empty() || p.algorithms.contains(&a))
                .map(|p| p.key)
                .collect()
        };
        assert_eq!(of("hnsw"), ["quantization", "m", "ef_construction"]);
        assert_eq!(of("vector-ivf"), ["lists", "quantization"]);
        assert_eq!(
            of("vector-diskann"),
            ["maxDegree", "lBuild", "quantization"]
        );
        // A vCore index read back (`ivf`, `diskann`) keeps its parameters.
        assert_eq!(of("ivf"), ["lists", "quantization"]);
        assert_eq!(
            designer_spec(Flavor::DocumentDb).algorithms,
            ["hnsw", "ivfflat"]
        );
        assert!(!designer_spec(Flavor::DocumentDb).filter_columns);
        assert_eq!(designer_spec(Flavor::Ferret).algorithms, ["hnsw", "ivf"]);
    }

    #[test]
    fn definitions_dbine_can_say_are_not_kept_whole() {
        // An explicit "none", a filter before the vector field, numbers as
        // doubles: the same definition as rebuilt.
        let spec = doc! { "name": "a", "type": "vectorSearch", "latestDefinition": { "fields": [
            { "type": "filter", "path": "c" },
            { "type": "vector", "path": "v", "numDimensions": 3.0, "similarity": "cosine", "quantization": "none" },
        ] } };
        let ix = search_index_def(&spec).unwrap();
        assert_eq!(ix.columns, ["v", "c"]);
        assert!(!ix.options.contains_key("definition"), "{:?}", ix.options);
        // A key DBine doesn't model (an object): kept, and the designer's
        // edits still go over it.
        let spec = doc! { "name": "b", "type": "vectorSearch", "latestDefinition": { "fields": [
            { "type": "filter", "path": "c" },
            { "type": "vector", "path": "v", "numDimensions": 3, "similarity": "cosine",
              "hnswOptions": { "maxEdges": 16, "future": { "x": 1 } }, "extra": { "y": true } },
        ] } };
        let mut ix = search_index_def(&spec).unwrap();
        assert!(ix.options.contains_key("definition"));
        let before = search_definition(&ix, None).unwrap();
        assert_eq!(
            normalized(&before),
            normalized(&ext(spec.get_document("latestDefinition").unwrap()))
        );
        // Metric, a parameter and a filter changed in the designer.
        let vi = ix.vector.as_mut().unwrap();
        vi.metric = Some(metric::IP.into());
        vi.params.insert("m".into(), "32".into());
        vi.params.insert("quantization".into(), "scalar".into());
        ix.columns = vec!["v".into(), "d".into()];
        let after = search_definition(&ix, None).unwrap();
        assert_eq!(
            after,
            json!({ "fields": [
                { "type": "vector", "path": "v", "numDimensions": 3, "similarity": "dotProduct",
                  "hnswOptions": { "future": { "x": 1 }, "maxEdges": 32 }, "extra": { "y": true }, "quantization": "scalar" },
                { "type": "filter", "path": "d" },
            ] })
        );
        let mut orig = search_index_def(&spec).unwrap();
        assert!(!same_search(&orig, &ix));
        // Compare: the same definition in another order is the same index.
        let reordered = doc! { "name": "b", "type": "vectorSearch", "latestDefinition": { "fields": [
            { "type": "vector", "path": "v", "numDimensions": 3, "similarity": "cosine",
              "hnswOptions": { "future": { "x": 1 }, "maxEdges": 16 }, "extra": { "y": true } },
            { "type": "filter", "path": "c" },
        ] } };
        assert!(same_search(&orig, &search_index_def(&reordered).unwrap()));
        // Its field typed: describe takes the dimension out of the index.
        let mut t = TableSchema {
            name: "c".into(),
            indexes: vec![orig.clone()],
            ..Default::default()
        };
        describe_fields(&mut t);
        assert_eq!(t.columns[0].data_type, "vector(3)");
        orig = with_dims(&t, &t.indexes[0]);
        assert!(same_search(&orig, &search_index_def(&reordered).unwrap()));
    }

    #[test]
    fn search_indexes_without_a_vector_field_or_with_knn_vectors() {
        // Auto-embedding: listed and kept whole, its text field untyped.
        let auto = doc! { "name": "ae", "type": "vectorSearch", "status": "READY", "latestDefinition": { "fields": [
            { "type": "autoEmbed", "path": "text", "model": "voyage-3.5" },
            { "type": "filter", "path": "lang" },
        ] } };
        let ix = search_index_def(&auto).unwrap();
        assert_eq!(ix.columns, ["text", "lang"]);
        assert!(ix.options.contains_key("definition"));
        assert_eq!(
            normalized(&search_definition(&ix, None).unwrap()),
            normalized(&ext(auto.get_document("latestDefinition").unwrap()))
        );
        let mut t = TableSchema {
            name: "c".into(),
            indexes: vec![ix],
            ..Default::default()
        };
        describe_fields(&mut t);
        assert!(t.columns.is_empty(), "{:?}", t.columns);
        assert_eq!(
            search_usage(&auto).unwrap().key_columns,
            ["text (autoEmbed)", "lang (filter)"]
        );
        // Atlas Search with knnVector fields (nested too).
        let knn = doc! { "name": "k", "type": "search", "status": "READY", "latestDefinition": { "mappings": { "dynamic": false, "fields": {
            "title": { "type": "string" },
            "emb": [{ "type": "knnVector", "dimensions": 4, "similarity": "euclidean" }, { "type": "token" }],
            "meta": { "type": "document", "fields": { "e2": { "type": "knnVector", "dimensions": 2, "similarity": "cosine" } } },
        } } } };
        let ix = search_index_def(&knn).unwrap();
        assert_eq!(ix.kind.as_deref(), Some(FTS));
        assert_eq!(ix.columns, ["emb"]);
        assert_eq!(
            ix.vector.as_ref().unwrap().metric.as_deref(),
            Some(metric::L2)
        );
        assert_eq!(ix.options.get("dimensions").map(String::as_str), Some("4"));
        let u = search_usage(&knn).unwrap();
        assert_eq!(u.kind, "SEARCH READY");
        assert_eq!(
            u.key_columns,
            [
                "emb (knnVector 4, euclidean)",
                "meta.e2 (knnVector 2, cosine)"
            ]
        );
        assert!(search_usage(&doc! { "name": "t", "type": "search", "latestDefinition": { "mappings": { "dynamic": true } } }).is_none());
        let mut t = TableSchema {
            name: "c".into(),
            indexes: vec![ix.clone()],
            ..Default::default()
        };
        describe_fields(&mut t);
        assert_eq!(t.columns[0].data_type, "vector(4)");
        let ix = with_dims(&t, &t.indexes[0]);
        // Written back: the same definition; created as `search`.
        assert_eq!(
            normalized(&search_definition(&ix, None).unwrap()),
            normalized(&ext(knn.get_document("latestDefinition").unwrap()))
        );
        let c = create_search("db.getCollection(\"c\")", &ix).unwrap();
        assert!(c.contains("\"type\":\"search\""), "{c}");
        // The field's dimension and the metric go into its mapping; a
        // rename moves the mapping.
        let mut e = ix.clone();
        e.options.insert("dimensions".into(), "8".into());
        e.vector.as_mut().unwrap().metric = Some(metric::IP.into());
        e.columns = vec!["vec".into()];
        let d = search_definition(&e, None).unwrap();
        assert_eq!(d["mappings"]["fields"]["vec"][0]["dimensions"], 8);
        assert_eq!(
            d["mappings"]["fields"]["vec"][0]["similarity"],
            "dotProduct"
        );
        assert!(d["mappings"]["fields"].get("emb").is_none());
        // A type change (search → vectorSearch) isn't the same index.
        let vs = search_index_def(&atlas_spec()).unwrap();
        assert!(!same_search(&ix, &vs));
    }

    #[test]
    fn only_a_server_without_search_says_so_for_good() {
        assert!(no_search(Some(31082), "Using Atlas Search Database Commands and the $listSearchIndexes aggregation stage requires additional configuration."));
        assert!(no_search(
            Some(40324),
            "Unrecognized pipeline stage name: '$listSearchIndexes'"
        ));
        assert!(no_search(None, "Unrecognized pipeline stage name"));
        // MongoDB 7 without mongot (seen live).
        assert!(no_search(
            Some(6047401),
            "$listSearchIndexes stage is only allowed on MongoDB Atlas (Location6047401)"
        ));
        // Asked again later: no permission, mongot still starting, network.
        assert!(!no_search(
            Some(13),
            "not authorized on db to execute command"
        ));
        assert!(!no_search(
            Some(125),
            "Error connecting to Search Index Management service."
        ));
        assert!(!no_search(None, "connection reset by peer"));
    }
}
