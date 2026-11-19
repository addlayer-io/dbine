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
//!
//! # Indexes ([`IndexDef::kind`])
//!
//! - `vectorSearch`: Atlas Vector Search, and MongoDB Community /
//!   Enterprise 8.2+ with `mongot`. A search index, listed by
//!   `$listSearchIndexes` and made with `createSearchIndex`,
//!   `updateSearchIndex` and `dropSearchIndex`; its definition is
//!   `{ fields: [{ type: "vector", path, numDimensions, similarity,
//!   quantization, hnswOptions }, { type: "filter", path }…] }`. `columns`
//!   are the vector path and then the filter paths. Metrics: `cosine`,
//!   `euclidean` (`l2`), `dotProduct` (`ip`); parameters `quantization`
//!   (`scalar`, `binary`), `m` (`hnswOptions.maxEdges`) and
//!   `ef_construction` (`hnswOptions.numEdgeCandidates`). A definition the
//!   columns and the [`VectorIndex`] can't say (several vector fields, a
//!   filter before the vector) is kept whole in the option `definition`.
//! - `vector`: Amazon DocumentDB, `createIndex({ f: "vector" }, {
//!   vectorOptions: { type: hnsw | ivfflat, dimensions, similarity:
//!   euclidean | cosine | dotProduct, m, efConstruction, lists } })`. One
//!   field, no filters.
//! - `cosmosSearch`: FerretDB 2 (the DocumentDB extension) and Azure
//!   Cosmos DB for MongoDB vCore, `createIndex({ f: "cosmosSearch" }, {
//!   cosmosSearchOptions: { kind: vector-hnsw | vector-ivf |
//!   vector-diskann, similarity: COS | L2 | IP, dimensions, m,
//!   efConstruction, numLists, compression, filter: [paths] } })`.
//!
//! The dimension is the vector field's type (`vector(N)`) when the field
//! is a column of the table, otherwise the native option (`numDimensions`,
//! `dimensions`). A vector index without a kind gets the server's
//! ([`for_flavor`]), and one of another server's kind is translated to it.

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

fn ext(d: &Document) -> Value {
    Bson::Document(d.clone()).into_relaxed_extjson()
}

/// A `$listSearchIndexes` entry of type `vectorSearch` as an index.
pub(crate) fn search_index_def(spec: &Document) -> Option<IndexDef> {
    if spec.get_str("type") != Ok(SEARCH) {
        return None;
    }
    let name = spec.get_str("name").ok()?.to_string();
    let def = spec
        .get_document("latestDefinition")
        .or_else(|_| spec.get_document("definition"))
        .ok()?;
    let fields: Vec<&Document> = def
        .get_array("fields")
        .ok()?
        .iter()
        .filter_map(Bson::as_document)
        .collect();
    let first = fields.iter().find(|f| f.get_str("type") == Ok("vector"))?;
    let mut columns = vec![first.get_str("path").ok()?.to_string()];
    columns.extend(
        fields
            .iter()
            .filter(|f| f.get_str("type") == Ok("filter"))
            .filter_map(|f| f.get_str("path").ok().map(str::to_string)),
    );
    let mut vi = VectorIndex {
        metric: first.get_str("similarity").ok().map(|m| {
            atlas_metric(m)
                .map(str::to_string)
                .unwrap_or_else(|| m.to_string())
        }),
        ..Default::default()
    };
    let mut options = BTreeMap::new();
    for (k, x) in first.iter() {
        match k.as_str() {
            "type" | "path" | "similarity" => {}
            "numDimensions" => {
                if let Some(n) = num_text(x) {
                    options.insert(k.clone(), n);
                }
            }
            "quantization" => {
                if let Some(q) = x.as_str().filter(|q| *q != "none") {
                    vi.params.insert(param::QUANTIZATION.into(), q.into());
                }
            }
            "hnswOptions" => {
                if let Some(h) = x.as_document() {
                    for (hk, hv) in h {
                        let key = match hk.as_str() {
                            "maxEdges" => param::M.to_string(),
                            "numEdgeCandidates" => param::EF_CONSTRUCTION.to_string(),
                            other => other.to_string(),
                        };
                        if let Some(n) = num_text(hv) {
                            vi.params.insert(key, n);
                        }
                    }
                }
            }
            other => {
                if let Some(t) = num_text(x) {
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
    // What the columns and the vector don't say goes whole.
    if search_definition(&ix, None).ok().as_ref() != Some(&ext(def)) {
        ix.options.insert("definition".into(), ext(def).to_string());
    }
    Some(ix)
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
            (DOCDB, "similarity") => {
                vi.metric = x.as_str().map(|m| {
                    atlas_metric(m)
                        .map(str::to_string)
                        .unwrap_or_else(|| m.to_string())
                })
            }
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

/// A vector search index for the explorer: kind `VECTOR SEARCH` with its
/// status (`READY`, `BUILDING`, `PENDING`, `FAILED`…), the vector paths
/// and the filter paths.
pub(crate) fn search_usage(spec: &Document) -> Option<IndexUsage> {
    if spec.get_str("type") != Ok(SEARCH) {
        return None;
    }
    let name = spec.get_str("name").ok()?.to_string();
    let def = spec
        .get_document("latestDefinition")
        .or_else(|_| spec.get_document("definition"))
        .ok();
    let mut key_columns = Vec::new();
    for f in def
        .and_then(|d| d.get_array("fields").ok())
        .map(|a| a.as_slice())
        .unwrap_or(&[])
        .iter()
        .filter_map(Bson::as_document)
    {
        let (Ok(t), Ok(p)) = (f.get_str("type"), f.get_str("path")) else {
            continue;
        };
        key_columns.push(match (t, f.get("numDimensions").and_then(num_text)) {
            ("vector", Some(n)) => format!(
                "{p} (vector {n}, {})",
                f.get_str("similarity").unwrap_or("?")
            ),
            _ => format!("{p} ({t})"),
        });
    }
    let status = spec.get_str("status").unwrap_or("");
    Some(IndexUsage {
        name,
        kind: format!("VECTOR SEARCH {status}").trim().to_string(),
        key_columns,
        ..Default::default()
    })
}

/// Index kinds that are vector indexes.
pub(crate) fn is_vector_kind(kind: Option<&str>) -> bool {
    kind.is_some_and(|k| {
        [SEARCH, DOCDB, COSMOS]
            .iter()
            .any(|x| k.trim().eq_ignore_ascii_case(x))
    })
}

/// The index is a search index (`vectorSearch`): it has its own commands.
pub(crate) fn is_search(ix: &IndexDef) -> bool {
    ix.kind
        .as_deref()
        .is_some_and(|k| k.eq_ignore_ascii_case(SEARCH))
}

fn dims_key(ix: &IndexDef) -> &'static str {
    if is_search(ix) {
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

/// The read-back schema: each vector index's field gets its `vector(N)`
/// type (a column is added when no sampled document has it), and the
/// dimension leaves the index's options when the column carries it.
pub(crate) fn describe_fields(t: &mut TableSchema) {
    for ix in t.indexes.iter_mut().filter(|i| i.vector.is_some()) {
        let key = dims_key(ix);
        let Some(path) = ix.columns.first().cloned() else {
            continue;
        };
        if path.contains('.') || ix.options.contains_key("definition") {
            continue;
        }
        let dims = ix.options.get(key).and_then(|n| n.parse::<u32>().ok());
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
        if dims.is_some() {
            ix.options.remove(key);
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

/// A `vectorSearch` index's `definition`.
pub(crate) fn search_definition(ix: &IndexDef, dims: Option<u32>) -> Result<Value> {
    if let Some(d) = ix.options.get("definition") {
        return crate::ddl::relaxed(d, |e| {
            format!("la definición del índice {} no es válida: {e}", ix.name)
        });
    }
    let path = ix
        .columns
        .first()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
        .ok_or_else(|| Error::Query(format!("el índice {} no tiene campos", ix.name)))?;
    let vi = ix.vector.clone().unwrap_or_default();
    let mut f = Map::new();
    f.insert("type".into(), "vector".into());
    f.insert("path".into(), path.into());
    f.insert("numDimensions".into(), dims_of(ix, dims)?.into());
    f.insert(
        "similarity".into(),
        metric_atlas(vi.metric.as_deref().unwrap_or(metric::COSINE))?.into(),
    );
    let mut hnsw = Map::new();
    for (k, x) in &vi.params {
        match k.as_str() {
            param::QUANTIZATION => {
                if x.trim() != "none" && !x.trim().is_empty() {
                    f.insert("quantization".into(), x.trim().into());
                }
            }
            param::M | "maxEdges" => {
                hnsw.insert("maxEdges".into(), number(x));
            }
            param::EF_CONSTRUCTION | "numEdgeCandidates" => {
                hnsw.insert("numEdgeCandidates".into(), number(x));
            }
            other if !x.trim().is_empty() => {
                f.insert(other.to_string(), number(x));
            }
            _ => {}
        }
    }
    if !hnsw.is_empty() {
        f.insert("hnswOptions".into(), Value::Object(hnsw));
    }
    let mut fields = vec![Value::Object(f)];
    for c in ix.columns[1..]
        .iter()
        .map(|c| c.trim())
        .filter(|c| !c.is_empty())
    {
        fields.push(json!({ "type": "filter", "path": c }));
    }
    Ok(json!({ "fields": fields }))
}

/// `createSearchIndex` for a `vectorSearch` index.
pub(crate) fn create_search(coll: &str, ix: &IndexDef) -> Result<String> {
    let def = search_definition(ix, None)?;
    Ok(format!(
        "{coll}.createSearchIndex({})",
        json!({ "name": ix.name.trim(), "type": SEARCH, "definition": def })
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
        if x.trim().is_empty() {
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

/// `t`'s vector indexes in `flavor`'s terms: a missing kind gets the
/// server's, another server's kind is translated (with a warning for what
/// can't go). MongoDB keeps `cosmosSearch` (Cosmos DB vCore speaks the
/// MongoDB protocol).
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
        let to = match (flavor, from.as_str()) {
            (Flavor::Mongo, COSMOS) => COSMOS,
            _ => kind_of(flavor),
        };
        if from.eq_ignore_ascii_case(to) {
            ix.kind = Some(to.into());
            continue;
        }
        let mut vi = ix.vector.clone().unwrap_or_default();
        let mut lost: Vec<String> = Vec::new();
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
        if let Some(d) = ix.options.remove("definition") {
            lost.push(format!("la definición ({d})"));
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
            let ok = keep.contains(&k.as_str()) && (k != param::QUANTIZATION || quant_ok(x.trim()));
            if !ok {
                lost.push(format!("{k}={x}"));
            }
            ok
        });
        vi.algorithm = match (to, vi.algorithm.as_deref()) {
            (SEARCH, Some(a)) if a != "hnsw" => {
                lost.push(format!("el algoritmo {a}"));
                None
            }
            (SEARCH, _) => None,
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
        if let Some(m) = vi.metric.as_deref().filter(|m| !known_metric(m)) {
            lost.push(format!("la distancia {m} (queda cosine)"));
            vi.metric = Some(metric::COSINE.into());
        }
        if to == DOCDB && ix.columns.len() > 1 {
            lost.push(format!("los filtros ({})", ix.columns[1..].join(", ")));
            ix.columns.truncate(1);
        }
        if !from.is_empty() && !lost.is_empty() {
            warnings.push(format!(
                "El índice vectorial {} pasa de {from} a {to} y pierde {}.",
                ix.name,
                lost.join(", ")
            ));
        }
        ix.kind = Some(to.into());
        ix.vector = Some(vi);
    }
    (out, warnings)
}

/// The designer's vector index choices for `flavor`.
pub(crate) fn designer_spec(flavor: Flavor) -> VectorIndexSpec {
    let p = |key, algorithms: &[&'static str], default| VectorParamSpec {
        key,
        algorithms: algorithms.to_vec(),
        default,
    };
    match flavor {
        Flavor::Mongo => VectorIndexSpec {
            algorithms: Vec::new(),
            metrics: vec![metric::COSINE, metric::L2, metric::IP],
            params: vec![
                p(param::QUANTIZATION, &[], "none"),
                p(param::M, &[], "16"),
                p(param::EF_CONSTRUCTION, &[], "100"),
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

/// `$set` value of an edit that writes vector `values` over `field`,
/// keeping the stored form: when the document holds a BinData vector, a
/// BinData of the same element (told by its size: float32, int8 or packed
/// bits), otherwise the array. An aggregation expression for a pipeline
/// update.
pub(crate) fn keep_form_expr(field: &str, values: &[f64]) -> String {
    use base64::Engine;
    let b64 = |el| {
        let bin = encode(values, el);
        format!(
            "BinData(9, \"{}\")",
            base64::engine::general_purpose::STANDARD.encode(&bin.bytes)
        )
    };
    let path = if field.contains('.') || field.starts_with('$') {
        format!(
            "{{ $getField: {{ $literal: {} }} }}",
            Value::String(field.into())
        )
    } else {
        Value::String(format!("${field}")).to_string()
    };
    let n = values.len();
    // `$binarySize` of anything but a BinData (or a string) fails, even in
    // a branch not taken on some servers (FerretDB): it sees `""` then.
    let size = format!(
        "{{ $binarySize: {{ $cond: [{{ $eq: [{{ $type: {path} }}, \"binData\"] }}, {path}, \"\"] }} }}"
    );
    let mut branches = Vec::new();
    let ints = values.iter().all(|x| x.fract() == 0.0);
    if ints && values.iter().all(|x| (-128.0..=127.0).contains(x)) && n > 1 {
        branches.push(format!(
            "{{ case: {{ $eq: [{size}, {}] }}, then: {} }}",
            2 + n,
            b64(VectorElement::I8)
        ));
    }
    if ints && values.iter().all(|x| *x == 0.0 || *x == 1.0) && n > 1 {
        branches.push(format!(
            "{{ case: {{ $eq: [{size}, {}] }}, then: {} }}",
            2 + n.div_ceil(8),
            b64(VectorElement::Bit)
        ));
    }
    let binary = if branches.is_empty() {
        b64(VectorElement::F32)
    } else {
        format!(
            "{{ $switch: {{ branches: [{}], default: {} }} }}",
            branches.join(", "),
            b64(VectorElement::F32)
        )
    };
    // The array as doubles, as an array vector is stored.
    let array = Value::Array(values.iter().map(|x| dbine_driver::json_f64(*x)).collect());
    format!("{{ $cond: [{{ $eq: [{{ $type: {path} }}, \"binData\"] }}, {binary}, {array}] }}")
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
    /// `coll`'s search indexes (`$listSearchIndexes`); none where the
    /// server has no search (the first failure says so for the session).
    pub(crate) async fn search_indexes(&self, db: &mongodb::Database, coll: &str) -> Vec<Document> {
        use futures::TryStreamExt;
        use std::sync::atomic::Ordering;
        if self.no_search.load(Ordering::Relaxed) {
            return Vec::new();
        }
        let cmd = mongodb::bson::doc! { "aggregate": coll, "pipeline": [{ "$listSearchIndexes": {} }], "cursor": {} };
        let mut cur = match db.run_cursor_command(cmd).await {
            Ok(c) => c,
            Err(e) => {
                // A collection that doesn't exist yet answers the same on
                // Atlas; anything else means no search on this server.
                if crate::command_code(&e) != Some(26) {
                    self.no_search.store(true, Ordering::Relaxed);
                }
                return Vec::new();
            }
        };
        let mut out = Vec::new();
        while let Ok(Some(d)) = cur.try_next().await {
            out.push(d);
        }
        out
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
                .iter()
                .filter_map(search_index_def),
        );
        for ix in &defs {
            let dims = ix
                .options
                .get(dims_key(ix))
                .and_then(|n| n.parse::<u32>().ok());
            if let Some(p) = ix.columns.first() {
                paths.insert(p.clone(), VectorMeta::dense(dims, VectorElement::F32));
            }
            // A definition with several vector fields: each one.
            if let Some(d) = ix
                .options
                .get("definition")
                .and_then(|d| serde_json::from_str::<Value>(d).ok())
            {
                for f in d["fields"].as_array().map(|a| a.as_slice()).unwrap_or(&[]) {
                    if let (Some("vector"), Some(p)) = (f["type"].as_str(), f["path"].as_str()) {
                        let dims = f["numDimensions"].as_u64().map(|n| n as u32);
                        paths.insert(p.to_string(), VectorMeta::dense(dims, VectorElement::F32));
                    }
                }
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
                out.insert(k, VectorMeta::dense(dims, el));
            }
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
        assert_eq!(vi.algorithm, None);
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
        // No kind: the server's, nothing lost.
        let (m, w) = for_flavor(&t, Flavor::Mongo);
        assert_eq!(m.indexes[0].kind.as_deref(), Some(SEARCH));
        assert!(w.is_empty());
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
        let e = keep_form_expr("v", &[1.0, 0.0, 0.0]);
        assert!(
            e.starts_with("{ $cond: [{ $eq: [{ $type: \"$v\" }, \"binData\"] }"),
            "{e}"
        );
        assert!(e.ends_with(", [1.0,0.0,0.0]] }"), "{e}");
        // Integers in range: int8 and bit branches by size.
        assert!(
            e.contains("\"\"] } }, 5]") && e.contains("\"\"] } }, 3]"),
            "{e}"
        );
        // It parses as an update pipeline value.
        let st = crate::shell::parse_script(&format!(
            "db.c.updateOne({{ _id: 1 }}, [{{ $set: {{ \"v\": {e} }} }}])"
        ))
        .unwrap();
        assert!(st[0].cmd.get_array("updates").is_ok());
        let f = keep_form_expr("a.b", &[0.5, 2.0]);
        assert!(!f.contains("$switch") && f.contains("$getField"), "{f}");
    }

    #[test]
    fn designer_choices_per_server() {
        assert!(designer_spec(Flavor::Mongo).filter_columns);
        assert!(designer_spec(Flavor::Mongo).algorithms.is_empty());
        assert_eq!(
            designer_spec(Flavor::DocumentDb).algorithms,
            ["hnsw", "ivfflat"]
        );
        assert!(!designer_spec(Flavor::DocumentDb).filter_columns);
        assert_eq!(designer_spec(Flavor::Ferret).algorithms, ["hnsw", "ivf"]);
    }
}
